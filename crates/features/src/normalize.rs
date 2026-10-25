use serde::{Deserialize, Serialize};

use crate::dataset::{DatasetRow, DatasetSpec, Split};
use crate::error::FeatureError;
use crate::features_v1::FEATURE_NAMES_V1;

/// Train-only standardization statistics.
///
/// The fit path rejects any row that is not in the training split or whose
/// decision timestamp is at/after `train_end_ms`, so future rows cannot
/// influence the mean or scale.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Normalizer {
    pub feature_names: Vec<String>,
    pub means: Vec<f64>,
    pub scales: Vec<f64>,
    pub train_rows: u64,
    pub train_end_ms: i64,
}

#[derive(Debug, Clone)]
pub struct RunningStats {
    count: u64,
    means: Vec<f64>,
    m2: Vec<f64>,
    feature_count: usize,
}

impl RunningStats {
    pub fn new(feature_count: usize) -> Self {
        Self {
            count: 0,
            means: vec![0.0; feature_count],
            m2: vec![0.0; feature_count],
            feature_count,
        }
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// Streaming Welford update with an explicit point-in-time guard.
    pub fn push_train_row(
        &mut self,
        row: &DatasetRow,
        train_end_ms: i64,
    ) -> Result<(), FeatureError> {
        if row.split != Split::Train {
            return Err(FeatureError::Leakage(format!(
                "normalizer fit received a {} row at decision {}",
                row.split, row.decision_time_ms
            )));
        }
        if row.decision_time_ms >= train_end_ms {
            return Err(FeatureError::Leakage(format!(
                "normalizer fit row at {} is not before train_end_ms {}",
                row.decision_time_ms, train_end_ms
            )));
        }
        if row.features.len() != self.feature_count {
            return Err(FeatureError::CorruptOutput(format!(
                "expected {} features, got {}",
                self.feature_count,
                row.features.len()
            )));
        }
        for (name, value) in FEATURE_NAMES_V1.iter().zip(row.features.iter()) {
            if !value.is_finite() {
                return Err(FeatureError::NonFiniteFeature {
                    feature: (*name).to_string(),
                    decision_time_ms: row.decision_time_ms,
                    value: *value,
                });
            }
        }
        self.count += 1;
        for index in 0..self.feature_count {
            let delta = row.features[index] - self.means[index];
            self.means[index] += delta / self.count as f64;
            let delta_after = row.features[index] - self.means[index];
            self.m2[index] += delta * delta_after;
        }
        Ok(())
    }

    pub fn finalize(self, train_end_ms: i64, min_rows: u64) -> Result<Normalizer, FeatureError> {
        if self.count < min_rows {
            return Err(FeatureError::InvalidSpec(format!(
                "normalizer needs at least {min_rows} train rows, got {}",
                self.count
            )));
        }
        let mut scales = Vec::with_capacity(self.feature_count);
        for index in 0..self.feature_count {
            let variance = if self.count > 1 {
                self.m2[index] / (self.count - 1) as f64
            } else {
                0.0
            };
            let std = variance.max(0.0).sqrt();
            scales.push(if std > 1e-12 { std } else { 1.0 });
        }
        Ok(Normalizer {
            feature_names: FEATURE_NAMES_V1
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            means: self.means,
            scales,
            train_rows: self.count,
            train_end_ms,
        })
    }
}

impl Normalizer {
    /// Fit from an in-memory row slice, using only training rows.
    pub fn fit(rows: &[DatasetRow], spec: &DatasetSpec) -> Result<Self, FeatureError> {
        let train_end_ms = spec.train_end_ms.ok_or_else(|| {
            FeatureError::InvalidSpec(
                "cannot fit a normalizer without spec.train_end_ms".to_string(),
            )
        })?;
        let mut stats = RunningStats::new(FEATURE_NAMES_V1.len());
        let mut pushed = 0u64;
        for row in rows {
            if row.split == Split::Train {
                stats.push_train_row(row, train_end_ms)?;
                pushed += 1;
            }
        }
        if pushed == 0 {
            return Err(FeatureError::InvalidSpec(
                "no training rows available to fit the normalizer".to_string(),
            ));
        }
        stats.finalize(train_end_ms, 2)
    }

    pub fn transform(&self, features: &[f64]) -> Result<Vec<f64>, FeatureError> {
        if features.len() != self.means.len() {
            return Err(FeatureError::CorruptOutput(format!(
                "expected {} features, got {}",
                self.means.len(),
                features.len()
            )));
        }
        let mut output = Vec::with_capacity(features.len());
        for (index, feature) in features.iter().enumerate() {
            let value = (feature - self.means[index]) / self.scales[index];
            if !value.is_finite() {
                return Err(FeatureError::CorruptOutput(format!(
                    "normalized feature {} is not finite",
                    self.feature_names[index]
                )));
            }
            output.push(value);
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::{build_rows_from_bars, DatasetSpec};
    use crate::selftest::synthetic_bars;

    fn fixture() -> (Vec<DatasetRow>, DatasetSpec) {
        let bars = synthetic_bars(300, 1_704_067_200_000, 60_000, 11);
        let spec = DatasetSpec {
            warmup_bars: 32,
            decision_every_bars: 2,
            target: crate::target::TargetSpec {
                horizon_bars: 4,
                flat_band_bps: 5.0,
            },
            train_end_ms: Some(1_704_067_200_000 + 200 * 60_000),
            ..Default::default()
        };
        let (rows, _) = build_rows_from_bars(&bars, spec.clone(), 100_000).unwrap();
        (rows, spec)
    }

    #[test]
    fn fit_uses_train_rows_only_and_guards_future_rows() {
        let (rows, spec) = fixture();
        let normalizer = Normalizer::fit(&rows, &spec).unwrap();
        let train_rows = rows.iter().filter(|row| row.split == Split::Train).count() as u64;
        assert_eq!(normalizer.train_rows, train_rows);
        assert_eq!(normalizer.train_end_ms, spec.train_end_ms.unwrap());

        let future = rows.iter().find(|row| row.split == Split::Holdout).unwrap();
        let mut stats = RunningStats::new(FEATURE_NAMES_V1.len());
        let error = stats
            .push_train_row(future, spec.train_end_ms.unwrap())
            .unwrap_err();
        assert!(matches!(error, FeatureError::Leakage(_)));

        let mut future_as_train = future.clone();
        future_as_train.split = Split::Train;
        let mut stats = RunningStats::new(FEATURE_NAMES_V1.len());
        let error = stats
            .push_train_row(&future_as_train, spec.train_end_ms.unwrap())
            .unwrap_err();
        assert!(matches!(error, FeatureError::Leakage(_)));
    }

    #[test]
    fn transform_standardizes_train_rows() {
        let (rows, spec) = fixture();
        let normalizer = Normalizer::fit(&rows, &spec).unwrap();
        for row in rows.iter().filter(|row| row.split == Split::Train).take(20) {
            let transformed = normalizer.transform(&row.features).unwrap();
            assert_eq!(transformed.len(), FEATURE_NAMES_V1.len());
            assert!(transformed.iter().all(|value| value.is_finite()));
        }
        assert!(normalizer.transform(&[0.0]).is_err());
    }

    #[test]
    fn fit_requires_split_boundary_and_min_rows() {
        let (rows, mut spec) = fixture();
        spec.train_end_ms = None;
        assert!(Normalizer::fit(&rows, &spec).is_err());
        spec.train_end_ms = Some(1_704_067_200_000 + 60 * 60_000);
        assert!(Normalizer::fit(&rows, &spec).is_err());
    }
}
