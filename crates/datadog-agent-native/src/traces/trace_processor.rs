// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Trace processing and enrichment for APM traces.
//!
//! This module implements the core trace processor responsible for:
//! - **Trace Enrichment**: Adding agent version, hostname, environment, and container tags
//! - **Span Filtering**: Filtering spans by tag requirements and rejections (exact and regex)
//! - **Obfuscation**: Removing sensitive data from spans (SQL queries, URLs, etc.)
//! - **Normalization**: Standardizing service names and resource names
//! - **Stats Generation**: Computing trace statistics for APM metrics
//! - **Application Security**: Integrating with AppSec processor for security events
//! - **Trace Forwarding**: Sending processed traces to Datadog intake endpoints
//!
//! # Processing Pipeline
//!
//! ```text
//! Raw Traces → Filter by Tags → Obfuscate Sensitive Data → Enrich Metadata →
//! Generate Stats → Send to Datadog
//! ```
//!
//! # Span Filtering
//!
//! The processor supports four types of tag-based filtering:
//! 1. **Require Tags** (`DD_APM_FILTER_TAGS_REQUIRE`): Span must match all tags exactly
//! 2. **Require Regex** (`DD_APM_FILTER_TAGS_REGEX_REQUIRE`): Span must match all regex patterns
//! 3. **Reject Tags** (`DD_APM_FILTER_TAGS_REJECT`): Span is dropped if any tag matches exactly
//! 4. **Reject Regex** (`DD_APM_FILTER_TAGS_REGEX_REJECT`): Span is dropped if any regex matches
//!
//! # Obfuscation
//!
//! Sensitive data obfuscation includes:
//! - SQL query parameters
//! - URLs and query strings
//! - Custom tag patterns
//!
//! Configuration controlled via:
//! - `DD_APM_OBFUSCATION_SQL_ENABLED`
//! - `DD_APM_OBFUSCATION_HTTP_ENABLED`
//! - Custom obfuscation rules

#[cfg(feature = "appsec")]
use crate::appsec::processor::context::HoldArguments;
#[cfg(feature = "appsec")]
use crate::appsec::processor::Processor as AppSecProcessor;
use crate::config;
use crate::proc::hostname;
use crate::tags::provider;
use crate::traces::span_pointers::{attach_span_pointers_to_meta, SpanPointer};
use crate::traces::S_TO_MS;
use async_trait::async_trait;
use datadog_trace_obfuscation::obfuscate::obfuscate_span;
use datadog_trace_obfuscation::obfuscation_config;
use datadog_trace_protobuf::pb;
use datadog_trace_protobuf::pb::Span;
use datadog_trace_utils::config_utils::trace_intake_url;
use datadog_trace_utils::send_data::{Compression, SendDataBuilder};
use datadog_trace_utils::send_with_retry::{RetryBackoffType, RetryStrategy};
use datadog_trace_utils::trace_utils::{self};
use datadog_trace_utils::tracer_header_tags;
use datadog_trace_utils::tracer_payload::{TraceChunkProcessor, TracerPayloadCollection};
use ddcommon::Endpoint;
use regex::Regex;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::Sender;
#[cfg(feature = "appsec")]
use tokio::sync::Mutex;
use tracing::{debug, error, trace, warn};

use super::stats_generator::StatsGenerator;
use super::trace_aggregator::SendDataBuilderInfo;

/// Agent version tag added to all traces.
///
/// This constant is injected as `_dd.agent_version` in trace metadata to identify
/// which version of the agent processed the trace. The value is extracted from
/// the package version at compile time.
///
/// Example: `_dd.agent_version: 0.1.0`
const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Creates a Datadog trace intake endpoint from configuration.
///
/// This function handles endpoint creation with fallback logic:
/// 1. If `DD_APM_DD_URL` is set, use it directly
/// 2. If not set but `DD_SITE` is configured, generate URL from site
/// 3. If neither is set, use default site `datadoghq.com`
///
/// # URL Generation
///
/// For a given site (e.g., `datadoghq.com`), the trace intake URL is:
/// `https://trace.agent.datadoghq.com`
///
/// # Arguments
///
/// * `config` - Agent configuration containing APM URL and site settings
///
/// # Returns
///
/// An `Endpoint` configured for trace intake with:
/// - URL (generated or configured)
/// - Timeout from `DD_FLUSH_TIMEOUT`
/// - No API key (uses agent authentication)
///
/// # Example
///
/// ```rust,ignore
/// let config = Config {
///     site: "datadoghq.com".to_string(),
///     flush_timeout: 5, // 5 seconds
///     ..Default::default()
/// };
///
/// let endpoint = create_endpoint_from_config(&config);
/// // endpoint.url = "https://trace.agent.datadoghq.com"
/// // endpoint.timeout_ms = 5000
/// ```
fn create_endpoint_from_config(config: &config::Config) -> Endpoint {
    let apm_url = if config.apm_dd_url.is_empty() {
        // Auto-generate URL from site, matching ConfigBuilder behavior
        let site = if config.site.is_empty() {
            warn!(
                "TRACE_PROCESSOR | Both apm_dd_url and site are not configured. \
                Using default site 'datadoghq.com' to generate trace intake URL. \
                Please set DD_SITE or DD_APM_DD_URL environment variable."
            );
            "datadoghq.com"
        } else {
            &config.site
        };

        let generated_url = trace_intake_url(site);
        debug!(
            "TRACE_PROCESSOR | apm_dd_url not configured, auto-generated from site '{}': {}",
            site, generated_url
        );
        generated_url
    } else {
        config.apm_dd_url.clone()
    };

    let url = hyper::Uri::from_str(&apm_url).unwrap_or_else(|e| {
        error!(
            "TRACE_PROCESSOR | Failed to parse apm_dd_url '{}': {}. \
            This should not happen - please report this issue.",
            apm_url, e
        );
        // This should never happen since trace_intake_url generates valid URLs
        // But if it does, use the default datadoghq.com URL
        hyper::Uri::from_str(&trace_intake_url("datadoghq.com"))
            .expect("default trace intake URL should always be valid")
    });

    Endpoint {
        url,
        api_key: None,
        timeout_ms: config.flush_timeout * S_TO_MS,
        test_token: None,
    }
}

