// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! The CPU statistics of the process's own cgroup. As the Go agent does in a container, they are
//! read at the root of the cgroup mount, which holds the container's own cgroup.

use std::fs;
use std::path::Path;

/// Where the cgroup file system is mounted.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgroupVersion {
    V1,
    V2,
}

/// The CPU statistics of a cgroup. A field is `None` when its file is missing or not valid.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CpuStats {
    /// The cumulative CPU time, in nanoseconds.
    pub total_nanoseconds: Option<u64>,
    /// The number of CPUs in the cpuset.
    pub cpuset_count: Option<u64>,
    /// The scheduler quota and period, in microseconds. There is no quota when it is unlimited.
    pub quota: Option<u64>,
    pub period: Option<u64>,
}

/// Version 2 when the unified hierarchy is mounted at `root`, else version 1 when its `cpu` and
/// `cpuacct` controllers are.
pub fn detect_version(root: &Path) -> Option<CgroupVersion> {
    if root.join("cgroup.controllers").is_file() {
        Some(CgroupVersion::V2)
    } else if root.join("cpu").is_dir() && root.join("cpuacct").is_dir() {
        Some(CgroupVersion::V1)
    } else {
        None
    }
}

/// Reads the statistics, or `None` when the cgroup has no CPU controller.
pub fn read_stats(root: &Path, version: CgroupVersion) -> Option<CpuStats> {
    match version {
        CgroupVersion::V2 => read_v2_stats(root),
        CgroupVersion::V1 => (root.join("cpu").is_dir() && root.join("cpuacct").is_dir())
            .then(|| read_v1_stats(root)),
    }
}

fn read_v2_stats(root: &Path) -> Option<CpuStats> {
    let controllers = read(&root.join("cgroup.controllers"))?;
    let activated = |name: &str| controllers.split_whitespace().any(|c| c == name);
    if !activated("cpu") {
        return None;
    }
    let total_nanoseconds = read(&root.join("cpu.stat")).and_then(|contents| {
        contents
            .lines()
            .find_map(|line| match line.split_once(' ') {
                Some(("usage_usec", value)) => value.trim().parse::<u64>().ok(),
                _ => None,
            })
    });
    let total_nanoseconds = total_nanoseconds.map(|usec| usec.saturating_mul(1_000));
    let (quota, period) = read(&root.join("cpu.max"))
        .and_then(|contents| {
            let (quota, period) = contents.trim().split_once(' ')?;
            let period = period.parse::<u64>().ok()?;
            Some((quota.parse::<u64>().ok(), Some(period)))
        })
        .unwrap_or_default();
    let cpuset_count = if activated("cpuset") {
        read(&root.join("cpuset.cpus.effective")).and_then(|contents| cpuset_count(&contents))
    } else {
        None
    };
    Some(CpuStats {
        total_nanoseconds,
        cpuset_count,
        quota,
        period,
    })
}

fn read_v1_stats(root: &Path) -> CpuStats {
    let number = |path: &str| read(&root.join(path)).and_then(|c| c.trim().parse::<i64>().ok());
    CpuStats {
        total_nanoseconds: number("cpuacct/cpuacct.usage").and_then(|v| u64::try_from(v).ok()),
        cpuset_count: read(&root.join("cpuset/cpuset.cpus"))
            .and_then(|contents| cpuset_count(&contents)),
        // A quota of -1 is unlimited.
        quota: number("cpu/cpu.cfs_quota_us").and_then(|v| u64::try_from(v).ok()),
        period: number("cpu/cpu.cfs_period_us").and_then(|v| u64::try_from(v).ok()),
    }
}

fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// The number of CPUs in a cpuset list such as `0-3,6`, or `None` when it is empty.
fn cpuset_count(contents: &str) -> Option<u64> {
    let count: u64 = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .flat_map(|line| line.split(','))
        .map(|range| match range.split_once('-') {
            Some((first, last)) => match (first.parse::<u64>(), last.parse::<u64>()) {
                (Ok(first), Ok(last)) if last >= first => last - first + 1,
                _ => 0,
            },
            None => 1,
        })
        .sum();
    (count != 0).then_some(count)
}

/// The CPU limit in nanocores: the smaller of the cpuset and the scheduler quota, else the host's
/// CPU count. A cpuset of every host CPU is no limit.
#[allow(clippy::cast_precision_loss)]
pub fn limit_nanocores(stats: &CpuStats, host_cpu_count: u64) -> f64 {
    let cpuset = stats
        .cpuset_count
        .filter(|count| *count != host_cpu_count)
        .map(|count| count as f64);
    let quota = match (stats.quota, stats.period) {
        (Some(quota), Some(period)) if period > 0 => Some(quota as f64 / period as f64),
        _ => None,
    };
    let cores = match (cpuset, quota) {
        (Some(cpuset), Some(quota)) => cpuset.min(quota),
        (Some(limit), None) | (None, Some(limit)) => limit,
        (None, None) => host_cpu_count as f64,
    };
    cores * 1e9
}

