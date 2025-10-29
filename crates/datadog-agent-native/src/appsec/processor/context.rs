//! Security context management for WAF execution per request.
//!
//! This module provides the [`Context`] type that tracks the security state of a
//! single HTTP request/response transaction throughout its lifecycle. Each context
//! maintains its own WAF execution state, accumulated security events, and trace tags.
//!
//! # Context Lifecycle
//!
//! ```text
//! 1. Create Context
//!         │
//!         ├─> WAF context initialized
//!         ├─> Trace tags initialized (enabled, version)
//!         └─> Timeout budget allocated
//!         │
//! 2. Analyze Request (run())
//!         │
//!         ├─> Extract addresses from InvocationPayload
//!         ├─> Execute WAF ruleset
//!         ├─> Collect security events (SQL injection, XSS, etc.)
//!         ├─> Update trace tags
//!         └─> Consume timeout budget
//!         │
//! 3. Hold Trace (optional)
//!         │
//!         ├─> Trace cannot be sent yet (response not seen)
//!         └─> Store trace for later processing
//!         │
//! 4. Analyze Response (run())
//!         │
//!         ├─> Extract addresses from response payload
//!         ├─> Execute WAF ruleset (data leakage detection)
//!         └─> Update trace tags
//!         │
//! 5. Finalize (set_response_seen())
//!         │
//!         ├─> Mark response as complete
//!         ├─> Process held trace with accumulated tags
//!         └─> Send trace to aggregator
//! ```
//!
//! # WAF Integration
//!
//! Each context wraps a libddwaf [`WafContext`] that maintains rule execution state.
//! The context can be invoked multiple times (request, response) with different
//! address data, and the WAF accumulates findings across invocations.
//!
//! # Timeout Budget
//!
//! Each context has a timeout budget (default: 5ms) that is consumed across all
//! WAF invocations. When the budget is exhausted, subsequent runs are skipped to
//! prevent request latency impact.
//!
//! # Trace Tag Accumulation
//!
//! Security findings (events, attributes) are collected as trace tags throughout
//! the request lifecycle. Tags are categorized as:
//! - **Always**: Emitted on every trace (method, URL, status code)
//! - **OnEvent**: Emitted only when security events detected (route, client IP)
//! - **Metric**: Numeric metrics (WAF duration, timeouts)
//! - **AppSecEvents**: Security event JSON payload (triggers)
//!
//! # Example
//!
//! ```rust,ignore
//! use datadog_agent_native::appsec::processor::context::Context;
//!
//! // Create context for request
//! let mut ctx = Context::new("req-123".to_string(), &mut waf_handle, "1.0", Duration::from_millis(5));
//!
//! // Analyze request
//! ctx.run(&http_transaction);
//!
//! // Analyze response
//! ctx.run(&http_transaction); // After response is populated
//!
//! // Finalize and process span
//! ctx.set_response_seen().await;
//! ctx.process_span(&mut span);
//! ```

use std::collections::HashMap;
use std::io::{read_to_string, Read};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, Bytes};
use datadog_trace_protobuf::pb::Span;
use datadog_trace_utils::tracer_header_tags;
use libddwaf::object::{Keyed, WafMap, WafObject};
use libddwaf::{waf_map, Context as WafContext, Handle, RunError, RunOutput, RunResult};
use mime::Mime;
use multipart::server::Multipart;
use tracing::{debug, info, warn};

/// Maximum size for AppSec request/response body parsing (10MB).
///
/// Bodies larger than this will be truncated to prevent excessive memory allocation
/// and denial-of-service attacks via large payloads. The WAF will only analyze the
/// first 10MB of request/response bodies.
///
/// # Memory Safety
///
/// This limit prevents:
/// - **Memory exhaustion**: Parsing multi-GB payloads into memory
/// - **DoS attacks**: Malicious actors sending huge bodies to exhaust resources
/// - **Timeout issues**: Large body parsing consuming WAF timeout budget
///
/// # Trade-offs
///
/// Threats in the truncated portion (>10MB) will not be detected. This is an
/// acceptable trade-off as most attacks occur in the first few KB of payload.
const MAX_BODY_SIZE_BYTES: u64 = 10 * 1024 * 1024;

use crate::appsec::processor::response::ExpectedResponseFormat;
use crate::appsec::processor::{InvocationPayload, Processor};
use crate::config::Config;
use crate::tags::provider::Provider;
use crate::traces::span_pointers::SpanPointer;
use crate::traces::trace_processor::SendingTraceProcessor;

/// Security context for a single HTTP request/response transaction.
///
/// This struct tracks the complete security analysis state for one request, including:
/// - WAF execution state and findings
/// - Accumulated trace tags (always, on-event, metrics)
/// - Timeout budget management
/// - Held traces awaiting response completion
///
/// # Lifecycle States
///
/// - **Active**: Request being analyzed, response pending
/// - **Finalized**: Response seen, trace processed and sent
///
/// # Thread Safety
///
/// This type is NOT thread-safe (no internal locking). It's designed for single-threaded
/// processing within the async context of one request handler.
///
/// # Memory Management
///
/// - **WafContext**: Owned WAF context (~1KB overhead)
/// - **Trace Tags**: HashMap of accumulated tags (~100-500 bytes typical)
/// - **Held Trace**: Optional Vec<Span> held until response (0-100KB typical)
///
/// # Must Use
///
/// Marked `#[must_use]` to prevent accidentally dropping contexts that hold traces.
/// Dropping a context before calling `set_response_seen()` will trigger debug assertions.
///
/// # Example
///
/// ```rust,ignore
/// use datadog_agent_native::appsec::processor::context::Context;
///
/// // Create context
/// let mut ctx = Context::new("req-123".to_string(), &mut waf_handle, "1.0", Duration::from_millis(5));
///
/// // Analyze request
/// ctx.run(&http_transaction);
///
/// // Hold trace for later
/// ctx.hold_trace(trace, sender, args);
///
/// // Analyze response
/// ctx.run(&http_transaction_with_response);
///
/// // Finalize
/// ctx.set_response_seen().await; // Processes and sends held trace
/// ```
#[must_use]
pub struct Context {
    /// The request ID of the invocation.
    ///
    /// Used to correlate security events across request/response phases and
    /// to identify contexts in debug logs.
    pub(super) rid: String,

    /// The expected response payload format.
    ///
    /// Detected during first `run()` call based on invocation source (API Gateway, direct HTTP, etc.).
    /// Used to parse response payloads correctly in multi-invocation scenarios.
    #[allow(dead_code)]
    pub(super) expected_response_format: ExpectedResponseFormat,

    /// The libddwaf context for this invocation.
    ///
    /// Maintains WAF rule execution state. Can be invoked multiple times with different
    /// address data (request, response), accumulating findings across invocations.
    #[allow(dead_code)]
    waf: WafContext,

    /// The remaining WAF timeout budget for this request.
    ///
    /// Starts at the configured timeout (default: 5ms) and is decremented after each
    /// WAF run. When exhausted, subsequent runs are skipped.
    #[allow(dead_code)]
    waf_timeout: Duration,

    /// The total time spent executing the WAF for this request.
    ///
    /// Accumulated across all `run()` invocations. Emitted as a span metric
    /// (`_dd.appsec.waf.duration`) for performance monitoring.
    waf_duration: Duration,

    /// The number of times the WAF timed out during this request.
    ///
    /// Incremented when WAF execution exceeds the timeout or when the budget is
    /// exhausted before invocation. Emitted as a span metric (`_dd.appsec.waf.timeouts`).
    waf_timed_out_occurrences: usize,

    /// Whether this context has detected security events.
    ///
    /// Set to `true` when the WAF detects threats (SQL injection, XSS, etc.).
    /// Controls whether "OnEvent" tags are included in the trace.
    has_events: bool,

