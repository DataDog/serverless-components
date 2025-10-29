//! Sled-backed storage helpers for Remote Config metadata and target files.
//!
//! The Go agent uses BoltDB to persist Uptane state. This module provides a
//! sled-backed equivalent with the same tree/bucket layout, automatic metadata
//! validation, and helper APIs to manipulate org UUIDs, target files, and
//! delegated metadata.

use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hex::encode as hex_encode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sled::{Config as SledConfig, Db, IVec, Tree};
use thiserror::Error;
use time::OffsetDateTime;

/// Tree name dedicated to store metadata records.
const META_TREE: &str = "__meta";
/// Key for the JSON-encoded [`Metadata`] record.
const META_KEY: &[u8] = b"meta.json";
/// Prefix used for storing organisation UUIDs keyed by config root version.
const ORG_UUID_PREFIX: &str = "org_uuid/";
/// Key storing the last root version associated with [`TREE_ORG_DATA`].
const ORG_LATEST_ROOT_KEY: &[u8] = b"org/latest_root_version";
/// Key storing the current organisation UUID associated with the cache.
const ORG_CURRENT_UUID_KEY: &[u8] = b"org/current_uuid";

/// Name of the tree storing Uptane configuration repository metadata.
pub const TREE_CONFIG_REPO: &str = "config_repo";
/// Name of the tree storing Uptane director repository metadata.
pub const TREE_DIRECTOR_REPO: &str = "director_repo";
/// Name of the tree storing raw target files.
pub const TREE_TARGET_FILES: &str = "target_files";
/// Name of the tree storing organisation-level data (such as Org UUID).
pub const TREE_ORG_DATA: &str = "org_data";

/// Type alias for key-value pairs returned from sled operations.
type KvPairs = Vec<(Vec<u8>, Vec<u8>)>;

/// Metadata persisted alongside the embedded database.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Metadata {
    /// Datadog Agent version associated with this store.
    pub version: String,
    /// SHA256 hash of the API key used to initialise the store.
    pub api_key_hash: String,
    /// RFC3339 timestamp when the store was created.
    pub creation_time: OffsetDateTime,
    /// Remote configuration backend URL.
    pub url: String,
}

/// Errors emitted by the [`RcStore`].
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] sled::Error),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("metadata missing from remote-config store")]
    MissingMetadata,
}

/// Errors returned from key-iteration helpers.
#[derive(Debug)]
pub enum ForEachKeyError<E> {
    Store(StoreError),
    Visitor(E),
}

impl<E> std::fmt::Display for ForEachKeyError<E>
where
    E: std::fmt::Display,
{
    /// Formats the inner error (either store or visitor) for display purposes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForEachKeyError::Store(err) => err.fmt(f),
            ForEachKeyError::Visitor(err) => err.fmt(f),
        }
    }
}

impl<E> std::error::Error for ForEachKeyError<E>
where
    E: std::error::Error + 'static,
{
    /// Propagates the underlying error so callers can inspect chained failures.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ForEachKeyError::Store(err) => Some(err),
            ForEachKeyError::Visitor(err) => Some(err),
        }
    }
}

impl<E> From<StoreError> for ForEachKeyError<E> {
    /// Wraps a plain `StoreError` in the visitor-friendly error type.
    fn from(err: StoreError) -> Self {
        Self::Store(err)
    }
}

/// Wrapper around a sled database providing bucket-oriented access.
#[derive(Debug, Clone)]
pub struct RcStore {
    db: Db,
    path: PathBuf,
}

