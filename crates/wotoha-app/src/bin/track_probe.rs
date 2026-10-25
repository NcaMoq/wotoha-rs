use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use songbird::Songbird;
use wotoha_contracts::VoiceRuntime;
use wotoha_core::{
    TrackRequest,
    automix::{
        AutoMixConfig, TrackAnalysis, plan_guarded_transition_with_base_gains,
        transition_score_breakdown,
    },
    config::LoudnessConfig,
    loudness::loudness_normalization_gain,
    operational_metrics::{YtDlpWorkerProfile, operational_metrics},
};
use wotoha_media::MediaResolver;
use wotoha_runtime::{SongbirdRuntime, SongbirdRuntimeError};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let options = ProbeOptions::parse(std::env::args().skip(1))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    if options.urls.is_empty() {
        eprintln!(
            "usage: cargo run -p wotoha-app --bin track_probe -- [--strict] [--warmup] [--automix-plan] [--automix-preview <file.wav>] [--loudness-max-boost-db <db>] <url>..."
        );
        std::process::exit(2);
    }
    let loudness_max_boost_db = options.loudness_max_boost_db;
    let automix_requested = options.automix_plan || options.automix_preview.is_some();
    if automix_requested && options.urls.len() < 2 {
        eprintln!("AutoMix probing requires at least two URLs");
        std::process::exit(2);
    }
    if options.automix_preview.is_some() && options.urls.len() != 2 {
        eprintln!("--automix-preview requires exactly two URLs");
        std::process::exit(2);
    }

    if options.parallelism > 1 && automix_requested {
        eprintln!("--parallelism cannot be combined with AutoMix preview options");
        std::process::exit(2);
    }

    let resolver = MediaResolver::new()?;
    if options.warmup {
        resolver.warmup_providers().await;
    }
    let runtime = SongbirdRuntime::new(Songbird::serenity())?;
    if options.parallelism > 1 {
        return run_parallel_probe(
            Arc::new(resolver),
            Arc::new(runtime),
            options.parallelism,
            options.strict,
            options.urls,
        )
        .await;
    }
    let mut prepared_tracks = Vec::new();
    let mut probe_failed = false;

    for (index, url) in options.urls.into_iter().enumerate() {
        let started_at = Instant::now();
        println!("SOURCE\t{index}\t{url}");
        let metrics_before = operational_metrics().snapshot();
        let resolve_started_at = Instant::now();
        let resolved = resolver.resolve(&url).await;
        let youtube_extract = resolve_started_at.elapsed();
        let metrics_after = operational_metrics().snapshot();
        let mut timings = TrackProbeTimings {
            youtube_extract: Some(youtube_extract),
            yt_dlp_process: latency_delta(
                metrics_before.yt_dlp_process_latency,
                metrics_after.yt_dlp_process_latency,
            ),
            yt_dlp_process_count: process_count_delta(
                metrics_before.yt_dlp_process_latency,
                metrics_after.yt_dlp_process_latency,
            ),
            worker_startup: latency_delta(
                metrics_before.yt_dlp_worker_startup_latency,
                metrics_after.yt_dlp_worker_startup_latency,
            ),
            worker_ipc: latency_delta(
                metrics_before.yt_dlp_worker_ipc_latency,
                metrics_after.yt_dlp_worker_ipc_latency,
            ),
            worker_api_extract: latency_delta(
                metrics_before.yt_dlp_worker_api_latency,
                metrics_after.yt_dlp_worker_api_latency,
            ),
            worker_import: latency_delta(
                metrics_before.yt_dlp_worker_import_latency,
                metrics_after.yt_dlp_worker_import_latency,
            ),
            worker_startup_count: process_count_delta(
                metrics_before.yt_dlp_worker_startup_latency,
                metrics_after.yt_dlp_worker_startup_latency,
            ),
            worker_ipc_count: process_count_delta(
                metrics_before.yt_dlp_worker_ipc_latency,
                metrics_after.yt_dlp_worker_ipc_latency,
            ),
            worker_api_count: process_count_delta(
                metrics_before.yt_dlp_worker_api_latency,
                metrics_after.yt_dlp_worker_api_latency,
            ),
            format_fallback_count: metrics_after
                .yt_dlp_format_fallbacks
                .saturating_sub(metrics_before.yt_dlp_format_fallbacks),
            cli_fallback_count: metrics_after
                .yt_dlp_worker_cli_fallbacks
                .saturating_sub(metrics_before.yt_dlp_worker_cli_fallbacks),
            worker_profile: difference_worker_profile(
                metrics_before.yt_dlp_worker_profile,
                metrics_after.yt_dlp_worker_profile,
            ),
            ..TrackProbeTimings::default()
        };
        match resolved {
            Ok(request) => {
                let prepare_started_at = Instant::now();
                let metrics_before_prepare = operational_metrics().snapshot();
                let prepared_result = resolver.prepare_playback(&request).await;
                let metrics_after_prepare = operational_metrics().snapshot();
                timings.yt_dlp_process = add_durations(
                    timings.yt_dlp_process,
                    latency_delta(
                        metrics_before_prepare.yt_dlp_process_latency,
                        metrics_after_prepare.yt_dlp_process_latency,
                    ),
                );
                timings.yt_dlp_process_count += process_count_delta(
                    metrics_before_prepare.yt_dlp_process_latency,
                    metrics_after_prepare.yt_dlp_process_latency,
                );
                timings.worker_startup = add_durations(
                    timings.worker_startup,
                    latency_delta(
                        metrics_before_prepare.yt_dlp_worker_startup_latency,
                        metrics_after_prepare.yt_dlp_worker_startup_latency,
                    ),
                );
                timings.worker_ipc = add_durations(
                    timings.worker_ipc,
                    latency_delta(
                        metrics_before_prepare.yt_dlp_worker_ipc_latency,
                        metrics_after_prepare.yt_dlp_worker_ipc_latency,
                    ),
                );
                timings.worker_api_extract = add_durations(
                    timings.worker_api_extract,
                    latency_delta(
                        metrics_before_prepare.yt_dlp_worker_api_latency,
                        metrics_after_prepare.yt_dlp_worker_api_latency,
                    ),
                );
                timings.worker_import = add_durations(
                    timings.worker_import,
                    latency_delta(
                        metrics_before_prepare.yt_dlp_worker_import_latency,
                        metrics_after_prepare.yt_dlp_worker_import_latency,
                    ),
                );
                timings.worker_startup_count += process_count_delta(
                    metrics_before_prepare.yt_dlp_worker_startup_latency,
                    metrics_after_prepare.yt_dlp_worker_startup_latency,
                );
                timings.worker_ipc_count += process_count_delta(
                    metrics_before_prepare.yt_dlp_worker_ipc_latency,
                    metrics_after_prepare.yt_dlp_worker_ipc_latency,
                );
                timings.worker_api_count += process_count_delta(
                    metrics_before_prepare.yt_dlp_worker_api_latency,
                    metrics_after_prepare.yt_dlp_worker_api_latency,
                );
                timings.format_fallback_count += metrics_after_prepare
                    .yt_dlp_format_fallbacks
                    .saturating_sub(metrics_before_prepare.yt_dlp_format_fallbacks);
                timings.cli_fallback_count += metrics_after_prepare
                    .yt_dlp_worker_cli_fallbacks
                    .saturating_sub(metrics_before_prepare.yt_dlp_worker_cli_fallbacks);
                timings.worker_profile = add_worker_profiles(
                    timings.worker_profile,
                    difference_worker_profile(
                        metrics_before_prepare.yt_dlp_worker_profile,
                        metrics_after_prepare.yt_dlp_worker_profile,
                    ),
                );
                let prepared = match prepared_result {
                    Ok(prepared) => {
                        timings.prepare_playback = Some(prepare_started_at.elapsed());
                        prepared
                    }
                    Err(error) => {
                        timings.prepare_playback = Some(prepare_started_at.elapsed());
                        eprintln!("PROVIDER_ERROR\t{index}\tprepare_playback\tfailed");
                        println!("RESOLVED\t{index}\terror");
                        probe_failed = true;
                        print_timings(index, timings, started_at.elapsed());
                        if !options.strict {
                            return Err(Box::new(error).into());
                        }
                        continue;
                    }
                };
                println!(
                    "RESOLVED\t{index}\t{}\t{}\t{}",
                    prepared.provider_id, prepared.canonical_key, prepared.metadata.title
                );
                let playable_started_at = Instant::now();
                match runtime.verify_track(&prepared).await {
                    Ok(()) => println!("PLAYABLE\t{index}\tok"),
                    Err(error) => {
                        let failure = safe_playable_failure(&error);
                        eprintln!("PROVIDER_ERROR\t{index}\tverify_track\t{failure}");
                        println!("PLAYABLE\t{index}\terror\t{failure}");
                        probe_failed = true;
                    }
                }
                timings.songbird_make_playable = Some(playable_started_at.elapsed());
                print_timings(index, timings, started_at.elapsed());
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
            Err(_) => {
                eprintln!("PROVIDER_ERROR\t{index}\tresolve\tfailed");
                println!("RESOLVED\t{index}\terror");
                probe_failed = true;
                if automix_requested {
                    prepared_tracks.push(PreparedProbe {
                        index,
                        request: None,
                        analysis: None,
                    });
                }
                print_timings(index, timings, started_at.elapsed());
            }
        }
    }

    if automix_requested {
        print_automix_plans(&prepared_tracks, loudness_max_boost_db);
    }
    if let Some(path) = options.automix_preview.as_ref() {
        render_preview(&runtime, &prepared_tracks, path, loudness_max_boost_db).await?;
    }

    if options.strict && probe_failed {
        return Err(probe_error("strict YouTube/media probe failed"));
    }

    Ok(())
}

