// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Trace agent HTTP server and request handling.
//!
//! This module implements the HTTP server that receives traces from instrumented applications.
//! It handles the Datadog APM trace ingestion protocol, including:
//! - Trace submission endpoints (v0.4, v0.5)
//! - Request validation and parsing
//! - AppSec integration for security monitoring
//! - Stats generation and aggregation
//! - Coordination with trace processors and flushers
//!
//! # Architecture
//!
//! The trace agent uses Axum for HTTP request handling and communicates with other
//! components via message passing (Tokio channels). Each trace request is parsed,
//! optionally processed by AppSec, enriched with tags, and sent to the trace processor.
//!
//! # Endpoints
//!
//! - `POST/PUT /v0.4/traces` - APM trace submission (msgpack)
//! - `POST/PUT /v0.5/traces` - APM trace submission with client-computed stats (msgpack)
//! - `POST /v0.6/stats` - APM statistics submission (protobuf)
//! - `POST /v0.7/config` - Remote configuration endpoint (protobuf)
//! - `GET /info` - Agent information endpoint

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, post},
    Router,
};
use lazy_static::lazy_static;
use regex::Regex;
use serde_json::json;
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Mutex,
};
use tokio_util::sync::CancellationToken;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::{debug, error, warn};
use uuid::Uuid;

#[cfg(feature = "appsec")]
use crate::appsec::processor::Processor as AppSecProcessor;
use crate::traces::trace_processor::SendingTraceProcessor;
use crate::{
    config,
    http::{extract_request_body, get_client, handler_not_found},
    tags::provider,
    traces::{
        proxy_aggregator::{self, ProxyRequest},
        stats_aggregator,
        stats_generator::StatsGenerator,
        stats_processor,
        trace_aggregator::{self, SendDataBuilderInfo},
        trace_processor,
        uds::{create_listener, Listener, ListenerInfo},
    },
};
use datadog_trace_protobuf::pb;
use datadog_trace_utils::trace_utils::{self};
use ddcommon::hyper_migration;

use crate::remote_config;
use crate::traces::stats_concentrator_service::StatsConcentratorHandle;
use prost::Message;
use remote_config_proto::remoteconfig::ClientGetConfigsRequest;

#[allow(dead_code)] // May be used by tests or external modules
const TRACE_AGENT_PORT: usize = 8126;

// Agent endpoints
const V4_TRACE_ENDPOINT_PATH: &str = "/v0.4/traces";
const V5_TRACE_ENDPOINT_PATH: &str = "/v0.5/traces";
const STATS_ENDPOINT_PATH: &str = "/v0.6/stats";
const REMOTE_CONFIG_ENDPOINT_PATH: &str = "/v0.7/config";
const DSM_AGENT_PATH: &str = "/v0.1/pipeline_stats";
const PROFILING_ENDPOINT_PATH: &str = "/profiling/v1/input";
const LLM_OBS_EVAL_METRIC_ENDPOINT_PATH: &str = "/evp_proxy/v2/api/intake/llm-obs/v1/eval-metric";
const LLM_OBS_EVAL_METRIC_ENDPOINT_PATH_V2: &str =
    "/evp_proxy/v2/api/intake/llm-obs/v2/eval-metric";
const LLM_OBS_SPANS_ENDPOINT_PATH: &str = "/evp_proxy/v2/api/v2/llmobs";
const INFO_ENDPOINT_PATH: &str = "/info";
const DEBUGGER_ENDPOINT_PATH: &str = "/debugger/v1/input";
const DEBUGGER_DIAGNOSTICS_ENDPOINT_PATH: &str = "/debugger/v1/diagnostics";
const DEBUGGER_V2_ENDPOINT_PATH: &str = "/debugger/v2/input";
const SYMDB_ENDPOINT_PATH: &str = "/symdb/v1/input";
const LOGS_ENDPOINT_PATH: &str = "/v1/input";
const INSTRUMENTATION_ENDPOINT_PATH: &str = "/telemetry/proxy/api/v2/apmtelemetry";
const DOGSTATSD_V2_PROXY_PATH: &str = "/dogstatsd/v2/proxy";
const TRACER_FLARE_ENDPOINT_PATH: &str = "/tracer_flare/v1";
const EVP_PROXY_V1_BASE_PATH: &str = "/evp_proxy/v1";
const EVP_PROXY_V1_WILDCARD_PATH: &str = "/evp_proxy/v1/{*proxy_path}";
const EVP_PROXY_V2_BASE_PATH: &str = "/evp_proxy/v2";
const EVP_PROXY_V2_WILDCARD_PATH: &str = "/evp_proxy/v2/{*proxy_path}";
const EVP_PROXY_V3_BASE_PATH: &str = "/evp_proxy/v3";
const EVP_PROXY_V3_WILDCARD_PATH: &str = "/evp_proxy/v3/{*proxy_path}";
const EVP_PROXY_V4_BASE_PATH: &str = "/evp_proxy/v4";
const EVP_PROXY_V4_WILDCARD_PATH: &str = "/evp_proxy/v4/{*proxy_path}";

// Intake endpoints
const DSM_INTAKE_PATH: &str = "/api/v0.1/pipeline_stats";
const LLM_OBS_SPANS_INTAKE_PATH: &str = "/api/v2/llmobs";
const LLM_OBS_EVAL_METRIC_INTAKE_PATH: &str = "/api/intake/llm-obs/v1/eval-metric";
const LLM_OBS_EVAL_METRIC_INTAKE_PATH_V2: &str = "/api/intake/llm-obs/v2/eval-metric";
const PROFILING_INTAKE_PATH: &str = "/api/v2/profile";
const DEBUGGER_LOGS_INTAKE_PATH: &str = "/api/v2/logs";
const DEBUGGER_INTAKE_PATH: &str = "/api/v2/debugger";
const INSTRUMENTATION_INTAKE_PATH: &str = "/api/v2/apmtelemetry";

const TRACER_PAYLOAD_CHANNEL_BUFFER_SIZE: usize = 10;
const STATS_PAYLOAD_CHANNEL_BUFFER_SIZE: usize = 10;
pub(crate) const TRACE_REQUEST_BODY_LIMIT: usize = 50 * 1024 * 1024;
pub(crate) const DEFAULT_REQUEST_BODY_LIMIT: usize = 2 * 1024 * 1024;
pub(crate) const MAX_CONTENT_LENGTH: usize = 50 * 1024 * 1024;
const DD_TAGS_QUERY_STRING_MAX_LEN: usize = 4001;
const EVP_PROXY_ALLOWED_HEADERS: [&str; 5] = [
    "content-type",
    "accept-encoding",
    "content-encoding",
    "user-agent",
    "dd-ci-provider-name",
];
const EVP_PROXY_SUBDOMAIN_HEADER: &str = "x-datadog-evp-subdomain";
const EVP_PROXY_NEEDS_APP_KEY_HEADER: &str = "x-datadog-needsappkey";
const EVP_PROXY_ORIGIN: &str = "agent-evp-proxy";
const EVP_PROXY_PATH_ALLOWED_CHARS: &str = "/_-+";
const EVP_PROXY_QUERY_ALLOWED_CHARS: &str = "/_-+@?&=.:\"[]";
const EVP_PROXY_SUBDOMAIN_ALLOWED_CHARS: &str = "_-.";
const EVP_PROXY_MAX_PAYLOAD_BYTES: usize = 5 * 1024 * 1024;
const EVP_PROXY_DEFAULT_TIMEOUT_SECS: u64 = 30;
const SERVERLESS_FLARE_ENDPOINT_PATH: &str = "/api/ui/support/serverless/flare";

#[derive(Clone)]
pub struct TraceState {
    pub config: Arc<config::Config>,
    pub trace_sender: Arc<trace_processor::SendingTraceProcessor>,
    pub tags_provider: Arc<provider::Provider>,
}

#[derive(Clone)]
pub struct StatsState {
    pub stats_processor: Arc<dyn stats_processor::StatsProcessor + Send + Sync>,
    pub stats_tx: Sender<pb::ClientStatsPayload>,
}

#[derive(Clone)]
pub struct ProxyState {
    pub config: Arc<config::Config>,
    pub proxy_aggregator: Arc<Mutex<proxy_aggregator::Aggregator>>,
}

#[derive(Clone)]
pub struct RemoteConfigState {
    pub remote_config_service: Arc<remote_config::RemoteConfigService>,
}

pub struct TraceAgent {
    pub config: Arc<config::Config>,
    pub trace_processor: Arc<dyn trace_processor::TraceProcessor + Send + Sync>,
    pub stats_aggregator: Arc<Mutex<stats_aggregator::StatsAggregator>>,
    pub stats_processor: Arc<dyn stats_processor::StatsProcessor + Send + Sync>,
    pub proxy_aggregator: Arc<Mutex<proxy_aggregator::Aggregator>>,
    pub tags_provider: Arc<provider::Provider>,
    #[cfg(feature = "appsec")]
    appsec_processor: Option<Arc<Mutex<AppSecProcessor>>>,
    remote_config_service: Option<Arc<remote_config::RemoteConfigService>>,
    shutdown_token: CancellationToken,
    tx: Sender<SendDataBuilderInfo>,
    stats_concentrator: StatsConcentratorHandle,
    /// Connection information (TCP port or UDS path) after HTTP server starts successfully.
    /// None if running in FFI-only mode.
    listener_info: Arc<Mutex<Option<ListenerInfo>>>,
}

#[derive(Clone, Copy)]
pub enum ApiVersion {
    V04,
    V05,
}

