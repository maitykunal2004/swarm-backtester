use mb_data::Bar;
use serde::{Deserialize, Serialize};

use crate::error::FeatureError;
use crate::pit::PointInTime;

pub const TARGET_SCHEMA_VERSION: u32 = 1;

/// Fixed-horizon forecast target definition.
///
/// At every decision timestamp the model must predict:
/// - a categorical distribution over down / flat / up forward returns, and
/// - the expected absolute forward magnitude.
///
/// Labels are realized strictly after the decision timestamp: from the open of
/// the first bar after the decision bar through the close of the horizon bar.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TargetSpec {
    /// Fixed forecast horizon in bars (>= 1).
    pub horizon_bars: usize,
    /// Absolute forward return (bps) at or below which the outcome is flat.
    pub flat_band_bps: f64,
}

impl Default for TargetSpec {
    fn default() -> Self {
        Self {
            horizon_bars: 12,
            flat_band_bps: 10.0,
        }
    }
}

impl TargetSpec {
    pub fn validate(&self) -> Result<(), FeatureError> {
        if self.horizon_bars == 0 {
            return Err(FeatureError::InvalidSpec(
                "target horizon_bars must be >= 1".to_string(),
            ));
        }
        if !self.flat_band_bps.is_finite() || self.flat_band_bps < 0.0 {
            return Err(FeatureError::InvalidSpec(format!(
                "flat_band_bps must be finite and non-negative, got {}",
                self.flat_band_bps
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Down,
    Flat,
    Up,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Down => "down",
            Direction::Flat => "flat",
            Direction::Up => "up",
        }
    }

    pub fn index(self) -> usize {
        match self {
            Direction::Down => 0,
            Direction::Flat => 1,
            Direction::Up => 2,
        }
    }

    pub fn from_return_bps(return_bps: f64, flat_band_bps: f64) -> Self {
        if return_bps > flat_band_bps {
            Direction::Up
        } else if return_bps < -flat_band_bps {
            Direction::Down
        } else {
            Direction::Flat
        }
    }
}

impl std::fmt::Display for Direction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Realized fixed-horizon outcome. `p_*` are a one-hot categorical target
/// today; soft/quantile target builders can replace them later without
/// changing the row schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetLabel {
    pub entry_time_ms: i64,
    pub exit_time_ms: i64,
    pub fwd_log_return: f64,
    pub fwd_return_bps: f64,
    pub p_down: f64,
    pub p_flat: f64,
    pub p_up: f64,
    pub direction: Direction,
    pub expected_magnitude_abs: f64,
    pub magnitude_bps: f64,
}

/// Compute the label from bars strictly after the decision timestamp. The
/// decision bar's prices are never used as entry or exit.
pub fn compute_label(
    decision: &PointInTime<'_>,
    future_bars: &[Bar],
    spec: &TargetSpec,
) -> Result<TargetLabel, FeatureError> {
    spec.validate()?;
    if future_bars.len() != spec.horizon_bars {
        return Err(FeatureError::IncompleteLabel(format!(
            "need {} future bars, got {}",
            spec.horizon_bars,
            future_bars.len()
        )));
    }
    let decision_time_ms = decision.decision_time_ms();
    let entry_bar = &future_bars[0];
    if entry_bar.open_time_ms <= decision_time_ms {
        return Err(FeatureError::Leakage(format!(
            "label entry at {} is not strictly after decision time {decision_time_ms}",
            entry_bar.open_time_ms
        )));
    }
    for pair in future_bars.windows(2) {
        if pair[1].open_time_ms != pair[0].close_time_ms + 1 {
            return Err(FeatureError::IncompleteLabel(format!(
                "gap inside label horizon between {} and {}",
                pair[0].open_time_ms, pair[1].open_time_ms
            )));
        }
    }
    let exit_bar = &future_bars[spec.horizon_bars - 1];
    if exit_bar.close_time_ms <= decision_time_ms {
        return Err(FeatureError::Leakage(format!(
            "label exit at {} is not after decision time {decision_time_ms}",
            exit_bar.close_time_ms
        )));
    }
    let entry_price = entry_bar.open;
    let exit_price = exit_bar.close;
    if !entry_price.is_finite() || entry_price <= 0.0 || !exit_price.is_finite() || exit_price <= 0.0
    {
        return Err(FeatureError::IncompleteLabel(format!(
            "non-positive label prices: entry {entry_price}, exit {exit_price}"
        )));
    }
    let fwd_log_return = (exit_price / entry_price).ln();
    let fwd_return_bps = fwd_log_return * 10_000.0;
    let direction = Direction::from_return_bps(fwd_return_bps, spec.flat_band_bps);
    let (p_down, p_flat, p_up) = match direction {
        Direction::Down => (1.0, 0.0, 0.0),
        Direction::Flat => (0.0, 1.0, 0.0),
        Direction::Up => (0.0, 0.0, 1.0),
    };
    let absolute = fwd_log_return.abs();
    Ok(TargetLabel {
        entry_time_ms: entry_bar.open_time_ms,
        exit_time_ms: exit_bar.close_time_ms,
        fwd_log_return,
        fwd_return_bps,
        p_down,
        p_flat,
        p_up,
        direction,
        expected_magnitude_abs: absolute,
        magnitude_bps: absolute * 10_000.0,
    })
}
