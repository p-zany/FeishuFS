use std::time::Duration;

use parking_lot::{Condvar, Mutex};

/// Bounded concurrency, not an application-wide API rate limiter.
pub(crate) struct FetchSlots {
    active: Mutex<usize>,
    available: Condvar,
    capacity: usize,
}

impl FetchSlots {
    pub fn new(capacity: usize) -> Self {
        Self {
            active: Mutex::new(0),
            available: Condvar::new(),
            capacity,
        }
    }

    pub fn acquire(&self) -> FetchPermit<'_> {
        let mut active = self.active.lock();
        while *active >= self.capacity {
            self.available.wait(&mut active);
        }
        *active += 1;
        FetchPermit(self)
    }
}

pub(crate) struct FetchPermit<'a>(&'a FetchSlots);

impl Drop for FetchPermit<'_> {
    fn drop(&mut self) {
        *self.0.active.lock() -= 1;
        self.0.available.notify_one();
    }
}

/// Fixed-delay scheduling with capped backoff after failed directory traversals.
pub(crate) struct RefreshBackoff {
    base: Duration,
    delay: Duration,
}

impl RefreshBackoff {
    pub fn new(base: Duration) -> Self {
        Self { base, delay: base }
    }

    pub fn complete(&mut self, succeeded: bool) -> Duration {
        self.delay = if succeeded {
            self.base
        } else {
            self.delay
                .saturating_mul(2)
                .min(self.base.saturating_mul(8))
        };
        self.delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_capped_and_success_resets_it() {
        let base = Duration::from_secs(60);
        let mut backoff = RefreshBackoff::new(base);
        assert_eq!(backoff.complete(false), base * 2);
        assert_eq!(backoff.complete(false), base * 4);
        assert_eq!(backoff.complete(false), base * 8);
        assert_eq!(backoff.complete(false), base * 8);
        assert_eq!(backoff.complete(true), base);
        assert_eq!(backoff.complete(false), base * 2);
    }
}
