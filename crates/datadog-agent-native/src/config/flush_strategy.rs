//! Flush strategy configuration for serverless and containerized environments.
//!
//! This module defines how and when the agent flushes telemetry data to Datadog.
//! Different strategies are optimized for different execution models:
//! - **Serverless** (AWS Lambda, Azure Functions): Flush at invocation end to ensure no data loss
//! - **Long-running containers**: Flush periodically for real-time data delivery
//! - **Hybrid**: Combine periodic and end-of-invocation flushing
//!
//! # Flush Strategies
//!
//! - **Default**: Flush every 1 second + at invocation end (safest)
//! - **End**: Flush only at invocation end (minimal overhead, serverless-optimized)
//! - **Periodically**: Flush every N milliseconds (long-running processes)
//! - **EndPeriodically**: Flush every N milliseconds + at invocation end (hybrid)
//! - **Continuously**: Non-blocking async flush (high-throughput scenarios)
//!
//! # Configuration
//!
//! Flush strategy can be configured via:
//! - **Environment variable**: `DD_SERVERLESS_FLUSH_STRATEGY=end` or `DD_SERVERLESS_FLUSH_STRATEGY=periodically,5000`
//! - **YAML config**: `flush_strategy: "end,1000"`

use serde::{Deserialize, Deserializer};
use tracing::debug;

/// Periodic flush configuration specifying the flush interval in milliseconds.
///
/// Used by periodic flush strategies to determine how often to send data to Datadog.
///
/// # Examples
///
/// ```
/// use datadog_agent_native::config::flush_strategy::PeriodicStrategy;
///
/// // Flush every 5 seconds
/// let strategy = PeriodicStrategy { interval: 5000 };
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PeriodicStrategy {
    /// Flush interval in milliseconds.
    ///
    /// Common values:
    /// - `1000` - Every 1 second (default, near real-time)
    /// - `5000` - Every 5 seconds (balanced)
    /// - `10000` - Every 10 seconds (reduced overhead)
    /// - `60000` - Every 1 minute (minimal overhead for cold starts)
    pub interval: u64,
}

/// Flush strategy determining when telemetry data is sent to Datadog.
///
/// The flush strategy impacts:
/// - **Data freshness**: How quickly data appears in Datadog UI
/// - **Performance overhead**: CPU/memory/network usage
/// - **Data loss risk**: Probability of losing data on crashes/timeouts
///
/// # Strategy Selection Guide
///
/// | Environment | Recommended Strategy | Rationale |
/// |------------|---------------------|-----------|
/// | AWS Lambda | `End` or `EndPeriodically(1000)` | Ensures flush before timeout |
/// | Long-running container | `Periodically(5000)` | Real-time data delivery |
/// | High-throughput API | `Continuously(1000)` | Non-blocking, async flushing |
/// | Development | `Default` | Safe, predictable behavior |
///
/// # Parsing
///
/// Supports multiple string formats:
/// - `"end"` → Flush only at invocation end
/// - `"periodically,5000"` → Flush every 5 seconds
/// - `"end,1000"` → Flush every 1 second + at invocation end
/// - `"continuously,1000"` → Non-blocking flush every 1 second
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FlushStrategy {
    /// Default strategy: Flush every 1 second and at the end of the invocation.
    ///
    /// This is the safest strategy, providing both:
    /// - Near real-time data delivery (1s periodic flush)
    /// - No data loss on crashes (end-of-invocation flush)
    ///
    /// **Use when**: You want predictable, safe behavior without tuning.
    Default,
    /// Flush periodically at the specified interval (in milliseconds).
    ///
    /// Does NOT block on invocation completion. Data may be lost if the process
    /// terminates before the next flush.
    ///
    /// **Use when**: Long-running processes where invocation boundaries don't exist.
    ///
    /// **Example**: `Periodically(PeriodicStrategy { interval: 5000 })` flushes every 5 seconds.
    Periodically(PeriodicStrategy),
    /// Flush only at the end of the invocation.
    ///
    /// Minimizes overhead during execution by deferring all flushing until the end.
    /// Ideal for short-lived serverless functions with predictable execution times.
    ///
    /// **Use when**: Serverless functions with short execution time (<1s) where periodic
    /// flushing would add unnecessary overhead.
    ///
    /// **Warning**: If the process crashes or times out before flushing, data will be lost.
    End,
    /// Flush both periodically and at the end of the invocation (hybrid strategy).
    ///
    /// Combines periodic flushing for real-time data delivery with end-of-invocation
    /// flushing to ensure no data loss.
    ///
    /// **Use when**: Long-running serverless functions (>5s) or containers with defined
    /// invocation boundaries.
    ///
    /// **Example**: `EndPeriodically(PeriodicStrategy { interval: 1000 })` flushes
    /// every 1 second and at invocation end.
    EndPeriodically(PeriodicStrategy),
    /// Flush asynchronously in the background without blocking the next invocation.
    ///
    /// Data is flushed continuously in a non-blocking manner, allowing the next
    /// invocation to start immediately without waiting for the flush to complete.
    ///
    /// **Use when**: High-throughput scenarios where blocking on flush would impact
    /// latency or throughput.
    ///
    /// **Warning**: Data may be buffered longer, and there's a small risk of data loss
    /// on abrupt termination.
    ///
    /// **Example**: `Continuously(PeriodicStrategy { interval: 1000 })` flushes
    /// asynchronously every 1 second.
    Continuously(PeriodicStrategy),
}

