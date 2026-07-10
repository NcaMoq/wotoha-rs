use std::sync::atomic::{AtomicBool, Ordering};

use songbird::input::{Input, LiveInput};
use symphonia::core::{audio::SampleBuffer, errors::Error};
use wotoha_core::{
    audio_analysis::{LowBandFilter, analyze_mono_pcm_with_low_band, apply_energy_structure},
    automix::TrackAnalysis,
    key_analysis::estimate_musical_key,
    vocal_analysis::{VocalActivityAnalyzer, apply_vocal_activity},
};

const ANALYSIS_RATE: u32 = 1_000;
const TONAL_RATE: u32 = 11_025;
const STRUCTURE_RATE: u32 = 4;
const MAX_ANALYSIS_SECONDS: usize = 30 * 60;
const MAX_TONAL_SECONDS: usize = 6 * 60;

#[cfg(test)]
pub(crate) fn analyze_input(input: Input) -> Option<TrackAnalysis> {
    analyze_input_with_cancel(input, &AtomicBool::new(false))
}

pub(crate) fn analyze_input_with_cancel(
    input: Input,
    cancelled: &AtomicBool,
) -> Option<TrackAnalysis> {
    analyze_input_with_limit(
        input,
        ANALYSIS_RATE as usize * MAX_ANALYSIS_SECONDS,
        cancelled,
    )
}

