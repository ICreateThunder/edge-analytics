use crate::error::{Error, Result};
use crate::state::{AppState, RefreshGuard};
use crate::time;
use aws_sdk_dynamodb::operation::update_item::UpdateItemError;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

/// Maximum request body size for POST /views (1KB)
const MAX_VIEW_BODY_BYTES: usize = 1024;

/// Maximum views stored per path per hour — prevents counter inflation
const MAX_VIEWS_PER_HOUR: i64 = 1000;

/// How long to cache /stats responses
pub(crate) const STATS_CACHE_SECS: u64 = 300;

/// Maximum requests to /stats per minute (shared across all callers)
pub(crate) const STATS_RATE_LIMIT_PER_MINUTE: u32 = 12;

#[derive(Deserialize)]
struct ViewRequest {
    path: Option<String>,
}

#[derive(Serialize, Clone)]
pub(crate) struct StatusResponse {
    status: &'static str,
    uptime_seconds: u64,
    paths_monitored: usize,
}

#[derive(Serialize, Clone)]
pub(crate) struct StatsResponse {
    total_views: i64,
    today: i64,
    top_pages: Vec<PageStats>,
}

#[derive(Serialize, Clone)]
struct PageStats {
    path: String,
    views: i64,
}

pub(crate) async fn health_status(State(state): State<AppState>) -> Result<Json<StatusResponse>> {
    if !state.enable_status {
        return Err(Error::NotFound);
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

pub(crate) async fn stats(State(state): State<AppState>) -> Result<Json<StatsResponse>> {
    // Rate limit /stats to prevent scan abuse
    {
        let mut bucket = state
            .stats_rate_limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !bucket.try_consume(Instant::now(), STATS_RATE_LIMIT_PER_MINUTE) {
            return Err(Error::RateLimited);
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
        // Another task is already refreshing; return stale cache or 503
        let cache = state.stats_cache.read().unwrap_or_else(|e| e.into_inner());
        if let Some((_, ref data)) = *cache {
            return Ok(Json(StatsResponse::clone(data)));
        }
        return Err(Error::StatsUnavailable);
    }

    // Guard clears the flag even if refresh_stats panics
    let _guard = RefreshGuard(state.stats_refreshing.clone());

    refresh_stats(&state).await
}

async fn refresh_stats(state: &AppState) -> Result<Json<StatsResponse>> {
    let today = time::date();

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

        let result = req.send().await?;

        if let Some(items) = &result.items {
            for item in items {
                let views = item
                    .get("views")
                    .and_then(|v| v.as_n().ok())
                    .and_then(|n| n.parse::<i64>().ok())
                    .unwrap_or(0);

                total_views += views;

                let is_today = item
                    .get("dateHour")
                    .and_then(|v| v.as_s().ok())
                    .is_some_and(|dh| dh.starts_with(&today));

                if is_today {
                    today_views += views;
                }

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

pub(crate) async fn track_view(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> std::result::Result<StatusCode, Error> {
    // Require application/json — our frontend uses fetch(), not sendBeacon
    match headers.get(axum::http::header::CONTENT_TYPE) {
        Some(ct) if ct.as_bytes().starts_with(b"application/json") => {}
        _ => return Ok(StatusCode::NO_CONTENT),
    }

    if body.len() > MAX_VIEW_BODY_BYTES {
        return Err(Error::PayloadTooLarge);
    }

    let payload: ViewRequest = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return Ok(StatusCode::NO_CONTENT),
    };

    let path = match payload.path {
        Some(ref p) if p.is_empty() => return Ok(StatusCode::NO_CONTENT),
        Some(p) if p.len() <= 128 => {
            let trimmed = p.trim_end_matches('/');
            if trimmed.is_empty() {
                "/".to_string()
            } else if trimmed.len() == p.len() {
                p
            } else {
                trimmed.to_string()
            }
        }
        _ => return Ok(StatusCode::NO_CONTENT),
    };

    // Validate against sitemap-derived whitelist
    {
        let valid = state.valid_paths.read().unwrap_or_else(|e| e.into_inner());
        if !valid.contains(&path) {
            return Ok(StatusCode::NO_CONTENT);
        }
    }

    // Rate limit by path
    {
        let mut limiter = state.rate_limiter.lock().unwrap_or_else(|e| e.into_inner());
        if !limiter.allow(&path) {
            return Err(Error::RateLimited);
        }
    }

    let now = time::date_hour();

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

    match result {
        Ok(_) => {}
        Err(ref e) if is_conditional_check_failure(e) => {
            // Expected: hourly cap reached for this path
            tracing::debug!("hourly view cap reached");
        }
        Err(e) => {
            tracing::error!("DynamoDB write failed: {e}");
        }
    }

    // Opportunistic early cache refresh
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

    Ok(StatusCode::NO_CONTENT)
}

/// Check if an UpdateItem SdkError is a ConditionalCheckFailedException
fn is_conditional_check_failure(err: &aws_sdk_dynamodb::error::SdkError<UpdateItemError>) -> bool {
    matches!(
        err.as_service_error(),
        Some(UpdateItemError::ConditionalCheckFailedException(_))
    )
}

/// Probabilistic early cache refresh for /stats.
///
/// As the cache approaches expiry, background requests have an increasing
/// probability of triggering a refresh. This spreads refresh load and
/// prevents a thundering herd at the exact expiry boundary.
fn should_early_refresh(cached_at: Instant) -> bool {
    let elapsed = cached_at.elapsed().as_secs();
    if elapsed < STATS_CACHE_SECS * 4 / 5 {
        return false;
    }
    let window = STATS_CACHE_SECS / 5;
    let into_window = elapsed - (STATS_CACHE_SECS * 4 / 5);
    let probability = (into_window as f64) / (window as f64);
    rand_simple() < probability
}

/// Simple pseudo-random f64 in [0, 1) seeded from timing jitter
fn rand_simple() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let mixed = nanos.wrapping_mul(2654435761);
    (mixed as f64) / (u32::MAX as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn early_refresh_false_for_fresh_cache() {
        let recent = Instant::now();
        assert!(!should_early_refresh(recent));
    }

    #[test]
    fn rand_simple_in_range() {
        for _ in 0..100 {
            let val = rand_simple();
            assert!((0.0..1.0).contains(&val));
        }
    }

    #[test]
    fn path_trim_reuses_allocation() {
        let original = String::from("/profile");
        let ptr_before = original.as_ptr();
        let trimmed = original.trim_end_matches('/');
        assert_eq!(trimmed.as_ptr(), ptr_before);
    }
}
