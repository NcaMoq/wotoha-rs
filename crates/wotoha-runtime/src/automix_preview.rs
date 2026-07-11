use std::time::Duration;

use songbird::input::{Input, LiveInput};
use symphonia::core::{audio::SampleBuffer, errors::Error as SymphoniaError};
use thiserror::Error;
use wotoha_core::automix::{
    AutoMixConfig, AutoMixQualityReport, EqTransition, EqTransitionRole, TrackAnalysis,
    TransitionPlan, automix_mix_gains, plan_guarded_transition,
};

use crate::{
    tempo_stretch::TempoStretchProcessor,
    transition_dsp::{EqualizerControl, OutputTimeline, ThreeBandEqualizer},
};

const PREVIEW_CHANNELS: usize = 2;
const TEMPO_PREVIEW_PADDING: Duration = Duration::from_secs(1);
const MIN_PREVIEW_QUIETEST_TO_EDGE_RATIO: f32 = 0.35;
const MIN_PREVIEW_MID_TO_EDGE_RATIO: f32 = 0.60;
const MAX_PREVIEW_SAMPLE_PEAK_DBFS: f32 = -0.01;

pub struct AutoMixPreview {
    pub plan: TransitionPlan,
    pub quality: AutoMixQualityReport,
    pub render_metrics: AutoMixPreviewRenderMetrics,
    pub render_issues: Vec<AutoMixPreviewRenderIssue>,
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
}

pub(crate) fn render_automix_preview_inputs(
    outgoing_input: Input,
    incoming_input: Input,
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
) -> Result<AutoMixPreview, AutoMixPreviewError> {
    let guarded = plan_guarded_transition(outgoing, incoming, config);
    let plan = guarded.plan;
    let quality = guarded.quality;
    let output_rate = parsed_sample_rate(&outgoing_input)?;
    let output_frames = duration_frames(plan.duration, output_rate).max(1);
    let incoming_source_duration = plan.tempo_envelope.map_or(plan.duration, |envelope| {
        envelope.source_elapsed(plan.duration)
    });

    let mut outgoing = decode_segment(
        outgoing_input,
        plan.outgoing_start,
        plan.duration,
        PREVIEW_CHANNELS,
    )?;
    ensure_frames(&mut outgoing.samples, output_frames, PREVIEW_CHANNELS);
    if outgoing.sample_rate != output_rate {
        outgoing.samples = resample_interleaved(
            &outgoing.samples,
            outgoing.sample_rate,
            output_rate,
            PREVIEW_CHANNELS,
        );
    }
    ensure_frames(&mut outgoing.samples, output_frames, PREVIEW_CHANNELS);
    outgoing.samples.truncate(output_frames * PREVIEW_CHANNELS);

    let incoming_duration = incoming_source_duration.saturating_add(TEMPO_PREVIEW_PADDING);
    let mut incoming = decode_segment(
        incoming_input,
        plan.incoming_start,
        incoming_duration,
        PREVIEW_CHANNELS,
    )?;
    incoming.samples = render_incoming_deck(
        incoming.samples,
        incoming.sample_rate,
        plan.duration,
        plan.tempo_envelope,
    )?;
    if incoming.sample_rate != output_rate {
        incoming.samples = resample_interleaved(
            &incoming.samples,
            incoming.sample_rate,
            output_rate,
            PREVIEW_CHANNELS,
        );
    }
    ensure_frames(&mut incoming.samples, output_frames, PREVIEW_CHANNELS);
    incoming.samples.truncate(output_frames * PREVIEW_CHANNELS);

    apply_equalizer(
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
    );
    apply_equalizer(
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
    );

    let mixed = automix_mix(
        &outgoing.samples,
        &incoming.samples,
        PREVIEW_CHANNELS,
        plan.kind,
        plan.incoming_gain,
    );
    let render_metrics = preview_render_metrics(&mixed, output_rate, PREVIEW_CHANNELS);
    let render_issues = preview_render_issues(render_metrics);

    Ok(AutoMixPreview {
        plan,
        quality,
        render_metrics,
        render_issues,
        sample_rate: output_rate,
        channels: PREVIEW_CHANNELS as u16,
        wav: encode_wav_i16(&mixed, output_rate, PREVIEW_CHANNELS as u16),
    })
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
) -> Result<DecodedSegment, AutoMixPreviewError> {
    let Input::Live(LiveInput::Parsed(mut parsed), _) = input else {
        return Err(AutoMixPreviewError::UnparsedInput);
    };
    let sample_rate = parsed
        .decoder
        .codec_params()
        .sample_rate
        .ok_or(AutoMixPreviewError::MissingSampleRate)?;
    let skip_frames = duration_frames(source_start, sample_rate);
    let needed_frames = duration_frames(duration, sample_rate);
    let mut skipped = 0_usize;
    let mut output = Vec::with_capacity(needed_frames.saturating_mul(output_channels));
    let mut normalized = Vec::new();

    while output.len() / output_channels < needed_frames {
        let packet = match parsed.format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(error) => return Err(AutoMixPreviewError::Decode(error.to_string())),
        };
        if packet.track_id() != parsed.track_id {
            continue;
        }
        let decoded = match parsed.decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(error) => return Err(AutoMixPreviewError::Decode(error.to_string())),
        };
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
        let samples = normalize_channels(samples, input_channels, output_channels, &mut normalized)
            .map_err(AutoMixPreviewError::Decode)?;
        output.extend_from_slice(samples);
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

