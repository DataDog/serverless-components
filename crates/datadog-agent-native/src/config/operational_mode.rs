//! Operational mode configuration for the Datadog agent.
//!
//! This module defines how the agent accepts telemetry data from instrumented applications.
//! The agent can run in different modes depending on the deployment environment.
//!
//! # Operational Modes
//!
//! 1. **HTTP with Fixed Port** - Traditional production mode with well-known ports
//! 2. **HTTP with Ephemeral Port** - Dynamic port allocation for testing/containers
//! 3. **HTTP with Unix Domain Socket** - File-based sockets for local communication
//!
//! See [`OperationalMode`] enum for detailed documentation on each mode.

use serde::{Deserialize, Serialize};

/// Defines how the Datadog agent operates and accepts telemetry data.
///
/// The agent can run in three different modes:
/// 1. **HTTP with Fixed Port** - Traditional mode with HTTP servers on fixed TCP ports
/// 2. **HTTP with Ephemeral Port** - HTTP servers on auto-assigned TCP ports (useful for testing/containers)
/// 3. **HTTP with Unix Domain Socket** - HTTP servers using Unix sockets instead of TCP
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationalMode {
    /// HTTP servers run on fixed, well-known TCP ports.
    ///
    /// - Trace Agent: Port specified in config (default 8126)
    /// - Logs Agent: Port specified in config (if implemented)
    /// - Metrics: Port specified in config (default 8125, if implemented)
    ///
    /// This is the traditional mode for production deployments.
    HttpFixedPort,

    /// HTTP servers run on ephemeral (auto-assigned) TCP ports.
    ///
    /// - Ports are assigned by the OS (port 0 binding)
    /// - Actual bound ports can be queried after startup
    /// - Useful for testing, CI/CD, and containerized environments
    ///
    /// Use `get_bound_ports()` on the coordinator to retrieve actual ports.
    HttpEphemeralPort,

    /// HTTP servers run on Unix Domain Sockets (or Named Pipes on Windows).
    ///
    /// - Uses filesystem-based sockets instead of TCP/IP
    /// - Socket path auto-generated using process PID (e.g., `/tmp/dd-trace-12345.sock`)
    /// - Better security (filesystem permissions) and performance (no TCP overhead)
    /// - Prevents port conflicts in multi-process environments
    ///
    /// Socket path is returned in `AgentStartInfo` after startup.
    /// Supported on Unix-like systems (Linux, macOS) and Windows 10 1803+.
    HttpUds,
}

impl OperationalMode {
    /// Returns true if HTTP servers should be started in this mode.
    pub const fn requires_http_server(self) -> bool {
        matches!(
            self,
            Self::HttpFixedPort | Self::HttpEphemeralPort | Self::HttpUds
        )
    }

    /// Returns true if ephemeral TCP ports should be used.
    pub const fn uses_ephemeral_ports(self) -> bool {
        matches!(self, Self::HttpEphemeralPort)
    }

    /// Returns true if Unix Domain Sockets should be used.
    pub const fn uses_uds(self) -> bool {
        matches!(self, Self::HttpUds)
    }

    /// Returns true if TCP (either fixed or ephemeral port) should be used.
    pub const fn uses_tcp(self) -> bool {
        matches!(self, Self::HttpFixedPort | Self::HttpEphemeralPort)
    }

    /// Parse from environment variable string.
    ///
    /// Accepts: "http_fixed_port", "http_ephemeral_port", "http_uds"
    /// Aliases: "fixed", "http", "ephemeral", "dynamic", "uds", "socket"
    pub fn from_env_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "http_fixed_port" | "fixed" | "http" => Some(Self::HttpFixedPort),
            "http_ephemeral_port" | "ephemeral" | "dynamic" => Some(Self::HttpEphemeralPort),
            "http_uds" | "uds" | "socket" => Some(Self::HttpUds),
            _ => None,
        }
    }
}

impl Default for OperationalMode {
    fn default() -> Self {
        // Default to traditional HTTP mode with fixed ports
        Self::HttpFixedPort
    }
}

