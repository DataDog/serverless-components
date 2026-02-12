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
use socket2::{Domain, Protocol, Socket, Type};
use tracing::{debug, error, trace};

// Windows-specific imports
#[cfg(all(windows, feature = "windows-pipes"))]
use {std::sync::Arc, tokio::io::AsyncReadExt, tokio::net::windows::named_pipe::ServerOptions};

// Default buffer size for receiving DogStatsD packets (one read call).
// Used for both UDP recv_from and Windows named pipe reads.
const DEFAULT_BUFFER_SIZE: usize = 8192;

/// Configuration for the DogStatsD server
pub struct DogStatsDConfig {
    /// Host to bind UDP socket to (e.g., "127.0.0.1")
    pub host: String,
    /// Port to bind UDP socket to (e.g., 8125), will be 0 if we're using a Named Pipe
    pub port: u16,
    /// Optional namespace to prepend to all metric names (e.g., "myapp")
    pub metric_namespace: Option<String>,
    /// Optional Windows named pipe name. (e.g., "\\\\.\\pipe\\my_pipe").
    #[cfg(all(windows, feature = "windows-pipes"))]
    pub windows_pipe_name: Option<String>,
    /// Optional socket receive buffer size (SO_RCVBUF) in bytes.
    /// If None, uses the OS default. Increase this to reduce packet loss under high load.
    pub so_rcvbuf: Option<usize>,
    /// Max size of a single UDP packet read, in bytes. Defaults to 8192.
    /// Both the server and client must agree — the client must batch metrics
    /// into packets of this size for the increase to take effect.
    pub buffer_size: Option<usize>,
}

/// Represents the source of a DogStatsD message. Varies by transport method.
#[derive(Debug, Clone)]
pub enum MessageSource {
    /// Message received from a network socket (UDP)
    Network(SocketAddr),
    /// Message received from a Windows named pipe (Arc for efficient cloning)
    #[cfg(all(windows, feature = "windows-pipes"))]
    NamedPipe(Arc<String>),
}

impl std::fmt::Display for MessageSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(addr) => write!(f, "{}", addr),
            #[cfg(all(windows, feature = "windows-pipes"))]
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
    #[cfg(all(windows, feature = "windows-pipes"))]
    NamedPipe {
        pipe_name: Arc<String>,
        receiver: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>>,
    },
}

