use std::env;

use thiserror::Error;

#[derive(Clone, Debug)]
pub struct BotConfig {
    pub discord_token: String,
}

impl BotConfig {
    pub fn load() -> Result<Self, ConfigError> {
        let _ = dotenvy::from_filename(".env");

        let discord_token = env::var("DISCORD_TOKEN")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .ok_or(ConfigError::MissingDiscordToken)?;

        Ok(Self { discord_token })
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("DISCORD_TOKEN を環境変数または .env に設定してください。")]
    MissingDiscordToken,
}
