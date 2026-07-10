use std::time::Duration;

use crate::automix::TrackAnalysis;
use crate::vocal_analysis::{analyze_vocal_activity, apply_vocal_activity};

const MIN_BPM: usize = 70;
const MAX_BPM: usize = 180;
const ONSET_BLOCKS_PER_SECOND: usize = 200;
const MIN_KICK_TRACK_CONFIDENCE: f32 = 0.35;
const MIN_KICK_ENERGY_RATIO: f32 = 0.015;
const MIN_KICK_MARKER_CONFIDENCE: f32 = 0.25;
const MIN_KICK_FIRST_CONFIDENCE: f32 = 0.1;
const STRUCTURE_BINS_PER_SECOND: u32 = 4;
const MIN_ENERGY_PROFILE_DBFS: f32 = -80.0;

#[derive(Clone, Copy, Debug, Default)]
struct EnergyStructure {
    intro_end: Option<Duration>,
    intro_confidence: f32,
    outro_start: Option<Duration>,
    outro_confidence: f32,
}

/// Lightweight 40-180 Hz filter used before analysis-rate downsampling.
pub struct LowBandFilter {
    high_pass_alpha: f32,
    low_pass_alpha: f32,
    previous_input: f32,
    previous_high_pass_1: f32,
    high_pass_1: f32,
    high_pass_2: f32,
    low_pass_1: f32,
    low_pass_2: f32,
}

impl LowBandFilter {
    pub fn new(sample_rate: u32) -> Option<Self> {
        if sample_rate == 0 {
            return None;
        }
        let rate = sample_rate as f32;
        Some(Self {
            high_pass_alpha: (-std::f32::consts::TAU * 40.0 / rate).exp(),
            low_pass_alpha: (-std::f32::consts::TAU * 180.0 / rate).exp(),
            previous_input: 0.0,
            previous_high_pass_1: 0.0,
            high_pass_1: 0.0,
            high_pass_2: 0.0,
            low_pass_1: 0.0,
            low_pass_2: 0.0,
        })
    }

    pub fn process(&mut self, sample: f32) -> f32 {
        self.high_pass_1 = self.high_pass_alpha * (self.high_pass_1 + sample - self.previous_input);
        self.previous_input = sample;
        self.high_pass_2 = self.high_pass_alpha
            * (self.high_pass_2 + self.high_pass_1 - self.previous_high_pass_1);
        self.previous_high_pass_1 = self.high_pass_1;
        self.low_pass_1 =
            (1.0 - self.low_pass_alpha) * self.high_pass_2 + self.low_pass_alpha * self.low_pass_1;
        self.low_pass_2 =
            (1.0 - self.low_pass_alpha) * self.low_pass_1 + self.low_pass_alpha * self.low_pass_2;
        self.low_pass_2
    }
}

/// Lightweight mono PCM analysis used by AutoMix. Samples must be normalized to -1..=1.
pub fn analyze_mono_pcm(samples: &[f32], sample_rate: u32) -> Option<TrackAnalysis> {
    let mut filter = LowBandFilter::new(sample_rate)?;
    let low_band = samples
        .iter()
        .copied()
        .map(|sample| filter.process(sample))
        .collect::<Vec<_>>();
    let mut analysis = analyze_mono_pcm_with_low_band(samples, &low_band, sample_rate)?;
    apply_vocal_activity(&mut analysis, analyze_vocal_activity(samples, sample_rate));
    Some(analysis)
}

