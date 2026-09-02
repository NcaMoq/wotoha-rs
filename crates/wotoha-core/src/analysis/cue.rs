//! DJ cue candidates and human cue adapters.
//!
//! Cue generation here is intentionally heuristic and bounded.  It consumes
//! beat indexes and already-computed structure evidence; it does not parse
//! Rekordbox XML, load a model, or perform DSP.

use std::{borrow::Borrow, cmp::Ordering, time::Duration};

use serde::{Deserialize, Serialize};

use super::{
    structure::{PhraseBoundary, PhraseBoundarySource, SectionLabel, StructureAnalysis},
    value::UnitInterval,
};

/// Maximum number of candidates retained for a planner search.
pub const MAX_CUE_CANDIDATES: usize = 96;
/// Maximum number of candidates retained for either mix role.
pub const MAX_ROLE_CUES: usize = 8;

const fn score_from_bool(enabled: bool) -> UnitInterval {
    if enabled {
        UnitInterval::ONE
    } else {
        UnitInterval::ZERO
    }
}

/// Origin of a cue candidate.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CueProvenance {
    Detected,
    #[default]
    Heuristic,
    PeriodicPrior,
    Imported(HumanCueSource),
    /// Several candidate sources landed on the same beat.  The individual
    /// sources are still represented by the structure evidence; this variant
    /// prevents a merged cue from masquerading as one source.
    Mixed,
}

impl CueProvenance {
    pub const fn is_human(self) -> bool {
        matches!(self, Self::Imported(_))
    }

    pub const fn is_periodic_prior(self) -> bool {
        matches!(self, Self::PeriodicPrior)
    }
}

/// Role used when selecting a bounded subset of cue candidates.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CueRole {
    #[default]
    MixIn,
    MixOut,
}

/// A beat-indexed DJ cue candidate.
///
/// `importance` is an ordering score for candidate selection.  It is not a
/// probability and should not be interpreted as calibrated confidence.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DjCue {
    pub beat_index: usize,
    pub importance: UnitInterval,
    pub mix_in: UnitInterval,
    pub mix_out: UnitInterval,
    pub cut_safe: UnitInterval,
    pub phrase_boundary: UnitInterval,
    pub drop: UnitInterval,
    pub build: UnitInterval,
    pub provenance: CueProvenance,
}

impl Default for DjCue {
    fn default() -> Self {
        Self::new(0, UnitInterval::ZERO)
    }
}

/// Common acronym spelling for callers that use the name from the product
/// vocabulary.
#[allow(clippy::upper_case_acronyms)]
pub type DJCue = DjCue;
/// Short alias useful in planner code that has no other cue type in scope.
pub type Cue = DjCue;

impl DjCue {
    /// Construct a typed V2 cue with no role or feature evidence yet.
    pub const fn new(beat_index: usize, importance: UnitInterval) -> Self {
        Self::scored(
            beat_index,
            importance,
            UnitInterval::ZERO,
            UnitInterval::ZERO,
            UnitInterval::ZERO,
            UnitInterval::ZERO,
            UnitInterval::ZERO,
            UnitInterval::ZERO,
            CueProvenance::Heuristic,
        )
    }

    /// Explicit legacy constructor for a floating-point importance value.
    pub fn new_legacy(beat_index: usize, importance: f32) -> Self {
        Self::from_legacy_flags(
            beat_index, importance, false, false, false, false, false, false,
        )
    }

    /// Construct a typed V2 cue.  The feature fields are semantic scores,
    /// rather than booleans, so weak evidence can be retained without losing
    /// information at the domain boundary.
    #[allow(clippy::too_many_arguments)]
    pub const fn scored(
        beat_index: usize,
        importance: UnitInterval,
        mix_in: UnitInterval,
        mix_out: UnitInterval,
        cut_safe: UnitInterval,
        phrase_boundary: UnitInterval,
        drop: UnitInterval,
        build: UnitInterval,
        provenance: CueProvenance,
    ) -> Self {
        Self {
            beat_index,
            importance,
            mix_in,
            mix_out,
            cut_safe,
            phrase_boundary,
            drop,
            build,
            provenance,
        }
    }

