//! Small deterministic streaming resampler for the neural analysis clock.
//!
//! Beat This! consumes 22.05 kHz audio. The decoder's source rates are often
//! 44.1 or 48 kHz, so point sampling at the output clock would fold cymbals
//! into the beat band. This resampler uses a 49-tap Hann-windowed sinc low-pass
//! around each output position. It keeps only the finite kernel neighborhood,
//! making the 12-minute neural memory bound apply to output samples rather
//! than retaining the source track. The cutoff is 0.96 times the destination
//! Nyquist frequency; the finite Hann window gives deterministic stop-band
//! attenuation while preserving the output clock without a whole-track buffer.

use std::collections::VecDeque;

const KERNEL_RADIUS: u64 = 24;
const KERNEL_TAPS: usize = KERNEL_RADIUS as usize * 2 + 1;

#[derive(Debug)]
pub(crate) struct StreamingNeuralResampler {
    source_rate: u32,
    target_rate: u32,
    identity: bool,
    source_base: u64,
    source: VecDeque<f32>,
    phase_weights: Vec<Option<Box<[f64; KERNEL_TAPS]>>>,
    next_output: u64,
    total_input: u64,
}

impl StreamingNeuralResampler {
    pub(crate) fn new(source_rate: u32, target_rate: u32) -> Option<Self> {
        (source_rate >= target_rate && source_rate > 0 && target_rate > 0).then_some(Self {
            source_rate,
            target_rate,
            identity: source_rate == target_rate,
            source_base: 0,
            source: VecDeque::with_capacity((KERNEL_RADIUS * 2 + 4) as usize),
            phase_weights: if source_rate != target_rate {
                vec![None; target_rate as usize]
            } else {
                Vec::new()
            },
            next_output: 0,
            total_input: 0,
        })
    }

    /// Push one mono source sample. Returns false only once the caller has
    /// already reached its bounded output capacity and more source follows.
    pub(crate) fn push_sample(
        &mut self,
        sample: f32,
        output: &mut Vec<f32>,
        max_output: usize,
    ) -> bool {
        if output.len() >= max_output {
            return false;
        }
        self.total_input = self.total_input.saturating_add(1);
        let sample = if sample.is_finite() { sample } else { 0.0 };
        if self.source_rate == self.target_rate {
            output.push(sample);
            return true;
        }
        self.source.push_back(sample);
        self.generate_available(output, max_output, false);
        true
    }

    /// Finish the stream and zero-pad only the finite right-hand filter tail.
    /// The returned bool is false when the exact output duration exceeds the
    /// caller's bound.
    pub(crate) fn finish(&mut self, output: &mut Vec<f32>, max_output: usize) -> bool {
        if self.identity {
            return output.len() == self.total_input as usize && output.len() <= max_output;
        }
        self.generate_available(output, max_output, true);
        let required = self
            .total_input
            .saturating_mul(u64::from(self.target_rate))
            .saturating_add(u64::from(self.source_rate).saturating_sub(1))
            / u64::from(self.source_rate);
        output.len() >= required as usize && output.len() <= max_output
    }

    fn generate_available(&mut self, output: &mut Vec<f32>, max_output: usize, final_input: bool) {
        while output.len() < max_output {
            let position_num = self.next_output.saturating_mul(u64::from(self.source_rate));
            let center = position_num / u64::from(self.target_rate);
            let remainder = position_num % u64::from(self.target_rate);
            let last_source = self.source_base + self.source.len() as u64;
            if !final_input && center.saturating_add(KERNEL_RADIUS) >= last_source {
                break;
            }
            if final_input
                && position_num >= self.total_input.saturating_mul(u64::from(self.target_rate))
            {
                break;
            }

            let phase = remainder as usize;
            if self.phase_weights[phase].is_none() {
                self.phase_weights[phase] = Some(Box::new(make_weights(
                    remainder as f64 / f64::from(self.target_rate),
                    self.source_rate,
                    self.target_rate,
                )));
            }
            let weights = self.phase_weights[phase]
                .as_ref()
                .expect("phase weights were initialized");
            let mut value = 0.0_f64;
            let mut weight_sum = 0.0_f64;
            let center_i = center as i64;
            for (tap, offset) in (-(KERNEL_RADIUS as i64)..=(KERNEL_RADIUS as i64)).enumerate() {
                let index = center_i + offset;
                let weight = weights[tap];
                if weight == 0.0 {
                    continue;
                }
                let sample = if index < 0 {
                    0.0
                } else {
                    self.sample_at(index as u64)
                };
                value += f64::from(sample) * weight;
                weight_sum += weight;
            }
            let value = if weight_sum.abs() > f64::EPSILON {
                value / weight_sum
            } else {
                0.0
            };
            output.push(value as f32);
            self.next_output = self.next_output.saturating_add(1);

            let next_position = self.next_output.saturating_mul(u64::from(self.source_rate))
                / u64::from(self.target_rate);
            let keep_from = next_position.saturating_sub(KERNEL_RADIUS + 1);
            while self.source_base < keep_from && !self.source.is_empty() {
                self.source.pop_front();
                self.source_base += 1;
            }
        }
    }

    fn sample_at(&self, index: u64) -> f32 {
        let Some(offset) = index.checked_sub(self.source_base) else {
            return 0.0;
        };
        self.source
            .get(offset as usize)
            .copied()
            .unwrap_or_default()
    }
}

