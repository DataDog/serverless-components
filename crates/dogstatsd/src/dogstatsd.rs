// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! DogStatsD server implementation for receiving and processing metrics.
//!
//! This module implements a DogStatsD-compatible server that receives metric data from multiple
//! transport mechanisms (UDP sockets, Windows named pipes), parses the metrics, applies optional
//! namespacing, and forwards them to an aggregator for batching and shipping to Datadog.

use std::net::SocketAddr;
use std::str::Split;

use crate::aggregator_service::AggregatorHandle;
use crate::errors::ParseError::UnsupportedType;
use crate::metric::{id, parse, Metric};
use tracing::{debug, error, trace};

// Windows-specific imports
#[cfg(windows)]
use {
    std::sync::Arc,
    tokio::io::AsyncReadExt,
    tokio::net::windows::named_pipe::ServerOptions,
    tokio::time::{sleep, Duration},
};

// DogStatsD buffer size for receiving metrics
// TODO(astuyve) buf should be dynamic
// Max buffer size is configurable in Go Agent with a default of 8KB
// https://github.com/DataDog/datadog-agent/blob/85939a62b5580b2a15549f6936f257e61c5aa153/pkg/config/config_template.yaml#L2154-L2158
const BUFFER_SIZE: usize = 8192;

// Maximum number of consecutive errors before giving up on named pipe operations.
// Backoff formula: 10ms * 2^error_count, capped at MAX to prevent overflow
// With MAX = 5: backoffs are 20ms, 40ms, 80ms, 160ms, 320ms (total: ~600ms before giving up)
#[cfg(windows)]
const MAX_NAMED_PIPE_ERRORS: u32 = 5;

/// Configuration for the DogStatsD server
pub struct DogStatsDConfig {
    /// Host to bind UDP socket to (e.g., "127.0.0.1")
    pub host: String,
    /// Port to bind UDP socket to (e.g., 8125), will be 0 if we're using a Named Pipe
    pub port: u16,
    /// Optional namespace to prepend to all metric names (e.g., "myapp")
    pub metric_namespace: Option<String>,
    /// Optional Windows named pipe path (e.g., "\\\\.\\pipe\\my_pipe")
    pub windows_pipe_name: Option<String>,
}

/// Represents the source of a DogStatsD message. Varies by transport method.
#[derive(Debug, Clone)]
pub enum MessageSource {
    /// Message received from a network socket (UDP)
    Network(SocketAddr),
    /// Message received from a Windows named pipe (Arc for efficient cloning)
    #[cfg(windows)]
    NamedPipe(Arc<String>),
}

impl std::fmt::Display for MessageSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(addr) => write!(f, "{}", addr),
            #[cfg(windows)]
            Self::NamedPipe(name) => write!(f, "{}", name),
        }
    }
}

// BufferReader abstracts transport methods for metric data.
enum BufferReader {
    /// UDP socket reader (cross-platform, default transport)
    UdpSocket(tokio::net::UdpSocket),

    /// Mirror reader for testing - replays a fixed buffer
    #[allow(dead_code)]
    MirrorTest(Vec<u8>, SocketAddr),

    /// Windows named pipe reader (Windows-only transport)
    #[cfg(windows)]
    NamedPipe {
        pipe_name: Arc<String>,
        cancel_token: tokio_util::sync::CancellationToken,
    },
}

impl BufferReader {
    /// This is the main entry point for receiving metric data.
    /// Note: Different transports have different blocking behaviors.
    async fn read(&self) -> std::io::Result<(Vec<u8>, MessageSource)> {
        match self {
            BufferReader::UdpSocket(socket) => {
                // UDP socket: blocks until a packet arrives
                let mut buf = [0; BUFFER_SIZE];

                #[allow(clippy::expect_used)]
                let (amt, src) = socket
                    .recv_from(&mut buf)
                    .await
                    .expect("didn't receive data");
                Ok((buf[..amt].to_owned(), MessageSource::Network(src)))
            }
            BufferReader::MirrorTest(data, socket) => {
                // Mirror Reader: returns immediately with stored data
                Ok((data.clone(), MessageSource::Network(*socket)))
            }
            #[cfg(windows)]
            BufferReader::NamedPipe {
                pipe_name,
                cancel_token,
            } => {
                // Named Pipe Reader: retries with backoff due to expected transient errors
                #[allow(clippy::expect_used)]
                let data = read_from_named_pipe(pipe_name, cancel_token)
                    .await
                    .expect("didn't receive data");
                Ok((data, MessageSource::NamedPipe(Arc::clone(pipe_name))))
            }
        }
    }
}

