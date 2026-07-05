use wotoha_contracts::{
    ChannelKey, EnqueueOutcome, GuildKey, PlaybackService, UserKey, VoiceActionAccess,
    VoicePeerSnapshot, VoiceUpdateDecision,
};
use wotoha_core::QueuePreview;

#[derive(Clone)]
pub struct ControlService<P: PlaybackService> {
    playback: P,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComponentAction {
    Skip,
    Loop,
    Shuffle,
    AutoMix,
    Queue { limit: usize },
}

#[derive(Clone, Debug)]
pub enum ComponentOutcome {
    Skip { was_looping: bool },
    Loop { enabled: bool },
    LoopBlockedByAutoMix,
    Shuffle,
    NothingToShuffle,
    AutoMix { enabled: bool },
    QueuePreview(Box<QueuePreview>),
    QueueEmpty,
    NoTrackPlaying,
    VoiceChannelRequired,
}

impl<P: PlaybackService> ControlService<P> {
    pub fn new(playback: P) -> Self {
        Self { playback }
    }

    pub async fn play(
        &self,
        guild_id: GuildKey,
        source_url: &str,
    ) -> Result<EnqueueOutcome, P::Error> {
        self.playback.enqueue(guild_id, source_url).await
    }

    pub fn voice_action_access(
        &self,
        guild_id: GuildKey,
        actor_channel: Option<ChannelKey>,
    ) -> VoiceActionAccess {
        self.playback.voice_action_access(guild_id, actor_channel)
    }

    pub async fn handle_component(
        &self,
        guild_id: GuildKey,
        actor_channel: Option<ChannelKey>,
        action: ComponentAction,
    ) -> ComponentOutcome {
        if !matches!(
            self.voice_action_access(guild_id, actor_channel),
            VoiceActionAccess::SameChannel { .. }
        ) {
            return ComponentOutcome::VoiceChannelRequired;
        }

        match action {
            ComponentAction::Skip => match self.playback.skip(guild_id).await {
                Some(was_looping) => ComponentOutcome::Skip { was_looping },
                None => ComponentOutcome::NoTrackPlaying,
            },
            ComponentAction::Loop => {
                if self.playback.automix_enabled(guild_id) {
                    ComponentOutcome::LoopBlockedByAutoMix
                } else {
                    match self.playback.toggle_loop(guild_id).await {
                        Some(enabled) => ComponentOutcome::Loop { enabled },
                        None => ComponentOutcome::NoTrackPlaying,
                    }
                }
            }
            ComponentAction::Shuffle => {
                if self.playback.shuffle(guild_id).await {
                    ComponentOutcome::Shuffle
                } else {
                    ComponentOutcome::NothingToShuffle
                }
            }
            ComponentAction::AutoMix => match self.playback.toggle_automix(guild_id).await {
                Some(enabled) => ComponentOutcome::AutoMix { enabled },
                None => ComponentOutcome::NoTrackPlaying,
            },
            ComponentAction::Queue { limit } => {
                match self.playback.queue_preview(guild_id, limit) {
                    Some(preview) if preview.current().is_some() || preview.total_queued() > 0 => {
                        ComponentOutcome::QueuePreview(Box::new(preview))
                    }
                    _ => ComponentOutcome::QueueEmpty,
                }
            }
        }
    }

    pub fn has_current_track(&self, guild_id: GuildKey) -> bool {
        self.playback.has_current_track(guild_id)
    }

    pub fn automix_enabled(&self, guild_id: GuildKey) -> bool {
        self.playback.automix_enabled(guild_id)
    }

    pub async fn disconnect_guild(&self, guild_id: GuildKey) {
        self.playback.disconnect_guild(guild_id).await;
    }

    pub fn bootstrap_voice_state(
        &self,
        guild_id: GuildKey,
        bot_channel: ChannelKey,
        peers: Vec<VoicePeerSnapshot>,
    ) {
        self.playback
            .bootstrap_voice_state(guild_id, bot_channel, peers);
    }

    pub fn update_bot_voice_channel(&self, guild_id: GuildKey, new_channel: Option<ChannelKey>) {
        self.playback
            .update_bot_voice_channel(guild_id, new_channel);
    }

    pub fn clear_voice_state(&self, guild_id: GuildKey) {
        self.playback.clear_voice_state(guild_id);
    }

    pub fn apply_peer_voice_state(
        &self,
        guild_id: GuildKey,
        user_id: UserKey,
        old_channel: Option<ChannelKey>,
        new_channel: Option<ChannelKey>,
    ) -> VoiceUpdateDecision {
        self.playback
            .apply_peer_voice_state(guild_id, user_id, old_channel, new_channel)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fmt,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use wotoha_contracts::{
        ChannelKey, EnqueueOutcome, GuildKey, PlaybackService, UserKey, VoiceActionAccess,
        VoicePeerSnapshot, VoiceUpdateDecision,
    };
    use wotoha_core::{PreparedSource, QueuePreview, TrackMetadata, TrackRequest};

    use super::{ComponentAction, ComponentOutcome, ControlService};

    const GUILD: GuildKey = GuildKey::new(1);
    const CHANNEL: ChannelKey = ChannelKey::new(10);
    const OTHER_CHANNEL: ChannelKey = ChannelKey::new(11);

    #[derive(Clone, Default)]
    struct MockPlayback {
        state: Arc<Mutex<MockState>>,
    }

    #[derive(Default)]
    struct MockState {
        access: Option<VoiceActionAccess>,
        skip: Option<bool>,
        loop_result: Option<bool>,
        automix_enabled: bool,
        shuffle: bool,
        preview: Option<QueuePreview>,
        disconnected: usize,
    }

    #[derive(Debug)]
    struct MockError;

    impl fmt::Display for MockError {
        fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
            out.write_str("mock playback error")
        }
    }

