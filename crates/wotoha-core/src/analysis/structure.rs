//! Lightweight structure and phrase evidence used by AutoMix.
//!
//! This module is deliberately a data/validation layer.  It does not run an
//! ML model or a DSP pass.  In particular, a periodic phrase grid is useful
//! as a search prior, but is kept distinct from an observed boundary so that
//! disagreement with that prior cannot by itself make a transition unsafe.

use serde::{Deserialize, Serialize};

use super::value::{Confidence, UnitInterval};

/// Labels that can be attached to a section of a track.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SectionLabel {
    Intro,
    Verse,
    PreChorus,
    Chorus,
    Build,
    Drop,
    Breakdown,
    Bridge,
    Outro,
    Instrumental,
    #[default]
    Unknown,
}

pub type SectionKind = SectionLabel;
pub type StructureLabel = SectionLabel;

/// A section label with an explicit bounded score.  Legacy label-only inputs
/// are adapted by [`Section::new`] with a score of one; production V2 records
/// should retain the score when one is available.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SectionLabelScore {
    pub label: SectionLabel,
    pub score: UnitInterval,
}

impl SectionLabelScore {
    pub const fn new(label: SectionLabel, score: UnitInterval) -> Self {
        Self { label, score }
    }

    pub const fn legacy(label: SectionLabel) -> Self {
        Self {
            label,
            score: UnitInterval::ONE,
        }
    }

    pub fn from_score(label: SectionLabel, score: f32) -> Self {
        Self::new(label, UnitInterval::clamped(score))
    }

    pub fn validate(&self) -> bool {
        self.score.validate()
    }
}

/// A section in beat-grid coordinates.
///
/// `end_beat` is exclusive.  The confidence is an importance/ranking signal,
/// not a probability; consumers should not use it as a calibrated likelihood.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Section {
    pub start_beat: usize,
    pub end_beat: usize,
    pub boundary_confidence: Confidence,
    pub labels: Vec<SectionLabelScore>,
}

impl Section {
    /// Construct a section from the label-only legacy representation.
    ///
    /// The floating-point confidence is explicitly clamped by this adapter;
    /// callers that already have typed V2 values should use [`Section::scored`].
    /// Call [`Section::validate`] (or [`Section::try_new`]) at an input
    /// boundary.  Keeping the fields public also makes adapters from existing
    /// analysis caches straightforward.
    pub fn new(
        start_beat: usize,
        end_beat: usize,
        boundary_confidence: f32,
        labels: impl IntoIterator<Item = SectionLabel>,
    ) -> Self {
        Self {
            start_beat,
            end_beat,
            boundary_confidence: Confidence::clamped(boundary_confidence),
            labels: labels.into_iter().map(SectionLabelScore::legacy).collect(),
        }
    }

    /// Construct a V2 section with typed scores for both boundaries and labels.
    pub fn scored(
        start_beat: usize,
        end_beat: usize,
        boundary_confidence: Confidence,
        labels: impl IntoIterator<Item = SectionLabelScore>,
    ) -> Self {
        Self {
            start_beat,
            end_beat,
            boundary_confidence,
            labels: labels.into_iter().collect(),
        }
    }

    /// Fallible constructor for callers that prefer validation at creation.
    pub fn try_new(
        start_beat: usize,
        end_beat: usize,
        boundary_confidence: f32,
        labels: impl IntoIterator<Item = SectionLabel>,
    ) -> Result<Self, StructureValidationError> {
        let section = Self::new(start_beat, end_beat, boundary_confidence, labels);
        section.validate()?;
        Ok(section)
    }

    pub fn checked(
        start_beat: usize,
        end_beat: usize,
        boundary_confidence: f32,
        labels: impl IntoIterator<Item = SectionLabel>,
    ) -> Option<Self> {
        Self::try_new(start_beat, end_beat, boundary_confidence, labels).ok()
    }