    /// Trace tags to be applied for this invocation.
    ///
    /// Accumulated throughout request/response analysis. Includes:
    /// - **Always tags**: http.method, http.url, http.status_code
    /// - **OnEvent tags**: http.route, network.client.ip (only if has_events)
    /// - **Metrics**: _dd.appsec.waf.duration, _dd.appsec.waf.timeouts
    /// - **AppSecEvents**: _dd.appsec.json (security event JSON)
    trace_tags: HashMap<TagName, TagValue>,

    /// Whether the response for this request has been seen.
    ///
    /// Transitions from `false` → `true` via `set_response_seen()`. Once true,
    /// no further WAF runs should occur, and held traces can be processed.
    response_seen: bool,

    /// Holds a trace for sending once the response is seen.
    ///
    /// In standalone AppSec mode, traces arrive before the response is analyzed.
    /// This field stores the trace until `set_response_seen()` is called, at which
    /// point the accumulated security tags are applied and the trace is sent.
    ///
    /// Format: `(trace, sender, tracer_header_args)`
    held_trace: Option<(Vec<Span>, SendingTraceProcessor, HoldArguments)>,
}
impl Context {
    /// Creates a new security context for the provided request ID.
    ///
    /// This method initializes a fresh context with a new WAF execution context,
    /// timeout budget, and baseline trace tags.
    ///
    /// # Arguments
    ///
    /// * `rid` - Request ID for correlation and logging
    /// * `waf_handle` - Handle to the shared WAF instance
    /// * `ruleset_version` - Version string of the loaded ruleset (e.g., "1.2.3")
    /// * `waf_timeout` - Total timeout budget for WAF execution (default: 5ms)
    ///
    /// # Returns
    ///
    /// A new context in the "Active" state (response_seen = false).
    ///
    /// # Baseline Tags
    ///
    /// Initializes these tags:
    /// - `_dd.appsec.enabled`: 1.0 (metric)
    /// - `_dd.appsec.waf.version`: libddwaf version string
    /// - `_dd.appsec.event_rules.version`: Ruleset version (if non-empty)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let ctx = Context::new(
    ///     "req-123".to_string(),
    ///     &mut waf_handle,
    ///     "1.2.3",
    ///     Duration::from_millis(5)
    /// );
    /// ```
    pub(crate) fn new(
        rid: String,
        waf_handle: &mut Handle,
        ruleset_version: &str,
        waf_timeout: Duration,
    ) -> Self {
        // Initialize baseline trace tags for AppSec telemetry
        let mut trace_tags = HashMap::from([
            // Metric tag indicating AppSec is enabled
            (TagName::AppsecEnabled, TagValue::Metric(1.0)),
            // WAF version tag for debugging and compatibility tracking
            (
                TagName::AppsecWafVersion,
                TagValue::Always(libddwaf::get_version().to_string_lossy().to_string()),
            ),
        ]);

        // Add ruleset version if provided
        // Decision: Empty string means no custom ruleset, use default
        if !ruleset_version.is_empty() {
            trace_tags.insert(
                TagName::AppsecEventRulesVersion,
                TagValue::Always(ruleset_version.to_string()),
            );
        }

        Self {
            rid,
            // Unknown until first run() call determines invocation source
            expected_response_format: ExpectedResponseFormat::default(),
            // Create new WAF context for this request
            waf: waf_handle.new_context(),
            waf_timeout,
            // Tracking starts at zero
            waf_duration: Duration::ZERO,
            waf_timed_out_occurrences: 0,
            has_events: false,
            trace_tags,
            // Response not yet seen
            response_seen: false,
            // No held trace yet
            held_trace: None,
        }
    }

