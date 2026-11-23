// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! The platform that serverless-init runs on, and the tags that traces and DogStatsD metrics
//! get, as Go serverless-init builds them once at start.

use std::collections::BTreeMap;

use dogstatsd::origin::{OriginCategory, OriginService};
use tracing::warn;

/// The serverless-init version: `DD_SERVERLESS_INIT_VERSION` at build time (the image version
/// that serverless-init-ci passes), or the crate version.
pub const VERSION: &str = match option_env!("DD_SERVERLESS_INIT_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

const MICROVM_VARIABLE: &str = "AWS_LAMBDA_MICROVM_IMAGE_ARN";
const CLOUD_RUN_SERVICE_VARIABLE: &str = "K_SERVICE";
const CLOUD_RUN_FUNCTION_VARIABLE: &str = "FUNCTION_TARGET";
const CLOUD_RUN_JOB_VARIABLE: &str = "CLOUD_RUN_JOB";
const CONTAINER_APP_VARIABLE: &str = "CONTAINER_APP_NAME";
const APP_SERVICE_VARIABLE: &str = "WEBSITE_STACK";

const INSTANCE_TAG: &str = "instance";
const CONTAINER_APP_ORIGIN: &str = "containerapp";
const CLOUD_RUN_ORIGIN: &str = "cloudrun";
const CLOUD_RUN_SERVICE_TAG_PREFIX: &str = "gcr.";

/// The tags that Go serverless-init leaves out of DogStatsD metrics for their cardinality
/// (`highCardinalityTags` in `cmd/serverless-init/tag/tag.go`).
const HIGH_CARDINALITY_TAGS: &[&str] = &[
    "container_id",
    "gcr.container_id",
    "gcrfx.container_id",
    "replica_name",
    "aca.replica.name",
    "gcrj.execution_name",
    "gcrj.container_id",
    "gcrj.task_index",
    "gcrj.task_attempt",
    "gcrj.task_count",
    INSTANCE_TAG,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Go serverless-init detects no cloud platform.
    Local,
    AzureContainerApp,
    GoogleCloudRunService,
    /// A cloud platform, whose tags and origin are not supported yet.
    Unsupported,
}

/// The detected platform and its tags, as Go's `CloudService` gives them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub kind: Kind,
    tags: BTreeMap<String, String>,
}

impl Platform {
    /// Detects the platform in Go's order from the variables that `lookup` finds, even set to an
    /// empty value, and builds its tags from the environment. `hostname` is read only on the
    /// local platform; an empty one gives `instance:unknown`. On Google Cloud Run the tags from
    /// the metadata server come with `with_gcp_metadata`.
    pub fn detect(
        lookup: impl Fn(&str) -> Option<String>,
        hostname: impl FnOnce() -> String,
    ) -> Self {
        let is_set = |name: &str| lookup(name).is_some();
        let (kind, tags) = if is_set(MICROVM_VARIABLE) {
            (Kind::Unsupported, BTreeMap::new())
        } else if is_set(CLOUD_RUN_SERVICE_VARIABLE) {
            if is_set(CLOUD_RUN_FUNCTION_VARIABLE) {
                (Kind::Unsupported, BTreeMap::new())
            } else {
                (Kind::GoogleCloudRunService, cloud_run_service_tags(&lookup))
            }
        } else if is_set(CLOUD_RUN_JOB_VARIABLE) {
            (Kind::Unsupported, BTreeMap::new())
        } else if is_set(CONTAINER_APP_VARIABLE) {
            (Kind::AzureContainerApp, container_app_tags(&lookup))
        } else if is_set(APP_SERVICE_VARIABLE) {
            (Kind::Unsupported, BTreeMap::new())
        } else {
            let tags = [
                (INSTANCE_TAG, value_or_unknown(&hostname()).to_string()),
                ("local", "true".to_string()),
            ];
            (Kind::Local, owned(tags))
        };
        Self { kind, tags }
    }

