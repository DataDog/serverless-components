// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use crate::constants::{DEFAULT_COMPRESSION_LEVEL, DEFAULT_FLUSH_TIMEOUT_SECS, DEFAULT_SITE};
use crate::logs_additional_endpoint::{LogsAdditionalEndpoint, parse_additional_endpoints};

/// Controls where and how logs are shipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// Ship to Datadog Logs API.
    /// Endpoint: `https://http-intake.logs.{site}/api/v2/logs`
    /// Headers: `DD-API-KEY`, `DD-PROTOCOL: agent-json`, optionally `Content-Encoding: zstd`
    Datadog,

    /// Ship to an Observability Pipelines Worker.
    /// Endpoint: the provided URL.
    /// Headers: `DD-API-KEY` only. Compression is always disabled for OPW.
    ObservabilityPipelinesWorker { url: String },

    /// Ship to a Datadog logs intake at a full URL.
    /// Endpoint: the provided URL.
    /// Headers: `DD-API-KEY`, optionally `Content-Encoding: zstd`, and the optional `headers`
    /// that are set. Compression: as `Datadog`.
    CustomUrl {
        url: String,
        headers: CustomUrlHeaders,
    },
}

/// The optional headers of a [`Destination::CustomUrl`]. None is sent by default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CustomUrlHeaders {
    /// `DD-PROTOCOL`.
    pub protocol: Option<String>,
    /// `DD-EVP-ORIGIN` and `DD-EVP-ORIGIN-VERSION`.
    pub origin: Option<IntakeOrigin>,
    /// `User-Agent`.
    pub user_agent: Option<String>,
    /// `dd-message-timestamp` and `dd-current-timestamp`, in Unix milliseconds.
    pub timestamps: bool,
}

/// The product that sends the logs, in the `DD-EVP-ORIGIN` and `DD-EVP-ORIGIN-VERSION` headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakeOrigin {
    pub name: String,
    pub version: String,
}

/// Configuration for [`LogFlusher`](crate::flusher::LogFlusher).
#[derive(Debug, Clone)]
pub struct LogFlusherConfig {
    /// Datadog API key.
    pub api_key: String,

    /// Datadog site (e.g. "datadoghq.com", "datadoghq.eu").
    pub site: String,

    /// Where logs are shipped.
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
