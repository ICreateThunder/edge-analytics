use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Maximum requests per path per minute before rate limiting kicks in
const MAX_REQUESTS_PER_MINUTE: u32 = 60;
const BUCKET_WINDOW_SECS: u64 = 60;

/// Maximum views stored per path per hour — prevents counter inflation
const MAX_VIEWS_PER_HOUR: i64 = 1000;

/// How often to refresh the sitemap-derived path whitelist
const SITEMAP_REFRESH_SECS: u64 = 3600;

/// How long to cache /stats responses
const STATS_CACHE_SECS: u64 = 300;

/// Maximum request body size for POST /views (1KB)
const MAX_VIEW_BODY_BYTES: usize = 1024;

/// Fallback paths used if sitemap fetch fails on startup
static FALLBACK_PATHS: &[&str] = &["/"];

/// Maximum requests to /stats per minute (shared across all callers)
const STATS_RATE_LIMIT_PER_MINUTE: u32 = 12;

/// Maximum concurrent connections the server will accept
const MAX_CONNECTIONS: usize = 1024;

/// Per-request timeout in seconds
const REQUEST_TIMEOUT_SECS: u64 = 10;

/// Time to wait for in-flight requests to complete during shutdown
const GRACEFUL_SHUTDOWN_SECS: u64 = 10;

#[derive(Clone)]
struct AppState {
    dynamo: aws_sdk_dynamodb::Client,
    table_name: Arc<str>,
    rate_limiter: Arc<Mutex<RateLimiter>>,
    valid_paths: Arc<RwLock<HashSet<String>>>,
    started_at: Instant,
    stats_cache: StatsCache,
    stats_refreshing: Arc<AtomicBool>,
    stats_rate_limiter: Arc<Mutex<TokenBucket>>,
    enable_status: bool,
}

struct RateLimiter {
    buckets: HashMap<String, TokenBucket>,
}

struct TokenBucket {
    tokens: u32,
    last_refill: Instant,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            buckets: HashMap::new(),
        }
    }

    fn allow(&mut self, key: &str) -> bool {
        let now = Instant::now();
        let bucket = self.buckets.entry(key.to_string()).or_insert(TokenBucket {
            tokens: MAX_REQUESTS_PER_MINUTE,
            last_refill: now,
        });

        bucket.try_consume(now, MAX_REQUESTS_PER_MINUTE)
    }

    fn cleanup(&mut self) {
        let now = Instant::now();
        self.buckets
            .retain(|_, b| now.duration_since(b.last_refill).as_secs() < BUCKET_WINDOW_SECS * 5);
    }
}

impl TokenBucket {
    fn new(capacity: u32) -> Self {
        Self {
            tokens: capacity,
            last_refill: Instant::now(),
        }
    }

    fn try_consume(&mut self, now: Instant, capacity: u32) -> bool {
        let elapsed = now.duration_since(self.last_refill).as_secs();
        if elapsed >= BUCKET_WINDOW_SECS {
            self.tokens = capacity;
            self.last_refill = now;
        }

        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            false
        }
    }
}

type StatsCache = Arc<RwLock<Option<(Instant, Arc<StatsResponse>)>>>;

/// RAII guard that clears the stats_refreshing flag on drop.
/// Prevents the flag from getting stuck if the refresh task panics.
struct RefreshGuard(Arc<AtomicBool>);

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Maximum sitemap response size (1MB) — prevents memory exhaustion
const MAX_SITEMAP_BYTES: u64 = 1_048_576;
/// Maximum paths accepted from a single sitemap
const MAX_SITEMAP_PATHS: usize = 500;

