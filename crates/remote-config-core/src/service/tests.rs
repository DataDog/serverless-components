//! Integration-style tests covering the Remote Config service surface.
//!
//! The suite mirrors the Go agent coverage: cache-bypass behaviour, Uptane
//! interactions, CDN throttling, websocket checks, and credential rotation.
//! Keeping these tests exhaustive ensures the Rust port remains compatible
//! with backend expectations.

#![cfg(test)]

use super::*;
use crate::http::{BackoffConfig, HttpError};
use crate::rc_key::RcKey;
use crate::service::client::{
    build_cached_index, config_is_expired, parse_custom_metadata, parse_semver_version,
    predicate_matches,
};
use crate::service::config::{
    ServiceConfig, DEFAULT_CACHE_BYPASS_LIMIT, DEFAULT_CLIENTS_TTL, DEFAULT_REFRESH_INTERVAL,
    MAX_BACKOFF_INTERVAL, MIN_BACKOFF_INTERVAL, MIN_REFRESH_INTERVAL,
};
use crate::service::refresh::{extract_refresh_override, ServiceShared};
use crate::service::state::{
    classify_error_kind, CacheBypassTracker, OrgStatusSnapshot, RefreshErrorLevel, RuntimeMetadata,
    ServiceState, WebsocketCheckResult, MAX_FETCH_503_LOG_LEVEL,
    MAX_ORG_STATUS_FETCH_503_LOG_LEVEL,
};
use crate::service::telemetry::{NoopTelemetry, RemoteConfigTelemetry};
use crate::service::test_support::*;
use crate::service::util::{canonicalize_json_bytes, compute_sha256};
use crate::store::StoreError;
use crate::telemetry::CountingTelemetry;
use crate::uptane::{TufVersions, UptaneError};
use crate::uptane_path::{
    classify_config_path, parse_config_path, parse_datadog_config_path, parse_employee_config_path,
    ConfigPathSource,
};
use data_encoding::BASE32_NOPAD;
use futures_util::{SinkExt, StreamExt};
use httptest::matchers::{all_of, contains, not, request};
use httptest::{cycle, responders::status_code, Expectation, Server};
use prost::Message;
use remote_config_proto::remoteconfig::{
    ClientAgent, ClientGetConfigsRequest, ClientGetConfigsResponse, ConfigMetas, ConfigStatus,
    DirectorMetas, File, LatestConfigsResponse, OrgDataResponse, OrgStatusResponse, TargetFileHash,
    TargetFileMeta, TracerPredicateV1,
};
use serde::Serialize;
use serde_json::json;
use serde_json::Value;
use std::io::{Error as IoError, ErrorKind as IoErrorKind};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::runtime::Runtime;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::{accept_async, connect_async, tungstenite::Message as WsMessage};

/// Sends a signal when dropped so tests can detect task cancellation.
struct AuxDropNotifier(Option<oneshot::Sender<()>>);

impl Drop for AuxDropNotifier {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

#[tokio::test]
/// Verifies auxiliary background tasks registered on the service are aborted explicitly.
async fn auxiliary_tasks_abort_when_requested() {
    let server = Server::run();
    let service = build_service(&server);
    let (tx, rx) = oneshot::channel();
    let guard = AuxDropNotifier(Some(tx));
    let handle = tokio::spawn(async move {
        let _guard = guard;
        futures_util::future::pending::<()>().await;
    });
    service.register_aux_task(handle).await;
    service.abort_aux_tasks().await;
    assert!(rx.await.is_ok(), "auxiliary task should drop after abort");
}

/// Encodes a fake RC key for use in org validation tests.
fn encoded_rc_key(app_key: &str, org_id: u64) -> String {
    #[derive(Serialize)]
    struct RcKeyWire<'a> {
        #[serde(rename = "key")]
        app_key: &'a str,
        #[serde(rename = "org")]
        org_id: u64,
        #[serde(rename = "dc")]
        datacenter: &'a str,
    }
    let payload = RcKeyWire {
        app_key,
        org_id,
        datacenter: "datadoghq.com",
    };
    let bytes = rmp_serde::to_vec(&payload).expect("rc key serialization");
    format!("DDRCM_{}", BASE32_NOPAD.encode(&bytes))
}

/// Returns a clone of the shared service internals for test assertions.
fn shared(service: &RemoteConfigService) -> Arc<ServiceShared> {
    service.shared_for_tests().clone()
}

#[derive(Clone, Default)]
struct TimingTelemetry {
    events: Arc<Mutex<Vec<(Instant, &'static str)>>>,
}

impl TimingTelemetry {
    fn events(&self) -> Arc<Mutex<Vec<(Instant, &'static str)>>> {
        self.events.clone()
    }
}

impl RemoteConfigTelemetry for TimingTelemetry {
    fn on_refresh_success(&self, _next_interval: Duration) {
        self.events
            .lock()
            .unwrap()
            .push((Instant::now(), "success"));
    }

    fn on_refresh_error(&self, _error: &ServiceError) {
        self.events.lock().unwrap().push((Instant::now(), "error"));
    }
}

/// Ensures the RemoteConfigHandle shuts down cleanly even when tasks fail.
#[tokio::test]
async fn shutdown_handles_aborted_tasks() {
    let server = Server::run();
    let service = build_service(&server);
    let (tx, _) = broadcast::channel(1);
    let handle = tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    handle.abort();
    let managed = RemoteConfigHandle::from_parts(tx, vec![handle], service);
    managed.shutdown().await;
}

/// Exercises the default telemetry hooks to ensure they remain no-ops.
#[test]
fn noop_telemetry_methods_are_stable() {
    let telemetry = NoopTelemetry;
    telemetry.on_refresh_success(Duration::from_secs(1));
    telemetry.on_refresh_error(&ServiceError::InvalidRequest("error".into()));
    telemetry.on_bypass_timeout();
    telemetry.on_bypass_rejected();
    telemetry.on_bypass_enqueued();
    telemetry.on_websocket_check(WebsocketCheckResult::Success);
    telemetry.on_org_status_change(
        None,
        OrgStatusSnapshot {
            enabled: true,
            authorized: true,
            fetched_at: tokio::time::Instant::now(),
        },
    );
}

/// Validates that cache-bypass failures propagate telemetry when the receiver is dropped.
#[tokio::test]
async fn client_get_configs_records_failed_bypass_send() {
    let server = Server::run();
    let mut config = base_config();
    config.disable_background_poller = true;
    config.enable_websocket_echo = false;
    let service = build_service_with_config(&server, config);
    let telemetry = Arc::new(RecordingTelemetry::default());
    service.set_telemetry(telemetry.clone()).await;

    let response = sample_response(1, 1);
    let shared = shared(&service);
    shared.uptane.update(&response).unwrap();

    let (bypass_tx, bypass_rx) = mpsc::channel(1);
    drop(bypass_rx);
    {
        let mut guard = shared.cache_bypass_tx.lock().await;
        *guard = Some(bypass_tx);
    }

    let request = ClientGetConfigsRequest {
        client: Some(tracer_client("bypass-client")),
        cached_target_files: Vec::new(),
    };

    let result = service.client_get_configs(request).await;
    assert!(
        result.is_ok(),
        "client_get_configs failed: {:?}",
        result.err()
    );
    assert_eq!(
        telemetry.bypass_rejected.load(Ordering::Relaxed),
        1,
        "bypass rejection telemetry not recorded"
    );
}

/// Returns the bypass slot when enqueueing fails before the poller observes the signal.
#[tokio::test]
async fn bypass_slot_refunded_when_enqueue_fails() {
    let server = Server::run();
    let service = build_service(&server);

    let shared = shared(&service);
    let initial_allowance = {
        let guard = shared.state.lock().await;
        guard.cache_bypass_remaining()
    };
    {
        let mut guard = shared.cache_bypass_tx.lock().await;
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        *guard = Some(tx);
    }

    let request = ClientGetConfigsRequest {
        client: Some(tracer_client("enqueue-refund")),
        cached_target_files: Vec::new(),
    };
    let _ = service.client_get_configs(request).await;

    let remaining = {
        let guard = shared.state.lock().await;
        guard.cache_bypass_remaining()
    };
    assert_eq!(
        remaining, initial_allowance,
        "enqueue failures should refund the bypass allowance"
    );
}

/// Returns the bypass slot when manual refresh attempts cannot complete.
#[tokio::test]
async fn bypass_slot_refunded_when_manual_refresh_fails() {
    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("POST", "/api/v0.1/configurations"))
            .times(1)
            .respond_with(status_code(503)),
    );
    let service = build_service(&server);
    let shared = shared(&service);
    let initial_allowance = {
        let guard = shared.state.lock().await;
        guard.cache_bypass_remaining()
    };
    let request = ClientGetConfigsRequest {
        client: Some(tracer_client("manual-refund")),
        cached_target_files: Vec::new(),
    };
    let _ = service.client_get_configs(request).await;

    let remaining = {
        let guard = shared.state.lock().await;
        guard.cache_bypass_remaining()
    };
    assert_eq!(
        remaining, initial_allowance,
        "manual refresh failures should refund the bypass allowance"
    );
}

/// Verifies the background poller processes bypass signals and reports telemetry.
#[tokio::test(flavor = "multi_thread")]
async fn config_poller_processes_bypass_signal() {
    let server = Server::run();
    let response = sample_response(5, 5);
    server.expect(
        Expectation::matching(request::method_path("POST", "/api/v0.1/configurations"))
            .times(1)
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );
    let mut config = base_config();
    config.disable_background_poller = false;
    config.enable_websocket_echo = false;
    config.default_refresh_interval = Duration::from_millis(50);
    config.min_refresh_interval = Duration::from_millis(10);
    config.org_status_interval = Duration::from_secs(0);
    config.bypass_block_timeout = Duration::from_millis(500);
    config.new_client_block_timeout = Duration::from_millis(500);

    let service = build_service_with_config(&server, config);
    let telemetry = CountingTelemetry::default();
    let counters = telemetry.counters();
    service.set_telemetry(Arc::new(telemetry)).await;

    let handle = service.start().await;

    let request = ClientGetConfigsRequest {
        client: Some(tracer_client("poller-client")),
        cached_target_files: Vec::new(),
    };
    let result = service
        .client_get_configs(request)
        .await
        .expect("bypass request should succeed");
    assert_eq!(result.config_status, ConfigStatus::Ok as i32);

    let snapshot = counters.snapshot();
    assert_eq!(
        snapshot.bypass_enqueued, 1,
        "bypass enqueue telemetry should increment"
    );
    assert_eq!(
        snapshot.bypass_timeout, 0,
        "no timeout expected when poller processes bypass"
    );
    assert!(
        snapshot.refresh_success >= 1,
        "refresh successes should be recorded"
    );

    handle.shutdown().await;
}

/// Ensures payload-related failures trigger an immediate retry when the background poller is enabled.
#[tokio::test(flavor = "multi_thread")]
async fn config_poller_retries_immediately_after_payload_error() {
    let server = Server::run();
    let mut missing_payload = sample_response(2, 2);
    missing_payload.target_files.clear();
    server.expect(
        Expectation::matching(request::method_path("GET", "/api/v0.1/status"))
            .times(0..=2)
            .respond_with(status_code(200)),
    );
    server.expect(
        Expectation::matching(request::method_path("POST", "/api/v0.1/configurations"))
            .times(2)
            .respond_with(cycle![
                status_code(200).body(missing_payload.encode_to_vec()),
                status_code(200).body(sample_response(3, 3).encode_to_vec()),
            ]),
    );

    let mut config = base_config();
    config.disable_background_poller = false;
    config.enable_websocket_echo = false;
    config.default_refresh_interval = Duration::from_secs(60);
    config.min_refresh_interval = Duration::from_secs(60);
    config.org_status_interval = Duration::from_secs(0);
    config.backoff_config.base_seconds = 30.0;
    config.backoff_config.max_backoff = Duration::from_secs(120);
    config.backoff_config.factor = 2.0;

    let service = build_service_with_config(&server, config);
    let telemetry = CountingTelemetry::default();
    let counters = telemetry.counters();
    service.set_telemetry(Arc::new(telemetry)).await;

    let handle = service.start().await;

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = service.snapshot().await;
            if snapshot.last_success.is_some() && snapshot.consecutive_errors == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("config poller should retry immediately after payload errors");

    let snapshot = counters.snapshot();
    assert!(
        snapshot.refresh_error >= 1,
        "expected at least one refresh error to be recorded"
    );
    assert!(
        snapshot.refresh_success >= 1,
        "expected subsequent success after the immediate retry"
    );

    handle.shutdown().await;
}

