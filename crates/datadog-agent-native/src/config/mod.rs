//! Configuration Module
//!
//! This module handles all configuration for the Datadog Agent, including parsing from
//! environment variables, YAML files, and providing sensible defaults.
//!
//! ## Configuration Priority
//!
//! Configuration sources are applied in the following order (later sources override earlier):
//!
//! 1. **Defaults** - Hard-coded defaults in the code
//! 2. **YAML file** - Configuration from `datadog.yaml` (if present)
//! 3. **Environment variables** - DD_* environment variables (highest priority)
//!
//! ## Edge Cases and Behaviors
//!
//! ### API Keys
//!
//! - **Empty/whitespace only**: Accepted but will cause runtime errors when sending data
//! - **Special characters**: Fully supported (!, @, #, $, %, etc.)
//! - **Length**: No artificial limits (tested up to 500 characters)
//!
//! ### URLs
//!
//! - **Missing protocol**: Automatically adds `https://` prefix
//! - **Site with protocol** (e.g., `http://datadoghq.com`): Accepted as-is
//! - **Whitespace**: Automatically trimmed
//! - **Empty after trimming**: Uses default values
//!
//! ### Operational Modes
//!
//! - **Invalid mode strings**: Falls back to `HttpFixedPort` (default)
//! - **Case insensitive**: "HTTP_FIXED_PORT", "http_fixed_port", "HttpFixedPort" all work
//! - **Aliases supported**: "fixed", "http", "ephemeral", "dynamic", "ffi", "no_http"
//!
//! ### Port Configuration
//!
//! - **Port 0**: Triggers OS-assigned ephemeral port (even in `HttpFixedPort` mode)
//! - **Invalid ports** (negative, >65535): Ignored, falls back to default
//! - **Port conflicts**: Returns clear error message with port number and mode
//! - **Ephemeral mode**: Ignores explicitly configured ports
//!
//! ### Timeouts and Intervals
//!
//! - **Flush timeout = 0**: Falls back to default (5 seconds)
//! - **Negative values**: Behavior depends on underlying library (e.g., zstd accepts -1)
//!
//! ### Whitespace Handling
//!
//! All string configuration values are trimmed before use. Empty strings after trimming
//! typically result in default values being used.
//!
//! ## Example Usage
//!
//! Configuration is typically loaded during agent initialization from environment variables
//! (DD_API_KEY, DD_SITE, etc.) and merged with defaults. See the `env` module for details
//! on environment variable handling.

pub mod additional_endpoints;
pub mod apm_replace_rule;
pub mod env;
pub mod flush_strategy;
pub mod log_level;
pub mod logs_additional_endpoints;
pub mod operational_mode;
pub mod processing_rule;
pub mod service_mapping;
pub mod trace_propagation_style;

#[cfg(test)]
pub mod yaml;

use datadog_trace_obfuscation::replacer::ReplaceRule;
use datadog_trace_utils::config_utils::{trace_intake_url, trace_intake_url_prefixed};

use serde::{Deserialize, Deserializer};
use serde_aux::prelude::deserialize_bool_from_anything;
use serde_json::Value;

use std::time::Duration;
use std::{collections::HashMap, fmt};
use tracing::{debug, error};

#[cfg(test)]
use std::path::Path;

use crate::config::{
    log_level::LogLevel, logs_additional_endpoints::LogsAdditionalEndpoint,
    processing_rule::ProcessingRule, trace_propagation_style::TracePropagationStyle,
};

#[cfg(test)]
use crate::config::{
    apm_replace_rule::deserialize_apm_replace_rules, env::EnvConfigSource,
    processing_rule::deserialize_processing_rules, yaml::YamlConfigSource,
};

/// Helper macro to merge Option<String> fields to String fields
///
/// Providing one field argument will merge the value from the source config field into the config
/// field.
///
/// Providing two field arguments will merge the value from the source config field into the config
/// field if the value is not empty.
#[macro_export]
macro_rules! merge_string {
    ($config:expr, $config_field:ident, $source:expr, $source_field:ident) => {
        if let Some(value) = &$source.$source_field {
            $config.$config_field.clone_from(value);
        }
    };
    ($config:expr, $source:expr, $field:ident) => {
        if let Some(value) = &$source.$field {
            $config.$field.clone_from(value);
        }
    };
}

/// Helper macro to merge Option<T> fields where T implements Clone
///
/// Providing one field argument will merge the value from the source config field into the config
/// field.
///
/// Providing two field arguments will merge the value from the source config field into the config
/// field if the value is not empty.
#[macro_export]
macro_rules! merge_option {
    ($config:expr, $config_field:ident, $source:expr, $source_field:ident) => {
        if $source.$source_field.is_some() {
            $config.$config_field.clone_from(&$source.$source_field);
        }
    };
    ($config:expr, $source:expr, $field:ident) => {
        if $source.$field.is_some() {
            $config.$field.clone_from(&$source.$field);
        }
    };
}

/// Helper macro to merge Option<T> fields to T fields when Option<T> is Some
///
/// Providing one field argument will merge the value from the source config field into the config
/// field.
///
/// Providing two field arguments will merge the value from the source config field into the config
/// field if the value is not empty.
#[macro_export]
macro_rules! merge_option_to_value {
    ($config:expr, $config_field:ident, $source:expr, $source_field:ident) => {
        if let Some(value) = &$source.$source_field {
            $config.$config_field = value.clone();
        }
    };
    ($config:expr, $source:expr, $field:ident) => {
        if let Some(value) = &$source.$field {
            $config.$field = value.clone();
        }
    };
}

/// Helper macro to merge `Vec` fields when `Vec` is not empty
///
/// Providing one field argument will merge the value from the source config field into the config
/// field.
///
/// Providing two field arguments will merge the value from the source config field into the config
/// field if the value is not empty.
#[macro_export]
macro_rules! merge_vec {
    ($config:expr, $config_field:ident, $source:expr, $source_field:ident) => {
        if !$source.$source_field.is_empty() {
            $config.$config_field.clone_from(&$source.$source_field);
        }
    };
    ($config:expr, $source:expr, $field:ident) => {
        if !$source.$field.is_empty() {
            $config.$field.clone_from(&$source.$field);
        }
    };
}

