// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0
use std::{sync::Arc, time};
use tokio::sync::{Mutex, OnceCell, mpsc::Receiver};
use tracing::{debug, error};
use reqwest::header::HeaderMap;

use crate::config::Config;
use crate::proxy_aggregator::{ProxyAggregator, ProxyRequest};
use crate::http_utils::build_client;
use core::time::Duration;

pub struct ProxyFlusher {
    // Starts a proxy flusher that listens for proxy payloads
    pub aggregator: Arc<Mutex<ProxyAggregator>>,
    pub config: Arc<Config>,
    client: reqwest::Client,
    headers: OnceCell<HeaderMap>,
}

impl ProxyFlusher {
    pub fn new(aggregator: Arc<Mutex<ProxyAggregator>>, config: Arc<Config>) -> Self {
        // let client = (|| -> Result<reqwest::Client, Box<dyn std::error::Error>> {
        //     let mut builder = create_reqwest_client_builder()?.timeout(Duration::from_secs(30));
        //     if let Some(proxy) = &config.proxy_url {
        //         builder = builder.proxy(reqwest::Proxy::all(proxy)?);
        //     }
        //     Ok(builder.build()?)
        // })()
        // .unwrap_or_else(|e| {
        //     error!("Failed to create HTTP client: {}, using default", e);
        //     reqwest::Client::new()
        // });
        let client = build_client(config.proxy_url.as_deref(), Duration::from_secs(30))
            .unwrap_or_else(|e| {
                error!(
                    "Unable to parse proxy configuration: {}, no proxy will be used",
                    e
                );
                reqwest::Client::new()
            });
        ProxyFlusher { aggregator, config, client, headers: OnceCell::new() }
    }

    async fn get_headers(&self, api_key: &str) -> &HeaderMap {
        self.headers
            .get_or_init(move || async move {
                let mut headers = HeaderMap::new();
                headers.insert(
                    "DD-API-KEY",
                    api_key.parse().expect("Failed to parse API key header"),
                );
                headers
            })
            .await
    }

    pub async fn start_proxy_flusher(&self, mut rx: Receiver<ProxyRequest>) {
        let aggregator = Arc::clone(&self.aggregator);
        tokio::spawn(async move {
            while let Some(proxy_payload) = rx.recv().await {
                let mut guard = aggregator.lock().await;
                guard.add(proxy_payload);
            }
        });
        
        loop {
            tokio::time::sleep(time::Duration::from_secs(self.config.proxy_flush_interval)).await;
            self.flush(None).await;
        }
    }

    /// Flushes proxy requests by getting every available batch on the aggregator.
    /// If `failed_requests` is provided, it will attempt to send those instead of fetching new requests.
    /// Returns any requests that failed to send and should be retried.
    async fn flush(&self, failed_requests: Option<Vec<ProxyRequest>>) -> Option<Vec<ProxyRequest>> {
        let mut failed_batch: Option<Vec<ProxyRequest>> = None;

        if let Some(requests) = failed_requests {
            // If we have requests from a previous failed attempt, try to send those first
            if !requests.is_empty() {
                debug!("Proxy Flusher | Retrying {} failed requests", requests.len());
                let retry_result = self.send_requests(requests).await;
                if retry_result.is_some() {
                    // Still failed, return to retry later
                    return retry_result;
                }
            }
        }
        
        // Process new requests from the aggregator
        let mut guard = self.aggregator.lock().await;
        let mut requests = guard.get_batch();
        while !requests.is_empty() {
            if let Some(failed) = self.send_requests(requests).await {
                // Keep track of the failed batch
                failed_batch = Some(failed);
                // Stop processing more batches if we have a failure
                break;
            }

            requests = guard.get_batch();
        }
        failed_batch
    }

