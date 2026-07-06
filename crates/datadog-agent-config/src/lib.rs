pub mod deserializers;
pub mod sources;

// Re-export submodules at the crate root so existing imports like
// `crate::flush_strategy::FlushStrategy` and `crate::env::EnvConfigSource` keep working.
pub use deserializers::{
    additional_endpoints, apm_replace_rule, flush_strategy, log_level, logs_additional_endpoints,
    processing_rule, service_mapping,
};
pub use sources::{env, yaml};

pub use datadog_opentelemetry::configuration::TracePropagationStyle;
// Re-export all helper deserializers so consumers and internal modules can
// use `crate::deserialize_optional_string` etc. without reaching into submodules.
pub use deserializers::helpers::*;

use libdd_trace_obfuscation::replacer::ReplaceRule;
use libdd_trace_utils::config_utils::{trace_intake_url, trace_intake_url_prefixed};

use serde::Deserialize;

use std::collections::HashMap;
use std::path::Path;
use tracing::{debug, error};

use crate::{
    apm_replace_rule::deserialize_apm_replace_rules,
    env::EnvConfigSource,
    log_level::LogLevel,
    logs_additional_endpoints::LogsAdditionalEndpoint,
    processing_rule::{ProcessingRule, deserialize_processing_rules},
    yaml::YamlConfigSource,
};

// ---------------------------------------------------------------------------
// Config — the resolved configuration struct
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Clone)]
#[allow(clippy::module_name_repetitions)]
#[allow(clippy::struct_excessive_bools)]
pub struct Config<E: ConfigExtension = NoExtension> {
    pub site: String,
    pub api_key: String,
    /// Datadog organization UUID. When set, enables delegated authentication
    /// against the Datadog SaaS auth tier without requiring an api_key in some
    /// flows. Sourced from `DD_ORG_UUID` / `org_uuid` in datadog.yaml.
    pub dd_org_uuid: String,
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
    pub tls_cert_file: Option<String>,
    pub skip_ssl_validation: bool,

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
    /// Master toggle for log collection. Sourced from `DD_LOGS_ENABLED` /
    /// `logs_enabled` in datadog.yaml. Defaults to `false` to match the
    /// dd-agent default; consumers that want a different default should
    /// override post-build or in their `ConfigExtension`.
    pub logs_enabled: bool,
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
    pub statsd_metric_namespace: Option<String>,
    /// Size of the receive buffer for `DogStatsD` UDP packets, in bytes (`SO_RCVBUF`).
    /// Increase to reduce packet loss under high-throughput metric bursts.
    /// If None, uses the OS default.
    pub dogstatsd_so_rcvbuf: Option<usize>,
    /// Maximum size of a single read from any transport (UDP or named pipe), in bytes.
    /// Defaults to 8192. For UDP, the client must batch metrics into packets of
    /// this size for the increase to take effect.
    pub dogstatsd_buffer_size: Option<usize>,
    /// Internal queue capacity between the socket reader and metric processor.
    /// Defaults to 1024. Increase if the processor can't keep up with burst traffic.
    pub dogstatsd_queue_size: Option<usize>,

    // OTLP
    //
    // - APM / Traces
    pub otlp_config_traces_enabled: bool,
    pub otlp_config_traces_span_name_as_resource_name: bool,
    pub otlp_config_traces_span_name_remappings: HashMap<String, String>,
    pub otlp_config_ignore_missing_datadog_fields: bool,
    //
    // - Receiver / HTTP
    pub otlp_config_receiver_protocols_http_endpoint: Option<String>,
    // - Unsupported Configuration
    //
    // - Receiver / GRPC
    pub otlp_config_receiver_protocols_grpc_endpoint: Option<String>,
    pub otlp_config_receiver_protocols_grpc_transport: Option<String>,
    pub otlp_config_receiver_protocols_grpc_max_recv_msg_size_mib: Option<i32>,
    // - Metrics
    pub otlp_config_metrics_enabled: bool,
    pub otlp_config_metrics_resource_attributes_as_tags: bool,
    pub otlp_config_metrics_instrumentation_scope_metadata_as_tags: bool,
    pub otlp_config_metrics_tag_cardinality: Option<String>,
    pub otlp_config_metrics_delta_ttl: Option<i32>,
    pub otlp_config_metrics_histograms_mode: Option<String>,
    pub otlp_config_metrics_histograms_send_count_sum_metrics: bool,
    pub otlp_config_metrics_histograms_send_aggregation_metrics: bool,
    pub otlp_config_metrics_sums_cumulative_monotonic_mode: Option<String>,
    // nit: is the e in cumulative missing intentionally?
    pub otlp_config_metrics_sums_initial_cumulativ_monotonic_value: Option<String>,
    pub otlp_config_metrics_summaries_mode: Option<String>,
    // - Traces
    pub otlp_config_traces_probabilistic_sampler_sampling_percentage: Option<i32>,
    // - Logs
    pub otlp_config_logs_enabled: bool,

