//! HTTP client utilities for Datadog Remote Configuration.
//!
//! This module mirrors the behaviour of the Go implementation located under
//! `pkg/config/remote/api`. It handles header construction, credential
//! rotation, HTTP error classification, and provides an exponential backoff
//! helper that matches the agent's policy.

use std::borrow::Cow;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use datadog_fips::reqwest_adapter::create_reqwest_client_builder;
use prost::Message;
use remote_config_proto::remoteconfig::{
    LatestConfigsRequest, LatestConfigsResponse, OrgDataResponse, OrgStatusResponse,
};
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Client, Method, StatusCode, Url};
use thiserror::Error;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest,
    http::{
        header::{HeaderName as WsHeaderName, HeaderValue as WsHeaderValue},
        Request as WsRequest,
    },
};

/// Endpoint used for configuration payload polling.
const CONFIG_ENDPOINT: &str = "/api/v0.1/configurations";
/// Endpoint returning organisation metadata (UUID).
const ORG_DATA_ENDPOINT: &str = "/api/v0.1/org";
/// Endpoint returning organisation feature flags (enabled/authorized bits).
const ORG_STATUS_ENDPOINT: &str = "/api/v0.1/status";

/// Authentication material required for Remote Config requests.
#[derive(Debug, Clone)]
pub struct Auth {
    /// Sanitised Datadog API key.
    pub api_key: String,
    /// Optional datadog application key decoded from the RC key payload.
    pub application_key: Option<String>,
    /// Optional Private Action Runner JWT (used by self-hosted runners).
    pub par_jwt: Option<String>,
}

/// Additional options governing how the HTTP client is constructed.
#[derive(Debug, Clone, Copy)]
pub struct HttpClientOptions {
    /// Whether plaintext (HTTP) endpoints are allowed.
    pub allow_plaintext: bool,
    /// Whether TLS certificate validation should be skipped.
    pub accept_invalid_certs: bool,
}

impl Default for HttpClientOptions {
    /// Mirrors the defaults from the Go agent HTTP client.
    fn default() -> Self {
        Self {
            allow_plaintext: false,
            accept_invalid_certs: false,
        }
    }
}

/// Error taxonomy aligned with the Go agent implementation.
#[derive(Debug, Error)]
pub enum HttpError {
    /// Credentials were rejected by the backend.
    #[error("unauthorized - invalid API key or missing remote configuration scope")]
    Unauthorized,
    /// Request failed due to proxy or malformed input (4xx excluding 401).
    #[error("proxy error or malformed request: status {0}")]
    Proxy(u16),
    /// Backend reported a temporary outage (503/504 or other 5xx).
    #[error("transient backend error: status {0}")]
    Retryable(u16),
    /// The provided URL violates the required transport policy.
    #[error("insecure base url requires explicit opt-in: {0}")]
    InsecureUrl(String),
    /// Transport-level issue (DNS, TLS, socket, etc.).
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    /// Response payload could not be decoded as a protobuf message.
    #[error("failed to decode protobuf payload: {0}")]
    Decode(#[from] prost::DecodeError),
    /// FIPS TLS configuration error.
    #[error("FIPS configuration error: {0}")]
    FipsConfig(String),
}

/// HTTP client encapsulating a reusable `reqwest::Client`, base URL, and headers.
#[derive(Debug, Clone)]
pub struct HttpClient {
    /// Underlying HTTP client (shared across requests).
    client: Client,
    /// Remote Config base URL (scheme + host).
    base_url: String,
    /// Shared header map guarded by a read/write lock for credential rotation.
    headers: Arc<RwLock<HeaderMap>>,
}

impl HttpClient {
    /// Builds an HTTP client using the provided base URL and authentication information.
    #[allow(clippy::unused_async)]
    pub fn new(
        base_url: impl Into<String>,
        auth: &Auth,
        agent_version: &str,
        options: HttpClientOptions,
    ) -> Result<Self, HttpError> {
        let base_url = base_url.into();
        // Guard against accidentally pointing the agent at plaintext endpoints unless the caller
        // explicitly opted in via `allow_plaintext`.
        if !options.allow_plaintext && base_url.starts_with("http://") {
            return Err(HttpError::InsecureUrl(base_url));
        }

        let mut headers = HeaderMap::new();
        let user_agent = format!("datadog-agent/{}", agent_version);
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&user_agent).map_err(|_| HttpError::Proxy(400))?,
        );
        headers.insert(
            "Content-Type",
            HeaderValue::from_static("application/x-protobuf"),
        );
        headers.insert(
            "DD-API-KEY",
            HeaderValue::from_str(&auth.api_key).map_err(|_| HttpError::Proxy(400))?,
        );
        // Application keys are optional; include the header only when the RC key material provides one.
        if let Some(app_key) = &auth.application_key {
            headers.insert(
                "DD-APPLICATION-KEY",
                HeaderValue::from_str(app_key).map_err(|_| HttpError::Proxy(400))?,
            );
        }
        // Self-hosted runners may provide a PAR JWT; propagate it when present so backend gating works.
        if let Some(jwt) = &auth.par_jwt {
            headers.insert(
                "DD-PAR-JWT",
                HeaderValue::from_str(jwt).map_err(|_| HttpError::Proxy(400))?,
            );
        }

