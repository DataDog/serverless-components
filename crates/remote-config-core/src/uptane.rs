//! Uptane/TUF state management mirroring the Go remote-config client.
//!
//! This module owns the sled-backed storage of metadata and target files,
//! ensuring the same layout (trees, keys, pruning rules) the Go agent relies
//! on for resumable refreshes and diagnostics.

use std::borrow::Cow;
#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};
use std::convert::TryFrom;
use std::ops::ControlFlow;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, TimeZone, Utc};
use hex::encode as hex_encode;
use remote_config_proto::remoteconfig::{
    ConfigMetas, DelegatedMeta, DirectorMetas, LatestConfigsResponse, TopMeta,
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sled::transaction::{TransactionError, Transactional};
use sled::Batch;
use thiserror::Error;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use tuf::database::Database;
use tuf::error::Error as TufError;
use tuf::metadata::{
    Metadata as TufMetadata, MetadataPath, RawSignedMetadata, RawSignedMetadataSet,
    RawSignedMetadataSetBuilder,
};
use tuf::pouf::Pouf1;

use crate::store::{
    Metadata as StoreMetadata, RcStore, RcTree, StoreError, TREE_CONFIG_REPO, TREE_DIRECTOR_REPO,
    TREE_TARGET_FILES,
};
use crate::targets::{parse_targets_map_from_bytes, TargetDescription};
use crate::uptane_path::{parse_config_path_opt, ConfigPathSource};

/// File name for TUF root metadata.
const META_ROOT: &str = "root.json";
/// File name for TUF timestamp metadata.
const META_TIMESTAMP: &str = "timestamp.json";
/// File name for TUF snapshot metadata.
const META_SNAPSHOT: &str = "snapshot.json";
/// File name for TUF targets metadata.
const META_TARGETS: &str = "targets.json";

/// Prefix used when storing delegated targets in the config tree.
const CONFIG_DELEGATED_PREFIX: &str = "delegated/";

/// Prefix applied to raw target file keys.
const TARGET_PREFIX: &str = "target:";

/// Canonical top-level metadata filenames that always exist in a repository.
const TOP_LEVEL_META_FILES: [&str; 4] = [META_ROOT, META_TIMESTAMP, META_SNAPSHOT, META_TARGETS];

#[cfg(test)]
thread_local! {
    static TUF_VERIFICATION_ENABLED: Cell<bool> = Cell::new(false);
}

/// Returns `true` when verification should be skipped (unit tests inject unsigned metadata).
fn tuf_verification_disabled() -> bool {
    #[cfg(test)]
    {
        // In tests we flip the flag per thread so unit tests can opt into verification in isolation.
        !TUF_VERIFICATION_ENABLED.with(|flag| flag.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

/// Returns the timestamp passed to rust-tuf when verifying metadata (we skip expiry checks like Go).
fn tuf_verification_start_time() -> DateTime<Utc> {
    Utc.timestamp_opt(0, 0)
        .single()
        .expect("unix epoch should always construct")
}

/// Decodes the snapshot metadata structure to extract custom fields.
#[derive(Debug, Deserialize)]
struct SnapshotDocument {
    signed: SnapshotSignedSection,
}

/// Captures whether the snapshot metadata includes an `org_uuid` binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotOrgBinding {
    /// Snapshot metadata is missing or does not declare an org binding.
    Missing,
    /// Snapshot metadata explicitly includes an org UUID string.
    Present(String),
}

impl SnapshotOrgBinding {
    /// Returns the UUID as `&str` when present.
    pub fn as_deref(&self) -> Option<&str> {
        match self {
            SnapshotOrgBinding::Present(value) => Some(value.as_str()),
            SnapshotOrgBinding::Missing => None,
        }
    }

    /// Converts the binding into an owned `Option<String>`.
    pub fn into_option(self) -> Option<String> {
        match self {
            SnapshotOrgBinding::Present(value) => Some(value),
            SnapshotOrgBinding::Missing => None,
        }
    }
}

/// Represents the signed part of the snapshot metadata.
#[derive(Debug, Deserialize)]
struct SnapshotSignedSection {
    #[serde(default)]
    custom: Option<SnapshotCustomSection>,
}

/// Custom metadata embedded in the snapshot (e.g., org UUID binding).
#[derive(Debug, Deserialize, Default)]
struct SnapshotCustomSection {
    #[serde(default, rename = "org_uuid")]
    org_uuid: Option<String>,
}

/// Parses the snapshot bytes and extracts the org binding state.
fn parse_snapshot_org_binding(bytes: &[u8]) -> Result<SnapshotOrgBinding> {
    let document: SnapshotDocument = serde_json::from_slice(bytes)?;
    Ok(
        match document.signed.custom.and_then(|custom| custom.org_uuid) {
            Some(uuid) => SnapshotOrgBinding::Present(uuid),
            None => SnapshotOrgBinding::Missing,
        },
    )
}

/// Entry describing the version of a metadata file referenced by the snapshot.
#[derive(Debug, Deserialize)]
struct SnapshotMetaEntry {
    /// Version number advertised for the metadata file.
    version: u64,
}

/// Signed snapshot payload used to discover referenced metadata versions.
#[derive(Debug, Deserialize)]
struct SnapshotMetaSignedDocument {
    signed: SnapshotMetaSigned,
}

/// Inner section that holds individual metadata version entries.
#[derive(Debug, Deserialize)]
struct SnapshotMetaSigned {
    meta: BTreeMap<String, SnapshotMetaEntry>,
}

/// Errors emitted by the Uptane state manager.
#[derive(Debug, Error)]
pub enum UptaneError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("tuf metadata parse error: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("timestamp parse error: {0}")]
    Time(#[from] time::error::Parse),
    #[error("missing field {0}")]
    MissingField(&'static str),
    #[error("snapshot org uuid missing (expected '{expected}')")]
    OrgUuidMissing { expected: String },
    #[error("snapshot org uuid '{snapshot}' does not match stored '{stored}'")]
    OrgUuidMismatch { stored: String, snapshot: String },
    #[error("target path '{path}' org_id mismatch (expected {expected}, actual {actual})")]
    OrgIdMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("tuf verification error: {0}")]
    Tuf(#[from] TufError),
    #[error("director target '{path}' missing from config repository")]
    TargetMissingInConfig { path: String },
    #[error(
        "target '{path}' length mismatch (director {director_length}, config {config_length})"
    )]
    TargetLengthMismatch {
        path: String,
        director_length: i64,
        config_length: i64,
    },
    #[error("target '{path}' missing hash '{algorithm}' in config repository")]
    TargetHashMissing { path: String, algorithm: String },
    #[error("target '{path}' hash mismatch for '{algorithm}'")]
    TargetHashMismatch { path: String, algorithm: String },
    #[error("target payload '{path}' is not referenced by director metadata")]
    UnexpectedTargetPayload { path: String },
    #[error("target payload '{path}' missing from response and cache")]
    MissingTargetPayload { path: String },
    #[error("target '{path}' payload length mismatch (expected {expected}, got {actual})")]
    TargetPayloadLengthMismatch {
        path: String,
        expected: i64,
        actual: i64,
    },
    #[error("target '{path}' payload hash mismatch for '{algorithm}'")]
    TargetPayloadHashMismatch { path: String, algorithm: String },
    #[error("unsupported hash algorithm '{algorithm}' for target validation")]
    UnsupportedHashAlgorithm { algorithm: String },
}

/// Convenience alias for results emitted by the Uptane helpers.
type Result<T> = std::result::Result<T, UptaneError>;

/// Configuration knobs that influence how [`UptaneState`] enforces tenancy.
#[derive(Debug, Clone)]
pub struct UptaneConfig {
    /// Controls org UUID / org ID binding behaviour.
    pub org_binding: OrgBindingConfig,
}

impl Default for UptaneConfig {
    fn default() -> Self {
        Self {
            org_binding: OrgBindingConfig::default(),
        }
    }
}

/// Optional org binding expectations mirrored from the Go agent.
#[derive(Debug, Clone)]
pub struct OrgBindingConfig {
    /// Org UUID the snapshot custom field must advertise (when `Some`).
    pub expected_uuid: Option<String>,
    /// Org ID encoded in target paths (when `Some`).
    pub expected_org_id: Option<u64>,
    /// Whether the snapshot UUID may be auto-learned and persisted when expectations are absent.
    pub auto_store_snapshot_uuid: bool,
}

impl Default for OrgBindingConfig {
    fn default() -> Self {
        Self {
            expected_uuid: None,
            expected_org_id: None,
            auto_store_snapshot_uuid: true,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct OrgBindingDecision {
    persist_uuid: Option<String>,
}

/// Maintains Uptane metadata and target files using [`RcStore`].
#[derive(Debug, Clone)]
pub struct UptaneState {
    store: RcStore,
    config: Arc<RwLock<UptaneConfig>>,
}

impl UptaneState {
    /// Creates a new Uptane state manager backed by the provided [`RcStore`].
    pub fn new(store: RcStore) -> Result<Self> {
        Self::with_config(store, UptaneConfig::default())
    }

    /// Creates a new Uptane state manager with the provided configuration.
    pub fn with_config(store: RcStore, config: UptaneConfig) -> Result<Self> {
        // Ensure required trees exist.
        store.tree(TREE_CONFIG_REPO)?;
        store.tree(TREE_DIRECTOR_REPO)?;
        store.tree(TREE_TARGET_FILES)?;
        Ok(Self {
            store,
            config: Arc::new(RwLock::new(config)),
        })
    }

    /// Resets all cached metadata and targets while preserving store identity.
    pub fn reset_cache(&self) -> Result<()> {
        self.store.reset_data()?;
        Ok(())
    }

    /// Returns the organisation UUID stored alongside the cache, when available.
    pub fn stored_org_uuid(&self) -> Result<Option<String>> {
        let root_version = self.current_config_root_version()?;
        Ok(self.store.org_uuid(root_version)?)
    }

    /// Returns the most recent `(root_version, org_uuid)` tuple stored in the cache, when present.
    pub fn latest_org_binding(&self) -> Result<Option<(u64, String)>> {
        Ok(self.store.latest_org_binding()?)
    }

    /// Updates the org ID enforcement derived from the current RC key.
    pub fn set_expected_org_id(&self, org_id: Option<u64>) {
        if let Ok(mut cfg) = self.config.write() {
            cfg.org_binding.expected_org_id = org_id;
        }
    }

    /// Updates the persisted organisation UUID associated with the cache.
    pub fn update_org_uuid(&self, uuid: &str) -> Result<()> {
        let root_version = self.current_config_root_version()?;
        self.persist_org_binding(root_version, uuid)?;
        Ok(())
    }

    /// Updates the store metadata to reflect new credentials or version.
    pub fn update_metadata(&self, version: &str, api_key: &str, url: &str) -> Result<()> {
        self.store.update_metadata(version, api_key, url)?;
        Ok(())
    }

    /// Seeds the local store with trusted config and director roots when no metadata is present.
    pub fn seed_trust_roots(&self, config_root: &[u8], director_root: &[u8]) -> Result<()> {
        self.seed_root(self.config_tree()?, config_root)?;
        self.seed_root(self.director_tree()?, director_root)?;
        Ok(())
    }

    /// Returns a snapshot of the current org binding configuration.
    fn org_binding_config(&self) -> OrgBindingConfig {
        self.config
            .read()
            .expect("uptane org binding lock poisoned")
            .org_binding
            .clone()
    }

    /// Returns the persisted metadata describing the cache identity.
    pub fn store_metadata(&self) -> Result<StoreMetadata> {
        Ok(self.store.metadata()?)
    }

    /// Opens the sled tree containing configuration repository metadata.
    fn config_tree(&self) -> Result<RcTree> {
        self.store.tree(TREE_CONFIG_REPO).map_err(Into::into)
    }

    /// Returns the version of the currently stored configuration root metadata.
    fn current_config_root_version(&self) -> Result<u64> {
        let tree = self.config_tree()?;
        match tree.get(META_ROOT.as_bytes())? {
            Some(bytes) => Ok(meta_version(&bytes)),
            None => Err(UptaneError::Store(StoreError::MissingMetadata)),
        }
    }

    /// Opens the sled tree containing director repository metadata.
    fn director_tree(&self) -> Result<RcTree> {
        self.store.tree(TREE_DIRECTOR_REPO).map_err(Into::into)
    }

    /// Opens the sled tree containing cached target files.
    fn target_tree(&self) -> Result<RcTree> {
        self.store.tree(TREE_TARGET_FILES).map_err(Into::into)
    }

    /// Seeds a sled tree with the initial root and its versioned copy.
    fn seed_root(&self, tree: RcTree, root_bytes: &[u8]) -> Result<()> {
        if tree.get(META_ROOT.as_bytes())?.is_some() {
            // Store already contains a root, so skip reseeding to preserve history.
            return Ok(());
        }
        let version = meta_version(root_bytes);
        let mut batch = Batch::default();
        batch.insert(META_ROOT.as_bytes(), root_bytes);
        let key = root_version_key(version);
        batch.insert(key.as_bytes(), root_bytes);
        tree.apply_batch(batch)?;
        Ok(())
    }

    /// Persists the org binding tuple for the provided config root version.
    fn persist_org_binding(&self, root_version: u64, uuid: &str) -> Result<()> {
        self.store.set_org_uuid(root_version, uuid)?;
        Ok(())
    }

    /// Updates stored metadata and target files using the backend response.
    /// Applies a [`LatestConfigsResponse`] to the local store, mirroring the
    /// Go agent: config metadata, director metadata, and target files are
    /// written atomically, and stale targets are pruned.
    pub fn update(&self, response: &LatestConfigsResponse) -> Result<()> {
        use tracing::debug;

        debug!("uptane: Updating local store with backend response");
        debug!(
            "uptane:   target_files in response: {}",
            response.target_files.len()
        );

        let config_metas = response
            .config_metas
            .as_ref()
            .ok_or(UptaneError::MissingField("config_metas"))?;
        let director_metas = response
            .director_metas
            .as_ref()
            .ok_or(UptaneError::MissingField("director_metas"))?;

        let (director_targets, org_decision) =
            self.verify_latest_configs(config_metas, director_metas)?;
        let target_tree = self.target_tree()?;
        let complete_target_files = self.build_complete_target_payloads(
            &target_tree,
            &director_targets,
            &response.target_files,
        )?;

        let config_batch = build_config_batch(config_metas);
        let director_batch = build_director_batch(director_metas);

        let target_batch = build_target_batch(&target_tree, &complete_target_files)?;
        let config_tree = self.config_tree()?;
        let director_tree = self.director_tree()?;

        let config_raw = config_tree.as_tree();
        let director_raw = director_tree.as_tree();
        let target_raw = target_tree.as_tree();

        (&config_raw, &director_raw, &target_raw)
            .transaction(|(config_tx, director_tx, target_tx)| {
                config_tx.apply_batch(&config_batch)?;
                director_tx.apply_batch(&director_batch)?;
                target_tx.apply_batch(&target_batch)?;
                Ok(())
            })
            .map_err(|err| match err {
                TransactionError::Abort(e) | TransactionError::Storage(e) => StoreError::Db(e),
            })?;

        debug!("uptane: Store update complete");
        if let Some(uuid) = org_decision.persist_uuid {
            let root_version = self.current_config_root_version()?;
            self.persist_org_binding(root_version, &uuid)?;
        }
        Ok(())
    }

    /// Runs rust-tuf verification against the config and director repositories before caching data.
    /// Returns the parsed director targets metadata for downstream payload validation.
    fn verify_latest_configs(
        &self,
        config_metas: &ConfigMetas,
        director_metas: &DirectorMetas,
    ) -> Result<(BTreeMap<String, TargetDescription>, OrgBindingDecision)> {
        let current_root_version = match self.current_config_root_version() {
            Ok(version) => version,
            Err(UptaneError::Store(StoreError::MissingMetadata)) => 0,
            Err(err) => return Err(err),
        };
        let incoming_root_version = config_metas
            .roots
            .iter()
            .map(|root| root.version)
            .max()
            .map(|version| version.max(current_root_version))
            .unwrap_or(current_root_version);
        let snapshot_binding = self.snapshot_binding_from_metas(config_metas)?;
        let org_decision = self.verify_org_binding(
            snapshot_binding,
            current_root_version,
            incoming_root_version,
        )?;
        if tuf_verification_disabled() {
            // Even when signature verification is skipped (unit-test mode), we still enforce org binding
            // and target parity so corrupted metadata never reaches the sled store.
            let targets = self.verify_target_parity(config_metas, director_metas)?;
            return Ok((targets, org_decision));
        }
        let verification_time = tuf_verification_start_time();
        self.verify_config_repo(config_metas, verification_time)?;
        self.verify_director_repo(director_metas, verification_time)?;
        let targets = self.verify_target_parity(config_metas, director_metas)?;
        Ok((targets, org_decision))
    }

    /// Validates the configuration repository metadata chain and delegated targets set.
    fn verify_config_repo(&self, metas: &ConfigMetas, now: DateTime<Utc>) -> Result<()> {
        let tree = self.config_tree()?;
        let mut db = self.build_database(&tree, now)?;
        self.apply_root_chain(&mut db, &metas.roots)?;
        let snapshot_versions = self.verify_top_level_metadata(
            &tree,
            &mut db,
            metas.timestamp.as_ref(),
            metas.snapshot.as_ref(),
            metas.top_targets.as_ref(),
            now,
        )?;
        self.verify_delegations(
            &tree,
            &mut db,
            &metas.delegated_targets,
            &snapshot_versions,
            now,
        )?;
        Ok(())
    }

    /// Validates the director repository metadata chain (no delegations).
    fn verify_director_repo(&self, metas: &DirectorMetas, now: DateTime<Utc>) -> Result<()> {
        let tree = self.director_tree()?;
        let mut db = self.build_database(&tree, now)?;
        self.apply_root_chain(&mut db, &metas.roots)?;
        let _ = self.verify_top_level_metadata(
            &tree,
            &mut db,
            metas.timestamp.as_ref(),
            metas.snapshot.as_ref(),
            metas.targets.as_ref(),
            now,
        )?;
        Ok(())
    }

    /// Ensures every director target is mirrored by the config repository metadata.
    fn verify_target_parity(
        &self,
        config_metas: &ConfigMetas,
        director_metas: &DirectorMetas,
    ) -> Result<BTreeMap<String, TargetDescription>> {
        // Load the latest config and director targets documents, tolerating cached copies when
        // the backend omits fields the snapshot already signed off on.
        let config_tree = self.config_tree()?;
        let config_targets = self.collect_config_targets(&config_tree, config_metas)?;
        let director_targets = self.load_director_targets_map(director_metas)?;
        self.verify_target_sets(&config_targets, &director_targets)?;
        Ok(director_targets)
    }

    /// Loads the director repository targets metadata into memory, reusing cached blobs when needed.
    fn load_director_targets_map(
        &self,
        metas: &DirectorMetas,
    ) -> Result<BTreeMap<String, TargetDescription>> {
        let tree = self.director_tree()?;
        let director_targets_bytes =
            self.resolve_meta_bytes(&tree, META_TARGETS, metas.targets.as_ref(), "targets", None)?;
        parse_targets_map_from_bytes(&director_targets_bytes).map_err(UptaneError::from)
    }

    /// Compares the config and director targets maps, ensuring metadata parity.
    fn verify_target_sets(
        &self,
        config_targets: &BTreeMap<String, TargetDescription>,
        director_targets: &BTreeMap<String, TargetDescription>,
    ) -> Result<()> {
        for (path, director_meta) in director_targets {
            // Branch 1: director references a path the config repo never published.
            let Some(config_meta) = config_targets.get(path) else {
                return Err(UptaneError::TargetMissingInConfig { path: path.clone() });
            };
            // Branch 2: metadata lengths diverge, so reject before touching disk.
            if director_meta.length != config_meta.length {
                return Err(UptaneError::TargetLengthMismatch {
                    path: path.clone(),
                    director_length: director_meta.length,
                    config_length: config_meta.length,
                });
            }
            let director_hashes = normalised_hashes(&director_meta.hashes);
            let config_hashes = normalised_hashes(&config_meta.hashes);
            for (algorithm, director_hash) in director_hashes.iter() {
                // Branch 3: config repo omitted a hash algorithm.
                let Some(config_hash) = config_hashes.get(algorithm) else {
                    return Err(UptaneError::TargetHashMissing {
                        path: path.clone(),
                        algorithm: algorithm.clone(),
                    });
                };
                // Branch 4: hashes disagree even though the algorithm exists.
                if director_hash != config_hash {
                    return Err(UptaneError::TargetHashMismatch {
                        path: path.clone(),
                        algorithm: algorithm.clone(),
                    });
                }
            }
        }

        if let Some(expected_org_id) = self.org_binding_config().expected_org_id {
            self.verify_org_id_for_targets(director_targets, expected_org_id)?;
        }
        Ok(())
    }

    /// Ensures every target path embeds the expected org ID when configured.
    fn verify_org_id_for_targets(
        &self,
        director_targets: &BTreeMap<String, TargetDescription>,
        expected_org_id: u64,
    ) -> Result<()> {
        for path in director_targets.keys() {
            if let Some(path_info) = parse_config_path_opt(path) {
                // Employee configs omit org IDs by design; skip them.
                if matches!(path_info.source, ConfigPathSource::Employee) {
                    continue;
                }
                match path_info.org_id {
                    Some(org_id) if org_id == expected_org_id => {}
                    Some(other) => {
                        tracing::warn!(
                            target_path = %path,
                            expected_org_id,
                            actual_org_id = other,
                            "uptane: target org id mismatch"
                        );
                        return Err(UptaneError::OrgIdMismatch {
                            path: path.clone(),
                            expected: expected_org_id,
                            actual: other,
                        });
                    }
                    None => {
                        tracing::warn!(
                            target_path = %path,
                            expected_org_id,
                            "uptane: target missing org id"
                        );
                        return Err(UptaneError::OrgIdMismatch {
                            path: path.clone(),
                            expected: expected_org_id,
                            actual: 0,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Applies org binding rules against the provided snapshot binding.
    fn verify_org_binding(
        &self,
        binding: SnapshotOrgBinding,
        current_root_version: u64,
        incoming_root_version: u64,
    ) -> Result<OrgBindingDecision> {
        let cfg = self.org_binding_config();
        let stored_binding = self.latest_org_binding()?;
        match binding {
            SnapshotOrgBinding::Missing => {
                if let Some(expected) = cfg.expected_uuid.as_ref() {
                    tracing::warn!(
                        expected_uuid = %expected,
                        "uptane: snapshot missing org uuid while expectation is configured"
                    );
                    return Err(UptaneError::OrgUuidMissing {
                        expected: expected.clone(),
                    });
                }
                Ok(OrgBindingDecision::default())
            }
            SnapshotOrgBinding::Present(snapshot_uuid) => {
                if let Some(expected) = cfg.expected_uuid.as_ref() {
                    if snapshot_uuid != *expected {
                        tracing::warn!(
                            expected_uuid = %expected,
                            snapshot_uuid = %snapshot_uuid,
                            "uptane: snapshot org uuid mismatch"
                        );
                        return Err(UptaneError::OrgUuidMismatch {
                            stored: expected.clone(),
                            snapshot: snapshot_uuid,
                        });
                    }
                    return Ok(OrgBindingDecision::default());
                }
                if let Some((_, stored_uuid)) = stored_binding {
                    if snapshot_uuid != stored_uuid {
                        if cfg.auto_store_snapshot_uuid
                            && incoming_root_version > current_root_version
                        {
                            tracing::info!(
                                snapshot_uuid = %snapshot_uuid,
                                previous_uuid = %stored_uuid,
                                incoming_root_version,
                                current_root_version,
                                "uptane: adopting snapshot org uuid after root rotation"
                            );
                            return Ok(OrgBindingDecision {
                                persist_uuid: Some(snapshot_uuid),
                            });
                        }
                        tracing::warn!(
                            stored_uuid = %stored_uuid,
                            snapshot_uuid = %snapshot_uuid,
                            current_root_version,
                            incoming_root_version,
                            "uptane: snapshot org uuid mismatch"
                        );
                        return Err(UptaneError::OrgUuidMismatch {
                            stored: stored_uuid,
                            snapshot: snapshot_uuid,
                        });
                    }
                    return Ok(OrgBindingDecision::default());
                }
                if cfg.auto_store_snapshot_uuid {
                    tracing::info!(
                        snapshot_uuid = %snapshot_uuid,
                        current_root_version,
                        "uptane: learning snapshot org uuid for this cache"
                    );
                    return Ok(OrgBindingDecision {
                        persist_uuid: Some(snapshot_uuid),
                    });
                }
                Ok(OrgBindingDecision::default())
            }
        }
    }

    /// Builds the complete target file list by combining provided payloads with cached copies.
    fn build_complete_target_payloads(
        &self,
        target_tree: &RcTree,
        director_targets: &BTreeMap<String, TargetDescription>,
        files: &[remote_config_proto::remoteconfig::File],
    ) -> Result<Vec<remote_config_proto::remoteconfig::File>> {
        use std::collections::HashSet;
        use tracing::debug;

        let mut complete = Vec::new();
        let mut provided_paths = HashSet::new();

        for file in files {
            let logical_path_cow = logical_target_path(&file.path);
            let logical_path = logical_path_cow.as_ref();
            // Branch 1: backend sent a payload whose path is not in the director metadata.
            let Some(meta) = director_targets.get(logical_path) else {
                return Err(UptaneError::UnexpectedTargetPayload {
                    path: logical_path.to_string(),
                });
            };
            validate_payload_bytes(logical_path, meta, &file.raw)?;
            provided_paths.insert(logical_path.to_string());
            complete.push(remote_config_proto::remoteconfig::File {
                path: logical_path.to_string(),
                raw: file.raw.clone(),
            });
        }

        for (path, meta) in director_targets {
            if provided_paths.contains(path) {
                continue;
            }
            // Branch 2: backend omitted the payload; reuse the cached copy (legacy keys included).
            let mut cached = self.load_cached_target_payload(target_tree, path)?;
            if cached.is_none() {
                if let Some(legacy_path) = legacy_physical_target_path(path, meta) {
                    cached = self.load_cached_target_payload(target_tree, &legacy_path)?;
                    if cached.is_some() {
                        debug!(
                            logical_path = %path,
                            legacy_path = %legacy_path,
                            "uptane: migrating legacy hashed target path"
                        );
                    }
                }
            }
            let bytes =
                cached.ok_or_else(|| UptaneError::MissingTargetPayload { path: path.clone() })?;
            validate_payload_bytes(path, meta, &bytes)?;
            complete.push(remote_config_proto::remoteconfig::File {
                path: path.clone(),
                raw: bytes,
            });
        }

        Ok(complete)
    }

    /// Aggregates the configuration repository targets, including delegated roles stored on disk.
    fn collect_config_targets(
        &self,
        tree: &RcTree,
        metas: &ConfigMetas,
    ) -> Result<BTreeMap<String, TargetDescription>> {
        let mut combined = BTreeMap::new();
        let top_targets_bytes = self.resolve_meta_bytes(
            tree,
            META_TARGETS,
            metas.top_targets.as_ref(),
            "targets",
            None,
        )?;
        merge_target_maps(
            &mut combined,
            parse_targets_map_from_bytes(&top_targets_bytes).map_err(UptaneError::from)?,
        );

        for delegated in &metas.delegated_targets {
            merge_target_maps(
                &mut combined,
                parse_targets_map_from_bytes(&delegated.raw).map_err(UptaneError::from)?,
            );
        }

        let cached = tree.scan_prefix(CONFIG_DELEGATED_PREFIX.as_bytes())?;
        for (_, bytes) in cached {
            merge_target_maps(
                &mut combined,
                parse_targets_map_from_bytes(&bytes).map_err(UptaneError::from)?,
            );
        }

        Ok(combined)
    }

    /// Constructs a `rust-tuf` database pre-loaded with the stored metadata set.
    fn build_database(&self, tree: &RcTree, now: DateTime<Utc>) -> Result<Database<Pouf1>> {
        let stored = self.load_metadata_set(tree)?;
        Database::from_trusted_metadata_with_start_time(&stored, &now).map_err(UptaneError::from)
    }

    /// Loads the trusted root/timestamp/snapshot/targets set from sled for verification seeding.
    fn load_metadata_set(&self, tree: &RcTree) -> Result<RawSignedMetadataSet<Pouf1>> {
        let root_bytes = tree
            .get(META_ROOT.as_bytes())?
            .ok_or(UptaneError::Store(StoreError::MissingMetadata))?;
        let mut builder =
            RawSignedMetadataSetBuilder::<Pouf1>::new().root(RawSignedMetadata::new(root_bytes));
        if let Some(bytes) = tree.get(META_TIMESTAMP.as_bytes())? {
            // Timestamp is optional when the store has just been seeded.
            builder = builder.timestamp(RawSignedMetadata::new(bytes));
        }
        if let Some(bytes) = tree.get(META_SNAPSHOT.as_bytes())? {
            // Snapshot may be absent when the agent has not completed its first refresh.
            builder = builder.snapshot(RawSignedMetadata::new(bytes));
        }
        if let Some(bytes) = tree.get(META_TARGETS.as_bytes())? {
            // Targets will only exist after the first successful refresh.
            builder = builder.targets(RawSignedMetadata::new(bytes));
        }
        Ok(builder.build())
    }

    /// Applies any newer root metadata blobs shipped in the response.
    fn apply_root_chain(&self, db: &mut Database<Pouf1>, roots: &[TopMeta]) -> Result<()> {
        if roots.is_empty() {
            // Nothing to update, so keep trusting the current root.
            return Ok(());
        }
        let mut sorted = roots.to_vec();
        sorted.sort_by_key(|meta| meta.version);
        for meta in sorted {
            let current = db.trusted_root().version();
            let new_version = u32::try_from(meta.version).map_err(|_| {
                UptaneError::Tuf(TufError::MetadataVersionMustBeSmallerThanMaxU32(
                    MetadataPath::root(),
                ))
            })?;
            if new_version <= current {
                // Ignore stale roots that were already trusted.
                continue;
            }
            db.update_root(&RawSignedMetadata::new(meta.raw.clone()))
                .map_err(UptaneError::from)?;
        }
        Ok(())
    }

    /// Verifies timestamp, snapshot, and targets metadata with rust-tuf and returns snapshot versions.
    fn verify_top_level_metadata(
        &self,
        tree: &RcTree,
        db: &mut Database<Pouf1>,
        timestamp: Option<&TopMeta>,
        snapshot: Option<&TopMeta>,
        targets: Option<&TopMeta>,
        now: DateTime<Utc>,
    ) -> Result<BTreeMap<String, SnapshotMetaEntry>> {
        let timestamp_bytes =
            self.resolve_meta_bytes(tree, META_TIMESTAMP, timestamp, "timestamp", None)?;
        let snapshot_bytes =
            self.resolve_meta_bytes(tree, META_SNAPSHOT, snapshot, "snapshot", None)?;
        let versions = snapshot_expected_versions(&snapshot_bytes)?;
        let targets_expected = versions.get(META_TARGETS).map(|entry| entry.version);
        let targets_bytes =
            self.resolve_meta_bytes(tree, META_TARGETS, targets, "targets", targets_expected)?;
        let set = RawSignedMetadataSetBuilder::<Pouf1>::new()
            .timestamp(RawSignedMetadata::new(timestamp_bytes))
            .snapshot(RawSignedMetadata::new(snapshot_bytes))
            .targets(RawSignedMetadata::new(targets_bytes))
            .build();
        db.update_metadata_with_start_time(&set, &now)
            .map_err(UptaneError::from)?;
        Ok(versions)
    }

    /// Returns the raw bytes for the provided metadata, falling back to cached copies when absent.
    ///
    /// Mirrors the Go agent behaviour: partial responses may omit timestamp/snapshot/targets when
    /// nothing changed, so we reuse the previously verified blob stored on disk. On cold starts we
    /// still require the backend to send every field so the cache cannot be populated with
    /// unauthenticated data.
    fn resolve_meta_bytes(
        &self,
        tree: &RcTree,
        key: &str,
        incoming: Option<&TopMeta>,
        field: &'static str,
        expected_version: Option<u64>,
    ) -> Result<Vec<u8>> {
        if let Some(meta) = incoming {
            return Ok(meta.raw.clone());
        }
        let Some(bytes) = tree.get(key.as_bytes())? else {
            // Cold-starts still require the backend to provide all metadata fields.
            return Err(UptaneError::MissingField(field));
        };
        if let Some(version) = expected_version {
            let cached = meta_version(&bytes);
            if cached != version {
                // Snapshot announced a different version, so cached data is stale.
                return Err(UptaneError::MissingField(field));
            }
        }
        Ok(bytes.to_vec())
    }

    /// Ensures delegated metadata referenced by the snapshot is available in the verification DB.
    ///
    /// Go reuses cached delegated targets when the backend omits a role or replays an older
    /// version. We mirror that behaviour by checking the snapshot's expected versions and
    /// hydrating rust-tuf with either the provided blob or the cached copy when it is still valid.
    fn verify_delegations(
        &self,
        tree: &RcTree,
        db: &mut Database<Pouf1>,
        metas: &[DelegatedMeta],
        snapshot_versions: &BTreeMap<String, SnapshotMetaEntry>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        use tracing::{debug, warn};

        let delegated_expectations: BTreeMap<String, u64> = snapshot_versions
            .iter()
            .filter_map(|(name, entry)| {
                if TOP_LEVEL_META_FILES.contains(&name.as_str()) {
                    return None;
                }
                let role = name
                    .trim_end_matches(".json")
                    .trim_start_matches(CONFIG_DELEGATED_PREFIX)
                    .to_string();
                Some((role, entry.version))
            })
            .collect();

        let mut provided_roles: HashSet<String> = HashSet::new();
        let targets_path = MetadataPath::targets();

        for meta in metas {
            provided_roles.insert(meta.role.clone());
            if let Some(expected_version) = delegated_expectations.get(&meta.role) {
                let provided_version = meta_version(&meta.raw);
                if provided_version != *expected_version {
                    if self.try_use_cached_delegation(
                        tree,
                        db,
                        &targets_path,
                        &meta.role,
                        *expected_version,
                        now,
                    )? {
                        warn!(
                            role = %meta.role,
                            expected_version,
                            provided_version,
                            "uptane: backend returned stale delegated metadata; reusing cached copy"
                        );
                        continue;
                    }
                }
            }
            self.apply_delegated_metadata(db, &targets_path, &meta.role, meta.raw.clone(), now)?;
        }

        for (role, expected_version) in delegated_expectations {
            if provided_roles.contains(&role) {
                continue;
            }
            if self.try_use_cached_delegation(
                tree,
                db,
                &targets_path,
                &role,
                expected_version,
                now,
            )? {
                continue;
            }
            debug!(
                role = %role,
                expected_version,
                "uptane: snapshot references delegated role without backend data or cached copy; deferring verification"
            );
        }
        Ok(())
    }

    /// Inserts delegated metadata into the rust-tuf database under the `targets` parent.
    fn apply_delegated_metadata(
        &self,
        db: &mut Database<Pouf1>,
        parent: &MetadataPath,
        role: &str,
        bytes: Vec<u8>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let metadata_path = MetadataPath::new(role.to_string()).map_err(UptaneError::from)?;
        db.update_delegated_targets(&now, parent, &metadata_path, &RawSignedMetadata::new(bytes))
            .map_err(UptaneError::from)
            .map(|_| ())
    }

    /// Attempts to satisfy a delegated role from the sled cache when the backend omitted it.
    fn try_use_cached_delegation(
        &self,
        tree: &RcTree,
        db: &mut Database<Pouf1>,
        parent: &MetadataPath,
        role: &str,
        expected_version: u64,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let Some(bytes) = self.load_cached_delegation(tree, role)? else {
            return Ok(false);
        };
        if meta_version(&bytes) != expected_version {
            return Ok(false);
        }
        self.apply_delegated_metadata(db, parent, role, bytes, now)?;
        Ok(true)
    }

    /// Loads a previously stored delegated metadata blob for the provided role.
    fn load_cached_delegation(&self, tree: &RcTree, role: &str) -> Result<Option<Vec<u8>>> {
        let key = delegated_storage_key(role);
        tree.get(key.as_bytes()).map_err(Into::into)
    }

    /// Loads a cached target payload by its logical path.
    fn load_cached_target_payload(&self, tree: &RcTree, path: &str) -> Result<Option<Vec<u8>>> {
        let key = target_key(path);
        tree.get(&key).map_err(Into::into)
    }

    /// Writes configuration repository metadata and delegated targets into the store.
    /// Returns the current aggregated targets metadata as JSON.
    pub fn director_targets(&self) -> Result<Value> {
        load_json(&self.director_tree()?, META_TARGETS)
    }

    /// Returns the raw director `targets.json`.
    pub fn director_targets_raw(&self) -> Result<Vec<u8>> {
        self.director_tree()?
            .get(META_TARGETS.as_bytes())
            .map_err(Into::into)
            .and_then(not_found_to_err)
    }

    /// Returns the raw config `targets.json`.
    pub fn config_targets_raw(&self) -> Result<Vec<u8>> {
        self.config_tree()?
            .get(META_TARGETS.as_bytes())
            .map_err(Into::into)
            .and_then(not_found_to_err)
    }

    /// Returns the raw director `root.json` blob when available.
    pub fn director_root_raw(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.director_tree()?.get(META_ROOT.as_bytes())?)
    }

    /// Returns the sequence of director roots newer than `current_version` up to `target_version`.
    pub fn director_roots_since(
        &self,
        current_version: u64,
        target_version: u64,
    ) -> Result<Vec<Vec<u8>>> {
        if target_version <= current_version {
            return Ok(Vec::new());
        }
        let tree = self.director_tree()?;
        let mut roots = Vec::new();
        for version in (current_version + 1)..=target_version {
            let key = root_version_key(version);
            let bytes = tree
                .get(key.as_bytes())
                .map_err(Into::into)
                .and_then(not_found_to_err)?;
            roots.push(bytes);
        }
        Ok(roots)
    }

    /// Returns the raw configuration `root.json` blob when available.
    pub fn config_root_raw(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.config_tree()?.get(META_ROOT.as_bytes())?)
    }

    /// Returns the organisation UUID embedded in the config snapshot metadata (if any).
    pub fn snapshot_org_uuid(&self) -> Result<Option<String>> {
        Ok(self.snapshot_org_binding()?.into_option())
    }

    /// Returns the snapshot org binding state (missing vs. present).
    pub fn snapshot_org_binding(&self) -> Result<SnapshotOrgBinding> {
        // Access the configuration repository tree; failure propagates as a store error.
        let tree = self.config_tree()?;
        let Some(bytes) = tree.get(META_SNAPSHOT.as_bytes())? else {
            // No snapshot stored yet (cold cache), so there is no org binding to validate.
            return Ok(SnapshotOrgBinding::Missing);
        };
        parse_snapshot_org_binding(&bytes)
    }

    /// Resolves the org binding embedded in the provided config metadata snapshot (incoming or cached).
    fn snapshot_binding_from_metas(&self, metas: &ConfigMetas) -> Result<SnapshotOrgBinding> {
        let tree = self.config_tree()?;
        let snapshot_bytes = self.resolve_meta_bytes(
            &tree,
            META_SNAPSHOT,
            metas.snapshot.as_ref(),
            "snapshot",
            None,
        )?;
        parse_snapshot_org_binding(&snapshot_bytes)
    }

    /// Enumerates all cached target files along with their payloads.
    pub fn all_target_files(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let mut files = Vec::new();
        for (key, value) in self.target_tree()?.entries()? {
            let path = String::from_utf8_lossy(&key).to_string();
            if let Some(stripped) = path.strip_prefix(TARGET_PREFIX) {
                files.push((stripped.to_string(), value));
            }
        }
        Ok(files)
    }

    /// Returns the current timestamp expiry (if available).
    pub fn timestamp_expires(&self) -> Result<Option<OffsetDateTime>> {
        let tree = self.director_tree()?;
        match tree.get(META_TIMESTAMP.as_bytes())? {
            Some(raw) => {
                let value: Value = serde_json::from_slice(&raw)?;
                if let Some(expires) = value.pointer("/signed/expires").and_then(Value::as_str) {
                    // Convert the RFC3339 string into a strongly typed timestamp for comparisons.
                    let parsed = OffsetDateTime::parse(expires, &Rfc3339)?;
                    Ok(Some(parsed))
                } else {
                    Ok(None)
                }
            }
            None => Ok(None),
        }
    }

    /// Returns the current Uptane versions overview.
    pub fn tuf_versions(&self) -> Result<TufVersions> {
        let config_tree = self.config_tree()?;
        let director_tree = self.director_tree()?;

        let director_root = tree_get(&director_tree, META_ROOT)?;
        let director_targets = tree_get(&director_tree, META_TARGETS)?;
        let config_root = tree_get(&config_tree, META_ROOT)?;
        let config_snapshot = tree_get(&config_tree, META_SNAPSHOT)?;

        Ok(TufVersions {
            director_root: meta_version(&director_root),
            director_targets: meta_version(&director_targets),
            config_root: meta_version(&config_root),
            config_snapshot: meta_version(&config_snapshot),
        })
    }

    /// Returns the Uptane state summary used for diagnostics.
    pub fn state(&self) -> Result<State> {
        let mut config = BTreeMap::new();
        for (key, value) in self.config_tree()?.entries()? {
            let key_str = String::from_utf8_lossy(&key).to_string();
            // Each entry is summarised into a MetaState to mirror the Go diagnostic output.
            config.insert(key_str, MetaState::from_bytes(&value)?);
        }

        let mut director = BTreeMap::new();
        for (key, value) in self.director_tree()?.entries()? {
            let key_str = String::from_utf8_lossy(&key).to_string();
            director.insert(key_str, MetaState::from_bytes(&value)?);
        }

        let mut targets = BTreeMap::new();
        for (key, value) in self.target_tree()?.entries()? {
            let path = String::from_utf8_lossy(&key).to_string();
            targets.insert(path, meta_hash(&value));
        }

        Ok(State {
            config,
            director,
            target_filenames: targets,
        })
    }

    /// Fetches raw target file contents by TUF path.
    pub fn target_file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let key = target_key(path);
        Ok(self.target_tree()?.get(&key)?)
    }

    /// Flushes pending writes to disk, useful during shutdown.
    pub fn flush(&self) -> Result<()> {
        self.store.flush()?;
        Ok(())
    }
}

/// Prepares a sled batch that updates the config metadata tree.
fn build_config_batch(meta: &remote_config_proto::remoteconfig::ConfigMetas) -> Batch {
    let mut batch = Batch::default();
    if !meta.roots.is_empty() {
        insert_root_versions(&mut batch, &meta.roots, META_ROOT);
    }
    if let Some(timestamp) = &meta.timestamp {
        insert_meta(&mut batch, META_TIMESTAMP, timestamp);
    }
    if let Some(snapshot) = &meta.snapshot {
        insert_meta(&mut batch, META_SNAPSHOT, snapshot);
    }
    if let Some(targets) = &meta.top_targets {
        insert_meta(&mut batch, META_TARGETS, targets);
    }
    for delegated in &meta.delegated_targets {
        // Delegated roles are stored under their role name, mirroring the Go implementation.
        let key = delegated_storage_key(&delegated.role);
        insert_delegated(&mut batch, &key, delegated);
    }
    batch
}

/// Prepares a sled batch that updates the director metadata tree.
fn build_director_batch(meta: &remote_config_proto::remoteconfig::DirectorMetas) -> Batch {
    let mut batch = Batch::default();
    if !meta.roots.is_empty() {
        insert_root_versions(&mut batch, &meta.roots, META_ROOT);
    }
    if let Some(timestamp) = &meta.timestamp {
        insert_meta(&mut batch, META_TIMESTAMP, timestamp);
    }
    if let Some(snapshot) = &meta.snapshot {
        insert_meta(&mut batch, META_SNAPSHOT, snapshot);
    }
    if let Some(targets) = &meta.targets {
        insert_meta(&mut batch, META_TARGETS, targets);
    }
    batch
}

/// Builds a sled batch that writes the provided targets and removes stale ones.
fn build_target_batch(
    tree: &RcTree,
    files: &[remote_config_proto::remoteconfig::File],
) -> Result<Batch> {
    use tracing::debug;

    debug!(
        "uptane: Storing {} target_files in sled database",
        files.len()
    );

    let mut batch = Batch::default();
    let mut seen_paths: HashSet<Vec<u8>> = HashSet::new();

    for (idx, file) in files.iter().enumerate() {
        let key = target_key(&file.path);
        debug!(
            index = idx,
            target_path = %file.path,
            payload_size = file.raw.len(),
            key = ?key,
            "uptane: storing target file"
        );
        let key_copy = key.clone();
        batch.insert(key, file.raw.as_slice());
        seen_paths.insert(key_copy);
    }

    tree.for_each_key(|existing| {
        if !seen_paths.contains(existing) {
            batch.remove(existing);
        }
        ControlFlow::<()>::Continue(())
    })?;

    debug!("uptane: Successfully prepared target batch");
    Ok(batch)
}

/// Collection of key Uptane version numbers for diagnostics and refresh logic.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TufVersions {
    /// Version of the director root metadata.
    pub director_root: u64,
    /// Version of the director targets metadata.
    pub director_targets: u64,
    /// Version of the configuration root metadata.
    pub config_root: u64,
    /// Version of the configuration snapshot metadata.
    pub config_snapshot: u64,
}

/// Metadata state summary containing version and hash information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetaState {
    /// Metadata version number.
    pub version: u64,
    /// SHA-256 hash of the metadata contents.
    pub hash: String,
}

impl MetaState {
    /// Builds a [`MetaState`] from raw metadata bytes.
    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Ok(Self {
            version: meta_version(bytes),
            hash: meta_hash(bytes),
        })
    }
}

/// Snapshot of the configuration and director repositories plus target hashes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// Metadata entries from the configuration repository.
    pub config: BTreeMap<String, MetaState>,
    /// Metadata entries from the director repository.
    pub director: BTreeMap<String, MetaState>,
    /// Mapping of target file paths to their hashes.
    pub target_filenames: BTreeMap<String, String>,
}

/// Inserts all provided root versions and updates the latest root slot.
fn insert_root_versions(batch: &mut Batch, roots: &[TopMeta], latest_key: &str) {
    for root in roots {
        let key = root_version_key(root.version);
        batch.insert(key.as_bytes(), root.raw.as_slice());
    }
    if let Some(last_root) = roots.last() {
        batch.insert(latest_key.as_bytes(), last_root.raw.as_slice());
    }
}

/// Inserts a top-level metadata blob into the batch under its canonical filename.
fn insert_meta(batch: &mut Batch, name: &str, meta: &TopMeta) {
    // Mirror the Go agent: top-level metadata is stored under its canonical filename.
    batch.insert(name.as_bytes(), meta.raw.as_slice());
}

/// Inserts delegated metadata into the batch using the provided key.
fn insert_delegated(batch: &mut Batch, key: &str, meta: &DelegatedMeta) {
    // Delegated roles are addressed using their role name.
    batch.insert(key.as_bytes(), meta.raw.as_slice());
}

/// Returns the sled key used to store delegated metadata for a role.
fn delegated_storage_key(role: &str) -> String {
    format!("{CONFIG_DELEGATED_PREFIX}{role}.json")
}

/// Returns the sled key used to store a target file.
fn target_key(path: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(TARGET_PREFIX.len() + path.len());
    key.extend_from_slice(TARGET_PREFIX.as_bytes());
    key.extend_from_slice(path.as_bytes());
    key
}

/// Normalises backend target paths by stripping the `{hash}.` prefix the CDN adds.
fn logical_target_path(path: &str) -> Cow<'_, str> {
    let (dir, file) = split_dir_and_file(path);
    if let Some((prefix, suffix)) = file.split_once('.') {
        if is_hex_hash(prefix) && !suffix.is_empty() {
            let mut normalised = String::with_capacity(dir.len() + suffix.len());
            normalised.push_str(dir);
            normalised.push_str(suffix);
            return Cow::Owned(normalised);
        }
    }
    Cow::Borrowed(path)
}

/// Computes the legacy sled key used by historical agents that stored `{hash}.{file}` paths.
fn legacy_physical_target_path(logical_path: &str, meta: &TargetDescription) -> Option<String> {
    let hash = meta.hashes.get("sha256")?;
    let (dir, file) = split_dir_and_file(logical_path);
    let mut physical = String::with_capacity(dir.len() + hash.len() + 1 + file.len());
    physical.push_str(dir);
    physical.push_str(hash);
    physical.push('.');
    physical.push_str(file);
    Some(physical)
}

/// Splits a path into `directory/` (with trailing slash when present) and `file` segments.
fn split_dir_and_file(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(idx) => (&path[..=idx], &path[idx + 1..]),
        None => ("", path),
    }
}

/// Returns `true` when the supplied input looks like a 256-bit hexadecimal digest.
fn is_hex_hash(input: &str) -> bool {
    input.len() == 64 && input.chars().all(|c| c.is_ascii_hexdigit())
}

/// Extracts the version number from a TUF metadata document.
fn meta_version(bytes: &[u8]) -> u64 {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|value| value.pointer("/signed/version").and_then(Value::as_u64))
        .unwrap_or_default()
}

/// Computes the SHA-256 digest for a metadata blob.
fn meta_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(hasher.finalize())
}

/// Returns the sled key used to store a versioned root.
fn root_version_key(version: u64) -> String {
    format!("root/{version}.json")
}

/// Fetches a mandatory value from a sled tree.
fn tree_get(tree: &RcTree, key: &str) -> Result<Vec<u8>> {
    tree.get(key.as_bytes())
        .map_err(Into::into)
        .and_then(not_found_to_err)
}

/// Converts an optional sled value to a result, raising an error when missing.
fn not_found_to_err(opt: Option<Vec<u8>>) -> Result<Vec<u8>> {
    opt.ok_or_else(|| UptaneError::Store(StoreError::MissingMetadata))
}

/// Loads a JSON document from the specified tree and key.
fn load_json(tree: &RcTree, key: &str) -> Result<Value> {
    let bytes = tree
        .get(key.as_bytes())
        .map_err(Into::into)
        .and_then(not_found_to_err)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Parses the snapshot metadata to discover the versions of referenced files.
fn snapshot_expected_versions(bytes: &[u8]) -> Result<BTreeMap<String, SnapshotMetaEntry>> {
    let document: SnapshotMetaSignedDocument = serde_json::from_slice(bytes)?;
    Ok(document.signed.meta)
}

/// Extends `dest` with every entry contained in `src`, overriding duplicates.
fn merge_target_maps(
    dest: &mut BTreeMap<String, TargetDescription>,
    src: BTreeMap<String, TargetDescription>,
) {
    for (path, entry) in src {
        dest.insert(path, entry);
    }
}

/// Validates the payload bytes against the advertised metadata entry.
fn validate_payload_bytes(path: &str, meta: &TargetDescription, bytes: &[u8]) -> Result<()> {
    let actual_len = bytes.len() as i64;
    if actual_len != meta.length {
        return Err(UptaneError::TargetPayloadLengthMismatch {
            path: path.to_string(),
            expected: meta.length,
            actual: actual_len,
        });
    }
    for (algorithm, expected_hash) in &meta.hashes {
        let computed = compute_payload_hash(algorithm, bytes)?;
        if expected_hash != &computed {
            return Err(UptaneError::TargetPayloadHashMismatch {
                path: path.to_string(),
                algorithm: algorithm.clone(),
            });
        }
    }
    Ok(())
}

/// Normalises hash maps so algorithm names compare case-insensitively.
fn normalised_hashes(hashes: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    hashes
        .iter()
        .map(|(algo, value)| (algo.to_ascii_lowercase(), value.clone()))
        .collect()
}

/// Computes a lowercase-hex digest for the requested algorithm.
fn compute_payload_hash(algorithm: &str, bytes: &[u8]) -> Result<String> {
    match algorithm.to_ascii_lowercase().as_str() {
        "sha256" => {
            let mut hasher = Sha256::new();
            hasher.update(bytes);
            Ok(hex_encode(hasher.finalize()))
        }
        _ => Err(UptaneError::UnsupportedHashAlgorithm {
            algorithm: algorithm.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remote_config_proto::remoteconfig::{
        ConfigMetas, DirectorMetas, File, LatestConfigsResponse, TopMeta,
    };
    use serde_json::json;
    use std::collections::HashMap;
    use tempfile::TempDir;

    mod tuf_builder {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/common/tuf.rs"));
    }
    use tuf_builder::{
        build_signed_repo, build_signed_repo_with_delegations, DelegationFixture, TargetData,
    };

    /// Helper that toggles the global verification flag for the duration of a test.
    struct VerificationGuard {
        previous: bool,
    }

    impl VerificationGuard {
        /// Enables verification and remembers the previous flag value.
        fn enable() -> Self {
            let previous = TUF_VERIFICATION_ENABLED.with(|flag| {
                let prev = flag.get();
                flag.set(true);
                prev
            });
            Self { previous }
        }
    }

    impl Drop for VerificationGuard {
        /// Restores the previous verification flag when the guard goes out of scope.
        fn drop(&mut self) {
            // Restore whichever mode (enabled/disabled) the caller had configured before.
            TUF_VERIFICATION_ENABLED.with(|flag| flag.set(self.previous));
        }
    }

    /// Creates an in-memory store for testing purposes.
    fn test_store() -> RcStore {
        let tmp = TempDir::new().unwrap();
        RcStore::open(
            tmp.path().join("rc.db"),
            "7.60.0",
            "test_api_key",
            "https://config.datadoghq.com",
        )
        .unwrap()
    }

    /// Seeds both repositories with the first root delivered in the response so verification can run.
    fn seed_store_with_initial_roots(uptane: &UptaneState, response: &LatestConfigsResponse) {
        if let Some(root) = response
            .config_metas
            .as_ref()
            .and_then(|metas| metas.roots.first())
        {
            let tree = uptane.config_tree().unwrap();
            uptane.seed_root(tree, &root.raw).unwrap();
        }
        if let Some(root) = response
            .director_metas
            .as_ref()
            .and_then(|metas| metas.roots.first())
        {
            let tree = uptane.director_tree().unwrap();
            uptane.seed_root(tree, &root.raw).unwrap();
        }
    }

    /// Helper to craft a metadata document with the specified type and version.
    fn meta_json(meta_type: &str, version: u64) -> Vec<u8> {
        json!({
            "signed": {
                "_type": meta_type,
                "version": version,
                "expires": "2030-01-01T00:00:00Z",
                "targets": {
                    "pkg1": {
                        "length": 123,
                        "hashes": { "sha256": "abc" },
                        "custom": {}
                    }
                }
            },
            "signatures": []
        })
        .to_string()
        .into_bytes()
    }

    /// Helper to craft a timestamp document.
    fn timestamp_json(version: u64, expires: &str) -> Vec<u8> {
        json!({
            "signed": {
                "_type": "timestamp",
                "version": version,
                "expires": expires
            },
            "signatures": []
        })
        .to_string()
        .into_bytes()
    }

    /// Wraps the root/timestamp/snapshot helpers into a [`TopMeta`] value.
    fn mk_top_meta(meta_type: &str, version: u64) -> TopMeta {
        TopMeta {
            version,
            raw: meta_json(meta_type, version),
        }
    }

    /// Creates a sample `targets.json` metadata document.
    fn mk_targets_meta(version: u64) -> TopMeta {
        let raw = json!({
            "signed": {
                "_type": "targets",
                "spec_version": "1.0",
                "version": version,
                "expires": "2030-01-01T00:00:00Z",
                "targets": {
                    (SAMPLE_TARGET_PATH): {
                        "length": SAMPLE_TARGET_PAYLOAD.len(),
                        "hashes": { "sha256": SAMPLE_TARGET_HASH }
                    }
                }
            },
            "signatures": []
        })
        .to_string()
        .into_bytes();

        TopMeta { version, raw }
    }

    fn rewrite_target_path(meta: &mut TopMeta, old_path: &str, new_path: &str) {
        mutate_targets_map(meta, |targets| {
            if let Some(entry) = targets.remove(old_path) {
                targets.insert(new_path.to_string(), entry);
            }
        });
    }

    /// Embeds an org UUID inside the provided snapshot metadata blob.
    fn set_snapshot_org(meta: &mut TopMeta, org_uuid: &str) {
        let mut document: serde_json::Value =
            serde_json::from_slice(&meta.raw).expect("snapshot metadata valid");
        if let Some(signed) = document
            .get_mut("signed")
            .and_then(serde_json::Value::as_object_mut)
        {
            signed.insert("custom".into(), json!({ "org_uuid": org_uuid }));
        }
        meta.raw = serde_json::to_vec(&document).expect("snapshot metadata serializable");
    }

    /// Rewrites the targets map inside a `TopMeta` value using the provided callback.
    fn mutate_targets_map(
        meta: &mut TopMeta,
        mut action: impl FnMut(&mut serde_json::Map<String, serde_json::Value>),
    ) {
        let mut document: serde_json::Value =
            serde_json::from_slice(&meta.raw).expect("targets metadata must be valid JSON");
        {
            let map = document
                .pointer_mut("/signed/targets")
                .and_then(serde_json::Value::as_object_mut)
                .expect("targets map exists");
            action(map);
        }
        meta.raw = serde_json::to_vec(&document).expect("targets metadata must serialize");
    }

    /// Drops every entry in the targets map.
    fn clear_targets(meta: &mut TopMeta) {
        mutate_targets_map(meta, |targets| targets.clear());
    }

    /// Overwrites the first target's SHA-256 hash with a custom value.
    fn rewrite_first_target_hash(meta: &mut TopMeta, new_hash: &str) {
        mutate_targets_map(meta, |targets| {
            if let Some((_path, entry)) = targets.iter_mut().next() {
                if let Some(hashes) = entry
                    .get_mut("hashes")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    hashes.insert(
                        "sha256".to_string(),
                        serde_json::Value::String(new_hash.to_string()),
                    );
                }
            }
        });
    }

    /// Inserts or replaces a target entry using metadata derived from the payload bytes.
    fn upsert_target_entry(meta: &mut TopMeta, path: &str, payload: &[u8]) {
        let hash = {
            let mut hasher = Sha256::new();
            hasher.update(payload);
            hex_encode(hasher.finalize())
        };
        mutate_targets_map(meta, |targets| {
            targets.insert(
                path.to_string(),
                json!({
                    "length": payload.len(),
                    "hashes": { "sha256": hash }
                }),
            );
        });
    }

    const SAMPLE_TARGET_PATH: &str = "pkg1";
    const SAMPLE_TARGET_PAYLOAD: &[u8; 7] = b"payload";
    const SAMPLE_TARGET_HASH: &str =
        "239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5";

    /// Builds a representative backend response used by the tests.
    fn sample_response() -> LatestConfigsResponse {
        LatestConfigsResponse {
            config_metas: Some(ConfigMetas {
                roots: vec![mk_top_meta("root", 1)],
                timestamp: Some(TopMeta {
                    version: 1,
                    raw: timestamp_json(1, "2030-01-01T00:00:00Z"),
                }),
                snapshot: Some(mk_top_meta("snapshot", 1)),
                top_targets: Some(mk_targets_meta(1)),
                delegated_targets: vec![],
            }),
            director_metas: Some(DirectorMetas {
                roots: vec![mk_top_meta("root", 1)],
                timestamp: Some(TopMeta {
                    version: 1,
                    raw: timestamp_json(1, "2030-01-01T00:00:00Z"),
                }),
                snapshot: Some(mk_top_meta("snapshot", 1)),
                targets: Some(mk_targets_meta(2)),
            }),
            target_files: vec![File {
                path: SAMPLE_TARGET_PATH.to_string(),
                raw: SAMPLE_TARGET_PAYLOAD.to_vec(),
            }],
            ..Default::default()
        }
    }

    const SIGNED_TARGET_PATH: &str = "datadog/12345/APM_TRACING/config/pkg1.json";

    /// Builds a canonical signed response for verification-focused tests.
    fn signed_response(config_version: u64, director_version: u64) -> LatestConfigsResponse {
        let targets = vec![TargetData {
            path: SIGNED_TARGET_PATH,
            payload: b"payload",
            custom: None,
        }];
        let (config_roots, config_timestamp, config_snapshot, config_targets) =
            build_signed_repo(config_version, "2030-01-01T00:00:00Z", &targets, None);
        let (director_roots, director_timestamp, director_snapshot, director_targets) =
            build_signed_repo(director_version, "2030-01-01T00:00:00Z", &targets, None);

        LatestConfigsResponse {
            config_metas: Some(ConfigMetas {
                roots: config_roots,
                timestamp: Some(config_timestamp),
                snapshot: Some(config_snapshot),
                top_targets: Some(config_targets),
                delegated_targets: vec![],
            }),
            director_metas: Some(DirectorMetas {
                roots: director_roots,
                timestamp: Some(director_timestamp),
                snapshot: Some(director_snapshot),
                targets: Some(director_targets),
            }),
            target_files: vec![File {
                path: SIGNED_TARGET_PATH.to_string(),
                raw: b"payload".to_vec(),
            }],
            ..Default::default()
        }
    }

    /// Computes the backend pathname that prefixes the logical file with its SHA-256 hash.
    fn hashed_backend_path(logical_path: &str, payload: &[u8]) -> String {
        let hash = payload_sha256(payload);
        let (dir, file) = split_dir_and_file(logical_path);
        format!("{}{}.{}", dir, hash, file)
    }

    /// Returns the lowercase hexadecimal SHA-256 digest for a payload.
    fn payload_sha256(payload: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(payload);
        hex::encode(hasher.finalize())
    }

    /// Builds a response whose config repository delegates targets to a product-specific role.
    fn delegated_response(config_version: u64, delegated_version: u32) -> LatestConfigsResponse {
        const DELEGATED_PATH: &str = "datadog/12345/LIVE_DEBUGGING/config/live.json";
        let delegated_payload = br#"{ "live_debugging": true }"#;
        let delegated_targets = [TargetData {
            path: DELEGATED_PATH,
            payload: delegated_payload,
            custom: None,
        }];
        let delegated_paths = [DELEGATED_PATH];
        let delegations = [DelegationFixture {
            role: "LIVE_DEBUGGING",
            version: delegated_version,
            paths: &delegated_paths,
            targets: &delegated_targets,
        }];
        let no_top_targets: [TargetData; 0] = [];

        let (config_roots, config_timestamp, config_snapshot, config_targets, delegated_metas) =
            build_signed_repo_with_delegations(
                config_version,
                "2030-01-01T00:00:00Z",
                &no_top_targets,
                &delegations,
            );
        let (director_roots, director_timestamp, director_snapshot, director_targets) =
            build_signed_repo(
                config_version,
                "2030-01-01T00:00:00Z",
                &delegated_targets,
                None,
            );

        LatestConfigsResponse {
            config_metas: Some(ConfigMetas {
                roots: config_roots,
                timestamp: Some(config_timestamp),
                snapshot: Some(config_snapshot),
                top_targets: Some(config_targets),
                delegated_targets: delegated_metas,
            }),
            director_metas: Some(DirectorMetas {
                roots: director_roots,
                timestamp: Some(director_timestamp),
                snapshot: Some(director_snapshot),
                targets: Some(director_targets),
            }),
            target_files: vec![File {
                path: DELEGATED_PATH.to_string(),
                raw: delegated_payload.to_vec(),
            }],
            ..Default::default()
        }
    }

    #[test]
    /// Ensures Uptane updates populate the store and the diagnostic helpers work.
    fn uptane_update_and_state() {
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();
        let response = sample_response();

        uptane.update(&response).unwrap();

        let versions = uptane.tuf_versions().unwrap();
        assert_eq!(
            versions,
            TufVersions {
                director_root: 1,
                director_targets: 2,
                config_root: 1,
                config_snapshot: 1
            }
        );

        let state = uptane.state().unwrap();
        assert!(state.config.contains_key(META_ROOT));
        assert!(state.director.contains_key(META_TARGETS));
        assert!(state
            .target_filenames
            .contains_key(&format!("{TARGET_PREFIX}pkg1")));

        let expires = uptane.timestamp_expires().unwrap().unwrap();
        assert_eq!(
            expires,
            OffsetDateTime::parse("2030-01-01T00:00:00Z", &Rfc3339).unwrap()
        );

        let director_targets = uptane.director_targets().unwrap();
        assert!(director_targets.pointer("/signed/targets/pkg1").is_some());
    }

    #[test]
    /// Snapshot org binding helper reports Missing/Present states.
    fn snapshot_org_binding_reports_presence() {
        let store = test_store();
        let uptane = UptaneState::new(store.clone()).unwrap();

        assert_eq!(
            uptane.snapshot_org_binding().unwrap(),
            SnapshotOrgBinding::Missing
        );

        let tree = uptane.config_tree().unwrap();
        tree.insert(
            META_SNAPSHOT.as_bytes(),
            br#"{"signed":{"custom":{"org_uuid":"org-test"}}}"#,
        )
        .unwrap();
        assert_eq!(
            uptane.snapshot_org_binding().unwrap(),
            SnapshotOrgBinding::Present("org-test".into())
        );

        tree.insert(META_SNAPSHOT.as_bytes(), br#"{"signed":{}}"#)
            .unwrap();
        assert_eq!(
            uptane.snapshot_org_binding().unwrap(),
            SnapshotOrgBinding::Missing
        );
    }

    #[test]
    /// Auto-store behaviour persists the snapshot org UUID when no binding exists.
    fn org_binding_auto_stores_snapshot_uuid() {
        let store = test_store();
        let mut config = UptaneConfig::default();
        config.org_binding.auto_store_snapshot_uuid = true;
        let uptane = UptaneState::with_config(store.clone(), config).unwrap();
        let mut response = signed_response(1, 1);
        if let Some(config_metas) = response.config_metas.as_mut() {
            if let Some(snapshot) = config_metas.snapshot.as_mut() {
                set_snapshot_org(snapshot, "org-auto");
            }
        }
        seed_store_with_initial_roots(&uptane, &response);

        uptane.update(&response).unwrap();

        let binding = uptane
            .latest_org_binding()
            .unwrap()
            .expect("org binding stored");
        assert_eq!(binding.0, 1);
        assert_eq!(binding.1, "org-auto");
    }

    #[test]
    /// Missing snapshot org UUID is rejected when an expectation is configured.
    fn org_binding_rejects_missing_snapshot_uuid_when_expected() {
        let store = test_store();
        let mut config = UptaneConfig::default();
        config.org_binding.expected_uuid = Some("org-required".into());
        config.org_binding.auto_store_snapshot_uuid = false;
        let uptane = UptaneState::with_config(store.clone(), config).unwrap();
        let response = signed_response(1, 1);
        seed_store_with_initial_roots(&uptane, &response);

        let err = uptane.update(&response).unwrap_err();
        match err {
            UptaneError::OrgUuidMissing { expected } => assert_eq!(expected, "org-required"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    /// Snapshot org UUID mismatches are rejected when a binding already exists.
    fn org_binding_rejects_mismatched_snapshot_uuid() {
        let store = test_store();
        let mut config = UptaneConfig::default();
        config.org_binding.auto_store_snapshot_uuid = true;
        let uptane = UptaneState::with_config(store.clone(), config).unwrap();

        // First update stores the baseline org binding.
        let mut initial = signed_response(1, 1);
        if let Some(config_metas) = initial.config_metas.as_mut() {
            if let Some(snapshot) = config_metas.snapshot.as_mut() {
                set_snapshot_org(snapshot, "org-initial");
            }
        }
        seed_store_with_initial_roots(&uptane, &initial);
        uptane.update(&initial).unwrap();

        // Second update advertises a different org UUID, which should be rejected.
        let mut mismatch = signed_response(1, 1);
        if let Some(config_metas) = mismatch.config_metas.as_mut() {
            if let Some(snapshot) = config_metas.snapshot.as_mut() {
                set_snapshot_org(snapshot, "org-other");
            }
        }
        let err = uptane.update(&mismatch).unwrap_err();
        match err {
            UptaneError::OrgUuidMismatch { stored, snapshot } => {
                assert_eq!(stored, "org-initial");
                assert_eq!(snapshot, "org-other");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    /// Org ID enforcement rejects metadata when the path encodes a different org.
    fn org_id_enforcement_rejects_mismatch() {
        let store = test_store();
        let mut cfg = UptaneConfig::default();
        cfg.org_binding.expected_org_id = Some(999);
        let uptane = UptaneState::with_config(store.clone(), cfg).unwrap();
        let response = signed_response(1, 1);
        seed_store_with_initial_roots(&uptane, &response);
        let err = uptane.update(&response).unwrap_err();
        match err {
            UptaneError::OrgIdMismatch {
                path,
                expected,
                actual,
            } => {
                assert_eq!(path, SIGNED_TARGET_PATH);
                assert_eq!(expected, 999);
                assert_eq!(actual, 12345);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    /// Org ID enforcement allows matching paths.
    fn org_id_enforcement_accepts_matching_paths() {
        let store = test_store();
        let mut cfg = UptaneConfig::default();
        cfg.org_binding.expected_org_id = Some(12345);
        let uptane = UptaneState::with_config(store.clone(), cfg).unwrap();
        let response = signed_response(1, 1);
        seed_store_with_initial_roots(&uptane, &response);
        uptane.update(&response).unwrap();
    }

    #[test]
    /// Employee paths bypass org ID enforcement even when configured.
    fn org_id_enforcement_skips_employee_paths() {
        let store = test_store();
        let mut cfg = UptaneConfig::default();
        cfg.org_binding.expected_org_id = Some(999);
        let uptane = UptaneState::with_config(store.clone(), cfg).unwrap();
        let mut response = signed_response(1, 1);
        const EMPLOYEE_PATH: &str = "employee/APM_TRACING/config/pkg1.json";
        if let Some(config_metas) = response.config_metas.as_mut() {
            if let Some(targets) = config_metas.top_targets.as_mut() {
                rewrite_target_path(targets, SIGNED_TARGET_PATH, EMPLOYEE_PATH);
            }
        }
        if let Some(director_metas) = response.director_metas.as_mut() {
            if let Some(targets) = director_metas.targets.as_mut() {
                rewrite_target_path(targets, SIGNED_TARGET_PATH, EMPLOYEE_PATH);
            }
        }
        if let Some(file) = response.target_files.get_mut(0) {
            file.path = EMPLOYEE_PATH.to_string();
        }
        seed_store_with_initial_roots(&uptane, &response);
        uptane.update(&response).unwrap();
    }

    #[test]
    /// Snapshot org UUID can be updated after a root rotation.
    fn org_binding_adopts_new_uuid_after_root_rotation() {
        let store = test_store();
        let mut config = UptaneConfig::default();
        config.org_binding.auto_store_snapshot_uuid = true;
        let uptane = UptaneState::with_config(store.clone(), config).unwrap();

        // Seed binding with the initial root/snapshot pair.
        let mut initial = signed_response(1, 1);
        if let Some(config_metas) = initial.config_metas.as_mut() {
            if let Some(snapshot) = config_metas.snapshot.as_mut() {
                set_snapshot_org(snapshot, "org-initial");
            }
        }
        seed_store_with_initial_roots(&uptane, &initial);
        uptane.update(&initial).unwrap();
        let binding = uptane
            .latest_org_binding()
            .unwrap()
            .expect("initial org binding");
        assert_eq!(binding.0, 1);
        assert_eq!(binding.1, "org-initial");

        // Rotate the root and ensure the new org UUID is accepted.
        let mut rotated = signed_response(2, 2);
        if let Some(config_metas) = rotated.config_metas.as_mut() {
            if let Some(snapshot) = config_metas.snapshot.as_mut() {
                set_snapshot_org(snapshot, "org-rotated");
            }
        }
        uptane.update(&rotated).unwrap();
        let binding = uptane
            .latest_org_binding()
            .unwrap()
            .expect("rotated org binding");
        assert_eq!(binding.0, 2);
        assert_eq!(binding.1, "org-rotated");
    }

    #[test]
    /// Missing snapshot org UUID is tolerated when no expectation is configured.
    fn org_binding_allows_missing_snapshot_uuid_without_expectation() {
        let store = test_store();
        let uptane = UptaneState::new(store.clone()).unwrap();
        let response = signed_response(1, 1);
        seed_store_with_initial_roots(&uptane, &response);
        uptane.update(&response).unwrap();
    }

    #[test]
    /// Snapshot org UUID that matches the expected UUID passes verification.
    fn org_binding_accepts_matching_snapshot_uuid() {
        let store = test_store();
        let mut config = UptaneConfig::default();
        config.org_binding.expected_uuid = Some("org-match".into());
        let uptane = UptaneState::with_config(store.clone(), config).unwrap();
        let mut response = signed_response(1, 1);
        if let Some(config_metas) = response.config_metas.as_mut() {
            if let Some(snapshot) = config_metas.snapshot.as_mut() {
                set_snapshot_org(snapshot, "org-match");
            }
        }
        seed_store_with_initial_roots(&uptane, &response);
        uptane.update(&response).unwrap();
    }

    #[test]
    /// Latest org binding helper surfaces the stored tuple.
    fn latest_org_binding_tracks_updates() {
        let store = test_store();
        let uptane = UptaneState::new(store.clone()).unwrap();
        seed_store_with_initial_roots(&uptane, &sample_response());

        assert!(uptane.latest_org_binding().unwrap().is_none());

        uptane.update_org_uuid("org-initial").unwrap();
        let first = uptane
            .latest_org_binding()
            .unwrap()
            .expect("org binding stored");
        assert_eq!(first.0, 1);
        assert_eq!(first.1, "org-initial");

        uptane.store.set_org_uuid(2, "org-next").unwrap();
        let second = uptane
            .latest_org_binding()
            .unwrap()
            .expect("org binding stored");
        assert_eq!(second.0, 2);
        assert_eq!(second.1, "org-next");
    }

    #[test]
    /// Verifies update requests fail when mandatory metadata is missing.
    fn update_requires_all_mandatory_fields() {
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();
        let mut response = sample_response();
        response.config_metas = None;

        let err = uptane.update(&response).unwrap_err();
        assert!(matches!(err, UptaneError::MissingField("config_metas")));
    }

    #[test]
    /// Confirms target pruning and accessor helpers behave as expected.
    fn target_pruning_and_accessors() {
        let store = test_store();
        let uptane = UptaneState::new(store.clone()).unwrap();

        let mut initial = sample_response();
        let extra_payload = b"payload2".to_vec();
        initial.target_files.push(File {
            path: "pkg2".to_string(),
            raw: extra_payload.clone(),
        });
        if let Some(config) = initial.config_metas.as_mut() {
            if let Some(targets) = config.top_targets.as_mut() {
                upsert_target_entry(targets, "pkg2", &extra_payload);
            }
        }
        if let Some(director) = initial.director_metas.as_mut() {
            if let Some(targets) = director.targets.as_mut() {
                upsert_target_entry(targets, "pkg2", &extra_payload);
            }
        }
        uptane.update(&initial).unwrap();

        // Second update drops pkg1 to exercise the prune logic.
        let mut follow_up = sample_response();
        follow_up.target_files = vec![File {
            path: "pkg2".to_string(),
            raw: extra_payload.clone(),
        }];
        if let Some(config) = follow_up.config_metas.as_mut() {
            if let Some(targets) = config.top_targets.as_mut() {
                clear_targets(targets);
                upsert_target_entry(targets, "pkg2", &extra_payload);
            }
        }
        if let Some(director) = follow_up.director_metas.as_mut() {
            if let Some(targets) = director.targets.as_mut() {
                clear_targets(targets);
                upsert_target_entry(targets, "pkg2", &extra_payload);
            }
        }
        uptane.update(&follow_up).unwrap();

        // pkg1 should have been removed, pkg2 retained.
        assert!(uptane.target_file("pkg1").unwrap().is_none());
        match uptane.target_file("pkg2").unwrap() {
            Some(bytes) => assert_eq!(bytes, b"payload2".to_vec()),
            None => panic!("pkg2 payload should remain cached"),
        }

        // Raw metadata helpers should succeed now that the store contains records.
        assert!(uptane.director_targets_raw().is_ok());
        assert!(uptane.config_targets_raw().is_ok());
    }

    #[test]
    /// Ensures cached metadata is reused when the backend omits targets entries.
    fn uptane_update_reuses_cached_targets() {
        let _guard = VerificationGuard::enable();
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();
        let initial = signed_response(1, 1);
        seed_store_with_initial_roots(&uptane, &initial);
        uptane.update(&initial).unwrap();

        let mut partial = signed_response(1, 1);
        if let Some(config) = partial.config_metas.as_mut() {
            config.top_targets = None;
        }
        if let Some(director) = partial.director_metas.as_mut() {
            director.targets = None;
        }
        partial.target_files.clear();

        uptane.update(&partial).unwrap();

        let cached = uptane
            .target_file(SIGNED_TARGET_PATH)
            .unwrap()
            .expect("cached payload remains available");
        assert_eq!(cached, b"payload".to_vec());
    }

    #[test]
    /// Ensures missing payloads trigger an error when no cached copy exists.
    fn uptane_update_requires_target_payloads_when_uncached() {
        let _guard = VerificationGuard::enable();
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();
        let mut response = signed_response(1, 1);
        seed_store_with_initial_roots(&uptane, &response);
        response.target_files.clear();
        let err = uptane.update(&response).unwrap_err();
        match err {
            UptaneError::MissingTargetPayload { path } => assert_eq!(path, SIGNED_TARGET_PATH),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    /// Confirms cold starts still require the backend to send targets metadata.
    fn uptane_update_requires_targets_on_cold_start() {
        let _guard = VerificationGuard::enable();
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();
        let mut response = signed_response(1, 1);
        seed_store_with_initial_roots(&uptane, &response);
        if let Some(config) = response.config_metas.as_mut() {
            config.top_targets = None;
        }
        if let Some(director) = response.director_metas.as_mut() {
            director.targets = None;
        }

        let err = uptane.update(&response).unwrap_err();
        assert!(matches!(err, UptaneError::MissingField("targets")));
    }

    #[test]
    /// Rejects updates that advance snapshot targets version without providing the metadata bytes.
    fn uptane_update_rejects_missing_targets_update() {
        let _guard = VerificationGuard::enable();
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();
        let initial = signed_response(1, 1);
        seed_store_with_initial_roots(&uptane, &initial);
        uptane.update(&initial).unwrap();

        let mut follow_up = signed_response(2, 1);
        if let Some(config) = follow_up.config_metas.as_mut() {
            config.top_targets = None;
        }
        follow_up.target_files.clear();

        let err = uptane.update(&follow_up).unwrap_err();
        assert!(matches!(err, UptaneError::MissingField("targets")));
    }

    #[test]
    /// Ensures verification fails when the config repository omits a director target.
    fn uptane_rejects_config_target_gaps() {
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();
        let mut response = signed_response(1, 1);
        seed_store_with_initial_roots(&uptane, &response);
        if let Some(config) = response.config_metas.as_mut() {
            if let Some(meta) = config.top_targets.as_mut() {
                clear_targets(meta);
            }
        }

        let err = uptane
            .update(&response)
            .expect_err("missing config target must fail");
        match err {
            UptaneError::TargetMissingInConfig { path } => assert_eq!(path, SIGNED_TARGET_PATH),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    /// Ensures mismatched hashes between config and director metadata are rejected.
    fn uptane_rejects_target_hash_mismatches() {
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();
        let mut response = signed_response(1, 1);
        seed_store_with_initial_roots(&uptane, &response);
        if let Some(config) = response.config_metas.as_mut() {
            if let Some(meta) = config.top_targets.as_mut() {
                rewrite_first_target_hash(meta, "deadbeef");
            }
        }

        let err = uptane
            .update(&response)
            .expect_err("hash mismatch must fail");
        match err {
            UptaneError::TargetHashMismatch { path, algorithm } => {
                assert_eq!(path, SIGNED_TARGET_PATH);
                assert_eq!(algorithm, "sha256");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    /// Rejects payloads whose hash does not match the director metadata.
    fn uptane_rejects_target_payload_hash_mismatches() {
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();
        let mut response = signed_response(1, 1);
        seed_store_with_initial_roots(&uptane, &response);
        if let Some(file) = response.target_files.get_mut(0) {
            file.raw = b"PAYLOAD".to_vec();
        }

        let err = uptane.update(&response).unwrap_err();
        match err {
            UptaneError::TargetPayloadHashMismatch { path, algorithm } => {
                assert_eq!(path, SIGNED_TARGET_PATH);
                assert_eq!(algorithm, "sha256");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    /// Rejects payloads whose length differs from the metadata entry.
    fn uptane_rejects_target_payload_length_mismatches() {
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();
        let mut response = signed_response(1, 1);
        seed_store_with_initial_roots(&uptane, &response);
        if let Some(file) = response.target_files.get_mut(0) {
            file.raw.clear();
        }

        let err = uptane.update(&response).unwrap_err();
        match err {
            UptaneError::TargetPayloadLengthMismatch {
                path,
                expected,
                actual,
            } => {
                assert_eq!(path, SIGNED_TARGET_PATH);
                assert!(expected > 0);
                assert_eq!(actual, 0);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    /// Rejects payloads the director metadata never referenced.
    fn uptane_rejects_unexpected_target_payloads() {
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();
        let mut response = signed_response(1, 1);
        seed_store_with_initial_roots(&uptane, &response);
        response.target_files.push(File {
            path: "datadog/12345/APM_TRACING/config/extra.json".to_string(),
            raw: b"payload".to_vec(),
        });

        let err = uptane.update(&response).unwrap_err();
        match err {
            UptaneError::UnexpectedTargetPayload { path } => {
                assert_eq!(path, "datadog/12345/APM_TRACING/config/extra.json");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    /// Accepts backend payloads whose paths include the legacy `{hash}.logical` prefix.
    fn uptane_normalises_hashed_target_paths() {
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();
        let mut response = signed_response(1, 1);
        seed_store_with_initial_roots(&uptane, &response);
        let hashed_path = hashed_backend_path(SIGNED_TARGET_PATH, b"payload");
        if let Some(file) = response.target_files.get_mut(0) {
            file.path = hashed_path;
        }

        uptane.update(&response).unwrap();

        let files = uptane.all_target_files().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, SIGNED_TARGET_PATH);
    }

    #[test]
    /// Falls back to cached `{hash}.logical` files when the backend omits payloads.
    fn uptane_migrates_cached_legacy_paths() {
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();
        let mut response = signed_response(1, 1);
        seed_store_with_initial_roots(&uptane, &response);
        let hashed_path = hashed_backend_path(SIGNED_TARGET_PATH, b"payload");
        if let Some(file) = response.target_files.get_mut(0) {
            file.path = hashed_path.clone();
        }
        uptane.update(&response).unwrap();

        // Simulate a legacy store by renaming the cached key back to the hashed format.
        let tree = uptane.target_tree().unwrap();
        let trimmed_key = target_key(SIGNED_TARGET_PATH);
        let cached = tree.get(&trimmed_key).unwrap().unwrap();
        tree.remove(&trimmed_key).unwrap();
        let legacy_key = target_key(&hashed_path);
        tree.insert(&legacy_key, &cached).unwrap();

        // Next refresh omits payloads, forcing the updater to reuse the legacy key.
        let mut response_without_payloads = signed_response(2, 2);
        response_without_payloads.target_files.clear();
        uptane.update(&response_without_payloads).unwrap();

        let files = uptane.all_target_files().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, SIGNED_TARGET_PATH);
    }

    #[test]
    /// Ensures delegated metadata updates provided by the backend are accepted.
    fn uptane_accepts_delegated_metadata_updates() {
        let _guard = VerificationGuard::enable();
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();

        let initial = delegated_response(1, 1);
        seed_store_with_initial_roots(&uptane, &initial);
        uptane.update(&initial).unwrap();

        let follow_up = delegated_response(2, 2);
        uptane
            .update(&follow_up)
            .expect("delegated metadata update should succeed");
    }

    #[test]
    /// Reuses cached delegated metadata when the backend replays an older version.
    fn uptane_reuses_cached_delegated_metadata_on_version_mismatch() {
        let _guard = VerificationGuard::enable();
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();

        let initial = delegated_response(1, 1);
        let stale_meta = initial
            .config_metas
            .as_ref()
            .and_then(|config| config.delegated_targets.first().cloned())
            .expect("delegated metadata present");
        seed_store_with_initial_roots(&uptane, &initial);
        uptane.update(&initial).unwrap();

        let follow_up = delegated_response(2, 2);
        uptane.update(&follow_up).unwrap();

        let mut replayed = delegated_response(2, 2);
        if let Some(config) = replayed.config_metas.as_mut() {
            config.delegated_targets = vec![stale_meta];
        }

        uptane
            .update(&replayed)
            .expect("stale delegated metadata should fall back to cached version");
    }

    #[test]
    /// Ensures rust-tuf rejects attempts to roll back to older snapshot metadata.
    fn tuf_verification_rejects_snapshot_rollback() {
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();

        let response_v2 = signed_response(2, 2);
        seed_store_with_initial_roots(&uptane, &response_v2);
        let _guard = VerificationGuard::enable();
        uptane.update(&response_v2).unwrap();

        let response_v1 = signed_response(1, 1);
        let err = uptane.update(&response_v1).unwrap_err();
        assert!(matches!(err, UptaneError::Tuf(_)));
    }

    #[test]
    /// Confirms tampering with the signed targets metadata causes the refresh to fail.
    fn tuf_verification_rejects_tampered_targets() {
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();

        let mut tampered = signed_response(1, 1);
        seed_store_with_initial_roots(&uptane, &tampered);
        let _guard = VerificationGuard::enable();
        // Alter the signed config targets payload to emulate a MitM.
        if let Some(meta) = tampered
            .config_metas
            .as_mut()
            .and_then(|metas| metas.top_targets.as_mut())
        {
            if let Some(byte) = meta.raw.get_mut(0) {
                *byte ^= 0xFF;
            }
        }

        let err = uptane.update(&tampered).unwrap_err();
        assert!(matches!(err, UptaneError::Tuf(_)));
    }

    #[test]
    /// Exercises timestamp parsing for both the `None` and error branches.
    fn timestamp_expires_covers_error_and_none_paths() {
        let store = test_store();
        let uptane = UptaneState::new(store.clone()).unwrap();

        // Without metadata we should see the `None` branch.
        assert!(uptane.timestamp_expires().unwrap().is_none());

        // Inject an invalid timestamp document to ensure parsing errors bubble up.
        let tree = store.tree(TREE_DIRECTOR_REPO).unwrap();
        let invalid_timestamp = timestamp_json(1, "not-a-timestamp");
        tree.insert(META_TIMESTAMP.as_bytes(), &invalid_timestamp)
            .unwrap();

        let err = uptane
            .timestamp_expires()
            .expect_err("invalid timestamp must error");
        assert!(matches!(err, UptaneError::Time(_)));
    }

    #[test]
    /// Ensures accessors surface `MissingMetadata` when data is absent.
    fn missing_metadata_errors_from_accessors() {
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();

        let err = uptane
            .director_targets_raw()
            .expect_err("missing director metadata should raise error");
        assert!(matches!(
            err,
            UptaneError::Store(StoreError::MissingMetadata)
        ));
    }

    #[test]
    /// Confirms director root history is retained for incremental fetches.
    fn director_root_history_is_preserved() {
        let store = test_store();
        let uptane = UptaneState::new(store).unwrap();

        let first = sample_response();
        uptane.update(&first).unwrap();

        let mut second = sample_response();
        second.director_metas.as_mut().unwrap().roots = vec![mk_top_meta("root", 2)];
        uptane.update(&second).unwrap();

        let roots = uptane.director_roots_since(0, 2).unwrap();
        assert_eq!(roots.len(), 2);
        assert_eq!(meta_version(&roots[0]), 1);
        assert_eq!(meta_version(&roots[1]), 2);

        let delta = uptane.director_roots_since(1, 2).unwrap();
        assert_eq!(delta.len(), 1);
        assert_eq!(meta_version(&delta[0]), 2);

        assert!(uptane.director_roots_since(2, 2).unwrap().is_empty());
    }

    #[test]
    /// Ensures `build_target_batch` inserts new files and removes stale ones.
    fn build_target_batch_updates_and_prunes_targets() {
        let store = test_store();
        let tree = store.tree(TREE_TARGET_FILES).unwrap();
        let stale_key = target_key("datadog/2/stale");
        tree.insert(&stale_key, b"stale").unwrap();
        let keep_key = target_key("datadog/2/keep");
        tree.insert(&keep_key, b"old").unwrap();

        let batch = build_target_batch(
            &tree,
            &[
                File {
                    path: "datadog/2/keep".into(),
                    raw: b"new-keep".to_vec(),
                },
                File {
                    path: "datadog/2/fresh".into(),
                    raw: b"fresh".to_vec(),
                },
            ],
        )
        .unwrap();
        tree.apply_batch(batch).unwrap();

        let entries = tree.entries().unwrap();
        assert_eq!(entries.len(), 2);
        let mut seen: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        for (key, value) in entries {
            seen.insert(key, value);
        }
        assert_eq!(
            seen.get(&target_key("datadog/2/keep")).map(|v| &v[..]),
            Some(&b"new-keep"[..])
        );
        assert_eq!(
            seen.get(&target_key("datadog/2/fresh")).map(|v| &v[..]),
            Some(&b"fresh"[..])
        );
        assert!(!seen.contains_key(&target_key("datadog/2/stale")));
    }
}
