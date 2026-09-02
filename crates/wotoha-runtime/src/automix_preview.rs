use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use songbird::input::{Input, LiveInput};
use symphonia::core::{audio::SampleBuffer, errors::Error as SymphoniaError};
use thiserror::Error;
use wotoha_core::{
    automix::{
        AutoMixConfig, AutoMixPeakGuard, AutoMixQualityReport, EqTransition, EqTransitionRole,
        TrackAnalysis, TransitionKind, TransitionPlan, automix_peak_safe_mix_gains,
        plan_guarded_transition_with_base_gains,
    },
    config::LoudnessConfig,
    loudness::loudness_normalization_gain,
};

use crate::{
    tempo_stretch::{TempoStretchProcessor, discard_interleaved_frames, effective_latency_frames},
    transition_dsp::{EqualizerControl, OutputTimeline, ThreeBandEqualizer},
};

const PREVIEW_CHANNELS: usize = 2;
const TEMPO_PREVIEW_PADDING: Duration = Duration::from_secs(1);
const GAPLESS_PREVIEW_SIDE: Duration = Duration::from_secs(4);
const MIN_PREVIEW_QUIETEST_TO_EDGE_RATIO: f32 = 0.35;
const MIN_PREVIEW_MID_TO_EDGE_RATIO: f32 = 0.60;
const MAX_PREVIEW_SAMPLE_PEAK_DBFS: f32 = -0.01;