    /// Explicit legacy adapter for callers that still expose six boolean
    /// flags and a floating-point importance score.
    #[allow(clippy::too_many_arguments)]
    pub fn from_legacy_flags(
        beat_index: usize,
        importance: f32,
        mix_in: bool,
        mix_out: bool,
        cut_safe: bool,
        phrase_boundary: bool,
        drop: bool,
        build: bool,
    ) -> Self {
        Self {
            beat_index,
            importance: UnitInterval::clamped(importance),
            mix_in: score_from_bool(mix_in),
            mix_out: score_from_bool(mix_out),
            cut_safe: score_from_bool(cut_safe),
            phrase_boundary: score_from_bool(phrase_boundary),
            drop: score_from_bool(drop),
            build: score_from_bool(build),
            provenance: CueProvenance::Heuristic,
        }
    }

    pub fn from_bool(beat_index: usize, importance: f32, mix_in: bool, mix_out: bool) -> Self {
        Self::from_legacy_flags(
            beat_index, importance, mix_in, mix_out, false, false, false, false,
        )
    }

    pub const fn from_phrase_boundary(boundary: PhraseBoundary) -> Self {
        let provenance = match boundary.source {
            PhraseBoundarySource::Detected => CueProvenance::Detected,
            PhraseBoundarySource::Heuristic => CueProvenance::Heuristic,
            PhraseBoundarySource::PeriodicPrior => CueProvenance::PeriodicPrior,
            PhraseBoundarySource::Imported => CueProvenance::Heuristic,
        };
        Self {
            beat_index: boundary.beat_index,
            importance: boundary.strength,
            mix_in: UnitInterval::ZERO,
            mix_out: UnitInterval::ZERO,
            cut_safe: UnitInterval::ZERO,
            phrase_boundary: UnitInterval::ONE,
            drop: UnitInterval::ZERO,
            build: UnitInterval::ZERO,
            provenance,
        }
    }

    pub fn validate_for_beat_count(&self, beat_count: usize) -> Result<(), CueValidationError> {
        if self.beat_index >= beat_count {
            return Err(CueValidationError::BeatIndexOutOfBounds {
                beat_index: self.beat_index,
                beat_count,
            });
        }
        if !self.importance.validate()
            || !self.mix_in.validate()
            || !self.mix_out.validate()
            || !self.cut_safe.validate()
            || !self.phrase_boundary.validate()
            || !self.drop.validate()
            || !self.build.validate()
        {
            return Err(CueValidationError::ImportanceOutOfRange);
        }
        Ok(())
    }

    pub fn has_role(self, role: CueRole) -> bool {
        match role {
            CueRole::MixIn => !self.mix_in.is_zero(),
            CueRole::MixOut => !self.mix_out.is_zero(),
        }
    }

    /// Legacy boolean views.  The stored V2 values remain semantic scores.
    pub const fn mix_in_enabled(self) -> bool {
        !self.mix_in.is_zero()
    }

    pub const fn mix_out_enabled(self) -> bool {
        !self.mix_out.is_zero()
    }

    pub const fn cut_safe_enabled(self) -> bool {
        !self.cut_safe.is_zero()
    }

    pub const fn phrase_boundary_enabled(self) -> bool {
        !self.phrase_boundary.is_zero()
    }

    pub const fn drop_enabled(self) -> bool {
        !self.drop.is_zero()
    }

    pub const fn build_enabled(self) -> bool {
        !self.build.is_zero()
    }

    /// Convert this cue to a human cue using the source's beat timestamp.
    pub fn to_human_cue(self, beat_times: &[Duration], source: HumanCueSource) -> Option<HumanCue> {
        human_cue_from_dj_cue(&self, beat_times, source)
    }
}

