//! Time-based state: the notification throttle and the per-tid backoff after
//! a failed `sched_setaffinity`. Both take time from a [`Clock`] so tests can
//! drive them with a [`FakeClock`].

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub trait Clock: Send + Sync {
    /// Monotonic time for throttling and backoff.
    fn now(&self) -> Instant;
    /// Wall-clock time for interventions and metrics.
    fn wall(&self) -> SystemTime;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wall(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A clock that only moves when told to; both readings advance together.
#[derive(Clone)]
pub struct FakeClock(Arc<Mutex<(Instant, SystemTime)>>);

impl FakeClock {
    pub fn new(wall: SystemTime) -> Self {
        Self(Arc::new(Mutex::new((Instant::now(), wall))))
    }

    pub fn advance(&self, by: Duration) {
        let mut t = self.0.lock().unwrap();
        t.0 += by;
        t.1 += by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.0.lock().unwrap().0
    }

    fn wall(&self) -> SystemTime {
        self.0.lock().unwrap().1
    }
}

/// Lets one event per key through per window.
#[derive(Debug)]
pub struct Throttle<K> {
    window: Duration,
    last: HashMap<K, Instant>,
}

impl<K: Eq + Hash> Throttle<K> {
    pub fn new(window: Duration) -> Self {
        Self { window, last: HashMap::new() }
    }

    /// True if `key` has not passed within the window; records it if so.
    pub fn allow(&mut self, key: K, now: Instant) -> bool {
        let window = self.window;
        self.last.retain(|_, at| now.duration_since(*at) < window);
        if self.last.contains_key(&key) {
            return false;
        }
        self.last.insert(key, now);
        true
    }
}

/// Exponential backoff per tid: `initial`, doubling up to `max`.
#[derive(Debug)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    entries: HashMap<i32, (Instant, Duration)>,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self { initial, max, entries: HashMap::new() }
    }

    /// True while `tid` must not be retried.
    pub fn blocked(&self, tid: i32, now: Instant) -> bool {
        self.entries.get(&tid).is_some_and(|(until, _)| now < *until)
    }

    pub fn failed(&mut self, tid: i32, now: Instant) {
        let delay = match self.entries.get(&tid) {
            Some((_, prev)) => (*prev * 2).min(self.max),
            None => self.initial,
        };
        self.entries.insert(tid, (now + delay, delay));
    }

    pub fn succeeded(&mut self, tid: i32) {
        self.entries.remove(&tid);
    }

    /// Forgets tids that no longer exist, so a reused tid starts fresh.
    pub fn retain(&mut self, live: &HashSet<i32>) {
        self.entries.retain(|tid, _| live.contains(tid));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn throttle_lets_one_per_key_and_window() {
        let clock = FakeClock::new(SystemTime::UNIX_EPOCH);
        let mut t = Throttle::new(3600 * S);
        assert!(t.allow("a", clock.now()));
        assert!(!t.allow("a", clock.now()));
        assert!(t.allow("b", clock.now()));
        clock.advance(3599 * S);
        assert!(!t.allow("a", clock.now()));
        clock.advance(S);
        assert!(t.allow("a", clock.now()));
        assert!(!t.allow("a", clock.now()));
    }

    #[test]
    fn backoff_doubles_up_to_max_and_resets() {
        let clock = FakeClock::new(SystemTime::UNIX_EPOCH);
        let mut b = Backoff::new(10 * S, 25 * S);
        assert!(!b.blocked(7, clock.now()));

        b.failed(7, clock.now()); // 10 s
        clock.advance(9 * S);
        assert!(b.blocked(7, clock.now()));
        assert!(!b.blocked(8, clock.now()));
        clock.advance(S);
        assert!(!b.blocked(7, clock.now()));

        b.failed(7, clock.now()); // 20 s
        clock.advance(19 * S);
        assert!(b.blocked(7, clock.now()));
        clock.advance(S);
        assert!(!b.blocked(7, clock.now()));

        b.failed(7, clock.now()); // capped at 25 s
        clock.advance(24 * S);
        assert!(b.blocked(7, clock.now()));
        clock.advance(S);
        assert!(!b.blocked(7, clock.now()));

        b.failed(7, clock.now());
        b.succeeded(7);
        assert!(!b.blocked(7, clock.now()));

        b.failed(7, clock.now());
        b.retain(&HashSet::new());
        assert!(!b.blocked(7, clock.now()));
    }
}
