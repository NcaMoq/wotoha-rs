use std::{error::Error, sync::Arc};

use async_trait::async_trait;
use serenity::all::{ChannelId, GuildId, UserId};
use songbird::{Songbird, input::Input};
use wotoha_core::{QueuePreview, TrackRequest};

#[derive(Clone, Debug)]
pub struct EnqueueOutcome {
    pub now_playing: bool,
    pub request: TrackRequest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VoicePeerSnapshot {
    pub user_id: UserId,
    pub channel_id: ChannelId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoiceUpdateDecision {
    Ignore,
    StayConnected,
    DisconnectAlone,
}

#[async_trait]
pub trait MediaBackend: Clone + Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    async fn resolve(&self, source_url: &str) -> Result<TrackRequest, Self::Error>;
    fn open_input(&self, request: &TrackRequest) -> Result<Input, Self::Error>;
}

#[async_trait]
pub trait PlaybackService: Clone + Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    async fn enqueue(
        &self,
        manager: Arc<Songbird>,
        guild_id: GuildId,
        source_url: &str,
    ) -> Result<EnqueueOutcome, Self::Error>;

    fn queue_preview(&self, guild_id: GuildId, limit: usize) -> Option<QueuePreview>;
    async fn toggle_loop(&self, guild_id: GuildId) -> Option<bool>;
    async fn skip(&self, guild_id: GuildId) -> Option<bool>;
    fn has_current_track(&self, guild_id: GuildId) -> bool;
    async fn shuffle(&self, guild_id: GuildId) -> bool;
    async fn disconnect_guild(&self, manager: Arc<Songbird>, guild_id: GuildId);

    fn bootstrap_voice_state(
        &self,
        guild_id: GuildId,
        bot_channel: ChannelId,
        peers: Vec<VoicePeerSnapshot>,
    );

    fn update_bot_voice_channel(&self, guild_id: GuildId, new_channel: Option<ChannelId>);
    fn clear_voice_state(&self, guild_id: GuildId);

    fn apply_peer_voice_state(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        old_channel: Option<ChannelId>,
        new_channel: Option<ChannelId>,
    ) -> VoiceUpdateDecision;
}
