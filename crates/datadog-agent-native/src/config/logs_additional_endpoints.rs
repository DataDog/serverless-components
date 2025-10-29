//! Additional logs endpoints for dual-shipping logs to multiple Datadog organizations.
//!
//! This module supports sending logs to multiple Datadog organizations simultaneously,
//! useful for:
//! - **Multi-tenancy**: Separate logs for different customers/teams
//! - **Data sovereignty**: Keep logs in specific regions
//! - **Migration**: Send logs to both old and new organizations during transitions
//! - **Redundancy**: Backup logs to secondary organizations
//!
//! # Configuration
//!
//! Additional logs endpoints are configured via:
//! - **Environment variable**: `DD_LOGS_CONFIG_ADDITIONAL_ENDPOINTS='[{"api_key":"key2","Host":"logs.datadoghq.eu","Port":443,"is_reliable":true}]'`
//!
//! # Format
//!
//! Each endpoint specifies:
//! - `api_key`: Datadog API key for the target organization
//! - `Host`: Logs intake hostname (e.g., `agent-http-intake.logs.datadoghq.com`)
//! - `Port`: HTTPS port (typically 443)
//! - `is_reliable`: Whether to use TCP (true) or UDP (false) transport
//!
//! # Example
//!
//! ```json
//! [
//!   {
//!     "api_key": "secondary_org_key",
//!     "Host": "agent-http-intake.logs.datadoghq.eu",
//!     "Port": 443,
//!     "is_reliable": true
//!   }
//! ]
//! ```

use serde::{Deserialize, Deserializer};
use serde_json::Value;
use tracing::error;

/// Configuration for an additional logs endpoint.
///
/// Defines a secondary Datadog organization to send logs to, in addition to the primary
/// endpoint. Useful for dual-shipping logs to multiple organizations simultaneously.
///
/// # Fields
///
/// - `api_key`: Datadog API key for the target organization (must have log write permissions)
/// - `host`: Logs intake hostname for the target Datadog site
/// - `port`: HTTPS port (typically 443 for secure connections)
/// - `is_reliable`: Transport reliability (`true` for TCP, `false` for UDP)
///
/// # Transport Reliability
///
/// - **TCP (is_reliable=true)**: Guaranteed delivery, higher overhead, better for critical logs
/// - **UDP (is_reliable=false)**: Best-effort delivery, lower overhead, acceptable log loss
///
/// # Common Hosts
///
/// | Datadog Site | Logs Intake Host |
/// |-------------|------------------|
/// | US1 (default) | `agent-http-intake.logs.datadoghq.com` |
/// | EU | `agent-http-intake.logs.datadoghq.eu` |
/// | US3 | `agent-http-intake.logs.us3.datadoghq.com` |
/// | US5 | `agent-http-intake.logs.us5.datadoghq.com` |
/// | AP1 | `agent-http-intake.logs.ap1.datadoghq.com` |
/// | US1-FED | `agent-http-intake.logs.ddog-gov.com` |
#[derive(Debug, PartialEq, Clone, Deserialize)]
pub struct LogsAdditionalEndpoint {
    /// Datadog API key for the target organization.
    ///
    /// Must have log write permissions. This is typically a different API key
    /// than the primary endpoint to separate organizations.
    pub api_key: String,
    /// Logs intake hostname for the target Datadog site.
    ///
    /// **Note**: Field name is capitalized ("Host") to match Datadog's JSON format.
    ///
    /// **Examples**:
    /// - US1: `"agent-http-intake.logs.datadoghq.com"`
    /// - EU: `"agent-http-intake.logs.datadoghq.eu"`
    #[serde(rename = "Host")]
    pub host: String,
    /// HTTPS port for the logs intake endpoint.
    ///
    /// **Note**: Field name is capitalized ("Port") to match Datadog's JSON format.
    ///
    /// Typically `443` for secure HTTPS connections.
    #[serde(rename = "Port")]
    pub port: u32,
    /// Transport reliability flag.
    ///
    /// - `true`: Use TCP for guaranteed delivery (recommended for critical logs)
    /// - `false`: Use UDP for best-effort delivery (lower overhead, some logs may be lost)
    pub is_reliable: bool,
}

/// Deserializes additional logs endpoints from JSON strings.
///
/// # Input Format
///
/// Expects a JSON array encoded as a string (from environment variables):
/// ```json
/// "[{\"api_key\":\"key2\",\"Host\":\"logs.datadoghq.eu\",\"Port\":443,\"is_reliable\":true}]"
/// ```
///
/// # Error Handling
///
/// - Invalid JSON logs an error and returns an empty vector
/// - Empty strings return an empty vector
/// - Non-string inputs return an empty vector
///
/// This lenient behavior ensures the agent can start even if additional endpoints
/// are misconfigured - logs will only go to the primary endpoint.
///
/// # Examples
///
/// ```
/// use datadog_agent_native::config::logs_additional_endpoints::LogsAdditionalEndpoint;
/// use serde_json::json;
///
/// // Valid endpoint
/// let input = json!("[{\"api_key\":\"key2\",\"Host\":\"logs.datadoghq.eu\",\"Port\":443,\"is_reliable\":true}]");
/// // Result: Vec with one endpoint
///
/// // Invalid JSON returns empty vector (with error log)
/// let input = json!("invalid-json");
/// // Result: Empty vec![]
///
/// // Empty string returns empty vector
/// let input = json!("");
/// // Result: Empty vec![]
/// ```
#[allow(clippy::module_name_repetitions)]
pub fn deserialize_logs_additional_endpoints<'de, D>(
    deserializer: D,
) -> Result<Vec<LogsAdditionalEndpoint>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;

    match value {
        // JSON string format (from environment variables)
        // Example: "[{\"api_key\":\"key2\",\"Host\":\"logs.datadoghq.eu\",\"Port\":443,\"is_reliable\":true}]"
        Value::String(s) if !s.is_empty() => {
            // Parse the JSON string into a vector of endpoints
            Ok(serde_json::from_str(&s).unwrap_or_else(|err| {
                // Invalid JSON - log error and return empty vector
                // This ensures the agent can start even with invalid additional endpoints
                error!("Failed to deserialize DD_LOGS_CONFIG_ADDITIONAL_ENDPOINTS: {err}");
                vec![]
            }))
        }
        // Empty string or non-string value - return empty vector
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_deserialize_logs_additional_endpoints_valid() {
        let input = json!(
            "[{\"api_key\": \"apiKey2\", \"Host\": \"agent-http-intake.logs.datadoghq.com\", \"Port\": 443, \"is_reliable\": true}]"
        );

        let result = deserialize_logs_additional_endpoints(input)
            .expect("Failed to deserialize logs additional endpoints");
        let expected = vec![LogsAdditionalEndpoint {
            api_key: "apiKey2".to_string(),
            host: "agent-http-intake.logs.datadoghq.com".to_string(),
            port: 443,
            is_reliable: true,
        }];

        assert_eq!(result, expected);
    }

    #[test]
    fn test_deserialize_logs_additional_endpoints_invalid() {
        // input missing "Port" field
        let input = json!(
            "[{\"api_key\": \"apiKey2\", \"Host\": \"agent-http-intake.logs.datadoghq.com\", \"is_reliable\": true}]"
        );

        let result = deserialize_logs_additional_endpoints(input)
            .expect("Failed to deserialize logs additional endpoints");
        let expected = Vec::new(); // expect empty list due to invalid input

        assert_eq!(result, expected);
    }
}
