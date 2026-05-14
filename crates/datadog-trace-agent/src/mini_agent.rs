// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use http_body_util::BodyExt;
use hyper::service::service_fn;
use hyper::{Method, Response, StatusCode, http};
use libdd_common::http_common;
use serde_json::json;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::oneshot;
#[cfg(all(windows, feature = "windows-pipes"))]
use tracing::warn;
use tracing::{debug, error};

use crate::http_utils::{log_and_create_http_response, verify_request_content_length};
use crate::proxy_flusher::{ProxyFlusher, ProxyRequest, ProxyRequestKind};

#[cfg(all(windows, feature = "windows-pipes"))]
use tokio::net::windows::named_pipe::ServerOptions;

use crate::stats_concentrator_service::SPAN_KINDS_STATS_COMPUTED;
use crate::{config, env_verifier, stats_flusher, stats_processor, trace_flusher, trace_processor};
use libdd_trace_protobuf::pb;
use libdd_trace_utils::trace_utils;
use libdd_trace_utils::trace_utils::SendData;

const TRACE_ENDPOINT_PATH: &str = "/v0.4/traces";
const STATS_ENDPOINT_PATH: &str = "/v0.6/stats";
const INFO_ENDPOINT_PATH: &str = "/info";
const DSM_ENDPOINT_PATH: &str = "/v0.1/pipeline_stats";
const PROFILING_ENDPOINT_PATH: &str = "/profiling/v1/input";
const TRACER_PAYLOAD_CHANNEL_BUFFER_SIZE: usize = 10;
const STATS_PAYLOAD_CHANNEL_BUFFER_SIZE: usize = 10;
const PROXY_PAYLOAD_CHANNEL_BUFFER_SIZE: usize = 10;

/// JoinHandle type for a transport accept loop (TCP or named pipe).
type TransportHandle =
    tokio::task::JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>;

pub struct MiniAgent {
    pub config: Arc<config::Config>,
    pub trace_processor: Arc<dyn trace_processor::TraceProcessor + Send + Sync>,
    pub trace_flusher: Arc<dyn trace_flusher::TraceFlusher + Send + Sync>,
    pub stats_processor: Arc<dyn stats_processor::StatsProcessor + Send + Sync>,
    pub stats_flusher: Arc<dyn stats_flusher::StatsFlusher + Send + Sync>,
    pub env_verifier: Arc<dyn env_verifier::EnvVerifier + Send + Sync>,
    pub proxy_flusher: Arc<ProxyFlusher>,
}