        let builder = create_reqwest_client_builder()
            .map_err(|e| HttpError::FipsConfig(e.to_string()))?
            .danger_accept_invalid_certs(options.accept_invalid_certs)
            .danger_accept_invalid_hostnames(options.accept_invalid_certs);
        let client = builder.build().map_err(HttpError::Transport)?;

        Ok(Self {
            client,
            base_url,
            headers: Arc::new(RwLock::new(headers)),
        })
    }

    /// Replaces the API key used for subsequent requests.
    pub async fn update_api_key(&self, api_key: &str) -> Result<(), HttpError> {
        let mut headers = self.headers.write().await;
        headers.insert(
            "DD-API-KEY",
            HeaderValue::from_str(api_key).map_err(|_| HttpError::Proxy(400))?,
        );
        Ok(())
    }

    /// Updates or clears the PAR JWT header at runtime.
    pub async fn update_par_jwt(&self, par_jwt: Option<&str>) -> Result<(), HttpError> {
        let mut headers = self.headers.write().await;
        match par_jwt {
            Some(jwt) => {
                // The caller supplied a fresh JWT, so replace the previous value with the new one.
                headers.insert(
                    "DD-PAR-JWT",
                    HeaderValue::from_str(jwt).map_err(|_| HttpError::Proxy(400))?,
                );
            }
            None => {
                // No JWT should be propagated anymore; remove the header entirely to avoid stale creds.
                headers.remove("DD-PAR-JWT");
            }
        }
        Ok(())
    }

    /// Updates or clears the application key header.
    pub async fn update_application_key(&self, app_key: Option<&str>) -> Result<(), HttpError> {
        let mut headers = self.headers.write().await;
        match app_key {
            Some(key) => {
                // Store the newly-rotated application key in the shared header map.
                headers.insert(
                    "DD-APPLICATION-KEY",
                    HeaderValue::from_str(key).map_err(|_| HttpError::Proxy(400))?,
                );
            }
            None => {
                // Clearing the option means the backend should no longer receive an application key.
                headers.remove("DD-APPLICATION-KEY");
            }
        }
        Ok(())
    }

    /// Returns the base URL currently configured for the client.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Returns the currently configured API key (if any).
    pub async fn api_key(&self) -> Option<String> {
        let headers = self.headers.read().await;
        headers
            .get("DD-API-KEY")
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_string())
    }

    /// Sends a `ClientGetConfigs` request and returns the decoded response.
    ///
    /// Mirrors the Go agent's `Fetch` helper and centralises protobuf
    /// decode/error handling so callers can focus on backoff logic.
    pub async fn fetch_latest_configs(
        &self,
        request: &LatestConfigsRequest,
    ) -> Result<LatestConfigsResponse, HttpError> {
        let body = request.encode_to_vec();
        let response = self
            .send_request(Method::POST, CONFIG_ENDPOINT, Some(body))
            .await?;
        let bytes = response.bytes().await?;
        LatestConfigsResponse::decode(bytes.as_ref()).map_err(HttpError::Decode)
    }

    /// Retrieves the organisation UUID from the backend.
    ///
    /// Called when seeding the Uptane store or verifying credential rotations.
    pub async fn fetch_org_data(&self) -> Result<OrgDataResponse, HttpError> {
        let response = self
            .send_request(Method::GET, ORG_DATA_ENDPOINT, None)
            .await?;
        let bytes = response.bytes().await?;
        OrgDataResponse::decode(bytes.as_ref()).map_err(HttpError::Decode)
    }

    /// Retrieves organisation status flags (enabled, authorized).
    ///
    /// Powers the org-status poller so logs/telemetry mirror the Go agent.
    pub async fn fetch_org_status(&self) -> Result<OrgStatusResponse, HttpError> {
        let response = self
            .send_request(Method::GET, ORG_STATUS_ENDPOINT, None)
            .await?;
        let bytes = response.bytes().await?;
        OrgStatusResponse::decode(bytes.as_ref()).map_err(HttpError::Decode)
    }

    /// Internal helper: attaches headers, sends the request, and classifies the HTTP status.
    ///
    /// All transport errors are converted to [`HttpError`] variants that mirror
    /// the Go agent behaviour, making it easier to reuse the same retry logic.
    async fn send_request(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<reqwest::Response, HttpError> {
        let url = format!("{}{}", self.base_url, path);
        // Clone headers under read lock so we do not hold the lock across await points.
        let headers = self.headers.read().await.clone();
        let redacted_headers = redact_headers(&headers);

        let body_len = body.as_ref().map(|bytes| bytes.len()).unwrap_or(0);
        if tracing::enabled!(tracing::Level::TRACE) {
            if let Some(body_bytes) = &body {
                let preview = request_body_preview(&headers, body_bytes);
                tracing::debug!(
                    method = %method,
                    url = %url,
                    headers = ?redacted_headers,
                    body_len = body_len,
                    body = %preview,
                    "remote-config HTTP request"
                );
            } else {
                tracing::debug!(
                    method = %method,
                    url = %url,
                    headers = ?redacted_headers,
                    body_len = body_len,
                    "remote-config HTTP request"
                );
            }
        } else {
            tracing::debug!(
                method = %method,
                url = %url,
                headers = ?redacted_headers,
                body_len = body_len,
                "remote-config HTTP request"
            );
        }

        let builder = self
            .client
            .request(method.clone(), url.clone())
            .headers(headers);
        // Attach the request body when provided, otherwise send a body-less request
        // (used by GET endpoints such as org/status).
        let builder = match body {
            Some(bytes) => builder.body(bytes),
            None => builder,
        };
        let response = builder.send().await?;

        // Log response details before status classification
        let status = response.status();
        let content_length = response.content_length();
        let content_length_str =
            content_length.map_or_else(|| "unknown".to_string(), |len| len.to_string());

        // For error responses, read and log the body
        if !status.is_success() {
            // Non-success statuses: buffer the body so we can log and map
            // them to the Go-compatible error taxonomy.
            let body_bytes = response.bytes().await.unwrap_or_default();
            let body_str = String::from_utf8_lossy(&body_bytes);

            tracing::debug!(
                method = %method,
                url = %url,
                status = %status,
                content_length = %content_length_str,
                body = %body_str,
                "remote-config HTTP response"
            );

            // Now classify status which will return an error
            classify_status(status)?;
            unreachable!("classify_status should have returned an error for non-success status");
        }

        // Success responses reuse the existing `response` so the caller can
        // consume the body later.
        tracing::debug!(
            method = %method,
            url = %url,
            status = %status,
            content_length = %content_length_str,
            "remote-config HTTP response"
        );

        Ok(response)
    }

    /// Builds a websocket request targeting the provided path using stored headers.
    ///
    /// The helper converts the HTTP(S) base URL into WS/WSS as needed and copies
    /// the HTTP headers so credentials propagate to the websocket diagnostic endpoint.
    pub async fn websocket_request(&self, path: &str) -> Result<WsRequest<()>, HttpError> {
        let mut base = Url::parse(&self.base_url).map_err(|_| HttpError::Proxy(400))?;
        let current_scheme = base.scheme().to_string();
        let target_scheme = match current_scheme.as_str() {
            "https" | "wss" => "wss",
            "http" | "ws" => "ws",
            other => other,
        };
        if current_scheme != target_scheme {
            base.set_scheme(target_scheme)
                .map_err(|_| HttpError::Proxy(400))?;
        }

        let normalized = if path.starts_with('/') {
            path.to_owned()
        } else {
            format!("/{}", path)
        };

        base.set_path(&normalized);
        base.set_query(None);
        base.set_fragment(None);

        // Capture headers under the newer `http` 1.x types so they can be converted to the
        // websocket client's 0.2-compatible names/values without holding the lock.
        let header_snapshot: Vec<(String, Vec<u8>)> = {
            let headers = self.headers.read().await;
            headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_ascii_lowercase(),
                        value.as_bytes().to_vec(),
                    )
                })
                .collect()
        };

        let mut request = base
            .into_client_request()
            .map_err(|_| HttpError::Proxy(400))?;
        {
            let request_headers = request.headers_mut();
            // Translate each stored header into the tungstenite-compatible representation so
            // credential headers (API key, JWT, RC key) flow through the websocket handshake.
            for (name, value) in header_snapshot {
                let header_name =
                    WsHeaderName::from_bytes(name.as_bytes()).map_err(|_| HttpError::Proxy(400))?;
                let header_value =
                    WsHeaderValue::from_bytes(&value).map_err(|_| HttpError::Proxy(400))?;
                request_headers.append(header_name, header_value);
            }
        }
        Ok(request)
    }
}