    /// Checks if the response for this request is still pending.
    ///
    /// This method determines whether the context is still waiting for the
    /// response phase to complete. While pending, traces may be held and
    /// additional WAF runs may occur.
    ///
    /// # Returns
    ///
    /// * `true` - Response not yet seen, context is still active
    /// * `false` - Response has been seen, context is finalized
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if ctx.is_pending_response() {
    ///     // Can still analyze response data
    ///     ctx.run(&http_transaction_with_response);
    /// }
    /// ```
    pub(crate) const fn is_pending_response(&self) -> bool {
        !self.response_seen
    }

    /// Marks the response as seen and processes any held trace.
    ///
    /// This method transitions the context from "Active" to "Finalized" state.
    /// If a trace was held via `hold_trace()`, it is now processed with the
    /// accumulated security tags and sent to the trace aggregator.
    ///
    /// # Lifecycle
    ///
    /// 1. Check if already finalized (idempotent)
    /// 2. If trace held:
    ///    a. Apply accumulated security tags to service entry span
    ///    b. Send trace to aggregator with tracer header tags
    ///    c. Log success or failure
    /// 3. Mark response as seen
    ///
    /// # Side Effects
    ///
    /// - Modifies `self.response_seen` to `true`
    /// - Consumes `self.held_trace` (take ownership)
    /// - Sends trace to aggregator (async network I/O)
    ///
    /// # Async
    ///
    /// This method is async because it may send traces to the aggregator,
    /// which involves network I/O.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // After response analysis complete
    /// ctx.set_response_seen().await;
    /// // Context is now finalized, trace has been sent
    /// ```
    pub(super) async fn set_response_seen(&mut self) {
        // Check if already finalized (idempotent safety)
        // Decision: Allow multiple calls to avoid runtime errors in edge cases
        if self.response_seen {
            return;
        }

        // Check if we have a held trace to process
        // Decision: take() moves ownership and sets held_trace to None
        if let Some((mut trace, sender, args)) = self.held_trace.take() {
            // Find the service entry span (root span) to apply security tags
            if let Some(span) = Processor::service_entry_span_mut(&mut trace) {
                // Debug assertion: verify we're holding the correct trace
                // Decision: Debug-only check to catch logic errors during development
                debug_assert_eq!(
                    span.meta.get("request_id").map(String::as_str),
                    Some(self.rid.as_str())
                );

                debug!(
                    "aap: processing trace for request {} now that the response was seen",
                    self.rid
                );

                // Apply all accumulated security tags to the span
                self.process_span(span);
            }

            debug!("aap: flushing out trace for request {}", self.rid);

            // Send the processed trace to the aggregator
            // Decision: Use match to handle both success and failure cases explicitly
            match sender
                .send_processed_traces(
                    args.config,
                    args.tags_provider,
                    tracer_header_tags::TracerHeaderTags {
                        lang: &args.tracer_header_tags_lang,
                        lang_version: &args.tracer_header_tags_lang_version,
                        lang_interpreter: &args.tracer_header_tags_lang_interpreter,
                        lang_vendor: &args.tracer_header_tags_lang_vendor,
                        tracer_version: &args.tracer_header_tags_tracer_version,
                        container_id: &args.tracer_header_tags_container_id,
                        client_computed_top_level: args
                            .tracer_header_tags_client_computed_top_level,
                        client_computed_stats: args.tracer_header_tags_client_computed_stats,
                        dropped_p0_traces: args.tracer_header_tags_dropped_p0_traces,
                        dropped_p0_spans: args.tracer_header_tags_dropped_p0_spans,
                    },
                    vec![trace],
                    args.body_size,
                    args.span_pointers,
                )
                .await
            {
                // Success: trace sent to aggregator buffer
                Ok(()) => debug!("aap: successfully sent trace to aggregator buffer"),
                // Failure: log warning but continue (don't crash on telemetry failures)
                // Decision: Telemetry failures should not impact application availability
                Err(e) => warn!("aap: failed to send trace to aggregator buffer: {e}"),
            }
        }

        debug!(
            "aap: marking security context for {} as having seen the response...",
            self.rid
        );
        // Transition to finalized state
        self.response_seen = true;
    }

    /// Holds a trace for future processing once the response is seen.
    ///
    /// In standalone AppSec mode, traces arrive before the response is fully
    /// analyzed. This method stores the trace until `set_response_seen()` is
    /// called, at which point accumulated security tags are applied.
    ///
    /// # Arguments
    ///
    /// * `trace` - The trace spans to hold
    /// * `sender` - Channel for sending processed traces to aggregator
    /// * `args` - Tracer header arguments needed for sending
    ///
    /// # Panics (debug mode)
    ///
    /// Panics in debug mode if called after `set_response_seen()` has been
    /// invoked. In release mode, logs a warning and returns early.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Trace arrives before response analysis complete
    /// ctx.hold_trace(trace, sender, args);
    ///
    /// // Later, after response analysis
    /// ctx.set_response_seen().await; // Processes held trace
    /// ```
    pub(crate) fn hold_trace(
        &mut self,
        trace: Vec<Span>,
        sender: SendingTraceProcessor,
        args: HoldArguments,
    ) {
        // Verify this is being called at the correct lifecycle stage
        // Decision: Holding a trace after response is seen is a logic error
        if !self.is_pending_response() {
            unreachable_warn("Context::hold_trace called after response was seen!");
            return;
        }

        // Store trace for later processing
        // Decision: Overwrite any existing held trace (shouldn't happen in practice)
        self.held_trace = Some((trace, sender, args));
    }

    /// Executes the WAF ruleset against the provided invocation payload.
    ///
    /// This method extracts HTTP address data (URI, headers, body, etc.) from the
    /// payload, runs the WAF ruleset, and collects security events and trace tags.
    /// It can be called multiple times per request (once for request, once for response).
    ///
    /// # Arguments
    ///
    /// * `payload` - The HTTP transaction data (request and/or response)
    ///
    /// # Execution Flow
    ///
    /// 1. Check timeout budget (skip if exhausted)
    /// 2. Update expected response format (first call only)
    /// 3. Collect span tags from payload
    /// 4. Extract WAF address data
    /// 5. Execute WAF ruleset
    /// 6. Process results (events, actions, attributes)
    ///
    /// # Timeout Budget
    ///
    /// The WAF has a per-request timeout budget (default: 5ms) that is consumed
    /// across all invocations. When exhausted, subsequent calls are skipped to
    /// prevent request latency impact.
    ///
    /// # Side Effects
    ///
    /// - Decrements `self.waf_timeout` by execution duration
    /// - Increments `self.waf_duration` (total time spent)
    /// - Updates `self.trace_tags` with findings
    /// - May set `self.has_events` to `true` if threats detected
    ///
    /// # Panics (debug mode)
    ///
    /// Panics in debug mode if called after `set_response_seen()`. In release mode,
    /// logs a warning and returns early.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Analyze request
    /// ctx.run(&http_transaction);
    ///
    /// // Later, analyze response
    /// ctx.run(&http_transaction_with_response);
    /// ```
    pub(super) fn run(&mut self, payload: &dyn InvocationPayload) {
        // Verify WAF is not run after context finalization
        // Decision: Running WAF after response finalization is a logic error
        if self.response_seen {
            unreachable_warn("aap: Context::run called after response was seen!");
        }

        // Check if timeout budget is exhausted
        // Decision: Skip WAF execution rather than run with 0 timeout
        if self.waf_timeout.is_zero() {
            // Log as INFO rather than WARN to avoid alarm fatigue
            // Decision: Customers may intentionally set low timeouts for performance
            info!(
                "aap: WAF execution time budget for this request is exhausted, skipping WAF ruleset evaluation entirely (consider tweaking DD_APPSEC_WAF_TIMEOUT)"
            );

            // Count this as a timeout even though we didn't actually invoke WAF
            // Decision: Important to track budget exhaustion for tuning guidance
            self.waf_timed_out_occurrences += 1;
            return;
        }

        // Detect response format on first run (API Gateway vs direct HTTP)
        // Decision: Format detection happens once and persists for subsequent runs
        if self.expected_response_format.is_unknown() {
            self.expected_response_format = payload.corresponding_response_format();
        }

        // Extract span tag information from the payload
        // Decision: Tags collected before WAF run so they're available even if WAF fails
        self.collect_span_tags(payload);

        // Extract address data for WAF (URI, headers, body, etc.)
        // Decision: Early return if no address data (nothing for WAF to analyze)
        let Some(address_data) = to_address_data(payload) else {
            // Produced no address data, nothing more to do...
            return;
        };

        // Execute the WAF ruleset with extracted address data
        // Decision: Both Match and NoMatch contain useful result data (duration, actions)
        match self.waf.run(Some(address_data), None, self.waf_timeout) {
            // WAF completed successfully (with or without matches)
            Ok(RunResult::Match(result) | RunResult::NoMatch(result)) => {
                self.process_result(result);
            }
            // WAF execution failed (internal error)
            // Decision: Log error but continue (don't crash on security monitoring failures)
            Err(e) => log_waf_run_error(e),
        };
    }

    /// Retrieves endpoint information for API Security sampling.
    ///
    /// This method extracts the HTTP method, route pattern, and status code from
    /// the accumulated trace tags. Used by the API Security sampler to compute
    /// per-endpoint sampling decisions.
    ///
    /// # Returns
    ///
    /// A tuple of `(method, route, status_code)` as strings. Empty strings are
    /// returned for missing values.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let (method, route, status) = ctx.endpoint_info();
    /// // => ("GET", "/users/{id}", "200")
    ///
    /// let should_sample = sampler.decision_for(&method, &route, &status);
    /// ```
    pub(super) fn endpoint_info(&self) -> (String, String, String) {
        (
            // HTTP method (GET, POST, etc.)
            self.trace_tags
                .get(&TagName::HttpMethod)
                .map(TagValue::to_string)
                .unwrap_or_default(),
            // Route pattern (/users/{id}, etc.)
            self.trace_tags
                .get(&TagName::HttpRoute)
                .map(TagValue::to_string)
                .unwrap_or_default(),
            // HTTP status code (200, 404, etc.)
            self.trace_tags
                .get(&TagName::HttpStatusCode)
                .map(TagValue::to_string)
                .unwrap_or_default(),
        )
    }

    /// Enables API Security schema extraction for this context.
    ///
    /// This method triggers a special WAF run that extracts API schemas (request/response
    /// structure) instead of performing threat detection. Used for API discovery and
    /// documentation generation.
    ///
    /// # Schema Extraction
    ///
    /// The WAF processor is instructed to extract:
    /// - Request body schema (JSON/form structure)
    /// - Response body schema
    /// - Parameter types and formats
    ///
    /// Extracted schemas are returned as WAF attributes and added to trace tags.
    ///
    /// # Timeout Budget
    ///
    /// Schema extraction consumes the same timeout budget as threat detection.
    /// If the budget is exhausted, this operation is skipped.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // After normal WAF analysis
    /// if should_extract_schema {
    ///     ctx.extract_schemas();
    /// }
    /// ```
    pub(super) fn extract_schemas(&mut self) {
        // Create special address data instructing WAF to extract schemas
        // Decision: Use waf.context.processor address to control WAF behavior
        let address_data = waf_map! {
            ("waf.context.processor", waf_map! {
                ("extract-schema", true)
            })
        };

        // Run WAF in schema extraction mode
        // Decision: Both Match and NoMatch can contain schema data
        match self.waf.run(Some(address_data), None, self.waf_timeout) {
            Ok(RunResult::Match(result) | RunResult::NoMatch(result)) => {
                self.process_result(result);
            }
            // Log errors but don't crash on schema extraction failures
            // Decision: Schema extraction is a nice-to-have feature, not critical
            Err(e) => log_waf_run_error(e),
        }
    }

    /// Applies accumulated security tags and metrics to a span.
    ///
    /// This method transfers all collected security information from the context
    /// to a trace span, making it available for Datadog analysis and alerting.
    ///
    /// # Tag Categories
    ///
    /// - **Always tags**: Added unconditionally (method, URL, WAF version)
    /// - **OnEvent tags**: Added only if threats detected (route, client IP)
    /// - **Metrics**: Numeric values (WAF duration, timeouts)
    /// - **AppSecEvents**: JSON payload with security event details
    ///
    /// # Origin Tag
    ///
    /// Sets `_dd.origin = "appsec"` to mark the span as AppSec-generated,
    /// ensuring proper routing and priority handling in the Datadog backend.
    ///
    /// # Arguments
    ///
    /// * `span` - The span to annotate with security tags
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut span = create_span();
    /// ctx.process_span(&mut span);
    /// // Span now has all accumulated security tags
    /// ```
    pub(super) fn process_span(&self, span: &mut Span) {
        debug!(
            "aap: setting up to {} span tags/metrics on span {}",
            self.trace_tags.len(),
            span.span_id
        );

        // Iterate through all accumulated trace tags
        for (key, value) in &self.trace_tags {
            match value {
                // Always tags: included unconditionally
                TagValue::Always(value) => {
                    span.meta.insert(key.to_string(), value.clone());
                }

                // AppSec events: JSON structure with triggers array
                TagValue::AppSecEvents { triggers } => {
                    // Serialize triggers as JSON object with "triggers" key
                    // Decision: Use serde_json for proper JSON encoding
                    span.meta.insert(
                        key.to_string(),
                        serde_json::Value::Object(serde_json::Map::from_iter([(
                            "triggers".to_string(),
                            serde_json::Value::Array(triggers.clone()),
                        )]))
                        .to_string(),
                    );
                }

                // OnEvent tags: only included if security events detected
                TagValue::OnEvent(value) => {
                    // Skip if no events detected
                    // Decision: Reduce trace size when no threats found
                    if !self.has_events {
                        continue;
                    }

                    // Handle both string and non-string JSON values
                    match value {
                        serde_json::Value::String(value) => {
                            span.meta.insert(key.to_string(), value.clone());
                        }
                        value => {
                            // Convert non-string values to JSON string
                            span.meta.insert(key.to_string(), value.to_string());
                        }
                    }
                }

                // Metrics: numeric span metrics
                TagValue::Metric(value) => {
                    span.metrics.insert(key.to_string(), *value);
                }
            }
        }

        // Add WAF performance metrics (always included)
        span.metrics.insert(
            TagName::AppsecWafDuration.to_string(),
            self.waf_duration.as_micros() as f64,
        );
        span.metrics.insert(
            TagName::AppsecWafTimeouts.to_string(),
            self.waf_timed_out_occurrences as f64,
        );

        // Set origin tag if not already present
        // Decision: Allow clippy::map_entry to keep debug log statement
        #[allow(clippy::map_entry)]
        if !span.meta.contains_key(&TagName::Origin.to_string()) {
            debug!("aap: setting span tag {}:appsec", TagName::Origin);
            // Mark span as AppSec-generated for proper backend routing
            span.meta
                .insert(TagName::Origin.to_string(), "appsec".to_string());
        }
    }

    /// Request headers that are always collected as long as AAP is enabled.
    ///
    /// This list should contain lowercase-normalized header names.
    ///
    /// See: <https://datadoghq.atlassian.net/wiki/spaces/SAAL/pages/2186870984/HTTP+header+collection>.
    const REQUEST_HEADERS_ON_EVENT: &[&str] = &[
        // IP address releated headers
        "x-forwarded-for",
        "x-real-ip",
        "true-client-ip",
        "x-client-ip",
        "x-forwarded",
        "forwarded-for",
        "x-cluster-client-ip",
        "fastly-client-ip",
        "cf-connecting-ip",
        "cf-connecting-ipv6",
        "forwarded",
        "via",
        // Message body information
        "content-length",
        "content-encoding",
        "content-language",
        // Host request context
        "host",
        // Content negotiation
        "accept-encoding",
        "accept-language",
    ];
    /// Request headers that are collected only if AAP is enabled, and security activity has been
    /// detected.
    ///
    /// This list should contain lowercase-normalized header names.
    ///
    /// See: <https://datadoghq.atlassian.net/wiki/spaces/SAAL/pages/2186870984/HTTP+header+collection>.
    const REQUEST_HEADERS_ALWAYS: &[&str] = &[
        // Message body information
        "content-type",
        // Client user agent
        "user-agent",
        // Content negotiation
        "accept",
        // AWS WAF logs to traces (RFC 0996)
        "x-amzn-trace-id",
        // WAF Integration - Identify Requests (RFC 0992)
        "cloudfront-viewer-ja3-fingerprint",
        "cf-ray",
        "x-cloud-trace-context",
        "x-appgw-trace-id",
        "x-sigsci-requestid",
        "x-sigsci-tags",
        "akamai-user-risk",
    ];
    /// Response headers that are always collected as long as AAP is enabled.
    ///
    /// See: <https://datadoghq.atlassian.net/wiki/spaces/SAAL/pages/2186870984/HTTP+header+collection>.
    const RESPONSE_HEADERS_ALWAYS: &[&str] = &[
        // Message body information
        "content-length",
        "content-type",
        "content-encoding",
        "content-language",
    ];

    /// Extracts span tags from the invocation payload.
    ///
    /// This method collects HTTP metadata (method, URI, headers, etc.) from the
    /// payload and stores it as trace tags. Tags are categorized as either "Always"
    /// (included unconditionally) or "OnEvent" (included only when threats detected).
    ///
    /// # Tag Categories
    ///
    /// **Always Tags** (low-cardinality):
    /// - HTTP method, URL, status code
    /// - Security-relevant request headers (content-type, user-agent, etc.)
    /// - Response headers (content-type, content-length, etc.)
    ///
    /// **OnEvent Tags** (potentially high-cardinality):
    /// - Route pattern (may vary per request)
    /// - Client IP (privacy concern, only on threats)
    /// - IP-related headers (X-Forwarded-For, etc.)
    ///
    /// # Arguments
    ///
    /// * `payload` - The HTTP transaction to extract tags from
    ///
    /// # Header Collection
    ///
    /// See Datadog's [HTTP Header Collection Policy](https://datadoghq.atlassian.net/wiki/spaces/SAAL/pages/2186870984/HTTP+header+collection)
    /// for the rationale behind which headers are collected.
    fn collect_span_tags(&mut self, payload: &dyn InvocationPayload) {
        // Macro for extracting simple field tags
        // Decision: Use macros to reduce boilerplate for field extraction
        macro_rules! span_tag {
            // OnEvent tags: included only when security events detected
            ($field:ident on event => $name:expr) => {
                if let Some(value) = payload.$field() {
                    self.trace_tags.insert(
                        $name,
                        TagValue::OnEvent(serde_json::Value::String(value.clone())),
                    );
                }
            };
            // Always tags: included unconditionally
            ($field:ident => $name:expr) => {
                if let Some(value) = payload.$field() {
                    self.trace_tags
                        .insert($name, TagValue::Always(value.to_string()));
                }
            };
        }

        // Request span tags
        span_tag!(raw_uri => TagName::HttpUrl); // Always (low cardinality)
        span_tag!(method => TagName::HttpMethod); // Always (low cardinality: GET/POST/PUT/etc.)
        span_tag!(route on event => TagName::HttpRoute); // OnEvent (may be high cardinality)
        span_tag!(client_ip on event => TagName::NetworkClientIp); // OnEvent (privacy concern)

        // Response span tags
        span_tag!(response_status_code => TagName::HttpStatusCode); // Always (low cardinality: 200/404/500/etc.)

        // Collect request headers
        let headers = payload.request_headers_no_cookies();
        if !headers.is_empty() {
            // Always-collected request headers (security-relevant, low cardinality)
            for name in Self::REQUEST_HEADERS_ALWAYS {
                // Skip if header not present
                let Some(values) = headers.get(*name) else {
                    continue;
                };
                // Join multi-value headers with comma separator
                // Decision: Comma-separated as per HTTP spec for multi-value headers
                self.trace_tags.insert(
                    TagName::Dynamic(format!("http.request.headers.{name}")),
                    TagValue::Always(values.join(",")),
                );
            }

            // OnEvent-collected request headers (may be high cardinality)
            for name in Self::REQUEST_HEADERS_ON_EVENT {
                let Some(values) = headers.get(*name) else {
                    continue;
                };
                self.trace_tags.insert(
                    TagName::Dynamic(format!("http.request.headers.{name}")),
                    TagValue::OnEvent(serde_json::Value::String(values.join(","))),
                );
            }
        }

        // Collect response headers (always included, low cardinality)
        let headers = payload.response_headers_no_cookies();
        if !headers.is_empty() {
            for name in Self::RESPONSE_HEADERS_ALWAYS {
                let Some(values) = headers.get(*name) else {
                    continue;
                };
                self.trace_tags.insert(
                    TagName::Dynamic(format!("http.response.headers.{name}")),
                    TagValue::Always(values.join(",")),
                );
            }
        }
    }

    /// Processes WAF execution results and updates context state.
    ///
    /// This method handles the output from a WAF run, including:
    /// - Timeout budget management
    /// - Sampling priority updates
    /// - Security event collection
    /// - Synthetic attribute extraction
    /// - Action handling (blocking, redirecting, etc.)
    ///
    /// # Arguments
    ///
    /// * `result` - The output from a WAF ruleset evaluation
    ///
    /// # Side Effects
    ///
    /// - Decrements `self.waf_timeout` by execution duration
    /// - Increments `self.waf_duration` (total time spent)
    /// - May set `self.has_events = true` if threats detected
    /// - Updates `self.trace_tags` with events and attributes
    /// - May set sampling priority to USER_KEEP
    fn process_result(&mut self, result: RunOutput) {
        // Sampling priority constant for USER_KEEP (force sampling)
        const SAMPLING_PRIORITY_USER_KEEP: f64 = 2.0;

        // Update timeout budget based on WAF execution duration
        let duration = result.duration();
        // Decision: Use saturating_sub to prevent underflow (minimum is 0)
        self.waf_timeout = self.waf_timeout.saturating_sub(duration);
        self.waf_duration += duration;

        debug!(
            "aap: WAF ruleset evaluation took {:?}, the remaining WAF budget is {:?} (total time spent so far: {:?})",
            duration, self.waf_timeout, self.waf_duration
        );

        // Check if WAF timed out during execution
        // Decision: Log as INFO to avoid alarm fatigue for customers with low timeouts
        if result.timeout() {
            info!(
                "aap: time out reached while evaluating the WAF ruleset; detections may be incomplete. Consider tuning DD_APPSEC_WAF_TIMEOUT"
            );
            self.waf_timed_out_occurrences += 1;
        }

        // Check if WAF requested trace sampling priority upgrade
        // Decision: WAF can force traces to be kept for important security events
        if result.keep() {
            debug!("aap: a WAF rule requested the trace to be upgraded to USER_KEEP priority");
            self.trace_tags.insert(
                TagName::SamplingPriorityV1,
                TagValue::Metric(SAMPLING_PRIORITY_USER_KEEP),
            );
        }

        // Extract synthetic attributes from WAF (API schemas, extracted data)
        if let Some(attributes) = result.attributes() {
            for item in attributes.iter() {
                // Get attribute name
                // Decision: Attributes with invalid UTF-8 names are skipped
                let name = match item.key_str() {
                    Ok(name) => name,
                    Err(e) => {
                        warn!(
                            "aap: falied to convert attribute name to string, the attribute will be ignored: {e}"
                        );
                        continue;
                    }
                };

                // Get attribute value (try string first, fall back to JSON)
                // Decision: String values are preferred for efficiency
                let value = if let Some(value) = item.to_str() {
                    value.to_string()
                } else {
                    // Fall back to JSON serialization for complex types
                    match serde_json::to_string(item.inner()) {
                        Ok(value) => value,
                        Err(e) => {
                            warn!(
                                "aap: failed to convert attribute {name:#?} value to JSON, the attribute will be ignored: {e}\n{item:?}"
                            );
                            continue;
                        }
                    }
                };

                debug!("aap: produced synthetic tag {name}:{value}");
                // Add attribute as always-included trace tag
                self.trace_tags
                    .insert(TagName::Dynamic(name.to_string()), TagValue::Always(value));
            }
        }

        // Extract security events (threats detected)
        if let Some(events) = result.events() {
            debug!("aap: WAF ruleset detected {} events", events.len());

            // Process events if any were detected
            // Decision: Empty event array means no threats (skip processing)
            if !events.is_empty() {
                // Set appsec.event tag to indicate threats were found
                self.trace_tags
                    .insert(TagName::AppsecEvent, TagValue::Always("true".to_string()));
                self.has_events = true;

                // Get or create the AppSecEvents entry
                // Decision: Use entry API for efficiency (avoid double lookup)
                let entry =
                    self.trace_tags
                        .entry(TagName::AppsecJson)
                        .or_insert(TagValue::AppSecEvents {
                            triggers: Vec::with_capacity(events.len()),
                        });

                // Extract triggers vec (should always be AppSecEvents variant)
                let TagValue::AppSecEvents { triggers } = entry else {
                    unreachable!(
                        "the {} tag entry is always a TagValue::AppSecEvents{{...}}",
                        TagName::AppsecJson
                    );
                };

                // Reserve capacity for all events
                triggers.reserve(events.len());

                // Convert each event to JSON and add to triggers array
                for event in events.iter() {
                    // Decision: JSON serialization allows backend to parse event details
                    let value = match serde_json::to_value(event) {
                        Ok(value) => value,
                        Err(e) => {
                            warn!(
                                "aap: failed to convert event to JSON, the event will be dropped: {e}\n{event:?}"
                            );
                            continue;
                        }
                    };
                    triggers.push(value);
                }
            }
        }

        // Handle WAF actions (blocking, redirecting, etc.)
        // Decision: Log warning as actions are not yet supported in general agent
        if let Some(actions) = result.actions() {
            for action in actions.iter() {
                warn!(
                    "aap: WAF ruleset triggered actions that are currently ignored by Serverless AAP: {action:?}"
                );
            }
        }
    }
}

/// Cleanup implementation that ensures no traces are accidentally dropped.
///
/// This implementation provides defensive checks to catch logic errors where
/// a context is dropped while still holding a trace that hasn't been sent.
/// In debug mode, this will panic to fail fast. In release mode, it logs a
/// warning but continues execution.
///
/// # Safety Checks
///
/// 1. **Gentle nudge (always)**: Logs debug message if response not marked as seen
/// 2. **Hard crash (debug only)**: Panics if dropping a context with an unsent trace
///
/// # Rationale
///
/// Dropping a trace means security events will not be reported to Datadog.
/// This is a serious issue that should be caught during development.
impl Drop for Context {
    fn drop(&mut self) {
        // Check if response was marked as seen before drop
        // Decision: Warn in all builds to catch potential trace loss
        if !self.response_seen {
            debug!(
                "aap: Context being dropped without the response being marked as seen, this may cause traces to be dropped"
            );
        }

        // In debug assertions mode, hard-crash if dropping an unsent trace
        // Decision: Fail fast in development to catch logic errors early
        debug_assert!(
            self.response_seen || self.held_trace.is_none(),
            "aap: Context is being dropped without the response being marked as seen. A trace will be dropped!"
        );
    }
}

/// Arguments needed to send a held trace to the aggregator.
///
/// This struct bundles all the metadata required for the `send_processed_traces`
/// call. It's stored alongside the held trace so that when `set_response_seen()`
/// is called, the trace can be sent with all the correct headers and metadata.
///
/// # Fields
///
/// - **config**: Agent configuration (endpoints, API keys, etc.)
/// - **tags_provider**: Global tag provider (env, hostname, etc.)
/// - **body_size**: Size of the trace payload in bytes
/// - **span_pointers**: Optional span pointers for trace aggregation
/// - **tracer_header_tags_***: Tracer metadata from the client library
///
/// # Usage
///
/// ```rust,ignore
/// let args = HoldArguments {
///     config: config.clone(),
///     tags_provider: tags.clone(),
///     body_size: payload.len(),
///     span_pointers: Some(pointers),
///     tracer_header_tags_lang: "rust".to_string(),
///     // ... other tracer headers
/// };
///
/// context.hold_trace(trace, sender, args);
/// ```
pub struct HoldArguments {
    /// Agent configuration (API keys, endpoints, etc.)
    pub config: Arc<Config>,

