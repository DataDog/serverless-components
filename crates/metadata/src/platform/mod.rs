// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

mod azure;
mod gcp;

use libdd_common::azure_app_services::QueryEnv;
use libdd_trace_utils::trace_utils::EnvironmentType;
use serde_json::{Map, Value};

pub(crate) struct PlatformData {
    pub workload_type: &'static str,
    pub resource_id: String,
    pub resource_name: String,
    pub metadata: Map<String, Value>,
}

// Wrapper-supplied engine identity takes precedence over platform hints.
fn wrapper_runtime(env: &impl QueryEnv) -> Option<String> {
    env.get_var("DD_SERVERLESS_COMPAT_RUNTIME")
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("unknown"))
}

pub(crate) async fn collect(env_type: &EnvironmentType) -> Option<PlatformData> {
    match env_type {
        EnvironmentType::AzureFunction => azure::collect(),
        EnvironmentType::CloudFunction => gcp::collect().await,
        EnvironmentType::LambdaFunction | EnvironmentType::AzureSpringApp => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::FakeEnv;

    #[test]
    fn wrapper_runtime_is_an_optional_best_effort_hint() {
        for (value, expected) in [
            ("python3.13.7", Some("python3.13.7")),
            ("java21.0.1+12", Some("java21.0.1+12")),
            (" python3.13.7 ", Some("python3.13.7")),
            ("", None),
            ("   ", None),
            ("unknown", None),
            (" UNKNOWN ", None),
        ] {
            let env = FakeEnv::new(&[("DD_SERVERLESS_COMPAT_RUNTIME", value)]);
            assert_eq!(wrapper_runtime(&env).as_deref(), expected);
        }
        assert_eq!(wrapper_runtime(&FakeEnv::new(&[])), None);
    }
}
