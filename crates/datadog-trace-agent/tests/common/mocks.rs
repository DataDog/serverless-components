// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Mock implementations of trace agent components for testing

use datadog_trace_agent::{
    config::Config,
    env_verifier::EnvVerifier,
    stats_flusher::StatsFlusher,
    stats_processor::StatsProcessor,
    trace_flusher::TraceFlusher,
    trace_processor::TraceProcessor,
};
use libdd_common::hyper_migration;
use libdd_trace_protobuf::pb;
use libdd_trace_utils::trace_utils::{self, MiniAgentMetadata, SendData};
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender};

/// Mock trace processor that returns 200 OK for all requests
#[allow(dead_code)]
pub struct MockTraceProcessor;

#[async_trait::async_trait]
impl TraceProcessor for MockTraceProcessor {
    async fn process_traces(
        &self,
        _config: Arc<Config>,
        _req: hyper_migration::HttpRequest,
        _trace_tx: Sender<SendData>,
        _mini_agent_metadata: Arc<MiniAgentMetadata>,
    ) -> Result<hyper_migration::HttpResponse, hyper::http::Error> {
        hyper::Response::builder()
            .status(200)
            .body(hyper_migration::Body::from("{}"))
    }
}

/// Mock trace flusher that consumes messages without processing
pub struct MockTraceFlusher;

#[async_trait::async_trait]
impl TraceFlusher for MockTraceFlusher {
    fn new(
        _aggregator: Arc<tokio::sync::Mutex<datadog_trace_agent::aggregator::TraceAggregator>>,
        _config: Arc<Config>,
    ) -> Self {
        MockTraceFlusher
    }

    async fn start_trace_flusher(&self, mut trace_rx: Receiver<SendData>) {
        // Consume messages from the channel without processing them
        while let Some(_trace) = trace_rx.recv().await {
            // Just discard the trace - we're not testing the flusher
        }
    }

    async fn send(&self, _traces: Vec<SendData>) -> Option<Vec<SendData>> {
        None
    }

    async fn flush(&self, _failed_traces: Option<Vec<SendData>>) -> Option<Vec<SendData>> {
        None
    }
}

/// Mock stats processor that returns 200 OK for all requests
pub struct MockStatsProcessor;

#[async_trait::async_trait]
impl StatsProcessor for MockStatsProcessor {
    async fn process_stats(
        &self,
        _config: Arc<Config>,
        _req: hyper_migration::HttpRequest,
        _stats_tx: Sender<pb::ClientStatsPayload>,
    ) -> Result<hyper_migration::HttpResponse, hyper::http::Error> {
        hyper::Response::builder()
            .status(200)
            .body(hyper_migration::Body::from("{}"))
    }
}

/// Mock stats flusher that consumes messages without processing
pub struct MockStatsFlusher;

#[async_trait::async_trait]
impl StatsFlusher for MockStatsFlusher {
    async fn start_stats_flusher(
        &self,
        _config: Arc<Config>,
        mut stats_rx: Receiver<pb::ClientStatsPayload>,
    ) {
        // Consume messages from the channel without processing them
        while let Some(_stats) = stats_rx.recv().await {
            // Just discard the stats - we're not testing the flusher
        }
    }

    async fn flush_stats(&self, _config: Arc<Config>, _traces: Vec<pb::ClientStatsPayload>) {
        // Do nothing
    }
}

/// Mock environment verifier that returns default metadata
pub struct MockEnvVerifier;

#[async_trait::async_trait]
impl EnvVerifier for MockEnvVerifier {
    async fn verify_environment(
        &self,
        _timeout_ms: u64,
        _env_type: &trace_utils::EnvironmentType,
        _os: &str,
    ) -> MiniAgentMetadata {
        MiniAgentMetadata::default()
    }
}
