use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
    redirect::Policy,
};
use serenity::all::{ChannelId, GuildId};
use songbird::{
    Songbird,
    constants::TIMESTEP_LENGTH,
    error::JoinError,
    events::{Event, EventContext, EventData, EventHandler as VoiceEventHandler, TrackEvent},
    input::{
        MakePlayableError,
        codecs::{get_codec_registry, get_probe},
    },
    tracks::{PlayMode, ReadyState, Track, TrackHandle},
};
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use wotoha_contracts::{
    ChannelKey, FrameScheduledTransitionSupport, GuildKey, PlaybackId, PlaybackRuntimeEvent,
    RuntimeEventSink, RuntimeTrackHandle, TrackEndReason, TrackStartOptions,
    TransitionArmFailureKind, TransitionArmResult, VoiceGatewayEvent, VoiceGatewayRuntime,
    VoiceRuntime,
};
use wotoha_core::{
    PreparedHeader, PreparedSource, TrackRequest,
    analysis::TrackAnalysisV2,
    automix::{
        AutoMixConfig, TrackAnalysis, V2GuardedTransitionPlan,
        plan_guarded_transition_v2_for_analysis,
    },
    config::LoudnessConfig,
    debug::append_debug_log,
    url::{is_allowed_prepared_url, summarize_url_for_logs},
};

use crate::{
    AnalysisCache, AnalysisCacheKey,
    audio_decode::{
        AnalysisBackend, AnalysisOutcome, MAX_ANALYSIS_DURATION, analyze_input_with_cancel_outcome,
    },
    automix_cache::{ANALYSIS_CACHE_ANALYZER_VERSION, ANALYSIS_CACHE_CLASSICAL_ANALYZER_VERSION},
    automix_preview::{
        AutoMixPreview, AutoMixPreviewError, render_automix_preview_inputs_with_cancel,
    },
    beat_this_analysis::{
        track_analysis_v2_from_legacy, track_analysis_v2_from_legacy_with_backend,
    },
    cancellable_http::CancellableHttpRequest,
    niconico_hls::NiconicoHlsRequest,
    ranged_http::RangedHttpRequest,
    tempo_stretch::{
        StretchTimeline, build_stretched_input_with_equalizer, build_trimmed_input_with_equalizer,
    },
    transition_dsp::{EqualizerControl, wrap_parsed_equalizer},
    validated_hls::ValidatedHlsRequest,
};

const STREAM_PROVIDER_IDS: [&str; 7] = [
    "youtube",
    "soundcloud",
    "bandcamp",
    "niconico",
    "vimeo",
    "twitch",
    "x",
];
const STREAM_FORCE_IPV4: bool = false;
const STREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(20);
const STREAM_TCP_KEEPALIVE: Duration = Duration::from_secs(30);
const STREAM_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const STREAM_HTTP2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);
const STREAM_HTTP2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_POOL_MAX_IDLE_PER_HOST: usize = 4;
const STREAM_REDIRECT_LIMIT: usize = 5;
const ANALYSIS_TIMEOUT: Duration = Duration::from_secs(90);
// A classical fallback is deliberately process-local and short-lived. This
// bounds retries after a transient model failure while allowing a newly
// available embedded model to upgrade the same source on the next attempt.
const CLASSICAL_FALLBACK_RETRY_TTL: Duration = Duration::from_secs(60);

struct CancelAnalysisOnDrop {
    cancelled: Arc<AtomicBool>,
    cancellation: CancellationToken,
}

struct CancelPreviewOnDrop {
    cancelled: Arc<AtomicBool>,
    cancellation: CancellationToken,
    armed: bool,
}

impl CancelPreviewOnDrop {
    fn new(cancelled: Arc<AtomicBool>, cancellation: CancellationToken) -> Self {
        Self {
            cancelled,
            cancellation,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelPreviewOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
            self.cancellation.cancel();
        }
    }
}

impl Drop for CancelAnalysisOnDrop {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.cancellation.cancel();
    }
}

#[derive(Clone)]
pub struct SongbirdRuntime {
    manager: Arc<Songbird>,
    stream_clients: Arc<HashMap<&'static str, Client>>,
    analysis_cache: Arc<AnalysisCache>,
    classical_cache: Arc<AnalysisCache>,
    classical_fallbacks: Arc<Mutex<HashMap<AnalysisCacheKey, ClassicalFallback>>>,
    analysis_limit: Arc<Semaphore>,
    tracks: Arc<Mutex<HashMap<TrackIdentity, Weak<SongbirdTrackHandle>>>>,
}

struct ClassicalFallback {
    stored_at: Instant,
    analysis: TrackAnalysis,
}

impl ClassicalFallback {
    fn is_fresh_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.stored_at) < CLASSICAL_FALLBACK_RETRY_TTL
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TrackIdentity {
    guild_id: GuildKey,
    session_id: u64,
    playback_id: PlaybackId,
}

struct RegisterTrackParams<'a> {
    guild_id: GuildKey,
    session_id: u64,
    playback_id: PlaybackId,
    request: &'a TrackRequest,
    events: RuntimeEventSink,
    options: TrackStartOptions,
    start_paused: bool,
}

#[derive(Debug, Error)]
pub enum SongbirdRuntimeError {
    #[error("failed to initialize stream HTTP client: {0}")]
    HttpClient(reqwest::Error),
    #[error("voice call is not connected")]
    MissingCall,
    #[error("failed to join voice channel: {0}")]
    Join(String),
    #[error("failed to build request header name: {0}")]
    InvalidHeaderName(String),
    #[error("failed to build request header value: {0}")]
    InvalidHeaderValue(String),
    #[error("failed to attach track end listener: {0}")]
    TrackEvent(String),
    #[error("failed to ready audio track before arming: {0}")]
    TrackReady(String),
    #[error("resolved source is not playable: {0}")]
    MakePlayable(String),
    #[error("failed to initialize tempo-matched playback: {0}")]
    TempoStretch(String),
    #[error("failed to remove voice call: {0}")]
    Disconnect(String),
    #[error("missing stream HTTP client for provider: {0}")]
    MissingStreamClient(String),
    #[error("resolved source URL is not allowed for provider {provider_id}: {url}")]
    DisallowedPreparedUrl { provider_id: String, url: String },
}

