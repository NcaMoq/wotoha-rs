use crate::{automix::TrackAnalysis, config::LoudnessConfig};

/// Computes the per-track linear gain used for loudness normalization.
///
/// Boosting requires both integrated loudness and true-peak measurements so
/// the configured ceiling can be enforced. Attenuation remains safe when a
/// true-peak measurement is unavailable.
pub fn loudness_normalization_gain(
    config: &LoudnessConfig,
    analysis: Option<&TrackAnalysis>,
) -> f32 {
    if !config.enabled
        || !config.target_lufs.is_finite()
        || !config.max_boost_db.is_finite()
        || config.max_boost_db < 0.0
        || !config.true_peak_ceiling_dbtp.is_finite()
    {
        return 1.0;
    }

    let Some(analysis) = analysis else {
        return 1.0;
    };
    let Some(integrated_lufs) = analysis.integrated_lufs else {
        return 1.0;
    };
    if !integrated_lufs.is_finite() {
        return 1.0;
    }
    let true_peak_dbtp = match analysis.true_peak_dbtp {
        Some(value) if value.is_finite() => Some(value),
        Some(_) => return 1.0,
        None => None,
    };

    let desired_db = config.target_lufs - integrated_lufs;
    if !desired_db.is_finite() {
        return 1.0;
    }
    let applied_db = if desired_db > 0.0 {
        let Some(true_peak_dbtp) = true_peak_dbtp else {
            return 1.0;
        };
        desired_db
            .min(config.max_boost_db)
            .min(config.true_peak_ceiling_dbtp - true_peak_dbtp)
    } else if let Some(true_peak_dbtp) = true_peak_dbtp {
        desired_db.min(config.true_peak_ceiling_dbtp - true_peak_dbtp)
    } else {
        desired_db
    };

    linear_gain_from_db(applied_db)
}

/// Converts a finite decibel gain to a finite, strictly positive linear gain.
pub fn linear_gain_from_db(gain_db: f32) -> f32 {
    if !gain_db.is_finite() {
        return 1.0;
    }
    10.0_f64
        .powf(f64::from(gain_db) / 20.0)
        .clamp(f64::from(f32::MIN_POSITIVE), f64::from(f32::MAX)) as f32
}

/// Interpolates two positive linear gains on a decibel scale.
pub fn interpolate_linear_gain_db(start: f32, end: f32, progress: f32) -> f32 {
    if !start.is_finite() || start <= 0.0 || !end.is_finite() || end <= 0.0 || !progress.is_finite()
    {
        return 1.0;
    }
    let progress = progress.clamp(0.0, 1.0);
    let start_db = 20.0 * start.log10();
    let end_db = 20.0 * end.log10();
    linear_gain_from_db(start_db + (end_db - start_db) * progress)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn config() -> LoudnessConfig {
        LoudnessConfig {
            enabled: true,
            target_lufs: -16.0,
            max_boost_db: 6.0,
            true_peak_ceiling_dbtp: -2.0,
        }
    }

    fn analysis(integrated_lufs: Option<f32>, true_peak_dbtp: Option<f32>) -> TrackAnalysis {
        let mut analysis = TrackAnalysis::unanalyzed(Duration::from_secs(180));
        analysis.integrated_lufs = integrated_lufs;
        analysis.true_peak_dbtp = true_peak_dbtp;
        analysis
    }

    fn gain_db(gain: f32) -> f32 {
        20.0 * gain.log10()
    }

    #[test]
    fn disabled_or_missing_measurement_is_unity() {
        let mut disabled = config();
        disabled.enabled = false;
        assert_eq!(
            loudness_normalization_gain(&disabled, Some(&analysis(Some(-22.0), Some(-8.0)))),
            1.0
        );
        assert_eq!(loudness_normalization_gain(&config(), None), 1.0);
        assert_eq!(
            loudness_normalization_gain(&config(), Some(&analysis(None, Some(-8.0)))),
            1.0
        );
    }

    #[test]
    fn desired_gain_reaches_target_when_below_all_caps() {
        let gain = loudness_normalization_gain(&config(), Some(&analysis(Some(-19.0), Some(-8.0))));
        assert!((gain_db(gain) - 3.0).abs() < 0.0001);
    }

    #[test]
    fn boost_is_limited_by_configured_maximum() {
        let gain =
            loudness_normalization_gain(&config(), Some(&analysis(Some(-30.0), Some(-20.0))));
        assert!((gain_db(gain) - 6.0).abs() < 0.0001);
    }

    #[test]
    fn boost_is_limited_by_true_peak_ceiling() {
        let gain = loudness_normalization_gain(&config(), Some(&analysis(Some(-22.0), Some(-3.0))));
        assert!((gain_db(gain) - 1.0).abs() < 0.0001);
    }

    #[test]
    fn missing_true_peak_blocks_boost_but_allows_attenuation() {
        assert_eq!(
            loudness_normalization_gain(&config(), Some(&analysis(Some(-22.0), None))),
            1.0
        );
        let gain = loudness_normalization_gain(&config(), Some(&analysis(Some(-10.0), None)));
        assert!((gain_db(gain) + 6.0).abs() < 0.0001);
    }

    #[test]
    fn hot_true_peak_can_require_more_attenuation_than_loudness_target() {
        let gain = loudness_normalization_gain(&config(), Some(&analysis(Some(-14.0), Some(1.0))));
        assert!((gain_db(gain) + 3.0).abs() < 0.0001);
    }

    #[test]
    fn non_finite_config_or_measurement_is_unity() {
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                loudness_normalization_gain(&config(), Some(&analysis(Some(value), Some(-8.0)))),
                1.0
            );
            assert_eq!(
                loudness_normalization_gain(&config(), Some(&analysis(Some(-18.0), Some(value)))),
                1.0
            );
        }
        let mut invalid = config();
        invalid.target_lufs = f32::NAN;
        assert_eq!(
            loudness_normalization_gain(&invalid, Some(&analysis(Some(-18.0), Some(-8.0)))),
            1.0
        );
        invalid = config();
        invalid.max_boost_db = f32::INFINITY;
        assert_eq!(
            loudness_normalization_gain(&invalid, Some(&analysis(Some(-18.0), Some(-8.0)))),
            1.0
        );
        invalid = config();
        invalid.true_peak_ceiling_dbtp = f32::NEG_INFINITY;
        assert_eq!(
            loudness_normalization_gain(&invalid, Some(&analysis(Some(-18.0), Some(-8.0)))),
            1.0
        );
    }

    #[test]
    fn returned_gain_is_always_finite_and_positive() {
        for integrated_lufs in [-f32::MAX, -1_000.0, -16.0, 1_000.0, f32::MAX] {
            let gain = loudness_normalization_gain(
                &config(),
                Some(&analysis(Some(integrated_lufs), Some(-20.0))),
            );
            assert!(gain.is_finite() && gain > 0.0, "gain={gain}");
        }
    }

    #[test]
    fn ramp_interpolates_on_decibel_scale() {
        let end = linear_gain_from_db(-12.0);
        let midpoint = interpolate_linear_gain_db(1.0, end, 0.5);
        assert!((gain_db(midpoint) + 6.0).abs() < 0.0001);
        assert_eq!(interpolate_linear_gain_db(1.0, end, 0.0), 1.0);
        assert!((interpolate_linear_gain_db(1.0, end, 1.0) - end).abs() < 0.0001);
    }
}
