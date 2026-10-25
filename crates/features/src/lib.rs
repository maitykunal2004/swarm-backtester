//! Point-in-time feature engineering, fixed-horizon labels and leakage guards.

pub mod dataset;
pub mod error;
pub mod features_v1;
pub mod normalize;
pub mod pit;
pub mod selftest;
pub mod target;
pub mod writer;

pub use dataset::{
    build_rows_from_bars, BuildStats, DatasetRow, DatasetSpec, Split, StreamingDataset,
    DATASET_SCHEMA_VERSION,
};
pub use error::FeatureError;
pub use features_v1::{
    compute_features_v1, FEATURE_NAMES_V1, FEATURE_SET_VERSION, MIN_HISTORY_BARS_V1,
};
pub use normalize::{Normalizer, RunningStats};
pub use pit::PointInTime;
pub use selftest::{run_leakage_selftest, synthetic_bars, LeakageSelfTestReport, SelfTestCheck};
pub use target::{compute_label, Direction, TargetLabel, TargetSpec, TARGET_SCHEMA_VERSION};
pub use writer::{
    cached_range, normalized_header, raw_header, write_dataset_from_series, DatasetManifest,
    OutputPolicy, SourceInfo, MANIFEST_JSON, NORMALIZED_CSV, RAW_CSV,
};
