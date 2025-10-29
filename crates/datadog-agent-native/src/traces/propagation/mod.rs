//! Distributed trace context propagation for connecting traces across service boundaries.
//!
//! This module implements the propagation layer for distributed tracing, enabling trace context
//! to be extracted from incoming requests and injected into outgoing requests. This allows
//! traces to span multiple services while maintaining parent-child relationships.
//!
//! # Propagation Styles
//!
//! Multiple propagation formats are supported to ensure interoperability:
//! - **Datadog**: Native Datadog headers (`x-datadog-trace-id`, `x-datadog-parent-id`, etc.)
//! - **W3C TraceContext**: Standard W3C propagation (`traceparent`, `tracestate`)
//! - **B3/B3Multi**: Zipkin-style propagation (planned)
//!
//! # Composite Propagation
//!
//! The `DatadogCompositePropagator` supports multiple propagation styles simultaneously:
//! - **Extraction**: Reads context from multiple header formats in configured order
//! - **Context Resolution**: Merges contexts when multiple formats are present
//! - **Span Links**: Creates span links for conflicting trace IDs across formats
//! - **Baggage**: Attaches custom key-value metadata to distributed traces
//!
//! # Trace Context Flow
//!
//! ```text
//! Incoming Request Headers
//!   ↓
//! Extract (read trace context from headers)
//!   ↓
//! SpanContext (trace ID, span ID, sampling, tags)
//!   ↓
//! Process Request (create child span)
//!   ↓
//! Inject (write trace context to headers)
//!   ↓
//! Outgoing Request Headers
//! ```
//!
//! # Example: Extracting Trace Context
//!
//! ```rust,ignore
//! use datadog_agent_native::traces::propagation::{DatadogCompositePropagator, Propagator};
//! use std::collections::HashMap;
//!
//! let config = Arc::new(Config::default());
//! let propagator = DatadogCompositePropagator::new(config);
//!
//! // Extract from incoming request headers
//! let headers = HashMap::from([
//!     ("x-datadog-trace-id".to_string(), "1234567890".to_string()),
//!     ("x-datadog-parent-id".to_string(), "5678".to_string()),
//! ]);
//!
//! let context = propagator.extract(&headers);
//! ```

use std::{collections::HashMap, sync::Arc};

use crate::{
    config::{self, trace_propagation_style::TracePropagationStyle},
    traces::context::SpanContext,
};
use carrier::{Extractor, Injector};
use datadog_trace_protobuf::pb::SpanLink;
use text_map_propagator::{
    BAGGAGE_PREFIX, DATADOG_HIGHER_ORDER_TRACE_ID_BITS_KEY, DATADOG_LAST_PARENT_ID_KEY,
    TRACESTATE_KEY,
};

pub mod carrier;
pub mod error;
pub mod text_map_propagator;

/// Trait for extracting and injecting distributed trace context.
///
/// Propagators provide a generic interface for reading trace context from incoming
/// requests and writing trace context to outgoing requests. Different propagators
/// support different header formats (Datadog, W3C TraceContext, B3).
///
/// # Thread Safety
///
/// Implementations must be thread-safe (`Send + Sync`) for use in async contexts.
pub trait Propagator {
    /// Extracts trace context from a carrier (e.g., HTTP headers).
    ///
    /// # Arguments
    ///
    /// * `carrier` - The carrier to extract trace context from (typically HTTP headers)
    ///
    /// # Returns
    ///
    /// `Some(SpanContext)` if valid trace context was found, `None` otherwise
    fn extract(&self, carrier: &dyn Extractor) -> Option<SpanContext>;

    /// Injects trace context into a carrier (e.g., HTTP headers).
    ///
    /// # Arguments
    ///
    /// * `context` - The span context to inject
    /// * `carrier` - The carrier to inject trace context into (typically HTTP headers)
    fn inject(&self, context: SpanContext, carrier: &mut dyn Injector);
}

/// Composite propagator supporting multiple trace context formats.
///
/// This propagator can extract and inject trace context using multiple header formats
/// (Datadog, W3C TraceContext, B3) simultaneously. It handles complex scenarios like:
/// - Multiple propagation styles in a single request
/// - Conflicting trace IDs across different formats
/// - Context merging and span link creation
/// - Baggage propagation for custom metadata
///
/// # Configuration
///
/// The propagator is configured via `Config`:
/// - `trace_propagation_style_extract`: List of formats to extract (order matters)
/// - `trace_propagation_extract_first`: If true, use first successful extraction
/// - `trace_propagation_http_baggage_enabled`: Enable baggage propagation
///
/// # Context Resolution
///
/// When multiple formats provide trace context:
/// 1. The first format in the configured list becomes the "primary" context
/// 2. Additional contexts with matching trace IDs are merged (tracestate, parent ID)
/// 3. Contexts with conflicting trace IDs become span links (for visualization)
///
/// # Example
///
/// ```rust,ignore
/// use datadog_agent_native::traces::propagation::DatadogCompositePropagator;
/// use std::sync::Arc;
///
/// let config = Arc::new(Config {
///     trace_propagation_style_extract: vec![
///         TracePropagationStyle::Datadog,
///         TracePropagationStyle::TraceContext,
///     ],
///     ..Default::default()
/// });
///
/// let propagator = DatadogCompositePropagator::new(config);
/// ```
pub struct DatadogCompositePropagator {
    /// List of configured propagators in extraction order.
    ///
    /// Each propagator handles a specific header format (Datadog, TraceContext, B3).
    /// Order matters: the first propagator to successfully extract becomes the primary context.
    propagators: Vec<Box<dyn Propagator + Send + Sync>>,
    /// Agent configuration controlling propagation behavior.
    config: Arc<config::Config>,
}

