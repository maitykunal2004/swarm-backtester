//! Backtest / simulator placeholder for the next pass.
//!
//! Cost and execution configuration is defined now so later strategy work has
//! a stable contract. [`Simulator::run`] intentionally returns
//! [`BacktestError::NotImplemented`].

use serde::{Deserialize, Serialize};

pub const BACKTEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum BacktestError {
    #[error("simulator is not implemented in the data-layer pass: {0}")]
    NotImplemented(String),

    #[error("invalid simulation config: {0}")]
    InvalidConfig(String),
}

/// Explicit frictions: no simulated PnL is credible without them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostModel {
    pub fee_bps_per_side: f64,
    pub slippage_bps_per_side: f64,
}

impl Default for CostModel {
    fn default() -> Self {
        Self {
            fee_bps_per_side: 4.0,
            slippage_bps_per_side: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SimulationConfig {
    pub initial_capital: f64,
    pub cost_model: CostModel,
    /// Fraction of equity risked per decision, before costs.
    pub risk_fraction: f64,
}

impl Default for SimulationConfig {
    fn default() -> Self {
        Self {
            initial_capital: 10_000.0,
            cost_model: CostModel::default(),
            risk_fraction: 0.05,
        }
    }
}

impl SimulationConfig {
    pub fn validate(&self) -> Result<(), BacktestError> {
        if !self.initial_capital.is_finite() || self.initial_capital <= 0.0 {
            return Err(BacktestError::InvalidConfig(
                "initial_capital must be finite and positive".to_string(),
            ));
        }
        if !(0.0..=1.0).contains(&self.risk_fraction) {
            return Err(BacktestError::InvalidConfig(
                "risk_fraction must be in [0, 1]".to_string(),
            ));
        }
        for (name, value) in [
            ("fee_bps_per_side", self.cost_model.fee_bps_per_side),
            (
                "slippage_bps_per_side",
                self.cost_model.slippage_bps_per_side,
            ),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(BacktestError::InvalidConfig(format!(
                    "{name} must be finite and non-negative"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SimulationReport {
    pub decisions: u64,
    pub trades: u64,
    pub ending_equity: f64,
    pub max_drawdown: f64,
}

/// Stable entry point for the future simulator.
#[derive(Debug, Default, Clone, Copy)]
pub struct Simulator;

impl Simulator {
    pub fn run(&self, _config: &SimulationConfig) -> Result<SimulationReport, BacktestError> {
        Err(BacktestError::NotImplemented(
            "execution simulation is out of scope for the data-layer pass".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulator_is_explicitly_stubbed() {
        let config = SimulationConfig::default();
        config.validate().unwrap();
        let error = Simulator.run(&config).unwrap_err();
        assert!(matches!(error, BacktestError::NotImplemented(_)));
    }

    #[test]
    fn rejects_bad_config() {
        let config = SimulationConfig {
            initial_capital: 0.0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }
}
