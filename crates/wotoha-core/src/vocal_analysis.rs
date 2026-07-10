use std::time::Duration;

use crate::automix::TrackAnalysis;

pub const VOCAL_ACTIVITY_RATE: u8 = 4;
const VOICING_RATE: u32 = 2_000;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VocalActivityProfile {
    pub activity: Vec<u8>,
    pub confidences: Vec<u8>,
    pub rate: u8,
}

#[derive(Clone, Copy, Debug)]
struct RawBin {
    full_rms: f32,
    vocal_ratio: f32,
    voicing: f32,
    modulation: f32,
    activity_duty: f32,
    zero_crossing_rate: f32,
    crest: f32,
}

/// Streaming, dependency-free voice-risk analyzer.
///
/// This deliberately estimates foreground vocal risk rather than claiming
/// source separation. Ambiguous mid-band material receives lower confidence so
/// the transition planner can choose the conservative path.
pub struct VocalActivityAnalyzer {
    sample_rate: u32,
    filter: VocalBandFilter,
    bin_phase: u64,
    full_energy: f64,
    vocal_energy: f64,
    peak: f32,
    frames: u64,
    downsample_phase: u64,
    downsample_sum: f32,
    downsample_count: u32,
    voiced_samples: Vec<f32>,
    raw: Vec<RawBin>,
}

impl VocalActivityAnalyzer {
    pub fn new(sample_rate: u32) -> Option<Self> {
        Some(Self {
            sample_rate,
            filter: VocalBandFilter::new(sample_rate)?,
            bin_phase: 0,
            full_energy: 0.0,
            vocal_energy: 0.0,
            peak: 0.0,
            frames: 0,
            downsample_phase: 0,
            downsample_sum: 0.0,
            downsample_count: 0,
            voiced_samples: Vec::with_capacity(
                (VOICING_RATE / u32::from(VOCAL_ACTIVITY_RATE)) as usize + 1,
            ),
            raw: Vec::new(),
        })
    }

    pub fn push(&mut self, sample: f32) {
        let vocal = self.filter.process(sample);
        self.full_energy += f64::from(sample) * f64::from(sample);
        self.vocal_energy += f64::from(vocal) * f64::from(vocal);
        self.peak = self.peak.max(vocal.abs());
        self.frames += 1;

        self.downsample_sum += vocal;
        self.downsample_count += 1;
        self.downsample_phase += VOICING_RATE as u64;
        if self.downsample_phase >= self.sample_rate as u64 {
            self.downsample_phase -= self.sample_rate as u64;
            self.voiced_samples
                .push(self.downsample_sum / self.downsample_count.max(1) as f32);
            self.downsample_sum = 0.0;
            self.downsample_count = 0;
        }

        self.bin_phase += u64::from(VOCAL_ACTIVITY_RATE);
        if self.bin_phase >= self.sample_rate as u64 {
            self.bin_phase -= self.sample_rate as u64;
            self.finish_bin();
        }
    }

    pub fn finish(mut self) -> VocalActivityProfile {
        if self.frames >= u64::from(self.sample_rate / u32::from(VOCAL_ACTIVITY_RATE) / 2) {
            self.finish_bin();
        }
        if self.raw.is_empty() {
            return VocalActivityProfile::default();
        }
        let energy_reference = percentile(
            &self.raw.iter().map(|bin| bin.full_rms).collect::<Vec<_>>(),
            0.9,
        );
        let reliable_rate = self.sample_rate >= 8_000;
        let mut activity = Vec::with_capacity(self.raw.len());
        let mut confidences = Vec::with_capacity(self.raw.len());
        for bin in self.raw {
            let active = bin.full_rms >= energy_reference * 0.03;
            let ratio = normalize(bin.vocal_ratio, 0.12, 0.72);
            let voicing = normalize(bin.voicing, 0.12, 0.65);
            let modulation = normalize(bin.modulation, 0.08, 0.65);
            let zcr = triangular(bin.zero_crossing_rate, 0.03, 0.22, 0.48);
            let transient_penalty = normalize(bin.crest, 6.0, 16.0) * (1.0 - voicing);
            let mut risk = 0.34 * ratio + 0.31 * voicing + 0.25 * modulation + 0.1 * zcr
                - 0.35 * transient_penalty;
            if !active {
                risk = 0.0;
            }
            if bin.activity_duty < 0.25 {
                risk = risk.min(0.35);
            } else if bin.modulation < 0.08 {
                risk = risk.min(0.5);
            }
            risk = risk.clamp(0.0, 1.0);

            let clear_vocal = active && ratio >= 0.35 && voicing >= 0.3 && modulation >= 0.2;
            let clear_non_vocal = !active
                || ratio < 0.12
                || bin.activity_duty < 0.25
                || (voicing < 0.12 && transient_penalty > 0.25)
                || (modulation < 0.08 && risk < 0.55);
            let confidence: f32 = if !reliable_rate {
                0.25
            } else if clear_vocal {
                0.9
            } else if clear_non_vocal {
                0.8
            } else {
                0.45
            };
            activity.push(quantize(risk));
            confidences.push(quantize(confidence));
        }
        smooth_profile(&mut activity);
        VocalActivityProfile {
            activity,
            confidences,
            rate: VOCAL_ACTIVITY_RATE,
        }
    }

