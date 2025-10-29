//! Distributed tracing context propagation styles.
//!
//! This module defines how trace context (trace ID, span ID, sampling decision) is propagated
//! between services in distributed systems. Different propagation styles use different HTTP
//! headers and formats.
//!
//! # Propagation Styles
//!
//! - **Datadog**: Datadog's native propagation format (recommended for Datadog-only environments)
//! - **B3 Multi**: Zipkin's multi-header format (compatible with Zipkin and many other tracers)
//! - **B3 Single**: Zipkin's single-header format (more compact than B3 Multi)
//! - **TraceContext**: W3C Trace Context standard (vendor-neutral, emerging standard)
//! - **None**: Disable trace propagation
//!
//! # Configuration
//!
//! Propagation styles can be configured via:
//! - **Environment variable**: `DD_TRACE_PROPAGATION_STYLE=datadog,b3multi`
//! - **YAML config**: `trace_propagation_style: "datadog,tracecontext"`
//!
//! Multiple styles can be specified for injection (outgoing requests) and extraction (incoming requests).
//!
//! # Use Cases
//!
//! - **Datadog-only**: Use `datadog` for best performance and compatibility
//! - **Mixed environment**: Use `datadog,b3multi` to support both Datadog and Zipkin/Jaeger
//! - **Vendor-neutral**: Use `tracecontext` for W3C standard compliance
//! - **Multi-cloud**: Use multiple styles to ensure compatibility across different tracing systems

use std::{fmt::Display, str::FromStr};

use serde::{Deserialize, Deserializer};
use tracing::error;

/// Distributed trace context propagation style.
///
/// Defines the format used to encode trace context (trace ID, span ID, sampling decision)
/// in HTTP headers when making cross-service requests. Different styles use different
/// header names and encoding formats.
///
/// # Propagation Direction
///
/// - **Injection**: Adding trace context to outgoing HTTP requests
/// - **Extraction**: Reading trace context from incoming HTTP requests
///
/// # Header Formats
///
/// | Style | Headers | Example |
/// |-------|---------|---------|
/// | Datadog | `x-datadog-*` | `x-datadog-trace-id: 123` |
/// | B3Multi | `X-B3-TraceId`, `X-B3-SpanId`, `X-B3-Sampled` | Multiple headers |
/// | B3 | `b3` | Single header: `b3: trace-span-sampled` |
/// | TraceContext | `traceparent`, `tracestate` | W3C standard |
/// | None | - | No propagation |
///
/// # Compatibility
///
/// - **Datadog ↔ Datadog**: Full compatibility, best performance
/// - **Datadog ↔ Zipkin/Jaeger**: Use B3Multi or B3
/// - **Datadog ↔ OpenTelemetry**: Use TraceContext
/// - **Cross-vendor**: Use multiple styles or TraceContext
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TracePropagationStyle {
    /// Datadog's native trace propagation format.
    ///
    /// Uses `x-datadog-trace-id`, `x-datadog-parent-id`, and `x-datadog-sampling-priority` headers.
    ///
    /// **Use when**: All services use Datadog APM for best compatibility and performance.
    ///
    /// **Headers**:
    /// - `x-datadog-trace-id`: 64-bit or 128-bit trace ID
    /// - `x-datadog-parent-id`: 64-bit parent span ID
    /// - `x-datadog-sampling-priority`: Sampling decision (0 or 1)
    Datadog,
    /// Zipkin B3 multi-header propagation format.
    ///
    /// Uses separate headers for each trace context component. Compatible with Zipkin,
    /// Jaeger, and many other distributed tracing systems.
    ///
    /// **Use when**: Integrating with Zipkin, Jaeger, or other B3-compatible systems.
    ///
    /// **Headers**:
    /// - `X-B3-TraceId`: 128-bit trace ID (hex-encoded)
    /// - `X-B3-SpanId`: 64-bit span ID (hex-encoded)
    /// - `X-B3-ParentSpanId`: 64-bit parent span ID (optional)
    /// - `X-B3-Sampled`: Sampling decision (`0` or `1`)
    /// - `X-B3-Flags`: Debug flag (optional)
    B3Multi,
    /// Zipkin B3 single-header propagation format.
    ///
    /// Encodes all trace context in a single `b3` header, more compact than B3Multi.
    ///
    /// **Use when**: Header size is a concern or when B3 single-header is preferred.
    ///
    /// **Header**:
    /// - `b3`: Format is `{TraceId}-{SpanId}-{SamplingDecision}-{ParentSpanId}` (hex-encoded)
    ///
    /// **Example**: `b3: 80f198ee56343ba864fe8b2a57d3eff7-e457b5a2e4d86bd1-1-05e3ac9a4f6e3b90`
    B3,
    /// W3C Trace Context standard propagation format.
    ///
    /// Vendor-neutral trace propagation standard defined by the W3C. Designed for
    /// interoperability between different tracing vendors and tools.
    ///
    /// **Use when**: Working in multi-vendor environments or when W3C standard compliance is required.
    ///
    /// **Headers**:
    /// - `traceparent`: Format is `{version}-{trace-id}-{parent-id}-{trace-flags}`
    /// - `tracestate`: Vendor-specific key-value pairs (optional)
    ///
    /// **Example**: `traceparent: 00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01`
    TraceContext,
    /// Disable trace propagation.
    ///
    /// No trace context is injected into outgoing requests or extracted from incoming requests.
    ///
    /// **Use when**: Testing, debugging, or when trace propagation is not desired.
    None,
}

