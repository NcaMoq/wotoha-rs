use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use songbird::Songbird;
use wotoha_contracts::VoiceRuntime;
use wotoha_core::{
    TrackRequest,
    automix::{AutoMixConfig, TrackAnalysis, plan_guarded_transition},
};
use wotoha_media::MediaResolver;
use wotoha_runtime::SongbirdRuntime;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let options = ProbeOptions::parse(std::env::args().skip(1))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    if options.urls.is_empty() {
        eprintln!(
            "usage: cargo run -p wotoha-app --bin track_probe -- [--warmup] [--automix-plan] [--automix-preview <file.wav>] <url>..."
        );
        std::process::exit(2);
    }
    let automix_requested = options.automix_plan || options.automix_preview.is_some();
    if automix_requested && options.urls.len() < 2 {
        eprintln!("AutoMix probing requires at least two URLs");
        std::process::exit(2);
    }
    if options.automix_preview.is_some() && options.urls.len() != 2 {
        eprintln!("--automix-preview requires exactly two URLs");
        std::process::exit(2);
    }

    let resolver = MediaResolver::new()?;
    if options.warmup {
        resolver.warmup_providers().await;
    }
    let runtime = SongbirdRuntime::new(Songbird::serenity())?;
    let mut prepared_tracks = Vec::new();

    for (index, url) in options.urls.into_iter().enumerate() {
        let started_at = Instant::now();
        println!("SOURCE\t{index}\t{url}");
        match resolver.resolve(&url).await {
            Ok(request) => {
                let prepared = resolver.prepare_playback(&request).await?;
                println!(
                    "RESOLVED\t{index}\t{}\t{}\t{}",
                    prepared.provider_id, prepared.canonical_key, prepared.metadata.title
                );
                match runtime.verify_track(&prepared).await {
                    Ok(()) => println!("PLAYABLE\t{index}\tok"),
                    Err(error) => println!("PLAYABLE\t{index}\terror\t{error}"),
                }
                let analysis = if automix_requested {
                    let analysis = runtime.analyze_track(&prepared).await;
                    print_analysis(index, analysis.as_ref());
                    analysis
                } else {
                    None
                };
                prepared_tracks.push(PreparedProbe {
                    index,
                    request: Some(prepared),
                    analysis,
                });
            }
            Err(error) => {
                println!("RESOLVED\t{index}\terror\t{error}");
                if automix_requested {
                    prepared_tracks.push(PreparedProbe {
                        index,
                        request: None,
                        analysis: None,
                    });
                }
            }
        }
        println!(
            "ELAPSED_MS\t{index}\t{:.2}",
            started_at.elapsed().as_secs_f64() * 1000.0
        );
    }

    if automix_requested {
        print_automix_plans(&prepared_tracks);
    }
    if let Some(path) = options.automix_preview.as_ref() {
        render_preview(&runtime, &prepared_tracks, path).await?;
    }

    Ok(())
}

struct PreparedProbe {
    index: usize,
    request: Option<TrackRequest>,
    analysis: Option<TrackAnalysis>,
}

struct ProbeOptions {
    warmup: bool,
    automix_plan: bool,
    automix_preview: Option<PathBuf>,
    urls: Vec<String>,
}

impl ProbeOptions {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut warmup = false;
        let mut automix_plan = false;
        let mut automix_preview = None;
        let mut urls = Vec::new();
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--warmup" => warmup = true,
                "--automix-plan" => automix_plan = true,
                "--automix-preview" => {
                    let Some(path) = args.next() else {
                        return Err("--automix-preview requires a file path".to_owned());
                    };
                    automix_preview = Some(PathBuf::from(path));
                }
                value if value.starts_with("--") => {
                    return Err(format!("unknown option: {value}"));
                }
                _ => urls.push(arg),
            }
        }

        Ok(Self {
            warmup,
            automix_plan,
            automix_preview,
            urls,
        })
    }
}

