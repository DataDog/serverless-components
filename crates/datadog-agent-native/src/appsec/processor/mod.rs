//! AppSec request processor and WAF integration.
//!
//! This module implements the core AppSec processing logic, integrating the
//! libddwaf library to analyze HTTP requests and responses for security threats.
//!
//! # Architecture
//!
//! ```text
//!    HTTP Request
//!         │
//!         v
//!   ┌──────────────┐
//!   │  Processor   │ (Creates context per request)
//!   └──────┬───────┘
//!         │
//!         v
//!   ┌──────────────┐
//!   │   Context    │ (WAF execution environment)
//!   └──────┬───────┘
//!         │
//!         v
//!   ┌──────────────┐
//!   │ WAF Analysis │ (Ruleset evaluation)
//!   └──────┬───────┘
//!         │
//!         v
//!   ┌──────────────┐
//!   │   Results    │ (Security events, actions)
//!   └──────────────┘
//! ```
//!
//! # Processing Flow
//!
//! 1. **Request Phase**: `process_http_request()` creates context and runs WAF on request data
//! 2. **Response Phase**: `process_http_response()` runs WAF on response data and samples API schemas
//! 3. **Span Integration**: `process_span()` attaches security events to APM spans
//! 4. **Context Management**: Contexts are buffered (max 1,000) with FIFO eviction
//!
//! # Memory Management
//!
//! The processor maintains a bounded buffer of security contexts (max 1,000 entries).
//! When the buffer is full, the oldest context is evicted (FIFO policy) to prevent
//! unbounded memory growth under high request volume.
//!
//! # WAF Integration
//!
//! The processor uses libddwaf (Datadog's Web Application Firewall) for security analysis:
//! - Ruleset loaded from configuration or default recommended rules
//! - Configurable timeout per WAF execution (default: 5ms)
//! - API security sampling for schema extraction
//!
//! # Thread Safety
//!
//! The processor itself is not thread-safe (uses `&mut self`), but can be
//! wrapped in synchronization primitives if needed. The API security sampler
//! uses `Arc<Mutex<>>` for thread-safe access across async tasks.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::Read;
use std::num::NonZero;
use std::sync::Arc;
use std::time::Duration;

use datadog_trace_protobuf::pb::Span;
use libddwaf::object::{WafMap, WafOwned};
use libddwaf::{Builder, Config as WafConfig, Handle};
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::appsec::processor::context::Context;
use crate::appsec::processor::response::ExpectedResponseFormat;
use crate::appsec::{is_enabled, is_standalone};
use crate::config::Config;
// Lambda-specific trigger removed - not needed for general agent
// use crate::lifecycle::invocation::triggers::IdentifiedTrigger;

mod apisec;
pub mod context;
pub mod http;
pub mod response;
mod ruleset;

/// Maximum number of AppSec contexts that can be buffered before eviction.
///
/// # Value: 1,000 contexts
///
/// This prevents unbounded memory growth under high request volume. When the
/// buffer reaches this limit, the oldest context is evicted (FIFO policy).
///
/// # Memory Impact
///
/// Each context contains:
/// - Request/response data (~1-10KB per request)
/// - WAF execution state (~1KB)
/// - Security events (~0-5KB when threats detected)
///
/// Total memory: ~1,000 × 10KB = ~10MB maximum
const MAX_CONTEXT_BUFFER_SIZE: usize = 1_000;

/// AppSec processor for analyzing HTTP requests and responses.
///
/// The processor integrates with libddwaf (Datadog's Web Application Firewall)
/// to detect security threats in HTTP traffic. It maintains a bounded buffer
/// of security contexts, one per request, and manages the lifecycle of WAF analysis.
///
/// # Fields
///
/// - `handle`: WAF instance handle for executing security rules
/// - `ruleset_version`: Version string of the loaded security ruleset
/// - `waf_timeout`: Maximum time allowed for WAF execution per request
/// - `api_sec_sampler`: Optional sampler for API security schema extraction
/// - `context_buffer`: FIFO buffer of active security contexts (max 1,000)
/// - `max_context_buffer_size`: Maximum number of contexts before eviction
///
/// # Example
///
/// ```rust,ignore
/// use datadog_agent_native::appsec::processor::Processor;
/// use datadog_agent_native::config::Config;
///
/// let config = Config {
///     appsec_enabled: true,
///     ..Config::default()
/// };
///
/// let mut processor = Processor::new(&config)?;
///
/// // Process request
/// processor.process_http_request("req-123", &http_transaction).await;
///
/// // Process response
/// processor.process_http_response("req-123", &http_transaction).await;
/// ```
pub struct Processor {
    handle: Handle,
    ruleset_version: String,
    waf_timeout: Duration,
    api_sec_sampler: Option<Arc<Mutex<apisec::Sampler>>>, // Must be [`Arc`] so [`Processor`] can be [`Send`].
    context_buffer: VecDeque<Context>,
    max_context_buffer_size: usize,
}
impl Processor {
    const CONTEXT_BUFFER_DEFAULT_CAPACITY: NonZero<usize> = unsafe { NonZero::new_unchecked(5) };

    /// Creates a new [`Processor`] instance using the provided [`Config`].
    ///
    /// # Errors
    /// - If the [`Config::appsec_rules`] points to a non-existent file;
    /// - If the [`Config::appsec_rules`] points to a file that is not a valid JSON-encoded ruleset;
    /// - If the in-app WAF fails to initialize, integrate the ruleset, or build the WAF instance.
    pub fn new(cfg: &Config) -> Result<Self, Error> {
        Self::with_capacity(cfg, Self::CONTEXT_BUFFER_DEFAULT_CAPACITY)
    }

