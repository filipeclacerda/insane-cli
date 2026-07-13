//! Sliding-window-log rate limiter shared across all outbound HTTP requests.
//!
//! Guarantees that no more than `capacity` acquires complete within any
//! moving window of `window` duration, and serves waiters in FIFO order.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

/// Production defaults per SPEC §4: 40 requests / 60 seconds.
pub const DEFAULT_CAPACITY: usize = 40;
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct RateLimiterMetrics {
    pub capacity: Option<usize>,
    /// Number of acquires currently counted within the trailing window.
    pub used: usize,
    /// `capacity - used`.
    pub remaining: Option<usize>,
    /// Milliseconds until a new slot frees up (0 if a slot is available now).
    pub next_slot_in_ms: u64,
    pub min_interval_ms: u64,
    pub next_interval_in_ms: u64,
    pub next_request_in_ms: u64,
    /// Milliseconds until usage falls strictly below 75% of capacity
    /// (0 when already below, or when capacity is unlimited).
    pub next_below_75pct_in_ms: u64,
    /// Cumulative milliseconds spent waiting across all `acquire()` calls.
    pub total_waited_ms: u64,
    /// Cumulative number of completed acquires.
    pub total_acquired: u64,
}

struct Inner {
    log: VecDeque<Instant>,
    /// Global pause deadline set by `penalize` (e.g. from a Retry-After
    /// header). No acquire completes before this instant.
    paused_until: Option<Instant>,
    total_waited: Duration,
    total_acquired: u64,
    last_admitted: Option<Instant>,
    /// Monotonically increasing ticket counter to serialize FIFO order.
    next_ticket: u64,
    next_to_serve: u64,
}

/// Sliding-window-log rate limiter. Cheap to clone via `Arc` at the call
/// site; internally uses a single `tokio::Mutex` to guarantee FIFO fairness.
pub struct RateLimiter {
    capacity: Option<usize>,
    window: Duration,
    min_interval: Duration,
    inner: Mutex<Inner>,
    /// Notifies waiters when the internal state changes (a slot may have
    /// freed up, or the pause lifted).
    notify: tokio::sync::Notify,
}

impl RateLimiter {
    pub fn new(capacity: usize, window: Duration) -> Self {
        Self::with_policy(Some(capacity), window, Duration::ZERO)
    }

    pub fn with_policy(capacity: Option<usize>, window: Duration, min_interval: Duration) -> Self {
        RateLimiter {
            capacity,
            window,
            min_interval,
            inner: Mutex::new(Inner {
                log: VecDeque::new(),
                paused_until: None,
                total_waited: Duration::ZERO,
                total_acquired: 0,
                last_admitted: None,
                next_ticket: 0,
                next_to_serve: 0,
            }),
            notify: tokio::sync::Notify::new(),
        }
    }

    pub fn production() -> Self {
        Self::new(DEFAULT_CAPACITY, DEFAULT_WINDOW)
    }

    /// Waits (in FIFO order relative to other callers) until a slot is
    /// available, then records the acquire. Never lets more than `capacity`
    /// acquires land inside any `window`-length moving window.
    pub async fn acquire(&self) {
        let wait_start = Instant::now();
        let my_ticket = {
            let mut inner = self.inner.lock().await;
            let t = inner.next_ticket;
            inner.next_ticket += 1;
            t
        };

        loop {
            enum Step {
                Admitted,
                WaitFor(Duration),
                NotHeadOfLine,
            }

            let step = {
                let mut inner = self.inner.lock().await;

                // Only the head-of-line ticket may attempt to proceed; this
                // enforces strict FIFO even though many tasks may be racing
                // on the mutex.
                if inner.next_to_serve != my_ticket {
                    Step::NotHeadOfLine
                } else {
                    let now = Instant::now();

                    // Respect an active global pause from Retry-After.
                    let paused_wait = inner.paused_until.and_then(|until| {
                        if now < until {
                            Some(until - now)
                        } else {
                            inner.paused_until = None;
                            None
                        }
                    });

                    if let Some(d) = paused_wait {
                        Step::WaitFor(d)
                    } else {
                        evict_expired(&mut inner.log, now, self.window);
                        let rate_wait = self.capacity.and_then(|capacity| {
                            if inner.log.len() < capacity {
                                None
                            } else {
                                let oldest = *inner.log.front().expect("len >= capacity > 0");
                                Some((oldest + self.window).saturating_duration_since(now))
                            }
                        });
                        let interval_wait = inner.last_admitted.and_then(|last| {
                            let deadline = last + self.min_interval;
                            (deadline > now).then_some(deadline - now)
                        });
                        let required_wait = match (rate_wait, interval_wait) {
                            (Some(a), Some(b)) => Some(a.max(b)),
                            (Some(a), None) | (None, Some(a)) => Some(a),
                            (None, None) => None,
                        };
                        if let Some(wait) = required_wait {
                            Step::WaitFor(wait)
                        } else {
                            if self.capacity.is_some() {
                                inner.log.push_back(now);
                            }
                            inner.last_admitted = Some(now);
                            inner.next_to_serve = my_ticket + 1;
                            inner.total_acquired += 1;
                            inner.total_waited += wait_start.elapsed();
                            Step::Admitted
                        }
                    }
                }
            };

            match step {
                Step::Admitted => {
                    self.notify.notify_waiters();
                    return;
                }
                Step::NotHeadOfLine => {
                    self.notify.notified().await;
                }
                Step::WaitFor(d) => {
                    tokio::select! {
                        _ = tokio::time::sleep(d) => {}
                        _ = self.notify.notified() => {}
                    }
                }
            }
        }
    }

