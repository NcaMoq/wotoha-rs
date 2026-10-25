use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use dashmap::DashMap;
use futures::future::join_all;
use serenity::{
    all::{
        ActivityData, ButtonStyle, ChannelId, Colour, Command, CommandInteraction,
        CommandOptionType, ComponentInteraction, Context, CreateActionRow, CreateAllowedMentions,
        CreateButton, CreateCommand, CreateCommandOption, CreateComponentV2, CreateEmbed,
        CreateInteractionResponse, CreateInteractionResponseFollowup,
        CreateInteractionResponseMessage, CreateSectionAccessory, EditMessage, EmojiId, Guild,
        Interaction, MessageFlags, MessageId, ReactionType, Ready, UnavailableGuild,
        VoiceServerUpdateEvent, VoiceState,
    },
    async_trait,
    builder::CreateMessage,
    cache::Settings as CacheSettings,
    client::{Client, EventHandler},
    http::Http,
};
use songbird::{SerenityInit, Songbird};
use tracing::{error, info, warn};
use wotoha_contracts::{
    ChannelKey, GuildKey, PlaybackFailureKind, PlaybackId, PlaybackService, UserKey,
    VoiceActionAccess, VoiceGatewayEvent, VoiceGatewayRuntime, VoiceGatewayServerUpdate,
    VoiceGatewayStateUpdate, VoicePeerSnapshot, VoiceUpdateDecision,
};
use wotoha_control::{ComponentAction, ComponentOutcome, ControlService};
use wotoha_core::{
    QueuePreview, TrackMetadata, TrackRequest, TrackRequestId,
    debug::{append_debug_log, sanitize_log_message},
    operational_metrics,
    ui::{
        AUTOMIX_EMOJI_ID, AUTOMIX_EMOJI_NAME, BUTTON_AUTOMIX, BUTTON_LOOP, BUTTON_QUEUE,
        BUTTON_SHUFFLE, BUTTON_SKIP, COLOR_ERROR, COLOR_INFO, INVITE_BUTTON_LABEL, LOOP_EMOJI_ID,
        LOOP_EMOJI_NAME, MSG_ALLOWED_URL_ONLY, MSG_JOIN_ACTIVE_VOICE, MSG_JOIN_VOICE_FIRST,
        MSG_NO_TRACK_PLAYING, MSG_PLAYING_IN_ANOTHER_VOICE, MSG_QUEUE_EMPTY,
        PLAY_COMMAND_DESCRIPTION, PLAY_COMMAND_NAME, PLAY_COMMAND_URL_MAX_LENGTH,
        PLAY_COMMAND_URL_OPTION, SHUFFLE_EMOJI_ID, SHUFFLE_EMOJI_NAME, SKIP_EMOJI_ID,
        SKIP_EMOJI_NAME,
    },
    url::summarize_url_for_logs,
};

use crate::{SongbirdRuntime, reconnect::ReconnectStore};

const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const QUEUE_PREVIEW_LIMIT: usize = 10;
const MAX_RESTART_NOTIFICATIONS: usize = 64;
const RESTART_NOTIFICATION_TIMEOUT: Duration = Duration::from_millis(750);
const MAX_RESTART_SNAPSHOTS: usize = 128;
const PLAYER_PANEL_UPDATE_INTERVAL: Duration = Duration::from_secs(1);
const PLAYER_PANEL_NO_PLAYBACK_GRACE: Duration = Duration::from_secs(30);
const EMBED_FIELD_LIMIT: usize = 1024;
const EMBED_TITLE_LIMIT: usize = 180;
const WOTOHA_INVITE_URL: &str =
    "https://discord.com/oauth2/authorize?client_id=1238488423208063107";
const UPDATE_NOTICE_TITLE: &str = "アップデートのお知らせ";
const UPDATE_NOTICE_DESCRIPTION: &str =
    "最新版へのアップデートのため、まもなく再起動します。再生中の音声は一時的に停止します。";