/// Fetch sitemap and extract paths as a HashSet.
///
/// Uses a pre-configured client with redirects disabled and streams the
/// response with a size cap to prevent memory exhaustion.
async fn fetch_sitemap_paths(
    client: &reqwest::Client,
    sitemap_url: &str,
    site_origin: &str,
) -> Option<HashSet<String>> {
    let resp = client.get(sitemap_url).send().await.ok()?;

    if !resp.status().is_success() {
        tracing::warn!("sitemap fetch returned status {}", resp.status());
        return None;
    }

    // Early rejection via Content-Length before buffering the body
    let content_length = resp.content_length();
    if let Some(len) = content_length {
        if len > MAX_SITEMAP_BYTES {
            tracing::warn!(
                "sitemap Content-Length too large ({} bytes), rejecting",
                len
            );
            return None;
        }
    }

    // Pre-allocate based on Content-Length hint, capped to MAX_SITEMAP_BYTES
    let capacity = content_length
        .map(|len| len as usize)
        .unwrap_or(8192)
        .min(MAX_SITEMAP_BYTES as usize);
    let mut body_bytes = Vec::with_capacity(capacity);

    // Stream body with a hard cap — prevents slow-drip attacks that omit Content-Length
    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        body_bytes.extend_from_slice(&chunk);
        if body_bytes.len() as u64 > MAX_SITEMAP_BYTES {
            tracing::warn!("sitemap response exceeded size limit during streaming, rejecting");
            return None;
        }
    }

    let body = String::from_utf8_lossy(&body_bytes);

    let mut paths = HashSet::new();

    for segment in body.split("<loc>") {
        if paths.len() >= MAX_SITEMAP_PATHS {
            break;
        }
        if let Some(url) = segment.split("</loc>").next() {
            if let Some(path) = url.strip_prefix(site_origin) {
                let normalized = path.trim_end_matches('/');
                let normalized = if normalized.is_empty() {
                    "/"
                } else {
                    normalized
                };

                if is_valid_path_chars(normalized) {
                    paths.insert(normalized.to_string());
                }
            }
        }
    }

    if paths.is_empty() {
        None
    } else {
        Some(paths)
    }
}

fn is_valid_path_chars(path: &str) -> bool {
    path == "/"
        || (path.starts_with('/')
            && path.len() <= 128
            && path
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '/'))
}

#[derive(Deserialize)]
struct ViewRequest {
    path: Option<String>,
}

#[derive(Serialize, Clone)]
struct StatusResponse {
    status: &'static str,
    uptime_seconds: u64,
    paths_monitored: usize,
}

#[derive(Serialize, Clone)]
struct StatsResponse {
    total_views: i64,
    today: i64,
    top_pages: Vec<PageStats>,
}

#[derive(Serialize, Clone)]
struct PageStats {
    path: String,
    views: i64,
}

async fn health_status(State(state): State<AppState>) -> Result<Json<StatusResponse>, StatusCode> {
    if !state.enable_status {
        return Err(StatusCode::NOT_FOUND);
    }

    let uptime = state.started_at.elapsed().as_secs();
    let paths = state
        .valid_paths
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .len();

    Ok(Json(StatusResponse {
        status: "ok",
        uptime_seconds: uptime,
        paths_monitored: paths,
    }))
}

async fn stats(State(state): State<AppState>) -> Result<Json<StatsResponse>, StatusCode> {
    // Rate limit /stats to prevent scan abuse
    {
        let mut bucket = state
            .stats_rate_limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !bucket.try_consume(Instant::now(), STATS_RATE_LIMIT_PER_MINUTE) {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
    }

    // Check cache
    {
        let cache = state.stats_cache.read().unwrap_or_else(|e| e.into_inner());
        if let Some((cached_at, ref data)) = *cache {
            if cached_at.elapsed().as_secs() < STATS_CACHE_SECS {
                return Ok(Json(StatsResponse::clone(data)));
            }
        }
    }

    // Prevent thundering herd — only one task refreshes, others get stale cache
    if state
        .stats_refreshing
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        // Another task is already refreshing; return stale cache if available
        let cache = state.stats_cache.read().unwrap_or_else(|e| e.into_inner());
        if let Some((_, ref data)) = *cache {
            return Ok(Json(StatsResponse::clone(data)));
        }
        // No cache at all — first request ever, fall through to wait
    }

    // Guard clears the flag even if refresh_stats panics
    let _guard = RefreshGuard(state.stats_refreshing.clone());

    refresh_stats(&state).await
}

