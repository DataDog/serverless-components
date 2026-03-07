// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! CPU metrics collector for Azure Functions
//!
//! This module provides functionality to read raw CPU statistics
//! and compute the CPU usage and limit in Windows environments.
//!
//! All CPU metrics are reported in nanocores (1 core = 1,000,000,000 nanocores).

use crate::cpu::{CpuStats, CpuStatsReader};
use tracing::debug;

pub struct WindowsCpuStatsReader;

impl CpuStatsReader for WindowsCpuStatsReader {
    fn read(&self) -> Option<CpuStats> {
        debug!("CPU enhanced metrics are not yet supported on Windows Azure Functions");
        None
    }
}