    /// Creates a new [`Processor`] instance using the provided [`Config`].
    ///
    /// This method initializes the WAF (Web Application Firewall) with security
    /// rules and sets up the processor for analyzing HTTP requests.
    ///
    /// # Arguments
    ///
    /// * `cfg` - Configuration containing AppSec settings and ruleset path
    /// * `capacity` - Initial capacity for the context buffer (pre-allocates memory)
    ///
    /// # Returns
    ///
    /// * `Ok(Processor)` - Successfully initialized processor
    /// * `Err(Error)` - Initialization failed (see error variants below)
    ///
    /// # Errors
    ///
    /// - [`Error::FeatureDisabled`] - AppSec is not enabled in configuration
    /// - [`Error::AppsecRulesError`] - Custom ruleset file could not be read
    /// - [`Error::WafRulesetParseError`] - Ruleset JSON is malformed
    /// - [`Error::WafBuilderCreationFailed`] - WAF library initialization failed
    /// - [`Error::WafRulesetLoadingError`] - Rules could not be loaded into WAF
    /// - [`Error::WafInitializationFailed`] - WAF handle could not be created
    pub fn with_capacity(cfg: &Config, capacity: NonZero<usize>) -> Result<Self, Error> {
        // Check if AppSec is enabled in configuration
        // Decision: Fail early to avoid unnecessary initialization
        if !is_enabled(cfg) {
            return Err(Error::FeatureDisabled);
        }

        debug!("aap: starting App & API Protection processor");

        // Check if running in standalone mode (no tracing)
        // Decision: Log prominently as this affects APM behavior
        if is_standalone() {
            info!(
                "aap: starting App & API Protection in standalone mode. APM tracing will be disabled for this service."
            );
        }

        // Create WAF builder with default configuration
        // Decision: Use default config for standard WAF behavior
        let Some(mut builder) = Builder::new(&WafConfig::default()) else {
            return Err(Error::WafBuilderCreationFailed);
        };

        // Load security rules from config or default recommended ruleset
        let rules = Self::get_rules(cfg)?;

        // Add rules to WAF builder and collect diagnostics
        let mut diagnostics = WafOwned::<WafMap>::default();
        if !builder.add_or_update_config("rules", &rules, Some(&mut diagnostics)) {
            // Rules failed to load - return diagnostics for debugging
            return Err(Error::WafRulesetLoadingError(diagnostics));
        }

        // Extract ruleset version from diagnostics for logging and telemetry
        let ruleset_version =
            if let Some(version) = diagnostics.get(b"ruleset_version").and_then(|o| o.to_str()) {
                // Version found in ruleset metadata
                debug!("aap: loaded ruleset version {version}");
                version.to_string()
            } else {
                // No version information available - use empty string
                String::new()
            };

        // Build final WAF handle from configured builder
        // Decision: Handle is None if no active rules or processors exist
        let Some(handle) = builder.build() else {
            return Err(Error::WafInitializationFailed);
        };

        Ok(Self {
            handle,
            ruleset_version,
            waf_timeout: cfg.appsec_waf_timeout,
            // Initialize API security sampler if enabled
            // Decision: Use Arc<Mutex<>> for thread-safe access across async tasks
            api_sec_sampler: if cfg.api_security_enabled {
                Some(Arc::new(Mutex::new(apisec::Sampler::with_interval(
                    cfg.api_security_sample_delay,
                ))))
            } else {
                None
            },
            context_buffer: VecDeque::with_capacity(capacity.get()),
            max_context_buffer_size: MAX_CONTEXT_BUFFER_SIZE,
        })
    }

    /// Process an HTTP request and start security analysis.
    ///
    /// This creates a new security context for the request and runs the WAF rules
    /// against the request data. Call this when an HTTP request is received.
    ///
    /// # Arguments
    /// * `request_id` - A unique identifier for this request (e.g., trace ID or request ID)
    /// * `payload` - The HTTP request/transaction data to analyze
    ///
    /// # Example
    /// ```ignore
    /// let mut processor = Processor::new(&config)?;
    /// let request_id = "req-12345";
    /// let http_data = HttpTransaction::from_request(request);
    /// processor.process_http_request(request_id, &http_data).await;
    /// ```
    pub async fn process_http_request(
        &mut self,
        request_id: &str,
        payload: &dyn InvocationPayload,
    ) {
        debug!("aap: processing HTTP request for {request_id}");
        self.new_context(request_id).await.run(payload);
    }

    /// Process an HTTP response and complete security analysis.
    ///
    /// This runs the WAF rules against the response data and performs API security
    /// sampling if enabled. Call this after the HTTP response is generated, before
    /// sending it to the client.
    ///
    /// # Arguments
    /// * `request_id` - The same unique identifier used in `process_http_request`
    /// * `payload` - The complete HTTP transaction including the response
    ///
    /// # Example
    /// ```ignore
    /// let request_id = "req-12345";
    /// let http_data = transaction.with_response(response);
    /// processor.process_http_response(request_id, &http_data).await;
    /// ```
    pub async fn process_http_response(
        &mut self,
        request_id: &str,
        payload: &dyn InvocationPayload,
    ) {
        debug!("aap: processing HTTP response for {request_id}");

        // Clone Arc to sampler before mutable borrow of self
        // Decision: Clone Arc first to avoid borrow checker issues
        let api_sec_sampler = self.api_sec_sampler.as_ref().map(Arc::clone);

        // Lock sampler if it exists (async Mutex for thread-safe access)
        let api_sec_sampler = if let Some(api_sec_sampler) = &api_sec_sampler {
            Some(api_sec_sampler.lock().await)
        } else {
            None
        };

        // Get mutable context for this request ID
        // Decision: Return early if no context found (request wasn't processed)
        let Some(ctx) = self.get_context_mut(request_id) else {
            // No context means request was never processed or already cleaned up
            debug!("aap: no security context found for {request_id}");
            return;
        };

        // Run WAF security rules against the response data
        ctx.run(payload);

        // Check if we should extract API schemas for this endpoint
        // Decision: Sample based on method, route, and status code combination
        let (method, route, status_code) = ctx.endpoint_info();
        if api_sec_sampler
            .is_some_and(|mut sampler| sampler.decision_for(&method, &route, &status_code))
        {
            // Sampler decided to extract schema for this endpoint
            debug!(
                "aap: extracting API Security schema for request <{method}, {route}, {status_code}>"
            );
            ctx.extract_schemas();
        }

        // Mark response as seen so context can be finalized and cleaned up
        ctx.set_response_seen().await;
    }