impl BufferReader {
    /// This is the main entry point for receiving metric data.
    /// Note: Different transports have different blocking behaviors.
    async fn read(&self, buf_size: usize) -> std::io::Result<(Vec<u8>, MessageSource)> {
        match self {
            BufferReader::UdpSocket(socket) => {
                // UDP socket: blocks until a packet arrives
                let mut buf = vec![0u8; buf_size];

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
            #[cfg(all(windows, feature = "windows-pipes"))]
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
    buf_size: usize,
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
        // Determine if we should use a named pipe or UDP/UDS
        #[cfg(all(windows, feature = "windows-pipes"))]
        let pipe_name_opt = config.windows_pipe_name.as_ref();
        #[cfg(not(all(windows, feature = "windows-pipes")))]
        let pipe_name_opt: Option<&String> = None;

        let buffer_reader = if let Some(pipe_name_ref) = pipe_name_opt {
            // Windows named pipe transport
            #[cfg(all(windows, feature = "windows-pipes"))]
            {
                let pipe_name = Arc::new(pipe_name_ref.clone());

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
            #[cfg(not(all(windows, feature = "windows-pipes")))]
            {
                let _ = pipe_name_ref; // Suppress unused variable warning
                unreachable!(
                    "Named pipes are only supported on Windows with the windows-pipes feature \
                    enabled, cannot use pipe: {}.",
                    pipe_name_ref
                );
            }
        } else {
            // UDP socket for all platforms
            let addr = format!("{}:{}", config.host, config.port);
            // TODO (UDS socket)
            #[allow(clippy::expect_used)]
            let socket = create_udp_socket(&addr, config.so_rcvbuf)
                .await
                .expect("couldn't create UDP socket");
            BufferReader::UdpSocket(socket)
        };

        let buf_size = match config.buffer_size {
            Some(0) => {
                error!(
                    "DogStatsD buffer_size cannot be 0, falling back to default ({})",
                    DEFAULT_BUFFER_SIZE
                );
                DEFAULT_BUFFER_SIZE
            }
            Some(size) => size,
            None => DEFAULT_BUFFER_SIZE,
        };

        DogStatsD {
            cancel_token,
            aggregator_handle,
            buffer_reader,
            metric_namespace: config.metric_namespace.clone(),
            buf_size,
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
            .read(self.buf_size)
            .await
            .expect("didn't receive data");

        // Skip empty buffers (e.g., from channel close)
        if buf.is_empty() {
            debug!("Received empty buffer from {}, skipping", src);
            return;
        }

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

async fn create_udp_socket(
    addr: &str,
    so_rcvbuf: Option<usize>,
) -> std::io::Result<tokio::net::UdpSocket> {
    // Resolve via lookup_host to support hostnames (e.g. "localhost:8125"),
    // matching the previous behavior of tokio::net::UdpSocket::bind().
    let socket_addr = tokio::net::lookup_host(addr).await?.next().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("Could not resolve address '{}'", addr),
        )
    })?;

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

    // Log the kernel's rmem_max cap so operators can tell
    // whether the requested SO_RCVBUF was capped by the OS.
    #[cfg(target_os = "linux")]
    if let Ok(rmem_max) = std::fs::read_to_string("/proc/sys/net/core/rmem_max") {
        debug!("DogStatsD Kernel rmem_max={} bytes", rmem_max.trim());
    }

    if let Some(buf_size) = so_rcvbuf {
        socket.set_recv_buffer_size(buf_size)?;

        // The kernel may cap the value; log what we actually got.
        let actual = socket.recv_buffer_size().unwrap_or(0);
        debug!(
            "DogStatsD SO_RCVBUF: requested={} bytes, actual={} bytes",
            buf_size, actual
        );
    } else {
        debug!(
            "DogStatsD using default SO_RCVBUF: {} bytes",
            socket.recv_buffer_size().unwrap_or(0)
        );
    }

    // Required for tokio compatibility
    socket.set_nonblocking(true)?;

    socket.bind(&socket_addr.into())?;

    let std_socket: std::net::UdpSocket = socket.into();
    tokio::net::UdpSocket::from_std(std_socket)
}

/// Named Pipe server - accepts client connections and forwards metrics.
///
/// Uses a multi-instance approach (like winio in the main agent):
/// - Creates new server instance for each client
/// - Spawns task to handle each client
#[cfg(all(windows, feature = "windows-pipes"))]
async fn run_named_pipe_server(
    pipe_name: Arc<String>,
    sender: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    loop {
        if cancel_token.is_cancelled() {
            break;
        }

        // Create new named pipe server
        let server = match ServerOptions::new().create(&*pipe_name) {
            Ok(s) => {
                debug!("Created pipe server instance '{}' in byte mode", pipe_name);
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
        let cancel_clone = cancel_token.clone();
        let pipe_name_clone = pipe_name.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; DEFAULT_BUFFER_SIZE];
            let mut server = server;
            // Byte mode requires manual buffering to handle messages split across reads
            // so let's track where we're writing each read in the buffer
            let mut start_write_index = 0;

            loop {
                let read_result = tokio::select! {
                    result = server.read(&mut buf[start_write_index..]) => result,
                    _ = cancel_clone.cancelled() => break,
                };

                let bytes_read = match read_result {
                    Err(e) => {
                        error!("Read error on '{}': {}", pipe_name_clone, e);
                        break;
                    }
                    Ok(0) => {
                        debug!("Client disconnected from '{}'", pipe_name_clone);
                        break;
                    }
                    Ok(n) => n,
                };

                let end_index = start_write_index + bytes_read;

                // From the start of the buffer to the end of the last complete message
                let complete_message_size = buf[..end_index]
                    .iter()
                    .rposition(|&b| b == b'\n')
                    .map(|pos| pos + 1) // \n is part of that last message, so +1
                    .unwrap_or(0);

                if complete_message_size > 0 {
                    // Send complete messages
                    match sender_clone.send(buf[..complete_message_size].to_vec()) {
                        Err(e) => {
                            error!("Failed to send data from '{}': {}", pipe_name_clone, e);
                            break;
                        }
                        Ok(_) => {
                            if std::str::from_utf8(&buf[..complete_message_size]).is_ok() {
                                debug!(
                                    "Sent {} bytes from '{}'",
                                    complete_message_size, pipe_name_clone
                                );
                            }
                        }
                    }
                }

                // Complete message has been sent, so we can write over it.
                start_write_index = end_index - complete_message_size;

                // If the message is bigger than the buffer size, drop it and go on.
                if start_write_index >= DEFAULT_BUFFER_SIZE {
                    start_write_index = 0;
                } else if start_write_index > 0 {
                    // Keep incomplete data in the buffer
                    buf.copy_within(complete_message_size..end_index, 0);
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

    #[tokio::test]
    async fn test_create_udp_socket_default_so_rcvbuf() {
        let socket = super::create_udp_socket("127.0.0.1:0", None).await.unwrap();
        let std_socket = socket.into_std().unwrap();
        let s2 = socket2::Socket::from(std_socket);
        let buf_size = s2.recv_buffer_size().unwrap();
        assert!(buf_size > 0, "default SO_RCVBUF should be non-zero");
    }

    #[tokio::test]
    async fn test_create_udp_socket_custom_so_rcvbuf() {
        let requested: usize = 262_144;
        let socket = super::create_udp_socket("127.0.0.1:0", Some(requested))
            .await
            .unwrap();
        let std_socket = socket.into_std().unwrap();
        let s2 = socket2::Socket::from(std_socket);
        let actual = s2.recv_buffer_size().unwrap();
        // The kernel may double the value (Linux) or cap it, but it should
        // be at least as large as the requested size.
        assert!(
            actual >= requested,
            "SO_RCVBUF actual ({}) should be >= requested ({})",
            actual,
            requested
        );
        async fn test_dogstatsd_custom_buffer_size() {
            // Use a large buffer to verify custom buf_size is wired through.
            // The MirrorTest reader copies data into the caller's buffer, so
            // a payload that fits within the custom size should parse correctly.
            let payload = "large.buf.metric:1|c\nlarge.buf.metric2:2|c\n";
            let custom_buf_size: usize = 16384;

            let (service, handle) = AggregatorService::new(EMPTY_TAGS, 1_024)
                .expect("aggregator service creation failed");
            let service_task = tokio::spawn(service.run());
            let cancel_token = tokio_util::sync::CancellationToken::new();

            let dogstatsd = DogStatsD {
                cancel_token,
                aggregator_handle: handle.clone(),
                buffer_reader: BufferReader::MirrorTest(
                    payload.as_bytes().to_vec(),
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0),
                ),
                metric_namespace: None,
                buf_size: custom_buf_size,
            };
            dogstatsd.consume_statsd().await;

            let response = handle.flush().await.expect("Failed to flush");
            assert_eq!(response.series.len(), 1);
            assert_eq!(response.series[0].series.len(), 2);

            handle.shutdown().expect("Failed to shutdown");
            service_task.await.expect("Service task failed");
        }

        #[tokio::test]
        #[traced_test]
        async fn test_dogstatsd_zero_buffer_size_falls_back_to_default() {
            let cancel_token = tokio_util::sync::CancellationToken::new();
            let (service, handle) = AggregatorService::new(EMPTY_TAGS, 1_024)
                .expect("aggregator service creation failed");
            tokio::spawn(service.run());

            let config = super::DogStatsDConfig {
                host: "127.0.0.1".to_string(),
                port: 0,
                metric_namespace: None,
                #[cfg(all(windows, feature = "windows-pipes"))]
                windows_pipe_name: None,
                so_rcvbuf: None,
                buffer_size: Some(0),
            };

            let dogstatsd = DogStatsD::new(&config, handle.clone(), cancel_token).await;
            assert_eq!(dogstatsd.buf_size, super::DEFAULT_BUFFER_SIZE);
            assert!(logs_contain("buffer_size cannot be 0"));

            handle.shutdown().expect("Failed to shutdown");
        }

        async fn setup_and_consume_dogstatsd(
            statsd_string: &str,
            metric_namespace: Option<String>,
        ) -> crate::aggregator_service::FlushResponse {
            // Create the aggregator service
            let (service, handle) = AggregatorService::new(EMPTY_TAGS, 1_024)
                .expect("aggregator service creation failed");

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
                buf_size: super::DEFAULT_BUFFER_SIZE,
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
}