    /// Agent-specific extension fields defined by the consumer.
    /// Use `NoExtension` (the default) when no extra fields are needed.
    pub ext: E,
}

impl<E: ConfigExtension> Default for Config<E> {
    fn default() -> Self {
        Self {
            site: String::default(),
            api_key: String::default(),
            dd_org_uuid: String::default(),
            log_level: LogLevel::default(),
            flush_timeout: 30,

            // Proxy
            proxy_https: None,
            proxy_no_proxy: vec![],
            http_protocol: None,
            tls_cert_file: None,
            skip_ssl_validation: false,

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
            logs_enabled: false,
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
            statsd_metric_namespace: None,

            // DogStatsD
            // Defaults to None, which uses the OS default.
            dogstatsd_so_rcvbuf: None,
            // Defaults to 8192 internally.
            dogstatsd_buffer_size: None,
            // Defaults to 1024 internally.
            dogstatsd_queue_size: None,

            // OTLP
            otlp_config_traces_enabled: true,
            otlp_config_traces_span_name_as_resource_name: false,
            otlp_config_traces_span_name_remappings: HashMap::new(),
            otlp_config_ignore_missing_datadog_fields: false,
            otlp_config_receiver_protocols_http_endpoint: None,
            otlp_config_receiver_protocols_grpc_endpoint: None,
            otlp_config_receiver_protocols_grpc_transport: None,
            otlp_config_receiver_protocols_grpc_max_recv_msg_size_mib: None,
            otlp_config_metrics_enabled: false, // TODO(duncanista): Go Agent default is to true
            otlp_config_metrics_resource_attributes_as_tags: false,
            otlp_config_metrics_instrumentation_scope_metadata_as_tags: false,
            otlp_config_metrics_tag_cardinality: None,
            otlp_config_metrics_delta_ttl: None,
            otlp_config_metrics_histograms_mode: None,
            otlp_config_metrics_histograms_send_count_sum_metrics: false,
            otlp_config_metrics_histograms_send_aggregation_metrics: false,
            otlp_config_metrics_sums_cumulative_monotonic_mode: None,
            otlp_config_metrics_sums_initial_cumulativ_monotonic_value: None,
            otlp_config_metrics_summaries_mode: None,
            otlp_config_traces_probabilistic_sampler_sampling_percentage: None,
            otlp_config_logs_enabled: false,

            ext: E::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Loading — entry points for building a Config
// ---------------------------------------------------------------------------

#[allow(clippy::module_name_repetitions)]
#[inline]
#[must_use]
pub fn get_config(config_directory: &Path) -> Config {
    get_config_with_extension(config_directory)
}

/// Load configuration with a custom extension type.
///
/// Consumers that need additional fields should call this with their
/// extension type instead of `get_config`.
#[allow(clippy::module_name_repetitions)]
#[inline]
#[must_use]
pub fn get_config_with_extension<E: ConfigExtension>(config_directory: &Path) -> Config<E> {
    let path: std::path::PathBuf = config_directory.join("datadog.yaml");
    ConfigBuilder::default()
        .add_source(Box::new(YamlConfigSource { path }))
        .add_source(Box::new(EnvConfigSource))
        .build()
}

// ---------------------------------------------------------------------------
// ConfigExtension — trait for additional configuration fields
// ---------------------------------------------------------------------------

/// Trait that extension configs must implement to add additional configuration
/// fields beyond what the core provides.
///
/// Extensions allow consumers to define their own external configuration fields
/// that are deserialized from environment variables and YAML files alongside
/// core fields via dual extraction.
///
/// # Source type requirements
///
/// The `Source` type **must** use `#[serde(default)]` on the struct and graceful
/// deserializers (e.g., `deserialize_optional_bool_from_anything`) on each field.
/// Without these, a missing or malformed value will cause the entire extension
/// extraction to fail — the extension silently falls back to `E::default()` with
/// a `tracing::warn!` log. See [`ConfigExtension::Source`] for details.
///
/// # Flat fields only
///
/// A single `Source` type is used for both environment variable and YAML
/// extraction. This works when all extension fields are top-level (flat) in
/// the YAML file, which is the common case for extension configs:
///
/// ```yaml
/// # Works: flat fields map naturally to both DD_* env vars and YAML keys
/// enhanced_metrics: true
/// capture_lambda_payload: false
/// ```
///
/// If you need nested YAML structures (e.g., `lambda: { enhanced_metrics: true }`)
/// that differ from the flat env var layout, implement `merge_from` with a
/// nested source struct and handle the mapping manually instead of using
/// `merge_fields!`.
///
/// # Field name collisions with core config
///
/// Extension fields are extracted independently from the same figment as core
/// fields. If an extension defines a field with the same name as a core field
/// (e.g., `api_key`), both will deserialize their own copy — they do not
/// interfere with each other, but the extension copy will **not** override the
/// core value. Avoid shadowing core field names to prevent confusion.
pub trait ConfigExtension: Clone + Default + std::fmt::Debug + PartialEq {
    /// Intermediate deserialization type for extension fields, used for both
    /// environment variable and YAML extraction.
    ///
    /// # Requirements
    ///
    /// The struct **must** have:
    ///
    /// 1. `#[serde(default)]` on the struct — so missing fields get defaults
    ///    instead of failing the whole extraction.
    /// 2. Graceful per-field deserializers (e.g.,
    ///    `#[serde(deserialize_with = "deserialize_optional_bool_from_anything")]`)
    ///    — so one malformed value doesn't fail the whole extraction.
    ///
    /// **If either is missing**, `figment::extract::<Source>()` will fail at
    /// runtime when a field is absent or malformed. The extension falls back to
    /// `E::default()` and a `tracing::warn!` is emitted — no panic, but all
    /// extension fields silently get their default values.
    type Source: Default + serde::de::DeserializeOwned + Clone + std::fmt::Debug;

    /// Merge parsed source fields into self.
    fn merge_from(&mut self, source: &Self::Source);
}

/// A no-op extension for consumers that don't need extra fields.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct NoExtension;

/// A no-op source for deserialization that accepts (and ignores) any input.
/// Uses a regular struct (not unit struct) so serde deserializes it from
/// map-shaped data that figment provides, rather than expecting null/unit.
#[derive(Clone, Default, Debug, Deserialize)]
pub struct NoExtensionSource {}

impl ConfigExtension for NoExtension {
    type Source = NoExtensionSource;
    fn merge_from(&mut self, _source: &Self::Source) {}
}

// ---------------------------------------------------------------------------
// ConfigBuilder — orchestrates loading from multiple sources
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
#[allow(clippy::module_name_repetitions)]
pub enum ConfigError {
    ParseError(String),
    UnsupportedField(String),
}

#[allow(clippy::module_name_repetitions)]
pub trait ConfigSource<E: ConfigExtension> {
    fn load(&self, config: &mut Config<E>) -> Result<(), ConfigError>;
}

#[allow(clippy::module_name_repetitions)]
pub struct ConfigBuilder<E: ConfigExtension = NoExtension> {
    sources: Vec<Box<dyn ConfigSource<E>>>,
    config: Config<E>,
}

impl<E: ConfigExtension> Default for ConfigBuilder<E> {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
            config: Config::default(),
        }
    }
}

#[allow(clippy::module_name_repetitions)]
impl<E: ConfigExtension> ConfigBuilder<E> {
    #[must_use]
    pub fn add_source(mut self, source: Box<dyn ConfigSource<E>>) -> Self {
        self.sources.push(source);
        self
    }

