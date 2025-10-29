//! Framework-agnostic HTTP types for AppSec security analysis.
//!
//! This module provides standard HTTP request/response types that can be populated
//! from any web framework (Axum, Actix, Rocket, Hyper, etc.) for security analysis.
//!
//! # Design Goals
//!
//! - **Framework Agnostic**: Works with any Rust web framework
//! - **Zero Copy**: References to request/response data where possible
//! - **Complete Data**: Captures all security-relevant HTTP components
//! - **Easy Integration**: Simple to populate from framework-specific types
//!
//! # Usage Pattern
//!
//! ```text
//! Web Framework Request
//!         │
//!         ├─> Extract data
//!         │
//!         v
//!   HttpRequest/HttpTransaction
//!         │
//!         ├─> Security analysis
//!         │
//!         v
//!   AppSec Processor (WAF)
//! ```
//!
//! # Example Integration
//!
//! ```rust,ignore
//! use datadog_agent_native::appsec::processor::http::*;
//!
//! // In your web framework middleware/handler:
//! async fn security_middleware(req: FrameworkRequest) -> Response {
//!     // Convert framework request to HttpRequest
//!     let http_req = HttpRequest {
//!         method: req.method().to_string(),
//!         uri: req.uri().path().to_string(),
//!         route: Some(route_pattern),
//!         client_ip: extract_client_ip(&req),
//!         headers: extract_headers(&req),
//!         // ... populate other fields
//!         ..Default::default()
//!     };
//!
//!     // Create transaction and analyze
//!     let transaction = HttpTransaction::from_request(http_req);
//!     appsec_processor.process_http_request("req-123", &transaction).await;
//!
//!     // Process request...
//!     let response = handle_request(req).await;
//!
//!     // Analyze response
//!     let http_resp = HttpResponse {
//!         status_code: response.status().as_u16() as i64,
//!         headers: extract_response_headers(&response),
//!         body: Some(response_body_bytes),
//!         ..Default::default()
//!     };
//!
//!     transaction.with_response(http_resp);
//!     appsec_processor.process_http_response("req-123", &transaction).await;
//!
//!     response
//! }
//! ```

use std::collections::HashMap;
use std::io::Read;

use crate::appsec::processor::response::ExpectedResponseFormat;
use crate::appsec::processor::InvocationPayload;

/// Standard HTTP request representation for AppSec analysis.
///
/// This struct provides a framework-agnostic way to pass HTTP request data
/// to the AppSec processor. Applications should populate this from their
/// web framework's request type (Axum, Actix, Rocket, Hyper, etc.).
///
/// # Fields
///
/// All fields are public for easy population from framework-specific types.
/// Use `Default::default()` and struct update syntax for convenience.
///
/// # Security Analysis
///
/// The WAF analyzes these components for threats:
/// - **Method + URI**: Directory traversal, path injection
/// - **Headers**: Header injection, XSS via headers
/// - **Cookies**: Session fixation, authentication bypass
/// - **Query/Path params**: SQL injection, command injection
/// - **Body**: XSS, SQL injection, malicious payloads
///
/// # Example
///
/// ```rust,ignore
/// use datadog_agent_native::appsec::processor::http::HttpRequest;
///
/// let request = HttpRequest {
///     method: "POST".to_string(),
///     uri: "/api/users/123".to_string(),
///     route: Some("/api/users/{id}".to_string()),
///     client_ip: Some("192.168.1.100".to_string()),
///     body: Some(b"{\"name\": \"Alice\"}".to_vec()),
///     ..Default::default()
/// };
/// ```
#[derive(Debug, Clone, Default)]
pub struct HttpRequest {
    /// The HTTP method (GET, POST, PUT, DELETE, etc.).
    pub method: String,

    /// The full URI including path (query string excluded).
    ///
    /// Example: `/api/v1/users/123`
    pub uri: String,

    /// The route pattern if available.
    ///
    /// Example: `/api/v1/users/{id}`
    ///
    /// This helps group requests by endpoint for API security sampling.
    pub route: Option<String>,

    /// Client IP address (IPv4 or IPv6).
    ///
    /// Example: `192.168.1.1` or `2001:0db8:85a3::8a2e:0370:7334`
    ///
    /// Consider X-Forwarded-For or X-Real-IP headers for proxied requests.
    pub client_ip: Option<String>,

    /// Request headers (lowercase keys, cookies excluded).
    ///
    /// Multi-value headers are supported. Cookies should be extracted
    /// separately into the `cookies` field.
    pub headers: HashMap<String, Vec<String>>,

