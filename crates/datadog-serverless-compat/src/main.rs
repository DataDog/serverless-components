// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(not(test), deny(clippy::panic))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![cfg_attr(not(test), deny(clippy::todo))]
#![cfg_attr(not(test), deny(clippy::unimplemented))]

use datadog_serverless_core::{ServerlessServices, ServicesConfig};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    // Load configuration from environment
    let config = match ServicesConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load configuration: {}", e);
            std::process::exit(1);
        }
    };

    // Setup logging
    let env_filter = format!("h2=off,hyper=off,rustls=off,{}", config.log_level);

    #[allow(clippy::expect_used)]
    let subscriber = tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(
            EnvFilter::try_new(env_filter).expect("could not parse log level in configuration"),
        )
        .with_level(true)
        .with_thread_names(false)
        .with_thread_ids(false)
        .with_line_number(false)
        .with_file(false)
        .with_target(true)
        .without_time()
        .finish();

    #[allow(clippy::expect_used)]
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    tracing::debug!("Logging subsystem enabled");

    // Create and start services
    let services = ServerlessServices::new(config);
    let handle = match services.start().await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("Failed to start services: {}", e);
            std::process::exit(1);
        }
    };

    tracing::info!("Serverless services started successfully");

    // Wait for shutdown signal
    #[allow(clippy::expect_used)]
    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for ctrl-c");

    tracing::info!("Received shutdown signal");

    // Stop services
    if let Err(e) = handle.stop().await {
        tracing::error!("Error stopping services: {}", e);
        std::process::exit(1);
    }

    tracing::info!("Services stopped successfully");
}