    /// Global tag provider (environment, hostname, etc.)
    pub tags_provider: Arc<Provider>,

    /// Size of the trace payload in bytes
    pub body_size: usize,

    /// Optional span pointers for trace aggregation
    pub span_pointers: Option<Vec<SpanPointer>>,

    // Tracer header metadata from client library
    /// Language of the traced application (e.g., "rust", "python", "java")
    pub tracer_header_tags_lang: String,

    /// Language version (e.g., "1.75.0" for Rust)
    pub tracer_header_tags_lang_version: String,

    /// Language interpreter/runtime (e.g., "rustc", "CPython")
    pub tracer_header_tags_lang_interpreter: String,

    /// Language vendor (e.g., "rust-lang", "python")
    pub tracer_header_tags_lang_vendor: String,

    /// Datadog tracer library version (e.g., "0.6.0")
    pub tracer_header_tags_tracer_version: String,

    /// Container ID if running in a container
    pub tracer_header_tags_container_id: String,

    /// Whether the client computed top-level spans
    pub tracer_header_tags_client_computed_top_level: bool,

    /// Whether the client computed stats
    pub tracer_header_tags_client_computed_stats: bool,

    /// Number of P0 traces dropped by the client
    pub tracer_header_tags_dropped_p0_traces: usize,

