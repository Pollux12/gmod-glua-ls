use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "cli")]
use mimalloc as _;

#[cfg(feature = "cli")]
unsafe extern "C" {
    fn mi_collect(force: bool);
}

/// Returns freed heap pages to the OS on the calling thread when `mimalloc` is
/// linked. `force = false` sweeps thread-local free lists cheaply; `force =
/// true` purges abandoned segments and commits OS decommit/madvise.
pub(crate) fn collect_thread_heap(force: bool) {
    #[cfg(feature = "cli")]
    unsafe {
        mi_collect(force);
    }
    #[cfg(not(feature = "cli"))]
    let _ = force;
}

/// RAII guard that drops thread-local analysis caches when a blocking task or
/// request exits (normally or by unwinding), preventing idle Tokio / blocking
/// threads from retaining dead syntax trees and type allocations.
pub(crate) struct ClearThreadCachesOnDrop {
    collect_heap: bool,
}

impl ClearThreadCachesOnDrop {
    /// Clears thread-local caches on drop without forcing an allocator sweep.
    pub(crate) fn caches_only() -> Self {
        Self {
            collect_heap: false,
        }
    }

    /// Clears thread-local caches and runs a light `mi_collect(false)` sweep
    /// on drop (intended for blocking worker threads).
    pub(crate) fn with_heap_sweep() -> Self {
        Self { collect_heap: true }
    }
}

impl Drop for ClearThreadCachesOnDrop {
    fn drop(&mut self) {
        glua_code_analysis::release_thread_caches();
        if self.collect_heap {
            collect_thread_heap(false);
        }
    }
}

/// Spawns at most one background blocking task to clear thread-local caches
/// and force-collect `mimalloc` across every Rayon worker and the blocking
/// thread itself. Safe to call outside a Tokio runtime (falls back to a
/// no-op in synchronous unit tests).
pub(crate) fn spawn_release_free_memory() {
    static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }

    handle.spawn_blocking(|| {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                IN_FLIGHT.store(false, Ordering::Release);
            }
        }
        let _reset = Reset;
        glua_code_analysis::release_analysis_caches_with(|| collect_thread_heap(true));
    });
}
