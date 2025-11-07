// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use bytes::Bytes;

use reqwest::header::HeaderMap;
use std::sync::Arc;
use tokio::sync::mpsc::Receiver;
use tracing::{debug, error};

use crate::config::Config;
use crate::http_utils::build_client;
use core::time::Duration;

const DD_ADDITIONAL_TAGS_HEADER: &str = "X-Datadog-Additional-Tags";

pub struct ProxyRequest {
    pub headers: HeaderMap,
    pub body: Bytes,
    pub target_url: String,
}

pub struct ProxyFlusher {
    pub config: Arc<Config>,
    client: reqwest::Client,
}

impl ProxyFlusher {
    pub fn new(config: Arc<Config>) -> Self {
        debug!(
            "Proxy Flusher | Creating new proxy flusher with target URL: {}",
            config.profiling_intake.url
        );
        let client = build_client(
            config.proxy_url.as_deref(),
            Duration::from_secs(config.proxy_client_timeout),
        )
        .unwrap_or_else(|e| {
            error!(
                "Unable to parse proxy configuration: {}, no proxy will be used",
                e
            );
            reqwest::Client::new()
        });
        ProxyFlusher { config, client }
    }

    /// Starts the proxy flusher that listens for proxy payloads from the channel and forwards them to Datadog
    pub async fn start_proxy_flusher(&self, mut rx: Receiver<ProxyRequest>) {
        let Some(api_key) = self.config.profiling_intake.api_key.as_ref() else {
            error!("Proxy Flusher | No API key configured, cannot start");
            return;
        };

        debug!("Proxy Flusher | Started, listening for requests");

        while let Some(proxy_payload) = rx.recv().await {
            debug!(
                "Proxy Flusher | Received request from channel, body size: {} bytes",
                proxy_payload.body.len()
            );
            self.send_request(proxy_payload, api_key).await;
        }
    }

    async fn create_request(
        &self,
        request: &ProxyRequest,
        api_key: &str,
    ) -> Result<reqwest::RequestBuilder, String> {
        let mut headers = request.headers.clone();

        // Remove headers that are not needed for the proxy request
        headers.remove("host");
        headers.remove("content-length");

        // Add headers to the request
        match api_key.parse() {
            Ok(parsed_key) => headers.insert("DD-API-KEY", parsed_key),
            Err(e) => return Err(format!("Failed to parse API key: {}", e)),
        };

        // Add additional Azure Function-specific tags
        let mut tag_parts = vec![
            format!("_dd.origin:azure_functions"),
            format!(
                "functionname:{}",
                self.config.app_name.as_deref().unwrap_or_default()
            ),
        ];

        // Append aas.* tags
        for (key, value) in self.config.tags.tags() {
            if key.starts_with("aas.") {
                tag_parts.push(format!("{}:{}", key, value));
            }
        }

        let additional_tags = tag_parts.join(";");
        debug!("Proxy Flusher | Adding profiling tags: {}", additional_tags);
        match additional_tags.parse() {
            Ok(parsed_tags) => {
                headers.insert(DD_ADDITIONAL_TAGS_HEADER, parsed_tags);
            }
            Err(e) => {
                return Err(format!("Failed to parse additional tags header: {}", e));
            }
        };

        Ok(self
            .client
            .post(&request.target_url)
            .headers(headers)
            .timeout(std::time::Duration::from_secs(
                self.config.proxy_request_timeout,
            ))
            .body(request.body.clone()))
    }

    async fn send_request(&self, request: ProxyRequest, api_key: &str) {
        let max_retries = self.config.proxy_max_retries;
        let mut attempts = 0;

        loop {
            attempts += 1;

            let request_builder = match self.create_request(&request, api_key).await {
                Ok(builder) => builder,
                Err(e) => {
                    error!("Proxy Flusher | {}", e);
                    return;
                }
            };

            debug!(
                "Proxy Flusher | Sending request (attempt {}/{})",
                attempts, max_retries
            );

            let time = std::time::Instant::now();
            let response = request_builder.send().await;
            let elapsed = time.elapsed();

            match response {
                Ok(r) => {
                    let url = r.url().to_string();
                    let status = r.status();
                    let body = r.text().await;
                    if status == 202 || status == 200 {
                        debug!(
                            "Proxy Flusher | Successfully sent request in {} ms to {url}",
                            elapsed.as_millis()
                        );
                    } else {
                        error!("Proxy Flusher | Request failed with status {status}: {body:?}");
                    }
                    return;
                }
                Err(e) => {
                    // Only retry on network errors
                    error!(
                        "Proxy Flusher | Network error (attempt {}): {:?}",
                        attempts, e
                    );
                    if attempts >= max_retries {
                        error!(
                            "Proxy Flusher | Failed to send request after {} attempts: {:?}",
                            attempts, e
                        );
                        return;
                    }
                    // Exponential backoff before retry
                    let backoff_ms =
                        self.config.proxy_retry_backoff_base_ms * (2_u64.pow(attempts - 1));
                    debug!("Proxy Flusher | Retrying after {}ms backoff", backoff_ms);
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                }
            }
        }
    }
}