    /// Adds the Google Cloud metadata tags of Go's `CloudRun.GetTags`: the non-empty instance
    /// ID, region and project, each also under the `gcr.` prefix, and the service's resource
    /// name when the project, the region and the service are known.
    pub fn with_gcp_metadata(mut self, metadata: &GcpMetadata) -> Self {
        for (key, value) in [
            ("container_id", &metadata.instance_id),
            ("location", &metadata.region),
            ("project_id", &metadata.project_id),
        ] {
            if !value.is_empty() {
                self.tags.insert(key.to_string(), value.clone());
                self.tags.insert(
                    format!("{CLOUD_RUN_SERVICE_TAG_PREFIX}{key}"),
                    value.clone(),
                );
            }
        }
        let service = self.tags.get("service_name").cloned().unwrap_or_default();
        if !metadata.project_id.is_empty() && !metadata.region.is_empty() && !service.is_empty() {
            self.tags.insert(
                format!("{CLOUD_RUN_SERVICE_TAG_PREFIX}resource_name"),
                format!(
                    "projects/{}/locations/{}/services/{service}",
                    metadata.project_id, metadata.region
                ),
            );
        }
        self
    }

    /// The origin of chunks and spans, and the default source of logs (Go's `GetOrigin` and
    /// `GetDefaultLogsSource`). Empty when it is not known.
    pub fn origin(&self) -> &'static str {
        match self.kind {
            Kind::Local => "local",
            Kind::AzureContainerApp => CONTAINER_APP_ORIGIN,
            Kind::GoogleCloudRunService => CLOUD_RUN_ORIGIN,
            Kind::Unsupported => "",
        }
    }

    /// The source of logs when `DD_SOURCE` is not set.
    pub fn default_logs_source(&self) -> &'static str {
        match self.kind {
            Kind::Local | Kind::Unsupported => "unknown",
            Kind::AzureContainerApp => CONTAINER_APP_ORIGIN,
            Kind::GoogleCloudRunService => CLOUD_RUN_ORIGIN,
        }
    }

    /// The names and tags of the enhanced metrics. `None` on a platform that is not supported
    /// yet.
    pub fn enhanced_metrics(&self) -> Option<EnhancedMetricsProfile> {
        let tag = |key: &str| value_or_unknown(self.tags.get(key).map_or("", String::as_str));
        let (prefix, legacy_prefix, base, usage) = match self.kind {
            Kind::Local => (
                "datadog.serverless_agent.",
                None,
                vec![("local", tag("local"))],
                (INSTANCE_TAG, tag(INSTANCE_TAG)),
            ),
            Kind::AzureContainerApp => (
                "azure.app_containerapps.",
                Some("azure.containerapp."),
                vec![
                    ("name", tag("app_name")),
                    ("origin", tag("origin")),
                    ("region", tag("region")),
                    ("resource_group", tag("resource_group")),
                    ("revisionname", tag("revision")),
                    ("subscription_id", tag("subscription_id")),
                ],
                ("replica", tag("replica_name")),
            ),
            Kind::GoogleCloudRunService => (
                "gcp.run.container.",
                Some("gcp.run."),
                vec![
                    ("location", tag("location")),
                    ("origin", tag("origin")),
                    ("project_id", tag("project_id")),
                    ("revision_name", tag("revision_name")),
                    ("service_name", tag("service_name")),
                ],
                (INSTANCE_TAG, tag("container_id")),
            ),
            Kind::Unsupported => return None,
        };
        let base_tags = owned(base);
        let mut usage_tags = base_tags.clone();
        usage_tags.insert(usage.0.to_string(), usage.1.to_string());
        Some(EnhancedMetricsProfile {
            prefix,
            legacy_prefix,
            usage_metric_suffix: usage.0,
            base_tags,
            usage_tags,
        })
    }

    /// Go's `ContainerApp.Init`, which runs only with an API key: on Azure Container Apps
    /// `DD_AZURE_SUBSCRIPTION_ID` and `DD_AZURE_RESOURCE_GROUP` must be set, even to an empty
    /// value. Returns Go's fatal error message for the first one missing.
    pub fn missing_required_setting(
        &self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Option<&'static str> {
        if self.kind != Kind::AzureContainerApp || crate::settings::api_key(&lookup).is_none() {
            return None;
        }
        [
            (
                "DD_AZURE_SUBSCRIPTION_ID",
                "Must set Subscription ID as an environment variable. Please set the \
                 DD_AZURE_SUBSCRIPTION_ID value to your Subscription ID your App Container is in.",
            ),
            (
                "DD_AZURE_RESOURCE_GROUP",
                "Must set Resource Group as an environment variable. Please set the \
                 DD_AZURE_RESOURCE_GROUP value to your Resource Group your App Container is in.",
            ),
        ]
        .into_iter()
        .find(|(variable, _)| lookup(variable).is_none())
        .map(|(_, message)| message)
    }

    /// The origin of DogStatsD metrics, or of enhanced metrics when `enhanced`. On a cloud
    /// platform, DogStatsD metrics named `runtime.*` get the runtime service, as Go's
    /// `serverlessSourceCustomToRuntime` gives them; on local Go keeps its source.
    pub fn metric_origin(&self, enhanced: bool) -> MetricOrigin {
        let category = match self.kind {
            Kind::Local | Kind::Unsupported => {
                return MetricOrigin {
                    category: OriginCategory::Other,
                    service: OriginService::Other,
                    runtime_service: None,
                };
            }
            Kind::AzureContainerApp => OriginCategory::ContainerAppMetrics,
            Kind::GoogleCloudRunService => OriginCategory::CloudRunMetrics,
        };
        if enhanced {
            MetricOrigin {
                category,
                service: OriginService::ServerlessEnhanced,
                runtime_service: None,
            }
        } else {
            MetricOrigin {
                category,
                service: OriginService::ServerlessCustom,
                runtime_service: Some(OriginService::ServerlessRuntime),
            }
        }
    }
}