    /// Applies a global pause (e.g. from a 429 `Retry-After` header). No
    /// acquire will complete before `now + duration`.
    pub async fn penalize(&self, duration: Duration) {
        let mut inner = self.inner.lock().await;
        let until = Instant::now() + duration;
        inner.paused_until = Some(inner.paused_until.map_or(until, |cur| cur.max(until)));
        drop(inner);
        self.notify.notify_waiters();
    }

    pub async fn metrics(&self) -> RateLimiterMetrics {
        let mut inner = self.inner.lock().await;
        let now = Instant::now();
        evict_expired(&mut inner.log, now, self.window);
        let used = inner.log.len();
        let next_slot_in_ms = self
            .capacity
            .filter(|capacity| used >= *capacity)
            .and_then(|_| inner.log.front().copied())
            .map(|oldest| {
                (oldest + self.window)
                    .saturating_duration_since(now)
                    .as_millis() as u64
            })
            .unwrap_or(0);
        let next_interval_in_ms = inner
            .last_admitted
            .map(|last| {
                (last + self.min_interval)
                    .saturating_duration_since(now)
                    .as_millis() as u64
            })
            .unwrap_or(0);
        let next_below_75pct_in_ms = self
            .capacity
            .map(|capacity| ms_until_below_fraction(&inner.log, now, self.window, capacity, 3, 4))
            .unwrap_or(0);
        RateLimiterMetrics {
            capacity: self.capacity,
            used,
            remaining: self.capacity.map(|capacity| capacity.saturating_sub(used)),
            next_slot_in_ms,
            min_interval_ms: self.min_interval.as_millis() as u64,
            next_interval_in_ms,
            next_request_in_ms: next_slot_in_ms.max(next_interval_in_ms),
            next_below_75pct_in_ms,
            total_waited_ms: inner.total_waited.as_millis() as u64,
            total_acquired: inner.total_acquired,
        }
    }

    /// Milliseconds until trailing-window usage is at or below `percent` of
    /// capacity. Unlimited capacity, `percent == 0`, or `percent >= 100`
    /// returns 0.
    pub async fn next_below_percent_in_ms(&self, percent: u32) -> u64 {
        if self.capacity.is_none() || percent == 0 || percent >= 100 {
            return 0;
        }
        let mut inner = self.inner.lock().await;
        let now = Instant::now();
        evict_expired(&mut inner.log, now, self.window);
        ms_until_below_percent(
            &inner.log,
            now,
            self.window,
            self.capacity.expect("checked above"),
            percent,
        )
    }

    /// Waits until trailing-window usage is at or below `percent` of
    /// capacity. Unlimited capacity, `percent == 0`, or `percent >= 100`
    /// returns immediately.
    pub async fn wait_until_below_percent(&self, percent: u32) {
        if self.capacity.is_none() || percent == 0 || percent >= 100 {
            return;
        }
        loop {
            let wait = {
                let mut inner = self.inner.lock().await;
                let now = Instant::now();
                evict_expired(&mut inner.log, now, self.window);
                ms_until_below_percent(
                    &inner.log,
                    now,
                    self.window,
                    self.capacity.expect("checked above"),
                    percent,
                )
            };
            if wait == 0 {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(wait)) => {}
                _ = self.notify.notified() => {}
            }
        }
    }

    /// Waits until the trailing-window usage drops strictly below
    /// `numerator/denominator` of capacity. Unlimited capacity returns
    /// immediately.
    pub async fn wait_until_below_fraction(&self, numerator: usize, denominator: usize) {
        if denominator == 0 {
            return;
        }
        let percent = ((numerator as u64) * 100 / (denominator as u64)).min(100) as u32;
        self.wait_until_below_percent(percent).await;
    }
}

fn evict_expired(log: &mut VecDeque<Instant>, now: Instant, window: Duration) {
    while let Some(&front) = log.front() {
        if now.duration_since(front) >= window {
            log.pop_front();
        } else {
            break;
        }
    }
}

fn ms_until_below_fraction(
    log: &VecDeque<Instant>,
    now: Instant,
    window: Duration,
    capacity: usize,
    numerator: usize,
    denominator: usize,
) -> u64 {
    let threshold = (capacity * numerator) / denominator;
    if log.len() < threshold.saturating_add(1) {
        return 0;
    }
    let idx = log.len() - threshold;
    log.get(idx)
        .map(|instant| {
            (*instant + window)
                .saturating_duration_since(now)
                .as_millis() as u64
        })
        .unwrap_or(0)
}

