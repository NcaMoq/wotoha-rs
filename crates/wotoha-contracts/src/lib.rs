use std::{error::Error, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::sync::mpsc;
use wotoha_core::{QueuePreview, TrackRequest, automix::TrackAnalysis};

macro_rules! runtime_key {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self::new(value)
            }
        }
    };
}

runtime_key!(GuildKey);
runtime_key!(ChannelKey);
runtime_key!(UserKey);
runtime_key!(PlaybackId);

#[derive(Clone, Debug)]
pub struct EnqueueOutcome {
    pub now_playing: bool,
    pub request: TrackRequest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VoicePeerSnapshot {
    pub user_id: UserKey,
    pub channel_id: ChannelKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoiceUpdateDecision {
    Ignore,
    StayConnected,
    DisconnectAlone,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoiceActionAccess {
    NoActiveChannel,
    UserNotInVoice,
    SameChannel {
        channel_id: ChannelKey,
    },
    DifferentChannel {
        active_channel: ChannelKey,
        actor_channel: ChannelKey,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackEndReason {
    Completed,
    Stopped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaybackRuntimeEvent {
    TrackStarted {
        guild_id: GuildKey,
        session_id: u64,
        playback_id: PlaybackId,
    },
    TrackEnded {
        guild_id: GuildKey,
        session_id: u64,
        playback_id: PlaybackId,
        reason: TrackEndReason,
    },
    TransitionDue {
        guild_id: GuildKey,
        session_id: u64,
        playback_id: PlaybackId,
    },
    TransitionPrefetchDue {
        guild_id: GuildKey,
        session_id: u64,
        playback_id: PlaybackId,
    },
    TrackErrored {
        guild_id: GuildKey,
        session_id: u64,
        playback_id: PlaybackId,
        message: Arc<str>,
    },
    VoiceDisconnected {
        guild_id: GuildKey,
        reason: Arc<str>,
    },
}

pub type RuntimeEventSink = mpsc::UnboundedSender<PlaybackRuntimeEvent>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceGatewayStateUpdate {
    pub guild_id: GuildKey,
    pub user_id: UserKey,
    pub channel_id: Option<ChannelKey>,
    pub session_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceGatewayServerUpdate {
    pub guild_id: GuildKey,
    pub endpoint: Option<String>,
    pub token: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VoiceGatewayEvent {
    StateUpdate(VoiceGatewayStateUpdate),
    ServerUpdate(VoiceGatewayServerUpdate),
}

#[async_trait]
pub trait RuntimeTrackHandle: Send + Sync + 'static {
    fn stop(&self);
    fn set_volume(&self, volume: f32);

    fn pause(&self) {}

    fn resume(&self) {}

    async fn position(&self) -> Option<Duration> {
        None
    }

    async fn seek(&self, _position: Duration) -> bool {
        false
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaybackRestartSnapshot {
    pub current_source_url: String,
    pub queued_source_urls: Vec<String>,
    pub position: Duration,
    pub looping: bool,
    pub automix_enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackStartOptions {
    pub initial_gain: f32,
    pub prefetch_after: Option<Duration>,
    pub transition_after: Option<Duration>,
}

impl Default for TrackStartOptions {
    fn default() -> Self {
        Self {
            initial_gain: 1.0,
            prefetch_after: None,
            transition_after: None,
        }
    }
}

#[async_trait]
pub trait MediaBackend: Clone + Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    async fn resolve(&self, source_url: &str) -> Result<TrackRequest, Self::Error>;
    async fn prepare_playback(&self, request: &TrackRequest) -> Result<TrackRequest, Self::Error>;
}

#[async_trait]
pub trait VoiceRuntime: Clone + Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    async fn play_track(
        &self,
        guild_id: GuildKey,
        session_id: u64,
        playback_id: PlaybackId,
        request: &TrackRequest,
        events: RuntimeEventSink,
    ) -> Result<Arc<dyn RuntimeTrackHandle>, Self::Error>;

    async fn play_track_with_options(
        &self,
        guild_id: GuildKey,
        session_id: u64,
        playback_id: PlaybackId,
        request: &TrackRequest,
        events: RuntimeEventSink,
        options: TrackStartOptions,
    ) -> Result<Arc<dyn RuntimeTrackHandle>, Self::Error> {
        let handle = self
            .play_track(guild_id, session_id, playback_id, request, events)
            .await?;
        handle.set_volume(options.initial_gain);
        Ok(handle)
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
        let handle = self
            .play_track_with_options(guild_id, session_id, playback_id, request, events, options)
            .await?;
        handle.pause();
        Ok(handle)
    }

    async fn analyze_track(&self, _request: &TrackRequest) -> Option<TrackAnalysis> {
        None
    }

    async fn disconnect_guild(&self, guild_id: GuildKey) -> Result<(), Self::Error>;
}

#[async_trait]
pub trait VoiceGatewayRuntime: VoiceRuntime {
    async fn ensure_joined(
        &self,
        guild_id: GuildKey,
        channel_id: ChannelKey,
    ) -> Result<bool, Self::Error>;

    async fn handle_gateway_event(&self, event: VoiceGatewayEvent) -> Result<(), Self::Error>;
}

#[async_trait]
pub trait PlaybackService: Clone + Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    async fn enqueue(
        &self,
        guild_id: GuildKey,
        source_url: &str,
    ) -> Result<EnqueueOutcome, Self::Error>;

    fn queue_preview(&self, guild_id: GuildKey, limit: usize) -> Option<QueuePreview>;
    async fn toggle_loop(&self, guild_id: GuildKey) -> Option<bool>;
    async fn skip(&self, guild_id: GuildKey) -> Option<bool>;
    fn has_current_track(&self, guild_id: GuildKey) -> bool;
    async fn shuffle(&self, guild_id: GuildKey) -> bool;
    fn automix_enabled(&self, _guild_id: GuildKey) -> bool {
        false
    }
    async fn toggle_automix(&self, _guild_id: GuildKey) -> Option<bool> {
        None
    }
    async fn restart_snapshot(&self, _guild_id: GuildKey) -> Option<PlaybackRestartSnapshot> {
        None
    }
    async fn restore_restart_snapshot(
        &self,
        _guild_id: GuildKey,
        _snapshot: PlaybackRestartSnapshot,
    ) -> bool {
        false
    }
    async fn disconnect_guild(&self, guild_id: GuildKey);

    fn bootstrap_voice_state(
        &self,
        guild_id: GuildKey,
        bot_channel: ChannelKey,
        peers: Vec<VoicePeerSnapshot>,
    );

    fn update_bot_voice_channel(&self, guild_id: GuildKey, new_channel: Option<ChannelKey>);
    fn clear_voice_state(&self, guild_id: GuildKey);

    fn apply_peer_voice_state(
        &self,
        guild_id: GuildKey,
        user_id: UserKey,
        old_channel: Option<ChannelKey>,
        new_channel: Option<ChannelKey>,
    ) -> VoiceUpdateDecision;

    fn voice_action_access(
        &self,
        guild_id: GuildKey,
        actor_channel: Option<ChannelKey>,
    ) -> VoiceActionAccess;
}
