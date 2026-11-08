use std::{env, io};

use anyhow::{Context as _, Result, ensure};
use poise::serenity_prelude::ChannelId;

/// Runtime configuration. The token must never be logged.
pub(crate) struct Config {
    pub token: String,
    pub quotes_channel: ChannelId,
    pub groq: Option<GroqConfig>,
}

pub(crate) struct GroqConfig {
    pub key: String,
    pub model: String,
    pub language: String,
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
        let groq = match env::var("GROQ_API_KEY") {
            Ok(key) => {
                ensure!(!key.trim().is_empty(), "GROQ_API_KEY is present but empty");
                let model = env::var("GROQ_MODEL")
                    .unwrap_or_else(|_| "qwen/qwen3.8-27b".into());
                ensure!(!model.trim().is_empty(), "GROQ_MODEL cannot be empty");
                let language = env::var("TOD_LANGUAGE").unwrap_or_else(|_| "ka".into());
                ensure!(matches!(language.as_str(), "ka" | "en"), "TOD_LANGUAGE must be ka or en");
                Some(GroqConfig { key, model, language })
            }
            Err(env::VarError::NotPresent) => None,
            Err(error) => return Err(error).context("failed to read GROQ_API_KEY"),
        };

        Ok(Self { token, quotes_channel, groq })
    }
}

fn parse_channel_id(raw: &str) -> Result<ChannelId> {
    let id: u64 = raw.parse().context("TARGET_CHANNEL_ID must be a numeric Discord channel ID")?;
    ensure!(id != 0, "TARGET_CHANNEL_ID cannot be zero");
    Ok(ChannelId::new(id))
}

#[cfg(test)]
mod tests {
    use super::parse_channel_id;

    #[test]
    fn channel_id_accepts_positive_integer() {
        assert_eq!(parse_channel_id("123456").unwrap().get(), 123456);
    }

    #[test]
    fn channel_id_rejects_invalid_values() {
        assert!(parse_channel_id("not-a-channel").is_err());
        assert!(parse_channel_id("0").is_err());
    }
}
