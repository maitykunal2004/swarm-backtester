use crate::error::FeatureError;
use crate::pit::PointInTime;

pub const FEATURE_SET_VERSION: &str = "v1";
/// Minimum visible bars including the decision bar.
pub const MIN_HISTORY_BARS_V1: usize = 21;
pub const FEATURE_NAMES_V1: [&str; 10] = [
    "ret_1_bps",
    "ret_5_bps",
    "ret_15_bps",
    "realized_vol_20_bps",
    "range_bps",
    "close_location",
    "sma20_gap_bps",
    "volume_z_20",
    "taker_buy_ratio_20",
    "trades_z_20",
];

/// Compute the v1 feature vector using only the point-in-time view.
///
/// Every input comes from `pit.bars()`, which ends at the decision bar. The
/// function is pure and deterministic.
pub fn compute_features_v1(pit: &PointInTime<'_>) -> Result<Vec<f64>, FeatureError> {
    let bars = pit.bars();
    if bars.len() < MIN_HISTORY_BARS_V1 {
        return Err(FeatureError::InsufficientHistory {
            decision_time_ms: pit.decision_time_ms(),
            needed: MIN_HISTORY_BARS_V1,
            available: bars.len(),
        });
    }
    let n = bars.len();
    let close = |index: usize| bars[index].close;
    let current_close = close(n - 1);
    let current = pit.decision_bar();

    let ret_1 = log_return_bps(current_close, close(n - 2));
    let ret_5 = log_return_bps(current_close, close(n - 6));
    let ret_15 = log_return_bps(current_close, close(n - 16));

    let returns: Vec<f64> = (n - 20..n)
        .map(|index| (close(index) / close(index - 1)).ln())
        .collect();
    let realized_vol_20_bps = stddev(&returns) * 10_000.0;

    let range = current.high - current.low;
    let range_bps = range / current_close * 10_000.0;
    let close_location = if range > current_close * 1e-12 {
        (current.close - current.low) / range
    } else {
        0.5
    };

    let closes: Vec<f64> = (n - 20..n).map(close).collect();
    let sma20 = mean(&closes);
    let sma20_gap_bps = (current_close / sma20 - 1.0) * 10_000.0;

    let volumes: Vec<f64> = (n - 20..n).map(|index| bars[index].volume).collect();
    let volume_mean = mean(&volumes);
    let volume_std = stddev(&volumes);
    let volume_z_20 = if volume_std > 1e-12 {
        (current.volume - volume_mean) / volume_std
    } else {
        0.0
    };

    let mut taker_sum = 0.0;
    let mut volume_sum = 0.0;
    let mut trades: Vec<f64> = Vec::with_capacity(20);
    for index in n - 20..n {
        taker_sum += bars[index].taker_buy_base_volume;
        volume_sum += bars[index].volume;
        trades.push(bars[index].trades as f64);
    }
    let taker_buy_ratio_20 = if volume_sum > 1e-12 {
        taker_sum / volume_sum
    } else {
        0.5
    };
    let trades_std = stddev(&trades);
    let trades_z_20 = if trades_std > 1e-12 {
        (current.trades as f64 - mean(&trades)) / trades_std
    } else {
        0.0
    };

    let features = vec![
        ret_1,
        ret_5,
        ret_15,
        realized_vol_20_bps,
        range_bps,
        close_location,
        sma20_gap_bps,
        volume_z_20,
        taker_buy_ratio_20,
        trades_z_20,
    ];
    for (name, value) in FEATURE_NAMES_V1.iter().zip(features.iter()) {
        if !value.is_finite() {
            return Err(FeatureError::NonFiniteFeature {
                feature: (*name).to_string(),
                decision_time_ms: pit.decision_time_ms(),
                value: *value,
            });
        }
    }
    Ok(features)
}

fn log_return_bps(current: f64, previous: f64) -> f64 {
    (current / previous).ln() * 10_000.0
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

fn stddev(values: &[f64]) -> f64 {
    let average = mean(values);
    let variance = values
        .iter()
        .map(|value| {
            let delta = value - average;
            delta * delta
        })
        .sum::<f64>()
        / values.len() as f64;
    variance.sqrt()
}