const MODE_NICKNAME_LOOP: &str = "🔁";
const MODE_NICKNAME_SHUFFLE: &str = "🔀";
const MODE_NICKNAME_AUTOMIX: &str = "🎧";
const BOT_BASE_NICKNAME: &str = "音葉";
const DISCORD_NICKNAME_MAX_CHARS: usize = 32;
const BUILD_VERSION: &str = match option_env!("WOTOHA_BUILD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

fn version_activity(guild_count: usize) -> ActivityData {
    let version = BUILD_VERSION.strip_prefix('v').unwrap_or(BUILD_VERSION);
    ActivityData::custom(format!("v{version} | in {guild_count} servers"))
}

fn update_version_activity(ctx: &Context) {
    ctx.set_activity(Some(version_activity(ctx.cache.guilds().len())));
}

pub fn recommended_cache_settings() -> CacheSettings {
    let mut settings = CacheSettings::default();
    settings.time_to_live = CACHE_TTL;
    settings.cache_channels = false;
    settings.cache_users = false;
    settings
}

#[derive(Clone)]
pub struct DiscordGateway<P: PlaybackService, R: VoiceGatewayRuntime> {
    control: ControlService<P>,
    runtime: R,
    startup: Arc<StartupState>,
    notification_channels: Arc<DashMap<GuildKey, ChannelId>>,
    player_panels: Arc<DashMap<GuildKey, GuildPlayerPanels>>,
    player_panel_locks: Arc<DashMap<GuildKey, Arc<tokio::sync::Mutex<()>>>>,
    player_panel_skip_pending: Arc<DashMap<GuildKey, PlaybackId>>,
    active_voice_channels: Arc<DashMap<GuildKey, ChannelKey>>,
    mode_nickname_states: Arc<DashMap<GuildKey, ModeNicknameState>>,
    reconnect_store: ReconnectStore,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ModeNicknameState {
    looping: bool,
    shuffle: bool,
    automix: bool,
}

#[derive(Clone)]
struct PlayerPanelState {
    channel_id: ChannelId,
    message_id: MessageId,
    request_id: TrackRequestId,
    playback_id: Option<PlaybackId>,
    requester: UserKey,
    last_payload: Arc<tokio::sync::Mutex<String>>,
}

#[derive(Clone)]
struct GuildPlayerPanels {
    by_request_id: HashMap<TrackRequestId, PlayerPanelState>,
    update_generation: Arc<()>,
}

impl GuildPlayerPanels {
    fn new() -> Self {
        Self {
            by_request_id: HashMap::new(),
            update_generation: Arc::new(()),
        }
    }
}

fn player_panel_no_playback_grace_expired(since: Instant, now: Instant) -> bool {
    now.saturating_duration_since(since) >= PLAYER_PANEL_NO_PLAYBACK_GRACE
}

fn is_stale_player_panel_snapshot(
    panel_playback_id: Option<PlaybackId>,
    snapshot_playback_id: PlaybackId,
) -> bool {
    panel_playback_id.is_some_and(|panel_id| snapshot_playback_id < panel_id)
}

fn is_skipped_playback_snapshot(
    skipped_playback_id: Option<PlaybackId>,
    snapshot_playback_id: PlaybackId,
) -> bool {
    skipped_playback_id == Some(snapshot_playback_id)
}

fn player_panel_for_request(
    panels: &GuildPlayerPanels,
    request_id: TrackRequestId,
) -> Option<PlayerPanelState> {
    panels.by_request_id.get(&request_id).cloned()
}

fn register_player_panel(panels: &mut GuildPlayerPanels, panel: PlayerPanelState) {
    panels.by_request_id.insert(panel.request_id, panel);
}

fn retain_live_player_panels(
    panels: &mut GuildPlayerPanels,
    live_request_ids: &HashSet<TrackRequestId>,
) {
    panels
        .by_request_id
        .retain(|request_id, _| live_request_ids.contains(request_id));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlayerPanelUpdateResult {
    Updated,
    Unchanged,
    NoPlayback,
    RetryableFailure,
    PermanentFailure(TrackRequestId),
}

#[derive(Default)]
struct StartupState {
    boot_tasks_done: AtomicBool,
}

impl<P: PlaybackService, R: VoiceGatewayRuntime> DiscordGateway<P, R> {
    pub fn new(control: ControlService<P>, runtime: R) -> Self {
        Self {
            control,
            runtime,
            startup: Arc::default(),
            notification_channels: Arc::default(),
            player_panels: Arc::default(),
            player_panel_locks: Arc::default(),
            player_panel_skip_pending: Arc::default(),
            active_voice_channels: Arc::default(),
            mode_nickname_states: Arc::default(),
            reconnect_store: ReconnectStore::from_env(),
        }
    }

    fn record_active_voice_counts(&self) {
        let count = self.active_voice_channels.len();
        operational_metrics().set_active_guilds(count);
        operational_metrics().set_active_voice_sessions(count);
    }

    async fn refresh_mode_nickname(
        &self,
        ctx: &Context,
        guild_id: serenity::all::GuildId,
        guild_key: GuildKey,
        shuffle_override: Option<bool>,
        looping_override: Option<bool>,
        automix_override: Option<bool>,
    ) {
        let previous = self
            .mode_nickname_states
            .get(&guild_key)
            .map(|state| *state)
            .unwrap_or_default();
        let state = match self.control.restart_snapshot(guild_key).await {
            Some(snapshot) => ModeNicknameState {
                looping: snapshot.looping,
                shuffle: self.control.shuffle_enabled(guild_key),
                automix: snapshot.automix_enabled,
            },
            None => ModeNicknameState {
                shuffle: self.control.shuffle_enabled(guild_key),
                automix: self.control.automix_enabled(guild_key),
                looping: previous.looping,
            },
        };
        let mut state = state;
        if let Some(shuffle) = shuffle_override {
            state.shuffle = shuffle;
        }
        if let Some(looping) = looping_override {
            state.looping = looping;
        }
        if let Some(automix) = automix_override {
            state.automix = automix;
        }
        self.mode_nickname_states.insert(guild_key, state);
        self.apply_mode_nickname(ctx, guild_id, state).await;
    }

    async fn apply_mode_nickname(
        &self,
        ctx: &Context,
        guild_id: serenity::all::GuildId,
        state: ModeNicknameState,
    ) {
        // `current_user_member` targets Discord's current-user endpoint.  Bot
        // tokens cannot use that endpoint for guild-member nickname updates;
        // fetch the bot's member record by its user id instead.
        let bot_user_id = ctx.cache.current_user().id;
        let member = match guild_id.member(&ctx.http, bot_user_id).await {
            Ok(member) => member,
            Err(error) => {
                warn!(
                    guild_id = guild_id.get(),
                    error = %error,
                    "failed to read bot member before updating mode nickname"
                );
                return;
            }
        };
        let nickname = render_mode_nickname(BOT_BASE_NICKNAME, state);
        if member.nick.as_deref() == Some(nickname.as_str()) {
            return;
        }

        if let Err(error) = guild_id.edit_nickname(&ctx.http, Some(&nickname)).await {
            warn!(
                guild_id = guild_id.get(),
                error = %error,
                "failed to update bot mode nickname"
            );
        }
    }

    async fn reset_disconnected_guild(
        &self,
        ctx: &Context,
        guild_id: serenity::all::GuildId,
        guild_key: GuildKey,
    ) {
        self.mode_nickname_states.remove(&guild_key);
        let panel_lock = self.player_panel_lock(guild_key);
        let _panel_guard = panel_lock.lock().await;
        self.player_panels.remove(&guild_key);
        self.player_panel_skip_pending.remove(&guild_key);
        drop(_panel_guard);
        self.control.disconnect_guild(guild_key).await;
        self.apply_mode_nickname(ctx, guild_id, ModeNicknameState::default())
            .await;
    }

    fn player_panel_lock(&self, guild_key: GuildKey) -> Arc<tokio::sync::Mutex<()>> {
        self.player_panel_locks
            .entry(guild_key)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    async fn publish_player_panel(
        &self,
        ctx: &Context,
        command: &CommandInteraction,
        guild_key: GuildKey,
        request: &TrackRequest,
        requested_by: UserKey,
    ) -> serenity::Result<()> {
        let panel_lock = self.player_panel_lock(guild_key);
        let _panel_guard = panel_lock.lock().await;
        let existing_panels = self
            .player_panels
            .get(&guild_key)
            .map(|panel| panel.value().clone());
        let should_start_updates = existing_panels.is_none();
        let mut panels = existing_panels.unwrap_or_else(GuildPlayerPanels::new);
        let snapshot = self
            .control
            .now_playing_snapshot(guild_key)
            .await
            .filter(|snapshot| snapshot.request_id == request.request_id);
        let position = snapshot.as_ref().and_then(|snapshot| snapshot.position);
        let playback_id = snapshot.as_ref().map(|snapshot| snapshot.playback_id);
        let components = player_panel_components(
            &request.metadata,
            request.provider_id.as_ref(),
            position,
            Some(requested_by),
        );
        let last_payload = serde_json::to_string(&components).unwrap_or_default();
        let message = command
            .create_followup(&ctx.http, player_panel_followup(components))
            .await?;
        let request_panel = PlayerPanelState {
            channel_id: message.channel_id,
            message_id: message.id,
            request_id: request.request_id,
            playback_id,
            requester: requested_by,
            last_payload: Arc::new(tokio::sync::Mutex::new(last_payload)),
        };
        register_player_panel(&mut panels, request_panel);
        let update_generation = panels.update_generation.clone();
        self.player_panels.insert(guild_key, panels);
        if should_start_updates {
            self.start_player_panel_updates(ctx.clone(), guild_key, update_generation);
        }
        Ok(())
    }

    async fn publish_queued_track_response(
        &self,
        ctx: &Context,
        command: &CommandInteraction,
        guild_key: GuildKey,
        request: &TrackRequest,
        requested_by: UserKey,
    ) -> serenity::Result<()> {
        let panel_lock = self.player_panel_lock(guild_key);
        let _panel_guard = panel_lock.lock().await;
        let existing_panels = self
            .player_panels
            .get(&guild_key)
            .map(|panels| panels.value().clone());
        let should_start_updates = existing_panels.is_none();
        let mut panels = existing_panels.unwrap_or_else(GuildPlayerPanels::new);
        let snapshot = self
            .control
            .now_playing_snapshot(guild_key)
            .await
            .filter(|snapshot| snapshot.request_id == request.request_id);
        let components = player_panel_components(
            &request.metadata,
            request.provider_id.as_ref(),
            snapshot.as_ref().and_then(|snapshot| snapshot.position),
            Some(requested_by),
        );
        let last_payload = serde_json::to_string(&components).unwrap_or_default();
        let message = command
            .create_followup(&ctx.http, player_panel_followup(components))
            .await?;
        register_player_panel(
            &mut panels,
            PlayerPanelState {
                channel_id: message.channel_id,
                message_id: message.id,
                request_id: request.request_id,
                playback_id: snapshot.as_ref().map(|snapshot| snapshot.playback_id),
                requester: requested_by,
                last_payload: Arc::new(tokio::sync::Mutex::new(last_payload)),
            },
        );
        let update_generation = panels.update_generation.clone();
        self.player_panels.insert(guild_key, panels);
        if should_start_updates {
            self.start_player_panel_updates(ctx.clone(), guild_key, update_generation);
        }
        Ok(())
    }

    fn start_player_panel_updates(
        &self,
        ctx: Context,
        guild_key: GuildKey,
        update_generation: Arc<()>,
    ) {
        let gateway = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval_at(
                tokio::time::Instant::now() + PLAYER_PANEL_UPDATE_INTERVAL,
                PLAYER_PANEL_UPDATE_INTERVAL,
            );
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut no_playback_since = None;
            let mut retry_backoff = Duration::from_secs(1);
            let mut first_refresh = true;
            loop {
                ticker.tick().await;
                if !gateway.has_player_panel_generation(guild_key, &update_generation) {
                    return;
                }
                let result = gateway.refresh_player_panel(&ctx, guild_key).await;
                if first_refresh {
                    append_debug_log(format!(
                        "discord: player panel updater first refresh guild_id={} result={result:?}",
                        guild_key.get()
                    ));
                    first_refresh = false;
                }
                match result {
                    PlayerPanelUpdateResult::Updated | PlayerPanelUpdateResult::Unchanged => {
                        no_playback_since = None;
                        retry_backoff = Duration::from_secs(1);
                    }
                    PlayerPanelUpdateResult::NoPlayback => {
                        let pending_track = gateway.control.has_current_track(guild_key)
                            || gateway
                                .control
                                .queue_preview(guild_key, 0)
                                .is_some_and(|queue| queue.total_queued() > 0);
                        if pending_track {
                            no_playback_since = None;
                            continue;
                        }
                        let since = no_playback_since.get_or_insert_with(Instant::now);
                        if player_panel_no_playback_grace_expired(*since, Instant::now()) {
                            gateway
                                .remove_player_panel_generation(guild_key, &update_generation)
                                .await;
                            return;
                        }
                    }
                    PlayerPanelUpdateResult::PermanentFailure(request_id) => {
                        if gateway
                            .remove_player_panel(guild_key, &update_generation, request_id)
                            .await
                        {
                            return;
                        }
                    }
                    PlayerPanelUpdateResult::RetryableFailure => {
                        tokio::time::sleep(retry_backoff).await;
                        retry_backoff =
                            retry_backoff.saturating_mul(2).min(Duration::from_secs(30));
                        ticker.reset();
                    }
                }
            }
        });
    }

    fn has_player_panel_generation(&self, guild_key: GuildKey, generation: &Arc<()>) -> bool {
        self.player_panels
            .get(&guild_key)
            .is_some_and(|panels| Arc::ptr_eq(&panels.update_generation, generation))
    }

    async fn remove_player_panel_generation(&self, guild_key: GuildKey, generation: &Arc<()>) {
        let panel_lock = self.player_panel_lock(guild_key);
        let _panel_guard = panel_lock.lock().await;
        if self.has_player_panel_generation(guild_key, generation) {
            self.player_panels.remove(&guild_key);
        }
    }

    async fn remove_player_panel(
        &self,
        guild_key: GuildKey,
        generation: &Arc<()>,
        request_id: TrackRequestId,
    ) -> bool {
        let panel_lock = self.player_panel_lock(guild_key);
        let _panel_guard = panel_lock.lock().await;
        let mut empty = false;
        if let Some(mut panels) = self.player_panels.get_mut(&guild_key)
            && Arc::ptr_eq(&panels.update_generation, generation)
        {
            panels.by_request_id.remove(&request_id);
            empty = panels.by_request_id.is_empty();
        }
        if empty {
            self.player_panels.remove(&guild_key);
        }
        empty
    }

    async fn refresh_player_panel(
        &self,
        ctx: &Context,
        guild_key: GuildKey,
    ) -> PlayerPanelUpdateResult {
        self.refresh_player_panel_to(ctx, guild_key).await
    }

    async fn refresh_player_panel_to(
        &self,
        ctx: &Context,
        guild_key: GuildKey,
    ) -> PlayerPanelUpdateResult {
        let panel_lock = self.player_panel_lock(guild_key);
        let _panel_guard = panel_lock.lock().await;
        let panels = self
            .player_panels
            .get(&guild_key)
            .map(|panel| panel.value().clone());
        let Some(snapshot) = self.control.now_playing_snapshot(guild_key).await else {
            self.player_panel_skip_pending.remove(&guild_key);
            return PlayerPanelUpdateResult::NoPlayback;
        };
        let skipped_playback_id = self
            .player_panel_skip_pending
            .get(&guild_key)
            .map(|pending| *pending.value());
        if is_skipped_playback_snapshot(skipped_playback_id, snapshot.playback_id) {
            return PlayerPanelUpdateResult::Unchanged;
        }
        if skipped_playback_id.is_some() {
            self.player_panel_skip_pending.remove(&guild_key);
        }
        let Some(panels) = panels else {
            return PlayerPanelUpdateResult::Unchanged;
        };

        // Panel registrations can arrive out of order relative to playback
        // transitions. Keep only the current and queued request identities;
        // never let whichever Discord response completed last decide which
        // song owns the update target.
        let mut live_request_ids = HashSet::from([snapshot.request_id]);
        if let Some(queue) = self.control.queue_preview(guild_key, usize::MAX) {
            if let Some(current) = queue.current() {
                live_request_ids.insert(current.request_id);
            }
            live_request_ids.extend(queue.upcoming().iter().map(|track| track.request_id));
        }
        if let Some(mut current) = self.player_panels.get_mut(&guild_key)
            && Arc::ptr_eq(&current.update_generation, &panels.update_generation)
        {
            retain_live_player_panels(&mut current, &live_request_ids);
        }

        let Some(panel) = player_panel_for_request(&panels, snapshot.request_id) else {
            return PlayerPanelUpdateResult::Unchanged;
        };
        if panel.request_id != snapshot.request_id {
            return PlayerPanelUpdateResult::Unchanged;
        }
        if is_stale_player_panel_snapshot(panel.playback_id, snapshot.playback_id) {
            return PlayerPanelUpdateResult::Unchanged;
        }

        // A skip or automatic transition may complete while the first
        // snapshot is being rendered. Recheck immediately before editing so
        // an outgoing card is not advanced using the next track's state.
        let Some(latest_snapshot) = self.control.now_playing_snapshot(guild_key).await else {
            return PlayerPanelUpdateResult::NoPlayback;
        };
        if latest_snapshot.playback_id != snapshot.playback_id
            || latest_snapshot.request_id != snapshot.request_id
        {
            return PlayerPanelUpdateResult::Unchanged;
        }
        let components = player_panel_components(
            &snapshot.metadata,
            snapshot.provider_id.as_ref(),
            snapshot.position,
            Some(panel.requester),
        );
        let result = self
            .edit_player_panel(ctx, guild_key, &panel, components)
            .await;
        if matches!(
            result,
            PlayerPanelUpdateResult::PermanentFailure(_)
                | PlayerPanelUpdateResult::RetryableFailure
        ) {
            return result;
        }

        if let Some(mut current) = self.player_panels.get_mut(&guild_key)
            && Arc::ptr_eq(&current.update_generation, &panels.update_generation)
            && let Some(current_panel) = current.by_request_id.get_mut(&snapshot.request_id)
        {
            current_panel.playback_id = Some(snapshot.playback_id);
        }
        result
    }

    async fn edit_player_panel(
        &self,
        ctx: &Context,
        guild_key: GuildKey,
        panel: &PlayerPanelState,
        components: Vec<CreateComponentV2>,
    ) -> PlayerPanelUpdateResult {
        let serialized = serde_json::to_string(&components).unwrap_or_default();
        let mut last_payload = panel.last_payload.lock().await;
        if *last_payload == serialized {
            return PlayerPanelUpdateResult::Unchanged;
        }
        match panel
            .channel_id
            .edit_message(
                &ctx.http,
                panel.message_id,
                EditMessage::new()
                    .allowed_mentions(CreateAllowedMentions::new())
                    .components_v2(components),
            )
            .await
        {
            Ok(_) => {
                *last_payload = serialized;
                PlayerPanelUpdateResult::Updated
            }
            Err(error) => {
                let permanent = matches!(
                    &error,
                    serenity::Error::Http(http_error)
                        if http_error.status_code().is_some_and(|status| {
                            (400..500).contains(&status.as_u16()) && status.as_u16() != 429
                        })
                );
                let safe_error = sanitize_log_message(&error.to_string());
                warn!(
                    guild_id = guild_key.get(),
                    message_id = panel.message_id.get(),
                    error = safe_error.as_str(),
                    "failed to refresh Components V2 player panel"
                );
                if permanent {
                    PlayerPanelUpdateResult::PermanentFailure(panel.request_id)
                } else {
                    PlayerPanelUpdateResult::RetryableFailure
                }
            }
        }
    }

    pub async fn notify_restart(&self, http: &Http) {
        let targets = self
            .notification_channels
            .iter()
            .filter(|entry| self.control.has_current_track(*entry.key()))
            .map(|entry| *entry.value())
            .take(MAX_RESTART_NOTIFICATIONS)
            .collect::<Vec<_>>();
        let notifications = targets.into_iter().map(|channel_id| async move {
            tokio::time::timeout(
                RESTART_NOTIFICATION_TIMEOUT,
                channel_id.send_message(http, CreateMessage::new().embed(update_restart_embed())),
            )
            .await
        });

        for result in join_all(notifications).await {
            match result {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    error!(error = %error, "failed to send update restart notification")
                }
                Err(_) => error!("restart notification timed out"),
            }
        }
    }

    pub async fn persist_restart_state(&self) {
        let mut voice_connections = self
            .active_voice_channels
            .iter()
            .map(|entry| (*entry.key(), *entry.value()))
            .collect::<Vec<_>>();
        voice_connections.sort_unstable_by_key(|(guild_id, _)| guild_id.get());
        voice_connections.truncate(MAX_RESTART_SNAPSHOTS);
        let connections = join_all(voice_connections.into_iter().map(|(guild_id, channel_id)| {
            let control = self.control.clone();
            async move {
                let playback = control.restart_snapshot(guild_id).await;
                (guild_id, channel_id, playback)
            }
        }))
        .await;
        if let Err(error) = self.reconnect_store.save(&connections) {
            error!(error = %error, "failed to persist voice reconnect handoff");
        } else {
            info!(
                connections = connections.len(),
                "persisted voice reconnect handoff"
            );
        }
    }

    async fn restore_voice_connections(&self, ctx: &Context) {
        let connections = match self.reconnect_store.take() {
            Ok(connections) => connections,
            Err(error) => {
                error!(error = %error, "failed to consume voice reconnect handoff");
                return;
            }
        };

        for (guild_id, channel_id, playback) in connections {
            match self.runtime.ensure_joined(guild_id, channel_id).await {
                Ok(_) => {
                    self.active_voice_channels.insert(guild_id, channel_id);
                    self.record_active_voice_counts();
                    let serenity_guild_id = serenity::all::GuildId::new(guild_id.get());
                    let bot_user_id = UserKey::new(ctx.cache.current_user().id.get());
                    self.control.bootstrap_voice_state(
                        guild_id,
                        channel_id,
                        self.guild_voice_snapshot(ctx, serenity_guild_id, bot_user_id),
                    );
                    if let Some(snapshot) = playback {
                        let mode_state = ModeNicknameState {
                            looping: snapshot.looping,
                            shuffle: snapshot.shuffle_enabled,
                            automix: snapshot.automix_enabled,
                        };
                        let restored = self
                            .control
                            .restore_restart_snapshot(guild_id, snapshot)
                            .await;
                        if !restored {
                            error!(
                                guild_id = guild_id.get(),
                                "failed to restore playback after restart"
                            );
                        } else {
                            self.mode_nickname_states.insert(guild_id, mode_state);
                            self.apply_mode_nickname(ctx, serenity_guild_id, mode_state)
                                .await;
                        }
                    } else {
                        self.mode_nickname_states
                            .insert(guild_id, ModeNicknameState::default());
                        self.apply_mode_nickname(
                            ctx,
                            serenity_guild_id,
                            ModeNicknameState::default(),
                        )
                        .await;
                    }
                    info!(
                        guild_id = guild_id.get(),
                        channel_id = channel_id.get(),
                        "restored voice connection after restart"
                    );
                }
                Err(error) => {
                    error!(
                        guild_id = guild_id.get(),
                        channel_id = channel_id.get(),
                        error = %error,
                        "failed to restore voice connection after restart"
                    );
                }
            }
        }
    }

    async fn handle_play(
        &self,
        ctx: &Context,
        command: &CommandInteraction,
    ) -> serenity::Result<()> {
        let interaction_started = Instant::now();
        let Some(guild_id) = command.guild_id else {
            return Ok(());
        };

        let source_url = command
            .data
            .options
            .iter()
            .find(|option| option.name == PLAY_COMMAND_URL_OPTION)
            .and_then(|option| option.value.as_str())
            .map(str::trim)
            .unwrap_or_default()
            .to_owned();
        let source_url_log = summarize_url_for_logs(&source_url);
        append_debug_log(format!(
            "discord: /play received guild_id={} user_id={} url={source_url_log}",
            guild_id.get(),
            command.user.id.get()
        ));
        info!(
            guild_id = guild_id.get(),
            user_id = command.user.id.get(),
            source = source_url_log.as_str(),
            "received play command"
        );

        if !wotoha_core::url::is_allowed_track_url(&source_url) {
            append_debug_log("discord: /play rejected by allowlist");
            return command
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .ephemeral(true)
                            .embed(error_embed(MSG_ALLOWED_URL_ONLY)),
                    ),
                )
                .await;
        }

        let Some(user_channel) = self.user_voice_channel(ctx, guild_id, command.user.id) else {
            append_debug_log("discord: /play rejected because user not in voice");
            return command
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .ephemeral(true)
                            .embed(error_embed(MSG_JOIN_VOICE_FIRST)),
                    ),
                )
                .await;
        };

        let guild_key = GuildKey::new(guild_id.get());
        let user_channel_key = ChannelKey::new(user_channel.get());
        match self
            .control
            .voice_action_access(guild_key, Some(user_channel_key))
        {
            VoiceActionAccess::NoActiveChannel | VoiceActionAccess::SameChannel { .. } => {}
            VoiceActionAccess::DifferentChannel { .. } => {
                append_debug_log(
                    "discord: /play rejected because bot is active in another voice channel",
                );
                command
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .ephemeral(true)
                                .embed(error_embed(MSG_PLAYING_IN_ANOTHER_VOICE)),
                        ),
                    )
                    .await?;
                return Ok(());
            }
            VoiceActionAccess::UserNotInVoice => {
                command
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .ephemeral(true)
                                .embed(error_embed(MSG_JOIN_VOICE_FIRST)),
                        ),
                    )
                    .await?;
                return Ok(());
            }
        }

        command
            .create_response(&ctx.http, deferred_play_response())
            .await?;
        operational_metrics().observe_interaction_ack(interaction_started.elapsed());
        let joined_now = match self
            .runtime
            .ensure_joined(guild_key, user_channel_key)
            .await
        {
            Ok(joined) => joined,
            Err(error) => {
                let safe_error = sanitize_log_message(&error.to_string());
                append_debug_log(format!("discord: ensure_joined failed: {safe_error}"));
                command
                    .create_followup(
                        &ctx.http,
                        CreateInteractionResponseFollowup::new()
                            .flags(MessageFlags::EPHEMERAL)
                            .embed(error_embed(
                                "接続に失敗しました。しばらくしてから再試行してください。",
                            )),
                    )
                    .await?;
                return Ok(());
            }
        };
        self.active_voice_channels
            .insert(guild_key, user_channel_key);

        let bot_user_id = UserKey::new(ctx.cache.current_user().id.get());
        self.control.bootstrap_voice_state(
            guild_key,
            user_channel_key,
            self.guild_voice_snapshot(ctx, guild_id, bot_user_id),
        );

        match self.control.play(guild_key, &source_url).await {
            Ok(outcome) => {
                self.notification_channels
                    .insert(guild_key, command.channel_id);
                let safe_canonical_key =
                    sanitize_log_message(outcome.request.canonical_key.as_ref());
                let safe_title = sanitize_log_message(outcome.request.metadata.title.as_ref());
                append_debug_log(format!(
                    "discord: control.play ok provider={} key={} title={} now_playing={}",
                    outcome.request.provider_id.as_ref(),
                    safe_canonical_key,
                    safe_title,
                    outcome.now_playing
                ));
                info!(
                    guild_id = guild_id.get(),
                    provider_id = outcome.request.provider_id.as_ref(),
                    canonical_key = safe_canonical_key.as_str(),
                    title = safe_title.as_str(),
                    now_playing = outcome.now_playing,
                    "play command resolved successfully"
                );
                self.refresh_mode_nickname(ctx, guild_id, guild_key, None, None, None)
                    .await;
                let requested_by = UserKey::new(command.user.id.get());
                if outcome.now_playing {
                    self.publish_player_panel(
                        ctx,
                        command,
                        guild_key,
                        &outcome.request,
                        requested_by,
                    )
                    .await?;
                } else {
                    self.publish_queued_track_response(
                        ctx,
                        command,
                        guild_key,
                        &outcome.request,
                        requested_by,
                    )
                    .await?;
                }
            }
            Err(error) => {
                let safe_error = sanitize_log_message(&error.to_string());
                append_debug_log(format!("discord: control.play failed: {safe_error}"));
                error!(
                    guild_id = guild_id.get(),
                    user_id = command.user.id.get(),
                    source = source_url_log.as_str(),
                    error = safe_error.as_str(),
                    "play command failed"
                );
                if joined_now && !self.control.has_current_track(guild_key) {
                    self.active_voice_channels.remove(&guild_key);
                    self.record_active_voice_counts();
                    self.reset_disconnected_guild(ctx, guild_id, guild_key)
                        .await;
                }
                command
                    .create_followup(
                        &ctx.http,
                        CreateInteractionResponseFollowup::new()
                            .flags(MessageFlags::EPHEMERAL)
                            .embed(error_embed(playback_failure_message(
                                self.control.playback_failure_kind(&error),
                            ))),
                    )
                    .await?;
            }
        }

        Ok(())
    }

    async fn handle_component(
        &self,
        ctx: &Context,
        component: &ComponentInteraction,
    ) -> serenity::Result<()> {
        let interaction_started = Instant::now();
        let Some(guild_id) = component.guild_id else {
            return Ok(());
        };
        let guild_key = GuildKey::new(guild_id.get());

        let Some(action) = component_action(&component.data.custom_id) else {
            return Ok(());
        };
        let actor_channel = self
            .user_voice_channel(ctx, guild_id, component.user.id)
            .map(|channel_id| ChannelKey::new(channel_id.get()));

        // A component interaction has a short acknowledgement deadline.  The
        // control operation may wait on per-guild playback state, so acknowledge
        // before taking that lock or performing any follow-up HTTP work.
        component.defer(&ctx.http).await?;
        operational_metrics().observe_interaction_ack(interaction_started.elapsed());

        let skip_action = matches!(action, ComponentAction::Skip);
        let panel_lock = skip_action.then(|| self.player_panel_lock(guild_key));
        let _panel_guard = if let Some(panel_lock) = panel_lock.as_ref() {
            Some(panel_lock.lock().await)
        } else {
            None
        };
        let skipped_playback_id = if skip_action {
            self.control
                .now_playing_snapshot(guild_key)
                .await
                .map(|snapshot| snapshot.playback_id)
        } else {
            None
        };
        let outcome = self
            .control
            .handle_component(guild_key, actor_channel, action)
            .await;
        if matches!(&outcome, ComponentOutcome::Skip { .. })
            && let Some(playback_id) = skipped_playback_id
        {
            self.player_panel_skip_pending
                .insert(guild_key, playback_id);
        }
        drop(_panel_guard);

        let mode_update = match &outcome {
            ComponentOutcome::Skip { .. } => Some((None, Some(false), None)),
            ComponentOutcome::Loop { enabled } => Some((None, Some(*enabled), None)),
            ComponentOutcome::Shuffle { enabled } => Some((Some(*enabled), None, None)),
            ComponentOutcome::AutoMix { enabled } => Some((None, None, Some(*enabled))),
            _ => None,
        };
        if let Some((shuffle_override, looping_override, automix_override)) = mode_update {
            self.refresh_mode_nickname(
                ctx,
                guild_id,
                guild_key,
                shuffle_override,
                looping_override,
                automix_override,
            )
            .await;
        }

        if matches!(
            &outcome,
            ComponentOutcome::Loop { .. }
                | ComponentOutcome::Shuffle { .. }
                | ComponentOutcome::AutoMix { .. }
        ) {
            let result = self.refresh_player_panel(ctx, guild_key).await;
            append_debug_log(format!(
                "discord: player panel interaction refresh guild_id={} result={result:?}",
                guild_key.get()
            ));
        }

        match outcome {
            ComponentOutcome::Skip { .. } => {}
            ComponentOutcome::Loop { .. } => {}
            ComponentOutcome::Shuffle { .. } => {}
            ComponentOutcome::AutoMix { .. } => {}
            ComponentOutcome::QueuePreview(preview) => {
                component
                    .create_followup(
                        &ctx.http,
                        CreateInteractionResponseFollowup::new()
                            .flags(MessageFlags::EPHEMERAL)
                            .embed(queue_embed(&preview)),
                    )
                    .await?;
            }
            ComponentOutcome::QueueEmpty => {
                component
                    .create_followup(
                        &ctx.http,
                        CreateInteractionResponseFollowup::new()
                            .flags(MessageFlags::EPHEMERAL)
                            .embed(error_embed(MSG_QUEUE_EMPTY)),
                    )
                    .await?;
            }
            ComponentOutcome::NoTrackPlaying => {
                component
                    .create_followup(
                        &ctx.http,
                        CreateInteractionResponseFollowup::new()
                            .flags(MessageFlags::EPHEMERAL)
                            .embed(error_embed(MSG_NO_TRACK_PLAYING)),
                    )
                    .await?;
            }
            ComponentOutcome::VoiceChannelRequired => {
                component
                    .create_followup(
                        &ctx.http,
                        CreateInteractionResponseFollowup::new()
                            .flags(MessageFlags::EPHEMERAL)
                            .embed(error_embed(MSG_JOIN_ACTIVE_VOICE)),
                    )
                    .await?;
            }
        }

        Ok(())
    }

    fn user_voice_channel(
        &self,
        ctx: &Context,
        guild_id: serenity::all::GuildId,
        user_id: serenity::all::UserId,
    ) -> Option<ChannelId> {
        let guild = ctx.cache.guild(guild_id)?;
        guild.voice_states.get(&user_id)?.channel_id
    }

    fn guild_voice_snapshot(
        &self,
        ctx: &Context,
        guild_id: serenity::all::GuildId,
        bot_user_id: UserKey,
    ) -> Vec<VoicePeerSnapshot> {
        let Some(guild) = ctx.cache.guild(guild_id) else {
            return Vec::new();
        };

        guild
            .voice_states
            .iter()
            .filter_map(|(user_id, state)| {
                let user_key = UserKey::new(user_id.get());
                if user_key == bot_user_id {
                    return None;
                }

                state.channel_id.map(|channel_id| VoicePeerSnapshot {
                    user_id: user_key,
                    channel_id: ChannelKey::new(channel_id.get()),
                })
            })
            .collect()
    }
}