impl RcStore {
    /// Opens (or creates) a remote-config store at the provided path.
    ///
    /// The store embeds metadata describing the active agent version, API key hash,
    /// and backend URL. If an existing store contains conflicting metadata, it will
    /// be discarded and recreated to avoid mixing data from different environments.
    pub fn open<P, S1, S2, S3>(
        path: P,
        agent_version: S1,
        api_key: S2,
        url: S3,
    ) -> Result<Self, StoreError>
    where
        P: AsRef<Path>,
        S1: AsRef<str>,
        S2: AsRef<str>,
        S3: AsRef<str>,
    {
        // Normalise the path and ensure the directory exists.
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                // sled does not create intermediate directories automatically.
                fs::create_dir_all(parent)?;
            }
        }

        let config = sled_config(&path);
        let api_key_hash = hash_api_key(api_key.as_ref());
        let agent_version = agent_version.as_ref().to_owned();
        let url = url.as_ref().to_owned();

        match config.open() {
            Ok(db) => {
                let store = RcStore {
                    db,
                    path: path.clone(),
                };
                match store.validate_or_write_metadata(&agent_version, &api_key_hash, &url)? {
                    MetadataState::Valid => Ok(store),
                    MetadataState::Written => {
                        // Metadata written for the first time means trees must be initialised.
                        store.initialise_default_trees()?;
                        Ok(store)
                    }
                    MetadataState::Mismatch => {
                        // Previous contents belong to another identity; wipe the database.
                        drop(store);
                        reset_path(&path)?;
                        let db = sled_config(&path).open()?;
                        let new_store = RcStore { db, path };
                        new_store.write_metadata(&agent_version, &api_key_hash, &url)?;
                        new_store.initialise_default_trees()?;
                        Ok(new_store)
                    }
                }
            }
            Err(err) => match err {
                sled::Error::Io(_) => {
                    // IO errors typically mean stale locks or damaged files — rebuild from scratch.
                    reset_path(&path)?;
                    let db = sled_config(&path).open()?;
                    let store = RcStore { db, path };
                    store.write_metadata(&agent_version, &api_key_hash, &url)?;
                    store.initialise_default_trees()?;
                    Ok(store)
                }
                other => Err(StoreError::Db(other)),
            },
        }
    }

    /// Opens an in-memory remote-config store (ephemeral across restarts).
    pub fn open_ephemeral<S1, S2, S3>(
        agent_version: S1,
        api_key: S2,
        url: S3,
    ) -> Result<Self, StoreError>
    where
        S1: AsRef<str>,
        S2: AsRef<str>,
        S3: AsRef<str>,
    {
        let api_key_hash = hash_api_key(api_key.as_ref());
        let agent_version = agent_version.as_ref().to_owned();
        let url = url.as_ref().to_owned();

        let db = SledConfig::new().temporary(true).open()?;
        let store = RcStore {
            db,
            path: PathBuf::new(),
        };
        store.write_metadata(&agent_version, &api_key_hash, &url)?;
        store.initialise_default_trees()?;
        Ok(store)
    }

    /// Returns the filesystem path backing the store.
    ///
    /// Ephemeral stores return an empty path because data resides in memory only.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Fetches the store metadata.
    pub fn metadata(&self) -> Result<Metadata, StoreError> {
        let tree = self.db.open_tree(META_TREE)?;
        let Some(bytes) = tree.get(META_KEY)? else {
            return Err(StoreError::MissingMetadata);
        };
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Opens (or creates) a tree with the given name.
    pub fn tree(&self, name: &str) -> Result<RcTree, StoreError> {
        let tree = self.db.open_tree(name)?;
        Ok(RcTree {
            inner: Arc::new(tree),
        })
    }

    /// Flushes pending writes to disk.
    pub fn flush(&self) -> Result<(), StoreError> {
        self.db.flush()?;
        Ok(())
    }

    /// Persists the organisation UUID associated with the provided config root version.
    pub fn set_org_uuid(&self, root_version: u64, uuid: &str) -> Result<(), StoreError> {
        let tree = self.tree(TREE_ORG_DATA)?;
        let key = org_uuid_key(root_version);
        tree.insert(key.as_slice(), uuid.as_bytes())?;
        let version_bytes = root_version.to_le_bytes();
        tree.insert(ORG_LATEST_ROOT_KEY, version_bytes.as_slice())?;
        tree.insert(ORG_CURRENT_UUID_KEY, uuid.as_bytes())?;
        Ok(())
    }

    /// Returns the stored organisation UUID for the provided config root version (if any).
    pub fn org_uuid(&self, root_version: u64) -> Result<Option<String>, StoreError> {
        let tree = self.tree(TREE_ORG_DATA)?;
        let key = org_uuid_key(root_version);
        Ok(tree
            .get(key.as_slice())?
            .map(|bytes| String::from_utf8_lossy(&bytes).to_string()))
    }

    /// Returns the latest `(root_version, org_uuid)` pair stored in the org data tree.
    pub fn latest_org_binding(&self) -> Result<Option<(u64, String)>, StoreError> {
        let tree = self.tree(TREE_ORG_DATA)?;
        let Some(version_bytes) = tree.get(ORG_LATEST_ROOT_KEY)? else {
            return Ok(None);
        };
        let Some(uuid_bytes) = tree.get(ORG_CURRENT_UUID_KEY)? else {
            return Ok(None);
        };
        if version_bytes.len() != std::mem::size_of::<u64>() {
            return Ok(None);
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&version_bytes);
        let version = u64::from_le_bytes(buf);
        let uuid = String::from_utf8_lossy(&uuid_bytes).to_string();
        Ok(Some((version, uuid)))
    }

    /// Clears all cached Uptane data while preserving metadata identity.
    pub fn reset_data(&self) -> Result<(), StoreError> {
        for bucket in [
            TREE_CONFIG_REPO,
            TREE_DIRECTOR_REPO,
            TREE_TARGET_FILES,
            TREE_ORG_DATA,
        ] {
            match self.db.drop_tree(bucket) {
                Ok(_) => {}
                // The tree may not exist yet (fresh caches), which is harmless.
                Err(sled::Error::CollectionNotFound(_)) => {}
                Err(err) => return Err(StoreError::Db(err)),
            }
        }
        self.initialise_default_trees()?;
        Ok(())
    }

    /// Rewrites the metadata record using the supplied identity.
    pub fn update_metadata(
        &self,
        version: &str,
        api_key: &str,
        url: &str,
    ) -> Result<(), StoreError> {
        let api_key_hash = hash_api_key(api_key);
        self.write_metadata(version, &api_key_hash, url)?;
        Ok(())
    }

    /// Creates the default trees used by the remote-config cache.
    fn initialise_default_trees(&self) -> Result<(), StoreError> {
        for bucket in [
            TREE_CONFIG_REPO,
            TREE_DIRECTOR_REPO,
            TREE_TARGET_FILES,
            TREE_ORG_DATA,
        ] {
            let _ = self.db.open_tree(bucket)?;
        }
        self.db.flush()?;
        Ok(())
    }

    /// Validates existing metadata or writes a new record if none exist.
    fn validate_or_write_metadata(
        &self,
        version: &str,
        api_key_hash: &str,
        url: &str,
    ) -> Result<MetadataState, StoreError> {
        let tree = self.db.open_tree(META_TREE)?;
        match tree.get(META_KEY)? {
            None => {
                // No metadata present yet: write a new record.
                self.write_metadata(version, api_key_hash, url)?;
                Ok(MetadataState::Written)
            }
            Some(bytes) => {
                let metadata: Metadata = serde_json::from_slice(&bytes)?;
                if metadata.version == version
                    && metadata.api_key_hash == api_key_hash
                    && metadata.url == url
                {
                    Ok(MetadataState::Valid)
                } else {
                    // Different identity detected (version/API key/url mismatch).
                    Ok(MetadataState::Mismatch)
                }
            }
        }
    }

    /// Writes metadata reflecting the current agent identity.
    fn write_metadata(
        &self,
        version: &str,
        api_key_hash: &str,
        url: &str,
    ) -> Result<(), StoreError> {
        let metadata = Metadata {
            version: version.to_owned(),
            api_key_hash: api_key_hash.to_owned(),
            creation_time: OffsetDateTime::now_utc(),
            url: url.to_owned(),
        };
        let bytes = serde_json::to_vec(&metadata)?;
        let tree = self.db.open_tree(META_TREE)?;
        tree.insert(META_KEY, bytes)?;
        tree.flush()?;
        Ok(())
    }
}

