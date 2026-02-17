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
    time::{interval, Duration},
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

use dogstatsd::{
    aggregator_service::{AggregatorHandle, AggregatorService},
    api_key::ApiKeyFactory,
    constants::CONTEXTS,
    datadog::{MetricsIntakeUrlPrefix, RetryStrategy, Site},
    dogstatsd::{DogStatsD, DogStatsDConfig},
    flusher::{Flusher, FlusherConfig},
    util::parse_metric_namespace,
};

use dogstatsd::metric::{SortedTags, EMPTY_TAGS};
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
            dd_api_key,
            dd_site,
            https_proxy,
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

    let mut flush_interval = interval(Duration::from_secs(DOGSTATSD_FLUSH_INTERVAL));
    flush_interval.tick().await; // discard first tick, which is instantaneous

    loop {
        flush_interval.tick().await;

        if let Some(metrics_flusher) = metrics_flusher.as_ref() {
            debug!("Flushing dogstatsd metrics");
            metrics_flusher.flush().await;
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
    };

    #[cfg(not(all(windows, feature = "windows-pipes")))]
    let dogstatsd_config = DogStatsDConfig {
        host: AGENT_HOST.to_string(),
        port,
        metric_namespace,
        so_rcvbuf: None,
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
            #[allow(clippy::expect_used)]
            let metrics_flusher = Flusher::new(FlusherConfig {
                api_key_factory: Arc::new(ApiKeyFactory::new(&dd_api_key)),
                aggregator_handle: handle.clone(),
                metrics_intake_url_prefix: MetricsIntakeUrlPrefix::new(
                    Some(Site::new(dd_site).expect("Failed to parse site")),
                    None,
                )
                .expect("Failed to create intake URL prefix"),
                https_proxy,
                timeout: DOGSTATSD_TIMEOUT_DURATION,
                retry_strategy: RetryStrategy::LinearBackoff(3, 1),
                compression_level: CompressionLevel::try_from(6).unwrap_or_default(),
                // Not supported yet
                ca_cert_path: None,
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
