use std::time::Duration;

use crate::automix::TrackAnalysis;

const MIN_BPM: usize = 70;
const MAX_BPM: usize = 180;
const ONSET_BLOCKS_PER_SECOND: usize = 200;

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

    let block = (sample_rate as usize / ONSET_BLOCKS_PER_SECOND).max(1);
    let onset_rate = sample_rate as f32 / block as f32;
    let energy = samples
        .chunks(block)
        .map(|chunk| chunk.iter().map(|sample| sample * sample).sum::<f32>() / chunk.len() as f32)
        .collect::<Vec<_>>();
    let onset = energy
        .windows(2)
        .map(|pair| (pair[1].sqrt() - pair[0].sqrt()).max(0.0))
        .collect::<Vec<_>>();
    let Some((bpm, confidence, beat_lag)) = estimate_tempo(&onset, onset_rate) else {
        return Some(TrackAnalysis {
            duration: duration(samples.len(), sample_rate),
            audible_start: duration(first, sample_rate),
            audible_end: duration(last.saturating_add(1), sample_rate),
            bpm: None,
            beat_confidence: 0.0,
            first_beat: None,
            beat_markers: Vec::new(),
            first_downbeat: None,
            downbeat_confidence: 0.0,
            musical_key: None,
            rms_dbfs: None,
            sample_peak_dbfs: None,
        });
    };
    let audible_start_block = first / block;
    let audible_end_block = last / block;
    let beat_markers =
        estimate_beat_markers(&onset, beat_lag, audible_start_block, audible_end_block);
    let first_beat_block = beat_markers
        .first()
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
            beat_markers: Vec::new(),
            first_downbeat: None,
            downbeat_confidence: 0.0,
            musical_key: None,
            rms_dbfs: None,
            sample_peak_dbfs: None,
        });
    }
    let beat_lag_blocks = beat_lag.round().max(1.0) as usize;
    let (downbeat_offset, downbeat_confidence) =
        estimate_downbeat_phase(&onset, first_beat_block - 1, beat_lag_blocks);
    let first_downbeat_block = first_beat_block + downbeat_offset * beat_lag_blocks;

    Some(TrackAnalysis {
        duration: duration(samples.len(), sample_rate),
        audible_start: duration(first, sample_rate),
        audible_end: duration(last.saturating_add(1), sample_rate),
        bpm: Some(bpm),
        beat_confidence: confidence,
        first_beat: Some(duration(first_beat_block * block, sample_rate)),
        beat_markers: beat_markers
            .into_iter()
            .map(|index| duration((index + 1) * block, sample_rate))
            .collect(),
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
        if counts[phase] > 0 {
            sums[phase] / counts[phase] as f32
        } else {
            0.0
        }
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

fn estimate_tempo(onset: &[f32], onset_rate: f32) -> Option<(f32, f32, f32)> {
    if onset.len() < onset_rate as usize || onset_rate <= 0.0 {
        return None;
    }
    let energy = onset.iter().map(|value| value * value).sum::<f32>();
    if energy <= f32::EPSILON {
        return None;
    }
    let minimum_lag = (onset_rate * 60.0 / MAX_BPM as f32).floor() as usize;
    let maximum_lag = (onset_rate * 60.0 / MIN_BPM as f32).ceil() as usize;
    let scores = (minimum_lag.max(1)..=maximum_lag.min(onset.len() - 1))
        .map(|lag| (lag, autocorrelation(onset, lag)))
        .collect::<Vec<_>>();
    let best_index = scores
        .iter()
        .enumerate()
        .max_by(|left, right| left.1.1.total_cmp(&right.1.1))?
        .0;
    let (best_lag, best_score) = scores[best_index];
    let fractional_offset = if best_index > 0 && best_index + 1 < scores.len() {
        parabolic_peak_offset(
            scores[best_index - 1].1,
            best_score,
            scores[best_index + 1].1,
        )
    } else {
        0.0
    };
    let refined_lag = best_lag as f32 + fractional_offset;
    (refined_lag > 0.0).then(|| {
        (
            onset_rate * 60.0 / refined_lag,
            (best_score / energy).clamp(0.0, 1.0),
            refined_lag,
        )
    })
}

fn autocorrelation(onset: &[f32], lag: usize) -> f32 {
    onset
        .iter()
        .zip(onset.iter().skip(lag))
        .map(|(left, right)| left * right)
        .sum()
}

fn parabolic_peak_offset(left: f32, center: f32, right: f32) -> f32 {
    let denominator = left - 2.0 * center + right;
    if denominator.abs() <= f32::EPSILON {
        0.0
    } else {
        (0.5 * (left - right) / denominator).clamp(-0.5, 0.5)
    }
}

fn estimate_beat_markers(
    onset: &[f32],
    beat_lag: f32,
    audible_start: usize,
    audible_end: usize,
) -> Vec<usize> {
    if onset.is_empty() || beat_lag <= 1.0 || !beat_lag.is_finite() {
        return Vec::new();
    }
    const PHASE_STEPS_PER_BLOCK: usize = 4;
    let phase_steps = (beat_lag * PHASE_STEPS_PER_BLOCK as f32).ceil() as usize;
    let phase = (0..phase_steps)
        .map(|step| {
            let phase = step as f32 / PHASE_STEPS_PER_BLOCK as f32;
            let mut score = 0.0;
            let mut count = 0_u32;
            let mut position = phase;
            while position < onset.len() as f32 {
                score += interpolated(onset, position);
                count += 1;
                position += beat_lag;
            }
            (score / count.max(1) as f32, phase)
        })
        .max_by(|left, right| left.0.total_cmp(&right.0))
        .map(|(_, phase)| phase);
    let Some(phase) = phase else {
        return Vec::new();
    };

    let peak = onset.iter().copied().fold(0.0_f32, f32::max);
    let snap_threshold = peak * 0.1;
    let search_radius = (beat_lag * 0.2).round().clamp(5.0, 20.0) as usize;
    let mut candidates = Vec::new();
    let mut position = phase;
    while position + (search_radius as f32) < audible_start as f32 {
        position += beat_lag;
    }
    while position < onset.len() as f32 && position <= audible_end as f32 + search_radius as f32 {
        let center = position.round() as usize;
        let from = center.saturating_sub(search_radius);
        let to = center.saturating_add(search_radius + 1).min(onset.len());
        if from < to {
            let (offset, strength) = match onset[from..to]
                .iter()
                .copied()
                .enumerate()
                .max_by(|left, right| left.1.total_cmp(&right.1))
            {
                Some(candidate) => candidate,
                None => {
                    position += beat_lag;
                    continue;
                }
            };
            let detected = from + offset;
            candidates.push((center, detected, strength));
            if strength >= snap_threshold {
                position = detected as f32 + beat_lag;
                continue;
            }
        }
        position += beat_lag;
    }
    let Some(first) = candidates.iter().position(|(_, position, strength)| {
        *position + 1 >= audible_start && *strength >= peak * 0.25
    }) else {
        return Vec::new();
    };
    candidates[first..]
        .iter()
        .map(|(predicted, detected, strength)| {
            if *strength >= snap_threshold {
                *detected
            } else {
                *predicted
            }
        })
        .filter(|position| *position <= audible_end)
        .fold(Vec::new(), |mut markers, position| {
            if markers.last().is_none_or(|previous| *previous < position) {
                markers.push(position);
            }
            markers
        })
}

fn interpolated(values: &[f32], position: f32) -> f32 {
    let left = position.floor().max(0.0) as usize;
    let right = (left + 1).min(values.len().saturating_sub(1));
    let fraction = position - left as f32;
    values[left] * (1.0 - fraction) + values[right] * fraction
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

    #[test]
    fn estimates_fractional_tempo_without_accumulating_beat_drift() {
        let sample_rate = 1_000;
        let bpm = 123.0_f32;
        let interval = 60.0 / bpm;
        let mut samples = vec![0.0; 62_000];
        let mut beat = 1.0_f32;
        while beat < 61.0 {
            let position = (beat * sample_rate as f32).round() as usize;
            samples[position..position + 8].fill(1.0);
            beat += interval;
        }

        let analysis = analyze_mono_pcm(&samples, sample_rate).unwrap();
        assert!((analysis.bpm.unwrap() - bpm).abs() < 0.15, "{analysis:?}");
        assert!(
            analysis
                .first_beat
                .unwrap()
                .abs_diff(Duration::from_secs(1))
                < Duration::from_millis(8),
            "{analysis:?}"
        );
    }

    #[test]
    fn locks_phase_to_the_repeating_kick_grid_instead_of_a_loud_pickup() {
        let sample_rate = 1_000;
        let mut samples = vec![0.0; 20_000];
        samples[1_250..1_270].fill(1.0);
        for beat in (2_000..19_000).step_by(500) {
            samples[beat..beat + 12].fill(0.7);
        }

        let analysis = analyze_mono_pcm(&samples, sample_rate).unwrap();
        assert!((analysis.bpm.unwrap() - 120.0).abs() < 0.2);
        assert!(
            analysis
                .first_beat
                .unwrap()
                .abs_diff(Duration::from_secs(2))
                < Duration::from_millis(8),
            "{analysis:?}"
        );
    }

    #[test]
    fn beat_markers_follow_gradual_tempo_drift() {
        let sample_rate = 1_000;
        let mut samples = vec![0.0; 62_000];
        let mut beat = 1.0_f32;
        let mut beats = Vec::new();
        while beat < 61.0 {
            beats.push(beat);
            let progress = ((beat - 1.0) / 60.0).clamp(0.0, 1.0);
            let bpm = 120.0 + 3.0 * progress;
            beat += 60.0 / bpm;
        }
        for beat in &beats {
            let position = (*beat * sample_rate as f32).round() as usize;
            samples[position..position + 8].fill(1.0);
        }

        let analysis = analyze_mono_pcm(&samples, sample_rate).unwrap();
        let expected_last = Duration::from_secs_f32(*beats.last().unwrap());
        let detected_last = *analysis.beat_markers.last().unwrap();
        assert!(
            detected_last.abs_diff(expected_last) < Duration::from_millis(15),
            "expected={expected_last:?} detected={detected_last:?} analysis={analysis:?}"
        );
        let trailing_intervals = analysis
            .beat_markers
            .windows(2)
            .rev()
            .take(8)
            .map(|pair| pair[1].saturating_sub(pair[0]).as_secs_f32())
            .collect::<Vec<_>>();
        let local_bpm = 60.0 / (trailing_intervals.iter().sum::<f32>() / 8.0);
        assert!(local_bpm > 122.0, "local_bpm={local_bpm}");
    }
}
