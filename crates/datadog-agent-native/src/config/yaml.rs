//! YAML file-based configuration (test-only).
//!
//! This module provides the ability to configure the Datadog agent using YAML
//! configuration files (e.g., `datadog.yaml`), following Datadog's traditional
//! configuration file format.
//!
//! **NOTE**: This module is only compiled for tests to reduce binary size.
//! Production builds use FFI arguments and environment variables for configuration.
//!
//! # Configuration Files
//!
//! YAML configuration files provide a structured way to configure all agent settings
//! in a human-readable format. This is the traditional configuration method used by
//! the full Datadog Agent.
//!
//! # Configuration Priority
//!
//! YAML configurations are loaded via the Figment configuration system and can be
//! combined with other configuration sources (environment variables, defaults, etc.).
//!
//! # Example Configuration
//!
//! ```yaml
//! api_key: your_api_key_here
//! site: datadoghq.com
//! service: my-service
//! env: production
//! tags:
//!   - team:platform
//!   - app:web
//! ```

#![cfg(test)]

use std::time::Duration;
use std::{collections::HashMap, path::PathBuf};

use crate::{
    config::{
        additional_endpoints::deserialize_additional_endpoints,
        deserialize_apm_replace_rules, deserialize_key_value_pair_array_to_hashmap,
        deserialize_option_lossless, deserialize_optional_bool_from_anything,
        deserialize_optional_duration_from_microseconds,
        deserialize_optional_duration_from_seconds, deserialize_optional_string,
        deserialize_processing_rules, deserialize_string_or_int,
        log_level::LogLevel,
        logs_additional_endpoints::LogsAdditionalEndpoint,
        service_mapping::deserialize_service_mapping,
        trace_propagation_style::{deserialize_trace_propagation_style, TracePropagationStyle},
        Config, ConfigError, ConfigSource, ProcessingRule,
    },
    merge_hashmap, merge_option, merge_option_to_value, merge_string, merge_vec,
};

#[cfg(test)]
use crate::config::operational_mode::OperationalMode;
use datadog_trace_obfuscation::replacer::ReplaceRule;
use figment::{
    providers::{Format, Yaml},
    Figment,
};
use serde::Deserialize;

/// `YamlConfig` is a struct that represents some of the fields in the `datadog.yaml` file.
///
/// It is used to deserialize the `datadog.yaml` file into a struct that can be merged
/// with the `Config` struct.
#[derive(Debug, PartialEq, Deserialize, Clone, Default)]
#[serde(default)]
#[allow(clippy::module_name_repetitions)]
pub struct YamlConfig {
    #[serde(deserialize_with = "deserialize_optional_string")]
    pub site: Option<String>,
    #[serde(deserialize_with = "deserialize_optional_string")]
    pub api_key: Option<String>,
    pub log_level: Option<LogLevel>,

    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub flush_timeout: Option<u64>,

    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub compression_level: Option<i32>,

    // Proxy
    pub proxy: ProxyConfig,
    // nit: this should probably be in the endpoints section
    #[serde(deserialize_with = "deserialize_optional_string")]
    pub dd_url: Option<String>,
    #[serde(deserialize_with = "deserialize_optional_string")]
    pub http_protocol: Option<String>,

    // Endpoints
    #[serde(deserialize_with = "deserialize_additional_endpoints")]
    /// Field used for Dual Shipping for Metrics
    pub additional_endpoints: HashMap<String, Vec<String>>,

    // Unified Service Tagging
    #[serde(deserialize_with = "deserialize_string_or_int")]
    pub env: Option<String>,
    #[serde(deserialize_with = "deserialize_string_or_int")]
    pub service: Option<String>,
    #[serde(deserialize_with = "deserialize_string_or_int")]
    pub version: Option<String>,
    #[serde(deserialize_with = "deserialize_key_value_pair_array_to_hashmap")]
    pub tags: HashMap<String, String>,

    // Logs
    pub logs_config: LogsConfig,

    // APM
    pub apm_config: ApmConfig,
    #[serde(deserialize_with = "deserialize_service_mapping")]
    pub service_mapping: HashMap<String, String>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub trace_aws_service_representation_enabled: Option<bool>,
    // Trace Propagation
    #[serde(deserialize_with = "deserialize_trace_propagation_style")]
    pub trace_propagation_style: Vec<TracePropagationStyle>,
    #[serde(deserialize_with = "deserialize_trace_propagation_style")]
    pub trace_propagation_style_extract: Vec<TracePropagationStyle>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub trace_propagation_extract_first: Option<bool>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub trace_propagation_http_baggage_enabled: Option<bool>,

