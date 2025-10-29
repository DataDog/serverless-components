//! Default AppSec ruleset management and embedding.
//!
//! This module provides access to the default recommended security ruleset,
//! which is embedded directly in the binary for zero-config security.
//!
//! # Compression
//!
//! The ruleset JSON (~500KB) is ZSTD-compressed (~50KB) to minimize binary size.
//! Decompression happens once at startup when the WAF is initialized.
//!
//! # Ruleset Content
//!
//! The default ruleset includes rules for detecting:
//! - SQL injection attempts
//! - Cross-site scripting (XSS) attacks
//! - Command injection
//! - Path traversal attempts
//! - Server-side request forgery (SSRF)
//! - And other common web application threats
//!
//! # Custom Rulesets
//!
//! Users can override the default ruleset by specifying `appsec_rules` in
//! the configuration, pointing to a custom JSON ruleset file.

use std::io;

use zstd::Decoder;

/// The default recommended ruleset, ZSTD-compressed and embedded at compile time.
///
/// # Size Optimization
///
/// - **Uncompressed**: ~500KB JSON
/// - **Compressed**: ~50KB ZSTD (~10x reduction)
/// - **Decompression**: Once at startup, minimal overhead
///
/// # Update Strategy
///
/// This embedded ruleset is updated with each agent release. For dynamic
/// updates between releases, use Remote Configuration to push new rulesets
/// without redeploying the agent.
const DEFAULT_RECOMMENDED_RULES: &[u8] = include_bytes!("default-recommended-ruleset.json.zst");

/// Returns a reader for the recommended default ruleset's JSON document.
///
/// This function decompresses the embedded ZSTD-compressed ruleset and returns
/// a reader that provides the JSON data.
///
/// # Returns
///
/// An `io::Read` implementation that yields the decompressed JSON ruleset.
///
/// # Panics
///
/// Panics if the ZSTD decoder cannot be created (should never happen in practice
/// as the embedded data is validated at build time).
///
/// # Example
///
/// ```rust,ignore
/// use datadog_agent_native::appsec::processor::ruleset::default_recommended_ruleset;
///
/// let reader = default_recommended_ruleset();
/// let ruleset: serde_json::Value = serde_json::from_reader(reader)?;
/// println!("Loaded {} rules", ruleset["rules"].as_array().unwrap().len());
/// ```
pub(super) fn default_recommended_ruleset() -> impl io::Read {
    // Decompress embedded ZSTD-compressed ruleset
    // Decision: Panic on failure as this should never happen (data is validated)
    Decoder::new(DEFAULT_RECOMMENDED_RULES).expect("failed to create ruleset reader")
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_default_recommended_ruleset() {
        let reader = super::default_recommended_ruleset();
        serde_json::from_reader::<_, serde_json::Value>(reader).expect("failed to parse ruleset");
    }
}
