//! Carrier traits for trace context propagation.
//!
//! Carriers provide an abstraction for reading from and writing to different
//! transport mechanisms (HTTP headers, message queue metadata, etc.) used to
//! propagate trace context across service boundaries.
//!
//! # Carrier Types
//!
//! This module implements carriers for:
//! - **HashMap**: For testing and in-memory operations
//! - **serde_json::Value**: For JSON-based message formats
//!
//! # Case Insensitivity
//!
//! All carrier implementations are case-insensitive to handle HTTP header
//! normalization (e.g., `X-Datadog-Trace-Id` vs `x-datadog-trace-id`).
//!
//! # Inspired By
//!
//! Code inspired and adapted from the OpenTelemetry Rust project:
//! <https://github.com/open-telemetry/opentelemetry-rust/blob/main/opentelemetry/src/propagation/mod.rs>

use std::collections::HashMap;

use serde_json::Value;

/// Trait for injecting trace context into a carrier.
///
/// Injectors provide a generic interface for writing trace context (trace ID, span ID,
/// sampling decision) into various transport mechanisms like HTTP headers or message metadata.
///
/// # Case Handling
///
/// Keys are normalized to lowercase to ensure case-insensitive matching,
/// which is important for HTTP header compatibility.
///
/// # Example
///
/// ```
/// use std::collections::HashMap;
/// use datadog_agent_native::traces::propagation::carrier::Injector;
///
/// let mut headers = HashMap::new();
/// headers.set("X-Datadog-Trace-Id", "123456".to_string());
///
/// // Key is stored as lowercase
/// assert_eq!(headers.get("x-datadog-trace-id"), Some(&"123456".to_string()));
/// ```
pub trait Injector {
    /// Sets a key-value pair in the carrier.
    ///
    /// Keys are normalized to lowercase for case-insensitive matching.
    ///
    /// # Arguments
    ///
    /// * `key` - Header or metadata key (will be lowercased)
    /// * `value` - Value to associate with the key
    fn set(&mut self, key: &str, value: String);
}

/// Trait for extracting trace context from a carrier.
///
/// Extractors provide a generic interface for reading trace context (trace ID, span ID,
/// sampling decision) from various transport mechanisms like HTTP headers or message metadata.
///
/// # Case Handling
///
/// Keys are normalized to lowercase to ensure case-insensitive matching,
/// which is important for HTTP header compatibility.
///
/// # Example
///
/// ```
/// use std::collections::HashMap;
/// use datadog_agent_native::traces::propagation::carrier::{Injector, Extractor};
///
/// let mut headers = HashMap::new();
/// headers.set("X-Datadog-Trace-Id", "123456".to_string());
///
/// // Extract with different case - key was lowercased during set
/// assert_eq!(Extractor::get(&headers, "x-datadog-trace-id"), Some("123456"));
/// assert_eq!(Extractor::get(&headers, "X-DATADOG-TRACE-ID"), Some("123456"));
/// ```
pub trait Extractor {
    /// Gets a value from the carrier by key.
    ///
    /// Keys are normalized to lowercase for case-insensitive matching.
    ///
    /// # Arguments
    ///
    /// * `key` - Header or metadata key to retrieve (case-insensitive)
    ///
    /// # Returns
    ///
    /// `Some(&str)` if the key exists, `None` otherwise
    fn get(&self, key: &str) -> Option<&str>;

    /// Gets all keys present in the carrier.
    ///
    /// Returns lowercase keys for consistency.
    ///
    /// # Returns
    ///
    /// Vector of all keys in the carrier
    fn keys(&self) -> Vec<&str>;
}

/// `Injector` implementation for `HashMap`.
///
/// Stores keys in lowercase for case-insensitive matching.
impl<S: std::hash::BuildHasher> Injector for HashMap<String, String, S> {
    /// Sets a key-value pair in the HashMap with lowercase key normalization.
    ///
    /// # Example
    ///
    /// ```
    /// use std::collections::HashMap;
    /// use datadog_agent_native::traces::propagation::carrier::Injector;
    ///
    /// let mut headers = HashMap::new();
    /// headers.set("X-Datadog-Trace-Id", "123".to_string());
    ///
    /// // Stored as lowercase
    /// assert_eq!(headers.get("x-datadog-trace-id"), Some(&"123".to_string()));
    /// ```
    fn set(&mut self, key: &str, value: String) {
        // Normalize key to lowercase for case-insensitive lookup
        self.insert(key.to_lowercase(), value);
    }
}

