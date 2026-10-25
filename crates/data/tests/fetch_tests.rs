use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mb_data::{
    CachedSeries, FetchConfig, FetchReport, FetchRequest, HttpResponse, HttpTransport, Interval,
    KlinesFetcher, RateLimitPolicy, RawCache, Sleeper,
};

#[derive(Default)]
struct MockTransport {
    responses: Mutex<VecDeque<Result<HttpResponse, String>>>,
    queries: Mutex<Vec<Vec<(String, String)>>>,
}

impl MockTransport {
    fn new(responses: Vec<HttpResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into_iter().map(Ok).collect()),
            queries: Mutex::new(Vec::new()),
        })
    }

    fn call_count(&self) -> usize {
        self.queries.lock().unwrap().len()
    }
}

impl HttpTransport for MockTransport {
    fn get(
        &self,
        _url: &str,
        query: &[(&str, String)],
        _headers: &[(&str, String)],
    ) -> Result<HttpResponse, String> {
        self.queries.lock().unwrap().push(
            query
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect(),
        );
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err("mock exhausted".to_string()))
    }
}

#[derive(Default)]
struct MockSleeper {
    sleeps: Mutex<Vec<Duration>>,
}

impl Sleeper for MockSleeper {
    fn sleep(&self, duration: Duration) {
        self.sleeps.lock().unwrap().push(duration);
    }
}

fn response(status: u16, body: String, headers: &[(&str, &str)]) -> HttpResponse {
    HttpResponse {
        status,
        body: body.into_bytes(),
        headers: headers
            .iter()
            .map(|(key, value)| (key.to_ascii_lowercase(), value.to_string()))
            .collect::<BTreeMap<_, _>>(),
    }
}

fn klines(open_times: &[i64], price: f64) -> String {
    let rows: Vec<String> = open_times
        .iter()
        .map(|open| {
            format!(
                r#"[{open},"{price}","{}","{}","{price}","10.0",{},"1000.0",5,"4.0","400.0","0"]"#,
                price + 10.0,
                price - 10.0,
                open + 59_999
            )
        })
        .collect();
    format!("[{}]", rows.join(","))
}

fn config() -> FetchConfig {
    FetchConfig {
        max_retries: 2,
        max_pages: 100,
        rate_limit: RateLimitPolicy {
            weight_budget_per_minute: 10_000,
            safety_margin: 0,
        },
        ..Default::default()
    }
}

fn fetch_all_with_limit(
    fetcher: &mut KlinesFetcher,
    start: i64,
    end: i64,
    limit: u16,
) -> (Vec<mb_data::Bar>, FetchReport) {
    let mut request = FetchRequest::new("BTCUSDT", Interval::M1, start, end);
    request.limit = limit;
    let mut collected = Vec::new();
    let report = fetcher
        .fetch(&request, &mut |bars| {
            collected.extend_from_slice(bars);
            Ok(())
        })
        .unwrap();
    (collected, report)
}

fn fetch_all(
    fetcher: &mut KlinesFetcher,
    start: i64,
    end: i64,
) -> (Vec<mb_data::Bar>, FetchReport) {
    fetch_all_with_limit(fetcher, start, end, 1000)
}

const START: i64 = 1_704_067_200_000;

