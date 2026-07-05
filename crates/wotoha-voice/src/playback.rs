use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use dashmap::DashMap;
use parking_lot::Mutex;
use thiserror::Error;
use tokio::sync::{
    Mutex as AsyncMutex,
    mpsc::{UnboundedReceiver, unbounded_channel},
    oneshot,
};
use tracing::{info, warn};
use wotoha_contracts::{
    ChannelKey, EnqueueOutcome, GuildKey, MediaBackend, PlaybackId, PlaybackRestartSnapshot,
    PlaybackRuntimeEvent, PlaybackService, RuntimeEventSink, RuntimeTrackHandle, TrackEndReason,
    TrackStartOptions, UserKey, VoiceActionAccess, VoicePeerSnapshot, VoiceRuntime,
    VoiceUpdateDecision,
};
use wotoha_core::{
    GuildPlayerState, QueuePreview, TrackRequest,
    automix::{
        AutoMixConfig, TrackAnalysis, TransitionTiming, plan_transition, plan_transition_timing,
    },
    debug::append_debug_log,
};

type CompletionSender<ME, RE> = oneshot::Sender<Result<EnqueueOutcome, PlaybackError<ME, RE>>>;
type SessionHandle<ME, RE> = Arc<GuildSession<ME, RE>>;

const MAX_QUEUE_LEN: usize = 512;
const MAX_PENDING_ENQUEUES: usize = 64;
const BEAT_ALIGNMENT_LOOKAHEAD: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Clone)]
pub struct PlaybackCoordinator<M: MediaBackend, R: VoiceRuntime> {
    inner: Arc<PlaybackCoordinatorInner<M, R>>,
}

struct PlaybackCoordinatorInner<M: MediaBackend, R: VoiceRuntime> {
    media: M,
    runtime: R,
    events: RuntimeEventSink,
    sessions: DashMap<GuildKey, SessionHandle<M::Error, R::Error>>,
    next_session_id: AtomicU64,
    next_playback_id: AtomicU64,
    automix: AutoMixConfig,
}

struct GuildSession<ME, RE>
where
    ME: std::error::Error + Send + Sync + 'static,
    RE: std::error::Error + Send + Sync + 'static,
{
    id: u64,
    operation: AsyncMutex<()>,
    playback: Mutex<PlaybackRuntime<ME, RE>>,
    voice: Mutex<GuildVoiceIndex>,
}

struct PlaybackRuntime<ME, RE>
where
    ME: std::error::Error + Send + Sync + 'static,
    RE: std::error::Error + Send + Sync + 'static,
{
    logical: GuildPlayerState,
    active_handle: Option<Arc<dyn RuntimeTrackHandle>>,
    current_playback_id: Option<PlaybackId>,
    retiring_handle: Option<(PlaybackId, Arc<dyn RuntimeTrackHandle>)>,
    fade_abort: Option<tokio::task::AbortHandle>,
    automix_enabled: bool,
    prefetch_generation: u64,
    transition_due: Option<PlaybackId>,
    prepared_transition: Option<PreparedTransition>,
    current_analysis: Option<TrackAnalysis>,
    next_enqueue_seq: u64,
    next_commit_seq: u64,
    pending_enqueues: BTreeMap<u64, PendingEnqueue<ME, RE>>,
}

struct PendingEnqueue<ME, RE>
where
    ME: std::error::Error + Send + Sync + 'static,
    RE: std::error::Error + Send + Sync + 'static,
{
    outcome: Option<Result<TrackRequest, PlaybackError<ME, RE>>>,
    completion: Option<CompletionSender<ME, RE>>,
}

struct PreparedTransition {
    origin_playback_id: PlaybackId,
    next: TrackRequest,
    incoming: StartedTrack,
    timing: TransitionTiming,
    start_delay: std::time::Duration,
    incoming_analysis: Option<TrackAnalysis>,
}

#[derive(Default)]
struct GuildVoiceIndex {
    bot_channel: Option<ChannelKey>,
    peers_by_user: HashMap<UserKey, ChannelKey>,
    peer_counts_by_channel: HashMap<ChannelKey, usize>,
    bootstrapped: bool,
}

#[derive(Debug, Error)]
pub enum PlaybackError<ME, RE>
where
    ME: std::error::Error + Send + Sync + 'static,
    RE: std::error::Error + Send + Sync + 'static,
{
    #[error(transparent)]
    Resolve(ME),
    #[error(transparent)]
    Runtime(RE),
    #[error("session state changed while processing the request")]
    SessionExpired,
    #[error("queue is full for this guild")]
    QueueFull,
}

enum FlushAction<ME, RE>
where
    ME: std::error::Error + Send + Sync + 'static,
    RE: std::error::Error + Send + Sync + 'static,
{
    WaitForEarlier,
    CommitQueued {
        completion: Option<CompletionSender<ME, RE>>,
        request: TrackRequest,
    },
    Fail {
        completion: Option<CompletionSender<ME, RE>>,
        error: PlaybackError<ME, RE>,
    },
    StartCurrent {
        completion: Option<CompletionSender<ME, RE>>,
        request: TrackRequest,
    },
}

impl<ME, RE> Default for PlaybackRuntime<ME, RE>
where
    ME: std::error::Error + Send + Sync + 'static,
    RE: std::error::Error + Send + Sync + 'static,
{
    fn default() -> Self {
        Self {
            logical: GuildPlayerState::default(),
            active_handle: None,
            current_playback_id: None,
            retiring_handle: None,
            fade_abort: None,
            automix_enabled: false,
            prefetch_generation: 0,
            transition_due: None,
            prepared_transition: None,
            current_analysis: None,
            next_enqueue_seq: 0,
            next_commit_seq: 0,
            pending_enqueues: BTreeMap::new(),
        }
    }
}

