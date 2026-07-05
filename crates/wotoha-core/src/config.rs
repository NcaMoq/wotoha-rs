use std::{env, path::PathBuf};

use thiserror::Error;

use crate::automix::AutoMixConfig;

const DEFAULT_LOG_DIR: &str = "target";
const DEFAULT_LOG_FILE: &str = "wotoha-app.runtime.log";
const DEFAULT_RUST_LOG: &str = "info,wotoha_debug=info";
const DEFAULT_LOG_ANSI: bool = false;
const DEFAULT_PLAYBACK_VOLUME: f32 = 0.10;
const DEFAULT_MAX_QUEUE_LEN: usize = 512;
const DEFAULT_MAX_PENDING_ENQUEUES: usize = 64;
const DEFAULT_AUTOMIX_ENABLED: bool = true;
const DEFAULT_AUTOMIX_CROSSFADE_SECONDS: f32 = 8.0;
const DEFAULT_AUTOMIX_MAX_TEMPO_ADJUSTMENT: f32 = 0.06;
const DEFAULT_AUTOMIX_MIN_BEAT_CONFIDENCE: f32 = 0.70;
const MAX_QUEUE_LEN_LIMIT: usize = 512;
const MAX_PENDING_ENQUEUES_LIMIT: usize = 64;
const MAX_PLAYBACK_VOLUME: f32 = 2.0;

