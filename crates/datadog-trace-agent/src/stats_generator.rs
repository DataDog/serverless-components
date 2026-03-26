// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use crate::stats_concentrator_service::{StatsConcentratorHandle, StatsError};
use libdd_library_config::tracer_metadata::TracerMetadata;
use libdd_trace_utils::tracer_payload::TracerPayloadCollection;
use tracing::error;

pub struct StatsGenerator {
    stats_concentrator: StatsConcentratorHandle,
}

#[derive(Debug, thiserror::Error)]
pub enum StatsGeneratorError {
    #[error("Failed to send command to stats concentrator: {0}")]
    ConcentratorCommandError(StatsError),
    #[error("Unsupported tracer payload version. Failed to send trace stats.")]
    TracerPayloadVersionError,
}

// Sends tracer payloads to the stats concentrator
impl StatsGenerator {
    #[must_use]
    pub fn new(stats_concentrator: StatsConcentratorHandle) -> Self {
        Self { stats_concentrator }
    }

    pub fn send(
        &self,
        tracer_payload_collection: &TracerPayloadCollection,
    ) -> Result<(), StatsGeneratorError> {
        if let TracerPayloadCollection::V07(tracer_payloads) = tracer_payload_collection {
            for tracer_payload in tracer_payloads {
                let metadata = Arc::new(TracerMetadata {
                    schema_version: 2,
                    runtime_id: None,
                    tracer_language: tracer_payload.language_name.clone(),
                    tracer_version: tracer_payload.tracer_version.clone(),
                    hostname: String::new(),
                    service_name: None,
                    service_env: Some(tracer_payload.env.clone()),
                    service_version: Some(tracer_payload.app_version.clone()),
                    process_tags: None,
                    container_id: Some(tracer_payload.container_id.clone()),
                });
                for chunk in &tracer_payload.chunks {
                    if let Err(err) = self
                        .stats_concentrator
                        .add_chunk(chunk.clone(), Arc::clone(&metadata))
                    {
                        error!("Failed to send trace chunk to concentrator: {err}");
                        return Err(StatsGeneratorError::ConcentratorCommandError(err));
                    }
                }
            }
            Ok(())
        } else {
            error!("Unsupported tracer payload version. Failed to send trace stats.");
            Err(StatsGeneratorError::TracerPayloadVersionError)
        }
    }
}
