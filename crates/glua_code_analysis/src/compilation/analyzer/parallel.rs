//! Shared helpers for running per-file analysis passes concurrently.
//!
//! Several indexing passes (flow binding, declaration collection, gmod metadata
//! collection) process each file independently: they read only the file's own
//! AST plus pre-existing immutable `&DbIndex` state, and produce a per-file
//! result that is merged back into the db sequentially afterward. These helpers
//! run the per-file work on rayon's persistent global thread pool, so a cold run
//! pays for thread creation once instead of once per pass.
//!
//! Safety model:
//! - `&DbIndex` is shared immutably across worker threads. The diagnostics phase
//!   already shares `&analysis` (hence `&DbIndex`) across 16 threads, so the
//!   read paths used here are concurrency-safe.
//! - rowan red trees are `!Send`; workers therefore never receive a red tree.
//!   The per-file closure rebuilds the red tree locally from the (Send) green
//!   tree stored in the VFS.
//! - Results are written back to the db on the caller's thread in deterministic
//!   file order, preserving identical behavior to the sequential version.

use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::db_index::DbIndex;
use crate::{FileId, profile::Profile};

/// Below this many files, dispatch and cross-thread handoff cost more than the
/// per-file work itself saves, so the batch runs inline. Picked from the
/// profiled cost of one pass over a handful of small files versus fanning it
/// out to a worker pool.
const MIN_PARALLEL_FILES: usize = 8;

/// Number of worker threads to use for per-file analysis passes. Capped at 16 to
/// match the diagnostics path and avoid oversubscription on large machines.
fn worker_count(file_count: usize) -> usize {
    if file_count < MIN_PARALLEL_FILES {
        return 1;
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    cores.clamp(1, 16).min(file_count)
}

/// Size rayon's global pool to the same 16-thread cap the scoped pool used, and
/// give its workers the stack analysis needs.
///
/// Workers run the same recursive inference and type-graph walks the caller's
/// thread does, so they get [`crate::ANALYSIS_STACK_SIZE`] rather than the
/// default. `build_global` is a no-op error if something already initialized the
/// pool, in which case the workers are not ours to size.
pub(crate) fn init_pool() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(cores.clamp(1, 16))
            .stack_size(crate::ANALYSIS_STACK_SIZE)
            .build_global();
    });
}

/// Run `f` for every file id concurrently and collect the results into a `Vec`
/// aligned with `file_ids` (same index order). `f` receives an immutable
/// `&DbIndex` and the file id, and must be self-contained (read-only db access).
///
/// When there is a single file (or one worker), runs inline with no threading
/// overhead.
pub fn map_files_collect<T, F>(db: &DbIndex, file_ids: &[FileId], f: F) -> Vec<T>
where
    T: Send,
    F: Fn(&DbIndex, FileId) -> T + Sync,
{
    let n = file_ids.len();
    let workers = worker_count(n);

    if workers <= 1 {
        return file_ids.iter().map(|&id| f(db, id)).collect();
    }

    let _p = Profile::cond_new("parallel map_files", n > 1);
    init_pool();

    // Longest-processing-time-first dispatch. Per-file cost spans orders of
    // magnitude, so feeding work in slice order let a large file drawn last run
    // alone while every other worker idled. Only the order in which files are
    // picked up changes; each result carries its own index, so callers see the
    // same index-aligned `Vec` as before.
    let dispatch = dispatch_order(db, file_ids);
    let report_step = if crate::progress::is_active() {
        (n / 50).max(1)
    } else {
        0
    };
    let done = AtomicUsize::new(0);

    let computed: Vec<(usize, T)> = dispatch
        .par_iter()
        .map(|&idx| {
            if report_step != 0 {
                let seq = done.fetch_add(1, Ordering::Relaxed);
                if seq.is_multiple_of(report_step) {
                    crate::progress::advance_current_phase(seq, n, "files");
                }
            }
            (idx, f(db, file_ids[idx]))
        })
        .collect();

    let mut results: Vec<Option<T>> = (0..n).map(|_| None).collect();
    for (idx, value) in computed {
        results[idx] = Some(value);
    }

    results
        .into_iter()
        .map(|slot| slot.expect("slot written"))
        .collect()
}

/// Indices into `file_ids`, ordered largest source first so the long poles are
/// started before the short ones. Ties break on the index, keeping the order a
/// pure function of the input.
fn dispatch_order(db: &DbIndex, file_ids: &[FileId]) -> Vec<usize> {
    let vfs = db.get_vfs();
    let mut order: Vec<usize> = (0..file_ids.len()).collect();
    order.sort_unstable_by_key(|&idx| {
        let size = vfs
            .get_file_content(&file_ids[idx])
            .map_or(0, |text| text.len());
        (std::cmp::Reverse(size), idx)
    });
    order
}
