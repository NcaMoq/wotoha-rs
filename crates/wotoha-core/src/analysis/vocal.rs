//! Versioned vocal-activity domain values.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::value::{Confidence, UnitInterval};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct VocalFrame {
    pub time: Duration,
    pub activity: UnitInterval,
    pub confidence: Confidence,
}

impl VocalFrame {
    pub fn validate(&self) -> bool {
        self.activity.validate() && self.confidence.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VocalAnalysis {
    /// Activity and confidence have matching indexes and are sampled at the
    /// declared rate in Hz.
    pub activity: Vec<UnitInterval>,
    pub confidences: Vec<Confidence>,
    pub rate_hz: u16,
}

impl Default for VocalAnalysis {
    fn default() -> Self {
        Self {
            activity: Vec::new(),
            confidences: Vec::new(),
            rate_hz: 1,
        }
    }
}

impl VocalAnalysis {
    pub fn new(
        activity: Vec<UnitInterval>,
        confidences: Vec<Confidence>,
        rate_hz: u16,
    ) -> Option<Self> {
        let analysis = Self {
            activity,
            confidences,
            rate_hz,
        };
        analysis.validate().then_some(analysis)
    }

    /// Adapter-friendly constructor for the legacy quantized profile format.
    pub fn from_quantized(activity: &[u8], confidences: &[u8], rate_hz: u16) -> Option<Self> {
        if activity.len() != confidences.len() {
            return None;
        }
        Self::new(
            activity
                .iter()
                .map(|value| UnitInterval::clamped(f32::from(*value) / 255.0))
                .collect(),
            confidences
                .iter()
                .map(|value| Confidence::clamped(f32::from(*value) / 255.0))
                .collect(),
            rate_hz,
        )
    }

    pub fn validate(&self) -> bool {
        self.rate_hz > 0
            && self.activity.len() == self.confidences.len()
            && self.activity.iter().all(UnitInterval::validate)
            && self.confidences.iter().all(Confidence::validate)
    }

    pub fn risk_at(&self, position: Duration) -> Option<UnitInterval> {
        if !self.validate() {
            return None;
        }
        let index = (position.as_secs_f64() * f64::from(self.rate_hz)).floor() as usize;
        let (&risk, &confidence) = self.activity.get(index).zip(self.confidences.get(index))?;
        Some(UnitInterval::clamped(
            confidence.get() * risk.get() + (1.0 - confidence.get()) * 0.65,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_quantized_profile_can_be_adapted() {
        let vocal = VocalAnalysis::from_quantized(&[0, 255], &[255, 128], 4).unwrap();
        assert!(vocal.validate());
        assert!(vocal.risk_at(Duration::from_millis(250)).is_some());
    }

    #[test]
    fn mismatched_profile_indexes_are_rejected() {
        assert!(VocalAnalysis::from_quantized(&[1], &[], 4).is_none());
    }
}
