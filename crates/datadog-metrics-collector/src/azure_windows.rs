// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! CPU metrics collector for Azure Functions (Windows)
//!
//! NOTE: Windows CPU enhanced metrics are not yet supported.
//! WindowsCpuStatsReader currently always returns None, so no CPU
//! usage information is reported in Windows environments.
//!
//! All CPU metrics will be reported in nanocores (1 core = 1,000,000,000 nanocores).

use crate::azure_cpu::{CpuStats, CpuStatsReader};

pub struct WindowsCpuStatsReader;

impl CpuStatsReader for WindowsCpuStatsReader {
    fn read(&self) -> Option<CpuStats> {
        None
    }
}