    /// Number of P0 spans dropped by the client
    pub tracer_header_tags_dropped_p0_spans: usize,
}

/// Tag name enum for type-safe trace tag management.
///
/// This enum provides a type-safe way to manage trace tag names, preventing
/// typos and making it easier to track which tags are used throughout the codebase.
///
/// # Variants
///
/// **AppSec Tags**:
/// - `AppsecEnabled`: Metric indicating AppSec is enabled
/// - `AppsecEvent`: Boolean tag set when threats detected
/// - `AppsecEventRulesVersion`: Version of loaded ruleset
/// - `AppsecJson`: JSON payload with security events
/// - `AppsecWafDuration`: Total WAF execution time (microseconds)
/// - `AppsecWafTimeouts`: Number of WAF timeouts
/// - `AppsecWafVersion`: libddwaf version string
///
/// **General Tags**:
/// - `HttpUrl`, `HttpMethod`, `HttpRoute`, `HttpStatusCode`: HTTP metadata
/// - `NetworkClientIp`: Client IP address
/// - `SamplingPriorityV1`: Trace sampling priority override
/// - `Origin`: Trace origin marker ("appsec")
///
/// **Dynamic Tags**:
/// - `Dynamic(String)`: Runtime-defined tag name (e.g., headers, custom attributes)
///
/// # String Conversion
///
/// All variants convert to their canonical Datadog tag names via `as_str()`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TagName {
    // AppSec tags
    AppsecEnabled,
    AppsecEvent,
    AppsecEventRulesVersion,
    AppsecJson,
    AppsecWafDuration,
    AppsecWafTimeouts,
    AppsecWafVersion,
    // Hidden span tags of relevance
    Origin,
    // General request tags
    HttpUrl,
    HttpMethod,
    HttpRoute,
    HttpStatusCode,
    NetworkClientIp,
    // Special tags
    SamplingPriorityV1,
    /// A tag name that is not statically known.
    Dynamic(String),
}
impl TagName {
    fn as_str(&self) -> &str {
        match self {
            Self::AppsecEnabled => "_dd.appsec.enabled",
            Self::AppsecEvent => "appsec.event",
            Self::AppsecEventRulesVersion => "_dd.appsec.event_rules.version",
            Self::AppsecJson => "_dd.appsec.json",
            Self::AppsecWafDuration => "_dd.appsec.waf.duration",
            Self::AppsecWafTimeouts => "_dd.appsec.waf.timeouts",
            Self::AppsecWafVersion => "_dd.appsec.waf.version",
            Self::Origin => "_dd.origin",
            Self::HttpUrl => "http.url",
            Self::HttpMethod => "http.method",
            Self::HttpRoute => "http.route",
            Self::HttpStatusCode => "http.status_code",
            Self::NetworkClientIp => "network.client.ip",
            Self::SamplingPriorityV1 => "_sampling_priority_v1",
            Self::Dynamic(name) => name,
        }
    }
}
impl std::fmt::Display for TagName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Tag value enum representing different emission strategies.
///
/// This enum categorizes trace tag values based on when they should be included
/// in spans. The goal is to minimize trace size while ensuring all security-relevant
/// data is captured when threats are detected.
///
/// # Variants
///
/// ## Always(String)
/// Included in every trace, regardless of whether threats were detected.
/// Use for low-cardinality, always-relevant data:
/// - HTTP method (GET/POST/PUT/etc.)
/// - HTTP status code (200/404/500/etc.)
/// - WAF version
///
/// ## OnEvent(serde_json::Value)
/// Included only when `has_events = true` (threats detected).
/// Use for potentially high-cardinality or privacy-sensitive data:
/// - Client IP address (PII concern)
/// - Route pattern (may be high cardinality)
/// - IP-related headers (X-Forwarded-For, etc.)
///
/// ## AppSecEvents { triggers: Vec<serde_json::Value> }
/// Special variant for the `_dd.appsec.json` tag containing security event details.
/// Accumulates all detected threats (SQL injection, XSS, etc.) as JSON objects.
///
/// ## Metric(f64)
/// Numeric metrics (not string tags).
/// Use for telemetry data:
/// - WAF execution duration
/// - Timeout count
/// - Sampling priority
///
/// # Cardinality Management
///
/// Always tags have ~100x lower cardinality than OnEvent tags:
/// - **Always**: ~10-50 unique values per tag (methods, status codes)
/// - **OnEvent**: ~1K-1M unique values per tag (IPs, routes)
///
/// By using OnEvent for high-cardinality data, we keep trace size manageable
/// when no threats are detected (the common case).
///
/// # Example
///
/// ```rust,ignore
/// // Low cardinality: always include
/// tags.insert(TagName::HttpMethod, TagValue::Always("GET".to_string()));
///
/// // High cardinality: only on threats
/// tags.insert(TagName::NetworkClientIp, TagValue::OnEvent(json!("192.168.1.1")));
///
/// // Numeric metric
/// tags.insert(TagName::AppsecWafDuration, TagValue::Metric(2500.0));
/// ```
#[derive(Debug, Clone, PartialEq)]
enum TagValue {
    /// A tag value that is always emitted.
    ///
    /// Included unconditionally in every span. Use for low-cardinality,
    /// always-relevant metadata.
    Always(String),