/// Ensures cache-bypass signals trigger an immediate retry when the first payload refresh fails.
#[tokio::test(flavor = "multi_thread")]
async fn bypass_signal_retries_immediately_after_payload_error() {
    let server = Server::run();
    let mut missing_payload = sample_response(2, 2);
    missing_payload.target_files.clear();
    server.expect(
        Expectation::matching(request::method_path("POST", "/api/v0.1/configurations"))
            .times(3)
            .respond_with(cycle![
                status_code(503),
                status_code(200).body(missing_payload.encode_to_vec()),
                status_code(200).body(sample_response(3, 3).encode_to_vec()),
            ]),
    );

    let mut config = base_config();
    config.disable_background_poller = false;
    config.enable_websocket_echo = false;
    config.default_refresh_interval = Duration::from_secs(30);
    config.min_refresh_interval = Duration::from_secs(1);
    config.org_status_interval = Duration::from_secs(0);

    let service = build_service_with_config(&server, config);
    let handle = service.start().await;

    // Wait for the first scheduled poller attempt to fail so backoff state is primed.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = service.snapshot().await;
            if snapshot.consecutive_errors >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("initial refresh should succeed");

    let sender = service
        .cache_bypass_sender()
        .await
        .expect("cache bypass sender available");
    let (tx, rx) = oneshot::channel();
    sender
        .send(CacheBypassSignal {
            completion: Some(tx),
        })
        .await
        .expect("bypass signal send");
    let bypass_result = rx.await.expect("bypass completion");
    assert!(
        bypass_result.is_err(),
        "expected payload failure to surface through bypass completion"
    );
    let error_time = Instant::now();

    let success_time = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = service.snapshot().await;
            if let Some(last_success) = snapshot.last_success {
                if last_success >= error_time {
                    break last_success;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("bypass retry should succeed quickly");

    handle.shutdown().await;

    let elapsed = success_time.duration_since(error_time);
    assert!(
        elapsed <= Duration::from_secs(2),
        "immediate retry should avoid waiting the full interval; observed {:?}",
        elapsed
    );
}

/// Confirms non-retryable errors stick to the backoff delay instead of retrying immediately.
#[tokio::test(flavor = "multi_thread")]
async fn config_poller_respects_backoff_for_http_errors() {
    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("POST", "/api/v0.1/configurations"))
            .times(2)
            .respond_with(cycle![
                status_code(503),
                status_code(200).body(sample_response(5, 5).encode_to_vec()),
            ]),
    );

    let mut config = base_config();
    config.disable_background_poller = false;
    config.enable_websocket_echo = false;
    config.default_refresh_interval = Duration::from_secs(30);
    config.min_refresh_interval = Duration::from_secs(1);
    config.org_status_interval = Duration::from_secs(0);
    config.backoff_config.base_seconds = 1.0;
    config.backoff_config.max_backoff = Duration::from_secs(2);
    config.backoff_config.factor = 1.0;

    let service = build_service_with_config(&server, config);
    let telemetry = TimingTelemetry::default();
    let events = telemetry.events.clone();
    service.set_telemetry(Arc::new(telemetry)).await;
    let handle = service.start().await;

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = service.snapshot().await;
            if snapshot.last_success.is_some() && snapshot.consecutive_errors == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("config poller should recover from retryable http errors using backoff");

    handle.shutdown().await;

    let events = events.lock().unwrap().clone();
    let first_error = events
        .iter()
        .find(|(_, kind)| *kind == "error")
        .map(|(ts, _)| *ts)
        .expect("expected refresh error to be recorded");
    let first_success = events
        .iter()
        .find(|(ts, kind)| *kind == "success" && *ts > first_error)
        .map(|(ts, _)| *ts)
        .expect("expected success after retry");
    let delta = first_success
        .checked_duration_since(first_error)
        .unwrap_or_default();
    assert!(
        delta >= Duration::from_secs(1),
        "backoff delay should prevent immediate retry; observed {:?}",
        delta
    );
}

/// Ensures the poller stops issuing immediate retries after hitting the cap.
#[tokio::test(flavor = "multi_thread")]
async fn config_poller_applies_immediate_retry_cap() {
    let server = Server::run();
    let mut missing_payload = sample_response(1, 1);
    missing_payload.target_files.clear();
    server.expect(
        Expectation::matching(request::method_path("POST", "/api/v0.1/configurations"))
            .times(5)
            .respond_with(cycle![
                status_code(200).body(missing_payload.encode_to_vec()),
                status_code(200).body(missing_payload.encode_to_vec()),
                status_code(200).body(missing_payload.encode_to_vec()),
                status_code(200).body(missing_payload.encode_to_vec()),
                status_code(200).body(sample_response(2, 2).encode_to_vec()),
            ]),
    );

    let mut config = base_config();
    config.disable_background_poller = false;
    config.enable_websocket_echo = false;
    config.default_refresh_interval = Duration::from_secs(2);
    config.min_refresh_interval = Duration::from_secs(1);
    config.org_status_interval = Duration::from_secs(0);
    config.backoff_config.base_seconds = 1.0;
    config.backoff_config.max_backoff = Duration::from_secs(2);
    config.backoff_config.factor = 1.0;

    let service = build_service_with_config(&server, config);
    let telemetry = TimingTelemetry::default();
    let events = telemetry.events();
    service.set_telemetry(Arc::new(telemetry)).await;
    let handle = service.start().await;

    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let snapshot = service.snapshot().await;
            if snapshot.last_success.is_some() && snapshot.consecutive_errors == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("config poller should eventually succeed after exhausting immediate retries");

    handle.shutdown().await;

    let events = events.lock().unwrap().clone();
    let error_times: Vec<Instant> = events
        .iter()
        .filter(|(_, kind)| *kind == "error")
        .map(|(ts, _)| *ts)
        .collect();
    assert_eq!(
        error_times.len(),
        4,
        "expected initial attempt plus three immediate retries before backoff"
    );

    let last_error = *error_times.last().expect("at least one error timestamp");
    let success_after_cap = events
        .iter()
        .find(|(ts, kind)| *kind == "success" && *ts > last_error)
        .map(|(ts, _)| *ts)
        .expect("success should follow after backoff interval");
    let delta = success_after_cap
        .checked_duration_since(last_error)
        .unwrap_or_default();
    assert!(
        delta >= Duration::from_secs(1),
        "poller should honor backoff after hitting the immediate retry cap; observed {:?}",
        delta
    );
}

/// Ensures the on-demand poller services bypass signals when the background loop is disabled.
#[tokio::test(flavor = "multi_thread")]
async fn on_demand_poller_processes_bypass_signal() {
    let server = Server::run();
    let response = sample_response(5, 5);
    server.expect(
        Expectation::matching(request::method_path("POST", "/api/v0.1/configurations"))
            .times(1)
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );
    let mut config = base_config();
    config.disable_background_poller = true;
    config.enable_websocket_echo = false;
    config.org_status_interval = Duration::from_secs(0);
    config.bypass_block_timeout = Duration::from_millis(500);
    config.new_client_block_timeout = Duration::from_millis(500);

    let service = build_service_with_config(&server, config);
    let telemetry = CountingTelemetry::default();
    let counters = telemetry.counters();
    service.set_telemetry(Arc::new(telemetry)).await;

    let handle = service.start().await;

    let request = ClientGetConfigsRequest {
        client: Some(tracer_client("manual-poller-client")),
        cached_target_files: Vec::new(),
    };
    let result = service
        .client_get_configs(request)
        .await
        .expect("on-demand bypass request should succeed");
    assert_eq!(result.config_status, ConfigStatus::Ok as i32);

    let snapshot = counters.snapshot();
    assert_eq!(
        snapshot.bypass_enqueued, 1,
        "bypass enqueue telemetry should increment for manual poller"
    );
    assert_eq!(
        snapshot.bypass_timeout, 0,
        "manual poller should not trigger bypass timeouts"
    );
    assert!(
        snapshot.refresh_success >= 1,
        "manual poller refreshes should register as successes"
    );

    handle.shutdown().await;
}

/// Ensures the on-demand poller issues an immediate follow-up refresh after payload errors.
#[tokio::test(flavor = "multi_thread")]
async fn on_demand_poller_retries_immediately_after_payload_error() {
    let server = Server::run();
    let mut missing_payload = sample_response(1, 1);
    missing_payload.target_files.clear();
    server.expect(
        Expectation::matching(request::method_path("POST", "/api/v0.1/configurations"))
            .times(2)
            .respond_with(cycle![
                status_code(200).body(missing_payload.encode_to_vec()),
                status_code(200).body(sample_response(2, 2).encode_to_vec()),
            ]),
    );

    let mut config = base_config();
    config.disable_background_poller = true;
    config.default_refresh_interval = Duration::from_secs(60);
    config.min_refresh_interval = Duration::from_secs(60);
    config.org_status_interval = Duration::from_secs(0);

    let service = build_service_with_config(&server, config);
    let telemetry = CountingTelemetry::default();
    let counters = telemetry.counters();
    service.set_telemetry(Arc::new(telemetry)).await;

    let handle = service.start().await;

    let request = ClientGetConfigsRequest {
        client: Some(tracer_client("retry-client")),
        cached_target_files: Vec::new(),
    };
    // Fresh caches surface `MissingMetadata` before the first refresh completes, so we only care
    // about triggering the bypass rather than the response outcome.
    let _ = service.client_get_configs(request).await;

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = service.snapshot().await;
            if snapshot.last_success.is_some() && snapshot.consecutive_errors == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("on-demand poller should retry immediately after payload errors");

    let snapshot = counters.snapshot();
    assert!(
        snapshot.refresh_error >= 1,
        "expected refresh error counter to increment"
    );
    assert!(
        snapshot.refresh_success >= 1,
        "expected follow-up success counter to increment"
    );

    handle.shutdown().await;
}

