// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(not(test), deny(clippy::panic))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![cfg_attr(not(test), deny(clippy::todo))]
#![cfg_attr(not(test), deny(clippy::unimplemented))]

use std::{env, sync::Arc};
use tokio::{
    sync::Mutex as TokioMutex,
    time::{Duration, interval},
};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;
use zstd::zstd_safe::CompressionLevel;

use datadog_trace_agent::{
    aggregator::TraceAggregator,
    config, env_verifier, metrics_collector, mini_agent, proxy_flusher, stats_concentrator_service, stats_flusher,
   
    stats_processor,
    trace_flusher::{self, TraceFlusher},
    trace_processor,
};

use datadog_metrics_collector::cpu::CpuMetricsCollector;

use libdd_common::azure_app_services;
use libdd_trace_utils::{config_utils::read_cloud_env, trace_utils::EnvironmentType};

use datadog_fips::reqwest_adapter::create_reqwest_client_builder;
use datadog_logs_agent::{
    AggregatorHandle as LogAggregatorHandle, AggregatorService as LogAggregatorService,
    Destination as LogDestination, LogFlusher, LogFlusherConfig, LogServer, LogServerConfig,
};
use dogstatsd::{
    aggregator::{AggregatorHandle, AggregatorService},
    api_key::ApiKeyFactory,
    constants::CONTEXTS,
    datadog::{MetricsIntakeUrlPrefix, RetryStrategy, Site},
    dogstatsd::{DogStatsD, DogStatsDConfig},
    flusher::{Flusher, FlusherConfig},
    util::parse_metric_namespace,
};

use datadog_metrics_collector::azure_instance::InstanceMetricsCollector;
use dogstatsd::metric::{EMPTY_TAGS, SortedTags};
use tokio_util::sync::CancellationToken;

const CPU_METRICS_COLLECTION_INTERVAL: u64 = 3;
const DOGSTATSD_FLUSH_INTERVAL: u64 = 10;
const INSTANCE_METRICS_COLLECTION_INTERVAL_SECS: u64 = 3;
const DOGSTATSD_TIMEOUT_DURATION: Duration = Duration::from_secs(5);
const DEFAULT_DOGSTATSD_PORT: u16 = 8125;
const DEFAULT_LOG_INTAKE_PORT: u16 = 10517;
const AGENT_HOST: &str = "0.0.0.0";

struct StatsConcentratorComponents {
    handle: stats_concentrator_service::StatsConcentratorHandle,
    service_handle: tokio::task::JoinHandle<()>,
}