/// Helper to handle retry logic with exponential backoff for named pipe errors.
/// Returns true if should continue retrying, false if should break (cancelled).
#[cfg(windows)]
async fn handle_pipe_error_with_backoff(
    consecutive_errors: &mut u32,
    error_type: &str,
    error: &std::io::Error,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> std::io::Result<bool> {
    *consecutive_errors += 1;

    if *consecutive_errors >= MAX_NAMED_PIPE_ERRORS {
        return Err(std::io::Error::other(format!(
            "Too many consecutive {} errors: {}",
            error_type, error
        )));
    }

    let backoff_ms = 10u64 * (1 << *consecutive_errors);

    // Sleep with cancellation support for clean, fast shutdown
    tokio::select! {
        _ = sleep(Duration::from_millis(backoff_ms)) => Ok(true),
        _ = cancel_token.cancelled() => Ok(false),
    }
}

/// Reads data from a Windows named pipe with retry logic.
///
/// Windows named pipes can experience transient failures (client disconnect, pipe errors).
/// This function uses a retry loop with exponential backoff to handle these failures
/// and has additional logic to allow clean shutdown via cancel_token.
#[cfg(windows)]
async fn read_from_named_pipe(
    pipe_name: &str,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> std::io::Result<Vec<u8>> {
    let mut consecutive_errors = 0;
    let mut current_pipe: Option<tokio::net::windows::named_pipe::NamedPipeServer> = None;
    let mut needs_connection = true; // Track whether we need to wait for a client

    // Let named pipes cancel cleanly when the server is shut down
    while !cancel_token.is_cancelled() {
        // Create pipe if needed (initial startup or after error)
        if current_pipe.is_none() {
            debug!("Creating named pipe: {}", pipe_name);
            match ServerOptions::new().create(pipe_name) {
                Ok(new_pipe) => {
                    consecutive_errors = 0; // Reset on successful pipe creation
                    current_pipe = Some(new_pipe);
                    needs_connection = true;
                }
                Err(e) => {
                    error!("Failed to create named pipe: {}", e);
                    if !handle_pipe_error_with_backoff(
                        &mut consecutive_errors,
                        "pipe creation",
                        &e,
                        cancel_token,
                    )
                    .await?
                    {
                        break;
                    }
                    continue;
                }
            }
        }

        // Wait for client connection if needed (after creation or after disconnect)
        if needs_connection {
            #[allow(clippy::expect_used)]
            let pipe = current_pipe.as_ref().expect("pipe must exist");

            debug!("Waiting for client connection on named pipe");
            let connect_result = tokio::select! {
                result = pipe.connect() => result,
                _ = cancel_token.cancelled() => {
                    break;
                }
            };

            match connect_result {
                Ok(()) => {
                    debug!("Client connected to DogStatsD named pipe");
                    consecutive_errors = 0;
                    needs_connection = false;
                }
                Err(e) => {
                    // Connection failed - recreate pipe on next iteration
                    current_pipe = None;
                    if !handle_pipe_error_with_backoff(
                        &mut consecutive_errors,
                        "connection",
                        &e,
                        cancel_token,
                    )
                    .await?
                    {
                        break;
                    }
                    continue;
                }
            }
        }

        // Read from the connected pipe
        debug!("Starting read from named pipe");
        let mut buf = [0; BUFFER_SIZE];

        #[allow(clippy::expect_used)]
        let pipe = current_pipe
            .as_mut()
            .expect("pipe must exist and be connected");

        let read_result = tokio::select! {
            result = pipe.read(&mut buf) => result,
            _ = cancel_token.cancelled() => {
                return Ok(Vec::new());
            }
        };

        match read_result {
            Ok(0) => {
                // Client disconnected - reuse pipe by disconnecting and waiting for new client
                debug!("Client disconnected, preparing for reconnection");
                if let Err(e) = pipe.disconnect() {
                    error!("Failed to disconnect pipe: {}", e);
                    current_pipe = None; // Recreate on error
                } else {
                    needs_connection = true; // Wait for new client on same pipe
                }
                continue;
            }
            Ok(amt) => {
                // this is the success state: we read some data from the pipe
                debug!("Read {} bytes from named pipe", amt);
                return Ok(buf[..amt].to_vec());
            }
            Err(e) => {
                error!("Error reading from named pipe: {}", e);
                current_pipe = None; // Recreate pipe on read error

                if !handle_pipe_error_with_backoff(
                    &mut consecutive_errors,
                    "read",
                    &e,
                    cancel_token,
                )
                .await?
                {
                    break;
                }
                continue;
            }
        }
    }

    // If we exit due to cancellation, return an empty result for a clean shutdown.
    Ok(Vec::new())
}

/// DogStatsD server to receive, parse, and forward metrics.
pub struct DogStatsD {
    cancel_token: tokio_util::sync::CancellationToken,
    aggregator_handle: AggregatorHandle,
    buffer_reader: BufferReader,
    metric_namespace: Option<String>,
}

impl DogStatsD {
    /// Creates a new DogStatsD server instance.
    ///
    /// The server will bind to either a UDP socket or Windows named pipe based on the config.
    /// Metrics received will be forwarded to the provided aggregator_handle.
    #[must_use]
    pub async fn new(
        config: &DogStatsDConfig,
        aggregator_handle: AggregatorHandle,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> DogStatsD {
        #[allow(unused_variables)] // pipe_name unused on non-Windows
        let buffer_reader = if let Some(ref pipe_name) = config.windows_pipe_name {
            #[cfg(windows)]
            {
                BufferReader::NamedPipe {
                    pipe_name: Arc::new(pipe_name.clone()),
                    cancel_token: cancel_token.clone(),
                }
            }
            #[cfg(not(windows))]
            #[allow(clippy::panic)]
            {
                panic!("Named pipes are only supported on Windows.")
            }
        } else {
            // UDP socket for all platforms
            let addr = format!("{}:{}", config.host, config.port);
            // TODO (UDS socket)
            #[allow(clippy::expect_used)]
            let socket = tokio::net::UdpSocket::bind(addr)
                .await
                .expect("couldn't bind to address");
            BufferReader::UdpSocket(socket)
        };

        DogStatsD {
            cancel_token,
            aggregator_handle,
            buffer_reader,
            metric_namespace: config.metric_namespace.clone(),
        }
    }

    /// Main event loop that continuously receives and processes metrics.
    pub async fn spin(self) {
        let mut spin_cancelled = false;
        while !spin_cancelled {
            self.consume_statsd().await;
            spin_cancelled = self.cancel_token.is_cancelled();
        }
    }

    /// Receive one batch of metrics from the transport layer and process them.
    async fn consume_statsd(&self) {
        #[allow(clippy::expect_used)]
        let (buf, src) = self
            .buffer_reader
            .read()
            .await
            .expect("didn't receive data");

        #[allow(clippy::expect_used)]
        let msgs = std::str::from_utf8(&buf).expect("couldn't parse as string");
        trace!("Received message: {} from {}", msgs, src);
        let statsd_metric_strings = msgs.split('\n');
        self.insert_metrics(statsd_metric_strings);
    }

    /// Parses metrics from raw DogStatsD format and forwards them to the aggregator.
    fn insert_metrics(&self, msg: Split<char>) {
        let namespace = self.metric_namespace.as_deref();
        let all_valid_metrics: Vec<Metric> = msg
            .filter(|m| {
                !m.is_empty()
                    && !m.starts_with("_sc|")
                    && !m.starts_with("_e{")
                    // todo(serverless): remove this hack, and create a blocklist for metrics
                    // or another mechanism for this.
                    //
                    // avoid metric duplication with lambda layer
                    && !m.starts_with("aws.lambda.enhanced.invocations")
            }) // exclude empty messages, service checks, and events
            .map(|m| m.replace('\n', ""))
            .filter_map(|m| match parse(m.as_str()) {
                Ok(metric) => Some(metric),
                Err(e) => {
                    // unsupported type is quite common with dd_trace metrics. Avoid perf issue and
                    // log spam in that case
                    match e {
                        UnsupportedType(_) => debug!("Unsupported metric type: {}. {}", m, e),
                        _ => error!("Failed to parse metric {}: {}", m, e),
                    }
                    None
                }
            })
            .map(|mut metric| {
                if let Some(ns) = namespace {
                    Self::prepend_namespace(ns, &mut metric);
                }
                metric
            })
            .collect();
        if !all_valid_metrics.is_empty() {
            debug!(
                "Inserting {} metrics into aggregator",
                all_valid_metrics.len()
            );
            for metric in &all_valid_metrics {
                debug!("  - {}: {:?}", metric.name, metric.value);
            }
            // Send metrics through the channel - no lock needed!
            if let Err(e) = self.aggregator_handle.insert_batch(all_valid_metrics) {
                error!("Failed to send metrics to aggregator: {}", e);
            }
        }
    }

    /// Prepends a namespace to a metric's name and updates its ID.
    fn prepend_namespace(namespace: &str, metric: &mut Metric) {
        let new_name = format!("{}.{}", namespace, metric.name);
        metric.name = ustr::Ustr::from(&new_name);
        metric.id = id(metric.name, &metric.tags, metric.timestamp);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use crate::aggregator_service::AggregatorService;
    use crate::dogstatsd::{BufferReader, DogStatsD};
    use crate::metric::EMPTY_TAGS;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use tracing_test::traced_test;

    #[tokio::test]
    async fn test_dogstatsd_multi_distribution() {
        let response = setup_and_consume_dogstatsd(
            "single_machine_performance.rouster.api.series_v2.payload_size_bytes:269942|d|T1656581409
single_machine_performance.rouster.metrics_min_timestamp_latency:1426.90870216|d|T1656581409
single_machine_performance.rouster.metrics_max_timestamp_latency:1376.90870216|d|T1656581409
", None
        )
        .await;

        assert_eq!(response.distributions.len(), 1);
        assert_eq!(response.distributions[0].sketches.len(), 3);
        assert_eq!(response.series.len(), 0);
    }

    #[tokio::test]
    async fn test_dogstatsd_multi_metric() {
        let mut now: i64 = std::time::UNIX_EPOCH
            .elapsed()
            .expect("unable to poll clock, unrecoverable")
            .as_secs()
            .try_into()
            .unwrap_or_default();
        now = (now / 10) * 10;

        let response = setup_and_consume_dogstatsd(
            format!(
                "metric3:3|c|#tag3:val3,tag4:val4\nmetric1:1|c\nmetric2:2|c|#tag2:val2|T{:}\n",
                now,
            )
            .as_str(),
            None,
        )
        .await;

        assert_eq!(response.series.len(), 1);
        assert_eq!(response.series[0].series.len(), 3);
        assert_eq!(response.distributions.len(), 0);
    }

    #[tokio::test]
    async fn test_dogstatsd_single_metric() {
        let response = setup_and_consume_dogstatsd("metric123:99123|c|T1656581409", None).await;

        assert_eq!(response.series.len(), 1);
        assert_eq!(response.series[0].series.len(), 1);
        assert_eq!(response.distributions.len(), 0);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_dogstatsd_filter_service_check() {
        let response = setup_and_consume_dogstatsd("_sc|servicecheck|0", None).await;

        assert!(!logs_contain("Failed to parse metric"));
        assert_eq!(response.series.len(), 0);
        assert_eq!(response.distributions.len(), 0);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_dogstatsd_filter_event() {
        let response = setup_and_consume_dogstatsd("_e{5,10}:event|test event", None).await;

        assert!(!logs_contain("Failed to parse metric"));
        assert_eq!(response.series.len(), 0);
        assert_eq!(response.distributions.len(), 0);
    }

    #[tokio::test]
    async fn test_dogstatsd_with_namespace() {
        let response =
            setup_and_consume_dogstatsd("my.metric:42|c", Some("custom.namespace".to_string()))
                .await;

        assert_eq!(response.series.len(), 1);
        assert_eq!(response.series[0].series.len(), 1);
        assert!(response.series[0].series[0]
            .metric
            .starts_with("custom.namespace.my.metric"));
    }

    async fn setup_and_consume_dogstatsd(
        statsd_string: &str,
        metric_namespace: Option<String>,
    ) -> crate::aggregator_service::FlushResponse {
        // Create the aggregator service
        let (service, handle) =
            AggregatorService::new(EMPTY_TAGS, 1_024).expect("aggregator service creation failed");

        // Start the service in a background task
        let service_task = tokio::spawn(service.run());

        let cancel_token = tokio_util::sync::CancellationToken::new();

        let dogstatsd = DogStatsD {
            cancel_token,
            aggregator_handle: handle.clone(),
            buffer_reader: BufferReader::MirrorTest(
                statsd_string.as_bytes().to_vec(),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(111, 112, 113, 114)), 0),
            ),
            metric_namespace,
        };
        dogstatsd.consume_statsd().await;

        // Get the metrics via flush
        let response = handle.flush().await.expect("Failed to flush");

        // Shutdown the service
        handle.shutdown().expect("Failed to shutdown");
        service_task.await.expect("Service task failed");

        response
    }
}
