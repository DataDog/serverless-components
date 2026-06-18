use std::{collections::HashMap, path::PathBuf};

use crate::{
    Config, ConfigError, ConfigExtension, ConfigSource, ProcessingRule, TracePropagationStyle,
    additional_endpoints::deserialize_additional_endpoints, deserialize_apm_replace_rules,
    deserialize_key_value_pair_array_to_hashmap, deserialize_option_lossless,
    deserialize_optional_bool_from_anything, deserialize_optional_string,
    deserialize_processing_rules, deserialize_string_or_int, deserialize_trace_propagation_style,
    deserialize_with_default, log_level::LogLevel,
    logs_additional_endpoints::LogsAdditionalEndpoint, merge_hashmap, merge_option,
    merge_option_to_value, merge_string, merge_vec, service_mapping::deserialize_service_mapping,
};
use figment::{
    Figment,
    providers::{Format, Yaml},
};
use libdd_trace_obfuscation::replacer::ReplaceRule;
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
    /// YAML key: `org_uuid`. Datadog organization UUID. When set, enables
    /// delegated auth so the agent can submit telemetry without a long-lived
    /// API key. Merges into the resolved config field `dd_org_uuid`.
    #[serde(deserialize_with = "deserialize_optional_string")]
    pub org_uuid: Option<String>,
    #[serde(deserialize_with = "deserialize_with_default")]
    pub log_level: Option<LogLevel>,

    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub flush_timeout: Option<u64>,

    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub compression_level: Option<i32>,

    // Proxy
    #[serde(deserialize_with = "deserialize_with_default")]
    pub proxy: ProxyConfig,
    // nit: this should probably be in the endpoints section
    #[serde(deserialize_with = "deserialize_optional_string")]
    pub dd_url: Option<String>,
    #[serde(deserialize_with = "deserialize_optional_string")]
    pub http_protocol: Option<String>,
    #[serde(deserialize_with = "deserialize_optional_string")]
    pub tls_cert_file: Option<String>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub skip_ssl_validation: Option<bool>,

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
    #[serde(deserialize_with = "deserialize_with_default")]
    pub logs_config: LogsConfig,

    // APM
    #[serde(deserialize_with = "deserialize_with_default")]
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
    #[serde(deserialize_with = "deserialize_with_default")]
    pub metrics_config: MetricsConfig,

    // DogStatsD
    /// Size of the receive buffer for `DogStatsD` UDP packets, in bytes (`SO_RCVBUF`).
    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub dogstatsd_so_rcvbuf: Option<usize>,
    /// Maximum size of a single read from any transport (UDP or named pipe), in bytes.
    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub dogstatsd_buffer_size: Option<usize>,
    /// Internal queue capacity between the socket reader and metric processor.
    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub dogstatsd_queue_size: Option<usize>,

    // OTLP
    #[serde(deserialize_with = "deserialize_with_default")]
    pub otlp_config: Option<OtlpConfig>,
}

/// Proxy Config
///

#[derive(Debug, PartialEq, Deserialize, Clone, Default)]
#[serde(default)]
#[allow(clippy::module_name_repetitions)]
pub struct ProxyConfig {
    #[serde(deserialize_with = "deserialize_with_default")]
    pub https: Option<String>,
    #[serde(deserialize_with = "deserialize_with_default")]
    pub no_proxy: Option<Vec<String>>,
}

/// Logs Config
///

#[derive(Debug, PartialEq, Deserialize, Clone, Default)]
#[serde(default)]
#[allow(clippy::module_name_repetitions)]
pub struct LogsConfig {
    #[serde(deserialize_with = "deserialize_with_default")]
    pub logs_dd_url: Option<String>,
    #[serde(deserialize_with = "deserialize_processing_rules")]
    pub processing_rules: Option<Vec<ProcessingRule>>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub use_compression: Option<bool>,
    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub compression_level: Option<i32>,
    #[serde(deserialize_with = "deserialize_with_default")]
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
    #[serde(deserialize_with = "deserialize_with_default")]
    pub apm_dd_url: Option<String>,
    #[serde(deserialize_with = "deserialize_apm_replace_rules")]
    pub replace_tags: Option<Vec<ReplaceRule>>,
    #[serde(deserialize_with = "deserialize_with_default")]
    pub obfuscation: Option<ApmObfuscation>,
    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub compression_level: Option<i32>,
    #[serde(deserialize_with = "deserialize_with_default")]
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
    #[serde(deserialize_with = "deserialize_with_default")]
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

/// OTLP Config
///

#[derive(Debug, PartialEq, Deserialize, Clone, Default)]
#[serde(default)]
#[allow(clippy::module_name_repetitions)]
pub struct OtlpConfig {
    #[serde(deserialize_with = "deserialize_with_default")]
    pub receiver: Option<OtlpReceiverConfig>,
    #[serde(deserialize_with = "deserialize_with_default")]
    pub traces: Option<OtlpTracesConfig>,

