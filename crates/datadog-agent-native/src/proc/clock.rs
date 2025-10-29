//! System clock tick rate utilities.
//!
//! This module provides functions to query the system clock tick rate (CLK_TCK),
//! which is used to convert CPU time values from `/proc/stat` (measured in clock
//! ticks) to real time units (seconds/milliseconds).
//!
//! # Clock Ticks (USER_HZ)
//!
//! On Linux, CPU time in `/proc/stat` is measured in "clock ticks" or "USER_HZ".
//! The clock tick rate is typically 100 Hz (100 ticks per second), meaning each
//! tick represents 10ms. However, this can vary by system configuration.
//!
//! To convert CPU ticks to milliseconds:
//! ```text
//! time_ms = (ticks / CLK_TCK) × 1000
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! let clk_tck = get_clk_tck()?;  // e.g., 100
//! let cpu_ticks = 2337.0;         // from /proc/stat
//! let cpu_time_ms = (cpu_ticks / clk_tck as f64) * 1000.0;  // 23370ms
//! ```

use nix::unistd::{sysconf, SysconfVar};
use std::io;

/// Queries the system clock tick rate (CLK_TCK) on Unix systems.
///
/// This function returns the number of clock ticks per second (USER_HZ),
/// which is needed to convert CPU time values from `/proc/stat` to real time.
///
/// # Returns
///
/// * `Ok(ticks_per_second)` - Clock tick rate (typically 100 Hz)
/// * `Err` - If `sysconf(CLK_TCK)` fails or returns invalid value
///
/// # Platform
///
/// Unix/Linux only. Windows uses the stub implementation below.
///
/// # Example
///
/// ```rust,ignore
/// let clk_tck = get_clk_tck()?;
/// println!("System clock ticks per second: {}", clk_tck);
/// // Typical output: "System clock ticks per second: 100"
/// ```
#[allow(clippy::cast_sign_loss)]
#[cfg(not(target_os = "windows"))]
pub fn get_clk_tck() -> Result<u64, io::Error> {
    // Query system configuration for clock ticks per second
    match sysconf(SysconfVar::CLK_TCK) {
        // Decision: Only accept positive values (zero or negative would be invalid)
        Ok(Some(clk_tck)) if clk_tck > 0 => Ok(clk_tck as u64),
        // Decision: Return error rather than panic for graceful degradation
        _ => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Could not find system clock ticks per second",
        )),
    }
}

/// Windows stub for `get_clk_tck` (CLK_TCK is not applicable).
///
/// Windows doesn't use the USER_HZ/CLK_TCK concept for CPU time measurement.
/// This stub returns 1 to allow division operations to work (though the
/// CPU time parsing logic itself is Unix-specific).
///
/// # Platform
///
/// Windows only. Unix platforms use the real implementation above.
///
/// # Returns
///
/// Always returns `Ok(1)` on Windows.
#[cfg(target_os = "windows")]
pub fn get_clk_tck() -> Result<u64, io::Error> {
    // Decision: Return 1 rather than error to allow compilation on Windows
    // (even though /proc parsing won't work)
    Ok(1)
}
