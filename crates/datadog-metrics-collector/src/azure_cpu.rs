// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! CPU metrics collector for Azure Functions
//!
//! This module provides OS-agnostic CPU stats collection, CPU usage
//! computation, and metrics submission to Datadog.
//!
//! All CPU metrics are reported in nanocores (1 core = 1,000,000,000 nanocores).

use dogstatsd::aggregator::AggregatorHandle;
use dogstatsd::metric::{Metric, MetricValue, SortedTags};
use tracing::{debug, error};

const CPU_USAGE_METRIC: &str = "azure.functions.enhanced.cpu.usage";

/// Computed CPU total usage metric
pub struct CpuStats {
    pub total: u64, // Cumulative CPU usage in nanoseconds
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
        #[cfg(all(windows, feature = "windows-enhanced-metrics"))]
        let reader: Box<dyn CpuStatsReader> = Box::new(crate::azure_windows::WindowsCpuStatsReader);
        #[cfg(not(windows))]
        let reader: Box<dyn CpuStatsReader> = Box::new(crate::azure_linux::LinuxCpuStatsReader);
        Self {
            reader,
            aggregator,
            tags,
            last_usage_ns: None,
            last_collection_time: std::time::Instant::now(),
        }
    }

    pub fn collect_and_submit(&mut self) {
        let Some(cpu_stats) = self.reader.read() else {
            debug!(
                "Skipping CPU enhanced metrics collection - could not find data to generate CPU usage metrics"
            );
            return;
        };

        let current_usage_ns = cpu_stats.total;
        let now_instant = std::time::Instant::now();
        let now = std::time::UNIX_EPOCH
            .elapsed()
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .try_into()
            .unwrap_or(0);

        // Skip first collection
        let Some(last_usage_ns) = self.last_usage_ns else {
            debug!("First CPU collection, skipping interval");
            self.last_usage_ns = Some(current_usage_ns);
            self.last_collection_time = now_instant;
            return;
        };

        let elapsed_secs = now_instant
            .duration_since(self.last_collection_time)
            .as_secs_f64();

        // Update state so the next collection always compares against the most recent reading, even if the interval is skipped
        self.last_usage_ns = Some(current_usage_ns);
        self.last_collection_time = now_instant;

        let Some(usage_rate_nc) =
            compute_usage_rate_nc(current_usage_ns, last_usage_ns, elapsed_secs)
        else {
            return;
        };

        let usage_metric = Metric::new(
            CPU_USAGE_METRIC.into(),
            MetricValue::distribution(usage_rate_nc),
            self.tags.clone(),
            Some(now),
        );

        if let Err(e) = self.aggregator.insert_batch(vec![usage_metric]) {
            error!("Failed to insert CPU usage metric: {}", e);
        }
    }
}

fn compute_usage_rate_nc(
    current_usage_ns: u64,
    last_usage_ns: u64,
    elapsed_secs: f64,
) -> Option<f64> {
    if current_usage_ns < last_usage_ns {
        debug!("Current CPU usage is less than last usage, skipping interval");
        return None;
    }
    if elapsed_secs <= 0.0 {
        debug!("Elapsed time is less than or equal to 0, skipping interval");
        return None;
    }
    let delta_ns = (current_usage_ns - last_usage_ns) as f64;
    Some(delta_ns / elapsed_secs)
}

