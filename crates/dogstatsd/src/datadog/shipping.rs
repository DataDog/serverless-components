// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use super::{MetricsIntakeUrlPrefix, Series};
use crate::flusher::ShippingError;
use datadog_protos::metrics::SketchPayload;
use protobuf::Message;
use reqwest::{Client, Response};
use serde_json;
use std::io::Write;
use std::time::Duration;
use tracing::{debug, trace};
use zstd::stream::write::Encoder;
use zstd::zstd_safe::CompressionLevel;

/// Interface for the `DogStatsD` metrics intake API.
#[derive(Debug, Clone)]
pub struct DdApi {
    api_key: String,
    metrics_intake_url_prefix: MetricsIntakeUrlPrefix,
    client: Client,
    retry_strategy: RetryStrategy,
    compression_level: CompressionLevel,
}

impl DdApi {
    #[must_use]
    pub fn new(
        api_key: String,
        metrics_intake_url_prefix: MetricsIntakeUrlPrefix,
        client: Client,
        retry_strategy: RetryStrategy,
        compression_level: CompressionLevel,
    ) -> Self {
        DdApi {
            api_key,
            metrics_intake_url_prefix,
            client,
            retry_strategy,
            compression_level,
        }
    }

    /// Ship a serialized series to the API, blocking
    pub async fn ship_series(&self, series: &Series) -> Result<Response, ShippingError> {
        let url = format!("{}/api/v2/series", &self.metrics_intake_url_prefix);
        let safe_body = serde_json::to_vec(&series)
            .map_err(|e| ShippingError::Payload(format!("Failed to serialize series: {e}")))?;
        trace!("Sending body: {:?}", &series);
        self.ship_data(url, safe_body, "application/json").await
    }

    pub async fn ship_distributions(
        &self,
        sketches: &SketchPayload,
    ) -> Result<Response, ShippingError> {
        let url = format!("{}/api/beta/sketches", &self.metrics_intake_url_prefix);
        let safe_body = sketches
            .write_to_bytes()
            .map_err(|e| ShippingError::Payload(format!("Failed to serialize series: {e}")))?;
        trace!("Sending distributions: {:?}", &sketches);
        self.ship_data(url, safe_body, "application/x-protobuf")
            .await
        // TODO maybe go to coded output stream if we incrementally
        // add sketch payloads to the buffer
        // something like this, but fix the utf-8 encoding issue
        // {
        //     let mut output_stream = CodedOutputStream::vec(&mut buf);
        //     let _ = output_stream.write_tag(1, protobuf::rt::WireType::LengthDelimited);
        //     let _ = output_stream.write_message_no_tag(&sketches);
        //     TODO not working, has utf-8 encoding issue in dist-intake
        //}
    }

    async fn ship_data(
        &self,
        url: String,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<Response, ShippingError> {
        let client = &self.client;
        let start = std::time::Instant::now();

        let result = (|| -> std::io::Result<Vec<u8>> {
            let mut encoder = Encoder::new(Vec::new(), self.compression_level)?;
            encoder.write_all(&body)?;
            encoder.finish()
        })();

        let mut builder = client
            .post(&url)
            .header("DD-API-KEY", &self.api_key)
            .header("Content-Type", content_type);

        builder = match result {
            Ok(compressed) => builder.header("Content-Encoding", "zstd").body(compressed),
            Err(err) => {
                debug!("Sending uncompressed data, failed to compress: {err}");
                builder.body(body)
            }
        };

        let resp = self.send_with_retry(builder).await;

        let elapsed = start.elapsed();
        debug!("Request to {} took {}ms", url, elapsed.as_millis());
        resp
    }

    async fn send_with_retry(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<Response, ShippingError> {
        let mut attempts = 0;
        loop {
            attempts += 1;
            let cloned_builder = match builder.try_clone() {
                Some(b) => b,
                None => {
                    return Err(ShippingError::Destination(
                        None,
                        "Failed to clone request".to_string(),
                    ));
                }
            };

            let response = cloned_builder.send().await;
            match response {
                Ok(response) if response.status().is_success() => {
                    return Ok(response);
                }
                _ => {}
            }

            match self.retry_strategy {
                RetryStrategy::LinearBackoff(max_attempts, _)
                | RetryStrategy::Immediate(max_attempts)
                    if attempts >= max_attempts =>
                {
                    let status = match response {
                        Ok(response) => Some(response.status()),
                        Err(err) => err.status(),
                    };
                    // handle if status code missing like timeout
                    return Err(ShippingError::Destination(
                        status,
                        format!("Failed to send request after {attempts} attempts").to_string(),
                    ));
                }
                RetryStrategy::LinearBackoff(_, delay) => {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                _ => {}
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum RetryStrategy {
    Immediate(u64),          // attempts
    LinearBackoff(u64, u64), // attempts, delay
}