/// Ensures the on-demand poller stops retrying once it consumes the immediate retry budget.
#[tokio::test(flavor = "multi_thread")]
async fn on_demand_poller_stops_after_retry_cap() {
    let server = Server::run();
    let mut missing_payload = sample_response(1, 1);
    missing_payload.target_files.clear();
    server.expect(
        Expectation::matching(request::method_path("POST", "/api/v0.1/configurations"))
            .times(4)
            .respond_with(status_code(200).body(missing_payload.encode_to_vec())),
    );

    let mut config = base_config();
    config.disable_background_poller = true;
    config.default_refresh_interval = Duration::from_secs(60);
    config.min_refresh_interval = Duration::from_secs(1);
    config.org_status_interval = Duration::from_secs(0);

    let service = build_service_with_config(&server, config);
    let telemetry = CountingTelemetry::default();
    let counters = telemetry.counters();
    service.set_telemetry(Arc::new(telemetry)).await;

    let handle = service.start().await;

    let request = ClientGetConfigsRequest {
        client: Some(tracer_client("retry-cap-client")),
        cached_target_files: Vec::new(),
    };
    let _ = service.client_get_configs(request).await;

    let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = counters.snapshot();
            if snapshot.refresh_error >= 4 {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("expected on-demand retries to finish");
    assert_eq!(
        snapshot.refresh_error, 4,
        "expected initial attempt plus three immediate retries before giving up"
    );
    assert_eq!(
        snapshot.refresh_success, 0,
        "no refresh should succeed when backend never returns payloads"
    );

    handle.shutdown().await;
}

/// Ensures stalled bypass completions surface timeout telemetry.
#[tokio::test(flavor = "multi_thread")]
async fn client_bypass_timeout_records_telemetry() {
    let server = Server::run();
    let mut config = base_config();
    config.disable_background_poller = true;
    config.enable_websocket_echo = false;
    config.bypass_block_timeout = Duration::from_millis(20);
    config.new_client_block_timeout = Duration::from_millis(20);

    let service = build_service_with_config(&server, config);
    let telemetry = CountingTelemetry::default();
    let counters = telemetry.counters();
    service.set_telemetry(Arc::new(telemetry)).await;

    let response = sample_response(2, 2);
    let shared_arc = shared(&service);
    shared_arc
        .uptane
        .update(&response)
        .expect("populate uptane");

    let (bypass_tx, bypass_rx) = mpsc::channel(1);
    {
        let mut guard = shared_arc.cache_bypass_tx.lock().await;
        *guard = Some(bypass_tx);
    }

    // Keep the receiver alive without draining messages so the sender succeeds but the
    // completion channel never fires, forcing the caller to observe a timeout.
    let _receiver_guard = bypass_rx;

    let request = ClientGetConfigsRequest {
        client: Some(tracer_client("timeout-client")),
        cached_target_files: Vec::new(),
    };
    let result = service
        .client_get_configs(request)
        .await
        .expect("timeout path should still return payloads");
    assert_eq!(result.config_status, ConfigStatus::Ok as i32);

    let snapshot = counters.snapshot();
    assert_eq!(
        snapshot.bypass_enqueued, 1,
        "bypass enqueue telemetry should increment"
    );
    assert_eq!(
        snapshot.bypass_timeout, 1,
        "timeout telemetry should increment when completion stalls"
    );
}

/// Ensures rate-limited bypass requests emit dedicated telemetry.
#[tokio::test(flavor = "multi_thread")]
async fn client_bypass_rate_limit_records_telemetry() {
    let server = Server::run();
    let mut config = base_config();
    config.cache_bypass_limit = 1;
    config.disable_background_poller = true;
    config.enable_websocket_echo = false;
    let service = build_service_with_config(&server, config);
    let telemetry = CountingTelemetry::default();
    let counters = telemetry.counters();
    service.set_telemetry(Arc::new(telemetry)).await;

    let response = sample_response(2, 2);
    let shared_arc = shared(&service);
    shared_arc
        .uptane
        .update(&response)
        .expect("populate uptane");

    {
        let mut guard = shared_arc.state.lock().await;
        assert!(
            guard.cache_bypass.try_consume(),
            "expected initial bypass budget to be available"
        );
        assert!(
            !guard.cache_bypass.try_consume(),
            "second consume should exhaust the budget"
        );
    }

    let (bypass_tx, _bypass_rx) = mpsc::channel(1);
    {
        let mut guard = shared_arc.cache_bypass_tx.lock().await;
        *guard = Some(bypass_tx);
    }

    let request = ClientGetConfigsRequest {
        client: Some(tracer_client("rate-limit-client")),
        cached_target_files: Vec::new(),
    };
    let response = service
        .client_get_configs(request)
        .await
        .expect("rate-limited request should still succeed");
    assert_eq!(response.config_status, ConfigStatus::Ok as i32);

    let snapshot = counters.snapshot();
    assert_eq!(
        snapshot.bypass_rate_limited, 1,
        "rate-limited telemetry should increment"
    );
    assert_eq!(
        snapshot.bypass_rejected, 1,
        "generic rejection counter should still track the event"
    );
}

/// Validates the service shared debug formatter renders without locking state.
#[tokio::test]
async fn service_shared_debug_renders_config() {
    let server = Server::run();
    let service = build_service(&server);
    let rendered = format!("{:?}", shared(&service));
    assert!(
        rendered.contains("ServiceShared"),
        "debug output should include struct name"
    );
}

/// Verifies that the refresh interval accessor returns the stored value.
#[tokio::test]
async fn get_refresh_interval_reflects_state() {
    let server = Server::run();
    let service = build_service(&server);
    let shared_arc = shared(&service);
    {
        let mut guard = shared_arc.state.lock().await;
        guard.refresh_interval = Duration::from_millis(123);
    }
    let interval = service.get_refresh_interval().await;
    assert_eq!(interval, Duration::from_millis(123));
}

/// Confirms that client_state reports registered clients with elapsed timings.
#[tokio::test]
async fn client_state_reports_registered_clients() {
    let server = Server::run();
    let service = build_service(&server);
    register_client_for_test(&service, "client-a", &["APM_TRACING"]).await;
    let state = service.client_state().await;
    assert_eq!(state.len(), 1);
    assert_eq!(state[0].id, "client-a");
    assert_eq!(state[0].products, vec!["APM_TRACING".to_string()]);
}

/// Ensures config path classification recognises all prefixes.
#[test]
fn classify_config_path_detects_sources() {
    assert_eq!(
        classify_config_path("datadog/1/product/config/file"),
        ConfigPathSource::Datadog
    );
    assert_eq!(
        classify_config_path("employee/product/config/file"),
        ConfigPathSource::Employee
    );
    assert_eq!(
        classify_config_path("thirdparty/product/config/file"),
        ConfigPathSource::Unknown
    );
}

/// Covers error paths in config path parsing helpers.
#[test]
fn parse_config_path_reports_malformed_inputs() {
    assert!(
        parse_config_path("unknown/prefix").is_err(),
        "unknown prefix should be rejected"
    );
    assert!(
        parse_datadog_config_path("datadog/not-a-number/product/config/file").is_err(),
        "non-numeric org id should be rejected"
    );
    assert!(
        parse_employee_config_path("employee//config/file").is_err(),
        "empty product segment should be rejected"
    );
}

/// Exercises custom metadata parsing edge cases.
#[test]
fn parse_custom_metadata_handles_null_and_values() {
    assert!(parse_custom_metadata(None).is_ok());
    assert!(parse_custom_metadata(Some(&Value::Null)).is_ok());
    let custom = json!({ "opaque_backend_state": "state" });
    assert!(parse_custom_metadata(Some(&custom)).is_ok());
}

/// Ensures invalid expiration timestamps propagate an error.
#[test]
fn config_is_expired_reports_invalid_timestamps() {
    let err = config_is_expired(i64::MAX).unwrap_err();
    assert!(
        matches!(err, ServiceError::InvalidRequest(ref message) if message.contains("invalid config expiration timestamp")),
        "expected invalid timestamp error, found {err:?}"
    );
}

/// Ensures predicate helpers surface malformed version constraints.
#[test]
fn predicate_matches_rejects_invalid_versions() {
    let mut client = tracer_client("predicate-client");
    client.client_tracer.as_mut().unwrap().tracer_version = "abc".into();
    let mut predicate = TracerPredicateV1::default();
    predicate.tracer_version = ">=1.0.0".into();
    let err = predicate_matches(&client, &predicate).unwrap_err();
    assert!(
        matches!(err, ServiceError::InvalidRequest(ref message) if message.contains("invalid tracer version")),
        "expected invalid tracer version error, found {err:?}"
    );

    client.client_tracer.as_mut().unwrap().tracer_version = "1.2.3".into();
    predicate.tracer_version = "??".into();
    let err = predicate_matches(&client, &predicate).unwrap_err();
    assert!(
        matches!(err, ServiceError::InvalidRequest(ref message) if message.contains("invalid tracer predicate constraint")),
        "expected invalid constraint error, found {err:?}"
    );
}

/// Confirms that lenient semver parsing normalises shorthand versions.
#[test]
fn parse_semver_version_normalises_shorthand() {
    let parsed = parse_semver_version("1.2").expect("parse failed");
    assert_eq!(parsed.major, 1);
    assert_eq!(parsed.minor, 2);
    assert_eq!(parsed.patch, 0);
    let parsed_with_suffix = parse_semver_version("2+meta").expect("parse with suffix failed");
    assert_eq!(parsed_with_suffix.major, 2);
}

/// Validates error kind classification used by refresh telemetry.
#[test]
fn classify_error_kind_covers_all_variants() {
    let runtime = Runtime::new().unwrap();
    let transport_err =
        runtime.block_on(async { reqwest::get("http://127.0.0.1:9").await.unwrap_err() });
    assert_eq!(
        classify_error_kind(&ServiceError::Http(HttpError::Unauthorized)),
        "unauthorized"
    );
    assert_eq!(
        classify_error_kind(&ServiceError::Http(HttpError::Proxy(400))),
        "proxy-400"
    );
    assert_eq!(
        classify_error_kind(&ServiceError::Http(HttpError::Retryable(503))),
        "retryable-503"
    );
    assert_eq!(
        classify_error_kind(&ServiceError::Http(HttpError::Transport(transport_err))),
        "transport"
    );
    assert_eq!(
        classify_error_kind(&ServiceError::Http(HttpError::Decode(
            prost::DecodeError::new("decode")
        ))),
        "decode"
    );
    assert_eq!(
        classify_error_kind(&ServiceError::Uptane(UptaneError::Store(
            StoreError::MissingMetadata
        ))),
        "uptane"
    );
    assert_eq!(
        classify_error_kind(&ServiceError::InvalidRequest("bad".into())),
        "invalid-request"
    );
    assert_eq!(
        classify_error_kind(&ServiceError::Json(serde_json::Error::io(IoError::new(
            IoErrorKind::Other,
            "bad"
        )))),
        "json"
    );
    assert_eq!(
        classify_error_kind(&ServiceError::Protobuf(prost::DecodeError::new(
            "bad proto"
        ))),
        "protobuf"
    );
}

/// Ensures a successful refresh updates Uptane state and service metadata.
#[tokio::test(flavor = "multi_thread")]
async fn refresh_updates_state() {
    let server = Server::run();
    let response = sample_response(1, 2);
    let body = response.encode_to_vec();
    server.expect(
        Expectation::matching(all_of![
            request::method_path("POST", "/api/v0.1/configurations"),
            request::headers(contains(("dd-api-key", "key")))
        ])
        .respond_with(status_code(200).body(body)),
    );

    let service = build_service(&server);
    service.refresh_once().await.expect("refresh must succeed");

    let uptane = service.uptane_state();
    let payload = uptane
        .target_file(SAMPLE_TARGET_PATH)
        .expect("target lookup")
        .expect("payload exists");
    assert_eq!(payload, b"payload".to_vec());

    let snapshot = service.snapshot().await;
    assert!(snapshot.last_error.is_none());
    assert!(snapshot.last_success.is_some());
    assert_eq!(snapshot.next_refresh, Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread")]
/// Ensures refresh fails when the backend sends tampered target payloads.
async fn refresh_rejects_target_payload_hash_mismatch() {
    use httptest::responders::status_code;

    let server = Server::run();
    let mut response = sample_response(1, 1);
    if let Some(file) = response.target_files.get_mut(0) {
        file.raw = b"PAYLOAD".to_vec();
    }
    server.expect(
        Expectation::matching(request::method_path("POST", "/api/v0.1/configurations"))
            .times(1)
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );

    let service = build_service(&server);
    let err = service
        .refresh_once()
        .await
        .expect_err("tampered payload should fail");
    match err {
        ServiceError::Uptane(UptaneError::TargetPayloadHashMismatch { .. }) => {}
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
/// Ensures refresh fails when the backend omits required target payloads on cold start.
async fn refresh_rejects_missing_target_payloads() {
    use httptest::responders::status_code;

    let server = Server::run();
    let mut response = sample_response(1, 1);
    response.target_files.clear();
    server.expect(
        Expectation::matching(request::method_path("POST", "/api/v0.1/configurations"))
            .times(1)
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );

    let service = build_service(&server);
    let err = service
        .refresh_once()
        .await
        .expect_err("missing payloads should fail");
    match err {
        ServiceError::Uptane(UptaneError::MissingTargetPayload { .. }) => {}
        other => panic!("unexpected error: {other:?}"),
    }
}

/// Verifies that failing refreshes trigger backoff accounting.
#[tokio::test(flavor = "multi_thread")]
async fn refresh_error_tracks_backoff() {
    let server = Server::run();
    server.expect(
        Expectation::matching(request::path("/api/v0.1/configurations"))
            .respond_with(status_code(503)),
    );

    let service = build_service(&server);
    let result = service.refresh_once().await;
    assert!(result.is_err());

    let snapshot = service.snapshot().await;
    assert!(snapshot.last_error.is_some());
    assert_eq!(snapshot.consecutive_errors, 1);
    assert!(snapshot.next_refresh >= Duration::from_secs(30));
}

/// Confirms the organisation status poller records backend responses.
#[tokio::test(flavor = "multi_thread")]
async fn org_status_poller_updates_snapshot() {
    let server = Server::run();
    server.expect(
        Expectation::matching(request::path("/api/v0.1/status")).respond_with(
            status_code(200).body(
                OrgStatusResponse {
                    enabled: true,
                    authorized: false,
                }
                .encode_to_vec(),
            ),
        ),
    );

    let mut config = base_config();
    config.org_status_interval = Duration::from_millis(200);
    let service = build_service_with_config(&server, config);
    #[derive(Clone, Default)]
    struct RecordingTelemetry {
        events: Arc<Mutex<Vec<(bool, bool)>>>,
    }

    impl RemoteConfigTelemetry for RecordingTelemetry {
        /// Hooks into the telemetry output to capture status transitions.
        fn on_org_status_change(
            &self,
            _previous: Option<OrgStatusSnapshot>,
            current: OrgStatusSnapshot,
        ) {
            self.events
                .lock()
                .unwrap()
                .push((current.enabled, current.authorized));
        }
    }

    let recording = RecordingTelemetry::default();
    service.set_telemetry(Arc::new(recording.clone())).await;
    let handle = service.start().await;

    let (snapshot, events) = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = service.snapshot().await;
            let events = recording.events.lock().unwrap().clone();
            if snapshot.org_status.is_some() && !events.is_empty() {
                break (snapshot, events);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("org status poller should publish status");
    let status = snapshot.org_status.expect("org status snapshot missing");
    assert!(status.enabled);
    assert!(!status.authorized);
    assert_eq!(events, vec![(true, false)]);

    handle.shutdown().await;
}

/// Ensures org-status poller tracks 503/504 counters and resets after success.
#[tokio::test(flavor = "multi_thread")]
async fn org_status_poll_tracks_503_errors() {
    let server = Server::run();
    let service = build_service(&server);
    let shared = shared(&service);

    for expected in 1..=2 {
        server.expect(
            Expectation::matching(request::method_path("GET", "/api/v0.1/status"))
                .respond_with(status_code(503)),
        );
        let result = shared.poll_org_status().await;
        assert!(result.is_err(), "poll should fail with retryable error");
        let guard = shared.state.lock().await;
        assert_eq!(guard.org_status_fetch_503_count, expected);
    }

    server.expect(
        Expectation::matching(request::method_path("GET", "/api/v0.1/status")).respond_with(
            status_code(200).body(
                OrgStatusResponse {
                    enabled: true,
                    authorized: true,
                }
                .encode_to_vec(),
            ),
        ),
    );

    shared
        .poll_org_status()
        .await
        .expect("successful poll should reset counter");
    let guard = shared.state.lock().await;
    assert_eq!(guard.org_status_fetch_503_count, 0);
}

/// Verifies the org-status poller performs the first request immediately even with long intervals.
#[tokio::test(flavor = "multi_thread")]
async fn org_status_poller_runs_immediately_with_long_interval() {
    let server = Server::run();
    server.expect(
        Expectation::matching(request::path("/api/v0.1/status"))
            .times(1)
            .respond_with(
                status_code(200).body(
                    OrgStatusResponse {
                        enabled: true,
                        authorized: true,
                    }
                    .encode_to_vec(),
                ),
            ),
    );

    let mut config = base_config();
    config.org_status_interval = Duration::from_secs(30);
    let service = build_service_with_config(&server, config);
    let start = Instant::now();
    let handle = service.start().await;

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if service.snapshot().await.org_status.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("org status poller should run immediately despite long interval");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "initial org status poll should not wait for the configured interval"
    );

    handle.shutdown().await;
}

/// Confirms the cache-bypass tracker replenishes its budget after the window elapses.
#[test]
fn cache_bypass_tracker_resets_window() {
    let mut tracker = CacheBypassTracker::new(1, Duration::from_millis(10));
    assert!(tracker.try_consume());
    assert!(!tracker.try_consume());
    std::thread::sleep(Duration::from_millis(15));
    assert!(tracker.try_consume());
}

/// Verifies that backend overrides tighten the bypass window to the new interval.
#[test]
fn handle_refresh_success_updates_bypass_window() {
    let mut config = base_config();
    config.default_refresh_interval = Duration::from_secs(30);
    let mut state = ServiceState::new(&config);
    let next = state.handle_refresh_success(&config, Some(Duration::from_secs(5)));
    assert_eq!(state.refresh_interval, Duration::from_secs(5));
    assert_eq!(next, Duration::from_secs(5));
    assert_eq!(state.cache_bypass.window(), Duration::from_secs(5));
}

/// Confirms the cache-bypass sender becomes available after the service starts.
#[tokio::test(flavor = "multi_thread")]
async fn cache_bypass_sender_available_post_start() {
    let server = Server::run();
    let service = build_service(&server);
    assert!(
        service.cache_bypass_sender().await.is_none(),
        "sender should be absent before start"
    );
    let handle = service.start().await;
    assert!(
        service.cache_bypass_sender().await.is_some(),
        "sender should be present after start"
    );
    handle.shutdown().await;
}

/// Ensures refresh overrides are only extracted when the custom field is present.
#[test]
fn extract_refresh_override_handles_missing_custom_section() {
    let without_override = response_without_override();
    assert!(extract_refresh_override(&without_override).is_none());

    let with_override = sample_response(1, 1);
    assert_eq!(
        extract_refresh_override(&with_override),
        Some(Duration::from_secs(5))
    );
}

/// Validates that the service state throttles refresh attempts within the configured window.
#[test]
fn service_state_throttles_min_spacing() {
    let config = base_config();
    let mut state = ServiceState::new(&config);
    assert!(state
        .time_until_next_attempt(Duration::from_millis(50))
        .is_none());
    state.record_attempt();
    let wait = state
        .time_until_next_attempt(Duration::from_millis(50))
        .expect("throttle expected");
    assert!(wait <= Duration::from_millis(50));
}

/// Confirms error bookkeeping updates the next refresh interval using backoff.
#[test]
fn service_state_handles_error_backoff() {
    let config = base_config();
    let mut state = ServiceState::new(&config);
    let error = ServiceError::Http(HttpError::Retryable(500));
    let (delay, _) = state.handle_refresh_error(&config, &error);
    assert!(delay >= config.min_refresh_interval);
    assert_eq!(state.consecutive_errors, 1);
    assert!(state.last_error.is_some());
}

/// Ensures timestamp boundary guard mirrors the TUF resolution.
#[test]
fn time_until_timestamp_boundary_respects_resolution() {
    let config = base_config();
    let mut state = ServiceState::new(&config);
    state.last_update_timestamp = Some(Instant::now());
    let wait = state
        .time_until_timestamp_boundary(Duration::from_millis(20))
        .expect("delay expected");
    assert!(wait <= Duration::from_millis(20));
    std::thread::sleep(Duration::from_millis(25));
    assert!(state
        .time_until_timestamp_boundary(Duration::from_millis(20))
        .is_none());
}

/// Ensures error log levels downgrade when org status is disabled or unauthorized.
#[test]
fn handle_refresh_error_uses_org_status_for_log_level() {
    let config = base_config();
    let mut state = ServiceState::new(&config);
    state.previous_org_status = Some(OrgStatusSnapshot {
        enabled: false,
        authorized: false,
        fetched_at: Instant::now(),
    });

    let error = ServiceError::Http(HttpError::Retryable(503));
    let (_, level) = state.handle_refresh_error(&config, &error);
    assert_eq!(level, RefreshErrorLevel::Debug);

    state.previous_org_status = Some(OrgStatusSnapshot {
        enabled: true,
        authorized: true,
        fetched_at: Instant::now(),
    });

    for _ in 0..MAX_FETCH_503_LOG_LEVEL {
        let (_, lvl) =
            state.handle_refresh_error(&config, &ServiceError::Http(HttpError::Retryable(503)));
        if lvl == RefreshErrorLevel::Error {
            assert!(state.fetch_503_504_count >= MAX_FETCH_503_LOG_LEVEL);
        }
    }
}

/// Ensures 503/504 counters increment and reset after a successful refresh.
#[test]
fn fetch_503_counters_reset_after_success() {
    let config = base_config();
    let mut state = ServiceState::new(&config);
    let error = ServiceError::Http(HttpError::Retryable(503));

    state.handle_refresh_error(&config, &error);
    assert_eq!(state.fetch_503_504_count, 1);

    state.handle_refresh_error(&config, &error);
    assert_eq!(state.fetch_503_504_count, 2);

    state.handle_refresh_error(&config, &ServiceError::Http(HttpError::Retryable(504)));
    assert_eq!(state.fetch_503_504_count, 3);

    state.handle_refresh_success(&config, None);
    assert_eq!(state.fetch_503_504_count, 0);
}

/// Ensures org-status 503 counters escalate and reset after success.
#[test]
fn org_status_fetch_503_counter_resets_after_success() {
    let config = base_config();
    let mut state = ServiceState::new(&config);
    for attempt in 0..MAX_ORG_STATUS_FETCH_503_LOG_LEVEL {
        let escalate = state.record_org_status_fetch_error();
        if attempt + 1 >= MAX_ORG_STATUS_FETCH_503_LOG_LEVEL {
            assert!(escalate, "expected escalation on attempt {attempt}");
        } else {
            assert!(!escalate, "unexpected escalation prior to threshold");
        }
    }
    state.reset_org_status_fetch_errors();
    assert!(!state.record_org_status_fetch_error());
}

/// Confirms cold-start requests return an empty payload instead of erroring.
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_returns_empty_on_cold_start() {
    let server = Server::run();
    let service = build_service(&server);
    register_client_for_test(&service, "cold-client", &["APM_TRACING"]).await;

    let req = ClientGetConfigsRequest {
        client: Some(tracer_client("cold-client")),
        cached_target_files: Vec::new(),
    };
    let response = service
        .client_get_configs(req)
        .await
        .expect("client request should succeed during cold start");
    assert!(response.roots.is_empty());
    assert!(response.targets.is_empty());
    assert!(response.target_files.is_empty());
    assert!(response.client_configs.is_empty());
    assert_eq!(response.config_status, 0);
}

/// Verifies that `client_get_configs` returns payloads for registered clients.
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_returns_payloads() {
    let server = Server::run();
    let response = sample_response(1, 2);
    server.expect(
        Expectation::matching(request::path("/api/v0.1/configurations"))
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );

    let service = build_service(&server);
    service.refresh_once().await.unwrap();
    register_client_for_test(&service, "client-a", &["APM_TRACING"]).await;

    let req = ClientGetConfigsRequest {
        client: Some(tracer_client("client-a")),
        cached_target_files: Vec::new(),
    };
    let result = service.client_get_configs(req).await.unwrap();
    assert!(!result.roots.is_empty());
    assert!(!result.targets.is_empty());
    assert!(!result.target_files.is_empty());
    assert_eq!(result.client_configs, vec![SAMPLE_TARGET_PATH.to_string()]);
    assert_eq!(result.config_status, ConfigStatus::Ok as i32);
}

#[tokio::test(flavor = "multi_thread")]
/// Tracks org enablement flags and refresh errors via the status handle API.
async fn status_handle_tracks_flags_and_errors() {
    let server = Server::run();
    let service = build_service(&server);
    let status = service.status_handle();
    assert!(!status.org_enabled());
    assert!(!status.org_authorized());
    assert!(status.last_error().await.is_none());

    let shared = shared(&service);
    shared.status.set_org_flags(true, false);
    let error = ServiceError::InvalidRequest("boom".into());
    shared.handle_refresh_error(&error).await;
    assert!(status.org_enabled());
    assert!(!status.org_authorized());
    assert_eq!(
        status.last_error().await.as_deref(),
        Some("invalid request: boom")
    );

    shared.record_refresh_success(Duration::from_secs(1)).await;
    assert!(status.last_error().await.is_none());
}

/// Ensures datadog configs are rejected when the org_id does not match the rc_key.
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_rejects_mismatched_org_id() {
    let server = Server::run();
    let response = sample_response(1, 2);
    server.expect(
        Expectation::matching(request::path("/api/v0.1/configurations"))
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );

    let mut config = base_config();
    config.rc_key = Some(RcKey {
        app_key: "app".into(),
        org_id: 999999,
        datacenter: "datadoghq.com".into(),
    });
    let service = build_service_with_config(&server, config);
    let err = service
        .refresh_once()
        .await
        .expect_err("refresh should fail when org IDs diverge");
    match err {
        ServiceError::Uptane(UptaneError::OrgIdMismatch {
            path,
            expected,
            actual,
        }) => {
            assert_eq!(path, SAMPLE_TARGET_PATH);
            assert_eq!(expected, 999999);
            assert_eq!(actual, 12345);
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

#[tokio::test]
/// Verifies the snapshot org UUID must match the stored identifier before serving configs.
async fn client_get_configs_rejects_snapshot_org_uuid_mismatch() {
    let server = Server::run();
    let service = build_service(&server);
    let shared_arc = shared(&service);
    let mut response = sample_response(1, 1);
    if let Some(config_metas) = response.config_metas.as_mut() {
        // Embed a snapshot org UUID that intentionally diverges from the stored value.
        config_metas.snapshot = Some(mk_snapshot_meta_with_org(2, Some("org-snapshot")));
    }
    let err = shared_arc
        .uptane
        .update(&response)
        .expect_err("uptane update should reject mismatched snapshot org uuid");
    match err {
        UptaneError::OrgUuidMismatch { stored, snapshot } => {
            assert_eq!(stored, "00000000-0000-0000-0000-000000000000");
            assert_eq!(snapshot, "org-snapshot");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
/// Confirms configs are served when the snapshot org UUID matches the stored UUID.
async fn client_get_configs_accepts_snapshot_org_uuid_match() {
    let server = Server::run();
    let service = build_service(&server);
    let shared_arc = shared(&service);
    let mut response = sample_response(1, 1);
    if let Some(config_metas) = response.config_metas.as_mut() {
        // Embed the org UUID that matches the credentials associated with the cache.
        config_metas.snapshot = Some(mk_snapshot_meta_with_org(2, Some("org-aligned")));
    }
    shared_arc
        .uptane
        .update_org_uuid("org-aligned")
        .expect("org uuid stored");
    {
        let mut guard = shared_arc.state.lock().await;
        guard.org_uuid = Some("org-aligned".into());
    }
    shared_arc.uptane.update(&response).unwrap();
    register_client_for_test(&service, "org-snapshot-allow", &["APM_TRACING"]).await;
    let request = ClientGetConfigsRequest {
        client: Some(tracer_client("org-snapshot-allow")),
        cached_target_files: Vec::new(),
    };
    let response = service
        .client_get_configs(request)
        .await
        .expect("matching snapshot org uuid should succeed");
    assert_eq!(response.config_status, ConfigStatus::Ok as i32);
    assert!(
        !response.target_files.is_empty(),
        "matching snapshot should still serve target files"
    );
}

/// Ensures clients receive every director root they are missing (and nothing else).
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_streams_only_missing_director_roots() {
    let server = Server::run();
    let mut response = sample_response(2, 3);
    if let Some(director) = response.director_metas.as_mut() {
        director.roots = vec![mk_top_meta("root", 2), mk_top_meta("root", 3)];
    }
    server.expect(
        Expectation::matching(request::path("/api/v0.1/configurations"))
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );

    let service = build_service(&server);
    service.refresh_once().await.unwrap();

    register_client_for_test(&service, "root-stream-client", &["APM_TRACING"]).await;
    let mut client = tracer_client("root-stream-client");
    if let Some(state) = client.state.as_mut() {
        state.root_version = 1;
    }

    let req = ClientGetConfigsRequest {
        client: Some(client),
        cached_target_files: Vec::new(),
    };
    let result = service.client_get_configs(req).await.unwrap();
    let root_versions: Vec<u64> = result
        .roots
        .iter()
        .map(|bytes| {
            serde_json::from_slice::<serde_json::Value>(bytes)
                .expect("root json")
                .pointer("/signed/version")
                .and_then(Value::as_u64)
                .expect("version field present")
        })
        .collect();
    assert_eq!(root_versions, vec![2, 3]);
}

/// Ensures the service returns an empty payload when the client targets version is up to date.
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_returns_empty_when_up_to_date() {
    let server = Server::run();
    let response = sample_response(1, 3);
    server.expect(
        Expectation::matching(request::path("/api/v0.1/configurations"))
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );

    let service = build_service(&server);
    service.refresh_once().await.unwrap();
    register_client_for_test(&service, "client-up-to-date", &["APM_TRACING"]).await;

    let mut client = tracer_client("client-up-to-date");
    if let Some(state) = client.state.as_mut() {
        // Mirror the backend version so the service detects parity.
        state.targets_version = 3;
    }

    let req = ClientGetConfigsRequest {
        client: Some(client),
        cached_target_files: Vec::new(),
    };
    let result = service.client_get_configs(req).await.unwrap();
    assert!(result.roots.is_empty());
    assert!(result.targets.is_empty());
    assert!(result.target_files.is_empty());
    assert!(result.client_configs.is_empty());
    assert_eq!(result.config_status, ConfigStatus::Ok as i32);
}

/// Ensures targets metadata is forwarded byte-for-byte without canonicalisation.
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_preserves_targets_serialization() {
    let server = Server::run();
    let mut response = sample_response(1, 2);
    let custom_targets = format!(
        "{{  \"signatures\":[], \"signed\":{{ \"targets\":{{\"{path}\":{{\"length\":7,\"hashes\":{{\"sha256\":\"{hash}\"}},\"custom\":{{\"agent_refresh_interval\":7}} }} }}, \"version\": 2, \"_type\": \"targets\", \"expires\": \"2030-01-01T00:00:00Z\" }} }}",
        path = SAMPLE_TARGET_PATH,
        hash = SAMPLE_TARGET_HASH
    );
    let raw_targets = custom_targets.into_bytes();
    if let Some(director) = response.director_metas.as_mut() {
        if let Some(targets) = director.targets.as_mut() {
            targets.raw = raw_targets.clone();
        }
    }
    server.expect(
        Expectation::matching(request::path("/api/v0.1/configurations"))
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );

    let service = build_service(&server);
    service.refresh_once().await.unwrap();
    register_client_for_test(&service, "targets-client", &["APM_TRACING"]).await;

    let req = ClientGetConfigsRequest {
        client: Some(tracer_client("targets-client")),
        cached_target_files: Vec::new(),
    };
    let result = service.client_get_configs(req).await.unwrap();
    assert_eq!(
        result.targets, raw_targets,
        "targets bytes should match the stored payload"
    );
}

/// Ensures timestamp expiration triggers a flush response mirroring the Go agent.
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_flushes_when_timestamp_expired() {
    let server = Server::run();
    let mut response = sample_response(1, 4);
    if let Some(config) = response.config_metas.as_mut() {
        config.timestamp = Some(mk_timestamp_meta(1, "2000-01-01T00:00:00Z"));
    }
    if let Some(director) = response.director_metas.as_mut() {
        director.timestamp = Some(mk_timestamp_meta(4, "2000-01-01T00:00:00Z"));
    }
    server.expect(
        Expectation::matching(request::path("/api/v0.1/configurations"))
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );

    let service = build_service(&server);
    service.refresh_once().await.unwrap();
    register_client_for_test(&service, "client-flush", &["APM_TRACING"]).await;

    let mut client = tracer_client("client-flush");
    if let Some(state) = client.state.as_mut() {
        // Use a stale version to ensure the flush path wins over normal delivery.
        state.targets_version = 1;
    }

    let req = ClientGetConfigsRequest {
        client: Some(client),
        cached_target_files: Vec::new(),
    };
    let result = service.client_get_configs(req).await.unwrap();
    assert_eq!(result.config_status, ConfigStatus::Expired as i32);
    assert!(result.roots.is_empty());
    assert!(result.target_files.is_empty());
    assert!(result.client_configs.is_empty());

    let expected_targets = service
        .uptane_state()
        .director_targets_raw()
        .expect("targets available");
    assert_eq!(result.targets, expected_targets);
}

/// Ensures tracer predicates restrict the configurations served to the client.
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_honours_tracer_predicates() {
    let server = Server::run();
    let payload_a = br#"{ "alpha": true }"#.to_vec();
    let payload_b = br#"{ "beta": true }"#.to_vec();

    let targets_value = json!({
        (SAMPLE_TARGET_PATH): {
            "length": payload_a.len(),
            "hashes": { "sha256": compute_sha256(&payload_a) },
            "custom": {
                "tracer-predicates": {
                    "tracer_predicates_v1": [
                        { "language": "rust" }
                    ]
                },
                "expires": 4_102_444_800i64
            }
        },
        (SAMPLE_SECOND_TARGET_PATH): {
            "length": payload_b.len(),
            "hashes": { "sha256": compute_sha256(&payload_b) },
            "custom": {
                "tracer-predicates": {
                    "tracer_predicates_v1": [
                        { "language": "python" }
                    ]
                },
                "expires": 4_102_444_800i64
            }
        }
    });

    let response = LatestConfigsResponse {
        config_metas: Some(ConfigMetas {
            roots: vec![mk_top_meta("root", 2)],
            timestamp: Some(mk_timestamp_meta(2, "2030-01-01T00:00:00Z")),
            snapshot: Some(mk_top_meta("snapshot", 2)),
            top_targets: Some(mk_targets_meta_from_value(2, targets_value.clone(), None)),
            delegated_targets: vec![],
        }),
        director_metas: Some(DirectorMetas {
            roots: vec![mk_top_meta("root", 2)],
            timestamp: Some(mk_timestamp_meta(2, "2030-01-01T00:00:00Z")),
            snapshot: Some(mk_top_meta("snapshot", 2)),
            targets: Some(mk_targets_meta_from_value(2, targets_value, None)),
        }),
        target_files: vec![
            File {
                path: SAMPLE_TARGET_PATH.to_string(),
                raw: payload_a.clone(),
            },
            File {
                path: SAMPLE_SECOND_TARGET_PATH.to_string(),
                raw: payload_b.clone(),
            },
        ],
        ..Default::default()
    };

    server.expect(
        Expectation::matching(request::path("/api/v0.1/configurations"))
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );

    let service = build_service(&server);
    service.refresh_once().await.unwrap();
    register_client_for_test(&service, "client-predicates", &["APM_TRACING"]).await;

    let req = ClientGetConfigsRequest {
        client: Some(tracer_client("client-predicates")),
        cached_target_files: Vec::new(),
    };
    let result = service.client_get_configs(req).await.unwrap();

    assert_eq!(result.client_configs, vec![SAMPLE_TARGET_PATH.to_string()]);
    assert_eq!(result.target_files.len(), 1);
    assert_eq!(result.target_files[0].path, SAMPLE_TARGET_PATH);
    assert_eq!(result.target_files[0].raw, payload_a);
}

/// Ensures cached targets are not re-transmitted to the client.
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_skips_cached_files() {
    let server = Server::run();
    let response = sample_response(1, 2);
    server.expect(
        Expectation::matching(request::path("/api/v0.1/configurations"))
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );

    let service = build_service(&server);
    service.refresh_once().await.unwrap();
    register_client_for_test(&service, "client-b", &["APM_TRACING"]).await;

    let payload = service
        .uptane_state()
        .target_file(SAMPLE_TARGET_PATH)
        .unwrap()
        .expect("target exists");
    let hash = compute_sha256(&payload);

    let cached_meta = TargetFileMeta {
        path: SAMPLE_TARGET_PATH.into(),
        length: payload.len() as i64,
        hashes: vec![TargetFileHash {
            algorithm: "sha256".into(),
            hash,
        }],
    };

    let req = ClientGetConfigsRequest {
        client: Some(tracer_client("client-b")),
        cached_target_files: vec![cached_meta],
    };
    let result = service.client_get_configs(req).await.unwrap();
    assert!(result.target_files.is_empty());
    assert_eq!(result.client_configs, vec![SAMPLE_TARGET_PATH.to_string()]);
}

/// Ensures the byte-oriented helper decodes requests and encodes responses.
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_from_bytes_round_trip() {
    let server = Server::run();
    let response = sample_response(1, 2);
    server.expect(
        Expectation::matching(request::path("/api/v0.1/configurations"))
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );

    let service = build_service(&server);
    service.refresh_once().await.unwrap();
    register_client_for_test(&service, "client-bytes", &["APM_TRACING"]).await;

    let request = ClientGetConfigsRequest {
        client: Some(tracer_client("client-bytes")),
        cached_target_files: Vec::new(),
    };

    let payload = request.encode_to_vec();
    let response_bytes = service
        .client_get_configs_from_bytes(&payload)
        .await
        .expect("byte helper succeeds");
    let decoded = ClientGetConfigsResponse::decode(response_bytes.as_slice()).unwrap();
    assert_eq!(decoded.config_status, ConfigStatus::Ok as i32);
}

/// Confirms invalid protobuf payloads propagate a decode error.
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_from_bytes_errors_on_invalid_payload() {
    let server = Server::run();
    let service = build_service(&server);
    let err = service
        .client_get_configs_from_bytes(b"not-protobuf")
        .await
        .expect_err("invalid payload should fail");
    matches!(err, ServiceError::Protobuf(_));
}

/// Validates that a new client triggers a bypass refresh and receives the stored payload bytes.
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_performs_bypass_and_preserves_files() {
    let server = Server::run();
    let payload = br#"{ "z": true, "a": false }"#.to_vec();
    let mut response = sample_response(3, 4);
    response.target_files = vec![File {
        path: SAMPLE_TARGET_PATH.into(),
        raw: payload.clone(),
    }];
    let targets_value = json!({
        (SAMPLE_TARGET_PATH): {
            "length": payload.len(),
            "hashes": { "sha256": compute_sha256(&payload) }
        }
    });
    if let Some(config) = response.config_metas.as_mut() {
        config.top_targets = Some(mk_targets_meta_from_value(3, targets_value.clone(), None));
    }
    if let Some(director) = response.director_metas.as_mut() {
        director.targets = Some(mk_targets_meta_from_value(4, targets_value, None));
    }
    server.expect(
        Expectation::matching(request::method_path("POST", "/api/v0.1/configurations"))
            .times(1)
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );

    let service = build_service(&server);
    let req = ClientGetConfigsRequest {
        client: Some(tracer_client("fresh-client")),
        cached_target_files: Vec::new(),
    };

    let result = service.client_get_configs(req).await.unwrap();
    assert_eq!(result.config_status, ConfigStatus::Ok as i32);
    assert_eq!(result.target_files.len(), 1);
    let delivered = &result.target_files[0];
    assert_eq!(delivered.path, SAMPLE_TARGET_PATH);
    assert_eq!(result.client_configs, vec![SAMPLE_TARGET_PATH.to_string()]);

    assert_eq!(delivered.raw, payload);
}

/// Ensures existing clients trigger a bypass refresh when they start reporting products.
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_triggers_bypass_when_products_added() {
    let server = Server::run();
    let payload = br#"{ "z": true, "a": false }"#.to_vec();
    let mut response = sample_response(7, 8);
    response.target_files = vec![File {
        path: SAMPLE_TARGET_PATH.into(),
        raw: payload.clone(),
    }];
    let targets_value = json!({
        (SAMPLE_TARGET_PATH): {
            "length": payload.len(),
            "hashes": { "sha256": compute_sha256(&payload) }
        }
    });
    if let Some(config) = response.config_metas.as_mut() {
        config.top_targets = Some(mk_targets_meta_from_value(7, targets_value.clone(), None));
    }
    if let Some(director) = response.director_metas.as_mut() {
        director.targets = Some(mk_targets_meta_from_value(8, targets_value, None));
    }
    server.expect(
        Expectation::matching(request::method_path("POST", "/api/v0.1/configurations"))
            .times(1)
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );

    let service = build_service(&server);
    register_client_for_test(&service, "delayed-products", &[]).await;

    let request = ClientGetConfigsRequest {
        client: Some(tracer_client("delayed-products")),
        cached_target_files: Vec::new(),
    };

    let result = service.client_get_configs(request).await.unwrap();
    assert_eq!(result.config_status, ConfigStatus::Ok as i32);
    assert_eq!(result.target_files.len(), 1);
    assert_eq!(result.client_configs, vec![SAMPLE_TARGET_PATH.to_string()]);
    assert_eq!(result.target_files[0].raw, payload);
}

/// Confirms the service surfaces validation errors for malformed client requests.
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_requires_client_descriptor() {
    let server = Server::run();
    let service = build_service(&server);
    let err = service
        .client_get_configs(ClientGetConfigsRequest::default())
        .await
        .expect_err("missing client should fail");
    let ServiceError::InvalidRequest(message) = err else {
        panic!("unexpected error variant");
    };
    assert_eq!(
        message,
        "client is a required field for client config update requests"
    );
}

/// Ensures the validator rejects clients missing required state information.
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_enforces_state_requirements() {
    let server = Server::run();
    let service = build_service(&server);

    let mut missing_state = tracer_client("missing-state");
    missing_state.state = None;
    let err = service
        .client_get_configs(ClientGetConfigsRequest {
            client: Some(missing_state),
            cached_target_files: Vec::new(),
        })
        .await
        .expect_err("missing state should fail");
    let ServiceError::InvalidRequest(message) = err else {
        panic!("unexpected error variant");
    };
    assert_eq!(
        message,
        "client.state is a required field for client config update requests"
    );

    let mut invalid_root = tracer_client("invalid-root");
    if let Some(state) = invalid_root.state.as_mut() {
        // Force the client to advertise an invalid root version.
        state.root_version = 0;
    }
    let err = service
        .client_get_configs(ClientGetConfigsRequest {
            client: Some(invalid_root),
            cached_target_files: Vec::new(),
        })
        .await
        .expect_err("invalid root version should fail");
    let ServiceError::InvalidRequest(message) = err else {
        panic!("unexpected error variant");
    };
    assert_eq!(
        message,
        "client.state.root_version must be >= 1 (clients must start with the base TUF director root)"
    );
}

/// Validates that role flags, language, and runtime identifiers mirror the Go checks.
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_enforces_role_validation() {
    let server = Server::run();
    let service = build_service(&server);

    let mut multi_role = tracer_client("multi-role");
    multi_role.is_agent = true;
    multi_role.client_agent = Some(ClientAgent::default());
    let err = service
        .client_get_configs(ClientGetConfigsRequest {
            client: Some(multi_role),
            cached_target_files: Vec::new(),
        })
        .await
        .expect_err("multiple roles should fail");
    let ServiceError::InvalidRequest(message) = err else {
        panic!("unexpected error variant");
    };
    assert_eq!(
        message,
        "client.is_tracer, client.is_agent, and client.is_updater are mutually exclusive"
    );

    let mut no_role = tracer_client("no-role");
    no_role.is_tracer = false;
    no_role.client_tracer = None;
    let err = service
        .client_get_configs(ClientGetConfigsRequest {
            client: Some(no_role),
            cached_target_files: Vec::new(),
        })
        .await
        .expect_err("missing role should fail");
    let ServiceError::InvalidRequest(message) = err else {
        panic!("unexpected error variant");
    };
    assert_eq!(
        message,
        "agents only support remote config updates from tracer or agent or updater at this time"
    );

    let mut missing_language = tracer_client("missing-language");
    if let Some(tracer) = missing_language.client_tracer.as_mut() {
        // Blank out the tracer language to trigger the validation failure.
        tracer.language.clear();
    }
    let err = service
        .client_get_configs(ClientGetConfigsRequest {
            client: Some(missing_language),
            cached_target_files: Vec::new(),
        })
        .await
        .expect_err("missing language should fail");
    let ServiceError::InvalidRequest(message) = err else {
        panic!("unexpected error variant");
    };
    assert_eq!(
        message,
        "client.client_tracer.language is a required field for tracer client config update requests"
    );

    let mut matching_runtime = tracer_client("matching-runtime");
    if let Some(tracer) = matching_runtime.client_tracer.as_mut() {
        // Intentionally mirror the client identifier with the runtime identifier.
        tracer.runtime_id = matching_runtime.id.clone();
    }
    let err = service
        .client_get_configs(ClientGetConfigsRequest {
            client: Some(matching_runtime),
            cached_target_files: Vec::new(),
        })
        .await
        .expect_err("matching runtime id should fail");
    let ServiceError::InvalidRequest(message) = err else {
        panic!("unexpected error variant");
    };
    assert_eq!(
        message,
        "client.id must be different from client.client_tracer.runtime_id"
    );

    let mut tracer_with_agent = tracer_client("tracer-with-agent");
    tracer_with_agent.client_agent = Some(ClientAgent::default());
    let err = service
        .client_get_configs(ClientGetConfigsRequest {
            client: Some(tracer_with_agent),
            cached_target_files: Vec::new(),
        })
        .await
        .expect_err("tracer with agent payload should fail");
    let ServiceError::InvalidRequest(message) = err else {
        panic!("unexpected error variant");
    };
    assert_eq!(
        message,
        "client.client_agent must not be set if client.is_tracer is true"
    );
}

/// Ensures cached target metadata validation mirrors the Go implementation.
#[tokio::test(flavor = "multi_thread")]
async fn client_get_configs_validates_cached_targets() {
    let server = Server::run();
    let service = build_service(&server);

    let valid_hash = TargetFileHash {
        algorithm: "sha256".into(),
        hash: "abc".into(),
    };
    let cases = vec![
        (
            TargetFileMeta {
                path: String::new(),
                length: 1,
                hashes: vec![valid_hash.clone()],
            },
            "cached_target_files[0].path is a required field for client config update requests"
                .to_string(),
        ),
        (
            TargetFileMeta {
                path: "invalid".into(),
                length: 1,
                hashes: vec![valid_hash.clone()],
            },
            "cached_target_files[0].path is not a valid path: config path 'invalid' has unknown source"
                .to_string(),
        ),
        (
            TargetFileMeta {
                path: SAMPLE_TARGET_PATH.into(),
                length: 0,
                hashes: vec![valid_hash.clone()],
            },
            "cached_target_files[0].length must be >= 1 (no empty file allowed)"
                .to_string(),
        ),
        (
            TargetFileMeta {
                path: SAMPLE_TARGET_PATH.into(),
                length: 1,
                hashes: Vec::new(),
            },
            "cached_target_files[0].hashes is a required field for client config update requests"
                .to_string(),
        ),
        (
            TargetFileMeta {
                path: SAMPLE_TARGET_PATH.into(),
                length: 1,
                hashes: vec![TargetFileHash {
                    algorithm: String::new(),
                    hash: "abc".into(),
                }],
            },
            "cached_target_files[0].hashes[0].algorithm is a required field for client config update requests"
                .to_string(),
        ),
        (
            TargetFileMeta {
                path: SAMPLE_TARGET_PATH.into(),
                length: 1,
                hashes: vec![TargetFileHash {
                    algorithm: "sha256".into(),
                    hash: String::new(),
                }],
            },
            "cached_target_files[0].hashes[0].hash is a required field for client config update requests"
                .to_string(),
        ),
    ];

    for (meta, expected) in cases {
        // Exercise each invalid cached target scenario and ensure we mirror the Go error text.
        let err = service
            .client_get_configs(ClientGetConfigsRequest {
                client: Some(tracer_client("cached-validator")),
                cached_target_files: vec![meta],
            })
            .await
            .expect_err("invalid cached entry should fail");
        let ServiceError::InvalidRequest(message) = err else {
            panic!("unexpected error variant");
        };
        assert_eq!(message, expected);
    }
}

/// Confirms the default configuration aligns with Go defaults.
#[test]
fn service_config_default_matches_expectations() {
    let config = ServiceConfig::default();
    assert_eq!(config.cache_bypass_limit, 5);
    assert_eq!(config.clients_ttl, Duration::from_secs(30));
    assert_eq!(config.bypass_block_timeout, Duration::from_secs(2));
    assert_eq!(config.new_client_block_timeout, Duration::from_secs(2));
    assert!(!config.disable_background_poller);
}

#[test]
/// Validates guardrails clamp configuration to Go-aligned defaults.
fn service_config_limits_are_enforced() {
    let server = Server::run();
    let mut config = ServiceConfig::default();
    config.cache_bypass_limit = 0;
    config.clients_ttl = Duration::from_secs(120);
    config.default_refresh_interval = Duration::from_secs(3);
    config.min_refresh_interval = Duration::from_secs(120);
    config.org_status_interval = Duration::from_secs(3);
    config.new_client_block_timeout = Duration::from_secs(0);
    let mut backoff = BackoffConfig::default();
    backoff.max_backoff = Duration::from_secs(600);
    config.backoff_config = backoff;

    let service = build_service_with_config(&server, config);
    let sanitised = service.config();
    assert_eq!(sanitised.cache_bypass_limit, DEFAULT_CACHE_BYPASS_LIMIT);
    assert_eq!(sanitised.clients_ttl, DEFAULT_CLIENTS_TTL);
    assert_eq!(sanitised.default_refresh_interval, DEFAULT_REFRESH_INTERVAL);
    assert!(sanitised.allow_refresh_override);
    assert_eq!(
        sanitised.min_refresh_interval,
        sanitised.default_refresh_interval
    );
    assert_eq!(sanitised.org_status_interval, DEFAULT_REFRESH_INTERVAL);
    assert_eq!(sanitised.backoff_config.max_backoff, MAX_BACKOFF_INTERVAL);
    assert_eq!(sanitised.new_client_block_timeout, Duration::from_secs(2));
}

#[test]
/// Ensures a custom default refresh interval disables backend overrides.
fn custom_refresh_interval_disables_backend_override() {
    let server = Server::run();
    let mut config = ServiceConfig::default();
    config.default_refresh_interval = Duration::from_secs(90);

    let service = build_service_with_config(&server, config);
    let sanitised = service.config();
    assert_eq!(sanitised.default_refresh_interval, Duration::from_secs(90));
    assert!(!sanitised.allow_refresh_override);
}

#[test]
/// Clamps the minimum refresh interval to the crate-level floor.
fn service_config_min_refresh_is_clamped_to_minimum() {
    let server = Server::run();
    let mut config = ServiceConfig::default();
    config.min_refresh_interval = Duration::from_secs(1);

    let service = build_service_with_config(&server, config);
    let sanitised = service.config();
    assert_eq!(sanitised.min_refresh_interval, MIN_REFRESH_INTERVAL);
}

#[test]
/// Enforces the lower bound for the exponential backoff max interval.
fn service_config_backoff_clamps_lower_bound() {
    let server = Server::run();
    let mut config = ServiceConfig::default();
    let mut backoff = BackoffConfig::default();
    backoff.max_backoff = Duration::from_secs(30);
    config.backoff_config = backoff;

    let service = build_service_with_config(&server, config);
    let sanitised = service.config();
    assert_eq!(sanitised.backoff_config.max_backoff, MIN_BACKOFF_INTERVAL);
}

#[test]
/// Leaves user-provided config untouched when Go limit enforcement is disabled.
fn disabling_go_limits_leaves_configuration_unchanged() {
    let server = Server::run();
    let mut config = ServiceConfig::default();
    config.enforce_go_limits = false;
    config.cache_bypass_limit = 0;
    config.clients_ttl = Duration::from_secs(120);
    config.default_refresh_interval = Duration::from_secs(1);
    config.backoff_config.max_backoff = Duration::from_secs(30);

    let service = build_service_with_config(&server, config);
    let sanitised = service.config();
    assert_eq!(sanitised.cache_bypass_limit, 0);
    assert_eq!(sanitised.clients_ttl, Duration::from_secs(120));
    assert_eq!(sanitised.default_refresh_interval, Duration::from_secs(1));
    assert_eq!(
        sanitised.backoff_config.max_backoff,
        Duration::from_secs(30)
    );
}

/// Ensures active clients and backend state are propagated in refresh requests.
#[tokio::test(flavor = "multi_thread")]
async fn build_latest_configs_request_includes_active_clients_and_backend_state() {
    let server = Server::run();
    let service = build_service(&server);

    // Register a client descriptor containing tracer metadata.
    let mut descriptor = tracer_client("client-active");
    if let Some(state) = descriptor.state.as_mut() {
        state.targets_version = 7;
    }
    register_client_for_test(&service, "client-active", &["APM_TRACING"]).await;
    let shared_arc = shared(&service);
    {
        let mut guard = shared_arc.state.lock().await;
        let _ = guard.register_client(&descriptor, service.config().clients_ttl);
    }

    // Compose a request and verify that client metadata and backend state are echoed.
    let backend_state = vec![1, 2, 3, 4];
    let runtime = RuntimeMetadata::from_config(service.config());
    let request = {
        let mut guard = shared_arc.state.lock().await;
        guard.build_latest_configs_request(
            service.config(),
            &runtime,
            TufVersions {
                director_root: 1,
                director_targets: 2,
                config_root: 3,
                config_snapshot: 4,
            },
            backend_state.clone(),
        )
    };

    assert_eq!(request.backend_client_state, backend_state);
    assert_eq!(request.active_clients.len(), 1);
    let active = &request.active_clients[0];
    assert_eq!(active.id, "client-active");
    assert!(!active.products.is_empty());
    assert!(active.last_seen > 0);
}

/// Ensures the trace agent environment propagates into refresh requests.
#[test]
fn build_latest_configs_request_includes_trace_env() {
    let mut config = base_config();
    config.trace_agent_env = Some("trace-env".into());
    let mut state = ServiceState::new(&config);
    let runtime = RuntimeMetadata::from_config(&config);
    let request =
        state.build_latest_configs_request(&config, &runtime, TufVersions::default(), Vec::new());
    assert_eq!(request.trace_agent_env, "trace-env");
}

/// Verifies that force_refresh bypasses throttling.
#[tokio::test(flavor = "multi_thread")]
async fn force_refresh_ignores_spacing() {
    let server = Server::run();
    let response = sample_response(1, 1);
    server.expect(
        Expectation::matching(request::path("/api/v0.1/configurations"))
            .times(2)
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );

    let service = build_service(&server);
    service.refresh_once().await.unwrap();

    let shared_arc = shared(&service);
    {
        let mut guard = shared_arc.state.lock().await;
        guard.last_attempt = Some(Instant::now());
    }

    let outcome = service.force_refresh().await.expect("force refresh works");
    assert!(outcome.next_interval <= Duration::from_secs(5));
}

/// Ensures refresh_products queues new products for the next refresh.
#[tokio::test(flavor = "multi_thread")]
async fn refresh_products_tracks_new_entries() {
    let server = Server::run();
    let service = build_service(&server);
    service
        .refresh_products(vec!["APM_TRACING".to_string()])
        .await;
    let shared_arc = shared(&service);
    {
        let guard = shared_arc.state.lock().await;
        assert!(guard.new_products.contains("APM_TRACING"));
    }
}

/// Confirms client_state returns known clients with durations.
#[tokio::test(flavor = "multi_thread")]
async fn client_state_reports_active_clients() {
    let server = Server::run();
    let service = build_service(&server);
    register_client_for_test(&service, "client-x", &["APM_TRACING"]).await;
    let state = service.client_state().await;
    assert_eq!(state.len(), 1);
    assert_eq!(state[0].id, "client-x");
    assert_eq!(state[0].products, vec!["APM_TRACING".to_string()]);
}

/// Ensures credential rotation updates headers and metadata hashes.
#[tokio::test(flavor = "multi_thread")]
async fn rotate_credentials_updates_headers_and_metadata() {
    let server = Server::run();
    let org_body = OrgDataResponse {
        uuid: "00000000-0000-0000-0000-000000000000".into(),
    }
    .encode_to_vec();
    let status_body = OrgStatusResponse {
        enabled: true,
        authorized: true,
        ..Default::default()
    }
    .encode_to_vec();

    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/api/v0.1/org"),
            request::headers(contains(("dd-api-key", "rotated-key"))),
            request::headers(contains(("dd-application-key", "rotated-app"))),
        ])
        .respond_with(status_code(200).body(org_body)),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/api/v0.1/status"),
            request::headers(contains(("dd-api-key", "rotated-key"))),
            request::headers(contains(("dd-application-key", "rotated-app"))),
            request::headers(contains(("dd-par-jwt", "rotated-jwt"))),
        ])
        .respond_with(status_code(200).body(status_body)),
    );

    let service = build_service(&server);
    let rc_key = encoded_rc_key("rotated-app", 2);
    service
        .rotate_credentials("rotated-key", Some(rc_key.as_str()), Some("rotated-jwt"))
        .await
        .expect("credentials rotate");

    shared(&service)
        .http_client
        .fetch_org_status()
        .await
        .expect("status fetch succeeds");

    let metadata = service
        .uptane_state()
        .store_metadata()
        .expect("metadata available");
    assert_eq!(metadata.api_key_hash, compute_sha256(b"rotated-key"));
    assert_eq!(shared(&service).expected_org_id(), Some(2));
}

/// Ensures the service warns when credential rotation crosses organisations.
#[tokio::test(flavor = "multi_thread")]
async fn rotate_credentials_detects_org_mismatch() {
    let server = Server::run();
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/api/v0.1/org"),
            request::headers(contains(("dd-api-key", "new-key"))),
        ])
        .respond_with(
            status_code(200).body(
                OrgDataResponse {
                    uuid: "org-new".into(),
                }
                .encode_to_vec(),
            ),
        ),
    );

    let service = build_service(&server);
    let shared_arc = shared(&service);
    shared_arc
        .uptane
        .update_org_uuid("org-old")
        .expect("store old uuid");
    {
        let mut guard = shared_arc.state.lock().await;
        guard.org_uuid = Some("org-old".into());
    }

    service
        .rotate_credentials("new-key", None, None)
        .await
        .expect("rotation succeeds despite mismatch");

    let stored = service
        .uptane_state()
        .stored_org_uuid()
        .expect("lookup succeeds");
    assert_eq!(stored.as_deref(), Some("org-old"));
    let guard = shared_arc.state.lock().await;
    assert_eq!(guard.org_uuid.as_deref(), Some("org-old"));
}

/// Ensures clearing the RC key removes org-binding enforcement.
#[tokio::test(flavor = "multi_thread")]
async fn rotate_credentials_clears_expected_org_id() {
    let server = Server::run();
    let org_body = OrgDataResponse {
        uuid: "00000000-0000-0000-0000-000000000000".into(),
    }
    .encode_to_vec();
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/api/v0.1/org"),
            request::headers(contains(("dd-api-key", "rotated-key"))),
            request::headers(contains(("dd-application-key", "rotated-app"))),
        ])
        .respond_with(status_code(200).body(org_body.clone())),
    );
    server.expect(
        Expectation::matching(all_of![
            request::method_path("GET", "/api/v0.1/org"),
            request::headers(contains(("dd-api-key", "rotated-key"))),
            request::headers(not(contains(("dd-application-key", "rotated-app")))),
        ])
        .respond_with(status_code(200).body(org_body)),
    );

    let service = build_service(&server);
    let rc_key = encoded_rc_key("rotated-app", 2);
    service
        .rotate_credentials("rotated-key", Some(rc_key.as_str()), None)
        .await
        .expect("credentials rotate with rc key");
    assert_eq!(shared(&service).expected_org_id(), Some(2));

    service
        .rotate_credentials("rotated-key", None, None)
        .await
        .expect("credentials rotate without rc key");
    assert!(shared(&service).expected_org_id().is_none());
}

