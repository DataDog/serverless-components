// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

/// A single log entry in the Datadog Logs intake format.
///
/// Standard Datadog fields are typed fields. Runtime-specific enrichment
/// (e.g. `{"lambda": {"arn": "...", "request_id": "..."}}` for Lambda,
/// `{"azure": {"resource_id": "..."}}` for Azure Functions) goes in `attributes`,
/// which is flattened into the JSON object at serialization time.
///
/// # Example — Lambda extension consumer
/// ```ignore
/// let mut attrs = serde_json::Map::new();
/// attrs.insert("lambda".to_string(), serde_json::json!({
///     "arn": function_arn,
///     "request_id": request_id,
/// }));
/// let entry = LogEntry {
///     message: log_line,
///     timestamp: timestamp_ms,
///     hostname: Some(function_arn),
///     service: Some(service_name),
///     ddsource: Some("lambda".to_string()),
///     ddtags: Some(tags),
///     status: Some("info".to_string()),
///     attributes: attrs,
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// The log message body.
    pub message: String,

    /// Unix timestamp in milliseconds.
    pub timestamp: i64,

    /// The hostname (e.g. Lambda function ARN, Azure resource ID).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,

    /// The service name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,

    /// The log source tag (e.g. "lambda", "azure-functions", "gcp-functions").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ddsource: Option<String>,

    /// Comma-separated Datadog tags (e.g. "env:prod,version:1.0").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ddtags: Option<String>,

    /// Log level / status (e.g. "info", "error", "warn", "debug").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,

    /// Runtime-specific enrichment fields, flattened into the JSON object.
    /// Use this for fields like `{"lambda": {"arn": "...", "request_id": "..."}}`.
    /// An empty map is not serialized. Extra fields in JSON are collected here on deserialization.
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub attributes: serde_json::Map<String, serde_json::Value>,
}

impl LogEntry {
    /// Create a minimal log entry from a message and timestamp.
    /// All optional fields default to `None`; use struct literal syntax to set them.
    pub fn from_message(message: impl Into<String>, timestamp: i64) -> Self {
        Self {
            message: message.into(),
            timestamp,
            hostname: None,
            service: None,
            ddsource: None,
            ddtags: None,
            status: None,
            attributes: serde_json::Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_entry_minimal_serialization() {
        let entry = LogEntry::from_message("hello world", 1_700_000_000_000);
        let json = serde_json::to_string(&entry).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");

        assert_eq!(v["message"], "hello world");
        assert_eq!(v["timestamp"], 1_700_000_000_000_i64);
        // Optional fields must be absent when None
        assert!(v.get("hostname").is_none());
        assert!(v.get("service").is_none());
        assert!(v.get("ddsource").is_none());
        assert!(v.get("ddtags").is_none());
        assert!(v.get("status").is_none());
    }

    #[test]
    fn test_log_entry_full_serialization() {
        let entry = LogEntry {
            message: "user logged in".to_string(),
            timestamp: 1_700_000_001_000,
            hostname: Some("my-host".to_string()),
            service: Some("my-service".to_string()),
            ddsource: Some("lambda".to_string()),
            ddtags: Some("env:prod,version:1.0".to_string()),
            status: Some("info".to_string()),
            attributes: serde_json::Map::new(),
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");

        assert_eq!(v["message"], "user logged in");
        assert_eq!(v["hostname"], "my-host");
        assert_eq!(v["service"], "my-service");
        assert_eq!(v["ddsource"], "lambda");
        assert_eq!(v["ddtags"], "env:prod,version:1.0");
        assert_eq!(v["status"], "info");
        assert!(
            v.get("attributes").is_none(),
            "empty attributes must not appear in output"
        );
    }

    #[test]
    fn test_log_entry_with_lambda_attributes_flattened() {
        // Simulates what the lambda extension would build
        let mut attrs = serde_json::Map::new();
        attrs.insert(
            "lambda".to_string(),
            serde_json::json!({
                "arn": "arn:aws:lambda:us-east-1:123456789012:function:my-fn",
                "request_id": "abc-123"
            }),
        );
        let entry = LogEntry {
            message: "function invoked".to_string(),
            timestamp: 1_700_000_002_000,
            hostname: Some("arn:aws:lambda:us-east-1:123456789012:function:my-fn".to_string()),
            service: Some("my-fn".to_string()),
            ddsource: Some("lambda".to_string()),
            ddtags: Some("env:prod".to_string()),
            status: Some("info".to_string()),
            attributes: attrs,
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");

        // Lambda-specific fields appear at top level (flattened)
        assert_eq!(
            v["lambda"]["arn"],
            "arn:aws:lambda:us-east-1:123456789012:function:my-fn"
        );
        assert_eq!(v["lambda"]["request_id"], "abc-123");
        assert_eq!(v["message"], "function invoked");
    }

    #[test]
    fn test_log_entry_deserialization_roundtrip() {
        let original = LogEntry {
            message: "test".to_string(),
            timestamp: 42,
            hostname: Some("h".to_string()),
            service: None,
            ddsource: Some("gcp-functions".to_string()),
            ddtags: None,
            status: Some("error".to_string()),
            attributes: serde_json::Map::new(),
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: LogEntry = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.message, original.message);
        assert_eq!(restored.timestamp, original.timestamp);
        assert_eq!(restored.hostname, original.hostname);
        assert_eq!(restored.ddsource, original.ddsource);
        assert_eq!(restored.status, original.status);
        assert!(
            restored.attributes.is_empty(),
            "no extra attributes expected after roundtrip"
        );
    }
}
