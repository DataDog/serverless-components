// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! The trace receiver and flusher, configured for serverless-init. The configuration comes
//! from the environment directly, with no cloud environment check, so the receiver also runs
//! on platforms that the trace agent does not recognize.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use datadog_agent_trace_sampler::ErrorSamplerConfig;
use datadog_serverless_core::env::parse_bool;
use datadog_trace_agent::aggregator::TraceAggregator;
use datadog_trace_agent::config::{AgentPayloadFields, Config, InstallSignature, SpanTags, Tags};
use datadog_trace_agent::env_verifier::EnvVerifier;
use datadog_trace_agent::mini_agent::MiniAgent;
use datadog_trace_agent::peer_tags::peer_tag_keys;
use datadog_trace_agent::proxy_flusher::ProxyFlusher;
use datadog_trace_agent::stats_concentrator_service::StatsConcentratorService;
use datadog_trace_agent::stats_flusher::ServerlessStatsFlusher;
use datadog_trace_agent::stats_processor::ServerlessStatsProcessor;
use datadog_trace_agent::trace_flusher::{ServerlessTraceFlusher, TraceFlusher};
use datadog_trace_agent::trace_processor::ServerlessTraceProcessor;
use libdd_common::{Endpoint, parse_uri};
use libdd_trace_normalization::normalize_utils::normalize_tag_cloned;
use libdd_trace_obfuscation::obfuscation_config::ObfuscationConfig;
use libdd_trace_utils::config_utils::{
    trace_intake_url, trace_intake_url_prefixed, trace_stats_url, trace_stats_url_prefixed,
};
use libdd_trace_utils::trace_utils::{EnvironmentType, MiniAgentMetadata};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::{platform, settings};

const DEFAULT_RECEIVER_PORT: u16 = 8126;
const DEFAULT_DOGSTATSD_PORT: u16 = 8125;
const DATA_STREAMS_ROUTE: &str = "/api/v0.1/pipeline_stats";
const PROFILING_ROUTE: &str = "/api/v2/profile";
/// The Go trace agent's default target of its samplers, in traces per second.
const DEFAULT_TPS: f64 = 10.0;

/// The trace agent settings read from the environment.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceAgentSettings {
    /// `DD_API_KEY`, else the deprecated `DD_APM_API_KEY`, without surrounding whitespace.
    pub api_key: String,
    /// `DD_SITE`.
    pub site: String,
    /// `DD_APM_RECEIVER_PORT`.
    pub receiver_port: u16,
    /// `DD_APM_NON_LOCAL_TRAFFIC`. Sidecar mode sets it to true when it is unset.
    pub non_local_traffic: bool,
    /// `DD_DOGSTATSD_PORT`, reported by `/info`.
    pub dogstatsd_port: u16,
    /// From the `DD_APM_DD_URL` prefix, else from `DD_SITE`.
    pub trace_intake_url: String,
    pub stats_intake_url: String,
    pub data_streams_intake_url: String,
    /// `DD_APM_PROFILING_DD_URL`, a full URL as in the Go agent, else from `DD_SITE`.
    pub profiling_intake_url: String,
    /// The trace agent's default env, from [`default_env`].
    pub env: String,
    /// `DD_TAGS`.
    pub tags: Option<String>,
    /// `DD_PROXY_HTTPS`, else `HTTPS_PROXY`.
    pub proxy_url: Option<String>,
    /// The first of `DD_APM_TARGET_TPS`, `DD_APM_MAX_TPS` and `DD_MAX_TPS` that is set.
    pub target_tps: f64,
    /// `DD_APM_ERROR_TPS`.
    pub error_tps: f64,
    /// `DD_SERVERLESS_INIT_DISABLE_TRACE_STATS`.
    pub trace_stats_disabled: bool,
    /// `DD_SERVERLESS_INIT_ENABLE_BACKEND_TRACE_STATS`, which sets `_dd.compute_stats:1` on spans.
    pub backend_trace_stats: bool,
}

/// Why the trace agent does not start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceAgentDisabled {
    /// `DD_APM_ENABLED` is false.
    ApmDisabled,
    /// Neither `DD_API_KEY` nor `DD_APM_API_KEY` is set to a non-empty value.
    MissingApiKey,
}