/// Confirms cache resets wipe stored state and reset in-memory bookkeeping.
#[tokio::test(flavor = "multi_thread")]
async fn reset_cache_clears_store_and_state() {
    let server = Server::run();
    let response = sample_response(1, 2);
    server.expect(
        Expectation::matching(request::path("/api/v0.1/configurations"))
            .respond_with(status_code(200).body(response.encode_to_vec())),
    );

    let service = build_service(&server);
    service.refresh_once().await.expect("initial refresh");

    // Cache should now contain the target file from the response.
    assert!(!service
        .uptane_state()
        .all_target_files()
        .expect("list target files")
        .is_empty());

    service.reset_cache().await.expect("cache reset");

    // After resetting, the store should be empty and state back to the initial values.
    assert!(service
        .uptane_state()
        .all_target_files()
        .expect("list target files")
        .is_empty());

    let metadata = service
        .uptane_state()
        .store_metadata()
        .expect("metadata available after reset");
    assert_eq!(metadata.api_key_hash, compute_sha256(b"key"));
    assert_eq!(metadata.version, service.config().agent_version);
    assert_eq!(metadata.url, shared(&service).http_client.base_url());

    let snapshot = service.snapshot().await;
    assert!(snapshot.first_refresh_pending);
    assert!(snapshot.last_success.is_none());
}

