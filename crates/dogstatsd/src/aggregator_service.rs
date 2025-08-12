// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::aggregator::Aggregator;
use crate::datadog::Series;
use crate::metric::{Metric, SortedTags};
use datadog_protos::metrics::SketchPayload;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, warn};

#[derive(Debug)]
pub enum AggregatorCommand {
    InsertBatch(Vec<Metric>),
    Flush(oneshot::Sender<FlushResponse>),
    Clear,
    Shutdown,
}

#[derive(Debug)]
pub struct FlushResponse {
    pub series: Vec<Series>,
    pub distributions: Vec<SketchPayload>,
}

#[derive(Clone)]
pub struct AggregatorHandle {
    tx: mpsc::UnboundedSender<AggregatorCommand>,
}

impl AggregatorHandle {
    pub fn insert_batch(
        &self,
        metrics: Vec<Metric>,
    ) -> Result<(), mpsc::error::SendError<AggregatorCommand>> {
        self.tx.send(AggregatorCommand::InsertBatch(metrics))
    }

    pub async fn flush(&self) -> Result<FlushResponse, String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.tx
            .send(AggregatorCommand::Flush(response_tx))
            .map_err(|e| format!("Failed to send flush command: {}", e))?;

        response_rx
            .await
            .map_err(|e| format!("Failed to receive flush response: {}", e))
    }

    pub fn clear(&self) -> Result<(), mpsc::error::SendError<AggregatorCommand>> {
        self.tx.send(AggregatorCommand::Clear)
    }

    pub fn shutdown(&self) -> Result<(), mpsc::error::SendError<AggregatorCommand>> {
        self.tx.send(AggregatorCommand::Shutdown)
    }
}

pub struct AggregatorService {
    aggregator: Aggregator,
    rx: mpsc::UnboundedReceiver<AggregatorCommand>,
}

impl AggregatorService {
    pub fn new(
        tags: SortedTags,
        max_context: usize,
    ) -> Result<(Self, AggregatorHandle), crate::errors::Creation> {
        let (tx, rx) = mpsc::unbounded_channel();
        let aggregator = Aggregator::new(tags, max_context)?;

        let service = Self { aggregator, rx };

        let handle = AggregatorHandle { tx };

        Ok((service, handle))
    }

    pub async fn run(mut self) {
        debug!("Aggregator service started");

        while let Some(command) = self.rx.recv().await {
            match command {
                AggregatorCommand::InsertBatch(metrics) => {
                    let mut insert_errors = 0;
                    for metric in metrics {
                        if let Err(e) = self.aggregator.insert(metric) {
                            insert_errors += 1;
                        }
                    }
                    if insert_errors > 0 {
                        warn!("Total of {} metrics failed to insert", insert_errors);
                    }
                }

                AggregatorCommand::Flush(response_tx) => {
                    let series = self.aggregator.consume_metrics();
                    let distributions = self.aggregator.consume_distributions();

                    let response = FlushResponse {
                        series,
                        distributions,
                    };

                    if let Err(_) = response_tx.send(response) {
                        error!("Failed to send flush response - receiver dropped");
                    }
                }

                AggregatorCommand::Clear => {
                    self.aggregator.clear();
                    debug!("Aggregator cleared");
                }

                AggregatorCommand::Shutdown => {
                    debug!("Aggregator service shutting down");
                    break;
                }
            }
        }

        debug!("Aggregator service stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metric::{parse, EMPTY_TAGS};

    #[tokio::test]
    async fn test_aggregator_service_basic_flow() {
        let (service, handle) =
            AggregatorService::new(EMPTY_TAGS, 1000).expect("Failed to create aggregator service");

        // Start the service in a background task
        let service_task = tokio::spawn(service.run());

        // Insert some metrics
        let metrics = vec![
            parse("test:1|c|#k:v").expect("metric parse failed"),
            parse("foo:2|c|#k:v").expect("metric parse failed"),
        ];

        handle
            .insert_batch(metrics)
            .expect("Failed to insert metrics");

        // Flush and check results
        let response = handle.flush().await.expect("Failed to flush");
        assert_eq!(response.series.len(), 1);
        assert_eq!(response.series[0].series.len(), 2);

        // Shutdown the service
        handle.shutdown().expect("Failed to shutdown");
        service_task.await.expect("Service task failed");
    }

    #[tokio::test]
    async fn test_aggregator_service_distributions() {
        let (service, handle) =
            AggregatorService::new(EMPTY_TAGS, 1000).expect("Failed to create aggregator service");

        // Start the service in a background task
        let service_task = tokio::spawn(service.run());

        // Insert distribution metrics
        let metrics = vec![
            parse("dist1:100|d|#k:v").expect("metric parse failed"),
            parse("dist2:200|d|#k:v").expect("metric parse failed"),
        ];

        handle
            .insert_batch(metrics)
            .expect("Failed to insert metrics");

        // Flush and check results
        let response = handle.flush().await.expect("Failed to flush");
        assert_eq!(response.distributions.len(), 1);
        assert_eq!(response.distributions[0].sketches.len(), 2);
        assert_eq!(response.series.len(), 0);

        // Shutdown the service
        handle.shutdown().expect("Failed to shutdown");
        service_task.await.expect("Service task failed");
    }

    #[tokio::test]
    async fn test_aggregator_service_clear() {
        let (service, handle) =
            AggregatorService::new(EMPTY_TAGS, 1000).expect("Failed to create aggregator service");

        // Start the service in a background task
        let service_task = tokio::spawn(service.run());

        // Insert metrics
        let metrics = vec![parse("test:1|c|#k:v").expect("metric parse failed")];

        handle
            .insert_batch(metrics)
            .expect("Failed to insert metrics");

        // Clear the aggregator
        handle.clear().expect("Failed to clear");

        // Flush should return empty results
        let response = handle.flush().await.expect("Failed to flush");
        assert_eq!(response.series.len(), 0);
        assert_eq!(response.distributions.len(), 0);

        // Shutdown the service
        handle.shutdown().expect("Failed to shutdown");
        service_task.await.expect("Service task failed");
    }
}