/// Classification of metadata validation outcomes.
#[derive(Debug)]
enum MetadataState {
    Valid,
    Written,
    Mismatch,
}

/// Hashes the API key so it is not persisted in clear-text.
fn hash_api_key(api_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(api_key.as_bytes());
    hex_encode(hasher.finalize())
}

/// Builds the sled key used to store an org UUID for the given root version.
fn org_uuid_key(root_version: u64) -> Vec<u8> {
    format!("{ORG_UUID_PREFIX}{root_version}").into_bytes()
}

/// Builds a sled configuration using the provided filesystem path.
fn sled_config(path: &Path) -> SledConfig {
    let mut config = SledConfig::new();
    config = config.path(path);
    config = config.cache_capacity(64 * 1024 * 1024); // 64MB cache
    config
}

/// Deletes the database file or directory to start from a clean slate.
fn reset_path(path: &Path) -> Result<(), StoreError> {
    if path.exists() {
        if path.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// A handle to a named tree within the remote-config store.
#[derive(Debug, Clone)]
pub struct RcTree {
    inner: Arc<Tree>,
}

impl RcTree {
    /// Retrieves the value associated with `key`.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.inner.get(key)?.map(|value| value.as_ref().to_vec()))
    }

    /// Inserts `value` for `key`, returning the previous value if present.
    pub fn insert(&self, key: &[u8], value: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self
            .inner
            .insert(key, value)?
            .map(|value| value.as_ref().to_vec()))
    }

    /// Removes the value for `key`, returning it if present.
    pub fn remove(&self, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.inner.remove(key)?.map(|value| value.as_ref().to_vec()))
    }

    /// Returns all key/value pairs whose keys start with `prefix`.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<KvPairs, StoreError> {
        let iter = self.inner.scan_prefix(prefix);
        let mut entries = Vec::new();
        for result in iter {
            let (key, value) = result?;
            entries.push((ivec_to_vec(key), ivec_to_vec(value)));
        }
        Ok(entries)
    }

    /// Returns all key/value pairs stored in the tree.
    pub fn entries(&self) -> Result<KvPairs, StoreError> {
        let iter = self.inner.iter();
        let mut entries = Vec::new();
        for result in iter {
            let (key, value) = result?;
            entries.push((ivec_to_vec(key), ivec_to_vec(value)));
        }
        Ok(entries)
    }

    /// Applies a batch of write operations atomically.
    pub fn apply_batch(&self, batch: sled::Batch) -> Result<(), StoreError> {
        self.inner.apply_batch(batch)?;
        Ok(())
    }

    /// Visits each key in the tree until the visitor breaks the traversal.
    /// Returning `ControlFlow::Break(value)` stops iteration and surfaces `Some(value)`.
    pub fn for_each_key<F, T>(&self, mut visit: F) -> Result<Option<T>, StoreError>
    where
        F: FnMut(&[u8]) -> ControlFlow<T>,
    {
        self.try_for_each_key::<_, T, std::convert::Infallible>(|key| Ok(visit(key)))
            .map_err(|err| match err {
                ForEachKeyError::Store(err) => err,
                ForEachKeyError::Visitor(infallible) => match infallible {},
            })
    }

    /// Like [`for_each_key`] but allows the visitor to return domain-specific errors.
    pub fn try_for_each_key<F, T, E>(&self, mut visit: F) -> Result<Option<T>, ForEachKeyError<E>>
    where
        F: FnMut(&[u8]) -> Result<ControlFlow<T>, E>,
    {
        for result in self.inner.iter().keys() {
            let key = result.map_err(|err| ForEachKeyError::Store(StoreError::Db(err)))?;
            match visit(key.as_ref()).map_err(ForEachKeyError::Visitor)? {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(value) => return Ok(Some(value)),
            }
        }
        Ok(None)
    }

    /// Exposes the underlying sled tree for advanced operations (e.g., transactions).
    pub fn as_tree(&self) -> Tree {
        self.inner.as_ref().clone()
    }
}