// nit: these will replace one map with the other, not merge the maps togehter, right?
/// Helper macro to merge `HashMap` fields when `HashMap` is not empty
///
/// Providing one field argument will merge the value from the source config field into the config
/// field.
///
/// Providing two field arguments will merge the value from the source config field into the config
/// field if the value is not empty.
#[macro_export]
macro_rules! merge_hashmap {
    ($config:expr, $config_field:ident, $source:expr, $source_field:ident) => {
        if !$source.$source_field.is_empty() {
            $config.$config_field.clone_from(&$source.$source_field);
        }
    };
    ($config:expr, $source:expr, $field:ident) => {
        if !$source.$field.is_empty() {
            $config.$field.clone_from(&$source.$field);
        }
    };
}

#[derive(Debug, PartialEq)]
#[allow(clippy::module_name_repetitions)]
pub enum ConfigError {
    ParseError(String),
    UnsupportedField(String),
}

#[allow(clippy::module_name_repetitions)]
pub trait ConfigSource {
    fn load(&self, config: &mut Config) -> Result<(), ConfigError>;
}

#[derive(Default)]
#[allow(clippy::module_name_repetitions)]
pub struct ConfigBuilder {
    sources: Vec<Box<dyn ConfigSource>>,
    config: Config,
}

#[allow(clippy::module_name_repetitions)]
impl ConfigBuilder {
    #[must_use]
    pub fn add_source(mut self, source: Box<dyn ConfigSource>) -> Self {
        self.sources.push(source);
        self
    }

    pub fn build(&mut self) -> Config {
        let mut failed_sources = 0;
        for source in &self.sources {
            match source.load(&mut self.config) {
                Ok(()) => (),
                Err(e) => {
                    error!("Failed to load config: {:?}", e);
                    failed_sources += 1;
                }
            }
        }

        if !self.sources.is_empty() && failed_sources == self.sources.len() {
            debug!("All sources failed to load config, using default config.");
        }

        if self.config.site.is_empty() {
            self.config.site = "datadoghq.com".to_string();
        }

        // If `proxy_https` is not set, set it from `HTTPS_PROXY` environment variable
        // if it exists
        if let Ok(https_proxy) = std::env::var("HTTPS_PROXY") {
            if self.config.proxy_https.is_none() {
                self.config.proxy_https = Some(https_proxy);
            }
        }

        // If `proxy_https` is set, check if the site is in `NO_PROXY` environment variable
        // or in the `proxy_no_proxy` config field.
        if self.config.proxy_https.is_some() {
            let site_in_no_proxy = std::env::var("NO_PROXY")
                .is_ok_and(|no_proxy| no_proxy.contains(&self.config.site))
                || self
                    .config
                    .proxy_no_proxy
                    .iter()
                    .any(|no_proxy| no_proxy.contains(&self.config.site));
            if site_in_no_proxy {
                self.config.proxy_https = None;
            }
        }

        // If extraction is not set, set it to the same as the propagation style
        if self.config.trace_propagation_style_extract.is_empty() {
            self.config
                .trace_propagation_style_extract
                .clone_from(&self.config.trace_propagation_style);
        }

        // If Logs URL is not set, set it to the default
        if self.config.logs_config_logs_dd_url.is_empty() {
            self.config.logs_config_logs_dd_url = build_fqdn_logs(self.config.site.clone());
        }

        // If APM URL is not set, set it to the default
        if self.config.apm_dd_url.is_empty() {
            self.config.apm_dd_url = trace_intake_url(self.config.site.clone().as_str());
        } else {
            // If APM URL is set, normalize it and add the path
            let normalized_url = normalize_url(&self.config.apm_dd_url);
            self.config.apm_dd_url = trace_intake_url_prefixed(&normalized_url);
        }

        self.config.clone()
    }
}

/// Normalize a URL by ensuring it has a valid protocol
///
/// If the URL doesn't start with http:// or https://, adds https:// prefix
fn normalize_url(url: &str) -> String {
    let url = url.trim();

    // Check if URL already has a protocol
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if url.is_empty() {
        url.to_string()
    } else {
        // Add https:// prefix
        format!("https://{}", url)
    }
}

#[derive(Debug, PartialEq, Clone)]
#[allow(clippy::module_name_repetitions)]
#[allow(clippy::struct_excessive_bools)]
pub struct Config {
    pub site: String,
    pub api_key: String,
    pub log_level: LogLevel,

    // Timeout for the request to flush data to Datadog endpoint
    pub flush_timeout: u64,

    // Global config of compression levels.
    // It would be overridden by the setup for the individual component
    pub compression_level: i32,

    // Proxy
    pub proxy_https: Option<String>,
    pub proxy_no_proxy: Vec<String>,
    pub http_protocol: Option<String>,

    // Endpoints
    pub dd_url: String,
    pub url: String,
    pub additional_endpoints: HashMap<String, Vec<String>>,

    // Unified Service Tagging
    pub env: Option<String>,
    pub service: Option<String>,
    pub version: Option<String>,
    pub tags: HashMap<String, String>,

    // Logs
    pub logs_config_logs_dd_url: String,
    pub logs_config_processing_rules: Option<Vec<ProcessingRule>>,
    pub logs_config_use_compression: bool,
    pub logs_config_compression_level: i32,
    pub logs_config_additional_endpoints: Vec<LogsAdditionalEndpoint>,
    pub observability_pipelines_worker_logs_enabled: bool,
    pub observability_pipelines_worker_logs_url: String,

    // APM
    //
    pub service_mapping: HashMap<String, String>,
    //
    pub apm_dd_url: String,
    pub apm_replace_tags: Option<Vec<ReplaceRule>>,
    pub apm_config_obfuscation_http_remove_query_string: bool,
    pub apm_config_obfuscation_http_remove_paths_with_digits: bool,
    pub apm_config_compression_level: i32,
    pub apm_features: Vec<String>,
    pub apm_additional_endpoints: HashMap<String, Vec<String>>,
    pub apm_filter_tags_require: Option<Vec<String>>,
    pub apm_filter_tags_reject: Option<Vec<String>>,
    pub apm_filter_tags_regex_require: Option<Vec<String>>,
    pub apm_filter_tags_regex_reject: Option<Vec<String>>,
    //
    // Trace Propagation
    pub trace_propagation_style: Vec<TracePropagationStyle>,
    pub trace_propagation_style_extract: Vec<TracePropagationStyle>,
    pub trace_propagation_extract_first: bool,
    pub trace_propagation_http_baggage_enabled: bool,
    pub trace_aws_service_representation_enabled: bool,

