# Instance Enhanced Metric Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an `azure.functions.enhanced.instance` metric that reports the Azure Functions instance identity, enabling per-instance observability.

**Architecture:** Create the `datadog-metrics-collector` crate (ported from the CPU metrics branch scaffolding, minus CPU-specific code). The crate exposes an `InstanceMetricsCollector` that reads instance identity from env vars (`WEBSITE_INSTANCE_ID`, `WEBSITE_POD_NAME`, `CONTAINER_NAME`) and submits a **gauge** metric with value `1.0` on each collection tick, with the instance ID as an `instance_id` tag. This follows the datadog-agent pattern (PR 47421) where usage/instance metrics are gauges (not distributions) because the instance tag already provides a unique identifier, avoiding aggregation issues. The collector is wired into `datadog-serverless-compat`'s main loop via a `tokio::select!` arm, sharing the existing DogStatsD aggregator. Origin classification in `dogstatsd/src/origin.rs` needs the `azure.functions` prefix added to route instance metrics as `ServerlessEnhanced`.

**Tech Stack:** Rust, tokio, dogstatsd crate (local), libdd-common (libdatadog)

---

### Task 1: Create `datadog-metrics-collector` crate with shared tag builder

**Files:**
- Create: `crates/datadog-metrics-collector/Cargo.toml`
- Create: `crates/datadog-metrics-collector/src/lib.rs`

This task creates the crate shell. No metric logic yet. The workspace `Cargo.toml` uses `crates/*` glob so no workspace edit is needed.

- [ ] **Step 1: Create `Cargo.toml`**

```toml
[package]
name = "datadog-metrics-collector"
version = "0.1.0"
edition.workspace = true
license.workspace = true
description = "Collector to read, compute, and submit enhanced metrics in Serverless environments"

[dependencies]
dogstatsd = { path = "../dogstatsd", default-features = true }
tracing = { version = "0.1", default-features = false }
libdd-common = { git = "https://github.com/DataDog/libdatadog", rev = "d52ee90209cb12a28bdda0114535c1a985a29d95", default-features = false }

[features]
windows-enhanced-metrics = []
```

Note: `num_cpus` is intentionally omitted — it's only needed for CPU metrics, not instance metrics.

- [ ] **Step 2: Create `lib.rs`**

```rust
// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(not(test), deny(clippy::panic))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![cfg_attr(not(test), deny(clippy::todo))]
#![cfg_attr(not(test), deny(clippy::unimplemented))]

pub mod instance;
pub mod tags;
```

- [ ] **Step 3: Verify the crate compiles**

Run: `cargo check -p datadog-metrics-collector`
Expected: success (will fail until Task 2 and 3 create the modules)

Note: This step will be verified after Tasks 2 and 3 are done.

- [ ] **Step 4: Commit**

```bash
git add crates/datadog-metrics-collector/Cargo.toml crates/datadog-metrics-collector/src/lib.rs
git commit -m "feat(metrics-collector): create datadog-metrics-collector crate shell"
```

---

### Task 2: Extract shared tag builder into `tags.rs`

**Files:**
- Create: `crates/datadog-metrics-collector/src/tags.rs`

The tag builder is lifted from `cpu.rs` on the CPU branch. It's shared infrastructure for all enhanced metrics (instance, CPU, memory, etc.), so it lives in its own module.

- [ ] **Step 1: Create `tags.rs`**

```rust
// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Shared tag builder for enhanced metrics.
//!
//! Tags are attached to all enhanced metrics submitted by the metrics collector.

use dogstatsd::metric::SortedTags;
use libdd_common::azure_app_services;
use std::env;

/// Builds the common tags for all enhanced metrics.
///
/// Sources:
/// - Azure metadata (resource_group, subscription_id, name) from libdd_common
/// - Environment variables (region, plan_tier, service, env, version, serverless_compat_version)
///
/// The DogStatsD origin tag (e.g. `origin:azurefunction`) is added by the aggregator,
/// not here.
pub fn build_enhanced_metrics_tags() -> Option<SortedTags> {
    let mut tag_parts = Vec::new();

    if let Some(aas_metadata) = &*azure_app_services::AAS_METADATA_FUNCTION {
        let aas_tags = [
            ("resource_group", aas_metadata.get_resource_group()),
            ("subscription_id", aas_metadata.get_subscription_id()),
            ("name", aas_metadata.get_site_name()),
        ];
        for (name, value) in aas_tags {
            if value != "unknown" {
                tag_parts.push(format!("{}:{}", name, value));
            }
        }
    }

    for (tag_name, env_var) in [
        ("region", "REGION_NAME"),
        ("plan_tier", "WEBSITE_SKU"),
        ("service", "DD_SERVICE"),
        ("env", "DD_ENV"),
        ("version", "DD_VERSION"),
        ("serverless_compat_version", "DD_SERVERLESS_COMPAT_VERSION"),
    ] {
        if let Ok(val) = env::var(env_var)
            && !val.is_empty()
        {
            tag_parts.push(format!("{}:{}", tag_name, val));
        }
    }

    if tag_parts.is_empty() {
        return None;
    }
    SortedTags::parse(&tag_parts.join(",")).ok()
}
```