impl TraceAgent {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Arc<config::Config>,
        trace_aggregator: Arc<Mutex<trace_aggregator::TraceAggregator>>,
        trace_processor: Arc<dyn trace_processor::TraceProcessor + Send + Sync>,
        stats_aggregator: Arc<Mutex<stats_aggregator::StatsAggregator>>,
        stats_processor: Arc<dyn stats_processor::StatsProcessor + Send + Sync>,
        proxy_aggregator: Arc<Mutex<proxy_aggregator::Aggregator>>,
        #[cfg(feature = "appsec")] appsec_processor: Option<Arc<Mutex<AppSecProcessor>>>,
        tags_provider: Arc<provider::Provider>,
        stats_concentrator: StatsConcentratorHandle,
        remote_config_service: Option<Arc<remote_config::RemoteConfigService>>,
        shutdown_token: CancellationToken,
    ) -> TraceAgent {
        // Set up a channel to send processed traces to our trace aggregator. tx is passed through each
        // endpoint_handler to the trace processor, which uses it to send de-serialized
        // processed trace payloads to our trace aggregator.
        let (trace_tx, mut trace_rx): (Sender<SendDataBuilderInfo>, Receiver<SendDataBuilderInfo>) =
            mpsc::channel(TRACER_PAYLOAD_CHANNEL_BUFFER_SIZE);

        // Start the trace aggregator, which receives and buffers trace payloads to be consumed by the trace flusher.
        tokio::spawn(async move {
            while let Some(tracer_payload_info) = trace_rx.recv().await {
                let mut aggregator = trace_aggregator.lock().await;
                aggregator.add(tracer_payload_info);
            }
        });

        TraceAgent {
            config: config.clone(),
            trace_processor,
            stats_aggregator,
            stats_processor,
            proxy_aggregator,
            #[cfg(feature = "appsec")]
            appsec_processor,
            remote_config_service,
            tags_provider,
            tx: trace_tx,
            shutdown_token,
            stats_concentrator,
            listener_info: Arc::new(Mutex::new(None)),
        }
    }

    /// Get the actual bound port of the HTTP server.
    ///
    /// Returns `None` if:
    /// - Running in FFI-only mode (no HTTP server)
    /// - Running in HttpUds mode (UDS doesn't have a port)
    /// - HTTP server hasn't started yet
    ///
    /// # Returns
    /// The port number the trace agent HTTP server is listening on.
    pub async fn get_bound_port(&self) -> Option<u16> {
        self.listener_info
            .lock()
            .await
            .as_ref()
            .and_then(|info| info.tcp_port)
    }

    /// Get the listener information (TCP port or UDS path).
    ///
    /// Returns `None` if:
    /// - Running in FFI-only mode (no HTTP server)
    /// - HTTP server hasn't started yet
    ///
    /// # Returns
    /// The listener information containing either TCP port or UDS path.
    pub async fn get_listener_info(&self) -> Option<ListenerInfo> {
        self.listener_info.lock().await.clone()
    }

    /// Returns a clone of the Arc reference to the listener info.
    /// This allows the coordinator to retain access to the connection info even after
    /// the TraceAgent is moved into a task.
    pub fn get_listener_info_ref(&self) -> Arc<Mutex<Option<ListenerInfo>>> {
        Arc::clone(&self.listener_info)
    }

    /// Returns a clone of the Arc reference to the bound port (deprecated).
    /// This allows the coordinator to retain access to the port even after
    /// the TraceAgent is moved into a task.
    ///
    /// **Deprecated**: Use `get_listener_info_ref()` instead for better UDS support.
    #[allow(dead_code)] // Kept for backward compatibility
    pub fn get_bound_port_ref(&self) -> Arc<Mutex<Option<u16>>> {
        // For backward compatibility, create a wrapper that extracts just the port
        let _listener_info = Arc::clone(&self.listener_info);
        Arc::new(Mutex::new(None)) // Placeholder - will need special handling
    }

    #[allow(clippy::cast_possible_truncation)]
    pub async fn start(&self) -> Result<(), Box<dyn std::error::Error>> {
        let now = Instant::now();

        // Check operational mode
        if !self.config.operational_mode.requires_http_server() {
            debug!("TRACE AGENT | Running in FFI-only mode, HTTP server disabled");
            return Ok(());
        }

        // Set up a channel to send processed stats to our stats aggregator.
        let (stats_tx, mut stats_rx): (
            Sender<pb::ClientStatsPayload>,
            Receiver<pb::ClientStatsPayload>,
        ) = mpsc::channel(STATS_PAYLOAD_CHANNEL_BUFFER_SIZE);

        // Start the stats aggregator, which receives and buffers stats payloads to be consumed by the stats flusher.
        let stats_aggregator = self.stats_aggregator.clone();
        tokio::spawn(async move {
            while let Some(stats_payload) = stats_rx.recv().await {
                let mut aggregator = stats_aggregator.lock().await;
                aggregator.add(stats_payload);
            }
        });

        let router = self.make_router(stats_tx);

        // Create listener (TCP or UDS based on operational mode)
        let (listener, info) = create_listener(&self.config).await?;

        // Store the listener info
        {
            let mut listener_info = self.listener_info.lock().await;
            *listener_info = Some(info.clone());
        }

        // Log connection information
        if let Some(port) = info.tcp_port {
            debug!(
                "TRACE AGENT | Mode: {} | Listening on TCP port {}",
                self.config.operational_mode, port
            );
        } else if let Some(ref path) = info.uds_path {
            debug!(
                "TRACE AGENT | Mode: {} | Listening on Unix socket: {}",
                self.config.operational_mode, path
            );
        }

        debug!(
            "TRACE AGENT | Time taken to start: {} ms",
            now.elapsed().as_millis()
        );

        let shutdown_token_clone = self.shutdown_token.clone();

        // Serve based on listener type
        match listener {
            Listener::Tcp(tcp_listener) => {
                axum::serve(tcp_listener, router)
                    .with_graceful_shutdown(Self::graceful_shutdown(shutdown_token_clone))
                    .await?;
            }
            #[cfg(unix)]
            Listener::Unix(unix_listener) => {
                axum::serve(unix_listener, router)
                    .with_graceful_shutdown(Self::graceful_shutdown(shutdown_token_clone))
                    .await?;
            }
        }

        Ok(())
    }

    fn make_router(&self, stats_tx: Sender<pb::ClientStatsPayload>) -> Router {
        let stats_generator = Arc::new(StatsGenerator::new(self.stats_concentrator.clone()));
        let trace_state = TraceState {
            config: Arc::clone(&self.config),
            trace_sender: Arc::new(SendingTraceProcessor {
                #[cfg(feature = "appsec")]
                appsec: self.appsec_processor.clone(),
                processor: Arc::clone(&self.trace_processor),
                trace_tx: self.tx.clone(),
                stats_generator,
            }),
            tags_provider: Arc::clone(&self.tags_provider),
        };

        let stats_state = StatsState {
            stats_processor: Arc::clone(&self.stats_processor),
            stats_tx,
        };

        let proxy_state = ProxyState {
            config: Arc::clone(&self.config),
            proxy_aggregator: Arc::clone(&self.proxy_aggregator),
        };

        // Optional remote config router - only if service is available
        let remote_config_router = if let Some(ref rc_service) = self.remote_config_service {
            debug!(
                "TRACE_AGENT | Remote config service available, registering {} endpoint",
                REMOTE_CONFIG_ENDPOINT_PATH
            );
            let rc_state = RemoteConfigState {
                remote_config_service: Arc::clone(rc_service),
            };
            Some(
                Router::new()
                    .route(REMOTE_CONFIG_ENDPOINT_PATH, post(Self::remote_config))
                    .layer(RequestBodyLimitLayer::new(DEFAULT_REQUEST_BODY_LIMIT))
                    .with_state(rc_state),
            )
        } else {
            debug!("TRACE_AGENT | Remote config service NOT available, {} endpoint will NOT be registered (will return 404)", REMOTE_CONFIG_ENDPOINT_PATH);
            None
        };

        let trace_router = Router::new()
            .route(
                V4_TRACE_ENDPOINT_PATH,
                post(Self::v04_traces).put(Self::v04_traces),
            )
            .route(
                V5_TRACE_ENDPOINT_PATH,
                post(Self::v05_traces).put(Self::v05_traces),
            )
            .layer(RequestBodyLimitLayer::new(TRACE_REQUEST_BODY_LIMIT))
            .with_state(trace_state.clone());

        let stats_router = Router::new()
            .route(STATS_ENDPOINT_PATH, post(Self::stats).put(Self::stats))
            .layer(RequestBodyLimitLayer::new(DEFAULT_REQUEST_BODY_LIMIT))
            .with_state(stats_state);

        let evp_router = Router::new()
            .route(EVP_PROXY_V1_BASE_PATH, any(Self::evp_proxy_v1))
            .route(EVP_PROXY_V1_WILDCARD_PATH, any(Self::evp_proxy_v1))
            .route(EVP_PROXY_V2_BASE_PATH, any(Self::evp_proxy_v2))
            .route(EVP_PROXY_V2_WILDCARD_PATH, any(Self::evp_proxy_v2))
            .route(EVP_PROXY_V3_BASE_PATH, any(Self::evp_proxy_v3))
            .route(EVP_PROXY_V3_WILDCARD_PATH, any(Self::evp_proxy_v3))
            .route(EVP_PROXY_V4_BASE_PATH, any(Self::evp_proxy_v4))
            .route(EVP_PROXY_V4_WILDCARD_PATH, any(Self::evp_proxy_v4))
            .layer(RequestBodyLimitLayer::new(EVP_PROXY_MAX_PAYLOAD_BYTES))
            .with_state(proxy_state.clone());

        let proxy_router = Router::new()
            .route(DSM_AGENT_PATH, post(Self::dsm_proxy))
            .route(PROFILING_ENDPOINT_PATH, post(Self::profiling_proxy))
            .route(
                LLM_OBS_EVAL_METRIC_ENDPOINT_PATH,
                post(Self::llm_obs_eval_metric_proxy),
            )
            .route(
                LLM_OBS_EVAL_METRIC_ENDPOINT_PATH_V2,
                post(Self::llm_obs_eval_metric_proxy_v2),
            )
            .route(LLM_OBS_SPANS_ENDPOINT_PATH, post(Self::llm_obs_spans_proxy))
            .route(DEBUGGER_ENDPOINT_PATH, post(Self::debugger_logs_proxy))
            .route(
                DEBUGGER_DIAGNOSTICS_ENDPOINT_PATH,
                post(Self::debugger_diagnostics_proxy),
            )
            .route(DEBUGGER_V2_ENDPOINT_PATH, post(Self::debugger_v2_proxy))
            .route(SYMDB_ENDPOINT_PATH, post(Self::symdb_proxy))
            .route(LOGS_ENDPOINT_PATH, post(Self::logs_proxy))
            .route(
                INSTRUMENTATION_ENDPOINT_PATH,
                post(Self::instrumentation_proxy),
            )
            .route(DOGSTATSD_V2_PROXY_PATH, post(Self::dogstatsd_proxy))
            .route(TRACER_FLARE_ENDPOINT_PATH, post(Self::tracer_flare_proxy))
            .merge(evp_router)
            .layer(RequestBodyLimitLayer::new(DEFAULT_REQUEST_BODY_LIMIT))
            .with_state(proxy_state);

        let info_router = Router::new()
            .route(INFO_ENDPOINT_PATH, any(Self::info))
            .with_state(trace_state.clone());

        let mut main_router = Router::new()
            .merge(trace_router)
            .merge(stats_router)
            .merge(proxy_router)
            .merge(info_router);

        // Optionally merge remote config router if available
        if let Some(rc_router) = remote_config_router {
            debug!("TRACE_AGENT | Merging remote config router into main router");
            main_router = main_router.merge(rc_router);
        } else {
            debug!("TRACE_AGENT | Remote config router NOT merged (service unavailable)");
        }

        main_router
            .fallback(handler_not_found)
            // Disable the default body limit so we can use our own limit
            .layer(DefaultBodyLimit::disable())
    }

    async fn graceful_shutdown(shutdown_token: CancellationToken) {
        shutdown_token.cancelled().await;
        debug!("TRACE_AGENT | Shutdown signal received, shutting down");
    }

    async fn v04_traces(State(state): State<TraceState>, request: Request) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        debug!(
            "TRACE_AGENT | v04_traces handler called: {} {}",
            method, path
        );

        // Extract body for logging
        let (parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                let response = error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {e}"),
                );
                return response;
            }
        };

        // Log request with body preview
        let body_preview = if parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false)
        {
            // JSON request - show as string
            if body.len() <= 1000 {
                String::from_utf8_lossy(&body).to_string()
            } else {
                format!(
                    "{}... ({} bytes total)",
                    String::from_utf8_lossy(&body[..1000]),
                    body.len()
                )
            }
        } else {
            // Binary/other request - show preview (limit to first 500 bytes for msgpack)
            let preview_bytes = if body.len() <= 500 {
                &body[..]
            } else {
                &body[..500]
            };

            // Try to convert to UTF-8
            match std::str::from_utf8(preview_bytes) {
                Ok(s) => {
                    if body.len() <= 500 {
                        s.to_string()
                    } else {
                        format!("{}... ({} bytes total)", s, body.len())
                    }
                }
                Err(_) => {
                    // Not valid UTF-8, show as hex for better readability
                    let hex_preview: String = preview_bytes
                        .iter()
                        .take(100)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");

                    if body.len() <= 500 {
                        format!("<hex: {}>", hex_preview)
                    } else {
                        format!("<hex: {}>... ({} bytes total)", hex_preview, body.len())
                    }
                }
            }
        };

        debug!(
            "HTTP Request: {} {} | Content-Type: {} | Content-Length: {} | Body: {}",
            method,
            path,
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none"),
            body.len(),
            body_preview
        );

        // Reconstruct request
        let request = Request::from_parts(parts, axum::body::Body::from(body));

        let response = Self::handle_traces(
            state.config,
            request,
            state.trace_sender,
            state.tags_provider,
            ApiVersion::V04,
        )
        .await;

        // Log response with body
        let status = response.status();
        let (parts, body) = response.into_parts();
        let body_bytes = match axum::body::to_bytes(body, usize::MAX).await {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to read response body for logging: {}", e);
                let response = Response::from_parts(parts, axum::body::Body::empty());
                return response;
            }
        };

        let response_body_preview = if parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false)
        {
            // JSON response - show as string
            if body_bytes.len() <= 1000 {
                String::from_utf8_lossy(&body_bytes).to_string()
            } else {
                format!(
                    "{}... ({} bytes total)",
                    String::from_utf8_lossy(&body_bytes[..1000]),
                    body_bytes.len()
                )
            }
        } else {
            // Binary/other response - show preview
            if body_bytes.len() <= 200 {
                format!("{:?}", body_bytes)
            } else {
                format!(
                    "{:?}... ({} bytes total)",
                    &body_bytes[..200],
                    body_bytes.len()
                )
            }
        };

        debug!(
            "HTTP Response: {} {} | Status: {} | Body: {}",
            method,
            path,
            status.as_u16(),
            response_body_preview
        );

        Response::from_parts(parts, axum::body::Body::from(body_bytes))
    }

    async fn v05_traces(State(state): State<TraceState>, request: Request) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        debug!(
            "TRACE_AGENT | v05_traces handler called: {} {}",
            method, path
        );

        // Extract body for logging
        let (parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                let response = error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {e}"),
                );
                return response;
            }
        };

        // Log request with body preview
        let body_preview = if parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false)
        {
            // JSON request - show as string
            if body.len() <= 1000 {
                String::from_utf8_lossy(&body).to_string()
            } else {
                format!(
                    "{}... ({} bytes total)",
                    String::from_utf8_lossy(&body[..1000]),
                    body.len()
                )
            }
        } else {
            // Binary/other request - show preview (limit to first 500 bytes for msgpack)
            let preview_bytes = if body.len() <= 500 {
                &body[..]
            } else {
                &body[..500]
            };

            // Try to convert to UTF-8
            match std::str::from_utf8(preview_bytes) {
                Ok(s) => {
                    if body.len() <= 500 {
                        s.to_string()
                    } else {
                        format!("{}... ({} bytes total)", s, body.len())
                    }
                }
                Err(_) => {
                    // Not valid UTF-8, show as hex for better readability
                    let hex_preview: String = preview_bytes
                        .iter()
                        .take(100)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");

                    if body.len() <= 500 {
                        format!("<hex: {}>", hex_preview)
                    } else {
                        format!("<hex: {}>... ({} bytes total)", hex_preview, body.len())
                    }
                }
            }
        };

        debug!(
            "HTTP Request: {} {} | Content-Type: {} | Content-Length: {} | Body: {}",
            method,
            path,
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none"),
            body.len(),
            body_preview
        );

        // Reconstruct request
        let request = Request::from_parts(parts, axum::body::Body::from(body));

        let response = Self::handle_traces(
            state.config,
            request,
            state.trace_sender,
            state.tags_provider,
            ApiVersion::V05,
        )
        .await;

        // Log response with body
        let status = response.status();
        let (parts, body) = response.into_parts();
        let body_bytes = match axum::body::to_bytes(body, usize::MAX).await {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to read response body for logging: {}", e);
                let response = Response::from_parts(parts, axum::body::Body::empty());
                return response;
            }
        };

        let response_body_preview = if parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false)
        {
            // JSON response - show as string
            if body_bytes.len() <= 1000 {
                String::from_utf8_lossy(&body_bytes).to_string()
            } else {
                format!(
                    "{}... ({} bytes total)",
                    String::from_utf8_lossy(&body_bytes[..1000]),
                    body_bytes.len()
                )
            }
        } else {
            // Binary/other response - show preview
            if body_bytes.len() <= 200 {
                format!("{:?}", body_bytes)
            } else {
                format!(
                    "{:?}... ({} bytes total)",
                    &body_bytes[..200],
                    body_bytes.len()
                )
            }
        };

        debug!(
            "HTTP Response: {} {} | Status: {} | Body: {}",
            method,
            path,
            status.as_u16(),
            response_body_preview
        );

        Response::from_parts(parts, axum::body::Body::from(body_bytes))
    }

    async fn stats(State(state): State<StatsState>, request: Request) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        // Extract body for logging
        let (parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                let response = error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {e}"),
                );
                return response;
            }
        };

        // Log request with body preview
        let body_preview = if parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false)
        {
            // JSON request - show as string
            if body.len() <= 1000 {
                String::from_utf8_lossy(&body).to_string()
            } else {
                format!(
                    "{}... ({} bytes total)",
                    String::from_utf8_lossy(&body[..1000]),
                    body.len()
                )
            }
        } else {
            // Binary/other request - show preview (limit to first 500 bytes for msgpack)
            let preview_bytes = if body.len() <= 500 {
                &body[..]
            } else {
                &body[..500]
            };

            // Try to convert to UTF-8
            match std::str::from_utf8(preview_bytes) {
                Ok(s) => {
                    if body.len() <= 500 {
                        s.to_string()
                    } else {
                        format!("{}... ({} bytes total)", s, body.len())
                    }
                }
                Err(_) => {
                    // Not valid UTF-8, show as hex for better readability
                    let hex_preview: String = preview_bytes
                        .iter()
                        .take(100)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");

                    if body.len() <= 500 {
                        format!("<hex: {}>", hex_preview)
                    } else {
                        format!("<hex: {}>... ({} bytes total)", hex_preview, body.len())
                    }
                }
            }
        };

        debug!(
            "HTTP Request: {} {} | Content-Type: {} | Content-Length: {} | Body: {}",
            method,
            path,
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none"),
            body.len(),
            body_preview
        );

        // Reconstruct request
        let request = Request::from_parts(parts, axum::body::Body::from(body));

        let response = match state
            .stats_processor
            .process_stats(request, state.stats_tx)
            .await
        {
            Ok(result) => result.into_response(),
            Err(err) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Error processing trace stats: {err}"),
            ),
        };

        // Log response with body
        let status = response.status();
        let (parts, body) = response.into_parts();
        let body_bytes = match axum::body::to_bytes(body, usize::MAX).await {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to read response body for logging: {}", e);
                let response = Response::from_parts(parts, axum::body::Body::empty());
                return response;
            }
        };

        let response_body_preview = if parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false)
        {
            // JSON response - show as string
            if body_bytes.len() <= 1000 {
                String::from_utf8_lossy(&body_bytes).to_string()
            } else {
                format!(
                    "{}... ({} bytes total)",
                    String::from_utf8_lossy(&body_bytes[..1000]),
                    body_bytes.len()
                )
            }
        } else {
            // Binary/other response - show preview
            if body_bytes.len() <= 200 {
                format!("{:?}", body_bytes)
            } else {
                format!(
                    "{:?}... ({} bytes total)",
                    &body_bytes[..200],
                    body_bytes.len()
                )
            }
        };

        debug!(
            "HTTP Response: {} {} | Status: {} | Body: {}",
            method,
            path,
            status.as_u16(),
            response_body_preview
        );

        Response::from_parts(parts, axum::body::Body::from(body_bytes))
    }

    async fn dsm_proxy(State(state): State<ProxyState>, request: Request) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        // Extract body for logging
        let (parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                let response = error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {e}"),
                );
                return response;
            }
        };

        // Log request with body preview
        // Check if body is compressed (Content-Encoding header)
        let content_encoding = parts
            .headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let is_json = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false);

        let body_preview = if is_json {
            // Try to decompress if compressed
            let decompressed_body = if content_encoding.contains("gzip") {
                // Attempt gzip decompression
                use flate2::read::GzDecoder;
                use std::io::Read;
                let mut decoder = GzDecoder::new(&body[..]);
                let mut decompressed = Vec::new();
                decoder
                    .read_to_end(&mut decompressed)
                    .ok()
                    .map(|_| decompressed)
            } else {
                // Not compressed, use body as-is
                Some(body.to_vec())
            };

            // Show JSON as string (decompressed if possible)
            if let Some(data) = decompressed_body {
                if data.len() <= 1000 {
                    String::from_utf8_lossy(&data).to_string()
                } else {
                    format!(
                        "{}... ({} bytes total, {} compressed)",
                        String::from_utf8_lossy(&data[..1000]),
                        data.len(),
                        body.len()
                    )
                }
            } else {
                // Decompression failed, show as binary
                if body.len() <= 500 {
                    format!("{:?} (gzip decompression failed)", body)
                } else {
                    format!(
                        "{:?}... ({} bytes total, gzip decompression failed)",
                        &body[..500],
                        body.len()
                    )
                }
            }
        } else {
            // Binary/other request - show preview
            let preview_bytes = if body.len() <= 500 {
                &body[..]
            } else {
                &body[..500]
            };

            // Try to convert to UTF-8
            match std::str::from_utf8(preview_bytes) {
                Ok(s) => {
                    if body.len() <= 500 {
                        s.to_string()
                    } else {
                        format!("{}... ({} bytes total)", s, body.len())
                    }
                }
                Err(_) => {
                    // Not valid UTF-8, show as hex for better readability
                    let hex_preview: String = preview_bytes
                        .iter()
                        .take(100)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");

                    if body.len() <= 500 {
                        format!("<hex: {}>", hex_preview)
                    } else {
                        format!("<hex: {}>... ({} bytes total)", hex_preview, body.len())
                    }
                }
            }
        };

        debug!(
            "HTTP Request: {} {} | Content-Type: {} | Content-Length: {} | Body: {}",
            method,
            path,
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none"),
            body.len(),
            body_preview
        );

        // Reconstruct request
        let request = Request::from_parts(parts, axum::body::Body::from(body));

        let response = Self::handle_proxy(
            state.config,
            state.proxy_aggregator,
            request,
            "trace.agent",
            DSM_INTAKE_PATH,
            "DSM",
            "agent-dsm",
            false, // Don't include service/version/config.tags for DSM
        )
        .await;

        debug!(
            "HTTP Response: {} {} | Status: {}",
            method,
            path,
            response.status().as_u16()
        );

        response
    }

    async fn profiling_proxy(State(state): State<ProxyState>, request: Request) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        // Extract body for logging
        let (parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                let response = error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {e}"),
                );
                return response;
            }
        };

        // Log request with body preview
        // Check if body is compressed (Content-Encoding header)
        let content_encoding = parts
            .headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let is_json = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false);

        let body_preview = if is_json {
            // Try to decompress if compressed
            let decompressed_body = if content_encoding.contains("gzip") {
                // Attempt gzip decompression
                use flate2::read::GzDecoder;
                use std::io::Read;
                let mut decoder = GzDecoder::new(&body[..]);
                let mut decompressed = Vec::new();
                decoder
                    .read_to_end(&mut decompressed)
                    .ok()
                    .map(|_| decompressed)
            } else {
                // Not compressed, use body as-is
                Some(body.to_vec())
            };

            // Show JSON as string (decompressed if possible)
            if let Some(data) = decompressed_body {
                if data.len() <= 1000 {
                    String::from_utf8_lossy(&data).to_string()
                } else {
                    format!(
                        "{}... ({} bytes total, {} compressed)",
                        String::from_utf8_lossy(&data[..1000]),
                        data.len(),
                        body.len()
                    )
                }
            } else {
                // Decompression failed, show as binary
                if body.len() <= 500 {
                    format!("{:?} (gzip decompression failed)", body)
                } else {
                    format!(
                        "{:?}... ({} bytes total, gzip decompression failed)",
                        &body[..500],
                        body.len()
                    )
                }
            }
        } else {
            // Binary/other request - show preview
            let preview_bytes = if body.len() <= 500 {
                &body[..]
            } else {
                &body[..500]
            };

            // Try to convert to UTF-8
            match std::str::from_utf8(preview_bytes) {
                Ok(s) => {
                    if body.len() <= 500 {
                        s.to_string()
                    } else {
                        format!("{}... ({} bytes total)", s, body.len())
                    }
                }
                Err(_) => {
                    // Not valid UTF-8, show as hex for better readability
                    let hex_preview: String = preview_bytes
                        .iter()
                        .take(100)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");

                    if body.len() <= 500 {
                        format!("<hex: {}>", hex_preview)
                    } else {
                        format!("<hex: {}>... ({} bytes total)", hex_preview, body.len())
                    }
                }
            }
        };

        debug!(
            "HTTP Request: {} {} | Content-Type: {} | Content-Length: {} | Body: {}",
            method,
            path,
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none"),
            body.len(),
            body_preview
        );

        // Reconstruct request
        let request = Request::from_parts(parts, axum::body::Body::from(body));

        let response = Self::handle_proxy(
            state.config,
            state.proxy_aggregator,
            request,
            "intake.profile",
            PROFILING_INTAKE_PATH,
            "profiling",
            "agent-profiling",
            false, // Don't include service/version/config.tags for profiling
        )
        .await;

        debug!(
            "HTTP Response: {} {} | Status: {}",
            method,
            path,
            response.status().as_u16()
        );

        response
    }

    /// Shared handler for debugger intake endpoints
    /// Proxies Dynamic Instrumentation messages to the debugger intake.
    async fn handle_debugger_intake_proxy(
        state: ProxyState,
        request: Request,
        metric_name: &str,
    ) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        // Extract body for logging
        let (parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                let response = error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {e}"),
                );
                return response;
            }
        };

        // Log request with body preview
        // Check if body is compressed (Content-Encoding header)
        let content_encoding = parts
            .headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let is_json = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false);

        let body_preview = if is_json {
            // Try to decompress if compressed
            let decompressed_body = if content_encoding.contains("gzip") {
                // Attempt gzip decompression
                use flate2::read::GzDecoder;
                use std::io::Read;
                let mut decoder = GzDecoder::new(&body[..]);
                let mut decompressed = Vec::new();
                decoder
                    .read_to_end(&mut decompressed)
                    .ok()
                    .map(|_| decompressed)
            } else {
                // Not compressed, use body as-is
                Some(body.to_vec())
            };

            // Show JSON as string (decompressed if possible)
            if let Some(data) = decompressed_body {
                if data.len() <= 1000 {
                    String::from_utf8_lossy(&data).to_string()
                } else {
                    format!(
                        "{}... ({} bytes total, {} compressed)",
                        String::from_utf8_lossy(&data[..1000]),
                        data.len(),
                        body.len()
                    )
                }
            } else {
                // Decompression failed, show as binary
                if body.len() <= 500 {
                    format!("{:?} (gzip decompression failed)", body)
                } else {
                    format!(
                        "{:?}... ({} bytes total, gzip decompression failed)",
                        &body[..500],
                        body.len()
                    )
                }
            }
        } else {
            // Binary/other request - try to show as UTF-8 string, fallback to hex
            let preview_bytes = if body.len() <= 500 {
                &body[..]
            } else {
                &body[..500]
            };

            // Try to convert to UTF-8
            match std::str::from_utf8(preview_bytes) {
                Ok(s) => {
                    if body.len() <= 500 {
                        s.to_string()
                    } else {
                        format!("{}... ({} bytes total)", s, body.len())
                    }
                }
                Err(_) => {
                    // Not valid UTF-8, show as hex for better readability
                    let hex_preview: String = preview_bytes
                        .iter()
                        .take(100)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");

                    if body.len() <= 500 {
                        format!("<hex: {}>", hex_preview)
                    } else {
                        format!("<hex: {}>... ({} bytes total)", hex_preview, body.len())
                    }
                }
            }
        };

        debug!(
            "HTTP Request: {} {} | Content-Type: {} | Content-Length: {} | Body: {}",
            method,
            path,
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none"),
            body.len(),
            body_preview
        );

        // Reconstruct request
        let request = Request::from_parts(parts, axum::body::Body::from(body));

        let response = Self::handle_proxy(
            state.config,
            state.proxy_aggregator,
            request,
            "debugger-intake",
            DEBUGGER_INTAKE_PATH,
            metric_name,
            "agent-debugger",
            false, // Don't include service/version/config.tags for debugger
        )
        .await;

        debug!(
            "HTTP Response: {} {} | Status: {}",
            method,
            path,
            response.status().as_u16()
        );

        response
    }

    /// Handler for debugger diagnostics endpoint (/debugger/v1/diagnostics)
    /// Proxies Dynamic Instrumentation diagnostic messages to the debugger intake.
    async fn debugger_diagnostics_proxy(
        State(state): State<ProxyState>,
        request: Request,
    ) -> Response {
        Self::handle_debugger_intake_proxy(state, request, "debugger_diagnostics").await
    }

    /// Handler for debugger v2 endpoint (/debugger/v2/input)
    /// Proxies Dynamic Instrumentation messages to the debugger intake (v2 API).
    async fn debugger_v2_proxy(State(state): State<ProxyState>, request: Request) -> Response {
        Self::handle_debugger_intake_proxy(state, request, "debugger_v2").await
    }

    /// Handler for Symbol Database endpoint (/symdb/v1/input)
    /// Proxies symbol database uploads to the debugger intake.
    /// NOTE: Unlike other endpoints, SymDB uses X-Datadog-Additional-Tags HEADER instead of ddtags query parameter.
    async fn symdb_proxy(State(state): State<ProxyState>, request: Request) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        // Extract body for logging
        let (parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                let response = error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {e}"),
                );
                return response;
            }
        };

        // Log request with body preview
        debug!(
            "HTTP Request: {} {} | Content-Type: {} | Content-Length: {} | Body: {}",
            method,
            path,
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("unknown"),
            body.len(),
            String::from_utf8_lossy(&body[..std::cmp::min(200, body.len())])
        );

        // Reconstruct request
        let request = Request::from_parts(parts, axum::body::Body::from(body));

        // Call core proxy handler with header-based tags
        let response = Self::handle_proxy_with_header_tags(
            state.config,
            state.proxy_aggregator,
            request,
            "debugger-intake",    // Backend subdomain (same as debugger)
            DEBUGGER_INTAKE_PATH, // Backend path (same as debugger)
            "symdb",              // Metrics name
            "agent-symdb",        // EVP-ORIGIN header
        )
        .await;

        debug!(
            "HTTP Response: {} {} | Status: {}",
            method,
            path,
            response.status().as_u16()
        );

        response
    }

    async fn llm_obs_eval_metric_proxy(
        State(state): State<ProxyState>,
        request: Request,
    ) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        // Extract body for logging
        let (parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                let response = error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {e}"),
                );
                return response;
            }
        };

        // Log request with body preview
        // Check if body is compressed (Content-Encoding header)
        let content_encoding = parts
            .headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let is_json = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false);

        let body_preview = if is_json {
            // Try to decompress if compressed
            let decompressed_body = if content_encoding.contains("gzip") {
                // Attempt gzip decompression
                use flate2::read::GzDecoder;
                use std::io::Read;
                let mut decoder = GzDecoder::new(&body[..]);
                let mut decompressed = Vec::new();
                decoder
                    .read_to_end(&mut decompressed)
                    .ok()
                    .map(|_| decompressed)
            } else {
                // Not compressed, use body as-is
                Some(body.to_vec())
            };

            // Show JSON as string (decompressed if possible)
            if let Some(data) = decompressed_body {
                if data.len() <= 1000 {
                    String::from_utf8_lossy(&data).to_string()
                } else {
                    format!(
                        "{}... ({} bytes total, {} compressed)",
                        String::from_utf8_lossy(&data[..1000]),
                        data.len(),
                        body.len()
                    )
                }
            } else {
                // Decompression failed, show as binary
                if body.len() <= 500 {
                    format!("{:?} (gzip decompression failed)", body)
                } else {
                    format!(
                        "{:?}... ({} bytes total, gzip decompression failed)",
                        &body[..500],
                        body.len()
                    )
                }
            }
        } else {
            // Binary/other request - show preview
            let preview_bytes = if body.len() <= 500 {
                &body[..]
            } else {
                &body[..500]
            };

            // Try to convert to UTF-8
            match std::str::from_utf8(preview_bytes) {
                Ok(s) => {
                    if body.len() <= 500 {
                        s.to_string()
                    } else {
                        format!("{}... ({} bytes total)", s, body.len())
                    }
                }
                Err(_) => {
                    // Not valid UTF-8, show as hex for better readability
                    let hex_preview: String = preview_bytes
                        .iter()
                        .take(100)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");

                    if body.len() <= 500 {
                        format!("<hex: {}>", hex_preview)
                    } else {
                        format!("<hex: {}>... ({} bytes total)", hex_preview, body.len())
                    }
                }
            }
        };

        debug!(
            "HTTP Request: {} {} | Content-Type: {} | Content-Length: {} | Body: {}",
            method,
            path,
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none"),
            body.len(),
            body_preview
        );

        // Reconstruct request
        let request = Request::from_parts(parts, axum::body::Body::from(body));

        let response = Self::handle_proxy(
            state.config,
            state.proxy_aggregator,
            request,
            "api",
            LLM_OBS_EVAL_METRIC_INTAKE_PATH,
            "llm_obs_eval_metric",
            "agent-llm-obs",
            false, // Don't include service/version/config.tags for LLM obs
        )
        .await;

        debug!(
            "HTTP Response: {} {} | Status: {}",
            method,
            path,
            response.status().as_u16()
        );

        response
    }

    async fn llm_obs_eval_metric_proxy_v2(
        State(state): State<ProxyState>,
        request: Request,
    ) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        // Extract body for logging
        let (parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                let response = error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {e}"),
                );
                return response;
            }
        };

        // Log request with body preview
        // Check if body is compressed (Content-Encoding header)
        let content_encoding = parts
            .headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let is_json = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false);

        let body_preview = if is_json {
            // Try to decompress if compressed
            let decompressed_body = if content_encoding.contains("gzip") {
                // Attempt gzip decompression
                use flate2::read::GzDecoder;
                use std::io::Read;
                let mut decoder = GzDecoder::new(&body[..]);
                let mut decompressed = Vec::new();
                decoder
                    .read_to_end(&mut decompressed)
                    .ok()
                    .map(|_| decompressed)
            } else {
                // Not compressed, use body as-is
                Some(body.to_vec())
            };

            // Show JSON as string (decompressed if possible)
            if let Some(data) = decompressed_body {
                if data.len() <= 1000 {
                    String::from_utf8_lossy(&data).to_string()
                } else {
                    format!(
                        "{}... ({} bytes total, {} compressed)",
                        String::from_utf8_lossy(&data[..1000]),
                        data.len(),
                        body.len()
                    )
                }
            } else {
                // Decompression failed, show as binary
                if body.len() <= 500 {
                    format!("{:?} (gzip decompression failed)", body)
                } else {
                    format!(
                        "{:?}... ({} bytes total, gzip decompression failed)",
                        &body[..500],
                        body.len()
                    )
                }
            }
        } else {
            // Binary/other request - show preview
            let preview_bytes = if body.len() <= 500 {
                &body[..]
            } else {
                &body[..500]
            };

            // Try to convert to UTF-8
            match std::str::from_utf8(preview_bytes) {
                Ok(s) => {
                    if body.len() <= 500 {
                        s.to_string()
                    } else {
                        format!("{}... ({} bytes total)", s, body.len())
                    }
                }
                Err(_) => {
                    // Not valid UTF-8, show as hex for better readability
                    let hex_preview: String = preview_bytes
                        .iter()
                        .take(100)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");

                    if body.len() <= 500 {
                        format!("<hex: {}>", hex_preview)
                    } else {
                        format!("<hex: {}>... ({} bytes total)", hex_preview, body.len())
                    }
                }
            }
        };

        debug!(
            "HTTP Request: {} {} | Content-Type: {} | Content-Length: {} | Body: {}",
            method,
            path,
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none"),
            body.len(),
            body_preview
        );

        // Reconstruct request
        let request = Request::from_parts(parts, axum::body::Body::from(body));

        let response = Self::handle_proxy(
            state.config,
            state.proxy_aggregator,
            request,
            "api",
            LLM_OBS_EVAL_METRIC_INTAKE_PATH_V2,
            "llm_obs_eval_metric",
            "agent-llm-obs",
            false, // Don't include service/version/config.tags for LLM obs
        )
        .await;

        debug!(
            "HTTP Response: {} {} | Status: {}",
            method,
            path,
            response.status().as_u16()
        );

        response
    }

    async fn llm_obs_spans_proxy(State(state): State<ProxyState>, request: Request) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        // Extract body for logging
        let (parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                let response = error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {e}"),
                );
                return response;
            }
        };

        // Log request with body preview
        // Check if body is compressed (Content-Encoding header)
        let content_encoding = parts
            .headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let is_json = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false);

        let body_preview = if is_json {
            // Try to decompress if compressed
            let decompressed_body = if content_encoding.contains("gzip") {
                // Attempt gzip decompression
                use flate2::read::GzDecoder;
                use std::io::Read;
                let mut decoder = GzDecoder::new(&body[..]);
                let mut decompressed = Vec::new();
                decoder
                    .read_to_end(&mut decompressed)
                    .ok()
                    .map(|_| decompressed)
            } else {
                // Not compressed, use body as-is
                Some(body.to_vec())
            };

            // Show JSON as string (decompressed if possible)
            if let Some(data) = decompressed_body {
                if data.len() <= 1000 {
                    String::from_utf8_lossy(&data).to_string()
                } else {
                    format!(
                        "{}... ({} bytes total, {} compressed)",
                        String::from_utf8_lossy(&data[..1000]),
                        data.len(),
                        body.len()
                    )
                }
            } else {
                // Decompression failed, show as binary
                if body.len() <= 500 {
                    format!("{:?} (gzip decompression failed)", body)
                } else {
                    format!(
                        "{:?}... ({} bytes total, gzip decompression failed)",
                        &body[..500],
                        body.len()
                    )
                }
            }
        } else {
            // Binary/other request - show preview
            let preview_bytes = if body.len() <= 500 {
                &body[..]
            } else {
                &body[..500]
            };

            // Try to convert to UTF-8
            match std::str::from_utf8(preview_bytes) {
                Ok(s) => {
                    if body.len() <= 500 {
                        s.to_string()
                    } else {
                        format!("{}... ({} bytes total)", s, body.len())
                    }
                }
                Err(_) => {
                    // Not valid UTF-8, show as hex for better readability
                    let hex_preview: String = preview_bytes
                        .iter()
                        .take(100)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");

                    if body.len() <= 500 {
                        format!("<hex: {}>", hex_preview)
                    } else {
                        format!("<hex: {}>... ({} bytes total)", hex_preview, body.len())
                    }
                }
            }
        };

        debug!(
            "HTTP Request: {} {} | Content-Type: {} | Content-Length: {} | Body: {}",
            method,
            path,
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none"),
            body.len(),
            body_preview
        );

        // Reconstruct request
        let request = Request::from_parts(parts, axum::body::Body::from(body));

        let response = Self::handle_proxy(
            state.config,
            state.proxy_aggregator,
            request,
            "llmobs-intake",
            LLM_OBS_SPANS_INTAKE_PATH,
            "llm_obs_spans",
            "agent-llm-obs",
            false, // Don't include service/version/config.tags for LLM obs
        )
        .await;

        debug!(
            "HTTP Response: {} {} | Status: {}",
            method,
            path,
            response.status().as_u16()
        );

        response
    }

    /// Handles legacy `/evp_proxy/v1/*` requests.
    async fn evp_proxy_v1(State(state): State<ProxyState>, request: Request) -> Response {
        Self::handle_evp_proxy(state, request, 1).await
    }

    /// Handles `/evp_proxy/v2/*` requests (current tracer default).
    async fn evp_proxy_v2(State(state): State<ProxyState>, request: Request) -> Response {
        Self::handle_evp_proxy(state, request, 2).await
    }

    /// Handles `/evp_proxy/v3/*` requests (reserved for compatibility).
    async fn evp_proxy_v3(State(state): State<ProxyState>, request: Request) -> Response {
        Self::handle_evp_proxy(state, request, 3).await
    }

    /// Handles `/evp_proxy/v4/*` requests (latest behaviour).
    async fn evp_proxy_v4(State(state): State<ProxyState>, request: Request) -> Response {
        Self::handle_evp_proxy(state, request, 4).await
    }

    /// Streams EVP proxy requests directly to the backend, mirroring the Go agent.
    async fn handle_evp_proxy(state: ProxyState, request: Request, version: u8) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        debug!(
            "TRACE_AGENT | EVP proxy handler called: {} {}",
            method, path
        );

        let (mut parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                let message = e.to_string();
                if message.contains("length limit exceeded") {
                    return error_response(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!(
                            "Payload exceeds maximum allowed size of {} bytes",
                            EVP_PROXY_MAX_PAYLOAD_BYTES
                        ),
                    );
                }

                return error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {message}"),
                );
            }
        };

        if body.len() > EVP_PROXY_MAX_PAYLOAD_BYTES {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "Payload exceeds maximum allowed size of {} bytes",
                    EVP_PROXY_MAX_PAYLOAD_BYTES
                ),
            );
        }

        let subdomain = match parts
            .headers
            .get(EVP_PROXY_SUBDOMAIN_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
        {
            Some(value) if !value.is_empty() => value.to_string(),
            _ => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "Missing X-Datadog-EVP-Subdomain header",
                );
            }
        };

        if !Self::is_valid_evp_subdomain(&subdomain) {
            return error_response(
                StatusCode::BAD_REQUEST,
                "Invalid subdomain provided for EVP proxy",
            );
        }

        let target_path = match Self::extract_evp_target_path(parts.uri.path(), version) {
            Some(path) => path,
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "Invalid path for EVP proxy endpoint",
                );
            }
        };

        if !Self::is_valid_evp_path(&target_path) {
            return error_response(
                StatusCode::BAD_REQUEST,
                "Path contains invalid characters for EVP proxy",
            );
        }

        if let Some(query) = parts.uri.query() {
            if !Self::is_valid_evp_query(query) {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "Query string contains invalid characters for EVP proxy",
                );
            }
        }

        let needs_app_key = Self::header_truthy(&parts.headers, EVP_PROXY_NEEDS_APP_KEY_HEADER);

        parts.headers.remove(EVP_PROXY_SUBDOMAIN_HEADER);
        parts.headers.remove(EVP_PROXY_NEEDS_APP_KEY_HEADER);

        let target_url = Self::build_evp_target_url(
            &state.config.site,
            &subdomain,
            &target_path,
            parts.uri.query(),
        );

        let mut forward_headers = Self::collect_allowed_evp_headers(&parts.headers);

        forward_headers.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static("trace-agent"),
        );
        if let Ok(via_value) =
            HeaderValue::from_str(&format!("trace-agent {}", env!("AGENT_VERSION")))
        {
            forward_headers.insert(HeaderName::from_static("via"), via_value);
        }

        let request_id = Uuid::new_v4().to_string();
        if let Ok(request_id_value) = HeaderValue::from_str(&request_id) {
            forward_headers.insert(HeaderName::from_static("dd-request-id"), request_id_value);
        }

        forward_headers.insert(
            HeaderName::from_static("dd-evp-origin"),
            HeaderValue::from_static(EVP_PROXY_ORIGIN),
        );

        if let Ok(timeout_value) =
            HeaderValue::from_str(&EVP_PROXY_DEFAULT_TIMEOUT_SECS.to_string())
        {
            forward_headers.insert(HeaderName::from_static("x-datadog-timeout"), timeout_value);
        }

        let hostname_value = crate::proc::hostname::get_hostname();
        if let Ok(host_value) = HeaderValue::from_str(&hostname_value) {
            forward_headers.insert(HeaderName::from_static("x-datadog-hostname"), host_value);
        }

        let default_env = state.config.env.as_deref().unwrap_or("none");
        if let Ok(env_value) = HeaderValue::from_str(default_env) {
            forward_headers.insert(
                HeaderName::from_static("x-datadog-agentdefaultenv"),
                env_value,
            );
        }

        let container_id_provider = super::container::ContainerIDProvider::new();
        let container_tags_provider = super::container::ContainerTagsProvider::new();
        if let Some(container_id) = container_id_provider.get_container_id(&parts.headers) {
            if let Ok(container_value) = HeaderValue::from_str(&container_id) {
                forward_headers.insert(
                    HeaderName::from_static("datadog-container-id"),
                    container_value,
                );
            }

            if let Some(formatted_tags) = container_tags_provider.format_tags(&container_id) {
                if let Ok(tag_value) = HeaderValue::from_str(&formatted_tags) {
                    forward_headers.insert(
                        HeaderName::from_static("x-datadog-container-tags"),
                        tag_value,
                    );
                }
            }
        }

        if needs_app_key {
            match env::var("DD_APP_KEY").ok().filter(|v| !v.trim().is_empty()) {
                Some(app_key) => {
                    if let Ok(app_key_value) = HeaderValue::from_str(app_key.trim()) {
                        forward_headers
                            .insert(HeaderName::from_static("dd-application-key"), app_key_value);
                    } else {
                        return error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "Invalid DD_APP_KEY value",
                        );
                    }
                }
                None => {
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "DD_APP_KEY must be configured when X-Datadog-NeedsAppKey is true",
                    );
                }
            }
        }

        let api_key = state.config.api_key.trim();
        if api_key.is_empty() {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "API key is required for EVP proxy forwarding",
            );
        }
        if let Ok(api_key_value) = HeaderValue::from_str(api_key) {
            forward_headers.insert(HeaderName::from_static("dd-api-key"), api_key_value);
        } else {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid API key value for EVP proxy",
            );
        }

        let client = get_client(&state.config);
        let request_builder = client
            .request(method.clone(), &target_url)
            .headers(forward_headers)
            .timeout(Duration::from_secs(EVP_PROXY_DEFAULT_TIMEOUT_SECS))
            .body(body);

        let upstream_response = match request_builder.send().await {
            Ok(resp) => resp,
            Err(err) => {
                error!(
                    "TRACE_AGENT | EVP proxy request failed ({} {}): {}",
                    method, path, err
                );
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "Failed to reach Datadog EVP backend",
                );
            }
        };

        let status = upstream_response.status();
        let headers_snapshot = upstream_response.headers().clone();
        let body_stream = upstream_response.bytes_stream();
        let response_body = Body::from_stream(body_stream);

        let mut response_builder = Response::builder().status(status);
        for (name, value) in headers_snapshot.iter() {
            if name == reqwest::header::TRANSFER_ENCODING {
                continue;
            }
            response_builder = response_builder.header(name, value);
        }

        match response_builder.body(response_body) {
            Ok(response) => response,
            Err(err) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to build EVP proxy response: {err}"),
            ),
        }
    }

    /// Proxies tracer flare uploads to the Datadog serverless flare endpoint.
    async fn tracer_flare_proxy(State(state): State<ProxyState>, request: Request) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        debug!(
            "TRACE_AGENT | tracer flare handler called: {} {}",
            method, path
        );

        let (parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {e}"),
                );
            }
        };

        let api_key = state.config.api_key.trim();
        if api_key.is_empty() {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "API key is required to forward tracer flare requests",
            );
        }

        let target_url =
            match Self::build_tracer_flare_url(&state.config.site, env!("AGENT_VERSION")) {
                Some(url) => url,
                None => {
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to compute tracer flare target URL",
                    );
                }
            };

        let mut headers = parts.headers.clone();
        headers.remove(axum::http::header::HOST);
        headers.remove(axum::http::header::CONTENT_LENGTH);
        headers.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static("trace-agent"),
        );
        if let Ok(host_value) = HeaderValue::from_str(&crate::proc::hostname::get_hostname()) {
            headers.insert(HeaderName::from_static("x-datadog-hostname"), host_value);
        }
        match HeaderValue::from_str(api_key) {
            Ok(value) => {
                headers.insert(HeaderName::from_static("dd-api-key"), value);
            }
            Err(_) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Invalid API key value for tracer flare proxy",
                );
            }
        }

        let client = get_client(&state.config);
        let upstream = match client
            .request(method.clone(), &target_url)
            .headers(headers)
            .timeout(Duration::from_secs(EVP_PROXY_DEFAULT_TIMEOUT_SECS))
            .body(body.clone())
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                error!(
                    "TRACE_AGENT | tracer flare proxy failed to reach backend: {}",
                    err
                );
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "Failed to reach Datadog serverless flare backend",
                );
            }
        };

        let status = upstream.status();
        let headers_snapshot = upstream.headers().clone();
        let response_body = Body::from_stream(upstream.bytes_stream());

        let mut response_builder = Response::builder().status(status);
        for (name, value) in headers_snapshot.iter() {
            if name == reqwest::header::TRANSFER_ENCODING {
                continue;
            }
            response_builder = response_builder.header(name, value);
        }

        match response_builder.body(response_body) {
            Ok(response) => response,
            Err(err) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to build tracer flare response: {err}"),
            ),
        }
    }

    async fn debugger_logs_proxy(State(state): State<ProxyState>, request: Request) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        // Extract body for logging
        let (parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                let response = error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {e}"),
                );
                return response;
            }
        };

        // Log request with body preview
        // Check if body is compressed (Content-Encoding header)
        let content_encoding = parts
            .headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let is_json = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false);

        let body_preview = if is_json {
            // Try to decompress if compressed
            let decompressed_body = if content_encoding.contains("gzip") {
                // Attempt gzip decompression
                use flate2::read::GzDecoder;
                use std::io::Read;
                let mut decoder = GzDecoder::new(&body[..]);
                let mut decompressed = Vec::new();
                decoder
                    .read_to_end(&mut decompressed)
                    .ok()
                    .map(|_| decompressed)
            } else {
                // Not compressed, use body as-is
                Some(body.to_vec())
            };

            // Show JSON as string (decompressed if possible)
            if let Some(data) = decompressed_body {
                if data.len() <= 1000 {
                    String::from_utf8_lossy(&data).to_string()
                } else {
                    format!(
                        "{}... ({} bytes total, {} compressed)",
                        String::from_utf8_lossy(&data[..1000]),
                        data.len(),
                        body.len()
                    )
                }
            } else {
                // Decompression failed, show as binary
                if body.len() <= 500 {
                    format!("{:?} (gzip decompression failed)", body)
                } else {
                    format!(
                        "{:?}... ({} bytes total, gzip decompression failed)",
                        &body[..500],
                        body.len()
                    )
                }
            }
        } else {
            // Binary/other request - show preview
            let preview_bytes = if body.len() <= 500 {
                &body[..]
            } else {
                &body[..500]
            };

            // Try to convert to UTF-8
            match std::str::from_utf8(preview_bytes) {
                Ok(s) => {
                    if body.len() <= 500 {
                        s.to_string()
                    } else {
                        format!("{}... ({} bytes total)", s, body.len())
                    }
                }
                Err(_) => {
                    // Not valid UTF-8, show as hex for better readability
                    let hex_preview: String = preview_bytes
                        .iter()
                        .take(100)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");

                    if body.len() <= 500 {
                        format!("<hex: {}>", hex_preview)
                    } else {
                        format!("<hex: {}>... ({} bytes total)", hex_preview, body.len())
                    }
                }
            }
        };

        debug!(
            "HTTP Request: {} {} | Content-Type: {} | Content-Length: {} | Body: {}",
            method,
            path,
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none"),
            body.len(),
            body_preview
        );

        // Reconstruct request
        let request = Request::from_parts(parts, axum::body::Body::from(body));

        let response = Self::handle_proxy(
            state.config,
            state.proxy_aggregator,
            request,
            "http-intake.logs",
            DEBUGGER_LOGS_INTAKE_PATH,
            "debugger_logs",
            "agent-debugger",
            false, // Don't include service/version/config.tags for debugger logs
        )
        .await;

        debug!(
            "HTTP Response: {} {} | Status: {}",
            method,
            path,
            response.status().as_u16()
        );

        response
    }

    async fn logs_proxy(State(state): State<ProxyState>, request: Request) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        // Extract body for logging
        let (parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                let response = error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {e}"),
                );
                return response;
            }
        };

        // Log request with body preview
        // Check if body is compressed (Content-Encoding header)
        let content_encoding = parts
            .headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let is_json = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false);

        let body_preview = if is_json {
            // Try to decompress if compressed
            let decompressed_body = if content_encoding.contains("gzip") {
                // Attempt gzip decompression
                use flate2::read::GzDecoder;
                use std::io::Read;
                let mut decoder = GzDecoder::new(&body[..]);
                let mut decompressed = Vec::new();
                decoder
                    .read_to_end(&mut decompressed)
                    .ok()
                    .map(|_| decompressed)
            } else {
                // Not compressed, use body as-is
                Some(body.to_vec())
            };

            // Show JSON as string (decompressed if possible)
            if let Some(data) = decompressed_body {
                if data.len() <= 1000 {
                    String::from_utf8_lossy(&data).to_string()
                } else {
                    format!(
                        "{}... ({} bytes total, {} compressed)",
                        String::from_utf8_lossy(&data[..1000]),
                        data.len(),
                        body.len()
                    )
                }
            } else {
                // Decompression failed, show as binary
                if body.len() <= 500 {
                    format!("{:?} (gzip decompression failed)", body)
                } else {
                    format!(
                        "{:?}... ({} bytes total, gzip decompression failed)",
                        &body[..500],
                        body.len()
                    )
                }
            }
        } else {
            // Binary/other request - show preview
            let preview_bytes = if body.len() <= 500 {
                &body[..]
            } else {
                &body[..500]
            };

            // Try to convert to UTF-8
            match std::str::from_utf8(preview_bytes) {
                Ok(s) => {
                    if body.len() <= 500 {
                        s.to_string()
                    } else {
                        format!("{}... ({} bytes total)", s, body.len())
                    }
                }
                Err(_) => {
                    // Not valid UTF-8, show as hex for better readability
                    let hex_preview: String = preview_bytes
                        .iter()
                        .take(100)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");

                    if body.len() <= 500 {
                        format!("<hex: {}>", hex_preview)
                    } else {
                        format!("<hex: {}>... ({} bytes total)", hex_preview, body.len())
                    }
                }
            }
        };

        debug!(
            "HTTP Request: {} {} | Content-Type: {} | Content-Length: {} | Body: {}",
            method,
            path,
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none"),
            body.len(),
            body_preview
        );

        // Reconstruct request
        let request = Request::from_parts(parts, axum::body::Body::from(body));

        let response = Self::handle_proxy(
            state.config,
            state.proxy_aggregator,
            request,
            "http-intake.logs",
            "/api/v2/logs",
            "application_logs",
            "agent-logs",
            true, // Include service/version/config.tags for logs endpoint
        )
        .await;

        debug!(
            "HTTP Response: {} {} | Status: {}",
            method,
            path,
            response.status().as_u16()
        );

        response
    }

    async fn instrumentation_proxy(State(state): State<ProxyState>, request: Request) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        // Extract body for logging
        let (parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                let response = error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {e}"),
                );
                return response;
            }
        };

        // Log request with body preview
        // Check if body is compressed (Content-Encoding header)
        let content_encoding = parts
            .headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let is_json = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false);

        let body_preview = if is_json {
            // Try to decompress if compressed
            let decompressed_body = if content_encoding.contains("gzip") {
                // Attempt gzip decompression
                use flate2::read::GzDecoder;
                use std::io::Read;
                let mut decoder = GzDecoder::new(&body[..]);
                let mut decompressed = Vec::new();
                decoder
                    .read_to_end(&mut decompressed)
                    .ok()
                    .map(|_| decompressed)
            } else {
                // Not compressed, use body as-is
                Some(body.to_vec())
            };

            // Show JSON as string (decompressed if possible)
            if let Some(data) = decompressed_body {
                if data.len() <= 1000 {
                    String::from_utf8_lossy(&data).to_string()
                } else {
                    format!(
                        "{}... ({} bytes total, {} compressed)",
                        String::from_utf8_lossy(&data[..1000]),
                        data.len(),
                        body.len()
                    )
                }
            } else {
                // Decompression failed, show as binary
                if body.len() <= 500 {
                    format!("{:?} (gzip decompression failed)", body)
                } else {
                    format!(
                        "{:?}... ({} bytes total, gzip decompression failed)",
                        &body[..500],
                        body.len()
                    )
                }
            }
        } else {
            // Binary/other request - show preview
            let preview_bytes = if body.len() <= 500 {
                &body[..]
            } else {
                &body[..500]
            };

            // Try to convert to UTF-8
            match std::str::from_utf8(preview_bytes) {
                Ok(s) => {
                    if body.len() <= 500 {
                        s.to_string()
                    } else {
                        format!("{}... ({} bytes total)", s, body.len())
                    }
                }
                Err(_) => {
                    // Not valid UTF-8, show as hex for better readability
                    let hex_preview: String = preview_bytes
                        .iter()
                        .take(100)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");

                    if body.len() <= 500 {
                        format!("<hex: {}>", hex_preview)
                    } else {
                        format!("<hex: {}>... ({} bytes total)", hex_preview, body.len())
                    }
                }
            }
        };

        debug!(
            "HTTP Request: {} {} | Content-Type: {} | Content-Length: {} | Body: {}",
            method,
            path,
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none"),
            body.len(),
            body_preview
        );

        // Reconstruct request
        let request = Request::from_parts(parts, axum::body::Body::from(body));

        let response = Self::handle_proxy(
            state.config,
            state.proxy_aggregator,
            request,
            "instrumentation-telemetry-intake",
            INSTRUMENTATION_INTAKE_PATH,
            "instrumentation",
            "agent-instrumentation",
            false, // Don't include service/version/config.tags for instrumentation
        )
        .await;

        debug!(
            "HTTP Response: {} {} | Status: {}",
            method,
            path,
            response.status().as_u16()
        );

        response
    }

    /// Handler for DogStatsD proxy endpoint
    /// Receives StatsD metrics via HTTP POST and forwards them via UDP to the local DogStatsD server
    async fn dogstatsd_proxy(State(state): State<ProxyState>, request: Request) -> Response {
        debug!("TRACE_AGENT | dogstatsd_proxy called");

        // Extract body from request
        let body = match extract_request_body(request).await {
            Ok((_, body)) => body,
            Err(e) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {e}"),
                );
            }
        };

        // Skip if body is empty
        if body.is_empty() {
            return success_response("DogStatsD proxy: no metrics to forward");
        }

        // Get the DogStatsD port from config
        let dogstatsd_port = state.config.dogstatsd_port;
        let target_addr = format!("127.0.0.1:{}", dogstatsd_port);

        // Create UDP socket for sending metrics
        let socket = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to create UDP socket for DogStatsD proxy: {}", e);
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to create UDP socket: {e}"),
                );
            }
        };

        // Split body by newlines and forward each metric
        let metrics: Vec<&[u8]> = body.split(|&b| b == b'\n').collect();
        let mut sent_count = 0;
        let mut error_count = 0;

        for metric in metrics {
            if metric.is_empty() {
                continue;
            }

            match socket.send_to(metric, &target_addr).await {
                Ok(_) => sent_count += 1,
                Err(e) => {
                    error!(
                        "Failed to send metric to DogStatsD at {}: {}",
                        target_addr, e
                    );
                    error_count += 1;
                }
            }
        }

        debug!(
            "DogStatsD proxy: sent {} metrics, {} errors to {}",
            sent_count, error_count, target_addr
        );

        success_response("DogStatsD proxy: metrics forwarded")
    }

    #[allow(clippy::unused_async)]
    async fn info(State(trace_state): State<TraceState>, request: Request) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();

        debug!("TRACE_AGENT | info handler called: {} {}", method, path);

        // Extract body for logging (info endpoint typically has no body, but log it anyway)
        let (parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                let response = error_response(
                    StatusCode::BAD_REQUEST,
                    format!("Error extracting request body: {e}"),
                );
                return response;
            }
        };

        // Log request with body (typically empty for GET requests)
        let body_preview = if body.is_empty() {
            "(empty)".to_string()
        } else if body.len() <= 200 {
            String::from_utf8_lossy(&body).to_string()
        } else {
            format!(
                "{}... ({} bytes total)",
                String::from_utf8_lossy(&body[..200]),
                body.len()
            )
        };

        debug!(
            "HTTP Request: {} {} | Content-Type: {} | Content-Length: {} | Body: {}",
            method,
            path,
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none"),
            body.len(),
            body_preview
        );

        let config = &trace_state.config;

        let response_json = json!(
            {
                "version": "7.70.0",
                "endpoints": [
                    V4_TRACE_ENDPOINT_PATH,
                    V5_TRACE_ENDPOINT_PATH,
                    STATS_ENDPOINT_PATH,
                    REMOTE_CONFIG_ENDPOINT_PATH,
                    DSM_AGENT_PATH,
                    PROFILING_ENDPOINT_PATH,
                    INFO_ENDPOINT_PATH,
                    LLM_OBS_EVAL_METRIC_ENDPOINT_PATH,
                    LLM_OBS_EVAL_METRIC_ENDPOINT_PATH_V2,
                    LLM_OBS_SPANS_ENDPOINT_PATH,
                    DEBUGGER_ENDPOINT_PATH,
                    DEBUGGER_DIAGNOSTICS_ENDPOINT_PATH,
                    DEBUGGER_V2_ENDPOINT_PATH,
                    SYMDB_ENDPOINT_PATH,
                    LOGS_ENDPOINT_PATH,
                    INSTRUMENTATION_ENDPOINT_PATH,
                    DOGSTATSD_V2_PROXY_PATH,
                    TRACER_FLARE_ENDPOINT_PATH,
                    EVP_PROXY_V1_BASE_PATH,
                    EVP_PROXY_V2_BASE_PATH,
                    EVP_PROXY_V3_BASE_PATH,
                    EVP_PROXY_V4_BASE_PATH,
                ],
                "client_drop_p0s": true,
                "span_meta_structs": true,
                "long_running_spans": true,
                "span_events": true,
                "config": {
                    "default_env": config.env.as_ref().unwrap_or(&"none".to_string()),
                    "max_request_bytes": MAX_CONTENT_LENGTH,
                    "receiver_port": config.trace_agent_port.unwrap_or(8126),
                    "statsd_port": config.dogstatsd_port,
                    "obfuscation": {
                        "http": {
                            "remove_query_string": config.apm_config_obfuscation_http_remove_query_string,
                            "remove_path_digits": config.apm_config_obfuscation_http_remove_paths_with_digits,
                        }
                    }
                }
            }
        );
        let response_body = response_json.to_string();
        let response = (StatusCode::OK, response_body.clone()).into_response();

        debug!(
            "HTTP Response: {} {} | Status: {} | Body size: {} bytes | Body: {}",
            method,
            path,
            response.status().as_u16(),
            response_body.len(),
            response_body
        );

        response
    }

    /// Remote Config endpoint handler supporting both protobuf and JSON.
    ///
    /// This endpoint handles tracer requests for Remote Configuration via `/v0.7/config`.
    ///
    /// ## Supported Content Types
    ///
    /// **Protobuf (default):**
    /// - Request: `Content-Type: application/x-protobuf`
    /// - Response: `Content-Type: application/x-protobuf`
    /// - Default format when Content-Type header is missing
    ///
    /// **JSON:**
    /// - Request: `Content-Type: application/json`
    /// - Response: `Content-Type: application/json`
    ///
    /// ## Message Format
    ///
    /// Request: `ClientGetConfigsRequest`
    /// ```json
    /// {
    ///   "client": {
    ///     "id": "client-id",
    ///     "products": ["APM"],
    ///     "is_tracer": true,
    ///     "client_tracer": {
    ///       "runtime_id": "runtime-id",
    ///       "language": "rust",
    ///       "tracer_version": "1.0.0",
    ///       "service": "my-service",
    ///       "env": "production"
    ///     }
    ///   },
    ///   "cached_target_files": []
    /// }
    /// ```
    ///
    /// Response: `ClientGetConfigsResponse`
    /// ```json
    /// {
    ///   "roots": ["<base64-encoded-root>"],
    ///   "targets": "<base64-encoded-targets>",
    ///   "target_files": [],
    ///   "client_configs": []
    /// }
    /// ```
    async fn remote_config(State(state): State<RemoteConfigState>, request: Request) -> Response {
        let method = request.method().clone();
        let path = request.uri().path().to_string();
        debug!(
            "REMOTE_CONFIG | Handler called: {} {} | Headers: {:?}",
            method,
            path,
            request.headers()
        );

        // Extract the request parts and body
        let (parts, body) = match extract_request_body(request).await {
            Ok(parts) => parts,
            Err(e) => {
                let response = error_response(
                    StatusCode::BAD_REQUEST,
                    format!("TRACE_AGENT | remote_config | Error extracting request body: {e}"),
                );
                debug!(
                    "HTTP Response: {} {} | Status: {}",
                    method,
                    path,
                    response.status().as_u16()
                );
                return response;
            }
        };

        debug!("REMOTE_CONFIG | Request body size: {} bytes", body.len());

        // Log request with body preview
        // Check if body is compressed (Content-Encoding header)
        let content_encoding = parts
            .headers
            .get("content-encoding")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let is_json = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false);

        let body_preview = if is_json {
            // Try to decompress if compressed
            let decompressed_body = if content_encoding.contains("gzip") {
                // Attempt gzip decompression
                use flate2::read::GzDecoder;
                use std::io::Read;
                let mut decoder = GzDecoder::new(&body[..]);
                let mut decompressed = Vec::new();
                decoder
                    .read_to_end(&mut decompressed)
                    .ok()
                    .map(|_| decompressed)
            } else {
                // Not compressed, use body as-is
                Some(body.to_vec())
            };

            // Show JSON as string (decompressed if possible)
            if let Some(data) = decompressed_body {
                if data.len() <= 1000 {
                    String::from_utf8_lossy(&data).to_string()
                } else {
                    format!(
                        "{}... ({} bytes total, {} compressed)",
                        String::from_utf8_lossy(&data[..1000]),
                        data.len(),
                        body.len()
                    )
                }
            } else {
                // Decompression failed, show as binary
                if body.len() <= 500 {
                    format!("{:?} (gzip decompression failed)", body)
                } else {
                    format!(
                        "{:?}... ({} bytes total, gzip decompression failed)",
                        &body[..500],
                        body.len()
                    )
                }
            }
        } else {
            // Binary/other request - show preview
            let preview_bytes = if body.len() <= 500 {
                &body[..]
            } else {
                &body[..500]
            };

            // Try to convert to UTF-8
            match std::str::from_utf8(preview_bytes) {
                Ok(s) => {
                    if body.len() <= 500 {
                        s.to_string()
                    } else {
                        format!("{}... ({} bytes total)", s, body.len())
                    }
                }
                Err(_) => {
                    // Not valid UTF-8, show as hex for better readability
                    let hex_preview: String = preview_bytes
                        .iter()
                        .take(100)
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join(" ");

                    if body.len() <= 500 {
                        format!("<hex: {}>", hex_preview)
                    } else {
                        format!("<hex: {}>... ({} bytes total)", hex_preview, body.len())
                    }
                }
            }
        };

        debug!(
            "HTTP Request: {} {} | Content-Type: {} | Content-Length: {} | Body: {}",
            method,
            path,
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("none"),
            body.len(),
            body_preview
        );

        // Determine content type from header (default to protobuf if not specified)
        let content_type = parts
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/x-protobuf");

        let is_json = content_type.contains("application/json");

        // Parse the request based on content type
        let request_msg = if is_json {
            // Parse JSON request
            match serde_json::from_slice::<ClientGetConfigsRequest>(&body) {
                Ok(req) => req,
                Err(e) => {
                    let response = error_response(
                        StatusCode::BAD_REQUEST,
                        format!("TRACE_AGENT | remote_config | Error parsing JSON request: {e}"),
                    );
                    debug!(
                        "HTTP Response: {} {} | Status: {}",
                        method,
                        path,
                        response.status().as_u16()
                    );
                    return response;
                }
            }
        } else {
            // Parse protobuf request
            match ClientGetConfigsRequest::decode(&body[..]) {
                Ok(req) => req,
                Err(e) => {
                    let response = error_response(
                        StatusCode::BAD_REQUEST,
                        format!(
                            "TRACE_AGENT | remote_config | Error parsing protobuf request: {e}"
                        ),
                    );
                    debug!(
                        "HTTP Response: {} {} | Status: {}",
                        method,
                        path,
                        response.status().as_u16()
                    );
                    return response;
                }
            }
        };

        // Process the remote config request using the service
        let response = match state
            .remote_config_service
            .client_get_configs(request_msg)
            .await
        {
            Ok(response_msg) => {
                // Return the response in the same format as the request
                if is_json {
                    // Return JSON response
                    match serde_json::to_vec(&response_msg) {
                        Ok(json_bytes) => {
                            // Log response body content
                            let body_preview = if json_bytes.len() <= 1000 {
                                String::from_utf8_lossy(&json_bytes).to_string()
                            } else {
                                format!(
                                    "{}... ({} bytes total)",
                                    String::from_utf8_lossy(&json_bytes[..1000]),
                                    json_bytes.len()
                                )
                            };

                            let response = (
                                StatusCode::OK,
                                [("Content-Type", "application/json")],
                                json_bytes,
                            )
                                .into_response();
                            debug!(
                                "HTTP Response: {} {} | Status: {} | Body: {}",
                                method,
                                path,
                                response.status().as_u16(),
                                body_preview
                            );
                            response
                        }
                        Err(e) => {
                            let response = error_response(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                format!("TRACE_AGENT | remote_config | Error serializing JSON response: {e}"),
                            );
                            debug!(
                                "HTTP Response: {} {} | Status: {}",
                                method,
                                path,
                                response.status().as_u16()
                            );
                            response
                        }
                    }
                } else {
                    // Return protobuf response
                    let response_bytes = response_msg.encode_to_vec();

                    // Log response body content (binary preview)
                    let body_preview = if response_bytes.len() <= 200 {
                        format!("{:?}", response_bytes)
                    } else {
                        format!(
                            "{:?}... ({} bytes total)",
                            &response_bytes[..200],
                            response_bytes.len()
                        )
                    };

                    let response = (
                        StatusCode::OK,
                        [("Content-Type", "application/x-protobuf")],
                        response_bytes,
                    )
                        .into_response();
                    debug!(
                        "HTTP Response: {} {} | Status: {} | Body: {}",
                        method,
                        path,
                        response.status().as_u16(),
                        body_preview
                    );
                    response
                }
            }
            Err(err) => {
                error!(
                    "TRACE_AGENT | remote_config | Error processing config request: {}",
                    err
                );
                let response = error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Error processing remote config request: {err}"),
                );
                debug!(
                    "HTTP Response: {} {} | Status: {}",
                    method,
                    path,
                    response.status().as_u16()
                );
                response
            }
        };

        response
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_lines)]
    async fn handle_traces(
        config: Arc<config::Config>,
        request: Request,
        trace_sender: Arc<SendingTraceProcessor>,
        tags_provider: Arc<provider::Provider>,
        version: ApiVersion,
    ) -> Response {
        let (parts, body) = match extract_request_body(request).await {
            Ok(r) => r,
            Err(e) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("TRACE_AGENT | handle_traces | Error extracting request body: {e}"),
                );
            }
        };

        if let Some(content_length) = parts
            .headers
            .get("content-length")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.parse::<usize>().ok())
            .filter(|l| *l > MAX_CONTENT_LENGTH)
        {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "Content-Length {content_length} exceeds maximum allowed size {MAX_CONTENT_LENGTH}"
                ),
            );
        }

        let tracer_header_tags = (&parts.headers).into();

        let (body_size, traces) = match version {
            ApiVersion::V04 => match trace_utils::get_traces_from_request_body(
                hyper_migration::Body::from_bytes(body),
            )
            .await
            {
                Ok(result) => result,
                Err(err) => {
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Error deserializing trace from request body: {err}"),
                    );
                }
            },
            ApiVersion::V05 => match trace_utils::get_v05_traces_from_request_body(
                hyper_migration::Body::from_bytes(body),
            )
            .await
            {
                Ok(result) => result,
                Err(err) => {
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Error deserializing trace from request body: {err}"),
                    );
                }
            },
        };

        // Send traces to the trace aggregator
        if let Err(err) = trace_sender
            .send_processed_traces(
                config,
                tags_provider,
                tracer_header_tags,
                traces,
                body_size,
                None,
            )
            .await
        {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Error sending traces to the trace aggregator: {err:?}"),
            );
        }

        success_response("Successfully buffered traces to be aggregated.")
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_proxy(
        config: Arc<config::Config>,
        proxy_aggregator: Arc<Mutex<proxy_aggregator::Aggregator>>,
        request: Request,
        backend_domain: &str,
        backend_path: &str,
        context: &str,
        evp_origin: &str,
        include_service_tags: bool,
    ) -> Response {
        debug!("TRACE_AGENT | Proxied request for {context}");
        let (mut parts, body) = match extract_request_body(request).await {
            Ok(r) => r,
            Err(e) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("TRACE_AGENT | handle_proxy | Error extracting request body: {e}"),
                );
            }
        };

        // Add DD-REQUEST-ID header (UUID v4)
        let request_id = Uuid::new_v4().to_string();
        parts
            .headers
            .insert("DD-REQUEST-ID", request_id.parse().unwrap());

        // Add DD-EVP-ORIGIN header
        parts
            .headers
            .insert("DD-EVP-ORIGIN", evp_origin.parse().unwrap());

        // Build ddtags query parameter
        let mut tags = Vec::new();

        // Get hostname (use nix crate since it's already a dependency)
        if let Ok(hostname) = nix::unistd::gethostname() {
            if let Ok(hostname_str) = hostname.into_string() {
                tags.push(format!("host:{}", hostname_str));
            }
        }

        // Add default environment
        if let Some(ref env) = config.env {
            tags.push(format!("default_env:{}", env));
        }

        // Add agent version from build metadata
        tags.push(format!("agent_version:{}", env!("AGENT_VERSION")));

        // Add service, version, and config tags ONLY for logs endpoint (not debugger/profiling)
        if include_service_tags {
            if let Some(ref service) = config.service {
                tags.push(format!("default_service:{}", service));
            }

            if let Some(ref version) = config.version {
                tags.push(format!("default_version:{}", version));
            }

            // Add config tags
            for (key, value) in &config.tags {
                tags.push(format!("{}:{}", key, value));
            }
        }

        // Extract and add container tags if container ID is present
        let container_id_provider = super::container::ContainerIDProvider::new();
        let container_tags_provider = super::container::ContainerTagsProvider::new();

        if let Some(container_id) = container_id_provider.get_container_id(&parts.headers) {
            if let Some(container_tags_str) = container_tags_provider.format_tags(&container_id) {
                tags.push(container_tags_str);
            }
        }

        // Extract and add custom tags from X-Datadog-Additional-Tags header
        if let Some(additional_tags) = parts.headers.get("X-Datadog-Additional-Tags") {
            if let Ok(tags_str) = additional_tags.to_str() {
                tags.push(tags_str.to_string());
            }
        }

        // Parse query string once to:
        // 1. Extract existing ddtags for merging with agent tags
        // 2. Preserve all other query parameters
        // This matches Go behavior which preserves all query params
        let mut query_params = HashMap::new();
        if let Some(query) = parts.uri.query() {
            for param in query.split('&') {
                if let Some((key, value)) = param.split_once('=') {
                    let decoded_value = Self::url_decode(value);
                    if key == "ddtags" {
                        // Merge client-provided ddtags with agent tags
                        tags.push(decoded_value.clone());
                        debug!(
                            "TRACE_AGENT | handle_proxy | Merging existing ddtags from request: {}",
                            decoded_value
                        );
                    } else {
                        // Preserve all other query parameters
                        query_params.insert(key.to_string(), decoded_value);
                    }
                }
            }
        }

        // Build final ddtags string
        let mut ddtags = tags.join(",");

        // Truncate to max length if necessary
        if ddtags.len() > DD_TAGS_QUERY_STRING_MAX_LEN {
            warn!(
                "TRACE_AGENT | handle_proxy | Truncating tags in upload to {}. Got {} bytes, max is {}",
                context,
                ddtags.len(),
                DD_TAGS_QUERY_STRING_MAX_LEN
            );
            ddtags.truncate(DD_TAGS_QUERY_STRING_MAX_LEN);
        }

        // Set/update ddtags with our merged tags
        query_params.insert("ddtags".to_string(), ddtags);

        // Construct target URL - handle mock/test backends differently
        let target_url =
            if config.site.starts_with("127.0.0.1") || config.site.starts_with("localhost") {
                // For local testing, use HTTP and skip backend_domain prefix
                format!("http://{}{}", config.site, backend_path)
            } else {
                // Production: use HTTPS with backend subdomain
                format!("https://{}.{}{}", backend_domain, config.site, backend_path)
            };
        let proxy_request = ProxyRequest {
            headers: parts.headers,
            body,
            target_url,
            query_params,
        };

        let mut proxy_aggregator = proxy_aggregator.lock().await;
        proxy_aggregator.add(proxy_request);

        (
            StatusCode::OK,
            format!("Acknowledged request for {context}"),
        )
            .into_response()
    }

    /// Handle proxy requests that use X-Datadog-Additional-Tags HEADER instead of ddtags query parameter
    /// This is used by SymDB endpoint which differs from other endpoints.
    #[allow(clippy::too_many_arguments)]
    async fn handle_proxy_with_header_tags(
        config: Arc<config::Config>,
        proxy_aggregator: Arc<Mutex<proxy_aggregator::Aggregator>>,
        request: Request,
        backend_domain: &str,
        backend_path: &str,
        context: &str,
        evp_origin: &str,
    ) -> Response {
        debug!("TRACE_AGENT | Proxied request for {context} (header-based tags)");
        let (mut parts, body) = match extract_request_body(request).await {
            Ok(r) => r,
            Err(e) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("TRACE_AGENT | handle_proxy_with_header_tags | Error extracting request body: {e}"),
                );
            }
        };

        // Add DD-REQUEST-ID header (UUID v4)
        let request_id = Uuid::new_v4().to_string();
        parts
            .headers
            .insert("DD-REQUEST-ID", request_id.parse().unwrap());

        // Add DD-EVP-ORIGIN header
        parts
            .headers
            .insert("DD-EVP-ORIGIN", evp_origin.parse().unwrap());

        // Build tags for X-Datadog-Additional-Tags header
        let mut tags = Vec::new();

        // Get hostname
        if let Ok(hostname) = nix::unistd::gethostname() {
            if let Ok(hostname_str) = hostname.into_string() {
                tags.push(format!("host:{}", hostname_str));
            }
        }

        // Add default environment
        if let Some(ref env) = config.env {
            tags.push(format!("default_env:{}", env));
        }

        // Add agent version
        tags.push(format!("agent_version:{}", env!("AGENT_VERSION")));

        // Extract and add container tags if container ID is present
        let container_id_provider = super::container::ContainerIDProvider::new();
        let container_tags_provider = super::container::ContainerTagsProvider::new();

        if let Some(container_id) = container_id_provider.get_container_id(&parts.headers) {
            if let Some(container_tags_str) = container_tags_provider.format_tags(&container_id) {
                tags.push(container_tags_str);
            }
        }

        // Merge existing X-Datadog-Additional-Tags header if present
        if let Some(additional_tags) = parts.headers.get("X-Datadog-Additional-Tags") {
            if let Ok(tags_str) = additional_tags.to_str() {
                tags.push(tags_str.to_string());
                debug!("TRACE_AGENT | handle_proxy_with_header_tags | Merging existing X-Datadog-Additional-Tags: {}", tags_str);
            }
        }

        // Merge ddtags from query parameters if present (Go does this for SymDB)
        if let Some(query) = parts.uri.query() {
            for param in query.split('&') {
                if let Some((key, value)) = param.split_once('=') {
                    if key == "ddtags" {
                        let decoded_value = Self::url_decode(value);
                        tags.push(decoded_value.clone());
                        debug!("TRACE_AGENT | handle_proxy_with_header_tags | Merging ddtags from query: {}", decoded_value);
                    }
                }
            }
        }

        // Build final tags string
        let final_tags = tags.join(",");

        // Set X-Datadog-Additional-Tags header with aggregated tags
        parts
            .headers
            .insert("X-Datadog-Additional-Tags", final_tags.parse().unwrap());

        debug!("TRACE_AGENT | handle_proxy_with_header_tags | Setting X-Datadog-Additional-Tags={} for {}", final_tags, context);

        // Preserve query parameters as-is (don't modify them)
        let mut query_params = HashMap::new();
        if let Some(query) = parts.uri.query() {
            for param in query.split('&') {
                if let Some((key, value)) = param.split_once('=') {
                    query_params.insert(key.to_string(), Self::url_decode(value));
                }
            }
        }

        // Construct target URL
        let target_url =
            if config.site.starts_with("127.0.0.1") || config.site.starts_with("localhost") {
                format!("http://{}{}", config.site, backend_path)
            } else {
                format!("https://{}.{}{}", backend_domain, config.site, backend_path)
            };

        let proxy_request = ProxyRequest {
            headers: parts.headers,
            body,
            target_url,
            query_params,
        };

        let mut proxy_aggregator = proxy_aggregator.lock().await;
        proxy_aggregator.add(proxy_request);

        (
            StatusCode::OK,
            format!("Acknowledged request for {context}"),
        )
            .into_response()
    }

    /// Simple URL decoder for query parameter values
    /// Handles %XX hex encoding and + to space conversion
    fn url_decode(s: &str) -> String {
        let mut result = String::with_capacity(s.len());
        let mut chars = s.chars();

        while let Some(ch) = chars.next() {
            match ch {
                '%' => {
                    // Try to read two hex digits
                    let hex: String = chars.by_ref().take(2).collect();
                    if hex.len() == 2 {
                        if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                            result.push(byte as char);
                        } else {
                            // Invalid hex, keep the % and continue
                            result.push('%');
                            result.push_str(&hex);
                        }
                    } else {
                        // Not enough characters, keep what we have
                        result.push('%');
                        result.push_str(&hex);
                    }
                }
                '+' => result.push(' '),
                _ => result.push(ch),
            }
        }

        result
    }

    /// Copies only the headers allowed by the EVP intake.
    fn collect_allowed_evp_headers(source: &HeaderMap) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for name in EVP_PROXY_ALLOWED_HEADERS {
            if let Some(value) = source.get(name) {
                headers.insert(HeaderName::from_static(name), value.clone());
            }
        }
        headers
    }

    /// Returns true when the header exists with a case-insensitive value of `true`.
    fn header_truthy(headers: &HeaderMap, name: &str) -> bool {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|value| value.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    }

    /// Removes the `/evp_proxy/v{version}` prefix and returns the backend path.
    fn extract_evp_target_path(full_path: &str, version: u8) -> Option<String> {
        let prefix = format!("/evp_proxy/v{version}");
        let remainder = full_path.strip_prefix(&prefix)?;
        if remainder.is_empty() {
            return Some("/".to_string());
        }
        if remainder.starts_with('/') {
            return Some(remainder.to_string());
        }
        None
    }

    /// Removes any scheme prefix from the configured site.
    fn sanitize_site(site: &str) -> &str {
        let trimmed = site
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        if trimmed.is_empty() {
            site
        } else {
            trimmed
        }
    }

    /// Returns true when the site points to a localhost override.
    fn is_local_destination(site: &str) -> bool {
        let sanitized = Self::sanitize_site(site);
        sanitized.starts_with("127.") || sanitized.starts_with("localhost")
    }

    /// Constructs the upstream URL, switching to HTTP when targeting localhost.
    fn build_evp_target_url(
        site: &str,
        subdomain: &str,
        path: &str,
        query: Option<&str>,
    ) -> String {
        let mut url = if Self::is_local_destination(site) {
            format!("http://{}{}", Self::sanitize_site(site), path)
        } else {
            format!(
                "https://{}.{}{}",
                subdomain,
                Self::sanitize_site(site),
                path
            )
        };

        if let Some(query) = query {
            if !query.is_empty() {
                url.push('?');
                url.push_str(query);
            }
        }

        url
    }

    /// Ensures subdomains contain only permitted characters.
    fn is_valid_evp_subdomain(value: &str) -> bool {
        !value.is_empty()
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || EVP_PROXY_SUBDOMAIN_ALLOWED_CHARS.contains(c))
    }

    /// Ensures backend paths contain only permitted characters.
    fn is_valid_evp_path(value: &str) -> bool {
        value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || EVP_PROXY_PATH_ALLOWED_CHARS.contains(c))
    }

    /// Percent-decodes a query component for validation.
    fn decode_query_component(raw: &str) -> Option<String> {
        let mut result = String::with_capacity(raw.len());
        let mut chars = raw.chars();
        while let Some(ch) = chars.next() {
            match ch {
                '%' => {
                    let hex: String = chars.by_ref().take(2).collect();
                    if hex.len() == 2 {
                        if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                            result.push(byte as char);
                        } else {
                            return None;
                        }
                    } else {
                        return None;
                    }
                }
                '+' => result.push(' '),
                other => result.push(other),
            }
        }
        Some(result)
    }

    /// Ensures the query string only contains permitted characters.
    fn is_valid_evp_query(value: &str) -> bool {
        if value.is_empty() {
            return true;
        }
        match Self::decode_query_component(value) {
            Some(decoded) => decoded
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || EVP_PROXY_QUERY_ALLOWED_CHARS.contains(c)),
            None => false,
        }
    }

    /// Builds the Datadog serverless flare URL based on the configured site.
    fn build_tracer_flare_url(site: &str, agent_version: &str) -> Option<String> {
        let sanitized = Self::sanitize_site(site);
        if sanitized.is_empty() {
            return None;
        }

        if Self::is_local_destination(site) {
            return Some(format!(
                "http://{}{}",
                sanitized, SERVERLESS_FLARE_ENDPOINT_PATH
            ));
        }

        let mut host = sanitized.to_string();
        if DD_NO_SUBDOMAIN_REGEX.is_match(host.as_str()) {
            host = format!("app.{host}");
        }

        if DD_URL_REGEX.is_match(host.as_str()) {
            if let Some(caps) = VERSION_NUMBER_REGEX.captures(agent_version) {
                if let Some((_, rest)) = host.split_once('.') {
                    let new_subdomain = format!("{}-{}-{}-flare", &caps[1], &caps[2], &caps[3]);
                    host = format!("{new_subdomain}.{rest}");
                }
            }
        }

        Some(format!(
            "https://{}{}",
            host, SERVERLESS_FLARE_ENDPOINT_PATH
        ))
    }

    #[must_use]
    pub fn get_sender_copy(&self) -> Sender<SendDataBuilderInfo> {
        self.tx.clone()
    }

    #[must_use]
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown_token.clone()
    }
}