/// The CPU usage in nanocores between two readings `elapsed_seconds` apart, or `None` when it
/// cannot be computed, as on the first reading.
pub fn usage_nanocores(
    current: Option<u64>,
    previous: Option<u64>,
    elapsed_seconds: f64,
) -> Option<f64> {
    let (current, previous) = (current?, previous?);
    if elapsed_seconds <= 0.0 || current <= previous {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    Some((current - previous) as f64 / elapsed_seconds)
}

/// The number of CPUs the host has online.
pub fn host_cpu_count() -> u64 {
    // SAFETY: sysconf only reads a system setting.
    let count = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    u64::try_from(count)
        .ok()
        .filter(|count| *count > 0)
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A cgroup root under the temporary directory with the given files.
    fn cgroup_root(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("datadog-init-cgroup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (path, contents) in files {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn read_stats_table() {
        type Case = (
            &'static str,
            &'static [(&'static str, &'static str)],
            Option<CgroupVersion>,
            Option<CpuStats>,
        );
        let cases: &[Case] = &[
            (
                "v2-limited",
                &[
                    ("cgroup.controllers", "cpuset cpu io memory pids\n"),
                    (
                        "cpu.stat",
                        "usage_usec 1500\nuser_usec 1000\nsystem_usec 500\n",
                    ),
                    ("cpu.max", "50000 100000\n"),
                    ("cpuset.cpus.effective", "0-3,6\n"),
                ],
                Some(CgroupVersion::V2),
                Some(CpuStats {
                    total_nanoseconds: Some(1_500_000),
                    cpuset_count: Some(5),
                    quota: Some(50_000),
                    period: Some(100_000),
                }),
            ),
            (
                "v2-unlimited",
                &[
                    ("cgroup.controllers", "cpu io memory pids\n"),
                    ("cpu.stat", "usage_usec 7\n"),
                    ("cpu.max", "max 100000\n"),
                    ("cpuset.cpus.effective", "0-3\n"),
                ],
                Some(CgroupVersion::V2),
                Some(CpuStats {
                    total_nanoseconds: Some(7_000),
                    cpuset_count: None,
                    quota: None,
                    period: Some(100_000),
                }),
            ),
            (
                "v2-without-cpu-controller",
                &[
                    ("cgroup.controllers", "io memory pids\n"),
                    ("cpu.stat", "usage_usec 7\n"),
                ],
                Some(CgroupVersion::V2),
                None,
            ),
            (
                "v1-limited",
                &[
                    ("cpuacct/cpuacct.usage", "123456789\n"),
                    ("cpu/cpu.cfs_quota_us", "200000\n"),
                    ("cpu/cpu.cfs_period_us", "100000\n"),
                    ("cpuset/cpuset.cpus", "0-1\n"),
                ],
                Some(CgroupVersion::V1),
                Some(CpuStats {
                    total_nanoseconds: Some(123_456_789),
                    cpuset_count: Some(2),
                    quota: Some(200_000),
                    period: Some(100_000),
                }),
            ),
            (
                "v1-unlimited",
                &[
                    ("cpuacct/cpuacct.usage", "42\n"),
                    ("cpu/cpu.cfs_quota_us", "-1\n"),
                    ("cpu/cpu.cfs_period_us", "100000\n"),
                ],
                Some(CgroupVersion::V1),
                Some(CpuStats {
                    total_nanoseconds: Some(42),
                    cpuset_count: None,
                    quota: None,
                    period: Some(100_000),
                }),
            ),
            ("none", &[], None, None),
        ];
        for (name, files, expected_version, expected_stats) in cases {
            let root = cgroup_root(name, files);
            let version = detect_version(&root);
            assert_eq!(version, *expected_version, "case {name}");
            let stats = version.and_then(|version| read_stats(&root, version));
            assert_eq!(stats, *expected_stats, "case {name}");
            fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn limit_nanocores_table() {
        let stats = |cpuset_count, quota, period| CpuStats {
            total_nanoseconds: None,
            cpuset_count,
            quota,
            period,
        };
        let cases = [
            (stats(None, None, None), 4, 4_000_000_000.0),
            (stats(Some(4), None, None), 4, 4_000_000_000.0),
            (stats(Some(2), None, None), 4, 2_000_000_000.0),
            (stats(None, Some(50_000), Some(100_000)), 4, 500_000_000.0),
            (
                stats(Some(2), Some(300_000), Some(100_000)),
                4,
                2_000_000_000.0,
            ),
            (
                stats(Some(2), Some(150_000), Some(100_000)),
                4,
                1_500_000_000.0,
            ),
            (
                stats(Some(4), Some(600_000), Some(100_000)),
                4,
                6_000_000_000.0,
            ),
            (stats(None, Some(50_000), None), 4, 4_000_000_000.0),
        ];
        for (stats, host_cpu_count, expected) in cases {
            assert_eq!(
                limit_nanocores(&stats, host_cpu_count),
                expected,
                "stats {stats:?}"
            );
        }
    }

    #[test]
    fn usage_nanocores_table() {
        let cases = [
            (
                Some(3_000_000_000),
                Some(1_000_000_000),
                2.0,
                Some(1_000_000_000.0),
            ),
            (Some(3_000_000_000), None, 2.0, None),
            (None, Some(1_000_000_000), 2.0, None),
            (Some(1_000_000_000), Some(1_000_000_000), 2.0, None),
            (Some(1_000_000_000), Some(3_000_000_000), 2.0, None),
            (Some(3_000_000_000), Some(1_000_000_000), 0.0, None),
        ];
        for (current, previous, elapsed_seconds, expected) in cases {
            assert_eq!(
                usage_nanocores(current, previous, elapsed_seconds),
                expected,
                "current {current:?}, previous {previous:?}, elapsed {elapsed_seconds}"
            );
        }
    }
}
