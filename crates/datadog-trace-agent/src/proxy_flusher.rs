// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use bytes::Bytes;

use std::{sync::Arc};
use tokio::sync::{mpsc::Receiver};
use tracing::{debug, error};
use reqwest::header::HeaderMap;

use crate::config::Config;
use crate::http_utils::build_client;
use core::time::Duration;

pub struct ProxyRequest {
    pub headers: HeaderMap,
    pub body: Bytes,
    pub target_url: String,
}

pub struct ProxyFlusher {
    /// Handles forwarding proxy requests to Datadog with retry logic
    pub config: Arc<Config>,
    client: reqwest::Client,
}

impl ProxyFlusher {
    pub fn new(config: Arc<Config>) -> Self {
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
        while let Some(proxy_payload) = rx.recv().await {
            self.send_request(proxy_payload).await;
        }
    }

    async fn create_request(
        &self,
        request: &ProxyRequest,
        api_key: &str,
    ) -> reqwest::RequestBuilder {
        let mut headers = request.headers.clone();

        // Remove headers that are not needed for the proxy request
        headers.remove("host");
        headers.remove("content-length");

        // Add headers to the request
        headers.insert("DD-API-KEY", api_key.parse().expect("Failed to parse API key header"));

        self.client
            .post(&request.target_url)
            .headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .body(request.body.clone())
    }

    async fn send_request(&self, request: ProxyRequest) {
        const MAX_RETRIES: u32 = 3;
        let mut attempts = 0;

        loop {
            attempts += 1;

            let request_builder = self.create_request(&request, self.config.proxy_intake.api_key.as_ref().unwrap()).await;
            let time = std::time::Instant::now();
            let response = request_builder.send().await;
            let elapsed = time.elapsed();

            match response {
                Ok(r) => {
                    let url = r.url().to_string();
                    let status = r.status();
                    let body = r.text().await;
                    if status == 202 || status == 200 {
                        debug!("Proxy Flusher | Successfully sent request in {} ms to {url}", elapsed.as_millis());
                    } else {
                        error!("Proxy Flusher | Request failed with status {status}: {body:?}");
                    }
                    return;
                }
                Err(e) => {
                    error!("Network error (attempt {}): {:?}", attempts, e);
                    if attempts >= MAX_RETRIES {
                        error!("Proxy Flusher | Failed to send request after {} attempts: {:?}", attempts, e);
                        return;
                    };
                }
            }
            // Exponential backoff
            let backoff_ms = 100 * (2_u64.pow(attempts - 1));
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        }

    }   
}

