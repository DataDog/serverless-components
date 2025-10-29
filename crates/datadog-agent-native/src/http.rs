//! HTTP client utilities and Axum handlers.
//!
//! This module provides HTTP infrastructure for the agent:
//! - **HTTP client creation**: Configured reqwest clients with proxy and protocol support
//! - **Request handling**: Utilities for extracting request bodies and headers
//! - **Error handlers**: Standard HTTP error responses
//!
//! # HTTP Client Configuration
//!
//! The HTTP client supports:
//! - **FIPS mode**: Uses FIPS-compliant TLS when enabled
//! - **HTTP/2 with h2c**: Prior knowledge HTTP/2 with keep-alive
//! - **HTTP/1**: Fallback for proxy compatibility
//! - **Proxy support**: HTTPS proxy configuration via environment
//! - **Timeouts**: Configurable flush timeout from agent config
//! - **Connection pooling**: Idle timeout and TCP keep-alive
//!
//! # Protocol Selection
//!
//! The client automatically selects HTTP/1 or HTTP/2:
//! - **Explicit**: Set via `DD_HTTP_PROTOCOL=http1` or `http2`
//! - **Proxy**: Defaults to HTTP/1 when proxy is configured (better compatibility)
//! - **Default**: Uses HTTP/2 with prior knowledge (h2c) for direct connections
//!
//! # Example
//!
//! ```rust,ignore
//! use datadog_agent_native::http::get_client;
//!
//! let client = get_client(&config);
//! let response = client.post("https://api.datadoghq.com/v1/traces")
//!     .body(payload)
//!     .send()
//!     .await?;
//! ```

