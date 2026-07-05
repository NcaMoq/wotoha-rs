use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
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
    error::JoinError,
    events::{Event, EventContext, EventData, EventHandler as VoiceEventHandler, TrackEvent},
    input::{
        HttpRequest, MakePlayableError,
        codecs::{get_codec_registry, get_probe},
    },
    tracks::{PlayMode, Track, TrackHandle},
};
use thiserror::Error;
use tracing::{info, warn};
use wotoha_contracts::{
    ChannelKey, GuildKey, PlaybackId, PlaybackRuntimeEvent, RuntimeEventSink, RuntimeTrackHandle,
    TrackEndReason, TrackStartOptions, VoiceGatewayEvent, VoiceGatewayRuntime, VoiceRuntime,
};
use wotoha_core::{
    PreparedHeader, PreparedSource, TrackRequest,
    debug::append_debug_log,
    url::{is_allowed_prepared_url, summarize_url_for_logs},
};

use crate::{
    niconico_hls::NiconicoHlsRequest, ranged_http::RangedHttpRequest,
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

#[derive(Clone)]
pub struct SongbirdRuntime {
    manager: Arc<Songbird>,
    stream_clients: Arc<HashMap<&'static str, Client>>,
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
    #[error("resolved source is not playable: {0}")]
    MakePlayable(String),
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

        Ok(Self {
            manager,
            stream_clients: Arc::new(stream_clients),
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
        guild_id: GuildKey,
        session_id: u64,
        playback_id: PlaybackId,
        request: &TrackRequest,
        events: RuntimeEventSink,
        options: TrackStartOptions,
        start_paused: bool,
    ) -> Result<Arc<dyn RuntimeTrackHandle>, SongbirdRuntimeError> {
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
        let transition_events = events.clone();
        let prefetch_events = events.clone();
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
            if let Some(delay) = options.transition_after {
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
            if let Some(delay) = options.prefetch_after {
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
                },
            )?;
            Ok(())
        })();
        if let Err(error) = listener_result {
            let _ = handle.stop();
            return Err(SongbirdRuntimeError::TrackEvent(error.to_string()));
        }
        append_debug_log(format!(
            "runtime: play_track handle registered guild_id={} session_id={} playback_id={} paused={}",
            guild_id.get(),
            session_id,
            playback_id.get(),
            start_paused
        ));

        Ok(Arc::new(SongbirdTrackHandle { handle, lifecycle }))
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
        self.register_track(
            guild_id,
            session_id,
            playback_id,
            request,
            events,
            options,
            false,
        )
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
        self.register_track(
            guild_id,
            session_id,
            playback_id,
            request,
            events,
            options,
            true,
        )
        .await
    }

    async fn disconnect_guild(&self, guild_id: GuildKey) -> Result<(), Self::Error> {
        match self.manager.remove(to_guild_id(guild_id)).await {
            Ok(()) | Err(JoinError::NoCall) => Ok(()),
            Err(error) => Err(SongbirdRuntimeError::Disconnect(error.to_string())),
        }
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
                Ok(RangedHttpRequest::new_with_headers(
                    client.clone(),
                    stream_url.to_string(),
                    headers,
                    Some(content_length),
                    range_chunk_size,
                    *range_mode,
                )
                .into())
            } else {
                let mut input =
                    HttpRequest::new_with_headers(client.clone(), stream_url.to_string(), headers);
                input.content_length = *content_length;
                Ok(input.into())
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
                Ok(
                    NiconicoHlsRequest::new(client.clone(), playlist_url.to_string(), headers)
                        .into(),
                )
            } else {
                Ok(ValidatedHlsRequest::new(
                    client.clone(),
                    request.provider_id.to_string(),
                    playlist_url.to_string(),
                    headers,
                )
                .into())
            }
        }
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
}

impl RuntimeTrackHandle for SongbirdTrackHandle {
    fn stop(&self) {
        self.lifecycle.request_stop();
        let _ = self.handle.stop();
    }

    fn set_volume(&self, volume: f32) {
        let _ = self.handle.set_volume(volume);
    }

    fn pause(&self) {
        let _ = self.handle.pause();
    }

    fn resume(&self) {
        let _ = self.handle.play();
    }
}

struct TrackEndNotifier {
    guild_id: GuildKey,
    session_id: u64,
    playback_id: PlaybackId,
    events: RuntimeEventSink,
    lifecycle: Arc<TrackLifecycle>,
}

#[serenity::async_trait]
impl VoiceEventHandler for TrackEndNotifier {
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
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
}

#[serenity::async_trait]
impl VoiceEventHandler for TrackErrorLogger {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
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
    use reqwest::Client;
    use wotoha_core::{PreparedHeader, PreparedSource, TrackMetadata, TrackRequest};

    use super::{
        SongbirdRuntimeError, TrackEndReason, TrackLifecycle, build_input,
        should_use_ranged_request,
    };

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
    fn rejects_disallowed_prepared_http_url_before_playback() {
        let client = Client::new();
        let request = TrackRequest::new(
            "youtube",
            "video-id",
            "https://www.youtube.com/watch?v=video-id",
            "https://www.youtube.com/watch?v=video-id",
            "https://www.youtube.com/watch?v=video-id",
            PreparedSource::http(
                "https://example.com/audio.webm",
                Vec::<PreparedHeader>::new().into_boxed_slice(),
                None,
                None,
            ),
            TrackMetadata::new(
                "title",
                "author",
                "https://www.youtube.com/watch?v=video-id",
                None,
                None,
            ),
        );

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
        let request = TrackRequest::new(
            "youtube",
            "video-id",
            "https://www.youtube.com/watch?v=video-id",
            "https://www.youtube.com/watch?v=video-id",
            "https://www.youtube.com/watch?v=video-id",
            PreparedSource::http(
                "https://manifest.googlevideo.com/videoplayback",
                Vec::<PreparedHeader>::new().into_boxed_slice(),
                None,
                None,
            ),
            TrackMetadata::new(
                "title",
                "author",
                "https://www.youtube.com/watch?v=video-id",
                None,
                None,
            ),
        );

        assert!(build_input(&client, &request).is_ok());
    }
}
