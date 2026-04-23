use serenity::{
    all::{Context, GatewayIntents, GuildId, Ready},
    async_trait,
    client::{Client, EventHandler},
};
use tracing::{info, level_filters::LevelFilter};
use tracing_subscriber::EnvFilter;
use wotoha_core::BotConfig;

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
    let handler = ProbeHandler;
    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;

    let mut client = Client::builder(config.discord_token, intents)
        .event_handler(handler)
        .await?;

    client.start().await?;
    Ok(())
}

struct ProbeHandler;

#[async_trait]
impl EventHandler for ProbeHandler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!(guilds = ready.guilds.len(), "PROBE_READY");
    }

    async fn cache_ready(&self, _ctx: Context, guilds: Vec<GuildId>) {
        info!(guilds = guilds.len(), "PROBE_CACHE_READY");
    }
}
