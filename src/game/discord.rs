//! Discord presentation and interaction handling for the game state machine.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context as _, Result};
use poise::serenity_prelude as serenity;
use serenity::builder::{
    CreateActionRow, CreateAllowedMentions, CreateButton, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, EditMessage,
};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::state::MAX_PLAYERS;
use super::{Action, Category, Game, Kind, Phase, PromptService, Transition};
use crate::CommandContext;

type SharedGame = Arc<Mutex<Game>>;

/// One game per text channel. A per-game mutex preserves interaction ordering
/// without blocking unrelated channels during Discord HTTP requests.
#[derive(Default)]
pub(crate) struct Games {
    sessions: Mutex<HashMap<serenity::ChannelId, SharedGame>>,
    next_id: AtomicU64,
}

impl Games {
    async fn find(&self, channel: serenity::ChannelId) -> Option<SharedGame> {
        self.sessions.lock().await.get(&channel).cloned()
    }

    async fn remove_if_current(&self, channel: serenity::ChannelId, session: &SharedGame) {
        let mut sessions = self.sessions.lock().await;
        if sessions.get(&channel).is_some_and(|stored| Arc::ptr_eq(stored, session)) {
            sessions.remove(&channel);
        }
    }
}

/// Create a lobby, remembering the message ID so stale buttons can never
/// manipulate a later game in the same channel.
pub(crate) async fn create_lobby(ctx: CommandContext<'_>, category: Category) -> Result<()> {
    ctx.defer_ephemeral()
        .await
        .context("failed to acknowledge /tod start")?;

    let channel = ctx.channel_id();
    let actor = ctx.author().id;
    let games = &ctx.data().games;
    // Abandoned sessions should not block this channel forever.
    if let Some(previous) = games.find(channel).await {
        let old = previous.lock().await;
        if old.is_expired() {
            if let Some(message_id) = old.message_id {
                let edit = EditMessage::new()
                    .content(finished_message("timed out after two hours of inactivity"))
                    .components(Vec::new())
                    .allowed_mentions(no_pings());
                if let Err(error) = channel.edit_message(ctx.http(), message_id, edit).await {
                    warn!(%channel, error = ?error, "could not disable buttons on expired lobby");
                }
            }
            drop(old);
            games.remove_if_current(channel, &previous).await;
        }
    }

    let id = games.next_id.fetch_add(1, Ordering::Relaxed) + 1;
    let session = Arc::new(Mutex::new(Game::new(id, actor, category)));
    {
        let mut sessions = games.sessions.lock().await;
        if sessions.contains_key(&channel) {
            drop(sessions);
            ctx.say("A Truth or Dare game already exists here. End it before creating another.")
                .await
                .context("failed to report existing game")?;
            return Ok(());
        }
        sessions.insert(channel, Arc::clone(&session));
    }

    let mut game = session.lock().await;
    let view = View::from_game(&game);
    let send = channel
        .send_message(ctx.http(), view.into_create_message())
        .await;
    let message = match send {
        Ok(message) => message,
        Err(error) => {
            drop(game);
            games.remove_if_current(channel, &session).await;
            return Err(error).context("failed to publish Truth or Dare lobby");
        }
    };
    game.message_id = Some(message.id);
    drop(game);
    info!(
        %channel, %actor, session_id = id, category = category.label(),
        "Truth or Dare lobby created"
    );
    ctx.say("Lobby created. Other players can now use **Join** in the game message.")
        .await
        .context("failed to acknowledge created lobby")?;
    Ok(())
}

/// A standalone prompt requires no lobby and doesn't mutate party game state.
pub(crate) async fn quick(ctx: CommandContext<'_>, kind: Kind, category: Category) -> Result<()> {
    let prompt = ctx.data().prompts.draw(category, kind, None).await;
    ctx.say(format!(
        "**{} · {}**\n\n{prompt}\n\nYou can always skip a prompt you're uncomfortable with.",
        kind.label(),
        category.label()
    ))
    .await
    .context("failed to send quick prompt")?;
    info!(
        user_id = %ctx.author().id,
        category = category.label(),
        kind = kind.label(),
        "quick prompt served"
    );
    Ok(())
}

