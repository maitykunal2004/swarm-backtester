use mb_data::Bar;
use serde::Serialize;

use crate::dataset::{build_rows_from_bars, DatasetRow, DatasetSpec, Split};
use crate::error::FeatureError;
use crate::features_v1::FEATURE_NAMES_V1;
use crate::normalize::{Normalizer, RunningStats};
use crate::pit::PointInTime;
use crate::target::TargetSpec;

/// Deterministic synthetic OHLCV bars for offline tests. Prices follow a
/// seeded random walk with contiguous one-minute candles.
pub fn synthetic_bars(count: usize, start_ms: i64, interval_ms: i64, seed: u64) -> Vec<Bar> {
    let mut state = seed
        .wrapping_mul(2_862_933_555_777_941_757)
        .wrapping_add(3_037_000_493);
    let mut price = 42_000.0;
    let mut bars = Vec::with_capacity(count);
    for index in 0..count {
        let open_time_ms = start_ms + index as i64 * interval_ms;
        let open = price;
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let unit = ((state >> 11) as f64) / ((1u64 << 53) as f64);
        let log_return = (unit - 0.5) * 0.003;
        let close = open * log_return.exp();
        let wick = open * (0.0002 + 0.0008 * unit);
        let high = open.max(close) + wick;
        let low = (open.min(close) - wick).max(1.0);
        let volume = 50.0 + 450.0 * unit;
        let taker_buy_base_volume = volume * (0.3 + 0.4 * unit);
        let trades = 100 + (unit * 900.0) as u64;
        bars.push(Bar {
            open_time_ms,
            open,
            high,
            low,
            close,
            volume,
            close_time_ms: open_time_ms + interval_ms - 1,
            quote_volume: volume * close,
            trades,
            taker_buy_base_volume,
            taker_buy_quote_volume: taker_buy_base_volume * close,
        });
        price = close;
    }
    bars
}

#[derive(Debug, Clone, Serialize)]
pub struct SelfTestCheck {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LeakageSelfTestReport {
    pub checks: Vec<SelfTestCheck>,
    pub passed: bool,
    pub rows: usize,
    pub train_rows: usize,
    pub holdout_rows: usize,
    pub decision_time_ms: i64,
}

impl LeakageSelfTestReport {
    fn record(&mut self, name: &str, passed: bool, detail: impl Into<String>) {
        self.checks.push(SelfTestCheck {
            name: name.to_string(),
            passed,
            detail: detail.into(),
        });
        if !passed {
            self.passed = false;
        }
    }

    pub fn failures(&self) -> Vec<&SelfTestCheck> {
        self.checks.iter().filter(|check| !check.passed).collect()
    }
}

fn find_row(rows: &[DatasetRow], decision_time_ms: i64) -> Option<&DatasetRow> {
    rows.iter()
        .find(|row| row.decision_time_ms == decision_time_ms)
}

fn same_features(left: &[f64], right: &[f64]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right.iter())
            .all(|(a, b)| a.to_bits() == b.to_bits())
}

fn same_label(left: &DatasetRow, right: &DatasetRow) -> bool {
    left.entry_time_ms == right.entry_time_ms
        && left.exit_time_ms == right.exit_time_ms
        && left.fwd_log_return.to_bits() == right.fwd_log_return.to_bits()
        && left.fwd_return_bps.to_bits() == right.fwd_return_bps.to_bits()
        && left.p_down.to_bits() == right.p_down.to_bits()
        && left.p_flat.to_bits() == right.p_flat.to_bits()
        && left.p_up.to_bits() == right.p_up.to_bits()
        && left.direction == right.direction
        && left.expected_magnitude_abs.to_bits() == right.expected_magnitude_abs.to_bits()
}

