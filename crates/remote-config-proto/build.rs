use std::error::Error;
use std::path::Path;

fn main() -> Result<(), Box<dyn Error>> {
    let proto_root = Path::new("proto");
    let proto_path = proto_root.join("datadog/remoteconfig/remoteconfig.proto");

    println!("cargo:rerun-if-changed={}", proto_path.display());

    let mut config = prost_build::Config::new();

    config.type_attribute(".", "#[derive(::serde::Serialize, ::serde::Deserialize)]");

    for message in [
        "ConfigMetas",
        "DirectorMetas",
        "DelegatedMeta",
        "TopMeta",
        "File",
        "LatestConfigsRequest",
        "LatestConfigsResponse",
        "OrgDataResponse",
        "OrgStatusResponse",
        "Client",
        "ClientTracer",
        "ClientAgent",
        "ClientUpdater",
        "PackageState",
        "PackageStateTask",
        "TaskError",
        "ConfigState",
        "ClientState",
        "TargetFileHash",
        "TargetFileMeta",
        "ClientGetConfigsRequest",
        "ClientGetConfigsResponse",
        "FileMetaState",
        "GetStateConfigResponse",
        "ResetStateConfigResponse",
        "TracerPredicateV1",
        "TracerPredicates",
    ] {
        config.type_attribute(format!(".datadog.config.{}", message), "#[serde(default)]");
    }

    // Annotate required binary blobs so JSON encoding uses base64 strings.
    for field in [
        ".datadog.config.DelegatedMeta.raw",
        ".datadog.config.TopMeta.raw",
        ".datadog.config.File.raw",
        ".datadog.config.LatestConfigsRequest.backend_client_state",
        ".datadog.config.Client.capabilities",
        ".datadog.config.LatestConfigsResponse.config_metas.roots.raw",
        ".datadog.config.LatestConfigsResponse.director_metas.roots.raw",
    ] {
        config.field_attribute(field, "#[serde(with = \"crate::serde_base64\")]");
    }

    // Annotate optional binary blobs
    let backend_state_field = ".datadog.config.ClientState.backend_client_state";
    config.field_attribute(
        backend_state_field,
        "#[serde(with = \"crate::serde_base64_option\")]",
    );

    // ClientGetConfigsResponse.targets uses base64 encoding
    config.field_attribute(
        ".datadog.config.ClientGetConfigsResponse.targets",
        "#[serde(with = \"crate::serde_base64\", skip_serializing_if = \"crate::skip_empty_bytes::is_empty\")]",
    );

    // Repeated bytes (Vec<Vec<u8>>) require custom serde helpers.
    config.field_attribute(
        ".datadog.config.ClientGetConfigsResponse.roots",
        "#[serde(with = \"crate::serde_bytes_vec\", skip_serializing_if = \"crate::skip_vec::is_empty\")]",
    );

    config.field_attribute(
        ".datadog.config.ClientGetConfigsResponse.config_status",
        "#[serde(skip_serializing_if = \"crate::skip_zero_i32::is_zero\")]",
    );

    config.field_attribute(
        ".datadog.config.ClientGetConfigsResponse.target_files",
        "#[serde(default, skip_serializing_if = \"crate::skip_vec::is_empty\")]",
    );

    config.field_attribute(
        ".datadog.config.ClientGetConfigsResponse.client_configs",
        "#[serde(default, skip_serializing_if = \"crate::skip_vec::is_empty\")]",
    );

    config.field_attribute(
        ".datadog.config.LatestConfigsResponse.target_files",
        "#[serde(default)]",
    );

    // Add field-level serde deserializer for ClientTracer fields that can be null
    // Using custom null_as_default module to treat explicit null as default value
    for field in [
        ".datadog.config.ClientTracer.extra_services",
        ".datadog.config.ClientTracer.tags",
        ".datadog.config.ClientTracer.process_tags",
        ".datadog.config.ClientTracer.container_tags",
        ".datadog.config.ClientTracer.runtime_id",
        ".datadog.config.ClientTracer.language",
        ".datadog.config.ClientTracer.tracer_version",
        ".datadog.config.ClientTracer.service",
        ".datadog.config.ClientTracer.env",
        ".datadog.config.ClientTracer.app_version",
    ] {
        config.field_attribute(field, "#[serde(deserialize_with = \"crate::null_as_default::deserialize\")]");
    }

    // Add field-level serde deserializer for Client fields that can be null
    for field in [
        ".datadog.config.Client.id",
        ".datadog.config.Client.products",
    ] {
        config.field_attribute(field, "#[serde(deserialize_with = \"crate::null_as_default::deserialize\")]");
    }

    // Add field-level serde deserializer for LatestConfigsRequest fields that can be null
    for field in [
        ".datadog.config.LatestConfigsRequest.hostname",
        ".datadog.config.LatestConfigsRequest.agentVersion",
        ".datadog.config.LatestConfigsRequest.products",
        ".datadog.config.LatestConfigsRequest.new_products",
        ".datadog.config.LatestConfigsRequest.active_clients",
        ".datadog.config.LatestConfigsRequest.error",
        ".datadog.config.LatestConfigsRequest.trace_agent_env",
        ".datadog.config.LatestConfigsRequest.org_uuid",
        ".datadog.config.LatestConfigsRequest.tags",
        ".datadog.config.LatestConfigsRequest.agent_uuid",
    ] {
        config.field_attribute(field, "#[serde(deserialize_with = \"crate::null_as_default::deserialize\")]");
    }

    // Add field-level serde deserializer for ConfigState fields that can be null
    let apply_error_field = ".datadog.config.ConfigState.apply_error";
    config.field_attribute(
        apply_error_field,
        "#[serde(deserialize_with = \"crate::null_as_default::deserialize\")]",
    );

    config.compile_protos(&[proto_path], &[proto_root])?;

    Ok(())
}
