// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! CPU metrics collector for Azure Functions (Windows)
//!
//! This module provides functionality to read CPU usage from Windows Job Objects.
//!
//! All CPU metrics are reported in nanocores (1 core = 1,000,000,000 nanocores).

use crate::azure_cpu::{CpuStats, CpuStatsReader};
use tracing::debug;
use windows::Win32::System::JobObjects::{
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
    QueryInformationJobObject,
};

pub struct WindowsCpuStatsReader;

impl CpuStatsReader for WindowsCpuStatsReader {
    fn read(&self) -> Option<CpuStats> {
        read_cpu_usage_from_job_object()
    }
}

fn read_cpu_usage_from_job_object() -> Option<CpuStats> {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    let result = unsafe {
        QueryInformationJobObject(
            None, // If the handle is None, the current process's job object is used
            JobObjectBasicAccountingInformation, // The type of info to retrieve
            &mut info as *mut _ as *mut _, // Pointer to the struct that will store the info
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            None,
        )
    };
    match result {
        Ok(()) => {
            // TotalUserTime and TotalKernelTime are in 100-nanosecond units - multiply by 100 to get nanoseconds
            let total_ns = (info.TotalUserTime + info.TotalKernelTime) as u64 * 100;
            Some(CpuStats { total: total_ns })
        }
        Err(_) => {
            debug!("Failed to read CPU usage from Job Object");
            None
        }
    }
}
