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
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;
use zstd::zstd_safe::CompressionLevel;

use datadog_trace_agent::{
    aggregator::TraceAggregator,
    config, env_verifier, mini_agent, proxy_flusher, stats_flusher, stats_processor,
    trace_flusher::{self, TraceFlusher},
    trace_processor,
};

use libdd_trace_utils::{config_utils::read_cloud_env, trace_utils::EnvironmentType};

use datadog_fips::reqwest_adapter::create_reqwest_client_builder;
use datadog_log_agent::{
    aggregator::{
        AggregatorHandle as LogAggregatorHandle, AggregatorService as LogAggregatorService,
    },
    config::LogFlusherConfig,
    flusher::LogFlusher,
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

use dogstatsd::metric::{EMPTY_TAGS, SortedTags};
use tokio_util::sync::CancellationToken;

const DOGSTATSD_FLUSH_INTERVAL: u64 = 10;
const DOGSTATSD_TIMEOUT_DURATION: Duration = Duration::from_secs(5);
const DEFAULT_DOGSTATSD_PORT: u16 = 8125;
const AGENT_HOST: &str = "0.0.0.0";

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

    let https_proxy = env::var("DD_PROXY_HTTPS")
        .or_else(|_| env::var("HTTPS_PROXY"))
        .ok();
    let dd_logs_enabled = env::var("DD_LOGS_ENABLED")
        .map(|val| val.to_lowercase() != "false")
        .unwrap_or(true);
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

    let trace_processor = Arc::new(trace_processor::ServerlessTraceProcessor {});

    let stats_flusher = Arc::new(stats_flusher::ServerlessStatsFlusher {});
    let stats_processor = Arc::new(stats_processor::ServerlessStatsProcessor {});

    let config = match config::Config::new() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            error!("Error creating config on serverless trace mini agent startup: {e}");
            return;
        }
    };

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

    tokio::spawn(async move {
        let res = mini_agent.start_mini_agent().await;
        if let Err(e) = res {
            error!("Error when starting serverless trace mini agent: {e:?}");
        }
    });

    let (metrics_flusher, _aggregator_handle) = if dd_use_dogstatsd {
        debug!("Starting dogstatsd");
        let (_, metrics_flusher, aggregator_handle) = start_dogstatsd(
            dd_dogstatsd_port,
            dd_api_key.clone(),
            dd_site,
            https_proxy.clone(),
            dogstatsd_tags,
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
        (metrics_flusher, Some(aggregator_handle))
    } else {
        info!("dogstatsd disabled");
        (None, None)
    };

    let log_flusher: Option<LogFlusher> = if dd_logs_enabled {
        debug!("Starting log agent");
        let log_flusher = start_log_agent(dd_api_key, https_proxy);
        info!("log agent started");
        log_flusher
    } else {
        info!("log agent disabled");
        None
    };

    let mut flush_interval = interval(Duration::from_secs(DOGSTATSD_FLUSH_INTERVAL));
    flush_interval.tick().await; // discard first tick, which is instantaneous

    loop {
        flush_interval.tick().await;

        if let Some(metrics_flusher) = metrics_flusher.as_ref() {
            debug!("Flushing dogstatsd metrics");
            metrics_flusher.flush().await;
        }

        if let Some(log_flusher) = log_flusher.as_ref() {
            debug!("Flushing log agent");
            log_flusher.flush().await;
        }
    }
}

async fn start_dogstatsd(
    port: u16,
    dd_api_key: Option<String>,
    dd_site: String,
    https_proxy: Option<String>,
    dogstatsd_tags: &str,
    metric_namespace: Option<String>,
    #[cfg(all(windows, feature = "windows-pipes"))] windows_pipe_name: Option<String>,
) -> (CancellationToken, Option<Flusher>, AggregatorHandle) {
    // 1. Create the aggregator service
    #[allow(clippy::expect_used)]
    let (service, handle) = AggregatorService::new(
        SortedTags::parse(dogstatsd_tags).unwrap_or(EMPTY_TAGS),
        CONTEXTS,
    )
    .expect("Failed to create aggregator service");

    // 2. Start the aggregator service in the background
    tokio::spawn(service.run());

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

    // 3. Use handle in DogStatsD (cheap to clone)
    let dogstatsd_client = DogStatsD::new(
        &dogstatsd_config,
        handle.clone(),
        dogstatsd_cancel_token.clone(),
    )
    .await;

    tokio::spawn(async move {
        dogstatsd_client.spin().await;
    });

    let metrics_flusher = match dd_api_key {
        Some(dd_api_key) => {
            let client = match build_metrics_client(https_proxy, DOGSTATSD_TIMEOUT_DURATION) {
                Ok(client) => client,
                Err(e) => {
                    error!("Failed to build HTTP client: {e}, won't flush metrics");
                    return (dogstatsd_cancel_token, None, handle);
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
                    return (dogstatsd_cancel_token, None, handle);
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

    (dogstatsd_cancel_token, metrics_flusher, handle)
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

fn start_log_agent(dd_api_key: Option<String>, https_proxy: Option<String>) -> Option<LogFlusher> {
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
            builder.build().ok()
        });

    let client = match client {
        Some(c) => c,
        None => {
            error!("failed to build HTTP client for log agent, log flushing disabled");
            return None;
        }
    };

    if dd_api_key.is_none() {
        error!("DD_API_KEY not set, log agent won't flush logs");
    }

    let config = LogFlusherConfig::from_env();
    Some(LogFlusher::new(config, client, handle))
}

#[cfg(test)]
mod log_agent_integration_tests {
    use datadog_log_agent::{
        AggregatorService, FlusherMode, LogEntry, LogFlusher, LogFlusherConfig,
    };

    #[tokio::test]
    async fn test_log_agent_full_pipeline_compiles_and_runs() {
        let (service, handle) = AggregatorService::new();
        tokio::spawn(service.run());

        handle
            .insert_batch(vec![LogEntry {
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

    /// Verify the LogFlusher struct is constructible from within this crate — compile-time test.
    #[allow(dead_code)]
    fn _assert_log_flusher_constructible() {
        let _ = std::mem::size_of::<LogFlusher>();
        let _ = std::mem::size_of::<LogFlusherConfig>();
        let _ = std::mem::size_of::<FlusherMode>();
    }
}
