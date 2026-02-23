// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! CPU metrics collector for Azure Functions
//!
//! This module provides functionality to read raw CPU statistics from cgroup v1 files,
//! compute the CPU usage and limit, and submit them as distribution metrics to Datadog.
//!
//! All CPU metrics are reported in nanocores (1 core = 1,000,000,000 nanocores).

use dogstatsd::aggregator_service::AggregatorHandle;
use dogstatsd::metric::SortedTags;
use num_cpus;
use std::fs;
use std::io;
use tracing::debug;

const CGROUP_CPU_USAGE_PATH: &str = "/sys/fs/cgroup/cpu/cpuacct.usage"; // Reports the total CPU time, in nanoseconds, consumed by all tasks in this cgroup
const CGROUP_CPUSET_CPUS_PATH: &str = "/sys/fs/cgroup/cpuset/cpuset.cpus"; // Specifies the CPUs that tasks in this cgroup are permitted to access
const CGROUP_CPU_PERIOD_PATH: &str = "/sys/fs/cgroup/cpu/cpu.cfs_period_us"; // Specifies a period of time, in microseconds, for how regularly a cgroup's access to CPU resources should be reallocated
const CGROUP_CPU_QUOTA_PATH: &str = "/sys/fs/cgroup/cpu/cpu.cfs_quota_us"; // Specifies the total amount of time, in microseconds, for which all tasks in a cgroup can run during one period

const CPU_USAGE_METRIC: &str = "azure.functions.cpu.usage";
const CPU_LIMIT_METRIC: &str = "azure.functions.cpu.limit";

/// Statistics from cgroup v1 files, normalized to nanoseconds
struct CgroupStats {
    total: Option<u64>,     // Total CPU usage (from cpuacct.usage) in nanoseconds
    cpu_count: Option<u64>, // Number of accessible logical CPUs (from cpuset.cpus)
    scheduler_period: Option<u64>, // CFS scheduler period (from cpu.cfs_period_us) in nanoseconds
    scheduler_quota: Option<u64>, // CFS scheduler quota (from cpu.cfs_quota_us) in nanoseconds
}

/// Computed CPU total and limit metrics
struct CpuStats {
    pub total: f64,            // Total CPU usage in nanoseconds
    pub limit: Option<f64>,    // CPU limit in nanoseconds
    pub defaulted_limit: bool, // Whether CPU limit was defaulted to host CPU count
}

fn read_cpu_stats() -> Option<CpuStats> {
    let cgroup_stats = read_cgroup_stats();
    build_cpu_stats(&cgroup_stats)
}

/// Builds CPU stats - rate and limit
fn build_cpu_stats(cgroup_stats: &CgroupStats) -> Option<CpuStats> {
    let total = cgroup_stats.total?;

    let (limit_pct, defaulted) = compute_cpu_limit_pct(cgroup_stats);

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

/// Computes the CPU limit percentage
fn compute_cpu_limit_pct(cgroup_stats: &CgroupStats) -> (f64, bool) {
    match compute_cgroup_cpu_limit_pct(cgroup_stats) {
        Some(limit) => (limit, false),
        None => {
            let host_cpu_count = num_cpus::get() as f64;
            debug!(
                "No CPU limit found, defaulting to host CPU count: {} CPUs",
                host_cpu_count
            );
            (host_cpu_count * 100.0, true)
        }
    }
}

/// Computes the CPU limit percentage from cgroup statistics
/// Limit is computed using min(CPUSet, CFS CPU Quota)
fn compute_cgroup_cpu_limit_pct(cgroup_stats: &CgroupStats) -> Option<f64> {
    let mut limit_pct = None;

    if let Some(cpu_count) = cgroup_stats.cpu_count {
        let host_cpu_count = num_cpus::get() as u64;
        if cpu_count != host_cpu_count {
            let cpuset_limit_pct = cpu_count as f64 * 100.0;
            limit_pct = Some(cpuset_limit_pct);
            debug!(
                "CPU limit from cpuset: {} CPUs ({}%)",
                cpu_count, cpuset_limit_pct
            );
        }
    }

    if let (Some(scheduler_quota), Some(scheduler_period)) =
        (cgroup_stats.scheduler_quota, cgroup_stats.scheduler_period)
    {
        let quota_limit_pct = 100.0 * (scheduler_quota as f64 / scheduler_period as f64);
        match limit_pct {
            None => {
                limit_pct = Some(quota_limit_pct);
                debug!(
                    "limit_pct is None, setting CPU limit from cfs quota: {}%",
                    quota_limit_pct
                );
            }
            Some(current_limit_pct) if quota_limit_pct < current_limit_pct => {
                limit_pct = Some(quota_limit_pct);
                debug!("CPU limit from cfs quota is less than current limit, setting CPU limit from cfs quota: {}%", quota_limit_pct);
            }
            _ => {
                debug!("Keeping cpuset limit: {:?}%", limit_pct);
            }
        }
    }
    limit_pct
}

pub struct CpuMetricsCollector {
    aggregator: AggregatorHandle,
    tags: Option<SortedTags>,
    last_usage_ns: i64,
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
        last_usage_ns: i64,
        collection_interval_secs: u64,
    ) -> Self {
        Self {
            aggregator,
            tags,
            last_usage_ns,
            collection_interval_secs,
        }
    }

    pub fn collect_and_submit(&self) {
        if let Some(cpu_stats) = read_cpu_stats() {
            // Submit metrics
            debug!("Collected cpu stats!");
            debug!("CPU usage: {}", cpu_stats.total);
            if let Some(limit) = cpu_stats.limit {
                debug!(
                    "CPU limit: {}%, defaulted: {}",
                    limit, cpu_stats.defaulted_limit
                );
            } else {
                debug!("CPU limit: None, defaulted: {}", cpu_stats.defaulted_limit);
            }
            debug!("Submitting CPU metrics!");
        } else {
            debug!("Skipping CPU metrics collection - could not find data to generate CPU usage and limit enhanced metrics");
        }
    }
}