    // Metrics
    pub metrics_config: MetricsConfig,

    // Enhanced Metrics
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub enhanced_metrics: Option<bool>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub compute_trace_stats_on_extension: Option<bool>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub appsec_enabled: Option<bool>,
    #[serde(deserialize_with = "deserialize_optional_string")]
    pub appsec_rules: Option<String>,
    #[serde(deserialize_with = "deserialize_optional_duration_from_microseconds")]
    pub appsec_waf_timeout: Option<Duration>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub api_security_enabled: Option<bool>,
    #[serde(deserialize_with = "deserialize_optional_duration_from_seconds")]
    pub api_security_sample_delay: Option<Duration>,

    // Remote Configuration
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub remote_configuration_enabled: Option<bool>,
    #[serde(deserialize_with = "deserialize_optional_string")]
    pub remote_configuration_api_key: Option<String>,
    #[serde(deserialize_with = "deserialize_optional_string")]
    pub remote_configuration_key: Option<String>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub remote_configuration_no_tls: Option<bool>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub remote_configuration_no_tls_validation: Option<bool>,

    // Operational Mode
    #[serde(deserialize_with = "deserialize_optional_string")]
    pub operational_mode: Option<String>,
    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub trace_agent_port: Option<u16>,
    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub metrics_agent_port: Option<u16>,
    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub logs_agent_port: Option<u16>,
}

/// Proxy Config
///

#[derive(Debug, PartialEq, Deserialize, Clone, Default)]
#[serde(default)]
#[allow(clippy::module_name_repetitions)]
pub struct ProxyConfig {
    pub https: Option<String>,
    pub no_proxy: Option<Vec<String>>,
}

/// Logs Config
///

#[derive(Debug, PartialEq, Deserialize, Clone, Default)]
#[serde(default)]
#[allow(clippy::module_name_repetitions)]
pub struct LogsConfig {
    pub logs_dd_url: Option<String>,
    #[serde(deserialize_with = "deserialize_processing_rules")]
    pub processing_rules: Option<Vec<ProcessingRule>>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub use_compression: Option<bool>,
    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub compression_level: Option<i32>,
    pub additional_endpoints: Vec<LogsAdditionalEndpoint>,
}

/// Metrics specific config
///
#[derive(Debug, PartialEq, Deserialize, Clone, Copy, Default)]
#[serde(default)]
#[allow(clippy::module_name_repetitions)]
pub struct MetricsConfig {
    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub compression_level: Option<i32>,
}

/// APM Config
///

#[derive(Debug, PartialEq, Deserialize, Clone, Default)]
#[serde(default)]
#[allow(clippy::module_name_repetitions)]
pub struct ApmConfig {
    pub apm_dd_url: Option<String>,
    #[serde(deserialize_with = "deserialize_apm_replace_rules")]
    pub replace_tags: Option<Vec<ReplaceRule>>,
    pub obfuscation: Option<ApmObfuscation>,
    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub compression_level: Option<i32>,
    pub features: Vec<String>,
    #[serde(deserialize_with = "deserialize_additional_endpoints")]
    pub additional_endpoints: HashMap<String, Vec<String>>,
}

impl ApmConfig {
    #[must_use]
    pub fn obfuscation_http_remove_query_string(&self) -> Option<bool> {
        self.obfuscation
            .as_ref()
            .and_then(|obfuscation| obfuscation.http.remove_query_string)
    }

    #[must_use]
    pub fn obfuscation_http_remove_paths_with_digits(&self) -> Option<bool> {
        self.obfuscation
            .as_ref()
            .and_then(|obfuscation| obfuscation.http.remove_paths_with_digits)
    }
}

#[derive(Debug, PartialEq, Deserialize, Clone, Copy, Default)]
#[serde(default)]
#[allow(clippy::module_name_repetitions)]
pub struct ApmObfuscation {
    pub http: ApmHttpObfuscation,
}

