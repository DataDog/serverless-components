// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! The log filter, set from `DD_LOG_LEVEL` with Go serverless-init's level names.

/// The crates whose logs follow `DD_LOG_LEVEL`. Every other crate, such as hyper, logs only
/// errors.
const SERVERLESS_CRATES: &[&str] = &[
    "datadog_init",
    "datadog_serverless_core",
    "datadog_trace_agent",
    "datadog_agent_trace_sampler",
    "dogstatsd",
    "datadog_logs_agent",
    "datadog_metrics_collector",
    "datadog_fips",
];

/// Maps a Go log level name to a tracing level. `critical` maps to `error` and `warning` to
/// `warn`. Returns `None` for an unknown name.
fn tracing_level(go_level: &str) -> Option<&'static str> {
    match go_level.trim().to_lowercase().as_str() {
        "trace" => Some("trace"),
        "debug" => Some("debug"),
        "info" => Some("info"),
        "warn" | "warning" => Some("warn"),
        "error" | "critical" => Some("error"),
        "off" => Some("off"),
        _ => None,
    }
}

/// Builds the filter directives for a `DD_LOG_LEVEL` value. The default and an unknown value
/// both give `error`, as Go serverless-init does by default.
pub fn filter_directives(raw_level: Option<&str>) -> String {
    match raw_level.and_then(tracing_level).unwrap_or("error") {
        level @ ("error" | "off") => level.to_string(),
        level => std::iter::once("error".to_string())
            .chain(
                SERVERLESS_CRATES
                    .iter()
                    .map(|name| format!("{name}={level}")),
            )
            .collect::<Vec<_>>()
            .join(","),
    }
}

/// Logs to stdout at the level of `DD_LOG_LEVEL`. An unknown level is reported after the
/// subscriber is installed.
pub fn init() {
    let raw_level = std::env::var("DD_LOG_LEVEL").ok();
    let filter = tracing_subscriber::EnvFilter::try_new(filter_directives(raw_level.as_deref()))
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stdout)
        .without_time()
        .init();
    if let Some(raw_level) = raw_level.filter(|level| tracing_level(level).is_none()) {
        tracing::error!("DD_LOG_LEVEL={raw_level:?} is not a valid level, so error is used");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_directives_table() {
        let serverless_info = "error,datadog_init=info,datadog_serverless_core=info,\
            datadog_trace_agent=info,datadog_agent_trace_sampler=info,dogstatsd=info,\
            datadog_logs_agent=info,datadog_metrics_collector=info,datadog_fips=info";
        let cases: &[(Option<&str>, &str)] = &[
            (None, "error"),
            (Some("error"), "error"),
            (Some("critical"), "error"),
            (Some("CRITICAL"), "error"),
            (Some("off"), "off"),
            (Some("not a level"), "error"),
            (Some("info"), serverless_info),
            (Some(" Info "), serverless_info),
            (
                Some("warning"),
                "error,datadog_init=warn,datadog_serverless_core=warn,datadog_trace_agent=warn,\
                 datadog_agent_trace_sampler=warn,dogstatsd=warn,datadog_logs_agent=warn,\
                 datadog_metrics_collector=warn,datadog_fips=warn",
            ),
            (
                Some("trace"),
                "error,datadog_init=trace,datadog_serverless_core=trace,\
                 datadog_trace_agent=trace,datadog_agent_trace_sampler=trace,dogstatsd=trace,\
                 datadog_logs_agent=trace,datadog_metrics_collector=trace,datadog_fips=trace",
            ),
        ];
        for (raw, expected) in cases {
            assert_eq!(filter_directives(*raw), *expected, "level {raw:?}");
        }
    }
}
