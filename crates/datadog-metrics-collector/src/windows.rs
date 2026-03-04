// Copyright 2023-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

//! CPU metrics collector for Azure Functions
//!
//! This module provides functionality to read raw CPU statistics
//! and compute the CPU usage and limit in Windows environments.
//!
//! All CPU metrics are reported in nanocores (1 core = 1,000,000,000 nanocores).

use crate::cpu::{CpuStats, CpuStatsReader};
use std::mem::size_of;
use tracing::debug;
use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::JobObjects::{
    JobObjectBasicAccountingInformation, JobObjectCpuRateControlInformation,
    QueryInformationJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, GetSystemTimes, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

pub struct WindowsCpuStatsReader;

fn read_system_cpu_usage_ns() -> Option<u64> {
    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();

    unsafe {
        GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).ok()?;
    }
    let idle_ns = filetime_to_ns(&idle);
    let kernel_ns = filetime_to_ns(&kernel);
    let user_ns = filetime_to_ns(&user);
    let active_ns = (kernel_ns - idle_ns) + user_ns;
    Some(active_ns)
}

fn filetime_to_ns(filetime: &FILETIME) -> u64 {
    (((filetime.dwHighDateTime as u64) << 32) | filetime.dwLowDateTime as u64) * 100
}

fn read_job_cpu_time_ns() -> Option<u64> {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    unsafe {
        QueryInformationJobObject(
            None, // If the handle is null, the job associated with the current process is used
            JobObjectBasicAccountingInformation, // The type of info to retrieve
            &mut info as *mut _ as *mut _, // Pointer to the struct to receive the info
            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            None,
        )
        .ok()?;
    };
    // TotalUserTime and TotalKernelTime are in 100-nanosecond units - multiply by 100 to get nanoseconds
    let total_ns = (info.TotalUserTime + info.TotalKernelTime) as u64 * 100;
    debug!(
        "Job CPU time: {} ns (user: {} ns, kernel: {} ns)",
        total_ns,
        info.TotalUserTime as u64 * 100,
        info.TotalKernelTime as u64 * 100
    );
    Some(total_ns)
}

/// Reads the CPU rate limit for the job object in nanocores.
/// Falls back to host CPU count if no limit is set.
fn read_job_cpu_limit_nc() -> (f64, bool) {
    let mut info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
    let result = unsafe {
        QueryInformationJobObject(
            None,
            JobObjectCpuRateControlInformation,
            &mut info as *mut _ as *mut _,
            size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            None,
        )
    };
    if result.is_ok()
        && (info.ControlFlags & JOB_OBJECT_CPU_RATE_CONTROL_ENABLE)
            == JOB_OBJECT_CPU_RATE_CONTROL_ENABLE
    {
        // CpuRate is in units of 1/100th of a percent (10000 = 100% = 1 core)
        let cpu_rate = unsafe { info.Anonymous.CpuRate } as f64;
        let limit_nc = (cpu_rate / 10000.0) * num_cpus::get() as f64 * 1_000_000_000.0;
        debug!(
            "Job CPU rate limit: {} nanocores (CpuRate: {})",
            limit_nc, cpu_rate
        );
        (limit_nc, false)
    } else {
        let limit_nc = num_cpus::get() as f64 * 1_000_000_000.0;
        debug!(
            "No job CPU rate limit found, defaulting to host CPU count: {} nanocores",
            limit_nc
        );
        (limit_nc, true)
    }
}

impl CpuStatsReader for WindowsCpuStatsReader {
    fn read(&self) -> Option<CpuStats> {
        // Using QueryInformationJobObject
        let total_time_ns = read_job_cpu_time_ns()?;
        let host_total = read_system_cpu_usage_ns()?;

        let (limit_nc, defaulted_limit) = read_job_cpu_limit_nc();

        Some(CpuStats {
            total: total_time_ns as f64,
            host_total: host_total as f64,
            limit: Some(limit_nc),
            defaulted_limit: defaulted_limit,
        })

        // let limit_nc = num_cpus::get() as f64 * 1000000000.0;
        // debug!("Windows CPU limit: {} nc", limit_nc);

        // Some(CpuStats {
        //     total: total_time_ns as f64,
        //     limit: Some(limit_nc),
        //     defaulted_limit: true,
        // })
    }
}
