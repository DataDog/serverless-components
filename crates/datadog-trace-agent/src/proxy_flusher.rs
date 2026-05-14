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
use libdd_trace_utils::trace_utils;

const DD_ADDITIONAL_TAGS_HEADER: &str = "X-Datadog-Additional-Tags";

/// Returns the appropriate _dd.origin value based on the environment type
fn get_dd_origin(env_type: &trace_utils::EnvironmentType) -> &'static str {
    match env_type {
        trace_utils::EnvironmentType::AzureFunction => "azurefunction",
        trace_utils::EnvironmentType::CloudFunction => "cloudrun",
        _ => "unknown",
    }
}

pub struct ProxyRequest {
    pub headers: HeaderMap,
    pub body: Bytes,
    pub target_url: String,
    pub kind: ProxyRequestKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProxyRequestKind {
    DataStreams,
    Profiling,
}

pub struct ProxyFlusher {
    pub config: Arc<Config>,
    client: reqwest::Client,
}

impl ProxyFlusher {
    pub fn new(config: Arc<Config>) -> Self {
        debug!("Creating new proxy flusher");
        let client = build_client(
            config.proxy_url.as_deref(),
            Duration::from_secs(config.proxy_request_timeout_secs),
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
        debug!("Started, listening for requests");

        while let Some(proxy_payload) = rx.recv().await {
            debug!(
                "Received request from channel, body size: {} bytes",
                proxy_payload.body.len()
            );
            self.send_request(proxy_payload).await;
        }
    }

    fn api_key_for_kind(&self, kind: ProxyRequestKind) -> Option<&str> {
        match kind {
            ProxyRequestKind::DataStreams => self.config.dsm_intake.api_key.as_deref(),
            ProxyRequestKind::Profiling => self.config.profiling_intake.api_key.as_deref(),
        }
    }

    fn kind_name(kind: ProxyRequestKind) -> &'static str {
        match kind {
            ProxyRequestKind::DataStreams => "data streams",
            ProxyRequestKind::Profiling => "profiling",
        }
    }

    fn parse_additional_tags(tag_string: &str) -> impl Iterator<Item = &str> {
        tag_string
            .split([',', ';'])
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
    }

    fn push_unique_tag(tags: &mut Vec<String>, tag: impl Into<String>) {
        let tag = tag.into();
        if !tags.iter().any(|existing| existing == &tag) {
            tags.push(tag);
        }
    }

    fn merged_additional_tags(&self, headers: &HeaderMap) -> Result<Option<String>, String> {
        let mut tags = Vec::new();

        if let Some(existing_tags) = headers.get(DD_ADDITIONAL_TAGS_HEADER) {
            let existing_tags = existing_tags
                .to_str()
                .map_err(|e| format!("Failed to parse existing additional tags header: {e}"))?;
            for tag in Self::parse_additional_tags(existing_tags) {
                Self::push_unique_tag(&mut tags, tag.to_string());
            }
        }

        if let Some(function_tags) = self.config.tags.function_tags() {
            for tag in Self::parse_additional_tags(function_tags) {
                Self::push_unique_tag(&mut tags, tag.to_string());
            }
        }

        Self::push_unique_tag(
            &mut tags,
            format!(
                "functionname:{}",
                self.config.app_name.as_deref().unwrap_or_default()
            ),
        );
        Self::push_unique_tag(
            &mut tags,
            format!("_dd.origin:{}", get_dd_origin(&self.config.env_type)),
        );

        if tags.is_empty() {
            Ok(None)
        } else {
            Ok(Some(tags.join(",")))
        }
    }

    async fn create_request(
        &self,
        request: &ProxyRequest,
    ) -> Result<reqwest::RequestBuilder, String> {
        let mut headers = request.headers.clone();
        let api_key = self.api_key_for_kind(request.kind).ok_or_else(|| {
            format!(
                "No API key configured for {}",
                Self::kind_name(request.kind)
            )
        })?;

        // Remove headers that are not needed for the proxy request
        headers.remove("host");
        headers.remove("content-length");

        // Preserve any incoming additional tags and augment them with the
        // serverless tags this mini-agent is responsible for adding.
        if let Some(additional_tags) = self.merged_additional_tags(&headers)? {
            match additional_tags.parse() {
                Ok(parsed_tags) => {
                    headers.insert(DD_ADDITIONAL_TAGS_HEADER, parsed_tags);
                }
                Err(e) => {
                    return Err(format!("Failed to parse additional tags header: {}", e));
                }
            };
        }

        match api_key.parse() {
            Ok(parsed_key) => headers.insert("DD-API-KEY", parsed_key),
            Err(e) => return Err(format!("Failed to parse API key: {}", e)),
        };

        Ok(self
            .client
            .post(&request.target_url)
            .headers(headers)
            .timeout(std::time::Duration::from_secs(
                self.config.proxy_request_timeout_secs,
            ))
            .body(request.body.clone()))
    }

    async fn send_request(&self, request: ProxyRequest) {
        let max_retries = self.config.proxy_request_max_retries;
        let mut attempts = 0;

        loop {
            attempts += 1;

            let request_builder = match self.create_request(&request).await {
                Ok(builder) => builder,
                Err(e) => {
                    error!("{}", e);
                    return;
                }
            };

            debug!("Sending request (attempt {}/{})", attempts, max_retries);

            let time = std::time::Instant::now();
            let response = request_builder.send().await;
            let elapsed = time.elapsed();

            match response {
                Ok(r) => {
                    let url = r.url().to_string();
                    let status = r.status();
                    if status.is_success() {
                        debug!(
                            "Successfully sent request in {} ms to {url}",
                            elapsed.as_millis()
                        );
                    } else {
                        error!(
                            "Request failed with status {status} after {} ms to {url}; response body omitted",
                            elapsed.as_millis()
                        );
                    }
                    return;
                }
                Err(e) => {
                    // Only retry on network errors
                    error!("Network error (attempt {}): {:?}", attempts, e);
                    if attempts >= max_retries {
                        error!(
                            "Failed to send request after {} attempts: {:?}",
                            attempts, e
                        );
                        return;
                    }
                    // Exponential backoff before retry
                    let backoff_ms =
                        self.config.proxy_request_retry_backoff_base_ms * (2_u64.pow(attempts - 1));
                    debug!("Retrying after {}ms backoff", backoff_ms);
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                }
            }
        }
    }
}
