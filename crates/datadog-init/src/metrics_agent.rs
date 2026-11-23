// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! The DogStatsD listener and the metrics flusher, configured for serverless-init.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use datadog_fips::reqwest_adapter::create_reqwest_client_builder;
use datadog_serverless_core::env::parse_bool;
use dogstatsd::aggregator::{AggregatorHandle, AggregatorService, SeriesFormat};
use dogstatsd::api_key::ApiKeyFactory;
use dogstatsd::constants::CONTEXTS;
use dogstatsd::datadog::{
    DdDdUrl, DdUrl, MetricsIntakeUrlPrefix, MetricsIntakeUrlPrefixOverride, RetryStrategy, Site,
};
use dogstatsd::dogstatsd::{DogStatsD, DogStatsDConfig};
use dogstatsd::flusher::{Flusher, FlusherConfig};
use dogstatsd::metric::{EMPTY_TAGS, SortedTags};
use dogstatsd::origin::OriginProduct;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::agent_telemetry::AgentTelemetry;
use crate::enhanced_metrics::EnhancedMetrics;
use crate::platform::{self, EnhancedMetricsProfile, MetricOrigin, Platform};
use crate::settings;

const DEFAULT_DOGSTATSD_PORT: u16 = 8125;
/// serverless-init flushes metrics every 3 seconds.
const FLUSH_INTERVAL: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const COMPRESSION_LEVEL: i32 = 6;
/// How long shutdown waits for the listener to hand its queued packets to the aggregator.
const LISTENER_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
/// How much of the metrics budget shutdown leaves unused, so that it returns before the
/// budget's timeout while it waits for a periodic flush still in flight.
const BUDGET_MARGIN: Duration = Duration::from_millis(50);

/// The metrics agent settings read from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsAgentSettings {
    /// `DD_API_KEY`, without surrounding whitespace. Empty when it is not set.
    pub api_key: String,
    /// `DD_USE_DOGSTATSD`.
    pub use_dogstatsd: bool,
    /// `DD_ENHANCED_METRICS_ENABLED`.
    pub enhanced_metrics: bool,
    /// `DD_DOGSTATSD_PORT`.
    pub port: u16,
    /// `DD_DOGSTATSD_NON_LOCAL_TRAFFIC`. Sidecar mode sets it to true when it is unset.
    pub non_local_traffic: bool,
    /// From `DD_DD_URL`, else `DD_URL`, else `DD_SITE`, as in the Go agent.
    pub intake_url_prefix: MetricsIntakeUrlPrefix,
    /// `DD_PROXY_HTTPS`, else `HTTPS_PROXY`.
    pub proxy_url: Option<String>,
}

/// Why the DogStatsD listener and its flush do not start. The agent's own telemetry is sent
/// either way, as the Go agent's aggregator flushes without them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DogStatsDDisabled {
    /// `DD_USE_DOGSTATSD` is false.
    UseDogStatsDFalse,
    /// `DD_API_KEY` is not set to a non-empty value, so the Go agent turns DogStatsD off.
    MissingApiKey,
}

impl MetricsAgentSettings {
    pub fn dogstatsd_disabled(&self) -> Option<DogStatsDDisabled> {
        if !self.use_dogstatsd {
            Some(DogStatsDDisabled::UseDogStatsDFalse)
        } else if self.api_key.is_empty() {
            Some(DogStatsDDisabled::MissingApiKey)
        } else {
            None
        }
    }

    /// Without an API key, Go serverless-init sends no enhanced metrics. They do not need
    /// DogStatsD.
    pub fn enhanced_metrics_enabled(&self) -> bool {
        self.enhanced_metrics && !self.api_key.is_empty()
    }
}