/// Reads the trace agent settings through `lookup`, which returns an environment variable's
/// value.
pub fn settings_from_environment(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<TraceAgentSettings, TraceAgentDisabled> {
    let bool_setting = |name: &str, default: bool| {
        parse_bool(lookup(name).as_deref(), default).unwrap_or_else(|error| {
            warn!("{name}: {error}, so the default {default} is used");
            default
        })
    };
    let port_setting = |name: &str, default: u16| {
        lookup(name)
            .and_then(|port| port.parse::<u16>().ok())
            .unwrap_or(default)
    };

    if !bool_setting("DD_APM_ENABLED", true) {
        return Err(TraceAgentDisabled::ApmDisabled);
    }
    let api_key = settings::api_key(&lookup).ok_or(TraceAgentDisabled::MissingApiKey)?;

    let site = settings::site(&lookup);
    let intake_site = settings::intake_site(&lookup);
    let (trace_intake_url, stats_intake_url, data_streams_intake_url) =
        match lookup("DD_APM_DD_URL") {
            Some(prefix) => (
                trace_intake_url_prefixed(&prefix),
                trace_stats_url_prefixed(&prefix),
                format!("{prefix}{DATA_STREAMS_ROUTE}"),
            ),
            None => (
                trace_intake_url(&intake_site),
                trace_stats_url(&intake_site),
                format!("https://trace.agent.{intake_site}{DATA_STREAMS_ROUTE}"),
            ),
        };
    let profiling_intake_url = lookup("DD_APM_PROFILING_DD_URL")
        .unwrap_or_else(|| format!("https://intake.profile.{intake_site}{PROFILING_ROUTE}"));

    Ok(TraceAgentSettings {
        api_key,
        receiver_port: port_setting("DD_APM_RECEIVER_PORT", DEFAULT_RECEIVER_PORT),
        non_local_traffic: bool_setting("DD_APM_NON_LOCAL_TRAFFIC", false),
        dogstatsd_port: port_setting("DD_DOGSTATSD_PORT", DEFAULT_DOGSTATSD_PORT),
        trace_intake_url,
        stats_intake_url,
        data_streams_intake_url,
        profiling_intake_url,
        env: default_env(&lookup),
        tags: lookup("DD_TAGS"),
        proxy_url: settings::proxy_url(&lookup),
        target_tps: tps_setting(
            &lookup,
            &["DD_APM_TARGET_TPS", "DD_APM_MAX_TPS", "DD_MAX_TPS"],
        ),
        error_tps: tps_setting(&lookup, &["DD_APM_ERROR_TPS"]),
        trace_stats_disabled: bool_setting("DD_SERVERLESS_INIT_DISABLE_TRACE_STATS", false),
        backend_trace_stats: bool_setting("DD_SERVERLESS_INIT_ENABLE_BACKEND_TRACE_STATS", false),
        site,
    })
}

/// The Go trace agent's default env: the first of `DD_APM_ENV`, `DD_ENV` and the first `env:`
/// entry of `DD_TAGS` and `DD_EXTRA_TAGS`, which are split on whitespace, else `none`. The value
/// is normalized as a tag value. Empty variables count as unset.
pub fn default_env(lookup: impl Fn(&str) -> Option<String>) -> String {
    let set = |name: &str| lookup(name).filter(|value| !value.is_empty());
    let env = set("DD_APM_ENV").or_else(|| set("DD_ENV")).or_else(|| {
        ["DD_TAGS", "DD_EXTRA_TAGS"]
            .iter()
            .filter_map(|name| set(name))
            .find_map(|tags| {
                tags.split_whitespace()
                    .find_map(|tag| tag.strip_prefix("env:").map(str::to_string))
            })
    });
    match env.filter(|env| !env.is_empty()) {
        Some(env) => normalize_tag_value(&env),
        None => "none".to_string(),
    }
}

/// Go's `NormalizeTagValue`. It differs from `normalize_tag` only at the first character, which
/// it keeps when it is a digit, `.`, `/` or `-`, and turns into `_` when it is not allowed. A
/// value that starts with something other than a letter gets a letter in front, which makes
/// `normalize_tag` treat it as the rest of a tag, and loses it afterwards.
fn normalize_tag_value(value: &str) -> String {
    if value.chars().next().is_some_and(char::is_alphabetic) {
        return normalize_tag_cloned(value);
    }
    let normalized = normalize_tag_cloned(&format!("a{value}"));
    normalized.get(1..).unwrap_or_default().to_string()
}

/// The first of `names` that is set, read as a number. A value that is not a number reads as 0,
/// as in the Go agent.
fn tps_setting(lookup: impl Fn(&str) -> Option<String>, names: &[&str]) -> f64 {
    let Some((name, value)) = names
        .iter()
        .find_map(|name| Some((name, lookup(name).filter(|value| !value.is_empty())?)))
    else {
        return DEFAULT_TPS;
    };
    value.parse::<f64>().unwrap_or_else(|_| {
        warn!("{name}: {value:?} is not a number, so 0 is used");
        0.0
    })
}

/// Go's install signature: the one that `DD_INSTRUMENTATION_INSTALL_ID`,
/// `DD_INSTRUMENTATION_INSTALL_TYPE` and `DD_INSTRUMENTATION_INSTALL_TIME` configure when any of
/// them is set to a non-empty value, else the one in the file at `path`, else a new one, which is used only once it is
/// written there. `None` when the signature has no ID, as Go then sets no install tags.
pub fn install_signature(
    lookup: impl Fn(&str) -> Option<String>,
    path: &Path,
) -> Option<InstallSignature> {
    let names = [
        "DD_INSTRUMENTATION_INSTALL_ID",
        "DD_INSTRUMENTATION_INSTALL_TYPE",
        "DD_INSTRUMENTATION_INSTALL_TIME",
    ];
    let set = |name: &str| lookup(name).filter(|value| !value.is_empty());
    let signature = if names.iter().any(|name| set(name).is_some()) {
        InstallSignature {
            id: set(names[0]).unwrap_or_default(),
            install_type: set(names[1]).unwrap_or_default(),
            time: set(names[2])
                .and_then(|time| time.parse().ok())
                .unwrap_or(0),
        }
    } else if let Some(signature) = read_install_signature(path) {
        signature
    } else {
        let install_type = match (
            set("DOCKER_DD_AGENT").is_some(),
            set("DD_APM_ENABLED").is_some(),
        ) {
            (false, _) => "manual",
            (true, false) => "docker_manual",
            (true, true) => "docker_single_step",
        };
        let signature = InstallSignature {
            id: uuid::Uuid::new_v4().to_string(),
            install_type: install_type.to_string(),
            time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs() as i64),
        };
        write_install_signature(path, &signature).ok()?;
        signature
    };
    (!signature.id.is_empty()).then_some(signature)
}