impl<M, R> PlaybackCoordinator<M, R>
where
    M: MediaBackend,
    R: VoiceRuntime,
{
    pub fn new(media: M, runtime: R) -> Self {
        Self::new_with_automix(
            media,
            runtime,
            AutoMixConfig {
                enabled: false,
                crossfade: std::time::Duration::ZERO,
                max_tempo_adjustment: 0.0,
                min_beat_confidence: 1.0,
            },
        )
    }

    pub fn new_with_automix(media: M, runtime: R, automix: AutoMixConfig) -> Self {
        let (events, receiver) = unbounded_channel();
        let playback = Self {
            inner: Arc::new(PlaybackCoordinatorInner {
                media,
                runtime,
                events,
                sessions: DashMap::new(),
                next_session_id: AtomicU64::new(1),
                next_playback_id: AtomicU64::new(1),
                automix,
            }),
        };
        playback.spawn_runtime_event_loop(receiver);
        playback
    }

    fn spawn_runtime_event_loop(&self, mut receiver: UnboundedReceiver<PlaybackRuntimeEvent>) {
        let playback = self.clone();
        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                match event {
                    PlaybackRuntimeEvent::TrackStarted { .. } => {}
                    PlaybackRuntimeEvent::TransitionDue {
                        guild_id,
                        session_id,
                        playback_id,
                    } => {
                        let playback = playback.clone();
                        tokio::spawn(async move {
                            playback.transition(guild_id, session_id, playback_id).await
                        });
                    }
                    PlaybackRuntimeEvent::TransitionPrefetchDue {
                        guild_id,
                        session_id,
                        playback_id,
                    } => {
                        let playback = playback.clone();
                        tokio::spawn(async move {
                            playback
                                .prefetch_transition(guild_id, session_id, playback_id)
                                .await
                        });
                    }
                    PlaybackRuntimeEvent::TrackEnded {
                        guild_id,
                        session_id,
                        playback_id,
                        reason,
                    } => {
                        playback
                            .advance(guild_id, session_id, playback_id, reason)
                            .await
                    }
                    PlaybackRuntimeEvent::TrackErrored {
                        guild_id,
                        session_id,
                        playback_id,
                        message,
                    } => {
                        playback
                            .handle_track_error(guild_id, session_id, playback_id, message.as_ref())
                            .await
                    }
                    PlaybackRuntimeEvent::VoiceDisconnected { guild_id, reason } => {
                        warn!(
                            guild_id = guild_id.get(),
                            reason = reason.as_ref(),
                            "runtime reported voice disconnect"
                        );
                        playback.disconnect_guild(guild_id).await;
                    }
                }
            }
        });
    }

    fn next_playback_id(&self) -> PlaybackId {
        PlaybackId::new(self.inner.next_playback_id.fetch_add(1, Ordering::Relaxed))
    }

    fn get_session(&self, guild_id: GuildKey) -> Option<SessionHandle<M::Error, R::Error>> {
        self.inner
            .sessions
            .get(&guild_id)
            .map(|entry| entry.clone())
    }

    fn get_or_create_session(&self, guild_id: GuildKey) -> SessionHandle<M::Error, R::Error> {
        self.inner
            .sessions
            .entry(guild_id)
            .or_insert_with(|| {
                let mut playback = PlaybackRuntime::default();
                playback.automix_enabled = self.inner.automix.enabled;
                Arc::new(GuildSession {
                    id: self.inner.next_session_id.fetch_add(1, Ordering::Relaxed),
                    operation: AsyncMutex::new(()),
                    playback: Mutex::new(playback),
                    voice: Mutex::new(GuildVoiceIndex::default()),
                })
            })
            .clone()
    }

    fn session_is_current(&self, guild_id: GuildKey, session_id: u64) -> bool {
        let Some(session) = self.get_session(guild_id) else {
            return false;
        };

        session.id == session_id
    }

    async fn enqueue_impl(
        &self,
        guild_id: GuildKey,
        source_url: &str,
    ) -> Result<EnqueueOutcome, PlaybackError<M::Error, R::Error>> {
        let session = self.get_or_create_session(guild_id);
        let session_id = session.id;

        let (sequence, completion) = {
            let _operation = session.operation.lock().await;
            let mut playback = session.playback.lock();
            if playback.pending_enqueues.len() >= MAX_PENDING_ENQUEUES
                || playback.logical.queue().len() >= MAX_QUEUE_LEN
            {
                return Err(PlaybackError::QueueFull);
            }
            let sequence = playback.next_enqueue_seq;
            playback.next_enqueue_seq += 1;
            let (tx, rx) = oneshot::channel();
            playback.pending_enqueues.insert(
                sequence,
                PendingEnqueue {
                    outcome: None,
                    completion: Some(tx),
                },
            );
            (sequence, rx)
        };

        let resolved = self
            .inner
            .media
            .resolve(source_url)
            .await
            .map_err(PlaybackError::Resolve);
        match &resolved {
            Ok(request) => append_debug_log(format!(
                "voice: media.resolve ok guild_id={} provider={} key={} title={}",
                guild_id.get(),
                request.provider_id.as_ref(),
                request.canonical_key.as_ref(),
                request.metadata.title.as_ref()
            )),
            Err(error) => append_debug_log(format!(
                "voice: media.resolve failed guild_id={} error={error}",
                guild_id.get()
            )),
        }
        self.finish_enqueue(guild_id, session.clone(), session_id, sequence, resolved)
            .await?;

        completion
            .await
            .unwrap_or(Err(PlaybackError::SessionExpired))
    }

    pub fn queue_preview(&self, guild_id: GuildKey, limit: usize) -> Option<QueuePreview> {
        let session = self.get_session(guild_id)?;
        let playback = session.playback.lock();
        Some(playback.logical.queue_preview(limit))
    }

    pub async fn toggle_loop(&self, guild_id: GuildKey) -> Option<bool> {
        let session = self.get_session(guild_id)?;
        let _operation = session.operation.lock().await;
        let (enabled, prepared) = {
            let mut playback = session.playback.lock();
            if playback.automix_enabled {
                playback.automix_enabled = false;
                playback.logical.disable_loop();
            }
            let prepared = invalidate_prepared_transition(&mut playback);
            (playback.logical.toggle_loop(), prepared)
        };
        if let Some(handle) = prepared {
            handle.stop();
        }
        Some(enabled)
    }

    pub async fn skip(&self, guild_id: GuildKey) -> Option<bool> {
        let session = self.get_session(guild_id)?;
        let _operation = session.operation.lock().await;

        let (was_looping, handle, retiring, fade_abort, prepared) = {
            let mut playback = session.playback.lock();
            let was_looping = playback.logical.disable_loop();
            let handle = playback.active_handle.take();
            let retiring = playback.retiring_handle.take().map(|(_, handle)| handle);
            let fade_abort = playback.fade_abort.take();
            let prepared = invalidate_prepared_transition(&mut playback);
            (was_looping, handle, retiring, fade_abort, prepared)
        };

        if let Some(abort) = fade_abort {
            abort.abort();
        }

        if let Some(handle) = handle {
            handle.stop();
        }
        if let Some(handle) = retiring {
            handle.stop();
        }
        if let Some(handle) = prepared {
            handle.stop();
        }

        Some(was_looping)
    }

    pub fn has_current_track(&self, guild_id: GuildKey) -> bool {
        let Some(session) = self.get_session(guild_id) else {
            return false;
        };

        let playback = session.playback.lock();
        playback.logical.current().is_some()
    }

    pub async fn shuffle(&self, guild_id: GuildKey) -> bool {
        let Some(session) = self.get_session(guild_id) else {
            return false;
        };

        let _operation = session.operation.lock().await;
        let (shuffled, prepared) = {
            let mut playback = session.playback.lock();
            let shuffled = playback.logical.shuffle();
            let prepared = shuffled
                .then(|| invalidate_prepared_transition(&mut playback))
                .flatten();
            (shuffled, prepared)
        };
        if let Some(handle) = prepared {
            handle.stop();
        }
        shuffled
    }

    pub async fn toggle_automix(&self, guild_id: GuildKey) -> Option<bool> {
        let session = self.get_session(guild_id)?;
        let _operation = session.operation.lock().await;
        let (enabled, prepared) = {
            let mut playback = session.playback.lock();
            playback.automix_enabled = !playback.automix_enabled;
            if playback.automix_enabled {
                playback.logical.disable_loop();
            }
            let prepared = invalidate_prepared_transition(&mut playback);
            (playback.automix_enabled, prepared)
        };
        if let Some(handle) = prepared {
            handle.stop();
        }
        Some(enabled)
    }

    pub fn automix_enabled(&self, guild_id: GuildKey) -> bool {
        self.get_session(guild_id)
            .is_some_and(|session| session.playback.lock().automix_enabled)
    }

    pub async fn restart_snapshot(&self, guild_id: GuildKey) -> Option<PlaybackRestartSnapshot> {
        let session = self.get_session(guild_id)?;
        let (handle, current_source_url, queued_source_urls, looping, automix_enabled) = {
            let playback = session.playback.lock();
            let current = playback.logical.current()?;
            (
                playback.active_handle.clone()?,
                current.source_url.to_string(),
                playback
                    .logical
                    .queue()
                    .iter()
                    .map(|track| track.source_url.to_string())
                    .collect(),
                playback.logical.is_looping(),
                playback.automix_enabled,
            )
        };
        Some(PlaybackRestartSnapshot {
            current_source_url,
            queued_source_urls,
            position: tokio::time::timeout(std::time::Duration::from_secs(2), handle.position())
                .await
                .ok()
                .flatten()
                .unwrap_or_default(),
            looping,
            automix_enabled,
        })
    }

    pub async fn restore_restart_snapshot(
        &self,
        guild_id: GuildKey,
        snapshot: PlaybackRestartSnapshot,
    ) -> bool {
        if self
            .enqueue_impl(guild_id, &snapshot.current_source_url)
            .await
            .is_err()
        {
            return false;
        }
        let handle = self
            .get_session(guild_id)
            .and_then(|session| session.playback.lock().active_handle.clone());
        if let Some(handle) = handle
            && !snapshot.position.is_zero()
        {
            let _ = handle.seek(snapshot.position).await;
        }
        for source_url in snapshot.queued_source_urls {
            let _ = self.enqueue_impl(guild_id, &source_url).await;
        }
        if let Some(session) = self.get_session(guild_id) {
            let _operation = session.operation.lock().await;
            let mut playback = session.playback.lock();
            playback.automix_enabled = snapshot.automix_enabled;
            playback.logical.disable_loop();
            if snapshot.looping && !snapshot.automix_enabled {
                playback.logical.toggle_loop();
            }
        }
        true
    }

    pub async fn disconnect_guild(&self, guild_id: GuildKey) {
        let session = self
            .inner
            .sessions
            .remove(&guild_id)
            .map(|(_, session)| session);

        if let Some(session) = session {
            let (handle, retiring, fade_abort, prepared, pending) = {
                let mut playback = session.playback.lock();
                playback.logical.clear();
                let handle = playback.active_handle.take();
                let retiring = playback.retiring_handle.take().map(|(_, handle)| handle);
                let fade_abort = playback.fade_abort.take();
                let prepared = invalidate_prepared_transition(&mut playback);
                playback.current_playback_id = None;
                let pending = drain_pending(&mut playback);
                (handle, retiring, fade_abort, prepared, pending)
            };
            session.voice.lock().clear();

            if let Some(abort) = fade_abort {
                abort.abort();
            }
            if let Some(handle) = handle {
                handle.stop();
            }
            if let Some(handle) = retiring {
                handle.stop();
            }
            if let Some(handle) = prepared {
                handle.stop();
            }
            complete_all(pending, || PlaybackError::SessionExpired);
        }

        if let Err(error) = self.inner.runtime.disconnect_guild(guild_id).await {
            warn!(guild_id = guild_id.get(), error = %error, "failed to disconnect runtime session");
        }
    }

    pub fn bootstrap_voice_state(
        &self,
        guild_id: GuildKey,
        bot_channel: ChannelKey,
        peers: Vec<VoicePeerSnapshot>,
    ) {
        let session = self.get_or_create_session(guild_id);
        let mut voice = session.voice.lock();
        voice.bootstrap(bot_channel, peers);
    }

    pub fn update_bot_voice_channel(&self, guild_id: GuildKey, new_channel: Option<ChannelKey>) {
        match new_channel {
            Some(channel_id) => {
                let session = self.get_or_create_session(guild_id);
                session.voice.lock().update_bot_channel(Some(channel_id));
            }
            None => self.clear_voice_state(guild_id),
        }
    }

    pub fn clear_voice_state(&self, guild_id: GuildKey) {
        let Some(session) = self.get_session(guild_id) else {
            return;
        };

        session.voice.lock().clear();
    }

    pub fn apply_peer_voice_state(
        &self,
        guild_id: GuildKey,
        user_id: UserKey,
        old_channel: Option<ChannelKey>,
        new_channel: Option<ChannelKey>,
    ) -> VoiceUpdateDecision {
        let Some(session) = self.get_session(guild_id) else {
            return VoiceUpdateDecision::Ignore;
        };

        let mut voice = session.voice.lock();
        voice.apply_peer_update(user_id, old_channel, new_channel)
    }

    pub fn voice_action_access(
        &self,
        guild_id: GuildKey,
        actor_channel: Option<ChannelKey>,
    ) -> VoiceActionAccess {
        let Some(session) = self.get_session(guild_id) else {
            return VoiceActionAccess::NoActiveChannel;
        };

        session.voice.lock().action_access(actor_channel)
    }

    async fn finish_enqueue(
        &self,
        guild_id: GuildKey,
        session: SessionHandle<M::Error, R::Error>,
        session_id: u64,
        sequence: u64,
        resolved: Result<TrackRequest, PlaybackError<M::Error, R::Error>>,
    ) -> Result<(), PlaybackError<M::Error, R::Error>> {
        let _operation = session.operation.lock().await;

        if !self.session_is_current(guild_id, session_id) {
            let completion = remove_pending(&session, sequence);
            if let Some(completion) = completion {
                let _ = completion.send(Err(PlaybackError::SessionExpired));
            }
            return Err(PlaybackError::SessionExpired);
        }

        {
            let mut playback = session.playback.lock();
            let Some(pending) = playback.pending_enqueues.get_mut(&sequence) else {
                return Err(PlaybackError::SessionExpired);
            };
            pending.outcome = Some(resolved);
        }

        self.flush_ready_prefix(guild_id, &session, session_id)
            .await;
        Ok(())
    }

    async fn flush_ready_prefix(
        &self,
        guild_id: GuildKey,
        session: &SessionHandle<M::Error, R::Error>,
        session_id: u64,
    ) {
        loop {
            let action = {
                let mut playback = session.playback.lock();
                take_flush_action(&mut playback)
            };

            match action {
                FlushAction::WaitForEarlier => return,
                FlushAction::CommitQueued {
                    mut completion,
                    request,
                } => {
                    if let Some(completion) = completion.take() {
                        let _ = completion.send(Ok(EnqueueOutcome {
                            now_playing: false,
                            request,
                        }));
                    }
                }
                FlushAction::Fail {
                    mut completion,
                    error,
                } => {
                    if let Some(completion) = completion.take() {
                        let _ = completion.send(Err(error));
                    }
                }
                FlushAction::StartCurrent {
                    mut completion,
                    request,
                } => match self.play_request(guild_id, session_id, &request, 1.0).await {
                    Ok(handle) => {
                        let mut playback = session.playback.lock();
                        playback.current_playback_id = Some(handle.playback_id);
                        playback.active_handle = Some(handle.handle);
                        if let Some(completion) = completion.take() {
                            let _ = completion.send(Ok(EnqueueOutcome {
                                now_playing: true,
                                request,
                            }));
                        }
                    }
                    Err(PlaybackError::SessionExpired) => {
                        {
                            let mut playback = session.playback.lock();
                            playback.logical.clear_current();
                            playback.current_playback_id = None;
                            let pending = drain_pending(&mut playback);
                            drop(playback);
                            if let Some(completion) = completion.take() {
                                let _ = completion.send(Err(PlaybackError::SessionExpired));
                            }
                            complete_all(pending, || PlaybackError::SessionExpired);
                        }
                        return;
                    }
                    Err(PlaybackError::Runtime(error)) => {
                        let mut playback = session.playback.lock();
                        playback.logical.clear_current();
                        playback.current_playback_id = None;
                        drop(playback);
                        if let Some(completion) = completion.take() {
                            let _ = completion.send(Err(PlaybackError::Runtime(error)));
                        }
                    }
                    Err(PlaybackError::Resolve(error)) => {
                        let mut playback = session.playback.lock();
                        playback.logical.clear_current();
                        playback.current_playback_id = None;
                        drop(playback);
                        if let Some(completion) = completion.take() {
                            let _ = completion.send(Err(PlaybackError::Resolve(error)));
                        }
                    }
                    Err(PlaybackError::QueueFull) => {
                        let mut playback = session.playback.lock();
                        playback.logical.clear_current();
                        playback.current_playback_id = None;
                        drop(playback);
                        if let Some(completion) = completion.take() {
                            let _ = completion.send(Err(PlaybackError::QueueFull));
                        }
                    }
                },
            }
        }
    }

    async fn advance(
        &self,
        guild_id: GuildKey,
        session_id: u64,
        playback_id: PlaybackId,
        reason: TrackEndReason,
    ) {
        let Some(session) = self.get_session(guild_id) else {
            return;
        };
        if session.id != session_id {
            return;
        }

        let _operation = session.operation.lock().await;
        if !self.session_is_current(guild_id, session_id) {
            return;
        }

        let failed_prefetch = {
            let mut playback = session.playback.lock();
            let matches_prefetch = playback
                .prepared_transition
                .as_ref()
                .is_some_and(|prepared| prepared.incoming.playback_id == playback_id);
            if matches_prefetch {
                playback.prefetch_generation = playback.prefetch_generation.wrapping_add(1);
                playback
                    .prepared_transition
                    .take()
                    .map(|prepared| prepared.incoming.handle)
            } else {
                None
            }
        };
        if let Some(handle) = failed_prefetch {
            handle.stop();
            return;
        }

        info!(
            guild_id = guild_id.get(),
            session_id,
            playback_id = playback_id.get(),
            reason = ?reason,
            "runtime reported track end"
        );

        let mut expected_playback_id = Some(playback_id);

        loop {
            let (next, prepared) = {
                let mut playback = session.playback.lock();
                if playback.current_playback_id != expected_playback_id {
                    return;
                }
                playback.active_handle = None;
                playback.current_playback_id = None;
                playback.current_analysis = None;
                let prepared = invalidate_prepared_transition(&mut playback);
                (playback.logical.prepare_next_track(), prepared)
            };
            if let Some(handle) = prepared {
                handle.stop();
            }

            let Some(next) = next else {
                return;
            };

            match self.play_request(guild_id, session_id, &next, 1.0).await {
                Ok(handle) => {
                    let mut playback = session.playback.lock();
                    playback.logical.replace_current(next);
                    playback.current_playback_id = Some(handle.playback_id);
                    playback.active_handle = Some(handle.handle);
                    return;
                }
                Err(PlaybackError::SessionExpired) => return,
                Err(PlaybackError::Runtime(error)) => {
                    warn!(guild_id = guild_id.get(), error = %error, "runtime failed to start next track");
                    let mut playback = session.playback.lock();
                    playback.logical.disable_loop();
                    playback.logical.clear_current();
                    playback.current_playback_id = None;
                    expected_playback_id = None;
                }
                Err(error) => {
                    warn!(guild_id = guild_id.get(), error = %error, "failed to start next track");
                    let mut playback = session.playback.lock();
                    playback.logical.disable_loop();
                    playback.logical.clear_current();
                    playback.current_playback_id = None;
                    expected_playback_id = None;
                }
            }
        }
    }

    async fn handle_track_error(
        &self,
        guild_id: GuildKey,
        session_id: u64,
        playback_id: PlaybackId,
        message: &str,
    ) {
        warn!(
            guild_id = guild_id.get(),
            session_id,
            playback_id = playback_id.get(),
            message,
            "runtime reported track error"
        );
        self.advance(guild_id, session_id, playback_id, TrackEndReason::Completed)
            .await;
    }

    async fn prefetch_transition(
        &self,
        guild_id: GuildKey,
        session_id: u64,
        playback_id: PlaybackId,
    ) {
        let Some(session) = self.get_session(guild_id) else {
            return;
        };
        let (generation, next, timing, event_after, outgoing_analysis) = {
            let _operation = session.operation.lock().await;
            if !self.session_is_current(guild_id, session_id) {
                return;
            }
            let mut playback = session.playback.lock();
            if playback.current_playback_id != Some(playback_id) {
                return;
            }
            if !playback.automix_enabled || playback.transition_due == Some(playback_id) {
                return;
            }
            let Some(outgoing_duration) = playback
                .logical
                .current()
                .and_then(|track| track.metadata.duration)
            else {
                return;
            };
            let Some(next) = playback.logical.peek_next_track().cloned() else {
                return;
            };
            let Some(incoming_duration) = next.metadata.duration else {
                return;
            };
            let Some(timing) = plan_transition_timing(
                outgoing_duration,
                incoming_duration,
                self.inner.automix.crossfade,
            ) else {
                return;
            };
            let Some(scheduled) = track_transition_timing(&self.inner.automix, outgoing_duration)
            else {
                return;
            };
            playback.prefetch_generation = playback.prefetch_generation.wrapping_add(1);
            let generation = playback.prefetch_generation;
            let event_after = scheduled
                .transition_after
                .saturating_sub(BEAT_ALIGNMENT_LOOKAHEAD);
            (
                generation,
                next,
                timing,
                event_after,
                playback.current_analysis.clone(),
            )
        };

        let prepared = match self.inner.media.prepare_playback(&next).await {
            Ok(prepared) => prepared,
            Err(error) => {
                warn!(guild_id = guild_id.get(), error = %error, "AutoMix failed to prefetch next track");
                return;
            }
        };
        if !self.session_is_current(guild_id, session_id) {
            return;
        }
        {
            let playback = session.playback.lock();
            if playback.current_playback_id != Some(playback_id)
                || playback.prefetch_generation != generation
                || playback.transition_due == Some(playback_id)
                || !playback.automix_enabled
            {
                return;
            }
        }
        let incoming_id = self.next_playback_id();
        let options = track_start_options(&self.inner.automix, prepared.metadata.duration, 0.0);
        let (incoming_result, incoming_analysis) = tokio::join!(
            self.inner.runtime.prepare_track_with_options(
                guild_id,
                session_id,
                incoming_id,
                &prepared,
                self.inner.events.clone(),
                options,
            ),
            self.inner.runtime.analyze_track(&prepared),
        );
        let incoming = match incoming_result {
            Ok(handle) => StartedTrack {
                playback_id: incoming_id,
                handle,
            },
            Err(error) => {
                warn!(guild_id = guild_id.get(), error = %error, "AutoMix runtime prefetch failed");
                return;
            }
        };
        let mut timing = timing;
        let mut incoming_start = std::time::Duration::ZERO;
        let mut transition_after = timing.transition_after;
        if let (Some(outgoing), Some(incoming_analysis)) =
            (outgoing_analysis.as_ref(), incoming_analysis.as_ref())
        {
            let plan = plan_transition(outgoing, incoming_analysis, &self.inner.automix);
            if !plan.duration.is_zero() {
                timing.fade_duration = plan.duration;
                transition_after = plan.outgoing_start;
                incoming_start = plan.incoming_start;
            }
        }
        if !incoming_start.is_zero() {
            let _ = incoming.handle.seek(incoming_start).await;
        }
        let start_delay = transition_after.saturating_sub(event_after);

        let mut incoming = Some(incoming);
        {
            let _operation = session.operation.lock().await;
            let mut playback = session.playback.lock();
            let valid = self.session_is_current(guild_id, session_id)
                && playback.current_playback_id == Some(playback_id)
                && playback.automix_enabled
                && playback.prefetch_generation == generation
                && playback.transition_due != Some(playback_id)
                && playback.logical.peek_next_track() == Some(&next);
            if valid {
                if let Some(previous) = playback.prepared_transition.replace(PreparedTransition {
                    origin_playback_id: playback_id,
                    next,
                    incoming: incoming.take().expect("prefetched track is available"),
                    timing,
                    start_delay,
                    incoming_analysis,
                }) {
                    previous.incoming.handle.stop();
                }
            }
        }
        if let Some(incoming) = incoming {
            incoming.handle.stop();
        }
    }

    async fn transition(&self, guild_id: GuildKey, session_id: u64, playback_id: PlaybackId) {
        let Some(session) = self.get_session(guild_id) else {
            return;
        };
        let start_delay = {
            let _operation = session.operation.lock().await;
            if !self.session_is_current(guild_id, session_id) {
                return;
            }
            let mut playback = session.playback.lock();
            if playback.current_playback_id != Some(playback_id)
                || !playback.automix_enabled
                || playback.transition_due == Some(playback_id)
            {
                return;
            }
            playback.transition_due = Some(playback_id);
            playback.prefetch_generation = playback.prefetch_generation.wrapping_add(1);
            playback
                .prepared_transition
                .as_ref()
                .filter(|prepared| prepared.origin_playback_id == playback_id)
                .map(|prepared| prepared.start_delay)
        };
        let Some(start_delay) = start_delay else {
            return;
        };
        if !start_delay.is_zero() {
            tokio::time::sleep(start_delay).await;
        }

        let _operation = session.operation.lock().await;
        let (prepared, outgoing, previous_retiring) = {
            let mut playback = session.playback.lock();
            let invalid = !self.session_is_current(guild_id, session_id)
                || playback.current_playback_id != Some(playback_id)
                || !playback.automix_enabled
                || playback.transition_due != Some(playback_id);
            if invalid {
                if let Some(stale) = playback.prepared_transition.take() {
                    stale.incoming.handle.stop();
                }
                return;
            }
            let Some(prepared) = playback.prepared_transition.take() else {
                return;
            };
            if prepared.origin_playback_id != playback_id
                || playback.logical.peek_next_track() != Some(&prepared.next)
            {
                prepared.incoming.handle.stop();
                return;
            }
            let Some(outgoing) = playback
                .active_handle
                .replace(prepared.incoming.handle.clone())
            else {
                prepared.incoming.handle.stop();
                return;
            };
            let previous_retiring = playback
                .retiring_handle
                .replace((playback_id, outgoing.clone()));
            playback.logical.prepare_next_track();
            playback.logical.replace_current(prepared.next.clone());
            playback.current_playback_id = Some(prepared.incoming.playback_id);
            playback.current_analysis = prepared.incoming_analysis.clone();
            playback.transition_due = None;
            (prepared, outgoing, previous_retiring)
        };
        if let Some((_, handle)) = previous_retiring {
            handle.stop();
        }
        prepared.incoming.handle.resume();

        let coordinator = self.clone();
        let fade_task = tokio::spawn(async move {
            run_linear_fade(
                outgoing.clone(),
                prepared.incoming.handle,
                prepared.timing.fade_duration,
            )
            .await;
            outgoing.stop();
            if let Some(session) = coordinator.get_session(guild_id)
                && session.id == session_id
            {
                let mut playback = session.playback.lock();
                if playback
                    .retiring_handle
                    .as_ref()
                    .is_some_and(|(id, _)| *id == playback_id)
                {
                    playback.retiring_handle = None;
                }
            }
        });
        session.playback.lock().fade_abort = Some(fade_task.abort_handle());
        drop(_operation);
    }

    async fn play_request(
        &self,
        guild_id: GuildKey,
        session_id: u64,
        request: &TrackRequest,
        initial_gain: f32,
    ) -> Result<StartedTrack, PlaybackError<M::Error, R::Error>> {
        if !self.session_is_current(guild_id, session_id) {
            return Err(PlaybackError::SessionExpired);
        }

        let playback_id = self.next_playback_id();
        let prepared = self
            .inner
            .media
            .prepare_playback(request)
            .await
            .map_err(PlaybackError::Resolve)?;
        append_debug_log(format!(
            "voice: prepare_playback ok guild_id={} session_id={} provider={} key={} title={}",
            guild_id.get(),
            session_id,
            prepared.provider_id.as_ref(),
            prepared.canonical_key.as_ref(),
            prepared.metadata.title.as_ref()
        ));
        if !self.session_is_current(guild_id, session_id) {
            return Err(PlaybackError::SessionExpired);
        }

        info!(
            guild_id = guild_id.get(),
            session_id,
            provider_id = prepared.provider_id.as_ref(),
            canonical_key = prepared.canonical_key.as_ref(),
            title = prepared.metadata.title.as_ref(),
            "prepared track for runtime playback"
        );
        let handle = self
            .inner
            .runtime
            .play_track_with_options(
                guild_id,
                session_id,
                playback_id,
                &prepared,
                self.inner.events.clone(),
                track_start_options(
                    &self.inner.automix,
                    prepared.metadata.duration,
                    initial_gain,
                ),
            )
            .await
            .map_err(PlaybackError::Runtime)?;
        append_debug_log(format!(
            "voice: runtime.play_track ok guild_id={} session_id={} playback_id={} provider={} key={} title={}",
            guild_id.get(),
            session_id,
            playback_id.get(),
            prepared.provider_id.as_ref(),
            prepared.canonical_key.as_ref(),
            prepared.metadata.title.as_ref()
        ));
        info!(
            guild_id = guild_id.get(),
            session_id,
            playback_id = playback_id.get(),
            provider_id = prepared.provider_id.as_ref(),
            canonical_key = prepared.canonical_key.as_ref(),
            title = prepared.metadata.title.as_ref(),
            "runtime accepted track"
        );

        if !self.session_is_current(guild_id, session_id) {
            handle.stop();
            return Err(PlaybackError::SessionExpired);
        }

        let coordinator = self.clone();
        let analysis_request = prepared.clone();
        tokio::spawn(async move {
            if let Some(analysis) = coordinator
                .inner
                .runtime
                .analyze_track(&analysis_request)
                .await
                && let Some(session) = coordinator.get_session(guild_id)
                && session.id == session_id
            {
                let mut playback = session.playback.lock();
                if playback.current_playback_id == Some(playback_id) {
                    playback.current_analysis = Some(analysis);
                }
            }
        });

        Ok(StartedTrack {
            playback_id,
            handle,
        })
    }
}

