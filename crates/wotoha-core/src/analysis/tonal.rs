//! Tonal observations independent of the legacy automix representation.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::value::Confidence;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyMode {
    #[default]
    Major,
    Minor,
}

/// A reusable global key value. C is tonic 0, C-sharp 1, through B 11.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MusicalKey {
    pub tonic: u8,
    pub mode: KeyMode,
    pub confidence: Confidence,
}

pub type GlobalKey = MusicalKey;
pub type Key = MusicalKey;

impl MusicalKey {
    pub fn new(tonic: u8, mode: KeyMode, confidence: Confidence) -> Option<Self> {
        (tonic < 12).then_some(Self {
            tonic,
            mode,
            confidence,
        })
    }

    pub fn validate(&self) -> bool {
        self.tonic < 12 && self.confidence.validate()
    }
}

/// A bounded local tonal observation.  Windows are kept in source-time
/// coordinates so consumers can reuse the global key representation without
/// embedding cache or model-specific fields in the domain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalTonalWindow {
    pub start: Duration,
    pub end: Duration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<MusicalKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
}

impl LocalTonalWindow {
    pub fn new(
        start: Duration,
        end: Duration,
        key: Option<MusicalKey>,
        confidence: Option<Confidence>,
    ) -> Option<Self> {
        let window = Self {
            start,
            end,
            key,
            confidence,
        };
        window.validate().then_some(window)
    }

    pub fn validate(&self) -> bool {
        self.start < self.end
            && self.key.as_ref().is_none_or(MusicalKey::validate)
            && self.confidence.as_ref().is_none_or(Confidence::validate)
    }

    pub fn contains(&self, position: Duration) -> bool {
        self.start <= position && position < self.end
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TonalAnalysis {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_key: Option<MusicalKey>,
    #[serde(default)]
    pub local_windows: Vec<LocalTonalWindow>,
    #[serde(default)]
    pub alternatives: Vec<MusicalKey>,
}

impl TonalAnalysis {
    pub fn new(global_key: Option<MusicalKey>) -> Option<Self> {
        let analysis = Self {
            global_key,
            local_windows: Vec::new(),
            alternatives: Vec::new(),
        };
        analysis.validate().then_some(analysis)
    }

    pub fn validate(&self) -> bool {
        self.global_key.as_ref().is_none_or(MusicalKey::validate)
            && self.alternatives.iter().all(MusicalKey::validate)
            && self.local_windows.iter().all(LocalTonalWindow::validate)
            && self
                .local_windows
                .windows(2)
                .all(|window| window[0].end <= window[1].start)
    }

    pub fn key(&self) -> Option<MusicalKey> {
        self.global_key
    }

    pub fn local_key_at(&self, position: Duration) -> Option<MusicalKey> {
        self.local_windows
            .iter()
            .find(|window| window.contains(position))
            .and_then(|window| window.key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_key_is_reusable_and_bounded() {
        assert!(MusicalKey::new(12, KeyMode::Major, Confidence::ONE).is_none());
        let key = MusicalKey::new(0, KeyMode::Major, Confidence::ONE).unwrap();
        let tonal = TonalAnalysis::new(Some(key)).unwrap();
        assert_eq!(tonal.key(), Some(key));
    }

    #[test]
    fn local_tonal_windows_are_ordered_and_reusable() {
        let key = MusicalKey::new(7, KeyMode::Minor, Confidence::ONE).unwrap();
        let window = LocalTonalWindow::new(
            Duration::from_secs(1),
            Duration::from_secs(3),
            Some(key),
            Some(Confidence::clamped(0.8)),
        )
        .unwrap();
        let mut tonal = TonalAnalysis::new(None).unwrap();
        tonal.local_windows.push(window);
        assert_eq!(tonal.local_key_at(Duration::from_secs(2)), Some(key));
        assert!(tonal.validate());
    }
}
