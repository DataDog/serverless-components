//! Text map propagators for Datadog and W3C TraceContext formats.
//!
//! This module implements the concrete propagators for extracting and injecting trace context
//! from/to HTTP headers and other text-based carriers. It supports two major propagation formats:
//!
//! # Datadog Native Format
//!
//! The `DatadogHeaderPropagator` extracts and injects trace context using Datadog's native headers:
//! - **`x-datadog-trace-id`**: 64-bit trace identifier
//! - **`x-datadog-parent-id`**: 64-bit parent span identifier
//! - **`x-datadog-sampling-priority`**: Sampling decision (-1 to 2)
//! - **`x-datadog-origin`**: Trace origin (e.g., "synthetics", "rum")
//! - **`x-datadog-tags`**: Comma-separated trace tags
//!
//! # W3C TraceContext Format
//!
//! The `TraceContextPropagator` extracts and injects trace context using W3C standard headers:
//! - **`traceparent`**: Version, trace ID, span ID, and sampling flags (128-bit trace IDs)
//! - **`tracestate`**: Vendor-specific state including Datadog metadata
//!
//! # Header Format Examples
//!
//! **Datadog Headers:**
//! ```text
//! x-datadog-trace-id: 1234567890
//! x-datadog-parent-id: 9876543210
//! x-datadog-sampling-priority: 1
//! x-datadog-origin: synthetics
//! x-datadog-tags: _dd.p.dm=-3,_dd.p.tid=abc123
//! ```
//!
//! **W3C TraceContext Headers:**
//! ```text
//! traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01
//! tracestate: dd=s:2;o:rum;t.dm:-4;t.usr.id:baz64,congo=t61rcWkgMzE
//! ```
//!
//! # Sampling Priority
//!
//! Both formats propagate sampling decisions with standard priority values:
//! - **-1**: User reject (drop trace)
//! - **0**: Auto reject (sampled out)
//! - **1**: Auto keep (default)
//! - **2**: User keep (explicit retention)
//!
//! # 128-bit Trace IDs
//!
//! W3C TraceContext supports 128-bit trace IDs, which are split into:
//! - **High 64 bits**: Stored in `_dd.p.tid` tag
//! - **Low 64 bits**: Used as the primary trace ID for compatibility

use std::collections::HashMap;

use lazy_static::lazy_static;
use regex::Regex;
use tracing::{debug, error, warn};

use crate::traces::context::{Sampling, SpanContext};
use crate::traces::propagation::{
    carrier::{Extractor, Injector},
    error::Error,
    Propagator,
};

// ============================================================================
// Datadog Header Keys
// ============================================================================

/// HTTP header key for Datadog trace ID (64-bit).
///
/// Example: `x-datadog-trace-id: 1234567890`
pub const DATADOG_TRACE_ID_KEY: &str = "x-datadog-trace-id";

/// HTTP header key for Datadog parent span ID (64-bit).
///
/// Example: `x-datadog-parent-id: 9876543210`
pub const DATADOG_PARENT_ID_KEY: &str = "x-datadog-parent-id";

/// HTTP header key for Datadog span ID (alternative to parent ID).
///
/// Example: `x-datadog-span-id: 9876543210`
pub const DATADOG_SPAN_ID_KEY: &str = "x-datadog-span-id";

/// HTTP header key for Datadog sampling priority.
///
/// Values: -1 (drop), 0 (auto reject), 1 (auto keep), 2 (user keep)
///
/// Example: `x-datadog-sampling-priority: 1`
pub const DATADOG_SAMPLING_PRIORITY_KEY: &str = "x-datadog-sampling-priority";

/// HTTP header key for Datadog trace origin.
///
/// Common values: "synthetics", "rum", "lambda", "appsec"
///
/// Example: `x-datadog-origin: synthetics`
const DATADOG_ORIGIN_KEY: &str = "x-datadog-origin";

/// HTTP header key for Datadog trace tags (comma-separated key=value pairs).
///
/// Example: `x-datadog-tags: _dd.p.dm=-3,_dd.p.tid=abc123`
pub const DATADOG_TAGS_KEY: &str = "x-datadog-tags";

