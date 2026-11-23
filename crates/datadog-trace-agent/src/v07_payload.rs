// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Msgpack v0.7 trace payloads: one tracer payload with its own metadata and chunks, decoded as
//! the Go agent's `TracerPayload.UnmarshalMsg` and converted as its `ConvertToIdx`.

use std::collections::HashMap;

use anyhow::{Context, anyhow, bail};
use libdd_trace_protobuf::pb;
use rmpv::Value;

use crate::trace_processor::{DECISION_MAKER_KEY, PRIORITY_NONE};

/// The Go decoder's limit on the length of an array or a map.
const MAX_LENGTH: usize = 500_000;

/// The fields of a v0.7 tracer payload that are not in its chunks.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct PayloadFields {
    container_id: String,
    language_name: String,
    language_version: String,
    tracer_version: String,
    runtime_id: String,
    tags: HashMap<String, String>,
    env: String,
    hostname: String,
    app_version: String,
}

/// The fields of a v0.7 chunk that are not its spans. The tracer's dropped flag is not kept,
/// because the samplers decide it, as in Go.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ChunkFields {
    /// 0 when missing, as in Go.
    priority: i32,
    origin: String,
    tags: HashMap<String, String>,
}

/// A v0.7 chunk with at least one span.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Chunk {
    pub(crate) fields: ChunkFields,
    pub(crate) spans: Vec<pb::Span>,
}

impl AsMut<Vec<pb::Span>> for Chunk {
    fn as_mut(&mut self) -> &mut Vec<pb::Span> {
        &mut self.spans
    }
}

/// Decodes a v0.7 body. As in Go, the payload, its chunks and its spans must be maps, unknown
/// fields are skipped, null chunks and spans are skipped, chunks without spans are dropped, and
/// arrays and maps are limited to 500,000 entries.
pub(crate) fn decode(mut body: &[u8]) -> anyhow::Result<(PayloadFields, Vec<Chunk>)> {
    let value = rmpv::decode::read_value(&mut body).context("Error reading v0.7 payload")?;
    let mut fields = PayloadFields::default();
    let mut chunks = Vec::new();
    for (key, value) in map(value, "payload")? {
        match key.as_str() {
            "container_id" => fields.container_id = string(value, &key)?,
            "language_name" => fields.language_name = string(value, &key)?,
            "language_version" => fields.language_version = string(value, &key)?,
            "tracer_version" => fields.tracer_version = string(value, &key)?,
            "runtime_id" => fields.runtime_id = string(value, &key)?,
            "tags" => fields.tags = string_map(value, &key)?,
            "env" => fields.env = string(value, &key)?,
            "hostname" => fields.hostname = string(value, &key)?,
            "app_version" => fields.app_version = string(value, &key)?,
            "chunks" => {
                for chunk in array(value, &key)? {
                    if matches!(chunk, Value::Nil) {
                        continue;
                    }
                    let chunk = decode_chunk(chunk)?;
                    if !chunk.spans.is_empty() {
                        chunks.push(chunk);
                    }
                }
            }
            _ => {}
        }
    }
    Ok((fields, chunks))
}

fn decode_chunk(value: Value) -> anyhow::Result<Chunk> {
    let mut chunk = Chunk::default();
    for (key, value) in map(value, "chunk")? {
        match key.as_str() {
            "priority" => {
                chunk.fields.priority = value
                    .as_i64()
                    .and_then(|priority| i32::try_from(priority).ok())
                    .ok_or_else(|| anyhow!("{key} is not a 32-bit integer: {value}"))?;
            }
            "origin" => chunk.fields.origin = string(value, &key)?,
            "tags" => chunk.fields.tags = string_map(value, &key)?,
            "dropped_trace" => {
                if !value.is_bool() {
                    bail!("{key} is not a boolean: {value}");
                }
            }
            "spans" => {
                for span in array(value, &key)? {
                    if matches!(span, Value::Nil) {
                        continue;
                    }
                    if !span.is_map() {
                        bail!("span is not a map: {span}");
                    }
                    let mut bytes = Vec::new();
                    rmpv::encode::write_value(&mut bytes, &span)?;
                    chunk.spans.push(
                        rmp_serde::from_slice(&bytes)
                            .map_err(|err| anyhow!("Error reading span: {err}"))?,
                    );
                }
            }
            _ => {}
        }
    }
    Ok(chunk)
}

fn map(value: Value, name: &str) -> anyhow::Result<Vec<(String, Value)>> {
    let Value::Map(entries) = value else {
        bail!("{name} is not a map: {value}");
    };
    if entries.len() > MAX_LENGTH {
        bail!("{name} has more than {MAX_LENGTH} entries");
    }
    entries
        .into_iter()
        .map(|(key, value)| Ok((string(key, name)?, value)))
        .collect()
}

fn array(value: Value, name: &str) -> anyhow::Result<Vec<Value>> {
    let Value::Array(entries) = value else {
        bail!("{name} is not an array: {value}");
    };
    if entries.len() > MAX_LENGTH {
        bail!("{name} has more than {MAX_LENGTH} entries");
    }
    Ok(entries)
}