    /// Process a complete HTTP transaction (request + response) in one call.
    ///
    /// This is a convenience method that combines `process_http_request` and
    /// `process_http_response` into a single call. Use this when you have both
    /// request and response data available at the same time.
    ///
    /// # Arguments
    /// * `request_id` - A unique identifier for this request
    /// * `payload` - The complete HTTP transaction with request and response
    ///
    /// # Example
    /// ```ignore
    /// let mut processor = Processor::new(&config)?;
    /// let request_id = "req-12345";
    /// let transaction = HttpTransaction::from_request(request).with_response(response);
    /// processor.process_http_transaction(request_id, &transaction).await;
    /// ```
    pub async fn process_http_transaction(
        &mut self,
        request_id: &str,
        payload: &dyn InvocationPayload,
    ) {
        self.process_http_request(request_id, payload).await;
        self.process_http_response(request_id, payload).await;
    }

    // Lambda-specific invocation methods commented out for general agent
    // These were used for Lambda event processing and are not needed
    /*
    /// Process the intercepted payload for the next invocation.
    pub async fn process_invocation_next(&mut self, rid: &str, payload: &IdentifiedTrigger) {
        if payload.is_unknown() {
            return;
        }
        self.new_context(rid).await.run(payload);
    }

    /// Process the intercepted payload for the result of an invocation.
    pub async fn process_invocation_result(&mut self, rid: &str, payload: &Bytes) {
        // Taking the sampler first, as it implies a temporary immutable borrow...
        let api_sec_sampler = self.api_sec_sampler.as_ref().map(Arc::clone);
        let api_sec_sampler = if let Some(api_sec_sampler) = &api_sec_sampler {
            Some(api_sec_sampler.lock().await)
        } else {
            None
        };

        let Some(ctx) = self.get_context_mut(rid) else {
            // Nothing to do...
            return;
        };

        match ctx.expected_response_format.parse(payload.as_ref()) {
            Ok(Some(payload)) => {
                debug!("aap: successfully parsed response payload, evaluating ruleset...");
                ctx.run(payload.as_ref());
            }
            Ok(None) => debug!("aap: no response payload available"),
            Err(e) => warn!("aap: failed to parse invocation result payload: {e}"),
        }

        let (method, route, status_code) = ctx.endpoint_info();
        if api_sec_sampler
            .is_some_and(|mut sampler| sampler.decision_for(&method, &route, &status_code))
        {
            debug!(
                "aap: extracing API Security schema for request <{method}, {route}, {status_code}>"
            );
            ctx.extract_schemas();
        }

        // Finally, mark the response as having been seen.
        ctx.set_response_seen().await;
    }
    */

    /// Returns the first `aws.lambda` span from the provided trace, if one
    /// exists.
    ///
    /// # Returns
    /// The span on which security information will be attached.
    pub fn service_entry_span_mut(trace: &mut [Span]) -> Option<&mut Span> {
        trace.iter_mut().find(|span| span.name == "aws.lambda")
    }

    /// Processes an intercepted [`Span`] and attaches security events.
    ///
    /// This method integrates AppSec with APM by attaching security findings
    /// to the appropriate span. It checks if a security context exists for the
    /// span's request ID and attaches any detected threats as span tags.
    ///
    /// # Arguments
    ///
    /// * `span` - The APM span to process and potentially attach security data to
    ///
    /// # Returns
    ///
    /// Returns a tuple `(bool, Option<&mut Context>)`:
    /// - `(true, None)` - Span is finalized and can be sent immediately
    /// - `(false, Some(ctx))` - Span is pending response, defer finalization
    ///
    /// # Finalization Logic
    ///
    /// A span is finalized when:
    /// 1. No request_id is found (not an HTTP request span)
    /// 2. No security context exists (request not analyzed)
    /// 3. Response has been processed (security analysis complete)
    pub fn process_span(&mut self, span: &mut Span) -> (bool, Option<&mut Context>) {
        // Extract request ID from span metadata
        // Decision: Return early if no request_id (not an HTTP span)
        let Some(rid) = span.meta.get("request_id").cloned() else {
            // Can't match this to a request ID - nothing to attach
            debug!(
                "aap | {} @ {} | no request_id found in span meta, nothing to do...",
                span.name, span.span_id
            );
            return (true, None);
        };

        // Check if security context exists for this request ID
        // Decision: Return early if no context (request not analyzed by AppSec)
        let Some(ctx) = self.get_context(&rid) else {
            // No security analysis was performed for this request
            debug!(
                "aap | {} @ {} | no security context is associated with request {rid}, nothing to do...",
                span.name, span.span_id
            );
            return (true, None);
        };

        debug!(
            "aap | {} @ {} | processing span with request {rid}",
            span.name, span.span_id
        );

        // Attach security events/tags to the span
        ctx.process_span(span);

        // Check if response has been processed
        // Decision: Delete context and finalize span if response was seen
        if !ctx.is_pending_response() {
            // Response already processed - span is complete from security perspective
            debug!(
                "aap | {} @ {} | span is finalized, deleting context",
                span.name, span.span_id
            );
            self.delete_context(&rid);
            return (true, None);
        }

        // Response not yet processed - return mutable context for deferred processing
        // Decision: Re-fetch context to satisfy borrow checker (previous was immutable)
        (false, self.get_context_mut(&rid))
    }

