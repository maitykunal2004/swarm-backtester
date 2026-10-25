use serde::Serialize;
use std::time::Duration;

/// Binance spot IP weight budget per rolling minute. The public default is
/// 6000; we default to a conservative share so this tool can coexist with
/// other clients on the same egress IP.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct RateLimitPolicy {
    pub weight_budget_per_minute: u32,
    /// Extra headroom kept unused at all times.
    pub safety_margin: u32,
}

impl Default for RateLimitPolicy {
    fn default() -> Self {
        Self {
            weight_budget_per_minute: 1_200,
            safety_margin: 60,
        }
    }
}

impl RateLimitPolicy {
    pub fn usable_budget(&self) -> u32 {
        self.weight_budget_per_minute
            .saturating_sub(self.safety_margin)
            .max(1)
    }
}

/// Request weight for `/api/v3/klines` by limit, per Binance documentation.
pub fn klines_weight(limit: u16) -> u32 {
    match limit {
        1..=100 => 1,
        101..=500 => 2,
        501..=1000 => 5,
        1001..=1500 => 10,
        _ => 20,
    }
}

/// A rolling one-minute local weight limiter. The exchange-reported used
/// weight header is folded in when available so the local estimate never runs
/// far ahead of reality.
#[derive(Debug, Clone)]
pub struct WeightLimiter {
    policy: RateLimitPolicy,
    window_start_ms: i64,
    used_weight: u32,
    /// Active window's server-reported weight hint.
    server_weight_hint: u32,
    /// Lifetime maximum, kept for reporting across window resets.
    server_used_weight_max: u32,
}

impl WeightLimiter {
    pub fn new(policy: RateLimitPolicy) -> Self {
        Self {
            policy,
            window_start_ms: 0,
            used_weight: 0,
            server_weight_hint: 0,
            server_used_weight_max: 0,
        }
    }

    pub fn policy(&self) -> RateLimitPolicy {
        self.policy
    }

    pub fn used_weight(&self) -> u32 {
        self.used_weight
    }

    pub fn server_used_weight_max(&self) -> u32 {
        self.server_used_weight_max
    }

    /// Attempt to reserve `weight` at `now_ms`.
    ///
    /// Returns `Duration::ZERO` when the caller may proceed immediately, or
    /// the time to wait before retrying. The caller should sleep and retry;
    /// this keeps the limiter clock-injectable and unit-testable.
    pub fn acquire_at(&mut self, weight: u32, now_ms: i64) -> Duration {
        let budget = self.policy.usable_budget();
        if weight > budget {
            // A single request can never fit; wait out the window and then
            // still allow it (the fetch layer also caps limit at 1000).
            return Duration::from_millis(
                (60_000 - (now_ms - self.window_start_ms).clamp(0, 60_000)) as u64,
            );
        }
        if self.window_start_ms == 0 {
            self.window_start_ms = now_ms;
            self.used_weight = 0;
        } else if now_ms - self.window_start_ms >= 60_000 {
            self.window_start_ms = now_ms;
            self.used_weight = 0;
            self.server_weight_hint = 0;
        }
        let effective = self.used_weight.max(self.server_weight_hint);
        if effective + weight <= budget {
            self.used_weight = self.used_weight.saturating_add(weight);
            return Duration::ZERO;
        }
        let wait_ms = 60_000 - (now_ms - self.window_start_ms).clamp(0, 60_000);
        Duration::from_millis(wait_ms.max(1) as u64)
    }

    /// Fold in `X-MBX-USED-WEIGHT-1M` from a response.
    pub fn observe_server_weight(&mut self, used_weight: u32) {
        self.server_weight_hint = self.server_weight_hint.max(used_weight);
        self.used_weight = self.used_weight.max(used_weight);
        self.server_used_weight_max = self.server_used_weight_max.max(used_weight);
    }

    pub fn reset_window(&mut self, now_ms: i64) {
        self.window_start_ms = now_ms;
        self.used_weight = 0;
        self.server_weight_hint = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weight_table_matches_binance_docs() {
        assert_eq!(klines_weight(100), 1);
        assert_eq!(klines_weight(101), 2);
        assert_eq!(klines_weight(500), 2);
        assert_eq!(klines_weight(501), 5);
        assert_eq!(klines_weight(1000), 5);
    }

    #[test]
    fn limiter_waits_when_budget_is_used_up() {
        let policy = RateLimitPolicy {
            weight_budget_per_minute: 100,
            safety_margin: 0,
        };
        let mut limiter = WeightLimiter::new(policy);
        assert_eq!(limiter.acquire_at(60, 1_000), Duration::ZERO);
        assert_eq!(limiter.acquire_at(30, 2_000), Duration::ZERO);
        let wait = limiter.acquire_at(20, 3_000);
        assert_eq!(wait, Duration::from_millis(58_000));
        // A new window resets the budget.
        assert_eq!(limiter.acquire_at(20, 61_001), Duration::ZERO);
    }

    #[test]
    fn server_weight_is_respected_and_resets_with_window() {
        let policy = RateLimitPolicy {
            weight_budget_per_minute: 100,
            safety_margin: 0,
        };
        let mut limiter = WeightLimiter::new(policy);
        limiter.observe_server_weight(95);
        assert_eq!(limiter.acquire_at(5, 10_000), Duration::ZERO);
        assert!(limiter.acquire_at(6, 11_000) > Duration::ZERO);
        assert_eq!(limiter.acquire_at(6, 71_000), Duration::ZERO);
        assert_eq!(limiter.server_used_weight_max(), 95);
    }
}