fn make_weights(fraction: f64, source_rate: u32, target_rate: u32) -> [f64; KERNEL_TAPS] {
    let cutoff = 0.5_f64 * f64::from(target_rate) / f64::from(source_rate) * 0.96;
    let mut weights = [0.0_f64; KERNEL_TAPS];
    let mut sum = 0.0_f64;
    for (tap, offset) in (-(KERNEL_RADIUS as i64)..=(KERNEL_RADIUS as i64)).enumerate() {
        let distance = offset as f64 - fraction;
        let sinc_argument = 2.0 * cutoff * distance;
        let sinc = if sinc_argument.abs() < 1.0e-12 {
            1.0
        } else {
            let angle = std::f64::consts::PI * sinc_argument;
            angle.sin() / angle
        };
        let window =
            0.5 * (1.0 + (std::f64::consts::PI * distance.abs() / KERNEL_RADIUS as f64).cos());
        let weight = 2.0 * cutoff * sinc * window;
        weights[tap] = weight;
        sum += weight;
    }
    if sum.abs() > f64::EPSILON {
        for weight in &mut weights {
            *weight /= sum;
        }
    }
    weights
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resample(source_rate: u32, samples: &[f32], chunk: usize) -> Vec<f32> {
        let mut resampler = StreamingNeuralResampler::new(source_rate, 22_050).unwrap();
        let mut output = Vec::new();
        for part in samples.chunks(chunk) {
            for sample in part {
                assert!(resampler.push_sample(*sample, &mut output, usize::MAX));
            }
        }
        assert!(resampler.finish(&mut output, usize::MAX));
        output
    }

    #[test]
    fn chunk_boundaries_do_not_change_output() {
        let samples = (0..48_000)
            .map(|index| (index as f32 * 0.013).sin())
            .collect::<Vec<_>>();
        let one_by_one = resample(48_000, &samples, 1);
        let packetized = resample(48_000, &samples, 997);
        assert_eq!(one_by_one.len(), packetized.len());
        assert!(
            one_by_one
                .iter()
                .zip(packetized)
                .all(|(left, right)| (left - right).abs() < 1.0e-6)
        );
    }

    #[test]
    fn output_clock_has_no_minute_scale_drift() {
        for source_rate in [44_100, 48_000] {
            let input_len = source_rate as usize * 60;
            let samples = vec![0.0; input_len];
            let output = resample(source_rate, &samples, 4096);
            let expected = (input_len as u64 * 22_050 / source_rate as u64) as usize;
            assert_eq!(output.len(), expected, "source_rate={source_rate}");
        }
    }

    #[test]
    fn high_frequency_hat_does_not_alias_into_output() {
        let source_rate = 48_000_u32;
        let samples = (0..source_rate as usize)
            .map(|index| {
                (std::f32::consts::TAU * 16_000.0 * index as f32 / source_rate as f32).sin()
            })
            .collect::<Vec<_>>();
        let output = resample(source_rate, &samples, 701);
        let steady = &output[1_000..];
        let rms = (steady
            .iter()
            .map(|sample| f64::from(*sample) * f64::from(*sample))
            .sum::<f64>()
            / steady.len() as f64)
            .sqrt();
        assert!(rms < 0.03, "high-frequency residual rms={rms}");
    }

    #[test]
    fn click_clock_is_within_one_millisecond_at_common_rates() {
        for source_rate in [44_100_u32, 48_000] {
            let mut samples = vec![0.0_f32; source_rate as usize * 4];
            for second in 1..4 {
                let index = second * source_rate as usize;
                samples[index] = 1.0;
            }
            let output = resample(source_rate, &samples, 503);
            for second in 1..4 {
                let expected = second * 22_050_usize;
                let actual = output
                    .iter()
                    .enumerate()
                    .skip(expected.saturating_sub(30))
                    .take(61)
                    .max_by(|left, right| left.1.abs().total_cmp(&right.1.abs()))
                    .map(|(index, _)| index)
                    .unwrap();
                assert!(actual.abs_diff(expected) <= 22, "source_rate={source_rate}");
            }
        }
    }

    #[test]
    fn bounded_output_capacity_stops_before_growing_memory() {
        let mut resampler = StreamingNeuralResampler::new(22_050, 22_050).unwrap();
        let mut output = Vec::new();
        for _ in 0..10 {
            assert!(resampler.push_sample(0.0, &mut output, 10));
        }
        assert_eq!(output.len(), 10);
        assert!(!resampler.push_sample(0.0, &mut output, 10));
        assert_eq!(output.len(), 10);
    }

    #[test]
    fn equal_rate_identity_preserves_empty_and_chunked_streams() {
        let mut empty = StreamingNeuralResampler::new(22_050, 22_050).unwrap();
        let mut empty_output = Vec::new();
        assert!(empty.finish(&mut empty_output, 8));
        assert!(empty_output.is_empty());

        let samples = [0.0_f32, 0.25, -0.5, 1.0, -1.0, 0.75];
        let mut resampler = StreamingNeuralResampler::new(22_050, 22_050).unwrap();
        let mut output = Vec::new();
        for sample in samples.iter().copied().take(2) {
            assert!(resampler.push_sample(sample, &mut output, samples.len()));
        }
        for sample in samples.iter().copied().skip(2) {
            assert!(resampler.push_sample(sample, &mut output, samples.len()));
        }
        assert!(resampler.finish(&mut output, samples.len()));
        assert_eq!(output, samples);
    }
}