/// The origin category and service of metrics, and the service of metrics named `runtime.*`.
pub struct MetricOrigin {
    pub category: OriginCategory,
    pub service: OriginService,
    pub runtime_service: Option<OriginService>,
}

/// The platform's enhanced metric names and tags, from Go's `GetMetricPrefix`,
/// `GetUsageMetricSuffix` and `GetEnhancedMetricTags`. The cold start and shutdown metrics are
/// also sent under `legacy_prefix`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnhancedMetricsProfile {
    pub prefix: &'static str,
    pub legacy_prefix: Option<&'static str>,
    pub usage_metric_suffix: &'static str,
    pub base_tags: BTreeMap<String, String>,
    pub usage_tags: BTreeMap<String, String>,
}

/// The Azure Container Apps tags of Go's `ContainerApp.GetTags`. The four Azure variables are
/// tagged even when empty; the subscription, the resource group and the resource ID only when
/// known.
fn container_app_tags(lookup: impl Fn(&str) -> Option<String>) -> BTreeMap<String, String> {
    let value = |name: &str| lookup(name).unwrap_or_default();
    let app_name = value(CONTAINER_APP_VARIABLE);
    let region = container_app_region(&value("CONTAINER_APP_ENV_DNS_SUFFIX"));
    let revision = value("CONTAINER_APP_REVISION");
    let replica = value("CONTAINER_APP_REPLICA_NAME");
    let subscription_id = value("DD_AZURE_SUBSCRIPTION_ID");
    let resource_group = value("DD_AZURE_RESOURCE_GROUP");
    let mut tags = owned([
        ("app_name", app_name.clone()),
        ("region", region.clone()),
        ("revision", revision.clone()),
        ("replica_name", replica.clone()),
        ("aca.app.name", app_name.clone()),
        ("aca.app.region", region),
        ("aca.app.revision", revision),
        ("aca.replica.name", replica),
        ("origin", CONTAINER_APP_ORIGIN.to_string()),
        ("_dd.origin", CONTAINER_APP_ORIGIN.to_string()),
    ]);
    let resource_id = if subscription_id.is_empty()
        || resource_group.is_empty()
        || app_name.is_empty()
    {
        String::new()
    } else {
        format!(
            "/subscriptions/{subscription_id}/resourcegroups/{resource_group}/providers/microsoft.app/containerapps/{}",
            app_name.to_lowercase()
        )
    };
    for (keys, value) in [
        (["subscription_id", "aca.subscription.id"], subscription_id),
        (["resource_group", "aca.resource.group"], resource_group),
        (["resource_id", "aca.resource.id"], resource_id),
    ] {
        if !value.is_empty() {
            for key in keys {
                tags.insert(key.to_string(), value.clone());
            }
        }
    }
    tags
}

