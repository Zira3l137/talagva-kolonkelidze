use std::time::{Duration, Instant};

use poise::serenity_prelude::{MessageId, UserId};

use super::{Category, Kind, draw, random};

pub(crate) const MAX_PLAYERS: usize = 16;
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60 * 2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Join,
    Leave,
    Begin,
    Truth,
    Dare,
    Complete,
    Skip,
    Stop,
}

impl Action {
    pub fn id(self, session_id: u64) -> String {
        let verb = match self {
            Self::Join => "join",
            Self::Leave => "leave",
            Self::Begin => "begin",
            Self::Truth => "truth",
            Self::Dare => "dare",
            Self::Complete => "complete",
            Self::Skip => "skip",
            Self::Stop => "stop",
        };
        format!("tod:{session_id}:{verb}")
    }

    pub fn parse(raw: &str) -> Option<(u64, Self)> {
        let mut parts = raw.split(':');
        if parts.next()? != "tod" {
            return None;
        }
        let session_id = parts.next()?.parse().ok()?;
        let action = match parts.next()? {
            "join" => Self::Join,
            "leave" => Self::Leave,
            "begin" => Self::Begin,
            "truth" => Self::Truth,
            "dare" => Self::Dare,
            "complete" => Self::Complete,
            "skip" => Self::Skip,
            "stop" => Self::Stop,
            _ => return None,
        };
        parts.next().is_none().then_some((session_id, action))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Lobby,
    Choosing,
    Challenge { kind: Kind, prompt: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Transition {
    Updated,
    Ended,
}

/// The longest written response a player may submit.
pub(crate) const MAX_RESPONSE_CHARS: usize = 1000;

const NO_CHALLENGE: &str = "There isn't a challenge to complete yet.";

/// Everything worth keeping about a finished turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletedTurn {
    pub player: UserId,
    pub round: u32,
    pub kind: Kind,
    pub prompt: String,
    /// `None` when the player left the response empty (for example, a spoken dare).
    pub response: Option<String>,
}

/// Custom ID of the response form. It names the round so that a form left
/// open while the turn moved on can never be recorded against a later challenge.
pub(crate) fn response_id(session_id: u64, round: u32) -> String {
    format!("tod:{session_id}:response:{round}")
}

pub(crate) fn parse_response_id(raw: &str) -> Option<(u64, u32)> {
    let mut parts = raw.split(':');
    if parts.next()? != "tod" {
        return None;
    }
    let session_id = parts.next()?.parse().ok()?;
    if parts.next()? != "response" {
        return None;
    }
    let round = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((session_id, round))
}

/// Trim a submitted response; blank means no response, overlong text is cut.
pub(crate) fn normalize_response(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let cut: String = trimmed.chars().take(MAX_RESPONSE_CHARS).collect();
    Some(cut.trim_end().to_owned())
}

#[derive(Debug, Clone)]
pub(crate) struct Game {
    pub id: u64,
    pub host: UserId,
    pub message_id: Option<MessageId>,
    pub players: Vec<UserId>,
    pub category: Category,
    pub phase: Phase,
    pub round: u32,
    pub last_activity: Instant,
    current: usize,
    last_prompt: Option<String>,
}

impl Game {
    pub fn new(id: u64, host: UserId, category: Category) -> Self {
        Self {
            id,
            host,
            message_id: None,
            players: vec![host],
            category,
            phase: Phase::Lobby,
            round: 0,
            last_activity: Instant::now(),
            current: 0,
            last_prompt: None,
        }
    }

    pub fn current_player(&self) -> Option<UserId> {
        (!matches!(&self.phase, Phase::Lobby)).then(|| self.players[self.current])
    }

    pub fn is_expired(&self) -> bool {
        self.last_activity.elapsed() >= IDLE_TIMEOUT
    }

    pub fn last_prompt(&self) -> Option<&str> {
        self.last_prompt.as_deref()
    }

    /// Whether `actor` may finish the current challenge right now.
    pub fn can_complete(&self, actor: UserId) -> Result<(), &'static str> {
        self.require_turn(actor)?;
        if matches!(&self.phase, Phase::Challenge { .. }) {
            Ok(())
        } else {
            Err(NO_CHALLENGE)
        }
    }

    /// Finish the current challenge with an optional written response and pass
    /// the turn on. The returned record is what the channel should keep.
    pub fn complete_turn(
        &mut self,
        actor: UserId,
        response: Option<String>,
    ) -> Result<CompletedTurn, &'static str> {
        self.require_turn(actor)?;
        let Phase::Challenge { kind, prompt } = self.phase.clone() else {
            return Err(NO_CHALLENGE);
        };
        let completed = CompletedTurn {
            player: actor,
            round: self.round,
            kind,
            prompt,
            response: response.as_deref().and_then(normalize_response),
        };
        self.advance();
        self.last_activity = Instant::now();
        Ok(completed)
    }

