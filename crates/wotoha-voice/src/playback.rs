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
use serenity::all::{ChannelId, GuildId, UserId};
use songbird::{
    Songbird,
    events::{Event, EventContext, EventHandler as VoiceEventHandler, TrackEvent},
    tracks::TrackHandle,
};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tracing::warn;
use wotoha_contracts::{
    EnqueueOutcome, MediaBackend, PlaybackService, VoicePeerSnapshot, VoiceUpdateDecision,
};
use wotoha_core::{GuildPlayerState, QueuePreview, TrackRequest};

type CompletionSender<E> = oneshot::Sender<Result<EnqueueOutcome, PlaybackError<E>>>;

#[derive(Clone)]
pub struct PlaybackCoordinator<M: MediaBackend> {
    inner: Arc<PlaybackCoordinatorInner<M>>,
}

struct PlaybackCoordinatorInner<M: MediaBackend> {
    media: M,
    sessions: DashMap<GuildId, Arc<GuildSession<M::Error>>>,
    next_session_id: AtomicU64,
}

struct GuildSession<E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    id: u64,
    operation: AsyncMutex<()>,
    playback: Mutex<PlaybackRuntime<E>>,
    voice: Mutex<GuildVoiceIndex>,
}

struct PlaybackRuntime<E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    logical: GuildPlayerState,
    active_handle: Option<TrackHandle>,
    next_enqueue_seq: u64,
    next_commit_seq: u64,
    pending_enqueues: BTreeMap<u64, PendingEnqueue<E>>,
}

struct PendingEnqueue<E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    outcome: Option<Result<TrackRequest, PlaybackError<E>>>,
    completion: Option<CompletionSender<E>>,
}

#[derive(Default)]
struct GuildVoiceIndex {
    bot_channel: Option<ChannelId>,
    peers_by_user: HashMap<UserId, ChannelId>,
    peer_counts_by_channel: HashMap<ChannelId, usize>,
    bootstrapped: bool,
}

#[derive(Debug, Error)]
pub enum PlaybackError<E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    #[error(transparent)]
    Resolve(E),
    #[error("voice session is missing")]
    NoCall,
    #[error("session state changed while processing the request")]
    SessionExpired,
}

enum FlushAction<E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    WaitForEarlier,
    CommitQueued {
        completion: Option<CompletionSender<E>>,
        request: TrackRequest,
    },
    Fail {
        completion: Option<CompletionSender<E>>,
        error: PlaybackError<E>,
    },
    StartCurrent {
        completion: Option<CompletionSender<E>>,
        request: TrackRequest,
    },
}

impl<E> Default for PlaybackRuntime<E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn default() -> Self {
        Self {
            logical: GuildPlayerState::default(),
            active_handle: None,
            next_enqueue_seq: 0,
            next_commit_seq: 0,
            pending_enqueues: BTreeMap::new(),
        }
    }
}

impl<M: MediaBackend> PlaybackCoordinator<M> {
    pub fn new(media: M) -> Self {
        Self {
            inner: Arc::new(PlaybackCoordinatorInner {
                media,
                sessions: DashMap::new(),
                next_session_id: AtomicU64::new(1),
            }),
        }
    }

    fn get_session(&self, guild_id: GuildId) -> Option<Arc<GuildSession<M::Error>>> {
        self.inner
            .sessions
            .get(&guild_id)
            .map(|entry| entry.clone())
    }

    fn get_or_create_session(&self, guild_id: GuildId) -> Arc<GuildSession<M::Error>> {
        self.inner
            .sessions
            .entry(guild_id)
            .or_insert_with(|| {
                Arc::new(GuildSession {
                    id: self.inner.next_session_id.fetch_add(1, Ordering::Relaxed),
                    operation: AsyncMutex::new(()),
                    playback: Mutex::new(PlaybackRuntime::default()),
                    voice: Mutex::new(GuildVoiceIndex::default()),
                })
            })
            .clone()
    }

    fn session_is_current(&self, guild_id: GuildId, session_id: u64) -> bool {
        let Some(session) = self.get_session(guild_id) else {
            return false;
        };

        session.id == session_id
    }

