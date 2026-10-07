use std::str::FromStr;

use serde::{Deserialize, Deserializer};
use serde_json::Value;
use tracing::error;

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum LogLevel {
    /// Designates very serious errors.
    Error,
    /// Designates hazardous situations.
    #[default]
    Warn,
    /// Designates useful information.
    Info,
    /// Designates lower priority information.
    Debug,
    /// Designates very low priority, often extremely verbose, information.
    Trace,
}

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
    /// Construct a `log::LevelFilter` from a `LogLevel`
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

impl FromStr for LogLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "error" => Ok(LogLevel::Error),
            "warn" => Ok(LogLevel::Warn),
            "info" => Ok(LogLevel::Info),
            "debug" => Ok(LogLevel::Debug),
            "trace" => Ok(LogLevel::Trace),
            _ => Err(format!(
                "Invalid log level: '{s}'. Valid levels are: error, warn, info, debug, trace",
            )),
        }
    }
}

impl<'de> Deserialize<'de> for LogLevel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;

        if let Value::String(s) = value {
            match LogLevel::from_str(&s) {
                Ok(level) => Ok(level),
                // A `RUST_LOG`-style directive list, such as `info,cold_start_duration=debug`.
                // Its level is the last entry with no target, or warn if it has none, the same as
                // for an invalid value.
                Err(_) if s.contains(['=', ',']) => Ok(s
                    .rsplit(',')
                    .find_map(|entry| LogLevel::from_str(entry.trim()).ok())
                    .unwrap_or(LogLevel::Warn)),
                Err(e) => {
                    error!("{}", e);
                    Ok(LogLevel::Warn)
                }
            }
        } else {
            error!("Expected a string for log level, got {:?}", value);
            Ok(LogLevel::Warn)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LogLevel;
    use serde_json::json;

    fn parse(value: &str) -> LogLevel {
        serde_json::from_value(json!(value)).expect("log level always deserializes")
    }

    #[test]
    fn parses_plain_level() {
        assert_eq!(parse("DEBUG"), LogLevel::Debug);
    }

    #[test]
    fn takes_level_from_directives() {
        assert_eq!(parse("debug,cold_start_duration=off"), LogLevel::Debug);
        assert_eq!(parse("cold_start_duration=debug, warn"), LogLevel::Warn);
    }

    #[test]
    fn defaults_directives_without_level_to_warn() {
        assert_eq!(parse("cold_start_duration=debug"), LogLevel::Warn);
    }

    #[test]
    fn falls_back_to_warn_for_invalid_level() {
        assert_eq!(parse("verbose"), LogLevel::Warn);
    }
}
