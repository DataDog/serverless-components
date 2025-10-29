//! Service name mapping for trace obfuscation.
//!
//! This module provides deserialization support for service name mappings, which allow
//! users to rename services in their traces. This is useful for:
//! - Consolidating multiple service names into one
//! - Renaming legacy service names to match new naming conventions
//! - Hiding internal service names from external traces
//!
//! # Format
//!
//! Service mappings are provided as a comma-separated list of colon-separated pairs:
//! ```text
//! old-service-1:new-service-1,old-service-2:new-service-2
//! ```
//!
//! # Example
//!
//! ```bash
//! DD_SERVICE_MAPPING="legacy-api:api,old-db:database"
//! ```
//!
//! This will rename all spans from `legacy-api` to `api` and all spans from `old-db` to `database`.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer};

/// Deserializes service name mappings from a comma-separated string of colon-separated pairs.
///
/// # Format
///
/// Input string format: `"service1:mapped1,service2:mapped2,..."`
///
/// Each mapping consists of:
/// - **Original service name** (before the colon)
/// - **Mapped service name** (after the colon)
///
/// # Whitespace Handling
///
/// Leading and trailing whitespace in service names is automatically trimmed:
/// - `"  old-service  :  new-service  "` becomes `{"old-service": "new-service"}`
///
/// # Error Handling
///
/// Invalid pairs (missing colon, empty parts) are logged and skipped. The function
/// never fails - it returns an empty map if all pairs are invalid.
///
/// # Examples
///
/// ```
/// use std::collections::HashMap;
/// use serde_json::json;
///
/// // Valid mapping
/// let input = json!("legacy-api:api,old-db:database");
/// // Result: {"legacy-api": "api", "old-db": "database"}
///
/// // With whitespace (automatically trimmed)
/// let input = json!("  service1  :  renamed1  ,  service2  :  renamed2  ");
/// // Result: {"service1": "renamed1", "service2": "renamed2"}
///
/// // Invalid pairs are skipped
/// let input = json!("valid:mapping,invalid-no-colon,another:valid");
/// // Result: {"valid": "mapping", "another": "valid"}
/// // "invalid-no-colon" is logged as an error and skipped
/// ```
#[allow(clippy::module_name_repetitions)]
pub fn deserialize_service_mapping<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = String::deserialize(deserializer)?;

    let map = s
        .split(',')
        .filter_map(|pair| {
            // Split each pair on ':' to extract service name and mapped name
            let mut split = pair.split(':');

            let service = split.next();
            let to_map = split.next();

            // Both parts must exist for a valid mapping
            if let (Some(service), Some(to_map)) = (service, to_map) {
                // Trim whitespace from both parts and create the mapping
                Some((service.trim().to_string(), to_map.trim().to_string()))
            } else {
                // Invalid format - log error and skip this pair
                tracing::error!("Failed to parse service mapping '{}', expected format 'service:mapped_service', ignoring", pair.trim());
                None
            }
        })
        .collect();

    Ok(map)
}
