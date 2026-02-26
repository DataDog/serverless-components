// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::metric::{Metric, SortedTags};
use datadog_protos::metrics::Origin;

// Metric tag keys
const DD_ORIGIN_TAG_KEY: &str = "origin";
const AWS_LAMBDA_TAG_KEY: &str = "function_arn";

// Metric tag values
const GOOGLE_CLOUD_RUN_TAG_VALUE: &str = "cloudrun";
const AZURE_APP_SERVICES_TAG_VALUE: &str = "appservice";
const AZURE_CONTAINER_APP_TAG_VALUE: &str = "containerapp";
const AZURE_FUNCTIONS_TAG_VALUE: &str = "azurefunction";

// Metric prefixes
const DATADOG_PREFIX: &str = "datadog.";
const AWS_LAMBDA_PREFIX: &str = "aws.lambda";
const GOOGLE_CLOUD_RUN_PREFIX: &str = "gcp.run";
const AZURE_FUNCTIONS_PREFIX: &str = "azure.functions";
const JVM_PREFIX: &str = "jvm.";
const RUNTIME_PREFIX: &str = "runtime.";

/// Represents the product origin of a metric.
/// The full enum is exhaustive so we only include what we need.
pub enum OriginProduct {
    Other = 0,
    Serverless = 1,
}

impl From<OriginProduct> for u32 {
    fn from(product: OriginProduct) -> u32 {
        product as u32
    }
}

/// Represents the category origin of a metric.
/// The full enum is exhaustive so we only include what we need.
pub enum OriginCategory {
    Other = 0,
    AppServicesMetrics = 35,
    CloudRunMetrics = 36,
    ContainerAppMetrics = 37,
    LambdaMetrics = 38,
    AzureFunctionsMetrics = 71,
}

impl From<OriginCategory> for u32 {
    fn from(category: OriginCategory) -> u32 {
        category as u32
    }
}

/// Represents the service origin of a metric.
/// The full enum is exhaustive so we only include what we need.
pub enum OriginService {
    Other = 0,
    ServerlessCustom = 472,
    ServerlessEnhanced = 473,
    ServerlessRuntime = 474,
}

impl From<OriginService> for u32 {
    fn from(service: OriginService) -> u32 {
        service as u32
    }
}

impl Metric {
    /// Finds the origin of a metric based on its tags and name prefix.
    pub fn find_origin(&self, tags: SortedTags) -> Option<Origin> {
        let metric_name = self.name.to_string();

        // First check if it's a Datadog metric
        if metric_name.starts_with(DATADOG_PREFIX) {
            return None;
        }

        let metric_prefix = metric_name
            .split('.')
            .take(2)
            .collect::<Vec<&str>>()
            .join(".");

        // Determine the service based on metric prefix first
        let service = if metric_name.starts_with(JVM_PREFIX)
            || metric_name.starts_with(RUNTIME_PREFIX)
        {
            OriginService::ServerlessRuntime
        } else if metric_prefix == AWS_LAMBDA_PREFIX || metric_prefix == GOOGLE_CLOUD_RUN_PREFIX || metric_prefix == AZURE_FUNCTIONS_PREFIX {
            OriginService::ServerlessEnhanced
        } else {
            OriginService::ServerlessCustom
        };

        // Then determine the category based on tags
        let category = if has_tag_value(&tags, AWS_LAMBDA_TAG_KEY, "") {
            OriginCategory::LambdaMetrics
        } else if has_tag_value(&tags, DD_ORIGIN_TAG_KEY, AZURE_APP_SERVICES_TAG_VALUE) {
            OriginCategory::AppServicesMetrics
        } else if has_tag_value(&tags, DD_ORIGIN_TAG_KEY, GOOGLE_CLOUD_RUN_TAG_VALUE) {
            OriginCategory::CloudRunMetrics
        } else if has_tag_value(&tags, DD_ORIGIN_TAG_KEY, AZURE_CONTAINER_APP_TAG_VALUE) {
            OriginCategory::ContainerAppMetrics
        } else if has_tag_value(&tags, DD_ORIGIN_TAG_KEY, AZURE_FUNCTIONS_TAG_VALUE) {
            OriginCategory::AzureFunctionsMetrics
        } else {
            return None;
        };

        Some(Origin {
            origin_product: OriginProduct::Serverless.into(),
            origin_service: service.into(),
            origin_category: category.into(),
            ..Default::default()
        })
    }
}