- [ ] **Step 2: Commit**

```bash
git add crates/datadog-metrics-collector/src/tags.rs
git commit -m "feat(metrics-collector): add shared tag builder for enhanced metrics"
```

---

### Task 3: Implement `InstanceMetricsCollector`

**Files:**
- Create: `crates/datadog-metrics-collector/src/instance.rs`

The instance metric is simple: read the instance ID from env vars, submit `azure.functions.enhanced.instance` as a **gauge** with value `1.0` and an `instance_id` tag. Following the datadog-agent pattern (PR 47421), usage/instance metrics use gauges because the instance tag provides a unique identifier — no aggregation issues like CPU metrics have. No delta computation, no OS-specific reader.

- [ ] **Step 1: Write failing test for `resolve_instance_id`**

Create `crates/datadog-metrics-collector/src/instance.rs` with the test first:

```rust
// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Instance identity metric collector for Azure Functions.
//!
//! Submits `azure.functions.enhanced.instance` with value 1.0 on each
//! collection tick, tagged with the instance identifier. The env var
//! checked depends on the Azure plan type:
//!
//! - Elastic Premium / Premium: `WEBSITE_INSTANCE_ID`
//! - Flex Consumption / Consumption: `WEBSITE_POD_NAME` or `CONTAINER_NAME`

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_instance_id_returns_none_when_no_env_vars() {
        // Ensure none of the vars are set (they shouldn't be in test env)
        let id = resolve_instance_id_from(None, None, None);
        assert!(id.is_none());
    }

    #[test]
    fn test_resolve_instance_id_prefers_website_instance_id() {
        let id = resolve_instance_id_from(
            Some("instance-abc"),
            Some("pod-xyz"),
            Some("container-123"),
        );
        assert_eq!(id, Some("instance-abc".to_string()));
    }

    #[test]
    fn test_resolve_instance_id_falls_back_to_pod_name() {
        let id = resolve_instance_id_from(None, Some("pod-xyz"), Some("container-123"));
        assert_eq!(id, Some("pod-xyz".to_string()));
    }

    #[test]
    fn test_resolve_instance_id_falls_back_to_container_name() {
        let id = resolve_instance_id_from(None, None, Some("container-123"));
        assert_eq!(id, Some("container-123".to_string()));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p datadog-metrics-collector -- test_resolve_instance_id 2>&1`
Expected: FAIL — `resolve_instance_id_from` not found

- [ ] **Step 3: Implement `resolve_instance_id_from` and `resolve_instance_id`**

Add above the `#[cfg(test)]` block:

