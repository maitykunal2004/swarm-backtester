use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use clap::{Parser, Subcommand};

use mb_data::{
    CachedSeries, FetchConfig, FetchRequest, Interval, KlinesFetcher, RateLimitPolicy, RawCache,
};
use mb_features::{
    cached_range, run_leakage_selftest, write_dataset_from_series, DatasetSpec, OutputPolicy,
    TargetSpec,
};
use mb_tracker::{ExperimentStore, NewRun};

#[derive(Parser, Debug)]
#[command(
    name = "mb",
    version,
    about = "BTCUSDT market-prediction research harness (keyless Binance data, no models yet)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Fetch public Binance klines into the append-only raw cache.
    Fetch(FetchArgs),
    /// Build a point-in-time dataset (features + fixed-horizon labels).
    BuildDataset(BuildDatasetArgs),
    /// Inspect cached raw pages and their timestamp coverage.
    CacheInfo(CacheInfoArgs),
    /// Run the offline leakage self-test.
    Selftest,
    /// Show which layers are implemented and which are stubs.
    Status,
}

#[derive(clap::Args, Debug)]
struct FetchArgs {
    #[arg(long, default_value = "BTCUSDT")]
    symbol: String,
    #[arg(long, default_value = "1m")]
    interval: String,
    /// RFC3339, `YYYY-MM-DD`, or epoch milliseconds.
    #[arg(long)]
    start: String,
    /// Defaults to now.
    #[arg(long)]
    end: Option<String>,
    #[arg(long, default_value = "data/cache")]
    cache_dir: PathBuf,
    #[arg(long, default_value_t = 1000)]
    limit: u16,
    #[arg(long)]
    refresh: bool,
    #[arg(long, default_value_t = 1200)]
    weight_budget_per_minute: u32,
    #[arg(long, default_value_t = 200_000)]
    max_pages: usize,
    #[arg(long, default_value_t = 5)]
    max_retries: u32,
    /// Keyless Binance market-data base URL.
    #[arg(long, default_value = mb_data::BINANCE_DATA_API_BASE_URL)]
    base_url: String,
}

#[derive(clap::Args, Debug)]
struct BuildDatasetArgs {
    #[arg(long, default_value = "data/cache")]
    cache_dir: PathBuf,
    #[arg(long, default_value = "BTCUSDT")]
    symbol: String,
    #[arg(long, default_value = "1m")]
    interval: String,
    /// Defaults to the first cached bar.
    #[arg(long)]
    start: Option<String>,
    /// Defaults to the end of the cached range.
    #[arg(long)]
    end: Option<String>,
    /// Defaults to a timestamped directory under data/datasets.
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long, default_value_t = 1)]
    decision_every_bars: u32,
    #[arg(long, default_value_t = 12)]
    horizon_bars: usize,
    #[arg(long, default_value_t = 10.0)]
    flat_band_bps: f64,
    #[arg(long, default_value_t = 64)]
    warmup_bars: usize,
    /// Decision timestamps before this boundary form the train split.
    #[arg(long)]
    train_end: Option<String>,
    /// `fail` (default) or `overwrite` for dataset outputs.
    #[arg(long, default_value = "fail")]
    output_policy: String,
    /// Optional experiment tracker root; records a build run.
    #[arg(long)]
    tracker_dir: Option<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct CacheInfoArgs {
    #[arg(long, default_value = "data/cache")]
    cache_dir: PathBuf,
    #[arg(long, default_value = "BTCUSDT")]
    symbol: String,
    #[arg(long, default_value = "1m")]
    interval: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Fetch(args) => run_fetch(args),
        Command::BuildDataset(args) => run_build_dataset(args),
        Command::CacheInfo(args) => run_cache_info(args),
        Command::Selftest => run_selftest(),
        Command::Status => run_status(),
    }
}

