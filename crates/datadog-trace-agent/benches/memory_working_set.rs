// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0
//
// Measures the steady-state RSS working set of the datadog-trace-agent under a
// high workload.
//
// Traces are sent at a fixed rate over HTTP to the datadog-trace-agent server with
// realistic flush intervals. The benchmark runs long enough to span multiple
// flush cycles and reports per-cycle peak/trough RSS once the system has
// reached steady state.
//
// Run with: cargo bench --bench memory_working_set

use async_trait::async_trait;
use datadog_trace_agent::{
    aggregator::TraceAggregator,
    config::{Config, Tags},
    env_verifier::EnvVerifier,
    mini_agent::MiniAgent,
    proxy_flusher::ProxyFlusher,
    stats_flusher::ServerlessStatsFlusher,
    stats_processor::ServerlessStatsProcessor,
    trace_flusher::{ServerlessTraceFlusher, TraceFlusher},
    trace_processor::ServerlessTraceProcessor,
};
use http_body_util::BodyExt;
use hyper::Request;
use hyper_util::rt::TokioIo;
use libdd_common::{Endpoint, http_common};
use libdd_trace_obfuscation::obfuscation_config::ObfuscationConfig;
use libdd_trace_utils::trace_utils::{self, MiniAgentMetadata};
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::oneshot;
use tokio::time::MissedTickBehavior;

const BENCH_PORT: u16 = 18126;

/// Traces sent per second. Each produces 1 root span + CHILD_SPANS_PER_INVOCATION spans.
const TRACE_RATE_HZ: u64 = 10000;
const CHILD_SPANS_PER_INVOCATION: usize = 10;

/// Flush interval in seconds — matches a realistic production default.
const FLUSH_INTERVAL_SECS: u64 = 3;

/// Cycles discarded before measurement begins (allow the system to warm up).
const WARMUP_CYCLES: u64 = 2;

/// Cycles over which steady-state RSS is averaged.
const MEASURE_CYCLES: u64 = 10;

const SERVICE: &str = "test-service";
const TRACE_ID: u64 = 0x30c5a_ec80b_b4081_5;
const ROOT_SPAN_ID: u64 = 3_514_407_258_445_580_309;

// ── Agent setup ───────────────────────────────────────────────────────────────

struct NoopEnvVerifier;

#[async_trait]
impl EnvVerifier for NoopEnvVerifier {
    async fn verify_environment(
        &self,
        _timeout_ms: u64,
        _env_type: &trace_utils::EnvironmentType,
        _os: &str,
    ) -> MiniAgentMetadata {
        MiniAgentMetadata::default()
    }
}

#[allow(clippy::unwrap_used)]
fn create_bench_config() -> Config {
    Config {
        dd_site: "notdog.com".to_string(),
        dd_apm_receiver_port: BENCH_PORT,
        dd_dogstatsd_port: 8125,
        env_type: trace_utils::EnvironmentType::AzureFunction,
        app_name: Some(SERVICE.to_string()),
        max_request_content_length: 10 * 1024 * 1024,
        obfuscation_config: ObfuscationConfig::new().unwrap(),
        os: "linux".to_string(),
        tags: Tags::new(),
        stats_flush_interval_secs: FLUSH_INTERVAL_SECS,
        trace_flush_interval_secs: FLUSH_INTERVAL_SECS,
        // Flush destination is unreachable; the send fails fast and the
        // aggregator queue is already cleared by the time send() is called,
        // so memory is freed regardless of send outcome.
        trace_intake: Endpoint::default(),
        trace_stats_intake: Endpoint::default(),
        profiling_intake: Endpoint::default(),
        proxy_request_timeout_secs: 1,
        // No retries — the flush phase must complete quickly for stable cycles.
        proxy_request_max_retries: 0,
        proxy_request_retry_backoff_base_ms: 0,
        verify_env_timeout_ms: 100,
        proxy_url: None,
    }
}

// ── Span builders ─────────────────────────────────────────────────────────────

fn span_meta() -> serde_json::Value {
    json!({
        "_dd.agent_hostname": "",
        "_dd.tracer_version": "5.87.0",
        "_dd.serverless_compat_version": "0.0.0",
        "aas.environment.function_runtime": "~4",
        "aas.environment.instance_id": "unknown",
        "aas.environment.instance_name": "unknown",
        "aas.environment.os": "linux",
        "aas.environment.runtime": "node",
        "aas.environment.runtime_version": "22",
        "aas.resource.group": "rg-test",
        "aas.resource.id": format!("/subscriptions/00000000-0000-0000-0000-000000000000/resourcegroups/rg-test/providers/microsoft.web/sites/{SERVICE}"),
        "aas.site.kind": "functionapp",
        "aas.site.name": SERVICE,
        "aas.site.type": "function",
        "aas.subscription.id": "00000000-0000-0000-0000-000000000000",
        "env": "test",
        "language": "javascript",
        "process_id": "123",
        "runtime-id": "00000000-0000-0000-0000-000000000000",
        "service": SERVICE,
        "version": "0.0.0",
    })
}

