// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use libdd_library_config::tracer_metadata::TracerMetadata;
use libdd_trace_protobuf::pb::{ClientStatsPayload, TraceChunk};
use libdd_trace_stats::span_concentrator::SpanConcentrator;
use std::time::{Duration, SystemTime};
use tracing::{debug, error};

const S_TO_NS: u64 = 1_000_000_000;
const BUCKET_DURATION_NS: u64 = 10 * S_TO_NS; // 10 seconds

#[derive(Debug, thiserror::Error)]
pub enum StatsError {
    #[error("Failed to send command to concentrator: {0}")]
    SendError(Box<mpsc::error::SendError<ConcentratorCommand>>),
    #[error("Failed to receive response from concentrator: {0}")]
    RecvError(oneshot::error::RecvError),
}

pub enum ConcentratorCommand {
    AddChunk(Box<TraceChunk>, Arc<TracerMetadata>),
    Flush(bool, oneshot::Sender<Option<ClientStatsPayload>>),
}

/// A cloneable handle to the stats concentrator service, safe to share across async tasks.
#[derive(Clone)]
pub struct StatsConcentratorHandle {
    tx: mpsc::UnboundedSender<ConcentratorCommand>,
}

impl StatsConcentratorHandle {
    #[must_use]
    pub fn new(
        tx: mpsc::UnboundedSender<ConcentratorCommand>,
    ) -> Self {
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
            .map_err(|e| {
                StatsError::SendError(Box::new(e))
            })
    }

    pub async fn flush(&self, force_flush: bool) -> Result<Option<ClientStatsPayload>, StatsError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.tx
            .send(ConcentratorCommand::Flush(force_flush, response_tx))
            .map_err(|e| {
                StatsError::SendError(Box::new(e))
            })?;
        response_rx.await.map_err(StatsError::RecvError)
    }
}

pub struct StatsConcentratorService {
    concentrator: SpanConcentrator,
    rx: mpsc::UnboundedReceiver<ConcentratorCommand>,
    tracer_metadata: Option<Arc<TracerMetadata>>,
    config: Arc<Config>,
}

impl StatsConcentratorService {
    #[must_use]
    pub fn new(config: Arc<Config>) -> (Self, StatsConcentratorHandle) {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = StatsConcentratorHandle::new(tx);
        // TODO: set span_kinds_stats_computed and peer_tag_keys
        let concentrator = SpanConcentrator::new(
            Duration::from_nanos(BUCKET_DURATION_NS),
            SystemTime::now(),
            vec![],
            vec![],
        );
        let service = Self {
            concentrator,
            rx,
            tracer_metadata: None,
            config,
        };
        (service, handle)
    }

    pub async fn run(mut self) {
        while let Some(command) = self.rx.recv().await {
            match command {
                ConcentratorCommand::AddChunk(chunk, metadata) => {
                    if self.tracer_metadata.is_none() {
                        self.tracer_metadata = Some(metadata);
                    }
                    for span in &chunk.spans {
                        self.concentrator.add_span(span);
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
        response_tx: oneshot::Sender<Option<ClientStatsPayload>>,
    ) {
        let stats_buckets = self.concentrator.flush(SystemTime::now(), force_flush);
        let stats = if stats_buckets.is_empty() {
            None
        } else {
            let default_metadata = TracerMetadata::default();
            let metadata = self.tracer_metadata.as_deref().unwrap_or(&default_metadata);
            Some(ClientStatsPayload {
                // Do not set hostname so the trace stats backend can aggregate stats properly
                hostname: String::new(),
                // Prefer env from the tracer payload, fall back to agent config
                env: metadata
                    .service_env
                    .clone()
                    .filter(|s| !s.is_empty())
                    .or_else(|| self.config.env.clone())
                    .unwrap_or_default(),
                version: metadata.service_version.clone().unwrap_or_default(),
                lang: metadata.tracer_language.clone(),
                tracer_version: metadata.tracer_version.clone(),
                // Not set for agent-computed stats; runtime_id identifies tracer-computed payloads
                runtime_id: String::new(),
                // Not supported yet
                sequence: 0,
                // Not supported yet
                agent_aggregation: String::new(),
                // One service per app for serverless
                service: self.config.service.clone().unwrap_or_default(),
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
        };
        if let Err(e) = response_tx.send(stats) {
            error!("Failed to return trace stats: {e:?}");
        }
    }
}