/// Why a cue is present in the human-editable cue list.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanCueSource {
    RekordboxMemory,
    /// Primary source name for Rekordbox hot cues.
    RekordboxHotCue,
    /// Legacy spelling retained for callers and cache records that used the
    /// shorter source name.
    HotCue,
    #[default]
    ManualWotoha,
}

impl HumanCueSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RekordboxMemory => "rekordbox_memory",
            Self::RekordboxHotCue => "rekordbox_hot_cue",
            Self::HotCue => "hot_cue",
            Self::ManualWotoha => "manual_wotoha",
        }
    }

    pub const fn is_imported(self) -> bool {
        !matches!(self, Self::ManualWotoha)
    }
}

/// Role/type supplied by a human cue source.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanCueKind {
    #[default]
    Unlabeled,
    MixIn,
    MixOut,
    Drop,
    Break,
    Vocal,
    /// Legacy kind retained for imports that used a cut-safe marker.
    CutSafe,
    /// Legacy kind retained for imports that used a phrase marker.
    PhraseBoundary,
    /// Legacy unlabeled spelling.
    Cue,
}

/// Human-authored or imported cue in source-time coordinates.
///
/// Absence of a `HumanCue` means that no human evidence was provided; it is
/// unknown rather than negative evidence and must not be treated as a veto.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct HumanCue {
    pub time: Duration,
    pub source: HumanCueSource,
    pub kind: HumanCueKind,
}

/// Alias retained for adapters that call this field a cue kind.
pub type CueKind = HumanCueKind;

impl HumanCue {
    pub const fn new(time: Duration, source: HumanCueSource, kind: HumanCueKind) -> Self {
        Self { time, source, kind }
    }

    pub fn to_dj_cue(self, beat_times: &[Duration]) -> Option<DjCue> {
        dj_cue_from_human_cue(&self, beat_times)
    }

    pub fn from_dj_cue(cue: &DjCue, beat_times: &[Duration]) -> Option<Self> {
        human_cue_from_dj_cue(cue, beat_times, cue.provenance.human_source()?)
    }

    pub fn validate(self, duration: Option<Duration>) -> Result<(), CueValidationError> {
        if duration.is_some_and(|duration| self.time > duration) {
            return Err(CueValidationError::TimeOutOfBounds { time: self.time });
        }
        Ok(())
    }
}

impl CueProvenance {
    fn human_source(self) -> Option<HumanCueSource> {
        match self {
            Self::Imported(source) => Some(source),
            _ => None,
        }
    }
}

/// Input adapter for heuristic cue generation.  All fields are in beat-grid
/// coordinates, with `audible_end_beat` exclusive.
#[derive(Clone, Debug, Default)]
pub struct CueGenerationInput<'a> {
    pub beat_count: usize,
    pub audible_start_beat: usize,
    pub audible_end_beat: usize,
    pub intro_end_beat: Option<usize>,
    pub outro_start_beat: Option<usize>,
    pub structure: Option<&'a StructureAnalysis>,
}

impl<'a> CueGenerationInput<'a> {
    pub const fn new(
        beat_count: usize,
        audible_start_beat: usize,
        audible_end_beat: usize,
    ) -> Self {
        Self {
            beat_count,
            audible_start_beat,
            audible_end_beat,
            intro_end_beat: None,
            outro_start_beat: None,
            structure: None,
        }
    }

    pub const fn with_intro_end(mut self, beat_index: Option<usize>) -> Self {
        self.intro_end_beat = beat_index;
        self
    }

    pub const fn with_outro_start(mut self, beat_index: Option<usize>) -> Self {
        self.outro_start_beat = beat_index;
        self
    }

    pub const fn with_structure(mut self, structure: &'a StructureAnalysis) -> Self {
        self.structure = Some(structure);
        self
    }
}

