// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! The enhanced metrics, as Go serverless-init sends them: the cold start and shutdown
//! distributions, also under the platform's legacy names, and every 3 seconds the usage gauge
//! (`instance` or `replica`) and the CPU usage and limit distributions of the process's own
//! cgroup.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use dogstatsd::aggregator::AggregatorHandle;
use dogstatsd::flusher::Flusher;
use dogstatsd::metric::{Metric, MetricValue, SortedTags};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::cgroup_cpu::{self, CgroupVersion, CpuStats};
use crate::platform::{self, EnhancedMetricsProfile};

/// The names after the platform's prefix and `enhanced.`.
const COLD_START: &str = "cold_start";
const SHUTDOWN: &str = "shutdown";
const CPU_USAGE: &str = "cpu.usage";
const CPU_LIMIT: &str = "cpu.limit";

const COLLECTION_INTERVAL: Duration = Duration::from_secs(3);
/// How long shutdown waits for the collector's final collection.
const COLLECTOR_STOP_TIMEOUT: Duration = Duration::from_millis(500);

/// The tags of the enhanced metrics, and the tags of the instance metric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnhancedMetricTags {
    pub metric: String,
    pub usage_metric: String,
}

/// The enhanced metric tags, as Go serverless-init builds them. Each set is a map in which a
/// later source replaces a tag of an earlier one: the unified service tags, then for the enhanced
/// metrics other than the usage metric the configured tags, then the platform's tags of
/// `profile`, the version and the mode.
pub fn tags(
    profile: &EnhancedMetricsProfile,
    sidecar: bool,
    version: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> EnhancedMetricTags {
    let unified = platform::unified_service_tags(&lookup);
    let mode_tags = [
        (platform::version_tag_key(sidecar), version.to_string()),
        ("sidecar", sidecar.to_string()),
    ];
    let with_platform = |mut tags: BTreeMap<String, String>, platform_tags: &BTreeMap<_, _>| {
        tags.extend(platform_tags.clone());
        for (key, value) in &mode_tags {
            tags.insert((*key).to_string(), value.clone());
        }
        platform::join_tags(&tags)
    };

    let mut metric = unified.clone();
    metric.extend(platform::configured_tags(&lookup));

    EnhancedMetricTags {
        metric: with_platform(metric, &profile.base_tags),
        usage_metric: with_platform(unified, &profile.usage_tags),
    }
}

/// The enhanced metrics' aggregator and flusher, and the collector that feeds them.
pub struct EnhancedMetrics {
    aggregator: AggregatorHandle,
    flusher: Flusher,
    names: Names,
    metric_tags: Option<SortedTags>,
    /// The tags of the legacy metrics: the DogStatsD tags of the platform, without
    /// `DD_DOGSTATSD_TAGS`.
    legacy_tags: Option<SortedTags>,
    collector_loop: Option<JoinHandle<()>>,
}

/// The metric names of a platform.
#[derive(Debug, Clone)]
struct Names {
    prefix: &'static str,
    legacy_prefix: Option<&'static str>,
    usage_metric_suffix: &'static str,
}

impl Names {
    fn enhanced(&self, name: &str) -> String {
        format!("{}enhanced.{name}", self.prefix)
    }

    /// The distribution of 1 under the name, and under the legacy name when there is one.
    fn distributions_of_one(
        &self,
        name: &str,
        tags: &Option<SortedTags>,
        legacy_tags: &Option<SortedTags>,
    ) -> Vec<Metric> {
        let legacy = self
            .legacy_prefix
            .map(|prefix| distribution_of_one(&format!("{prefix}enhanced.{name}"), legacy_tags));
        std::iter::once(distribution_of_one(&self.enhanced(name), tags))
            .chain(legacy)
            .collect()
    }
}

impl EnhancedMetrics {
    /// Sends the cold start metric and starts collecting every `COLLECTION_INTERVAL` until
    /// `cancel_token` is cancelled. There is no collector when the process has no cgroup.
    pub fn start(
        aggregator: AggregatorHandle,
        flusher: Flusher,
        profile: &EnhancedMetricsProfile,
        sidecar: bool,
        legacy_tags: &[String],
        cancel_token: CancellationToken,
    ) -> Self {
        let tags = tags(profile, sidecar, platform::VERSION, |name| {
            std::env::var(name).ok()
        });
        let names = Names {
            prefix: profile.prefix,
            legacy_prefix: profile.legacy_prefix,
            usage_metric_suffix: profile.usage_metric_suffix,
        };
        let metric_tags = parse_tags(&tags.metric);
        let legacy_tags = parse_tags(&legacy_tags.join(","));
        insert(
            &aggregator,
            names.distributions_of_one(COLD_START, &metric_tags, &legacy_tags),
        );
        let collector_loop = match CpuCollector::new(
            PathBuf::from(cgroup_cpu::CGROUP_ROOT),
            names.clone(),
            metric_tags.clone(),
            parse_tags(&tags.usage_metric),
        ) {
            Some(mut collector) => {
                info!("Enhanced metrics collector started");
                // Collected here, so a shutdown right after the start still has two collections.
                insert(&aggregator, collector.collect());
                Some(tokio::spawn(collect_periodically(
                    collector,
                    aggregator.clone(),
                    cancel_token,
                )))
            }
            None => {
                warn!(
                    "Failed to initialize enhanced metrics collector: no cgroup found at {}",
                    cgroup_cpu::CGROUP_ROOT
                );
                None
            }
        };
        Self {
            aggregator,
            flusher,
            names,
            metric_tags,
            legacy_tags,
            collector_loop,
        }
    }

    pub fn flusher(&self) -> &Flusher {
        &self.flusher
    }

    /// Sends the shutdown metric, waits for the collector's final collection, which starts once
    /// the caller cancels the collector, and flushes every bucket.
    pub async fn flush_at_shutdown(self) {
        insert(
            &self.aggregator,
            self.names
                .distributions_of_one(SHUTDOWN, &self.metric_tags, &self.legacy_tags),
        );
        if let Some(collector_loop) = self.collector_loop {
            match tokio::time::timeout(COLLECTOR_STOP_TIMEOUT, collector_loop).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => error!("The enhanced metrics collector task failed: {error}"),
                Err(_) => warn!(
                    "The enhanced metrics collector did not stop within \
                     {COLLECTOR_STOP_TIMEOUT:?}; flushing without its final collection"
                ),
            }
        }
        self.flusher.flush().await;
    }
}