fn safe_playable_failure(error: &SongbirdRuntimeError) -> String {
    match error {
        SongbirdRuntimeError::HttpStatus { status } => format!("http_status_{status}"),
        SongbirdRuntimeError::MakePlayable(_) => "make_playable_failed".to_owned(),
        SongbirdRuntimeError::TrackReady(_) => "track_not_ready".to_owned(),
        _ => "playback_unavailable".to_owned(),
    }
}

struct ParallelProbeResult {
    index: usize,
    youtube_extract: Duration,
    prepare_playback: Duration,
    songbird_make_playable: Duration,
    total: Duration,
    failure: Option<&'static str>,
}

async fn run_parallel_probe(
    resolver: Arc<MediaResolver>,
    runtime: Arc<SongbirdRuntime>,
    parallelism: usize,
    strict: bool,
    urls: Vec<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let metrics_before = operational_metrics().snapshot();
    let mut pending = urls.into_iter().enumerate();
    let mut tasks = tokio::task::JoinSet::new();
    let mut outcomes = Vec::new();

    for _ in 0..parallelism {
        let Some((index, url)) = pending.next() else {
            break;
        };
        let resolver = Arc::clone(&resolver);
        let runtime = Arc::clone(&runtime);
        tasks.spawn(async move { probe_one_parallel(index, url, resolver, runtime).await });
    }

    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(outcome) => outcomes.push(outcome),
            Err(_) => return Err(probe_error("parallel probe task failed")),
        }
        if let Some((index, url)) = pending.next() {
            let resolver = Arc::clone(&resolver);
            let runtime = Arc::clone(&runtime);
            tasks.spawn(async move { probe_one_parallel(index, url, resolver, runtime).await });
        }
    }

    outcomes.sort_by_key(|outcome| outcome.index);
    let mut failed = false;
    for outcome in outcomes {
        if let Some(reason) = outcome.failure {
            failed = true;
            println!("RESOLVED\t{}\terror\t{reason}", outcome.index);
        } else {
            println!("PLAYABLE\t{}\tok", outcome.index);
        }
        println!(
            "TIMING_MS\t{}\tyoutube_extract_ms={:.2}\tprepare_playback_ms={:.2}\tsongbird_make_playable_ms={:.2}\ttotal_ms={:.2}",
            outcome.index,
            outcome.youtube_extract.as_secs_f64() * 1000.0,
            outcome.prepare_playback.as_secs_f64() * 1000.0,
            outcome.songbird_make_playable.as_secs_f64() * 1000.0,
            outcome.total.as_secs_f64() * 1000.0,
        );
    }

    let metrics_after = operational_metrics().snapshot();
    print_worker_profile(
        "all",
        difference_worker_profile(
            metrics_before.yt_dlp_worker_profile,
            metrics_after.yt_dlp_worker_profile,
        ),
    );
    println!(
        "RUN_SUMMARY\tparallelism={parallelism}\tworker_cli_fallbacks={}",
        metrics_after
            .yt_dlp_worker_cli_fallbacks
            .saturating_sub(metrics_before.yt_dlp_worker_cli_fallbacks),
    );
    if strict && failed {
        return Err(probe_error("strict parallel media probe failed"));
    }
    Ok(())
}