/// Checks if the given key-value pair exists in the tags.
fn has_tag_value(tags: &SortedTags, key: &str, value: &str) -> bool {
    if value.is_empty() {
        return !tags.find_all(key).is_empty();
    }
    tags.find_all(key)
        .iter()
        .any(|tag_value| tag_value.as_str() == value)
}

#[cfg(test)]
mod tests {
    use crate::metric::MetricValue;

    use super::*;

    #[test]
    fn test_origin_product() {
        let origin_product: u32 = OriginProduct::Serverless.into();
        assert_eq!(origin_product, 1);
    }

    #[test]
    fn test_origin_category() {
        let origin_category: u32 = OriginCategory::LambdaMetrics.into();
        assert_eq!(origin_category, 38);
    }

    #[test]
    fn test_origin_service() {
        let origin_service: u32 = OriginService::ServerlessRuntime.into();
        assert_eq!(origin_service, 474);
    }

    #[test]
    fn test_find_metric_origin_lambda_runtime() {
        let tags = SortedTags::parse("function_arn:hello123").unwrap();
        let metric = Metric {
            id: 0,
            name: "runtime.memory.used".into(),
            value: MetricValue::Gauge(1.0),
            tags: Some(tags.clone()),
            timestamp: 0,
        };
        let origin = metric.find_origin(tags).unwrap();
        assert_eq!(
            origin.origin_product as u32,
            OriginProduct::Serverless as u32
        );
        assert_eq!(
            origin.origin_category as u32,
            OriginCategory::LambdaMetrics as u32
        );
        assert_eq!(
            origin.origin_service as u32,
            OriginService::ServerlessRuntime as u32
        );
    }

    #[test]
    fn test_find_metric_origin_lambda_enhanced() {
        let tags = SortedTags::parse("function_arn:hello123").unwrap();
        let metric = Metric {
            id: 0,
            name: "aws.lambda.enhanced.invocations".into(),
            value: MetricValue::Gauge(1.0),
            tags: Some(tags.clone()),
            timestamp: 0,
        };
        let origin = metric.find_origin(tags).unwrap();
        assert_eq!(
            origin.origin_product as u32,
            OriginProduct::Serverless as u32
        );
        assert_eq!(
            origin.origin_category as u32,
            OriginCategory::LambdaMetrics as u32
        );
        assert_eq!(
            origin.origin_service as u32,
            OriginService::ServerlessEnhanced as u32
        );
    }

    #[test]
    fn test_find_metric_origin_lambda_custom() {
        let tags = SortedTags::parse("function_arn:hello123").unwrap();
        let metric = Metric {
            id: 0,
            name: "my.custom.metric".into(),
            value: MetricValue::Gauge(1.0),
            tags: Some(tags.clone()),
            timestamp: 0,
        };
        let origin = metric.find_origin(tags).unwrap();
        assert_eq!(
            origin.origin_product as u32,
            OriginProduct::Serverless as u32
        );
        assert_eq!(
            origin.origin_category as u32,
            OriginCategory::LambdaMetrics as u32
        );
        assert_eq!(
            origin.origin_service as u32,
            OriginService::ServerlessCustom as u32
        );
    }

    #[test]
    fn test_find_metric_origin_cloudrun_enhanced() {
        let tags = SortedTags::parse("origin:cloudrun").unwrap();
        let metric = Metric {
            id: 0,
            name: "gcp.run.enhanced.cold_start".into(),
            value: MetricValue::Gauge(1.0),
            tags: Some(tags.clone()),
            timestamp: 0,
        };
        let origin = metric.find_origin(tags).unwrap();
        assert_eq!(
            origin.origin_product as u32,
            OriginProduct::Serverless as u32
        );
        assert_eq!(
            origin.origin_category as u32,
            OriginCategory::CloudRunMetrics as u32
        );
        assert_eq!(
            origin.origin_service as u32,
            OriginService::ServerlessEnhanced as u32
        );
    }

