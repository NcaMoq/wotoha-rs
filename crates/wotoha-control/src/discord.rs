use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serenity::{
    all::{
        ButtonStyle, ChannelId, Colour, Command, CommandInteraction, CommandOptionType,
        ComponentInteraction, Context, CreateActionRow, CreateButton, CreateCommand,
        CreateCommandOption, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter,
        CreateInteractionResponse, CreateInteractionResponseFollowup,
        CreateInteractionResponseMessage, EmojiId, Interaction, MessageFlags, ReactionType, Ready,
        VoiceState,
    },
    async_trait,
    cache::Settings as CacheSettings,
    client::EventHandler,
};
use songbird::get as get_songbird;
use tracing::{error, info};
use wotoha_contracts::{PlaybackService, VoicePeerSnapshot, VoiceUpdateDecision};
use wotoha_core::{
    QueuePreview,
    ui::{
        self, BUTTON_LOOP, BUTTON_LOOP_LABEL, BUTTON_QUEUE, BUTTON_QUEUE_LABEL, BUTTON_SHUFFLE,
        BUTTON_SHUFFLE_LABEL, BUTTON_SKIP, BUTTON_SKIP_LABEL, COLOR_ERROR, COLOR_INFO,
        LOOP_EMOJI_ID, LOOP_EMOJI_NAME, LOOPING_NICKNAME, MSG_ALLOWED_URL_ONLY,
        MSG_JOIN_VOICE_FIRST, MSG_NO_TRACK_PLAYING, MSG_NOTHING_TO_SHUFFLE, MSG_QUEUE_EMPTY,
        MSG_SHUFFLED, PLAY_COMMAND_DESCRIPTION, PLAY_COMMAND_NAME, PLAY_COMMAND_URL_OPTION,
        QUEUE_EMOJI_ID, QUEUE_EMOJI_NAME, SHUFFLE_EMOJI_ID, SHUFFLE_EMOJI_NAME, SKIP_EMOJI_ID,
        SKIP_EMOJI_NAME,
    },
};

const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const QUEUE_PREVIEW_LIMIT: usize = 10;

pub fn recommended_cache_settings() -> CacheSettings {
    let mut settings = CacheSettings::default();
    settings.time_to_live = CACHE_TTL;
    settings.cache_channels = false;
    settings.cache_users = false;
    settings
}

#[derive(Clone)]
pub struct DiscordControlPlane<P: PlaybackService> {
    playback: P,
    startup: Arc<StartupState>,
}

#[derive(Default)]
struct StartupState {
    boot_tasks_done: AtomicBool,
}

