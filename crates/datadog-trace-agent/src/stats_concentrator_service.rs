// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use libdd_library_config::tracer_metadata::TracerMetadata;
use libdd_trace_protobuf::pb::{ClientStatsPayload, TraceChunk};
use libdd_trace_stats::span_concentrator::SpanConcentrator;
use std::time::{Duration, SystemTime};
use tracing::error;

const S_TO_NS: u64 = 1_000_000_000;
const BUCKET_DURATION_NS: u64 = 10 * S_TO_NS; // 10 seconds
pub const SPAN_KINDS_STATS_COMPUTED: &[&str] = &["server", "client", "producer", "consumer"];

#[derive(Debug, thiserror::Error)]
pub enum StatsError {
    #[error("Failed to send command to concentrator: {0}")]
    SendError(Box<mpsc::error::SendError<ConcentratorCommand>>),
    #[error("Failed to receive response from concentrator: {0}")]
    RecvError(oneshot::error::RecvError),
}

pub enum ConcentratorCommand {
    AddChunk(Box<TraceChunk>, Arc<TracerMetadata>),
    Flush(bool, oneshot::Sender<Vec<ClientStatsPayload>>),
}

/// A cloneable handle to the stats concentrator service, safe to share across async tasks.
#[derive(Clone)]
pub struct StatsConcentratorHandle {
    tx: mpsc::UnboundedSender<ConcentratorCommand>,
}

impl StatsConcentratorHandle {
    #[must_use]
    pub fn new(tx: mpsc::UnboundedSender<ConcentratorCommand>) -> Self {
        Self { tx }
    }

    /// Adds a trace chunk for stats computation.
    pub fn add_chunk(
        &self,
        chunk: TraceChunk,
        metadata: Arc<TracerMetadata>,
    ) -> Result<(), StatsError> {
        self.tx
            .send(ConcentratorCommand::AddChunk(Box::new(chunk), metadata))
            .map_err(|e| StatsError::SendError(Box::new(e)))
    }

    pub async fn flush(&self, force_flush: bool) -> Result<Vec<ClientStatsPayload>, StatsError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.tx
            .send(ConcentratorCommand::Flush(force_flush, response_tx))
            .map_err(|e| StatsError::SendError(Box::new(e)))?;
        response_rx.await.map_err(StatsError::RecvError)
    }
}

fn new_concentrator() -> SpanConcentrator {
    // TODO: set peer_tag_keys
    SpanConcentrator::new(
        Duration::from_nanos(BUCKET_DURATION_NS),
        SystemTime::now(),
        SPAN_KINDS_STATS_COMPUTED
            .iter()
            .map(|s| s.to_string())
            .collect(),
        vec![],
    )
}

pub struct StatsConcentratorService {
    /// One concentrator per unique TracerMetadata.
    concentrators: HashMap<Arc<TracerMetadata>, SpanConcentrator>,
    rx: mpsc::UnboundedReceiver<ConcentratorCommand>,
    config: Arc<Config>,
}

impl StatsConcentratorService {
    #[must_use]
    pub fn new(config: Arc<Config>) -> (Self, StatsConcentratorHandle) {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = StatsConcentratorHandle::new(tx);
        let service = Self {
            concentrators: HashMap::new(),
            rx,
            config,
        };
        (service, handle)
    }

    pub async fn run(mut self) {
        while let Some(command) = self.rx.recv().await {
            match command {
                ConcentratorCommand::AddChunk(chunk, metadata) => {
                    // A single tracer may produce payloads with different metadata depending on the
                    // integration. For example, the .NET process integration appends `-command` to
                    // the base service and omits the version. A separate concentrator is kept per
                    // unique metadata so that each payload's stats are flushed with the metadata
                    // from the originating payload.
                    let concentrator = self
                        .concentrators
                        .entry(Arc::clone(&metadata))
                        .or_insert_with(new_concentrator);

                    for span in &chunk.spans {
                        concentrator.add_span(span);
                    }
                }
                ConcentratorCommand::Flush(force_flush, response_tx) => {
                    self.handle_flush(force_flush, response_tx);
                }
            }
        }
    }

    fn handle_flush(
        &mut self,
        force_flush: bool,
        response_tx: oneshot::Sender<Vec<ClientStatsPayload>>,
    ) {
        let payloads = self
            .concentrators
            .iter_mut()
            .filter_map(|(metadata, concentrator)| {
                let stats_buckets = concentrator.flush(SystemTime::now(), force_flush);
                if stats_buckets.is_empty() {
                    return None;
                }
                Some(ClientStatsPayload {
                    // Do not set hostname so the trace stats backend can aggregate stats properly
                    hostname: String::new(),
                    // Prefer env from the tracer payload, fall back to agent config
                    env: metadata
                        .service_env
                        .clone()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| self.config.env.clone()),
                    version: metadata.service_version.clone().unwrap_or_default(),
                    lang: metadata.tracer_language.clone(),
                    tracer_version: metadata.tracer_version.clone(),
                    // Not set for agent-computed stats; runtime_id identifies tracer-computed payloads
                    runtime_id: String::new(),
                    // Not supported yet
                    sequence: 0,
                    // Not supported yet
                    agent_aggregation: String::new(),
                    service: metadata.service_name.clone().unwrap_or_default(),
                    container_id: metadata.container_id.clone().unwrap_or_default(),
                    // Not supported yet
                    tags: vec![],
                    // Not supported yet
                    git_commit_sha: String::new(),
                    // Not supported yet
                    image_tag: String::new(),
                    stats: stats_buckets,
                    // Not supported yet
                    process_tags: String::new(),
                    // Not supported yet
                    process_tags_hash: 0,
                })
            })
            .collect();
        if let Err(e) = response_tx.send(payloads) {
            error!("Failed to return trace stats: {e:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_helpers::create_tcp_test_config;
    use libdd_trace_protobuf::pb::TraceChunk;

    fn make_metadata(language: &str, service: &str, version: &str) -> Arc<TracerMetadata> {
        Arc::new(TracerMetadata {
            tracer_language: language.to_string(),
            service_name: Some(service.to_string()),
            service_version: Some(version.to_string()),
            ..Default::default()
        })
    }

    fn empty_chunk() -> TraceChunk {
        TraceChunk {
            spans: vec![],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_unique_metadata_gets_separate_concentrators() {
        let config = Arc::new(create_tcp_test_config(0));
        let (service, handle) = StatsConcentratorService::new(config);
        tokio::spawn(service.run());

        handle
            .add_chunk(
                empty_chunk(),
                make_metadata("python", "my-service", "1.0.0"),
            )
            .unwrap();
        handle
            .add_chunk(
                empty_chunk(),
                make_metadata("python", "my-service", "2.0.0"),
            )
            .unwrap();
        handle
            .add_chunk(
                empty_chunk(),
                make_metadata("nodejs", "my-service", "1.0.0"),
            )
            .unwrap();

        assert!(handle.flush(false).await.is_ok());
    }

    #[tokio::test]
    async fn test_continues_with_same_metadata() {
        let config = Arc::new(create_tcp_test_config(0));
        let (service, handle) = StatsConcentratorService::new(config);
        tokio::spawn(service.run());

        handle
            .add_chunk(
                empty_chunk(),
                make_metadata("python", "my-service", "1.0.0"),
            )
            .unwrap();
        handle
            .add_chunk(
                empty_chunk(),
                make_metadata("python", "my-service", "1.0.0"),
            )
            .unwrap();

        assert!(handle.flush(false).await.is_ok());
    }
}
