//! # Datadog Agent Native
//!
//! This crate provides a native Rust implementation of the Datadog Agent, designed to run
//! embedded within applications for serverless and containerized environments.
//!
//! ## Overview
//!
//! The agent supports multiple data pipelines:
//! - **Traces**: APM trace collection, aggregation, and forwarding
//! - **Logs**: Log ingestion and forwarding
//! - **Metrics**: DogStatsD metrics collection (via UDP or Unix Domain Sockets)
//! - **Application Security (AppSec)**: WAF and security monitoring
//! - **Remote Configuration**: Dynamic configuration updates from Datadog
//!
//! ## Architecture
//!
//! The library is organized into several key modules:
//! - [`agent`]: Agent lifecycle management and coordination
//! - [`traces`]: Trace collection, aggregation, and statistics
//! - [`logs`]: Log ingestion and batch processing
//! - [`metrics`]: Enhanced metrics and usage tracking
//! - [`appsec`]: Application security processing (WAF)
//! - [`remote_config`]: Remote configuration client
//! - [`ffi`]: Foreign Function Interface for C/C#/Python bindings
//!
//! ## Features
//!
//! - Multiple operational modes: HTTP, UDS (Unix Domain Sockets), FFI-only
//! - Automatic port assignment to avoid conflicts
//! - Built-in retry logic with exponential backoff
//! - Remote configuration support for dynamic updates
//! - FIPS 140-2 compliant cryptography (when enabled)

#![deny(clippy::all)]
#![deny(clippy::pedantic)]
#![deny(clippy::unwrap_used)]
#![deny(unused_extern_crates)]
#![deny(unused_allocation)]
#![deny(unused_assignments)]
#![deny(unused_comparisons)]
#![deny(unreachable_pub)]
#![deny(missing_copy_implementations)]
// #![deny(missing_debug_implementations)]

// TODO: Remove these lints over time as documentation is completed
#![allow(missing_docs)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::needless_pass_by_value)]
// Allow use of the `coverage_nightly` attribute for code coverage
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

/// Agent lifecycle management and service coordination
pub mod agent;

/// Application Security (AppSec) - Web Application Firewall (WAF) processing
#[cfg(feature = "appsec")]
pub mod appsec;

/// Configuration management - YAML files, environment variables, and defaults
pub mod config;

/// DogStatsD adapter for metrics collection
pub mod dogstatsd_adapter;

/// Event bus for inter-component communication
pub mod event_bus;

/// Foreign Function Interface (FFI) for C, C#, Python, and other language bindings
pub mod ffi;

/// FIPS 140-2 compliant cryptography support
pub mod fips;

/// HTTP utilities for request/response handling
pub mod http;

/// Logging infrastructure and tracing setup
pub mod logger;

/// Log ingestion, batching, and forwarding
pub mod logs;

/// Enhanced metrics, usage tracking, and system monitoring
pub mod metrics;

/// Process utilities - hostname, clock, and system information
pub mod proc;

/// Remote configuration client for dynamic updates from Datadog
pub mod remote_config;

/// Tag providers for unified service tagging
pub mod tags;

/// Trace collection, aggregation, statistics, and forwarding
pub mod traces;

/// Maximum number of retries when flushing data to backend fails.
///
/// When data submission fails (network error, 5xx response, etc.), the agent will
/// retry up to this many times with exponential backoff before dropping the data.
///
/// # Future Work
/// TODO: Make this configurable via environment variable or configuration file
pub(crate) const FLUSH_RETRY_COUNT: usize = 3;

/// Agent version reported to Datadog backend for remote configuration.
///
/// This version string is sent to the Datadog backend during remote configuration
/// handshakes. It helps Datadog identify the agent version for compatibility and
/// feature support purposes.
///
/// The version follows the Datadog Agent versioning scheme and should be kept in sync
/// with the official agent releases when possible.
pub const AGENT_VERSION: &str = "7.70.0";

/// Build timestamp (set at compile time).
///
/// This timestamp is injected at build time via the `BUILD_TIMESTAMP` environment
/// variable set in `build.rs`. It helps track when a particular binary was compiled,
/// which is useful for debugging and version tracking in production environments.
///
/// # Format
/// The timestamp is in ISO 8601 format: `YYYY-MM-DDTHH:MM:SSZ`
pub const BUILD_TIMESTAMP: &str = env!("BUILD_TIMESTAMP");

/// Logs build information (version and timestamp) at INFO level.
///
/// This function should be called during agent initialization to record the exact
/// version and build time in the logs. This information is invaluable for:
/// - Debugging issues in production
/// - Confirming which version is running
/// - Tracking deployments
///
/// # Example
/// ```no_run
/// use datadog_agent_native::log_build_info;
///
/// fn main() {
///     // Initialize tracing/logging first
///     log_build_info();  // Logs: "datadog-agent-native version: 7.70.0, built: 2024-01-15T10:30:00Z"
/// }
/// ```
pub fn log_build_info() {
    tracing::info!(
        "datadog-agent-native version: {}, built: {}",
        AGENT_VERSION,
        BUILD_TIMESTAMP
    );
}
