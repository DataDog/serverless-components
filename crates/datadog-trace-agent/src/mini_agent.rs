// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use http_body_util::BodyExt;
use hyper::service::service_fn;
use hyper::{http, Method, Response, StatusCode};
use libdd_common::hyper_migration;
use serde_json::json;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tracing::{debug, error};

use crate::http_utils::{log_and_create_http_response, verify_request_content_length};
use crate::proxy_flusher::{ProxyFlusher, ProxyRequest};

#[cfg(windows)]
use tokio::{net::windows::named_pipe::ServerOptions, time::{sleep, Duration}};

use crate::{config, env_verifier, stats_flusher, stats_processor, trace_flusher, trace_processor};
use libdd_trace_protobuf::pb;
use libdd_trace_utils::trace_utils;
use libdd_trace_utils::trace_utils::SendData;

const TRACE_ENDPOINT_PATH: &str = "/v0.4/traces";
const STATS_ENDPOINT_PATH: &str = "/v0.6/stats";
const INFO_ENDPOINT_PATH: &str = "/info";
const PROFILING_ENDPOINT_PATH: &str = "/profiling/v1/input";
const TRACER_PAYLOAD_CHANNEL_BUFFER_SIZE: usize = 10;
const STATS_PAYLOAD_CHANNEL_BUFFER_SIZE: usize = 10;
const PROXY_PAYLOAD_CHANNEL_BUFFER_SIZE: usize = 10;

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
    pub async fn start_mini_agent(&self) -> Result<(), Box<dyn std::error::Error>> {
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

        // start our stats flusher.
        let stats_flusher = self.stats_flusher.clone();
        let stats_config = self.config.clone();
        let stats_flusher_handle = tokio::spawn(async move {
            stats_flusher
                .start_stats_flusher(stats_config, stats_rx)
                .await;
        });

        // channels to send processed profiling requests to our proxy flusher
        let (proxy_tx, proxy_rx): (Sender<ProxyRequest>, Receiver<ProxyRequest>) =
            mpsc::channel(PROXY_PAYLOAD_CHANNEL_BUFFER_SIZE);

        // start our proxy flusher for profiling requests
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
                req.map(hyper_migration::Body::incoming),
                trace_processor,
                trace_tx,
                stats_processor,
                stats_tx,
                mini_agent_metadata,
                proxy_tx,
            )
        });

        // Determine which transport to use based on configuration
        if let Some(ref pipe_name) = self.config.dd_apm_windows_pipe_name {
            debug!("Mini Agent started: listening on named pipe {}", pipe_name);
        } else {
            debug!("Mini Agent started: listening on port {}", self.config.dd_apm_receiver_port);
        }
        debug!(
            "Time taken to start the Mini Agent: {} ms",
            now.elapsed().as_millis()
        );

        if let Some(ref pipe_name) = self.config.dd_apm_windows_pipe_name {
            // Windows named pipe transport
            #[cfg(windows)]
            {
                Self::serve_named_pipe(
                    pipe_name,
                    service,
                    trace_flusher_handle,
                    stats_flusher_handle,
                )
                .await?;
            }

            #[cfg(not(windows))]
            {
                error!("Named pipes are only supported on Windows, cannot use pipe: {}", pipe_name);
                return Err("Named pipes are only supported on Windows".into());
            }
        } else {
            // TCP transport
            let addr = SocketAddr::from(([127, 0, 0, 1], self.config.dd_apm_receiver_port));
            let listener = tokio::net::TcpListener::bind(&addr).await?;

            Self::serve_tcp(listener, service, trace_flusher_handle, stats_flusher_handle)
                .await?;
        }

        Ok(())
    }

    async fn serve_tcp<S>(
        listener: tokio::net::TcpListener,
        service: S,
        mut trace_flusher_handle: tokio::task::JoinHandle<()>,
        mut stats_flusher_handle: tokio::task::JoinHandle<()>,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        S: hyper::service::Service<
                hyper::Request<hyper::body::Incoming>,
                Response = hyper::Response<hyper_migration::Body>,
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
                        error!("Server error: {e}");
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
                        // Don't kill server on panic - log and continue
                        error!("Connection handler panicked: {:?}", e);
                        continue;
                    },
                    Ok(()) | Err(_) => continue,
                },
                result = &mut trace_flusher_handle => {
                    error!("Trace flusher task died: {:?}", result);
                    return Err("Trace flusher task terminated unexpectedly".into());
                },
                result = &mut stats_flusher_handle => {
                    error!("Stats flusher task died: {:?}", result);
                    return Err("Stats flusher task terminated unexpectedly".into());
                },
            };
            let conn = hyper_util::rt::TokioIo::new(conn);
            let server = server.clone();
            let service = service.clone();
            joinset.spawn(async move {
                if let Err(e) = server.serve_connection(conn, service).await {
                    error!("Connection error: {e}");
                }
            });
        }
    }

    #[cfg(windows)]
    async fn serve_named_pipe<S>(
        pipe_name: &str,
        service: S,
        mut trace_flusher_handle: tokio::task::JoinHandle<()>,
        mut stats_flusher_handle: tokio::task::JoinHandle<()>,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        S: hyper::service::Service<
                hyper::Request<hyper::body::Incoming>,
                Response = hyper::Response<hyper_migration::Body>,
            > + Clone
            + Send
            + 'static,
        S::Future: Send,
        S::Error: std::error::Error + Send + Sync + 'static,
    {
        let server = hyper::server::conn::http1::Builder::new();
        let mut joinset = tokio::task::JoinSet::new();
        let mut consecutive_errors = 0u32;
        const MAX_CONSECUTIVE_ERRORS: u32 = 10;

        // Pre-create first pipe instance to minimize connection gap
        let mut current_pipe = loop {
            match ServerOptions::new()
                .first_pipe_instance(false)
                .create(pipe_name)
            {
                Ok(pipe) => break pipe,
                Err(e) => {
                    consecutive_errors += 1;
                    error!(
                        "Failed to create initial named pipe (attempt {}/{}): {}",
                        consecutive_errors, MAX_CONSECUTIVE_ERRORS, e
                    );

                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        error!("Too many consecutive pipe creation failures during startup, shutting down");
                        return Err(e.into());
                    }

                    // Exponential backoff: 10ms, 20ms, 40ms, 80ms, ...
                    let backoff_ms = 10u64 * (1 << consecutive_errors.min(6));
                    sleep(Duration::from_millis(backoff_ms)).await;
                }
            }
        };

        // Reset error counter after successful creation
        consecutive_errors = 0;

        loop {
            let conn_result = tokio::select! {
                connect_res = current_pipe.connect() => {
                    match connect_res {
                        Ok(()) => Ok(current_pipe),
                        Err(e) => Err(e),
                    }
                },
                finished = async {
                    match joinset.join_next().await {
                        Some(finished) => finished,
                        None => std::future::pending().await,
                    }
                } => match finished {
                    Err(e) if e.is_panic() => {
                        // Don't kill server on panic - log and continue
                        error!("Connection handler panicked: {:?}", e);
                        continue;
                    },
                    Ok(()) | Err(_) => continue,
                },
                result = &mut trace_flusher_handle => {
                    error!("Trace flusher task died: {:?}", result);
                    return Err("Trace flusher task terminated unexpectedly".into());
                },
                result = &mut stats_flusher_handle => {
                    error!("Stats flusher task died: {:?}", result);
                    return Err("Stats flusher task terminated unexpectedly".into());
                },
            };

            let connected_pipe = match conn_result {
                Ok(pipe) => pipe,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::ConnectionAborted
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    // Transient connection errors - recreate pipe and continue
                    current_pipe = match ServerOptions::new()
                        .first_pipe_instance(false)
                        .create(pipe_name)
                    {
                        Ok(pipe) => {
                            consecutive_errors = 0;
                            pipe
                        }
                        Err(e) => {
                            consecutive_errors += 1;
                            error!(
                                "Failed to recreate pipe after transient error (attempt {}/{}): {}",
                                consecutive_errors, MAX_CONSECUTIVE_ERRORS, e
                            );

                            if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                                return Err(e.into());
                            }

                            let backoff_ms = 10u64 * (1 << consecutive_errors.min(6));
                            sleep(Duration::from_millis(backoff_ms)).await;
                            continue;
                        }
                    };
                    continue;
                }
                Err(e) => {
                    // Log non-transient errors but don't immediately kill server
                    error!("Named pipe connection error: {e}");
                    consecutive_errors += 1;

                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        error!("Too many consecutive connection errors, shutting down");
                        return Err(e.into());
                    }

                    // Recreate pipe with backoff
                    let backoff_ms = 10u64 * (1 << consecutive_errors.min(6));
                    sleep(Duration::from_millis(backoff_ms)).await;

                    current_pipe = match ServerOptions::new()
                        .first_pipe_instance(false)
                        .create(pipe_name)
                    {
                        Ok(pipe) => pipe,
                        Err(e) => {
                            error!("Failed to recreate pipe after error: {}", e);
                            continue;
                        }
                    };
                    continue;
                }
            };

            // Connection successful! Immediately create next pipe instance
            // to minimize gap where no pipe is available
            let next_pipe = match ServerOptions::new()
                .first_pipe_instance(false)
                .create(pipe_name)
            {
                Ok(pipe) => {
                    consecutive_errors = 0; // Reset on success
                    pipe
                }
                Err(e) => {
                    consecutive_errors += 1;
                    error!(
                        "Failed to create next pipe instance (attempt {}/{}): {}",
                        consecutive_errors, MAX_CONSECUTIVE_ERRORS, e
                    );

                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        return Err(e.into());
                    }

                    // Brief pause before retry
                    sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };

            // Spawn handler with connected_pipe (ownership moved to task)
            let conn = hyper_util::rt::TokioIo::new(connected_pipe);
            let server = server.clone();
            let service = service.clone();
            joinset.spawn(async move {
                if let Err(e) = server.serve_connection(conn, service).await {
                    error!("Connection error: {e}");
                }
            });

            // Use next_pipe for the next iteration
            current_pipe = next_pipe;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn trace_endpoint_handler(
        config: Arc<config::Config>,
        req: hyper_migration::HttpRequest,
        trace_processor: Arc<dyn trace_processor::TraceProcessor + Send + Sync>,
        trace_tx: Sender<SendData>,
        stats_processor: Arc<dyn stats_processor::StatsProcessor + Send + Sync>,
        stats_tx: Sender<pb::ClientStatsPayload>,
        mini_agent_metadata: Arc<trace_utils::MiniAgentMetadata>,
        proxy_tx: Sender<ProxyRequest>,
    ) -> http::Result<hyper_migration::HttpResponse> {
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
            (&Method::POST, PROFILING_ENDPOINT_PATH) => {
                match Self::profiling_proxy_handler(config, req, proxy_tx).await {
                    Ok(res) => Ok(res),
                    Err(err) => log_and_create_http_response(
                        &format!("Error processing profiling request: {err}"),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    ),
                }
            }
            (_, INFO_ENDPOINT_PATH) => match Self::info_handler(
                config.dd_apm_receiver_port,
                config.dd_apm_windows_pipe_name.as_deref(),
                config.dd_dogstatsd_port,
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

    /// Handles incoming proxy requests for profiling - can be abstracted into a generic proxy handler for other proxy requests in the future
    async fn profiling_proxy_handler(
        config: Arc<config::Config>,
        request: hyper_migration::HttpRequest,
        proxy_tx: Sender<ProxyRequest>,
    ) -> http::Result<hyper_migration::HttpResponse> {
        debug!("Received profiling request");

        // Extract headers and body
        let (parts, body) = request.into_parts();
        if let Some(response) = verify_request_content_length(
            &parts.headers,
            config.max_request_content_length,
            "Error processing profiling request",
        ) {
            return response;
        }

        let body_bytes = match body.collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(e) => {
                return log_and_create_http_response(
                    &format!("Error reading profiling request body: {e}"),
                    StatusCode::BAD_REQUEST,
                );
            }
        };

        // Create proxy request
        let proxy_request = ProxyRequest {
            headers: parts.headers,
            body: body_bytes,
            target_url: config.profiling_intake.url.to_string(),
        };

        debug!(
            "Sending profiling request to channel, target: {}",
            proxy_request.target_url
        );

        // Send to channel
        match proxy_tx.send(proxy_request).await {
            Ok(_) => log_and_create_http_response(
                "Successfully buffered profiling request to be flushed",
                StatusCode::OK,
            ),
            Err(err) => log_and_create_http_response(
                &format!("Error sending profiling request to the proxy flusher: {err}"),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        }
    }

    fn info_handler(
        dd_apm_receiver_port: u16,
        dd_apm_windows_pipe_name: Option<&str>,
        dd_dogstatsd_port: u16,
    ) -> http::Result<hyper_migration::HttpResponse> {
        let mut config_json = serde_json::json!({
            "receiver_port": dd_apm_receiver_port,
            "statsd_port": dd_dogstatsd_port,
            "receiver_socket": serde_json::json!(dd_apm_windows_pipe_name.unwrap_or(""))
        });

        let response_json = json!(
            {
                "endpoints": [
                    TRACE_ENDPOINT_PATH,
                    STATS_ENDPOINT_PATH,
                    INFO_ENDPOINT_PATH,
                    PROFILING_ENDPOINT_PATH
                ],
                "client_drop_p0s": true,
                "config": config_json
            }
        );
        Response::builder()
            .status(200)
            .body(hyper_migration::Body::from(response_json.to_string()))
    }
}