    async fn enqueue_impl(
        &self,
        manager: Arc<Songbird>,
        guild_id: GuildId,
        source_url: &str,
    ) -> Result<EnqueueOutcome, PlaybackError<M::Error>> {
        let session = self.get_or_create_session(guild_id);
        let session_id = session.id;

        let (sequence, completion) = {
            let _operation = session.operation.lock().await;
            let mut playback = session.playback.lock();
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
        if let Err(error) = self
            .finish_enqueue(
                manager,
                guild_id,
                session.clone(),
                session_id,
                sequence,
                resolved,
            )
            .await
        {
            return Err(error);
        }

        completion
            .await
            .unwrap_or(Err(PlaybackError::SessionExpired))
    }

    pub fn queue_preview(&self, guild_id: GuildId, limit: usize) -> Option<QueuePreview> {
        let session = self.get_session(guild_id)?;
        let playback = session.playback.lock();
        Some(playback.logical.queue_preview(limit))
    }

    pub async fn toggle_loop(&self, guild_id: GuildId) -> Option<bool> {
        let session = self.get_session(guild_id)?;
        let _operation = session.operation.lock().await;
        let mut playback = session.playback.lock();
        Some(playback.logical.toggle_loop())
    }

    pub async fn skip(&self, guild_id: GuildId) -> Option<bool> {
        let session = self.get_session(guild_id)?;
        let _operation = session.operation.lock().await;

        let (was_looping, handle) = {
            let mut playback = session.playback.lock();
            let was_looping = playback.logical.disable_loop();
            let handle = playback.active_handle.take();
            (was_looping, handle)
        };

        if let Some(handle) = handle {
            let _ = handle.stop();
        }

        Some(was_looping)
    }

    pub fn has_current_track(&self, guild_id: GuildId) -> bool {
        let Some(session) = self.get_session(guild_id) else {
            return false;
        };

        let playback = session.playback.lock();
        playback.logical.current().is_some()
    }

    pub async fn shuffle(&self, guild_id: GuildId) -> bool {
        let Some(session) = self.get_session(guild_id) else {
            return false;
        };

        let _operation = session.operation.lock().await;
        let mut playback = session.playback.lock();
        playback.logical.shuffle()
    }

    pub async fn disconnect_guild(&self, manager: Arc<Songbird>, guild_id: GuildId) {
        let session = self
            .inner
            .sessions
            .remove(&guild_id)
            .map(|(_, session)| session);

        if let Some(session) = session {
            let _operation = session.operation.lock().await;
            let (handle, pending) = {
                let mut playback = session.playback.lock();
                playback.logical.clear();
                let handle = playback.active_handle.take();
                let pending = drain_pending(&mut playback);
                (handle, pending)
            };
            session.voice.lock().clear();

            if let Some(handle) = handle {
                let _ = handle.stop();
            }
            complete_all(pending, || PlaybackError::SessionExpired);
        }

        let _ = manager.remove(guild_id).await;
    }

    pub fn bootstrap_voice_state(
        &self,
        guild_id: GuildId,
        bot_channel: ChannelId,
        peers: Vec<VoicePeerSnapshot>,
    ) {
        let session = self.get_or_create_session(guild_id);
        let mut voice = session.voice.lock();
        voice.bootstrap(bot_channel, peers);
    }

    pub fn update_bot_voice_channel(&self, guild_id: GuildId, new_channel: Option<ChannelId>) {
        match new_channel {
            Some(channel_id) => {
                let session = self.get_or_create_session(guild_id);
                session.voice.lock().update_bot_channel(Some(channel_id));
            }
            None => self.clear_voice_state(guild_id),
        }
    }

    pub fn clear_voice_state(&self, guild_id: GuildId) {
        let Some(session) = self.get_session(guild_id) else {
            return;
        };

        session.voice.lock().clear();
    }

    pub fn apply_peer_voice_state(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        old_channel: Option<ChannelId>,
        new_channel: Option<ChannelId>,
    ) -> VoiceUpdateDecision {
        let Some(session) = self.get_session(guild_id) else {
            return VoiceUpdateDecision::Ignore;
        };

        let mut voice = session.voice.lock();
        voice.apply_peer_update(user_id, old_channel, new_channel)
    }

    async fn finish_enqueue(
        &self,
        manager: Arc<Songbird>,
        guild_id: GuildId,
        session: Arc<GuildSession<M::Error>>,
        session_id: u64,
        sequence: u64,
        resolved: Result<TrackRequest, PlaybackError<M::Error>>,
    ) -> Result<(), PlaybackError<M::Error>> {
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

        self.flush_ready_prefix(manager, guild_id, &session, session_id)
            .await;
        Ok(())
    }

    async fn flush_ready_prefix(
        &self,
        manager: Arc<Songbird>,
        guild_id: GuildId,
        session: &Arc<GuildSession<M::Error>>,
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
                } => match self
                    .play_request(manager.clone(), guild_id, session_id, &request)
                    .await
                {
                    Ok(handle) => {
                        let mut playback = session.playback.lock();
                        playback.active_handle = Some(handle);
                        if let Some(completion) = completion.take() {
                            let _ = completion.send(Ok(EnqueueOutcome {
                                now_playing: true,
                                request,
                            }));
                        }
                    }
                    Err(PlaybackError::NoCall) => {
                        {
                            let mut playback = session.playback.lock();
                            playback.logical.clear_current();
                            let pending = drain_pending(&mut playback);
                            drop(playback);
                            if let Some(completion) = completion.take() {
                                let _ = completion.send(Err(PlaybackError::NoCall));
                            }
                            complete_all(pending, || PlaybackError::NoCall);
                        }
                        return;
                    }
                    Err(PlaybackError::SessionExpired) => {
                        {
                            let mut playback = session.playback.lock();
                            playback.logical.clear_current();
                            let pending = drain_pending(&mut playback);
                            drop(playback);
                            if let Some(completion) = completion.take() {
                                let _ = completion.send(Err(PlaybackError::SessionExpired));
                            }
                            complete_all(pending, || PlaybackError::SessionExpired);
                        }
                        return;
                    }
                    Err(PlaybackError::Resolve(error)) => {
                        let mut playback = session.playback.lock();
                        playback.logical.clear_current();
                        drop(playback);
                        if let Some(completion) = completion.take() {
                            let _ = completion.send(Err(PlaybackError::Resolve(error)));
                        }
                    }
                },
            }
        }
    }

    async fn advance(&self, manager: Arc<Songbird>, guild_id: GuildId, session_id: u64) {
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

        loop {
            let next = {
                let mut playback = session.playback.lock();
                playback.active_handle = None;
                playback.logical.prepare_next_track()
            };

            let Some(next) = next else {
                return;
            };

            match self
                .play_request(manager.clone(), guild_id, session_id, &next)
                .await
            {
                Ok(handle) => {
                    let mut playback = session.playback.lock();
                    playback.logical.replace_current(next);
                    playback.active_handle = Some(handle);
                    return;
                }
                Err(PlaybackError::NoCall | PlaybackError::SessionExpired) => return,
                Err(error) => {
                    warn!(guild_id = guild_id.get(), error = %error, "failed to start next track");
                    let mut playback = session.playback.lock();
                    playback.logical.disable_loop();
                    playback.logical.clear_current();
                }
            }
        }
    }

    async fn play_request(
        &self,
        manager: Arc<Songbird>,
        guild_id: GuildId,
        session_id: u64,
        request: &TrackRequest,
    ) -> Result<TrackHandle, PlaybackError<M::Error>> {
        if !self.session_is_current(guild_id, session_id) {
            return Err(PlaybackError::SessionExpired);
        }

        let Some(call_lock) = manager.get(guild_id) else {
            return Err(PlaybackError::NoCall);
        };

        let input = self
            .inner
            .media
            .open_input(request)
            .map_err(PlaybackError::Resolve)?;
        let handle = {
            let mut call = call_lock.lock().await;
            call.play_input(input)
        };

        if !self.session_is_current(guild_id, session_id) {
            let _ = handle.stop();
            return Err(PlaybackError::SessionExpired);
        }

        let _ = handle.set_volume(0.10);
        let _ = handle.add_event(
            Event::Track(TrackEvent::End),
            TrackEndNotifier {
                guild_id,
                session_id,
                manager,
                playback: self.clone(),
            },
        );

        Ok(handle)
    }
}

