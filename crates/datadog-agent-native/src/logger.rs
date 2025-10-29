//! Custom tracing formatter for Datadog agent logs.
//!
//! This module provides a custom log formatter that prefixes all log messages
//! with `DD_EXTENSION` for easy identification in Lambda environments.
//!
//! # Format
//!
//! The formatter produces logs in this format:
//! ```text
//! DD_EXTENSION | LEVEL | [span_name{span_fields}:] message {event_fields}
//! ```
//!
//! # Examples
//!
//! ```text
//! DD_EXTENSION | INFO | agent started port=8124
//! DD_EXTENSION | ERROR | traces_endpoint{attempt=3}: Failed to send trace timeout=30s
//! DD_EXTENSION | DEBUG | logs_aggregator{queue_size=42}: Flushing logs count=42
//! ```
//!
//! # Usage
//!
//! ```rust,ignore
//! use tracing_subscriber::FmtSubscriber;
//! use datadog_agent_native::logger::Formatter;
//!
//! let subscriber = FmtSubscriber::builder()
//!     .event_format(Formatter)
//!     .with_max_level(tracing::Level::INFO)
//!     .finish();
//!
//! tracing::subscriber::set_global_default(subscriber)?;
//! ```
//!
//! # Span Context
//!
//! The formatter includes all active spans in the log output, providing
//! hierarchical context for nested operations. Span fields are shown in
//! curly braces `{field=value}`.

use std::fmt;
use tracing_core::{Event, Subscriber};
use tracing_subscriber::fmt::{
    format::{self, FormatEvent, FormatFields},
    FmtContext, FormattedFields,
};
use tracing_subscriber::registry::LookupSpan;

/// Custom log formatter that prefixes messages with `DD_EXTENSION`.
///
/// This formatter is used to make Datadog agent logs easily identifiable
/// in Lambda CloudWatch Logs alongside application logs.
///
/// # Format Structure
///
/// 1. **Prefix**: Always `DD_EXTENSION` for filtering
/// 2. **Level**: Log level (ERROR, WARN, INFO, DEBUG, TRACE)
/// 3. **Span Context**: Active spans with their fields (if any)
/// 4. **Message**: The log message
/// 5. **Event Fields**: Structured fields attached to the event
///
/// # Example Output
///
/// ```text
/// DD_EXTENSION | INFO | trace_processor{batch_size=10}: Processing traces count=10
/// ```
///
/// Here:
/// - `DD_EXTENSION` = Prefix for filtering
/// - `INFO` = Log level
/// - `trace_processor{batch_size=10}` = Active span with field
/// - `Processing traces count=10` = Message with event field
#[derive(Debug, Clone, Copy)]
pub struct Formatter;

impl<S, N> FormatEvent<S, N> for Formatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    /// Formats a tracing event as a log line.
    ///
    /// This method implements the core formatting logic, including:
    /// - Adding the `DD_EXTENSION` prefix
    /// - Formatting the log level
    /// - Including span context hierarchy
    /// - Appending event fields
    ///
    /// # Arguments
    ///
    /// * `ctx` - Formatting context with access to spans
    /// * `writer` - Output writer for the formatted message
    /// * `event` - The tracing event to format
    ///
    /// # Returns
    ///
    /// * `Ok(())` - If formatting succeeded
    /// * `Err` - If writing to the output failed
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: format::Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        // Write prefix and log level: "DD_EXTENSION | LEVEL | "
        let metadata = event.metadata();
        write!(&mut writer, "DD_EXTENSION | {} | ", metadata.level())?;

        // Format all the spans in the event's span context (from root to current)
        // Decision: Include full span hierarchy for complete context
        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                // Write span name
                write!(writer, "{}", span.name())?;

                // Get span fields that were formatted during `new_span`
                // These are stored in the span's extensions by the `fmt` layer
                let ext = span.extensions();
                let fields = &ext
                    .get::<FormattedFields<N>>()
                    .expect("will never be `None`");

                // Write span fields in curly braces if present
                // Decision: Only show braces if fields exist (cleaner output)
                if !fields.is_empty() {
                    write!(writer, "{{{fields}}}")?;
                }
                write!(writer, ": ")?;
            }
        }

        // Write the event's message and fields
        // Decision: Let the field formatter handle message + fields together
        ctx.field_format().format_fields(writer.by_ref(), event)?;

        // Terminate the log line
        writeln!(writer)
    }
}
