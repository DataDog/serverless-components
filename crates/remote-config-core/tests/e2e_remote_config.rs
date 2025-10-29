//! End-to-end integration test covering the Remote Config service lifecycle.

mod common;

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use common::fixtures::{
    org_data_response, org_status_response, tracer_client, ResponseBuilder, TargetFixture,
    SAMPLE_SECOND_TARGET_PATH, SAMPLE_TARGET_PATH,
};
use common::tuf::{build_signed_repo, TargetData};
use data_encoding::BASE32_NOPAD;
use futures_util::StreamExt;
use prost::Message;
use remote_config_core::bootstrap::{handle_client_request, RemoteConfigHost};
use remote_config_core::http::{Auth, HttpClient, HttpClientOptions};
use remote_config_core::service::{
    RemoteConfigHandle, RemoteConfigService, ServiceConfig, WebsocketCheckResult,
};
use remote_config_core::store::RcStore;
use remote_config_core::telemetry::{CountingTelemetry, TelemetryCounters};
use remote_config_core::uptane::UptaneState;
use remote_config_proto::remoteconfig::{
    ClientGetConfigsRequest, ConfigStatus, LatestConfigsRequest, LatestConfigsResponse,
};
use serde::Serialize;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{sleep, Instant};

/// Shared backend state storing scripted responses and collected requests.
#[derive(Default)]
struct BackendState {
    /// Queue of configuration responses served to `/api/v0.1/configurations`.
    config_responses: VecDeque<LatestConfigsResponse>,
    /// Queue of organisation data responses served to `/api/v0.1/org`.
    org_data_responses: VecDeque<remote_config_proto::remoteconfig::OrgDataResponse>,
    /// Queue of organisation status responses served to `/api/v0.1/org`.
    org_status_responses: VecDeque<remote_config_proto::remoteconfig::OrgStatusResponse>,
    /// Log of requests received by the backend.
    request_log: Vec<RecordedRequest>,
    /// Decoded configuration requests captured for assertions.
    config_requests: Vec<LatestConfigsRequest>,
    /// Active websocket mode.
    websocket_mode: WebsocketMode,
}

/// Captures the HTTP requests the service sends to the backend.
#[derive(Debug, Clone)]
struct RecordedRequest {
    /// Path component extracted from the request URI.
    path: String,
    /// Map of header names to their last observed value.
    headers: HashMap<String, String>,
    /// Raw request payload (protobuf encoded).
    _body: Vec<u8>,
}

/// Encodes the legacy RC key payload the same way production hosts provide it.
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

/// Mode used by the websocket harness to control handshake outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebsocketMode {
    /// The websocket handshake and echo exchange succeed.
    Success,
    /// The handshake receives a terminal failure (HTTP 500).
    FailHandshake,
}

impl Default for WebsocketMode {
    /// Defaults to successful websocket behaviour so tests opt into failures explicitly.
    fn default() -> Self {
        WebsocketMode::Success
    }
}

/// Type alias for the shared backend state.
type SharedState = Arc<Mutex<BackendState>>;

/// Backend harness implementing the Remote Config HTTP and websocket APIs.
struct BackendHarness {
    /// Base URL used by the service under test.
    base_url: String,
    /// Shared mutable state storing scripted responses and logs.
    state: SharedState,
    /// Signal used to terminate the HTTP server.
    shutdown_tx: Option<oneshot::Sender<()>>,
    /// Join handle for the HTTP server task.
    handle: Option<JoinHandle<()>>,
}

impl BackendHarness {
    /// Spawns the backend harness and returns a handle for scripting scenarios.
    async fn start() -> Self {
        let state = Arc::new(Mutex::new(BackendState {
            websocket_mode: WebsocketMode::Success,
            ..BackendState::default()
        }));
        let router = Router::new()
            .route("/api/v0.1/configurations", post(handle_configurations))
            .route("/api/v0.1/org", get(handle_org_data))
            .route("/api/v0.1/status", get(handle_org_status))
            .route("/api/v0.2/echo-test", get(handle_websocket))
            .fallback(handle_not_found)
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("backend bind");
        let addr = listener.local_addr().expect("backend address");
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let server = async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("server run");
        };

