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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Lobby,
    Choosing,
    Challenge { kind: Kind, prompt: &'static str },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Transition {
    Updated,
    Ended,
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
    last_prompt: Option<&'static str>,
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
        (!matches!(self.phase, Phase::Lobby)).then(|| self.players[self.current])
    }

    pub fn is_expired(&self) -> bool {
        self.last_activity.elapsed() >= IDLE_TIMEOUT
    }

    pub fn apply(&mut self, actor: UserId, action: Action) -> Result<Transition, &'static str> {
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
                let kind = if action == A::Truth { Kind::Truth } else { Kind::Dare };
                let prompt = draw(self.category, kind, self.last_prompt);
                self.last_prompt = Some(prompt);
                self.phase = Phase::Challenge { kind, prompt };
                Transition::Updated
            }
            A::Complete => {
                self.require_turn(actor)?;
                if !matches!(self.phase, Phase::Challenge { .. }) {
                    return Err("There isn't a challenge to complete yet.");
                }
                self.advance();
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
        for action in [Action::Join, Action::Leave, Action::Begin, Action::Truth, Action::Dare,
                       Action::Complete, Action::Skip, Action::Stop] {
            assert_eq!(Action::parse(&action.id(42)), Some((42, action)));
        }
        assert_eq!(Action::parse("tod:42:unknown"), None);
        assert_eq!(Action::parse("tod:42:join:extra"), None);
        assert_eq!(Action::parse("other:42:join"), None);
    }

    #[test]
    fn lobby_permissions_and_limits() {
        let mut game = Game::new(1, user(1), Category::Casual);
        assert_eq!(game.apply(user(1), Action::Begin), Err("At least two players must join before starting."));
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
        let other = if current == user(1) { user(2) } else { user(1) };
        assert!(game.apply(other, Action::Truth).is_err());
        assert!(game.apply(current, Action::Truth).is_ok());
        assert!(matches!(game.phase, Phase::Challenge { kind: Kind::Truth, .. }));
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