/// Host-only slash-command fallback when the message scrolls out of view.
pub(crate) async fn stop(ctx: CommandContext<'_>) -> Result<()> {
    ctx.defer_ephemeral()
        .await
        .context("failed to acknowledge /tod stop")?;
    let channel = ctx.channel_id();
    let games = &ctx.data().games;
    let Some(session) = games.find(channel).await else {
        ctx.say("There is no active Truth or Dare game in this channel.")
            .await
            .context("failed to report missing game")?;
        return Ok(());
    };
    let game = session.lock().await;
    if ctx.author().id != game.host {
        ctx.say("Only the game host can end it.")
            .await
            .context("failed to report missing host permission")?;
        return Ok(());
    }
    let message_id = game
        .message_id
        .context("the game lobby has not finished initializing")?;
    let edit = EditMessage::new()
        .content(finished_message("ended by the host"))
        .components(Vec::new())
        .allowed_mentions(no_pings());
    channel
        .edit_message(ctx.http(), message_id, edit)
        .await
        .context("failed to close game message")?;
    info!(%channel, session_id = game.id, "Truth or Dare game ended by slash command");
    drop(game);
    games.remove_if_current(channel, &session).await;
    ctx.say("Truth or Dare game ended.")
        .await
        .context("failed to acknowledge end")?;
    Ok(())
}

/// Anyone can retrieve an ephemeral jump link without publishing another
/// message into an active conversation.
pub(crate) async fn show(ctx: CommandContext<'_>) -> Result<()> {
    ctx.defer_ephemeral().await.context("failed to acknowledge /tod show")?;
    let channel = ctx.channel_id();
    let Some(session) = ctx.data().games.find(channel).await else {
        ctx.say("There is no active Truth or Dare game in this channel.")
            .await.context("failed to report missing game")?;
        return Ok(());
    };
    let game = session.lock().await;
    if game.is_expired() {
        ctx.say("The previous game has expired. Use `/tod start` to play again.")
            .await.context("failed to report expired game")?;
        return Ok(());
    }
    let Some(message_id) = game.message_id else {
        ctx.say("The game message is still being created. Try again shortly.")
            .await.context("failed to report initializing game")?;
        return Ok(());
    };
    let guild = ctx.guild_id().context("/tod show requires a server")?;
    let link = message_link(guild, channel, message_id);
    drop(game);
    ctx.say(format!("[Jump to the current Truth or Dare game]({link})"))
        .await
        .context("failed to send game message link")?;
    Ok(())
}

/// The host can manually re-post the game if conversation hides the message
/// between turns. This doesn't advance or otherwise modify game state.
pub(crate) async fn bump(ctx: CommandContext<'_>) -> Result<()> {
    ctx.defer_ephemeral().await.context("failed to acknowledge /tod bump")?;
    let channel = ctx.channel_id();
    let Some(session) = ctx.data().games.find(channel).await else {
        ctx.say("There is no active Truth or Dare game in this channel.")
            .await.context("failed to report missing game")?;
        return Ok(());
    };
    let mut game = session.lock().await;
    if game.host != ctx.author().id {
        ctx.say("Only the game host can bump it.")
            .await.context("failed to report missing host permission")?;
        return Ok(());
    }
    if game.is_expired() {
        ctx.say("The previous game has expired. Use `/tod start` to play again.")
            .await.context("failed to report expired game")?;
        return Ok(());
    }
    repost(ctx.http(), channel, &mut game).await.context("failed to bump game")?;
    game.last_activity = Instant::now();
    let guild = ctx.guild_id().context("/tod bump requires a server")?;
    let message_id = game.message_id.context("bumped game lacks a message ID")?;
    info!(%channel, session_id = game.id, user_id = %ctx.author().id,
        "Truth or Dare game manually bumped");
    let link = message_link(guild, channel, message_id);
    drop(game);
    ctx.say(format!("Game moved: [jump to the latest message]({link})."))
        .await
        .context("failed to acknowledge bumped game")?;
    Ok(())
}

fn message_link(
    guild: serenity::GuildId,
    channel: serenity::ChannelId,
    message: serenity::MessageId,
) -> String {
    format!("https://discord.com/channels/{}/{}/{}", guild.get(), channel.get(), message.get())
}