/// Parses trace propagation styles from strings (case-insensitive).
///
/// # Supported Inputs
///
/// - `"datadog"` → `TracePropagationStyle::Datadog`
/// - `"b3multi"` → `TracePropagationStyle::B3Multi`
/// - `"b3"` → `TracePropagationStyle::B3`
/// - `"tracecontext"` → `TracePropagationStyle::TraceContext`
/// - `"none"` → `TracePropagationStyle::None`
///
/// # Error Handling
///
/// Invalid inputs are logged and default to `TracePropagationStyle::None` (no propagation).
/// This ensures the agent can start even with invalid configuration.
impl FromStr for TracePropagationStyle {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Convert to lowercase for case-insensitive matching
        match s.to_lowercase().as_str() {
            "datadog" => Ok(TracePropagationStyle::Datadog),
            "b3multi" => Ok(TracePropagationStyle::B3Multi),
            "b3" => Ok(TracePropagationStyle::B3),
            "tracecontext" => Ok(TracePropagationStyle::TraceContext),
            "none" => Ok(TracePropagationStyle::None),
            // Invalid style - log error and default to None
            _ => {
                error!("Trace propagation style is invalid: {:?}, using None", s);
                Ok(TracePropagationStyle::None)
            }
        }
    }
}

/// Formats trace propagation styles as lowercase strings.
///
/// # Output Format
///
/// - `TracePropagationStyle::Datadog` → `"datadog"`
/// - `TracePropagationStyle::B3Multi` → `"b3multi"`
/// - `TracePropagationStyle::B3` → `"b3"`
/// - `TracePropagationStyle::TraceContext` → `"tracecontext"`
/// - `TracePropagationStyle::None` → `"none"`
impl Display for TracePropagationStyle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let style = match self {
            TracePropagationStyle::Datadog => "datadog",
            TracePropagationStyle::B3Multi => "b3multi",
            TracePropagationStyle::B3 => "b3",
            TracePropagationStyle::TraceContext => "tracecontext",
            TracePropagationStyle::None => "none",
        };
        write!(f, "{style}")
    }
}

/// Deserializes trace propagation styles from comma-separated strings.
///
/// Supports multiple styles separated by commas, allowing the agent to inject and extract
/// trace context in multiple formats for cross-vendor compatibility.
///
/// # Format
///
/// - **Single style**: `"datadog"`
/// - **Multiple styles**: `"datadog,b3multi,tracecontext"`
///
/// # Error Handling
///
/// - Invalid styles are logged and skipped
/// - Whitespace around commas is automatically trimmed
/// - Returns empty vector if all styles are invalid
///
/// # Examples
///
/// ```ignore
/// use datadog_agent_native::config::trace_propagation_style::TracePropagationStyle;
/// use serde_json::json;
///
/// // Single style
/// let styles: Vec<TracePropagationStyle> = serde_json::from_value(json!("datadog")).unwrap();
/// // Result: [Datadog]
///
/// // Multiple styles (with whitespace trimming)
/// let styles: Vec<TracePropagationStyle> = serde_json::from_value(
///     json!("datadog,  b3multi,  tracecontext")
/// ).unwrap();
/// // Result: [Datadog, B3Multi, TraceContext]
///
/// // Invalid styles are skipped
/// let styles: Vec<TracePropagationStyle> = serde_json::from_value(
///     json!("datadog,invalid,b3")
/// ).unwrap();
/// // Result: [Datadog, B3] (invalid style logged and skipped)
/// ```
#[allow(clippy::module_name_repetitions)]
pub fn deserialize_trace_propagation_style<'de, D>(
    deserializer: D,
) -> Result<Vec<TracePropagationStyle>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = String::deserialize(deserializer)?;

    // Parse comma-separated list of propagation styles
    // Example: "datadog,b3multi,tracecontext" → [Datadog, B3Multi, TraceContext]
    Ok(s.split(',')
        .filter_map(|style| {
            // Trim whitespace and parse each style
            match TracePropagationStyle::from_str(style.trim()) {
                Ok(parsed_style) => Some(parsed_style),
                Err(e) => {
                    // Invalid style - log error and skip
                    tracing::error!("Failed to parse trace propagation style: {}, ignoring", e);
                    None
                }
            }
        })
        .collect())
}
