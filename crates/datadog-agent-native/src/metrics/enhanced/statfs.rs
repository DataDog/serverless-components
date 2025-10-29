//! Filesystem statistics for monitoring `/tmp` directory usage.
//!
//! This module provides Unix-specific functions to query filesystem statistics
//! using the `statfs` system call. It's primarily used to monitor `/tmp` directory
//! usage in Lambda environments where disk space is limited.
//!
//! # Platform Support
//!
//! - **Unix/Linux/macOS**: Full support via `nix::sys::statfs`
//! - **Windows**: Not supported (returns error)
//!
//! # Usage Pattern
//!
//! ```rust,ignore
//! use datadog_agent_native::metrics::enhanced::statfs::{get_tmp_max, get_tmp_used};
//!
//! // Query /tmp capacity
//! let tmp_max = get_tmp_max()?;
//! println!("Total /tmp capacity: {} bytes", tmp_max);
//!
//! // Query /tmp usage
//! let tmp_used = get_tmp_used()?;
//! println!("Used /tmp space: {} bytes", tmp_used);
//! ```
//!
//! # Lambda /tmp Directory
//!
//! Lambda provides 512MB-10GB of `/tmp` storage (configurable). This space is:
//! - **Ephemeral**: Cleared between cold starts
//! - **Shared**: Available across invocations in the same sandbox
//! - **Limited**: Filling it causes I/O errors
//!
//! Monitoring `/tmp` usage helps detect storage leaks and prevent failures.

#![allow(clippy::module_name_repetitions)]

use crate::metrics::enhanced::constants;
use nix::sys::statfs::statfs;
use std::io;
use std::path::Path;

/// Queries filesystem statistics for a directory path (Unix only).
///
/// Returns the block size, total blocks, and available blocks for the specified
/// filesystem. These values can be used to calculate total capacity and usage.
///
/// # Arguments
///
/// * `path` - Directory path to query (e.g., "/tmp")
///
/// # Returns
///
/// * `Ok((block_size, total_blocks, available_blocks))` - Filesystem statistics
/// * `Err(io::Error)` - If `statfs` system call fails
///
/// # Platform
///
/// This function is only available on Unix-like systems. Windows builds will
/// use the stub implementation that returns an error.
///
/// # Formula
///
/// - **Total capacity**: `block_size × total_blocks`
/// - **Used space**: `block_size × (total_blocks - available_blocks)`
/// - **Free space**: `block_size × available_blocks`
///
/// # Example
///
/// ```rust,ignore
/// let (bsize, blocks, bavail) = statfs_info("/tmp")?;
/// let total_bytes = bsize * blocks;
/// let used_bytes = bsize * (blocks - bavail);
/// let free_bytes = bsize * bavail;
/// ```
#[cfg(not(target_os = "windows"))]
#[allow(clippy::cast_lossless)]
pub fn statfs_info(path: &str) -> Result<(f64, f64, f64), io::Error> {
    // Query filesystem statistics via statfs(2) system call
    // Decision: Convert nix::Error to io::Error for consistent error handling
    let stat = statfs(Path::new(path)).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    // Extract block size and block counts as f64 for large filesystem support
    // Decision: Use f64 to handle filesystems >2^53 bytes (8 petabytes)
    Ok((
        stat.block_size() as f64,
        stat.blocks() as f64,
        stat.blocks_available() as f64,
    ))
}

/// Windows stub for `statfs_info` (not supported).
///
/// This function always returns an error on Windows platforms since
/// Windows doesn't support the Unix `statfs` system call.
///
/// # Platform
///
/// Windows only. Unix platforms use the real implementation above.
///
/// # Returns
///
/// Always returns `Err` with message "Cannot get tmp data on Windows".
#[cfg(target_os = "windows")]
fn statfs_info(path: &str) -> Result<(f64, f64, f64), io::Error> {
    // Decision: Return error rather than panic for graceful degradation
    Err(io::Error::new(
        io::ErrorKind::Other,
        "Cannot get tmp data on Windows",
    ))
}

/// Returns the total capacity of the `/tmp` directory (in bytes).
///
/// This is calculated as `block_size × total_blocks` for the `/tmp` filesystem.
/// In Lambda, this is the configured ephemeral storage size (512MB-10GB).
///
/// # Returns
///
/// * `Ok(bytes)` - Total `/tmp` capacity in bytes
/// * `Err(io::Error)` - If `statfs` system call fails
///
/// # Example
///
/// ```rust,ignore
/// let tmp_max = get_tmp_max()?;
/// println!("Total /tmp capacity: {} MB", tmp_max / 1024.0 / 1024.0);
/// ```
pub fn get_tmp_max() -> Result<f64, io::Error> {
    // Query filesystem statistics for /tmp
    let (bsize, blocks, _) = statfs_info(constants::TMP_PATH)?;

    // Calculate total capacity: block_size × total_blocks
    // Decision: Ignore available blocks since we only need total capacity
    let tmp_max = bsize * blocks;
    Ok(tmp_max)
}

/// Returns the currently used space in the `/tmp` directory (in bytes).
///
/// This is calculated as `block_size × (total_blocks - available_blocks)`.
/// This represents all files and directories currently in `/tmp`.
///
/// # Returns
///
/// * `Ok(bytes)` - Used `/tmp` space in bytes
/// * `Err(io::Error)` - If `statfs` system call fails
///
/// # Example
///
/// ```rust,ignore
/// let tmp_used = get_tmp_used()?;
/// println!("Used /tmp space: {} MB", tmp_used / 1024.0 / 1024.0);
/// ```
///
/// # Lambda Usage
///
/// In Lambda, monitor this metric to:
/// - Detect storage leaks across invocations
/// - Prevent I/O errors from full `/tmp`
/// - Trigger cleanup when usage exceeds threshold
pub fn get_tmp_used() -> Result<f64, io::Error> {
    // Query filesystem statistics for /tmp
    let (bsize, blocks, bavail) = statfs_info(constants::TMP_PATH)?;

    // Calculate used space: block_size × (total_blocks - available_blocks)
    // Decision: Use available_blocks (not free_blocks) for accurate usage
    let tmp_used = bsize * (blocks - bavail);
    Ok(tmp_used)
}