    fn finish_bin(&mut self) {
        if self.frames == 0 {
            return;
        }
        if self.downsample_count > 0 {
            self.voiced_samples
                .push(self.downsample_sum / self.downsample_count as f32);
        }
        let full_rms = (self.full_energy / self.frames as f64).sqrt() as f32;
        let vocal_rms = (self.vocal_energy / self.frames as f64).sqrt() as f32;
        let vocal_ratio = vocal_rms / full_rms.max(1.0e-7);
        let vocal_mean_square = self.vocal_energy as f32 / self.frames as f32;
        let crest = self.peak * self.peak / vocal_mean_square.max(1.0e-9);
        self.raw.push(RawBin {
            full_rms,
            vocal_ratio,
            voicing: voicing_strength(&self.voiced_samples, VOICING_RATE),
            modulation: amplitude_modulation(&self.voiced_samples, VOICING_RATE),
            activity_duty: activity_duty(&self.voiced_samples, VOICING_RATE),
            zero_crossing_rate: zero_crossing_rate(&self.voiced_samples),
            crest,
        });
        self.full_energy = 0.0;
        self.vocal_energy = 0.0;
        self.peak = 0.0;
        self.frames = 0;
        self.downsample_sum = 0.0;
        self.downsample_count = 0;
        self.voiced_samples.clear();
    }
}

pub fn analyze_vocal_activity(samples: &[f32], sample_rate: u32) -> VocalActivityProfile {
    let Some(mut analyzer) = VocalActivityAnalyzer::new(sample_rate) else {
        return VocalActivityProfile::default();
    };
    for sample in samples {
        analyzer.push(*sample);
    }
    analyzer.finish()
}

pub fn apply_vocal_activity(analysis: &mut TrackAnalysis, profile: VocalActivityProfile) {
    analysis.vocal_activity = profile.activity;
    analysis.vocal_activity_confidences = profile.confidences;
    analysis.vocal_activity_rate = profile.rate;
}

pub fn effective_vocal_risk(analysis: &TrackAnalysis, position: Duration) -> f32 {
    if analysis.vocal_activity_rate == 0
        || analysis.vocal_activity.len() != analysis.vocal_activity_confidences.len()
    {
        return 0.65;
    }
    let index = (position.as_secs_f64() * f64::from(analysis.vocal_activity_rate)).floor() as usize;
    let Some((&risk, &confidence)) = analysis
        .vocal_activity
        .get(index)
        .zip(analysis.vocal_activity_confidences.get(index))
    else {
        return 0.65;
    };
    let risk = f32::from(risk) / 255.0;
    let confidence = f32::from(confidence) / 255.0;
    confidence * risk + (1.0 - confidence) * 0.65
}

struct VocalBandFilter {
    high_alpha: f32,
    low_alpha: f32,
    previous_input_1: f32,
    previous_input_2: f32,
    high_1: f32,
    high_2: f32,
    low_1: f32,
    low_2: f32,
}

impl VocalBandFilter {
    fn new(sample_rate: u32) -> Option<Self> {
        if sample_rate == 0 {
            return None;
        }
        let rate = sample_rate as f32;
        let low_cutoff = 4_000.0_f32.min(rate * 0.45);
        Some(Self {
            high_alpha: (-std::f32::consts::TAU * 180.0 / rate).exp(),
            low_alpha: (-std::f32::consts::TAU * low_cutoff / rate).exp(),
            previous_input_1: 0.0,
            previous_input_2: 0.0,
            high_1: 0.0,
            high_2: 0.0,
            low_1: 0.0,
            low_2: 0.0,
        })
    }

    fn process(&mut self, sample: f32) -> f32 {
        self.high_1 = self.high_alpha * (self.high_1 + sample - self.previous_input_1);
        self.previous_input_1 = sample;
        self.high_2 = self.high_alpha * (self.high_2 + self.high_1 - self.previous_input_2);
        self.previous_input_2 = self.high_1;
        self.low_1 = (1.0 - self.low_alpha) * self.high_2 + self.low_alpha * self.low_1;
        self.low_2 = (1.0 - self.low_alpha) * self.low_1 + self.low_alpha * self.low_2;
        self.low_2
    }
}

fn voicing_strength(samples: &[f32], sample_rate: u32) -> f32 {
    if samples.len() < 32 {
        return 0.0;
    }
    let energy = samples.iter().map(|value| value * value).sum::<f32>();
    if energy <= f32::EPSILON {
        return 0.0;
    }
    let minimum_lag = (sample_rate / 350).max(1) as usize;
    let maximum_lag = (sample_rate / 80).max(minimum_lag as u32) as usize;
    (minimum_lag..=maximum_lag.min(samples.len() - 1))
        .map(|lag| {
            let mut dot = 0.0;
            let mut left_energy = 0.0;
            let mut right_energy = 0.0;
            for (left, right) in samples.iter().zip(samples.iter().skip(lag)) {
                dot += left * right;
                left_energy += left * left;
                right_energy += right * right;
            }
            dot / (left_energy * right_energy).sqrt().max(1.0e-9)
        })
        .fold(0.0_f32, f32::max)
        .clamp(0.0, 1.0)
}

