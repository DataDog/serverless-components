// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Local test helper: inserts sample log entries and flushes them via the log agent pipeline.
//!
//! # Usage
//!
//! ## Flush to a local capture server (recommended for local dev)
//!
//! The easiest way — runs capture server and example together:
//!   ./scripts/test-log-intake.sh
//!
//! Or manually in two terminals:
//!
//! In terminal 1 — start the capture server (handles POST, prints JSON):
//!   python3 scripts/test-log-intake.sh   # not available standalone
//!   # Use the script above, or run: python3 -c "$(sed -n '/PYEOF/,/PYEOF/p' scripts/test-log-intake.sh)"
//!
//! In terminal 2 — run this example:
//!   DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED=true \
//!   DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL=http://localhost:9999/logs \
//!   DD_API_KEY=local-test-key \
//!   cargo run -p datadog-log-agent --example send_logs
//!
//! NOTE: `python3 -m http.server` does NOT work — it rejects POST requests.
//!
//! ## Flush to a real Datadog endpoint
//!
//!   DD_API_KEY=<your-key> \
//!   DD_SITE=datadoghq.com \
//!   cargo run -p datadog-log-agent --example send_logs
//!
//! ## Configuration via env vars
//!
//! | Variable                                         | Default            |
//! |--------------------------------------------------|--------------------|
//! | DD_API_KEY                                       | (empty)            |
//! | DD_SITE                                          | datadoghq.com      |
//! | DD_LOGS_CONFIG_USE_COMPRESSION                   | true               |
//! | DD_LOGS_CONFIG_COMPRESSION_LEVEL                 | 3                  |
//! | DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_ENABLED   | false              |
//! | DD_OBSERVABILITY_PIPELINES_WORKER_LOGS_URL       | (empty)            |
//! | LOG_ENTRY_COUNT                                  | 5                  |

use datadog_log_agent::{AggregatorService, FlusherMode, LogEntry, LogFlusher, LogFlusherConfig};

#[allow(clippy::disallowed_methods)] // plain reqwest::Client for local testing
#[tokio::main]
async fn main() {
    let entry_count: usize = std::env::var("LOG_ENTRY_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    let config = LogFlusherConfig::from_env();

    // Print effective configuration
    let (endpoint, compressed) = describe_config(&config);
    println!("──────────────────────────────────────────");
    println!("  datadog-log-agent local test");
    println!("──────────────────────────────────────────");
    println!("  endpoint    : {endpoint}");
    println!("  api_key     : {}", mask(&config.api_key));
    println!("  compressed  : {compressed}");
    println!("  entries     : {entry_count}");
    println!("──────────────────────────────────────────");

    // Start aggregator service
    let (service, handle) = AggregatorService::new();
    tokio::spawn(service.run());

    // Insert sample entries representing different runtimes
    let mut entries = Vec::with_capacity(entry_count);

    for i in 0..entry_count {
        let entry = match i % 3 {
            0 => lambda_entry(i),
            1 => azure_entry(i),
            _ => plain_entry(i),
        };
        entries.push(entry);
    }

    println!("\nInserting {entry_count} log entries...");
    handle.insert_batch(entries).expect("insert_batch failed");

    // Build HTTP client
    let client = reqwest::Client::builder()
        .timeout(config.flush_timeout)
        .build()
        .expect("failed to build HTTP client");

    // Flush
    println!("Flushing to {endpoint}...");
    let flusher = LogFlusher::new(config, client, handle);
    let ok = flusher.flush().await;

    if ok {
        println!("\n✓ Flush succeeded");
    } else {
        eprintln!("\n✗ Flush failed — check endpoint and API key");
        std::process::exit(1);
    }
}

// ── Sample log entry builders ─────────────────────────────────────────────────

fn lambda_entry(i: usize) -> LogEntry {
    let mut attrs = serde_json::Map::new();
    attrs.insert(
        "lambda".to_string(),
        serde_json::json!({
            "arn": "arn:aws:lambda:us-east-1:123456789012:function:my-fn",
            "request_id": format!("req-{i:04}")
        }),
    );
    LogEntry {
        message: format!("[lambda] invocation #{i} completed"),
        timestamp: now_ms(),
        hostname: Some("arn:aws:lambda:us-east-1:123456789012:function:my-fn".to_string()),
        service: Some("my-fn".to_string()),
        ddsource: Some("lambda".to_string()),
        ddtags: Some("env:local,runtime:lambda".to_string()),
        status: Some("info".to_string()),
        attributes: attrs,
    }
}

fn azure_entry(i: usize) -> LogEntry {
    let mut attrs = serde_json::Map::new();
    attrs.insert(
        "azure".to_string(),
        serde_json::json!({
            "resource_id": "/subscriptions/sub-123/resourceGroups/rg/providers/Microsoft.Web/sites/my-fn",
            "operation_name": "Microsoft.Web/sites/functions/run/action"
        }),
    );
    LogEntry {
        message: format!("[azure] function triggered #{i}"),
        timestamp: now_ms(),
        hostname: Some("my-azure-fn".to_string()),
        service: Some("payments".to_string()),
        ddsource: Some("azure-functions".to_string()),
        ddtags: Some("env:local,runtime:azure".to_string()),
        status: Some("info".to_string()),
        attributes: attrs,
    }
}

fn plain_entry(i: usize) -> LogEntry {
    LogEntry {
        message: format!("[generic] log message #{i}"),
        timestamp: now_ms(),
        hostname: Some("localhost".to_string()),
        service: Some("test-service".to_string()),
        ddsource: Some("rust".to_string()),
        ddtags: Some("env:local".to_string()),
        status: if i % 5 == 0 {
            Some("error".to_string())
        } else {
            Some("info".to_string())
        },
        attributes: serde_json::Map::new(),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn describe_config(config: &LogFlusherConfig) -> (String, bool) {
    match &config.mode {
        FlusherMode::Datadog => (
            format!("https://http-intake.logs.{}/api/v2/logs", config.site),
            config.use_compression,
        ),
        FlusherMode::ObservabilityPipelinesWorker { url } => (url.clone(), false),
    }
}

fn mask(s: &str) -> String {
    if s.is_empty() {
        return "(not set)".to_string();
    }
    if s.len() <= 8 {
        return "*".repeat(s.len());
    }
    format!("{}…{}", &s[..4], &s[s.len() - 4..])
}
