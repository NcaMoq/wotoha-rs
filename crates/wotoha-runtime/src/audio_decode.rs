use songbird::input::{Input, LiveInput};
use symphonia::core::{audio::SampleBuffer, errors::Error};
use wotoha_core::{audio_analysis::analyze_mono_pcm, automix::TrackAnalysis};

const ANALYSIS_RATE: u32 = 1_000;
const MAX_ANALYSIS_SECONDS: usize = 30 * 60;

pub(crate) fn analyze_input(input: Input) -> Option<TrackAnalysis> {
    let Input::Live(LiveInput::Parsed(mut parsed), _) = input else {
        return None;
    };
    let mut mono = Vec::with_capacity(ANALYSIS_RATE as usize * 180);
    let mut accumulator = 0.0_f32;
    let mut accumulated = 0_usize;
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
        if channels == 0 || source_rate == 0 {
            continue;
        }
        let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        buffer.copy_interleaved_ref(decoded);
        let frames_per_output = (source_rate / ANALYSIS_RATE).max(1) as usize;
        for frame in buffer.samples().chunks(channels) {
            accumulator += frame.iter().sum::<f32>() / channels as f32;
            accumulated += 1;
            if accumulated >= frames_per_output {
                mono.push(accumulator / accumulated as f32);
                accumulator = 0.0;
                accumulated = 0;
                if mono.len() >= ANALYSIS_RATE as usize * MAX_ANALYSIS_SECONDS {
                    break;
                }
            }
        }
        if mono.len() >= ANALYSIS_RATE as usize * MAX_ANALYSIS_SECONDS {
            break;
        }
    }
    (!mono.is_empty())
        .then(|| analyze_mono_pcm(&mono, ANALYSIS_RATE))
        .flatten()
}
