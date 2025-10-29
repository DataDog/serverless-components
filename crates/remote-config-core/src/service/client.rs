//! Client validation and targeting helpers.
//!
//! These routines mirror the Go agent implementation for handling
//! `ClientGetConfigs` requests. They validate client descriptors, filter cached
//! metadata, evaluate tracer predicates, and canonicalise target payloads so
//! that the core service can remain focused on orchestration.

use std::collections::{HashMap, HashSet};

use remote_config_proto::remoteconfig::{
    Client, TargetFileMeta, TracerPredicateV1, TracerPredicates,
};
use semver::{Version, VersionReq};
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;

use crate::service::ServiceError;
use crate::targets::{parse_targets_document_from_value, TargetsDocument};
use crate::uptane_path::{parse_config_path, ConfigPathSource};

/// Validates the client payload and cached entries exactly like the Go implementation.
pub(crate) fn validate_client_request(
    client: &Client,
    cached_target_files: &[TargetFileMeta],
) -> Result<(), ServiceError> {
    validate_client_descriptor(client)?;
    validate_cached_target_files(cached_target_files)?;
    Ok(())
}

/// Ensures the client descriptor satisfies the constraints enforced by the Go service.
fn validate_client_descriptor(client: &Client) -> Result<(), ServiceError> {
    // Remote Config requires a populated `state` describing the current Uptane roots.
    let state = if let Some(state) = client.state.as_ref() {
        state
    } else {
        return Err(ServiceError::InvalidRequest(
            "client.state is a required field for client config update requests".to_string(),
        ));
    };
    // Clients must bootstrap from the initial director root before requesting configs.
    if state.root_version == 0 {
        return Err(ServiceError::InvalidRequest(
            "client.state.root_version must be >= 1 (clients must start with the base TUF director root)".to_string(),
        ));
    }

    let is_agent = client.is_agent;
    let is_tracer = client.is_tracer;
    let is_updater = client.is_updater;

    // Ensure client descriptors only request capabilities they can satisfy.
    if is_agent && client.client_agent.is_none() {
        return Err(ServiceError::InvalidRequest(
            "client.client_agent is a required field for agent client config update requests"
                .to_string(),
        ));
    }
    if is_tracer && client.client_tracer.is_none() {
        return Err(ServiceError::InvalidRequest(
            "client.client_tracer is a required field for tracer client config update requests"
                .to_string(),
        ));
    }
    if is_updater && client.client_updater.is_none() {
        return Err(ServiceError::InvalidRequest(
            "client.client_updater is a required field for updater client config update requests"
                .to_string(),
        ));
    }

    // Agent/tracer/updater roles are mutually exclusive just like in the Go agent.
    let role_count = (is_agent as u8) + (is_tracer as u8) + (is_updater as u8);
    if role_count > 1 {
        return Err(ServiceError::InvalidRequest(
            "client.is_tracer, client.is_agent, and client.is_updater are mutually exclusive"
                .to_string(),
        ));
    }
    if role_count == 0 {
        return Err(ServiceError::InvalidRequest(
            "agents only support remote config updates from tracer or agent or updater at this time".to_string(),
        ));
    }

    // Empty identifiers make deduplication and targeting impossible.
    if client.id.trim().is_empty() {
        return Err(ServiceError::InvalidRequest(
            "client.id is a required field for client config update requests".to_string(),
        ));
    }

    // Tracer-specific validation mirrors the backend: ensure tracer block is well formed.
    if is_tracer {
        let tracer = if let Some(tracer) = client.client_tracer.as_ref() {
            tracer
        } else {
            return Err(ServiceError::InvalidRequest(
                "client.client_tracer must be set if client.is_tracer is true".to_string(),
            ));
        };
        if client.client_agent.is_some() {
            return Err(ServiceError::InvalidRequest(
                "client.client_agent must not be set if client.is_tracer is true".to_string(),
            ));
        }
        if client.id == tracer.runtime_id {
            return Err(ServiceError::InvalidRequest(
                "client.id must be different from client.client_tracer.runtime_id".to_string(),
            ));
        }
        if tracer.language.is_empty() {
            return Err(ServiceError::InvalidRequest(
                "client.client_tracer.language is a required field for tracer client config update requests".to_string(),
            ));
        }
    }

    // Agent descriptors cannot mix tracer metadata and must provide agent info.
    if is_agent {
        if client.client_agent.is_none() {
            return Err(ServiceError::InvalidRequest(
                "client.client_agent must be set if client.is_agent is true".to_string(),
            ));
        }
        if client.client_tracer.is_some() {
            return Err(ServiceError::InvalidRequest(
                "client.client_tracer must not be set if client.is_agent is true".to_string(),
            ));
        }
    }

    Ok(())
}