fn render_mode_nickname(base: &str, state: ModeNicknameState) -> String {
    let clean_base = base
        .replace(MODE_NICKNAME_LOOP, "")
        .replace(MODE_NICKNAME_SHUFFLE, "")
        .replace(MODE_NICKNAME_AUTOMIX, "")
        .trim()
        .to_owned();
    let mut indicators = Vec::with_capacity(3);
    if state.looping {
        indicators.push(MODE_NICKNAME_LOOP);
    }
    if state.shuffle && !state.looping {
        indicators.push(MODE_NICKNAME_SHUFFLE);
    }
    if state.automix {
        indicators.push(MODE_NICKNAME_AUTOMIX);
    }
    if indicators.is_empty() {
        return clean_base
            .chars()
            .take(DISCORD_NICKNAME_MAX_CHARS)
            .collect();
    }

    let suffix = indicators.join("");
    let base_limit = DISCORD_NICKNAME_MAX_CHARS.saturating_sub(suffix.chars().count());
    let truncated_base = clean_base.chars().take(base_limit).collect::<String>();
    format!("{truncated_base}{suffix}")
}

impl<P> DiscordGateway<P, SongbirdRuntime>
where
    P: PlaybackService,
{
    pub async fn build_songbird_client(
        discord_token: impl Into<String>,
        control: ControlService<P>,
        runtime: SongbirdRuntime,
        songbird: Arc<Songbird>,
    ) -> Result<Client, Box<dyn std::error::Error + Send + Sync>> {
        let handler = Self::new(control, runtime);
        let intents = serenity::all::GatewayIntents::GUILDS
            | serenity::all::GatewayIntents::GUILD_VOICE_STATES;

        let client = Client::builder(discord_token.into(), intents)
            .cache_settings(recommended_cache_settings())
            .event_handler(handler)
            .register_songbird_with(songbird)
            .await?;

        Ok(client)
    }
}

