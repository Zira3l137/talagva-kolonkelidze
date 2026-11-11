//! Persistent movie suggestions, kept separately for every Discord channel.
//!
//! The caller serializes operations through a Tokio mutex. This also lets
//! `/moviepoll` hold the queue steady until Discord confirms the poll was sent.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_MOVIES: usize = 10; // Discord's native poll limit.
const MAX_ANSWER_CHARS: usize = 55;
const STORE_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Movie {
    pub name: String,
    pub year: u16,
    pub genre: String,
}

impl Movie {
    pub fn new(name: String, year: u16, genre: String) -> Result<Self> {
        let movie = Self { name: name.trim().into(), year, genre: genre.trim().into() };
        ensure!(!movie.name.is_empty(), "Movie name cannot be empty.");
        ensure!(!movie.genre.is_empty(), "Movie genre cannot be empty.");
        ensure!(
            !movie.name.chars().any(char::is_control)
                && !movie.genre.chars().any(char::is_control),
            "Movie name and genre cannot contain line breaks or control characters."
        );
        ensure!((1888..=2200).contains(&movie.year), "Movie year must be between 1888 and 2200.");
        ensure!(
            movie.label().chars().count() <= MAX_ANSWER_CHARS,
            "Name, year and genre together must fit within 55 characters."
        );
        Ok(movie)
    }

    /// Keep all three fields in the answer so the choice is unambiguous.
    pub fn label(&self) -> String {
        format!("{} ({}) · {}", self.name, self.year, self.genre)
    }

    fn matches(&self, other: &Self) -> bool {
        self.year == other.year
            && self.name.to_lowercase() == other.name.to_lowercase()
            && self.genre.to_lowercase() == other.genre.to_lowercase()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Snapshot {
    version: u32,
    channels: BTreeMap<u64, Vec<Movie>>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self { version: STORE_VERSION, channels: BTreeMap::new() }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum AddOutcome {
    Added,
    Duplicate,
    Full,
}

pub(crate) struct MovieStore {
    path: PathBuf,
    snapshot: Snapshot,
}

impl MovieStore {
    pub fn open(path: PathBuf) -> Result<Self> {
        let snapshot = match fs::read_to_string(&path) {
            Ok(json) => {
                let snapshot: Snapshot = serde_json::from_str(&json)
                    .with_context(|| format!("invalid movie store at {}", path.display()))?;
                ensure!(
                    snapshot.version == STORE_VERSION,
                    "unsupported movie store version {} at {}",
                    snapshot.version,
                    path.display()
                );
                snapshot
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Snapshot::default(),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path, snapshot })
    }

    pub fn for_channel(&self, channel_id: u64) -> Vec<Movie> {
        self.snapshot.channels.get(&channel_id).cloned().unwrap_or_default()
    }

    pub fn add(&mut self, channel_id: u64, movie: Movie) -> Result<AddOutcome> {
        let current = self.for_channel(channel_id);
        if current.iter().any(|candidate| candidate.matches(&movie)) {
            return Ok(AddOutcome::Duplicate);
        }
        if current.len() >= MAX_MOVIES {
            return Ok(AddOutcome::Full);
        }
        let mut next = self.snapshot.clone();
        next.channels.entry(channel_id).or_default().push(movie);
        self.commit(next)?;
        Ok(AddOutcome::Added)
    }

    pub fn remove(&mut self, channel_id: u64, movie: &Movie) -> Result<bool> {
        let mut next = self.snapshot.clone();
        let Some(movies) = next.channels.get_mut(&channel_id) else {
            return Ok(false);
        };
        let Some(index) = movies.iter().position(|candidate| candidate.matches(movie)) else {
            return Ok(false);
        };
        movies.remove(index);
        if movies.is_empty() {
            next.channels.remove(&channel_id);
        }
        self.commit(next)?;
        Ok(true)
    }

    /// Call only after Discord has accepted the poll, while holding the mutex.
    pub fn clear(&mut self, channel_id: u64) -> Result<()> {
        if !self.snapshot.channels.contains_key(&channel_id) {
            return Ok(());
        }
        let mut next = self.snapshot.clone();
        next.channels.remove(&channel_id);
        self.commit(next)
    }

    /// Write before updating memory so disk errors never silently discard data.
    fn commit(&mut self, next: Snapshot) -> Result<()> {
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).context("creating movie store directory")?;
        }
        let json = serde_json::to_vec_pretty(&next).context("serializing movie store")?;
        let mut temp_path = self.path.as_os_str().to_os_string();
        temp_path.push(".tmp");
        let temp_path = PathBuf::from(temp_path);
        fs::write(&temp_path, json).context("writing temporary movie store")?;
        fs::rename(&temp_path, &self.path).context("replacing movie store")?;
        self.snapshot = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn example(name: &str, year: u16, genre: &str) -> Movie {
        Movie::new(name.into(), year, genre.into()).unwrap()
    }

    fn store() -> MovieStore {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "talagva-movies-{}-{id}.json", std::process::id()
        ));
        MovieStore::open(path).unwrap()
    }

    #[test]
    fn movie_validation_and_labels() {
        assert_eq!(example(" Alien ", 1979, " Sci-Fi ").label(), "Alien (1979) · Sci-Fi");
        assert!(Movie::new(" ".into(), 2000, "Drama".into()).is_err());
        assert!(Movie::new("Alien".into(), 2000, " ".into()).is_err());
        assert!(Movie::new("Alien".into(), 0, "Drama".into()).is_err());
        assert!(Movie::new("Alien\nNext".into(), 1979, "Drama".into()).is_err());
        assert!(Movie::new("x".repeat(55), 2000, "Drama".into()).is_err());
    }

    #[test]
    fn suggestions_are_per_channel_and_survive_restart() {
        let mut store = store();
        let path = store.path.clone();
        let movie = example("Alien", 1979, "Sci-Fi");
        assert_eq!(store.add(1, movie.clone()).unwrap(), AddOutcome::Added);
        assert_eq!(
            store.add(1, example("alien", 1979, "sci-fi")).unwrap(),
            AddOutcome::Duplicate
        );
        assert_eq!(store.for_channel(2).len(), 0);
        assert_eq!(store.add(2, movie.clone()).unwrap(), AddOutcome::Added);
        drop(store);
        let mut reopened = MovieStore::open(path.clone()).unwrap();
        assert_eq!(reopened.for_channel(1), vec![movie.clone()]);
        assert!(!reopened.remove(1, &example("Alien", 1980, "Sci-Fi")).unwrap());
        assert!(reopened.remove(1, &example("ALIEN", 1979, "SCI-FI")).unwrap());
        assert!(reopened.for_channel(1).is_empty());
        reopened.clear(2).unwrap();
        assert!(reopened.for_channel(2).is_empty());
        drop(reopened);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn ten_choices_maximum() {
        let mut store = store();
        let path = store.path.clone();
        for number in 0..MAX_MOVIES {
            assert_eq!(
                store.add(10, example(&format!("Movie {number}"), 2000, "Drama")).unwrap(),
                AddOutcome::Added
            );
        }
        assert_eq!(
            store.add(10, example("Extra", 2000, "Drama")).unwrap(),
            AddOutcome::Full
        );
        drop(store);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn unreadable_json_does_not_get_overwritten() {
        let store = store();
        let path = store.path.clone();
        fs::write(&path, "{invalid-json").unwrap();
        assert!(MovieStore::open(path.clone()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{invalid-json");
        fs::remove_file(path).unwrap();
    }
}
