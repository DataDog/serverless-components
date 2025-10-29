//! APM replace rules for obfuscating sensitive data in trace tags.
//!
//! Replace rules allow users to redact or transform sensitive information in trace tags
//! before sending traces to Datadog. Common use cases include:
//! - **PII removal**: Mask emails, phone numbers, SSNs in trace data
//! - **Credential obfuscation**: Remove API keys, tokens, passwords from tags
//! - **Data compliance**: Redact data to comply with GDPR, CCPA, HIPAA
//! - **Resource name normalization**: Remove IDs from resource names for better aggregation
//!
//! # Configuration
//!
//! Replace rules can be configured via:
//! - **Environment variable**: `DD_APM_REPLACE_TAGS='[{"name":"resource.name","pattern":"user/[0-9]+","repl":"user/?"}]'`
//! - **YAML config**: See `datadog.yaml` documentation
//!
//! # Rule Structure
//!
//! Each rule consists of:
//! - `name`: Tag name to apply the rule to (e.g., `"resource.name"`, `"http.url"`)
//! - `pattern`: Regular expression pattern to match
//! - `repl`: Replacement text for matches
//!
//! # Examples
//!
//! ```yaml
//! apm_config:
//!   replace_tags:
//!     # Remove user IDs from resource names
//!     - name: resource.name
//!       pattern: 'user/[0-9]+'
//!       repl: 'user/?'
//!
//!     # Mask email addresses
//!     - name: user.email
//!       pattern: '[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}'
//!       repl: '[EMAIL_REDACTED]'
//!
//!     # Remove API keys from URLs
//!     - name: http.url
//!       pattern: 'api_key=[^&]+'
//!       repl: 'api_key=REDACTED'
//! ```

use datadog_trace_obfuscation::replacer::{parse_rules_from_string, ReplaceRule};
use serde::de::{Deserializer, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json;
use std::fmt;

/// YAML representation of a replace rule.
///
/// This intermediate struct is used to deserialize rules from YAML files before
/// converting them to the internal `ReplaceRule` format.
#[derive(Deserialize, Serialize)]
struct ReplaceRuleYaml {
    /// Tag name to apply the rule to (e.g., "resource.name", "http.url").
    name: String,
    /// Regular expression pattern to match against the tag value.
    pattern: String,
    /// Replacement text for matches.
    repl: String,
}

/// Visitor for deserializing replace rules from either JSON strings or YAML sequences.
///
/// This visitor handles two input formats:
/// - **JSON string** (from environment variables): `"[{\"name\":\"...\",\"pattern\":\"...\",\"repl\":\"...\"}]"`
/// - **YAML sequence** (from YAML files): Direct array of rule objects
struct StringOrReplaceRulesVisitor;

impl<'de> Visitor<'de> for StringOrReplaceRulesVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON string or YAML sequence of replace rules")
    }

    /// Handle JSON strings from environment variables.
    ///
    /// Validates the JSON and returns the string for further processing.
    /// Invalid JSON is logged and returns an empty string (no rules).
    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        // Validate that the string contains valid JSON
        match serde_json::from_str::<serde_json::Value>(value) {
            Ok(_) => Ok(value.to_string()),
            Err(e) => {
                // Invalid JSON - log error and return empty string
                tracing::error!("Invalid JSON string for APM replace rules: {}", e);
                Ok(String::new())
            }
        }
    }

    /// Handle YAML sequences from YAML files.
    ///
    /// Converts the YAML sequence to a JSON string for uniform processing.
    /// Conversion failures are logged and return an empty string (no rules).
    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        // Collect all rules from the YAML sequence
        let mut rules = Vec::new();
        while let Some(rule) = seq.next_element::<ReplaceRuleYaml>()? {
            rules.push(rule);
        }

        // Convert the YAML rules to a JSON string for uniform parsing
        match serde_json::to_string(&rules) {
            Ok(json) => Ok(json),
            Err(e) => {
                // Conversion failed - log error and return empty string
                tracing::error!("Failed to convert YAML rules to JSON: {}", e);
                Ok(String::new())
            }
        }
    }
}

/// Deserializes APM replace rules from config sources.
///
/// Handles two input formats:
/// - **JSON string**: Rules encoded as a JSON string (from environment variables like `DD_APM_REPLACE_TAGS`)
/// - **YAML sequence**: Direct array of rules (from `datadog.yaml`)
///
/// # Error Handling
///
/// This implementation is lenient:
/// - Invalid JSON/YAML logs an error and returns `None`
/// - Failed rule parsing logs an error and returns `None`
/// - Never fails deserialization - ensures agent can start with invalid rules
///
/// # Format Examples
///
/// **JSON string** (environment variable):
/// ```json
/// "[{\"name\":\"resource.name\",\"pattern\":\"user/[0-9]+\",\"repl\":\"user/?\"}]"
/// ```
///
/// **YAML sequence** (YAML file):
/// ```yaml
/// apm_config:
///   replace_tags:
///     - name: resource.name
///       pattern: 'user/[0-9]+'
///       repl: 'user/?'
/// ```
///
/// # Processing Pipeline
///
/// 1. **Deserialize** via visitor → JSON string
/// 2. **Parse** JSON string → Vec<ReplaceRule>
/// 3. **Compile** regex patterns → Ready to use
///
/// If any step fails, logs an error and returns `None`.
pub fn deserialize_apm_replace_rules<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<ReplaceRule>>, D::Error>
where
    D: Deserializer<'de>,
{
    // Use the custom visitor to handle both JSON strings and YAML sequences
    let json_string = deserializer.deserialize_any(StringOrReplaceRulesVisitor)?;

    // Parse the JSON string into replace rules
    // This also compiles the regex patterns for efficient matching
    match parse_rules_from_string(&json_string) {
        Ok(rules) => Ok(Some(rules)),
        Err(e) => {
            // Failed to parse rules - log error and return None
            // This ensures the agent can start even with invalid replace rules
            tracing::error!("Failed to parse APM replace rule, ignoring: {}", e);
            Ok(None)
        }
    }
}
