//! Helpers for decoding legacy Remote Config keys (DDRCM_...).
//!
//! RC keys embed the org ID, datacenter, and application key in a base32
//! payload so agents can derive the correct credentials at runtime. This
//! module exposes a tiny decoder that mirrors the Go implementation.

use data_encoding::BASE32_NOPAD;
use rmp_serde::decode::Error as RmpError;
use serde::Deserialize;
use thiserror::Error;

/// Decoded representation of the legacy RC key payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RcKey {
    pub app_key: String,
    pub org_id: u64,
    pub datacenter: String,
}

#[derive(Debug, Error)]
pub enum RcKeyError {
    #[error("rc key must not be empty")]
    Empty,
    #[error("rc key base32 decode error: {0}")]
    Base32(data_encoding::DecodeError),
    #[error("rc key payload decode error: {0}")]
    Msgpack(RmpError),
    #[error("rc key missing required fields")]
    MissingFields,
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct RcKeyWire {
    #[serde(rename = "key")]
    app_key: String,
    #[serde(rename = "org")]
    org_id: u64,
    #[serde(rename = "dc")]
    datacenter: String,
}

impl RcKey {
    /// Decodes a legacy RC key string (with or without the `DDRCM_` prefix)
    /// into its structured representation.
    ///
    /// The function validates the base32 payload, ensures all required
    /// fields are present, and rejects empty inputs so callers can provide
    /// actionable error messages to users.
    pub fn decode(input: &str) -> Result<Self, RcKeyError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            // An empty string cannot produce a valid key, so return a
            // dedicated error to help the caller differentiate from other
            // parsing failures.
            return Err(RcKeyError::Empty);
        }
        let without_prefix = trimmed.strip_prefix("DDRCM_").unwrap_or(trimmed);
        let bytes = BASE32_NOPAD
            .decode(without_prefix.as_bytes())
            .map_err(RcKeyError::Base32)?;
        let wire: RcKeyWire = rmp_serde::from_slice(&bytes).map_err(RcKeyError::Msgpack)?;
        if wire.app_key.is_empty() || wire.datacenter.is_empty() {
            // The encoded payload must include both the application key and
            // datacenter so agents can build HTTP headers. Missing fields are
            // reported explicitly.
            return Err(RcKeyError::MissingFields);
        }
        Ok(Self {
            app_key: wire.app_key,
            org_id: wire.org_id,
            datacenter: wire.datacenter,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Verifies a well-formed RC key decodes into structured fields.
    fn decode_valid_key() {
        let wire = RcKeyWire {
            app_key: "58d58c60b8ac337293ce2ca6b28b19eb".into(),
            org_id: 2,
            datacenter: "datadoghq.com".into(),
        };
        let encoded = {
            let bytes = rmp_serde::to_vec(&wire).unwrap();
            BASE32_NOPAD.encode(&bytes)
        };
        let formatted = format!("DDRCM_{}", encoded);
        let decoded = RcKey::decode(&formatted).unwrap();
        assert_eq!(decoded.app_key, wire.app_key);
        assert_eq!(decoded.org_id, wire.org_id as u64);
        assert_eq!(decoded.datacenter, wire.datacenter);
    }

    #[test]
    /// Rejects empty inputs before hitting the base32 decoder.
    fn decode_rejects_empty_input() {
        assert!(matches!(RcKey::decode("  \n"), Err(RcKeyError::Empty)));
    }
}