pub struct AutoMixPreview {
    pub plan: TransitionPlan,
    pub quality: AutoMixQualityReport,
    pub render_metrics: AutoMixPreviewRenderMetrics,
    pub render_issues: Vec<AutoMixPreviewRenderIssue>,
    pub outgoing_normalization_gain: f32,
    pub incoming_normalization_gain: f32,
    pub sample_rate: u32,
    pub channels: u16,
    pub wav: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct AutoMixPreviewRenderMetrics {
    pub start_rms_dbfs: f32,
    pub mid_rms_dbfs: f32,
    pub end_rms_dbfs: f32,
    pub quietest_window_rms_dbfs: f32,
    pub quietest_to_edge_ratio: f32,
    pub mid_to_edge_ratio: f32,
    pub sample_peak_dbfs: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AutoMixPreviewRenderIssue {
    QuietGap { ratio: f32 },
    MidpointDrop { ratio: f32 },
    ClippingRisk { sample_peak_dbfs: f32 },
}

#[derive(Debug, Error)]
pub enum AutoMixPreviewError {
    #[error("preview source failed: {0}")]
    Source(String),
    #[error("preview input is not playable: {0}")]
    MakePlayable(String),
    #[error("preview input is not parsed audio")]
    UnparsedInput,
    #[error("preview audio has no known sample rate")]
    MissingSampleRate,
    #[error("preview decode failed: {0}")]
    Decode(String),
    #[error("preview tempo stretch failed: {0}")]
    TempoStretch(String),
    #[error("preview audio ended before the requested segment")]
    SegmentUnavailable,
    #[error("preview rendering was cancelled")]
    Cancelled,
}

#[allow(dead_code)]
pub(crate) fn render_automix_preview_inputs(
    outgoing_input: Input,
    incoming_input: Input,
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
    loudness: &LoudnessConfig,
) -> Result<AutoMixPreview, AutoMixPreviewError> {
    let cancelled = AtomicBool::new(false);
    render_automix_preview_inputs_with_cancel(
        outgoing_input,
        incoming_input,
        outgoing,
        incoming,
        config,
        loudness,
        &cancelled,
    )
}

pub(crate) fn render_automix_preview_inputs_with_cancel(
    outgoing_input: Input,
    incoming_input: Input,
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
    loudness: &LoudnessConfig,
    cancelled: &AtomicBool,
) -> Result<AutoMixPreview, AutoMixPreviewError> {
    check_cancelled(cancelled)?;
    let outgoing_normalization_gain = loudness_normalization_gain(loudness, Some(outgoing));
    let incoming_normalization_gain = loudness_normalization_gain(loudness, Some(incoming));
    let guarded = plan_guarded_transition_with_base_gains(
        outgoing,
        incoming,
        config,
        outgoing_normalization_gain,
        incoming_normalization_gain,
    );
    let plan = guarded.plan;
    let quality = guarded.quality;
    let output_rate = parsed_sample_rate(&outgoing_input)?;
    check_cancelled(cancelled)?;

    if plan.kind == TransitionKind::Gapless {
        return render_gapless_preview_inputs_with_cancel(
            outgoing_input,
            incoming_input,
            outgoing,
            incoming,
            plan,
            quality,
            outgoing_normalization_gain,
            incoming_normalization_gain,
            output_rate,
            cancelled,
        );
    }

    let peak_guard = AutoMixPeakGuard::from_analyses_with_base_gains(
        outgoing,
        incoming,
        outgoing_normalization_gain,
        incoming_normalization_gain,
    );
    let output_frames = duration_frames(plan.duration, output_rate).max(1);
    let incoming_source_duration = plan.tempo_envelope.map_or(plan.duration, |envelope| {
        envelope.source_elapsed(plan.duration)
    });

    let mut outgoing = decode_segment(
        outgoing_input,
        plan.outgoing_start,
        plan.duration,
        PREVIEW_CHANNELS,
        cancelled,
    )?;
    ensure_frames_with_cancel(
        &mut outgoing.samples,
        output_frames,
        PREVIEW_CHANNELS,
        cancelled,
    )?;
    if outgoing.sample_rate != output_rate {
        outgoing.samples = resample_interleaved_with_cancel(
            &outgoing.samples,
            outgoing.sample_rate,
            output_rate,
            PREVIEW_CHANNELS,
            cancelled,
        )?;
    }
    check_cancelled(cancelled)?;
    ensure_frames_with_cancel(
        &mut outgoing.samples,
        output_frames,
        PREVIEW_CHANNELS,
        cancelled,
    )?;
    outgoing.samples.truncate(output_frames * PREVIEW_CHANNELS);
    check_cancelled(cancelled)?;

    let incoming_duration = incoming_source_duration.saturating_add(TEMPO_PREVIEW_PADDING);
    let mut incoming = decode_segment(
        incoming_input,
        plan.incoming_start,
        incoming_duration,
        PREVIEW_CHANNELS,
        cancelled,
    )?;
    incoming.samples = render_incoming_deck_with_cancel(
        incoming.samples,
        incoming.sample_rate,
        plan.duration,
        plan.tempo_envelope,
        cancelled,
    )?;
    if incoming.sample_rate != output_rate {
        incoming.samples = resample_interleaved_with_cancel(
            &incoming.samples,
            incoming.sample_rate,
            output_rate,
            PREVIEW_CHANNELS,
            cancelled,
        )?;
    }
    check_cancelled(cancelled)?;
    ensure_frames_with_cancel(
        &mut incoming.samples,
        output_frames,
        PREVIEW_CHANNELS,
        cancelled,
    )?;
    incoming.samples.truncate(output_frames * PREVIEW_CHANNELS);
    check_cancelled(cancelled)?;

    apply_equalizer_with_cancel(
        &mut outgoing.samples,
        output_rate,
        OutputTimeline::trimmed(plan.outgoing_start),
        EqTransition {
            id: 1,
            source_start: plan.outgoing_start,
            duration: plan.duration,
            role: EqTransitionRole::Outgoing,
            harmonic_compatibility: plan.harmonic_compatibility,
        },
        cancelled,
    )?;
    apply_equalizer_with_cancel(
        &mut incoming.samples,
        output_rate,
        if let Some(envelope) = plan.tempo_envelope {
            OutputTimeline::stretched(plan.incoming_start, envelope)
        } else {
            OutputTimeline::trimmed(plan.incoming_start)
        },
        EqTransition {
            id: 1,
            source_start: plan.incoming_start,
            duration: incoming_source_duration,
            role: EqTransitionRole::Incoming,
            harmonic_compatibility: plan.harmonic_compatibility,
        },
        cancelled,
    )?;

    let mixed = automix_mix_with_cancel(
        &outgoing.samples,
        &incoming.samples,
        PREVIEW_CHANNELS,
        plan.kind,
        outgoing_normalization_gain,
        incoming_normalization_gain,
        peak_guard,
        cancelled,
    )?;
    let render_metrics =
        preview_render_metrics_with_cancel(&mixed, output_rate, PREVIEW_CHANNELS, cancelled)?;
    let render_issues = preview_render_issues(render_metrics);
    let wav = encode_wav_i16_with_cancel(&mixed, output_rate, PREVIEW_CHANNELS as u16, cancelled)?;
    check_cancelled(cancelled)?;

    Ok(AutoMixPreview {
        plan,
        quality,
        render_metrics,
        render_issues,
        outgoing_normalization_gain,
        incoming_normalization_gain,
        sample_rate: output_rate,
        channels: PREVIEW_CHANNELS as u16,
        wav,
    })
}

#[allow(clippy::too_many_arguments)]
fn render_gapless_preview_inputs_with_cancel(
    outgoing_input: Input,
    incoming_input: Input,
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    plan: TransitionPlan,
    quality: AutoMixQualityReport,
    outgoing_normalization_gain: f32,
    incoming_normalization_gain: f32,
    output_rate: u32,
    cancelled: &AtomicBool,
) -> Result<AutoMixPreview, AutoMixPreviewError> {
    check_cancelled(cancelled)?;
    let (outgoing_start, outgoing_duration) = gapless_outgoing_segment(&plan, outgoing);
    let (incoming_start, incoming_duration) = gapless_incoming_segment(&plan, incoming);

    let mut outgoing = decode_segment(
        outgoing_input,
        outgoing_start,
        outgoing_duration,
        PREVIEW_CHANNELS,
        cancelled,
    )?;
    if outgoing.sample_rate != output_rate {
        outgoing.samples = resample_interleaved_with_cancel(
            &outgoing.samples,
            outgoing.sample_rate,
            output_rate,
            PREVIEW_CHANNELS,
            cancelled,
        )?;
    }
    apply_linear_gain_with_cancel(
        &mut outgoing.samples,
        outgoing_normalization_gain,
        cancelled,
    )?;

    let mut incoming = decode_segment(
        incoming_input,
        incoming_start,
        incoming_duration,
        PREVIEW_CHANNELS,
        cancelled,
    )?;
    if incoming.sample_rate != output_rate {
        incoming.samples = resample_interleaved_with_cancel(
            &incoming.samples,
            incoming.sample_rate,
            output_rate,
            PREVIEW_CHANNELS,
            cancelled,
        )?;
    }
    apply_linear_gain_with_cancel(
        &mut incoming.samples,
        incoming_normalization_gain,
        cancelled,
    )?;

    let mut rendered = outgoing.samples;
    check_cancelled(cancelled)?;
    rendered.reserve(incoming.samples.len());
    for chunk in incoming.samples.chunks(8192) {
        check_cancelled(cancelled)?;
        rendered.extend_from_slice(chunk);
    }
    let render_metrics =
        preview_render_metrics_with_cancel(&rendered, output_rate, PREVIEW_CHANNELS, cancelled)?;
    let render_issues = preview_render_issues(render_metrics);
    let wav =
        encode_wav_i16_with_cancel(&rendered, output_rate, PREVIEW_CHANNELS as u16, cancelled)?;
    check_cancelled(cancelled)?;

    Ok(AutoMixPreview {
        plan,
        quality,
        render_metrics,
        render_issues,
        outgoing_normalization_gain,
        incoming_normalization_gain,
        sample_rate: output_rate,
        channels: PREVIEW_CHANNELS as u16,
        wav,
    })
}

fn gapless_outgoing_segment(
    plan: &TransitionPlan,
    analysis: &TrackAnalysis,
) -> (Duration, Duration) {
    let boundary = plan.outgoing_start.min(analysis.duration);
    let duration = boundary.min(GAPLESS_PREVIEW_SIDE);
    (boundary.saturating_sub(duration), duration)
}

fn gapless_incoming_segment(
    plan: &TransitionPlan,
    analysis: &TrackAnalysis,
) -> (Duration, Duration) {
    let start = plan.incoming_start.min(analysis.duration);
    let duration = analysis
        .duration
        .saturating_sub(start)
        .min(GAPLESS_PREVIEW_SIDE);
    (start, duration)
}

fn apply_linear_gain_with_cancel(
    samples: &mut [f32],
    gain: f32,
    cancelled: &AtomicBool,
) -> Result<(), AutoMixPreviewError> {
    for (index, sample) in samples.iter_mut().enumerate() {
        if index % 8192 == 0 {
            check_cancelled(cancelled)?;
        }
        *sample *= gain;
    }
    check_cancelled(cancelled)
}

struct DecodedSegment {
    samples: Vec<f32>,
    sample_rate: u32,
}

fn parsed_sample_rate(input: &Input) -> Result<u32, AutoMixPreviewError> {
    let Input::Live(LiveInput::Parsed(parsed), _) = input else {
        return Err(AutoMixPreviewError::UnparsedInput);
    };
    parsed
        .decoder
        .codec_params()
        .sample_rate
        .ok_or(AutoMixPreviewError::MissingSampleRate)
}

fn decode_segment(
    input: Input,
    source_start: Duration,
    duration: Duration,
    output_channels: usize,
    cancelled: &AtomicBool,
) -> Result<DecodedSegment, AutoMixPreviewError> {
    check_cancelled(cancelled)?;
    let Input::Live(LiveInput::Parsed(mut parsed), _) = input else {
        return Err(AutoMixPreviewError::UnparsedInput);
    };
    let sample_rate = parsed
        .decoder
        .codec_params()
        .sample_rate
        .ok_or(AutoMixPreviewError::MissingSampleRate)?;
    if duration.is_zero() {
        return Err(AutoMixPreviewError::SegmentUnavailable);
    }
    let skip_frames = duration_frames(source_start, sample_rate);
    let needed_frames = duration_frames(duration, sample_rate).max(1);
    let mut skipped = 0_usize;
    let mut output = Vec::with_capacity(needed_frames.saturating_mul(output_channels));
    let mut normalized = Vec::new();

    while output.len() / output_channels < needed_frames {
        check_cancelled(cancelled)?;
        let packet = match parsed.format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                check_cancelled(cancelled)?;
                break;
            }
            Err(error) => {
                if cancelled.load(Ordering::Acquire) {
                    return Err(AutoMixPreviewError::Cancelled);
                }
                return Err(AutoMixPreviewError::Decode(error.to_string()));
            }
        };
        check_cancelled(cancelled)?;
        if packet.track_id() != parsed.track_id {
            continue;
        }
        let decoded = match parsed.decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(error) => {
                if cancelled.load(Ordering::Acquire) {
                    return Err(AutoMixPreviewError::Cancelled);
                }
                return Err(AutoMixPreviewError::Decode(error.to_string()));
            }
        };
        check_cancelled(cancelled)?;
        let input_channels = decoded.spec().channels.count();
        if input_channels == 0 {
            return Err(AutoMixPreviewError::Decode(
                "decoded audio has no channels".to_owned(),
            ));
        }
        let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        buffer.copy_interleaved_ref(decoded);
        let available_frames = buffer.samples().len() / input_channels;
        if skipped.saturating_add(available_frames) <= skip_frames {
            skipped += available_frames;
            continue;
        }
        let frame_offset = skip_frames.saturating_sub(skipped);
        skipped += frame_offset;
        let samples = &buffer.samples()[frame_offset * input_channels..];
        let samples = normalize_channels_with_cancel(
            samples,
            input_channels,
            output_channels,
            &mut normalized,
            cancelled,
        )?;
        for chunk in samples.chunks(8192) {
            check_cancelled(cancelled)?;
            output.extend_from_slice(chunk);
        }
    }

    if output.is_empty() {
        return Err(AutoMixPreviewError::SegmentUnavailable);
    }
    output.truncate(needed_frames.saturating_mul(output_channels));
    Ok(DecodedSegment {
        samples: output,
        sample_rate,
    })
}

