// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! CPU metrics collector for Azure Functions (Windows)
//!
//! This module provides functionality to read CPU usage from Windows Job Objects.
//!
//! Raw CPU time is stored in nanoseconds. The CPU metric is reported in nanocores (1 core = 1,000,000,000 nanocores).

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
    // SAFETY: `info` is a stack-allocated `JOBOBJECT_BASIC_ACCOUNTING_INFORMATION` initialized via `default()`, so the compiler guarantees its alignment.
    // The buffer size argument is `size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>()`, which exactly matches `info`, so the API cannot write out of bounds.
    // Passing `None` for the job handle is documented to use the current process's job object.
    let result = unsafe {
        QueryInformationJobObject(
            None,
            JobObjectBasicAccountingInformation, // The type of info to retrieve
            &mut info as *mut _ as *mut _,       // Pointer to the struct that will store the info
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            None,
        )
    };
    match result {
        Ok(()) => {
            // TotalUserTime and TotalKernelTime are in 100-nanosecond units - multiply by 100 to get nanoseconds
            let total_user_ns = u64::try_from(info.TotalUserTime).ok()?;
            let total_kernel_ns = u64::try_from(info.TotalKernelTime).ok()?;
            let total_ns = total_user_ns
                .checked_add(total_kernel_ns)?
                .checked_mul(100)?;
            Some(CpuStats { total: total_ns })
        }
        Err(e) => {
            debug!("Failed to read CPU usage from Job Object: {}", e);
            None
        }
    }
}