fn render_incoming_deck(
    samples: Vec<f32>,
    sample_rate: u32,
    duration: Duration,
    envelope: Option<wotoha_core::automix::TempoEnvelope>,
) -> Result<Vec<f32>, AutoMixPreviewError> {
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
        processor
            .process_into(chunk, &mut output)
            .map_err(|error| AutoMixPreviewError::TempoStretch(error.to_string()))?;
    }
    processor
        .flush_into(&mut output)
        .map_err(|error| AutoMixPreviewError::TempoStretch(error.to_string()))?;
    let skip = processor.latency_samples().min(output.len());
    output.drain(..skip);
    let frames = duration_frames(duration, sample_rate);
    ensure_frames(&mut output, frames, PREVIEW_CHANNELS);
    output.truncate(frames.saturating_mul(PREVIEW_CHANNELS));
    Ok(output)
}

fn apply_equalizer(
    samples: &mut [f32],
    sample_rate: u32,
    timeline: OutputTimeline,
    transition: EqTransition,
) {
    let mut equalizer = ThreeBandEqualizer::new(
        EqualizerControl::new(true, Some(transition)),
        sample_rate,
        PREVIEW_CHANNELS,
    );
    equalizer.process_interleaved(samples, 0, timeline);
}

fn automix_mix(
    outgoing: &[f32],
    incoming: &[f32],
    channels: usize,
    kind: wotoha_core::automix::TransitionKind,
    incoming_gain: f32,
) -> Vec<f32> {
    let frames = (outgoing.len() / channels)
        .min(incoming.len() / channels)
        .max(1);
    let last = frames.saturating_sub(1).max(1) as f32;
    let mut output = Vec::with_capacity(frames * channels);
    for frame in 0..frames {
        let progress = frame as f32 / last;
        let (outgoing_gain, incoming_curve_gain) = automix_mix_gains(kind, progress);
        let incoming_gain = incoming_gain * incoming_curve_gain;
        for channel in 0..channels {
            let index = frame * channels + channel;
            output.push(outgoing[index] * outgoing_gain + incoming[index] * incoming_gain);
        }
    }
    output
}

