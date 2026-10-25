use std::collections::BTreeMap;

use mb_data::{Bar, Interval};
use serde::{Deserialize, Serialize};

use crate::error::FeatureError;
use crate::features_v1::{compute_features_v1, FEATURE_NAMES_V1, MIN_HISTORY_BARS_V1};
use crate::pit::PointInTime;
use crate::target::{compute_label, Direction, TargetSpec};

pub const DATASET_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatasetSpec {
    pub symbol: String,
    pub interval: Interval,
    /// Fixed decision cadence, measured in bars on the interval grid.
    pub decision_every_bars: u32,
    /// Minimum visible bars (including decision bar) before a decision is made.
    pub warmup_bars: usize,
    pub target: TargetSpec,
    /// Decision timestamps before this boundary are the training split. `None`
    /// leaves every row unassigned (no normalization fit).
    pub train_end_ms: Option<i64>,
}

impl Default for DatasetSpec {
    fn default() -> Self {
        Self {
            symbol: "BTCUSDT".to_string(),
            interval: Interval::M1,
            decision_every_bars: 1,
            warmup_bars: 64,
            target: TargetSpec::default(),
            train_end_ms: None,
        }
    }
}

impl DatasetSpec {
    pub fn validate(&self) -> Result<(), FeatureError> {
        if self.symbol.is_empty()
            || !self
                .symbol
                .chars()
                .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
        {
            return Err(FeatureError::InvalidSpec(format!(
                "symbol must be uppercase alphanumeric, got '{}'",
                self.symbol
            )));
        }
        if self.decision_every_bars == 0 {
            return Err(FeatureError::InvalidSpec(
                "decision_every_bars must be >= 1".to_string(),
            ));
        }
        if self.warmup_bars < MIN_HISTORY_BARS_V1 {
            return Err(FeatureError::InvalidSpec(format!(
                "warmup_bars must be >= {MIN_HISTORY_BARS_V1}, got {}",
                self.warmup_bars
            )));
        }
        if let Some(train_end) = self.train_end_ms {
            if train_end <= 0 {
                return Err(FeatureError::InvalidSpec(
                    "train_end_ms must be positive".to_string(),
                ));
            }
        }
        self.target.validate()?;
        Ok(())
    }

    /// Decisions are aligned to the interval grid, not to the first bar read,
    /// so the same timestamp is a decision point in every run.
    pub fn is_decision_bar(&self, bar: &Bar) -> bool {
        let interval_ms = self.interval.duration_ms();
        bar.open_time_ms > 0
            && bar.open_time_ms % interval_ms == 0
            && (bar.open_time_ms / interval_ms) % self.decision_every_bars as i64 == 0
    }

    pub fn split_for(&self, decision_time_ms: i64) -> Split {
        match self.train_end_ms {
            None => Split::Unassigned,
            Some(train_end) if decision_time_ms < train_end => Split::Train,
            Some(_) => Split::Holdout,
        }
    }

    pub fn decision_horizon_ms(&self) -> i64 {
        self.target.horizon_bars as i64 * self.interval.duration_ms()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Split {
    Unassigned,
    Train,
    Holdout,
}

impl Split {
    pub fn as_str(self) -> &'static str {
        match self {
            Split::Unassigned => "unassigned",
            Split::Train => "train",
            Split::Holdout => "holdout",
        }
    }
}

impl std::fmt::Display for Split {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatasetRow {
    pub schema_version: u32,
    pub symbol: String,
    pub interval: String,
    pub decision_time_ms: i64,
    pub decision_bar_open_time_ms: i64,
    pub entry_time_ms: i64,
    pub exit_time_ms: i64,
    pub features: Vec<f64>,
    pub fwd_log_return: f64,
    pub fwd_return_bps: f64,
    pub p_down: f64,
    pub p_flat: f64,
    pub p_up: f64,
    pub direction: Direction,
    pub expected_magnitude_abs: f64,
    pub magnitude_bps: f64,
    pub split: Split,
}

impl DatasetRow {
    pub fn feature_map(&self) -> Result<BTreeMap<&'static str, f64>, FeatureError> {
        if self.features.len() != FEATURE_NAMES_V1.len() {
            return Err(FeatureError::CorruptOutput(format!(
                "expected {} features, got {} at decision {}",
                FEATURE_NAMES_V1.len(),
                self.features.len(),
                self.decision_time_ms
            )));
        }
        Ok(FEATURE_NAMES_V1
            .iter()
            .copied()
            .zip(self.features.iter().copied())
            .collect())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildStats {
    pub bars_seen: u64,
    pub decisions_considered: u64,
    pub rows_written: u64,
    pub train_rows: u64,
    pub holdout_rows: u64,
    pub unassigned_rows: u64,
    pub dropped_gaps: u64,
    pub dropped_incomplete_horizon: u64,
    pub first_decision_time_ms: Option<i64>,
    pub last_decision_time_ms: Option<i64>,
}

/// Streaming point-in-time dataset builder.
///
/// It retains only `warmup + horizon + 2` bars in memory. Features are
/// computed from `PointInTime`, labels from the `horizon_bars` immediately
/// after the decision bar. Decisions lacking a complete forward window are
/// dropped, never imputed.
#[derive(Debug)]
pub struct StreamingDataset {
    spec: DatasetSpec,
    buffer: Vec<Bar>,
    total_bars: u64,
    max_buffer: usize,
    stats: BuildStats,
}

impl StreamingDataset {
    pub fn new(spec: DatasetSpec) -> Result<Self, FeatureError> {
        spec.validate()?;
        let max_buffer = spec
            .warmup_bars
            .saturating_add(spec.target.horizon_bars)
            .saturating_add(2);
        Ok(Self {
            spec,
            buffer: Vec::with_capacity(max_buffer + 8),
            total_bars: 0,
            max_buffer,
            stats: BuildStats::default(),
        })
    }