#[async_trait]
impl<P, R> EventHandler for DiscordGateway<P, R>
where
    P: PlaybackService,
    R: VoiceGatewayRuntime,
{
    async fn ready(&self, ctx: Context, ready: Ready) {
        append_debug_log("discord: ready event received");
        operational_metrics().record_gateway_ready();
        operational_metrics().set_active_guilds(self.active_voice_channels.len());
        operational_metrics().set_active_voice_sessions(self.active_voice_channels.len());
        update_version_activity(&ctx);
        if self.startup.boot_tasks_done.swap(true, Ordering::AcqRel) {
            info!("Gateway ready received again; skipping boot-only tasks");
            return;
        }

        info!(guild_count = ready.guilds.len(), "Bot is ready");

        self.restore_voice_connections(&ctx).await;

        if let Err(error) = ensure_global_commands(&ctx).await {
            error!(error = %error, "failed to synchronize slash commands");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        append_debug_log(format!(
            "discord: interaction_create kind={:?}",
            interaction.kind()
        ));
        match interaction {
            Interaction::Command(command) if command.data.name == PLAY_COMMAND_NAME => {
                if let Err(error) = self.handle_play(&ctx, &command).await {
                    error!(error = %error, "play command failed");
                }
            }
            Interaction::Component(component) => {
                if let Err(error) = self.handle_component(&ctx, &component).await {
                    error!(error = %error, "component interaction failed");
                }
            }
            _ => {}
        }
    }

    async fn guild_create(&self, ctx: Context, _guild: Guild, _is_new: Option<bool>) {
        update_version_activity(&ctx);
    }

    async fn guild_delete(
        &self,
        ctx: Context,
        _incomplete: UnavailableGuild,
        _full: Option<Guild>,
    ) {
        update_version_activity(&ctx);
    }

    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        let Some(guild_id) = new.guild_id else {
            return;
        };
        let guild_key = GuildKey::new(guild_id.get());
        let user_id = UserKey::new(new.user_id.get());
        let new_channel = new
            .channel_id
            .map(|channel_id| ChannelKey::new(channel_id.get()));
        let bot_user_id = UserKey::new(ctx.cache.current_user().id.get());
        if user_id == bot_user_id {
            if let Err(error) = self
                .runtime
                .handle_gateway_event(VoiceGatewayEvent::StateUpdate(VoiceGatewayStateUpdate {
                    guild_id: guild_key,
                    user_id,
                    channel_id: new_channel,
                    session_id: new.session_id.clone(),
                }))
                .await
            {
                error!(
                    guild_id = guild_key.get(),
                    user_id = user_id.get(),
                    error = %error,
                    "failed to forward voice state update to runtime"
                );
            }
            self.control
                .update_bot_voice_channel(guild_key, new_channel);
            match new_channel {
                Some(channel_id) => {
                    self.active_voice_channels.insert(guild_key, channel_id);
                    self.record_active_voice_counts();
                }
                None => {
                    self.active_voice_channels.remove(&guild_key);
                    self.record_active_voice_counts();
                    self.notification_channels.remove(&guild_key);
                    self.reset_disconnected_guild(&ctx, guild_id, guild_key)
                        .await;
                }
            }
            return;
        }

        let decision = self.control.apply_peer_voice_state(
            guild_key,
            user_id,
            old.and_then(|state| state.channel_id)
                .map(|channel_id| ChannelKey::new(channel_id.get())),
            new_channel,
        );
        if decision != VoiceUpdateDecision::DisconnectAlone {
            return;
        }

        self.active_voice_channels.remove(&guild_key);
        self.record_active_voice_counts();
        self.notification_channels.remove(&guild_key);
        self.reset_disconnected_guild(&ctx, guild_id, guild_key)
            .await;
    }

    async fn voice_server_update(&self, _ctx: Context, event: VoiceServerUpdateEvent) {
        let Some(guild_id) = event.guild_id else {
            return;
        };
        let guild_key = GuildKey::new(guild_id.get());
        if let Err(error) = self
            .runtime
            .handle_gateway_event(VoiceGatewayEvent::ServerUpdate(VoiceGatewayServerUpdate {
                guild_id: guild_key,
                endpoint: event.endpoint,
                token: event.token,
            }))
            .await
        {
            error!(
                guild_id = guild_key.get(),
                error = %error,
                "failed to forward voice server update to runtime"
            );
        }
    }

    async fn cache_ready(&self, ctx: Context, guilds: Vec<serenity::all::GuildId>) {
        update_version_activity(&ctx);
        append_debug_log(format!("discord: cache_ready guild_count={}", guilds.len()));
        info!(guild_count = guilds.len(), "Cache is ready");
    }
}