    pub fn build(&mut self) -> Config<E> {
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
        if let Ok(https_proxy) = std::env::var("HTTPS_PROXY")
            && self.config.proxy_https.is_none()
        {
            self.config.proxy_https = Some(https_proxy);
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

        // Trim trailing slashes so suffix-appending downstream (e.g. the APM intake path
        // here, or the logs `/api/v2/logs` path appended by consumers) doesn't produce a
        // double slash.
        self.config.dd_url = trim_url(&self.config.dd_url);
        self.config.url = trim_url(&self.config.url);

        // If Logs URL is not set, set it to the default
        if self.config.logs_config_logs_dd_url.trim().is_empty() {
            self.config.logs_config_logs_dd_url = build_fqdn_logs(self.config.site.clone());
        } else {
            self.config.logs_config_logs_dd_url =
                logs_intake_url(&trim_url(&self.config.logs_config_logs_dd_url));
        }

        // If APM URL is not set, set it to the default
        if self.config.apm_dd_url.is_empty() {
            self.config.apm_dd_url = trace_intake_url(self.config.site.clone().as_str());
        } else {
            // If APM URL is set, add the site to the URL
            self.config.apm_dd_url = trace_intake_url_prefixed(&trim_url(&self.config.apm_dd_url));
        }

        self.config.clone()
    }
}

#[inline]
#[must_use]
fn trim_url(url: &str) -> String {
    url.trim_end_matches('/').to_owned()
}

#[inline]
#[must_use]
fn build_fqdn_logs(site: String) -> String {
    format!("https://http-intake.logs.{site}")
}

#[inline]
#[must_use]
fn logs_intake_url(url: &str) -> String {
    let url = url.trim();
    if url.is_empty() {
        return url.to_string();
    }
    if url.starts_with("https://") || url.starts_with("http://") {
        return url.to_string();
    }
    format!("https://{url}")
}

// ---------------------------------------------------------------------------
// Merge macros — used by sources and extension implementations
// ---------------------------------------------------------------------------

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

/// Batch-merge extension fields from a source struct.
///
/// Groups fields by merge strategy so you don't have to write individual
/// `merge_string!` / `merge_option_to_value!` / `merge_option!` calls.
///
/// ```ignore
/// merge_fields!(self, source,
///     string: [api_key_secret_arn, kms_api_key],
///     value:  [enhanced_metrics, capture_lambda_payload],
///     option: [span_dedup_timeout, appsec_rules],
/// );
/// ```
#[macro_export]
macro_rules! merge_fields {
    // Internal rules dispatched by keyword
    (@string $config:expr, $source:expr, [$($field:ident),* $(,)?]) => {
        $( $crate::merge_string!($config, $source, $field); )*
    };
    (@value $config:expr, $source:expr, [$($field:ident),* $(,)?]) => {
        $( $crate::merge_option_to_value!($config, $source, $field); )*
    };
    (@option $config:expr, $source:expr, [$($field:ident),* $(,)?]) => {
        $( $crate::merge_option!($config, $source, $field); )*
    };
    // Public entry point: accepts any combination of groups in any order
    ($config:expr, $source:expr, $($kind:ident: [$($field:ident),* $(,)?]),* $(,)?) => {
        $( $crate::merge_fields!(@$kind $config, $source, [$($field),*]); )*
    };
}

#[cfg_attr(coverage_nightly, coverage(off))] // Test modules skew coverage metrics
#[cfg(test)]
#[allow(clippy::result_large_err)]
pub mod tests {
    use libdd_trace_obfuscation::replacer::parse_rules_from_string;