/// Errors returned by cue validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CueValidationError {
    BeatIndexOutOfBounds {
        beat_index: usize,
        beat_count: usize,
    },
    NonFiniteImportance,
    ImportanceOutOfRange,
    TimeOutOfBounds {
        time: Duration,
    },
}

/// Generate bounded heuristic candidates around audible and structural edges.
///
/// The generator includes nearby beat indexes for audible start/end, intro and
/// outro hints, section boundaries, and both observed and periodic phrase
/// boundaries.  Periodic prior cues stay tagged as `PeriodicPrior` and remain
/// low-importance search candidates.  Missing human cues do not suppress any
/// generated candidate because absence is not negative evidence.
pub fn generate_heuristic_cues<'a>(input: impl Borrow<CueGenerationInput<'a>>) -> Vec<DjCue> {
    let input = input.borrow();
    if input.beat_count == 0 || input.audible_start_beat >= input.audible_end_beat {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    let audible_end = input.audible_end_beat.saturating_sub(1);
    push_neighborhood(
        &mut candidates,
        input.audible_start_beat,
        input.beat_count,
        |beat_index| {
            let mut cue = DjCue::new(beat_index, UnitInterval::clamped(0.70));
            cue.mix_in = UnitInterval::ONE;
            cue.cut_safe = UnitInterval::ZERO;
            cue
        },
    );
    push_neighborhood(
        &mut candidates,
        audible_end,
        input.beat_count,
        |beat_index| {
            let mut cue = DjCue::new(beat_index, UnitInterval::clamped(0.70));
            cue.mix_out = UnitInterval::ONE;
            cue.cut_safe = UnitInterval::ONE;
            cue
        },
    );

    if let Some(intro_end) = input.intro_end_beat {
        push_neighborhood(&mut candidates, intro_end, input.beat_count, |beat_index| {
            let mut cue = DjCue::new(beat_index, UnitInterval::clamped(0.80));
            cue.mix_in = UnitInterval::ONE;
            cue.cut_safe = UnitInterval::ONE;
            cue
        });
    }
    if let Some(outro_start) = input.outro_start_beat {
        push_neighborhood(
            &mut candidates,
            outro_start,
            input.beat_count,
            |beat_index| {
                let mut cue = DjCue::new(beat_index, UnitInterval::clamped(0.80));
                cue.mix_out = UnitInterval::ONE;
                cue.cut_safe = UnitInterval::ONE;
                cue
            },
        );
    }

    if let Some(structure) = input.structure {
        for section in &structure.sections {
            let labels = section.labels.as_slice();
            let mut cue = DjCue::scored(
                section.start_beat,
                UnitInterval::from(section.boundary_confidence),
                UnitInterval::ONE,
                UnitInterval::ZERO,
                UnitInterval::ZERO,
                UnitInterval::ONE,
                UnitInterval::ZERO,
                UnitInterval::ZERO,
                CueProvenance::Heuristic,
            );
            cue.mix_in = UnitInterval::ONE;
            cue.cut_safe = if labels.iter().any(|scored| {
                matches!(
                    scored.label,
                    SectionLabel::Intro | SectionLabel::Build | SectionLabel::Drop
                )
            }) {
                UnitInterval::ONE
            } else {
                UnitInterval::ZERO
            };
            cue.drop = labels
                .iter()
                .find(|scored| scored.label == SectionLabel::Drop)
                .map_or(UnitInterval::ZERO, |scored| scored.score);
            cue.build = labels
                .iter()
                .find(|scored| scored.label == SectionLabel::Build)
                .map_or(UnitInterval::ZERO, |scored| scored.score);
            candidates.push(cue);

            if section.end_beat > section.start_beat {
                let mut cue = cue;
                cue.beat_index = section.end_beat;
                cue.mix_in = UnitInterval::ZERO;
                cue.mix_out = UnitInterval::ONE;
                cue.cut_safe = UnitInterval::ONE;
                cue.drop = UnitInterval::ZERO;
                cue.build = UnitInterval::ZERO;
                candidates.push(cue);
            }
        }
        for boundary in &structure.phrase_boundaries {
            candidates.push(DjCue::from_phrase_boundary(*boundary));
        }
    }

    let audible_start = input.audible_start_beat.min(input.beat_count);
    let audible_end = input.audible_end_beat.min(input.beat_count);
    let mut bounded =
        bound_cue_candidates_with_limit(candidates, input.beat_count, MAX_CUE_CANDIDATES);
    // Cues around a source boundary are useful only when they remain in the
    // audible beat span.  `bound_cue_candidates` separately enforces the
    // complete beat-grid bound for adapters that do not have this span.
    bounded.retain(|cue| (audible_start..audible_end).contains(&cue.beat_index));
    bounded
}

/// Generate candidates from phrase boundaries without constructing a full
/// structure object.  This is useful for adapters around existing caches.
pub fn generate_heuristic_cues_from_boundaries(
    beat_count: usize,
    audible_start_beat: usize,
    audible_end_beat: usize,
    phrase_boundaries: &[PhraseBoundary],
) -> Vec<DjCue> {
    let structure = StructureAnalysis::new(Vec::new(), phrase_boundaries.to_vec());
    let input = CueGenerationInput::new(beat_count, audible_start_beat, audible_end_beat)
        .with_structure(&structure);
    generate_heuristic_cues(&input)
}

/// Merge duplicate beat candidates, retaining the strongest score for every
/// semantic field.  The output is sorted by beat index.
pub fn merge_cue_candidates(candidates: impl IntoIterator<Item = DjCue>) -> Vec<DjCue> {
    let mut merged = Vec::new();
    for candidate in candidates {
        if !candidate.importance.validate() {
            continue;
        }
        if let Some(existing) = merged
            .iter_mut()
            .find(|existing: &&mut DjCue| existing.beat_index == candidate.beat_index)
        {
            existing.importance = max_score(existing.importance, candidate.importance);
            existing.mix_in = max_score(existing.mix_in, candidate.mix_in);
            existing.mix_out = max_score(existing.mix_out, candidate.mix_out);
            existing.cut_safe = max_score(existing.cut_safe, candidate.cut_safe);
            existing.phrase_boundary =
                max_score(existing.phrase_boundary, candidate.phrase_boundary);
            existing.drop = max_score(existing.drop, candidate.drop);
            existing.build = max_score(existing.build, candidate.build);
            existing.provenance = merge_provenance(existing.provenance, candidate.provenance);
        } else {
            merged.push(candidate);
        }
    }
    merged.sort_by_key(|cue| cue.beat_index);
    merged
}

/// Filter invalid indexes, deduplicate, rank, and retain at most
/// [`MAX_CUE_CANDIDATES`] candidates.
pub fn bound_cue_candidates(
    candidates: impl IntoIterator<Item = DjCue>,
    beat_count: usize,
) -> Vec<DjCue> {
    bound_cue_candidates_with_limit(candidates, beat_count, MAX_CUE_CANDIDATES)
}

/// Configurable form of [`bound_cue_candidates`] for callers with a tighter
/// search budget.
pub fn bound_cue_candidates_with_limit(
    candidates: impl IntoIterator<Item = DjCue>,
    beat_count: usize,
    limit: usize,
) -> Vec<DjCue> {
    if beat_count == 0 || limit == 0 {
        return Vec::new();
    }
    let candidates = merge_cue_candidates(candidates)
        .into_iter()
        .filter(|cue| cue.validate_for_beat_count(beat_count).is_ok());
    let mut bounded: Vec<_> = candidates.collect();
    bounded.sort_by(|left, right| {
        right
            .importance
            .partial_cmp(&left.importance)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.beat_index.cmp(&right.beat_index))
    });
    bounded.truncate(limit);
    bounded
}

