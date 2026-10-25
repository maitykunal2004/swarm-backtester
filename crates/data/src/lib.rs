//! Keyless Binance spot data ingestion and immutable raw caching.
//!
//! Design constraints:
//! - only public REST endpoints; no API keys or signed requests;
//! - raw exchange bytes are persisted verbatim and never overwritten;
//! - page bodies are processed one at a time so memory stays bounded.

pub mod bar;
pub mod cache;
pub mod error;
pub mod fetch;
pub mod http;
pub mod interval;
pub mod rate_limit;
pub mod series;

pub use bar::Bar;
pub use cache::{
    now_ms, sha256_hex, CachedPage, PageMeta, PageRef, PageRequest, RawCache, StoreContext,
    StoreOutcome,
};
pub use error::DataError;
pub use fetch::{
    FetchConfig, FetchReport, FetchRequest, KlinesFetcher, Sleeper, StopReason, ThreadSleeper,
};
pub use http::{HttpResponse, HttpTransport, UreqTransport};
pub use interval::Interval;
pub use rate_limit::{klines_weight, RateLimitPolicy, WeightLimiter};
pub use series::{CachedSeries, SeriesReadReport};

/// Binance public market-data base URL (no authentication required).
pub const BINANCE_SPOT_BASE_URL: &str = "https://api.binance.com";
/// Official keyless market-data endpoint; usable from regions where the main
/// API returns HTTP 451. Same `/api/v3/klines` schema.
pub const BINANCE_DATA_API_BASE_URL: &str = "https://data-api.binance.vision";