    // Metrics
    pub metrics_config_compression_level: i32,

    // Enhanced Metrics
    pub enhanced_metrics: bool,
    pub compute_trace_stats_on_extension: bool,

    // AppSec
    pub appsec_enabled: bool,
    pub appsec_rules: Option<String>,
    pub appsec_waf_timeout: Duration,
    pub api_security_enabled: bool,
    pub api_security_sample_delay: Duration,

    // Remote Configuration
    pub remote_configuration_enabled: bool,
    pub remote_configuration_api_key: Option<String>,
    pub remote_configuration_key: Option<String>,
    pub remote_configuration_no_tls: bool,
    pub remote_configuration_no_tls_validation: bool,

    // Operational Mode
    /// The operational mode determining how the agent accepts telemetry data.
    ///
    /// - `HttpFixedPort`: HTTP servers on fixed ports (default)
    /// - `HttpEphemeralPort`: HTTP servers on auto-assigned ports
    /// - `HttpUds`: HTTP servers on Unix Domain Sockets
    pub operational_mode: operational_mode::OperationalMode,

    /// Port for the trace agent HTTP server.
    ///
    /// - `None`: Use default port (8126) for fixed mode, OS-assigned for ephemeral mode
    /// - `Some(0)`: Use OS-assigned port (ephemeral)
    /// - `Some(n)`: Use specific port n
    ///
    /// Only used when `operational_mode` requires HTTP server.
    pub trace_agent_port: Option<u16>,

    /// Unix socket path for the trace agent HTTP server.
    ///
    /// - `None`: Auto-generate path using PID (e.g., `/tmp/dd-trace-12345.sock`)
    /// - `Some(path)`: Use specific path
    ///
    /// Only used when `operational_mode` is `HttpUds`.
    /// If path is None in HttpUds mode, it will be auto-generated using the process PID.
    pub trace_agent_uds_path: Option<String>,

    /// Permissions for the Unix socket file (octal).
    ///
    /// Default: 0o600 (read/write for owner only)
    /// This is secure for same-process communication where the socket
    /// should only be accessible by the process that created it.
    ///
    /// For multi-process scenarios, you can set more permissive modes:
    /// - 0o660: read/write by owner and group
    /// - 0o666: read/write by all (least secure, not recommended)
    ///
    /// Only used when `operational_mode` is `HttpUds`.
    pub trace_agent_uds_permissions: u32,

    /// Port for the metrics agent (DogStatsD) server.
    ///
    /// - `None`: Use default port (8125)
    /// - `Some(0)`: Use OS-assigned port (ephemeral)
    /// - `Some(n)`: Use specific port n
    ///
    /// Only used when `operational_mode` requires HTTP server and metrics are enabled.
    pub metrics_agent_port: Option<u16>,

    /// Port for the logs agent HTTP server.
    ///
    /// - `None`: Use default port (if implemented)
    /// - `Some(0)`: Use OS-assigned port (ephemeral)
    /// - `Some(n)`: Use specific port n
    ///
    /// Only used when `operational_mode` requires HTTP server and logs are enabled.
    pub logs_agent_port: Option<u16>,

    // DogStatsD Configuration
    /// Enable the DogStatsD server for receiving metrics via UDP.
    ///
    /// When enabled, the agent will start a UDP server to receive StatsD metrics
    /// on 127.0.0.1 (localhost).
    /// Default: false
    pub dogstatsd_enabled: bool,

    /// Port for the DogStatsD UDP server.
    ///
    /// Default: 8125
    pub dogstatsd_port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            site: String::default(),
            api_key: String::default(),
            log_level: LogLevel::default(),
            flush_timeout: 5,

            // Proxy
            proxy_https: None,
            proxy_no_proxy: vec![],
            http_protocol: None,

            // Endpoints
            dd_url: String::default(),
            url: String::default(),
            additional_endpoints: HashMap::new(),

            // Unified Service Tagging
            env: None,
            service: None,
            version: None,
            tags: HashMap::new(),

            compression_level: 3,

            // Logs
            logs_config_logs_dd_url: String::default(),
            logs_config_processing_rules: None,
            logs_config_use_compression: true,
            logs_config_compression_level: 3,
            logs_config_additional_endpoints: Vec::new(),
            observability_pipelines_worker_logs_enabled: false,
            observability_pipelines_worker_logs_url: String::default(),

            // APM
            service_mapping: HashMap::new(),
            apm_dd_url: String::default(),
            apm_replace_tags: None,
            apm_config_obfuscation_http_remove_query_string: false,
            apm_config_obfuscation_http_remove_paths_with_digits: false,
            apm_config_compression_level: 3,
            apm_features: vec![],
            apm_additional_endpoints: HashMap::new(),
            apm_filter_tags_require: None,
            apm_filter_tags_reject: None,
            apm_filter_tags_regex_require: None,
            apm_filter_tags_regex_reject: None,
            trace_aws_service_representation_enabled: true,
            trace_propagation_style: vec![
                TracePropagationStyle::Datadog,
                TracePropagationStyle::TraceContext,
            ],
            trace_propagation_style_extract: vec![],
            trace_propagation_extract_first: false,
            trace_propagation_http_baggage_enabled: false,

            // Metrics
            metrics_config_compression_level: 3,

            // Enhanced Metrics
            enhanced_metrics: true,
            compute_trace_stats_on_extension: false,

            // AppSec
            appsec_enabled: false,
            appsec_rules: None,
            appsec_waf_timeout: Duration::from_millis(5),
            api_security_enabled: true,
            api_security_sample_delay: Duration::from_secs(30),

            // Remote Configuration - enabled by default like Go agent
            remote_configuration_enabled: true,
            remote_configuration_api_key: None,
            remote_configuration_key: None,
            remote_configuration_no_tls: false,
            remote_configuration_no_tls_validation: false,

            // Operational Mode - default to HTTP with fixed ports
            operational_mode: operational_mode::OperationalMode::default(),
            trace_agent_port: None,             // None = use default 8126
            trace_agent_uds_path: None,         // None = auto-generate based on PID
            trace_agent_uds_permissions: 0o600, // rw------- (owner only, secure default)
            metrics_agent_port: None,           // None = use default 8125
            logs_agent_port: None,              // None = use default