fn string(value: Value, name: &str) -> anyhow::Result<String> {
    match value {
        Value::String(string) => string
            .into_str()
            .ok_or_else(|| anyhow!("{name} is not valid UTF-8")),
        value => bail!("{name} is not a string: {value}"),
    }
}

fn string_map(value: Value, name: &str) -> anyhow::Result<HashMap<String, String>> {
    map(value, name)?
        .into_iter()
        .map(|(key, value)| Ok((key, string(value, name)?)))
        .collect()
}

impl ChunkFields {
    /// Gives the chunk the payload's priority and origin when the spans have none, as Go's
    /// `ConvertToIdx` does, and the payload's tags, where the decision maker promoted from the
    /// spans wins.
    pub(crate) fn apply_to_chunk(self, chunk: &mut pb::TraceChunk) {
        if chunk.priority == PRIORITY_NONE {
            chunk.priority = self.priority;
        }
        if chunk.origin.is_empty() {
            chunk.origin = self.origin;
        }
        let span_decision_maker = chunk.tags.remove(DECISION_MAKER_KEY);
        chunk.tags.extend(self.tags);
        if let Some(decision_maker) = span_decision_maker {
            chunk
                .tags
                .insert(DECISION_MAKER_KEY.to_string(), decision_maker);
        }
    }
}