// ============================================================================
// Datadog Tag Keys (for internal trace metadata)
// ============================================================================

/// Tag key for higher-order trace ID bits (128-bit trace ID support).
///
/// Contains the high 64 bits of a 128-bit trace ID as a hex string.
///
/// Example: `_dd.p.tid: 9291375655657946024`
pub const DATADOG_HIGHER_ORDER_TRACE_ID_BITS_KEY: &str = "_dd.p.tid";

/// Tag key for propagation error messages.
///
/// Set when trace context extraction encounters errors.
///
/// Example: `_dd.propagation_error: decoding_error`
const DATADOG_PROPAGATION_ERROR_KEY: &str = "_dd.propagation_error";

/// Tag key for last parent span ID (used in context resolution).
///
/// Example: `_dd.parent_id: 00f067aa0ba902b7`
pub const DATADOG_LAST_PARENT_ID_KEY: &str = "_dd.parent_id";

/// Tag key for sampling decision mechanism.
///
/// Indicates how the sampling decision was made.
///
/// Example: `_dd.p.dm: -3`
pub const DATADOG_SAMPLING_DECISION_KEY: &str = "_dd.p.dm";

// ============================================================================
// W3C TraceContext Keys
// ============================================================================

/// HTTP header key for W3C traceparent (version-traceId-spanId-flags).
///
/// Format: `00-{32-char-trace-id}-{16-char-span-id}-{2-char-flags}`
///
/// Example: `traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01`
const TRACEPARENT_KEY: &str = "traceparent";

/// HTTP header key for W3C tracestate (vendor-specific state).
///
/// Format: Comma-separated list of vendor key-value pairs
///
/// Example: `tracestate: dd=s:2;o:rum;t.dm:-4,congo=t61rcWkgMzE`
pub const TRACESTATE_KEY: &str = "tracestate";

// ============================================================================
// Baggage Key Prefix
// ============================================================================

/// Prefix for OpenTracing baggage headers.
///
/// Headers starting with this prefix are extracted as trace baggage.
///
/// Example: `ot-baggage-user-id: 12345`
pub const BAGGAGE_PREFIX: &str = "ot-baggage-";

lazy_static! {
    /// Regex for parsing W3C traceparent header.
    ///
    /// Format: `version-traceId-spanId-flags[-optional]`
    /// - Version: 2 hex digits
    /// - Trace ID: 32 hex digits (128-bit)
    /// - Span ID: 16 hex digits (64-bit)
    /// - Flags: 2 hex digits
    /// - Optional: Additional vendor-specific data
    static ref TRACEPARENT_REGEX: Regex =
        Regex::new(r"(?i)^([a-f0-9]{2})-([a-f0-9]{32})-([a-f0-9]{16})-([a-f0-9]{2})(-.*)?$")
            .expect("failed creating regex");

    /// Regex for detecting invalid all-zero segments (e.g., "00000000").
    static ref INVALID_SEGMENT_REGEX: Regex = Regex::new(r"^0+$").expect("failed creating regex");

    /// Regex for validating Datadog trace tag keys.
    ///
    /// Valid keys start with `_dd.p.` and contain only printable ASCII characters.
    static ref VALID_TAG_KEY_REGEX: Regex =
        Regex::new(r"^_dd\.p\.[\x21-\x2b\x2d-\x7e]+$").expect("failed creating regex");

    /// Regex for validating Datadog trace tag values.
    ///
    /// Valid values contain only printable ASCII characters (space through tilde).
    static ref VALID_TAG_VALUE_REGEX: Regex =
        Regex::new(r"^[\x20-\x2b\x2d-\x7e]*$").expect("failed creating regex");

    /// Regex for detecting invalid non-ASCII characters in tracestate.
    static ref INVALID_ASCII_CHARACTERS_REGEX: Regex =
        Regex::new(r"[^\x20-\x7E]+").expect("failed creating regex");

    /// Regex for validating sampling decision values.
    ///
    /// Valid format: `-{digit}` (e.g., "-0", "-1", "-2", "-3", "-4")
    static ref VALID_SAMPLING_DECISION_REGEX: Regex =
        Regex::new(r"^-([0-9])$").expect("failed creating regex");
}

