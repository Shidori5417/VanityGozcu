//! Global request budget, 429 pauses, Discord bucket tracking and error backoff.
//!
//! All checks go to a single route (`GET /invites/{code}`), so every known bucket
//! pause applies to every request.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use reqwest::header::HeaderMap;
use tokio::time::{Instant, sleep};

use crate::model::CheckResult;

const DEFAULT_BUCKET: &str = "default";
const MAX_BURST: u32 = 10;
const MAX_BACKOFF_EXPONENT: u32 = 16;

/// Rate limit headers of a response (`X-RateLimit-*`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RateHeaders {
    pub bucket: Option<String>,
    pub remaining: Option<u64>,
    pub reset_after: Option<Duration>,
}

impl RateHeaders {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        Self {
            bucket: text("x-ratelimit-bucket").map(str::to_owned),
            remaining: text("x-ratelimit-remaining").and_then(|v| v.trim().parse().ok()),
            reset_after: text("x-ratelimit-reset-after")
                .and_then(|v| v.trim().parse::<f64>().ok())
                .and_then(|secs| Duration::try_from_secs_f64(secs).ok()),
        }
    }
}

#[derive(Debug)]
struct State {
    tokens: f64,
    last_refill: Instant,
    global_pause: Option<Instant>,
    bucket_pauses: HashMap<String, Instant>,
}

/// Token bucket shared by all checks, plus global and per-bucket pauses.
#[derive(Debug)]
pub struct RateLimiter {
    /// Tokens added per second.
    rate: f64,
    capacity: f64,
    state: Mutex<State>,
}

impl RateLimiter {
    /// Budget of `max_requests_per_minute` with a small default burst.
    pub fn new(max_requests_per_minute: u32) -> Self {
        let burst = (max_requests_per_minute / 4).clamp(1, MAX_BURST);
        Self::with_burst(max_requests_per_minute, burst)
    }