/// `Extractor` implementation for `HashMap`.
///
/// Retrieves values using case-insensitive key matching.
impl<S: std::hash::BuildHasher> Extractor for HashMap<String, String, S> {
    /// Gets a value for a key from the HashMap (case-insensitive).
    ///
    /// # Example
    ///
    /// ```
    /// use std::collections::HashMap;
    /// use datadog_agent_native::traces::propagation::carrier::{Injector, Extractor};
    ///
    /// let mut headers = HashMap::new();
    /// headers.set("X-Datadog-Trace-Id", "123".to_string());
    ///
    /// // Case-insensitive retrieval
    /// assert_eq!(Extractor::get(&headers, "x-datadog-trace-id"), Some("123"));
    /// assert_eq!(Extractor::get(&headers, "X-DATADOG-TRACE-ID"), Some("123"));
    /// ```
    fn get(&self, key: &str) -> Option<&str> {
        // Normalize key to lowercase for lookup
        self.get(&key.to_lowercase()).map(String::as_str)
    }

    /// Collects all keys from the HashMap.
    ///
    /// Returns keys in their stored (lowercase) form.
    fn keys(&self) -> Vec<&str> {
        self.keys().map(String::as_str).collect::<Vec<_>>()
    }
}

/// `Injector` implementation for `serde_json::Value`.
///
/// Only works with `Value::Object` variants. Non-object values are silently ignored.
impl Injector for Value {
    /// Sets a key-value pair in a JSON object with lowercase key normalization.
    ///
    /// Does nothing if the `Value` is not an `Object` variant.
    ///
    /// # Example
    ///
    /// ```
    /// use serde_json::Value;
    /// use datadog_agent_native::traces::propagation::carrier::Injector;
    ///
    /// let mut json = Value::Object(serde_json::Map::new());
    /// json.set("X-Datadog-Trace-Id", "123".to_string());
    ///
    /// // Verify lowercase storage
    /// assert_eq!(json["x-datadog-trace-id"], "123");
    /// ```
    fn set(&mut self, key: &str, value: String) {
        // Only operate on JSON objects
        if let Value::Object(map) = self {
            // Normalize key to lowercase
            map.insert(key.to_lowercase(), Value::String(value));
        }
        // Silently ignore non-object values
    }
}

/// `Extractor` implementation for `serde_json::Value`.
///
/// Only works with `Value::Object` variants. Non-object values return `None`.
impl Extractor for Value {
    /// Gets a value for a key from a JSON object (case-insensitive).
    ///
    /// Returns `None` if the `Value` is not an `Object` or the key doesn't exist.
    ///
    /// # Example
    ///
    /// ```
    /// use serde_json::Value;
    /// use datadog_agent_native::traces::propagation::carrier::{Injector, Extractor};
    ///
    /// let mut json = Value::Object(serde_json::Map::new());
    /// json.set("X-Datadog-Trace-Id", "123".to_string());
    ///
    /// // Case-insensitive retrieval
    /// assert_eq!(Extractor::get(&json, "x-datadog-trace-id"), Some("123"));
    /// assert_eq!(Extractor::get(&json, "X-DATADOG-TRACE-ID"), Some("123"));
    /// ```
    fn get(&self, key: &str) -> Option<&str> {
        // Only operate on JSON objects
        if let Value::Object(map) = self {
            // Normalize key to lowercase and extract string value
            map.get(&key.to_lowercase()).and_then(|v| v.as_str())
        } else {
            None
        }
    }

    /// Collects all keys from a JSON object.
    ///
    /// Returns an empty vector if the `Value` is not an `Object`.
    fn keys(&self) -> Vec<&str> {
        if let Value::Object(map) = self {
            map.keys().map(String::as_str).collect::<Vec<_>>()
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn hash_map_get() {
        let mut carrier = HashMap::new();
        carrier.set("headerName", "value".to_string());

        assert_eq!(
            Extractor::get(&carrier, "HEADERNAME"),
            Some("value"),
            "case insensitive extraction"
        );
    }

    #[test]
    fn hash_map_keys() {
        let mut carrier = HashMap::new();
        carrier.set("headerName1", "value1".to_string());
        carrier.set("headerName2", "value2".to_string());

        let got = Extractor::keys(&carrier);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&"headername1"));
        assert!(got.contains(&"headername2"));
    }

    #[test]
    fn serde_value_get() {
        let mut carrier = Value::Object(serde_json::Map::new());
        carrier.set("headerName", "value".to_string());

        assert_eq!(
            Extractor::get(&carrier, "HEADERNAME"),
            Some("value"),
            "case insensitive extraction"
        );
    }

    #[test]
    fn serde_value_keys() {
        let mut carrier = Value::Object(serde_json::Map::new());
        carrier.set("headerName1", "value1".to_string());
        carrier.set("headerName2", "value2".to_string());

        let got = Extractor::keys(&carrier);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&"headername1"));
        assert!(got.contains(&"headername2"));
    }
}