    #[cfg(test)]
    pub fn apply(&mut self, actor: UserId, action: Action) -> Result<Transition, &'static str> {
        self.apply_with_prompt(actor, action, None)
    }

    /// An externally generated prompt is only consumed after the turn is validated.
    pub fn apply_with_prompt(
        &mut self,
        actor: UserId,
        action: Action,
        supplied_prompt: Option<String>,
    ) -> Result<Transition, &'static str> {
        use Action as A;
        let outcome = match action {
            A::Stop => {
                if actor != self.host {
                    return Err("Only the host can end this game.");
                }
                Transition::Ended
            }
            A::Join => {
                if self.phase != Phase::Lobby {
                    return Err("The game has already started.");
                }
                if self.players.contains(&actor) {
                    return Err("You're already in the game.");
                }
                if self.players.len() == MAX_PLAYERS {
                    return Err("This lobby is full.");
                }
                self.players.push(actor);
                Transition::Updated
            }
            A::Leave => {
                if self.phase != Phase::Lobby {
                    return Err("You can't leave after the game starts; ask the host to end it.");
                }
                if actor == self.host {
                    return Err("The host should use End Game instead.");
                }
                if !self.players.contains(&actor) {
                    return Err("You're not in this lobby.");
                }
                self.players.retain(|user| *user != actor);
                Transition::Updated
            }
            A::Begin => {
                if actor != self.host {
                    return Err("Only the host can start the game.");
                }
                if self.phase != Phase::Lobby {
                    return Err("The game has already started.");
                }
                if self.players.len() < 2 {
                    return Err("At least two players must join before starting.");
                }
                self.current = random::index(self.players.len());
                self.round = 1;
                self.phase = Phase::Choosing;
                Transition::Updated
            }
            A::Truth | A::Dare => {
                self.require_turn(actor)?;
                if self.phase != Phase::Choosing {
                    return Err("It's not time to choose Truth or Dare.");
                }
                let kind = if action == A::Truth {
                    Kind::Truth
                } else {
                    Kind::Dare
                };
                let prompt = supplied_prompt.unwrap_or_else(|| {
                    draw(self.category, kind, self.last_prompt.as_deref()).to_owned()
                });
                self.last_prompt = Some(prompt.clone());
                self.phase = Phase::Challenge { kind, prompt };
                Transition::Updated
            }
            A::Complete => {
                self.complete_turn(actor, None)?;
                Transition::Updated
            }
            A::Skip => {
                if actor != self.host {
                    self.require_turn(actor)?;
                }
                if self.phase == Phase::Lobby {
                    return Err("Start the game before skipping turns.");
                }
                self.advance();
                Transition::Updated
            }
        };
        self.last_activity = Instant::now();
        Ok(outcome)
    }

    fn require_turn(&self, actor: UserId) -> Result<(), &'static str> {
        if self.current_player() == Some(actor) {
            Ok(())
        } else {
            Err("Only the current player can use this button.")
        }
    }

    fn advance(&mut self) {
        self.current = (self.current + 1) % self.players.len();
        self.round += 1;
        self.phase = Phase::Choosing;
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, Category, Game, Kind, MAX_PLAYERS, Phase, Transition, UserId, IDLE_TIMEOUT};

    fn user(id: u64) -> UserId {
        UserId::new(id)
    }

    #[test]
    fn ids_are_strict_and_versioned() {
        for action in [
            Action::Join,
            Action::Leave,
            Action::Begin,
            Action::Truth,
            Action::Dare,
            Action::Complete,
            Action::Skip,
            Action::Stop,
        ] {
            assert_eq!(Action::parse(&action.id(42)), Some((42, action)));
        }
        assert_eq!(Action::parse("tod:42:unknown"), None);
        assert_eq!(Action::parse("tod:42:join:extra"), None);
        assert_eq!(Action::parse("other:42:join"), None);
    }

    #[test]
    fn lobby_permissions_and_limits() {
        let mut game = Game::new(1, user(1), Category::Casual);
        assert_eq!(
            game.apply(user(1), Action::Begin),
            Err("At least two players must join before starting.")
        );
        assert!(game.apply(user(2), Action::Join).is_ok());
        assert!(game.apply(user(2), Action::Join).is_err());
        assert!(game.apply(user(2), Action::Begin).is_err());
        assert!(game.apply(user(1), Action::Leave).is_err());
        assert_eq!(game.apply(user(2), Action::Leave), Ok(Transition::Updated));
        for id in 2..=MAX_PLAYERS as u64 {
            assert!(game.apply(user(id), Action::Join).is_ok());
        }
        assert!(game.apply(user(100), Action::Join).is_err());
    }

    #[test]
    fn turns_are_private_and_advance_without_repeats() {
        let mut game = Game::new(5, user(1), Category::Personal);
        game.apply(user(2), Action::Join).unwrap();
        game.apply(user(1), Action::Begin).unwrap();
        let current = game.current_player().unwrap();
        let other = if current == user(1) {
            user(2)
        } else {
            user(1)
        };
        assert!(game.apply(other, Action::Truth).is_err());
        assert!(game.apply(current, Action::Truth).is_ok());
        assert!(matches!(
            &game.phase,
            Phase::Challenge {
                kind: Kind::Truth,
                ..
            }
        ));
        assert!(game.apply(other, Action::Complete).is_err());
        assert!(game.apply(current, Action::Complete).is_ok());
        assert_eq!(game.round, 2);
        assert_eq!(game.current_player(), Some(other));
        assert!(game.apply(other, Action::Dare).is_ok());
        assert!(game.apply(other, Action::Skip).is_ok());
        assert_eq!(game.current_player(), Some(current));
        assert_eq!(game.round, 3);
        assert!(game.apply(other, Action::Join).is_err());
    }

    #[test]
    fn activity_refreshes_and_sessions_expire() {
        let mut game = Game::new(99, user(1), Category::Casual);
        game.last_activity = std::time::Instant::now() - IDLE_TIMEOUT;
        assert!(game.is_expired());
        game.apply(user(2), Action::Join).unwrap();
        assert!(!game.is_expired());
    }

    #[test]
    fn host_may_skip_or_end_but_not_complete_someones_turn() {
        let mut game = Game::new(10, user(1), Category::Casual);
        game.apply(user(2), Action::Join).unwrap();
        game.apply(user(1), Action::Begin).unwrap();
        if game.current_player() == Some(user(1)) {
            game.apply(user(1), Action::Skip).unwrap();
        }
        assert_eq!(game.current_player(), Some(user(2)));
        assert!(game.apply(user(1), Action::Complete).is_err());
        assert!(game.apply(user(1), Action::Skip).is_ok());
        assert!(game.apply(user(2), Action::Stop).is_err());
        assert_eq!(game.apply(user(1), Action::Stop), Ok(Transition::Ended));
    }
}

