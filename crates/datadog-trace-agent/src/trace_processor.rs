// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use async_trait::async_trait;
use datadog_agent_trace_sampler::{
    ErrorSamplerConfig, ErrorsSampler, SampleDecision, SpanView, TraceView,
};
use http_body_util::BodyExt;
use hyper::{StatusCode, http};
use libdd_common::http_common;
use libdd_library_config::tracer_metadata::TracerMetadata;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, warn};

use libdd_trace_obfuscation::obfuscate::obfuscate_span;
use libdd_trace_protobuf::pb;
use libdd_trace_utils::trace_utils::{self};
use libdd_trace_utils::trace_utils::{EnvironmentType, SendData};
use libdd_trace_utils::tracer_payload::{TraceChunkProcessor, TracerPayloadCollection};
use prost::Message;

use crate::{
    aggregator::MAX_CONTENT_SIZE_BYTES,
    config::Config,
    http_utils::{self, log_and_create_http_response, log_and_create_traces_success_http_response},
    stats_concentrator_service::StatsConcentratorHandle,
};

const TRACER_PAYLOAD_FUNCTION_TAGS_TAG_KEY: &str = "_dd.tags.function";

/// The root-span metric used for error rescue without priority promotion.
/// A positive value signals error sampling; backend retention also depends on
/// its ingestion-reason handling (for example, probabilistic decision makers).
const ERRORS_SR_METRIC_KEY: &str = "_dd.errors_sr";
const DECISION_MAKER_KEY: &str = "_dd.p.dm";
const MANUAL_DECISION_MAKER: &str = "-4";

/// Rough upper bound on the protobuf framing overhead added when a V07 `TracerPayload` is
/// wrapped in the outer `AgentPayload` envelope before being sent
const V07_ENVELOPE_OVERHEAD_BYTES: usize = 64;

/// Splits `payloads` so that each returned `TracerPayload`'s encoded size fits within
/// `max_size` where possible. Recursively bisects by trace-chunk boundary. A single chunk
/// that's still oversized is returned as-is and gets sent standalone.
fn split_oversized_payloads(
    payloads: Vec<pb::TracerPayload>,
    max_size: usize,
) -> Vec<pb::TracerPayload> {
    payloads
        .into_iter()
        .flat_map(|tp| split_tracer_payload(tp, max_size))
        .collect()
}

fn split_tracer_payload(tp: pb::TracerPayload, max_size: usize) -> Vec<pb::TracerPayload> {
    if tp.encoded_len() <= max_size {
        return vec![tp];
    }

    if tp.chunks.len() > 1 {
        let mid = tp.chunks.len() / 2;

        // Avoid cloning large chunk/span data on each bisection: clone only the metadata.
        let mut base = tp;
        let mut first_chunks = std::mem::take(&mut base.chunks);
        let second_chunks = first_chunks.split_off(mid);
        let mut first = base.clone();
        first.chunks = first_chunks;
        let mut second = base;
        second.chunks = second_chunks;

        let mut result = split_tracer_payload(first, max_size);
        result.extend(split_tracer_payload(second, max_size));
        return result;
    }

    vec![tp]
}

/// Computes the total encoded size of the inner TracerPayloads
fn encoded_size(payloads: &[pb::TracerPayload]) -> usize {
    payloads.iter().map(Message::encoded_len).sum()
}

#[async_trait]
pub trait TraceProcessor {
    /// Deserializes traces from a hyper request body and sends them through the provided tokio mpsc
    /// Sender.
    async fn process_traces(
        &self,
        config: Arc<Config>,
        req: http_common::HttpRequest,
        tx: Sender<trace_utils::SendData>,
        mini_agent_metadata: Arc<trace_utils::MiniAgentMetadata>,
    ) -> http::Result<http_common::HttpResponse>;
}

struct ChunkProcessor {
    config: Arc<Config>,
    mini_agent_metadata: Arc<trace_utils::MiniAgentMetadata>,
}

impl TraceChunkProcessor for ChunkProcessor {
    fn process(&mut self, chunk: &mut pb::TraceChunk, root_span_index: usize) {
        // Promote before local enrichment or any future sampled-out filtering.
        // Existing chunk values, including empty strings, take precedence.
        if !chunk.tags.contains_key(DECISION_MAKER_KEY)
            && let Some(value) = chunk
                .spans
                .iter()
                .find_map(|span| span.meta.get(DECISION_MAKER_KEY))
        {
            chunk
                .tags
                .insert(DECISION_MAKER_KEY.to_string(), value.clone());
        }

        // Clone app_name once instead of once per span
        let app_name = self.config.app_name.clone();

        trace_utils::set_serverless_root_span_tags(
            &mut chunk.spans[root_span_index],
            app_name.clone(),
            &self.config.env_type,
        );
        for span in chunk.spans.iter_mut() {
            trace_utils::enrich_span_with_mini_agent_metadata(span, &self.mini_agent_metadata);
            trace_utils::enrich_span_with_azure_function_metadata(span);
            if let EnvironmentType::CloudFunction = &self.config.env_type {
                trace_utils::enrich_span_with_google_cloud_function_metadata(
                    span,
                    &self.mini_agent_metadata,
                    app_name.clone(),
                );
            }
            obfuscate_span(span, &self.config.obfuscation_config);
        }
    }
}
/// Maximum number of trace-enqueue tasks that may be in flight (spawned but not yet finished
/// handing their pieces to the flusher) at once
const MAX_IN_FLIGHT_ENQUEUES: usize = 10;

/// Error rescue sampler and its monotonic timestamp floor, shared under one mutex.
pub struct ErrorRescueState {
    sampler: ErrorsSampler,
    last_sampler_timestamp: i64,
}

impl ErrorRescueState {
    fn new(config: ErrorSamplerConfig) -> Self {
        Self {
            sampler: ErrorsSampler::new(config),
            last_sampler_timestamp: i64::MIN,
        }
    }

    /// Records a timestamp without moving the sampler's rolling window backwards.
    fn clamp_timestamp(&mut self, now_unix_secs: i64) -> i64 {
        self.last_sampler_timestamp = self.last_sampler_timestamp.max(now_unix_secs);
        self.last_sampler_timestamp
    }
}

#[derive(Clone)]
pub struct ServerlessTraceProcessor {
    pub stats_concentrator: Option<StatsConcentratorHandle>,
    enqueue_permits: Arc<tokio::sync::Semaphore>,
    /// Processor clones and independently constructed processors using this
    /// `Arc` share one TPS budget and timestamp floor under the same lock.
    error_rescue_state: Arc<Mutex<ErrorRescueState>>,
}

