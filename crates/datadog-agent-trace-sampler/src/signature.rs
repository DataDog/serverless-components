// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Trace signature computation and deterministic sample-by-rate.
//!
//! 1:1 port of the Go trace agent's `pkg/trace/sampler/signature.go` and the
//! `SampleByRate` helper in `pkg/trace/sampler/sampler.go`.

use crate::SpanView;

/// A hash representation of a trace, used to identify similar traces.
pub type Signature = u64;

// FNV-1a 32-bit constants (see `pkg/trace/sampler/signature.go`).
const OFFSET32: u32 = 2_166_136_261;
const PRIME32: u32 = 16_777_619;

// Good number for Knuth hashing (large, prime). Matches the Go agent's
// `samplerHasher` so keep decisions are identical for a given trace ID.
const SAMPLER_HASHER: u64 = 1_111_111_111_111_111_111;

/// FNV-1a hash accumulator, simplified from `hash/fnv` to avoid allocations,
/// mirroring the Go agent's `sum32a`.
struct Fnv1a32(u32);

impl Fnv1a32 {
    fn new() -> Self {
        Fnv1a32(OFFSET32)
    }

    fn write(&mut self, data: &[u8]) {
        let mut hash = self.0;
        for &c in data {
            hash ^= u32::from(c);
            hash = hash.wrapping_mul(PRIME32);
        }
        self.0 = hash;
    }

    fn write_char(&mut self, c: u8) {
        let mut hash = self.0;
        hash ^= u32::from(c);
        hash = hash.wrapping_mul(PRIME32);
        self.0 = hash;
    }

    fn sum32(&self) -> u32 {
        self.0
    }
}

/// Hash of a single span: FNV-1a over `(env, service, name, error)`, plus
/// `resource` for the root, plus `http.status_code` and `error.type` when present.
fn compute_span_hash(span: &SpanView, env: &str, with_resource: bool) -> u32 {
    let mut h = Fnv1a32::new();
    h.write(env.as_bytes());
    h.write(span.service.as_bytes());
    h.write(span.name.as_bytes());
    h.write_char(u8::from(span.error));
    if with_resource {
        h.write(span.resource.as_bytes());
    }
    if let Some(code) = span.http_status_code {
        h.write(code.as_bytes());
    }
    if let Some(typ) = span.error_type {
        h.write(typ.as_bytes());
    }
    h.sum32()
}

/// Generates the signature of a trace given its root span.
///
/// The signature is based on the hash of `(env, service, name, resource, error)`
/// for the root, XOR-merged with the sorted, deduplicated set of
/// `(env, service, name, error)` hashes of every span.
///
/// `spans` must be non-empty; callers guard the empty case.
pub fn compute_signature_with_root_and_env(
    spans: &[SpanView],
    root_index: usize,
    env: &str,
) -> Signature {
    let root_hash = compute_span_hash(&spans[root_index], env, true);

    let mut span_hashes: Vec<u32> = spans
        .iter()
        .map(|span| compute_span_hash(span, env, false))
        .collect();

    // Sort, dedupe, then merge all the hashes to build the signature.
    span_hashes.sort_unstable();

    let mut last = span_hashes[0];
    let mut trace_hash = last ^ root_hash;
    for &hash in &span_hashes[1..] {
        if hash != last {
            last = hash;
            trace_hash ^= hash;
        }
    }

    Signature::from(trace_hash)
}