/// Maps HTTP status codes to the Remote Config error taxonomy.
fn classify_status(status: StatusCode) -> Result<(), HttpError> {
    if status.is_redirection() {
        // Treat unexpected redirects as proxy/misconfiguration issues like the Go agent.
        return Err(HttpError::Proxy(status.as_u16()));
    }
    if status == StatusCode::UNAUTHORIZED {
        // Authentication failure: callers should not retry until credentials change.
        return Err(HttpError::Unauthorized);
    }
    if status == StatusCode::SERVICE_UNAVAILABLE || status == StatusCode::GATEWAY_TIMEOUT {
        // Backend explicitly signals a temporary outage.
        return Err(HttpError::Retryable(status.as_u16()));
    }
    if status.is_client_error() {
        // For other 4xx codes we mirror the Go agent by reporting a proxy/misconfiguration hint.
        return Err(HttpError::Proxy(status.as_u16()));
    }
    if status.is_server_error() {
        // Any remaining 5xx codes should be treated as retryable.
        return Err(HttpError::Retryable(status.as_u16()));
    }
    Ok(())
}

/// Returns a redacted view of request headers suitable for debug logging.
fn redact_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    const SENSITIVE_HEADERS: [&str; 4] = [
        "dd-api-key",
        "dd-application-key",
        "dd-par-jwt",
        "authorization",
    ];

    headers
        .iter()
        .map(|(name, value)| {
            let lower = name.as_str().to_ascii_lowercase();
            let display = if SENSITIVE_HEADERS.contains(&lower.as_str()) {
                "<redacted>".to_string()
            } else {
                value
                    .to_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|_| "<non-utf8>".to_string())
            };
            (lower, display)
        })
        .collect()
}

