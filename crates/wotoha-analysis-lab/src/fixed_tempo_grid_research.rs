//! Research-only fixed-tempo grid fitting.
//!
//! This module deliberately keeps the raw event clock outside the fitted
//! result.  It estimates one stationary period and phase from runtime
//! observations while allowing a small number of missing beats and extra
//! events.  It is not a production authority and never consumes truth,
//! fixture identity, filenames, or external reference values.

use serde::{Deserialize, Serialize};

const MIN_EVENTS: usize = 8;
const MIN_PERIOD_MICROS: f64 = 200_000.0;
const MAX_PERIOD_MICROS: f64 = 1_500_000.0;
const MAX_INDEX_JUMP: i64 = 4;
const ROBUST_RESIDUAL_FLOOR_MICROS: f64 = 35_000.0;
// The runtime event decoder is frame-quantized.  A 35 ms median residual is
// the fixed research tolerance used by the lab's known-clock diagnostics;
// the fitted period remains continuous and the full residual distribution is
// still reported, so this is not a precision claim or an integer-BPM snap.
const MAX_INLIER_MEDIAN_MICROS: f64 = 35_000.0;
const MIN_INLIER_FRACTION: f64 = 0.65;
const MAX_STATIONARY_DRIFT: f64 = 0.03;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StableTempoGrid {
    pub algorithm: String,
    pub accepted: bool,
    pub stationarity_status: String,
    pub raw_event_count: usize,
    pub inlier_event_count: usize,
    pub fit_period_micros: Option<f64>,
    pub fit_bpm: Option<f32>,
    pub phase_micros: Option<f64>,
    pub raw_residual_median_micros: Option<u64>,
    pub raw_residual_p95_micros: Option<u64>,
    pub inlier_residual_median_micros: Option<u64>,
    pub inlier_residual_p95_micros: Option<u64>,
    pub explained_event_fraction: f32,
    pub inserted_grid_count: usize,
    pub rejected_event_count: usize,
    pub stationarity_drift: Option<f64>,
    pub early_period_micros: Option<f64>,
    pub middle_period_micros: Option<f64>,
    pub late_period_micros: Option<f64>,
    pub assigned_indices: Vec<Option<i64>>,
    pub grid_times_micros: Vec<u64>,
    pub reason: String,
}

#[derive(Clone, Debug)]
struct Candidate {
    period: f64,
    phase: f64,
    indices: Vec<i64>,
    residuals: Vec<f64>,
    inlier: Vec<bool>,
    drift: f64,
    early_period: Option<f64>,
    middle_period: Option<f64>,
    late_period: Option<f64>,
}