/// Propagator for Datadog native header format.
///
/// Extracts and injects trace context using Datadog's proprietary HTTP headers:
/// - `x-datadog-trace-id`: 64-bit trace identifier
/// - `x-datadog-parent-id`: 64-bit parent span identifier
/// - `x-datadog-sampling-priority`: Sampling decision
/// - `x-datadog-origin`: Trace origin
/// - `x-datadog-tags`: Propagated trace tags
///
/// # Example Headers
///
/// ```text
/// x-datadog-trace-id: 1234567890
/// x-datadog-parent-id: 9876543210
/// x-datadog-sampling-priority: 1
/// x-datadog-origin: synthetics
/// x-datadog-tags: _dd.p.dm=-3,_dd.p.tid=abc123
/// ```
///
/// # Extraction
///
/// The propagator validates and extracts each component:
/// 1. **Trace ID**: Must be non-zero, parsed as u64
/// 2. **Parent ID**: Optional, defaults to 0
/// 3. **Sampling Priority**: Defaults to 2 (user keep) if not present
/// 4. **Origin**: Optional trace origin string
/// 5. **Tags**: Comma-separated key=value pairs starting with `_dd.p.`
///
/// # Error Handling
///
/// Invalid headers result in `None` being returned. Errors are logged for debugging.
#[derive(Clone, Copy)]
pub struct DatadogHeaderPropagator;

impl Propagator for DatadogHeaderPropagator {
    fn extract(&self, carrier: &dyn Extractor) -> Option<SpanContext> {
        Self::extract_context(carrier)
    }

    fn inject(&self, _context: SpanContext, _carrier: &mut dyn Injector) {
        todo!();
    }
}

impl DatadogHeaderPropagator {
    fn extract_context(carrier: &dyn Extractor) -> Option<SpanContext> {
        let trace_id = match Self::extract_trace_id(carrier) {
            Ok(trace_id) => trace_id,
            Err(e) => {
                debug!("{e}");
                return None;
            }
        };

        let parent_id = Self::extract_parent_id(carrier).unwrap_or(0);
        let sampling_priority = match Self::extract_sampling_priority(carrier) {
            Ok(sampling_priority) => sampling_priority,
            Err(e) => {
                debug!("{e}");
                return None;
            }
        };
        let origin = Self::extract_origin(carrier);
        let mut tags = Self::extract_tags(carrier);
        Self::validate_sampling_decision(&mut tags);

        Some(SpanContext {
            trace_id,
            span_id: parent_id,
            sampling: Some(Sampling {
                priority: Some(sampling_priority),
                mechanism: None,
            }),
            origin,
            tags,
            links: Vec::new(),
        })
    }

    fn validate_sampling_decision(tags: &mut HashMap<String, String>) {
        let should_remove =
            tags.get(DATADOG_SAMPLING_DECISION_KEY)
                .is_some_and(|sampling_decision| {
                    let is_invalid = !VALID_SAMPLING_DECISION_REGEX.is_match(sampling_decision);
                    if is_invalid {
                        warn!("Failed to decode `_dd.p.dm`: {}", sampling_decision);
                    }
                    is_invalid
                });

        if should_remove {
            tags.remove(DATADOG_SAMPLING_DECISION_KEY);
            tags.insert(
                DATADOG_PROPAGATION_ERROR_KEY.to_string(),
                "decoding_error".to_string(),
            );
        }

        // todo: appsec standalone
    }

    fn extract_trace_id(carrier: &dyn Extractor) -> Result<u64, Error> {
        let trace_id = carrier
            .get(DATADOG_TRACE_ID_KEY)
            .ok_or(Error::extract("`trace_id` not found", "datadog"))?;

        if INVALID_SEGMENT_REGEX.is_match(trace_id) {
            return Err(Error::extract("Invalid `trace_id` found", "datadog"));
        }

        trace_id
            .parse::<u64>()
            .map_err(|_| Error::extract("Failed to decode `trace_id`", "datadog"))
    }

