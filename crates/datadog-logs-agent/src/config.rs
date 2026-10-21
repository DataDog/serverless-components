// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use crate::constants::{DEFAULT_COMPRESSION_LEVEL, DEFAULT_FLUSH_TIMEOUT_SECS, DEFAULT_SITE};
use crate::logs_additional_endpoint::{LogsAdditionalEndpoint, parse_additional_endpoints};

/// Controls where and how logs are shipped.
#[derive(Debug, Clone)]
pub enum Destination {
    /// Ship to Datadog Logs API.
    /// Endpoint: `https://http-intake.logs.{site}/api/v2/logs`
    /// Headers: `DD-API-KEY`, `DD-PROTOCOL: agent-json`, optionally `Content-Encoding: zstd`
    Datadog,

    /// Ship to an Observability Pipelines Worker.
    /// Endpoint: the provided URL.
    /// Headers: `DD-API-KEY` only. Compression is always disabled for OPW.
    ObservabilityPipelinesWorker { url: String },
}

/// Configuration for [`LogFlusher`](crate::flusher::LogFlusher).
#[derive(Debug, Clone)]
pub struct LogFlusherConfig {
    /// Datadog API key.
    pub api_key: String,

    /// Datadog site (e.g. "datadoghq.com", "datadoghq.eu").
    pub site: String,

    /// Flusher mode — Datadog vs Observability Pipelines Worker.
    pub mode: Destination,

    /// Additional Datadog intake endpoints to ship each batch to in parallel.
    /// Each endpoint uses its own API key and full intake URL.
    pub additional_endpoints: Vec<LogsAdditionalEndpoint>,

    /// Enable zstd compression (ignored in OPW mode, which is always uncompressed).
    pub use_compression: bool,

    /// zstd compression level (ignored when `use_compression` is false).
    pub compression_level: i32,

    /// Per-request timeout.
    pub flush_timeout: Duration,
}

impl LogFlusherConfig {
    /// Build a config from environment variables, falling back to sensible defaults.
    ///
    /// | Variable | Default |
    /// |---|---|
    /// | `DD_API_KEY` | `""` |
    /// | `DD_SITE` | `datadoghq.com` |
    /// | `DD_LOGS_CONFIG_USE_COMPRESSION` | `true` |
    /// | `DD_LOGS_CONFIG_COMPRESSION_LEVEL` | `3` |
    /// | `DD_FLUSH_TIMEOUT` | `5` (seconds) |
    /// | `DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED` | `false` |
    /// | `DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL` | (none) |
    #[must_use]
    pub fn from_env() -> Self {
        let api_key = std::env::var("DD_API_KEY").unwrap_or_default();
        let site = std::env::var("DD_SITE").unwrap_or_else(|_| DEFAULT_SITE.to_string());

        let use_compression = std::env::var("DD_LOGS_CONFIG_USE_COMPRESSION")
            .map(|v| v.to_lowercase() != "false")
            .unwrap_or(true);

        let compression_level = std::env::var("DD_LOGS_CONFIG_COMPRESSION_LEVEL")
            .ok()
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(DEFAULT_COMPRESSION_LEVEL);

        let flush_timeout_secs = std::env::var("DD_FLUSH_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_FLUSH_TIMEOUT_SECS);

        let opw_enabled = std::env::var("DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED")
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(false);

        let mode = if opw_enabled {
            let url =
                std::env::var("DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL").unwrap_or_default();
            if url.is_empty() {
                tracing::warn!(
                    "OPW mode enabled but DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL is not set — log flush will fail"
                );
            }
            Destination::ObservabilityPipelinesWorker { url }
        } else {
            Destination::Datadog
        };

        Self {
            api_key,
            site,
            mode,
            additional_endpoints: std::env::var("DD_LOGS_CONFIG_ADDITIONAL_ENDPOINTS")
                .map(|v| parse_additional_endpoints(&v))
                .unwrap_or_default(),
            use_compression,
            compression_level,
            flush_timeout: Duration::from_secs(flush_timeout_secs),
        }
    }
}

/// Optional flush behavior. `FlushPolicy::default()` is the crate's standard behavior.
/// Build it with `..FlushPolicy::default()` so new fields don't break callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlushPolicy {
    /// `false` (default): 4xx responses other than 408/425/429 are dropped after one attempt.
    /// `true`: every non-2xx response except 403 is retried and, if it keeps failing,
    /// handed back. 403 is always dropped.
    pub retry_client_errors: bool,
    /// `false` (default): additional-endpoint failures are dropped with a warning.
    /// `true`: they are handed back from `flush` / `retry` alongside primary failures.
    pub return_additional_endpoint_failures: bool,
    /// `false` (default): per batch, additional endpoints are sent after the primary
    /// request (including its retries) finishes. `true`: they are sent at the same time.
    pub parallel_additional_endpoints: bool,
    /// How failed requests are logged.
    pub failure_logging: FailureLogging,
}

/// How the flusher logs failed requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum FailureLogging {
    /// One warning per failed attempt, plus one when giving up (default).
    #[default]
    PerAttempt,
    /// One error when a request gives up or is dropped; individual attempts log at debug.
    /// The warning for a dropped additional-endpoint failure and the compression-fallback
    /// warning still log at warn.
    OncePerRequest,
}

impl Default for FlushPolicy {
    fn default() -> Self {
        Self {
            retry_client_errors: false,
            return_additional_endpoint_failures: false,
            parallel_additional_endpoints: false,
            failure_logging: FailureLogging::PerAttempt,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENV_VARS: [&str; 8] = [
        "DD_API_KEY",
        "DD_SITE",
        "DD_LOGS_CONFIG_USE_COMPRESSION",
        "DD_LOGS_CONFIG_COMPRESSION_LEVEL",
        "DD_FLUSH_TIMEOUT",
        "DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED",
        "DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL",
        "DD_LOGS_CONFIG_ADDITIONAL_ENDPOINTS",
    ];

    fn clear_env() {
        for var in ENV_VARS {
            // SAFETY: other threads in this test binary only access the environment through
            // std::env (which takes std's env lock); nothing calls libc getenv/setenv directly.
            unsafe { std::env::remove_var(var) };
        }
    }

    fn set_env(var: &str, value: &str) {
        // SAFETY: other threads in this test binary only access the environment through
        // std::env (which takes std's env lock); nothing calls libc getenv/setenv directly.
        unsafe { std::env::set_var(var, value) };
    }

    // Kept as one test so the env var changes cannot race with each other.
    #[test]
    fn test_from_env_defaults() {
        clear_env();

        let config = LogFlusherConfig::from_env();

        assert_eq!(config.api_key, "");
        assert_eq!(config.site, "datadoghq.com");
        assert!(matches!(config.mode, Destination::Datadog));
        assert!(config.additional_endpoints.is_empty());
        assert!(config.use_compression);
        assert_eq!(config.compression_level, 3);
        assert_eq!(config.flush_timeout, Duration::from_secs(5));

        set_env("DD_LOGS_CONFIG_USE_COMPRESSION", "false");
        assert!(!LogFlusherConfig::from_env().use_compression);
        clear_env();

        // A URL alone does not select OPW mode.
        set_env(
            "DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL",
            "http://opw.local:8080",
        );
        assert!(matches!(
            LogFlusherConfig::from_env().mode,
            Destination::Datadog
        ));

        set_env("DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED", "true");
        assert!(matches!(
            LogFlusherConfig::from_env().mode,
            Destination::ObservabilityPipelinesWorker { url } if url == "http://opw.local:8080"
        ));
        clear_env();
    }
}
