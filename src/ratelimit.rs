// token bucket rate limiter. one bucket per client key (api token, or
// ip:port when no auth is configured), refilled lazily on each check.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

struct Bucket {
    tokens: f64,
    last: Instant,
}

pub struct RateLimiter {
    enabled: bool,
    capacity: f64,
    refill_per_sec: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    pub fn new(enabled: bool, capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            enabled,
            capacity,
            refill_per_sec,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    #[allow(dead_code)]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    // returns true when the request is allowed
    pub fn check(&self, key: &str) -> bool {
        if !self.enabled {
            return true;
        }

        let now = Instant::now();
        let mut buckets = match self.buckets.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        let bucket = buckets
            .entry(key.to_string())
            .and_modify(|bucket| {
                let elapsed = now.duration_since(bucket.last).as_secs_f64();
                bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
                bucket.last = now;
            })
            .or_insert(Bucket {
                tokens: self.capacity,
                last: now,
            });

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    // best-effort cleanup so long-running instances don't grow forever
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut buckets = match self.buckets.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        buckets.retain(|_, bucket| {
            now.duration_since(bucket.last).as_secs_f64() < self.capacity / self.refill_per_sec.max(0.001) * 2.0 + 60.0
        });
    }
}
