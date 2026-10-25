use std::path::Path;

use mb_data::{Bar, CachedSeries, Interval, PageRequest, RawCache, StoreContext};
use mb_features::{
    normalized_header, raw_header, run_leakage_selftest, synthetic_bars, write_dataset_from_series,
    DatasetManifest, DatasetSpec, OutputPolicy, TargetSpec,
};

const START: i64 = 1_704_067_200_000;
const INTERVAL_MS: i64 = 60_000;

pub fn klines_json(bars: &[Bar]) -> String {
    let rows: Vec<String> = bars
        .iter()
        .map(|bar| {
            format!(
                r#"[{},"{}","{}","{}","{}","{}",{},"{}",{},"{}","{}","0"]"#,
                bar.open_time_ms,
                bar.open,
                bar.high,
                bar.low,
                bar.close,
                bar.volume,
                bar.close_time_ms,
                bar.quote_volume,
                bar.trades,
                bar.taker_buy_base_volume,
                bar.taker_buy_quote_volume
            )
        })
        .collect();
    format!("[{}]", rows.join(","))
}

pub fn seed_cache(bars: &[Bar], page_size: usize) -> RawCache {
    let dir = tempfile::tempdir().unwrap();
    let cache = RawCache::new(dir.keep());
    for (page_number, chunk) in bars.chunks(page_size).enumerate() {
        let start = chunk[0].open_time_ms;
        let end = start + chunk.len() as i64 * INTERVAL_MS;
        let request = PageRequest::new("BTCUSDT", Interval::M1, start, chunk.len() as u16, end);
        cache
            .store_page(
                &request,
                klines_json(chunk).as_bytes(),
                &StoreContext {
                    fetched_at_ms: 1_700_000_000_000 + page_number as i64,
                    http_status: 200,
                    used_weight_1m: Some(5),
                    retry_after_seconds: None,
                    source_url: "test".to_string(),
                    record_count: chunk.len(),
                    first_open_time_ms: Some(chunk[0].open_time_ms),
                    last_open_time_ms: Some(chunk.last().unwrap().open_time_ms),
                },
            )
            .unwrap();
    }
    cache
}

pub fn spec(train_end_ms: Option<i64>) -> DatasetSpec {
    DatasetSpec {
        symbol: "BTCUSDT".to_string(),
        interval: Interval::M1,
        decision_every_bars: 2,
        warmup_bars: 32,
        target: TargetSpec {
            horizon_bars: 4,
            flat_band_bps: 5.0,
        },
        train_end_ms,
    }
}

pub fn read_manifest(dir: &Path) -> DatasetManifest {
    serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap()
}