#[tokio::main]
pub async fn main() {
    let log_level = env::var("DD_LOG_LEVEL")
        .map(|val| val.to_lowercase())
        .unwrap_or("info".to_string());

    let (_, env_type) = match read_cloud_env() {
        Some(value) => value,
        None => {
            error!("Unable to identify environment. Shutting down Mini Agent.");
            return;
        }
    };

    let dogstatsd_tags = match env_type {
        EnvironmentType::CloudFunction => "origin:cloudfunction,dd.origin:cloudfunction",
        EnvironmentType::AzureFunction => "origin:azurefunction,dd.origin:azurefunction",
        EnvironmentType::AzureSpringApp => "origin:azurespringapp,dd.origin:azurespringapp",
        EnvironmentType::LambdaFunction => "origin:lambda,dd.origin:lambda", // historical reasons
    };

    let dd_api_key: Option<String> = env::var("DD_API_KEY").ok();

    // Windows named pipe name for DogStatsD.
    // Normalize by adding \\.\pipe\ prefix if not present
    let dd_dogstatsd_windows_pipe_name: Option<String> = {
        #[cfg(all(windows, feature = "windows-pipes"))]
        {
            env::var("DD_DOGSTATSD_WINDOWS_PIPE_NAME")
                .ok()
                .map(|pipe_name| {
                    if pipe_name.starts_with("\\\\.\\pipe\\") || pipe_name.starts_with(r"\\.\pipe\")
                    {
                        pipe_name
                    } else {
                        format!(r"\\.\pipe\{}", pipe_name)
                    }
                })
        }
        #[cfg(not(all(windows, feature = "windows-pipes")))]
        {
            None
        }
    };
    let dd_dogstatsd_port: u16 = if dd_dogstatsd_windows_pipe_name.is_some() {
        0 // Override to 0 when using Windows named pipe
    } else {
        env::var("DD_DOGSTATSD_PORT")
            .ok()
            .and_then(|port| port.parse::<u16>().ok())
            .unwrap_or(DEFAULT_DOGSTATSD_PORT)
    };
    let dd_site = env::var("DD_SITE").unwrap_or_else(|_| "datadoghq.com".to_string());
    let dd_use_dogstatsd = env::var("DD_USE_DOGSTATSD")
        .map(|val| val.to_lowercase() != "false")
        .unwrap_or(true);
    let dd_statsd_metric_namespace: Option<String> = env::var("DD_STATSD_METRIC_NAMESPACE")
        .ok()
        .and_then(|val| parse_metric_namespace(&val));

    // Only enable enhanced metrics for Azure Functions
    let dd_enhanced_metrics = env_type == EnvironmentType::AzureFunction
        && env::var("DD_ENHANCED_METRICS_ENABLED")
            .map(|val| val.to_lowercase() != "false")
            .unwrap_or(true);

    let https_proxy = env::var("DD_PROXY_HTTPS")
        .or_else(|_| env::var("HTTPS_PROXY"))
        .ok();
    let dd_logs_enabled = env::var("DD_LOGS_ENABLED")
        .map(|val| val.to_lowercase() == "true")
        .unwrap_or(false);
    let dd_logs_port: u16 = env::var("DD_LOGS_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(DEFAULT_LOG_INTAKE_PORT);

    let instance_metric_enabled = env_type == EnvironmentType::AzureFunction;

    let dd_agent_stats_computation_enabled = env::var("DD_AGENT_STATS_COMPUTATION_ENABLED")
        .map(|val| val.to_lowercase() == "true")
        .unwrap_or(false);

    debug!("Starting serverless trace mini agent");

    let env_filter = format!("h2=off,hyper=off,rustls=off,{}", log_level);

    #[allow(clippy::expect_used)]
    let subscriber = tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(
            EnvFilter::try_new(env_filter).expect("could not parse log level in configuration"),
        )
        .with_level(true)
        .with_thread_names(false)
        .with_thread_ids(false)
        .with_line_number(false)
        .with_file(false)
        .with_target(true)
        .without_time()
        .finish();

    #[allow(clippy::expect_used)]
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    debug!("Logging subsystem enabled");

    let env_verifier = Arc::new(env_verifier::ServerlessEnvVerifier::default());

    let config = match config::Config::new() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            error!("Error creating config on serverless trace mini agent startup: {e}");
            return;
        }
    };

    let stats_concentrator = if dd_agent_stats_computation_enabled {
        info!("agent stats computation enabled");
        let (service, handle) =
            stats_concentrator_service::StatsConcentratorService::new(config.clone());
        Some(StatsConcentratorComponents {
            service_handle: tokio::spawn(service.run()),
            handle,
        })
    } else {
        info!("agent stats computation disabled");
        None
    };

    let trace_processor = Arc::new(trace_processor::ServerlessTraceProcessor {
        stats_concentrator: stats_concentrator.as_ref().map(|c| c.handle.clone()),
    });

    let stats_flusher = Arc::new(stats_flusher::ServerlessStatsFlusher {
        stats_concentrator: stats_concentrator.as_ref().map(|c| c.handle.clone()),
    });
    let stats_processor = Arc::new(stats_processor::ServerlessStatsProcessor {});

    let trace_aggregator = Arc::new(TokioMutex::new(TraceAggregator::default()));
    let trace_flusher = Arc::new(trace_flusher::ServerlessTraceFlusher::new(
        trace_aggregator,
        Arc::clone(&config),
    ));

    let proxy_flusher = Arc::new(proxy_flusher::ProxyFlusher::new(Arc::clone(&config)));

    let mini_agent = Box::new(mini_agent::MiniAgent {
        config: Arc::clone(&config),
        env_verifier,
        trace_processor,
        trace_flusher,
        stats_processor,
        stats_flusher,
        proxy_flusher,
    });

    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let res = mini_agent
            .start_mini_agent(shutdown_rx, stats_concentrator.map(|c| c.service_handle))
            .await;
        if let Err(e) = res {
            error!("Error when starting serverless trace mini agent: {e:?}");
        }
    });

    let enabled_metrics_components =
        decide_metrics_components(dd_use_dogstatsd, instance_metric_enabled);

    // The metrics aggregator and flusher are started together and shared between dogstatsd and enhanced metrics,
    // so they are started if either is enabled.
    let (metrics_flusher, aggregator_handle) =
        if enabled_metrics_components.start_metrics_aggregator_and_flusher {
            debug!("Creating metrics flusher and aggregator");

            let (flusher, handle) = start_aggregator(
                dd_api_key.clone(),
                dd_site,
                https_proxy.clone(),
                dogstatsd_tags,
            )
            .await;

            if enabled_metrics_components.start_dogstatsd_listener {
                debug!("Starting dogstatsd");
                let _ = start_dogstatsd_listener(
                    dd_dogstatsd_port,
                    handle.clone(),
                    dd_statsd_metric_namespace,
                    #[cfg(all(windows, feature = "windows-pipes"))]
                    dd_dogstatsd_windows_pipe_name.clone(),
                )
                .await;
                if let Some(ref windows_pipe_name) = dd_dogstatsd_windows_pipe_name {
                    info!("dogstatsd-pipe: starting to listen on pipe {windows_pipe_name}");
                } else {
                    info!("dogstatsd-udp: starting to listen on port {dd_dogstatsd_port}");
                }
            } else {
                info!("dogstatsd disabled");
            }
            (flusher, Some(handle))
        } else {
            info!("dogstatsd and enhanced metrics disabled");
            (None, None)
        };

    // Skip enhanced metrics collection if we can't flush metrics
    let instance_collector: Option<InstanceMetricsCollector> =
    if enabled_metrics_components.start_instance_metrics_collector && metrics_flusher.is_some()
    {
        aggregator_handle.as_ref().and_then(|handle| {
            let tags = datadog_metrics_collector::azure_tags::build_enhanced_metrics_tags();
            InstanceMetricsCollector::new(handle.clone(), tags)
        })
    } else {
        None
    };

    let (log_flusher, _log_aggregator_handle): (Option<LogFlusher>, Option<LogAggregatorHandle>) =
    if dd_logs_enabled {
        debug!("Starting log agent");
        match start_log_agent(dd_api_key, https_proxy, dd_logs_port) {
            Some((flusher, handle)) => {
                info!("log agent started");
                (Some(flusher), Some(handle))
            }
            None => {
                warn!("log agent failed to start, log flushing disabled");
                (None, None)
            }
        }
    } else {
        info!("log agent disabled");
        (None, None)
    };

    // If DD_ENHANCED_METRICS is true, start the CPU metrics collector
    // Use the existing aggregator handle
    // TODO: See if this works in Google Cloud Functions Gen 1. If not, only enable this for Azure Functions.
    let mut cpu_collector = if dd_enhanced_metrics && metrics_flusher.is_some() {
        aggregator_handle.as_ref().map(|handle| {
            let tags = build_cpu_metrics_tags();
            CpuMetricsCollector::new(handle.clone(), tags)
        })
    } else {
        if !dd_enhanced_metrics {
            info!("Enhanced metrics disabled");
        } else {
            info!("Enhanced metrics enabled but metrics flusher not found");
        }
        None
    };

    let mut flush_interval = interval(Duration::from_secs(DOGSTATSD_FLUSH_INTERVAL));
    let mut instance_metrics_collection_interval = interval(Duration::from_secs(
        INSTANCE_METRICS_COLLECTION_INTERVAL_SECS,
    ));
    let mut cpu_collection_interval =
        interval(Duration::from_secs(CPU_METRICS_COLLECTION_INTERVAL));
    flush_interval.tick().await; // discard first tick, which is instantaneous
    instance_metrics_collection_interval.tick().await;
    cpu_collection_interval.tick().await;

    // Builders for log batches that failed transiently in the previous flush
    // cycle. They are redriven on the next cycle before new batches are sent.
    let mut pending_log_retries: Vec<reqwest::RequestBuilder> = Vec::new();

    loop {
        tokio::select! {
            _ = flush_interval.tick() => {
                if let Some(metrics_flusher) = metrics_flusher.clone() {
                    debug!("Flushing dogstatsd metrics");
                    tokio::spawn(async move {
                        metrics_flusher.flush().await;
                    });
                }
                if let Some(log_flusher) = log_flusher.as_ref() {
                    debug!("Flushing log agent");
                    let retry_in = std::mem::take(&mut pending_log_retries);
                    let failed = log_flusher.flush(retry_in).await;
                    if !failed.is_empty() {
                        // TODO: surface flush failures into health/metrics telemetry so
                        // operators have a durable signal beyond log lines when logs are
                        // being dropped (e.g. increment a statsd counter or set a gauge).
                        warn!(
                            "log agent flush failed for {} batch(es); will retry next cycle",
                            failed.len()
                        );
                        pending_log_retries = failed;
                    }
                }
            }
            _ = instance_metrics_collection_interval.tick(), if instance_collector.is_some() => {
                if let Some(ref collector) = instance_collector {
                    collector.collect_and_submit();
                }
            }
            _ = cpu_collection_interval.tick() => {
                if let Some(ref mut collector) = cpu_collector {
                    collector.collect_and_submit();
                }
            }
        }
        
    }
}

