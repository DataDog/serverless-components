// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

#![cfg_attr(not(test), deny(clippy::panic))]
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![cfg_attr(not(test), deny(clippy::todo))]
#![cfg_attr(not(test), deny(clippy::unimplemented))]

pub mod aggregator;
pub mod config;
pub mod constants;
pub mod errors;
pub mod flusher;
pub mod intake_entry;
pub mod logs_additional_endpoint;

pub mod server;

// Re-export the most commonly used types at the crate root
pub use aggregator::{AggregatorHandle, AggregatorService};
pub use config::{FlusherMode, LogFlusherConfig};
pub use flusher::LogFlusher;
pub use intake_entry::IntakeEntry;
pub use logs_additional_endpoint::LogsAdditionalEndpoint;
pub use server::{LogServer, LogServerConfig};
