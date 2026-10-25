use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::DataError;
use crate::interval::Interval;

pub const CACHE_SCHEMA_VERSION: u32 = 1;
pub const VENUE: &str = "binance-spot";
pub const ENDPOINT: &str = "/api/v3/klines";

/// One exact page request. Cache keys are exact request ranges: a cached page
/// is only reused for the same symbol, interval, start, limit and end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    pub symbol: String,
    pub interval: Interval,
    pub start_ms: i64,
    pub limit: u16,
    pub end_ms: i64,
}

impl PageRequest {
    pub fn new(
        symbol: impl Into<String>,
        interval: Interval,
        start_ms: i64,
        limit: u16,
        end_ms: i64,
    ) -> Self {
        Self {
            symbol: symbol.into(),
            interval,
            start_ms,
            limit,
            end_ms,
        }
    }
}

/// Metadata sidecar for a raw page. The raw exchange bytes live in a separate
/// immutable `.json` file; this file records provenance only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageMeta {
    pub schema_version: u32,
    pub venue: String,
    pub endpoint: String,
    pub symbol: String,
    pub interval: String,
    pub request_start_ms: i64,
    pub request_limit: u16,
    pub request_end_ms: i64,
    pub fetched_at_ms: i64,
    pub http_status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_weight_1m: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    pub sha256: String,
    pub body_bytes: usize,
    pub record_count: usize,
    pub first_open_time_ms: Option<i64>,
    pub last_open_time_ms: Option<i64>,
    pub source_url: String,
}

#[derive(Debug, Clone)]
pub struct StoreContext {
    pub fetched_at_ms: i64,
    pub http_status: u16,
    pub used_weight_1m: Option<u32>,
    pub retry_after_seconds: Option<u64>,
    pub source_url: String,
    pub record_count: usize,
    pub first_open_time_ms: Option<i64>,
    pub last_open_time_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct StoreOutcome {
    pub path: PathBuf,
    pub meta: PageMeta,
    /// True when an identical page already existed (same request, timestamp and
    /// content hash). Existing files are never modified.
    pub deduplicated: bool,
}

#[derive(Debug, Clone)]
pub struct CachedPage {
    pub path: PathBuf,
    pub meta: PageMeta,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRef {
    pub path: PathBuf,
    pub start_ms: i64,
    pub limit: u16,
    pub end_ms: i64,
    pub fetched_at_ms: i64,
}

/// Append-only raw cache. Layout:
///
/// ```text
/// <root>/binance-spot/klines/<SYMBOL>/<interval>/<start>_<limit>_<end>_<fetched_at>_<sha8>.json
///                                                                ... .meta.json
/// <root>/binance-spot/klines/<SYMBOL>/<interval>/index.jsonl
/// ```
///
/// Files are written with `create_new`/hard-link semantics, so an existing raw
/// payload is never overwritten. A repeated fetch with `refresh` creates a new
/// timestamped, content-addressed file alongside the old one.
#[derive(Debug, Clone)]
pub struct RawCache {
    root: PathBuf,
}

impl RawCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn series_dir(&self, symbol: &str, interval: Interval) -> PathBuf {
        self.root
            .join(VENUE)
            .join("klines")
            .join(symbol)
            .join(interval.as_str())
    }

