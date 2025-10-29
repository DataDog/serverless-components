//! Additional endpoints for dual-shipping metrics and traces to multiple Datadog organizations.
//!
//! This module supports sending telemetry (metrics and traces) to multiple Datadog organizations
//! simultaneously. This is useful for:
//! - **Multi-tenancy**: Route data to different organizations per customer/team
//! - **Data sovereignty**: Keep data in specific geographic regions
//! - **Migration**: Send data to both old and new organizations during transitions
//! - **Redundancy**: Backup critical telemetry to secondary organizations
//!
//! # Configuration
//!
//! Additional endpoints are configured via:
//! - **Environment variable**: `DD_ADDITIONAL_ENDPOINTS='{"https://app.datadoghq.eu":["api_key_1","api_key_2"]}'`
//! - **YAML config**: See example below
//!
//! # Format
//!
//! The configuration is a map where:
//! - **Keys**: Datadog site URLs (e.g., `"https://app.datadoghq.com"`, `"https://app.datadoghq.eu"`)
//! - **Values**: Array of API keys for each site
//!
//! Multiple API keys per site allow sending data to multiple organizations in the same region.
//!
//! # Examples
//!
//! **YAML format** (`datadog.yaml`):
//! ```yaml
//! additional_endpoints:
//!   "https://app.datadoghq.com":
//!     - secondary_us_api_key_1
//!     - secondary_us_api_key_2
//!   "https://app.datadoghq.eu":
//!     - eu_api_key
//! ```
//!
//! **JSON format** (environment variable):
//! ```bash
//! DD_ADDITIONAL_ENDPOINTS='{"https://app.datadoghq.com":["key1","key2"],"https://app.datadoghq.eu":["key3"]}'
//! ```
//!
//! # Common Datadog Sites
//!
//! | Site | URL | Region |
//! |------|-----|--------|
//! | US1 | `https://app.datadoghq.com` | United States (default) |
//! | EU | `https://app.datadoghq.eu` | Europe (Frankfurt) |
//! | US3 | `https://us3.datadoghq.com` | United States (Oregon) |
//! | US5 | `https://us5.datadoghq.com` | United States |
//! | AP1 | `https://ap1.datadoghq.com` | Asia Pacific (Tokyo) |
//! | US1-FED | `https://app.ddog-gov.com` | US Government (FedRAMP) |

use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::HashMap;
use tracing::error;

