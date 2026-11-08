//! Truth or Dare rules, prompts, and Discord integration.

mod groq;
mod prompts;
mod random;
mod state;

pub(crate) mod discord;

pub(crate) use prompts::{Category, Kind, draw};
pub(crate) use state::{Action, Game, Phase, Transition};
