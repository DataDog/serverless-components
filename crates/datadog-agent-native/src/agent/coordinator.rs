//! Agent Coordinator Implementation
//!
//! The coordinator manages the lifecycle of all agent components, ensuring
//! proper initialization, startup, and graceful shutdown.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

#[cfg(feature = "appsec")]
use crate::appsec;
use crate::{
    config::Config,
    event_bus::{Event, EventBus},
    remote_config,
    tags::provider::Provider as TagsProvider,
    traces,
};
use traces::{stats_flusher::StatsFlusher, trace_flusher::TraceFlusher};

/// Reasons for agent shutdown
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    /// Graceful shutdown requested (e.g., SIGTERM)
    GracefulShutdown,
    /// User-initiated shutdown (e.g., Ctrl+C)
    UserInterrupt,
    /// Fatal error occurred
    FatalError,
    /// Shutdown timeout exceeded
    Timeout,
}

/// Errors that can occur during agent operations
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Initialization error: {0}")]
    InitError(String),

    #[error("Startup error: {0}")]
    StartupError(String),

    #[error("Shutdown error: {0}")]
    ShutdownError(String),

    #[error("Remote Config error: {0}")]
    RemoteConfigError(String),

    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
}

/// Handle to a running agent, used for sending shutdown signals
#[derive(Clone)]
pub struct AgentHandle {
    shutdown_token: CancellationToken,
}

impl AgentHandle {
    /// Request a graceful shutdown of the agent
    pub fn shutdown(&self) {
        self.shutdown_token.cancel();
    }

    /// Check if shutdown has been requested
    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown_token.is_cancelled()
    }
}

/// Agent Coordinator - Central orchestrator for all Datadog Agent components.
///
/// The coordinator manages the complete lifecycle of all agent subsystems:
/// - **Trace Agent**: Collects, aggregates, and forwards APM traces
/// - **Logs Agent**: Ingests and forwards application logs
/// - **DogStatsD**: Receives and forwards metrics via UDP
/// - **Remote Configuration**: Dynamically updates agent configuration from Datadog backend
/// - **AppSec**: Monitors security events and threats
///
/// ## Lifecycle Stages
///
/// 1. **Construction** (`new()`): Initialize all components with shared resources
/// 2. **Startup** (`start()`): Start all services in dependency order
/// 3. **Running**: All services operate concurrently, processing data
/// 4. **Shutdown** (`shutdown()`): Graceful shutdown in reverse dependency order
///
/// ## Operational Modes
///
/// The coordinator supports multiple operational modes (configured via `Config`):
/// - **HttpFixedPort**: Traditional mode with fixed HTTP ports (default 8126)
/// - **HttpEphemeralPort**: OS-assigned ports to avoid conflicts
/// - **FfiOnly**: No HTTP server, only FFI direct submission
/// - **HttpUds**: Unix Domain Sockets (most secure for single-process scenarios)
///
/// ## Thread Safety
///
/// The coordinator itself is NOT thread-safe. It should be used from a single async task.
/// However, handles obtained from it (like `trace_sender`) can be safely shared across threads.
pub struct AgentCoordinator {
    /// Shared configuration for all agent components.
    ///
    /// Wrapped in Arc to allow cheap cloning for distribution to subsystems.
    config: Arc<Config>,

    /// Shutdown coordination token.
    ///
    /// When cancelled, all subsystems begin graceful shutdown. This token is cloned
    /// and passed to each service during startup, allowing coordinated shutdown across
    /// all components.
    shutdown_token: CancellationToken,

    /// Event bus sender for inter-component communication.
    ///
    /// Used to publish events that other components can subscribe to. Currently
    /// marked as dead_code as the event bus is not yet fully utilized.
    #[allow(dead_code)]
    event_bus_tx: mpsc::Sender<Event>,

    /// Event bus receiver for processing events.
    ///
    /// Wrapped in Option because it's consumed during startup when the event processing
    /// task is spawned. After startup, this will be None.
    event_bus_rx: Option<mpsc::Receiver<Event>>,

    /// Tags provider for unified service tagging.
    ///
    /// Provides consistent tags (service, env, version) across all telemetry.
    /// Shared via Arc for efficient distribution to subsystems.
    tags_provider: Arc<TagsProvider>,

    /// Flag indicating if remote configuration is enabled.
    ///
    /// When true, the agent will fetch configuration updates from Datadog backend
    /// and apply them dynamically without restart.
    remote_config_enabled: bool,

    /// Remote configuration service (if enabled).
    ///
    /// Handles TUF/Uptane security protocol, configuration fetching, and validation.
    /// None if remote configuration is disabled.
    remote_config_service: Option<Arc<remote_config::RemoteConfigService>>,

    /// Handle to control the remote configuration service.
    ///
    /// Provides methods to check service status and request configuration refreshes.
    /// None if remote configuration is disabled or not yet started.
    remote_config_handle: Option<remote_config::RemoteConfigHandle>,

    /// Task handles for gracefully-shutdown services.
    ///
    /// These tasks are awaited during shutdown to ensure clean termination.
    /// Includes: trace agent, logs agent, metrics aggregators, remote config service.
    task_handles: Vec<JoinHandle<()>>,

    /// Task handles that should be aborted immediately during shutdown.
    ///
    /// These tasks don't need graceful shutdown (e.g., periodic cleanup tasks).
    /// They're simply aborted to stop quickly.
    abort_immediately: Vec<JoinHandle<()>>,