pub fn fit_stable_tempo_grid(
    times_micros: &[u64],
    duration_micros: u64,
    anchor_bpm: Option<f32>,
) -> StableTempoGrid {
    let unavailable = |reason: &str| StableTempoGrid {
        algorithm: "stable_fixed_tempo_grid_v2".into(),
        accepted: false,
        stationarity_status: "unavailable".into(),
        raw_event_count: times_micros.len(),
        inlier_event_count: 0,
        fit_period_micros: None,
        fit_bpm: None,
        phase_micros: None,
        raw_residual_median_micros: None,
        raw_residual_p95_micros: None,
        inlier_residual_median_micros: None,
        inlier_residual_p95_micros: None,
        explained_event_fraction: 0.0,
        inserted_grid_count: 0,
        rejected_event_count: times_micros.len(),
        stationarity_drift: None,
        early_period_micros: None,
        middle_period_micros: None,
        late_period_micros: None,
        assigned_indices: vec![None; times_micros.len()],
        grid_times_micros: Vec::new(),
        reason: reason.into(),
    };

    if times_micros.len() < MIN_EVENTS {
        return unavailable("insufficient_support");
    }
    if times_micros.windows(2).any(|pair| pair[1] <= pair[0]) {
        return unavailable("non_monotonic_events");
    }

    let intervals = times_micros
        .windows(2)
        .map(|pair| (pair[1] - pair[0]) as f64)
        .collect::<Vec<_>>();
    let Some(median_interval) = median(&intervals) else {
        return unavailable("no_positive_intervals");
    };
    let trimmed_interval = trimmed_median(&intervals);
    let mut seeds = vec![median_interval, trimmed_interval];
    if let Some(bpm) = anchor_bpm.filter(|value| value.is_finite() && *value > 0.0) {
        seeds.push(60_000_000.0 / f64::from(bpm));
    }
    seeds.sort_by(f64::total_cmp);
    seeds.dedup_by(|left, right| (*left - *right).abs() <= 0.5);

    let mut candidates = seeds
        .into_iter()
        .filter(|seed| (MIN_PERIOD_MICROS..=MAX_PERIOD_MICROS).contains(seed))
        .filter_map(|seed| fit_candidate(times_micros, seed))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| candidate_score(right).total_cmp(&candidate_score(left)));
    let Some(best) = candidates.into_iter().next() else {
        return unavailable("bounded_fit_failed");
    };

    let inlier_count = best.inlier.iter().filter(|value| **value).count();
    let inlier_fraction = inlier_count as f64 / times_micros.len() as f64;
    let inlier_residuals = best
        .residuals
        .iter()
        .zip(best.inlier.iter())
        .filter_map(|(residual, inlier)| (*inlier).then_some(*residual))
        .collect::<Vec<_>>();
    let inlier_median = median(&inlier_residuals);
    let stationarity_ok = best.drift <= MAX_STATIONARY_DRIFT;
    let residual_ok = inlier_median.is_some_and(|value| value <= MAX_INLIER_MEDIAN_MICROS);
    let support_ok = inlier_fraction >= MIN_INLIER_FRACTION;
    let accepted = stationarity_ok && residual_ok && support_ok;
    let stationarity_status = if !stationarity_ok {
        "nonstationary"
    } else if !support_ok || !residual_ok {
        "residual_or_support_rejected"
    } else {
        "stationary"
    };
    let grid_times = if accepted {
        let min_index = *best.indices.iter().min().unwrap_or(&0);
        let max_index = *best.indices.iter().max().unwrap_or(&0);
        (min_index..=max_index)
            .filter_map(|index| {
                let time = best.phase + index as f64 * best.period;
                (time >= 0.0 && time <= duration_micros as f64).then_some(time.round() as u64)
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let unique_indices = best
        .indices
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    let inserted = grid_times.len().saturating_sub(unique_indices.len());
    StableTempoGrid {
        algorithm: "stable_fixed_tempo_grid_v2".into(),
        accepted,
        stationarity_status: stationarity_status.into(),
        raw_event_count: times_micros.len(),
        inlier_event_count: inlier_count,
        fit_period_micros: Some(best.period),
        fit_bpm: Some((60_000_000.0 / best.period) as f32),
        phase_micros: Some(best.phase),
        raw_residual_median_micros: median(&best.residuals).map(round_u64),
        raw_residual_p95_micros: percentile(&best.residuals, 0.95).map(round_u64),
        inlier_residual_median_micros: inlier_median.map(round_u64),
        inlier_residual_p95_micros: percentile(&inlier_residuals, 0.95).map(round_u64),
        explained_event_fraction: inlier_fraction as f32,
        inserted_grid_count: inserted,
        rejected_event_count: times_micros.len().saturating_sub(inlier_count),
        stationarity_drift: Some(best.drift),
        early_period_micros: best.early_period,
        middle_period_micros: best.middle_period,
        late_period_micros: best.late_period,
        assigned_indices: best.indices.iter().map(|index| Some(*index)).collect(),
        grid_times_micros: grid_times,
        reason: if accepted {
            "accepted_stationary_robust_global_fit".into()
        } else {
            "stationarity_or_residual_guard_rejected".into()
        },
    }
}

fn fit_candidate(times: &[u64], seed_period: f64) -> Option<Candidate> {
    let mut period = seed_period;
    let mut phase = initial_phase(times, period);
    let mut indices = assign_indices(times, phase, period);
    for _ in 0..10 {
        let representatives = representatives(times, &indices, phase, period);
        if representatives.len() < MIN_EVENTS {
            return None;
        }
        let residuals = representatives
            .iter()
            .map(|(index, time)| (*time - (phase + *index as f64 * period)).abs())
            .collect::<Vec<_>>();
        let center = median(&residuals).unwrap_or(f64::INFINITY);
        let mad = median(
            &residuals
                .iter()
                .map(|value| (value - center).abs())
                .collect::<Vec<_>>(),
        )
        .unwrap_or(f64::INFINITY);
        let cutoff = ROBUST_RESIDUAL_FLOOR_MICROS.max(center + 4.0 * 1.4826 * mad);
        let inliers = representatives
            .iter()
            .zip(residuals.iter())
            .filter_map(|((index, time), residual)| {
                (*residual <= cutoff).then_some((*index, *time))
            })
            .collect::<Vec<_>>();
        if inliers.len() < MIN_EVENTS {
            return None;
        }
        let (new_phase, new_period) = least_squares(&inliers)?;
        if !(MIN_PERIOD_MICROS..=MAX_PERIOD_MICROS).contains(&new_period) {
            return None;
        }
        let stable = (new_period - period).abs() < 0.25 && (new_phase - phase).abs() < 0.25;
        period = new_period;
        phase = new_phase;
        indices = assign_indices(times, phase, period);
        if stable {
            break;
        }
    }

    let residuals = times
        .iter()
        .zip(indices.iter())
        .map(|(time, index)| (*time as f64 - (phase + *index as f64 * period)).abs())
        .collect::<Vec<_>>();
    let center = median(&residuals).unwrap_or(f64::INFINITY);
    let mad = median(
        &residuals
            .iter()
            .map(|value| (value - center).abs())
            .collect::<Vec<_>>(),
    )
    .unwrap_or(f64::INFINITY);
    let cutoff = ROBUST_RESIDUAL_FLOOR_MICROS.max(center + 4.0 * 1.4826 * mad);
    let inlier = residuals
        .iter()
        .map(|value| *value <= cutoff)
        .collect::<Vec<_>>();
    let (early_period, middle_period, late_period) = segment_periods(times, &indices, &inlier);
    let drift = early_period
        .zip(late_period)
        .map(|(early, late)| (late - early).abs() / period.max(1.0))
        .unwrap_or(f64::INFINITY);
    Some(Candidate {
        period,
        phase,
        indices,
        residuals,
        inlier,
        drift,
        early_period,
        middle_period,
        late_period,
    })
}

fn initial_phase(times: &[u64], period: f64) -> f64 {
    let phases = times
        .iter()
        .map(|time| {
            let turns = (*time as f64 / period).round();
            *time as f64 - turns * period
        })
        .collect::<Vec<_>>();
    median(&phases).unwrap_or(times[0] as f64)
}

fn assign_indices(times: &[u64], phase: f64, period: f64) -> Vec<i64> {
    let mut result = Vec::with_capacity(times.len());
    for time in times {
        let proposed = ((*time as f64 - phase) / period).round() as i64;
        let index = result.last().copied().map_or(proposed, |last: i64| {
            proposed.max(last).min(last + MAX_INDEX_JUMP)
        });
        result.push(index);
    }
    result
}

fn representatives(times: &[u64], indices: &[i64], phase: f64, period: f64) -> Vec<(i64, f64)> {
    let mut output: Vec<(i64, f64)> = Vec::new();
    for (position, index) in indices.iter().enumerate() {
        let error = (times[position] as f64 - (phase + *index as f64 * period)).abs();
        if let Some(existing) = output.iter_mut().find(|(current, _)| current == index) {
            let existing_error = (existing.1 - (phase + *index as f64 * period)).abs();
            if error < existing_error {
                existing.1 = times[position] as f64;
            }
        } else {
            output.push((*index, times[position] as f64));
        }
    }
    output
}

fn least_squares(points: &[(i64, f64)]) -> Option<(f64, f64)> {
    if points.len() < 2 {
        return None;
    }
    let mean_x = points.iter().map(|(x, _)| *x as f64).sum::<f64>() / points.len() as f64;
    let mean_y = points.iter().map(|(_, y)| *y).sum::<f64>() / points.len() as f64;
    let denominator = points
        .iter()
        .map(|(x, _)| (*x as f64 - mean_x).powi(2))
        .sum::<f64>();
    if denominator <= f64::EPSILON {
        return None;
    }
    let period = points
        .iter()
        .map(|(x, y)| (*x as f64 - mean_x) * (*y - mean_y))
        .sum::<f64>()
        / denominator;
    let phase = mean_y - period * mean_x;
    period.is_finite().then_some((phase, period))
}

fn segment_periods(
    times: &[u64],
    indices: &[i64],
    inlier: &[bool],
) -> (Option<f64>, Option<f64>, Option<f64>) {
    let mut points = times
        .iter()
        .zip(indices.iter())
        .zip(inlier.iter())
        .filter_map(|((time, index), keep)| (*keep).then_some((*index, *time as f64)))
        .collect::<Vec<_>>();
    if points.len() < MIN_EVENTS {
        return (None, None, None);
    }
    let chunk = (points.len() / 3).max(2);
    let first = least_squares(&points[..chunk.min(points.len())]).map(|(_, period)| period);
    let middle_start = points.len() / 3;
    let middle_end = (points.len() * 2 / 3)
        .max(middle_start + 2)
        .min(points.len());
    let middle = (middle_end > middle_start)
        .then(|| least_squares(&points[middle_start..middle_end]))
        .flatten()
        .map(|(_, period)| period);
    let late_start = points.len().saturating_sub(chunk);
    let late = least_squares(&points[late_start..]).map(|(_, period)| period);
    points.clear();
    (first, middle, late)
}

fn candidate_score(candidate: &Candidate) -> f64 {
    let inlier_fraction = candidate.inlier.iter().filter(|value| **value).count() as f64
        / candidate.inlier.len().max(1) as f64;
    let median_residual = median(
        &candidate
            .residuals
            .iter()
            .zip(candidate.inlier.iter())
            .filter_map(|(value, keep)| (*keep).then_some(*value))
            .collect::<Vec<_>>(),
    )
    .unwrap_or(f64::INFINITY);
    inlier_fraction - 4.0 * median_residual / candidate.period.max(1.0) - 4.0 * candidate.drift
}

fn trimmed_median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let trim = (sorted.len() / 20).min(sorted.len().saturating_sub(1));
    median(&sorted[trim..sorted.len().saturating_sub(trim)]).unwrap_or(sorted[sorted.len() / 2])
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    Some(sorted[sorted.len() / 2])
}

fn percentile(values: &[f64], fraction: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let index = ((sorted.len() - 1) as f64 * fraction.clamp(0.0, 1.0)).round() as usize;
    sorted.get(index).copied()
}

fn round_u64(value: f64) -> u64 {
    value.max(0.0).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock(period: f64, count: usize) -> Vec<u64> {
        (0..count)
            .map(|index| (index as f64 * period).round() as u64)
            .collect()
    }

    #[test]
    fn recovers_non_integer_period_without_integer_snap() {
        let result = fit_stable_tempo_grid(&clock(600_000.0 / 1.0, 100), 60_000_000, None);
        assert!(result.accepted);
        assert!((result.fit_bpm.unwrap() - 100.0).abs() < 0.001);

        let result = fit_stable_tempo_grid(&clock(60_000_000.0 / 127.5, 100), 60_000_000, None);
        assert!(result.accepted);
        assert!((result.fit_bpm.unwrap() - 127.5).abs() < 0.001);
    }

    #[test]
    fn recovers_non_integer_fixture_bpm_values() {
        for truth_bpm in [95.7, 119.8, 127.5, 129.7, 174.2] {
            let result =
                fit_stable_tempo_grid(&clock(60_000_000.0 / truth_bpm, 120), 90_000_000, None);
            assert!(result.accepted, "{truth_bpm}: {result:?}");
            assert!((f64::from(result.fit_bpm.unwrap()) - truth_bpm).abs() < 0.01);
        }
    }

    #[test]
    fn tolerates_missing_beats_and_extra_events() {
        let mut times = clock(500_000.0, 100);
        times.remove(40);
        times.insert(55, times[54] + 80_000);
        times.sort_unstable();
        let result = fit_stable_tempo_grid(&times, 60_000_000, None);
        assert!(result.accepted);
        assert!(result.rejected_event_count >= 1);
        assert!((result.fit_bpm.unwrap() - 120.0).abs() < 0.1);
    }

    #[test]
    fn rejects_nonstationary_clock() {
        let mut times = clock(500_000.0, 80);
        for (index, time) in times.iter_mut().enumerate().skip(40) {
            *time += (index - 39) as u64 * 20_000;
        }
        let result = fit_stable_tempo_grid(&times, 60_000_000, None);
        assert!(!result.accepted);
        assert_ne!(result.stationarity_status, "stationary");
    }

    #[test]
    fn tolerates_bounded_jitter_and_bad_endpoints() {
        let mut times = clock(500_000.0, 100);
        for (index, time) in times.iter_mut().enumerate() {
            *time = time.saturating_add((index % 5) as u64 * 3_000);
        }
        times[0] = 80_000;
        times[99] = times[98] + 620_000;
        let result = fit_stable_tempo_grid(&times, 60_000_000, None);
        assert!(result.accepted, "{result:?}");
        assert!((f64::from(result.fit_bpm.unwrap()) - 120.0).abs() < 0.2);
        assert!(result.rejected_event_count >= 1);
    }

    #[test]
    fn is_deterministic() {
        let times = clock(469_000.0, 100);
        assert_eq!(
            fit_stable_tempo_grid(&times, 60_000_000, None),
            fit_stable_tempo_grid(&times, 60_000_000, None)
        );
    }
}
