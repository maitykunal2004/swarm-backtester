use mb_data::Bar;

use crate::error::FeatureError;

/// A strict point-in-time view over a bar buffer.
///
/// Feature code only ever receives this type, never the underlying buffer, so
/// it cannot index a bar after `decision_bar_index`. The constructor validates
/// ordering and rejects an out-of-range decision index.
#[derive(Debug, Clone, Copy)]
pub struct PointInTime<'a> {
    bars: &'a [Bar],
    decision_bar_index: usize,
}

impl<'a> PointInTime<'a> {
    pub fn new(bars: &'a [Bar], decision_bar_index: usize) -> Result<Self, FeatureError> {
        if bars.is_empty() {
            return Err(FeatureError::InvalidSpec(
                "point-in-time view requires at least one bar".to_string(),
            ));
        }
        if decision_bar_index >= bars.len() {
            return Err(FeatureError::Leakage(format!(
                "decision index {decision_bar_index} is outside {} bars",
                bars.len()
            )));
        }
        for window in bars.windows(2) {
            if window[1].open_time_ms <= window[0].open_time_ms {
                return Err(FeatureError::InvalidSpec(
                    "bars must be strictly increasing in open time".to_string(),
                ));
            }
        }
        let decision_time_ms = bars[decision_bar_index].decision_time_ms();
        if bars[decision_bar_index].close_time_ms != decision_time_ms {
            return Err(FeatureError::InvalidSpec(
                "decision timestamp must equal the decision bar close time".to_string(),
            ));
        }
        Ok(Self {
            bars,
            decision_bar_index,
        })
    }

    /// All bars visible at the decision instant. Future bars are not reachable
    /// through this API.
    pub fn bars(&self) -> &'a [Bar] {
        &self.bars[..=self.decision_bar_index]
    }

    pub fn decision_bar(&self) -> &'a Bar {
        &self.bars[self.decision_bar_index]
    }

    pub fn decision_index(&self) -> usize {
        self.decision_bar_index
    }

    /// First instant at which the decision bar is complete.
    pub fn decision_time_ms(&self) -> i64 {
        self.bars[self.decision_bar_index].decision_time_ms()
    }

    pub fn len(&self) -> usize {
        self.decision_bar_index + 1
    }

    pub fn is_empty(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selftest::synthetic_bars;

    #[test]
    fn hides_future_bars() {
        let bars = synthetic_bars(10, 1_704_067_200_000, 60_000, 7);
        let pit = PointInTime::new(&bars, 3).unwrap();
        assert_eq!(pit.len(), 4);
        assert_eq!(pit.bars().len(), 4);
        assert_eq!(
            pit.bars().last().unwrap().open_time_ms,
            bars[3].open_time_ms
        );
        assert_eq!(pit.decision_time_ms(), bars[3].close_time_ms);
        assert!(PointInTime::new(&bars, 10).is_err());
        assert!(PointInTime::new(&[], 0).is_err());
    }
}
