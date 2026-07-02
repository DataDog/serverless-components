# Datadog Agent Trace Sampler

Agent-side trace sampling shared across the serverless agents (bottlecap and the
Serverless Compatibility Layer).

This crate is a dependency-free 1:1 port of the Go trace agent's **error sampler**
(`ScoreSampler` targeting `ErrorTPS`, from `pkg/trace/sampler/` in
`DataDog/datadog-agent`). The error sampler is a *rescue* sampler: after an agent
decides to drop a trace, the trace gets a second look, and if it contains an
error it is kept, up to a budget of `target_tps` error traces per second
distributed fairly across distinct trace signatures. This guarantees error
visibility even under aggressive sampling.

## Why dependency-free

The public API takes primitives in (`SpanView` / `TraceView`) and returns a
`SampleDecision` out; it never exposes a protobuf `Span` type. This lets
consumers that pin different `libdatadog` revisions share the crate without
compiling incompatible `pb::Span` types into their build graphs.

## Usage

```rust
use datadog_agent_trace_sampler::{
    ErrorSamplerConfig, ErrorsSampler, SampleDecision, SpanView, TraceView,
};

let mut sampler = ErrorsSampler::new(ErrorSamplerConfig::default());

let spans = [SpanView {
    service: "web",
    name: "web.request",
    resource: "GET /",
    error: true,
    http_status_code: Some("500"),
    error_type: None,
}];
let trace = TraceView {
    env: "prod",
    trace_id: 0xdead_beef,
    root_index: 0,
    root_global_sample_rate: 1.0,
    spans: &spans,
};

// `now_unix_secs` drives the rolling window and is passed in (not read from a
// clock) so the crate stays dependency-free and deterministically testable.
match sampler.sample(1_700_000_000, &trace) {
    SampleDecision::Keep { errors_sr } => {
        // caller stamps `_dd.errors_sr = errors_sr` on the root span
    }
    SampleDecision::Drop => {
        // the pending agent-side drop proceeds
    }
}
```

`ErrorsSampler::sample` takes `&mut self` (the rolling buffer and rate map mutate
on every call). Consumers that share one sampler across threads wrap it in
`Arc<Mutex<ErrorsSampler>>`.

Setting `target_tps` to `0.0` disables the sampler: every candidate returns
`SampleDecision::Drop` (i.e. nothing is rescued).