/// Filters spans based on tag requirements and rejections.
///
/// This function implements four types of tag-based filtering configured via environment variables:
///
/// # Filtering Types
///
/// 1. **Require Tags** (`DD_APM_FILTER_TAGS_REQUIRE`):
///    - Span must match ALL tags exactly (AND logic)
///    - Example: `service:web,env:prod` requires both tags
///    - Returns `true` (drop span) if any required tag is missing
///
/// 2. **Require Regex** (`DD_APM_FILTER_TAGS_REGEX_REQUIRE`):
///    - Span must match ALL regex patterns (AND logic)
///    - Example: `service:web-.*,env:prod-.*` requires both patterns
///    - Returns `true` (drop span) if any required pattern doesn't match
///
/// 3. **Reject Tags** (`DD_APM_FILTER_TAGS_REJECT`):
///    - Span is dropped if ANY tag matches exactly (OR logic)
///    - Example: `service:test,env:debug` rejects if either tag matches
///    - Returns `true` (drop span) if any reject tag matches
///
/// 4. **Reject Regex** (`DD_APM_FILTER_TAGS_REGEX_REJECT`):
///    - Span is dropped if ANY regex pattern matches (OR logic)
///    - Example: `service:test-.*,env:debug-.*` rejects if either pattern matches
///    - Returns `true` (drop span) if any reject pattern matches
///
/// # Arguments
///
/// * `span` - The span to check for filtering
/// * `config` - Agent configuration containing filter rules
///
/// # Returns
///
/// * `true` - Span should be filtered out (dropped)
/// * `false` - Span passes all filters (keep)
///
/// # Examples
///
/// ```text
/// # Require all spans to have service:web AND env:prod
/// DD_APM_FILTER_TAGS_REQUIRE="service:web,env:prod"
///
/// # Reject all spans with env:test OR env:debug
/// DD_APM_FILTER_TAGS_REJECT="env:test,env:debug"
///
/// # Require service name starting with "api-"
/// DD_APM_FILTER_TAGS_REGEX_REQUIRE="service:api-.*"
/// ```
fn filter_span_by_tags(span: &Span, config: &config::Config) -> bool {
    // Handle required tags from DD_APM_FILTER_TAGS_REQUIRE (exact match)
    // If configured, ALL required tags must be present (AND logic)
    if let Some(require_tags) = &config.apm_filter_tags_require {
        if !require_tags.is_empty() {
            let matches_require = require_tags
                .iter()
                .all(|filter| span_matches_tag_exact_filter(span, filter));
            if !matches_require {
                debug!(
                    "TRACE_PROCESSOR | Filtering out span '{}' - doesn't match all required tags {}",
                    span.name,
                    require_tags.join(", ")
                );
                return true;
            }
        }
    }

    // Handle required regex tags from DD_APM_FILTER_TAGS_REGEX_REQUIRE (regex match)
    if let Some(require_regex_tags) = &config.apm_filter_tags_regex_require {
        if !require_regex_tags.is_empty() {
            let matches_require_regex = require_regex_tags
                .iter()
                .all(|filter| span_matches_tag_regex_filter(span, filter));
            if !matches_require_regex {
                debug!(
                    "TRACE_PROCESSOR | Filtering out span '{}' - doesn't match all required regex tags {}",
                    span.name,
                    require_regex_tags.join(", ")
                );
                return true;
            }
        }
    }

    // Handle reject tags from DD_APM_FILTER_TAGS_REJECT (exact match)
    if let Some(reject_tags) = &config.apm_filter_tags_reject {
        if !reject_tags.is_empty() {
            let matches_reject = reject_tags
                .iter()
                .any(|filter| span_matches_tag_exact_filter(span, filter));
            if matches_reject {
                debug!(
                    "TRACE_PROCESSOR | Filtering out span '{}' - matches reject tags {}",
                    span.name,
                    reject_tags.join(", ")
                );
                return true;
            }
        }
    }

    // Handle reject regex tags from DD_APM_FILTER_TAGS_REGEX_REJECT (regex match)
    if let Some(reject_regex_tags) = &config.apm_filter_tags_regex_reject {
        if !reject_regex_tags.is_empty() {
            let matches_reject_regex = reject_regex_tags
                .iter()
                .any(|filter| span_matches_tag_regex_filter(span, filter));
            if matches_reject_regex {
                debug!(
                    "TRACE_PROCESSOR | Filtering out span '{}' - matches reject regex tags {}",
                    span.name,
                    reject_regex_tags.join(", ")
                );
                return true;
            }
        }
    }

    false
}

