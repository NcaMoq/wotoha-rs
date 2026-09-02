//! Analysis schema V2 domain and evidence shared by planners and adapters.

pub mod cue;
pub mod energy;
pub mod legacy;
pub mod provenance;
pub mod rhythm;
pub mod structure;
pub mod tonal;
pub mod track;
pub mod value;
pub mod vocal;

pub use cue::{
    Cue, CueGenerationInput, CueKind, CueProvenance, CueRole, CueValidationError, DJCue, DjCue,
    HumanCue, HumanCueKind, HumanCueSource, MAX_CUE_CANDIDATES, MAX_ROLE_CUES,
    bound_cue_candidates, bound_cue_candidates_with_limit, dj_cue_from_human_cue,
    generate_heuristic_cues, generate_heuristic_cues_from_boundaries, human_cue_from_dj_cue,
    human_cue_to_dj_cue, merge_cue_candidates, top_mix_in_cues, top_mix_out_cues, top_role_cues,
    top_role_cues_with_limit,
};
pub use energy::{EnergyAnalysis, EnergyFrame};
pub use legacy::{
    LegacyAdapter, LegacyAnalysisPlaceholder, LegacyComponentAdapter, LegacyTrackAnalysisAdapter,
};
pub use provenance::{AnalysisMethod, AnalysisProvenance, ComponentProvenance, ModelIdentity};
pub use rhythm::{
    BeatEvent, MAX_TEMPO_BPM, METER_RESOLVE_MIN_MARGIN, METER_RESOLVE_MIN_SCORE, MIN_TEMPO_BPM,
    MeterHypothesis, Relation, RhythmAnalysis, TempoHypothesis, TempoRelation,
};
pub use structure::{
    PeriodicPriorCue, PhraseBoundary, PhraseBoundarySource, PhraseSource, Section, SectionKind,
    SectionLabel, SectionLabelScore, StructureAnalysis, StructureLabel, StructureValidationError,
    adapt_periodic_phrase_prior, is_periodic_prior_only, periodic_phrase_boundaries,
    periodic_phrase_prior, periodic_phrase_prior_with_lengths, phrase_mismatch_is_hard_block,
    phrase_mismatch_requires_observed_evidence,
};
pub use tonal::{GlobalKey, Key, KeyMode, LocalTonalWindow, MusicalKey, TonalAnalysis};
pub use track::TrackAnalysisV2;
pub use value::{Confidence, ModelScore, Support, UnitInterval};
pub use vocal::{VocalAnalysis, VocalFrame};