```rust
use dogstatsd::aggregator::AggregatorHandle;
use dogstatsd::metric::{Metric, MetricValue, SortedTags};
use std::env;
use tracing::{debug, error, info};

const INSTANCE_METRIC: &str = "azure.functions.enhanced.instance";

/// Resolves the instance ID from explicit values (used by tests).
fn resolve_instance_id_from(
    website_instance_id: Option<&str>,
    website_pod_name: Option<&str>,
    container_name: Option<&str>,
) -> Option<String> {
    website_instance_id
        .or(website_pod_name)
        .or(container_name)
        .map(String::from)
}

/// Resolves the instance ID from environment variables.
///
/// Checks in order:
/// 1. `WEBSITE_INSTANCE_ID` (Elastic Premium / Premium plans)
/// 2. `WEBSITE_POD_NAME` (Flex Consumption plans)
/// 3. `CONTAINER_NAME` (Consumption plans)
fn resolve_instance_id() -> Option<String> {
    resolve_instance_id_from(
        env::var("WEBSITE_INSTANCE_ID").ok().as_deref(),
        env::var("WEBSITE_POD_NAME").ok().as_deref(),
        env::var("CONTAINER_NAME").ok().as_deref(),
    )
}

pub struct InstanceMetricsCollector {
    aggregator: AggregatorHandle,
    tags: Option<SortedTags>,
    instance_id: Option<String>,
}

impl InstanceMetricsCollector {
    pub fn new(aggregator: AggregatorHandle, tags: Option<SortedTags>) -> Self {
        let instance_id = resolve_instance_id();
        if let Some(ref id) = instance_id {
            info!("Instance ID resolved: {}", id);
        } else {
            debug!("No instance ID found, instance metric will not be submitted");
        }
        Self {
            aggregator,
            tags,
            instance_id,
        }
    }

    pub fn collect_and_submit(&self) {
        let Some(ref instance_id) = self.instance_id else {
            debug!("No instance ID available, skipping instance metric");
            return;
        };

        // Build tags: start with shared tags, add instance_id
        let instance_tag = format!("instance_id:{}", instance_id);
        let tag_string = match &self.tags {
            Some(existing) => format!("{},{}", existing, instance_tag),
            None => instance_tag,
        };
        let tags = SortedTags::parse(&tag_string).ok();

        let now = std::time::UNIX_EPOCH
            .elapsed()
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .try_into()
            .unwrap_or(0);

        let metric = Metric::new(
            INSTANCE_METRIC.into(),
            MetricValue::gauge(1.0),
            tags,
            Some(now),
        );

        if let Err(e) = self.aggregator.insert_batch(vec![metric]) {
            error!("Failed to insert instance metric: {}", e);
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p datadog-metrics-collector -- test_resolve_instance_id`
Expected: all 4 tests PASS

- [ ] **Step 5: Verify crate compiles**

Run: `cargo check -p datadog-metrics-collector`
Expected: success

- [ ] **Step 6: Commit**

```bash
git add crates/datadog-metrics-collector/src/instance.rs
git commit -m "feat(metrics-collector): add instance identity metric collector"
```

---

### Task 4: Add `azure.functions` prefix to origin classification

**Files:**
- Modify: `crates/dogstatsd/src/origin.rs`

The current `main` branch doesn't include `azure.functions` in the enhanced-service prefix check. The CPU branch added it. We need it so `azure.functions.enhanced.instance` gets classified as `ServerlessEnhanced`.

- [ ] **Step 1: Write failing test for the new origin classification**

Add this test to the `mod tests` block at the bottom of `crates/dogstatsd/src/origin.rs`:

```rust
    #[test]
    fn test_find_metric_origin_azure_functions_enhanced() {
        let tags = SortedTags::parse("origin:azurefunction").unwrap();
        let metric = Metric {
            id: 0,
            name: "azure.functions.enhanced.instance".into(),
            value: MetricValue::Gauge(1.0),
            tags: Some(tags.clone()),
            timestamp: 0,
        };
        let origin = metric.find_origin(tags).unwrap();
        assert_eq!(
            origin.origin_product as u32,
            OriginProduct::Serverless as u32
        );
        assert_eq!(
            origin.origin_category as u32,
            OriginCategory::AzureFunctionsMetrics as u32
        );
        assert_eq!(
            origin.origin_service as u32,
            OriginService::ServerlessEnhanced as u32
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p dogstatsd -- test_find_metric_origin_azure_functions_enhanced`
Expected: FAIL — `azure.functions` prefix not matched for `ServerlessEnhanced`, falls through to `ServerlessCustom`

- [ ] **Step 3: Add `azure.functions` prefix constant and update matching**

In `crates/dogstatsd/src/origin.rs`, add the constant:

```rust
const AZURE_FUNCTIONS_PREFIX: &str = "azure.functions";
```

And update the service matching in `find_origin` to include it:

```rust
        } else if metric_prefix == AWS_LAMBDA_PREFIX
            || metric_prefix == GOOGLE_CLOUD_RUN_PREFIX
            || metric_prefix == AZURE_FUNCTIONS_PREFIX
        {
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p dogstatsd -- test_find_metric_origin_azure_functions_enhanced`
Expected: PASS

- [ ] **Step 5: Run all dogstatsd tests to check for regressions**

