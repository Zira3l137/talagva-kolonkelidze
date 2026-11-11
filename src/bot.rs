use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use poise::serenity_prelude as serenity;
use tracing::error;
use tracing::info;

use crate::Data;
use crate::Error;
use crate::ai::AiClient;
use crate::commands;
use crate::config::Config;
use crate::game;
use crate::movies::MovieStore;

pub(crate) async fn run(config: Config) -> Result<()> {
    let intents =
        serenity::GatewayIntents::non_privileged() | serenity::GatewayIntents::MESSAGE_CONTENT;
    let quotes_channel = config.quotes_channel;
    // One client for the whole bot: every feature that talks to the model shares
    // its credentials and its request limit.
    let ai = config
        .ai
        .map(AiClient::new)
        .transpose()
        .context("failed to initialize the AI client")?
        .map(Arc::new);
    let tod = config.tod;
    let movies = MovieStore::open(config.movie_store_path)
        .context("failed to load the movie suggestion store")?;

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                commands::enquote(),
                commands::summary(),
                commands::tod(),
                commands::what(),
                commands::suggest(),
                commands::forget(),
                commands::moviepoll(),
            ],
            event_handler: |framework, event| {
                Box::pin(async move {
                    if let serenity::FullEvent::InteractionCreate { interaction } = event {
                        match interaction {
                            serenity::Interaction::Component(component) => {
                                game::discord::on_component(
                                    framework.serenity_context,
                                    &framework.user_data.games,
                                    &framework.user_data.prompts,
                                    component,
                                )
                                .await?;
                            }
                            serenity::Interaction::Modal(modal) => {
                                game::discord::on_modal(
                                    framework.serenity_context,
                                    &framework.user_data.games,
                                    modal,
                                )
                                .await?;
                            }
                            _ => {}
                        }
                    }
                    Ok(())
                })
            },
            on_error: |error| Box::pin(handle_error(error)),
            // Preserve the old text prefix; this is not an application slash command.
            prefix_options: poise::PrefixFrameworkOptions {
                prefix: Some("/".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .setup(move |ctx, ready, framework| {
            Box::pin(async move {
                info!(user = %ready.user.name, user_id = %ready.user.id, "connected to Discord");
                poise::builtins::register_globally(ctx, &framework.options().commands)
                    .await
                    .context("failed to register Discord slash commands")?;
                let prompts = game::PromptService::new(ai.clone(), tod)
                    .context("failed to initialize prompt generation")?;
                prompts.warm();
                Ok(Data {
                    quotes_channel,
                    games: game::discord::Games::default(),
                    prompts,
                    movies: tokio::sync::Mutex::new(movies),
                    ai,
                })
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