/// Analyzes full-band PCM plus a 40-180 Hz stream sampled at the same rate.
pub fn analyze_mono_pcm_with_low_band(
    samples: &[f32],
    low_band: &[f32],
    sample_rate: u32,
) -> Option<TrackAnalysis> {
    if samples.is_empty() || samples.len() != low_band.len() || sample_rate == 0 {
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
    let structure_rms = rms_envelope(samples, sample_rate, STRUCTURE_BINS_PER_SECOND);
    let structure = estimate_energy_structure(
        &structure_rms,
        STRUCTURE_BINS_PER_SECOND,
        duration(first, sample_rate),
        duration(last.saturating_add(1), sample_rate),
    );
    let energy_profile = quantize_energy_profile(&structure_rms);
    let energy_profile_rate = STRUCTURE_BINS_PER_SECOND as u8;

    let block = (sample_rate as usize / ONSET_BLOCKS_PER_SECOND).max(1);
    let onset_rate = sample_rate as f32 / block as f32;
    let energy = block_energy(samples, block);
    let onset = energy
        .windows(2)
        .map(|pair| (pair[1].sqrt() - pair[0].sqrt()).max(0.0))
        .collect::<Vec<_>>();
    let low_energy = block_energy(low_band, block);
    let low_smoothed = low_energy
        .windows(4)
        .map(|window| window.iter().sum::<f32>() / window.len() as f32)
        .collect::<Vec<_>>();
    let mut kick_onset = vec![0.0; 3];
    kick_onset.extend(
        low_smoothed
            .windows(2)
            .map(|pair| (pair[1].sqrt() - pair[0].sqrt()).max(0.0)),
    );
    kick_onset.resize(onset.len(), 0.0);

    let full_tempo = estimate_tempo(&onset, onset_rate);
    let low_tempo = estimate_tempo(&kick_onset, onset_rate);
    let full_energy_sum = energy.iter().sum::<f32>();
    let low_energy_ratio = low_energy.iter().sum::<f32>() / full_energy_sum.max(f32::EPSILON);
    let kick_reliable = low_tempo.is_some_and(|(_, confidence, _)| {
        confidence >= MIN_KICK_TRACK_CONFIDENCE && low_energy_ratio >= MIN_KICK_ENERGY_RATIO
    });
    let selected_tempo = if kick_reliable {
        match (low_tempo, full_tempo) {
            (Some(low), Some(full)) if (low.0 / full.0 - 1.0).abs() <= 0.03 => Some(full),
            (low, _) => low,
        }
    } else {
        full_tempo
    };
    let Some((bpm, confidence, beat_lag)) = selected_tempo else {
        return Some(TrackAnalysis {
            duration: duration(samples.len(), sample_rate),
            audible_start: duration(first, sample_rate),
            audible_end: duration(last.saturating_add(1), sample_rate),
            intro_end: structure.intro_end,
            intro_confidence: structure.intro_confidence,
            outro_start: structure.outro_start,
            outro_confidence: structure.outro_confidence,
            vocal_activity: Vec::new(),
            vocal_activity_confidences: Vec::new(),
            vocal_activity_rate: 0,
            energy_profile: energy_profile.clone(),
            energy_profile_rate,
            bpm: None,
            beat_confidence: 0.0,
            first_beat: None,
            beat_markers: Vec::new(),
            beat_marker_confidences: Vec::new(),
            first_downbeat: None,
            downbeat_confidence: 0.0,
            musical_key: None,
            rms_dbfs: None,
            sample_peak_dbfs: None,
        });
    };
    let audible_start_block = first / block;
    let audible_end_block = last / block;
    let phase_onset = if kick_reliable { &kick_onset } else { &onset };
    let (beat_markers, beat_marker_confidences) = estimate_beat_markers(
        phase_onset,
        &kick_onset,
        kick_reliable,
        confidence,
        beat_lag,
        audible_start_block,
        audible_end_block,
    );
    let first_beat_block = beat_markers
        .first()
        .map(|index| index + 1)
        .unwrap_or_default();
    if first_beat_block == 0 {
        return Some(TrackAnalysis {
            duration: duration(samples.len(), sample_rate),
            audible_start: duration(first, sample_rate),
            audible_end: duration(last.saturating_add(1), sample_rate),
            intro_end: structure.intro_end,
            intro_confidence: structure.intro_confidence,
            outro_start: structure.outro_start,
            outro_confidence: structure.outro_confidence,
            vocal_activity: Vec::new(),
            vocal_activity_confidences: Vec::new(),
            vocal_activity_rate: 0,
            energy_profile: energy_profile.clone(),
            energy_profile_rate,
            bpm: None,
            beat_confidence: 0.0,
            first_beat: None,
            beat_markers: Vec::new(),
            beat_marker_confidences: Vec::new(),
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
        intro_end: structure.intro_end,
        intro_confidence: structure.intro_confidence,
        outro_start: structure.outro_start,
        outro_confidence: structure.outro_confidence,
        vocal_activity: Vec::new(),
        vocal_activity_confidences: Vec::new(),
        vocal_activity_rate: 0,
        energy_profile,
        energy_profile_rate,
        bpm: Some(bpm),
        beat_confidence: confidence,
        first_beat: Some(duration(first_beat_block * block, sample_rate)),
        beat_markers: beat_markers
            .iter()
            .copied()
            .map(|index| duration((index + 1) * block, sample_rate))
            .collect(),
        beat_marker_confidences,
        first_downbeat: Some(duration(first_downbeat_block * block, sample_rate)),
        downbeat_confidence,
        musical_key: None,
        rms_dbfs: None,
        sample_peak_dbfs: None,
    })
}

fn block_energy(samples: &[f32], block: usize) -> Vec<f32> {
    samples
        .chunks(block)
        .map(|chunk| chunk.iter().map(|sample| sample * sample).sum::<f32>() / chunk.len() as f32)
        .collect()
}

/// Replaces structure cues using a source-rate RMS envelope.
pub fn apply_energy_structure(analysis: &mut TrackAnalysis, rms: &[f32], bins_per_second: u32) {
    let structure = estimate_energy_structure(
        rms,
        bins_per_second,
        analysis.audible_start,
        analysis.audible_end,
    );
    analysis.intro_end = structure.intro_end;
    analysis.intro_confidence = structure.intro_confidence;
    analysis.outro_start = structure.outro_start;
    analysis.outro_confidence = structure.outro_confidence;
    analysis.energy_profile = quantize_energy_profile(rms);
    analysis.energy_profile_rate = bins_per_second.min(u32::from(u8::MAX)) as u8;
}

fn quantize_energy_profile(rms: &[f32]) -> Vec<u8> {
    rms.iter()
        .map(|value| {
            let dbfs = if *value > f32::EPSILON {
                20.0 * value.log10()
            } else {
                MIN_ENERGY_PROFILE_DBFS
            };
            (((dbfs.clamp(MIN_ENERGY_PROFILE_DBFS, 0.0) - MIN_ENERGY_PROFILE_DBFS)
                / -MIN_ENERGY_PROFILE_DBFS)
                * 255.0)
                .round() as u8
        })
        .collect()
}

fn rms_envelope(samples: &[f32], sample_rate: u32, bins_per_second: u32) -> Vec<f32> {
    let block = (sample_rate as usize / bins_per_second.max(1) as usize).max(1);
    samples
        .chunks(block)
        .map(|chunk| {
            (chunk.iter().map(|sample| sample * sample).sum::<f32>() / chunk.len() as f32).sqrt()
        })
        .collect()
}

fn estimate_energy_structure(
    rms: &[f32],
    bins_per_second: u32,
    audible_start: Duration,
    audible_end: Duration,
) -> EnergyStructure {
    if rms.is_empty() || bins_per_second == 0 || audible_end <= audible_start {
        return EnergyStructure::default();
    }
    let rate = bins_per_second as usize;
    let start = (audible_start.as_secs_f64() * bins_per_second as f64).floor() as usize;
    let end = (audible_end.as_secs_f64() * bins_per_second as f64).ceil() as usize;
    let values = &rms[start.min(rms.len())..end.min(rms.len())];
    let minimum_edge = rate;
    let core_run = rate * 4;
    if values.len() < minimum_edge * 2 + core_run {
        return EnergyStructure::default();
    }
    let smoothed = median_smooth(values, rate.max(1));
    let reference = percentile(&smoothed, 0.9);
    if reference <= f32::EPSILON {
        return EnergyStructure::default();
    }
    let weak_threshold = reference * 0.35;
    let core_threshold = reference * 0.55;
    let search = ((smoothed.len() * 35) / 100)
        .max(minimum_edge + core_run)
        .min(rate * 30)
        .min(smoothed.len());

    let intro = (minimum_edge..search.saturating_sub(core_run)).find_map(|boundary| {
        let before = &smoothed[..boundary];
        let after = &smoothed[boundary..boundary + core_run];
        let weak_ratio = ratio_matching(before, |value| value <= weak_threshold);
        let immediate_weak =
            ratio_matching(&smoothed[boundary - minimum_edge..boundary], |value| {
                value <= weak_threshold
            });
        let core_ratio = ratio_matching(after, |value| value >= core_threshold);
        if weak_ratio < 0.75 || immediate_weak < 0.75 || core_ratio < 0.75 {
            return None;
        }
        let contrast = ((mean(after) - mean(before)) / reference).clamp(0.0, 1.0);
        let confidence = (0.45 * contrast + 0.3 * weak_ratio + 0.25 * core_ratio).clamp(0.0, 1.0);
        (confidence >= 0.65).then_some((boundary, confidence))
    });

    let outro_search_start = smoothed.len().saturating_sub(search).max(core_run);
    let outro =
        (outro_search_start..smoothed.len().saturating_sub(minimum_edge)).find_map(|boundary| {
            let before = &smoothed[boundary - core_run..boundary];
            let after = &smoothed[boundary..];
            let core_ratio = ratio_matching(before, |value| value >= core_threshold);
            let weak_ratio = ratio_matching(after, |value| value <= weak_threshold);
            let immediate_weak =
                ratio_matching(&after[..minimum_edge], |value| value <= weak_threshold);
            let longest_recovery = longest_run(after, |value| value >= core_threshold);
            if core_ratio < 0.75
                || weak_ratio < 0.75
                || immediate_weak < 0.75
                || longest_recovery >= rate * 2
            {
                return None;
            }
            let contrast = ((mean(before) - mean(after)) / reference).clamp(0.0, 1.0);
            let confidence =
                (0.45 * contrast + 0.3 * weak_ratio + 0.25 * core_ratio).clamp(0.0, 1.0);
            (confidence >= 0.65).then_some((boundary, confidence))
        });

    // `values` starts at the floored profile bin, which can precede a
    // fractional audible boundary. Convert from that absolute bin instead of
    // adding the unquantized boundary a second time.
    let to_duration =
        |index: usize| Duration::from_secs_f64((start + index) as f64 / bins_per_second as f64);
    let mut structure = EnergyStructure {
        intro_end: intro.map(|(index, _)| to_duration(index)),
        intro_confidence: intro.map_or(0.0, |(_, confidence)| confidence),
        outro_start: outro.map(|(index, _)| to_duration(index)),
        outro_confidence: outro.map_or(0.0, |(_, confidence)| confidence),
    };
    if structure
        .intro_end
        .zip(structure.outro_start)
        .is_some_and(|(intro, outro)| intro >= outro)
    {
        if structure.intro_confidence <= structure.outro_confidence {
            structure.intro_end = None;
            structure.intro_confidence = 0.0;
        } else {
            structure.outro_start = None;
            structure.outro_confidence = 0.0;
        }
    }
    structure
}

fn median_smooth(values: &[f32], width: usize) -> Vec<f32> {
    let radius = width / 2;
    (0..values.len())
        .map(|index| {
            let from = index.saturating_sub(radius);
            let to = (index + radius + 1).min(values.len());
            percentile(&values[from..to], 0.5)
        })
        .collect()
}

fn percentile(values: &[f32], percentile: f32) -> f32 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f32::total_cmp);
    let index =
        ((sorted.len().saturating_sub(1)) as f32 * percentile.clamp(0.0, 1.0)).round() as usize;
    sorted.get(index).copied().unwrap_or_default()
}

