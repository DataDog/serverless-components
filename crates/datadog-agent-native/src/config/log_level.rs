//! Log level configuration for the Datadog Agent.
//!
//! This module defines the `LogLevel` enum and provides parsing from strings (case-insensitive)
//! and deserialization from config files and environment variables.
//!
//! # Log Levels
//!
//! The agent supports five log levels, ordered from most to least verbose:
//! - **ERROR**: Very serious errors that prevent normal operation
//! - **WARN**: Hazardous situations that may lead to errors (default)
//! - **INFO**: Useful information about normal operations
//! - **DEBUG**: Lower priority information for debugging
//! - **TRACE**: Very low priority, extremely verbose information for troubleshooting
//!
//! # Configuration
//!
//! Log level can be set via:
//! - **Environment variable**: `DD_LOG_LEVEL=debug`
//! - **YAML config**: `log_level: debug`
//! - **Programmatically**: `Config { log_level: LogLevel::Debug, .. }`
//!
//! # Default
//!
//! If no log level is specified or an invalid value is provided, the agent defaults to **WARN**.

use std::str::FromStr;

use serde::{Deserialize, Deserializer};
use serde_json::Value;
use tracing::error;

/// Agent log level controlling verbosity of logging output.
///
/// This enum represents the five log levels supported by the agent, ordered from most to least
/// verbose. The default level is `Warn`, which provides a balance between visibility and noise.
///
/// # Parsing
///
/// Log levels can be parsed from strings (case-insensitive):
/// ```
/// use datadog_agent_native::config::log_level::LogLevel;
/// use std::str::FromStr;
///
/// let level = LogLevel::from_str("debug").unwrap();
/// assert_eq!(level, LogLevel::Debug);
///
/// let level = LogLevel::from_str("ERROR").unwrap(); // Case-insensitive
/// assert_eq!(level, LogLevel::Error);
/// ```
///
/// # Conversion
///
/// Can be converted to `log::LevelFilter` for integration with the `log` crate:
/// ```
/// use datadog_agent_native::config::log_level::LogLevel;
///
/// let filter = LogLevel::Debug.as_level_filter();
/// assert_eq!(filter, log::LevelFilter::Debug);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum LogLevel {
    /// Designates very serious errors that prevent normal operation.
    ///
    /// Use for: Fatal errors, panics, or conditions that require immediate attention.
    Error,
    /// Designates hazardous situations that may lead to errors.
    ///
    /// Use for: Recoverable errors, deprecated features, or potentially problematic conditions.
    /// This is the **default** log level.
    #[default]
    Warn,
    /// Designates useful information about normal operations.
    ///
    /// Use for: Agent startup/shutdown, configuration changes, or important state transitions.
    Info,
    /// Designates lower priority information useful for debugging.
    ///
    /// Use for: Detailed operation information, request/response details, or diagnostic data.
    Debug,
    /// Designates very low priority, extremely verbose information.
    ///
    /// Use for: Granular execution details, every function call, or troubleshooting rare issues.
    Trace,
}

/// Provides string representation of log levels in uppercase format.
///
/// Returns:
/// - `"ERROR"` for `LogLevel::Error`
/// - `"WARN"` for `LogLevel::Warn`
/// - `"INFO"` for `LogLevel::Info`
/// - `"DEBUG"` for `LogLevel::Debug`
/// - `"TRACE"` for `LogLevel::Trace`
impl AsRef<str> for LogLevel {
    fn as_ref(&self) -> &str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Warn => "WARN",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
            LogLevel::Trace => "TRACE",
        }
    }
}

