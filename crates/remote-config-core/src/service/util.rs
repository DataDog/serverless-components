//! Shared utility helpers for the remote configuration service.
//!
//! These helpers are consumed by multiple submodules (client handling,
//! response rendering, tests) and are therefore centralised here to keep other
//! files focused on their primary responsibilities.

use sha2::{Digest, Sha256};

#[cfg(test)]
use super::ServiceError;
#[cfg(test)]
use serde_json::Value;

/// Canonicalises JSON payloads by recursively sorting object keys (tests only).
///
/// This mirrors the Go agent behaviour where canonical JSON is used to compare
/// payloads deterministically in tests.
#[cfg(test)]
pub(crate) fn canonicalize_json_bytes(bytes: &[u8]) -> Result<Vec<u8>, ServiceError> {
    let value: Value = serde_json::from_slice(bytes)?;
    let canonical = canonicalize_json_value(value);
    Ok(serde_json::to_vec(&canonical)?)
}

/// Recursively sorts JSON objects to produce stable serialisation output.
#[cfg(test)]
fn canonicalize_json_value(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            // Object branch sorts keys recursively to ensure deterministic serialization.
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut sorted = serde_json::Map::with_capacity(entries.len());
            for (key, val) in entries {
                sorted.insert(key, canonicalize_json_value(val));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => {
            // Array branch preserves order but canonicalises each element.
            Value::Array(items.into_iter().map(canonicalize_json_value).collect())
        }
        other => {
            // Scalar branch leaves primitives untouched.
            other
        }
    }
}

/// Computes the hexadecimal SHA-256 digest for the provided payload.
///
/// Used by tests to assert target-file hashes match expected values.
pub(crate) fn compute_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures canonicalisation sorts object keys deterministically.
    #[test]
    fn canonicalize_json_bytes_sorts_objects() {
        let payload = r#"{ "z": true, "a": false }"#.as_bytes();
        let canonical = canonicalize_json_bytes(payload).unwrap();
        assert_eq!(
            String::from_utf8(canonical).unwrap(),
            r#"{"a":false,"z":true}"#
        );
    }

    /// Confirms the SHA-256 helper matches a known digest.
    #[test]
    fn compute_sha256_matches_expected_digest() {
        assert_eq!(
            compute_sha256(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