impl ServerlessTraceProcessor {
    #[allow(clippy::must_use_candidate)]
    pub fn new(
        stats_concentrator: Option<StatsConcentratorHandle>,
        error_rescue_state: Arc<Mutex<ErrorRescueState>>,
    ) -> Self {
        ServerlessTraceProcessor {
            stats_concentrator,
            enqueue_permits: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT_ENQUEUES)),
            error_rescue_state,
        }
    }

    /// Builds the shared error rescue state from parsed config settings.
    #[must_use]
    pub fn new_error_sampler(config: &Config) -> Arc<Mutex<ErrorRescueState>> {
        if config.error_rare_sample_tracer_drop && config.error_sampler.target_tps <= 0.0 {
            warn!(
                "error_rare_sample_tracer_drop is enabled but DD_APM_ERROR_TPS <= 0 disables error rescue"
            );
        }
        Arc::new(Mutex::new(ErrorRescueState::new(config.error_sampler)))
    }

    /// Locks the shared sampler and timestamp floor, recovering from a poisoned
    /// guard (a panic in another thread while holding the lock).
    fn lock_sampler(&self) -> MutexGuard<'_, ErrorRescueState> {
        self.error_rescue_state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Applies the error rescue pass to a payload collection.
    ///
    /// For every eligible V07 chunk containing a non-zero error flag, consults
    /// the shared sampler. P0 and no-priority (-128) chunks are always eligible;
    /// the opt-in feature adds other negative priorities except exact manual drops.
    /// On a keep, stamps the sampler's `_dd.errors_sr` on the chunk's root
    /// span without priority promotion. Retention is decided by the backend.
    /// Chunks are never removed or reordered: unrescued chunks stay in the
    /// payload for the backend to handle.
    fn apply_error_rescue(&self, payload: &mut TracerPayloadCollection, config: &Config) {
        self.apply_error_rescue_with_clock(payload, config, || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64)
        });
    }

    fn apply_error_rescue_with_clock<F: FnOnce() -> i64>(
        &self,
        payload: &mut TracerPayloadCollection,
        config: &Config,
        read_clock: F,
    ) {
        let TracerPayloadCollection::V07(tracer_payloads) = payload else {
            return;
        };
        let mut state = None;
        let mut read_clock = Some(read_clock);
        for tracer_payload in tracer_payloads.iter_mut() {
            let env = resolve_payload_env(&tracer_payload.env, &config.env);
            for chunk in &mut tracer_payload.chunks {
                let Some(root_index) =
                    rescue_root_index(chunk, config.error_rare_sample_tracer_drop)
                else {
                    continue;
                };
                // Requests without candidates never take the shared lock.
                let state = state.get_or_insert_with(|| self.lock_sampler());
                if let Some(read_clock) = read_clock.take() {
                    if state.sampler.is_disabled() {
                        return;
                    }
                    // Read once under the lock, and clamp backward wall-clock steps
                    // so the sampler's rolling window cannot move backwards.
                    state.clamp_timestamp(read_clock());
                }
                let now_unix_secs = state.last_sampler_timestamp;
                let root = &chunk.spans[root_index];
                let views: Vec<SpanView> = chunk.spans.iter().map(span_view).collect();
                let trace = TraceView {
                    env,
                    trace_id: root.trace_id,
                    root_index,
                    // The shared sampler sanitizes the raw wire rate.
                    root_global_sample_rate: root
                        .metrics
                        .get("_sample_rate")
                        .copied()
                        .unwrap_or(1.0),
                    spans: &views,
                };
                if let SampleDecision::Keep { errors_sr } =
                    state.sampler.sample(now_unix_secs, &trace)
                    && let Some(root) = chunk.spans.get_mut(root_index)
                {
                    root.metrics
                        .insert(ERRORS_SR_METRIC_KEY.to_string(), errors_sr);
                }
            }
        }
    }

    /// Test-only variant that injects a synthetic clock reading through the
    /// production clamp path, exercising the rolling window without sleeping.
    #[cfg(test)]
    fn apply_error_rescue_at(
        &self,
        payload: &mut TracerPayloadCollection,
        config: &Config,
        now_unix_secs: i64,
    ) {
        self.apply_error_rescue_with_clock(payload, config, || now_unix_secs);
    }

    fn send_to_concentrator(
        concentrator: &StatsConcentratorHandle,
        payload: &TracerPayloadCollection,
    ) {
        if let TracerPayloadCollection::V07(tracer_payloads) = payload {
            for tracer_payload in tracer_payloads {
                // Fetch service from the `_dd.base_service` attribute on the root span
                let service_name = tracer_payload.chunks.iter().find_map(|c| {
                    trace_utils::get_root_span_index(&c.spans)
                        .ok()
                        .and_then(|i| c.spans[i].meta.get("_dd.base_service"))
                        .filter(|v| !v.is_empty())
                        .cloned()
                });
                let metadata = Arc::new(TracerMetadata {
                    schema_version: 2,
                    runtime_id: None,
                    tracer_language: tracer_payload.language_name.clone(),
                    tracer_version: tracer_payload.tracer_version.clone(),
                    hostname: String::new(),
                    service_name,
                    service_env: Some(tracer_payload.env.clone()),
                    service_version: Some(tracer_payload.app_version.clone()),
                    process_tags: None,
                    container_id: Some(tracer_payload.container_id.clone()),
                });
                for chunk in &tracer_payload.chunks {
                    if let Err(e) = concentrator.add_chunk(chunk.clone(), Arc::clone(&metadata)) {
                        error!("Failed to send trace chunk to concentrator: {e}");
                    }
                }
            }
        } else {
            let version = match payload {
                TracerPayloadCollection::V04(_) => "V04",
                TracerPayloadCollection::V05(_) => "V05",
                TracerPayloadCollection::V07(_) => unreachable!(),
                TracerPayloadCollection::V1(_) => "V1",
            };
            error!("Unsupported tracer payload version {version}. Failed to send trace stats.");
        }
    }
}

/// Chooses the env used to key per-signature state: the payload-reported env
/// when nonempty, otherwise the agent's configured env. Shared by stats
/// flushing and error-rescue sampling so the fallback rule can't drift
/// between the two.
pub(crate) fn resolve_payload_env<'a>(payload_env: &'a str, config_env: &'a str) -> &'a str {
    if payload_env.is_empty() {
        config_env
    } else {
        payload_env
    }
}

/// Builds the sampler's read-only view of a span from its already enriched and
/// obfuscated fields.
fn span_view(span: &pb::Span) -> SpanView<'_> {
    SpanView {
        service: &span.service,
        name: &span.name,
        resource: &span.resource,
        error: span.error != 0,
        http_status_code: span.meta.get("http.status_code").map(String::as_str),
        error_type: span.meta.get("error.type").map(String::as_str),
    }
}

/// Returns the root span index of a chunk that needs an error sampler decision,
/// or `None` when the chunk is not a rescue candidate.
///
/// P0 and no-priority (-128) chunks are candidates. No priority means a missing
/// sampling decision, not an explicit drop, matching Go's baseline eligibility.
/// With the feature enabled, other negative priorities also qualify except
/// priority -1 with chunk DM -4.
/// Only promoted chunk tags are consulted, never span metadata. The chunk
/// must contain at least one span with a non-zero error flag; HTTP status or
/// error metadata alone does not qualify. The root is resolved with
/// `get_root_span_index`: empty chunks are left unchanged, and non-empty
/// chunks always resolve a root, falling back to the last span when no span
/// has `parent_id 0` (e.g. a cyclic chunk).
fn rescue_root_index(chunk: &pb::TraceChunk, tracer_drop_enabled: bool) -> Option<usize> {
    let eligible = chunk.priority == 0
        || chunk.priority == i32::from(i8::MIN)
        || (tracer_drop_enabled
            && chunk.priority < 0
            && !(chunk.priority == -1
                && chunk.tags.get(DECISION_MAKER_KEY).map(String::as_str)
                    == Some(MANUAL_DECISION_MAKER)));
    if !eligible {
        return None;
    }
    // An error anywhere in the chunk makes it a rescue candidate, not just an
    // error on the root span. Checked before the root-span search because
    // non-errored chunks are the common case on this path.
    if !chunk.spans.iter().any(|span| span.error != 0) {
        return None;
    }
    // No identifiable root span: leave the chunk unchanged rather than
    // guessing an index.
    trace_utils::get_root_span_index(&chunk.spans)
        .ok()
        .filter(|&root_index| root_index < chunk.spans.len())
}

#[async_trait]
impl TraceProcessor for ServerlessTraceProcessor {
    async fn process_traces(
        &self,
        config: Arc<Config>,
        req: http_common::HttpRequest,
        tx: Sender<trace_utils::SendData>,
        mini_agent_metadata: Arc<trace_utils::MiniAgentMetadata>,
    ) -> http::Result<http_common::HttpResponse> {
        debug!("Received traces to process");
        let (parts, body) = req.into_parts();

        if let Some(response) = http_utils::verify_request_content_length(
            &parts.headers,
            config.max_request_content_length,
            "Error processing traces",
        ) {
            return response;
        }

        // Bound how many requests can be in the decode/enrich/split/enqueue pipeline at once
        // and carried through to the spawned enqueue task at the end
        let permit = match tokio::time::timeout(
            std::time::Duration::from_secs(config.enqueue_permit_timeout_secs),
            self.enqueue_permits.clone().acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) | Err(_) => {
                // The traces are dropped. The body is still drained so the connection can be
                // kept alive for the next request instead of being closed.
                warn!("Could not acquire an enqueue permit in time; dropping traces");
                if let Err(err) = body.collect().await {
                    debug!("Error draining /v0.4/traces request body while dropping traces: {err}");
                }
                return log_and_create_traces_success_http_response(
                    "Dropped traces due to enqueue capacity",
                    StatusCode::OK,
                );
            }
        };

        let tracer_header_tags = (&parts.headers).into();

