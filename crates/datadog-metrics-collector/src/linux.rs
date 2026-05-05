// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! CPU metrics collector for Azure Functions (Linux)
//!
//! This module provides functionality to read raw CPU statistics from cgroup v1 files
//! and compute the CPU usage in Linux environments.
//!
//! All CPU metrics are reported in nanocores (1 core = 1,000,000,000 nanocores).

use crate::cpu::{CpuStats, CpuStatsReader};
use std::fs;
use tracing::debug;

const CGROUP_CPU_USAGE_PATH: &str = "/sys/fs/cgroup/cpu/cpuacct.usage"; // Reports the total CPU time, in nanoseconds, consumed by all tasks in this cgroup

/// Statistics from cgroup v1 files, normalized to nanoseconds
struct CgroupStats {
    total: Option<u64>, // Cumulative CPU usage (from cpuacct.usage) in nanoseconds
}

pub struct LinuxCpuStatsReader;

impl CpuStatsReader for LinuxCpuStatsReader {
    fn read(&self) -> Option<CpuStats> {
        let cgroup_stats = read_cgroup_stats();
        build_cpu_stats(&cgroup_stats)
    }
}

fn build_cpu_stats(cgroup_stats: &CgroupStats) -> Option<CpuStats> {
    let total = cgroup_stats.total?;

    Some(CpuStats { total })
}

/// Reads raw CPU statistics from cgroup v1 files and converts to nanoseconds
fn read_cgroup_stats() -> CgroupStats {
    let total = fs::read_to_string(CGROUP_CPU_USAGE_PATH)
        .ok()
        .and_then(|contents| contents.trim().parse::<u64>().ok());
    if total.is_none() {
        debug!("Could not read CPU usage from {CGROUP_CPU_USAGE_PATH}");
    }

    CgroupStats { total }
}