    use super::*;

    use std::time::Duration;

    use crate::{TracePropagationStyle, log_level::LogLevel, processing_rule::ProcessingRule};

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
                "https://agent-http-intake-pci.logs.datadoghq.com:443".to_string()
            );
            Ok(())
        });
    }

    #[test]
    fn test_logs_intake_url_adds_prefix() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env(
                "DD_LOGS_CONFIG_LOGS_DD_URL",
                "dr-test-failover-http-intake.logs.datadoghq.com:443",
            );

            let config = get_config(Path::new(""));
            // ensure host:port URL is prefixed with https://
            assert_eq!(
                config.logs_config_logs_dd_url,
                "https://dr-test-failover-http-intake.logs.datadoghq.com:443".to_string()
            );
            Ok(())
        });
    }

    #[test]
    fn test_prefixed_logs_intake_url() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env(
                "DD_LOGS_CONFIG_LOGS_DD_URL",
                "https://custom-intake.logs.datadoghq.com:443",
            );

            let config = get_config(Path::new(""));
            assert_eq!(
                config.logs_config_logs_dd_url,
                "https://custom-intake.logs.datadoghq.com:443".to_string()
            );
            Ok(())
        });
    }

    #[test]
    fn test_logs_intake_url_trims_trailing_slash() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env(
                "DD_LOGS_CONFIG_LOGS_DD_URL",
                "https://custom-intake.logs.datadoghq.com/",
            );

            let config = get_config(Path::new(""));
            assert_eq!(
                config.logs_config_logs_dd_url,
                "https://custom-intake.logs.datadoghq.com".to_string()
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
    fn test_dd_dd_url_trims_trailing_slash() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_DD_URL", "https://test.agent.datadoghq.com/");

            let config = get_config(Path::new(""));
            assert_eq!(
                config.dd_url,
                "https://test.agent.datadoghq.com".to_string()
            );
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
    fn test_dd_url_trims_trailing_slash() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_URL", "https://test.datadoghq.com/");

            let config = get_config(Path::new(""));
            assert_eq!(config.url, "https://test.datadoghq.com".to_string());
            Ok(())
        });
    }

    #[test]
    fn test_apm_dd_url_trims_trailing_slash() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_APM_DD_URL", "https://test.agent.datadoghq.com/");

            let config = get_config(Path::new(""));
            assert_eq!(
                config.apm_dd_url,
                "https://test.agent.datadoghq.com/api/v0.2/traces".to_string()
            );
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
                    apm_dd_url: trace_intake_url("datadoghq.com").clone(),
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
            jail.set_env("DD_TRACE_PROPAGATION_STYLE", "datadog,tracecontext");
            let config = get_config(Path::new(""));

            let expected_styles = vec![
                TracePropagationStyle::Datadog,
                TracePropagationStyle::TraceContext,
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
    fn test_tags_comma_separated() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_TAGS", "team:serverless,env:prod,version:1.0");
            let config = get_config(Path::new(""));
            assert_eq!(config.tags.get("team"), Some(&"serverless".to_string()));
            assert_eq!(config.tags.get("env"), Some(&"prod".to_string()));
            assert_eq!(config.tags.get("version"), Some(&"1.0".to_string()));
            assert_eq!(config.tags.len(), 3);
            Ok(())
        });
    }

    #[test]
    fn test_tags_space_separated() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_TAGS", "team:serverless env:prod version:1.0");
            let config = get_config(Path::new(""));
            assert_eq!(config.tags.get("team"), Some(&"serverless".to_string()));
            assert_eq!(config.tags.get("env"), Some(&"prod".to_string()));
            assert_eq!(config.tags.get("version"), Some(&"1.0".to_string()));
            assert_eq!(config.tags.len(), 3);
            Ok(())
        });
    }

    #[test]
    fn test_tags_space_separated_with_extra_spaces() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_TAGS", "team:serverless  env:prod   version:1.0");
            let config = get_config(Path::new(""));
            assert_eq!(config.tags.get("team"), Some(&"serverless".to_string()));
            assert_eq!(config.tags.get("env"), Some(&"prod".to_string()));
            assert_eq!(config.tags.get("version"), Some(&"1.0".to_string()));
            assert_eq!(config.tags.len(), 3);
            Ok(())
        });
    }

    #[test]
    fn test_tags_mixed_separators() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_TAGS", "team:serverless,env:prod version:1.0");
            let config = get_config(Path::new(""));
            assert_eq!(config.tags.get("team"), Some(&"serverless".to_string()));
            assert_eq!(config.tags.get("env"), Some(&"prod".to_string()));
            assert_eq!(config.tags.get("version"), Some(&"1.0".to_string()));
            assert_eq!(config.tags.len(), 3);
            Ok(())
        });
    }

    #[test]
    fn test_parse_bool_from_anything() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_LOGS_CONFIG_USE_COMPRESSION", "TRUE");
            jail.set_env("DD_SKIP_SSL_VALIDATION", "1");
            let config = get_config(Path::new(""));
            assert!(config.logs_config_use_compression);
            assert!(config.skip_ssl_validation);
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
        // Negative and non-integer values gracefully fall back to None
        assert_eq!(
            serde_json::from_str::<Value>(r#"{"duration":-1}"#).expect("should not fail"),
            Value { duration: None }
        );
        assert_eq!(
            serde_json::from_str::<Value>(r#"{"duration":1000000}"#).expect("failed to parse JSON"),
            Value {
                duration: Some(Duration::from_secs(1))
            }
        );
        assert_eq!(
            serde_json::from_str::<Value>(r#"{"duration":-1.5}"#).expect("should not fail"),
            Value { duration: None }
        );
        assert_eq!(
            serde_json::from_str::<Value>(r#"{"duration":1.5}"#).expect("should not fail"),
            Value { duration: None }
        );
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
    fn test_deserialize_key_value_pairs_ignores_empty_keys() {
        #[derive(Deserialize, Debug, PartialEq)]
        struct TestStruct {
            #[serde(deserialize_with = "deserialize_key_value_pairs")]
            tags: HashMap<String, String>,
        }

        let result = serde_json::from_str::<TestStruct>(r#"{"tags": ":value,valid:tag"}"#)
            .expect("failed to parse JSON");
        let mut expected = HashMap::new();
        expected.insert("valid".to_string(), "tag".to_string());
        assert_eq!(result.tags, expected);
    }

    #[test]
    fn test_deserialize_key_value_pairs_ignores_empty_values() {
        #[derive(Deserialize, Debug, PartialEq)]
        struct TestStruct {
            #[serde(deserialize_with = "deserialize_key_value_pairs")]
            tags: HashMap<String, String>,
        }

        let result = serde_json::from_str::<TestStruct>(r#"{"tags": "key:,valid:tag"}"#)
            .expect("failed to parse JSON");
        let mut expected = HashMap::new();
        expected.insert("valid".to_string(), "tag".to_string());
        assert_eq!(result.tags, expected);
    }

    #[test]
    fn test_deserialize_key_value_pairs_with_url_values() {
        #[derive(Deserialize, Debug, PartialEq)]
        struct TestStruct {
            #[serde(deserialize_with = "deserialize_key_value_pairs")]
            tags: HashMap<String, String>,
        }

        let result = serde_json::from_str::<TestStruct>(
            r#"{"tags": "git.repository_url:https://gitlab.ddbuild.io/DataDog/serverless-e2e-tests.git,env:prod"}"#
        )
        .expect("failed to parse JSON");
        let mut expected = HashMap::new();
        expected.insert(
            "git.repository_url".to_string(),
            "https://gitlab.ddbuild.io/DataDog/serverless-e2e-tests.git".to_string(),
        );
        expected.insert("env".to_string(), "prod".to_string());
        assert_eq!(result.tags, expected);
    }

    #[test]
    fn test_deserialize_key_value_pair_array_with_urls() {
        #[derive(Deserialize, Debug, PartialEq)]
        struct TestStruct {
            #[serde(deserialize_with = "deserialize_key_value_pair_array_to_hashmap")]
            tags: HashMap<String, String>,
        }

        let result = serde_json::from_str::<TestStruct>(
            r#"{"tags": ["git.repository_url:https://gitlab.ddbuild.io/DataDog/serverless-e2e-tests.git", "env:prod", "version:1.2.3"]}"#
        )
        .expect("failed to parse JSON");
        let mut expected = HashMap::new();
        expected.insert(
            "git.repository_url".to_string(),
            "https://gitlab.ddbuild.io/DataDog/serverless-e2e-tests.git".to_string(),
        );
        expected.insert("env".to_string(), "prod".to_string());
        expected.insert("version".to_string(), "1.2.3".to_string());
        assert_eq!(result.tags, expected);
    }

    #[test]
    fn test_deserialize_key_value_pair_array_ignores_invalid() {
        #[derive(Deserialize, Debug, PartialEq)]
        struct TestStruct {
            #[serde(deserialize_with = "deserialize_key_value_pair_array_to_hashmap")]
            tags: HashMap<String, String>,
        }

        let result = serde_json::from_str::<TestStruct>(
            r#"{"tags": ["valid:tag", "invalid_no_colon", "another:good:value:with:colons"]}"#,
        )
        .expect("failed to parse JSON");
        let mut expected = HashMap::new();
        expected.insert("valid".to_string(), "tag".to_string());
        expected.insert("another".to_string(), "good:value:with:colons".to_string());
        assert_eq!(result.tags, expected);
    }

    #[test]
    fn test_deserialize_key_value_pair_array_empty() {
        #[derive(Deserialize, Debug, PartialEq)]
        struct TestStruct {
            #[serde(deserialize_with = "deserialize_key_value_pair_array_to_hashmap")]
            tags: HashMap<String, String>,
        }

        let result =
            serde_json::from_str::<TestStruct>(r#"{"tags": []}"#).expect("failed to parse JSON");
        assert_eq!(result.tags, HashMap::new());
    }

    // -- ConfigExtension tests --

    /// A test extension with a few fields, mimicking what a consumer like Lambda would define.
    #[derive(Clone, Default, Debug, PartialEq)]
    struct TestExtension {
        custom_flag: bool,
        custom_name: String,
    }

    #[derive(Clone, Default, Debug, Deserialize)]
    #[serde(default)]
    struct TestExtSource {
        #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
        custom_flag: Option<bool>,
        #[serde(deserialize_with = "deserialize_optional_string")]
        custom_name: Option<String>,
    }

    impl ConfigExtension for TestExtension {
        type Source = TestExtSource;

        fn merge_from(&mut self, source: &TestExtSource) {
            merge_fields!(self, source,
                string: [custom_name],
                value:  [custom_flag],
            );
        }
    }

    #[test]
    fn test_no_extension_config_works() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_SITE", "datad0g.com");
            let config = get_config(Path::new(""));
            assert_eq!(config.site, "datad0g.com");
            assert_eq!(config.ext, NoExtension);
            Ok(())
        });
    }

    #[test]
    fn test_extension_receives_env_vars() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_SITE", "datad0g.com");
            jail.set_env("DD_CUSTOM_FLAG", "true");
            jail.set_env("DD_CUSTOM_NAME", "my-extension");

            let config: Config<TestExtension> = get_config_with_extension(Path::new(""));

            // Core fields work
            assert_eq!(config.site, "datad0g.com");
            // Extension fields are populated
            assert!(config.ext.custom_flag);
            assert_eq!(config.ext.custom_name, "my-extension");
            Ok(())
        });
    }

    #[test]
    fn test_extension_receives_yaml_fields() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r#"
site: "datad0g.com"
custom_flag: true
custom_name: "yaml-ext"
"#,
            )?;

            let config: Config<TestExtension> = get_config_with_extension(Path::new(""));

            assert_eq!(config.site, "datad0g.com");
            assert!(config.ext.custom_flag);
            assert_eq!(config.ext.custom_name, "yaml-ext");
            Ok(())
        });
    }

    #[test]
    fn test_extension_env_overrides_yaml() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r#"
