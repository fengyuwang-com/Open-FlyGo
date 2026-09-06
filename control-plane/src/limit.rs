// limit.rs - minimal fixed-window, in-memory rate limiter (per-key).
//
// Enough for an MVP: register / activate / admin-login abuse protection.
// A real deployment behind a reverse proxy should also enforce limits
// at the proxy layer (nginx limit_req, fail2ban, etc.).

use crate::api::ApiError;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

struct Window {
    start: i64,
    count: u32,
}

#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<HashMap<String, Window>>>,
    max_entries: usize,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            max_entries: 10_000,
        }
    }

    /// Allow at most `limit` calls per `window_secs` per key.
    /// Returns Err(429) once the limit is exceeded.
    pub fn check(&self, key: &str, limit: u32, window_secs: i64, now: i64) -> Result<(), ApiError> {
        let mut map = self.inner.lock().unwrap();
        let entry = map
            .entry(key.to_string())
            .or_insert(Window { start: now, count: 0 });
        if now - entry.start >= window_secs {
            entry.start = now;
            entry.count = 0;
        }
        entry.count += 1;
        if entry.count > limit {
            // Opportunistic cleanup so the map cannot grow unbounded.
            if map.len() > self.max_entries {
                map.retain(|_, w| now - w.start < window_secs);
            }
            return Err(ApiError::too_many_requests(
                "rate limit exceeded, try again later",
            ));
        }
        Ok(())
    }
}