    /// A tag value that is only emitted when an event is matched.
    ///
    /// Included only when `has_events = true`. Use for high-cardinality
    /// or privacy-sensitive data that's only needed for threat investigation.
    OnEvent(serde_json::Value),

    /// The special key used to encode AppSec events.
    ///
    /// Contains an array of security event JSON objects (triggers).
    /// Each trigger describes a detected threat (rule ID, matched data, etc.).
    AppSecEvents { triggers: Vec<serde_json::Value> },

    /// A numeric metric value.
    ///
    /// Emitted as a span metric (not a string tag). Use for telemetry
    /// and performance monitoring.
    Metric(f64),
}
impl std::fmt::Display for TagValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Always(str) => write!(f, "{str}"),
            Self::OnEvent(value) => write!(f, "{value}"),
            Self::AppSecEvents { triggers } => write!(
                f,
                r#"{{ "triggers": {} }}"#,
                serde_json::Value::Array(triggers.clone())
            ),
            Self::Metric(value) => write!(f, "{value}"),
        }
    }
}

/// Extracts WAF address data from an invocation payload.
///
/// This function converts an HTTP transaction into the address data structure
/// expected by libddwaf. Addresses are named data points that the WAF rules
/// match against (e.g., "server.request.uri.raw", "server.request.body").
///
/// # Address Mapping
///
/// **Request Addresses**:
/// - `server.request.uri.raw` ← URI path
/// - `http.client_ip` ← Client IP address
/// - `server.request.headers.no_cookies` ← Request headers (multi-value)
/// - `server.request.cookies` ← Request cookies (multi-value)
/// - `server.request.query` ← Query parameters (multi-value)
/// - `server.request.path_params` ← Path parameters (single-value)
/// - `server.request.body` ← Parsed request body (JSON/form/multipart/plain)
///
/// **Response Addresses**:
/// - `server.response.status` ← HTTP status code
/// - `server.response.headers.no_cookies` ← Response headers (multi-value)
/// - `server.response.body` ← Parsed response body
///
/// # Returns
///
/// - `Some(WafMap)` - Address data ready for WAF analysis
/// - `None` - No address data available (nothing to analyze)
///
/// # Body Parsing
///
/// Request/response bodies are parsed based on Content-Type:
/// - `application/json` → JSON object/array
/// - `application/x-www-form-urlencoded` → Form data map
/// - `multipart/form-data` → Multipart map (recursive parsing)
/// - `text/plain` → Plain text string
/// - Other MIME types → Skipped (unsupported)
///
/// Bodies larger than 10MB are truncated to prevent memory exhaustion.
///
/// # Example
///
/// ```rust,ignore
/// let addresses = to_address_data(&http_transaction)?;
/// // addresses contains WAF-ready data structure
/// waf_context.run(Some(addresses), None, timeout)?;
/// ```
fn to_address_data(payload: &dyn InvocationPayload) -> Option<WafMap> {
    let mut addresses = Vec::<Keyed<WafObject>>::with_capacity(10);

    macro_rules! address {
        // Option<String> kind of fields...
        ($field:ident$(.$transform:ident())? => $name:literal) => {
            if let Some(value) = payload.$field() {
                addresses.push(($name, value$(.$transform())?).into());
            }
        };

        // Bodies
        ($field:ident mime from $headers:ident => $name:literal) => {
            if let Some(body) = payload.$field() {
                let $headers = payload.$headers();
                // Note -- headers are case-normalized to lower case by the JSON parser...
                let content_type = $headers.get("content-type").map(|mime|mime.last()).flatten().map_or("application/json", |mime|mime.as_str());
                match try_parse_body(body, content_type) {
                    Ok(value) => {
                        addresses.push(($name, value).into());
                    },
                    // Logging these as INFO as the user is often unable to do anything about these issues, and hence
                    // WARN is excessive.
                    Err(e) => info!("aap: unable to parse body, it will not be analyzed for security activity: {e}"),
                }
            }
        };

        // Single-valued HashMap fields...
        ($field:ident[] => $name:literal) => {
            let value = payload.$field();
            if !value.is_empty() {
                let mut waf_value = libddwaf::object::WafMap::new(value.len() as u64);
                for (idx, (key, item)) in value.iter().enumerate() {
                    waf_value[idx] = (key.as_str(), item.as_str()).into();
                }
                addresses.push(($name, waf_value).into());
            }
        };

        // Multi-valued HashMap fields...
        ($field:ident[][] => $name:literal) => {
            let value = payload.$field();
            if !value.is_empty() {
                let mut waf_value = libddwaf::object::WafMap::new(value.len() as u64);
                for (idx, (key, elements)) in value.iter().enumerate() {
                    let mut waf_elements = libddwaf::object::WafArray::new(elements.len() as u64);
                    for (idx, elt) in elements.iter().enumerate() {
                        waf_elements[idx] = elt.as_str().into();
                    }
                    waf_value[idx] = (key.as_str(), waf_elements).into();
                }
                addresses.push(($name, waf_value).into());
            }
        };
    }

    // Request addresses
    address!(raw_uri.as_str() => "server.request.uri.raw");
    address!(client_ip.as_str() => "http.client_ip");
    address!(request_headers_no_cookies[][] => "server.request.headers.no_cookies");
    address!(request_cookies[][] => "server.request.cookies");
    address!(query_params[][] => "server.request.query");
    address!(path_params[] => "server.request.path_params");
    address!(request_body mime from request_headers_no_cookies => "server.request.body");

    // Response addresses
    address!(response_status_code => "server.response.status");
    address!(response_headers_no_cookies[][] => "server.response.headers.no_cookies");
    address!(response_body mime from response_headers_no_cookies => "server.response.body");

    if addresses.is_empty() {
        return None;
    }

    let mut result = WafMap::new(addresses.len() as u64);
    for (idx, item) in addresses.into_iter().enumerate() {
        result[idx] = item;
    }

    Some(result)
}