custom_name: "yaml-value"
custom_flag: false
"#,
            )?;
            jail.set_env("DD_CUSTOM_NAME", "env-value");
            jail.set_env("DD_CUSTOM_FLAG", "true");

            let config: Config<TestExtension> = get_config_with_extension(Path::new(""));

            // Env should override YAML (env source loaded after yaml)
            assert!(config.ext.custom_flag);
            assert_eq!(config.ext.custom_name, "env-value");
            Ok(())
        });
    }

    #[test]
    fn test_extension_defaults_when_not_set() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();

            let config: Config<TestExtension> = get_config_with_extension(Path::new(""));

            // Extension fields should be at their defaults
            assert!(!config.ext.custom_flag);
            assert_eq!(config.ext.custom_name, "");
            // Core fields should have post-processing defaults
            assert_eq!(config.site, "datadoghq.com");
            Ok(())
        });
    }

    #[test]
    fn test_extension_does_not_interfere_with_core() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DD_SITE", "us5.datadoghq.com");
            jail.set_env("DD_API_KEY", "test-key");
            jail.set_env("DD_CUSTOM_FLAG", "true");

            let config: Config<TestExtension> = get_config_with_extension(Path::new(""));

            // Core fields are not affected by extension env vars
            assert_eq!(config.site, "us5.datadoghq.com");
            assert_eq!(config.api_key, "test-key");
            // Extension fields work alongside core
            assert!(config.ext.custom_flag);
            Ok(())
        });
    }
}