/// Only callbacks with a game-specific custom ID are handled here; Poise owns
/// application slash command interactions separately.
pub(crate) async fn on_component(
    ctx: &serenity::Context,
    games: &Games,
    prompts: &PromptService,
    component: &serenity::ComponentInteraction,
) -> Result<()> {
    let Some((session_id, action)) = Action::parse(&component.data.custom_id) else {
        return Ok(());
    };
    let channel = component.channel_id;
    let Some(session) = games.find(channel).await else {
        return private_reply(ctx, component, "This game has ended or the bot was restarted.").await;
    };

    let mut game = session.lock().await;
    if game.id != session_id || game.message_id != Some(component.message.id) {
        return private_reply(
            ctx,
            component,
            "This button belongs to an older game. Use `/tod show` to find the current one.",
        )
        .await;
    }
    if game.is_expired() {
        let response = CreateInteractionResponseMessage::new()
            .content(finished_message("timed out after two hours of inactivity"))
            .components(Vec::new())
            .allowed_mentions(no_pings());
        component
            .create_response(ctx, CreateInteractionResponse::UpdateMessage(response))
            .await
            .context("failed to close expired game")?;
        info!(%channel, session_id, "Truth or Dare game expired");
        drop(game);
        games.remove_if_current(channel, &session).await;
        return Ok(());
    }

    // Stage changes on a copy. If Discord rejects the update, the actual
    // game stays on the old turn and the player can retry safely.
    let mut updated = game.clone();
    // The cache lookup has no network I/O: the 3-second Discord interaction
    // deadline is not held hostage by generation or Groq rate limits.
    let generated = match action {
        Action::Truth | Action::Dare
            if game.phase == Phase::Choosing
                && game.current_player() == Some(component.user.id) => {
            let kind = if action == Action::Truth { Kind::Truth } else { Kind::Dare };
            Some(prompts.draw(game.category, kind, game.last_prompt()).await)
        }
        _ => None,
    };
    let result = match updated.apply_with_prompt(component.user.id, action, generated) {
        Ok(result) => result,
        Err(message) => return private_reply(ctx, component, message).await,
    };
    if result == Transition::Updated && should_repost(action) {
        // Updating a Discord message does not move it to the latest position.
        // Acknowledge the button before performing two separate HTTP requests.
        component
            .create_response(ctx, CreateInteractionResponse::Acknowledge)
            .await
            .context("failed to acknowledge game action")?;

        if let Err(error) = repost(ctx.http.as_ref(), channel, &mut updated).await {
            warn!(%channel, session_id, error = ?error,
                "could not move game to the bottom; updating existing message instead");
            // If the new message failed to send, keep the existing game usable.
            // State is only committed after the fallback edit succeeds.
            channel
                .edit_message(
                    ctx,
                    component.message.id,
                    View::from_game(&updated).into_edit_message(),
                )
                .await
                .context("failed to update game after repost failure")?;
        }
    } else {
        // Lobby membership and game termination edit in place, avoiding noise.
        let response = match result {
            Transition::Updated => View::from_game(&updated).into_response(),
            Transition::Ended => CreateInteractionResponseMessage::new()
                .content(finished_message("ended by the host"))
                .components(Vec::new())
                .allowed_mentions(no_pings()),
        };
        component
            .create_response(ctx, CreateInteractionResponse::UpdateMessage(response))
            .await
            .context("failed to update Truth or Dare game message")?;
    }
    *game = updated;

    debug!(
        %channel, session_id, user_id = %component.user.id, ?action, ?result,
        "game action accepted"
    );
    if result == Transition::Ended {
        info!(%channel, session_id, "Truth or Dare game ended");
        drop(game);
        games.remove_if_current(channel, &session).await;
    }
    Ok(())
}

/// Repost only when the game progresses. Reposting every lobby join or leave
/// would generate unnecessary Discord traffic.
fn should_repost(action: Action) -> bool {
    matches!(
        action,
        Action::Begin | Action::Truth | Action::Dare | Action::Complete | Action::Skip
    )
}

/// Post the new state before removing the old message, so a failed send never
/// makes the live game disappear. The per-session lock protects this sequence
/// from simultaneous button presses and /tod bump calls.
async fn repost(
    http: &serenity::Http,
    channel: serenity::ChannelId,
    game: &mut Game,
) -> Result<()> {
    let previous = game.message_id.context("the game message is not initialized")?;
    let posted = channel
        .send_message(http, View::from_game(game).into_create_message())
        .await
        .context("failed to post game at the bottom of the channel")?;
    game.message_id = Some(posted.id);
    retire_previous(http, channel, previous, posted.id).await;
    debug!(%channel, old_message = %previous, new_message = %posted.id,
        "Truth or Dare message moved to the bottom");
    Ok(())
}

async fn retire_previous(
    http: &serenity::Http,
    channel: serenity::ChannelId,
    previous: serenity::MessageId,
    replacement: serenity::MessageId,
) {
    if let Err(error) = channel.delete_message(http, previous).await {
        warn!(%channel, %previous, error = ?error,
            "could not delete previous game message; disabling old buttons");
        // An old copy must never present apparently functional buttons. If
        // deletion fails, leave a pointer to the current message instead.
        let edit = EditMessage::new()
            .content(format!(
                "**Truth or Dare** has moved to message {replacement}. Use `/tod show` to jump to it."
            ))
            .components(Vec::new())
            .allowed_mentions(no_pings());
        if let Err(error) = channel.edit_message(http, previous, edit).await {
            warn!(%channel, %previous, error = ?error,
                "could not disable buttons on previous game message");
        }
    }
}