/// The Google Cloud Run service tags of Go's `CloudRun.GetTags` that come from the environment:
/// the origin, and the non-empty revision, service and configuration, each also under the `gcr.`
/// prefix.
fn cloud_run_service_tags(lookup: impl Fn(&str) -> Option<String>) -> BTreeMap<String, String> {
    let mut tags = owned([
        ("origin", CLOUD_RUN_ORIGIN),
        ("_dd.origin", CLOUD_RUN_ORIGIN),
    ]);
    for (key, variable) in [
        ("revision_name", "K_REVISION"),
        ("service_name", CLOUD_RUN_SERVICE_VARIABLE),
        ("configuration_name", "K_CONFIGURATION"),
    ] {
        if let Some(value) = lookup(variable).filter(|value| !value.is_empty()) {
            tags.insert(
                format!("{CLOUD_RUN_SERVICE_TAG_PREFIX}{key}"),
                value.clone(),
            );
            tags.insert(key.to_string(), value);
        }
    }
    tags
}

/// The answers of the Google Cloud metadata server, lowercased, each empty when its request
/// failed. `region` is the last part of `projects/<number>/regions/<region>`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcpMetadata {
    pub instance_id: String,
    pub region: String,
    pub project_id: String,
}

/// The third-from-last label of the environment's DNS suffix, or `unknown`.
fn container_app_region(dns_suffix: &str) -> String {
    let labels: Vec<&str> = dns_suffix.split('.').collect();
    let region = labels
        .len()
        .checked_sub(3)
        .map_or("", |index| labels[index]);
    value_or_unknown(region).to_string()
}

fn owned<'a>(
    tags: impl IntoIterator<Item = (&'a str, impl Into<String>)>,
) -> BTreeMap<String, String> {
    tags.into_iter()
        .map(|(key, value)| (key.to_string(), value.into()))
        .collect()
}

/// The tag that holds the version, without the `_dd.` prefix that traces and DogStatsD metrics
/// add.
pub fn version_tag_key(sidecar: bool) -> &'static str {
    if sidecar {
        "datadog_sidecar_version"
    } else {
        "datadog_init_version"
    }
}