/// Offline, deterministic leakage self-test.
///
/// It proves both directions of the guard:
/// 1. corrupting every bar after the decision leaves features unchanged;
/// 2. corrupting every bar up to and including the decision leaves labels
///    unchanged;
/// 3. labels actually depend on future data (they change when it is corrupted);
/// 4. features actually depend on past data (they change when it is corrupted);
/// 5. normalization statistics are invariant to future corruption and reject
///    future rows outright.
pub fn run_leakage_selftest() -> Result<LeakageSelfTestReport, FeatureError> {
    let start_ms = 1_704_067_200_000i64;
    let interval_ms = 60_000i64;
    let bars = synthetic_bars(600, start_ms, interval_ms, 20_240_101);
    let train_end_ms = bars[220].decision_time_ms();
    let spec = DatasetSpec {
        symbol: "BTCUSDT".to_string(),
        interval: mb_data::Interval::M1,
        decision_every_bars: 2,
        warmup_bars: 40,
        target: TargetSpec {
            horizon_bars: 6,
            flat_band_bps: 5.0,
        },
        train_end_ms: Some(train_end_ms),
    };
    let (rows, stats) = build_rows_from_bars(&bars, spec.clone(), 100_000)?;
    let mut report = LeakageSelfTestReport {
        checks: Vec::new(),
        passed: true,
        rows: rows.len(),
        train_rows: stats.train_rows as usize,
        holdout_rows: stats.holdout_rows as usize,
        decision_time_ms: 0,
    };
    if rows.len() < 50 {
        return Err(FeatureError::InvalidSpec(format!(
            "selftest produced only {} rows",
            rows.len()
        )));
    }

    let base = rows[rows.len() * 2 / 3].clone();
    report.decision_time_ms = base.decision_time_ms;
    let decision_index = ((base.decision_time_ms - start_ms) / interval_ms) as usize;
    let horizon = spec.target.horizon_bars;
    let last_bar = bars.last().expect("synthetic bars");

    report.record(
        "splits_present",
        stats.train_rows > 0 && stats.holdout_rows > 0,
        format!("train={} holdout={}", stats.train_rows, stats.holdout_rows),
    );
    report.record(
        "feature_vector_shape",
        base.features.len() == FEATURE_NAMES_V1.len()
            && base.features.iter().all(|value| value.is_finite()),
        format!("features={}", base.features.len()),
    );

    // Label values must equal a hand-computed forward return over bars t+1..t+H.
    let expected_entry = bars[decision_index + 1].open;
    let expected_exit = bars[decision_index + horizon].close;
    let expected_bps = (expected_exit / expected_entry).ln() * 10_000.0;
    report.record(
        "label_math_matches_forward_window",
        (base.fwd_return_bps - expected_bps).abs() < 1e-9
            && base.entry_time_ms == bars[decision_index + 1].open_time_ms
            && base.exit_time_ms == bars[decision_index + horizon].close_time_ms,
        format!(
            "label={:.6}bps expected={:.6}bps",
            base.fwd_return_bps, expected_bps
        ),
    );
    report.record(
        "label_class_matches_threshold",
        base.direction
            == crate::target::Direction::from_return_bps(expected_bps, spec.target.flat_band_bps),
        format!("direction={}", base.direction),
    );

    // Every label must start strictly after the decision timestamp and all
    // rows must have a fully observed horizon.
    let labels_start_after = rows.iter().all(|row| {
        row.entry_time_ms > row.decision_time_ms && row.exit_time_ms > row.entry_time_ms
    });
    report.record(
        "labels_start_after_decision",
        labels_start_after,
        format!(
            "decision={} entry={}",
            base.decision_time_ms, base.entry_time_ms
        ),
    );
    let complete_horizons = rows
        .iter()
        .all(|row| row.exit_time_ms == row.decision_time_ms + horizon as i64 * interval_ms)
        && rows
            .iter()
            .all(|row| row.exit_time_ms <= last_bar.close_time_ms);
    report.record(
        "no_incomplete_horizon_rows",
        complete_horizons && stats.dropped_incomplete_horizon > 0,
        format!(
            "dropped_incomplete={} last_exit={}",
            stats.dropped_incomplete_horizon, last_bar.close_time_ms
        ),
    );

    // PointInTime cannot reach the future.
    let pit = PointInTime::new(&bars, decision_index)?;
    let pit_hides_future = pit.bars().len() == decision_index + 1
        && pit.bars().last().map(|bar| bar.open_time_ms) == Some(bars[decision_index].open_time_ms)
        && PointInTime::new(&bars, bars.len()).is_err();
    report.record(
        "point_in_time_view_hides_future",
        pit_hides_future,
        format!("visible_bars={}", pit.len()),
    );

    // 1) Future corruption must not change the decision row's features.
    let mut future_corrupted = bars.clone();
    for (index, bar) in future_corrupted
        .iter_mut()
        .enumerate()
        .skip(decision_index + 1)
    {
        let factor = 1.0 + 0.08 * (index - decision_index) as f64;
        bar.open *= factor;
        bar.high *= factor;
        bar.low *= factor;
        bar.close *= factor;
        bar.volume *= 3.0;
        bar.trades = bar.trades.saturating_add(100);
    }
    let (future_rows, future_stats) =
        build_rows_from_bars(&future_corrupted, spec.clone(), 100_000)?;
    let future_row = find_row(&future_rows, base.decision_time_ms).ok_or_else(|| {
        FeatureError::CorruptOutput("future-corrupted dataset lost the decision row".to_string())
    })?;
    report.record(
        "future_data_does_not_change_features",
        same_features(&base.features, &future_row.features),
        "features must be bit-identical after corrupting all bars after the decision".to_string(),
    );
    report.record(
        "labels_depend_on_future",
        !same_label(&base, future_row),
        format!(
            "original={:.6}bps corrupted={:.6}bps",
            base.fwd_return_bps, future_row.fwd_return_bps
        ),
    );
    report.record(
        "future_corruption_preserves_row_counts",
        future_stats.rows_written == stats.rows_written,
        format!(
            "original={} corrupted={}",
            stats.rows_written, future_stats.rows_written
        ),
    );

    // 2) Corrupting the past/decision bar must change features but not labels.
    let mut past_corrupted = bars.clone();
    for (index, bar) in past_corrupted
        .iter_mut()
        .enumerate()
        .take(decision_index + 1)
    {
        let factor = 1.0 + 0.003 * ((index % 9) as f64 - 4.0);
        bar.open *= factor;
        bar.high *= factor;
        bar.low *= factor;
        bar.close *= factor;
        bar.volume *= 1.0 + 0.15 * (index % 5) as f64;
        bar.trades = bar.trades.saturating_add(1 + (index % 3) as u64);
    }
    let (past_rows, _) = build_rows_from_bars(&past_corrupted, spec.clone(), 100_000)?;
    let past_row = find_row(&past_rows, base.decision_time_ms).ok_or_else(|| {
        FeatureError::CorruptOutput("past-corrupted dataset lost the decision row".to_string())
    })?;
    report.record(
        "past_data_changes_features",
        !same_features(&base.features, &past_row.features),
        "features must actually depend on history (not be constants)".to_string(),
    );
    report.record(
        "labels_ignore_past_and_decision_bar",
        same_label(&base, past_row),
        "labels must be unchanged when only bars at/before the decision are corrupted".to_string(),
    );

    // 3) Normalizer fit guard and future invariance.
    let baseline_normalizer = Normalizer::fit(&rows, &spec)?;
    let future_normalizer = Normalizer::fit(&future_rows, &spec)?;
    report.record(
        "normalizer_rejects_future_rows",
        {
            let mut guard = RunningStats::new(FEATURE_NAMES_V1.len());
            let mut sneaky = future_row.clone();
            sneaky.split = Split::Train;
            matches!(
                guard.push_train_row(&sneaky, train_end_ms),
                Err(FeatureError::Leakage(_))
            )
        },
        "a future row relabelled as train must be rejected".to_string(),
    );
    report.record(
        "normalizer_stats_ignore_future_data",
        baseline_normalizer.means == future_normalizer.means
            && baseline_normalizer.scales == future_normalizer.scales,
        format!("train_rows={}", baseline_normalizer.train_rows),
    );
    report.record(
        "normalizer_fit_window_is_train_only",
        baseline_normalizer.train_end_ms == train_end_ms
            && baseline_normalizer.train_rows == stats.train_rows,
        format!(
            "fitted_rows={} train_rows={} train_end={}",
            baseline_normalizer.train_rows, stats.train_rows, train_end_ms
        ),
    );

    report.record(
        "feature_names_stable",
        baseline_normalizer.feature_names.len() == FEATURE_NAMES_V1.len(),
        format!("names={}", baseline_normalizer.feature_names.len()),
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leakage_selftest_passes() {
        let report = run_leakage_selftest().unwrap();
        let failures: Vec<String> = report
            .failures()
            .iter()
            .map(|check| format!("{}: {}", check.name, check.detail))
            .collect();
        assert!(
            report.passed,
            "leakage self-test failed: {}",
            failures.join("; ")
        );
        assert!(report.checks.len() >= 10);
        assert!(report.rows > 100);
    }

    #[test]
    fn synthetic_bars_are_contiguous_and_valid() {
        let bars = synthetic_bars(100, 1_704_067_200_000, 60_000, 5);
        assert_eq!(bars.len(), 100);
        for pair in bars.windows(2) {
            assert_eq!(pair[1].open_time_ms, pair[0].close_time_ms + 1);
        }
        for bar in &bars {
            bar.validate().unwrap();
        }
    }
}
