//! /proc filesystem parsing utilities for Linux system metrics.
//!
//! This module provides functions to parse various `/proc` files to collect
//! system metrics needed for Lambda enhanced metrics and monitoring. All
//! functions are Unix-specific and will not work on Windows.
//!
//! # Supported Metrics
//!
//! - **Process enumeration**: List all PIDs from `/proc`
//! - **Network statistics**: RX/TX bytes from `/proc/net/dev`
//! - **CPU time**: User, system, and idle time from `/proc/stat`
//! - **System uptime**: Uptime in seconds from `/proc/uptime`
//! - **File descriptors**: Limits and usage from `/proc/<pid>/limits` and `/proc/<pid>/fd/`
//! - **Threads**: Limits and usage from `/proc/<pid>/limits` and `/proc/<pid>/task/`
//!
//! # /proc Filesystem Structure
//!
//! ```text
//! /proc/
//!   ├── <pid>/              # Per-process directories (numeric PIDs)
//!   │   ├── limits          # Resource limits (ulimit values)
//!   │   ├── fd/             # Open file descriptors (symlinks)
//!   │   └── task/           # Thread directories
//!   ├── net/dev             # Network interface statistics
//!   ├── stat                # CPU time statistics
//!   └── uptime              # System uptime and idle time
//! ```
//!
//! # Lambda Environment
//!
//! In AWS Lambda, these metrics are used to:
//! - Monitor network usage across Lambda network interfaces
//! - Track CPU utilization for cost optimization
//! - Detect file descriptor leaks
//! - Monitor thread count for concurrency issues
//!
//! # Example
//!
//! ```rust,ignore
//! use datadog_agent_native::proc::{get_network_data, get_cpu_data, get_uptime};
//!
//! // Query network statistics
//! let network = get_network_data()?;
//! println!("RX: {} bytes, TX: {} bytes", network.rx_bytes, network.tx_bytes);
//!
//! // Query CPU time
//! let cpu = get_cpu_data()?;
//! println!("User time: {}ms", cpu.total_user_time_ms);
//!
//! // Query system uptime
//! let uptime = get_uptime()?;
//! println!("Uptime: {}ms", uptime);
//! ```

pub mod clock;
pub mod constants;
pub mod hostname;

use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, BufRead},
};

use constants::{
    LAMBDA_NETWORK_INTERFACE, LAMBDA_RUNTIME_NETWORK_INTERFACE, PROC_NET_DEV_PATH, PROC_PATH,
    PROC_STAT_PATH, PROC_UPTIME_PATH,
};
use regex::Regex;
use tracing::{debug, trace};

/// Enumerates all process IDs (PIDs) from `/proc`.
///
/// This is a convenience wrapper around [`get_pid_list_from_path`] that uses
/// the default `/proc` path.
///
/// # Returns
///
/// A vector of PIDs (as `i64`) corresponding to all running processes.
/// Returns an empty vector if `/proc` cannot be read.
///
/// # Example
///
/// ```rust,ignore
/// let pids = get_pid_list();
/// println!("Found {} processes", pids.len());
/// ```
#[must_use]
pub fn get_pid_list() -> Vec<i64> {
    get_pid_list_from_path(PROC_PATH)
}

/// Enumerates all process IDs (PIDs) from a specified directory path.
///
/// This function reads a directory (typically `/proc`) and returns all
/// subdirectories whose names are valid integers (which represent PIDs).
///
/// # Arguments
///
/// * `path` - Directory path to scan (e.g., "/proc")
///
/// # Returns
///
/// A vector of PIDs found in the directory. Non-directory entries and
/// directories with non-numeric names are ignored.
///
/// # Implementation Details
///
/// - Only directories are considered (not files or symlinks)
/// - Directory names must parse as valid `i64` integers
/// - Read errors on individual entries are silently ignored
/// - Returns empty vector if directory cannot be read
///
/// # Example
///
/// ```rust,ignore
/// let pids = get_pid_list_from_path("/proc");
/// // pids might be: [1, 42, 1337, 9999]
/// ```
pub fn get_pid_list_from_path(path: &str) -> Vec<i64> {
    let mut pids = Vec::<i64>::new();

    // Decision: Return empty vector rather than error if /proc can't be read
    // This allows the agent to continue without PID-based metrics
    let Ok(entries) = fs::read_dir(path) else {
        debug!("Could not list /proc files");
        return pids;
    };

    // Decision: Use filter_map to ignore errors and non-PID entries
    pids.extend(entries.filter_map(|entry| {
        entry.ok().and_then(|dir_entry| {
            // Decision: Only consider directories (PIDs are directories in /proc)
            if dir_entry.file_type().ok()?.is_dir() {
                // Decision: Parse directory name as i64 (PID), ignore non-numeric names
                // This filters out /proc entries like "self", "cpuinfo", "meminfo"
                dir_entry.file_name().to_str()?.parse::<i64>().ok()
            } else {
                None
            }
        })
    }));

    pids
}

