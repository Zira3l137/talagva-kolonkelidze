use std::env;

use anyhow::{Context as _, Result, anyhow};
use tracing_subscriber::EnvFilter;

/// RUST_LOG overrides the default filter, including per-module directives.
pub(crate) fn init() -> Result<()> {
    let filter = match env::var("RUST_LOG") {
        Ok(value) => EnvFilter::try_new(value).context("invalid RUST_LOG filter")?,
        Err(env::VarError::NotPresent) => EnvFilter::new("info,serenity=warn,poise=warn"),
        Err(error) => return Err(error).context("failed to read RUST_LOG"),
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init()
        .map_err(|error| anyhow!("failed to install tracing subscriber: {error}"))
}