#[test]
fn paginates_persists_and_reuses_cache() {
    let dir = tempfile::tempdir().unwrap();
    let cache = RawCache::new(dir.path());
    let end = START + 5 * 60_000;
    let transport = MockTransport::new(vec![
        response(
            200,
            klines(&[START, START + 60_000, START + 120_000], 42_000.0),
            &[("X-MBX-USED-WEIGHT-1M", "7")],
        ),
        response(
            200,
            klines(&[START + 180_000, START + 240_000], 42_000.0),
            &[("X-MBX-USED-WEIGHT-1M", "12")],
        ),
    ]);
    let sleeper = Arc::new(MockSleeper::default());
    let mut fetcher =
        KlinesFetcher::with_components(cache.clone(), config(), transport.clone(), sleeper);

    let (bars, report) = fetch_all_with_limit(&mut fetcher, START, end, 3);
    assert_eq!(bars.len(), 5);
    assert_eq!(report.bars_emitted, 5);
    assert_eq!(report.pages_network, 2);
    assert_eq!(report.pages_stored, 2);
    assert_eq!(report.pages_cache, 0);
    assert!(report.complete);
    assert_eq!(report.server_used_weight_max, 12);
    assert_eq!(transport.call_count(), 2);
    assert_eq!(cache.count_pages("BTCUSDT", Interval::M1).unwrap(), 2);

    // The second page request must use startTime = 3 minutes after START.
    let queries = transport.queries.lock().unwrap();
    assert_eq!(queries[0][2], ("startTime".to_string(), START.to_string()));
    assert_eq!(
        queries[1][2],
        ("startTime".to_string(), (START + 180_000).to_string())
    );
    assert_eq!(
        queries[0][3],
        ("endTime".to_string(), (end - 1).to_string())
    );
    drop(queries);

    // Second run uses only cached pages: the mock has no responses left.
    let (cached_bars, cached_report) = fetch_all_with_limit(&mut fetcher, START, end, 3);
    assert_eq!(cached_bars, bars);
    assert_eq!(cached_report.pages_cache, 2);
    assert_eq!(cached_report.pages_network, 0);
    assert_eq!(cached_report.pages_stored, 0);
    assert_eq!(transport.call_count(), 2);

    // Metadata records provenance and the raw body is byte-identical.
    let pages = cache.list_pages("BTCUSDT", Interval::M1).unwrap();
    assert_eq!(pages.len(), 2);
    let meta_path = pages[0].path.with_extension("meta.json");
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(meta_path).unwrap()).unwrap();
    assert_eq!(meta["http_status"], 200);
    assert_eq!(meta["record_count"], 3);
    assert_eq!(meta["used_weight_1m"], 7);
    assert_eq!(meta["symbol"], "BTCUSDT");
}

#[test]
fn refresh_appends_new_pages_without_overwriting() {
    let dir = tempfile::tempdir().unwrap();
    let cache = RawCache::new(dir.path());
    let end = START + 3 * 60_000;
    let transport = MockTransport::new(vec![
        response(
            200,
            klines(&[START, START + 60_000, START + 120_000], 42_000.0),
            &[],
        ),
        // Refreshed data at the same timestamps but different prices.
        response(
            200,
            klines(&[START, START + 60_000, START + 120_000], 43_000.0),
            &[],
        ),
    ]);
    let mut fetcher = KlinesFetcher::with_components(
        cache.clone(),
        config(),
        transport,
        Arc::new(MockSleeper::default()),
    );
    let (first, report) = fetch_all(&mut fetcher, START, end);
    assert_eq!(report.pages_stored, 1);
    let old_paths: Vec<_> = cache
        .list_pages("BTCUSDT", Interval::M1)
        .unwrap()
        .into_iter()
        .map(|page| page.path)
        .collect();
    let old_bytes = std::fs::read(&old_paths[0]).unwrap();

    let mut refresh = FetchRequest::new("BTCUSDT", Interval::M1, START, end);
    refresh.refresh = true;
    let mut refreshed = Vec::new();
    let report = fetcher
        .fetch(&refresh, &mut |bars| {
            refreshed.extend_from_slice(bars);
            Ok(())
        })
        .unwrap();
    assert_eq!(report.pages_stored, 1);
    assert_eq!(refreshed[0].close, 43_000.0);
    assert_ne!(first[0].close, refreshed[0].close);

    let pages = cache.list_pages("BTCUSDT", Interval::M1).unwrap();
    assert_eq!(pages.len(), 1, "reads use one snapshot per exact request");
    assert_eq!(cache.count_files("BTCUSDT", Interval::M1).unwrap(), 2);
    assert_ne!(pages[0].path, old_paths[0], "newest refresh snapshot wins");
    assert!(old_paths[0].exists());
    assert_eq!(std::fs::read(&old_paths[0]).unwrap(), old_bytes);

    let series = CachedSeries::new(cache, "BTCUSDT", Interval::M1);
    let newest = series.collect_range(START, end, 10).unwrap();
    assert_eq!(newest[0].close, 43_000.0);
}