#[test]
fn director_path_mapping_caches_until_version_changes() {
    let config = base_config();
    let mut state = ServiceState::new(&config);
    let first = serde_json::to_vec(&json!({
        "signed": {
            "targets": {
                "cfg/foo.json": {
                    "hashes": { "sha256": "aaaabbbbccccddddeeeeffff0000111122223333444455556666777788889999" },
                    "length": 10
                }
            }
        }
    }))
    .expect("serialize targets");
    let map_one = state
        .director_path_mapping(1, &first)
        .expect("first mapping builds");
    assert_eq!(
        map_one.get("cfg/foo.json").map(String::as_str),
        Some("cfg/aaaabbbbccccddddeeeeffff0000111122223333444455556666777788889999.foo.json")
    );

    // Reusing the same version should return the cached mapping even if the JSON differs.
    let second = serde_json::to_vec(&json!({
        "signed": {
            "targets": {
                "cfg/bar.json": {
                    "hashes": { "sha256": "bbbbccccddddeeeeffff0000111122223333444455556666777788889999aaaa" },
                    "length": 12
                }
            }
        }
    }))
    .expect("serialize second targets");
    let cached = state
        .director_path_mapping(1, &second)
        .expect("cached mapping available");
    assert!(cached.contains_key("cfg/foo.json"));
    assert!(!cached.contains_key("cfg/bar.json"));

    // Bumping the version should rebuild the mapping with the new content.
    let rebuilt_blob = serde_json::to_vec(&json!({
        "signed": {
            "targets": {
                "cfg/bar.json": {
                    "hashes": { "sha256": "bbbbccccddddeeeeffff0000111122223333444455556666777788889999aaaa" },
                    "length": 12
                }
            }
        }
        }))
        .expect("serialize rebuilt targets");
    let rebuilt = state
        .director_path_mapping(2, &rebuilt_blob)
        .expect("rebuild mapping");
    assert!(rebuilt.contains_key("cfg/bar.json"));
}