#[derive(Debug, PartialEq, Deserialize, Clone, Copy, Default)]
#[serde(default)]
#[allow(clippy::module_name_repetitions)]
pub struct ApmHttpObfuscation {
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub remove_query_string: Option<bool>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub remove_paths_with_digits: Option<bool>,
}

#[allow(clippy::too_many_lines)]
fn merge_config(config: &mut Config, yaml_config: &YamlConfig) {
    // Basic fields
    merge_string!(config, yaml_config, site);
    merge_string!(config, yaml_config, api_key);
    merge_option_to_value!(config, yaml_config, log_level);
    merge_option_to_value!(config, yaml_config, flush_timeout);

    // Unified Service Tagging
    merge_option!(config, yaml_config, env);
    merge_option!(config, yaml_config, service);
    merge_option!(config, yaml_config, version);
    merge_hashmap!(config, yaml_config, tags);

    merge_option_to_value!(config, yaml_config, compression_level);
    // Proxy
    merge_option!(config, proxy_https, yaml_config.proxy, https);
    merge_option_to_value!(config, proxy_no_proxy, yaml_config.proxy, no_proxy);
    merge_option!(config, yaml_config, http_protocol);

    // Endpoints
    merge_hashmap!(config, yaml_config, additional_endpoints);
    merge_string!(config, yaml_config, dd_url);

    // Logs
    merge_string!(
        config,
        logs_config_logs_dd_url,
        yaml_config.logs_config,
        logs_dd_url
    );
    merge_option!(
        config,
        logs_config_processing_rules,
        yaml_config.logs_config,
        processing_rules
    );
    merge_option_to_value!(
        config,
        logs_config_use_compression,
        yaml_config.logs_config,
        use_compression
    );
    merge_option_to_value!(
        config,
        logs_config_compression_level,
        yaml_config,
        compression_level
    );
    merge_option_to_value!(
        config,
        logs_config_compression_level,
        yaml_config.logs_config,
        compression_level
    );
    merge_vec!(
        config,
        logs_config_additional_endpoints,
        yaml_config.logs_config,
        additional_endpoints
    );

    merge_option_to_value!(
        config,
        metrics_config_compression_level,
        yaml_config,
        compression_level
    );

    merge_option_to_value!(
        config,
        metrics_config_compression_level,
        yaml_config.metrics_config,
        compression_level
    );

    // APM
    merge_hashmap!(config, yaml_config, service_mapping);
    merge_string!(config, apm_dd_url, yaml_config.apm_config, apm_dd_url);
    merge_option!(
        config,
        apm_replace_tags,
        yaml_config.apm_config,
        replace_tags
    );
    merge_option_to_value!(
        config,
        apm_config_compression_level,
        yaml_config,
        compression_level
    );
    merge_option_to_value!(
        config,
        apm_config_compression_level,
        yaml_config.apm_config,
        compression_level
    );
    merge_hashmap!(
        config,
        apm_additional_endpoints,
        yaml_config.apm_config,
        additional_endpoints
    );

    // Not using the macro here because we need to call a method on the struct
    if let Some(remove_query_string) = yaml_config
        .apm_config
        .obfuscation_http_remove_query_string()
    {
        config
            .apm_config_obfuscation_http_remove_query_string
            .clone_from(&remove_query_string);
    }
    if let Some(remove_paths_with_digits) = yaml_config
        .apm_config
        .obfuscation_http_remove_paths_with_digits()
    {
        config
            .apm_config_obfuscation_http_remove_paths_with_digits
            .clone_from(&remove_paths_with_digits);
    }

    merge_vec!(config, apm_features, yaml_config.apm_config, features);

    // Trace Propagation
    merge_vec!(config, yaml_config, trace_propagation_style);
    merge_vec!(config, yaml_config, trace_propagation_style_extract);
    merge_option_to_value!(config, yaml_config, trace_propagation_extract_first);
    merge_option_to_value!(config, yaml_config, trace_propagation_http_baggage_enabled);
    merge_option_to_value!(
        config,
        yaml_config,
        trace_aws_service_representation_enabled
    );

    // Enhanced Metrics and AppSec
    merge_option_to_value!(config, yaml_config, enhanced_metrics);
    merge_option_to_value!(config, yaml_config, compute_trace_stats_on_extension);
    merge_option_to_value!(config, yaml_config, appsec_enabled);
    merge_option!(config, yaml_config, appsec_rules);
    merge_option_to_value!(config, yaml_config, appsec_waf_timeout);
    merge_option_to_value!(config, yaml_config, api_security_enabled);
    merge_option_to_value!(config, yaml_config, api_security_sample_delay);

    // Remote Configuration
    merge_option_to_value!(config, yaml_config, remote_configuration_enabled);
    merge_option!(config, yaml_config, remote_configuration_api_key);
    merge_option!(config, yaml_config, remote_configuration_key);
    merge_option_to_value!(config, yaml_config, remote_configuration_no_tls);
    merge_option_to_value!(config, yaml_config, remote_configuration_no_tls_validation);

    // Operational Mode
    if let Some(mode_str) = &yaml_config.operational_mode {
        use crate::config::operational_mode::OperationalMode;
        if let Some(mode) = OperationalMode::from_env_str(mode_str) {
            config.operational_mode = mode;
        }
        // If invalid, keep default (HttpFixedPort)
    }
    merge_option!(config, yaml_config, trace_agent_port);
    merge_option!(config, yaml_config, metrics_agent_port);
    merge_option!(config, yaml_config, logs_agent_port);
}