fn amplitude_modulation(samples: &[f32], sample_rate: u32) -> f32 {
    let window = (sample_rate / 50).max(1) as usize;
    let levels = samples
        .chunks(window)
        .map(|chunk| {
            (chunk.iter().map(|sample| sample * sample).sum::<f32>() / chunk.len() as f32).sqrt()
        })
        .collect::<Vec<_>>();
    if levels.len() < 2 {
        return 0.0;
    }
    let minimum = levels.iter().copied().fold(f32::INFINITY, f32::min);
    let maximum = levels.iter().copied().fold(0.0_f32, f32::max);
    ((maximum - minimum) / maximum.max(1.0e-7)).clamp(0.0, 1.0)
}

fn activity_duty(samples: &[f32], sample_rate: u32) -> f32 {
    let window = (sample_rate / 50).max(1) as usize;
    let levels = samples
        .chunks(window)
        .map(|chunk| {
            (chunk.iter().map(|sample| sample * sample).sum::<f32>() / chunk.len() as f32).sqrt()
        })
        .collect::<Vec<_>>();
    let peak = levels.iter().copied().fold(0.0_f32, f32::max);
    if peak <= f32::EPSILON {
        return 0.0;
    }
    levels.iter().filter(|level| **level >= peak * 0.25).count() as f32 / levels.len() as f32
}

fn zero_crossing_rate(samples: &[f32]) -> f32 {
    if samples.len() < 2 {
        return 0.0;
    }
    samples
        .windows(2)
        .filter(|pair| pair[0].is_sign_positive() != pair[1].is_sign_positive())
        .count() as f32
        / (samples.len() - 1) as f32
}

fn smooth_profile(activity: &mut [u8]) {
    let original = activity.to_vec();
    for (index, value) in activity.iter_mut().enumerate() {
        let from = index.saturating_sub(1);
        let to = (index + 2).min(original.len());
        let mut window = original[from..to].to_vec();
        window.sort_unstable();
        *value = window[window.len() / 2];
    }
}

fn normalize(value: f32, low: f32, high: f32) -> f32 {
    ((value - low) / (high - low)).clamp(0.0, 1.0)
}

fn triangular(value: f32, low: f32, peak: f32, high: f32) -> f32 {
    if value <= low || value >= high {
        0.0
    } else if value <= peak {
        (value - low) / (peak - low)
    } else {
        (high - value) / (high - peak)
    }
}

fn quantize(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn percentile(values: &[f32], percentile: f32) -> f32 {
    let mut values = values.to_vec();
    values.sort_by(f32::total_cmp);
    let index =
        ((values.len().saturating_sub(1)) as f32 * percentile.clamp(0.0, 1.0)).round() as usize;
    values.get(index).copied().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_syllabic_formant_voice_over_low_frequency_bed() {
        let rate = 16_000;
        let mut samples = vec![0.0; rate as usize * 12];
        for (index, sample) in samples.iter_mut().enumerate() {
            let time = index as f32 / rate as f32;
            *sample = 0.15 * (std::f32::consts::TAU * 80.0 * time).sin();
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
                        *sample +=
                            gain * (std::f32::consts::TAU * frequency * time).sin() * envelope;
                    }
                }
            }
        }

        let profile = analyze_vocal_activity(&samples, rate);
        let active = profile.activity[8..40]
            .iter()
            .filter(|risk| **risk >= 140)
            .count();
        assert!(active >= 20, "{profile:?}");
    }

    #[test]
    fn stationary_mid_band_tone_is_not_classified_as_confident_voice() {
        let rate = 16_000;
        let samples = (0..rate as usize * 8)
            .map(|index| {
                let time = index as f32 / rate as f32;
                0.4 * (std::f32::consts::TAU * 700.0 * time).sin()
            })
            .collect::<Vec<_>>();

        let profile = analyze_vocal_activity(&samples, rate);
        assert!(
            profile.activity.iter().all(|risk| *risk < 140),
            "{profile:?}"
        );
    }

    #[test]
    fn short_high_band_impulses_are_rejected_as_transients() {
        let rate = 16_000;
        let mut samples = vec![0.0; rate as usize * 8];
        for start in (rate as usize..rate as usize * 7).step_by(rate as usize / 2) {
            for offset in 0..rate as usize / 100 {
                let time = offset as f32 / rate as f32;
                samples[start + offset] = 0.8 * (std::f32::consts::TAU * 3_200.0 * time).sin();
            }
        }

        let profile = analyze_vocal_activity(&samples, rate);
        assert!(
            profile.activity.iter().all(|risk| *risk < 140),
            "{profile:?}"
        );
    }
}