        // deserialize traces from the request body, convert to protobuf structs (see trace-protobuf
        // crate)
        let (body_size, traces) = match trace_utils::get_traces_from_request_body(body).await {
            Ok(res) => res,
            Err(err) => {
                return log_and_create_http_response(
                    &format!("Error deserializing trace from request body: {err}"),
                    StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
        };

        // double check content length is < max request content length in case transfer encoding is used
        if body_size > config.max_request_content_length {
            return log_and_create_http_response(
                "Error processing traces: Payload too large",
                StatusCode::PAYLOAD_TOO_LARGE,
            );
        }

        let mut payload = match trace_utils::collect_pb_trace_chunks(
            traces,
            &tracer_header_tags,
            &mut ChunkProcessor {
                config: config.clone(),
                mini_agent_metadata: mini_agent_metadata.clone(),
            },
            true, // In mini agent, we always send agentless
        ) {
            Ok(res) => res,
            Err(err) => {
                return log_and_create_traces_success_http_response(
                    &format!("Error processing trace chunks: {err}"),
                    StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
        };

        // Add function_tags to payload if we can
        if let Some(function_tags) = config.tags.function_tags()
            && let TracerPayloadCollection::V07(ref mut tracer_payloads) = payload
        {
            for tracer_payload in tracer_payloads {
                tracer_payload.tags.insert(
                    TRACER_PAYLOAD_FUNCTION_TAGS_TAG_KEY.to_string(),
                    function_tags.to_string(),
                );
            }
        }

        // When agent stats computation is enabled, the agent unconditionally computes trace
        // stats, ignoring the Datadog-Client-Computed-Stats header.
        if let Some(ref concentrator) = self.stats_concentrator
            && config.agent_stats_computation_enabled
        {
            Self::send_to_concentrator(concentrator, &payload);
        }

        // Error rescue is independent of agent stats computation: eligible errored
        // chunks can arrive whether or not the agent computes stats. Run it after
        // conditional stats submission so the concentrator observes chunks before
        // rescue stamping, and before payload splitting so the inserted metric is
        // included in the recomputed outbound size.
        self.apply_error_rescue(&mut payload, &config);

        let pieces: Vec<(TracerPayloadCollection, usize)> = match payload {
            TracerPayloadCollection::V07(payloads) => {
                let split_budget =
                    MAX_CONTENT_SIZE_BYTES.saturating_sub(V07_ENVELOPE_OVERHEAD_BYTES);
                split_oversized_payloads(payloads, split_budget)
                    .into_iter()
                    .map(|tp| {
                        let size =
                            encoded_size(std::slice::from_ref(&tp)) + V07_ENVELOPE_OVERHEAD_BYTES;
                        (TracerPayloadCollection::V07(vec![tp]), size)
                    })
                    .collect()
            }
            other => vec![(other, body_size)],
        };

        if pieces.len() > 1 {
            debug!(
                piece_count = pieces.len(),
                "Oversized trace payload split into multiple pieces"
            );
        }

        let send_datas: Vec<SendData> = pieces
            .into_iter()
            .map(|(piece, size)| {
                if size > MAX_CONTENT_SIZE_BYTES {
                    // For V07, `size` includes V07_ENVELOPE_OVERHEAD_BYTES; for other
                    // formats it's the raw body_size - both approximate checks
                    warn!(
                        payload_size = size,
                        max_content_size_bytes = MAX_CONTENT_SIZE_BYTES,
                        "Trace payload is over max batch size; sending standalone"
                    );
                }

                SendData::new(
                    size,
                    piece,
                    tracer_header_tags.clone(),
                    &config.trace_intake,
                )
            })
            .collect();

        tokio::spawn(async move {
            let _permit = permit; // released when this task ends
            for send_data in send_datas {
                if let Err(err) = tx.send(send_data).await {
                    error!("Error sending traces to the trace flusher: {err}");
                    return;
                }
            }
        });

        log_and_create_traces_success_http_response(
            "Successfully buffered traces to be flushed.",
            StatusCode::OK,
        )
    }
}

#[cfg(test)]
mod tests {
    use hyper::Request;
    use libdd_trace_obfuscation::obfuscation_config::ObfuscationConfig;
    use std::{collections::HashMap, sync::Arc, time::UNIX_EPOCH};
    use tokio::sync::mpsc::{self, Receiver, Sender};

    use crate::{
        aggregator::MAX_CONTENT_SIZE_BYTES,
        config::{Config, Tags},
        peer_tags::peer_tag_keys,
        trace_processor::{
            self, ErrorRescueState, MAX_IN_FLIGHT_ENQUEUES, TRACER_PAYLOAD_FUNCTION_TAGS_TAG_KEY,
            TraceProcessor, encoded_size, split_oversized_payloads,
        },
    };
    use datadog_agent_trace_sampler::{
        ErrorSamplerConfig, ErrorSamplerMode, ErrorsSampler, SampleDecision, SpanView, TraceView,
    };
    use libdd_common::{Endpoint, http_common};
    use libdd_trace_protobuf::pb;
    use libdd_trace_utils::test_utils::{create_test_gcp_json_span, create_test_gcp_span};
    use libdd_trace_utils::trace_utils::{MiniAgentMetadata, SendData};
    use libdd_trace_utils::tracer_header_tags::TracerHeaderTags;
    use libdd_trace_utils::{
        test_utils::create_test_json_span, trace_utils, tracer_payload::TracerPayloadCollection,
    };

    fn get_current_timestamp_nanos() -> i64 {
        UNIX_EPOCH.elapsed().unwrap().as_nanos() as i64
    }

    fn create_test_config() -> Config {
        Config {
            app_name: Some("dummy_function_name".to_string()),
            max_request_content_length: 10 * 1024 * 1024,
            trace_flush_interval_secs: 3,
            stats_flush_interval_secs: 3,
            proxy_request_timeout_secs: 30,
            proxy_request_max_retries: 3,
            proxy_request_retry_backoff_base_ms: 100,
            verify_env_timeout_ms: 100,
            enqueue_permit_timeout_secs: 2,
            trace_intake: Endpoint {
                url: hyper::Uri::from_static("https://trace.agent.notdog.com/traces"),
                api_key: Some("dummy_api_key".into()),
                ..Default::default()
            },
            trace_stats_intake: Endpoint {
                url: hyper::Uri::from_static("https://trace.agent.notdog.com/stats"),
                api_key: Some("dummy_api_key".into()),
                ..Default::default()
            },
            dsm_intake: Endpoint {
                url: hyper::Uri::from_static(
                    "https://trace.agent.notdog.com/api/v0.1/pipeline_stats",
                ),
                api_key: Some("dummy_api_key".into()),
                ..Default::default()
            },
            dd_site: "datadoghq.com".to_string(),
            dd_apm_receiver_port: 8126,
            #[cfg(any(all(windows, feature = "windows-pipes"), test))]
            dd_apm_windows_pipe_name: None,
            dd_dogstatsd_port: 8125,
            #[cfg(any(all(windows, feature = "windows-pipes"), test))]
            dd_dogstatsd_windows_pipe_name: None,
            env_type: trace_utils::EnvironmentType::CloudFunction,
            os: "linux".to_string(),
            obfuscation_config: ObfuscationConfig::new().unwrap(),
            proxy_url: None,
            profiling_intake: Endpoint {
                url: hyper::Uri::from_static("https://proxy.agent.notdog.com/proxy"),
                api_key: Some("dummy_api_key".into()),
                ..Default::default()
            },
            tags: Tags::from_env_string("env:test,service:my-service"),
            env: "test-env".to_string(),
            peer_tags: peer_tag_keys().unwrap(),
            experimental_features_enabled: false,
            additional_metric_tags: vec![],
            additional_metric_tags_cardinality_limit: None,
            agent_stats_computation_enabled: false,
            error_sampler: ErrorSamplerConfig::default(),
            error_rare_sample_tracer_drop: false,
        }
    }

    fn default_error_sampler() -> Arc<std::sync::Mutex<ErrorRescueState>> {
        sampler_for(&ErrorSamplerConfig::default())
    }

    fn create_test_metadata() -> MiniAgentMetadata {
        MiniAgentMetadata {
            azure_spring_app_hostname: Default::default(),
            azure_spring_app_name: Default::default(),
            gcp_project_id: Some("dummy_project_id".to_string()),
            gcp_region: Some("dummy_region_west".to_string()),
            version: Some("dummy_version".to_string()),
        }
    }

    fn make_span(meta: HashMap<String, String>) -> pb::Span {
        pb::Span {
            meta,
            ..Default::default()
        }
    }

    fn make_chunk(spans: Vec<pb::Span>) -> pb::TraceChunk {
        pb::TraceChunk {
            spans,
            ..Default::default()
        }
    }

    fn make_payload(chunks: Vec<pb::TraceChunk>) -> pb::TracerPayload {
        pb::TracerPayload {
            chunks,
            ..Default::default()
        }
    }

    fn big_span() -> pb::Span {
        make_span(HashMap::from([("blob".to_string(), "x".repeat(50))]))
    }

    #[test]
    fn test_no_split_needed_when_under_max() {
        let payload = make_payload(vec![make_chunk(vec![big_span()])]);
        let size = encoded_size(std::slice::from_ref(&payload));

        let result = split_oversized_payloads(vec![payload], size);

        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_splits_multiple_chunks_when_collectively_oversized() {
        // Two chunks, each individually small, but together over max_size.
        let payload = make_payload(vec![
            make_chunk(vec![big_span()]),
            make_chunk(vec![big_span()]),
        ]);
        let one_chunk_size =
            encoded_size(std::slice::from_ref(&make_payload(vec![make_chunk(vec![
                big_span(),
            ])])));
        let max_size = one_chunk_size + 10; // fits one chunk, not both

        let result = split_oversized_payloads(vec![payload], max_size);

        assert_eq!(result.len(), 2);
        for piece in &result {
            assert_eq!(piece.chunks.len(), 1);
            assert!(encoded_size(std::slice::from_ref(piece)) <= max_size);
        }
    }

    #[test]
    fn test_single_oversized_span_returned_as_is() {
        // One chunk, one span, that span alone already exceeds max_size.
        let payload = make_payload(vec![make_chunk(vec![big_span()])]);
        let actual_size = encoded_size(std::slice::from_ref(&payload));
        let max_size = actual_size - 1; // impossible to fit, even alone

        let result = split_oversized_payloads(vec![payload], max_size);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].chunks.len(), 1);
        assert_eq!(result[0].chunks[0].spans.len(), 1);
        // Still oversized - this is the signal the caller logs a warning for.
        assert!(encoded_size(&result) > max_size);
    }

    #[test]
    fn test_encoded_size_sums_multiple_payloads() {
        let a = make_payload(vec![make_chunk(vec![big_span()])]);
        let b = make_payload(vec![make_chunk(vec![big_span()])]);
        let a_size = encoded_size(std::slice::from_ref(&a));
        let b_size = encoded_size(std::slice::from_ref(&b));

        assert_eq!(encoded_size(&[a, b]), a_size + b_size);
    }

    #[tokio::test]
    async fn test_process_trace() {
        let (tx, mut rx): (
            Sender<trace_utils::SendData>,
            Receiver<trace_utils::SendData>,
        ) = mpsc::channel(1);

        let start = get_current_timestamp_nanos();

        let json_span = create_test_json_span(11, 222, 333, start, false);

        let bytes = rmp_serde::to_vec(&vec![vec![json_span]]).unwrap();
        let request = Request::builder()
            .header("datadog-meta-tracer-version", "4.0.0")
            .header("datadog-meta-lang", "nodejs")
            .header("datadog-meta-lang-version", "v19.7.0")
            .header("datadog-meta-lang-interpreter", "v8")
            .header("datadog-container-id", "33")
            .header("content-length", "100")
            .body(http_common::Body::from(bytes))
            .unwrap();

        let trace_processor =
            trace_processor::ServerlessTraceProcessor::new(None, default_error_sampler());
        let res = trace_processor
            .process_traces(
                Arc::new(create_test_config()),
                request,
                tx,
                Arc::new(create_test_metadata()),
            )
            .await;
        assert!(res.is_ok());

        let tracer_payload = rx.recv().await;

        assert!(tracer_payload.is_some());

        let expected_tracer_payload = pb::TracerPayload {
            container_id: "33".to_string(),
            language_name: "nodejs".to_string(),
            language_version: "v19.7.0".to_string(),
            tracer_version: "4.0.0".to_string(),
            runtime_id: "test-runtime-id-value".to_string(),
            chunks: vec![pb::TraceChunk {
                priority: i8::MIN as i32,
                origin: "".to_string(),
                spans: vec![create_test_gcp_span(11, 222, 333, start, true)],
                tags: HashMap::new(),
                dropped_trace: false,
            }],
            tags: HashMap::from([(
                TRACER_PAYLOAD_FUNCTION_TAGS_TAG_KEY.to_string(),
                "env:test,service:my-service".to_string(),
            )]),
            env: "test-env".to_string(),
            hostname: "".to_string(),
            app_version: "".to_string(),
            container_debug: None,
        };

        let received_payload =
            if let TracerPayloadCollection::V07(payload) = tracer_payload.unwrap().get_payloads() {
                Some(payload[0].clone())
            } else {
                None
            };
        assert_eq!(expected_tracer_payload, received_payload.unwrap());
    }

    #[tokio::test]
    async fn test_process_trace_top_level_span_set() {
        let (tx, mut rx): (
            Sender<trace_utils::SendData>,
            Receiver<trace_utils::SendData>,
        ) = mpsc::channel(1);

        let start = get_current_timestamp_nanos();

        let json_trace = vec![
            create_test_gcp_json_span(11, 333, 222, start),
            create_test_gcp_json_span(11, 222, 0, start),
            create_test_gcp_json_span(11, 444, 333, start),
        ];

        let bytes = rmp_serde::to_vec(&vec![json_trace]).unwrap();
        let request = Request::builder()
            .header("datadog-meta-tracer-version", "4.0.0")
            .header("datadog-meta-lang", "nodejs")
            .header("datadog-meta-lang-version", "v19.7.0")
            .header("datadog-meta-lang-interpreter", "v8")
            .header("datadog-container-id", "33")
            .header("content-length", "100")
            .body(http_common::Body::from(bytes))
            .unwrap();

        let trace_processor =
            trace_processor::ServerlessTraceProcessor::new(None, default_error_sampler());
        let res = trace_processor
            .process_traces(
                Arc::new(create_test_config()),
                request,
                tx,
                Arc::new(create_test_metadata()),
            )
            .await;
        assert!(res.is_ok());

        let tracer_payload = rx.recv().await;

        assert!(tracer_payload.is_some());

        let expected_tracer_payload = pb::TracerPayload {
            container_id: "33".to_string(),
            language_name: "nodejs".to_string(),
            language_version: "v19.7.0".to_string(),
            tracer_version: "4.0.0".to_string(),
            runtime_id: "test-runtime-id-value".to_string(),
            chunks: vec![pb::TraceChunk {
                priority: i8::MIN as i32,
                origin: "".to_string(),
                spans: vec![
                    create_test_gcp_span(11, 333, 222, start, false),
                    create_test_gcp_span(11, 222, 0, start, true),
                    create_test_gcp_span(11, 444, 333, start, false),
                ],
                tags: HashMap::new(),
                dropped_trace: false,
            }],
            tags: HashMap::from([(
                TRACER_PAYLOAD_FUNCTION_TAGS_TAG_KEY.to_string(),
                "env:test,service:my-service".to_string(),
            )]),
            env: "test-env".to_string(),
            hostname: "".to_string(),
            app_version: "".to_string(),
            container_debug: None,
        };

        let received_payload =
            if let TracerPayloadCollection::V07(payload) = tracer_payload.unwrap().get_payloads() {
                Some(payload[0].clone())
            } else {
                None
            };

        assert_eq!(expected_tracer_payload, received_payload.unwrap());
    }

    #[tokio::test]
    async fn test_process_trace_sends_oversized_single_chunk_standalone() {
        let (tx, mut rx): (
            Sender<trace_utils::SendData>,
            Receiver<trace_utils::SendData>,
        ) = mpsc::channel(10);

        let start = get_current_timestamp_nanos();

        // One trace (one chunk) with a single span whose meta field alone exceeds
        // MAX_CONTENT_SIZE_BYTES once encoded, but stays under max_request_content_length.
        let mut spans = Vec::new();
        let mut span = create_test_json_span(11, 222, 333, start, false);
        if let Some(obj) = span.as_object_mut() {
            obj.insert(
                "meta".to_string(),
                serde_json::json!({
                    "large_field": "x".repeat(MAX_CONTENT_SIZE_BYTES)
                }),
            );
        }
        spans.push(span);

        let bytes = rmp_serde::to_vec(&vec![spans]).unwrap();
        let request = Request::builder()
            .header("datadog-meta-tracer-version", "4.0.0")
            .header("datadog-meta-lang", "nodejs")
            .header("datadog-meta-lang-version", "v19.7.0")
            .header("datadog-meta-lang-interpreter", "v8")
            .header("datadog-container-id", "33")
            .header("content-length", "100")
            .body(http_common::Body::from(bytes))
            .unwrap();

        let trace_processor =
            trace_processor::ServerlessTraceProcessor::new(None, default_error_sampler());
        let res = trace_processor
            .process_traces(
                Arc::new(create_test_config()),
                request,
                tx,
                Arc::new(create_test_metadata()),
            )
            .await;
        assert!(res.is_ok());

        let send_data = rx
            .recv()
            .await
            .expect("expected the oversized single-chunk trace to be sent standalone");

        assert!(
            rx.try_recv().is_err(),
            "expected exactly one piece to be sent"
        );
        assert!(
            send_data.len() > MAX_CONTENT_SIZE_BYTES,
            "expected the standalone piece to still be reported as oversized (size {})",
            send_data.len()
        );
    }

    fn small_trace_request() -> http_common::HttpRequest {
        let start = get_current_timestamp_nanos();
        let json_span = create_test_json_span(11, 222, 333, start, false);
        let bytes = rmp_serde::to_vec(&vec![vec![json_span]]).unwrap();
        Request::builder()
            .header("datadog-meta-tracer-version", "4.0.0")
            .header("datadog-meta-lang", "nodejs")
            .header("datadog-meta-lang-version", "v19.7.0")
            .header("datadog-meta-lang-interpreter", "v8")
            .header("datadog-container-id", "33")
            .header("content-length", "100")
            .body(http_common::Body::from(bytes))
            .unwrap()
    }

    fn dummy_send_data(config: &Config) -> trace_utils::SendData {
        SendData::new(
            1,
            TracerPayloadCollection::V07(Vec::new()),
            TracerHeaderTags::default(),
            &config.trace_intake,
        )
    }

    #[tokio::test]
    async fn test_enqueue_permits_bound_in_flight_tasks_and_shed_load() {
        let (tx, mut rx): (
            Sender<trace_utils::SendData>,
            Receiver<trace_utils::SendData>,
        ) = mpsc::channel(1);

        let trace_processor =
            trace_processor::ServerlessTraceProcessor::new(None, default_error_sampler());
        let config = Arc::new(Config {
            enqueue_permit_timeout_secs: 0,
            ..create_test_config()
        });

        // Pre-fill the channel's one slot so every enqueue attempt below blocks on tx.send()
        // (and holds its permit) instead of succeeding immediately.
        tx.try_send(dummy_send_data(&config)).unwrap();

        // Saturate all MAX_IN_FLIGHT_ENQUEUES permits: each call acquires a permit and spawns
        // a task that then blocks forever on tx.send(), since nothing drains rx yet. Permit
        // acquisition happens synchronously inside process_traces before it returns, so by the
        // time this loop finishes, all permits are deterministically held - no race.
        for _ in 0..MAX_IN_FLIGHT_ENQUEUES {
            let res = trace_processor
                .process_traces(
                    config.clone(),
                    small_trace_request(),
                    tx.clone(),
                    Arc::new(create_test_metadata()),
                )
                .await;
            assert!(res.is_ok());
        }
        assert_eq!(
            trace_processor.enqueue_permits.available_permits(),
            0,
            "expected all permits to be held after saturating requests"
        );

        // All permits are held, so this request can't get one in time (timeout is 0) and
        // should shed load rather than hang - still a 200, but its trace is dropped before
        // ever reaching tx.send() (permit acquisition happens before decode).
        let res = trace_processor
            .process_traces(
                config.clone(),
                small_trace_request(),
                tx.clone(),
                Arc::new(create_test_metadata()),
            )
            .await;
        assert!(res.is_ok());

        // Drop the original sender - the shed 11th request never created a clone of its own,
        // since it sheds before decoding/building anything. The channel will only close once
        // every clone held by the 10 blocked tasks is also dropped, which happens as each one
        // is unblocked in turn by draining the item ahead of it.
        drop(tx);

        let mut received = 0;
        while rx.recv().await.is_some() {
            received += 1;
        }
        assert_eq!(
            received,
            MAX_IN_FLIGHT_ENQUEUES + 1,
            "expected exactly the dummy plus the 10 saturating payloads, nothing from the shed 11th"
        );
    }

    #[tokio::test]
    async fn test_enqueue_permit_releases_after_send_completes() {
        let (tx, mut rx): (
            Sender<trace_utils::SendData>,
            Receiver<trace_utils::SendData>,
        ) = mpsc::channel(MAX_IN_FLIGHT_ENQUEUES + 1);

        let trace_processor =
            trace_processor::ServerlessTraceProcessor::new(None, default_error_sampler());
        // Uses the default (non-zero) enqueue_permit_timeout_secs, so the 11th request's permit
        // acquire has real time to succeed once an earlier request's send completes and
        // releases its permit, rather than racing a 0-second timeout against task scheduling.
        let config = Arc::new(create_test_config());

        // With enough channel capacity for every send to complete immediately, all
        // MAX_IN_FLIGHT_ENQUEUES requests should succeed and their permits should be released
        // right away - leaving room for one more request to succeed too, not shed.
        for _ in 0..=MAX_IN_FLIGHT_ENQUEUES {
            let res = trace_processor
                .process_traces(
                    config.clone(),
                    small_trace_request(),
                    tx.clone(),
                    Arc::new(create_test_metadata()),
                )
                .await;
            assert!(res.is_ok());
            // Give the previous request's spawned enqueue task a chance to run its (instant,
            // since the channel has room) send and release its permit before the next request
            // tries to acquire one.
            tokio::task::yield_now().await;
        }

        // Drop the original sender so the channel closes once every clone held by the 11
        // completed send tasks is also dropped, then drain to closure for a deterministic
        // count instead of a try_recv() snapshot that could race a still-completing task.
        drop(tx);

        let mut received = 0;
        while rx.recv().await.is_some() {
            received += 1;
        }
        assert_eq!(
            received,
            MAX_IN_FLIGHT_ENQUEUES + 1,
            "expected every request's permit to be released after its send completed, \
             allowing all of them to succeed rather than shedding load"
        );
    }

    // ---- Error rescue tests ----

    const RESCUE_NOW: i64 = 1_700_000_000;

    fn test_span(trace_id: u64, span_id: u64, parent_id: u64, error: i32) -> pb::Span {
        pb::Span {
            service: "test-service".to_string(),
            name: "test-operation".to_string(),
            resource: "GET /test".to_string(),
            trace_id,
            span_id,
            parent_id,
            error,
            ..Default::default()
        }
    }

    fn test_chunk(spans: Vec<pb::Span>, priority: i32) -> pb::TraceChunk {
        pb::TraceChunk {
            spans,
            priority,
            ..Default::default()
        }
    }

    fn errored_root_chunk(trace_id: u64, priority: i32) -> pb::TraceChunk {
        test_chunk(vec![test_span(trace_id, trace_id + 1, 0, 1)], priority)
    }

    fn rescue_config(error_sampler: ErrorSamplerConfig) -> Config {
        Config {
            agent_stats_computation_enabled: true,
            env: "agent-env".to_string(),
            error_sampler,
            ..create_test_config()
        }
    }

    fn always_keep_sampler_config() -> ErrorSamplerConfig {
        ErrorSamplerConfig {
            mode: ErrorSamplerMode::AlwaysKeep,
            target_tps: 10.0,
            extra_sample_rate: 1.0,
        }
    }

    fn rate_limited_sampler_config(target_tps: f64) -> ErrorSamplerConfig {
        ErrorSamplerConfig {
            mode: ErrorSamplerMode::RateLimited,
            target_tps,
            extra_sample_rate: 1.0,
        }
    }

    fn sampler_for(config: &ErrorSamplerConfig) -> Arc<std::sync::Mutex<ErrorRescueState>> {
        Arc::new(std::sync::Mutex::new(ErrorRescueState::new(*config)))
    }

    /// Builds the standard rescue test fixture: a config with the given
    /// sampler settings and a processor whose shared sampler uses them.
    fn rescue_setup(
        sampler_config: ErrorSamplerConfig,
    ) -> (Config, trace_processor::ServerlessTraceProcessor) {
        let config = rescue_config(sampler_config);
        let processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&config.error_sampler),
        );
        (config, processor)
    }

