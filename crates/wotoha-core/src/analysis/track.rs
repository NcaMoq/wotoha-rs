//! Top-level versioned track analysis record.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{
    AnalysisProvenance, DjCue, EnergyAnalysis, RhythmAnalysis, StructureAnalysis, TonalAnalysis,
    VocalAnalysis,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrackAnalysisV2 {
    pub duration: Duration,
    pub audible_start: Duration,
    pub audible_end: Duration,
    #[serde(default)]
    pub rhythm: RhythmAnalysis,
    #[serde(default)]
    pub structure: StructureAnalysis,
    #[serde(default)]
    pub tonal: TonalAnalysis,
    #[serde(default)]
    pub vocal: VocalAnalysis,
    #[serde(default)]
    pub energy: EnergyAnalysis,
    #[serde(default)]
    pub cues: Vec<DjCue>,
    #[serde(default)]
    pub provenance: AnalysisProvenance,
}

impl TrackAnalysisV2 {
    pub fn new(duration: Duration, audible_start: Duration, audible_end: Duration) -> Option<Self> {
        let analysis = Self {
            duration,
            audible_start,
            audible_end,
            rhythm: RhythmAnalysis::default(),
            structure: StructureAnalysis::default(),
            tonal: TonalAnalysis::default(),
            vocal: VocalAnalysis::default(),
            energy: EnergyAnalysis::default(),
            cues: Vec::new(),
            provenance: AnalysisProvenance::default(),
        };
        analysis.validate().then_some(analysis)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_components(
        duration: Duration,
        audible_start: Duration,
        audible_end: Duration,
        rhythm: RhythmAnalysis,
        structure: StructureAnalysis,
        tonal: TonalAnalysis,
        vocal: VocalAnalysis,
        energy: EnergyAnalysis,
        cues: Vec<DjCue>,
        provenance: AnalysisProvenance,
    ) -> Option<Self> {
        let analysis = Self {
            duration,
            audible_start,
            audible_end,
            rhythm,
            structure,
            tonal,
            vocal,
            energy,
            cues,
            provenance,
        };
        analysis.validate().then_some(analysis)
    }

    pub fn unanalyzed(duration: Duration) -> Self {
        Self {
            duration,
            audible_start: Duration::ZERO,
            audible_end: duration,
            rhythm: RhythmAnalysis::default(),
            structure: StructureAnalysis::default(),
            tonal: TonalAnalysis::default(),
            vocal: VocalAnalysis::default(),
            energy: EnergyAnalysis::default(),
            cues: Vec::new(),
            provenance: AnalysisProvenance::default(),
        }
    }

    pub fn validate(&self) -> bool {
        !self.duration.is_zero()
            && self.audible_start <= self.audible_end
            && self.audible_end <= self.duration
            && self.rhythm.validate()
            && self
                .structure
                .validate_for_beat_count(self.rhythm.beats.len())
                .is_ok()
            && self.tonal.validate()
            && self.vocal.validate()
            && self.energy.validate()
            && self
                .cues
                .iter()
                .all(|cue| cue.validate_for_beat_count(self.rhythm.beats.len()).is_ok())
            && self.provenance.validate()
    }

    pub fn cue_at(&self, beat_index: usize) -> Option<&DjCue> {
        self.cues.iter().find(|cue| cue.beat_index == beat_index)
    }

    pub fn section_index_at(&self, beat_index: usize) -> Option<usize> {
        self.structure
            .sections
            .iter()
            .position(|section| (section.start_beat..section.end_beat).contains(&beat_index))
    }
}

impl Default for TrackAnalysisV2 {
    fn default() -> Self {
        Self::unanalyzed(Duration::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{BeatEvent, ComponentProvenance, Section, SectionLabel};

    #[test]
    fn audible_bounds_must_fit_duration() {
        let mut analysis = TrackAnalysisV2::unanalyzed(Duration::from_secs(10));
        assert!(analysis.validate());
        analysis.audible_end = Duration::from_secs(11);
        assert!(!analysis.validate());
        analysis.audible_end = Duration::from_secs(5);
        analysis.audible_start = Duration::from_secs(6);
        assert!(!analysis.validate());
    }

    #[test]
    fn zero_duration_is_not_a_track_analysis() {
        assert!(!TrackAnalysisV2::unanalyzed(Duration::ZERO).validate());
    }

    #[test]
    fn nested_rhythm_structure_and_provenance_are_validated_together() {
        let mut analysis = TrackAnalysisV2::unanalyzed(Duration::from_secs(10));
        analysis.rhythm.beats = vec![
            BeatEvent::at(Duration::from_secs(1), super::super::Confidence::ONE),
            BeatEvent::at(Duration::from_secs(1), super::super::Confidence::ONE),
        ];
        assert!(
            !analysis.validate(),
            "duplicate beat events must fail V2 validation"
        );

        analysis.rhythm.beats = vec![
            BeatEvent::at(Duration::from_secs(1), super::super::Confidence::ONE),
            BeatEvent::at(Duration::from_secs(2), super::super::Confidence::ONE),
        ];
        analysis.structure = StructureAnalysis::new(
            vec![Section::new(0, 3, 0.8, [SectionLabel::Verse])],
            Vec::new(),
        );
        assert!(
            !analysis.validate(),
            "section indexes must fit the beat grid"
        );

        analysis.structure = StructureAnalysis::default();
        analysis.provenance.rhythm = Some(ComponentProvenance::default());
        assert!(
            !analysis.validate(),
            "named provenance slots must use their slot name"
        );
    }
}