#[derive(Debug, PartialEq, Clone)]
#[allow(clippy::module_name_repetitions)]
pub struct YamlConfigSource {
    pub path: PathBuf,
}

impl ConfigSource for YamlConfigSource {
    fn load(&self, config: &mut Config) -> Result<(), ConfigError> {
        let figment = Figment::new().merge(Yaml::file(self.path.clone()));

        match figment.extract::<YamlConfig>() {
            Ok(yaml_config) => merge_config(config, &yaml_config),
            Err(e) => {
                return Err(ConfigError::ParseError(format!(
                    "Failed to parse config from yaml file: {e}, using default config."
                )));
            }
        }

        Ok(())
    }
}

#[cfg_attr(coverage_nightly, coverage(off))] // Test modules skew coverage metrics
#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use crate::config::processing_rule::Kind;

    use super::*;

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_merge_config_overrides_with_yaml_file() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r#"
# Basic fields
site: "test-site"
api_key: "test-api-key"
log_level: "debug"
flush_timeout: 42
compression_level: 4
# Proxy
proxy:
  https: "https://proxy.example.com"
  no_proxy: ["localhost", "127.0.0.1"]
dd_url: "https://metrics.datadoghq.com"
http_protocol: "http1"

# Endpoints
additional_endpoints:
  "https://app.datadoghq.com":
    - apikey2
    - apikey3
  "https://app.datadoghq.eu":
    - apikey4

# Unified Service Tagging
env: "test-env"
service: "test-service"
version: "1.0.0"
tags:
  - "team:test-team"
  - "project:test-project"

# Logs
logs_config:
  logs_dd_url: "https://logs.datadoghq.com"
  processing_rules:
    - name: "test-exclude"
      type: "exclude_at_match"
      pattern: "test-pattern"
  use_compression: false
  compression_level: 1
  additional_endpoints:
    - api_key: "apikey2"
      Host: "agent-http-intake.logs.datadoghq.com"
      Port: 443
      is_reliable: true

# APM
apm_config:
  apm_dd_url: "https://apm.datadoghq.com"
  replace_tags: []
  obfuscation:
    http:
      remove_query_string: true
      remove_paths_with_digits: true
  compression_level: 2
  features:
    - "enable_stats_by_span_kind"
  additional_endpoints:
    "https://trace.agent.datadoghq.com":
        - apikey2
        - apikey3
    "https://trace.agent.datadoghq.eu":
        - apikey4

service_mapping: old-service:new-service

# Trace Propagation
trace_propagation_style: "datadog"
trace_propagation_style_extract: "b3"
trace_propagation_extract_first: true
trace_propagation_http_baggage_enabled: true
trace_aws_service_representation_enabled: true

metrics_config:
  compression_level: 3

# Enhanced Metrics
enhanced_metrics: false
compute_trace_stats_on_extension: true

# AppSec
appsec_rules: "/path/to/rules.json"
appsec_waf_timeout: 1000000 # Microseconds
api_security_enabled: false
api_security_sample_delay: 60 # Seconds