/// Parses an HTTP body based on its Content-Type header.
///
/// This function dispatches to the appropriate parser based on MIME type,
/// ensuring that only the first 10MB of the body is read to prevent
/// memory exhaustion attacks.
///
/// # Arguments
///
/// * `body` - The body reader (request or response)
/// * `content_type` - The Content-Type header value
///
/// # Returns
///
/// * `Ok(WafObject)` - Successfully parsed body data
/// * `Err(BodyParseError)` - Parsing failed (invalid MIME, unsupported type, I/O error)
///
/// # Supported MIME Types
///
/// - `application/json`, `text/json` → JSON object/array
/// - `application/vnd.api+json` → JSON API format
/// - `application/x-www-form-urlencoded` → Form data
/// - `multipart/form-data` → Multipart form (recursive)
/// - `text/plain` → Plain text string
///
/// # Memory Safety
///
/// Bodies are limited to 10MB to prevent DoS attacks via large payloads.
fn try_parse_body(body: impl Read, content_type: &str) -> Result<WafObject, BodyParseError> {
    // Parse MIME type from Content-Type header
    // Decision: Invalid MIME types result in parse error (not ignored)
    let mime_type: Mime = content_type.parse()?;

    // Limit body size to prevent excessive memory allocation
    // Decision: 10MB limit balances security coverage with memory safety
    let limited_body = body.take(MAX_BODY_SIZE_BYTES);

    try_parse_body_with_mime(limited_body, mime_type)
}

