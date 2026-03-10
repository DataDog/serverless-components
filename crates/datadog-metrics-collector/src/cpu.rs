// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! CPU metrics collector for Azure Functions
//!
//! This module provides OS-agnostic CPU stats collection, CPU usage
//! and limit computation, and metrics submission to Datadog.
//!
//! All CPU metrics are reported in nanocores (1 core = 1,000,000,000 nanocores).

use dogstatsd::aggregator::AggregatorHandle;
use dogstatsd::metric::{Metric, MetricValue, SortedTags};
use tracing::{debug, error};

const CPU_USAGE_METRIC: &str = "azure.functions.enhanced.cpu.usage";
const CPU_LIMIT_METRIC: &str = "azure.functions.enhanced.cpu.limit";

/// Computed CPU total and limit metrics
pub struct CpuStats {
    pub total: u64,            // Cumulative CPU usage in nanoseconds
    pub limit: Option<f64>,    // CPU limit in nanocores
    pub defaulted_limit: bool, // Whether CPU limit was defaulted to host CPU count
}

pub trait CpuStatsReader {
    fn read(&self) -> Option<CpuStats>;
}

pub struct CpuMetricsCollector {
    reader: Box<dyn CpuStatsReader>,
    aggregator: AggregatorHandle,
    tags: Option<SortedTags>,
    last_usage_ns: Option<u64>,
    last_collection_time: std::time::Instant,
}

impl CpuMetricsCollector {
    /// Creates a new CpuMetricsCollector
    ///
    /// # Arguments
    ///
    /// * `aggregator` - The aggregator handle to submit metrics to
    /// * `tags` - Optional tags to attach to all metrics
    pub fn new(aggregator: AggregatorHandle, tags: Option<SortedTags>) -> Self {
        #[cfg(feature = "windows-enhanced-metrics")]
        let reader: Box<dyn CpuStatsReader> = Box::new(crate::windows::WindowsCpuStatsReader);
        #[cfg(not(feature = "windows-enhanced-metrics"))]
        let reader: Box<dyn CpuStatsReader> = Box::new(crate::linux::LinuxCpuStatsReader);
        Self {
            reader,
            aggregator,
            tags,
            last_usage_ns: None,
            last_collection_time: std::time::Instant::now(),
        }
    }

    pub fn collect_and_submit(&mut self) {
        if let Some(cpu_stats) = self.reader.read() {
            // Submit metrics
            debug!("Collected CPU stats!");
            let current_usage_ns = cpu_stats.total;
            let now_instant = std::time::Instant::now();

            // Skip first collection
            let Some(last_usage_ns) = self.last_usage_ns else {
                debug!("First CPU collection, skipping interval");
                self.last_usage_ns = Some(current_usage_ns);
                self.last_collection_time = now_instant;
                return;
            };

            if current_usage_ns < last_usage_ns {
                debug!("Current CPU usage is less than last usage, skipping interval");
                self.last_usage_ns = Some(current_usage_ns);
                self.last_collection_time = now_instant;
                return;
            }
            let delta_ns = (current_usage_ns - last_usage_ns) as f64;
            self.last_usage_ns = Some(current_usage_ns);
            let elapsed_secs = now_instant
                .duration_since(self.last_collection_time)
                .as_secs_f64();
            self.last_collection_time = now_instant;

            // Divide nanoseconds delta by elapsed time to get usage rate in nanocores
            let usage_rate_nc = delta_ns / elapsed_secs;

            let now = std::time::UNIX_EPOCH
                .elapsed()
                .map(|d| d.as_secs())
                .unwrap_or(0)
                .try_into()
                .unwrap_or(0);

            let usage_metric = Metric::new(
                CPU_USAGE_METRIC.into(),
                MetricValue::distribution(usage_rate_nc),
                self.tags.clone(),
                Some(now),
            );

            if let Err(e) = self.aggregator.insert_batch(vec![usage_metric]) {
                error!("Failed to insert CPU usage metric: {}", e);
            }

            if let Some(limit) = cpu_stats.limit {
                if cpu_stats.defaulted_limit {
                    debug!("CPU limit defaulted to host CPU count");
                }
                let limit_metric = Metric::new(
                    CPU_LIMIT_METRIC.into(),
                    MetricValue::distribution(limit),
                    self.tags.clone(),
                    Some(now),
                );
                if let Err(e) = self.aggregator.insert_batch(vec![limit_metric]) {
                    error!("Failed to insert CPU limit metric: {}", e);
                }
            }
            debug!("Submitting CPU metrics!");
        } else {
            debug!("Skipping CPU metrics collection - could not find data to generate CPU usage and limit enhanced metrics");
        }
    }
}