    /// Validate the section independently of a particular track length.
    pub fn validate(&self) -> Result<(), StructureValidationError> {
        if self.start_beat >= self.end_beat {
            return Err(StructureValidationError::InvalidSectionRange);
        }
        if !self.boundary_confidence.validate() {
            return Err(StructureValidationError::BoundaryConfidenceOutOfRange);
        }
        if self.labels.is_empty() {
            return Err(StructureValidationError::EmptySectionLabels);
        }
        if self.labels.iter().any(|label| !label.validate()) {
            return Err(StructureValidationError::InvalidSectionLabelScore);
        }
        Ok(())
    }

    /// Validate this section against a concrete beat-grid length.
    pub fn validate_for_beat_count(
        &self,
        beat_count: usize,
    ) -> Result<(), StructureValidationError> {
        self.validate()?;
        if self.end_beat > beat_count {
            return Err(StructureValidationError::SectionOutOfBounds {
                start_beat: self.start_beat,
                end_beat: self.end_beat,
                beat_count,
            });
        }
        Ok(())
    }

    /// Whether this section has the supplied label.
    pub fn has_label(&self, label: SectionLabel) -> bool {
        self.labels.iter().any(|scored| scored.label == label)
    }
}

impl Default for Section {
    fn default() -> Self {
        Self {
            start_beat: 0,
            end_beat: 0,
            boundary_confidence: Confidence::ZERO,
            labels: vec![SectionLabelScore::legacy(SectionLabel::Unknown)],
        }
    }
}

/// Provenance for a phrase boundary.
///
/// `PeriodicPrior` is intentionally its own variant.  It represents the
/// existing 4/8/16-bar periodic grid and must never be promoted to a real
/// detection merely because no other evidence is present.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhraseBoundarySource {
    Detected,
    Heuristic,
    /// Default is deliberately prior-only and never invents detection.
    #[default]
    PeriodicPrior,
    Imported,
}

pub type PhraseSource = PhraseBoundarySource;

impl PhraseBoundarySource {
    /// Returns true for evidence that may support an observed boundary.
    pub const fn is_real_detection(self) -> bool {
        !matches!(self, Self::PeriodicPrior)
    }

    /// Alias that reads naturally at call sites deciding whether to score a
    /// mismatch as actionable.
    pub const fn is_actionable(self) -> bool {
        self.is_real_detection()
    }

    pub const fn is_periodic_prior(self) -> bool {
        matches!(self, Self::PeriodicPrior)
    }
}

/// A boundary in beat-grid coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PhraseBoundary {
    pub beat_index: usize,
    /// A bounded ranking strength, not a probability.
    pub strength: UnitInterval,
    pub source: PhraseBoundarySource,
}

/// A small, dependency-free representation of a legacy periodic phrase cue.
///
/// Existing V1 adapters can map their `PhraseCue` values to this type without
/// importing the legacy `automix` module into the V2 domain.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct PeriodicPriorCue {
    pub beat_index: usize,
    pub bars: usize,
}

impl PeriodicPriorCue {
    pub const fn new(beat_index: usize, bars: usize) -> Self {
        Self { beat_index, bars }
    }
}

impl From<(usize, usize)> for PeriodicPriorCue {
    fn from((beat_index, bars): (usize, usize)) -> Self {
        Self::new(beat_index, bars)
    }
}

impl PhraseBoundary {
    pub const fn new(
        beat_index: usize,
        strength: UnitInterval,
        source: PhraseBoundarySource,
    ) -> Self {
        Self {
            beat_index,
            strength,
            source,
        }
    }

    pub const fn detected(beat_index: usize, strength: UnitInterval) -> Self {
        Self::new(beat_index, strength, PhraseBoundarySource::Detected)
    }

    pub const fn heuristic(beat_index: usize, strength: UnitInterval) -> Self {
        Self::new(beat_index, strength, PhraseBoundarySource::Heuristic)
    }

    pub const fn periodic_prior(beat_index: usize, strength: UnitInterval) -> Self {
        Self::new(beat_index, strength, PhraseBoundarySource::PeriodicPrior)
    }

    pub const fn imported(beat_index: usize, strength: UnitInterval) -> Self {
        Self::new(beat_index, strength, PhraseBoundarySource::Imported)
    }