/// Starts the metrics aggregator service and creates a flusher to send
/// aggregated metrics to the Datadog intake.
async fn start_aggregator(
    dd_api_key: Option<String>,
    dd_site: String,
    https_proxy: Option<String>,
    dogstatsd_tags: &str,
) -> (Option<Flusher>, AggregatorHandle) {
    // Create the aggregator service
    #[allow(clippy::expect_used)]
    let (service, handle) = AggregatorService::new(
        SortedTags::parse(dogstatsd_tags).unwrap_or(EMPTY_TAGS),
        CONTEXTS,
    )
    .expect("Failed to create aggregator service");

    // Start the aggregator service in the background
    tokio::spawn(service.run());

    let metrics_flusher = match dd_api_key {
        Some(dd_api_key) => {
            let client = match build_metrics_client(https_proxy, DOGSTATSD_TIMEOUT_DURATION) {
                Ok(client) => client,
                Err(e) => {
                    error!("Failed to build HTTP client: {e}, won't flush metrics");
                    return (None, handle);
                }
            };
            let metrics_intake_url_prefix = match Site::new(dd_site)
                .map_err(|e| e.to_string())
                .and_then(|site| {
                    MetricsIntakeUrlPrefix::new(Some(site), None).map_err(|e| e.to_string())
                }) {
                Ok(prefix) => prefix,
                Err(e) => {
                    error!("Failed to create metrics intake URL: {e}, won't flush metrics");
                    return (None, handle);
                }
            };

            let metrics_flusher = Flusher::new(FlusherConfig {
                api_key_factory: Arc::new(ApiKeyFactory::new(&dd_api_key)),
                aggregator_handle: handle.clone(),
                metrics_intake_url_prefix,
                client,
                retry_strategy: RetryStrategy::LinearBackoff(3, 1),
                compression_level: CompressionLevel::try_from(6).unwrap_or_default(),
            });
            Some(metrics_flusher)
        }
        None => {
            error!("DD_API_KEY not set, won't flush metrics");
            None
        }
    };

    (metrics_flusher, handle)
}

