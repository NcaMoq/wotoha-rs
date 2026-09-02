//! Adapter seams for the pre-V2 analysis types.
//!
//! This module intentionally does not import `automix`, `beat_analysis`, or
//! any other V1 module.  Concrete adapters can live in an integration layer,
//! avoiding a dependency cycle between the stable domain and legacy runtime.

/// Minimal one-way adapter contract for a legacy value.
pub trait LegacyAdapter<Legacy>: Sized {
    fn from_legacy(value: &Legacy) -> Option<Self>;
}

/// Named contract for complete legacy track records.
pub trait LegacyTrackAnalysisAdapter<Legacy>: Sized {
    fn from_legacy_track(value: &Legacy) -> Option<Self>;
}

/// Named contract for legacy components such as vocal and energy profiles.
pub trait LegacyComponentAdapter<Legacy>: Sized {
    fn from_legacy_component(value: &Legacy) -> Option<Self>;
}

/// Explicit placeholder used by callers that have no V1 representation in
/// scope. It carries no cache data and cannot accidentally become a V1 import.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LegacyAnalysisPlaceholder;

impl<Legacy> LegacyAdapter<Legacy> for LegacyAnalysisPlaceholder {
    fn from_legacy(_: &Legacy) -> Option<Self> {
        Some(Self)
    }
}