async fn ensure_global_commands(ctx: &Context) -> serenity::Result<()> {
    let current = Command::get_global_commands(&ctx.http).await?;
    if let Some(command) = current
        .iter()
        .find(|command| command.name == PLAY_COMMAND_NAME)
    {
        if matches_play_command(command) {
            return Ok(());
        }

        let _ = Command::edit_global_command(&ctx.http, command.id, play_command()).await?;
    } else {
        let _ = Command::create_global_command(&ctx.http, play_command()).await?;
    }
    Ok(())
}

fn matches_play_command(command: &Command) -> bool {
    command.name == PLAY_COMMAND_NAME
        && command.description == PLAY_COMMAND_DESCRIPTION
        && command.kind == serenity::all::CommandType::ChatInput
        && !command.nsfw
        && command.options.len() == 1
        && matches_play_option(&command.options[0])
}

fn matches_play_option(option: &serenity::all::CommandOption) -> bool {
    option.name == PLAY_COMMAND_URL_OPTION
        && option.description == "URL"
        && option.kind == CommandOptionType::String
        && option.required
        && option.choices.is_empty()
        && option.options.is_empty()
        && option.channel_types.is_empty()
        && option.min_value.is_none()
        && option.max_value.is_none()
        && option.min_length == Some(1)
        && option.max_length == Some(PLAY_COMMAND_URL_MAX_LENGTH)
        && !option.autocomplete
}

fn play_command() -> CreateCommand {
    CreateCommand::new(PLAY_COMMAND_NAME)
        .description(PLAY_COMMAND_DESCRIPTION)
        .add_option(
            CreateCommandOption::new(CommandOptionType::String, PLAY_COMMAND_URL_OPTION, "URL")
                .required(true)
                .min_length(1)
                .max_length(PLAY_COMMAND_URL_MAX_LENGTH),
        )
}

fn error_embed(message: &str) -> CreateEmbed {
    CreateEmbed::new()
        .description(message)
        .color(Colour::new(COLOR_ERROR))
}

fn component_action(custom_id: &str) -> Option<ComponentAction> {
    match custom_id {
        BUTTON_SKIP => Some(ComponentAction::Skip),
        BUTTON_LOOP => Some(ComponentAction::Loop),
        BUTTON_SHUFFLE => Some(ComponentAction::Shuffle),
        BUTTON_AUTOMIX => Some(ComponentAction::AutoMix),
        BUTTON_QUEUE => Some(ComponentAction::Queue {
            limit: QUEUE_PREVIEW_LIMIT,
        }),
        _ => None,
    }
}

fn deferred_play_response() -> CreateInteractionResponse {
    CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new())
}

fn player_panel_followup(components: Vec<CreateComponentV2>) -> CreateInteractionResponseFollowup {
    CreateInteractionResponseFollowup::new()
        .flags(MessageFlags::IS_COMPONENTS_V2)
        .allowed_mentions(CreateAllowedMentions::new())
        .components_v2(components)
}

fn update_restart_embed() -> CreateEmbed {
    CreateEmbed::new()
        .title(UPDATE_NOTICE_TITLE)
        .description(UPDATE_NOTICE_DESCRIPTION)
        .color(Colour::new(COLOR_INFO))
}

fn playback_failure_message(kind: Option<PlaybackFailureKind>) -> &'static str {
    match kind {
        Some(PlaybackFailureKind::AgeVerificationRequired) => {
            "この動画はYouTubeで年齢確認が必要なため、現在の設定では再生できません。"
        }
        Some(PlaybackFailureKind::AuthenticationRequired) => {
            "この動画は認証が必要なため、現在の設定では再生できません。"
        }
        Some(PlaybackFailureKind::AccessRestricted) => "この動画は現在の環境から再生できません。",
        Some(PlaybackFailureKind::TemporarySource) | None => {
            "音声の取得に失敗しました。しばらくしてから再試行してください。"
        }
    }
}

fn player_panel_components(
    metadata: &TrackMetadata,
    provider_id: &str,
    position: Option<Duration>,
    requested_by: Option<UserKey>,
) -> Vec<CreateComponentV2> {
    let title = escape_markdown(&single_line(&metadata.title, 256));
    let artist = escape_markdown(&single_line(&metadata.author, 256));
    let provider = provider_display_line(provider_id, metadata.uri.as_ref());
    let title_text = match public_display_url(metadata.uri.as_ref()) {
        Some(uri) => format!("**[{title}]({uri})**"),
        None => format!("**{title}**"),
    };
    let track_text = vec![
        CreateComponentV2::text_display(provider.clone()),
        CreateComponentV2::text_display(title_text),
        CreateComponentV2::text_display(artist),
    ];
    let track_section = if let Some(thumbnail_url) = metadata
        .thumbnail_url
        .as_deref()
        .and_then(safe_thumbnail_url)
    {
        CreateComponentV2::section(
            track_text,
            CreateSectionAccessory::thumbnail(thumbnail_url, Some(title)),
        )
    } else {
        CreateComponentV2::text_display(format!(
            "{provider}\n{}\n{}",
            match public_display_url(metadata.uri.as_ref()) {
                Some(uri) => format!("**[{title}]({uri})**"),
                None => format!("**{title}**"),
            },
            escape_markdown(&single_line(&metadata.author, 256)),
        ))
    };

    let components = vec![
        track_section,
        CreateComponentV2::text_display(format_player_progress(position, metadata.duration)),
        CreateComponentV2::text_display(format!(
            "Requested by {}",
            requested_by.map_or_else(|| "不明".to_owned(), |user| format!("<@{}>", user.get())),
        )),
        CreateComponentV2::separator(true, 1),
        player_action_row(),
    ];

    vec![CreateComponentV2::container_with_accent(
        components, COLOR_INFO,
    )]
}