fn ratio_matching(values: &[f32], predicate: impl Fn(f32) -> bool) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().filter(|value| predicate(**value)).count() as f32 / values.len() as f32
}

fn longest_run(values: &[f32], predicate: impl Fn(f32) -> bool) -> usize {
    values
        .iter()
        .fold((0, 0), |(longest, current), value| {
            let current = if predicate(*value) { current + 1 } else { 0 };
            (longest.max(current), current)
        })
        .0
}

fn mean(values: &[f32]) -> f32 {
    values.iter().sum::<f32>() / values.len().max(1) as f32
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
    phase_onset: &[f32],
    kick_onset: &[f32],
    kick_reliable: bool,
    track_confidence: f32,
    beat_lag: f32,
    audible_start: usize,
    audible_end: usize,
) -> (Vec<usize>, Vec<f32>) {
    if phase_onset.is_empty() || beat_lag <= 1.0 || !beat_lag.is_finite() {
        return (Vec::new(), Vec::new());
    }
    const PHASE_STEPS_PER_BLOCK: usize = 4;
    let phase_steps = (beat_lag * PHASE_STEPS_PER_BLOCK as f32).ceil() as usize;
    let phase = (0..phase_steps)
        .map(|step| {
            let phase = step as f32 / PHASE_STEPS_PER_BLOCK as f32;
            let mut score = 0.0;
            let mut count = 0_u32;
            let mut position = phase;
            while position < phase_onset.len() as f32 {
                score += interpolated(phase_onset, position);
                count += 1;
                position += beat_lag;
            }
            (score / count.max(1) as f32, phase)
        })
        .max_by(|left, right| left.0.total_cmp(&right.0))
        .map(|(_, phase)| phase);
    let Some(phase) = phase else {
        return (Vec::new(), Vec::new());
    };

    let tracking_onset = if kick_reliable {
        kick_onset
    } else {
        phase_onset
    };
    let reference = onset_reference(tracking_onset);
    let search_radius = (beat_lag * 0.2).round().clamp(5.0, 20.0) as usize;
    let mut candidates = Vec::new();
    let mut position = phase;
    while position + (search_radius as f32) < audible_start as f32 {
        position += beat_lag;
    }
    while position < tracking_onset.len() as f32
        && position <= audible_end as f32 + search_radius as f32
    {
        let center = position.round() as usize;
        let from = center.saturating_sub(search_radius);
        let to = center
            .saturating_add(search_radius + 1)
            .min(tracking_onset.len());
        if from < to {
            let (offset, strength) = match tracking_onset[from..to]
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
            let proximity = 1.0 - detected.abs_diff(center) as f32 / search_radius.max(1) as f32;
            let marker_confidence = if kick_reliable {
                (strength / reference.max(f32::EPSILON)).clamp(0.0, 1.0)
                    * proximity.clamp(0.0, 1.0)
                    * (track_confidence / MIN_KICK_TRACK_CONFIDENCE).clamp(0.0, 1.0)
            } else {
                0.0
            };
            candidates.push((center, detected, strength, marker_confidence));
            let should_snap = if kick_reliable {
                marker_confidence >= MIN_KICK_MARKER_CONFIDENCE
            } else {
                strength >= reference * 0.1
            };
            if should_snap {
                position = detected as f32 + beat_lag;
                continue;
            }
        }
        position += beat_lag;
    }
    let Some(first) = candidates
        .iter()
        .position(|(_, position, strength, confidence)| {
            *position + 1 >= audible_start
                && if kick_reliable {
                    *confidence >= MIN_KICK_FIRST_CONFIDENCE
                } else {
                    *strength >= reference * 0.25
                }
        })
    else {
        return (Vec::new(), Vec::new());
    };
    candidates[first..]
        .iter()
        .enumerate()
        .map(
            |(candidate_index, (predicted, detected, strength, confidence))| {
                let should_snap = if kick_reliable {
                    *confidence
                        >= if candidate_index == 0 {
                            MIN_KICK_FIRST_CONFIDENCE
                        } else {
                            MIN_KICK_MARKER_CONFIDENCE
                        }
                } else {
                    *strength >= reference * 0.1
                };
                (
                    if should_snap { *detected } else { *predicted },
                    *confidence,
                )
            },
        )
        .filter(|(position, _)| *position <= audible_end)
        .fold(
            (Vec::new(), Vec::new()),
            |(mut markers, mut confidences), (position, confidence)| {
                if markers.last().is_none_or(|previous| *previous < position) {
                    markers.push(position);
                    confidences.push(confidence);
                }
                (markers, confidences)
            },
        )
}