        let handle = tokio::spawn(server);

        Self {
            base_url: format!("http://{}", addr),
            state,
            shutdown_tx: Some(shutdown_tx),
            handle: Some(handle),
        }
    }

    /// Returns the base URL the Remote Config service should use.
    fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Queues a configuration response that will be served to the next poll.
    async fn enqueue_config_response(&self, response: LatestConfigsResponse) {
        let mut guard = self.state.lock().await;
        guard.config_responses.push_back(response);
    }

    /// Queues an organisation data response used during credential validation.
    async fn enqueue_org_data_response(
        &self,
        response: remote_config_proto::remoteconfig::OrgDataResponse,
    ) {
        let mut guard = self.state.lock().await;
        guard.org_data_responses.push_back(response);
    }

    /// Queues an organisation status response controlling enablement checks.
    async fn enqueue_org_status_response(
        &self,
        response: remote_config_proto::remoteconfig::OrgStatusResponse,
    ) {
        let mut guard = self.state.lock().await;
        guard.org_status_responses.push_back(response);
    }

    /// Clears request logs collected so far.
    async fn reset_logs(&self) {
        let mut guard = self.state.lock().await;
        guard.request_log.clear();
        guard.config_requests.clear();
    }

    /// Records the desired websocket behaviour for upcoming exchanges.
    async fn set_websocket_mode(&self, mode: WebsocketMode) {
        let mut guard = self.state.lock().await;
        guard.websocket_mode = mode;
    }

    /// Returns all configuration requests observed since the last reset.
    async fn take_config_requests(&self) -> Vec<LatestConfigsRequest> {
        let mut guard = self.state.lock().await;
        std::mem::take(&mut guard.config_requests)
    }

    /// Returns all logged HTTP requests observed so far.
    async fn take_request_log(&self) -> Vec<RecordedRequest> {
        let mut guard = self.state.lock().await;
        std::mem::take(&mut guard.request_log)
    }
}