/// Config state snapshot exposes Uptane metadata and active clients.
#[tokio::test(flavor = "multi_thread")]
async fn config_state_includes_uptane_and_clients() {
    let server = Server::run();
    let service = build_service(&server);
    let shared_arc = shared(&service);
    let response = sample_response(1, 1);
    shared_arc.uptane.update(&response).unwrap();
    register_client_for_test(&service, "diag-client", &["APM_TRACING"]).await;

    let snapshot = service.config_state().await.expect("config state");
    assert!(
        !snapshot.uptane.target_filenames.is_empty(),
        "uptane state should include target files"
    );
    assert!(
        !snapshot.active_clients.is_empty(),
        "active clients should be populated"
    );
}

/// Reset state reseeds trust anchors and clears diagnostics.
#[tokio::test(flavor = "multi_thread")]
async fn reset_state_clears_store_and_clients() {
    let server = Server::run();
    let service = build_service(&server);
    let shared_arc = shared(&service);
    let response = sample_response(2, 2);
    shared_arc.uptane.update(&response).unwrap();
    register_client_for_test(&service, "client-reset", &["APM_TRACING"]).await;

    let before = service.config_state().await.expect("state before reset");
    assert!(
        !before.uptane.target_filenames.is_empty(),
        "expected populated uptane state before reset"
    );
    assert!(
        !before.active_clients.is_empty(),
        "expected active clients before reset"
    );

    service.reset_state().await.expect("reset succeeds");

    let after = service.config_state().await.expect("state after reset");
    assert!(
        after.uptane.target_filenames.is_empty(),
        "target files should be cleared after reset"
    );
    assert!(
        after.active_clients.is_empty(),
        "active clients should be cleared after reset"
    );
    assert!(
        service.snapshot().await.first_refresh_pending,
        "service state should be reset to first refresh pending"
    );
}

