//! Low-cardinality operational counters shared by the runtime crates.
//!
//! This intentionally is not a metrics exporter.  The process keeps only
//! aggregate atomics and count/sum/max latency accumulators, which can be
//! emitted in one structured snapshot without retaining URLs, user IDs, or
//! guild IDs.

use std::{
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
pub struct LatencyAccumulator {
    count: AtomicU64,
    sum_micros: AtomicU64,
    max_micros: AtomicU64,
}

impl LatencyAccumulator {
    pub fn observe(&self, elapsed: Duration) {
        let micros = elapsed.as_micros().min(u64::MAX as u128) as u64;
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_micros
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(micros))
            })
            .ok();
        self.max_micros.fetch_max(micros, Ordering::Relaxed);
    }

    fn snapshot(&self) -> LatencySnapshot {
        LatencySnapshot {
            count: self.count.load(Ordering::Relaxed),
            sum_micros: self.sum_micros.load(Ordering::Relaxed),
            max_micros: self.max_micros.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LatencySnapshot {
    pub count: u64,
    pub sum_micros: u64,
    pub max_micros: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct YtDlpWorkerProfile {
    pub extraction_count: u64,
    pub requests_handler_workers: u64,
    pub urllib_handler_workers: u64,
    pub http_requests: u64,
    pub http_open_micros: u64,
    pub http_body_read_micros: u64,
    pub dns_calls: u64,
    pub dns_micros: u64,
    pub tcp_connect_calls: u64,
    pub tcp_connect_micros: u64,
    pub tls_handshakes: u64,
    pub tls_micros: u64,
    pub js_solver_calls: u64,
    pub js_solver_micros: u64,
    pub js_runtime_calls: u64,
    pub js_runtime_micros: u64,
    pub cpu_user_micros: u64,
    pub cpu_system_micros: u64,
    pub child_cpu_user_micros: u64,
    pub child_cpu_system_micros: u64,
    pub rss_highwater_kb: u64,
    pub child_rss_highwater_kb: u64,
    pub youtube_requests: u64,
    pub media_cdn_requests: u64,
    pub other_requests: u64,
    pub http_request_detail_count: u8,
    pub http_request_details: [YtDlpHttpRequestDetail; MAX_YTDLP_HTTP_REQUEST_DETAILS],
}

pub const MAX_YTDLP_HTTP_REQUEST_DETAILS: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct YtDlpHttpRequestDetail {
    pub kind: YtDlpHttpRequestKind,
    pub status_code: u16,
    pub open_micros: u64,
    pub body_read_micros: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum YtDlpHttpRequestKind {
    #[default]
    Other,
    YouTubePlayerApi,
    YouTubeClientConfig,
    YouTubeWebpage,
    YouTubeInitialData,
    YouTubePlayerScript,
    MediaCdn,
}

impl YtDlpHttpRequestKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Other => "other",
            Self::YouTubePlayerApi => "youtube_player_api",
            Self::YouTubeClientConfig => "youtube_client_config",
            Self::YouTubeWebpage => "youtube_webpage",
            Self::YouTubeInitialData => "youtube_initial_data",
            Self::YouTubePlayerScript => "youtube_player_script",
            Self::MediaCdn => "media_cdn",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OperationalMetricsSnapshot {
    pub gateway_ready: u64,
    pub gateway_disconnected: u64,
    pub active_guilds: u64,
    pub active_voice_sessions: u64,
    pub analysis_running: u64,
    pub analysis_queued: u64,
    pub analysis_completed: u64,
    pub analysis_timeout: u64,
    pub cache_read_failures: u64,
    pub cache_write_failures: u64,
    pub loudness_cache_hits: u64,
    pub loudness_cache_misses: u64,
    pub normalized_at_start: u64,
    pub late_analysis_for_future: u64,
    pub playback_failures: u64,
    pub shutdown_signals: u64,
    pub interaction_ack_latency: LatencySnapshot,
    pub enqueue_latency: LatencySnapshot,
    pub media_resolve_latency: LatencySnapshot,
    pub yt_dlp_process_latency: LatencySnapshot,
    pub yt_dlp_worker_startup_latency: LatencySnapshot,
    pub yt_dlp_worker_ipc_latency: LatencySnapshot,
    pub yt_dlp_worker_api_latency: LatencySnapshot,
    pub yt_dlp_worker_import_latency: LatencySnapshot,
    pub yt_dlp_worker_profile: YtDlpWorkerProfile,
    pub yt_dlp_worker_cli_fallbacks: u64,
    pub yt_dlp_format_fallbacks: u64,
    pub playback_start_latency: LatencySnapshot,
    pub analysis_duration: LatencySnapshot,
}

#[derive(Default)]
pub struct OperationalMetrics {
    gateway_ready: AtomicU64,
    gateway_disconnected: AtomicU64,
    active_guilds: AtomicU64,
    active_voice_sessions: AtomicU64,
    analysis_running: AtomicU64,
    analysis_queued: AtomicU64,
    analysis_completed: AtomicU64,
    analysis_timeout: AtomicU64,
    cache_read_failures: AtomicU64,
    cache_write_failures: AtomicU64,
    loudness_cache_hits: AtomicU64,
    loudness_cache_misses: AtomicU64,
    normalized_at_start: AtomicU64,
    late_analysis_for_future: AtomicU64,
    playback_failures: AtomicU64,
    shutdown_signals: AtomicU64,
    interaction_ack_latency: LatencyAccumulator,
    enqueue_latency: LatencyAccumulator,
    media_resolve_latency: LatencyAccumulator,
    yt_dlp_process_latency: LatencyAccumulator,
    yt_dlp_worker_startup_latency: LatencyAccumulator,
    yt_dlp_worker_ipc_latency: LatencyAccumulator,
    yt_dlp_worker_api_latency: LatencyAccumulator,
    yt_dlp_worker_import_latency: LatencyAccumulator,
    yt_dlp_worker_profile: Mutex<YtDlpWorkerProfile>,
    yt_dlp_worker_cli_fallbacks: AtomicU64,
    yt_dlp_format_fallbacks: AtomicU64,
    playback_start_latency: LatencyAccumulator,
    analysis_duration: LatencyAccumulator,
}

impl OperationalMetrics {
    pub fn record_gateway_ready(&self) {
        self.gateway_ready.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_gateway_disconnected(&self) {
        self.gateway_disconnected.fetch_add(1, Ordering::Relaxed);
    }
    pub fn set_active_guilds(&self, count: usize) {
        self.active_guilds.store(count as u64, Ordering::Relaxed);
    }
    pub fn set_active_voice_sessions(&self, count: usize) {
        self.active_voice_sessions
            .store(count as u64, Ordering::Relaxed);
    }
    pub fn record_analysis_queued(&self) {
        self.analysis_queued.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_analysis_started(&self) {
        self.analysis_running.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_analysis_completed(&self, elapsed: Duration) {
        self.analysis_running.fetch_sub(1, Ordering::Relaxed);
        self.analysis_completed.fetch_add(1, Ordering::Relaxed);
        self.analysis_duration.observe(elapsed);
    }
    pub fn record_analysis_timeout(&self) {
        self.analysis_timeout.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_cache_read_failure(&self) {
        self.cache_read_failures.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_cache_write_failure(&self) {
        self.cache_write_failures.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_loudness_cache_hit(&self) {
        self.loudness_cache_hits.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_loudness_cache_miss(&self) {
        self.loudness_cache_misses.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_normalized_at_start(&self) {
        self.normalized_at_start.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_late_analysis_for_future(&self) {
        self.late_analysis_for_future
            .fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_playback_failure(&self) {
        self.playback_failures.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_shutdown_signal(&self) {
        self.shutdown_signals.fetch_add(1, Ordering::Relaxed);
    }
    pub fn observe_interaction_ack(&self, elapsed: Duration) {
        self.interaction_ack_latency.observe(elapsed);
    }
    pub fn observe_enqueue(&self, elapsed: Duration) {
        self.enqueue_latency.observe(elapsed);
    }
    pub fn observe_media_resolve(&self, elapsed: Duration) {
        self.media_resolve_latency.observe(elapsed);
    }
    pub fn observe_yt_dlp_process(&self, elapsed: Duration) {
        self.yt_dlp_process_latency.observe(elapsed);
    }
    pub fn observe_yt_dlp_worker_startup(&self, elapsed: Duration) {
        self.yt_dlp_worker_startup_latency.observe(elapsed);
    }
    pub fn observe_yt_dlp_worker_ipc(&self, elapsed: Duration) {
        self.yt_dlp_worker_ipc_latency.observe(elapsed);
    }
    pub fn observe_yt_dlp_worker_api(&self, elapsed: Duration) {
        self.yt_dlp_worker_api_latency.observe(elapsed);
    }
    pub fn observe_yt_dlp_worker_import(&self, elapsed: Duration) {
        self.yt_dlp_worker_import_latency.observe(elapsed);
    }
    pub fn record_yt_dlp_worker_handler(&self, name: &str) {
        let mut profile = self
            .yt_dlp_worker_profile
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match name {
            "requests" => profile.requests_handler_workers += 1,
            "urllib" => profile.urllib_handler_workers += 1,
            _ => {}
        }
    }
    pub fn record_yt_dlp_worker_profile(&self, observed: YtDlpWorkerProfile) {
        let mut profile = self
            .yt_dlp_worker_profile
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        macro_rules! add {
            ($($field:ident),+ $(,)?) => { $(profile.$field = profile.$field.saturating_add(observed.$field);)+ };
        }
        add!(
            extraction_count,
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
        if observed.http_request_detail_count > 0 {
            profile.http_request_detail_count = observed.http_request_detail_count;
            profile.http_request_details = observed.http_request_details;
        }
        profile.rss_highwater_kb = profile.rss_highwater_kb.max(observed.rss_highwater_kb);
        profile.child_rss_highwater_kb = profile
            .child_rss_highwater_kb
            .max(observed.child_rss_highwater_kb);
    }
    pub fn record_yt_dlp_format_fallback(&self) {
        self.yt_dlp_format_fallbacks.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_yt_dlp_worker_cli_fallback(&self) {
        self.yt_dlp_worker_cli_fallbacks
            .fetch_add(1, Ordering::Relaxed);
    }
    pub fn observe_playback_start(&self, elapsed: Duration) {
        self.playback_start_latency.observe(elapsed);
    }

    pub fn snapshot(&self) -> OperationalMetricsSnapshot {
        OperationalMetricsSnapshot {
            gateway_ready: self.gateway_ready.load(Ordering::Relaxed),
            gateway_disconnected: self.gateway_disconnected.load(Ordering::Relaxed),
            active_guilds: self.active_guilds.load(Ordering::Relaxed),
            active_voice_sessions: self.active_voice_sessions.load(Ordering::Relaxed),
            analysis_running: self.analysis_running.load(Ordering::Relaxed),
            analysis_queued: self.analysis_queued.load(Ordering::Relaxed),
            analysis_completed: self.analysis_completed.load(Ordering::Relaxed),
            analysis_timeout: self.analysis_timeout.load(Ordering::Relaxed),
            cache_read_failures: self.cache_read_failures.load(Ordering::Relaxed),
            cache_write_failures: self.cache_write_failures.load(Ordering::Relaxed),
            loudness_cache_hits: self.loudness_cache_hits.load(Ordering::Relaxed),
            loudness_cache_misses: self.loudness_cache_misses.load(Ordering::Relaxed),
            normalized_at_start: self.normalized_at_start.load(Ordering::Relaxed),
            late_analysis_for_future: self.late_analysis_for_future.load(Ordering::Relaxed),
            playback_failures: self.playback_failures.load(Ordering::Relaxed),
            shutdown_signals: self.shutdown_signals.load(Ordering::Relaxed),
            interaction_ack_latency: self.interaction_ack_latency.snapshot(),
            enqueue_latency: self.enqueue_latency.snapshot(),
            media_resolve_latency: self.media_resolve_latency.snapshot(),
            yt_dlp_process_latency: self.yt_dlp_process_latency.snapshot(),
            yt_dlp_worker_startup_latency: self.yt_dlp_worker_startup_latency.snapshot(),
            yt_dlp_worker_ipc_latency: self.yt_dlp_worker_ipc_latency.snapshot(),
            yt_dlp_worker_api_latency: self.yt_dlp_worker_api_latency.snapshot(),
            yt_dlp_worker_import_latency: self.yt_dlp_worker_import_latency.snapshot(),
            yt_dlp_worker_profile: *self
                .yt_dlp_worker_profile
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            yt_dlp_worker_cli_fallbacks: self.yt_dlp_worker_cli_fallbacks.load(Ordering::Relaxed),
            yt_dlp_format_fallbacks: self.yt_dlp_format_fallbacks.load(Ordering::Relaxed),
            playback_start_latency: self.playback_start_latency.snapshot(),
            analysis_duration: self.analysis_duration.snapshot(),
        }
    }
}

static GLOBAL_OPERATIONAL_METRICS: OnceLock<OperationalMetrics> = OnceLock::new();

pub fn operational_metrics() -> &'static OperationalMetrics {
    GLOBAL_OPERATIONAL_METRICS.get_or_init(OperationalMetrics::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_accumulator_is_bounded_to_count_sum_and_max() {
        let metrics = OperationalMetrics::default();
        metrics.observe_enqueue(Duration::from_millis(2));
        metrics.observe_enqueue(Duration::from_millis(5));
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.enqueue_latency.count, 2);
        assert_eq!(snapshot.enqueue_latency.sum_micros, 7_000);
        assert_eq!(snapshot.enqueue_latency.max_micros, 5_000);
    }

    #[test]
    fn yt_dlp_worker_profile_keeps_only_aggregate_counts_timings_and_peak_memory() {
        let metrics = OperationalMetrics::default();
        metrics.record_yt_dlp_worker_profile(YtDlpWorkerProfile {
            extraction_count: 1,
            http_requests: 3,
            http_open_micros: 900,
            tls_handshakes: 2,
            tls_micros: 700,
            cpu_user_micros: 1_200,
            rss_highwater_kb: 32_000,
            ..YtDlpWorkerProfile::default()
        });
        metrics.record_yt_dlp_worker_profile(YtDlpWorkerProfile {
            extraction_count: 1,
            http_requests: 2,
            http_open_micros: 500,
            tls_handshakes: 1,
            tls_micros: 300,
            cpu_user_micros: 600,
            rss_highwater_kb: 28_000,
            ..YtDlpWorkerProfile::default()
        });
        let snapshot = metrics.snapshot().yt_dlp_worker_profile;
        assert_eq!(snapshot.extraction_count, 2);
        assert_eq!(snapshot.http_requests, 5);
        assert_eq!(snapshot.http_open_micros, 1_400);
        assert_eq!(snapshot.tls_handshakes, 3);
        assert_eq!(snapshot.tls_micros, 1_000);
        assert_eq!(snapshot.cpu_user_micros, 1_800);
        assert_eq!(snapshot.rss_highwater_kb, 32_000);
    }
}
