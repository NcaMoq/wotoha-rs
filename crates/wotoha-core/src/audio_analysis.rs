use std::time::Duration;

use crate::automix::TrackAnalysis;

const MIN_BPM: usize = 70;
const MAX_BPM: usize = 180;

/// Lightweight mono PCM analysis used by AutoMix. Samples must be normalized to -1..=1.
pub fn analyze_mono_pcm(samples: &[f32], sample_rate: u32) -> Option<TrackAnalysis> {
    if samples.is_empty() || sample_rate == 0 {
        return None;
    }
    let peak = samples
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    if peak <= f32::EPSILON {
        return Some(TrackAnalysis::unanalyzed(duration(
            samples.len(),
            sample_rate,
        )));
    }
    let threshold = (peak * 0.02).max(0.0005);
    let first = samples
        .iter()
        .position(|sample| sample.abs() >= threshold)
        .unwrap_or(0);
    let last = samples
        .iter()
        .rposition(|sample| sample.abs() >= threshold)
        .unwrap_or(samples.len() - 1);

    let block = (sample_rate as usize / 100).max(1);
    let energy = samples
        .chunks(block)
        .map(|chunk| chunk.iter().map(|sample| sample * sample).sum::<f32>() / chunk.len() as f32)
        .collect::<Vec<_>>();
    let onset = energy
        .windows(2)
        .map(|pair| (pair[1].sqrt() - pair[0].sqrt()).max(0.0))
        .collect::<Vec<_>>();
    let (bpm, confidence, _) = estimate_tempo(&onset)?;
    let onset_peak = onset.iter().copied().fold(0.0_f32, f32::max);
    let first_beat_block = onset
        .iter()
        .position(|value| *value >= onset_peak * 0.5)
        .map(|index| index + 1)?;

    Some(TrackAnalysis {
        duration: duration(samples.len(), sample_rate),
        audible_start: duration(first, sample_rate),
        audible_end: duration(last.saturating_add(1), sample_rate),
        bpm: Some(bpm),
        beat_confidence: confidence,
        first_beat: Some(duration(first_beat_block * block, sample_rate)),
    })
}

fn estimate_tempo(onset: &[f32]) -> Option<(f32, f32, usize)> {
    if onset.len() < 100 {
        return None;
    }
    let energy = onset.iter().map(|value| value * value).sum::<f32>();
    if energy <= f32::EPSILON {
        return None;
    }
    let mut best = (0.0_f32, 0_usize);
    for bpm in MIN_BPM..=MAX_BPM {
        let lag = (6000.0 / bpm as f32).round() as usize;
        if lag >= onset.len() {
            continue;
        }
        let score = onset
            .iter()
            .zip(onset.iter().skip(lag))
            .map(|(left, right)| left * right)
            .sum::<f32>();
        if score > best.0 {
            best = (score, lag);
        }
    }
    (best.1 > 0).then(|| {
        (
            6000.0 / best.1 as f32,
            (best.0 / energy).clamp(0.0, 1.0),
            best.1,
        )
    })
}

fn duration(samples: usize, sample_rate: u32) -> Duration {
    Duration::from_secs_f64(samples as f64 / sample_rate as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_tempo_phase_and_silence_from_click_track() {
        let sample_rate = 1_000;
        let mut samples = vec![0.0; 12_000];
        for beat in (1_000..11_000).step_by(500) {
            for sample in &mut samples[beat..beat + 20] {
                *sample = 1.0;
            }
        }
        let analysis = analyze_mono_pcm(&samples, sample_rate).unwrap();
        assert!((analysis.bpm.unwrap() - 120.0).abs() < 1.0);
        assert!(analysis.beat_confidence > 0.7);
        assert_eq!(analysis.audible_start, Duration::from_secs(1));
        assert_eq!(analysis.first_beat, Some(Duration::from_secs(1)));
    }

    #[test]
    fn silent_pcm_falls_back_to_unanalyzed() {
        let analysis = analyze_mono_pcm(&vec![0.0; 1_000], 1_000).unwrap();
        assert_eq!(analysis.bpm, None);
        assert_eq!(analysis.beat_confidence, 0.0);
    }
}