/// Parses an HTTP body using the parsed MIME type.
///
/// This function contains the actual parsing logic for different MIME types.
/// It's separated from `try_parse_body` to allow recursive parsing of
/// multipart entries.
///
/// # MIME Type Handling
///
/// The function uses pattern matching on `(type, subtype, suffix)` to determine
/// the appropriate parser:
///
/// - `(text|application, json, None)` → JSON parser
/// - `(application, vnd.api, Some(json))` → JSON API parser
/// - `(application, www-form-urlencoded, None)` → Form parser
/// - `(multipart, form-data, None)` → Multipart parser (recursive)
/// - `(text, plain, None)` → Text reader
/// - Other combinations → UnsupportedMimeType error
///
/// # Multipart Recursion
///
/// Multipart entries are parsed recursively based on their Content-Type.
/// This allows handling complex nested structures like:
/// ```text
/// multipart/form-data
///   ├─ json: application/json → parsed as JSON
///   ├─ file: text/plain → parsed as text
///   └─ nested: application/x-www-form-urlencoded → parsed as form
/// ```
///
/// # Arguments
///
/// * `body` - The body reader (already size-limited)
/// * `mime_type` - The parsed MIME type
///
/// # Returns
///
/// * `Ok(WafObject)` - Successfully parsed body data
/// * `Err(BodyParseError)` - Parsing failed
fn try_parse_body_with_mime(body: impl Read, mime_type: Mime) -> Result<WafObject, BodyParseError> {
    match (mime_type.type_(), mime_type.subtype(), mime_type.suffix()) {
        (typ, sub, suff)
            if ((typ == mime::TEXT || typ == mime::APPLICATION)
                && sub == mime::JSON
                && suff.is_none())
                || (typ == mime::APPLICATION && sub == "vnd.api" && suff == Some(mime::JSON)) =>
        {
            Ok(serde_json::from_reader(body)?)
        }
        (mime::APPLICATION, mime::WWW_FORM_URLENCODED, None) => {
            let pairs: Vec<(String, String)> = serde_html_form::from_reader(body)?;
            let mut res = WafMap::new(pairs.len() as u64);
            for (i, (key, value)) in pairs.into_iter().enumerate() {
                res[i] = (key.as_str(), value.as_str()).into();
            }
            Ok(res.into())
        }
        (mime::MULTIPART, mime::FORM_DATA, None) => {
            let Some(boundary) = mime_type.get_param("boundary") else {
                return Err(BodyParseError::MissingBoundary(mime_type));
            };
            let mut multipart = Multipart::with_body(body, boundary.as_str());
            let mut items = Vec::new();
            loop {
                match multipart.read_entry() {
                    Ok(Some(mut entry)) => {
                        let mime = entry.headers.content_type.unwrap_or(mime::TEXT_PLAIN);
                        let mut data = Vec::new();
                        let _ = entry.data.read_to_end(&mut data)?;
                        let value = match try_parse_body_with_mime(Bytes::from(data).reader(), mime)
                        {
                            Ok(value) => value,
                            Err(e) => {
                                // Logging as INFO as this is often not directly actionnable by the
                                // customer and can lead to excessive log spam if sent as WARN.
                                info!(
                                    "aap: failed to parse multipart body entry {name}: {e}",
                                    name = entry.headers.name
                                );
                                WafObject::default()
                            }
                        };

                        items.push((entry.headers.name.to_string(), value));
                    }
                    Ok(None) => break,
                    Err(e) => {
                        warn!("aap: failed to read multipart body entry: {e}");
                        break;
                    }
                }
            }
            let mut res = WafMap::new(items.len() as u64);
            for (idx, (name, value)) in items.into_iter().enumerate() {
                res[idx] = (name.as_str(), value).into();
            }
            Ok(res.into())
        }
        (mime::TEXT, mime::PLAIN, None) => {
            let body = read_to_string(body)?;
            Ok(body.as_str().into())
        }
        _ => Err(BodyParseError::UnsupportedMimeType(mime_type)),
    }
}

#[derive(Debug, thiserror::Error)]
enum BodyParseError {
    #[error("failed to parse content type: {0}")]
    ContentTypeParseError(#[from] mime::FromStrError),
    #[error("cannot parse {0} body: missing boundary parameter")]
    MissingBoundary(Mime),
    #[error("unsupported MIME type: {0}")]
    UnsupportedMimeType(Mime),
    #[error("failed to read body: {0}")]
    IOError(#[from] std::io::Error),
    #[error("failed to parse body: {0}")]
    SerdeError(Box<dyn std::error::Error>),
}
impl From<serde_json::Error> for BodyParseError {
    fn from(e: serde_json::Error) -> Self {
        Self::SerdeError(Box::new(e))
    }
}
impl From<serde_html_form::de::Error> for BodyParseError {
    fn from(e: serde_html_form::de::Error) -> Self {
        Self::SerdeError(Box::new(e))
    }
}

/// Logs a WAF execution error.
///
/// This function is called when the WAF fails to execute (internal error,
/// not a timeout or match). These errors should be rare in practice.
///
/// # Cold Path Optimization
///
/// Marked `#[cold]` to inform the compiler this is an unlikely path, allowing
/// better optimization of the hot path (successful WAF execution).
///
/// # Arguments
///
/// * `e` - The WAF execution error
///
/// # Example
///
/// ```rust,ignore
/// match waf.run(addresses, None, timeout) {
///     Ok(result) => process_result(result),
///     Err(e) => log_waf_run_error(e), // Rare error path
/// }
/// ```
#[cold]
fn log_waf_run_error(e: RunError) {
    warn!("aap: failed to evaluate WAF ruleset: {e}");
}

/// Logs an "unreachable" code path, panicking in debug mode.
///
/// This function is used to mark logic errors that should never happen in
/// correct code. In debug builds, it panics immediately to fail fast. In
/// release builds, it logs a warning and continues execution to avoid
/// crashing production systems.
///
/// # Behavior
///
/// - **Debug mode**: Panics with the provided message (fail fast)
/// - **Release mode**: Logs warning and continues (graceful degradation)
///
/// # Cold Path Optimization
///
/// Marked `#[cold]` because these code paths should never be reached in
/// correct code, allowing better optimization of normal execution paths.
///
/// # Track Caller
///
/// Marked `#[track_caller]` to include the calling location in panic messages,
/// making it easier to identify where the logic error occurred.
///
/// # Arguments
///
/// * `msg` - Static error message describing the logic error
///
/// # Example
///
/// ```rust,ignore
/// if response_already_seen {
///     unreachable_warn("Context::run called after response finalized");
///     return;
/// }
/// ```
#[cold]
#[track_caller]
fn unreachable_warn(msg: &'static str) {
    // Decision: Panic in debug to catch logic errors during development
    if cfg!(debug_assertions) {
        unreachable!("{msg}");
    } else {
        // Decision: Warn in release to avoid crashing production systems
        warn!("{msg}");
    }
}

#[cfg_attr(coverage_nightly, coverage(off))] // Test modules skew coverage metrics
#[cfg(test)]
mod tests {
    use std::io;

    use libddwaf::waf_map;

    use super::*;

    #[test]
    fn test_try_parse_body() {
        let body = r#"--BOUNDARY
Content-Disposition: form-data; name="json"
Content-Type: application/json

{"foo": "bar"}
--BOUNDARY
Content-Disposition: form-data; name="text"

Hello, world!
--BOUNDARY
Content-Disposition: form-data; name="vnd.api+json"
Content-Type: application/vnd.api+json; charset=utf-8

{"baz": "qux"}

--BOUNDARY
Content-Disposition: form-data; name="invalid"
Content-Type: text/json

{invalid json}
--BOUNDARY
Content-Disposition: form-data; name="urlencoded"
Content-Type: application/x-www-form-urlencoded

key=value&space=%20
--BOUNDARY--"#
            .replace('\n', "\r\n"); // LF -> CRLF
        let body = Bytes::from(body);
        let parsed = try_parse_body(body.reader(), "multipart/form-data; boundary=BOUNDARY")
            .expect("should have parsed successfully");
        assert_eq!(
            waf_map! {
                ("json", waf_map!{ ("foo", "bar") }),
                ("text", "Hello, world!"),
                ("vnd.api+json", waf_map!{ ("baz", "qux") }),
                ("invalid", WafObject::default()),
                ("urlencoded", waf_map!{ ("key", "value"), ("space", " ") }),
            },
            parsed,
        );
    }

    #[test]
    fn test_try_parse_body_missing_boundary() {
        try_parse_body(io::empty(), "multipart/form-data").expect_err("should have failed");
    }
}