/// The tags of spans, and with `dogstatsd_tags` of DogStatsD metrics. A later source replaces a
/// tag of an earlier one: the unified service tags, the configured tags, the platform's tags,
/// then the version.
pub fn telemetry_tags(
    platform: &Platform,
    sidecar: bool,
    version: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> BTreeMap<String, String> {
    let mut tags = unified_service_tags(&lookup);
    tags.extend(configured_tags(&lookup));
    tags.extend(platform.tags.clone());
    tags.insert(
        format!("_dd.{}", version_tag_key(sidecar)),
        version.to_string(),
    );
    tags
}

/// The tags that every DogStatsD metric gets: the whole tags of `DD_DOGSTATSD_TAGS`, separated
/// by whitespace, then the telemetry tags without the high-cardinality ones.
pub fn dogstatsd_tags(
    telemetry_tags: &BTreeMap<String, String>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut tags: Vec<String> = lookup("DD_DOGSTATSD_TAGS")
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    tags.extend(
        telemetry_tags
            .iter()
            .filter(|(key, _)| !HIGH_CARDINALITY_TAGS.contains(&key.as_str()))
            .map(|(key, value)| format!("{key}:{value}")),
    );
    tags
}

/// The tags from `DD_ENV`, `DD_SERVICE` and `DD_VERSION`, lowercased.
pub fn unified_service_tags(lookup: impl Fn(&str) -> Option<String>) -> BTreeMap<String, String> {
    [
        ("env", "DD_ENV"),
        ("service", "DD_SERVICE"),
        ("version", "DD_VERSION"),
    ]
    .into_iter()
    .filter_map(|(key, variable)| {
        lookup(variable)
            .filter(|value| !value.is_empty())
            .map(|value| (key.to_string(), value.to_lowercase()))
    })
    .collect()
}

/// The `key:value` tags of `DD_TAGS` then `DD_EXTRA_TAGS`, separated by whitespace or commas and
/// lowercased, as the Go agent reads them. A tag without a value is dropped.
pub fn configured_tags(lookup: impl Fn(&str) -> Option<String>) -> BTreeMap<String, String> {
    ["DD_TAGS", "DD_EXTRA_TAGS"]
        .into_iter()
        .filter_map(lookup)
        .flat_map(|list| {
            list.split(|c: char| c.is_whitespace() || c == ',')
                .filter(|tag| !tag.is_empty())
                .filter_map(|tag| match tag.split_once(':') {
                    Some((key, value)) => Some((key.to_lowercase(), value.to_lowercase())),
                    None => {
                        warn!("Tag {tag} has not expected format");
                        None
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn join_tags(tags: &BTreeMap<String, String>) -> String {
    tags.iter()
        .map(|(key, value)| format!("{key}:{value}"))
        .collect::<Vec<_>>()
        .join(",")
}

pub fn value_or_unknown(value: &str) -> &str {
    if value.is_empty() { "unknown" } else { value }
}

/// The host name, or an empty string when it cannot be read.
pub fn hostname() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: gethostname writes at most `buffer.len()` bytes into the buffer.
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        warn!(
            "Failed to get the hostname for the instance tag: {}",
            std::io::Error::last_os_error()
        );
        return String::new();
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    type Variables = &'static [(&'static str, &'static str)];

    fn lookup(variables: Variables) -> impl Fn(&str) -> Option<String> {
        move |name| {
            variables
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    fn local(hostname: &str) -> Platform {
        Platform::detect(|_| None, || hostname.to_string())
    }

    const CONTAINER_APP: Variables = &[
        ("CONTAINER_APP_NAME", "Parity-App"),
        ("CONTAINER_APP_REVISION", "parity-app--1"),
        ("CONTAINER_APP_REPLICA_NAME", "parity-app--1-abc"),
        (
            "CONTAINER_APP_ENV_DNS_SUFFIX",
            "env.eastus.azurecontainerapps.io",
        ),
        ("DD_AZURE_SUBSCRIPTION_ID", "sub-1"),
        ("DD_AZURE_RESOURCE_GROUP", "group-1"),
    ];

    #[test]
    fn detect_table() {
        let cases: &[(Variables, Kind)] = &[
            (&[], Kind::Local),
            (&[("DD_SERVICE", "a")], Kind::Local),
            (&[("AWS_LAMBDA_MICROVM_IMAGE_ARN", "")], Kind::Unsupported),
            (&[("K_SERVICE", "a")], Kind::GoogleCloudRunService),
            (
                &[("K_SERVICE", "a"), ("FUNCTION_TARGET", "a")],
                Kind::Unsupported,
            ),
            (&[("CLOUD_RUN_JOB", "a")], Kind::Unsupported),
            (&[("CONTAINER_APP_NAME", "")], Kind::AzureContainerApp),
            (&[("WEBSITE_STACK", "a")], Kind::Unsupported),
            // Go checks Cloud Run before Container Apps, and Container Apps before App Service.
            (
                &[("CONTAINER_APP_NAME", "a"), ("K_SERVICE", "a")],
                Kind::GoogleCloudRunService,
            ),
            (
                &[("K_SERVICE", "a"), ("AWS_LAMBDA_MICROVM_IMAGE_ARN", "a")],
                Kind::Unsupported,
            ),
            (
                &[("CONTAINER_APP_NAME", "a"), ("WEBSITE_STACK", "a")],
                Kind::AzureContainerApp,
            ),
        ];
        for (variables, expected) in cases {
            let platform = Platform::detect(lookup(variables), || "host-1".to_string());
            assert_eq!(platform.kind, *expected, "{variables:?}");
        }
    }

    #[test]
    fn platform_tags_table() {
        let cases: &[(Variables, &str)] = &[
            (&[], "instance:host-1,local:true"),
            (
                CONTAINER_APP,
                "_dd.origin:containerapp,aca.app.name:Parity-App,aca.app.region:eastus,\
                 aca.app.revision:parity-app--1,aca.replica.name:parity-app--1-abc,\
                 aca.resource.group:group-1,\
                 aca.resource.id:/subscriptions/sub-1/resourcegroups/group-1/providers/microsoft.app/containerapps/parity-app,\
                 aca.subscription.id:sub-1,app_name:Parity-App,origin:containerapp,region:eastus,\
                 replica_name:parity-app--1-abc,resource_group:group-1,\
                 resource_id:/subscriptions/sub-1/resourcegroups/group-1/providers/microsoft.app/containerapps/parity-app,\
                 revision:parity-app--1,subscription_id:sub-1",
            ),
            // Without the subscription there is no resource ID, and a DNS suffix of fewer than
            // three labels gives an unknown region.
            (
                &[
                    ("CONTAINER_APP_NAME", "app"),
                    ("CONTAINER_APP_ENV_DNS_SUFFIX", "eastus.io"),
                    ("DD_AZURE_RESOURCE_GROUP", "group-1"),
                ],
                "_dd.origin:containerapp,aca.app.name:app,aca.app.region:unknown,aca.app.revision:,\
                 aca.replica.name:,aca.resource.group:group-1,app_name:app,origin:containerapp,\
                 region:unknown,replica_name:,resource_group:group-1,revision:",
            ),
            // Without the metadata server's answers, Cloud Run has only the environment's tags.
            (
                &[
                    ("K_SERVICE", "svc"),
                    ("K_REVISION", ""),
                    ("K_CONFIGURATION", "config"),
                ],
                "_dd.origin:cloudrun,configuration_name:config,gcr.configuration_name:config,\
                 gcr.service_name:svc,origin:cloudrun,service_name:svc",
            ),
            (&[("CLOUD_RUN_JOB", "a")], ""),
        ];
        for (variables, expected) in cases {
            let platform = Platform::detect(lookup(variables), || "host-1".to_string());
            assert_eq!(join_tags(&platform.tags), *expected, "{variables:?}");
        }
    }

    #[test]
    fn enhanced_metrics_table() {
        /// The prefix, the legacy prefix, the usage metric suffix, the base tags and the usage tags.
        type Profile<'a> = (&'a str, Option<&'a str>, &'a str, &'a str, &'a str);
        let cases: &[(Variables, Option<Profile>)] = &[
            (
                &[],
                Some((
                    "datadog.serverless_agent.",
                    None,
                    "instance",
                    "local:true",
                    "instance:host-1,local:true",
                )),
            ),
            (
                CONTAINER_APP,
                Some((
                    "azure.app_containerapps.",
                    Some("azure.containerapp."),
                    "replica",
                    "name:Parity-App,origin:containerapp,region:eastus,resource_group:group-1,\
                     revisionname:parity-app--1,subscription_id:sub-1",
                    "name:Parity-App,origin:containerapp,region:eastus,replica:parity-app--1-abc,\
                     resource_group:group-1,revisionname:parity-app--1,subscription_id:sub-1",
                )),
            ),
            (
                &[("CONTAINER_APP_NAME", "app")],
                Some((
                    "azure.app_containerapps.",
                    Some("azure.containerapp."),
                    "replica",
                    "name:app,origin:containerapp,region:unknown,resource_group:unknown,\
                     revisionname:unknown,subscription_id:unknown",
                    "name:app,origin:containerapp,region:unknown,replica:unknown,\
                     resource_group:unknown,revisionname:unknown,subscription_id:unknown",
                )),
            ),
            (
                &[("K_SERVICE", "svc")],
                Some((
                    "gcp.run.container.",
                    Some("gcp.run."),
                    "instance",
                    "location:unknown,origin:cloudrun,project_id:unknown,revision_name:unknown,\
                     service_name:svc",
                    "instance:unknown,location:unknown,origin:cloudrun,project_id:unknown,\
                     revision_name:unknown,service_name:svc",
                )),
            ),
            (&[("CLOUD_RUN_JOB", "a")], None),
        ];
        for (variables, expected) in cases {
            let platform = Platform::detect(lookup(variables), || "host-1".to_string());
            let actual = platform.enhanced_metrics().map(|profile| {
                (
                    profile.prefix,
                    profile.legacy_prefix,
                    profile.usage_metric_suffix,
                    join_tags(&profile.base_tags),
                    join_tags(&profile.usage_tags),
                )
            });
            let expected = expected.map(|(prefix, legacy, suffix, base, usage)| {
                (prefix, legacy, suffix, base.to_string(), usage.to_string())
            });
            assert_eq!(actual, expected, "{variables:?}");
        }
    }

    #[test]
    fn with_gcp_metadata_table() {
        let metadata = |instance_id: &str, region: &str, project_id: &str| GcpMetadata {
            instance_id: instance_id.to_string(),
            region: region.to_string(),
            project_id: project_id.to_string(),
        };
        let cases: &[(Variables, GcpMetadata, &str)] = &[
            (
                &[("K_SERVICE", "svc")],
                metadata("id-1", "us-central1", "project-1"),
                "_dd.origin:cloudrun,container_id:id-1,gcr.container_id:id-1,\
                 gcr.location:us-central1,gcr.project_id:project-1,\
                 gcr.resource_name:projects/project-1/locations/us-central1/services/svc,\
                 gcr.service_name:svc,location:us-central1,origin:cloudrun,project_id:project-1,\
                 service_name:svc",
            ),
            // A failed request leaves its tags out, and the resource name needs the region.
            (
                &[("K_SERVICE", "svc")],
                metadata("id-1", "", "project-1"),
                "_dd.origin:cloudrun,container_id:id-1,gcr.container_id:id-1,\
                 gcr.project_id:project-1,gcr.service_name:svc,origin:cloudrun,\
                 project_id:project-1,service_name:svc",
            ),
            // The resource name needs the service, which an empty K_SERVICE leaves out.
            (
                &[("K_SERVICE", "")],
                metadata("", "us-central1", "project-1"),
                "_dd.origin:cloudrun,gcr.location:us-central1,gcr.project_id:project-1,\
                 location:us-central1,origin:cloudrun,project_id:project-1",
            ),
        ];
        for (variables, metadata, expected) in cases {
            let platform =
                Platform::detect(lookup(variables), String::new).with_gcp_metadata(metadata);
            assert_eq!(
                join_tags(&platform.tags),
                *expected,
                "{variables:?} {metadata:?}"
            );
        }
    }

    #[test]
    fn metric_origin_table() {
        // The category, the service and the runtime service of DogStatsD then enhanced metrics.
        type Origins = [(u32, u32, Option<u32>); 2];
        let cases: &[(Variables, Origins)] = &[
            (&[], [(0, 0, None), (0, 0, None)]),
            (&[("CLOUD_RUN_JOB", "a")], [(0, 0, None), (0, 0, None)]),
            (
                &[("CONTAINER_APP_NAME", "a")],
                [(37, 472, Some(474)), (37, 473, None)],
            ),
            (
                &[("K_SERVICE", "a")],
                [(36, 472, Some(474)), (36, 473, None)],
            ),
        ];
        for (variables, expected) in cases {
            let platform = Platform::detect(lookup(variables), String::new);
            let actual = [false, true].map(|enhanced| {
                let origin = platform.metric_origin(enhanced);
                (
                    origin.category.into(),
                    origin.service.into(),
                    origin.runtime_service.map(u32::from),
                )
            });
            assert_eq!(actual, *expected, "{variables:?}");
        }
    }

    #[test]
    fn missing_required_setting_table() {
        const SUBSCRIPTION: &str = "Must set Subscription ID as an environment variable. Please \
             set the DD_AZURE_SUBSCRIPTION_ID value to your Subscription ID your App Container is in.";
        const GROUP: &str = "Must set Resource Group as an environment variable. Please set the \
             DD_AZURE_RESOURCE_GROUP value to your Resource Group your App Container is in.";
        let cases: &[(Variables, Option<&str>)] = &[
            (
                &[("CONTAINER_APP_NAME", "a"), ("DD_API_KEY", "k")],
                Some(SUBSCRIPTION),
            ),
            (
                &[
                    ("CONTAINER_APP_NAME", "a"),
                    ("DD_APM_API_KEY", "k"),
                    ("DD_AZURE_SUBSCRIPTION_ID", ""),
                ],
                Some(GROUP),
            ),
            (
                &[
                    ("CONTAINER_APP_NAME", "a"),
                    ("DD_API_KEY", "k"),
                    ("DD_AZURE_SUBSCRIPTION_ID", ""),
                    ("DD_AZURE_RESOURCE_GROUP", ""),
                ],
                None,
            ),
            // Without an API key, or off Container Apps, nothing is required.
            (&[("CONTAINER_APP_NAME", "a"), ("DD_API_KEY", " ")], None),
            (&[("DD_API_KEY", "k")], None),
        ];
        for (variables, expected) in cases {
            let platform = Platform::detect(lookup(variables), String::new);
            assert_eq!(
                platform.missing_required_setting(lookup(variables)),
                *expected,
                "{variables:?}"
            );
        }
    }

    #[test]
    fn telemetry_tags_table() {
        let unsupported = Platform::detect(lookup(&[("CLOUD_RUN_JOB", "a")]), String::new);
        let cases: [(Platform, bool, Variables, &str); 5] = [
            (
                local("host-1"),
                false,
                &[],
                "_dd.datadog_init_version:1.2.3,instance:host-1,local:true",
            ),
            (
                local(""),
                true,
                &[],
                "_dd.datadog_sidecar_version:1.2.3,instance:unknown,local:true",
            ),
            (
                unsupported,
                false,
                &[("DD_SERVICE", "Parity-App")],
                "_dd.datadog_init_version:1.2.3,service:parity-app",
            ),
            (
                local("host-1"),
                false,
                &[
                    ("DD_ENV", "Prod"),
                    ("DD_VERSION", "2"),
                    ("DD_TAGS", "team:parity"),
                    ("DD_EXTRA_TAGS", "extra:1"),
                ],
                "_dd.datadog_init_version:1.2.3,env:prod,extra:1,instance:host-1,local:true,\
                 team:parity,version:2",
            ),
            // Configured tags replace the unified service tags, and the platform's tags replace
            // both.
            (
                local("host-1"),
                false,
                &[
                    ("DD_SERVICE", "from-service"),
                    ("DD_TAGS", "service:from-tags,local:false,instance:other"),
                ],
                "_dd.datadog_init_version:1.2.3,instance:host-1,local:true,service:from-tags",
            ),
        ];
        for (platform, sidecar, variables, expected) in cases {
            let tags = telemetry_tags(&platform, sidecar, "1.2.3", lookup(variables));
            assert_eq!(join_tags(&tags), expected, "{platform:?}, {variables:?}");
        }
    }

    #[test]
    fn dogstatsd_tags_table() {
        let telemetry = BTreeMap::from([
            ("instance".to_string(), "host-1".to_string()),
            ("local".to_string(), "true".to_string()),
            ("replica_name".to_string(), "r1".to_string()),
        ]);
        let cases: [(Variables, &[&str]); 2] = [
            (&[], &["local:true"]),
            (
                &[("DD_DOGSTATSD_TAGS", "pipeline:dogstatsd  Team:A a:1,b:2")],
                &["pipeline:dogstatsd", "Team:A", "a:1,b:2", "local:true"],
            ),
        ];
        for (variables, expected) in cases {
            assert_eq!(
                dogstatsd_tags(&telemetry, lookup(variables)),
                expected,
                "{variables:?}"
            );
        }
    }
}