    // NOT SUPPORTED
    #[serde(deserialize_with = "deserialize_with_default")]
    pub metrics: Option<OtlpMetricsConfig>,
    #[serde(deserialize_with = "deserialize_with_default")]
    pub logs: Option<OtlpLogsConfig>,
}

#[derive(Debug, PartialEq, Deserialize, Clone, Default)]
#[serde(default)]
#[allow(clippy::module_name_repetitions)]
pub struct OtlpReceiverConfig {
    #[serde(deserialize_with = "deserialize_with_default")]
    pub protocols: Option<OtlpReceiverProtocolsConfig>,
}

#[derive(Debug, PartialEq, Deserialize, Clone, Default)]
#[serde(default)]
#[allow(clippy::module_name_repetitions)]
pub struct OtlpReceiverProtocolsConfig {
    #[serde(deserialize_with = "deserialize_with_default")]
    pub http: Option<OtlpReceiverHttpConfig>,

    // NOT SUPPORTED
    #[serde(deserialize_with = "deserialize_with_default")]
    pub grpc: Option<OtlpReceiverGrpcConfig>,
}

#[derive(Debug, PartialEq, Deserialize, Clone, Default)]
#[serde(default)]
#[allow(clippy::module_name_repetitions)]
pub struct OtlpReceiverHttpConfig {
    #[serde(deserialize_with = "deserialize_with_default")]
    pub endpoint: Option<String>,
}

#[derive(Debug, PartialEq, Deserialize, Clone, Default)]
#[serde(default)]
#[allow(clippy::module_name_repetitions)]
pub struct OtlpReceiverGrpcConfig {
    #[serde(deserialize_with = "deserialize_with_default")]
    pub endpoint: Option<String>,
    #[serde(deserialize_with = "deserialize_with_default")]
    pub transport: Option<String>,
    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub max_recv_msg_size_mib: Option<i32>,
}

#[derive(Debug, PartialEq, Deserialize, Clone, Default)]
#[serde(default)]
#[allow(clippy::module_name_repetitions)]
pub struct OtlpTracesConfig {
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub enabled: Option<bool>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub span_name_as_resource_name: Option<bool>,
    #[serde(deserialize_with = "deserialize_with_default")]
    pub span_name_remappings: HashMap<String, String>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub ignore_missing_datadog_fields: Option<bool>,