fn analyze_input_with_limit(
    input: Input,
    max_samples: usize,
    cancelled: &AtomicBool,
) -> Option<TrackAnalysis> {
    let Input::Live(LiveInput::Parsed(mut parsed), _) = input else {
        return None;
    };
    let mut mono = Vec::with_capacity(ANALYSIS_RATE as usize * 180);
    let mut low_band = Vec::with_capacity(ANALYSIS_RATE as usize * 180);
    let mut accumulator = 0.0_f32;
    let mut low_band_accumulator = 0.0_f32;
    let mut accumulated = 0_usize;
    let mut phase = 0_u64;
    let mut tonal = Vec::with_capacity(TONAL_RATE as usize * 180);
    let mut tonal_accumulator = 0.0_f32;
    let mut tonal_accumulated = 0_usize;
    let mut tonal_phase = 0_u64;
    let mut stream_rate = None;
    let mut low_band_filter = None;
    let mut vocal_analyzer = None;
    let mut structure_rms = Vec::with_capacity(STRUCTURE_RATE as usize * 180);
    let mut structure_sum_squares = 0.0_f64;
    let mut structure_frames = 0_u64;
    let mut structure_phase = 0_u64;
    let mut sum_squares = 0.0_f64;
    let mut loudness_samples = 0_u64;
    let mut sample_peak = 0.0_f32;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let packet = match parsed.format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(_) => break,
        };
        if packet.track_id() != parsed.track_id {
            continue;
        }
        let decoded = match parsed.decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(_) => continue,
        };
        let source_rate = decoded.spec().rate;
        let channels = decoded.spec().channels.count();
        if channels == 0 || source_rate < ANALYSIS_RATE {
            continue;
        }
        if stream_rate.is_some_and(|rate| rate != source_rate) {
            return None;
        }
        stream_rate = Some(source_rate);
        if low_band_filter.is_none() {
            low_band_filter = LowBandFilter::new(source_rate);
            vocal_analyzer = VocalActivityAnalyzer::new(source_rate);
        }
        let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        buffer.copy_interleaved_ref(decoded);
        for sample in buffer.samples() {
            let sample = *sample;
            sum_squares += f64::from(sample) * f64::from(sample);
            loudness_samples = loudness_samples.saturating_add(1);
            sample_peak = sample_peak.max(sample.abs());
        }
        for frame in buffer.samples().chunks(channels) {
            let sample = frame
                .iter()
                .copied()
                .max_by(|left, right| left.abs().total_cmp(&right.abs()))
                .unwrap_or_default();
            let stable_mono = frame.iter().copied().sum::<f32>() / channels as f32;
            let low_band_sample = low_band_filter.as_mut()?.process(stable_mono);
            vocal_analyzer.as_mut()?.push(stable_mono);
            structure_sum_squares += frame
                .iter()
                .map(|sample| f64::from(*sample) * f64::from(*sample))
                .sum::<f64>()
                / channels as f64;
            structure_frames += 1;
            structure_phase += STRUCTURE_RATE as u64;
            if structure_phase >= source_rate as u64 {
                structure_phase -= source_rate as u64;
                structure_rms
                    .push((structure_sum_squares / structure_frames.max(1) as f64).sqrt() as f32);
                structure_sum_squares = 0.0;
                structure_frames = 0;
            }
            accumulator += sample;
            low_band_accumulator += low_band_sample;
            accumulated += 1;
            phase += ANALYSIS_RATE as u64;
            if phase >= source_rate as u64 {
                phase -= source_rate as u64;
                mono.push(accumulator / accumulated as f32);
                low_band.push(low_band_accumulator / accumulated as f32);
                accumulator = 0.0;
                low_band_accumulator = 0.0;
                accumulated = 0;
                if mono.len() > max_samples {
                    break;
                }
            }
            if source_rate >= TONAL_RATE && tonal.len() < TONAL_RATE as usize * MAX_TONAL_SECONDS {
                tonal_accumulator += sample;
                tonal_accumulated += 1;
                tonal_phase += TONAL_RATE as u64;
                if tonal_phase >= source_rate as u64 {
                    tonal_phase -= source_rate as u64;
                    tonal.push(tonal_accumulator / tonal_accumulated as f32);
                    tonal_accumulator = 0.0;
                    tonal_accumulated = 0;
                }
            }
        }
        if mono.len() > max_samples {
            break;
        }
    }
    if mono.len() > max_samples {
        return None;
    }
    let mut analysis = (!mono.is_empty())
        .then(|| analyze_mono_pcm_with_low_band(&mono, &low_band, ANALYSIS_RATE))
        .flatten()?;
    apply_energy_structure(&mut analysis, &structure_rms, STRUCTURE_RATE);
    apply_vocal_activity(&mut analysis, vocal_analyzer?.finish());
    analysis.musical_key = estimate_musical_key(&tonal, TONAL_RATE);
    if loudness_samples > 0 && sum_squares > f64::EPSILON {
        analysis.rms_dbfs = Some((10.0 * (sum_squares / loudness_samples as f64).log10()) as f32);
    }
    if sample_peak > f32::EPSILON {
        analysis.sample_peak_dbfs = Some(20.0 * sample_peak.log10());
    }
    Some(analysis)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use songbird::input::codecs::{get_codec_registry, get_probe};

    use super::*;

    #[tokio::test]
    async fn decodes_wav_and_detects_click_track_tempo() {
        let input = Input::from(click_track_wav());
        let playable = input
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .expect("generated WAV should be playable");

        let analysis = analyze_input(playable).expect("WAV should produce an analysis");

        assert!(analysis.duration.abs_diff(Duration::from_secs(12)) < Duration::from_millis(1));
        assert!((analysis.bpm.expect("tempo should be detected") - 120.0).abs() < 1.0);
        assert!(analysis.beat_confidence > 0.7);
        assert!(analysis.audible_start.abs_diff(Duration::from_secs(1)) < Duration::from_millis(5));
        assert!(
            analysis
                .first_beat
                .expect("first beat should be detected")
                .abs_diff(Duration::from_secs(1))
                < Duration::from_millis(15)
        );
    }

    #[tokio::test]
    async fn decodes_low_band_kicks_before_analysis_downsampling() {
        let input = Input::from(kick_hat_wav());
        let playable = input
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .expect("generated WAV should be playable");

        let analysis = analyze_input(playable).expect("WAV should produce an analysis");

        assert!((analysis.bpm.unwrap() - 120.0).abs() < 0.7, "{analysis:?}");
        assert!(
            analysis
                .first_beat
                .unwrap()
                .abs_diff(Duration::from_secs(1))
                < Duration::from_millis(30),
            "{analysis:?}"
        );
        assert!(analysis.trusted_kick_coverage() > 0.6, "{analysis:?}");
    }

    #[tokio::test]
    async fn decodes_source_rate_energy_structure() {
        let input = Input::from(structured_wav());
        let playable = input
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .expect("generated WAV should be playable");

        let analysis = analyze_input(playable).expect("WAV should produce an analysis");

        assert!(analysis.intro_end.is_some(), "{analysis:?}");
        assert!(analysis.outro_start.is_some(), "{analysis:?}");
        assert!(
            analysis.intro_end.unwrap().abs_diff(Duration::from_secs(6)) <= Duration::from_secs(1),
            "{analysis:?}"
        );
        assert!(
            analysis
                .outro_start
                .unwrap()
                .abs_diff(Duration::from_secs(22))
                <= Duration::from_secs(1),
            "{analysis:?}"
        );
        assert_eq!(analysis.energy_profile_rate, STRUCTURE_RATE as u8);
        assert!(!analysis.energy_profile.is_empty(), "{analysis:?}");
    }

    #[tokio::test]
    async fn decodes_source_rate_vocal_activity() {
        let input = Input::from(vocal_wav());
        let playable = input
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .expect("generated WAV should be playable");

        let analysis = analyze_input(playable).expect("WAV should produce an analysis");

        assert_eq!(analysis.vocal_activity_rate, 4, "{analysis:?}");
        assert_eq!(analysis.vocal_activity.len(), 48, "{analysis:?}");
        assert_eq!(
            analysis.vocal_activity.len(),
            analysis.vocal_activity_confidences.len(),
            "{analysis:?}"
        );
        let active = analysis.vocal_activity[8..40]
            .iter()
            .filter(|risk| **risk >= 140)
            .count();
        assert!(active >= 20, "{analysis:?}");
        assert!(
            analysis.vocal_activity[..8]
                .iter()
                .chain(&analysis.vocal_activity[40..])
                .all(|risk| *risk < 140),
            "{analysis:?}"
        );
        assert!(
            analysis
                .vocal_activity_confidences
                .iter()
                .map(|confidence| usize::from(*confidence))
                .sum::<usize>()
                / analysis.vocal_activity_confidences.len()
                >= 153,
            "{analysis:?}"
        );
    }

    #[tokio::test]
    async fn rejects_audio_longer_than_the_analysis_limit() {
        let input = Input::from(click_track_wav());
        let playable = input
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .expect("generated WAV should be playable");

        assert!(
            analyze_input_with_limit(playable, ANALYSIS_RATE as usize, &AtomicBool::new(false))
                .is_none()
        );
    }

    #[tokio::test]
    async fn cancelled_analysis_stops_before_decode() {
        let input = Input::from(click_track_wav());
        let playable = input
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .expect("generated WAV should be playable");
        let cancelled = AtomicBool::new(true);

        assert!(analyze_input_with_cancel(playable, &cancelled).is_none());
    }

    #[tokio::test]
    async fn decodes_wav_and_detects_musical_key() {
        let input = Input::from(c_major_wav());
        let playable = input
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .expect("generated WAV should be playable");

        let analysis = analyze_input(playable).expect("WAV should produce an analysis");
        let key = analysis.musical_key.expect("key should be detected");
        assert_eq!(key.tonic, 0);
        assert_eq!(key.mode, wotoha_core::automix::KeyMode::Major);
        let rms = analysis.rms_dbfs.expect("RMS level should be measured");
        let peak = analysis
            .sample_peak_dbfs
            .expect("sample peak should be measured");
        assert!(rms < peak);
        assert!(peak <= 0.01);
    }

    fn click_track_wav() -> Vec<u8> {
        const SAMPLE_RATE: u32 = 44_100;
        const SECONDS: usize = 12;
        const CLICK_SAMPLES: usize = SAMPLE_RATE as usize / 50;

        let sample_count = SAMPLE_RATE as usize * SECONDS;
        let data_len = sample_count * size_of::<i16>();
        let mut wav = Vec::with_capacity(44 + data_len);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&(SAMPLE_RATE * size_of::<i16>() as u32).to_le_bytes());
        wav.extend_from_slice(&(size_of::<i16>() as u16).to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data_len as u32).to_le_bytes());

        for index in 0..sample_count {
            let after_lead_in = index.saturating_sub(SAMPLE_RATE as usize);
            let is_click = index >= SAMPLE_RATE as usize
                && after_lead_in % (SAMPLE_RATE as usize / 2) < CLICK_SAMPLES
                && index < SAMPLE_RATE as usize * 11;
            let sample = if is_click { i16::MAX } else { 0 };
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }

    fn kick_hat_wav() -> Vec<u8> {
        const SAMPLE_RATE: u32 = 16_000;
        const SECONDS: usize = 14;
        let samples = (0..SAMPLE_RATE as usize * SECONDS)
            .map(|index| {
                let time = index as f32 / SAMPLE_RATE as f32;
                let kick_phase = (time - 1.0).rem_euclid(0.5);
                let hat_phase = (time - 1.25).rem_euclid(0.5);
                let kick = if (1.0..13.0).contains(&time) && kick_phase < 0.07 {
                    0.3 * (std::f32::consts::TAU * 80.0 * kick_phase).sin()
                        * (1.0 - kick_phase / 0.07)
                } else {
                    0.0
                };
                let hat = if (1.25..13.0).contains(&time) && hat_phase < 0.015 {
                    0.8 * (std::f32::consts::TAU * 3_200.0 * hat_phase).sin()
                        * (1.0 - hat_phase / 0.015)
                } else {
                    0.0
                };
                ((kick + hat).clamp(-1.0, 1.0) * i16::MAX as f32) as i16
            })
            .collect::<Vec<_>>();
        mono_wav(SAMPLE_RATE, &samples)
    }

    fn structured_wav() -> Vec<u8> {
        const SAMPLE_RATE: u32 = 16_000;
        const SECONDS: usize = 30;
        let samples = (0..SAMPLE_RATE as usize * SECONDS)
            .map(|index| {
                let time = index as f32 / SAMPLE_RATE as f32;
                let gain = if (1.0..6.0).contains(&time) || (22.0..29.0).contains(&time) {
                    0.08
                } else if (6.0..22.0).contains(&time) {
                    0.6
                } else {
                    0.0
                };
                (gain * (std::f32::consts::TAU * 100.0 * time).sin() * i16::MAX as f32) as i16
            })
            .collect::<Vec<_>>();
        mono_wav(SAMPLE_RATE, &samples)
    }

    fn vocal_wav() -> Vec<u8> {
        const SAMPLE_RATE: u32 = 16_000;
        const SECONDS: usize = 12;
        let samples = (0..SAMPLE_RATE as usize * SECONDS)
            .map(|index| {
                let time = index as f32 / SAMPLE_RATE as f32;
                let mut sample = 0.15 * (std::f32::consts::TAU * 80.0 * time).sin();
                if (2.0..10.0).contains(&time) {
                    let syllable = (time - 2.0).rem_euclid(0.25);
                    if syllable < 0.16 {
                        let envelope = (std::f32::consts::PI * syllable / 0.16).sin().powi(2);
                        for (frequency, gain) in [
                            (220.0, 0.04),
                            (700.0, 0.12),
                            (1_200.0, 0.09),
                            (2_400.0, 0.04),
                        ] {
                            sample +=
                                gain * (std::f32::consts::TAU * frequency * time).sin() * envelope;
                        }
                    }
                }
                (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
            })
            .collect::<Vec<_>>();
        mono_wav(SAMPLE_RATE, &samples)
    }

    fn mono_wav(sample_rate: u32, samples: &[i16]) -> Vec<u8> {
        let data_len = std::mem::size_of_val(samples);
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
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }

    fn c_major_wav() -> Vec<u8> {
        const SAMPLE_RATE: u32 = 44_100;
        const SECONDS: usize = 8;
        let samples = (0..SAMPLE_RATE as usize * SECONDS)
            .map(|index| {
                let time = index as f32 / SAMPLE_RATE as f32;
                let value = [261.63_f32, 329.63, 392.0]
                    .into_iter()
                    .map(|frequency| (std::f32::consts::TAU * frequency * time).sin())
                    .sum::<f32>()
                    / 3.0;
                (value * i16::MAX as f32 * 0.8) as i16
            })
            .collect::<Vec<_>>();
        let data_len = samples.len() * size_of::<i16>();
        let mut wav = Vec::with_capacity(44 + data_len);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&(SAMPLE_RATE * size_of::<i16>() as u32).to_le_bytes());
        wav.extend_from_slice(&(size_of::<i16>() as u16).to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data_len as u32).to_le_bytes());
        for sample in samples {
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }
}