impl SongbirdRuntime {
    pub fn new(manager: Arc<Songbird>) -> Result<Self, SongbirdRuntimeError> {
        let stream_clients = STREAM_PROVIDER_IDS
            .into_iter()
            .map(|provider_id| build_stream_client(provider_id).map(|client| (provider_id, client)))
            .collect::<Result<HashMap<_, _>, _>>()?;

        let cache_root = std::env::var_os("WOTOHA_ANALYSIS_CACHE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ".wotoha-analysis".into());
        Ok(Self {
            manager,
            stream_clients: Arc::new(stream_clients),
            analysis_cache: Arc::new(
                AnalysisCache::new(cache_root.clone(), ANALYSIS_CACHE_ANALYZER_VERSION)
                    .expect("static analyzer version is valid"),
            ),
            classical_cache: Arc::new(
                AnalysisCache::new(
                    cache_root.join("classical"),
                    ANALYSIS_CACHE_CLASSICAL_ANALYZER_VERSION,
                )
                .expect("static analyzer version is valid"),
            ),
            classical_fallbacks: Arc::new(Mutex::new(HashMap::new())),
            analysis_limit: Arc::new(Semaphore::new(2)),
            tracks: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn ensure_joined(
        &self,
        guild_id: GuildKey,
        channel_id: ChannelKey,
    ) -> Result<bool, SongbirdRuntimeError> {
        append_debug_log(format!(
            "runtime: ensure_joined guild_id={} channel_id={}",
            guild_id.get(),
            channel_id.get()
        ));
        let guild = to_guild_id(guild_id);
        let channel = to_channel_id(channel_id);
        if let Some(call_lock) = self.manager.get(guild) {
            let call = call_lock.lock().await;
            let same_channel = call.current_channel() == Some(channel.into());
            let connected = call.current_connection().is_some();
            let deafened = call.is_deaf();
            drop(call);

            if same_channel && connected {
                append_debug_log(format!(
                    "runtime: ensure_joined already connected guild_id={} channel_id={}",
                    guild_id.get(),
                    channel_id.get()
                ));
                if !deafened {
                    let deafen_call = call_lock.clone();
                    tokio::spawn(async move {
                        let mut call = deafen_call.lock().await;
                        if let Err(error) = call.deafen(true).await {
                            warn!(
                                guild_id = guild_id.get(),
                                error = %error,
                                "failed to deafen bot after confirming existing voice connection"
                            );
                        }
                    });
                }
                return Ok(false);
            }

            if same_channel {
                append_debug_log(format!(
                    "runtime: ensure_joined already pending guild_id={} channel_id={}",
                    guild_id.get(),
                    channel_id.get()
                ));
                return Ok(false);
            }
        }

        let call_lock = self.manager.get_or_insert(guild);
        let join = {
            let mut call = call_lock.lock().await;
            call.join(channel)
                .await
                .map_err(|error| SongbirdRuntimeError::Join(error.to_string()))?
        };

        let join_call = call_lock.clone();
        let join_manager = self.manager.clone();
        tokio::spawn(async move {
            match join.await {
                Ok(()) => {
                    append_debug_log(format!(
                        "runtime: ensure_joined connected guild_id={} channel_id={}",
                        guild_id.get(),
                        channel_id.get()
                    ));
                    let mut call = join_call.lock().await;
                    if call.current_channel() == Some(channel.into())
                        && let Err(error) = call.deafen(true).await
                    {
                        warn!(
                            guild_id = guild_id.get(),
                            error = %error,
                            "failed to deafen bot after join"
                        );
                    }
                }
                Err(error) => {
                    warn!(
                        guild_id = guild_id.get(),
                        channel_id = channel_id.get(),
                        error = %error,
                        "failed to complete voice join"
                    );
                    let _ = join_manager.remove(guild).await;
                }
            }
        });

        append_debug_log(format!(
            "runtime: ensure_joined requested guild_id={} channel_id={}",
            guild_id.get(),
            channel_id.get()
        ));
        Ok(true)
    }

    pub async fn verify_track(&self, request: &TrackRequest) -> Result<(), SongbirdRuntimeError> {
        build_input(self.stream_client(request.provider_id.as_ref())?, request)?
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .map(|_| ())
            .map_err(make_playable_error)
    }

    pub async fn render_automix_preview(
        &self,
        outgoing: &TrackRequest,
        incoming: &TrackRequest,
        outgoing_analysis: &TrackAnalysis,
        incoming_analysis: &TrackAnalysis,
        config: &AutoMixConfig,
        loudness: &LoudnessConfig,
    ) -> Result<AutoMixPreview, AutoMixPreviewError> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = CancellationToken::new();
        let mut cancel_on_drop = CancelPreviewOnDrop::new(cancelled.clone(), cancellation.clone());

        let outgoing_input = build_input_with_cancellation(
            self.stream_client(outgoing.provider_id.as_ref())
                .map_err(|error| AutoMixPreviewError::Source(error.to_string()))?,
            outgoing,
            Some(cancellation.clone()),
        )
        .map_err(|error| AutoMixPreviewError::Source(error.to_string()))?
        .make_playable_async(get_codec_registry(), get_probe())
        .await
        .map_err(|error| AutoMixPreviewError::MakePlayable(error.to_string()))?;
        let incoming_input = build_input_with_cancellation(
            self.stream_client(incoming.provider_id.as_ref())
                .map_err(|error| AutoMixPreviewError::Source(error.to_string()))?,
            incoming,
            Some(cancellation.clone()),
        )
        .map_err(|error| AutoMixPreviewError::Source(error.to_string()))?
        .make_playable_async(get_codec_registry(), get_probe())
        .await
        .map_err(|error| AutoMixPreviewError::MakePlayable(error.to_string()))?;

        let worker_cancelled = cancelled.clone();
        let outgoing_analysis = outgoing_analysis.clone();
        let incoming_analysis = incoming_analysis.clone();
        let config = config.clone();
        let loudness = loudness.clone();
        let rendered = tokio::task::spawn_blocking(move || {
            render_automix_preview_inputs_with_cancel(
                outgoing_input,
                incoming_input,
                &outgoing_analysis,
                &incoming_analysis,
                &config,
                &loudness,
                &worker_cancelled,
            )
        })
        .await
        .map_err(|error| {
            AutoMixPreviewError::Source(format!("preview worker failed: {error}"))
        })??;
        cancellation.cancel();
        cancel_on_drop.disarm();
        Ok(rendered)
    }

    pub fn paired() -> Result<(Self, Arc<Songbird>), SongbirdRuntimeError> {
        let songbird = Songbird::serenity();
        let runtime = Self::new(songbird.clone())?;
        Ok((runtime, songbird))
    }

    fn stream_client(&self, provider_id: &str) -> Result<&Client, SongbirdRuntimeError> {
        self.stream_clients
            .get(provider_id)
            .ok_or_else(|| SongbirdRuntimeError::MissingStreamClient(provider_id.to_owned()))
    }

    async fn register_track(
        &self,
        params: RegisterTrackParams<'_>,
    ) -> Result<Arc<dyn RuntimeTrackHandle>, SongbirdRuntimeError> {
        let RegisterTrackParams {
            guild_id,
            session_id,
            playback_id,
            request,
            events,
            options,
            start_paused,
        } = params;
        append_debug_log(format!(
            "runtime: play_track start guild_id={} session_id={} playback_id={} provider={} key={} title={} paused={}",
            guild_id.get(),
            session_id,
            playback_id.get(),
            request.provider_id.as_ref(),
            request.canonical_key.as_ref(),
            request.metadata.title.as_ref(),
            start_paused
        ));
        let Some(call_lock) = self.manager.get(to_guild_id(guild_id)) else {
            append_debug_log("runtime: play_track missing call");
            return Err(SongbirdRuntimeError::MissingCall);
        };

        let input = build_input(self.stream_client(request.provider_id.as_ref())?, request)?
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .map_err(make_playable_error)?;
        let equalizer =
            EqualizerControl::new(options.equalizer_enabled, options.equalizer_transition);
        let worker_equalizer = options.equalizer_enabled.then(|| equalizer.clone());
        let (input, stretch_timeline) = if let Some(envelope) = options.tempo_envelope {
            let (input, timeline) = build_stretched_input_with_equalizer(
                input,
                options.source_start,
                envelope,
                worker_equalizer,
            )
            .map_err(SongbirdRuntimeError::TempoStretch)?;
            (input, Some(timeline))
        } else if options.source_start.is_zero() {
            (
                wrap_parsed_equalizer(input, equalizer.clone())
                    .map_err(SongbirdRuntimeError::TempoStretch)?,
                None,
            )
        } else {
            (
                build_trimmed_input_with_equalizer(input, options.source_start, worker_equalizer)
                    .map_err(SongbirdRuntimeError::TempoStretch)?,
                None,
            )
        };
        let transition_events = events.clone();
        let prefetch_events = events.clone();
        let frame_slot = Arc::new(Mutex::new(None));
        let identity = TrackIdentity {
            guild_id,
            session_id,
            playback_id,
        };
        let handle = {
            let mut call = call_lock.lock().await;
            append_debug_log(format!(
                "runtime: play_track call state guild_id={} session_id={} playback_id={} current_channel={:?} current_connection={}",
                guild_id.get(),
                session_id,
                playback_id.get(),
                call.current_channel().map(|channel_id| channel_id.0.get()),
                call.current_connection().is_some()
            ));
            let mut track = Track::new(input).volume(options.initial_gain);
            if start_paused {
                track = track.pause();
            }
            if let Some(delay) = options
                .transition_after
                .map(|delay| stretched_event_delay(delay, stretch_timeline))
            {
                track.events.add_event(
                    EventData::new(
                        Event::Delayed(delay),
                        TrackTransitionNotifier {
                            guild_id,
                            session_id,
                            playback_id,
                            events: transition_events,
                        },
                    ),
                    Duration::ZERO,
                );
            }
            if let Some(delay) = options
                .prefetch_after
                .map(|delay| stretched_event_delay(delay, stretch_timeline))
            {
                track.events.add_event(
                    EventData::new(
                        Event::Delayed(delay),
                        TrackPrefetchNotifier {
                            guild_id,
                            session_id,
                            playback_id,
                            events: prefetch_events,
                        },
                    ),
                    Duration::ZERO,
                );
            }
            // This cheap local timer is inert for ordinary playback.  When a
            // prepared deck is armed, it is the mixer-clock boundary that
            // starts the incoming deck; no host position/sleep round trip is
            // involved at the audible handoff.
            track.events.add_event(
                EventData::new(
                    Event::Periodic(TIMESTEP_LENGTH, None),
                    TargetFrameTransitionNotifier {
                        slot: frame_slot.clone(),
                    },
                ),
                Duration::ZERO,
            );
            call.play(track)
        };
        let lifecycle = Arc::new(TrackLifecycle::default());
        let playback_events = events.clone();
        let error_events = events.clone();
        let listener_result: Result<(), songbird::tracks::ControlError> = (|| {
            handle.add_event(
                Event::Track(TrackEvent::End),
                TrackEndNotifier {
                    guild_id,
                    session_id,
                    playback_id,
                    events,
                    lifecycle: lifecycle.clone(),
                    frame_slot: frame_slot.clone(),
                },
            )?;
            handle.add_event(
                Event::Track(TrackEvent::Playable),
                TrackPlayableLogger {
                    guild_id,
                    session_id,
                    playback_id,
                    title: request.metadata.title.to_string(),
                    provider_id: request.provider_id.to_string(),
                    canonical_key: request.canonical_key.to_string(),
                    events: playback_events,
                },
            )?;
            handle.add_event(
                Event::Track(TrackEvent::Error),
                TrackErrorLogger {
                    guild_id,
                    session_id,
                    playback_id,
                    title: request.metadata.title.to_string(),
                    provider_id: request.provider_id.to_string(),
                    canonical_key: request.canonical_key.to_string(),
                    events: error_events,
                    lifecycle: lifecycle.clone(),
                    frame_slot: frame_slot.clone(),
                },
            )?;
            Ok(())
        })();
        if let Err(error) = listener_result {
            let _ = handle.stop();
            return Err(SongbirdRuntimeError::TrackEvent(error.to_string()));
        }
        // `Track::pause()` prevents audio from being mixed, but it does not
        // itself guarantee that the input has crossed Songbird's lazy
        // initialisation boundary.  Ready the decoder before handing a
        // prepared deck to the playback coordinator.  This also makes a
        // failed/underflowing source visible while the outgoing deck is still
        // active, so BeatMatched is never claimed for an unready deck.
        if let Err(error) = handle.make_playable_async().await {
            let _ = handle.stop();
            return Err(SongbirdRuntimeError::TrackReady(error.to_string()));
        }
        append_debug_log(format!(
            "runtime: play_track handle registered guild_id={} session_id={} playback_id={} paused={} ready=true",
            guild_id.get(),
            session_id,
            playback_id.get(),
            start_paused
        ));

        let concrete = Arc::new(SongbirdTrackHandle {
            handle,
            lifecycle,
            source_start: options.source_start,
            stretch_timeline,
            equalizer,
            identity,
            frame_slot,
            registry: Arc::downgrade(&self.tracks),
        });
        if let Ok(mut tracks) = self.tracks.lock() {
            tracks.insert(identity, Arc::downgrade(&concrete));
        }
        Ok(concrete)
    }
}

fn build_stream_client(provider_id: &'static str) -> Result<Client, SongbirdRuntimeError> {
    let builder = Client::builder()
        .user_agent("wotoha-rust/0.1.0")
        .connect_timeout(STREAM_CONNECT_TIMEOUT)
        .read_timeout(STREAM_READ_TIMEOUT)
        .tcp_keepalive(STREAM_TCP_KEEPALIVE)
        .pool_idle_timeout(STREAM_POOL_IDLE_TIMEOUT)
        .http2_keep_alive_interval(STREAM_HTTP2_KEEP_ALIVE_INTERVAL)
        .http2_keep_alive_timeout(STREAM_HTTP2_KEEP_ALIVE_TIMEOUT)
        .http2_keep_alive_while_idle(false)
        .pool_max_idle_per_host(STREAM_POOL_MAX_IDLE_PER_HOST)
        .redirect(Policy::custom(move |attempt| {
            if attempt.previous().len() >= STREAM_REDIRECT_LIMIT {
                attempt.error("too many redirects")
            } else if is_allowed_prepared_url(provider_id, attempt.url().as_str()) {
                attempt.follow()
            } else {
                attempt.error("redirect target host is not allowed for provider")
            }
        }));

    let builder = if STREAM_FORCE_IPV4 {
        builder.local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
    } else {
        builder
    };

    builder.build().map_err(SongbirdRuntimeError::HttpClient)
}

#[async_trait]
impl VoiceRuntime for SongbirdRuntime {
    type Error = SongbirdRuntimeError;

    async fn play_track(
        &self,
        guild_id: GuildKey,
        session_id: u64,
        playback_id: PlaybackId,
        request: &TrackRequest,
        events: RuntimeEventSink,
    ) -> Result<Arc<dyn RuntimeTrackHandle>, Self::Error> {
        self.play_track_with_options(
            guild_id,
            session_id,
            playback_id,
            request,
            events,
            TrackStartOptions::default(),
        )
        .await
    }

    async fn play_track_with_options(
        &self,
        guild_id: GuildKey,
        session_id: u64,
        playback_id: PlaybackId,
        request: &TrackRequest,
        events: RuntimeEventSink,
        options: TrackStartOptions,
    ) -> Result<Arc<dyn RuntimeTrackHandle>, Self::Error> {
        self.register_track(RegisterTrackParams {
            guild_id,
            session_id,
            playback_id,
            request,
            events,
            options,
            start_paused: false,
        })
        .await
    }

    async fn prepare_track_with_options(
        &self,
        guild_id: GuildKey,
        session_id: u64,
        playback_id: PlaybackId,
        request: &TrackRequest,
        events: RuntimeEventSink,
        options: TrackStartOptions,
    ) -> Result<Arc<dyn RuntimeTrackHandle>, Self::Error> {
        self.register_track(RegisterTrackParams {
            guild_id,
            session_id,
            playback_id,
            request,
            events,
            options,
            start_paused: true,
        })
        .await
    }

    async fn arm_transition(
        &self,
        guild_id: GuildKey,
        session_id: u64,
        outgoing_playback_id: PlaybackId,
        incoming_playback_id: PlaybackId,
        target_position: Duration,
        generation: u64,
        events: RuntimeEventSink,
    ) -> TransitionArmResult {
        let outgoing_identity = TrackIdentity {
            guild_id,
            session_id,
            playback_id: outgoing_playback_id,
        };
        let incoming_identity = TrackIdentity {
            guild_id,
            session_id,
            playback_id: incoming_playback_id,
        };
        let (Some(outgoing), Some(incoming)) =
            (self.track(outgoing_identity), self.track(incoming_identity))
        else {
            return TransitionArmResult::IdentityMismatch;
        };
        let (Ok(outgoing_state), Ok(incoming_state)) = (
            outgoing.handle.get_info().await,
            incoming.handle.get_info().await,
        ) else {
            return TransitionArmResult::Underflow;
        };
        if incoming_state.ready != ReadyState::Playable {
            return TransitionArmResult::NotReady;
        }
        if outgoing_state.playing != PlayMode::Play {
            return TransitionArmResult::DeadlineMissed;
        }
        let target_track_position = outgoing.track_position_for_source(target_position);
        // The periodic event fires one tick early and its Play command is
        // consumed on the following mixer tick.  Refuse an arm that cannot
        // reserve both of those ticks; this prevents a late arm from being
        // mislabeled BeatMatched.
        let current_frame = frame_for(outgoing_state.position);
        let target_frame = frame_for(target_track_position);
        if target_frame <= current_frame.saturating_add(2) {
            return TransitionArmResult::DeadlineMissed;
        }
        let trigger_frame = target_frame.saturating_sub(1);
        let mut slot = match outgoing.frame_slot.lock() {
            Ok(slot) => slot,
            Err(_) => return TransitionArmResult::Underflow,
        };
        if slot.is_some() {
            return TransitionArmResult::IdentityMismatch;
        }
        *slot = Some(ArmedFrameTransition {
            guild_id,
            session_id,
            outgoing_playback_id,
            incoming_playback_id,
            generation,
            target_position,
            target_frame,
            trigger_frame,
            last_observed_frame: current_frame,
            incoming: incoming.handle.clone(),
            events,
            pending: None,
        });
        TransitionArmResult::Armed { target_frame }
    }

    async fn analyze_track(&self, request: &TrackRequest) -> Option<TrackAnalysis> {
        self.analyze_track_with_backend(request)
            .await
            .map(|outcome| outcome.analysis)
    }

    fn cached_track_analysis(&self, request: &TrackRequest) -> Option<TrackAnalysis> {
        if !analysis_source_supported(request) {
            return None;
        }
        let key = AnalysisCacheKey::from_request(request).ok()?;
        self.analysis_cache.load(&key).ok().flatten()
    }

    async fn disconnect_guild(&self, guild_id: GuildKey) -> Result<(), Self::Error> {
        match self.manager.remove(to_guild_id(guild_id)).await {
            Ok(()) | Err(JoinError::NoCall) => Ok(()),
            Err(error) => Err(SongbirdRuntimeError::Disconnect(error.to_string())),
        }
    }
}

impl SongbirdRuntime {
    /// Analyze a track while preserving the backend/provenance needed by
    /// callers that enforce a fresh neural-analysis gate.
    pub async fn analyze_track_with_backend(
        &self,
        request: &TrackRequest,
    ) -> Option<AnalysisOutcome> {
        self.analyze_track_with_backend_mode(request, false).await
    }

    /// Additive V2 conversion entry point for callers that are ready to feed
    /// the versioned planner while the existing songbird/playback API remains
    /// V1.  Cache hits preserve the backend provenance of the cached record;
    /// transient/permanent classical results are converted with a classical
    /// rhythm method and retain every non-rhythm component.
    pub async fn analyze_track_v2(&self, request: &TrackRequest) -> Option<TrackAnalysisV2> {
        if !analysis_source_supported(request) {
            return None;
        }
        let key = AnalysisCacheKey::from_request(request).ok()?;
        // Prefer the compact V2 namespace when available.  This preserves
        // model scores, timing support, hypotheses and provenance that a V1
        // cache cannot represent, while leaving the V1 cache untouched.
        if let Ok(Some(analysis)) = self.analysis_cache.load_v2(&key) {
            return Some(analysis);
        }
        if let Ok(Some(analysis)) = self.classical_cache.load_v2(&key) {
            return Some(analysis);
        }
        let outcome = self.analyze_track_with_backend(request).await?;
        let backend = outcome.backend;
        let (analysis, neural_rhythm) = if let Some(rhythm) = outcome.v2_rhythm {
            match crate::beat_this_analysis::track_analysis_v2_from_legacy_rhythm(
                &outcome.analysis,
                rhythm,
                true,
            ) {
                Some(analysis) => (analysis, true),
                // A future decoder/schema change must fail closed to the
                // unchanged classical V1 aggregate rather than dropping the
                // whole V2 record or mislabeling it Hybrid.
                None => (track_analysis_v2_from_legacy(&outcome.analysis)?, false),
            }
        } else {
            (
                track_analysis_v2_from_legacy_with_backend(&outcome.analysis, backend)?,
                matches!(
                    backend,
                    AnalysisBackend::Neural | AnalysisBackend::CachedNeural
                ),
            )
        };
        if neural_rhythm {
            let _ = self.analysis_cache.store_v2(&key, &analysis);
        } else if matches!(
            backend,
            AnalysisBackend::ClassicalPermanentIneligible
                | AnalysisBackend::CachedClassicalPermanentIneligible
        ) {
            let _ = self.classical_cache.store_v2(&key, &analysis);
        }
        Some(analysis)
    }

    /// Opt-in V2 planner hook for playback integrations.  The established
    /// VoiceRuntime trait and V1 playback planner remain unchanged; callers
    /// that have two V2 records can explicitly select the timeline-first
    /// guarded plan here and then hand its legacy executable plan to the
    /// existing render/arm path.
    pub fn plan_transition_v2(
        &self,
        outgoing: &TrackAnalysisV2,
        incoming: &TrackAnalysisV2,
        config: &AutoMixConfig,
    ) -> V2GuardedTransitionPlan {
        plan_guarded_transition_v2_for_analysis(outgoing, incoming, config)
    }

    /// Analyze a newly-resolved playback request without reusing an earlier analysis result.
    ///
    /// Corpus acquisition retries deliberately obtain a fresh signed source URL.  A provider
    /// can keep the same canonical key, so the ordinary method would otherwise report a cache
    /// hit after a prior cycle and prevent the strict fresh-neural gate from observing the new
    /// attempt.  Cancellation and result storage remain identical to the ordinary path.
    pub async fn analyze_track_with_backend_fresh(
        &self,
        request: &TrackRequest,
    ) -> Option<AnalysisOutcome> {
        self.analyze_track_with_backend_mode(request, true).await
    }

    async fn analyze_track_with_backend_mode(
        &self,
        request: &TrackRequest,
        bypass_cache: bool,
    ) -> Option<AnalysisOutcome> {
        if !analysis_source_supported(request) {
            return None;
        }
        let key = AnalysisCacheKey::from_request(request).ok()?;
        if !bypass_cache {
            if let Ok(Some(analysis)) = self.analysis_cache.load(&key) {
                return Some(AnalysisOutcome {
                    analysis,
                    backend: AnalysisBackend::CachedNeural,
                    v2_rhythm: None,
                });
            }
            // This directory has a distinct analyzer identity and is written only
            // for permanent neural ineligibility. A transient model failure never
            // reaches disk, so a later model retry can upgrade the same source.
            if let Ok(Some(analysis)) = self.classical_cache.load(&key) {
                return Some(AnalysisOutcome {
                    analysis,
                    backend: AnalysisBackend::CachedClassicalPermanentIneligible,
                    v2_rhythm: None,
                });
            }
            if let Some(analysis) = self.classical_fallback(&key) {
                return Some(AnalysisOutcome {
                    analysis,
                    backend: AnalysisBackend::CachedClassicalTransientFailure,
                    v2_rhythm: None,
                });
            }
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = CancellationToken::new();
        let cancel_on_drop = CancelAnalysisOnDrop {
            cancelled: cancelled.clone(),
            cancellation: cancellation.clone(),
        };
        let outcome = tokio::time::timeout(ANALYSIS_TIMEOUT, async {
            let _permit = self.analysis_limit.acquire().await.ok()?;
            if !bypass_cache {
                if let Ok(Some(analysis)) = self.analysis_cache.load(&key) {
                    return Some(AnalysisOutcome {
                        analysis,
                        backend: AnalysisBackend::CachedNeural,
                        v2_rhythm: None,
                    });
                }
                if let Ok(Some(analysis)) = self.classical_cache.load(&key) {
                    return Some(AnalysisOutcome {
                        analysis,
                        backend: AnalysisBackend::CachedClassicalPermanentIneligible,
                        v2_rhythm: None,
                    });
                }
                if let Some(analysis) = self.classical_fallback(&key) {
                    return Some(AnalysisOutcome {
                        analysis,
                        backend: AnalysisBackend::CachedClassicalTransientFailure,
                        v2_rhythm: None,
                    });
                }
            }
            let input = build_input_with_cancellation(
                self.stream_client(request.provider_id.as_ref()).ok()?,
                request,
                Some(cancellation.clone()),
            )
            .ok()?
            .make_playable_async(get_codec_registry(), get_probe())
            .await
            .ok()?;
            let worker_cancelled = cancelled.clone();
            tokio::task::spawn_blocking(move || {
                analyze_input_with_cancel_outcome(input, &worker_cancelled)
            })
            .await
            .ok()?
        })
        .await
        .ok()??;
        drop(cancel_on_drop);
        self.store_analysis_outcome(&key, &outcome);
        Some(outcome)
    }

    fn store_analysis_outcome(&self, key: &AnalysisCacheKey, outcome: &AnalysisOutcome) {
        match outcome.backend {
            AnalysisBackend::Neural => {
                if let Ok(mut fallbacks) = self.classical_fallbacks.lock() {
                    fallbacks.remove(key);
                }
                let _ = self.analysis_cache.store(key, &outcome.analysis);
            }
            AnalysisBackend::ClassicalPermanentIneligible => {
                if let Ok(mut fallbacks) = self.classical_fallbacks.lock() {
                    fallbacks.remove(key);
                }
                let _ = self.classical_cache.store(key, &outcome.analysis);
            }
            AnalysisBackend::ClassicalTransientFailure => {
                if let Ok(mut fallbacks) = self.classical_fallbacks.lock() {
                    fallbacks.insert(
                        key.clone(),
                        ClassicalFallback {
                            stored_at: Instant::now(),
                            analysis: outcome.analysis.clone(),
                        },
                    );
                }
            }
            AnalysisBackend::CachedNeural | AnalysisBackend::CachedClassicalPermanentIneligible => {
            }
            AnalysisBackend::CachedClassicalTransientFailure => {}
        }
    }

    fn classical_fallback(&self, key: &AnalysisCacheKey) -> Option<TrackAnalysis> {
        let mut fallbacks = self.classical_fallbacks.lock().ok()?;
        let fallback = fallbacks.get(key)?;
        if fallback.is_fresh_at(Instant::now()) {
            return Some(fallback.analysis.clone());
        }
        fallbacks.remove(key);
        None
    }

    fn track(&self, identity: TrackIdentity) -> Option<Arc<SongbirdTrackHandle>> {
        let mut tracks = self.tracks.lock().ok()?;
        tracks.retain(|_, handle| handle.strong_count() != 0);
        tracks.get(&identity).and_then(Weak::upgrade)
    }
}

#[async_trait]
impl VoiceGatewayRuntime for SongbirdRuntime {
    async fn ensure_joined(
        &self,
        guild_id: GuildKey,
        channel_id: ChannelKey,
    ) -> Result<bool, Self::Error> {
        SongbirdRuntime::ensure_joined(self, guild_id, channel_id).await
    }

    async fn handle_gateway_event(&self, _event: VoiceGatewayEvent) -> Result<(), Self::Error> {
        Ok(())
    }
}

pub fn build_input(
    client: &Client,
    request: &TrackRequest,
) -> Result<songbird::input::Input, SongbirdRuntimeError> {
    build_input_with_cancellation(client, request, None)
}

fn build_input_with_cancellation(
    client: &Client,
    request: &TrackRequest,
    cancellation: Option<CancellationToken>,
) -> Result<songbird::input::Input, SongbirdRuntimeError> {
    match &request.prepared {
        PreparedSource::Http {
            stream_url,
            headers,
            content_length,
            range_chunk_size,
            range_mode,
            expires_at_unix: _,
        } => {
            validate_prepared_source_url(request.provider_id.as_ref(), stream_url.as_ref())?;
            let headers = build_headers(headers)?;
            if let (Some(content_length), Some(range_chunk_size)) =
                (*content_length, *range_chunk_size)
                && should_use_ranged_request(Some(content_length), Some(range_chunk_size))
            {
                Ok(RangedHttpRequest::new_with_headers_and_cancellation(
                    client.clone(),
                    stream_url.to_string(),
                    headers,
                    Some(content_length),
                    range_chunk_size,
                    *range_mode,
                    cancellation,
                )
                .into())
            } else {
                Ok(CancellableHttpRequest::new_with_headers(
                    client.clone(),
                    stream_url.to_string(),
                    headers,
                    *content_length,
                    cancellation,
                )
                .into())
            }
        }
        PreparedSource::Hls {
            playlist_url,
            headers,
            expires_at_unix: _,
        } => {
            validate_prepared_source_url(request.provider_id.as_ref(), playlist_url.as_ref())?;
            let headers = build_headers(headers)?;
            if request.provider_id.as_ref() == "niconico" {
                Ok(NiconicoHlsRequest::new_with_cancellation(
                    client.clone(),
                    playlist_url.to_string(),
                    headers,
                    cancellation,
                )
                .into())
            } else {
                Ok(ValidatedHlsRequest::new_with_cancellation(
                    client.clone(),
                    request.provider_id.to_string(),
                    playlist_url.to_string(),
                    headers,
                    cancellation,
                )
                .into())
            }
        }
    }
}

fn analysis_source_supported(request: &TrackRequest) -> bool {
    match request.prepared {
        PreparedSource::Http { .. } => true,
        PreparedSource::Hls { .. } => request
            .metadata
            .duration
            .is_some_and(|duration| !duration.is_zero() && duration <= MAX_ANALYSIS_DURATION),
    }
}

fn should_use_ranged_request(content_length: Option<u64>, range_chunk_size: Option<u64>) -> bool {
    content_length.is_some() && range_chunk_size.is_some()
}

fn make_playable_error(error: MakePlayableError) -> SongbirdRuntimeError {
    SongbirdRuntimeError::MakePlayable(error.to_string())
}

fn validate_prepared_source_url(
    provider_id: &str,
    raw_url: &str,
) -> Result<(), SongbirdRuntimeError> {
    if is_allowed_prepared_url(provider_id, raw_url) {
        return Ok(());
    }

    Err(SongbirdRuntimeError::DisallowedPreparedUrl {
        provider_id: provider_id.to_owned(),
        url: summarize_url_for_logs(raw_url),
    })
}

fn build_headers(headers: &[PreparedHeader]) -> Result<HeaderMap, SongbirdRuntimeError> {
    let mut out = HeaderMap::new();
    for header in headers {
        let name = HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|_| SongbirdRuntimeError::InvalidHeaderName(header.name.to_string()))?;
        let value = HeaderValue::from_str(header.value.as_ref())
            .map_err(|_| SongbirdRuntimeError::InvalidHeaderValue(header.value.to_string()))?;
        out.insert(name, value);
    }

    Ok(out)
}

fn to_guild_id(guild_id: GuildKey) -> GuildId {
    GuildId::new(guild_id.get())
}

fn to_channel_id(channel_id: ChannelKey) -> ChannelId {
    ChannelId::new(channel_id.get())
}

#[derive(Default)]
struct TrackLifecycle {
    state: AtomicU8,
}

impl TrackLifecycle {
    const RUNNING: u8 = 0;
    const STOP_REQUESTED: u8 = 1;
    const ERRORED: u8 = 2;
    const TERMINATED: u8 = 3;

    fn request_stop(&self) {
        let _ = self.state.compare_exchange(
            Self::RUNNING,
            Self::STOP_REQUESTED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn mark_error(&self) -> bool {
        self.state
            .compare_exchange(
                Self::RUNNING,
                Self::ERRORED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn finish_reason(&self) -> Option<TrackEndReason> {
        let state = self.state.swap(Self::TERMINATED, Ordering::AcqRel);
        match state {
            Self::RUNNING => Some(TrackEndReason::Completed),
            Self::STOP_REQUESTED => Some(TrackEndReason::Stopped),
            Self::ERRORED | Self::TERMINATED => None,
            _ => Some(TrackEndReason::Completed),
        }
    }
}

struct SongbirdTrackHandle {
    handle: TrackHandle,
    lifecycle: Arc<TrackLifecycle>,
    source_start: Duration,
    stretch_timeline: Option<StretchTimeline>,
    equalizer: EqualizerControl,
    identity: TrackIdentity,
    frame_slot: Arc<Mutex<Option<ArmedFrameTransition>>>,
    registry: Weak<Mutex<HashMap<TrackIdentity, Weak<SongbirdTrackHandle>>>>,
}

struct ArmedFrameTransition {
    guild_id: GuildKey,
    session_id: u64,
    outgoing_playback_id: PlaybackId,
    incoming_playback_id: PlaybackId,
    generation: u64,
    target_position: Duration,
    target_frame: u64,
    trigger_frame: u64,
    last_observed_frame: u64,
    incoming: TrackHandle,
    events: RuntimeEventSink,
    pending: Option<Arc<AtomicU8>>,
}

#[async_trait]
impl RuntimeTrackHandle for SongbirdTrackHandle {
    fn stop(&self) {
        self.lifecycle.request_stop();
        if let Ok(mut slot) = self.frame_slot.lock() {
            *slot = None;
        }
        if let Some(registry) = self.registry.upgrade()
            && let Ok(mut tracks) = registry.lock()
        {
            tracks.remove(&self.identity);
        }
        let _ = self.handle.stop();
    }

    fn set_volume(&self, volume: f32) {
        let _ = self.handle.set_volume(volume);
    }

    fn frame_scheduled_transition_support(&self) -> FrameScheduledTransitionSupport {
        FrameScheduledTransitionSupport::TrackPositionDelayedEvent
    }

    fn pause(&self) {
        let _ = self.handle.pause();
    }

    fn resume(&self) {
        let _ = self.handle.play();
    }

    async fn position(&self) -> Option<Duration> {
        self.handle.get_info().await.ok().map(|state| {
            self.stretch_timeline
                .map_or(self.source_start + state.position, |timeline| {
                    timeline.source_start + timeline.envelope.source_elapsed(state.position)
                })
        })
    }

    async fn seek(&self, position: Duration) -> bool {
        self.stretch_timeline.is_none()
            && self.source_start.is_zero()
            && self.handle.seek_async(position).await.is_ok()
    }

    fn schedule_equalizer_transition(
        &self,
        transition: wotoha_core::automix::EqTransition,
    ) -> bool {
        self.equalizer.schedule(transition)
    }

    fn cancel_equalizer_transition(&self, id: u64) {
        self.equalizer.cancel(id);
    }
}

impl SongbirdTrackHandle {
    fn track_position_for_source(&self, source_position: Duration) -> Duration {
        self.stretch_timeline.map_or_else(
            || source_position.saturating_sub(self.source_start),
            |timeline| {
                timeline
                    .envelope
                    .output_elapsed(source_position.saturating_sub(timeline.source_start))
            },
        )
    }
}

fn frame_for(position: Duration) -> u64 {
    (position.as_nanos() / TIMESTEP_LENGTH.as_nanos()).min(u128::from(u64::MAX)) as u64
}

fn stretched_event_delay(source_position: Duration, timeline: Option<StretchTimeline>) -> Duration {
    timeline.map_or(source_position, |timeline| {
        timeline
            .envelope
            .output_elapsed(source_position.saturating_sub(timeline.source_start))
    })
}

struct TrackEndNotifier {
    guild_id: GuildKey,
    session_id: u64,
    playback_id: PlaybackId,
    events: RuntimeEventSink,
    lifecycle: Arc<TrackLifecycle>,
    frame_slot: Arc<Mutex<Option<ArmedFrameTransition>>>,
}

#[serenity::async_trait]
impl VoiceEventHandler for TrackEndNotifier {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        if let Ok(mut slot) = self.frame_slot.lock() {
            *slot = None;
        }
        let reason = self.lifecycle.finish_reason()?;
        append_debug_log(format!(
            "runtime: track end guild_id={} session_id={} playback_id={} reason={reason:?}",
            self.guild_id.get(),
            self.session_id,
            self.playback_id.get()
        ));
        let _ = self.events.send(PlaybackRuntimeEvent::TrackEnded {
            guild_id: self.guild_id,
            session_id: self.session_id,
            playback_id: self.playback_id,
            reason,
        });
        None
    }
}

struct TrackPlayableLogger {
    guild_id: GuildKey,
    session_id: u64,
    playback_id: PlaybackId,
    title: String,
    provider_id: String,
    canonical_key: String,
    events: RuntimeEventSink,
}

struct TrackTransitionNotifier {
    guild_id: GuildKey,
    session_id: u64,
    playback_id: PlaybackId,
    events: RuntimeEventSink,
}

struct TrackPrefetchNotifier {
    guild_id: GuildKey,
    session_id: u64,
    playback_id: PlaybackId,
    events: RuntimeEventSink,
}

const FRAME_START_PENDING: u8 = 1;
const FRAME_START_STARTED: u8 = 2;
const FRAME_START_FAILED: u8 = 3;

struct TargetFrameTransitionNotifier {
    slot: Arc<Mutex<Option<ArmedFrameTransition>>>,
}

#[serenity::async_trait]
impl VoiceEventHandler for TargetFrameTransitionNotifier {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        // Songbird's mixer drains track commands, mixes one quantum, and
        // advances TrackState.position before the event runner processes its
        // Periodic event.  Consequently this position is the just-completed
        // mixer tick.  Queueing Play here is consumed by the next command
        // drain, so the trigger is intentionally one quantum before target.
        let EventContext::Track([(state, _)]) = ctx else {
            return None;
        };
        let mut slot = match self.slot.lock() {
            Ok(slot) => slot,
            Err(_) => return None,
        };
        let armed = slot.as_mut()?;
        let current_frame = frame_for(state.position);
        if current_frame < armed.last_observed_frame {
            let _ = armed
                .events
                .send(PlaybackRuntimeEvent::TransitionArmFailed {
                    guild_id: armed.guild_id,
                    session_id: armed.session_id,
                    outgoing_playback_id: armed.outgoing_playback_id,
                    incoming_playback_id: armed.incoming_playback_id,
                    generation: armed.generation,
                    target_position: armed.target_position,
                    target_frame: armed.target_frame,
                    actual_frame: Some(current_frame),
                    skew_ms: Some(current_frame.abs_diff(armed.target_frame) * 20),
                    underflow: false,
                    failure_kind: TransitionArmFailureKind::IdentityMismatch,
                    reason: "outgoing source position moved backwards while armed".into(),
                });
            *slot = None;
            return None;
        }
        armed.last_observed_frame = current_frame;

        if let Some(pending) = &armed.pending {
            match pending.load(Ordering::Acquire) {
                FRAME_START_STARTED => {
                    let actual_frame = current_frame;
                    let skew_ms = actual_frame.abs_diff(armed.target_frame) * 20;
                    if skew_ms > 35 {
                        let _ = armed
                            .events
                            .send(PlaybackRuntimeEvent::TransitionArmFailed {
                                guild_id: armed.guild_id,
                                session_id: armed.session_id,
                                outgoing_playback_id: armed.outgoing_playback_id,
                                incoming_playback_id: armed.incoming_playback_id,
                                generation: armed.generation,
                                target_position: armed.target_position,
                                target_frame: armed.target_frame,
                                actual_frame: Some(actual_frame),
                                skew_ms: Some(skew_ms),
                                underflow: false,
                                failure_kind: TransitionArmFailureKind::StartedLate,
                                reason: "target-frame start exceeded skew budget".into(),
                            });
                    } else {
                        let _ = armed.events.send(PlaybackRuntimeEvent::TransitionStarted {
                            guild_id: armed.guild_id,
                            session_id: armed.session_id,
                            outgoing_playback_id: armed.outgoing_playback_id,
                            incoming_playback_id: armed.incoming_playback_id,
                            generation: armed.generation,
                            target_position: armed.target_position,
                            target_frame: armed.target_frame,
                            actual_frame,
                        });
                    }
                    *slot = None;
                }
                FRAME_START_FAILED => {
                    let actual_frame = current_frame;
                    let _ = armed
                        .events
                        .send(PlaybackRuntimeEvent::TransitionArmFailed {
                            guild_id: armed.guild_id,
                            session_id: armed.session_id,
                            outgoing_playback_id: armed.outgoing_playback_id,
                            incoming_playback_id: armed.incoming_playback_id,
                            generation: armed.generation,
                            target_position: armed.target_position,
                            target_frame: armed.target_frame,
                            actual_frame: Some(actual_frame),
                            skew_ms: Some(actual_frame.abs_diff(armed.target_frame) * 20),
                            underflow: true,
                            failure_kind: TransitionArmFailureKind::ActionFailed,
                            reason: "incoming deck was not playable at target frame".into(),
                        });
                    *slot = None;
                }
                _ if current_frame > armed.target_frame.saturating_add(1) => {
                    let _ = armed
                        .events
                        .send(PlaybackRuntimeEvent::TransitionArmFailed {
                            guild_id: armed.guild_id,
                            session_id: armed.session_id,
                            outgoing_playback_id: armed.outgoing_playback_id,
                            incoming_playback_id: armed.incoming_playback_id,
                            generation: armed.generation,
                            target_position: armed.target_position,
                            target_frame: armed.target_frame,
                            actual_frame: Some(current_frame),
                            skew_ms: Some(current_frame.abs_diff(armed.target_frame) * 20),
                            underflow: false,
                            failure_kind: TransitionArmFailureKind::DeadlineMissed,
                            reason: "target-frame start missed its deadline".into(),
                        });
                    *slot = None;
                }
                _ => {}
            }
            return None;
        }

        if current_frame > armed.target_frame.saturating_add(1) {
            let _ = armed
                .events
                .send(PlaybackRuntimeEvent::TransitionArmFailed {
                    guild_id: armed.guild_id,
                    session_id: armed.session_id,
                    outgoing_playback_id: armed.outgoing_playback_id,
                    incoming_playback_id: armed.incoming_playback_id,
                    generation: armed.generation,
                    target_position: armed.target_position,
                    target_frame: armed.target_frame,
                    actual_frame: Some(current_frame),
                    skew_ms: Some(current_frame.abs_diff(armed.target_frame) * 20),
                    underflow: false,
                    failure_kind: TransitionArmFailureKind::DeadlineMissed,
                    reason: "target-frame arm was observed late".into(),
                });
            *slot = None;
            return None;
        }
        if current_frame < armed.trigger_frame {
            return None;
        }

        let pending = Arc::new(AtomicU8::new(FRAME_START_PENDING));
        let status = pending.clone();
        let action_result = armed.incoming.action(move |view| {
            if view.ready == ReadyState::Playable
                && !matches!(
                    view.playing,
                    PlayMode::Stop | PlayMode::End | PlayMode::Errored(_)
                )
            {
                // `TrackHandle::action` executes on the mixer thread.  The
                // state mutation is therefore consumed by this same runtime
                // clock, and the first incoming PCM frame is mixed on the
                // following 20 ms tick.
                *view.playing = PlayMode::Play;
                status.store(FRAME_START_STARTED, Ordering::Release);
            } else {
                status.store(FRAME_START_FAILED, Ordering::Release);
            }
            None
        });
        if action_result.is_err() {
            let _ = armed
                .events
                .send(PlaybackRuntimeEvent::TransitionArmFailed {
                    guild_id: armed.guild_id,
                    session_id: armed.session_id,
                    outgoing_playback_id: armed.outgoing_playback_id,
                    incoming_playback_id: armed.incoming_playback_id,
                    generation: armed.generation,
                    target_position: armed.target_position,
                    target_frame: armed.target_frame,
                    actual_frame: Some(current_frame),
                    skew_ms: Some(current_frame.abs_diff(armed.target_frame) * 20),
                    underflow: false,
                    failure_kind: TransitionArmFailureKind::ActionFailed,
                    reason: "incoming deck command queue was closed".into(),
                });
            *slot = None;
        } else {
            armed.pending = Some(pending);
        }
        None
    }
}

#[serenity::async_trait]
impl VoiceEventHandler for TrackPrefetchNotifier {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        let _ = self
            .events
            .send(PlaybackRuntimeEvent::TransitionPrefetchDue {
                guild_id: self.guild_id,
                session_id: self.session_id,
                playback_id: self.playback_id,
            });
        None
    }
}

#[serenity::async_trait]
impl VoiceEventHandler for TrackTransitionNotifier {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        append_debug_log(format!(
            "runtime: transition due guild_id={} session_id={} playback_id={}",
            self.guild_id.get(),
            self.session_id,
            self.playback_id.get()
        ));
        let _ = self.events.send(PlaybackRuntimeEvent::TransitionDue {
            guild_id: self.guild_id,
            session_id: self.session_id,
            playback_id: self.playback_id,
        });
        None
    }
}

#[serenity::async_trait]
impl VoiceEventHandler for TrackPlayableLogger {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        append_debug_log(format!(
            "runtime: track playable guild_id={} session_id={} playback_id={} provider={} key={} title={}",
            self.guild_id.get(),
            self.session_id,
            self.playback_id.get(),
            self.provider_id,
            self.canonical_key,
            self.title
        ));
        let _ = self.events.send(PlaybackRuntimeEvent::TrackStarted {
            guild_id: self.guild_id,
            session_id: self.session_id,
            playback_id: self.playback_id,
        });
        info!(
            guild_id = self.guild_id.get(),
            session_id = self.session_id,
            playback_id = self.playback_id.get(),
            provider_id = self.provider_id,
            canonical_key = self.canonical_key,
            title = self.title,
            "track became playable"
        );
        None
    }
}

struct TrackErrorLogger {
    guild_id: GuildKey,
    session_id: u64,
    playback_id: PlaybackId,
    title: String,
    provider_id: String,
    canonical_key: String,
    events: RuntimeEventSink,
    lifecycle: Arc<TrackLifecycle>,
    frame_slot: Arc<Mutex<Option<ArmedFrameTransition>>>,
}

#[cfg(test)]
mod frame_scheduler_tests {
    use std::time::Duration;

    const QUANTUM: u64 = 20;
    const MAX_SKEW_MS: u64 = 35;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct ArmKey {
        outgoing: u64,
        outgoing_generation: u64,
        incoming: u64,
        incoming_generation: u64,
        token: u64,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum TickResult {
        Wait,
        Enqueued,
        Started { actual_frame: u64, skew_ms: u64 },
        Failure(&'static str),
    }

    #[derive(Clone, Copy, Debug)]
    struct TickScheduler {
        key: ArmKey,
        target_frame: u64,
        armed: bool,
        enqueued: bool,
        cancelled: bool,
        last_frame: u64,
    }

    impl TickScheduler {
        fn arm(key: ArmKey, current_frame: u64, target_frame: u64) -> Result<Self, &'static str> {
            if target_frame <= current_frame.saturating_add(2) {
                return Err("deadline");
            }
            Ok(Self {
                key,
                target_frame,
                armed: true,
                enqueued: false,
                cancelled: false,
                last_frame: current_frame,
            })
        }

        fn cancel(&mut self) {
            self.cancelled = true;
            self.armed = false;
        }

        fn tick(
            &mut self,
            key: ArmKey,
            current_frame: u64,
            ready: bool,
            action_ok: bool,
        ) -> TickResult {
            if self.cancelled || !self.armed {
                return TickResult::Failure("cancelled");
            }
            if key != self.key {
                self.armed = false;
                return TickResult::Failure("identity");
            }
            if current_frame < self.last_frame {
                self.armed = false;
                return TickResult::Failure("backward");
            }
            self.last_frame = current_frame;
            if self.enqueued {
                self.armed = false;
                let skew_ms = current_frame
                    .abs_diff(self.target_frame)
                    .saturating_mul(QUANTUM);
                return if skew_ms > MAX_SKEW_MS {
                    TickResult::Failure("late")
                } else {
                    TickResult::Started {
                        actual_frame: current_frame,
                        skew_ms,
                    }
                };
            }
            if current_frame > self.target_frame.saturating_add(1) {
                self.armed = false;
                return TickResult::Failure("late");
            }
            if current_frame.saturating_add(1) < self.target_frame {
                return TickResult::Wait;
            }
            if !ready {
                self.armed = false;
                return TickResult::Failure("not-ready");
            }
            if !action_ok {
                self.armed = false;
                return TickResult::Failure("action");
            }
            self.enqueued = true;
            TickResult::Enqueued
        }
    }

    fn key() -> ArmKey {
        ArmKey {
            outgoing: 1,
            outgoing_generation: 2,
            incoming: 3,
            incoming_generation: 4,
            token: 5,
        }
    }

    #[test]
    fn sixty_second_lookahead_never_starts_early() {
        let target = 60_000 / QUANTUM;
        let k = key();
        let mut scheduler = TickScheduler::arm(k, 0, target).unwrap();
        for frame in 0..target.saturating_sub(1) {
            assert_eq!(scheduler.tick(k, frame, true, true), TickResult::Wait);
        }
        assert!(!scheduler.enqueued);
        assert_eq!(
            scheduler.tick(k, target - 1, true, true),
            TickResult::Enqueued
        );
        assert_eq!(
            scheduler.tick(k, target, true, true),
            TickResult::Started {
                actual_frame: target,
                skew_ms: 0
            }
        );
    }

    #[test]
    fn one_quantum_late_is_within_budget_and_two_is_failure() {
        let k = key();
        let target = 300;
        let mut one_late = TickScheduler::arm(k, 0, target).unwrap();
        assert_eq!(one_late.tick(k, target, true, true), TickResult::Enqueued);
        assert_eq!(
            one_late.tick(k, target + 1, true, true),
            TickResult::Started {
                actual_frame: target + 1,
                skew_ms: 20
            }
        );
        let mut two_late = TickScheduler::arm(k, 0, target).unwrap();
        assert_eq!(
            two_late.tick(k, target + 2, true, true),
            TickResult::Failure("late")
        );
    }

    #[test]
    fn stale_identity_cancel_reorder_readiness_and_action_fail_without_start() {
        let k = key();
        let stale = ArmKey { incoming: 9, ..k };
        let mut scheduler = TickScheduler::arm(k, 0, 100).unwrap();
        assert_eq!(
            scheduler.tick(stale, 99, true, true),
            TickResult::Failure("identity")
        );
        for stale in [
            ArmKey { outgoing: 8, ..k },
            ArmKey {
                outgoing_generation: 8,
                ..k
            },
            ArmKey {
                incoming_generation: 8,
                ..k
            },
            ArmKey { token: 8, ..k },
        ] {
            let mut scheduler = TickScheduler::arm(k, 0, 100).unwrap();
            assert_eq!(
                scheduler.tick(stale, 99, true, true),
                TickResult::Failure("identity")
            );
        }
        let mut not_ready = TickScheduler::arm(k, 0, 100).unwrap();
        assert_eq!(
            not_ready.tick(k, 99, false, true),
            TickResult::Failure("not-ready")
        );
        let mut action_failed = TickScheduler::arm(k, 0, 100).unwrap();
        assert_eq!(
            action_failed.tick(k, 99, true, false),
            TickResult::Failure("action")
        );
        let mut cancelled = TickScheduler::arm(k, 0, 100).unwrap();
        cancelled.cancel();
        assert_eq!(
            cancelled.tick(k, 99, true, true),
            TickResult::Failure("cancelled")
        );
        let mut duplicate = TickScheduler::arm(k, 0, 100).unwrap();
        assert_eq!(duplicate.tick(k, 99, true, true), TickResult::Enqueued);
        assert_eq!(
            duplicate.tick(k, 99, true, true),
            TickResult::Started {
                actual_frame: 99,
                skew_ms: 20
            }
        );
        assert_eq!(
            duplicate.tick(k, 100, true, true),
            TickResult::Failure("cancelled")
        );
        let mut backwards = TickScheduler::arm(k, 0, 100).unwrap();
        assert_eq!(backwards.tick(k, 50, true, true), TickResult::Wait);
        assert_eq!(
            backwards.tick(k, 49, true, true),
            TickResult::Failure("backward")
        );
    }

    #[test]
    fn arm_after_deadline_is_rejected_without_floating_point_accumulation() {
        let target = Duration::from_secs(60).as_millis() as u64 / QUANTUM;
        assert!(matches!(
            TickScheduler::arm(key(), target - 2, target),
            Err("deadline")
        ));
    }
}

#[serenity::async_trait]
impl VoiceEventHandler for TrackErrorLogger {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let Ok(mut slot) = self.frame_slot.lock() {
            *slot = None;
        }
        if !self.lifecycle.mark_error() {
            return None;
        }

        let message = match ctx {
            EventContext::Track([(state, _)]) => match &state.playing {
                PlayMode::Errored(error) => error.to_string(),
                play_mode => format!("track failed in state {play_mode:?}"),
            },
            _ => "track failed during playback".to_owned(),
        };
        append_debug_log(format!(
            "runtime: track error guild_id={} session_id={} playback_id={} provider={} key={} title={} message={}",
            self.guild_id.get(),
            self.session_id,
            self.playback_id.get(),
            self.provider_id,
            self.canonical_key,
            self.title,
            message
        ));
        let _ = self.events.send(PlaybackRuntimeEvent::TrackErrored {
            guild_id: self.guild_id,
            session_id: self.session_id,
            playback_id: self.playback_id,
            message: message.clone().into(),
        });
        if let EventContext::Track([(state, _)]) = ctx {
            warn!(
                guild_id = self.guild_id.get(),
                session_id = self.session_id,
                playback_id = self.playback_id.get(),
                provider_id = self.provider_id,
                canonical_key = self.canonical_key,
                title = self.title,
                play_mode = ?state.playing,
                errored = matches!(state.playing, PlayMode::Errored(_)),
                error_message = message.as_str(),
                "track failed during playback"
            );
        } else {
            warn!(
                guild_id = self.guild_id.get(),
                session_id = self.session_id,
                playback_id = self.playback_id.get(),
                provider_id = self.provider_id,
                canonical_key = self.canonical_key,
                title = self.title,
                error_message = message.as_str(),
                "track failed during playback"
            );
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use reqwest::Client;
    use songbird::Songbird;
    use wotoha_core::{
        PreparedHeader, PreparedSource, TrackMetadata, TrackRequest, analysis::TrackAnalysisV2,
        automix::AutoMixConfig,
    };

    use super::{
        AnalysisBackend, AnalysisCacheKey, AnalysisOutcome, CLASSICAL_FALLBACK_RETRY_TTL,
        ClassicalFallback, MAX_ANALYSIS_DURATION, SongbirdRuntime, SongbirdRuntimeError,
        TrackEndReason, TrackLifecycle, analysis_source_supported, build_input,
        should_use_ranged_request,
    };

    fn request_with_source(prepared: PreparedSource) -> TrackRequest {
        request_with_source_and_duration(prepared, None)
    }

    fn request_with_source_and_duration(
        prepared: PreparedSource,
        duration: Option<Duration>,
    ) -> TrackRequest {
        TrackRequest::new(
            "youtube",
            "video-id",
            "https://www.youtube.com/watch?v=video-id",
            "https://www.youtube.com/watch?v=video-id",
            "https://www.youtube.com/watch?v=video-id",
            prepared,
            TrackMetadata::new(
                "title",
                "author",
                "https://www.youtube.com/watch?v=video-id",
                None,
                duration,
            ),
        )
    }

    #[test]
    fn only_enables_ranged_requests_with_known_content_length() {
        assert!(should_use_ranged_request(Some(1024), Some(256)));
        assert!(!should_use_ranged_request(None, Some(256)));
        assert!(!should_use_ranged_request(Some(1024), None));
    }

    #[test]
    fn lifecycle_marks_completed_when_track_ends_naturally() {
        let lifecycle = TrackLifecycle::default();
        assert_eq!(lifecycle.finish_reason(), Some(TrackEndReason::Completed));
    }

    #[test]
    fn lifecycle_marks_stopped_when_stop_was_requested() {
        let lifecycle = TrackLifecycle::default();
        lifecycle.request_stop();
        assert_eq!(lifecycle.finish_reason(), Some(TrackEndReason::Stopped));
    }

    #[test]
    fn lifecycle_suppresses_end_after_error() {
        let lifecycle = TrackLifecycle::default();
        assert!(lifecycle.mark_error());
        assert_eq!(lifecycle.finish_reason(), None);
        assert!(!lifecycle.mark_error());
    }

    #[test]
    fn v2_planner_hook_consumes_versioned_records() {
        let runtime = SongbirdRuntime::new(Songbird::serenity()).unwrap();
        let outgoing = TrackAnalysisV2::unanalyzed(Duration::from_secs(10));
        let incoming = TrackAnalysisV2::unanalyzed(Duration::from_secs(10));
        let config = AutoMixConfig {
            enabled: true,
            crossfade: Duration::from_secs(4),
            max_tempo_adjustment: 0.08,
            min_beat_confidence: 0.6,
        };

        let plan = runtime.plan_transition_v2(&outgoing, &incoming, &config);

        assert_eq!(plan.diagnostics.beatmatched_candidates, 0);
    }

    #[test]
    fn transient_classical_fallback_expires_for_a_neural_upgrade() {
        let stored_at = Instant::now();
        let fallback = ClassicalFallback {
            stored_at,
            analysis: wotoha_core::automix::TrackAnalysis::unanalyzed(Duration::from_secs(1)),
        };
        assert!(fallback.is_fresh_at(stored_at + CLASSICAL_FALLBACK_RETRY_TTL / 2));
        assert!(
            !fallback
                .is_fresh_at(stored_at + CLASSICAL_FALLBACK_RETRY_TTL + Duration::from_nanos(1))
        );
    }

    #[tokio::test]
    async fn transient_fallback_cache_hit_reports_distinct_backend() {
        let runtime = SongbirdRuntime::new(Songbird::serenity()).unwrap();
        let request = request_with_source(PreparedSource::http(
            "https://manifest.googlevideo.com/videoplayback",
            Vec::<PreparedHeader>::new().into_boxed_slice(),
            None,
            None,
        ));
        let key = AnalysisCacheKey::from_request(&request).unwrap();
        let fresh = AnalysisOutcome {
            analysis: wotoha_core::automix::TrackAnalysis::unanalyzed(Duration::from_secs(1)),
            backend: AnalysisBackend::ClassicalTransientFailure,
            v2_rhythm: None,
        };
        assert_eq!(fresh.backend, AnalysisBackend::ClassicalTransientFailure);
        runtime.store_analysis_outcome(&key, &fresh);

        let outcome = runtime
            .analyze_track_with_backend(&request)
            .await
            .expect("fresh in-memory fallback should be returned");
        assert_eq!(
            outcome.backend,
            AnalysisBackend::CachedClassicalTransientFailure
        );
        assert_ne!(outcome.backend, AnalysisBackend::ClassicalTransientFailure);
        assert!(!outcome.backend.is_fresh_neural());
        assert!(outcome.v2_rhythm.is_none());
    }

    #[test]
    fn rejects_disallowed_prepared_http_url_before_playback() {
        let client = Client::new();
        let request = request_with_source(PreparedSource::http(
            "https://example.com/audio.webm",
            Vec::<PreparedHeader>::new().into_boxed_slice(),
            None,
            None,
        ));

        let error = match build_input(&client, &request) {
            Ok(_) => panic!("disallowed prepared URL was accepted"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            SongbirdRuntimeError::DisallowedPreparedUrl { .. }
        ));
    }

    #[test]
    fn accepts_allowed_prepared_http_url_before_playback() {
        let client = Client::new();
        let request = request_with_source(PreparedSource::http(
            "https://manifest.googlevideo.com/videoplayback",
            Vec::<PreparedHeader>::new().into_boxed_slice(),
            None,
            None,
        ));

        assert!(build_input(&client, &request).is_ok());
    }

    #[test]
    fn analysis_accepts_http_and_hls_sources() {
        let http = request_with_source(PreparedSource::http(
            "https://manifest.googlevideo.com/videoplayback",
            Vec::<PreparedHeader>::new().into_boxed_slice(),
            None,
            None,
        ));
        let hls = request_with_source_and_duration(
            PreparedSource::hls(
                "https://manifest.googlevideo.com/hls/playlist.m3u8",
                Vec::<PreparedHeader>::new().into_boxed_slice(),
                None,
            ),
            Some(Duration::from_secs(5 * 60)),
        );

        assert!(analysis_source_supported(&http));
        assert!(analysis_source_supported(&hls));
    }

    #[test]
    fn analysis_rejects_hls_without_finite_duration() {
        let hls = request_with_source(PreparedSource::hls(
            "https://manifest.googlevideo.com/hls/playlist.m3u8",
            Vec::<PreparedHeader>::new().into_boxed_slice(),
            None,
        ));

        assert!(!analysis_source_supported(&hls));
    }

    #[test]
    fn analysis_rejects_hls_over_analysis_duration_limit() {
        let hls = request_with_source_and_duration(
            PreparedSource::hls(
                "https://manifest.googlevideo.com/hls/playlist.m3u8",
                Vec::<PreparedHeader>::new().into_boxed_slice(),
                None,
            ),
            Some(MAX_ANALYSIS_DURATION + Duration::from_secs(1)),
        );

        assert!(!analysis_source_supported(&hls));
    }

    #[test]
    fn analysis_keeps_http_sources_without_duration_eligible() {
        let http = request_with_source(PreparedSource::http(
            "https://manifest.googlevideo.com/videoplayback",
            Vec::<PreparedHeader>::new().into_boxed_slice(),
            None,
            None,
        ));

        assert!(analysis_source_supported(&http));
    }

    #[test]
    fn rejects_disallowed_prepared_hls_url_before_playback() {
        let client = Client::new();
        let request = request_with_source(PreparedSource::hls(
            "https://example.com/audio.m3u8",
            Vec::<PreparedHeader>::new().into_boxed_slice(),
            None,
        ));

        let error = match build_input(&client, &request) {
            Ok(_) => panic!("disallowed prepared HLS URL was accepted"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            SongbirdRuntimeError::DisallowedPreparedUrl { .. }
        ));
    }

    #[test]
    fn accepts_allowed_prepared_hls_url_before_playback() {
        let client = Client::new();
        let request = request_with_source(PreparedSource::hls(
            "https://manifest.googlevideo.com/hls/playlist.m3u8",
            Vec::<PreparedHeader>::new().into_boxed_slice(),
            None,
        ));

        assert!(build_input(&client, &request).is_ok());
    }
}