fn root_span(start_ns: i64) -> serde_json::Value {
    json!({
        "service": SERVICE,
        "name": "azure.functions.invoke",
        "resource": "GET /api/httptest",
        "trace_id": TRACE_ID,
        "span_id": ROOT_SPAN_ID,
        "parent_id": 0,
        "start": start_ns,
        "duration": 10_000_000,
        "error": 0,
        "meta": span_meta(),
        "metrics": { "_dd.measured": 1, "_dd.top_level": 1, "_sampling_priority_v1": 1, "_top_level": 1, "_trace_root": 1 },
        "type": ""
    })
}

fn child_span(span_id: u64, start_ns: i64) -> serde_json::Value {
    json!({
        "service": SERVICE,
        "name": "test.span",
        "resource": "test.span",
        "trace_id": TRACE_ID,
        "span_id": span_id,
        "parent_id": ROOT_SPAN_ID,
        "start": start_ns,
        "duration": 10_000_000,
        "error": 0,
        "meta": span_meta(),
        "metrics": { "_dd.measured": 0, "_sampling_priority_v1": 1 },
        "type": ""
    })
}

fn make_invocation_payload(invocation_start_ns: i64, idx: usize) -> Vec<u8> {
    let mut spans = vec![root_span(invocation_start_ns)];
    for j in 0..CHILD_SPANS_PER_INVOCATION {
        let span_id = (idx * CHILD_SPANS_PER_INVOCATION + j + 1) as u64;
        spans.push(child_span(span_id, invocation_start_ns));
    }
    rmp_serde::to_vec(&vec![spans]).expect("Failed to msgpack-serialize trace payload")
}

// ── Sender ────────────────────────────────────────────────────────────────────

/// Sends invocations at TRACE_RATE_HZ over a single keep-alive connection
/// for the given duration.
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn run_sender(port: u16, start_ns: i64, duration: Duration) {
    let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("Failed to connect to mini agent");
    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .expect("HTTP/1.1 handshake failed");

    tokio::spawn(async move {
        let _ = conn.await;
    });

    let mut ticker = tokio::time::interval(Duration::from_micros(1_000_000 / TRACE_RATE_HZ));
    // Skip missed ticks rather than bursting — if a send is slow we accept
    // the lower effective rate rather than flooding the server.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let deadline = Instant::now() + duration;
    let mut idx = 0usize;

    loop {
        ticker.tick().await;
        if Instant::now() >= deadline {
            break;
        }

        let invocation_start = start_ns + (idx as i64 * 1_000_000);
        let body = make_invocation_payload(invocation_start, idx);
        let body_len = body.len();

        let request = Request::builder()
            .method("POST")
            .uri("/v0.4/traces")
            .header("Content-Type", "application/msgpack")
            .header("Content-Length", body_len.to_string())
            .header("datadog-meta-lang", "javascript")
            .header("datadog-meta-tracer-version", "5.87.0")
            .header("datadog-meta-lang-version", "22")
            .header("datadog-container-id", "bench-container-01")
            .body(http_common::Body::from(body))
            .expect("Failed to build HTTP request");

        let response = sender
            .send_request(request)
            .await
            .expect("Failed to send trace request");

        // Consume response body so the connection can be reused.
        let _ = response.into_body().collect().await;
        idx += 1;
    }
}

// ── RSS sampler ───────────────────────────────────────────────────────────────

fn rss_kb() -> usize {
    memory_stats::memory_stats()
        .map(|s| s.physical_mem / 1024)
        .unwrap_or(0)
}

/// Records (elapsed_secs, rss_kb) samples every 100ms until stop_rx fires.
async fn run_sampler(
    bench_start: Instant,
    samples: Arc<Mutex<Vec<(f64, usize)>>>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let elapsed = bench_start.elapsed().as_secs_f64();
                let rss = rss_kb();
                #[allow(clippy::unwrap_used)]
                samples.lock().unwrap().push((elapsed, rss));
            }
            _ = &mut stop_rx => break,
        }
    }
}

// ── Analysis ──────────────────────────────────────────────────────────────────

struct CycleStats {
    peak_kb: usize,
    trough_kb: usize,
}