fn print_analysis(index: usize, analysis: Option<&TrackAnalysis>) {
    let Some(analysis) = analysis else {
        println!("ANALYSIS\t{index}\terror\tunavailable");
        return;
    };
    println!(
        "ANALYSIS\t{index}\tok\tduration_ms={}\taudible_start_ms={}\taudible_end_ms={}\tbpm={}\tbeat_confidence={:.3}\tkick_coverage={:.3}\tintro_ms={}\toutro_ms={}\trms_dbfs={}\tpeak_dbfs={}",
        analysis.duration.as_millis(),
        analysis.audible_start.as_millis(),
        analysis.audible_end.as_millis(),
        format_optional_f32(analysis.bpm),
        analysis.beat_confidence,
        analysis.trusted_kick_coverage(),
        format_optional_duration_ms(analysis.intro_end),
        format_optional_duration_ms(analysis.outro_start),
        format_optional_f32(analysis.rms_dbfs),
        format_optional_f32(analysis.sample_peak_dbfs),
    );
}

fn print_automix_plans(tracks: &[PreparedProbe]) {
    let config = automix_probe_config();
    for pair in tracks.windows(2) {
        let [outgoing, incoming] = pair else {
            continue;
        };
        let Some(outgoing_analysis) = outgoing.analysis.as_ref() else {
            println!(
                "AUTOMIX_PLAN\t{}\t{}\terror\tmissing_outgoing_analysis",
                outgoing.index, incoming.index
            );
            continue;
        };
        let Some(incoming_analysis) = incoming.analysis.as_ref() else {
            println!(
                "AUTOMIX_PLAN\t{}\t{}\terror\tmissing_incoming_analysis",
                outgoing.index, incoming.index
            );
            continue;
        };
        let guarded = plan_guarded_transition(outgoing_analysis, incoming_analysis, &config);
        let plan = &guarded.plan;
        let quality = &guarded.quality;
        println!(
            "AUTOMIX_PLAN\t{}\t{}\tok\toutgoing_key={}\tincoming_key={}\tguarded={}\trejected_kind={}\trejected_quality_issues={}\tkind={:?}\toutgoing_start_ms={}\tincoming_start_ms={}\tfade_ms={}\ttempo_ratio={:.6}\ttempo_end_ratio={:.6}\tincoming_gain={:.3}\tquality_ok={}\tquality_issues={:?}\tharmonic_compatibility={}\tbeat_pairs_checked={}\tmax_beat_phase_error_ms={}\thandoff_beat_phase_error_ms={}\tdownbeat_pairs_checked={}\tmax_downbeat_phase_error_ms={}\thandoff_downbeat_phase_error_ms={}\tphrase_pairs_checked={}\tmax_phrase_phase_error_ms={}\thandoff_phrase_phase_error_ms={}\tlow_handoff_min={}\tlow_handoff_max={}\tvocal_overlap_samples_checked={}\tmax_dual_vocal_risk={}\tenergy_samples_checked={}\tmin_mix_energy_ratio={}\tmax_mix_energy_ratio={}",
            outgoing.index,
            incoming.index,
            track_key(outgoing),
            track_key(incoming),
            guarded.rejected_quality.is_some(),
            guarded
                .rejected_plan
                .as_ref()
                .map_or_else(|| "-".to_owned(), |plan| format!("{:?}", plan.kind)),
            guarded
                .rejected_quality
                .as_ref()
                .map_or_else(|| "-".to_owned(), |quality| format!("{:?}", quality.issues)),
            plan.kind,
            plan.outgoing_start.as_millis(),
            plan.incoming_start.as_millis(),
            plan.duration.as_millis(),
            plan.incoming_tempo_ratio,
            plan.tempo_envelope
                .map_or(plan.incoming_tempo_ratio, |envelope| envelope.mix_end_speed),
            plan.incoming_gain,
            quality.is_ok(),
            quality.issues,
            format_optional_f32(quality.harmonic_compatibility),
            quality.beat_pairs_checked,
            format_optional_duration_ms(quality.max_beat_phase_error),
            format_optional_duration_ms(quality.handoff_beat_phase_error),
            quality.downbeat_pairs_checked,
            format_optional_duration_ms(quality.max_downbeat_phase_error),
            format_optional_duration_ms(quality.handoff_downbeat_phase_error),
            quality.phrase_pairs_checked,
            format_optional_duration_ms(quality.max_phrase_phase_error),
            format_optional_duration_ms(quality.handoff_phrase_phase_error),
            format_optional_f32(quality.low_handoff_min),
            format_optional_f32(quality.low_handoff_max),
            quality.vocal_overlap_samples_checked,
            format_optional_f32(quality.max_dual_vocal_risk),
            quality.energy_samples_checked,
            format_optional_f32(quality.min_mix_energy_ratio),
            format_optional_f32(quality.max_mix_energy_ratio),
        );
    }
}