    /// Parses the App & API Protection ruleset from the provided [`Config`], or
    /// the default built-in ruleset if the [`Config::appsec_rules`] field is
    /// [`None`].
    ///
    /// # Arguments
    ///
    /// * `cfg` - Configuration containing optional custom ruleset path
    ///
    /// # Returns
    ///
    /// * `Ok(WafMap)` - Parsed security ruleset ready for WAF integration
    /// * `Err(Error)` - File I/O or JSON parsing error
    ///
    /// # Ruleset Sources
    ///
    /// 1. **Custom ruleset**: If `cfg.appsec_rules` is `Some(path)`, load from file
    /// 2. **Default ruleset**: If `None`, use built-in recommended ruleset
    ///
    /// # Error Handling
    ///
    /// - [`Error::AppsecRulesError`] - Custom ruleset file could not be opened
    /// - [`Error::WafRulesetParseError`] - JSON parsing failed for either source
    fn get_rules(cfg: &Config) -> Result<WafMap, Error> {
        // Check if custom ruleset path is configured
        // Decision: Prefer custom rules over default for flexibility
        if let Some(ref rules) = cfg.appsec_rules {
            // Custom ruleset specified - open file and parse JSON
            let file = File::open(rules).map_err(|e| Error::AppsecRulesError(rules.clone(), e))?;
            serde_json::from_reader(file)
        } else {
            // No custom ruleset - use default recommended ruleset (embedded)
            // Decision: Embed default rules for zero-config security
            serde_json::from_reader(ruleset::default_recommended_ruleset())
        }
        .map_err(Error::WafRulesetParseError)
    }

    /// Creates a new [`Context`] for the given request ID, and tracks it in the
    /// context buffer.
    ///
    /// If the buffer is full, the oldest context is evicted (FIFO policy) to prevent
    /// unbounded memory growth. Duplicate request IDs are also evicted before adding
    /// the new context.
    ///
    /// # Arguments
    ///
    /// * `rid` - Request ID to associate with the new context
    ///
    /// # Returns
    ///
    /// A mutable reference to the newly created context
    ///
    /// # Memory Management
    ///
    /// The buffer uses a FIFO eviction strategy with these rules:
    /// 1. **Duplicate IDs**: Remove existing context with same request ID
    /// 2. **Buffer full**: Remove oldest context (front of deque)
    /// 3. **Has capacity**: Simply add new context to back
    ///
    /// # Graceful Eviction
    ///
    /// Before evicting a context, any pending security events are flushed
    /// to ensure no data loss.
    async fn new_context(&mut self, rid: &str) -> &mut Context {
        // Check if we need to evict a context before adding new one
        let dropped = if let Some(idx) = self.context_buffer.iter().position(|c| c.rid == rid) {
            // Duplicate request ID found - remove existing context
            // Decision: Replace existing context to avoid confusion
            self.context_buffer.remove(idx)
        } else if self.context_buffer.len() >= self.max_context_buffer_size {
            // Buffer is at maximum capacity - evict oldest context (FIFO)
            // Decision: FIFO eviction ensures fairness and predictability
            debug!(
                "aap: AppSec context buffer full ({} contexts), dropping oldest context",
                self.max_context_buffer_size
            );
            self.context_buffer.pop_front()
        } else {
            // Buffer has capacity - no eviction needed
            None
        };

        // Gracefully clean up evicted context if any
        if let Some(mut ctx) = dropped {
            // Flush any pending security events before dropping context
            // Decision: Ensures no data loss during eviction
            ctx.set_response_seen().await;
        }

        // Add new context to back of buffer (most recent)
        self.context_buffer.push_back(Context::new(
            rid.to_string(),
            &mut self.handle,
            &self.ruleset_version,
            self.waf_timeout,
        ));

        // Return mutable reference to newly created context
        // Decision: Use expect() as we just pushed - buffer is never empty here
        self.context_buffer
            .back_mut()
            .expect("should have at least one element")
    }

    /// Retrieves the [`Context`] associated with the given request ID, if there
    /// is one.
    fn get_context(&self, rid: &str) -> Option<&Context> {
        self.context_buffer.iter().find(|c| c.rid == rid)
    }

    /// Retrieves the [`Context`] associated with the given request ID, if there
    /// is one.
    fn get_context_mut(&mut self, rid: &str) -> Option<&mut Context> {
        self.context_buffer.iter_mut().find(|c| c.rid == rid)
    }

    /// Removes the [`Context`] associated with the given request ID from the
    /// buffer, if there is one.
    fn delete_context(&mut self, rid: &str) {
        if let Some(idx) = self.context_buffer.iter().position(|c| c.rid == rid) {
            self.context_buffer.remove(idx);
        }
    }
}
impl std::fmt::Debug for Processor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Processor")
            .field("waf_timeout", &self.waf_timeout)
            .finish_non_exhaustive()
    }
}