    // NOT SUPORTED
    #[serde(deserialize_with = "deserialize_with_default")]
    pub probabilistic_sampler: Option<OtlpTracesProbabilisticSampler>,
}

#[derive(Debug, PartialEq, Clone, Deserialize, Default, Copy)]
pub struct OtlpTracesProbabilisticSampler {
    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub sampling_percentage: Option<i32>,
}

#[derive(Debug, PartialEq, Deserialize, Clone, Default)]
#[serde(default)]
pub struct OtlpMetricsConfig {
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub enabled: Option<bool>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub resource_attributes_as_tags: Option<bool>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub instrumentation_scope_metadata_as_tags: Option<bool>,
    #[serde(deserialize_with = "deserialize_with_default")]
    pub tag_cardinality: Option<String>,
    #[serde(deserialize_with = "deserialize_option_lossless")]
    pub delta_ttl: Option<i32>,
    #[serde(deserialize_with = "deserialize_with_default")]
    pub histograms: Option<OtlpMetricsHistograms>,
    #[serde(deserialize_with = "deserialize_with_default")]
    pub sums: Option<OtlpMetricsSums>,
    #[serde(deserialize_with = "deserialize_with_default")]
    pub summaries: Option<OtlpMetricsSummaries>,
}

#[derive(Debug, PartialEq, Clone, Deserialize, Default)]
#[serde(default)]
pub struct OtlpMetricsHistograms {
    #[serde(deserialize_with = "deserialize_with_default")]
    pub mode: Option<String>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub send_count_sum_metrics: Option<bool>,
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub send_aggregation_metrics: Option<bool>,
}

#[derive(Debug, PartialEq, Clone, Deserialize, Default)]
#[serde(default)]
pub struct OtlpMetricsSums {
    #[serde(deserialize_with = "deserialize_with_default")]
    pub cumulative_monotonic_mode: Option<String>,
    #[serde(deserialize_with = "deserialize_with_default")]
    pub initial_cumulative_monotonic_value: Option<String>,
}

#[derive(Debug, PartialEq, Clone, Deserialize, Default)]
#[serde(default)]
pub struct OtlpMetricsSummaries {
    #[serde(deserialize_with = "deserialize_with_default")]
    pub mode: Option<String>,
}

#[derive(Debug, PartialEq, Clone, Deserialize, Default, Copy)]
#[serde(default)]
pub struct OtlpLogsConfig {
    #[serde(deserialize_with = "deserialize_optional_bool_from_anything")]
    pub enabled: Option<bool>,
}

impl OtlpConfig {
    #[must_use]
    pub fn receiver_protocols_http_endpoint(&self) -> Option<String> {
        self.receiver.as_ref().and_then(|receiver| {
            receiver.protocols.as_ref().and_then(|protocols| {
                protocols
                    .http
                    .as_ref()
                    .and_then(|http| http.endpoint.clone())
            })
        })
    }

    #[must_use]
    pub fn receiver_protocols_grpc(&self) -> Option<&OtlpReceiverGrpcConfig> {
        self.receiver.as_ref().and_then(|receiver| {
            receiver
                .protocols
                .as_ref()
                .and_then(|protocols| protocols.grpc.as_ref())
        })
    }

    #[must_use]
    pub fn traces_enabled(&self) -> Option<bool> {
        self.traces.as_ref().and_then(|traces| traces.enabled)
    }

    #[must_use]
    pub fn traces_ignore_missing_datadog_fields(&self) -> Option<bool> {
        self.traces
            .as_ref()
            .and_then(|traces| traces.ignore_missing_datadog_fields)
    }

    #[must_use]
    pub fn traces_span_name_as_resource_name(&self) -> Option<bool> {
        self.traces
            .as_ref()
            .and_then(|traces| traces.span_name_as_resource_name)
    }