async fn refresh_stats(state: &AppState) -> Result<Json<StatsResponse>, StatusCode> {
    let today = chrono_lite_date();

    let mut total_views: i64 = 0;
    let mut today_views: i64 = 0;
    let mut per_page: HashMap<String, i64> = HashMap::new();

    // Paginated scan — handles tables larger than 1MB
    let mut exclusive_start_key = None;
    loop {
        let mut req = state
            .dynamo
            .scan()
            .table_name(&*state.table_name)
            .projection_expression("#p, dateHour, #v")
            .expression_attribute_names("#p", "path")
            .expression_attribute_names("#v", "views");

        if let Some(key) = exclusive_start_key.take() {
            req = req.set_exclusive_start_key(Some(key));
        }

        let result = req.send().await.map_err(|e| {
            tracing::error!("DynamoDB scan failed: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        if let Some(items) = &result.items {
            for item in items {
                let views = item
                    .get("views")
                    .and_then(|v| v.as_n().ok())
                    .and_then(|n| n.parse::<i64>().ok())
                    .unwrap_or(0);

                total_views += views;

                // Borrow dateHour directly — no allocation needed for the starts_with check
                let is_today = item
                    .get("dateHour")
                    .and_then(|v| v.as_s().ok())
                    .is_some_and(|dh| dh.starts_with(&today));

                if is_today {
                    today_views += views;
                }

                // Only allocate path String for the HashMap aggregation
                if let Some(path) = item.get("path").and_then(|v| v.as_s().ok()) {
                    *per_page.entry(path.clone()).or_insert(0) += views;
                }
            }
        }

        match result.last_evaluated_key {
            Some(key) if !key.is_empty() => exclusive_start_key = Some(key),
            _ => break,
        }
    }

    let mut top_pages: Vec<PageStats> = per_page
        .into_iter()
        .map(|(path, views)| PageStats { path, views })
        .collect();
    top_pages.sort_by_key(|p| std::cmp::Reverse(p.views));
    top_pages.truncate(10);

    let response = Arc::new(StatsResponse {
        total_views,
        today: today_views,
        top_pages,
    });

    // Update cache
    {
        let mut cache = state.stats_cache.write().unwrap_or_else(|e| e.into_inner());
        *cache = Some((Instant::now(), Arc::clone(&response)));
    }

    Ok(Json(Arc::unwrap_or_clone(response)))
}

/// Probabilistic early cache refresh for /stats.
///
/// As the cache approaches expiry, background requests have an increasing
/// probability of triggering a refresh. This spreads refresh load and
/// prevents a thundering herd at the exact expiry boundary.
fn should_early_refresh(cached_at: Instant) -> bool {
    let elapsed = cached_at.elapsed().as_secs();
    if elapsed < STATS_CACHE_SECS * 4 / 5 {
        return false; // Too early — no refresh before 80% of TTL
    }
    // Linear probability ramp from 0% at 80% TTL to 100% at 100% TTL
    let window = STATS_CACHE_SECS / 5;
    let into_window = elapsed - (STATS_CACHE_SECS * 4 / 5);
    let probability = (into_window as f64) / (window as f64);
    rand_simple() < probability
}

/// Simple pseudo-random f64 in [0, 1) seeded from Instant timing jitter
fn rand_simple() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    // Mix bits for better distribution
    let mixed = nanos.wrapping_mul(2654435761); // Knuth multiplicative hash
    (mixed as f64) / (u32::MAX as f64)
}

async fn track_view(State(state): State<AppState>, body: axum::body::Bytes) -> StatusCode {
    // Enforce body size limit before parsing
    if body.len() > MAX_VIEW_BODY_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE;
    }

    let payload: ViewRequest = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return StatusCode::NO_CONTENT,
    };

    let path = match payload.path {
        Some(p) if p.len() <= 128 => {
            let trimmed = p.trim_end_matches('/');
            if trimmed.is_empty() {
                "/".to_string()
            } else if trimmed.len() == p.len() {
                p // No trailing slash — reuse original allocation
            } else {
                trimmed.to_string()
            }
        }
        _ => return StatusCode::NO_CONTENT,
    };

    // Validate against sitemap-derived whitelist
    {
        let valid = state.valid_paths.read().unwrap_or_else(|e| e.into_inner());
        if !valid.contains(&path) {
            return StatusCode::NO_CONTENT;
        }
    }

    // Rate limit by path
    {
        let mut limiter = state.rate_limiter.lock().unwrap_or_else(|e| e.into_inner());
        if !limiter.allow(&path) {
            return StatusCode::TOO_MANY_REQUESTS;
        }
    }

    let now = chrono_lite_date_hour();

    // Conditional update — stops incrementing past the hourly cap
    let result = state
        .dynamo
        .update_item()
        .table_name(&*state.table_name)
        .key("path", aws_sdk_dynamodb::types::AttributeValue::S(path))
        .key("dateHour", aws_sdk_dynamodb::types::AttributeValue::S(now))
        .update_expression("ADD #v :inc")
        .condition_expression("attribute_not_exists(#v) OR #v < :max")
        .expression_attribute_names("#v", "views")
        .expression_attribute_values(
            ":inc",
            aws_sdk_dynamodb::types::AttributeValue::N("1".to_string()),
        )
        .expression_attribute_values(
            ":max",
            aws_sdk_dynamodb::types::AttributeValue::N(MAX_VIEWS_PER_HOUR.to_string()),
        )
        .send()
        .await;

    if let Err(e) = result {
        tracing::error!("DynamoDB write failed: {}", e);
    }

    // Opportunistic early cache refresh — amortises /stats scan cost
    {
        let cache = state.stats_cache.read().unwrap_or_else(|e| e.into_inner());
        if let Some((cached_at, _)) = *cache {
            if should_early_refresh(cached_at) {
                let state = state.clone();
                tokio::spawn(async move {
                    if state
                        .stats_refreshing
                        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                        .is_ok()
                    {
                        let _guard = RefreshGuard(state.stats_refreshing.clone());
                        let _ = refresh_stats(&state).await;
                    }
                });
            }
        }
    }

    StatusCode::NO_CONTENT
}

