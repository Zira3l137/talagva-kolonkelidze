use anyhow::{Context as _, Result};
use poise::serenity_prelude::builder::{CreateEmbed, CreateMessage};
use tracing::info;

use crate::{CommandContext, Error};

const MAX_DESCRIPTION_CHARS: usize = 4_000;

/// Save the message being replied to in the configured quotes channel.
#[poise::command(prefix_command, guild_only)]
pub(crate) async fn enquote(ctx: CommandContext<'_>) -> Result<(), Error> {
    execute(ctx).await.map_err(Into::into)
}

async fn execute(ctx: CommandContext<'_>) -> Result<()> {
    let poise::Context::Prefix(prefix) = ctx else {
        return Ok(());
    };

    let Some(reference) = &prefix.msg.message_reference else {
        ctx.say("Reply to a message with `/enquote`.")
            .await
            .context("failed to send usage hint")?;
        return Ok(());
    };

    let Some(message_id) = reference.message_id else {
        ctx.say("Couldn't identify the original message.")
            .await
            .context("failed to send missing-message hint")?;
        return Ok(());
    };

    let original = reference
        .channel_id
        .message(ctx.http(), message_id)
        .await
        .with_context(|| {
            format!("failed to fetch message {message_id} in {}", reference.channel_id)
        })?;

    let Some(description) = quote_description(
        &original.content,
        original.attachments.iter().map(|attachment| attachment.url.as_str()),
    ) else {
        ctx.say("That message has no text or attachments.")
            .await
            .context("failed to send empty-message hint")?;
        return Ok(());
    };

    let guild_id = ctx.guild_id().context("quote command was invoked outside a server")?;
    let source_url = format!(
        "https://discord.com/channels/{}/{}/{}",
        guild_id.get(),
        original.channel_id.get(),
        original.id.get()
    );
    let embed = CreateEmbed::new()
        .title(format!("Quote by {}", original.author.name))
        .description(description)
        .field("Source", format!("[Jump to message]({source_url})"), false)
        .color(0x5865F2);

    ctx.data()
        .quotes_channel
        .send_message(ctx.http(), CreateMessage::new().embed(embed))
        .await
        .with_context(|| {
            format!("failed to post quote to channel {}", ctx.data().quotes_channel)
        })?;

    info!(
        author_id = %ctx.author().id,
        source_message_id = %original.id,
        target_channel_id = %ctx.data().quotes_channel,
        "quote saved"
    );

    ctx.say("Quote saved! ✅")
        .await
        .context("quote posted, but failed to acknowledge command")?;
    Ok(())
}

/// Formatting is independent of Discord so it can be unit tested.
fn quote_description<'a>(
    content: &str,
    urls: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let attachments = urls.into_iter().collect::<Vec<_>>().join("\n");
    let formatted = match (content.is_empty(), attachments.is_empty()) {
        (true, true) => return None,
        (true, false) => attachments,
        (false, true) => format!("# *\"{content}\"*"),
        (false, false) => format!("# *\"{content}\"*\n\n{attachments}"),
    };
    Some(formatted.chars().take(MAX_DESCRIPTION_CHARS).collect())
}

#[cfg(test)]
mod tests {
    use super::{MAX_DESCRIPTION_CHARS, quote_description};

    #[test]
    fn empty_message_is_rejected() {
        assert_eq!(quote_description("", std::iter::empty::<&str>()), None);
    }

    #[test]
    fn text_is_formatted_as_quote() {
        assert_eq!(
            quote_description("Hello", std::iter::empty::<&str>()).as_deref(),
            Some("# *\"Hello\"*")
        );
    }

    #[test]
    fn attachments_are_preserved() {
        assert_eq!(
            quote_description("", ["https://example.com/a", "https://example.com/b"]).as_deref(),
            Some("https://example.com/a\nhttps://example.com/b")
        );
        assert_eq!(
            quote_description("Hello", ["https://example.com/a"]).as_deref(),
            Some("# *\"Hello\"*\n\nhttps://example.com/a")
        );
    }

    #[test]
    fn description_is_truncated_on_character_boundaries() {
        let content = "ა".repeat(MAX_DESCRIPTION_CHARS + 10);
        let result = quote_description(&content, std::iter::empty::<&str>()).unwrap();
        assert_eq!(result.chars().count(), MAX_DESCRIPTION_CHARS);
    }
}
