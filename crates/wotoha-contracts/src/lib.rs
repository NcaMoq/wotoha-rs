use std::{error::Error, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::sync::mpsc;
use wotoha_core::{
    QueuePreview, TrackRequest,
    automix::{EqTransition, TempoEnvelope, TrackAnalysis},
};

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
    /// The runtime observed the prepared incoming deck start on its target
    /// mixer frame.  This is emitted by a runtime-owned target-frame
    /// scheduler, not by the lookahead notification.
    TransitionStarted {
        guild_id: GuildKey,
        session_id: u64,
        outgoing_playback_id: PlaybackId,
        incoming_playback_id: PlaybackId,
        generation: u64,
        target_position: Duration,
        target_frame: u64,
        actual_frame: u64,
    },
    TransitionArmFailed {
        guild_id: GuildKey,
        session_id: u64,
        outgoing_playback_id: PlaybackId,
        incoming_playback_id: PlaybackId,
        generation: u64,
        target_position: Duration,
        target_frame: u64,
        actual_frame: Option<u64>,
        skew_ms: Option<u64>,
        underflow: bool,
        failure_kind: TransitionArmFailureKind,
        reason: Arc<str>,
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

/// Describes the clock that a runtime can use for a prepared two-deck
/// transition.
///
/// Songbird's public track API only exposes asynchronous control messages.  A
/// runtime must opt in explicitly before the playback layer is allowed to
/// claim that a beat-matched transition was scheduled on a shared output
/// frame.  The conservative default is `Unsupported`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameScheduledTransitionSupport {
    Unsupported,
    /// A Songbird track-position delayed event starts the prepared deck on
    /// the next mixer tick after a compensated trigger.  This is deliberately
    /// distinct from a multi-track atomic barrier.
    TrackPositionDelayedEvent,
    /// Reserved for a proven runtime-owned barrier.  Songbird does not use
    /// this variant; independent TrackHandle queues must never advertise it.
    SharedOutputFrame,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionArmFailureKind {
    DeadlineMissed,
    ActionFailed,
    StartedLate,
    Underflow,
    IdentityMismatch,
}

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

    /// Returns whether this handle can participate in a sample/frame
    /// coordinated transition with another handle from the same runtime.
    ///
    /// This is deliberately opt-in.  Implementations that only expose
    /// asynchronous pause/resume or volume commands must leave the default in
    /// place; the playback layer will then use its strict crossfade fallback.
    fn frame_scheduled_transition_support(&self) -> FrameScheduledTransitionSupport {
        FrameScheduledTransitionSupport::Unsupported
    }

    /// Legacy capability retained for source compatibility.  The playback
    /// coordinator does not use this per-track operation for BeatMatched,
    /// because two independent command queues are not atomic.
    fn arm_shared_output_frame(&self) -> bool {
        false
    }

    fn pause(&self) {}

    fn resume(&self) {}

    async fn position(&self) -> Option<Duration> {
        None
    }

    async fn seek(&self, _position: Duration) -> bool {
        false
    }

    /// Schedules source-timeline EQ automation when supported by the runtime.
    fn schedule_equalizer_transition(&self, _transition: EqTransition) -> bool {
        false
    }

    fn cancel_equalizer_transition(&self, _id: u64) {}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionArmResult {
    Armed { target_frame: u64 },
    Unsupported,
    NotReady,
    IdentityMismatch,
    DeadlineMissed,
    Underflow,
}

/// Stable identity for a prepared deck.  Backends may use `token` to bind a
/// decoder/preroll generation; playback never treats a playback id alone as
/// sufficient identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PreparedDeckToken {
    pub guild_id: GuildKey,
    pub session_id: u64,
    pub playback_id: PlaybackId,
    pub generation: u64,
    pub token: u64,
}

/// Structured arm input for runtimes that expose a target-frame scheduler.
/// The existing `VoiceRuntime::arm_transition` method remains the compatibility
/// entry point; this type keeps identity/deadline data separable for backends
/// and deterministic drivers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArmRequest {
    pub outgoing: PreparedDeckToken,
    pub incoming: PreparedDeckToken,
    pub target_position: Duration,
}

pub type ArmResult = TransitionArmResult;
pub type TransitionRuntimeEvent = PlaybackRuntimeEvent;

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
    pub source_start: Duration,
    pub tempo_envelope: Option<TempoEnvelope>,
    pub equalizer_enabled: bool,
    pub equalizer_transition: Option<EqTransition>,
}

impl Default for TrackStartOptions {
    fn default() -> Self {
        Self {
            initial_gain: 1.0,
            prefetch_after: None,
            transition_after: None,
            source_start: Duration::ZERO,
            tempo_envelope: None,
            equalizer_enabled: false,
            equalizer_transition: None,
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

    /// Arm a prepared incoming deck against an outgoing deck's runtime clock.
    /// Runtimes without a target-frame scheduler return `Unsupported`, which
    /// makes the playback layer use its ordinary Crossfade/Gapless fallback.
    #[allow(clippy::too_many_arguments)]
    async fn arm_transition(
        &self,
        _guild_id: GuildKey,
        _session_id: u64,
        _outgoing_playback_id: PlaybackId,
        _incoming_playback_id: PlaybackId,
        _target_position: Duration,
        _generation: u64,
        _events: RuntimeEventSink,
    ) -> TransitionArmResult {
        TransitionArmResult::Unsupported
    }

    async fn analyze_track(&self, _request: &TrackRequest) -> Option<TrackAnalysis> {
        None
    }

    /// Returns an already-cached analysis without starting decode work.
    fn cached_track_analysis(&self, _request: &TrackRequest) -> Option<TrackAnalysis> {
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

#[cfg(test)]
mod tests {
    use super::*;

    struct MinimalTrackHandle;

    #[async_trait]
    impl RuntimeTrackHandle for MinimalTrackHandle {
        fn stop(&self) {}

        fn set_volume(&self, _volume: f32) {}
    }

    #[test]
    fn equalizer_contract_defaults_are_backwards_compatible() {
        let options = TrackStartOptions::default();
        assert!(!options.equalizer_enabled);
        assert_eq!(options.equalizer_transition, None);

        let handle = MinimalTrackHandle;
        let transition = EqTransition {
            id: 42,
            source_start: Duration::from_secs(3),
            duration: Duration::from_secs(4),
            role: wotoha_core::automix::EqTransitionRole::Incoming,
            harmonic_compatibility: None,
        };
        assert!(!handle.schedule_equalizer_transition(transition));
        handle.cancel_equalizer_transition(transition.id);
    }
}
