// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, warn};

use crate::aggregator::{Aggregator, OversizePolicy};
use crate::intake_entry::IntakeEntry;

#[derive(Debug)]
enum AggregatorCommand {
    InsertBatch(Vec<IntakeEntry>),
    InsertSerializedBatch(Vec<String>),
    GetBatches(oneshot::Sender<Vec<Vec<u8>>>),
    Shutdown,
}

/// Cloneable handle for sending commands to a running [`AggregatorService`].
#[derive(Clone)]
pub struct AggregatorHandle {
    tx: mpsc::UnboundedSender<AggregatorCommand>,
}

impl AggregatorHandle {
    /// Queue a batch of log entries for aggregation.
    ///
    /// Returns an error only if the service has already stopped.
    pub fn insert_batch(&self, entries: Vec<IntakeEntry>) -> Result<(), String> {
        self.tx
            .send(AggregatorCommand::InsertBatch(entries))
            .map_err(|e| format!("failed to send InsertBatch: {e}"))
    }

    /// Queue log entries that are already serialized as JSON objects. They are batched
    /// as-is, without validation; the caller must pass valid JSON. The oversize policy
    /// still applies.
    ///
    /// Returns an error only if the service has already stopped.
    pub fn insert_serialized_batch(&self, entries: Vec<String>) -> Result<(), String> {
        self.tx
            .send(AggregatorCommand::InsertSerializedBatch(entries))
            .map_err(|e| format!("failed to send InsertSerializedBatch: {e}"))
    }

    /// Retrieve and drain all accumulated log batches as JSON arrays.
    ///
    /// Returns an empty `Vec` if the aggregator holds no logs.
    pub async fn get_batches(&self) -> Result<Vec<Vec<u8>>, String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(AggregatorCommand::GetBatches(tx))
            .map_err(|e| format!("failed to send GetBatches: {e}"))?;
        rx.await
            .map_err(|e| format!("failed to receive GetBatches response: {e}"))
    }

    /// Signal the service to stop processing and exit its run loop.
    pub fn shutdown(&self) -> Result<(), String> {
        self.tx
            .send(AggregatorCommand::Shutdown)
            .map_err(|e| format!("failed to send Shutdown: {e}"))
    }
}

/// Background tokio task owning a [`Aggregator`] and processing commands.
///
/// Create with [`AggregatorService::new`], spawn with `tokio::spawn(service.run())`,
/// and interact via the returned [`AggregatorHandle`].
pub struct AggregatorService {
    aggregator: Aggregator,
    rx: mpsc::UnboundedReceiver<AggregatorCommand>,
}

impl AggregatorService {
    /// Create a new service and its associated handle.
    pub fn new() -> (Self, AggregatorHandle) {
        let (tx, rx) = mpsc::unbounded_channel();
        let service = Self {
            aggregator: Aggregator::new(),
            rx,
        };
        let handle = AggregatorHandle { tx };
        (service, handle)
    }

    /// Set how entries larger than `MAX_LOG_BYTES` are handled. Default: drop them.
    #[must_use]
    pub fn with_oversize_policy(mut self, policy: OversizePolicy) -> Self {
        self.aggregator = std::mem::take(&mut self.aggregator).with_oversize_policy(policy);
        self
    }