/// Return the strongest at most eight candidates for one mix role.
pub fn top_role_cues(candidates: &[DjCue], role: CueRole) -> Vec<DjCue> {
    top_role_cues_with_limit(candidates, role, MAX_ROLE_CUES)
}

/// Configurable form of [`top_role_cues`].
pub fn top_role_cues_with_limit(candidates: &[DjCue], role: CueRole, limit: usize) -> Vec<DjCue> {
    let mut selected: Vec<_> = merge_cue_candidates(candidates.iter().copied())
        .into_iter()
        .filter(|cue| cue.has_role(role) && cue.importance.validate())
        .collect();
    selected.sort_by(|left, right| {
        right
            .importance
            .partial_cmp(&left.importance)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.beat_index.cmp(&right.beat_index))
    });
    selected.truncate(limit);
    selected
}

pub fn top_mix_in_cues(candidates: &[DjCue]) -> Vec<DjCue> {
    top_role_cues(candidates, CueRole::MixIn)
}

pub fn top_mix_out_cues(candidates: &[DjCue]) -> Vec<DjCue> {
    top_role_cues(candidates, CueRole::MixOut)
}

/// Map a human cue to its nearest valid beat-grid index.
pub fn dj_cue_from_human_cue(human: &HumanCue, beat_times: &[Duration]) -> Option<DjCue> {
    let beat_index = nearest_beat_index(human.time, beat_times)?;
    let mut cue = DjCue::new(beat_index, UnitInterval::ONE);
    cue.provenance = CueProvenance::Imported(human.source);
    match human.kind {
        HumanCueKind::Unlabeled | HumanCueKind::Cue => {}
        HumanCueKind::MixIn => cue.mix_in = UnitInterval::ONE,
        HumanCueKind::MixOut => cue.mix_out = UnitInterval::ONE,
        HumanCueKind::CutSafe => cue.cut_safe = UnitInterval::ONE,
        HumanCueKind::PhraseBoundary => cue.phrase_boundary = UnitInterval::ONE,
        HumanCueKind::Drop => {
            cue.drop = UnitInterval::ONE;
            cue.phrase_boundary = UnitInterval::ONE;
        }
        HumanCueKind::Break => cue.phrase_boundary = UnitInterval::ONE,
        HumanCueKind::Vocal => cue.cut_safe = UnitInterval::clamped(0.5),
    }
    Some(cue)
}

