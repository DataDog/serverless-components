// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use async_trait::async_trait;
use std::{error::Error, sync::Arc, time};
use tokio::sync::{Mutex, mpsc::Receiver, oneshot};
use tracing::{debug, error};

use http_body_util::BodyExt;
use libdd_capabilities::http::{HttpClientCapability, HttpError};
use libdd_capabilities::{MaybeSend, Request, Response, SleepCapability};
use libdd_capabilities_impl::NativeSleepCapability;
use libdd_common::connector::Connector;
use libdd_common::http_common::{self, Body, GenericHttpClient};
use libdd_trace_protobuf::pb;
use libdd_trace_utils::send_with_retry::{
    CompressionStrategy, RetryStrategy, SendWithRetryResult, send_with_retry,
};
use libdd_trace_utils::trace_utils;
use libdd_trace_utils::trace_utils::SendData;
use libdd_trace_utils::tracer_payload::TracerPayloadCollection;
use prost::Message;

use crate::aggregator::TraceAggregator;
use crate::config::Config;

#[async_trait]
pub trait TraceFlusher {
    fn new(aggregator: Arc<Mutex<TraceAggregator>>, config: Arc<Config>) -> Self
    where
        Self: Sized;
    /// Adds the trace payloads received on `rx` to the aggregator and flushes on the configured
    /// interval. When `shutdown` fires, it keeps receiving until every sender is dropped, so a
    /// payload that is still being enqueued is not lost. It then flushes and returns.
    async fn start_trace_flusher(&self, rx: Receiver<SendData>, shutdown: oneshot::Receiver<()>);
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

    async fn start_trace_flusher(
        &self,
        mut rx: Receiver<SendData>,
        mut shutdown: oneshot::Receiver<()>,
    ) {
        // A zero interval would panic in `interval_at`.
        let period = time::Duration::from_secs(self.config.trace_flush_interval_secs.max(1));
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                Some(tracer_payload) = rx.recv() => {
                    self.aggregator.lock().await.add(tracer_payload);
                }
                _ = interval.tick() => self.flush().await,
                _ = &mut shutdown => break,
            }
        }

        while let Some(tracer_payload) = rx.recv().await {
            self.aggregator.lock().await.add(tracer_payload);
        }
        self.flush().await;
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

        for coalesced_traces in trace_utils::coalesce_send_data(traces) {
            let result = match (
                &self.config.agent_payload_fields,
                coalesced_traces.get_payloads(),
            ) {
                // `SendData::send` sends protobuf only with an API key.
                (Some(fields), TracerPayloadCollection::V07(tracer_payloads))
                    if coalesced_traces.get_target().api_key.is_some() =>
                {
                    let agent_payload = pb::AgentPayload {
                        env: self.config.env.clone(),
                        agent_version: fields.agent_version.clone(),
                        target_tps: fields.target_tps,
                        error_tps: self.config.error_sampler.target_tps,
                        tracer_payloads: tracer_payloads.clone(),
                        ..Default::default()
                    };
                    send_agent_payload(&http_client, coalesced_traces.get_target(), &agent_payload)
                        .await
                        .map(|_| ())
                        .map_err(|e| format!("{e:?}"))
                }
                // Retries are handled internally by SendData::send()
                _ => coalesced_traces
                    .send(&http_client)
                    .await
                    .last_result
                    .map(|_| ())
                    .map_err(|e| format!("{e:?}")),
            };
            match result {
                Ok(_) => debug!("Successfully flushed traces"),
                Err(e) => {
                    error!("Error sending trace: {e}");
                }
            }
        }
    }
}

/// Sends `agent_payload` with the retries and headers of `SendData::send`, except the tracer's
/// headers, which the Go trace agent does not send either. `SendData::send` builds its own agent
/// payload, without the fields of [`crate::config::AgentPayloadFields`].
async fn send_agent_payload(
    http_client: &ProxyHttpClient,
    target: &libdd_common::Endpoint,
    agent_payload: &pb::AgentPayload,
) -> SendWithRetryResult {
    let headers = hyper::http::HeaderMap::from_iter([
        (
            hyper::http::header::CONTENT_TYPE,
            hyper::http::HeaderValue::from_static("application/x-protobuf"),
        ),
        (
            hyper::http::HeaderName::from_static("datadog-send-real-http-status"),
            hyper::http::HeaderValue::from_static("1"),
        ),
    ]);
    send_with_retry(
        http_client,
        target,
        agent_payload.encode_to_vec(),
        &headers,
        &RetryStrategy::default(),
        CompressionStrategy::None,
    )
    .await
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
    // The `HttpClientCapability` constructors take no arguments, so we use `with_proxy` to
    // take in the proxy URL and build the client. Neither `new_client` nor
    // `new_periodic` is called on our code path — we only ever hand an
    // already-built instance to `SendData::send` — so their proxy-less clients are unused.
    fn with_proxy(proxy_https: Option<&String>) -> Result<Self, Box<dyn Error>> {
        Self::with_proxy_pooling(proxy_https, true)
    }

    fn with_proxy_pooling(
        proxy_https: Option<&String>,
        connection_pooling: bool,
    ) -> Result<Self, Box<dyn Error>> {
        let proxy_connector = match proxy_https {
            Some(proxy) => hyper_http_proxy::ProxyConnector::from_proxy(
                Connector::default(),
                hyper_http_proxy::Proxy::new(hyper_http_proxy::Intercept::Https, proxy.parse()?),
            )?,
            None => hyper_http_proxy::ProxyConnector::new(Connector::default())?,
        };
        let mut builder = http_common::client_builder();
        if !connection_pooling {
            builder.pool_max_idle_per_host(0);
        }
        Ok(Self {
            client: builder.build(proxy_connector),
        })
    }
}

impl HttpClientCapability for ProxyHttpClient {
    #[allow(clippy::expect_used)]
    fn new_client() -> Self {
        Self::with_proxy(None).expect("building proxy connector with default TLS should not fail")
    }

    #[allow(clippy::expect_used)]
    fn new_periodic() -> Self {
        Self::with_proxy_pooling(None, false)
            .expect("building proxy connector with default TLS should not fail")
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

impl SleepCapability for ProxyHttpClient {
    #[allow(clippy::expect_used)]
    fn new() -> Self {
        Self::with_proxy(None).expect("building proxy connector with default TLS should not fail")
    }

    fn sleep(
        &self,
        duration: std::time::Duration,
    ) -> impl std::future::Future<Output = ()> + MaybeSend {
        NativeSleepCapability.sleep(duration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_helpers::create_tcp_test_config;

    #[tokio::test]
    async fn start_trace_flusher_accepts_a_zero_interval() {
        let mut config = create_tcp_test_config(0);
        config.trace_flush_interval_secs = 0;
        let flusher = ServerlessTraceFlusher::new(
            Arc::new(Mutex::new(TraceAggregator::default())),
            Arc::new(config),
        );
        let (trace_sender, trace_receiver) = tokio::sync::mpsc::channel(1);
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        drop(trace_sender);
        shutdown_sender.send(()).unwrap();

        tokio::time::timeout(
            time::Duration::from_secs(2),
            flusher.start_trace_flusher(trace_receiver, shutdown_receiver),
        )
        .await
        .expect("the trace flusher did not return within 2 seconds");
    }
}
