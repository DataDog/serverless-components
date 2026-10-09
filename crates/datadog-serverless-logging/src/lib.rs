// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Log filter setup shared by the serverless agents.

use tracing_subscriber::{
    EnvFilter,
    filter::{Directive, ParseError},
};

/// Comma-separated `<target>=<level>` entries, each of which sets the log level of one tracing
/// target. The other targets keep the agent's default level.
pub const LEVEL_BY_TARGET_ENV_VAR: &str = "DD_LOG_LEVEL_BY_TARGET";

/// Builds a log filter from the agent's base filter and a [`LEVEL_BY_TARGET_ENV_VAR`] value.
/// The base filter uses the same comma-separated syntax, and it also sets the default level. For
/// example, `h2=off,info` turns off the `h2` logs and sets the default level to info.
///
/// A target is a custom target that a statement sets, such as `cold_start_duration`, or a
/// module path, such as `bottlecap::traces` or the crate name `dogstatsd`. An entry matches
/// every target that starts with its text. The crate README lists the kinds of targets.
///
/// Returns the entries that are not valid, so that the caller can log them after it sets up the
/// subscriber. A typo then gives a warning instead of a failed startup.
///
/// # Errors
///
/// Returns an error if `base` is not a valid filter.
pub fn build_env_filter(
    base: &str,
    levels_by_target: &str,
) -> Result<(EnvFilter, Vec<String>), ParseError> {
    let mut env_filter = EnvFilter::try_new(base)?;
    let mut invalid_entries = Vec::new();
    for entry in levels_by_target
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        match parse_target_level(entry) {
            Some(directive) => env_filter = env_filter.add_directive(directive),
            None => invalid_entries.push(entry.to_string()),
        }
    }
    Ok((env_filter, invalid_entries))
}

/// Parses one `<target>=<level>` entry. Rejects the other forms that `EnvFilter` accepts, such as a
/// span filter like `[span]`, or a level with no target, which would replace the default level.
fn parse_target_level(entry: &str) -> Option<Directive> {
    let (target, level) = entry.split_once('=')?;
    let (target, level) = (target.trim(), level.trim());
    let valid_target =
        !target.is_empty() && !target.contains(|c: char| c.is_whitespace() || "[]{}".contains(c));
    let valid_level = matches!(
        level.to_ascii_lowercase().as_str(),
        "off" | "error" | "warn" | "info" | "debug" | "trace"
    );
    if !(valid_target && valid_level) {
        return None;
    }
    format!("{target}={level}").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::build_env_filter;

    fn filter_entries(base: &str, levels_by_target: &str) -> (Vec<String>, Vec<String>) {
        let (filter, invalid) =
            build_env_filter(base, levels_by_target).expect("base filter is valid");
        let entries = filter.to_string().split(',').map(String::from).collect();
        (entries, invalid)
    }

    #[test]
    fn adds_level_per_entry() {
        let (entries, invalid) = filter_entries(
            "h2=off,info",
            " cold_start_duration=debug, trace_flush_duration = TRACE ,",
        );
        assert!(entries.contains(&"cold_start_duration=debug".to_string()));
        assert!(entries.contains(&"trace_flush_duration=trace".to_string()));
        assert!(entries.contains(&"info".to_string()));
        assert!(invalid.is_empty());
    }

    #[test]
    fn skips_invalid_entries() {
        let (entries, invalid) = filter_entries(
            "h2=off,info",
            "debug,cold_start_duration,cold_start_duration=verbose,=debug,[span]=debug,a b=debug,trace_flush_duration=debug",
        );
        assert!(entries.contains(&"trace_flush_duration=debug".to_string()));
        assert!(entries.contains(&"info".to_string()));
        assert!(!entries.contains(&"debug".to_string()));
        assert_eq!(
            invalid,
            vec![
                "debug",
                "cold_start_duration",
                "cold_start_duration=verbose",
                "=debug",
                "[span]=debug",
                "a b=debug",
            ]
        );
    }

    #[test]
    fn overrides_base_filter() {
        let (entries, _) = filter_entries("h2=off,debug", "bottlecap::logs=warn,h2=debug");
        assert!(entries.contains(&"bottlecap::logs=warn".to_string()));
        assert!(entries.contains(&"h2=debug".to_string()));
        assert!(!entries.contains(&"h2=off".to_string()));
    }

    #[test]
    fn rejects_invalid_base() {
        assert!(build_env_filter("info,cold_start_duration=verbose", "").is_err());
    }
}
