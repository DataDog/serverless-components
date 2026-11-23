// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use datadog_agent_trace_sampler::{
    ErrorSamplerConfig, ErrorsSampler, PrioritySampler, SampleDecision, SpanView, TraceView,
};
use http_body_util::BodyExt;
use hyper::{StatusCode, http};
use libdd_common::http_common;
use libdd_library_config::tracer_metadata::TracerMetadata;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, mpsc::Sender};
use tracing::{debug, error, warn};

use libdd_trace_obfuscation::obfuscate::obfuscate_pb_span;
use libdd_trace_obfuscation::obfuscation_config::{DbmsKind, SqlConfig};
use libdd_trace_obfuscation::sql::obfuscate_sql_resource;
use libdd_trace_protobuf::pb;
use libdd_trace_utils::trace_utils::{self};
use libdd_trace_utils::trace_utils::{EnvironmentType, SendData};
use libdd_trace_utils::tracer_payload::{TraceChunkProcessor, TracerPayloadCollection};
use prost::Message;

use crate::{
    aggregator::MAX_CONTENT_SIZE_BYTES,
    config::{Config, InstallSignature, SpanTags, resolve_payload_env},
    http_utils::{self, log_and_create_http_response, log_and_create_traces_success_http_response},
    internal_span_filter,
    stats_concentrator_service::StatsConcentratorHandle,
};

const TRACER_PAYLOAD_FUNCTION_TAGS_TAG_KEY: &str = "_dd.tags.function";

/// The root-span metric used for error rescue without priority promotion.
/// A positive value signals error sampling; backend retention also depends on
/// its ingestion-reason handling (for example, probabilistic decision makers).
const ERRORS_SR_METRIC_KEY: &str = "_dd.errors_sr";
/// The root-span metric with the rate of the sampler for chunks without a priority.
const NO_PRIORITY_RATE_METRIC_KEY: &str = "_dd.no_p_sr";
/// The root-span metric with the priority sampler's rate, which the agent sets when the tracer
/// sent none of the rate metrics.
const PRIORITY_RATE_METRIC_KEY: &str = "_sampling_priority_rate_v1";
/// The root-span metrics with the rates that tracers apply with the agent's rates and with rules.
const TRACER_PRIORITY_RATE_METRIC_KEYS: [&str; 2] = ["_dd.agent_psr", "_dd.rule_psr"];
/// The chunk priority of a trace without a sampling priority.
const PRIORITY_NONE: i32 = -128;
/// The chunk priority that the Go agent gives a chunk kept by single span sampling.
const PRIORITY_USER_KEEP: i32 = 2;
const SPAN_SAMPLING_MECHANISM_KEY: &str = "_dd.span_sampling.mechanism";
/// The analytics event extraction rate of a span.
const EVENT_EXTRACTION_RATE_KEY: &str = "_dd1.sr.eausr";
const DECISION_MAKER_KEY: &str = "_dd.p.dm";
const ORIGIN_KEY: &str = "_dd.origin";
const MANUAL_DECISION_MAKER: &str = "-4";
/// The payload version that the Go agent converted the spans from.
const CONVERTED_V1_KEY: &str = "_dd.convertedv1";
const SQL_QUERY_KEY: &str = "sql.query";
const DBMS_KEY: &str = "db.type";
const INSTALL_ID_KEY: &str = "_dd.install.id";
const INSTALL_TYPE_KEY: &str = "_dd.install.type";
const INSTALL_TIME_KEY: &str = "_dd.install.time";

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

        let go_fields = self.config.agent_payload_fields.is_some();
        if go_fields {
            // The receiver accepts only msgpack v0.4 payloads. Configured tags replace the value.
            for span in &mut chunk.spans {
                span.meta
                    .insert(CONVERTED_V1_KEY.to_string(), "v04".to_string());
            }
        }
        match &self.config.span_tags {
            Some(span_tags) => apply_span_tags(chunk, span_tags),
            None => trace_utils::set_serverless_root_span_tags(
                &mut chunk.spans[root_span_index],
                app_name.clone(),
                &self.config.env_type,
            ),
        }
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
            let sql_query = (go_fields
                && matches!(span.r#type.as_str(), "sql" | "cassandra")
                && !span.resource.is_empty())
            .then(|| (span.resource.clone(), span.meta.get(SQL_QUERY_KEY).cloned()));
            obfuscate_pb_span(span, &self.config.obfuscation_config);
            if let Some((raw_resource, tracer_query)) = sql_query {
                restore_sql_query(
                    span,
                    &raw_resource,
                    tracer_query,
                    &self.config.obfuscation_config.sql,
                );
            }
        }
    }
}

/// Gives `sql.query` the Go agent's value after `obfuscate_pb_span`, which sets it to the
/// obfuscated resource. Go obfuscates only the tracer's non-empty value, reusing the obfuscated
/// resource when the value equals the raw resource, and removes an empty one.
fn restore_sql_query(
    span: &mut pb::Span,
    raw_resource: &str,
    tracer_query: Option<String>,
    config: &SqlConfig,
) {
    match tracer_query.filter(|query| !query.is_empty()) {
        None => {
            span.meta.remove(SQL_QUERY_KEY);
        }
        Some(query) if query == raw_resource => {}
        Some(query) => {
            let dbms: DbmsKind = span
                .meta
                .get(DBMS_KEY)
                .and_then(|dbms| dbms.as_str().try_into().ok())
                .unwrap_or_default();
            span.meta.insert(
                SQL_QUERY_KEY.to_string(),
                obfuscate_sql_resource(&query, config, dbms),
            );
        }
    }
}