        // If we have requests from a previous failed attempt, try to send those first
        // if failed_requests.as_ref().is_some_and(|r| !r.is_empty()) {
        //     let retries = failed_requests.unwrap_or_default();
        //     debug!("Proxy Flusher | Retrying {} failed requests", retries.len());
        //     requests = retries;
        // } else {
        //     let mut aggregator = self.aggregator.lock().await;
        //     for pr in aggregator.get_batch() {
        //         requests.push(self.create_request(pr, self.config.proxy_intake.api_key.as_ref().unwrap()).await);
        //     }
        //     for request in requests {
        //         if let Some(failed) = Self::send_request(request).await {
        //             failed_batch = Some(failed);
        //             // Put requests back into the aggregator?
        //             break;
        //         }
        //     }
        // }
        // failed_batch

    async fn create_request(
        &self,
        request: ProxyRequest,
        api_key: &str,
    ) -> reqwest::RequestBuilder {
        let mut headers = request.headers.clone();

        // Remove headers that are not needed for the proxy request
        headers.remove("host");
        headers.remove("content-length");

        headers.extend(self.get_headers(api_key).await.clone());

        // TODO: Figure out what client to use / how data should be sent
        self.client
            .post(&request.target_url)
            .headers(headers)
            .timeout(std::time::Duration::from_secs(30))
            .body(request.body)
    }

    async fn send_requests(&self, requests: Vec<ProxyRequest>) -> Option<Vec<ProxyRequest>> {
        if requests.is_empty() {
            return None;
        }
        debug!("Proxy Flusher | Attempting to send {} requests", requests.len());

        let mut failed_requests = Vec::new();

        for request_payload in requests {
            // Clone the payload before creating the request builder (which consumes body)
            let cloned_payload = request_payload.clone();
            let request = self.create_request(request_payload, self.config.proxy_intake.api_key.as_ref().unwrap()).await;
            let time = std::time::Instant::now();
            match request.send().await {
                Ok(r) => {
                    let elapsed = time.elapsed();
                    let url = r.url().to_string();
                    let status = r.status();
                    let body = r.text().await;
                    if status == 202 || status == 200 {
                        debug!("Proxy Flusher | Successfully sent request {url} in {} ms", elapsed.as_millis());
                    } else {
                        error!("Proxy Flusher | Request failed with status {status}: {body:?}");
                        failed_requests.push(cloned_payload);
                    }
                }
                Err(e) => {
                    error!("Proxy Flusher | Failed to send request: {e:?}");
                    failed_requests.push(cloned_payload);
                }
            }
        }

        if failed_requests.is_empty() {
            None
        } else {
            Some(failed_requests)
        }
    }   

    // /// Given a `reqwest::RequestBuilder`, send the request and handle retries.
    // async fn send_request(request: reqwest::RequestBuilder) -> Result<(), Box<dyn std::error::Error + Send>> {
    //     debug!("Proxy Flusher | Attempting to send request");
    //     let mut attempts = 0;

    //     loop {
    //         attempts += 1;

    //         let Some(cloned_request) = request.try_clone() else {
    //             return Err(Box::new(std::io::Error::new(
    //                 std::io::ErrorKind::Other,
    //                 "can't clone proxy request",
    //             )));
    //         };

    //         let time = std::time::Instant::now();
    //         let response = cloned_request.send().await;
    //         let elapsed = time.elapsed();

    //         match response {
    //             Ok(r) => {
    //                 let url = r.url().to_string();
    //                 let status = r.status();
    //                 let body = r.text().await;
    //                 if status == 202 || status == 200 {
    //                     debug!(
    //                         "Proxy Flusher | Successfully sent request in {} ms to {url}",
    //                         elapsed.as_millis()
    //                     );
    //                 } else {
    //                     error!("Proxy Flusher | Request failed with status {status}: {body:?}");
    //                 }

    //                 return Ok(());
    //             }
    //             Err(e) => {
    //                 if attempts >= 3 {
    //                     error!(
    //                         "Proxy Flusher | Failed to send request after {} attempts: {:?}",
    //                         attempts, e
    //                     );

    //                     return Err(Box::new(FailedProxyRequestError {
    //                         request,
    //                         message: e.to_string(),
    //                     }));
    //                 }
    //             }
    //         }
    //     }
    // }
}

