// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! Span pointers for linking async and decoupled trace operations.
//!
//! Span pointers provide a mechanism to link spans that don't have traditional parent-child
//! relationships. This is particularly useful for:
//! - **Async messaging**: Linking producer and consumer spans in message queues
//! - **Stream processing**: Connecting spans across AWS S3 or DynamoDB streams
//! - **Event-driven architectures**: Linking event publishers and subscribers
//! - **Batch processing**: Connecting batch job spans to individual item spans
//!
//! # How Span Pointers Work
//!
//! 1. **Hash Generation**: A deterministic hash is generated from operation-specific components
//!    (e.g., S3 bucket + key, DynamoDB stream ARN)
//! 2. **Bidirectional Linking**: Both "upstream" (producer) and "downstream" (consumer) spans
//!    include the same hash in their metadata
//! 3. **Backend Matching**: The Datadog backend uses these hashes to link spans together
//!
//! # Span Pointer Direction
//!
//! - **Upstream (`ptr.dir=u`)**: Points from producer to future consumer (producer doesn't know consumer yet)
//! - **Downstream (`ptr.dir=d`)**: Points from consumer back to producer (consumer knows about producer)
//!
//! # Supported Services
//!
//! Span pointers are currently used for:
//! - **AWS S3**: Link S3 upload spans to S3-triggered Lambda execution spans
//! - **AWS DynamoDB Streams**: Link DynamoDB write spans to stream consumer spans
//!
//! # References
//!
//! See [Datadog Span Pointer Rules](https://github.com/DataDog/dd-span-pointer-rules/blob/main/README.md)
//! for detailed hashing rules and supported scenarios.

use datadog_trace_protobuf::pb::SpanLink;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Length of span pointer hash (first 32 characters of SHA-256 hex string).
const SPAN_POINTER_HASH_LENGTH: usize = 32;

/// A span pointer linking two spans that don't have a parent-child relationship.
///
/// Span pointers use deterministic hashes to link asynchronous operations, such as
/// message queue producers/consumers or S3 uploads/downloads.
///
/// # Fields
///
/// - `hash`: Deterministic SHA-256 hash (truncated to 32 chars) identifying the linked operation
/// - `kind`: Type of span pointer (e.g., "aws.s3", "aws.dynamodb.stream")
#[derive(Clone)]
pub struct SpanPointer {
    /// Deterministic hash identifying the linked operation.
    ///
    /// Generated using `generate_span_pointer_hash()` from operation-specific components.
    /// Both upstream and downstream spans use the same hash for matching.
    pub hash: String,
    /// Type/category of the span pointer.
    ///
    /// Common values:
    /// - `"aws.s3"`: AWS S3 operations
    /// - `"aws.dynamodb.stream"`: DynamoDB stream operations
    /// - `"aws.sqs"`: SQS message queue operations
    pub kind: String,
}

/// Generates a deterministic hash for span pointer matching.
///
/// Returns the first 32 characters of the SHA-256 hash of the components joined by a '|'.
/// Used by span pointers to uniquely & deterministically identify an `S3` or `DynamoDB` stream.
///
/// # Hashing Algorithm
///
/// 1. Join components with `|` separator
/// 2. Compute SHA-256 hash of the joined string
/// 3. Take first 32 characters of hex-encoded hash
///
/// # Arguments
///
/// * `components` - Array of string components to hash (e.g., `["bucket-name", "object-key", "etag"]`)
///
/// # Returns
///
/// 32-character hex string (first half of SHA-256 hash)
///
/// # Example
///
/// ```
/// use datadog_agent_native::traces::span_pointers::generate_span_pointer_hash;
///
/// let hash = generate_span_pointer_hash(&["my-bucket", "file.txt", "abc123"]);
/// assert_eq!(hash.len(), 32);
/// ```
///
/// # References
///
/// See [General Hashing Rules](https://github.com/DataDog/dd-span-pointer-rules/blob/main/README.md#General%20Hashing%20Rules)
#[must_use]
pub fn generate_span_pointer_hash(components: &[&str]) -> String {
    let mut hasher = Sha256::new();
    // Join components with pipe separator as per Datadog span pointer rules
    hasher.update(components.join("|").as_bytes());
    let result = hasher.finalize();
    // Take first 32 characters of hex-encoded hash
    hex::encode(result)[..SPAN_POINTER_HASH_LENGTH].to_string()
}