impl LogLevel {
    /// Converts this `LogLevel` to a `log::LevelFilter` for use with the `log` crate.
    ///
    /// This allows integration with Rust's standard logging infrastructure via the `log` crate.
    ///
    /// # Examples
    ///
    /// ```
    /// use datadog_agent_native::config::log_level::LogLevel;
    ///
    /// let level = LogLevel::Debug;
    /// let filter = level.as_level_filter();
    /// assert_eq!(filter, log::LevelFilter::Debug);
    /// ```
    #[must_use]
    pub fn as_level_filter(self) -> log::LevelFilter {
        match self {
            LogLevel::Error => log::LevelFilter::Error,
            LogLevel::Warn => log::LevelFilter::Warn,
            LogLevel::Info => log::LevelFilter::Info,
            LogLevel::Debug => log::LevelFilter::Debug,
            LogLevel::Trace => log::LevelFilter::Trace,
        }
    }
}

/// Parses log levels from strings with case-insensitive matching.
///
/// # Supported Inputs
///
/// The following strings are accepted (case-insensitive):
/// - `"error"` → `LogLevel::Error`
/// - `"warn"` → `LogLevel::Warn`
/// - `"info"` → `LogLevel::Info`
/// - `"debug"` → `LogLevel::Debug`
/// - `"trace"` → `LogLevel::Trace`
///
/// # Errors
///
/// Returns an error string describing the invalid input and listing valid options.
///
/// # Examples
///
/// ```
/// use datadog_agent_native::config::log_level::LogLevel;
/// use std::str::FromStr;
///
/// // Case-insensitive parsing
/// assert_eq!(LogLevel::from_str("debug").unwrap(), LogLevel::Debug);
/// assert_eq!(LogLevel::from_str("DEBUG").unwrap(), LogLevel::Debug);
/// assert_eq!(LogLevel::from_str("DeBuG").unwrap(), LogLevel::Debug);
///
/// // Invalid input returns error
/// assert!(LogLevel::from_str("invalid").is_err());
/// ```
impl FromStr for LogLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Convert to lowercase for case-insensitive matching
        match s.to_lowercase().as_str() {
            "error" => Ok(LogLevel::Error),
            "warn" => Ok(LogLevel::Warn),
            "info" => Ok(LogLevel::Info),
            "debug" => Ok(LogLevel::Debug),
            "trace" => Ok(LogLevel::Trace),
            // Invalid log level - return descriptive error
            _ => Err(format!(
                "Invalid log level: '{s}'. Valid levels are: error, warn, info, debug, trace",
            )),
        }
    }
}

/// Deserializes log levels from config files and environment variables.
///
/// # Behavior
///
/// - **Valid string input**: Parsed via `FromStr` (case-insensitive)
/// - **Invalid string**: Logs error, returns `LogLevel::Warn` (default)
/// - **Non-string input**: Logs error, returns `LogLevel::Warn` (default)
///
/// This implementation is lenient - it never fails deserialization. Instead, it logs
/// errors and falls back to the default `Warn` level, ensuring the agent can start
/// even with invalid configuration.
///
/// # Examples
///
/// ```
/// use datadog_agent_native::config::log_level::LogLevel;
/// use serde_json::json;
///
/// // Valid log level
/// let level: LogLevel = serde_json::from_value(json!("debug")).unwrap();
/// assert_eq!(level, LogLevel::Debug);
///
/// // Invalid log level defaults to Warn (and logs error)
/// let level: LogLevel = serde_json::from_value(json!("invalid")).unwrap();
/// assert_eq!(level, LogLevel::Warn);
///
/// // Non-string input defaults to Warn (and logs error)
/// let level: LogLevel = serde_json::from_value(json!(123)).unwrap();
/// assert_eq!(level, LogLevel::Warn);
/// ```
impl<'de> Deserialize<'de> for LogLevel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;

        // Check if the value is a string
        if let Value::String(s) = value {
            // Try to parse the string as a log level
            match LogLevel::from_str(&s) {
                Ok(level) => Ok(level),
                Err(e) => {
                    // Invalid log level string - log error and use default
                    error!("{}", e);
                    Ok(LogLevel::Warn)
                }
            }
        } else {
            // Non-string value - log error and use default
            error!("Expected a string for log level, got {:?}", value);
            Ok(LogLevel::Warn)
        }
    }
}