    /// Cookies parsed from Cookie header.
    ///
    /// Multi-value cookies (same name) are supported.
    pub cookies: HashMap<String, Vec<String>>,

    /// Query string parameters.
    ///
    /// Example: For `?page=1&sort=name&filter=active`,
    /// this would contain `{"page": ["1"], "sort": ["name"], "filter": ["active"]}`
    pub query_params: HashMap<String, Vec<String>>,

    /// Path parameters extracted from route template.
    ///
    /// Example: For route `/users/{id}/posts/{post_id}` and URI `/users/123/posts/456`,
    /// this would contain `{"id": "123", "post_id": "456"}`
    pub path_params: HashMap<String, String>,

    /// Request body as bytes.
    ///
    /// Set to `None` for requests without a body (GET, HEAD, etc.).
    pub body: Option<Vec<u8>>,
}

/// Standard HTTP response representation for AppSec analysis.
///
/// This struct provides a framework-agnostic way to pass HTTP response data
/// to the AppSec processor for security analysis (data leakage, error disclosure, etc.).
///
/// # Security Analysis
///
/// The WAF analyzes responses for:
/// - **Status codes**: Error disclosure (500 errors with stack traces)
/// - **Headers**: Security header validation, information disclosure
/// - **Body**: Sensitive data leakage (PII, credentials, API keys)
///
/// # Example
///
/// ```rust,ignore
/// use datadog_agent_native::appsec::processor::http::HttpResponse;
///
/// let response = HttpResponse {
///     status_code: 200,
///     headers: [
///         ("content-type".to_string(), vec!["application/json".to_string()]),
///     ].into_iter().collect(),
///     body: Some(b"{\"id\": 123, \"name\": \"Alice\"}".to_vec()),
/// };
/// ```
#[derive(Debug, Clone, Default)]
pub struct HttpResponse {
    /// HTTP status code (e.g., 200, 404, 500).
    pub status_code: i64,

    /// Response headers (lowercase keys, cookies excluded).
    ///
    /// Multi-value headers are supported.
    pub headers: HashMap<String, Vec<String>>,

    /// Response body as bytes.
    ///
    /// Set to `None` for responses without a body (204 No Content, 304 Not Modified, etc.).
    pub body: Option<Vec<u8>>,
}

/// Combined HTTP request and response for complete transaction analysis.
///
/// This represents a full HTTP transaction (request + optional response) that
/// can be analyzed by AppSec. The WAF can analyze the request independently,
/// then update its analysis when the response becomes available.
///
/// # Two-Phase Analysis
///
/// 1. **Request Phase**: Analyze incoming request for attacks
/// 2. **Response Phase**: Analyze outgoing response for data leakage
///
/// # Example
///
/// ```rust,ignore
/// use datadog_agent_native::appsec::processor::http::*;
///
/// // Phase 1: Analyze request
/// let transaction = HttpTransaction::from_request(request);
/// processor.process_http_request("req-123", &transaction).await;
///
/// // ... handle request ...
///
/// // Phase 2: Analyze response
/// let transaction = transaction.with_response(response);
/// processor.process_http_response("req-123", &transaction).await;
/// ```
#[derive(Debug, Clone, Default)]
pub struct HttpTransaction {
    /// The HTTP request.
    pub request: HttpRequest,

    /// The HTTP response (if available).
    ///
    /// Set to `None` during request-only analysis, then populated
    /// before response analysis.
    pub response: Option<HttpResponse>,
}

impl HttpTransaction {
    /// Creates a new HTTP transaction from a request.
    ///
    /// The transaction initially has no response. Use `with_response()` or
    /// `set_response()` to add the response after it's available.
    ///
    /// # Arguments
    ///
    /// * `request` - The HTTP request to analyze
    ///
    /// # Returns
    ///
    /// A new transaction with the request but no response.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let transaction = HttpTransaction::from_request(request);
    /// processor.process_http_request("req-123", &transaction).await;
    /// ```
    pub fn from_request(request: HttpRequest) -> Self {
        Self {
            request,
            response: None,
        }
    }

    /// Adds the response to this transaction (builder pattern).
    ///
    /// This method consumes the transaction and returns a new one with the
    /// response attached. Use this for fluent/builder-style code.
    ///
    /// # Arguments
    ///
    /// * `response` - The HTTP response to add
    ///
    /// # Returns
    ///
    /// The transaction with the response attached.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let transaction = HttpTransaction::from_request(request)
    ///     .with_response(response);
    /// processor.process_http_response("req-123", &transaction).await;
    /// ```
    pub fn with_response(mut self, response: HttpResponse) -> Self {
        self.response = Some(response);
        self
    }