/// A concrete flush strategy (excluding the `Default` variant).
///
/// This is a restricted subset of `FlushStrategy` used internally after the `Default`
/// strategy has been resolved to a concrete implementation. The `Default` variant
/// must be translated to one of the concrete strategies before use.
///
/// # Why This Exists
///
/// The `Default` variant is a placeholder that must be resolved to an actual strategy
/// (typically `EndPeriodically` with a 1-second interval). This enum ensures that
/// internal code paths only work with concrete, resolved strategies.
#[allow(clippy::module_name_repetitions)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConcreteFlushStrategy {
    /// Flush periodically at the specified interval.
    Periodically(PeriodicStrategy),
    /// Flush only at the end of the invocation.
    End,
    /// Flush both periodically and at the end of the invocation.
    EndPeriodically(PeriodicStrategy),
    /// Flush asynchronously in the background.
    Continuously(PeriodicStrategy),
}

/// Deserializes flush strategies from config strings.
///
/// # Supported Formats
///
/// - **Simple strategy**: `"end"` → `FlushStrategy::End`
/// - **Periodic**: `"periodically,5000"` → `FlushStrategy::Periodically(PeriodicStrategy { interval: 5000 })`
/// - **Continuous**: `"continuously,1000"` → `FlushStrategy::Continuously(PeriodicStrategy { interval: 1000 })`
/// - **End + Periodic**: `"end,1000"` → `FlushStrategy::EndPeriodically(PeriodicStrategy { interval: 1000 })`
///
/// # Error Handling
///
/// This implementation is lenient - it never fails. Invalid inputs default to `FlushStrategy::Default`
/// with a debug log message.
///
/// # Examples
///
/// ```
/// use datadog_agent_native::config::flush_strategy::FlushStrategy;
/// use serde_json::json;
///
/// // Simple "end" strategy
/// let strategy: FlushStrategy = serde_json::from_value(json!("end")).unwrap();
/// assert_eq!(strategy, FlushStrategy::End);
///
/// // Periodic with interval
/// let strategy: FlushStrategy = serde_json::from_value(json!("periodically,5000")).unwrap();
/// // Result: FlushStrategy::Periodically(PeriodicStrategy { interval: 5000 })
///
/// // Invalid input defaults to Default (with debug log)
/// let strategy: FlushStrategy = serde_json::from_value(json!("invalid")).unwrap();
/// assert_eq!(strategy, FlushStrategy::Default);
/// ```
impl<'de> Deserialize<'de> for FlushStrategy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;

        // Special case: "end" without interval means flush only at invocation end
        if value.as_str() == "end" {
            return Ok(FlushStrategy::End);
        }

        // Parse comma-separated format: "strategy,interval_ms"
        // Examples: "periodically,60000", "end,1000", "continuously,5000"
        let mut split_value = value.as_str().split(',');

        // Extract strategy name (before comma)
        let strategy = split_value.next();

        // Extract interval in milliseconds (after comma), parse as u64
        let interval: Option<u64> = split_value.next().and_then(|v| v.parse().ok());

        // Match strategy name and interval to determine flush strategy
        match (strategy, interval) {
            // "periodically,60000" → Flush every 60 seconds
            (Some("periodically"), Some(interval)) => {
                Ok(FlushStrategy::Periodically(PeriodicStrategy { interval }))
            }
            // "continuously,5000" → Non-blocking flush every 5 seconds
            (Some("continuously"), Some(interval)) => {
                Ok(FlushStrategy::Continuously(PeriodicStrategy { interval }))
            }
            // "end,1000" → Flush every 1 second AND at invocation end
            (Some("end"), Some(interval)) => Ok(FlushStrategy::EndPeriodically(PeriodicStrategy {
                interval,
            })),
            // Strategy name present but interval missing or invalid → default
            (Some(strategy), _) => {
                debug!("Invalid flush interval: {}, using default", strategy);
                Ok(FlushStrategy::Default)
            }
            // Both strategy and interval missing → default
            _ => {
                debug!("Invalid flush strategy: {}, using default", value);
                Ok(FlushStrategy::Default)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_end() {
        let flush_strategy: FlushStrategy = serde_json::from_str("\"end\"").unwrap();
        assert_eq!(flush_strategy, FlushStrategy::End);
    }

    #[test]
    fn deserialize_periodically() {
        let flush_strategy: FlushStrategy = serde_json::from_str("\"periodically,60000\"").unwrap();
        assert_eq!(
            flush_strategy,
            FlushStrategy::Periodically(PeriodicStrategy { interval: 60000 })
        );
    }

    #[test]
    fn deserialize_end_periodically() {
        let flush_strategy: FlushStrategy = serde_json::from_str("\"end,1000\"").unwrap();
        assert_eq!(
            flush_strategy,
            FlushStrategy::EndPeriodically(PeriodicStrategy { interval: 1000 })
        );
    }

    #[test]
    fn deserialize_invalid() {
        let flush_strategy: FlushStrategy = serde_json::from_str("\"invalid\"").unwrap();
        assert_eq!(flush_strategy, FlushStrategy::Default);
    }

    #[test]
    fn deserialize_invalid_interval() {
        let flush_strategy: FlushStrategy =
            serde_json::from_str("\"periodically,invalid\"").unwrap();
        assert_eq!(flush_strategy, FlushStrategy::Default);
    }

    #[test]
    fn deserialize_invalid_end_interval() {
        let flush_strategy: FlushStrategy = serde_json::from_str("\"end,invalid\"").unwrap();
        assert_eq!(flush_strategy, FlushStrategy::Default);
    }
}
