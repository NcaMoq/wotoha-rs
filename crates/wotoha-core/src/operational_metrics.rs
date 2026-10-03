//! Low-cardinality operational counters shared by the runtime crates.
//!
//! This intentionally is not a metrics exporter.  The process keeps only
//! aggregate atomics and count/sum/max latency accumulators, which can be
//! emitted in one structured snapshot without retaining URLs, user IDs, or
//! guild IDs.

use std::{
    sync::{
        OnceLock,
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
}