async fn probe_one_parallel(
    index: usize,
    url: String,
    resolver: Arc<MediaResolver>,
    runtime: Arc<SongbirdRuntime>,
) -> ParallelProbeResult {
    let started = Instant::now();
    let resolve_started = Instant::now();
    let resolved = resolver.resolve(&url).await;
    let youtube_extract = resolve_started.elapsed();
    let mut prepare_playback = Duration::ZERO;
    let mut songbird_make_playable = Duration::ZERO;
    let failure = match resolved {
        Ok(request) => {
            let prepare_started = Instant::now();
            let prepared = resolver.prepare_playback(&request).await;
            prepare_playback = prepare_started.elapsed();
            match prepared {
                Ok(prepared) => {
                    let playable_started = Instant::now();
                    let result = runtime.verify_track(&prepared).await;
                    songbird_make_playable = playable_started.elapsed();
                    result
                        .err()
                        .as_ref()
                        .map(|error| match safe_playable_failure(error).as_str() {
                            "http_status_403" => "http_status_403",
                            "http_status_404" => "http_status_404",
                            "http_status_410" => "http_status_410",
                            _ => "make_playable_failed",
                        })
                }
                Err(_) => Some("prepare_failed"),
            }
        }
        Err(_) => Some("resolve_failed"),
    };
    ParallelProbeResult {
        index,
        youtube_extract,
        prepare_playback,
        songbird_make_playable,
        total: started.elapsed(),
        failure,
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct TrackProbeTimings {
    youtube_extract: Option<Duration>,
    yt_dlp_process: Option<Duration>,
    yt_dlp_process_count: u64,
    worker_startup: Option<Duration>,
    worker_ipc: Option<Duration>,
    worker_api_extract: Option<Duration>,
    worker_import: Option<Duration>,
    worker_startup_count: u64,
    worker_ipc_count: u64,
    worker_api_count: u64,
    format_fallback_count: u64,
    cli_fallback_count: u64,
    worker_profile: YtDlpWorkerProfile,
    prepare_playback: Option<Duration>,
    songbird_make_playable: Option<Duration>,
}

fn process_count_delta(
    before: wotoha_core::operational_metrics::LatencySnapshot,
    after: wotoha_core::operational_metrics::LatencySnapshot,
) -> u64 {
    after.count.saturating_sub(before.count)
}

fn latency_delta(
    before: wotoha_core::operational_metrics::LatencySnapshot,
    after: wotoha_core::operational_metrics::LatencySnapshot,
) -> Option<Duration> {
    let count = after.count.saturating_sub(before.count);
    (count > 0).then(|| Duration::from_micros(after.sum_micros.saturating_sub(before.sum_micros)))
}

fn add_durations(left: Option<Duration>, right: Option<Duration>) -> Option<Duration> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (Some(duration), None) | (None, Some(duration)) => Some(duration),
        (None, None) => None,
    }
}

