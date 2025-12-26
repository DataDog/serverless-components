// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::net::SocketAddr;
use std::str::Split;

use crate::aggregator_service::AggregatorHandle;
use crate::errors::ParseError::UnsupportedType;
use crate::metric::{id, parse, Metric};
use tracing::{debug, error, trace};

// Windows-specific imports and constants
#[cfg(windows)]
use {
    std::cell::RefCell,
    tokio::io::AsyncReadExt,
    tokio::net::windows::named_pipe::ServerOptions,
    tokio::time::{sleep, Duration},
};

#[cfg(windows)]
const MAX_CONSECUTIVE_ERRORS: u32 = 10;

/// Represents the source of a DogStatsD message
#[derive(Debug, Clone)]
pub enum MessageSource {
    /// Message received from a network socket (UDP)
    Network(SocketAddr),
    /// Message received from a Windows named pipe
    #[cfg(windows)]
    NamedPipe(String),
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

pub struct DogStatsD {
    cancel_token: tokio_util::sync::CancellationToken,
    aggregator_handle: AggregatorHandle,
    buffer_reader: BufferReader,
    metric_namespace: Option<String>,
}

pub struct DogStatsDConfig {
    pub host: String,
    pub port: u16,
    pub metric_namespace: Option<String>,
    pub windows_pipe_name: Option<String>,
}

enum BufferReader {
    UdpSocketReader(tokio::net::UdpSocket),
    #[allow(dead_code)]
    MirrorReader(Vec<u8>, SocketAddr),
    #[cfg(windows)]
    NamedPipeReader {
        pipe_name: String,
        current_pipe: RefCell<Option<tokio::net::windows::named_pipe::NamedPipeServer>>,
        consecutive_errors: RefCell<u32>,
    },
}

#[cfg(windows)]
async fn create_and_connect_pipe(
    pipe_name: &str,
    consecutive_errors: &mut u32,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    // Create pipe with retry logic
    let pipe = loop {
        match ServerOptions::new()
            .first_pipe_instance(false)
            .create(pipe_name)
        {
            Ok(p) => {
                *consecutive_errors = 0;
                break p;
            }
            Err(e) => {
                error!("Failed to create named pipe: {}", e);
                *consecutive_errors += 1;

                if *consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "Too many consecutive pipe creation errors",
                    ));
                }

                let backoff_ms = 10u64 * (1 << consecutive_errors.min(6));
                sleep(Duration::from_millis(backoff_ms)).await;
                continue;
            }
        }
    };

    // Wait for client to connect
    match pipe.connect().await {
        Ok(()) => {
            *consecutive_errors = 0;
            debug!("Client connected to DogStatsD named pipe");
            Ok(pipe)
        }
        Err(e) => {
            error!("Failed to accept connection on named pipe: {}", e);
            *consecutive_errors += 1;

            if *consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Too many consecutive connection errors",
                ));
            }

            let backoff_ms = 10u64 * (1 << consecutive_errors.min(6));
            sleep(Duration::from_millis(backoff_ms)).await;
            Err(e)
        }
    }
}

impl BufferReader {
    async fn read(&self) -> std::io::Result<(Vec<u8>, MessageSource)> {
        match self {
            BufferReader::UdpSocketReader(socket) => {
                // TODO(astuyve) this should be dynamic
                // Max buffer size is configurable in Go Agent and the default is 8KB
                // https://github.com/DataDog/datadog-agent/blob/85939a62b5580b2a15549f6936f257e61c5aa153/pkg/config/config_template.yaml#L2154-L2158
                let mut buf = [0; 8192];

                #[allow(clippy::expect_used)]
                let (amt, src) = socket
                    .recv_from(&mut buf)
                    .await
                    .expect("didn't receive data");
                Ok((buf[..amt].to_owned(), MessageSource::Network(src)))
            }
            BufferReader::MirrorReader(data, socket) => {
                Ok((data.clone(), MessageSource::Network(*socket)))
            }
            #[cfg(windows)]
            BufferReader::NamedPipeReader {
                pipe_name,
                current_pipe,
                consecutive_errors,
            } => {
                loop {
                    // Create pipe if needed
                    if current_pipe.borrow().is_none() {
                        let mut errors = consecutive_errors.borrow_mut();
                        match create_and_connect_pipe(pipe_name, &mut errors).await {
                            Ok(new_pipe) => {
                                drop(errors);
                                *current_pipe.borrow_mut() = Some(new_pipe);
                            }
                            Err(e) => {
                                return Err(e);
                            }
                        }
                    }

                    // Read from the connected pipe
                    let mut buf = [0; 8192];
                    let mut pipe_ref = current_pipe.borrow_mut();
                    let pipe = pipe_ref.as_mut().unwrap();

                    match pipe.read(&mut buf).await {
                        Ok(0) => {
                            *pipe_ref = None;
                            drop(pipe_ref);
                            continue;
                        }
                        Ok(amt) => {
                            *consecutive_errors.borrow_mut() = 0;
                            return Ok((
                                buf[..amt].to_vec(),
                                MessageSource::NamedPipe(pipe_name.to_string()),
                            ));
                        }
                        Err(e) => {
                            // Read error
                            error!("Error reading from named pipe: {}", e);
                            *consecutive_errors.borrow_mut() += 1;
                            *pipe_ref = None;
                            drop(pipe_ref);

                            let current_errors = *consecutive_errors.borrow();
                            if current_errors >= MAX_CONSECUTIVE_ERRORS {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::Other,
                                    "Too many consecutive read errors",
                                ));
                            }

                            let backoff_ms = 10u64 * (1 << current_errors.min(6));
                            sleep(Duration::from_millis(backoff_ms)).await;
                            continue;
                        }
                    }
                }
            }
        }
    }
}

impl DogStatsD {
    #[must_use]
    pub async fn new(
        config: &DogStatsDConfig,
        aggregator_handle: AggregatorHandle,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> DogStatsD {
        // Fail fast on non-Windows if pipe name is configured
        #[cfg(not(windows))]
        if config.windows_pipe_name.is_some() {
            panic!("Named pipes are only supported on Windows");
        }

        #[allow(unused_variables)]  // pipe_name unused on non-Windows
        let buffer_reader = if let Some(ref pipe_name) = config.windows_pipe_name {
            #[cfg(windows)]
            {
                BufferReader::NamedPipeReader {
                    pipe_name: pipe_name.clone(),
                    current_pipe: RefCell::new(None),
                    consecutive_errors: RefCell::new(0),
                }
            }
            #[cfg(not(windows))]
            {
                unreachable!("Windows pipe on non-Windows (checked above)")
            }
        } else {
            // UDP socket for all platforms
            let addr = format!("{}:{}", config.host, config.port);
            // TODO (UDS socket)
            #[allow(clippy::expect_used)]
            let socket = tokio::net::UdpSocket::bind(addr)
                .await
                .expect("couldn't bind to address");
            BufferReader::UdpSocketReader(socket)
        };

        DogStatsD {
            cancel_token,
            aggregator_handle,
            buffer_reader,
            metric_namespace: config.metric_namespace.clone(),
        }
    }

    pub async fn spin(self) {
        let mut spin_cancelled = false;
        while !spin_cancelled {
            self.consume_statsd().await;
            spin_cancelled = self.cancel_token.is_cancelled();
        }
    }

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
            buffer_reader: BufferReader::MirrorReader(
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
