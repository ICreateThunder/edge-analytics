use crate::handlers::StatsResponse;
use crate::rate_limit::{RateLimiter, TokenBucket};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

pub type StatsCache = Arc<RwLock<Option<(Instant, Arc<StatsResponse>)>>>;

#[derive(Clone)]
pub struct AppState {
    pub dynamo: aws_sdk_dynamodb::Client,
    pub table_name: Arc<str>,
    pub rate_limiter: Arc<Mutex<RateLimiter>>,
    pub valid_paths: Arc<RwLock<HashSet<String>>>,
    pub started_at: Instant,
    pub stats_cache: StatsCache,
    pub stats_refreshing: Arc<AtomicBool>,
    pub stats_rate_limiter: Arc<Mutex<TokenBucket>>,
    pub enable_status: bool,
}

/// RAII guard that clears the stats_refreshing flag on drop.
/// Prevents the flag from getting stuck if the refresh task panics.
pub struct RefreshGuard(pub Arc<AtomicBool>);

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_guard_clears_flag() {
        let flag = Arc::new(AtomicBool::new(true));
        {
            let _guard = RefreshGuard(Arc::clone(&flag));
            assert!(flag.load(Ordering::Relaxed));
        }
        assert!(!flag.load(Ordering::Relaxed));
    }
}