    pub fn detected_legacy(beat_index: usize, strength: f32) -> Self {
        Self::new(
            beat_index,
            UnitInterval::clamped(strength),
            PhraseBoundarySource::Detected,
        )
    }

    pub fn heuristic_legacy(beat_index: usize, strength: f32) -> Self {
        Self::new(
            beat_index,
            UnitInterval::clamped(strength),
            PhraseBoundarySource::Heuristic,
        )
    }

    pub fn periodic_prior_legacy(beat_index: usize, strength: f32) -> Self {
        Self::new(
            beat_index,
            UnitInterval::clamped(strength),
            PhraseBoundarySource::PeriodicPrior,
        )
    }

    pub fn validate(&self) -> Result<(), StructureValidationError> {
        if !self.strength.validate() {
            return Err(StructureValidationError::PhraseStrengthOutOfRange);
        }
        Ok(())
    }

    pub fn validate_for_beat_count(
        &self,
        beat_count: usize,
    ) -> Result<(), StructureValidationError> {
        self.validate()?;
        if self.beat_index >= beat_count {
            return Err(StructureValidationError::PhraseBoundaryOutOfBounds {
                beat_index: self.beat_index,
                beat_count,
            });
        }
        Ok(())
    }

    pub const fn is_real_detection(&self) -> bool {
        self.source.is_real_detection()
    }
}

/// Structure evidence for a track.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StructureAnalysis {
    pub sections: Vec<Section>,
    pub phrase_boundaries: Vec<PhraseBoundary>,
}

impl StructureAnalysis {
    pub fn new(sections: Vec<Section>, phrase_boundaries: Vec<PhraseBoundary>) -> Self {
        Self {
            sections,
            phrase_boundaries,
        }
    }

    pub fn try_new(
        sections: Vec<Section>,
        phrase_boundaries: Vec<PhraseBoundary>,
    ) -> Result<Self, StructureValidationError> {
        let analysis = Self::new(sections, phrase_boundaries);
        analysis.validate()?;
        Ok(analysis)
    }

    pub fn checked(sections: Vec<Section>, phrase_boundaries: Vec<PhraseBoundary>) -> Option<Self> {
        Self::try_new(sections, phrase_boundaries).ok()
    }

    /// Validate ranges, ordering, and score bounds without assuming a track
    /// length.  Use [`Self::validate_for_beat_count`] when indexes can be
    /// checked against a concrete beat grid.
    pub fn validate(&self) -> Result<(), StructureValidationError> {
        let mut previous_start = None;
        let mut previous_end = None;
        for section in &self.sections {
            section.validate()?;
            if let Some(start) = previous_start
                && section.start_beat < start
            {
                return Err(StructureValidationError::SectionsNotOrdered);
            }
            if let Some(end) = previous_end
                && section.start_beat < end
            {
                return Err(StructureValidationError::OverlappingSections);
            }
            previous_start = Some(section.start_beat);
            previous_end = Some(section.end_beat);
        }
        for boundary in &self.phrase_boundaries {
            boundary.validate()?;
        }
        Ok(())
    }

    /// Validate all section and phrase indexes against `beat_count`.
    pub fn validate_for_beat_count(
        &self,
        beat_count: usize,
    ) -> Result<(), StructureValidationError> {
        self.validate()?;
        if beat_count == 0 && (!self.sections.is_empty() || !self.phrase_boundaries.is_empty()) {
            return Err(StructureValidationError::EmptyBeatGrid);
        }
        for section in &self.sections {
            if section.end_beat > beat_count {
                return Err(StructureValidationError::SectionOutOfBounds {
                    start_beat: section.start_beat,
                    end_beat: section.end_beat,
                    beat_count,
                });
            }
        }
        for boundary in &self.phrase_boundaries {
            if boundary.beat_index >= beat_count {
                return Err(StructureValidationError::PhraseBoundaryOutOfBounds {
                    beat_index: boundary.beat_index,
                    beat_count,
                });
            }
        }
        Ok(())
    }

    pub fn has_real_phrase_detection(&self) -> bool {
        self.phrase_boundaries
            .iter()
            .any(PhraseBoundary::is_real_detection)
    }