    fn extract_parent_id(carrier: &dyn Extractor) -> Option<u64> {
        let parent_id = carrier.get(DATADOG_PARENT_ID_KEY)?;

        parent_id.parse::<u64>().ok()
    }

    fn extract_sampling_priority(carrier: &dyn Extractor) -> Result<i8, Error> {
        // todo: enum? Default is USER_KEEP=2
        let sampling_priority = carrier.get(DATADOG_SAMPLING_PRIORITY_KEY).unwrap_or("2");

        sampling_priority
            .parse::<i8>()
            .map_err(|_| Error::extract("Failed to decode `sampling_priority`", "datadog"))
    }

    fn extract_origin(carrier: &dyn Extractor) -> Option<String> {
        let origin = carrier.get(DATADOG_ORIGIN_KEY)?;
        Some(origin.to_string())
    }

    pub fn extract_tags(carrier: &dyn Extractor) -> HashMap<String, String> {
        let carrier_tags = carrier.get(DATADOG_TAGS_KEY).unwrap_or_default();
        let mut tags: HashMap<String, String> = HashMap::new();

        // todo:
        // - trace propagation disabled
        // - trace propagation max lenght

        let pairs = carrier_tags.split(',');
        for pair in pairs {
            if let Some((k, v)) = pair.split_once('=') {
                // todo: reject key on tags extract reject
                if k.starts_with("_dd.p.") {
                    tags.insert(k.to_string(), v.to_string());
                }
            }
        }

        // Handle 128bit trace ID
        if !tags.is_empty() {
            if let Some(trace_id_higher_order_bits) =
                carrier.get(DATADOG_HIGHER_ORDER_TRACE_ID_BITS_KEY)
            {
                if !Self::higher_order_bits_valid(trace_id_higher_order_bits) {
                    warn!(
                        "Malformed Trace ID: {trace_id_higher_order_bits} Failed to decode trace ID from carrier."
                    );
                    tags.insert(
                        DATADOG_PROPAGATION_ERROR_KEY.to_string(),
                        format!("malformed tid {trace_id_higher_order_bits}"),
                    );
                    tags.remove(DATADOG_HIGHER_ORDER_TRACE_ID_BITS_KEY);
                }
            }
        }

        if !tags.contains_key(DATADOG_SAMPLING_DECISION_KEY) {
            tags.insert(DATADOG_SAMPLING_DECISION_KEY.to_string(), "-3".to_string());
        }

        tags
    }

    fn higher_order_bits_valid(trace_id_higher_order_bits: &str) -> bool {
        if trace_id_higher_order_bits.len() != 16 {
            return false;
        }

        match u64::from_str_radix(trace_id_higher_order_bits, 16) {
            Ok(_) => {}
            Err(_) => return false,
        }

        true
    }
}

/// Parsed W3C traceparent header components.
///
/// The traceparent header format is: `version-traceId-spanId-flags`
///
/// # Example
///
/// Header: `00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01`
/// - Version: `00` (W3C TraceContext version 0)
/// - Trace ID: `4bf92f3577b34da6a3ce929d0e0e4736` (128-bit)
/// - Span ID: `00f067aa0ba902b7` (64-bit)
/// - Flags: `01` (sampled)
struct Traceparent {
    /// Sampling priority derived from flags byte.
    ///
    /// - Sampled (`01`): Priority 1 (auto keep)
    /// - Not sampled (`00`): Priority 0 (auto reject)
    sampling_priority: i8,
    /// 128-bit trace identifier.
    trace_id: u128,
    /// 64-bit span identifier.
    span_id: u64,
}