fn track_transition_timing(
    config: &AutoMixConfig,
    duration: std::time::Duration,
) -> Option<TransitionTiming> {
    if !config.enabled {
        return None;
    }
    plan_transition_timing(duration, duration, config.crossfade)
}

fn track_start_options(
    config: &AutoMixConfig,
    duration: Option<std::time::Duration>,
    initial_gain: f32,
) -> TrackStartOptions {
    let timing = duration.and_then(|duration| track_transition_timing(config, duration));
    TrackStartOptions {
        initial_gain,
        prefetch_after: timing.map(|timing| timing.prefetch_after),
        transition_after: timing.map(|timing| {
            timing
                .transition_after
                .saturating_sub(BEAT_ALIGNMENT_LOOKAHEAD)
        }),
    }
}

async fn run_linear_fade(
    outgoing: Arc<dyn RuntimeTrackHandle>,
    incoming: Arc<dyn RuntimeTrackHandle>,
    duration: std::time::Duration,
) {
    let steps = (duration.as_millis() / 50).clamp(1, 200) as u32;
    let interval = duration / steps;
    for step in 1..=steps {
        tokio::time::sleep(interval).await;
        let progress = step as f32 / steps as f32;
        outgoing.set_volume(1.0 - progress);
        incoming.set_volume(progress);
    }
}