impl Drop for BackendHarness {
    /// Tears down the backend listener tasks when the harness goes out of scope.
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// Handles configuration fetches from the Remote Config service.
async fn handle_configurations(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let mut guard = state.lock().await;
    let headers_map = headers_to_map(&headers);
    if let Ok(request) = LatestConfigsRequest::decode(body.clone()) {
        // Preserve decoded payloads so assertions can inspect the exact client metadata.
        guard.config_requests.push(request);
    }
    guard.request_log.push(RecordedRequest {
        path: "/api/v0.1/configurations".to_string(),
        headers: headers_map,
        _body: body.to_vec(),
    });
    let payload = guard
        .config_responses
        .pop_front()
        .unwrap_or_default()
        .encode_to_vec();
    drop(guard);

    (
        [(axum::http::header::CONTENT_TYPE, "application/x-protobuf")],
        payload,
    )
}

/// Handles organisation data lookups used to confirm credential ownership.
async fn handle_org_data(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let mut guard = state.lock().await;
    guard.request_log.push(RecordedRequest {
        path: "/api/v0.1/org".to_string(),
        headers: headers_to_map(&headers),
        _body: Vec::new(),
    });
    guard
        .org_data_responses
        .pop_front()
        .unwrap_or_else(|| org_data_response("missing"))
        .encode_to_vec()
}

/// Handles organisation status polling to simulate enablement transitions.
async fn handle_org_status(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let mut guard = state.lock().await;
    guard.request_log.push(RecordedRequest {
        path: "/api/v0.1/org".to_string(),
        headers: headers_to_map(&headers),
        _body: Vec::new(),
    });
    guard
        .org_status_responses
        .pop_front()
        .unwrap_or_else(|| org_status_response(true, true))
        .encode_to_vec()
}

/// Handles websocket echo checks performed by the Remote Config service.
async fn handle_websocket(
    State(state): State<SharedState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let mode = { state.lock().await.websocket_mode };
    if mode == WebsocketMode::FailHandshake {
        // Simulate a backend outage so the service exercises its failure telemetry.
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let log_headers = headers_to_map(&headers);
    {
        let mut guard = state.lock().await;
        guard.request_log.push(RecordedRequest {
            path: "/api/v0.2/echo-test".to_string(),
            headers: log_headers,
            _body: Vec::new(),
        });
    }

    ws.on_upgrade(|socket| async move {
        if let Err(err) = websocket_echo(socket).await {
            eprintln!("websocket echo failed: {err}");
        }
    })
}

/// Executes the websocket echo exchange to validate end-to-end behaviour.
async fn websocket_echo(mut socket: WebSocket) -> Result<(), String> {
    socket
        .send(WsMessage::Text("hello".into()))
        .await
        .map_err(|err| err.to_string())?;
    if let Some(Ok(message)) = socket.next().await {
        match message {
            WsMessage::Text(text) if text == "hello" => {}
            other => return Err(format!("unexpected websocket text frame: {other:?}")),
        }
    }
    socket
        .send(WsMessage::Binary(vec![1, 2, 3]))
        .await
        .map_err(|err| err.to_string())?;
    if let Some(Ok(message)) = socket.next().await {
        match message {
            WsMessage::Binary(bytes) if bytes == vec![1, 2, 3] => {}
            other => return Err(format!("unexpected websocket binary frame: {other:?}")),
        }
    }
    socket
        .send(WsMessage::Ping(vec![9, 9, 9]))
        .await
        .map_err(|err| err.to_string())?;
    if let Some(Ok(message)) = socket.next().await {
        match message {
            WsMessage::Pong(bytes) if bytes == vec![9, 9, 9] => {}
            other => return Err(format!("unexpected websocket pong frame: {other:?}")),
        }
    }
    socket.close().await.ok();
    Ok(())
}

/// Records unmatched routes to aid debugging of unexpected backend calls.
async fn handle_not_found(
    State(state): State<SharedState>,
    req: Request<Body>,
) -> impl IntoResponse {
    let (parts, _) = req.into_parts();
    let mut guard = state.lock().await;
    guard.request_log.push(RecordedRequest {
        path: parts.uri.path().to_string(),
        headers: headers_to_map(&parts.headers),
        _body: Vec::new(),
    });
    StatusCode::NOT_FOUND
}

/// Converts hyper-style headers into a plain string map for request logging.
fn headers_to_map(headers: &HeaderMap) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (name, value) in headers.iter() {
        if let Ok(value_str) = value.to_str() {
            map.insert(name.as_str().to_ascii_lowercase(), value_str.to_string());
        }
    }
    map
}

/// Waits for an asynchronous condition to succeed within the supplied timeout.
async fn wait_for_condition<T, F, Fut>(timeout: Duration, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        if Instant::now() >= deadline {
            panic!("condition not satisfied within {:?}", timeout);
        }
        // Yield to the scheduler briefly before probing again to avoid tight busy loops.
        sleep(Duration::from_millis(20)).await;
    }
}

/// Host implementation embedding the service under test.
struct TestHost {
    /// Remote Config service instance.
    service: RemoteConfigService,
    /// Telemetry collector sharing counters with the test.
    telemetry: Arc<CountingTelemetry>,
    /// Handle managing background tasks.
    handle: RemoteConfigHandle,
}

impl RemoteConfigHost for TestHost {
    /// Exposes the hosted service for helper functions.
    fn remote_config_service(&self) -> &RemoteConfigService {
        &self.service
    }
}