/// Writes the signature as Go does: its fields in Go's order, in a file created with mode 0644.
fn write_install_signature(path: &Path, signature: &InstallSignature) -> std::io::Result<()> {
    let contents = format!(
        r#"{{"install_id":{},"install_type":{},"install_time":{}}}"#,
        serde_json::Value::from(signature.id.as_str()),
        serde_json::Value::from(signature.install_type.as_str()),
        signature.time,
    );
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o644);
    std::io::Write::write_all(&mut options.open(path)?, contents.as_bytes())
}

/// The signature in an `install.json` file. Missing fields are empty, as in Go.
fn read_install_signature(path: &Path) -> Option<InstallSignature> {
    let contents: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let text = |key: &str| contents.get(key).and_then(|value| value.as_str());
    Some(InstallSignature {
        id: text("install_id").unwrap_or_default().to_string(),
        install_type: text("install_type").unwrap_or_default().to_string(),
        time: contents
            .get("install_time")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0),
    })
}

/// Builds the trace agent configuration, which adds `span_tags` to every span in place of the
/// environment type's root span origin. The obfuscation settings come from the environment. The
/// environment type `AzureSpringApp` adds no platform tags to spans, but the environment check
/// still receives it, and the profiling proxy adds `_dd.origin:unknown` from it to every profile.
pub fn build_config(
    settings: TraceAgentSettings,
    mut span_tags: SpanTags,
    install_signature: Option<InstallSignature>,
) -> Result<Config, Box<dyn std::error::Error>> {
    if settings.backend_trace_stats {
        span_tags
            .tags
            .insert("_dd.compute_stats".to_string(), "1".to_string());
    }
    let endpoint = |url: &str| -> Result<Endpoint, Box<dyn std::error::Error>> {
        Ok(Endpoint {
            url: parse_uri(url)?,
            api_key: Some(settings.api_key.clone().into()),
            ..Default::default()
        })
    };
    Ok(Config {
        dd_site: settings.site.clone(),
        dd_apm_receiver_port: settings.receiver_port,
        dd_apm_non_local_traffic: settings.non_local_traffic,
        dd_dogstatsd_port: settings.dogstatsd_port,
        env_type: EnvironmentType::AzureSpringApp,
        app_name: None,
        max_request_content_length: 10 * 1024 * 1024,
        obfuscation_config: ObfuscationConfig::new_from_env()
            .map_err(|error| format!("Error creating the obfuscation configuration: {error}"))?,
        os: std::env::consts::OS.to_string(),
        tags: settings
            .tags
            .as_deref()
            .map(Tags::from_env_string)
            .unwrap_or_default(),
        stats_flush_interval_secs: 3,
        trace_flush_interval_secs: 3,
        trace_intake: endpoint(&settings.trace_intake_url)?,
        trace_stats_intake: endpoint(&settings.stats_intake_url)?,
        dsm_intake: endpoint(&settings.data_streams_intake_url)?,
        profiling_intake: endpoint(&settings.profiling_intake_url)?,
        proxy_request_timeout_secs: 30,
        proxy_request_max_retries: 3,
        proxy_request_retry_backoff_base_ms: 100,
        verify_env_timeout_ms: 100,
        enqueue_permit_timeout_secs: 2,
        proxy_url: settings.proxy_url.clone(),
        env: settings.env.clone(),
        peer_tags: peer_tag_keys()?,
        experimental_features_enabled: false,
        additional_metric_tags: vec![],
        additional_metric_tags_cardinality_limit: None,
        agent_stats_computation_enabled: !settings.trace_stats_disabled,
        error_sampler: ErrorSamplerConfig {
            target_tps: settings.error_tps,
            ..ErrorSamplerConfig::default()
        },
        error_rare_sample_tracer_drop: false,
        span_tags: Some(span_tags),
        agent_payload_fields: Some(AgentPayloadFields {
            agent_version: platform::VERSION.to_string(),
            target_tps: settings.target_tps,
            install_signature,
        }),
    })
}