impl<P: PlaybackService> DiscordControlPlane<P> {
    pub fn new(playback: P) -> Self {
        Self {
            playback,
            startup: Arc::default(),
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

        if !wotoha_core::url::is_allowed_track_url(&source_url) {
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

        command.defer(&ctx.http).await?;

        let Some(manager) = get_songbird(ctx).await else {
            command
                .create_followup(
                    &ctx.http,
                    CreateInteractionResponseFollowup::new()
                        .ephemeral(true)
                        .embed(error_embed("音声マネージャの初期化に失敗しました。")),
                )
                .await?;
            return Ok(());
        };

        if manager.get(guild_id).is_none() {
            let (_, call_lock) = match manager.join_gateway(guild_id, user_channel).await {
                Ok(result) => result,
                Err(error) => {
                    command
                        .create_followup(
                            &ctx.http,
                            CreateInteractionResponseFollowup::new()
                                .ephemeral(true)
                                .embed(error_embed(&format!("接続エラー: {error}"))),
                        )
                        .await?;
                    return Ok(());
                }
            };

            let bot_user_id = ctx.cache.current_user().id;
            self.playback.bootstrap_voice_state(
                guild_id,
                user_channel,
                self.guild_voice_snapshot(ctx, guild_id, bot_user_id),
            );

            let mut call = call_lock.lock().await;
            let _ = call.deafen(true).await;
        }

        match self.playback.enqueue(manager, guild_id, &source_url).await {
            Ok(outcome) => {
                let embed = track_embed(
                    &outcome.request.metadata,
                    command.user.name.as_str(),
                    command.user.avatar_url().as_deref(),
                );
                let response = CreateInteractionResponseFollowup::new()
                    .embed(embed)
                    .components(vec![player_action_row()]);
                command.create_followup(&ctx.http, response).await?;
            }
            Err(error) => {
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

        match component.data.custom_id.as_str() {
            BUTTON_SKIP => {
                if !self.playback.has_current_track(guild_id) {
                    return component
                        .create_response(
                            &ctx.http,
                            CreateInteractionResponse::Message(
                                CreateInteractionResponseMessage::new()
                                    .ephemeral(true)
                                    .embed(error_embed(MSG_NO_TRACK_PLAYING)),
                            ),
                        )
                        .await;
                }

                let was_looping = self.playback.skip(guild_id).await.unwrap_or(false);
                if was_looping {
                    let _ = guild_id.edit_nickname(&ctx.http, None).await;
                }
                component.defer(&ctx.http).await?;
            }
            BUTTON_LOOP => {
                let Some(looping) = self.playback.toggle_loop(guild_id).await else {
                    return component
                        .create_response(
                            &ctx.http,
                            CreateInteractionResponse::Message(
                                CreateInteractionResponseMessage::new()
                                    .ephemeral(true)
                                    .embed(error_embed(MSG_NO_TRACK_PLAYING)),
                            ),
                        )
                        .await;
                };

                let nickname = if looping {
                    Some(LOOPING_NICKNAME)
                } else {
                    None
                };
                let _ = guild_id.edit_nickname(&ctx.http, nickname).await;
                component.defer(&ctx.http).await?;
            }
            BUTTON_SHUFFLE => {
                if !self.playback.shuffle(guild_id).await {
                    return component
                        .create_response(
                            &ctx.http,
                            CreateInteractionResponse::Message(
                                CreateInteractionResponseMessage::new()
                                    .ephemeral(true)
                                    .embed(error_embed(MSG_NOTHING_TO_SHUFFLE)),
                            ),
                        )
                        .await;
                }

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
            BUTTON_QUEUE => {
                let Some(preview) = self.playback.queue_preview(guild_id, QUEUE_PREVIEW_LIMIT)
                else {
                    return component
                        .create_response(
                            &ctx.http,
                            CreateInteractionResponse::Message(
                                CreateInteractionResponseMessage::new()
                                    .ephemeral(true)
                                    .embed(error_embed(MSG_QUEUE_EMPTY)),
                            ),
                        )
                        .await;
                };

                if preview.current().is_none() && preview.total_queued() == 0 {
                    return component
                        .create_response(
                            &ctx.http,
                            CreateInteractionResponse::Message(
                                CreateInteractionResponseMessage::new()
                                    .ephemeral(true)
                                    .embed(error_embed(MSG_QUEUE_EMPTY)),
                            ),
                        )
                        .await;
                }

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
            _ => {}
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
        bot_user_id: serenity::all::UserId,
    ) -> Vec<VoicePeerSnapshot> {
        let Some(guild) = ctx.cache.guild(guild_id) else {
            return Vec::new();
        };

        guild
            .voice_states
            .iter()
            .filter_map(|(user_id, state)| {
                if *user_id == bot_user_id {
                    return None;
                }

                state.channel_id.map(|channel_id| VoicePeerSnapshot {
                    user_id: *user_id,
                    channel_id,
                })
            })
            .collect()
    }
}

#[async_trait]
impl<P> EventHandler for DiscordControlPlane<P>
where
    P: PlaybackService,
{
    async fn ready(&self, ctx: Context, ready: Ready) {
        if self.startup.boot_tasks_done.swap(true, Ordering::AcqRel) {
            info!("Gateway ready received again; skipping boot-only tasks");
            return;
        }

        info!("Bot is ready! Resetting nicknames...");
        for guild in &ready.guilds {
            let _ = guild.id.edit_nickname(&ctx.http, None).await;
        }

        if let Err(error) = ensure_global_commands(&ctx).await {
            error!(error = %error, "failed to synchronize slash commands");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
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

        let bot_user_id = ctx.cache.current_user().id;
        if new.user_id == bot_user_id {
            self.playback
                .update_bot_voice_channel(guild_id, new.channel_id);
            return;
        }

        let decision = self.playback.apply_peer_voice_state(
            guild_id,
            new.user_id,
            old.and_then(|state| state.channel_id),
            new.channel_id,
        );
        if decision != VoiceUpdateDecision::DisconnectAlone {
            return;
        }

        let Some(manager) = get_songbird(&ctx).await else {
            return;
        };

        self.playback.disconnect_guild(manager, guild_id).await;
        let _ = guild_id.edit_nickname(&ctx.http, None).await;
    }

    async fn cache_ready(&self, _ctx: Context, guilds: Vec<serenity::all::GuildId>) {
        info!(guild_count = guilds.len(), "Cache is ready");
    }
}

async fn ensure_global_commands(ctx: &Context) -> serenity::Result<()> {
    let current = Command::get_global_commands(&ctx.http).await?;
    if matches_expected_commands(&current) {
        return Ok(());
    }

    let _ = Command::set_global_commands(&ctx.http, vec![play_command()]).await?;
    Ok(())
}

fn matches_expected_commands(commands: &[Command]) -> bool {
    if commands.len() != 1 {
        return false;
    }

    let Some(play) = commands.first() else {
        return false;
    };

    play.name == PLAY_COMMAND_NAME
        && play.description == PLAY_COMMAND_DESCRIPTION
        && play.options.len() == 1
        && play.options[0].name == PLAY_COMMAND_URL_OPTION
        && play.options[0].kind == CommandOptionType::String
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

fn track_embed(
    metadata: &wotoha_core::TrackMetadata,
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
            format!(
                "[{}]({})",
                current.metadata.title.as_ref(),
                current.metadata.uri.as_ref()
            ),
            false,
        );
    }

    if !preview.upcoming().is_empty() {
        let mut lines = String::new();
        for (index, track) in preview.upcoming().iter().enumerate() {
            lines.push_str(&format!(
                "{}. [{}]({})\n",
                index + 1,
                track.metadata.title.as_ref(),
                track.metadata.uri.as_ref()
            ));
        }

        if preview.total_queued() > preview.upcoming().len() {
            lines.push_str(&format!(
                "\n...and {} more songs",
                preview.total_queued() - preview.upcoming().len()
            ));
        }

        embed = embed.field("Next Up", lines, false);
    }

    embed
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