/// Network interface statistics (RX/TX bytes).
///
/// This struct holds cumulative network traffic statistics for Lambda
/// network interfaces. Values represent total bytes since system boot.
///
/// # Fields
///
/// * `rx_bytes` - Total bytes received (RX) on the interface
/// * `tx_bytes` - Total bytes transmitted (TX) on the interface
///
/// # Lambda Network Interfaces
///
/// Lambda uses specific network interfaces:
/// - `vinternal_1`: Lambda service communication (invocations)
/// - `vint_runtime`: Runtime/extension API communication
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct NetworkData {
    /// Total bytes received (RX) on the network interface.
    pub rx_bytes: f64,
    /// Total bytes transmitted (TX) on the network interface.
    pub tx_bytes: f64,
}

/// Queries network statistics for Lambda network interfaces from `/proc/net/dev`.
///
/// This is a convenience wrapper around [`get_network_data_from_path`] that uses
/// the default `/proc/net/dev` path.
///
/// # Returns
///
/// * `Ok(NetworkData)` - RX/TX bytes for Lambda network interfaces
/// * `Err` - If file cannot be read or Lambda interfaces are not found
///
/// # Lambda Network Interfaces
///
/// Searches for either:
/// - `vinternal_1`: Lambda service interface
/// - `vint_runtime`: Runtime API interface
///
/// # Example
///
/// ```rust,ignore
/// let network = get_network_data()?;
/// println!("Received: {} bytes, Transmitted: {} bytes",
///          network.rx_bytes, network.tx_bytes);
/// ```
pub fn get_network_data() -> Result<NetworkData, io::Error> {
    get_network_data_from_path(PROC_NET_DEV_PATH)
}

/// Queries network statistics from a specified `/proc/net/dev` file.
///
/// This function parses the `/proc/net/dev` file format to extract RX/TX
/// bytes for Lambda-specific network interfaces.
///
/// # Arguments
///
/// * `path` - Path to the `/proc/net/dev` file
///
/// # Returns
///
/// * `Ok(NetworkData)` - RX/TX bytes for the first matching Lambda interface
/// * `Err` - If file cannot be read, format is invalid, or no Lambda interface found
///
/// # /proc/net/dev Format
///
/// ```text
/// Inter-|   Receive                                                |  Transmit
///  face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
/// vinternal_1:     180      10    0    0    0     0          0         0      254      10    0    0    0     0       0          0
/// ```
///
/// This function extracts:
/// - Column 1 (after interface name): RX bytes
/// - Column 9: TX bytes (after skipping 7 RX columns)
///
/// # Example
///
/// ```rust,ignore
/// let network = get_network_data_from_path("/proc/net/dev")?;
/// ```
fn get_network_data_from_path(path: &str) -> Result<NetworkData, io::Error> {
    let file = File::open(path)?;
    let reader = io::BufReader::new(file);

    // Decision: Scan all lines looking for Lambda network interfaces
    for line in reader.lines() {
        let line = line?;
        let mut values = line.split_whitespace();

        // Decision: Check interface name for Lambda-specific prefixes
        // Lambda uses either vinternal_1 or vint_runtime depending on the communication type
        if values.next().is_some_and(|interface_name| {
            interface_name.starts_with(LAMBDA_NETWORK_INTERFACE)
                || interface_name.starts_with(LAMBDA_RUNTIME_NETWORK_INTERFACE)
        }) {
            // Read the value for received bytes (first column after interface name)
            let rx_bytes: Option<f64> = values.next().and_then(|s| s.parse().ok());

            // Skip over the next 7 values representing other RX metrics
            // (packets, errs, drop, fifo, frame, compressed, multicast)
            // Then read the value for bytes transmitted (column 9)
            // Decision: Use nth(7) to skip 7 values and get the 8th (TX bytes)
            let tx_bytes: Option<f64> = values.nth(7).and_then(|s| s.parse().ok());

            // Decision: Require both RX and TX to be valid numbers
            match (rx_bytes, tx_bytes) {
                (Some(rx_val), Some(tx_val)) => {
                    return Ok(NetworkData {
                        rx_bytes: rx_val,
                        tx_bytes: tx_val,
                    });
                }
                // Decision: Return error if values are malformed (not valid numbers)
                (_, _) => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "Network data not found",
                    ));
                }
            }
        }
    }

    // Decision: Return error if no Lambda interface found in file
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "Network data not found",
    ))
}