#[async_trait]
impl<M> PlaybackService for PlaybackCoordinator<M>
where
    M: MediaBackend,
{
    type Error = PlaybackError<M::Error>;

    async fn enqueue(
        &self,
        manager: Arc<Songbird>,
        guild_id: GuildId,
        source_url: &str,
    ) -> Result<EnqueueOutcome, Self::Error> {
        self.enqueue_impl(manager, guild_id, source_url).await
    }

    fn queue_preview(&self, guild_id: GuildId, limit: usize) -> Option<QueuePreview> {
        Self::queue_preview(self, guild_id, limit)
    }

    async fn toggle_loop(&self, guild_id: GuildId) -> Option<bool> {
        Self::toggle_loop(self, guild_id).await
    }

    async fn skip(&self, guild_id: GuildId) -> Option<bool> {
        Self::skip(self, guild_id).await
    }

    fn has_current_track(&self, guild_id: GuildId) -> bool {
        Self::has_current_track(self, guild_id)
    }

    async fn shuffle(&self, guild_id: GuildId) -> bool {
        Self::shuffle(self, guild_id).await
    }

    async fn disconnect_guild(&self, manager: Arc<Songbird>, guild_id: GuildId) {
        Self::disconnect_guild(self, manager, guild_id).await;
    }

    fn bootstrap_voice_state(
        &self,
        guild_id: GuildId,
        bot_channel: ChannelId,
        peers: Vec<VoicePeerSnapshot>,
    ) {
        Self::bootstrap_voice_state(self, guild_id, bot_channel, peers);
    }

    fn update_bot_voice_channel(&self, guild_id: GuildId, new_channel: Option<ChannelId>) {
        Self::update_bot_voice_channel(self, guild_id, new_channel);
    }

    fn clear_voice_state(&self, guild_id: GuildId) {
        Self::clear_voice_state(self, guild_id);
    }

    fn apply_peer_voice_state(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        old_channel: Option<ChannelId>,
        new_channel: Option<ChannelId>,
    ) -> VoiceUpdateDecision {
        Self::apply_peer_voice_state(self, guild_id, user_id, old_channel, new_channel)
    }
}

