use std::time::Duration;

use poise::CreateReply;
use poise::serenity_prelude::{CreateAllowedMentions, CreateMessage, CreatePoll, CreatePollAnswer};
use tracing::{error, info};

use crate::movies::{AddOutcome, Movie, MAX_MOVIES};
use crate::{CommandContext, Error};

/// Suggest a movie for this channel's next movie night.
#[poise::command(slash_command, guild_only)]
pub(crate) async fn suggest(
    ctx: CommandContext<'_>,
    #[description = "Movie title"] movie_name: String,
    #[description = "Release year"] movie_year: u16,
    #[description = "Genre, e.g. Sci-Fi"] movie_genre: String,
) -> Result<(), Error> {
    let movie = match Movie::new(movie_name, movie_year, movie_genre) {
        Ok(movie) => movie,
        Err(error) => {
            private(ctx, error.to_string()).await?;
            return Ok(());
        }
    };

    let outcome = ctx.data().movies.lock().await.add(ctx.channel_id().get(), movie.clone())?;
    let response = match outcome {
        AddOutcome::Added => {
            info!(channel_id = %ctx.channel_id(), user_id = %ctx.author().id, movie = %movie.label(), "movie suggested");
            format!("Added **{}** to this channel's movie list.", movie.label())
        }
        AddOutcome::Duplicate => format!("**{}** is already on this channel's list.", movie.label()),
        AddOutcome::Full => format!("This channel already has {MAX_MOVIES} movies, Discord's poll limit. Start a poll or remove one first."),
    };
    private(ctx, response).await?;
    Ok(())
}

/// Remove a movie matching its title, year and genre from this channel.
#[poise::command(slash_command, guild_only)]
pub(crate) async fn forget(
    ctx: CommandContext<'_>,
    #[description = "Movie title"] movie_name: String,
    #[description = "Release year"] movie_year: u16,
    #[description = "Genre"] movie_genre: String,
) -> Result<(), Error> {
    let movie = match Movie::new(movie_name, movie_year, movie_genre) {
        Ok(movie) => movie,
        Err(error) => {
            private(ctx, error.to_string()).await?;
            return Ok(());
        }
    };
    let removed = ctx.data().movies.lock().await.remove(ctx.channel_id().get(), &movie)?;
    if removed {
        info!(channel_id = %ctx.channel_id(), user_id = %ctx.author().id, movie = %movie.label(), "movie forgotten");
        private(ctx, format!("Removed **{}** from this channel's movie list.", movie.label())).await?;
    } else {
        private(ctx, format!("**{}** wasn't found in this channel's list.", movie.label())).await?;
    }
    Ok(())
}

/// Start a native Discord movie poll and clear the channel's suggestions.
#[poise::command(slash_command, guild_only)]
pub(crate) async fn moviepoll(
    ctx: CommandContext<'_>,
    #[description = "Duration: 1h, 6h, 1d, 7d (max 32d)"] time: String,
) -> Result<(), Error> {
    let Some(hours) = parse_hours(&time) else {
        private(ctx, "Use a whole number of hours or days, e.g. `1h`, `12h`, `2d` (1h–32d).".into()).await?;
        return Ok(());
    };

    // Discord requires an interaction response within 3 seconds. Keep the
    // acknowledgement private; the actual poll is posted publicly below.
    ctx.defer_ephemeral().await?;
    let mut store = ctx.data().movies.lock().await;
    let movies = store.for_channel(ctx.channel_id().get());
    if movies.len() < 2 {
        drop(store);
        ctx.say("Suggest at least two movies in this channel before starting a poll.").await?;
        return Ok(());
    }

    let count = movies.len();
    let answers = movies
        .into_iter()
        .map(|movie| CreatePollAnswer::new().text(movie.label()))
        .collect();
    let poll = CreatePoll::new()
        .question("Which movie should we watch?")
        .answers(answers)
        .duration(Duration::from_secs(u64::from(hours) * 3600));

    // Keep the mutex across the HTTP request: a concurrent /suggest cannot
    // accidentally be cleared with the suggestions used for this poll.
    let message = ctx
        .channel_id()
        .send_message(
            ctx.http(),
            CreateMessage::new().poll(poll).allowed_mentions(CreateAllowedMentions::new()),
        )
        .await?;
    let clear_result = store.clear(ctx.channel_id().get());
    drop(store);

    if let Err(error) = clear_result {
        error!(channel_id = %ctx.channel_id(), message_id = %message.id, error = ?error,
            "movie poll posted, but failed to clear the saved suggestions");
        ctx.say(format!(
            "[Movie poll created]({}), but the saved movie list could not be cleared. Check the bot's storage/logs before starting another poll.",
            message.link()
        )).await?;
        return Ok(());
    }

    info!(channel_id = %ctx.channel_id(), message_id = %message.id, choices = count,
        hours, "movie poll created and suggestions cleared");
    ctx.say(format!(
        "[Movie poll created]({}) with {count} choices for {hours} hour(s). This channel's movie list is now empty.",
        message.link()
    )).await?;
    Ok(())
}

async fn private(ctx: CommandContext<'_>, message: String) -> Result<(), Error> {
    ctx.send(
        CreateReply::default()
            .content(message)
            .ephemeral(true)
            .allowed_mentions(CreateAllowedMentions::new()),
    )
    .await?;
    Ok(())
}

/// The native API requires duration in whole hours, from 1 through 768.
fn parse_hours(input: &str) -> Option<u16> {
    let input = input.trim().to_ascii_lowercase();
    let (number, multiplier): (&str, u16) = if let Some(number) = input.strip_suffix('d') {
        (number, 24)
    } else if let Some(number) = input.strip_suffix('h') {
        (number, 1)
    } else {
        (input.as_str(), 1)
    };
    let hours = number.trim().parse::<u16>().ok()?.checked_mul(multiplier)?;
    (1..=768).contains(&hours).then_some(hours)
}

#[cfg(test)]
mod tests {
    use super::parse_hours;

    #[test]
    fn valid_poll_durations() {
        assert_eq!(parse_hours("1h"), Some(1));
        assert_eq!(parse_hours(" 12H "), Some(12));
        assert_eq!(parse_hours("2d"), Some(48));
        assert_eq!(parse_hours("32d"), Some(768));
        assert_eq!(parse_hours("3"), Some(3));
    }

    #[test]
    fn invalid_poll_durations() {
        for invalid in ["", "0", "33d", "769h", "30m", "1.5h", "-2h", "65535d"] {
            assert_eq!(parse_hours(invalid), None, "{invalid}");
        }
    }
}