async fn start_dogstatsd_listener(
    port: u16,
    handle: AggregatorHandle,
    metric_namespace: Option<String>,
    #[cfg(all(windows, feature = "windows-pipes"))] windows_pipe_name: Option<String>,
) -> CancellationToken {
    #[cfg(all(windows, feature = "windows-pipes"))]
    let dogstatsd_config = DogStatsDConfig {
        host: AGENT_HOST.to_string(),
        port,
        metric_namespace,
        windows_pipe_name,
        so_rcvbuf: None,
        buffer_size: None,
        queue_size: None,
    };

    #[cfg(not(all(windows, feature = "windows-pipes")))]
    let dogstatsd_config = DogStatsDConfig {
        host: AGENT_HOST.to_string(),
        port,
        metric_namespace,
        so_rcvbuf: None,
        buffer_size: None,
        queue_size: None,
    };
    let dogstatsd_cancel_token = tokio_util::sync::CancellationToken::new();

    // Use handle in DogStatsD (cheap to clone)
    let dogstatsd_client = DogStatsD::new(
        &dogstatsd_config,
        handle.clone(),
        dogstatsd_cancel_token.clone(),
    )
    .await;

    tokio::spawn(async move {
        dogstatsd_client.spin().await;
    });

    dogstatsd_cancel_token
}

