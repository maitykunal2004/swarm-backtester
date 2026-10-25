use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use mb_data::{now_ms, CachedSeries, DataError, Interval, RawCache, SeriesReadReport};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::dataset::{BuildStats, DatasetRow, DatasetSpec, Split, StreamingDataset};
use crate::error::FeatureError;
use crate::features_v1::{FEATURE_NAMES_V1, FEATURE_SET_VERSION};
use crate::normalize::{Normalizer, RunningStats};

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const RAW_CSV: &str = "dataset.raw.csv";
pub const NORMALIZED_CSV: &str = "dataset.normalized.csv";
pub const MANIFEST_JSON: &str = "manifest.json";

const META_COLUMNS: [&str; 8] = [
    "schema_version",
    "symbol",
    "interval",
    "decision_time_ms",
    "decision_bar_open_time_ms",
    "entry_time_ms",
    "exit_time_ms",
    "split",
];

const LABEL_COLUMNS: [&str; 8] = [
    "direction",
    "fwd_log_return",
    "fwd_return_bps",
    "p_down",
    "p_flat",
    "p_up",
    "expected_magnitude_abs",
    "magnitude_bps",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputPolicy {
    /// Refuse to touch existing dataset outputs.
    FailIfExists,
    /// Replace outputs atomically, leaving the raw cache untouched.
    Overwrite,
}

impl OutputPolicy {
    pub fn parse(value: &str) -> Result<Self, FeatureError> {
        match value {
            "fail" => Ok(OutputPolicy::FailIfExists),
            "overwrite" => Ok(OutputPolicy::Overwrite),
            other => Err(FeatureError::InvalidSpec(format!(
                "output policy must be 'fail' or 'overwrite', got '{other}'"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceInfo {
    pub description: String,
    pub cache_root: Option<String>,
    pub pages_matched: usize,
    pub pages_read: usize,
    pub bars_emitted: u64,
    pub duplicate_bars_skipped: u64,
    pub first_bar_open_time_ms: Option<i64>,
    pub last_bar_open_time_ms: Option<i64>,
    pub first_gap_after_ms: Option<i64>,
}

impl SourceInfo {
    fn from_series(
        cache_root: Option<&Path>,
        report: &SeriesReadReport,
        description: impl Into<String>,
    ) -> Self {
        Self {
            description: description.into(),
            cache_root: cache_root.map(|path| path.to_string_lossy().to_string()),
            pages_matched: report.pages_matched,
            pages_read: report.pages_read,
            bars_emitted: report.bars_emitted,
            duplicate_bars_skipped: report.duplicate_bars_skipped,
            first_bar_open_time_ms: report.first_open_time_ms,
            last_bar_open_time_ms: report.last_open_time_ms,
            first_gap_after_ms: report.first_gap_after_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatasetManifest {
    pub schema_version: u32,
    pub created_at_ms: i64,
    pub spec: DatasetSpec,
    pub feature_set_version: String,
    pub feature_names: Vec<String>,
    pub stats: BuildStats,
    pub source: SourceInfo,
    pub normalizer: Option<Normalizer>,
    pub raw_csv: String,
    pub normalized_csv: Option<String>,
    pub raw_csv_sha256: String,
    pub normalized_csv_sha256: Option<String>,
    pub output_policy: OutputPolicy,
}

/// Range covered by the cached raw pages for a series, if any.
pub fn cached_range(
    cache: &RawCache,
    symbol: &str,
    interval: Interval,
) -> Result<Option<(i64, i64)>, FeatureError> {
    let pages = cache.list_pages(symbol, interval)?;
    let Some(first) = pages.first() else {
        return Ok(None);
    };
    let start = first.start_ms;
    let end = pages
        .iter()
        .map(|page| page.end_ms)
        .max()
        .unwrap_or(first.end_ms);
    if end <= start {
        return Ok(None);
    }
    Ok(Some((start, end)))
}

pub fn raw_header() -> Vec<String> {
    let mut header: Vec<String> = META_COLUMNS
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    header.extend(FEATURE_NAMES_V1.iter().map(|name| format!("f_{name}")));
    header.extend(LABEL_COLUMNS.iter().map(|name| (*name).to_string()));
    header
}

pub fn normalized_header() -> Vec<String> {
    let mut header: Vec<String> = META_COLUMNS
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    header.extend(FEATURE_NAMES_V1.iter().map(|name| format!("z_{name}")));
    header.extend(LABEL_COLUMNS.iter().map(|name| (*name).to_string()));
    header
}

/// Build a dataset from cached raw pages, streaming bars and rows so peak
/// memory is bounded by one cache page plus the streaming feature window.
pub fn write_dataset_from_series(
    series: &CachedSeries,
    start_ms: i64,
    end_ms: i64,
    spec: DatasetSpec,
    out_dir: &Path,
    policy: OutputPolicy,
) -> Result<DatasetManifest, FeatureError> {
    spec.validate()?;
    if start_ms >= end_ms {
        return Err(FeatureError::InvalidSpec(format!(
            "invalid dataset range [{start_ms}, {end_ms})"
        )));
    }
    fs::create_dir_all(out_dir)?;

    let raw_path = out_dir.join(RAW_CSV);
    let normalized_path = out_dir.join(NORMALIZED_CSV);
    let manifest_path = out_dir.join(MANIFEST_JSON);
    let raw_temp = temp_path(&raw_path);
    let normalized_temp = temp_path(&normalized_path);
    let manifest_temp = temp_path(&manifest_path);

    let training = spec.train_end_ms.is_some();
    let mut targets = vec![&raw_path, &manifest_path];
    if training {
        targets.push(&normalized_path);
    }
    if policy == OutputPolicy::FailIfExists {
        for target in targets {
            if target.exists() {
                return Err(FeatureError::OutputExists(
                    target.to_string_lossy().to_string(),
                ));
            }
        }
    }

    let mut stream = StreamingDataset::new(spec.clone())?;
    let mut running = RunningStats::new(FEATURE_NAMES_V1.len());
    let file = File::create(&raw_temp)?;
    let mut raw_writer = csv::WriterBuilder::new().from_writer(file);
    raw_writer.write_record(raw_header())?;
    raw_writer.flush()?;

    let mut pending: Option<FeatureError> = None;
    let train_end_ms = spec.train_end_ms;
    let read_result = series.read_range(start_ms, end_ms, &mut |bar| {
        match stream.push(bar.clone()) {
            Ok(Some(row)) => {
                if let Err(error) = write_raw_row(&mut raw_writer, &row) {
                    pending = Some(error);
                    return Err(DataError::Consumer("feature pipeline failed".to_string()));
                }
                if let (true, Some(train_end)) = (training, train_end_ms) {
                    if row.split == Split::Train {
                        if let Err(error) = running.push_train_row(&row, train_end) {
                            pending = Some(error);
                            return Err(DataError::Consumer("normalizer guard failed".to_string()));
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                pending = Some(error);
                return Err(DataError::Consumer("feature pipeline failed".to_string()));
            }
        }
        Ok(())
    });
    if let Some(error) = pending {
        let _ = fs::remove_file(&raw_temp);
        return Err(error);
    }
    let read_report = read_result?;
    raw_writer.flush()?;
    drop(raw_writer);

    let stats = stream.finish();
    if stats.rows_written != stats.train_rows + stats.holdout_rows + stats.unassigned_rows {
        let _ = fs::remove_file(&raw_temp);
        return Err(FeatureError::CorruptOutput(
            "split counts do not add up to rows written".to_string(),
        ));
    }
    if training && stats.train_rows < 2 {
        let _ = fs::remove_file(&raw_temp);
        return Err(FeatureError::InvalidSpec(format!(
            "need at least 2 training rows, got {}; move train_end_ms later or extend data",
            stats.train_rows
        )));
    }

    sync_file(&raw_temp)?;
    let raw_sha = sha256_file(&raw_temp)?;
    install(&raw_temp, &raw_path, policy)?;

    let normalizer = if training {
        let normalizer = running.finalize(train_end_ms.expect("checked"), 2)?;
        write_normalized_csv(&raw_path, &normalized_temp, &normalizer, stats.rows_written)?;
        sync_file(&normalized_temp)?;
        install(&normalized_temp, &normalized_path, policy)?;
        Some(normalizer)
    } else {
        None
    };
    let normalized_sha = if normalizer.is_some() {
        Some(sha256_file(&normalized_path)?)
    } else {
        None
    };
    let normalized_name = if normalizer.is_some() {
        Some(NORMALIZED_CSV.to_string())
    } else {
        None
    };

    let source = SourceInfo::from_series(
        Some(series.cache().root()),
        &read_report,
        format!(
            "cached Binance klines {} {}",
            series.symbol(),
            series.interval()
        ),
    );
    let manifest = DatasetManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        created_at_ms: now_ms(),
        spec,
        feature_set_version: FEATURE_SET_VERSION.to_string(),
        feature_names: FEATURE_NAMES_V1
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
        stats,
        source,
        normalizer,
        raw_csv: RAW_CSV.to_string(),
        normalized_csv: normalized_name,
        raw_csv_sha256: raw_sha,
        normalized_csv_sha256: normalized_sha,
        output_policy: policy,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let mut manifest_file = File::create(&manifest_temp)?;
    manifest_file.write_all(&manifest_bytes)?;
    manifest_file.sync_all()?;
    drop(manifest_file);
    install(&manifest_temp, &manifest_path, policy)?;
    Ok(manifest)
}

fn write_raw_row(writer: &mut csv::Writer<File>, row: &DatasetRow) -> Result<(), FeatureError> {
    let mut record = csv::StringRecord::new();
    record.push_field(&row.schema_version.to_string());
    record.push_field(&row.symbol);
    record.push_field(&row.interval);
    record.push_field(&row.decision_time_ms.to_string());
    record.push_field(&row.decision_bar_open_time_ms.to_string());
    record.push_field(&row.entry_time_ms.to_string());
    record.push_field(&row.exit_time_ms.to_string());
    record.push_field(row.split.as_str());
    for feature in &row.features {
        record.push_field(&feature.to_string());
    }
    record.push_field(row.direction.as_str());
    record.push_field(&row.fwd_log_return.to_string());
    record.push_field(&row.fwd_return_bps.to_string());
    record.push_field(&row.p_down.to_string());
    record.push_field(&row.p_flat.to_string());
    record.push_field(&row.p_up.to_string());
    record.push_field(&row.expected_magnitude_abs.to_string());
    record.push_field(&row.magnitude_bps.to_string());
    writer.write_record(&record)?;
    Ok(())
}

fn write_normalized_csv(
    raw_path: &Path,
    normalized_temp: &Path,
    normalizer: &Normalizer,
    expected_rows: u64,
) -> Result<(), FeatureError> {
    let mut reader = csv::ReaderBuilder::new().from_path(raw_path)?;
    let headers = reader.headers()?.clone();
    let feature_flags: Vec<bool> = headers
        .iter()
        .map(|header| header.starts_with("f_"))
        .collect();
    if feature_flags.iter().filter(|flag| **flag).count() != FEATURE_NAMES_V1.len() {
        return Err(FeatureError::CorruptOutput(
            "raw CSV has an unexpected feature column count".to_string(),
        ));
    }
    let normalized_headers = normalized_header();
    if normalized_headers.len() != headers.len() {
        return Err(FeatureError::CorruptOutput(
            "normalized header length mismatch".to_string(),
        ));
    }
    let file = File::create(normalized_temp)?;
    let mut writer = csv::WriterBuilder::new().from_writer(file);
    writer.write_record(&normalized_headers)?;
    let mut written = 0u64;
    for record in reader.records() {
        let record = record?;
        let mut features = Vec::with_capacity(FEATURE_NAMES_V1.len());
        for (index, flag) in feature_flags.iter().enumerate() {
            if *flag {
                let text = record.get(index).ok_or_else(|| {
                    FeatureError::CorruptOutput("missing feature field".to_string())
                })?;
                let value = text.parse::<f64>().map_err(|err| {
                    FeatureError::CorruptOutput(format!("feature '{text}' is not numeric: {err}"))
                })?;
                features.push(value);
            }
        }
        let normalized = normalizer.transform(&features)?;
        let mut output = csv::StringRecord::new();
        let mut feature_cursor = 0usize;
        for (index, field) in record.iter().enumerate() {
            if feature_flags[index] {
                output.push_field(&normalized[feature_cursor].to_string());
                feature_cursor += 1;
            } else {
                output.push_field(field);
            }
        }
        writer.write_record(&output)?;
        written += 1;
    }
    writer.flush()?;
    if written != expected_rows {
        return Err(FeatureError::CorruptOutput(format!(
            "normalized {written} rows, expected {expected_rows}"
        )));
    }
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".to_string());
    path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()))
}

fn sync_file(path: &Path) -> Result<(), FeatureError> {
    let file = File::open(path)?;
    file.sync_all()?;
    Ok(())
}

fn install(temp: &Path, target: &Path, policy: OutputPolicy) -> Result<(), FeatureError> {
    if target.exists() {
        match policy {
            OutputPolicy::FailIfExists => {
                let _ = fs::remove_file(temp);
                return Err(FeatureError::OutputExists(
                    target.to_string_lossy().to_string(),
                ));
            }
            OutputPolicy::Overwrite => fs::remove_file(target)?,
        }
    }
    fs::rename(temp, target)?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, FeatureError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