/// Reads the metrics agent settings through `lookup`, which returns an environment variable's
/// value. Fails when the intake URL or the site is not valid.
pub fn settings_from_environment(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<MetricsAgentSettings, String> {
    let bool_setting = |name: &str, default: bool| {
        parse_bool(lookup(name).as_deref(), default).unwrap_or_else(|error| {
            warn!("{name}: {error}, so the default {default} is used");
            default
        })
    };

    let api_key = lookup("DD_API_KEY")
        .map(|key| key.trim().to_string())
        .unwrap_or_default();
    let non_empty = |name: &str| lookup(name).filter(|value| !value.is_empty());
    // Go ignores an empty value of these settings, and reads an invalid one as false.
    let on_unless_false = |name: &str| {
        parse_bool(non_empty(name).as_deref(), true).unwrap_or_else(|error| {
            warn!("{name}: {error}, so it is read as false");
            false
        })
    };
    let intake_url_prefix = intake_url_prefix(
        non_empty("DD_DD_URL"),
        non_empty("DD_URL"),
        settings::intake_site(&lookup),
    )?;

    Ok(MetricsAgentSettings {
        api_key,
        use_dogstatsd: on_unless_false("DD_USE_DOGSTATSD"),
        enhanced_metrics: on_unless_false("DD_ENHANCED_METRICS_ENABLED"),
        port: lookup("DD_DOGSTATSD_PORT")
            .and_then(|port| port.parse::<u16>().ok())
            .unwrap_or(DEFAULT_DOGSTATSD_PORT),
        non_local_traffic: bool_setting("DD_DOGSTATSD_NON_LOCAL_TRAFFIC", false),
        intake_url_prefix,
        proxy_url: settings::proxy_url(&lookup),
    })
}

/// The Go agent's `dd_url` (`DD_DD_URL`, else `DD_URL`) takes precedence over the site. A
/// trailing slash is dropped, because the series and sketch paths are appended to the prefix.
fn intake_url_prefix(
    dd_dd_url: Option<String>,
    dd_url: Option<String>,
    site: String,
) -> Result<MetricsIntakeUrlPrefix, String> {
    let without_slash = |url: String| url.trim_end_matches('/').to_string();
    let dd_dd_url = dd_dd_url
        .map(|url| DdDdUrl::new(without_slash(url)))
        .transpose()
        .map_err(|error| error.to_string())?;
    let dd_url = dd_url
        .map(|url| DdUrl::new(without_slash(url)))
        .transpose()
        .map_err(|error| error.to_string())?;
    let site = match (&dd_dd_url, &dd_url) {
        (None, None) => Some(Site::new(site).map_err(|error| error.to_string())?),
        _ => None,
    };
    MetricsIntakeUrlPrefix::new(
        site,
        MetricsIntakeUrlPrefixOverride::maybe_new(dd_url, dd_dd_url),
    )
    .map_err(|error| error.to_string())
}

/// A running metrics agent, kept until shutdown.
pub struct MetricsAgent {
    cancel_token: CancellationToken,
    flush_loop: JoinHandle<()>,
    telemetry: AgentTelemetry,
    dogstatsd: Option<DogStatsDPipeline>,
    enhanced: Option<EnhancedMetrics>,
}

/// The DogStatsD listener and the flusher of its aggregator.
struct DogStatsDPipeline {
    listener: JoinHandle<()>,
    flusher: Flusher,
}

impl MetricsAgent {
    /// Starts the periodic flush of the agent's own telemetry, unless DogStatsD is disabled the
    /// DogStatsD listener and its aggregator, and unless they are disabled the enhanced metrics,
    /// from the environment. Every DogStatsD metric gets `telemetry_tags`, and `sidecar` sets the
    /// enhanced metrics' mode tags. Returns `None`, after logging why, when the intake
    /// configuration is invalid.
    pub fn start(
        sidecar: bool,
        platform: &Platform,
        telemetry_tags: &BTreeMap<String, String>,
    ) -> Option<Self> {
        let settings = match settings_from_environment(|name| std::env::var(name).ok()) {
            Ok(settings) => settings,
            Err(error) => {
                error!("{error}; metrics are disabled");
                return None;
            }
        };
        let client = match build_client(settings.proxy_url.clone()) {
            Ok(client) => client,
            Err(error) => {
                error!("Failed to build the metrics HTTP client: {error}; metrics are disabled");
                return None;
            }
        };
        let telemetry = AgentTelemetry::new(
            client.clone(),
            settings.api_key.clone(),
            &settings.intake_url_prefix,
        );
        let cancel_token = CancellationToken::new();
        let dogstatsd = match settings.dogstatsd_disabled() {
            Some(DogStatsDDisabled::UseDogStatsDFalse) => {
                info!("DogStatsD is disabled");
                None
            }
            Some(DogStatsDDisabled::MissingApiKey) => {
                warn!("DD_API_KEY is not set; DogStatsD is disabled");
                None
            }
            None => {
                let tags =
                    platform::dogstatsd_tags(telemetry_tags, |name| std::env::var(name).ok());
                start_dogstatsd(
                    &settings,
                    client.clone(),
                    &tags,
                    platform.metric_origin(false),
                    cancel_token.clone(),
                )
            }
        };
        let enhanced = match platform.enhanced_metrics() {
            _ if !settings.enhanced_metrics_enabled() => None,
            Some(profile) => start_enhanced_metrics(
                &settings,
                client,
                EnhancedMetricsSetup {
                    profile: &profile,
                    origin: platform.metric_origin(true),
                    sidecar,
                    legacy_tags: &platform::dogstatsd_tags(telemetry_tags, |_| None),
                },
                cancel_token.clone(),
            ),
            None => {
                info!("Enhanced metrics are not supported on this platform yet; they are disabled");
                None
            }
        };
        Some(Self::spawn(telemetry, dogstatsd, enhanced, cancel_token))
    }

    fn spawn(
        telemetry: AgentTelemetry,
        dogstatsd: Option<DogStatsDPipeline>,
        enhanced: Option<EnhancedMetrics>,
        cancel_token: CancellationToken,
    ) -> Self {
        let flush_loop = tokio::spawn(flush_periodically(
            dogstatsd.as_ref().map(|pipeline| pipeline.flusher.clone()),
            enhanced.as_ref().map(|enhanced| enhanced.flusher().clone()),
            telemetry.clone(),
            cancel_token.clone(),
        ));
        Self {
            cancel_token,
            flush_loop,
            telemetry,
            dogstatsd,
            enhanced,
        }
    }

    /// Stops the listener, the enhanced metrics collector and the periodic flush, then flushes
    /// every bucket, including the open one, and sends the agent's own telemetry. The listener
    /// hands its queued packets to the aggregator before it returns, so the final flush waits
    /// for it, for at most `LISTENER_DRAIN_TIMEOUT`, as the Go agent waits for its DogStatsD
    /// workers before it stops the aggregator.
    ///
    /// A periodic flush still in flight can wait up to `REQUEST_TIMEOUT`, which is longer than
    /// the metrics budget, so the final flush runs alongside it instead of after it. Afterwards
    /// shutdown waits for it until `BUDGET_MARGIN` before the end of `budget`, the caller's
    /// metrics budget; past that, it keeps running in the background until the process exits.
    pub async fn flush_at_shutdown(self, budget: Duration) {
        let deadline = tokio::time::Instant::now() + budget.saturating_sub(BUDGET_MARGIN);
        self.cancel_token.cancel();
        let enhanced_flush = async {
            if let Some(enhanced) = self.enhanced {
                enhanced.flush_at_shutdown().await;
            }
        };
        let final_flush = async {
            if let Some(pipeline) = self.dogstatsd {
                match tokio::time::timeout(LISTENER_DRAIN_TIMEOUT, pipeline.listener).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => error!("The DogStatsD listener task failed: {error}"),
                    Err(_) => warn!(
                        "The DogStatsD listener did not stop within {LISTENER_DRAIN_TIMEOUT:?}; \
                         flushing without the packets it still holds"
                    ),
                }
                pipeline.flusher.flush().await;
            }
        };
        // The telemetry waits for a periodic flush's send still in flight, within the budget.
        let telemetry = async {
            if tokio::time::timeout_at(deadline, self.telemetry.send())
                .await
                .is_err()
            {
                debug!("The final agent telemetry did not finish within the metrics budget");
            }
        };
        tokio::join!(final_flush, enhanced_flush, telemetry);
        match tokio::time::timeout_at(deadline, self.flush_loop).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => error!("The periodic metrics flush task failed: {error}"),
            Err(_) => debug!(
                "The periodic metrics flush did not finish within the metrics budget; it \
                 continues until the process exits"
            ),
        }
    }
}