    /// Trace aggregator for batching and sampling traces.
    ///
    /// Collects individual spans, groups them into traces, applies sampling decisions,
    /// and batches them for efficient transmission. Wrapped in Arc<Mutex> because
    /// it's accessed from multiple async tasks (HTTP handlers, FFI calls).
    /// None in FfiOnly mode if trace processing is disabled.
    trace_aggregator: Option<Arc<tokio::sync::Mutex<traces::trace_aggregator::TraceAggregator>>>,

    /// Trace sender for FFI direct trace submission.
    ///
    /// Allows traces to be submitted directly via FFI calls without going through HTTP.
    /// This is the primary mechanism in FfiOnly mode. Shared via Arc for thread-safe access.
    trace_sender: Option<Arc<traces::trace_processor::SendingTraceProcessor>>,

    /// Information about the trace agent listener (port or UDS path).
    ///
    /// In HTTP modes: contains the bound TCP port
    /// In UDS mode: contains the Unix Domain Socket path
    /// In FfiOnly mode: None (no listener)
    ///
    /// Wrapped in Arc<Mutex> because the listener info is set asynchronously after
    /// the HTTP server starts and binds to a port.
    trace_listener_info: Option<Arc<tokio::sync::Mutex<Option<traces::uds::ListenerInfo>>>>,

    /// Flag indicating if DogStatsD metrics collection is enabled.
    ///
    /// When true, the agent starts a UDP server listening for StatsD metrics.
    dogstatsd_enabled: bool,

    /// Handle for sending metrics to the DogStatsD aggregator.
    ///
    /// Used to submit metrics programmatically. The aggregator batches metrics
    /// and forwards them to Datadog. None if DogStatsD is disabled.
    dogstatsd_aggregator_handle: Option<dogstatsd::aggregator_service::AggregatorHandle>,

    /// Actual UDP port bound by DogStatsD server.
    ///
    /// May differ from configured port when port 0 (ephemeral) is requested.
    /// The OS assigns an available port, and we store it here for reporting.
    /// None if DogStatsD is disabled or failed to start.
    dogstatsd_actual_port: Option<u16>,

    /// Join handle for the DogStatsD UDP server task.
    ///
    /// Stored separately so we can abort it before waiting on the graceful
    /// shutdown list; the underlying dogstatsd implementation blocks on socket
    /// reads and does not react to cancellation tokens.
    dogstatsd_server_task: Option<JoinHandle<()>>,

    /// Flag tracking if the coordinator has been started.
    ///
    /// Prevents double-start, which would spawn duplicate services.
    /// Set to true by `start()`, checked on subsequent calls.
    is_started: bool,
}

impl AgentCoordinator {
    /// Create a new Agent Coordinator
    ///
    /// This initializes the foundational components. Call `start()` to begin processing.
    pub fn new(config: Config) -> Result<Self, AgentError> {
        crate::log_build_info();
        info!("Initializing Datadog Agent Coordinator");

        let config = Arc::new(config);
        let shutdown_token = CancellationToken::new();

        // Create event bus
        let (event_bus, event_bus_tx) = EventBus::run();
        let event_bus_rx = Some(event_bus.rx);

        // Create tags provider
        let tags_provider = Arc::new(TagsProvider::new(Arc::clone(&config)));

        let remote_config_enabled = config.remote_configuration_enabled;
        if remote_config_enabled {
            info!("Remote Configuration is enabled");
            // Note: Remote Config service initialization would go here
            // This requires setting up the RemoteConfigService with proper configuration
        } else {
            debug!("Remote Configuration is disabled");
        }

        let dogstatsd_enabled = config.dogstatsd_enabled;

        info!("Agent Coordinator initialized successfully");

        Ok(Self {
            config,
            shutdown_token,
            event_bus_tx,
            event_bus_rx,
            tags_provider,
            remote_config_enabled,
            remote_config_service: None,
            remote_config_handle: None,
            task_handles: Vec::new(),
            abort_immediately: Vec::new(),
            trace_aggregator: None,
            trace_sender: None,
            trace_listener_info: None,
            dogstatsd_enabled,
            dogstatsd_aggregator_handle: None,
            dogstatsd_actual_port: None,
            dogstatsd_server_task: None,
            is_started: false,
        })
    }