#[test]
fn writes_raw_and_train_only_normalized_dataset() {
    let bars = synthetic_bars(300, START, INTERVAL_MS, 42);
    let cache = seed_cache(&bars, 100);
    let series = CachedSeries::new(cache, "BTCUSDT", Interval::M1);
    let end = START + 300 * INTERVAL_MS;
    let train_end = START + 200 * INTERVAL_MS;
    let out = tempfile::tempdir().unwrap();
    let manifest = write_dataset_from_series(
        &series,
        START,
        end,
        spec(Some(train_end)),
        out.path(),
        OutputPolicy::FailIfExists,
    )
    .unwrap();

    assert!(out.path().join("dataset.raw.csv").exists());
    assert!(out.path().join("dataset.normalized.csv").exists());
    assert!(out.path().join("manifest.json").exists());
    assert!(manifest.stats.rows_written > 100);
    assert!(manifest.stats.train_rows > 2);
    assert!(manifest.stats.holdout_rows > 0);
    assert_eq!(
        manifest.stats.rows_written,
        manifest.stats.train_rows + manifest.stats.holdout_rows
    );
    assert!(manifest.stats.dropped_incomplete_horizon > 0);
    assert_eq!(manifest.feature_names.len(), 10);
    let normalizer = manifest.normalizer.as_ref().unwrap();
    assert_eq!(normalizer.train_rows, manifest.stats.train_rows);
    assert_eq!(normalizer.train_end_ms, train_end);
    assert_eq!(normalizer.feature_names, manifest.feature_names);

    let mut reader = csv::Reader::from_path(out.path().join("dataset.raw.csv")).unwrap();
    assert_eq!(
        reader.headers().unwrap().iter().collect::<Vec<_>>(),
        raw_header().iter().map(String::as_str).collect::<Vec<_>>()
    );
    let mut rows = 0usize;
    for record in reader.records() {
        let record = record.unwrap();
        assert_eq!(record.len(), raw_header().len());
        let decision: i64 = record[3].parse().unwrap();
        let entry: i64 = record[5].parse().unwrap();
        let exit: i64 = record[6].parse().unwrap();
        assert!(entry > decision && exit > entry);
        assert_eq!(exit, decision + 4 * INTERVAL_MS);
        assert!(&record[7] == "train" || &record[7] == "holdout");
        let p_down: f64 = record[21].parse().unwrap();
        let p_flat: f64 = record[22].parse().unwrap();
        let p_up: f64 = record[23].parse().unwrap();
        assert!((p_down + p_flat + p_up - 1.0).abs() < 1e-12);
        rows += 1;
    }
    assert_eq!(rows as u64, manifest.stats.rows_written);

    let mut reader = csv::Reader::from_path(out.path().join("dataset.normalized.csv")).unwrap();
    assert_eq!(
        reader.headers().unwrap().iter().collect::<Vec<_>>(),
        normalized_header()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    let feature_start = 8usize;
    let feature_count = manifest.feature_names.len();
    let mut sums = vec![0.0; feature_count];
    let mut count = 0.0;
    for record in reader.records() {
        let record = record.unwrap();
        if &record[7] != "train" {
            continue;
        }
        for index in 0..feature_count {
            let value: f64 = record[feature_start + index].parse().unwrap();
            sums[index] += value;
        }
        count += 1.0;
    }
    assert_eq!(count as u64, manifest.stats.train_rows);
    for sum in sums {
        assert!((sum / count).abs() < 1e-9, "train feature mean is not zero");
    }

    let raw_bytes = std::fs::read(out.path().join("dataset.raw.csv")).unwrap();
    assert_eq!(mb_data::sha256_hex(&raw_bytes), manifest.raw_csv_sha256);
    let normalized_bytes = std::fs::read(out.path().join("dataset.normalized.csv")).unwrap();
    assert_eq!(
        mb_data::sha256_hex(&normalized_bytes),
        manifest.normalized_csv_sha256.unwrap()
    );
}

#[test]
fn output_policy_prevents_silent_dataset_overwrites() {
    let bars = synthetic_bars(160, START, INTERVAL_MS, 7);
    let cache = seed_cache(&bars, 80);
    let series = CachedSeries::new(cache, "BTCUSDT", Interval::M1);
    let end = START + 160 * INTERVAL_MS;
    let out = tempfile::tempdir().unwrap();
    let manifest = write_dataset_from_series(
        &series,
        START,
        end,
        spec(None),
        out.path(),
        OutputPolicy::FailIfExists,
    )
    .unwrap();
    assert!(manifest.normalizer.is_none());
    assert!(manifest.normalized_csv.is_none());
    assert!(!out.path().join("dataset.normalized.csv").exists());
    assert!(manifest.stats.unassigned_rows == manifest.stats.rows_written);

    let raw_before = std::fs::read(out.path().join("dataset.raw.csv")).unwrap();
    let error = write_dataset_from_series(
        &series,
        START,
        end,
        spec(None),
        out.path(),
        OutputPolicy::FailIfExists,
    )
    .unwrap_err();
    assert!(matches!(error, mb_features::FeatureError::OutputExists(_)));
    assert_eq!(
        std::fs::read(out.path().join("dataset.raw.csv")).unwrap(),
        raw_before
    );

    let overwritten = write_dataset_from_series(
        &series,
        START,
        end,
        spec(None),
        out.path(),
        OutputPolicy::Overwrite,
    )
    .unwrap();
    assert_eq!(overwritten.stats.rows_written, manifest.stats.rows_written);
}

#[test]
fn cached_range_and_manifest_round_trip() {
    let bars = synthetic_bars(120, START, INTERVAL_MS, 3);
    let cache = seed_cache(&bars, 60);
    let range = mb_features::cached_range(&cache, "BTCUSDT", Interval::M1)
        .unwrap()
        .unwrap();
    assert_eq!(range, (START, START + 120 * INTERVAL_MS));
    let series = CachedSeries::new(cache, "BTCUSDT", Interval::M1);
    let out = tempfile::tempdir().unwrap();
    write_dataset_from_series(
        &series,
        range.0,
        range.1,
        spec(Some(START + 90 * INTERVAL_MS)),
        out.path(),
        OutputPolicy::FailIfExists,
    )
    .unwrap();
    let manifest = read_manifest(out.path());
    assert_eq!(
        manifest.schema_version,
        mb_features::writer::MANIFEST_SCHEMA_VERSION
    );
    assert_eq!(manifest.source.pages_matched, 2);
    assert_eq!(manifest.source.bars_emitted, 120);
    assert_eq!(manifest.spec.train_end_ms, Some(START + 90 * INTERVAL_MS));
    assert!(manifest.normalizer.is_some());
}

#[test]
fn leakage_selftest_is_green() {
    let report = run_leakage_selftest().unwrap();
    let failures = report.failures();
    assert!(report.passed, "failed checks: {:?}", failures);
    assert!(report.checks.len() >= 12);
}
