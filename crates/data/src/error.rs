use std::path::PathBuf;

/// All errors produced by the data layer. Errors are deliberately explicit:
/// malformed exchange responses and cache conflicts are never silently skipped.
#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("http transport error: {0}")]
    Transport(String),

    #[error("binance returned HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },

    #[error("retries exhausted after {attempts} attempts: {last_error}")]
    RetryExhausted { attempts: u32, last_error: String },

    #[error("rate limit budget exhausted; retry after {retry_after_ms} ms")]
    RateLimitBudget { retry_after_ms: i64 },

    #[error("malformed kline payload: {0}")]
    Parse(String),

    #[error("cache conflict at {path}: {details} (refusing to overwrite)")]
    CacheConflict { path: PathBuf, details: String },

    #[error("cache error at {path}: {source}")]
    CacheIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("consumer rejected bar batch: {0}")]
    Consumer(String),
}

impl DataError {
    pub fn cache_io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::CacheIo {
            path: path.into(),
            source,
        }
    }
}
