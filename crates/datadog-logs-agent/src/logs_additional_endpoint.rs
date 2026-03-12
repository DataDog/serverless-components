// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use serde::Deserialize;
use tracing::warn;

/// An additional Datadog intake endpoint to ship each log batch to alongside
/// the primary endpoint.
///
/// The JSON wire format (from `DD_LOGS_CONFIG_ADDITIONAL_ENDPOINTS`) matches the
/// bottlecap / datadog-agent convention:
/// ```json
/// [{"api_key":"<key>","Host":"agent-http-intake.logs.datadoghq.com","Port":443,"is_reliable":true}]
/// ```
#[derive(Debug, PartialEq, Clone)]
pub struct LogsAdditionalEndpoint {
    /// API key used exclusively for this endpoint.
    pub api_key: String,
    /// Full intake URL, e.g. `https://agent-http-intake.logs.datadoghq.com:443/api/v2/logs`.
    /// Computed from `Host` and `Port` at deserialize time.
    pub url: String,
    /// When `true`, failures on this endpoint are counted toward overall flush reliability.
    /// Currently stored but not yet acted upon; reserved for future use.
    pub is_reliable: bool,
}

/// Internal representation that mirrors the JSON wire format.
#[derive(Deserialize)]
struct RawEndpoint {
    api_key: String,
    #[serde(rename = "Host")]
    host: String,
    #[serde(rename = "Port")]
    port: u32,
    is_reliable: bool,
}

impl From<RawEndpoint> for LogsAdditionalEndpoint {
    fn from(r: RawEndpoint) -> Self {
        Self {
            api_key: r.api_key,
            url: format!("https://{}:{}/api/v2/logs", r.host, r.port),
            is_reliable: r.is_reliable,
        }
    }
}

/// Parse the value of `DD_LOGS_CONFIG_ADDITIONAL_ENDPOINTS` (a JSON array string).
///
/// Returns an empty `Vec` and emits a warning on parse failure.
pub fn parse_additional_endpoints(s: &str) -> Vec<LogsAdditionalEndpoint> {
    match serde_json::from_str::<Vec<RawEndpoint>>(s) {
        Ok(raw) => raw.into_iter().map(Into::into).collect(),
        Err(e) => {
            warn!("failed to parse DD_LOGS_CONFIG_ADDITIONAL_ENDPOINTS: {e}");
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_endpoint() {
        let s = r#"[{"api_key":"key2","Host":"agent-http-intake.logs.datadoghq.com","Port":443,"is_reliable":true}]"#;
        let result = parse_additional_endpoints(s);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].api_key, "key2");
        assert_eq!(
            result[0].url,
            "https://agent-http-intake.logs.datadoghq.com:443/api/v2/logs"
        );
        assert!(result[0].is_reliable);
    }

    #[test]
    fn test_parse_missing_port_returns_empty() {
        // Missing required "Port" field — should warn and return []
        let s = r#"[{"api_key":"key","Host":"intake.logs.datadoghq.com","is_reliable":true}]"#;
        let result = parse_additional_endpoints(s);
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_empty_string_returns_empty() {
        let result = parse_additional_endpoints("");
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_multiple_endpoints() {
        let s = r#"[
            {"api_key":"k1","Host":"host1.example.com","Port":443,"is_reliable":true},
            {"api_key":"k2","Host":"host2.example.com","Port":10516,"is_reliable":false}
        ]"#;
        let result = parse_additional_endpoints(s);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].url, "https://host1.example.com:443/api/v2/logs");
        assert_eq!(result[1].url, "https://host2.example.com:10516/api/v2/logs");
        assert!(!result[1].is_reliable);
    }
}
