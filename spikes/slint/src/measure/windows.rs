//! Windows process accounting for the opt-in development binary only.
//! Collected before exit, including short runs; no polling or global OS changes.
//! ABI references: Microsoft Learn PROCESS_MEMORY_COUNTERS_EX, IO_COUNTERS,
//! GetProcessMemoryInfo, GetProcessTimes and GetProcessIoCounters.
use std::ffi::c_void;
use std::io;

#[repr(C)]
#[derive(Default)]
struct MemoryCounters {
    cb: u32,
    page_faults: u32,
    peak_working_set: usize,
    working_set: usize,
    peak_paged_pool: usize,
    paged_pool: usize,
    peak_nonpaged_pool: usize,
    nonpaged_pool: usize,
    pagefile: usize,
    peak_pagefile: usize,
    private_usage: usize,
}

#[repr(C)]
#[derive(Default)]
struct IoCounters {
    reads: u64,
    writes: u64,
    others: u64,
    read_bytes: u64,
    write_bytes: u64,
    other_bytes: u64,
}

#[repr(C)]
#[derive(Default)]
struct FileTime {
    low: u32,
    high: u32,
}

impl FileTime {
    fn seconds(&self) -> f64 {
        ((u64::from(self.high) << 32) | u64::from(self.low)) as f64 / 10_000_000.0
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "GetCurrentProcess"]
    fn current_process() -> *mut c_void;
    #[link_name = "GetCurrentThread"]
    fn current_thread() -> *mut c_void;
    #[link_name = "GetCurrentThreadId"]
    fn current_thread_id() -> u32;
    #[link_name = "K32GetProcessMemoryInfo"]
    fn memory_info(process: *mut c_void, counters: *mut MemoryCounters, bytes: u32) -> i32;
    #[link_name = "GetProcessIoCounters"]
    fn io_counters(process: *mut c_void, counters: *mut IoCounters) -> i32;
    #[link_name = "GetProcessTimes"]
    fn process_times(
        process: *mut c_void,
        created: *mut FileTime,
        exited: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    #[link_name = "GetThreadTimes"]
    fn thread_times(
        thread: *mut c_void,
        created: *mut FileTime,
        exited: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
}

pub(super) fn thread_id() -> u32 {
    // SAFETY: no arguments; returns the calling thread's identifier.
    unsafe { current_thread_id() }
}
pub(super) fn thread_cpu_ms() -> io::Result<f64> {
    let (mut created, mut exited, mut kernel, mut user) = (
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
    );
    // SAFETY: current-thread pseudo-handle and writable repr(C) FILETIMEs.
    unsafe {
        require_success(thread_times(
            current_thread(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        ))?;
    }
    Ok((kernel.seconds() + user.seconds()) * 1000.0)
}

struct Snapshot {
    memory: MemoryCounters,
    io: IoCounters,
    user_s: f64,
    system_s: f64,
}

fn require_success(result: i32) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn snapshot() -> io::Result<Snapshot> {
    let mut memory = MemoryCounters {
        cb: std::mem::size_of::<MemoryCounters>() as u32,
        ..Default::default()
    };
    let mut io = IoCounters::default();
    let (mut created, mut exited, mut kernel, mut user) = (
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
    );
    // SAFETY: the pseudo-handle refers to this live process and needs no close.
    // All repr(C) output structures are writable and have the documented layout.
    // A failed call is immediately reported before reading its output values.
    unsafe {
        let process = current_process();
        require_success(memory_info(
            process,
            &mut memory,
            std::mem::size_of::<MemoryCounters>() as u32,
        ))?;
        require_success(io_counters(process, &mut io))?;
        require_success(process_times(
            process,
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        ))?;
    }
    Ok(Snapshot {
        memory,
        io,
        user_s: user.seconds(),
        system_s: kernel.seconds(),
    })
}

pub(super) fn snapshot_json() -> io::Result<String> {
    let s = snapshot()?;
    #[cfg(feature = "allocation-profiling")]
    let breakdown = super::windows_memory::json()?;
    #[cfg(not(feature = "allocation-profiling"))]
    let breakdown = "null";
    Ok(format!(
        "{{\"backend\":\"win32-self-before-exit\",\"peak_working_set_bytes\":{},\"working_set_bytes\":{},\"private_commit_bytes\":{},\"peak_private_commit_bytes\":{},\"page_faults\":{},\"user_s\":{:.7},\"system_s\":{:.7},\"io_read_operations\":{},\"io_write_operations\":{},\"io_other_operations\":{},\"io_read_bytes\":{},\"io_write_bytes\":{},\"io_other_bytes\":{},\"working_set_breakdown\":{breakdown}}}",
        s.memory.peak_working_set,
        s.memory.working_set,
        s.memory.private_usage,
        s.memory.peak_pagefile,
        s.memory.page_faults,
        s.user_s,
        s.system_s,
        s.io.reads,
        s.io.writes,
        s.io.others,
        s.io.read_bytes,
        s.io.write_bytes,
        s.io.other_bytes
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_accounting_reports_resident_pages_and_lifetime_peaks() {
        let cpu_before = thread_cpu_ms().unwrap();
        let before = snapshot().unwrap();
        let pages = vec![0x5au8; 8 * 1024 * 1024];
        std::hint::black_box(&pages);
        let after = snapshot().unwrap();
        assert!(after.memory.working_set > 0);
        assert!(after.memory.peak_working_set >= after.memory.working_set);
        assert!(after.memory.peak_working_set >= before.memory.peak_working_set);
        assert!(after.memory.peak_pagefile >= after.memory.private_usage);
        assert!(after.user_s >= before.user_s && after.system_s >= before.system_s);
        assert!(after.io.reads >= before.io.reads);
        assert!(thread_cpu_ms().unwrap() >= cpu_before);
        drop(pages);
    }
}
