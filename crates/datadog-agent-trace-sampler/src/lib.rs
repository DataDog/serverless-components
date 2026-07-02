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
//! This crate is a dependency-free 1:1 port of the Go trace agent's error
//! sampler (`ScoreSampler` targeting `ErrorTPS`). The error sampler is a
//! *rescue* sampler: after an agent decides to drop a trace, the trace gets a
//! second look, and if it contains an error it is kept, up to a budget of
//! `target_tps` error traces per second distributed fairly across distinct trace
//! signatures. This guarantees error visibility even under aggressive sampling.
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
    pub root_global_sample_rate: f64,
    pub spans: &'a [SpanView<'a>],
}

/// Configuration for the error sampler.
#[derive(Debug, Clone, Copy)]
pub struct ErrorSamplerConfig {
    /// Target error traces per second (`ErrorTPS`). `0.0` disables the sampler
    /// (every candidate is dropped, i.e. never rescued).
    pub target_tps: f64,
    /// Extra raw sampling rate applied on top of the computed rate.
    pub extra_sample_rate: f64,
}

impl Default for ErrorSamplerConfig {
    /// Matches the Go agent defaults: `ErrorTPS = 10`, `ExtraSampleRate = 1.0`.
    fn default() -> Self {
        ErrorSamplerConfig {
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