/// Deserializes additional endpoints from config sources.
///
/// Handles two input formats:
/// - **JSON string**: Endpoints encoded as a JSON object string (from environment variables)
/// - **YAML object**: Direct map of site URLs to API key arrays (from YAML files)
///
/// # Format Details
///
/// The resulting `HashMap<String, Vec<String>>` maps:
/// - **Key**: Datadog site URL (e.g., `"https://app.datadoghq.com"`)
/// - **Value**: Vector of API keys to send data to for that site
///
/// # Error Handling
///
/// This implementation is lenient:
/// - Invalid YAML values (non-arrays) are logged and skipped for that key
/// - Invalid JSON strings are logged and return an empty map
/// - Empty strings or unexpected types return an empty map
///
/// This ensures the agent can start even if additional endpoints are misconfigured -
/// data will only go to the primary endpoint.
///
/// # Examples
///
/// **YAML format** (from `datadog.yaml`):
/// ```yaml
/// additional_endpoints:
///   "https://app.datadoghq.com":
///     - apikey2
///     - apikey3
///   "https://app.datadoghq.eu":
///     - apikey4
/// ```
/// Result: `{"https://app.datadoghq.com": ["apikey2", "apikey3"], "https://app.datadoghq.eu": ["apikey4"]}`
///
/// **JSON format** (from `DD_ADDITIONAL_ENDPOINTS`):
/// ```json
/// {"https://app.datadoghq.com":["apikey2","apikey3"],"https://app.datadoghq.eu":["apikey4"]}
/// ```
/// Result: Same as above
///
/// **Invalid YAML** (non-array value):
/// ```yaml
/// additional_endpoints:
///   "https://app.datadoghq.com":
///     - apikey2
///   "https://app.datadoghq.eu": "not-an-array"  # Invalid!
/// ```
/// Result: `{"https://app.datadoghq.com": ["apikey2"]}` (invalid entry skipped with error log)
#[allow(clippy::module_name_repetitions)]
pub fn deserialize_additional_endpoints<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;

    match value {
        // Format 1: YAML object (from datadog.yaml)
        // Example: {"https://app.datadoghq.com": ["apikey2", "apikey3"]}
        Value::Object(map) => {
            let mut result = HashMap::new();
            for (key, value) in map {
                match value {
                    // Valid: array of API keys
                    Value::Array(arr) => {
                        // Extract strings from the array, filtering out non-string values
                        let urls: Vec<String> = arr
                            .into_iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect();
                        result.insert(key, urls);
                    }
                    // Invalid: expected an array of API keys
                    _ => {
                        error!(
                            "Failed to deserialize additional endpoints - Invalid YAML format: expected array for key {}",
                            key
                        );
                    }
                }
            }
            Ok(result)
        }
        // Format 2: JSON string (from environment variables like DD_ADDITIONAL_ENDPOINTS)
        // Example: "{\"https://app.datadoghq.com\":[\"apikey2\",\"apikey3\"]}"
        Value::String(s) if !s.is_empty() => {
            // Parse the JSON string into a HashMap
            if let Ok(map) = serde_json::from_str(&s) {
                Ok(map)
            } else {
                // Invalid JSON - log error and return empty map
                error!("Failed to deserialize additional endpoints - Invalid JSON format");
                Ok(HashMap::new())
            }
        }
        // Empty string or unexpected type - return empty map
        _ => Ok(HashMap::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_deserialize_additional_endpoints_yaml() {
        // Test YAML format (object)
        let input = json!({
            "https://app.datadoghq.com": ["key1", "key2"],
            "https://app.datadoghq.eu": ["key3"]
        });

        let result = deserialize_additional_endpoints(input)
            .expect("Failed to deserialize additional endpoints");

        let mut expected = HashMap::new();
        expected.insert(
            "https://app.datadoghq.com".to_string(),
            vec!["key1".to_string(), "key2".to_string()],
        );
        expected.insert(
            "https://app.datadoghq.eu".to_string(),
            vec!["key3".to_string()],
        );

        assert_eq!(result, expected);
    }

    #[test]
    fn test_deserialize_additional_endpoints_json() {
        // Test JSON string format
        let input = json!(
            "{\"https://app.datadoghq.com\":[\"key1\",\"key2\"],\"https://app.datadoghq.eu\":[\"key3\"]}"
        );

        let result = deserialize_additional_endpoints(input)
            .expect("Failed to deserialize additional endpoints");

        let mut expected = HashMap::new();
        expected.insert(
            "https://app.datadoghq.com".to_string(),
            vec!["key1".to_string(), "key2".to_string()],
        );
        expected.insert(
            "https://app.datadoghq.eu".to_string(),
            vec!["key3".to_string()],
        );

        assert_eq!(result, expected);
    }

    #[test]
    fn test_deserialize_additional_endpoints_invalid_or_empty() {
        // Test empty YAML
        let input = json!({});
        let result = deserialize_additional_endpoints(input)
            .expect("Failed to deserialize additional endpoints");
        assert!(result.is_empty());

        // Test empty JSON
        let input = json!("");
        let result = deserialize_additional_endpoints(input)
            .expect("Failed to deserialize additional endpoints");
        assert!(result.is_empty());

        let input = json!({
            "https://app.datadoghq.com": "invalid-yaml"
        });
        let result = deserialize_additional_endpoints(input)
            .expect("Failed to deserialize additional endpoints");
        assert!(result.is_empty());

        let input = json!("invalid-json");
        let result = deserialize_additional_endpoints(input)
            .expect("Failed to deserialize additional endpoints");
        assert!(result.is_empty());
    }
}
