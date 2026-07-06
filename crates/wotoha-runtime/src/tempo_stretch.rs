use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use songbird::input::{AsyncAdapterStream, AsyncReadOnlySource, Input, LiveInput, RawAdapter};
use symphonia::core::{audio::SampleBuffer, errors::Error as SymphoniaError};
use timestretch::{StreamProcessor, StretchError};
use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};
use wotoha_core::automix::TempoEnvelope;

pub(crate) struct TempoStretchProcessor {
    inner: StreamProcessor,
    envelope: TempoEnvelope,
    sample_rate: u32,
    channels: usize,
    emitted_samples: usize,
    latency_samples: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct StretchTimeline {
    pub(crate) source_start: std::time::Duration,
    pub(crate) envelope: TempoEnvelope,
}

pub(crate) fn build_stretched_input(
    input: Input,
    source_start: std::time::Duration,
    envelope: TempoEnvelope,
) -> Result<(Input, StretchTimeline), String> {
    let Input::Live(LiveInput::Parsed(parsed), _) = input else {
        return Err("tempo stretching requires a parsed input".into());
    };
    let sample_rate = parsed
        .decoder
        .codec_params()
        .sample_rate
        .ok_or_else(|| "tempo stretching requires a known sample rate".to_owned())?;
    let channels = output_channel_count(
        parsed
            .decoder
            .codec_params()
            .channels
            .map(|channels| channels.count()),
    );
    let source_bpm = 120.0;
    let target_bpm = source_bpm * f64::from(envelope.initial_speed);
    let processor =
        TempoStretchProcessor::new(source_bpm, target_bpm, sample_rate, channels, envelope)
            .map_err(|error| error.to_string())?;
    let source_skip_frames = duration_frames(source_start, sample_rate);

    let cancelled = Arc::new(AtomicBool::new(false));
    let (reader, writer) = tokio::io::duplex(384 * 1024);
    let source = AsyncReadOnlySource::new(CancelOnDropReader {
        inner: reader,
        cancelled: cancelled.clone(),
    });
    let adapter = AsyncAdapterStream::new(Box::new(source), 128 * 1024);
    let output: Input = RawAdapter::new(adapter, sample_rate, channels as u32).into();
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        if let Err(error) = decode_and_stretch(
            parsed,
            writer,
            processor,
            source_skip_frames,
            channels,
            cancelled,
            runtime,
        ) {
            tracing::warn!(%error, "AutoMix tempo-stretch worker stopped");
        }
    });
    Ok((
        output,
        StretchTimeline {
            source_start,
            envelope,
        },
    ))
}

pub(crate) fn build_trimmed_input(
    input: Input,
    source_start: std::time::Duration,
) -> Result<Input, String> {
    if source_start.is_zero() {
        return Ok(input);
    }
    let Input::Live(LiveInput::Parsed(parsed), _) = input else {
        return Err("source trimming requires a parsed input".into());
    };
    let sample_rate = parsed
        .decoder
        .codec_params()
        .sample_rate
        .ok_or_else(|| "source trimming requires a known sample rate".to_owned())?;
    let channels = output_channel_count(
        parsed
            .decoder
            .codec_params()
            .channels
            .map(|channels| channels.count()),
    );

    let source_skip_frames = duration_frames(source_start, sample_rate);
    let cancelled = Arc::new(AtomicBool::new(false));
    let (reader, writer) = tokio::io::duplex(384 * 1024);
    let source = AsyncReadOnlySource::new(CancelOnDropReader {
        inner: reader,
        cancelled: cancelled.clone(),
    });
    let adapter = AsyncAdapterStream::new(Box::new(source), 128 * 1024);
    let output: Input = RawAdapter::new(adapter, sample_rate, channels as u32).into();
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        if let Err(error) = decode_and_trim(
            parsed,
            writer,
            source_skip_frames,
            channels,
            cancelled,
            runtime,
        ) {
            tracing::warn!(%error, "AutoMix source-trim worker stopped");
        }
    });
    Ok(output)
}

fn output_channel_count(metadata_channels: Option<usize>) -> usize {
    metadata_channels
        .filter(|channels| (1..=2).contains(channels))
        .unwrap_or(2)
}

fn duration_frames(duration: std::time::Duration, sample_rate: u32) -> usize {
    (duration.as_secs_f64() * f64::from(sample_rate)) as usize
}