impl Propagator for DatadogCompositePropagator {
    /// Extracts trace context from incoming request headers.
    ///
    /// This method supports two extraction modes:
    ///
    /// # Extract First Mode
    ///
    /// If `trace_propagation_extract_first` is enabled:
    /// - Try each propagator in order
    /// - Return the first successful extraction
    /// - Attach baggage if configured
    ///
    /// # Extract All Mode (default)
    ///
    /// If `trace_propagation_extract_first` is disabled:
    /// - Extract contexts from all available formats
    /// - Resolve multiple contexts (merge matching, create span links for conflicts)
    /// - Attach baggage if configured
    ///
    /// # Arguments
    ///
    /// * `carrier` - The carrier containing trace headers (typically HTTP headers)
    ///
    /// # Returns
    ///
    /// `Some(SpanContext)` if any trace context was found, `None` otherwise
    fn extract(&self, carrier: &dyn Extractor) -> Option<SpanContext> {
        // Extract first mode: return first successful extraction
        if self.config.trace_propagation_extract_first {
            for propagator in &self.propagators {
                let context = propagator.extract(carrier);

                if let Some(mut context) = context {
                    // Attach baggage metadata if enabled
                    if self.config.trace_propagation_http_baggage_enabled {
                        Self::attach_baggage(&mut context, carrier);
                    }
                    return Some(context);
                }
            }
        }

        // Extract all mode: get contexts from all available formats
        let (contexts, styles) = self.extract_available_contexts(carrier);
        if contexts.is_empty() {
            return None;
        }

        // Resolve multiple contexts into a single primary context with span links
        let mut context = Self::resolve_contexts(contexts, styles, carrier);
        if self.config.trace_propagation_http_baggage_enabled {
            Self::attach_baggage(&mut context, carrier);
        }

        Some(context)
    }

    /// Injects trace context into outgoing request headers.
    ///
    /// **Note**: Injection is not yet implemented (marked as `todo!()`).
    ///
    /// # Arguments
    ///
    /// * `context` - The span context to inject
    /// * `carrier` - The carrier to inject headers into (typically HTTP headers)
    fn inject(&self, _context: SpanContext, _carrier: &mut dyn Injector) {
        todo!()
    }
}