fn print_timings(index: usize, timings: TrackProbeTimings, total: Duration) {
    fn format_ms(duration: Option<Duration>) -> String {
        duration.map_or_else(
            || "n/a".to_owned(),
            |duration| format!("{:.2}", duration.as_secs_f64() * 1000.0),
        )
    }

    println!(
        "TIMING_MS\t{index}\tyoutube_extract_ms={}\tyt_dlp_process_ms={}\tyt_dlp_process_count={}\tpython_worker_startup_ms={}\tpython_worker_startup_count={}\tpython_worker_import_ms={}\tpython_worker_ipc_ms={}\tpython_worker_ipc_count={}\tpython_api_extract_ms={}\tpython_api_extract_count={}\tformat_fallback_count={}\tcli_fallback_count={}\tprepare_playback_ms={}\tsongbird_make_playable_ms={}\ttotal_ms={:.2}",
        format_ms(timings.youtube_extract),
        format_ms(timings.yt_dlp_process),
        timings.yt_dlp_process_count,
        format_ms(timings.worker_startup),
        timings.worker_startup_count,
        format_ms(timings.worker_import),
        format_ms(timings.worker_ipc),
        timings.worker_ipc_count,
        format_ms(timings.worker_api_extract),
        timings.worker_api_count,
        timings.format_fallback_count,
        timings.cli_fallback_count,
        format_ms(timings.prepare_playback),
        format_ms(timings.songbird_make_playable),
        total.as_secs_f64() * 1000.0,
    );
    print_worker_profile(index, timings.worker_profile);
    // Keep the pre-existing probe field available to scripts that consume it.
    println!("ELAPSED_MS\t{index}\t{:.2}", total.as_secs_f64() * 1000.0);
}

