// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! CPU metrics collector for Azure Functions
//!
//! This module provides functionality to read raw CPU statistics from cgroup v1 files,
//! compute the CPU usage and limit, and submit them as distribution metrics to Datadog.
//!
//! All CPU metrics are reported in nanocores (1 core = 1,000,000,000 nanocores).

use dogstatsd::aggregator_service::AggregatorHandle;
use dogstatsd::metric::{Metric, MetricValue, SortedTags};
use num_cpus;
use std::fs;
use std::io;
use tracing::{debug, error};

const CGROUP_CPU_USAGE_PATH: &str = "/sys/fs/cgroup/cpu/cpuacct.usage"; // Reports the total CPU time, in nanoseconds, consumed by all tasks in this cgroup
const CGROUP_CPUSET_CPUS_PATH: &str = "/sys/fs/cgroup/cpuset/cpuset.cpus"; // Specifies the CPUs that tasks in this cgroup are permitted to access
const CGROUP_CPU_PERIOD_PATH: &str = "/sys/fs/cgroup/cpu/cpu.cfs_period_us"; // Specifies a period of time, in microseconds, for how regularly a cgroup's access to CPU resources should be reallocated
const CGROUP_CPU_QUOTA_PATH: &str = "/sys/fs/cgroup/cpu/cpu.cfs_quota_us"; // Specifies the total amount of time, in microseconds, for which all tasks in a cgroup can run during one period

const CPU_USAGE_METRIC: &str = "azure.functions.enhanced.test.cpu.usage";
const CPU_LIMIT_METRIC: &str = "azure.functions.enhanced.test.cpu.limit";

/// Statistics from cgroup v1 files, normalized to nanoseconds
struct CgroupStats {
    total: Option<u64>,     // Cumulative CPU usage (from cpuacct.usage) in nanoseconds
    cpu_count: Option<u64>, // Number of accessible logical CPUs (from cpuset.cpus)
    scheduler_period: Option<u64>, // CFS scheduler period (from cpu.cfs_period_us) in nanoseconds
    scheduler_quota: Option<u64>, // CFS scheduler quota (from cpu.cfs_quota_us) in nanoseconds
}

/// Computed CPU total and limit metrics
struct CpuStats {
    pub total: f64,            // Cumulative CPU usage in nanoseconds
    pub limit: Option<f64>,    // CPU limit in nanocores
    pub defaulted_limit: bool, // Whether CPU limit was defaulted to host CPU count
}

fn read_cpu_stats() -> Option<CpuStats> {
    let cgroup_stats = read_cgroup_stats();
    build_cpu_stats(&cgroup_stats)
}

/// Builds CPU stats - rate and limit
fn build_cpu_stats(cgroup_stats: &CgroupStats) -> Option<CpuStats> {
    let total = cgroup_stats.total?;

    let (limit_pct, defaulted) = compute_cpu_limit_nc(cgroup_stats);

    Some(CpuStats {
        total: total as f64,
        limit: Some(limit_pct),
        defaulted_limit: defaulted,
    })
}

/// Reads raw CPU statistics from cgroup v1 files and converts to nanoseconds
fn read_cgroup_stats() -> CgroupStats {
    let total = fs::read_to_string(CGROUP_CPU_USAGE_PATH)
        .ok()
        .and_then(|contents| contents.trim().parse::<u64>().ok());
    if total.is_none() {
        debug!("Could not read CPU usage from {CGROUP_CPU_USAGE_PATH}");
    }

    let cpu_count = read_cpu_count_from_file(CGROUP_CPUSET_CPUS_PATH).ok();
    if cpu_count.is_none() {
        debug!("Could not read CPU count from {CGROUP_CPUSET_CPUS_PATH}");
    }

    let scheduler_period = fs::read_to_string(CGROUP_CPU_PERIOD_PATH)
        .ok()
        .and_then(|contents| contents.trim().parse::<u64>().map(|v| v * 1000).ok()); // Convert from microseconds to nanoseconds
    if scheduler_period.is_none() {
        debug!("Could not read scheduler period from {CGROUP_CPU_PERIOD_PATH}");
    }

    let scheduler_quota = fs::read_to_string(CGROUP_CPU_QUOTA_PATH)
        .ok()
        .and_then(|contents| {
            contents.trim().parse::<i64>().ok().and_then(|quota| {
                // Convert from microseconds to nanoseconds
                if quota == -1 {
                    debug!("CFS scheduler quota is -1, setting to None");
                    None
                } else {
                    Some((quota * 1000) as u64)
                }
            })
        });
    if scheduler_quota.is_none() {
        debug!("Could not read scheduler quota from {CGROUP_CPU_QUOTA_PATH}");
    }

    CgroupStats {
        total,
        cpu_count,
        scheduler_period,
        scheduler_quota,
    }
}

/// Reads CPU count from cpuset.cpus
///
/// The cpuset.cpus file contains a comma-separated list, with dashes to represent ranges of CPUs,
/// e.g., "0-2,16" represents CPUs 0, 1, 2, and 16
/// This function returns the count of CPUs, in this case 4.
fn read_cpu_count_from_file(path: &str) -> Result<u64, io::Error> {
    let contents = fs::read_to_string(path)?;
    let cpuset_str = contents.trim();
    if cpuset_str.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("File {path} is empty"),
        ));
    }
    debug!("Contents of {path}: {cpuset_str}");

    let mut cpu_count: u64 = 0;

    for part in cpuset_str.split(',') {
        let range: Vec<&str> = part.split('-').collect();
        if range.len() == 2 {
            // Range like "0-3"
            debug!("Range: {range:?}");
            let start: u64 = range[0].parse().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Failed to parse u64 from range {range:?}: {e}"),
                )
            })?;
            let end: u64 = range[1].parse().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Failed to parse u64 from range {range:?}: {e}"),
                )
            })?;
            cpu_count += end - start + 1;
        } else {
            // Single CPU like "2"
            debug!("Single CPU: {part}");
            cpu_count += 1;
        }
    }

    debug!("Total CPU count: {cpu_count}");
    Ok(cpu_count)
}