#[cfg_attr(not(test), allow(dead_code))]
fn render_incoming_deck(
    samples: Vec<f32>,
    sample_rate: u32,
    duration: Duration,
    envelope: Option<wotoha_core::automix::TempoEnvelope>,
) -> Result<Vec<f32>, AutoMixPreviewError> {
    let cancelled = AtomicBool::new(false);
    render_incoming_deck_with_cancel(samples, sample_rate, duration, envelope, &cancelled)
}

fn render_incoming_deck_with_cancel(
    samples: Vec<f32>,
    sample_rate: u32,
    duration: Duration,
    envelope: Option<wotoha_core::automix::TempoEnvelope>,
    cancelled: &AtomicBool,
) -> Result<Vec<f32>, AutoMixPreviewError> {
    check_cancelled(cancelled)?;
    let Some(envelope) = envelope else {
        return Ok(samples);
    };
    let mut processor = TempoStretchProcessor::new(
        120.0,
        120.0 * f64::from(envelope.initial_speed),
        sample_rate,
        PREVIEW_CHANNELS,
        envelope,
    )
    .map_err(|error| AutoMixPreviewError::TempoStretch(error.to_string()))?;
    let mut output = Vec::with_capacity(samples.len());
    for chunk in samples.chunks(PREVIEW_CHANNELS * 1024) {
        check_cancelled(cancelled)?;
        processor
            .process_into(chunk, &mut output)
            .map_err(|error| AutoMixPreviewError::TempoStretch(error.to_string()))?;
    }
    processor
        .flush_into(&mut output)
        .map_err(|error| AutoMixPreviewError::TempoStretch(error.to_string()))?;
    check_cancelled(cancelled)?;
    // The shared processor decision reports effective latency in frames;
    // consume it as complete interleaved frames, just like runtime chunks.
    let mut discard_frames = effective_latency_frames(envelope, processor.latency_frames());
    let skip = discard_interleaved_frames(&output, &mut discard_frames, PREVIEW_CHANNELS)
        .map_err(|error| AutoMixPreviewError::TempoStretch(error.to_string()))?;
    check_cancelled(cancelled)?;
    output.drain(..skip);
    check_cancelled(cancelled)?;
    let frames = duration_frames(duration, sample_rate);
    ensure_frames_with_cancel(&mut output, frames, PREVIEW_CHANNELS, cancelled)?;
    output.truncate(frames.saturating_mul(PREVIEW_CHANNELS));
    check_cancelled(cancelled)?;
    Ok(output)
}

