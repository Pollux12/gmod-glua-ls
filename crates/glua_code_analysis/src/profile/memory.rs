//! Process memory, for the benchmark's peak report and, under
//! `GLUALS_PROFILE`, checkpoints through a re-analysis.

/// The process's memory, in bytes.
#[derive(Debug, Clone, Copy)]
pub struct ProcessMemory {
    pub working_set: usize,
    pub private: usize,
    pub peak_working_set: usize,
    pub peak_private: usize,
}

#[cfg(windows)]
mod platform {
    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(
            process: isize,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
    }

    pub(super) fn process_memory() -> Option<super::ProcessMemory> {
        let mut counters = ProcessMemoryCounters {
            cb: std::mem::size_of::<ProcessMemoryCounters>() as u32,
            ..Default::default()
        };
        // SAFETY: `counters` is a correctly sized, writable
        // PROCESS_MEMORY_COUNTERS, and the pseudo-handle needs no closing.
        let ok =
            unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
        (ok != 0).then_some(super::ProcessMemory {
            working_set: counters.working_set_size,
            private: counters.pagefile_usage,
            peak_working_set: counters.peak_working_set_size,
            peak_private: counters.peak_pagefile_usage,
        })
    }
}

#[cfg(not(windows))]
mod platform {
    pub(super) fn process_memory() -> Option<super::ProcessMemory> {
        None
    }
}

/// The process's current and peak memory, where the platform reports it.
pub fn process_memory() -> Option<ProcessMemory> {
    platform::process_memory()
}

fn megabytes(bytes: usize) -> f64 {
    bytes as f64 / 1_048_576.0
}

/// Counters a binary with a counting allocator lends the profile.
#[derive(Debug, Clone, Copy)]
pub struct AllocCounters {
    /// The bytes allocated and not yet freed.
    pub live: fn() -> usize,
    /// The highest live bytes since the last call, resetting it to the live bytes.
    pub take_peak: fn() -> usize,
    /// The allocations made and the bytes they asked for, in total.
    pub allocated: fn() -> (u64, u64),
}

static ALLOC_COUNTERS: std::sync::OnceLock<AllocCounters> = std::sync::OnceLock::new();
static LAST_ALLOCATED: std::sync::Mutex<(u64, u64)> = std::sync::Mutex::new((0, 0));

/// Lends the profile the allocator's counters, which checkpoints then print.
pub fn set_alloc_counters(counters: AllocCounters) {
    let _ = ALLOC_COUNTERS.set(counters);
}

/// Whether checkpoints print: under `GLUALS_PROFILE` or `GLUALS_MEMORY`.
pub fn checkpoints() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| super::profiling() || std::env::var_os("GLUALS_MEMORY").is_some())
}

/// Prints the process's memory at `label` under `GLUALS_PROFILE` or
/// `GLUALS_MEMORY`.
pub fn checkpoint(label: &str) {
    if !checkpoints() {
        return;
    }
    let allocator = ALLOC_COUNTERS.get().map(|counters| {
        let (allocations, bytes) = (counters.allocated)();
        let mut last = LAST_ALLOCATED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let since = (allocations - last.0, bytes - last.1);
        *last = (allocations, bytes);
        format!(
            "; live {:.0} MB, peak since last {:.0} MB, {} allocations of {:.0} MB since last",
            megabytes((counters.live)()),
            megabytes((counters.take_peak)()),
            since.0,
            megabytes(since.1 as usize)
        )
    });
    if let Some(memory) = process_memory() {
        eprintln!(
            "[mem] {label}: working set {:.0} MB, private {:.0} MB (peak working set {:.0} MB, peak private {:.0} MB){}",
            megabytes(memory.working_set),
            megabytes(memory.private),
            megabytes(memory.peak_working_set),
            megabytes(memory.peak_private),
            allocator.unwrap_or_default()
        );
    }
}

/// Prints the process's peak memory; always, not only under `GLUALS_PROFILE`.
pub fn report_peak() {
    if let Some(memory) = process_memory() {
        eprintln!(
            "[mem] peak working set {:.0} MB, peak private {:.0} MB",
            megabytes(memory.peak_working_set),
            megabytes(memory.peak_private)
        );
    }
}