/// Parsed W3C tracestate header Datadog vendor data.
///
/// The tracestate header contains vendor-specific key-value pairs.
/// Datadog uses the `dd=` prefix for its vendor data.
///
/// # Example
///
/// Header: `tracestate: dd=s:2;o:rum;t.dm:-4;t.usr.id:baz64,congo=t61rcWkgMzE`
///
/// Datadog portion (`dd=s:2;o:rum;t.dm:-4;t.usr.id:baz64`) contains:
/// - `s:2`: Sampling priority = 2 (user keep)
/// - `o:rum`: Origin = "rum"
/// - `t.dm:-4`: Sampling decision mechanism = -4
/// - `t.usr.id:baz64`: User ID tag = "baz64"
struct Tracestate {
    /// Sampling priority from tracestate (overrides traceparent if present).
    sampling_priority: Option<i8>,
    /// Trace origin (e.g., "synthetics", "rum", "lambda").
    origin: Option<String>,
    /// Lower-order trace ID bits for matching (hex string).
    lower_order_trace_id: Option<String>,
}

/// Propagator for W3C TraceContext format.
///
/// Extracts and injects trace context using W3C standard headers:
/// - **`traceparent`**: Contains version, trace ID (128-bit), span ID, and sampling flags
/// - **`tracestate`**: Contains vendor-specific metadata including Datadog tags
///
/// # Example Headers
///
/// ```text
/// traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01
/// tracestate: dd=s:2;o:rum;t.dm:-4;t.usr.id:baz64,congo=t61rcWkgMzE
/// ```
///
/// # 128-bit Trace IDs
///
/// W3C TraceContext supports 128-bit trace IDs. The propagator:
/// 1. Extracts the full 128-bit trace ID from traceparent
/// 2. Splits it into high and low 64-bit values
/// 3. Uses the low 64 bits as the primary trace ID (for Datadog compatibility)
/// 4. Stores the high 64 bits in `_dd.p.tid` tag
///
/// # Sampling Priority
///
/// Sampling priority is determined by:
/// 1. Tracestate `s:` value (if present, highest priority)
/// 2. Traceparent flags byte (bit 0 = sampled)
///
/// # Error Handling
///
/// Invalid headers result in `None` being returned. Errors are logged for debugging.
#[derive(Clone, Copy)]
pub struct TraceContextPropagator;

impl Propagator for TraceContextPropagator {
    fn extract(&self, carrier: &dyn Extractor) -> Option<SpanContext> {
        Self::extract_context(carrier)
    }

    fn inject(&self, _context: SpanContext, _carrier: &mut dyn Injector) {
        todo!()
    }
}

impl TraceContextPropagator {
    fn extract_context(carrier: &dyn Extractor) -> Option<SpanContext> {
        let tp = carrier.get(TRACEPARENT_KEY)?.trim();

        match Self::extract_traceparent(tp) {
            Ok(traceparent) => {
                let mut tags = HashMap::new();
                tags.insert(TRACEPARENT_KEY.to_string(), tp.to_string());

                let mut origin = None;
                let mut sampling_priority = traceparent.sampling_priority;
                if let Some(ts) = carrier.get(TRACESTATE_KEY) {
                    if let Some(tracestate) = Self::extract_tracestate(ts, &mut tags) {
                        if let Some(lpid) = tracestate.lower_order_trace_id {
                            tags.insert(DATADOG_LAST_PARENT_ID_KEY.to_string(), lpid);
                        }

                        origin = tracestate.origin;

                        sampling_priority = Self::define_sampling_priority(
                            traceparent.sampling_priority,
                            tracestate.sampling_priority,
                        );
                    }
                } else {
                    debug!("No `dd` value found in tracestate");
                }

                let (trace_id_higher_order_bits, trace_id_lower_order_bits) =
                    Self::split_trace_id(traceparent.trace_id);
                tags.insert(
                    DATADOG_HIGHER_ORDER_TRACE_ID_BITS_KEY.to_string(),
                    trace_id_higher_order_bits.to_string(),
                );

                Some(SpanContext {
                    trace_id: trace_id_lower_order_bits,
                    span_id: traceparent.span_id,
                    sampling: Some(Sampling {
                        priority: Some(sampling_priority),
                        mechanism: None,
                    }),
                    origin,
                    tags,
                    links: Vec::new(),
                })
            }
            Err(e) => {
                error!("Failed to extract traceparent: {e}");
                None
            }
        }
    }

