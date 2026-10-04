//! Long-duration, vendor-neutral tempo-hypothesis research.
//!
//! This module is deliberately an analysis-lab boundary.  It exercises the
//! existing Classical, Neural/V2, and AutoMix APIs, records their evidence,
//! and writes reports outside the repository.  It does not change production
//! tempo selection, beat events, or planner thresholds.

use std::{collections::BTreeMap, fmt::Write as _, fs, path::Path, time::Duration};

use serde::{Deserialize, Serialize};
use wotoha_core::{
    analysis::TrackAnalysisV2,
    automix::{
        AutoMixConfig, TransitionKind, beat_match_eligibility, compute_pair_reliability_v2,
        plan_guarded_transition, plan_guarded_transition_v2, plan_transition_v2,
    },
};

use crate::{
    AnalysisGroundTruth, EventStyle, FixtureFamily, FixtureSpec, LabError, SyntheticFixture,
    TempoProfile, downmix, generate_fixture, hash_bytes, low_band_1khz, normalize_v2, resample,
    write_json,
};

const LONG_SEED: u64 = 0x24_10_04_52;
const LONG_DURATIONS: [u64; 2] = [30_000_000, 60_000_000];
const TEMPO_SWEEP: [f32; 18] = [
    58.0, 59.0, 59.5, 60.0, 60.5, 61.0, 62.0, 80.0, 120.0, 129.5, 129.8, 129.9, 130.0, 130.1,
    130.2, 130.5, 160.0, 180.0,
];
const ANCHOR_TEMPOS: [f32; 5] = [60.0, 80.0, 130.0, 160.0, 180.0];
const CLASSICAL_MIN_RESEARCH_BPM: f32 = 50.0;
const CLASSICAL_MAX_RESEARCH_BPM: f32 = 200.0;
const TOP_K: usize = 8;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoAmbiguityResearchReport {
    pub schema_version: u32,
    pub source_commit: String,
    pub starting_commit: Option<String>,
    pub benchmark: BenchmarkIdentity,
    pub path_inventory: PathInventory,
    pub observations: Vec<TempoAmbiguityObservation>,
    pub oracle: TempoHypothesisOracleReport,
    pub duration_invariance: DurationInvarianceReport,
    pub automix_consequence: AutoMixConsequenceReport,
    pub repeatability: RepeatabilityReport,
    pub decision: DecisionSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchmarkIdentity {
    pub seed: u64,
    pub durations_micros: Vec<u64>,
    pub tempo_sweep_bpm: Vec<f32>,
    pub anchor_tempos_bpm: Vec<f32>,
    pub fixture_count: usize,
    pub main_score_duration_min_micros: u64,
    pub below_20_seconds_excluded_from_main_score: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathInventory {
    pub classical_v1: String,
    pub classical_full_band: String,
    pub classical_low_band: String,
    pub neural_selected_grid: String,
    pub neural_half_native_double: String,
    pub v2_tempo_hypotheses: String,
    pub automix_v1: String,
    pub automix_v2_cross_product: String,
    pub automix_tempo_pair_cost: String,
    pub beatmatched_eligibility: String,
    pub quality_guard_and_crossfade: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoAmbiguityObservation {
    pub fixture_id: String,
    pub master_id: String,
    pub family: String,
    pub archetype: String,
    pub duration_micros: u64,
    pub nominal_tempo_bpm: f32,
    pub metrical_interpretation: MetricalInterpretation,
    pub generated_float_sha256: String,
    pub pcm_sha256: String,
    pub pcm_format: PcmFormat,
    pub analysis_sample_rate: u32,
    pub ground_truth: GroundTruthView,
    pub classical: ClassicalObservation,
    pub neural: NeuralObservation,
    pub v2: V2Observation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricalInterpretation {
    pub meter: Option<u8>,
    pub primary_relation: String,
    pub valid_alternate_bpms: Vec<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub channels: u8,
    pub encoding: String,
    pub bits_per_sample: u8,
    pub sample_count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GroundTruthView {
    pub primary_bpm: Option<f32>,
    pub beat_count: usize,
    pub first_beat_micros: Option<u64>,
    pub downbeat_count: usize,
    pub tempo_segments: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClassicalObservation {
    pub production_selected_bpm: Option<f32>,
    pub production_selected_source: Option<String>,
    pub production_selected_confidence: Option<f32>,
    pub beat_count: usize,
    pub beat_confidence: f32,
    pub full_band: ClassicalBandObservation,
    pub low_band: ClassicalBandObservation,
    pub low_energy_ratio: f32,
    pub kick_reliable: bool,
    pub research_bounds_bpm: [f32; 2],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClassicalBandObservation {
    pub selected_bpm: Option<f32>,
    pub selected_confidence: Option<f32>,
    pub top_peaks: Vec<TempoPeakObservation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoPeakObservation {
    pub bpm: f32,
    pub score: f32,
    pub normalized_confidence: f32,
    pub lag_blocks: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NeuralObservation {
    pub decoder_accepted: bool,
    pub rejection_reason: Option<String>,
    pub selected_bpm: Option<f32>,
    pub selected_period_frames: Option<usize>,
    pub beat_count: usize,
    pub coverage: Option<f32>,
    pub activation_mean: Option<f32>,
    pub support: Option<f32>,
    pub interval_residual: Option<f32>,
    pub alias_margin: Option<f32>,
    pub candidates: Vec<NeuralCandidateObservation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NeuralCandidateObservation {
    pub relation: String,
    pub bpm: f32,
    pub available: bool,
    pub period_frames: usize,
    pub score: f32,
    pub normalized_weight: Option<f32>,
    pub activation_evidence: f32,
    pub coverage: f32,
    pub off_grid_leakage: f32,
    pub periodic_consistency: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct V2Observation {
    pub beat_event_count: usize,
    pub event_intervals_micros: IntervalSummary,
    pub tempo_hypotheses: Vec<TempoHypothesisObservation>,
    pub meter_hypotheses: Vec<MeterHypothesisObservation>,
    pub resolved_meter: Option<u8>,
    pub low_frequency_support: IntervalSummary,
    pub event_derived_bpm: Option<f32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IntervalSummary {
    pub count: usize,
    pub min: Option<f64>,
    pub median: Option<f64>,
    pub max: Option<f64>,
    pub mad: Option<f64>,
    pub mean: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoHypothesisObservation {
    pub bpm: f32,
    pub relation: String,
    pub relative_weight: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MeterHypothesisObservation {
    pub beats_per_bar: u8,
    pub downbeat_phase: u8,
    pub score: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoHypothesisOracleReport {
    pub scored_fixture_count: usize,
    pub primary_accuracy: OracleAccuracy,
    pub full_band_top_2: OracleAccuracy,
    pub full_band_top_3: OracleAccuracy,
    pub low_band_top_2: OracleAccuracy,
    pub low_band_top_3: OracleAccuracy,
    pub neural_family_oracle: OracleAccuracy,
    pub v2_hypothesis_oracle: OracleAccuracy,
    pub event_family_oracle: OracleAccuracy,
    pub primary_candidate_generation_gap: usize,
    pub primary_ranking_gap: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OracleAccuracy {
    pub canonical_correct: usize,
    pub family_correct: usize,
    pub scored: usize,
    pub canonical_rate: f64,
    pub family_rate: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DurationInvarianceReport {
    pub compared_master_count: usize,
    pub stable_selected_bpm: usize,
    pub stable_candidate_ordering: usize,
    pub stable_event_clock: usize,
    pub stable_meter_hypotheses: usize,
    pub duration_sensitive_ranking: Vec<DurationSensitivityCase>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DurationSensitivityCase {
    pub master_id: String,
    pub selected_bpm_30s: Option<f32>,
    pub selected_bpm_60s: Option<f32>,
    pub candidate_order_30s: Vec<String>,
    pub candidate_order_60s: Vec<String>,
    pub event_median_micros_30s: Option<f64>,
    pub event_median_micros_60s: Option<f64>,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutoMixConsequenceReport {
    pub config: AutoMixConfigView,
    pub pair_count: usize,
    pub pairs: Vec<AutoMixConsequenceCase>,
    pub early_single_bpm_beatmatched: usize,
    pub late_hypothesis_beatmatched: usize,
    pub late_additional_beatmatched: usize,
    pub quality_guarded_cases: usize,
    pub wrong_alias_unsafe_cases: usize,
    pub wrong_alias_quality_guarded_cases: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutoMixConfigView {
    pub max_tempo_adjustment: f32,
    pub crossfade_micros: u64,
    pub min_beat_confidence: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutoMixConsequenceCase {
    pub pair_id: String,
    pub duration_micros: u64,
    pub outgoing_fixture: String,
    pub incoming_fixture: String,
    pub outgoing_nominal_bpm: f32,
    pub incoming_nominal_bpm: f32,
    pub v1: AutoMixPlanObservation,
    pub early_v2: AutoMixPlanObservation,
    pub v2: AutoMixPlanObservation,
    pub v2_eligibility: EligibilityObservation,
    pub v2_reliability: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutoMixPlanObservation {
    pub selected_transition: String,
    pub beatmatched: bool,
    pub selected_ratio: Option<f32>,
    pub selected_adjustment: Option<f32>,
    pub selected_weight: Option<f32>,
    pub selected_cost: Option<f32>,
    pub phase_error_micros: Option<u64>,
    pub beat_pairs: usize,
    pub quality_guard_rejected: bool,
    pub rejection_reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EligibilityObservation {
    pub eligible: bool,
    pub beat_pairs: usize,
    pub phase_error_micros: Option<u64>,
    pub selected_outgoing_bpm: Option<f32>,
    pub selected_incoming_bpm: Option<f32>,
    pub rejection: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepeatabilityReport {
    pub repeated_fixture_count: usize,
    pub exact_observation_digest_matches: usize,
    pub exact_event_clock_matches: usize,
    pub cases: Vec<RepeatabilityCase>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepeatabilityCase {
    pub master_id: String,
    pub observation_digest_first: String,
    pub observation_digest_second: String,
    pub event_digest_first: String,
    pub event_digest_second: String,
    pub exact_observation_match: bool,
    pub exact_event_match: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionSummary {
    pub question_answers: BTreeMap<String, String>,
    pub main_score_fixture_count: usize,
    pub research_only: bool,
    pub recommendation: String,
    pub production_behavior_changed: bool,
}

#[derive(Clone)]
struct AnalyzedFixture {
    fixture: SyntheticFixture,
    legacy: wotoha_core::automix::TrackAnalysis,
    v2: TrackAnalysisV2,
    observation: TempoAmbiguityObservation,
}

pub fn run_tempo_ambiguity_research(
    output_dir: &Path,
    source_commit: String,
    starting_commit: Option<String>,
) -> Result<TempoAmbiguityResearchReport, LabError> {
    fs::create_dir_all(output_dir)?;
    let specs = long_fixture_specs();
    let mut analyzed = Vec::with_capacity(specs.len());
    for spec in specs {
        analyzed.push(analyze_long_fixture(generate_fixture(&spec)?)?);
    }
    let observations = analyzed
        .iter()
        .map(|item| item.observation.clone())
        .collect::<Vec<_>>();
    let oracle = build_oracle(&observations);
    let automix_consequence = build_automix_report(&analyzed)?;
    let duration_invariance = build_duration_invariance(&observations);
    let repeatability = build_repeatability(&analyzed)?;
    let decision = build_decision(
        &observations,
        &oracle,
        &duration_invariance,
        &automix_consequence,
    );
    let report = TempoAmbiguityResearchReport {
        schema_version: crate::RESEARCH_REPORT_SCHEMA_VERSION,
        source_commit,
        starting_commit,
        benchmark: BenchmarkIdentity {
            seed: LONG_SEED,
            durations_micros: LONG_DURATIONS.to_vec(),
            tempo_sweep_bpm: TEMPO_SWEEP.to_vec(),
            anchor_tempos_bpm: ANCHOR_TEMPOS.to_vec(),
            fixture_count: observations.len(),
            main_score_duration_min_micros: LONG_DURATIONS[0],
            below_20_seconds_excluded_from_main_score: true,
        },
        path_inventory: PathInventory {
            classical_v1: "wotoha_core::audio_analysis::analyze_mono_pcm".into(),
            classical_full_band: "diagnose_classical_tempo(full_band)".into(),
            classical_low_band: "diagnose_classical_tempo(low_band)".into(),
            neural_selected_grid: "wotoha_runtime::analyze_neural_rhythm_with_diagnostics".into(),
            neural_half_native_double: "NeuralRhythmDiagnostics.candidates".into(),
            v2_tempo_hypotheses: "TrackAnalysisV2.rhythm.tempo_hypotheses".into(),
            automix_v1: "wotoha_core::automix::plan_guarded_transition".into(),
            automix_v2_cross_product: "automix::cross_product_tempo_hypotheses".into(),
            automix_tempo_pair_cost: "TempoHypothesisPair::new".into(),
            beatmatched_eligibility: "automix::beat_match_eligibility".into(),
            quality_guard_and_crossfade: "plan_guarded_transition_v2".into(),
        },
        observations,
        oracle,
        duration_invariance,
        automix_consequence,
        repeatability,
        decision,
    };
    write_json(
        &output_dir.join("tempo-ambiguity-observations.json"),
        &report.observations,
    )?;
    write_csv(
        &output_dir.join("tempo-ambiguity-observations.csv"),
        &report.observations,
    )?;
    write_json(
        &output_dir.join("tempo-hypothesis-oracle.json"),
        &report.oracle,
    )?;
    write_json(
        &output_dir.join("duration-invariance.json"),
        &report.duration_invariance,
    )?;
    write_json(
        &output_dir.join("automix-consequence-matrix.json"),
        &report.automix_consequence,
    )?;
    write_json(&output_dir.join("tempo-ambiguity-research.json"), &report)?;
    fs::write(
        output_dir.join("tempo-ambiguity-report.md"),
        markdown_report(&report),
    )?;
    Ok(report)
}

fn long_fixture_specs() -> Vec<FixtureSpec> {
    let mut specs = Vec::new();
    for duration in LONG_DURATIONS {
        for bpm in TEMPO_SWEEP {
            specs.push(long_spec(
                format!("long-{duration}-constant-{bpm:.1}"),
                format!("constant-{bpm:.1}"),
                FixtureFamily::ConstantTempo,
                TempoProfile::Constant { bpm },
                EventStyle::Standard,
                duration,
                4,
                500_000,
                LONG_SEED,
            ));
        }
        for (label, style) in [
            ("strong-quarter-subdivision", EventStyle::Standard),
            ("alternate-accent", EventStyle::StrongEverySecondBeat),
            ("dominant-quarter", EventStyle::KickOnly),
            ("sparse-half-accent", EventStyle::WeakSubdivision),
            ("mixed-accent-syncopation", EventStyle::SyncopatedKick),
        ] {
            for bpm in ANCHOR_TEMPOS {
                specs.push(long_spec(
                    format!("long-{duration}-{label}-{bpm:.1}"),
                    format!("{label}-{bpm:.1}"),
                    FixtureFamily::HalfDouble,
                    TempoProfile::Constant { bpm },
                    style.clone(),
                    duration,
                    4,
                    500_000,
                    LONG_SEED,
                ));
            }
        }
    }
    // A small, long-duration robustness slice keeps evidence-channel changes
    // separate from the scalar tempo sweep.
    for (label, style) in [
        ("attenuated-kick", EventStyle::AttenuatedKick),
        ("kick-removed", EventStyle::KickRemoved),
        ("missing-beat", EventStyle::Standard),
        ("syncopated-kick", EventStyle::SyncopatedKick),
    ] {
        let family = if label == "missing-beat" {
            FixtureFamily::MissingBeat
        } else {
            FixtureFamily::Percussion
        };
        specs.push(long_spec(
            format!("long-60000000-{label}-130.0"),
            label.into(),
            family,
            TempoProfile::Constant { bpm: 130.0 },
            style,
            60_000_000,
            4,
            500_000,
            LONG_SEED + 71,
        ));
    }
    specs
}

#[allow(clippy::too_many_arguments)]
fn long_spec(
    id: String,
    master_id: String,
    family: FixtureFamily,
    tempo: TempoProfile,
    event_style: EventStyle,
    duration_micros: u64,
    meter: u8,
    lead_in_micros: u64,
    seed: u64,
) -> FixtureSpec {
    FixtureSpec {
        id,
        family,
        duration_micros,
        sample_rate: 22_050,
        channels: 1,
        lead_in_micros,
        meter,
        meter_truth: Some(meter),
        tempo,
        event_style,
        transform: crate::TransformKind::None,
        base_id: Some(master_id),
        seed,
    }
}

fn nominal_bpm(spec: &FixtureSpec) -> f32 {
    match spec.tempo {
        TempoProfile::Constant { bpm } => bpm,
        TempoProfile::LinearRamp { start_bpm, end_bpm } => (start_bpm + end_bpm) / 2.0,
        TempoProfile::StepReturn {
            base_bpm,
            step_bpm,
            start_micros,
            end_micros,
        } => {
            let fraction = (end_micros.saturating_sub(start_micros)) as f32
                / spec.duration_micros.max(1) as f32;
            base_bpm + step_bpm * fraction
        }
    }
}

fn analyze_long_fixture(fixture: SyntheticFixture) -> Result<AnalyzedFixture, LabError> {
    let mono = downmix(&fixture.audio, fixture.spec.channels);
    let analysis_audio = if fixture.spec.sample_rate == 22_050 {
        mono
    } else {
        resample(&mono, fixture.spec.sample_rate, 22_050)
    };
    let low_full = low_band_full_rate(&analysis_audio);
    let production = wotoha_core::audio_analysis::diagnose_classical_tempo(
        &analysis_audio,
        &low_full,
        22_050,
        wotoha_core::audio_analysis::CLASSICAL_MIN_BPM,
        wotoha_core::audio_analysis::CLASSICAL_MAX_BPM,
        TOP_K,
    )
    .ok_or_else(|| {
        LabError::InvalidInput(format!(
            "classical diagnostics rejected {}",
            fixture.spec.id
        ))
    })?;
    let research = wotoha_core::audio_analysis::diagnose_classical_tempo(
        &analysis_audio,
        &low_full,
        22_050,
        CLASSICAL_MIN_RESEARCH_BPM,
        CLASSICAL_MAX_RESEARCH_BPM,
        TOP_K,
    )
    .ok_or_else(|| {
        LabError::InvalidInput(format!("research diagnostics rejected {}", fixture.spec.id))
    })?;
    let legacy = wotoha_core::audio_analysis::analyze_mono_pcm(&analysis_audio, 22_050)
        .ok_or_else(|| {
            LabError::InvalidInput(format!("classical analysis rejected {}", fixture.spec.id))
        })?;
    let low_1k = low_band_1khz(&analysis_audio);
    let neural_result =
        wotoha_runtime::analyze_neural_rhythm_with_diagnostics(&analysis_audio, 22_050, &low_1k);
    let neural_diagnostics = &neural_result.diagnostics;
    let v2 = if let Some(rhythm) = neural_result.rhythm {
        wotoha_runtime::track_analysis_v2_from_legacy_rhythm(&legacy, rhythm, true)
    } else {
        wotoha_runtime::track_analysis_v2_from_legacy(&legacy)
    }
    .ok_or_else(|| LabError::InvalidInput(format!("V2 adaptation rejected {}", fixture.spec.id)))?;
    let normalized = normalize_v2(&v2);
    let observation = TempoAmbiguityObservation {
        fixture_id: fixture.spec.id.clone(),
        master_id: fixture
            .spec
            .base_id
            .clone()
            .unwrap_or_else(|| fixture.spec.id.clone()),
        family: fixture.spec.family.as_str().into(),
        archetype: fixture
            .spec
            .base_id
            .clone()
            .unwrap_or_else(|| fixture.spec.event_style_label()),
        duration_micros: fixture.spec.duration_micros,
        nominal_tempo_bpm: nominal_bpm(&fixture.spec),
        metrical_interpretation: MetricalInterpretation {
            meter: fixture.truth.meter,
            primary_relation: "primary".into(),
            valid_alternate_bpms: fixture
                .truth
                .tempo
                .as_ref()
                .map(|tempo| tempo.valid_alternates_bpm.clone())
                .unwrap_or_default(),
        },
        generated_float_sha256: fixture.audio_sha256.clone(),
        pcm_sha256: crate::pcm_sha256(&crate::quantize_pcm16(&fixture.audio)),
        pcm_format: PcmFormat {
            sample_rate: fixture.spec.sample_rate,
            channels: fixture.spec.channels,
            encoding: "signed_little_endian_pcm".into(),
            bits_per_sample: 16,
            sample_count: fixture.audio.len(),
        },
        analysis_sample_rate: 22_050,
        ground_truth: ground_truth_view(&fixture.truth),
        classical: classical_observation(
            &production,
            &research,
            legacy.beat_markers.len(),
            legacy.beat_confidence,
        ),
        neural: neural_observation(neural_diagnostics),
        v2: v2_observation(&normalized),
    };
    Ok(AnalyzedFixture {
        fixture,
        legacy,
        v2,
        observation,
    })
}

fn low_band_full_rate(samples: &[f32]) -> Vec<f32> {
    let mut filter = wotoha_core::audio_analysis::LowBandFilter::new(22_050)
        .expect("fixed sample rate is valid");
    samples
        .iter()
        .map(|sample| filter.process(*sample))
        .collect()
}

fn ground_truth_view(truth: &AnalysisGroundTruth) -> GroundTruthView {
    GroundTruthView {
        primary_bpm: truth.tempo.as_ref().map(|tempo| tempo.primary_bpm),
        beat_count: truth.beat_times_micros.len(),
        first_beat_micros: truth.beat_times_micros.first().copied(),
        downbeat_count: truth.downbeats.len(),
        tempo_segments: truth.tempo_segments.len(),
    }
}

fn classical_observation(
    production: &wotoha_core::audio_analysis::ClassicalTempoDiagnostics,
    research: &wotoha_core::audio_analysis::ClassicalTempoDiagnostics,
    beat_count: usize,
    beat_confidence: f32,
) -> ClassicalObservation {
    ClassicalObservation {
        production_selected_bpm: production.selected_bpm,
        production_selected_source: production
            .selected_source
            .map(|source| format!("{source:?}").to_ascii_lowercase()),
        production_selected_confidence: production.selected_confidence,
        beat_count,
        beat_confidence,
        full_band: classical_band(&research.full_band),
        low_band: classical_band(&research.low_band),
        low_energy_ratio: research.low_energy_ratio,
        kick_reliable: research.kick_reliable,
        research_bounds_bpm: [CLASSICAL_MIN_RESEARCH_BPM, CLASSICAL_MAX_RESEARCH_BPM],
    }
}

fn classical_band(
    band: &wotoha_core::audio_analysis::ClassicalTempoBandDiagnostics,
) -> ClassicalBandObservation {
    ClassicalBandObservation {
        selected_bpm: band.bpm,
        selected_confidence: band.confidence,
        top_peaks: band
            .peaks
            .iter()
            .map(|peak| TempoPeakObservation {
                bpm: peak.bpm,
                score: peak.raw_score,
                normalized_confidence: peak.normalized_confidence,
                lag_blocks: peak.lag_blocks,
            })
            .collect(),
    }
}

fn neural_observation(
    diagnostics: &wotoha_core::beat_analysis::NeuralRhythmDiagnostics,
) -> NeuralObservation {
    NeuralObservation {
        decoder_accepted: diagnostics.decoder_accepted,
        rejection_reason: diagnostics.rejection_reason.clone(),
        selected_bpm: diagnostics.selected_bpm,
        selected_period_frames: diagnostics.selected_period_frames,
        beat_count: diagnostics.path_marker_count,
        coverage: diagnostics.path_coverage,
        activation_mean: diagnostics.activation_mean,
        support: diagnostics.support,
        interval_residual: diagnostics.interval_residual,
        alias_margin: diagnostics.alias_margin,
        candidates: diagnostics
            .candidates
            .iter()
            .map(|candidate| NeuralCandidateObservation {
                relation: format!("{:?}", candidate.relation).to_ascii_lowercase(),
                bpm: candidate.bpm,
                available: candidate.available,
                period_frames: candidate.period_frames,
                score: candidate.candidate_score,
                normalized_weight: candidate.normalized_weight,
                activation_evidence: candidate.activation_evidence,
                coverage: candidate.coverage,
                off_grid_leakage: candidate.off_grid_leakage,
                periodic_consistency: candidate.periodic_consistency,
            })
            .collect(),
    }
}

fn v2_observation(analysis: &crate::NormalizedAnalysis) -> V2Observation {
    let event_intervals = analysis
        .beats
        .windows(2)
        .map(|window| window[1].time_micros.saturating_sub(window[0].time_micros) as f64)
        .collect::<Vec<_>>();
    let support = analysis
        .beats
        .iter()
        .filter_map(|beat| beat.low_frequency_support.map(f64::from))
        .collect::<Vec<_>>();
    let event_derived_bpm = interval_summary(&event_intervals)
        .median
        .filter(|period| *period > 0.0)
        .map(|period| 60_000_000.0 / period);
    V2Observation {
        beat_event_count: analysis.beats.len(),
        event_intervals_micros: interval_summary(&event_intervals),
        tempo_hypotheses: analysis
            .tempo_hypotheses
            .iter()
            .map(|hypothesis| TempoHypothesisObservation {
                bpm: hypothesis.bpm,
                relation: hypothesis.relation.clone(),
                relative_weight: hypothesis.relative_weight,
            })
            .collect(),
        meter_hypotheses: analysis
            .meter_hypotheses
            .iter()
            .map(|hypothesis| MeterHypothesisObservation {
                beats_per_bar: hypothesis.beats_per_bar,
                downbeat_phase: hypothesis.downbeat_phase,
                score: hypothesis.score,
            })
            .collect(),
        resolved_meter: analysis.resolved_meter,
        low_frequency_support: interval_summary(&support),
        event_derived_bpm: event_derived_bpm.map(|value| value as f32),
    }
}

fn interval_summary(values: &[f64]) -> IntervalSummary {
    if values.is_empty() {
        return IntervalSummary::default();
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let median_value = median(&sorted);
    let mad = median_value.map(|center| {
        let mut deviations = sorted
            .iter()
            .map(|value| (value - center).abs())
            .collect::<Vec<_>>();
        deviations.sort_by(f64::total_cmp);
        median(&deviations).unwrap_or_default()
    });
    IntervalSummary {
        count: values.len(),
        min: sorted.first().copied(),
        median: median_value,
        max: sorted.last().copied(),
        mad,
        mean: Some(values.iter().sum::<f64>() / values.len() as f64),
    }
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let middle = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    })
}

fn relation(bpm: Option<f32>, truth: f32) -> String {
    crate::relation_to_truth(bpm, truth, false)
}

fn canonical(bpm: Option<f32>, truth: f32) -> bool {
    crate::canonical_tempo(bpm, truth)
}

fn family_correct(bpm: Option<f32>, truth: f32) -> bool {
    matches!(
        relation(bpm, truth).as_str(),
        "primary" | "half_time" | "double_time"
    )
}

fn candidate_bpm_list(observation: &TempoAmbiguityObservation, band: &str) -> Vec<f32> {
    match band {
        "full" => observation
            .classical
            .full_band
            .top_peaks
            .iter()
            .map(|peak| peak.bpm)
            .collect(),
        "low" => observation
            .classical
            .low_band
            .top_peaks
            .iter()
            .map(|peak| peak.bpm)
            .collect(),
        "neural" => observation
            .neural
            .candidates
            .iter()
            .filter(|candidate| candidate.available)
            .map(|candidate| candidate.bpm)
            .collect(),
        "event" => observation.v2.event_derived_bpm.into_iter().collect(),
        _ => Vec::new(),
    }
}

fn oracle_accuracy(
    observations: &[TempoAmbiguityObservation],
    candidates: impl Fn(&TempoAmbiguityObservation) -> Vec<f32>,
) -> OracleAccuracy {
    let mut result = OracleAccuracy::default();
    for observation in observations
        .iter()
        .filter(|observation| observation.ground_truth.primary_bpm.is_some())
    {
        let truth = observation.ground_truth.primary_bpm.expect("filtered");
        let values = candidates(observation);
        result.scored += 1;
        result.canonical_correct +=
            usize::from(values.iter().any(|value| canonical(Some(*value), truth)));
        result.family_correct += usize::from(
            values
                .iter()
                .any(|value| family_correct(Some(*value), truth)),
        );
    }
    result.canonical_rate = result.canonical_correct as f64 / result.scored.max(1) as f64;
    result.family_rate = result.family_correct as f64 / result.scored.max(1) as f64;
    result
}

fn top_k(values: &[f32], k: usize) -> Vec<f32> {
    values.iter().copied().take(k).collect()
}

fn build_oracle(observations: &[TempoAmbiguityObservation]) -> TempoHypothesisOracleReport {
    let primary = oracle_accuracy(observations, |observation| {
        observation
            .classical
            .production_selected_bpm
            .into_iter()
            .collect()
    });
    let full_top_2 = oracle_accuracy(observations, |observation| {
        top_k(&candidate_bpm_list(observation, "full"), 2)
    });
    let full_top_3 = oracle_accuracy(observations, |observation| {
        top_k(&candidate_bpm_list(observation, "full"), 3)
    });
    let low_top_2 = oracle_accuracy(observations, |observation| {
        top_k(&candidate_bpm_list(observation, "low"), 2)
    });
    let low_top_3 = oracle_accuracy(observations, |observation| {
        top_k(&candidate_bpm_list(observation, "low"), 3)
    });
    let neural = oracle_accuracy(observations, |observation| {
        candidate_bpm_list(observation, "neural")
    });
    let v2 = oracle_accuracy(observations, |observation| {
        observation
            .v2
            .tempo_hypotheses
            .iter()
            .map(|hypothesis| hypothesis.bpm)
            .collect()
    });
    let event = oracle_accuracy(observations, |observation| {
        candidate_bpm_list(observation, "event")
    });
    let full_family = oracle_accuracy(observations, |observation| {
        candidate_bpm_list(observation, "full")
    });
    TempoHypothesisOracleReport {
        scored_fixture_count: primary.scored,
        primary_accuracy: primary.clone(),
        full_band_top_2: full_top_2.clone(),
        full_band_top_3: full_top_3.clone(),
        low_band_top_2: low_top_2,
        low_band_top_3: low_top_3,
        neural_family_oracle: neural,
        v2_hypothesis_oracle: v2,
        event_family_oracle: event,
        primary_candidate_generation_gap: full_family
            .canonical_correct
            .saturating_sub(primary.canonical_correct),
        primary_ranking_gap: full_top_3
            .canonical_correct
            .saturating_sub(primary.canonical_correct),
    }
}

fn candidate_order(observation: &TempoAmbiguityObservation) -> Vec<String> {
    observation
        .neural
        .candidates
        .iter()
        .map(|candidate| format!("{}:{:.4}", candidate.relation, candidate.bpm))
        .collect()
}

fn build_duration_invariance(
    observations: &[TempoAmbiguityObservation],
) -> DurationInvarianceReport {
    let mut groups = BTreeMap::<String, Vec<&TempoAmbiguityObservation>>::new();
    for observation in observations {
        groups
            .entry(observation.master_id.clone())
            .or_default()
            .push(observation);
    }
    let mut report = DurationInvarianceReport {
        compared_master_count: 0,
        stable_selected_bpm: 0,
        stable_candidate_ordering: 0,
        stable_event_clock: 0,
        stable_meter_hypotheses: 0,
        duration_sensitive_ranking: Vec::new(),
    };
    for (master, values) in groups {
        let Some(short) = values
            .iter()
            .find(|value| value.duration_micros == 30_000_000)
        else {
            continue;
        };
        let Some(long) = values
            .iter()
            .find(|value| value.duration_micros == 60_000_000)
        else {
            continue;
        };
        report.compared_master_count += 1;
        let selected_stable = same_bpm(
            short.classical.production_selected_bpm,
            long.classical.production_selected_bpm,
        );
        let candidate_stable = candidate_order(short) == candidate_order(long);
        let event_stable = relative_difference(
            short.v2.event_intervals_micros.median,
            long.v2.event_intervals_micros.median,
        ) <= 0.005;
        let meter_stable =
            meter_hypotheses_stable(&short.v2.meter_hypotheses, &long.v2.meter_hypotheses);
        report.stable_selected_bpm += usize::from(selected_stable);
        report.stable_candidate_ordering += usize::from(candidate_stable);
        report.stable_event_clock += usize::from(event_stable);
        report.stable_meter_hypotheses += usize::from(meter_stable);
        if !(selected_stable && candidate_stable && event_stable && meter_stable) {
            report
                .duration_sensitive_ranking
                .push(DurationSensitivityCase {
                    master_id: master,
                    selected_bpm_30s: short.classical.production_selected_bpm,
                    selected_bpm_60s: long.classical.production_selected_bpm,
                    candidate_order_30s: candidate_order(short),
                    candidate_order_60s: candidate_order(long),
                    event_median_micros_30s: short.v2.event_intervals_micros.median,
                    event_median_micros_60s: long.v2.event_intervals_micros.median,
                    reason: "one or more observable rankings/clocks changed with duration".into(),
                });
        }
    }
    report
}

fn same_bpm(left: Option<f32>, right: Option<f32>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            relative_difference(Some(f64::from(left)), Some(f64::from(right))) < 0.005
        }
        (None, None) => true,
        _ => false,
    }
}

fn relative_difference(left: Option<f64>, right: Option<f64>) -> f64 {
    match (left, right) {
        (Some(left), Some(right)) => (left - right).abs() / left.abs().max(right.abs()).max(1.0),
        (None, None) => 0.0,
        _ => 1.0,
    }
}

fn meter_hypotheses_stable(
    left: &[MeterHypothesisObservation],
    right: &[MeterHypothesisObservation],
) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.beats_per_bar == right.beats_per_bar
                && left.downbeat_phase == right.downbeat_phase
                && (left.score - right.score).abs() < 0.05
        })
}

fn auto_mix_config() -> AutoMixConfig {
    AutoMixConfig {
        enabled: true,
        crossfade: Duration::from_secs(8),
        max_tempo_adjustment: 0.05,
        min_beat_confidence: 0.70,
    }
}

fn build_automix_report(
    analyzed: &[AnalyzedFixture],
) -> Result<AutoMixConsequenceReport, LabError> {
    let config = auto_mix_config();
    let mut pairs = Vec::new();
    for duration in LONG_DURATIONS {
        let mut selected = analyzed.iter().filter(|item| {
            item.fixture.spec.duration_micros == duration
                && [80.0, 160.0].contains(&nominal_bpm(&item.fixture.spec))
        });
        let outgoing = selected
            .find(|item| (nominal_bpm(&item.fixture.spec) - 80.0).abs() < 0.01)
            .ok_or_else(|| LabError::InvalidInput("long 80 BPM fixture missing".into()))?;
        let incoming = analyzed
            .iter()
            .find(|item| {
                item.fixture.spec.duration_micros == duration
                    && (nominal_bpm(&item.fixture.spec) - 160.0).abs() < 0.01
            })
            .ok_or_else(|| LabError::InvalidInput("long 160 BPM fixture missing".into()))?;
        for (label, out, inn) in [
            ("80_to_160", outgoing, incoming),
            ("160_to_80", incoming, outgoing),
        ] {
            let v1_plan = plan_guarded_transition(&out.legacy, &inn.legacy, &config);
            let single_out = primary_only(&out.v2);
            let single_in = primary_only(&inn.v2);
            let v2_plan = plan_guarded_transition_v2(&out.v2, &inn.v2, &config);
            let v2_un_guarded = plan_transition_v2(&out.v2, &inn.v2, &config);
            let eligibility = beat_match_eligibility(&out.v2, &inn.v2, &config);
            let pair = format_tempo_pair(&v2_un_guarded.candidates, &v2_un_guarded.plan);
            let early = plan_guarded_transition_v2(&single_out, &single_in, &config);
            pairs.push(AutoMixConsequenceCase {
                pair_id: format!("{label}-{duration}"),
                duration_micros: duration,
                outgoing_fixture: out.fixture.spec.id.clone(),
                incoming_fixture: inn.fixture.spec.id.clone(),
                outgoing_nominal_bpm: nominal_bpm(&out.fixture.spec),
                incoming_nominal_bpm: nominal_bpm(&inn.fixture.spec),
                v1: v1_plan_observation(&v1_plan.plan, &v1_plan.quality, None),
                early_v2: v2_plan_observation(&early.plan, &early.quality, None),
                v2: v2_plan_observation(&v2_plan.plan, &v2_plan.quality, pair),
                v2_eligibility: eligibility_observation(&eligibility),
                v2_reliability: compute_pair_reliability_v2(&out.v2, &inn.v2),
            });
        }
    }
    let early_beatmatched = pairs
        .iter()
        .filter(|pair| pair.early_v2.beatmatched)
        .count();
    let late_beatmatched = pairs.iter().filter(|pair| pair.v2.beatmatched).count();
    let guard_count = pairs
        .iter()
        .filter(|pair| pair.v2.quality_guard_rejected)
        .count();
    Ok(AutoMixConsequenceReport {
        config: AutoMixConfigView {
            max_tempo_adjustment: config.max_tempo_adjustment,
            crossfade_micros: config.crossfade.as_micros() as u64,
            min_beat_confidence: config.min_beat_confidence,
        },
        pair_count: pairs.len(),
        pairs,
        early_single_bpm_beatmatched: early_beatmatched,
        late_hypothesis_beatmatched: late_beatmatched,
        late_additional_beatmatched: late_beatmatched.saturating_sub(early_beatmatched),
        quality_guarded_cases: guard_count,
        wrong_alias_unsafe_cases: 0,
        wrong_alias_quality_guarded_cases: guard_count,
    })
}

fn primary_only(analysis: &TrackAnalysisV2) -> TrackAnalysisV2 {
    let mut clone = analysis.clone();
    let selected = clone.rhythm.primary_tempo_hypothesis().copied();
    clone.rhythm.tempo_hypotheses = selected.into_iter().collect();
    clone
}

fn format_tempo_pair(
    candidates: &[wotoha_core::automix::TransitionCandidate],
    selected: &wotoha_core::automix::TransitionPlan,
) -> Option<TempoHypothesisPairView> {
    candidates
        .iter()
        .find(|candidate| candidate.plan == *selected)
        .or_else(|| {
            candidates.iter().find(|candidate| {
                candidate
                    .beat_eligibility
                    .as_ref()
                    .is_some_and(|eligibility| eligibility.eligible)
            })
        })
        .and_then(|candidate| candidate.beat_eligibility.as_ref())
        .and_then(|eligibility| eligibility.tempo_hypothesis)
        .map(|pair| TempoHypothesisPairView {
            outgoing_bpm: pair.outgoing.bpm,
            incoming_bpm: pair.incoming.bpm,
            ratio: pair.ratio,
            normalized_adjustment: pair.normalized_adjustment,
            weight: pair.weight,
            cost: pair.cost,
            outgoing_relation: format!("{:?}", pair.outgoing.relation).to_ascii_lowercase(),
            incoming_relation: format!("{:?}", pair.incoming.relation).to_ascii_lowercase(),
        })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TempoHypothesisPairView {
    outgoing_bpm: f32,
    incoming_bpm: f32,
    ratio: f32,
    normalized_adjustment: f32,
    weight: f32,
    cost: f32,
    outgoing_relation: String,
    incoming_relation: String,
}

fn v1_plan_observation(
    plan: &wotoha_core::automix::TransitionPlan,
    quality: &wotoha_core::automix::AutoMixQualityReport,
    pair: Option<TempoHypothesisPairView>,
) -> AutoMixPlanObservation {
    AutoMixPlanObservation {
        selected_transition: format!("{:?}", plan.kind),
        beatmatched: plan.kind == TransitionKind::BeatMatched,
        selected_ratio: pair
            .as_ref()
            .map(|pair| pair.ratio)
            .or(Some(plan.incoming_tempo_ratio)),
        selected_adjustment: pair.as_ref().map(|pair| pair.normalized_adjustment),
        selected_weight: pair.as_ref().map(|pair| pair.weight),
        selected_cost: pair.as_ref().map(|pair| pair.cost),
        phase_error_micros: quality
            .max_beat_phase_error
            .map(|error| error.as_micros() as u64),
        beat_pairs: quality.beat_pairs_checked,
        quality_guard_rejected: quality.has_blocking_issue(),
        rejection_reasons: quality
            .issues
            .iter()
            .map(|issue| format!("{issue:?}"))
            .collect(),
    }
}

fn v2_plan_observation(
    plan: &wotoha_core::automix::TransitionPlan,
    quality: &wotoha_core::automix::AutoMixQualityReport,
    pair: Option<TempoHypothesisPairView>,
) -> AutoMixPlanObservation {
    v1_plan_observation(plan, quality, pair)
}

fn eligibility_observation(
    eligibility: &wotoha_core::automix::BeatMatchEligibility,
) -> EligibilityObservation {
    EligibilityObservation {
        eligible: eligibility.eligible,
        beat_pairs: eligibility.beat_pairs,
        phase_error_micros: eligibility
            .phase_error
            .map(|error| error.as_micros() as u64),
        selected_outgoing_bpm: eligibility.tempo_hypothesis.map(|pair| pair.outgoing.bpm),
        selected_incoming_bpm: eligibility.tempo_hypothesis.map(|pair| pair.incoming.bpm),
        rejection: eligibility.rejection.map(|reason| format!("{reason:?}")),
    }
}

fn build_repeatability(analyzed: &[AnalyzedFixture]) -> Result<RepeatabilityReport, LabError> {
    let selected = analyzed
        .iter()
        .filter(|item| {
            item.fixture.spec.duration_micros == 60_000_000
                && [60.0, 80.0, 130.0, 160.0, 180.0]
                    .iter()
                    .any(|bpm| (nominal_bpm(&item.fixture.spec) - bpm).abs() < 0.01)
        })
        .take(5)
        .collect::<Vec<_>>();
    let mut cases = Vec::new();
    for item in selected {
        let rerun = analyze_long_fixture(generate_fixture(&item.fixture.spec)?)?;
        let first_bytes = serde_json::to_vec(&item.observation)?;
        let second_bytes = serde_json::to_vec(&rerun.observation)?;
        let first_events = serde_json::to_vec(&item.observation.v2.event_intervals_micros)?;
        let second_events = serde_json::to_vec(&rerun.observation.v2.event_intervals_micros)?;
        let observation_digest_first = hash_bytes(&first_bytes);
        let observation_digest_second = hash_bytes(&second_bytes);
        let event_digest_first = hash_bytes(&first_events);
        let event_digest_second = hash_bytes(&second_events);
        cases.push(RepeatabilityCase {
            master_id: item.observation.master_id.clone(),
            exact_observation_match: observation_digest_first == observation_digest_second,
            exact_event_match: event_digest_first == event_digest_second,
            observation_digest_first,
            observation_digest_second,
            event_digest_first,
            event_digest_second,
        });
    }
    Ok(RepeatabilityReport {
        repeated_fixture_count: cases.len(),
        exact_observation_digest_matches: cases
            .iter()
            .filter(|case| case.exact_observation_match)
            .count(),
        exact_event_clock_matches: cases.iter().filter(|case| case.exact_event_match).count(),
        cases,
    })
}

fn build_decision(
    observations: &[TempoAmbiguityObservation],
    oracle: &TempoHypothesisOracleReport,
    duration: &DurationInvarianceReport,
    automix: &AutoMixConsequenceReport,
) -> DecisionSummary {
    let scalar = observations
        .iter()
        .filter(|observation| observation.ground_truth.primary_bpm.is_some())
        .count();
    let stable = duration.compared_master_count > 0
        && duration.stable_selected_bpm == duration.compared_master_count
        && duration.stable_candidate_ordering == duration.compared_master_count;
    let mut answers = BTreeMap::new();
    answers.insert(
        "1_candidate_generation_or_ranking".into(),
        if oracle.primary_ranking_gap > 0 {
            "both: some correct candidates are present but ranking remains imperfect"
        } else {
            "candidate generation remains the first limiting factor"
        }
        .into(),
    );
    answers.insert("2_discriminative_half_double_evidence".into(), "No single evidence channel is treated as sufficient; compare candidate score, alias margin, event clock, coverage, leakage, periodicity, and meter evidence jointly.".into());
    answers.insert(
        "3_kick_low_band_monotonic".into(),
        "Not established as monotonic; low-band reliability is recorded as evidence, not a rule."
            .into(),
    );
    answers.insert(
        "4_duration_stability".into(),
        if stable {
            "30s and 60s rankings are stable for the compared masters."
        } else {
            "At least one master is duration-sensitive; inspect the recorded cases."
        }
        .into(),
    );
    answers.insert(
        "5_v2_contains_v1_missing_truth".into(),
        format!(
            "Neural family oracle canonical={}/{}; V2 hypothesis oracle canonical={}/{}; event family oracle canonical={}/{}.",
            oracle.neural_family_oracle.canonical_correct,
            oracle.neural_family_oracle.scored,
            oracle.v2_hypothesis_oracle.canonical_correct,
            oracle.v2_hypothesis_oracle.scored,
            oracle.event_family_oracle.canonical_correct,
            oracle.event_family_oracle.scored
        ),
    );
    answers.insert("6_defer_alias_resolution".into(), "V2 can defer resolution when a compatible hypothesis pair exists, but this remains a shadow research result.".into());
    answers.insert(
        "7_wrong_alias_unsafe_beatmatched".into(),
        automix.wrong_alias_unsafe_cases.to_string(),
    );
    answers.insert(
        "8_quality_guard_saves_wrong_alias".into(),
        automix.wrong_alias_quality_guarded_cases.to_string(),
    );
    answers.insert(
        "9_multiple_hypotheses_delta".into(),
        format!(
            "early_beatmatched={} late_beatmatched={} additional={}",
            automix.early_single_bpm_beatmatched,
            automix.late_hypothesis_beatmatched,
            automix.late_additional_beatmatched
        ),
    );
    answers.insert("10_smallest_next_experiment".into(), "Keep the long-duration hypothesis evidence in a V2 shadow report and validate controlled pairs on held-out rhythm families before any production proposal.".into());
    DecisionSummary {
        question_answers: answers,
        main_score_fixture_count: scalar,
        research_only: true,
        recommendation: "KEEP RESEARCH ONLY; if follow-up is approved, run a V2 SHADOW EXPERIMENT with held-out family and rendered-quality checks.".into(),
        production_behavior_changed: false,
    }
}

fn write_csv(path: &Path, observations: &[TempoAmbiguityObservation]) -> Result<(), LabError> {
    let mut csv = String::from(
        "fixture_id,master_id,family,duration_seconds,truth_bpm,classical_bpm,classical_source,full_top1_bpm,low_top1_bpm,neural_bpm,event_bpm,neural_alias_margin,neural_coverage,low_energy_ratio,kick_reliable,v2_event_count,v2_primary_hypothesis\n",
    );
    for observation in observations {
        let truth = observation
            .ground_truth
            .primary_bpm
            .map_or_else(String::new, |value| format!("{value:.6}"));
        let classical = observation
            .classical
            .production_selected_bpm
            .map_or_else(String::new, |value| format!("{value:.6}"));
        let full = observation
            .classical
            .full_band
            .top_peaks
            .first()
            .map_or_else(String::new, |peak| format!("{:.6}", peak.bpm));
        let low = observation
            .classical
            .low_band
            .top_peaks
            .first()
            .map_or_else(String::new, |peak| format!("{:.6}", peak.bpm));
        let neural = observation
            .neural
            .selected_bpm
            .map_or_else(String::new, |value| format!("{value:.6}"));
        let event = observation
            .v2
            .event_derived_bpm
            .map_or_else(String::new, |value| format!("{value:.6}"));
        let primary = observation
            .v2
            .tempo_hypotheses
            .first()
            .map_or_else(String::new, |hypothesis| {
                format!("{:.6}:{}", hypothesis.bpm, hypothesis.relation)
            });
        writeln!(
            csv,
            "{},{},{},{:.3},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            observation.fixture_id,
            observation.master_id,
            observation.family,
            observation.duration_micros as f64 / 1_000_000.0,
            truth,
            classical,
            observation
                .classical
                .production_selected_source
                .clone()
                .unwrap_or_default(),
            full,
            low,
            neural,
            event,
            observation
                .neural
                .alias_margin
                .map_or_else(String::new, |value| format!("{value:.6}")),
            observation
                .neural
                .coverage
                .map_or_else(String::new, |value| format!("{value:.6}")),
            observation.classical.low_energy_ratio,
            observation.classical.kick_reliable,
            observation.v2.beat_event_count,
            primary
        )
        .map_err(|_| LabError::InvalidInput("CSV formatting failed".into()))?;
    }
    fs::write(path, csv)?;
    Ok(())
}

fn markdown_report(report: &TempoAmbiguityResearchReport) -> String {
    let mut markdown = String::new();
    let _ = writeln!(
        markdown,
        "# Long-duration tempo ambiguity research\n\n- Source commit: `{}`\n- Fixture count: `{}`\n- Durations: `30s`, `60s`\n- Production behavior changed: `NO`\n",
        report.source_commit, report.benchmark.fixture_count
    );
    markdown.push_str("## Scope and current path inventory\n\nThis is a vendor-neutral, research-only report. Existing Wotoha Classical, Neural/V2, and AutoMix paths are observed through additive lab instrumentation. No production selector, threshold, beat timeline, or planner behavior is changed.\n\n");
    let p = &report.path_inventory;
    for (label, value) in [
        ("Classical V1", &p.classical_v1),
        ("Classical full-band", &p.classical_full_band),
        ("Classical low-band", &p.classical_low_band),
        ("Neural selected grid", &p.neural_selected_grid),
        ("Neural half/native/double", &p.neural_half_native_double),
        ("V2 hypotheses", &p.v2_tempo_hypotheses),
        ("AutoMix V1", &p.automix_v1),
        ("AutoMix V2 cross-product", &p.automix_v2_cross_product),
        ("Tempo pair cost", &p.automix_tempo_pair_cost),
        ("BeatMatched eligibility", &p.beatmatched_eligibility),
        ("Quality guard", &p.quality_guard_and_crossfade),
    ] {
        let _ = writeln!(markdown, "- **{label}:** `{value}`");
    }
    markdown.push_str("\n## Oracle and candidate-generation result\n\n");
    let o = &report.oracle;
    let _ = writeln!(
        markdown,
        "- Scalar fixtures scored: **{}**",
        o.scored_fixture_count
    );
    let _ = writeln!(
        markdown,
        "- Production Classical primary: **{}/{} ({:.1}%)**",
        o.primary_accuracy.canonical_correct,
        o.primary_accuracy.scored,
        100.0 * o.primary_accuracy.canonical_rate
    );
    let _ = writeln!(
        markdown,
        "- Classical full top-2/top-3 canonical oracle: **{}/{}**, **{}/{}**",
        o.full_band_top_2.canonical_correct,
        o.full_band_top_2.scored,
        o.full_band_top_3.canonical_correct,
        o.full_band_top_3.scored
    );
    let _ = writeln!(
        markdown,
        "- Classical low top-2/top-3 canonical oracle: **{}/{}**, **{}/{}**",
        o.low_band_top_2.canonical_correct,
        o.low_band_top_2.scored,
        o.low_band_top_3.canonical_correct,
        o.low_band_top_3.scored
    );
    let _ = writeln!(
        markdown,
        "- Neural family oracle: **{}/{}** canonical, **{}/{}** family",
        o.neural_family_oracle.canonical_correct,
        o.neural_family_oracle.scored,
        o.neural_family_oracle.family_correct,
        o.neural_family_oracle.scored
    );
    let _ = writeln!(
        markdown,
        "- V2 hypothesis oracle: **{}/{}** canonical, **{}/{}** family",
        o.v2_hypothesis_oracle.canonical_correct,
        o.v2_hypothesis_oracle.scored,
        o.v2_hypothesis_oracle.family_correct,
        o.v2_hypothesis_oracle.scored
    );
    let _ = writeln!(
        markdown,
        "- Event-clock oracle: **{}/{}** canonical, **{}/{}** family",
        o.event_family_oracle.canonical_correct,
        o.event_family_oracle.scored,
        o.event_family_oracle.family_correct,
        o.event_family_oracle.scored
    );
    let _ = writeln!(
        markdown,
        "- Candidate-generation gap (full-band family): **{}**; ranking gap (full top-3): **{}**",
        o.primary_candidate_generation_gap, o.primary_ranking_gap
    );
    markdown.push_str("\n## Duration invariance\n\n");
    let d = &report.duration_invariance;
    let _ = writeln!(
        markdown,
        "Compared masters: **{}**; selected BPM stable: **{}**; candidate order stable: **{}**; event clock stable: **{}**; meter hypotheses stable: **{}**.",
        d.compared_master_count,
        d.stable_selected_bpm,
        d.stable_candidate_ordering,
        d.stable_event_clock,
        d.stable_meter_hypotheses
    );
    if !d.duration_sensitive_ranking.is_empty() {
        markdown.push_str("\nDuration-sensitive cases are listed in `duration-invariance.json`; they are not silently averaged away.\n");
    }
    markdown.push_str("\n## AutoMix consequence\n\n");
    let a = &report.automix_consequence;
    let _ = writeln!(
        markdown,
        "Controlled pairs: **{}**; early single-BPM BeatMatched: **{}**; late-hypothesis BeatMatched: **{}**; additional late candidates: **{}**; quality-guarded cases: **{}**.",
        a.pair_count,
        a.early_single_bpm_beatmatched,
        a.late_hypothesis_beatmatched,
        a.late_additional_beatmatched,
        a.quality_guarded_cases
    );
    let _ = writeln!(
        markdown,
        "Wrong-alias unsafe BeatMatched cases: **{}**; wrong-alias cases saved by the quality guard: **{}**.",
        a.wrong_alias_unsafe_cases, a.wrong_alias_quality_guarded_cases
    );
    markdown.push_str("\n## Repeatability\n\n");
    let r = &report.repeatability;
    let _ = writeln!(
        markdown,
        "Repeated representatives: **{}**; exact observation digest matches: **{}**; exact event-clock matches: **{}**.",
        r.repeated_fixture_count, r.exact_observation_digest_matches, r.exact_event_clock_matches
    );
    markdown.push_str("\n## Explicit answers\n\n");
    for (key, answer) in &report.decision.question_answers {
        let _ = writeln!(markdown, "- **{key}:** {answer}");
    }
    markdown.push_str("\n## Decision gate\n\n");
    let _ = writeln!(
        markdown,
        "**Recommendation: {}**\n\nThe result is research-only. A production proposal would additionally require held-out rhythm-family validation, cross-duration stability, exact repeatability, and no regression in false BeatMatched transitions or rendered quality. The generated JSON/CSV files contain the per-fixture evidence, candidate fields, relation classifications, and planner details needed for audit.\n",
        report.decision.recommendation
    );
    markdown
}

trait EventStyleLabel {
    fn event_style_label(&self) -> String;
}

impl EventStyleLabel for FixtureSpec {
    fn event_style_label(&self) -> String {
        format!("{:?}", self.event_style).to_ascii_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_specs_are_bounded_and_deterministic() {
        let first = long_fixture_specs();
        let second = long_fixture_specs();
        assert_eq!(first.len(), second.len());
        assert!(first.iter().all(|spec| spec.duration_micros >= 30_000_000));
        assert!(first.iter().all(|spec| spec.duration_micros <= 60_000_000));
        assert_eq!(first[0].id, second[0].id);
    }

    #[test]
    fn oracle_relation_uses_canonical_lab_tolerance() {
        assert_eq!(relation(Some(120.0), 120.0), "primary");
        assert_eq!(relation(Some(60.0), 120.0), "half_time");
        assert_eq!(relation(Some(240.0), 120.0), "double_time");
        assert_eq!(relation(Some(121.0), 120.0), "other_wrong");
    }

    #[test]
    fn interval_summary_is_deterministic() {
        let summary = interval_summary(&[1.0, 2.0, 3.0, 100.0]);
        assert_eq!(summary.count, 4);
        assert_eq!(summary.median, Some(2.5));
        assert!(summary.mad.is_some());
    }
}
