use std::{
    collections::VecDeque,
    error::Error,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::warn;
use wotoha_core::{
    QueuePreview, TrackRequest,
    analysis::TrackAnalysisV2,
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

const RUNTIME_EVENT_INGRESS_CAPACITY: usize = 1024;
const RUNTIME_CRITICAL_OVERFLOW_CAPACITY: usize = 128;

/// Bounded runtime-event ingress shared by runtimes and the playback router.
///
/// Replaceable telemetry is rejected when the bounded channel is full. Critical
/// lifecycle events have a separate, fixed-size overflow queue so a busy
/// runtime cannot make track completion or disconnect state disappear. The
/// queue coalesces repeated identities and therefore remains bounded even when
/// an upstream runtime repeats the same callback.
#[derive(Clone)]
pub struct RuntimeEventSink {
    sender: mpsc::Sender<PlaybackRuntimeEvent>,
    critical_overflow: Arc<Mutex<VecDeque<PlaybackRuntimeEvent>>>,
    critical_notify: Arc<tokio::sync::Notify>,
}

pub struct RuntimeEventReceiver {
    receiver: mpsc::Receiver<PlaybackRuntimeEvent>,
    critical_overflow: Arc<Mutex<VecDeque<PlaybackRuntimeEvent>>>,
    critical_notify: Arc<tokio::sync::Notify>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeEventSendError;

impl RuntimeEventSink {
    pub fn channel() -> (Self, RuntimeEventReceiver) {
        let (sender, receiver) = mpsc::channel(RUNTIME_EVENT_INGRESS_CAPACITY);
        let critical_overflow = Arc::new(Mutex::new(VecDeque::new()));
        let critical_notify = Arc::new(tokio::sync::Notify::new());
        (
            Self {
                sender,
                critical_overflow: critical_overflow.clone(),
                critical_notify: critical_notify.clone(),
            },
            RuntimeEventReceiver {
                receiver,
                critical_overflow,
                critical_notify,
            },
        )
    }

    /// Try to enqueue one runtime event without awaiting from a runtime
    /// callback. Critical lifecycle events use the bounded overflow path when
    /// the normal ingress queue is full.
    pub fn send(&self, event: PlaybackRuntimeEvent) -> Result<(), RuntimeEventSendError> {
        match self.sender.try_send(event) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(event)) if is_critical_runtime_event(&event) => {
                let queued = {
                    let mut overflow = self
                        .critical_overflow
                        .lock()
                        .expect("runtime event overflow mutex poisoned");
                    queue_critical_event(&mut overflow, event)
                };
                self.critical_notify.notify_one();
                if !queued {
                    warn!(
                        queue_limit = RUNTIME_CRITICAL_OVERFLOW_CAPACITY,
                        "critical runtime event overflow is saturated"
                    );
                    Err(RuntimeEventSendError)
                } else {
                    Ok(())
                }
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(
                    queue_limit = RUNTIME_EVENT_INGRESS_CAPACITY,
                    "dropping replaceable runtime event because ingress is full"
                );
                Err(RuntimeEventSendError)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(RuntimeEventSendError),
        }
    }
}

impl RuntimeEventReceiver {
    pub async fn recv(&mut self) -> Option<PlaybackRuntimeEvent> {
        loop {
            if let Ok(event) = self.receiver.try_recv() {
                return Some(event);
            }
            if let Some(event) = self
                .critical_overflow
                .lock()
                .expect("runtime event overflow mutex poisoned")
                .pop_front()
            {
                return Some(event);
            }
            tokio::select! {
                event = self.receiver.recv() => return event,
                _ = self.critical_notify.notified() => {}
            }
        }
    }
}

fn is_critical_runtime_event(event: &PlaybackRuntimeEvent) -> bool {
    matches!(
        event,
        PlaybackRuntimeEvent::TrackEnded { .. }
            | PlaybackRuntimeEvent::TransitionDue { .. }
            | PlaybackRuntimeEvent::TransitionStarted { .. }
            | PlaybackRuntimeEvent::TransitionArmFailed { .. }
            | PlaybackRuntimeEvent::TrackErrored { .. }
            | PlaybackRuntimeEvent::VoiceDisconnected { .. }
    )
}

fn queue_critical_event(
    overflow: &mut VecDeque<PlaybackRuntimeEvent>,
    event: PlaybackRuntimeEvent,
) -> bool {
    if overflow
        .iter()
        .any(|queued| same_critical_identity(queued, &event))
    {
        return true;
    }
    if overflow.len() >= RUNTIME_CRITICAL_OVERFLOW_CAPACITY {
        return false;
    }
    overflow.push_back(event);
    true
}

fn same_critical_identity(left: &PlaybackRuntimeEvent, right: &PlaybackRuntimeEvent) -> bool {
    match (left, right) {
        (
            PlaybackRuntimeEvent::TrackEnded {
                guild_id: left_guild,
                session_id: left_session,
                playback_id: left_playback,
                ..
            },
            PlaybackRuntimeEvent::TrackEnded {
                guild_id: right_guild,
                session_id: right_session,
                playback_id: right_playback,
                ..
            },
        )
        | (
            PlaybackRuntimeEvent::TrackErrored {
                guild_id: left_guild,
                session_id: left_session,
                playback_id: left_playback,
                ..
            },
            PlaybackRuntimeEvent::TrackErrored {
                guild_id: right_guild,
                session_id: right_session,
                playback_id: right_playback,
                ..
            },
        ) => {
            left_guild == right_guild
                && left_session == right_session
                && left_playback == right_playback
        }
        (
            PlaybackRuntimeEvent::TransitionDue {
                guild_id: left_guild,
                session_id: left_session,
                playback_id: left_playback,
            },
            PlaybackRuntimeEvent::TransitionDue {
                guild_id: right_guild,
                session_id: right_session,
                playback_id: right_playback,
            },
        ) => {
            left_guild == right_guild
                && left_session == right_session
                && left_playback == right_playback
        }
        (
            PlaybackRuntimeEvent::TransitionStarted {
                guild_id: left_guild,
                session_id: left_session,
                outgoing_playback_id: left_outgoing,
                incoming_playback_id: left_incoming,
                generation: left_generation,
                ..
            }
            | PlaybackRuntimeEvent::TransitionArmFailed {
                guild_id: left_guild,
                session_id: left_session,
                outgoing_playback_id: left_outgoing,
                incoming_playback_id: left_incoming,
                generation: left_generation,
                ..
            },
            PlaybackRuntimeEvent::TransitionStarted {
                guild_id: right_guild,
                session_id: right_session,
                outgoing_playback_id: right_outgoing,
                incoming_playback_id: right_incoming,
                generation: right_generation,
                ..
            }
            | PlaybackRuntimeEvent::TransitionArmFailed {
                guild_id: right_guild,
                session_id: right_session,
                outgoing_playback_id: right_outgoing,
                incoming_playback_id: right_incoming,
                generation: right_generation,
                ..
            },
        ) => {
            left_guild == right_guild
                && left_session == right_session
                && left_outgoing == right_outgoing
                && left_incoming == right_incoming
                && left_generation == right_generation
        }
        (
            PlaybackRuntimeEvent::VoiceDisconnected {
                guild_id: left_guild,
                ..
            },
            PlaybackRuntimeEvent::VoiceDisconnected {
                guild_id: right_guild,
                ..
            },
        ) => left_guild == right_guild,
        _ => false,
    }
}

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

    /// Obtain the native timeline-first analysis when the runtime supports
    /// it. Implementations must reuse their existing cache/inference path;
    /// callers treat a miss as a safe V1 fallback.
    async fn analyze_track_v2(&self, _request: &TrackRequest) -> Option<TrackAnalysisV2> {
        None
    }

    /// Returns an already-cached analysis without starting decode work.
    fn cached_track_analysis(&self, _request: &TrackRequest) -> Option<TrackAnalysis> {
        None
    }

    /// Async cache lookup for playback paths. Filesystem-backed runtimes
    /// override this so synchronous decoding never blocks the voice executor.
    /// The default preserves compatibility for in-memory implementations.
    async fn cached_track_analysis_async(&self, request: &TrackRequest) -> Option<TrackAnalysis> {
        self.cached_track_analysis(request)
    }

    /// Returns a cached V2 record without starting decode or inference work.
    /// Playback uses this optional hook only for non-authoritative shadow
    /// comparisons; a miss never changes the V1 plan.
    fn cached_track_analysis_v2(&self, _request: &TrackRequest) -> Option<TrackAnalysisV2> {
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

    #[tokio::test]
    async fn runtime_event_ingress_is_bounded_but_preserves_critical_overflow() {
        let (sink, mut receiver) = RuntimeEventSink::channel();
        let replaceable = PlaybackRuntimeEvent::TrackStarted {
            guild_id: GuildKey::new(1),
            session_id: 1,
            playback_id: PlaybackId::new(1),
        };
        for _ in 0..RUNTIME_EVENT_INGRESS_CAPACITY {
            assert!(sink.send(replaceable.clone()).is_ok());
        }
        assert_eq!(sink.send(replaceable).map_err(|_| ()), Err(()));

        let critical = PlaybackRuntimeEvent::TrackEnded {
            guild_id: GuildKey::new(1),
            session_id: 1,
            playback_id: PlaybackId::new(1),
            reason: TrackEndReason::Completed,
        };
        assert!(sink.send(critical.clone()).is_ok());
        for _ in 0..RUNTIME_EVENT_INGRESS_CAPACITY {
            assert!(matches!(
                receiver.recv().await,
                Some(PlaybackRuntimeEvent::TrackStarted { .. })
            ));
        }
        assert_eq!(receiver.recv().await, Some(critical));
    }
}