    pub fn is_periodic_prior_only(&self) -> bool {
        !self.phrase_boundaries.is_empty()
            && self
                .phrase_boundaries
                .iter()
                .all(|boundary| boundary.source == PhraseBoundarySource::PeriodicPrior)
    }

    /// A periodic prior by itself is not enough evidence to hard-block a
    /// phrase mismatch.  Empty evidence is likewise non-blocking.
    pub fn phrase_mismatch_is_hard_block(&self) -> bool {
        self.has_real_phrase_detection()
    }

    /// Return a copy with duplicate boundaries at one beat merged by maximum
    /// strength.  Source provenance is retained whenever all duplicates agree;
    /// mixed evidence is represented by the strongest non-prior source.
    pub fn deduplicated_phrase_boundaries(&self) -> Vec<PhraseBoundary> {
        merge_phrase_boundaries(&self.phrase_boundaries)
    }

    /// Build an analysis containing only the existing periodic 4/8/16-bar
    /// search prior.  It is deliberately not labelled as detected evidence.
    pub fn from_periodic_prior(
        beat_count: usize,
        first_downbeat: usize,
        beats_per_bar: usize,
    ) -> Self {
        Self::new(
            Vec::new(),
            periodic_phrase_prior(beat_count, first_downbeat, beats_per_bar),
        )
    }
}

/// Validation failures for structure and phrase evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StructureValidationError {
    EmptyBeatGrid,
    InvalidSectionRange,
    NonFiniteBoundaryConfidence,
    BoundaryConfidenceOutOfRange,
    EmptySectionLabels,
    InvalidSectionLabelScore,
    SectionsNotOrdered,
    OverlappingSections,
    SectionOutOfBounds {
        start_beat: usize,
        end_beat: usize,
        beat_count: usize,
    },
    NonFinitePhraseStrength,
    PhraseStrengthOutOfRange,
    PhraseBoundaryOutOfBounds {
        beat_index: usize,
        beat_count: usize,
    },
}

/// Generate the existing 4/8/16-bar phrase prior in beat coordinates.
pub fn periodic_phrase_prior(
    beat_count: usize,
    first_downbeat: usize,
    beats_per_bar: usize,
) -> Vec<PhraseBoundary> {
    periodic_phrase_prior_with_lengths(beat_count, first_downbeat, beats_per_bar, &[4, 8, 16])
}

/// Variant used by adapters that already know the desired phrase lengths.
/// Lengths are in bars; zero-length and overflowing periods are ignored.
pub fn periodic_phrase_prior_with_lengths(
    beat_count: usize,
    first_downbeat: usize,
    beats_per_bar: usize,
    lengths_in_bars: &[usize],
) -> Vec<PhraseBoundary> {
    if beat_count == 0 || first_downbeat >= beat_count || beats_per_bar == 0 {
        return Vec::new();
    }
    let mut boundaries = Vec::new();
    for &bars in lengths_in_bars {
        let Some(period) = bars.checked_mul(beats_per_bar) else {
            continue;
        };
        if period == 0 {
            continue;
        }
        let strength = prior_strength(bars);
        let mut beat = first_downbeat;
        while beat < beat_count {
            boundaries.push(PhraseBoundary::periodic_prior(beat, strength));
            let Some(next) = beat.checked_add(period) else {
                break;
            };
            beat = next;
        }
    }
    merge_phrase_boundaries(&boundaries)
}

/// Adapt `(beat_index, bars)` cues produced by an existing periodic grid.
/// The adapter intentionally assigns [`PhraseBoundarySource::PeriodicPrior`].
pub fn adapt_periodic_phrase_prior<I, C>(cues: I) -> Vec<PhraseBoundary>
where
    I: IntoIterator<Item = C>,
    C: Into<PeriodicPriorCue>,
{
    let boundaries = cues
        .into_iter()
        .map(Into::into)
        .map(|cue| PhraseBoundary::periodic_prior(cue.beat_index, prior_strength(cue.bars)));
    merge_phrase_boundaries(&boundaries.collect::<Vec<_>>())
}