struct StartedTrack {
    playback_id: PlaybackId,
    handle: Arc<dyn RuntimeTrackHandle>,
}

#[async_trait]
impl<M, R> PlaybackService for PlaybackCoordinator<M, R>
where
    M: MediaBackend,
    R: VoiceRuntime,
{
    type Error = PlaybackError<M::Error, R::Error>;

    async fn enqueue(
        &self,
        guild_id: GuildKey,
        source_url: &str,
    ) -> Result<EnqueueOutcome, Self::Error> {
        self.enqueue_impl(guild_id, source_url).await
    }

    fn queue_preview(&self, guild_id: GuildKey, limit: usize) -> Option<QueuePreview> {
        Self::queue_preview(self, guild_id, limit)
    }

    async fn toggle_loop(&self, guild_id: GuildKey) -> Option<bool> {
        Self::toggle_loop(self, guild_id).await
    }

    async fn skip(&self, guild_id: GuildKey) -> Option<bool> {
        Self::skip(self, guild_id).await
    }

    fn has_current_track(&self, guild_id: GuildKey) -> bool {
        Self::has_current_track(self, guild_id)
    }

    async fn shuffle(&self, guild_id: GuildKey) -> bool {
        Self::shuffle(self, guild_id).await
    }

    fn automix_enabled(&self, guild_id: GuildKey) -> bool {
        Self::automix_enabled(self, guild_id)
    }

    async fn toggle_automix(&self, guild_id: GuildKey) -> Option<bool> {
        Self::toggle_automix(self, guild_id).await
    }

    async fn restart_snapshot(&self, guild_id: GuildKey) -> Option<PlaybackRestartSnapshot> {
        Self::restart_snapshot(self, guild_id).await
    }

    async fn restore_restart_snapshot(
        &self,
        guild_id: GuildKey,
        snapshot: PlaybackRestartSnapshot,
    ) -> bool {
        Self::restore_restart_snapshot(self, guild_id, snapshot).await
    }

    async fn disconnect_guild(&self, guild_id: GuildKey) {
        Self::disconnect_guild(self, guild_id).await;
    }

    fn bootstrap_voice_state(
        &self,
        guild_id: GuildKey,
        bot_channel: ChannelKey,
        peers: Vec<VoicePeerSnapshot>,
    ) {
        Self::bootstrap_voice_state(self, guild_id, bot_channel, peers);
    }

    fn update_bot_voice_channel(&self, guild_id: GuildKey, new_channel: Option<ChannelKey>) {
        Self::update_bot_voice_channel(self, guild_id, new_channel);
    }

    fn clear_voice_state(&self, guild_id: GuildKey) {
        Self::clear_voice_state(self, guild_id);
    }

    fn apply_peer_voice_state(
        &self,
        guild_id: GuildKey,
        user_id: UserKey,
        old_channel: Option<ChannelKey>,
        new_channel: Option<ChannelKey>,
    ) -> VoiceUpdateDecision {
        Self::apply_peer_voice_state(self, guild_id, user_id, old_channel, new_channel)
    }

    fn voice_action_access(
        &self,
        guild_id: GuildKey,
        actor_channel: Option<ChannelKey>,
    ) -> VoiceActionAccess {
        Self::voice_action_access(self, guild_id, actor_channel)
    }
}

impl GuildVoiceIndex {
    fn bootstrap(&mut self, bot_channel: ChannelKey, peers: Vec<VoicePeerSnapshot>) {
        self.clear();
        self.bot_channel = Some(bot_channel);
        self.bootstrapped = true;

        for peer in peers {
            self.set_peer_channel(peer.user_id, Some(peer.channel_id));
        }
    }

    fn update_bot_channel(&mut self, new_channel: Option<ChannelKey>) {
        self.bot_channel = new_channel;
    }

    fn clear(&mut self) {
        self.bot_channel = None;
        self.peers_by_user.clear();
        self.peer_counts_by_channel.clear();
        self.bootstrapped = false;
    }

    fn apply_peer_update(
        &mut self,
        user_id: UserKey,
        old_channel: Option<ChannelKey>,
        new_channel: Option<ChannelKey>,
    ) -> VoiceUpdateDecision {
        if !self.bootstrapped {
            return VoiceUpdateDecision::Ignore;
        }

        let tracked_old = self.peers_by_user.get(&user_id).copied();
        let effective_old = tracked_old.or(old_channel);
        if effective_old == new_channel {
            return self.decision();
        }

        self.set_peer_channel(user_id, new_channel);
        self.decision()
    }

    fn action_access(&self, actor_channel: Option<ChannelKey>) -> VoiceActionAccess {
        let Some(actor_channel) = actor_channel else {
            return VoiceActionAccess::UserNotInVoice;
        };
        let Some(bot_channel) = self.bot_channel else {
            return VoiceActionAccess::NoActiveChannel;
        };

        if actor_channel == bot_channel {
            VoiceActionAccess::SameChannel {
                channel_id: bot_channel,
            }
        } else {
            VoiceActionAccess::DifferentChannel {
                active_channel: bot_channel,
                actor_channel,
            }
        }
    }

    fn set_peer_channel(&mut self, user_id: UserKey, new_channel: Option<ChannelKey>) {
        if let Some(previous_channel) = self.peers_by_user.remove(&user_id) {
            self.decrement_channel(previous_channel);
        }

        if let Some(channel_id) = new_channel {
            self.peers_by_user.insert(user_id, channel_id);
            *self.peer_counts_by_channel.entry(channel_id).or_default() += 1;
        }
    }

    fn decrement_channel(&mut self, channel_id: ChannelKey) {
        let remove_entry = match self.peer_counts_by_channel.get_mut(&channel_id) {
            Some(count) if *count > 1 => {
                *count -= 1;
                false
            }
            Some(_) => true,
            None => false,
        };

        if remove_entry {
            self.peer_counts_by_channel.remove(&channel_id);
        }
    }

    fn decision(&self) -> VoiceUpdateDecision {
        if !self.bootstrapped {
            return VoiceUpdateDecision::Ignore;
        }

        let Some(bot_channel) = self.bot_channel else {
            return VoiceUpdateDecision::Ignore;
        };

        if self
            .peer_counts_by_channel
            .get(&bot_channel)
            .copied()
            .unwrap_or_default()
            == 0
        {
            VoiceUpdateDecision::DisconnectAlone
        } else {
            VoiceUpdateDecision::StayConnected
        }
    }
}

fn take_flush_action<ME, RE>(playback: &mut PlaybackRuntime<ME, RE>) -> FlushAction<ME, RE>
where
    ME: std::error::Error + Send + Sync + 'static,
    RE: std::error::Error + Send + Sync + 'static,
{
    let sequence = playback.next_commit_seq;
    let Some(pending) = playback.pending_enqueues.get_mut(&sequence) else {
        return FlushAction::WaitForEarlier;
    };
    let Some(outcome) = pending.outcome.take() else {
        return FlushAction::WaitForEarlier;
    };

    let Some(mut pending) = playback.pending_enqueues.remove(&sequence) else {
        return FlushAction::WaitForEarlier;
    };
    playback.next_commit_seq += 1;

    match outcome {
        Err(error) => FlushAction::Fail {
            completion: pending.completion.take(),
            error,
        },
        Ok(request) => {
            let now_playing = playback.logical.enqueue(request.clone());
            if now_playing {
                FlushAction::StartCurrent {
                    completion: pending.completion.take(),
                    request,
                }
            } else {
                FlushAction::CommitQueued {
                    completion: pending.completion.take(),
                    request,
                }
            }
        }
    }
}

fn invalidate_prepared_transition<ME, RE>(
    playback: &mut PlaybackRuntime<ME, RE>,
) -> Option<Arc<dyn RuntimeTrackHandle>>
where
    ME: std::error::Error + Send + Sync + 'static,
    RE: std::error::Error + Send + Sync + 'static,
{
    playback.prefetch_generation = playback.prefetch_generation.wrapping_add(1);
    playback.transition_due = None;
    playback
        .prepared_transition
        .take()
        .map(|prepared| prepared.incoming.handle)
}

fn drain_pending<ME, RE>(playback: &mut PlaybackRuntime<ME, RE>) -> Vec<CompletionSender<ME, RE>>
where
    ME: std::error::Error + Send + Sync + 'static,
    RE: std::error::Error + Send + Sync + 'static,
{
    std::mem::take(&mut playback.pending_enqueues)
        .into_values()
        .filter_map(|mut pending| pending.completion.take())
        .collect()
}