#[cfg(test)]
fn queued_track_panel_components(
    request: &TrackRequest,
    requested_by: UserKey,
) -> Vec<CreateComponentV2> {
    player_panel_components(
        &request.metadata,
        request.provider_id.as_ref(),
        None,
        Some(requested_by),
    )
}

fn player_action_row() -> CreateComponentV2 {
    CreateComponentV2::action_row(CreateActionRow::Buttons(vec![
        player_icon_button(
            BUTTON_SKIP,
            ButtonStyle::Secondary,
            SKIP_EMOJI_NAME,
            SKIP_EMOJI_ID,
        ),
        player_icon_button(
            BUTTON_LOOP,
            ButtonStyle::Secondary,
            LOOP_EMOJI_NAME,
            LOOP_EMOJI_ID,
        ),
        player_icon_button(
            BUTTON_SHUFFLE,
            ButtonStyle::Secondary,
            SHUFFLE_EMOJI_NAME,
            SHUFFLE_EMOJI_ID,
        ),
        player_icon_button(
            BUTTON_AUTOMIX,
            ButtonStyle::Secondary,
            AUTOMIX_EMOJI_NAME,
            AUTOMIX_EMOJI_ID,
        )
        .label("AutoMix"),
        player_link_button(WOTOHA_INVITE_URL, INVITE_BUTTON_LABEL),
    ]))
}

fn provider_display_name(provider_id: &str, track_uri: &str) -> &'static str {
    match provider_id.to_ascii_lowercase().as_str() {
        "bandcamp" => "Bandcamp",
        "niconico" => "Niconico",
        "youtube" => "YouTube",
        "soundcloud" => "SoundCloud",
        "twitch" => "Twitch",
        "vimeo" => "Vimeo",
        "x" => "X",
        _ => {
            let host = url::Url::parse(track_uri)
                .ok()
                .and_then(|url| url.host_str().map(str::to_ascii_lowercase));
            match host.as_deref() {
                Some("youtube.com" | "www.youtube.com" | "m.youtube.com" | "youtu.be") => "YouTube",
                Some("soundcloud.com" | "www.soundcloud.com") => "SoundCloud",
                Some("open.spotify.com") => "Spotify",
                Some("nicovideo.jp" | "www.nicovideo.jp") => "Niconico",
                Some("x.com" | "www.x.com") => "X",
                Some("twitch.tv" | "www.twitch.tv" | "clips.twitch.tv") => "Twitch",
                Some("vimeo.com" | "www.vimeo.com" | "player.vimeo.com") => "Vimeo",
                Some("bandcamp.com") => "Bandcamp",
                Some(host) if host.ends_with(".bandcamp.com") => "Bandcamp",
                _ => "Audio source",
            }
        }
    }
}

fn provider_display_line(provider_id: &str, track_uri: &str) -> String {
    let provider = provider_display_name(provider_id, track_uri);
    let emoji = match provider {
        "Bandcamp" => Some(("bandcamp", "1563933089778892817")),
        "Niconico" => Some(("niconico", "1563933110478053548")),
        "SoundCloud" => Some(("soundcloud", "1563933135475843182")),
        "Twitch" => Some(("twitch", "1563933156657070191")),
        "Vimeo" => Some(("vimeo", "1563933178136109156")),
        "X" => Some(("x", "1563933199787360496")),
        "YouTube" => Some(("youtube", "1563933220041523251")),
        _ => None,
    };

    emoji.map_or_else(
        || format!("**{provider}**"),
        |(name, id)| format!("<:{name}:{id}> **{provider}**"),
    )
}

const PLAYER_PROGRESS_BAR_WIDTH: usize = 19;

fn format_player_progress(position: Option<Duration>, duration: Option<Duration>) -> String {
    let Some(duration) = duration else {
        return position.map_or_else(
            || "再生時間不明".to_owned(),
            |position| format!("`{}` · 時間不明", format_player_clock(position)),
        );
    };
    if duration.is_zero() {
        return position.map_or_else(
            || "再生時間不明".to_owned(),
            |position| format!("`{}` · 時間不明", format_player_clock(position)),
        );
    }
    let position = position.map(|position| position.min(duration));
    let bar = match position {
        Some(position) => {
            let ratio = (position.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0);
            let cursor = (ratio * (PLAYER_PROGRESS_BAR_WIDTH - 1) as f64).round() as usize;
            (0..PLAYER_PROGRESS_BAR_WIDTH)
                .map(|index| {
                    if index == cursor {
                        '●'
                    } else if index < cursor {
                        '━'
                    } else {
                        '─'
                    }
                })
                .collect::<String>()
        }
        None => "─".repeat(PLAYER_PROGRESS_BAR_WIDTH),
    };
    let elapsed = position.map_or_else(|| "--:--".to_owned(), format_player_clock);
    format!("`{elapsed}` {bar} `{}`", format_player_clock(duration))
}

fn format_player_clock(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let minutes = seconds / 60;
    let remainder = seconds % 60;
    if minutes >= 60 {
        format!("{}:{:02}:{remainder:02}", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}:{remainder:02}")
    }
}

fn single_line(text: &str, max_len: usize) -> String {
    truncate_embed_text(
        &sanitize_display_text(text).replace(['\n', '\r'], " "),
        max_len,
    )
}