Run: `cargo test -p dogstatsd`
Expected: all tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/dogstatsd/src/origin.rs
git commit -m "feat(dogstatsd): classify azure.functions prefix as ServerlessEnhanced origin"
```

---

### Task 5: Wire `InstanceMetricsCollector` into `main.rs`

**Files:**
- Modify: `crates/datadog-serverless-compat/Cargo.toml`
- Modify: `crates/datadog-serverless-compat/src/main.rs`

This is the integration task. The main changes to `main.rs`:
1. Add `DD_ENHANCED_METRICS_ENABLED` env var check (Azure Functions only, default true, disabled on Windows)
2. Refactor aggregator creation to be shared between dogstatsd and enhanced metrics (same pattern as CPU branch)
3. Add a `tokio::select!` loop with separate flush and collection intervals
4. Create and run the `InstanceMetricsCollector`

- [ ] **Step 1: Add `datadog-metrics-collector` dependency to `Cargo.toml`**

Add to `[dependencies]` in `crates/datadog-serverless-compat/Cargo.toml`:

```toml
datadog-metrics-collector = { path = "../datadog-metrics-collector" }
```

Add to `[features]`:

```toml
windows-enhanced-metrics = ["datadog-metrics-collector/windows-enhanced-metrics"]
```

- [ ] **Step 2: Update `main.rs` — add import and collection interval constant**

Add import near the top with other use statements:

```rust
use datadog_metrics_collector::instance::InstanceMetricsCollector;
```

Add constant:

```rust
const ENHANCED_METRICS_COLLECTION_INTERVAL_SECS: u64 = 10;
```

- [ ] **Step 3: Update `main.rs` — add `dd_enhanced_metrics` env var check**

After the `dd_logs_enabled` / `dd_logs_port` env var reads (around line 121), add:

```rust
    // Only enable enhanced metrics for Linux Azure Functions
    #[cfg(not(feature = "windows-enhanced-metrics"))]
    let dd_enhanced_metrics = env_type == EnvironmentType::AzureFunction
        && env::var("DD_ENHANCED_METRICS_ENABLED")
            .map(|val| val.to_lowercase() != "false")
            .unwrap_or(true);

    // Enhanced metrics are not yet supported in Windows environments
    #[cfg(feature = "windows-enhanced-metrics")]
    let dd_enhanced_metrics = false;
```

- [ ] **Step 4: Update `main.rs` — refactor aggregator to be shared**

Replace the dogstatsd startup block (lines 185-207) and the code down through the flush loop with the shared-aggregator pattern from the CPU branch. The key structural change is:

1. Create aggregator when `dd_use_dogstatsd || dd_enhanced_metrics`
2. Start DogStatsD listener separately (only if `dd_use_dogstatsd`)
3. Create `InstanceMetricsCollector` when enhanced metrics enabled
4. Use `tokio::select!` with both flush and collection intervals

Replace lines 185-207 (the dogstatsd startup block) with:

```rust
    let needs_aggregator = dd_use_dogstatsd || dd_enhanced_metrics;

    // The aggregator is shared between dogstatsd and enhanced metrics.
    // It is started independently so that either can be enabled without the other.
    let (metrics_flusher, aggregator_handle) = if needs_aggregator {
        debug!("Creating metrics flusher and aggregator");

        let (flusher, handle) =
            start_aggregator(dd_api_key.clone(), dd_site, https_proxy.clone(), dogstatsd_tags).await;

        if dd_use_dogstatsd {
            debug!("Starting dogstatsd");
            let _ = start_dogstatsd_listener(
                dd_dogstatsd_port,
                handle.clone(),
                dd_statsd_metric_namespace,
                #[cfg(all(windows, feature = "windows-pipes"))]
                dd_dogstatsd_windows_pipe_name.clone(),
            )
            .await;
            if let Some(ref windows_pipe_name) = dd_dogstatsd_windows_pipe_name {
                info!("dogstatsd-pipe: starting to listen on pipe {windows_pipe_name}");
            } else {
                info!("dogstatsd-udp: starting to listen on port {dd_dogstatsd_port}");
            }
        } else {
            info!("dogstatsd disabled");
        }
        (flusher, Some(handle))
    } else {
        info!("dogstatsd and enhanced metrics disabled");
        (None, None)
    };