/// Error conditions that can arise from calling [`Processor::new`].
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// The App & API Protection feature is not enabled
    #[error("aap: feature is not enabled")]
    FeatureDisabled,
    /// The WAF builder could not be created (unlikely)
    #[error("aap: WAF builder creation failed")]
    WafBuilderCreationFailed,
    /// The user-configured App & API Protection ruleset file could not be read
    #[error("aap: failed to open WAF rules file {0:#?}: {1}")]
    AppsecRulesError(String, std::io::Error),
    /// The App & API Protection ruleset could not be parsed from JSON
    #[error("aap: failed to parse WAF rulesset: {0}")]
    WafRulesetParseError(serde_json::Error),
    /// The App & API Protection ruleset could not be loaded into the WAF
    #[error("aap: failed to load configured WAF ruleset: {0:?}")]
    WafRulesetLoadingError(WafOwned<WafMap>),
    /// The WAF initialization produced a [`None`] handle (this happens when the
    /// configured ruleset contains no active rule nor processor)
    #[error(
        "aap: WAF initialization failed (is there any active rule or processor in the ruleset?)"
    )]
    WafInitializationFailed,
}

/// Trait representing HTTP request/response data for security analysis.
///
/// This trait defines the interface for extracting security-relevant data from
/// HTTP transactions. Implementors provide methods to extract request and response
/// components that the WAF analyzes for security threats.
///
/// # Design
///
/// All fields are optional and default to `None` or empty collections, allowing
/// implementors to provide only the data available in their environment (e.g.,
/// some event sources may not have cookies or path parameters).
///
/// # Security Analysis
///
/// The WAF analyzes these components for threats:
/// - **Request data**: URI, method, headers, cookies, query params, body
/// - **Response data**: Status code, headers, body
/// - **Context**: Client IP, route patterns
///
/// # Implementation Example
///
/// ```rust,ignore
/// impl InvocationPayload for MyHttpRequest {
///     fn corresponding_response_format(&self) -> ExpectedResponseFormat {
///         ExpectedResponseFormat::Raw
///     }
///
///     fn method(&self) -> Option<String> {
///         Some(self.http_method.clone())
///     }
///
///     fn raw_uri(&self) -> Option<String> {
///         Some(format!("{}{}", self.domain, self.path))
///     }
///
///     // Implement other methods as needed...
/// }
/// ```
pub trait InvocationPayload {
    /// Returns the expected format of the response for this request type.
    ///
    /// This helps the processor determine how to parse the response body
    /// when it becomes available.
    fn corresponding_response_format(&self) -> ExpectedResponseFormat;

    /// The raw URI of the request (query string excluded).
    ///
    /// # Example
    /// `https://api.example.com/v1/users/123`
    fn raw_uri(&self) -> Option<String> {
        None
    }

    /// The HTTP method for the request.
    ///
    /// # Example
    /// `GET`, `POST`, `PUT`, `DELETE`, etc.
    fn method(&self) -> Option<String> {
        None
    }

    /// The route pattern for the request.
    ///
    /// This is typically a templated path with parameter placeholders.
    ///
    /// # Example
    /// `/api/v1/users/{id}` or `GET /api/v1/users/{id}`
    fn route(&self) -> Option<String> {
        None
    }

    /// The client IP address for the request.
    ///
    /// # Example
    /// `192.168.1.1` or `2001:0db8:85a3::8a2e:0370:7334`
    fn client_ip(&self) -> Option<String> {
        None
    }

    /// Multi-value HTTP headers for the request, excluding cookies.
    ///
    /// Cookies are excluded because they're provided separately via
    /// `request_cookies()` for more structured analysis.
    ///
    /// # Example
    /// ```text
    /// {
    ///   "content-type": ["application/json"],
    ///   "accept": ["application/json", "text/plain"]
    /// }
    /// ```
    fn request_headers_no_cookies(&self) -> HashMap<String, Vec<String>> {
        HashMap::default()
    }

    /// HTTP cookies for the request.
    ///
    /// Cookies are provided separately from headers for structured analysis
    /// of authentication tokens and session identifiers.
    ///
    /// # Example
    /// ```text
    /// {
    ///   "session_id": ["abc123"],
    ///   "user_prefs": ["lang=en", "theme=dark"]
    /// }
    /// ```
    fn request_cookies(&self) -> HashMap<String, Vec<String>> {
        HashMap::default()
    }

    /// Query string parameters for the request.
    ///
    /// # Example
    /// For `?page=1&limit=10&sort=name&sort=id`:
    /// ```text
    /// {
    ///   "page": ["1"],
    ///   "limit": ["10"],
    ///   "sort": ["name", "id"]
    /// }
    /// ```
    fn query_params(&self) -> HashMap<String, Vec<String>> {
        HashMap::default()
    }

    /// Path parameters extracted from the route template.
    ///
    /// # Example
    /// For route `/users/{id}/posts/{post_id}` and URI `/users/123/posts/456`:
    /// ```text
    /// {
    ///   "id": "123",
    ///   "post_id": "456"
    /// }
    /// ```
    fn path_params(&self) -> HashMap<String, String> {
        HashMap::default()
    }

    /// Request body as a readable stream.
    ///
    /// Returns `None` if no body is present or available. The WAF will
    /// attempt to parse common formats (JSON, XML, form data) for analysis.
    ///
    /// # Example
    /// ```text
    /// Some(Box::new(Cursor::new(b"{\"user\": \"john\"}")))
    /// ```
    fn request_body<'a>(&'a self) -> Option<Box<dyn Read + 'a>> {
        None
    }

    /// HTTP status code of the response.
    ///
    /// # Example
    /// `200`, `404`, `500`, etc.
    fn response_status_code(&self) -> Option<i64> {
        None
    }

    /// Multi-value HTTP headers for the response, excluding cookies.
    ///
    /// # Example
    /// ```text
    /// {
    ///   "content-type": ["application/json"],
    ///   "x-rate-limit": ["100"]
    /// }
    /// ```
    fn response_headers_no_cookies(&self) -> HashMap<String, Vec<String>> {
        HashMap::default()
    }

    /// Response body as a readable stream.
    ///
    /// Returns `None` if no body is present or available. The WAF will
    /// analyze the response for data leakage and other security issues.
    ///
    /// # Example
    /// ```text
    /// Some(Box::new(Cursor::new(b"{\"result\": \"success\"}")))
    /// ```
    fn response_body<'a>(&'a self) -> Option<Box<dyn Read + 'a>> {
        None
    }
}