/// Creates a Remote Config host with the supplied backend and environment configuration.
/// Builds a Remote Config service instance backed by the in-process backend double.
async fn bootstrap_service(
    backend: &BackendHarness,
    store_dir: &Path,
    config_overrides: impl FnOnce(&mut ServiceConfig),
) -> TestHost {
    let mut config = ServiceConfig::default();
    config.hostname = "test-host".into();
    config.agent_version = "7.60.0".into();
    config.agent_uuid = "rust-agent".into();
    config.tags = vec!["env:test".into()];
    config.default_refresh_interval = Duration::from_millis(200);
    config.min_refresh_interval = Duration::from_millis(50);
    config.org_status_interval = Duration::from_millis(200);
    config.websocket_echo_interval = Duration::from_millis(200);
    config.websocket_echo_timeout = Duration::from_millis(200);
    config.disable_background_poller = false;
    config.enforce_go_limits = false;
    // Allow the caller to tweak knobs required for each scenario (e.g., override UUID).
    config_overrides(&mut config);

    let db_path = store_dir.join("remote-config.db");
    let store = RcStore::open(
        &db_path,
        &config.agent_version,
        "api-key",
        backend.base_url(),
    )
    .expect("open remote-config store");
    let uptane = UptaneState::new(store).expect("uptane state");
    let empty_targets: [TargetData<'static>; 0] = [];
    let (config_roots, _, _, _) =
        build_signed_repo(1, "2030-01-01T00:00:00Z", &empty_targets, None);
    let (director_roots, _, _, _) =
        build_signed_repo(1, "2030-01-01T00:00:00Z", &empty_targets, None);
    let config_root = config_roots
        .last()
        .expect("config root available")
        .raw
        .clone();
    let director_root = director_roots
        .last()
        .expect("director root available")
        .raw
        .clone();
    uptane
        .seed_trust_roots(&config_root, &director_root)
        .expect("seed trust anchors");
    let auth = Auth {
        api_key: "api-key".into(),
        application_key: None,
        par_jwt: None,
    };
    let http = HttpClient::new(
        backend.base_url().to_string(),
        &auth,
        "7.70.0",
        HttpClientOptions {
            allow_plaintext: true,
            accept_invalid_certs: true,
        },
    )
    .expect("http client initialisation");
    let service = RemoteConfigService::new(http, uptane, config);

    let telemetry = Arc::new(CountingTelemetry::new(Arc::new(
        TelemetryCounters::default(),
    )));
    service.set_telemetry(telemetry.clone()).await;
    let handle = service.start().await;

    TestHost {
        service,
        telemetry,
        handle,
    }
}

/// Validates the entire Remote Config lifecycle against the scripted backend.
#[tokio::test(flavor = "multi_thread")]
async fn remote_config_end_to_end() {
    let backend = BackendHarness::start().await;
    let store_dir = tempfile::TempDir::new().expect("store dir");

    // Step 1: enqueue baseline responses so the initial refresh succeeds.
    backend
        .enqueue_org_data_response(org_data_response("org-123"))
        .await;
    backend
        .enqueue_org_status_response(org_status_response(true, true))
        .await;
    backend
        .enqueue_config_response(ResponseBuilder::new(1, 1).build())
        .await;

    let host = bootstrap_service(&backend, store_dir.path(), |config| {
        config.agent_uuid = "rust-agent".into();
    })
    .await;

    // Wait until the first refresh completes to ensure Uptane state is primed.
    let service = host.service.clone();
    let initial_snapshot = wait_for_condition(Duration::from_secs(5), || {
        let service = service.clone();
        async move {
            let snapshot = service.snapshot().await;
            if snapshot.first_refresh_pending {
                None
            } else {
                Some(snapshot)
            }
        }
    })
    .await;
    assert!(
        initial_snapshot.last_success.is_some(),
        "initial refresh should have succeeded"
    );
    let telemetry_after_refresh = host.telemetry.counters().snapshot();
    assert!(
        telemetry_after_refresh.refresh_success >= 1,
        "expected at least one refresh success during bootstrap"
    );

    // Confirm target files were cached on disk.
    let cached_payload = host
        .service
        .uptane_state()
        .target_file(SAMPLE_TARGET_PATH)
        .unwrap()
        .expect("target payload should exist");
    assert!(
        !cached_payload.is_empty(),
        "target payload is unexpectedly empty"
    );

    // Validate the backend observed credential headers.
    let request_log = backend.take_request_log().await;
    let config_call = request_log
        .iter()
        .find(|entry| entry.path == "/api/v0.1/configurations")
        .expect("configuration call");
    assert_eq!(
        config_call.headers.get("dd-api-key").map(String::as_str),
        Some("api-key")
    );
    assert!(
        config_call.headers.get("dd-application-key").is_none(),
        "application key header should be absent when not configured"
    );

    // Step 2: serve a tracer request and ensure configuration material is delivered.
    let base_request = ClientGetConfigsRequest {
        client: Some(tracer_client("e2e-tracer")),
        cached_target_files: Vec::new(),
    };
    let base_response = handle_client_request(&host, base_request.clone())
        .await
        .expect("client request");
    assert_eq!(
        base_response.config_status,
        ConfigStatus::Ok as i32,
        "expected configs to apply"
    );
    assert_eq!(
        base_response.target_files.len(),
        1,
        "expected a single target payload"
    );
    assert_eq!(
        base_response.client_configs,
        vec![SAMPLE_TARGET_PATH.to_string()]
    );

    let versions = host
        .service
        .uptane_state()
        .tuf_versions()
        .expect("versions available");

    // Step 3: simulate a tracer already holding the current targets version.
    let mut cached_client = tracer_client("cached-tracer");
    if let Some(state) = cached_client.state.as_mut() {
        state.targets_version = versions.director_targets;
    }
    let cached_response = handle_client_request(
        &host,
        ClientGetConfigsRequest {
            client: Some(cached_client),
            cached_target_files: Vec::new(),
        },
    )
    .await
    .expect("cached request result");
    assert!(
        cached_response.target_files.is_empty(),
        "no target files should have been returned"
    );

    // Step 4: deliver metadata with expired timestamps and ensure the flush response is emitted.
    backend
        .enqueue_config_response(
            ResponseBuilder::new(2, 2)
                .with_config_timestamp("2000-01-01T00:00:00Z")
                .with_director_timestamp("2000-01-01T00:00:00Z")
                .build(),
        )
        .await;
    host.service
        .refresh_once()
        .await
        .expect("expired refresh should succeed");
    let mut stale_client = tracer_client("stale-tracer");
    if let Some(state) = stale_client.state.as_mut() {
        state.targets_version = 1;
    }
    let flush_response = handle_client_request(
        &host,
        ClientGetConfigsRequest {
            client: Some(stale_client),
            cached_target_files: Vec::new(),
        },
    )
    .await
    .expect("flush response");
    assert_eq!(
        flush_response.config_status,
        ConfigStatus::Expired as i32,
        "expected timestamp expiry to trigger flush"
    );
    assert!(
        flush_response.target_files.is_empty(),
        "no payloads should accompany flush responses"
    );

    // Step 5: emulate a forced refresh by setting the backend refresh override.
    backend.reset_logs().await;
    backend
        .enqueue_config_response(ResponseBuilder::new(3, 3).with_refresh_override(1).build())
        .await;
    let bypass_response = handle_client_request(
        &host,
        ClientGetConfigsRequest {
            client: Some(tracer_client("bypass-tracer")),
            cached_target_files: Vec::new(),
        },
    )
    .await
    .expect("bypass response");
    assert_eq!(
        bypass_response.config_status,
        ConfigStatus::Ok as i32,
        "bypass delivery should include configs"
    );
    let telemetry_clone = host.telemetry.clone();
    let counters_after_bypass = wait_for_condition(Duration::from_secs(1), move || {
        let telemetry = telemetry_clone.clone();
        async move {
            let counters = telemetry.counters().snapshot();
            if counters.bypass_enqueued > 0 {
                Some(counters)
            } else {
                None
            }
        }
    })
    .await;
    assert!(
        counters_after_bypass.refresh_success >= 2,
        "refresh counter should register bypass-triggered refresh"
    );
    let bypass_requests = backend.take_config_requests().await;
    assert!(
        !bypass_requests.is_empty(),
        "bypass should have triggered a backend fetch"
    );

    // Step 6: supply predicate-guarded targets and confirm selective delivery.
    let payload_rust = br#"{ "alpha": true }"#.to_vec();
    let payload_python = br#"{ "beta": true }"#.to_vec();
    let predicate_response = ResponseBuilder::new(4, 4)
        .with_targets(vec![
            TargetFixture {
                path: SAMPLE_TARGET_PATH,
                payload: payload_rust.clone(),
                custom: Some(json!({
                    "tracer-predicates": { "tracer_predicates_v1": [ { "language": "rust" } ] },
                    "expires": 4_102_444_800i64
                })),
            },
            TargetFixture {
                path: SAMPLE_SECOND_TARGET_PATH,
                payload: payload_python.clone(),
                custom: Some(json!({
                    "tracer-predicates": { "tracer_predicates_v1": [ { "language": "python" } ] },
                    "expires": 4_102_444_800i64
                })),
            },
        ])
        .build();
    backend.enqueue_config_response(predicate_response).await;
    let service_clone = host.service.clone();
    wait_for_condition(Duration::from_secs(3), || {
        let service = service_clone.clone();
        async move {
            match service.uptane_state().tuf_versions() {
                Ok(versions) if versions.director_targets >= 4 => Some(()),
                _ => None,
            }
        }
    })
    .await;

    let predicate_match = handle_client_request(
        &host,
        ClientGetConfigsRequest {
            client: Some(tracer_client("rusty")),
            cached_target_files: Vec::new(),
        },
    )
    .await
    .expect("predicate match");
    assert_eq!(
        predicate_match.client_configs,
        vec![SAMPLE_TARGET_PATH.to_string()]
    );
    assert_eq!(
        predicate_match.target_files.len(),
        1,
        "target payload list mismatch for matching predicate"
    );

    let mut python_client = tracer_client("python");
    if let Some(tracer) = python_client.client_tracer.as_mut() {
        tracer.language = "python".into();
    }
    let predicate_mismatch = handle_client_request(
        &host,
        ClientGetConfigsRequest {
            client: Some(python_client),
            cached_target_files: Vec::new(),
        },
    )
    .await
    .expect("predicate mismatch");
    assert_eq!(
        predicate_mismatch.client_configs,
        vec![SAMPLE_SECOND_TARGET_PATH.to_string()],
        "python tracer should only receive python-targeted configs"
    );
    assert_eq!(
        predicate_mismatch.target_files.len(),
        1,
        "python tracer should receive exactly one targeted payload"
    );
    assert_eq!(
        predicate_mismatch.target_files[0].path, SAMPLE_SECOND_TARGET_PATH,
        "python tracer should receive the python-targeted path"
    );

    // Step 7: provide cached target metadata so the service can skip retransmission.
    let canonical_payload = host
        .service
        .uptane_state()
        .target_file(SAMPLE_TARGET_PATH)
        .unwrap()
        .expect("canonical payload exists");
    let cached_meta = common::fixtures::target_meta_from_fixture(&TargetFixture {
        path: SAMPLE_TARGET_PATH,
        payload: canonical_payload.clone(),
        custom: None,
    });
    let cached_skip = handle_client_request(
        &host,
        ClientGetConfigsRequest {
            client: Some(tracer_client("cache-skipper")),
            cached_target_files: vec![cached_meta],
        },
    )
    .await
    .expect("cached skip");
    assert!(
        cached_skip.target_files.is_empty(),
        "cached targets should prevent retransmission"
    );

    // Step 8: toggle organisation status to disabled and back to enabled.
    backend
        .enqueue_org_status_response(org_status_response(false, true))
        .await;
    let service_clone = host.service.clone();
    let disabled_snapshot = wait_for_condition(Duration::from_secs(2), || {
        let service = service_clone.clone();
        async move {
            let snapshot = service.snapshot().await;
            match snapshot.org_status {
                // Only exit once the backend reports the org as disabled.
                Some(status) if !status.enabled => Some(snapshot),
                _ => None,
            }
        }
    })
    .await;
    assert!(
        matches!(disabled_snapshot.org_status, Some(status) if !status.enabled),
        "expected org status to report disabled"
    );

    backend
        .enqueue_org_status_response(org_status_response(true, true))
        .await;
    let service_clone = host.service.clone();
    let enabled_snapshot = wait_for_condition(Duration::from_secs(2), || {
        let service = service_clone.clone();
        async move {
            let snapshot = service.snapshot().await;
            match snapshot.org_status {
                // Wait until the backend signals that the org has been re-enabled.
                Some(status) if status.enabled => Some(snapshot),
                _ => None,
            }
        }
    })
    .await;
    assert!(
        matches!(enabled_snapshot.org_status, Some(status) if status.enabled),
        "organisation status failed to recover"
    );

    // Step 9: rotate credentials with both matching and mismatched organisation UUIDs.
    backend
        .enqueue_org_data_response(org_data_response("org-123"))
        .await;
    let rc_key = encoded_rc_key("rotated-app", 2);
    host.service
        .rotate_credentials("rotated-key", Some(rc_key.as_str()), None)
        .await
        .expect("rotation should succeed");

    backend
        .enqueue_org_data_response(org_data_response("org-mismatch"))
        .await;
    host.service
        .rotate_credentials("mismatched-key", Some(rc_key.as_str()), None)
        .await
        .expect("rotation should succeed even when org differs");
    let stored_uuid = host
        .service
        .uptane_state()
        .stored_org_uuid()
        .expect("stored uuid");
    assert_eq!(
        stored_uuid.as_deref(),
        Some("org-123"),
        "org uuid should remain unchanged on mismatch"
    );

    // Step 10: ensure websocket diagnostics record both success and failure modes.
    backend.set_websocket_mode(WebsocketMode::Success).await;
    let service_clone = host.service.clone();
    let success_snapshot = wait_for_condition(Duration::from_secs(2), || {
        let service = service_clone.clone();
        async move {
            let snapshot = service.snapshot().await;
            match snapshot.websocket_last_result {
                Some(WebsocketCheckResult::Success) => Some(snapshot),
                _ => None,
            }
        }
    })
    .await;
    assert_eq!(
        success_snapshot.websocket_last_result,
        Some(WebsocketCheckResult::Success)
    );

    backend
        .set_websocket_mode(WebsocketMode::FailHandshake)
        .await;
    let service_clone = host.service.clone();
    let failure_snapshot = wait_for_condition(Duration::from_secs(2), || {
        let service = service_clone.clone();
        async move {
            let snapshot = service.snapshot().await;
            match snapshot.websocket_last_result {
                Some(WebsocketCheckResult::Failure) => Some(snapshot),
                _ => None,
            }
        }
    })
    .await;
    assert_eq!(
        failure_snapshot.websocket_last_result,
        Some(WebsocketCheckResult::Failure)
    );

    // Step 11: shut down the service and rehydrate it using the same persistent store.
    let persisted_payload = host
        .service
        .uptane_state()
        .target_file(SAMPLE_TARGET_PATH)
        .unwrap()
        .expect("persisted payload");
    host.handle.shutdown().await;

    backend
        .enqueue_org_data_response(org_data_response("org-123"))
        .await;
    backend
        .enqueue_org_status_response(org_status_response(true, true))
        .await;
    backend
        .enqueue_config_response(
            ResponseBuilder::new(5, 5)
                .with_targets(vec![TargetFixture {
                    path: SAMPLE_TARGET_PATH,
                    payload: canonical_payload.clone(),
                    custom: None,
                }])
                .build(),
        )
        .await;

    let host2 = bootstrap_service(&backend, store_dir.path(), |config| {
        config.agent_uuid = "rust-agent".into();
    })
    .await;

    let service_clone = host2.service.clone();
    let snapshot2 = wait_for_condition(Duration::from_secs(3), || {
        let service = service_clone.clone();
        async move {
            let snapshot = service.snapshot().await;
            if snapshot.first_refresh_pending {
                None
            } else {
                Some(snapshot)
            }
        }
    })
    .await;
    assert!(
        snapshot2.last_success.is_some(),
        "rehydrated service failed to refresh"
    );
    let persisted_after_restart = host2
        .service
        .uptane_state()
        .target_file(SAMPLE_TARGET_PATH)
        .unwrap()
        .expect("restart target");
    assert_eq!(
        persisted_after_restart, persisted_payload,
        "persisted target payload changed unexpectedly after restart"
    );
    host2.handle.shutdown().await;
}