```

- [ ] **Step 5: Update `main.rs` — create instance collector**

After the aggregator block, add:

```rust
    let instance_collector = if dd_enhanced_metrics && metrics_flusher.is_some() {
        aggregator_handle.as_ref().map(|handle| {
            let tags = datadog_metrics_collector::tags::build_enhanced_metrics_tags();
            InstanceMetricsCollector::new(handle.clone(), tags)
        })
    } else {
        if !dd_enhanced_metrics {
            info!("Enhanced metrics disabled");
        } else {
            info!("Enhanced metrics enabled but metrics flusher not found");
        }
        None
    };
```

- [ ] **Step 6: Update `main.rs` — replace flush loop with `tokio::select!`**

Replace the existing flush loop (from `let mut flush_interval` through end of `loop`) with:

```rust
    let mut flush_interval = interval(Duration::from_secs(DOGSTATSD_FLUSH_INTERVAL));
    let mut enhanced_metrics_collection_interval =
        interval(Duration::from_secs(ENHANCED_METRICS_COLLECTION_INTERVAL_SECS));
    flush_interval.tick().await; // discard first tick, which is instantaneous
    enhanced_metrics_collection_interval.tick().await;

    // Builders for log batches that failed transiently in the previous flush
    // cycle. They are redriven on the next cycle before new batches are sent.
    let mut pending_log_retries: Vec<reqwest::RequestBuilder> = Vec::new();

    loop {
        tokio::select! {
            _ = flush_interval.tick() => {
                if let Some(metrics_flusher) = metrics_flusher.clone() {
                    debug!("Flushing dogstatsd metrics");
                    tokio::spawn(async move {
                        metrics_flusher.flush().await;
                    });
                }

                if let Some(log_flusher) = log_flusher.as_ref() {
                    debug!("Flushing log agent");
                    let retry_in = std::mem::take(&mut pending_log_retries);
                    let failed = log_flusher.flush(retry_in).await;
                    if !failed.is_empty() {
                        warn!(
                            "log agent flush failed for {} batch(es); will retry next cycle",
                            failed.len()
                        );
                        pending_log_retries = failed;
                    }
                }
            }
            _ = enhanced_metrics_collection_interval.tick() => {
                if let Some(ref collector) = instance_collector {
                    collector.collect_and_submit();
                }
            }
        }
    }
```

- [ ] **Step 7: Update `main.rs` — refactor `start_dogstatsd` into `start_aggregator` + `start_dogstatsd_listener`**

Replace the existing `start_dogstatsd` function with two functions:

```rust
async fn start_aggregator(
    dd_api_key: Option<String>,
    dd_site: String,
    https_proxy: Option<String>,
    dogstatsd_tags: &str,
) -> (Option<Flusher>, AggregatorHandle) {
    #[allow(clippy::expect_used)]
    let (service, handle) = AggregatorService::new(
        SortedTags::parse(dogstatsd_tags).unwrap_or(EMPTY_TAGS),
        CONTEXTS,
    )
    .expect("Failed to create aggregator service");

    tokio::spawn(service.run());

    let metrics_flusher = match dd_api_key {
        Some(dd_api_key) => {
            let client = match build_metrics_client(https_proxy, DOGSTATSD_TIMEOUT_DURATION) {
                Ok(client) => client,
                Err(e) => {
                    error!("Failed to build HTTP client: {e}, won't flush metrics");
                    return (None, handle);
                }
            };
            let metrics_intake_url_prefix = match Site::new(dd_site)
                .map_err(|e| e.to_string())
                .and_then(|site| {
                    MetricsIntakeUrlPrefix::new(Some(site), None).map_err(|e| e.to_string())
                }) {
                Ok(prefix) => prefix,
                Err(e) => {
                    error!("Failed to create metrics intake URL: {e}, won't flush metrics");
                    return (None, handle);
                }
            };

            let metrics_flusher = Flusher::new(FlusherConfig {
                api_key_factory: Arc::new(ApiKeyFactory::new(&dd_api_key)),
                aggregator_handle: handle.clone(),
                metrics_intake_url_prefix,
                client,
                retry_strategy: RetryStrategy::LinearBackoff(3, 1),
                compression_level: CompressionLevel::try_from(6).unwrap_or_default(),
            });
            Some(metrics_flusher)
        }
        None => {
            error!("DD_API_KEY not set, won't flush metrics");
            None
        }
    };

    (metrics_flusher, handle)
}