/// Adds the configured origin and tags to the chunk and its spans, as the Go trace agent applies
/// its global tags and the serverless span modifier.
fn apply_span_tags(chunk: &mut pb::TraceChunk, span_tags: &SpanTags) {
    if let Some(origin) = span_tags.tags.get(ORIGIN_KEY) {
        chunk.origin = origin.clone();
    } else if chunk.origin.is_empty() {
        chunk.origin = span_tags.origin.clone();
    }
    for span in &mut chunk.spans {
        if !span_tags.origin.is_empty() && span.meta.get(ORIGIN_KEY).is_none_or(String::is_empty) {
            span.meta
                .insert(ORIGIN_KEY.to_string(), span_tags.origin.clone());
        }
        for (key, value) in &span_tags.tags {
            if key != ORIGIN_KEY {
                span.meta.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Maximum number of trace-enqueue tasks that may be in flight (spawned but not yet finished
/// handing their pieces to the flusher) at once
const MAX_IN_FLIGHT_ENQUEUES: usize = 10;

/// A sampler and its monotonic timestamp floor, shared under one mutex.
struct SamplerState<S> {
    sampler: S,
    last_sampler_timestamp: i64,
}

type ScoreSamplerState = SamplerState<ErrorsSampler>;

impl<S> SamplerState<S> {
    fn new(sampler: S) -> Self {
        Self {
            sampler,
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
    /// Processor clones share one TPS budget and timestamp floor under the same lock.
    error_rescue_state: Arc<Mutex<ScoreSamplerState>>,
    /// False when the configured TPS disables the sampler, so rescue never locks.
    error_rescue_enabled: bool,
    /// The sampler for chunks without a priority, present when `agent_payload_fields` is set.
    no_priority_state: Option<Arc<Mutex<ScoreSamplerState>>>,
    /// The priority sampler's rate loop, present when `agent_payload_fields` is set.
    priority_state: Option<Arc<Mutex<SamplerState<PrioritySampler>>>>,
    /// The services whose first sampled trace got the install signature.
    first_trace_services: Arc<std::sync::Mutex<HashSet<String>>>,
}

impl ServerlessTraceProcessor {
    #[allow(clippy::must_use_candidate)]
    pub fn new(stats_concentrator: Option<StatsConcentratorHandle>, config: &Config) -> Self {
        if config.error_rare_sample_tracer_drop && config.error_sampler.target_tps <= 0.0 {
            warn!(
                "error_rare_sample_tracer_drop is enabled but DD_APM_ERROR_TPS <= 0 disables error rescue"
            );
        }
        let error_rescue_state = SamplerState::new(ErrorsSampler::new(config.error_sampler));
        ServerlessTraceProcessor {
            stats_concentrator,
            enqueue_permits: Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT_ENQUEUES)),
            error_rescue_enabled: !error_rescue_state.sampler.is_disabled(),
            error_rescue_state: Arc::new(Mutex::new(error_rescue_state)),
            no_priority_state: config.agent_payload_fields.as_ref().map(|fields| {
                Arc::new(Mutex::new(SamplerState::new(ErrorsSampler::new(
                    ErrorSamplerConfig {
                        target_tps: fields.target_tps,
                        ..ErrorSamplerConfig::default()
                    },
                ))))
            }),
            priority_state: config.agent_payload_fields.as_ref().map(|fields| {
                Arc::new(Mutex::new(SamplerState::new(PrioritySampler::new(
                    fields.target_tps,
                    1.0,
                ))))
            }),
            first_trace_services: Arc::default(),
        }
    }

    /// Runs the agent samplers on V07 chunks, as the Go agent does after it computes stats:
    /// - the priority sampler's rate loop on chunks with priority 0 or 1, see
    ///   [`Self::apply_priority_sampler`];
    /// - the no-priority sampler on chunks without a priority, stamping `_dd.no_p_sr` on kept
    ///   roots;
    /// - the error sampler on the chunks selected by [`rescue_root_index`] and on errored chunks
    ///   that the no-priority sampler drops, stamping `_dd.errors_sr` on kept roots.
    ///
    /// Of a chunk that the no-priority sampler drops and the error sampler does not keep, only what
    /// [`keep_sampled_spans`] keeps is forwarded. A chunk with nothing kept is removed, with the
    /// tracer payloads left empty by it. Every other chunk is forwarded.
    async fn apply_samplers(
        &self,
        payload: &mut TracerPayloadCollection,
        config: &Config,
        client_dropped_p0s: usize,
    ) {
        self.apply_samplers_with_clock(payload, config, client_dropped_p0s, unix_now_secs)
            .await;
    }

    async fn apply_samplers_with_clock(
        &self,
        payload: &mut TracerPayloadCollection,
        config: &Config,
        client_dropped_p0s: usize,
        read_clock: impl Fn() -> i64,
    ) {
        let dropped = self
            .apply_no_priority_sampler(payload, config, &read_clock)
            .await;
        let rescued: HashSet<(usize, usize)> = self
            .apply_error_rescue_with_clock(payload, config, &dropped, &read_clock)
            .await
            .into_iter()
            .collect();
        let dropped: HashSet<(usize, usize)> = dropped.into_iter().collect();
        self.apply_priority_sampler(
            payload,
            config,
            client_dropped_p0s,
            &dropped,
            &rescued,
            &read_clock,
        )
        .await;
        let TracerPayloadCollection::V07(tracer_payloads) = payload else {
            return;
        };
        if let Some(signature) = config
            .agent_payload_fields
            .as_ref()
            .and_then(|fields| fields.install_signature.as_ref())
        {
            for (payload_index, tracer_payload) in tracer_payloads.iter_mut().enumerate() {
                for (chunk_index, chunk) in tracer_payload.chunks.iter_mut().enumerate() {
                    let index = (payload_index, chunk_index);
                    // Single span sampling keeps a chunk that the samplers drop, and Go then tags
                    // its original root, which is sent only when it is one of the kept spans.
                    let sampled = kept_by_samplers(index, chunk, &dropped, &rescued)
                        || chunk.spans.iter().any(is_single_span_sampled);
                    if sampled {
                        self.set_first_trace_tags(chunk, signature);
                    }
                }
            }
        }
        let removed: HashSet<(usize, usize)> = dropped
            .into_iter()
            .filter(|&(payload_index, chunk_index)| {
                !rescued.contains(&(payload_index, chunk_index))
                    && !keep_sampled_spans(&mut tracer_payloads[payload_index].chunks[chunk_index])
            })
            .collect();
        if removed.is_empty() {
            return;
        }
        let mut payload_index = 0;
        tracer_payloads.retain_mut(|tracer_payload| {
            let mut chunk_index = 0;
            let chunk_count = tracer_payload.chunks.len();
            tracer_payload.chunks.retain(|_| {
                chunk_index += 1;
                !removed.contains(&(payload_index, chunk_index - 1))
            });
            payload_index += 1;
            tracer_payload.chunks.len() == chunk_count || !tracer_payload.chunks.is_empty()
        });
    }

    /// Sets the install signature on the root span of the first sampled chunk of the root's
    /// service, keeping the tracer's values.
    fn set_first_trace_tags(&self, chunk: &mut pb::TraceChunk, signature: &InstallSignature) {
        let Ok(root_index) = trace_utils::get_root_span_index(&chunk.spans) else {
            return;
        };
        let root = &mut chunk.spans[root_index];
        let first = self
            .first_trace_services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.service.clone());
        if !first {
            return;
        }
        for (key, value) in [
            (INSTALL_ID_KEY, signature.id.clone()),
            (INSTALL_TYPE_KEY, signature.install_type.clone()),
            (INSTALL_TIME_KEY, signature.time.to_string()),
        ] {
            root.meta.entry(key.to_string()).or_insert(value);
        }
    }

    /// Counts V07 chunks with priority 0 or 1 in the priority sampler, as the Go agent does, and
    /// sets the sampler's rate as `_sampling_priority_rate_v1` on the root of each priority 1
    /// chunk whose root has no parent and none of the tracer's rate metrics.
    ///
    /// Chunks are counted in the order of [`go_processing_order`], given the chunks that Go drops
    /// whole: those that the samplers don't keep (see [`kept_by_samplers`]) and that have no
    /// sampled spans. Each chunk also weighs `client_dropped_p0s` divided by the chunks left in
    /// its tracer payload.
    async fn apply_priority_sampler(
        &self,
        payload: &mut TracerPayloadCollection,
        config: &Config,
        client_dropped_p0s: usize,
        dropped: &HashSet<(usize, usize)>,
        rescued: &HashSet<(usize, usize)>,
        read_clock: impl Fn() -> i64,
    ) {
        let (Some(state), TracerPayloadCollection::V07(tracer_payloads)) =
            (&self.priority_state, payload)
        else {
            return;
        };
        if !tracer_payloads
            .iter()
            .flat_map(|tracer_payload| &tracer_payload.chunks)
            .any(|chunk| matches!(chunk.priority, 0 | 1))
        {
            return;
        }
        let orders: Vec<Vec<(usize, usize)>> = tracer_payloads
            .iter()
            .enumerate()
            .map(|(payload_index, tracer_payload)| {
                let removed: Vec<bool> = tracer_payload
                    .chunks
                    .iter()
                    .enumerate()
                    .map(|(chunk_index, chunk)| {
                        !kept_by_samplers((payload_index, chunk_index), chunk, dropped, rescued)
                            && !has_sampled_spans(chunk)
                    })
                    .collect();
                go_processing_order(&removed)
            })
            .collect();
        let mut state = state.lock().await;
        let now_unix_secs = state.clamp_timestamp(read_clock());
        for (tracer_payload, order) in tracer_payloads.iter_mut().zip(orders) {
            let env = resolve_payload_env(&tracer_payload.env, &config.env).to_string();
            for (chunk_index, chunks_left) in order {
                let chunk = &mut tracer_payload.chunks[chunk_index];
                if !matches!(chunk.priority, 0 | 1) {
                    continue;
                }
                let Ok(root_index) = trace_utils::get_root_span_index(&chunk.spans) else {
                    continue;
                };
                let root = &mut chunk.spans[root_index];
                let rate = state.sampler.count(
                    now_unix_secs,
                    &root.service,
                    &env,
                    global_sample_rate(root),
                    client_dropped_p0s as f64 / chunks_left as f64,
                );
                if chunk.priority == 1
                    && root.parent_id == 0
                    && !TRACER_PRIORITY_RATE_METRIC_KEYS
                        .iter()
                        .any(|key| root.metrics.contains_key(*key))
                {
                    root.metrics
                        .entry(PRIORITY_RATE_METRIC_KEY.to_string())
                        .or_insert(rate);
                }
            }
        }
    }

    /// Samples V07 chunks without a priority, as the Go agent's no-priority sampler does, and
    /// stamps `_dd.no_p_sr` on kept roots. Returns the tracer payload and chunk indices of the
    /// chunks it drops.
    async fn apply_no_priority_sampler(
        &self,
        payload: &mut TracerPayloadCollection,
        config: &Config,
        read_clock: impl Fn() -> i64,
    ) -> Vec<(usize, usize)> {
        let mut dropped = Vec::new();
        let (Some(state), TracerPayloadCollection::V07(tracer_payloads)) =
            (&self.no_priority_state, payload)
        else {
            return dropped;
        };
        let has_candidates = tracer_payloads
            .iter()
            .flat_map(|tracer_payload| &tracer_payload.chunks)
            .any(|chunk| chunk.priority == PRIORITY_NONE);
        if !has_candidates {
            return dropped;
        }
        let mut state = state.lock().await;
        let now_unix_secs = state.clamp_timestamp(read_clock());
        for (payload_index, tracer_payload) in tracer_payloads.iter_mut().enumerate() {
            let env = resolve_payload_env(&tracer_payload.env, &config.env).to_string();
            for (chunk_index, chunk) in tracer_payload.chunks.iter_mut().enumerate() {
                if chunk.priority != PRIORITY_NONE {
                    continue;
                }
                let Ok(root_index) = trace_utils::get_root_span_index(&chunk.spans) else {
                    continue;
                };
                let decision = {
                    let views: Vec<SpanView> = chunk.spans.iter().map(span_view).collect();
                    let root = &chunk.spans[root_index];
                    state.sampler.sample(
                        now_unix_secs,
                        &TraceView {
                            env: &env,
                            trace_id: root.trace_id,
                            root_index,
                            root_global_sample_rate: global_sample_rate(root),
                            spans: &views,
                        },
                    )
                };
                match decision {
                    SampleDecision::Keep { errors_sr: rate } => {
                        chunk.spans[root_index]
                            .metrics
                            .insert(NO_PRIORITY_RATE_METRIC_KEY.to_string(), rate);
                    }
                    SampleDecision::Drop => dropped.push((payload_index, chunk_index)),
                }
            }
        }
        dropped
    }

    /// Samples V07 chunks selected by [`rescue_root_index`], and the errored chunks among
    /// `extra_candidates` (tracer payload and chunk indices), and stamps `_dd.errors_sr` on kept
    /// roots. Priorities and chunk order are unchanged. Returns the indices of the kept chunks.
    async fn apply_error_rescue_with_clock(
        &self,
        payload: &mut TracerPayloadCollection,
        config: &Config,
        extra_candidates: &[(usize, usize)],
        read_clock: impl Fn() -> i64,
    ) -> Vec<(usize, usize)> {
        let TracerPayloadCollection::V07(tracer_payloads) = payload else {
            return Vec::new();
        };
        if !self.error_rescue_enabled {
            return Vec::new();
        }

        // Build the sampler inputs before locking, so the shared lock covers
        // only sampling. Requests without candidates never take the lock.
        let candidates: Vec<(usize, usize, &str, usize, Vec<SpanView>)> =
            tracer_payloads
                .iter()
                .enumerate()
                .flat_map(|(payload_index, tracer_payload)| {
                    let env = resolve_payload_env(&tracer_payload.env, &config.env);
                    tracer_payload.chunks.iter().enumerate().filter_map(
                        move |(chunk_index, chunk)| {
                            let root_index =
                                if extra_candidates.contains(&(payload_index, chunk_index)) {
                                    errored_root_index(chunk)
                                } else {
                                    rescue_root_index(chunk, config.error_rare_sample_tracer_drop)
                                }?;
                            let views = chunk.spans.iter().map(span_view).collect();
                            Some((payload_index, chunk_index, env, root_index, views))
                        },
                    )
                })
                .collect();
        if candidates.is_empty() {
            return Vec::new();
        }

        let kept: Vec<(usize, usize, usize, f64)> = {
            let mut state = self.error_rescue_state.lock().await;
            // Read once under the lock, and clamp backward wall-clock steps so
            // the sampler's rolling window cannot move backwards.
            let now_unix_secs = state.clamp_timestamp(read_clock());
            candidates
                .iter()
                .filter_map(|(payload_index, chunk_index, env, root_index, views)| {
                    let root =
                        &tracer_payloads[*payload_index].chunks[*chunk_index].spans[*root_index];
                    let trace = TraceView {
                        env,
                        trace_id: root.trace_id,
                        root_index: *root_index,
                        root_global_sample_rate: global_sample_rate(root),
                        spans: views,
                    };
                    match state.sampler.sample(now_unix_secs, &trace) {
                        SampleDecision::Keep { errors_sr } => {
                            Some((*payload_index, *chunk_index, *root_index, errors_sr))
                        }
                        SampleDecision::Drop => None,
                    }
                })
                .collect()
        };

        kept.into_iter()
            .map(|(payload_index, chunk_index, root_index, errors_sr)| {
                tracer_payloads[payload_index].chunks[chunk_index].spans[root_index]
                    .metrics
                    .insert(ERRORS_SR_METRIC_KEY.to_string(), errors_sr);
                (payload_index, chunk_index)
            })
            .collect()
    }

    /// Test-only variant that injects a synthetic clock reading through the
    /// production clamp path, exercising the rolling window without sleeping.
    #[cfg(test)]
    async fn apply_error_rescue_at(
        &self,
        payload: &mut TracerPayloadCollection,
        config: &Config,
        now_unix_secs: i64,
    ) {
        self.apply_error_rescue_with_clock(payload, config, &[], || now_unix_secs)
            .await;
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
                    threadlocal_metadata: None,
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

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The root's raw `_sample_rate`, else 1. The shared sampler sanitizes the raw wire rate.
fn global_sample_rate(root: &pb::Span) -> f64 {
    root.metrics.get("_sample_rate").copied().unwrap_or(1.0)
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

/// Returns the root index of an eligible errored chunk, otherwise `None`.
/// P0 chunks qualify by default. The feature adds other negative priorities
/// except no-priority (-128) and manual drops (priority -1 with chunk DM -4).
/// Only chunk tags determine DM; any span with `error != 0` supplies the error.
/// Root selection delegates to `get_root_span_index`; empty chunks yield `None`.
fn rescue_root_index(chunk: &pb::TraceChunk, tracer_drop_enabled: bool) -> Option<usize> {
    let eligible = match chunk.priority {
        0 => true,
        -128 => false,
        -1 => {
            tracer_drop_enabled
                && chunk.tags.get(DECISION_MAKER_KEY).map(String::as_str)
                    != Some(MANUAL_DECISION_MAKER)
        }
        priority => tracer_drop_enabled && priority < 0,
    };
    if !eligible {
        return None;
    }
    errored_root_index(chunk)
}

/// Keeps what the Go agent keeps of a chunk that its samplers drop, and returns whether anything
/// is kept. Single span sampling keeps only the spans with `_dd.span_sampling.mechanism`, with
/// priority user keep. Otherwise a chunk with an analytics event candidate (a positive
/// `_dd1.sr.eausr`) is kept whole: Go sends only the events that its event processor samples.
fn keep_sampled_spans(chunk: &mut pb::TraceChunk) -> bool {
    if chunk.spans.iter().any(is_single_span_sampled) {
        chunk.spans.retain(is_single_span_sampled);
        chunk.priority = PRIORITY_USER_KEEP;
        chunk.dropped_trace = false;
        return true;
    }
    has_sampled_spans(chunk)
}

/// Whether [`keep_sampled_spans`] keeps anything of a chunk that the samplers drop.
fn has_sampled_spans(chunk: &pb::TraceChunk) -> bool {
    chunk.spans.iter().any(|span| {
        is_single_span_sampled(span)
            || span
                .metrics
                .get(EVENT_EXTRACTION_RATE_KEY)
                .is_some_and(|rate| *rate > 0.0)
    })
}

/// Whether the samplers keep the chunk at `index` (tracer payload and chunk indices): a positive
/// priority, a chunk without a priority that the no-priority sampler doesn't drop, or a chunk
/// that the error sampler keeps.
fn kept_by_samplers(
    index: (usize, usize),
    chunk: &pb::TraceChunk,
    dropped: &HashSet<(usize, usize)>,
    rescued: &HashSet<(usize, usize)>,
) -> bool {
    rescued.contains(&index)
        || match chunk.priority {
            PRIORITY_NONE => !dropped.contains(&index),
            priority => priority > 0,
        }
}

/// The order in which the Go agent processes the chunks of a tracer payload, as each chunk's
/// index with the number of chunks left in the payload at that point. Go removes a chunk that it
/// drops whole (`removed`) by moving the last chunk into its place.
fn go_processing_order(removed: &[bool]) -> Vec<(usize, usize)> {
    let mut slots: Vec<usize> = (0..removed.len()).collect();
    let mut order = Vec::with_capacity(removed.len());
    let mut slot = 0;
    while slot < slots.len() {
        let index = slots[slot];
        order.push((index, slots.len()));
        if removed[index] {
            slots.swap_remove(slot);
        } else {
            slot += 1;
        }
    }
    order
}

fn is_single_span_sampled(span: &pb::Span) -> bool {
    span.metrics.contains_key(SPAN_SAMPLING_MECHANISM_KEY)
}

/// Returns the root index of a chunk with an errored span, otherwise `None`.
fn errored_root_index(chunk: &pb::TraceChunk) -> Option<usize> {
    // Skip root resolution for healthy chunks.
    if !chunk.spans.iter().any(|span| span.error != 0) {
        return None;
    }
    trace_utils::get_root_span_index(&chunk.spans).ok()
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
        let (body_size, mut traces) = match trace_utils::get_traces_from_request_body(body).await {
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

        // Go removes the internal spans before normalization, so they reach neither the traces
        // nor the stats, and it sends nothing when no chunk is left.
        if config.agent_payload_fields.is_some()
            && internal_span_filter::discard_internal_spans(&mut traces)
            && traces.is_empty()
        {
            return log_and_create_traces_success_http_response(
                "Successfully buffered traces to be flushed.",
                StatusCode::OK,
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

        // The samplers run whether or not the agent computes stats. They run after
        // stats submission, so the concentrator observes every chunk before sampling
        // stamps or drops it, and before payload splitting, so the inserted metrics
        // are included in the recomputed outbound size.
        self.apply_samplers(
            &mut payload,
            &config,
            tracer_header_tags.generic.dropped_p0_traces,
        )
        .await;

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
    use datadog_agent_trace_sampler::{ErrorSamplerConfig, ErrorSamplerMode};
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
            dd_apm_non_local_traffic: false,
            #[cfg(any(all(windows, feature = "windows-pipes"), test))]
            dd_apm_windows_pipe_name: None,
            dd_dogstatsd_port: 8125,
            #[cfg(any(all(windows, feature = "windows-pipes"), test))]
            dd_dogstatsd_windows_pipe_name: None,
            env_type: trace_utils::EnvironmentType::CloudFunction,
            os: "linux".to_string(),
            obfuscation_config: ObfuscationConfig::new_from_env().unwrap(),
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
            span_tags: None,
            agent_payload_fields: None,
        }
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
            trace_processor::ServerlessTraceProcessor::new(None, &create_test_config());
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

    /// Each case gives whether `agent_payload_fields` is set and whether the request has the
    /// trace with a kept root, then the span IDs of each chunk that is sent, which are also the
    /// chunks that stats are computed from. The other trace is one internal span.
    #[tokio::test]
    async fn test_process_trace_discards_internal_spans() {
        use crate::config::AgentPayloadFields;
        use crate::stats_concentrator_service::{ConcentratorCommand, StatsConcentratorHandle};
        type Case = (bool, bool, Option<Vec<Vec<u64>>>);
        let cases: [Case; 3] = [
            (false, true, Some(vec![vec![10, 11], vec![20]])),
            (true, true, Some(vec![vec![10]])),
            // Nothing is sent when every span is internal.
            (true, false, None),
        ];
        let start = get_current_timestamp_nanos();
        let with_meta = |mut span: serde_json::Value, key: &str, value: &str| {
            span["meta"][key] = value.into();
            span
        };
        for (go_fields, with_kept_root, expected) in cases {
            let mut traces = vec![vec![with_meta(
                create_test_json_span(2, 20, 0, start, false),
                "dns.address",
                "127.0.0.1",
            )]];
            if with_kept_root {
                traces.insert(
                    0,
                    vec![
                        create_test_json_span(1, 10, 0, start, false),
                        with_meta(
                            create_test_json_span(1, 11, 10, start, false),
                            "http.url",
                            "http://127.0.0.1:8125/",
                        ),
                    ],
                );
            }
            let body = rmp_serde::to_vec(&traces).unwrap();
            let request = Request::builder()
                .header("content-length", body.len())
                .body(http_common::Body::from(body))
                .unwrap();
            let config = Config {
                agent_payload_fields: go_fields.then(|| AgentPayloadFields {
                    agent_version: "1.2.3".to_string(),
                    target_tps: 10.0,
                    install_signature: None,
                }),
                agent_stats_computation_enabled: true,
                ..create_test_config()
            };
            let (stats_tx, mut stats_rx) = mpsc::unbounded_channel();
            let (tx, mut rx) = mpsc::channel(1);
            let processor = trace_processor::ServerlessTraceProcessor::new(
                Some(StatsConcentratorHandle::new(stats_tx)),
                &config,
            );
            let res = processor
                .process_traces(
                    Arc::new(config),
                    request,
                    tx,
                    Arc::new(create_test_metadata()),
                )
                .await;
            assert!(res.is_ok());
            let span_ids =
                |chunk: &pb::TraceChunk| chunk.spans.iter().map(|span| span.span_id).collect();
            let sent: Option<Vec<Vec<u64>>> = rx.recv().await.map(|send_data| {
                let TracerPayloadCollection::V07(payloads) = send_data.get_payloads() else {
                    unreachable!()
                };
                payloads[0].chunks.iter().map(span_ids).collect()
            });
            let mut stats: Vec<Vec<u64>> = Vec::new();
            while let Ok(ConcentratorCommand::AddChunk(chunk, _)) = stats_rx.try_recv() {
                stats.push(span_ids(&chunk));
            }
            let case = format!("{go_fields} {with_kept_root}");
            assert_eq!(sent, expected, "{case}");
            assert_eq!(stats, expected.unwrap_or_default(), "{case}");
        }
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
            trace_processor::ServerlessTraceProcessor::new(None, &create_test_config());
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
            trace_processor::ServerlessTraceProcessor::new(None, &create_test_config());
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
            trace_processor::ServerlessTraceProcessor::new(None, &create_test_config());
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
            trace_processor::ServerlessTraceProcessor::new(None, &create_test_config());
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

    /// Builds the standard rescue test fixture: a config with the given
    /// sampler settings and a processor whose shared sampler uses them.
    fn rescue_setup(
        sampler_config: ErrorSamplerConfig,
    ) -> (Config, trace_processor::ServerlessTraceProcessor) {
        let config = rescue_config(sampler_config);
        let processor = trace_processor::ServerlessTraceProcessor::new(None, &config);
        (config, processor)
    }

    async fn run_rescue(
        processor: &trace_processor::ServerlessTraceProcessor,
        config: &Config,
        chunks: Vec<pb::TraceChunk>,
        now_unix_secs: i64,
    ) -> Vec<pb::TraceChunk> {
        let mut payload = TracerPayloadCollection::V07(vec![pb::TracerPayload {
            chunks,
            ..Default::default()
        }]);
        processor
            .apply_error_rescue_at(&mut payload, config, now_unix_secs)
            .await;
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
    fn test_decision_maker_promotion_precedence() {
        use libdd_trace_utils::tracer_payload::TraceChunkProcessor as _;
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
            let mut spans = vec![test_span(1, 2, 0, 1), test_span(1, 3, 2, 0)];
            if child_first {
                spans.swap(0, 1);
            }
            for (span, dm) in spans.iter_mut().zip([first, second]) {
                if let Some(dm) = dm {
                    span.meta
                        .insert(super::DECISION_MAKER_KEY.into(), dm.into());
                }
            }
            let mut chunk = test_chunk(spans, -1);
            if let Some(dm) = chunk_dm {
                chunk
                    .tags
                    .insert(super::DECISION_MAKER_KEY.into(), dm.into());
            }
            super::ChunkProcessor {
                config: Arc::new(create_test_config()),
                mini_agent_metadata: Arc::new(create_test_metadata()),
            }
            .process(&mut chunk, usize::from(child_first));
            assert_eq!(
                chunk
                    .tags
                    .get(super::DECISION_MAKER_KEY)
                    .map(String::as_str),
                expected
            );
        }
    }

    /// Each case gives the tracer's chunk origin and root `_dd.origin`, the configured tags, then
    /// the expected chunk origin and the expected meta of the root and the child span.
    #[test]
    fn test_span_tags() {
        use crate::config::SpanTags;
        use libdd_trace_utils::tracer_payload::TraceChunkProcessor as _;
        type Meta = &'static [(&'static str, &'static str)];
        type Case = (
            &'static str,
            Option<&'static str>,
            Meta,
            &'static str,
            Meta,
            Meta,
        );
        let cases: [Case; 3] = [
            (
                "",
                None,
                &[("local", "true"), ("service", "parity-app")],
                "local",
                &[
                    ("_dd.origin", "local"),
                    ("local", "true"),
                    ("service", "parity-app"),
                ],
                &[
                    ("_dd.origin", "local"),
                    ("local", "true"),
                    ("service", "parity-app"),
                ],
            ),
            // The tracer's origins are kept.
            (
                "synthetics",
                Some("synthetics"),
                &[],
                "synthetics",
                &[("_dd.origin", "synthetics"), ("service", "from-tracer")],
                &[("_dd.origin", "local")],
            ),
            // A `_dd.origin` tag replaces the chunk origin, not the span tag.
            (
                "synthetics",
                None,
                &[("_dd.origin", "configured")],
                "configured",
                &[("_dd.origin", "local"), ("service", "from-tracer")],
                &[("_dd.origin", "local")],
            ),
        ];
        let to_map = |meta: Meta| {
            meta.iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<HashMap<_, _>>()
        };
        for (chunk_origin, root_origin, tags, expected_origin, root_meta, child_meta) in cases {
            let mut root = test_span(1, 2, 0, 0);
            root.meta.insert("service".into(), "from-tracer".into());
            if let Some(origin) = root_origin {
                root.meta.insert("_dd.origin".into(), origin.into());
            }
            let mut chunk = test_chunk(vec![root, test_span(1, 3, 2, 0)], 1);
            chunk.origin = chunk_origin.into();
            let config = Config {
                span_tags: Some(SpanTags {
                    origin: "local".to_string(),
                    tags: tags
                        .iter()
                        .map(|(key, value)| (key.to_string(), value.to_string()))
                        .collect(),
                }),
                env_type: trace_utils::EnvironmentType::AzureSpringApp,
                ..create_test_config()
            };
            super::ChunkProcessor {
                config: Arc::new(config),
                mini_agent_metadata: Arc::new(MiniAgentMetadata::default()),
            }
            .process(&mut chunk, 0);
            assert_eq!(chunk.origin, expected_origin, "tags {tags:?}");
            assert_eq!(chunk.spans[0].meta, to_map(root_meta), "tags {tags:?}");
            assert_eq!(chunk.spans[1].meta, to_map(child_meta), "tags {tags:?}");
        }
    }

    /// Each case gives the chunk priority, the no-priority and error targets, and the child's
    /// error, then the root's expected `_dd.no_p_sr` and `_dd.errors_sr`, or `None` when the chunk
    /// is dropped. The root comes after its child, and a second tracer payload is always kept.
    #[tokio::test]
    async fn test_no_priority_sampler() {
        use crate::config::AgentPayloadFields;
        type Case = (i32, f64, f64, i32, Option<(Option<f64>, Option<f64>)>);
        let cases: [Case; 9] = [
            (-128, 10.0, 10.0, 0, Some((Some(1.0), None))),
            (-128, 10.0, 10.0, 1, Some((Some(1.0), None))),
            (-128, 0.0, 10.0, 0, None),
            // The error sampler rescues an errored chunk that the no-priority sampler drops.
            (-128, 0.0, 10.0, 1, Some((None, Some(1.0)))),
            (-128, 0.0, 0.0, 1, None),
            (-1, 0.0, 10.0, 0, Some((None, None))),
            (0, 0.0, 10.0, 0, Some((None, None))),
            (1, 0.0, 10.0, 0, Some((None, None))),
            (2, 0.0, 10.0, 0, Some((None, None))),
        ];
        for (priority, target_tps, error_tps, child_error, expected) in cases {
            let config = Config {
                agent_payload_fields: Some(AgentPayloadFields {
                    agent_version: "1.2.3".to_string(),
                    target_tps,
                    install_signature: None,
                }),
                error_sampler: rate_limited_sampler_config(error_tps),
                ..create_test_config()
            };
            let processor = trace_processor::ServerlessTraceProcessor::new(None, &config);
            let chunk = test_chunk(
                vec![test_span(1, 3, 2, child_error), test_span(1, 2, 0, 0)],
                priority,
            );
            let kept = test_chunk(vec![test_span(9, 10, 0, 0)], 2);
            let mut payload = TracerPayloadCollection::V07(vec![
                make_payload(vec![chunk]),
                make_payload(vec![kept.clone()]),
            ]);
            processor
                .apply_samplers_with_clock(&mut payload, &config, 0, || RESCUE_NOW)
                .await;
            let TracerPayloadCollection::V07(payloads) = payload else {
                unreachable!()
            };
            let case = format!("priority {priority}, targets {target_tps} {error_tps}");
            assert_eq!(payloads.last().unwrap().chunks, vec![kept], "{case}");
            let metrics = (payloads.len() == 2).then(|| {
                let spans = &payloads[0].chunks[0].spans;
                assert_eq!(spans[0].metrics.get("_dd.no_p_sr"), None, "{case}");
                (
                    spans[1].metrics.get("_dd.no_p_sr").copied(),
                    spans[1].metrics.get("_dd.errors_sr").copied(),
                )
            });
            assert_eq!(metrics, expected, "{case}");
        }
    }

    /// Each case gives whether `agent_payload_fields` is set, the chunk priority, the root's parent
    /// and a rate metric on the root, then the root's expected `_sampling_priority_rate_v1`.
    #[tokio::test]
    async fn test_priority_rate() {
        use crate::config::AgentPayloadFields;
        type Case = (bool, i32, u64, Option<(&'static str, f64)>, Option<f64>);
        let cases: [Case; 9] = [
            (true, 1, 0, None, Some(1.0)),
            (false, 1, 0, None, None),
            (true, 0, 0, None, None),
            (true, 2, 0, None, None),
            (true, -1, 0, None, None),
            // A root whose parent is not in the chunk.
            (true, 1, 5, None, None),
            (true, 1, 0, Some(("_dd.agent_psr", 0.3)), None),
            (true, 1, 0, Some(("_dd.rule_psr", 0.3)), None),
            (
                true,
                1,
                0,
                Some(("_sampling_priority_rate_v1", 0.3)),
                Some(0.3),
            ),
        ];
        for (go_fields, priority, parent_id, metric, expected) in cases {
            let config = Config {
                agent_payload_fields: go_fields.then(|| AgentPayloadFields {
                    agent_version: "1.2.3".to_string(),
                    target_tps: 10.0,
                    install_signature: None,
                }),
                ..create_test_config()
            };
            let processor = trace_processor::ServerlessTraceProcessor::new(None, &config);
            let mut root = test_span(1, 2, parent_id, 0);
            root.metrics
                .extend(metric.map(|(key, value)| (key.to_string(), value)));
            let mut payload = TracerPayloadCollection::V07(vec![make_payload(vec![test_chunk(
                vec![root],
                priority,
            )])]);
            processor
                .apply_samplers_with_clock(&mut payload, &config, 0, || RESCUE_NOW)
                .await;
            let TracerPayloadCollection::V07(payloads) = payload else {
                unreachable!()
            };
            assert_eq!(
                payloads[0].chunks[0].spans[0]
                    .metrics
                    .get("_sampling_priority_rate_v1")
                    .copied(),
                expected,
                "fields {go_fields}, priority {priority}, parent {parent_id}, metric {metric:?}"
            );
        }
    }

    /// Each case gives the target, then the requests, as the second, the payload env, the
    /// tracer's dropped P0 chunks and the chunks as groups of a count, a priority and a
    /// `_sample_rate`, then the expected `_sampling_priority_rate_v1` of a chunk of the last
    /// request. Chunks without errors at priority 0 or below are dropped by Go, which counts them
    /// in [`trace_processor::go_processing_order`].
    #[tokio::test]
    async fn test_priority_rate_counts() {
        use crate::config::AgentPayloadFields;
        type Request = (i64, &'static str, usize, &'static [(usize, i32, f64)]);
        let cases: [(f64, &[Request], usize, f64); 8] = [
            // Kept chunks before the checked one: 101 chunks in one 5-second bucket are 20.2 per
            // second.
            (
                10.0,
                &[(0, "", 0, &[(100, 1, 1.0), (1, 1, 1.0)])],
                100,
                0.49504950495049505,
            ),
            (10.0, &[(0, "", 0, &[(100, 2, 1.0), (1, 1, 1.0)])], 100, 1.0),
            // The dropped chunks after the first put the last chunk second in Go's order.
            (10.0, &[(0, "", 0, &[(100, 0, 1.0), (1, 1, 1.0)])], 100, 1.0),
            // Dropped chunks at priority 0 are counted, and at priority -1 they are not.
            (
                10.0,
                &[(0, "", 0, &[(100, 0, 1.0)]), (0, "", 0, &[(1, 1, 1.0)])],
                0,
                0.49504950495049505,
            ),
            (
                10.0,
                &[(0, "", 0, &[(100, -1, 1.0)]), (0, "", 0, &[(1, 1, 1.0)])],
                0,
                1.0,
            ),
            // A `_sample_rate` of 0.5 counts each chunk twice.
            (
                10.0,
                &[(0, "", 0, &[(50, 1, 0.5), (1, 1, 1.0)])],
                50,
                0.49504950495049505,
            ),
            // 3 dropped P0 chunks: the dropped chunk weighs 1 + 3 / 3, then the two others,
            // counted last-first, weigh 1 + 3 / 2. 7 in one bucket are 1.4 per second.
            (0.7, &[(0, "", 3, &[(1, 0, 1.0), (2, 1, 1.0)])], 1, 0.5),
            // A signature per payload env: once the next bucket starts, the signature of env-b
            // gets its own rate instead of sharing the 100 chunks of env-a.
            (
                10.0,
                &[
                    (0, "env-a", 0, &[(100, 1, 1.0)]),
                    (0, "env-b", 0, &[(1, 1, 1.0)]),
                    (5, "env-b", 0, &[(1, 1, 1.0)]),
                ],
                0,
                1.0,
            ),
        ];
        for (target_tps, requests, checked, expected) in cases {
            let config = Config {
                agent_payload_fields: Some(AgentPayloadFields {
                    agent_version: "1.2.3".to_string(),
                    target_tps,
                    install_signature: None,
                }),
                ..create_test_config()
            };
            let processor = trace_processor::ServerlessTraceProcessor::new(None, &config);
            let mut trace_id = 0;
            let mut chunks = Vec::new();
            for &(second, env, client_dropped_p0s, groups) in requests {
                chunks.clear();
                for &(count, priority, sample_rate) in groups {
                    for _ in 0..count {
                        trace_id += 1;
                        let mut root = test_span(trace_id, trace_id, 0, 0);
                        root.metrics.insert("_sample_rate".to_string(), sample_rate);
                        chunks.push(test_chunk(vec![root], priority));
                    }
                }
                let mut payload = TracerPayloadCollection::V07(vec![pb::TracerPayload {
                    env: env.to_string(),
                    chunks: chunks.clone(),
                    ..Default::default()
                }]);
                processor
                    .apply_samplers_with_clock(&mut payload, &config, client_dropped_p0s, || {
                        RESCUE_NOW + second
                    })
                    .await;
                let TracerPayloadCollection::V07(mut payloads) = payload else {
                    unreachable!()
                };
                chunks = payloads.remove(0).chunks;
            }
            assert_eq!(
                chunks[checked].spans[0]
                    .metrics
                    .get("_sampling_priority_rate_v1")
                    .copied(),
                Some(expected),
                "target {target_tps}, requests {requests:?}"
            );
        }
    }

    /// Each case gives which chunks Go drops whole, then each chunk's index with the number of
    /// chunks left, in Go's order.
    #[test]
    fn test_go_processing_order() {
        type Case = (&'static [bool], &'static [(usize, usize)]);
        let cases: [Case; 4] = [
            (&[], &[]),
            (&[false, false], &[(0, 2), (1, 2)]),
            (&[true, false, false], &[(0, 3), (2, 2), (1, 2)]),
            // The `traces.sampling` fixture: priority 0, 1, 2, errored 0 and -1.
            (
                &[true, false, false, false, true],
                &[(0, 5), (4, 4), (3, 3), (1, 3), (2, 3)],
            ),
        ];
        for (removed, expected) in cases {
            assert_eq!(
                trace_processor::go_processing_order(removed),
                expected,
                "{removed:?}"
            );
        }
    }

    /// Each case gives the metrics of a root and its child, then the span IDs and priority of what
    /// is kept from a dropped chunk, or `None` when nothing is kept.
    #[test]
    fn test_kept_from_dropped_chunk() {
        type Metrics = &'static [(&'static str, f64)];
        type Case = (Metrics, Metrics, Option<(Vec<u64>, i32)>);
        let cases: [Case; 5] = [
            (&[], &[], None),
            (
                &[],
                &[("_dd.span_sampling.mechanism", 8.0)],
                Some((vec![3], 2)),
            ),
            // Single span sampling wins over analytics events.
            (
                &[("_dd1.sr.eausr", 1.0)],
                &[("_dd.span_sampling.mechanism", 8.0)],
                Some((vec![3], 2)),
            ),
            (&[("_dd1.sr.eausr", 0.5)], &[], Some((vec![2, 3], -128))),
            (&[("_dd1.sr.eausr", 0.0)], &[], None),
        ];
        for (root_metrics, child_metrics, expected) in cases {
            let mut root = test_span(1, 2, 0, 0);
            root.metrics.extend(
                root_metrics
                    .iter()
                    .map(|(key, value)| (key.to_string(), *value)),
            );
            let mut child = test_span(1, 3, 2, 0);
            child.metrics.extend(
                child_metrics
                    .iter()
                    .map(|(key, value)| (key.to_string(), *value)),
            );
            let mut chunk = test_chunk(vec![root, child], -128);
            let kept = super::keep_sampled_spans(&mut chunk).then(|| {
                let span_ids = chunk.spans.iter().map(|span| span.span_id).collect();
                (span_ids, chunk.priority)
            });
            assert_eq!(kept, expected, "metrics {root_metrics:?} {child_metrics:?}");
        }
    }

    /// A chunk that the no-priority sampler drops keeps its single-span-sampled spans.
    #[tokio::test]
    async fn test_no_priority_drop_keeps_single_span_sampled_spans() {
        use crate::config::AgentPayloadFields;
        let config = Config {
            agent_payload_fields: Some(AgentPayloadFields {
                agent_version: "1.2.3".to_string(),
                target_tps: 0.0,
                install_signature: None,
            }),
            ..create_test_config()
        };
        let processor = trace_processor::ServerlessTraceProcessor::new(None, &config);
        let mut child = test_span(1, 3, 2, 0);
        child
            .metrics
            .insert("_dd.span_sampling.mechanism".into(), 8.0);
        let chunk = test_chunk(vec![test_span(1, 2, 0, 0), child], -128);
        let mut payload = TracerPayloadCollection::V07(vec![make_payload(vec![chunk])]);
        processor
            .apply_samplers_with_clock(&mut payload, &config, 0, || RESCUE_NOW)
            .await;
        let TracerPayloadCollection::V07(payloads) = payload else {
            unreachable!()
        };
        let chunks = &payloads[0].chunks;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].priority, 2);
        assert_eq!(
            chunks[0]
                .spans
                .iter()
                .map(|span| span.span_id)
                .collect::<Vec<_>>(),
            vec![3]
        );
    }

    /// Each case gives the chunks of one request, as the root's service, the chunk priority, the
    /// root's error, the root's `_dd.install.id` from the tracer and whether single span sampling
    /// keeps the root, then the install tags that each forwarded root gets. The error sampler keeps
    /// every errored chunk.
    #[tokio::test]
    async fn test_first_trace_tags() {
        use crate::config::{AgentPayloadFields, InstallSignature};
        type Install = Option<(&'static str, &'static str, &'static str)>;
        type Case = (
            &'static [(&'static str, i32, i32, Option<&'static str>, bool)],
            Vec<Install>,
        );
        let signature = Some(("install-1", "manual", "1700000000"));
        let cases: [Case; 6] = [
            (&[("a", 1, 0, None, false)], vec![signature]),
            (
                &[
                    ("a", -128, 0, None, false),
                    ("a", 2, 0, None, false),
                    ("b", 1, 0, None, false),
                ],
                vec![signature, None, signature],
            ),
            // A chunk that the samplers do not keep does not count as the service's first.
            (
                &[("a", 0, 0, None, false), ("a", 1, 0, None, false)],
                vec![None, signature],
            ),
            (&[("a", 0, 1, None, false)], vec![signature]),
            // A chunk that single span sampling keeps counts, and its root gets the tags.
            (
                &[("a", 0, 0, None, true), ("a", 1, 0, None, false)],
                vec![signature, None],
            ),
            // A tracer's value is kept.
            (
                &[("a", 1, 0, Some("from-tracer"), false)],
                vec![Some(("from-tracer", "manual", "1700000000"))],
            ),
        ];
        for (chunks, expected) in cases {
            let config = Config {
                agent_payload_fields: Some(AgentPayloadFields {
                    agent_version: "1.2.3".to_string(),
                    target_tps: 10.0,
                    install_signature: Some(InstallSignature {
                        id: "install-1".to_string(),
                        install_type: "manual".to_string(),
                        time: 1_700_000_000,
                    }),
                }),
                error_sampler: always_keep_sampler_config(),
                ..create_test_config()
            };
            let processor = trace_processor::ServerlessTraceProcessor::new(None, &config);
            let traces: Vec<pb::TraceChunk> = chunks
                .iter()
                .enumerate()
                .map(
                    |(index, (service, priority, error, tracer_id, single_span))| {
                        let mut root = test_span(index as u64 + 1, 1, 0, *error);
                        root.service = service.to_string();
                        if let Some(id) = tracer_id {
                            root.meta.insert("_dd.install.id".into(), id.to_string());
                        }
                        if *single_span {
                            root.metrics
                                .insert("_dd.span_sampling.mechanism".into(), 8.0);
                        }
                        test_chunk(vec![root], *priority)
                    },
                )
                .collect();
            let mut payload = TracerPayloadCollection::V07(vec![make_payload(traces)]);
            processor
                .apply_samplers_with_clock(&mut payload, &config, 0, || RESCUE_NOW)
                .await;
            let TracerPayloadCollection::V07(payloads) = payload else {
                unreachable!()
            };
            let install: Vec<Option<(&str, &str, &str)>> = payloads[0]
                .chunks
                .iter()
                .map(|chunk| {
                    let meta = &chunk.spans[0].meta;
                    let get = |key: &str| meta.get(key).map(String::as_str);
                    Some((
                        get("_dd.install.id")?,
                        get("_dd.install.type")?,
                        get("_dd.install.time")?,
                    ))
                })
                .collect();
            assert_eq!(install, expected, "chunks {chunks:?}");
        }
    }

    /// Each case gives whether the agent payload fields are set, a span's type and the tracer's
    /// `sql.query`, then the span's expected `sql.query` and `_dd.convertedv1`.
    #[test]
    fn test_go_span_meta() {
        use crate::config::{AgentPayloadFields, SpanTags};
        use libdd_trace_utils::tracer_payload::TraceChunkProcessor as _;
        type Case = (
            bool,
            &'static str,
            Option<&'static str>,
            Option<&'static str>,
            Option<&'static str>,
            Option<&'static str>,
        );
        let cases: [Case; 8] = [
            (true, "sql", None, None, None, Some("v04")),
            (true, "sql", Some(""), None, None, Some("v04")),
            (
                true,
                "sql",
                Some("SELECT 1"),
                None,
                Some("SELECT ?"),
                Some("v04"),
            ),
            (
                true,
                "sql",
                Some("SELECT 2 FROM users"),
                None,
                Some("SELECT ? FROM users"),
                Some("v04"),
            ),
            (
                true,
                "web",
                Some("SELECT 1"),
                None,
                Some("SELECT 1"),
                Some("v04"),
            ),
            // A configured tag replaces the value from the payload version.
            (
                true,
                "web",
                None,
                Some("configured"),
                None,
                Some("configured"),
            ),
            (false, "sql", None, None, Some("SELECT ?"), None),
            (
                false,
                "sql",
                Some("SELECT 2 FROM users"),
                None,
                Some("SELECT ?"),
                None,
            ),
        ];
        for (go_fields, span_type, tracer_query, configured, expected_query, expected_converted) in
            cases
        {
            let config = Config {
                agent_payload_fields: go_fields.then(|| AgentPayloadFields {
                    agent_version: "1.2.3".to_string(),
                    target_tps: 10.0,
                    install_signature: None,
                }),
                span_tags: configured.map(|value| SpanTags {
                    origin: String::new(),
                    tags: [("_dd.convertedv1".to_string(), value.to_string())].into(),
                }),
                env_type: trace_utils::EnvironmentType::AzureSpringApp,
                ..create_test_config()
            };
            let mut span = test_span(1, 2, 0, 0);
            span.r#type = span_type.to_string();
            span.resource = "SELECT 1".to_string();
            if let Some(query) = tracer_query {
                span.meta.insert("sql.query".into(), query.to_string());
            }
            let mut chunk = test_chunk(vec![span], 1);
            super::ChunkProcessor {
                config: Arc::new(config),
                mini_agent_metadata: Arc::new(MiniAgentMetadata::default()),
            }
            .process(&mut chunk, 0);
            let meta = &chunk.spans[0].meta;
            let case = format!("{go_fields} {span_type} {tracer_query:?} {configured:?}");
            assert_eq!(
                meta.get("sql.query").map(String::as_str),
                expected_query,
                "{case}"
            );
            assert_eq!(
                meta.get("_dd.convertedv1").map(String::as_str),
                expected_converted,
                "{case}"
            );
        }
    }

    #[test]
    fn test_tracer_drop_rescue_eligibility_matrix() {
        // Expected eligibility with the feature off and on, respectively.
        for (priority, dm, expected) in [
            (0, None, [true, true]),
            (0, Some("-4"), [true, true]),
            (-128, None, [false, false]),
            (-128, Some("-4"), [false, false]),
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

    #[tokio::test]
    async fn test_rescue_preserves_chunks_except_root_metric() {
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
        let mut chunks = run_rescue(&processor, &config, original.clone(), RESCUE_NOW).await;
        assert_eq!(chunks.len(), 2, "every chunk must still be forwarded");
        assert_eq!(errors_sr(&chunks[0].spans[0]), Some(1.0));
        assert_eq!(errors_sr(&chunks[0].spans[1]), None);
        chunks[0].spans[0]
            .metrics
            .remove(super::ERRORS_SR_METRIC_KEY);
        assert_eq!(chunks, original);
    }

    #[tokio::test]
    async fn test_rescue_uses_resolved_root_not_first_span() {
        let (config, processor) = rescue_setup(always_keep_sampler_config());

        // Root (parent_id 0) appears last; the first span is a child.
        let spans = vec![
            test_span(0xbeef, 0x201, 0x203, 0),
            test_span(0xbeef, 0x202, 0x203, 1),
            test_span(0xbeef, 0x203, 0, 0),
        ];
        let chunks = run_rescue(&processor, &config, vec![test_chunk(spans, 0)], RESCUE_NOW).await;

        assert_eq!(chunks.len(), 1);
        assert_eq!(
            errors_sr(&chunks[0].spans[2]),
            Some(1.0),
            "the resolved root (last span) must be stamped"
        );
        assert_eq!(errors_sr(&chunks[0].spans[0]), None);
        assert_eq!(errors_sr(&chunks[0].spans[1]), None);
    }

    #[tokio::test]
    async fn test_no_error_flags_leaves_chunk_unchanged_and_spends_no_budget() {
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

        let chunks = run_rescue(&processor, &config, chunks, RESCUE_NOW).await;

        for (i, chunk) in chunks.iter().enumerate() {
            let is_last = i == chunks.len() - 1;
            assert_eq!(
                errors_sr(root(chunk)),
                if is_last { Some(1.0) } else { None },
                "chunk {i}: only the errored chunk may be rescued"
            );
        }
    }

    #[tokio::test]
    async fn test_empty_chunk_and_fallback_root() {
        let (config, processor) = rescue_setup(always_keep_sampler_config());

        // Eligible empty chunks return at the no-error guard before root resolution.
        // The cyclic chunk has no parent_id 0, so the last span is the fallback root.
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
        )
        .await;

        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].spans.is_empty(), "empty chunk unchanged");
        assert_eq!(chunks[0].priority, 0);
        assert_eq!(chunks[1].priority, 0);
        assert_eq!(
            errors_sr(&chunks[1].spans[0]),
            None,
            "non-root span not stamped"
        );
        assert_eq!(
            errors_sr(&chunks[1].spans[1]),
            Some(1.0),
            "fallback root (last span) stamped"
        );
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
            &config,
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

    #[tokio::test]
    async fn test_disabled_tps_skips_rescue() {
        for tps in [0.0_f64, -3.0] {
            let (mut config, processor) = rescue_setup(rate_limited_sampler_config(tps));
            config.error_rare_sample_tracer_drop = true;

            let original = pb::TracerPayload {
                chunks: vec![
                    errored_root_chunk(0xe000, 0),
                    errored_root_chunk(0xe001, -1),
                ],
                ..Default::default()
            };
            let mut payload = TracerPayloadCollection::V07(vec![original.clone()]);
            processor
                .apply_error_rescue_with_clock(&mut payload, &config, &[], || {
                    panic!("disabled sampler must not read the clock");
                })
                .await;

            assert_eq!(
                processor
                    .error_rescue_state
                    .lock()
                    .await
                    .last_sampler_timestamp,
                i64::MIN
            );
            let TracerPayloadCollection::V07(payloads) = payload else {
                panic!("disabled rescue must preserve the payload format");
            };
            assert_eq!(payloads, vec![original], "tps={tps}");
        }
    }

    #[tokio::test]
    async fn test_client_sample_rate_affects_error_rescue() {
        let mut rescued_by_rate = Vec::new();
        for sample_rate in [None, Some(0.25_f64)] {
            let (config, processor) = rescue_setup(rate_limited_sampler_config(10.0));

            // 100 errored chunks with the same signature in one bucket. The
            // client `_sample_rate` weights each chunk, so the two cases keep
            // different counts.
            let input: Vec<pb::TraceChunk> = (0..100_u64)
                .map(|i| {
                    let mut chunk = errored_root_chunk(0x11_0000 + i, 0);
                    if let Some(rate) = sample_rate {
                        chunk.spans[0].metrics.insert("_sample_rate".into(), rate);
                    }
                    chunk
                })
                .collect();
            let chunks = run_rescue(&processor, &config, input, RESCUE_NOW).await;

            assert_eq!(chunks.len(), 100, "every chunk is still forwarded");
            assert!(
                chunks.iter().all(|chunk| chunk.priority == 0),
                "priority changed"
            );
            rescued_by_rate.push(rescued_count(&chunks));
        }

        assert!(
            rescued_by_rate[1] < rescued_by_rate[0],
            "the client's root sample rate must affect error-rescue decisions"
        );
    }

    #[tokio::test]
    async fn test_processor_clones_share_one_budget() {
        let (mut config, processor) = rescue_setup(rate_limited_sampler_config(1.0));
        config.error_rare_sample_tracer_drop = true;
        let processor_clone = processor.clone();

        // Ten errored chunks with the same signature but distinct IDs: five
        // through the original, five through the clone. Mix P0 and explicit
        // drops. Ineligible chunks interleaved with them spend no budget.
        let mut observed = Vec::new();
        for i in 0..10_u64 {
            let id = 0x33_0000 + i;
            let target = if i < 5 { &processor } else { &processor_clone };
            let mut manual = errored_root_chunk(id, -1);
            manual
                .tags
                .insert(super::DECISION_MAKER_KEY.into(), "-4".into());
            let mut healthy = errored_root_chunk(id, -1);
            healthy.spans[0].error = 0;
            let excluded = run_rescue(
                target,
                &config,
                vec![manual, healthy, errored_root_chunk(id, 1)],
                RESCUE_NOW,
            )
            .await;
            assert_eq!(rescued_count(&excluded), 0);
            let priority = if i % 2 == 0 { 0 } else { -1 };
            let chunks = run_rescue(
                target,
                &config,
                vec![errored_root_chunk(id, priority)],
                RESCUE_NOW,
            )
            .await;
            observed.push(errors_sr(root(&chunks[0])).is_some());
        }
        assert_eq!(
            observed,
            [
                true, true, true, true, true, true, false, false, false, false
            ]
        );
    }

    #[tokio::test]
    async fn test_error_rescue_yields_while_sampler_is_locked() {
        use std::{
            future::{Future, poll_fn},
            task::Poll,
        };

        let (config, processor) = rescue_setup(always_keep_sampler_config());
        let guard = processor.error_rescue_state.lock().await;
        let mut rescue = std::pin::pin!(run_rescue(
            &processor,
            &config,
            vec![errored_root_chunk(0x10000, 0)],
            RESCUE_NOW,
        ));

        poll_fn(|cx| {
            assert!(rescue.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(guard.last_sampler_timestamp, i64::MIN);
        drop(guard);

        let chunks = rescue.await;
        assert_eq!(errors_sr(root(&chunks[0])), Some(1.0));
        assert_eq!(
            processor
                .error_rescue_state
                .lock()
                .await
                .last_sampler_timestamp,
            RESCUE_NOW
        );
    }

    #[tokio::test]
    async fn test_error_rescue_clamps_backward_timestamps_across_clones() {
        let (config, processor) = rescue_setup(always_keep_sampler_config());
        let clone = processor.clone();
        assert_eq!(
            processor
                .error_rescue_state
                .lock()
                .await
                .last_sampler_timestamp,
            i64::MIN
        );

        for (i, (now, expected)) in [
            (RESCUE_NOW, RESCUE_NOW),
            (RESCUE_NOW - 40, RESCUE_NOW),
            (RESCUE_NOW, RESCUE_NOW),
            (RESCUE_NOW + 40, RESCUE_NOW + 40),
            (RESCUE_NOW + 20, RESCUE_NOW + 40),
        ]
        .into_iter()
        .enumerate()
        {
            let target = if i % 2 == 0 { &processor } else { &clone };
            let chunks = run_rescue(
                target,
                &config,
                vec![errored_root_chunk(0x10000 + i as u64, 0)],
                now,
            )
            .await;
            assert_eq!(errors_sr(root(&chunks[0])), Some(1.0));
            assert_eq!(
                target
                    .error_rescue_state
                    .lock()
                    .await
                    .last_sampler_timestamp,
                expected
            );
        }
    }

    #[tokio::test]
    async fn test_error_rescue_without_candidates_skips_sampler() {
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

        processor
            .apply_error_rescue_with_clock(&mut payload, &config, &[], || {
                panic!("a payload without rescue candidates must not consult the sampler");
            })
            .await;

        assert_eq!(
            processor
                .error_rescue_state
                .lock()
                .await
                .last_sampler_timestamp,
            i64::MIN
        );
        let TracerPayloadCollection::V07(payloads) = payload else {
            panic!("rescue must preserve the payload format");
        };
        assert_eq!(payloads, vec![original]);
    }

    #[tokio::test]
    async fn test_stats_concentrator_observes_pre_rescue_chunks() {
        let mut config = rescue_config(always_keep_sampler_config());
        config.error_rare_sample_tracer_drop = true;
        let config = Arc::new(config);
        let (stats_tx, mut stats_rx) = tokio::sync::mpsc::unbounded_channel();
        let processor = trace_processor::ServerlessTraceProcessor::new(
            Some(super::StatsConcentratorHandle::new(stats_tx)),
            &config,
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