impl PayloadFields {
    /// Gives the tracer payload the payload's tracer, container and runtime fields, which replace
    /// the headers' and the spans' values as in Go, its non-empty env, hostname and app version,
    /// which replace the values found in the spans, and its tags.
    pub(crate) fn apply_to_payload(&self, tracer_payload: &mut pb::TracerPayload) {
        tracer_payload.container_id.clone_from(&self.container_id);
        tracer_payload.language_name.clone_from(&self.language_name);
        tracer_payload
            .language_version
            .clone_from(&self.language_version);
        tracer_payload
            .tracer_version
            .clone_from(&self.tracer_version);
        tracer_payload.runtime_id.clone_from(&self.runtime_id);
        for (target, value) in [
            (&mut tracer_payload.env, &self.env),
            (&mut tracer_payload.hostname, &self.hostname),
            (&mut tracer_payload.app_version, &self.app_version),
        ] {
            if !value.is_empty() {
                target.clone_from(value);
            }
        }
        tracer_payload.tags.extend(self.tags.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value as Json, json};

    fn span(name: &str, trace_id: u64) -> Json {
        json!({"service": "svc", "name": name, "resource": "res", "trace_id": trace_id, "span_id": 1})
    }

    type OwnedSummary = (i32, String, Vec<(String, String)>, Vec<String>);
    type DecodeCase = (&'static str, Vec<u8>, Option<Vec<ChunkSummary>>);

    /// The priority, origin, tags and span names of each decoded chunk.
    type ChunkSummary = (
        i32,
        &'static str,
        Vec<(&'static str, &'static str)>,
        Vec<&'static str>,
    );

    fn summary(chunks: &[Chunk]) -> Vec<OwnedSummary> {
        chunks
            .iter()
            .map(|chunk| {
                let mut tags: Vec<_> = chunk.fields.tags.clone().into_iter().collect();
                tags.sort();
                (
                    chunk.fields.priority,
                    chunk.fields.origin.clone(),
                    tags,
                    chunk.spans.iter().map(|span| span.name.clone()).collect(),
                )
            })
            .collect()
    }

    fn owned(expected: &[ChunkSummary]) -> Vec<OwnedSummary> {
        expected
            .iter()
            .map(|(priority, origin, tags, names)| {
                (
                    *priority,
                    origin.to_string(),
                    tags.iter()
                        .map(|(key, value)| (key.to_string(), value.to_string()))
                        .collect(),
                    names.iter().map(|name| name.to_string()).collect(),
                )
            })
            .collect()
    }

    /// Each case gives a body, then the decoded chunks, or `None` for an error.
    #[test]
    fn test_decode_chunks() {
        let encode = |body: Json| rmp_serde::to_vec(&body).unwrap();
        let cases: Vec<DecodeCase> = vec![
            (
                "chunk fields, a null span, a dropped flag and an unknown field",
                encode(json!({"chunks": [{
                    "priority": 2,
                    "origin": "synthetics",
                    "tags": {"_dd.p.dm": "-4"},
                    "dropped_trace": true,
                    "spans": [span("first", 5), null, span("second", 5)],
                    "unknown": 1,
                }]})),
                Some(vec![(
                    2,
                    "synthetics",
                    vec![("_dd.p.dm", "-4")],
                    vec!["first", "second"],
                )]),
            ),
            (
                "a missing priority is 0",
                encode(json!({"chunks": [{"spans": [span("first", 5)]}]})),
                Some(vec![(0, "", vec![], vec!["first"])]),
            ),
            (
                "chunks with the same trace ID keep their own fields",
                encode(json!({"chunks": [
                    {"priority": 2, "origin": "synthetics", "spans": [span("first", 0)]},
                    {"priority": -128, "spans": [span("second", 0)]},
                ]})),
                Some(vec![
                    (2, "synthetics", vec![], vec!["first"]),
                    (-128, "", vec![], vec!["second"]),
                ]),
            ),
            (
                "null chunks and chunks without spans are dropped",
                encode(json!({"chunks": [null, {"priority": 1, "spans": [null]}, {"spans": []}]})),
                Some(vec![]),
            ),
            ("an empty body", vec![], None),
            (
                "a truncated body",
                encode(json!({"chunks": [{"spans": [span("first", 5)]}]}))[..20].to_vec(),
                None,
            ),
            ("a top-level array", encode(json!([[]])), None),
            (
                "a string priority",
                encode(json!({"chunks": [{"priority": "2"}]})),
                None,
            ),
            ("chunks in a map", encode(json!({"chunks": {}})), None),
            (
                "a span in an array",
                encode(json!({"chunks": [{"spans": [["svc"]]}]})),
                None,
            ),
            (
                "a wrongly typed span field",
                encode(json!({"chunks": [{"spans": [{"trace_id": "x"}]}]})),
                None,
            ),
            (
                "more than 500,000 chunks",
                encode(json!({"chunks": vec![Json::Null; 500_001]})),
                None,
            ),
        ];
        for (name, body, expected) in cases {
            let decoded = decode(&body).ok().map(|(_, chunks)| summary(&chunks));
            assert_eq!(decoded, expected.as_deref().map(owned), "{name}");
        }
    }

    /// Each case gives the chunk's priority, origin and decision maker after normalization and
    /// promotion from the spans, and the payload's chunk fields, then the chunk's priority,
    /// origin and tags.
    #[test]
    fn test_apply_to_chunk() {
        type Case = (
            &'static str,
            (i32, &'static str, Option<&'static str>),
            (i32, &'static str, &'static [(&'static str, &'static str)]),
            (i32, &'static str, &'static [(&'static str, &'static str)]),
        );
        let cases: [Case; 6] = [
            (
                "the span priority wins",
                (1, "", None),
                (2, "", &[]),
                (1, "", &[]),
            ),
            (
                "the payload priority without one in the spans",
                (PRIORITY_NONE, "", None),
                (2, "", &[]),
                (2, "", &[]),
            ),
            (
                "a missing payload priority is 0",
                (PRIORITY_NONE, "", None),
                (0, "", &[]),
                (0, "", &[]),
            ),
            (
                "the span origin wins",
                (1, "synthetics-browser", None),
                (1, "synthetics", &[]),
                (1, "synthetics-browser", &[]),
            ),
            (
                "the payload origin and tags without them in the spans",
                (1, "", None),
                (1, "synthetics", &[("_dd.p.dm", "-0"), ("other", "value")]),
                (1, "synthetics", &[("_dd.p.dm", "-0"), ("other", "value")]),
            ),
            (
                "the span decision maker wins",
                (1, "", Some("-4")),
                (1, "", &[("_dd.p.dm", "-0"), ("other", "value")]),
                (1, "", &[("_dd.p.dm", "-4"), ("other", "value")]),
            ),
        ];
        for (name, (priority, origin, decision_maker), payload, expected) in cases {
            let mut chunk = pb::TraceChunk {
                priority,
                origin: origin.to_string(),
                tags: decision_maker
                    .map(|value| (DECISION_MAKER_KEY.to_string(), value.to_string()))
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            let to_map = |tags: &[(&str, &str)]| -> HashMap<String, String> {
                tags.iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect()
            };
            ChunkFields {
                priority: payload.0,
                origin: payload.1.to_string(),
                tags: to_map(payload.2),
            }
            .apply_to_chunk(&mut chunk);
            assert_eq!(
                (chunk.priority, chunk.origin.as_str(), chunk.tags),
                (expected.0, expected.1, to_map(expected.2)),
                "{name}"
            );
        }
    }

    /// Each case gives the body's runtime ID and env, then the tracer payload's runtime ID and env,
    /// where the spans gave `span-runtime` and `span-env`.
    #[test]
    fn test_apply_to_payload() {
        let cases = [
            (("", ""), ("", "span-env")),
            (("body-runtime", "body-env"), ("body-runtime", "body-env")),
        ];
        for ((runtime_id, env), expected) in cases {
            let mut tracer_payload = pb::TracerPayload {
                runtime_id: "span-runtime".to_string(),
                env: "span-env".to_string(),
                language_name: "header-language".to_string(),
                ..Default::default()
            };
            PayloadFields {
                runtime_id: runtime_id.to_string(),
                env: env.to_string(),
                language_name: "python".to_string(),
                ..Default::default()
            }
            .apply_to_payload(&mut tracer_payload);
            assert_eq!(
                (
                    tracer_payload.runtime_id.as_str(),
                    tracer_payload.env.as_str(),
                    tracer_payload.language_name.as_str()
                ),
                (expected.0, expected.1, "python"),
                "body {runtime_id:?} {env:?}"
            );
        }
    }
}