    pub fn spec(&self) -> &DatasetSpec {
        &self.spec
    }

    /// Add the next bar. Returns at most one completed row.
    pub fn push(&mut self, bar: Bar) -> Result<Option<DatasetRow>, FeatureError> {
        if let Some(last) = self.buffer.last() {
            if bar.open_time_ms <= last.open_time_ms {
                return Err(FeatureError::InvalidSpec(format!(
                    "bars must be fed in increasing order: {} after {}",
                    bar.open_time_ms, last.open_time_ms
                )));
            }
        }
        self.buffer.push(bar);
        self.total_bars += 1;
        self.stats.bars_seen += 1;

        let row = self.try_build_row()?;

        if self.buffer.len() > self.max_buffer {
            let excess = self.buffer.len() - self.max_buffer;
            self.buffer.drain(0..excess);
        }
        Ok(row)
    }

    fn try_build_row(&mut self) -> Result<Option<DatasetRow>, FeatureError> {
        let horizon = self.spec.target.horizon_bars;
        if self.buffer.len() <= horizon {
            return Ok(None);
        }
        let position = self.buffer.len() - 1 - horizon;
        let global_index = self.total_bars as usize - 1 - horizon;
        if global_index < self.spec.warmup_bars
            || !self.spec.is_decision_bar(&self.buffer[position])
        {
            return Ok(None);
        }
        self.stats.decisions_considered += 1;
        let pit = PointInTime::new(&self.buffer, position)?;
        let future = &self.buffer[position + 1..=position + horizon];
        let label = match compute_label(&pit, future, &self.spec.target) {
            Ok(label) => label,
            Err(FeatureError::IncompleteLabel(_)) => {
                self.stats.dropped_gaps += 1;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let features = compute_features_v1(&pit)?;
        let decision_bar = pit.decision_bar();
        let row = DatasetRow {
            schema_version: DATASET_SCHEMA_VERSION,
            symbol: self.spec.symbol.clone(),
            interval: self.spec.interval.as_str().to_string(),
            decision_time_ms: pit.decision_time_ms(),
            decision_bar_open_time_ms: decision_bar.open_time_ms,
            entry_time_ms: label.entry_time_ms,
            exit_time_ms: label.exit_time_ms,
            features,
            fwd_log_return: label.fwd_log_return,
            fwd_return_bps: label.fwd_return_bps,
            p_down: label.p_down,
            p_flat: label.p_flat,
            p_up: label.p_up,
            direction: label.direction,
            expected_magnitude_abs: label.expected_magnitude_abs,
            magnitude_bps: label.magnitude_bps,
            split: self.spec.split_for(pit.decision_time_ms()),
        };
        self.stats.rows_written += 1;
        match row.split {
            Split::Train => self.stats.train_rows += 1,
            Split::Holdout => self.stats.holdout_rows += 1,
            Split::Unassigned => self.stats.unassigned_rows += 1,
        }
        self.stats
            .first_decision_time_ms
            .get_or_insert(row.decision_time_ms);
        self.stats.last_decision_time_ms = Some(row.decision_time_ms);
        Ok(Some(row))
    }

    /// Close the stream and count decisions whose forward window never fully
    /// arrived. These are reported, never imputed.
    pub fn finish(mut self) -> BuildStats {
        let horizon = self.spec.target.horizon_bars as u64;
        let total = self.total_bars;
        let buffer_start = total as i64 - self.buffer.len() as i64;
        let mut dropped = 0u64;
        for global_index in total.saturating_sub(horizon)..total {
            if global_index < self.spec.warmup_bars as u64 {
                continue;
            }
            let offset = global_index as i64 - buffer_start;
            if offset >= 0 && (offset as usize) < self.buffer.len() {
                if self.spec.is_decision_bar(&self.buffer[offset as usize]) {
                    dropped += 1;
                }
            }
        }
        self.stats.dropped_incomplete_horizon = dropped;
        self.stats
    }

    pub fn stats(&self) -> &BuildStats {
        &self.stats
    }
}

/// In-memory convenience for tests and diagnostics. `max_rows` prevents
/// accidental unbounded allocation; use the CSV writer for full histories.
pub fn build_rows_from_bars(
    bars: &[Bar],
    spec: DatasetSpec,
    max_rows: usize,
) -> Result<(Vec<DatasetRow>, BuildStats), FeatureError> {
    let mut stream = StreamingDataset::new(spec)?;
    let mut rows = Vec::new();
    for bar in bars {
        if let Some(row) = stream.push(bar.clone())? {
            if rows.len() >= max_rows {
                return Err(FeatureError::InvalidSpec(format!(
                    "build_rows_from_bars exceeded cap of {max_rows} rows"
                )));
            }
            rows.push(row);
        }
    }
    let stats = stream.finish();
    Ok((rows, stats))
}
