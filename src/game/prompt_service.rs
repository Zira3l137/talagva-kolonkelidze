//! Cached AI-generated prompts with an immediate curated fallback.
//!
//! Discord component responses must be sent promptly. We never wait for a
//! model response during a button press: the cache refills asynchronously.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{Mutex, Semaphore};
use tracing::{info, warn};

use super::groq::Groq;
use super::{Category, Kind, draw, random};
use crate::config::GroqConfig;

const REFILL_BELOW: usize = 3;
const RECENT_LIMIT: usize = 24;
const RETRY_DELAY: Duration = Duration::from_secs(90);

#[derive(Default)]
pub(crate) struct PromptService {
    ai: Option<Arc<Cache>>,
}

struct Cache {
    provider: Groq,
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

impl PromptService {
    pub fn new(config: Option<GroqConfig>) -> Result<Self> {
        let ai = config
            .map(|config| -> Result<Arc<Cache>> {
                Ok(Arc::new(Cache {
                    provider: Groq::new(config)?,
                    buckets: Mutex::new(HashMap::new()),
                    limit: Semaphore::new(1),
                }))
            })
            .transpose()?;
        Ok(Self { ai })
    }

    /// Warm every combination in the background without delaying bot startup.
    pub fn warm(&self) {
        let Some(ai) = &self.ai else {
            info!("Groq is not configured; using curated Truth or Dare prompts");
            return;
        };
        info!("Groq prompt generation enabled; starting background cache warmup");
        for category in [Category::Casual, Category::Personal, Category::Romantic] {
            for kind in [Kind::Truth, Kind::Dare] {
                let ai = Arc::clone(ai);
                tokio::spawn(async move { ai.ensure_refill(category, kind).await });
            }
        }
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
        drop(buckets);
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
                    .map(|bucket| bucket.recently_used.iter().cloned().collect())
                    .unwrap_or_default()
            };
            self.provider.generate(category, kind, &recent).await
        }.await;

        let mut buckets = self.buckets.lock().await;
        let bucket = buckets.entry((category, kind)).or_default();
        bucket.loading = false;
        match result {
            Ok(prompts) => {
                let mut added = 0;
                for prompt in prompts {
                    if bucket.ready.iter().chain(bucket.recently_used.iter())
                        .any(|existing| existing.eq_ignore_ascii_case(&prompt)) {
                        continue;
                    }
                    bucket.ready.push_back(prompt);
                    added += 1;
                }
                bucket.retry_at = if added == 0 { Some(Instant::now() + RETRY_DELAY) } else { None };
                info!(category = category.label(), kind = kind.label(), added, "AI prompt cache refilled");
            }
            Err(error) => {
                bucket.retry_at = Some(Instant::now() + RETRY_DELAY);
                warn!(category = category.label(), kind = kind.label(), error = ?error,
                    "Groq refill failed; curated prompts remain available");
            }
        }
    }
}

fn remember(recent: &mut VecDeque<String>, prompt: &str) {
    recent.push_back(prompt.to_owned());
    if recent.len() > RECENT_LIMIT {
        recent.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use super::{RECENT_LIMIT, remember};

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
}