fn ms_until_below_percent(
    log: &VecDeque<Instant>,
    now: Instant,
    window: Duration,
    capacity: usize,
    percent: u32,
) -> u64 {
    let threshold = ((capacity as u64) * (percent as u64) / 100) as usize;
    if log.len() < threshold.saturating_add(1) {
        return 0;
    }
    let idx = log.len() - threshold;
    log.get(idx)
        .map(|instant| {
            (*instant + window)
                .saturating_duration_since(now)
                .as_millis() as u64
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::time::{sleep, Duration as TokioDuration};

    /// Records completion timestamps and asserts no window ever holds more
    /// than `capacity` acquires.
    async fn run_and_check(limiter: Arc<RateLimiter>, capacity: usize, window: Duration, n: usize) {
        let mut handles = Vec::new();
        for _ in 0..n {
            let l = limiter.clone();
            handles.push(tokio::spawn(async move {
                l.acquire().await;
                Instant::now()
            }));
        }
        let mut timestamps = Vec::new();
        for h in handles {
            timestamps.push(h.await.unwrap());
        }
        timestamps.sort();

        // Sliding check: for every timestamp, count how many completions
        // fall within [t, t+window) and assert <= capacity.
        for &t in &timestamps {
            let count = timestamps
                .iter()
                .filter(|&&x| x >= t && x < t + window)
                .count();
            assert!(
                count <= capacity,
                "window starting at {:?} contained {} acquires (capacity {})",
                t,
                count,
                capacity
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn never_exceeds_capacity_in_any_window() {
        let capacity = 5;
        let window = TokioDuration::from_millis(200);
        let limiter = Arc::new(RateLimiter::new(capacity, window));
        run_and_check(limiter, capacity, window, 30).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fifo_fairness_under_concurrency() {
        // Launch tasks in order with tiny stagger so ticket order is
        // deterministic, then verify completion order matches ticket order
        // (since only one may complete per available slot at a time once
        // the window is saturated).
        let capacity = 2;
        let window = TokioDuration::from_millis(150);
        let limiter = Arc::new(RateLimiter::new(capacity, window));

        let mut handles = Vec::new();
        for i in 0..6 {
            let l = limiter.clone();
            handles.push(tokio::spawn(async move {
                l.acquire().await;
                i
            }));
            sleep(TokioDuration::from_millis(5)).await;
        }
        let mut order = Vec::new();
        for h in handles {
            order.push(h.await.unwrap());
        }
        assert_eq!(
            order,
            vec![0, 1, 2, 3, 4, 5],
            "acquires must complete in FIFO ticket order"
        );
    }

    #[tokio::test]
    async fn penalize_delays_next_acquire() {
        let limiter = RateLimiter::new(10, TokioDuration::from_millis(200));
        limiter.acquire().await;
        let start = Instant::now();
        limiter.penalize(TokioDuration::from_millis(150)).await;
        limiter.acquire().await;
        assert!(start.elapsed() >= TokioDuration::from_millis(140));
    }

    #[tokio::test]
    async fn metrics_reflect_usage() {
        let limiter = RateLimiter::new(3, TokioDuration::from_millis(500));
        limiter.acquire().await;
        limiter.acquire().await;
        let m = limiter.metrics().await;
        assert_eq!(m.used, 2);
        assert_eq!(m.remaining, Some(1));
        assert_eq!(m.total_acquired, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn window_slides_and_frees_slots() {
        let capacity = 2;
        let window = TokioDuration::from_millis(100);
        let limiter = Arc::new(RateLimiter::new(capacity, window));
        limiter.acquire().await;
        limiter.acquire().await;
        let start = Instant::now();
        // Third acquire must wait roughly one window before a slot frees.
        limiter.acquire().await;
        assert!(start.elapsed() >= TokioDuration::from_millis(90));
    }

    #[tokio::test]
    async fn minimum_interval_spaces_requests() {
        let limiter = Arc::new(RateLimiter::with_policy(
            None,
            Duration::from_secs(60),
            Duration::from_millis(40),
        ));
        let first = Instant::now();
        limiter.acquire().await;
        limiter.acquire().await;
        assert!(first.elapsed() >= Duration::from_millis(35));
        let metrics = limiter.metrics().await;
        assert_eq!(metrics.capacity, None);
        assert_eq!(metrics.min_interval_ms, 40);
    }

    #[tokio::test]
    async fn rpm_and_interval_are_both_enforced() {
        let limiter = RateLimiter::with_policy(
            Some(2),
            Duration::from_millis(120),
            Duration::from_millis(30),
        );
        let start = Instant::now();
        limiter.acquire().await;
        limiter.acquire().await;
        limiter.acquire().await;
        assert!(start.elapsed() >= Duration::from_millis(110));
    }
}