impl std::fmt::Display for OperationalMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HttpFixedPort => write!(f, "HTTP (Fixed Port)"),
            Self::HttpEphemeralPort => write!(f, "HTTP (Ephemeral Port)"),
            Self::HttpUds => write!(f, "HTTP (Unix Domain Socket)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_requires_http_server() {
        assert!(OperationalMode::HttpFixedPort.requires_http_server());
        assert!(OperationalMode::HttpEphemeralPort.requires_http_server());
        assert!(OperationalMode::HttpUds.requires_http_server());
    }

    #[test]
    fn test_uses_ephemeral_ports() {
        assert!(!OperationalMode::HttpFixedPort.uses_ephemeral_ports());
        assert!(OperationalMode::HttpEphemeralPort.uses_ephemeral_ports());
        assert!(!OperationalMode::HttpUds.uses_ephemeral_ports());
    }

    #[test]
    fn test_uses_uds() {
        assert!(!OperationalMode::HttpFixedPort.uses_uds());
        assert!(!OperationalMode::HttpEphemeralPort.uses_uds());
        assert!(OperationalMode::HttpUds.uses_uds());
    }

    #[test]
    fn test_uses_tcp() {
        assert!(OperationalMode::HttpFixedPort.uses_tcp());
        assert!(OperationalMode::HttpEphemeralPort.uses_tcp());
        assert!(!OperationalMode::HttpUds.uses_tcp());
    }

    #[test]
    fn test_from_env_str() {
        assert_eq!(
            OperationalMode::from_env_str("http_fixed_port"),
            Some(OperationalMode::HttpFixedPort)
        );
        assert_eq!(
            OperationalMode::from_env_str("fixed"),
            Some(OperationalMode::HttpFixedPort)
        );
        assert_eq!(
            OperationalMode::from_env_str("http_ephemeral_port"),
            Some(OperationalMode::HttpEphemeralPort)
        );
        assert_eq!(
            OperationalMode::from_env_str("ephemeral"),
            Some(OperationalMode::HttpEphemeralPort)
        );
        assert_eq!(
            OperationalMode::from_env_str("http_uds"),
            Some(OperationalMode::HttpUds)
        );
        assert_eq!(
            OperationalMode::from_env_str("uds"),
            Some(OperationalMode::HttpUds)
        );
        assert_eq!(
            OperationalMode::from_env_str("socket"),
            Some(OperationalMode::HttpUds)
        );
        assert_eq!(OperationalMode::from_env_str("invalid"), None);
    }

    #[test]
    fn test_default() {
        assert_eq!(OperationalMode::default(), OperationalMode::HttpFixedPort);
    }

    #[test]
    fn test_display() {
        assert_eq!(
            format!("{}", OperationalMode::HttpFixedPort),
            "HTTP (Fixed Port)"
        );
        assert_eq!(
            format!("{}", OperationalMode::HttpEphemeralPort),
            "HTTP (Ephemeral Port)"
        );
        assert_eq!(
            format!("{}", OperationalMode::HttpUds),
            "HTTP (Unix Domain Socket)"
        );
    }

    #[test]
    fn test_serialize_http_fixed_port() {
        let mode = OperationalMode::HttpFixedPort;
        let json = serde_json::to_string(&mode).expect("Failed to serialize");
        assert_eq!(json, "\"http_fixed_port\"");
    }

    #[test]
    fn test_serialize_http_ephemeral_port() {
        let mode = OperationalMode::HttpEphemeralPort;
        let json = serde_json::to_string(&mode).expect("Failed to serialize");
        assert_eq!(json, "\"http_ephemeral_port\"");
    }

    #[test]
    fn test_serialize_http_uds() {
        let mode = OperationalMode::HttpUds;
        let json = serde_json::to_string(&mode).expect("Failed to serialize");
        assert_eq!(json, "\"http_uds\"");
    }

    #[test]
    fn test_deserialize_http_fixed_port() {
        let json = "\"http_fixed_port\"";
        let mode: OperationalMode = serde_json::from_str(json).expect("Failed to deserialize");
        assert_eq!(mode, OperationalMode::HttpFixedPort);
    }

    #[test]
    fn test_deserialize_http_ephemeral_port() {
        let json = "\"http_ephemeral_port\"";
        let mode: OperationalMode = serde_json::from_str(json).expect("Failed to deserialize");
        assert_eq!(mode, OperationalMode::HttpEphemeralPort);
    }

    #[test]
    fn test_deserialize_http_uds() {
        let json = "\"http_uds\"";
        let mode: OperationalMode = serde_json::from_str(json).expect("Failed to deserialize");
        assert_eq!(mode, OperationalMode::HttpUds);
    }

    #[test]
    fn test_round_trip_all_modes() {
        let modes = [
            OperationalMode::HttpFixedPort,
            OperationalMode::HttpEphemeralPort,
            OperationalMode::HttpUds,
        ];

        for mode in modes {
            // Serialize to JSON
            let json = serde_json::to_string(&mode).expect("Failed to serialize");

            // Deserialize back
            let deserialized: OperationalMode =
                serde_json::from_str(&json).expect("Failed to deserialize");

            // Should match original
            assert_eq!(mode, deserialized);
        }
    }

    #[test]
    fn test_deserialize_invalid_value() {
        let json = "\"invalid_mode\"";
        let result: Result<OperationalMode, _> = serde_json::from_str(json);
        assert!(result.is_err(), "Should fail to deserialize invalid value");
    }
}