impl MiniAgent {
    pub async fn start_mini_agent(
        &self,
        shutdown_rx: tokio::sync::watch::Receiver<bool>,
        stats_concentrator_service_handle: Option<tokio::task::JoinHandle<()>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let now = Instant::now();

        // verify we are in a serverless function environment. if not, shut down the mini agent.
        let mini_agent_metadata = Arc::new(
            self.env_verifier
                .verify_environment(
                    self.config.verify_env_timeout_ms,
                    &self.config.env_type,
                    &self.config.os,
                )
                .await,
        );

        debug!(
            "Time taken to fetch Mini Agent metadata: {} ms",
            now.elapsed().as_millis()
        );

        // setup a channel to send processed traces to our flusher. tx is passed through each
        // endpoint_handler to the trace processor, which uses it to send de-serialized
        // processed trace payloads to our trace flusher.
        let (trace_tx, trace_rx): (Sender<SendData>, Receiver<SendData>) =
            mpsc::channel(TRACER_PAYLOAD_CHANNEL_BUFFER_SIZE);

        // start our trace flusher. receives trace payloads and handles buffering + deciding when to
        // flush to backend.
        let trace_flusher = self.trace_flusher.clone();
        let trace_flusher_handle = tokio::spawn(async move {
            trace_flusher.start_trace_flusher(trace_rx).await;
        });

        // channels to send processed stats to our stats flusher.
        let (stats_tx, stats_rx): (
            Sender<pb::ClientStatsPayload>,
            Receiver<pb::ClientStatsPayload>,
        ) = mpsc::channel(STATS_PAYLOAD_CHANNEL_BUFFER_SIZE);

        // Separate shutdown channel for the stats flusher: serve() drains all
        // in-flight HTTP handlers before triggering the final flush, preventing
        // AddChunk/ClientStatsPayload messages from being missed.
        let (flusher_shutdown_tx, flusher_shutdown_rx) = oneshot::channel::<()>();

        // start our stats flusher.
        let stats_flusher = self.stats_flusher.clone();
        let stats_config = self.config.clone();
        let stats_flusher_handle = tokio::spawn(async move {
            stats_flusher
                .start_stats_flusher(stats_config, stats_rx, flusher_shutdown_rx)
                .await;
        });

        // channel used to forward proxied payloads (for example profiling and DSM)
        let (proxy_tx, proxy_rx): (Sender<ProxyRequest>, Receiver<ProxyRequest>) =
            mpsc::channel(PROXY_PAYLOAD_CHANNEL_BUFFER_SIZE);

        // start our proxy flusher for proxied requests
        let proxy_flusher = self.proxy_flusher.clone();
        tokio::spawn(async move {
            proxy_flusher.start_proxy_flusher(proxy_rx).await;
        });

        // setup our hyper http server, where the endpoint_handler handles incoming requests
        let trace_processor = self.trace_processor.clone();
        let stats_processor = self.stats_processor.clone();
        let endpoint_config = self.config.clone();

        let service = service_fn(move |req| {
            // called for each http request
            let trace_processor = trace_processor.clone();
            let trace_tx = trace_tx.clone();
            let stats_processor = stats_processor.clone();
            let stats_tx = stats_tx.clone();
            let endpoint_config = endpoint_config.clone();
            let mini_agent_metadata = Arc::clone(&mini_agent_metadata);

            let proxy_tx = proxy_tx.clone();

            MiniAgent::trace_endpoint_handler(
                endpoint_config,
                req.map(http_common::Body::incoming),
                trace_processor,
                trace_tx,
                stats_processor,
                stats_tx,
                mini_agent_metadata,
                proxy_tx,
            )
        });

        debug!(
            "Time taken to start the Mini Agent: {} ms",
            now.elapsed().as_millis()
        );

        let addr = SocketAddr::from(([127, 0, 0, 1], self.config.dd_apm_receiver_port));

        // TCP listener: required without a named pipe, fails safely when a named pipe is configured.
        // Multiple Azure apps share the same VM on certain windows Azure plans.
        // Only one agent can own a given VM port, which leads to competition over the default value.
        // On Windows, we use a unique named pipe to avoid this issue and a port bind failure is not fatal.
        //
        // If no named pipe is configured, TCP is the sole transport. A bind failure
        // here means the agent cannot receive any traces, so we propagate the error and
        // shut down.
        #[cfg(all(windows, feature = "windows-pipes"))]
        let tcp_listener: Option<tokio::net::TcpListener> = if self
            .config
            .dd_apm_windows_pipe_name
            .is_some()
        {
            match tokio::net::TcpListener::bind(&addr).await {
                Ok(l) => {
                    debug!(
                        "Mini Agent listening on TCP port {}",
                        self.config.dd_apm_receiver_port
                    );
                    Some(l)
                }
                Err(e) => {
                    // Another mini-agent on this host already owns the set port.
                    warn!(
                        "Mini Agent could not bind TCP port {} for APM receiver: {}. Named-pipe transport is active; if you are not seeing traces, you may need a newer tracer supporting named pipes.",
                        self.config.dd_apm_receiver_port, e
                    );
                    None
                }
            }
        } else {
            // No named pipe — TCP is the only transport; a bind failure is fatal.
            let l = tokio::net::TcpListener::bind(&addr).await?;
            debug!(
                "Mini Agent listening on TCP port {}",
                self.config.dd_apm_receiver_port
            );
            Some(l)
        };

        #[cfg(not(all(windows, feature = "windows-pipes")))]
        let tcp_listener: Option<tokio::net::TcpListener> = {
            // Non-Windows has no named-pipe alternative; TCP bind is always required.
            let l = tokio::net::TcpListener::bind(&addr).await?;
            debug!(
                "Mini Agent listening on TCP port {}",
                self.config.dd_apm_receiver_port
            );
            Some(l)
        };

        // Named pipe: only on Windows when feature-enabled and explicitly configured.
        // On all other builds the code is absent entirely (no symbols, no select arm).
        #[cfg(all(windows, feature = "windows-pipes"))]
        let pipe_handle = self
            .config
            .dd_apm_windows_pipe_name
            .as_ref()
            .map(|pipe_name| {
                debug!("Mini Agent also listening on named pipe {}", pipe_name);
                let pipe_service = service.clone();
                let pipe_shutdown_rx = shutdown_rx.clone();
                tokio::spawn(Self::serve_accept_loop_named_pipe(
                    pipe_name.clone(),
                    pipe_service,
                    pipe_shutdown_rx,
                ))
            });

        // Spawn the TCP accept loop only if we successfully bound the port.
        let tcp_handle: Option<TransportHandle> = tcp_listener.map(|l| {
            let tcp_shutdown_rx = shutdown_rx.clone();
            tokio::spawn(Self::serve_accept_loop_tcp(l, service, tcp_shutdown_rx))
        });

        Self::serve(
            tcp_handle,
            #[cfg(all(windows, feature = "windows-pipes"))]
            pipe_handle,
            trace_flusher_handle,
            stats_flusher_handle,
            stats_concentrator_service_handle,
            shutdown_rx,
            flusher_shutdown_tx,
        )
        .await
    }

