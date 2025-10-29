//! Application Security (AppSec) and API Protection module.
//!
//! This module provides runtime application self-protection (RASP) capabilities
//! and API security monitoring through integration with Datadog's AppSec backend.
//!
//! # Features
//!
//! - **Application Security Monitoring**: Detect security threats in real-time
//! - **API Security**: Monitor API endpoints for attacks and anomalies
//! - **Rule-based Detection**: WAF-style rules powered by threat intelligence
//! - **Attack Detection**: SQL injection, XSS, command injection, path traversal
//! - **Standalone Mode**: Can operate without tracing enabled
//!
//! # Architecture
//!
//! ```text
//!    HTTP Request
//!         │
//!         v
//!   ┌─────────────┐
//!   │  Processor  │ (Apply security rules)
//!   └──────┬──────┘
//!         │
//!         v
//!   ┌─────────────┐
//!   │  Analysis   │ (Detect attacks)
//!   └──────┬──────┘
//!         │
//!         v
//!   ┌─────────────┐
//!   │  Response   │ (Block or allow)
//!   └──────┬──────┘
//!         │
//!         v
//!    HTTP Response + Security Events
//! ```
//!
//! # Modes of Operation
//!
//! 1. **Integrated Mode**: AppSec + APM tracing together
//! 2. **Standalone Mode**: AppSec without tracing (via `DD_APM_TRACING_ENABLED=false`)
//!
//! # Configuration
//!
//! AppSec is enabled via `Config::appsec_enabled`. Rules are loaded from
//! the backend or local configuration.

use std::env;

use crate::config::Config;

pub mod processor;

// Re-export key types for easier access
pub use processor::http::{HttpRequest, HttpResponse, HttpTransaction};
pub use processor::{Error as ProcessorError, InvocationPayload, Processor};

/// Determines whether App & API Protection features are enabled.
///
/// This checks the configuration to see if AppSec has been enabled by the user.
/// When disabled, no security rules are loaded or evaluated, and requests are
/// passed through without security analysis.
///
/// # Arguments
///
/// * `cfg` - The agent configuration containing AppSec settings
///
/// # Returns
///
/// * `true` - AppSec features are enabled, security rules will be evaluated
/// * `false` - AppSec features are disabled, requests pass through without analysis
///
/// # Example
///
/// ```rust,ignore
/// use datadog_agent_native::appsec;
/// use datadog_agent_native::config::Config;
///
/// let config = Config::default();
/// if appsec::is_enabled(&config) {
///     println!("AppSec is enabled");
/// }
/// ```
#[must_use]
pub const fn is_enabled(cfg: &Config) -> bool {
    cfg.appsec_enabled
}

/// Determines whether APM is only used as a transport for App & API Protection,
/// instead of being used for tracing as well.
///
/// In standalone mode, the APM tracer is used solely to transport AppSec events
/// to the Datadog backend, without collecting or sending trace data. This is useful
/// when you want security monitoring without the overhead of distributed tracing.
///
/// # Environment Variable
///
/// Set `DD_APM_TRACING_ENABLED=true` to enable standalone mode.
///
/// # Returns
///
/// * `true` - Standalone mode: AppSec without tracing
/// * `false` - Integrated mode: AppSec with tracing enabled
///
/// # Example
///
/// ```rust,ignore
/// use datadog_agent_native::appsec;
///
/// if appsec::is_standalone() {
///     println!("Running in standalone mode (no tracing)");
/// } else {
///     println!("Running in integrated mode (with tracing)");
/// }
/// ```
#[must_use]
pub fn is_standalone() -> bool {
    // Check if DD_APM_TRACING_ENABLED is set to "true"
    // Decision: Check case-insensitively for user convenience
    env::var("DD_APM_TRACING_ENABLED").is_ok_and(|s| s.to_lowercase() == "true")
}
