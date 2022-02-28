use std::{cmp::min, time::Duration};

pub struct Backoff {
    initial: u64,
    scale: f64,
    max: u64,
    next: u64,
}

impl Backoff {
    pub fn new(initial: u64, scale: f64, max: u64) -> Self {
        Backoff {
            initial,
            scale,
            max,
            next: initial,
        }
    }

    pub fn reset(&mut self) {
        self.next = self.initial;
    }

    pub fn next(&mut self) -> Duration {
        let result = Duration::from_millis(self.next);
        self.next = min(((self.next as f64) * self.scale) as u64, self.max);

        result
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff::new(1000, 1.2, 30000)
    }
}
