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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, Once};

use rayon::prelude::*;

use crate::db_index::DbIndex;
use crate::db_index::derivation::DerivationId;
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
/// before the first analysis is created, which sizes it.
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

thread_local! {
    /// The files whose groups the superstep task on this thread runs.
    static TASK_FILES: std::cell::RefCell<Option<Vec<FileId>>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f` as the superstep task over `files`: batches it reaches run on
/// this thread, so no other task's work interleaves with it on a worker
/// blocked in a nested join.
pub(crate) fn in_task<T>(files: &[FileId], f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Vec<FileId>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TASK_FILES.with(|task| *task.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(TASK_FILES.with(|task| task.borrow_mut().replace(files.to_vec())));
    f()
}

/// Whether a superstep task runs on this thread.
pub(crate) fn task_running() -> bool {
    TASK_FILES.with(|task| task.borrow().is_some())
}

/// Whether the superstep task running on this thread covers `file_id`.
pub(crate) fn task_covers(file_id: FileId) -> bool {
    TASK_FILES.with(|task| {
        task.borrow()
            .as_ref()
            .is_some_and(|files| files.contains(&file_id))
    })
}

/// Whether this batch is better run on the caller's thread than handed to the
/// pool: too few files to pay the dispatch, a superstep task running, or
/// nothing to run in parallel on.
///
/// The pool's size is [`init_pool`]'s business; this only decides whether to
/// reach for it at all.
pub(crate) fn runs_inline(file_count: usize) -> bool {
    file_count < MIN_PARALLEL_FILES || runs_on_caller(file_count)
}

/// [`runs_inline`] for work each of whose files costs far more than handing
/// it to the pool: only a single file, a superstep task running, or nothing
/// to run in parallel on keeps it on the caller's thread.
fn runs_on_caller(file_count: usize) -> bool {
    // An evaluation records its reads on its own thread; work handed to a
    // pool worker would read unrecorded.
    file_count < 2
        || crate::db_index::derivation::recording()
        || task_running()
        || analysis_threads() <= 1
}

/// Size rayon's global pool to [`analysis_threads`], and give its workers the
/// stack analysis needs.
///
/// Workers run the same recursive inference and type-graph walks the caller's
/// thread does, so they get [`crate::ANALYSIS_STACK_SIZE`] rather than the
/// default. `build_global` fails if something already initialized the pool, in
/// which case the workers are not ours to size.
pub(crate) fn init_pool() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        if let Err(error) = rayon::ThreadPoolBuilder::new()
            .num_threads(analysis_threads())
            .stack_size(crate::ANALYSIS_STACK_SIZE)
            .build_global()
        {
            log::warn!("analysis runs on a thread pool it did not size: {error}");
        }
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
    if runs_inline(file_ids.len()) {
        return file_ids.iter().map(|&id| f(db, id)).collect();
    }
    map_on_pool(db, file_ids, dispatch_order(db, file_ids), f)
}

/// [`map_files_collect`] for derivations: several may belong to one file, and
/// each is computed on its own. Results align with `ids`.
pub(crate) fn map_derivations_collect<T, F>(db: &DbIndex, ids: &[DerivationId], f: F) -> Vec<T>
where
    T: Send,
    F: Fn(&DbIndex, DerivationId) -> T + Sync,
{
    if runs_inline(ids.len()) {
        return ids.iter().map(|&id| f(db, id)).collect();
    }
    let files = ids.iter().map(|id| id.file_id).collect::<Vec<_>>();
    map_on_pool(db, ids, dispatch_order(db, &files), f)
}

/// [`map_files_collect`] for superstep tasks, which each cost far more than
/// the dispatch: two files already run on the pool, the costliest first by
/// `cost`, each file's estimate, when there is one.
pub(crate) fn map_tasks_collect<T, F>(
    db: &DbIndex,
    file_ids: &[FileId],
    cost: Option<Vec<f64>>,
    f: F,
) -> Vec<T>
where
    T: Send,
    F: Fn(&DbIndex, FileId) -> T + Sync,
{
    if runs_on_caller(file_ids.len()) {
        return file_ids.iter().map(|&id| f(db, id)).collect();
    }
    let dispatch = match cost {
        Some(cost) => {
            let mut order = (0..file_ids.len()).collect::<Vec<_>>();
            order.sort_by(|left, right| cost[*right].total_cmp(&cost[*left]).then(left.cmp(right)));
            order
        }
        None => dispatch_order(db, file_ids),
    };
    map_on_pool(db, file_ids, dispatch, f)
}

fn map_on_pool<I, T, F>(db: &DbIndex, items: &[I], dispatch: Vec<usize>, f: F) -> Vec<T>
where
    I: Copy + Sync,
    T: Send,
    F: Fn(&DbIndex, I) -> T + Sync,
{
    let n = items.len();

    let _p = Profile::cond_new("parallel map_files", n > 1);
    init_pool();

    // Longest-processing-time-first dispatch. Per-file cost spans orders of
    // magnitude, so feeding work in slice order let a large file drawn last run
    // alone while every other worker idled. Only the order in which files are
    // picked up changes; each result carries its own index, so callers see the
    // same index-aligned `Vec` as before.
    let report_step = if crate::progress::is_active() {
        (n / 50).max(1)
    } else {
        0
    };
    let done = AtomicUsize::new(0);

    // Every worker, the calling thread among them, takes the next file in
    // dispatch order as it frees up, so the largest files start first: a
    // splitting `par_iter` would hand each worker a contiguous run of them.
    let computed = Mutex::new(Vec::with_capacity(n));
    let drain = || {
        let mut local = Vec::new();
        loop {
            let seq = done.fetch_add(1, Ordering::Relaxed);
            let Some(&idx) = dispatch.get(seq) else {
                break;
            };
            if report_step != 0 && seq.is_multiple_of(report_step) {
                crate::progress::advance_current_phase(seq, n, "files");
            }
            local.push((idx, f(db, items[idx])));
        }
        computed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .append(&mut local);
    };
    rayon::in_place_scope(|scope| {
        for _ in 1..analysis_threads().min(n) {
            scope.spawn(|_| drain());
        }
        drain();
    });
    let mut computed = computed
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

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

/// Run `f` for every item concurrently and collect the results into a `Vec`
/// aligned with `items`, as part of the evaluation recording on this thread:
/// each chunk of items records its reads on its own, and they are filed with
/// the evaluation in item order. Runs on the caller's thread when no
/// evaluation records, a superstep task runs or the batch is too small to pay
/// the dispatch.
pub(crate) fn map_items_recorded<T, R>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R>
where
    T: Sync,
    R: Send,
{
    let Some(id) = crate::db_index::derivation::current() else {
        return items.iter().map(f).collect();
    };
    if items.len() < MIN_PARALLEL_FILES || analysis_threads() <= 1 || task_running() {
        return items.iter().map(f).collect();
    }

    let _p = Profile::cond_new("parallel map_items_recorded", items.len() > 1);
    init_pool();
    let chunk_len = items.len().div_ceil(analysis_threads() * 4);
    let mut chunks = Vec::new();
    rayon::in_place_scope(|_| {
        chunks = items
            .par_chunks(chunk_len)
            .map(|chunk| {
                crate::db_index::derivation::detached(id, || {
                    chunk.iter().map(&f).collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
    });
    let mut results = Vec::with_capacity(items.len());
    for (chunk, reads) in chunks {
        crate::db_index::derivation::note_reads(reads);
        results.extend(chunk);
    }
    results
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

    /// Runs this test binary's `test` alone in a fresh process, where rayon's
    /// global pool is not yet built, with `GLUALS_POOL_PROBE` set to `threads`.
    fn probe_in_fresh_process(test: &str, threads: usize) {
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([test, "--exact", "--test-threads=1"])
            .env("GLUALS_POOL_PROBE", threads.to_string())
            .output()
            .expect("the probe runs");
        assert!(
            output.status.success(),
            "{test} with {threads} thread(s) failed:
{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    /// The thread count the probe asks for, when this runs as one.
    fn probed_threads() -> Option<usize> {
        std::env::var("GLUALS_POOL_PROBE").ok()?.parse().ok()
    }

    /// The pool takes the requested size even when the first update is too
    /// small for the batched passes, which size it, and reaches the raw
    /// parallel iterators first.
    #[test]
    fn a_small_first_update_runs_on_the_requested_pool() {
        if let Some(threads) = probed_threads() {
            set_analysis_threads(threads);
            let mut ws = crate::VirtualWorkspace::new();
            ws.def_file("lua/autorun/sh_one.lua", "local one = 1");
            assert_eq!(rayon::current_num_threads(), threads);
            return;
        }
        for threads in [2, 3] {
            probe_in_fresh_process(
                "compilation::analyzer::parallel::tests::a_small_first_update_runs_on_the_requested_pool",
                threads,
            );
        }
    }

    #[test]
    fn a_large_first_update_runs_on_the_requested_pool() {
        if let Some(threads) = probed_threads() {
            set_analysis_threads(threads);
            let mut ws = crate::VirtualWorkspace::new();
            ws.def_files(
                (0..MIN_PARALLEL_FILES * 2)
                    .map(|index| (format!("lua/autorun/f{index}.lua"), "local a = 1"))
                    .collect::<Vec<_>>()
                    .iter()
                    .map(|(path, text)| (path.as_str(), *text))
                    .collect(),
            );
            assert_eq!(rayon::current_num_threads(), threads);
            return;
        }
        probe_in_fresh_process(
            "compilation::analyzer::parallel::tests::a_large_first_update_runs_on_the_requested_pool",
            3,
        );
    }
}