fn escape_markdown(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(
            character,
            '\\' | '*' | '_' | '~' | '`' | '|' | '>' | '[' | ']'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn queue_embed(preview: &QueuePreview) -> CreateEmbed {
    let mut embed = CreateEmbed::new()
        .title("📜 Playlist")
        .color(Colour::new(COLOR_INFO));

    if let Some(current) = preview.current() {
        embed = embed.field(
            "Now playing",
            truncate_embed_text(current.metadata.title.as_ref(), EMBED_FIELD_LIMIT),
            false,
        );
    }

    if !preview.upcoming().is_empty() {
        embed = embed.field("Next Up", queue_lines(preview), false);
    }

    embed
}

fn queue_lines(preview: &QueuePreview) -> String {
    let mut lines = String::new();
    let overflow_suffix = if preview.total_queued() > preview.upcoming().len() {
        format!(
            "\n...and {} more songs",
            preview.total_queued() - preview.upcoming().len()
        )
    } else {
        String::new()
    };
    let budget = EMBED_FIELD_LIMIT.saturating_sub(overflow_suffix.len());

    for (index, track) in preview.upcoming().iter().enumerate() {
        let line = format!(
            "{}. {}\n",
            index + 1,
            truncate_embed_text(track.metadata.title.as_ref(), EMBED_TITLE_LIMIT)
        );
        if lines.len().saturating_add(line.len()) > budget {
            break;
        }
        lines.push_str(&line);
    }

    if !overflow_suffix.is_empty() {
        lines.push_str(&overflow_suffix);
    }

    lines
}

fn truncate_embed_text(text: &str, max_len: usize) -> String {
    let text = sanitize_display_text(text);
    if text.chars().count() <= max_len {
        return text;
    }

    let keep = max_len.saturating_sub(3);
    let mut out = String::with_capacity(max_len);
    for (index, ch) in text.chars().enumerate() {
        if index >= keep {
            break;
        }
        out.push(ch);
    }
    out.push_str("...");
    out
}

fn sanitize_display_text(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_control() && ch != '\n' && ch != '\t' {
                ' '
            } else {
                ch
            }
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn public_display_url(raw_url: &str) -> Option<String> {
    if !wotoha_core::url::is_allowed_track_url(raw_url) {
        return None;
    }
    let mut url = url::Url::parse(raw_url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let is_youtube = matches!(
        host.as_str(),
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtu.be"
    );
    let query = url
        .query_pairs()
        .filter(|(key, _)| {
            (is_youtube && key == "v") || (!is_youtube && !is_tracking_query_key(key))
        })
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    url.set_query(None);
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (key, value) in query {
            pairs.append_pair(&key, &value);
        }
    }
    url.set_fragment(None);
    Some(url.to_string())
}

fn safe_thumbnail_url(raw_url: &str) -> Option<String> {
    if !wotoha_core::url::is_safe_https_url(raw_url) {
        return None;
    }
    let mut url = url::Url::parse(raw_url).ok()?;
    url.set_fragment(None);
    Some(url.to_string())
}

fn is_tracking_query_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "fbclid" || key == "gclid" || key == "si" || key == "feature" || key.starts_with("utm_")
}

fn player_link_button(url: &str, label: &str) -> CreateButton {
    CreateButton::new_link(url).label(label)
}

fn player_icon_button(
    custom_id: &str,
    style: ButtonStyle,
    emoji_name: &str,
    emoji_id: u64,
) -> CreateButton {
    player_button_base(custom_id, style, emoji_name, emoji_id)
}

fn player_button_base(
    custom_id: &str,
    style: ButtonStyle,
    emoji_name: &str,
    emoji_id: u64,
) -> CreateButton {
    CreateButton::new(custom_id)
        .style(style)
        .emoji(ReactionType::Custom {
            animated: false,
            id: EmojiId::new(emoji_id),
            name: Some(emoji_name.to_owned()),
        })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Arc, time::Instant};

    use super::{
        BOT_BASE_NICKNAME, BUILD_VERSION, DISCORD_NICKNAME_MAX_CHARS, EMBED_FIELD_LIMIT,
        GuildPlayerPanels, INVITE_BUTTON_LABEL, ModeNicknameState, PLAYER_PROGRESS_BAR_WIDTH,
        PlayerPanelState, component_action, deferred_play_response, escape_markdown,
        format_player_progress, is_skipped_playback_snapshot, is_stale_player_panel_snapshot,
        matches_play_option, play_command, player_action_row, player_panel_components,
        player_panel_followup, player_panel_for_request, player_panel_no_playback_grace_expired,
        provider_display_line, public_display_url, queue_lines, queued_track_panel_components,
        register_player_panel, render_mode_nickname, retain_live_player_panels, safe_thumbnail_url,
        truncate_embed_text, version_activity,
    };
    use serenity::all::{
        ActivityType, ChannelId, CreateAllowedMentions, EditMessage, MessageFlags, MessageId,
    };
    use wotoha_contracts::{PlaybackId, UserKey};
    use wotoha_control::ComponentAction;
    use wotoha_core::ui::{
        AUTOMIX_EMOJI_ID, AUTOMIX_EMOJI_NAME, BUTTON_AUTOMIX, BUTTON_LOOP, BUTTON_QUEUE,
        BUTTON_SHUFFLE, BUTTON_SKIP, LOOP_EMOJI_ID, LOOP_EMOJI_NAME, SHUFFLE_EMOJI_ID,
        SHUFFLE_EMOJI_NAME, SKIP_EMOJI_ID, SKIP_EMOJI_NAME,
    };
    use wotoha_core::{GuildPlayerState, PreparedSource, TrackMetadata, TrackRequest};

    #[test]
    fn truncate_embed_text_enforces_limit() {
        let input = "a".repeat(300);
        let output = truncate_embed_text(&input, 32);
        assert!(output.len() <= 32);
        assert!(output.ends_with("..."));
    }

    #[test]
    fn queue_lines_stay_inside_embed_budget() {
        let mut state = GuildPlayerState::default();
        for index in 0..20 {
            state.enqueue(track(format!("track {}", "x".repeat(180)), index));
        }

        let preview = state.queue_preview(20);
        let lines = queue_lines(&preview);
        assert!(lines.len() <= EMBED_FIELD_LIMIT);
    }

    #[test]
    fn version_activity_contains_build_version() {
        let version = BUILD_VERSION.strip_prefix('v').unwrap_or(BUILD_VERSION);
        let activity = version_activity(42);
        assert_eq!(
            activity.state.as_deref(),
            Some(format!("v{version} | in 42 servers").as_str())
        );
        assert_eq!(activity.kind, ActivityType::Custom);
    }

    #[test]
    fn public_youtube_url_preserves_video_identity_and_removes_tracking() {
        assert_eq!(
            public_display_url("https://www.youtube.com/watch?v=abc&utm_source=test&feature=share"),
            Some("https://www.youtube.com/watch?v=abc".to_owned())
        );
    }

    #[test]
    fn public_and_thumbnail_url_policies_are_distinct() {
        assert_eq!(
            public_display_url("https://user:pass@www.youtube.com/watch?v=abc"),
            None
        );
        assert_eq!(
            safe_thumbnail_url("https://cdn.example.test/thumb.jpg?signature=keep"),
            Some("https://cdn.example.test/thumb.jpg?signature=keep".to_owned())
        );
        assert_eq!(
            public_display_url("https://rr1---sn.example.googlevideo.com/videoplayback?id=abc"),
            None
        );
    }

    #[test]
    fn player_message_uses_components_v2_without_embeds_or_legacy_content() {
        let metadata = TrackMetadata::new(
            "title",
            "artist",
            "https://www.youtube.com/watch?v=abc",
            Some("https://cdn.example.test/thumb.jpg".into()),
            Some(std::time::Duration::from_secs(198)),
        );
        let components = player_panel_components(
            &metadata,
            "youtube",
            Some(std::time::Duration::from_secs(55)),
            Some(UserKey::new(123_456_789_012_345_678)),
        );
        let payload = serde_json::to_value(player_panel_followup(components))
            .expect("Components V2 followup serializes");

        assert_eq!(payload["flags"], MessageFlags::IS_COMPONENTS_V2.bits());
        assert_eq!(
            payload["flags"].as_u64().unwrap() & MessageFlags::EPHEMERAL.bits(),
            0
        );
        assert!(payload.get("content").is_none());
        assert!(payload.get("embeds").is_none());
        assert!(
            payload["allowed_mentions"]["parse"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );
        assert!(
            payload["allowed_mentions"]["users"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );
        assert!(
            payload["allowed_mentions"]["roles"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );
        let container = &payload["components"][0];
        assert_eq!(container["type"], 17);
        let track_section = &container["components"][0];
        assert_eq!(track_section["type"], 9);
        assert_eq!(
            track_section["components"].as_array().map(Vec::len),
            Some(3)
        );
        assert_eq!(
            track_section["components"][0]["content"],
            "<:youtube:1563933220041523251> **YouTube**"
        );
        assert_eq!(track_section["accessory"]["type"], 11);
        assert_eq!(
            track_section["accessory"]["media"]["url"],
            "https://cdn.example.test/thumb.jpg"
        );
        assert!(
            payload
                .to_string()
                .contains("`0:55` ━━━━━●───────────── `3:18`")
        );
        assert!(
            payload
                .to_string()
                .contains("Requested by <@123456789012345678>")
        );
    }

    #[test]
    fn provider_source_titles_use_the_configured_custom_emoji() {
        for (provider_id, expected_line) in [
            ("bandcamp", "<:bandcamp:1563933089778892817> **Bandcamp**"),
            ("niconico", "<:niconico:1563933110478053548> **Niconico**"),
            (
                "soundcloud",
                "<:soundcloud:1563933135475843182> **SoundCloud**",
            ),
            ("twitch", "<:twitch:1563933156657070191> **Twitch**"),
            ("vimeo", "<:vimeo:1563933178136109156> **Vimeo**"),
            ("x", "<:x:1563933199787360496> **X**"),
            ("youtube", "<:youtube:1563933220041523251> **YouTube**"),
        ] {
            assert_eq!(
                provider_display_line(provider_id, "https://example.test/track"),
                expected_line
            );
        }
    }

    #[test]
    fn queued_track_response_renders_the_queued_item_as_a_static_panel() {
        let request = track("queued song".to_owned(), 0);
        let payload = serde_json::to_string(&queued_track_panel_components(
            &request,
            UserKey::new(123_456_789_012_345_678),
        ))
        .expect("queued player components serialize");

        assert!(payload.contains("**queued song**"));
        assert!(payload.contains("Requested by <@123456789012345678>"));
        assert!(!payload.contains("AutoMix **"));
        assert!(payload.contains("再生時間不明"));
    }

    #[test]
    fn player_panel_survives_the_track_change_snapshot_gap() {
        let now = Instant::now();
        assert!(!player_panel_no_playback_grace_expired(
            now,
            now + std::time::Duration::from_secs(29)
        ));
        assert!(player_panel_no_playback_grace_expired(
            now,
            now + std::time::Duration::from_secs(30)
        ));
    }

    #[test]
    fn older_playback_snapshot_cannot_rewrite_the_active_panel() {
        assert!(is_stale_player_panel_snapshot(
            Some(PlaybackId::new(12)),
            PlaybackId::new(11),
        ));
        assert!(!is_stale_player_panel_snapshot(
            Some(PlaybackId::new(12)),
            PlaybackId::new(12),
        ));
        assert!(!is_stale_player_panel_snapshot(
            Some(PlaybackId::new(12)),
            PlaybackId::new(13),
        ));
    }

    #[test]
    fn skipped_track_panel_freezes_until_playback_identity_changes() {
        let skipped = PlaybackId::new(21);
        assert!(is_skipped_playback_snapshot(Some(skipped), skipped));
        assert!(!is_skipped_playback_snapshot(
            Some(skipped),
            PlaybackId::new(22)
        ));
        assert!(!is_skipped_playback_snapshot(None, skipped));
    }

    #[test]
    fn each_track_request_routes_updates_to_its_own_panel() {
        let first = track("first song".to_owned(), 1);
        let second = track("second song".to_owned(), 2);
        let mut panels = GuildPlayerPanels::new();
        // Simulate Discord follow-up responses completing in reverse order.
        for (request, message_id) in [(&second, 202), (&first, 101)] {
            register_player_panel(
                &mut panels,
                PlayerPanelState {
                    channel_id: ChannelId::new(10),
                    message_id: MessageId::new(message_id),
                    request_id: request.request_id,
                    playback_id: None,
                    requester: UserKey::new(123),
                    last_payload: Arc::new(tokio::sync::Mutex::new(String::new())),
                },
            );
        }

        let selected = player_panel_for_request(&panels, second.request_id)
            .expect("the second track has its own panel");

        assert_eq!(selected.message_id, MessageId::new(202));
        assert!(panels.by_request_id.contains_key(&first.request_id));
        assert_eq!(panels.by_request_id.len(), 2);
    }

    #[test]
    fn stale_song_panels_are_pruned_without_reassigning_live_panels() {
        let first = track("first song".to_owned(), 1);
        let second = track("second song".to_owned(), 2);
        let queued = track("queued song".to_owned(), 3);
        let mut panels = GuildPlayerPanels::new();
        for (request, message_id) in [(&first, 101), (&second, 202), (&queued, 303)] {
            register_player_panel(
                &mut panels,
                PlayerPanelState {
                    channel_id: ChannelId::new(10),
                    message_id: MessageId::new(message_id),
                    request_id: request.request_id,
                    playback_id: None,
                    requester: UserKey::new(123),
                    last_payload: Arc::new(tokio::sync::Mutex::new(String::new())),
                },
            );
        }

        retain_live_player_panels(
            &mut panels,
            &HashSet::from([second.request_id, queued.request_id]),
        );

        assert!(player_panel_for_request(&panels, second.request_id).is_some());
        assert!(player_panel_for_request(&panels, queued.request_id).is_some());
        assert!(player_panel_for_request(&panels, first.request_id).is_none());
        assert_eq!(panels.by_request_id.len(), 2);
    }

    #[test]
    fn player_panel_omits_automix_status_text() {
        let metadata = TrackMetadata::new("title", "artist", "https://example.com/a", None, None);
        let payload = serde_json::to_string(&player_panel_components(
            &metadata,
            "direct",
            None,
            Some(UserKey::new(123_456_789_012_345_678)),
        ))
        .expect("panel serializes");

        assert!(payload.contains("Requested by <@123456789012345678>"));
        assert!(!payload.contains("AutoMix **"));
    }

    #[test]
    fn player_action_row_has_four_controls_and_invite_next_to_automix() {
        let payload = serde_json::to_value(player_action_row()).expect("controls serialize");
        assert_eq!(payload["type"], 1);
        let buttons = payload["components"].as_array().expect("button array");
        assert_eq!(buttons.len(), 5);
        let expected = [
            (BUTTON_SKIP, SKIP_EMOJI_NAME, SKIP_EMOJI_ID),
            (BUTTON_LOOP, LOOP_EMOJI_NAME, LOOP_EMOJI_ID),
            (BUTTON_SHUFFLE, SHUFFLE_EMOJI_NAME, SHUFFLE_EMOJI_ID),
            (BUTTON_AUTOMIX, AUTOMIX_EMOJI_NAME, AUTOMIX_EMOJI_ID),
        ];
        for (button, (custom_id, emoji_name, emoji_id)) in buttons.iter().take(4).zip(expected) {
            assert_eq!(button["custom_id"], custom_id);
            assert_eq!(button["emoji"]["name"], emoji_name);
            assert_eq!(button["emoji"]["id"], emoji_id.to_string());
        }
        for button in buttons.iter().take(3) {
            assert!(button.get("label").is_none());
        }
        assert_eq!(buttons[3]["label"], "AutoMix");
        let invitation = &buttons[4];
        assert_eq!(invitation["style"], 5);
        assert_eq!(invitation["label"], INVITE_BUTTON_LABEL);
        assert_eq!(
            invitation["url"],
            "https://discord.com/oauth2/authorize?client_id=1238488423208063107"
        );
        assert!(invitation.get("custom_id").is_none());
        assert!(invitation.get("emoji").is_none());
        assert!(matches!(
            component_action(BUTTON_QUEUE),
            Some(ComponentAction::Queue { .. })
        ));
    }

    #[test]
    fn player_panel_edit_keeps_components_v2_layout() {
        let metadata = TrackMetadata::new("title", "artist", "https://example.com/a", None, None);
        let components = player_panel_components(
            &metadata,
            "direct",
            None,
            Some(UserKey::new(123_456_789_012_345_678)),
        );
        let payload = serde_json::to_value(
            EditMessage::new()
                .allowed_mentions(CreateAllowedMentions::new())
                .components_v2(components),
        )
        .expect("Components V2 message edit serializes");

        assert!(payload.get("content").is_none());
        assert!(payload.get("embeds").is_none());
        assert!(
            payload["allowed_mentions"]["parse"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );
        assert_eq!(payload["components"][0]["type"], 17);
        assert_eq!(payload["components"][0]["components"][0]["type"], 10);
        assert_eq!(
            payload["components"][0]["components"][2]["content"],
            "Requested by <@123456789012345678>"
        );
        assert_eq!(payload["components"][0]["components"][3]["type"], 14);
        assert_eq!(payload["components"][0]["components"][4]["type"], 1);
    }

    #[test]
    fn player_panel_status_contains_only_the_requester() {
        let metadata = TrackMetadata::new("title", "artist", "https://example.com/a", None, None);
        let panel = serde_json::to_value(player_panel_components(
            &metadata,
            "direct",
            Some(std::time::Duration::from_secs(4)),
            Some(UserKey::new(123_456_789_012_345_678)),
        ))
        .expect("Components V2 player panel serializes");

        assert_eq!(
            panel[0]["components"][2]["content"],
            "Requested by <@123456789012345678>"
        );
        assert!(!panel.to_string().contains("AutoMix **"));
    }

    #[test]
    fn play_processing_uses_the_public_defer_from_the_previous_reply_flow() {
        let payload = serde_json::to_value(deferred_play_response())
            .expect("deferred interaction response serializes");
        assert_eq!(payload["type"], 5);
        assert!(payload["data"].get("flags").is_none());
    }

    #[test]
    fn invitation_is_a_link_button_in_the_action_row_after_automix() {
        let metadata = TrackMetadata::new("title", "artist", "https://example.com/a", None, None);
        let payload = serde_json::to_value(player_panel_components(
            &metadata,
            "direct",
            None,
            Some(UserKey::new(123_456_789_012_345_678)),
        ))
        .expect("panel serializes");
        let container = &payload[0]["components"];
        let action_row = &container[4];
        assert_eq!(action_row["type"], 1);
        let invite = &action_row["components"][4];
        assert_eq!(invite["type"], 2);
        assert_eq!(invite["style"], 5);
        assert_eq!(invite["label"], "招待");
        assert_eq!(INVITE_BUTTON_LABEL, "招待");
        assert_eq!(
            invite["url"],
            "https://discord.com/oauth2/authorize?client_id=1238488423208063107"
        );
        assert!(invite.get("custom_id").is_none());
        assert!(invite.get("emoji").is_none());
        assert_eq!(
            container[2]["content"],
            "Requested by <@123456789012345678>"
        );
        assert!(matches!(
            component_action(BUTTON_QUEUE),
            Some(ComponentAction::Queue { .. })
        ));
    }

    #[test]
    fn progress_handles_unknown_live_and_long_durations() {
        use std::time::Duration;

        assert_eq!(format_player_progress(None, None), "再生時間不明");
        assert_eq!(
            format_player_progress(Some(Duration::from_secs(9)), None),
            "`0:09` · 時間不明"
        );
        assert_eq!(
            format_player_progress(
                Some(Duration::from_secs(3661)),
                Some(Duration::from_secs(7200))
            ),
            "`1:01:01` ━━━━━━━━━●───────── `2:00:00`"
        );
        assert_eq!(
            format_player_progress(None, Some(Duration::from_secs(247))),
            "`--:--` ─────────────────── `4:07`"
        );
        assert_eq!(
            format_player_progress(
                Some(Duration::from_secs(198)),
                Some(Duration::from_secs(198))
            ),
            "`3:18` ━━━━━━━━━━━━━━━━━━● `3:18`"
        );
    }

    #[test]
    fn player_progress_clock_changes_every_second_and_bar_has_nineteen_glyphs() {
        use std::time::Duration;

        let first = format_player_progress(
            Some(Duration::from_secs(55)),
            Some(Duration::from_secs(247)),
        );
        let next = format_player_progress(
            Some(Duration::from_secs(56)),
            Some(Duration::from_secs(247)),
        );
        assert_ne!(first, next);
        assert!(first.contains("`0:55`"));
        assert!(next.contains("`0:56`"));
        let bar = first.split(' ').nth(1).expect("progress bar");
        assert_eq!(bar.chars().count(), PLAYER_PROGRESS_BAR_WIDTH);
        assert_eq!(bar.chars().filter(|ch| *ch == '●').count(), 1);
        assert!(bar.chars().all(|ch| matches!(ch, '━' | '●' | '─')));
    }

    #[test]
    fn display_text_escapes_markdown_and_does_not_mutate_unknown_progress() {
        assert_eq!(escape_markdown("*hello*"), "\\*hello\\*");
        let metadata = TrackMetadata::new("live", "artist", "https://example.com/a", None, None);
        let payload = serde_json::to_string(&player_panel_components(
            &metadata,
            "direct",
            Some(std::time::Duration::from_secs(23)),
            None,
        ))
        .expect("panel serializes");
        assert!(payload.contains("`0:23` · 時間不明"));
        assert!(payload.contains("Requested by 不明"));
        assert!(!payload.contains("AutoMix **"));
        assert!(!payload.contains("Now Playing"));
        assert!(!payload.contains("Audio effects"));
    }

    #[test]
    fn loop_and_shuffle_indicators_cannot_render_together() {
        let nickname = render_mode_nickname(
            BOT_BASE_NICKNAME,
            ModeNicknameState {
                looping: true,
                shuffle: true,
                automix: false,
            },
        );

        assert_eq!(nickname, "音葉🔁");
    }

    #[test]
    fn shuffle_and_automix_indicators_can_render_together() {
        let nickname = render_mode_nickname(
            BOT_BASE_NICKNAME,
            ModeNicknameState {
                looping: false,
                shuffle: true,
                automix: true,
            },
        );

        assert_eq!(nickname, "音葉🔀🎧");
    }

    #[test]
    fn disconnected_mode_uses_the_fixed_base_nickname() {
        assert_eq!(
            render_mode_nickname(BOT_BASE_NICKNAME, ModeNicknameState::default()),
            "音葉"
        );
    }

    #[test]
    fn mode_nickname_states_render_independently_per_guild() {
        let guild_a = ModeNicknameState {
            looping: true,
            ..ModeNicknameState::default()
        };
        let guild_b = ModeNicknameState {
            shuffle: true,
            ..ModeNicknameState::default()
        };

        assert_eq!(render_mode_nickname(BOT_BASE_NICKNAME, guild_a), "音葉🔁");
        assert_eq!(render_mode_nickname(BOT_BASE_NICKNAME, guild_b), "音葉🔀");
    }

    #[test]
    fn mode_nickname_removes_stale_indicators_and_respects_limit() {
        let base = format!("{} 🔁 🔀 🎧", "音".repeat(DISCORD_NICKNAME_MAX_CHARS));
        let nickname = render_mode_nickname(&base, ModeNicknameState::default());

        assert_eq!(nickname, "音".repeat(DISCORD_NICKNAME_MAX_CHARS));
    }

    #[test]
    fn play_command_declares_bounded_url_option() {
        let value = serde_json::to_value(play_command()).expect("command serializes");
        let option = &value["options"][0];
        assert_eq!(option["required"], true);
        assert_eq!(option["min_length"], 1);
        assert_eq!(option["max_length"], 2048);
    }

    #[test]
    fn command_matcher_rejects_schema_drift() {
        let option = |max_length| {
            serde_json::from_value(serde_json::json!({
                "type": 3,
                "name": "url",
                "name_localizations": null,
                "description": "URL",
                "description_localizations": null,
                "required": true,
                "choices": [],
                "options": [],
                "channel_types": [],
                "min_value": null,
                "max_value": null,
                "min_length": 1,
                "max_length": max_length,
                "autocomplete": false
            }))
            .expect("command option deserializes")
        };

        assert!(matches_play_option(&option(serde_json::json!(2048))));
        assert!(!matches_play_option(&option(serde_json::json!(2047))));
        assert!(!matches_play_option(&option(serde_json::Value::Null)));
    }

    fn track(title: String, index: usize) -> TrackRequest {
        TrackRequest::new(
            "test",
            format!("key-{index}"),
            format!("https://example.com/requested/{index}"),
            format!("https://example.com/canonical/{index}"),
            format!("https://example.com/source/{index}"),
            PreparedSource::http(
                format!("https://example.com/stream/{index}"),
                Vec::new(),
                None,
                None,
            ),
            TrackMetadata::new(
                title,
                "author",
                format!("https://example.com/watch/{index}"),
                None,
                None,
            ),
        )
    }
}