fn apply_equalizer_with_cancel(
    samples: &mut [f32],
    sample_rate: u32,
    timeline: OutputTimeline,
    transition: EqTransition,
    cancelled: &AtomicBool,
) -> Result<(), AutoMixPreviewError> {
    let mut equalizer = ThreeBandEqualizer::new(
        EqualizerControl::new(true, Some(transition)),
        sample_rate,
        PREVIEW_CHANNELS,
    );
    let chunk_samples = PREVIEW_CHANNELS * 4096;
    for (chunk_index, chunk) in samples.chunks_mut(chunk_samples).enumerate() {
        check_cancelled(cancelled)?;
        equalizer.process_interleaved(chunk, (chunk_index * 4096) as u64, timeline);
    }
    check_cancelled(cancelled)
}

#[cfg_attr(not(test), allow(dead_code))]
fn automix_mix(
    outgoing: &[f32],
    incoming: &[f32],
    channels: usize,
    kind: wotoha_core::automix::TransitionKind,
    outgoing_base_gain: f32,
    incoming_base_gain: f32,
    peak_guard: AutoMixPeakGuard,
) -> Vec<f32> {
    let cancelled = AtomicBool::new(false);
    automix_mix_with_cancel(
        outgoing,
        incoming,
        channels,
        kind,
        outgoing_base_gain,
        incoming_base_gain,
        peak_guard,
        &cancelled,
    )
    .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn automix_mix_with_cancel(
    outgoing: &[f32],
    incoming: &[f32],
    channels: usize,
    kind: wotoha_core::automix::TransitionKind,
    outgoing_base_gain: f32,
    incoming_base_gain: f32,
    peak_guard: AutoMixPeakGuard,
    cancelled: &AtomicBool,
) -> Result<Vec<f32>, AutoMixPreviewError> {
    check_cancelled(cancelled)?;
    let frames = (outgoing.len() / channels)
        .min(incoming.len() / channels)
        .max(1);
    let last = frames.saturating_sub(1).max(1) as f32;
    let mut output = Vec::with_capacity(frames * channels);
    for frame in 0..frames {
        if frame % 4096 == 0 {
            check_cancelled(cancelled)?;
        }
        let progress = frame as f32 / last;
        let (outgoing_gain, incoming_curve_gain) =
            automix_peak_safe_mix_gains(kind, progress, peak_guard);
        let outgoing_gain = outgoing_base_gain * outgoing_gain;
        let incoming_gain = incoming_base_gain * incoming_curve_gain;
        for channel in 0..channels {
            let index = frame * channels + channel;
            output.push(outgoing[index] * outgoing_gain + incoming[index] * incoming_gain);
        }
    }
    check_cancelled(cancelled)?;
    Ok(output)
}

fn preview_render_metrics_with_cancel(
    samples: &[f32],
    sample_rate: u32,
    channels: usize,
    cancelled: &AtomicBool,
) -> Result<AutoMixPreviewRenderMetrics, AutoMixPreviewError> {
    check_cancelled(cancelled)?;
    let frames = samples.len() / channels.max(1);
    let window_frames = ((sample_rate as usize) / 2).clamp(1, frames.max(1));
    let start = window_rms_with_cancel(samples, channels, 0, window_frames, cancelled)?;
    let mid_start = frames.saturating_sub(window_frames) / 2;
    let mid = window_rms_with_cancel(samples, channels, mid_start, window_frames, cancelled)?;
    let end_start = frames.saturating_sub(window_frames);
    let end = window_rms_with_cancel(samples, channels, end_start, window_frames, cancelled)?;
    let quietest = quietest_window_rms_with_cancel(samples, channels, window_frames, cancelled)?;
    let edge = start.min(end).max(f32::EPSILON);
    let mut peak = 0.0_f32;
    for (index, sample) in samples.iter().enumerate() {
        if index % 8192 == 0 {
            check_cancelled(cancelled)?;
        }
        peak = peak.max(sample.abs());
    }

    Ok(AutoMixPreviewRenderMetrics {
        start_rms_dbfs: dbfs(start),
        mid_rms_dbfs: dbfs(mid),
        end_rms_dbfs: dbfs(end),
        quietest_window_rms_dbfs: dbfs(quietest),
        quietest_to_edge_ratio: quietest / edge,
        mid_to_edge_ratio: mid / edge,
        sample_peak_dbfs: dbfs(peak),
    })
}

fn preview_render_issues(metrics: AutoMixPreviewRenderMetrics) -> Vec<AutoMixPreviewRenderIssue> {
    let mut issues = Vec::new();
    if metrics.quietest_to_edge_ratio < MIN_PREVIEW_QUIETEST_TO_EDGE_RATIO {
        issues.push(AutoMixPreviewRenderIssue::QuietGap {
            ratio: metrics.quietest_to_edge_ratio,
        });
    }
    if metrics.mid_to_edge_ratio < MIN_PREVIEW_MID_TO_EDGE_RATIO {
        issues.push(AutoMixPreviewRenderIssue::MidpointDrop {
            ratio: metrics.mid_to_edge_ratio,
        });
    }
    if metrics.sample_peak_dbfs > MAX_PREVIEW_SAMPLE_PEAK_DBFS {
        issues.push(AutoMixPreviewRenderIssue::ClippingRisk {
            sample_peak_dbfs: metrics.sample_peak_dbfs,
        });
    }
    issues
}

fn quietest_window_rms_with_cancel(
    samples: &[f32],
    channels: usize,
    window_frames: usize,
    cancelled: &AtomicBool,
) -> Result<f32, AutoMixPreviewError> {
    check_cancelled(cancelled)?;
    let frames = samples.len() / channels.max(1);
    if frames == 0 {
        return Ok(0.0);
    }
    let window_frames = window_frames.clamp(1, frames);
    let stride = (window_frames / 4).max(1);
    let mut quietest = f32::INFINITY;
    let mut start = 0;
    while start < frames {
        check_cancelled(cancelled)?;
        quietest = quietest.min(window_rms_with_cancel(
            samples,
            channels,
            start,
            window_frames,
            cancelled,
        )?);
        if start + window_frames >= frames {
            break;
        }
        start += stride;
    }
    Ok(quietest)
}

fn window_rms_with_cancel(
    samples: &[f32],
    channels: usize,
    frame_start: usize,
    window_frames: usize,
    cancelled: &AtomicBool,
) -> Result<f32, AutoMixPreviewError> {
    check_cancelled(cancelled)?;
    let channels = channels.max(1);
    let sample_start = frame_start.saturating_mul(channels).min(samples.len());
    let sample_end = frame_start
        .saturating_add(window_frames)
        .saturating_mul(channels)
        .min(samples.len());
    rms_with_cancel(&samples[sample_start..sample_end], cancelled)
}

fn rms_with_cancel(samples: &[f32], cancelled: &AtomicBool) -> Result<f32, AutoMixPreviewError> {
    check_cancelled(cancelled)?;
    if samples.is_empty() {
        return Ok(0.0);
    }
    let mut sum = 0.0_f32;
    for (index, sample) in samples.iter().enumerate() {
        if index % 8192 == 0 {
            check_cancelled(cancelled)?;
        }
        sum += sample * sample;
    }
    Ok((sum / samples.len() as f32).sqrt())
}

fn dbfs(value: f32) -> f32 {
    if value <= f32::EPSILON {
        -120.0
    } else {
        20.0 * value.log10()
    }
}

fn normalize_channels_with_cancel<'a>(
    samples: &'a [f32],
    input_channels: usize,
    output_channels: usize,
    output: &'a mut Vec<f32>,
    cancelled: &AtomicBool,
) -> Result<&'a [f32], AutoMixPreviewError> {
    check_cancelled(cancelled)?;
    if input_channels == 0 || output_channels == 0 || !samples.len().is_multiple_of(input_channels)
    {
        return Err(AutoMixPreviewError::Decode(
            "invalid decoded channel layout".to_owned(),
        ));
    }
    if input_channels == output_channels {
        return Ok(samples);
    }
    output.clear();
    output.reserve(samples.len() / input_channels * output_channels);
    for (index, frame) in samples.chunks_exact(input_channels).enumerate() {
        if index % 4096 == 0 {
            check_cancelled(cancelled)?;
        }
        match output_channels {
            1 => output.push(frame.iter().copied().sum::<f32>() / input_channels as f32),
            2 if input_channels == 1 => output.extend_from_slice(&[frame[0], frame[0]]),
            2 => output.extend_from_slice(&frame[..2]),
            _ => {
                return Err(AutoMixPreviewError::Decode(format!(
                    "unsupported output channel count: {output_channels}"
                )));
            }
        }
    }
    check_cancelled(cancelled)?;
    Ok(output)
}

