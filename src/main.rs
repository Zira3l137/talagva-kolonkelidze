mod ai;
mod bot;
mod commands;
mod config;
mod game;
mod logging;

use anyhow::{Context as _, Result};
use poise::serenity_prelude::ChannelId;
use tracing::error;

pub(crate) type Error = Box<dyn std::error::Error + Send + Sync>;
pub(crate) type CommandContext<'a> = poise::Context<'a, Data, Error>;

pub(crate) struct Data {
    quotes_channel: ChannelId,
    games: game::discord::Games,
    prompts: game::PromptService,
    /// Present when `GROQ_API_KEY` is set; shared with the prompt service.
    ai: Option<std::sync::Arc<ai::AiClient>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env before configuring the subscriber so RUST_LOG may live there.
    config::load_dotenv().context("failed to load environment file")?;
    logging::init().context("failed to initialize logging")?;

    let result = async {
        let config = config::Config::from_env().context("invalid bot configuration")?;
        bot::run(config).await
    }
    .await;

    if let Err(error) = &result {
        error!(error = ?error, "bot terminated with an error");
    }
    result
}
