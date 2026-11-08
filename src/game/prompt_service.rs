//! Cached AI-generated prompts with an immediate curated fallback.
//!
//! Discord component responses must be sent promptly. We never wait for a
//! model response during a button press: the cache refills asynchronously.
//!
//! Unused prompts are saved to disk, so a restart neither loses prompts that
//! were already paid for nor requests new ones for categories nobody plays.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{Mutex, Semaphore};
use tracing::{info, warn};

use super::groq::Groq;
use super::store::{BucketData, Snapshot, Store, bucket_key};
use super::{Category, Kind, draw, random};
use crate::config::GroqConfig;

const REFILL_BELOW: usize = 3;
/// How many served prompts are remembered locally to reject duplicates.
const RECENT_LIMIT: usize = 24;
/// How many of those are sent to the model as "do not repeat" context. Sending
/// fewer keeps input tokens down; the larger local history still filters
/// duplicates, and the refill log reports how many were rejected.
const HISTORY_SENT: usize = 6;
const RETRY_DELAY: Duration = Duration::from_secs(90);

const CATEGORIES: [Category; 3] = [Category::Casual, Category::Personal, Category::Romantic];
const KINDS: [Kind; 2] = [Kind::Truth, Kind::Dare];

#[derive(Default)]
pub(crate) struct PromptService {
    ai: Option<Arc<Cache>>,
}

struct Cache {
    provider: Groq,
    language: String,
    store: Arc<Store>,
    /// Buckets for other languages, kept verbatim so that switching
    /// `TOD_LANGUAGE` never discards prompts that were already generated.
    foreign: BTreeMap<String, BucketData>,
    snapshots: AtomicU64,
    buckets: Mutex<HashMap<(Category, Kind), Bucket>>,
    // Keep free-tier usage predictable, including on first warmup.
    limit: Semaphore,
}

#[derive(Default)]
struct Bucket {
    ready: VecDeque<String>,
    recently_used: VecDeque<String>,
    loading: bool,
    retry_at: Option<Instant>,
}

/// A serialized snapshot waiting to be written by a blocking task.
struct Write {
    seq: u64,
    json: String,
}

impl Bucket {
    fn from_data(data: BucketData) -> Self {
        let mut recently_used: VecDeque<String> = data.recent.into_iter().collect();
        while recently_used.len() > RECENT_LIMIT {
            recently_used.pop_front();
        }
        Self {
            ready: data.ready.into_iter().filter(|prompt| !prompt.trim().is_empty()).collect(),
            recently_used,
            ..Self::default()
        }
    }

    fn to_data(&self) -> BucketData {
        BucketData {
            ready: self.ready.iter().cloned().collect(),
            recent: self.recently_used.iter().cloned().collect(),
        }
    }
}

impl PromptService {
    pub fn new(config: Option<GroqConfig>) -> Result<Self> {
        let ai = config
            .map(|config| Cache::open(config).map(Arc::new))
            .transpose()?;
        Ok(Self { ai })
    }

    /// Top up categories that were already being played, without delaying
    /// bot startup. Nothing is requested for categories nobody has used.
    pub fn warm(&self) {
        let Some(ai) = &self.ai else {
            info!("Groq is not configured; using curated Truth or Dare prompts");
            return;
        };
        let ai = Arc::clone(ai);
        tokio::spawn(async move { ai.warm().await });
    }

    pub async fn draw(&self, category: Category, kind: Kind, previous: Option<&str>) -> String {
        if let Some(ai) = &self.ai {
            if let Some(prompt) = ai.take(category, kind, previous).await {
                return prompt;
            }
        }
        draw(category, kind, previous).to_string()
    }
}

impl Cache {
    fn open(config: GroqConfig) -> Result<Self> {
        let language = config.language.clone();
        let store = Store::new(config.store_path.clone());
        let mut snapshot = store.load();

        let mut buckets = HashMap::new();
        let mut loaded = 0;
        for category in CATEGORIES {
            for kind in KINDS {
                if let Some(data) = snapshot.buckets.remove(&bucket_key(&language, category, kind)) {
                    let bucket = Bucket::from_data(data);
                    loaded += bucket.ready.len();
                    buckets.insert((category, kind), bucket);
                }
            }
        }
        info!(path = %store.path().display(), loaded, language = language.as_str(),
            "loaded saved AI prompts");

        Ok(Self {
            provider: Groq::new(config)?,
            language,
            store: Arc::new(store),
            foreign: snapshot.buckets,
            snapshots: AtomicU64::new(0),
            buckets: Mutex::new(buckets),
            limit: Semaphore::new(1),
        })
    }