    impl Error for MockError {}

    #[async_trait]
    impl PlaybackService for MockPlayback {
        type Error = MockError;

        async fn enqueue(
            &self,
            _guild_id: GuildKey,
            source_url: &str,
        ) -> Result<EnqueueOutcome, Self::Error> {
            Ok(EnqueueOutcome {
                now_playing: true,
                request: track(source_url),
            })
        }

        fn queue_preview(&self, _guild_id: GuildKey, _limit: usize) -> Option<QueuePreview> {
            self.state.lock().expect("mock state").preview.clone()
        }

        async fn toggle_loop(&self, _guild_id: GuildKey) -> Option<bool> {
            self.state.lock().expect("mock state").loop_result
        }

        async fn skip(&self, _guild_id: GuildKey) -> Option<bool> {
            self.state.lock().expect("mock state").skip
        }

        fn has_current_track(&self, _guild_id: GuildKey) -> bool {
            self.state
                .lock()
                .expect("mock state")
                .preview
                .as_ref()
                .and_then(QueuePreview::current)
                .is_some()
        }

        async fn shuffle(&self, _guild_id: GuildKey) -> bool {
            self.state.lock().expect("mock state").shuffle
        }

        fn automix_enabled(&self, _guild_id: GuildKey) -> bool {
            self.state.lock().expect("mock state").automix_enabled
        }

        async fn disconnect_guild(&self, _guild_id: GuildKey) {
            self.state.lock().expect("mock state").disconnected += 1;
        }

        fn bootstrap_voice_state(
            &self,
            _guild_id: GuildKey,
            _bot_channel: ChannelKey,
            _peers: Vec<VoicePeerSnapshot>,
        ) {
        }

        fn update_bot_voice_channel(&self, _guild_id: GuildKey, _new_channel: Option<ChannelKey>) {}

        fn clear_voice_state(&self, _guild_id: GuildKey) {}

        fn apply_peer_voice_state(
            &self,
            _guild_id: GuildKey,
            _user_id: UserKey,
            _old_channel: Option<ChannelKey>,
            _new_channel: Option<ChannelKey>,
        ) -> VoiceUpdateDecision {
            VoiceUpdateDecision::Ignore
        }

        fn voice_action_access(
            &self,
            _guild_id: GuildKey,
            actor_channel: Option<ChannelKey>,
        ) -> VoiceActionAccess {
            self.state
                .lock()
                .expect("mock state")
                .access
                .unwrap_or_else(|| {
                    actor_channel.map_or(VoiceActionAccess::UserNotInVoice, |channel_id| {
                        if channel_id == CHANNEL {
                            VoiceActionAccess::SameChannel { channel_id }
                        } else {
                            VoiceActionAccess::DifferentChannel {
                                active_channel: CHANNEL,
                                actor_channel: channel_id,
                            }
                        }
                    })
                })
        }
    }

    #[tokio::test]
    async fn component_rejects_user_outside_active_voice_channel() {
        let playback = MockPlayback::default();
        let service = ControlService::new(playback);

        let outcome = service
            .handle_component(GUILD, Some(OTHER_CHANNEL), ComponentAction::Skip)
            .await;

        assert!(matches!(outcome, ComponentOutcome::VoiceChannelRequired));
    }

    #[tokio::test]
    async fn component_skip_reports_loop_state() {
        let playback = MockPlayback::default();
        playback.state.lock().expect("mock state").skip = Some(true);
        let service = ControlService::new(playback);

        let outcome = service
            .handle_component(GUILD, Some(CHANNEL), ComponentAction::Skip)
            .await;

        assert!(matches!(
            outcome,
            ComponentOutcome::Skip { was_looping: true }
        ));
    }

    #[tokio::test]
    async fn component_queue_distinguishes_empty_and_present_queue() {
        let playback = MockPlayback::default();
        let service = ControlService::new(playback.clone());

        let empty = service
            .handle_component(GUILD, Some(CHANNEL), ComponentAction::Queue { limit: 10 })
            .await;
        assert!(matches!(empty, ComponentOutcome::QueueEmpty));

        let mut state = wotoha_core::GuildPlayerState::default();
        state.enqueue(track("https://example.com/one"));
        playback.state.lock().expect("mock state").preview = Some(state.queue_preview(10));

        let present = service
            .handle_component(GUILD, Some(CHANNEL), ComponentAction::Queue { limit: 10 })
            .await;
        assert!(matches!(present, ComponentOutcome::QueuePreview(_)));
    }

    #[tokio::test]
    async fn component_shuffle_reports_empty_queue() {
        let playback = MockPlayback::default();
        let service = ControlService::new(playback);

        let outcome = service
            .handle_component(GUILD, Some(CHANNEL), ComponentAction::Shuffle)
            .await;

        assert!(matches!(outcome, ComponentOutcome::NothingToShuffle));
    }

    #[tokio::test]
    async fn component_blocks_loop_while_automix_is_enabled() {
        let playback = MockPlayback::default();
        {
            let mut state = playback.state.lock().expect("mock state");
            state.automix_enabled = true;
            state.loop_result = Some(true);
        }
        let service = ControlService::new(playback);

        let outcome = service
            .handle_component(GUILD, Some(CHANNEL), ComponentAction::Loop)
            .await;

        assert!(matches!(outcome, ComponentOutcome::LoopBlockedByAutoMix));
    }

    fn track(source_url: &str) -> TrackRequest {
        TrackRequest::new(
            "test",
            source_url,
            source_url,
            source_url,
            source_url,
            PreparedSource::http(source_url, Vec::new(), None, None),
            TrackMetadata::new("title", "author", source_url, None, None),
        )
    }
}
