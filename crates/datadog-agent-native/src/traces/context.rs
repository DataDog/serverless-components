//! Distributed trace context structures for propagating trace information across service boundaries.
//!
//! This module defines the core context structures used in distributed tracing:
//! - **`SpanContext`**: Complete trace context including trace ID, span ID, sampling, and metadata
//! - **`Sampling`**: Sampling decision and mechanism for trace retention
//!
//! # Trace Context
//!
//! In distributed tracing, context must be propagated between services to connect spans
//! into a complete trace. The `SpanContext` structure contains all information needed to:
//! - Link child spans to parent spans
//! - Maintain trace identity across service boundaries
//! - Propagate sampling decisions
//! - Carry trace-level metadata (origin, tags)
//! - Support distributed tracing features (span links)
//!
//! # Sampling
//!
//! Sampling controls which traces are retained and sent to Datadog. The `Sampling` structure
//! contains:
//! - **Priority**: Sampling priority (-1 to drop, 0 for auto-reject, 1 for auto-keep, 2 for user-keep)
//! - **Mechanism**: How the sampling decision was made (e.g., agent, rule, manual)
//!
//! # Usage
//!
//! Context is typically extracted from incoming requests, modified during processing,
//! and injected into outgoing requests:
//!
//! ```text
//! Incoming Request
//!   ↓
//! Extract SpanContext (from headers)
//!   ↓
//! Process Request (create child span)
//!   ↓
//! Inject SpanContext (into outgoing request headers)
//!   ↓
//! Outgoing Request
//! ```

use std::collections::HashMap;

use datadog_trace_protobuf::pb::SpanLink;

/// Sampling decision and mechanism for trace retention.
///
/// Sampling controls whether a trace is kept and sent to Datadog or dropped.
/// The sampling decision is made at the root span and propagated to all child spans
/// in the trace.
///
/// # Priority Values
///
/// Sampling priority is an `i8` with standard values:
/// - **-1**: User reject - explicitly drop the trace
/// - **0**: Auto reject - sampled out by automatic sampling
/// - **1**: Auto keep - sampled in by automatic sampling (default)
/// - **2**: User keep - explicitly retain the trace (highest priority)
///
/// Traces with priority ≥ 1 are kept and sent to Datadog.
///
/// # Mechanism
///
/// The mechanism indicates how the sampling decision was made:
/// - **0**: Default/unknown
/// - **1**: Agent sampling
/// - **2**: Rule-based sampling
/// - **3**: Manual/user decision
/// - **4**: Remote configuration
///
/// # Example
///
/// ```
/// use datadog_agent_native::traces::context::Sampling;
///
/// // Auto-keep with default mechanism
/// let sampling = Sampling {
///     priority: Some(1),
///     mechanism: Some(1),
/// };
/// ```
#[derive(Copy, Clone, Default, Debug, PartialEq)]
pub struct Sampling {
    /// Sampling priority controlling trace retention.
    ///
    /// Standard values:
    /// - `-1`: User reject (drop)
    /// - `0`: Auto reject (sampled out)
    /// - `1`: Auto keep (sampled in, default)
    /// - `2`: User keep (explicit retention)
    pub priority: Option<i8>,
    /// Sampling mechanism indicating how the decision was made.
    ///
    /// Common values:
    /// - `0`: Default/unknown
    /// - `1`: Agent sampling
    /// - `2`: Rule-based sampling
    /// - `3`: Manual/user decision
    /// - `4`: Remote configuration
    pub mechanism: Option<u8>,
}

/// Complete distributed trace context for a span.
///
/// Contains all information needed to propagate trace context across service boundaries
/// and link spans together into a complete distributed trace.
///
/// # Core Identity
///
/// - **`trace_id`**: Unique identifier for the entire distributed trace (64-bit)
/// - **`span_id`**: Unique identifier for this specific span (64-bit)
///
/// All spans in the same distributed trace share the same `trace_id`. The `span_id`
/// is unique within the trace and identifies this specific operation.
///
/// # Sampling
///
/// The `sampling` field contains the sampling decision (keep/drop) and mechanism.
/// This is propagated from the root span to all child spans in the trace.
///
/// # Origin
///
/// The `origin` field identifies the source of the trace:
/// - `"synthetics"`: Datadog Synthetic Monitoring
/// - `"rum"`: Real User Monitoring
/// - `"lambda"`: AWS Lambda invocation
/// - `"appsec"`: Application Security event
///
/// # Tags
///
/// Trace-level tags propagated across all spans in the trace. Common tags include:
/// - `_dd.origin`: Trace origin
/// - `_dd.p.tid`: 128-bit trace ID extension
/// - Custom user tags
///
/// # Span Links
///
/// Links to other spans for representing relationships beyond parent-child:
/// - Async processing (producer-consumer)
/// - Fan-out/fan-in patterns
/// - Batch processing
///
/// # Example
///
/// ```
/// use datadog_agent_native::traces::context::{SpanContext, Sampling};
/// use std::collections::HashMap;
///
/// let context = SpanContext {
///     trace_id: 1234567890,
///     span_id: 9876543210,
///     sampling: Some(Sampling {
///         priority: Some(1),
///         mechanism: Some(1),
///     }),
///     origin: Some("synthetics".to_string()),
///     tags: HashMap::new(),
///     links: Vec::new(),
/// };
/// ```
#[derive(Clone, Default, Debug, PartialEq)]
#[allow(clippy::module_name_repetitions)]
pub struct SpanContext {
    /// Unique 64-bit trace identifier shared by all spans in the distributed trace.
    ///
    /// All spans belonging to the same distributed trace have the same `trace_id`.
    /// This allows the Datadog backend to group spans together into a complete trace view.
    pub trace_id: u64,
    /// Unique 64-bit span identifier for this specific span.
    ///
    /// Each span in a trace has a unique `span_id`. Parent-child relationships are
    /// established by referencing the parent's `span_id`.
    pub span_id: u64,
    /// Sampling decision and mechanism for this trace.
    ///
    /// Determines whether the trace is kept (sent to Datadog) or dropped.
    /// The sampling decision is made at the root span and propagated to all children.
    pub sampling: Option<Sampling>,
    /// Origin of the trace (e.g., "synthetics", "rum", "lambda", "appsec").
    ///
    /// Identifies the source system that initiated the trace. This is used for:
    /// - Filtering and grouping traces by origin
    /// - Applying origin-specific processing rules
    /// - Attribution and billing
    pub origin: Option<String>,
    /// Trace-level tags propagated across all spans.
    ///
    /// These tags are added to every span in the trace and used for:
    /// - Filtering and grouping traces in the Datadog UI
    /// - Propagating context-specific information
    /// - Custom business logic tags
    ///
    /// Common system tags:
    /// - `_dd.origin`: Trace origin
    /// - `_dd.p.tid`: 128-bit trace ID extension (high bits)
    pub tags: HashMap<String, String>,
    /// Links to other spans for non-parent-child relationships.
    ///
    /// Span links represent relationships beyond the traditional parent-child tree:
    /// - **Async processing**: Link consumer span to producer span
    /// - **Fan-out/fan-in**: Link aggregated span to multiple input spans
    /// - **Batch processing**: Link batch span to individual item spans
    ///
    /// Each link contains:
    /// - Trace ID and span ID of the linked span
    /// - Link attributes for additional context
    pub links: Vec<SpanLink>,
}