async fn start_dogstatsd_listener(
    port: u16,
    handle: AggregatorHandle,
    metric_namespace: Option<String>,
    #[cfg(all(windows, feature = "windows-pipes"))] windows_pipe_name: Option<String>,
) -> CancellationToken {
    #[cfg(all(windows, feature = "windows-pipes"))]
    let dogstatsd_config = DogStatsDConfig {
        host: AGENT_HOST.to_string(),
        port,
        metric_namespace,
        windows_pipe_name,
        so_rcvbuf: None,
        buffer_size: None,
        queue_size: None,
    };

    #[cfg(not(all(windows, feature = "windows-pipes")))]
    let dogstatsd_config = DogStatsDConfig {
        host: AGENT_HOST.to_string(),
        port,
        metric_namespace,
        so_rcvbuf: None,
        buffer_size: None,
        queue_size: None,
    };
    let dogstatsd_cancel_token = tokio_util::sync::CancellationToken::new();

    let dogstatsd_client = DogStatsD::new(
        &dogstatsd_config,
        handle.clone(),
        dogstatsd_cancel_token.clone(),
    )
    .await;

    tokio::spawn(async move {
        dogstatsd_client.spin().await;
    });

    dogstatsd_cancel_token
}
```

- [ ] **Step 8: Remove unused import `warn` if no longer needed, clean up**

Check if `warn` is still used (it is, for log flush failures). Remove the `_aggregator_handle` underscore-prefixed variable since it's now used.

- [ ] **Step 9: Verify everything compiles**

Run: `cargo check -p datadog-serverless-compat`
Expected: success

- [ ] **Step 10: Run all workspace tests**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 11: Commit**

```bash
git add crates/datadog-serverless-compat/Cargo.toml crates/datadog-serverless-compat/src/main.rs
git commit -m "feat(serverless-compat): wire instance metrics collector into main loop"
```

---

### Task 6: Update CI workflows for `windows-enhanced-metrics` feature

**Files:**
- Modify: `.github/workflows/build-datadog-serverless-compat.yml`
- Modify: `.github/workflows/cargo.yml`

Windows builds need the `windows-enhanced-metrics` feature flag so that the feature-gated code compiles correctly.

- [ ] **Step 1: Update `build-datadog-serverless-compat.yml`**

Change the Windows build command from:

```yaml
run: cargo build --release -p datadog-serverless-compat --features windows-pipes
```

to:

```yaml
run: cargo build --release -p datadog-serverless-compat --features windows-pipes,windows-enhanced-metrics
```

- [ ] **Step 2: Update `cargo.yml`**

Change the Windows test command from:

```yaml
cargo nextest run --workspace --features datadog-serverless-compat/windows-pipes
```

to:

```yaml
cargo nextest run --workspace --features datadog-serverless-compat/windows-pipes,datadog-serverless-compat/windows-enhanced-metrics
```

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/build-datadog-serverless-compat.yml .github/workflows/cargo.yml
git commit -m "ci: add windows-enhanced-metrics feature flag to CI builds"
```

---

### Task 7: Update `Cargo.lock` and `LICENSE-3rdparty.csv`

**Files:**
- Modify: `Cargo.lock` (auto-generated)
- Modify: `LICENSE-3rdparty.csv` (if any new third-party deps)

- [ ] **Step 1: Generate lock file**

Run: `cargo generate-lockfile` or just `cargo check` to update `Cargo.lock`

- [ ] **Step 2: Check if LICENSE-3rdparty.csv needs updating**

Since we're only adding local crate dependencies (dogstatsd, libdd-common which are already in the dependency tree), this likely needs no changes. Verify by checking if the CI has a license check step and whether it passes.

- [ ] **Step 3: Commit if changed**

```bash
git add Cargo.lock
git commit -m "chore: update Cargo.lock for datadog-metrics-collector"
```

---

### Task 8: Final verification

- [ ] **Step 1: Run full workspace check**

Run: `cargo check --workspace`
Expected: success

- [ ] **Step 2: Run full workspace tests**

Run: `cargo test --workspace`
Expected: all tests pass

- [ ] **Step 3: Run clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: no warnings

- [ ] **Step 4: Verify Windows feature flag compiles**

Run: `cargo check -p datadog-serverless-compat --features windows-enhanced-metrics,windows-pipes`
Expected: success (will use stub code paths)