    /// Starts all agent services and background tasks.
    ///
    /// This method initializes and starts all configured agent components in dependency order:
    /// 1. Remote Configuration (if enabled)
    /// 2. Trace Agent (always - core functionality)
    /// 3. DogStatsD (if enabled)
    /// 4. Event Bus processor
    ///
    /// # Service Initialization Order
    ///
    /// Services must start in a specific order to ensure dependencies are available:
    /// - Remote Config starts first so trace agent can subscribe to config updates
    /// - Trace agent starts next with all its sub-components (aggregators, flushers, HTTP server)
    /// - DogStatsD starts independently (can run with or without trace agent)
    /// - Event bus starts last to receive events from all other services
    ///
    /// # Background Tasks
    ///
    /// Each service spawns one or more tokio tasks that run until shutdown:
    /// - **Graceful shutdown tasks** (in `task_handles`): Await signal, then perform cleanup
    /// - **Immediate abort tasks** (in `abort_immediately`): Aborted immediately on shutdown
    ///   (used for infinite loops that poll channels which never close)
    ///
    /// # Errors
    ///
    /// Returns `AgentError` if:
    /// - Agent is already started (double-start protection)
    /// - Any service fails to initialize (config errors, network binding failures, etc.)
    /// - Resource allocation fails (memory, file handles, etc.)
    ///
    /// On error, no services are left running (partial startup is prevented).
    pub async fn start(&mut self) -> Result<(), AgentError> {
        // Prevent double-start - this is a safety check to avoid spawning duplicate tasks
        // which would cause resource leaks and race conditions
        if self.is_started {
            return Err(AgentError::StartupError(
                "Agent is already started. Cannot start again.".to_string(),
            ));
        }

        info!("Starting Datadog Agent services");

        // ========================================================================
        // PHASE 1: Initialize Remote Configuration (if enabled)
        // ========================================================================
        // Remote Config must start BEFORE the trace agent so the trace agent can
        // subscribe to configuration updates (AppSec rules, sampling rates, etc.)
        if self.remote_config_enabled {
            info!("Initializing Remote Config service");

            // Check if we have the necessary configuration from environment variables
            // RemoteConfigEnv reads DD_REMOTE_CONFIGURATION_ENABLED and related vars
            let mut rc_env = remote_config::RemoteConfigEnv::from_os_env();

            // Remote Config requires BOTH an API key AND explicit enablement via env vars
            // This double-check prevents accidental activation in environments where it's not wanted
            if rc_env.enabled && !self.config.api_key.is_empty() {
                // Determine store path for Remote Config cache
                // This directory stores:
                // - TUF/Uptane metadata for secure update verification
                // - Cached configuration files (AppSec rules, sampling rates, etc.)
                // - Agent state for incremental updates
                // Default: /tmp/datadog-agent-native-rc (override via DD_REMOTE_CONFIG_STORE_PATH)
                let store_path = std::env::var("DD_REMOTE_CONFIG_STORE_PATH")
                    .unwrap_or_else(|_| "/tmp/datadog-agent-native-rc".to_string());
                let store_path = std::path::PathBuf::from(store_path);

                // Ensure store directory exists before attempting to open the database
                // If this fails, we skip Remote Config initialization (non-fatal error)
                if let Err(e) = std::fs::create_dir_all(&store_path) {
                    warn!("Failed to create Remote Config store directory: {}", e);
                } else {
                    info!("Remote Config store path: {}", store_path.display());

                    // ================================================================
                    // Remote Config Initialization Chain
                    // ================================================================
                    // The bootstrap helper mirrors the Go agent flow but keeps the
                    // sequencing readable:
                    //   1. RemoteConfigEnv: gather env overrides, subdomain, TLS flags.
                    //   2. Host runtime snapshot: hostname, env/service tags, UUID hint.
                    //   3. Store selection: persistent sled DB path vs ephemeral.
                    //   4. RemoteConfigBootstrap::build(): constructs HTTP client,
                    //      RcStore, Uptane state, ServiceConfig, telemetry handles.
                    //   5. install(): wires telemetry + runtime/credential watchers.
                    //   6. RemoteConfigService::start(): spawns pollers (refresh,
                    //      org status, websocket echo) and returns a shutdown handle.
                    //
                    // We keep the verbose logging/warnings from the previous manual
                    // implementation so failures remain easy to diagnose.

                    // Derive base settings from the host environment and merge agent overrides.
                    rc_env.enabled = self.remote_config_enabled && rc_env.enabled;
                    if rc_env.api_key.is_none() && !self.config.api_key.is_empty() {
                        rc_env.api_key = Some(self.config.api_key.clone());
                    }
                    if !self.config.site.is_empty() {
                        rc_env.site = self.config.site.clone();
                    }
                    rc_env.no_tls = self.config.remote_configuration_no_tls;
                    rc_env.no_tls_validation =
                        self.config.remote_configuration_no_tls_validation;

                    if !rc_env.enabled {
                        info!("Remote Config is disabled via environment overrides");
                    } else if rc_env.api_key.is_none() {
                        warn!("Remote Config is enabled but not properly configured (missing API key or empty value)");
                    } else {
                        // At this point we have a sanitized config; record the derived
                        // base URL/site for logging to mirror Go's startup banner.
                        let base_url = rc_env.base_url();
                        let site = rc_env.site.clone();
                        let tags = self.tags_provider.get_tags_vec();
                        // Remote Config cares about hostname/env/UUID so we surface
                        // those through a WatchedRemoteConfigRuntime (even if we do
                        // not yet stream live updates). Additional runtime data such as
                        // custom refresh cadences can be added here later.
                        let runtime_snapshot = remote_config::HostRuntimeSnapshot {
                            hostname: Some(Cow::Owned(crate::proc::hostname::get_hostname())),
                            trace_agent_env: self
                                .config
                                .env
                                .clone()
                                .map(Cow::Owned),
                            ..Default::default()
                        };
                        let runtime =
                            remote_config::WatchedRemoteConfigRuntime::new(runtime_snapshot);

                        // RemoteConfigBootstrap stitches everything together using the
                        // embedder-provided environment, agent identity, and store path.
                        // It returns telemetry counters plus the service instance, keeping
                        // parity with the Go agent's `remoteconfig/service` package.
                        let bootstrap = remote_config::RemoteConfigBootstrap {
                            env: rc_env,
                            agent_version: crate::AGENT_VERSION,
                            agent_uuid: None,
                            tags,
                            store: remote_config::StoreMode::Persistent(store_path.as_path()),
                            runtime: Some(&runtime),
                        };

                        match bootstrap.build() {
                            Ok(artifacts) => {
                                // install() attaches telemetry + watchers before handing
                                // control back to the coordinator.
                                let (service, _counters) = artifacts.install().await;
                                let rc_service = Arc::new(service);
                                info!("Remote Config service created successfully");
                                info!("  - API key: configured");
                                info!("  - Site: {}", site);
                                info!("  - Remote Config URL: {}", base_url);

                                self.remote_config_service = Some(Arc::clone(&rc_service));
                                let rc_handle = rc_service.start().await;
                                self.remote_config_handle = Some(rc_handle);

                                info!("Remote Config service started with background polling");
                            }
                            Err(remote_config::BootstrapError::Disabled) => {
                                info!(
                                    "Remote Config was disabled during bootstrap (force-off or missing prerequisites)"
                                );
                            }
                            Err(err) => {
                                warn!("Failed to bootstrap Remote Config service: {}", err);
                            }
                        }
                    }
                }
            } else {
                warn!("Remote Config is enabled but not properly configured (missing API key or disabled in env)");
            }
        } else {
            debug!("Remote Config is disabled");
        }

        // ========================================================================
        // PHASE 2: Initialize Trace Agent (ALWAYS - core functionality)
        // ========================================================================
        // The Trace Agent is the core of the agent - it ALWAYS runs regardless of
        // operational mode. Even in FfiOnly mode, trace processing happens, just
        // without an HTTP server.
        //
        // Trace Agent components:
        // - TraceAggregator: Batches individual spans into complete traces
        // - StatsAggregator: Computes statistics from trace data
        // - ProxyAggregator: Handles trace proxy requests
        // - TraceProcessor: Obfuscates sensitive data (PII, credentials, etc.)
        // - StatsProcessor: Processes trace statistics
        // - AppSecProcessor: Scans traces for security threats (if enabled)
        // - Flushers: Periodically send data to Datadog backend
        // - HTTP Server: Receives traces via HTTP (unless FfiOnly mode)
        info!("Initializing Trace Agent");

        // ----------------------------------------------------------------
        // Sub-component 1: Obfuscation Config
        // ----------------------------------------------------------------
        // Configures rules for removing sensitive data from traces (PII, credentials, etc.)
        // Obfuscation happens before traces are sent to Datadog backend
        let obfuscation_config = Arc::new(
            datadog_trace_obfuscation::obfuscation_config::ObfuscationConfig::new().map_err(
                |e| AgentError::InitError(format!("Failed to create obfuscation config: {}", e)),
            )?,
        );

        // ----------------------------------------------------------------
        // Sub-component 2: Stats Concentrator Service
        // ----------------------------------------------------------------
        // Receives raw stats from traces and concentrates them into time buckets
        // for efficient submission to the backend
        let (stats_concentrator_service, stats_concentrator_handle) =
            traces::stats_concentrator_service::StatsConcentratorService::new(Arc::clone(
                &self.config,
            ));

        // Start stats concentrator service in a background task
        // IMPORTANT: This task runs an infinite loop waiting on a channel.
        // The channel never closes (because handles are cloned everywhere),
        // so we must ABORT this task immediately on shutdown rather than
        // waiting for graceful termination (which would never happen).
        let stats_concentrator_task = tokio::spawn(async move {
            stats_concentrator_service.run().await;
        });
        self.abort_immediately.push(stats_concentrator_task);

        // ----------------------------------------------------------------
        // Sub-component 3: Aggregators
        // ----------------------------------------------------------------
        // Aggregators collect and batch data before flushing to the backend

        // TraceAggregator: Collects individual spans and groups them into complete traces.
        // Use default settings to honor Datadog's 3.2MB maximum payload size per flush.
        let trace_aggregator = Arc::new(tokio::sync::Mutex::new(
            traces::trace_aggregator::TraceAggregator::default(),
        ));
        // Store trace aggregator for coordinator access (used by FFI and flushers)
        self.trace_aggregator = Some(Arc::clone(&trace_aggregator));

        // StatsAggregator: Aggregates trace statistics before sending to concentrator
        let stats_aggregator = Arc::new(tokio::sync::Mutex::new(
            traces::stats_aggregator::StatsAggregator::new_with_concentrator(
                stats_concentrator_handle.clone(),
            ),
        ));

        // ProxyAggregator: Handles trace proxy requests (for multi-hop scenarios)
        let proxy_aggregator = Arc::new(tokio::sync::Mutex::new(
            traces::proxy_aggregator::Aggregator::default(),
        ));

        // ----------------------------------------------------------------
        // Sub-component 4: Processors
        // ----------------------------------------------------------------
        // Processors transform trace data before aggregation

        // TraceProcessor: Obfuscates sensitive data (PII, SQL queries, URLs, etc.)
        let trace_processor: Arc<dyn traces::trace_processor::TraceProcessor + Send + Sync> =
            Arc::new(traces::trace_processor::GenericTraceProcessor::new(
                obfuscation_config,
            ));

        // StatsProcessor: Extracts and processes statistics from traces
        let stats_processor: Arc<dyn traces::stats_processor::StatsProcessor + Send + Sync> =
            Arc::new(traces::stats_processor::ServerlessStatsProcessor {});

        // ----------------------------------------------------------------
        // Sub-component 5: AppSec Processor (conditional)
        // ----------------------------------------------------------------
        // If AppSec is enabled, create a processor to scan traces for security threats
        // (SQL injection, XSS, command injection, etc.)
        // AppSec is OPTIONAL - the agent continues to run even if AppSec fails to initialize
        // This ensures trace collection isn't blocked by AppSec issues
        #[cfg(feature = "appsec")]
        let appsec_processor = if self.is_appsec_enabled() {
            info!("AppSec is enabled, initializing AppSec processor");
            // Try to create AppSec processor, but don't fail startup if it fails
            match appsec::processor::Processor::new(&self.config) {
                Ok(processor) => Some(Arc::new(tokio::sync::Mutex::new(processor))),
                Err(e) => {
                    // Log warning but continue - traces will still be collected without AppSec scanning
                    warn!("Failed to initialize AppSec processor: {}", e);
                    None
                }
            }
        } else {
            None
        };

        // Create API key factory
        let api_key_factory =
            Arc::new(dogstatsd::api_key::ApiKeyFactory::new(&self.config.api_key));

        // Create trace agent
        let trace_agent = traces::trace_agent::TraceAgent::new(
            Arc::clone(&self.config),
            Arc::clone(&trace_aggregator),
            trace_processor.clone(),
            Arc::clone(&stats_aggregator),
            stats_processor,
            Arc::clone(&proxy_aggregator),
            #[cfg(feature = "appsec")]
            appsec_processor.clone(),
            Arc::clone(&self.tags_provider),
            stats_concentrator_handle.clone(),
            self.remote_config_service.clone(),
            self.shutdown_token.clone(),
        );

        // Store the listener_info reference before moving trace_agent into task
        self.trace_listener_info = Some(trace_agent.get_listener_info_ref());

        // Create SendingTraceProcessor for FFI trace submission
        let trace_tx = trace_agent.get_sender_copy();
        let stats_generator = Arc::new(traces::stats_generator::StatsGenerator::new(
            stats_concentrator_handle.clone(),
        ));
        let trace_sender = Arc::new(traces::trace_processor::SendingTraceProcessor {
            #[cfg(feature = "appsec")]
            appsec: appsec_processor,
            processor: trace_processor,
            trace_tx,
            stats_generator,
        });
        self.trace_sender = Some(trace_sender);

        // ----------------------------------------------------------------
        // Sub-component 6: Flusher Tasks
        // ----------------------------------------------------------------
        // Flushers run on periodic intervals and send batched data to Datadog backend
        // Each flusher implements retry logic with exponential backoff
        // All flushers respect the shutdown token for graceful termination

        // Trace Flusher: Sends collected traces to backend
        // Runs every `flush_timeout` seconds (configurable)
        // Retries failed traces on next flush attempt
        let trace_flusher = traces::trace_flusher::ServerlessTraceFlusher::new(
            Arc::clone(&trace_aggregator),
            Arc::clone(&self.config),
            Arc::clone(&api_key_factory),
        );
        let trace_flush_interval = std::time::Duration::from_secs(self.config.flush_timeout);
        let shutdown_token = self.shutdown_token.clone();
        let trace_flush_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(trace_flush_interval);
            let mut failed_traces = None;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        failed_traces = trace_flusher.flush(failed_traces).await;
                    }
                    () = shutdown_token.cancelled() => {
                        debug!("Trace flusher shutting down, performing final flush");
                        trace_flusher.flush(failed_traces).await;
                        break;
                    }
                }
            }
        });
        self.task_handles.push(trace_flush_task);

        // Start stats flusher
        let stats_flusher = traces::stats_flusher::ServerlessStatsFlusher::new(
            Arc::clone(&api_key_factory),
            Arc::clone(&stats_aggregator),
            Arc::clone(&self.config),
        );
        let stats_flush_interval = std::time::Duration::from_secs(10); // Stats flush every 10s
        let shutdown_token = self.shutdown_token.clone();
        let stats_flush_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(stats_flush_interval);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        stats_flusher.flush(false).await;
                    }
                    () = shutdown_token.cancelled() => {
                        debug!("Stats flusher shutting down, performing final flush");
                        stats_flusher.flush(true).await;
                        break;
                    }
                }
            }
        });
        self.task_handles.push(stats_flush_task);

        // Start proxy flusher
        let proxy_flusher = traces::proxy_flusher::Flusher::new(
            Arc::clone(&api_key_factory),
            Arc::clone(&proxy_aggregator),
            Arc::clone(&self.tags_provider),
            Arc::clone(&self.config),
        );
        let proxy_flush_interval = std::time::Duration::from_secs(self.config.flush_timeout);
        let shutdown_token = self.shutdown_token.clone();
        let proxy_flush_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(proxy_flush_interval);
            let mut failed_requests = None;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        failed_requests = proxy_flusher.flush(failed_requests).await;
                    }
                    () = shutdown_token.cancelled() => {
                        debug!("Proxy flusher shutting down");
                        break;
                    }
                }
            }
        });
        self.task_handles.push(proxy_flush_task);

        // Log operational mode before starting
        match self.config.operational_mode {
            crate::config::operational_mode::OperationalMode::HttpEphemeralPort => {
                info!("Trace Agent | Mode: HTTP (ephemeral port) | Starting...");
            }
            crate::config::operational_mode::OperationalMode::HttpFixedPort => {
                let port = self.config.trace_agent_port.unwrap_or(8126);
                info!(
                    "Trace Agent | Mode: HTTP (fixed port) | Port: {} | Starting...",
                    port
                );
            }
            crate::config::operational_mode::OperationalMode::HttpUds => {
                info!("Trace Agent | Mode: HTTP (Unix Domain Socket) | Starting...");
            }
        }

        // Start trace agent HTTP server (or skip if FFI-only mode)
        let trace_agent_task = tokio::spawn(async move {
            if let Err(e) = trace_agent.start().await {
                error!("Trace Agent error: {}", e);
            }
        });
        self.task_handles.push(trace_agent_task);

        // ========================================================================
        // PHASE 3: Initialize DogStatsD (if enabled)
        // ========================================================================
        // DogStatsD is a StatsD-compatible metrics collection service
        // Accepts metrics over UDP (or UDS) and forwards them to Datadog
        // This is OPTIONAL - disabled by default for minimal resource usage
        if self.dogstatsd_enabled {
            info!("Initializing DogStatsD");

            // Create SortedTags from tags provider
            let tags_vec = self.tags_provider.get_tags_vec();
            let tags_string = tags_vec.join(",");
            let sorted_tags = if tags_string.is_empty() {
                dogstatsd::metric::EMPTY_TAGS
            } else {
                match dogstatsd::metric::SortedTags::parse(&tags_string) {
                    Ok(tags) => tags,
                    Err(e) => {
                        error!("Failed to parse tags for DogStatsD: {:?}", e);
                        dogstatsd::metric::EMPTY_TAGS
                    }
                }
            };

            // Create aggregator service
            let max_context = 10_000; // Max number of unique metric contexts
            let (aggregator_service, aggregator_handle) =
                match dogstatsd::aggregator_service::AggregatorService::new(
                    sorted_tags,
                    max_context,
                ) {
                    Ok((service, handle)) => (service, handle),
                    Err(e) => {
                        error!("Failed to create DogStatsD aggregator service: {:?}", e);
                        return Err(AgentError::InitError(format!(
                            "Failed to create DogStatsD aggregator service: {:?}",
                            e
                        )));
                    }
                };

            // Store aggregator handle for later use
            self.dogstatsd_aggregator_handle = Some(aggregator_handle.clone());

            // Create DogStatsD configuration
            let dogstatsd_config = dogstatsd::dogstatsd::DogStatsDConfig {
                host: "127.0.0.1".to_string(),
                port: self.config.dogstatsd_port,
            };

            // Create and start DogStatsD UDP server
            let cancel_token = self.shutdown_token.child_token();
            let (dogstatsd_server, actual_port) = crate::dogstatsd_adapter::DogStatsDAdapter::new(
                &dogstatsd_config,
                aggregator_handle.clone(),
                cancel_token.clone(),
            )
            .await
            .map_err(|e| {
                AgentError::StartupError(format!("Failed to create DogStatsD adapter: {}", e))
            })?;

            // Store the actual bound port (important for ephemeral port support)
            self.dogstatsd_actual_port = Some(actual_port);

            // Spawn aggregator service task
            // This task runs an infinite loop waiting on a channel that never closes,
            // so we abort it immediately on shutdown
            let aggregator_task = tokio::spawn(async move {
                aggregator_service.run().await;
            });
            self.abort_immediately.push(aggregator_task);

            // Spawn DogStatsD server task
            let dogstatsd_task = tokio::spawn(async move {
                dogstatsd_server.spin().await;
            });
            self.dogstatsd_server_task = Some(dogstatsd_task);

            // Create MetricsIntakeUrlPrefix
            let site = match dogstatsd::datadog::Site::new(self.config.site.clone()) {
                Ok(site) => site,
                Err(e) => {
                    error!("Failed to create site for DogStatsD: {:?}", e);
                    return Err(AgentError::InitError(format!(
                        "Failed to create site: {:?}",
                        e
                    )));
                }
            };
            let metrics_intake_url_prefix =
                match dogstatsd::datadog::MetricsIntakeUrlPrefix::new(Some(site), None) {
                    Ok(prefix) => prefix,
                    Err(e) => {
                        error!("Failed to create metrics intake URL prefix: {:?}", e);
                        return Err(AgentError::InitError(format!(
                            "Failed to create metrics intake URL prefix: {:?}",
                            e
                        )));
                    }
                };

            // Create flusher config
            let flusher_config = dogstatsd::flusher::FlusherConfig {
                api_key_factory: Arc::clone(&api_key_factory),
                aggregator_handle: aggregator_handle.clone(),
                metrics_intake_url_prefix,
                https_proxy: None,
                timeout: Duration::from_secs(self.config.flush_timeout),
                retry_strategy: dogstatsd::datadog::RetryStrategy::LinearBackoff(3, 100), // 3 attempts, 100ms delay
                compression_level: 3, // zstd compression level
            };

            // Create flusher
            let mut flusher = dogstatsd::flusher::Flusher::new(flusher_config);

            // Start flusher task
            let flush_interval = Duration::from_secs(10); // Flush every 10s
            let shutdown_token = self.shutdown_token.clone();
            let dogstatsd_flush_task = tokio::spawn(async move {
                let mut interval = tokio::time::interval(flush_interval);
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            flusher.flush().await;
                        }
                        () = shutdown_token.cancelled() => {
                            debug!("DogStatsD flusher shutting down, performing final flush");
                            flusher.flush().await;
                            break;
                        }
                    }
                }
            });
            self.task_handles.push(dogstatsd_flush_task);

            // Log initialization with actual bound port
            if actual_port != self.config.dogstatsd_port {
                info!(
                    "DogStatsD initialized successfully on 127.0.0.1:{} (configured: {})",
                    actual_port, self.config.dogstatsd_port
                );
            } else {
                info!(
                    "DogStatsD initialized successfully on 127.0.0.1:{}",
                    actual_port
                );
            }
        } else {
            debug!("DogStatsD is disabled");
        }

        // NOTE: Logs Agent requires additional infrastructure
        // - Logs: Need HTTP log intake endpoint and log aggregation pipeline
        // This is marked as TODO for future implementation

        // Start Event Bus processor
        if let Some(mut event_bus_rx) = self.event_bus_rx.take() {
            debug!("Starting Event Bus processor");
            let shutdown_token = self.shutdown_token.clone();
            let handle = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        Some(event) = event_bus_rx.recv() => {
                            Self::handle_event(event);
                        }
                        () = shutdown_token.cancelled() => {
                            debug!("Event bus shutting down");
                            break;
                        }
                    }
                }
            });
            self.task_handles.push(handle);
        }

        info!(
            "Agent services started ({} background tasks running)",
            self.task_handles.len()
        );
        info!("Services status:");

        // Wait briefly for the trace agent to publish its listener info so the summary is meaningful
        let trace_info = self.await_trace_listener_info(Duration::from_secs(5)).await;

        let trace_status = match trace_info {
            Some(info) => {
                if let Some(port) = info.tcp_port {
                    format!("on TCP port {}", port)
                } else if let Some(path) = info.uds_path {
                    format!("on Unix socket {}", path)
                } else {
                    "initialized".to_string()
                }
            }
            None => "initializing (listener info pending)".to_string(),
        };
        info!("  - Trace Agent: RUNNING {}", trace_status);

        if self.remote_config_enabled {
            info!("  - Remote Config: RUNNING (polling every 60s)");
        }

        if self.dogstatsd_enabled {
            if let Some(actual_port) = self.dogstatsd_actual_port {
                if actual_port != self.config.dogstatsd_port {
                    info!(
                        "  - DogStatsD: RUNNING on 127.0.0.1:{} (configured: {})",
                        actual_port, self.config.dogstatsd_port
                    );
                } else {
                    info!("  - DogStatsD: RUNNING on 127.0.0.1:{}", actual_port);
                }
            } else {
                info!(
                    "  - DogStatsD: RUNNING on 127.0.0.1:{}",
                    self.config.dogstatsd_port
                );
            }
        } else {
            info!("  - DogStatsD: DISABLED");
        }

        info!("  - Logs Agent: NOT IMPLEMENTED (requires HTTP log intake)");

        // Mark agent as started
        self.is_started = true;

        Ok(())
    }

    /// Handle events from the event bus
    fn handle_event(event: Event) {
        match event {
            Event::TraceReceived {
                trace_id,
                span_count,
            } => {
                debug!(
                    "Trace received: trace_id={}, spans={}",
                    trace_id, span_count
                );
            }
            Event::LogsFlushed { count } => {
                debug!("Logs flushed: count={}", count);
            }
            Event::MetricsFlushed { count } => {
                debug!("Metrics flushed: count={}", count);
            }
            Event::OutOfMemory(used_mb) => {
                warn!("Out of memory warning: {} MB used", used_mb);
            }
            Event::Tombstone => {
                info!("Tombstone event received - initiating shutdown");
            }
        }
    }

    /// Get a handle to the agent for external shutdown control
    #[must_use]
    pub fn handle(&self) -> AgentHandle {
        AgentHandle {
            shutdown_token: self.shutdown_token.clone(),
        }
    }

    /// Get the trace agent listener information (TCP port or UDS path)
    ///
    /// Returns `None` if the trace agent has not been started yet or if running in FFI-only mode.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use datadog_agent_native::agent::coordinator::AgentCoordinator;
    /// # use datadog_agent_native::config::Config;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut coordinator = AgentCoordinator::new(Config::default())?;
    /// coordinator.start().await?;
    ///
    /// if let Some(info_ref) = coordinator.get_trace_agent_listener_info() {
    ///     let info = info_ref.lock().await;
    ///     if let Some(listener_info) = info.as_ref() {
    ///         if let Some(port) = listener_info.tcp_port {
    ///             println!("Trace Agent listening on TCP port: {}", port);
    ///         } else if let Some(ref path) = listener_info.uds_path {
    ///             println!("Trace Agent listening on Unix socket: {}", path);
    ///         }
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn get_trace_agent_listener_info(
        &self,
    ) -> Option<Arc<tokio::sync::Mutex<Option<traces::uds::ListenerInfo>>>> {
        self.trace_listener_info.clone()
    }

    /// Wait for the trace agent to publish listener information.
    ///
    /// Returns `Some(ListenerInfo)` as soon as the trace agent records its
    /// TCP port or UDS path, or `None` if the timeout elapses or the agent is
    /// running in FFI-only mode.
    async fn await_trace_listener_info(
        &self,
        timeout: Duration,
    ) -> Option<traces::uds::ListenerInfo> {
        let listener_ref = match &self.trace_listener_info {
            Some(listener_ref) => Arc::clone(listener_ref),
            None => return None,
        };

        let start = Instant::now();

        loop {
            if let Some(info) = {
                let guard = listener_ref.lock().await;
                guard.clone()
            } {
                return Some(info);
            }

            if start.elapsed() >= timeout {
                return None;
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Wait for a shutdown signal
    ///
    /// This blocks until a shutdown is requested via:
    /// - SIGTERM/SIGINT (Ctrl+C)
    /// - AgentHandle::shutdown()
    /// - Internal error condition
    pub async fn wait_for_shutdown(&self) -> ShutdownReason {
        tokio::select! {
            () = self.shutdown_token.cancelled() => {
                info!("Shutdown requested via cancellation token");
                ShutdownReason::GracefulShutdown
            }
            result = tokio::signal::ctrl_c() => {
                match result {
                    Ok(()) => {
                        info!("Received Ctrl+C, initiating shutdown");
                        ShutdownReason::UserInterrupt
                    }
                    Err(e) => {
                        error!("Failed to listen for Ctrl+C: {}", e);
                        ShutdownReason::FatalError
                    }
                }
            }
        }
    }

    /// Perform graceful shutdown of all services
    ///
    /// Shuts down all services in reverse dependency order with a timeout.
    pub async fn shutdown(&mut self) -> Result<(), AgentError> {
        info!("Initiating graceful shutdown");

        // Signal all components to shut down
        self.shutdown_token.cancel();

        // Shutdown Remote Config service FIRST, before waiting for other tasks.
        // This ensures the Sled database is closed immediately and doesn't keep
        // running for the full timeout period.
        //
        // CRITICAL: We must drop BOTH the handle AND the Arc reference to the service.
        // The coordinator holds Arc::clone(&rc_service) which prevents the service
        // from being destroyed even after we drop the handle. If we don't drop this
        // Arc, the Sled database stays alive for the full 30-second timeout!
        self.remote_config_service = None; // Drop Arc reference FIRST

        if let Some(rc_handle) = self.remote_config_handle.take() {
            info!("Shutting down Remote Config service");
            rc_handle.shutdown().await;
            info!("Remote Config service shutdown complete");
        }

        if let Some(server_handle) = self.dogstatsd_server_task.take() {
            debug!("Aborting DogStatsD server task to unblock shutdown");
            server_handle.abort();
        }

        // Abort tasks that don't need graceful shutdown (e.g., infinite loops waiting on channels)
        // These tasks can't respond to shutdown signals because their channels never close
        // (handles are cloned to multiple components), so we must abort them immediately.
        if !self.abort_immediately.is_empty() {
            info!(
                "Aborting {} tasks that don't need graceful shutdown",
                self.abort_immediately.len()
            );
            for handle in self.abort_immediately.drain(..) {
                handle.abort();
            }
        }

        // Wait for all tasks with timeout
        let shutdown_timeout = Duration::from_secs(30);
        let shutdown_deadline = tokio::time::Instant::now() + shutdown_timeout;

        info!("Waiting for {} tasks to complete", self.task_handles.len());

        // Drain task handles (we can't move out of self since it has Drop)
        let mut handles = Vec::new();
        std::mem::swap(&mut handles, &mut self.task_handles);

        for (idx, handle) in handles.into_iter().enumerate() {
            let remaining =
                shutdown_deadline.saturating_duration_since(tokio::time::Instant::now());

            if remaining.is_zero() {
                warn!("Shutdown timeout exceeded, aborting remaining tasks");
                handle.abort();
                continue;
            }

            match tokio::time::timeout(remaining, handle).await {
                Ok(Ok(())) => {
                    debug!("Task {} completed successfully", idx);
                }
                Ok(Err(e)) => {
                    error!("Task {} failed: {}", idx, e);
                }
                Err(_) => {
                    warn!("Task {} timed out, aborting", idx);
                }
            }
        }

        info!("Agent shutdown complete");
        Ok(())
    }

    /// Get the current configuration
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Check if AppSec is enabled
    #[must_use]
    #[cfg(feature = "appsec")]
    pub fn is_appsec_enabled(&self) -> bool {
        appsec::is_enabled(&self.config)
    }

    /// Check if AppSec is enabled (always false when feature is disabled)
    #[must_use]
    #[cfg(not(feature = "appsec"))]
    pub fn is_appsec_enabled(&self) -> bool {
        if self.config.appsec_enabled {
            warn!("AppSec requested via configuration, but the build was compiled without the `appsec` feature. Ignoring AppSec settings.");
        }
        false
    }

    /// Check if Remote Config is enabled
    #[must_use]
    pub fn is_remote_config_enabled(&self) -> bool {
        self.remote_config_enabled
    }

    /// Returns the bound port of the trace agent's HTTP server.
    ///
    /// Returns `None` if:
    /// - The agent is running in FFI-only mode (no HTTP server)
    /// - The HTTP server hasn't started yet
    /// - The agent hasn't been started
    /// - The agent is using Unix Domain Sockets (not TCP)
    pub async fn get_bound_port(&self) -> Option<u16> {
        match &self.trace_listener_info {
            Some(listener_ref) => {
                let info = listener_ref.lock().await;
                info.as_ref().and_then(|i| i.tcp_port)
            }
            None => None,
        }
    }

    /// Returns the actual bound port of the DogStatsD UDP server.
    ///
    /// Returns `None` if:
    /// - DogStatsD is disabled
    /// - The agent hasn't been started yet
    ///
    /// When port 0 is configured (ephemeral port), this returns the actual port
    /// assigned by the OS, allowing clients to discover the correct port for metric submission.
    pub fn get_dogstatsd_port(&self) -> Option<u16> {
        self.dogstatsd_actual_port
    }
}

impl Drop for AgentCoordinator {
    fn drop(&mut self) {
        // Ensure shutdown signal is sent
        self.shutdown_token.cancel();

        // Abort any remaining tasks
        for handle in &self.task_handles {
            handle.abort();
        }

        // Abort tasks that need immediate shutdown
        for handle in &self.abort_immediately {
            handle.abort();
        }

        if let Some(handle) = self.dogstatsd_server_task.take() {
            handle.abort();
        }
    }
}
