// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(not(test), deny(clippy::panic))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![cfg_attr(not(test), deny(clippy::todo))]
#![cfg_attr(not(test), deny(clippy::unimplemented))]

//! Agent-side trace sampling shared across serverless agents (bottlecap and the
//! Serverless Compatibility Layer).
//!
//! This crate implements a dual-mode *rescue* sampler for the Go trace agent's
//! error sampler (`ScoreSampler` targeting `ErrorTPS`): after an agent decides to
//! drop a trace, the trace gets a second look via [`ErrorsSampler::sample`], and
//! if it contains an error it may be rescued. Two strategies are available,
//! selected by [`ErrorSamplerMode`]:
//!
//! - [`ErrorSamplerMode::AlwaysKeep`]: keep every error chunk unconditionally, no
//!   budget/state/clock; stamps `_dd.errors_sr = 1.0`. Suits low-volume,
//!   freeze/thaw environments like Lambda (bottlecap's default).
//! - [`ErrorSamplerMode::RateLimited`]: a dependency-free 1:1 port of the Go
//!   agent's `ScoreSampler`, keeping up to `target_tps` error traces per second
//!   distributed fairly across distinct trace signatures. Suits continuous
//!   processes that can hit error storms (the Serverless Compatibility Layer's
//!   default).
//!
//! Per-platform defaults are chosen by each consumer's config layer, not this
//! crate.
//!
//! The public API takes primitives in and returns a decision out (no protobuf
//! `Span` type), so consumers pinning different `libdatadog` revisions can share
//! it without compiling incompatible span types into their build graphs.
//!
//! # Example
//!
//! ```
//! use datadog_agent_trace_sampler::{
//!     ErrorSamplerConfig, ErrorsSampler, SampleDecision, SpanView, TraceView,
//! };
//!
//! let mut sampler = ErrorsSampler::new(ErrorSamplerConfig::default());
//! let spans = [SpanView {
//!     service: "web",
//!     name: "web.request",
//!     resource: "GET /",
//!     error: true,
//!     http_status_code: Some("500"),
//!     error_type: None,
//! }];
//! let trace = TraceView {
//!     env: "prod",
//!     trace_id: 0xdead_beef,
//!     root_index: 0,
//!     root_global_sample_rate: 1.0,
//!     spans: &spans,
//! };
//! match sampler.sample(/* now_unix_secs */ 1_700_000_000, &trace) {
//!     SampleDecision::Keep { errors_sr } => {
//!         // caller stamps `_dd.errors_sr = errors_sr` on the root span
//!         let _ = errors_sr;
//!     }
//!     SampleDecision::Drop => { /* the pending drop proceeds */ }
//! }
//! ```

mod score_sampler;
mod signature;

pub use score_sampler::ErrorsSampler;
pub use signature::Signature;

/// A read-only view of a single span, holding only the fields the sampler needs.
///
/// `http_status_code` and `error_type` come from the span's `meta` map keys
/// `http.status_code` and `error.type` respectively.
#[derive(Debug, Clone, Copy)]
pub struct SpanView<'a> {
    pub service: &'a str,
    pub name: &'a str,
    pub resource: &'a str,
    /// Whether the span is an error. The Go agent's span `Error` field is an
    /// `int32`, and its v0 signature hash folds the raw low byte into the hash,
    /// so a span with `error > 1` would hash differently there. Normalizing to
    /// a bool matches Go's newer `computeSpanHashV1`; in practice tracers only
    /// ever emit 0 or 1, so the two agree on real traffic.
    pub error: bool,
    pub http_status_code: Option<&'a str>,
    pub error_type: Option<&'a str>,
}

/// A read-only view of a trace chunk to be sampled.
#[derive(Debug, Clone, Copy)]
pub struct TraceView<'a> {
    pub env: &'a str,
    pub trace_id: u64,
    /// Index of the root span within `spans`.
    pub root_index: usize,
    /// The root span's global sample rate (`metrics["_sample_rate"]`), default 1.0.
    /// Callers may pass the raw wire value: the error sampler falls back to 1.0 for
    /// anything non-finite or outside `(0, 1]`.
    pub root_global_sample_rate: f64,
    pub spans: &'a [SpanView<'a>],
}

/// Selects which rescue strategy [`ErrorsSampler`] uses.
///
/// Per-platform defaults are chosen by each consumer's config layer (bottlecap
/// defaults to `AlwaysKeep`, the Serverless Compatibility Layer to
/// `RateLimited`); this crate has no notion of which platform it runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSamplerMode {
    /// Keep every error chunk. No budget, no rolling window, no clock. Stamps
    /// `_dd.errors_sr = 1.0`. `target_tps` still gates whether the sampler is
    /// disabled (`<= 0.0` drops everything, matching `RateLimited`);
    /// `extra_sample_rate` is unused.
    AlwaysKeep,
    /// Full 1:1 Go `ScoreSampler` port: keep up to `target_tps` error traces
    /// per second, distributed fairly across distinct trace signatures. Stamps
    /// the computed `errors_sr`.
    RateLimited,
}

/// Configuration for the error sampler.
#[derive(Debug, Clone, Copy)]
pub struct ErrorSamplerConfig {
    /// Which rescue strategy to use.
    pub mode: ErrorSamplerMode,
    /// Target error traces per second (`ErrorTPS`). `0.0` (or negative)
    /// disables the sampler in both modes (every candidate is dropped, i.e.
    /// never rescued). Only meaningful for rate computation in `RateLimited`.
    pub target_tps: f64,
    /// Extra raw sampling rate applied on top of the computed rate. Only
    /// meaningful in `RateLimited`.
    pub extra_sample_rate: f64,
}

impl Default for ErrorSamplerConfig {
    /// Matches the Go agent defaults: `ErrorTPS = 10`, `ExtraSampleRate = 1.0`,
    /// mode `RateLimited` (the crate-level default preserves Go parity;
    /// per-platform defaults are set by each consumer).
    fn default() -> Self {
        ErrorSamplerConfig {
            mode: ErrorSamplerMode::RateLimited,
            target_tps: 10.0,
            extra_sample_rate: 1.0,
        }
    }
}

/// The outcome of sampling a trace.
#[derive(Debug, PartialEq)]
pub enum SampleDecision {
    /// Keep (rescue) the trace. The caller should stamp `_dd.errors_sr` on the
    /// root span with `errors_sr`.
    Keep { errors_sr: f64 },
    /// Drop the trace (do not rescue it); the pending agent-side drop proceeds.
    Drop,
}
