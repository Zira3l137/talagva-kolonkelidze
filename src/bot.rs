use anyhow::{Context as _, Result};
use poise::serenity_prelude as serenity;
use tracing::{error, info};

use crate::{Data, Error, commands, config::Config};

pub(crate) async fn run(config: Config) -> Result<()> {
    let intents =
        serenity::GatewayIntents::non_privileged() | serenity::GatewayIntents::MESSAGE_CONTENT;
    let quotes_channel = config.quotes_channel;

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![commands::enquote()],
            on_error: |error| Box::pin(handle_error(error)),
            // Preserve the old text prefix; this is not an application slash command.
            prefix_options: poise::PrefixFrameworkOptions {
                prefix: Some("/".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .setup(move |_, ready, _| {
            Box::pin(async move {
                info!(user = %ready.user.name, user_id = %ready.user.id, "connected to Discord");
                Ok(Data { quotes_channel })
            })
        })
        .build();

    let mut client = serenity::ClientBuilder::new(config.token, intents)
        .framework(framework)
        .await
        .context("failed to create Discord client")?;

    info!(quotes_channel_id = %quotes_channel.get(), "starting Discord gateway");
    client.start().await.context("Discord gateway stopped unexpectedly")?;
    Ok(())
}

async fn handle_error(error: poise::FrameworkError<'_, Data, Error>) {
    match error {
        poise::FrameworkError::Command { error, ctx, .. } => {
            error!(
                command = %ctx.command().name,
                user_id = %ctx.author().id,
                error = ?error,
                "command execution failed"
            );
            // Don't expose internal errors or stack traces to the channel.
            if let Err(reply_error) = ctx.say("Sorry, that command failed. Please try again.").await
            {
                error!(error = ?reply_error, "failed to send command error response");
            }
        }
        other => {
            if let Err(error) = poise::builtins::on_error(other).await {
                error!(error = ?error, "failed to handle framework error");
            }
        }
    }
}
