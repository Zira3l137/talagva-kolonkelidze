//! A growing, de-duplicated library of generated prompts for one language,
//! category and kind, remembering which channels have already seen each one.
//!
//! The logic here is pure (no I/O, no clock, no global randomness) so the
//! reuse rules can be tested without a network or a Discord connection.

use std::collections::HashSet;

use super::store::{BucketData, EntryData};

/// The oldest prompts are dropped beyond this, bounding file size and scan cost.
pub(super) const LIBRARY_LIMIT: usize = 500;

#[derive(Default)]
pub(super) struct Library {
    entries: Vec<Entry>,
}

struct Entry {
    text: String,
    /// Normalized text used to reject near-identical prompts.
    fingerprint: String,
    seen_by: HashSet<u64>,
}

impl Library {
    pub fn from_data(data: BucketData) -> Self {
        let mut library = Self::default();
        for entry in data.entries {
            library.insert(&entry.text, entry.seen_by.into_iter().collect());
        }
        library.trim();
        library
    }

    pub fn to_data(&self) -> BucketData {
        BucketData {
            entries: self.entries.iter()
                .map(|entry| {
                    let mut seen_by: Vec<u64> = entry.seen_by.iter().copied().collect();
                    seen_by.sort_unstable();
                    EntryData { text: entry.text.clone(), seen_by }
                })
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Add new prompts, skipping any already in the library. Returns how many were added.
    pub fn add(&mut self, prompts: impl IntoIterator<Item = String>) -> usize {
        let added = prompts.into_iter()
            .filter(|prompt| self.insert(prompt, HashSet::new()))
            .count();
        self.trim();
        added
    }

    /// Pick a prompt this channel has not seen yet and mark it as seen.
    /// `pick` receives the number of candidates and returns an index below it.
    pub fn take(
        &mut self,
        channel: u64,
        previous: Option<&str>,
        pick: impl FnOnce(usize) -> usize,
    ) -> Option<String> {
        let candidates: Vec<usize> = self.entries.iter().enumerate()
            .filter(|(_, entry)| !entry.seen_by.contains(&channel) && previous != Some(entry.text.as_str()))
            .map(|(index, _)| index)
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let entry = &mut self.entries[candidates[pick(candidates.len())]];
        entry.seen_by.insert(channel);
        Some(entry.text.clone())
    }

    /// How many prompts this channel has not seen.
    pub fn unseen(&self, channel: u64) -> usize {
        self.entries.iter().filter(|entry| !entry.seen_by.contains(&channel)).count()
    }

    /// True when a channel that has used this library has fewer than
    /// `threshold` prompts left to see.
    pub fn needs_refill(&self, threshold: usize) -> bool {
        let channels: HashSet<u64> = self.entries.iter()
            .flat_map(|entry| entry.seen_by.iter().copied())
            .collect();
        channels.into_iter().any(|channel| self.unseen(channel) < threshold)
    }

    /// The most recently added prompts, oldest first. These are what the model
    /// is most likely to repeat, so they are what it is asked to avoid.
    pub fn newest(&self, count: usize) -> Vec<String> {
        let skip = self.entries.len().saturating_sub(count);
        self.entries.iter().skip(skip).map(|entry| entry.text.clone()).collect()
    }

    fn insert(&mut self, text: &str, seen_by: HashSet<u64>) -> bool {
        let text = text.trim();
        let fingerprint = fingerprint(text);
        if fingerprint.is_empty() || self.entries.iter().any(|entry| entry.fingerprint == fingerprint) {
            return false;
        }
        self.entries.push(Entry { text: text.to_owned(), fingerprint, seen_by });
        true
    }

    fn trim(&mut self) {
        let excess = self.entries.len().saturating_sub(LIBRARY_LIMIT);
        self.entries.drain(..excess);
    }
}

/// Case, spacing and punctuation differences do not make a prompt new.
fn fingerprint(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{LIBRARY_LIMIT, Library, fingerprint};

    fn library(texts: &[&str]) -> Library {
        let mut library = Library::default();
        library.add(texts.iter().map(|text| (*text).to_owned()));
        library
    }

    #[test]
    fn a_prompt_is_kept_and_reused_by_other_channels() {
        let mut library = library(&["only prompt"]);
        assert_eq!(library.take(1, None, |_| 0).as_deref(), Some("only prompt"));
        // Channel 1 has seen everything; channel 2 still gets the same prompt.
        assert_eq!(library.take(1, None, |_| 0), None);
        assert_eq!(library.take(2, None, |_| 0).as_deref(), Some("only prompt"));
        assert_eq!(library.unseen(1), 0);
        assert_eq!(library.unseen(3), 1);
    }

    #[test]
    fn a_channel_never_sees_a_repeat_until_the_library_is_exhausted() {
        let mut library = library(&["a one", "b two", "c three", "d four"]);
        let mut served: Vec<String> = (0..4).filter_map(|_| library.take(7, None, |_| 0)).collect();
        served.sort();
        assert_eq!(served, ["a one", "b two", "c three", "d four"]);
        assert_eq!(library.take(7, None, |_| 0), None);
    }

    #[test]
    fn the_previous_prompt_is_skipped_even_if_unseen() {
        let mut library = library(&["first prompt", "second prompt"]);
        assert_eq!(library.take(1, Some("first prompt"), |_| 0).as_deref(), Some("second prompt"));
    }

    #[test]
    fn near_identical_prompts_are_rejected() {
        let mut library = library(&["What's your favorite villain?"]);
        let added = library.add([
            "what's your favorite villain".to_owned(),
            "  WHAT'S YOUR FAVORITE   VILLAIN?! ".to_owned(),
            "Who is your favorite hero?".to_owned(),
            "...".to_owned(),
        ]);
        assert_eq!(added, 1);
        assert_eq!(fingerprint("ქართული, ტექსტი!"), "ქართულიტექსტი");
    }

    #[test]
    fn refill_is_needed_only_when_a_channel_runs_low() {
        let mut library = library(&["p one", "p two", "p three", "p four", "p five"]);
        // Nobody has used it yet: nothing to refill for.
        assert!(!library.needs_refill(3));
        library.take(1, None, |_| 0);
        library.take(1, None, |_| 0);
        assert!(!library.needs_refill(3)); // 3 unseen left
        library.take(1, None, |_| 0);
        assert!(library.needs_refill(3)); // 2 unseen left
        // New prompts help every channel at once.
        library.add(["p six".to_owned(), "p seven".to_owned()]);
        assert!(!library.needs_refill(3));
    }

    #[test]
    fn newest_returns_the_latest_in_order() {
        let library = library(&["p1 a", "p2 b", "p3 c", "p4 d"]);
        assert_eq!(library.newest(2), ["p3 c", "p4 d"]);
        assert_eq!(library.newest(10).len(), 4);
    }

    #[test]
    fn oldest_prompts_are_dropped_beyond_the_limit() {
        let mut library = Library::default();
        library.add((0..LIBRARY_LIMIT + 5).map(|index| format!("prompt number {index}")));
        let data = library.to_data();
        assert_eq!(data.entries.len(), LIBRARY_LIMIT);
        assert_eq!(data.entries[0].text, "prompt number 5");
    }

    #[test]
    fn seen_tracking_survives_serialization() {
        let mut original = library(&["alpha beta", "gamma delta"]);
        original.take(9, None, |_| 0);
        let restored = Library::from_data(original.to_data());
        assert_eq!(restored.unseen(9), 1);
        assert_eq!(restored.unseen(10), 2);
        assert_eq!(restored.to_data(), original.to_data());
    }
}
