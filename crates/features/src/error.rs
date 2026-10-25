#[derive(Debug, thiserror::Error)]
pub enum FeatureError {
    #[error("leakage guard violation: {0}")]
    Leakage(String),

    #[error("invalid dataset spec: {0}")]
    InvalidSpec(String),

    #[error("insufficient history at decision {decision_time_ms}: need {needed} bars, got {available}")]
    InsufficientHistory {
        decision_time_ms: i64,
        needed: usize,
        available: usize,
    },

    #[error("label window is incomplete: {0}")]
    IncompleteLabel(String),

    #[error("non-finite feature '{feature}' at decision {decision_time_ms}: {value}")]
    NonFiniteFeature {
        feature: String,
        decision_time_ms: i64,
        value: f64,
    },

    #[error("output already exists: {0}")]
    OutputExists(String),

    #[error("output is incomplete or corrupt: {0}")]
    CorruptOutput(String),

    #[error("not implemented: {0}")]
    NotImplemented(String),

    #[error(transparent)]
    Data(#[from] mb_data::DataError),

    #[error(transparent)]
    Csv(#[from] csv::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