    #[must_use]
    pub fn traces_span_name_remappings(&self) -> HashMap<String, String> {
        self.traces
            .as_ref()
            .map(|traces| traces.span_name_remappings.clone())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn traces_probabilistic_sampler(&self) -> Option<&OtlpTracesProbabilisticSampler> {
        self.traces
            .as_ref()
            .and_then(|traces| traces.probabilistic_sampler.as_ref())
    }

    #[must_use]
    pub fn logs(&self) -> Option<&OtlpLogsConfig> {
        self.logs.as_ref()
    }
}

#[allow(clippy::too_many_lines)]
fn merge_config<E: ConfigExtension>(config: &mut Config<E>, yaml_config: &YamlConfig) {
    // Basic fields
    merge_string!(config, yaml_config, site);
    merge_string!(config, yaml_config, api_key);
    merge_string!(config, dd_org_uuid, yaml_config, org_uuid);
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
    merge_option!(config, yaml_config, tls_cert_file);
    merge_option_to_value!(config, yaml_config, skip_ssl_validation);

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

    // DogStatsD
    merge_option!(config, yaml_config, dogstatsd_so_rcvbuf);
    merge_option!(config, yaml_config, dogstatsd_buffer_size);
    merge_option!(config, yaml_config, dogstatsd_queue_size);

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

    // OTLP
    if let Some(otlp_config) = &yaml_config.otlp_config {
        // Traces

        // Not using macros in some cases because we need to call a method on the struct
        if let Some(traces_enabled) = otlp_config.traces_enabled() {
            config
                .otlp_config_traces_enabled
                .clone_from(&traces_enabled);
        }
        if let Some(traces_span_name_as_resource_name) =
            otlp_config.traces_span_name_as_resource_name()
        {
            config
                .otlp_config_traces_span_name_as_resource_name
                .clone_from(&traces_span_name_as_resource_name);
        }

        let traces_span_name_remappings = otlp_config.traces_span_name_remappings();
        if !traces_span_name_remappings.is_empty() {
            config
                .otlp_config_traces_span_name_remappings
                .clone_from(&traces_span_name_remappings);
        }
        if let Some(traces_ignore_missing_datadog_fields) =
            otlp_config.traces_ignore_missing_datadog_fields()
        {
            config
                .otlp_config_ignore_missing_datadog_fields
                .clone_from(&traces_ignore_missing_datadog_fields);
        }

        if let Some(probabilistic_sampler) = otlp_config.traces_probabilistic_sampler() {
            merge_option!(
                config,
                otlp_config_traces_probabilistic_sampler_sampling_percentage,
                probabilistic_sampler,
                sampling_percentage
            );
        }

        // Receiver
        let receiver_protocols_http_endpoint = otlp_config.receiver_protocols_http_endpoint();
        if receiver_protocols_http_endpoint.is_some() {
            config
                .otlp_config_receiver_protocols_http_endpoint
                .clone_from(&receiver_protocols_http_endpoint);
        }

        if let Some(receiver_protocols_grpc) = otlp_config.receiver_protocols_grpc() {
            merge_option!(
                config,
                otlp_config_receiver_protocols_grpc_endpoint,
                receiver_protocols_grpc,
                endpoint
            );
            merge_option!(
                config,
                otlp_config_receiver_protocols_grpc_transport,
                receiver_protocols_grpc,
                transport
            );
            merge_option!(
                config,
                otlp_config_receiver_protocols_grpc_max_recv_msg_size_mib,
                receiver_protocols_grpc,
                max_recv_msg_size_mib
            );
        }

        // Metrics
        if let Some(metrics) = &otlp_config.metrics {
            merge_option_to_value!(config, otlp_config_metrics_enabled, metrics, enabled);
            merge_option_to_value!(
                config,
                otlp_config_metrics_resource_attributes_as_tags,
                metrics,
                resource_attributes_as_tags
            );
            merge_option_to_value!(
                config,
                otlp_config_metrics_instrumentation_scope_metadata_as_tags,
                metrics,
                instrumentation_scope_metadata_as_tags
            );
            merge_option!(
                config,
                otlp_config_metrics_tag_cardinality,
                metrics,
                tag_cardinality
            );
            merge_option!(config, otlp_config_metrics_delta_ttl, metrics, delta_ttl);
            if let Some(histograms) = &metrics.histograms {
                merge_option_to_value!(
                    config,
                    otlp_config_metrics_histograms_send_count_sum_metrics,
                    histograms,
                    send_count_sum_metrics
                );
                merge_option_to_value!(
                    config,
                    otlp_config_metrics_histograms_send_aggregation_metrics,
                    histograms,
                    send_aggregation_metrics
                );
                merge_option!(
                    config,
                    otlp_config_metrics_histograms_mode,
                    histograms,
                    mode
                );
            }
            if let Some(sums) = &metrics.sums {
                merge_option!(
                    config,
                    otlp_config_metrics_sums_cumulative_monotonic_mode,
                    sums,
                    cumulative_monotonic_mode
                );
                merge_option!(
                    config,
                    otlp_config_metrics_sums_initial_cumulativ_monotonic_value,
                    sums,
                    initial_cumulative_monotonic_value
                );
            }
            if let Some(summaries) = &metrics.summaries {
                merge_option!(config, otlp_config_metrics_summaries_mode, summaries, mode);
            }
        }

        // Logs
        if let Some(logs) = &otlp_config.logs {
            merge_option_to_value!(config, otlp_config_logs_enabled, logs, enabled);
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
#[allow(clippy::module_name_repetitions)]
pub struct YamlConfigSource {
    pub path: PathBuf,
}

impl<E: ConfigExtension> ConfigSource<E> for YamlConfigSource {
    fn load(&self, config: &mut Config<E>) -> Result<(), ConfigError> {
        let figment = Figment::new().merge(Yaml::file(self.path.clone()));

        match figment.extract::<YamlConfig>() {
            Ok(yaml_config) => merge_config(config, &yaml_config),
            Err(e) => {
                return Err(ConfigError::ParseError(format!(
                    "Failed to parse config from yaml file: {e}, using default config."
                )));
            }
        }

        // Extract extension fields via dual extraction
        match figment.extract::<E::Source>() {
            Ok(ext_source) => config.ext.merge_from(&ext_source),
            Err(e) => {
                tracing::warn!(
                    "Failed to parse extension config from yaml file: {e}, using default extension config."
                );
            }
        }

        Ok(())
    }
}

#[cfg_attr(coverage_nightly, coverage(off))] // Test modules skew coverage metrics
#[cfg(test)]
#[allow(clippy::result_large_err)]
mod tests {
    use std::path::Path;

    use crate::{log_level::LogLevel, processing_rule::Kind};

    use super::*;

    /// Comprehensive test: every field in the YAML set to the wrong type.
    /// The load MUST succeed, and every field must fall back to its default.
    ///
    /// When adding a new field to YamlConfig or any nested struct, add an entry
    /// here with the wrong type to ensure graceful deserialization is in place.
    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn test_all_yaml_fields_wrong_type_fallback_to_default() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            // Non-string fields are set to [1, 2, 3] (wrong type) to exercise
            // graceful fallback. String fields are set to valid non-default
            // values to prove they survive alongside broken non-string fields.
            jail.create_file(
                "datadog.yaml",
                r#"
# Basic fields — string fields get valid non-default values
site: "custom-site.example.com"
api_key: "test-api-key-12345"
org_uuid: "00000000-0000-0000-0000-000000000001"
log_level: [1, 2, 3]
flush_timeout: [1, 2, 3]
compression_level: [1, 2, 3]

# Proxy (nested)
proxy:
  https: "https://proxy.example.com"
  no_proxy: 12345

# Endpoints
dd_url: "https://custom-metrics.example.com"
http_protocol: "http1"
tls_cert_file: "/opt/ca-cert.pem"
skip_ssl_validation: [1, 2, 3]
additional_endpoints: [1, 2, 3]

# Unified Service Tagging
env: "test_env"
service: "test_service"
version: "v1.0.0"
tags: 12345

# Logs (nested)
logs_config:
  logs_dd_url: "https://custom-logs.example.com"
  processing_rules: 12345
  use_compression: [1, 2, 3]
  compression_level: [1, 2, 3]
  additional_endpoints: 12345

# APM (nested)
apm_config:
  apm_dd_url: "https://custom-apm.example.com"
  replace_tags: 12345
  obfuscation:
    http:
      remove_query_string: [1, 2, 3]
      remove_paths_with_digits: [1, 2, 3]
  compression_level: [1, 2, 3]
  features: 12345
  additional_endpoints: [1, 2, 3]

service_mapping: [1, 2, 3]
trace_aws_service_representation_enabled: [1, 2, 3]

# Trace Propagation
trace_propagation_style: [1, 2, 3]
trace_propagation_style_extract: [1, 2, 3]
trace_propagation_extract_first: [1, 2, 3]
trace_propagation_http_baggage_enabled: [1, 2, 3]

# Metrics (nested)
metrics_config:
  compression_level: [1, 2, 3]

# DogStatsD
dogstatsd_so_rcvbuf: [1, 2, 3]
dogstatsd_buffer_size: [1, 2, 3]
dogstatsd_queue_size: [1, 2, 3]

# OTLP (deeply nested) — string fields get valid values
otlp_config:
  receiver:
    protocols:
      http:
        endpoint: "0.0.0.0:4318"
      grpc:
        endpoint: "0.0.0.0:4317"
        transport: "tcp"
        max_recv_msg_size_mib: [1, 2, 3]
  traces:
    enabled: [1, 2, 3]
    span_name_as_resource_name: [1, 2, 3]
    span_name_remappings: [1, 2, 3]
    ignore_missing_datadog_fields: [1, 2, 3]
    probabilistic_sampler:
      sampling_percentage: [1, 2, 3]
  metrics:
    enabled: [1, 2, 3]
    resource_attributes_as_tags: [1, 2, 3]
    instrumentation_scope_metadata_as_tags: [1, 2, 3]
    tag_cardinality: "orchestrator"
    delta_ttl: [1, 2, 3]
    histograms:
      mode: "distributions"
      send_count_sum_metrics: [1, 2, 3]
      send_aggregation_metrics: [1, 2, 3]
    sums:
      cumulative_monotonic_mode: "to_delta"
      initial_cumulative_monotonic_value: "keep"
    summaries:
      mode: "noquantiles"
  logs:
    enabled: [1, 2, 3]
"#,
            )?;

            let mut config: Config = Config::default();
            let source = YamlConfigSource {
                path: PathBuf::from("datadog.yaml"),
            };
            // This MUST succeed — no single field should crash the whole config
            source
                .load(&mut config)
                .expect("load must not fail when fields have wrong types");

            // Build expected: string fields have their non-default values,
            // all non-string fields stay at defaults.
            let mut expected: Config = Config::default();
            expected.site = "custom-site.example.com".to_string();
            expected.api_key = "test-api-key-12345".to_string();
            expected.dd_org_uuid = "00000000-0000-0000-0000-000000000001".to_string();
            expected.dd_url = "https://custom-metrics.example.com".to_string();
            expected.logs_config_logs_dd_url = "https://custom-logs.example.com".to_string();
            expected.apm_dd_url = "https://custom-apm.example.com".to_string();
            // Option<String> fields
            expected.proxy_https = Some("https://proxy.example.com".to_string());
            expected.http_protocol = Some("http1".to_string());
            expected.tls_cert_file = Some("/opt/ca-cert.pem".to_string());
            expected.env = Some("test_env".to_string());
            expected.service = Some("test_service".to_string());
            expected.version = Some("v1.0.0".to_string());
            expected.otlp_config_receiver_protocols_http_endpoint =
                Some("0.0.0.0:4318".to_string());
            expected.otlp_config_receiver_protocols_grpc_endpoint =
                Some("0.0.0.0:4317".to_string());
            expected.otlp_config_receiver_protocols_grpc_transport = Some("tcp".to_string());
            expected.otlp_config_metrics_tag_cardinality = Some("orchestrator".to_string());
            expected.otlp_config_metrics_histograms_mode = Some("distributions".to_string());
            expected.otlp_config_metrics_sums_cumulative_monotonic_mode =
                Some("to_delta".to_string());
            expected.otlp_config_metrics_sums_initial_cumulativ_monotonic_value =
                Some("keep".to_string());
            expected.otlp_config_metrics_summaries_mode = Some("noquantiles".to_string());

            assert_eq!(config, expected);
            Ok(())
        });
    }

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
tls_cert_file: "/opt/ca-cert.pem"
skip_ssl_validation: true

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
    - "enable_otlp_compute_top_level_by_span_kind"
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
trace_propagation_style_extract: "tracecontext"
trace_propagation_extract_first: true
trace_propagation_http_baggage_enabled: true
trace_aws_service_representation_enabled: true

metrics_config:
  compression_level: 3

dogstatsd_so_rcvbuf: 1048576
dogstatsd_buffer_size: 65507
dogstatsd_queue_size: 2048

# OTLP
otlp_config:
  receiver:
    protocols:
      http:
        endpoint: "http://localhost:4318"
      grpc:
        endpoint: "http://localhost:4317"
        transport: "tcp"
        max_recv_msg_size_mib: 4
  traces:
    enabled: false
    span_name_as_resource_name: true
    span_name_remappings:
      "old-span": "new-span"
    ignore_missing_datadog_fields: true
    probabilistic_sampler:
      sampling_percentage: 50
  metrics:
    enabled: true
    resource_attributes_as_tags: true
    instrumentation_scope_metadata_as_tags: true
    tag_cardinality: "low"
    delta_ttl: 3600
    histograms:
      mode: "counters"
      send_count_sum_metrics: true
      send_aggregation_metrics: true
    sums:
      cumulative_monotonic_mode: "to_delta"
      initial_cumulative_monotonic_value: "auto"
    summaries:
      mode: "quantiles"
  logs:
    enabled: true
"#,
            )?;

