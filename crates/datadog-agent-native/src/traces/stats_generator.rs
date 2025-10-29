//! Stats generation from full traces.
//!
//! This module extracts statistical information from full trace payloads to generate
//! APM metrics without requiring full trace retention. The stats generator bridges
//! the gap between trace collection and stats aggregation.
//!
//! # Agent-Side Stats Generation
//!
//! While clients can compute stats themselves (see `stats_processor`), the agent also
//! generates stats from full traces:
//! - Provides metrics for clients that don't compute stats
//! - Enables stats even when sampling drops traces
//! - Serves as a fallback mechanism
//!
//! # Processing Flow
//!
//! 1. **Receive Traces**: Full `TracerPayloadCollection` from trace processor
//! 2. **Extract Metadata**: Service name, environment, tracer version
//! 3. **Per-Span Stats**: Generate metrics for each span in each trace
//! 4. **Forward to Concentrator**: Send span stats for aggregation
//!
//! # Relationship to Client Stats
//!
//! - **Client-Computed Stats** (`stats_processor`): Tracers pre-aggregate stats
//! - **Agent-Generated Stats** (this module): Agent computes stats from traces
//!
//! Both paths feed into the same `StatsConcentrator` for aggregation and flushing.

use crate::traces::stats_concentrator_service::StatsConcentratorHandle;
use datadog_trace_utils::tracer_payload::TracerPayloadCollection;
use tracing::error;

use crate::traces::stats_concentrator_service::StatsError;

/// Stats generator that extracts statistical information from full traces.
///
/// The `StatsGenerator` processes complete trace payloads to generate span-level
/// statistics for APM metrics. It iterates through all spans in a trace and forwards
/// stats to the concentrator for aggregation.
///
/// # Architecture
///
/// ```text
/// Traces → StatsGenerator → StatsConcentrator → Aggregation → Datadog
/// ```
///
/// # Usage
///
/// The stats generator is typically invoked after trace processing but before
/// flushing, allowing metrics to be generated even when traces are sampled out.
pub struct StatsGenerator {
    /// Handle to the stats concentrator for forwarding generated stats.
    ///
    /// The concentrator aggregates stats from both agent-generated and
    /// client-computed sources into time-bucketed metrics.
    stats_concentrator: StatsConcentratorHandle,
}

/// Errors that can occur during stats generation.
#[derive(Debug, thiserror::Error)]
pub enum StatsGeneratorError {
    /// Failed to send stats to the concentrator.
    ///
    /// This typically indicates the concentrator task has stopped or the
    /// channel buffer is full.
    #[error("Error sending trace stats to the stats concentrator: {0}")]
    ConcentratorCommandError(StatsError),

    /// Trace payload version is not supported.
    ///
    /// Currently only V07 trace payloads are supported. Older versions
    /// (V04, V05) are not handled by the stats generator.
    #[error("Unsupported trace payload version. Failed to send trace stats.")]
    TracePayloadVersionError,
}

impl StatsGenerator {
    /// Creates a new stats generator with the given concentrator handle.
    ///
    /// # Arguments
    ///
    /// * `stats_concentrator` - Handle to the stats concentrator for forwarding stats
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::traces::stats_generator::StatsGenerator;
    /// // let concentrator_handle = ...; // From stats concentrator service
    /// // let generator = StatsGenerator::new(concentrator_handle);
    /// ```
    #[must_use]
    pub fn new(stats_concentrator: StatsConcentratorHandle) -> Self {
        Self { stats_concentrator }
    }

    /// Generates and sends stats from a trace payload collection.
    ///
    /// This method:
    /// 1. Validates the trace payload version (only V07 supported)
    /// 2. Sets tracer metadata (service, env, version) in the concentrator
    /// 3. Iterates through all spans in all traces
    /// 4. Forwards each span to the concentrator for stats extraction
    ///
    /// # Arguments
    ///
    /// * `traces` - Collection of tracer payloads to generate stats from
    ///
    /// # Returns
    ///
    /// - `Ok(())` - Stats successfully generated and forwarded
    /// - `Err(TracePayloadVersionError)` - Unsupported payload version
    /// - `Err(ConcentratorCommandError)` - Failed to send to concentrator
    ///
    /// # Processing
    ///
    /// For each trace:
    /// - Sets tracer metadata (service name, environment, version)
    /// - Processes all spans in all chunks
    /// - Forwards spans to concentrator for aggregation
    ///
    /// # Example
    ///
    /// ```rust
    /// use datadog_agent_native::traces::stats_generator::StatsGenerator;
    /// use datadog_trace_utils::tracer_payload::TracerPayloadCollection;
    ///
    /// async fn generate_stats(generator: &StatsGenerator, traces: &TracerPayloadCollection) {
    ///     match generator.send(traces) {
    ///         Ok(()) => println!("Stats generated successfully"),
    ///         Err(e) => eprintln!("Stats generation failed: {}", e),
    ///     }
    /// }
    /// ```
    pub fn send(&self, traces: &TracerPayloadCollection) -> Result<(), StatsGeneratorError> {
        // Only V07 trace payloads are supported for stats generation
        if let TracerPayloadCollection::V07(traces) = traces {
            for trace in traces {
                // Step 1: Set tracer metadata in the concentrator
                // This includes service name, environment, tracer version, etc.
                if let Err(err) = self.stats_concentrator.set_tracer_metadata(trace) {
                    error!("Failed to set tracer metadata: {err}");
                    return Err(StatsGeneratorError::ConcentratorCommandError(err));
                }

                // Step 2: Generate stats for each span in the trace
                // Traces are organized as: trace -> chunks -> spans
                for chunk in &trace.chunks {
                    for span in &chunk.spans {
                        // Send span to concentrator for stats extraction and aggregation
                        if let Err(err) = self.stats_concentrator.add(span) {
                            error!("Failed to send trace stats: {err}");
                            return Err(StatsGeneratorError::ConcentratorCommandError(err));
                        }
                    }
                }
            }
            Ok(())
        } else {
            // Older trace payload versions (V04, V05) are not supported
            error!("Unsupported trace payload version. Failed to send trace stats.");
            Err(StatsGeneratorError::TracePayloadVersionError)
        }
    }
}