#[cfg(test)]
mod ai_prompt_tests {
    use super::*;

    #[test]
    fn supplied_prompt_is_used_and_kept_across_turns() {
        let mut game = Game::new(77, UserId::new(1), Category::Personal);
        game.apply(UserId::new(2), Action::Join).unwrap();
        game.apply(UserId::new(1), Action::Begin).unwrap();
        let actor = game.current_player().unwrap();
        let text = "დაასახელე შენი საყვარელი ფილმი.";
        game.apply_with_prompt(actor, Action::Truth, Some(text.to_owned())).unwrap();
        assert_eq!(game.last_prompt(), Some(text));
        assert_eq!(game.phase, Phase::Challenge { kind: Kind::Truth, prompt: text.to_owned() });
        game.apply(actor, Action::Complete).unwrap();
        assert_eq!(game.last_prompt(), Some(text));
    }

    #[test]
    fn illegal_player_cannot_install_a_generated_prompt() {
        let mut game = Game::new(78, UserId::new(1), Category::Personal);
        game.apply(UserId::new(2), Action::Join).unwrap();
        game.apply(UserId::new(1), Action::Begin).unwrap();
        let actor = if game.current_player() == Some(UserId::new(1)) { UserId::new(2) } else { UserId::new(1) };
        assert!(game.apply_with_prompt(actor, Action::Truth, Some("invalid prompt".to_owned())).is_err());
        assert_eq!(game.last_prompt(), None);
        assert_eq!(game.phase, Phase::Choosing);
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;

    /// A two-player game on the current player's challenge, plus the other player.
    fn challenge(prompt: &str) -> (Game, UserId, UserId) {
        let mut game = Game::new(5, UserId::new(1), Category::Casual);
        game.apply(UserId::new(2), Action::Join).unwrap();
        game.apply(UserId::new(1), Action::Begin).unwrap();
        let current = game.current_player().unwrap();
        let other = if current == UserId::new(1) { UserId::new(2) } else { UserId::new(1) };
        game.apply_with_prompt(current, Action::Truth, Some(prompt.to_owned())).unwrap();
        (game, current, other)
    }

    #[test]
    fn completing_records_the_question_and_response_and_advances() {
        let (mut game, current, other) = challenge("What is your favorite word?");
        let done = game.complete_turn(current, Some("  Serendipity \n".to_owned())).unwrap();
        assert_eq!(done, CompletedTurn {
            player: current,
            round: 1,
            kind: Kind::Truth,
            prompt: "What is your favorite word?".to_owned(),
            response: Some("Serendipity".to_owned()),
        });
        assert_eq!(game.round, 2);
        assert_eq!(game.phase, Phase::Choosing);
        assert_eq!(game.current_player(), Some(other));
    }

    #[test]
    fn an_empty_response_is_allowed_for_spoken_dares() {
        let (mut game, current, _) = challenge("Sing a line.");
        let done = game.complete_turn(current, Some("   ".to_owned())).unwrap();
        assert_eq!(done.response, None);
        let (mut game2, current2, _) = challenge("Sing a line.");
        assert_eq!(game2.complete_turn(current2, None).unwrap().response, None);
        assert_eq!(game.round, 2);
    }

    #[test]
    fn only_the_current_player_can_complete_and_only_during_a_challenge() {
        let (mut game, current, other) = challenge("A question here.");
        assert_eq!(game.can_complete(other), Err("Only the current player can use this button."));
        assert!(game.complete_turn(other, Some("sneaky".to_owned())).is_err());
        // A rejected attempt changes nothing.
        assert_eq!(game.round, 1);
        assert!(matches!(game.phase, Phase::Challenge { .. }));
        assert_eq!(game.can_complete(current), Ok(()));

        game.complete_turn(current, None).unwrap();
        // The next player is still choosing: nothing to complete yet.
        assert_eq!(game.can_complete(other), Err("There isn't a challenge to complete yet."));
        assert_eq!(game.complete_turn(other, None), Err("There isn't a challenge to complete yet."));
    }

    #[test]
    fn long_responses_are_cut_and_blank_ones_dropped() {
        assert_eq!(normalize_response(""), None);
        assert_eq!(normalize_response(" \n\t "), None);
        let long = "ა".repeat(MAX_RESPONSE_CHARS + 50);
        assert_eq!(normalize_response(&long).unwrap().chars().count(), MAX_RESPONSE_CHARS);
    }

    #[test]
    fn response_ids_are_strict_and_name_the_round() {
        assert_eq!(parse_response_id(&response_id(42, 7)), Some((42, 7)));
        assert_eq!(parse_response_id("tod:42:response"), None);
        assert_eq!(parse_response_id("tod:42:response:7:extra"), None);
        assert_eq!(parse_response_id("tod:42:join:7"), None);
        assert_eq!(parse_response_id("other:42:response:7"), None);
        // Button IDs and response IDs never collide.
        assert_eq!(Action::parse(&response_id(42, 7)), None);
    }
}
