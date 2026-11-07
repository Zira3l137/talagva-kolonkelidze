use anyhow::Context;
use anyhow::Result;
use poise::serenity_prelude as serenity;
use serenity::builder::CreateEmbed;
use serenity::builder::CreateMessage;
use serenity::model::id::ChannelId;

type Error = Box<dyn std::error::Error + Send + Sync>;
type ErrorContext<'a> = poise::Context<'a, Data, Error>;

struct Data {
    quotes_channel: ChannelId,
}

// Reply to a message with ?enquote.
#[poise::command(prefix_command, guild_only)]
async fn enquote(ctx: ErrorContext<'_>) -> Result<(), Error> {
    // This command only supports prefix invocation.
    let poise::Context::Prefix(prefix) = ctx else {
        return Ok(());
    };

    // Find the message being replied to.
    let Some(reference) = &prefix.msg.message_reference else {
        ctx.say("Reply to a message with `?quote`.").await?;
        return Ok(());
    };

    let Some(message_id) = reference.message_id else {
        ctx.say("Couldn't identify the original message.").await?;
        return Ok(());
    };

    // Retrieve the original message from Discord.
    let original = reference.channel_id.message(ctx.http(), message_id).await?;

    // Include attachments as links.
    let attachments = original
        .attachments
        .iter()
        .map(|attachment| attachment.url.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let content = if original.content.is_empty() {
        attachments.clone()
    } else if attachments.is_empty() {
        format!("# *\"{}\"*", original.content.clone())
    } else {
        format!("# *\"{}\"*\n\n{}", original.content, attachments)
    };

    if content.is_empty() {
        ctx.say("That message has no text or attachments.").await?;
        return Ok(());
    }

    // Discord embed descriptions have a 4096-character limit.
    let description = content.chars().take(4000).collect::<String>();

    let guild_id = ctx.guild_id().expect("guild_only command");
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

    // Post to #quotes.
    ctx.data().quotes_channel.send_message(ctx.http(), CreateMessage::new().embed(embed)).await?;

    ctx.say("Quote saved! ✅").await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok().context("failed to retrieve .env variables")?;
    let token = std::env::var("DISCORD_TOKEN").context("failed to retrieve discord bot token")?;
    let quotes_channel: u64 = std::env::var("TARGET_CHANNEL_ID")
        .context("failed to retrieve target channel id")?
        .parse()
        .context("failed to parse channel id from string")?;

    let intents =
        serenity::GatewayIntents::non_privileged() | serenity::GatewayIntents::MESSAGE_CONTENT;

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![enquote()],

            prefix_options: poise::PrefixFrameworkOptions {
                prefix: Some("/".into()),
                ..Default::default()
            },

            ..Default::default()
        })
        .setup(move |ctx, ready, framework| {
            Box::pin(async move {
                println!("Logged in as {}", ready.user.name);

                poise::builtins::register_globally(ctx, &framework.options().commands).await?;

                Ok(Data { quotes_channel: ChannelId::new(quotes_channel) })
            })
        })
        .build();

    let mut client = serenity::ClientBuilder::new(token, intents)
        .framework(framework)
        .await
        .context("failed to create serenity client")?;

    client.start().await.context("failed to start a client")?;

    Ok(())
}