    pub fn with_burst(max_requests_per_minute: u32, burst: u32) -> Self {
        let capacity = f64::from(burst.max(1));
        Self {
            rate: f64::from(max_requests_per_minute.max(1)) / 60.0,
            capacity,
            state: Mutex::new(State {
                tokens: capacity,
                last_refill: Instant::now(),
                global_pause: None,
                bucket_pauses: HashMap::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits until a request may be sent: no pause is active and a token is free.
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut s = self.lock();
                let now = Instant::now();
                s.bucket_pauses.retain(|_, until| *until > now);
                let pause = s
                    .bucket_pauses
                    .values()
                    .copied()
                    .chain(s.global_pause)
                    .max()
                    .filter(|until| *until > now);
                if let Some(until) = pause {
                    until - now
                } else {
                    let elapsed = now.saturating_duration_since(s.last_refill);
                    s.tokens = (s.tokens + elapsed.as_secs_f64() * self.rate).min(self.capacity);
                    s.last_refill = now;
                    if s.tokens >= 1.0 {
                        s.tokens -= 1.0;
                        return;
                    }
                    Duration::from_secs_f64((1.0 - s.tokens) / self.rate)
                }
            };
            sleep(wait).await;
        }
    }

    /// Makes every check wait for `duration` (extends an existing pause, never shortens it).
    pub fn pause_global(&self, duration: Duration) {
        let until = Instant::now() + duration;
        let mut s = self.lock();
        s.global_pause = Some(s.global_pause.map_or(until, |cur| cur.max(until)));
    }

    /// Pauses one Discord bucket until it resets.
    pub fn pause_bucket(&self, bucket: &str, duration: Duration) {
        let until = Instant::now() + duration;
        let mut s = self.lock();
        let entry = s.bucket_pauses.entry(bucket.to_owned()).or_insert(until);
        *entry = (*entry).max(until);
    }

    /// Feeds a response back: any 429 pauses everything, an exhausted bucket pauses
    /// until its reset.
    pub fn observe(&self, result: &CheckResult, headers: &RateHeaders) {
        if let CheckResult::RateLimited { retry_after, .. } = result {
            self.pause_global(*retry_after);
        }
        if headers.remaining == Some(0)
            && let Some(reset) = headers.reset_after
        {
            let bucket = headers.bucket.as_deref().unwrap_or(DEFAULT_BUCKET);
            self.pause_bucket(bucket, reset);
        }
    }
}

/// Exponential backoff for `failures` consecutive errors: `base * 2^failures`,
/// capped at `max`. Never shorter than `base`.
pub fn backoff_delay(base: Duration, failures: u32, max: Duration) -> Duration {
    if failures == 0 {
        return base;
    }
    let factor = 2f64.powi(failures.min(MAX_BACKOFF_EXPONENT) as i32);
    base.mul_f64(factor).min(max).max(base)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[tokio::test(start_paused = true)]
    async fn burst_then_paced() {
        // 60/min = 1 token per second, burst 3.
        let rl = RateLimiter::with_burst(60, 3);
        let start = Instant::now();
        for _ in 0..3 {
            rl.acquire().await;
        }
        assert_eq!(start.elapsed(), Duration::ZERO);
        rl.acquire().await;
        assert!(start.elapsed() >= Duration::from_millis(999));
        assert!(start.elapsed() < Duration::from_millis(1100));
        rl.acquire().await;
        assert!(start.elapsed() >= Duration::from_millis(1999));
    }

    #[tokio::test(start_paused = true)]
    async fn global_pause_blocks_all_waiters() {
        let rl = std::sync::Arc::new(RateLimiter::with_burst(6000, 10));
        let start = Instant::now();
        rl.observe(
            &CheckResult::RateLimited {
                retry_after: Duration::from_secs(5),
                global: false,
            },
            &RateHeaders::default(),
        );
        let a = tokio::spawn({
            let rl = rl.clone();
            async move {
                rl.acquire().await;
                Instant::now()
            }
        });
        let b = tokio::spawn({
            let rl = rl.clone();
            async move {
                rl.acquire().await;
                Instant::now()
            }
        });
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        assert!(a - start >= Duration::from_secs(5));
        assert!(b - start >= Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn pause_is_extended_never_shortened() {
        let rl = RateLimiter::with_burst(6000, 10);
        let start = Instant::now();
        rl.pause_global(Duration::from_secs(10));
        rl.pause_global(Duration::from_secs(2));
        rl.acquire().await;
        assert!(start.elapsed() >= Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_bucket_pauses_until_reset() {
        let rl = RateLimiter::with_burst(6000, 10);
        let start = Instant::now();
        let headers = RateHeaders {
            bucket: Some("abc".into()),
            remaining: Some(0),
            reset_after: Some(Duration::from_millis(2500)),
        };
        rl.observe(&CheckResult::Free, &headers);
        rl.acquire().await;
        assert!(start.elapsed() >= Duration::from_millis(2500));
        // Remaining > 0 does not pause.
        let headers = RateHeaders {
            remaining: Some(3),
            ..headers
        };
        let before = Instant::now();
        rl.observe(&CheckResult::Free, &headers);
        rl.acquire().await;
        assert_eq!(before.elapsed(), Duration::ZERO);
    }

    #[test]
    fn parses_rate_headers() {
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-bucket", HeaderValue::from_static("b1"));
        h.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        h.insert("x-ratelimit-reset-after", HeaderValue::from_static("1.5"));
        let parsed = RateHeaders::from_headers(&h);
        assert_eq!(parsed.bucket.as_deref(), Some("b1"));
        assert_eq!(parsed.remaining, Some(0));
        assert_eq!(parsed.reset_after, Some(Duration::from_millis(1500)));
        assert_eq!(
            RateHeaders::from_headers(&HeaderMap::new()),
            RateHeaders::default()
        );
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let base = Duration::from_secs(10);
        let max = Duration::from_secs(300);
        assert_eq!(backoff_delay(base, 0, max), base);
        assert_eq!(backoff_delay(base, 1, max), Duration::from_secs(20));
        assert_eq!(backoff_delay(base, 3, max), Duration::from_secs(80));
        assert_eq!(backoff_delay(base, 10, max), max);
        assert_eq!(backoff_delay(base, u32::MAX, max), max);
        // A base larger than the cap is never shortened.
        assert_eq!(
            backoff_delay(Duration::from_secs(600), 2, max),
            Duration::from_secs(600)
        );
    }
}