#[test]
fn honors_retry_after_and_retries_5xx() {
    let dir = tempfile::tempdir().unwrap();
    let cache = RawCache::new(dir.path());
    let end = START + 3 * 60_000;
    let transport = MockTransport::new(vec![
        response(429, "{\"code\":-1003}".to_string(), &[("Retry-After", "2")]),
        response(503, "upstream".to_string(), &[]),
        response(
            200,
            klines(&[START, START + 60_000, START + 120_000], 42_000.0),
            &[],
        ),
    ]);
    let sleeper = Arc::new(MockSleeper::default());
    let mut fetcher =
        KlinesFetcher::with_components(cache, config(), transport.clone(), sleeper.clone());
    let (bars, report) = fetch_all(&mut fetcher, START, end);
    assert_eq!(bars.len(), 3);
    assert_eq!(report.rate_limit_hits, 1);
    assert_eq!(report.retries, 2);
    assert_eq!(transport.call_count(), 3);
    let sleeps = sleeper.sleeps.lock().unwrap();
    assert_eq!(sleeps[0], Duration::from_secs(2));
    assert!(sleeps[1] >= Duration::from_millis(500));
}

#[test]
fn stops_at_end_of_available_data() {
    let dir = tempfile::tempdir().unwrap();
    let cache = RawCache::new(dir.path());
    let transport = MockTransport::new(vec![response(
        200,
        klines(&[START, START + 60_000], 42_000.0),
        &[],
    )]);
    let mut fetcher = KlinesFetcher::with_components(
        cache,
        config(),
        transport,
        Arc::new(MockSleeper::default()),
    );
    let (bars, report) = fetch_all(&mut fetcher, START, START + 10 * 60_000);
    assert_eq!(bars.len(), 2);
    assert!(!report.complete);
    assert_eq!(
        report.stop_reason,
        Some(mb_data::StopReason::EndOfAvailableData)
    );
    assert_eq!(report.next_unfetched_ms, START + 2 * 60_000);
}

#[test]
fn empty_page_is_stored_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    let cache = RawCache::new(dir.path());
    let transport = MockTransport::new(vec![response(200, "[]".to_string(), &[])]);
    let mut fetcher = KlinesFetcher::with_components(
        cache.clone(),
        config(),
        transport,
        Arc::new(MockSleeper::default()),
    );
    let (bars, report) = fetch_all(&mut fetcher, START, START + 60_000);
    assert!(bars.is_empty());
    assert_eq!(report.stop_reason, Some(mb_data::StopReason::NoData));
    assert_eq!(cache.count_pages("BTCUSDT", Interval::M1).unwrap(), 1);
}

#[test]
fn rejects_non_2xx_without_retrying() {
    let dir = tempfile::tempdir().unwrap();
    let transport = MockTransport::new(vec![response(400, "bad symbol".to_string(), &[])]);
    let mut fetcher = KlinesFetcher::with_components(
        RawCache::new(dir.path()),
        config(),
        transport.clone(),
        Arc::new(MockSleeper::default()),
    );
    let request = FetchRequest::new("BTCUSDT", Interval::M1, START, START + 60_000);
    let error = fetcher.fetch(&request, &mut |_| Ok(())).unwrap_err();
    assert!(matches!(
        error,
        mb_data::DataError::HttpStatus { status: 400, .. }
    ));
    assert_eq!(transport.call_count(), 1);
}

