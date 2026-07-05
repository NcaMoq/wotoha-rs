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
    let Some((bpm, confidence, beat_lag)) = estimate_tempo(&onset) else {
        return Some(TrackAnalysis {
            duration: duration(samples.len(), sample_rate),
            audible_start: duration(first, sample_rate),
            audible_end: duration(last.saturating_add(1), sample_rate),
            bpm: None,
            beat_confidence: 0.0,
            first_beat: None,
            first_downbeat: None,
            downbeat_confidence: 0.0,
            musical_key: None,
            rms_dbfs: None,
            sample_peak_dbfs: None,
        });
    };
    let onset_peak = onset.iter().copied().fold(0.0_f32, f32::max);
    let first_beat_block = onset
        .iter()
        .position(|value| *value >= onset_peak * 0.5)
        .map(|index| index + 1)
        .unwrap_or_default();
    if first_beat_block == 0 {
        return Some(TrackAnalysis {
            duration: duration(samples.len(), sample_rate),
            audible_start: duration(first, sample_rate),
            audible_end: duration(last.saturating_add(1), sample_rate),
            bpm: None,
            beat_confidence: 0.0,
            first_beat: None,
            first_downbeat: None,
            downbeat_confidence: 0.0,
            musical_key: None,
            rms_dbfs: None,
            sample_peak_dbfs: None,
        });
    }
    let (downbeat_offset, downbeat_confidence) =
        estimate_downbeat_phase(&onset, first_beat_block - 1, beat_lag);
    let first_downbeat_block = first_beat_block + downbeat_offset * beat_lag;

    Some(TrackAnalysis {
        duration: duration(samples.len(), sample_rate),
        audible_start: duration(first, sample_rate),
        audible_end: duration(last.saturating_add(1), sample_rate),
        bpm: Some(bpm),
        beat_confidence: confidence,
        first_beat: Some(duration(first_beat_block * block, sample_rate)),
        first_downbeat: Some(duration(first_downbeat_block * block, sample_rate)),
        downbeat_confidence,
        musical_key: None,
        rms_dbfs: None,
        sample_peak_dbfs: None,
    })
}

fn estimate_downbeat_phase(onset: &[f32], first_beat: usize, beat_lag: usize) -> (usize, f32) {
    if beat_lag == 0 {
        return (0, 0.0);
    }
    let mut sums = [0.0_f32; 4];
    let mut counts = [0_u32; 4];
    let mut beat = 0_usize;
    let mut position = first_beat;
    while position < onset.len() {
        let from = position.saturating_sub(1);
        let to = (position + 2).min(onset.len());
        sums[beat % 4] += onset[from..to].iter().copied().fold(0.0_f32, f32::max);
        counts[beat % 4] += 1;
        beat += 1;
        position = position.saturating_add(beat_lag);
    }
    let means = std::array::from_fn::<_, 4, _>(|phase| {
        (counts[phase] > 0)
            .then(|| sums[phase] / counts[phase] as f32)
            .unwrap_or_default()
    });
    let mut ranked = means.into_iter().enumerate().collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.total_cmp(&left.1));
    let (phase, best) = ranked[0];
    let second = ranked[1].1;
    let confidence = if best > f32::EPSILON && beat >= 8 {
        ((best - second) / best).clamp(0.0, 1.0)
    } else {
        0.0
    };
    if confidence < 0.05 {
        (0, confidence)
    } else {
        (phase, confidence)
    }
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
        assert_eq!(analysis.first_downbeat, Some(Duration::from_secs(1)));
    }

    #[test]
    fn silent_pcm_falls_back_to_unanalyzed() {
        let analysis = analyze_mono_pcm(&vec![0.0; 1_000], 1_000).unwrap();
        assert_eq!(analysis.bpm, None);
        assert_eq!(analysis.beat_confidence, 0.0);
        assert_eq!(analysis.first_downbeat, None);
    }

    #[test]
    fn detects_downbeat_phase_from_four_beat_accents() {
        let sample_rate = 1_000;
        let mut samples = vec![0.0; 18_000];
        for (beat_index, beat) in (1_000..17_000).step_by(500).enumerate() {
            let level = if beat_index % 4 == 2 { 1.0 } else { 0.55 };
            for sample in &mut samples[beat..beat + 20] {
                *sample = level;
            }
        }

        let analysis = analyze_mono_pcm(&samples, sample_rate).unwrap();
        assert_eq!(analysis.first_beat, Some(Duration::from_secs(1)));
        assert_eq!(analysis.first_downbeat, Some(Duration::from_secs(2)));
        assert!(analysis.downbeat_confidence > 0.35);
    }
}