fn build_metrics_client(
    https_proxy: Option<String>,
    timeout: Duration,
) -> Result<reqwest::Client, Box<dyn std::error::Error>> {
    let mut builder = create_reqwest_client_builder()?.timeout(timeout);
    if let Some(proxy) = https_proxy {
        builder = builder.proxy(reqwest::Proxy::https(proxy)?);
    }
    Ok(builder.build()?)
}

fn start_log_agent(
    dd_api_key: Option<String>,
    https_proxy: Option<String>,
    logs_port: u16,
) -> Option<(LogFlusher, LogAggregatorHandle)> {
    let Some(api_key) = dd_api_key else {
        error!("DD_API_KEY not set, log agent disabled");
        return None;
    };

    let (service, handle): (LogAggregatorService, LogAggregatorHandle) =
        LogAggregatorService::new();
    tokio::spawn(service.run());

    let client = create_reqwest_client_builder()
        .map_err(|e| error!("failed to create FIPS HTTP client for log agent: {e}"))
        .ok()
        .and_then(|b| {
            let mut builder = b.timeout(DOGSTATSD_TIMEOUT_DURATION);
            if let Some(ref proxy) = https_proxy {
                match reqwest::Proxy::https(proxy.as_str()) {
                    Ok(p) => builder = builder.proxy(p),
                    Err(e) => error!("invalid HTTPS proxy for log agent: {e}"),
                }
            }
            match builder.build() {
                Ok(c) => Some(c),
                Err(e) => {
                    error!("failed to build HTTP client for log agent: {e}");
                    None
                }
            }
        });

    let client = client?; // error already logged above

    let config = LogFlusherConfig {
        api_key,
        ..LogFlusherConfig::from_env()
    };

    // Fail fast: OPW mode with an empty URL will always produce a network error at flush time.
    if let LogDestination::ObservabilityPipelinesWorker { url } = &config.mode
        && url.is_empty()
    {
        error!(
            "OPW mode enabled but DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL is empty — log agent disabled"
        );
        return None;
    }

    // Start the HTTP intake server so external adapters can POST log entries.
    let server = LogServer::new(
        LogServerConfig {
            host: AGENT_HOST.to_string(),
            port: logs_port,
        },
        handle.clone(),
    );
    // TODO(SVLS-bind-fail-fast): `LogServer::serve` binds the port inside the
    // spawned task, so any bind failure (e.g. port already in use) is only
    // logged as an error and silently swallowed — this function still returns
    // `Some(...)` and the caller logs "log agent started" even though the
    // server never came up.
    // Fix: split `LogServer` into a `bind() -> Result<BoundLogServer, io::Error>`
    // step and a `BoundLogServer::serve()` accept-loop step (both in server.rs).
    // Make this fn `async`, call `server.bind().await`, return `None` on error,
    // and only spawn `bound.serve()` after a successful bind. Add tests:
    // `test_bind_returns_err_when_port_already_in_use` (server.rs) and
    // `test_start_log_agent_returns_none_when_port_already_in_use` (main.rs).
    tokio::spawn(server.serve());
    info!("log server listening on {AGENT_HOST}:{logs_port}");

    let flusher = LogFlusher::new(config, client, handle.clone());
    Some((flusher, handle))
}

