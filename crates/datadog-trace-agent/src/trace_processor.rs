// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use async_trait::async_trait;
use datadog_agent_trace_sampler::{ErrorsSampler, SampleDecision, SpanView, TraceView};
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

/// The root-span metric the backend uses to rescue traces the ordinary P0 drop
/// would discard: a positive `_dd.errors_sr` resolves the chunk's ingestion
/// reason to `error`, which is retained at low priority without any priority
/// promotion.
const ERRORS_SR_METRIC_KEY: &str = "_dd.errors_sr";

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

#[derive(Clone)]
pub struct ServerlessTraceProcessor {
    pub stats_concentrator: Option<StatsConcentratorHandle>,
    enqueue_permits: Arc<tokio::sync::Semaphore>,
    /// Shared error rescue sampler. The `Arc` means processor clones (one per
    /// connection) share a single sampler state and TPS budget for the whole
    /// process lifetime.
    error_sampler: Arc<Mutex<ErrorsSampler>>,
    /// Last timestamp handed to the sampler, shared across clones. Clamps the
    /// clock so a backward wall-clock step cannot move sampler bucket IDs
    /// backwards. Updated only under the sampler lock.
    last_sampler_timestamp: Arc<AtomicI64>,
}

impl ServerlessTraceProcessor {
    #[allow(clippy::must_use_candidate)]
    pub fn new(
        stats_concentrator: Option<StatsConcentratorHandle>,
        error_sampler: Arc<Mutex<ErrorsSampler>>,
    ) -> Self {
        ServerlessTraceProcessor {
            stats_concentrator,
            enqueue_permits: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT_ENQUEUES)),
            error_sampler,
            last_sampler_timestamp: Arc::new(AtomicI64::new(i64::MIN)),
        }
    }

    /// Builds the error rescue sampler from parsed config settings.
    #[must_use]
    pub fn new_error_sampler(config: &Config) -> Arc<Mutex<ErrorsSampler>> {
        Arc::new(Mutex::new(ErrorsSampler::new(config.error_sampler)))
    }

    /// Locks the shared sampler, recovering from a poisoned guard (a panic in
    /// another thread while holding the lock) instead of panicking on unlock.
    fn lock_sampler(&self) -> MutexGuard<'_, ErrorsSampler> {
        self.error_sampler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Applies the error rescue pass to a payload collection.
    ///
    /// For every V07 chunk that would be dropped by the backend's ordinary P0
    /// drop (chunk priority is exactly 0, i.e. an automatic drop, not an
    /// explicit user drop or the no-priority sentinel) and contains at least
    /// one span with a non-zero error flag, consults the shared error sampler.
    /// On a keep, stamps the sampler's `_dd.errors_sr` on the chunk's root
    /// span; the backend keeps such chunks without any priority promotion.
    /// Chunks are never removed or reordered: unrescued chunks stay in the
    /// payload and the backend discards them.
    ///
    fn apply_error_rescue(&self, payload: &mut TracerPayloadCollection, config: &Config) {
        let mut sampler = self.lock_sampler();
        // Skip all view construction when the sampler is disabled by config
        // (target_tps <= 0): nothing can be rescued.
        if sampler.is_disabled() {
            return;
        }
        // Read the clock while holding the sampler lock so that concurrent
        // requests deliver timestamps in lock-acquisition order, and clamp so a
        // backward wall-clock step cannot move the sampler's rolling window
        // backwards, which would undercount TPS and rescue too many chunks.
        let now_unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let now_unix_secs = self.clamp_sampler_timestamp(now_unix_secs);
        self.rescue_with_sampler(payload, config, now_unix_secs, &mut sampler);
    }

    /// Test-only variant that injects a synthetic timestamp so tests can
    /// exercise the rolling window without sleeping.
    #[cfg(test)]
    fn apply_error_rescue_at(
        &self,
        payload: &mut TracerPayloadCollection,
        config: &Config,
        now_unix_secs: i64,
    ) {
        let mut sampler = self.lock_sampler();
        if sampler.is_disabled() {
            return;
        }
        self.rescue_with_sampler(payload, config, now_unix_secs, &mut sampler);
    }

    /// Clamps the timestamp so it never moves backwards relative to the last
    /// one handed to the shared sampler, and records it as the new floor. Must
    /// be called while holding the sampler lock, alongside the clock read, so
    /// concurrent requests cannot interleave a read with the clamp.
    fn clamp_sampler_timestamp(&self, now_unix_secs: i64) -> i64 {
        let previous = self
            .last_sampler_timestamp
            .fetch_max(now_unix_secs, Ordering::Relaxed);
        now_unix_secs.max(previous)
    }

    fn rescue_with_sampler(
        &self,
        payload: &mut TracerPayloadCollection,
        config: &Config,
        now_unix_secs: i64,
        sampler: &mut ErrorsSampler,
    ) {
        let TracerPayloadCollection::V07(tracer_payloads) = payload else {
            return;
        };
        for tracer_payload in tracer_payloads.iter_mut() {
            // The sampler keys its per-signature rate limits on the env the
            // tracer reported for this payload, falling back to the agent's
            // configured env, consistent with how stats are flushed.
            let env: &str = resolve_payload_env(&tracer_payload.env, &config.env);
            for chunk in tracer_payload.chunks.iter_mut() {
                sample_and_stamp(sampler, chunk, env, now_unix_secs);
            }
        }
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

/// Chooses the env used to key the error sampler's per-signature rates: the
/// tracer payload's own env when nonempty, otherwise the agent's configured
/// env. This matches how stats prefer the payload env and fall back to the
/// agent config env.
fn resolve_payload_env<'a>(tracer_payload_env: &'a str, config_env: &'a str) -> &'a str {
    if tracer_payload_env.is_empty() {
        config_env
    } else {
        tracer_payload_env
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

/// Consults the error sampler for one chunk and, on a keep, stamps the
/// sampler's `_dd.errors_sr` value on the chunk's root span.
///
/// Only automatic-drop chunks are candidates: chunk priority must be exactly 0.
/// Explicit user drops (-1), other negative priorities, positive priorities,
/// and the no-priority sentinel (`i8::MIN`) are all left untouched. The chunk
/// must contain at least one span with a non-zero error flag; HTTP status or
/// error metadata alone does not qualify. The root is resolved with
/// `get_root_span_index`: empty chunks are left unchanged, and non-empty
/// chunks always resolve a root, falling back to the last span when no span
/// has `parent_id 0` (e.g. a cyclic chunk). Chunks are never removed here: on
/// a Drop decision the unrescued chunk is forwarded as-is and the backend's
/// ordinary P0 drop handles it.
fn sample_and_stamp(
    sampler: &mut ErrorsSampler,
    chunk: &mut pb::TraceChunk,
    env: &str,
    now_unix_secs: i64,
) {
    if chunk.priority != 0 {
        return;
    }
    // An error anywhere in the chunk makes it a rescue candidate, not just an
    // error on the root span. Checked before the root-span search because
    // non-errored chunks are the common case on this path.
    if !chunk.spans.iter().any(|span| span.error != 0) {
        return;
    }
    let Ok(root_index) = trace_utils::get_root_span_index(&chunk.spans) else {
        // No identifiable root span: leave the chunk unchanged rather than
        // guessing an index.
        return;
    };

    let Some(root) = chunk.spans.get(root_index) else {
        return;
    };
    let views: Vec<SpanView> = chunk.spans.iter().map(span_view).collect();
    let trace = TraceView {
        env,
        trace_id: root.trace_id,
        root_index,
        // The raw `_sample_rate` wire value is passed through: the shared
        // sampler sanitizes non-finite or out-of-range rates to 1.0 itself.
        root_global_sample_rate: root.metrics.get("_sample_rate").copied().unwrap_or(1.0),
        spans: &views,
    };
    let decision = sampler.sample(now_unix_secs, &trace);

    if let SampleDecision::Keep { errors_sr } = decision
        && let Some(root) = chunk.spans.get_mut(root_index)
    {
        root.metrics
            .insert(ERRORS_SR_METRIC_KEY.to_string(), errors_sr);
    }
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

        // Error rescue runs after stats submission so the concentrator observes every
        // submitted chunk exactly as the tracer sent it, and before payload splitting so
        // the newly inserted metric is included in the recomputed outbound size. It is
        // gated on agent stats computation: without it, P0 chunks are not expected here.
        if config.agent_stats_computation_enabled {
            self.apply_error_rescue(&mut payload, &config);
        }

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
            self, MAX_IN_FLIGHT_ENQUEUES, TRACER_PAYLOAD_FUNCTION_TAGS_TAG_KEY, TraceProcessor,
            encoded_size, split_oversized_payloads,
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
        }
    }

    fn default_error_sampler() -> Arc<std::sync::Mutex<ErrorsSampler>> {
        Arc::new(std::sync::Mutex::new(ErrorsSampler::new(
            ErrorSamplerConfig::default(),
        )))
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

    fn sampler_for(config: &ErrorSamplerConfig) -> Arc<std::sync::Mutex<ErrorsSampler>> {
        Arc::new(std::sync::Mutex::new(ErrorsSampler::new(*config)))
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

    #[test]
    fn test_rescue_stamps_errored_p0_root_and_keeps_priority() {
        let config = rescue_config(always_keep_sampler_config());
        let processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&config.error_sampler),
        );

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
    fn test_rescue_stamps_actual_root_when_error_is_on_child() {
        let config = rescue_config(always_keep_sampler_config());
        let processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&config.error_sampler),
        );

        // Healthy root first, errored child second: only the root may be stamped.
        let spans = vec![
            test_span(0xfeed, 0x101, 0, 0),
            test_span(0xfeed, 0x102, 0x101, 1),
        ];
        let chunks = run_rescue(&processor, &config, vec![test_chunk(spans, 0)], RESCUE_NOW);

        assert_eq!(chunks.len(), 1);
        assert_eq!(errors_sr(&chunks[0].spans[0]), Some(1.0), "root stamped");
        assert_eq!(
            errors_sr(&chunks[0].spans[1]),
            None,
            "errored child must not be stamped"
        );
    }

    #[test]
    fn test_rescue_uses_resolved_root_not_first_span() {
        let config = rescue_config(always_keep_sampler_config());
        let processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&config.error_sampler),
        );

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
        let config = rescue_config(rate_limited_sampler_config(1.0));
        let processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&config.error_sampler),
        );

        let mut chunks = Vec::new();
        for i in 0..9_u64 {
            chunks.push(errored_root_chunk(0xa000 + i, 0));
            // Strip the error flag: healthy P0 chunk with a distinct signature.
            chunks.last_mut().unwrap().spans[0].error = 0;
        }
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
    fn test_http_500_metadata_alone_is_not_an_error() {
        let config = rescue_config(always_keep_sampler_config());
        let processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&config.error_sampler),
        );

        let mut span = test_span(0xc0de, 0xc0de + 1, 0, 0);
        span.meta
            .insert("http.status_code".to_string(), "500".to_string());
        span.meta
            .insert("error.type".to_string(), "Error".to_string());

        let chunks = run_rescue(
            &processor,
            &config,
            vec![test_chunk(vec![span], 0)],
            RESCUE_NOW,
        );

        assert_eq!(chunks.len(), 1);
        assert_eq!(errors_sr(root(&chunks[0])), None);
    }

    #[test]
    fn test_non_automatic_drop_priorities_are_never_rescued() {
        let config = rescue_config(always_keep_sampler_config());
        let processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&config.error_sampler),
        );

        // -1 (explicit user drop), other negatives, positive priorities, and
        // the no-priority sentinel (i8::MIN) are all out of scope.
        let priorities = [-1_i32, -5, 1, 2, i8::MIN as i32];
        let chunks = run_rescue(
            &processor,
            &config,
            priorities
                .iter()
                .map(|p| errored_root_chunk(0x1000_u64 + (*p).unsigned_abs() as u64, *p))
                .collect(),
            RESCUE_NOW,
        );

        for chunk in &chunks {
            assert_eq!(
                errors_sr(root(chunk)),
                None,
                "priority {} must not be rescued",
                chunk.priority
            );
        }
    }

    #[test]
    fn test_empty_chunk_does_not_panic_and_stays_unchanged() {
        let config = rescue_config(always_keep_sampler_config());
        let processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&config.error_sampler),
        );

        // An empty chunk cannot be scored: root resolution fails and it must be
        // left unchanged without panicking. For non-empty chunks,
        // `get_root_span_index` always resolves (falling back to the last span
        // when no span has parent_id 0), so the cyclic chunk below exercises
        // the fallback path and gets stamped on the resolved fallback root.
        let cyclic = test_chunk(
            vec![
                test_span(0xd00d, 0xd01, 0xd02, 1),
                test_span(0xd00d, 0xd02, 0xd01, 0),
            ],
            0,
        );

        let chunks = run_rescue(
            &processor,
            &config,
            vec![test_chunk(vec![], 0), cyclic],
            RESCUE_NOW,
        );

        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].spans.is_empty(), "empty chunk unchanged");
        assert_eq!(
            errors_sr(&chunks[1].spans[1]),
            Some(1.0),
            "fallback root (last span) stamped"
        );
        assert_eq!(errors_sr(&chunks[1].spans[0]), None);
    }

    #[tokio::test]
    async fn test_rescue_is_noop_when_agent_stats_disabled() {
        let config = rescue_config(always_keep_sampler_config());
        let config = Config {
            agent_stats_computation_enabled: false,
            ..config
        };
        let (tx, mut rx): (
            Sender<trace_utils::SendData>,
            tokio::sync::mpsc::Receiver<trace_utils::SendData>,
        ) = mpsc::channel(1);

        let start = get_current_timestamp_nanos();
        let mut json_span = create_test_json_span(11, 222, 333, start, true);
        // Root span with an error and an automatic-drop priority: eligible.
        json_span["error"] = serde_json::json!(1);
        json_span["metrics"]["_sampling_priority_v1"] = serde_json::json!(0.0);
        let bytes = rmp_serde::to_vec(&vec![vec![json_span]]).unwrap();
        let request = Request::builder()
            .header("datadog-meta-tracer-version", "4.0.0")
            .header("datadog-meta-lang", "nodejs")
            .header("datadog-meta-lang-version", "v19.7.0")
            .header("datadog-meta-lang-interpreter", "v8")
            .header("content-length", "100")
            .body(http_common::Body::from(bytes))
            .unwrap();

        let trace_processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&config.error_sampler),
        );
        let res = trace_processor
            .process_traces(
                Arc::new(config),
                request,
                tx,
                Arc::new(create_test_metadata()),
            )
            .await;
        assert!(res.is_ok());

        let send_data = rx.recv().await.expect("payload forwarded");
        let payloads = send_data.get_payloads();
        let TracerPayloadCollection::V07(tracer_payloads) = payloads else {
            panic!("expected V07 payload");
        };
        let chunk = &tracer_payloads[0].chunks[0];
        assert_eq!(chunk.priority, 0);
        for span in &chunk.spans {
            assert_eq!(
                errors_sr(span),
                None,
                "rescue must be a no-op when agent stats computation is disabled"
            );
        }
    }

    #[test]
    fn test_disabled_tps_disables_rescue_in_both_modes() {
        for mode in [ErrorSamplerMode::RateLimited, ErrorSamplerMode::AlwaysKeep] {
            for tps in [0.0_f64, -3.0] {
                let sampler_config = ErrorSamplerConfig {
                    mode,
                    target_tps: tps,
                    extra_sample_rate: 1.0,
                };
                let config = rescue_config(sampler_config);
                let processor = trace_processor::ServerlessTraceProcessor::new(
                    None,
                    sampler_for(&config.error_sampler),
                );

                let chunks = run_rescue(
                    &processor,
                    &config,
                    vec![errored_root_chunk(0xe000, 0)],
                    RESCUE_NOW,
                );

                assert_eq!(chunks.len(), 1, "chunk still forwarded");
                assert_eq!(
                    errors_sr(root(&chunks[0])),
                    None,
                    "mode {mode:?} tps {tps}: disabled sampler must not rescue"
                );
            }
        }
    }

    #[test]
    fn test_always_keep_rescues_every_eligible_chunk() {
        let config = rescue_config(always_keep_sampler_config());
        let processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&config.error_sampler),
        );

        let chunks = run_rescue(
            &processor,
            &config,
            (0..10_u64)
                .map(|i| errored_root_chunk(0xf000 + i, 0))
                .collect(),
            RESCUE_NOW,
        );

        assert_eq!(chunks.len(), 10);
        for chunk in &chunks {
            assert_eq!(errors_sr(root(chunk)), Some(1.0));
            assert_eq!(chunk.priority, 0);
        }
    }

    #[test]
    fn test_rate_limited_under_sustained_load_keeps_and_rejects() {
        let sampler_config = rate_limited_sampler_config(10.0);
        let config = rescue_config(sampler_config);
        let processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&config.error_sampler),
        );

        // 100 chunks with the same signature in one 5-second bucket: the
        // default rate is 10 / (100 / 5) = 0.5, so a deterministic mix of keeps
        // and rejections is expected. No exact keep count is asserted: the
        // sampler is an adaptive rolling-window sampler, not a hard token
        // bucket.
        let ids: Vec<u64> = (0..100_u64).map(|i| 0x11_0000 + i).collect();
        let input: Vec<pb::TraceChunk> = ids.iter().map(|id| errored_root_chunk(*id, 0)).collect();
        let chunks = run_rescue(&processor, &config, input, RESCUE_NOW);

        let keeps = chunks
            .iter()
            .filter(|c| errors_sr(root(c)).is_some())
            .count();
        let drops = chunks.len() - keeps;
        assert!(keeps > 0, "expected some keeps, got none");
        assert!(drops > 0, "expected some drops, got none");
        assert_eq!(chunks.len(), 100, "every chunk is still forwarded");
        for chunk in &chunks {
            assert_eq!(chunk.priority, 0, "priority unchanged on rescue decision");
        }

        // The same input through a standalone shared sampler must produce the
        // identical decision sequence, validating the adapter wiring (env,
        // sample rate, views) against direct crate usage.
        let mut standalone = ErrorsSampler::new(sampler_config);
        for (chunk, id) in chunks.iter().zip(&ids) {
            let views: Vec<SpanView> = chunk
                .spans
                .iter()
                .map(|s| SpanView {
                    service: &s.service,
                    name: &s.name,
                    resource: &s.resource,
                    error: s.error != 0,
                    http_status_code: s.meta.get("http.status_code").map(String::as_str),
                    error_type: s.meta.get("error.type").map(String::as_str),
                })
                .collect();
            let trace = equivalent_trace_view(chunk, &config.env, &views);
            let expected = standalone.sample(RESCUE_NOW, &trace);
            let actual = match errors_sr(root(chunk)) {
                Some(errors_sr) => SampleDecision::Keep { errors_sr },
                None => SampleDecision::Drop,
            };
            assert_eq!(actual, expected, "decision mismatch for trace id {id}");
        }
    }

    #[test]
    fn test_rate_limited_bucket_transitions_and_steady_state() {
        let sampler_config = rate_limited_sampler_config(10.0);
        let config = rescue_config(sampler_config);
        let processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&config.error_sampler),
        );

        // First bucket: 100 distinct signatures push the default rate to 0.5.
        let ids: Vec<u64> = (0..100_u64).map(|i| 0x22_0000 + i).collect();
        let first: Vec<pb::TraceChunk> = ids.iter().map(|id| errored_root_chunk(*id, 0)).collect();
        let first_chunks = run_rescue(&processor, &config, first, RESCUE_NOW);
        let first_keeps = first_chunks
            .iter()
            .filter(|c| errors_sr(root(c)).is_some())
            .count();
        assert!(
            first_keeps > 0 && first_keeps < 100,
            "mixed decisions in the first bucket"
        );

        // A full window later (the rolling window is 6 buckets of 5 seconds),
        // the same IDs in a fresh bucket still produce mixed decisions, and
        // every chunk is still forwarded.
        let second: Vec<pb::TraceChunk> = ids.iter().map(|id| errored_root_chunk(*id, 0)).collect();
        let second_chunks = run_rescue(&processor, &config, second, RESCUE_NOW + 40);
        assert_eq!(second_chunks.len(), 100);
        let second_keeps = second_chunks
            .iter()
            .filter(|c| errors_sr(root(c)).is_some())
            .count();
        assert!(
            second_keeps > 0 && second_keeps < 100,
            "mixed decisions after window rotation"
        );
    }

    #[test]
    fn test_processor_clones_share_one_budget() {
        let sampler_config = rate_limited_sampler_config(1.0);
        let config = rescue_config(sampler_config);
        let processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&config.error_sampler),
        );
        let processor_clone = processor.clone();

        // Ten errored chunks with the same signature but distinct IDs: five
        // through the original, five through the clone. All must draw from the
        // same budget, matching a standalone sampler fed the same sequence.
        let ids: Vec<u64> = (0..10_u64).map(|i| 0x33_0000 + i).collect();
        let mut observed = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            let target = if i < 5 { &processor } else { &processor_clone };
            let chunks = run_rescue(
                target,
                &config,
                vec![errored_root_chunk(*id, 0)],
                RESCUE_NOW,
            );
            observed.push(errors_sr(root(&chunks[0])).is_some());
        }

        let mut standalone = ErrorsSampler::new(sampler_config);
        let spans = [SpanView {
            service: "test-service",
            name: "test-operation",
            resource: "GET /test",
            error: true,
            http_status_code: None,
            error_type: None,
        }];
        for (i, id) in ids.iter().enumerate() {
            let trace = TraceView {
                env: &config.env,
                trace_id: *id,
                root_index: 0,
                root_global_sample_rate: 1.0,
                spans: &spans,
            };
            let expected = matches!(
                standalone.sample(RESCUE_NOW, &trace),
                SampleDecision::Keep { .. }
            );
            assert_eq!(
                observed[i], expected,
                "clone {i} decision diverged from the shared-budget standalone sampler"
            );
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
    fn test_sampler_timestamp_clamp_never_moves_backwards() {
        let processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&rate_limited_sampler_config(1.0)),
        );
        // A clone shares the clamp floor with the original.
        let clone = processor.clone();

        assert_eq!(processor.clamp_sampler_timestamp(100), 100);
        // Backward timestamps, through either instance, are clamped to the
        // last value seen; forward ones update the floor.
        assert_eq!(clone.clamp_sampler_timestamp(50), 100);
        assert_eq!(processor.clamp_sampler_timestamp(100), 100);
        assert_eq!(clone.clamp_sampler_timestamp(200), 200);
        assert_eq!(processor.clamp_sampler_timestamp(199), 200);
    }

    #[test]
    fn test_rescue_uses_payload_env_with_config_fallback() {
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

    #[test]
    fn test_rescue_preserves_chunk_metadata_and_span_order() {
        let config = rescue_config(always_keep_sampler_config());
        let processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&config.error_sampler),
        );

        let mut root_span = test_span(0x501d, 0x502, 0, 0);
        root_span
            .metrics
            .insert("_sampling_priority_v1".to_string(), 0.0);
        root_span.metrics.insert("_sample_rate".to_string(), 0.25);
        root_span
            .metrics
            .insert("_dd.span_sampling.rule".to_string(), 1.0);
        let mut child = test_span(0x501d, 0x503, 0x502, 1);
        child.meta.insert("keep".to_string(), "me".to_string());

        let mut chunk = test_chunk(vec![root_span, child], 0);
        chunk.tags.insert("_dd.p.dm".to_string(), "-4".to_string());
        chunk
            .tags
            .insert("origin".to_string(), "synthetics".to_string());
        chunk.origin = "synthetics".to_string();

        let chunks = run_rescue(&processor, &config, vec![chunk], RESCUE_NOW);

        assert_eq!(chunks.len(), 1);
        let rescued = &chunks[0];
        assert_eq!(rescued.priority, 0, "priority preserved");
        assert_eq!(rescued.origin, "synthetics", "origin preserved");
        assert_eq!(
            rescued.tags.get("_dd.p.dm").map(String::as_str),
            Some("-4"),
            "decision maker preserved"
        );
        assert_eq!(
            rescued.tags.get("origin").map(String::as_str),
            Some("synthetics")
        );
        assert_eq!(
            rescued.spans[0]
                .metrics
                .get("_dd.span_sampling.rule")
                .copied(),
            Some(1.0),
            "single-span sampling metrics preserved"
        );
        assert_eq!(
            rescued.spans[0]
                .metrics
                .get("_sampling_priority_v1")
                .copied(),
            Some(0.0),
            "sampling priority metric preserved"
        );
        assert_eq!(
            errors_sr(&rescued.spans[0]),
            Some(1.0),
            "root stamped despite existing metrics"
        );
        assert_eq!(
            rescued.spans[1].meta.get("keep").map(String::as_str),
            Some("me"),
            "span metadata and order preserved"
        );
        assert_eq!(rescued.spans.len(), 2, "no spans added or removed");
    }

    #[test]
    fn test_probabilistic_decision_maker_chunk_gets_no_workaround() {
        // A priority-0 chunk carrying the chunk-level probabilistic decision
        // maker (`_dd.p.dm = "-9"`) is rescued like any other automatic-drop
        // chunk: priority and decision maker are preserved untouched. The
        // backend resolves such chunks to the `probabilistic` ingestion reason
        // before checking `_dd.errors_sr`, so it may still drop them; SCL does
        // not work around that limitation.
        let config = rescue_config(always_keep_sampler_config());
        let processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&config.error_sampler),
        );

        let mut chunk = errored_root_chunk(0x600d, 0);
        chunk.tags.insert("_dd.p.dm".to_string(), "-9".to_string());

        let chunks = run_rescue(&processor, &config, vec![chunk], RESCUE_NOW);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].priority, 0, "priority preserved, no promotion");
        assert_eq!(
            chunks[0].tags.get("_dd.p.dm").map(String::as_str),
            Some("-9"),
            "probabilistic decision maker preserved, no rewrite"
        );
        assert_eq!(errors_sr(root(&chunks[0])), Some(1.0));
    }

    #[test]
    fn test_rescue_passes_raw_sample_rate_to_sampler() {
        // The raw `_sample_rate` wire value is passed to the shared sampler,
        // which sanitizes it. A value outside (0, 1] falls back to 1.0, so a
        // bogus rate does not change the stamped rescue rate.
        let sampler_config = rate_limited_sampler_config(10.0);
        let config = rescue_config(sampler_config);
        let processor = trace_processor::ServerlessTraceProcessor::new(
            None,
            sampler_for(&config.error_sampler),
        );

        let mut chunk = errored_root_chunk(0x77_00, 0);
        chunk.spans[0]
            .metrics
            .insert("_sample_rate".to_string(), f64::NAN);

        let chunks = run_rescue(&processor, &config, vec![chunk], RESCUE_NOW);

        // Single signature, well under budget: sanitized rate 1.0 keeps it.
        assert_eq!(errors_sr(root(&chunks[0])), Some(1.0));
    }

    #[tokio::test]
    async fn test_stats_concentrator_observes_pre_rescue_chunks() {
        let config = rescue_config(always_keep_sampler_config());
        let (stats_tx, mut stats_rx) = tokio::sync::mpsc::unbounded_channel();
        let concentrator = super::StatsConcentratorHandle::new(stats_tx);
        let processor = trace_processor::ServerlessTraceProcessor::new(
            Some(concentrator),
            sampler_for(&config.error_sampler),
        );

        let start = get_current_timestamp_nanos();
        let mut json_span = create_test_json_span(11, 222, 333, start, true);
        json_span["error"] = serde_json::json!(1);
        json_span["metrics"]["_sampling_priority_v1"] = serde_json::json!(0.0);
        let bytes = rmp_serde::to_vec(&vec![vec![json_span]]).unwrap();
        let request = Request::builder()
            .header("datadog-meta-tracer-version", "4.0.0")
            .header("datadog-meta-lang", "nodejs")
            .header("datadog-meta-lang-version", "v19.7.0")
            .header("datadog-meta-lang-interpreter", "v8")
            .header("content-length", "100")
            .body(http_common::Body::from(bytes))
            .unwrap();

        let res = processor
            .process_traces(
                Arc::new(config),
                request,
                mpsc::channel(1).0,
                Arc::new(create_test_metadata()),
            )
            .await;
        assert!(res.is_ok());

        // The concentrator must receive the chunk exactly as the tracer sent
        // it: before rescue stamping, with no `_dd.errors_sr` anywhere.
        let (chunk, _metadata) = match stats_rx.try_recv() {
            Ok(crate::stats_concentrator_service::ConcentratorCommand::AddChunk(
                chunk,
                metadata,
            )) => (*chunk, metadata),
            Ok(_) => panic!("expected an AddChunk command"),
            Err(err) => panic!("expected an AddChunk command, got {err}"),
        };
        assert_eq!(chunk.priority, 0);
        for span in &chunk.spans {
            assert_eq!(
                errors_sr(span),
                None,
                "stats must observe the pre-rescue chunk"
            );
        }
    }
}
