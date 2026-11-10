//! AI-generated prompts with an immediate curated fallback.
//!
//! Discord component responses must be sent promptly. We never wait for a
//! model response during a button press: the library refills asynchronously.
//!
//! Generated prompts are kept in a library, saved to disk, and tracked per
//! channel. A prompt that one channel has used is still available to every
//! other channel, so a request to the model is only made when a channel that
//! is actually playing is about to run out of prompts it has not seen.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::generator::PromptGenerator;
use super::library::Library;
use super::store::{BucketData, Snapshot, Store, bucket_key};
use super::{Category, Kind, draw, random};
use crate::ai::AiClient;
use crate::config::TodConfig;

/// Request more prompts when a channel has fewer than this many left to see.
const REFILL_BELOW: usize = 3;
/// How many of the newest library prompts are sent to the model as "do not
/// repeat" context. Sending fewer keeps input tokens down; the whole library
/// still filters duplicates locally, and the refill log reports how many were
/// rejected.
const HISTORY_SENT: usize = 6;
const RETRY_DELAY: Duration = Duration::from_secs(90);

const CATEGORIES: [Category; 3] = [Category::Casual, Category::Personal, Category::Romantic];
const KINDS: [Kind; 2] = [Kind::Truth, Kind::Dare];

#[derive(Default)]
pub(crate) struct PromptService {
    ai: Option<Arc<Cache>>,
}

struct Cache {
    generator: PromptGenerator,
    language: String,
    store: Arc<Store>,
    /// Buckets for other languages, kept verbatim so that switching
    /// `TOD_LANGUAGE` never discards prompts that were already generated.
    foreign: BTreeMap<String, BucketData>,
    snapshots: AtomicU64,
    buckets: Mutex<HashMap<(Category, Kind), Bucket>>,
}

#[derive(Default)]
struct Bucket {
    library: Library,
    loading: bool,
    retry_at: Option<Instant>,
}

/// A serialized snapshot waiting to be written by a blocking task.
struct Write {
    seq: u64,
    json: String,
}

impl PromptService {
    /// Without a client, only the built-in prompts are used.
    pub fn new(ai: Option<Arc<AiClient>>, tod: TodConfig) -> Result<Self> {
        let ai = ai
            .map(|ai| Cache::open(ai, tod).map(Arc::new))
            .transpose()?;
        Ok(Self { ai })
    }

    /// Top up libraries that a channel is close to exhausting, without delaying
    /// bot startup. Nothing is requested for categories nobody has played.
    pub fn warm(&self) {
        let Some(ai) = &self.ai else {
            info!("Groq is not configured; using curated Truth or Dare prompts");
            return;
        };
        let ai = Arc::clone(ai);
        tokio::spawn(async move { ai.warm().await });
    }

    /// `channel` is the Discord channel ID: prompts are not repeated within a
    /// channel until its library is exhausted.
    pub async fn draw(&self, channel: u64, category: Category, kind: Kind, previous: Option<&str>) -> String {
        if let Some(ai) = &self.ai {
            if let Some(prompt) = ai.take(channel, category, kind, previous).await {
                return prompt;
            }
        }
        draw(category, kind, previous).to_string()
    }
}

impl Cache {
    fn open(ai: Arc<AiClient>, tod: TodConfig) -> Result<Self> {
        let TodConfig { language, store_path } = tod;
        let store = Store::new(store_path);
        let mut snapshot = store.load();

        let mut buckets = HashMap::new();
        let mut loaded = 0;
        for category in CATEGORIES {
            for kind in KINDS {
                if let Some(data) = snapshot.buckets.remove(&bucket_key(&language, category, kind)) {
                    loaded += data.entries.len();
                    buckets.insert(
                        (category, kind),
                        Bucket { library: Library::from_data(data), ..Bucket::default() },
                    );
                }
            }
        }
        info!(path = %store.path().display(), loaded, language = language.as_str(),
            "loaded saved AI prompt library");

        Ok(Self {
            generator: PromptGenerator::new(ai, language.clone()),
            language,
            store: Arc::new(store),
            foreign: snapshot.buckets,
            snapshots: AtomicU64::new(0),
            buckets: Mutex::new(buckets),
        })
    }