fn error_response<E: std::fmt::Display>(status: StatusCode, error: E) -> Response {
    error!("{}", error);
    (status, error.to_string()).into_response()
}

fn success_response(message: &str) -> Response {
    debug!("{}", message);
    (StatusCode::OK, json!({"rate_by_service": {}}).to_string()).into_response()
}

lazy_static! {
    static ref DD_NO_SUBDOMAIN_REGEX: Regex =
        Regex::new(r"^(datad(oghq|0g)\.(com|eu)|ddog-gov\.com)$").unwrap();
    static ref DD_URL_REGEX: Regex =
        Regex::new(r"^app(\.[a-z]{2}\d)?\.(datad(oghq|0g)\.(com|eu)|ddog-gov\.com)$").unwrap();
    static ref VERSION_NUMBER_REGEX: Regex = Regex::new(r"(\d+)\.(\d+)\.(\d+)").unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::operational_mode::OperationalMode;
    use crate::traces::{
        proxy_aggregator,
        stats_concentrator_service::{ConcentratorCommand, StatsConcentratorHandle},
        trace_aggregator::TraceAggregator,
        trace_processor::TraceProcessor,
    };
    use axum::body::to_bytes;
    use datadog_trace_utils::tracer_header_tags;
    use datadog_trace_utils::tracer_payload::TracerPayloadCollection;
    use httpmock::MockServer;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    // Helper function to create a minimal test configuration
    fn create_test_config() -> Arc<config::Config> {
        Arc::new(config::Config {
            api_key: "test_api_key".to_string(),
            site: "datadoghq.com".to_string(),
            apm_dd_url: "https://trace.agent.datadoghq.com".to_string(),
            env: Some("test".to_string()),
            service: Some("test-service".to_string()),
            version: Some("1.0.0".to_string()),
            tags: HashMap::new(),
            operational_mode: OperationalMode::HttpFixedPort,
            trace_agent_port: Some(8126),
            ..Default::default()
        })
    }

    fn build_proxy_state_with_site(site: &str) -> ProxyState {
        let mut cfg = config::Config::default();
        cfg.api_key = "test_api_key".to_string();
        cfg.site = site.to_string();
        cfg.http_protocol = Some("http1".to_string());
        ProxyState {
            config: Arc::new(cfg),
            proxy_aggregator: Arc::new(Mutex::new(proxy_aggregator::Aggregator::default())),
        }
    }

    fn build_evp_request(path: &str) -> Request {
        Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .header("x-datadog-evp-subdomain", "trace.agent")
            .body(Body::from(r#"{"payload":true}"#))
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn evp_proxy_streams_upstream_response() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method("POST")
                .path("/api/v2/sample")
                .query_param("foo", "bar")
                .header("dd-api-key", "test_api_key");
            then.status(202)
                .header("content-type", "application/json")
                .body(r#"{"ok":true}"#);
        });

        let state = build_proxy_state_with_site(&server.address().to_string());
        let request = build_evp_request("/evp_proxy/v4/api/v2/sample?foo=bar");

        let response = TraceAgent::handle_evp_proxy(state, request, 4).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let (parts, body) = response.into_parts();
        let body_bytes = to_bytes(body, usize::MAX).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&body_bytes), r#"{"ok":true}"#);
        assert_eq!(
            parts
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        mock.assert();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn evp_proxy_returns_bad_gateway_on_error() {
        let state = build_proxy_state_with_site("127.0.0.1:9");
        let request = build_evp_request("/evp_proxy/v4/api/v2/sample");
        let response = TraceAgent::handle_evp_proxy(state, request, 4).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tracer_flare_proxy_forwards_requests() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method("POST")
                .path(SERVERLESS_FLARE_ENDPOINT_PATH)
                .header("dd-api-key", "test_api_key")
                .body(r#"{"payload":true}"#);
            then.status(202).body("flare-ok");
        });

        let site = format!("127.0.0.1:{}", server.port());
        let state = build_proxy_state_with_site(&site);
        let request = Request::builder()
            .method("POST")
            .uri(TRACER_FLARE_ENDPOINT_PATH)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"payload":true}"#))
            .unwrap();

        let response = TraceAgent::tracer_flare_proxy(axum::extract::State(state), request).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body, "flare-ok");
        mock.assert();
    }

    #[test]
    fn tracer_flare_url_rewrites_datadog_host() {
        let url = TraceAgent::build_tracer_flare_url("datad0g.com", "7.70.0").unwrap();
        assert!(url.starts_with("https://7-70-0-flare."));
        assert!(url.ends_with(SERVERLESS_FLARE_ENDPOINT_PATH));
    }

    // Mock TraceProcessor for testing
    struct MockTraceProcessor;

    impl TraceProcessor for MockTraceProcessor {
        fn process_traces(
            &self,
            config: Arc<config::Config>,
            _tags_provider: Arc<provider::Provider>,
            header_tags: tracer_header_tags::TracerHeaderTags<'_>,
            _traces: Vec<Vec<pb::Span>>,
            body_size: usize,
            _span_pointers: Option<Vec<crate::traces::span_pointers::SpanPointer>>,
        ) -> (SendDataBuilderInfo, TracerPayloadCollection) {
            use datadog_trace_utils::send_data::{Compression, SendDataBuilder};
            use datadog_trace_utils::send_with_retry::{RetryBackoffType, RetryStrategy};
            use ddcommon::Endpoint;
            use std::str::FromStr;

            // Create a minimal valid SendDataBuilder for testing
            let payload = TracerPayloadCollection::V07(vec![]);
            let endpoint = Endpoint {
                url: hyper::Uri::from_str(&config.apm_dd_url).expect("valid URL"),
                api_key: None,
                timeout_ms: config.flush_timeout * 1000,
                test_token: None,
            };

            let builder = SendDataBuilder::new(body_size, payload.clone(), header_tags, &endpoint)
                .with_compression(Compression::Zstd(config.apm_config_compression_level))
                .with_retry_strategy(RetryStrategy::new(
                    1,
                    100,
                    RetryBackoffType::Exponential,
                    None,
                ));

            (SendDataBuilderInfo::new(builder, body_size), payload)
        }
    }

    // Mock StatsProcessor for testing
    struct MockStatsProcessor;

    #[async_trait::async_trait]
    impl stats_processor::StatsProcessor for MockStatsProcessor {
        async fn process_stats(
            &self,
            _request: Request,
            _stats_tx: Sender<pb::ClientStatsPayload>,
        ) -> Result<Response, Box<dyn std::error::Error + Send + Sync>> {
            Ok((StatusCode::OK, "OK".to_string()).into_response())
        }
    }

    // Helper function to create a test StatsConcentratorHandle
    fn create_test_stats_concentrator() -> StatsConcentratorHandle {
        let (tx, _rx) = mpsc::channel::<ConcentratorCommand>(1000);
        StatsConcentratorHandle::new(tx)
    }

    // Helper function to create a test TraceAgent
    fn create_test_trace_agent() -> TraceAgent {
        let config = create_test_config();
        let trace_aggregator = Arc::new(Mutex::new(TraceAggregator::default()));
        let trace_processor: Arc<dyn TraceProcessor + Send + Sync> = Arc::new(MockTraceProcessor);
        let stats_concentrator = create_test_stats_concentrator();
        let stats_aggregator = Arc::new(Mutex::new(
            stats_aggregator::StatsAggregator::new_with_concentrator(stats_concentrator.clone()),
        ));
        let stats_processor: Arc<dyn stats_processor::StatsProcessor + Send + Sync> =
            Arc::new(MockStatsProcessor);
        let proxy_aggregator = Arc::new(Mutex::new(proxy_aggregator::Aggregator::default()));
        let tags_provider = Arc::new(provider::Provider::new(config.clone()));

        TraceAgent::new(
            config,
            trace_aggregator,
            trace_processor,
            stats_aggregator,
            stats_processor,
            proxy_aggregator,
            #[cfg(feature = "appsec")]
            None,
            tags_provider,
            stats_concentrator,
            None,
            CancellationToken::new(),
        )
    }

    #[test]
    fn test_api_version_enum() {
        // Test that ApiVersion variants can be created and are Copy
        let v04 = ApiVersion::V04;
        let v05 = ApiVersion::V05;

        // Test Copy trait
        let v04_copy = v04;
        let v05_copy = v05;

        // Both original and copy should be usable (validates Copy trait)
        match v04 {
            ApiVersion::V04 => assert!(true),
            ApiVersion::V05 => panic!("Expected V04"),
        }
        match v04_copy {
            ApiVersion::V04 => assert!(true),
            ApiVersion::V05 => panic!("Expected V04"),
        }

        match v05 {
            ApiVersion::V05 => assert!(true),
            ApiVersion::V04 => panic!("Expected V05"),
        }
        match v05_copy {
            ApiVersion::V05 => assert!(true),
            ApiVersion::V04 => panic!("Expected V05"),
        }
    }

    #[test]
    fn test_error_response() {
        let response = error_response(StatusCode::BAD_REQUEST, "Test error message");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_error_response_with_different_status_codes() {
        let test_cases = vec![
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error"),
            (StatusCode::NOT_FOUND, "Not found"),
            (StatusCode::UNAUTHORIZED, "Unauthorized"),
            (StatusCode::PAYLOAD_TOO_LARGE, "Too large"),
        ];

        for (status, message) in test_cases {
            let response = error_response(status, message);
            assert_eq!(response.status(), status);
        }
    }

    #[test]
    fn test_success_response() {
        let response = success_response("Test success message");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn test_success_response_format() {
        let response = success_response("Test message");
        assert_eq!(response.status(), StatusCode::OK);
        // Success response should contain rate_by_service JSON
    }

    #[test]
    fn test_trace_state_creation() {
        let config = create_test_config();
        let trace_processor: Arc<dyn TraceProcessor + Send + Sync> = Arc::new(MockTraceProcessor);
        let (tx, _rx): (Sender<SendDataBuilderInfo>, Receiver<SendDataBuilderInfo>) =
            mpsc::channel(10);
        let stats_concentrator = create_test_stats_concentrator();
        let stats_generator = Arc::new(StatsGenerator::new(stats_concentrator));

        let trace_sender = Arc::new(SendingTraceProcessor {
            #[cfg(feature = "appsec")]
            appsec: None,
            processor: trace_processor,
            trace_tx: tx,
            stats_generator,
        });

        let tags_provider = Arc::new(provider::Provider::new(config.clone()));

        let state = TraceState {
            config: config.clone(),
            trace_sender,
            tags_provider,
        };

        assert!(Arc::ptr_eq(&state.config, &config));
    }

    #[test]
    fn test_trace_state_clone() {
        let config = create_test_config();
        let trace_processor: Arc<dyn TraceProcessor + Send + Sync> = Arc::new(MockTraceProcessor);
        let (tx, _rx): (Sender<SendDataBuilderInfo>, Receiver<SendDataBuilderInfo>) =
            mpsc::channel(10);
        let stats_concentrator = create_test_stats_concentrator();
        let stats_generator = Arc::new(StatsGenerator::new(stats_concentrator));

        let trace_sender = Arc::new(SendingTraceProcessor {
            #[cfg(feature = "appsec")]
            appsec: None,
            processor: trace_processor,
            trace_tx: tx,
            stats_generator,
        });

        let tags_provider = Arc::new(provider::Provider::new(config.clone()));

        let state = TraceState {
            config,
            trace_sender,
            tags_provider,
        };

        let cloned_state = state.clone();
        assert!(Arc::ptr_eq(&state.config, &cloned_state.config));
    }

    #[test]
    fn test_stats_state_creation() {
        let stats_processor: Arc<dyn stats_processor::StatsProcessor + Send + Sync> =
            Arc::new(MockStatsProcessor);
        let (stats_tx, _stats_rx): (
            Sender<pb::ClientStatsPayload>,
            Receiver<pb::ClientStatsPayload>,
        ) = mpsc::channel(10);

        let state = StatsState {
            stats_processor: stats_processor.clone(),
            stats_tx,
        };

        assert!(Arc::ptr_eq(&state.stats_processor, &stats_processor));
    }

    #[test]
    fn test_stats_state_clone() {
        let stats_processor: Arc<dyn stats_processor::StatsProcessor + Send + Sync> =
            Arc::new(MockStatsProcessor);
        let (stats_tx, _stats_rx): (
            Sender<pb::ClientStatsPayload>,
            Receiver<pb::ClientStatsPayload>,
        ) = mpsc::channel(10);

        let state = StatsState {
            stats_processor: stats_processor.clone(),
            stats_tx,
        };

        let cloned_state = state.clone();
        assert!(Arc::ptr_eq(
            &state.stats_processor,
            &cloned_state.stats_processor
        ));
    }

    #[test]
    fn test_proxy_state_creation() {
        let config = create_test_config();
        let proxy_aggregator = Arc::new(Mutex::new(proxy_aggregator::Aggregator::default()));

        let state = ProxyState {
            config: config.clone(),
            proxy_aggregator: proxy_aggregator.clone(),
        };

        assert!(Arc::ptr_eq(&state.config, &config));
        assert!(Arc::ptr_eq(&state.proxy_aggregator, &proxy_aggregator));
    }

    #[test]
    fn test_proxy_state_clone() {
        let config = create_test_config();
        let proxy_aggregator = Arc::new(Mutex::new(proxy_aggregator::Aggregator::default()));

        let state = ProxyState {
            config,
            proxy_aggregator,
        };

        let cloned_state = state.clone();
        assert!(Arc::ptr_eq(&state.config, &cloned_state.config));
        assert!(Arc::ptr_eq(
            &state.proxy_aggregator,
            &cloned_state.proxy_aggregator
        ));
    }

    #[tokio::test]
    async fn test_trace_agent_new() {
        let config = create_test_config();
        let trace_aggregator = Arc::new(Mutex::new(TraceAggregator::default()));
        let trace_processor: Arc<dyn TraceProcessor + Send + Sync> = Arc::new(MockTraceProcessor);
        let stats_concentrator = create_test_stats_concentrator();
        let stats_aggregator = Arc::new(Mutex::new(
            stats_aggregator::StatsAggregator::new_with_concentrator(stats_concentrator.clone()),
        ));
        let stats_processor: Arc<dyn stats_processor::StatsProcessor + Send + Sync> =
            Arc::new(MockStatsProcessor);
        let proxy_aggregator = Arc::new(Mutex::new(proxy_aggregator::Aggregator::default()));
        let tags_provider = Arc::new(provider::Provider::new(config.clone()));

        let trace_agent = TraceAgent::new(
            config.clone(),
            trace_aggregator,
            trace_processor,
            stats_aggregator,
            stats_processor,
            proxy_aggregator,
            #[cfg(feature = "appsec")]
            None,
            tags_provider,
            stats_concentrator,
            None,
            CancellationToken::new(),
        );

        assert!(Arc::ptr_eq(&trace_agent.config, &config));
    }

    #[tokio::test]
    async fn test_trace_agent_get_bound_port_initially_none() {
        let trace_agent = create_test_trace_agent();
        let port = trace_agent.get_bound_port().await;
        assert_eq!(port, None);
    }

    #[tokio::test]
    async fn test_trace_agent_get_bound_port_after_set() {
        let trace_agent = create_test_trace_agent();

        // Simulate setting the listener info (as would happen in start())
        {
            let mut listener_info = trace_agent.listener_info.lock().await;
            *listener_info = Some(ListenerInfo::new(Some(8126), None));
        }

        let port = trace_agent.get_bound_port().await;
        assert_eq!(port, Some(8126));
    }

    #[tokio::test]
    async fn test_trace_agent_get_bound_port_different_values() {
        let trace_agent = create_test_trace_agent();

        // Test with different port values
        for test_port in [8126, 8080, 3000, 0] {
            let mut listener_info = trace_agent.listener_info.lock().await;
            *listener_info = Some(ListenerInfo::new(Some(test_port), None));
            drop(listener_info);

            let port = trace_agent.get_bound_port().await;
            assert_eq!(port, Some(test_port));
        }
    }

    #[tokio::test]
    async fn test_trace_agent_get_sender_copy() {
        let trace_agent = create_test_trace_agent();
        let sender1 = trace_agent.get_sender_copy();
        let sender2 = trace_agent.get_sender_copy();

        // Both senders should be usable
        assert!(!sender1.is_closed());
        assert!(!sender2.is_closed());
    }

    #[tokio::test]
    async fn test_trace_agent_get_sender_copy_can_send() {
        use datadog_trace_utils::send_data::{Compression, SendDataBuilder};
        use datadog_trace_utils::send_with_retry::{RetryBackoffType, RetryStrategy};
        use datadog_trace_utils::tracer_header_tags::TracerHeaderTags;
        use datadog_trace_utils::tracer_payload::TracerPayloadCollection;
        use ddcommon::Endpoint;
        use std::str::FromStr;

        let trace_agent = create_test_trace_agent();
        let sender = trace_agent.get_sender_copy();
        let config = create_test_config();

        // Create a minimal SendDataBuilderInfo for testing
        let payload = TracerPayloadCollection::V07(vec![]);
        let header_tags = TracerHeaderTags::default();
        let endpoint = Endpoint {
            url: hyper::Uri::from_str(&config.apm_dd_url).expect("valid URL"),
            api_key: None,
            timeout_ms: config.flush_timeout * 1000,
            test_token: None,
        };

        let builder = SendDataBuilder::new(100, payload, header_tags, &endpoint)
            .with_compression(Compression::Zstd(config.apm_config_compression_level))
            .with_retry_strategy(RetryStrategy::new(
                1,
                100,
                RetryBackoffType::Exponential,
                None,
            ));

        let test_data = SendDataBuilderInfo::new(builder, 100);

        // Should be able to send without error
        let result = sender.send(test_data).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_trace_agent_shutdown_token() {
        let trace_agent = create_test_trace_agent();
        let token = trace_agent.shutdown_token();

        // Token should not be cancelled initially
        assert!(!token.is_cancelled());
    }

    #[tokio::test]
    async fn test_trace_agent_shutdown_token_can_cancel() {
        let trace_agent = create_test_trace_agent();
        let token = trace_agent.shutdown_token();

        assert!(!token.is_cancelled());

        // Cancel the token
        token.cancel();

        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn test_trace_agent_shutdown_token_multiple_copies() {
        let trace_agent = create_test_trace_agent();
        let token1 = trace_agent.shutdown_token();
        let token2 = trace_agent.shutdown_token();

        assert!(!token1.is_cancelled());
        assert!(!token2.is_cancelled());

        // Cancelling one should cancel all
        token1.cancel();

        assert!(token1.is_cancelled());
        assert!(token2.is_cancelled());
    }

    #[test]
    fn test_remote_config_state_clone() {
        // RemoteConfigState is Clone, so let's verify the struct compiles and has Clone trait
        // We can't easily instantiate RemoteConfigService without extensive mocking,
        // but we can validate the type structure
        fn _test_remote_config_clone_trait<T: Clone>() {}
        _test_remote_config_clone_trait::<RemoteConfigState>();
    }

    #[test]
    fn test_constants() {
        // Test that constants have expected values
        assert_eq!(TRACE_AGENT_PORT, 8126);
        assert_eq!(V4_TRACE_ENDPOINT_PATH, "/v0.4/traces");
        assert_eq!(V5_TRACE_ENDPOINT_PATH, "/v0.5/traces");
        assert_eq!(STATS_ENDPOINT_PATH, "/v0.6/stats");
        assert_eq!(REMOTE_CONFIG_ENDPOINT_PATH, "/v0.7/config");
        assert_eq!(DSM_AGENT_PATH, "/v0.1/pipeline_stats");
        assert_eq!(PROFILING_ENDPOINT_PATH, "/profiling/v1/input");
        assert_eq!(INFO_ENDPOINT_PATH, "/info");
        assert_eq!(DEBUGGER_ENDPOINT_PATH, "/debugger/v1/input");
        assert_eq!(LOGS_ENDPOINT_PATH, "/v1/input");
        assert_eq!(
            INSTRUMENTATION_ENDPOINT_PATH,
            "/telemetry/proxy/api/v2/apmtelemetry"
        );
    }

    #[test]
    fn test_intake_paths_constants() {
        assert_eq!(DSM_INTAKE_PATH, "/api/v0.1/pipeline_stats");
        assert_eq!(LLM_OBS_SPANS_INTAKE_PATH, "/api/v2/llmobs");
        assert_eq!(
            LLM_OBS_EVAL_METRIC_INTAKE_PATH,
            "/api/intake/llm-obs/v1/eval-metric"
        );
        assert_eq!(
            LLM_OBS_EVAL_METRIC_INTAKE_PATH_V2,
            "/api/intake/llm-obs/v2/eval-metric"
        );
        assert_eq!(PROFILING_INTAKE_PATH, "/api/v2/profile");
        assert_eq!(DEBUGGER_LOGS_INTAKE_PATH, "/api/v2/logs");
        assert_eq!(INSTRUMENTATION_INTAKE_PATH, "/api/v2/apmtelemetry");
    }

    #[test]
    fn test_buffer_size_constants() {
        assert_eq!(TRACER_PAYLOAD_CHANNEL_BUFFER_SIZE, 10);
        assert_eq!(STATS_PAYLOAD_CHANNEL_BUFFER_SIZE, 10);
    }

    #[test]
    fn test_size_limit_constants() {
        assert_eq!(TRACE_REQUEST_BODY_LIMIT, 50 * 1024 * 1024);
        assert_eq!(DEFAULT_REQUEST_BODY_LIMIT, 2 * 1024 * 1024);
        assert_eq!(MAX_CONTENT_LENGTH, 50 * 1024 * 1024);
        assert_eq!(DD_TAGS_QUERY_STRING_MAX_LEN, 4001);
    }

    #[tokio::test]
    async fn test_trace_agent_with_remote_config_field() {
        // Test that TraceAgent can be configured with remote_config_service = Some(...)
        // We test this by checking that the field exists and can be set to None/Some conceptually
        let trace_agent = create_test_trace_agent();

        // The test TraceAgent is created with remote_config_service = None
        assert!(trace_agent.remote_config_service.is_none());

        // Note: Creating an actual RemoteConfigService requires complex setup with HTTP clients,
        // Uptane state, etc. The fact that the field compiles and accepts Option<Arc<RemoteConfigService>>
        // validates the type structure. Integration tests cover the full remote config flow.
    }

    #[tokio::test]
    async fn test_trace_agent_without_remote_config() {
        let trace_agent = create_test_trace_agent();
        assert!(trace_agent.remote_config_service.is_none());
    }

    #[cfg(feature = "appsec")]
    #[tokio::test]
    async fn test_trace_agent_without_appsec() {
        let trace_agent = create_test_trace_agent();
        assert!(trace_agent.appsec_processor.is_none());
    }

    // Tests for URL decoding functionality
    #[test]
    fn test_url_decode_basic() {
        assert_eq!(TraceAgent::url_decode("hello"), "hello");
        assert_eq!(TraceAgent::url_decode("hello+world"), "hello world");
        assert_eq!(TraceAgent::url_decode("hello%20world"), "hello world");
    }

    #[test]
    fn test_url_decode_special_characters() {
        assert_eq!(TraceAgent::url_decode("foo%3Abar"), "foo:bar");
        assert_eq!(TraceAgent::url_decode("test%2Cvalue"), "test,value");
        assert_eq!(TraceAgent::url_decode("key%3Dvalue"), "key=value");
        assert_eq!(TraceAgent::url_decode("a%2Bb%3Dc"), "a+b=c");
    }

    #[test]
    fn test_url_decode_mixed_encoding() {
        assert_eq!(
            TraceAgent::url_decode("host%3Alocalhost%2Cenv%3Aprod"),
            "host:localhost,env:prod"
        );
        assert_eq!(
            TraceAgent::url_decode("hello+world%20test"),
            "hello world test"
        );
    }

    #[test]
    fn test_url_decode_invalid_encoding() {
        // Invalid hex should be left as-is
        assert_eq!(TraceAgent::url_decode("test%GG"), "test%GG");
        assert_eq!(TraceAgent::url_decode("test%2"), "test%2");
        assert_eq!(TraceAgent::url_decode("test%"), "test%");
    }

    #[test]
    fn test_url_decode_empty() {
        assert_eq!(TraceAgent::url_decode(""), "");
    }

    #[test]
    fn test_url_decode_no_encoding() {
        assert_eq!(TraceAgent::url_decode("simplestring"), "simplestring");
        assert_eq!(
            TraceAgent::url_decode("no-encoding-here"),
            "no-encoding-here"
        );
    }

    #[test]
    fn test_url_decode_consecutive_encoded() {
        assert_eq!(TraceAgent::url_decode("%20%20"), "  ");
        assert_eq!(TraceAgent::url_decode("%2C%2C%2C"), ",,,");
    }

    #[test]
    fn test_url_decode_ddtags_example() {
        // Real-world example: ddtags from a client
        let encoded = "client%3Aweb%2Cversion%3A1.2.3%2Cregion%3Aus-east-1";
        let decoded = TraceAgent::url_decode(encoded);
        assert_eq!(decoded, "client:web,version:1.2.3,region:us-east-1");
    }
}