/// Starts the aggregator, whose metrics all get `tags` and the serverless origin product, and the
/// DogStatsD listener. Returns `None`, after logging why, when the aggregator cannot be created.
fn start_dogstatsd(
    settings: &MetricsAgentSettings,
    client: reqwest::Client,
    tags: &[String],
    origin: MetricOrigin,
    cancel_token: CancellationToken,
) -> Option<DogStatsDPipeline> {
    let (aggregator, aggregator_handle) = match dogstatsd_aggregator(tags, origin) {
        Ok(created) => created,
        Err(error) => {
            error!("Failed to create the metrics aggregator: {error}; DogStatsD is disabled");
            return None;
        }
    };
    tokio::spawn(aggregator.run());
    let flusher = build_flusher(settings, client, aggregator_handle.clone());

    let dogstatsd_config = DogStatsDConfig {
        host: listen_host(settings.non_local_traffic).to_string(),
        port: settings.port,
        metric_namespace: None,
        so_rcvbuf: None,
        buffer_size: None,
        queue_size: None,
    };
    // The listener is created inside the task, so a failed bind ends only this task.
    let listener = tokio::spawn(async move {
        let server = DogStatsD::new(&dogstatsd_config, aggregator_handle, cancel_token.clone())
            .await
            .applying_sample_rates()
            .with_extended_metric_types();
        info!(
            "dogstatsd-udp: starting to listen on {}:{}",
            dogstatsd_config.host, dogstatsd_config.port
        );
        server.spin().await;
        if !cancel_token.is_cancelled() {
            error!("dogstatsd: listener stopped unexpectedly");
        }
    });
    Some(DogStatsDPipeline { listener, flusher })
}