    async fn warm(self: &Arc<Self>) {
        let wanted = {
            let buckets = self.buckets.lock().await;
            refill_candidates(&buckets)
        };
        info!(wanted = wanted.len(), "Groq prompt generation enabled; refilling libraries in need");
        for (category, kind) in wanted {
            self.start_refill(category, kind).await;
        }
    }

    async fn take(
        self: &Arc<Self>,
        channel: u64,
        category: Category,
        kind: Kind,
        previous: Option<&str>,
    ) -> Option<String> {
        let mut buckets = self.buckets.lock().await;
        let bucket = buckets.entry((category, kind)).or_default();
        // Randomize among prompts this channel has not seen and never repeat
        // the previous one.
        let prompt = bucket.library.take(channel, previous, random::index);
        let low = bucket.library.unseen(channel) < REFILL_BELOW;
        let write = prompt.is_some().then(|| self.snapshot(&buckets)).flatten();
        drop(buckets);
        self.persist(write);
        if low {
            self.start_refill(category, kind).await;
        }
        prompt
    }

    /// Start a background refill unless one is running or recently failed.
    async fn start_refill(self: &Arc<Self>, category: Category, kind: Kind) {
        let mut buckets = self.buckets.lock().await;
        let bucket = buckets.entry((category, kind)).or_default();
        if bucket.loading || bucket.retry_at.is_some_and(|retry| Instant::now() < retry) {
            return;
        }
        bucket.loading = true;
        drop(buckets);
        let ai = Arc::clone(self);
        tokio::spawn(async move { ai.refill(category, kind).await });
    }

    async fn refill(self: Arc<Self>, category: Category, kind: Kind) {
        // The network request never runs while a cache mutex is held.
        let result: Result<Vec<String>> = async {
            let recent: Vec<String> = {
                let buckets = self.buckets.lock().await;
                buckets.get(&(category, kind))
                    .map(|bucket| bucket.library.newest(HISTORY_SENT))
                    .unwrap_or_default()
            };
            self.generator.generate(category, kind, &recent).await
        }.await;

        let mut buckets = self.buckets.lock().await;
        let bucket = buckets.entry((category, kind)).or_default();
        bucket.loading = false;
        let mut write = None;
        match result {
            Ok(prompts) => {
                let generated = prompts.len();
                let added = bucket.library.add(prompts);
                let duplicates = generated - added;
                bucket.retry_at = if added == 0 { Some(Instant::now() + RETRY_DELAY) } else { None };
                info!(category = category.label(), kind = kind.label(), generated, added, duplicates,
                    "AI prompt library refilled");
                if added > 0 {
                    write = self.snapshot(&buckets);
                }
            }
            Err(error) => {
                bucket.retry_at = Some(Instant::now() + RETRY_DELAY);
                warn!(category = category.label(), kind = kind.label(), error = ?error,
                    "Groq refill failed; curated prompts remain available");
            }
        }
        drop(buckets);
        self.persist(write);
    }

    /// Serialize the whole library. Called with the bucket lock held so that
    /// the sequence number reflects the order in which states existed.
    fn snapshot(&self, buckets: &HashMap<(Category, Kind), Bucket>) -> Option<Write> {
        let mut snapshot = Snapshot::new();
        snapshot.buckets = self.foreign.clone();
        for (&(category, kind), bucket) in buckets {
            if bucket.library.is_empty() {
                continue;
            }
            snapshot.buckets.insert(bucket_key(&self.language, category, kind), bucket.library.to_data());
        }
        match snapshot.to_json() {
            Ok(json) => Some(Write { seq: self.snapshots.fetch_add(1, Ordering::Relaxed) + 1, json }),
            Err(error) => {
                warn!(error = ?error, "could not serialize the AI prompt library");
                None
            }
        }
    }

    /// Write on a blocking thread: a button press never waits for the disk.
    fn persist(&self, write: Option<Write>) {
        let Some(Write { seq, json }) = write else { return };
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            if let Err(error) = store.save(seq, &json) {
                warn!(error = ?error, "could not save the AI prompt library");
            }
        });
    }
}