// Lambda-specific trait implementation commented out for general agent
// This was used for processing Lambda events (API Gateway, ALB, etc.)
/*
impl InvocationPayload for IdentifiedTrigger {
    fn corresponding_response_format(&self) -> ExpectedResponseFormat {
        match self {
            Self::APIGatewayHttpEvent(_)
            | Self::APIGatewayRestEvent(_)
            | Self::ALBEvent(_)
            | Self::LambdaFunctionUrlEvent(_) => ExpectedResponseFormat::ApiGatewayResponse,
            Self::APIGatewayWebSocketEvent(_) => ExpectedResponseFormat::Raw,

            // Events that are not relevant to App & API Protection
            Self::MSKEvent(_)
            | Self::SqsRecord(_)
            | Self::SnsRecord(_)
            | Self::DynamoDbRecord(_)
            | Self::S3Record(_)
            | Self::EventBridgeEvent(_)
            | Self::KinesisRecord(_)
            | Self::StepFunctionEvent(_)
            | Self::Unknown => ExpectedResponseFormat::Unknown,
        }
    }

    fn raw_uri(&self) -> Option<String> {
        match self {
            Self::APIGatewayHttpEvent(t) => Some(format!(
                "{domain}{path}",
                domain = t.request_context.domain_name,
                path = t.request_context.http.path
            )),
            Self::APIGatewayRestEvent(t) => Some(format!(
                "{domain}{path}",
                domain = t.request_context.domain_name,
                path = t.request_context.path
            )),
            Self::APIGatewayWebSocketEvent(t) => Some(
                if t.request_context.stage.is_empty() || t.request_context.stage == "$default" {
                    format!(
                        "{domain}{path}",
                        domain = t.request_context.domain_name,
                        path = t.path.as_ref().map_or("", |s| s.as_str())
                    )
                } else {
                    format!(
                        "{domain}/${stage}{path}",
                        domain = t.request_context.domain_name,
                        stage = t.request_context.stage,
                        path = t.path.as_ref().map_or("", |s| s.as_str())
                    )
                },
            ),

            #[allow(clippy::match_same_arms)]
            Self::ALBEvent(_) => None,
            Self::LambdaFunctionUrlEvent(t) => Some(format!(
                "{domain}{path}",
                domain = t.request_context.domain_name,
                path = t.request_context.http.path
            )),
            // Events that are not relevant to App & API Protection
            Self::MSKEvent(_)
            | Self::SqsRecord(_)
            | Self::SnsRecord(_)
            | Self::DynamoDbRecord(_)
            | Self::S3Record(_)
            | Self::EventBridgeEvent(_)
            | Self::KinesisRecord(_)
            | Self::StepFunctionEvent(_)
            | Self::Unknown => None,
        }
    }
    fn method(&self) -> Option<String> {
        match self {
            Self::APIGatewayHttpEvent(t) => Some(t.request_context.http.method.clone()),
            Self::APIGatewayRestEvent(t) => Some(t.request_context.method.clone()),
            Self::APIGatewayWebSocketEvent(t) => t.request_context.http_method.clone(),
            Self::ALBEvent(t) => Some(t.http_method.clone()),
            Self::LambdaFunctionUrlEvent(t) => Some(t.request_context.http.method.clone()),
            // Events that are not relevant to App & API Protection
            Self::MSKEvent(_)
            | Self::SqsRecord(_)
            | Self::SnsRecord(_)
            | Self::DynamoDbRecord(_)
            | Self::S3Record(_)
            | Self::EventBridgeEvent(_)
            | Self::KinesisRecord(_)
            | Self::StepFunctionEvent(_)
            | Self::Unknown => None,
        }
    }
    fn route(&self) -> Option<String> {
        match self {
            Self::APIGatewayHttpEvent(t) => Some(t.route_key.clone()),
            Self::APIGatewayRestEvent(t) => Some(format!(
                "{method} {resource}",
                method = t.request_context.method,
                resource = t.request_context.resource_path
            )),
            Self::APIGatewayWebSocketEvent(t) => Some(t.request_context.route_key.clone()),
            Self::ALBEvent(t) => Some(format!(
                "{method} {path}",
                method = t.http_method,
                path = t.path.as_ref().map_or("", |s| s.as_str()),
            )),
            Self::LambdaFunctionUrlEvent(t) => Some(format!(
                "{method} {path}",
                method = t.request_context.http.method,
                path = t.request_context.http.path
            )),
            // Events that are not relevant to App & API Protection
            Self::MSKEvent(_)
            | Self::SqsRecord(_)
            | Self::SnsRecord(_)
            | Self::DynamoDbRecord(_)
            | Self::S3Record(_)
            | Self::EventBridgeEvent(_)
            | Self::KinesisRecord(_)
            | Self::StepFunctionEvent(_)
            | Self::Unknown => None,
        }
    }
    fn client_ip(&self) -> Option<String> {
        match self {
            Self::APIGatewayHttpEvent(t) => Some(t.request_context.http.source_ip.clone()),
            Self::APIGatewayRestEvent(t) => Some(t.request_context.identity.source_ip.clone()),
            Self::APIGatewayWebSocketEvent(t) => t.request_context.identity.source_ip.clone(),
            #[allow(clippy::match_same_arms)]
            Self::ALBEvent(_) => None, // TODO: Can we extract from the headers instead?
            Self::LambdaFunctionUrlEvent(t) => Some(t.request_context.http.source_ip.clone()),
            // Events that are not relevant to App & API Protection
            Self::MSKEvent(_)
            | Self::SqsRecord(_)
            | Self::SnsRecord(_)
            | Self::DynamoDbRecord(_)
            | Self::S3Record(_)
            | Self::EventBridgeEvent(_)
            | Self::KinesisRecord(_)
            | Self::StepFunctionEvent(_)
            | Self::Unknown => None,
        }
    }
    fn request_headers_no_cookies(&self) -> HashMap<String, Vec<String>> {
        fn cloned<'a, K: Clone + 'a, V: Clone + 'a>((k, v): (&'a K, &'a V)) -> (K, V) {
            (k.clone(), v.clone())
        }
        fn as_multi<K, V>((k, v): (K, V)) -> (K, Vec<V>) {
            (k, vec![v])
        }
        fn without_cookie<K: PartialEq<&'static str>, V>((k, _): &(K, V)) -> bool {
            *k != "cookie"
        }

        match self {
            Self::APIGatewayHttpEvent(t) => t.headers.iter().map(cloned).map(as_multi).collect(),
            Self::APIGatewayRestEvent(t) => t
                .multi_value_headers
                .iter()
                .filter(without_cookie)
                .map(cloned)
                .collect(),
            Self::APIGatewayWebSocketEvent(t) => t
                .multi_value_headers
                .iter()
                .filter(without_cookie)
                .map(cloned)
                .collect(),
            Self::ALBEvent(t) => {
                if t.multi_value_headers.is_empty() {
                    t.headers
                        .iter()
                        .filter(without_cookie)
                        .map(cloned)
                        .map(as_multi)
                        .collect()
                } else {
                    t.multi_value_headers
                        .iter()
                        .filter(without_cookie)
                        .map(cloned)
                        .collect()
                }
            }
            Self::LambdaFunctionUrlEvent(t) => t.headers.iter().map(cloned).map(as_multi).collect(),
            // Events that are not relevant to App & API Protection
            Self::MSKEvent(_)
            | Self::SqsRecord(_)
            | Self::SnsRecord(_)
            | Self::DynamoDbRecord(_)
            | Self::S3Record(_)
            | Self::EventBridgeEvent(_)
            | Self::KinesisRecord(_)
            | Self::StepFunctionEvent(_)
            | Self::Unknown => HashMap::default(),
        }
    }
    fn request_cookies(&self) -> HashMap<String, Vec<String>> {
        fn parse_cookie<'a, T: Into<Cow<'a, str>>>(
            cookie: T,
        ) -> impl Iterator<Item = (String, String)> + use<'a, T> {
            Cookie::split_parse(cookie)
                .filter_map(Result::ok)
                .map(|c| (c.name().to_string(), c.value().to_string()))
        }
        fn list_to_map(list: impl AsRef<[String]>) -> HashMap<String, Vec<String>> {
            list.as_ref()
                .iter()
                .filter_map(|c| c.split_once('='))
                .chunk_by(|(k, _)| (*k).to_string())
                .into_iter()
                .map(|(k, v)| (k, v.into_iter().map(|(_, v)| v.to_string()).collect()))
                .collect()
        }

        match self {
            Self::APIGatewayHttpEvent(t) => t.cookies.as_ref().map(list_to_map).unwrap_or_default(),
            Self::APIGatewayRestEvent(t) => t
                .multi_value_headers
                .get("cookie")
                .cloned()
                .or_else(|| t.headers.get("cookie").map(|v| vec![v.clone()]))
                .unwrap_or_default()
                .iter()
                .flat_map(parse_cookie)
                .chunk_by(|(k, _)| (*k).to_string())
                .into_iter()
                .map(|(k, v)| (k, v.into_iter().map(|(_, v)| v.to_string()).collect()))
                .collect(),
            Self::APIGatewayWebSocketEvent(t) => t
                .multi_value_headers
                .get("cookie")
                .cloned()
                .unwrap_or_default()
                .iter()
                .flat_map(parse_cookie)
                .chunk_by(|(k, _)| k.to_string())
                .into_iter()
                .map(|(k, v)| (k, v.into_iter().map(|(_, v)| v.to_string()).collect()))
                .collect(),
            Self::ALBEvent(t) => t
                .multi_value_headers
                .get("cookie")
                .cloned()
                .or_else(|| t.headers.get("cookie").map(|v| vec![v.clone()]))
                .unwrap_or_default()
                .iter()
                .flat_map(parse_cookie)
                .chunk_by(|(k, _)| (*k).to_string())
                .into_iter()
                .map(|(k, v)| (k, v.into_iter().map(|(_, v)| v.to_string()).collect()))
                .collect(),
            Self::LambdaFunctionUrlEvent(t) => {
                t.cookies.as_ref().map(list_to_map).unwrap_or_default()
            }
            // Events that are not relevant to App & API Protection
            Self::MSKEvent(_)
            | Self::SqsRecord(_)
            | Self::SnsRecord(_)
            | Self::DynamoDbRecord(_)
            | Self::S3Record(_)
            | Self::EventBridgeEvent(_)
            | Self::KinesisRecord(_)
            | Self::StepFunctionEvent(_)
            | Self::Unknown => HashMap::default(),
        }
    }
    fn query_params(&self) -> HashMap<String, Vec<String>> {
        match self {
            Self::APIGatewayHttpEvent(t) => t
                .query_string_parameters
                .iter()
                .map(|(k, v)| (k.clone(), v.split(',').map(str::to_string).collect()))
                .collect(),
            Self::APIGatewayRestEvent(t) => t.query_parameters.clone(),
            Self::APIGatewayWebSocketEvent(t) => t.query_parameters.clone(),
            Self::ALBEvent(t) => {
                if t.multi_value_query_parameters.is_empty() {
                    t.query_parameters
                        .iter()
                        .map(|(k, v)| (k.clone(), vec![v.clone()]))
                        .collect()
                } else {
                    t.multi_value_query_parameters.clone()
                }
            }
            Self::LambdaFunctionUrlEvent(t) => t
                .query_string_parameters
                .iter()
                .map(|(k, v)| (k.clone(), vec![v.clone()]))
                .collect(),
            // Events that are not relevant to App & API Protection
            Self::MSKEvent(_)
            | Self::SqsRecord(_)
            | Self::SnsRecord(_)
            | Self::DynamoDbRecord(_)
            | Self::S3Record(_)
            | Self::EventBridgeEvent(_)
            | Self::KinesisRecord(_)
            | Self::StepFunctionEvent(_)
            | Self::Unknown => HashMap::default(),
        }
    }
    fn path_params(&self) -> HashMap<String, String> {
        match self {
            Self::APIGatewayHttpEvent(t) => t.path_parameters.clone(),
            Self::APIGatewayRestEvent(t) => t.path_parameters.clone(),
            Self::APIGatewayWebSocketEvent(t) => t.path_parameters.clone(),
            #[allow(clippy::match_same_arms)]
            Self::ALBEvent(_) => HashMap::default(),
            #[allow(clippy::match_same_arms)]
            Self::LambdaFunctionUrlEvent(_) => HashMap::default(),
            // Events that are not relevant to App & API Protection
            Self::MSKEvent(_)
            | Self::SqsRecord(_)
            | Self::SnsRecord(_)
            | Self::DynamoDbRecord(_)
            | Self::S3Record(_)
            | Self::EventBridgeEvent(_)
            | Self::KinesisRecord(_)
            | Self::StepFunctionEvent(_)
            | Self::Unknown => HashMap::default(),
        }
    }
    fn request_body<'a>(&'a self) -> Option<Box<dyn Read + 'a>> {
        match self {
            Self::APIGatewayHttpEvent(t) => t.body.reader().ok().flatten(),
            Self::APIGatewayRestEvent(t) => t.body.reader().ok().flatten(),
            Self::APIGatewayWebSocketEvent(t) => t.body.reader().ok().flatten(),
            Self::ALBEvent(t) => t.body.reader().ok().flatten(),
            Self::LambdaFunctionUrlEvent(t) => t.body.reader().ok().flatten(),
            // Events that are not relevant to App & API Protection
            Self::MSKEvent(_)
            | Self::SqsRecord(_)
            | Self::SnsRecord(_)
            | Self::DynamoDbRecord(_)
            | Self::S3Record(_)
            | Self::EventBridgeEvent(_)
            | Self::KinesisRecord(_)
            | Self::StepFunctionEvent(_)
            | Self::Unknown => None,
        }
    }
}
*/