/// What the enhanced metrics need from the platform and the mode.
struct EnhancedMetricsSetup<'a> {
    profile: &'a EnhancedMetricsProfile,
    origin: MetricOrigin,
    sidecar: bool,
    legacy_tags: &'a [String],
}

/// Starts the enhanced metrics with their own aggregator. Returns `None`, after logging why,
/// when the aggregator cannot be created.
fn start_enhanced_metrics(
    settings: &MetricsAgentSettings,
    client: reqwest::Client,
    setup: EnhancedMetricsSetup,
    cancel_token: CancellationToken,
) -> Option<EnhancedMetrics> {
    let (aggregator, aggregator_handle) = match AggregatorService::new(EMPTY_TAGS, CONTEXTS) {
        Ok((aggregator, handle)) => (serverless_aggregator(aggregator, setup.origin), handle),
        Err(error) => {
            error!(
                "Failed to create the enhanced metrics aggregator: {error}; enhanced metrics \
                 are disabled"
            );
            return None;
        }
    };
    tokio::spawn(aggregator.run());
    let flusher = build_flusher(settings, client, aggregator_handle.clone());
    Some(EnhancedMetrics::start(
        aggregator_handle,
        flusher,
        setup.profile,
        setup.sidecar,
        setup.legacy_tags,
        cancel_token,
    ))
}

fn dogstatsd_aggregator(
    tags: &[String],
    origin: MetricOrigin,
) -> Result<(AggregatorService, AggregatorHandle), dogstatsd::errors::Creation> {
    let tags = SortedTags::from_tags(tags.iter().map(String::as_str));
    let (aggregator, handle) = AggregatorService::new(tags, CONTEXTS)?;
    Ok((serverless_aggregator(aggregator, origin), handle))
}

/// The agent series format, tags sent once, and the serverless origin product with the
/// platform's category and services that Go gives DogStatsD and enhanced metrics.
fn serverless_aggregator(aggregator: AggregatorService, origin: MetricOrigin) -> AggregatorService {
    let aggregator = aggregator
        .with_series_format(SeriesFormat::Agent)
        .with_unique_tags()
        .with_origin(OriginProduct::Serverless, origin.category, origin.service);
    match origin.runtime_service {
        Some(service) => aggregator.with_runtime_origin_service(service),
        None => aggregator,
    }
}

fn build_flusher(
    settings: &MetricsAgentSettings,
    client: reqwest::Client,
    aggregator_handle: AggregatorHandle,
) -> Flusher {
    Flusher::new(FlusherConfig {
        api_key_factory: Arc::new(ApiKeyFactory::new(&settings.api_key)),
        aggregator_handle,
        metrics_intake_url_prefix: settings.intake_url_prefix.clone(),
        client,
        retry_strategy: RetryStrategy::LinearBackoff(3, 1),
        compression_level: COMPRESSION_LEVEL,
    })
}

/// Flushes the ended 10-second buckets of DogStatsD and of the enhanced metrics, when they run,
/// and sends the agent's own telemetry every `FLUSH_INTERVAL`, as the Go agent does, starting one
/// interval after the agent starts, until `cancel_token` is cancelled.
async fn flush_periodically(
    dogstatsd: Option<Flusher>,
    enhanced: Option<Flusher>,
    telemetry: AgentTelemetry,
    cancel_token: CancellationToken,
) {
    let mut ticker = interval(FLUSH_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker.tick().await;
    loop {
        tokio::select! {
            biased;
            _ = cancel_token.cancelled() => return,
            _ = ticker.tick() => {
                debug!("Flushing DogStatsD metrics");
                tokio::join!(
                    flush_closed_buckets(dogstatsd.as_ref()),
                    flush_closed_buckets(enhanced.as_ref()),
                    telemetry.send(),
                );
            }
        }
    }
}

async fn flush_closed_buckets(flusher: Option<&Flusher>) {
    if let Some(flusher) = flusher {
        flusher.flush_closed_buckets().await;
    }
}

/// DogStatsD accepts traffic from other hosts only when non-local traffic is allowed.
fn listen_host(non_local_traffic: bool) -> &'static str {
    if non_local_traffic {
        "0.0.0.0"
    } else {
        "127.0.0.1"
    }
}

