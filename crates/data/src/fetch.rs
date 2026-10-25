use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use serde::Serialize;

use crate::bar::Bar;
use crate::cache::{now_ms, PageRequest, RawCache, StoreContext};
use crate::error::DataError;
use crate::http::{HttpResponse, HttpTransport, UreqTransport};
use crate::interval::Interval;
use crate::rate_limit::{klines_weight, RateLimitPolicy, WeightLimiter};

#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub symbol: String,
    pub interval: Interval,
    pub start_ms: i64,
    pub end_ms: i64,
    pub limit: u16,
    /// Bypass the cache lookup. New pages are stored alongside older ones.
    pub refresh: bool,
}

impl FetchRequest {
    pub fn new(symbol: impl Into<String>, interval: Interval, start_ms: i64, end_ms: i64) -> Self {
        Self {
            symbol: symbol.into().to_ascii_uppercase(),
            interval,
            start_ms,
            end_ms,
            limit: 1000,
            refresh: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FetchConfig {
    pub base_url: String,
    pub max_retries: u32,
    pub max_pages: usize,
    pub max_response_bytes: usize,
    pub rate_limit: RateLimitPolicy,
    pub user_agent: String,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.binance.com".to_string(),
            max_retries: 5,
            max_pages: 200_000,
            max_response_bytes: 8 * 1024 * 1024,
            rate_limit: RateLimitPolicy::default(),
            user_agent: "swarm-backtester/0.1 (research; keyless)".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum StopReason {
    EndReached,
    EndOfAvailableData,
    NoData,
    MaxPages,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct FetchReport {
    pub pages_network: u64,
    pub pages_cache: u64,
    pub pages_stored: u64,
    pub cache_duplicates: u64,
    pub bars_emitted: u64,
    pub retries: u64,
    pub rate_limit_hits: u64,
    pub server_used_weight_max: u32,
    pub first_bar_open_time_ms: Option<i64>,
    pub last_bar_open_time_ms: Option<i64>,
    pub next_unfetched_ms: i64,
    pub complete: bool,
    pub stop_reason: Option<StopReason>,
}

pub trait Sleeper: Send + Sync {
    fn sleep(&self, duration: Duration);
}

pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

struct PageAttempt {
    response: HttpResponse,
    retries: u64,
    rate_limit_hits: u64,
}

/// Keyless Binance `/api/v3/klines` fetcher with explicit pagination, local
/// weight limiting, retry/backoff and an append-only raw cache.
pub struct KlinesFetcher {
    cache: RawCache,
    config: FetchConfig,
    transport: Arc<dyn HttpTransport>,
    sleeper: Arc<dyn Sleeper>,
    limiter: WeightLimiter,
}

impl KlinesFetcher {
    pub fn new(cache: RawCache, config: FetchConfig) -> Self {
        let transport = Arc::new(UreqTransport::new(
            Duration::from_secs(20),
            config.max_response_bytes,
        ));
        Self::with_components(cache, config, transport, Arc::new(ThreadSleeper))
    }

    pub fn with_components(
        cache: RawCache,
        config: FetchConfig,
        transport: Arc<dyn HttpTransport>,
        sleeper: Arc<dyn Sleeper>,
    ) -> Self {
        let limiter = WeightLimiter::new(config.rate_limit);
        Self {
            cache,
            config,
            transport,
            sleeper,
            limiter,
        }
    }

    pub fn cache(&self) -> &RawCache {
        &self.cache
    }

    /// Fetch `[start_ms, end_ms)` and stream each page to `on_bars`. The raw
    /// page is persisted before the callback runs.
    pub fn fetch(
        &mut self,
        request: &FetchRequest,
        on_bars: &mut dyn FnMut(&[Bar]) -> Result<(), DataError>,
    ) -> Result<FetchReport, DataError> {
        validate_request(request, &self.config)?;
        let interval_ms = request.interval.duration_ms();
        let mut report = FetchReport {
            next_unfetched_ms: request.start_ms,
            ..Default::default()
        };
        let mut cursor = request.start_ms;
        let mut pages = 0usize;

        while cursor < request.end_ms {
            if pages >= self.config.max_pages {
                report.stop_reason = Some(StopReason::MaxPages);
                break;
            }
            pages += 1;
            let page_request = PageRequest::new(
                request.symbol.clone(),
                request.interval,
                cursor,
                request.limit,
                request.end_ms,
            );

            let bars = if !request.refresh {
                if let Some(cached) = self.cache.lookup(&page_request)? {
                    report.pages_cache += 1;
                    Bar::parse_klines(&cached.body)?
                } else {
                    self.fetch_page(&page_request, &mut report)?
                }
            } else {
                self.fetch_page(&page_request, &mut report)?
            };

            let visible: Vec<Bar> = bars
                .iter()
                .filter(|bar| bar.open_time_ms >= cursor && bar.open_time_ms < request.end_ms)
                .cloned()
                .collect();
            if !visible.is_empty() {
                if let Some(first) = visible.first() {
                    report
                        .first_bar_open_time_ms
                        .get_or_insert(first.open_time_ms);
                }
                report.last_bar_open_time_ms = visible.last().map(|bar| bar.open_time_ms);
                report.bars_emitted += visible.len() as u64;
                on_bars(&visible)?;
            }

            let Some(last) = bars.last() else {
                report.stop_reason = Some(StopReason::NoData);
                break;
            };
            let next = last.open_time_ms + interval_ms;
            if next <= cursor {
                return Err(DataError::Parse(format!(
                    "page at {cursor} did not advance (last bar {})",
                    last.open_time_ms
                )));
            }
            cursor = next;
            report.next_unfetched_ms = cursor;

            if bars.len() < request.limit as usize {
                report.stop_reason = Some(StopReason::EndOfAvailableData);
                break;
            }
        }

        if cursor >= request.end_ms {
            report.complete = true;
            report.stop_reason = Some(StopReason::EndReached);
        }
        if report.stop_reason.is_none() {
            report.stop_reason = Some(StopReason::MaxPages);
        }
        report.server_used_weight_max = self.limiter.server_used_weight_max();
        Ok(report)
    }

    fn fetch_page(
        &mut self,
        page_request: &PageRequest,
        report: &mut FetchReport,
    ) -> Result<Vec<Bar>, DataError> {
        let weight = klines_weight(page_request.limit);
        self.acquire(weight)?;
        let attempt = self.request_page(page_request)?;
        report.pages_network += 1;
        report.retries += attempt.retries;
        report.rate_limit_hits += attempt.rate_limit_hits;

        let used_weight = attempt
            .response
            .header("x-mbx-used-weight-1m")
            .and_then(|value| value.parse::<u32>().ok());
        if let Some(used) = used_weight {
            self.limiter.observe_server_weight(used);
        }
        let retry_after = attempt
            .response
            .header("retry-after")
            .and_then(|value| value.parse::<u64>().ok());

        let bars = Bar::parse_klines(&attempt.response.body)?;
        let context = StoreContext {
            fetched_at_ms: now_ms(),
            http_status: attempt.response.status,
            used_weight_1m: used_weight,
            retry_after_seconds: retry_after,
            source_url: source_url(&self.config.base_url, page_request),
            record_count: bars.len(),
            first_open_time_ms: bars.first().map(|bar| bar.open_time_ms),
            last_open_time_ms: bars.last().map(|bar| bar.open_time_ms),
        };
        let outcome = self
            .cache
            .store_page(page_request, &attempt.response.body, &context)?;
        if outcome.deduplicated {
            report.cache_duplicates += 1;
        } else {
            report.pages_stored += 1;
        }
        Ok(bars)
    }

    fn acquire(&mut self, weight: u32) -> Result<(), DataError> {
        let mut waited_ms: i64 = 0;
        loop {
            let wait = self.limiter.acquire_at(weight, now_ms());
            if wait.is_zero() {
                return Ok(());
            }
            if waited_ms + wait.as_millis() as i64 > 180_000 {
                return Err(DataError::RateLimitBudget {
                    retry_after_ms: wait.as_millis() as i64,
                });
            }
            waited_ms += wait.as_millis() as i64;
            self.sleeper.sleep(wait);
        }
    }

    #[allow(unused_assignments)]
    fn request_page(&mut self, page_request: &PageRequest) -> Result<PageAttempt, DataError> {
        let url = format!(
            "{}/api/v3/klines",
            self.config.base_url.trim_end_matches('/')
        );
        let query = vec![
            ("symbol", page_request.symbol.clone()),
            ("interval", page_request.interval.as_str().to_string()),
            ("startTime", page_request.start_ms.to_string()),
            ("endTime", (page_request.end_ms - 1).to_string()),
            ("limit", page_request.limit.to_string()),
        ];
        let headers = vec![("User-Agent", self.config.user_agent.clone())];
        let mut retries = 0u64;
        let mut rate_limit_hits = 0u64;
        let mut attempt = 0u32;
        let mut last_error = String::new();

        loop {
            match self.transport.get(&url, &query, &headers) {
                Ok(response) if response.is_success() => {
                    return Ok(PageAttempt {
                        response,
                        retries,
                        rate_limit_hits,
                    })
                }
                Ok(response) if response.status == 429 || response.status == 418 => {
                    rate_limit_hits += 1;
                    last_error = format!("HTTP {} rate limited", response.status);
                    if attempt >= self.config.max_retries {
                        return Err(DataError::RetryExhausted {
                            attempts: attempt + 1,
                            last_error,
                        });
                    }
                    let retry_after = response
                        .header("retry-after")
                        .and_then(|value| value.parse::<u64>().ok())
                        .unwrap_or(60)
                        .clamp(1, 300);
                    self.sleeper.sleep(Duration::from_secs(retry_after));
                }
                Ok(response) if (500..600).contains(&response.status) => {
                    last_error = format!(
                        "HTTP {}: {}",
                        response.status,
                        truncate(&String::from_utf8_lossy(&response.body), 300)
                    );
                    if attempt >= self.config.max_retries {
                        return Err(DataError::RetryExhausted {
                            attempts: attempt + 1,
                            last_error,
                        });
                    }
                    self.sleeper.sleep(backoff(attempt));
                }
                Ok(response) => {
                    return Err(DataError::HttpStatus {
                        status: response.status,
                        body: truncate(&String::from_utf8_lossy(&response.body), 1000),
                    })
                }
                Err(err) => {
                    last_error = err;
                    if attempt >= self.config.max_retries {
                        return Err(DataError::RetryExhausted {
                            attempts: attempt + 1,
                            last_error,
                        });
                    }
                    self.sleeper.sleep(backoff(attempt));
                }
            }
            attempt += 1;
            retries += 1;
        }
    }
}

fn validate_request(request: &FetchRequest, config: &FetchConfig) -> Result<(), DataError> {
    if request.symbol.is_empty() || request.symbol.len() > 20 {
        return Err(DataError::InvalidRequest(format!(
            "invalid symbol '{}'",
            request.symbol
        )));
    }
    if !request
        .symbol
        .chars()
        .all(|character| character.is_ascii_uppercase() || character.is_ascii_digit())
    {
        return Err(DataError::InvalidRequest(format!(
            "symbol must be uppercase alphanumeric: '{}'",
            request.symbol
        )));
    }
    if request.start_ms <= 0 || request.end_ms <= request.start_ms {
        return Err(DataError::InvalidRequest(format!(
            "invalid range [{}, {})",
            request.start_ms, request.end_ms
        )));
    }
    if request.limit == 0 || request.limit > 1000 {
        return Err(DataError::InvalidRequest(format!(
            "limit must be 1..=1000, got {}",
            request.limit
        )));
    }
    if config.max_pages == 0 {
        return Err(DataError::InvalidRequest("max_pages must be > 0".to_string()));
    }
    Ok(())
}

fn source_url(base_url: &str, page: &PageRequest) -> String {
    format!(
        "{}/api/v3/klines?symbol={}&interval={}&startTime={}&endTime={}&limit={}",
        base_url.trim_end_matches('/'),
        page.symbol,
        page.interval.as_str(),
        page.start_ms,
        page.end_ms - 1,
        page.limit
    )
}

fn backoff(attempt: u32) -> Duration {
    let base = 500u64.saturating_mul(1u64 << attempt.min(6));
    let jitter = rand::thread_rng().gen_range(0..=250);
    Duration::from_millis(base.min(30_000) + jitter)
}

fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        value.to_string()
    } else {
        format!("{}...", &value[..max])
    }
}