            let mut config: Config = Config::default();
            let yaml_config_source = YamlConfigSource {
                path: Path::new("datadog.yaml").to_path_buf(),
            };
            yaml_config_source
                .load(&mut config)
                .expect("Failed to load config");

            let expected_config = Config {
                site: "test-site".to_string(),
                api_key: "test-api-key".to_string(),
                dd_org_uuid: String::default(),
                log_level: LogLevel::Debug,
                flush_timeout: 42,
                compression_level: 4,
                proxy_https: Some("https://proxy.example.com".to_string()),
                proxy_no_proxy: vec!["localhost".to_string(), "127.0.0.1".to_string()],
                http_protocol: Some("http1".to_string()),
                tls_cert_file: Some("/opt/ca-cert.pem".to_string()),
                skip_ssl_validation: true,
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
                apm_features: vec![
                    "enable_otlp_compute_top_level_by_span_kind".to_string(),
                    "enable_stats_by_span_kind".to_string(),
                ],
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
                trace_propagation_style_extract: vec![TracePropagationStyle::TraceContext],
                trace_propagation_extract_first: true,
                trace_propagation_http_baggage_enabled: true,
                trace_aws_service_representation_enabled: true,
                metrics_config_compression_level: 3,
                otlp_config_traces_enabled: false,
                otlp_config_traces_span_name_as_resource_name: true,
                otlp_config_traces_span_name_remappings: HashMap::from([(
                    "old-span".to_string(),
                    "new-span".to_string(),
                )]),
                otlp_config_ignore_missing_datadog_fields: true,
                otlp_config_receiver_protocols_http_endpoint: Some(
                    "http://localhost:4318".to_string(),
                ),
                otlp_config_receiver_protocols_grpc_endpoint: Some(
                    "http://localhost:4317".to_string(),
                ),
                otlp_config_receiver_protocols_grpc_transport: Some("tcp".to_string()),
                otlp_config_receiver_protocols_grpc_max_recv_msg_size_mib: Some(4),
                otlp_config_metrics_enabled: true,
                otlp_config_metrics_resource_attributes_as_tags: true,
                otlp_config_metrics_instrumentation_scope_metadata_as_tags: true,
                otlp_config_metrics_tag_cardinality: Some("low".to_string()),
                otlp_config_metrics_delta_ttl: Some(3600),
                otlp_config_metrics_histograms_mode: Some("counters".to_string()),
                otlp_config_metrics_histograms_send_count_sum_metrics: true,
                otlp_config_metrics_histograms_send_aggregation_metrics: true,
                otlp_config_metrics_sums_cumulative_monotonic_mode: Some("to_delta".to_string()),
                otlp_config_metrics_sums_initial_cumulativ_monotonic_value: Some(
                    "auto".to_string(),
                ),
                otlp_config_metrics_summaries_mode: Some("quantiles".to_string()),
                otlp_config_traces_probabilistic_sampler_sampling_percentage: Some(50),
                otlp_config_logs_enabled: true,
                apm_filter_tags_require: None,
                apm_filter_tags_reject: None,
                apm_filter_tags_regex_require: None,
                apm_filter_tags_regex_reject: None,
                statsd_metric_namespace: None,
                dogstatsd_so_rcvbuf: Some(1_048_576),
                dogstatsd_buffer_size: Some(65507),
                dogstatsd_queue_size: Some(2048),
                ext: crate::NoExtension,
            };