    #[test]
    fn test_find_metric_origin_cloudrun_custom() {
        let tags = SortedTags::parse("origin:cloudrun").unwrap();
        let metric = Metric {
            id: 0,
            name: "my.custom.metric".into(),
            value: MetricValue::Gauge(1.0),
            tags: Some(tags.clone()),
            timestamp: 0,
        };
        let origin = metric.find_origin(tags).unwrap();
        assert_eq!(
            origin.origin_product as u32,
            OriginProduct::Serverless as u32
        );
        assert_eq!(
            origin.origin_category as u32,
            OriginCategory::CloudRunMetrics as u32
        );
        assert_eq!(
            origin.origin_service as u32,
            OriginService::ServerlessCustom as u32
        );
    }

    #[test]
    fn test_find_metric_origin_azure_app_services() {
        let tags = SortedTags::parse("origin:appservice").unwrap();
        let metric = Metric {
            id: 0,
            name: "my.custom.metric".into(),
            value: MetricValue::Gauge(1.0),
            tags: Some(tags.clone()),
            timestamp: 0,
        };
        let origin = metric.find_origin(tags).unwrap();
        assert_eq!(
            origin.origin_product as u32,
            OriginProduct::Serverless as u32
        );
        assert_eq!(
            origin.origin_category as u32,
            OriginCategory::AppServicesMetrics as u32
        );
        assert_eq!(
            origin.origin_service as u32,
            OriginService::ServerlessCustom as u32
        );
    }

    #[test]
    fn test_find_metric_origin_azure_container_app() {
        let tags = SortedTags::parse("origin:containerapp").unwrap();
        let metric = Metric {
            id: 0,
            name: "my.custom.metric".into(),
            value: MetricValue::Gauge(1.0),
            tags: Some(tags.clone()),
            timestamp: 0,
        };
        let origin = metric.find_origin(tags).unwrap();
        assert_eq!(
            origin.origin_product as u32,
            OriginProduct::Serverless as u32
        );
        assert_eq!(
            origin.origin_category as u32,
            OriginCategory::ContainerAppMetrics as u32
        );
        assert_eq!(
            origin.origin_service as u32,
            OriginService::ServerlessCustom as u32
        );
    }

    #[test]
    fn test_find_metric_origin_azure_functions() {
        let tags = SortedTags::parse("origin:azurefunction").unwrap();
        let metric = Metric {
            id: 0,
            name: "my.custom.metric".into(),
            value: MetricValue::Gauge(1.0),
            tags: Some(tags.clone()),
            timestamp: 0,
        };
        let origin = metric.find_origin(tags).unwrap();
        assert_eq!(
            origin.origin_product as u32,
            OriginProduct::Serverless as u32
        );
        assert_eq!(
            origin.origin_category as u32,
            OriginCategory::AzureFunctionsMetrics as u32
        );
        assert_eq!(
            origin.origin_service as u32,
            OriginService::ServerlessCustom as u32
        );
    }

    #[test]
    fn test_find_metric_origin_jvm() {
        let tags = SortedTags::parse("function_arn:hello123").unwrap();
        let metric = Metric {
            id: 0,
            name: "jvm.memory.used".into(),
            value: MetricValue::Gauge(1.0),
            tags: Some(tags.clone()),
            timestamp: 0,
        };
        let origin = metric.find_origin(tags).unwrap();
        assert_eq!(
            origin.origin_product as u32,
            OriginProduct::Serverless as u32
        );
        assert_eq!(
            origin.origin_category as u32,
            OriginCategory::LambdaMetrics as u32
        );
        assert_eq!(
            origin.origin_service as u32,
            OriginService::ServerlessRuntime as u32
        );
    }

    #[test]
    fn test_find_metric_origin_datadog() {
        let tags = SortedTags::parse("function_arn:hello123").unwrap();
        let metric = Metric {
            id: 0,
            name: "datadog.agent.running".into(),
            value: MetricValue::Gauge(1.0),
            tags: Some(tags.clone()),
            timestamp: 0,
        };
        let origin = metric.find_origin(tags);
        assert_eq!(origin, None);
    }

    #[test]
    fn test_find_metric_origin_unknown() {
        let tags = SortedTags::parse("unknown:tag").unwrap();
        let metric = Metric {
            id: 0,
            name: "unknown.metric".into(),
            value: MetricValue::Gauge(1.0),
            tags: Some(tags.clone()),
            timestamp: 0,
        };
        let origin = metric.find_origin(tags);
        assert_eq!(origin, None);
    }
}
