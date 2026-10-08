//! Blind, research-only validation for user-supplied real-world audio.
//!
//! Filenames remain opaque identities. This module never searches titles,
//! consults an external BPM source, or uses metadata as an inference feature.
//! Audio is analyzed through the additive runtime decoder boundary.

use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use symphonia::{
    core::{
        audio::SampleBuffer,
        codecs::DecoderOptions,
        errors::Error as SymphoniaError,
        formats::FormatOptions,
        io::{MediaSourceStream, MediaSourceStreamOptions},
        meta::MetadataOptions,
        probe::Hint,
    },
    default::{get_codecs, get_probe},
};
use wotoha_core::{
    analysis::{TempoRelation, TrackAnalysisV2},
    automix::{AutoMixConfig, TransitionKind, plan_guarded_transition_v2, plan_transition_v2},
};
use wotoha_runtime::{AnalysisOutcome, analyze_bytes_for_research, analyze_file_for_research};

use crate::{LabError, fit_stable_tempo_grid, write_json};

const REPORT_SCHEMA: u32 = 1;
const SEGMENT_SECONDS: [u64; 2] = [30, 60];
const STATIONARY_RELATIVE_DRIFT: f64 = 0.03;
const STATIONARY_RELATIVE_MAD: f64 = 0.08;
const INTEGER_SNAP_RISK_RELATIVE: f64 = 0.0025;
const MAX_PAIR_REPORTS: usize = 16;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealSongResearchReport {
    pub schema_version: u32,
    pub source_commit: String,
    pub starting_commit: Option<String>,
    pub production_behavior_changed: bool,
    pub research_only: bool,
    pub track_count: usize,
    pub input_policy: InputPolicy,
    pub tracks: Vec<RealSongTrackReport>,
    pub shadow_pairs: Vec<RealSongShadowPair>,
    pub repeatability: Vec<RealSongRepeatability>,
    pub summary: RealSongSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputPolicy {
    pub filenames_are_identity_only: bool,
    pub metadata_used_for_inference: bool,
    pub external_reference_used: bool,
    pub external_bpm_lookup_used: bool,
    pub source_files_are_retained: bool,
    pub decoded_pcm_is_retained: bool,
    pub pcm_hash_encoding: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealSongTrackReport {
    pub track_id: String,
    pub source_filename: String,
    pub source_sha256: String,
    pub source_bytes: usize,
    pub decoded_pcm_sha256: String,
    pub container_extension: String,
    pub codec: String,
    pub sample_rate: u32,
    pub channels: usize,
    pub decoded_frames: usize,
    pub duration_micros: u64,
    pub full: RealSongAnalysisReport,
    pub segments: Vec<RealSongSegmentReport>,
    pub segment_consistency: SegmentConsistencyReport,
    pub failure_taxonomy: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealSongAnalysisReport {
    pub analysis_backend: String,
    pub classical_selected_bpm: Option<f32>,
    pub classical_candidates_bpm: Vec<f32>,
    pub tempo_hypotheses: Vec<TempoHypothesisReport>,
    pub conservative_decision: String,
    pub primary_relation: String,
    pub raw_event_times_micros: Vec<u64>,
    pub raw_event_digest: String,
    pub raw_event_clock: RawEventClockReport,
    pub grid_method_comparison: GridMethodComparisonReport,
    pub refined_grid: RefinedGridReport,
    pub integer_snap_audit: IntegerSnapAudit,
    pub stationarity: StationarityReport,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoHypothesisReport {
    pub bpm: f32,
    pub relation: String,
    pub relative_weight: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealSongSegmentReport {
    pub label: String,
    pub start_micros: u64,
    pub duration_micros: u64,
    pub analysis: RealSongAnalysisReport,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawEventClockReport {
    pub event_count: usize,
    pub median_interval_micros: Option<f64>,
    pub mad_interval_micros: Option<f64>,
    pub event_clock_bpm: Option<f64>,
    pub early_bpm: Option<f64>,
    pub middle_bpm: Option<f64>,
    pub late_bpm: Option<f64>,
    pub early_late_relative_drift: Option<f64>,
    pub short_intervals: usize,
    pub long_intervals: usize,
    pub confidence_min: Option<f32>,
    pub confidence_median: Option<f32>,
    pub confidence_max: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GridMethodComparisonReport {
    pub adjacent_median_bpm: Option<f64>,
    pub trimmed_adjacent_median_bpm: Option<f64>,
    pub early_event_clock_bpm: Option<f64>,
    pub middle_event_clock_bpm: Option<f64>,
    pub late_event_clock_bpm: Option<f64>,
    pub sequential_global_regression_bpm: Option<f64>,
    pub missing_jump_global_regression_bpm: Option<f64>,
    pub sequential_endpoint_bpm: Option<f64>,
    pub missing_jump_endpoint_bpm: Option<f64>,
    pub classical_selected_bpm: Option<f32>,
    pub classical_candidate_bpms: Vec<f32>,
    pub selected_primary_relation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefinedGridReport {
    pub accepted: bool,
    pub stationarity_status: String,
    pub fit_period_micros: Option<f64>,
    pub fit_bpm: Option<f64>,
    pub phase_micros: Option<f64>,
    pub residual_median_micros: Option<f64>,
    pub residual_p95_micros: Option<f64>,
    pub explained_event_fraction: f64,
    pub raw_event_count: usize,
    pub refined_grid_count: usize,
    pub inserted_grid_count: usize,
    pub rejected_event_count: usize,
    pub refined_grid_times_micros: Vec<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntegerSnapAudit {
    pub raw_event_clock_bpm: Option<f64>,
    pub nearest_integer_bpm: Option<f64>,
    pub absolute_distance_bpm: Option<f64>,
    pub relative_distance: Option<f64>,
    pub snap_eligible_under_research_rule: bool,
    pub snap_used: bool,
    pub risk_rule: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StationarityReport {
    pub status: String,
    pub support_events: usize,
    pub relative_period_drift: Option<f64>,
    pub relative_interval_mad: Option<f64>,
    pub inference_truth_free: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SegmentConsistencyReport {
    pub segment_count: usize,
    pub canonical_bpm_consistent: usize,
    pub family_consistent: usize,
    pub canonical_threshold_relative: f64,
    pub family_threshold_relative: f64,
    pub corpus_constant_tempo_assertion_used_only_after_inference: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealSongShadowPair {
    pub pair_id: String,
    pub outgoing_track_id: String,
    pub incoming_track_id: String,
    pub observed_pair_class: String,
    pub outgoing_bpm: Option<f32>,
    pub incoming_bpm: Option<f32>,
    pub ordinary_selected_kind: String,
    pub guarded_selected_kind: String,
    pub quality_first_selected_kind: String,
    pub ordinary_beatmatched_candidate_count: usize,
    pub guarded_beatmatched_candidate_count: usize,
    pub ordinary_beatmatched_candidate_cost: Option<f32>,
    pub ordinary_gapless_cost: Option<f32>,
    pub ordinary_crossfade_cost: Option<f32>,
    pub selected_tempo_pair: Option<TempoPairReport>,
    pub beat_pairs: usize,
    pub phase_error_micros: Option<u64>,
    pub cue_counts: CueCounts,
    pub quality_evidence: QualityEvidence,
    pub internal_classification: String,
    pub no_external_truth_claim: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TempoPairReport {
    pub outgoing_bpm: f32,
    pub incoming_bpm: f32,
    pub outgoing_relation: String,
    pub incoming_relation: String,
    pub normalized_adjustment: f32,
    pub ratio: f32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CueCounts {
    pub ordinary_outgoing_mix_out: usize,
    pub ordinary_incoming_mix_in: usize,
    pub cue_pairs_checked: usize,
    pub cue_tempo_combinations_checked: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QualityEvidence {
    pub beat_pairs_checked: usize,
    pub beat_phase_coverage: Option<f32>,
    pub max_beat_phase_error_micros: Option<u64>,
    pub blocking_issues: Vec<String>,
    pub render_quality: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RealSongRepeatability {
    pub track_id: String,
    pub runs: usize,
    pub observation_digests: Vec<String>,
    pub event_digests: Vec<String>,
    pub exact_observation_match: bool,
    pub exact_event_match: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RealSongSummary {
    pub repeatable_tracks: usize,
    pub canonical_segment_consistent_tracks: usize,
    pub family_segment_consistent_tracks: usize,
    pub refinement_accepted_tracks: usize,
    pub integer_snap_eligible_tracks: usize,
    pub integer_snap_used_tracks: usize,
    pub integer_snap_risk_tracks: usize,
    pub stationary_tracks: usize,
    pub internally_consistent_beatmatched_pairs: usize,
    pub ambiguous_beatmatched_pairs: usize,
    pub safe_fallback_pairs: usize,
    pub internal_contradiction_pairs: usize,
    pub suspicious_track_ids: Vec<String>,
}

struct DecodedSource {
    bytes_sha256: String,
    source_bytes: usize,
    pcm_sha256: String,
    codec: String,
    sample_rate: u32,
    channels: usize,
    samples: Vec<f32>,
}

struct AnalyzedTrack {
    report: RealSongTrackReport,
    v2: TrackAnalysisV2,
}

pub fn run_real_song_research(
    audio_root: &Path,
    output_dir: &Path,
    source_commit: String,
    starting_commit: Option<String>,
) -> Result<RealSongResearchReport, LabError> {
    fs::create_dir_all(output_dir)?;
    let paths = collect_audio_files(audio_root)?;
    if paths.is_empty() {
        return Err(LabError::InvalidInput(format!(
            "no supported audio files under {}",
            audio_root.display()
        )));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| LabError::InvalidInput(format!("tokio runtime: {error}")))?;
    let mut analyzed = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        analyzed.push(analyze_track(&runtime, index + 1, path)?);
    }
    let mut repeatability = Vec::with_capacity(analyzed.len());
    for (index, path) in paths.iter().enumerate() {
        repeatability.push(check_repeatability(
            &runtime,
            index + 1,
            path,
            &analyzed[index].report.full,
        )?);
    }
    let shadow_pairs = build_shadow_pairs(&analyzed);
    let summary = summarize(&analyzed, &shadow_pairs, &repeatability);
    let report = RealSongResearchReport {
        schema_version: REPORT_SCHEMA,
        source_commit,
        starting_commit,
        production_behavior_changed: false,
        research_only: true,
        track_count: analyzed.len(),
        input_policy: InputPolicy {
            filenames_are_identity_only: true,
            metadata_used_for_inference: false,
            external_reference_used: false,
            external_bpm_lookup_used: false,
            source_files_are_retained: false,
            decoded_pcm_is_retained: false,
            pcm_hash_encoding:
                "interleaved decoded f32 samples, IEEE-754 little-endian bytes; hash only".into(),
        },
        tracks: analyzed.into_iter().map(|item| item.report).collect(),
        shadow_pairs,
        repeatability,
        summary,
    };
    write_json(&output_dir.join("real-song-research.json"), &report)?;
    fs::write(output_dir.join("real-song-research.md"), markdown(&report))?;
    Ok(report)
}

fn collect_audio_files(root: &Path) -> Result<Vec<PathBuf>, LabError> {
    let mut files = Vec::new();
    collect_audio_files_recursive(root, &mut files)?;
    files.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));
    Ok(files)
}

fn collect_audio_files_recursive(root: &Path, files: &mut Vec<PathBuf>) -> Result<(), LabError> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if name != "__MACOSX" {
                collect_audio_files_recursive(&path, files)?;
            }
        } else if matches!(
            path.extension()
                .and_then(|extension| extension.to_str())
                .map(|extension| extension.to_ascii_lowercase())
                .as_deref(),
            Some("mp3") | Some("m4a") | Some("wav") | Some("flac") | Some("ogg") | Some("mp4")
        ) {
            files.push(path);
        }
    }
    Ok(())
}

fn analyze_track(
    runtime: &tokio::runtime::Runtime,
    index: usize,
    path: &Path,
) -> Result<AnalyzedTrack, LabError> {
    let decoded = decode_source(path)?;
    let outcome = runtime
        .block_on(analyze_file_for_research(path.to_path_buf()))
        .ok_or_else(|| {
            LabError::InvalidInput(format!("Wotoha analysis failed for {}", path.display()))
        })?;
    let (legacy, v2) = outcome_to_v2(outcome)?;
    let full = analysis_report(&legacy, &v2)?;
    let segments = analyze_segments(runtime, &decoded)?;
    let segment_consistency = segment_consistency(&full, &segments);
    let mut failure_taxonomy = Vec::new();
    if full.classical_selected_bpm.is_none() {
        failure_taxonomy.push("CANONICAL_BPM_DISAGREEMENT".into());
    }
    if segment_consistency.canonical_bpm_consistent < segment_consistency.segment_count {
        if segment_consistency.family_consistent == segment_consistency.segment_count {
            failure_taxonomy.push("HALF_DOUBLE_FAMILY_ONLY".into());
        } else {
            failure_taxonomy.push("SEGMENT_BPM_DRIFT".into());
        }
    }
    if full.raw_event_clock.event_count < 8 {
        failure_taxonomy.push("LOW_EVENT_COVERAGE".into());
    }
    if full.stationarity.status != "stationary" {
        failure_taxonomy.push("RAW_GRID_UNSTABLE".into());
    }
    if !full.refined_grid.accepted && full.raw_event_clock.event_count >= 8 {
        failure_taxonomy.push("REFINEMENT_REJECTED".into());
    }
    if full.integer_snap_audit.snap_eligible_under_research_rule {
        failure_taxonomy.push("INTEGER_SNAP_RISK".into());
    }
    if failure_taxonomy.is_empty() {
        failure_taxonomy.push("STABLE".into());
    }
    let channels = decoded.channels.max(1);
    let frames = decoded.samples.len() / channels;
    Ok(AnalyzedTrack {
        report: RealSongTrackReport {
            track_id: format!("track-{index:03}"),
            source_filename: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            source_sha256: decoded.bytes_sha256,
            source_bytes: decoded.source_bytes,
            decoded_pcm_sha256: decoded.pcm_sha256,
            container_extension: path
                .extension()
                .map(|value| value.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default(),
            codec: decoded.codec,
            sample_rate: decoded.sample_rate,
            channels: decoded.channels,
            decoded_frames: frames,
            duration_micros: duration_micros(frames, decoded.sample_rate),
            full,
            segments,
            segment_consistency,
            failure_taxonomy,
        },
        v2,
    })
}

fn decode_source(path: &Path) -> Result<DecodedSource, LabError> {
    let bytes = fs::read(path)?;
    let bytes_sha256 = sha256(&bytes);
    let file = File::open(path)?;
    let source = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
        hint.with_extension(extension);
    }
    let probed = get_probe()
        .format(
            &hint,
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|error| LabError::InvalidInput(format!("probe {}: {error}", path.display())))?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| LabError::InvalidInput(format!("no audio track: {}", path.display())))?;
    let codec = format!("{:?}", track.codec_params.codec);
    let track_id = track.id;
    let mut sample_rate = track.codec_params.sample_rate;
    let mut channels = track.codec_params.channels.map(|channels| channels.count());
    let mut decoder = get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|error| LabError::InvalidInput(format!("decoder {}: {error}", path.display())))?;
    let mut samples = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(error) => {
                return Err(LabError::InvalidInput(format!(
                    "decode {}: {error}",
                    path.display()
                )));
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = decoder.decode(&packet).map_err(|error| {
            LabError::InvalidInput(format!("decode {}: {error}", path.display()))
        })?;
        let spec = *decoded.spec();
        sample_rate.get_or_insert(spec.rate);
        channels.get_or_insert(spec.channels.count());
        let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buffer.copy_interleaved_ref(decoded);
        samples.extend_from_slice(buffer.samples());
    }
    let sample_rate = sample_rate.ok_or_else(|| {
        LabError::InvalidInput(format!("missing sample rate: {}", path.display()))
    })?;
    let channels = channels.unwrap_or_default();
    if channels == 0 || samples.is_empty() {
        return Err(LabError::InvalidInput(format!(
            "empty decoded audio: {}",
            path.display()
        )));
    }
    let mut pcm = Sha256::new();
    for sample in &samples {
        pcm.update(sample.to_le_bytes());
    }
    Ok(DecodedSource {
        bytes_sha256,
        source_bytes: bytes.len(),
        pcm_sha256: format!("{:x}", pcm.finalize()),
        codec,
        sample_rate,
        channels,
        samples,
    })
}

fn outcome_to_v2(
    outcome: AnalysisOutcome,
) -> Result<(wotoha_core::automix::TrackAnalysis, TrackAnalysisV2), LabError> {
    let legacy = outcome.analysis;
    let v2 = if let Some(rhythm) = outcome.v2_rhythm {
        wotoha_runtime::track_analysis_v2_from_legacy_rhythm(&legacy, rhythm, true)
    } else {
        wotoha_runtime::track_analysis_v2_from_legacy(&legacy)
    }
    .ok_or_else(|| LabError::InvalidInput("runtime V2 adaptation failed".into()))?;
    Ok((legacy, v2))
}

fn analyze_segments(
    runtime: &tokio::runtime::Runtime,
    decoded: &DecodedSource,
) -> Result<Vec<RealSongSegmentReport>, LabError> {
    let frames = decoded.samples.len() / decoded.channels;
    let duration = duration_micros(frames, decoded.sample_rate);
    let mut ranges = Vec::new();
    for seconds in SEGMENT_SECONDS {
        let span = seconds.saturating_mul(1_000_000);
        if duration >= span {
            ranges.push((format!("first_{seconds}s"), 0, span));
            ranges.push((format!("middle_{seconds}s"), (duration - span) / 2, span));
            ranges.push((format!("last_{seconds}s"), duration - span, span));
        }
    }
    ranges.sort_by_key(|(_, start, _)| *start);
    ranges.dedup();
    let mut segments = Vec::new();
    for (label, start, span) in ranges {
        let start_frame = ((start as u128 * decoded.sample_rate as u128) / 1_000_000) as usize;
        let frame_count = ((span as u128 * decoded.sample_rate as u128) / 1_000_000) as usize;
        let end_frame = start_frame.saturating_add(frame_count).min(frames);
        let begin = start_frame.saturating_mul(decoded.channels);
        let end = end_frame.saturating_mul(decoded.channels);
        if end <= begin || end > decoded.samples.len() {
            continue;
        }
        let wav = encode_wav_f32(
            &decoded.samples[begin..end],
            decoded.sample_rate,
            decoded.channels,
        )?;
        let outcome = runtime
            .block_on(analyze_bytes_for_research(wav))
            .ok_or_else(|| LabError::InvalidInput(format!("segment analysis failed: {label}")))?;
        let (legacy, v2) = outcome_to_v2(outcome)?;
        segments.push(RealSongSegmentReport {
            label,
            start_micros: start,
            duration_micros: duration_micros(end_frame - start_frame, decoded.sample_rate),
            analysis: analysis_report(&legacy, &v2)?,
        });
    }
    Ok(segments)
}

fn analysis_report(
    legacy: &wotoha_core::automix::TrackAnalysis,
    v2: &TrackAnalysisV2,
) -> Result<RealSongAnalysisReport, LabError> {
    let raw_events = v2
        .rhythm
        .beats
        .iter()
        .map(|beat| beat.time.as_micros() as u64)
        .collect::<Vec<_>>();
    let confidence_values = v2
        .rhythm
        .beats
        .iter()
        .map(|beat| beat.timing_confidence.get() as f64)
        .collect::<Vec<_>>();
    let raw_event_clock = raw_event_clock(&raw_events, &confidence_values);
    let refined_grid = refine_grid(&raw_events, legacy.duration.as_micros() as u64);
    let integer = integer_snap_audit(raw_event_clock.event_clock_bpm);
    let tempo_hypotheses = v2
        .rhythm
        .tempo_hypotheses
        .iter()
        .map(|hypothesis| TempoHypothesisReport {
            bpm: hypothesis.bpm,
            relation: format_relation(hypothesis.relation),
            relative_weight: hypothesis.relative_weight.into(),
        })
        .collect::<Vec<_>>();
    let relation = v2
        .rhythm
        .primary_tempo_hypothesis()
        .map(|hypothesis| format_relation(hypothesis.relation))
        .unwrap_or_else(|| "unknown".into());
    let grid_method_comparison = grid_method_comparison(
        &raw_events,
        &raw_event_clock,
        legacy.bpm,
        &tempo_hypotheses,
        &relation,
    );
    Ok(RealSongAnalysisReport {
        analysis_backend: if v2.has_native_rhythm_provenance() {
            "neural_v2".into()
        } else {
            "classical_legacy_adapter".into()
        },
        classical_selected_bpm: legacy.bpm,
        classical_candidates_bpm: tempo_hypotheses
            .iter()
            .map(|candidate| candidate.bpm)
            .collect(),
        tempo_hypotheses,
        conservative_decision: if legacy.bpm.is_some() {
            "selected"
        } else {
            "abstained"
        }
        .into(),
        primary_relation: relation,
        raw_event_digest: digest_u64s(&raw_events),
        raw_event_times_micros: raw_events,
        raw_event_clock: raw_event_clock.clone(),
        grid_method_comparison,
        refined_grid,
        integer_snap_audit: integer,
        stationarity: stationarity(&raw_event_clock),
    })
}

fn grid_method_comparison(
    events: &[u64],
    clock: &RawEventClockReport,
    classical_selected_bpm: Option<f32>,
    classical_candidates: &[TempoHypothesisReport],
    relation: &str,
) -> GridMethodComparisonReport {
    let intervals = events
        .windows(2)
        .filter_map(|pair| (pair[1] > pair[0]).then_some((pair[1] - pair[0]) as f64))
        .collect::<Vec<_>>();
    let mut sorted = intervals.clone();
    sorted.sort_by(f64::total_cmp);
    let trim = (sorted.len() / 20).min(sorted.len().saturating_sub(1));
    let trimmed = if sorted.is_empty() {
        None
    } else {
        median(&sorted[trim..sorted.len().saturating_sub(trim)])
    };
    let sequential = regression_period(events, None);
    let base = median(&intervals);
    let jump_indices = base.map(|period| cumulative_indices(events, period));
    let missing_jump = jump_indices
        .as_deref()
        .and_then(|indices| regression_period(events, Some(indices)));
    let sequential_endpoint = endpoint_bpm(events, None);
    let missing_jump_endpoint = jump_indices
        .as_deref()
        .and_then(|indices| endpoint_bpm(events, Some(indices)));
    GridMethodComparisonReport {
        adjacent_median_bpm: clock.event_clock_bpm,
        trimmed_adjacent_median_bpm: trimmed.map(|period| 60_000_000.0 / period),
        early_event_clock_bpm: clock.early_bpm,
        middle_event_clock_bpm: clock.middle_bpm,
        late_event_clock_bpm: clock.late_bpm,
        sequential_global_regression_bpm: sequential.map(|period| 60_000_000.0 / period),
        missing_jump_global_regression_bpm: missing_jump.map(|period| 60_000_000.0 / period),
        sequential_endpoint_bpm: sequential_endpoint,
        missing_jump_endpoint_bpm: missing_jump_endpoint,
        classical_selected_bpm,
        classical_candidate_bpms: classical_candidates
            .iter()
            .map(|candidate| candidate.bpm)
            .collect(),
        selected_primary_relation: relation.into(),
    }
}

fn cumulative_indices(events: &[u64], base_period: f64) -> Vec<i64> {
    let mut indices = vec![0_i64];
    for pair in events.windows(2) {
        let interval = pair[1].saturating_sub(pair[0]) as f64;
        let multiple = (interval / base_period).round().clamp(1.0, 4.0) as i64;
        indices.push(indices.last().copied().unwrap_or_default() + multiple);
    }
    indices
}

fn regression_period(events: &[u64], indices: Option<&[i64]>) -> Option<f64> {
    if events.len() < 2 {
        return None;
    }
    let points = events
        .iter()
        .enumerate()
        .map(|(position, time)| {
            (
                indices
                    .and_then(|values| values.get(position).copied())
                    .unwrap_or(position as i64) as f64,
                *time as f64,
            )
        })
        .collect::<Vec<_>>();
    let mean_x = points.iter().map(|(x, _)| *x).sum::<f64>() / points.len() as f64;
    let mean_y = points.iter().map(|(_, y)| *y).sum::<f64>() / points.len() as f64;
    let denominator = points
        .iter()
        .map(|(x, _)| (*x - mean_x).powi(2))
        .sum::<f64>();
    if denominator <= f64::EPSILON {
        return None;
    }
    let period = points
        .iter()
        .map(|(x, y)| (*x - mean_x) * (*y - mean_y))
        .sum::<f64>()
        / denominator;
    (period.is_finite() && period > 0.0).then_some(period)
}

fn endpoint_bpm(events: &[u64], indices: Option<&[i64]>) -> Option<f64> {
    if events.len() < 2 {
        return None;
    }
    let first_index = indices
        .and_then(|values| values.first().copied())
        .unwrap_or(0);
    let last_index = indices
        .and_then(|values| values.last().copied())
        .unwrap_or(events.len().saturating_sub(1) as i64);
    let span = last_index - first_index;
    (span > 0).then(|| 60_000_000.0 * span as f64 / (events[events.len() - 1] - events[0]) as f64)
}

fn raw_event_clock(events: &[u64], confidence_values: &[f64]) -> RawEventClockReport {
    let intervals = events
        .windows(2)
        .filter_map(|pair| (pair[1] > pair[0]).then_some((pair[1] - pair[0]) as f64))
        .collect::<Vec<_>>();
    let median_interval = median(&intervals);
    let mad_interval = median_interval.map(|value| {
        median(
            &intervals
                .iter()
                .map(|interval| (interval - value).abs())
                .collect::<Vec<_>>(),
        )
        .unwrap_or(0.0)
    });
    let bpm_of = |slice: &[u64]| {
        let values = slice
            .windows(2)
            .filter_map(|pair| (pair[1] > pair[0]).then_some((pair[1] - pair[0]) as f64))
            .collect::<Vec<_>>();
        median(&values).map(|interval| 60_000_000.0 / interval)
    };
    let split = events.len() / 2;
    let early_bpm = (split >= 2).then(|| bpm_of(&events[..=split])).flatten();
    let middle_bpm = (events.len() >= 4)
        .then(|| bpm_of(&events[events.len() / 4..events.len() * 3 / 4]))
        .flatten();
    let late_bpm = (events.len().saturating_sub(split) >= 2)
        .then(|| bpm_of(&events[split..]))
        .flatten();
    let drift = early_bpm
        .zip(late_bpm)
        .map(|(early, late)| (late / early - 1.0).abs());
    let short = median_interval
        .map(|base| {
            intervals
                .iter()
                .filter(|interval| **interval < base * 0.65)
                .count()
        })
        .unwrap_or(0);
    let long = median_interval
        .map(|base| {
            intervals
                .iter()
                .filter(|interval| **interval > base * 1.5)
                .count()
        })
        .unwrap_or(0);
    RawEventClockReport {
        event_count: events.len(),
        median_interval_micros: median_interval,
        mad_interval_micros: mad_interval,
        event_clock_bpm: median_interval.map(|interval| 60_000_000.0 / interval),
        early_bpm,
        middle_bpm,
        late_bpm,
        early_late_relative_drift: drift,
        short_intervals: short,
        long_intervals: long,
        confidence_min: percentile(confidence_values, 0.0).map(|value| value as f32),
        confidence_median: median(confidence_values).map(|value| value as f32),
        confidence_max: percentile(confidence_values, 1.0).map(|value| value as f32),
    }
}

fn stationarity(clock: &RawEventClockReport) -> StationarityReport {
    let relative_mad = clock
        .median_interval_micros
        .zip(clock.mad_interval_micros)
        .map(|(median, mad)| mad / median.max(1.0));
    let status = match (
        clock.event_count,
        clock.early_late_relative_drift,
        relative_mad,
    ) {
        (count, _, _) if count < 8 => "insufficient_support",
        (_, Some(drift), Some(mad))
            if drift <= STATIONARY_RELATIVE_DRIFT && mad <= STATIONARY_RELATIVE_MAD =>
        {
            "stationary"
        }
        (_, Some(_), Some(_)) => "nonstationary_or_ambiguous",
        _ => "ambiguous",
    };
    StationarityReport {
        status: status.into(),
        support_events: clock.event_count,
        relative_period_drift: clock.early_late_relative_drift,
        relative_interval_mad: relative_mad,
        inference_truth_free: true,
    }
}

fn refine_grid(events: &[u64], duration_micros: u64) -> RefinedGridReport {
    let fit = fit_stable_tempo_grid(events, duration_micros, None);
    RefinedGridReport {
        accepted: fit.accepted,
        stationarity_status: fit.stationarity_status,
        fit_period_micros: fit.fit_period_micros,
        fit_bpm: fit.fit_bpm.map(f64::from),
        phase_micros: fit.phase_micros,
        residual_median_micros: fit
            .inlier_residual_median_micros
            .or(fit.raw_residual_median_micros)
            .map(|value| value as f64),
        residual_p95_micros: fit
            .inlier_residual_p95_micros
            .or(fit.raw_residual_p95_micros)
            .map(|value| value as f64),
        explained_event_fraction: f64::from(fit.explained_event_fraction),
        raw_event_count: fit.raw_event_count,
        refined_grid_count: fit.grid_times_micros.len(),
        inserted_grid_count: fit.inserted_grid_count,
        rejected_event_count: fit.rejected_event_count,
        refined_grid_times_micros: fit.grid_times_micros,
    }
}

fn integer_snap_audit(bpm: Option<f64>) -> IntegerSnapAudit {
    let (nearest, distance, relative) = bpm
        .map(|value| {
            let nearest = value.round();
            (
                nearest,
                (value - nearest).abs(),
                (value - nearest).abs() / value.max(1.0),
            )
        })
        .map_or((None, None, None), |(nearest, distance, relative)| {
            (Some(nearest), Some(distance), Some(relative))
        });
    let eligible = relative.is_some_and(|value| value <= INTEGER_SNAP_RISK_RELATIVE);
    IntegerSnapAudit {
        raw_event_clock_bpm: bpm,
        nearest_integer_bpm: nearest,
        absolute_distance_bpm: distance,
        relative_distance: relative,
        snap_eligible_under_research_rule: eligible,
        snap_used: false,
        risk_rule:
            "audit-only; risk if nearest-integer relative distance <= 0.25%; no snap is applied"
                .into(),
    }
}

fn segment_consistency(
    full: &RealSongAnalysisReport,
    segments: &[RealSongSegmentReport],
) -> SegmentConsistencyReport {
    let full_bpm = full.classical_selected_bpm.map(f64::from);
    let mut canonical = 0;
    let mut family = 0;
    for segment in segments {
        let Some(segment_bpm) = segment.analysis.classical_selected_bpm.map(f64::from) else {
            continue;
        };
        let Some(full_bpm) = full_bpm else {
            continue;
        };
        if (segment_bpm / full_bpm - 1.0).abs() <= 0.01 {
            canonical += 1;
        }
        let family_error = [1.0, 2.0, 0.5]
            .iter()
            .map(|ratio| (segment_bpm / (full_bpm * ratio) - 1.0).abs())
            .fold(f64::INFINITY, f64::min);
        if family_error <= 0.01 {
            family += 1;
        }
    }
    SegmentConsistencyReport {
        segment_count: segments.len(),
        canonical_bpm_consistent: canonical,
        family_consistent: family,
        canonical_threshold_relative: 0.01,
        family_threshold_relative: 0.01,
        corpus_constant_tempo_assertion_used_only_after_inference: true,
    }
}

fn build_shadow_pairs(analyzed: &[AnalyzedTrack]) -> Vec<RealSongShadowPair> {
    let mut pairs = Vec::new();
    let config = AutoMixConfig {
        enabled: true,
        crossfade: Duration::from_secs(8),
        max_tempo_adjustment: 0.05,
        min_beat_confidence: 0.70,
    };
    let mut candidates = analyzed
        .iter()
        .enumerate()
        .flat_map(|(left, outgoing)| {
            analyzed
                .iter()
                .enumerate()
                .skip(left + 1)
                .map(move |(right, incoming)| {
                    let a = outgoing.report.full.classical_selected_bpm.map(f64::from);
                    let b = incoming.report.full.classical_selected_bpm.map(f64::from);
                    let distance = a
                        .zip(b)
                        .map(|(a, b)| (a / b - 1.0).abs())
                        .unwrap_or(f64::INFINITY);
                    let alias = a.zip(b).is_some_and(|(a, b)| {
                        ((a / b).abs() - 2.0).abs() <= 0.08 || ((b / a).abs() - 2.0).abs() <= 0.08
                    });
                    (
                        if alias {
                            2
                        } else if distance <= 0.01 {
                            0
                        } else if distance <= 0.05 {
                            1
                        } else {
                            3
                        },
                        distance,
                        left,
                        right,
                    )
                })
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.total_cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
            .then_with(|| left.3.cmp(&right.3))
    });
    for (ordinal, (_, _, left, right)) in candidates.into_iter().take(MAX_PAIR_REPORTS).enumerate()
    {
        let outgoing = &analyzed[left];
        let incoming = &analyzed[right];
        let ordinary = plan_transition_v2(&outgoing.v2, &incoming.v2, &config);
        let guarded = plan_guarded_transition_v2(&outgoing.v2, &incoming.v2, &config);
        let quality_first = crate::quality_first_shadow_plan(&outgoing.v2, &incoming.v2, &config).1;
        let beat_candidate = ordinary.candidates.iter().find(|candidate| {
            candidate.plan.kind == TransitionKind::BeatMatched && candidate.hard_rejection.is_none()
        });
        let tempo_pair = beat_candidate
            .and_then(|candidate| candidate.beat_eligibility.as_ref())
            .and_then(|eligibility| eligibility.tempo_hypothesis)
            .map(|pair| TempoPairReport {
                outgoing_bpm: pair.outgoing.bpm,
                incoming_bpm: pair.incoming.bpm,
                outgoing_relation: format_relation(pair.outgoing.relation),
                incoming_relation: format_relation(pair.incoming.relation),
                normalized_adjustment: pair.normalized_adjustment,
                ratio: pair.ratio,
            });
        let quality = QualityEvidence {
            beat_pairs_checked: quality_first.quality.beat_pairs_checked,
            beat_phase_coverage: quality_first.quality.beat_phase_coverage,
            max_beat_phase_error_micros: quality_first
                .quality
                .max_beat_phase_error
                .map(|value| value.as_micros() as u64),
            blocking_issues: quality_first
                .quality
                .issues
                .iter()
                .map(|issue| format!("{issue:?}"))
                .collect(),
            render_quality: "not_rendered; source audio remains outside report".into(),
        };
        let classification = if quality_first.plan.kind == TransitionKind::BeatMatched {
            "INTERNALLY_CONSISTENT_BEATMATCHED"
        } else if ordinary.diagnostics.beatmatched_candidates > 0 {
            "AMBIGUOUS_BEATMATCHED"
        } else {
            "SAFE_FALLBACK"
        };
        pairs.push(RealSongShadowPair {
            pair_id: format!("pair-{ordinal:03}"),
            outgoing_track_id: outgoing.report.track_id.clone(),
            incoming_track_id: incoming.report.track_id.clone(),
            observed_pair_class: observed_pair_class(
                outgoing.report.full.classical_selected_bpm,
                incoming.report.full.classical_selected_bpm,
            ),
            outgoing_bpm: outgoing.report.full.classical_selected_bpm,
            incoming_bpm: incoming.report.full.classical_selected_bpm,
            ordinary_selected_kind: format!("{:?}", ordinary.plan.kind),
            guarded_selected_kind: format!("{:?}", guarded.plan.kind),
            quality_first_selected_kind: format!("{:?}", quality_first.plan.kind),
            ordinary_beatmatched_candidate_count: ordinary.diagnostics.beatmatched_candidates,
            guarded_beatmatched_candidate_count: guarded.diagnostics.beatmatched_candidates,
            ordinary_beatmatched_candidate_cost: beat_candidate
                .map(|candidate| candidate.cost.total),
            ordinary_gapless_cost: ordinary
                .candidates
                .iter()
                .find(|candidate| candidate.plan.kind == TransitionKind::Gapless)
                .map(|candidate| candidate.cost.total),
            ordinary_crossfade_cost: ordinary
                .candidates
                .iter()
                .find(|candidate| candidate.plan.kind == TransitionKind::Crossfade)
                .map(|candidate| candidate.cost.total),
            selected_tempo_pair: tempo_pair,
            beat_pairs: beat_candidate
                .and_then(|candidate| candidate.beat_eligibility.as_ref())
                .map(|eligibility| eligibility.beat_pairs)
                .unwrap_or(0),
            phase_error_micros: beat_candidate
                .and_then(|candidate| candidate.beat_eligibility.as_ref())
                .and_then(|eligibility| eligibility.phase_error)
                .map(|value| value.as_micros() as u64),
            cue_counts: CueCounts {
                ordinary_outgoing_mix_out: ordinary.diagnostics.outgoing_mix_out_cues,
                ordinary_incoming_mix_in: ordinary.diagnostics.incoming_mix_in_cues,
                cue_pairs_checked: ordinary.diagnostics.cue_pairs_checked,
                cue_tempo_combinations_checked: ordinary.diagnostics.cue_tempo_combinations_checked,
            },
            quality_evidence: quality,
            internal_classification: classification.into(),
            no_external_truth_claim: true,
        });
    }
    pairs
}

fn check_repeatability(
    runtime: &tokio::runtime::Runtime,
    index: usize,
    path: &Path,
    first: &RealSongAnalysisReport,
) -> Result<RealSongRepeatability, LabError> {
    let mut observation_digests = vec![digest_json(first)?];
    let mut event_digests = vec![first.raw_event_digest.clone()];
    for _ in 1..3 {
        let outcome = runtime
            .block_on(analyze_file_for_research(path.to_path_buf()))
            .ok_or_else(|| {
                LabError::InvalidInput(format!("repeatability analysis failed: {}", path.display()))
            })?;
        let (legacy, v2) = outcome_to_v2(outcome)?;
        let report = analysis_report(&legacy, &v2)?;
        observation_digests.push(digest_json(&report)?);
        event_digests.push(report.raw_event_digest);
    }
    Ok(RealSongRepeatability {
        track_id: format!("track-{index:03}"),
        runs: 3,
        exact_observation_match: observation_digests
            .windows(2)
            .all(|window| window[0] == window[1]),
        exact_event_match: event_digests
            .windows(2)
            .all(|window| window[0] == window[1]),
        observation_digests,
        event_digests,
    })
}

fn summarize(
    analyzed: &[AnalyzedTrack],
    pairs: &[RealSongShadowPair],
    repeatability: &[RealSongRepeatability],
) -> RealSongSummary {
    RealSongSummary {
        repeatable_tracks: repeatability
            .iter()
            .filter(|case| case.exact_observation_match && case.exact_event_match)
            .count(),
        canonical_segment_consistent_tracks: analyzed
            .iter()
            .filter(|track| {
                track.report.segment_consistency.canonical_bpm_consistent
                    == track.report.segment_consistency.segment_count
            })
            .count(),
        family_segment_consistent_tracks: analyzed
            .iter()
            .filter(|track| {
                track.report.segment_consistency.family_consistent
                    == track.report.segment_consistency.segment_count
            })
            .count(),
        refinement_accepted_tracks: analyzed
            .iter()
            .filter(|track| track.report.full.refined_grid.accepted)
            .count(),
        integer_snap_eligible_tracks: analyzed
            .iter()
            .filter(|track| {
                track
                    .report
                    .full
                    .integer_snap_audit
                    .snap_eligible_under_research_rule
            })
            .count(),
        integer_snap_used_tracks: analyzed
            .iter()
            .filter(|track| track.report.full.integer_snap_audit.snap_used)
            .count(),
        integer_snap_risk_tracks: analyzed
            .iter()
            .filter(|track| {
                track
                    .report
                    .full
                    .integer_snap_audit
                    .snap_eligible_under_research_rule
            })
            .count(),
        stationary_tracks: analyzed
            .iter()
            .filter(|track| track.report.full.stationarity.status == "stationary")
            .count(),
        internally_consistent_beatmatched_pairs: pairs
            .iter()
            .filter(|pair| pair.internal_classification == "INTERNALLY_CONSISTENT_BEATMATCHED")
            .count(),
        ambiguous_beatmatched_pairs: pairs
            .iter()
            .filter(|pair| pair.internal_classification == "AMBIGUOUS_BEATMATCHED")
            .count(),
        safe_fallback_pairs: pairs
            .iter()
            .filter(|pair| pair.internal_classification == "SAFE_FALLBACK")
            .count(),
        internal_contradiction_pairs: pairs
            .iter()
            .filter(|pair| pair.internal_classification == "INTERNAL_CONTRADICTION")
            .count(),
        suspicious_track_ids: analyzed
            .iter()
            .filter(|track| {
                !track
                    .report
                    .failure_taxonomy
                    .iter()
                    .any(|value| value == "STABLE")
            })
            .map(|track| track.report.track_id.clone())
            .collect(),
    }
}

fn observed_pair_class(left: Option<f32>, right: Option<f32>) -> String {
    let Some((left, right)) = left.zip(right) else {
        return "missing_tempo".into();
    };
    let distance = (left / right - 1.0).abs();
    if distance <= 0.01 {
        "same_observed_tempo".into()
    } else if distance <= 0.05 {
        "near_observed_tempo".into()
    } else if ((left / right).abs() - 2.0).abs() <= 0.08 {
        "observed_half_double_alias".into()
    } else {
        "distant_observed_tempo".into()
    }
}

fn encode_wav_f32(samples: &[f32], sample_rate: u32, channels: usize) -> Result<Vec<u8>, LabError> {
    let data_len = samples.len().saturating_mul(2);
    let riff_len = 36usize.saturating_add(data_len);
    let block_align = channels
        .checked_mul(2)
        .ok_or_else(|| LabError::InvalidInput("channel overflow".into()))?;
    let byte_rate = sample_rate
        .checked_mul(block_align as u32)
        .ok_or_else(|| LabError::InvalidInput("rate overflow".into()))?;
    let mut output = Vec::with_capacity(riff_len + 8);
    output.extend_from_slice(b"RIFF");
    output.extend_from_slice(&(riff_len as u32).to_le_bytes());
    output.extend_from_slice(b"WAVEfmt ");
    output.extend_from_slice(&16u32.to_le_bytes());
    output.extend_from_slice(&1u16.to_le_bytes());
    output.extend_from_slice(&(channels as u16).to_le_bytes());
    output.extend_from_slice(&sample_rate.to_le_bytes());
    output.extend_from_slice(&byte_rate.to_le_bytes());
    output.extend_from_slice(&(block_align as u16).to_le_bytes());
    output.extend_from_slice(&16u16.to_le_bytes());
    output.extend_from_slice(b"data");
    output.extend_from_slice(&(data_len as u32).to_le_bytes());
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
        output.extend_from_slice(&value.to_le_bytes());
    }
    Ok(output)
}

fn duration_micros(frames: usize, sample_rate: u32) -> u64 {
    ((frames as u128).saturating_mul(1_000_000) / sample_rate.max(1) as u128) as u64
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut values = values.to_vec();
    values.sort_by(|left, right| left.total_cmp(right));
    Some(values[values.len() / 2])
}

fn percentile(values: &[f64], fraction: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut values = values.to_vec();
    values.sort_by(|left, right| left.total_cmp(right));
    let index = ((values.len() - 1) as f64 * fraction.clamp(0.0, 1.0)).round() as usize;
    values.get(index).copied()
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn digest_u64s(values: &[u64]) -> String {
    let mut hasher = Sha256::new();
    for value in values {
        hasher.update(value.to_le_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn digest_json<T: Serialize>(value: &T) -> Result<String, LabError> {
    Ok(sha256(&serde_json::to_vec(value)?))
}

fn format_relation(relation: TempoRelation) -> String {
    match relation {
        TempoRelation::Primary => "primary",
        TempoRelation::HalfTime => "half_time",
        TempoRelation::DoubleTime => "double_time",
        TempoRelation::Alternative => "alternative",
    }
    .into()
}

fn markdown(report: &RealSongResearchReport) -> String {
    let summary = &report.summary;
    format!(
        "# Blind real-song Wotoha research\n\n\
         - source commit: {}\n\
         - tracks analyzed: {}\n\
         - production behavior changed: NO\n\
         - metadata used for inference: NO\n\
         - external reference used: NO\n\n\
         ## Internal consistency\n\n\
         - repeatable tracks: {}/{}\n\
         - canonical segment consistency: {}/{}\n\
         - family segment consistency: {}/{}\n\
         - refinement accepted: {}\n\
         - integer-snap eligible/risk: {}\n\
         - integer-snap used: {}\n\
         - stationary full-song observations: {}\n\n\
         ## AutoMix shadow\n\n\
         - pairs: {}\n\
         - internally consistent BeatMatched: {}\n\
         - ambiguous BeatMatched: {}\n\
         - safe fallback: {}\n\
         - internal contradiction: {}\n\n\
         This report is blind and research-only. It contains no external \
         analyzer result and makes no claim that a BeatMatched shadow result \
         is objectively correct on real music.\n",
        report.source_commit,
        report.track_count,
        summary.repeatable_tracks,
        report.track_count,
        summary.canonical_segment_consistent_tracks,
        report.track_count,
        summary.family_segment_consistent_tracks,
        report.track_count,
        summary.refinement_accepted_tracks,
        summary.integer_snap_risk_tracks,
        summary.integer_snap_used_tracks,
        summary.stationary_tracks,
        report.shadow_pairs.len(),
        summary.internally_consistent_beatmatched_pairs,
        summary.ambiguous_beatmatched_pairs,
        summary.safe_fallback_pairs,
        summary.internal_contradiction_pairs,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_snap_is_audit_only() {
        let audit = integer_snap_audit(Some(120.01));
        assert!(audit.snap_eligible_under_research_rule);
        assert!(!audit.snap_used);
    }

    #[test]
    fn refinement_is_deterministic() {
        let events = (0..32)
            .map(|index| 500_000 + index * 500_000)
            .collect::<Vec<_>>();
        let first = refine_grid(&events, 20_000_000);
        let second = refine_grid(&events, 20_000_000);
        assert_eq!(
            first.refined_grid_times_micros,
            second.refined_grid_times_micros
        );
        assert!(first.accepted);
        assert!(
            first
                .refined_grid_times_micros
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
    }
}
