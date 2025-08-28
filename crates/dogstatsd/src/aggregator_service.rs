// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::aggregator::Aggregator;
use crate::datadog::Series;
use crate::metric::{Metric, SortedTags};
use datadog_protos::metrics::SketchPayload;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, warn};
use ustr::Ustr;

#[derive(Debug)]
pub enum AggregatorCommand {
    InsertBatch(Vec<Metric>),
    Flush(oneshot::Sender<FlushResponse>),
    GetEntryById {
        name: Ustr,
        tags: Option<SortedTags>,
        timestamp: i64,
        response_tx: oneshot::Sender<Option<Metric>>,
    },
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

    pub async fn get_entry_by_id(
        &self,
        name: Ustr,
        tags: Option<SortedTags>,
        timestamp: i64,
    ) -> Result<Option<Metric>, String> {
        let (response_tx, response_rx) = oneshot::channel();
        self.tx
            .send(AggregatorCommand::GetEntryById {
                name,
                tags,
                timestamp,
                response_tx,
            })
            .map_err(|e| format!("Failed to send get_entry_by_id command: {}", e))?;

        response_rx
            .await
            .map_err(|e| format!("Failed to receive get_entry_by_id response: {}", e))
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
                        // The only possible error here is an overflow
                        if let Err(_e) = self.aggregator.insert(metric) {
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

                AggregatorCommand::GetEntryById {
                    name,
                    tags,
                    timestamp,
                    response_tx,
                } => {
                    let entry = self.aggregator.get_entry_by_id(name, &tags, timestamp);
                    let response = entry.cloned();
                    if let Err(_) = response_tx.send(response) {
                        error!("Failed to send get_entry_by_id response - receiver dropped");
                    }
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
    async fn test_aggregator_service_get_entry_by_id() {
        use ustr::ustr;

        let (service, handle) =
            AggregatorService::new(EMPTY_TAGS, 1000).expect("Failed to create aggregator service");

        let service_task = tokio::spawn(service.run());

        let metric = parse("test_metric:42|c|#env:prod").expect("metric parse failed");
        let metric_name = metric.name;
        let metric_tags = metric.tags.clone();
        let metric_timestamp = metric.timestamp;

        handle
            .insert_batch(vec![metric.clone()])
            .expect("Failed to insert metric");

        let result = handle
            .get_entry_by_id(metric_name, metric_tags.clone(), metric_timestamp)
            .await
            .expect("Failed to get entry");

        assert!(result.is_some());
        let retrieved_metric = result.unwrap();
        assert_eq!(retrieved_metric.name, metric_name);
        assert_eq!(retrieved_metric.timestamp, metric_timestamp);

        let non_existent = handle
            .get_entry_by_id(ustr("non_existent"), None, 0)
            .await
            .expect("Failed to get entry");

        assert!(non_existent.is_none());

        // Shutdown the service
        handle.shutdown().expect("Failed to shutdown");
        service_task.await.expect("Service task failed");
    }
}
