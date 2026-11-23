// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use async_trait::async_trait;
use libdd_capabilities_impl::NativeHttpClient;
use std::{sync::Arc, time};
use tokio::sync::mpsc::Receiver;
use tokio::sync::oneshot;
use tracing::{debug, error};

use libdd_trace_protobuf::pb;
use libdd_trace_utils::stats_utils;

use crate::config::Config;
use crate::stats_concentrator_service::StatsConcentratorHandle;

/// Serializes and sends a single `StatsPayload` to the intake.
async fn send_stats_payload(config: &Arc<Config>, mut payload: pb::StatsPayload) {
    if let Some(fields) = &config.agent_payload_fields {
        payload.agent_env = config.env.clone();
        payload.agent_version = fields.agent_version.clone();
    }
    debug!("Stats payload to be sent: {payload:?}");
    let serialized = match stats_utils::serialize_stats_payload(payload) {
        Ok(res) => res,
        Err(err) => {
            error!("Failed to serialize stats payload, dropping stats: {err}");
            return;
        }
    };
    #[allow(clippy::unwrap_used)]
    match stats_utils::send_stats_payload::<NativeHttpClient>(
        serialized,
        &config.trace_stats_intake,
        config.trace_stats_intake.api_key.as_ref().unwrap(),
    )
    .await
    {
        Ok(_) => debug!("Successfully flushed stats"),
        Err(e) => error!("Error sending stats: {e:?}"),
    }
}

#[async_trait]
pub trait StatsFlusher {
    /// Starts a stats flusher that listens for stats payloads sent to the tokio mpsc Receiver,
    /// implementing flushing logic that calls flush_stats. Runs until the shutdown signal fires,
    /// at which point it performs a final force flush and returns.
    async fn start_stats_flusher(
        &self,
        config: Arc<Config>,
        rx: Receiver<pb::ClientStatsPayload>,
        shutdown_rx: oneshot::Receiver<()>,
    );
    /// Flushes stats to the Datadog trace stats intake.
    /// `force_flush` controls whether in-progress concentrator buckets are flushed (true on
    /// shutdown, false on normal interval flushes).
    async fn flush_stats(
        &self,
        config: Arc<Config>,
        client_stats: Vec<pb::ClientStatsPayload>,
        force_flush: bool,
    );
}

#[derive(Clone)]
pub struct ServerlessStatsFlusher {
    pub stats_concentrator: Option<StatsConcentratorHandle>,
}

#[async_trait]
impl StatsFlusher for ServerlessStatsFlusher {
    async fn start_stats_flusher(
        &self,
        config: Arc<Config>,
        mut rx: Receiver<pb::ClientStatsPayload>,
        mut shutdown_rx: oneshot::Receiver<()>,
    ) {
        // A zero interval would panic in `interval`.
        let mut interval = tokio::time::interval(time::Duration::from_secs(
            config.stats_flush_interval_secs.max(1),
        ));
        let mut buffer: Vec<pb::ClientStatsPayload> = Vec::new();

        loop {
            tokio::select! {
                // Receive client stats and add them to the buffer
                Some(stats) = rx.recv() => {
                    buffer.push(stats);
                }

                // Drain client stats in buffer and stats from concentrator on interval
                _ = interval.tick() => {
                    let client_stats = std::mem::take(&mut buffer);
                    // Flush if trace stats are received from the tracer
                    // or if there is a stats concentrator for agent computed trace stats
                    if !client_stats.is_empty() || self.stats_concentrator.is_some() {
                        self.flush_stats(config.clone(), client_stats, false).await;
                    }
                }

                _ = &mut shutdown_rx => {
                    // Drain any client stats that arrived before the shutdown signal
                    while let Ok(stats) = rx.try_recv() {
                        buffer.push(stats);
                    }
                    // Force flush all in progress concentrator stats buckets on shutdown signal
                    self.flush_stats(config.clone(), std::mem::take(&mut buffer), true).await;
                    return;
                }
            }
        }
    }

    /// Flushes client computed stats from the tracer and serverless computed stats as separate payloads
    async fn flush_stats(
        &self,
        config: Arc<Config>,
        client_stats: Vec<pb::ClientStatsPayload>,
        force_flush: bool,
    ) {
        // Flush client computed stats from the tracer
        if !client_stats.is_empty() {
            let payload = stats_utils::construct_stats_payload(client_stats);
            send_stats_payload(&config, payload).await;
        }

        // Flush agent computed trace stats from the stats concentrator
        if let Some(ref concentrator) = self.stats_concentrator {
            match concentrator.flush(force_flush).await {
                Ok(agent_stats) if !agent_stats.is_empty() => {
                    let mut payload = stats_utils::construct_stats_payload(agent_stats);
                    payload.client_computed = false;
                    send_stats_payload(&config, payload).await;
                }
                Ok(_) => {}
                Err(e) => error!("Failed to flush concentrator stats: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_helpers::create_tcp_test_config;

    #[tokio::test]
    async fn start_stats_flusher_accepts_a_zero_interval() {
        let mut config = create_tcp_test_config(0);
        config.stats_flush_interval_secs = 0;
        let flusher = ServerlessStatsFlusher {
            stats_concentrator: None,
        };
        let (_stats_sender, stats_receiver) = tokio::sync::mpsc::channel(1);
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        shutdown_sender.send(()).unwrap();

        tokio::time::timeout(
            time::Duration::from_secs(2),
            flusher.start_stats_flusher(Arc::new(config), stats_receiver, shutdown_receiver),
        )
        .await
        .expect("the stats flusher did not return within 2 seconds");
    }
}