/// Converts sled's owned buffer type into a plain `Vec<u8>`.
fn ivec_to_vec(value: IVec) -> Vec<u8> {
    value.as_ref().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const VERSION_A: &str = "7.60.0";
    const VERSION_B: &str = "7.61.0";
    const API_KEY_A: &str = "test_api_key";
    const API_KEY_B: &str = "different_api_key";
    const URL_A: &str = "https://config.datadoghq.com";

    #[test]
    /// Creates the on-disk database with metadata plus empty sled trees.
    fn open_creates_metadata_and_trees() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("remote-config.db");
        let store = RcStore::open(&path, VERSION_A, API_KEY_A, URL_A).unwrap();

        // metadata exists
        let metadata = store.metadata().unwrap();
        assert_eq!(metadata.version, VERSION_A);
        assert_eq!(metadata.api_key_hash, hash_api_key(API_KEY_A));
        assert_eq!(metadata.url, URL_A);

        // default trees exist
        for tree in [
            TREE_CONFIG_REPO,
            TREE_DIRECTOR_REPO,
            TREE_TARGET_FILES,
            TREE_ORG_DATA,
        ] {
            let handle = store.tree(tree).unwrap();
            assert!(handle.entries().unwrap().is_empty());
        }
    }

    #[test]
    /// Re-opens an existing store when the metadata identity matches.
    fn open_preserves_data_when_metadata_matches() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("remote-config.db");
        {
            let store = RcStore::open(&path, VERSION_A, API_KEY_A, URL_A).unwrap();
            let tree = store.tree(TREE_TARGET_FILES).unwrap();
            tree.insert(b"key", b"value").unwrap();
            store.flush().unwrap();
        }

        let reopened = RcStore::open(&path, VERSION_A, API_KEY_A, URL_A).unwrap();
        let tree = reopened.tree(TREE_TARGET_FILES).unwrap();
        assert_eq!(tree.get(b"key").unwrap(), Some(b"value".to_vec()));
    }

    #[test]
    /// Rebuilds the database when the agent version changes.
    fn open_recreates_store_when_metadata_differs() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("remote-config.db");
        {
            let store = RcStore::open(&path, VERSION_A, API_KEY_A, URL_A).unwrap();
            let tree = store.tree(TREE_TARGET_FILES).unwrap();
            tree.insert(b"key", b"value").unwrap();
            store.flush().unwrap();
        }

        // Different version triggers recreation
        let reopened =
            RcStore::open(&path, VERSION_B, API_KEY_A, URL_A).expect("reopen with new version");
        let tree = reopened.tree(TREE_TARGET_FILES).unwrap();
        assert!(tree.get(b"key").unwrap().is_none());

        // Metadata should reflect new version
        let metadata = reopened.metadata().unwrap();
        assert_eq!(metadata.version, VERSION_B);
    }

    #[test]
    /// Clears the cache when the API key differs between runs.
    fn open_recreates_store_when_api_key_changes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("remote-config.db");
        {
            let store = RcStore::open(&path, VERSION_A, API_KEY_A, URL_A).unwrap();
            let tree = store.tree(TREE_TARGET_FILES).unwrap();
            tree.insert(b"key", b"value").unwrap();
            store.flush().unwrap();
        }

        let reopened =
            RcStore::open(&path, VERSION_A, API_KEY_B, URL_A).expect("reopen with new api key");
        let tree = reopened.tree(TREE_TARGET_FILES).unwrap();
        assert!(tree.get(b"key").unwrap().is_none());

        let metadata = reopened.metadata().unwrap();
        assert_eq!(metadata.api_key_hash, hash_api_key(API_KEY_B));
    }

    #[test]
    /// Ensures `reset_data` wipes every sled tree.
    fn reset_data_clears_all_trees() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("remote-config.db");
        let store = RcStore::open(&path, VERSION_A, API_KEY_A, URL_A).unwrap();

        store
            .tree(TREE_CONFIG_REPO)
            .unwrap()
            .insert(b"config", b"value")
            .unwrap();
        store
            .tree(TREE_DIRECTOR_REPO)
            .unwrap()
            .insert(b"director", b"value")
            .unwrap();
        store
            .tree(TREE_TARGET_FILES)
            .unwrap()
            .insert(b"target", b"value")
            .unwrap();
        store.flush().unwrap();

        store.reset_data().unwrap();

        for tree in [
            TREE_CONFIG_REPO,
            TREE_DIRECTOR_REPO,
            TREE_TARGET_FILES,
            TREE_ORG_DATA,
        ] {
            assert!(store.tree(tree).unwrap().entries().unwrap().is_empty());
        }
    }

    #[test]
    /// Confirms metadata updates overwrite the identity document.
    fn update_metadata_rewrites_identity() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("remote-config.db");
        let store = RcStore::open(&path, VERSION_A, API_KEY_A, URL_A).unwrap();

        store
            .update_metadata(VERSION_B, API_KEY_B, URL_A)
            .expect("metadata update succeeds");

        let metadata = store.metadata().unwrap();
        assert_eq!(metadata.version, VERSION_B);
        assert_eq!(metadata.api_key_hash, hash_api_key(API_KEY_B));
    }

    #[test]
    /// Builds a temporary in-memory store for short-lived tests.
    fn open_ephemeral_creates_in_memory_store() {
        let store = RcStore::open_ephemeral(VERSION_A, API_KEY_A, URL_A).unwrap();
        assert!(store.path().as_os_str().is_empty());
        for tree in [
            TREE_CONFIG_REPO,
            TREE_DIRECTOR_REPO,
            TREE_TARGET_FILES,
            TREE_ORG_DATA,
        ] {
            assert!(store.tree(tree).unwrap().entries().unwrap().is_empty());
        }
    }

    #[test]
    /// Associates org UUIDs with individual config root versions.
    fn org_uuid_is_scoped_per_root_version() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("remote-config.db");
        let store = RcStore::open(&path, VERSION_A, API_KEY_A, URL_A).unwrap();

        store.set_org_uuid(1, "org-a").unwrap();
        store.set_org_uuid(2, "org-b").unwrap();

        assert_eq!(store.org_uuid(1).unwrap().as_deref(), Some("org-a"));
        assert_eq!(store.org_uuid(2).unwrap().as_deref(), Some("org-b"));
        assert!(store.org_uuid(3).unwrap().is_none());
    }

    #[test]
    /// Tracks the most recent org binding tuple for quick retrieval.
    fn latest_org_binding_tracks_updates() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("remote-config.db");
        let store = RcStore::open(&path, VERSION_A, API_KEY_A, URL_A).unwrap();

        assert!(store.latest_org_binding().unwrap().is_none());
        store.set_org_uuid(10, "org-initial").unwrap();
        let first = store.latest_org_binding().unwrap().expect("binding");
        assert_eq!(first.0, 10);
        assert_eq!(first.1, "org-initial");

        store.set_org_uuid(42, "org-updated").unwrap();
        let second = store.latest_org_binding().unwrap().expect("binding");
        assert_eq!(second.0, 42);
        assert_eq!(second.1, "org-updated");
    }

    #[test]
    /// Confirms the tree iterator streams keys without loading the entire tree into memory.
    fn tree_iterates_keys_without_collecting_all() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("remote-config.db");
        let store = RcStore::open(&path, VERSION_A, API_KEY_A, URL_A).unwrap();
        let tree = store.tree(TREE_TARGET_FILES).unwrap();
        tree.insert(b"key-a", b"value-a").unwrap();
        tree.insert(b"key-b", b"value-b").unwrap();

        let mut keys = Vec::new();
        tree.for_each_key(|key| {
            keys.push(key.to_vec());
            ControlFlow::<()>::Continue(())
        })
        .unwrap();
        keys.sort();
        assert_eq!(keys, vec![b"key-a".to_vec(), b"key-b".to_vec()]);
    }

    /// Validates that removing the metadata record triggers the `MissingMetadata` error.
    #[test]
    fn metadata_missing_returns_error() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("remote-config.db");
        let store = RcStore::open(&path, VERSION_A, API_KEY_A, URL_A).unwrap();

        // Drop the metadata entry manually to mimic corruption.
        let meta_tree = store.tree(META_TREE).unwrap();
        meta_tree.remove(META_KEY).unwrap();

        let err = store
            .metadata()
            .expect_err("metadata retrieval should fail");
        assert!(matches!(err, StoreError::MissingMetadata));
    }

    /// Exercises tree helper operations to ensure prefix scans, removals, and batch writes behave.
    #[test]
    fn tree_helpers_cover_prefix_removal_and_batch() {
        use std::fs;

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("remote-config.db");
        let store = RcStore::open(&path, VERSION_A, API_KEY_A, URL_A).unwrap();
        let tree = store.tree(TREE_TARGET_FILES).unwrap();

        tree.insert(b"cfg/foo", b"one").unwrap();
        tree.insert(b"cfg/bar", b"two").unwrap();
        tree.insert(b"other", b"three").unwrap();

        // Prefix scan should only surface the configuration entries.
        let mut prefix_entries = tree.scan_prefix(b"cfg/").unwrap();
        prefix_entries.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            prefix_entries,
            vec![
                (b"cfg/bar".to_vec(), b"two".to_vec()),
                (b"cfg/foo".to_vec(), b"one".to_vec())
            ]
        );

        // Apply a batch write to cover the dedicated method.
        let mut batch = sled::Batch::default();
        batch.insert(b"batch", b"value");
        tree.apply_batch(batch).unwrap();
        assert_eq!(tree.get(b"batch").unwrap(), Some(b"value".to_vec()));

        // Remove an entry and ensure the remainder still exists.
        let removed = tree.remove(b"cfg/foo").unwrap();
        assert_eq!(removed, Some(b"one".to_vec()));
        let keys: Vec<_> = tree
            .entries()
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert!(keys.contains(&b"cfg/bar".to_vec()));
        assert!(keys.contains(&b"other".to_vec()));
        assert!(keys.contains(&b"batch".to_vec()));
        assert!(!keys.contains(&b"cfg/foo".to_vec()));

        // Cover `reset_path` by tearing down the database file explicitly.
        drop(tree);
        drop(store);
        super::reset_path(&path).unwrap();
        assert!(!path.exists());

        // Also ensure directories are removed recursively.
        let dir_path = tmp.path().join("dir");
        fs::create_dir_all(&dir_path).unwrap();
        fs::write(dir_path.join("file"), b"payload").unwrap();
        super::reset_path(&dir_path).unwrap();
        assert!(!dir_path.exists());
    }
}