/// Convert a beat-indexed cue back to source-time coordinates.
pub fn human_cue_from_dj_cue(
    cue: &DjCue,
    beat_times: &[Duration],
    source: HumanCueSource,
) -> Option<HumanCue> {
    let time = *beat_times.get(cue.beat_index)?;
    let kind = if !cue.mix_in.is_zero() {
        HumanCueKind::MixIn
    } else if !cue.mix_out.is_zero() {
        HumanCueKind::MixOut
    } else if !cue.drop.is_zero() {
        HumanCueKind::Drop
    } else if !cue.build.is_zero() {
        // HumanCueKind deliberately uses the smaller exact vocabulary; a
        // legacy build marker round-trips as a structural break.
        HumanCueKind::Break
    } else if !cue.cut_safe.is_zero() {
        HumanCueKind::CutSafe
    } else if !cue.phrase_boundary.is_zero() {
        HumanCueKind::PhraseBoundary
    } else {
        HumanCueKind::Unlabeled
    };
    Some(HumanCue::new(time, source, kind))
}

/// Alias with the opposite naming order for external adapters.
pub fn human_cue_to_dj_cue(human: &HumanCue, beat_times: &[Duration]) -> Option<DjCue> {
    dj_cue_from_human_cue(human, beat_times)
}

fn nearest_beat_index(time: Duration, beat_times: &[Duration]) -> Option<usize> {
    beat_times
        .iter()
        .enumerate()
        .min_by_key(|(_, beat_time)| abs_duration(**beat_time, time))
        .map(|(index, _)| index)
}