    fn extract_tracestate(
        tracestate: &str,
        tags: &mut HashMap<String, String>,
    ) -> Option<Tracestate> {
        let ts_v = tracestate.split(',').map(str::trim);
        let ts = ts_v.clone().collect::<Vec<&str>>().join(",");

        if INVALID_ASCII_CHARACTERS_REGEX.is_match(&ts) {
            debug!("Received invalid tracestate header {tracestate}");
            return None;
        }

        tags.insert(TRACESTATE_KEY.to_string(), ts.to_string());

        let mut dd: Option<HashMap<String, String>> = None;
        for v in ts_v.clone() {
            if let Some(stripped) = v.strip_prefix("dd=") {
                dd = Some(
                    stripped
                        .split(';')
                        .filter_map(|item| {
                            let mut parts = item.splitn(2, ':');
                            Some((parts.next()?.to_string(), parts.next()?.to_string()))
                        })
                        .collect(),
                );
            }
        }

        if let Some(dd) = dd {
            let mut tracestate = Tracestate {
                sampling_priority: None,
                origin: None,
                lower_order_trace_id: None,
            };

            if let Some(ts_sp) = dd.get("s") {
                if let Ok(p_sp) = ts_sp.parse::<i8>() {
                    tracestate.sampling_priority = Some(p_sp);
                }
            }

            if let Some(o) = dd.get("o") {
                tracestate.origin = Some(Self::decode_tag_value(o));
            }

            if let Some(lo_tid) = dd.get("p") {
                tracestate.lower_order_trace_id = Some(lo_tid.to_string());
            }

            // Convert from `t.` to `_dd.p.`
            for (k, v) in &dd {
                if let Some(stripped) = k.strip_prefix("t.") {
                    let nk = format!("_dd.p.{stripped}");
                    tags.insert(nk, Self::decode_tag_value(v));
                }
            }

            return Some(tracestate);
        }

        None
    }

    fn decode_tag_value(value: &str) -> String {
        value.replace('~', "=")
    }

    fn define_sampling_priority(
        traceparent_sampling_priority: i8,
        tracestate_sampling_priority: Option<i8>,
    ) -> i8 {
        if let Some(ts_sp) = tracestate_sampling_priority {
            if (traceparent_sampling_priority == 1 && ts_sp > 0)
                || (traceparent_sampling_priority == 0 && ts_sp < 0)
            {
                return ts_sp;
            }
        }

        traceparent_sampling_priority
    }

    fn extract_traceparent(traceparent: &str) -> Result<Traceparent, Error> {
        let captures = TRACEPARENT_REGEX
            .captures(traceparent)
            .ok_or_else(|| Error::extract("invalid traceparent", "traceparent"))?;

        let version = &captures[1];
        let trace_id = &captures[2];
        let span_id = &captures[3];
        let flags = &captures[4];
        let tail = captures.get(5).map_or("", |m| m.as_str());

        Self::extract_version(version, tail)?;

        let trace_id = Self::extract_trace_id(trace_id)?;
        let span_id = Self::extract_span_id(span_id)?;

        let trace_flags = Self::extract_trace_flags(flags)?;
        let sampling_priority = i8::from(trace_flags & 0x1 != 0);

        Ok(Traceparent {
            sampling_priority,
            trace_id,
            span_id,
        })
    }

    fn extract_version(version: &str, tail: &str) -> Result<(), Error> {
        match version {
            "ff" => {
                return Err(Error::extract(
                    "`ff` is an invalid traceparent version",
                    "traceparent",
                ));
            }
            "00" => {
                if !tail.is_empty() {
                    return Err(Error::extract(
                        "Traceparent with version `00` should contain only 4 values delimited by `-`",
                        "traceparent",
                    ));
                }
            }
            _ => {
                warn!("Unsupported traceparent version {version}, still atempenting to parse");
            }
        }

        Ok(())
    }