# Operational Mode
operational_mode: "http_ephemeral_port"
trace_agent_port: 9999
metrics_agent_port: 8888
logs_agent_port: 7777
"#,
            )?;

            let mut config = Config::default();
            let yaml_config_source = YamlConfigSource {
                path: Path::new("datadog.yaml").to_path_buf(),
            };
            yaml_config_source
                .load(&mut config)
                .expect("Failed to load config");

            let expected_config = Config {
                site: "test-site".to_string(),
                api_key: "test-api-key".to_string(),
                log_level: LogLevel::Debug,
                flush_timeout: 42,
                compression_level: 4,
                proxy_https: Some("https://proxy.example.com".to_string()),
                proxy_no_proxy: vec!["localhost".to_string(), "127.0.0.1".to_string()],
                http_protocol: Some("http1".to_string()),
                dd_url: "https://metrics.datadoghq.com".to_string(),
                url: String::new(), // doesnt exist in yaml
                additional_endpoints: HashMap::from([
                    (
                        "https://app.datadoghq.com".to_string(),
                        vec!["apikey2".to_string(), "apikey3".to_string()],
                    ),
                    (
                        "https://app.datadoghq.eu".to_string(),
                        vec!["apikey4".to_string()],
                    ),
                ]),
                env: Some("test-env".to_string()),
                service: Some("test-service".to_string()),
                version: Some("1.0.0".to_string()),
                tags: HashMap::from([
                    ("team".to_string(), "test-team".to_string()),
                    ("project".to_string(), "test-project".to_string()),
                ]),
                logs_config_logs_dd_url: "https://logs.datadoghq.com".to_string(),
                logs_config_processing_rules: Some(vec![ProcessingRule {
                    name: "test-exclude".to_string(),
                    pattern: "test-pattern".to_string(),
                    kind: Kind::ExcludeAtMatch,
                    replace_placeholder: None,
                }]),
                logs_config_use_compression: false,
                logs_config_compression_level: 1,
                logs_config_additional_endpoints: vec![LogsAdditionalEndpoint {
                    api_key: "apikey2".to_string(),
                    host: "agent-http-intake.logs.datadoghq.com".to_string(),
                    port: 443,
                    is_reliable: true,
                }],
                observability_pipelines_worker_logs_enabled: false,
                observability_pipelines_worker_logs_url: String::default(),
                service_mapping: HashMap::from([(
                    "old-service".to_string(),
                    "new-service".to_string(),
                )]),
                apm_dd_url: "https://apm.datadoghq.com".to_string(),
                apm_replace_tags: Some(vec![]),
                apm_config_obfuscation_http_remove_query_string: true,
                apm_config_obfuscation_http_remove_paths_with_digits: true,
                apm_config_compression_level: 2,
                apm_features: vec!["enable_stats_by_span_kind".to_string()],
                apm_additional_endpoints: HashMap::from([
                    (
                        "https://trace.agent.datadoghq.com".to_string(),
                        vec!["apikey2".to_string(), "apikey3".to_string()],
                    ),
                    (
                        "https://trace.agent.datadoghq.eu".to_string(),
                        vec!["apikey4".to_string()],
                    ),
                ]),
                trace_propagation_style: vec![TracePropagationStyle::Datadog],
                trace_propagation_style_extract: vec![TracePropagationStyle::B3],
                trace_propagation_extract_first: true,
                trace_propagation_http_baggage_enabled: true,
                trace_aws_service_representation_enabled: true,
                metrics_config_compression_level: 3,
                enhanced_metrics: false,
                compute_trace_stats_on_extension: true,
                appsec_enabled: false,
                appsec_rules: Some("/path/to/rules.json".to_string()),
                appsec_waf_timeout: Duration::from_secs(1),
                api_security_enabled: false,
                api_security_sample_delay: Duration::from_secs(60),
                remote_configuration_enabled: true,
                remote_configuration_api_key: None,
                remote_configuration_key: None,
                remote_configuration_no_tls: false,
                remote_configuration_no_tls_validation: false,

                apm_filter_tags_require: None,
                apm_filter_tags_reject: None,
                apm_filter_tags_regex_require: None,
                apm_filter_tags_regex_reject: None,
                operational_mode: OperationalMode::HttpEphemeralPort,
                trace_agent_port: Some(9999),
                trace_agent_uds_path: None,
                trace_agent_uds_permissions: 0o600,
                metrics_agent_port: Some(8888),
                logs_agent_port: Some(7777),
                // DogStatsD Configuration - using defaults since no yaml config for it in this test
                dogstatsd_enabled: false,
                dogstatsd_port: 8125,
            };

            // Assert that
            assert_eq!(config, expected_config);

            Ok(())
        });
    }

    #[test]
    fn test_operational_mode_http_fixed_port_from_yaml() {
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

            let mut config = Config::default();
            let yaml_config_source = YamlConfigSource {
                path: Path::new("datadog.yaml").to_path_buf(),
            };
            yaml_config_source
                .load(&mut config)
                .expect("Failed to load config");

            assert_eq!(config.operational_mode, OperationalMode::HttpFixedPort);
            assert_eq!(config.trace_agent_port, Some(8126));

            Ok(())
        });
    }

    #[test]
    fn test_operational_mode_ffi_only_from_yaml() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r#"