fn resample_interleaved_with_cancel(
    samples: &[f32],
    input_rate: u32,
    output_rate: u32,
    channels: usize,
    cancelled: &AtomicBool,
) -> Result<Vec<f32>, AutoMixPreviewError> {
    check_cancelled(cancelled)?;
    if input_rate == output_rate || samples.is_empty() {
        return Ok(samples.to_vec());
    }
    let input_frames = samples.len() / channels;
    let output_frames =
        ((input_frames as f64 * f64::from(output_rate)) / f64::from(input_rate)).round() as usize;
    let mut output = Vec::with_capacity(output_frames * channels);
    for frame in 0..output_frames {
        if frame % 4096 == 0 {
            check_cancelled(cancelled)?;
        }
        let source = frame as f64 * f64::from(input_rate) / f64::from(output_rate);
        let left = source.floor() as usize;
        let right = (left + 1).min(input_frames.saturating_sub(1));
        let frac = (source - left as f64) as f32;
        for channel in 0..channels {
            let a = samples[left * channels + channel];
            let b = samples[right * channels + channel];
            output.push(a + (b - a) * frac);
        }
    }
    check_cancelled(cancelled)?;
    Ok(output)
}

fn ensure_frames(samples: &mut Vec<f32>, frames: usize, channels: usize) {
    let target = frames.saturating_mul(channels);
    if samples.len() < target {
        samples.resize(target, 0.0);
    }
}

fn ensure_frames_with_cancel(
    samples: &mut Vec<f32>,
    frames: usize,
    channels: usize,
    cancelled: &AtomicBool,
) -> Result<(), AutoMixPreviewError> {
    check_cancelled(cancelled)?;
    ensure_frames(samples, frames, channels);
    check_cancelled(cancelled)
}

fn duration_frames(duration: Duration, sample_rate: u32) -> usize {
    (duration.as_secs_f64() * f64::from(sample_rate)).round() as usize
}

#[cfg_attr(not(test), allow(dead_code))]
fn encode_wav_i16(samples: &[f32], sample_rate: u32, channels: u16) -> Vec<u8> {
    let cancelled = AtomicBool::new(false);
    encode_wav_i16_with_cancel(samples, sample_rate, channels, &cancelled).unwrap_or_default()
}

