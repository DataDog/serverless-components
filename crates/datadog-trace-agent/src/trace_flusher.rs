// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use async_trait::async_trait;
use std::{error::Error, sync::Arc, time};
use tokio::sync::{mpsc::Receiver, Mutex};
use tracing::{debug, error};

use libdd_common::{hyper_migration, GenericHttpClient};
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
            tokio::time::sleep(time::Duration::from_secs(
                self.config.trace_flush_interval_secs,
            ))
            .await;
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

        let http_client =
            match ServerlessTraceFlusher::get_http_client(self.config.proxy_url.as_ref()) {
                Ok(client) => client,
                Err(e) => {
                    error!("Failed to create HTTP client: {e:?}");
                    return None;
                }
            };

        for coalesced_traces in trace_utils::coalesce_send_data(traces) {
            match coalesced_traces.send(&http_client).await.last_result {
                Ok(_) => debug!("Successfully flushed traces"),
                Err(e) => {
                    error!("Error sending trace: {e:?}");
                    // Return the original traces for retry
                    return Some(traces_clone);
                }
            }
        }
        None
    }
}

impl ServerlessTraceFlusher {
    fn get_http_client(
        proxy_https: Option<&String>,
    ) -> Result<
        GenericHttpClient<hyper_http_proxy::ProxyConnector<libdd_common::connector::Connector>>,
        Box<dyn Error>,
    > {
        if let Some(proxy) = proxy_https {
            let proxy =
                hyper_http_proxy::Proxy::new(hyper_http_proxy::Intercept::Https, proxy.parse()?);
            let proxy_connector = hyper_http_proxy::ProxyConnector::from_proxy(
                libdd_common::connector::Connector::default(),
                proxy,
            )?;
            Ok(hyper_migration::client_builder().build(proxy_connector))
        } else {
            let proxy_connector = hyper_http_proxy::ProxyConnector::new(
                libdd_common::connector::Connector::default(),
            )?;
            Ok(hyper_migration::client_builder().build(proxy_connector))
        }
    }
}
