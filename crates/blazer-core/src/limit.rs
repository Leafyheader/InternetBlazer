//! Token-bucket speed limits.

use std::time::Duration;

use parking_lot::Mutex;
use tokio::time::Instant;

/// Lowest accepted limit in bytes per second; lower ones are raised to it.
pub const MIN_SPEED_LIMIT: u64 = 1024;
/// Unused allowance is kept for this many seconds' worth of the limit, which bounds the burst
/// after an idle spell.
const BURST: f64 = 0.25;
/// Waits are checked this often, so a raised limit takes effect right away.
const MAX_WAIT: Duration = Duration::from_millis(100);

pub(crate) fn normalize(limit: Option<u64>) -> Option<u64> {
    limit.map(|l| l.max(MIN_SPEED_LIMIT))
}

/// Shared by all connections it limits. Connections take the bytes they received and then wait
/// until the bucket is out of debt, so the long-run rate matches the limit exactly.
pub(crate) struct RateLimiter(Mutex<Bucket>);

struct Bucket {
    /// Bytes per second; `None` is unlimited.
    rate: Option<u64>,
    /// Negative while connections owe bytes they already received.
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    pub(crate) fn new(limit: Option<u64>) -> Self {
        Self(Mutex::new(Bucket { rate: normalize(limit), tokens: 0.0, last: Instant::now() }))
    }

    pub(crate) fn set(&self, limit: Option<u64>) {
        let mut bucket = self.0.lock();
        bucket.rate = normalize(limit);
        // Forgives debt run up under the old limit.
        bucket.tokens = 0.0;
        bucket.last = Instant::now();
    }

    pub(crate) fn limit(&self) -> Option<u64> {
        self.0.lock().rate
    }

    /// Accounts for `n` received bytes and waits until the limit allows more.
    pub(crate) async fn take(&self, n: usize) {
        let mut wait = self.consume(n as f64);
        while let Some(delay) = wait {
            tokio::time::sleep(delay.min(MAX_WAIT)).await;
            wait = self.consume(0.0);
        }
    }

    /// Takes `n` tokens and returns how long until the debt is paid off, if there is any.
    fn consume(&self, n: f64) -> Option<Duration> {
        let mut bucket = self.0.lock();
        let rate = bucket.rate? as f64;
        let now = Instant::now();
        let refill = now.duration_since(bucket.last).as_secs_f64() * rate;
        bucket.tokens = (bucket.tokens + refill).min(rate * BURST) - n;
        bucket.last = now;
        (bucket.tokens < 0.0).then(|| Duration::from_secs_f64(-bucket.tokens / rate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn holds_the_rate_to_the_limit() {
        let limiter = RateLimiter::new(Some(100_000));
        let started = Instant::now();
        for _ in 0..50 {
            limiter.take(10_000).await;
        }
        // 500 KB at 100 KB/s.
        let elapsed = started.elapsed().as_secs_f64();
        assert!((4.8..5.2).contains(&elapsed), "took {elapsed}s");
    }

    #[tokio::test(start_paused = true)]
    async fn raising_the_limit_ends_waits_early() {
        let limiter = std::sync::Arc::new(RateLimiter::new(Some(MIN_SPEED_LIMIT)));
        let waiter = tokio::spawn({
            let limiter = limiter.clone();
            async move { limiter.take(100 * 1024).await }
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        limiter.set(None);
        let started = Instant::now();
        waiter.await.unwrap();
        assert!(started.elapsed() <= MAX_WAIT);
    }

    #[tokio::test(start_paused = true)]
    async fn unlimited_never_waits() {
        let limiter = RateLimiter::new(None);
        let started = Instant::now();
        limiter.take(usize::MAX).await;
        assert_eq!(started.elapsed(), Duration::ZERO);
    }
}
