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
    tokio::net::windows::named_pipe::{ClientOptions, ServerOptions},
};

// DogStatsD buffer size for receiving metrics
// TODO(astuyve) buf should be dynamic
// Max buffer size is configurable in Go Agent with a default of 8KB
// https://github.com/DataDog/datadog-agent/blob/85939a62b5580b2a15549f6936f257e61c5aa153/pkg/config/config_template.yaml#L2154-L2158
const BUFFER_SIZE: usize = 8192;

/// Configuration for the DogStatsD server
pub struct DogStatsDConfig {
    /// Host to bind UDP socket to (e.g., "127.0.0.1")
    pub host: String,
    /// Port to bind UDP socket to (e.g., 8125), will be 0 if we're using a Named Pipe
    pub port: u16,
    /// Optional namespace to prepend to all metric names (e.g., "myapp")
    pub metric_namespace: Option<String>,
    /// Optional Windows named pipe name. (e.g., "\\\\.\\pipe\\my_pipe").
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
        receiver: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>>,
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
                receiver,
            } => {
                // Named Pipe Reader: receives data from client handler tasks
                match receiver.lock().await.recv().await {
                    Some(data) => Ok((data, MessageSource::NamedPipe(pipe_name.clone()))),
                    None => {
                        // Channel closed - server exited, already triggered cancellation
                        Ok((Vec::new(), MessageSource::NamedPipe(pipe_name.clone())))
                    }
                }
            }
        }
    }
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
                let pipe_name = Arc::new(pipe_name.clone());

                // Create channel for receiving data from client handlers
                let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
                let receiver = Arc::new(tokio::sync::Mutex::new(receiver));

                // Spawn server accept loop
                let server_pipe_name = pipe_name.clone();
                let server_cancel = cancel_token.clone();
                tokio::spawn(async move {
                    run_named_pipe_server(server_pipe_name, sender, server_cancel).await;
                });

                BufferReader::NamedPipe {
                    pipe_name,
                    receiver,
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

    fn prepend_namespace(namespace: &str, metric: &mut Metric) {
        let new_name = format!("{}.{}", namespace, metric.name);
        metric.name = ustr::Ustr::from(&new_name);
        metric.id = id(metric.name, &metric.tags, metric.timestamp);
    }

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
            // Send metrics through the channel - no lock needed!
            if let Err(e) = self.aggregator_handle.insert_batch(all_valid_metrics) {
                error!("Failed to send metrics to aggregator: {}", e);
            }
        }
    }
}

/// Named Pipe server - accepts client connections and forwards metrics.
///
/// Uses a multi-instance approach (like winio in the main agent):
/// - Creates new server instance for each client
/// - Spawns task to handle each client
#[cfg(windows)]
async fn run_named_pipe_server(
    pipe_name: Arc<String>,
    sender: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    loop {
        if cancel_token.is_cancelled() {
            break;
        }

        // Create new server instance
        let server = match ServerOptions::new().create(&*pipe_name) {
            Ok(s) => {
                debug!("Created pipe server instance '{}'", pipe_name);
                s
            }
            Err(e) => {
                error!("Failed to create pipe '{}': {}", pipe_name, e);
                tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => continue,
                    _ = cancel_token.cancelled() => break,
                }
            }
        };

        // Wait for client connection (cancellable)
        let connect_result = tokio::select! {
            result = server.connect() => result,
            _ = cancel_token.cancelled() => break,
        };

        if let Err(e) = connect_result {
            error!("Connection failed on '{}': {}", pipe_name, e);
            continue;
        }

        debug!("Client connected to '{}'", pipe_name);

        // Spawn task to handle this client
        let sender_clone = sender.clone();
        let pipe_name_clone = pipe_name.clone();
        let cancel_clone = cancel_token.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; BUFFER_SIZE];
            let mut server = server;

            loop {
                // Read with cancellation support
                let read_result = tokio::select! {
                    result = server.read(&mut buf) => result,
                    _ = cancel_clone.cancelled() => break,
                };

                let n = match read_result {
                    Ok(0) => {
                        debug!("Client disconnected from '{}'", pipe_name_clone);
                        break; // Client disconnected
                    }
                    Ok(n) => n,
                    Err(e) => {
                        error!("Read error on '{}': {}", pipe_name_clone, e);
                        break;
                    }
                };

                // Send data to the main read function / processing loop
                if sender_clone.send(buf[..n].to_vec()).is_err() {
                    error!("Failed to send data from '{}'", pipe_name_clone);
                    break;
                }
            }
            // Server instance is dropped here, automatically cleaned up
        });
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
