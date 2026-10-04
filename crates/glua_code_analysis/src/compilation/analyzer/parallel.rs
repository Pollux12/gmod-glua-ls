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

/// Analysis threads requested through [`set_analysis_threads`]; 0 until then.
static REQUESTED_THREADS: AtomicUsize = AtomicUsize::new(0);

/// Sets how many threads analysis uses, pool and caller included. One runs
/// every batch on the caller's thread. Takes effect for the pool only if set
/// before its first use, so callers set it before indexing.
pub fn set_analysis_threads(threads: usize) {
    REQUESTED_THREADS.store(threads, Ordering::Relaxed);
}

/// The requested thread count, else the machine's parallelism capped at 16.
pub(crate) fn analysis_threads() -> usize {
    match REQUESTED_THREADS.load(Ordering::Relaxed) {
        0 => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, 16),
        threads => threads,
    }
}

/// Whether derivations that compute on workers with recorded reads do so:
/// with more than one analysis thread, unless `GLUALS_RECORDED_WORKERS=0`.
pub(crate) fn records_on_workers() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    analysis_threads() > 1
        && !*DISABLED.get_or_init(|| {
            std::env::var_os("GLUALS_RECORDED_WORKERS").is_some_and(|value| value == "0")
        })
}

/// Held by tests that set [`set_analysis_threads`] or rely on its value.
#[cfg(test)]
pub(crate) static THREADS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Whether this batch is better run on the caller's thread than handed to the
/// pool: too few files to pay the dispatch, or nothing to run in parallel on.
///
/// The pool's size is [`init_pool`]'s business; this only decides whether to
/// reach for it at all.
pub(crate) fn runs_inline(file_count: usize) -> bool {
    // An evaluation records its reads on its own thread; work handed to a
    // pool worker would read unrecorded.
    file_count < MIN_PARALLEL_FILES
        || crate::db_index::derivation::recording()
        || analysis_threads() <= 1
}

/// Size rayon's global pool to [`analysis_threads`], and give its workers the
/// stack analysis needs.
///
/// Workers run the same recursive inference and type-graph walks the caller's
/// thread does, so they get [`crate::ANALYSIS_STACK_SIZE`] rather than the
/// default. `build_global` is a no-op error if something already initialized the
/// pool, in which case the workers are not ours to size.
pub(crate) fn init_pool() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(analysis_threads())
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
    if runs_inline(n) {
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

    // `in_place_scope` turns the calling thread into a worker for the
    // duration: a bare `par_iter` would park it on the job latch while the
    // pool alone drained the work.
    let mut computed: Vec<(usize, T)> = Vec::new();
    rayon::in_place_scope(|_| {
        computed = dispatch
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
    });

    // Each result carries the index it was computed for, so putting them back in
    // input order is a sort rather than a scatter into pre-filled slots.
    computed.sort_unstable_by_key(|(idx, _)| *idx);
    computed.into_iter().map(|(_, value)| value).collect()
}

/// Run `f` for every item concurrently and collect the results into a `Vec`
/// aligned with `items`. Each rayon task builds its own scratch state with
/// `init` and reuses it across the items it runs. `None` when the batch is
/// better run on the caller's thread, see [`runs_inline`].
pub fn map_items_init<T, S, R>(
    items: &[T],
    init: impl Fn() -> S + Sync + Send,
    f: impl Fn(&mut S, &T) -> R + Sync + Send,
) -> Option<Vec<R>>
where
    T: Sync,
    R: Send,
{
    if runs_inline(items.len()) {
        return None;
    }

    let _p = Profile::cond_new("parallel map_items", items.len() > 1);
    init_pool();
    let mut results = Vec::new();
    rayon::in_place_scope(|_| {
        results = items
            .par_iter()
            .map_init(&init, |state, item| f(state, item))
            .collect();
    });
    Some(results)
}

/// Indices into `file_ids`, the long poles first: most name and index
/// expressions bound, where whole-file inference spends its time, then largest
/// source, for files not bound yet. Ties break on the index, keeping the order
/// a pure function of the input.
pub(crate) fn dispatch_order(db: &DbIndex, file_ids: &[FileId]) -> Vec<usize> {
    let vfs = db.get_vfs();
    let flow_index = db.get_flow_index();
    let mut order: Vec<usize> = (0..file_ids.len()).collect();
    order.sort_by_cached_key(|&idx| {
        let file_id = &file_ids[idx];
        let bindings = flow_index
            .get_flow_tree(file_id)
            .map_or(0, |flow_tree| flow_tree.binding_count());
        let size = vfs.get_file_content(file_id).map_or(0, |text| text.len());
        (std::cmp::Reverse(bindings), std::cmp::Reverse(size), idx)
    });
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_analysis_thread_runs_every_batch_on_the_caller() {
        let mut ws = crate::VirtualWorkspace::new();
        let file_ids = (0..MIN_PARALLEL_FILES * 2)
            .map(|index| ws.def_file(&format!("lua/autorun/f{index}.lua"), "local a = 1"))
            .collect::<Vec<_>>();
        let caller = std::thread::current().id();
        let _threads = THREADS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        set_analysis_threads(1);
        let threads = map_files_collect(ws.get_db_mut(), &file_ids, |_, _| {
            std::thread::current().id()
        });
        set_analysis_threads(0);
        assert!(threads.iter().all(|thread| *thread == caller));
    }
}