// For testing only since CpuMetricsCollector::new() hardcodes the reader based on OS
#[cfg(test)]
impl CpuMetricsCollector {
    pub(crate) fn with_reader(
        reader: Box<dyn CpuStatsReader>,
        aggregator: AggregatorHandle,
        tags: Option<SortedTags>,
    ) -> Self {
        Self {
            reader,
            aggregator,
            tags,
            last_usage_ns: None,
            last_collection_time: std::time::Instant::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dogstatsd::aggregator::AggregatorService;
    use dogstatsd::metric::EMPTY_TAGS;
    use std::cell::Cell;

    #[test]
    fn test_normal_delta() {
        assert_eq!(
            compute_usage_rate_nc(2_000_000_000, 1_000_000_000, 1.0),
            Some(1_000_000_000.0)
        );
    }

    #[test]
    fn test_current_usage_less_than_last_usage_returns_none() {
        assert!(compute_usage_rate_nc(1_000_000_000, 2_000_000_000, 1.0).is_none());
    }

    #[test]
    fn test_zero_elapsed_time_returns_none() {
        assert!(compute_usage_rate_nc(2_000_000_000, 1_000_000_000, 0.0).is_none());
    }

    struct MockCpuStatsReader {
        idx: Cell<usize>,
        values: Vec<Option<u64>>,
    }

    impl MockCpuStatsReader {
        fn new(values: Vec<Option<u64>>) -> Self {
            Self {
                idx: Cell::new(0),
                values,
            }
        }
    }

    impl CpuStatsReader for MockCpuStatsReader {
        fn read(&self) -> Option<CpuStats> {
            let i = self.idx.get();
            self.idx.set(i + 1);
            self.values
                .get(i)
                .and_then(|v| v.map(|total| CpuStats { total }))
        }
    }

    #[tokio::test]
    async fn test_first_collection_skipped() {
        let (service, handle) =
            AggregatorService::new(EMPTY_TAGS, 1000).expect("Aggregator creation failed");
        let task = tokio::spawn(service.run());

        let mut collector = CpuMetricsCollector::with_reader(
            Box::new(MockCpuStatsReader::new(vec![Some(1_000_000_000)])),
            handle.clone(),
            None,
        );
        collector.collect_and_submit();

        let response = handle.flush().await.expect("flush failed");
        assert!(
            response.distributions.is_empty(),
            "Expected no batches flushed on first collection, got {:?}",
            response.distributions.len()
        );

        handle.shutdown().expect("Shutdown failed");
        task.await.expect("Service task panicked");
    }

    #[tokio::test]
    async fn test_second_collection_submits_metric() {
        let (service, handle) =
            AggregatorService::new(EMPTY_TAGS, 1000).expect("Aggregator creation failed");
        let task = tokio::spawn(service.run());

        let mut collector = CpuMetricsCollector::with_reader(
            Box::new(MockCpuStatsReader::new(vec![
                Some(1_000_000_000),
                Some(2_000_000_000),
            ])),
            handle.clone(),
            None,
        );
        collector.collect_and_submit();
        collector.collect_and_submit();

        let response = handle.flush().await.expect("Flush failed");
        assert_eq!(
            response.distributions.len(),
            1,
            "Expected 1 batch to be flushed, got {:?}",
            response.distributions.len()
        );
        assert_eq!(
            response.distributions[0].sketches.len(),
            1,
            "Expected 1 metric, got {:?}",
            response.distributions[0].sketches.len()
        );

        handle.shutdown().expect("Shutdown failed");
        task.await.expect("Service task panicked");
    }

    #[tokio::test]
    async fn test_current_usage_less_than_last_usage_skips_interval() {
        let (service, handle) =
            AggregatorService::new(EMPTY_TAGS, 1000).expect("Aggregator creation failed");
        let task = tokio::spawn(service.run());

        let mut collector = CpuMetricsCollector::with_reader(
            Box::new(MockCpuStatsReader::new(vec![
                Some(2_000_000_000),
                Some(1_000_000_000),
            ])),
            handle.clone(),
            None,
        );
        collector.collect_and_submit();
        collector.collect_and_submit();

        let response = handle.flush().await.expect("Flush failed");
        assert!(
            response.distributions.is_empty(),
            "Expected no batches flushed when current usage is less than last usage, got {:?}",
            response.distributions.len()
        );

        handle.shutdown().expect("Shutdown failed");
        task.await.expect("Service task panicked");
    }
}
