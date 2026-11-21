//! When to retry a request, and how far apart requests to one host must be. Both wait on `tokio::time`, so tests with
//! paused time take no real time.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;
use url::Url;
use zenodotus_core::model::content_hash;

/// The wait before the first retry; each later retry waits twice as long.
const BASE_DELAY: Duration = Duration::from_millis(500);

/// The jitter added to each wait is below this many milliseconds.
const JITTER_MS: u64 = 251;

/// The longest wait a `Retry-After` header may ask for.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// How many times a request is retried, and how long each retry waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt.
    pub retries: u32,
}

impl RetryPolicy {
    /// The wait before retry number `attempt` (0 for the first retry) of `url`: `retry_after` capped at
    /// [`MAX_RETRY_AFTER`] when the server sent one, otherwise 500 ms × 2^attempt plus a jitter below 251 ms that a
    /// hash of the URL and the attempt fixes.
    #[must_use]
    pub fn delay(&self, attempt: u32, url: &Url, retry_after: Option<Duration>) -> Duration {
        if let Some(wait) = retry_after {
            return wait.min(MAX_RETRY_AFTER);
        }
        BASE_DELAY.saturating_mul(1 << attempt.min(16)) + Duration::from_millis(jitter(url, attempt))
    }
}

/// A deterministic jitter in milliseconds for `attempt` of `url`.
fn jitter(url: &Url, attempt: u32) -> u64 {
    let hash = content_hash(&format!("{url} {attempt}"));
    u64::from_str_radix(&hash[..12], 16).expect("a SHA-256 is hex") % JITTER_MS
}

/// A `Retry-After` value in seconds; HTTP dates are ignored.
#[must_use]
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse().ok().map(Duration::from_secs)
}

/// One host's next free slot and its spacing.
#[derive(Debug, Clone, Copy)]
struct Slot {
    next: Instant,
    interval: Duration,
}

/// Spaces requests to the same host at least an interval apart. Hosts are independent.
#[derive(Debug)]
pub struct HostSchedule {
    interval: Duration,
    hosts: Mutex<HashMap<String, Slot>>,
}

impl HostSchedule {
    /// A schedule that spaces requests to each host `interval` apart.
    #[must_use]
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            hosts: Mutex::new(HashMap::new()),
        }
    }

    /// Raises the spacing of `host` to `interval`, for a robots.txt `Crawl-delay`; a shorter one changes nothing.
    pub fn slow_down(&self, host: &str, interval: Duration) {
        let mut hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = hosts.entry(host.to_owned()).or_insert(Slot {
            next: Instant::now(),
            interval: self.interval,
        });
        slot.interval = slot.interval.max(interval);
    }

    /// Waits for the next free slot of `host` and takes it.
    pub async fn wait(&self, host: &str) {
        let now = Instant::now();
        let start = {
            let mut hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
            let slot = hosts.entry(host.to_owned()).or_insert(Slot {
                next: now,
                interval: self.interval,
            });
            let start = slot.next.max(now);
            slot.next = start + slot.interval;
            start
        };
        tokio::time::sleep_until(start).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn waits_double_with_a_fixed_jitter_per_url_and_attempt() {
        let policy = RetryPolicy { retries: 3 };
        let page = url("https://docs.example/a");
        for attempt in 0..4 {
            let wait = policy.delay(attempt, &page, None);
            let base = BASE_DELAY * 2_u32.pow(attempt);
            assert!(
                wait >= base && wait < base + Duration::from_millis(JITTER_MS),
                "{wait:?}"
            );
            assert_eq!(wait, policy.delay(attempt, &page, None));
        }
        let jitters: Vec<u64> = (0..20)
            .map(|n| jitter(&url(&format!("https://docs.example/{n}")), 0))
            .collect();
        assert!(jitters.iter().any(|&ms| ms != jitters[0]), "{jitters:?}");
        assert_eq!(jitter(&page, 0), 35);
        assert!(policy.delay(40, &page, None) >= BASE_DELAY * 65_536);
    }

    #[test]
    fn retry_after_replaces_the_wait_up_to_a_minute() {
        let policy = RetryPolicy { retries: 1 };
        let page = url("https://docs.example/a");
        assert_eq!(
            policy.delay(0, &page, Some(Duration::from_secs(3))),
            Duration::from_secs(3)
        );
        assert_eq!(policy.delay(0, &page, Some(Duration::from_secs(600))), MAX_RETRY_AFTER);
        assert_eq!(parse_retry_after(" 7 "), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn requests_to_one_host_are_spaced_and_other_hosts_are_not_delayed() {
        let schedule = HostSchedule::new(Duration::from_millis(500));
        let start = Instant::now();
        schedule.wait("a.example").await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        schedule.wait("b.example").await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        schedule.wait("a.example").await;
        assert_eq!(start.elapsed(), Duration::from_millis(500));
        tokio::time::sleep(Duration::from_secs(5)).await;
        let later = Instant::now();
        schedule.wait("a.example").await;
        assert_eq!(later.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_crawl_delay_slows_one_host_down_but_never_speeds_it_up() {
        let schedule = HostSchedule::new(Duration::from_millis(500));
        schedule.slow_down("a.example", Duration::from_secs(2));
        schedule.slow_down("a.example", Duration::from_millis(100));
        schedule.slow_down("b.example", Duration::from_millis(100));
        let start = Instant::now();
        schedule.wait("a.example").await;
        schedule.wait("a.example").await;
        assert_eq!(start.elapsed(), Duration::from_secs(2));
        let start = Instant::now();
        schedule.wait("b.example").await;
        schedule.wait("b.example").await;
        assert_eq!(start.elapsed(), Duration::from_millis(500));
    }
}