/// Groups RSS samples into flush cycles starting after the warmup window
/// and returns per-cycle peak and trough.
fn analyze(
    samples: &[(f64, usize)],
    flush_interval_secs: u64,
    warmup_cycles: u64,
    measure_cycles: u64,
) -> Vec<CycleStats> {
    let fi = flush_interval_secs as f64;
    let warmup_end = warmup_cycles as f64 * fi;

    (0..measure_cycles)
        .filter_map(|c| {
            let start = warmup_end + c as f64 * fi;
            let end = start + fi;
            let rss_values: Vec<usize> = samples
                .iter()
                .filter(|(t, _)| *t >= start && *t < end)
                .map(|(_, rss)| *rss)
                .collect();
            let peak = rss_values.iter().copied().max()?;
            let trough = rss_values.iter().copied().min()?;
            Some(CycleStats {
                peak_kb: peak,
                trough_kb: trough,
            })
        })
        .collect()
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[allow(clippy::unwrap_used)]
fn main() {
    let rt = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");

    let config = Arc::new(create_bench_config());
    let aggregator = Arc::new(tokio::sync::Mutex::new(TraceAggregator::default()));

    let mini_agent = MiniAgent {
        config: config.clone(),
        trace_processor: Arc::new(ServerlessTraceProcessor {}),
        trace_flusher: Arc::new(ServerlessTraceFlusher::new(aggregator, config.clone())),
        stats_processor: Arc::new(ServerlessStatsProcessor {}),
        stats_flusher: Arc::new(ServerlessStatsFlusher {}),
        env_verifier: Arc::new(NoopEnvVerifier),
        proxy_flusher: Arc::new(ProxyFlusher::new(config)),
    };

    rt.spawn(async move {
        let _ = mini_agent.start_mini_agent().await;
    });

    // Poll until the server is accepting connections.
    rt.block_on(async {
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if tokio::net::TcpStream::connect(format!("127.0.0.1:{BENCH_PORT}"))
                .await
                .is_ok()
            {
                break;
            }
        }
    });

    let bench_start = Instant::now();
    let start_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;

    let send_duration = Duration::from_secs(FLUSH_INTERVAL_SECS * (WARMUP_CYCLES + MEASURE_CYCLES));
    // After sending ends, wait for the final flush cycle to drain before
    // stopping the sampler. One extra flush interval plus a small buffer is
    // enough since flush + failed send completes almost immediately.
    let drain_wait = Duration::from_secs(FLUSH_INTERVAL_SECS + 2);

    let samples: Arc<Mutex<Vec<(f64, usize)>>> = Arc::new(Mutex::new(Vec::new()));

    rt.block_on(async {
        let (stop_tx, stop_rx) = oneshot::channel::<()>();

        let sampler_handle = tokio::spawn(run_sampler(bench_start, Arc::clone(&samples), stop_rx));

        run_sender(BENCH_PORT, start_ns, send_duration).await;

        tokio::time::sleep(drain_wait).await;

        let _ = stop_tx.send(());
        let _ = sampler_handle.await;
    });

    let samples = Arc::try_unwrap(samples).unwrap().into_inner().unwrap();
    let cycles = analyze(&samples, FLUSH_INTERVAL_SECS, WARMUP_CYCLES, MEASURE_CYCLES);

    let total_invocations =
        (TRACE_RATE_HZ * FLUSH_INTERVAL_SECS * (WARMUP_CYCLES + MEASURE_CYCLES)) as usize;
    let measured_invocations = (TRACE_RATE_HZ * FLUSH_INTERVAL_SECS * MEASURE_CYCLES) as usize;

    println!("── Configuration ────────────────────────────────");
    println!("  rate:               {} inv/s", TRACE_RATE_HZ);
    println!("  spans per inv:      {}", 1 + CHILD_SPANS_PER_INVOCATION);
    println!("  flush interval:     {}s", FLUSH_INTERVAL_SECS);
    println!("  warmup cycles:      {}", WARMUP_CYCLES);
    println!("  measure cycles:     {}", MEASURE_CYCLES);
    println!("  total invocations:  {}", total_invocations);
    println!("  measured inv:       {}", measured_invocations);
    println!();
    println!("── Per-cycle RSS ────────────────────────────────");

    let mut sum_peak = 0usize;
    let mut sum_trough = 0usize;

    for (i, cycle) in cycles.iter().enumerate() {
        println!(
            "  cycle {}: peak {:>8} KB   trough {:>8} KB   delta {:>8} KB",
            i + 1,
            cycle.peak_kb,
            cycle.trough_kb,
            cycle.peak_kb.saturating_sub(cycle.trough_kb),
        );
        sum_peak += cycle.peak_kb;
        sum_trough += cycle.trough_kb;
    }

    let n = cycles.len().max(1);
    let avg_peak = sum_peak / n;
    let avg_trough = sum_trough / n;
    let avg_delta = avg_peak.saturating_sub(avg_trough);

    println!();
    println!("── Steady-state averages ────────────────────────");
    println!("  avg peak RSS:       {} KB", avg_peak);
    println!("  avg trough RSS:     {} KB", avg_trough);
    println!("  avg cycle delta:    {} KB", avg_delta);
    println!();

    // Machine-readable line parsed by CI to feed benchmark-action.
    let json = serde_json::json!([{
        "name": "steady-state-peak-rss",
        "unit": "KB",
        "value": avg_peak,
    }]);
    println!("__BENCH_JSON__:{json}");
}