    /// Run the service event loop.
    ///
    /// Returns when a `Shutdown` command is received or the last handle is dropped.
    pub async fn run(mut self) {
        debug!("log aggregator service started");

        while let Some(command) = self.rx.recv().await {
            match command {
                AggregatorCommand::InsertBatch(entries) => {
                    for entry in &entries {
                        if let Err(e) = self.aggregator.insert(entry) {
                            warn!("dropping log entry: {e}");
                        }
                    }
                }

                AggregatorCommand::InsertSerializedBatch(entries) => {
                    for entry in entries {
                        if let Err(e) = self.aggregator.insert_serialized(entry) {
                            warn!("dropping log entry: {e}");
                        }
                    }
                }

                AggregatorCommand::GetBatches(response_tx) => {
                    let batches = self.aggregator.get_all_batches();
                    if response_tx.send(batches).is_err() {
                        error!("failed to send GetBatches response — receiver dropped");
                    }
                }

                AggregatorCommand::Shutdown => {
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
    use crate::intake_entry::IntakeEntry;

    fn make_entry(msg: &str) -> IntakeEntry {
        IntakeEntry::from_message(msg, 1_700_000_000_000)
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

        let big = IntakeEntry::from_message("x".repeat(crate::constants::MAX_LOG_BYTES + 1), 0);
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

    use crate::test_support::LevelCounter;

    fn json_string_of_len(len: usize) -> String {
        format!("\"{}\"", "a".repeat(len - 2))
    }

    #[tokio::test]
    async fn test_insert_serialized_batch_roundtrip() {
        let (service, handle) = AggregatorService::new();
        let _task = tokio::spawn(service.run());
        handle
            .insert_serialized_batch(vec![r#"{"a":1}"#.to_string(), r#"{"b":2}"#.to_string()])
            .expect("insert");
        let batches = handle.get_batches().await.expect("batches");
        assert_eq!(batches, vec![br#"[{"a":1},{"b":2}]"#.to_vec()]);
    }

    #[tokio::test]
    async fn test_entry_and_serialized_inserts_share_one_queue() {
        let (service, handle) = AggregatorService::new();
        let _task = tokio::spawn(service.run());
        handle
            .insert_batch(vec![IntakeEntry::from_message("first", 1)])
            .expect("insert");
        handle
            .insert_serialized_batch(vec![r#"{"second":true}"#.to_string()])
            .expect("insert");
        let batches = handle.get_batches().await.expect("batches");
        assert_eq!(batches.len(), 1);
        let text = String::from_utf8(batches[0].clone()).expect("utf8");
        let first = text.find("first").expect("first entry");
        let second = text.find("second").expect("second entry");
        assert!(
            first < second,
            "entries keep insertion order across both insert paths"
        );
    }

    /// Default policy: an oversized pre-serialized entry is dropped with one warning.
    #[tokio::test(flavor = "current_thread")]
    async fn test_insert_serialized_batch_drops_oversized_entry_by_default() {
        let (logged, _guard) = LevelCounter::install();
        let (service, handle) = AggregatorService::new();
        let _task = tokio::spawn(service.run());
        handle
            .insert_serialized_batch(vec![json_string_of_len(
                crate::constants::MAX_LOG_BYTES + 1,
            )])
            .expect("insert");
        assert!(handle.get_batches().await.expect("batches").is_empty());
        assert_eq!((logged.warnings(), logged.errors()), (1, 0));
    }

    /// With `SendWithWarning`, each oversized entry that is kept logs exactly one warning.
    #[tokio::test(flavor = "current_thread")]
    async fn test_send_with_warning_warns_once_per_kept_oversized_entry() {
        let (logged, _guard) = LevelCounter::install();
        let (service, handle) = AggregatorService::new();
        let service = service.with_oversize_policy(OversizePolicy::SendWithWarning);
        let _task = tokio::spawn(service.run());
        let big = json_string_of_len(2 * crate::constants::MAX_LOG_BYTES);
        handle
            .insert_serialized_batch(vec![big.clone(), big])
            .expect("insert");
        assert!(!handle.get_batches().await.expect("batches").is_empty());
        assert_eq!((logged.warnings(), logged.errors()), (2, 0));
    }

    /// With `SendWithWarning`, an entry too big for any payload is dropped with one warning.
    #[tokio::test(flavor = "current_thread")]
    async fn test_send_with_warning_warns_once_per_entry_too_big_for_any_payload() {
        let (logged, _guard) = LevelCounter::install();
        let (service, handle) = AggregatorService::new();
        let service = service.with_oversize_policy(OversizePolicy::SendWithWarning);
        let _task = tokio::spawn(service.run());
        let too_big = json_string_of_len(crate::constants::MAX_CONTENT_BYTES);
        handle
            .insert_serialized_batch(vec![too_big])
            .expect("insert");
        assert!(handle.get_batches().await.expect("batches").is_empty());
        assert_eq!((logged.warnings(), logged.errors()), (1, 0));
    }
}