/// Records which metrics components are enabled and should be started.
#[derive(Debug, PartialEq)]
struct EnabledMetricsComponents {
    start_metrics_aggregator_and_flusher: bool,
    start_dogstatsd_listener: bool,
    start_instance_metrics_collector: bool,
}

/// Determines which components should be started based on configuration.
///
/// The metrics aggregator and flusher are started together and shared between dogstatsd and enhanced metrics,
/// so they are started if either is enabled.
fn decide_metrics_components(
    dd_use_dogstatsd: bool,
    instance_metric_enabled: bool,
) -> EnabledMetricsComponents {
    let start_dogstatsd_listener = dd_use_dogstatsd;
    let start_instance_metrics_collector = instance_metric_enabled;
    let start_metrics_aggregator_and_flusher =
        start_dogstatsd_listener || start_instance_metrics_collector;

    EnabledMetricsComponents {
        start_metrics_aggregator_and_flusher,
        start_dogstatsd_listener,
        start_instance_metrics_collector,
    }
}

#[cfg(test)]
mod log_agent_integration_tests {
    use datadog_logs_agent::{AggregatorService, IntakeEntry, LogServer, LogServerConfig};

    #[tokio::test]
    async fn test_log_agent_full_pipeline_compiles_and_runs() {
        let (service, handle) = AggregatorService::new();
        tokio::spawn(service.run());

        handle
            .insert_batch(vec![IntakeEntry {
                message: "azure function invoked".to_string(),
                timestamp: 1_700_000_000_000,
                hostname: Some("my-azure-fn".to_string()),
                service: Some("payments".to_string()),
                ddsource: Some("azure-functions".to_string()),
                ddtags: Some("env:prod".to_string()),
                status: Some("info".to_string()),
                attributes: serde_json::Map::new(),
            }])
            .expect("insert_batch");

        let batches = handle.get_batches().await.expect("get_batches");
        assert_eq!(batches.len(), 1);

        let arr: serde_json::Value = serde_json::from_slice(&batches[0]).expect("json");
        assert_eq!(arr[0]["ddsource"], "azure-functions");
        assert_eq!(arr[0]["service"], "payments");

        handle.shutdown().expect("shutdown");
    }

    /// start_log_agent must return None when OPW mode is enabled but the URL is empty.
    #[tokio::test]
    async fn test_opw_empty_url_is_detected() {
        use super::start_log_agent;
        // Enable OPW mode with a deliberately empty URL — the production guard
        // inside start_log_agent must catch this and return None.
        // SAFETY: test-only, single-threaded setup before any spawned tasks.
        unsafe {
            std::env::set_var("DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED", "true");
            std::env::set_var("DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL", "");
        }
        let result = start_log_agent(Some("test-key".to_string()), None, 0);
        // SAFETY: test-only cleanup.
        unsafe {
            std::env::remove_var("DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED");
            std::env::remove_var("DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL");
        }
        assert!(
            result.is_none(),
            "start_log_agent must return None when OPW URL is empty"
        );
    }

