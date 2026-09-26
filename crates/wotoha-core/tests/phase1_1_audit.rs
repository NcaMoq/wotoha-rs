use std::time::Duration;

use wotoha_core::{
    analysis::{
        BeatEvent, Confidence, CueGenerationInput, DjCue, MeterHypothesis, ModelScore,
        PhraseBoundary, RhythmAnalysis, Section, SectionLabel, StructureAnalysis, Support,
        TempoHypothesis, TempoRelation, TrackAnalysisV2, UnitInterval, generate_heuristic_cues,
        periodic_phrase_prior, top_mix_in_cues, top_mix_out_cues,
    },
    automix::{TempoHypothesisPair, V2AnalysisInput, select_tempo_hypothesis_pair},
};

fn tempo(bpm: f32, weight: f32, relation: TempoRelation) -> TempoHypothesis {
    TempoHypothesis::new(bpm, UnitInterval::clamped(weight), relation).unwrap()
}

#[test]
fn generic_onset_maps_to_its_own_evidence_channel() {
    let event = BeatEvent::new(
        Duration::from_millis(500),
        Some(ModelScore::new(0.72).unwrap()),
        None,
        Confidence::new(0.91).unwrap(),
        Some(Support::new(0.23).unwrap()),
        Some(Support::new(0.96).unwrap()),
    );
    let timeline = wotoha_core::automix::timeline_from_beat_events(&[event]);

    assert_eq!(timeline.events[0].timing_confidence, Some(0.91));
    assert_eq!(timeline.events[0].beat_evidence, Some(0.72));
    assert_eq!(timeline.events[0].generic_onset, Some(0.23));
}

#[test]
fn cue_role_selection_is_ranked_bounded_and_role_specific() {
    let candidates = (0..20)
        .map(|index| {
            let mut cue = DjCue::new(index, UnitInterval::clamped(index as f32 / 20.0));
            if index % 2 == 0 {
                cue.mix_in = UnitInterval::ONE;
            } else {
                cue.mix_out = UnitInterval::ONE;
            }
            cue
        })
        .collect::<Vec<_>>();

    let mix_in = top_mix_in_cues(&candidates);
    let mix_out = top_mix_out_cues(&candidates);
    assert_eq!(mix_in.len(), 8);
    assert_eq!(mix_out.len(), 8);
    assert!(
        mix_in
            .iter()
            .all(|cue| cue.mix_in_enabled() && !cue.mix_out_enabled())
    );
    assert!(
        mix_out
            .iter()
            .all(|cue| cue.mix_out_enabled() && !cue.mix_in_enabled())
    );
    assert_eq!(mix_in.first().unwrap().beat_index, 18);
    assert_eq!(mix_in.last().unwrap().beat_index, 4);
    assert_eq!(mix_out.first().unwrap().beat_index, 19);
    assert_eq!(mix_out.last().unwrap().beat_index, 5);
}

#[test]
fn generated_cues_stay_inside_the_audible_span_even_for_outside_hints() {
    let structure = StructureAnalysis::new(
        vec![Section::new(1, 11, 0.9, [SectionLabel::Intro])],
        vec![PhraseBoundary::periodic_prior(
            5,
            UnitInterval::clamped(0.2),
        )],
    );
    let input = CueGenerationInput::new(12, 4, 8)
        .with_intro_end(Some(1))
        .with_outro_start(Some(11))
        .with_structure(&structure);

    let cues = generate_heuristic_cues(&input);
    assert!(!cues.is_empty());
    assert!(cues.iter().all(|cue| (4..8).contains(&cue.beat_index)));
}

#[test]
fn tempo_pair_weight_is_geometric_and_selection_uses_strength_for_equal_stretch() {
    let outgoing = [
        tempo(120.0, 0.36, TempoRelation::Primary),
        tempo(123.0, 0.90, TempoRelation::Alternative),
    ];
    let incoming = [
        tempo(120.0, 0.64, TempoRelation::Primary),
        tempo(123.0, 0.90, TempoRelation::Alternative),
    ];

    let lower_weight_exact_match = TempoHypothesisPair::new(outgoing[0], incoming[0]).unwrap();
    assert!((lower_weight_exact_match.weight - 0.48).abs() < 1.0e-6);

    let selected = select_tempo_hypothesis_pair(&outgoing, &incoming, 0.05).unwrap();
    assert_eq!(selected.outgoing.bpm, 123.0);
    assert_eq!(selected.incoming.bpm, 123.0);
    assert!((selected.weight - 0.90).abs() < 1.0e-6);
    assert!(select_tempo_hypothesis_pair(&outgoing, &incoming, -0.01).is_none());
}

#[test]
fn unresolved_meter_stays_unknown_in_the_rhythm_domain_and_adapter() {
    let beats = (0..8)
        .map(|index| BeatEvent::at(Duration::from_millis(index * 500), Confidence::ONE))
        .collect::<Vec<_>>();
    let mut analysis = TrackAnalysisV2::unanalyzed(Duration::from_secs(10));
    analysis.rhythm = RhythmAnalysis::new(
        beats,
        Vec::new(),
        vec![
            MeterHypothesis::new(4, 0, UnitInterval::clamped(0.72)).unwrap(),
            MeterHypothesis::new(3, 0, UnitInterval::clamped(0.68)).unwrap(),
        ],
    )
    .unwrap();

    assert_eq!(analysis.rhythm.resolved_meter(), None);
    assert_eq!(analysis.rhythm.bar_position_at_index(0), None);
    let legacy = analysis.as_v2_legacy_view();
    assert_eq!(legacy.first_downbeat, None);
    assert_eq!(legacy.downbeat_confidence, 0.0);
}

#[test]
fn periodic_phrase_prior_does_not_become_legacy_downbeat_evidence() {
    let beats = (0..40)
        .map(|index| BeatEvent::at(Duration::from_millis(index * 500), Confidence::ONE))
        .collect::<Vec<_>>();
    let prior = periodic_phrase_prior(40, 0, 4);
    assert!(!prior.is_empty());
    assert!(prior.iter().all(|boundary| !boundary.is_real_detection()));
    let mut analysis = TrackAnalysisV2::unanalyzed(Duration::from_secs(30));
    analysis.rhythm = RhythmAnalysis::from_beats(beats).unwrap();
    analysis.structure = StructureAnalysis::new(Vec::new(), prior);

    let legacy = analysis.as_v2_legacy_view();
    assert_eq!(legacy.first_downbeat, None);
    assert_eq!(legacy.downbeat_confidence, 0.0);
    assert!(legacy.phrase_cues().is_empty());
}