/// Validates cached target metadata entries supplied by the client.
fn validate_cached_target_files(
    cached_target_files: &[TargetFileMeta],
) -> Result<(), ServiceError> {
    for (index, meta) in cached_target_files.iter().enumerate() {
        if meta.path.is_empty() {
            return Err(ServiceError::InvalidRequest(format!(
                "cached_target_files[{index}].path is a required field for client config update requests"
            )));
        }
        if let Err(err) = parse_config_path(&meta.path) {
            return Err(ServiceError::InvalidRequest(format!(
                "cached_target_files[{index}].path is not a valid path: {err}"
            )));
        }
        if meta.length <= 0 {
            return Err(ServiceError::InvalidRequest(format!(
                "cached_target_files[{index}].length must be >= 1 (no empty file allowed)"
            )));
        }
        if meta.hashes.is_empty() {
            return Err(ServiceError::InvalidRequest(format!(
                "cached_target_files[{index}].hashes is a required field for client config update requests"
            )));
        }
        for (hash_index, hash) in meta.hashes.iter().enumerate() {
            if hash.algorithm.is_empty() {
                return Err(ServiceError::InvalidRequest(format!(
                    "cached_target_files[{index}].hashes[{hash_index}].algorithm is a required field for client config update requests"
                )));
            }
            if hash.hash.is_empty() {
                return Err(ServiceError::InvalidRequest(format!(
                    "cached_target_files[{index}].hashes[{hash_index}].hash is a required field for client config update requests"
                )));
            }
        }
    }
    Ok(())
}

/// Builds a mapping from logical config paths to physical file paths.
/// Physical paths include the content hash: `{dir}/{content_hash}.{config_id}`.
pub(crate) fn build_path_mapping(
    targets: Value,
) -> Result<std::collections::HashMap<String, String>, ServiceError> {
    use std::collections::HashMap;

    let document: TargetsDocument = parse_targets_document_from_value(targets)?;
    let mut mapping = HashMap::new();

    for (logical_path, entry) in document.signed.targets {
        // Extract the content hash from the entry
        if let Some(content_hash) = entry.hashes.get("sha256") {
            // Split the logical path to get directory and file parts
            if let Some(last_slash_pos) = logical_path.rfind('/') {
                let dir_part = &logical_path[..last_slash_pos];
                let file_part = &logical_path[last_slash_pos + 1..];
                // Build physical path: {dir}/{content_hash}.{file_part}
                let physical_path = format!("{}/{}.{}", dir_part, content_hash, file_part);
                mapping.insert(logical_path, physical_path);
            } else {
                // No directory, just file
                let physical_path = format!("{}.{}", content_hash, logical_path);
                mapping.insert(logical_path, physical_path);
            }
        }
    }

    Ok(mapping)
}

/// Computes the list of configuration paths that match the provided client predicates.
///
/// Performs the same filtering steps as the Go agent: product membership,
/// optional org ID enforcement, expiration timestamps, and tracer predicates.
pub(crate) fn compute_matched_configs(
    client: &Client,
    targets: Value,
    enforce_org_id: Option<u64>,
) -> Result<Vec<String>, ServiceError> {
    let requested_products: HashSet<&str> = client
        .products
        .iter()
        .map(|product| product.as_str())
        .collect();
    let document: TargetsDocument = parse_targets_document_from_value(targets)?;
    let mut matched = Vec::new();

    for (path, entry) in document.signed.targets {
        let path_info = parse_config_path(&path)
            .map_err(|err| ServiceError::InvalidRequest(format!("invalid target path: {err}")))?;
        if let Some(expected) = enforce_org_id {
            // Datadog-managed paths must match the org UUID encoded in the RC key.
            if path_info.source != ConfigPathSource::Employee {
                match path_info.org_id {
                    Some(org) if org == expected => {}
                    Some(other) => {
                        return Err(ServiceError::InvalidRequest(format!(
                            "target path '{path}' org_id {other} does not match expected {expected}"
                        )));
                    }
                    None => {
                        return Err(ServiceError::InvalidRequest(format!(
                            "target path '{path}' missing org_id"
                        )));
                    }
                }
            }
        }
        if !requested_products.contains(path_info.product.as_str()) {
            // Skip configs for products the client did not subscribe to.
            continue;
        }
        let metadata = parse_custom_metadata(entry.custom.as_ref())?;
        if config_is_expired(metadata.expires)? {
            continue;
        }
        if tracer_predicates_match(client, metadata.tracer_predicates.as_ref())? {
            matched.push(path);
        }
    }

    matched.sort();
    matched.dedup();
    Ok(matched)
}

