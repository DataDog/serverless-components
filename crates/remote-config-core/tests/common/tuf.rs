// Minimal TUF signing helpers shared across tests.
//
// These fixtures produce signed metadata and target payloads so integration
// tests can emulate backend-issued Uptane repositories without reproducing the
// signing logic inline.
use std::collections::HashMap;
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use data_encoding::BASE64;
use remote_config_proto::remoteconfig::{DelegatedMeta, TopMeta};
use serde_json::{json, Value};
use tuf::crypto::{Ed25519PrivateKey, HashAlgorithm, PrivateKey};
use tuf::metadata::{
    Delegation, DelegationsBuilder, Metadata, MetadataDescription, MetadataPath,
    RootMetadataBuilder, SignedMetadata, SnapshotMetadataBuilder, TargetDescription, TargetPath,
    TargetsMetadataBuilder, TimestampMetadataBuilder,
};
use tuf::pouf::Pouf1;

/// Simplified target descriptor used by the signing helpers.
pub(crate) struct TargetData<'a> {
    pub path: &'a str,
    pub payload: &'a [u8],
    pub custom: Option<Value>,
}

/// Describes a delegated role plus the targets it manages.
pub(crate) struct DelegationFixture<'a> {
    pub role: &'a str,
    pub version: u32,
    pub paths: &'a [&'a str],
    pub targets: &'a [TargetData<'a>],
}

const TEST_KEY_B64: &str =
    "MFMCAQEwBQYDK2VwBCIEIIq7jzyNQN9NFmyKh0xOgJoOPtdo3ZaCLUoJyzj8SEmgoSMDIQDrisJrXJ7wJ5474+giYqk7zhb+WO5CJQDTjK9GHGWjtg==";

/// Lazily decodes the embedded Ed25519 keypair used by the test fixtures.
fn key_material() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| {
        BASE64
            .decode(TEST_KEY_B64.as_bytes())
            .expect("embedded TUF key")
    })
}

/// Loads the signing key from the cached binary material.
fn load_private_key() -> Ed25519PrivateKey {
    Ed25519PrivateKey::from_pkcs8(key_material()).expect("embedded Ed25519 key should decode")
}

