// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use serde::Deserialize;
use std::collections::{HashMap, HashSet};

// The ordered list of concepts whose fallback keys form the peer tag set.
const PEER_TAG_CONCEPTS: &[&str] = &[
    "_dd.base_service",
    "peer.service",
    "peer.hostname",
    "peer.db.name",
    "peer.db.system",
    "peer.cassandra.contact.points",
    "peer.couchbase.seed.nodes",
    "peer.messaging.destination",
    "peer.messaging.system",
    "peer.kafka.bootstrap.servers",
    "peer.rpc.service",
    "peer.rpc.system",
    "peer.aws.s3.bucket",
    "peer.aws.sqs.queue",
    "peer.aws.dynamodb.table",
    "peer.aws.kinesis.stream",
];

#[derive(Deserialize)]
struct Mappings {
    concepts: HashMap<String, Concept>,
}

#[derive(Deserialize)]
struct Concept {
    fallbacks: Vec<Fallback>,
}

#[derive(Deserialize)]
struct Fallback {
    name: String,
}

/// Returns the sorted, deduplicated list of peer tag keys, derived from the embedded
/// peer_tags_mappings.json filtered to the concepts listed in `PEER_TAG_CONCEPTS`.
pub fn peer_tag_keys() -> Result<Vec<String>, serde_json::Error> {
    let mappings: Mappings = serde_json::from_str(include_str!("peer_tags_mappings.json"))?;

    let set: HashSet<String> = PEER_TAG_CONCEPTS
        .iter()
        .filter_map(|concept| mappings.concepts.get(*concept))
        .flat_map(|c| c.fallbacks.iter().map(|f| f.name.clone()))
        .collect();
    let mut keys: Vec<String> = set.into_iter().collect();
    keys.sort();
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_tag_keys_sorted_and_deduped() {
        let keys = peer_tag_keys().unwrap();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "peer tag keys should be sorted");
        let unique: HashSet<_> = keys.iter().collect();
        assert_eq!(
            keys.len(),
            unique.len(),
            "peer tag keys should be deduplicated"
        );
    }

    #[test]
    fn test_peer_tag_concepts_all_present_in_json() {
        let mappings: Mappings =
            serde_json::from_str(include_str!("peer_tags_mappings.json")).unwrap();
        let missing: Vec<&&str> = PEER_TAG_CONCEPTS
            .iter()
            .filter(|c| !mappings.concepts.contains_key(**c))
            .collect();
        assert!(
            missing.is_empty(),
            "PEER_TAG_CONCEPTS entries missing from peer_tags_mappings.json: {missing:?}"
        );
    }

    #[test]
    fn test_peer_tag_keys_exact_set() {
        let keys = peer_tag_keys().unwrap();
        let expected: Vec<String> = {
            let mut v: Vec<String> = vec![
                "_dd.base_service",
                "active_record.db.vendor",
                "amqp.destination",
                "amqp.exchange",
                "amqp.queue",
                "aws.queue.name",
                "aws.s3.bucket",
                "bucketname",
                "cassandra.keyspace",
                "db.cassandra.contact.points",
                "db.couchbase.seed.nodes",
                "db.hostname",
                "db.instance",
                "db.name",
                "db.namespace",
                "db.system",
                "db.type",
                "dns.hostname",
                "grpc.host",
                "hostname",
                "http.host",
                "http.server_name",
                "messaging.destination",
                "messaging.destination.name",
                "messaging.kafka.bootstrap.servers",
                "messaging.rabbitmq.exchange",
                "messaging.system",
                "mongodb.db",
                "msmq.queue.path",
                "net.peer.name",
                "network.destination.ip",
                "network.destination.name",
                "out.host",
                "peer.hostname",
                "peer.service",
                "queuename",
                "rpc.service",
                "rpc.system",
                "sequel.db.vendor",
                "server.address",
                "streamname",
                "tablename",
                "topicname",
            ]
            .into_iter()
            .map(String::from)
            .collect();
            v.sort();
            v
        };
        assert_eq!(keys, expected);
    }
}
