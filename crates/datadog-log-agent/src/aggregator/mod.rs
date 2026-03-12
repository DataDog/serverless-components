// Copyright 2025-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

pub mod core;
pub use core::Aggregator;

pub mod service;
pub use service::{AggregatorHandle, AggregatorService};
