// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use async_trait::async_trait;
use std::{error::Error, sync::Arc, time};
use tokio::sync::{Mutex, mpsc::Receiver};
use tracing::{debug, error};

use http_body_util::BodyExt;
use libdd_capabilities::http::{HttpClientTrait, HttpError};
use libdd_capabilities::{MaybeSend, Request, Response};
use libdd_common::connector::Connector;
use libdd_common::http_common::{self, Body, GenericHttpClient};
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
    async fn send(&self, traces: Vec<SendData>);
    /// Flushes traces by getting every available batch on the aggregator.
    async fn flush(&self);
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
            self.flush().await;
        }
    }

    async fn flush(&self) {
        // Process traces from the aggregator
        let mut guard = self.aggregator.lock().await;
        let mut traces = guard.get_batch();

        while !traces.is_empty() {
            self.send(traces).await;
            traces = guard.get_batch();
        }
    }

    async fn send(&self, traces: Vec<SendData>) {
        if traces.is_empty() {
            return;
        }
        debug!("Flushing {} traces", traces.len());

        let http_client = match ProxyHttpClient::with_proxy(self.config.proxy_url.as_ref()) {
            Ok(client) => client,
            Err(e) => {
                error!("Failed to create HTTP client: {e:?}");
                return;
            }
        };

        // Retries are handled internally by SendData::send()
        for coalesced_traces in trace_utils::coalesce_send_data(traces) {
            let result = coalesced_traces.send(&http_client).await;
            match result.last_result {
                Ok(_) => debug!("Successfully flushed traces"),
                Err(e) => {
                    error!("Error sending trace: {e:?}");
                }
            }
        }
    }
}

#[derive(Clone)]
struct ProxyHttpClient {
    client: GenericHttpClient<hyper_http_proxy::ProxyConnector<Connector>>,
}

impl std::fmt::Debug for ProxyHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyHttpClient").finish()
    }
}

impl ProxyHttpClient {
    // `HttpClientTrait::new_client` takes no arguments, so we use `with_proxy` to
    // take in the proxy URL and build the client. `new_client` is never called on our code path.
    fn with_proxy(proxy_https: Option<&String>) -> Result<Self, Box<dyn Error>> {
        if let Some(proxy) = proxy_https {
            let proxy =
                hyper_http_proxy::Proxy::new(hyper_http_proxy::Intercept::Https, proxy.parse()?);
            let proxy_connector =
                hyper_http_proxy::ProxyConnector::from_proxy(Connector::default(), proxy)?;
            Ok(Self {
                client: http_common::client_builder().build(proxy_connector),
            })
        } else {
            let proxy_connector = hyper_http_proxy::ProxyConnector::new(Connector::default())?;
            Ok(Self {
                client: http_common::client_builder().build(proxy_connector),
            })
        }
    }
}

impl HttpClientTrait for ProxyHttpClient {
    #[allow(clippy::expect_used)]
    fn new_client() -> Self {
        Self::with_proxy(None).expect("building proxy connector with default TLS should not fail")
    }

    fn request(
        &self,
        req: Request<bytes::Bytes>,
    ) -> impl std::future::Future<Output = Result<Response<bytes::Bytes>, HttpError>> + MaybeSend
    {
        let client = self.client.clone();
        async move {
            let hyper_req = req.map(Body::from_bytes);
            let response = client
                .request(hyper_req)
                .await
                .map_err(|e| HttpError::Network(e.into()))?;
            let (parts, body) = response.into_parts();
            let collected = body
                .collect()
                .await
                .map_err(|e| HttpError::ResponseBody(e.into()))?
                .to_bytes();
            Ok(Response::from_parts(parts, collected))
        }
    }
}