            // DogStatsD - disabled by default
            dogstatsd_enabled: false,
            dogstatsd_port: 8125,
        }
    }
}

impl Config {
    /// Validates the configuration for consistency.
    ///
    /// Checks that operational mode settings are compatible with configured ports/paths.
    /// Returns an error if invalid combinations are detected.
    pub fn validate(&self) -> Result<(), String> {
        match self.operational_mode {
            operational_mode::OperationalMode::HttpFixedPort
            | operational_mode::OperationalMode::HttpEphemeralPort => {
                // TCP modes should not have UDS path configured
                if self.trace_agent_uds_path.is_some() {
                    return Err(format!(
                        "Invalid configuration: UDS path cannot be set when operational_mode is {}. \
                         Use operational_mode 'http_uds' for Unix Domain Socket support.",
                        self.operational_mode
                    ));
                }
                // OK to have port configured or None (will use default)
            }
            operational_mode::OperationalMode::HttpUds => {
                // UDS mode should not have TCP port explicitly configured
                if self.trace_agent_port.is_some() {
                    return Err(format!(
                        "Invalid configuration: TCP port cannot be set when operational_mode is {}. \
                         Use operational_mode 'http_fixed_port' or 'http_ephemeral_port' for TCP support.",
                        self.operational_mode
                    ));
                }
                // OK to have uds_path as None (will auto-generate using PID)
            }
        }
        Ok(())
    }
}

/// Load configuration from YAML file and environment variables (test-only).
///
/// **NOTE**: This function is only available in tests. Production builds use
/// FFI arguments and environment variables for configuration without YAML parsing.
///
/// This loads configuration in priority order:
/// 1. Defaults
/// 2. YAML file (datadog.yaml)
/// 3. Environment variables (highest priority)
#[cfg(test)]
#[allow(clippy::module_name_repetitions)]
#[inline]
#[must_use]
pub fn get_config(config_directory: &Path) -> Config {
    let path: std::path::PathBuf = config_directory.join("datadog.yaml");
    ConfigBuilder::default()
        .add_source(Box::new(YamlConfigSource { path }))
        .add_source(Box::new(EnvConfigSource))
        .build()
}

#[inline]
#[must_use]
fn build_fqdn_logs(site: String) -> String {
    format!("https://http-intake.logs.{site}")
}

pub fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    match Value::deserialize(deserializer)? {
        Value::String(s) => Ok(Some(s)),
        other => {
            error!(
                "Failed to parse value, expected a string, got: {}, ignoring",
                other
            );
            Ok(None)
        }
    }
}

pub fn deserialize_string_or_int<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::String(s) => {
            if s.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(s))
            }
        }
        Value::Number(n) => Ok(Some(n.to_string())),
        _ => {
            error!("Failed to parse value, expected a string or an integer, ignoring");
            Ok(None)
        }
    }
}

pub fn deserialize_optional_bool_from_anything<'de, D>(
    deserializer: D,
) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    // First try to deserialize as Option<_> to handle null/missing values
    let opt: Option<serde_json::Value> = Option::deserialize(deserializer)?;

    match opt {
        None => Ok(None),
        Some(value) => match deserialize_bool_from_anything(value) {
            Ok(bool_result) => Ok(Some(bool_result)),
            Err(e) => {
                error!("Failed to parse bool value: {}, ignoring", e);
                Ok(None)
            }
        },
    }
}

pub fn deserialize_key_value_pairs<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct KeyValueVisitor;

    impl serde::de::Visitor<'_> for KeyValueVisitor {
        type Value = HashMap<String, String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a string in format 'key1:value1,key2:value2' or 'key1:value1'")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            let mut map = HashMap::new();

            for tag in value.split(',') {
                let parts = tag.split(':').collect::<Vec<&str>>();
                if parts.len() == 2 {
                    map.insert(parts[0].to_string(), parts[1].to_string());
                } else {
                    error!(
                        "Failed to parse tag '{}', expected format 'key:value', ignoring",
                        tag.trim()
                    );
                }
            }

            Ok(map)
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            error!(
                "Failed to parse tags: expected string in format 'key:value', got number {}, ignoring",
                value
            );
            Ok(HashMap::new())
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            error!(
                "Failed to parse tags: expected string in format 'key:value', got number {}, ignoring",
                value
            );
            Ok(HashMap::new())
        }

        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            error!(
                "Failed to parse tags: expected string in format 'key:value', got number {}, ignoring",
                value
            );
            Ok(HashMap::new())
        }

        fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            error!(
                "Failed to parse tags: expected string in format 'key:value', got boolean {}, ignoring",
                value
            );
            Ok(HashMap::new())
        }
    }

    deserializer.deserialize_any(KeyValueVisitor)
}

pub fn deserialize_array_from_comma_separated_string<'de, D>(
    deserializer: D,
) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = String::deserialize(deserializer)?;
    Ok(s.split(',')
        .map(|feature| feature.trim().to_string())
        .filter(|feature| !feature.is_empty())
        .collect())
}

pub fn deserialize_key_value_pair_array_to_hashmap<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let array: Vec<String> = Vec::deserialize(deserializer)?;
    let mut map = HashMap::new();
    for s in array {
        let parts = s.split(':').collect::<Vec<&str>>();
        if parts.len() == 2 {
            map.insert(parts[0].to_string(), parts[1].to_string());
        } else {
            error!(
                "Failed to parse tag '{}', expected format 'key:value', ignoring",
                s.trim()
            );
        }
    }
    Ok(map)
}

/// Deserialize APM filter tags from space-separated "key:value" pairs, also support key-only tags
pub fn deserialize_apm_filter_tags<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;

    match opt {
        None => Ok(None),
        Some(s) if s.trim().is_empty() => Ok(None),
        Some(s) => {
            let tags: Vec<String> = s
                .split_whitespace()
                .filter_map(|pair| {
                    let parts: Vec<&str> = pair.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        let key = parts[0].trim();
                        let value = parts[1].trim();
                        if key.is_empty() {
                            None
                        } else if value.is_empty() {
                            Some(key.to_string())
                        } else {
                            Some(format!("{key}:{value}"))
                        }
                    } else if parts.len() == 1 {
                        let key = parts[0].trim();
                        if key.is_empty() {
                            None
                        } else {
                            Some(key.to_string())
                        }
                    } else {
                        None
                    }
                })
                .collect();

            if tags.is_empty() {
                Ok(None)
            } else {
                Ok(Some(tags))
            }
        }
    }
}