/// Extracts the opaque backend state from a targets document when present.
///
/// This value is echoed back to the backend via `backend_client_state` so the
/// control plane can maintain stateless context.
pub(crate) fn extract_backend_client_state(targets: Value) -> Result<Vec<u8>, ServiceError> {
    let document: TargetsDocument = parse_targets_document_from_value(targets)?;
    if let Some(custom) = document.signed.custom {
        if let Some(state) = custom.opaque_backend_state {
            return Ok(state.into_vec());
        }
    }
    Ok(Vec::new())
}

/// Parses the optional custom metadata into the strongly typed structure.
pub(crate) fn parse_custom_metadata(
    custom: Option<&Value>,
) -> Result<ConfigFileMetaCustom, ServiceError> {
    match custom {
        None => Ok(ConfigFileMetaCustom::default()),
        Some(Value::Null) => Ok(ConfigFileMetaCustom::default()),
        Some(value) => Ok(serde_json::from_value(value.clone())?),
    }
}

/// Custom metadata extracted from a target entry.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ConfigFileMetaCustom {
    /// Optional tracer predicates controlling config rollout.
    #[serde(rename = "tracer-predicates")]
    #[serde(default)]
    tracer_predicates: Option<TracerPredicates>,
    /// Epoch timestamp expressed in seconds indicating when the config expires.
    #[serde(default)]
    expires: i64,
}

/// Determines whether the config metadata has expired relative to the current time.
///
/// Matches the Go agent semantics: `expires == 0` means "no expiry".
pub(crate) fn config_is_expired(expires: i64) -> Result<bool, ServiceError> {
    if expires == 0 {
        return Ok(false);
    }
    let expiry = OffsetDateTime::from_unix_timestamp(expires).map_err(|err| {
        ServiceError::InvalidRequest(format!("invalid config expiration timestamp: {err}"))
    })?;
    Ok(OffsetDateTime::now_utc() > expiry)
}

