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
use windows::Win32::Foundation::{FILETIME, CloseHandle};
use windows::Win32::System::JobObjects::{
    JobObjectBasicAccountingInformation, JobObjectCpuRateControlInformation,
    QueryInformationJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, GetSystemTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW,
    PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};

pub struct WindowsCpuStatsReader;

pub fn log_all_processes() {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    let Ok(snapshot) = snapshot else {
        debug!("Failed to create process snapshot");
        return;
    };

    let mut entry = PROCESSENTRY32W::default();
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_err() {
        debug!("Failed to get first process");
        return;
    }

    loop {
        let name = String::from_utf16_lossy(
            &entry.szExeFile[..entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(260)]
        );
        debug!(
            "Process: PID={} PPID={} name={}",
            entry.th32ProcessID,
            entry.th32ParentProcessID,
            name
        );
        if let Some(cpu_ns) = read_process_cpu_time_ns(entry.th32ProcessID) {
            debug!(
                "PID={} CPU: {} ns",
                entry.th32ProcessID,
                cpu_ns
            );
        } else {
            debug!("PID={} CPU: unavailable", entry.th32ProcessID);
        }
        

        if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
            break;
        }
    }
    let job_total = read_job_cpu_time_ns();
    debug!("Job Object total CPU: {:?} ns", job_total);
}

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

fn read_process_cpu_time_ns(pid: u32) -> Option<u64> {
    let mut creation_time = FILETIME::default();
    let mut exit_time = FILETIME::default();
    let mut kernel_time = FILETIME::default();
    let mut user_time = FILETIME::default();

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let result = GetProcessTimes(
            handle,
            &mut creation_time,
            &mut exit_time,
            &mut kernel_time,
            &mut user_time,
        );
        CloseHandle(handle).ok();
        result.ok()?;
    }

    let user_ns = filetime_to_ns(&user_time);
    let kernel_ns = filetime_to_ns(&kernel_time);
    debug!("PID={} CPU: {} ns (user: {} ns, kernel: {} ns)", pid, user_ns + kernel_ns, user_ns, kernel_ns);
    Some(user_ns + kernel_ns)
}

fn read_process_cpu_usage_ns() -> Option<u64> {
    // Using GetProcessTimes
    let mut creation_time = FILETIME::default();
    let mut exit_time = FILETIME::default();
    let mut kernel_time = FILETIME::default();
    let mut user_time = FILETIME::default();

    // All calls to Win32 APIs require unsafe in Rust
    unsafe {
        let handle = GetCurrentProcess();
        GetProcessTimes(
            handle,
            &mut creation_time,
            &mut exit_time,
            &mut kernel_time,
            &mut user_time,
        )
        .ok()?;
    }

    // The FILETIME struct contains two 32-bit values that combine to form a 64-bit count of 100-nanosecond time units
    // Multiply by 100 to get a 64-bit count of nanoseconds
    let user_time_ns =
        (((user_time.dwHighDateTime as u64) << 32) | user_time.dwLowDateTime as u64) * 100;
    let kernel_time_ns =
        (((kernel_time.dwHighDateTime as u64) << 32) | kernel_time.dwLowDateTime as u64) * 100;
    let total_time_ns = user_time_ns + kernel_time_ns;
    debug!(
        "Windows CPU usage: {} ns (user: {} ns, kernel: {} ns)",
        total_time_ns, user_time_ns, kernel_time_ns
    );
    Some(total_time_ns)
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
        debug!("Reading CPU stats from Windows - using Job Object and comparing to GetProcessTimes for each process");
        log_all_processes();

        // let total_time_ns = read_system_cpu_usage_ns()?;
        // Using QueryInformationJobObject
        let total_time_ns = read_job_cpu_time_ns()?;
        // Using GetProcessTimes
        // let total_time_ns = read_process_cpu_usage_ns()?;

        let (limit_nc, defaulted_limit) = read_job_cpu_limit_nc();
        Some(CpuStats {
            total: total_time_ns as f64,
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
