//! Log processing rules for filtering and obfuscating log data.
//!
//! Processing rules allow users to control which logs are sent to Datadog and how sensitive
//! data is masked before transmission. Rules are applied in order, and the first matching
//! rule determines the action.
//!
//! # Rule Types
//!
//! - **ExcludeAtMatch**: Drop logs that match the pattern (e.g., health checks, debug logs)
//! - **IncludeAtMatch**: Only send logs that match the pattern (whitelist filtering)
//! - **MaskSequences**: Replace sensitive data with a placeholder (e.g., PII, credentials)
//!
//! # Configuration
//!
//! Rules can be configured via:
//! - **Environment variable**: `DD_LOGS_CONFIG_PROCESSING_RULES='[{"type":"exclude_at_match","name":"health_checks","pattern":"GET /health"}]'`
//! - **YAML config**: See `datadog.yaml` documentation
//!
//! # Example Rules
//!
//! ```yaml
//! logs_config:
//!   processing_rules:
//!     # Exclude health check logs to reduce noise
//!     - type: exclude_at_match
//!       name: exclude_health_checks
//!       pattern: "GET /health|GET /ping"
//!
//!     # Mask credit card numbers
//!     - type: mask_sequences
//!       name: mask_credit_cards
//!       pattern: '\d{4}-\d{4}-\d{4}-\d{4}'
//!       replace_placeholder: "[CREDIT_CARD_REDACTED]"
//!
//!     # Only send error logs (include filter)
//!     - type: include_at_match
//!       name: only_errors
//!       pattern: "ERROR|FATAL|CRITICAL"
//! ```

use serde::{Deserialize, Deserializer};
use serde_json::Value as JsonValue;

/// Type of log processing rule determining the action to take when a pattern matches.
///
/// Processing rules are evaluated in order. The first matching rule determines the action.
/// Rules can filter logs (exclude/include) or obfuscate sensitive data (mask).
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Exclude logs that match the pattern.
    ///
    /// When a log matches this pattern, it is dropped and not sent to Datadog.
    ///
    /// **Use for**: Health checks, debug logs, noisy endpoints, internal monitoring.
    ///
    /// **Example**: Exclude health check requests:
    /// ```json
    /// {"type": "exclude_at_match", "name": "health", "pattern": "GET /health"}
    /// ```
    ExcludeAtMatch,
    /// Include only logs that match the pattern.
    ///
    /// When a log matches this pattern, it is sent to Datadog. All other logs are dropped.
    /// This is a whitelist approach - only matching logs pass through.
    ///
    /// **Use for**: Filtering to specific log levels, services, or message types.
    ///
    /// **Example**: Only send error logs:
    /// ```json
    /// {"type": "include_at_match", "name": "errors_only", "pattern": "ERROR|FATAL"}
    /// ```
    IncludeAtMatch,
    /// Mask sensitive sequences that match the pattern.
    ///
    /// When a log matches this pattern, the matching text is replaced with `replace_placeholder`.
    /// The log is still sent to Datadog, but with sensitive data obfuscated.
    ///
    /// **Use for**: PII (emails, phone numbers), credentials (API keys, passwords), credit cards.
    ///
    /// **Example**: Mask email addresses:
    /// ```json
    /// {
    ///   "type": "mask_sequences",
    ///   "name": "mask_emails",
    ///   "pattern": "[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\\.[a-zA-Z]{2,}",
    ///   "replace_placeholder": "[EMAIL_REDACTED]"
    /// }
    /// ```
    MaskSequences,
}

/// A log processing rule that filters or obfuscates log data.
///
/// Processing rules are applied in order to each log message. Rules can:
/// - **Filter**: Exclude or include logs based on pattern matching
/// - **Obfuscate**: Mask sensitive data by replacing matches with a placeholder
///
/// # Fields
///
/// - `kind`: Type of rule (exclude, include, or mask)
/// - `name`: Human-readable rule identifier for debugging
/// - `pattern`: Regular expression to match against log messages
/// - `replace_placeholder`: Replacement text for mask rules (optional, only for `MaskSequences`)
///
/// # Pattern Matching
///
/// Patterns are regular expressions evaluated against the entire log message. Use standard
/// regex syntax with proper escaping (especially in JSON/YAML strings).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ProcessingRule {
    /// Type of processing rule (exclude, include, or mask).
    #[serde(rename = "type")]
    pub kind: Kind,
    /// Human-readable name for the rule (used in logs and debugging).
    ///
    /// Should be descriptive and unique, e.g., "exclude_health_checks", "mask_credit_cards".
    pub name: String,
    /// Regular expression pattern to match against log messages.
    ///
    /// Uses standard regex syntax. Matching is case-sensitive by default.
    ///
    /// **Examples**:
    /// - `"GET /health"` - Literal string match
    /// - `"ERROR|WARN"` - Match ERROR or WARN
    /// - `"\d{4}-\d{4}-\d{4}-\d{4}"` - Credit card number pattern
    pub pattern: String,
    /// Replacement text for masked sequences (only used for `MaskSequences` rules).
    ///
    /// When the pattern matches, the matching text is replaced with this placeholder.
    /// Ignored for `ExcludeAtMatch` and `IncludeAtMatch` rules.
    ///
    /// **Example**: `"[REDACTED]"`, `"***"`, `"[EMAIL_REMOVED]"`
    pub replace_placeholder: Option<String>,
}

/// Deserializes processing rules from config sources.
///
/// Handles two input formats:
/// - **JSON string**: Rules encoded as a JSON string (from environment variables)
/// - **JSON array**: Direct array of rules (from YAML files)
///
/// # Error Handling
///
/// This implementation is lenient:
/// - Invalid rules are logged and skipped
/// - If all rules are invalid, returns `None`
/// - Never fails deserialization - ensures agent can start with partial config
///
/// # Examples
///
/// ```json
/// // Format 1: JSON string (environment variable)
/// "[{\"type\":\"exclude_at_match\",\"name\":\"health\",\"pattern\":\"GET /health\"}]"
///
/// // Format 2: JSON array (YAML file)
/// [
///   {
///     "type": "exclude_at_match",
///     "name": "health",
///     "pattern": "GET /health"
///   }
/// ]
/// ```
pub fn deserialize_processing_rules<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<ProcessingRule>>, D::Error>
where
    D: Deserializer<'de>,
{
    // Deserialize the JSON value using serde_json::Value
    let value: JsonValue = Deserialize::deserialize(deserializer)?;

    match value {
        // Format 1: JSON string (from environment variables)
        // Example: "[{\"type\":\"exclude_at_match\",\"name\":\"health\",\"pattern\":\"GET /health\"}]"
        JsonValue::String(s) => match serde_json::from_str(&s) {
            Ok(values) => Ok(Some(values)),
            Err(e) => {
                // Invalid JSON string - log error and return None
                tracing::error!("Failed to parse processing rules: {}, ignoring", e);
                Ok(None)
            }
        },
        // Format 2: JSON array (from YAML files)
        // Parse each rule individually, collecting successful parses
        JsonValue::Array(a) => {
            let mut values = Vec::new();
            for v in a {
                match serde_json::from_value(v.clone()) {
                    Ok(rule) => values.push(rule),
                    Err(e) => {
                        // Invalid rule - log error and skip this rule
                        tracing::error!("Failed to parse processing rule: {}, ignoring", e);
                    }
                }
            }
            // Return None if all rules were invalid, otherwise return the valid rules
            if values.is_empty() {
                Ok(None)
            } else {
                Ok(Some(values))
            }
        }
        // Unexpected format - return None
        _ => Ok(None),
    }
}
