use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use dashmap::DashMap;
use futures::future::join_all;
use serenity::{
    all::{
        ActivityData, ButtonStyle, ChannelId, Colour, Command, CommandInteraction,
        CommandOptionType, ComponentInteraction, Context, CreateActionRow, CreateButton,
        CreateCommand, CreateCommandOption, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter,
        CreateInteractionResponse, CreateInteractionResponseFollowup,
        CreateInteractionResponseMessage, EmojiId, Interaction, MessageFlags, ReactionType, Ready,
        VoiceServerUpdateEvent, VoiceState,
    },
    async_trait,
    builder::CreateMessage,
    cache::Settings as CacheSettings,
    client::{Client, EventHandler},
    http::Http,
};
use songbird::{SerenityInit, Songbird};
use tracing::{error, info};
use wotoha_contracts::{
    ChannelKey, GuildKey, PlaybackService, UserKey, VoiceActionAccess, VoiceGatewayEvent,
    VoiceGatewayRuntime, VoiceGatewayServerUpdate, VoiceGatewayStateUpdate, VoicePeerSnapshot,
    VoiceUpdateDecision,
};
use wotoha_control::{ComponentAction, ComponentOutcome, ControlService};
use wotoha_core::{
    QueuePreview, TrackMetadata,
    debug::append_debug_log,
    ui::{
        self, AUTOMIX_EMOJI_ID, AUTOMIX_EMOJI_NAME, AUTOMIX_NICKNAME, BUTTON_AUTOMIX, BUTTON_LOOP,
        BUTTON_LOOP_LABEL, BUTTON_QUEUE, BUTTON_QUEUE_LABEL, BUTTON_SHUFFLE, BUTTON_SHUFFLE_LABEL,
        BUTTON_SKIP, BUTTON_SKIP_LABEL, COLOR_ERROR, COLOR_INFO, LOOP_EMOJI_ID, LOOP_EMOJI_NAME,
        LOOPING_NICKNAME, MSG_ALLOWED_URL_ONLY, MSG_JOIN_ACTIVE_VOICE, MSG_JOIN_VOICE_FIRST,
        MSG_NO_TRACK_PLAYING, MSG_NOTHING_TO_SHUFFLE, MSG_PLAYING_IN_ANOTHER_VOICE,
        MSG_QUEUE_EMPTY, MSG_SHUFFLED, PLAY_COMMAND_DESCRIPTION, PLAY_COMMAND_NAME,
        PLAY_COMMAND_URL_OPTION, QUEUE_EMOJI_ID, QUEUE_EMOJI_NAME, SHUFFLE_EMOJI_ID,
        SHUFFLE_EMOJI_NAME, SKIP_EMOJI_ID, SKIP_EMOJI_NAME,
    },
    url::summarize_url_for_logs,
};

use crate::{SongbirdRuntime, reconnect::ReconnectStore};

const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const QUEUE_PREVIEW_LIMIT: usize = 10;
const EMBED_FIELD_LIMIT: usize = 1024;
const EMBED_TITLE_LIMIT: usize = 180;
const UPDATE_NOTICE_TITLE: &str = "アップデートのお知らせ";
const UPDATE_NOTICE_DESCRIPTION: &str =
    "最新版へのアップデートのため、まもなく再起動します。再生中の音声は一時的に停止します。";