/// Evaluates the tracer predicates for the provided client.
fn tracer_predicates_match(
    client: &Client,
    predicates: Option<&TracerPredicates>,
) -> Result<bool, ServiceError> {
    let Some(predicates) = predicates else {
        return Ok(true);
    };
    let list = &predicates.tracer_predicates_v1;
    if list.is_empty() {
        return Ok(true);
    }
    for predicate in list {
        if predicate_matches(client, predicate)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Checks whether a single predicate applies to the provided client descriptor.
///
/// Applies only to tracer clients and mirrors the Go agent’s matching rules
/// (client ID, runtime ID, service/env/language/app version, and semver ranges).
pub(crate) fn predicate_matches(
    client: &Client,
    predicate: &TracerPredicateV1,
) -> Result<bool, ServiceError> {
    if !predicate.client_id.is_empty() && client.id != predicate.client_id {
        return Ok(false);
    }

    if client.is_tracer {
        let tracer = client
            .client_tracer
            .as_ref()
            .expect("tracer descriptor required for tracer clients");

        if !predicate.runtime_id.is_empty() && tracer.runtime_id != predicate.runtime_id {
            return Ok(false);
        }
        if !predicate.service.is_empty() && tracer.service != predicate.service {
            return Ok(false);
        }
        if !predicate.environment.is_empty() && tracer.env != predicate.environment {
            return Ok(false);
        }
        if !predicate.app_version.is_empty() && tracer.app_version != predicate.app_version {
            return Ok(false);
        }
        if !predicate.language.is_empty() && tracer.language != predicate.language {
            return Ok(false);
        }

        if !predicate.tracer_version.is_empty() {
            let version = parse_semver_version(&tracer.tracer_version).map_err(|err| {
                ServiceError::InvalidRequest(format!(
                    "invalid tracer version '{}': {err}",
                    tracer.tracer_version
                ))
            })?;
            let requirement = VersionReq::parse(&predicate.tracer_version).map_err(|err| {
                ServiceError::InvalidRequest(format!(
                    "invalid tracer predicate constraint '{}': {err}",
                    predicate.tracer_version
                ))
            })?;
            if !requirement.matches(&version) {
                return Ok(false);
            }
        }
    }

    Ok(true)
}

/// Parses a semantic version string, tolerating shorthand core versions.
///
/// Tracer versions frequently omit patch numbers; this helper pads missing
/// segments so semver comparisons still work.
pub(crate) fn parse_semver_version(input: &str) -> Result<Version, semver::Error> {
    match Version::parse(input) {
        Ok(version) => Ok(version),
        Err(original_err) => {
            let (core, suffix) = split_version_core_suffix(input);
            let mut segments: Vec<&str> = core
                .split('.')
                .filter(|segment| !segment.is_empty())
                .collect();
            if segments.is_empty() || segments.len() > 3 {
                return Err(original_err);
            }
            while segments.len() < 3 {
                segments.push("0");
            }
            let normalized = format!("{}{}", segments.join("."), suffix);
            Version::parse(&normalized).map_err(|_| original_err)
        }
    }
}

/// Splits a version string into its core and suffix components.
pub(crate) fn split_version_core_suffix(input: &str) -> (&str, &str) {
    if let Some(idx) = input.find(['-', '+']) {
        (&input[..idx], &input[idx..])
    } else {
        (input, "")
    }
}

/// Index entry describing a cached target file advertised by the client.
#[derive(Debug)]
pub(crate) struct CachedFileInfo {
    hash: String,
    length: i64,
}

impl CachedFileInfo {
    /// Returns true when the cached entry matches the provided target attributes.
    pub(crate) fn matches(&self, length: i64, hash: &str) -> bool {
        self.length == length && self.hash == hash
    }
}

/// Builds a lookup table of cached target files keyed by path.
pub(crate) fn build_cached_index(entries: &[TargetFileMeta]) -> HashMap<String, CachedFileInfo> {
    let mut map = HashMap::new();
    for meta in entries {
        if let Some(hash) = meta
            .hashes
            .iter()
            .find(|hash| hash.algorithm.eq_ignore_ascii_case("sha256"))
        {
            map.insert(
                meta.path.clone(),
                CachedFileInfo {
                    hash: hash.hash.clone(),
                    length: meta.length,
                },
            );
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use remote_config_proto::remoteconfig::{ClientAgent, ClientState, TargetFileHash};

    /// Builds a valid agent descriptor used throughout the tests.
    fn agent_client(id: &str) -> Client {
        Client {
            id: id.to_string(),
            state: Some(ClientState {
                root_version: 1,
                ..Default::default()
            }),
            is_agent: true,
            client_agent: Some(ClientAgent {
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Ensures missing `state` entries are rejected with a validation error.
    #[test]
    fn descriptor_requires_state() {
        let mut client = agent_client("agent");
        client.state = None;
        let err = validate_client_descriptor(&client).expect_err("state missing");
        assert!(
            err.to_string().contains("client.state is a required field"),
            "unexpected error message: {err}"
        );
    }

    /// Confirms a well-formed agent descriptor passes without errors.
    #[test]
    fn descriptor_accepts_agent_payload() {
        let client = agent_client("agent");
        validate_client_descriptor(&client).expect("agent descriptor valid");
    }

    /// Verifies cached target metadata must include a path component.
    #[test]
    fn cached_target_files_require_paths() {
        let meta = TargetFileMeta {
            path: String::new(),
            length: 10,
            hashes: vec![TargetFileHash {
                algorithm: "sha256".into(),
                hash: "deadbeef".into(),
            }],
        };
        let err = validate_cached_target_files(&[meta]).expect_err("invalid cache entry");
        assert!(
            err.to_string().contains("cached_target_files[0].path"),
            "unexpected error message: {err}"
        );
    }
}
