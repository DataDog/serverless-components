// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use async_trait::async_trait;
use std::{sync::Arc, time};
use tokio::sync::{mpsc::Receiver, Mutex};
use tracing::{debug, error};

use libdd_common::{connector, hyper_migration};
use libdd_trace_utils::trace_utils;
use libdd_trace_utils::trace_utils::SendData;

use crate::aggregator::TraceAggregator;
use crate::config::Config;

#[async_trait]
pub trait TraceFlusher {
    fn new(aggregator: Arc<Mutex<TraceAggregator>>, config: Arc<Config>) -> Self
    where
        Self: Sized;
    /// Starts a trace flusher that listens for trace payloads sent to the tokio mpsc Receiver,
    /// implementing flushing logic that calls flush_traces.
    async fn start_trace_flusher(&self, mut rx: Receiver<SendData>);
    /// Given a `Vec<SendData>`, a tracer payload, send it to the Datadog intake endpoint.
    /// Returns the traces back if there was an error sending them.
    async fn send(&self, traces: Vec<SendData>) -> Option<Vec<SendData>>;

    /// Flushes traces by getting every available batch on the aggregator.
    /// If `failed_traces` is provided, it will attempt to send those instead of fetching new traces.
    /// Returns any traces that failed to send and should be retried.
    async fn flush(&self, failed_traces: Option<Vec<SendData>>) -> Option<Vec<SendData>>;
}

#[derive(Clone)]
#[allow(clippy::module_name_repetitions)]
pub struct ServerlessTraceFlusher {
    pub aggregator: Arc<Mutex<TraceAggregator>>,
    pub config: Arc<Config>,
}

#[async_trait]
impl TraceFlusher for ServerlessTraceFlusher {
    fn new(aggregator: Arc<Mutex<TraceAggregator>>, config: Arc<Config>) -> Self {
        ServerlessTraceFlusher { aggregator, config }
    }

    async fn start_trace_flusher(&self, mut rx: Receiver<SendData>) {
        let aggregator = Arc::clone(&self.aggregator);
        tokio::spawn(async move {
            while let Some(tracer_payload) = rx.recv().await {
                let mut guard = aggregator.lock().await;
                guard.add(tracer_payload);
            }
        });

        loop {
            tokio::time::sleep(time::Duration::from_secs(self.config.trace_flush_interval)).await;
            self.flush(None).await;
        }
    }

    async fn flush(&self, failed_traces: Option<Vec<SendData>>) -> Option<Vec<SendData>> {
        let mut failed_batch: Option<Vec<SendData>> = None;

        if let Some(traces) = failed_traces {
            // If we have traces from a previous failed attempt, try to send those first
            if !traces.is_empty() {
                debug!("Retrying to send {} previously failed traces", traces.len());
                let retry_result = self.send(traces).await;
                if retry_result.is_some() {
                    // Still failed, return to retry later
                    return retry_result;
                }
            }
        }

        // Process new traces from the aggregator
        let mut guard = self.aggregator.lock().await;
        let mut traces = guard.get_batch();

        while !traces.is_empty() {
            if let Some(failed) = self.send(traces).await {
                // Keep track of the failed batch
                failed_batch = Some(failed);
                // Stop processing more batches if we have a failure
                break;
            }

            traces = guard.get_batch();
        }

        failed_batch
    }

    async fn send(&self, traces: Vec<SendData>) -> Option<Vec<SendData>> {
        if traces.is_empty() {
            return None;
        }
        debug!("Flushing {} traces", traces.len());

        // Since we return the original traces on error, we need to clone them before coalescing
        let traces_clone = traces.clone();

        for coalesced_traces in trace_utils::coalesce_send_data(traces) {
            let send_result = if let Some(proxy_url) = self.config.proxy_url.as_deref() {
                match proxy_url.parse::<hyper::Uri>() {
                    Ok(proxy_addr) => {
                        match hyper_http_proxy::ProxyConnector::from_proxy(
                            connector::Connector::default(),
                            hyper_http_proxy::Proxy::new(
                                hyper_http_proxy::Intercept::Https,
                                proxy_addr,
                            ),
                        ) {
                            Ok(proxy_connector) => {
                                let client =
                                    hyper_migration::client_builder().build(proxy_connector);
                                coalesced_traces.send(&client).await.last_result
                            }
                            Err(e) => {
                                error!(
                                    "Failed to build proxy connector: {e:?}, using default client"
                                );
                                let client = hyper_migration::new_default_client();
                                coalesced_traces.send(&client).await.last_result
                            }
                        }
                    }
                    Err(e) => {
                        error!("Invalid proxy URL: {e:?}, using default client");
                        let client = hyper_migration::new_default_client();
                        coalesced_traces.send(&client).await.last_result
                    }
                }
            } else {
                let client = hyper_migration::new_default_client();
                coalesced_traces.send(&client).await.last_result
            };

            match send_result {
                Ok(_) => debug!("Successfully flushed traces"),
                Err(e) => {
                    error!("Error sending trace: {e:?}");
                    // Return original traces for retry
                    return Some(traces_clone);
                }
            }
        }
        None
    }
}
