//! Embedded TUF root bundles and helpers to resolve them per site.
//!
//! The Go agent ships multiple trust anchors (prod, staging, gov) and allows
//! operators to override them via environment variables. This module mirrors
//! that behaviour so Uptane stores can be seeded deterministically before the
//! first refresh.

use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EmbeddedRootError {
    #[error("invalid embedded root json: missing signed.version")]
    MissingVersion,
    #[error("embedded root json parse error: {0}")]
    Parse(#[from] serde_json::Error),
}

/// Simple wrapper around a validated root payload.
#[derive(Debug, Clone)]
pub struct RootBundle {
    /// Raw JSON bytes for the TUF root document.
    pub raw: Vec<u8>,
}

/// Resolves the configuration and director roots for the provided Datadog site.
///
/// Optional override strings take precedence over the embedded assets and must
/// contain valid JSON documents mirroring backend-delivered roots.
pub fn resolve(
    site: &str,
    config_override: Option<&str>,
    director_override: Option<&str>,
) -> Result<(RootBundle, RootBundle), EmbeddedRootError> {
    let config_root = load_root(config_override, select_config_site(site))?;
    let director_root = load_root(director_override, select_director_site(site))?;
    Ok((config_root, director_root))
}

/// Loads either the override JSON or the embedded payload and validates it.
fn load_root(
    override_value: Option<&str>,
    embedded: &'static [u8],
) -> Result<RootBundle, EmbeddedRootError> {
    match override_value {
        Some(json) => {
            // Caller supplied a custom root, so validate and return the raw bytes.
            let bytes = json.as_bytes();
            validate_root(bytes)?;
            Ok(RootBundle {
                raw: bytes.to_vec(),
            })
        }
        None => {
            // Fall back to the embedded site-specific payload.
            validate_root(embedded)?;
            Ok(RootBundle {
                raw: embedded.to_vec(),
            })
        }
    }
}

/// Ensures the provided JSON payload exposes a valid `signed.version` field.
fn validate_root(bytes: &[u8]) -> Result<(), EmbeddedRootError> {
    let value: Value = serde_json::from_slice(bytes)?;
    match value.pointer("/signed/version").and_then(Value::as_u64) {
        Some(_) => {
            // Root payload is well-formed, so the caller can proceed.
            Ok(())
        }
        None => {
            // Missing version would break Uptane bookkeeping, so surface a dedicated error.
            Err(EmbeddedRootError::MissingVersion)
        }
    }
}

const PROD_CONFIG_ROOT: &[u8] = include_bytes!("embedded_roots/data/prod.config.json");
const PROD_DIRECTOR_ROOT: &[u8] = include_bytes!("embedded_roots/data/prod.director.json");
const STAGING_CONFIG_ROOT: &[u8] = include_bytes!("embedded_roots/data/staging.config.json");
const STAGING_DIRECTOR_ROOT: &[u8] = include_bytes!("embedded_roots/data/staging.director.json");
const GOV_CONFIG_ROOT: &[u8] = include_bytes!("embedded_roots/data/gov.config.json");
const GOV_DIRECTOR_ROOT: &[u8] = include_bytes!("embedded_roots/data/gov.director.json");

/// Selects the appropriate config root bundle for the provided site.
fn select_config_site(site: &str) -> &'static [u8] {
    match site {
        "datad0g.com" => STAGING_CONFIG_ROOT,
        "ddog-gov.com" => GOV_CONFIG_ROOT,
        _ => PROD_CONFIG_ROOT,
    }
}

/// Selects the appropriate director root bundle for the provided site.
fn select_director_site(site: &str) -> &'static [u8] {
    match site {
        "datad0g.com" => STAGING_DIRECTOR_ROOT,
        "ddog-gov.com" => GOV_DIRECTOR_ROOT,
        _ => PROD_DIRECTOR_ROOT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures the resolver returns embedded roots when no overrides are supplied.
    #[test]
    fn resolve_uses_embedded_payloads_by_default() {
        let (config, director) =
            resolve("datadoghq.com", None, None).expect("embedded roots resolve");
        assert!(!config.raw.is_empty());
        assert!(!director.raw.is_empty());
    }

    /// Confirms override strings take precedence over embedded bundles.
    #[test]
    fn resolve_prefers_override_values() {
        let override_payload = r#"{"signed":{"version":7}}"#;
        let (config, director) = resolve(
            "datadoghq.com",
            Some(override_payload),
            Some(override_payload),
        )
        .expect("override roots resolve");
        assert_eq!(config.raw, override_payload.as_bytes());
        assert_eq!(director.raw, override_payload.as_bytes());
    }

    /// Verifies invalid override payloads bubble up as `EmbeddedRootError::MissingVersion`.
    #[test]
    fn resolve_rejects_invalid_override() {
        let err =
            resolve("datadoghq.com", Some(r#"{"signed":{}}"#), None).expect_err("override invalid");
        assert!(matches!(err, EmbeddedRootError::MissingVersion));
    }
}
