//! Truth or Dare rules, prompts, and Discord integration.

mod generator;
mod library;
mod prompt_service;
mod prompts;
mod random;
mod state;
mod store;

pub(crate) mod discord;

pub(crate) use prompts::{Category, Kind, draw};
pub(crate) use state::{Action, Game, Phase, Transition};

pub(crate) use prompt_service::PromptService;