    async fn warm(self: &Arc<Self>) {
        let played = {
            let buckets = self.buckets.lock().await;
            played_buckets(&buckets)
        };
        info!(played = played.len(), "Groq prompt generation enabled; refilling categories in play");
        for (category, kind) in played {
            self.ensure_refill(category, kind).await;
        }
    }

    async fn take(self: &Arc<Self>, category: Category, kind: Kind, previous: Option<&str>) -> Option<String> {
        let mut buckets = self.buckets.lock().await;
        let bucket = buckets.entry((category, kind)).or_default();
        // Randomize within the batch and avoid repeating the previous prompt.
        let available: Vec<usize> = bucket.ready.iter().enumerate()
            .filter(|(_, p)| previous != Some(p.as_str()))
            .map(|(index, _)| index)
            .collect();
        let prompt = if available.is_empty() {
            None
        } else {
            let index = available[random::index(available.len())];
            let prompt = bucket.ready.remove(index);
            if let Some(ref prompt) = prompt {
                remember(&mut bucket.recently_used, prompt);
            }
            prompt
        };
        let write = prompt.is_some().then(|| self.snapshot(&buckets)).flatten();
        drop(buckets);
        self.persist(write);
        self.ensure_refill(category, kind).await;
        prompt
    }

    async fn ensure_refill(self: &Arc<Self>, category: Category, kind: Kind) {
        let mut buckets = self.buckets.lock().await;
        let bucket = buckets.entry((category, kind)).or_default();
        if bucket.ready.len() >= REFILL_BELOW || bucket.loading
            || bucket.retry_at.is_some_and(|retry| Instant::now() < retry)
        {
            return;
        }
        bucket.loading = true;
        drop(buckets);
        let ai = Arc::clone(self);
        tokio::spawn(async move { ai.refill(category, kind).await });
    }

