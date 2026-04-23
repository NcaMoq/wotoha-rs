use serenity::{all::GatewayIntents, client::Client};
use songbird::SerenityInit;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use wotoha_control::{DiscordControlPlane, recommended_cache_settings};
use wotoha_core::BotConfig;
use wotoha_media::MediaResolver;
use wotoha_voice::PlaybackCoordinator;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    let config = BotConfig::load()?;
    let resolver = MediaResolver::new()?;
    let playback = PlaybackCoordinator::new(resolver);
    let handler = DiscordControlPlane::new(playback);

    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;
    let mut client = Client::builder(config.discord_token, intents)
        .cache_settings(recommended_cache_settings())
        .event_handler(handler)
        .register_songbird()
        .await?;

    client.start().await?;
    Ok(())
}