struct CancelOnDropReader<R> {
    inner: R,
    cancelled: Arc<AtomicBool>,
}

impl<R: AsyncRead + Unpin> AsyncRead for CancelOnDropReader<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl<R> Drop for CancelOnDropReader<R> {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

fn decode_and_stretch(
    mut parsed: songbird::input::Parsed,
    mut writer: tokio::io::DuplexStream,
    mut processor: TempoStretchProcessor,
    mut source_skip_frames: usize,
    output_channels: usize,
    cancelled: Arc<AtomicBool>,
    runtime: tokio::runtime::Handle,
) -> Result<(), String> {
    let mut discard_output = processor.latency_samples();
    loop {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        let packet = match parsed.format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(error) => return Err(error.to_string()),
        };
        if packet.track_id() != parsed.track_id {
            continue;
        }
        let decoded = match parsed.decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(error) => return Err(error.to_string()),
        };
        let input_channels = decoded.spec().channels.count();
        if input_channels == 0 {
            return Err("decoded audio has no channels".into());
        }
        let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        buffer.copy_interleaved_ref(decoded);
        let available_frames = buffer.samples().len() / input_channels;
        let skip_frames = source_skip_frames.min(available_frames);
        source_skip_frames -= skip_frames;
        let source = &buffer.samples()[skip_frames * input_channels..];
        let mut normalized = Vec::new();
        let input = normalize_channels(source, input_channels, output_channels, &mut normalized)?;
        if input.is_empty() {
            continue;
        }
        let mut output = Vec::with_capacity(input.len() * 2 + processor.latency_samples());
        processor
            .process_into(input, &mut output)
            .map_err(|error| error.to_string())?;
        write_pcm(
            &mut writer,
            &output,
            &mut discard_output,
            &cancelled,
            &runtime,
        )?;
    }
    let mut output = Vec::with_capacity(processor.latency_samples() * 4);
    processor
        .flush_into(&mut output)
        .map_err(|error| error.to_string())?;
    write_pcm(
        &mut writer,
        &output,
        &mut discard_output,
        &cancelled,
        &runtime,
    )?;
    let _ = runtime.block_on(writer.shutdown());
    Ok(())
}

fn decode_and_trim(
    mut parsed: songbird::input::Parsed,
    mut writer: tokio::io::DuplexStream,
    mut source_skip_frames: usize,
    output_channels: usize,
    cancelled: Arc<AtomicBool>,
    runtime: tokio::runtime::Handle,
) -> Result<(), String> {
    loop {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        let packet = match parsed.format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(error) => return Err(error.to_string()),
        };
        if packet.track_id() != parsed.track_id {
            continue;
        }
        let decoded = match parsed.decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(error) => return Err(error.to_string()),
        };
        let input_channels = decoded.spec().channels.count();
        if input_channels == 0 {
            return Err("decoded audio has no channels".into());
        }
        let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        buffer.copy_interleaved_ref(decoded);
        let available_frames = buffer.samples().len() / input_channels;
        let skip_frames = source_skip_frames.min(available_frames);
        source_skip_frames -= skip_frames;
        let source = &buffer.samples()[skip_frames * input_channels..];
        if !source.is_empty() {
            let mut normalized = Vec::new();
            let samples =
                normalize_channels(source, input_channels, output_channels, &mut normalized)?;
            let mut discard = 0;
            write_pcm(&mut writer, samples, &mut discard, &cancelled, &runtime)?;
        }
    }
    let _ = runtime.block_on(writer.shutdown());
    Ok(())
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

fn write_pcm(
    writer: &mut tokio::io::DuplexStream,
    samples: &[f32],
    discard: &mut usize,
    cancelled: &AtomicBool,
    runtime: &tokio::runtime::Handle,
) -> Result<(), String> {
    let skip = (*discard).min(samples.len());
    *discard -= skip;
    if skip == samples.len() || cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }
    let mut bytes = Vec::with_capacity((samples.len() - skip) * size_of::<f32>());
    for sample in &samples[skip..] {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    runtime
        .block_on(writer.write_all(&bytes))
        .map_err(|error| error.to_string())
}