#[test]
fn series_reader_merges_overlapping_pages_and_dedupes() {
    let dir = tempfile::tempdir().unwrap();
    let cache = RawCache::new(dir.path());
    let body_a = klines(&[START, START + 60_000, START + 120_000], 42_000.0);
    let body_b = klines(
        &[START + 120_000, START + 180_000, START + 240_000],
        42_000.0,
    );
    for (start, limit, body) in [(START, 3u16, body_a), (START + 120_000, 3, body_b)] {
        cache
            .store_page(
                &mb_data::PageRequest::new("BTCUSDT", Interval::M1, start, limit, START + 600_000),
                body.as_bytes(),
                &mb_data::StoreContext {
                    fetched_at_ms: 1,
                    http_status: 200,
                    used_weight_1m: None,
                    retry_after_seconds: None,
                    source_url: "test".to_string(),
                    record_count: 3,
                    first_open_time_ms: Some(start),
                    last_open_time_ms: Some(start + 120_000),
                },
            )
            .unwrap();
    }
    let series = CachedSeries::new(cache, "BTCUSDT", Interval::M1);
    let bars = series.collect_range(START, START + 600_000, 10).unwrap();
    assert_eq!(bars.len(), 5);
    assert_eq!(bars.first().unwrap().open_time_ms, START);
    assert_eq!(bars.last().unwrap().open_time_ms, START + 240_000);
    let report = series
        .read_range(START, START + 600_000, &mut |_| Ok(()))
        .unwrap();
    assert_eq!(report.duplicate_bars_skipped, 1);
}

#[test]
fn identical_page_is_deduplicated_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let cache = RawCache::new(dir.path());
    let body = klines(&[START, START + 60_000], 42_000.0);
    let request = mb_data::PageRequest::new("BTCUSDT", Interval::M1, START, 2, START + 120_000);
    let context = mb_data::StoreContext {
        fetched_at_ms: 123,
        http_status: 200,
        used_weight_1m: Some(2),
        retry_after_seconds: None,
        source_url: "test".to_string(),
        record_count: 2,
        first_open_time_ms: Some(START),
        last_open_time_ms: Some(START + 60_000),
    };
    let first = cache
        .store_page(&request, body.as_bytes(), &context)
        .unwrap();
    assert!(!first.deduplicated);
    let bytes = std::fs::read(&first.path).unwrap();
    let second = cache
        .store_page(&request, body.as_bytes(), &context)
        .unwrap();
    assert!(second.deduplicated);
    assert_eq!(second.path, first.path);
    assert_eq!(std::fs::read(&first.path).unwrap(), bytes);
    assert_eq!(cache.count_pages("BTCUSDT", Interval::M1).unwrap(), 1);
    let cached = cache.lookup(&request).unwrap().unwrap();
    assert_eq!(cached.body, body.as_bytes());
    assert_eq!(cached.meta.sha256, mb_data::sha256_hex(body.as_bytes()));
}

#[test]
#[ignore = "hits Binance public market-data API (keyless)"]
fn live_binance_smoke() {
    let dir = tempfile::tempdir().unwrap();
    let cache = RawCache::new(dir.path());
    let start = 1_704_067_200_000i64;
    let end = start + 3 * 60_000;
    let transport = mb_data::UreqTransport::default();
    let mut fetcher = KlinesFetcher::with_components(
        cache.clone(),
        FetchConfig::default(),
        std::sync::Arc::new(transport),
        std::sync::Arc::new(mb_data::ThreadSleeper),
    );
    let mut request = FetchRequest::new("BTCUSDT", Interval::M1, start, end);
    request.limit = 3;
    let (bars, report) = fetch_all_with_limit(&mut fetcher, start, end, 3);
    assert_eq!(bars.len(), 3);
    assert!(report.complete);
    assert_eq!(bars[0].open_time_ms, start);
    assert_eq!(cache.count_pages("BTCUSDT", Interval::M1).unwrap(), 1);
}