/// Returns whether to keep a trace based on its ID and a sampling rate.
///
/// Assumes trace IDs are nearly uniformly distributed. Deterministic for a
/// given `(trace_id, rate)` pair, matching the Go agent's `SampleByRate`.
pub fn sample_by_rate(trace_id: u64, rate: f64) -> bool {
    if rate < 1.0 {
        // Integer comparison, matching Go's `traceID*samplerHasher < uint64(rate*maxTraceIDFloat)`:
        // the left side stays an exact u64 product and the right side is truncated to u64. Casting
        // the product to f64 instead would lose precision (52-bit mantissa) and flip boundary IDs.
        return trace_id.wrapping_mul(SAMPLER_HASHER) < (rate * (u64::MAX as f64)) as u64;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span<'a>(service: &'a str, name: &'a str, resource: &'a str, error: bool) -> SpanView<'a> {
        SpanView {
            service,
            name,
            resource,
            error,
            http_status_code: None,
            error_type: None,
        }
    }

    fn sig(spans: &[SpanView], env: &str) -> Signature {
        compute_signature_with_root_and_env(spans, 0, env)
    }

    // Ports TestSum32a: our FNV-1a must match the standard hash/fnv New32a output.
    #[test]
    fn fnv1a_matches_reference() {
        // Reference values computed with Go's hash/fnv New32a.
        let cases: &[(&str, u32)] = &[
            ("this", 0xda2b_d281),
            ("is", 0x4e38_8f15),
            ("a", 0xe40c_292c),
        ];
        for &(input, expected) in cases {
            let mut h = Fnv1a32::new();
            h.write(input.as_bytes());
            assert_eq!(h.sum32(), expected, "input={input}");
        }
    }

    // Ports TestSignatureSimilar: traces differing only in span count/duration
    // (same service/name/resource/error set) share a signature.
    #[test]
    fn signature_similar() {
        let t1 = [
            span("x1", "y1", "z1", false),
            span("x1", "y1", "z1", false),
            span("x1", "y1", "z1", false),
            span("x2", "y2", "z2", false),
        ];
        let t2 = [
            span("x1", "y1", "z1", false),
            span("x1", "y1", "z1", false),
            span("x2", "y2", "z2", false),
        ];
        assert_eq!(sig(&t1, ""), sig(&t2, ""));
    }

    // Ports TestSignatureDifferentError: an error on any span changes the signature.
    #[test]
    fn signature_different_error() {
        let t1 = [
            span("x1", "y1", "z1", false),
            span("x1", "y1", "z1", false),
            span("x2", "y2", "z2", false),
        ];
        let t2 = [
            span("x1", "y1", "z1", false),
            span("x1", "y1", "z1", true),
            span("x2", "y2", "z2", false),
        ];
        assert_ne!(sig(&t1, ""), sig(&t2, ""));
    }

    // Ports TestSignatureDifferentRoot: the root's resource is part of the signature.
    #[test]
    fn signature_different_root_resource() {
        let t1 = [span("x1", "y1", "z1", false), span("x1", "y1", "z1", false)];
        let t2 = [span("x1", "y1", "z2", false), span("x1", "y1", "z1", false)];
        assert_ne!(sig(&t1, ""), sig(&t2, ""));
    }

    // Ports TestSignatureDifference: http.status_code and error.type change the signature.
    #[test]
    fn signature_meta_difference() {
        let base = [span("x1", "y1", "z1", false)];

        let mut with_status = span("x1", "y1", "z1", false);
        with_status.http_status_code = Some("200");
        assert_ne!(sig(&base, ""), sig(&[with_status], ""));

        let mut with_error_type = span("x1", "y1", "z1", false);
        with_error_type.error_type = Some("error: nil");
        assert_ne!(sig(&base, ""), sig(&[with_error_type], ""));
    }

    #[test]
    fn signature_env_difference() {
        let t = [span("x1", "y1", "z1", false)];
        assert_ne!(sig(&t, "test"), sig(&t, "prod"));
    }

    // sample_by_rate is deterministic: same (trace_id, rate) always yields the
    // same decision, and rate >= 1 always keeps.
    #[test]
    fn sample_by_rate_determinism() {
        let trace_id = 0x1234_5678_9abc_def0;
        let a = sample_by_rate(trace_id, 0.5);
        let b = sample_by_rate(trace_id, 0.5);
        assert_eq!(a, b);

        assert!(sample_by_rate(trace_id, 1.0));
        assert!(sample_by_rate(0, 1.5));
        assert!(!sample_by_rate(trace_id, 0.0));
    }

    // Higher rates are monotonically more likely to keep a given trace ID.
    #[test]
    fn sample_by_rate_monotonic() {
        // A kept trace ID at a low rate stays kept as the rate rises.
        let kept_id = 987_654_321;
        assert!(
            sample_by_rate(kept_id, 0.25),
            "fixture must be kept at 0.25"
        );
        assert!(sample_by_rate(kept_id, 0.5));
        assert!(sample_by_rate(kept_id, 0.75));

        // A dropped trace ID at a high rate stays dropped as the rate falls.
        let dropped_id = u64::MAX / SAMPLER_HASHER; // product near u64::MAX => dropped
        assert!(
            !sample_by_rate(dropped_id, 0.9),
            "fixture must be dropped at 0.9"
        );
        assert!(!sample_by_rate(dropped_id, 0.5));
        assert!(!sample_by_rate(dropped_id, 0.1));
    }
}
