//! Model layer placeholder for the next pass.
//!
//! This pass establishes interfaces and target semantics only. No model is
//! trained or evaluated; [`StubForecaster`] exists so downstream wiring can be
//! compiled and tested without pretending a model exists.

use serde::{Deserialize, Serialize};

pub const MODEL_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("model is not implemented in the data-layer pass: {0}")]
    NotImplemented(String),

    #[error("invalid feature vector: expected {expected}, got {actual}")]
    FeatureLength { expected: usize, actual: usize },

    #[error("invalid forecast distribution: down={p_down} flat={p_flat} up={p_up}")]
    InvalidDistribution { p_down: f64, p_flat: f64, p_up: f64 },
}

/// Prediction for one decision timestamp: categorical return probabilities plus
/// expected absolute magnitude, matching `mb_features::TargetSpec`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ProbabilityForecast {
    pub p_down: f64,
    pub p_flat: f64,
    pub p_up: f64,
    /// Expected `|log return|` over the fixed horizon.
    pub expected_magnitude_abs: f64,
}

impl ProbabilityForecast {
    pub fn validate(&self) -> Result<(), ModelError> {
        for value in [self.p_down, self.p_flat, self.p_up] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(ModelError::InvalidDistribution {
                    p_down: self.p_down,
                    p_flat: self.p_flat,
                    p_up: self.p_up,
                });
            }
        }
        if (self.p_down + self.p_flat + self.p_up - 1.0).abs() > 1e-6 {
            return Err(ModelError::InvalidDistribution {
                p_down: self.p_down,
                p_flat: self.p_flat,
                p_up: self.p_up,
            });
        }
        if !self.expected_magnitude_abs.is_finite() || self.expected_magnitude_abs < 0.0 {
            return Err(ModelError::InvalidDistribution {
                p_down: self.p_down,
                p_flat: self.p_flat,
                p_up: self.p_up,
            });
        }
        Ok(())
    }

    pub fn argmax(&self) -> mb_features::Direction {
        if self.p_up >= self.p_down && self.p_up >= self.p_flat {
            mb_features::Direction::Up
        } else if self.p_down >= self.p_flat {
            mb_features::Direction::Down
        } else {
            mb_features::Direction::Flat
        }
    }
}

/// Contract every future model must satisfy.
pub trait Forecaster: Send + Sync {
    fn name(&self) -> &str;

    fn forecast(&self, features: &[f64]) -> Result<ProbabilityForecast, ModelError>;
}

/// Deliberate stub: proves the wiring compiles without a model existing.
#[derive(Debug, Default, Clone, Copy)]
pub struct StubForecaster;

impl Forecaster for StubForecaster {
    fn name(&self) -> &str {
        "stub-forecaster"
    }

    fn forecast(&self, features: &[f64]) -> Result<ProbabilityForecast, ModelError> {
        if features.len() != mb_features::FEATURE_NAMES_V1.len() {
            return Err(ModelError::FeatureLength {
                expected: mb_features::FEATURE_NAMES_V1.len(),
                actual: features.len(),
            });
        }
        Err(ModelError::NotImplemented(
            "model training is out of scope for the data-layer pass".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_forecaster_fails_loudly() {
        let features = vec![0.0; mb_features::FEATURE_NAMES_V1.len()];
        let error = StubForecaster.forecast(&features).unwrap_err();
        assert!(matches!(error, ModelError::NotImplemented(_)));
    }

    #[test]
    fn validates_forecast_distribution() {
        let forecast = ProbabilityForecast {
            p_down: 0.2,
            p_flat: 0.3,
            p_up: 0.5,
            expected_magnitude_abs: 0.001,
        };
        forecast.validate().unwrap();
        assert_eq!(forecast.argmax(), mb_features::Direction::Up);

        let invalid = ProbabilityForecast {
            p_down: 0.5,
            p_flat: 0.5,
            p_up: 0.5,
            expected_magnitude_abs: 0.001,
        };
        assert!(invalid.validate().is_err());
    }
}