/// Ensures the flush cache response helper returns the expected status.
#[test]
fn flush_cache_response_marks_expired() {
    let response = RemoteConfigService::flush_cache_response();
    assert_eq!(response.config_status, ConfigStatus::Expired as i32);
    assert!(response.roots.is_empty());
}

/// Ensures canonicalisation produces deterministically ordered JSON documents.
#[test]
fn canonicalize_json_bytes_sorts_objects() {
    let payload = json!({
        "b": { "y": 2, "x": 1 },
        "a": [ {"k": 1}, {"j": 0} ]
    })
    .to_string();
    let canonical = canonicalize_json_bytes(payload.as_bytes()).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
    assert_eq!(
        parsed,
        json!({
            "a": [{"k": 1}, {"j": 0}],
            "b": {"x": 1, "y": 2}
        })
    );
}

/// Verifies that cached index construction ignores entries without SHA-256 hashes.
#[test]
fn build_cached_index_ignores_non_sha() {
    let entries = vec![TargetFileMeta {
        path: "pkg".into(),
        length: 10,
        hashes: vec![TargetFileHash {
            algorithm: "md5".into(),
            hash: "dead".into(),
        }],
    }];
    let index = build_cached_index(&entries);
    assert!(index.is_empty());
}

/// Ensures the hashing helper produces the expected digest.
#[test]
fn compute_sha256_matches_known_digest() {
    assert_eq!(
        compute_sha256(b"hello"),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

/// Confirms that the shared service layer forwards telemetry callbacks.
#[tokio::test(flavor = "multi_thread")]
async fn telemetry_hooks_are_invoked() {
    let server = Server::run();
    let service = build_service(&server);
    let telemetry_impl = Arc::new(RecordingTelemetry::default());
    let telemetry: Arc<dyn RemoteConfigTelemetry> = telemetry_impl.clone();
    service.set_telemetry(telemetry).await;

    let shared = shared(&service);
    shared.record_refresh_success(Duration::from_secs(1)).await;
    shared
        .record_refresh_error(&ServiceError::Http(HttpError::Retryable(503)))
        .await;
    shared.record_bypass_rejected().await;
    shared.record_bypass_rate_limited().await;

    assert!(telemetry_impl.success.load(Ordering::Relaxed) > 0);
    assert!(telemetry_impl.error.load(Ordering::Relaxed) > 0);
    assert!(telemetry_impl.bypass_rejected.load(Ordering::Relaxed) > 0);
    assert!(telemetry_impl.bypass_rate_limited.load(Ordering::Relaxed) > 0);
}

/// Spawns a websocket test server that echoes back text frames and enforces the echo contract.
async fn spawn_text_echo_server() -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let mut ws = match accept_async(stream).await {
                Ok(socket) => socket,
                Err(_) => continue,
            };
            if ws.send(WsMessage::Text("hello".into())).await.is_err() {
                continue;
            }
            if let Some(Ok(WsMessage::Text(text))) = ws.next().await {
                assert_eq!(text, "hello");
            }
            if ws.send(WsMessage::Binary(vec![1, 2, 3])).await.is_err() {
                continue;
            }
            if let Some(Ok(WsMessage::Binary(bytes))) = ws.next().await {
                assert_eq!(bytes, vec![1, 2, 3]);
            }
            if ws.send(WsMessage::Ping(vec![9, 9, 9])).await.is_err() {
                continue;
            }
            if let Some(Ok(WsMessage::Pong(bytes))) = ws.next().await {
                assert_eq!(bytes, vec![9, 9, 9]);
            }
            let _ = ws.send(WsMessage::Pong(vec![4, 5, 6])).await;
            let _ = ws.close(None).await;
        }
    });
    (addr, handle)
}

