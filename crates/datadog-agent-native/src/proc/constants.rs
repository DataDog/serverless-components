//! Constants for /proc filesystem paths and Lambda resource limits.
//!
//! This module defines paths to various /proc files used for system metrics
//! collection, as well as AWS Lambda-specific constants for resource limits
//! and network interfaces.
//!
//! # /proc Filesystem
//!
//! The /proc filesystem is a virtual filesystem in Linux that provides
//! information about running processes and system resources:
//!
//! - `/proc/net/dev`: Network interface statistics (RX/TX bytes)
//! - `/proc/stat`: CPU time statistics (user, system, idle)
//! - `/proc/uptime`: System uptime in seconds
//! - `/proc/<pid>/limits`: Process resource limits (file descriptors, threads)
//! - `/proc/<pid>/fd/`: Open file descriptors for a process
//! - `/proc/<pid>/task/`: Threads for a process
//!
//! # Lambda Environment
//!
//! AWS Lambda provides specific default resource limits and uses custom
//! network interfaces for communication between the runtime and extension.

/// Path to `/proc/net/dev` file containing network interface statistics.
///
/// This file shows received and transmitted bytes for each network interface.
/// Format: Interface name followed by 16 columns of statistics.
pub const PROC_NET_DEV_PATH: &str = "/proc/net/dev";

/// Path to `/proc/stat` file containing CPU time statistics.
///
/// This file shows CPU time spent in various modes (user, system, idle, etc.)
/// for the system and each individual CPU core.
pub const PROC_STAT_PATH: &str = "/proc/stat";

/// Path to `/proc/uptime` file containing system uptime.
///
/// This file contains two values: system uptime and idle time (in seconds).
pub const PROC_UPTIME_PATH: &str = "/proc/uptime";

/// Path to `/proc` directory (root of proc filesystem).
///
/// Used as the base path for accessing per-process information like
/// `/proc/<pid>/limits`, `/proc/<pid>/fd/`, etc.
pub const PROC_PATH: &str = "/proc";

/// Path to `/etc/os-release` file containing OS distribution information.
///
/// This file provides information about the Linux distribution name,
/// version, and other identifying details.
pub const ETC_PATH: &str = "/etc/os-release";

/// Path to `/var/lang/bin` directory in Lambda environments.
///
/// This directory contains the runtime binaries provided by AWS Lambda
/// (e.g., Python, Node.js, Java interpreters).
pub const VAR_LANG_BIN_PATH: &str = "/var/lang/bin";

/// Name of the primary Lambda network interface.
///
/// `vinternal_1` is the network interface used for communication between
/// the Lambda function and the Lambda service (for invocation requests/responses).
pub const LAMBDA_NETWORK_INTERFACE: &str = "vinternal_1";

/// Name of the Lambda runtime API network interface.
///
/// `vint_runtime` is the network interface used for communication between
/// the Lambda runtime and extensions via the Runtime API and Extensions API.
pub const LAMBDA_RUNTIME_NETWORK_INTERFACE: &str = "vint_runtime";

/// Default soft limit for file descriptors in Lambda (ulimit -n).
///
/// Lambda sets a default soft limit of 1024 open file descriptors per function.
/// This is used as a fallback when we can't read the actual limit from
/// `/proc/<pid>/limits`.
pub(crate) const LAMBDA_FILE_DESCRIPTORS_DEFAULT_LIMIT: f64 = 1024.0;

/// Default soft limit for processes/threads in Lambda (ulimit -u).
///
/// Lambda sets a default soft limit of 1024 processes/threads per function.
/// This is used as a fallback when we can't read the actual limit from
/// `/proc/<pid>/limits`.
pub(crate) const LAMBDA_EXECUTION_PROCESSES_DEFAULT_LIMIT: f64 = 1024.0;