    async fn refill(self: Arc<Self>, category: Category, kind: Kind) {
        // The permit and network request never run while a cache mutex is held.
        let result: Result<Vec<String>> = async {
            let _permit = self.limit.acquire().await?;
            let recent: Vec<String> = {
                let buckets = self.buckets.lock().await;
                buckets.get(&(category, kind))
                    .map(|bucket| history_for_request(&bucket.recently_used))
                    .unwrap_or_default()
            };
            self.provider.generate(category, kind, &recent).await
        }.await;

        let mut buckets = self.buckets.lock().await;
        let bucket = buckets.entry((category, kind)).or_default();
        bucket.loading = false;
        let mut write = None;
        match result {
            Ok(prompts) => {
                let generated = prompts.len();
                let mut added = 0;
                for prompt in prompts {
                    if bucket.ready.iter().chain(bucket.recently_used.iter())
                        .any(|existing| existing.eq_ignore_ascii_case(&prompt)) {
                        continue;
                    }
                    bucket.ready.push_back(prompt);
                    added += 1;
                }
                let duplicates = generated - added;
                bucket.retry_at = if added == 0 { Some(Instant::now() + RETRY_DELAY) } else { None };
                info!(category = category.label(), kind = kind.label(), generated, added, duplicates,
                    "AI prompt cache refilled");
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

    /// Serialize the whole cache. Called with the bucket lock held so that the
    /// sequence number reflects the order in which states existed.
    fn snapshot(&self, buckets: &HashMap<(Category, Kind), Bucket>) -> Option<Write> {
        let mut snapshot = Snapshot::new();
        snapshot.buckets = self.foreign.clone();
        for (&(category, kind), bucket) in buckets {
            if bucket.ready.is_empty() && bucket.recently_used.is_empty() {
                continue;
            }
            snapshot.buckets.insert(bucket_key(&self.language, category, kind), bucket.to_data());
        }
        match snapshot.to_json() {
            Ok(json) => Some(Write { seq: self.snapshots.fetch_add(1, Ordering::Relaxed) + 1, json }),
            Err(error) => {
                warn!(error = ?error, "could not serialize the AI prompt cache");
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
                warn!(error = ?error, "could not save the AI prompt cache");
            }
        });
    }
}

/// Buckets somebody has already played: only these are worth refilling at startup.
fn played_buckets(buckets: &HashMap<(Category, Kind), Bucket>) -> Vec<(Category, Kind)> {
    buckets.iter()
        .filter(|(_, bucket)| !bucket.recently_used.is_empty())
        .map(|(&key, _)| key)
        .collect()
}

fn remember(recent: &mut VecDeque<String>, prompt: &str) {
    recent.push_back(prompt.to_owned());
    if recent.len() > RECENT_LIMIT {
        recent.pop_front();
    }
}

/// The newest `HISTORY_SENT` prompts, oldest first.
fn history_for_request(recent: &VecDeque<String>) -> Vec<String> {
    let skip = recent.len().saturating_sub(HISTORY_SENT);
    recent.iter().skip(skip).cloned().collect()
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::fs;

    use super::super::store::testing::temp_path;
    use super::{
        Bucket, Cache, Category, GroqConfig, HISTORY_SENT, Kind, RECENT_LIMIT, history_for_request,
        played_buckets, remember,
    };

    fn config(language: &str, name: &str) -> GroqConfig {
        GroqConfig {
            key: "test-key".into(),
            model: "test-model".into(),
            language: language.into(),
            store_path: temp_path(name),
        }
    }

    fn cleanup(config: &GroqConfig) {
        let _ = fs::remove_file(&config.store_path);
        let _ = fs::remove_file(format!("{}.tmp", config.store_path.display()));
    }

    #[tokio::test]
    async fn without_key_uses_curated_prompts() {
        let service = super::PromptService::new(None).unwrap();
        service.warm();
        let prompt = service.draw(super::Category::Casual, super::Kind::Truth, None).await;
        assert!(!prompt.is_empty());
    }

    #[test]
    fn recent_prompts_are_bounded() {
        let mut queue = VecDeque::new();
        for index in 0..(RECENT_LIMIT + 9) {
            remember(&mut queue, &index.to_string());
        }
        assert_eq!(queue.len(), RECENT_LIMIT);
        assert_eq!(queue.front().map(String::as_str), Some("9"));
    }

    #[test]
    fn request_history_is_the_newest_few_in_order() {
        let mut queue = VecDeque::new();
        for index in 0..RECENT_LIMIT {
            remember(&mut queue, &index.to_string());
        }
        let sent = history_for_request(&queue);
        assert_eq!(sent.len(), HISTORY_SENT);
        assert_eq!(sent.first().map(String::as_str), Some((RECENT_LIMIT - HISTORY_SENT).to_string().as_str()));
        assert_eq!(sent.last().map(String::as_str), Some((RECENT_LIMIT - 1).to_string().as_str()));
        // A short history is sent in full.
        let short: VecDeque<String> = ["a".to_owned(), "b".to_owned()].into();
        assert_eq!(history_for_request(&short), ["a", "b"]);
    }

    #[test]
    fn only_played_buckets_are_warmed() {
        let mut buckets = HashMap::new();
        let mut played = Bucket::default();
        played.recently_used.push_back("served once".into());
        buckets.insert((Category::Casual, Kind::Truth), played);
        // Prompts that were generated but never served do not justify a request.
        let mut stocked = Bucket::default();
        stocked.ready.push_back("never served".into());
        buckets.insert((Category::Romantic, Kind::Dare), stocked);
        buckets.insert((Category::Personal, Kind::Truth), Bucket::default());
        assert_eq!(played_buckets(&buckets), [(Category::Casual, Kind::Truth)]);
    }

    #[tokio::test]
    async fn cache_survives_a_restart_and_keeps_other_languages() {
        let georgian = config("ka", "restart-ka");
        {
            let cache = Cache::open(config("ka", "restart-ka")).unwrap();
            let mut buckets = cache.buckets.lock().await;
            let bucket = buckets.entry((Category::Casual, Kind::Truth)).or_default();
            // Enough stock that serving one prompt starts no network refill.
            for index in 0..5 {
                bucket.ready.push_back(format!("ready {index}"));
            }
            bucket.recently_used.push_back("served".into());
            let write = cache.snapshot(&buckets).unwrap();
            drop(buckets);
            cache.store.save(write.seq, &write.json).unwrap();
        }

        // Same file, same language: everything is restored.
        let cache = Cache::open(config("ka", "restart-ka")).unwrap();
        {
            let buckets = cache.buckets.lock().await;
            let bucket = &buckets[&(Category::Casual, Kind::Truth)];
            assert_eq!(bucket.ready.len(), 5);
            assert_eq!(bucket.recently_used, ["served"]);
        }

        // Another language starts empty, and saving does not discard Georgian prompts.
        let english = Cache::open(config("en", "restart-ka")).unwrap();
        {
            let buckets = english.buckets.lock().await;
            assert!(buckets.is_empty());
            let write = english.snapshot(&buckets).unwrap();
            english.store.save(write.seq, &write.json).unwrap();
        }
        let again = Cache::open(config("ka", "restart-ka")).unwrap();
        assert_eq!(again.buckets.lock().await[&(Category::Casual, Kind::Truth)].ready.len(), 5);

        cleanup(&georgian);
    }
}