/// CPU time statistics for system and individual cores.
///
/// This struct holds cumulative CPU time since system boot, broken down by
/// time spent in user mode, system mode, and idle state. All times are in
/// milliseconds.
///
/// # Fields
///
/// * `total_user_time_ms` - Total CPU time in user mode (all cores combined)
/// * `total_system_time_ms` - Total CPU time in system/kernel mode (all cores)
/// * `total_idle_time_ms` - Total CPU idle time (all cores combined)
/// * `individual_cpu_idle_times` - Per-core idle times (e.g., "cpu0" → 91880.0)
///
/// # Usage
///
/// These values are cumulative counters. To calculate CPU utilization over
/// a time period, take two samples and compute the difference:
///
/// ```text
/// cpu_used = (user_delta + system_delta)
/// cpu_total = (user_delta + system_delta + idle_delta)
/// utilization = cpu_used / cpu_total * 100%
/// ```
///
/// # Example
///
/// ```rust,ignore
/// let cpu = get_cpu_data()?;
/// println!("User: {}ms, System: {}ms, Idle: {}ms",
///          cpu.total_user_time_ms, cpu.total_system_time_ms, cpu.total_idle_time_ms);
/// println!("Cores: {}", cpu.individual_cpu_idle_times.len());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct CPUData {
    /// Total CPU time spent in user mode (milliseconds, all cores).
    pub total_user_time_ms: f64,
    /// Total CPU time spent in system/kernel mode (milliseconds, all cores).
    pub total_system_time_ms: f64,
    /// Total CPU idle time (milliseconds, all cores).
    pub total_idle_time_ms: f64,
    /// Per-core idle times (milliseconds), keyed by core name (e.g., "cpu0").
    pub individual_cpu_idle_times: HashMap<String, f64>,
}

/// Queries CPU time statistics from `/proc/stat`.
///
/// This is a convenience wrapper around [`get_cpu_data_from_path`] that uses
/// the default `/proc/stat` path.
///
/// # Returns
///
/// * `Ok(CPUData)` - CPU time statistics for all cores
/// * `Err` - If file cannot be read, format is invalid, or CLK_TCK cannot be determined
///
/// # Example
///
/// ```rust,ignore
/// let cpu = get_cpu_data()?;
/// let total_cpu_time = cpu.total_user_time_ms + cpu.total_system_time_ms + cpu.total_idle_time_ms;
/// let utilization = (cpu.total_user_time_ms + cpu.total_system_time_ms) / total_cpu_time * 100.0;
/// println!("CPU utilization: {:.2}%", utilization);
/// ```
pub fn get_cpu_data() -> Result<CPUData, io::Error> {
    get_cpu_data_from_path(PROC_STAT_PATH)
}

/// Queries CPU time statistics from a specified `/proc/stat` file.
///
/// This function parses the `/proc/stat` file to extract CPU time values
/// and convert them from clock ticks to milliseconds.
///
/// # Arguments
///
/// * `path` - Path to the `/proc/stat` file
///
/// # Returns
///
/// * `Ok(CPUData)` - CPU time statistics
/// * `Err` - If file cannot be read, format is invalid, or no per-core data found
///
/// # /proc/stat Format
///
/// ```text
/// cpu  2337 0 188 17838 0 0 0 0 0 0
/// cpu0 1188 0 94 9188 0 0 0 0 0 0
/// cpu1 1149 0 94 8649 0 0 0 0 0 0
/// ```
///
/// Format (values in clock ticks):
/// - Column 1: user time
/// - Column 2: nice time (skipped)
/// - Column 3: system time
/// - Column 4: idle time
/// - Columns 5-10: other states (ignored)
///
/// # Clock Tick Conversion
///
/// CPU time values are converted from clock ticks (USER_HZ) to milliseconds:
/// ```text
/// time_ms = (ticks / CLK_TCK) × 1000
/// ```
///
/// # Example
///
/// ```rust,ignore
/// let cpu = get_cpu_data_from_path("/proc/stat")?;
/// ```
fn get_cpu_data_from_path(path: &str) -> Result<CPUData, io::Error> {
    let file = File::open(path)?;
    let reader = io::BufReader::new(file);

    let mut cpu_data = CPUData {
        total_user_time_ms: 0.0,
        total_system_time_ms: 0.0,
        total_idle_time_ms: 0.0,
        individual_cpu_idle_times: HashMap::new(),
    };

    // SC_CLK_TCK is the system clock frequency in ticks per second
    // We'll use this to convert CPU times from clock ticks (USER_HZ) to milliseconds
    // Decision: Query CLK_TCK once for all conversions (typically 100 Hz)
    let clktck = clock::get_clk_tck()? as f64;

    // Decision: Parse all lines to extract both aggregate and per-core CPU data
    for line in reader.lines() {
        let line = line?;
        let mut values = line.split_whitespace();

        // Decision: Check label to determine if this is aggregate or per-core data
        if let Some(label) = values.next() {
            // Decision: "cpu" (no number) is the aggregate line for all cores
            if label == "cpu" {
                // Parse CPU times for total user, system, and idle
                let user: Option<f64> = values.next().and_then(|s| s.parse().ok());
                // Decision: Skip "nice" time (not needed for our metrics)
                values.next();
                let system: Option<f64> = values.next().and_then(|s| s.parse().ok());
                let idle: Option<f64> = values.next().and_then(|s| s.parse().ok());

                // Decision: Require all three values to be valid numbers
                match (user, system, idle) {
                    (Some(user_val), Some(system_val), Some(idle_val)) => {
                        // Convert from clock ticks to milliseconds: (ticks / CLK_TCK) × 1000
                        // Decision: Use floating point division to preserve precision
                        cpu_data.total_user_time_ms = (user_val / clktck) * 1000.0;
                        cpu_data.total_system_time_ms = (system_val / clktck) * 1000.0;
                        cpu_data.total_idle_time_ms = (idle_val / clktck) * 1000.0;
                    }
                    // Decision: Return error if aggregate CPU line is malformed
                    (_, _, _) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Failed to parse CPU data",
                        ));
                    }
                }
            // Decision: "cpu0", "cpu1", etc. are per-core lines
            } else if label.starts_with("cpu") {
                // Parse per-core idle times (used to calculate per-core utilization)
                // Skip the first three values (user, nice, system) and get the 4th value (idle)
                // Decision: Use nth(3) to skip 3 values and get the 4th (idle time)
                let idle: Option<f64> = values.nth(3).and_then(|s| s.parse().ok());

                // Decision: Require idle value to be a valid number
                match idle {
                    Some(idle_val) => {
                        // Convert from clock ticks to milliseconds
                        // Decision: Store per-core data in HashMap keyed by core name
                        cpu_data
                            .individual_cpu_idle_times
                            .insert(label.to_string(), (idle_val / clktck) * 1000.0);
                    }
                    // Decision: Return error if any per-core line is malformed
                    None => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Failed to parse per-core CPU data",
                        ));
                    }
                }
            }
        }
    }

    // Decision: Require at least one per-core entry (verify file is valid)
    if cpu_data.individual_cpu_idle_times.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Per-core CPU data not found",
        ));
    }

    Ok(cpu_data)
}