impl TempoStretchProcessor {
    pub(crate) fn new(
        source_bpm: f64,
        target_bpm: f64,
        sample_rate: u32,
        channels: usize,
        envelope: TempoEnvelope,
    ) -> Result<Self, StretchError> {
        let inner =
            StreamProcessor::try_from_tempo(source_bpm, target_bpm, sample_rate, channels as u32)?;
        let latency_samples = inner.latency_samples();
        Ok(Self {
            inner,
            envelope,
            sample_rate,
            channels,
            emitted_samples: 0,
            latency_samples,
        })
    }

    pub(crate) fn process_into(
        &mut self,
        input: &[f32],
        output: &mut Vec<f32>,
    ) -> Result<(), StretchError> {
        let output_elapsed = std::time::Duration::from_secs_f64(
            audible_output_samples(self.emitted_samples, self.latency_samples) as f64
                / self.channels as f64
                / self.sample_rate as f64,
        );
        let speed = self.envelope.speed_at(output_elapsed);
        self.inner.set_stretch_ratio(1.0 / f64::from(speed))?;
        let before = output.len();
        output.reserve(input.len().saturating_mul(2) + self.inner.latency_samples());
        self.inner.process_into(input, output)?;
        self.emitted_samples += output.len() - before;
        Ok(())
    }

    pub(crate) fn flush_into(&mut self, output: &mut Vec<f32>) -> Result<(), StretchError> {
        output.reserve(self.inner.latency_samples().saturating_mul(4));
        self.inner.flush_into(output)?;
        Ok(())
    }

    pub(crate) fn latency_samples(&self) -> usize {
        self.latency_samples
    }
}

