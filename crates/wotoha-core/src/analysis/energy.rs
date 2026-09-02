//! Versioned energy-profile domain values.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::value::{Confidence, UnitInterval};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnergyFrame {
    pub time: Duration,
    pub level: UnitInterval,
    pub confidence: Confidence,
}

impl EnergyFrame {
    pub fn validate(&self) -> bool {
        self.level.validate() && self.confidence.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnergyAnalysis {
    /// Normalized energy samples, with matching confidence values.
    pub profile: Vec<UnitInterval>,
    pub confidences: Vec<Confidence>,
    pub rate_hz: u16,
    /// Unweighted full-band RMS in dBFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rms_dbfs: Option<f32>,
    /// Sample peak in dBFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_peak_dbfs: Option<f32>,
    /// ITU-R BS.1770 integrated programme loudness in LUFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrated_lufs: Option<f32>,
    /// Maximum oversampled true peak in dBTP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub true_peak_dbtp: Option<f32>,
}

impl Default for EnergyAnalysis {
    fn default() -> Self {
        Self {
            profile: Vec::new(),
            confidences: Vec::new(),
            rate_hz: 1,
            rms_dbfs: None,
            sample_peak_dbfs: None,
            integrated_lufs: None,
            true_peak_dbtp: None,
        }
    }
}

impl EnergyAnalysis {
    pub fn new(
        profile: Vec<UnitInterval>,
        confidences: Vec<Confidence>,
        rate_hz: u16,
    ) -> Option<Self> {
        let analysis = Self {
            profile,
            confidences,
            rate_hz,
            rms_dbfs: None,
            sample_peak_dbfs: None,
            integrated_lufs: None,
            true_peak_dbtp: None,
        };
        analysis.validate().then_some(analysis)
    }

    /// Adapter-friendly constructor for the legacy u8 profile.
    pub fn from_quantized(profile: &[u8], rate_hz: u16) -> Option<Self> {
        Self::new(
            profile
                .iter()
                .map(|value| UnitInterval::clamped(f32::from(*value) / 255.0))
                .collect(),
            vec![Confidence::ONE; profile.len()],
            rate_hz,
        )
    }

    pub fn validate(&self) -> bool {
        self.rate_hz > 0
            && self.profile.len() == self.confidences.len()
            && self.profile.iter().all(UnitInterval::validate)
            && self.confidences.iter().all(Confidence::validate)
            && self.rms_dbfs.is_none_or(f32::is_finite)
            && self.sample_peak_dbfs.is_none_or(f32::is_finite)
            && self.integrated_lufs.is_none_or(f32::is_finite)
            && self.true_peak_dbtp.is_none_or(f32::is_finite)
    }

    pub fn with_loudness(
        mut self,
        rms_dbfs: Option<f32>,
        sample_peak_dbfs: Option<f32>,
        integrated_lufs: Option<f32>,
        true_peak_dbtp: Option<f32>,
    ) -> Option<Self> {
        self.rms_dbfs = rms_dbfs;
        self.sample_peak_dbfs = sample_peak_dbfs;
        self.integrated_lufs = integrated_lufs;
        self.true_peak_dbtp = true_peak_dbtp;
        self.validate().then_some(self)
    }

    pub fn level_at(&self, position: Duration) -> Option<UnitInterval> {
        if !self.validate() {
            return None;
        }
        let index = (position.as_secs_f64() * f64::from(self.rate_hz)).floor() as usize;
        let (&level, &confidence) = self.profile.get(index).zip(self.confidences.get(index))?;
        Some(UnitInterval::clamped(level.get() * confidence.get()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_energy_profile_can_be_adapted() {
        let energy = EnergyAnalysis::from_quantized(&[0, 255], 4).unwrap();
        assert_eq!(
            energy.level_at(Duration::from_millis(250)),
            Some(UnitInterval::ONE)
        );
        let energy = energy
            .with_loudness(Some(-18.0), Some(-1.0), Some(-14.0), Some(-0.5))
            .unwrap();
        assert!(energy.validate());
    }

    #[test]
    fn mismatched_profile_indexes_are_rejected() {
        assert!(EnergyAnalysis::new(vec![UnitInterval::ONE], Vec::new(), 4).is_none());
    }

    #[test]
    fn non_finite_loudness_is_rejected() {
        let mut energy = EnergyAnalysis::from_quantized(&[128], 4).unwrap();
        energy.rms_dbfs = Some(f32::NAN);
        assert!(!energy.validate());
    }
}
