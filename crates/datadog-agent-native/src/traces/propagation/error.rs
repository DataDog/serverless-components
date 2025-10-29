//! Error types for trace context propagation operations.
//!
//! This module defines errors that can occur during distributed tracing context
//! extraction and injection operations. Errors are non-fatal and indicate issues
//! with parsing or encoding trace headers.
//!
//! # Error Scenarios
//!
//! Propagation errors typically occur when:
//! - **Malformed headers**: Invalid trace ID, span ID, or sampling priority formats
//! - **Missing headers**: Required trace context headers are absent
//! - **Encoding issues**: Unable to encode trace context into carrier headers
//! - **Version mismatches**: Unsupported propagation format versions
//!
//! # Handling
//!
//! Propagation errors are typically logged and result in:
//! - **Extract failures**: Starting a new trace (no parent context)
//! - **Inject failures**: Continuing without propagation (trace is not distributed)

use thiserror::Error;

/// Error during trace context extraction or injection.
///
/// Contains information about what operation failed, which propagator encountered
/// the error, and a descriptive error message.
///
/// # Display Format
///
/// Errors are formatted as: `"Cannot {operation} from {message}, {propagator_name}"`
///
/// Example: `"Cannot extract from invalid trace ID format, DatadogHeaderPropagator"`
#[derive(Error, Debug, Copy, Clone)]
#[error("Cannot {} from {}, {}", operation, message, propagator_name)]
pub struct Error {
    /// Description of what went wrong.
    ///
    /// Examples:
    /// - `"invalid trace ID format"`
    /// - `"missing required header"`
    /// - `"unsupported version"`
    message: &'static str,
    /// Name of the propagator that encountered the error.
    ///
    /// Examples:
    /// - `"DatadogHeaderPropagator"`
    /// - `"TraceContextPropagator"`
    /// - `"B3Propagator"`
    propagator_name: &'static str,
    /// Operation that failed (`"extract"` or `"inject"`).
    operation: &'static str,
}

impl Error {
    /// Creates an extraction error.
    ///
    /// Used when extracting trace context from incoming request headers fails.
    ///
    /// # Arguments
    ///
    /// * `message` - Description of the extraction failure
    /// * `propagator_name` - Name of the propagator that failed
    ///
    /// # Example
    ///
    /// ```
    /// use datadog_agent_native::traces::propagation::error::Error;
    ///
    /// let err = Error::extract("invalid trace ID", "DatadogHeaderPropagator");
    /// ```
    #[must_use]
    pub fn extract(message: &'static str, propagator_name: &'static str) -> Self {
        Self {
            message,
            propagator_name,
            operation: "extract",
        }
    }

    /// Creates an injection error.
    ///
    /// Used when injecting trace context into outgoing request headers fails.
    ///
    /// # Arguments
    ///
    /// * `message` - Description of the injection failure
    /// * `propagator_name` - Name of the propagator that failed
    ///
    /// # Example
    ///
    /// ```
    /// use datadog_agent_native::traces::propagation::error::Error;
    ///
    /// let err = Error::inject("cannot encode span ID", "DatadogHeaderPropagator");
    /// ```
    #[allow(clippy::must_use_candidate)]
    pub fn inject(message: &'static str, propagator_name: &'static str) -> Self {
        Self {
            message,
            propagator_name,
            operation: "inject",
        }
    }
}