    fn run_rescue(
        processor: &trace_processor::ServerlessTraceProcessor,
        config: &Config,
        chunks: Vec<pb::TraceChunk>,
        now_unix_secs: i64,
    ) -> Vec<pb::TraceChunk> {
        let mut payload = TracerPayloadCollection::V07(vec![pb::TracerPayload {
            chunks,
            ..Default::default()
        }]);
        processor.apply_error_rescue_at(&mut payload, config, now_unix_secs);
        match payload {
            TracerPayloadCollection::V07(mut payloads) => payloads.remove(0).chunks,
            _ => unreachable!(),
        }
    }

    fn root(chunk: &pb::TraceChunk) -> &pb::Span {
        // All fixtures in this section place the root first or resolve it the
        // same way the processor does.
        &chunk.spans[0]
    }

    fn errors_sr(span: &pb::Span) -> Option<f64> {
        span.metrics.get("_dd.errors_sr").copied()
    }

    /// Builds the equivalent standalone `TraceView` for a fixture chunk, used to
    /// compare adapter decisions with direct shared-sampler usage.
    fn equivalent_trace_view<'a>(
        chunk: &pb::TraceChunk,
        env: &'a str,
        views: &'a [SpanView<'a>],
    ) -> TraceView<'a> {
        let root_index = trace_utils::get_root_span_index(&chunk.spans).unwrap();
        let root = &chunk.spans[root_index];
        TraceView {
            env,
            trace_id: root.trace_id,
            root_index,
            root_global_sample_rate: root.metrics.get("_sample_rate").copied().unwrap_or(1.0),
            spans: views,
        }
    }

    fn rescued_count(chunks: &[pb::TraceChunk]) -> usize {
        chunks
            .iter()
            .filter(|c| errors_sr(root(c)).is_some())
            .count()
    }

    /// Builds a `/v0.4/traces` request carrying a single root span with an
    /// error and the requested sampling priority and decision maker.
    fn errored_request(priority: i32, dm: Option<&str>) -> http_common::HttpRequest {
        let start = get_current_timestamp_nanos();
        let mut json_span = create_test_json_span(11, 222, 333, start, true);
        json_span["error"] = serde_json::json!(1);
        json_span["metrics"]["_sampling_priority_v1"] = serde_json::json!(priority);
        if let Some(dm) = dm {
            json_span["meta"]["_dd.p.dm"] = serde_json::json!(dm);
        }
        let bytes = rmp_serde::to_vec(&vec![vec![json_span]]).unwrap();
        Request::builder()
            .header("datadog-meta-tracer-version", "4.0.0")
            .header("datadog-meta-lang", "nodejs")
            .header("datadog-meta-lang-version", "v19.7.0")
            .header("datadog-meta-lang-interpreter", "v8")
            .header("content-length", "100")
            .body(http_common::Body::from(bytes))
            .unwrap()
    }

    #[test]
    fn test_decision_maker_promotion_is_unconditional_and_preserves_metadata() {
        use libdd_trace_utils::tracer_payload::TraceChunkProcessor as _;
        for feature in [false, true] {
            for (chunk_dm, first, second, child_first, expected) in [
                (None, None, None, false, None),
                (None, Some("-4"), None, false, Some("-4")),
                (None, None, Some("-3"), false, Some("-3")),
                (None, Some("-3"), Some("-4"), true, Some("-3")),
                (None, Some("-4"), Some("-3"), false, Some("-4")),
                (None, Some("-3"), Some("-4"), false, Some("-3")),
                (Some("-9"), Some("-4"), Some("-3"), false, Some("-9")),
                (Some(""), Some("-4"), None, false, Some("")),
                (None, Some(""), Some("-4"), true, Some("")),
            ] {
                let mut config = create_test_config();
                config.error_rare_sample_tracer_drop = feature;
                config.agent_stats_computation_enabled = false;
                let mut spans = vec![test_span(1, 2, 0, 1), test_span(1, 3, 2, 0)];
                if child_first {
                    spans.swap(0, 1);
                }
                for (span, dm) in spans.iter_mut().zip([first, second]) {
                    span.meta.insert("keep".into(), "metadata".into());
                    span.metrics.insert("_sampling_priority_v1".into(), -1.0);
                    if let Some(dm) = dm {
                        span.meta
                            .insert(super::DECISION_MAKER_KEY.into(), dm.into());
                    }
                }
                let mut chunk = test_chunk(spans, -1);
                chunk.origin = "synthetics".into();
                chunk.tags.insert("other".into(), "value".into());
                if let Some(dm) = chunk_dm {
                    chunk
                        .tags
                        .insert(super::DECISION_MAKER_KEY.into(), dm.into());
                }
                let original = chunk.clone();
                let root_index = usize::from(child_first);
                super::ChunkProcessor {
                    config: Arc::new(config),
                    mini_agent_metadata: Arc::new(create_test_metadata()),
                }
                .process(&mut chunk, root_index);
                assert_eq!(
                    chunk
                        .tags
                        .get(super::DECISION_MAKER_KEY)
                        .map(String::as_str),
                    expected
                );
                assert_eq!(chunk.tags.get("other"), original.tags.get("other"));
                assert_eq!(chunk.priority, original.priority);
                assert_eq!(chunk.origin, original.origin);
                for (span, before) in chunk.spans.iter().zip(&original.spans) {
                    assert_eq!(span.span_id, before.span_id);
                    assert_eq!(
                        span.metrics.get("_sampling_priority_v1"),
                        before.metrics.get("_sampling_priority_v1")
                    );
                    for (key, value) in &before.meta {
                        assert_eq!(span.meta.get(key), Some(value));
                    }
                }
            }
        }
    }

    #[test]
    fn test_tracer_drop_rescue_eligibility_matrix() {
        // Expected eligibility with the feature off and on, respectively.
        for (priority, dm, expected) in [
            (0, None, [true, true]),
            (0, Some("-4"), [true, true]),
            (-128, None, [true, true]),
            (-128, Some("-4"), [true, true]),
            (-1, None, [false, true]),
            (-1, Some(""), [false, true]),
            (-1, Some("-4"), [false, false]),
            (-1, Some("-3"), [false, true]),
            (-1, Some("-9"), [false, true]),
            (-1, Some("-40"), [false, true]),
            (-2, Some("-4"), [false, true]),
            (-127, None, [false, true]),
            (-5, None, [false, true]),
            (1, None, [false, false]),
            (2, None, [false, false]),
        ] {
            for (feature, eligible) in [false, true].into_iter().zip(expected) {
                for (error, has_error) in [(0, false), (1, true), (-1, true)] {
                    let mut chunk = test_chunk(
                        vec![test_span(1, 2, 0, 0), test_span(1, 3, 2, error)],
                        priority,
                    );
                    // Span metadata alone must not determine eligibility.
                    chunk.spans[0]
                        .meta
                        .insert(super::DECISION_MAKER_KEY.into(), "-4".into());
                    chunk.spans[0]
                        .meta
                        .insert("http.status_code".into(), "500".into());
                    chunk.spans[0]
                        .meta
                        .insert("error.type".into(), "Error".into());
                    if let Some(dm) = dm {
                        chunk
                            .tags
                            .insert(super::DECISION_MAKER_KEY.into(), dm.into());
                    }
                    assert_eq!(
                        super::rescue_root_index(&chunk, feature),
                        (eligible && has_error).then_some(0),
                        "feature={feature} priority={priority} dm={dm:?} error={error}"
                    );
                }
            }
        }
    }

    #[test]
    fn test_rescue_preserves_chunks_except_root_metric() {
        let (config, processor) = rescue_setup(always_keep_sampler_config());
        let mut chunk = test_chunk(vec![test_span(1, 2, 0, 0), test_span(1, 3, 2, 1)], 0);
        chunk.spans[0].metrics.extend([
            ("_sampling_priority_v1".into(), 0.0),
            ("_sample_rate".into(), 0.25),
            ("_dd.span_sampling.rule".into(), 1.0),
        ]);
        chunk.spans[0]
            .meta
            .insert(super::DECISION_MAKER_KEY.into(), "-3".into());
        chunk.spans[1].meta.insert("keep".into(), "me".into());
        chunk.origin = "synthetics".into();
        chunk.tags.insert("origin".into(), "synthetics".into());
        let healthy = test_chunk(vec![test_span(2, 4, 0, 0)], 0);
        let original = vec![chunk, healthy];
        let mut chunks = run_rescue(&processor, &config, original.clone(), RESCUE_NOW);
        assert_eq!(chunks.len(), 2, "every chunk must still be forwarded");
        assert_eq!(errors_sr(&chunks[0].spans[0]), Some(1.0));
        assert_eq!(errors_sr(&chunks[0].spans[1]), None);
        chunks[0].spans[0]
            .metrics
            .remove(super::ERRORS_SR_METRIC_KEY);
        assert_eq!(chunks, original);
    }

    #[test]
    #[tracing_test::traced_test]
    fn test_disabled_sampler_initialization_warning() {
        let warning = "error_rare_sample_tracer_drop is enabled but DD_APM_ERROR_TPS <= 0 disables error rescue";
        let mut expected_warnings = 0;
        for feature in [false, true] {
            for tps in [0.0, -1.0, 10.0] {
                let mut config = rescue_config(always_keep_sampler_config());
                config.error_rare_sample_tracer_drop = feature;
                config.error_sampler.target_tps = tps;
                let processor = trace_processor::ServerlessTraceProcessor::new(
                    None,
                    trace_processor::ServerlessTraceProcessor::new_error_sampler(&config),
                );
                expected_warnings += usize::from(feature && tps <= 0.0);
                logs_assert(|lines| {
                    let count = lines.iter().filter(|line| line.contains(warning)).count();
                    if count == expected_warnings {
                        Ok(())
                    } else {
                        Err(format!(
                            "expected {expected_warnings} warnings, got {count}"
                        ))
                    }
                });
                for _ in 0..3 {
                    run_rescue(
                        &processor.clone(),
                        &config,
                        vec![errored_root_chunk(1, -1)],
                        RESCUE_NOW,
                    );
                }
            }
        }
        logs_assert(|lines| {
            let count = lines
                .iter()
                .filter(|line| line.contains("DD_APM_ERROR_TPS <= 0 disables error rescue"))
                .count();
            if count == 2 {
                Ok(())
            } else {
                Err(format!("expected two initialization warnings, got {count}"))
            }
        });
    }

    #[test]
    fn test_rescue_stamps_errored_p0_root_and_keeps_priority() {
        let (config, processor) = rescue_setup(always_keep_sampler_config());

        let chunks = run_rescue(
            &processor,
            &config,
            vec![errored_root_chunk(0xdead_beef, 0)],
            RESCUE_NOW,
        );

        assert_eq!(chunks.len(), 1, "rescued chunk must still be forwarded");
        assert_eq!(chunks[0].priority, 0, "priority must not be promoted");
        assert_eq!(errors_sr(root(&chunks[0])), Some(1.0));
    }

    #[test]
    fn test_rescue_uses_resolved_root_not_first_span() {
        let (config, processor) = rescue_setup(always_keep_sampler_config());

        // Root (parent_id 0) appears last; the first span is a child.
        let spans = vec![
            test_span(0xbeef, 0x201, 0x203, 0),
            test_span(0xbeef, 0x202, 0x203, 1),
            test_span(0xbeef, 0x203, 0, 0),
        ];
        let chunks = run_rescue(&processor, &config, vec![test_chunk(spans, 0)], RESCUE_NOW);

        assert_eq!(chunks.len(), 1);
        assert_eq!(
            errors_sr(&chunks[0].spans[2]),
            Some(1.0),
            "the resolved root (last span) must be stamped"
        );
        assert_eq!(errors_sr(&chunks[0].spans[0]), None);
        assert_eq!(errors_sr(&chunks[0].spans[1]), None);
    }

    #[test]
    fn test_no_error_flags_leaves_chunk_unchanged_and_spends_no_budget() {
        // With RateLimited, counting is global across signatures via
        // `all_sigs_seen`, so if the healthy chunks below were wrongly fed to
        // the sampler, the final errored chunk's rate would drop below 1.0 and
        // it would not be stamped with 1.0. Correct behavior: only errored
        // chunks reach the sampler, so the errored chunk is the first count and
        // is kept at rate 1.0.
        let (config, processor) = rescue_setup(rate_limited_sampler_config(1.0));

        // Healthy P0 chunks with distinct trace IDs, then one errored chunk.
        let mut chunks: Vec<pb::TraceChunk> = (0..9_u64)
            .map(|i| {
                let trace_id = 0xa000 + i;
                test_chunk(vec![test_span(trace_id, trace_id + 1, 0, 0)], 0)
            })
            .collect();
        chunks.push(errored_root_chunk(0xb000, 0));

        let chunks = run_rescue(&processor, &config, chunks, RESCUE_NOW);

        for (i, chunk) in chunks.iter().enumerate() {
            let is_last = i == chunks.len() - 1;
            assert_eq!(
                errors_sr(root(chunk)),
                if is_last { Some(1.0) } else { None },
                "chunk {i}: only the errored chunk may be rescued"
            );
        }
    }

    #[test]
    fn test_empty_chunk_and_fallback_root() {
        for (feature, priority) in [(false, 0), (true, -1)] {
            let (mut config, processor) = rescue_setup(always_keep_sampler_config());
            config.error_rare_sample_tracer_drop = feature;
            let context = format!("feature={feature} priority={priority}");

            // Eligible empty chunks return at the no-error guard before root resolution.
            // The cyclic chunk has no parent_id 0, so the last span is the fallback root.
            let cyclic = test_chunk(
                vec![
                    test_span(0xd00d, 0xd01, 0xd02, 1),
                    test_span(0xd00d, 0xd02, 0xd01, 0),
                ],
                priority,
            );
            let chunks = run_rescue(
                &processor,
                &config,
                vec![test_chunk(vec![], priority), cyclic],
                RESCUE_NOW,
            );

            assert_eq!(chunks.len(), 2, "{context}");
            assert!(
                chunks[0].spans.is_empty(),
                "empty chunk unchanged: {context}"
            );
            assert_eq!(chunks[0].priority, priority, "{context}");
            assert_eq!(chunks[1].priority, priority, "{context}");
            assert_eq!(
                errors_sr(&chunks[1].spans[0]),
                None,
                "non-root span not stamped: {context}"
            );
            assert_eq!(
                errors_sr(&chunks[1].spans[1]),
                Some(1.0),
                "fallback root (last span) stamped: {context}"
            );
        }
    }

    #[tokio::test]
    async fn test_rescue_stamps_eligible_chunks_when_agent_stats_disabled() {
        let config = Arc::new(Config {
            agent_stats_computation_enabled: false,
            ..rescue_config(always_keep_sampler_config())
        });
        let (stats_tx, mut stats_rx) = tokio::sync::mpsc::unbounded_channel();
        let processor = trace_processor::ServerlessTraceProcessor::new(
            Some(super::StatsConcentratorHandle::new(stats_tx)),
            sampler_for(&config.error_sampler),
        );
        let (tx, mut rx) = mpsc::channel(1);
        let res = processor
            .process_traces(
                config,
                errored_request(0, None),
                tx,
                Arc::new(create_test_metadata()),
            )
            .await;
        assert!(res.is_ok());
        assert!(
            matches!(
                stats_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "stats must not be submitted when agent stats computation is disabled"
        );
        let send_data = rx.recv().await.expect("payload forwarded");
        let TracerPayloadCollection::V07(payloads) = send_data.get_payloads() else {
            panic!("expected V07 payload");
        };
        assert_eq!(errors_sr(&payloads[0].chunks[0].spans[0]), Some(1.0));
    }

    #[test]
    fn test_disabled_tps_disables_rescue_in_both_modes() {
        for mode in [ErrorSamplerMode::RateLimited, ErrorSamplerMode::AlwaysKeep] {
            for tps in [0.0_f64, -3.0] {
                let (mut config, processor) = rescue_setup(ErrorSamplerConfig {
                    mode,
                    target_tps: tps,
                    extra_sample_rate: 1.0,
                });
                config.error_rare_sample_tracer_drop = true;

                let chunks = run_rescue(
                    &processor,
                    &config,
                    vec![
                        errored_root_chunk(0xe000, 0),
                        errored_root_chunk(0xe001, -1),
                        errored_root_chunk(0xe002, i32::from(i8::MIN)),
                    ],
                    RESCUE_NOW,
                );

                assert_eq!(chunks.len(), 3, "chunks still forwarded");
                for chunk in &chunks {
                    assert_eq!(
                        errors_sr(root(chunk)),
                        None,
                        "mode {mode:?} tps {tps} priority {}: disabled sampler must not rescue",
                        chunk.priority
                    );
                }
            }
        }
    }

    #[test]
    fn test_rate_limited_under_sustained_load_keeps_and_rejects() {
        for sample_rate in [None, Some(0.25_f64)] {
            let sampler_config = rate_limited_sampler_config(10.0);
            let (config, processor) = rescue_setup(sampler_config);
            let context = format!("sample_rate={sample_rate:?}");

            // With the default client rate, 100 chunks with the same signature
            // in one 5-second bucket give a default rate of 10 / (100 / 5) = 0.5.
            // A finite client rate affects both weighting and applied probability.
            // No exact keep count is asserted: this is an adaptive rolling-window
            // sampler, not a hard token bucket.
            let ids: Vec<u64> = (0..100_u64).map(|i| 0x11_0000 + i).collect();
            let input: Vec<pb::TraceChunk> = ids
                .iter()
                .map(|id| {
                    let mut chunk = errored_root_chunk(*id, 0);
                    if let Some(rate) = sample_rate {
                        chunk.spans[0].metrics.insert("_sample_rate".into(), rate);
                    }
                    chunk
                })
                .collect();
            let chunks = run_rescue(&processor, &config, input, RESCUE_NOW);

            let keeps = rescued_count(&chunks);
            let drops = chunks.len() - keeps;
            if sample_rate.is_none() {
                assert!(keeps > 0, "{context}: expected some keeps, got none");
            }
            assert!(drops > 0, "{context}: expected some drops, got none");
            assert_eq!(
                chunks.len(),
                100,
                "{context}: every chunk is still forwarded"
            );

            // Compare every decision against direct crate usage. Set the expected
            // client rate from the input case, independently of the output metric.
            let mut standalone = ErrorsSampler::new(sampler_config);
            for (chunk, id) in chunks.iter().zip(&ids) {
                assert_eq!(
                    chunk.priority, 0,
                    "{context}: priority changed for trace id {id}"
                );
                let views: Vec<SpanView> = chunk.spans.iter().map(super::span_view).collect();
                let mut trace = equivalent_trace_view(chunk, &config.env, &views);
                trace.root_global_sample_rate = sample_rate.unwrap_or(1.0);
                let expected = standalone.sample(RESCUE_NOW, &trace);
                let actual = match errors_sr(root(chunk)) {
                    Some(errors_sr) => SampleDecision::Keep { errors_sr },
                    None => SampleDecision::Drop,
                };
                assert_eq!(
                    actual, expected,
                    "{context}: decision mismatch for trace id {id}"
                );
            }
        }
    }

    #[test]
    fn test_processor_clones_share_one_budget() {
        let sampler_config = rate_limited_sampler_config(1.0);
        let (mut config, processor) = rescue_setup(sampler_config);
        config.error_rare_sample_tracer_drop = true;
        let processor_clone = processor.clone();

        // Ten errored chunks with the same signature but distinct IDs: five
        // through the original, five through the clone. Mix P0, explicit drops,
        // and no-priority chunks. All must draw from the same budget, matching
        // a standalone sampler fed the same sequence.
        let ids: Vec<u64> = (0..10_u64).map(|i| 0x33_0000 + i).collect();
        let mut observed = Vec::new();
        let mut standalone = ErrorsSampler::new(sampler_config);
        for (i, id) in ids.iter().enumerate() {
            let target = if i < 5 { &processor } else { &processor_clone };
            let mut manual = errored_root_chunk(*id, -1);
            manual
                .tags
                .insert(super::DECISION_MAKER_KEY.into(), "-4".into());
            let mut healthy = errored_root_chunk(*id, -1);
            healthy.spans[0].error = 0;
            let excluded = run_rescue(
                target,
                &config,
                vec![manual, healthy, errored_root_chunk(*id, 1)],
                RESCUE_NOW,
            );
            assert_eq!(rescued_count(&excluded), 0);
            let priority = [0, -1, i32::from(i8::MIN)][i % 3];
            let chunks = run_rescue(
                target,
                &config,
                vec![errored_root_chunk(*id, priority)],
                RESCUE_NOW,
            );
            let actual = errors_sr(root(&chunks[0])).is_some();
            let views: Vec<SpanView> = chunks[0].spans.iter().map(super::span_view).collect();
            let trace = equivalent_trace_view(&chunks[0], &config.env, &views);
            let expected = matches!(
                standalone.sample(RESCUE_NOW, &trace),
                SampleDecision::Keep { .. }
            );
            assert_eq!(
                actual, expected,
                "clone {i} decision diverged from the shared-budget standalone sampler"
            );
            observed.push(actual);
        }
        assert!(
            observed.iter().any(|kept| *kept),
            "expected at least one keep"
        );
        assert!(
            !observed.iter().all(|kept| *kept),
            "expected at least one drop"
        );
    }

    #[test]
    fn test_error_rescue_clamps_backward_timestamps_across_clones() {
        let (config, processor) = rescue_setup(always_keep_sampler_config());
        let clone = processor.clone();

        let chunks = run_rescue(
            &processor,
            &config,
            vec![errored_root_chunk(0x10000, 0)],
            RESCUE_NOW,
        );
        assert_eq!(errors_sr(root(&chunks[0])), Some(1.0));
        assert_eq!(processor.lock_sampler().last_sampler_timestamp, RESCUE_NOW);

        let chunks = run_rescue(
            &clone,
            &config,
            vec![errored_root_chunk(0x10001, 0)],
            RESCUE_NOW - 40,
        );
        assert_eq!(errors_sr(root(&chunks[0])), Some(1.0));
        assert_eq!(clone.lock_sampler().last_sampler_timestamp, RESCUE_NOW);
    }

    #[test]
    fn test_error_rescue_reads_clock_under_lock() {
        let (config, processor) = rescue_setup(always_keep_sampler_config());
        let mut payload = TracerPayloadCollection::V07(vec![
            pb::TracerPayload {
                chunks: vec![
                    errored_root_chunk(0x10000, 1),
                    errored_root_chunk(0x10001, 0),
                ],
                ..Default::default()
            },
            pb::TracerPayload {
                chunks: vec![errored_root_chunk(0x20000, 0)],
                ..Default::default()
            },
        ]);
        let mut clock_called = false;

        processor.apply_error_rescue_with_clock(&mut payload, &config, || {
            clock_called = true;
            assert!(matches!(
                processor.error_rescue_state.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            RESCUE_NOW
        });

        assert!(clock_called);
        assert_eq!(processor.lock_sampler().last_sampler_timestamp, RESCUE_NOW);
        let TracerPayloadCollection::V07(payloads) = payload else {
            panic!("expected V07 payload");
        };
        assert_eq!(errors_sr(root(&payloads[0].chunks[0])), None);
        assert_eq!(errors_sr(root(&payloads[0].chunks[1])), Some(1.0));
        assert_eq!(errors_sr(root(&payloads[1].chunks[0])), Some(1.0));
    }

    #[test]
    fn test_error_rescue_without_candidates_skips_sampler() {
        let (config, processor) = rescue_setup(always_keep_sampler_config());
        let original = pb::TracerPayload {
            chunks: vec![
                // Kept priority: not eligible.
                errored_root_chunk(0x10000, 1),
                // Eligible priority but no errored span.
                test_chunk(vec![test_span(0x20000, 0x20001, 0, 0)], 0),
                // Negative priority with the tracer-drop feature disabled.
                errored_root_chunk(0x30000, -1),
            ],
            ..Default::default()
        };
        let mut payload = TracerPayloadCollection::V07(vec![original.clone()]);

        processor.apply_error_rescue_with_clock(&mut payload, &config, || {
            panic!("a payload without rescue candidates must not consult the sampler");
        });

        assert_eq!(processor.lock_sampler().last_sampler_timestamp, i64::MIN);
        let TracerPayloadCollection::V07(payloads) = payload else {
            panic!("rescue must preserve the payload format");
        };
        assert_eq!(payloads, vec![original]);
    }

    #[test]
    fn test_disabled_error_rescue_does_not_read_clock() {
        for mode in [ErrorSamplerMode::RateLimited, ErrorSamplerMode::AlwaysKeep] {
            for target_tps in [0.0, -3.0] {
                let (config, processor) = rescue_setup(ErrorSamplerConfig {
                    mode,
                    target_tps,
                    extra_sample_rate: 1.0,
                });
                let original = pb::TracerPayload {
                    chunks: vec![errored_root_chunk(0x10000, 0)],
                    ..Default::default()
                };
                let mut payload = TracerPayloadCollection::V07(vec![original.clone()]);

                processor.apply_error_rescue_with_clock(&mut payload, &config, || {
                    panic!("disabled sampler must not read the clock");
                });

                assert_eq!(processor.lock_sampler().last_sampler_timestamp, i64::MIN);
                let TracerPayloadCollection::V07(payloads) = payload else {
                    panic!("disabled rescue must preserve the payload format");
                };
                assert_eq!(payloads, vec![original]);
            }
        }
    }

    #[test]
    fn test_independent_processors_share_sampler_timestamp_floor() {
        let sampler = sampler_for(&rate_limited_sampler_config(1.0));
        let first = trace_processor::ServerlessTraceProcessor::new(None, Arc::clone(&sampler));
        let second = trace_processor::ServerlessTraceProcessor::new(None, sampler);

        assert_eq!(first.lock_sampler().last_sampler_timestamp, i64::MIN);
        assert_eq!(first.lock_sampler().clamp_timestamp(200), 200);
        assert_eq!(second.lock_sampler().clamp_timestamp(50), 200);
        assert_eq!(second.lock_sampler().clamp_timestamp(300), 300);
        assert_eq!(first.lock_sampler().clamp_timestamp(250), 300);
    }

    #[test]
    fn test_sampler_timestamp_clamp_never_moves_backwards() {
        let (_config, processor) = rescue_setup(rate_limited_sampler_config(1.0));
        // A clone shares the clamp floor with the original.
        let clone = processor.clone();

        assert_eq!(processor.lock_sampler().last_sampler_timestamp, i64::MIN);
        assert_eq!(processor.lock_sampler().clamp_timestamp(100), 100);
        // Backward timestamps, through either instance, are clamped to the
        // last value seen; forward ones update the floor.
        assert_eq!(clone.lock_sampler().clamp_timestamp(50), 100);
        assert_eq!(processor.lock_sampler().clamp_timestamp(100), 100);
        assert_eq!(clone.lock_sampler().clamp_timestamp(200), 200);
        assert_eq!(processor.lock_sampler().clamp_timestamp(199), 200);
    }

    #[test]
    fn test_resolve_payload_env_prefers_payload_env() {
        assert_eq!(
            super::resolve_payload_env("tracer-env", "agent-env"),
            "tracer-env",
            "nonempty payload env wins"
        );
        assert_eq!(
            super::resolve_payload_env("", "agent-env"),
            "agent-env",
            "empty payload env falls back to the agent config env"
        );
        assert_eq!(
            super::resolve_payload_env("", ""),
            "",
            "both empty stays empty"
        );
    }

    #[tokio::test]
    async fn test_stats_concentrator_observes_pre_rescue_chunks() {
        let mut config = rescue_config(always_keep_sampler_config());
        config.error_rare_sample_tracer_drop = true;
        let config = Arc::new(config);
        let (stats_tx, mut stats_rx) = tokio::sync::mpsc::unbounded_channel();
        let processor = trace_processor::ServerlessTraceProcessor::new(
            Some(super::StatsConcentratorHandle::new(stats_tx)),
            sampler_for(&config.error_sampler),
        );
        for (priority, dm, rescued) in [(0, None, true), (-1, Some("-4"), false)] {
            let (tx, mut rx) = mpsc::channel(1);
            let res = processor
                .process_traces(
                    config.clone(),
                    errored_request(priority, dm),
                    tx,
                    Arc::new(create_test_metadata()),
                )
                .await;
            assert!(res.is_ok());
            let chunk = match stats_rx.try_recv().expect("stats chunk") {
                crate::stats_concentrator_service::ConcentratorCommand::AddChunk(chunk, _) => {
                    *chunk
                }
                _ => panic!("expected AddChunk"),
            };
            assert_eq!(chunk.priority, priority);
            assert_eq!(
                chunk
                    .tags
                    .get(super::DECISION_MAKER_KEY)
                    .map(String::as_str),
                dm
            );
            assert!(
                chunk.spans.iter().all(|span| errors_sr(span).is_none()),
                "stats precede stamping"
            );
            let send_data = rx.recv().await.expect("every chunk forwarded");
            let TracerPayloadCollection::V07(payloads) = send_data.get_payloads() else {
                panic!("expected V07");
            };
            let forwarded = &payloads[0].chunks[0];
            assert_eq!(forwarded.priority, priority);
            assert_eq!(
                errors_sr(
                    &forwarded.spans[trace_utils::get_root_span_index(&forwarded.spans).unwrap()]
                ),
                rescued.then_some(1.0)
            );
        }
    }
}
