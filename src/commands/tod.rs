use crate::game::{Category, Kind, discord};
use crate::{CommandContext, Error};

/// Play Truth or Dare with friends.
#[poise::command(slash_command, guild_only, subcommands("start", "quick", "stop", "show", "bump"), subcommand_required)]
pub(crate) async fn tod(_ctx: CommandContext<'_>) -> Result<(), Error> {
    Ok(())
}

/// Create a lobby in this text channel.
#[poise::command(slash_command, guild_only)]
async fn start(
    ctx: CommandContext<'_>,
    #[description = "The questions and challenges to use"] category: Option<Category>,
) -> Result<(), Error> {
    discord::create_lobby(ctx, category.unwrap_or(Category::Casual)).await.map_err(Into::into)
}

/// Get an immediate prompt without joining a game.
#[poise::command(slash_command, guild_only)]
async fn quick(
    ctx: CommandContext<'_>,
    #[description = "Truth or Dare"] kind: Kind,
    #[description = "Prompt category"] category: Option<Category>,
) -> Result<(), Error> {
    discord::quick(ctx, kind, category.unwrap_or(Category::Casual)).await.map_err(Into::into)
}

/// End the current game (host only).
#[poise::command(slash_command, guild_only)]
async fn stop(ctx: CommandContext<'_>) -> Result<(), Error> {
    discord::stop(ctx).await.map_err(Into::into)
}

/// Get a private link to the current game message, without posting in the channel.
#[poise::command(slash_command, guild_only)]
async fn show(ctx: CommandContext<'_>) -> Result<(), Error> {
    discord::show(ctx).await.map_err(Into::into)
}

/// Move the current game to the bottom of the channel (host only).
#[poise::command(slash_command, guild_only)]
async fn bump(ctx: CommandContext<'_>) -> Result<(), Error> {
    discord::bump(ctx).await.map_err(Into::into)
}
