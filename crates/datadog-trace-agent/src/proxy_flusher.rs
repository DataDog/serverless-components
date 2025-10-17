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
        debug!("Proxy Flusher | Creating new proxy flusher with target URL: {}", config.proxy_intake.url);
        let client = build_client(config.proxy_url.as_deref(), Duration::from_secs(30))
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
        let Some(api_key) = self.config.proxy_intake.api_key.as_ref() else {
            error!("Proxy Flusher | No API key configured, cannot start");
            return;
        };

        debug!("Proxy Flusher | Started, listening for requests");

        while let Some(proxy_payload) = rx.recv().await {
            debug!("Proxy Flusher | Received request from channel, body size: {} bytes", proxy_payload.body.len());
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

        Ok(self
            .client
            .post(&request.target_url)
            .headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .body(request.body.clone()))
    }

    async fn send_request(&self, request: ProxyRequest, api_key: &str) {
        const MAX_RETRIES: u32 = 3;
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

            debug!("Proxy Flusher | Sending request (attempt {}/{})", attempts, MAX_RETRIES);

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
                    error!("Proxy Flusher | Network error (attempt {}): {:?}", attempts, e);
                    if attempts >= MAX_RETRIES {
                        error!(
                            "Proxy Flusher | Failed to send request after {} attempts: {:?}",
                            attempts, e
                        );
                        return;
                    };
                }
            }
            // Exponential backoff
            let backoff_ms = 100 * (2_u64.pow(attempts - 1));
            debug!("Proxy Flusher | Retrying after {}ms backoff", backoff_ms);
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        }
    }
}