fn audible_output_samples(emitted_samples: usize, latency_samples: usize) -> usize {
    emitted_samples.saturating_sub(latency_samples)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use songbird::input::{
        AudioStream, Parsed,
        codecs::{get_codec_registry, get_probe},
    };
    use symphonia::core::{
        audio::AudioBufferRef,
        codecs::{CodecDescriptor, CodecParameters, Decoder, DecoderOptions, FinalizeResult},
        errors::Result as SymphoniaResult,
        formats::Packet,
        io::MediaSource,
    };

    use super::*;

    #[test]
    fn tempo_timeline_starts_after_dsp_latency_is_discarded() {
        assert_eq!(audible_output_samples(512, 1_024), 0);
        assert_eq!(audible_output_samples(1_024, 1_024), 0);
        assert_eq!(audible_output_samples(1_280, 1_024), 256);
    }

    #[test]
    fn stretches_tempo_while_preserving_tone_pitch() {
        const SAMPLE_RATE: u32 = 48_000;
        let envelope = TempoEnvelope::new(
            120.0 / 124.0,
            120.0 / 124.0,
            Duration::from_secs(20),
            Duration::ZERO,
        );
        let mut processor = TempoStretchProcessor::new(124.0, 120.0, SAMPLE_RATE, 1, envelope)
            .expect("valid tempo configuration");
        let latency = processor.latency_samples();
        let input = sine(440.0, SAMPLE_RATE, 4.0);
        let mut output = Vec::with_capacity(input.len() * 2);
        for chunk in input.chunks(1_024) {
            processor.process_into(chunk, &mut output).unwrap();
        }
        processor.flush_into(&mut output).unwrap();

        assert!(output.len() > input.len());
        let from = latency.min(output.len() / 4);
        let to = (from + SAMPLE_RATE as usize * 2).min(output.len());
        let frequency = estimate_frequency(&output[from..to], SAMPLE_RATE);
        assert!((frequency - 440.0).abs() < 3.0, "frequency={frequency}");
    }

    #[test]
    fn follows_envelope_back_to_unity() {
        const SAMPLE_RATE: u32 = 48_000;
        let envelope = TempoEnvelope::new(
            0.96,
            0.96,
            Duration::from_millis(500),
            Duration::from_millis(500),
        );
        let mut processor = TempoStretchProcessor::new(125.0, 120.0, SAMPLE_RATE, 1, envelope)
            .expect("valid tempo configuration");
        let input = sine(220.0, SAMPLE_RATE, 3.0);
        let mut output = Vec::with_capacity(input.len() * 2);
        for chunk in input.chunks(960) {
            processor.process_into(chunk, &mut output).unwrap();
        }
        processor.flush_into(&mut output).unwrap();
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.len() > input.len());
    }

    #[test]
    fn tempo_ramp_produces_less_extension_than_a_constant_slowdown() {
        const SAMPLE_RATE: u32 = 48_000;
        let input = sine(220.0, SAMPLE_RATE, 8.0);
        let process = |envelope| {
            let mut processor =
                TempoStretchProcessor::new(120.0, 108.0, SAMPLE_RATE, 1, envelope).unwrap();
            let mut output = Vec::with_capacity(input.len() * 2);
            for chunk in input.chunks(960) {
                processor.process_into(chunk, &mut output).unwrap();
            }
            processor.flush_into(&mut output).unwrap();
            output
        };
        let constant = process(TempoEnvelope::new(
            0.9,
            0.9,
            Duration::from_secs(20),
            Duration::ZERO,
        ));
        let ramped = process(TempoEnvelope::new(
            0.9,
            0.9,
            Duration::from_millis(250),
            Duration::from_millis(750),
        ));

        assert!(ramped.len() > input.len());
        assert!(
            ramped.len() + SAMPLE_RATE as usize / 4 < constant.len(),
            "ramped={}, constant={}",
            ramped.len(),
            constant.len()
        );
    }

    #[test]
    fn stretches_stereo_without_collapsing_channels_or_pitch() {
        const SAMPLE_RATE: u32 = 48_000;
        let envelope = TempoEnvelope::new(0.96, 0.96, Duration::from_secs(20), Duration::ZERO);
        let mut processor = TempoStretchProcessor::new(125.0, 120.0, SAMPLE_RATE, 2, envelope)
            .expect("valid stereo tempo configuration");
        let latency = processor.latency_samples();
        let left = sine(330.0, SAMPLE_RATE, 4.0);
        let right = sine(660.0, SAMPLE_RATE, 4.0);
        let mut input = Vec::with_capacity(left.len() * 2);
        for (left, right) in left.into_iter().zip(right) {
            input.extend_from_slice(&[left, right]);
        }
        let mut output = Vec::with_capacity(input.len() * 2);
        for chunk in input.chunks(2_048) {
            processor.process_into(chunk, &mut output).unwrap();
        }
        processor.flush_into(&mut output).unwrap();

        assert!(output.len() > input.len());
        let from = latency.min(output.len() / 4);
        let from = from + from % 2;
        let to = (from + SAMPLE_RATE as usize * 2 * 2).min(output.len());
        let mut output_left = Vec::with_capacity((to - from) / 2);
        let mut output_right = Vec::with_capacity((to - from) / 2);
        for frame in output[from..to].chunks_exact(2) {
            output_left.push(frame[0]);
            output_right.push(frame[1]);
        }
        let left_frequency = spectral_peak_frequency(&output_left, SAMPLE_RATE, 300, 360);
        let right_frequency = spectral_peak_frequency(&output_right, SAMPLE_RATE, 620, 700);
        assert!(
            (left_frequency - 330.0).abs() < 3.0,
            "left_frequency={left_frequency}"
        );
        assert!(
            (right_frequency - 660.0).abs() < 3.0,
            "right_frequency={right_frequency}"
        );
    }

    #[test]
    fn dropping_pcm_reader_cancels_stretch_worker() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (reader, _writer) = tokio::io::duplex(64);
        let reader = CancelOnDropReader {
            inner: reader,
            cancelled: cancelled.clone(),
        };

        assert!(!cancelled.load(Ordering::Relaxed));
        drop(reader);
        assert!(cancelled.load(Ordering::Relaxed));
    }

    #[test]
    fn missing_channel_metadata_defaults_to_stereo() {
        assert_eq!(output_channel_count(None), 2);
        assert_eq!(output_channel_count(Some(1)), 1);
        assert_eq!(output_channel_count(Some(2)), 2);
        assert_eq!(output_channel_count(Some(6)), 2);
    }

    #[test]
    fn normalizes_mono_frames_to_stereo() {
        let mut output = Vec::new();
        let normalized = normalize_channels(&[0.25, -0.5], 1, 2, &mut output).unwrap();
        assert_eq!(normalized, [0.25, 0.25, -0.5, -0.5]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn songbird_adapter_trims_and_streams_stretched_pcm() {
        const SAMPLE_RATE: u32 = 48_000;
        let input = Input::from(sine_wav(440.0, SAMPLE_RATE, 5.0));
        let playable = input
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .unwrap();
        let envelope = TempoEnvelope::new(
            120.0 / 124.0,
            120.0 / 124.0,
            Duration::from_secs(20),
            Duration::ZERO,
        );
        let (stretched, timeline) =
            build_stretched_input(playable, Duration::from_secs(1), envelope).unwrap();
        assert_eq!(timeline.source_start, Duration::from_secs(1));
        let playable = stretched
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .unwrap();
        let Input::Live(LiveInput::Parsed(mut parsed), _) = playable else {
            panic!("raw adapter should parse");
        };
        let mut samples = Vec::new();
        while samples.len() < SAMPLE_RATE as usize * 2 {
            let packet = parsed.format.next_packet().unwrap();
            let decoded = parsed.decoder.decode(&packet).unwrap();
            let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
            buffer.copy_interleaved_ref(decoded);
            samples.extend_from_slice(buffer.samples());
        }
        let frequency = estimate_frequency(&samples, SAMPLE_RATE);
        assert!((frequency - 440.0).abs() < 3.0, "frequency={frequency}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trims_unseekable_input_before_native_playback() {
        const SAMPLE_RATE: u32 = 48_000;
        let playable = unseekable_playable(two_tone_wav(SAMPLE_RATE)).await;
        let Input::Live(LiveInput::Parsed(ref parsed), _) = playable else {
            panic!("WAV should parse");
        };
        assert!(!parsed.supports_backseek);

        let trimmed = build_trimmed_input(playable, Duration::from_secs(1)).unwrap();
        let samples = decode_samples(trimmed, SAMPLE_RATE as usize).await;
        let frequency = estimate_frequency(&samples, SAMPLE_RATE);
        assert!((frequency - 440.0).abs() < 3.0, "frequency={frequency}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trims_input_when_decoder_channel_metadata_is_missing() {
        const SAMPLE_RATE: u32 = 48_000;
        let playable = Input::from(two_tone_wav(SAMPLE_RATE))
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .unwrap();
        let playable = without_channel_metadata(playable);

        let trimmed = build_trimmed_input(playable, Duration::from_secs(1)).unwrap();
        let samples = decode_samples(trimmed, SAMPLE_RATE as usize * 2).await;
        let left = samples
            .chunks_exact(2)
            .map(|frame| frame[0])
            .collect::<Vec<_>>();
        let frequency = estimate_frequency(&left, SAMPLE_RATE);
        assert!((frequency - 440.0).abs() < 3.0, "frequency={frequency}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trims_unseekable_input_before_tempo_stretch() {
        const SAMPLE_RATE: u32 = 48_000;
        let playable = unseekable_playable(two_tone_wav(SAMPLE_RATE)).await;
        let envelope = TempoEnvelope::new(1.0, 1.0, Duration::from_secs(20), Duration::ZERO);
        let (trimmed, timeline) =
            build_stretched_input(playable, Duration::from_secs(1), envelope).unwrap();
        assert_eq!(timeline.source_start, Duration::from_secs(1));
        let samples = decode_samples(trimmed, SAMPLE_RATE as usize).await;
        let frequency = estimate_frequency(&samples, SAMPLE_RATE);
        assert!((frequency - 440.0).abs() < 3.0, "frequency={frequency}");
    }

    async fn unseekable_playable(bytes: Vec<u8>) -> Input {
        let (reader, mut writer) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            writer.write_all(&bytes).await.unwrap();
            writer.shutdown().await.unwrap();
        });
        let source = AsyncReadOnlySource::new(reader);
        let adapter = AsyncAdapterStream::new(Box::new(source), 64 * 1024);
        let input: Box<dyn MediaSource> = Box::new(adapter);
        Input::Live(LiveInput::Raw(AudioStream { input }), None)
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .unwrap()
    }

    async fn decode_samples(input: Input, minimum: usize) -> Vec<f32> {
        let playable = input
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .unwrap();
        let Input::Live(LiveInput::Parsed(mut parsed), _) = playable else {
            panic!("raw adapter should parse");
        };
        let mut samples = Vec::new();
        while samples.len() < minimum {
            let packet = parsed.format.next_packet().unwrap();
            let decoded = parsed.decoder.decode(&packet).unwrap();
            let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
            buffer.copy_interleaved_ref(decoded);
            samples.extend_from_slice(buffer.samples());
        }
        samples
    }

    fn without_channel_metadata(input: Input) -> Input {
        let Input::Live(LiveInput::Parsed(parsed), composer) = input else {
            panic!("input should be parsed");
        };
        let Parsed {
            format,
            decoder,
            track_id,
            meta,
            supports_backseek,
        } = parsed;
        let decoder = Box::new(MissingChannelMetadataDecoder::new(decoder));
        Input::Live(
            LiveInput::Parsed(Parsed {
                format,
                decoder,
                track_id,
                meta,
                supports_backseek,
            }),
            composer,
        )
    }

    struct MissingChannelMetadataDecoder {
        inner: Box<dyn Decoder>,
        params: CodecParameters,
    }

    impl MissingChannelMetadataDecoder {
        fn new(inner: Box<dyn Decoder>) -> Self {
            let mut params = inner.codec_params().clone();
            params.channels = None;
            Self { inner, params }
        }
    }

    impl Decoder for MissingChannelMetadataDecoder {
        fn try_new(_params: &CodecParameters, _options: &DecoderOptions) -> SymphoniaResult<Self> {
            unreachable!("test wrapper is created around an existing decoder")
        }

        fn supported_codecs() -> &'static [CodecDescriptor] {
            &[]
        }

        fn reset(&mut self) {
            self.inner.reset();
        }

        fn codec_params(&self) -> &CodecParameters {
            &self.params
        }

        fn decode(&mut self, packet: &Packet) -> SymphoniaResult<AudioBufferRef<'_>> {
            self.inner.decode(packet)
        }

        fn finalize(&mut self) -> FinalizeResult {
            self.inner.finalize()
        }

        fn last_decoded(&self) -> AudioBufferRef<'_> {
            self.inner.last_decoded()
        }
    }

    fn sine(frequency: f32, sample_rate: u32, seconds: f32) -> Vec<f32> {
        (0..(sample_rate as f32 * seconds) as usize)
            .map(|index| {
                (std::f32::consts::TAU * frequency * index as f32 / sample_rate as f32).sin() * 0.5
            })
            .collect()
    }

    fn estimate_frequency(samples: &[f32], sample_rate: u32) -> f32 {
        let crossings = samples
            .windows(2)
            .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
            .count();
        crossings as f32 * sample_rate as f32 / samples.len() as f32
    }

    fn spectral_peak_frequency(
        samples: &[f32],
        sample_rate: u32,
        minimum: u32,
        maximum: u32,
    ) -> f32 {
        let samples = &samples[..samples.len().min(sample_rate as usize)];
        (minimum..=maximum)
            .map(|frequency| {
                let omega = std::f64::consts::TAU * frequency as f64 / sample_rate as f64;
                let (real, imaginary) = samples.iter().enumerate().fold(
                    (0.0_f64, 0.0_f64),
                    |(real, imaginary), (index, sample)| {
                        let phase = omega * index as f64;
                        (
                            real + f64::from(*sample) * phase.cos(),
                            imaginary - f64::from(*sample) * phase.sin(),
                        )
                    },
                );
                (real * real + imaginary * imaginary, frequency as f32)
            })
            .max_by(|left, right| left.0.total_cmp(&right.0))
            .map(|(_, frequency)| frequency)
            .unwrap_or_default()
    }

    fn sine_wav(frequency: f32, sample_rate: u32, seconds: f32) -> Vec<u8> {
        let samples = sine(frequency, sample_rate, seconds);
        samples_wav(&samples, sample_rate)
    }

    fn two_tone_wav(sample_rate: u32) -> Vec<u8> {
        let mut samples = sine(220.0, sample_rate, 1.0);
        samples.extend(sine(440.0, sample_rate, 2.0));
        samples_wav(&samples, sample_rate)
    }

    fn samples_wav(samples: &[f32], sample_rate: u32) -> Vec<u8> {
        let data_len = samples.len() * size_of::<i16>();
        let mut wav = Vec::with_capacity(44 + data_len);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * size_of::<i16>() as u32).to_le_bytes());
        wav.extend_from_slice(&(size_of::<i16>() as u16).to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data_len as u32).to_le_bytes());
        for sample in samples {
            wav.extend_from_slice(&((sample * i16::MAX as f32) as i16).to_le_bytes());
        }
        wav
    }
}