/// Libraries where a channel that already plays is close to running out.
fn refill_candidates(buckets: &HashMap<(Category, Kind), Bucket>) -> Vec<(Category, Kind)> {
    buckets.iter()
        .filter(|(_, bucket)| bucket.library.needs_refill(REFILL_BELOW))
        .map(|(&key, _)| key)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;

    use super::super::store::testing::temp_path;
    use super::{AiClient, Arc, Bucket, Cache, Category, Kind, REFILL_BELOW, TodConfig, refill_candidates};
    use crate::config::AiConfig;

    fn ai() -> Arc<AiClient> {
        let config = AiConfig { key: "test-key".into(), model: "test-model".into() };
        Arc::new(AiClient::new(config).unwrap())
    }

    fn tod(language: &str, name: &str) -> TodConfig {
        TodConfig { language: language.into(), store_path: temp_path(name) }
    }

    #[tokio::test]
    async fn without_key_uses_curated_prompts() {
        let service = super::PromptService::new(None, tod("ka", "no-key")).unwrap();
        service.warm();
        let prompt = service.draw(1, super::Category::Casual, super::Kind::Truth, None).await;
        assert!(!prompt.is_empty());
    }

    #[test]
    fn only_libraries_a_channel_is_exhausting_are_refilled() {
        let texts = ["alpha one", "bravo two", "charlie three", "delta four", "echo five"];
        let mut buckets = HashMap::new();

        // Played, and one channel has only two prompts left: refill.
        let mut busy = Bucket::default();
        busy.library.add(texts.map(String::from));
        for _ in 0..(texts.len() - REFILL_BELOW + 1) {
            busy.library.take(1, None, |_| 0);
        }
        buckets.insert((Category::Casual, Kind::Truth), busy);

        // Stocked but never played: no request just because it exists.
        let mut stocked = Bucket::default();
        stocked.library.add(texts.map(String::from));
        buckets.insert((Category::Romantic, Kind::Dare), stocked);

        // Played lightly: plenty left for everyone who used it.
        let mut light = Bucket::default();
        light.library.add(texts.map(String::from));
        light.library.take(1, None, |_| 0);
        buckets.insert((Category::Personal, Kind::Truth), light);

        buckets.insert((Category::Personal, Kind::Dare), Bucket::default());
        assert_eq!(refill_candidates(&buckets), [(Category::Casual, Kind::Truth)]);
    }

    #[tokio::test]
    async fn library_and_seen_state_survive_a_restart_and_keep_other_languages() {
        let path = tod("ka", "restart").store_path;
        {
            let cache = Cache::open(ai(), tod("ka", "restart")).unwrap();
            let mut buckets = cache.buckets.lock().await;
            let bucket = buckets.entry((Category::Casual, Kind::Truth)).or_default();
            bucket.library.add((0..5).map(|index| format!("ready prompt {index}")));
            // Serve one prompt directly from the library (no network refill).
            bucket.library.take(77, None, |_| 0);
            let write = cache.snapshot(&buckets).unwrap();
            drop(buckets);
            cache.store.save(write.seq, &write.json).unwrap();
        }

        // Same file and language: prompts and what channel 77 has seen are restored.
        let cache = Cache::open(ai(), tod("ka", "restart")).unwrap();
        {
            let buckets = cache.buckets.lock().await;
            let library = &buckets[&(Category::Casual, Kind::Truth)].library;
            assert_eq!(library.unseen(77), 4);
            assert_eq!(library.unseen(78), 5);
        }

        // Another language starts empty, and saving does not discard Georgian prompts.
        let english = Cache::open(ai(), tod("en", "restart")).unwrap();
        {
            let buckets = english.buckets.lock().await;
            assert!(buckets.is_empty());
            let write = english.snapshot(&buckets).unwrap();
            english.store.save(write.seq, &write.json).unwrap();
        }
        let again = Cache::open(ai(), tod("ka", "restart")).unwrap();
        assert_eq!(again.buckets.lock().await[&(Category::Casual, Kind::Truth)].library.unseen(77), 4);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(format!("{}.tmp", path.display()));
    }
}