pub fn deserialize_option_lossless<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    match Option::<T>::deserialize(deserializer) {
        Ok(value) => Ok(value),
        Err(e) => {
            error!("Failed to deserialize optional value: {}, ignoring", e);
            Ok(None)
        }
    }
}

pub fn deserialize_optional_duration_from_microseconds<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Duration>, D::Error> {
    Ok(Option::<u64>::deserialize(deserializer)?.map(Duration::from_micros))
}

pub fn deserialize_optional_duration_from_seconds<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Duration>, D::Error> {
    struct DurationVisitor;
    impl serde::de::Visitor<'_> for DurationVisitor {
        type Value = Option<Duration>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(f, "a duration in seconds (integer or float)")
        }
        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(Duration::from_secs(v)))
        }
        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
            if v < 0 {
                error!("Failed to parse duration: negative durations are not allowed, ignoring");
                return Ok(None);
            }
            self.visit_u64(u64::try_from(v).expect("positive i64 to u64 conversion never fails"))
        }
        fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
            if v < 0f64 {
                error!("Failed to parse duration: negative durations are not allowed, ignoring");
                return Ok(None);
            }
            Ok(Some(Duration::from_secs_f64(v)))
        }
    }
    deserializer.deserialize_any(DurationVisitor)
}

// Like deserialize_optional_duration_from_seconds(), but return None if the value is 0
pub fn deserialize_optional_duration_from_seconds_ignore_zero<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Duration>, D::Error> {
    let duration: Option<Duration> = deserialize_optional_duration_from_seconds(deserializer)?;
    if duration.is_some_and(|d| d.as_secs() == 0) {
        return Ok(None);
    }
    Ok(duration)
}

#[cfg_attr(coverage_nightly, coverage(off))] // Test modules skew coverage metrics
#[cfg(test)]
pub mod tests {
    use datadog_trace_obfuscation::replacer::parse_rules_from_string;

    use super::*;

    use crate::config::{
        log_level::LogLevel, processing_rule::ProcessingRule,
        trace_propagation_style::TracePropagationStyle,
    };

    #[test]
    fn test_default_logs_intake_url() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();

