// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! CPU metrics collector for Azure Functions (Linux)
//!
//! This module provides functionality to read raw CPU statistics from cgroup v1 files
//! and compute the CPU usage in Linux environments.
//!
//! All CPU metrics are reported in nanocores (1 core = 1,000,000,000 nanocores).

use crate::azure_cpu::{CpuStats, CpuStatsReader};
use std::fs;
use tracing::debug;

const CGROUP_CPU_USAGE_PATH: &str = "/sys/fs/cgroup/cpu/cpuacct.usage"; // Reports the total CPU time, in nanoseconds, consumed by all tasks in this cgroup

pub struct LinuxCpuStatsReader;

impl CpuStatsReader for LinuxCpuStatsReader {
    fn read(&self) -> Option<CpuStats> {
        match fs::read_to_string(CGROUP_CPU_USAGE_PATH)
            .ok()
            .and_then(|contents| contents.trim().parse::<u64>().ok())
        {
            Some(total) => Some(CpuStats { total }),
            None => {
                debug!("Could not read CPU usage from {CGROUP_CPU_USAGE_PATH}");
                None
            }
        }
    }
}
