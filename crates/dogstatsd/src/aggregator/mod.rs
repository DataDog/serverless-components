// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! The aggregation of metrics.

mod core;
pub mod service;

pub use self::core::*;
pub use service::{AggregatorHandle, AggregatorService, FlushResponse};