fn onset_reference(onset: &[f32]) -> f32 {
    let mut positive = onset
        .iter()
        .copied()
        .filter(|value| *value > f32::EPSILON)
        .collect::<Vec<_>>();
    if positive.is_empty() {
        return f32::EPSILON;
    }
    positive.sort_by(f32::total_cmp);
    let index = ((positive.len() - 1) as f32 * 0.9).round() as usize;
    positive[index]
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

    #[test]
    fn low_band_kicks_define_phase_despite_louder_offbeat_hats() {
        let sample_rate = 16_000;
        let mut samples = vec![0.0; sample_rate as usize * 20];
        for beat in 0..36 {
            add_burst(
                &mut samples,
                sample_rate,
                1.0 + beat as f32 * 0.5,
                80.0,
                0.07,
                0.3,
            );
            add_burst(
                &mut samples,
                sample_rate,
                1.25 + beat as f32 * 0.5,
                3_200.0,
                0.015,
                0.8,
            );
        }

        let analysis = analyze_mono_pcm(&samples, sample_rate).unwrap();
        assert!((analysis.bpm.unwrap() - 120.0).abs() < 0.5, "{analysis:?}");
        assert!(
            analysis
                .first_beat
                .unwrap()
                .abs_diff(Duration::from_secs(1))
                < Duration::from_millis(30),
            "{analysis:?}"
        );
        assert_eq!(
            analysis.beat_marker_confidences.len(),
            analysis.beat_markers.len()
        );
        assert!(
            analysis
                .beat_marker_confidences
                .iter()
                .skip(2)
                .take(16)
                .filter(|confidence| **confidence >= MIN_PHASE_MARKER_CONFIDENCE_FOR_TEST)
                .count()
                >= 12,
            "{analysis:?}"
        );
    }

    #[test]
    fn low_band_tempo_wins_over_conflicting_high_band_rhythm() {
        let sample_rate = 16_000;
        let mut samples = vec![0.0; sample_rate as usize * 24];
        for beat in 0..44 {
            add_burst(
                &mut samples,
                sample_rate,
                1.0 + beat as f32 * 0.5,
                80.0,
                0.07,
                0.28,
            );
        }
        for beat in 0..55 {
            add_burst(
                &mut samples,
                sample_rate,
                1.1 + beat as f32 * 0.4,
                3_200.0,
                0.015,
                0.85,
            );
        }

        let analysis = analyze_mono_pcm(&samples, sample_rate).unwrap();
        assert!((analysis.bpm.unwrap() - 120.0).abs() < 0.7, "{analysis:?}");
        assert!(
            analysis
                .first_beat
                .unwrap()
                .abs_diff(Duration::from_secs(1))
                < Duration::from_millis(30),
            "{analysis:?}"
        );
    }

    #[test]
    fn missing_kick_keeps_grid_marker_with_lower_confidence() {
        let sample_rate = 16_000;
        let mut samples = vec![0.0; sample_rate as usize * 20];
        for beat in 0..36 {
            if beat != 12 {
                add_burst(
                    &mut samples,
                    sample_rate,
                    1.0 + beat as f32 * 0.5,
                    80.0,
                    0.07,
                    0.3,
                );
            }
        }

        let analysis = analyze_mono_pcm(&samples, sample_rate).unwrap();
        let missing = Duration::from_secs(7);
        let index = analysis
            .beat_markers
            .iter()
            .position(|marker| marker.abs_diff(missing) < Duration::from_millis(30))
            .expect("missing kick should retain a predicted grid marker");
        let confidence = analysis.beat_marker_confidences[index];
        let neighbors = (analysis.beat_marker_confidences[index - 1]
            + analysis.beat_marker_confidences[index + 1])
            * 0.5;
        assert!(confidence < neighbors * 0.4, "{analysis:?}");
    }

    #[test]
    fn high_band_only_falls_back_to_global_beat_grid() {
        let sample_rate = 16_000;
        let mut samples = vec![0.0; sample_rate as usize * 14];
        for beat in 0..24 {
            add_burst(
                &mut samples,
                sample_rate,
                1.0 + beat as f32 * 0.5,
                3_200.0,
                0.015,
                0.8,
            );
        }

        let analysis = analyze_mono_pcm(&samples, sample_rate).unwrap();
        assert!((analysis.bpm.unwrap() - 120.0).abs() < 1.0, "{analysis:?}");
        assert!(
            analysis
                .beat_marker_confidences
                .iter()
                .all(|value| *value < 0.2)
        );
    }

    #[test]
    fn detects_quiet_intro_body_and_quiet_outro() {
        let sample_rate = 1_000;
        let mut samples = vec![0.0; sample_rate as usize * 54];
        fill_carrier(&mut samples, sample_rate, 1.0, 9.0, 0.08);
        fill_carrier(&mut samples, sample_rate, 9.0, 41.0, 0.6);
        fill_carrier(&mut samples, sample_rate, 41.0, 53.0, 0.08);

        let analysis = analyze_mono_pcm(&samples, sample_rate).unwrap();

        assert!(
            analysis.intro_end.unwrap().abs_diff(Duration::from_secs(9)) <= Duration::from_secs(1),
            "{analysis:?}"
        );
        assert!(analysis.intro_confidence >= 0.65, "{analysis:?}");
        assert!(
            analysis
                .outro_start
                .unwrap()
                .abs_diff(Duration::from_secs(41))
                <= Duration::from_secs(1),
            "{analysis:?}"
        );
        assert!(analysis.outro_confidence >= 0.65, "{analysis:?}");
    }

    #[test]
    fn flat_track_does_not_invent_structure() {
        let sample_rate = 1_000;
        let mut samples = vec![0.0; sample_rate as usize * 50];
        fill_carrier(&mut samples, sample_rate, 1.0, 49.0, 0.4);

        let analysis = analyze_mono_pcm(&samples, sample_rate).unwrap();

        assert_eq!(analysis.intro_end, None, "{analysis:?}");
        assert_eq!(analysis.outro_start, None, "{analysis:?}");
    }

    #[test]
    fn trailing_silence_is_not_mistaken_for_an_outro() {
        let sample_rate = 1_000;
        let mut samples = vec![0.0; sample_rate as usize * 57];
        fill_carrier(&mut samples, sample_rate, 1.0, 49.0, 0.5);

        let analysis = analyze_mono_pcm(&samples, sample_rate).unwrap();

        assert!(analysis.audible_end.abs_diff(Duration::from_secs(49)) < Duration::from_millis(5));
        assert_eq!(analysis.outro_start, None, "{analysis:?}");
    }

    const MIN_PHASE_MARKER_CONFIDENCE_FOR_TEST: f32 = 0.35;

    fn add_burst(
        samples: &mut [f32],
        sample_rate: u32,
        start_seconds: f32,
        frequency: f32,
        duration_seconds: f32,
        gain: f32,
    ) {
        let start = (start_seconds * sample_rate as f32).round() as usize;
        let length = (duration_seconds * sample_rate as f32).round() as usize;
        for offset in 0..length.min(samples.len().saturating_sub(start)) {
            let time = offset as f32 / sample_rate as f32;
            let envelope = 1.0 - offset as f32 / length.max(1) as f32;
            samples[start + offset] +=
                gain * (std::f32::consts::TAU * frequency * time).sin() * envelope;
        }
    }

    fn fill_carrier(
        samples: &mut [f32],
        sample_rate: u32,
        start_seconds: f32,
        end_seconds: f32,
        gain: f32,
    ) {
        let start = (start_seconds * sample_rate as f32) as usize;
        let end = ((end_seconds * sample_rate as f32) as usize).min(samples.len());
        for (index, sample) in samples[start..end].iter_mut().enumerate() {
            *sample = if (index / 5).is_multiple_of(2) {
                gain
            } else {
                -gain
            };
        }
    }
}