    /// Sets the response for this transaction (mutating method).
    ///
    /// This method mutates the transaction in-place. Use this when you need
    /// to update an existing transaction.
    ///
    /// # Arguments
    ///
    /// * `response` - The HTTP response to set
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut transaction = HttpTransaction::from_request(request);
    /// // ... later ...
    /// transaction.set_response(response);
    /// processor.process_http_response("req-123", &transaction).await;
    /// ```
    pub fn set_response(&mut self, response: HttpResponse) {
        self.response = Some(response);
    }
}

impl InvocationPayload for HttpTransaction {
    fn corresponding_response_format(&self) -> ExpectedResponseFormat {
        // For standard HTTP, we use Raw format
        ExpectedResponseFormat::Raw
    }

    fn raw_uri(&self) -> Option<String> {
        Some(self.request.uri.clone())
    }

    fn method(&self) -> Option<String> {
        Some(self.request.method.clone())
    }

    fn route(&self) -> Option<String> {
        self.request.route.clone()
    }

    fn client_ip(&self) -> Option<String> {
        self.request.client_ip.clone()
    }

    fn request_headers_no_cookies(&self) -> HashMap<String, Vec<String>> {
        self.request.headers.clone()
    }

    fn request_cookies(&self) -> HashMap<String, Vec<String>> {
        self.request.cookies.clone()
    }

    fn query_params(&self) -> HashMap<String, Vec<String>> {
        self.request.query_params.clone()
    }

    fn path_params(&self) -> HashMap<String, String> {
        self.request.path_params.clone()
    }

    fn request_body<'a>(&'a self) -> Option<Box<dyn Read + 'a>> {
        self.request
            .body
            .as_ref()
            .map(|b| Box::new(std::io::Cursor::new(b)) as Box<dyn Read>)
    }

    fn response_status_code(&self) -> Option<i64> {
        self.response.as_ref().map(|r| r.status_code)
    }

    fn response_headers_no_cookies(&self) -> HashMap<String, Vec<String>> {
        self.response
            .as_ref()
            .map(|r| r.headers.clone())
            .unwrap_or_default()
    }

    fn response_body<'a>(&'a self) -> Option<Box<dyn Read + 'a>> {
        self.response.as_ref().and_then(|r| {
            r.body
                .as_ref()
                .map(|b| Box::new(std::io::Cursor::new(b)) as Box<dyn Read>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_transaction_creation() {
        let request = HttpRequest {
            method: "GET".to_string(),
            uri: "/api/users".to_string(),
            route: Some("/api/users".to_string()),
            client_ip: Some("127.0.0.1".to_string()),
            ..Default::default()
        };

        let transaction = HttpTransaction::from_request(request.clone());
        assert_eq!(transaction.request.method, "GET");
        assert_eq!(transaction.request.uri, "/api/users");
        assert!(transaction.response.is_none());

        let response = HttpResponse {
            status_code: 200,
            ..Default::default()
        };

        let transaction = transaction.with_response(response);
        assert!(transaction.response.is_some());
        assert_eq!(transaction.response.as_ref().unwrap().status_code, 200);
    }

    #[test]
    fn test_invocation_payload_implementation() {
        let request = HttpRequest {
            method: "POST".to_string(),
            uri: "/api/users/123".to_string(),
            route: Some("/api/users/{id}".to_string()),
            client_ip: Some("192.168.1.1".to_string()),
            body: Some(b"{\"name\": \"test\"}".to_vec()),
            ..Default::default()
        };

        let response = HttpResponse {
            status_code: 201,
            body: Some(b"{\"id\": 123}".to_vec()),
            ..Default::default()
        };

        let transaction = HttpTransaction::from_request(request).with_response(response);

        assert_eq!(transaction.method(), Some("POST".to_string()));
        assert_eq!(transaction.raw_uri(), Some("/api/users/123".to_string()));
        assert_eq!(transaction.route(), Some("/api/users/{id}".to_string()));
        assert_eq!(transaction.client_ip(), Some("192.168.1.1".to_string()));
        assert_eq!(transaction.response_status_code(), Some(201));
        assert!(transaction.request_body().is_some());
        assert!(transaction.response_body().is_some());
    }
}