/// Attaches span pointers to a span's metadata as span links.
///
/// Converts span pointers into the `_dd.span_links` metadata field, which is serialized
/// as JSON and attached to the span. If the span already has span links, the new pointers
/// are appended to the existing list.
///
/// # Span Link Format
///
/// Each span pointer is converted to a span link with:
/// - `link.kind`: `"span-pointer"` (identifies this as a span pointer link)
/// - `ptr.dir`: `"u"` (upstream direction - producer to consumer)
/// - `ptr.hash`: The span pointer hash
/// - `ptr.kind`: The span pointer kind (e.g., "aws.s3")
///
/// # Arguments
///
/// * `meta` - Span metadata map to attach span links to
/// * `span_pointers` - Optional list of span pointers to attach
///
/// # Example
///
/// ```
/// use std::collections::HashMap;
/// use datadog_agent_native::traces::span_pointers::{SpanPointer, attach_span_pointers_to_meta};
///
/// let mut meta = HashMap::new();
/// let pointers = Some(vec![
///     SpanPointer {
///         hash: "abc123".to_string(),
///         kind: "aws.s3".to_string(),
///     }
/// ]);
///
/// attach_span_pointers_to_meta(&mut meta, &pointers);
/// assert!(meta.contains_key("_dd.span_links"));
/// ```
pub fn attach_span_pointers_to_meta<S: ::std::hash::BuildHasher>(
    meta: &mut HashMap<String, String, S>,
    span_pointers: &Option<Vec<SpanPointer>>,
) {
    // Early return if no span pointers or empty list
    let Some(span_pointers) = span_pointers.as_ref().filter(|sp| !sp.is_empty()) else {
        return;
    };

    // Convert span pointers to span link format
    let new_span_links: Vec<SpanLink> = span_pointers
        .iter()
        .map(|sp| {
            SpanLink {
                // We set all these fields as 0 or empty since they're unknown; the frontend
                // uses `ptr.hash` instead to find the opposite link if it exists.
                // The hash-based matching works because both upstream and downstream spans
                // include the same deterministic hash.
                trace_id: 0,
                span_id: 0,
                trace_id_high: 0,
                tracestate: String::new(),
                flags: 0,
                attributes: HashMap::from([
                    ("link.kind".to_string(), "span-pointer".to_string()),
                    // "u" = upstream direction (producer pointing to future consumer)
                    ("ptr.dir".to_string(), "u".to_string()),
                    ("ptr.hash".to_string(), sp.hash.clone()),
                    ("ptr.kind".to_string(), sp.kind.clone()),
                ]),
            }
        })
        .collect();

    // Merge with existing span links if present
    let mut all_span_links = meta
        .get("_dd.span_links")
        .and_then(|existing| serde_json::from_str::<Vec<SpanLink>>(existing).ok())
        .unwrap_or_default();

    // Append new span pointers to existing span links
    all_span_links.extend(new_span_links);

    // Serialize and store back in metadata
    let _ = serde_json::to_string(&all_span_links)
        .map(|json| meta.insert("_dd.span_links".to_string(), json));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[derive(Debug, Default)]
    struct TestSpan {
        pub meta: HashMap<String, String>,
    }

    struct SpanPointerTestCase {
        test_name: &'static str,
        existing_links: Option<serde_json::Value>,
        span_pointers: Option<Vec<SpanPointer>>,
        expected_links: Option<serde_json::Value>,
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_attach_span_pointers_to_span() {
        let test_cases = vec![
            SpanPointerTestCase {
                test_name: "adds span links to span",
                existing_links: None,
                span_pointers: Some(vec![
                    SpanPointer {
                        hash: "hash1".to_string(),
                        kind: "test.kind1".to_string(),
                    },
                    SpanPointer {
                        hash: "hash2".to_string(),
                        kind: "test.kind2".to_string(),
                    },
                ]),
                expected_links: Some(json!([
                    {
                        "attributes": {
                            "link.kind": "span-pointer",
                            "ptr.dir": "u",
                            "ptr.hash": "hash1",
                            "ptr.kind": "test.kind1"
                        },
                        "span_id": 0,
                        "trace_id": 0,
                        "trace_id_high": 0,
                        "tracestate": "",
                        "flags": 0
                    },
                    {
                        "attributes": {
                            "link.kind": "span-pointer",
                            "ptr.dir": "u",
                            "ptr.hash": "hash2",
                            "ptr.kind": "test.kind2"
                        },
                        "span_id": 0,
                        "trace_id": 0,
                        "trace_id_high": 0,
                        "tracestate": "",
                        "flags": 0
                    }
                ])),
            },
            SpanPointerTestCase {
                test_name: "handles empty span pointers",
                existing_links: None,
                span_pointers: Some(vec![]),
                expected_links: None,
            },
            SpanPointerTestCase {
                test_name: "handles None span pointers",
                existing_links: None,
                span_pointers: None,
                expected_links: None,
            },
            SpanPointerTestCase {
                test_name: "appends to existing span links",
                existing_links: Some(json!([{
                    "attributes": {
                        "link.kind": "span-pointer",
                        "ptr.dir": "d",
                        "ptr.hash": "hash1",
                        "ptr.kind": "test.kind1"
                    },
                    "span_id": 0,
                    "trace_id": 0,
                    "trace_id_high": 0,
                    "tracestate": "",
                    "flags": 0
                }])),
                span_pointers: Some(vec![SpanPointer {
                    hash: "hash2".to_string(),
                    kind: "test.kind2".to_string(),
                }]),
                expected_links: Some(json!([
                    {
                        "attributes": {
                            "link.kind": "span-pointer",
                            "ptr.dir": "d",
                            "ptr.hash": "hash1",
                            "ptr.kind": "test.kind1"
                        },
                        "span_id": 0,
                        "trace_id": 0,
                        "trace_id_high": 0,
                        "tracestate": "",
                        "flags": 0
                    },
                    {
                        "attributes": {
                            "link.kind": "span-pointer",
                            "ptr.dir": "u",
                            "ptr.hash": "hash2",
                            "ptr.kind": "test.kind2"
                        },
                        "span_id": 0,
                        "trace_id": 0,
                        "trace_id_high": 0,
                        "tracestate": "",
                        "flags": 0
                    }
                ])),
            },
        ];

        for case in test_cases {
            let mut test_span = TestSpan {
                meta: HashMap::new(),
            };

            // Set up existing links if any
            if let Some(links) = case.existing_links {
                test_span
                    .meta
                    .insert("_dd.span_links".to_string(), links.to_string());
            }

            attach_span_pointers_to_meta(&mut test_span.meta, &case.span_pointers);

            match case.expected_links {
                Some(expected) => {
                    let span_links = test_span.meta.get("_dd.span_links").unwrap_or_else(|| {
                        panic!(
                            "[{}] _dd.span_links should be present in span meta",
                            case.test_name
                        )
                    });
                    let actual_links: serde_json::Value =
                        serde_json::from_str(span_links).expect("Should be valid JSON");
                    assert_eq!(
                        actual_links, expected,
                        "Failed test case: {}",
                        case.test_name
                    );
                }
                None => {
                    assert!(
                        !test_span.meta.contains_key("_dd.span_links"),
                        "Failed test case: {}",
                        case.test_name
                    );
                }
            }
        }
    }

    #[test]
    fn test_generate_span_pointer_hash() {
        let test_cases = vec![
            (
                "basic values",
                vec!["some-bucket", "some-key.data", "ab12ef34"],
                "e721375466d4116ab551213fdea08413",
            ),
            (
                "non-ascii key",
                vec!["some-bucket", "some-key.你好", "ab12ef34"],
                "d1333a04b9928ab462b5c6cadfa401f4",
            ),
            (
                "multipart-upload",
                vec!["some-bucket", "some-key.data", "ab12ef34-5"],
                "2b90dffc37ebc7bc610152c3dc72af9f",
            ),
        ];

        for (name, components, expected_hash) in test_cases {
            let actual_hash = generate_span_pointer_hash(&components);
            assert_eq!(actual_hash, expected_hash, "Test case: {name}");
        }
    }
}