/// Alias for callers that use “boundaries” rather than “prior” in their API.
pub fn periodic_phrase_boundaries(
    beat_count: usize,
    first_downbeat: usize,
    beats_per_bar: usize,
) -> Vec<PhraseBoundary> {
    periodic_phrase_prior(beat_count, first_downbeat, beats_per_bar)
}

/// A phrase mismatch can only be hard-blocking when at least one non-periodic
/// evidence source is available.  In particular, periodic-prior-only input
/// returns false.  Missing human/imported cues are unknown, not negative
/// evidence, and are therefore not used to force a block either.
pub fn phrase_mismatch_is_hard_block(boundaries: &[PhraseBoundary]) -> bool {
    boundaries.iter().any(PhraseBoundary::is_real_detection)
}

pub fn is_periodic_prior_only(boundaries: &[PhraseBoundary]) -> bool {
    !boundaries.is_empty()
        && boundaries
            .iter()
            .all(|boundary| boundary.source == PhraseBoundarySource::PeriodicPrior)
}

/// Same policy as [`phrase_mismatch_is_hard_block`], with the comparison made
/// explicit for transition planners.
pub fn phrase_mismatch_requires_observed_evidence(
    expected_beat: usize,
    actual_beat: usize,
    boundaries: &[PhraseBoundary],
) -> bool {
    expected_beat != actual_beat && phrase_mismatch_is_hard_block(boundaries)
}

fn prior_strength(bars: usize) -> UnitInterval {
    UnitInterval::clamped(match bars {
        4 => 0.25,
        8 => 0.35,
        16 => 0.45,
        _ => 0.20,
    })
}

fn merge_phrase_boundaries(boundaries: &[PhraseBoundary]) -> Vec<PhraseBoundary> {
    let mut merged = Vec::new();
    for boundary in boundaries.iter().copied() {
        if !boundary.strength.validate() {
            continue;
        }
        if let Some(existing) = merged
            .iter_mut()
            .find(|existing: &&mut PhraseBoundary| existing.beat_index == boundary.beat_index)
        {
            if boundary.strength > existing.strength {
                existing.strength = boundary.strength;
            }
            if existing.source == PhraseBoundarySource::PeriodicPrior
                && boundary.source != PhraseBoundarySource::PeriodicPrior
            {
                existing.source = boundary.source;
            }
        } else {
            merged.push(boundary);
        }
    }
    merged.sort_by_key(|boundary| boundary.beat_index);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periodic_prior_keeps_source_distinct_from_detected() {
        let prior = periodic_phrase_prior(128, 0, 4);
        assert!(!prior.is_empty());
        assert!(
            prior
                .iter()
                .all(|boundary| boundary.source == PhraseBoundarySource::PeriodicPrior)
        );
        assert!(!prior[0].is_real_detection());
        assert!(PhraseBoundary::detected(0, UnitInterval::clamped(0.8)).is_real_detection());
        assert!(!phrase_mismatch_is_hard_block(&prior));
    }

    #[test]
    fn invalid_sections_and_indexes_are_rejected() {
        let invalid = Section::new(8, 8, 0.5, [SectionLabel::Verse]);
        assert_eq!(
            invalid.validate(),
            Err(StructureValidationError::InvalidSectionRange)
        );

        let analysis = StructureAnalysis::new(
            vec![Section::new(0, 10, 0.5, [SectionLabel::Verse])],
            vec![PhraseBoundary::detected(9, UnitInterval::clamped(0.5))],
        );
        assert!(matches!(
            analysis.validate_for_beat_count(9),
            Err(StructureValidationError::SectionOutOfBounds { .. })
        ));
    }

    #[test]
    fn periodic_prior_mismatch_is_non_blocking_without_observed_evidence() {
        let prior = periodic_phrase_prior(64, 0, 4);
        assert!(!phrase_mismatch_requires_observed_evidence(16, 20, &prior));
        assert!(!phrase_mismatch_is_hard_block(&prior));

        let detected = vec![PhraseBoundary::detected(20, UnitInterval::clamped(0.8))];
        assert!(phrase_mismatch_requires_observed_evidence(
            16, 20, &detected
        ));
        assert!(phrase_mismatch_is_hard_block(&detected));
    }
}
