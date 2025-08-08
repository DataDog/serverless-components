// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::net::SocketAddr;
use std::str::Split;
use std::sync::Arc;

use crate::double_buffered_aggregator::DoubleBufferedAggregator;
use crate::errors::ParseError::UnsupportedType;
use crate::metric::{parse, Metric};
use tracing::{debug, error};

pub struct DogStatsD {
    cancel_token: tokio_util::sync::CancellationToken,
    aggregator: Arc<DoubleBufferedAggregator>,
    buffer_reader: BufferReader,
}

pub struct DogStatsDConfig {
    pub host: String,
    pub port: u16,
}

enum BufferReader {
    UdpSocketReader(tokio::net::UdpSocket),
    #[allow(dead_code)]
    MirrorReader(Vec<u8>, SocketAddr),
}

impl BufferReader {
    async fn read(&self) -> std::io::Result<(Vec<u8>, SocketAddr)> {
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
                Ok((buf[..amt].to_owned(), src))
            }
            BufferReader::MirrorReader(data, socket) => Ok((data.clone(), *socket)),
        }
    }
}

impl DogStatsD {
    #[must_use]
    pub async fn new(
        config: &DogStatsDConfig,
        aggregator: Arc<DoubleBufferedAggregator>,
        cancel_token: tokio_util::sync::CancellationToken,
    ) -> DogStatsD {
        let addr = format!("{}:{}", config.host, config.port);

        // TODO (UDS socket)
        #[allow(clippy::expect_used)]
        let socket = tokio::net::UdpSocket::bind(addr)
            .await
            .expect("couldn't bind to address");
        DogStatsD {
            cancel_token,
            aggregator,
            buffer_reader: BufferReader::UdpSocketReader(socket),
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
        debug!("Received message: {} from {}", msgs, src);
        let statsd_metric_strings = msgs.split('\n');
        self.insert_metrics(statsd_metric_strings);
    }

    fn insert_metrics(&self, msg: Split<char>) {
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
            .collect();
        if !all_valid_metrics.is_empty() {
            for a_valid_value in all_valid_metrics {
                let _ = self.aggregator.insert(a_valid_value);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use crate::double_buffered_aggregator::DoubleBufferedAggregator;
    use crate::dogstatsd::{BufferReader, DogStatsD};
    use crate::metric::EMPTY_TAGS;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;
    use tracing_test::traced_test;

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_dogstatsd_multi_distribution() {
        let aggregator_arc = setup_dogstatsd(
            "single_machine_performance.rouster.api.series_v2.payload_size_bytes:269942|d|T1656581409
single_machine_performance.rouster.metrics_min_timestamp_latency:1426.90870216|d|T1656581409
single_machine_performance.rouster.metrics_max_timestamp_latency:1376.90870216|d|T1656581409
",
        )
        .await;
        
        // Peek at metrics before flushing to verify they're there
        let (_, distributions_peek) = aggregator_arc.peek_metrics();
        assert_eq!(distributions_peek.sketches.len(), 3);
        
        // Now flush and verify we get the metrics
        let (_, distributions) = aggregator_arc.flush();
        let parsed_metrics = distributions.into_iter().next().unwrap();

        assert_eq!(parsed_metrics.sketches.len(), 3);
        
        // Verify sketch names
        let sketch_names: Vec<String> = parsed_metrics.sketches.iter()
            .map(|s| s.metric.to_string())
            .collect();
        assert!(sketch_names.contains(&"single_machine_performance.rouster.api.series_v2.payload_size_bytes".to_string()));
        assert!(sketch_names.contains(&"single_machine_performance.rouster.metrics_min_timestamp_latency".to_string()));
        assert!(sketch_names.contains(&"single_machine_performance.rouster.metrics_max_timestamp_latency".to_string()));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_dogstatsd_multi_metric() {
        let mut now: i64 = std::time::UNIX_EPOCH
            .elapsed()
            .expect("unable to poll clock, unrecoverable")
            .as_secs()
            .try_into()
            .unwrap_or_default();
        now = (now / 10) * 10;
        let aggregator_arc = setup_dogstatsd(
            format!(
                "metric3:3|c|#tag3:val3,tag4:val4\nmetric1:1|c\nmetric2:2|c|#tag2:val2|T{:}\n",
                now
            )
            .as_str(),
        )
        .await;
        
        // Peek at metrics before flushing to verify they're there
        let (series_peek, _) = aggregator_arc.peek_metrics();
        assert_eq!(series_peek.series.len(), 3);
        
        // Now flush and verify we get the metrics
        let (series, _) = aggregator_arc.flush();
        let parsed_metrics = series.into_iter().next().unwrap();

        assert_eq!(parsed_metrics.series.len(), 3);
        
        // Verify metric names
        let metric_names: Vec<_> = parsed_metrics.series.iter()
            .map(|s| s.metric.to_string())
            .collect();
        assert!(metric_names.contains(&"metric1".to_string()));
        assert!(metric_names.contains(&"metric2".to_string()));
        assert!(metric_names.contains(&"metric3".to_string()));
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)]
    async fn test_dogstatsd_single_metric() {
        let aggregator_arc = setup_dogstatsd("metric123:99123|c|T1656581409").await;
        
        // Peek at metrics before flushing to verify they're there
        let (series_peek, _) = aggregator_arc.peek_metrics();
        assert_eq!(series_peek.series.len(), 1);
        
        // Now flush and verify we get the metrics
        let (series, _) = aggregator_arc.flush();
        let parsed_metrics = series.into_iter().next().unwrap();

        assert_eq!(parsed_metrics.series.len(), 1);
        assert_eq!(parsed_metrics.series[0].metric, "metric123");
    }

    #[tokio::test]
    #[traced_test]
    #[cfg_attr(miri, ignore)]
    async fn test_dogstatsd_filter_service_check() {
        let aggregator_arc = setup_dogstatsd("_sc|servicecheck|0").await;
        
        let (series, _) = aggregator_arc.flush();

        assert!(!logs_contain("Failed to parse metric"));
        assert_eq!(series.len(), 0);
    }

    #[tokio::test]
    #[traced_test]
    #[cfg_attr(miri, ignore)]
    async fn test_dogstatsd_filter_event() {
        let aggregator_arc = setup_dogstatsd("_e{5,10}:event|test event").await;
        
        let (series, _) = aggregator_arc.flush();

        assert!(!logs_contain("Failed to parse metric"));
        assert_eq!(series.len(), 0);
    }

    async fn setup_dogstatsd(statsd_string: &str) -> Arc<DoubleBufferedAggregator> {
        let aggregator_arc = Arc::new(
            DoubleBufferedAggregator::new(EMPTY_TAGS, 1_024).expect("aggregator creation failed"),
        );
        let cancel_token = tokio_util::sync::CancellationToken::new();

        let dogstatsd = DogStatsD {
            cancel_token,
            aggregator: Arc::clone(&aggregator_arc),
            buffer_reader: BufferReader::MirrorReader(
                statsd_string.as_bytes().to_vec(),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(111, 112, 113, 114)), 0),
            ),
        };
        dogstatsd.consume_statsd().await;

        aggregator_arc
    }
}