/// Skips the cloud environment check and reports no platform metadata.
struct NoEnvironmentCheck;

#[async_trait]
impl EnvVerifier for NoEnvironmentCheck {
    async fn verify_environment(
        &self,
        _verify_env_timeout: u64,
        _env_type: &EnvironmentType,
        _os: &str,
    ) -> MiniAgentMetadata {
        MiniAgentMetadata::default()
    }
}

/// A running trace agent, kept until shutdown.
pub struct TraceAgent {
    shutdown_sender: watch::Sender<bool>,
    mini_agent: JoinHandle<()>,
    trace_flusher: Arc<ServerlessTraceFlusher>,
}

impl TraceAgent {
    /// Starts the receiver and the flushers from the environment, adding `span_tags` to every
    /// span. Returns `None`, after logging why, when the trace agent is disabled or its
    /// configuration is invalid.
    pub fn start(span_tags: SpanTags) -> Option<Self> {
        let settings = match settings_from_environment(|name| std::env::var(name).ok()) {
            Ok(settings) => settings,
            Err(TraceAgentDisabled::ApmDisabled) => {
                info!("Trace agent is disabled");
                return None;
            }
            Err(TraceAgentDisabled::MissingApiKey) => {
                warn!("DD_API_KEY is not set; trace collection is disabled");
                return None;
            }
        };
        let install_signature =
            install_signature(|name| std::env::var(name).ok(), Path::new("install.json"));
        let config = match build_config(settings, span_tags, install_signature) {
            Ok(config) => Arc::new(config),
            Err(error) => {
                error!("Error creating the trace agent configuration: {error}");
                return None;
            }
        };

        let trace_flusher = Arc::new(ServerlessTraceFlusher::new(
            Arc::new(Mutex::new(TraceAggregator::default())),
            config.clone(),
        ));
        let (stats_service, stats_handle) = StatsConcentratorService::new(config.clone());
        let stats_service = tokio::spawn(stats_service.run());
        let mini_agent = MiniAgent {
            config: config.clone(),
            env_verifier: Arc::new(NoEnvironmentCheck),
            trace_processor: Arc::new(ServerlessTraceProcessor::new(
                Some(stats_handle.clone()),
                &config,
            )),
            trace_flusher: trace_flusher.clone(),
            stats_processor: Arc::new(ServerlessStatsProcessor {}),
            stats_flusher: Arc::new(ServerlessStatsFlusher {
                stats_concentrator: Some(stats_handle),
            }),
            proxy_flusher: Arc::new(ProxyFlusher::new(config.clone())),
        };
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let mini_agent = tokio::spawn(async move {
            if let Err(error) = mini_agent
                .start_mini_agent(shutdown_receiver, Some(stats_service))
                .await
            {
                error!("Error when running the trace agent: {error}");
            }
        });
        Some(Self {
            shutdown_sender,
            mini_agent,
            trace_flusher,
        })
    }