fn difference_worker_profile(
    before: YtDlpWorkerProfile,
    after: YtDlpWorkerProfile,
) -> YtDlpWorkerProfile {
    macro_rules! difference {
        ($($field:ident),+ $(,)?) => { $(let $field = after.$field.saturating_sub(before.$field);)+ };
    }
    difference!(
        extraction_count,
        requests_handler_workers,
        urllib_handler_workers,
        http_requests,
        http_open_micros,
        http_body_read_micros,
        dns_calls,
        dns_micros,
        tcp_connect_calls,
        tcp_connect_micros,
        tls_handshakes,
        tls_micros,
        js_solver_calls,
        js_solver_micros,
        js_runtime_calls,
        js_runtime_micros,
        cpu_user_micros,
        cpu_system_micros,
        child_cpu_user_micros,
        child_cpu_system_micros,
        youtube_requests,
        media_cdn_requests,
        other_requests,
    );
    YtDlpWorkerProfile {
        extraction_count,
        requests_handler_workers,
        urllib_handler_workers,
        http_requests,
        http_open_micros,
        http_body_read_micros,
        dns_calls,
        dns_micros,
        tcp_connect_calls,
        tcp_connect_micros,
        tls_handshakes,
        tls_micros,
        js_solver_calls,
        js_solver_micros,
        js_runtime_calls,
        js_runtime_micros,
        cpu_user_micros,
        cpu_system_micros,
        child_cpu_user_micros,
        child_cpu_system_micros,
        rss_highwater_kb: after.rss_highwater_kb,
        child_rss_highwater_kb: after.child_rss_highwater_kb,
        youtube_requests,
        media_cdn_requests,
        other_requests,
        http_request_detail_count: after.http_request_detail_count,
        http_request_details: after.http_request_details,
    }
}

fn add_worker_profiles(left: YtDlpWorkerProfile, right: YtDlpWorkerProfile) -> YtDlpWorkerProfile {
    macro_rules! add {
        ($($field:ident),+ $(,)?) => { $(let $field = left.$field.saturating_add(right.$field);)+ };
    }
    add!(
        extraction_count,
        requests_handler_workers,
        urllib_handler_workers,
        http_requests,
        http_open_micros,
        http_body_read_micros,
        dns_calls,
        dns_micros,
        tcp_connect_calls,
        tcp_connect_micros,
        tls_handshakes,
        tls_micros,
        js_solver_calls,
        js_solver_micros,
        js_runtime_calls,
        js_runtime_micros,
        cpu_user_micros,
        cpu_system_micros,
        child_cpu_user_micros,
        child_cpu_system_micros,
        youtube_requests,
        media_cdn_requests,
        other_requests,
    );
    YtDlpWorkerProfile {
        extraction_count,
        requests_handler_workers,
        urllib_handler_workers,
        http_requests,
        http_open_micros,
        http_body_read_micros,
        dns_calls,
        dns_micros,
        tcp_connect_calls,
        tcp_connect_micros,
        tls_handshakes,
        tls_micros,
        js_solver_calls,
        js_solver_micros,
        js_runtime_calls,
        js_runtime_micros,
        cpu_user_micros,
        cpu_system_micros,
        child_cpu_user_micros,
        child_cpu_system_micros,
        rss_highwater_kb: left.rss_highwater_kb.max(right.rss_highwater_kb),
        child_rss_highwater_kb: left
            .child_rss_highwater_kb
            .max(right.child_rss_highwater_kb),
        youtube_requests,
        media_cdn_requests,
        other_requests,
        http_request_detail_count: if right.http_request_detail_count > 0 {
            right.http_request_detail_count
        } else {
            left.http_request_detail_count
        },
        http_request_details: if right.http_request_detail_count > 0 {
            right.http_request_details
        } else {
            left.http_request_details
        },
    }
}

