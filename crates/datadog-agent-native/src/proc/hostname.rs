// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Hostname detection utilities

use std::env;
use tracing::warn;

/// Get the system hostname
///
/// This function tries multiple methods to determine the hostname:
/// 1. DD_HOSTNAME environment variable (if set)
/// 2. HOSTNAME environment variable
/// 3. System hostname via nix::unistd::gethostname()
/// 4. Fallback to "unknown" if all methods fail
#[must_use]
pub fn get_hostname() -> String {
    // 1. Check DD_HOSTNAME (Datadog-specific override)
    // Decision: Prioritize DD_HOSTNAME for explicit user configuration
    // This allows users to override hostname detection in containerized environments
    if let Ok(hostname) = env::var("DD_HOSTNAME") {
        // Decision: Require non-empty value (empty string means not set)
        if !hostname.is_empty() {
            return hostname;
        }
    }

    // 2. Check HOSTNAME environment variable
    // Decision: Use standard HOSTNAME env var as second choice
    // This is commonly set in containers and Lambda environments
    if let Ok(hostname) = env::var("HOSTNAME") {
        // Decision: Require non-empty value
        if !hostname.is_empty() {
            return hostname;
        }
    }

    // 3. Try to get system hostname
    // Decision: Fall back to gethostname() syscall as last resort
    match nix::unistd::gethostname() {
        Ok(hostname_osstr) => {
            // Decision: Convert OsString to String, ignoring non-UTF8 hostnames
            if let Some(hostname_str) = hostname_osstr.to_str() {
                // Decision: Require non-empty hostname
                if !hostname_str.is_empty() {
                    return hostname_str.to_string();
                }
            }
        }
        Err(e) => {
            // Decision: Log warning but don't fail (continue to fallback)
            warn!("Failed to get system hostname: {}", e);
        }
    }

    // 4. Fallback
    // Decision: Return "unknown" rather than panic when all methods fail
    // This ensures the agent can continue operating even without a valid hostname
    warn!("Could not determine hostname, using 'unknown'");
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_hostname_not_empty() {
        let hostname = get_hostname();
        assert!(!hostname.is_empty());
        println!("Detected hostname: {}", hostname);
    }

    #[test]
    fn test_dd_hostname_override() {
        env::set_var("DD_HOSTNAME", "test-hostname-override");
        let hostname = get_hostname();
        assert_eq!(hostname, "test-hostname-override");
        env::remove_var("DD_HOSTNAME");
    }
}