api_key: "test-api-key"
operational_mode: "http_uds"
"#,
            )?;

            let mut config = Config::default();
            let yaml_config_source = YamlConfigSource {
                path: Path::new("datadog.yaml").to_path_buf(),
            };
            yaml_config_source
                .load(&mut config)
                .expect("Failed to load config");

            assert_eq!(config.operational_mode, OperationalMode::HttpUds);
            // Ports should be None in UDS mode
            assert_eq!(config.trace_agent_port, None);

            Ok(())
        });
    }

    #[test]
    fn test_operational_mode_aliases_from_yaml() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();

            // Test "fixed" alias
            jail.create_file(
                "datadog1.yaml",
                r#"
api_key: "test-api-key"
operational_mode: "fixed"
"#,
            )?;

            let mut config = Config::default();
            let yaml_config_source = YamlConfigSource {
                path: Path::new("datadog1.yaml").to_path_buf(),
            };
            yaml_config_source
                .load(&mut config)
                .expect("Failed to load config");
            assert_eq!(config.operational_mode, OperationalMode::HttpFixedPort);

            // Test "ephemeral" alias
            jail.create_file(
                "datadog2.yaml",
                r#"
api_key: "test-api-key"
operational_mode: "ephemeral"
"#,
            )?;

            let mut config = Config::default();
            let yaml_config_source = YamlConfigSource {
                path: Path::new("datadog2.yaml").to_path_buf(),
            };
            yaml_config_source
                .load(&mut config)
                .expect("Failed to load config");
            assert_eq!(config.operational_mode, OperationalMode::HttpEphemeralPort);

            // Test "uds" alias
            jail.create_file(
                "datadog3.yaml",
                r#"
api_key: "test-api-key"
operational_mode: "uds"
"#,
            )?;

            let mut config = Config::default();
            let yaml_config_source = YamlConfigSource {
                path: Path::new("datadog3.yaml").to_path_buf(),
            };
            yaml_config_source
                .load(&mut config)
                .expect("Failed to load config");
            assert_eq!(config.operational_mode, OperationalMode::HttpUds);

            Ok(())
        });
    }

    #[test]
    fn test_invalid_operational_mode_defaults_to_fixed_port() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r#"
api_key: "test-api-key"
operational_mode: "invalid_mode_value"
trace_agent_port: 8126
"#,
            )?;

            let mut config = Config::default();
            let yaml_config_source = YamlConfigSource {
                path: Path::new("datadog.yaml").to_path_buf(),
            };
            yaml_config_source
                .load(&mut config)
                .expect("Failed to load config");

            // Invalid mode should default to HttpFixedPort
            assert_eq!(config.operational_mode, OperationalMode::HttpFixedPort);
            assert_eq!(config.trace_agent_port, Some(8126));

            Ok(())
        });
    }

    #[test]
    fn test_all_agent_ports_from_yaml() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r#"
api_key: "test-api-key"
trace_agent_port: 19999
metrics_agent_port: 18888
logs_agent_port: 17777
"#,
            )?;

            let mut config = Config::default();
            let yaml_config_source = YamlConfigSource {
                path: Path::new("datadog.yaml").to_path_buf(),
            };
            yaml_config_source
                .load(&mut config)
                .expect("Failed to load config");

            assert_eq!(config.trace_agent_port, Some(19999));
            assert_eq!(config.metrics_agent_port, Some(18888));
            assert_eq!(config.logs_agent_port, Some(17777));

            Ok(())
        });
    }
}