    /// Stops the receiver. Once its in-flight requests finish, the receiver flushes the
    /// buffered traces and stats. A last flush then sends the traces left in the aggregator when
    /// the mini agent stopped early on an error, and sends nothing otherwise.
    pub async fn flush_at_shutdown(self) {
        let _ = self.shutdown_sender.send(true);
        if let Err(error) = self.mini_agent.await {
            error!("The trace agent task failed: {error}");
        }
        self.trace_flusher.flush().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_for(variables: &[(&str, &str)]) -> Result<TraceAgentSettings, TraceAgentDisabled> {
        settings_from_environment(|name| {
            variables
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    fn default_settings() -> TraceAgentSettings {
        TraceAgentSettings {
            api_key: "key".to_string(),
            site: "datadoghq.com".to_string(),
            receiver_port: 8126,
            non_local_traffic: false,
            dogstatsd_port: 8125,
            trace_intake_url: "https://trace.agent.datadoghq.com./api/v0.2/traces".to_string(),
            stats_intake_url: "https://trace.agent.datadoghq.com./api/v0.2/stats".to_string(),
            data_streams_intake_url: "https://trace.agent.datadoghq.com./api/v0.1/pipeline_stats"
                .to_string(),
            profiling_intake_url: "https://intake.profile.datadoghq.com./api/v2/profile"
                .to_string(),
            env: "none".to_string(),
            tags: None,
            proxy_url: None,
            target_tps: 10.0,
            error_tps: 10.0,
            trace_stats_disabled: false,
            backend_trace_stats: false,
        }
    }

    #[test]
    fn settings_from_environment_table() {
        type Case = (
            &'static [(&'static str, &'static str)],
            Result<TraceAgentSettings, TraceAgentDisabled>,
        );
        let cases: Vec<Case> = vec![
            (&[("DD_API_KEY", "key")], Ok(default_settings())),
            (&[], Err(TraceAgentDisabled::MissingApiKey)),
            (
                &[("DD_API_KEY", "  ")],
                Err(TraceAgentDisabled::MissingApiKey),
            ),
            (&[("DD_APM_API_KEY", " key ")], Ok(default_settings())),
            (
                &[("DD_API_KEY", "key"), ("DD_APM_ENABLED", "false")],
                Err(TraceAgentDisabled::ApmDisabled),
            ),
            (
                &[("DD_API_KEY", "key"), ("DD_APM_ENABLED", "not a bool")],
                Ok(default_settings()),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_APM_RECEIVER_PORT", "9126"),
                    ("DD_APM_NON_LOCAL_TRAFFIC", "true"),
                    ("DD_DOGSTATSD_PORT", "9125"),
                ],
                Ok(TraceAgentSettings {
                    receiver_port: 9126,
                    non_local_traffic: true,
                    dogstatsd_port: 9125,
                    ..default_settings()
                }),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_APM_RECEIVER_PORT", "not a port"),
                    ("DD_APM_NON_LOCAL_TRAFFIC", "yes"),
                ],
                Ok(default_settings()),
            ),
            (
                &[("DD_API_KEY", "key"), ("DD_SITE", "datadoghq.eu")],
                Ok(TraceAgentSettings {
                    site: "datadoghq.eu".to_string(),
                    trace_intake_url: "https://trace.agent.datadoghq.eu./api/v0.2/traces"
                        .to_string(),
                    stats_intake_url: "https://trace.agent.datadoghq.eu./api/v0.2/stats"
                        .to_string(),
                    data_streams_intake_url:
                        "https://trace.agent.datadoghq.eu./api/v0.1/pipeline_stats".to_string(),
                    profiling_intake_url: "https://intake.profile.datadoghq.eu./api/v2/profile"
                        .to_string(),
                    ..default_settings()
                }),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_SITE", "datadoghq.eu"),
                    ("DD_APM_DD_URL", "http://fakeintake:80"),
                    (
                        "DD_APM_PROFILING_DD_URL",
                        "http://fakeintake:80/api/v2/profile",
                    ),
                ],
                Ok(TraceAgentSettings {
                    site: "datadoghq.eu".to_string(),
                    trace_intake_url: "http://fakeintake:80/api/v0.2/traces".to_string(),
                    stats_intake_url: "http://fakeintake:80/api/v0.2/stats".to_string(),
                    data_streams_intake_url: "http://fakeintake:80/api/v0.1/pipeline_stats"
                        .to_string(),
                    profiling_intake_url: "http://fakeintake:80/api/v2/profile".to_string(),
                    ..default_settings()
                }),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_ENV", "prod"),
                    ("DD_TAGS", "team:serverless"),
                    ("HTTPS_PROXY", "http://proxy:3128"),
                ],
                Ok(TraceAgentSettings {
                    env: "prod".to_string(),
                    tags: Some("team:serverless".to_string()),
                    proxy_url: Some("http://proxy:3128".to_string()),
                    ..default_settings()
                }),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_PROXY_HTTPS", "http://datadog-proxy:3128"),
                    ("HTTPS_PROXY", "http://proxy:3128"),
                ],
                Ok(TraceAgentSettings {
                    proxy_url: Some("http://datadog-proxy:3128".to_string()),
                    ..default_settings()
                }),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_APM_TARGET_TPS", "5"),
                    ("DD_APM_MAX_TPS", "6"),
                    ("DD_APM_ERROR_TPS", "7"),
                ],
                Ok(TraceAgentSettings {
                    target_tps: 5.0,
                    error_tps: 7.0,
                    ..default_settings()
                }),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_APM_MAX_TPS", "6"),
                    ("DD_MAX_TPS", "8"),
                ],
                Ok(TraceAgentSettings {
                    target_tps: 6.0,
                    ..default_settings()
                }),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_APM_TARGET_TPS", "not a number"),
                    ("DD_MAX_TPS", "8"),
                    ("DD_APM_ERROR_TPS", "not a number"),
                ],
                Ok(TraceAgentSettings {
                    target_tps: 0.0,
                    error_tps: 0.0,
                    ..default_settings()
                }),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_SERVERLESS_INIT_DISABLE_TRACE_STATS", "true"),
                    ("DD_SERVERLESS_INIT_ENABLE_BACKEND_TRACE_STATS", "1"),
                ],
                Ok(TraceAgentSettings {
                    trace_stats_disabled: true,
                    backend_trace_stats: true,
                    ..default_settings()
                }),
            ),
        ];
        for (variables, expected) in cases {
            assert_eq!(settings_for(variables), expected, "variables {variables:?}");
        }
    }

    /// An env of 200 characters, the most that Go keeps.
    const LONG_ENV: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

    #[test]
    fn default_env_table() {
        let cases: [(&[(&str, &str)], &str); 12] = [
            (&[], "none"),
            (
                &[
                    ("DD_APM_ENV", "staging"),
                    ("DD_ENV", "prod"),
                    ("DD_TAGS", "env:dev"),
                ],
                "staging",
            ),
            (&[("DD_ENV", "prod"), ("DD_TAGS", "env:dev")], "prod"),
            (&[("DD_APM_ENV", ""), ("DD_ENV", "prod")], "prod"),
            (&[("DD_TAGS", "team:a env:dev env:qa")], "dev"),
            (
                &[("DD_TAGS", "team:a"), ("DD_EXTRA_TAGS", "env:dev")],
                "dev",
            ),
            // `DD_TAGS` is split on whitespace only.
            (&[("DD_TAGS", "env:dev,team:a")], "dev_team:a"),
            (&[("DD_ENV", "Prod Env!")], "prod_env"),
            // An empty `env:` entry stops the search, and the default applies.
            (&[("DD_TAGS", "team:a env: env:dev")], "none"),
            (&[("DD_ENV", "1-prod")], "1-prod"),
            (&[("DD_ENV", "!prod")], "_prod"),
            (&[("DD_ENV", LONG_ENV)], LONG_ENV),
        ];
        for (variables, expected) in cases {
            let env = default_env(|name| {
                variables
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            });
            assert_eq!(env, expected, "variables {variables:?}");
        }
    }

    /// When the mini agent has already stopped, for example after a task failed, the traces
    /// still in the aggregator reach the intake at shutdown.
    #[tokio::test(flavor = "multi_thread")]
    async fn flush_at_shutdown_flushes_traces_left_after_the_mini_agent_stopped() {
        use datadog_mock_intake::MockIntake;
        use libdd_trace_protobuf::pb;
        use libdd_trace_utils::trace_utils::{SendData, TracerHeaderTags};
        use libdd_trace_utils::tracer_payload::TracerPayloadCollection;

        let intake = MockIntake::start().await;
        let config = Arc::new(
            build_config(
                TraceAgentSettings {
                    trace_intake_url: intake.traces_url(),
                    ..default_settings()
                },
                SpanTags::default(),
                None,
            )
            .unwrap(),
        );
        let span = pb::Span {
            service: "parity-app".to_string(),
            resource: "left in the aggregator".to_string(),
            trace_id: 1,
            span_id: 1,
            ..Default::default()
        };
        let tracer_payload = pb::TracerPayload {
            chunks: vec![pb::TraceChunk {
                spans: vec![span],
                ..Default::default()
            }],
            ..Default::default()
        };
        let aggregator = Arc::new(Mutex::new(TraceAggregator::default()));
        aggregator.lock().await.add(SendData::new(
            1,
            TracerPayloadCollection::V07(vec![tracer_payload]),
            TracerHeaderTags::default(),
            &config.trace_intake,
        ));
        let (shutdown_sender, _shutdown_receiver) = watch::channel(false);
        let agent = TraceAgent {
            shutdown_sender,
            mini_agent: tokio::spawn(async {}),
            trace_flusher: Arc::new(ServerlessTraceFlusher::new(aggregator, config)),
        };

        agent.flush_at_shutdown().await;

        let resources: Vec<String> = intake
            .trace_payloads()
            .iter()
            .flat_map(|payload| &payload.tracer_payloads)
            .flat_map(|tracer_payload| &tracer_payload.chunks)
            .flat_map(|chunk| &chunk.spans)
            .map(|span| span.resource.clone())
            .collect();
        assert_eq!(resources, vec!["left in the aggregator".to_string()]);
    }

    #[test]
    fn config_uses_settings_without_a_cloud_environment() {
        let settings = TraceAgentSettings {
            receiver_port: 9126,
            non_local_traffic: true,
            trace_intake_url: "http://fakeintake:80/api/v0.2/traces".to_string(),
            trace_stats_disabled: true,
            backend_trace_stats: true,
            ..default_settings()
        };
        let span_tags = SpanTags {
            origin: "local".to_string(),
            tags: [("local".to_string(), "true".to_string())].into(),
        };
        let signature = InstallSignature {
            id: "install-1".to_string(),
            install_type: "manual".to_string(),
            time: 1_700_000_000,
        };
        let config = build_config(settings, span_tags, Some(signature.clone())).unwrap();
        assert_eq!(config.dd_apm_receiver_port, 9126);
        assert!(config.dd_apm_non_local_traffic);
        assert_eq!(
            config.trace_intake.url.to_string(),
            "http://fakeintake:80/api/v0.2/traces"
        );
        assert_eq!(config.trace_intake.api_key.as_deref(), Some("key"));
        assert_eq!(config.app_name, None);
        assert!(!config.agent_stats_computation_enabled);
        assert_eq!(
            config.span_tags,
            Some(SpanTags {
                origin: "local".to_string(),
                tags: [
                    ("_dd.compute_stats".to_string(), "1".to_string()),
                    ("local".to_string(), "true".to_string()),
                ]
                .into(),
            })
        );
        assert_eq!(config.error_sampler.target_tps, 10.0);
        assert_eq!(
            config.agent_payload_fields,
            Some(AgentPayloadFields {
                agent_version: "0.1.0".to_string(),
                target_tps: 10.0,
                install_signature: Some(signature),
            })
        );
    }

    /// Each case gives the environment and the contents of `install.json` before the call, then
    /// the expected signature, where `None` for the ID and time means a new UUID and a current
    /// time, and whether `install.json` holds the new signature afterwards.
    #[test]
    fn install_signature_table() {
        type Expected = Option<(Option<&'static str>, &'static str, Option<i64>)>;
        type Case = (
            &'static [(&'static str, &'static str)],
            Option<&'static str>,
            Expected,
        );
        let cases: [Case; 9] = [
            // Empty variables count as unset.
            (
                &[
                    ("DD_INSTRUMENTATION_INSTALL_ID", ""),
                    ("DD_INSTRUMENTATION_INSTALL_TIME", ""),
                ],
                Some(r#"{"install_id":"from-file","install_type":"manual","install_time":7}"#),
                Some((Some("from-file"), "manual", Some(7))),
            ),
            (
                &[("DD_INSTRUMENTATION_INSTALL_ID", "from-env")],
                Some(r#"{"install_id":"from-file"}"#),
                Some((Some("from-env"), "", Some(0))),
            ),
            (
                &[
                    ("DD_INSTRUMENTATION_INSTALL_ID", "from-env"),
                    ("DD_INSTRUMENTATION_INSTALL_TYPE", "k8s_single_step"),
                    ("DD_INSTRUMENTATION_INSTALL_TIME", "5"),
                ],
                None,
                Some((Some("from-env"), "k8s_single_step", Some(5))),
            ),
            // A configured signature without an ID sets no tags.
            (&[("DD_INSTRUMENTATION_INSTALL_TIME", "5")], None, None),
            (
                &[],
                Some(r#"{"install_id":"from-file","install_type":"manual","install_time":7}"#),
                Some((Some("from-file"), "manual", Some(7))),
            ),
            (&[], Some(r#"{"install_type":"manual"}"#), None),
            (&[], None, Some((None, "manual", None))),
            (
                &[("DOCKER_DD_AGENT", "true")],
                Some("not json"),
                Some((None, "docker_manual", None)),
            ),
            (
                &[("DOCKER_DD_AGENT", "true"), ("DD_APM_ENABLED", "true")],
                None,
                Some((None, "docker_single_step", None)),
            ),
        ];
        let directory =
            std::env::temp_dir().join(format!("install-signature-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("install.json");
        for (variables, file, expected) in cases {
            let _ = std::fs::remove_file(&path);
            if let Some(contents) = file {
                std::fs::write(&path, contents).unwrap();
            }
            let started = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            let signature = install_signature(
                |name| {
                    variables
                        .iter()
                        .find(|(key, _)| *key == name)
                        .map(|(_, value)| value.to_string())
                },
                &path,
            );
            let case = format!("variables {variables:?}, file {file:?}");
            let Some((id, install_type, time)) = expected else {
                assert_eq!(signature, None, "{case}");
                continue;
            };
            let signature = signature.unwrap_or_else(|| panic!("no signature: {case}"));
            assert_eq!(signature.install_type, install_type, "{case}");
            match (id, time) {
                (Some(id), Some(time)) => {
                    assert_eq!(
                        (signature.id.as_str(), signature.time),
                        (id, time),
                        "{case}"
                    );
                }
                _ => {
                    assert_eq!(signature.id.len(), 36, "{case}");
                    assert!(signature.time >= started, "{case}");
                    assert_eq!(read_install_signature(&path), Some(signature), "{case}");
                }
            }
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
