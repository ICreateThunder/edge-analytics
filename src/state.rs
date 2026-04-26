use crate::handlers::StatsResponse;
use crate::rate_limit::{RateLimiter, TokenBucket};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

pub(crate) type StatsCache = Arc<RwLock<Option<(Instant, Arc<StatsResponse>)>>>;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) dynamo: aws_sdk_dynamodb::Client,
    pub(crate) table_name: Arc<str>,
    pub(crate) rate_limiter: Arc<Mutex<RateLimiter>>,
    pub(crate) valid_paths: Arc<RwLock<HashSet<String>>>,
    pub(crate) started_at: Instant,
    pub(crate) stats_cache: StatsCache,
    pub(crate) stats_refreshing: Arc<AtomicBool>,
    pub(crate) stats_rate_limiter: Arc<Mutex<TokenBucket>>,
    pub(crate) enable_status: bool,
}

/// RAII guard that clears the stats_refreshing flag on drop.
/// Prevents the flag from getting stuck if the refresh task panics.
pub(crate) struct RefreshGuard(pub(crate) Arc<AtomicBool>);

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