async fn private_reply(
    ctx: &serenity::Context,
    component: &serenity::ComponentInteraction,
    message: &str,
) -> Result<()> {
    let reply = CreateInteractionResponseMessage::new()
        .content(message)
        .ephemeral(true);
    component
        .create_response(ctx, CreateInteractionResponse::Message(reply))
        .await
        .context("failed to send ephemeral game response")
}

fn no_pings() -> CreateAllowedMentions {
    CreateAllowedMentions::new()
        .all_users(false)
        .all_roles(false)
        .everyone(false)
}

fn finished_message(reason: &str) -> String {
    format!("**Truth or Dare**\nGame {reason}. Use `/tod start` to play again.")
}

struct View {
    content: String,
    components: Vec<CreateActionRow>,
}

impl View {
    fn from_game(game: &Game) -> Self {
        let mut content = format!(
            "**Truth or Dare · {}**\nHost: <@{}> · Players: {}/{}\n",
            game.category.label(),
            game.host.get(),
            game.players.len(),
            MAX_PLAYERS,
        );
        let actions = match &game.phase {
            Phase::Lobby => {
                content.push_str("\n**Lobby open** — join below!\n");
                for player in &game.players {
                    content.push_str(&format!("• <@{}>\n", player.get()));
                }
                content.push_str("\nHost: press **Start** when at least two people have joined.");
                vec![
                    (Action::Join, "Join"),
                    (Action::Leave, "Leave"),
                    (Action::Begin, "Start"),
                    (Action::Stop, "End Game"),
                ]
            }
            Phase::Choosing => {
                content.push_str(&format!(
                    "\n**Round {}** · <@{}>'s turn.\nChoose **Truth** or **Dare**.\n",
                    game.round,
                    game.current_player().expect("active game needs a current player").get(),
                ));
                vec![
                    (Action::Truth, "Truth"),
                    (Action::Dare, "Dare"),
                    (Action::Skip, "Skip Turn"),
                    (Action::Stop, "End Game"),
                ]
            }
            Phase::Challenge { kind, prompt } => {
                content.push_str(&format!(
                    "\n**Round {}** · <@{}>'s turn.\n**{}:** {prompt}\n\n",
                    game.round,
                    game.current_player().expect("active game needs a current player").get(), kind.label(),
                ));
                content.push_str(
                    "Complete your prompt or skip it. The host can skip an inactive player's turn.",
                );
                vec![
                    (Action::Complete, "Completed"),
                    (Action::Skip, "Skip Turn"),
                    (Action::Stop, "End Game"),
                ]
            }
        };
        let buttons = actions
            .into_iter()
            .map(|(action, label)| {
                let style = match action {
                    Action::Stop => serenity::ButtonStyle::Danger,
                    Action::Begin | Action::Truth | Action::Dare | Action::Complete => {
                        serenity::ButtonStyle::Primary
                    }
                    _ => serenity::ButtonStyle::Secondary,
                };
                CreateButton::new(action.id(game.id))
                    .label(label)
                    .style(style)
            })
            .collect();
        Self {
            content,
            components: vec![CreateActionRow::Buttons(buttons)],
        }
    }

    fn into_create_message(self) -> CreateMessage {
        CreateMessage::new()
            .content(self.content)
            .components(self.components)
            .allowed_mentions(no_pings())
    }

    fn into_edit_message(self) -> EditMessage {
        EditMessage::new()
            .content(self.content)
            .components(self.components)
            .allowed_mentions(no_pings())
    }

    fn into_response(self) -> CreateInteractionResponseMessage {
        CreateInteractionResponseMessage::new()
            .content(self.content)
            .components(self.components)
            .allowed_mentions(no_pings())
    }
}

#[cfg(test)]
mod visibility_tests {
    use super::{Action, message_link, should_repost};
    use poise::serenity_prelude::{ChannelId, GuildId, MessageId};

    #[test]
    fn advances_are_reposted_but_lobby_edits_and_stop_are_not() {
        for action in [Action::Begin, Action::Truth, Action::Dare, Action::Complete, Action::Skip] {
            assert!(should_repost(action), "{action:?} should reposition the game");
        }
        for action in [Action::Join, Action::Leave, Action::Stop] {
            assert!(!should_repost(action), "{action:?} should edit in place");
        }
    }

    #[test]
    fn link_uses_discord_message_url_format() {
        assert_eq!(
            message_link(GuildId::new(1), ChannelId::new(2), MessageId::new(3)),
            "https://discord.com/channels/1/2/3"
        );
    }
}
