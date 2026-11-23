// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Removes the spans of calls to serverless-init's own trace agent and DogStatsD, and of DNS
//! lookups of local addresses, as Go serverless-init's `filterSpanV1` does.

use std::borrow::Cow;

use libdd_trace_protobuf::pb;

const HTTP_URL_KEY: &str = "http.url";
const TCP_REMOTE_HOST_KEY: &str = "tcp.remote.host";
const TCP_REMOTE_PORT_KEY: &str = "tcp.remote.port";
const DNS_ADDRESS_KEY: &str = "dns.address";
/// Go serverless-init's fixed addresses of its trace agent and DogStatsD.
const INTERNAL_URL_PREFIXES: [&str; 2] = ["http://127.0.0.1:8124", "http://127.0.0.1:8125"];
const INTERNAL_DNS_ADDRESS_PREFIXES: [&str; 2] = ["0.0.0.0", "127.0.0.1"];

/// Removes the internal spans from each trace, then the traces left empty. Returns whether any
/// span was removed.
pub(crate) fn discard_internal_spans(traces: &mut Vec<Vec<pb::Span>>) -> bool {
    let mut removed = false;
    for trace in traces.iter_mut() {
        let count = trace.len();
        trace.retain(|span| !is_internal_span(span));
        removed |= trace.len() != count;
    }
    traces.retain(|trace| !trace.is_empty());
    removed
}

fn is_internal_span(span: &pb::Span) -> bool {
    let has_internal_prefix = |value: &str| {
        INTERNAL_URL_PREFIXES
            .iter()
            .any(|prefix| value.starts_with(prefix))
    };
    if attribute(span, HTTP_URL_KEY).is_some_and(|url| has_internal_prefix(&url)) {
        return true;
    }
    if let (Some(host), Some(port)) = (
        attribute(span, TCP_REMOTE_HOST_KEY),
        attribute(span, TCP_REMOTE_PORT_KEY),
    ) && has_internal_prefix(&format!("http://{host}:{port}"))
    {
        return true;
    }
    attribute(span, DNS_ADDRESS_KEY).is_some_and(|address| {
        INTERNAL_DNS_ADDRESS_PREFIXES
            .iter()
            .any(|prefix| address.starts_with(prefix))
    })
}

/// The span attribute as Go reads it after converting the span to the V1 format, where a
/// `meta_struct` value replaces a metric, which replaces a `meta` value of the same key.
fn attribute<'a>(span: &'a pb::Span, key: &str) -> Option<Cow<'a, str>> {
    if let Some(bytes) = span.meta_struct.get(key) {
        return Some(String::from_utf8_lossy(bytes));
    }
    if let Some(metric) = span.metrics.get(key) {
        return Some(Cow::Owned(metric.to_string()));
    }
    span.meta
        .get(key)
        .map(|value| Cow::Borrowed(value.as_str()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Each case gives the span's `meta`, `metrics` and `meta_struct` entries, then whether the
    /// span is internal.
    #[test]
    fn test_is_internal_span() {
        type Case = (
            &'static [(&'static str, &'static str)],
            &'static [(&'static str, f64)],
            &'static [(&'static str, &'static str)],
            bool,
        );
        let cases: [Case; 14] = [
            (
                &[("http.url", "http://127.0.0.1:8124/v0.4/traces")],
                &[],
                &[],
                true,
            ),
            (&[("http.url", "http://127.0.0.1:8125/")], &[], &[], true),
            // Go compares prefixes, so a longer port matches too.
            (&[("http.url", "http://127.0.0.1:81240/")], &[], &[], true),
            (&[("http.url", "http://127.0.0.1:8126/")], &[], &[], false),
            (&[("http.url", "http://localhost:8124/")], &[], &[], false),
            (
                &[
                    ("tcp.remote.host", "127.0.0.1"),
                    ("tcp.remote.port", "8124"),
                ],
                &[],
                &[],
                true,
            ),
            (&[("tcp.remote.host", "127.0.0.1")], &[], &[], false),
            // The port as a metric is read as Go formats a float.
            (
                &[("tcp.remote.host", "127.0.0.1")],
                &[("tcp.remote.port", 8124.0)],
                &[],
                true,
            ),
            (
                &[("tcp.remote.host", "10.0.0.1"), ("tcp.remote.port", "8124")],
                &[],
                &[],
                false,
            ),
            (&[("dns.address", "127.0.0.1")], &[], &[], true),
            (&[("dns.address", "0.0.0.0")], &[], &[], true),
            (&[("dns.address", "10.0.0.1")], &[], &[], false),
            // A metric replaces a `meta` value of the same key.
            (
                &[("dns.address", "127.0.0.1")],
                &[("dns.address", 1.0)],
                &[],
                false,
            ),
            // A `meta_struct` value replaces both.
            (
                &[("dns.address", "10.0.0.1")],
                &[("dns.address", 1.0)],
                &[("dns.address", "0.0.0.0")],
                true,
            ),
        ];
        for (meta, metrics, meta_struct, expected) in cases {
            let span = pb::Span {
                meta: meta
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect(),
                metrics: metrics
                    .iter()
                    .map(|(key, value)| (key.to_string(), *value))
                    .collect(),
                meta_struct: meta_struct
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.as_bytes().to_vec()))
                    .collect::<HashMap<_, _>>(),
                ..Default::default()
            };
            assert_eq!(
                is_internal_span(&span),
                expected,
                "{meta:?} {metrics:?} {meta_struct:?}"
            );
        }
    }
}
