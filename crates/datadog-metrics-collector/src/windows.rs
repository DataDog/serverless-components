// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use crate::cpu::{CpuStats, CpuStatsReader};
use tracing::debug;

pub struct WindowsCpuStatsReader;

impl CpuStatsReader for WindowsCpuStatsReader {
    fn read(&self) -> Option<CpuStats> {
        debug!("Reading CPU stats from Windows");
        None
    }
}
