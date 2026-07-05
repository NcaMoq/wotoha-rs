use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub struct AutoMixConfig {
    pub enabled: bool,
    pub crossfade: Duration,
    pub max_tempo_adjustment: f32,
    pub min_beat_confidence: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrackAnalysis {
    pub duration: Duration,
    pub audible_start: Duration,
    pub audible_end: Duration,
    pub bpm: Option<f32>,
    pub beat_confidence: f32,
}

impl TrackAnalysis {
    pub fn unanalyzed(duration: Duration) -> Self {
        Self {
            duration,
            audible_start: Duration::ZERO,
            audible_end: duration,
            bpm: None,
            beat_confidence: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionKind {
    Gapless,
    Crossfade,
    BeatMatched,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransitionPlan {
    pub kind: TransitionKind,
    pub outgoing_start: Duration,
    pub incoming_start: Duration,
    pub duration: Duration,
    /// Playback speed applied to the incoming deck. `1.0` preserves its tempo.
    pub incoming_tempo_ratio: f32,
}

pub fn plan_transition(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
) -> TransitionPlan {
    if !config.enabled {
        return gapless_plan(outgoing, incoming);
    }

    let available_outgoing = outgoing.audible_end.saturating_sub(outgoing.audible_start);
    let available_incoming = incoming.audible_end.saturating_sub(incoming.audible_start);
    let duration = config
        .crossfade
        .min(available_outgoing)
        .min(available_incoming);
    if duration.is_zero() {
        return gapless_plan(outgoing, incoming);
    }

    let tempo_ratio = compatible_tempo_ratio(outgoing, incoming, config);
    TransitionPlan {
        kind: if tempo_ratio.is_some() {
            TransitionKind::BeatMatched
        } else {
            TransitionKind::Crossfade
        },
        outgoing_start: outgoing.audible_end.saturating_sub(duration),
        incoming_start: incoming.audible_start,
        duration,
        incoming_tempo_ratio: tempo_ratio.unwrap_or(1.0),
    }
}

fn gapless_plan(outgoing: &TrackAnalysis, incoming: &TrackAnalysis) -> TransitionPlan {
    TransitionPlan {
        kind: TransitionKind::Gapless,
        outgoing_start: outgoing.audible_end,
        incoming_start: incoming.audible_start,
        duration: Duration::ZERO,
        incoming_tempo_ratio: 1.0,
    }
}

fn compatible_tempo_ratio(
    outgoing: &TrackAnalysis,
    incoming: &TrackAnalysis,
    config: &AutoMixConfig,
) -> Option<f32> {
    if outgoing.beat_confidence < config.min_beat_confidence
        || incoming.beat_confidence < config.min_beat_confidence
    {
        return None;
    }
    let outgoing_bpm = outgoing.bpm?;
    let incoming_bpm = incoming.bpm?;
    if outgoing_bpm <= 0.0 || incoming_bpm <= 0.0 {
        return None;
    }

    let ratio = outgoing_bpm / incoming_bpm;
    ((ratio - 1.0).abs() <= config.max_tempo_adjustment).then_some(ratio)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AutoMixConfig {
        AutoMixConfig {
            enabled: true,
            crossfade: Duration::from_secs(8),
            max_tempo_adjustment: 0.06,
            min_beat_confidence: 0.7,
        }
    }

    fn analyzed(bpm: f32) -> TrackAnalysis {
        TrackAnalysis {
            duration: Duration::from_secs(180),
            audible_start: Duration::from_secs(1),
            audible_end: Duration::from_secs(179),
            bpm: Some(bpm),
            beat_confidence: 0.9,
        }
    }

    #[test]
    fn chooses_beatmatch_for_compatible_confident_tempos() {
        let plan = plan_transition(&analyzed(120.0), &analyzed(124.0), &config());
        assert_eq!(plan.kind, TransitionKind::BeatMatched);
        assert!((plan.incoming_tempo_ratio - 120.0 / 124.0).abs() < 0.0001);
        assert_eq!(plan.duration, Duration::from_secs(8));
    }

    #[test]
    fn falls_back_to_crossfade_for_incompatible_tempos() {
        let plan = plan_transition(&analyzed(90.0), &analyzed(140.0), &config());
        assert_eq!(plan.kind, TransitionKind::Crossfade);
        assert_eq!(plan.incoming_tempo_ratio, 1.0);
    }

    #[test]
    fn disabled_automix_preserves_trimmed_gapless_boundary() {
        let mut config = config();
        config.enabled = false;
        let outgoing = analyzed(120.0);
        let incoming = analyzed(120.0);
        let plan = plan_transition(&outgoing, &incoming, &config);
        assert_eq!(plan.kind, TransitionKind::Gapless);
        assert_eq!(plan.outgoing_start, outgoing.audible_end);
        assert_eq!(plan.incoming_start, incoming.audible_start);
    }
}