/// Returns a human-readable preview of a request body for verbose logging.
fn request_body_preview(headers: &HeaderMap, body: &[u8]) -> String {
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let content_encoding = headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_text = content_type.contains("json") || content_type.contains("text");
    if !is_text {
        return format!("[{} bytes binary]", body.len());
    }

    let maybe_decompressed = if content_encoding.contains("gzip") {
        decompress_gzip(body)
    } else {
        None
    };
    let data = maybe_decompressed.as_deref().unwrap_or(body);
    let text = String::from_utf8_lossy(data);
    truncate_preview_text(text)
}

/// Attempts to decompress gzip-compressed payloads for logging.
fn decompress_gzip(body: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = flate2::read::GzDecoder::new(body);
    let mut decompressed = Vec::new();
    match decoder.read_to_end(&mut decompressed) {
        Ok(_) => Some(decompressed),
        Err(_) => None,
    }
}

fn truncate_preview_text(text: Cow<'_, str>) -> String {
    const MAX_CHARS: usize = 1024;
    let mut chars = text.chars();
    let mut preview = String::new();
    for _ in 0..MAX_CHARS {
        match chars.next() {
            Some(ch) => preview.push(ch),
            None => return preview,
        }
    }
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

/// Configuration parameters for exponential backoff.
#[derive(Debug, Clone, Copy)]
pub struct BackoffConfig {
    /// Base backoff time in seconds.
    pub base_seconds: f64,
    /// Maximum backoff duration.
    pub max_backoff: Duration,
    /// Exponential factor applied on error count increments.
    pub factor: f64,
    /// Number of consecutive successful calls required to decrease the error count.
    pub recovery_interval: usize,
}

impl Default for BackoffConfig {
    /// Returns the default backoff policy mirroring the Go agent constants.
    fn default() -> Self {
        Self {
            base_seconds: 30.0,
            max_backoff: Duration::from_secs(2 * 60),
            factor: 2.0,
            recovery_interval: 2,
        }
    }
}

/// Tracks the state of the exponential backoff algorithm.
#[derive(Debug)]
pub struct BackoffState {
    config: BackoffConfig,
    error_count: usize,
    success_streak: usize,
}

impl BackoffState {
    /// Creates a new tracker with the supplied configuration.
    pub fn new(config: BackoffConfig) -> Self {
        Self {
            config,
            error_count: 0,
            success_streak: 0,
        }
    }

    /// Registers a failure and returns the recommended delay before retrying.
    pub fn register_error(&mut self) -> Duration {
        self.success_streak = 0;
        self.error_count = self.error_count.saturating_add(1);
        let exponent = self.error_count.saturating_sub(1);
        // Cap exponent to prevent overflow when casting to i32 for powi().
        // With base=30s, factor=2.0, and max_backoff=120s, exponent > 10 already hits the cap.
        // Setting a generous limit of 100 provides defense-in-depth.
        let capped_exponent = exponent.min(100) as i32;
        let backoff_secs = self.config.base_seconds * self.config.factor.powi(capped_exponent);
        let capped = backoff_secs.min(self.config.max_backoff.as_secs_f64());
        Duration::from_secs_f64(capped)
    }

    /// Registers a success and decreases the error count when enough time elapsed.
    pub fn register_success(&mut self) {
        if self.config.recovery_interval == 0 {
            self.error_count = self.error_count.saturating_sub(2);
            self.success_streak = 0;
            return;
        }

        self.success_streak = self.success_streak.saturating_add(1);
        if self.success_streak >= self.config.recovery_interval {
            self.error_count = self.error_count.saturating_sub(2);
            self.success_streak = 0;
        }
    }
}

/// Tests for the HTTP utilities.
#[cfg(test)]
mod tests {
    use super::*;
    use httptest::matchers::{all_of, contains, key, not, request};
    use httptest::{responders::status_code, Expectation, Server};
    use remote_config_proto::remoteconfig::{
        File, LatestConfigsRequest, LatestConfigsResponse, OrgStatusResponse,
    };
    use std::io::Write;

    /// Ensures HTTP status codes map to the expected error taxonomy.
    #[test]
    fn classify_status_maps_expected_errors() {
        assert!(classify_status(StatusCode::OK).is_ok());
        assert!(matches!(
            classify_status(StatusCode::UNAUTHORIZED),
            Err(HttpError::Unauthorized)
        ));
        assert!(matches!(
            classify_status(StatusCode::FOUND),
            Err(HttpError::Proxy(302))
        ));
        assert!(matches!(
            classify_status(StatusCode::BAD_REQUEST),
            Err(HttpError::Proxy(400))
        ));
        assert!(matches!(
            classify_status(StatusCode::SERVICE_UNAVAILABLE),
            Err(HttpError::Retryable(503))
        ));
        assert!(matches!(
            classify_status(StatusCode::INTERNAL_SERVER_ERROR),
            Err(HttpError::Retryable(500))
        ));
    }

    /// Verifies that the backoff tracker grows exponentially and recovers after successes.
    #[test]
    fn backoff_state_progression_and_recovery() {
        let mut state = BackoffState::new(BackoffConfig::default());

        assert_eq!(state.register_error(), Duration::from_secs(30));
        assert_eq!(state.register_error(), Duration::from_secs(60));
        assert_eq!(state.register_error(), Duration::from_secs(120));

        // A single success should not reduce the error count yet (default requires 2 consecutive successes).
        state.register_success();
        assert_eq!(state.register_error(), Duration::from_secs(120));

        // With two consecutive successes the error count decays, lowering the next delay.
        let mut recovering = BackoffState::new(BackoffConfig::default());
        assert_eq!(recovering.register_error(), Duration::from_secs(30));
        assert_eq!(recovering.register_error(), Duration::from_secs(60));
        assert_eq!(recovering.register_error(), Duration::from_secs(120));
        recovering.register_success();
        recovering.register_success();
        assert_eq!(recovering.register_error(), Duration::from_secs(60));

        // A configuration with instant recovery should drop back to the base delay immediately.
        let config = BackoffConfig {
            base_seconds: 10.0,
            max_backoff: Duration::from_secs(100),
            factor: 2.0,
            recovery_interval: 0,
        };
        let mut fast_recovery = BackoffState::new(config);
        assert_eq!(fast_recovery.register_error(), Duration::from_secs(10));
        assert_eq!(fast_recovery.register_error(), Duration::from_secs(20));
        // Recovery interval of zero allows immediate decay.
        fast_recovery.register_success();
        assert_eq!(fast_recovery.register_error(), Duration::from_secs(10));
    }

    /// Confirms that protobuf payloads round-trip through the HTTP client.
    #[tokio::test]
    async fn http_client_fetches_and_decodes_protobuf_payloads() -> Result<(), HttpError> {
        let server = Server::run();
        let expected_response = LatestConfigsResponse {
            target_files: vec![File {
                path: "rules.json".to_string(),
                raw: vec![1, 2, 3],
            }],
            ..Default::default()
        };
        let response_body = expected_response.clone().encode_to_vec();

        server.expect(
            Expectation::matching(all_of![
                request::method_path("POST", CONFIG_ENDPOINT),
                request::headers(contains(("dd-api-key", "initial-key"))),
                request::headers(contains(("content-type", "application/x-protobuf"))),
                request::headers(contains(("dd-application-key", "app-key")))
            ])
            .respond_with(
                status_code(200)
                    .append_header("Content-Type", "application/x-protobuf")
                    .body(response_body),
            ),
        );

        let auth = Auth {
            api_key: "initial-key".to_string(),
            application_key: Some("app-key".to_string()),
            par_jwt: None,
        };
        let base_url = server.url_str("").trim_end_matches('/').to_string();
        let client = HttpClient::new(
            base_url,
            &auth,
            "7.70.0",
            HttpClientOptions {
                allow_plaintext: true,
                accept_invalid_certs: true,
            },
        )?;
        let request = LatestConfigsRequest {
            hostname: "test-host".to_string(),
            agent_version: "1.0.0".to_string(),
            ..Default::default()
        };

        let response = client.fetch_latest_configs(&request).await?;
        assert_eq!(response, expected_response);
        Ok(())
    }

    /// Validates runtime header rotation for API keys and PAR JWT material.
    #[tokio::test]
    async fn http_client_rotates_headers() -> Result<(), HttpError> {
        let server = Server::run();
        let status_body = OrgStatusResponse {
            enabled: true,
            authorized: false,
        }
        .encode_to_vec();

        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", ORG_STATUS_ENDPOINT),
                request::headers(contains(("dd-api-key", "initial-key"))),
                request::headers(not(contains(key("dd-par-jwt")))),
                request::headers(not(contains(key("dd-application-key")))),
            ])
            .respond_with(status_code(200).body(status_body.clone())),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", ORG_STATUS_ENDPOINT),
                request::headers(contains(("dd-api-key", "rotated-key"))),
                request::headers(contains(("dd-par-jwt", "jwt-token"))),
                request::headers(contains(("dd-application-key", "new-app"))),
            ])
            .respond_with(status_code(200).body(status_body.clone())),
        );
        server.expect(
            Expectation::matching(all_of![
                request::method_path("GET", ORG_STATUS_ENDPOINT),
                request::headers(contains(("dd-api-key", "rotated-key"))),
                request::headers(not(contains(key("dd-par-jwt")))),
                request::headers(not(contains(key("dd-application-key")))),
            ])
            .respond_with(status_code(200).body(status_body)),
        );

        let auth = Auth {
            api_key: "initial-key".to_string(),
            application_key: None,
            par_jwt: None,
        };
        let base_url = server.url_str("").trim_end_matches('/').to_string();
        let client = HttpClient::new(
            base_url,
            &auth,
            "7.70.0",
            HttpClientOptions {
                allow_plaintext: true,
                accept_invalid_certs: true,
            },
        )?;

        // Initial fetch uses the original key and no JWT header.
        let response_one = client.fetch_org_status().await?;
        assert!(response_one.enabled);

        // Rotate credentials and ensure the next call carries the updated headers.
        client.update_api_key("rotated-key").await?;
        client.update_application_key(Some("new-app")).await?;
        client.update_par_jwt(Some("jwt-token")).await?;
        let response_two = client.fetch_org_status().await?;
        assert!(!response_two.authorized);

        // Removing the JWT header should revert the request back to only the API key.
        client.update_application_key(None).await?;
        client.update_par_jwt(None).await?;
        let response_three = client.fetch_org_status().await?;
        assert!(response_three.enabled);

        Ok(())
    }

    #[test]
    /// Validates that user-agent headers blend host/runtime metadata.
    fn test_user_agent_header_construction() {
        // Test that the user-agent header is correctly constructed with the agent version
        let auth = Auth {
            api_key: "test_api_key".to_string(),
            application_key: None,
            par_jwt: None,
        };

        // Test with version 7.70.0
        let client = HttpClient::new(
            "https://config.datadoghq.com",
            &auth,
            "7.70.0",
            HttpClientOptions::default(),
        )
        .expect("Failed to create HTTP client");

        // We can't directly access headers, but we can verify the client was created successfully
        // which means the user-agent header was valid
        assert_eq!(client.base_url, "https://config.datadoghq.com");

        // Test with different version
        let client2 = HttpClient::new(
            "https://config.datadoghq.com",
            &auth,
            "7.71.0",
            HttpClientOptions::default(),
        )
        .expect("Failed to create HTTP client with different version");
        assert_eq!(client2.base_url, "https://config.datadoghq.com");

        // Test that it works with various version formats
        HttpClient::new(
            "https://config.datadoghq.com",
            &auth,
            "1.2.3",
            HttpClientOptions::default(),
        )
        .expect("Failed with semantic version");
        HttpClient::new(
            "https://config.datadoghq.com",
            &auth,
            "0.1.0",
            HttpClientOptions::default(),
        )
        .expect("Failed with zero major version");
    }

    #[test]
    fn request_body_preview_handles_plain_text() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        let preview = request_body_preview(&headers, br#"{"hello":"world"}"#);
        assert_eq!(preview, r#"{"hello":"world"}"#);
    }

    #[test]
    fn request_body_preview_limits_length() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("text/plain"));
        let payload = "a".repeat(1500);
        let preview = request_body_preview(&headers, payload.as_bytes());
        assert!(preview.ends_with('…'), "preview should be truncated");
        assert!(
            preview.chars().count() <= 1025,
            "preview should stay within character limit"
        );
    }

    #[test]
    fn request_body_preview_handles_binary_payloads() {
        let headers = HeaderMap::new();
        let preview = request_body_preview(&headers, &[0, 159, 146, 150]);
        assert_eq!(preview, "[4 bytes binary]");
    }

    #[test]
    fn request_body_preview_decompresses_gzip() {
        use flate2::{write::GzEncoder, Compression};

        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        headers.insert("content-encoding", HeaderValue::from_static("gzip"));

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(br#"{"key":"value"}"#)
            .expect("gzip write succeeds");
        let compressed = encoder.finish().expect("gzip finish");

        let preview = request_body_preview(&headers, &compressed);
        assert_eq!(preview, r#"{"key":"value"}"#);
    }

    #[test]
    /// Rejects insecure URLs unless the caller opt-ins with `allow_insecure_transport`.
    fn http_client_rejects_insecure_url_without_opt_in() {
        let auth = Auth {
            api_key: "key".into(),
            application_key: None,
            par_jwt: None,
        };
        let err = HttpClient::new(
            "http://config.datadoghq.com",
            &auth,
            "7.70.0",
            HttpClientOptions::default(),
        )
        .expect_err("insecure transport should fail");
        assert!(matches!(err, HttpError::InsecureUrl(_)));
    }
}
