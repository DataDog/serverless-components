# Datadog Agent Trace Sampler

Agent-side trace sampling shared across the serverless agents (bottlecap and the
Serverless Compatibility Layer).

This crate implements the Go trace agent's **error sampler** as a *rescue*
sampler: after an agent decides to drop a trace, the trace gets a second look
via `ErrorsSampler::sample`, and if it contains an error it may be kept. Two
rescue strategies are available, selected by `ErrorSamplerMode`:

- **`AlwaysKeep`** — keep every error chunk unconditionally, no budget/state/
  clock; stamps `_dd.errors_sr = 1.0`. Suits low-volume, freeze/thaw
  environments like Lambda (bottlecap's default).
- **`RateLimited`** — a dependency-free 1:1 port of the Go agent's
  `ScoreSampler` (`ScoreSampler` targeting `ErrorTPS`, from `pkg/trace/sampler/`
  in `DataDog/datadog-agent`): keep up to `target_tps` error traces per second,
  distributed fairly across distinct trace signatures. Suits continuous
  processes that can hit error storms (the Serverless Compatibility Layer's
  default).

Per-platform defaults are chosen by each consumer's config layer (e.g. via a
`DD_APM_ERROR_SAMPLER_MODE` env var), not this crate — it has no notion of
which platform it runs on.

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

// `ErrorSamplerConfig::default()` uses `ErrorSamplerMode::RateLimited`
// (Go-parity default); set `mode: ErrorSamplerMode::AlwaysKeep` for the
// simpler unconditional-rescue strategy.
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

Setting `target_tps` to `0.0` (or negative) disables the sampler in both modes:
every candidate returns `SampleDecision::Drop` (i.e. nothing is rescued).
`extra_sample_rate` is only meaningful in `RateLimited` mode.
