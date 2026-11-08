//! JSON persistence for generated Truth or Dare prompts.
//!
//! The file is a small snapshot rather than a database: it is rewritten
//! atomically (temporary file + rename) and an unreadable file never stops the
//! bot, it only means starting with an empty cache.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{Category, Kind};

const VERSION: u32 = 1;

/// Prompts for one language, category and kind.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub(super) struct BucketData {
    /// Generated but not yet served.
    #[serde(default)]
    pub ready: Vec<String>,
    /// Served recently, oldest first. Used to reject duplicates.
    #[serde(default)]
    pub recent: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub(super) struct Snapshot {
    version: u32,
    /// Keyed `language/category/kind`, for example `ka/casual/truth`.
    pub buckets: BTreeMap<String, BucketData>,
}

impl Snapshot {
    pub fn new() -> Self {
        Self { version: VERSION, buckets: BTreeMap::new() }
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).context("failed to serialize the prompt store")
    }
}

pub(super) fn bucket_key(language: &str, category: Category, kind: Kind) -> String {
    format!("{language}/{}/{}", category.label().to_lowercase(), kind.label().to_lowercase())
}

pub(super) struct Store {
    path: PathBuf,
    /// Sequence number of the newest snapshot written. Writes run on blocking
    /// threads and may be scheduled out of order; an older snapshot must never
    /// replace a newer one.
    written: Mutex<u64>,
}

impl Store {
    pub fn new(path: PathBuf) -> Self {
        Self { path, written: Mutex::new(0) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Never fails: a missing, unreadable, or incompatible file yields an empty snapshot.
    pub fn load(&self) -> Snapshot {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Snapshot::new(),
            Err(error) => {
                warn!(path = %self.path.display(), error = ?error,
                    "could not read the prompt store; starting with an empty cache");
                return Snapshot::new();
            }
        };
        match serde_json::from_str::<Snapshot>(&text) {
            Ok(snapshot) if snapshot.version == VERSION => snapshot,
            Ok(snapshot) => {
                warn!(path = %self.path.display(), version = snapshot.version,
                    "unsupported prompt store version; starting with an empty cache");
                self.set_aside();
                Snapshot::new()
            }
            Err(error) => {
                warn!(path = %self.path.display(), error = ?error,
                    "prompt store is corrupt; starting with an empty cache");
                self.set_aside();
                Snapshot::new()
            }
        }
    }

    /// Write `json` unless a newer snapshot has already been written.
    pub fn save(&self, seq: u64, json: &str) -> Result<()> {
        let mut written = self.written.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if seq <= *written {
            return Ok(());
        }
        if let Some(parent) = self.path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs::create_dir_all(parent).context("failed to create the prompt store directory")?;
        }
        let temporary = self.sibling(".tmp");
        fs::write(&temporary, json).context("failed to write the prompt store")?;
        fs::rename(&temporary, &self.path).context("failed to replace the prompt store")?;
        *written = seq;
        Ok(())
    }

    /// Keep an unusable file for inspection instead of silently overwriting it.
    fn set_aside(&self) {
        let backup = self.sibling(".bad");
        if let Err(error) = fs::rename(&self.path, &backup) {
            warn!(path = %self.path.display(), error = ?error, "could not set the prompt store aside");
        }
    }

    fn sibling(&self, suffix: &str) -> PathBuf {
        let mut name = self.path.clone().into_os_string();
        name.push(suffix);
        name.into()
    }
}

#[cfg(test)]
pub(super) mod testing {
    use std::path::PathBuf;

    /// A unique path in the system temp directory; the caller removes the files.
    pub fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tod-store-{}-{name}.json", std::process::id()))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::testing::temp_path;
    use super::{BucketData, Category, Kind, Snapshot, Store, bucket_key};

    fn cleanup(store: &Store) {
        for suffix in ["", ".tmp", ".bad"] {
            let _ = fs::remove_file(store.sibling(suffix));
        }
    }

    #[test]
    fn keys_separate_language_category_and_kind() {
        assert_eq!(bucket_key("ka", Category::Casual, Kind::Truth), "ka/casual/truth");
        assert_ne!(
            bucket_key("en", Category::Romantic, Kind::Dare),
            bucket_key("ka", Category::Romantic, Kind::Dare)
        );
    }

    #[test]
    fn snapshot_round_trips_through_disk() {
        let store = Store::new(temp_path("round-trip"));
        let mut snapshot = Snapshot::new();
        snapshot.buckets.insert(
            "ka/casual/truth".into(),
            BucketData { ready: vec!["ერთი".into()], recent: vec!["ორი".into()] },
        );
        store.save(1, &snapshot.to_json().unwrap()).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.buckets, snapshot.buckets);
        cleanup(&store);
    }

    #[test]
    fn missing_file_starts_empty() {
        let store = Store::new(temp_path("missing"));
        assert!(store.load().buckets.is_empty());
    }

    #[test]
    fn corrupt_file_is_set_aside_not_fatal() {
        let store = Store::new(temp_path("corrupt"));
        fs::write(store.path(), "{ not json").unwrap();
        assert!(store.load().buckets.is_empty());
        assert!(!store.path().exists());
        assert!(store.sibling(".bad").exists());
        cleanup(&store);
    }

    #[test]
    fn older_snapshot_never_replaces_a_newer_one() {
        let store = Store::new(temp_path("ordering"));
        let mut newer = Snapshot::new();
        newer.buckets.insert("ka/casual/dare".into(), BucketData { ready: vec!["new".into()], recent: vec![] });
        store.save(2, &newer.to_json().unwrap()).unwrap();
        store.save(1, &Snapshot::new().to_json().unwrap()).unwrap();
        assert_eq!(store.load().buckets, newer.buckets);
        cleanup(&store);
    }
}
