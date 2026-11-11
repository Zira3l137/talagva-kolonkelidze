use std::path::PathBuf;
use std::{env, io};

use anyhow::{Context as _, Result, ensure};
use poise::serenity_prelude::ChannelId;

/// Runtime configuration. The token and API key must never be logged.
pub(crate) struct Config {
    pub token: String,
    pub quotes_channel: ChannelId,
    /// Present only when `GROQ_API_KEY` is set.
    pub ai: Option<AiConfig>,
    pub tod: TodConfig,
    /// JSON file holding per-channel movie suggestions between restarts.
    pub movie_store_path: PathBuf,
}

/// Credentials for the model provider. Anything that talks to the model
/// shares these; feature-specific settings live next to the feature.
#[derive(Clone)]
pub(crate) struct AiConfig {
    pub key: String,
    pub model: String,
}

/// Settings for the Truth or Dare game.
pub(crate) struct TodConfig {
    pub language: String,
    /// JSON file where generated prompts are saved between restarts.
    pub store_path: PathBuf,
}

impl TodConfig {
    fn from_env() -> Result<Self> {
        let language = env::var("TOD_LANGUAGE").unwrap_or_else(|_| "ka".into());
        let store_path = env::var("PROMPT_STORE_PATH").unwrap_or_else(|_| "prompt_store.json".into());
        Self::new(language, store_path)
    }

    fn new(language: String, store_path: String) -> Result<Self> {
        ensure!(matches!(language.as_str(), "ka" | "en"), "TOD_LANGUAGE must be ka or en");
        ensure!(!store_path.trim().is_empty(), "PROMPT_STORE_PATH cannot be empty");
        Ok(Self { language, store_path: PathBuf::from(store_path) })
    }
}

/// The .env file is optional when a host sets variables itself (e.g. Termux).
pub(crate) fn load_dotenv() -> Result<()> {
    match dotenvy::dotenv() {
        Ok(_) => Ok(()),
        Err(dotenvy::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("could not read .env"),
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let token = env::var("DISCORD_TOKEN").context("DISCORD_TOKEN is not set")?;
        ensure!(!token.trim().is_empty(), "DISCORD_TOKEN cannot be empty");
        let channel = env::var("TARGET_CHANNEL_ID").context("TARGET_CHANNEL_ID is not set")?;
        let quotes_channel = parse_channel_id(&channel)?;
        let ai = match env::var("GROQ_API_KEY") {
            Ok(key) => {
                ensure!(!key.trim().is_empty(), "GROQ_API_KEY is present but empty");
                let model = env::var("GROQ_MODEL")
                    .unwrap_or_else(|_| "qwen/qwen3.8-27b".into());
                ensure!(!model.trim().is_empty(), "GROQ_MODEL cannot be empty");
                Some(AiConfig { key, model })
            }
            Err(env::VarError::NotPresent) => None,
            Err(error) => return Err(error).context("failed to read GROQ_API_KEY"),
        };

        let movie_store_path = env::var("MOVIE_STORE_PATH")
            .unwrap_or_else(|_| "movie_store.json".into());
        ensure!(!movie_store_path.trim().is_empty(), "MOVIE_STORE_PATH cannot be empty");

        Ok(Self {
            token,
            quotes_channel,
            ai,
            tod: TodConfig::from_env()?,
            movie_store_path: PathBuf::from(movie_store_path),
        })
    }
}

fn parse_channel_id(raw: &str) -> Result<ChannelId> {
    let id: u64 = raw.parse().context("TARGET_CHANNEL_ID must be a numeric Discord channel ID")?;
    ensure!(id != 0, "TARGET_CHANNEL_ID cannot be zero");
    Ok(ChannelId::new(id))
}

#[cfg(test)]
mod tests {
    use super::{TodConfig, parse_channel_id};

    #[test]
    fn channel_id_accepts_positive_integer() {
        assert_eq!(parse_channel_id("123456").unwrap().get(), 123456);
    }

    #[test]
    fn channel_id_rejects_invalid_values() {
        assert!(parse_channel_id("not-a-channel").is_err());
        assert!(parse_channel_id("0").is_err());
    }

    #[test]
    fn tod_settings_are_validated() {
        let ok = TodConfig::new("en".into(), "prompts.json".into()).unwrap();
        assert_eq!(ok.language, "en");
        assert_eq!(ok.store_path.to_str(), Some("prompts.json"));
        assert!(TodConfig::new("fr".into(), "prompts.json".into()).is_err());
        assert!(TodConfig::new("ka".into(), "  ".into()).is_err());
    }
}