/// Queries system uptime from `/proc/uptime`.
///
/// This is a convenience wrapper around [`get_uptime_from_path`] that uses
/// the default `/proc/uptime` path.
///
/// # Returns
///
/// * `Ok(uptime_ms)` - System uptime in milliseconds since boot
/// * `Err` - If file cannot be read or format is invalid
///
/// # Example
///
/// ```rust,ignore
/// let uptime = get_uptime()?;
/// let uptime_seconds = uptime / 1000.0;
/// println!("System has been up for {:.2} seconds", uptime_seconds);
/// ```
pub fn get_uptime() -> Result<f64, io::Error> {
    get_uptime_from_path(PROC_UPTIME_PATH)
}

/// Queries system uptime from a specified `/proc/uptime` file.
///
/// This function parses the `/proc/uptime` file to extract the system uptime
/// since boot, converting from seconds to milliseconds.
///
/// # Arguments
///
/// * `path` - Path to the `/proc/uptime` file
///
/// # Returns
///
/// * `Ok(uptime_ms)` - System uptime in milliseconds
/// * `Err` - If file cannot be read, format is invalid, or file is empty
///
/// # /proc/uptime Format
///
/// ```text
/// 3213103123.00 6426206246.00
/// ```
///
/// Format:
/// - First value: System uptime in seconds (since boot)
/// - Second value: Total idle time in seconds (sum of all cores)
///
/// This function returns the first value (uptime) converted to milliseconds.
///
/// # Example
///
/// ```rust,ignore
/// let uptime = get_uptime_from_path("/proc/uptime")?;
/// // uptime might be 3213103123000.0 (milliseconds)
/// ```
fn get_uptime_from_path(path: &str) -> Result<f64, io::Error> {
    let file = File::open(path)?;
    let reader = io::BufReader::new(file);

    // Decision: Read only the first line (/proc/uptime is a single-line file)
    if let Some(line) = reader.lines().next() {
        let line = line?;
        let mut values = line.split_whitespace();

        // Parse both values to validate file format
        let uptime: Option<f64> = values.next().and_then(|s| s.parse().ok());
        let idle: Option<f64> = values.next().and_then(|s| s.parse().ok());

        // Decision: Require both values to be present (validate file format)
        // even though we only return the uptime value
        match (uptime, idle) {
            (Some(uptime_val), Some(_idle_val)) => {
                // Convert from seconds to milliseconds
                // Decision: Multiply by 1000 to match other time metrics (all in ms)
                return Ok(uptime_val * 1000.0);
            }
            // Decision: Return error if file is malformed
            (_, _) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Failed to parse uptime data",
                ));
            }
        }
    }

    // Decision: Return error if file is empty
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "Uptime data not found",
    ))
}

/// Queries the minimum file descriptor soft limit across all processes.
///
/// This is a convenience wrapper around [`get_fd_max_data_from_path`] that uses
/// the default `/proc` path.
///
/// # Arguments
///
/// * `pids` - List of process IDs to check
///
/// # Returns
///
/// The minimum file descriptor soft limit found across all processes.
/// Returns `LAMBDA_FILE_DESCRIPTORS_DEFAULT_LIMIT` (1024) if no limits can be read.
///
/// # Example
///
/// ```rust,ignore
/// let pids = get_pid_list();
/// let fd_max = get_fd_max_data(&pids);
/// println!("File descriptor limit: {}", fd_max);
/// ```
#[must_use]
pub fn get_fd_max_data(pids: &[i64]) -> f64 {
    get_fd_max_data_from_path(PROC_PATH, pids)
}

