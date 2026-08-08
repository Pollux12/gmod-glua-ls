//! Shared helpers for running per-file analysis passes concurrently.
//!
//! Several indexing passes (flow binding, declaration collection, gmod metadata
//! collection) process each file independently: they read only the file's own
//! AST plus pre-existing immutable `&DbIndex` state, and produce a per-file
//! result that is merged back into the db sequentially afterward. These helpers
//! run the per-file work across a small thread pool using `std::thread::scope`,
//! mirroring the existing parallel diagnostics path.
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

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::db_index::DbIndex;
use crate::{FileId, profile::Profile};

/// Number of worker threads to use for per-file analysis passes. Capped at 16 to
/// match the diagnostics path and avoid oversubscription on large machines.
fn worker_count(file_count: usize) -> usize {
    if file_count <= 1 {
        return 1;
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    cores.clamp(1, 16).min(file_count)
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

    // Pre-fill the output so workers can write by index without coordination.
    // Each slot is written exactly once by exactly one worker, so we use a raw
    // pointer wrapper guarded by the disjoint-index invariant.
    let mut results: Vec<Option<T>> = (0..n).map(|_| None).collect();
    let slots = SlotsPtr(results.as_mut_ptr());
    let next = AtomicUsize::new(0);

    // Longest-processing-time-first dispatch. Per-file cost spans orders of
    // magnitude, so handing work out in slice order let a large file drawn last
    // run alone while every other worker idled. Only the order in which slots
    // are claimed changes; each still holds its own file's result, so callers
    // see the same index-aligned `Vec` as before.
    let dispatch = dispatch_order(db, file_ids);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let next = &next;
            let f = &f;
            let slots = &slots;
            let dispatch = &dispatch;
            scope.spawn(move || {
                loop {
                    let seq = next.fetch_add(1, Ordering::Relaxed);
                    if seq >= n {
                        break;
                    }
                    let idx = dispatch[seq];
                    let file_id = file_ids[idx];
                    let value = f(db, file_id);
                    // SAFETY: each `idx` is handed to exactly one worker via the
                    // atomic counter, so writes target disjoint slots and never
                    // alias. The `Vec` outlives the scope.
                    unsafe {
                        slots.0.add(idx).write(Some(value));
                    }
                }
            });
        }
    });

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

/// Wrapper making a `*mut Option<T>` shareable across the scoped threads. Safe
/// because workers only write disjoint indices (enforced by the atomic counter).
struct SlotsPtr<T>(*mut Option<T>);

// SAFETY: the pointer is only used to write disjoint slots from worker threads;
// `T: Send` ensures the written values can cross threads.
unsafe impl<T: Send> Sync for SlotsPtr<T> {}
unsafe impl<T: Send> Send for SlotsPtr<T> {}