fn abs_duration(left: Duration, right: Duration) -> Duration {
    left.abs_diff(right)
}

fn push_neighborhood(
    candidates: &mut Vec<DjCue>,
    center: usize,
    beat_count: usize,
    mut make: impl FnMut(usize) -> DjCue,
) {
    for beat_index in [center.saturating_sub(1), center, center.saturating_add(1)] {
        if beat_index < beat_count {
            candidates.push(make(beat_index));
        }
    }
}

fn merge_provenance(left: CueProvenance, right: CueProvenance) -> CueProvenance {
    if left == right {
        return left;
    }
    if left.is_human() {
        return left;
    }
    if right.is_human() {
        return right;
    }
    CueProvenance::Mixed
}

fn max_score(left: UnitInterval, right: UnitInterval) -> UnitInterval {
    if right > left { right } else { left }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::structure::{PhraseBoundary, Section, SectionLabel};

    #[test]
    fn generated_cues_are_deduplicated_bounded_and_in_range() {
        let structure = StructureAnalysis::new(
            vec![Section::new(0, 4, 0.8, [SectionLabel::Intro])],
            vec![PhraseBoundary::periodic_prior(
                0,
                UnitInterval::clamped(0.25),
            )],
        );
        let input = CueGenerationInput::new(4, 0, 4)
            .with_intro_end(Some(2))
            .with_structure(&structure);
        let cues = generate_heuristic_cues(&input);
        assert!(cues.len() <= MAX_CUE_CANDIDATES);
        assert!(cues.iter().all(|cue| cue.beat_index < 4));
        assert!(cues.iter().enumerate().all(|(index, cue)| {
            cues[..index]
                .iter()
                .all(|previous| previous.beat_index != cue.beat_index)
        }));
    }

    #[test]
    fn human_cue_roundtrip_uses_nearest_beat_and_source() {
        let beat_times = [Duration::from_secs(1), Duration::from_secs(2)];
        let human = HumanCue::new(
            Duration::from_millis(1_950),
            HumanCueSource::RekordboxHotCue,
            HumanCueKind::MixIn,
        );
        let dj = human.to_dj_cue(&beat_times).expect("beat grid");
        assert_eq!(dj.beat_index, 1);
        let roundtrip = HumanCue::from_dj_cue(&dj, &beat_times).expect("source time");
        assert_eq!(
            roundtrip,
            HumanCue::new(
                Duration::from_secs(2),
                HumanCueSource::RekordboxHotCue,
                HumanCueKind::MixIn,
            )
        );
    }

    #[test]
    fn human_cue_sources_keep_primary_and_legacy_names_distinct() {
        assert_eq!(
            HumanCueSource::RekordboxHotCue.as_str(),
            "rekordbox_hot_cue"
        );
        assert_eq!(HumanCueSource::HotCue.as_str(), "hot_cue");
        assert!(HumanCueSource::RekordboxHotCue.is_imported());
        assert!(HumanCueSource::HotCue.is_imported());
        assert!(!HumanCueSource::ManualWotoha.is_imported());
    }

    #[test]
    fn invalid_indexes_are_dropped_and_role_limit_is_eight() {
        let mut candidates = Vec::new();
        for beat_index in 0..12 {
            let mut cue = DjCue::new(beat_index, UnitInterval::clamped(beat_index as f32 / 12.0));
            cue.mix_in = UnitInterval::ONE;
            candidates.push(cue);
        }
        candidates.push(DjCue::new(99, UnitInterval::ONE));
        let bounded = bound_cue_candidates(candidates, 12);
        assert!(bounded.iter().all(|cue| cue.beat_index < 12));
        assert_eq!(top_mix_in_cues(&bounded).len(), MAX_ROLE_CUES);
    }
}