/// Queries the minimum file descriptor soft limit from `/proc/<pid>/limits` files.
///
/// This function reads the `/proc/<pid>/limits` file for each process to extract
/// the "Max open files" soft limit, returning the minimum value found.
///
/// # Arguments
///
/// * `path` - Base path to `/proc` directory
/// * `pids` - List of process IDs to check
///
/// # Returns
///
/// The minimum file descriptor soft limit across all processes. Returns
/// `LAMBDA_FILE_DESCRIPTORS_DEFAULT_LIMIT` (1024) as a fallback if no limits
/// can be read or all processes fail to open.
///
/// # /proc/<pid>/limits Format
///
/// ```text
/// Limit                     Soft Limit           Hard Limit           Units
/// Max open files            1024                 4096                 files
/// Max processes             1024                 1024                 processes
/// ```
///
/// This function extracts the "Soft Limit" column for the "Max open files" row.
///
/// # Lambda Behavior
///
/// In Lambda, the soft limit is typically 1024 (default). This function returns
/// the minimum across all processes to represent the most restrictive limit.
///
/// # Example
///
/// ```rust,ignore
/// let pids = vec![1, 42, 1337];
/// let fd_max = get_fd_max_data_from_path("/proc", &pids);
/// ```
fn get_fd_max_data_from_path(path: &str, pids: &[i64]) -> f64 {
    // Decision: Start with Lambda default and take minimum across all processes
    let mut fd_max = constants::LAMBDA_FILE_DESCRIPTORS_DEFAULT_LIMIT;

    // Regex to capture the soft limit value (first numeric value after "Max open files")
    // Decision: Compile regex once outside loop for performance
    let re = Regex::new(r"^Max open files\s+(\d+)").expect("Failed to create regex");

    // Decision: Check all processes and take the minimum limit found
    for &pid in pids {
        let limits_path = format!("{path}/{pid}/limits");
        // Decision: Skip processes we can't read (may have terminated or no permission)
        let Ok(file) = File::open(&limits_path) else {
            continue;
        };

        let reader = io::BufReader::new(file);
        // Decision: Use map_while to stop on first read error
        for line in reader.lines().map_while(Result::ok) {
            // Decision: Check if line matches "Max open files" pattern
            if let Some(line_items) = re.captures(&line) {
                // Decision: Parse the captured soft limit value
                if let Ok(fd_max_pid) = line_items[1].parse() {
                    // Decision: Take minimum to represent most restrictive limit
                    fd_max = fd_max.min(fd_max_pid);
                } else {
                    debug!("File descriptor max data not found in file {}", limits_path);
                }
                // Decision: Break after finding the line (don't need to read rest of file)
                break;
            }
        }
    }

    fd_max
}

/// Queries the total number of open file descriptors across all processes.
///
/// This is a convenience wrapper around [`get_fd_use_data_from_path`] that uses
/// the default `/proc` path.
///
/// # Arguments
///
/// * `pids` - List of process IDs to check
///
/// # Returns
///
/// The total count of open file descriptors across all processes.
///
/// # Example
///
/// ```rust,ignore
/// let pids = get_pid_list();
/// let fd_use = get_fd_use_data(&pids);
/// println!("Open file descriptors: {}", fd_use);
/// ```
#[must_use]
pub fn get_fd_use_data(pids: &[i64]) -> f64 {
    get_fd_use_data_from_path(PROC_PATH, pids)
}

/// Queries the total number of open file descriptors from `/proc/<pid>/fd/` directories.
///
/// This function counts the number of entries in each process's `/proc/<pid>/fd/`
/// directory, which contains symlinks to all open file descriptors.
///
/// # Arguments
///
/// * `path` - Base path to `/proc` directory
/// * `pids` - List of process IDs to check
///
/// # Returns
///
/// The total count of open file descriptors across all processes.
/// Processes that can't be read are silently skipped.
///
/// # /proc/<pid>/fd/ Structure
///
/// ```text
/// /proc/1337/fd/
///   ├── 0 -> /dev/null
///   ├── 1 -> /dev/null
///   ├── 2 -> /dev/null
///   ├── 3 -> socket:[12345]
///   └── 4 -> /var/log/app.log
/// ```
///
/// This function counts the number of entries in this directory (5 in this example).
///
/// # Example
///
/// ```rust,ignore
/// let pids = vec![1, 42, 1337];
/// let fd_use = get_fd_use_data_from_path("/proc", &pids);
/// // fd_use might be 15 (total across all processes)
/// ```
fn get_fd_use_data_from_path(path: &str, pids: &[i64]) -> f64 {
    let mut fd_use = 0;

    // Decision: Sum file descriptor counts across all processes
    for &pid in pids {
        let fd_path = format!("{path}/{pid}/fd");
        // Decision: Skip processes we can't read (may have terminated or no permission)
        let Ok(files) = fs::read_dir(&fd_path) else {
            trace!(
                "File descriptor use data not found in path {} with pid {}",
                fd_path,
                pid
            );
            continue;
        };
        // Decision: Count directory entries (each entry is an open fd)
        let count = files.count();
        fd_use += count;
    }

    // Decision: Convert to f64 to match metric type (all metrics use f64)
    fd_use as f64
}