/// Collects every `COLLECTION_INTERVAL` after the first collection, and a last time when
/// `cancel_token` is cancelled.
async fn collect_periodically(
    mut collector: CpuCollector,
    aggregator: AggregatorHandle,
    cancel_token: CancellationToken,
) {
    let mut ticker = interval(COLLECTION_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // The first tick completes at once; `EnhancedMetrics::start` made that collection.
    ticker.tick().await;
    loop {
        tokio::select! {
            biased;
            _ = cancel_token.cancelled() => {
                insert(&aggregator, collector.collect());
                return;
            }
            _ = ticker.tick() => insert(&aggregator, collector.collect()),
        }
    }
}

/// Reads the cgroup's CPU statistics at each collection.
struct CpuCollector {
    root: PathBuf,
    names: Names,
    version: CgroupVersion,
    host_cpu_count: u64,
    rate: CpuUsageRate,
    metric_tags: Option<SortedTags>,
    usage_metric_tags: Option<SortedTags>,
}

impl CpuCollector {
    fn new(
        root: PathBuf,
        names: Names,
        metric_tags: Option<SortedTags>,
        usage_metric_tags: Option<SortedTags>,
    ) -> Option<Self> {
        let version = cgroup_cpu::detect_version(&root)?;
        debug!("Using cgroup version {version:?}");
        Some(Self {
            root,
            names,
            version,
            host_cpu_count: cgroup_cpu::host_cpu_count(),
            rate: CpuUsageRate::default(),
            metric_tags,
            usage_metric_tags,
        })
    }

    /// The usage gauge, and the CPU usage and limit.
    fn collect(&mut self) -> Vec<Metric> {
        let at = Instant::now();
        let timestamp = now_seconds();
        let mut metrics = vec![Metric::new(
            self.names
                .enhanced(self.names.usage_metric_suffix)
                .as_str()
                .into(),
            MetricValue::gauge(1.0),
            self.usage_metric_tags.clone(),
            Some(timestamp),
        )];
        let stats = cgroup_cpu::read_stats(&self.root, self.version);
        if stats.is_none() {
            debug!("The cgroup has no CPU controller; sending the CPU enhanced metrics as 0");
        }
        let cpu = cpu_metrics(&mut self.rate, stats, at, self.host_cpu_count);
        metrics.extend(cpu.into_iter().map(|(name, value)| {
            Metric::new(
                self.names.enhanced(name).as_str().into(),
                MetricValue::distribution(value),
                self.metric_tags.clone(),
                Some(timestamp),
            )
        }));
        metrics
    }
}

/// The CPU usage, which needs a previous reading, and the CPU limit. Without CPU statistics both
/// are 0 and the previous reading is kept, as Go sends the zero values of its unset statistics.
fn cpu_metrics(
    rate: &mut CpuUsageRate,
    stats: Option<CpuStats>,
    at: Instant,
    host_cpu_count: u64,
) -> Vec<(&'static str, f64)> {
    let Some(stats) = stats else {
        return vec![(CPU_USAGE, 0.0), (CPU_LIMIT, 0.0)];
    };
    let usage = rate.next(&stats, at).map(|usage| (CPU_USAGE, usage));
    let limit = (
        CPU_LIMIT,
        cgroup_cpu::limit_nanocores(&stats, host_cpu_count),
    );
    usage.into_iter().chain([limit]).collect()
}

/// Computes the CPU usage between collections.
#[derive(Debug, Default)]
struct CpuUsageRate {
    previous: Option<(Option<u64>, Instant)>,
}

impl CpuUsageRate {
    /// The CPU usage in nanocores since the previous reading, which `stats` replaces.
    fn next(&mut self, stats: &CpuStats, at: Instant) -> Option<f64> {
        let (previous_total, previous_at) = self.previous.replace((stats.total_nanoseconds, at))?;
        cgroup_cpu::usage_nanocores(
            stats.total_nanoseconds,
            previous_total,
            at.saturating_duration_since(previous_at).as_secs_f64(),
        )
    }
}

fn distribution_of_one(name: &str, tags: &Option<SortedTags>) -> Metric {
    Metric::new(
        name.into(),
        MetricValue::distribution(1.0),
        tags.clone(),
        Some(now_seconds()),
    )
}

fn insert(aggregator: &AggregatorHandle, metrics: Vec<Metric>) {
    if let Err(error) = aggregator.insert_batch(metrics) {
        error!("Failed to insert the enhanced metrics: {error}");
    }
}

fn parse_tags(tags: &str) -> Option<SortedTags> {
    SortedTags::parse(tags)
        .map_err(|error| warn!("Failed to parse the enhanced metric tags {tags:?}: {error}"))
        .ok()
}

fn now_seconds() -> i64 {
    std::time::UNIX_EPOCH
        .elapsed()
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
        .try_into()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_table() {
        type Variables = &'static [(&'static str, &'static str)];
        let profile = |variables: Variables| {
            let lookup = move |name: &str| {
                variables
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            };
            platform::Platform::detect(lookup, || "host-1".to_string())
                .enhanced_metrics()
                .expect("supported platform")
        };
        let local = profile(&[]);
        let container_app = profile(&[
            ("CONTAINER_APP_NAME", "app"),
            ("CONTAINER_APP_REPLICA_NAME", "replica-1"),
        ]);
        type Case<'a> = (
            &'a EnhancedMetricsProfile,
            bool,
            Variables,
            &'a str,
            &'a str,
        );
        let cases: &[Case] = &[
            (
                &local,
                false,
                &[],
                "datadog_init_version:1.2.3,local:true,sidecar:false",
                "datadog_init_version:1.2.3,instance:host-1,local:true,sidecar:false",
            ),
            (
                &local,
                true,
                &[],
                "datadog_sidecar_version:1.2.3,local:true,sidecar:true",
                "datadog_sidecar_version:1.2.3,instance:host-1,local:true,sidecar:true",
            ),
            // The unified service tags go on both, the configured tags only on the first.
            (
                &local,
                false,
                &[
                    ("DD_ENV", "Prod"),
                    ("DD_SERVICE", "Parity-App"),
                    ("DD_VERSION", "2"),
                    ("DD_TAGS", "Team:Parity,tier:one other:x"),
                    ("DD_EXTRA_TAGS", "extra:1 no-value"),
                ],
                "datadog_init_version:1.2.3,env:prod,extra:1,local:true,other:x,\
                 service:parity-app,sidecar:false,team:parity,tier:one,version:2",
                "datadog_init_version:1.2.3,env:prod,instance:host-1,local:true,\
                 service:parity-app,sidecar:false,version:2",
            ),
            // Configured tags replace the unified service tags, and the platform's tags replace
            // both.
            (
                &local,
                false,
                &[
                    ("DD_SERVICE", "from-service"),
                    ("DD_TAGS", "service:from-tags,local:false,sidecar:true"),
                ],
                "datadog_init_version:1.2.3,local:true,service:from-tags,sidecar:false",
                "datadog_init_version:1.2.3,instance:host-1,local:true,service:from-service,\
                 sidecar:false",
            ),
            (
                &container_app,
                false,
                &[("DD_SERVICE", "svc")],
                "datadog_init_version:1.2.3,name:app,origin:containerapp,region:unknown,\
                 resource_group:unknown,revisionname:unknown,service:svc,sidecar:false,\
                 subscription_id:unknown",
                "datadog_init_version:1.2.3,name:app,origin:containerapp,region:unknown,\
                 replica:replica-1,resource_group:unknown,revisionname:unknown,service:svc,\
                 sidecar:false,subscription_id:unknown",
            ),
        ];
        for (profile, sidecar, variables, metric, usage_metric) in cases {
            let lookup = |name: &str| {
                variables
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            };
            assert_eq!(
                tags(profile, *sidecar, "1.2.3", lookup),
                EnhancedMetricTags {
                    metric: metric.to_string(),
                    usage_metric: usage_metric.to_string(),
                },
                "{profile:?}, sidecar {sidecar}, variables {variables:?}"
            );
        }
    }

    /// The CPU metrics of successive collections. Without CPU statistics both are 0, as in Go,
    /// and the previous reading is kept.
    #[test]
    fn cpu_metrics_table() {
        let start = Instant::now();
        let reading = |total_nanoseconds| {
            Some(CpuStats {
                total_nanoseconds: Some(total_nanoseconds),
                ..CpuStats::default()
            })
        };
        let mut rate = CpuUsageRate::default();
        let steps = [
            (reading(1_000_000_000), 0, vec![(CPU_LIMIT, 4e9)]),
            (None, 1, vec![(CPU_USAGE, 0.0), (CPU_LIMIT, 0.0)]),
            (
                reading(3_000_000_000),
                2,
                vec![(CPU_USAGE, 1e9), (CPU_LIMIT, 4e9)],
            ),
        ];
        for (stats, seconds, expected) in steps {
            let at = start + Duration::from_secs(seconds);
            assert_eq!(
                cpu_metrics(&mut rate, stats, at, 4),
                expected,
                "at {seconds} seconds"
            );
        }
    }
}