    /// List all raw pages for a series, sorted by request start then fetch time.
    /// Only file names are read here; payloads stay on disk.
    pub fn list_pages(&self, symbol: &str, interval: Interval) -> Result<Vec<PageRef>, DataError> {
        let dir = self.series_dir(symbol, interval);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut pages = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|err| DataError::cache_io(&dir, err))? {
            let entry = entry.map_err(|err| DataError::cache_io(&dir, err))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") || name.ends_with(".meta.json") {
                continue;
            }
            let stem = name.trim_end_matches(".json");
            let parts: Vec<&str> = stem.split('_').collect();
            if parts.len() != 5 {
                continue;
            }
            let parse = |value: &str| value.parse::<i64>().ok();
            let (Some(start), Some(limit), Some(end), Some(fetched)) = (
                parse(parts[0]),
                parse(parts[1]),
                parse(parts[2]),
                parse(parts[3]),
            ) else {
                continue;
            };
            pages.push(PageRef {
                path: entry.path(),
                start_ms: start,
                limit: limit as u16,
                end_ms: end,
                fetched_at_ms: fetched,
            });
        }
        pages.sort_by_key(|page| (page.start_ms, page.fetched_at_ms));
        Ok(pages)
    }

    /// Store one raw page. Never overwrites: identical bytes at the same
    /// content-addressed path are deduplicated, different bytes are an error.
    pub fn store_page(
        &self,
        request: &PageRequest,
        body: &[u8],
        context: &StoreContext,
    ) -> Result<StoreOutcome, DataError> {
        let dir = self.series_dir(&request.symbol, request.interval);
        fs::create_dir_all(&dir).map_err(|err| DataError::cache_io(&dir, err))?;

        let digest = sha256_hex(body);
        let hash8 = &digest[..8];
        let stem = format!(
            "{:013}_{:04}_{:013}_{:013}_{}",
            request.start_ms, request.limit, request.end_ms, context.fetched_at_ms, hash8
        );
        let body_path = dir.join(format!("{stem}.json"));
        let meta_path = dir.join(format!("{stem}.meta.json"));

        let mut deduplicated = false;
        if body_path.exists() {
            let existing = fs::read(&body_path)
                .map_err(|err| DataError::cache_io(&body_path, err))?;
            if existing != body {
                return Err(DataError::CacheConflict {
                    path: body_path,
                    details: "content hash prefix collided with different bytes".to_string(),
                });
            }
            deduplicated = true;
        } else {
            write_new_file(&body_path, body)?;
        }

        let meta = PageMeta {
            schema_version: CACHE_SCHEMA_VERSION,
            venue: VENUE.to_string(),
            endpoint: ENDPOINT.to_string(),
            symbol: request.symbol.clone(),
            interval: request.interval.as_str().to_string(),
            request_start_ms: request.start_ms,
            request_limit: request.limit,
            request_end_ms: request.end_ms,
            fetched_at_ms: context.fetched_at_ms,
            http_status: context.http_status,
            used_weight_1m: context.used_weight_1m,
            retry_after_seconds: context.retry_after_seconds,
            sha256: digest,
            body_bytes: body.len(),
            record_count: context.record_count,
            first_open_time_ms: context.first_open_time_ms,
            last_open_time_ms: context.last_open_time_ms,
            source_url: context.source_url.clone(),
        };

        if meta_path.exists() {
            let existing = fs::read(&meta_path)
                .map_err(|err| DataError::cache_io(&meta_path, err))?;
            let existing_meta: PageMeta = serde_json::from_slice(&existing).map_err(|err| {
                DataError::Parse(format!("corrupt cache metadata {meta_path:?}: {err}"))
            })?;
            if existing_meta != meta {
                return Err(DataError::CacheConflict {
                    path: meta_path,
                    details: "metadata differs for the same raw page".to_string(),
                });
            }
            deduplicated = true;
        } else {
            let meta_bytes = serde_json::to_vec_pretty(&meta)
                .map_err(|err| DataError::Parse(format!("could not serialize metadata: {err}")))?;
            write_new_file(&meta_path, &meta_bytes)?;
        }

        if !deduplicated {
            append_index_line(&dir.join("index.jsonl"), &meta)?;
        }

        Ok(StoreOutcome {
            path: body_path,
            meta,
            deduplicated,
        })
    }

    /// Return the newest cached page for an exact request, if present.
    pub fn lookup(&self, request: &PageRequest) -> Result<Option<CachedPage>, DataError> {
        let dir = self.series_dir(&request.symbol, request.interval);
        if !dir.exists() {
            return Ok(None);
        }
        let prefix = format!(
            "{:013}_{:04}_{:013}_",
            request.start_ms, request.limit, request.end_ms
        );
        let mut candidates: Vec<PathBuf> = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|err| DataError::cache_io(&dir, err))? {
            let entry = entry.map_err(|err| DataError::cache_io(&dir, err))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) && name.ends_with(".json") && !name.ends_with(".meta.json")
            {
                candidates.push(entry.path());
            }
        }
        candidates.sort();
        let Some(path) = candidates.pop() else {
            return Ok(None);
        };
        let body = fs::read(&path).map_err(|err| DataError::cache_io(&path, err))?;
        let meta_path = path.with_extension("meta.json");
        let meta_bytes = fs::read(&meta_path)
            .map_err(|err| DataError::cache_io(&meta_path, err))?;
        let meta: PageMeta = serde_json::from_slice(&meta_bytes).map_err(|err| {
            DataError::Parse(format!("corrupt cache metadata {meta_path:?}: {err}"))
        })?;
        let actual = sha256_hex(&body);
        if actual != meta.sha256 {
            return Err(DataError::CacheConflict {
                path,
                details: format!("sha256 mismatch: metadata {} actual {}", meta.sha256, actual),
            });
        }
        Ok(Some(CachedPage {
            path,
            meta,
            body,
        }))
    }

    pub fn count_pages(&self, symbol: &str, interval: Interval) -> Result<usize, DataError> {
        Ok(self.list_pages(symbol, interval)?.len())
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<(), DataError> {
    let parent = path
        .parent()
        .ok_or_else(|| DataError::InvalidRequest(format!("path has no parent: {path:?}")))?;
    let temp_name = format!(
        ".tmp-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    );
    let temp_path = parent.join(temp_name);
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|err| DataError::cache_io(&temp_path, err))?;
        file.write_all(bytes)
            .map_err(|err| DataError::cache_io(&temp_path, err))?;
        file.sync_all()
            .map_err(|err| DataError::cache_io(&temp_path, err))?;
    }
    match fs::hard_link(&temp_path, path) {
        Ok(()) => {
            let _ = fs::remove_file(&temp_path);
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&temp_path);
            let existing = fs::read(path).map_err(|read_err| DataError::cache_io(path, read_err))?;
            if existing == bytes {
                Ok(())
            } else {
                Err(DataError::CacheConflict {
                    path: path.to_path_buf(),
                    details: "refusing to overwrite an existing raw cache file".to_string(),
                })
            }
        }
        Err(err) => {
            let _ = fs::remove_file(&temp_path);
            Err(DataError::cache_io(path, err))
        }
    }
}

fn append_index_line(path: &Path, meta: &PageMeta) -> Result<(), DataError> {
    let mut line = serde_json::to_vec(meta)
        .map_err(|err| DataError::Parse(format!("could not serialize index entry: {err}")))?;
    line.push(b'\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| DataError::cache_io(path, err))?;
    file.write_all(&line)
        .map_err(|err| DataError::cache_io(path, err))?;
    file.sync_data()
        .map_err(|err| DataError::cache_io(path, err))?;
    Ok(())
}