const BUILD_VERSION: &str = match option_env!("WOTOHA_BUILD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

fn version_activity() -> ActivityData {
    let version = BUILD_VERSION.strip_prefix('v').unwrap_or(BUILD_VERSION);
    ActivityData::custom(format!("v{version}"))
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
    active_voice_channels: Arc<DashMap<GuildKey, ChannelKey>>,
    reconnect_store: ReconnectStore,
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
            active_voice_channels: Arc::default(),
            reconnect_store: ReconnectStore::from_env(),
        }
    }

    pub async fn notify_restart(&self, http: &Http) {
        let mut connections = self
            .active_voice_channels
            .iter()
            .map(|entry| (*entry.key(), *entry.value()))
            .collect::<Vec<_>>();
        connections.sort_unstable_by_key(|(guild_id, _)| guild_id.get());
        if let Err(error) = self.reconnect_store.save(&connections) {
            error!(error = %error, "failed to persist voice reconnect handoff");
        } else {
            info!(
                connections = connections.len(),
                "persisted voice reconnect handoff"
            );
        }

        let targets = self
            .notification_channels
            .iter()
            .filter(|entry| self.control.has_current_track(*entry.key()))
            .map(|entry| *entry.value())
            .collect::<Vec<_>>();
        let notifications = targets.into_iter().map(|channel_id| {
            channel_id.send_message(http, CreateMessage::new().embed(update_restart_embed()))
        });

        for result in join_all(notifications).await {
            if let Err(error) = result {
                error!(error = %error, "failed to send update restart notification");
            }
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

        for (guild_id, channel_id) in connections {
            match self.runtime.ensure_joined(guild_id, channel_id).await {
                Ok(_) => {
                    self.active_voice_channels.insert(guild_id, channel_id);
                    let serenity_guild_id = serenity::all::GuildId::new(guild_id.get());
                    let bot_user_id = UserKey::new(ctx.cache.current_user().id.get());
                    self.control.bootstrap_voice_state(
                        guild_id,
                        channel_id,
                        self.guild_voice_snapshot(ctx, serenity_guild_id, bot_user_id),
                    );
                    if self.control.automix_enabled(guild_id) {
                        let _ = serenity_guild_id
                            .edit_nickname(&ctx.http, Some(AUTOMIX_NICKNAME))
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
            "discord: /play received guild_id={} user_id={} url={source_url}",
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

        let (defer_result, join_result) = tokio::join!(
            command.defer(&ctx.http),
            self.runtime.ensure_joined(guild_key, user_channel_key),
        );
        defer_result?;

        let joined_now = match join_result {
            Ok(joined) => joined,
            Err(error) => {
                append_debug_log(format!("discord: ensure_joined failed: {error}"));
                command
                    .create_followup(
                        &ctx.http,
                        CreateInteractionResponseFollowup::new()
                            .flags(MessageFlags::EPHEMERAL)
                            .embed(error_embed(&format!("接続エラー: {error}"))),
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
                append_debug_log(format!(
                    "discord: control.play ok provider={} key={} title={} now_playing={}",
                    outcome.request.provider_id.as_ref(),
                    outcome.request.canonical_key.as_ref(),
                    outcome.request.metadata.title.as_ref(),
                    outcome.now_playing
                ));
                info!(
                    guild_id = guild_id.get(),
                    provider_id = outcome.request.provider_id.as_ref(),
                    canonical_key = outcome.request.canonical_key.as_ref(),
                    title = outcome.request.metadata.title.as_ref(),
                    now_playing = outcome.now_playing,
                    "play command resolved successfully"
                );
                if self.control.automix_enabled(guild_key) {
                    let _ = guild_id
                        .edit_nickname(&ctx.http, Some(AUTOMIX_NICKNAME))
                        .await;
                }
                let response = CreateInteractionResponseFollowup::new()
                    .embed(track_embed(
                        &outcome.request.metadata,
                        command.user.name.as_str(),
                        command.user.avatar_url().as_deref(),
                    ))
                    .components(vec![player_action_row()]);
                command.create_followup(&ctx.http, response).await?;
            }
            Err(error) => {
                append_debug_log(format!("discord: control.play failed: {error}"));
                error!(
                    guild_id = guild_id.get(),
                    user_id = command.user.id.get(),
                    source = source_url_log.as_str(),
                    error = %error,
                    "play command failed"
                );
                if joined_now && !self.control.has_current_track(guild_key) {
                    self.active_voice_channels.remove(&guild_key);
                    self.control.disconnect_guild(guild_key).await;
                }
                command
                    .create_followup(
                        &ctx.http,
                        CreateInteractionResponseFollowup::new()
                            .flags(MessageFlags::EPHEMERAL)
                            .embed(error_embed(&format!("読み込みエラー: {error}"))),
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
        let Some(guild_id) = component.guild_id else {
            return Ok(());
        };
        let guild_key = GuildKey::new(guild_id.get());

        let action = match component.data.custom_id.as_str() {
            BUTTON_SKIP => ComponentAction::Skip,
            BUTTON_LOOP => ComponentAction::Loop,
            BUTTON_SHUFFLE => ComponentAction::Shuffle,
            BUTTON_AUTOMIX => ComponentAction::AutoMix,
            BUTTON_QUEUE => ComponentAction::Queue {
                limit: QUEUE_PREVIEW_LIMIT,
            },
            _ => return Ok(()),
        };
        let actor_channel = self
            .user_voice_channel(ctx, guild_id, component.user.id)
            .map(|channel_id| ChannelKey::new(channel_id.get()));

        match self
            .control
            .handle_component(guild_key, actor_channel, action)
            .await
        {
            ComponentOutcome::Skip { was_looping } => {
                if was_looping {
                    let _ = guild_id.edit_nickname(&ctx.http, None).await;
                }
                component.defer(&ctx.http).await?;
            }
            ComponentOutcome::Loop { enabled } => {
                let nickname = enabled.then_some(LOOPING_NICKNAME);
                let _ = guild_id.edit_nickname(&ctx.http, nickname).await;
                component.defer(&ctx.http).await?;
            }
            ComponentOutcome::Shuffle => {
                component
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .ephemeral(true)
                                .embed(info_embed(MSG_SHUFFLED)),
                        ),
                    )
                    .await?;
            }
            ComponentOutcome::NothingToShuffle => {
                component
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .ephemeral(true)
                                .embed(error_embed(MSG_NOTHING_TO_SHUFFLE)),
                        ),
                    )
                    .await?;
            }
            ComponentOutcome::AutoMix { enabled } => {
                let nickname = enabled.then_some(AUTOMIX_NICKNAME);
                let _ = guild_id.edit_nickname(&ctx.http, nickname).await;
                component.defer(&ctx.http).await?;
            }
            ComponentOutcome::QueuePreview(preview) => {
                component
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .ephemeral(true)
                                .embed(queue_embed(&preview)),
                        ),
                    )
                    .await?;
            }
            ComponentOutcome::QueueEmpty => {
                component
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .ephemeral(true)
                                .embed(error_embed(MSG_QUEUE_EMPTY)),
                        ),
                    )
                    .await?;
            }
            ComponentOutcome::NoTrackPlaying => {
                component
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .ephemeral(true)
                                .embed(error_embed(MSG_NO_TRACK_PLAYING)),
                        ),
                    )
                    .await?;
            }
            ComponentOutcome::VoiceChannelRequired => {
                component
                    .create_response(
                        &ctx.http,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new()
                                .ephemeral(true)
                                .embed(error_embed(MSG_JOIN_ACTIVE_VOICE)),
                        ),
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
        ctx.set_activity(Some(version_activity()));
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
                }
                None => {
                    self.active_voice_channels.remove(&guild_key);
                    self.notification_channels.remove(&guild_key);
                    let _ = guild_id.edit_nickname(&ctx.http, None).await;
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
        self.control.disconnect_guild(guild_key).await;
        let _ = guild_id.edit_nickname(&ctx.http, None).await;
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

    async fn cache_ready(&self, _ctx: Context, guilds: Vec<serenity::all::GuildId>) {
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
        && command.options.len() == 1
        && command.options[0].name == PLAY_COMMAND_URL_OPTION
        && command.options[0].kind == CommandOptionType::String
}

fn play_command() -> CreateCommand {
    CreateCommand::new(PLAY_COMMAND_NAME)
        .description(PLAY_COMMAND_DESCRIPTION)
        .add_option(
            CreateCommandOption::new(CommandOptionType::String, PLAY_COMMAND_URL_OPTION, "URL")
                .required(true),
        )
}

fn error_embed(message: &str) -> CreateEmbed {
    CreateEmbed::new()
        .description(message)
        .color(Colour::new(COLOR_ERROR))
}

fn info_embed(message: &str) -> CreateEmbed {
    CreateEmbed::new()
        .description(message)
        .color(Colour::new(COLOR_INFO))
}

fn update_restart_embed() -> CreateEmbed {
    CreateEmbed::new()
        .title(UPDATE_NOTICE_TITLE)
        .description(UPDATE_NOTICE_DESCRIPTION)
        .color(Colour::new(COLOR_INFO))
}

fn track_embed(
    metadata: &TrackMetadata,
    requested_by: &str,
    avatar_url: Option<&str>,
) -> CreateEmbed {
    let mut embed = CreateEmbed::new()
        .color(Colour::new(COLOR_INFO))
        .author(CreateEmbedAuthor::new(metadata.author.as_ref()))
        .title(metadata.title.as_ref())
        .url(metadata.uri.as_ref())
        .field("Time", ui::format_duration(metadata.duration), true)
        .footer(match avatar_url {
            Some(icon_url) => CreateEmbedFooter::new(format!("Requested by {requested_by}"))
                .icon_url(icon_url.to_owned()),
            None => CreateEmbedFooter::new(format!("Requested by {requested_by}")),
        });

    if let Some(thumbnail_url) = &metadata.thumbnail_url {
        embed = embed.thumbnail(thumbnail_url.as_ref());
    }

    embed
}

fn queue_embed(preview: &QueuePreview) -> CreateEmbed {
    let mut embed = CreateEmbed::new()
        .title(format!(
            "<:{}:{}> Playlist",
            QUEUE_EMOJI_NAME, QUEUE_EMOJI_ID
        ))
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
    if text.chars().count() <= max_len {
        return text.to_owned();
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

fn player_action_row() -> CreateActionRow {
    CreateActionRow::Buttons(vec![
        player_button(
            BUTTON_SKIP,
            BUTTON_SKIP_LABEL,
            ButtonStyle::Secondary,
            SKIP_EMOJI_NAME,
            SKIP_EMOJI_ID,
        ),
        player_button(
            BUTTON_LOOP,
            BUTTON_LOOP_LABEL,
            ButtonStyle::Secondary,
            LOOP_EMOJI_NAME,
            LOOP_EMOJI_ID,
        ),
        player_button(
            BUTTON_SHUFFLE,
            BUTTON_SHUFFLE_LABEL,
            ButtonStyle::Secondary,
            SHUFFLE_EMOJI_NAME,
            SHUFFLE_EMOJI_ID,
        ),
        player_button(
            BUTTON_AUTOMIX,
            "",
            ButtonStyle::Secondary,
            AUTOMIX_EMOJI_NAME,
            AUTOMIX_EMOJI_ID,
        ),
        player_button(
            BUTTON_QUEUE,
            BUTTON_QUEUE_LABEL,
            ButtonStyle::Primary,
            QUEUE_EMOJI_NAME,
            QUEUE_EMOJI_ID,
        ),
    ])
}

fn player_button(
    custom_id: &str,
    label: &str,
    style: ButtonStyle,
    emoji_name: &str,
    emoji_id: u64,
) -> CreateButton {
    CreateButton::new(custom_id)
        .label(label)
        .style(style)
        .emoji(ReactionType::Custom {
            animated: false,
            id: EmojiId::new(emoji_id),
            name: Some(emoji_name.to_owned()),
        })
}

#[cfg(test)]
mod tests {
    use super::{
        BUILD_VERSION, EMBED_FIELD_LIMIT, queue_lines, truncate_embed_text, version_activity,
    };
    use serenity::all::ActivityType;
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
        let activity = version_activity();
        assert_eq!(
            activity.state.as_deref(),
            Some(format!("v{version}").as_str())
        );
        assert_eq!(activity.kind, ActivityType::Custom);
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