use crate::config;
use axum::{
    extract::{FromRequest, Request},
    http::{self, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use core::time::Duration;
use datadog_fips::reqwest_adapter::create_reqwest_client_builder;
use std::sync::Arc;
use std::{collections::HashMap, error::Error};
use tracing::error;

/// Creates a configured HTTP client with proxy and protocol support.
///
/// This function builds a reqwest client configured according to the agent's
/// settings, including flush timeout, proxy configuration, and HTTP protocol.
///
/// # Arguments
///
/// * `config` - Agent configuration containing HTTP settings
///
/// # Returns
///
/// A configured `reqwest::Client` ready for use.
///
/// # Error Handling
///
/// If proxy configuration is invalid, logs an error and returns a default
/// client (without proxy). This ensures the agent can continue operating
/// even with misconfigured proxy settings.
///
/// # Example
///
/// ```rust,ignore
/// let client = get_client(&config);
/// let response = client.post(url).body(payload).send().await?;
/// ```
#[must_use]
pub fn get_client(config: &Arc<config::Config>) -> reqwest::Client {
    match build_client(config) {
        Ok(client) => client,
        Err(e) => {
            error!(
                "Unable to parse proxy configuration: {}, falling back to direct connection",
                e
            );
            match build_client_without_proxy(config) {
                Ok(client) => client,
                Err(inner) => {
                    error!(
                        "Failed to build HTTP client without proxy: {}, using reqwest defaults",
                        inner
                    );
                    reqwest::Client::new()
                }
            }
        }
    }
}

/// Builds a reqwest HTTP client with full configuration.
///
/// This internal function constructs the client with all settings from config:
/// - FIPS-compliant TLS (if enabled via feature flag)
/// - Timeout from `flush_timeout` config
/// - Connection pooling with 270s idle timeout
/// - TCP keep-alive (120s)
/// - HTTP/2 or HTTP/1 based on config and proxy presence
/// - HTTPS proxy support
///
/// # Arguments
///
/// * `config` - Agent configuration
///
/// # Returns
///
/// * `Ok(client)` - Fully configured HTTP client
/// * `Err` - If proxy URL is invalid or TLS setup fails
///
/// # Protocol Selection Logic
///
/// - If `DD_HTTP_PROTOCOL=http1`: Always use HTTP/1
/// - If proxy configured + no protocol set: Use HTTP/1 (better proxy compat)
/// - Otherwise: Use HTTP/2 with prior knowledge (h2c)
fn build_client(config: &Arc<config::Config>) -> Result<reqwest::Client, Box<dyn Error>> {
    build_client_inner(config, true)
}

fn build_client_without_proxy(
    config: &Arc<config::Config>,
) -> Result<reqwest::Client, Box<dyn Error>> {
    build_client_inner(config, false)
}

fn build_client_inner(
    config: &Arc<config::Config>,
    allow_proxy: bool,
) -> Result<reqwest::Client, Box<dyn Error>> {
    // Start with FIPS-compliant client builder (if FIPS feature enabled)
    let mut client = create_reqwest_client_builder()?
        .timeout(Duration::from_secs(config.flush_timeout))
        .pool_idle_timeout(Some(Duration::from_secs(270)))
        // Enable TCP keepalive to detect dead connections
        // Decision: 120s interval balances connection health vs network overhead
        .tcp_keepalive(Some(Duration::from_secs(120)));

    // Determine HTTP protocol version based on config and proxy
    let proxy_configured = allow_proxy && config.proxy_https.is_some();

    let should_use_http1 = match &config.http_protocol {
        // Explicitly set to "http1" - honor user preference
        // Decision: Allow explicit protocol selection for compatibility
        Some(val) => val == "http1",
        // Not set - use HTTP/1 if proxy is configured, otherwise HTTP/2
        // Decision: HTTP/1 has better proxy compatibility (many proxies don't support h2c)
        None => proxy_configured,
    };

    // Configure HTTP/2 with prior knowledge (h2c) if not using HTTP/1
    if !should_use_http1 {
        // Decision: Use prior knowledge to avoid upgrade dance (faster)
        client = client
            .http2_prior_knowledge()
            // Keep-alive pings every 10s to maintain connection
            .http2_keep_alive_interval(Some(Duration::from_secs(10)))
            // Keep pinging even when idle (important for long-lived connections)
            .http2_keep_alive_while_idle(true)
            // 1000s timeout for ping responses (very generous)
            .http2_keep_alive_timeout(Duration::from_secs(1000));
    }

    // Apply HTTPS proxy if configured
    // This covers DD_PROXY_HTTPS and HTTPS_PROXY environment variables
    if allow_proxy {
        if let Some(https_uri) = &config.proxy_https {
            // Decision: Only HTTPS proxy supported (not HTTP proxy for HTTPS traffic)
            let proxy = reqwest::Proxy::https(https_uri.clone())?;
            client = client.proxy(proxy);
        }
    }

    Ok(client.build()?)
}

/// Standard 404 Not Found handler for Axum.
///
/// Returns a simple 404 response with "Not Found" body.
///
/// # Returns
///
/// An Axum response with status 404.
///
/// # Example
///
/// ```rust,ignore
/// Router::new().fallback(handler_not_found)
/// ```
pub async fn handler_not_found() -> Response {
    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

/// Extracts the request parts and body bytes from an Axum request.
///
/// This utility function splits an Axum request into its parts (method, headers, etc.)
/// and the body as `Bytes`. Useful for middleware that needs to inspect both.
///
/// # Arguments
///
/// * `request` - The incoming Axum request
///
/// # Returns
///
/// * `Ok((parts, bytes))` - Request parts and body bytes
/// * `Err` - If body extraction fails
///
/// # Example
///
/// ```rust,ignore
/// async fn middleware(request: Request) -> Result<Response, Box<dyn Error>> {
///     let (parts, body) = extract_request_body(request).await?;
///     println!("Method: {}, Body size: {}", parts.method, body.len());
///     // ... process request ...
///     Ok(response)
/// }
/// ```
pub async fn extract_request_body(
    request: Request,
) -> Result<(http::request::Parts, Bytes), Box<dyn std::error::Error>> {
    // Split request into parts and body
    let (parts, body) = request.into_parts();

    // Extract body as Bytes using Axum's FromRequest trait
    // Decision: Clone parts to allow reusing them after body extraction
    let bytes = Bytes::from_request(Request::from_parts(parts.clone(), body), &()).await?;

    Ok((parts, bytes))
}

/// Converts Axum `HeaderMap` to a standard `HashMap<String, String>`.
///
/// This utility converts HTTP headers to a simple map for easier manipulation.
/// Header values that aren't valid UTF-8 are replaced with empty strings.
///
/// # Arguments
///
/// * `headers` - The Axum header map
///
/// # Returns
///
/// A `HashMap` with lowercase header names as keys and string values.
///
/// # Example
///
/// ```rust,ignore
/// let headers_map = headers_to_map(&request.headers());
/// if let Some(content_type) = headers_map.get("content-type") {
///     println!("Content-Type: {}", content_type);
/// }
/// ```
///
/// # Note
///
/// Header names are lowercased by Axum. Non-UTF-8 header values are converted
/// to empty strings rather than failing.
#[must_use]
pub fn headers_to_map(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                // Decision: Use empty string for non-UTF-8 headers rather than panic
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}