    /// Supervises the long-lived tasks. Most task deaths are fatal because a
    /// tracer picks one transport at startup and stays on it — silently losing
    /// that transport would strand the tracer permanently.
    ///
    /// Exception: `tcp_handle` is `None` when the TCP bind was skipped (see
    /// `start_mini_agent`). In that case the named pipe is the active transport
    /// and a missing TCP listener is not an error.
    #[allow(clippy::too_many_arguments)]
    async fn serve(
        mut tcp_handle: Option<TransportHandle>,
        #[cfg(all(windows, feature = "windows-pipes"))] mut pipe_handle: Option<TransportHandle>,
        mut trace_flusher_handle: tokio::task::JoinHandle<()>,
        mut stats_flusher_handle: tokio::task::JoinHandle<()>,
        mut stats_concentrator_service_handle: Option<tokio::task::JoinHandle<()>>,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
        flusher_shutdown_tx: oneshot::Sender<()>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // task supervision cases: we react if...
        enum Event {
            TcpDied(String),
            #[cfg(all(windows, feature = "windows-pipes"))]
            PipeDied(String),
            TraceFlusherDied(String),
            StatsFlusherDied(String),
            ConcentratorDied(String),
            Shutdown,
        }

        #[cfg(all(windows, feature = "windows-pipes"))]
        let event = {
            // tcp_exit resolves only when TCP was actually started (Some). When
            // the bind was skipped (None) this future is permanently pending and
            // TcpDied never fires.
            let tcp_exit = async {
                match tcp_handle.as_mut() {
                    Some(h) => h.await,
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(tcp_exit);
            let pipe_exit = async {
                match pipe_handle.as_mut() {
                    Some(h) => h.await,
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(pipe_exit);
            let concentrator_exit = async {
                match stats_concentrator_service_handle.as_mut() {
                    Some(h) => h.await,
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(concentrator_exit);

            // biased: when shutdown_tx fires, the accept loops also see
            // shutdown_rx and exit cleanly. Their JoinHandles resolve in the
            // same poll cycle as our shutdown_rx.changed(); without biased,
            // a *Died arm can win the race and abort the supervisor before
            // it has a chance to signal the stats flusher to flush.
            tokio::select! {
                biased;
                _ = shutdown_rx.changed() => Event::Shutdown,
                r = &mut tcp_exit => Event::TcpDied(format!("{r:?}")),
                r = &mut pipe_exit => Event::PipeDied(format!("{r:?}")),
                r = &mut trace_flusher_handle => Event::TraceFlusherDied(format!("{r:?}")),
                r = &mut stats_flusher_handle => Event::StatsFlusherDied(format!("{r:?}")),
                r = &mut concentrator_exit => Event::ConcentratorDied(format!("{r:?}")),
            }
        };
        #[cfg(not(all(windows, feature = "windows-pipes")))]
        let event = {
            // On non-Windows, tcp_handle is always Some (TCP bind is required).
            let tcp_exit = async {
                match tcp_handle.as_mut() {
                    Some(h) => h.await,
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(tcp_exit);
            let concentrator_exit = async {
                match stats_concentrator_service_handle.as_mut() {
                    Some(h) => h.await,
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(concentrator_exit);

            // biased: see Windows branch above for rationale.
            tokio::select! {
                biased;
                _ = shutdown_rx.changed() => Event::Shutdown,
                r = &mut tcp_exit => Event::TcpDied(format!("{r:?}")),
                r = &mut trace_flusher_handle => Event::TraceFlusherDied(format!("{r:?}")),
                r = &mut stats_flusher_handle => Event::StatsFlusherDied(format!("{r:?}")),
                r = &mut concentrator_exit => Event::ConcentratorDied(format!("{r:?}")),
            }
        };

        let result: Result<(), Box<dyn std::error::Error>> = match event {
            Event::Shutdown => {
                // The same shutdown_rx fan-out has already fired in each
                // accept loop concurrently with our select arm here.
                // Awaiting the transport handles waits for them to drain.
                if let Some(h) = tcp_handle.as_mut()
                    && let Err(e) = h.await
                {
                    error!("TCP accept loop failed during shutdown: {e:?}");
                }
                #[cfg(all(windows, feature = "windows-pipes"))]
                if let Some(h) = pipe_handle.as_mut()
                    && let Err(e) = h.await
                {
                    error!("Named pipe accept loop failed during shutdown: {e:?}");
                }
                // Now all handlers have written to the channels. Force-flush
                // the stats flusher.
                let _ = flusher_shutdown_tx.send(());
                match (&mut stats_flusher_handle).await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        Err(format!("Stats flusher task failed during shutdown: {e:?}").into())
                    }
                }
            }
            Event::TcpDied(s) => {
                error!("TCP accept loop died: {s}");
                Err("TCP accept loop terminated unexpectedly".into())
            }
            #[cfg(all(windows, feature = "windows-pipes"))]
            Event::PipeDied(s) => {
                error!("Named pipe accept loop died: {s}");
                Err("Named pipe accept loop terminated unexpectedly".into())
            }
            Event::TraceFlusherDied(s) => {
                error!("Trace flusher task died: {s}");
                Err("Trace flusher task terminated unexpectedly".into())
            }
            Event::StatsFlusherDied(s) => {
                error!("Stats flusher task died: {s}");
                Err("Stats flusher task terminated unexpectedly".into())
            }
            Event::ConcentratorDied(s) => {
                error!("Stats concentrator service task died: {s}");
                Err("Stats concentrator service task terminated unexpectedly".into())
            }
        };

        // Abort surviving tasks so they don't detach and hold sockets,
        // named pipes, or buffered channel state. abort() is &self and a
        // no-op on already-finished tasks, so calling it on whichever
        // handle resolved in the select! above is harmless.
        if let Some(h) = tcp_handle.as_ref() {
            h.abort();
        }
        #[cfg(all(windows, feature = "windows-pipes"))]
        if let Some(h) = pipe_handle.as_ref() {
            h.abort();
        }
        trace_flusher_handle.abort();
        stats_flusher_handle.abort();
        if let Some(h) = stats_concentrator_service_handle.as_ref() {
            h.abort();
        }

        result
    }

    async fn serve_accept_loop_tcp<S>(
        listener: tokio::net::TcpListener,
        service: S,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        S: hyper::service::Service<
                hyper::Request<hyper::body::Incoming>,
                Response = hyper::Response<http_common::Body>,
            > + Clone
            + Send
            + 'static,
        S::Future: Send,
        S::Error: std::error::Error + Send + Sync + 'static,
    {
        let server = hyper::server::conn::http1::Builder::new();
        let mut joinset = tokio::task::JoinSet::new();

        loop {
            let conn = tokio::select! {
                con_res = listener.accept() => match con_res {
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::ConnectionAborted
                                | io::ErrorKind::ConnectionReset
                                | io::ErrorKind::ConnectionRefused
                        ) =>
                    {
                        continue;
                    }
                    Err(e) => {
                        error!("TCP server error: {e}");
                        return Err(e.into());
                    }
                    Ok((conn, _)) => conn,
                },
                finished = async {
                    match joinset.join_next().await {
                        Some(finished) => finished,
                        None => std::future::pending().await,
                    }
                } => match finished {
                    Err(e) if e.is_panic() => {
                        std::panic::resume_unwind(e.into_panic());
                    },
                    Ok(()) | Err(_) => continue,
                },
                _ = shutdown_rx.changed() => {
                    // drains every in-flight handler to completion here
                    // before the supervisor triggers the final flush.
                    while let Some(result) = joinset.join_next().await {
                        if let Err(e) = result
                            && e.is_panic() {
                                std::panic::resume_unwind(e.into_panic());
                            }
                    }
                    return Ok(());
                },
            };
            let conn = hyper_util::rt::TokioIo::new(conn);
            let server = server.clone();
            let service = service.clone();
            let mut conn_shutdown = shutdown_rx.clone();
            joinset.spawn(async move {
                let conn = server.serve_connection(conn, service);
                tokio::pin!(conn);
                tokio::select! {
                    result = conn.as_mut() => {
                        if let Err(e) = result {
                            error!("TCP connection error: {e}");
                        }
                    }
                    _ = conn_shutdown.changed() => {
                        conn.as_mut().graceful_shutdown();
                        if let Err(e) = conn.await {
                            error!("TCP connection error during graceful shutdown: {e}");
                        }
                    }
                }
            });
        }
    }

    #[cfg(all(windows, feature = "windows-pipes"))]
    async fn serve_accept_loop_named_pipe<S>(
        pipe_name: String,
        service: S,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        S: hyper::service::Service<
                hyper::Request<hyper::body::Incoming>,
                Response = hyper::Response<http_common::Body>,
            > + Clone
            + Send
            + 'static,
        S::Future: Send,
        S::Error: std::error::Error + Send + Sync + 'static,
    {
        let server = hyper::server::conn::http1::Builder::new();
        let mut joinset = tokio::task::JoinSet::new();

        loop {
            let pipe = match ServerOptions::new().create(&pipe_name) {
                Ok(pipe) => {
                    debug!("Created pipe server instance '{}' in byte mode", pipe_name);
                    pipe
                }
                Err(e) => {
                    error!("Failed to create named pipe: {e}");
                    return Err(e.into());
                }
            };

            let conn = tokio::select! {
                connect_res = pipe.connect() => match connect_res {
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::ConnectionAborted
                                | io::ErrorKind::ConnectionReset
                                | io::ErrorKind::ConnectionRefused
                        ) =>
                    {
                        continue;
                    }
                    Err(e) => {
                        error!("Named pipe connection error: {e}");
                        return Err(e.into());
                    }
                    Ok(()) => {
                        debug!("Client connected to '{}'", pipe_name);
                        pipe
                    }
                },
                finished = async {
                    match joinset.join_next().await {
                        Some(finished) => finished,
                        None => std::future::pending().await,
                    }
                } => match finished {
                    Err(e) if e.is_panic() => {
                        std::panic::resume_unwind(e.into_panic());
                    },
                    Ok(()) | Err(_) => continue,
                },
                _ = shutdown_rx.changed() => {
                    // drains every in-flight handler to completion here
                    // before the supervisor triggers the final flush.
                    while let Some(result) = joinset.join_next().await {
                        if let Err(e) = result
                            && e.is_panic() {
                                std::panic::resume_unwind(e.into_panic());
                            }
                    }
                    return Ok(());
                },
            };

            let conn = hyper_util::rt::TokioIo::new(conn);
            let server = server.clone();
            let service = service.clone();
            let mut conn_shutdown = shutdown_rx.clone();
            joinset.spawn(async move {
                let conn = server.serve_connection(conn, service);
                tokio::pin!(conn);
                tokio::select! {
                    result = conn.as_mut() => {
                        if let Err(e) = result {
                            error!("Named pipe connection error: {e}");
                        }
                    }
                    _ = conn_shutdown.changed() => {
                        conn.as_mut().graceful_shutdown();
                        if let Err(e) = conn.await {
                            error!("Named pipe connection error during graceful shutdown: {e}");
                        }
                    }
                }
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn trace_endpoint_handler(
        config: Arc<config::Config>,
        req: http_common::HttpRequest,
        trace_processor: Arc<dyn trace_processor::TraceProcessor + Send + Sync>,
        trace_tx: Sender<SendData>,
        stats_processor: Arc<dyn stats_processor::StatsProcessor + Send + Sync>,
        stats_tx: Sender<pb::ClientStatsPayload>,
        mini_agent_metadata: Arc<trace_utils::MiniAgentMetadata>,
        proxy_tx: Sender<ProxyRequest>,
    ) -> http::Result<http_common::HttpResponse> {
        match (req.method(), req.uri().path()) {
            (&Method::PUT | &Method::POST, TRACE_ENDPOINT_PATH) => {
                match trace_processor
                    .process_traces(config, req, trace_tx, mini_agent_metadata)
                    .await
                {
                    Ok(res) => Ok(res),
                    Err(err) => log_and_create_http_response(
                        &format!("Error processing traces: {err}"),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    ),
                }
            }
            (&Method::PUT | &Method::POST, STATS_ENDPOINT_PATH) => {
                match stats_processor.process_stats(config, req, stats_tx).await {
                    Ok(res) => Ok(res),
                    Err(err) => log_and_create_http_response(
                        &format!("Error processing trace stats: {err}"),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    ),
                }
            }
            (&Method::POST, DSM_ENDPOINT_PATH) => match Self::proxy_handler(
                config,
                req,
                proxy_tx,
                ProxyRequestKind::DataStreams,
                "data streams request",
                |cfg| cfg.dsm_intake.url.to_string(),
            )
            .await
            {
                Ok(res) => Ok(res),
                Err(err) => log_and_create_http_response(
                    &format!("Error processing data streams request: {err}"),
                    StatusCode::INTERNAL_SERVER_ERROR,
                ),
            },
            (&Method::POST, PROFILING_ENDPOINT_PATH) => {
                match Self::proxy_handler(
                    config,
                    req,
                    proxy_tx,
                    ProxyRequestKind::Profiling,
                    "profiling request",
                    |cfg| cfg.profiling_intake.url.to_string(),
                )
                .await
                {
                    Ok(res) => Ok(res),
                    Err(err) => log_and_create_http_response(
                        &format!("Error processing profiling request: {err}"),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    ),
                }
            }
            (_, INFO_ENDPOINT_PATH) => match Self::info_handler(
                config.dd_apm_receiver_port,
                #[cfg(all(windows, feature = "windows-pipes"))]
                config.dd_apm_windows_pipe_name.as_deref(),
                #[cfg(not(all(windows, feature = "windows-pipes")))]
                None,
                config.dd_dogstatsd_port,
                &config.peer_tags,
                config.agent_stats_computation_enabled,
            ) {
                Ok(res) => Ok(res),
                Err(err) => log_and_create_http_response(
                    &format!("Info endpoint error: {err}"),
                    StatusCode::INTERNAL_SERVER_ERROR,
                ),
            },
            _ => {
                let mut not_found = Response::default();
                *not_found.status_mut() = StatusCode::NOT_FOUND;
                Ok(not_found)
            }
        }
    }

    /// Handles incoming proxy requests such as profiling or DSM payloads.
    async fn proxy_handler<F>(
        config: Arc<config::Config>,
        request: http_common::HttpRequest,
        proxy_tx: Sender<ProxyRequest>,
        kind: ProxyRequestKind,
        context: &str,
        target_url: F,
    ) -> http::Result<http_common::HttpResponse>
    where
        F: FnOnce(&config::Config) -> String,
    {
        debug!("Received {context}");

        // Extract headers and body
        let (parts, body) = request.into_parts();
        if let Some(response) = verify_request_content_length(
            &parts.headers,
            config.max_request_content_length,
            &format!("Error processing {context}"),
        ) {
            return response;
        }

        let body_bytes = match body.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(e) => {
                return log_and_create_http_response(
                    &format!("Error reading {context} body: {e}"),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        // Create proxy request
        let proxy_request = ProxyRequest {
            headers: parts.headers,
            body: body_bytes,
            target_url: target_url(config.as_ref()),
            kind,
        };

        debug!(
            "Sending {context} to channel, target: {}",
            proxy_request.target_url
        );

        // Send to channel
        match proxy_tx.send(proxy_request).await {
            Ok(_) => log_and_create_http_response(
                &format!("Successfully buffered {context} to be flushed"),
                StatusCode::OK,
            ),
            Err(err) => log_and_create_http_response(
                &format!("Error sending {context} to the proxy flusher: {err}"),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        }
    }

    fn info_handler(
        dd_apm_receiver_port: u16,
        dd_apm_windows_pipe_name: Option<&str>,
        dd_dogstatsd_port: u16,
        peer_tags: &[String],
        agent_stats_computation_enabled: bool,
    ) -> http::Result<http_common::HttpResponse> {
        // pipe_name already includes \\.\pipe\ prefix from config
        let receiver_socket = dd_apm_windows_pipe_name.unwrap_or("");

        let config_json = serde_json::json!({
            "receiver_port": dd_apm_receiver_port,
            "statsd_port": dd_dogstatsd_port,
            "receiver_socket": receiver_socket
        });

        // client_drop_p0s tells the tracer whether it should drop unsampled P0 traces.
        // When the agent computes stats, tracers must send all traces to the agent
        // so it can compute accurate stats. P0 traces must not be dropped by the tracer.
        let client_drop_p0s = !agent_stats_computation_enabled;

        let response_json = json!(
            {
                "endpoints": [
                    TRACE_ENDPOINT_PATH,
                    STATS_ENDPOINT_PATH,
                    INFO_ENDPOINT_PATH,
                    DSM_ENDPOINT_PATH,
                    PROFILING_ENDPOINT_PATH
                ],
                "client_drop_p0s": client_drop_p0s,
                "span_kinds_stats_computed": SPAN_KINDS_STATS_COMPUTED,
                "peer_tags": peer_tags,
                "config": config_json
            }
        );
        Response::builder()
            .status(200)
            .body(http_common::Body::from(response_json.to_string()))
    }
}
