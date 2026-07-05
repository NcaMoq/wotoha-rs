use songbird::input::{Input, LiveInput};
use symphonia::core::{audio::SampleBuffer, errors::Error};
use wotoha_core::{audio_analysis::analyze_mono_pcm, automix::TrackAnalysis};

const ANALYSIS_RATE: u32 = 1_000;
const MAX_ANALYSIS_SECONDS: usize = 30 * 60;

pub(crate) fn analyze_input(input: Input) -> Option<TrackAnalysis> {
    analyze_input_with_limit(input, ANALYSIS_RATE as usize * MAX_ANALYSIS_SECONDS)
}

fn analyze_input_with_limit(input: Input, max_samples: usize) -> Option<TrackAnalysis> {
    let Input::Live(LiveInput::Parsed(mut parsed), _) = input else {
        return None;
    };
    let mut mono = Vec::with_capacity(ANALYSIS_RATE as usize * 180);
    let mut accumulator = 0.0_f32;
    let mut accumulated = 0_usize;
    let mut phase = 0_u64;
    let mut stream_rate = None;
    loop {
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
        let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        buffer.copy_interleaved_ref(decoded);
        for frame in buffer.samples().chunks(channels) {
            let sample = frame
                .iter()
                .copied()
                .max_by(|left, right| left.abs().total_cmp(&right.abs()))
                .unwrap_or_default();
            accumulator += sample;
            accumulated += 1;
            phase += ANALYSIS_RATE as u64;
            if phase >= source_rate as u64 {
                phase -= source_rate as u64;
                mono.push(accumulator / accumulated as f32);
                accumulator = 0.0;
                accumulated = 0;
                if mono.len() > max_samples {
                    break;
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
    (!mono.is_empty())
        .then(|| analyze_mono_pcm(&mono, ANALYSIS_RATE))
        .flatten()
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
    async fn rejects_audio_longer_than_the_analysis_limit() {
        let input = Input::from(click_track_wav());
        let playable = input
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .expect("generated WAV should be playable");

        assert!(analyze_input_with_limit(playable, ANALYSIS_RATE as usize).is_none());
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
}