/// Builds a fully signed repository tuple (roots/timestamp/snapshot/targets).
pub(crate) fn build_signed_repo(
    version: u64,
    expires: &str,
    targets: &[TargetData<'_>],
    refresh_override: Option<i64>,
) -> (Vec<TopMeta>, TopMeta, TopMeta, TopMeta) {
    let version_u32 = u32::try_from(version).expect("version fits in u32");
    let expires_dt = parse_timestamp(expires);

    let (targets_meta, delegated_metas) =
        build_targets(version_u32, expires_dt, targets, refresh_override, &[]);
    let snapshot = build_snapshot(version_u32, expires_dt, &targets_meta, &delegated_metas);
    let timestamp = build_timestamp(version_u32, expires_dt, &snapshot);
    let roots = build_roots(version_u32, expires_dt);

    (
        roots,
        signed_to_top_meta(timestamp),
        signed_to_top_meta(snapshot),
        signed_to_top_meta(targets_meta),
    )
}

/// Builds a signed repository that also returns delegated metadata blobs.
#[allow(dead_code)]
/// Builds a repository snapshot plus delegated metadata for tests that need delegations.
pub(crate) fn build_signed_repo_with_delegations(
    version: u64,
    expires: &str,
    targets: &[TargetData<'_>],
    delegations: &[DelegationFixture<'_>],
) -> (Vec<TopMeta>, TopMeta, TopMeta, TopMeta, Vec<DelegatedMeta>) {
    let version_u32 = u32::try_from(version).expect("version fits in u32");
    let expires_dt = parse_timestamp(expires);

    let (targets_meta, delegated_metas) =
        build_targets(version_u32, expires_dt, targets, None, delegations);
    let snapshot = build_snapshot(version_u32, expires_dt, &targets_meta, &delegated_metas);
    let timestamp = build_timestamp(version_u32, expires_dt, &snapshot);
    let roots = build_roots(version_u32, expires_dt);

    (
        roots,
        signed_to_top_meta(timestamp),
        signed_to_top_meta(snapshot),
        signed_to_top_meta(targets_meta),
        delegated_metas,
    )
}

/// Creates a signed targets metadata tuple plus any delegated metas requested by the caller.
fn build_targets(
    version: u32,
    expires: DateTime<Utc>,
    targets: &[TargetData<'_>],
    refresh_override: Option<i64>,
    delegations: &[DelegationFixture<'_>],
) -> (
    SignedMetadata<Pouf1, tuf::metadata::TargetsMetadata>,
    Vec<DelegatedMeta>,
) {
    let mut builder = TargetsMetadataBuilder::new()
        .version(version)
        .expires(expires);
    for target in targets {
        let path = TargetPath::new(target.path).expect("valid target path");
        let description = if let Some(custom) = target.custom.clone() {
            // Targets with custom metadata need their JSON converted back into a map before signing.
            let custom_map = custom_value_to_map(custom);
            TargetDescription::from_slice_with_custom(
                target.payload,
                &[HashAlgorithm::Sha256],
                custom_map,
            )
            .expect("hashes compute")
        } else {
            // Plain targets only carry the binary payload plus standard hash fields.
            TargetDescription::from_slice(target.payload, &[HashAlgorithm::Sha256])
                .expect("hashes compute")
        };
        builder = builder.insert_target_description(path, description);
    }
    let mut delegated_metas = Vec::new();
    if !delegations.is_empty() {
        // Delegations attach additional roles signed by a dedicated key.
        let delegated_key = load_private_key();
        let mut delegations_builder = DelegationsBuilder::new().key(delegated_key.public().clone());
        for fixture in delegations {
            let mut role_builder = TargetsMetadataBuilder::new()
                .version(fixture.version)
                .expires(expires);
            for target in fixture.targets {
                let path = TargetPath::new(target.path).expect("valid delegated target path");
                let description = if let Some(custom) = target.custom.clone() {
                    let custom_map = custom_value_to_map(custom);
                    TargetDescription::from_slice_with_custom(
                        target.payload,
                        &[HashAlgorithm::Sha256],
                        custom_map,
                    )
                    .expect("delegated target hashes compute")
                } else {
                    TargetDescription::from_slice(target.payload, &[HashAlgorithm::Sha256])
                        .expect("delegated target hashes compute")
                };
                role_builder = role_builder.insert_target_description(path, description);
            }
            let metadata = role_builder
                .build()
                .expect("delegated targets metadata builds");
            let signed_role: SignedMetadata<Pouf1, tuf::metadata::TargetsMetadata> =
                SignedMetadata::new(&metadata, &delegated_key).expect("delegated signing");
            let raw = signed_role.to_raw().expect("delegated raw metadata");
            delegated_metas.push(DelegatedMeta {
                version: fixture.version as u64,
                role: fixture.role.to_string(),
                raw: raw.as_bytes().to_vec(),
            });
            let mut delegation = Delegation::builder(
                MetadataPath::new(fixture.role.to_string()).expect("delegation path"),
            )
            .key(delegated_key.public());
            for &path in fixture.paths {
                let target_path = TargetPath::new(path).expect("delegated path entry");
                delegation = delegation.delegate_path(target_path);
            }
            delegations_builder =
                delegations_builder.role(delegation.build().expect("delegation build"));
        }
        builder = builder.delegations(delegations_builder.build().expect("delegations build"));
    }

    let metadata = builder.build().expect("targets metadata builds");
    let additional_fields = refresh_override
        .map(|seconds| {
            HashMap::from([(
                "custom".to_string(),
                json!({ "agent_refresh_interval": seconds }),
            )])
        })
        .unwrap_or_default();
    let metadata = tuf::metadata::TargetsMetadata::new(
        metadata.version(),
        *metadata.expires(),
        metadata.targets().clone(),
        metadata.delegations().clone(),
        additional_fields,
    )
    .expect("targets metadata reconstruction");
    let signing_key = load_private_key();
    let signed = SignedMetadata::new(&metadata, &signing_key).expect("targets signing");

    (signed, delegated_metas)
}

/// Produces a snapshot metadata document referencing the provided targets and delegations.
fn build_snapshot(
    version: u32,
    expires: DateTime<Utc>,
    targets: &SignedMetadata<Pouf1, tuf::metadata::TargetsMetadata>,
    delegated: &[DelegatedMeta],
) -> SignedMetadata<Pouf1, tuf::metadata::SnapshotMetadata> {
    let mut builder = SnapshotMetadataBuilder::from_targets(targets, &[HashAlgorithm::Sha256])
        .expect("snapshot builder");
    for meta in delegated {
        let path = MetadataPath::new(meta.role.to_string()).expect("snapshot path");
        let version_u32 = u32::try_from(meta.version).expect("delegated version fits in u32");
        let description =
            MetadataDescription::from_slice(&meta.raw, version_u32, &[HashAlgorithm::Sha256])
                .expect("delegated metadata description");
        builder = builder.insert_metadata_description(path, description);
    }
    builder = builder.version(version).expires(expires);
    let metadata = builder.build().expect("snapshot metadata");
    let signing_key = load_private_key();
    SignedMetadata::new(&metadata, &signing_key).expect("snapshot signing")
}

/// Produces a timestamp metadata document pointing at the supplied snapshot.
fn build_timestamp(
    version: u32,
    expires: DateTime<Utc>,
    snapshot: &SignedMetadata<Pouf1, tuf::metadata::SnapshotMetadata>,
) -> SignedMetadata<Pouf1, tuf::metadata::TimestampMetadata> {
    let builder = TimestampMetadataBuilder::from_snapshot(snapshot, &[HashAlgorithm::Sha256])
        .expect("timestamp builder")
        .version(version)
        .expires(expires);
    let metadata = builder.build().expect("timestamp metadata");
    let signing_key = load_private_key();
    SignedMetadata::new(&metadata, &signing_key).expect("timestamp signing")
}

/// Builds a contiguous chain of root metadata entries up to the requested version.
fn build_roots(version: u32, expires: DateTime<Utc>) -> Vec<TopMeta> {
    (1..=version)
        .map(|v| {
            let root_key = load_private_key();
            let snapshot_key = load_private_key();
            let timestamp_key = load_private_key();
            let targets_key = load_private_key();
            let builder = RootMetadataBuilder::new()
                .expires(expires)
                .version(v)
                .consistent_snapshot(true)
                .root_threshold(1)
                .snapshot_threshold(1)
                .timestamp_threshold(1)
                .targets_threshold(1)
                .root_key(root_key.public().clone())
                .snapshot_key(snapshot_key.public().clone())
                .timestamp_key(timestamp_key.public().clone())
                .targets_key(targets_key.public().clone());
            let signed = builder.signed::<Pouf1>(&root_key).expect("root signing");
            signed_to_top_meta(signed)
        })
        .collect()
}

/// Converts a signed TUF metadata object into the `TopMeta` protobuf wrapper.
fn signed_to_top_meta<M>(signed: SignedMetadata<Pouf1, M>) -> TopMeta
where
    M: Metadata,
{
    let version = signed.assume_valid().expect("metadata decode").version() as u64;
    let raw = signed.to_raw().expect("raw metadata");
    TopMeta {
        version,
        raw: raw.as_bytes().to_vec(),
    }
}

/// Normalises arbitrary JSON values into a map for insertion into TUF metadata.
fn custom_value_to_map(value: Value) -> HashMap<String, Value> {
    match value {
        Value::Object(map) => map.into_iter().collect(),
        // Non-object values are wrapped so downstream code can still treat them as maps.
        other => HashMap::from([("value".to_string(), other)]),
    }
}

/// Parses an RFC3339 timestamp string into a UTC `DateTime`.
fn parse_timestamp(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("valid timestamp")
        .with_timezone(&Utc)
}