pub(crate) fn build_client(
    proxy_url: Option<String>,
) -> Result<reqwest::Client, Box<dyn std::error::Error>> {
    let mut builder = create_reqwest_client_builder()?.timeout(REQUEST_TIMEOUT);
    if let Some(proxy) = proxy_url {
        builder = builder.proxy(reqwest::Proxy::https(proxy)?);
    }
    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datadog_serverless_core::shutdown::{FlushBudget, FlushOutcome, run_with_budget};
    use dogstatsd::datadog::{DdDdUrl, MetricsIntakeUrlPrefixOverride, Site};

    fn settings_for(variables: &[(&str, &str)]) -> Result<MetricsAgentSettings, String> {
        settings_from_environment(|name| {
            variables
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    fn prefix_from_site(site: &str) -> MetricsIntakeUrlPrefix {
        MetricsIntakeUrlPrefix::new(Some(Site::new(site.to_string()).unwrap()), None).unwrap()
    }

    fn prefix_from_url(url: &str) -> MetricsIntakeUrlPrefix {
        let override_prefix = MetricsIntakeUrlPrefixOverride::maybe_new(
            None,
            Some(DdDdUrl::new(url.to_string()).unwrap()),
        );
        MetricsIntakeUrlPrefix::new(None, override_prefix).unwrap()
    }

    fn default_settings() -> MetricsAgentSettings {
        MetricsAgentSettings {
            api_key: "key".to_string(),
            use_dogstatsd: true,
            enhanced_metrics: true,
            port: 8125,
            non_local_traffic: false,
            intake_url_prefix: prefix_from_site("datadoghq.com."),
            proxy_url: None,
        }
    }

    #[test]
    fn settings_from_environment_table() {
        type Case = (
            &'static [(&'static str, &'static str)],
            Result<MetricsAgentSettings, String>,
        );
        let without_api_key = MetricsAgentSettings {
            api_key: String::new(),
            ..default_settings()
        };
        let cases: Vec<Case> = vec![
            (&[("DD_API_KEY", "key")], Ok(default_settings())),
            (&[("DD_API_KEY", " key ")], Ok(default_settings())),
            (&[], Ok(without_api_key.clone())),
            (&[("DD_API_KEY", "  ")], Ok(without_api_key.clone())),
            (&[("DD_APM_API_KEY", "key")], Ok(without_api_key)),
            (
                &[("DD_API_KEY", "key"), ("DD_USE_DOGSTATSD", "false")],
                Ok(MetricsAgentSettings {
                    use_dogstatsd: false,
                    ..default_settings()
                }),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_ENHANCED_METRICS_ENABLED", "false"),
                ],
                Ok(MetricsAgentSettings {
                    enhanced_metrics: false,
                    ..default_settings()
                }),
            ),
            // Go ignores an empty value, and reads an invalid one as false.
            (
                &[("DD_API_KEY", "key"), ("DD_ENHANCED_METRICS_ENABLED", "")],
                Ok(default_settings()),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_ENHANCED_METRICS_ENABLED", "yes"),
                ],
                Ok(MetricsAgentSettings {
                    enhanced_metrics: false,
                    ..default_settings()
                }),
            ),
            (
                &[("DD_API_KEY", "key"), ("DD_USE_DOGSTATSD", "not a bool")],
                Ok(MetricsAgentSettings {
                    use_dogstatsd: false,
                    ..default_settings()
                }),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_DOGSTATSD_PORT", "9125"),
                    ("DD_DOGSTATSD_NON_LOCAL_TRAFFIC", "true"),
                ],
                Ok(MetricsAgentSettings {
                    port: 9125,
                    non_local_traffic: true,
                    ..default_settings()
                }),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_DOGSTATSD_PORT", "not a port"),
                    ("DD_DOGSTATSD_NON_LOCAL_TRAFFIC", "yes"),
                ],
                Ok(default_settings()),
            ),
            // The intake: DD_DD_URL, else DD_URL, else DD_SITE.
            (
                &[("DD_API_KEY", "key"), ("DD_SITE", "datadoghq.eu")],
                Ok(MetricsAgentSettings {
                    intake_url_prefix: prefix_from_site("datadoghq.eu."),
                    ..default_settings()
                }),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_SITE", "datadoghq.eu"),
                    ("DD_DD_URL", "http://fakeintake:80"),
                ],
                Ok(MetricsAgentSettings {
                    intake_url_prefix: prefix_from_url("http://fakeintake:80"),
                    ..default_settings()
                }),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_DD_URL", "http://dd-dd-url:80"),
                    ("DD_URL", "http://dd-url:80"),
                ],
                Ok(MetricsAgentSettings {
                    intake_url_prefix: prefix_from_url("http://dd-dd-url:80"),
                    ..default_settings()
                }),
            ),
            (
                &[("DD_API_KEY", "key"), ("DD_URL", "http://dd-url:80")],
                Ok(MetricsAgentSettings {
                    intake_url_prefix: prefix_from_url("http://dd-url:80"),
                    ..default_settings()
                }),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_DD_URL", ""),
                    ("DD_SITE", "datadoghq.eu"),
                ],
                Ok(MetricsAgentSettings {
                    intake_url_prefix: prefix_from_site("datadoghq.eu."),
                    ..default_settings()
                }),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_DD_URL", "https://intake.example.com/"),
                ],
                Ok(MetricsAgentSettings {
                    intake_url_prefix: prefix_from_url("https://intake.example.com"),
                    ..default_settings()
                }),
            ),
            (
                &[("DD_API_KEY", "key"), ("DD_DD_URL", "fakeintake")],
                Err("Invalid URL prefix: fakeintake".to_string()),
            ),
            (
                &[("DD_API_KEY", "key"), ("DD_SITE", "")],
                Ok(default_settings()),
            ),
            (
                &[("DD_API_KEY", "key"), ("DD_SITE", "datadoghq.com/path")],
                Err("Invalid site: datadoghq.com/path".to_string()),
            ),
            (
                &[
                    ("DD_API_KEY", "key"),
                    ("DD_PROXY_HTTPS", "http://datadog-proxy:3128"),
                    ("HTTPS_PROXY", "http://proxy:3128"),
                ],
                Ok(MetricsAgentSettings {
                    proxy_url: Some("http://datadog-proxy:3128".to_string()),
                    ..default_settings()
                }),
            ),
            (
                &[("DD_API_KEY", "key"), ("HTTPS_PROXY", "http://proxy:3128")],
                Ok(MetricsAgentSettings {
                    proxy_url: Some("http://proxy:3128".to_string()),
                    ..default_settings()
                }),
            ),
        ];
        for (variables, expected) in cases {
            assert_eq!(settings_for(variables), expected, "variables {variables:?}");
        }
    }

    #[test]
    fn dogstatsd_disabled_table() {
        let cases = [
            (true, "key", None),
            (true, "", Some(DogStatsDDisabled::MissingApiKey)),
            (false, "key", Some(DogStatsDDisabled::UseDogStatsDFalse)),
            (false, "", Some(DogStatsDDisabled::UseDogStatsDFalse)),
        ];
        for (use_dogstatsd, api_key, expected) in cases {
            let settings = MetricsAgentSettings {
                use_dogstatsd,
                api_key: api_key.to_string(),
                ..default_settings()
            };
            assert_eq!(
                settings.dogstatsd_disabled(),
                expected,
                "use_dogstatsd {use_dogstatsd}, api_key {api_key:?}"
            );
        }
    }

    /// Enhanced metrics need an API key, as in Go, but not DogStatsD.
    #[test]
    fn enhanced_metrics_enabled_table() {
        let cases = [
            (true, true, "key", true),
            (true, false, "key", true),
            (true, true, "", false),
            (false, true, "key", false),
        ];
        for (enhanced_metrics, use_dogstatsd, api_key, expected) in cases {
            let settings = MetricsAgentSettings {
                enhanced_metrics,
                use_dogstatsd,
                api_key: api_key.to_string(),
                ..default_settings()
            };
            assert_eq!(
                settings.enhanced_metrics_enabled(),
                expected,
                "enhanced_metrics {enhanced_metrics}, use_dogstatsd {use_dogstatsd}, \
                 api_key {api_key:?}"
            );
        }
    }

    /// The requests that a local intake received, as path and decompressed body.
    type Captured = Arc<std::sync::Mutex<Vec<(String, String)>>>;

    /// Starts an HTTP intake on a free local port that answers every request with 202 and
    /// records it.
    async fn start_capture_intake() -> (String, Captured) {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let captured = Captured::default();
        let recorder = captured.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let recorder = recorder.clone();
                tokio::spawn(async move {
                    let mut stream = BufReader::new(stream);
                    loop {
                        let mut request_line = String::new();
                        if stream.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let path = request_line.split(' ').nth(1).unwrap_or("").to_string();
                        let mut content_length = 0;
                        loop {
                            let mut header = String::new();
                            stream.read_line(&mut header).await.unwrap();
                            let header = header.trim_end();
                            if header.is_empty() {
                                break;
                            }
                            if let Some((name, value)) = header.split_once(':')
                                && name.eq_ignore_ascii_case("content-length")
                            {
                                content_length = value.trim().parse().unwrap();
                            }
                        }
                        let mut body = vec![0; content_length];
                        stream.read_exact(&mut body).await.unwrap();
                        let body = zstd::stream::decode_all(body.as_slice()).unwrap_or(body);
                        recorder
                            .lock()
                            .unwrap()
                            .push((path, String::from_utf8_lossy(&body).into_owned()));
                        stream
                            .get_mut()
                            .write_all(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n")
                            .await
                            .unwrap();
                    }
                });
            }
        });
        (base_url, captured)
    }

    /// Builds a metrics agent that flushes to `base_url`, with `listener` in place of the
    /// DogStatsD listener.
    fn agent_with_listener<F>(
        base_url: &str,
        listener: impl FnOnce(AggregatorHandle, CancellationToken) -> F,
    ) -> MetricsAgent
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let (aggregator, aggregator_handle) = AggregatorService::new(EMPTY_TAGS, CONTEXTS).unwrap();
        tokio::spawn(aggregator.run());
        let flusher = Flusher::new(FlusherConfig {
            api_key_factory: Arc::new(ApiKeyFactory::new("key")),
            aggregator_handle: aggregator_handle.clone(),
            metrics_intake_url_prefix: prefix_from_url(base_url),
            client: reqwest::Client::new(),
            retry_strategy: RetryStrategy::Immediate(1),
            compression_level: COMPRESSION_LEVEL,
        });
        let cancel_token = CancellationToken::new();
        let listener = tokio::spawn(listener(aggregator_handle, cancel_token.clone()));
        MetricsAgent::spawn(
            telemetry_for(base_url, "key"),
            Some(DogStatsDPipeline { listener, flusher }),
            None,
            cancel_token,
        )
    }

    fn telemetry_for(base_url: &str, api_key: &str) -> AgentTelemetry {
        AgentTelemetry::new(
            reqwest::Client::new(),
            api_key.to_string(),
            &prefix_from_url(base_url),
        )
    }

    fn metrics_budget() -> Duration {
        FlushBudget::default().metrics
    }

    fn series_bodies(captured: &Captured) -> Vec<String> {
        captured
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/api/v2/series")
            .map(|(_, body)| body.clone())
            .collect()
    }

    /// A packet still in the listener's queue when shutdown starts is flushed: the listener
    /// inserts it after the cancellation, while it drains its queue.
    #[tokio::test(flavor = "multi_thread")]
    async fn flush_at_shutdown_includes_packets_drained_after_cancellation() {
        let (base_url, captured) = start_capture_intake().await;
        let agent = agent_with_listener(&base_url, |aggregator, cancel_token| async move {
            cancel_token.cancelled().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let metric = dogstatsd::metric::parse("drained.metric:1|c").unwrap();
            aggregator.insert_batch(vec![metric]).unwrap();
        });

        agent.flush_at_shutdown(metrics_budget()).await;

        let bodies = series_bodies(&captured);
        assert!(
            bodies.iter().any(|body| body.contains("drained.metric")),
            "series bodies {bodies:?}"
        );
    }

    /// A periodic flush whose request is still in flight at shutdown does not hold back the final
    /// flush, which must finish inside the metrics budget.
    #[tokio::test(flavor = "multi_thread")]
    async fn final_flush_does_not_wait_for_an_in_flight_periodic_flush() {
        let (base_url, captured) = start_capture_intake().await;
        let mut agent = agent_with_listener(&base_url, |aggregator, _cancel_token| async move {
            let metric = dogstatsd::metric::parse("final.metric:1|c").unwrap();
            aggregator.insert_batch(vec![metric]).unwrap();
        });
        // Stands for a periodic flush waiting on its 5-second request timeout.
        agent.flush_loop = tokio::spawn(tokio::time::sleep(REQUEST_TIMEOUT));

        let outcome = run_with_budget(
            "metrics",
            metrics_budget(),
            agent.flush_at_shutdown(metrics_budget()),
        )
        .await;

        assert_eq!(outcome, FlushOutcome::Completed);
        let bodies = series_bodies(&captured);
        assert!(
            bodies.iter().any(|body| body.contains("final.metric")),
            "series bodies {bodies:?}"
        );
    }

    /// A periodic flush that finishes inside the metrics budget is waited for, so the closed
    /// buckets it carries are not lost when the process exits.
    #[tokio::test(flavor = "multi_thread")]
    async fn flush_at_shutdown_waits_for_a_periodic_flush_inside_the_budget() {
        let (base_url, _captured) = start_capture_intake().await;
        let mut agent = agent_with_listener(&base_url, |_aggregator, _cancel_token| async {});
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let periodic_flush_finished = finished.clone();
        // Stands for a periodic flush whose request takes 800 milliseconds more.
        agent.flush_loop = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(800)).await;
            periodic_flush_finished.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        agent.flush_at_shutdown(metrics_budget()).await;

        assert!(finished.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// The final flush sends the agent's own telemetry, with an empty API key and without
    /// DogStatsD, as the Go agent does.
    #[tokio::test(flavor = "multi_thread")]
    async fn flush_at_shutdown_sends_the_agent_telemetry_without_dogstatsd() {
        let (base_url, captured) = start_capture_intake().await;
        let agent = MetricsAgent::spawn(
            telemetry_for(&base_url, ""),
            None,
            None,
            CancellationToken::new(),
        );

        agent.flush_at_shutdown(metrics_budget()).await;

        let paths: Vec<String> = captured
            .lock()
            .unwrap()
            .iter()
            .map(|(path, _)| path.clone())
            .collect();
        assert!(
            paths.iter().any(|path| path == "/api/v1/check_run"),
            "paths {paths:?}"
        );
        let bodies = series_bodies(&captured);
        assert!(
            bodies
                .iter()
                .any(|body| body.contains("datadog.agent.running")),
            "series bodies {bodies:?}"
        );
    }

    /// DogStatsD metrics get the configured tags, once each, and the serverless origin product.
    #[tokio::test(flavor = "multi_thread")]
    async fn dogstatsd_metrics_get_the_tags_and_origin() {
        let (base_url, captured) = start_capture_intake().await;
        let (aggregator, aggregator_handle) = dogstatsd_aggregator(
            &[
                "_dd.datadog_init_version:1.2.3".to_string(),
                "local:true".to_string(),
            ],
            Platform::detect(|_| None, String::new).metric_origin(false),
        )
        .unwrap();
        tokio::spawn(aggregator.run());
        let flusher = build_flusher(
            &MetricsAgentSettings {
                intake_url_prefix: prefix_from_url(&base_url),
                ..default_settings()
            },
            reqwest::Client::new(),
            aggregator_handle.clone(),
        );
        let metric = dogstatsd::metric::parse("parity.count:1|c|#kind:count,local:true").unwrap();
        aggregator_handle.insert_batch(vec![metric]).unwrap();

        flusher.flush().await;

        let bodies = series_bodies(&captured);
        assert!(
            bodies.iter().any(|body| body.contains(
                r#""tags":["_dd.datadog_init_version:1.2.3","kind:count","local:true"]"#
            ) && body.contains(r#""origin_product":1,"#)),
            "series bodies {bodies:?}"
        );
    }

    /// The final flush sends the enhanced cold start and shutdown metrics without DogStatsD,
    /// under the platform's names and legacy names.
    #[tokio::test(flavor = "multi_thread")]
    async fn flush_at_shutdown_sends_the_enhanced_metrics() {
        let (base_url, captured) = start_capture_intake().await;
        let cancel_token = CancellationToken::new();
        let platform = Platform::detect(
            |name| (name == "CONTAINER_APP_NAME").then(|| "app".to_string()),
            String::new,
        );
        let enhanced = start_enhanced_metrics(
            &MetricsAgentSettings {
                intake_url_prefix: prefix_from_url(&base_url),
                ..default_settings()
            },
            reqwest::Client::new(),
            EnhancedMetricsSetup {
                profile: &platform.enhanced_metrics().unwrap(),
                origin: platform.metric_origin(true),
                sidecar: false,
                legacy_tags: &["origin:containerapp".to_string()],
            },
            cancel_token.clone(),
        );
        let agent = MetricsAgent::spawn(
            telemetry_for(&base_url, "key"),
            None,
            enhanced,
            cancel_token,
        );

        agent.flush_at_shutdown(metrics_budget()).await;

        let sketches: Vec<String> = captured
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/api/beta/sketches")
            .map(|(_, body)| body.clone())
            .collect();
        for metric in [
            "azure.app_containerapps.enhanced.cold_start",
            "azure.app_containerapps.enhanced.shutdown",
            "azure.containerapp.enhanced.cold_start",
            "azure.containerapp.enhanced.shutdown",
        ] {
            assert!(
                sketches.iter().any(|body| body.contains(metric)),
                "{metric} in sketch bodies {sketches:?}"
            );
        }
    }

    /// A listener that never stops delays the final flush by at most `LISTENER_DRAIN_TIMEOUT`.
    #[tokio::test(flavor = "multi_thread")]
    async fn flush_at_shutdown_bounds_the_wait_for_the_listener() {
        let (base_url, captured) = start_capture_intake().await;
        let agent = agent_with_listener(&base_url, |aggregator, _cancel_token| async move {
            let metric = dogstatsd::metric::parse("early.metric:1|c").unwrap();
            aggregator.insert_batch(vec![metric]).unwrap();
            std::future::pending::<()>().await;
        });

        let started = std::time::Instant::now();
        agent.flush_at_shutdown(metrics_budget()).await;

        assert!(
            started.elapsed() < LISTENER_DRAIN_TIMEOUT + Duration::from_millis(500),
            "elapsed {:?}",
            started.elapsed()
        );
        let bodies = series_bodies(&captured);
        assert!(
            bodies.iter().any(|body| body.contains("early.metric")),
            "series bodies {bodies:?}"
        );
    }
}
