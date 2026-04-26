mod error;
mod handlers;
mod rate_limit;
mod sitemap;
mod state;
mod time;

use axum::routing::{get, post};
use axum::Router;
use state::AppState;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Maximum requests per path per minute
const MAX_REQUESTS_PER_MINUTE: u32 = 60;

/// How often to refresh the sitemap-derived path whitelist
const SITEMAP_REFRESH_SECS: u64 = 3600;

/// Fallback paths used if sitemap fetch fails on startup
static FALLBACK_PATHS: &[&str] = &["/"];

/// Maximum concurrent connections the server will accept
const MAX_CONNECTIONS: usize = 1024;

/// Per-request timeout in seconds
const REQUEST_TIMEOUT_SECS: u64 = 10;

/// Time to wait for in-flight requests to complete during shutdown
const GRACEFUL_SHUTDOWN_SECS: u64 = 10;

async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to listen for ctrl+c");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to listen for SIGTERM")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received ctrl+c, starting graceful shutdown"),
        _ = terminate => tracing::info!("received SIGTERM, starting graceful shutdown"),
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let table_name: Arc<str> = std::env::var("TABLE_NAME")
        .expect("TABLE_NAME must be set")
        .into();
    let port = std::env::var("PORT").unwrap_or_else(|_| "3001".to_string());
    let site_origin: Arc<str> = std::env::var("SITE_ORIGIN")
        .expect("SITE_ORIGIN must be set (e.g. https://example.com)")
        .into();
    let sitemap_url: Arc<str> = std::env::var("SITEMAP_URL")
        .expect("SITEMAP_URL must be set (e.g. https://example.com/sitemap-0.xml)")
        .into();
    let enable_status = std::env::var("ENABLE_STATUS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    // HTTP client with redirects disabled — sitemap URL is a direct path,
    // following redirects would be an SSRF vector to internal services
    let http_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("failed to build HTTP client");

    // Load valid paths from sitemap, fall back to hardcoded list
    let initial_paths = match sitemap::fetch_paths(&http_client, &sitemap_url, &site_origin).await {
        Ok(paths) if !paths.is_empty() => {
            tracing::info!("loaded {} paths from sitemap", paths.len());
            paths
        }
        Ok(_) => {
            tracing::warn!("sitemap returned no paths, using fallback paths");
            FALLBACK_PATHS.iter().map(|s| s.to_string()).collect()
        }
        Err(e) => {
            tracing::warn!("sitemap fetch failed: {e}, using fallback paths");
            FALLBACK_PATHS.iter().map(|s| s.to_string()).collect()
        }
    };
    let valid_paths = Arc::new(RwLock::new(initial_paths));

    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let dynamo = aws_sdk_dynamodb::Client::new(&config);

    let state = AppState {
        dynamo,
        table_name,
        rate_limiter: Arc::new(Mutex::new(rate_limit::RateLimiter::new(
            MAX_REQUESTS_PER_MINUTE,
        ))),
        valid_paths: valid_paths.clone(),
        started_at: Instant::now(),
        stats_cache: Arc::new(RwLock::new(None)),
        stats_refreshing: Arc::new(AtomicBool::new(false)),
        stats_rate_limiter: Arc::new(Mutex::new(rate_limit::TokenBucket::new(
            handlers::STATS_RATE_LIMIT_PER_MINUTE,
        ))),
        enable_status,
    };

    // Spawn periodic sitemap refresh
    let paths_handle = valid_paths.clone();
    let refresh_sitemap_url = Arc::clone(&sitemap_url);
    let refresh_site_origin = Arc::clone(&site_origin);
    let refresh_client = http_client;
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(SITEMAP_REFRESH_SECS));
        interval.tick().await;
        loop {
            interval.tick().await;
            match sitemap::fetch_paths(&refresh_client, &refresh_sitemap_url, &refresh_site_origin)
                .await
            {
                Ok(new_paths) if !new_paths.is_empty() => {
                    tracing::info!("refreshed sitemap: {} paths", new_paths.len());
                    *paths_handle.write().unwrap_or_else(|e| e.into_inner()) = new_paths;
                }
                Ok(_) => tracing::warn!("sitemap refresh returned no paths, keeping existing"),
                Err(e) => tracing::warn!("sitemap refresh failed: {e}, keeping existing"),
            }
        }
    });

    // Spawn periodic cleanup of rate limiter buckets
    let limiter = state.rate_limiter.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            limiter.lock().unwrap_or_else(|e| e.into_inner()).cleanup();
        }
    });

    // Parse CORS origin header at startup — validated once, not per-request
    let cors_origin: axum::http::HeaderValue = site_origin
        .parse()
        .expect("SITE_ORIGIN must be a valid header value");

    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::exact(cors_origin))
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([axum::http::header::CONTENT_TYPE]);

    let app = Router::new()
        .route("/views", post(handlers::track_view))
        .route("/status", get(handlers::health_status))
        .route("/stats", get(handlers::stats))
        .layer(cors)
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS),
        ))
        .layer(tower::limit::ConcurrencyLimitLayer::new(MAX_CONNECTIONS))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    tracing::info!("edge-analytics listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind TCP listener");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");

    tracing::info!(
        "shutting down, waiting up to {}s for in-flight requests",
        GRACEFUL_SHUTDOWN_SECS
    );
}