    fn extract_trace_id(trace_id: &str) -> Result<u128, Error> {
        if INVALID_SEGMENT_REGEX.is_match(trace_id) {
            return Err(Error::extract(
                "`0` value for trace_id is invalid",
                "traceparent",
            ));
        }

        u128::from_str_radix(trace_id, 16)
            .map_err(|_| Error::extract("Failed to decode trace_id", "traceparent"))
    }

    #[allow(clippy::cast_possible_truncation)]
    fn split_trace_id(trace_id: u128) -> (u64, u64) {
        let trace_id_lower_order_bits = trace_id as u64;
        let trace_id_higher_order_bits = (trace_id >> 64) as u64;

        (trace_id_higher_order_bits, trace_id_lower_order_bits)
    }

    fn extract_span_id(span_id: &str) -> Result<u64, Error> {
        if INVALID_SEGMENT_REGEX.is_match(span_id) {
            return Err(Error::extract(
                "`0` value for span_id is invalid",
                "traceparent",
            ));
        }

        u64::from_str_radix(span_id, 16)
            .map_err(|_| Error::extract("Failed to decode span_id", "traceparent"))
    }

    fn extract_trace_flags(flags: &str) -> Result<u8, Error> {
        if flags.len() != 2 {
            return Err(Error::extract("Invalid trace flags length", "traceparent"));
        }

        u8::from_str_radix(flags, 16)
            .map_err(|_| Error::extract("Failed to decode trace_flags", "traceparent"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod test {
    use super::*;

    #[test]
    fn test_extract_datadog_propagator() {
        let headers = HashMap::from([
            ("x-datadog-trace-id".to_string(), "1234".to_string()),
            ("x-datadog-parent-id".to_string(), "5678".to_string()),
            ("x-datadog-sampling-priority".to_string(), "1".to_string()),
            ("x-datadog-origin".to_string(), "synthetics".to_string()),
            (
                "x-datadog-tags".to_string(),
                "_dd.p.test=value,_dd.p.tid=4321,any=tag".to_string(),
            ),
        ]);

        let propagator = DatadogHeaderPropagator;

        let context = propagator
            .extract(&headers)
            .expect("couldn't extract trace context");

        assert_eq!(context.trace_id, 1234);
        assert_eq!(context.span_id, 5678);
        assert_eq!(context.sampling.unwrap().priority, Some(1));
        assert_eq!(context.origin, Some("synthetics".to_string()));
        println!("{:?}", context.tags);
        assert_eq!(context.tags.get("_dd.p.test").unwrap(), "value");
        assert_eq!(context.tags.get("_dd.p.tid").unwrap(), "4321");
        assert_eq!(context.tags.get("_dd.p.dm").unwrap(), "-3");
    }

    #[test]
    fn test_extract_traceparent_propagator() {
        let headers = HashMap::from([
            (
                "traceparent".to_string(),
                "00-80f198ee56343ba864fe8b2a57d3eff7-00f067aa0ba902b7-01".to_string(),
            ),
            (
                "tracestate".to_string(),
                "dd=p:00f067aa0ba902b7;s:2;o:rum".to_string(),
            ),
        ]);

        let propagator = TraceContextPropagator;
        let context = propagator
            .extract(&headers)
            .expect("couldn't extract trace context");

        assert_eq!(context.trace_id, 7_277_407_061_855_694_839);
        assert_eq!(context.span_id, 67_667_974_448_284_343);
        assert_eq!(context.sampling.unwrap().priority, Some(2));
        assert_eq!(context.origin, Some("rum".to_string()));
        assert_eq!(
            context.tags.get("traceparent").unwrap(),
            "00-80f198ee56343ba864fe8b2a57d3eff7-00f067aa0ba902b7-01"
        );
        assert_eq!(
            context.tags.get("tracestate").unwrap(),
            "dd=p:00f067aa0ba902b7;s:2;o:rum"
        );
        assert_eq!(
            context.tags.get("_dd.p.tid").unwrap(),
            "9291375655657946024"
        );
        assert_eq!(
            context.tags.get("_dd.parent_id").unwrap(),
            "00f067aa0ba902b7"
        );
    }
}
