// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, warn};

use crate::aggregator::LogAggregator;
use crate::log_entry::LogEntry;

#[derive(Debug)]
enum LogAggregatorCommand {
    InsertBatch(Vec<LogEntry>),
    GetBatches(oneshot::Sender<Vec<Vec<u8>>>),
    Shutdown,
}

/// Cloneable handle for sending commands to a running [`AggregatorService`].
#[derive(Clone)]
pub struct AggregatorHandle {
    tx: mpsc::UnboundedSender<LogAggregatorCommand>,
}

impl AggregatorHandle {
    /// Queue a batch of log entries for aggregation.
    ///
    /// Returns an error only if the service has already stopped.
    pub fn insert_batch(&self, entries: Vec<LogEntry>) -> Result<(), String> {
        self.tx
            .send(LogAggregatorCommand::InsertBatch(entries))
            .map_err(|e| format!("failed to send InsertBatch: {e}"))
    }

    /// Retrieve and drain all accumulated log batches as JSON arrays.
    ///
    /// Returns an empty `Vec` if the aggregator holds no logs.
    pub async fn get_batches(&self) -> Result<Vec<Vec<u8>>, String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(LogAggregatorCommand::GetBatches(tx))
            .map_err(|e| format!("failed to send GetBatches: {e}"))?;
        rx.await
            .map_err(|e| format!("failed to receive GetBatches response: {e}"))
    }

    /// Signal the service to stop processing and exit its run loop.
    pub fn shutdown(&self) -> Result<(), String> {
        self.tx
            .send(LogAggregatorCommand::Shutdown)
            .map_err(|e| format!("failed to send Shutdown: {e}"))
    }
}

/// Background tokio task owning a [`LogAggregator`] and processing commands.
///
/// Create with [`AggregatorService::new`], spawn with `tokio::spawn(service.run())`,
/// and interact via the returned [`AggregatorHandle`].
pub struct AggregatorService {
    aggregator: LogAggregator,
    rx: mpsc::UnboundedReceiver<LogAggregatorCommand>,
}

impl AggregatorService {
    /// Create a new service and its associated handle.
    pub fn new() -> (Self, AggregatorHandle) {
        let (tx, rx) = mpsc::unbounded_channel();
        let service = Self {
            aggregator: LogAggregator::new(),
            rx,
        };
        let handle = AggregatorHandle { tx };
        (service, handle)
    }

    /// Run the service event loop.
    ///
    /// Returns when a `Shutdown` command is received or the last handle is dropped.
    pub async fn run(mut self) {
        debug!("log aggregator service started");

        while let Some(command) = self.rx.recv().await {
            match command {
                LogAggregatorCommand::InsertBatch(entries) => {
                    for entry in &entries {
                        if let Err(e) = self.aggregator.insert(entry) {
                            warn!("dropping log entry: {e}");
                        }
                    }
                }

                LogAggregatorCommand::GetBatches(response_tx) => {
                    let batches = self.aggregator.get_all_batches();
                    if response_tx.send(batches).is_err() {
                        error!("failed to send GetBatches response — receiver dropped");
                    }
                }

                LogAggregatorCommand::Shutdown => {
                    debug!("log aggregator service shutting down");
                    break;
                }
            }
        }

        debug!("log aggregator service stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_entry::LogEntry;

    fn make_entry(msg: &str) -> LogEntry {
        LogEntry::from_message(msg, 1_700_000_000_000)
    }

    #[tokio::test]
    async fn test_insert_and_get_batches_roundtrip() {
        let (service, handle) = AggregatorService::new();
        let task = tokio::spawn(service.run());

        handle
            .insert_batch(vec![make_entry("a"), make_entry("b")])
            .expect("insert_batch failed");

        let batches = handle.get_batches().await.expect("get_batches failed");
        assert_eq!(batches.len(), 1);
        let arr: serde_json::Value = serde_json::from_slice(&batches[0]).expect("json");
        assert_eq!(arr.as_array().unwrap().len(), 2);

        handle.shutdown().expect("shutdown failed");
        task.await.expect("task panicked");
    }

    #[tokio::test]
    async fn test_get_batches_empty_returns_empty_vec() {
        let (service, handle) = AggregatorService::new();
        let task = tokio::spawn(service.run());

        let batches = handle.get_batches().await.expect("get_batches");
        assert!(batches.is_empty());

        handle.shutdown().expect("shutdown");
        task.await.expect("task");
    }

    #[tokio::test]
    async fn test_oversized_entry_dropped_not_panicked() {
        let (service, handle) = AggregatorService::new();
        let task = tokio::spawn(service.run());

        let big = LogEntry::from_message("x".repeat(crate::constants::MAX_LOG_BYTES + 1), 0);
        handle.insert_batch(vec![big]).expect("send ok");

        let batches = handle.get_batches().await.expect("get_batches");
        assert!(
            batches.is_empty(),
            "oversized entry should have been dropped"
        );

        handle.shutdown().expect("shutdown");
        task.await.expect("task");
    }

    #[tokio::test]
    async fn test_handle_is_clone_and_both_can_insert() {
        let (service, handle) = AggregatorService::new();
        let task = tokio::spawn(service.run());

        let handle2 = handle.clone();
        handle
            .insert_batch(vec![make_entry("from h1")])
            .expect("h1");
        handle2
            .insert_batch(vec![make_entry("from h2")])
            .expect("h2");

        let batches = handle.get_batches().await.expect("get_batches");
        let arr: serde_json::Value = serde_json::from_slice(&batches[0]).expect("json");
        assert_eq!(arr.as_array().unwrap().len(), 2);

        handle.shutdown().expect("shutdown");
        task.await.expect("task");
    }
}
