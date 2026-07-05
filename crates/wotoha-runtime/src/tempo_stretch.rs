use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use songbird::input::{AsyncAdapterStream, AsyncReadOnlySource, Input, LiveInput, RawAdapter};
use symphonia::core::{
    audio::SampleBuffer,
    errors::Error as SymphoniaError,
    formats::{SeekMode, SeekTo},
    units::Time,
};
use timestretch::{StreamProcessor, StretchError};
use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};
use wotoha_core::automix::TempoEnvelope;

pub(crate) struct TempoStretchProcessor {
    inner: StreamProcessor,
    envelope: TempoEnvelope,
    sample_rate: u32,
    channels: usize,
    emitted_samples: usize,
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
    let Input::Live(LiveInput::Parsed(mut parsed), _) = input else {
        return Err("tempo stretching requires a parsed input".into());
    };
    let sample_rate = parsed
        .decoder
        .codec_params()
        .sample_rate
        .ok_or_else(|| "tempo stretching requires a known sample rate".to_owned())?;
    let channels = parsed
        .decoder
        .codec_params()
        .channels
        .map(|channels| channels.count())
        .ok_or_else(|| "tempo stretching requires a known channel layout".to_owned())?;
    if channels == 0 || channels > 2 {
        return Err(format!(
            "unsupported tempo-stretch channel count: {channels}"
        ));
    }
    let source_bpm = 120.0;
    let target_bpm = source_bpm * f64::from(envelope.initial_speed);
    let processor =
        TempoStretchProcessor::new(source_bpm, target_bpm, sample_rate, channels, envelope)
            .map_err(|error| error.to_string())?;
    let seeked = parsed
        .format
        .seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: Time::new(
                    source_start.as_secs(),
                    f64::from(source_start.subsec_nanos()) / 1e9,
                ),
                track_id: Some(parsed.track_id),
            },
        )
        .map_err(|error| error.to_string())?;
    parsed.decoder.reset();

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
            seeked.required_ts,
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
    required_ts: u64,
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
        let channels = decoded.spec().channels.count();
        let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        buffer.copy_interleaved_ref(decoded);
        let skip_frames = if packet.ts() < required_ts {
            (required_ts - packet.ts()).min(packet.dur()) as usize
        } else {
            0
        };
        let input = &buffer.samples()[(skip_frames * channels).min(buffer.samples().len())..];
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
        Ok(Self {
            inner,
            envelope,
            sample_rate,
            channels,
            emitted_samples: 0,
        })
    }

    pub(crate) fn process_into(
        &mut self,
        input: &[f32],
        output: &mut Vec<f32>,
    ) -> Result<(), StretchError> {
        let output_elapsed = std::time::Duration::from_secs_f64(
            self.emitted_samples as f64 / self.channels as f64 / self.sample_rate as f64,
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
        self.inner.latency_samples()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use songbird::input::codecs::{get_codec_registry, get_probe};

    use super::*;

    #[test]
    fn stretches_tempo_while_preserving_tone_pitch() {
        const SAMPLE_RATE: u32 = 48_000;
        let envelope = TempoEnvelope {
            initial_speed: 120.0 / 124.0,
            hold: Duration::from_secs(20),
            ramp: Duration::ZERO,
        };
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
        let envelope = TempoEnvelope {
            initial_speed: 0.96,
            hold: Duration::from_millis(500),
            ramp: Duration::from_millis(500),
        };
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
        let constant = process(TempoEnvelope {
            initial_speed: 0.9,
            hold: Duration::from_secs(20),
            ramp: Duration::ZERO,
        });
        let ramped = process(TempoEnvelope {
            initial_speed: 0.9,
            hold: Duration::from_millis(250),
            ramp: Duration::from_millis(750),
        });

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
        let envelope = TempoEnvelope {
            initial_speed: 0.96,
            hold: Duration::from_secs(20),
            ramp: Duration::ZERO,
        };
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn songbird_adapter_seeks_and_streams_stretched_pcm() {
        const SAMPLE_RATE: u32 = 48_000;
        let input = Input::from(sine_wav(440.0, SAMPLE_RATE, 5.0));
        let playable = input
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .unwrap();
        let envelope = TempoEnvelope {
            initial_speed: 120.0 / 124.0,
            hold: Duration::from_secs(20),
            ramp: Duration::ZERO,
        };
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