#[cfg_attr(coverage_nightly, coverage(off))] // Test modules skew coverage metrics
#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn test_new_with_default_config() {
        let config = Config {
            appsec_enabled: true,
            ..Config::default()
        };
        let _ = Processor::new(&config).expect("Should not fail");
    }

    #[test]
    fn test_new_disabled() {
        let config = Config::default();
        assert!(matches!(
            Processor::new(&config),
            Err(Error::FeatureDisabled)
        ));
    }

    #[test]
    fn test_new_with_invalid_config() {
        let tmp = tempfile::NamedTempFile::new().expect("Failed to create tempfile");

        let config = Config {
            appsec_enabled: true,
            appsec_rules: Some(
                tmp.path()
                    .to_str()
                    .expect("Failed to get tempfile path")
                    .to_string(),
            ),
            ..Config::default()
        };
        assert!(matches!(
            Processor::new(&config),
            Err(Error::WafRulesetParseError(_))
        ));
    }

    #[test]
    fn test_new_with_no_rules_or_processors() {
        let mut tmp = tempfile::NamedTempFile::new().expect("Failed to create tempfile");
        tmp.write_all(
            br#"{
                "version": "2.2",
                "metadata":{
                    "ruleset_version": "0.0.0-blank"
                },
                "scanners":[{
                    "id": "406f8606-52c4-4663-8db9-df70f9e8766c",
                    "name": "ZIP Code",
                    "key": {
                        "operator": "match_regex",
                        "parameters": {
                            "regex": "\\b(?:zip|postal)\\b",
                            "options": {
                                "case_sensitive": false,
                                "min_length": 3
                            }
                        }
                    },
                    "value": {
                        "operator": "match_regex",
                        "parameters": {
                            "regex": "^[0-9]{5}(?:-[0-9]{4})?$",
                            "options": {
                                "case_sensitive": true,
                                "min_length": 5
                            }
                        }
                    },
                    "tags": {
                        "type": "zipcode",
                        "category": "address"
                    }
                }]
            }"#,
        )
        .expect("Failed to write to temp file");
        tmp.flush().expect("Failed to flush temp file");

        let config = Config {
            appsec_enabled: true,
            appsec_rules: Some(
                tmp.path()
                    .to_str()
                    .expect("Failed to get tempfile path")
                    .to_string(),
            ),
            ..Config::default()
        };
        let result = Processor::new(&config);
        assert!(
            matches!(
                result,
                Err(Error::WafInitializationFailed), // There is no rule nor processor in the ruleset
            ),
            concat!(
                "should have failed with ",
                stringify!(Error::WafCreationFailed),
                " but was {:?}"
            ),
            result
        );
    }
}