fn span_matches_tag_exact_filter(span: &Span, filter: &str) -> bool {
    let parts: Vec<&str> = filter.splitn(2, ':').collect();

    if parts.len() == 2 {
        let key = parts[0].trim();
        let value = parts[1].trim();
        span_matches_tag_exact(span, key, value)
    } else if parts.len() == 1 {
        let key = parts[0].trim();
        span_has_key(span, key)
    } else {
        false
    }
}

fn span_matches_tag_regex_filter(span: &Span, filter: &str) -> bool {
    let parts: Vec<&str> = filter.splitn(2, ':').collect();

    if parts.len() == 2 {
        let key = parts[0].trim();
        let value = parts[1].trim();
        span_matches_tag_regex(span, key, value)
    } else if parts.len() == 1 {
        let key = parts[0].trim();
        span_has_key(span, key)
    } else {
        false
    }
}

fn span_matches_tag_exact(span: &Span, key: &str, value: &str) -> bool {
    if let Some(span_value) = span.meta.get(key) {
        if span_value == value {
            return true;
        }
    }

    let span_property_value = match key {
        "name" => Some(&span.name),
        "service" => Some(&span.service),
        "resource" => Some(&span.resource),
        "type" => Some(&span.r#type),
        _ => None,
    };

    if let Some(prop_value) = span_property_value {
        if prop_value == value {
            return true;
        }
    }

    false
}

fn span_matches_tag_regex(span: &Span, key: &str, value: &str) -> bool {
    let Ok(regex) = Regex::new(value) else {
        debug!(
            "TRACE_PROCESSOR | Invalid regex pattern '{}' for key '{}', treating as non-match",
            value, key
        );
        return false;
    };

    if let Some(span_value) = span.meta.get(key) {
        if regex.is_match(span_value) {
            return true;
        }
    }

    let span_property_value = match key {
        "name" => Some(&span.name),
        "service" => Some(&span.service),
        "resource" => Some(&span.resource),
        "type" => Some(&span.r#type),
        _ => None,
    };
    if let Some(prop_value) = span_property_value {
        if regex.is_match(prop_value) {
            return true;
        }
    }

    false
}

fn span_has_key(span: &Span, key: &str) -> bool {
    if span.meta.contains_key(key) {
        return true;
    }
    match key {
        "name" => !span.name.is_empty(),
        "service" => !span.service.is_empty(),
        "resource" => !span.resource.is_empty(),
        "type" => !span.r#type.is_empty(),
        _ => false,
    }
}

#[allow(clippy::module_name_repetitions)]
#[allow(clippy::too_many_arguments)]
#[async_trait]
pub trait TraceProcessor {
    fn process_traces(
        &self,
        config: Arc<config::Config>,
        tags_provider: Arc<provider::Provider>,
        header_tags: tracer_header_tags::TracerHeaderTags<'_>,
        traces: Vec<Vec<pb::Span>>,
        body_size: usize,
        span_pointers: Option<Vec<SpanPointer>>,
    ) -> (SendDataBuilderInfo, TracerPayloadCollection);
}

/// Trace processor for agent deployments
///
/// This processor handles trace processing including:
/// - Span obfuscation for sensitive data
/// - Agent metadata tagging (_dd.agent_version, _dd.agent_hostname)
/// - Tag enrichment from tags provider
/// - AppSec span pointers attachment
/// - Trace filtering based on configured rules
#[derive(Clone)]
#[allow(clippy::module_name_repetitions)]
pub struct GenericTraceProcessor {
    pub obfuscation_config: Arc<obfuscation_config::ObfuscationConfig>,
}

impl GenericTraceProcessor {
    #[must_use]
    pub fn new(obfuscation_config: Arc<obfuscation_config::ObfuscationConfig>) -> Self {
        Self { obfuscation_config }
    }
}

struct GenericChunkProcessor {
    config: Arc<config::Config>,
    obfuscation_config: Arc<obfuscation_config::ObfuscationConfig>,
    tags_provider: Arc<provider::Provider>,
    span_pointers: Option<Vec<SpanPointer>>,
    // Pre-allocated agent metadata to avoid repeated allocations
    agent_version: String,
    agent_hostname: String,
}

impl TraceChunkProcessor for GenericChunkProcessor {
    fn process(&mut self, chunk: &mut pb::TraceChunk, root_span_index: usize) {
        if let Some(root_span) = chunk.spans.get(root_span_index) {
            if filter_span_by_tags(root_span, &self.config) {
                chunk.spans.clear();
                return;
            }
        }

        // Add tags from tags provider to all spans
        for span in &mut chunk.spans {
            // Add tags from provider, but DO NOT overwrite service/version/env from the tracer
            // These should come from the application tracer, not the agent config
            self.tags_provider.get_tags_map().iter().for_each(|(k, v)| {
                if k == "service" || k == "version" || k == "env" {
                    // Only set if the span doesn't already have this tag
                    span.meta.entry(k.clone()).or_insert_with(|| v.clone());
                } else {
                    // Other tags can be set/overwritten by the agent
                    span.meta.insert(k.clone(), v.clone());
                }
            });

            // Always add agent metadata (using pre-allocated strings for efficiency)
            trace!(
                "TRACE_PROCESSOR | Setting agent metadata: version='{}', hostname='{}'",
                self.agent_version,
                self.agent_hostname
            );
            span.meta
                .insert("_dd.agent_version".to_string(), self.agent_version.clone());
            span.meta.insert(
                "_dd.agent_hostname".to_string(),
                self.agent_hostname.clone(),
            );

            // Obfuscate sensitive data
            obfuscate_span(span, &self.obfuscation_config);
        }

        // Attach span pointers to root span (for AppSec integration)
        if let Some(span) = chunk.spans.get_mut(root_span_index) {
            attach_span_pointers_to_meta(&mut span.meta, &self.span_pointers);
        }
    }
}

#[async_trait]
impl TraceProcessor for GenericTraceProcessor {
    fn process_traces(
        &self,
        config: Arc<config::Config>,
        tags_provider: Arc<provider::Provider>,
        header_tags: tracer_header_tags::TracerHeaderTags<'_>,
        traces: Vec<Vec<pb::Span>>,
        body_size: usize,
        span_pointers: Option<Vec<SpanPointer>>,
    ) -> (SendDataBuilderInfo, TracerPayloadCollection) {
        let mut payload = trace_utils::collect_pb_trace_chunks(
            traces,
            &header_tags,
            &mut GenericChunkProcessor {
                config: config.clone(),
                obfuscation_config: self.obfuscation_config.clone(),
                tags_provider: tags_provider.clone(),
                span_pointers,
                agent_version: AGENT_VERSION.to_string(),
                agent_hostname: hostname::get_hostname(),
            },
            true, // send agentless since we are the agent
        )
        .unwrap_or_else(|e| {
            error!("TRACE_PROCESSOR | Error processing traces: {:?}", e);
            TracerPayloadCollection::V07(vec![])
        });

        if let TracerPayloadCollection::V07(ref mut collection) = payload {
            // Add function tags (service, env, version) to all payloads
            // Using get_tags_map() which returns a reference to avoid cloning the entire HashMap
            let tags = tags_provider.get_tags_map();
            for tracer_payload in collection.iter_mut() {
                // Extend by iterating over the reference - only clones the values we need
                tracer_payload
                    .tags
                    .extend(tags.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
        }

        let endpoint = create_endpoint_from_config(&config);

        let builder = SendDataBuilder::new(body_size, payload.clone(), header_tags, &endpoint)
            .with_compression(Compression::Zstd(config.apm_config_compression_level))
            .with_retry_strategy(RetryStrategy::new(
                1,
                100,
                RetryBackoffType::Exponential,
                None,
            ));

        (SendDataBuilderInfo::new(builder, body_size), payload)
    }
}

/// A utility that is used to process, then send traces to the trace aggregator.
///
/// This applies [`AppSecProcessor::process_span`] on the `aws.lambda` span
/// contained in the traces (if any), and may buffer the traces if the
/// [`AppSecProcessor`] has not yet seen the corresponding response payload.
///
/// Once ready to flush, the traces are submitted to the provided [`Sender`].
#[derive(Clone)]
pub struct SendingTraceProcessor {
    /// The [`AppSecProcessor`] to use for security-processing the traces, if
    /// configured.
    #[cfg(feature = "appsec")]
    pub appsec: Option<Arc<Mutex<AppSecProcessor>>>,
    /// The [`TraceProcessor`]  to use for transforming raw traces into
    /// [`SendDataBuilderInfo`]s before flushing.
    pub processor: Arc<dyn TraceProcessor + Send + Sync>,
    /// The [`Sender`] to use for flushing the traces to the trace aggregator.
    pub trace_tx: Sender<SendDataBuilderInfo>,
    /// The [`StatsGenerator`] to use for generating stats and sending them to
    /// the stats concentrator.
    pub stats_generator: Arc<StatsGenerator>,
}
impl SendingTraceProcessor {
    /// Processes the provided traces, then flushes them to the trace aggregator
    /// for sending to the backend.
    #[cfg_attr(not(feature = "appsec"), allow(unused_mut))]
    pub async fn send_processed_traces(
        &self,
        config: Arc<config::Config>,
        tags_provider: Arc<provider::Provider>,
        header_tags: tracer_header_tags::TracerHeaderTags<'_>,
        mut traces: Vec<Vec<pb::Span>>,
        body_size: usize,
        span_pointers: Option<Vec<SpanPointer>>,
    ) -> Result<(), SendError<SendDataBuilderInfo>> {
        #[cfg(feature = "appsec")]
        {
            traces = if let Some(appsec) = &self.appsec {
                let mut appsec = appsec.lock().await;
                traces
                    .into_iter()
                    .filter_map(|mut trace| {
                        let Some(span) = AppSecProcessor::service_entry_span_mut(&mut trace) else {
                            return Some(trace);
                        };

                        let (finalized, ctx) = appsec.process_span(span);
                        if finalized {
                            Some(trace)
                        } else if let Some(ctx) = ctx {
                            debug!(
                                "TRACE_PROCESSOR | Holding trace for App & API Protection additional data"
                            );
                            ctx.hold_trace(
                                trace,
                                SendingTraceProcessor {
                                    appsec: None,
                                    processor: self.processor.clone(),
                                    trace_tx: self.trace_tx.clone(),
                                    stats_generator: self.stats_generator.clone(),
                                },
                                HoldArguments {
                                    config: Arc::clone(&config),
                                    tags_provider: Arc::clone(&tags_provider),
                                    body_size,
                                    span_pointers: span_pointers.clone(),
                                    tracer_header_tags_lang: header_tags.lang.to_string(),
                                    tracer_header_tags_lang_version: header_tags
                                        .lang_version
                                        .to_string(),
                                    tracer_header_tags_lang_interpreter: header_tags
                                        .lang_interpreter
                                        .to_string(),
                                    tracer_header_tags_lang_vendor: header_tags.lang_vendor.to_string(),
                                    tracer_header_tags_tracer_version: header_tags
                                        .tracer_version
                                        .to_string(),
                                    tracer_header_tags_container_id: header_tags
                                        .container_id
                                        .to_string(),
                                    tracer_header_tags_client_computed_top_level: header_tags
                                        .client_computed_top_level,
                                    tracer_header_tags_client_computed_stats: header_tags
                                        .client_computed_stats,
                                    tracer_header_tags_dropped_p0_traces: header_tags
                                        .dropped_p0_traces,
                                    tracer_header_tags_dropped_p0_spans: header_tags
                                        .dropped_p0_spans,
                                },
                            );
                            None
                        } else {
                            Some(trace)
                        }
                    })
                    .collect()
            } else {
                traces
            };
        }

        if traces.is_empty() {
            debug!("TRACE_PROCESSOR | no traces left to be sent, skipping...");
            return Ok(());
        }

        let (payload, processed_traces) = self.processor.process_traces(
            config.clone(),
            tags_provider,
            header_tags,
            traces,
            body_size,
            span_pointers,
        );
        self.trace_tx.send(payload).await?;

        // This needs to be after process_traces() because process_traces()
        // performs obfuscation, and we need to compute stats on the obfuscated traces.
        if config.compute_trace_stats_on_extension {
            if let Err(err) = self.stats_generator.send(&processed_traces) {
                // Just log the error. We don't think trace stats are critical, so we don't want to
                // return an error if only stats fail to send.
                error!("TRACE_PROCESSOR | Error sending traces to the stats concentrator: {err}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        time::{SystemTime, UNIX_EPOCH},
    };

    use datadog_trace_obfuscation::obfuscation_config::ObfuscationConfig;

    use crate::{config::Config, tags::provider::Provider};

    use super::*;

    fn get_current_timestamp_nanos() -> i64 {
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time went backwards")
                .as_nanos(),
        )
        .expect("can't parse time")
    }

    fn create_test_config() -> Arc<Config> {
        Arc::new(Config {
            apm_dd_url: "https://trace.agent.datadoghq.com".to_string(),
            service: Some("test-service".to_string()),
            tags: HashMap::from([
                ("test".to_string(), "tag".to_string()),
                ("env".to_string(), "test-env".to_string()),
            ]),
            ..Config::default()
        })
    }

    fn create_tags_provider(config: Arc<Config>) -> Arc<Provider> {
        Arc::new(Provider::new(config))
    }
    fn create_test_span(
        trace_id: u64,
        span_id: u64,
        parent_id: u64,
        start: i64,
        is_top_level: bool,
        tags_provider: Arc<Provider>,
    ) -> pb::Span {
        let mut meta: HashMap<String, String> = tags_provider.get_tags_map().clone();
        meta.insert(
            "runtime-id".to_string(),
            "test-runtime-id-value".to_string(),
        );

        let mut span = pb::Span {
            trace_id,
            span_id,
            service: "test-service".to_string(),
            name: "test_name".to_string(),
            resource: "test-resource".to_string(),
            parent_id,
            start,
            duration: 5,
            error: 0,
            meta: meta.clone(),
            metrics: HashMap::new(),
            r#type: String::new(),
            meta_struct: HashMap::new(),
            span_links: vec![],
            span_events: vec![],
        };
        if is_top_level {
            span.metrics.insert("_top_level".to_string(), 1.0);
            span.meta
                .insert("_dd.origin".to_string(), "lambda".to_string());
            span.meta.insert("origin".to_string(), "lambda".to_string());
            span.meta
                .insert("functionname".to_string(), "my-function".to_string());
            span.r#type = String::new();
        }
        span
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    #[cfg_attr(miri, ignore)]
    async fn test_process_trace() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let start = get_current_timestamp_nanos();

        let tags_provider = create_tags_provider(create_test_config());
        let mut span = create_test_span(11, 222, 333, start, true, tags_provider);

        // Add agent metadata tags that are added during processing
        span.meta.insert(
            "_dd.agent_version".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
        );
        span.meta
            .insert("_dd.agent_hostname".to_string(), hostname::get_hostname());

        let traces: Vec<Vec<pb::Span>> = vec![vec![span.clone()]];

        let header_tags = tracer_header_tags::TracerHeaderTags {
            lang: "nodejs",
            lang_version: "v19.7.0",
            lang_interpreter: "v8",
            lang_vendor: "vendor",
            tracer_version: "4.0.0",
            container_id: "33",
            client_computed_top_level: false,
            client_computed_stats: false,
            dropped_p0_traces: 0,
            dropped_p0_spans: 0,
        };

        let trace_processor = GenericTraceProcessor::new(Arc::new(
            ObfuscationConfig::new().expect("Failed to create ObfuscationConfig"),
        ));
        let config = create_test_config();
        let tags_provider = create_tags_provider(config.clone());
        let (tracer_payload, _processed_traces) = trace_processor.process_traces(
            config,
            tags_provider.clone(),
            header_tags,
            traces,
            100,
            None,
        );

        let expected_tracer_payload = pb::TracerPayload {
            container_id: "33".to_string(),
            language_name: "nodejs".to_string(),
            language_version: "v19.7.0".to_string(),
            tracer_version: "4.0.0".to_string(),
            runtime_id: "test-runtime-id-value".to_string(),
            chunks: vec![pb::TraceChunk {
                priority: i32::from(i8::MIN),
                origin: "lambda".to_string(), // Origin extracted from span meta
                spans: vec![span.clone()],
                tags: HashMap::new(),
                dropped_trace: false,
            }],
            tags: tags_provider.get_tags_map().clone(),
            env: "test-env".to_string(),
            hostname: String::new(),
            app_version: String::new(),
        };

        let received_payload = if let TracerPayloadCollection::V07(payload) =
            tracer_payload.builder.build().get_payloads()
        {
            Some(payload[0].clone())
        } else {
            None
        };

        assert_eq!(
            expected_tracer_payload,
            received_payload.expect("no payload received")
        );
    }

    #[test]
    fn test_span_matches_tag_exact() {
        let span = Span {
            service: "my-service".to_string(),
            meta: {
                let mut meta = std::collections::HashMap::new();
                meta.insert("env".to_string(), "production".to_string());
                meta
            },
            ..Default::default()
        };

        assert!(span_matches_tag_exact(&span, "env", "production"));
        assert!(!span_matches_tag_exact(&span, "env", "")); // Empty string doesn't match non-empty value
        assert!(!span_matches_tag_exact(&span, "env", "development"));

        assert!(span_matches_tag_exact(&span, "service", "my-service"));
        assert!(!span_matches_tag_exact(&span, "service", "")); // Empty string doesn't match non-empty value
        assert!(!span_matches_tag_exact(&span, "service", "other-service"));
    }

    #[test]
    fn test_span_matches_tag_regex() {
        let span = Span {
            service: "user-service".to_string(),
            meta: {
                let mut meta = std::collections::HashMap::new();
                meta.insert("env".to_string(), "production".to_string());
                meta
            },
            ..Default::default()
        };

        assert!(span_matches_tag_regex(&span, "env", r"prod.*"));
        assert!(span_matches_tag_regex(&span, "env", ""));
        assert!(!span_matches_tag_regex(&span, "env", r"dev.*"));

        assert!(span_matches_tag_regex(&span, "service", r".*-service"));
        assert!(span_matches_tag_regex(&span, "service", ""));
        assert!(!span_matches_tag_regex(&span, "service", r"api-.*"));

        assert!(!span_matches_tag_regex(&span, "env", r"[unclosed"));
    }

    #[test]
    fn test_span_matches_tag_with_config() {
        let span = Span {
            service: "user-service".to_string(),
            meta: {
                let mut meta = std::collections::HashMap::new();
                meta.insert("env".to_string(), "test-environment".to_string());
                meta
            },
            ..Default::default()
        };

        assert!(span_matches_tag_exact(&span, "env", "test-environment"));
        assert!(!span_matches_tag_exact(&span, "env", "test"));

        assert!(span_matches_tag_regex(&span, "env", r"test.*"));
        assert!(!span_matches_tag_regex(&span, "env", r"prod.*"));
    }

    #[test]
    fn test_filter_span_require_tags() {
        let config = Config {
            apm_filter_tags_require: Some(vec![
                "env:production".to_string(),
                "service:api".to_string(),
            ]),
            ..Default::default()
        };

        // Span that matches ALL of the require tags
        let mut span_match_all = Span::default();
        span_match_all
            .meta
            .insert("env".to_string(), "production".to_string());
        span_match_all
            .meta
            .insert("service".to_string(), "api".to_string());
        assert!(!filter_span_by_tags(&span_match_all, &config));

        // Span that matches only one of the require tags
        let mut span_partial_match = Span::default();
        span_partial_match
            .meta
            .insert("env".to_string(), "production".to_string());
        assert!(filter_span_by_tags(&span_partial_match, &config));

        // Span that doesn't match any require tags
        let mut span_no_match = Span::default();
        span_no_match
            .meta
            .insert("env".to_string(), "development".to_string());
        assert!(filter_span_by_tags(&span_no_match, &config));
    }

    #[test]
    fn test_filter_span_reject_tags() {
        let config = Config {
            apm_filter_tags_reject: Some(vec!["debug:true".to_string(), "env:test".to_string()]),
            ..Default::default()
        };

        // Span that matches a reject tag
        let mut span_reject = Span::default();
        span_reject
            .meta
            .insert("debug".to_string(), "true".to_string());
        span_reject
            .meta
            .insert("env".to_string(), "production".to_string());
        assert!(filter_span_by_tags(&span_reject, &config));

        // Span that doesn't match any reject tags
        let mut span_keep = Span::default();
        span_keep
            .meta
            .insert("env".to_string(), "production".to_string());
        assert!(!filter_span_by_tags(&span_keep, &config));
    }

    #[test]
    fn test_filter_span_require_and_reject_tags() {
        let config = Config {
            apm_filter_tags_require: Some(vec!["env:production".to_string()]),
            apm_filter_tags_reject: Some(vec!["debug:true".to_string()]),
            ..Default::default()
        };

        // Span that matches require but also matches reject
        let mut span_both = Span::default();
        span_both
            .meta
            .insert("env".to_string(), "production".to_string());
        span_both
            .meta
            .insert("debug".to_string(), "true".to_string());
        assert!(filter_span_by_tags(&span_both, &config));

        // Span that matches require and doesn't match reject
        let mut span_good = Span::default();
        span_good
            .meta
            .insert("env".to_string(), "production".to_string());
        assert!(!filter_span_by_tags(&span_good, &config));
    }

    #[test]
    fn test_root_span_filtering_drops_entire_trace() {
        use crate::tags::provider::Provider;
        use datadog_trace_obfuscation::obfuscation_config::ObfuscationConfig;
        use std::sync::Arc;

        let root_span = pb::Span {
            name: "lambda.invoke".to_string(),
            service: "my-service".to_string(),
            resource: "my-resource".to_string(),
            trace_id: 123,
            span_id: 456,
            parent_id: 0,
            start: 1000,
            duration: 5000,
            error: 0,
            meta: {
                let mut meta = std::collections::HashMap::new();
                meta.insert("env".to_string(), "test".to_string());
                meta
            },
            metrics: std::collections::HashMap::new(),
            r#type: String::new(),
            span_links: vec![],
            meta_struct: std::collections::HashMap::new(),
            span_events: vec![],
        };

        let child_span = pb::Span {
            name: "child.operation".to_string(),
            service: "my-service".to_string(),
            resource: "child-resource".to_string(),
            trace_id: 123,
            span_id: 789,
            parent_id: 456,
            start: 1100,
            duration: 1000,
            error: 0,
            meta: std::collections::HashMap::new(),
            metrics: std::collections::HashMap::new(),
            r#type: String::new(),
            span_links: vec![],
            meta_struct: std::collections::HashMap::new(),
            span_events: vec![],
        };

        let mut chunk = pb::TraceChunk {
            priority: 1,
            origin: "lambda".to_string(),
            spans: vec![root_span, child_span],
            tags: std::collections::HashMap::new(),
            dropped_trace: false,
        };

        let config = Arc::new(Config {
            apm_filter_tags_reject: Some(vec!["env:test".to_string()]),
            ..Default::default()
        });

        let mut processor = GenericChunkProcessor {
            config: config.clone(),
            obfuscation_config: Arc::new(
                ObfuscationConfig::new().expect("Failed to create ObfuscationConfig"),
            ),
            tags_provider: Arc::new(Provider::new(config)),
            span_pointers: None,
            agent_version: AGENT_VERSION.to_string(),
            agent_hostname: hostname::get_hostname(),
        };

        processor.process(&mut chunk, 0);

        assert_eq!(
            chunk.spans.len(),
            0,
            "Entire trace should be dropped when root span matches filter rules"
        );
    }

    #[test]
    fn test_root_span_filtering_allows_trace_when_no_match() {
        use crate::tags::provider::Provider;
        use datadog_trace_obfuscation::obfuscation_config::ObfuscationConfig;
        use std::sync::Arc;

        let root_span = pb::Span {
            name: "lambda.invoke".to_string(),
            service: "my-service".to_string(),
            resource: "my-resource".to_string(),
            trace_id: 123,
            span_id: 456,
            parent_id: 0,
            start: 1000,
            duration: 5000,
            error: 0,
            meta: {
                let mut meta = std::collections::HashMap::new();
                meta.insert("env".to_string(), "production".to_string());
                meta
            },
            metrics: std::collections::HashMap::new(),
            r#type: String::new(),
            span_links: vec![],
            meta_struct: std::collections::HashMap::new(),
            span_events: vec![],
        };

        let child_span = pb::Span {
            name: "child.operation".to_string(),
            service: "my-service".to_string(),
            resource: "child-resource".to_string(),
            trace_id: 123,
            span_id: 789,
            parent_id: 456,
            start: 1100,
            duration: 1000,
            error: 0,
            meta: std::collections::HashMap::new(),
            metrics: std::collections::HashMap::new(),
            r#type: String::new(),
            span_links: vec![],
            meta_struct: std::collections::HashMap::new(),
            span_events: vec![],
        };

        let mut chunk = pb::TraceChunk {
            priority: 1,
            origin: "lambda".to_string(),
            spans: vec![root_span, child_span],
            tags: std::collections::HashMap::new(),
            dropped_trace: false,
        };

        let config = Arc::new(Config {
            apm_filter_tags_reject: Some(vec!["env:test".to_string()]),
            ..Default::default()
        });

        let mut processor = GenericChunkProcessor {
            config: config.clone(),
            obfuscation_config: Arc::new(
                ObfuscationConfig::new().expect("Failed to create ObfuscationConfig"),
            ),
            tags_provider: Arc::new(Provider::new(config)),
            span_pointers: None,
            agent_version: AGENT_VERSION.to_string(),
            agent_hostname: hostname::get_hostname(),
        };

        processor.process(&mut chunk, 0);

        assert_eq!(
            chunk.spans.len(),
            2,
            "Trace should be kept when root span doesn't match filter rules"
        );
    }

    #[test]
    fn test_root_span_filtering_allows_trace_when_no_filter_tags() {
        use crate::tags::provider::Provider;
        use datadog_trace_obfuscation::obfuscation_config::ObfuscationConfig;
        use std::sync::Arc;

        let root_span = pb::Span {
            name: "lambda.invoke".to_string(),
            service: "my-service".to_string(),
            resource: "my-resource".to_string(),
            trace_id: 123,
            span_id: 456,
            parent_id: 0,
            start: 1000,
            duration: 5000,
            error: 0,
            meta: {
                let mut meta = std::collections::HashMap::new();
                meta.insert("env".to_string(), "production".to_string());
                meta
            },
            metrics: std::collections::HashMap::new(),
            r#type: String::new(),
            span_links: vec![],
            meta_struct: std::collections::HashMap::new(),
            span_events: vec![],
        };

        let child_span = pb::Span {
            name: "child.operation".to_string(),
            service: "my-service".to_string(),
            resource: "child-resource".to_string(),
            trace_id: 123,
            span_id: 789,
            parent_id: 456,
            start: 1100,
            duration: 1000,
            error: 0,
            meta: std::collections::HashMap::new(),
            metrics: std::collections::HashMap::new(),
            r#type: String::new(),
            span_links: vec![],
            meta_struct: std::collections::HashMap::new(),
            span_events: vec![],
        };

        let mut chunk = pb::TraceChunk {
            priority: 1,
            origin: "lambda".to_string(),
            spans: vec![root_span, child_span],
            tags: std::collections::HashMap::new(),
            dropped_trace: false,
        };

        let config = Arc::new(Config {
            ..Default::default()
        });

        let mut processor = GenericChunkProcessor {
            config: config.clone(),
            obfuscation_config: Arc::new(
                ObfuscationConfig::new().expect("Failed to create ObfuscationConfig"),
            ),
            tags_provider: Arc::new(Provider::new(config)),
            span_pointers: None,
            agent_version: AGENT_VERSION.to_string(),
            agent_hostname: hostname::get_hostname(),
        };

        processor.process(&mut chunk, 0);

        assert_eq!(
            chunk.spans.len(),
            2,
            "Trace should be kept when no filter tags are set."
        );
    }
}
