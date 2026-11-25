//! Per-organization request rate limiting.
//!
//! Back-pressure already bounds *documents* in flight (`api.interactive_max_pending`),
//! which is what stops one tenant from filling this copy's pipeline. It does nothing
//! about request volume: a tenant polling `GET /v1/jobs/{id}` in a loop, or replaying a
//! batch upload, costs Postgres and the blob store on every call and never shows up as
//! an in-flight document. This is that limit.
//!
//! **Per process, deliberately.** There is nothing between the copies to share a counter
//! — the same reason admission control is per copy (D13) — so a fleet of N copies allows
//! N × `api.tenant_rate_per_s` in the worst case. That is the number to size against, and
//! it is stated here rather than implied: a shared limiter would need Redis or a
//! database round trip on the hot path, which costs more than the requests it is meant
//! to shed.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Buckets kept before the idle ones are dropped. A bucket is 24 bytes and a tenant that
/// stops calling leaves one behind, so the map is pruned rather than grown forever.
const MAX_BUCKETS: usize = 10_000;

struct Bucket {
    /// Requests this tenant may still make immediately.
    tokens: f64,
    /// When `tokens` was last brought up to date.
    last: Instant,
}

/// A token bucket per organization.
pub struct RateLimiter {
    per_s: f64,
    burst: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiter").field("per_s", &self.per_s).field("burst", &self.burst).finish_non_exhaustive()
    }
}

impl RateLimiter {
    /// `per_s` of zero disables the limiter: [`RateLimiter::check`] then always allows.
    pub fn new(per_s: f64, burst: f64) -> Self {
        Self {
            per_s: per_s.max(0.0),
            // A burst below one request would refuse the first call of an idle tenant,
            // which is a limiter that allows nothing.
            burst: burst.max(1.0),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.per_s > 0.0
    }

    /// Takes one request's worth of budget.
    ///
    /// `Ok(())` to proceed, or `Err(seconds)` to answer 429 with that `Retry-After` —
    /// how long until one token is back, rounded up to a whole second so a client that
    /// obeys it is not refused again immediately.
    pub fn check(&self, tenant: &str) -> Result<(), u64> {
        if !self.enabled() {
            return Ok(());
        }
        let now = Instant::now();
        let mut buckets = self.buckets.lock().expect("rate limiter mutex is never held across an await");
        if buckets.len() >= MAX_BUCKETS && !buckets.contains_key(tenant) {
            // Only full buckets go: a tenant mid-limit must keep its state, or the
            // eviction would hand it a fresh burst.
            buckets.retain(|_, b| b.tokens + b.last.elapsed().as_secs_f64() * self.per_s < self.burst);
        }
        let b = buckets.entry(tenant.to_string()).or_insert(Bucket { tokens: self.burst, last: now });
        b.tokens = (b.tokens + now.duration_since(b.last).as_secs_f64() * self.per_s).min(self.burst);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            let wait = (1.0 - b.tokens) / self.per_s;
            Err(wait.ceil().max(1.0) as u64)
        }
    }

    /// Buckets currently held, for the test that the map does not grow without bound.
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.buckets.lock().expect("mutex").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_is_allowed_and_then_the_rate_applies() {
        let l = RateLimiter::new(10.0, 3.0);
        for i in 0..3 {
            assert!(l.check("acme").is_ok(), "request {i} is within the burst");
        }
        let wait = l.check("acme").expect_err("the fourth is over the burst");
        assert!(wait >= 1, "Retry-After is a whole second at least, so an obedient client is not refused again");
    }

    #[test]
    fn one_organization_cannot_spend_anothers_budget() {
        let l = RateLimiter::new(1.0, 1.0);
        assert!(l.check("acme").is_ok());
        assert!(l.check("acme").is_err(), "acme is out");
        assert!(l.check("other").is_ok(), "and that says nothing about anyone else");
    }

    #[test]
    fn a_rate_of_zero_is_no_limit_at_all() {
        let l = RateLimiter::new(0.0, 1.0);
        assert!(!l.enabled());
        for _ in 0..1000 {
            assert!(l.check("acme").is_ok());
        }
    }

    #[test]
    fn tokens_come_back_with_time() {
        let l = RateLimiter::new(1000.0, 1.0);
        assert!(l.check("acme").is_ok());
        assert!(l.check("acme").is_err());
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(l.check("acme").is_ok(), "5 ms at 1000/s is five tokens");
    }

    #[test]
    fn the_bucket_map_does_not_grow_without_bound() {
        let l = RateLimiter::new(1_000_000.0, 1.0);
        for i in 0..MAX_BUCKETS + 100 {
            let _ = l.check(&format!("t{i}"));
        }
        assert!(l.tracked() <= MAX_BUCKETS + 1, "idle buckets are pruned, not kept: {} tracked", l.tracked());
    }

    #[test]
    fn a_limited_tenant_keeps_its_state_when_the_map_is_pruned() {
        // The eviction must not be a way to get a fresh burst: fill one tenant's bucket,
        // then push the map past its cap with tenants that are idle.
        let l = RateLimiter::new(0.000_001, 1.0);
        assert!(l.check("acme").is_ok());
        assert!(l.check("acme").is_err());
        for i in 0..MAX_BUCKETS + 10 {
            let _ = l.check(&format!("t{i}"));
        }
        assert!(l.check("acme").is_err(), "acme's empty bucket survived the prune");
    }
}