fn remove_pending<ME, RE>(
    session: &GuildSession<ME, RE>,
    sequence: u64,
) -> Option<CompletionSender<ME, RE>>
where
    ME: std::error::Error + Send + Sync + 'static,
    RE: std::error::Error + Send + Sync + 'static,
{
    let mut playback = session.playback.lock();
    playback
        .pending_enqueues
        .remove(&sequence)
        .and_then(|mut pending| pending.completion.take())
}

fn complete_all<ME, RE, F>(pending: Vec<CompletionSender<ME, RE>>, error: F)
where
    ME: std::error::Error + Send + Sync + 'static,
    RE: std::error::Error + Send + Sync + 'static,
    F: Fn() -> PlaybackError<ME, RE>,
{
    for completion in pending {
        let _ = completion.send(Err(error()));
    }
}

#[cfg(test)]
mod tests {
    use super::{GuildVoiceIndex, MAX_PENDING_ENQUEUES, PlaybackCoordinator, PlaybackError};
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::{
        collections::HashMap,
        fmt,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tokio::{
        sync::{Notify, oneshot},
        task::JoinHandle,
        time::timeout,
    };
    use wotoha_contracts::{
        ChannelKey, EnqueueOutcome, GuildKey, MediaBackend, PlaybackId, PlaybackRuntimeEvent,
        RuntimeEventSink, RuntimeTrackHandle, TrackEndReason, VoiceActionAccess, VoiceRuntime,
    };
    use wotoha_core::{PreparedSource, TrackMetadata, TrackRequest, automix::AutoMixConfig};

    type TestPlayback = PlaybackCoordinator<MockMedia, MockRuntime>;
    type TestPlaybackError = PlaybackError<TestMediaError, TestRuntimeError>;
    type EnqueueJoinHandle = JoinHandle<Result<EnqueueOutcome, TestPlaybackError>>;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct TestMediaError(Arc<str>);

    impl TestMediaError {
        fn new(message: impl Into<Arc<str>>) -> Self {
            Self(message.into())
        }
    }

    impl fmt::Display for TestMediaError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "{}", self.0)
        }
    }

    impl std::error::Error for TestMediaError {}

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct TestRuntimeError(Arc<str>);

    impl TestRuntimeError {
        fn new(message: impl Into<Arc<str>>) -> Self {
            Self(message.into())
        }
    }

    impl fmt::Display for TestRuntimeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "{}", self.0)
        }
    }

    impl std::error::Error for TestRuntimeError {}

    enum ResolveScript {
        Immediate(Box<Result<TrackRequest, TestMediaError>>),
        Deferred(Box<oneshot::Receiver<Result<TrackRequest, TestMediaError>>>),
    }

    #[derive(Clone, Default)]
    struct MockMedia {
        state: Arc<MockMediaState>,
    }

    #[derive(Default)]
    struct MockMediaState {
        scripts: Mutex<HashMap<String, ResolveScript>>,
        prepare_scripts:
            Mutex<HashMap<String, oneshot::Receiver<Result<TrackRequest, TestMediaError>>>>,
        prepared_keys: Mutex<Vec<String>>,
        resolve_calls: AtomicUsize,
        prepare_calls: AtomicUsize,
        resolve_notify: Notify,
        prepare_notify: Notify,
    }

    impl MockMedia {
        fn resolve_with(&self, source_url: &str, result: Result<TrackRequest, TestMediaError>) {
            let previous = self.state.scripts.lock().insert(
                source_url.to_owned(),
                ResolveScript::Immediate(Box::new(result)),
            );
            assert!(previous.is_none());
        }

        fn block_resolve(
            &self,
            source_url: &str,
        ) -> oneshot::Sender<Result<TrackRequest, TestMediaError>> {
            let (sender, receiver) = oneshot::channel();
            let previous = self.state.scripts.lock().insert(
                source_url.to_owned(),
                ResolveScript::Deferred(Box::new(receiver)),
            );
            assert!(previous.is_none());
            sender
        }

        fn block_prepare(
            &self,
            canonical_key: &str,
        ) -> oneshot::Sender<Result<TrackRequest, TestMediaError>> {
            let (sender, receiver) = oneshot::channel();
            let previous = self
                .state
                .prepare_scripts
                .lock()
                .insert(canonical_key.to_owned(), receiver);
            assert!(previous.is_none());
            sender
        }

        async fn wait_for_resolve_count(&self, expected: usize) {
            timeout(Duration::from_secs(3), async {
                while self.state.resolve_calls.load(Ordering::SeqCst) < expected {
                    self.state.resolve_notify.notified().await;
                }
            })
            .await
            .expect("resolve count wait timed out");
        }

        async fn wait_for_prepare_count(&self, expected: usize) {
            timeout(Duration::from_secs(3), async {
                while self.state.prepare_calls.load(Ordering::SeqCst) < expected {
                    self.state.prepare_notify.notified().await;
                }
            })
            .await
            .expect("prepare count wait timed out");
        }

        fn resolve_count(&self) -> usize {
            self.state.resolve_calls.load(Ordering::SeqCst)
        }

        fn prepared_keys(&self) -> Vec<String> {
            self.state.prepared_keys.lock().clone()
        }
    }

    #[async_trait]
    impl MediaBackend for MockMedia {
        type Error = TestMediaError;

        async fn resolve(&self, source_url: &str) -> Result<TrackRequest, Self::Error> {
            let script = self.state.scripts.lock().remove(source_url);
            self.state.resolve_calls.fetch_add(1, Ordering::SeqCst);
            self.state.resolve_notify.notify_waiters();

            match script {
                Some(ResolveScript::Immediate(result)) => *result,
                Some(ResolveScript::Deferred(receiver)) => (*receiver)
                    .await
                    .unwrap_or_else(|_| Err(TestMediaError::new("resolve sender dropped"))),
                None => Ok(track_request(source_url)),
            }
        }

        async fn prepare_playback(
            &self,
            request: &TrackRequest,
        ) -> Result<TrackRequest, Self::Error> {
            self.state
                .prepared_keys
                .lock()
                .push(request.canonical_key.to_string());
            self.state.prepare_calls.fetch_add(1, Ordering::SeqCst);
            self.state.prepare_notify.notify_waiters();

            let receiver = self
                .state
                .prepare_scripts
                .lock()
                .remove(request.canonical_key.as_ref());

            match receiver {
                Some(receiver) => receiver
                    .await
                    .unwrap_or_else(|_| Err(TestMediaError::new("prepare sender dropped"))),
                None => Ok(request.clone()),
            }
        }
    }

    #[derive(Clone, Default)]
    struct MockRuntime {
        state: Arc<MockRuntimeState>,
    }

    #[derive(Default)]
    struct MockRuntimeState {
        played: Mutex<Vec<PlayedTrack>>,
        play_failures: Mutex<HashMap<String, TestRuntimeError>>,
        stopped: Mutex<Vec<PlaybackId>>,
        paused: Mutex<Vec<PlaybackId>>,
        resumed: Mutex<Vec<PlaybackId>>,
        volumes: Mutex<Vec<(PlaybackId, f32)>>,
        disconnects: Mutex<Vec<GuildKey>>,
        play_notify: Notify,
    }

    #[derive(Clone)]
    struct PlayedTrack {
        guild_id: GuildKey,
        session_id: u64,
        playback_id: PlaybackId,
        key: String,
        events: RuntimeEventSink,
    }

    impl MockRuntime {
        fn fail_play_for(&self, key: &str, error: TestRuntimeError) {
            let previous = self
                .state
                .play_failures
                .lock()
                .insert(key.to_owned(), error);
            assert!(previous.is_none());
        }

        async fn wait_for_play_count(&self, expected: usize) {
            timeout(Duration::from_secs(3), async {
                while self.state.played.lock().len() < expected {
                    self.state.play_notify.notified().await;
                }
            })
            .await
            .expect("play count wait timed out");
        }

        fn played(&self) -> Vec<PlayedTrack> {
            self.state.played.lock().clone()
        }

        fn stopped(&self) -> Vec<PlaybackId> {
            self.state.stopped.lock().clone()
        }

        fn volumes(&self) -> Vec<(PlaybackId, f32)> {
            self.state.volumes.lock().clone()
        }

        fn paused(&self) -> Vec<PlaybackId> {
            self.state.paused.lock().clone()
        }

        fn resumed(&self) -> Vec<PlaybackId> {
            self.state.resumed.lock().clone()
        }

        fn disconnects(&self) -> Vec<GuildKey> {
            self.state.disconnects.lock().clone()
        }
    }

    struct MockTrackHandle {
        playback_id: PlaybackId,
        state: Arc<MockRuntimeState>,
    }

    #[async_trait]
    impl RuntimeTrackHandle for MockTrackHandle {
        fn stop(&self) {
            self.state.stopped.lock().push(self.playback_id);
        }

        fn set_volume(&self, volume: f32) {
            self.state.volumes.lock().push((self.playback_id, volume));
        }

        fn pause(&self) {
            self.state.paused.lock().push(self.playback_id);
        }

        fn resume(&self) {
            self.state.resumed.lock().push(self.playback_id);
        }
    }

    #[async_trait]
    impl VoiceRuntime for MockRuntime {
        type Error = TestRuntimeError;

        async fn play_track(
            &self,
            guild_id: GuildKey,
            session_id: u64,
            playback_id: PlaybackId,
            request: &TrackRequest,
            events: RuntimeEventSink,
        ) -> Result<Arc<dyn RuntimeTrackHandle>, Self::Error> {
            if let Some(error) = self
                .state
                .play_failures
                .lock()
                .remove(request.canonical_key.as_ref())
            {
                return Err(error);
            }

            let handle = Arc::new(MockTrackHandle {
                playback_id,
                state: self.state.clone(),
            });
            self.state.played.lock().push(PlayedTrack {
                guild_id,
                session_id,
                playback_id,
                key: request.canonical_key.to_string(),
                events,
            });
            self.state.play_notify.notify_waiters();
            Ok(handle)
        }

        async fn disconnect_guild(&self, guild_id: GuildKey) -> Result<(), Self::Error> {
            self.state.disconnects.lock().push(guild_id);
            Ok(())
        }
    }

    fn track_request(key: &str) -> TrackRequest {
        track_request_with_duration(key, None)
    }

    fn track_request_with_duration(key: &str, duration: Option<Duration>) -> TrackRequest {
        let track_url = format!("https://example.invalid/{key}");
        TrackRequest::new(
            "test",
            key,
            track_url.clone(),
            track_url.clone(),
            format!("https://media.example.invalid/{key}.opus"),
            PreparedSource::http(
                format!("https://stream.example.invalid/{key}.opus"),
                Vec::new(),
                None,
                None,
            ),
            TrackMetadata::new(key, "tester", track_url, None, duration),
        )
    }

    fn automix_config(crossfade: Duration) -> AutoMixConfig {
        AutoMixConfig {
            enabled: true,
            crossfade,
            max_tempo_adjustment: 0.06,
            min_beat_confidence: 0.7,
        }
    }

    fn spawn_enqueue(
        playback: TestPlayback,
        guild_id: GuildKey,
        source_url: String,
    ) -> EnqueueJoinHandle {
        tokio::spawn(async move { playback.enqueue_impl(guild_id, &source_url).await })
    }

    async fn join_enqueue(handle: EnqueueJoinHandle) -> Result<EnqueueOutcome, TestPlaybackError> {
        timeout(Duration::from_secs(3), handle)
            .await
            .expect("enqueue wait timed out")
            .expect("enqueue task panicked")
    }

    async fn wait_for_pending_outcome(playback: &TestPlayback, guild_id: GuildKey, sequence: u64) {
        timeout(Duration::from_secs(3), async {
            loop {
                if let Some(session) = playback.get_session(guild_id) {
                    let runtime = session.playback.lock();
                    let ready = runtime
                        .pending_enqueues
                        .get(&sequence)
                        .and_then(|pending| pending.outcome.as_ref())
                        .is_some();
                    if ready {
                        return;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pending outcome wait timed out");
    }

    fn pending_len(playback: &TestPlayback, guild_id: GuildKey) -> usize {
        playback
            .get_session(guild_id)
            .map(|session| session.playback.lock().pending_enqueues.len())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn loop_replaces_automix_and_automix_replaces_loop() {
        let playback = PlaybackCoordinator::new(MockMedia::default(), MockRuntime::default());
        playback
            .enqueue_impl(GuildKey::new(1), "track")
            .await
            .expect("track should start");

        assert_eq!(playback.toggle_loop(GuildKey::new(1)).await, Some(true));
        assert_eq!(playback.toggle_automix(GuildKey::new(1)).await, Some(true));
        assert!(playback.automix_enabled(GuildKey::new(1)));
        assert_eq!(playback.toggle_loop(GuildKey::new(1)).await, Some(true));
        assert!(!playback.automix_enabled(GuildKey::new(1)));
    }

    #[test]
    fn action_access_requires_same_voice_channel() {
        let mut voice = GuildVoiceIndex::default();
        voice.update_bot_channel(Some(ChannelKey::new(10)));

        assert_eq!(voice.action_access(None), VoiceActionAccess::UserNotInVoice);
        assert_eq!(
            voice.action_access(Some(ChannelKey::new(10))),
            VoiceActionAccess::SameChannel {
                channel_id: ChannelKey::new(10),
            }
        );
        assert_eq!(
            voice.action_access(Some(ChannelKey::new(11))),
            VoiceActionAccess::DifferentChannel {
                active_channel: ChannelKey::new(10),
                actor_channel: ChannelKey::new(11),
            }
        );
    }

    #[tokio::test]
    async fn enqueue_preserves_reservation_order_when_resolution_finishes_out_of_order() {
        let guild_id = GuildKey::new(1);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new(media.clone(), runtime.clone());
        let first_release = media.block_resolve("first");
        let second_release = media.block_resolve("second");

        let first = spawn_enqueue(playback.clone(), guild_id, "first".to_owned());
        let second = spawn_enqueue(playback.clone(), guild_id, "second".to_owned());

        media.wait_for_resolve_count(2).await;
        second_release
            .send(Ok(track_request("second")))
            .expect("second resolve receiver dropped");
        first_release
            .send(Ok(track_request("first")))
            .expect("first resolve receiver dropped");

        let first_outcome = join_enqueue(first).await.expect("first enqueue failed");
        let second_outcome = join_enqueue(second).await.expect("second enqueue failed");
        runtime.wait_for_play_count(1).await;

        assert!(first_outcome.now_playing);
        assert!(!second_outcome.now_playing);
        assert_eq!(first_outcome.request.canonical_key.as_ref(), "first");
        assert_eq!(second_outcome.request.canonical_key.as_ref(), "second");
        assert_eq!(media.prepared_keys(), vec!["first"]);

        let played = runtime.played();
        assert_eq!(played.len(), 1);
        assert_eq!(played[0].key, "first");

        let preview = playback
            .queue_preview(guild_id, 8)
            .expect("queue preview missing");
        assert_eq!(
            preview
                .current()
                .expect("current track missing")
                .metadata
                .title
                .as_ref(),
            "first"
        );
        assert_eq!(preview.upcoming().len(), 1);
        assert_eq!(preview.upcoming()[0].metadata.title.as_ref(), "second");
    }

    #[tokio::test]
    async fn resolve_failure_releases_following_enqueue() {
        let guild_id = GuildKey::new(2);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new(media.clone(), runtime.clone());
        let failed_release = media.block_resolve("failed");
        let next_release = media.block_resolve("next");

        let failed = spawn_enqueue(playback.clone(), guild_id, "failed".to_owned());
        let next = spawn_enqueue(playback.clone(), guild_id, "next".to_owned());

        media.wait_for_resolve_count(2).await;
        next_release
            .send(Ok(track_request("next")))
            .expect("next resolve receiver dropped");
        failed_release
            .send(Err(TestMediaError::new("resolve failed")))
            .expect("failed resolve receiver dropped");

        let failed_outcome = join_enqueue(failed).await;
        let next_outcome = join_enqueue(next)
            .await
            .expect("next enqueue should start after earlier failure");
        runtime.wait_for_play_count(1).await;

        assert!(matches!(
            failed_outcome,
            Err(PlaybackError::Resolve(error)) if error == TestMediaError::new("resolve failed")
        ));
        assert!(next_outcome.now_playing);
        assert_eq!(next_outcome.request.canonical_key.as_ref(), "next");

        let played = runtime.played();
        assert_eq!(played.len(), 1);
        assert_eq!(played[0].key, "next");

        let preview = playback
            .queue_preview(guild_id, 8)
            .expect("queue preview missing");
        assert_eq!(
            preview
                .current()
                .expect("current track missing")
                .metadata
                .title
                .as_ref(),
            "next"
        );
        assert!(preview.upcoming().is_empty());
    }

    #[tokio::test]
    async fn stale_track_end_does_not_advance_queue() {
        let guild_id = GuildKey::new(3);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new(media.clone(), runtime.clone());
        media.resolve_with("current", Ok(track_request("current")));
        media.resolve_with("queued", Ok(track_request("queued")));

        let current = playback
            .enqueue_impl(guild_id, "current")
            .await
            .expect("current enqueue failed");
        let queued = playback
            .enqueue_impl(guild_id, "queued")
            .await
            .expect("queued enqueue failed");
        runtime.wait_for_play_count(1).await;

        assert!(current.now_playing);
        assert!(!queued.now_playing);

        let first_play = runtime.played()[0].clone();
        assert_eq!(first_play.guild_id, guild_id);
        playback
            .advance(
                guild_id,
                first_play.session_id,
                PlaybackId::new(first_play.playback_id.get() + 1000),
                TrackEndReason::Completed,
            )
            .await;

        let played_after_stale = runtime.played();
        assert_eq!(played_after_stale.len(), 1);
        assert_eq!(played_after_stale[0].key, "current");

        let preview_after_stale = playback
            .queue_preview(guild_id, 8)
            .expect("queue preview missing");
        assert_eq!(
            preview_after_stale
                .current()
                .expect("current track missing")
                .metadata
                .title
                .as_ref(),
            "current"
        );
        assert_eq!(preview_after_stale.upcoming().len(), 1);
        assert_eq!(
            preview_after_stale.upcoming()[0].metadata.title.as_ref(),
            "queued"
        );

        playback
            .advance(
                guild_id,
                first_play.session_id,
                first_play.playback_id,
                TrackEndReason::Completed,
            )
            .await;
        runtime.wait_for_play_count(2).await;

        let played_after_current_end = runtime.played();
        assert_eq!(played_after_current_end.len(), 2);
        assert_eq!(played_after_current_end[1].key, "queued");
        assert_eq!(runtime.stopped(), Vec::<PlaybackId>::new());
    }

    #[tokio::test]
    async fn track_end_after_skip_advances_to_next_track_and_duplicate_end_is_ignored() {
        let guild_id = GuildKey::new(4);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new(media.clone(), runtime.clone());
        media.resolve_with("current", Ok(track_request("current")));
        media.resolve_with("queued", Ok(track_request("queued")));
        media.resolve_with("after", Ok(track_request("after")));

        let current = playback
            .enqueue_impl(guild_id, "current")
            .await
            .expect("current enqueue failed");
        let queued = playback
            .enqueue_impl(guild_id, "queued")
            .await
            .expect("queued enqueue failed");
        let after = playback
            .enqueue_impl(guild_id, "after")
            .await
            .expect("after enqueue failed");
        runtime.wait_for_play_count(1).await;

        assert!(current.now_playing);
        assert!(!queued.now_playing);
        assert!(!after.now_playing);

        let first_play = runtime.played()[0].clone();
        assert_eq!(playback.skip(guild_id).await, Some(false));
        assert_eq!(runtime.stopped(), vec![first_play.playback_id]);

        first_play
            .events
            .send(PlaybackRuntimeEvent::TrackEnded {
                guild_id,
                session_id: first_play.session_id,
                playback_id: first_play.playback_id,
                reason: TrackEndReason::Stopped,
            })
            .expect("runtime event receiver dropped");
        runtime.wait_for_play_count(2).await;

        let played_after_skip = runtime.played();
        assert_eq!(played_after_skip.len(), 2);
        assert_eq!(played_after_skip[1].key, "queued");

        playback
            .advance(
                guild_id,
                first_play.session_id,
                first_play.playback_id,
                TrackEndReason::Stopped,
            )
            .await;

        let played_after_duplicate_end = runtime.played();
        assert_eq!(played_after_duplicate_end.len(), 2);

        let preview = playback
            .queue_preview(guild_id, 8)
            .expect("queue preview missing");
        assert_eq!(
            preview
                .current()
                .expect("current track missing")
                .metadata
                .title
                .as_ref(),
            "queued"
        );
        assert_eq!(preview.upcoming().len(), 1);
        assert_eq!(preview.upcoming()[0].metadata.title.as_ref(), "after");
    }

    #[tokio::test]
    async fn disconnect_guild_expires_waiting_pending_completion() {
        let guild_id = GuildKey::new(5);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new(media.clone(), runtime.clone());
        let first_release = media.block_resolve("first");
        media.resolve_with("second", Ok(track_request("second")));

        let first = spawn_enqueue(playback.clone(), guild_id, "first".to_owned());
        media.wait_for_resolve_count(1).await;
        let second = spawn_enqueue(playback.clone(), guild_id, "second".to_owned());
        media.wait_for_resolve_count(2).await;
        wait_for_pending_outcome(&playback, guild_id, 1).await;

        playback.disconnect_guild(guild_id).await;

        let second_outcome = join_enqueue(second).await;
        assert!(matches!(second_outcome, Err(PlaybackError::SessionExpired)));
        assert_eq!(runtime.disconnects(), vec![guild_id]);

        first_release
            .send(Ok(track_request("first")))
            .expect("first resolve receiver dropped");
        let first_outcome = join_enqueue(first).await;
        assert!(matches!(first_outcome, Err(PlaybackError::SessionExpired)));
    }

    #[tokio::test]
    async fn runtime_play_failure_does_not_block_following_ready_enqueue() {
        let guild_id = GuildKey::new(6);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new(media.clone(), runtime.clone());
        runtime.fail_play_for("bad", TestRuntimeError::new("runtime failed"));
        let bad_release = media.block_resolve("bad");
        let good_release = media.block_resolve("good");

        let bad = spawn_enqueue(playback.clone(), guild_id, "bad".to_owned());
        media.wait_for_resolve_count(1).await;
        let good = spawn_enqueue(playback.clone(), guild_id, "good".to_owned());
        media.wait_for_resolve_count(2).await;

        good_release
            .send(Ok(track_request("good")))
            .expect("good resolve receiver dropped");
        wait_for_pending_outcome(&playback, guild_id, 1).await;
        bad_release
            .send(Ok(track_request("bad")))
            .expect("bad resolve receiver dropped");

        let bad_outcome = join_enqueue(bad).await;
        let good_outcome = join_enqueue(good)
            .await
            .expect("good enqueue should start after runtime failure");
        runtime.wait_for_play_count(1).await;

        assert!(matches!(
            bad_outcome,
            Err(PlaybackError::Runtime(error)) if error == TestRuntimeError::new("runtime failed")
        ));
        assert!(good_outcome.now_playing);
        assert_eq!(good_outcome.request.canonical_key.as_ref(), "good");

        let played = runtime.played();
        assert_eq!(played.len(), 1);
        assert_eq!(played[0].key, "good");

        let preview = playback
            .queue_preview(guild_id, 8)
            .expect("queue preview missing");
        assert_eq!(
            preview
                .current()
                .expect("current track missing")
                .metadata
                .title
                .as_ref(),
            "good"
        );
        assert!(preview.upcoming().is_empty());
    }

    #[tokio::test]
    async fn pending_flush_handles_resolve_error_runtime_error_and_success_in_order() {
        let guild_id = GuildKey::new(7);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new(media.clone(), runtime.clone());
        runtime.fail_play_for("runtime-bad", TestRuntimeError::new("runtime failed"));
        let resolve_bad_release = media.block_resolve("resolve-bad");
        let runtime_bad_release = media.block_resolve("runtime-bad");
        let good_release = media.block_resolve("good");
        let tail_release = media.block_resolve("tail");

        let resolve_bad = spawn_enqueue(playback.clone(), guild_id, "resolve-bad".to_owned());
        let runtime_bad = spawn_enqueue(playback.clone(), guild_id, "runtime-bad".to_owned());
        let good = spawn_enqueue(playback.clone(), guild_id, "good".to_owned());
        let tail = spawn_enqueue(playback.clone(), guild_id, "tail".to_owned());

        media.wait_for_resolve_count(4).await;
        tail_release
            .send(Ok(track_request("tail")))
            .expect("tail resolve receiver dropped");
        good_release
            .send(Ok(track_request("good")))
            .expect("good resolve receiver dropped");
        runtime_bad_release
            .send(Ok(track_request("runtime-bad")))
            .expect("runtime-bad resolve receiver dropped");
        resolve_bad_release
            .send(Err(TestMediaError::new("resolve failed")))
            .expect("resolve-bad receiver dropped");

        let resolve_bad_outcome = join_enqueue(resolve_bad).await;
        let runtime_bad_outcome = join_enqueue(runtime_bad).await;
        let good_outcome = join_enqueue(good)
            .await
            .expect("good enqueue should start after earlier failures");
        let tail_outcome = join_enqueue(tail)
            .await
            .expect("tail enqueue should remain queued after good starts");
        runtime.wait_for_play_count(1).await;

        assert!(matches!(
            resolve_bad_outcome,
            Err(PlaybackError::Resolve(error)) if error == TestMediaError::new("resolve failed")
        ));
        assert!(matches!(
            runtime_bad_outcome,
            Err(PlaybackError::Runtime(error)) if error == TestRuntimeError::new("runtime failed")
        ));
        assert!(good_outcome.now_playing);
        assert!(!tail_outcome.now_playing);
        assert_eq!(pending_len(&playback, guild_id), 0);

        let played = runtime.played();
        assert_eq!(played.len(), 1);
        assert_eq!(played[0].key, "good");

        let preview = playback
            .queue_preview(guild_id, 8)
            .expect("queue preview missing");
        assert_eq!(
            preview
                .current()
                .expect("current track missing")
                .metadata
                .title
                .as_ref(),
            "good"
        );
        assert_eq!(preview.upcoming().len(), 1);
        assert_eq!(preview.upcoming()[0].metadata.title.as_ref(), "tail");
    }

    #[tokio::test]
    async fn track_end_skips_consecutive_runtime_failures_and_keeps_queue_order() {
        let guild_id = GuildKey::new(8);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new(media.clone(), runtime.clone());
        runtime.fail_play_for("bad-1", TestRuntimeError::new("bad-1 failed"));
        runtime.fail_play_for("bad-2", TestRuntimeError::new("bad-2 failed"));
        media.resolve_with("current", Ok(track_request("current")));
        media.resolve_with("bad-1", Ok(track_request("bad-1")));
        media.resolve_with("bad-2", Ok(track_request("bad-2")));
        media.resolve_with("good", Ok(track_request("good")));
        media.resolve_with("tail", Ok(track_request("tail")));

        let current = playback
            .enqueue_impl(guild_id, "current")
            .await
            .expect("current enqueue failed");
        let bad_1 = playback
            .enqueue_impl(guild_id, "bad-1")
            .await
            .expect("bad-1 enqueue failed");
        let bad_2 = playback
            .enqueue_impl(guild_id, "bad-2")
            .await
            .expect("bad-2 enqueue failed");
        let good = playback
            .enqueue_impl(guild_id, "good")
            .await
            .expect("good enqueue failed");
        let tail = playback
            .enqueue_impl(guild_id, "tail")
            .await
            .expect("tail enqueue failed");
        runtime.wait_for_play_count(1).await;

        assert!(current.now_playing);
        assert!(!bad_1.now_playing);
        assert!(!bad_2.now_playing);
        assert!(!good.now_playing);
        assert!(!tail.now_playing);
        assert_eq!(pending_len(&playback, guild_id), 0);

        let first_play = runtime.played()[0].clone();
        first_play
            .events
            .send(PlaybackRuntimeEvent::TrackEnded {
                guild_id,
                session_id: first_play.session_id,
                playback_id: first_play.playback_id,
                reason: TrackEndReason::Completed,
            })
            .expect("runtime event receiver dropped");
        runtime.wait_for_play_count(2).await;

        let played = runtime.played();
        assert_eq!(played.len(), 2);
        assert_eq!(played[0].key, "current");
        assert_eq!(played[1].key, "good");
        assert_eq!(
            media.prepared_keys(),
            vec!["current", "bad-1", "bad-2", "good"]
        );

        let preview = playback
            .queue_preview(guild_id, 8)
            .expect("queue preview missing");
        assert_eq!(
            preview
                .current()
                .expect("current track missing")
                .metadata
                .title
                .as_ref(),
            "good"
        );
        assert_eq!(preview.upcoming().len(), 1);
        assert_eq!(preview.upcoming()[0].metadata.title.as_ref(), "tail");
        assert_eq!(preview.total_queued(), 1);
    }

    #[tokio::test]
    async fn disconnect_guild_stops_active_track_and_expires_mixed_pending_requests() {
        let guild_id = GuildKey::new(9);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new(media.clone(), runtime.clone());
        media.resolve_with("current", Ok(track_request("current")));
        let blocked_release = media.block_resolve("blocked");
        media.resolve_with("ready", Ok(track_request("ready")));

        let current = playback
            .enqueue_impl(guild_id, "current")
            .await
            .expect("current enqueue failed");
        runtime.wait_for_play_count(1).await;
        let current_playback_id = runtime.played()[0].playback_id;
        let blocked = spawn_enqueue(playback.clone(), guild_id, "blocked".to_owned());
        media.wait_for_resolve_count(2).await;
        let ready = spawn_enqueue(playback.clone(), guild_id, "ready".to_owned());
        media.wait_for_resolve_count(3).await;
        wait_for_pending_outcome(&playback, guild_id, 2).await;

        assert!(current.now_playing);
        assert_eq!(pending_len(&playback, guild_id), 2);

        playback.disconnect_guild(guild_id).await;

        let ready_outcome = join_enqueue(ready).await;
        assert!(matches!(ready_outcome, Err(PlaybackError::SessionExpired)));
        assert_eq!(runtime.stopped(), vec![current_playback_id]);
        assert_eq!(runtime.disconnects(), vec![guild_id]);
        assert!(!playback.has_current_track(guild_id));
        assert!(playback.queue_preview(guild_id, 8).is_none());

        blocked_release
            .send(Ok(track_request("blocked")))
            .expect("blocked resolve receiver dropped");
        let blocked_outcome = join_enqueue(blocked).await;
        assert!(matches!(
            blocked_outcome,
            Err(PlaybackError::SessionExpired)
        ));
    }

    #[tokio::test]
    async fn disconnect_guild_returns_while_prepare_playback_is_blocked() {
        let guild_id = GuildKey::new(10);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new(media.clone(), runtime.clone());
        media.resolve_with("slow", Ok(track_request("slow")));
        let prepare_release = media.block_prepare("slow");

        let enqueue = spawn_enqueue(playback.clone(), guild_id, "slow".to_owned());
        media.wait_for_prepare_count(1).await;

        timeout(
            Duration::from_millis(100),
            playback.disconnect_guild(guild_id),
        )
        .await
        .expect("disconnect should not wait for blocked prepare");
        assert_eq!(runtime.disconnects(), vec![guild_id]);
        assert!(runtime.played().is_empty());

        prepare_release
            .send(Ok(track_request("slow")))
            .expect("prepare receiver dropped");
        let outcome = join_enqueue(enqueue).await;

        assert!(matches!(outcome, Err(PlaybackError::SessionExpired)));
        assert!(runtime.played().is_empty());
    }

    #[tokio::test]
    async fn pending_enqueue_limit_rejects_new_requests() {
        let guild_id = GuildKey::new(11);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new(media.clone(), runtime.clone());
        let mut releases = Vec::new();
        let mut tasks = Vec::new();

        for index in 0..MAX_PENDING_ENQUEUES {
            let source_url = format!("pending-{index}");
            releases.push((source_url.clone(), media.block_resolve(&source_url)));
            tasks.push(spawn_enqueue(playback.clone(), guild_id, source_url));
        }

        media.wait_for_resolve_count(MAX_PENDING_ENQUEUES).await;
        let overflow = playback.enqueue_impl(guild_id, "overflow").await;

        assert!(matches!(overflow, Err(PlaybackError::QueueFull)));
        assert_eq!(media.resolve_count(), MAX_PENDING_ENQUEUES);

        for (source_url, release) in releases {
            release
                .send(Ok(track_request(&source_url)))
                .expect("pending resolve receiver dropped");
        }

        for task in tasks {
            let outcome = join_enqueue(task).await.expect("pending enqueue failed");
            assert!(outcome.request.canonical_key.starts_with("pending-"));
        }

        runtime.wait_for_play_count(1).await;
        let preview = playback
            .queue_preview(guild_id, 128)
            .expect("queue preview missing");
        assert_eq!(
            preview
                .current()
                .expect("current track missing")
                .metadata
                .title
                .as_ref(),
            "pending-0"
        );
        assert_eq!(preview.total_queued(), MAX_PENDING_ENQUEUES - 1);
    }

    #[tokio::test]
    async fn automix_crossfades_to_queued_track() {
        let guild_id = GuildKey::new(12);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new_with_automix(
            media.clone(),
            runtime.clone(),
            automix_config(Duration::from_millis(20)),
        );
        media.resolve_with(
            "first",
            Ok(track_request_with_duration(
                "first",
                Some(Duration::from_secs(1)),
            )),
        );
        media.resolve_with(
            "second",
            Ok(track_request_with_duration(
                "second",
                Some(Duration::from_secs(1)),
            )),
        );

        playback.enqueue_impl(guild_id, "first").await.unwrap();
        playback.enqueue_impl(guild_id, "second").await.unwrap();
        let first = runtime.played()[0].clone();
        first
            .events
            .send(PlaybackRuntimeEvent::TransitionPrefetchDue {
                guild_id,
                session_id: first.session_id,
                playback_id: first.playback_id,
            })
            .unwrap();
        runtime.wait_for_play_count(2).await;
        let second = runtime.played()[1].playback_id;
        assert_eq!(runtime.paused(), vec![second]);
        assert!(runtime.resumed().is_empty());
        first
            .events
            .send(PlaybackRuntimeEvent::TransitionDue {
                guild_id,
                session_id: first.session_id,
                playback_id: first.playback_id,
            })
            .unwrap();

        timeout(Duration::from_secs(2), async {
            while !runtime.stopped().contains(&first.playback_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("crossfade did not finish");

        assert_eq!(runtime.resumed(), vec![second]);
        let volumes = runtime.volumes();
        assert!(
            volumes
                .iter()
                .any(|(id, gain)| *id == first.playback_id && *gain < 0.01)
        );
        assert!(
            volumes
                .iter()
                .any(|(id, gain)| *id == second && *gain > 0.99)
        );
        let preview = playback.queue_preview(guild_id, 8).unwrap();
        assert_eq!(preview.current().unwrap().metadata.title.as_ref(), "second");
        assert_eq!(preview.total_queued(), 0);
    }

    #[tokio::test]
    async fn automix_start_failure_preserves_current_and_queue() {
        let guild_id = GuildKey::new(13);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new_with_automix(
            media.clone(),
            runtime.clone(),
            automix_config(Duration::from_millis(20)),
        );
        media.resolve_with(
            "first",
            Ok(track_request_with_duration(
                "first",
                Some(Duration::from_secs(1)),
            )),
        );
        media.resolve_with(
            "second",
            Ok(track_request_with_duration(
                "second",
                Some(Duration::from_secs(1)),
            )),
        );
        runtime.fail_play_for("second", TestRuntimeError::new("unplayable"));

        playback.enqueue_impl(guild_id, "first").await.unwrap();
        playback.enqueue_impl(guild_id, "second").await.unwrap();
        let first = runtime.played()[0].clone();
        first
            .events
            .send(PlaybackRuntimeEvent::TransitionPrefetchDue {
                guild_id,
                session_id: first.session_id,
                playback_id: first.playback_id,
            })
            .unwrap();
        media.wait_for_prepare_count(2).await;
        tokio::task::yield_now().await;

        let preview = playback.queue_preview(guild_id, 8).unwrap();
        assert_eq!(preview.current().unwrap().metadata.title.as_ref(), "first");
        assert_eq!(preview.upcoming()[0].metadata.title.as_ref(), "second");
        assert_eq!(runtime.played().len(), 1);
        assert!(runtime.stopped().is_empty());
    }

    #[tokio::test]
    async fn duplicate_automix_transition_and_retired_end_do_not_advance_twice() {
        let guild_id = GuildKey::new(14);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new_with_automix(
            media.clone(),
            runtime.clone(),
            automix_config(Duration::from_millis(200)),
        );
        for key in ["first", "second", "third"] {
            media.resolve_with(
                key,
                Ok(track_request_with_duration(
                    key,
                    Some(Duration::from_secs(1)),
                )),
            );
            playback.enqueue_impl(guild_id, key).await.unwrap();
        }

        let first = runtime.played()[0].clone();
        playback
            .prefetch_transition(guild_id, first.session_id, first.playback_id)
            .await;
        playback
            .transition(guild_id, first.session_id, first.playback_id)
            .await;
        playback
            .transition(guild_id, first.session_id, first.playback_id)
            .await;
        playback
            .advance(
                guild_id,
                first.session_id,
                first.playback_id,
                TrackEndReason::Completed,
            )
            .await;

        let played = runtime.played();
        assert_eq!(played.len(), 2);
        assert_eq!(played[0].key, "first");
        assert_eq!(played[1].key, "second");
        let preview = playback.queue_preview(guild_id, 8).unwrap();
        assert_eq!(preview.current().unwrap().metadata.title.as_ref(), "second");
        assert_eq!(preview.upcoming().len(), 1);
        assert_eq!(preview.upcoming()[0].metadata.title.as_ref(), "third");
    }

    #[tokio::test]
    async fn skip_during_automix_overlap_stops_both_tracks_and_advances_once() {
        let guild_id = GuildKey::new(15);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new_with_automix(
            media.clone(),
            runtime.clone(),
            automix_config(Duration::from_millis(200)),
        );
        for key in ["first", "second", "third"] {
            media.resolve_with(
                key,
                Ok(track_request_with_duration(
                    key,
                    Some(Duration::from_secs(1)),
                )),
            );
            playback.enqueue_impl(guild_id, key).await.unwrap();
        }

        let first = runtime.played()[0].clone();
        playback
            .prefetch_transition(guild_id, first.session_id, first.playback_id)
            .await;
        playback
            .transition(guild_id, first.session_id, first.playback_id)
            .await;
        let second = runtime.played()[1].clone();

        assert_eq!(playback.skip(guild_id).await, Some(false));
        let stopped = runtime.stopped();
        assert!(stopped.contains(&first.playback_id));
        assert!(stopped.contains(&second.playback_id));

        playback
            .advance(
                guild_id,
                first.session_id,
                first.playback_id,
                TrackEndReason::Stopped,
            )
            .await;
        playback
            .advance(
                guild_id,
                second.session_id,
                second.playback_id,
                TrackEndReason::Stopped,
            )
            .await;
        playback
            .advance(
                guild_id,
                second.session_id,
                second.playback_id,
                TrackEndReason::Stopped,
            )
            .await;

        let played = runtime.played();
        assert_eq!(played.len(), 3);
        assert_eq!(played[2].key, "third");
        let preview = playback.queue_preview(guild_id, 8).unwrap();
        assert_eq!(preview.current().unwrap().metadata.title.as_ref(), "third");
        assert_eq!(preview.total_queued(), 0);
    }

    #[tokio::test]
    async fn disconnect_during_automix_overlap_stops_both_tracks_and_ignores_stale_events() {
        let guild_id = GuildKey::new(16);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new_with_automix(
            media.clone(),
            runtime.clone(),
            automix_config(Duration::from_millis(200)),
        );
        for key in ["first", "second", "third"] {
            media.resolve_with(
                key,
                Ok(track_request_with_duration(
                    key,
                    Some(Duration::from_secs(1)),
                )),
            );
            playback.enqueue_impl(guild_id, key).await.unwrap();
        }

        let first = runtime.played()[0].clone();
        playback
            .prefetch_transition(guild_id, first.session_id, first.playback_id)
            .await;
        playback
            .transition(guild_id, first.session_id, first.playback_id)
            .await;
        let second = runtime.played()[1].clone();

        playback.disconnect_guild(guild_id).await;
        let stopped = runtime.stopped();
        assert!(stopped.contains(&first.playback_id));
        assert!(stopped.contains(&second.playback_id));
        assert_eq!(runtime.disconnects(), vec![guild_id]);
        assert!(playback.queue_preview(guild_id, 8).is_none());

        playback
            .advance(
                guild_id,
                first.session_id,
                first.playback_id,
                TrackEndReason::Stopped,
            )
            .await;
        playback
            .advance(
                guild_id,
                second.session_id,
                second.playback_id,
                TrackEndReason::Stopped,
            )
            .await;

        assert_eq!(runtime.played().len(), 2);
        assert!(playback.queue_preview(guild_id, 8).is_none());
    }

    #[tokio::test]
    async fn disconnect_while_automix_prepare_is_blocked_prevents_incoming_track() {
        let guild_id = GuildKey::new(17);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new_with_automix(
            media.clone(),
            runtime.clone(),
            automix_config(Duration::from_millis(200)),
        );
        media.resolve_with(
            "first",
            Ok(track_request_with_duration(
                "first",
                Some(Duration::from_secs(1)),
            )),
        );
        media.resolve_with(
            "second",
            Ok(track_request_with_duration(
                "second",
                Some(Duration::from_secs(1)),
            )),
        );

        playback.enqueue_impl(guild_id, "first").await.unwrap();
        playback.enqueue_impl(guild_id, "second").await.unwrap();
        let first = runtime.played()[0].clone();
        let prepare_release = media.block_prepare("second");
        let transition = {
            let playback = playback.clone();
            tokio::spawn(async move {
                playback
                    .prefetch_transition(guild_id, first.session_id, first.playback_id)
                    .await;
            })
        };
        media.wait_for_prepare_count(2).await;

        timeout(
            Duration::from_millis(100),
            playback.disconnect_guild(guild_id),
        )
        .await
        .expect("disconnect should not wait for blocked AutoMix prepare");
        prepare_release
            .send(Ok(track_request_with_duration(
                "second",
                Some(Duration::from_secs(1)),
            )))
            .expect("prepare receiver dropped");
        timeout(Duration::from_secs(1), transition)
            .await
            .expect("transition did not return")
            .expect("transition task panicked");

        assert_eq!(runtime.played().len(), 1);
        assert!(runtime.stopped().contains(&first.playback_id));
        assert_eq!(runtime.disconnects(), vec![guild_id]);
        assert!(playback.queue_preview(guild_id, 8).is_none());
    }

    #[tokio::test]
    async fn late_prefetch_is_discarded_after_transition_deadline() {
        let guild_id = GuildKey::new(19);
        let media = MockMedia::default();
        let runtime = MockRuntime::default();
        let playback = PlaybackCoordinator::new_with_automix(
            media.clone(),
            runtime.clone(),
            automix_config(Duration::from_millis(20)),
        );
        for key in ["first", "second"] {
            media.resolve_with(
                key,
                Ok(track_request_with_duration(
                    key,
                    Some(Duration::from_secs(1)),
                )),
            );
            playback.enqueue_impl(guild_id, key).await.unwrap();
        }
        let first = runtime.played()[0].clone();
        let prepare_release = media.block_prepare("second");
        let prefetch = {
            let playback = playback.clone();
            tokio::spawn(async move {
                playback
                    .prefetch_transition(guild_id, first.session_id, first.playback_id)
                    .await
            })
        };
        media.wait_for_prepare_count(2).await;

        playback
            .transition(guild_id, first.session_id, first.playback_id)
            .await;
        prepare_release
            .send(Ok(track_request_with_duration(
                "second",
                Some(Duration::from_secs(1)),
            )))
            .unwrap();
        prefetch.await.unwrap();

        assert_eq!(runtime.played().len(), 1);
        let preview = playback.queue_preview(guild_id, 8).unwrap();
        assert_eq!(preview.current().unwrap().metadata.title.as_ref(), "first");
        assert_eq!(preview.upcoming()[0].metadata.title.as_ref(), "second");
    }

    #[tokio::test]
    async fn restart_snapshot_restores_current_queue_and_loop_mode() {
        let guild_id = GuildKey::new(20);
        let source_media = MockMedia::default();
        let source = PlaybackCoordinator::new(source_media.clone(), MockRuntime::default());
        source_media.resolve_with("first", Ok(track_request("first")));
        source_media.resolve_with("second", Ok(track_request("second")));
        source.enqueue_impl(guild_id, "first").await.unwrap();
        source.enqueue_impl(guild_id, "second").await.unwrap();
        assert_eq!(source.toggle_loop(guild_id).await, Some(true));
        let snapshot = source.restart_snapshot(guild_id).await.unwrap();

        let restored_media = MockMedia::default();
        restored_media.resolve_with(
            &snapshot.current_source_url,
            Ok(track_request("restored-first")),
        );
        restored_media.resolve_with(
            &snapshot.queued_source_urls[0],
            Ok(track_request("restored-second")),
        );
        let restored = PlaybackCoordinator::new(restored_media, MockRuntime::default());
        assert!(restored.restore_restart_snapshot(guild_id, snapshot).await);

        let preview = restored.queue_preview(guild_id, 8).unwrap();
        assert_eq!(
            preview.current().unwrap().metadata.title.as_ref(),
            "restored-first"
        );
        assert_eq!(
            preview.upcoming()[0].metadata.title.as_ref(),
            "restored-second"
        );
        assert_eq!(restored.toggle_loop(guild_id).await, Some(false));
    }
}