fn run_fetch(args: FetchArgs) -> Result<()> {
    let interval: Interval = args.interval.parse()?;
    let start_ms = parse_time(&args.start)?;
    let end_ms = match args.end.as_deref() {
        Some(value) => parse_time(value)?,
        None => Utc::now().timestamp_millis(),
    };
    anyhow::ensure!(start_ms < end_ms, "--start must be before --end");
    anyhow::ensure!((1..=1000).contains(&args.limit), "--limit must be 1..=1000");

    let cache = RawCache::new(&args.cache_dir);
    let config = FetchConfig {
        base_url: args.base_url,
        max_retries: args.max_retries,
        max_pages: args.max_pages,
        rate_limit: RateLimitPolicy {
            weight_budget_per_minute: args.weight_budget_per_minute,
            safety_margin: 60,
        },
        ..Default::default()
    };
    let mut fetcher = KlinesFetcher::new(cache, config);
    let mut request = FetchRequest::new(&args.symbol, interval, start_ms, end_ms);
    request.limit = args.limit;
    request.refresh = args.refresh;

    let mut bars = 0u64;
    let report = fetcher.fetch(&request, &mut |page| {
        bars += page.len() as u64;
        Ok(())
    })?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !report.complete {
        eprintln!(
            "warning: range not fully covered; next unfetched timestamp is {} ({})",
            report.next_unfetched_ms,
            report
                .stop_reason
                .map(|reason| format!("{reason:?}"))
                .unwrap_or_else(|| "unknown".to_string())
        );
    }
    eprintln!(
        "fetched {} bars into {} (network pages: {}, cache hits: {}, stored: {})",
        bars,
        args.cache_dir.display(),
        report.pages_network,
        report.pages_cache,
        report.pages_stored
    );
    Ok(())
}

