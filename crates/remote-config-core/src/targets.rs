//! Shared helpers for parsing TUF targets metadata.
//!
//! Both the Uptane implementation and the service layer need to interpret
//! `targets.json` documents. Centralising the serde shapes avoids drifting
//! struct definitions and guarantees that every consumer observes the same
//! view of the metadata.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_bytes::ByteBuf;
use serde_json::Value;

/// Deserialised representation of a TUF targets document.
#[derive(Debug, Deserialize, Clone)]
pub struct TargetsDocument {
    /// Signed section containing targets and optional custom fields.
    pub signed: TargetsSigned,
}

/// Signed payload containing the per-target metadata plus custom data.
#[derive(Debug, Deserialize, Clone)]
pub struct TargetsSigned {
    /// Mapping from target path to its metadata.
    #[serde(default)]
    pub targets: BTreeMap<String, TargetDescription>,
    /// Optional top-level custom section.
    #[serde(default)]
    pub custom: Option<TargetsSignedCustom>,
}

/// Top-level custom metadata embedded in a targets document.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct TargetsSignedCustom {
    /// Backend-provided opaque state echoed back to the service.
    #[serde(default, rename = "opaque_backend_state")]
    pub opaque_backend_state: Option<ByteBuf>,
    /// Backend-suggested refresh interval (seconds).
    #[serde(default, rename = "agent_refresh_interval")]
    pub agent_refresh_interval: Option<i64>,
}

/// Metadata describing an individual target entry.
#[derive(Debug, Deserialize, Clone)]
pub struct TargetDescription {
    /// Advertised file length.
    #[serde(default)]
    pub length: i64,
    /// Content hashes keyed by algorithm (e.g., `sha256`).
    #[serde(default)]
    pub hashes: BTreeMap<String, String>,
    /// Optional custom metadata associated with the target.
    #[serde(default)]
    pub custom: Option<Value>,
}

/// Parses a targets document from raw bytes.
pub fn parse_targets_document_from_bytes(
    bytes: &[u8],
) -> Result<TargetsDocument, serde_json::Error> {
    serde_json::from_slice(bytes)
}

/// Parses a targets document from a JSON value.
pub fn parse_targets_document_from_value(
    value: Value,
) -> Result<TargetsDocument, serde_json::Error> {
    serde_json::from_value(value)
}

/// Extracts the targets map from a raw JSON document.
pub fn parse_targets_map_from_bytes(
    bytes: &[u8],
) -> Result<BTreeMap<String, TargetDescription>, serde_json::Error> {
    Ok(parse_targets_document_from_bytes(bytes)?.signed.targets)
}

/// Extracts the targets map from an owned JSON value.
pub fn parse_targets_map_from_value(
    value: Value,
) -> Result<BTreeMap<String, TargetDescription>, serde_json::Error> {
    Ok(parse_targets_document_from_value(value)?.signed.targets)
}