/// Queries the minimum process/thread soft limit across all processes.
///
/// This is a convenience wrapper around [`get_threads_max_data_from_path`] that uses
/// the default `/proc` path.
///
/// # Arguments
///
/// * `pids` - List of process IDs to check
///
/// # Returns
///
/// The minimum process/thread soft limit found across all processes.
/// Returns `LAMBDA_EXECUTION_PROCESSES_DEFAULT_LIMIT` (1024) if no limits can be read.
///
/// # Note
///
/// The "Max processes" limit actually controls the maximum number of threads
/// (not processes) that a user can create. This is the `RLIMIT_NPROC` resource limit.
///
/// # Example
///
/// ```rust,ignore
/// let pids = get_pid_list();
/// let threads_max = get_threads_max_data(&pids);
/// println!("Thread limit: {}", threads_max);
/// ```
#[must_use]
pub fn get_threads_max_data(pids: &[i64]) -> f64 {
    get_threads_max_data_from_path(PROC_PATH, pids)
}

/// Queries the minimum process/thread soft limit from `/proc/<pid>/limits` files.
///
/// This function reads the `/proc/<pid>/limits` file for each process to extract
/// the "Max processes" soft limit, returning the minimum value found.
///
/// # Arguments
///
/// * `path` - Base path to `/proc` directory
/// * `pids` - List of process IDs to check
///
/// # Returns
///
/// The minimum process/thread soft limit across all processes. Returns
/// `LAMBDA_EXECUTION_PROCESSES_DEFAULT_LIMIT` (1024) as a fallback if no limits
/// can be read or all processes fail to open.
///
/// # /proc/<pid>/limits Format
///
/// ```text
/// Limit                     Soft Limit           Hard Limit           Units
/// Max processes             1024                 1024                 processes
/// ```
///
/// This function extracts the "Soft Limit" column for the "Max processes" row.
///
/// # Lambda Behavior
///
/// In Lambda, the soft limit is typically 1024 (default). This function returns
/// the minimum across all processes to represent the most restrictive limit.
///
/// # Example
///
/// ```rust,ignore
/// let pids = vec![1, 42, 1337];
/// let threads_max = get_threads_max_data_from_path("/proc", &pids);
/// ```
fn get_threads_max_data_from_path(path: &str, pids: &[i64]) -> f64 {
    // Decision: Start with Lambda default and take minimum across all processes
    let mut threads_max = constants::LAMBDA_EXECUTION_PROCESSES_DEFAULT_LIMIT;

    // Regex to capture the soft limit value (first numeric value after "Max processes")
    // Decision: Compile regex once outside loop for performance
    let re = Regex::new(r"^Max processes\s+(\d+)").expect("Failed to create regex");

    // Decision: Check all processes and take the minimum limit found
    for &pid in pids {
        let limits_path = format!("{path}/{pid}/limits");
        // Decision: Skip processes we can't read (may have terminated or no permission)
        let Ok(file) = File::open(&limits_path) else {
            continue;
        };

        let reader = io::BufReader::new(file);
        // Decision: Use map_while to stop on first read error
        for line in reader.lines().map_while(Result::ok) {
            // Decision: Check if line matches "Max processes" pattern
            if let Some(line_items) = re.captures(&line) {
                // Decision: Parse the captured soft limit value
                if let Ok(threads_max_pid) = line_items[1].parse() {
                    // Decision: Take minimum to represent most restrictive limit
                    threads_max = threads_max.min(threads_max_pid);
                } else {
                    debug!("Threads max data not found in file {}", limits_path);
                }
                // Decision: Break after finding the line (don't need to read rest of file)
                break;
            }
        }
    }

    threads_max
}

/// Queries the total number of threads across all processes.
///
/// This is a convenience wrapper around [`get_threads_use_data_from_path`] that uses
/// the default `/proc` path.
///
/// # Arguments
///
/// * `pids` - List of process IDs to check
///
/// # Returns
///
/// * `Ok(thread_count)` - Total number of threads across all processes
/// * `Err` - If any process's task directory cannot be read
///
/// # Example
///
/// ```rust,ignore
/// let pids = get_pid_list();
/// let threads_use = get_threads_use_data(&pids)?;
/// println!("Total threads: {}", threads_use);
/// ```
pub fn get_threads_use_data(pids: &[i64]) -> Result<f64, io::Error> {
    get_threads_use_data_from_path(PROC_PATH, pids)
}