#[derive(Clone, Debug)]
pub struct BotConfig {
    pub discord_token: String,
    pub logging: LogConfig,
    pub playback: PlaybackConfig,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogConfig {
    pub directory: PathBuf,
    pub file_name: String,
    pub rust_log: String,
    pub ansi: bool,
}

impl LogConfig {
    pub fn file_path(&self) -> PathBuf {
        self.directory.join(&self.file_name)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackConfig {
    pub default_volume: f32,
    pub max_queue_len: usize,
    pub max_pending_enqueues: usize,
    pub automix: AutoMixConfig,
}

impl BotConfig {
    pub fn load() -> Result<Self, ConfigError> {
        let _ = dotenvy::from_filename(".env");

        Self::from_vars(|name| env::var(name).ok())
    }

    fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let discord_token = read_required_string(&get, "DISCORD_TOKEN")?;
        let log_dir = read_optional_string(&get, "WOTOHA_LOG_DIR", DEFAULT_LOG_DIR)?;
        let log_file = validate_log_file_name(read_optional_string(
            &get,
            "WOTOHA_LOG_FILE",
            DEFAULT_LOG_FILE,
        )?)?;
        let rust_log = read_optional_string(&get, "RUST_LOG", DEFAULT_RUST_LOG)?;
        let log_ansi = read_optional_bool(&get, "WOTOHA_LOG_ANSI", DEFAULT_LOG_ANSI)?;
        let max_queue_len = read_optional_usize(
            &get,
            "WOTOHA_MAX_QUEUE_LEN",
            DEFAULT_MAX_QUEUE_LEN,
            1,
            MAX_QUEUE_LEN_LIMIT,
        )?;
        let max_pending_enqueues = read_optional_usize(
            &get,
            "WOTOHA_MAX_PENDING_ENQUEUES",
            DEFAULT_MAX_PENDING_ENQUEUES,
            1,
            MAX_PENDING_ENQUEUES_LIMIT,
        )?;
        if max_pending_enqueues > max_queue_len {
            return Err(ConfigError::InvalidRelation {
                name: "WOTOHA_MAX_PENDING_ENQUEUES",
                value: max_pending_enqueues,
                related_name: "WOTOHA_MAX_QUEUE_LEN",
                related_value: max_queue_len,
            });
        }
        let default_volume = read_optional_f32(
            &get,
            "WOTOHA_DEFAULT_VOLUME",
            DEFAULT_PLAYBACK_VOLUME,
            0.0,
            MAX_PLAYBACK_VOLUME,
        )?;
        let automix_enabled =
            read_optional_bool(&get, "WOTOHA_AUTOMIX_ENABLED", DEFAULT_AUTOMIX_ENABLED)?;
        let automix_crossfade_seconds = read_optional_f32(
            &get,
            "WOTOHA_AUTOMIX_CROSSFADE_SECONDS",
            DEFAULT_AUTOMIX_CROSSFADE_SECONDS,
            0.0,
            30.0,
        )?;
        let automix_max_tempo_adjustment = read_optional_f32(
            &get,
            "WOTOHA_AUTOMIX_MAX_TEMPO_ADJUSTMENT",
            DEFAULT_AUTOMIX_MAX_TEMPO_ADJUSTMENT,
            0.0,
            0.25,
        )?;
        let automix_min_beat_confidence = read_optional_f32(
            &get,
            "WOTOHA_AUTOMIX_MIN_BEAT_CONFIDENCE",
            DEFAULT_AUTOMIX_MIN_BEAT_CONFIDENCE,
            0.0,
            1.0,
        )?;

        Ok(Self {
            discord_token,
            logging: LogConfig {
                directory: PathBuf::from(log_dir),
                file_name: log_file,
                rust_log,
                ansi: log_ansi,
            },
            playback: PlaybackConfig {
                default_volume,
                max_queue_len,
                max_pending_enqueues,
                automix: AutoMixConfig {
                    enabled: automix_enabled,
                    crossfade: std::time::Duration::from_secs_f32(automix_crossfade_seconds),
                    max_tempo_adjustment: automix_max_tempo_adjustment,
                    min_beat_confidence: automix_min_beat_confidence,
                },
            },
        })
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("DISCORD_TOKEN is not set")]
    MissingDiscordToken,
    #[error("{name} is empty")]
    Empty { name: &'static str },
    #[error("{name} is not a valid boolean: {value}")]
    InvalidBool { name: &'static str, value: String },
    #[error("{name} is not a valid integer: {value}")]
    InvalidInteger { name: &'static str, value: String },
    #[error("{name} is not a valid number: {value}")]
    InvalidNumber { name: &'static str, value: String },
    #[error("{name} is out of range: {value} is outside {min}..={max}")]
    OutOfRange {
        name: &'static str,
        value: String,
        min: String,
        max: String,
    },
    #[error("{name} must be less than or equal to {related_name}: {value} > {related_value}")]
    InvalidRelation {
        name: &'static str,
        value: usize,
        related_name: &'static str,
        related_value: usize,
    },
    #[error("WOTOHA_LOG_FILE is not a plain file name: {value}")]
    InvalidLogFileName { value: String },
}

fn read_required_string(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
) -> Result<String, ConfigError> {
    get(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or(match name {
            "DISCORD_TOKEN" => ConfigError::MissingDiscordToken,
            _ => ConfigError::Empty { name },
        })
}

fn read_optional_string(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default: &'static str,
) -> Result<String, ConfigError> {
    match get(name) {
        Some(value) => {
            let trimmed = value.trim().to_owned();
            if trimmed.is_empty() {
                Err(ConfigError::Empty { name })
            } else {
                Ok(trimmed)
            }
        }
        None => Ok(default.to_owned()),
    }
}

fn read_optional_bool(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default: bool,
) -> Result<bool, ConfigError> {
    let Some(value) = get(name) else {
        return Ok(default);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Empty { name });
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::InvalidBool {
            name,
            value: trimmed.to_owned(),
        }),
    }
}

fn read_optional_usize(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, ConfigError> {
    let Some(value) = get(name) else {
        return Ok(default);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Empty { name });
    }
    let parsed = trimmed
        .parse::<usize>()
        .map_err(|_| ConfigError::InvalidInteger {
            name,
            value: trimmed.to_owned(),
        })?;
    if parsed < min || parsed > max {
        return Err(ConfigError::OutOfRange {
            name,
            value: parsed.to_string(),
            min: min.to_string(),
            max: max.to_string(),
        });
    }
    Ok(parsed)
}

fn read_optional_f32(
    get: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    default: f32,
    min: f32,
    max: f32,
) -> Result<f32, ConfigError> {
    let Some(value) = get(name) else {
        return Ok(default);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Empty { name });
    }
    let parsed = trimmed
        .parse::<f32>()
        .map_err(|_| ConfigError::InvalidNumber {
            name,
            value: trimmed.to_owned(),
        })?;
    if !parsed.is_finite() || parsed < min || parsed > max {
        return Err(ConfigError::OutOfRange {
            name,
            value: trimmed.to_owned(),
            min: min.to_string(),
            max: max.to_string(),
        });
    }
    Ok(parsed)
}

fn validate_log_file_name(value: String) -> Result<String, ConfigError> {
    let path = std::path::Path::new(&value);
    if path.components().count() != 1
        || value == "."
        || value == ".."
        || value.contains(['/', '\\'])
    {
        return Err(ConfigError::InvalidLogFileName { value });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{BotConfig, ConfigError};

    fn load_from(vars: &[(&str, &str)]) -> Result<BotConfig, ConfigError> {
        let vars = vars.iter().copied().collect::<HashMap<_, _>>();
        BotConfig::from_vars(|name| vars.get(name).map(|value| (*value).to_owned()))
    }

    #[test]
    fn fills_operational_defaults() {
        let config = load_from(&[("DISCORD_TOKEN", "token")]).unwrap();

        assert_eq!(config.discord_token, "token");
        assert_eq!(config.logging.directory, std::path::PathBuf::from("target"));
        assert_eq!(config.logging.file_name, "wotoha-app.runtime.log");
        assert_eq!(config.logging.rust_log, "info,wotoha_debug=info");
        assert!(!config.logging.ansi);
        assert_eq!(config.playback.default_volume, 0.10);
        assert_eq!(config.playback.max_queue_len, 512);
        assert_eq!(config.playback.max_pending_enqueues, 64);
        assert!(config.playback.automix.enabled);
        assert_eq!(config.playback.automix.crossfade.as_secs_f32(), 8.0);
    }

    #[test]
    fn reads_operational_environment_values() {
        let config = load_from(&[
            ("DISCORD_TOKEN", " token "),
            ("WOTOHA_LOG_DIR", "/var/log/wotoha"),
            ("WOTOHA_LOG_FILE", "runtime.log"),
            ("RUST_LOG", "warn,wotoha=debug"),
            ("WOTOHA_LOG_ANSI", "true"),
            ("WOTOHA_DEFAULT_VOLUME", "0.25"),
            ("WOTOHA_MAX_QUEUE_LEN", "256"),
            ("WOTOHA_MAX_PENDING_ENQUEUES", "32"),
            ("WOTOHA_AUTOMIX_ENABLED", "false"),
            ("WOTOHA_AUTOMIX_CROSSFADE_SECONDS", "12.5"),
            ("WOTOHA_AUTOMIX_MAX_TEMPO_ADJUSTMENT", "0.08"),
            ("WOTOHA_AUTOMIX_MIN_BEAT_CONFIDENCE", "0.80"),
        ])
        .unwrap();

        assert_eq!(config.discord_token, "token");
        assert_eq!(
            config.logging.directory,
            std::path::PathBuf::from("/var/log/wotoha")
        );
        assert_eq!(config.logging.file_name, "runtime.log");
        assert_eq!(config.logging.rust_log, "warn,wotoha=debug");
        assert!(config.logging.ansi);
        assert_eq!(config.playback.default_volume, 0.25);
        assert_eq!(config.playback.max_queue_len, 256);
        assert_eq!(config.playback.max_pending_enqueues, 32);
        assert!(!config.playback.automix.enabled);
        assert_eq!(config.playback.automix.crossfade.as_secs_f32(), 12.5);
        assert_eq!(config.playback.automix.max_tempo_adjustment, 0.08);
        assert_eq!(config.playback.automix.min_beat_confidence, 0.80);
    }

    #[test]
    fn rejects_nested_log_file_name() {
        let error = load_from(&[
            ("DISCORD_TOKEN", "token"),
            ("WOTOHA_LOG_FILE", "logs/app.log"),
        ])
        .unwrap_err();

        assert!(matches!(error, ConfigError::InvalidLogFileName { .. }));
    }

    #[test]
    fn rejects_pending_limit_above_queue_limit() {
        let error = load_from(&[
            ("DISCORD_TOKEN", "token"),
            ("WOTOHA_MAX_QUEUE_LEN", "32"),
            ("WOTOHA_MAX_PENDING_ENQUEUES", "64"),
        ])
        .unwrap_err();

        assert!(matches!(error, ConfigError::InvalidRelation { .. }));
    }

    #[test]
    fn rejects_non_finite_volume() {
        let error =
            load_from(&[("DISCORD_TOKEN", "token"), ("WOTOHA_DEFAULT_VOLUME", "nan")]).unwrap_err();

        assert!(matches!(error, ConfigError::OutOfRange { .. }));
    }

    #[test]
    fn rejects_queue_limits_above_runtime_range() {
        let error =
            load_from(&[("DISCORD_TOKEN", "token"), ("WOTOHA_MAX_QUEUE_LEN", "513")]).unwrap_err();

        assert!(matches!(error, ConfigError::OutOfRange { .. }));

        let error = load_from(&[
            ("DISCORD_TOKEN", "token"),
            ("WOTOHA_MAX_PENDING_ENQUEUES", "65"),
        ])
        .unwrap_err();

        assert!(matches!(error, ConfigError::OutOfRange { .. }));
    }
}