async fn render_preview(
    runtime: &SongbirdRuntime,
    tracks: &[PreparedProbe],
    path: &PathBuf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let [outgoing, incoming] = tracks else {
        return Err(probe_error("preview requires exactly two prepared tracks"));
    };
    let outgoing_request = outgoing
        .request
        .as_ref()
        .ok_or_else(|| probe_error("preview outgoing track was not resolved"))?;
    let incoming_request = incoming
        .request
        .as_ref()
        .ok_or_else(|| probe_error("preview incoming track was not resolved"))?;
    let outgoing_analysis = outgoing
        .analysis
        .as_ref()
        .ok_or_else(|| probe_error("preview outgoing analysis is unavailable"))?;
    let incoming_analysis = incoming
        .analysis
        .as_ref()
        .ok_or_else(|| probe_error("preview incoming analysis is unavailable"))?;
    let preview = runtime
        .render_automix_preview(
            outgoing_request,
            incoming_request,
            outgoing_analysis,
            incoming_analysis,
            &automix_probe_config(),
        )
        .await?;
    let bytes = preview.wav.len();
    std::fs::write(path, &preview.wav)?;
    println!(
        "AUTOMIX_PREVIEW\t{}\t{}\tok\tpath={}\tsample_rate={}\tchannels={}\tbytes={}\tkind={:?}\tquality_ok={}\trender_ok={}\trender_issues={:?}\tstart_rms_dbfs={:.2}\tmid_rms_dbfs={:.2}\tend_rms_dbfs={:.2}\tquietest_window_rms_dbfs={:.2}\tquietest_to_edge_ratio={:.3}\tmid_to_edge_ratio={:.3}\tsample_peak_dbfs={:.2}",
        outgoing.index,
        incoming.index,
        path.display(),
        preview.sample_rate,
        preview.channels,
        bytes,
        preview.plan.kind,
        preview.quality.is_ok(),
        preview.render_issues.is_empty(),
        preview.render_issues,
        preview.render_metrics.start_rms_dbfs,
        preview.render_metrics.mid_rms_dbfs,
        preview.render_metrics.end_rms_dbfs,
        preview.render_metrics.quietest_window_rms_dbfs,
        preview.render_metrics.quietest_to_edge_ratio,
        preview.render_metrics.mid_to_edge_ratio,
        preview.render_metrics.sample_peak_dbfs,
    );
    Ok(())
}

fn probe_error(message: &'static str) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message,
    ))
}

fn automix_probe_config() -> AutoMixConfig {
    AutoMixConfig {
        enabled: true,
        crossfade: Duration::from_secs(8),
        max_tempo_adjustment: 0.06,
        min_beat_confidence: 0.7,
    }
}

fn format_optional_duration_ms(value: Option<Duration>) -> String {
    value
        .map(|value| value.as_millis().to_string())
        .unwrap_or_else(|| "-".to_owned())
}

fn format_optional_f32(value: Option<f32>) -> String {
    value
        .map(|value| format!("{value:.3}"))
        .unwrap_or_else(|| "-".to_owned())
}

fn track_key(track: &PreparedProbe) -> &str {
    track
        .request
        .as_ref()
        .map(|request| request.canonical_key.as_ref())
        .unwrap_or("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_automix_plan_option() {
        let options = ProbeOptions::parse([
            "--warmup".to_owned(),
            "--automix-plan".to_owned(),
            "one".to_owned(),
            "two".to_owned(),
        ])
        .unwrap();

        assert!(options.warmup);
        assert!(options.automix_plan);
        assert_eq!(options.automix_preview, None);
        assert_eq!(options.urls, ["one", "two"]);
    }

    #[test]
    fn parses_automix_preview_option() {
        let options = ProbeOptions::parse([
            "--automix-preview".to_owned(),
            "preview.wav".to_owned(),
            "one".to_owned(),
            "two".to_owned(),
        ])
        .unwrap();

        assert!(!options.automix_plan);
        assert_eq!(options.automix_preview, Some(PathBuf::from("preview.wav")));
        assert_eq!(options.urls, ["one", "two"]);
    }
}