            // Assert that
            assert_eq!(config, expected_config);

            Ok(())
        });
    }

    #[test]
    fn test_yaml_dogstatsd_config() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file(
                "datadog.yaml",
                r"
dogstatsd_so_rcvbuf: 524288
dogstatsd_buffer_size: 16384
dogstatsd_queue_size: 512
",
            )?;
            let mut config: Config = Config::default();
            let yaml_config_source = YamlConfigSource {
                path: Path::new("datadog.yaml").to_path_buf(),
            };
            yaml_config_source
                .load(&mut config)
                .expect("Failed to load config");

            assert_eq!(config.dogstatsd_so_rcvbuf, Some(524_288));
            assert_eq!(config.dogstatsd_buffer_size, Some(16384));
            assert_eq!(config.dogstatsd_queue_size, Some(512));
            Ok(())
        });
    }

    #[test]
    fn test_yaml_dogstatsd_config_defaults_to_none() {
        figment::Jail::expect_with(|jail| {
            jail.clear_env();
            jail.create_file("datadog.yaml", "")?;
            let mut config: Config = Config::default();
            let yaml_config_source = YamlConfigSource {
                path: Path::new("datadog.yaml").to_path_buf(),
            };
            yaml_config_source
                .load(&mut config)
                .expect("Failed to load config");

            assert_eq!(config.dogstatsd_so_rcvbuf, None);
            assert_eq!(config.dogstatsd_buffer_size, None);
            assert_eq!(config.dogstatsd_queue_size, None);
            Ok(())
        });
    }
}
