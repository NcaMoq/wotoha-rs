use std::{
    borrow::Cow,
    sync::{Arc, RwLock},
    time::Duration,
};

use songbird::input::{Input, LiveInput, Parsed};
use symphonia::core::{
    audio::{AsAudioBufferRef, AudioBuffer, AudioBufferRef, Signal},
    codecs::{CodecDescriptor, CodecParameters, Decoder, DecoderOptions, FinalizeResult},
    errors::Result as SymphoniaResult,
    formats::Packet,
};
use wotoha_core::automix::{EqGains, EqTransition, TempoEnvelope};

/// Shared automation state. The decoder/stream worker owns the filter state, while
/// the track handle can replace or cancel automation without rebuilding the input.
#[derive(Clone)]
pub(crate) struct EqualizerControl {
    enabled: bool,
    transition: Arc<RwLock<Option<EqTransition>>>,
}

impl EqualizerControl {
    pub(crate) fn new(enabled: bool, transition: Option<EqTransition>) -> Self {
        Self {
            enabled,
            transition: Arc::new(RwLock::new(transition)),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn schedule(&self, transition: EqTransition) -> bool {
        if !self.enabled {
            return false;
        }
        *self
            .transition
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(transition);
        true
    }

    pub(crate) fn cancel(&self, id: u64) {
        let mut transition = self
            .transition
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if transition.is_some_and(|current| current.id == id) {
            *transition = None;
        }
    }

    #[cfg(test)]
    fn gains_at(&self, position: Duration) -> EqGains {
        gains_at(self.snapshot(), position)
    }

    fn snapshot(&self) -> Option<EqTransition> {
        *self
            .transition
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

const UNITY: EqGains = EqGains {
    low: 1.0,
    mid: 1.0,
    high: 1.0,
};

#[derive(Clone, Copy)]
pub(crate) struct OutputTimeline {
    source_start: Duration,
    tempo_envelope: Option<TempoEnvelope>,
}

impl OutputTimeline {
    pub(crate) fn trimmed(source_start: Duration) -> Self {
        Self {
            source_start,
            tempo_envelope: None,
        }
    }

    pub(crate) fn stretched(source_start: Duration, envelope: TempoEnvelope) -> Self {
        Self {
            source_start,
            tempo_envelope: Some(envelope),
        }
    }

    fn source_position(self, output_elapsed: Duration) -> Duration {
        self.source_start
            + self.tempo_envelope.map_or(output_elapsed, |envelope| {
                envelope.source_elapsed(output_elapsed)
            })
    }
}

/// Three-band Linkwitz-style reconstruction using two one-pole low passes.
/// `low + (low4k - low250) + (input - low4k)` reconstructs the input. Unity is
/// explicitly bypassed so enabling EQ without active automation is bit-exact.
pub(crate) struct ThreeBandEqualizer {
    control: EqualizerControl,
    sample_rate: u32,
    channels: usize,
    low_250: Vec<f32>,
    low_4k: Vec<f32>,
}

impl ThreeBandEqualizer {
    pub(crate) fn new(control: EqualizerControl, sample_rate: u32, channels: usize) -> Self {
        Self {
            control,
            sample_rate,
            channels,
            low_250: vec![0.0; channels],
            low_4k: vec![0.0; channels],
        }
    }

    pub(crate) fn reset(&mut self) {
        self.low_250.fill(0.0);
        self.low_4k.fill(0.0);
    }

    fn reconfigure(&mut self, sample_rate: u32, channels: usize) {
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.low_250 = vec![0.0; channels];
        self.low_4k = vec![0.0; channels];
    }

    pub(crate) fn process_interleaved(
        &mut self,
        samples: &mut [f32],
        output_frame_start: u64,
        timeline: OutputTimeline,
    ) {
        if self.channels == 0 || self.sample_rate == 0 {
            return;
        }
        debug_assert!(samples.len().is_multiple_of(self.channels));
        let low_alpha = low_pass_alpha(250.0, self.sample_rate);
        let high_alpha = low_pass_alpha(4_000.0, self.sample_rate);
        let transition = self.control.snapshot();
        for (frame_offset, frame) in samples.chunks_exact_mut(self.channels).enumerate() {
            let output_elapsed = frames_duration(
                output_frame_start.saturating_add(frame_offset as u64),
                self.sample_rate,
            );
            let gains = gains_at(transition, timeline.source_position(output_elapsed));
            for (channel, sample) in frame.iter_mut().enumerate() {
                let input = *sample;
                self.low_250[channel] += low_alpha * (input - self.low_250[channel]);
                self.low_4k[channel] += high_alpha * (input - self.low_4k[channel]);
                if gains == UNITY {
                    continue;
                }
                let low = self.low_250[channel];
                let mid = self.low_4k[channel] - low;
                let high = input - self.low_4k[channel];
                *sample = low * gains.low + mid * gains.mid + high * gains.high;
            }
        }
    }

    fn process_planar(&mut self, buffer: &mut AudioBuffer<f32>, packet_start: Duration) {
        if self.channels == 0 || self.sample_rate == 0 {
            return;
        }
        let frames = buffer.frames();
        let low_alpha = low_pass_alpha(250.0, self.sample_rate);
        let high_alpha = low_pass_alpha(4_000.0, self.sample_rate);
        let transition = self.control.snapshot();
        for frame in 0..frames {
            let position = packet_start + frames_duration(frame as u64, self.sample_rate);
            let gains = gains_at(transition, position);
            for channel in 0..self.channels {
                let sample = &mut buffer.chan_mut(channel)[frame];
                let input = *sample;
                self.low_250[channel] += low_alpha * (input - self.low_250[channel]);
                self.low_4k[channel] += high_alpha * (input - self.low_4k[channel]);
                if gains == UNITY {
                    continue;
                }
                let low = self.low_250[channel];
                let mid = self.low_4k[channel] - low;
                let high = input - self.low_4k[channel];
                *sample = low * gains.low + mid * gains.mid + high * gains.high;
            }
        }
    }
}

fn gains_at(transition: Option<EqTransition>, position: Duration) -> EqGains {
    transition.map_or(UNITY, |transition| transition.gains_at(position))
}

fn low_pass_alpha(cutoff: f32, sample_rate: u32) -> f32 {
    1.0 - (-std::f32::consts::TAU * cutoff / sample_rate as f32).exp()
}

fn frames_duration(frames: u64, sample_rate: u32) -> Duration {
    Duration::from_secs_f64(frames as f64 / f64::from(sample_rate))
}

pub(crate) fn wrap_parsed_equalizer(
    input: Input,
    control: EqualizerControl,
) -> Result<Input, String> {
    if !control.enabled() {
        return Ok(input);
    }
    let Input::Live(LiveInput::Parsed(parsed), composer) = input else {
        return Err("equalizer requires a parsed input".into());
    };
    let Parsed {
        format,
        decoder,
        track_id,
        meta,
        supports_backseek,
    } = parsed;
    let decoder = Box::new(EqualizerDecoder::new(decoder, control)?);
    Ok(Input::Live(
        LiveInput::Parsed(Parsed {
            format,
            decoder,
            track_id,
            meta,
            supports_backseek,
        }),
        composer,
    ))
}

struct EqualizerDecoder {
    inner: Box<dyn Decoder>,
    params: CodecParameters,
    output: AudioBuffer<f32>,
    equalizer: ThreeBandEqualizer,
}

impl EqualizerDecoder {
    fn new(inner: Box<dyn Decoder>, control: EqualizerControl) -> Result<Self, String> {
        let params = inner.codec_params().clone();
        let sample_rate = params.sample_rate.unwrap_or(0);
        let channels = params
            .channels
            .map(|channels| channels.count())
            .unwrap_or(0);
        Ok(Self {
            inner,
            params,
            output: AudioBuffer::unused(),
            equalizer: ThreeBandEqualizer::new(control, sample_rate, channels),
        })
    }

    fn packet_start(&self, packet: &Packet) -> Duration {
        self.params.time_base.map_or(Duration::ZERO, |base| {
            let time = base.calc_time(packet.ts());
            Duration::from_secs(time.seconds) + Duration::from_secs_f64(time.frac)
        })
    }
}

impl Decoder for EqualizerDecoder {
    fn try_new(_params: &CodecParameters, _options: &DecoderOptions) -> SymphoniaResult<Self> {
        unreachable!("equalizer wraps an initialized decoder")
    }

    fn supported_codecs() -> &'static [CodecDescriptor] {
        &[]
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.equalizer.reset();
        self.output.clear();
    }

    fn codec_params(&self) -> &CodecParameters {
        &self.params
    }

    fn decode(&mut self, packet: &Packet) -> SymphoniaResult<AudioBufferRef<'_>> {
        let packet_start = self.packet_start(packet);
        let decoded = match self.inner.decode(packet) {
            Ok(decoded) => decoded,
            Err(error) => {
                self.output.clear();
                return Err(error);
            }
        };
        if self.output.capacity() < decoded.frames() || self.output.spec() != decoded.spec() {
            self.output = decoded.make_equivalent::<f32>();
            self.equalizer
                .reconfigure(decoded.spec().rate, decoded.spec().channels.count());
        }
        decoded.convert(&mut self.output);
        self.equalizer
            .process_planar(&mut self.output, packet_start);
        Ok(self.output.as_audio_buffer_ref())
    }

    fn finalize(&mut self) -> FinalizeResult {
        self.inner.finalize()
    }

    fn last_decoded(&self) -> AudioBufferRef<'_> {
        AudioBufferRef::F32(Cow::Borrowed(&self.output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia::core::audio::{Channels, SignalSpec};
    use wotoha_core::automix::{
        AutoMixConfig, EqTransitionRole, TrackAnalysis, TransitionKind, automix_mix_gains,
        plan_transition,
    };

    fn outgoing(start: Duration) -> EqTransition {
        EqTransition {
            id: 7,
            source_start: start,
            duration: Duration::from_secs(4),
            role: EqTransitionRole::Outgoing,
            harmonic_compatibility: None,
        }
    }

    #[test]
    fn unity_is_bit_exact() {
        let control = EqualizerControl::new(true, None);
        let mut equalizer = ThreeBandEqualizer::new(control, 48_000, 2);
        let mut samples = (0..2_000)
            .map(|index| ((index as f32) * 0.123).sin())
            .collect::<Vec<_>>();
        let original = samples.clone();
        equalizer.process_interleaved(&mut samples, 0, OutputTimeline::trimmed(Duration::ZERO));
        assert_eq!(samples, original);
    }

    #[test]
    fn cancellation_only_matches_scheduled_id() {
        let control = EqualizerControl::new(true, Some(outgoing(Duration::ZERO)));
        control.cancel(8);
        assert!(control.gains_at(Duration::from_secs(3)).low < 0.01);
        control.cancel(7);
        assert_eq!(control.gains_at(Duration::from_secs(3)), UNITY);
    }

    #[test]
    fn trimmed_timeline_uses_absolute_source_position() {
        let control = EqualizerControl::new(true, Some(outgoing(Duration::from_secs(10))));
        let mut equalizer = ThreeBandEqualizer::new(control, 1_000, 1);
        let mut samples = vec![1.0; 2_001];
        equalizer.process_interleaved(
            &mut samples,
            0,
            OutputTimeline::trimmed(Duration::from_secs(12)),
        );
        assert!(
            samples[2_000].abs() < 0.2,
            "outgoing low band should be removed by source t=14s"
        );
    }

    #[test]
    fn stretched_timeline_maps_output_to_source() {
        let envelope = TempoEnvelope::new(2.0, 2.0, Duration::from_secs(8), Duration::ZERO);
        let timeline = OutputTimeline::stretched(Duration::from_secs(10), envelope);
        assert_eq!(
            timeline.source_position(Duration::from_secs(2)),
            Duration::from_secs(14)
        );
    }

    #[test]
    fn equalizer_decoder_uses_decoded_spec_when_initial_channel_metadata_is_missing() {
        let mut params = CodecParameters::new();
        params.sample_rate = Some(48_000);
        params.channels = None;
        let mut output = AudioBuffer::new(128, SignalSpec::new(48_000, Channels::FRONT_LEFT));
        output.render_silence(Some(128));

        let mut decoder = EqualizerDecoder::new(
            Box::new(BufferedDecoder { params, output }),
            EqualizerControl::new(true, Some(outgoing(Duration::ZERO))),
        )
        .expect("channel metadata may be missing before the first decoded buffer");

        let packet = Packet::new_from_slice(0, 0, 128, &[]);
        let decoded = decoder.decode(&packet).unwrap();
        assert!(decoded.frames() > 0);
        assert_eq!(decoded.spec().channels.count(), 1);
    }

    #[test]
    fn planned_beatmatched_overlap_renders_without_a_gap() {
        const SAMPLE_RATE: u32 = 8_000;
        let duration = Duration::from_secs(32);
        let config = AutoMixConfig {
            enabled: true,
            crossfade: Duration::from_secs(8),
            max_tempo_adjustment: 0.08,
            min_beat_confidence: 0.7,
        };
        let outgoing = beat_analysis(duration, 120.0);
        let incoming = beat_analysis(duration, 124.0);
        let plan = plan_transition(&outgoing, &incoming, &config);
        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        let envelope = plan
            .tempo_envelope
            .expect("incoming deck should be tempo-matched");

        let frames = duration_frames(plan.duration, SAMPLE_RATE);
        let outgoing_transition = EqTransition {
            id: 91,
            source_start: plan.outgoing_start,
            duration: plan.duration,
            role: EqTransitionRole::Outgoing,
            harmonic_compatibility: plan.harmonic_compatibility,
        };
        let incoming_transition = EqTransition {
            id: 91,
            source_start: plan.incoming_start,
            duration: envelope.source_elapsed(plan.duration),
            role: EqTransitionRole::Incoming,
            harmonic_compatibility: plan.harmonic_compatibility,
        };
        let mut outgoing_samples =
            render_deck_segment(frames, SAMPLE_RATE, 120.0, plan.outgoing_start, None);
        let mut incoming_samples = render_deck_segment(
            frames,
            SAMPLE_RATE,
            124.0,
            plan.incoming_start,
            Some(envelope),
        );
        ThreeBandEqualizer::new(
            EqualizerControl::new(true, Some(outgoing_transition)),
            SAMPLE_RATE,
            1,
        )
        .process_interleaved(
            &mut outgoing_samples,
            0,
            OutputTimeline::trimmed(plan.outgoing_start),
        );
        ThreeBandEqualizer::new(
            EqualizerControl::new(true, Some(incoming_transition)),
            SAMPLE_RATE,
            1,
        )
        .process_interleaved(
            &mut incoming_samples,
            0,
            OutputTimeline::stretched(plan.incoming_start, envelope),
        );

        let mixed = automix_mix(&outgoing_samples, &incoming_samples, plan.kind);
        let continuity_window = Duration::from_millis(500);
        let start_rms = window_rms(&mixed, SAMPLE_RATE, continuity_window);
        let mid_rms = window_rms_at(&mixed, SAMPLE_RATE, plan.duration / 2, continuity_window);
        let end_rms = window_rms_at(
            &mixed,
            SAMPLE_RATE,
            plan.duration.saturating_sub(continuity_window),
            continuity_window,
        );
        let quietest = min_window_rms(&mixed, SAMPLE_RATE, continuity_window);
        assert!(
            quietest > start_rms.min(end_rms) * 0.35,
            "rendered AutoMix overlap contains an audible gap: quietest={quietest}, start={start_rms}, end={end_rms}"
        );
        assert!(
            mid_rms > start_rms.min(end_rms) * 0.60,
            "rendered AutoMix midpoint lost too much energy: mid={mid_rms}, start={start_rms}, end={end_rms}"
        );

        let beat_probe = plan.duration / 2;
        let onbeat_position = output_beat_at_or_after(beat_probe, plan.outgoing_start, 120.0);
        let offbeat_position = onbeat_position + Duration::from_millis(250);
        assert!(offbeat_position < plan.duration);
        let onbeat = beat_window_energy(&mixed, SAMPLE_RATE, onbeat_position);
        let offbeat = beat_window_energy(&mixed, SAMPLE_RATE, offbeat_position);
        assert!(
            onbeat > offbeat * 4.0,
            "tempo-mapped incoming kicks are not landing on the outgoing beat grid: onbeat={onbeat}, offbeat={offbeat}"
        );
    }

    fn beat_analysis(duration: Duration, bpm: f32) -> TrackAnalysis {
        let beat = Duration::from_secs_f32(60.0 / bpm);
        let mut markers = Vec::new();
        let mut position = Duration::ZERO;
        while position <= duration {
            markers.push(position);
            position += beat;
        }
        TrackAnalysis {
            duration,
            audible_start: Duration::ZERO,
            audible_end: duration,
            intro_end: Some(Duration::from_secs(8)),
            intro_confidence: 1.0,
            outro_start: Some(duration.saturating_sub(Duration::from_secs(8))),
            outro_confidence: 1.0,
            vocal_activity: Vec::new(),
            vocal_activity_confidences: Vec::new(),
            vocal_activity_rate: 0,
            energy_profile: Vec::new(),
            energy_profile_rate: 0,
            bpm: Some(bpm),
            beat_confidence: 1.0,
            first_beat: Some(Duration::ZERO),
            beat_marker_confidences: vec![1.0; markers.len()],
            beat_markers: markers,
            first_downbeat: Some(Duration::ZERO),
            downbeat_confidence: 1.0,
            musical_key: None,
            rms_dbfs: Some(-12.0),
            sample_peak_dbfs: Some(-3.0),
        }
    }

    fn render_deck_segment(
        frames: usize,
        sample_rate: u32,
        bpm: f32,
        source_start: Duration,
        envelope: Option<TempoEnvelope>,
    ) -> Vec<f32> {
        (0..frames)
            .map(|frame| {
                let output_elapsed = frames_duration(frame as u64, sample_rate);
                let source_position = source_start
                    + envelope.map_or(output_elapsed, |envelope| {
                        envelope.source_elapsed(output_elapsed)
                    });
                synthetic_deck_sample(source_position.as_secs_f32(), bpm)
            })
            .collect()
    }

    fn synthetic_deck_sample(source_seconds: f32, bpm: f32) -> f32 {
        let beat_interval = 60.0 / bpm;
        let beat_phase = source_seconds / beat_interval;
        let nearest_beat = beat_phase.round() * beat_interval;
        let beat_distance = (source_seconds - nearest_beat).abs();
        let kick_envelope = (-(beat_distance / 0.018).powi(2)).exp();
        let kick = (std::f32::consts::TAU * 65.0 * source_seconds).sin() * kick_envelope * 0.85;
        let musical_bed = (std::f32::consts::TAU * 440.0 * source_seconds).sin() * 0.04
            + (std::f32::consts::TAU * 880.0 * source_seconds).sin() * 0.02;
        kick + musical_bed
    }

    fn automix_mix(outgoing: &[f32], incoming: &[f32], kind: TransitionKind) -> Vec<f32> {
        let last = outgoing.len().saturating_sub(1).max(1) as f32;
        outgoing
            .iter()
            .zip(incoming)
            .enumerate()
            .map(|(index, (outgoing, incoming))| {
                let progress = index as f32 / last;
                let (outgoing_gain, incoming_gain) = automix_mix_gains(kind, progress);
                outgoing * outgoing_gain + incoming * incoming_gain
            })
            .collect()
    }

    fn duration_frames(duration: Duration, sample_rate: u32) -> usize {
        (duration.as_secs_f64() * f64::from(sample_rate)).round() as usize
    }

    fn window_rms(samples: &[f32], sample_rate: u32, length: Duration) -> f32 {
        window_rms_at(samples, sample_rate, Duration::ZERO, length)
    }

    fn window_rms_at(samples: &[f32], sample_rate: u32, start: Duration, length: Duration) -> f32 {
        let start = duration_frames(start, sample_rate).min(samples.len());
        let frames = duration_frames(length, sample_rate).max(1);
        let end = start.saturating_add(frames).min(samples.len());
        rms(&samples[start..end])
    }

    fn min_window_rms(samples: &[f32], sample_rate: u32, length: Duration) -> f32 {
        let frames = duration_frames(length, sample_rate).max(1);
        samples
            .chunks(frames)
            .filter(|window| !window.is_empty())
            .map(rms)
            .fold(f32::INFINITY, f32::min)
    }

    fn beat_window_energy(samples: &[f32], sample_rate: u32, center: Duration) -> f32 {
        let center = duration_frames(center, sample_rate).min(samples.len());
        let radius = duration_frames(Duration::from_millis(24), sample_rate);
        let start = center.saturating_sub(radius);
        let end = center.saturating_add(radius).min(samples.len());
        samples[start..end]
            .iter()
            .map(|sample| sample * sample)
            .sum()
    }

    fn output_beat_at_or_after(
        output_position: Duration,
        source_start: Duration,
        bpm: f32,
    ) -> Duration {
        let interval = Duration::from_secs_f32(60.0 / bpm);
        let source_position = source_start + output_position;
        let beats = source_position.as_secs_f64() / interval.as_secs_f64();
        let beat_source = interval.mul_f64(beats.ceil());
        beat_source.saturating_sub(source_start)
    }

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        (samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32).sqrt()
    }

    struct BufferedDecoder {
        params: CodecParameters,
        output: AudioBuffer<f32>,
    }

    impl Decoder for BufferedDecoder {
        fn try_new(_params: &CodecParameters, _options: &DecoderOptions) -> SymphoniaResult<Self> {
            unreachable!("test decoder is constructed directly")
        }

        fn supported_codecs() -> &'static [CodecDescriptor] {
            &[]
        }

        fn reset(&mut self) {}

        fn codec_params(&self) -> &CodecParameters {
            &self.params
        }

        fn decode(&mut self, _packet: &Packet) -> SymphoniaResult<AudioBufferRef<'_>> {
            Ok(self.output.as_audio_buffer_ref())
        }

        fn finalize(&mut self) -> FinalizeResult {
            FinalizeResult::default()
        }

        fn last_decoded(&self) -> AudioBufferRef<'_> {
            AudioBufferRef::F32(Cow::Borrowed(&self.output))
        }
    }
}