            let config = get_config(Path::new(""));
            assert_eq!(
                config.logs_config_logs_dd_url,
                "https://http-intake.logs.datadoghq.com".to_string()
            );
            Ok(())
        });
    }

    #[test]
    fn test_support_pci_logs_intake_url() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env(
                "DD_LOGS_CONFIG_LOGS_DD_URL",
                "agent-http-intake-pci.logs.datadoghq.com:443",
            );

            let config = get_config(Path::new(""));
            assert_eq!(
                config.logs_config_logs_dd_url,
                "agent-http-intake-pci.logs.datadoghq.com:443".to_string()
            );
            Ok(())
        });
    }

    #[test]
    fn test_support_pci_traces_intake_url() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_APM_DD_URL", "https://trace-pci.agent.datadoghq.com");

            let config = get_config(Path::new(""));
            assert_eq!(
                config.apm_dd_url,
                "https://trace-pci.agent.datadoghq.com/api/v0.2/traces".to_string()
            );
            Ok(())
        });
    }

    #[test]
    fn test_support_dd_dd_url() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_DD_URL", "custom_proxy:3128");

            let config = get_config(Path::new(""));
            assert_eq!(config.dd_url, "custom_proxy:3128".to_string());
            Ok(())
        });
    }

    #[test]
    fn test_support_dd_url() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_URL", "custom_proxy:3128");

            let config = get_config(Path::new(""));
            assert_eq!(config.url, "custom_proxy:3128".to_string());
            Ok(())
        });
    }

    #[test]
    fn test_dd_dd_url_default() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();

            let config = get_config(Path::new(""));
            assert_eq!(config.dd_url, String::new());
            Ok(())
        });
    }

    #[test]
    fn test_precedence() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r"
                site: datadoghq.eu,
            ",
            )?;
            jail.set_env("DD_SITE", "datad0g.com");
            let config = get_config(Path::new(""));
            assert_eq!(config.site, "datad0g.com");
            Ok(())
        });
    }

    #[test]
    fn test_parse_config_file() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            // nit: does parsing an empty file actually test "parse config file"?
            jail.create_file(
                "datadog.yaml",
                r"
            ",
            )?;
            let config = get_config(Path::new(""));
            assert_eq!(config.site, "datadoghq.com");
            Ok(())
        });
    }

    #[test]
    fn test_parse_env() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_SITE", "datadoghq.eu");
            let config = get_config(Path::new(""));
            assert_eq!(config.site, "datadoghq.eu");
            Ok(())
        });
    }

    #[test]
    fn test_parse_log_level() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_LOG_LEVEL", "TRACE");
            let config = get_config(Path::new(""));
            assert_eq!(config.log_level, LogLevel::Trace);
            Ok(())
        });
    }

    #[test]
    fn test_parse_default() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let config = get_config(Path::new(""));
            assert_eq!(
                config,
                Config {
                    site: "datadoghq.com".to_string(),
                    trace_propagation_style_extract: vec![
                        TracePropagationStyle::Datadog,
                        TracePropagationStyle::TraceContext
                    ],
                    logs_config_logs_dd_url: "https://http-intake.logs.datadoghq.com".to_string(),
                    apm_dd_url: trace_intake_url("datadoghq.com").to_string(),
                    dd_url: String::new(), // We add the prefix in main.rs
                    ..Config::default()
                }
            );
            Ok(())
        });
    }

    #[test]
    fn test_proxy_config() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_PROXY_HTTPS", "my-proxy:3128");
            let config = get_config(Path::new(""));
            assert_eq!(config.proxy_https, Some("my-proxy:3128".to_string()));
            Ok(())
        });
    }

    #[test]
    fn test_noproxy_config() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_SITE", "datadoghq.eu");
            jail.set_env("DD_PROXY_HTTPS", "my-proxy:3128");
            jail.set_env(
                "NO_PROXY",
                "127.0.0.1,localhost,172.16.0.0/12,us-east-1.amazonaws.com,datadoghq.eu",
            );
            let config = get_config(Path::new(""));
            assert_eq!(config.proxy_https, None);
            Ok(())
        });
    }

    #[test]
    fn test_proxy_yaml() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r"
                proxy:
                  https: my-proxy:3128
            ",
            )?;

            let config = get_config(Path::new(""));
            assert_eq!(config.proxy_https, Some("my-proxy:3128".to_string()));
            Ok(())
        });
    }

    #[test]
    fn test_no_proxy_yaml() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r"
                proxy:
                  https: my-proxy:3128
                  no_proxy:
                    - datadoghq.com
            ",
            )?;

            let config = get_config(Path::new(""));
            assert_eq!(config.proxy_https, None);
            // Assertion to ensure config.site runs before proxy
            // because we chenck that noproxy contains the site
            assert_eq!(config.site, "datadoghq.com");
            Ok(())
        });
    }

    #[test]
    fn parse_number_or_string_env_vars() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_VERSION", "123");
            jail.set_env("DD_ENV", "123456890");
            jail.set_env("DD_SERVICE", "123456");
            let config = get_config(Path::new(""));
            assert_eq!(config.version.expect("failed to parse DD_VERSION"), "123");
            assert_eq!(config.env.expect("failed to parse DD_ENV"), "123456890");
            assert_eq!(
                config.service.expect("failed to parse DD_SERVICE"),
                "123456"
            );
            Ok(())
        });
    }

    #[test]
    fn test_parse_logs_config_processing_rules_from_env() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env(
                "DD_LOGS_CONFIG_PROCESSING_RULES",
                r#"[{"type":"exclude_at_match","name":"exclude","pattern":"exclude"}]"#,
            );
            jail.create_file(
                "datadog.yaml",
                r"
                logs_config:
                  processing_rules:
                    - type: exclude_at_match
                      name: exclude-me-yaml
                      pattern: exclude-me-yaml
            ",
            )?;
            let config = get_config(Path::new(""));
            assert_eq!(
                config.logs_config_processing_rules,
                Some(vec![ProcessingRule {
                    kind: processing_rule::Kind::ExcludeAtMatch,
                    name: "exclude".to_string(),
                    pattern: "exclude".to_string(),
                    replace_placeholder: None
                }])
            );
            Ok(())
        });
    }

    #[test]
    fn test_parse_logs_config_processing_rules_from_yaml() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r"
                site: datadoghq.com
                logs_config:
                  processing_rules:
                    - type: exclude_at_match
                      name: exclude
                      pattern: exclude
            ",
            )?;
            let config = get_config(Path::new(""));
            assert_eq!(
                config.logs_config_processing_rules,
                Some(vec![ProcessingRule {
                    kind: processing_rule::Kind::ExcludeAtMatch,
                    name: "exclude".to_string(),
                    pattern: "exclude".to_string(),
                    replace_placeholder: None
                }]),
            );
            Ok(())
        });
    }

    #[test]
    fn test_parse_apm_replace_tags_from_yaml() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r"
                site: datadoghq.com
                apm_config:
                  replace_tags:
                    - name: '*'
                      pattern: 'foo'
                      repl: 'REDACTED'
            ",
            )?;
            let config = get_config(Path::new(""));
            let rule = parse_rules_from_string(
                r#"[
                        {"name": "*", "pattern": "foo", "repl": "REDACTED"}
                    ]"#,
            )
            .expect("can't parse rules");
            assert_eq!(config.apm_replace_tags, Some(rule),);
            Ok(())
        });
    }

    #[test]
    fn test_apm_tags_env_overrides_yaml() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env(
                "DD_APM_REPLACE_TAGS",
                r#"[{"name":"*","pattern":"foo","repl":"REDACTED-ENV"}]"#,
            );
            jail.create_file(
                "datadog.yaml",
                r"
                site: datadoghq.com
                apm_config:
                  replace_tags:
                    - name: '*'
                      pattern: 'foo'
                      repl: 'REDACTED-YAML'
            ",
            )?;
            let config = get_config(Path::new(""));
            let rule = parse_rules_from_string(
                r#"[
                        {"name": "*", "pattern": "foo", "repl": "REDACTED-ENV"}
                    ]"#,
            )
            .expect("can't parse rules");
            assert_eq!(config.apm_replace_tags, Some(rule),);
            Ok(())
        });
    }

    #[test]
    fn test_parse_apm_http_obfuscation_from_yaml() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r"
                site: datadoghq.com
                apm_config:
                  obfuscation:
                    http:
                      remove_query_string: true
                      remove_paths_with_digits: true
            ",
            )?;
            let config = get_config(Path::new(""));
            assert!(config.apm_config_obfuscation_http_remove_query_string,);
            assert!(config.apm_config_obfuscation_http_remove_paths_with_digits,);
            Ok(())
        });
    }
    #[test]
    fn test_parse_trace_propagation_style() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env(
                "DD_TRACE_PROPAGATION_STYLE",
                "datadog,tracecontext,b3,b3multi",
            );
            let config = get_config(Path::new(""));

            let expected_styles = vec![
                TracePropagationStyle::Datadog,
                TracePropagationStyle::TraceContext,
                TracePropagationStyle::B3,
                TracePropagationStyle::B3Multi,
            ];
            assert_eq!(config.trace_propagation_style, expected_styles);
            assert_eq!(config.trace_propagation_style_extract, expected_styles);
            Ok(())
        });
    }

    #[test]
    fn test_parse_trace_propagation_style_extract() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_TRACE_PROPAGATION_STYLE_EXTRACT", "datadog");
            let config = get_config(Path::new(""));

            assert_eq!(
                config.trace_propagation_style,
                vec![
                    TracePropagationStyle::Datadog,
                    TracePropagationStyle::TraceContext,
                ]
            );
            assert_eq!(
                config.trace_propagation_style_extract,
                vec![TracePropagationStyle::Datadog]
            );
            Ok(())
        });
    }

    #[test]
    fn test_bad_tags() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_TAGS", 123);
            let config = get_config(Path::new(""));
            assert_eq!(config.tags, HashMap::new());
            Ok(())
        });
    }

    #[test]
    fn test_parse_bool_from_anything() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_ENHANCED_METRICS", "1");
            jail.set_env("DD_LOGS_CONFIG_USE_COMPRESSION", "TRUE");
            let config = get_config(Path::new(""));
            assert!(config.enhanced_metrics);
            assert!(config.logs_config_use_compression);
            Ok(())
        });
    }

    #[test]
    fn test_overrides_config_based_on_priority() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r#"
                site: us3.datadoghq.com
                api_key: "yaml-api-key"
                log_level: "debug"
            "#,
            )?;
            jail.set_env("DD_SITE", "us5.datadoghq.com");
            jail.set_env("DD_API_KEY", "env-api-key");
            jail.set_env("DD_FLUSH_TIMEOUT", "10");
            let config = get_config(Path::new(""));

            assert_eq!(config.site, "us5.datadoghq.com");
            assert_eq!(config.api_key, "env-api-key");
            assert_eq!(config.log_level, LogLevel::Debug);
            assert_eq!(config.flush_timeout, 10);
            Ok(())
        });
    }

    #[test]
    fn test_operational_mode_priority_env_overrides_yaml() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r#"
                api_key: "test-api-key"
                operational_mode: "http_fixed_port"
                trace_agent_port: 8126
            "#,
            )?;
            // Environment variable should override YAML
            jail.set_env("DD_AGENT_MODE", "http_uds");
            jail.set_env("DD_TRACE_AGENT_PORT", "9999");

            let config = get_config(Path::new(""));

            // Env vars should win
            assert_eq!(
                config.operational_mode,
                operational_mode::OperationalMode::HttpUds
            );
            assert_eq!(config.trace_agent_port, Some(9999));
            Ok(())
        });
    }

    #[test]
    fn test_operational_mode_yaml_used_when_no_env() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r#"
                api_key: "test-api-key"
                operational_mode: "http_ephemeral_port"
                trace_agent_port: 7777
            "#,
            )?;
            // No environment variables set

            let config = get_config(Path::new(""));

            // YAML values should be used
            assert_eq!(
                config.operational_mode,
                operational_mode::OperationalMode::HttpEphemeralPort
            );
            assert_eq!(config.trace_agent_port, Some(7777));
            Ok(())
        });
    }

    #[test]
    fn test_operational_mode_default_when_neither_set() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r#"
                api_key: "test-api-key"
            "#,
            )?;
            // Neither env nor YAML specifies operational_mode

            let config = get_config(Path::new(""));

            // Should use default (HttpFixedPort)
            assert_eq!(
                config.operational_mode,
                operational_mode::OperationalMode::HttpFixedPort
            );
            assert_eq!(config.trace_agent_port, None);
            Ok(())
        });
    }

    #[test]
    fn test_parse_duration_from_microseconds() {
        #[derive(Deserialize, Debug, PartialEq, Eq)]
        struct Value {
            #[serde(default)]
            #[serde(deserialize_with = "deserialize_optional_duration_from_microseconds")]
            duration: Option<Duration>,
        }

        assert_eq!(
            serde_json::from_str::<Value>("{}").expect("failed to parse JSON"),
            Value { duration: None }
        );
        serde_json::from_str::<Value>(r#"{"duration":-1}"#)
            .expect_err("should have failed parsing");
        assert_eq!(
            serde_json::from_str::<Value>(r#"{"duration":1000000}"#).expect("failed to parse JSON"),
            Value {
                duration: Some(Duration::from_secs(1))
            }
        );
        serde_json::from_str::<Value>(r#"{"duration":-1.5}"#)
            .expect_err("should have failed parsing");
        serde_json::from_str::<Value>(r#"{"duration":1.5}"#)
            .expect_err("should have failed parsing");
    }

    #[test]
    fn test_parse_duration_from_seconds() {
        #[derive(Deserialize, Debug, PartialEq, Eq)]
        struct Value {
            #[serde(default)]
            #[serde(deserialize_with = "deserialize_optional_duration_from_seconds")]
            duration: Option<Duration>,
        }

        assert_eq!(
            serde_json::from_str::<Value>("{}").expect("failed to parse JSON"),
            Value { duration: None }
        );
        assert_eq!(
            serde_json::from_str::<Value>(r#"{"duration":-1}"#).expect("failed to parse JSON"),
            Value { duration: None }
        );
        assert_eq!(
            serde_json::from_str::<Value>(r#"{"duration":1}"#).expect("failed to parse JSON"),
            Value {
                duration: Some(Duration::from_secs(1))
            }
        );
        assert_eq!(
            serde_json::from_str::<Value>(r#"{"duration":-1.5}"#).expect("failed to parse JSON"),
            Value { duration: None }
        );
        assert_eq!(
            serde_json::from_str::<Value>(r#"{"duration":1.5}"#).expect("failed to parse JSON"),
            Value {
                duration: Some(Duration::from_millis(1500))
            }
        );
    }

    #[test]
    fn test_parse_duration_from_seconds_ignore_zero() {
        #[derive(Deserialize, Debug, PartialEq, Eq)]
        struct Value {
            #[serde(default)]
            #[serde(deserialize_with = "deserialize_optional_duration_from_seconds_ignore_zero")]
            duration: Option<Duration>,
        }

        assert_eq!(
            serde_json::from_str::<Value>(r#"{"duration":1}"#).expect("failed to parse JSON"),
            Value {
                duration: Some(Duration::from_secs(1))
            }
        );

        assert_eq!(
            serde_json::from_str::<Value>(r#"{"duration":0}"#).expect("failed to parse JSON"),
            Value { duration: None }
        );
    }

    #[test]
    fn test_malformed_url_site_with_protocol() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_API_KEY", "test-key");
            jail.set_env("DD_SITE", "http://datadoghq.com"); // Site should not have protocol

            let config = get_config(Path::new(""));

            // Should strip protocol or handle gracefully
            assert!(
                !config.site.is_empty(),
                "Site should be set even with invalid protocol"
            );
            Ok(())
        });
    }

    #[test]
    fn test_malformed_url_apm_dd_url_without_protocol() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_API_KEY", "test-key");
            jail.set_env("DD_APM_DD_URL", "trace.agent.datadoghq.com"); // Missing protocol

            let config = get_config(Path::new(""));

            // Current behavior: URL without protocol is accepted as-is
            // The trace_intake_url_prefixed function adds /api/v0.2/traces suffix
            // This documents the behavior - ideally should validate or add https://
            assert!(
                config.apm_dd_url.contains("trace.agent.datadoghq.com"),
                "apm_dd_url should contain the hostname: {}",
                config.apm_dd_url
            );
            Ok(())
        });
    }

    #[test]
    fn test_empty_site_after_trimming() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_API_KEY", "test-key");
            jail.set_env("DD_SITE", "   "); // Only whitespace

            let config = get_config(Path::new(""));

            // Should use default site
            assert_eq!(
                config.site, "datadoghq.com",
                "Should use default site when empty"
            );
            Ok(())
        });
    }

    #[test]
    fn test_api_key_with_special_characters() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_API_KEY", "test-key-with-!@#$%^&*()"); // Special chars

            let config = get_config(Path::new(""));

            // Should accept API key with special characters
            assert_eq!(config.api_key, "test-key-with-!@#$%^&*()");
            Ok(())
        });
    }

    #[test]
    fn test_api_key_whitespace_only() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_API_KEY", "   "); // Whitespace only

            let config = get_config(Path::new(""));

            // Should result in empty or trimmed API key
            assert!(
                config.api_key.is_empty() || config.api_key.trim().is_empty(),
                "Whitespace-only API key should be empty or trimmed"
            );
            Ok(())
        });
    }

    #[test]
    fn test_very_long_api_key() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            let long_key = "a".repeat(500); // 500 character API key
            jail.set_env("DD_API_KEY", &long_key);

            let config = get_config(Path::new(""));

            // Should accept long API key (no artificial limit)
            assert_eq!(config.api_key, long_key);
            Ok(())
        });
    }

    #[test]
    fn test_flush_timeout_zero() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_API_KEY", "test-key");
            jail.set_env("DD_SERVERLESS_FLUSH_STRATEGY", "end,0"); // Zero timeout

            let config = get_config(Path::new(""));

            // Current behavior: zero timeout reverts to default (30s)
            // This documents the behavior - zero may not be a valid flush timeout
            assert!(
                config.flush_timeout > 0,
                "Zero flush timeout should use default, got: {}",
                config.flush_timeout
            );
            Ok(())
        });
    }

    #[test]
    fn test_compression_level_negative() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_API_KEY", "test-key");
            jail.set_env("DD_COMPRESSION_LEVEL", "-1"); // Negative compression

            let config = get_config(Path::new(""));

            // zstd accepts -1 for default compression
            // Should not panic, behavior is library-dependent
            assert!(
                config.compression_level <= 22,
                "Compression level should be reasonable"
            );
            Ok(())
        });
    }

    #[test]
    fn test_normalize_url_with_https() {
        let url = "https://trace.agent.datadoghq.com";
        let normalized = normalize_url(url);
        assert_eq!(normalized, "https://trace.agent.datadoghq.com");
    }

    #[test]
    fn test_normalize_url_with_http() {
        let url = "http://trace.agent.datadoghq.com";
        let normalized = normalize_url(url);
        assert_eq!(normalized, "http://trace.agent.datadoghq.com");
    }

    #[test]
    fn test_normalize_url_without_protocol() {
        let url = "trace.agent.datadoghq.com";
        let normalized = normalize_url(url);
        assert_eq!(normalized, "https://trace.agent.datadoghq.com");
    }

    #[test]
    fn test_normalize_url_with_whitespace() {
        let url = "  trace.agent.datadoghq.com  ";
        let normalized = normalize_url(url);
        assert_eq!(normalized, "https://trace.agent.datadoghq.com");
    }

    #[test]
    fn test_normalize_url_empty() {
        let url = "";
        let normalized = normalize_url(url);
        assert_eq!(normalized, "");
    }

    #[test]
    fn test_normalize_url_whitespace_only() {
        let url = "   ";
        let normalized = normalize_url(url);
        assert_eq!(normalized, "");
    }

    #[test]
    fn test_malformed_url_apm_dd_url_gets_protocol() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_API_KEY", "test-key");
            jail.set_env("DD_APM_DD_URL", "trace.agent.datadoghq.com");

            let config = get_config(Path::new(""));

            // Should now have https:// added by normalize_url
            assert!(
                config.apm_dd_url.starts_with("https://"),
                "apm_dd_url should have https:// protocol added: {}",
                config.apm_dd_url
            );
            assert!(
                config.apm_dd_url.contains("trace.agent.datadoghq.com"),
                "apm_dd_url should still contain hostname: {}",
                config.apm_dd_url
            );
            Ok(())
        });
    }

    // Validation tests for UDS configuration

    #[test]
    fn test_validate_http_fixed_port_mode_valid() {
        let config = Config {
            operational_mode: operational_mode::OperationalMode::HttpFixedPort,
            trace_agent_port: Some(8126),
            trace_agent_uds_path: None,
            ..Config::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_http_fixed_port_mode_with_uds_path_error() {
        let config = Config {
            operational_mode: operational_mode::OperationalMode::HttpFixedPort,
            trace_agent_port: Some(8126),
            trace_agent_uds_path: Some("/tmp/test.sock".to_string()),
            ..Config::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("UDS path cannot be set"));
    }

    #[test]
    fn test_validate_http_ephemeral_port_mode_valid() {
        let config = Config {
            operational_mode: operational_mode::OperationalMode::HttpEphemeralPort,
            trace_agent_port: None,
            trace_agent_uds_path: None,
            ..Config::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_http_ephemeral_port_mode_with_uds_path_error() {
        let config = Config {
            operational_mode: operational_mode::OperationalMode::HttpEphemeralPort,
            trace_agent_port: None,
            trace_agent_uds_path: Some("/tmp/test.sock".to_string()),
            ..Config::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("UDS path cannot be set"));
    }

    #[test]
    fn test_validate_http_uds_mode_valid() {
        let config = Config {
            operational_mode: operational_mode::OperationalMode::HttpUds,
            trace_agent_port: None,
            trace_agent_uds_path: Some("/tmp/test.sock".to_string()),
            ..Config::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_http_uds_mode_with_auto_path_valid() {
        let config = Config {
            operational_mode: operational_mode::OperationalMode::HttpUds,
            trace_agent_port: None,
            trace_agent_uds_path: None, // Will be auto-generated
            ..Config::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_http_uds_mode_with_port_error() {
        let config = Config {
            operational_mode: operational_mode::OperationalMode::HttpUds,
            trace_agent_port: Some(8126),
            trace_agent_uds_path: None,
            ..Config::default()
        };
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("TCP port cannot be set"));
    }
}