fn print_worker_profile(index: impl std::fmt::Display, profile: YtDlpWorkerProfile) {
    if profile.extraction_count == 0 {
        return;
    }
    let ms = |micros: u64| micros as f64 / 1000.0;
    println!(
        "YTDLP_PROFILE\t{index}\textractions={}\trequests_workers={}\turllib_workers={}\thttp_requests={}\thttp_open_ms={:.2}\thttp_body_ms={:.2}\tdns_calls={}\tdns_ms={:.2}\ttcp_connect_calls={}\ttcp_connect_ms={:.2}\ttls_handshakes={}\ttls_ms={:.2}\tjs_solver_calls={}\tjs_solver_ms={:.2}\tjs_runtime_calls={}\tjs_runtime_ms={:.2}\tcpu_user_ms={:.2}\tcpu_system_ms={:.2}\tchild_cpu_user_ms={:.2}\tchild_cpu_system_ms={:.2}\trss_highwater_kb={}\tchild_rss_highwater_kb={}\tyoutube_requests={}\tmedia_cdn_requests={}\tother_requests={}",
        profile.extraction_count,
        profile.requests_handler_workers,
        profile.urllib_handler_workers,
        profile.http_requests,
        ms(profile.http_open_micros),
        ms(profile.http_body_read_micros),
        profile.dns_calls,
        ms(profile.dns_micros),
        profile.tcp_connect_calls,
        ms(profile.tcp_connect_micros),
        profile.tls_handshakes,
        ms(profile.tls_micros),
        profile.js_solver_calls,
        ms(profile.js_solver_micros),
        profile.js_runtime_calls,
        ms(profile.js_runtime_micros),
        ms(profile.cpu_user_micros),
        ms(profile.cpu_system_micros),
        ms(profile.child_cpu_user_micros),
        ms(profile.child_cpu_system_micros),
        profile.rss_highwater_kb,
        profile.child_rss_highwater_kb,
        profile.youtube_requests,
        profile.media_cdn_requests,
        profile.other_requests,
    );
    for (ordinal, detail) in profile
        .http_request_details
        .iter()
        .take(profile.http_request_detail_count as usize)
        .enumerate()
    {
        println!(
            "YTDLP_HTTP_REQUEST\t{index}\tordinal={}\tkind={}\tstatus={}\topen_ms={:.3}\tbody_ms={:.3}",
            ordinal + 1,
            detail.kind.as_str(),
            detail.status_code,
            ms(detail.open_micros),
            ms(detail.body_read_micros),
        );
    }
}

struct PreparedProbe {
    index: usize,
    request: Option<TrackRequest>,
    analysis: Option<TrackAnalysis>,
}

struct ProbeOptions {
    strict: bool,
    warmup: bool,
    automix_plan: bool,
    automix_preview: Option<PathBuf>,
    loudness_max_boost_db: f32,
    parallelism: usize,
    urls: Vec<String>,
}

#[cfg(test)]
mod timing_tests {
    use super::*;
    use wotoha_core::operational_metrics::LatencySnapshot;

    #[test]
    fn process_latency_delta_is_per_probe_and_uses_microseconds() {
        let before = LatencySnapshot {
            count: 4,
            sum_micros: 10_000,
            max_micros: 5_000,
        };
        let after = LatencySnapshot {
            count: 5,
            sum_micros: 2_010_000,
            max_micros: 2_000_000,
        };
        assert_eq!(latency_delta(before, after), Some(Duration::from_secs(2)));
        assert_eq!(process_count_delta(before, after), 1);
    }

    #[test]
    fn process_latency_delta_is_unavailable_when_no_process_was_spawned() {
        let snapshot = LatencySnapshot::default();
        assert_eq!(latency_delta(snapshot, snapshot), None);
        assert_eq!(process_count_delta(snapshot, snapshot), 0);
    }

    #[test]
    fn process_durations_can_be_combined_across_resolve_and_prepare() {
        assert_eq!(
            add_durations(
                Some(Duration::from_millis(20)),
                Some(Duration::from_millis(5))
            ),
            Some(Duration::from_millis(25))
        );
        assert_eq!(
            add_durations(None, Some(Duration::from_millis(5))),
            Some(Duration::from_millis(5))
        );
        assert_eq!(add_durations(None, None), None);
    }
}