impl DatadogCompositePropagator {
    /// Creates a new composite propagator from configuration.
    ///
    /// Initializes propagators based on the configured extraction styles. Only propagators
    /// for supported styles (Datadog, TraceContext) are created; unsupported styles are
    /// filtered out.
    ///
    /// # Arguments
    ///
    /// * `config` - Agent configuration specifying which propagation styles to support
    ///
    /// # Returns
    ///
    /// A new `DatadogCompositePropagator` with configured propagators
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let config = Arc::new(Config {
    ///     trace_propagation_style_extract: vec![
    ///         TracePropagationStyle::Datadog,
    ///         TracePropagationStyle::TraceContext,
    ///     ],
    ///     ..Default::default()
    /// });
    ///
    /// let propagator = DatadogCompositePropagator::new(config);
    /// ```
    #[must_use]
    pub fn new(config: Arc<config::Config>) -> Self {
        // Create propagators for each configured extraction style
        let propagators: Vec<Box<dyn Propagator + Send + Sync>> = config
            .trace_propagation_style_extract
            .iter()
            .filter_map(|style| match style {
                TracePropagationStyle::Datadog => {
                    Some(Box::new(text_map_propagator::DatadogHeaderPropagator)
                        as Box<dyn Propagator + Send + Sync>)
                }
                TracePropagationStyle::TraceContext => {
                    Some(Box::new(text_map_propagator::TraceContextPropagator)
                        as Box<dyn Propagator + Send + Sync>)
                }
                // Unsupported styles (B3, B3Multi, None) are filtered out
                _ => None,
            })
            .collect();

        Self {
            propagators,
            config,
        }
    }

    /// Extracts all available trace contexts from a carrier.
    ///
    /// Attempts extraction with each configured propagator, collecting all successful
    /// extractions along with their corresponding propagation styles.
    ///
    /// # Arguments
    ///
    /// * `carrier` - The carrier containing trace headers
    ///
    /// # Returns
    ///
    /// A tuple of:
    /// - `Vec<SpanContext>`: All successfully extracted contexts
    /// - `Vec<TracePropagationStyle>`: Corresponding propagation styles for each context
    fn extract_available_contexts(
        &self,
        carrier: &dyn Extractor,
    ) -> (Vec<SpanContext>, Vec<TracePropagationStyle>) {
        let mut contexts = Vec::<SpanContext>::new();
        let mut styles = Vec::<TracePropagationStyle>::new();

        // Try each propagator and collect successful extractions
        for (i, propagator) in self.propagators.iter().enumerate() {
            if let Some(context) = propagator.extract(carrier) {
                contexts.push(context);
                styles.push(self.config.trace_propagation_style_extract[i]);
            }
        }

        (contexts, styles)
    }

    /// Resolves multiple trace contexts into a single primary context with span links.
    ///
    /// When multiple propagation formats provide trace context, this method determines
    /// which context is primary and how to handle the others:
    ///
    /// # Resolution Rules
    ///
    /// 1. **Primary Context**: The first context (from the first successful extractor)
    /// 2. **Matching Trace IDs**: Contexts with the same trace ID are merged
    ///    - TraceContext's tracestate is added to primary context tags
    ///    - Parent ID is updated if span IDs differ
    /// 3. **Conflicting Trace IDs**: Contexts with different trace IDs become span links
    ///    - Creates a terminated_context span link for visualization
    ///    - Preserves sampling decision and tracestate
    ///
    /// # Arguments
    ///
    /// * `contexts` - List of extracted contexts (must not be empty)
    /// * `styles` - Corresponding propagation styles for each context
    /// * `_carrier` - The carrier (unused, reserved for future use)
    ///
    /// # Returns
    ///
    /// The resolved primary `SpanContext` with span links for conflicting contexts
    fn resolve_contexts(
        contexts: Vec<SpanContext>,
        styles: Vec<TracePropagationStyle>,
        _carrier: &dyn Extractor,
    ) -> SpanContext {
        // First context becomes the primary context
        let mut primary_context = contexts[0].clone();
        let mut links = Vec::<SpanLink>::new();

        let mut i = 1;
        for context in contexts.iter().skip(1) {
            let style = styles[i];

            // Check if this context has a different trace ID (conflicting context)
            if context.span_id != 0
                && context.trace_id != 0
                && context.trace_id != primary_context.trace_id
            {
                // This context represents a terminated/conflicting trace
                // Create a span link to visualize the connection in the UI
                let sampling = context.sampling.unwrap_or_default().priority.unwrap_or(0);

                // Extract tracestate if this is a TraceContext format
                let tracestate: Option<String> = match style {
                    TracePropagationStyle::TraceContext => {
                        context.tags.get(TRACESTATE_KEY).cloned()
                    }
                    _ => None,
                };

                // Add metadata explaining why this span link exists
                let attributes = HashMap::from([
                    ("reason".to_string(), "terminated_context".to_string()),
                    ("context_headers".to_string(), style.to_string()),
                ]);

                // Extract high-order trace ID bits for 128-bit trace IDs
                let trace_id_high_str = context
                    .tags
                    .get(DATADOG_HIGHER_ORDER_TRACE_ID_BITS_KEY)
                    .cloned()
                    .unwrap_or_default();
                let trace_ig_high = u64::from_str_radix(&trace_id_high_str, 16).unwrap_or_default();

                // Create span link pointing to the conflicting context
                links.push(SpanLink {
                    trace_id: context.trace_id,
                    trace_id_high: trace_ig_high,
                    span_id: context.span_id,
                    flags: u32::from(sampling > 0), // Set sampled flag based on priority
                    tracestate: tracestate.unwrap_or_default(),
                    attributes,
                });
            } else if style == TracePropagationStyle::TraceContext {
                // TraceContext style with matching trace ID - merge metadata

                // Preserve tracestate from TraceContext headers
                if let Some(tracestate) = context.tags.get(TRACESTATE_KEY) {
                    primary_context
                        .tags
                        .insert(TRACESTATE_KEY.to_string(), tracestate.clone());
                }

                // If trace IDs match but span IDs differ, update parent ID tracking
                if primary_context.trace_id == context.trace_id
                    && primary_context.span_id != context.span_id
                {
                    // Try to find Datadog context to get the correct parent ID
                    let mut dd_context: Option<SpanContext> = None;
                    if styles.contains(&TracePropagationStyle::Datadog) {
                        let position = styles
                            .iter()
                            .position(|&s| s == TracePropagationStyle::Datadog)
                            .unwrap_or_default();
                        dd_context = contexts.get(position).cloned();
                    }

                    // Set last parent ID from TraceContext or Datadog context
                    if let Some(parent_id) = context.tags.get(DATADOG_LAST_PARENT_ID_KEY) {
                        // Use parent ID from TraceContext if available
                        primary_context
                            .tags
                            .insert(DATADOG_LAST_PARENT_ID_KEY.to_string(), parent_id.clone());
                    } else if let Some(sc) = dd_context {
                        // Fall back to Datadog context's span ID as parent
                        primary_context.tags.insert(
                            DATADOG_LAST_PARENT_ID_KEY.to_string(),
                            format!("{:016x}", sc.span_id),
                        );
                    }

                    // Use TraceContext's span ID as the current span ID
                    primary_context.span_id = context.span_id;
                }
            }

            i += 1;
        }

        // Attach all span links to the primary context
        primary_context.links = links;

        primary_context
    }

    /// Attaches baggage headers to the span context as tags.
    ///
    /// Baggage allows custom metadata to be propagated with distributed traces.
    /// Headers with the baggage prefix (`ot-baggage-`) are extracted and added
    /// to the context's tags without the prefix.
    ///
    /// # Baggage Format
    ///
    /// Baggage headers follow the OpenTracing baggage convention:
    /// - Header name: `ot-baggage-{key}`
    /// - Tag name: `{key}` (prefix removed)
    /// - Tag value: Header value
    ///
    /// # Arguments
    ///
    /// * `context` - The span context to attach baggage to
    /// * `carrier` - The carrier containing baggage headers
    ///
    /// # Example
    ///
    /// Given headers:
    /// - `ot-baggage-user-id`: `12345`
    /// - `ot-baggage-session`: `abc-def`
    ///
    /// Results in context tags:
    /// - `user-id`: `12345`
    /// - `session`: `abc-def`
    fn attach_baggage(context: &mut SpanContext, carrier: &dyn Extractor) {
        let keys = carrier.keys();

        // Extract all baggage headers and add them as context tags
        for key in keys {
            // Check if this is a baggage header (starts with "ot-baggage-")
            if let Some(stripped) = key.strip_prefix(BAGGAGE_PREFIX) {
                // Add to context tags with the prefix removed
                context.tags.insert(
                    stripped.to_string(),
                    carrier.get(key).unwrap_or_default().to_string(),
                );
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::vec;

    use lazy_static::lazy_static;

    use crate::traces::context::Sampling;

    use super::*;

    fn lower_64_bits(value: u128) -> u64 {
        (value & 0xFFFF_FFFF_FFFF_FFFF) as u64
    }

    lazy_static! {
        static ref TRACE_ID: u128 = 171_395_628_812_617_415_352_188_477_958_425_669_623;
        static ref TRACE_ID_LOWER_ORDER_BITS: u64 = lower_64_bits(*TRACE_ID);
        static ref TRACE_ID_HEX: String = String::from("80f198ee56343ba864fe8b2a57d3eff7");

        // TraceContext Headers
        static ref VALID_TRACECONTEXT_HEADERS_BASIC: HashMap<String, String> = HashMap::from([
            (
                "traceparent".to_string(),
                format!("00-{}-00f067aa0ba902b7-01", *TRACE_ID_HEX)
            ),
            (
                "tracestate".to_string(),
                "dd=p:00f067aa0ba902b7;s:2;o:rum".to_string()
            ),
        ]);
        static ref VALID_TRACECONTEXT_HEADERS_RUM_NO_SAMPLING_DECISION: HashMap<String, String> =
            HashMap::from([
                (
                    "traceparent".to_string(),
                    format!("00-{}-00f067aa0ba902b7-00", *TRACE_ID_HEX)
                ),
                (
                    "tracestate".to_string(),
                    "dd=o:rum".to_string()
                ),
            ]);
        static ref VALID_TRACECONTEXT_HEADERS: HashMap<String, String> = HashMap::from([
            (
                "traceparent".to_string(),
                format!("00-{}-00f067aa0ba902b7-01", *TRACE_ID_HEX)
            ),
            (
                "tracestate".to_string(),
                "dd=s:2;o:rum;t.dm:-4;t.usr.id:baz64,congo=t61rcWkgMz".to_string()
            ),
        ]);
        static ref VALID_TRACECONTEXT_HEADERS_VALID_64_BIT_TRACE_ID: HashMap<String, String> =
            HashMap::from([
                (
                    "traceparent".to_string(),
                    "00-000000000000000064fe8b2a57d3eff7-00f067aa0ba902b7-01".to_string()
                ),
                (
                    "tracestate".to_string(),
                    "dd=s:2;o:rum;t.dm:-4;t.usr.id:baz64,congo=t61rcWkgMzE".to_string()
                ),
            ]);

        // Datadog Headers
        static ref VALID_DATADOG_HEADERS: HashMap<String, String> = HashMap::from([
            (
                "x-datadog-trace-id".to_string(),
                "13088165645273925489".to_string(),
            ),
            ("x-datadog-parent-id".to_string(), "5678".to_string(),),
            ("x-datadog-sampling-priority".to_string(), "1".to_string()),
            ("x-datadog-origin".to_string(), "synthetics".to_string()),
        ]);
        static ref VALID_DATADOG_HEADERS_NO_PRIORITY: HashMap<String, String> = HashMap::from([
            (
                "x-datadog-trace-id".to_string(),
                "13088165645273925489".to_string(),
            ),
            ("x-datadog-parent-id".to_string(), "5678".to_string(),),
            ("x-datadog-origin".to_string(), "synthetics".to_string()),
        ]);
        static ref VALID_DATADOG_HEADERS_MATCHING_TRACE_CONTEXT_VALID_TRACE_ID: HashMap<String, String> =
            HashMap::from([
                (
                    "x-datadog-trace-id".to_string(),
                    TRACE_ID_LOWER_ORDER_BITS.to_string()
                ),
                ("x-datadog-parent-id".to_string(), "5678".to_string()),
                ("x-datadog-origin".to_string(), "synthetics".to_string()),
                ("x-datadog-sampling-priority".to_string(), "1".to_string()),
            ]);
        static ref INVALID_DATADOG_HEADERS: HashMap<String, String> = HashMap::from([
            (
                "x-datadog-trace-id".to_string(),
                "13088165645273925489".to_string(),
            ),
            ("x-datadog-parent-id".to_string(), "parent_id".to_string(),),
            ("x-datadog-sampling-priority".to_string(), "sample".to_string()),
        ]);

        // Fixtures
        //
        static ref ALL_VALID_HEADERS: HashMap<String, String> = {
            let mut h = HashMap::new();
            h.extend(VALID_DATADOG_HEADERS.clone());
            h.extend(VALID_TRACECONTEXT_HEADERS.clone());
            // todo: add b3
            h
        };
        static ref DATADOG_TRACECONTEXT_MATCHING_TRACE_ID_HEADERS: HashMap<String, String> = {
            let mut h = HashMap::new();
            h.extend(VALID_DATADOG_HEADERS_MATCHING_TRACE_CONTEXT_VALID_TRACE_ID.clone());
            // We use 64-bit traceparent trace id value here so it can match for
            // both 128-bit enabled and disabled
            h.extend(VALID_TRACECONTEXT_HEADERS_VALID_64_BIT_TRACE_ID.clone());
            h
        };
        // Edge cases
        static ref ALL_HEADERS_CHAOTIC_1: HashMap<String, String> = {
            let mut h = HashMap::new();
            h.extend(VALID_DATADOG_HEADERS_MATCHING_TRACE_CONTEXT_VALID_TRACE_ID.clone());
            h.extend(VALID_TRACECONTEXT_HEADERS_VALID_64_BIT_TRACE_ID.clone());
            // todo: add b3
            h
        };
        static ref ALL_HEADERS_CHAOTIC_2: HashMap<String, String> = {
            let mut h = HashMap::new();
            h.extend(VALID_DATADOG_HEADERS.clone());
            h.extend(VALID_TRACECONTEXT_HEADERS_VALID_64_BIT_TRACE_ID.clone());
            // todo: add b3
            h
        };
        static ref NO_TRACESTATE_SUPPORT_NOT_MATCHING_TRACE_ID: HashMap<String, String> = {
            let mut h = HashMap::new();
            h.extend(VALID_DATADOG_HEADERS.clone());
            h.extend(VALID_TRACECONTEXT_HEADERS_RUM_NO_SAMPLING_DECISION.clone());
            h
        };
    }

    macro_rules! test_propagation_extract {
        ($($name:ident: $value:expr,)*) => {
            $(
                #[test]
                fn $name() {
                    let (styles, carrier, expected) = $value;
                    let mut config = config::Config::default();
                    config.trace_propagation_style_extract = vec![TracePropagationStyle::Datadog, TracePropagationStyle::TraceContext];
                    if let Some(s) = styles {
                        config.trace_propagation_style_extract.clone_from(&s);
                    }

                    let propagator = DatadogCompositePropagator::new(Arc::new(config));

                    let context = propagator.extract(&carrier).unwrap_or_default();

                    assert_eq!(context, expected);
                }
            )*
        }
    }

    test_propagation_extract! {
        // Datadog Headers
        valid_datadog_default: (
            None,
            VALID_DATADOG_HEADERS.clone(),
            SpanContext {
                trace_id: 13_088_165_645_273_925_489,
                span_id: 5678,
                sampling: Some(Sampling {
                    priority: Some(1),
                    mechanism: None,
                }),
                origin: Some("synthetics".to_string()),
                tags: HashMap::from([
                    ("_dd.p.dm".to_string(), "-3".to_string())
                ]),
                links: vec![],
            }
        ),
        valid_datadog_no_priority: (
            None,
            VALID_DATADOG_HEADERS_NO_PRIORITY.clone(),
            SpanContext {
                trace_id: 13_088_165_645_273_925_489,
                span_id: 5678,
                sampling: Some(Sampling {
                    priority: Some(2),
                    mechanism: None,
                }),
                origin: Some("synthetics".to_string()),
                tags: HashMap::from([
                    ("_dd.p.dm".to_string(), "-3".to_string())
                ]),
                links: vec![],
            },
        ),
        invalid_datadog: (
            Some(vec![TracePropagationStyle::Datadog]),
            INVALID_DATADOG_HEADERS.clone(),
            SpanContext::default(),
        ),
        valid_datadog_explicit_style: (
            Some(vec![TracePropagationStyle::Datadog]),
            VALID_DATADOG_HEADERS.clone(),
            SpanContext {
                trace_id: 13_088_165_645_273_925_489,
                span_id: 5678,
                sampling: Some(Sampling {
                    priority: Some(1),
                    mechanism: None,
                }),
                origin: Some("synthetics".to_string()),
                tags: HashMap::from([
                    ("_dd.p.dm".to_string(), "-3".to_string())
                ]),
                links: vec![],
            },
        ),
        invalid_datadog_negative_trace_id: (
            Some(vec![TracePropagationStyle::Datadog]),
            HashMap::from([
                (
                    "x-datadog-trace-id".to_string(),
                    "-1".to_string(),
                ),
                ("x-datadog-parent-id".to_string(), "5678".to_string(),),
                ("x-datadog-sampling-priority".to_string(), "1".to_string()),
                ("x-datadog-origin".to_string(), "synthetics".to_string()),
            ]),
            SpanContext::default(),
        ),
        valid_datadog_no_datadog_style: (
            Some(vec![TracePropagationStyle::TraceContext]),
            VALID_DATADOG_HEADERS.clone(),
            SpanContext::default(),
        ),
        // TraceContext Headers
        valid_tracecontext_simple: (
            Some(vec![TracePropagationStyle::TraceContext]),
            VALID_TRACECONTEXT_HEADERS_BASIC.clone(),
            SpanContext {
                trace_id: 7_277_407_061_855_694_839,
                span_id: 67_667_974_448_284_343,
                sampling: Some(Sampling {
                    priority: Some(2),
                    mechanism: None,
                }),
                origin: Some("rum".to_string()),
                tags: HashMap::from([
                    ("tracestate".to_string(), "dd=p:00f067aa0ba902b7;s:2;o:rum".to_string()),
                    ("_dd.p.tid".to_string(), "9291375655657946024".to_string()),
                    ("traceparent".to_string(), "00-80f198ee56343ba864fe8b2a57d3eff7-00f067aa0ba902b7-01".to_string()),
                    ("_dd.parent_id".to_string(), "00f067aa0ba902b7".to_string()),
                ]),
                links: vec![],
            }
        ),
        valid_tracecontext_rum_no_sampling_decision: (
            Some(vec![TracePropagationStyle::TraceContext]),
            VALID_TRACECONTEXT_HEADERS_RUM_NO_SAMPLING_DECISION.clone(),
            SpanContext {
                trace_id: 7_277_407_061_855_694_839,
                span_id: 67_667_974_448_284_343,
                sampling: Some(Sampling {
                    priority: Some(0),
                    mechanism: None,
                }),
                origin: Some("rum".to_string()),
                tags: HashMap::from([
                    ("_dd.p.tid".to_string(), "9291375655657946024".to_string()),
                    ("tracestate".to_string(), "dd=o:rum".to_string()),
                    ("traceparent".to_string(), "00-80f198ee56343ba864fe8b2a57d3eff7-00f067aa0ba902b7-00".to_string()),
                ]),
                links: vec![],
            }
        ),
        // B3 Headers
        // todo: all of them
        // B3 single Headers
        // todo: all of them
        // All Headers
        valid_all_headers: (
            None,
            ALL_VALID_HEADERS.clone(),
            SpanContext {
                trace_id: 13_088_165_645_273_925_489,
                span_id: 5678,
                sampling: Some(Sampling {
                    priority: Some(1),
                    mechanism: None,
                }),
                origin: Some("synthetics".to_string()),
                tags: HashMap::from([
                    ("_dd.p.dm".to_string(), "-3".to_string())
                ]),
                links: vec![
                    SpanLink {
                        trace_id: 7_277_407_061_855_694_839,
                        trace_id_high: 0,
                        span_id: 67_667_974_448_284_343,
                        flags: 1,
                        tracestate: "dd=s:2;o:rum;t.dm:-4;t.usr.id:baz64,congo=t61rcWkgMz".to_string(),
                        attributes: HashMap::from([
                            ("reason".to_string(), "terminated_context".to_string()),
                            ("context_headers".to_string(), "tracecontext".to_string()),
                        ]),
                    }
                ],
            },
        ),
        valid_all_headers_all_styles: (
            Some(vec![TracePropagationStyle::Datadog, TracePropagationStyle::TraceContext]),
            ALL_VALID_HEADERS.clone(),
            SpanContext {
                trace_id: 13_088_165_645_273_925_489,
                span_id: 5678,
                sampling: Some(Sampling {
                    priority: Some(1),
                    mechanism: None,
                }),
                origin: Some("synthetics".to_string()),
                tags: HashMap::from([
                    ("_dd.p.dm".to_string(), "-3".to_string())
                ]),
                links: vec![
                    SpanLink {
                        trace_id: 7_277_407_061_855_694_839,
                        trace_id_high: 0,
                        span_id: 67_667_974_448_284_343,
                        flags: 1,
                        tracestate: "dd=s:2;o:rum;t.dm:-4;t.usr.id:baz64,congo=t61rcWkgMz".to_string(),
                        attributes: HashMap::from([
                            ("reason".to_string(), "terminated_context".to_string()),
                            ("context_headers".to_string(), "tracecontext".to_string()),
                        ]),
                    }
                    // todo: b3 span links
                ],
            },
        ),
        valid_all_headers_datadog_style: (
            Some(vec![TracePropagationStyle::Datadog]),
            ALL_VALID_HEADERS.clone(),
            SpanContext {
                trace_id: 13_088_165_645_273_925_489,
                span_id: 5678,
                sampling: Some(Sampling {
                    priority: Some(1),
                    mechanism: None,
                }),
                origin: Some("synthetics".to_string()),
                tags: HashMap::from([
                    ("_dd.p.dm".to_string(), "-3".to_string())
                ]),
                links: vec![]
            },
        ),
        // todo: valid_all_headers_b3_style
        // todo: valid_all_headers_both_b3_styles
        // todo: valid_all_headers_b3_single_style
        none_style: (
            Some(vec![TracePropagationStyle::None]),
            ALL_VALID_HEADERS.clone(),
            SpanContext::default(),
        ),
        valid_style_and_none_still_extracts: (
            Some(vec![TracePropagationStyle::Datadog, TracePropagationStyle::None]),
            ALL_VALID_HEADERS.clone(),
            SpanContext {
                trace_id: 13_088_165_645_273_925_489,
                span_id: 5678,
                sampling: Some(Sampling {
                    priority: Some(1),
                    mechanism: None,
                }),
                origin: Some("synthetics".to_string()),
                tags: HashMap::from([
                    ("_dd.p.dm".to_string(), "-3".to_string())
                ]),
                links: vec![],
            }
        ),
        // Order matters
        // todo: order_matters_b3_single_header_first
        // todo: order_matters_b3_first
        // todo: order_matters_b3_second_no_datadog_headers
        // Tracestate is still added when TraceContext style comes later and matches
        // first style's `trace_id`
        additional_tracestate_support_when_present_and_matches_first_style_trace_id: (
            Some(vec![TracePropagationStyle::Datadog, TracePropagationStyle::TraceContext]),
            DATADOG_TRACECONTEXT_MATCHING_TRACE_ID_HEADERS.clone(),
            SpanContext {
                trace_id: 7_277_407_061_855_694_839,
                span_id: 67_667_974_448_284_343,
                sampling: Some(Sampling {
                    priority: Some(1),
                    mechanism: None,
                }),
                origin: Some("synthetics".to_string()),
                tags: HashMap::from([
                    ("_dd.p.dm".to_string(), "-3".to_string()),
                    ("_dd.parent_id".to_string(), "000000000000162e".to_string()),
                    (TRACESTATE_KEY.to_string(), "dd=s:2;o:rum;t.dm:-4;t.usr.id:baz64,congo=t61rcWkgMzE".to_string())
                ]),
                links: vec![],
            }
        ),
        // Tracestate is not added when TraceContext style comes later and does not
        // match first style's `trace_id`
        no_additional_tracestate_support_when_present_and_trace_id_does_not_match: (
            Some(vec![TracePropagationStyle::Datadog, TracePropagationStyle::TraceContext]),
            NO_TRACESTATE_SUPPORT_NOT_MATCHING_TRACE_ID.clone(),
            SpanContext {
                trace_id: 13_088_165_645_273_925_489,
                span_id: 5678,
                sampling: Some(Sampling {
                    priority: Some(1),
                    mechanism: None,
                }),
                origin: Some("synthetics".to_string()),
                tags: HashMap::from([
                    ("_dd.p.dm".to_string(), "-3".to_string())
                ]),
                links: vec![
                    SpanLink {
                        trace_id: 7_277_407_061_855_694_839,
                        trace_id_high: 0,
                        span_id: 67_667_974_448_284_343,
                        flags: 0,
                        tracestate: "dd=o:rum".to_string(),
                        attributes: HashMap::from([
                            ("reason".to_string(), "terminated_context".to_string()),
                            ("context_headers".to_string(), "tracecontext".to_string()),
                        ]),
                    }
                ],
            }
        ),
        valid_all_headers_no_style: (
            Some(vec![]),
            ALL_VALID_HEADERS.clone(),
            SpanContext::default(),
        ),
        datadog_tracecontext_conflicting_span_ids: (
            Some(vec![TracePropagationStyle::Datadog, TracePropagationStyle::TraceContext]),
            HashMap::from([
                (
                    "x-datadog-trace-id".to_string(),
                    "9291375655657946024".to_string(),
                ),
                ("x-datadog-parent-id".to_string(), "15".to_string(),),
                ("traceparent".to_string(), "00-000000000000000080f198ee56343ba8-000000000000000a-01".to_string()),
            ]),
            SpanContext {
                trace_id: 9_291_375_655_657_946_024,
                span_id: 10,
                sampling: Some(Sampling {
                    priority: Some(2),
                    mechanism: None,
                }),
                origin: None,
                tags: HashMap::from([
                    ("_dd.parent_id".to_string(), "000000000000000f".to_string()),
                    ("_dd.p.dm".to_string(), "-3".to_string()),
                ]),
                links: vec![],
            }
        ),
        // todo: all_headers_all_styles_tracecontext_t_id_match_no_span_link
        all_headers_all_styles_do_not_create_span_link_for_context_w_out_span_id: (
            Some(vec![TracePropagationStyle::TraceContext, TracePropagationStyle::Datadog]),
            ALL_HEADERS_CHAOTIC_2.clone(),
            SpanContext {
                trace_id: 7_277_407_061_855_694_839,
                span_id: 67_667_974_448_284_343,
                sampling: Some(Sampling {
                    priority: Some(2),
                    mechanism: None,
                }),
                origin: Some("rum".to_string()),
                tags: HashMap::from([
                    ("_dd.p.dm".to_string(), "-4".to_string()),
                    ("_dd.p.tid".to_string(), "0".to_string()),
                    ("_dd.p.usr.id".to_string(), "baz64".to_string()),
                    ("traceparent".to_string(), "00-000000000000000064fe8b2a57d3eff7-00f067aa0ba902b7-01".to_string()),
                    ("tracestate".to_string(), "dd=s:2;o:rum;t.dm:-4;t.usr.id:baz64,congo=t61rcWkgMzE".to_string()),
                ]),
                links: vec![
                    SpanLink {
                        trace_id: 13_088_165_645_273_925_489,
                        trace_id_high: 0,
                        span_id: 5678,
                        flags: 1,
                        tracestate: String::new(),
                        attributes: HashMap::from([
                            ("reason".to_string(), "terminated_context".to_string()),
                            ("context_headers".to_string(), "datadog".to_string()),
                        ]),
                    }
                ],
            }
        ),
        all_headers_all_styles_tracecontext_primary_only_datadog_t_id_diff: (
            Some(vec![TracePropagationStyle::TraceContext, TracePropagationStyle::Datadog]),
            ALL_VALID_HEADERS.clone(),
            SpanContext {
                trace_id: 7_277_407_061_855_694_839,
                span_id: 67_667_974_448_284_343,
                sampling: Some(Sampling {
                    priority: Some(2),
                    mechanism: None,
                }),
                origin: Some("rum".to_string()),
                tags: HashMap::from([
                    ("_dd.p.dm".to_string(), "-4".to_string()),
                    ("_dd.p.tid".to_string(), "9291375655657946024".to_string()),
                    ("_dd.p.usr.id".to_string(), "baz64".to_string()),
                    ("traceparent".to_string(), "00-80f198ee56343ba864fe8b2a57d3eff7-00f067aa0ba902b7-01".to_string()),
                    ("tracestate".to_string(), "dd=s:2;o:rum;t.dm:-4;t.usr.id:baz64,congo=t61rcWkgMz".to_string()),
                ]),
                links: vec![
                    SpanLink {
                        trace_id: 13_088_165_645_273_925_489,
                        trace_id_high: 0,
                        span_id: 5678,
                        flags: 1,
                        tracestate: String::new(),
                        attributes: HashMap::from([
                            ("reason".to_string(), "terminated_context".to_string()),
                            ("context_headers".to_string(), "datadog".to_string()),
                        ]),
                    }
                ],
            }
        ),
        // todo: fix this test
        all_headers_all_styles_datadog_primary_only_datadog_t_id_diff: (
            Some(vec![TracePropagationStyle::Datadog, TracePropagationStyle::TraceContext]),
            ALL_VALID_HEADERS.clone(),
            SpanContext {
                trace_id: 13_088_165_645_273_925_489,
                span_id: 5678,
                sampling: Some(Sampling {
                    priority: Some(1),
                    mechanism: None,
                }),
                origin: Some("synthetics".to_string()),
                tags: HashMap::from([
                    ("_dd.p.dm".to_string(), "-3".to_string())
                ]),
                links: vec![
                    SpanLink {
                        trace_id: 7_277_407_061_855_694_839,
                        // this should be `9291375655657946024` not `0`, but we don't have this data
                        // with the current definition of `SpanContext`
                        trace_id_high: 0,
                        span_id: 67_667_974_448_284_343,
                        flags: 1,
                        tracestate: "dd=s:2;o:rum;t.dm:-4;t.usr.id:baz64,congo=t61rcWkgMz".to_string(),
                        attributes: HashMap::from([
                            ("reason".to_string(), "terminated_context".to_string()),
                            ("context_headers".to_string(), "tracecontext".to_string()),
                        ]),
                    }
                ],
            }
        ),
        // todo: datadog_primary_match_tracecontext_dif_from_b3_b3multi_invalid
    }

    #[test]
    fn test_new_filter_propagators() {
        let config = config::Config {
            trace_propagation_style_extract: vec![
                TracePropagationStyle::Datadog,
                TracePropagationStyle::TraceContext,
                TracePropagationStyle::B3,
                TracePropagationStyle::B3Multi,
            ],
            ..Default::default()
        };

        let propagator = DatadogCompositePropagator::new(Arc::new(config));

        assert_eq!(propagator.propagators.len(), 2);
    }

    #[test]
    fn test_new_no_propagators() {
        let config = config::Config {
            trace_propagation_style_extract: vec![TracePropagationStyle::None],
            ..Default::default()
        };
        let propagator = DatadogCompositePropagator::new(Arc::new(config));

        assert_eq!(propagator.propagators.len(), 0);
    }

    #[test]
    fn test_extract_available_contexts() {
        let config = config::Config {
            trace_propagation_style_extract: vec![
                TracePropagationStyle::Datadog,
                TracePropagationStyle::TraceContext,
            ],
            ..Default::default()
        };

        let propagator = DatadogCompositePropagator::new(Arc::new(config));

        let carrier = HashMap::from([
            (
                "traceparent".to_string(),
                "00-80f198ee56343ba864fe8b2a57d3eff7-00f067aa0ba902b7-01".to_string(),
            ),
            (
                "tracestate".to_string(),
                "dd=p:00f067aa0ba902b7;s:2;o:rum".to_string(),
            ),
            (
                "x-datadog-trace-id".to_string(),
                "7277407061855694839".to_string(),
            ),
            (
                "x-datadog-parent-id".to_string(),
                "67667974448284343".to_string(),
            ),
            ("x-datadog-sampling-priority".to_string(), "2".to_string()),
            ("x-datadog-origin".to_string(), "rum".to_string()),
            (
                "x-datadog-tags".to_string(),
                "_dd.p.test=value,_dd.p.tid=9291375655657946024,any=tag".to_string(),
            ),
        ]);
        let (contexts, styles) = propagator.extract_available_contexts(&carrier);

        assert_eq!(contexts.len(), 2);
        assert_eq!(styles.len(), 2);
    }

    #[test]
    fn test_extract_available_contexts_no_contexts() {
        let config = config::Config {
            trace_propagation_style_extract: vec![TracePropagationStyle::Datadog],
            ..Default::default()
        };

        let propagator = DatadogCompositePropagator::new(Arc::new(config));

        let carrier = HashMap::from([
            (
                "traceparent".to_string(),
                "00-80f198ee56343ba864fe8b2a57d3eff7-00f067aa0ba902b7-01".to_string(),
            ),
            (
                "tracestate".to_string(),
                "dd=p:00f067aa0ba902b7;s:2;o:rum".to_string(),
            ),
        ]);
        let (contexts, styles) = propagator.extract_available_contexts(&carrier);

        assert_eq!(contexts.len(), 0);
        assert_eq!(styles.len(), 0);
    }

    #[test]
    fn test_attach_baggage() {
        let mut context = SpanContext::default();
        let carrier = HashMap::from([
            ("x-datadog-trace-id".to_string(), "123".to_string()),
            ("x-datadog-parent-id".to_string(), "5678".to_string()),
            ("ot-baggage-key1".to_string(), "value1".to_string()),
        ]);

        DatadogCompositePropagator::attach_baggage(&mut context, &carrier);

        assert_eq!(context.tags.len(), 1);
        assert_eq!(context.tags.get("key1").expect("Missing tag"), "value1");
    }
}