fn run_build_dataset(args: BuildDatasetArgs) -> Result<()> {
    let interval: Interval = args.interval.parse()?;
    let cache = RawCache::new(&args.cache_dir);
    let cached = cached_range(&cache, &args.symbol, interval)?
        .with_context(|| format!("no cached pages for {} {}", args.symbol, args.interval))?;
    let start_ms = match args.start.as_deref() {
        Some(value) => parse_time(value)?,
        None => cached.0,
    };
    let end_ms = match args.end.as_deref() {
        Some(value) => parse_time(value)?,
        None => cached.1,
    };
    anyhow::ensure!(start_ms < end_ms, "dataset range is empty");

    let spec = DatasetSpec {
        symbol: args.symbol.to_ascii_uppercase(),
        interval,
        decision_every_bars: args.decision_every_bars,
        warmup_bars: args.warmup_bars,
        target: TargetSpec {
            horizon_bars: args.horizon_bars,
            flat_band_bps: args.flat_band_bps,
        },
        train_end_ms: args.train_end.as_deref().map(parse_time).transpose()?,
    };
    spec.validate()?;
    let policy = OutputPolicy::parse(&args.output_policy)?;
    let out_dir = args.out.unwrap_or_else(|| {
        PathBuf::from("data/datasets").join(format!(
            "{}-{}-d{}-h{}-{}",
            spec.symbol,
            interval,
            spec.decision_every_bars,
            spec.target.horizon_bars,
            Utc::now().format("%Y%m%dT%H%M%SZ")
        ))
    });

    let series = CachedSeries::new(cache, &spec.symbol, interval);
    let manifest =
        write_dataset_from_series(&series, start_ms, end_ms, spec.clone(), &out_dir, policy)?;

    let mut tracker_run = None;
    if let Some(tracker_dir) = &args.tracker_dir {
        let store = ExperimentStore::new(tracker_dir);
        let mut request = NewRun::new(
            "build-dataset",
            serde_json::to_value(&spec).context("serialize spec")?,
        );
        request.dataset_fingerprint = Some(manifest.raw_csv_sha256.clone());
        request.notes = format!("dataset at {}", out_dir.display());
        let run = store.create_run(request)?;
        run.log_event("dataset_built", &out_dir.to_string_lossy())?;
        let summary = serde_json::json!({
            "rows": manifest.stats.rows_written,
            "train_rows": manifest.stats.train_rows,
            "holdout_rows": manifest.stats.holdout_rows,
            "raw_csv_sha256": manifest.raw_csv_sha256,
        });
        run.finish(&summary)?;
        tracker_run = Some(run.run_id().to_string());
    }

    let output = serde_json::json!({
        "out_dir": out_dir.to_string_lossy(),
        "rows": manifest.stats.rows_written,
        "train_rows": manifest.stats.train_rows,
        "holdout_rows": manifest.stats.holdout_rows,
        "dropped_incomplete_horizon": manifest.stats.dropped_incomplete_horizon,
        "dropped_gaps": manifest.stats.dropped_gaps,
        "feature_set_version": manifest.feature_set_version,
        "normalizer_train_rows": manifest.normalizer.as_ref().map(|normalizer| normalizer.train_rows),
        "raw_csv_sha256": manifest.raw_csv_sha256,
        "tracker_run": tracker_run,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn run_cache_info(args: CacheInfoArgs) -> Result<()> {
    let interval: Interval = args.interval.parse()?;
    let cache = RawCache::new(&args.cache_dir);
    let symbol = args.symbol.to_ascii_uppercase();
    let pages = cache.list_pages(&symbol, interval)?;
    let first = pages.first();
    let last = pages.last();
    let summary = serde_json::json!({
        "cache_dir": args.cache_dir.to_string_lossy(),
        "symbol": symbol,
        "interval": interval.as_str(),
        "pages": pages.len(),
        "raw_files": cache.count_files(&symbol, interval)?,
        "start_ms": first.map(|page| page.start_ms),
        "end_ms": last.map(|page| page.end_ms),
        "first_page": first.map(|page| page.path.to_string_lossy().to_string()),
        "last_page": last.map(|page| page.path.to_string_lossy().to_string()),
    });
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

fn run_selftest() -> Result<()> {
    let report = run_leakage_selftest()?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !report.passed {
        eprintln!("leakage self-test FAILED");
        std::process::exit(1);
    }
    eprintln!("leakage self-test passed ({} checks)", report.checks.len());
    Ok(())
}

fn run_status() -> Result<()> {
    let status = serde_json::json!({
        "data_layer": "implemented: keyless klines fetch, pagination, rate limiting, append-only raw cache",
        "feature_label_layer": "implemented: point-in-time features, fixed-horizon one-hot direction targets + magnitude, train-only normalization, leakage self-test",
        "models": "stubbed: mb_models::StubForecaster returns NotImplemented",
        "backtest": "stubbed: mb_backtest::Simulator returns NotImplemented",
        "tracker": "implemented: append-only run.json/config.json, metrics.jsonl, events.jsonl, summary.json",
        "features_v1": mb_features::FEATURE_NAMES_V1,
        "target": "direction probabilities {down,flat,up} + expected |log return| over fixed horizon",
    });
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

/// Parse RFC3339, `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM:SS`, or epoch milliseconds.
fn parse_time(value: &str) -> Result<i64> {
    let trimmed = value.trim();
    if let Ok(milliseconds) = trimmed.parse::<i64>() {
        return Ok(milliseconds);
    }
    if let Ok(datetime) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(datetime.timestamp_millis());
    }
    for format in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(trimmed, format) {
            return Ok(naive.and_utc().timestamp_millis());
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let naive = date
            .and_hms_opt(0, 0, 0)
            .context("midnight is always valid")?;
        return Ok(naive.and_utc().timestamp_millis());
    }
    anyhow::bail!("cannot parse time '{value}'; use RFC3339, YYYY-MM-DD, or epoch milliseconds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_documented_time_formats() {
        assert_eq!(parse_time("1704067200000").unwrap(), 1_704_067_200_000);
        assert_eq!(parse_time("2024-01-01").unwrap(), 1_704_067_200_000);
        assert_eq!(
            parse_time("2024-01-01T00:00:00Z").unwrap(),
            1_704_067_200_000
        );
        assert_eq!(
            parse_time("2024-01-01T00:00:00").unwrap(),
            1_704_067_200_000
        );
        assert!(parse_time("not-a-date").is_err());
    }

    #[test]
    fn parses_fetch_subcommand() {
        let cli = Cli::try_parse_from([
            "mb",
            "fetch",
            "--symbol",
            "BTCUSDT",
            "--interval",
            "1m",
            "--start",
            "2024-01-01",
            "--end",
            "2024-01-02",
        ])
        .unwrap();
        match cli.command {
            Command::Fetch(args) => {
                assert_eq!(args.symbol, "BTCUSDT");
                assert_eq!(args.limit, 1000);
            }
            _ => panic!("expected fetch"),
        }
    }
}