impl ProbeOptions {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut strict = false;
        let mut warmup = false;
        let mut automix_plan = false;
        let mut automix_preview = None;
        let mut loudness_max_boost_db = LoudnessConfig::production_default().max_boost_db;
        let mut parallelism = 1usize;
        let mut urls = Vec::new();
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--strict" => strict = true,
                "--warmup" => warmup = true,
                "--parallelism" => {
                    let Some(value) = args.next() else {
                        return Err("--parallelism requires a value".to_owned());
                    };
                    parallelism = value
                        .parse::<usize>()
                        .map_err(|_| "--parallelism must be between 1 and 8".to_owned())?;
                    if !(1..=8).contains(&parallelism) {
                        return Err("--parallelism must be between 1 and 8".to_owned());
                    }
                }
                "--automix-plan" => automix_plan = true,
                "--automix-preview" => {
                    let Some(path) = args.next() else {
                        return Err("--automix-preview requires a file path".to_owned());
                    };
                    automix_preview = Some(PathBuf::from(path));
                }
                "--loudness-max-boost-db" => {
                    let Some(value) = args.next() else {
                        return Err("--loudness-max-boost-db requires a value".to_owned());
                    };
                    loudness_max_boost_db = value
                        .parse::<f32>()
                        .map_err(|_| "--loudness-max-boost-db must be finite".to_owned())?;
                    if !loudness_max_boost_db.is_finite()
                        || !(0.0..=12.0).contains(&loudness_max_boost_db)
                    {
                        return Err(
                            "--loudness-max-boost-db must be between 0 and 12 dB".to_owned()
                        );
                    }
                }
                value if value.starts_with("--") => {
                    return Err(format!("unknown option: {value}"));
                }
                _ => urls.push(arg),
            }
        }

        Ok(Self {
            strict,
            warmup,
            automix_plan,
            automix_preview,
            loudness_max_boost_db,
            parallelism,
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
        "ANALYSIS\t{index}\tok\tduration_ms={}\taudible_start_ms={}\taudible_end_ms={}\tbpm={}\tbeat_confidence={:.3}\tkick_coverage={:.3}\tintro_ms={}\toutro_ms={}\trms_dbfs={}\tpeak_dbfs={}\tintegrated_lufs={}\ttrue_peak_dbtp={}",
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
        format_optional_f32(analysis.integrated_lufs),
        format_optional_f32(analysis.true_peak_dbtp),
    );
}