impl GuildVoiceIndex {
    fn bootstrap(&mut self, bot_channel: ChannelId, peers: Vec<VoicePeerSnapshot>) {
        self.clear();
        self.bot_channel = Some(bot_channel);
        self.bootstrapped = true;

        for peer in peers {
            self.set_peer_channel(peer.user_id, Some(peer.channel_id));
        }
    }

    fn update_bot_channel(&mut self, new_channel: Option<ChannelId>) {
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
        user_id: UserId,
        old_channel: Option<ChannelId>,
        new_channel: Option<ChannelId>,
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

    fn set_peer_channel(&mut self, user_id: UserId, new_channel: Option<ChannelId>) {
        if let Some(previous_channel) = self.peers_by_user.remove(&user_id) {
            self.decrement_channel(previous_channel);
        }

        if let Some(channel_id) = new_channel {
            self.peers_by_user.insert(user_id, channel_id);
            *self.peer_counts_by_channel.entry(channel_id).or_default() += 1;
        }
    }

    fn decrement_channel(&mut self, channel_id: ChannelId) {
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

fn take_flush_action<E>(playback: &mut PlaybackRuntime<E>) -> FlushAction<E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    let sequence = playback.next_commit_seq;
    let Some(pending) = playback.pending_enqueues.get_mut(&sequence) else {
        return FlushAction::WaitForEarlier;
    };
    let Some(outcome) = pending.outcome.take() else {
        return FlushAction::WaitForEarlier;
    };

    let mut pending = playback
        .pending_enqueues
        .remove(&sequence)
        .expect("pending enqueue should still exist");
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

fn drain_pending<E>(playback: &mut PlaybackRuntime<E>) -> Vec<CompletionSender<E>>
where
    E: std::error::Error + Send + Sync + 'static,
{
    std::mem::take(&mut playback.pending_enqueues)
        .into_iter()
        .filter_map(|(_, mut pending)| pending.completion.take())
        .collect()
}

fn remove_pending<E>(session: &GuildSession<E>, sequence: u64) -> Option<CompletionSender<E>>
where
    E: std::error::Error + Send + Sync + 'static,
{
    let mut playback = session.playback.lock();
    playback
        .pending_enqueues
        .remove(&sequence)
        .and_then(|mut pending| pending.completion.take())
}

fn complete_all<E, F>(pending: Vec<CompletionSender<E>>, error: F)
where
    E: std::error::Error + Send + Sync + 'static,
    F: Fn() -> PlaybackError<E>,
{
    for completion in pending {
        let _ = completion.send(Err(error()));
    }
}

struct TrackEndNotifier<M: MediaBackend> {
    guild_id: GuildId,
    session_id: u64,
    manager: Arc<Songbird>,
    playback: PlaybackCoordinator<M>,
}

#[serenity::async_trait]
impl<M> VoiceEventHandler for TrackEndNotifier<M>
where
    M: MediaBackend,
{
    async fn act(&self, _ctx: &EventContext<'_>) -> Option<Event> {
        tokio::spawn({
            let playback = self.playback.clone();
            let manager = self.manager.clone();
            let guild_id = self.guild_id;
            let session_id = self.session_id;
            async move {
                playback.advance(manager, guild_id, session_id).await;
            }
        });

        None
    }
}
