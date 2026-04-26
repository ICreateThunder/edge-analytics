use std::collections::HashMap;
use std::time::Instant;

const BUCKET_WINDOW_SECS: u64 = 60;

pub(crate) struct RateLimiter {
    buckets: HashMap<String, TokenBucket>,
    capacity: u32,
}

pub(crate) struct TokenBucket {
    tokens: u32,
    last_refill: Instant,
}

impl RateLimiter {
    pub(crate) fn new(capacity: u32) -> Self {
        Self {
            buckets: HashMap::new(),
            capacity,
        }
    }

    pub(crate) fn allow(&mut self, key: &str) -> bool {
        let now = Instant::now();
        let capacity = self.capacity;
        let bucket = self.buckets.entry(key.to_string()).or_insert(TokenBucket {
            tokens: capacity,
            last_refill: now,
        });

        bucket.try_consume(now, capacity)
    }

    pub(crate) fn cleanup(&mut self) {
        let now = Instant::now();
        self.buckets
            .retain(|_, b| now.duration_since(b.last_refill).as_secs() < BUCKET_WINDOW_SECS * 5);
    }
}

impl TokenBucket {
    pub(crate) fn new(capacity: u32) -> Self {
        Self {
            tokens: capacity,
            last_refill: Instant::now(),
        }
    }

    pub(crate) fn try_consume(&mut self, now: Instant, capacity: u32) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_allows_up_to_capacity() {
        let mut limiter = RateLimiter::new(60);
        for _ in 0..60 {
            assert!(limiter.allow("/test"));
        }
        assert!(!limiter.allow("/test"));
        // Different path gets its own bucket
        assert!(limiter.allow("/other"));
    }

    #[test]
    fn cleanup_retains_recent_buckets() {
        let mut limiter = RateLimiter::new(60);
        limiter.allow("/stale");
        assert_eq!(limiter.buckets.len(), 1);
        limiter.cleanup();
        assert_eq!(limiter.buckets.len(), 1);
    }

    #[test]
    fn token_bucket_exhaustion() {
        let mut bucket = TokenBucket::new(3);
        let now = Instant::now();
        assert!(bucket.try_consume(now, 3));
        assert!(bucket.try_consume(now, 3));
        assert!(bucket.try_consume(now, 3));
        assert!(!bucket.try_consume(now, 3));
    }
}