fn print_automix_plans(tracks: &[PreparedProbe], loudness_max_boost_db: f32) {
    let config = automix_probe_config();
    let loudness = loudness_probe_config(loudness_max_boost_db);
    println!(
        "AUTOMIX_CONFIG\tloudness_enabled={}\ttarget_lufs={}\tmax_boost_db={}\ttrue_peak_ceiling_dbtp={}\tmaster_volume_assumption=1.0\ttrue_peak_scope=normalization-stage\tsource_commit={}",
        loudness.enabled,
        loudness.target_lufs,
        loudness.max_boost_db,
        loudness.true_peak_ceiling_dbtp,
        option_env!("WOTOHA_SOURCE_COMMIT").unwrap_or("unknown"),
    );
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
        let outgoing_normalization_gain =
            loudness_normalization_gain(&loudness, Some(outgoing_analysis));
        let incoming_normalization_gain =
            loudness_normalization_gain(&loudness, Some(incoming_analysis));
        let guarded = plan_guarded_transition_with_base_gains(
            outgoing_analysis,
            incoming_analysis,
            &config,
            outgoing_normalization_gain,
            incoming_normalization_gain,
        );
        let plan = &guarded.plan;
        let quality = &guarded.quality;
        let score_breakdown = transition_score_breakdown(quality);
        println!(
            "AUTOMIX_PLAN\t{}\t{}\tok\toutgoing_key={}\tincoming_key={}\tguarded={}\trejected_kind={}\trejected_quality_issues={}\tkind={:?}\toutgoing_start_ms={}\tincoming_start_ms={}\tincoming_cue_selected={}\tincoming_cue_default_start_ms={}\tincoming_cue_candidates_checked={}\tfade_ms={}\ttempo_ratio={:.6}\ttempo_end_ratio={:.6}\toutgoing_normalization_gain={:.3}\tincoming_normalization_gain={:.3}\tincoming_gain={:.3}\tquality_ok={}\tquality_issues={:?}\ttransition_score={}\tscore_energy_balance_penalty={}\tscore_vocal_penalty={}\tscore_short_mix_penalty={}\tscore_energy_step_penalty={}\tscore_handoff_energy_penalty={}\tscore_handoff_ownership_penalty={}\tscore_tempo_smoothness_penalty={}\tscore_phrase_strength_penalty={}\tscore_structure_usage_penalty={}\tscore_harmonic_overlap_penalty={}\tharmonic_compatibility={}\tbeat_pairs_checked={}\tmax_beat_phase_error_ms={}\thandoff_beat_phase_error_ms={}\tdownbeat_pairs_checked={}\tmax_downbeat_phase_error_ms={}\thandoff_downbeat_phase_error_ms={}\tphrase_pairs_checked={}\tmax_phrase_phase_error_ms={}\thandoff_phrase_phase_error_ms={}\tlow_handoff_min={}\tlow_handoff_max={}\tvocal_overlap_samples_checked={}\tmax_dual_vocal_risk={}\tenergy_samples_checked={}\tmin_mix_energy_ratio={}\tmax_mix_energy_ratio={}",
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
            plan.incoming_cue_selection
                .is_some_and(|selection| selection.selected_start != selection.default_start),
            format_optional_duration_ms(
                plan.incoming_cue_selection
                    .map(|selection| selection.default_start),
            ),
            plan.incoming_cue_selection.map_or_else(
                || "-".to_owned(),
                |selection| selection.candidates_checked.to_string()
            ),
            plan.duration.as_millis(),
            plan.incoming_tempo_ratio,
            plan.tempo_envelope
                .map_or(plan.incoming_tempo_ratio, |envelope| envelope.mix_end_speed),
            outgoing_normalization_gain,
            incoming_normalization_gain,
            plan.incoming_gain,
            quality.is_ok(),
            quality.issues,
            format_optional_f32(score_breakdown.map(|breakdown| breakdown.total)),
            format_optional_f32(score_breakdown.map(|breakdown| breakdown.energy_balance_penalty)),
            format_optional_f32(score_breakdown.map(|breakdown| breakdown.vocal_penalty)),
            format_optional_f32(score_breakdown.map(|breakdown| breakdown.short_mix_penalty)),
            format_optional_f32(score_breakdown.map(|breakdown| breakdown.energy_step_penalty)),
            format_optional_f32(score_breakdown.map(|breakdown| breakdown.handoff_energy_penalty)),
            format_optional_f32(
                score_breakdown.map(|breakdown| breakdown.handoff_ownership_penalty),
            ),
            format_optional_f32(
                score_breakdown.map(|breakdown| breakdown.tempo_smoothness_penalty)
            ),
            format_optional_f32(score_breakdown.map(|breakdown| breakdown.phrase_strength_penalty)),
            format_optional_f32(score_breakdown.map(|breakdown| breakdown.structure_usage_penalty)),
            format_optional_f32(
                score_breakdown.map(|breakdown| breakdown.harmonic_overlap_penalty)
            ),
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
    loudness_max_boost_db: f32,
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
            &loudness_probe_config(loudness_max_boost_db),
        )
        .await?;
    let bytes = preview.wav.len();
    std::fs::write(path, &preview.wav)?;
    println!(
        "AUTOMIX_PREVIEW\t{}\t{}\tok\tpath={}\tsample_rate={}\tchannels={}\tbytes={}\tkind={:?}\tquality_ok={}\trender_ok={}\trender_issues={:?}\toutgoing_normalization_gain={:.3}\tincoming_normalization_gain={:.3}\tstart_rms_dbfs={:.2}\tmid_rms_dbfs={:.2}\tend_rms_dbfs={:.2}\tquietest_window_rms_dbfs={:.2}\tquietest_to_edge_ratio={:.3}\tmid_to_edge_ratio={:.3}\tsample_peak_dbfs={:.2}",
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
        preview.outgoing_normalization_gain,
        preview.incoming_normalization_gain,
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

fn loudness_probe_config(max_boost_db: f32) -> LoudnessConfig {
    LoudnessConfig {
        max_boost_db,
        ..LoudnessConfig::production_default()
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
        assert!(!options.strict);
        assert!(options.automix_plan);
        assert_eq!(options.loudness_max_boost_db, 6.0);
        assert_eq!(options.automix_preview, None);
        assert_eq!(options.urls, ["one", "two"]);
    }

    #[test]
    fn parses_strict_option() {
        let options = ProbeOptions::parse([
            "--strict".to_owned(),
            "https://www.youtube.com/watch?v=H7HmzwI67ec".to_owned(),
        ])
        .unwrap();

        assert!(options.strict);
        assert_eq!(
            options.urls,
            ["https://www.youtube.com/watch?v=H7HmzwI67ec"]
        );
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

    #[test]
    fn production_loudness_boost_is_the_cli_default() {
        let options = ProbeOptions::parse(["one".to_owned()]).unwrap();

        assert_eq!(options.loudness_max_boost_db, 6.0);
    }

    #[test]
    fn explicit_loudness_boost_override_is_preserved() {
        let options = ProbeOptions::parse([
            "--loudness-max-boost-db".to_owned(),
            "6".to_owned(),
            "one".to_owned(),
        ])
        .unwrap();

        assert_eq!(options.loudness_max_boost_db, 6.0);
        assert_eq!(
            loudness_probe_config(options.loudness_max_boost_db).max_boost_db,
            6.0
        );
    }
}
