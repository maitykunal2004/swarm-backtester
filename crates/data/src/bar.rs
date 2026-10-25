use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::DataError;

/// A single Binance spot kline. Field names mirror the exchange payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    /// Kline open time, milliseconds since Unix epoch (UTC).
    pub open_time_ms: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    /// Kline close time, milliseconds since Unix epoch (UTC).
    pub close_time_ms: i64,
    pub quote_volume: f64,
    pub trades: u64,
    pub taker_buy_base_volume: f64,
    pub taker_buy_quote_volume: f64,
}

impl Bar {
    /// Parse a raw Binance `/api/v3/klines` JSON body.
    ///
    /// The raw body is always left untouched on disk; this function only
    /// produces typed views of it. It rejects malformed rows instead of
    /// dropping them, so data quality problems surface immediately.
    pub fn parse_klines(body: &[u8]) -> Result<Vec<Bar>, DataError> {
        let value: Value = serde_json::from_slice(body)
            .map_err(|err| DataError::Parse(format!("invalid JSON: {err}")))?;
        let rows = value
            .as_array()
            .ok_or_else(|| DataError::Parse("expected a JSON array of klines".to_string()))?;

        let mut bars = Vec::with_capacity(rows.len());
        let mut previous: Option<i64> = None;
        for (index, row) in rows.iter().enumerate() {
            let bar =
                parse_row(row).map_err(|err| DataError::Parse(format!("row {index}: {err}")))?;
            bar.validate()
                .map_err(|err| DataError::Parse(format!("row {index}: {err}")))?;
            if let Some(previous_open) = previous {
                if bar.open_time_ms <= previous_open {
                    return Err(DataError::Parse(format!(
                        "row {index}: open time {} is not strictly after previous {}",
                        bar.open_time_ms, previous_open
                    )));
                }
            }
            previous = Some(bar.open_time_ms);
            bars.push(bar);
        }
        Ok(bars)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.open_time_ms <= 0 {
            return Err("open_time_ms must be positive".to_string());
        }
        if self.close_time_ms <= self.open_time_ms {
            return Err("close_time_ms must be after open_time_ms".to_string());
        }
        for (name, value) in [
            ("open", self.open),
            ("high", self.high),
            ("low", self.low),
            ("close", self.close),
        ] {
            if !value.is_finite() || value <= 0.0 {
                return Err(format!("{name} must be finite and positive, got {value}"));
            }
        }
        if !self.volume.is_finite() || self.volume < 0.0 {
            return Err(format!(
                "volume must be finite and non-negative: {}",
                self.volume
            ));
        }
        if !self.quote_volume.is_finite() || self.quote_volume < 0.0 {
            return Err("quote_volume must be finite and non-negative".to_string());
        }
        if !self.taker_buy_base_volume.is_finite() || self.taker_buy_base_volume < 0.0 {
            return Err("taker_buy_base_volume must be finite and non-negative".to_string());
        }
        if !self.taker_buy_quote_volume.is_finite() || self.taker_buy_quote_volume < 0.0 {
            return Err("taker_buy_quote_volume must be finite and non-negative".to_string());
        }
        if self.high < self.low {
            return Err(format!("high {} < low {}", self.high, self.low));
        }
        if self.high < self.open.max(self.close) - self.high * 1e-9
            || self.low > self.open.min(self.close) + self.low * 1e-9
        {
            return Err("open/close outside [low, high]".to_string());
        }
        Ok(())
    }

    /// The decision timestamp used by the dataset layer: the last millisecond
    /// of this bar, at which its close is final. Labels start strictly after
    /// this instant, at the next bar's open.
    pub fn decision_time_ms(&self) -> i64 {
        self.close_time_ms
    }
}

fn parse_row(row: &Value) -> Result<Bar, String> {
    let cells = row
        .as_array()
        .ok_or_else(|| "expected array kline row".to_string())?;
    if cells.len() < 11 {
        return Err(format!(
            "expected at least 11 kline fields, got {}",
            cells.len()
        ));
    }
    Ok(Bar {
        open_time_ms: number_i64(&cells[0], "open_time")?,
        open: number_f64(&cells[1], "open")?,
        high: number_f64(&cells[2], "high")?,
        low: number_f64(&cells[3], "low")?,
        close: number_f64(&cells[4], "close")?,
        volume: number_f64(&cells[5], "volume")?,
        close_time_ms: number_i64(&cells[6], "close_time")?,
        quote_volume: number_f64(&cells[7], "quote_volume")?,
        trades: number_i64(&cells[8], "trades")? as u64,
        taker_buy_base_volume: number_f64(&cells[9], "taker_buy_base")?,
        taker_buy_quote_volume: number_f64(&cells[10], "taker_buy_quote")?,
    })
}

fn number_f64(value: &Value, field: &str) -> Result<f64, String> {
    match value {
        Value::String(text) => text
            .parse::<f64>()
            .map_err(|err| format!("invalid {field} '{text}': {err}")),
        Value::Number(number) => number
            .as_f64()
            .ok_or_else(|| format!("invalid numeric {field}")),
        _ => Err(format!("{field} must be a string or number")),
    }
}

fn number_i64(value: &Value, field: &str) -> Result<i64, String> {
    match value {
        Value::String(text) => text
            .parse::<i64>()
            .map_err(|err| format!("invalid {field} '{text}': {err}")),
        Value::Number(number) => number
            .as_i64()
            .ok_or_else(|| format!("invalid integer {field}")),
        _ => Err(format!("{field} must be a string or integer")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[
      [1704067200000,"42000.10","42100.00","41900.00","42050.50","123.4",1704067259999,"5180000.1",500,"60.1","2520000.2","0"],
      [1704067260000,"42050.50","42110.00","42000.00","42100.00","100.0",1704067319999,"4210000.0",450,"50.0","2105000.0","0"]
    ]"#;

    #[test]
    fn parses_binance_payload() {
        let bars = Bar::parse_klines(SAMPLE.as_bytes()).unwrap();
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].open_time_ms, 1_704_067_200_000);
        assert_eq!(bars[0].close, 42_050.50);
        assert_eq!(bars[0].trades, 500);
        assert_eq!(bars[0].decision_time_ms(), 1_704_067_259_999);
        assert_eq!(bars[1].open_time_ms, bars[0].open_time_ms + 60_000);
    }

    #[test]
    fn rejects_unsorted_and_malformed_rows() {
        let unsorted = r#"[[1704067260000,"1","1","1","1","1",1704067319999,"1",1,"0","0","0"],
                          [1704067200000,"1","1","1","1","1",1704067259999,"1",1,"0","0","0"]]"#;
        assert!(Bar::parse_klines(unsorted.as_bytes()).is_err());
        assert!(Bar::parse_klines(b"not json").is_err());
        assert!(Bar::parse_klines(b"[[]]").is_err());
    }
}
