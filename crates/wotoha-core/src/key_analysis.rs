use realfft::RealFftPlanner;

use crate::automix::{KeyMode, MusicalKey};

const FFT_SIZE: usize = 4_096;
const HOP_SIZE: usize = 2_048;
const MIN_FREQUENCY: f32 = 55.0;
const MAX_FREQUENCY: f32 = 2_000.0;

const MAJOR_PROFILE: [f32; 12] = [
    6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
];
const MINOR_PROFILE: [f32; 12] = [
    6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
];

/// Estimates a global major/minor key from mono PCM using a chroma spectrum.
pub fn estimate_musical_key(samples: &[f32], sample_rate: u32) -> Option<MusicalKey> {
    if sample_rate < 8_000 || samples.len() < FFT_SIZE {
        return None;
    }
    let mut planner = RealFftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let mut input = fft.make_input_vec();
    let mut spectrum = fft.make_output_vec();
    let mut chroma = [0.0_f64; 12];
    let mut windows = 0_u32;

    for frame in samples.windows(FFT_SIZE).step_by(HOP_SIZE) {
        for (index, (target, sample)) in input.iter_mut().zip(frame).enumerate() {
            let window =
                0.5 - 0.5 * (std::f32::consts::TAU * index as f32 / (FFT_SIZE - 1) as f32).cos();
            *target = *sample * window;
        }
        fft.process(&mut input, &mut spectrum).ok()?;
        let mut frame_chroma = [0.0_f64; 12];
        for (bin, value) in spectrum.iter().enumerate().skip(1) {
            let frequency = bin as f32 * sample_rate as f32 / FFT_SIZE as f32;
            if !(MIN_FREQUENCY..=MAX_FREQUENCY).contains(&frequency) {
                continue;
            }
            let midi = (69.0 + 12.0 * (frequency / 440.0).log2()).round() as i32;
            let pitch_class = midi.rem_euclid(12) as usize;
            frame_chroma[pitch_class] += value.norm_sqr() as f64 / frequency as f64;
        }
        let total = frame_chroma.iter().sum::<f64>();
        if total > f64::EPSILON {
            for (aggregate, energy) in chroma.iter_mut().zip(frame_chroma) {
                *aggregate += energy / total;
            }
            windows += 1;
        }
    }
    if windows < 3 {
        return None;
    }

    let mut candidates = Vec::with_capacity(24);
    for tonic in 0..12 {
        candidates.push((
            profile_similarity(&chroma, &MAJOR_PROFILE, tonic),
            tonic as u8,
            KeyMode::Major,
        ));
        candidates.push((
            profile_similarity(&chroma, &MINOR_PROFILE, tonic),
            tonic as u8,
            KeyMode::Minor,
        ));
    }
    candidates.sort_by(|left, right| right.0.total_cmp(&left.0));
    let (best, tonic, mode) = candidates[0];
    let runner_up = candidates[1].0;
    let confidence = ((best - runner_up) / best.max(0.001) * 8.0).clamp(0.0, 1.0);
    (confidence >= 0.15).then_some(MusicalKey {
        tonic,
        mode,
        confidence,
    })
}

fn profile_similarity(chroma: &[f64; 12], profile: &[f32; 12], tonic: usize) -> f32 {
    let mut dot = 0.0_f64;
    let mut chroma_norm = 0.0_f64;
    let mut profile_norm = 0.0_f64;
    for pitch_class in 0..12 {
        let expected = profile[(pitch_class + 12 - tonic) % 12] as f64;
        dot += chroma[pitch_class] * expected;
        chroma_norm += chroma[pitch_class] * chroma[pitch_class];
        profile_norm += expected * expected;
    }
    (dot / (chroma_norm.sqrt() * profile_norm.sqrt()).max(f64::EPSILON)) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_c_major_triad() {
        let samples = chord(&[261.63, 329.63, 392.0]);
        let key = estimate_musical_key(&samples, 11_025).expect("key should be detected");
        assert_eq!(key.tonic, 0);
        assert_eq!(key.mode, KeyMode::Major);
    }

    #[test]
    fn rejects_audio_too_short_for_stable_chroma() {
        assert!(estimate_musical_key(&[0.0; 2_000], 11_025).is_none());
    }

    fn chord(frequencies: &[f32]) -> Vec<f32> {
        let sample_rate = 11_025.0;
        (0..44_100)
            .map(|index| {
                frequencies
                    .iter()
                    .map(|frequency| {
                        (std::f32::consts::TAU * frequency * index as f32 / sample_rate).sin()
                    })
                    .sum::<f32>()
                    / frequencies.len() as f32
            })
            .collect()
    }
}