/// Computes the CPU limit in nanocores, with fallback to host CPU count
fn compute_cpu_limit_nc(cgroup_stats: &CgroupStats) -> (f64, bool) {
    match compute_cgroup_cpu_limit_nc(cgroup_stats) {
        Some(limit) => (limit, false),
        None => {
            let host_cpu_count = num_cpus::get() as f64;
            debug!(
                "No CPU limit found, defaulting to host CPU count: {} CPUs",
                host_cpu_count
            );
            (host_cpu_count * 1000000000.0, true) // Convert to nanocores
        }
    }
}

/// Computes the CPU limit in nanocores from cgroup statistics
/// Limit is computed using min(CPUSet, CFS CPU Quota)
fn compute_cgroup_cpu_limit_nc(cgroup_stats: &CgroupStats) -> Option<f64> {
    let mut limit_nc = None;

    if let Some(cpu_count) = cgroup_stats.cpu_count {
        let host_cpu_count = num_cpus::get() as u64;
        if cpu_count != host_cpu_count {
            let cpuset_limit_nc = cpu_count as f64 * 1000000000.0; // Convert to nanocores
            limit_nc = Some(cpuset_limit_nc);
            debug!(
                "CPU limit from cpuset: {} CPUs ({} nanocores)",
                cpu_count, cpuset_limit_nc
            );
        }
    }

    if let (Some(scheduler_quota), Some(scheduler_period)) =
        (cgroup_stats.scheduler_quota, cgroup_stats.scheduler_period)
    {
        let quota_limit_nc = 1000000000.0 * (scheduler_quota as f64 / scheduler_period as f64);
        match limit_nc {
            None => {
                limit_nc = Some(quota_limit_nc);
                debug!(
                    "limit_pct is None, setting CPU limit from cfs quota: {} nanocores",
                    quota_limit_nc
                );
            }
            Some(current_limit_nc) if quota_limit_nc < current_limit_nc => {
                limit_nc = Some(quota_limit_nc);
                debug!("CPU limit from cfs quota is less than current limit, setting CPU limit from cfs quota: {} nanocores", quota_limit_nc);
            }
            _ => {
                debug!("Keeping cpuset limit: {:?} nanocores", limit_nc);
            }
        }
    }
    limit_nc
}

pub struct CpuMetricsCollector {
    aggregator: AggregatorHandle,
    tags: Option<SortedTags>,
    last_usage_ns: f64,
    collection_interval_secs: u64,
}

impl CpuMetricsCollector {
    /// Creates a new CpuMetricsCollector
    ///
    /// # Arguments
    ///
    /// * `aggregator` - The aggregator handle to submit metrics to
    /// * `tags` - Optional tags to attach to all metrics
    /// * `last_usage_ns` - The last usage time in nanoseconds
    /// * `collection_interval_secs` - The interval in seconds to collect the metrics
    pub fn new(
        aggregator: AggregatorHandle,
        tags: Option<SortedTags>,
        last_usage_ns: f64,
        collection_interval_secs: u64,
    ) -> Self {
        Self {
            aggregator,
            tags,
            last_usage_ns,
            collection_interval_secs,
        }
    }

    pub fn collect_and_submit(&mut self) {
        if let Some(cpu_stats) = read_cpu_stats() {
            // Submit metrics
            debug!("Collected cpu stats!");
            let current_usage_ns = cpu_stats.total;
            debug!("CPU usage: {}", cpu_stats.total);
            
            // Skip first collection
            if self.last_usage_ns == -1.0 {
                debug!("First CPU collection, skipping rate computation");
                self.last_usage_ns = current_usage_ns;
                return;
            }

            let delta_ns = current_usage_ns - self.last_usage_ns as f64;
            self.last_usage_ns = current_usage_ns;

            // Divide nanoseconds delta by collection interval to get usage rate in nanocores
            let usage_rate_nc = delta_ns / self.collection_interval_secs as f64;
            debug!("Usage rate: {} nanocores/s", usage_rate_nc);

            let now = std::time::UNIX_EPOCH.elapsed()
                .map(|d| d.as_secs())
                .unwrap_or(0)
                .try_into()
                .unwrap_or(0);

            let usage_metric = Metric::new(CPU_USAGE_METRIC.into(), MetricValue::distribution(usage_rate_nc), self.tags.clone(), Some(now));

            if let Err(e) = self.aggregator.insert_batch(vec![usage_metric]) {
                error!("Failed to insert CPU usage metric: {}", e);
            }

            if let Some(limit) = cpu_stats.limit {
                debug!("CPU limit: {}", limit);
                if cpu_stats.defaulted_limit {
                    debug!("CPU limit defaulted to host CPU count");
                }
                let limit_metric = Metric::new(CPU_LIMIT_METRIC.into(), MetricValue::distribution(limit), self.tags.clone(), Some(now));
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