    /// Full network intake path: entries posted over HTTP to LogServer must
    /// reach the AggregatorService and be retrievable via get_batches.
    /// This mirrors what serverless-compat does when DD_LOGS_ENABLED=true.
    #[tokio::test]
    #[allow(clippy::disallowed_methods, clippy::unwrap_used, clippy::expect_used)]
    async fn test_log_server_network_intake_end_to_end() {
        use tokio::time::{Duration, sleep};

        let (service, handle) = AggregatorService::new();
        tokio::spawn(service.run());

        // Bind :0 to discover a free port, then hand it to LogServer
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let server = LogServer::new(
            LogServerConfig {
                host: "127.0.0.1".into(),
                port,
            },
            handle.clone(),
        );
        tokio::spawn(server.serve());
        sleep(Duration::from_millis(50)).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/v1/input"))
            .json(&serde_json::json!([{
                "message":  "lambda function invoked",
                "timestamp": 1_700_000_000_000_i64,
                "ddsource": "lambda",
                "service":  "my-fn"
            }]))
            .send()
            .await
            .expect("POST to log server failed");

        assert_eq!(resp.status(), 200, "server must accept the payload");

        let batches = handle.get_batches().await.expect("get_batches");
        assert_eq!(batches.len(), 1, "one batch expected");
        let arr: serde_json::Value = serde_json::from_slice(&batches[0]).expect("json");
        assert_eq!(arr[0]["message"], "lambda function invoked");
        assert_eq!(arr[0]["ddsource"], "lambda");
        assert_eq!(arr[0]["service"], "my-fn");

        handle.shutdown().expect("shutdown");
    }
}

fn build_cpu_metrics_tags() -> Option<SortedTags> {
    let mut tag_parts = Vec::new();
    // Azure tags from ddcommon
    if let Some(aas_metadata) = &*azure_app_services::AAS_METADATA_FUNCTION {
        let aas_tags = [
            ("resource_id", aas_metadata.get_resource_id()),
            ("resource_group", aas_metadata.get_resource_group()),
            ("subscription_id", aas_metadata.get_subscription_id()),
            ("name", aas_metadata.get_site_name()),
        ];
        for (name, value) in aas_tags {
            if value != "unknown" {
                tag_parts.push(format!("{}:{}", name, value));
            }
        }
    }

    // Tags from env vars (not in ddcommon) - origin tag is added by DogStatsD
    for (tag_name, env_var) in [
        ("region", "REGION_NAME"),
        ("plan_tier", "WEBSITE_SKU"),
        ("service", "DD_SERVICE"),
        ("env", "DD_ENV"),
        ("version", "DD_VERSION"),
        ("serverless_compat_version", "DD_SERVERLESS_COMPAT_VERSION"),
    ] {
        if let Ok(val) = env::var(env_var) {
            if !val.is_empty() {
                tag_parts.push(format!("{}:{}", tag_name, val));
            }
        }
    }

    if tag_parts.is_empty() {
        return None;
    }
    SortedTags::parse(&tag_parts.join(",")).ok()
}

#[cfg(test)]
mod metrics_components_tests {
    use super::{EnabledMetricsComponents, decide_metrics_components};

    #[test]
    fn test_decide_metrics_components() {
        let cases: &[(bool, bool, EnabledMetricsComponents)] = &[
            (
                false,
                false,
                EnabledMetricsComponents {
                    start_metrics_aggregator_and_flusher: false,
                    start_dogstatsd_listener: false,
                    start_instance_metrics_collector: false,
                },
            ),
            (
                true,
                false,
                EnabledMetricsComponents {
                    start_metrics_aggregator_and_flusher: true,
                    start_dogstatsd_listener: true,
                    start_instance_metrics_collector: false,
                },
            ),
            (
                false,
                true,
                EnabledMetricsComponents {
                    start_metrics_aggregator_and_flusher: true,
                    start_dogstatsd_listener: false,
                    start_instance_metrics_collector: true,
                },
            ),
            (
                true,
                true,
                EnabledMetricsComponents {
                    start_metrics_aggregator_and_flusher: true,
                    start_dogstatsd_listener: true,
                    start_instance_metrics_collector: true,
                },
            ),
        ];

        for (dogstatsd, instance, expected) in cases {
            let actual = decide_metrics_components(*dogstatsd, *instance);
            assert_eq!(
                &actual, expected,
                "case (dd_use_dogstatsd={dogstatsd}, instance_metric_enabled={instance})"
            );
        }
    }
}
