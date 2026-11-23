// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! The telemetry components that run while the child or the sidecar runs.

use std::time::Instant;

use datadog_serverless_core::shutdown::{FlushBudget, run_with_budget};
use datadog_trace_agent::config::SpanTags;

use crate::gcp_metadata;
use crate::health;
use crate::logs_agent::{LogSink, LogsAgent};
use crate::metrics_agent::{self, MetricsAgent};
use crate::mode::Mode;
use crate::platform::{self, Kind, Platform};
use crate::settings;
use crate::trace_agent::TraceAgent;

/// The running components, kept until shutdown.
pub struct Components {
    trace_agent: Option<TraceAgent>,
    metrics_agent: Option<MetricsAgent>,
    logs_agent: Option<LogsAgent>,
}

/// Starts every component for the mode, with the platform and its tags found once, after the
/// Google Cloud metadata on Google Cloud Run. The mode's
/// differences reach the components through the environment defaults that `main` sets before
/// they start, and the mode tags.
///
/// Returns Go's fatal error message, before anything starts, when the platform lacks a required
/// setting.
pub async fn start(mode: &Mode) -> Result<Components, &'static str> {
    let sidecar = matches!(mode, Mode::Sidecar);
    let lookup =
        |name: &str| std::env::var_os(name).map(|value| value.to_string_lossy().into_owned());
    let mut platform = Platform::detect(lookup, platform::hostname);
    if let Some(message) = platform.missing_required_setting(lookup) {
        return Err(message);
    }
    start_health_endpoint(settings::health_port(lookup), dogstatsd_runs(lookup)).await?;
    if platform.kind == Kind::GoogleCloudRunService {
        platform = platform.with_gcp_metadata(&gcp_metadata::fetch(gcp_metadata::BASE_URL).await);
    }
    let tags = platform::telemetry_tags(&platform, sidecar, platform::VERSION, |name| {
        std::env::var(name).ok()
    });
    Ok(Components {
        trace_agent: TraceAgent::start(SpanTags {
            origin: platform.origin().to_string(),
            tags: tags.clone(),
        }),
        metrics_agent: MetricsAgent::start(sidecar, &platform, &tags),
        logs_agent: LogsAgent::start(&tags, platform.default_logs_source()),
    })
}

/// Serves the health endpoint on the port, when there is one. Like Go, a port that cannot be
/// listened on stops the start before the child runs.
async fn start_health_endpoint(port: Option<i64>, dogstatsd: bool) -> Result<(), &'static str> {
    let Some(port) = port else {
        return Ok(());
    };
    let listener = match u16::try_from(port) {
        Ok(port) => health::bind(port).await.map_err(|error| error.to_string()),
        Err(_) => Err(format!("invalid port {port}")),
    };
    match listener {
        Ok(listener) => {
            tracing::debug!("Health check listening on port {port}");
            tokio::spawn(health::serve(listener, Instant::now(), dogstatsd));
            Ok(())
        }
        Err(error) => {
            tracing::error!("Failed to start the health endpoint on port {port}: {error}");
            Err("Failed to start the health endpoint")
        }
    }
}

/// Whether the DogStatsD server runs, by the metrics agent's own decision.
fn dogstatsd_runs(lookup: impl Fn(&str) -> Option<String>) -> bool {
    metrics_agent::settings_from_environment(lookup)
        .is_ok_and(|settings| settings.dogstatsd_disabled().is_none())
}

impl Components {
    /// Where the child's output goes as logs, when logs are on.
    pub fn log_sink(&self) -> Option<LogSink> {
        self.logs_agent.as_ref().map(LogsAgent::sink)
    }

    /// Flushes traces and stats, metrics, and logs, concurrently, each within its share of the
    /// default flush budget. The stats flush runs inside the trace agent's shutdown, so it shares
    /// the traces budget.
    pub async fn flush_at_shutdown(self) {
        let budget = FlushBudget::default();
        let traces = async {
            if let Some(trace_agent) = self.trace_agent {
                run_with_budget("traces", budget.traces, trace_agent.flush_at_shutdown()).await;
            }
        };
        let metrics = async {
            if let Some(metrics_agent) = self.metrics_agent {
                run_with_budget(
                    "metrics",
                    budget.metrics,
                    metrics_agent.flush_at_shutdown(budget.metrics),
                )
                .await;
            }
        };
        let logs = async {
            if let Some(logs_agent) = self.logs_agent {
                run_with_budget("logs", budget.logs, logs_agent.flush_at_shutdown()).await;
            }
        };
        tokio::join!(traces, metrics, logs);
    }
}