/// Queries the total number of threads from `/proc/<pid>/task/` directories.
///
/// This function counts the number of directory entries in each process's
/// `/proc/<pid>/task/` directory, where each subdirectory represents a thread.
///
/// # Arguments
///
/// * `path` - Base path to `/proc` directory
/// * `pids` - List of process IDs to check
///
/// # Returns
///
/// * `Ok(thread_count)` - Total number of threads across all processes
/// * `Err` - If any process's task directory cannot be read
///
/// # /proc/<pid>/task/ Structure
///
/// ```text
/// /proc/1337/task/
///   ├── 1337/    # Main thread (same as PID)
///   ├── 1338/    # Worker thread 1
///   ├── 1339/    # Worker thread 2
///   └── 1340/    # Worker thread 3
/// ```
///
/// This function counts the number of directory entries (4 in this example).
///
/// # Error Handling
///
/// Unlike other resource functions, this returns an error if any process's
/// task directory cannot be read, rather than silently skipping it. This is
/// because thread count is a critical metric and missing data could indicate
/// a problem.
///
/// # Example
///
/// ```rust,ignore
/// let pids = vec![1, 42, 1337];
/// let threads_use = get_threads_use_data_from_path("/proc", &pids)?;
/// // threads_use might be 20 (total across all processes)
/// ```
fn get_threads_use_data_from_path(path: &str, pids: &[i64]) -> Result<f64, io::Error> {
    let mut threads_use = 0;

    // Decision: Sum thread counts across all processes
    for &pid in pids {
        let task_path = format!("{path}/{pid}/task");
        // Decision: Return error if we can't read task directory (critical metric)
        // This is different from fd_use which silently skips processes
        let Ok(files) = fs::read_dir(task_path) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Threads use data not found",
            ));
        };

        // Decision: Count only directories in /proc/<pid>/task/ (each is a thread)
        // flatten() converts Result<DirEntry> to Option<DirEntry>, skipping errors
        // filter_map gets file type, then filter keeps only directories
        threads_use += files
            .flatten()
            .filter_map(|dir_entry| dir_entry.file_type().ok())
            .filter(fs::FileType::is_dir)
            .count();
    }

    // Decision: Convert to f64 to match metric type (all metrics use f64)
    Ok(threads_use as f64)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn path_from_root(file: &str) -> String {
        let mut safe_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        safe_path.push(file);
        safe_path.to_str().unwrap().to_string()
    }

    #[test]
    fn test_get_pid_list() {
        let path = "./tests/proc";
        let mut pids = get_pid_list_from_path(path_from_root(path).as_str());
        pids.sort_unstable();
        assert_eq!(pids.len(), 2);
        assert_eq!(pids[0], 13);
        assert_eq!(pids[1], 142);

        let path = "./tests/incorrect_folder";
        let pids = get_pid_list_from_path(path);
        assert_eq!(pids.len(), 0);
    }

    #[test]
    fn test_get_network_data() {
        let path = "./tests/proc/net/valid_dev";
        let network_data_result = get_network_data_from_path(path_from_root(path).as_str());
        assert!(network_data_result.is_ok());
        let network_data = network_data_result.unwrap();
        assert!((network_data.rx_bytes - 180.0).abs() < f64::EPSILON);
        assert!((network_data.tx_bytes - 254.0).abs() < f64::EPSILON);

        let path = "./tests/proc/net/invalid_dev_malformed";
        let network_data_result = get_network_data_from_path(path_from_root(path).as_str());
        assert!(network_data_result.is_err());

        let path = "./tests/proc/net/invalid_dev_non_numerical_value";
        let network_data_result = get_network_data_from_path(path_from_root(path).as_str());
        assert!(network_data_result.is_err());

        let path = "./tests/proc/net/missing_interface_dev";
        let network_data_result = get_network_data_from_path(path_from_root(path).as_str());
        assert!(network_data_result.is_err());

        let path = "./tests/proc/net/nonexistent_dev";
        let network_data_result = get_network_data_from_path(path_from_root(path).as_str());
        assert!(network_data_result.is_err());
    }

    #[test]
    fn test_get_cpu_data() {
        let path = "./tests/proc/stat/valid_stat";
        let cpu_data_result = get_cpu_data_from_path(path_from_root(path).as_str());
        assert!(cpu_data_result.is_ok());
        let cpu_data = cpu_data_result.unwrap();
        assert!((cpu_data.total_user_time_ms - 23370.0).abs() < f64::EPSILON);
        assert!((cpu_data.total_system_time_ms - 1880.0).abs() < f64::EPSILON);
        assert!((cpu_data.total_idle_time_ms - 178_380.0).abs() < f64::EPSILON);
        assert_eq!(cpu_data.individual_cpu_idle_times.len(), 2);
        assert!(
            (*cpu_data
                .individual_cpu_idle_times
                .get("cpu0")
                .expect("cpu0 not found")
                - 91880.0)
                .abs()
                < f64::EPSILON
        );
        assert!(
            (*cpu_data
                .individual_cpu_idle_times
                .get("cpu1")
                .expect("cpu1 not found")
                - 86490.0)
                .abs()
                < f64::EPSILON
        );

        let path = "./tests/proc/stat/invalid_stat_non_numerical_value_1";
        let cpu_data_result = get_cpu_data_from_path(path_from_root(path).as_str());
        assert!(cpu_data_result.is_err());

        let path = "./tests/proc/stat/invalid_stat_non_numerical_value_2";
        let cpu_data_result = get_cpu_data_from_path(path_from_root(path).as_str());
        assert!(cpu_data_result.is_err());

        let path = "./tests/proc/stat/invalid_stat_malformed_first_line";
        let cpu_data_result = get_cpu_data_from_path(path_from_root(path).as_str());
        assert!(cpu_data_result.is_err());

        let path = "./tests/proc/stat/invalid_stat_malformed_per_cpu_line";
        let cpu_data_result = get_cpu_data_from_path(path_from_root(path).as_str());
        assert!(cpu_data_result.is_err());

        let path = "./tests/proc/stat/invalid_stat_missing_cpun_data";
        let cpu_data_result = get_cpu_data_from_path(path_from_root(path).as_str());
        assert!(cpu_data_result.is_err());

        let path = "./tests/proc/stat/nonexistent_stat";
        let cpu_data_result = get_cpu_data_from_path(path_from_root(path).as_str());
        assert!(cpu_data_result.is_err());
    }

    #[test]
    fn test_get_uptime_data() {
        let path = "./tests/proc/uptime/valid_uptime";
        let uptime_data_result = get_uptime_from_path(path_from_root(path).as_str());
        assert!(uptime_data_result.is_ok());
        let uptime_data = uptime_data_result.unwrap();
        assert!((uptime_data - 3_213_103_123_000.0).abs() < f64::EPSILON);

        let path = "./tests/proc/uptime/invalid_data_uptime";
        let uptime_data_result = get_uptime_from_path(path_from_root(path).as_str());
        assert!(uptime_data_result.is_err());

        let path = "./tests/proc/uptime/malformed_uptime";
        let uptime_data_result = get_uptime_from_path(path_from_root(path).as_str());
        assert!(uptime_data_result.is_err());

        let path = "./tests/proc/uptime/nonexistent_uptime";
        let uptime_data_result = get_uptime_from_path(path_from_root(path).as_str());
        assert!(uptime_data_result.is_err());
    }

    #[test]
    fn test_get_fd_max_data() {
        let path = "./tests/proc/process/valid";
        let pids = get_pid_list_from_path(path_from_root(path).as_str());
        let fd_max = get_fd_max_data_from_path(path_from_root(path).as_str(), &pids);
        assert!((fd_max - 1024.0).abs() < f64::EPSILON);

        let path = "./tests/proc/process/invalid_malformed";
        let fd_max = get_fd_max_data_from_path(path_from_root(path).as_str(), &pids);
        // assert that fd_max is equal to AWS Lambda limit
        assert!((fd_max - constants::LAMBDA_FILE_DESCRIPTORS_DEFAULT_LIMIT).abs() < f64::EPSILON);

        let path = "./tests/proc/process/invalid_missing";
        let fd_max = get_fd_max_data_from_path(path_from_root(path).as_str(), &pids);
        // assert that fd_max is equal to AWS Lambda limit
        assert!((fd_max - constants::LAMBDA_FILE_DESCRIPTORS_DEFAULT_LIMIT).abs() < f64::EPSILON);
    }

    #[test]
    fn test_get_fd_use_data() {
        let path = "./tests/proc/process/valid";
        let pids = get_pid_list_from_path(path_from_root(path).as_str());
        let fd_use = get_fd_use_data_from_path(path_from_root(path).as_str(), &pids);
        assert!((fd_use - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_get_threads_max_data() {
        let path = "./tests/proc/process/valid";
        let pids = get_pid_list_from_path(path_from_root(path).as_str());
        let threads_max = get_threads_max_data_from_path(path_from_root(path).as_str(), &pids);
        assert!((threads_max - 1024.0).abs() < f64::EPSILON);

        let path = "./tests/proc/process/invalid_malformed";
        let threads_max = get_threads_max_data_from_path(path_from_root(path).as_str(), &pids);
        // assert that threads_max is equal to AWS Lambda limit
        assert!(
            (threads_max - constants::LAMBDA_EXECUTION_PROCESSES_DEFAULT_LIMIT).abs()
                < f64::EPSILON
        );

        let path = "./tests/proc/process/invalid_missing";
        let threads_max = get_threads_max_data_from_path(path_from_root(path).as_str(), &pids);
        // assert that threads_max is equal to AWS Lambda limit
        assert!(
            (threads_max - constants::LAMBDA_EXECUTION_PROCESSES_DEFAULT_LIMIT).abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn test_get_threads_use_data() {
        let path = "./tests/proc/process/valid";
        let pids = get_pid_list_from_path(path_from_root(path).as_str());
        let threads_use_result =
            get_threads_use_data_from_path(path_from_root(path).as_str(), &pids);
        assert!(threads_use_result.is_ok());
        let threads_use = threads_use_result.unwrap();
        assert!((threads_use - 5.0).abs() < f64::EPSILON);

        let path = "./tests/proc/process/invalid_missing";
        let threads_use_result =
            get_threads_use_data_from_path(path_from_root(path).as_str(), &pids);
        assert!(threads_use_result.is_err());
    }
}