fn preview_render_metrics(
    samples: &[f32],
    sample_rate: u32,
    channels: usize,
) -> AutoMixPreviewRenderMetrics {
    let frames = samples.len() / channels.max(1);
    let window_frames = ((sample_rate as usize) / 2).clamp(1, frames.max(1));
    let start = window_rms(samples, channels, 0, window_frames);
    let mid_start = frames.saturating_sub(window_frames) / 2;
    let mid = window_rms(samples, channels, mid_start, window_frames);
    let end_start = frames.saturating_sub(window_frames);
    let end = window_rms(samples, channels, end_start, window_frames);
    let quietest = quietest_window_rms(samples, channels, window_frames);
    let edge = start.min(end).max(f32::EPSILON);
    let peak = samples
        .iter()
        .map(|sample| sample.abs())
        .fold(0.0_f32, f32::max);

    AutoMixPreviewRenderMetrics {
        start_rms_dbfs: dbfs(start),
        mid_rms_dbfs: dbfs(mid),
        end_rms_dbfs: dbfs(end),
        quietest_window_rms_dbfs: dbfs(quietest),
        quietest_to_edge_ratio: quietest / edge,
        mid_to_edge_ratio: mid / edge,
        sample_peak_dbfs: dbfs(peak),
    }
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

fn quietest_window_rms(samples: &[f32], channels: usize, window_frames: usize) -> f32 {
    let frames = samples.len() / channels.max(1);
    if frames == 0 {
        return 0.0;
    }
    let window_frames = window_frames.clamp(1, frames);
    let stride = (window_frames / 4).max(1);
    let mut quietest = f32::INFINITY;
    let mut start = 0;
    while start < frames {
        quietest = quietest.min(window_rms(samples, channels, start, window_frames));
        if start + window_frames >= frames {
            break;
        }
        start += stride;
    }
    quietest
}

fn window_rms(samples: &[f32], channels: usize, frame_start: usize, window_frames: usize) -> f32 {
    let channels = channels.max(1);
    let sample_start = frame_start.saturating_mul(channels).min(samples.len());
    let sample_end = frame_start
        .saturating_add(window_frames)
        .saturating_mul(channels)
        .min(samples.len());
    rms(&samples[sample_start..sample_end])
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
}

fn dbfs(value: f32) -> f32 {
    if value <= f32::EPSILON {
        -120.0
    } else {
        20.0 * value.log10()
    }
}

fn normalize_channels<'a>(
    samples: &'a [f32],
    input_channels: usize,
    output_channels: usize,
    output: &'a mut Vec<f32>,
) -> Result<&'a [f32], String> {
    if input_channels == 0 || output_channels == 0 || !samples.len().is_multiple_of(input_channels)
    {
        return Err("invalid decoded channel layout".into());
    }
    if input_channels == output_channels {
        return Ok(samples);
    }
    output.clear();
    output.reserve(samples.len() / input_channels * output_channels);
    for frame in samples.chunks_exact(input_channels) {
        match output_channels {
            1 => output.push(frame.iter().copied().sum::<f32>() / input_channels as f32),
            2 if input_channels == 1 => output.extend_from_slice(&[frame[0], frame[0]]),
            2 => output.extend_from_slice(&frame[..2]),
            _ => {
                return Err(format!(
                    "unsupported output channel count: {output_channels}"
                ));
            }
        }
    }
    Ok(output)
}

fn resample_interleaved(
    samples: &[f32],
    input_rate: u32,
    output_rate: u32,
    channels: usize,
) -> Vec<f32> {
    if input_rate == output_rate || samples.is_empty() {
        return samples.to_vec();
    }
    let input_frames = samples.len() / channels;
    let output_frames =
        ((input_frames as f64 * f64::from(output_rate)) / f64::from(input_rate)).round() as usize;
    let mut output = Vec::with_capacity(output_frames * channels);
    for frame in 0..output_frames {
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
    output
}

fn ensure_frames(samples: &mut Vec<f32>, frames: usize, channels: usize) {
    let target = frames.saturating_mul(channels);
    if samples.len() < target {
        samples.resize(target, 0.0);
    }
}

fn duration_frames(duration: Duration, sample_rate: u32) -> usize {
    (duration.as_secs_f64() * f64::from(sample_rate)).round() as usize
}

fn encode_wav_i16(samples: &[f32], sample_rate: u32, channels: u16) -> Vec<u8> {
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
    for sample in samples {
        let sample = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

#[cfg(test)]
mod tests {
    use super::*;
    use songbird::input::codecs::{get_codec_registry, get_probe};
    use wotoha_core::automix::TransitionKind;

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

        let preview = render_automix_preview_inputs(
            outgoing,
            incoming,
            &outgoing_analysis,
            &incoming_analysis,
            &config,
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
        assert!(preview.wav.starts_with(b"RIFF"));
        assert!(preview.wav.len() > 44);
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

    fn beat_analysis(duration: Duration, bpm: f32) -> TrackAnalysis {
        let interval = Duration::from_secs_f32(60.0 / bpm);
        let mut markers = Vec::new();
        let mut position = Duration::ZERO;
        while position <= duration {
            markers.push(position);
            position += interval;
        }
        TrackAnalysis {
            duration,
            audible_start: Duration::ZERO,
            audible_end: duration,
            intro_end: Some(Duration::from_secs(4)),
            intro_confidence: 1.0,
            outro_start: Some(duration.saturating_sub(Duration::from_secs(4))),
            outro_confidence: 1.0,
            vocal_activity: Vec::new(),
            vocal_activity_confidences: Vec::new(),
            vocal_activity_rate: 0,
            energy_profile: Vec::new(),
            energy_profile_rate: 0,
            bpm: Some(bpm),
            beat_confidence: 1.0,
            first_beat: Some(Duration::ZERO),
            beat_markers: markers.clone(),
            beat_marker_confidences: vec![1.0; markers.len()],
            first_downbeat: Some(Duration::ZERO),
            downbeat_confidence: 1.0,
            musical_key: None,
            rms_dbfs: Some(-12.0),
            sample_peak_dbfs: Some(-3.0),
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
}