/// Returns current UTC date as "YYYY-MM-DD"
fn chrono_lite_date() -> String {
    chrono_lite_date_hour()[..10].to_string()
}

/// Returns current UTC date+hour as "YYYY-MM-DDTHH"
fn chrono_lite_date_hour() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let days = secs / 86400;
    let hour = (secs % 86400) / 3600;

    let mut y = 1970i64;
    let mut remaining = days as i64;
    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = is_leap(y);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0u32;
    for &md in &month_days {
        if remaining < md {
            break;
        }
        remaining -= md;
        m += 1;
    }
    format!("{:04}-{:02}-{:02}T{:02}", y, m + 1, remaining + 1, hour)
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

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

    let started_at = Instant::now();

    // HTTP client with redirects disabled — sitemap URL is a direct path,
    // following redirects would be an SSRF vector to internal services
    let http_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("failed to build HTTP client");

    // Load valid paths from sitemap, fall back to hardcoded list
    let initial_paths = match fetch_sitemap_paths(&http_client, &sitemap_url, &site_origin).await {
        Some(paths) => {
            tracing::info!("loaded {} paths from sitemap", paths.len());
            paths
        }
        None => {
            tracing::warn!("sitemap fetch failed, using fallback paths");
            FALLBACK_PATHS.iter().map(|s| s.to_string()).collect()
        }
    };
    let valid_paths = Arc::new(RwLock::new(initial_paths));

    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let dynamo = aws_sdk_dynamodb::Client::new(&config);

    let state = AppState {
        dynamo,
        table_name,
        rate_limiter: Arc::new(Mutex::new(RateLimiter::new())),
        valid_paths: valid_paths.clone(),
        started_at,
        stats_cache: Arc::new(RwLock::new(None)),
        stats_refreshing: Arc::new(AtomicBool::new(false)),
        stats_rate_limiter: Arc::new(Mutex::new(TokenBucket::new(STATS_RATE_LIMIT_PER_MINUTE))),
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
            if let Some(new_paths) =
                fetch_sitemap_paths(&refresh_client, &refresh_sitemap_url, &refresh_site_origin)
                    .await
            {
                tracing::info!("refreshed sitemap: {} paths", new_paths.len());
                *paths_handle.write().unwrap_or_else(|e| e.into_inner()) = new_paths;
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
        .route("/views", post(track_view))
        .route("/status", get(health_status))
        .route("/stats", get(stats))
        .layer(cors)
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS),
        ))
        .layer(tower::limit::ConcurrencyLimitLayer::new(MAX_CONNECTIONS))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    tracing::info!("edge-analytics listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();

    tracing::info!(
        "shutting down, waiting up to {}s for in-flight requests",
        GRACEFUL_SHUTDOWN_SECS
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_path_chars() {
        assert!(is_valid_path_chars("/"));
        assert!(is_valid_path_chars("/profile"));
        assert!(is_valid_path_chars("/projects/oxiflight"));
        assert!(is_valid_path_chars("/projects/my-project-123"));

        assert!(!is_valid_path_chars(""));
        assert!(!is_valid_path_chars("no-leading-slash"));
        assert!(!is_valid_path_chars("/<script>"));
        assert!(!is_valid_path_chars("/path with spaces"));
        assert!(!is_valid_path_chars("/path?query=1"));
        assert!(!is_valid_path_chars("/../../etc/passwd"));
        assert!(!is_valid_path_chars(&format!("/{}", "a".repeat(128))));
    }

    #[test]
    fn test_rate_limiter() {
        let mut limiter = RateLimiter::new();
        for _ in 0..MAX_REQUESTS_PER_MINUTE {
            assert!(limiter.allow("/test"));
        }
        assert!(!limiter.allow("/test"));
        // Different path should have its own bucket
        assert!(limiter.allow("/other"));
    }

    #[test]
    fn test_chrono_lite_date_hour_format() {
        let result = chrono_lite_date_hour();
        assert_eq!(result.len(), 13); // YYYY-MM-DDTHH
        assert_eq!(&result[4..5], "-");
        assert_eq!(&result[7..8], "-");
        assert_eq!(&result[10..11], "T");
    }

    #[test]
    fn test_chrono_lite_date() {
        let result = chrono_lite_date();
        assert_eq!(result.len(), 10); // YYYY-MM-DD
        assert_eq!(&result[4..5], "-");
        assert_eq!(&result[7..8], "-");
    }

    #[test]
    fn test_is_leap() {
        assert!(is_leap(2000));
        assert!(is_leap(2024));
        assert!(!is_leap(1900));
        assert!(!is_leap(2023));
    }

    #[test]
    fn test_rate_limiter_cleanup() {
        let mut limiter = RateLimiter::new();
        limiter.allow("/stale");
        assert_eq!(limiter.buckets.len(), 1);
        // Cleanup shouldn't remove recent buckets
        limiter.cleanup();
        assert_eq!(limiter.buckets.len(), 1);
    }

    #[test]
    fn test_should_early_refresh_before_window() {
        // Cache created just now — well within 80% TTL, should never refresh
        let recent = Instant::now();
        assert!(!should_early_refresh(recent));
    }

    #[test]
    fn test_rand_simple_range() {
        for _ in 0..100 {
            let val = rand_simple();
            assert!((0.0..1.0).contains(&val));
        }
    }

    #[test]
    fn test_token_bucket_try_consume() {
        let mut bucket = TokenBucket::new(3);
        let now = Instant::now();
        assert!(bucket.try_consume(now, 3));
        assert!(bucket.try_consume(now, 3));
        assert!(bucket.try_consume(now, 3));
        assert!(!bucket.try_consume(now, 3));
    }

    #[test]
    fn test_refresh_guard_clears_flag() {
        let flag = Arc::new(AtomicBool::new(true));
        {
            let _guard = RefreshGuard(Arc::clone(&flag));
            assert!(flag.load(Ordering::Relaxed));
        }
        // Guard dropped — flag should be cleared
        assert!(!flag.load(Ordering::Relaxed));
    }

    #[test]
    fn test_path_trim_reuses_allocation() {
        // Path without trailing slash — should not allocate a new String
        let original = String::from("/profile");
        let ptr_before = original.as_ptr();
        let trimmed = original.trim_end_matches('/');
        // trimmed is a slice of the original — same pointer
        assert_eq!(trimmed.as_ptr(), ptr_before);
    }
}
