use std::fmt;
use std::str::FromStr;

use crate::error::DataError;

/// Binance spot kline intervals supported by this project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Interval {
    M1,
    M3,
    M5,
    M15,
    M30,
    H1,
    H2,
    H4,
    H6,
    H8,
    H12,
    D1,
    D3,
    W1,
}

impl Interval {
    pub const ALL: [Interval; 14] = [
        Interval::M1,
        Interval::M3,
        Interval::M5,
        Interval::M15,
        Interval::M30,
        Interval::H1,
        Interval::H2,
        Interval::H4,
        Interval::H6,
        Interval::H8,
        Interval::H12,
        Interval::D1,
        Interval::D3,
        Interval::W1,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Interval::M1 => "1m",
            Interval::M3 => "3m",
            Interval::M5 => "5m",
            Interval::M15 => "15m",
            Interval::M30 => "30m",
            Interval::H1 => "1h",
            Interval::H2 => "2h",
            Interval::H4 => "4h",
            Interval::H6 => "6h",
            Interval::H8 => "8h",
            Interval::H12 => "12h",
            Interval::D1 => "1d",
            Interval::D3 => "3d",
            Interval::W1 => "1w",
        }
    }

    pub fn duration_ms(self) -> i64 {
        match self {
            Interval::M1 => 60_000,
            Interval::M3 => 3 * 60_000,
            Interval::M5 => 5 * 60_000,
            Interval::M15 => 15 * 60_000,
            Interval::M30 => 30 * 60_000,
            Interval::H1 => 60 * 60_000,
            Interval::H2 => 2 * 60 * 60_000,
            Interval::H4 => 4 * 60 * 60_000,
            Interval::H6 => 6 * 60 * 60_000,
            Interval::H8 => 8 * 60 * 60_000,
            Interval::H12 => 12 * 60 * 60_000,
            Interval::D1 => 24 * 60 * 60_000,
            Interval::D3 => 3 * 24 * 60 * 60_000,
            Interval::W1 => 7 * 24 * 60 * 60_000,
        }
    }

    /// Binance kline open times are aligned to the interval grid.
    pub fn aligns(self, timestamp_ms: i64) -> bool {
        timestamp_ms > 0 && timestamp_ms % self.duration_ms() == 0
    }
}

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Interval {
    type Err = DataError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "1m" => Ok(Interval::M1),
            "3m" => Ok(Interval::M3),
            "5m" => Ok(Interval::M5),
            "15m" => Ok(Interval::M15),
            "30m" => Ok(Interval::M30),
            "1h" => Ok(Interval::H1),
            "2h" => Ok(Interval::H2),
            "4h" => Ok(Interval::H4),
            "6h" => Ok(Interval::H6),
            "8h" => Ok(Interval::H8),
            "12h" => Ok(Interval::H12),
            "1d" => Ok(Interval::D1),
            "3d" => Ok(Interval::D3),
            "1w" => Ok(Interval::W1),
            other => Err(DataError::InvalidRequest(format!(
                "unsupported kline interval '{other}'"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_round_trip() {
        for interval in Interval::ALL {
            let parsed: Interval = interval.as_str().parse().unwrap();
            assert_eq!(parsed, interval);
            assert!(interval.duration_ms() > 0);
        }
    }

    #[test]
    fn alignment() {
        assert!(Interval::M1.aligns(60_000));
        assert!(Interval::M5.aligns(300_000));
        assert!(!Interval::M5.aligns(60_000));
    }
}