/// Spawns a websocket test server that completes the handshake and closes immediately.
async fn spawn_closing_server() -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            if let Ok(mut ws) = accept_async(stream).await {
                let _ = ws.close(None).await;
            }
        }
    });
    (addr, handle)
}

/// Spawns a websocket server that accepts connections but never sends frames.
async fn spawn_idle_server() -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                if let Ok(mut ws) = accept_async(stream).await {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    let _ = ws.close(None).await;
                }
            });
        }
    });
    (addr, handle)
}

/// Validates that the websocket echo task reports success and records telemetry.
#[tokio::test(flavor = "multi_thread")]
async fn websocket_echo_task_reports_success() {
    let (addr, server) = spawn_text_echo_server().await;
    let mut config = base_config();
    config.enable_websocket_echo = true;
    config.disable_background_poller = true;
    config.org_status_interval = Duration::from_secs(300);
    config.websocket_echo_interval = Duration::from_millis(50);
    config.websocket_echo_timeout = Duration::from_secs(2);
    let service = build_service_with_base_url(format!("http://{}", addr), config);
    let telemetry = Arc::new(RecordingTelemetry::default());
    service.set_telemetry(telemetry.clone()).await;

    let handle = service.start().await;
    let awaited = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = service.snapshot().await;
            if snapshot.websocket_last_result == Some(WebsocketCheckResult::Success) {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let snapshot = match awaited {
        Ok(snapshot) => snapshot,
        Err(_) => {
            let final_snapshot = service.snapshot().await;
            panic!(
                "websocket success not observed within timeout; last error: {:?}",
                final_snapshot.websocket_last_error
            );
        }
    };
    handle.shutdown().await;
    server.abort();
    let _ = server.await;

    assert_eq!(
        snapshot.websocket_last_result,
        Some(WebsocketCheckResult::Success)
    );
    assert!(
        snapshot.websocket_last_error.is_none(),
        "expected no websocket error, found {:?}",
        snapshot.websocket_last_error
    );
    assert!(telemetry.websocket_checks.load(Ordering::Relaxed) > 0);
}

/// Ensures the websocket echo task records failures when the endpoint is unreachable.
#[tokio::test(flavor = "multi_thread")]
async fn websocket_echo_records_failures() {
    let mut config = base_config();
    config.enable_websocket_echo = true;
    config.disable_background_poller = true;
    config.org_status_interval = Duration::from_secs(300);
    config.websocket_echo_interval = Duration::from_secs(60);
    config.websocket_echo_timeout = Duration::from_millis(100);
    let service = build_service_with_base_url("http://127.0.0.1:9".into(), config);
    let telemetry = Arc::new(RecordingTelemetry::default());
    service.set_telemetry(telemetry.clone()).await;

    let handle = service.start().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let snapshot = service.snapshot().await;
    handle.shutdown().await;

    assert_eq!(
        snapshot.websocket_last_result,
        Some(WebsocketCheckResult::Failure)
    );
    assert!(
        snapshot.websocket_last_error.is_some(),
        "expected websocket error to be recorded"
    );
    assert!(telemetry.websocket_checks.load(Ordering::Relaxed) > 0);
}

/// Ensures the HTTP client can establish a websocket session with propagated headers.
#[tokio::test(flavor = "multi_thread")]
async fn websocket_request_establishes_connection() {
    let (addr, server) = spawn_closing_server().await;

    let auth = crate::http::Auth {
        api_key: "key".into(),
        application_key: None,
        par_jwt: None,
    };
    let client = crate::http::HttpClient::new(
        format!("http://{}", addr),
        &auth,
        "7.70.0",
        crate::http::HttpClientOptions {
            allow_plaintext: true,
            accept_invalid_certs: true,
        },
    )
    .unwrap();
    let request = client
        .websocket_request("/api/v0.2/echo-test")
        .await
        .unwrap();
    let outcome = connect_async(request).await;
    assert!(
        outcome.is_ok(),
        "websocket connect failed: {:?}",
        outcome.err()
    );

    if let Ok((mut stream, _)) = outcome {
        stream.close(None).await.ok();
    }
    server.abort();
    let _ = server.await;
}

/// Validates that the websocket echo session helper succeeds end-to-end.
#[tokio::test(flavor = "multi_thread")]
async fn websocket_echo_session_returns_success() {
    let (addr, server) = spawn_text_echo_server().await;

    let mut config = base_config();
    config.enable_websocket_echo = true;
    config.disable_background_poller = true;
    config.org_status_interval = Duration::from_secs(300);
    config.websocket_echo_interval = Duration::from_secs(60);
    config.websocket_echo_timeout = Duration::from_secs(2);
    let service = build_service_with_base_url(format!("http://{}", addr), config);
    let result = shared(&service).websocket_echo_session().await;
    assert!(
        result.is_ok(),
        "websocket echo session failed: {:?}",
        result.err()
    );
    server.abort();
    let _ = server.await;
}

/// Ensures the websocket echo session surfaces idle timeouts when the server is silent.
#[tokio::test(flavor = "multi_thread")]
async fn websocket_echo_session_times_out_when_idle() {
    let (addr, server) = spawn_idle_server().await;

    let mut config = base_config();
    config.enable_websocket_echo = true;
    config.disable_background_poller = true;
    config.org_status_interval = Duration::from_secs(300);
    config.websocket_echo_interval = Duration::from_secs(60);
    config.websocket_echo_timeout = Duration::from_secs(5);
    let service = build_service_with_base_url(format!("http://{}", addr), config);
    let result = shared(&service).websocket_echo_session().await;
    assert!(
        matches!(result, Err(ref err) if err.contains("without receiving a frame")),
        "expected idle timeout error, got {:?}",
        result
    );
    server.abort();
    let _ = server.await;
}