fn encode_wav_i16_with_cancel(
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>, AutoMixPreviewError> {
    check_cancelled(cancelled)?;
    let data_len = samples.len() * size_of::<i16>();
    let mut wav = Vec::with_capacity(44 + data_len);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36_u32.saturating_add(data_len as u32)).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    let byte_rate = sample_rate * u32::from(channels) * size_of::<i16>() as u32;
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    let block_align = channels * size_of::<i16>() as u16;
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_len as u32).to_le_bytes());
    for (index, sample) in samples.iter().enumerate() {
        if index % 8192 == 0 {
            check_cancelled(cancelled)?;
        }
        let sample = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    check_cancelled(cancelled)?;
    Ok(wav)
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), AutoMixPreviewError> {
    if cancelled.load(Ordering::Acquire) {
        Err(AutoMixPreviewError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use songbird::input::codecs::{get_codec_registry, get_probe};
    use wotoha_core::automix::{TransitionKind, plan_guarded_transition, plan_transition};

    #[tokio::test]
    async fn renders_beatmatched_preview_wav_from_generated_audio() {
        let sample_rate = 48_000;
        let duration = Duration::from_secs(16);
        let outgoing = Input::from(click_track_wav(sample_rate, duration, 120.0))
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .unwrap();
        let incoming = Input::from(click_track_wav(sample_rate, duration, 124.0))
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .unwrap();
        let outgoing_analysis = beat_analysis(duration, 120.0);
        let incoming_analysis = beat_analysis(duration, 124.0);
        let config = AutoMixConfig {
            enabled: true,
            crossfade: Duration::from_secs(4),
            max_tempo_adjustment: 0.06,
            min_beat_confidence: 0.7,
        };
        let loudness = LoudnessConfig {
            enabled: false,
            target_lufs: -16.0,
            max_boost_db: 6.0,
            true_peak_ceiling_dbtp: -2.0,
        };
        let raw_plan = plan_transition(&outgoing_analysis, &incoming_analysis, &config);
        assert_eq!(
            raw_plan.kind,
            TransitionKind::BeatMatched,
            "raw plan={raw_plan:?}"
        );
        let guarded = plan_guarded_transition(&outgoing_analysis, &incoming_analysis, &config);
        assert_eq!(
            guarded.plan.kind,
            TransitionKind::BeatMatched,
            "guarded plan={:?}",
            guarded.plan
        );
        assert!(guarded.quality.beat_pairs_checked >= 8);
        assert!(
            guarded
                .quality
                .beat_phase_coverage
                .is_some_and(|coverage| coverage >= 0.65)
        );

        let preview = render_automix_preview_inputs(
            outgoing,
            incoming,
            &outgoing_analysis,
            &incoming_analysis,
            &config,
            &loudness,
        )
        .unwrap();

        assert_eq!(preview.plan.kind, TransitionKind::BeatMatched);
        assert_eq!(preview.sample_rate, sample_rate);
        assert_eq!(preview.channels, 2);
        assert!(preview.quality.is_ok());
        assert!(preview.render_issues.is_empty());
        assert!(preview.render_metrics.quietest_to_edge_ratio > 0.25);
        assert!(preview.render_metrics.mid_to_edge_ratio > 0.5);
        assert!(preview.render_metrics.sample_peak_dbfs.is_finite());
        assert_eq!(preview.outgoing_normalization_gain, 1.0);
        assert_eq!(preview.incoming_normalization_gain, 1.0);
        assert!(preview.wav.starts_with(b"RIFF"));
        assert!(preview.wav.len() > 44);
    }

    #[tokio::test]
    async fn gapless_preview_concatenates_normalized_boundary_audio() {
        let sample_rate = 8_000;
        let duration = Duration::from_secs(1);
        let outgoing = Input::from(constant_wav(sample_rate, duration, 0.8))
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .unwrap();
        let incoming = Input::from(constant_wav(sample_rate, duration, 0.8))
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .unwrap();
        let mut outgoing_analysis = beat_analysis(duration, 120.0);
        outgoing_analysis.integrated_lufs = Some(-9.9794);
        outgoing_analysis.true_peak_dbtp = Some(-1.0);
        let mut incoming_analysis = beat_analysis(duration, 120.0);
        incoming_analysis.integrated_lufs = Some(-3.9588);
        incoming_analysis.true_peak_dbtp = Some(-1.0);
        let config = AutoMixConfig {
            enabled: false,
            crossfade: Duration::from_secs(4),
            max_tempo_adjustment: 0.06,
            min_beat_confidence: 0.7,
        };
        let loudness = LoudnessConfig {
            enabled: true,
            target_lufs: -16.0,
            max_boost_db: 6.0,
            true_peak_ceiling_dbtp: -2.0,
        };

        let preview = render_automix_preview_inputs(
            outgoing,
            incoming,
            &outgoing_analysis,
            &incoming_analysis,
            &config,
            &loudness,
        )
        .unwrap();

        assert_eq!(preview.plan.kind, TransitionKind::Gapless);
        assert_eq!(preview.plan.duration, Duration::ZERO);
        assert_eq!(preview.sample_rate, sample_rate);
        assert_eq!(preview.channels, PREVIEW_CHANNELS as u16);
        assert!((preview.outgoing_normalization_gain - 0.5).abs() < 0.0001);
        assert!((preview.incoming_normalization_gain - 0.25).abs() < 0.0001);

        let pcm = preview
            .wav
            .get(44..)
            .unwrap()
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
            .collect::<Vec<_>>();
        let side_samples = sample_rate as usize * PREVIEW_CHANNELS;
        assert_eq!(pcm.len(), side_samples * 2);
        let expected_outgoing = (0.8 * 0.5 * f32::from(i16::MAX)) as i16;
        let expected_incoming = (0.8 * 0.25 * f32::from(i16::MAX)) as i16;
        assert!((pcm[side_samples - 1] - expected_outgoing).abs() <= 2);
        assert!((pcm[side_samples] - expected_incoming).abs() <= 2);
        assert!(rms_i16(&pcm[..side_samples]) > 1_000.0);
        assert!(rms_i16(&pcm[side_samples..]) > 1_000.0);
        assert!(preview.render_metrics.start_rms_dbfs.is_finite());
        assert!(preview.render_metrics.end_rms_dbfs.is_finite());
        assert!(preview.render_metrics.sample_peak_dbfs.is_finite());
        assert!(preview.render_issues.is_empty());
    }

    #[tokio::test]
    async fn pre_cancelled_preview_fails_without_a_wav_result() {
        let sample_rate = 8_000;
        let duration = Duration::from_secs(1);
        let outgoing = Input::from(constant_wav(sample_rate, duration, 0.5))
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .unwrap();
        let incoming = Input::from(constant_wav(sample_rate, duration, 0.5))
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .unwrap();
        let outgoing_analysis = beat_analysis(duration, 120.0);
        let incoming_analysis = beat_analysis(duration, 120.0);
        let config = AutoMixConfig {
            enabled: false,
            crossfade: Duration::from_secs(1),
            max_tempo_adjustment: 0.06,
            min_beat_confidence: 0.7,
        };
        let loudness = LoudnessConfig {
            enabled: false,
            target_lufs: -16.0,
            max_boost_db: 6.0,
            true_peak_ceiling_dbtp: -2.0,
        };
        let cancelled = AtomicBool::new(true);

        let result = render_automix_preview_inputs_with_cancel(
            outgoing,
            incoming,
            &outgoing_analysis,
            &incoming_analysis,
            &config,
            &loudness,
            &cancelled,
        );

        assert!(matches!(result, Err(AutoMixPreviewError::Cancelled)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_worker_returns_boundedly_without_blocking_current_thread_timer() {
        use std::sync::Arc;

        let frames = 4_000_000;
        let outgoing = vec![0.25; frames * PREVIEW_CHANNELS];
        let incoming = vec![0.25; frames * PREVIEW_CHANNELS];
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let started = Arc::new(AtomicBool::new(false));
        let worker_started = started.clone();
        let worker = tokio::task::spawn_blocking(move || {
            worker_started.store(true, Ordering::Release);
            automix_mix_with_cancel(
                &outgoing,
                &incoming,
                PREVIEW_CHANNELS,
                TransitionKind::Crossfade,
                1.0,
                1.0,
                AutoMixPeakGuard::new(1.0, 1.0),
                &worker_cancelled,
            )
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            while !started.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
            cancelled.store(true, Ordering::Release);
        })
        .await
        .expect("preview worker should start promptly");

        let timer = tokio::time::timeout(
            Duration::from_millis(100),
            tokio::time::sleep(Duration::from_millis(1)),
        )
        .await;
        assert!(
            timer.is_ok(),
            "current-thread timer should remain responsive"
        );
        let result = tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("cancelled preview worker should exit promptly")
            .expect("preview worker should not panic");
        assert!(matches!(result, Err(AutoMixPreviewError::Cancelled)));
    }

    #[test]
    fn gapless_preview_ranges_clamp_to_extremely_short_tracks() {
        let mut outgoing = beat_analysis(Duration::from_millis(5), 120.0);
        outgoing.audible_end = Duration::from_millis(20);
        let mut incoming = beat_analysis(Duration::from_millis(5), 120.0);
        incoming.audible_start = Duration::from_millis(4);
        let plan = TransitionPlan {
            kind: TransitionKind::Gapless,
            outgoing_start: outgoing.audible_end,
            incoming_start: incoming.audible_start,
            incoming_cue_selection: None,
            duration: Duration::ZERO,
            incoming_tempo_ratio: 1.0,
            harmonic_compatibility: None,
            incoming_gain: 1.0,
            tempo_envelope: None,
            energy_selection: None,
        };

        assert_eq!(
            gapless_outgoing_segment(&plan, &outgoing),
            (Duration::ZERO, Duration::from_millis(5))
        );
        assert_eq!(
            gapless_incoming_segment(&plan, &incoming),
            (Duration::from_millis(4), Duration::from_millis(1))
        );
    }

    #[test]
    fn render_issues_classify_gap_drop_and_clipping() {
        let issues = preview_render_issues(AutoMixPreviewRenderMetrics {
            start_rms_dbfs: -12.0,
            mid_rms_dbfs: -30.0,
            end_rms_dbfs: -12.0,
            quietest_window_rms_dbfs: -36.0,
            quietest_to_edge_ratio: 0.10,
            mid_to_edge_ratio: 0.40,
            sample_peak_dbfs: 0.0,
        });

        assert!(matches!(
            issues.as_slice(),
            [
                AutoMixPreviewRenderIssue::QuietGap { .. },
                AutoMixPreviewRenderIssue::MidpointDrop { .. },
                AutoMixPreviewRenderIssue::ClippingRisk { .. },
            ]
        ));
    }

    #[test]
    fn preview_mix_applies_normalization_gains_once() {
        let outgoing = vec![1.0, 1.0, 1.0, 1.0];
        let incoming = vec![1.0, 1.0, 1.0, 1.0];
        let mixed = automix_mix(
            &outgoing,
            &incoming,
            2,
            TransitionKind::Crossfade,
            0.25,
            0.5,
            AutoMixPeakGuard::new(0.25, 0.5),
        );

        assert_eq!(&mixed[..2], &[0.25, 0.25]);
        assert_eq!(&mixed[2..], &[0.5, 0.5]);
    }

    #[test]
    fn preview_and_runtime_timestretch_share_the_exact_stereo_latency_boundary() {
        let envelope = wotoha_core::automix::TempoEnvelope::new(
            1.0,
            1.0,
            Duration::from_secs(20),
            Duration::ZERO,
        );
        for sample_rate in [44_100, 48_000] {
            let duration = Duration::from_secs(2);
            let samples = stereo_kick_track(sample_rate, duration);
            let preview =
                render_incoming_deck(samples.clone(), sample_rate, duration, Some(envelope))
                    .unwrap();
            let runtime = stretch_runtime_pcm(samples, sample_rate, duration, envelope);

            assert_eq!(preview.len(), runtime.len(), "{sample_rate}Hz");
            assert_eq!(preview, runtime, "{sample_rate}Hz");
            let first_kick = first_stereo_frame_above(&preview, 0.1)
                .expect("the preview should retain the kick after latency removal");
            assert_stereo_kick_sides(&preview, first_kick, sample_rate);
        }
    }

    fn beat_analysis(duration: Duration, bpm: f32) -> TrackAnalysis {
        let interval = Duration::from_secs_f32(60.0 / bpm);
        let mut markers = Vec::new();
        let mut position = Duration::ZERO;
        while position <= duration {
            markers.push(position);
            position += interval;
        }
        let vocal_bins = (duration.as_secs_f64() * 10.0).ceil() as usize;
        TrackAnalysis {
            duration,
            audible_start: Duration::ZERO,
            audible_end: duration,
            intro_end: Some(Duration::from_secs(4)),
            intro_confidence: 1.0,
            outro_start: Some(duration.saturating_sub(Duration::from_secs(4))),
            outro_confidence: 1.0,
            vocal_activity: vec![0; vocal_bins],
            vocal_activity_confidences: vec![255; vocal_bins],
            vocal_activity_rate: 10,
            energy_profile: Vec::new(),
            energy_profile_rate: 0,
            bpm: Some(bpm),
            beat_confidence: 1.0,
            first_beat: Some(Duration::ZERO),
            beat_markers: markers.clone(),
            beat_marker_confidences: vec![1.0; markers.len()],
            first_downbeat: None,
            downbeat_confidence: 0.0,
            musical_key: None,
            rms_dbfs: Some(-12.0),
            sample_peak_dbfs: Some(-3.0),
            integrated_lufs: None,
            true_peak_dbtp: None,
        }
    }

    fn click_track_wav(sample_rate: u32, duration: Duration, bpm: f32) -> Vec<u8> {
        let frames = duration_frames(duration, sample_rate);
        let interval = 60.0 / bpm;
        let samples = (0..frames)
            .map(|frame| {
                let seconds = frame as f32 / sample_rate as f32;
                let nearest = (seconds / interval).round() * interval;
                let envelope = (-((seconds - nearest).abs() / 0.018).powi(2)).exp();
                ((std::f32::consts::TAU * 70.0 * seconds).sin() * envelope * 0.8
                    + (std::f32::consts::TAU * 440.0 * seconds).sin() * 0.04)
                    .clamp(-1.0, 1.0)
            })
            .collect::<Vec<_>>();
        encode_wav_i16(&samples, sample_rate, 1)
    }

    fn constant_wav(sample_rate: u32, duration: Duration, amplitude: f32) -> Vec<u8> {
        let samples = vec![amplitude; duration_frames(duration, sample_rate)];
        encode_wav_i16(&samples, sample_rate, 1)
    }

    fn stereo_kick_track(sample_rate: u32, duration: Duration) -> Vec<f32> {
        let frames = duration_frames(duration, sample_rate);
        let kick_start = sample_rate as usize / 2;
        let kick_frames = sample_rate as usize / 40;
        let mut samples = vec![0.0; frames * PREVIEW_CHANNELS];
        for frame in 0..kick_frames {
            let amplitude = 1.0 - frame as f32 / kick_frames as f32;
            let kick = (std::f32::consts::TAU * 65.0 * frame as f32 / sample_rate as f32).sin()
                * amplitude;
            let index = (kick_start + frame) * PREVIEW_CHANNELS;
            samples[index] = kick;
            samples[index + 1] = -kick * 0.5;
        }
        samples
    }

    fn stretch_runtime_pcm(
        samples: Vec<f32>,
        sample_rate: u32,
        duration: Duration,
        envelope: wotoha_core::automix::TempoEnvelope,
    ) -> Vec<f32> {
        let mut processor = TempoStretchProcessor::new(
            120.0,
            120.0 * f64::from(envelope.initial_speed),
            sample_rate,
            PREVIEW_CHANNELS,
            envelope,
        )
        .unwrap();
        let mut output = Vec::with_capacity(samples.len());
        for chunk in samples.chunks(PREVIEW_CHANNELS * 1024) {
            processor.process_into(chunk, &mut output).unwrap();
        }
        processor.flush_into(&mut output).unwrap();
        let mut discard_frames = processor.latency_frames();
        let skip =
            discard_interleaved_frames(&output, &mut discard_frames, PREVIEW_CHANNELS).unwrap();
        output.drain(..skip);
        let frames = duration_frames(duration, sample_rate);
        ensure_frames(&mut output, frames, PREVIEW_CHANNELS);
        output.truncate(frames * PREVIEW_CHANNELS);
        output
    }

    fn first_stereo_frame_above(samples: &[f32], threshold: f32) -> Option<usize> {
        samples
            .chunks_exact(PREVIEW_CHANNELS)
            .position(|frame| frame.iter().any(|sample| sample.abs() > threshold))
    }

    fn assert_stereo_kick_sides(samples: &[f32], first_kick: usize, sample_rate: u32) {
        let window = samples[first_kick * PREVIEW_CHANNELS..]
            .chunks_exact(PREVIEW_CHANNELS)
            .take(512)
            .collect::<Vec<_>>();
        let left_energy = window.iter().map(|frame| frame[0] * frame[0]).sum::<f32>();
        let right_energy = window.iter().map(|frame| frame[1] * frame[1]).sum::<f32>();
        let cross_energy = window.iter().map(|frame| frame[0] * frame[1]).sum::<f32>();
        assert!(left_energy > 0.1, "{sample_rate}Hz left kick");
        assert!(right_energy > 0.01, "{sample_rate}Hz right kick");
        assert!(cross_energy < 0.0, "{sample_rate}Hz kick polarity");
        let ratio = (right_energy / left_energy).sqrt();
        assert!(
            (0.3..0.7).contains(&ratio),
            "{sample_rate}Hz right/left kick ratio={ratio}"
        );
    }

    fn rms_i16(samples: &[i16]) -> f32 {
        (samples
            .iter()
            .map(|sample| f32::from(*sample).powi(2))
            .sum::<f32>()
            / samples.len() as f32)
            .sqrt()
    }
}
