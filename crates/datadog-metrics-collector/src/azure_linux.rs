// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! CPU metrics collector for Azure Functions (Linux)
//!
//! This module provides functionality to read raw CPU statistics from cgroup v1 files
//! and compute the CPU usage in Linux environments.
//!
//! Raw CPU time is read in nanoseconds. The CPU metric is reported in nanocores (1 core = 1,000,000,000 nanocores).

use crate::azure_cpu::{CpuStats, CpuStatsReader};
use std::fs;
use tracing::debug;

const CGROUP_CPU_USAGE_PATH: &str = "/sys/fs/cgroup/cpu/cpuacct.usage"; // Reports the total CPU time, in nanoseconds, consumed by all tasks in this cgroup

pub struct LinuxCpuStatsReader;

impl CpuStatsReader for LinuxCpuStatsReader {
    fn read(&self) -> Option<CpuStats> {
        read_cpuacct_usage_from_path(CGROUP_CPU_USAGE_PATH)
    }
}

fn read_cpuacct_usage_from_path(path: &str) -> Option<CpuStats> {
    match fs::read_to_string(path)
        .ok()
        .and_then(|contents| contents.trim().parse::<u64>().ok())
    {
        Some(total) => Some(CpuStats { total }),
        None => {
            debug!("Could not read CPU usage from {:?}", path);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_path(file: &str) -> String {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push(file);
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn test_reads_valid_value() {
        let result =
            read_cpuacct_usage_from_path(&fixture_path("tests/cgroup/valid_cpuacct_usage"));
        assert_eq!(result.map(|s| s.total), Some(12345678));
    }

    #[test]
    fn test_returns_none_for_missing_file() {
        let result = read_cpuacct_usage_from_path(&fixture_path("tests/cgroup/nonexistent_file"));
        assert!(result.is_none());
    }

    #[test]
    fn test_returns_none_for_invalid_content() {
        let result = read_cpuacct_usage_from_path(&fixture_path("tests/cgroup/invalid_content"));
        assert!(result.is_none());
    }

    #[test]
    fn test_trims_whitespace() {
        let result = read_cpuacct_usage_from_path(&fixture_path("tests/cgroup/whitespace"));
        assert_eq!(result.map(|s| s.total), Some(99999));
    }
}
