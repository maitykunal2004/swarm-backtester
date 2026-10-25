use serde::Serialize;

use crate::bar::Bar;
use crate::cache::RawCache;
use crate::error::DataError;
use crate::interval::Interval;

/// Streaming reader over the append-only raw cache. At most one cached page
/// (<= 1000 klines) is held in memory at a time.
#[derive(Debug, Clone)]
pub struct CachedSeries {
    cache: RawCache,
    symbol: String,
    interval: Interval,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SeriesReadReport {
    pub pages_matched: usize,
    pub pages_read: usize,
    pub duplicate_bars_skipped: u64,
    pub bars_emitted: u64,
    pub first_open_time_ms: Option<i64>,
    pub last_open_time_ms: Option<i64>,
    pub first_gap_after_ms: Option<i64>,
}

impl CachedSeries {
    pub fn new(cache: RawCache, symbol: impl Into<String>, interval: Interval) -> Self {
        Self {
            cache,
            symbol: symbol.into(),
            interval,
        }
    }

    pub fn cache(&self) -> &RawCache {
        &self.cache
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub fn interval(&self) -> Interval {
        self.interval
    }

    /// Stream bars with `start_ms <= open_time < end_ms` in ascending order.
    /// Duplicate bars across overlapping cached pages are skipped.
    pub fn read_range<F>(
        &self,
        start_ms: i64,
        end_ms: i64,
        on_bar: &mut F,
    ) -> Result<SeriesReadReport, DataError>
    where
        F: FnMut(&Bar) -> Result<(), DataError>,
    {
        if start_ms >= end_ms {
            return Err(DataError::InvalidRequest(format!(
                "empty series range [{start_ms}, {end_ms})"
            )));
        }
        let interval_ms = self.interval.duration_ms();
        let pages = self.cache.list_pages(&self.symbol, self.interval)?;
        let matched: Vec<_> = pages
            .into_iter()
            .filter(|page| {
                let page_span_end = page.start_ms.saturating_add(page.limit as i64 * interval_ms);
                page.start_ms < end_ms && page_span_end > start_ms
            })
            .collect();

        let mut report = SeriesReadReport {
            pages_matched: matched.len(),
            ..Default::default()
        };
        let mut last_open_time: Option<i64> = None;

        for page in matched {
            report.pages_read += 1;
            let body = std::fs::read(&page.path)
                .map_err(|err| DataError::cache_io(&page.path, err))?;
            let bars = Bar::parse_klines(&body)?;
            for bar in &bars {
                if bar.open_time_ms < start_ms || bar.open_time_ms >= end_ms {
                    continue;
                }
                if let Some(previous) = last_open_time {
                    if bar.open_time_ms <= previous {
                        report.duplicate_bars_skipped += 1;
                        continue;
                    }
                    if report.first_gap_after_ms.is_none()
                        && bar.open_time_ms != previous + interval_ms
                    {
                        report.first_gap_after_ms = Some(previous);
                    }
                }
                on_bar(bar)?;
                report.bars_emitted += 1;
                report
                    .first_open_time_ms
                    .get_or_insert(bar.open_time_ms);
                report.last_open_time_ms = Some(bar.open_time_ms);
                last_open_time = Some(bar.open_time_ms);
            }
        }
        Ok(report)
    }

    /// Convenience collector with an explicit cap so tests and ad-hoc tools
    /// cannot accidentally load a multi-year history into RAM.
    pub fn collect_range(
        &self,
        start_ms: i64,
        end_ms: i64,
        max_bars: usize,
    ) -> Result<Vec<Bar>, DataError> {
        let mut bars = Vec::new();
        let report = self.read_range(start_ms, end_ms, &mut |bar| {
            if bars.len() >= max_bars {
                return Err(DataError::Consumer(format!(
                    "collect_range exceeded cap of {max_bars} bars"
                )));
            }
            bars.push(bar.clone());
            Ok(())
        })?;
        let _ = report;
        Ok(bars)
    }
}
