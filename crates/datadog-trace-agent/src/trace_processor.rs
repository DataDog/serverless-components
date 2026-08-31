// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use async_trait::async_trait;
use hyper::{StatusCode, http};
use libdd_common::http_common;
use libdd_library_config::tracer_metadata::TracerMetadata;
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
#[derive(Clone)]
pub struct ServerlessTraceProcessor {
    pub stats_concentrator: Option<StatsConcentratorHandle>,
}

impl ServerlessTraceProcessor {
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

        for (piece, size) in pieces {
            let send_data = SendData::new(
                size,
                piece,
                tracer_header_tags.clone(),
                &config.trace_intake,
            );

            if size > MAX_CONTENT_SIZE_BYTES {
                // For V07, `size` includes V07_ENVELOPE_OVERHEAD_BYTES; for other
                // formats it's the raw body_size - both approximate checks
                warn!(
                    payload_size = size,
                    max_content_size_bytes = MAX_CONTENT_SIZE_BYTES,
                    "Trace payload is over max batch size; sending standalone"
                );
            }

            if let Err(err) = tx.send(send_data).await {
                return log_and_create_http_response(
                    &format!("Error sending traces to the trace flusher: {err}"),
                    StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
        }

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
            self, TRACER_PAYLOAD_FUNCTION_TAGS_TAG_KEY, TraceProcessor, encoded_size,
            split_oversized_payloads,
        },
    };
    use libdd_common::{Endpoint, http_common};
    use libdd_trace_protobuf::pb;
    use libdd_trace_utils::test_utils::{create_test_gcp_json_span, create_test_gcp_span};
    use libdd_trace_utils::trace_utils::MiniAgentMetadata;
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

        let trace_processor = trace_processor::ServerlessTraceProcessor {
            stats_concentrator: None,
        };
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

        let trace_processor = trace_processor::ServerlessTraceProcessor {
            stats_concentrator: None,
        };
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

        let trace_processor = trace_processor::ServerlessTraceProcessor {
            stats_concentrator: None,
        };
        let res = trace_processor
            .process_traces(
                Arc::new(create_test_config()),
                request,
                tx,
                Arc::new(create_test_metadata()),
            )
            .await;
        assert!(res.is_ok());

        let mut received = Vec::new();
        while let Ok(send_data) = rx.try_recv() {
            received.push(send_data);
        }

        assert_eq!(
            received.len(),
            1,
            "expected the oversized single-chunk trace to be sent standalone as one piece, got {}",
            received.len()
        );
        assert!(
            received[0].len() > MAX_CONTENT_SIZE_BYTES,
            "expected the standalone piece to still be reported as oversized (size {})",
            received[0].len()
        );
    }
}
