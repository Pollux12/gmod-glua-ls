//! Workspace diagnostic determinism harness.
//!
//! Answers one question: does re-analysing a workspace produce the same
//! diagnostics as building it from scratch? It indexes a workspace exactly like
//! `init_analysis` does, then re-analyses it in various ways and diffs both the
//! diagnostic sets and the derived indexes they are read from.
//!
//! The stages are ordered by how much they re-analyse, which localises a
//! divergence: `allreindex` matching cold while `mainreindex` does not puts the
//! gap in which files a partial re-index covers; both diverging puts it in
//! per-file removal.
//!
//! # Gates and bisect stages
//!
//! Gates: `repeat`, `fresh`, `order`, `reindex`, `allreindex`, `mainexpand`,
//! `noopedit`, `realedit`, `editrevert`, `indexrepeat`, `burst`. Each runs a
//! path the language server takes, or a ground-truth rebuild. Every one must
//! report `+0` diagnostics and `+0` index.
//!
//! `noopedit` and `realedit` gate different halves of editing. `noopedit` gates
//! the semantic no-op skip; its edit pair is what that gate rejects, so it
//! cannot reach re-analysis. `realedit`'s edit changes what the file means, so
//! it gates re-analysis against a cold build of the edited source.
//!
//! **No stage is expected to diverge**: a non-zero result is a defect in the
//! analyzer, never an accepted outcome. The bisect stages exist to localise one
//! when it happens. A non-identical comparison anywhere in a run makes the
//! process exit non-zero, so the verdict does not depend on someone reading the
//! log. `mainreindex`, `exact`, `editmid` and `split:N` must reach
//! `+0` like the gates — a divergence there is a bug, and where it lands says
//! which mechanism feeds it: `mainreindex` and `exact` run
//! `reindex_files_without_expansion`, which skips three convergence passes
//! production performs (`refresh_file_source_dependencies`,
//! `reindex_changed_inferred_guard_references`,
//! `reindex_changed_inferred_param_consumers`), so a divergence there means a
//! fact depends on those passes rather than on the file's text; `editmid`
//! shifts every offset with a semantically neutral edit pair, so a divergence
//! there means a stored fact depends on raw positions; `split:N` means it
//! depends on batch composition.
//!
//! `restabilize` removes and re-indexes every file three times through
//! `reindex_files_without_expansion`, diffing each round against the
//! immutable cold baseline: it characterises how far the retained-state
//! path is from a fixpoint, and is not a licence to leave it that way.
//!
//! Example:
//!   DET_CODEBASE=/path/to/addon DET_ANNOTATIONS=/path/to/annotations/output \
//!     DET_STAGES=repeat,noopedit,mainexpand DET_TARGETS=lua/autorun/init.lua \
//!     cargo run --release -p determinism
//!
//! Env vars:
//!   DET_TARGETS     comma separated workspace-relative paths to no-op edit
//!   DET_STAGES      comma separated subset of:
//!                     shiftsettle single shift-only edit (blank line at the
//!                               front) on each DET_TARGETS entry through the
//!                               production path (`update_file_text_only` +
//!                               `self_index_and_diff` + `ripple`), with the
//!                               dirty set broken down by source (seed /
//!                               remap-loss / diff-driven), plus the export
//!                               diff verdict and the textual-refresh names.
//!                               Bisects the RC-2 attribution: a shift-only
//!                               edit must produce an empty diff and no dirty
//!                               files.
//!                     repeat    collect diagnostics twice from one isolated
//!                               analysis with no intervening update: a
//!                               read-side mutation or cache-dependent second
//!                               result is a divergence
//!                     noopedit  EOF-newline edit pair on each DET_TARGETS
//!                               entry, through the same `update_file_by_uri`
//!                               path the LSP uses. The pair is semantically
//!                               unchanged, which is precisely what this stage
//!                               gates: the no-op gate must SKIP the re-index
//!                               and skipping must preserve state. It does not
//!                               exercise re-analysis — `realedit` gates that,
//!                               and `editmid` bisects it.
//!                     indexrepeat
//!                               re-index each DET_TARGETS entry with its text
//!                               untouched and require the INDEX to come back
//!                               identical. Catches drift the diagnostic gates
//!                               cannot see. Honours DET_INDEXREPEAT_ROUNDS
//!                               (default 1). Listed last in the default set: it
//!                               re-indexes in place, so an in-place stage after
//!                               it inherits that warm state.
//!                     editmid   offset-shifting no-op edit pair (newline at the
//!                               front of the file, then removed): the semantic
//!                               no-op gate cannot fire, so both edits run the
//!                               full re-index expansion. Bisect stage for the
//!                               batch-composition convergence gap: it must come
//!                               back `+0` like every other stage, and a
//!                               divergence here means a stored fact depends on
//!                               raw offsets rather than on the text. Run it
//!                               ALONE (or last): it leaves diverged warm state
//!                               behind, so any in-place stage listed after it
//!                               inherits that state and its diff is
//!                               meaningless. The index diff compares the cold
//!                               snapshot against the state each target leaves
//!                               behind.
//!                     editrevert
//!                               apply a real edit through the single-file
//!                               update path and then take it back out. The
//!                               source ends where it started, so the INDEX and
//!                               the diagnostics have to as well — a difference
//!                               is drift the edit path introduced, not a fact
//!                               about the code. Needs DET_EDIT_FIND; without it
//!                               the stage skips. Runs before `realedit`, which
//!                               leaves the edited file re-indexed behind it.
//!                               Covers what the other stages cannot:
//!                               `indexrepeat` re-indexes with the text
//!                               untouched and so never exercises an edit's
//!                               invalidation, and `noopedit`'s pair is
//!                               semantically neutral, so the update path skips
//!                               the re-index outright.
//!                     realedit  apply a real edit (DET_EDIT_FIND replaced by
//!                               DET_EDIT_REPLACE) to each DET_TARGETS entry and
//!                               compare the incremental result against a cold
//!                               build of the same edited source. The only stage
//!                               whose edit changes what the file means, so it is
//!                               the only one that measures how much of the index
//!                               an edit actually moves — which bounds what a
//!                               cheaper re-index mechanism has to reproduce.
//!                               Always diffs the index.
//!                               Needs DET_EDIT_FIND; without it the stage
//!                               skips loudly instead of gating anything.
//!                     blast     how many files ONE edit to each DET_TARGETS
//!                               entry dirties, gated against DET_BLAST_MAX
//!                               (default 50). The only stage that measures the
//!                               work an edit costs rather than the answer it
//!                               produces, and the only one that exits non-zero:
//!                               every result gate stays green when a settle
//!                               path re-analyses the whole workspace to reach
//!                               the right answer. Needs DET_EDIT_FIND.
//!                     burst     three edits per DET_TARGETS entry, each one
//!                               self-indexed on its own with its dirty set
//!                               accumulated, then ONE ripple over the union —
//!                               exactly what `DebouncedAnalysis::run` holds in
//!                               `owed_ripple` when a typing burst defers the
//!                               ripple behind its idle timer. Gates the settled
//!                               result against a cold build of the final text,
//!                               and reports `control` (the same edits settled
//!                               one at a time) beside it so a failure separates
//!                               the deferral from the path underneath it.
//!                               Needs DET_EDIT_FIND; without it the stage
//!                               skips loudly instead of gating anything.
//!                     exact     reindex DET_TARGETS with no text change and no
//!                               dependency expansion (bisects which file's
//!                               re-analysis perturbs a fact)
//!                     perfile   remove and re-add every file ON ITS OWN, so
//!                               each one is re-derived against the complete
//!                               settled workspace instead of the prefix the
//!                               cold walk had built when it reached that file.
//!                               Says whether the cold answer is simply one
//!                               computed from an incomplete view. It is, and it
//!                               converges: on CityRP round 1 moves 4277 type
//!                               caches away from cold and round 2 moves
//!                               nothing. Honours DET_PERFILE_ROUNDS (default
//!                               2). Slow -- ~830s a round -- because every file
//!                               pays the whole pipeline.
//!                     delete    remove each DET_TARGETS entry through
//!                               `update_file_by_uri` and compare both the
//!                               diagnostics and the index against a cold
//!                               build that never contained the file
//!                     reopen    same stage, second half: put the file's text
//!                               back and compare against the ordinary cold
//!                               build of the full workspace. Either name
//!                               runs both halves
//!                     reindex   full clear + rebuild, the ground truth
//!                     order     rebuild with the file list reversed
//!                     split:N   rebuild in N batches instead of one
//!                     batchexpand  re-walk each DET_TARGETS entry's expansion
//!                               in one `reindex_expanded_files` batch with the
//!                               text untouched. Localises sites that flip when
//!                               re-derived against a settled retained
//!                               neighborhood, without the edit signal `burst`
//!                               carries
//!                     mainreindex  re-analyse every main-workspace file at
//!                               once, deliberately *without* the dependency
//!                               expansion
//!                     mainexpand   same set, but through `self_index_and_diff`
//!                               + `ripple`, i.e. the two-phase edit path the
//!                               LSP actually applies. Phase 1 re-indexes the
//!                               set; phase 2 re-analyses only the files whose
//!                               facts the diff says changed.
//!                     allreindex   re-analyse every file, library included,
//!                               via per-file removal rather than `clear_index`
//!                     restabilize  remove and re-index every file three
//!                               times through
//!                               `reindex_files_without_expansion`, diffing
//!                               each round against the immutable cold
//!                               baseline. This is the only stage that gives
//!                               a full build the same retained state a
//!                               partial re-index inherits, repeatedly: a
//!                               round that still differs from cold shows
//!                               the retained-state path is non-confluent,
//!                               and a diff that keeps moving between rounds
//!                               shows it has not reached a fixpoint
//!                     fresh     build a second analysis in-process
//!   Index diff    every stage diffs every index reachable from `DbIndex`,
//!                   unconditionally: type caches, members, signatures, class
//!                   members, super types, net flows, inferred params, flow
//!                   trees, signature casts, special-call effects/facts/write
//!                   sites, decls, decl references, type-decl effective
//!                   payloads (kind, canonical locations, alias origin, enum
//!                   base/flatness, attribute type), generic params and
//!                   per-site alias contributions,
//!                   modules, globals, diagnostics (and their disable
//!                   actions), operators,
//!                   dependency sites, gmod load/hook/system/realm/
//!                   scoped-class/network/class metadata, dynamic-field
//!                   wildcards, dynamic-field owner/field maps (with
//!                   unattributed names, finite members and direct field
//!                   definitions), accessor-func
//!                   annotations and calls, numeric-range populations,
//!                   properties (merged and per-source contributions), and
//!                   metatables; the reference index's decl
//!                   and inverse ranges, member/global/string/type maps and
//!                   local reassignments; the call-site index's
//!                   source/signature/path/contribution/consumer/dependency
//!                   relations and parked previous params; and the JSON
//!                   schema index. Not covered: transient deferred
//!                   contribution queues and raw revisions/write
//!                   epochs/counters.
//!   DET_DUMP_INDEX  directory to write each index snapshot to as `<label>.idx`.
//!                   The stage diffs only compare snapshots *within* one process;
//!                   this writes them out so two runs of the same binary on the
//!                   same input can be diffed against each other, which is the
//!                   only way to see a per-process divergence (hash seeds,
//!                   allocation addresses) rather than an ordering one. The
//!                   bulkier indexes (every decl, diagnostic, dependency site,
//!                   ...) are hashed rather than rendered to keep the harness's
//!                   own memory bounded; those write as `<TAG>_HASH <hex>`
//!                   lines unless DET_DUMP_INDEX_DETAIL names them.
//!   DET_DUMP_INDEX_DETAIL  name of one bulky index (the `name` string used in
//!                   its diff, e.g. `decls` or `dependency_sites`) to keep full
//!                   text for, so it can still be inspected entry-by-entry
//!                   while every other bulky index stays hash-only.
//!   DET_SHOW_EXPANSION  print the reindex expansion set for each edit
//!   DET_DUMP        write the cold diagnostic set to this path
//!   DET_DUMP_FILE_IDS   print the main-workspace file id table
//!   DET_DUMP_EXPANSION  list every file `indexrepeat` re-indexes, so a drifting
//!                   entry can be told apart from one merely near the batch:
//!                   whether the writer of a fact sits inside or outside it is
//!                   what decides whether the reader saw it settled
//!   DET_DUMP_CLASS  print the member list of this class at each snapshot
//!   DET_LIMIT       max diff lines printed per bucket (default 40)
//!   DET_FILTER      substring an index-diff entry line must contain to be
//!                   printed. Bucket headers still print in full. Setting it
//!                   lifts DET_LIMIT: the filter is the limit, so a match never
//!                   hides behind the first 40 unrelated entries.

use mimalloc::MiMalloc;
use std::alloc::{GlobalAlloc, Layout};

/// mimalloc, plus a count of every allocation it hands out.
///
/// A sampling profiler blames the allocator rather than the caller.
/// `GLUALS_PROFILE=1` prints allocation counts alongside each phase's cost.
struct CountingMiMalloc;

/// Sample one in every `DET_ALLOC_SAMPLE` allocations and record its source
/// location.
mod alloc_sample {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    const MAX_FRAMES: usize = 64;

    // Documented in WinBase.h; returns the number of frames written.
    #[cfg(windows)]
    unsafe extern "system" {
        fn RtlCaptureStackBackTrace(
            frames_to_skip: u32,
            frames_to_capture: u32,
            back_trace: *mut *mut std::ffi::c_void,
            back_trace_hash: *mut u32,
        ) -> u16;
    }

    /// Raw instruction pointers for the current stack, innermost first. Returns
    /// how many of `buffer` were filled.
    ///
    /// Windows uses `RtlCaptureStackBackTrace`, which unwinds via the x64
    /// unwind tables. `backtrace::trace` goes through dbghelp's StackWalkEx
    /// there, taking a process-wide lock and costing milliseconds per capture.
    #[cfg(windows)]
    fn capture(buffer: &mut [*mut std::ffi::c_void]) -> usize {
        let captured = unsafe {
            RtlCaptureStackBackTrace(
                1,
                buffer.len() as u32,
                buffer.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        captured as usize
    }

    #[cfg(not(windows))]
    fn capture(buffer: &mut [*mut std::ffi::c_void]) -> usize {
        let mut filled = 0;
        backtrace::trace(|frame| {
            if filled >= buffer.len() {
                return false;
            }
            buffer[filled] = frame.ip();
            filled += 1;
            true
        });
        filled
    }

    static SAMPLE_RATE: AtomicUsize = AtomicUsize::new(0);
    static PHASE_SCOPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    static TICK: AtomicU64 = AtomicU64::new(0);
    /// Frame address -> number of sampled allocations whose stack contained it.
    /// Addresses are resolved to names once, at report time.
    static FRAMES: Mutex<Option<HashMap<usize, u64>>> = Mutex::new(None);
    /// Ordered stacks (innermost first) -> count. Kept alongside FRAMES, whose
    /// flat per-frame tally throws frame order away.
    type StackCounts = HashMap<Box<[usize]>, u64>;
    static STACKS: Mutex<Option<StackCounts>> = Mutex::new(None);

    thread_local! {
        /// Capturing a backtrace allocates; without this guard the sampler
        /// would recurse into itself.
        static SAMPLING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub fn init() {
        let rate = std::env::var("DET_ALLOC_SAMPLE")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        SAMPLE_RATE.store(rate, Ordering::Relaxed);
        PHASE_SCOPED.store(
            std::env::var_os("GLUALS_PROFILE_SAMPLE").is_some(),
            Ordering::Relaxed,
        );
    }

    #[inline]
    pub fn maybe_sample() {
        let rate = SAMPLE_RATE.load(Ordering::Relaxed);
        if rate == 0 {
            return;
        }
        // Sample one phase only (GLUALS_PROFILE_SAMPLE), when asked to.
        if PHASE_SCOPED.load(Ordering::Relaxed)
            && !glua_code_analysis::profile::sample_phase_active()
        {
            return;
        }
        if !TICK
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(rate as u64)
        {
            return;
        }
        SAMPLING.with(|sampling| {
            if sampling.get() {
                return;
            }
            sampling.set(true);
            // Raw instruction pointers only — no symbol resolution here.
            let mut buffer = [std::ptr::null_mut::<std::ffi::c_void>(); MAX_FRAMES];
            let captured = capture(&mut buffer);
            let mut ips: Vec<usize> = buffer[..captured].iter().map(|&ip| ip as usize).collect();
            {
                let mut stacks = STACKS.lock().unwrap_or_else(|p| p.into_inner());
                *stacks
                    .get_or_insert_with(HashMap::new)
                    .entry(ips.as_slice().into())
                    .or_insert(0) += 1;
            }
            let mut frames = FRAMES.lock().unwrap_or_else(|p| p.into_inner());
            let frames = frames.get_or_insert_with(HashMap::new);
            // Count each frame once per sample, so the number reads as "share of
            // sampled allocations made underneath this function".
            ips.sort_unstable();
            ips.dedup();
            for ip in ips {
                *frames.entry(ip).or_insert(0) += 1;
            }
            sampling.set(false);
        });
    }

    /// Attribute each sampled allocation to the innermost frame in our own
    /// crates. The inclusive tally says an allocation happened somewhere under a
    /// function; this says which function asked for it.
    fn nearest_caller_report(top: usize) {
        let stacks = STACKS.lock().unwrap_or_else(|p| p.into_inner());
        let Some(stacks) = stacks.as_ref() else {
            return;
        };
        let total: u64 = stacks.values().sum();
        if total == 0 {
            return;
        }

        let mut names: HashMap<usize, Option<String>> = HashMap::new();
        let mut resolve = |ip: usize| -> Option<String> {
            names
                .entry(ip)
                .or_insert_with(|| {
                    let mut found = None;
                    backtrace::resolve(ip as *mut _, |symbol| {
                        if found.is_none() {
                            found = symbol.name().map(|name| name.to_string());
                        }
                    });
                    found
                })
                .clone()
        };

        let mut by_caller: HashMap<String, u64> = HashMap::new();
        for (stack, count) in stacks {
            let owner = stack.iter().find_map(|&ip| {
                let name = resolve(ip)?;
                (name.starts_with("glua_") && !name.contains("::profile::")).then_some(name)
            });
            let owner = owner.unwrap_or_else(|| "<no glua frame>".to_string());
            let owner = owner
                .rsplit_once("::h")
                .filter(|(_, hash)| hash.len() == 16)
                .map_or(owner.as_str(), |(head, _)| head)
                .to_string();
            *by_caller.entry(owner).or_insert(0) += count;
        }

        let mut rows: Vec<_> = by_caller.into_iter().collect();
        rows.sort_unstable_by_key(|(_, count)| std::cmp::Reverse(*count));
        eprintln!("\n=== allocations by nearest owning function ({total} samples) ===");
        for (name, count) in rows.into_iter().take(top) {
            eprintln!("{:>6.2}%  {name}", (count as f64 / total as f64) * 100.0);
        }
    }

    /// Print the functions that appear most often across sampled allocations.
    pub fn report(top: usize) {
        // Symbol resolution allocates, so a sample taken while reporting would
        // re-enter and block on a lock this thread already holds.
        SAMPLE_RATE.store(0, Ordering::Relaxed);

        let frames = FRAMES.lock().unwrap_or_else(|p| p.into_inner());
        let Some(frames) = frames.as_ref() else {
            return;
        };
        // Samples, not frame hits: the most-hit frame is the allocator entry,
        // which every sample passes through.
        let total = frames.values().copied().max().unwrap_or(0);
        if total == 0 {
            return;
        }

        // Resolve once, then fold the per-address counts into per-function ones
        // (a function inlined or spread over several addresses is one entry).
        let mut by_name: HashMap<String, u64> = HashMap::new();
        for (&ip, &count) in frames {
            backtrace::resolve(ip as *mut _, |symbol| {
                let Some(name) = symbol.name() else { return };
                let name = name.to_string();
                // Strip the trailing hash rustc appends to monomorphized names.
                let name = name
                    .rsplit_once("::h")
                    .filter(|(_, hash)| hash.len() == 16)
                    .map_or(name.as_str(), |(head, _)| head)
                    .to_string();
                *by_name.entry(name).or_insert(0) += count;
            });
        }

        nearest_caller_report(top);

        let mut rows: Vec<_> = by_name
            .into_iter()
            .filter(|(name, _)| {
                !name.starts_with("core::")
                    && !name.starts_with("alloc::")
                    && !name.starts_with("std::")
                    && !name.contains("hashbrown")
                    && !name.contains("mi_")
                    && !name.contains("CountingMiMalloc")
                    && !name.contains("alloc_sample")
            })
            .collect();
        rows.sort_unstable_by_key(|(_, count)| std::cmp::Reverse(*count));
        eprintln!("\n=== sampled allocation frames ({total} samples) ===");
        eprintln!("share of sampled allocations made underneath each function:");
        for (name, count) in rows.into_iter().take(top) {
            eprintln!("{:>6.2}%  {name}", (count as f64 / total as f64) * 100.0);
        }
    }
}

// SAFETY: every method forwards to MiMalloc with the same arguments; the
// counters are plain relaxed atomics and do not affect allocation behavior.
unsafe impl GlobalAlloc for CountingMiMalloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        glua_code_analysis::profile::record_alloc(layout.size());
        alloc_sample::maybe_sample();
        unsafe { MiMalloc.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { MiMalloc.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        glua_code_analysis::profile::record_alloc(layout.size());
        unsafe { MiMalloc.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        glua_code_analysis::profile::record_alloc(new_size);
        unsafe { MiMalloc.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingMiMalloc = CountingMiMalloc;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use glua_code_analysis::{
    DirtySet, EmmyLuaAnalysis, Emmyrc, FileId, WorkspaceFolder, collect_workspace_files,
    load_configs,
};
use lsp_types::{DiagnosticSeverity, NumberOrString};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Snapshot {
    file: String,
    code: String,
    severity: u8,
    line: u32,
    character: u32,
    end_line: u32,
    end_character: u32,
    message: String,
}

impl Snapshot {
    fn render(&self) -> String {
        format!(
            "{sev} [{code}] {file}:{line}:{ch}-{eline}:{ech} {msg}",
            sev = match self.severity {
                1 => "ERROR",
                2 => "WARN ",
                3 => "INFO ",
                _ => "HINT ",
            },
            code = self.code,
            file = self.file,
            line = self.line + 1,
            ch = self.character + 1,
            eline = self.end_line + 1,
            ech = self.end_character + 1,
            msg = self.message.replace(['\r', '\n'], " "),
        )
    }
}

fn discover_config_files(root: &Path) -> Vec<PathBuf> {
    let gluarc = root.join(".gluarc.json");
    if gluarc.exists() {
        return vec![gluarc];
    }
    [
        root.join(".luarc.json"),
        root.join(".emmyrc.json"),
        root.join(".emmyrc.lua"),
    ]
    .into_iter()
    .filter(|path| path.exists())
    .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    Natural,
    Reverse,
}

fn build_analysis(codebase: &Path, annotations: &Path) -> EmmyLuaAnalysis {
    build_analysis_with(codebase, annotations, Order::Natural, 1, &[], None)
}

/// Cold build of a workspace that never contained `exclude`: the file is
/// dropped from the collected list before indexing, which is not the same
/// state as indexing it and then removing it.
fn build_analysis_excluding(
    codebase: &Path,
    annotations: &Path,
    exclude: &Path,
) -> EmmyLuaAnalysis {
    build_analysis_with(codebase, annotations, Order::Natural, 1, &[], Some(exclude))
}

/// Comparison key for a workspace file path. `DET_TARGETS` is written by hand,
/// so it has to match the loader's paths regardless of separator or case.
fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

/// Ordering key `update_files_by_uri` sorts on before assigning file ids:
/// the path round-tripped through its URI, normalized like
/// `normalize_path_for_ordering` (which is crate-private). The reservation
/// pass in `build_analysis_with` has to allocate ids in exactly this order
/// or every file's id shifts.
fn uri_order_key(path: &Path) -> String {
    let Some(uri) = glua_code_analysis::file_path_to_uri(&path.to_path_buf()) else {
        return String::new();
    };
    let mut normalized = glua_code_analysis::uri_to_file_path(&uri)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| uri.as_str().to_string());
    while normalized.ends_with('/') {
        normalized.pop();
    }
    #[cfg(target_os = "windows")]
    {
        normalized = normalized.to_lowercase();
    }
    normalized
}

fn build_analysis_with(
    codebase: &Path,
    annotations: &Path,
    order: Order,
    batches: usize,
    overrides: &[(PathBuf, String)],
    exclude: Option<&Path>,
) -> EmmyLuaAnalysis {
    let config_files = discover_config_files(codebase);
    let mut emmyrc = if config_files.is_empty() {
        Emmyrc::default()
    } else {
        load_configs(config_files, None)
    };
    emmyrc.gmod.enabled = true;
    emmyrc.pre_process_emmyrc(codebase);

    let mut analysis = EmmyLuaAnalysis::new();
    analysis.update_config(Arc::new(emmyrc.clone()));
    analysis.init_std_lib();
    analysis.add_library_workspace(annotations.to_path_buf());
    analysis.add_main_workspace(codebase.to_path_buf());

    let mut workspace_folders = vec![
        WorkspaceFolder::new(annotations.to_path_buf(), true),
        WorkspaceFolder::new(codebase.to_path_buf(), false),
    ];
    for lib in &emmyrc.workspace.library {
        let path = PathBuf::from(lib.get_path().clone());
        if path != annotations {
            analysis.add_library_workspace(path.clone());
            workspace_folders.push(WorkspaceFolder::new(path, true));
        }
    }

    let mut files = collect_workspace_files(&workspace_folders, &analysis.emmyrc, None, None)
        .into_iter()
        .filter(|file| !file.path.ends_with(".editorconfig"))
        .map(|file| file.into_tuple())
        .collect::<Vec<_>>();
    // An exclusion keeps its slot in the file list with `None` content: the
    // reservation pass below gives it the file id the full build would have
    // assigned, so every other file keeps the id a warm delete leaves behind
    // and the cold-absent snapshot's embedded ids compare equal.
    let mut excluded = None;
    if let Some(exclude) = exclude {
        let want = path_key(exclude);
        let mut matched = 0;
        for file in files.iter_mut() {
            if path_key(&file.0) == want {
                file.1 = None;
                excluded = Some(file.0.clone());
                matched += 1;
            }
        }
        // A silently unapplied exclusion would make this build a copy of the
        // full baseline, and every diff taken against it would read as
        // "identical".
        assert_eq!(
            matched, 1,
            "excluded path did not match exactly one collected workspace file"
        );
    }
    if order == Order::Reverse {
        files.reverse();
    }
    if !overrides.is_empty() {
        let mut applied = 0;
        for (path, content) in overrides {
            let want = path_key(path);
            for file in files.iter_mut() {
                if path_key(&file.0) == want {
                    file.1 = Some(content.clone());
                    applied += 1;
                }
            }
        }
        // A silently unapplied override would make this build a copy of the
        // baseline, and every diff taken against it would read as "identical".
        assert_eq!(
            applied,
            overrides.len(),
            "content override did not match a collected workspace file"
        );
        eprintln!("[setup] applied {applied} content override(s)");
    }
    let t = Instant::now();
    if let Some(excluded_path) = excluded {
        // Reserve every file id in `uri_order_key` order — the same order
        // `update_files_by_uri` hands them out — via `set_file_content(uri,
        // None)`, which allocates the path→id entry without parsing or
        // indexing. The excluded file stays an unindexed tombstone, matching
        // the VFS state a warm `update_file_by_uri(uri, None)` leaves behind,
        // so embedded file ids compare equal between the two baselines.
        let mut ordered = files.clone();
        ordered.sort_by_cached_key(|(path, _)| uri_order_key(path));
        for (path, _) in &ordered {
            let Some(uri) = glua_code_analysis::file_path_to_uri(path) else {
                continue;
            };
            analysis
                .compilation
                .get_db_mut()
                .get_vfs_mut()
                .set_file_content(&uri, None);
        }
        // One ordinary batch over the surviving files; the tombstone is not
        // in it, so the excluded file is never indexed.
        let surviving = files
            .into_iter()
            .filter(|(path, _)| path != &excluded_path)
            .collect();
        analysis.update_files_by_path(surviving);
    } else if batches <= 1 {
        analysis.update_files_by_path(files);
    } else {
        let chunk = files.len().div_ceil(batches);
        for group in files.chunks(chunk) {
            analysis.update_files_by_path(group.to_vec());
        }
    }

    eprintln!("[setup] indexed in {:.2}s", t.elapsed().as_secs_f64());
    if std::env::var_os("DET_DUMP_FILE_IDS").is_some() {
        for file_id in analysis.get_main_workspace_file_ids_for_diagnostics() {
            eprintln!("[fileid] {} {}", file_id.id, file_label(&analysis, file_id));
        }
    }
    analysis
}

fn collect(analysis: &EmmyLuaAnalysis, label: &str) -> BTreeSet<Snapshot> {
    let t = Instant::now();
    let shared = analysis.precompute_diagnostic_shared_data();
    let file_ids = analysis.get_main_workspace_file_ids_for_diagnostics();
    let mut out = BTreeSet::new();
    for file_id in file_ids {
        let Some(diagnostics) =
            analysis.diagnose_file_with_shared(file_id, CancellationToken::new(), shared.clone())
        else {
            continue;
        };
        if diagnostics.is_empty() {
            continue;
        }
        let file = file_label(analysis, file_id);
        for diagnostic in diagnostics {
            out.insert(Snapshot {
                file: file.clone(),
                code: match &diagnostic.code {
                    Some(NumberOrString::String(code)) => code.clone(),
                    Some(NumberOrString::Number(code)) => code.to_string(),
                    None => "<none>".to_string(),
                },
                severity: match diagnostic.severity.unwrap_or(DiagnosticSeverity::HINT) {
                    DiagnosticSeverity::ERROR => 1,
                    DiagnosticSeverity::WARNING => 2,
                    DiagnosticSeverity::INFORMATION => 3,
                    _ => 4,
                },
                line: diagnostic.range.start.line,
                character: diagnostic.range.start.character,
                end_line: diagnostic.range.end.line,
                end_character: diagnostic.range.end.character,
                message: diagnostic.message,
            });
        }
    }
    let (errors, warnings) = counts(&out);
    eprintln!(
        "[{label}] total={} errors={errors} warnings={warnings} ({:.2}s)",
        out.len(),
        t.elapsed().as_secs_f64()
    );
    out
}

/// Snapshot of the derived index state that diagnostics read from.
struct IndexSnapshot {
    type_caches: BTreeMap<String, String>,
    /// How each cached type was reached. Diffed apart from the type, so a
    /// value that stayed put while its reasoning moved is still visible.
    type_facts: BTreeMap<String, String>,
    /// Where each member is currently homed. Class member lists are keyed off
    /// this.
    member_owners: BTreeMap<String, String>,
    members: BTreeSet<String>,
    net_flows: Vec<String>,
    inferred_params: BTreeMap<String, String>,
    /// Class hierarchy. A dropped `---@class A : B` link breaks inherited-member
    /// lookup, which falls back to a by-name search and can land on a sibling.
    super_types: BTreeMap<String, String>,
    class_members: BTreeMap<String, String>,
    /// Each global path's table literals, sorted. Member ownership is derived
    /// from this, so a site list that moves between two runs is drift the
    /// member set can hide.
    definition_sites: BTreeMap<String, String>,
    signatures: BTreeMap<String, String>,
    flow_trees: BTreeMap<String, String>,
    signature_casts: BTreeMap<String, String>,
    special_call_effects: BTreeMap<String, String>,
    special_call_facts: BTreeMap<String, String>,
    /// Sites where a special-call write was recorded. Only the site set is
    /// snapshotted: the version values are a monotonic epoch, so they differ
    /// between a cold build and any re-index of the same text.
    special_call_write_sites: BTreeMap<String, String>,
    accessor_func_annotations: HashDumpResult,
    accessor_func_annotation_files: HashDumpResult,
    numeric_range_file_populations: HashDumpResult,
    numeric_range_global_populations: HashDumpResult,
    decls: HashDumpResult,
    /// Complete effective `LuaTypeDecl` payload per decl id: name, kind,
    /// canonically ordered locations (file path, range, flags), alias origin,
    /// enum base/flatness and attribute type. `decls` covers syntax decls;
    /// this covers the merged type-level declaration state they feed.
    type_decl_states: HashDumpResult,
    /// Every stored alias contribution (site, origin, generic params,
    /// precedence), not just the effective winner `type_decl_states` shows.
    alias_contributions: HashDumpResult,
    /// Declared generic parameter list per decl id. Order is meaningful
    /// (`fun<T, U>` vs `fun<U, T>`), so the rendered list keeps it.
    type_decl_generic_params: HashDumpResult,
    decl_references: HashDumpResult,
    /// Expression range → decl it refers to: the inverse view of
    /// `decl_references`, which keys the same cells by decl.
    reference_ranges: HashDumpResult,
    /// Member-key reference sites: key → file → syntax ids.
    index_references: HashDumpResult,
    /// Global-name reference sites: name → file → syntax ids.
    global_references: HashDumpResult,
    /// Per-file string literal reference sites.
    string_references: HashDumpResult,
    /// Doc type-name reference sites: file → type decl → ranges.
    type_references: HashDumpResult,
    /// First reassignment position of each local: file → decl → position.
    local_reassignments: HashDumpResult,
    modules: HashDumpResult,
    globals: HashDumpResult,
    diagnostics: HashDumpResult,
    diagnostic_actions: HashDumpResult,
    operators: HashDumpResult,
    dependency_sites: HashDumpResult,
    gmod_load_files: HashDumpResult,
    gmod_load_unresolved_edges: HashDumpResult,
    gmod_hook_files: HashDumpResult,
    gmod_system_files: HashDumpResult,
    gmod_gm_method_realm_annotations: HashDumpResult,
    gmod_realm_files: HashDumpResult,
    gmod_scoped_class_info: HashDumpResult,
    gmod_network_wire_coverage: HashDumpResult,
    gmod_class_file_metadata: HashDumpResult,
    dynamic_field_wildcards: HashDumpResult,
    dynamic_fields: HashDumpResult,
    accessor_func_calls: HashDumpResult,
    properties: HashDumpResult,
    metatables: HashDumpResult,
    metatable_factory_bindings: HashDumpResult,
    /// Source access paths each file declares, with the signature they
    /// resolve to and the parameter indexes that file mutates.
    call_site_source_signatures: HashDumpResult,
    /// Source signatures a re-indexed file had before its removal, parked so
    /// the update can tell which of its signatures merely moved.
    call_site_previous_source_signatures: HashDumpResult,
    /// Access path → current source function signature candidates.
    call_site_signatures_by_path: HashDumpResult,
    /// Signature → parameter indexes mutated by its source function.
    call_site_mutated_params: HashDumpResult,
    /// Observed call-site param evidence contributed by calls in each file.
    call_site_contributions: HashDumpResult,
    /// Files that have supplied call-site evidence for each signature.
    call_site_contributor_files: HashDumpResult,
    /// Signatures of each file that have call-site contributors.
    call_site_contributor_signatures: HashDumpResult,
    /// Facts parked across a remove/re-add so a re-index can tell which of
    /// its signatures merely moved.
    call_site_pending_previous_params: HashDumpResult,
    /// Return consumers recorded by each file's calls.
    call_site_file_return_consumers: HashDumpResult,
    /// Return consumers keyed by the signature whose return they read.
    call_site_return_consumers: HashDumpResult,
    /// Return consumers keyed by the file declaring the consumed signature.
    call_site_return_consumers_by_signature_file: HashDumpResult,
    /// Files owning callback parameters inferred from concrete structural
    /// table values.
    call_site_callback_files: HashDumpResult,
    /// Inference diagnostic events filed under each source file.
    call_site_inference_events: HashDumpResult,
    /// Consumer file → producer sources its call-site inference read.
    call_site_source_dependencies: HashDumpResult,
    /// Producer source → consumer files that read it.
    call_site_source_dependents: HashDumpResult,
    /// Producer file → consumer files that read one of its sources.
    call_site_source_file_dependents: HashDumpResult,
    /// File → workspace path, for source-path dependency lookups.
    call_site_source_paths: HashDumpResult,
    /// Workspace path → consumer files depending on the file at that path.
    call_site_source_path_dependents: HashDumpResult,
    /// Schema URL → resolution state.
    schema_files: HashDumpResult,
}

/// Accumulates one index's entries as sorted hashes rather than owned
/// strings, so a bulky index (every decl, every diagnostic, every dependency
/// site) costs 8 bytes per entry during the in-process diff instead of two
/// full rendered strings. `+N/-N` then comes from a sorted-merge of the two
/// hash vectors: a changed entry is a drop of its old hash plus a gain of its
/// new one, same as the existing members/net_flows set-diffs.
///
/// `DET_DUMP_INDEX_DETAIL=<name>` (matching the `name` passed to `diff_map`
/// in `diff_index`) additionally retains the full text for that one index, so
/// it can still be inspected entry-by-entry while every other index stays
/// hash-only.
struct HashDump {
    hashes: Vec<u64>,
    detail: Option<BTreeMap<String, String>>,
}

struct HashDumpResult {
    hashes: Vec<u64>,
    detail: Option<BTreeMap<String, String>>,
}

impl HashDump {
    fn new(name: &str) -> Self {
        let want_detail = std::env::var("DET_DUMP_INDEX_DETAIL").is_ok_and(|want| want == name);
        Self {
            hashes: Vec::new(),
            detail: want_detail.then(BTreeMap::new),
        }
    }

    fn insert(&mut self, key: String, value: String) {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        value.hash(&mut hasher);
        self.hashes.push(hasher.finish());
        if let Some(detail) = &mut self.detail {
            detail.insert(key, value);
        }
    }

    fn finish(mut self) -> HashDumpResult {
        self.hashes.sort_unstable();
        HashDumpResult {
            hashes: self.hashes,
            detail: self.detail,
        }
    }
}

/// Number of comparisons that were supposed to come back identical and did not.
///
/// A stage that prints a divergence and exits zero is a report, not a gate:
/// whoever reads the log has to notice, and nobody does. Every comparison the
/// stages make is an assertion, so the run fails at the end if this is
/// non-zero.
static DIVERGENCES: AtomicUsize = AtomicUsize::new(0);

fn record_divergence() {
    if MEASURE_ONLY.get() {
        return;
    }
    DIVERGENCES.fetch_add(1, Ordering::Relaxed);
}

thread_local! {
    /// Set while a measurement diff runs: the comparison still prints its
    /// dropped/gained/changed counts, but a non-identical result is the
    /// payload being measured, not a gate failure.
    static MEASURE_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run a diff as a measurement: full output, no divergence recorded.
///
/// For comparisons between *different* inputs — the original workspace versus
/// an edited one — where a non-identical result is the expected answer to
/// "how much did the edit move", not a defect. Equivalence gates (same final
/// text through two paths) must stay outside this and remain fatal.
fn measure(f: impl FnOnce()) {
    let was = MEASURE_ONLY.replace(true);
    f();
    MEASURE_ONLY.set(was);
}

/// Sorted-merge diff of two hash vectors: counts only, since a hash carries
/// no text to print a dropped/gained line from.
fn diff_sorted_hashes(base: &[u64], other: &[u64]) -> (usize, usize) {
    let mut dropped = 0;
    let mut gained = 0;
    let mut i = 0;
    let mut j = 0;
    while i < base.len() && j < other.len() {
        match base[i].cmp(&other[j]) {
            std::cmp::Ordering::Less => {
                dropped += 1;
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                gained += 1;
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    dropped += base.len() - i;
    gained += other.len() - j;
    (dropped, gained)
}

/// One hash-based bucket: full per-entry diff when `DET_DUMP_INDEX_DETAIL`
/// retained text for this index, otherwise a dropped/gained count from the
/// hash vectors alone.
#[allow(clippy::too_many_arguments)]
fn diff_hash_dump(
    base_label: &str,
    label: &str,
    name: &str,
    prefix: &str,
    base: &HashDumpResult,
    other: &HashDumpResult,
    limit: usize,
    emit: &impl Fn(String),
) {
    if let (Some(base_detail), Some(other_detail)) = (&base.detail, &other.detail) {
        diff_map(
            base_label,
            label,
            name,
            prefix,
            base_detail,
            other_detail,
            limit,
            emit,
        );
        return;
    }
    let (dropped, gained) = diff_sorted_hashes(&base.hashes, &other.hashes);
    if dropped > 0 || gained > 0 {
        record_divergence();
    }
    println!("INDEX {base_label} -> {label}: {name} dropped={dropped} gained={gained}");
}

fn collect_index(analysis: &EmmyLuaAnalysis, label: &str) -> IndexSnapshot {
    let db = analysis.compilation.get_db();
    let type_index = db.get_type_index();
    let mut type_caches = BTreeMap::new();
    let mut type_facts: BTreeMap<String, String> = BTreeMap::new();
    // Provenance records how each cached type was reached, so drift entries
    // carry the producing pass on both sides and can be grouped by cause.
    let with_provenance = true;
    for (owner, cache) in type_index.iter_type_caches() {
        let value = format!("{:?}", cache.as_type());
        if with_provenance {
            let fact = type_index.get_type_fact(owner);
            let (confidence, base, steps) = match &fact {
                Some(fact) => (
                    format!("{:?}", fact.confidence()),
                    format!("{:?}", fact.base_provenance_kind()),
                    fact.provenance()
                        .iter()
                        .map(|step| format!("{:?}", step.event.kind))
                        .collect::<Vec<_>>()
                        .join("+"),
                ),
                None => ("-".into(), "-".into(), String::new()),
            };
            let doc = if cache.is_doc() { "doc" } else { "infer" };
            // How many writers share this member's slot. Drift tracking these
            // rather than the individual site points at the merge.
            let writers = match owner {
                glua_code_analysis::LuaTypeOwner::Member(member_id) => {
                    let member_index = db.get_member_index();
                    member_index
                        .get_member(member_id)
                        .zip(member_index.get_member_owner(member_id))
                        .map(|(member, member_owner)| {
                            member_index
                                .get_current_owner_members_for_key(member_owner, member.get_key())
                                .len()
                        })
                        .map(|len| format!("w{len}"))
                        .unwrap_or_else(|| "w-".into())
                }
                _ => "w-".into(),
            };
            type_facts.insert(
                format!("{}|{owner:?}", file_label(analysis, owner.get_file_id())),
                format!("{doc}/{confidence}/{base}/[{steps}]/{writers}"),
            );
        }
        type_caches.insert(
            format!("{}|{owner:?}", file_label(analysis, owner.get_file_id())),
            value,
        );
    }

    let member_index = db.get_member_index();
    let mut members = BTreeSet::new();
    let mut all_owners = std::collections::HashMap::new();
    for file_id in db.get_vfs().get_all_file_ids() {
        let file = file_label(analysis, file_id);
        for member in member_index.get_file_members(file_id) {
            let member_id = member.get_id();
            members.insert(format!(
                "{file}|{:?}|owner={:?}|member={member:?}",
                member.get_key(),
                member_index.get_member_owner(&member_id),
            ));
            if let Some(owner) = member_index.get_member_owner(&member_id) {
                all_owners.insert(format!("{owner:?}"), owner.clone());
            }
        }
    }
    // Net flows are stored per file; sorted so the dump is canonical across
    // processes and membership differences show up in the diff.
    let network = db.get_gmod_network_index();
    let mut net_flows = Vec::new();
    for (file_id, _, flow) in network.iter_send_flows() {
        net_flows.push(format!("send {}|{flow:?}", file_label(analysis, file_id)));
    }
    for (file_id, flow) in network.iter_receive_flows() {
        net_flows.push(format!("recv {}|{flow:?}", file_label(analysis, file_id)));
    }
    net_flows.sort_unstable();

    let mut super_types = BTreeMap::new();
    // Complete effective declaration payload: kind, every location (sorted by
    // file path/range/flags rather than merge-arrival order), alias origin,
    // enum base/flatness, attribute type. `members`/`super_types` snapshot the
    // decl's edges; this snapshots the decl itself.
    let mut type_decl_states = HashDump::new("type_decl_states");
    // Declared generic parameter list per decl. Parameter order is semantic,
    // so the rendered list preserves it instead of sorting.
    let mut type_decl_generic_params = HashDump::new("type_decl_generic_params");
    // Ordered member list per class. Two members can share a key (a doc `@field`
    // and a file define, or two partial-class contributions) and this order picks
    // the winner, so drift changes overload selection without changing the set.
    let mut class_members = BTreeMap::new();
    for type_decl in type_index.get_all_types() {
        let decl_id = type_decl.get_id();
        if let Some(supers) = type_index.get_super_types(&decl_id) {
            super_types.insert(format!("{decl_id:?}"), format!("{supers:?}"));
        }
        let kind = match () {
            _ if type_decl.is_class() => "class",
            _ if type_decl.is_enum() => "enum",
            _ if type_decl.is_alias() => "alias",
            _ if type_decl.is_attribute() => "attribute",
            _ => "unknown",
        };
        let mut locations = type_decl
            .get_locations()
            .iter()
            .map(|location| {
                (
                    file_label(analysis, location.file_id),
                    u32::from(location.range.start()),
                    u32::from(location.range.end()),
                    location.flag.bits(),
                    format!("{location:?}"),
                )
            })
            .collect::<Vec<_>>();
        locations.sort_unstable();
        let locations = locations
            .into_iter()
            .map(|(file, start, end, _, debug)| format!("{file}@{start}..{end} {debug}"))
            .collect::<Vec<_>>()
            .join(",");
        type_decl_states.insert(
            format!("{decl_id:?}"),
            format!(
                "name={}|kind={kind}|locations=[{locations}]|alias_origin={:?}|enum_base={:?}|flat_enum={}|attribute_type={:?}",
                type_decl.get_name(),
                type_decl.get_alias_ref(),
                type_decl.get_enum_base(),
                type_decl.is_flat_enum(),
                type_decl.get_attribute_type(),
            ),
        );
        if let Some(params) = type_index.get_generic_params(&decl_id) {
            type_decl_generic_params.insert(
                format!("{decl_id:?}"),
                params
                    .iter()
                    .map(|param| format!("{param:?}"))
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        let owner = glua_code_analysis::LuaMemberOwner::Type(decl_id.clone());
        if let Some(members) = db.get_member_index().get_members(&owner) {
            let mut ordered = members
                .iter()
                .map(|member| format!("{:?}@{:?}|{member:?}", member.get_key(), member.get_id()))
                .collect::<Vec<_>>();
            ordered.sort_unstable();
            if let Ok(want) = std::env::var("DET_DUMP_CLASS")
                && decl_id.get_simple_name() == want
            {
                for member in &members {
                    println!(
                        "  [{label}] {want}.{:?} from {} id={:?}",
                        member.get_key(),
                        file_label(analysis, member.get_file_id()),
                        member.get_id()
                    );
                }
            }
            class_members.insert(format!("{decl_id:?}"), ordered.join(","));
        }
    }

    // Every stored alias contribution, not just the winning origin the
    // `type_decl_states` bucket shows: a losing site's origin/generic params
    // still decide what a removal re-selects. Sorted by site path/range so
    // the dump is canonical across processes.
    let mut alias_contributions = HashDump::new("alias_contributions");
    let mut alias_entries = type_index
        .iter_alias_sources()
        .map(|(decl_id, source)| {
            (
                file_label(analysis, source.site.file_id),
                u32::from(source.site.value.start()),
                u32::from(source.site.value.end()),
                format!("{decl_id:?}"),
                format!("{source:?}"),
            )
        })
        .collect::<Vec<_>>();
    alias_entries.sort_unstable();
    for (file, start, end, decl_id, source) in alias_entries {
        alias_contributions.insert(format!("{file}@{start}..{end}|decl={decl_id}"), source);
    }

    // DET_DUMP_OWNER_SLOTS=<simple class name>: dump the owner's raw key
    // slots — which slot lists each member id, plus the member's own current
    // key and owner — so an id listed under two slots (or a slot its current
    // owner no longer names) is visible rather than deduped away.
    if let Ok(want) = std::env::var("DET_DUMP_OWNER_SLOTS") {
        let member_index = db.get_member_index();
        for type_decl in type_index.get_all_types() {
            let decl_id = type_decl.get_id();
            if decl_id.get_simple_name() != want {
                continue;
            }
            let owner = glua_code_analysis::LuaMemberOwner::Type(decl_id.clone());
            let mut slot_lines = Vec::new();
            for key in member_index.get_member_keys(&owner) {
                let Some(item) = member_index.get_member_item(&owner, key) else {
                    continue;
                };
                let ids: Vec<glua_code_analysis::LuaMemberId> = match item {
                    glua_code_analysis::LuaMemberIndexItem::One(id) => vec![*id],
                    glua_code_analysis::LuaMemberIndexItem::Many(ids) => ids.clone(),
                };
                for id in ids {
                    let (member_key, current_owner, file) = match member_index.get_member(&id) {
                        Some(member) => (
                            format!("{:?}", member.get_key()),
                            format!("{:?}", member_index.get_member_owner(&id)),
                            file_label(analysis, member.get_file_id()),
                        ),
                        None => ("<gone>".into(), "<gone>".into(), "<gone>".into()),
                    };
                    slot_lines.push(format!(
                        "  [{label}] {want} slot={key:?} id={id:?} member_key={member_key} current_owner={current_owner} file={file}"
                    ));
                }
            }
            slot_lines.sort_unstable();
            for line in slot_lines {
                println!("{line}");
            }
        }
    }

    // The same visible-item view for non-type owners (table constants, element
    // owners, global paths), which decide member lookup on anonymous tables.
    for (key, owner) in &all_owners {
        if let Some(members) = db.get_member_index().get_members(owner) {
            let mut ordered = members
                .iter()
                .map(|member| format!("{:?}@{:?}|{member:?}", member.get_key(), member.get_id()))
                .collect::<Vec<_>>();
            ordered.sort_unstable();
            class_members.insert(format!("OWNER {key}"), ordered.join(","));
        }
    }

    let mut definition_sites = BTreeMap::new();
    for (path, sites) in db.get_member_index().sorted_definition_sites() {
        definition_sites.insert(
            path.get_name().to_string(),
            sites
                .iter()
                .map(|site| {
                    format!(
                        "{}@{:?}",
                        file_label(analysis, site.file_id),
                        (u32::from(site.value.start()), u32::from(site.value.end()))
                    )
                })
                .collect::<Vec<_>>()
                .join(","),
        );
    }

    // Resolved signature shapes. `inferred_params` covers only call-site-derived
    // facts; a signature's own param and return types are a separate index.
    let mut signatures = BTreeMap::new();
    for (signature_id, signature) in db.get_signature_index().iter() {
        signatures.insert(
            format!(
                "{}|{signature_id:?}",
                file_label(analysis, signature_id.get_file_id())
            ),
            format!(
                "params={:?} return={:?} optional={:?} sig={}",
                signature.get_type_params(),
                signature.get_return_type(),
                signature.get_param_optional_flags(),
                canonical_signature(signature)
            ),
        );
    }

    let flow = db.get_flow_index();
    let mut flow_trees = BTreeMap::new();
    for (file_id, tree) in flow.flow_trees() {
        flow_trees.insert(file_label(analysis, *file_id), format!("{tree:?}"));
    }
    let mut signature_casts = BTreeMap::new();
    for (file_id, signature_id, cast) in flow.signature_casts() {
        signature_casts.insert(
            format!("{}|{signature_id:?}", file_label(analysis, *file_id)),
            format!("{cast:?}"),
        );
    }
    let mut special_call_effects = BTreeMap::new();
    for (file_id, position, effect) in flow.special_call_effects() {
        special_call_effects.insert(
            format!("{}|{position:?}|{effect:?}", file_label(analysis, *file_id)),
            format!("{effect:?}"),
        );
    }
    let mut special_call_facts = BTreeMap::new();
    for fact in flow.settled_special_call_facts() {
        let text = format!("{fact:?}");
        special_call_facts.insert(text.clone(), text);
    }
    // Only the site set is semantic: the version values are a monotonic epoch
    // bumped once per novel write, so a re-index of unchanged text rewrites
    // them. Facts and effects carry the semantic payload.
    let mut special_call_write_sites = BTreeMap::new();
    for &(file_id, position) in flow.special_call_write_sites() {
        let site = format!("{}|{position:?}", file_label(analysis, file_id));
        special_call_write_sites.insert(site.clone(), site);
    }
    let mut inferred_params = BTreeMap::new();
    for (signature_id, param_idx, fact) in db.get_call_site_param_index().iter_inferred_params() {
        inferred_params.insert(
            format!(
                "{}|{signature_id:?}|param{param_idx}",
                file_label(analysis, signature_id.get_file_id())
            ),
            format!("{:?}", fact.typ()),
        );
    }

    // Where every member ended up. Contribution groups and class member lists
    // are both keyed off this.
    let mut member_owners = BTreeMap::new();
    if with_provenance {
        for file_id in db.get_vfs().get_all_file_ids() {
            for member in member_index.get_file_members(file_id) {
                member_owners.insert(
                    format!(
                        "{}:{:?}|{:?}",
                        file_id.id,
                        u32::from(member.get_id().get_position()),
                        member.get_key()
                    ),
                    format!("{:?}", member_index.get_current_owner(&member.get_id())),
                );
            }
        }
    }

    // Every index not already covered above, dumped through whatever public
    // iteration each one exposes. Per-file maps key on `file_label`; indexes
    // with no per-file getter but a global iterator key on their own item.
    let all_files = db.get_vfs().get_all_file_ids();

    let decl_index = db.get_decl_index();
    let mut decls = HashDump::new("decls");
    let mut decl_references = HashDump::new("decl_references");
    for &file_id in &all_files {
        let file = file_label(analysis, file_id);
        if let Some(tree) = decl_index.get_decl_tree(&file_id) {
            for (decl_id, decl) in tree.get_decls() {
                decls.insert(format!("{file}|{decl_id:?}"), format!("{decl:?}"));
            }
        }
        if let Some(reference) = db.get_reference_index().get_local_reference(&file_id) {
            for (decl_id, decl_reference) in reference.get_decl_references_map() {
                decl_references
                    .insert(format!("{file}|{decl_id:?}"), format!("{decl_reference:?}"));
            }
        }
    }

    // The rest of the reference index: the inverse range→decl map, the
    // member/global/string/type site maps, and per-local first-reassignment
    // positions. `file_revision`/`next_revision` are raw epochs and stay out.
    // Hash-set values render sorted so two processes agree on the text.
    let reference_index = db.get_reference_index();
    let mut reference_ranges = HashDump::new("reference_ranges");
    for (file_id, range, decl_id) in reference_index.iter_reference_ranges() {
        reference_ranges.insert(
            format!("{}|{range:?}", file_label(analysis, file_id)),
            format!("{decl_id:?}"),
        );
    }
    let mut index_references = HashDump::new("index_references");
    for (key, by_file) in reference_index.iter_index_references() {
        for (file_id, syntax_ids) in by_file {
            let mut sites = syntax_ids
                .iter()
                .map(|syntax_id| format!("{syntax_id:?}"))
                .collect::<Vec<_>>();
            sites.sort_unstable();
            index_references.insert(
                format!("{key:?}|{}", file_label(analysis, *file_id)),
                format!("{sites:?}"),
            );
        }
    }
    let mut global_references = HashDump::new("global_references");
    for (name, by_file) in reference_index.iter_global_references() {
        for (file_id, syntax_ids) in by_file {
            let mut sites = syntax_ids
                .iter()
                .map(|syntax_id| format!("{syntax_id:?}"))
                .collect::<Vec<_>>();
            sites.sort_unstable();
            global_references.insert(
                format!("{name}|{}", file_label(analysis, *file_id)),
                format!("{sites:?}"),
            );
        }
    }
    let mut string_references = HashDump::new("string_references");
    for (file_id, string_reference) in reference_index.iter_string_references() {
        let file = file_label(analysis, *file_id);
        for (string, ranges) in string_reference.iter_string_references() {
            string_references.insert(format!("{file}|{string}"), format!("{ranges:?}"));
        }
    }
    let mut type_references = HashDump::new("type_references");
    for (file_id, by_decl) in reference_index.iter_type_references() {
        let file = file_label(analysis, *file_id);
        for (type_decl_id, ranges) in by_decl {
            let mut sites = ranges
                .iter()
                .map(|range| format!("{range:?}"))
                .collect::<Vec<_>>();
            sites.sort_unstable();
            type_references.insert(format!("{file}|{type_decl_id:?}"), format!("{sites:?}"));
        }
    }
    let mut local_reassignments = HashDump::new("local_reassignments");
    for (file_id, positions) in reference_index.iter_local_reassignments() {
        let file = file_label(analysis, *file_id);
        for (decl_id, position) in positions {
            local_reassignments.insert(
                format!("{file}|{decl_id:?}"),
                format!("{}", u32::from(*position)),
            );
        }
    }

    // `module_id` is an arena index assigned in tree-build order, not a
    // function of content -- it legitimately differs between a full walk and
    // a partial re-index of the same files, so it is left out here.
    let mut modules = HashDump::new("modules");
    for info in db.get_module_index().get_module_infos() {
        modules.insert(
            file_label(analysis, info.file_id),
            format!(
                "full_module_name={:?} name={:?} visible={:?} export_type={:?} version_conds={:?} workspace_id={:?} semantic_id={:?} is_meta={:?}",
                info.full_module_name,
                info.name,
                info.visible,
                info.export_type,
                info.version_conds,
                info.workspace_id,
                info.semantic_id,
                info.is_meta
            ),
        );
    }

    let mut globals = HashDump::new("globals");
    for decl_id in db.get_global_index().get_all_global_decl_ids() {
        globals.insert(format!("{decl_id:?}"), format!("{decl_id:?}"));
    }

    let mut diagnostics = HashDump::new("diagnostics_index");
    let mut diagnostic_actions = HashDump::new("diagnostic_actions");
    let mut operators = HashDump::new("operators");
    for &file_id in &all_files {
        let file = file_label(analysis, file_id);
        if let Some(errors) = db.get_diagnostic_index().get_diagnostics(&file_id) {
            diagnostics.insert(file.clone(), format!("{errors:?}"));
        }
        if let Some(actions) = db.get_diagnostic_index().get_diagnostics_actions(file_id) {
            diagnostic_actions.insert(file.clone(), format!("{actions:?}"));
        }
        let ops = db.get_operator_index().operators_in_file(file_id);
        if !ops.is_empty() {
            operators.insert(file, format!("{ops:?}"));
        }
    }

    let mut dependency_sites = HashDump::new("dependency_sites");
    for (file_id, sites) in db.get_file_dependencies_index().iter_dependency_sites() {
        dependency_sites.insert(file_label(analysis, *file_id), format!("{sites:?}"));
    }

    let mut gmod_load_files = HashDump::new("gmod_load_files");
    for (file_id, info) in db.get_gmod_load_index().iter_file_infos() {
        gmod_load_files.insert(file_label(analysis, *file_id), format!("{info:?}"));
    }
    // Self-keyed rather than positional: an edge has no stable id, and a
    // positional key would turn one inserted/removed entry into a cascade of
    // false "changed" hits for every entry after it.
    let mut gmod_load_unresolved_edges = HashDump::new("gmod_load_unresolved_edges");
    for edge in db.get_gmod_load_index().unresolved_edges() {
        let text = format!("{edge:?}");
        gmod_load_unresolved_edges.insert(text.clone(), text);
    }

    let gmod_infer_index = db.get_gmod_infer_index();
    let mut gmod_hook_files = HashDump::new("gmod_hook_files");
    for (file_id, metadata) in gmod_infer_index.iter_hook_file_metadata() {
        gmod_hook_files.insert(file_label(analysis, *file_id), format!("{metadata:?}"));
    }
    let mut gmod_system_files = HashDump::new("gmod_system_files");
    for (file_id, metadata) in gmod_infer_index.iter_system_file_metadata() {
        gmod_system_files.insert(file_label(analysis, *file_id), format!("{metadata:?}"));
    }
    let mut gmod_gm_method_realm_annotations = HashDump::new("gmod_gm_method_realm_annotations");
    for (file_id, annotations) in gmod_infer_index.iter_gm_method_realm_annotations() {
        gmod_gm_method_realm_annotations
            .insert(file_label(analysis, *file_id), format!("{annotations:?}"));
    }
    let mut gmod_realm_files = HashDump::new("gmod_realm_files");
    let mut gmod_scoped_class_info = HashDump::new("gmod_scoped_class_info");
    for &file_id in &all_files {
        let file = file_label(analysis, file_id);
        if let Some(metadata) = gmod_infer_index.get_realm_file_metadata(&file_id) {
            gmod_realm_files.insert(file.clone(), format!("{metadata:?}"));
        }
        if let Some(info) = gmod_infer_index.get_scoped_class_info(&file_id) {
            gmod_scoped_class_info.insert(file, format!("{info:?}"));
        }
    }

    let mut gmod_network_wire_coverage = HashDump::new("gmod_network_wire_coverage");
    for (wire_format, coverage) in network.wire_format_coverage() {
        gmod_network_wire_coverage.insert(format!("{wire_format:?}"), format!("{coverage:?}"));
    }

    let mut gmod_class_file_metadata = HashDump::new("gmod_class_file_metadata");
    for (file_id, metadata) in db.get_gmod_class_metadata_index().iter_file_metadata() {
        gmod_class_file_metadata.insert(file_label(analysis, *file_id), format!("{metadata:?}"));
    }

    let mut dynamic_field_wildcards = HashDump::new("dynamic_field_wildcards");
    for entry in db.get_dynamic_field_index().get_all_wildcard_definitions() {
        dynamic_field_wildcards.insert(format!("{entry:?}"), format!("{entry:?}"));
    }

    // The named-field maps, unattributed names and finite members the wildcard
    // bucket does not cover. Hash-set values render sorted so two processes
    // agree on the text.
    let mut dynamic_fields = HashDump::new("dynamic_fields");
    let dynamic_field_index = db.get_dynamic_field_index();
    for (owner, fields) in dynamic_field_index.iter_all_owner_fields() {
        for (field_name, files) in fields {
            let mut files = files.iter().map(|f| f.id).collect::<Vec<_>>();
            files.sort_unstable();
            dynamic_fields.insert(
                format!("OWNERFIELD {owner:?}|{field_name}"),
                format!("{files:?}"),
            );
        }
    }
    for (owner, fields) in dynamic_field_index.iter_all_field_definitions() {
        for (field_name, definitions) in fields {
            let mut definitions = definitions
                .iter()
                .map(|d| format!("{d:?}"))
                .collect::<Vec<_>>();
            definitions.sort_unstable();
            dynamic_fields.insert(
                format!("FIELDDEF {owner:?}|{field_name}"),
                format!("{definitions:?}"),
            );
        }
    }
    for (owner, fields) in dynamic_field_index.iter_all_direct_field_definitions() {
        for (field_name, definitions) in fields {
            let mut definitions = definitions
                .iter()
                .map(|d| format!("{d:?}"))
                .collect::<Vec<_>>();
            definitions.sort_unstable();
            dynamic_fields.insert(
                format!("DIRECTFIELDDEF {owner:?}|{field_name}"),
                format!("{definitions:?}"),
            );
        }
    }
    for (field_name, sites) in dynamic_field_index.iter_all_unattributed_fields() {
        // Every site keeps its full identity — file id plus the index-key
        // range's numeric bounds — so same-file sites stay distinct records.
        let mut sites = sites
            .iter()
            .map(|site| {
                (
                    site.file_id.id,
                    u32::from(site.value.start()),
                    u32::from(site.value.end()),
                )
            })
            .collect::<Vec<_>>();
        sites.sort_unstable();
        dynamic_fields.insert(format!("UNATTRIBUTED {field_name}"), format!("{sites:?}"));
    }
    for (owner, member_ids) in dynamic_field_index.iter_all_finite_named_members() {
        let mut member_ids = member_ids
            .iter()
            .map(|m| format!("{m:?}"))
            .collect::<Vec<_>>();
        member_ids.sort_unstable();
        dynamic_fields.insert(format!("FINITE {owner:?}"), format!("{member_ids:?}"));
    }

    let mut accessor_func_annotations = HashDump::new("accessor_func_annotations");
    for (name, annotations) in db.get_accessor_func_index().annotation_entries() {
        let mut entries = annotations
            .iter()
            .map(|annotation| format!("{annotation:?}"))
            .collect::<Vec<_>>();
        entries.sort_unstable();
        accessor_func_annotations.insert(name.to_string(), format!("{entries:?}"));
    }
    let mut accessor_func_annotation_files = HashDump::new("accessor_func_annotation_files");
    for (file_id, names) in db.get_accessor_func_index().annotation_files() {
        let mut names = names
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        names.sort();
        accessor_func_annotation_files.insert(file_label(analysis, *file_id), format!("{names:?}"));
    }
    let mut numeric_range_file_populations = HashDump::new("numeric_range_file_populations");
    let mut numeric_range_global_populations = HashDump::new("numeric_range_global_populations");
    for (file_id, populations) in db
        .get_numeric_range_population_index()
        .populations_by_file()
    {
        let mut entries = populations
            .iter()
            .map(|population| format!("{population:?}"))
            .collect::<Vec<_>>();
        entries.sort_unstable();
        numeric_range_file_populations
            .insert(file_label(analysis, *file_id), format!("{entries:?}"));
    }
    for (global, populations) in db
        .get_numeric_range_population_index()
        .populations_by_global()
    {
        let mut entries = populations
            .iter()
            .map(|population| format!("{population:?}"))
            .collect::<Vec<_>>();
        entries.sort_unstable();
        numeric_range_global_populations.insert(global.clone(), format!("{entries:?}"));
    }
    let mut accessor_func_calls = HashDump::new("accessor_func_calls");
    for (file_id, calls) in db.get_accessor_func_call_index().iter() {
        accessor_func_calls.insert(file_label(analysis, *file_id), format!("{calls:?}"));
    }

    let mut properties = HashDump::new("properties");
    for (owner_id, property) in db.get_property_index().iter_owner_properties() {
        properties.insert(format!("{owner_id:?}"), format!("{property:?}"));
    }
    // Source-owned contributions, normalized across the sole/multi storage
    // split: the merged `properties` view alone cannot show a hidden
    // contribution that lost the merge but still decides what a deletion
    // re-selects.
    for contribution in db.get_property_index().iter_source_contributions() {
        properties.insert(
            format!(
                "PROPERTYCONTRIB {:?}|{}",
                contribution.owner_id,
                file_label(analysis, contribution.file_id)
            ),
            format!(
                "rank={}|path={}|{:?}",
                contribution.workspace_rank, contribution.normalized_path, contribution.property
            ),
        );
    }

    let mut metatables = HashDump::new("metatables");
    for (table, binding) in db.get_metatable_index().iter_bindings() {
        metatables.insert(
            format!("{table:?} <- {:?}", binding.writer_file_id),
            format!("{:?}", binding.metatable),
        );
    }
    let mut metatable_factory_bindings = HashDump::new("metatable_factory_bindings");
    for &file_id in &all_files {
        if let Some(bindings) = db.get_metatable_index().factory_bindings_for_file(file_id)
            && !bindings.is_empty()
        {
            metatable_factory_bindings
                .insert(file_label(analysis, file_id), format!("{bindings:?}"));
        }
    }

    // The call-site param index's persisted relations. `deferred_contributions`
    // is a transient queue and `contrib_writes`/`contrib_write_versions` are
    // raw epochs, so both stay out; everything else is semantic state a
    // re-index has to reproduce. Hash-set values render sorted so two
    // processes agree on the text.
    let call_site = db.get_call_site_param_index();
    let mut call_site_source_signatures = HashDump::new("call_site_source_signatures");
    for (file_id, signatures) in call_site.iter_file_source_signatures() {
        call_site_source_signatures
            .insert(file_label(analysis, *file_id), format!("{signatures:?}"));
    }
    let mut call_site_previous_source_signatures =
        HashDump::new("call_site_previous_source_signatures");
    for (file_id, signatures) in call_site.iter_previous_source_signatures() {
        call_site_previous_source_signatures
            .insert(file_label(analysis, *file_id), format!("{signatures:?}"));
    }
    let mut call_site_signatures_by_path = HashDump::new("call_site_signatures_by_path");
    for (path, signature_ids) in call_site.iter_source_signatures_by_path() {
        call_site_signatures_by_path.insert(path.clone(), format!("{signature_ids:?}"));
    }
    let mut call_site_mutated_params = HashDump::new("call_site_mutated_params");
    for (signature_id, params) in call_site.iter_mutated_params() {
        call_site_mutated_params.insert(
            format!(
                "{}|{signature_id:?}",
                file_label(analysis, signature_id.get_file_id())
            ),
            format!("{params:?}"),
        );
    }
    let mut call_site_contributions = HashDump::new("call_site_contributions");
    for (file_id, contributions) in call_site.iter_file_contributions() {
        call_site_contributions
            .insert(file_label(analysis, *file_id), format!("{contributions:?}"));
    }
    let mut call_site_contributor_files = HashDump::new("call_site_contributor_files");
    for (signature_id, files) in call_site.iter_contributor_files_by_signature() {
        let mut files = files
            .iter()
            .map(|file_id| file_label(analysis, *file_id))
            .collect::<Vec<_>>();
        files.sort_unstable();
        call_site_contributor_files.insert(
            format!(
                "{}|{signature_id:?}",
                file_label(analysis, signature_id.get_file_id())
            ),
            format!("{files:?}"),
        );
    }
    let mut call_site_contributor_signatures = HashDump::new("call_site_contributor_signatures");
    for (file_id, signature_ids) in call_site.iter_contributor_signatures_by_file() {
        let mut signature_ids = signature_ids
            .iter()
            .map(|signature_id| format!("{signature_id:?}"))
            .collect::<Vec<_>>();
        signature_ids.sort_unstable();
        call_site_contributor_signatures
            .insert(file_label(analysis, *file_id), format!("{signature_ids:?}"));
    }
    let mut call_site_pending_previous_params = HashDump::new("call_site_pending_previous_params");
    for ((signature_id, param_idx), fact) in call_site.iter_pending_previous_params() {
        call_site_pending_previous_params.insert(
            format!(
                "{}|{signature_id:?}|param{param_idx}",
                file_label(analysis, signature_id.get_file_id())
            ),
            format!("{fact:?}"),
        );
    }
    let mut call_site_file_return_consumers = HashDump::new("call_site_file_return_consumers");
    for (file_id, consumers) in call_site.iter_file_return_consumers() {
        call_site_file_return_consumers
            .insert(file_label(analysis, *file_id), format!("{consumers:?}"));
    }
    let mut call_site_return_consumers = HashDump::new("call_site_return_consumers");
    for (signature_id, consumers) in call_site.iter_return_consumers() {
        call_site_return_consumers.insert(
            format!(
                "{}|{signature_id:?}",
                file_label(analysis, signature_id.get_file_id())
            ),
            format!("{consumers:?}"),
        );
    }
    let mut call_site_return_consumers_by_signature_file =
        HashDump::new("call_site_return_consumers_by_signature_file");
    for (file_id, consumers) in call_site.iter_return_consumers_by_signature_file() {
        call_site_return_consumers_by_signature_file
            .insert(file_label(analysis, *file_id), format!("{consumers:?}"));
    }
    let mut call_site_callback_files = HashDump::new("call_site_callback_files");
    for file_id in call_site.iter_concrete_structural_callback_files() {
        let file = file_label(analysis, *file_id);
        call_site_callback_files.insert(file.clone(), file);
    }
    let mut call_site_inference_events = HashDump::new("call_site_inference_events");
    for (file_id, events) in call_site.iter_inference_events_by_file() {
        call_site_inference_events.insert(file_label(analysis, *file_id), format!("{events:?}"));
    }
    let mut call_site_source_dependencies = HashDump::new("call_site_source_dependencies");
    for (file_id, sources) in call_site.iter_file_source_dependencies() {
        let mut sources = sources
            .iter()
            .map(|source| format!("{source:?}"))
            .collect::<Vec<_>>();
        sources.sort_unstable();
        call_site_source_dependencies
            .insert(file_label(analysis, *file_id), format!("{sources:?}"));
    }
    let mut call_site_source_dependents = HashDump::new("call_site_source_dependents");
    for (source, files) in call_site.iter_source_dependents() {
        let mut files = files
            .iter()
            .map(|file_id| file_label(analysis, *file_id))
            .collect::<Vec<_>>();
        files.sort_unstable();
        call_site_source_dependents.insert(format!("{source:?}"), format!("{files:?}"));
    }
    let mut call_site_source_file_dependents = HashDump::new("call_site_source_file_dependents");
    for (file_id, files) in call_site.iter_source_file_dependents() {
        let mut files = files
            .iter()
            .map(|file_id| file_label(analysis, *file_id))
            .collect::<Vec<_>>();
        files.sort_unstable();
        call_site_source_file_dependents
            .insert(file_label(analysis, *file_id), format!("{files:?}"));
    }
    let mut call_site_source_paths = HashDump::new("call_site_source_paths");
    for (file_id, path) in call_site.iter_source_paths() {
        call_site_source_paths.insert(
            file_label(analysis, *file_id),
            path.display().to_string().replace('\\', "/"),
        );
    }
    let mut call_site_source_path_dependents = HashDump::new("call_site_source_path_dependents");
    for (path, files) in call_site.iter_source_path_dependents() {
        let mut files = files
            .iter()
            .map(|file_id| file_label(analysis, *file_id))
            .collect::<Vec<_>>();
        files.sort_unstable();
        call_site_source_path_dependents.insert(
            path.display().to_string().replace('\\', "/"),
            format!("{files:?}"),
        );
    }

    let mut schema_files = HashDump::new("schema_files");
    for (url, schema_file) in db.get_json_schema_index().iter_schema_files() {
        schema_files.insert(url.as_str().to_string(), format!("{schema_file:?}"));
    }

    eprintln!(
        "[{label}] index type_caches={} members={} net_flows={} flow_trees={} signature_casts={} special_call_effects={} special_call_facts={} special_call_sites={} accessor_annotations={} accessor_annotation_files={} numeric_range_files={} numeric_range_globals={} type_decl_states={} type_decl_generic_params={}",
        type_caches.len(),
        members.len(),
        net_flows.len(),
        flow_trees.len(),
        signature_casts.len(),
        special_call_effects.len(),
        special_call_facts.len(),
        special_call_write_sites.len(),
        accessor_func_annotations.hashes.len(),
        accessor_func_annotation_files.hashes.len(),
        numeric_range_file_populations.hashes.len(),
        numeric_range_global_populations.hashes.len(),
        type_decl_states.hashes.len(),
        type_decl_generic_params.hashes.len()
    );

    let snapshot = IndexSnapshot {
        type_caches,
        type_facts,
        member_owners,
        members,
        net_flows,
        inferred_params,
        super_types,
        class_members,
        definition_sites,
        signatures,
        flow_trees,
        signature_casts,
        special_call_effects,
        special_call_facts,
        special_call_write_sites,
        accessor_func_annotations: accessor_func_annotations.finish(),
        accessor_func_annotation_files: accessor_func_annotation_files.finish(),
        numeric_range_file_populations: numeric_range_file_populations.finish(),
        numeric_range_global_populations: numeric_range_global_populations.finish(),
        decls: decls.finish(),
        type_decl_states: type_decl_states.finish(),
        type_decl_generic_params: type_decl_generic_params.finish(),
        alias_contributions: alias_contributions.finish(),
        decl_references: decl_references.finish(),
        reference_ranges: reference_ranges.finish(),
        index_references: index_references.finish(),
        global_references: global_references.finish(),
        string_references: string_references.finish(),
        type_references: type_references.finish(),
        local_reassignments: local_reassignments.finish(),
        modules: modules.finish(),
        globals: globals.finish(),
        diagnostics: diagnostics.finish(),
        diagnostic_actions: diagnostic_actions.finish(),
        operators: operators.finish(),
        dependency_sites: dependency_sites.finish(),
        gmod_load_files: gmod_load_files.finish(),
        gmod_load_unresolved_edges: gmod_load_unresolved_edges.finish(),
        gmod_hook_files: gmod_hook_files.finish(),
        gmod_system_files: gmod_system_files.finish(),
        gmod_gm_method_realm_annotations: gmod_gm_method_realm_annotations.finish(),
        gmod_realm_files: gmod_realm_files.finish(),
        gmod_scoped_class_info: gmod_scoped_class_info.finish(),
        gmod_network_wire_coverage: gmod_network_wire_coverage.finish(),
        gmod_class_file_metadata: gmod_class_file_metadata.finish(),
        dynamic_field_wildcards: dynamic_field_wildcards.finish(),
        dynamic_fields: dynamic_fields.finish(),
        accessor_func_calls: accessor_func_calls.finish(),
        properties: properties.finish(),
        metatables: metatables.finish(),
        metatable_factory_bindings: metatable_factory_bindings.finish(),
        call_site_source_signatures: call_site_source_signatures.finish(),
        call_site_previous_source_signatures: call_site_previous_source_signatures.finish(),
        call_site_signatures_by_path: call_site_signatures_by_path.finish(),
        call_site_mutated_params: call_site_mutated_params.finish(),
        call_site_contributions: call_site_contributions.finish(),
        call_site_contributor_files: call_site_contributor_files.finish(),
        call_site_contributor_signatures: call_site_contributor_signatures.finish(),
        call_site_pending_previous_params: call_site_pending_previous_params.finish(),
        call_site_file_return_consumers: call_site_file_return_consumers.finish(),
        call_site_return_consumers: call_site_return_consumers.finish(),
        call_site_return_consumers_by_signature_file: call_site_return_consumers_by_signature_file
            .finish(),
        call_site_callback_files: call_site_callback_files.finish(),
        call_site_inference_events: call_site_inference_events.finish(),
        call_site_source_dependencies: call_site_source_dependencies.finish(),
        call_site_source_dependents: call_site_source_dependents.finish(),
        call_site_source_file_dependents: call_site_source_file_dependents.finish(),
        call_site_source_paths: call_site_source_paths.finish(),
        call_site_source_path_dependents: call_site_source_path_dependents.finish(),
        schema_files: schema_files.finish(),
    };

    // Cross-process comparison: write the whole snapshot so two runs of the same
    // binary on the same input can be diffed against each other.
    if let Ok(dir) = std::env::var("DET_DUMP_INDEX") {
        let mut out = String::new();
        for (tag, map) in [
            ("TC", &snapshot.type_caches),
            ("TF", &snapshot.type_facts),
            ("MO", &snapshot.member_owners),
            ("PARAM", &snapshot.inferred_params),
            ("SUPER", &snapshot.super_types),
            ("CLSMEM", &snapshot.class_members),
            ("DEFSITE", &snapshot.definition_sites),
            ("SIG", &snapshot.signatures),
            ("FLOW", &snapshot.flow_trees),
            ("CAST", &snapshot.signature_casts),
            ("SCEFFECT", &snapshot.special_call_effects),
            ("SCFACT", &snapshot.special_call_facts),
            ("SCSITE", &snapshot.special_call_write_sites),
        ] {
            for (key, value) in map {
                out.push_str(&format!("{tag} {key} = {value}\n"));
            }
        }
        for entry in &snapshot.members {
            out.push_str(&format!("MEM {entry}\n"));
        }
        for entry in &snapshot.net_flows {
            out.push_str(&format!("NET {entry}\n"));
        }
        // Hash-only unless `DET_DUMP_INDEX_DETAIL` named this index: write its
        // retained text, otherwise write just the sorted hashes so a
        // cross-process compare still has *something* to diff.
        for (tag, dump) in [
            ("DECL", &snapshot.decls),
            ("TDECL", &snapshot.type_decl_states),
            ("TGEN", &snapshot.type_decl_generic_params),
            ("ALIASCONTRIB", &snapshot.alias_contributions),
            ("DREF", &snapshot.decl_references),
            ("REFRANGE", &snapshot.reference_ranges),
            ("IDXREF", &snapshot.index_references),
            ("GLOBREF", &snapshot.global_references),
            ("STRREF", &snapshot.string_references),
            ("TYPEREF", &snapshot.type_references),
            ("LOCALREASSIGN", &snapshot.local_reassignments),
            ("MOD", &snapshot.modules),
            ("GLOBAL", &snapshot.globals),
            ("DIAG", &snapshot.diagnostics),
            ("DIAGACT", &snapshot.diagnostic_actions),
            ("OP", &snapshot.operators),
            ("DEP", &snapshot.dependency_sites),
            ("GLOAD", &snapshot.gmod_load_files),
            ("GLOADEDGE", &snapshot.gmod_load_unresolved_edges),
            ("GHOOK", &snapshot.gmod_hook_files),
            ("GSYS", &snapshot.gmod_system_files),
            ("GMETHREALM", &snapshot.gmod_gm_method_realm_annotations),
            ("GREALM", &snapshot.gmod_realm_files),
            ("GSCOPED", &snapshot.gmod_scoped_class_info),
            ("GWIRE", &snapshot.gmod_network_wire_coverage),
            ("GCLASS", &snapshot.gmod_class_file_metadata),
            ("DYNWILD", &snapshot.dynamic_field_wildcards),
            ("DYNFIELD", &snapshot.dynamic_fields),
            ("ACCANN", &snapshot.accessor_func_annotations),
            ("ACCFILES", &snapshot.accessor_func_annotation_files),
            ("NUMFILE", &snapshot.numeric_range_file_populations),
            ("NUMGLOBAL", &snapshot.numeric_range_global_populations),
            ("ACCCALL", &snapshot.accessor_func_calls),
            ("PROP", &snapshot.properties),
            ("METATBL", &snapshot.metatables),
            ("METAFACT", &snapshot.metatable_factory_bindings),
            ("CSSIG", &snapshot.call_site_source_signatures),
            ("CSPREVSIG", &snapshot.call_site_previous_source_signatures),
            ("CSSIGPATH", &snapshot.call_site_signatures_by_path),
            ("CSMUT", &snapshot.call_site_mutated_params),
            ("CSCONTRIB", &snapshot.call_site_contributions),
            ("CSCONTRIBF", &snapshot.call_site_contributor_files),
            ("CSCONTRIBS", &snapshot.call_site_contributor_signatures),
            ("CSPREV", &snapshot.call_site_pending_previous_params),
            ("CSFCONS", &snapshot.call_site_file_return_consumers),
            ("CSCONS", &snapshot.call_site_return_consumers),
            (
                "CSCONSSIGF",
                &snapshot.call_site_return_consumers_by_signature_file,
            ),
            ("CSCB", &snapshot.call_site_callback_files),
            ("CSEVENTS", &snapshot.call_site_inference_events),
            ("CSDEP", &snapshot.call_site_source_dependencies),
            ("CSSRCDEP", &snapshot.call_site_source_dependents),
            ("CSFILEDEP", &snapshot.call_site_source_file_dependents),
            ("CSPATH", &snapshot.call_site_source_paths),
            ("CSPATHDEP", &snapshot.call_site_source_path_dependents),
            ("SCHEMA", &snapshot.schema_files),
        ] {
            if let Some(detail) = &dump.detail {
                for (key, value) in detail {
                    out.push_str(&format!("{tag} {key} = {value}\n"));
                }
            } else {
                for hash in &dump.hashes {
                    out.push_str(&format!("{tag}_HASH {hash:016x}\n"));
                }
            }
        }
        let file_name = label
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect::<String>();
        let _ = std::fs::write(format!("{dir}/{file_name}.idx"), out);
    }

    snapshot
}

/// Canonical serialization of one signature for the index snapshot.
///
/// `LuaSignature`'s derived `Debug` renders `param_docs` — a
/// `std::collections::HashMap` re-seeded per signature — in randomized order,
/// so two identical builds can produce different strings for the same
/// signature. Every field is serialized explicitly instead: `param_docs`
/// sorted by parameter index, private fields through their getters.
fn canonical_signature(signature: &glua_code_analysis::LuaSignature) -> String {
    let mut param_docs = signature
        .param_docs
        .iter()
        .map(|(idx, info)| format!("{idx}:{info:?}"))
        .collect::<Vec<_>>();
    param_docs.sort_unstable();
    format!(
        "generic_params={:?} overloads={:?} param_docs={param_docs:?} out_params={:?} params={:?} \
         return_docs={:?} resolve_return={:?} is_colon_define={:?} async_state={:?} nodiscard={:?} \
         is_vararg={:?} params_filled_from_slot={:?} return_filled_from_slot={:?} \
         require_guard_param={:?} nil_return_guard_params={:?} return_correlations={:?} \
         direct_param_return_alias={:?} class_name_param_return_alias={:?} \
         falsy_param_nil_free_return_slots={:?} falsy_param_return_aliases={:?}",
        signature.generic_params,
        signature.overloads,
        signature.out_params,
        signature.params,
        signature.return_docs,
        signature.resolve_return,
        signature.is_colon_define,
        signature.async_state,
        signature.nodiscard,
        signature.is_vararg,
        signature.params_filled_from_slot,
        signature.return_filled_from_slot,
        signature.require_guard_param(),
        signature.nil_return_guard_params(),
        signature.return_correlations(),
        signature.direct_param_return_alias(),
        signature.class_name_param_return_alias(),
        signature.falsy_param_nil_free_return_slots(),
        signature.falsy_param_return_aliases(),
    )
}

/// Max diff lines printed per bucket.
fn diff_limit() -> usize {
    std::env::var("DET_LIMIT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(40)
}

/// One `BTreeMap` bucket of the index snapshot: keys only in `base`, keys only
/// in `other`, and keys whose value changed.
#[allow(clippy::too_many_arguments)]
fn diff_map(
    base_label: &str,
    label: &str,
    name: &str,
    prefix: &str,
    base: &BTreeMap<String, String>,
    other: &BTreeMap<String, String>,
    limit: usize,
    emit: &impl Fn(String),
) {
    let base_keys = base.keys().collect::<BTreeSet<_>>();
    let other_keys = other.keys().collect::<BTreeSet<_>>();
    let dropped = base_keys.difference(&other_keys).collect::<Vec<_>>();
    let gained = other_keys.difference(&base_keys).collect::<Vec<_>>();
    let changed = base_keys
        .intersection(&other_keys)
        .filter(|key| base.get(**key) != other.get(**key))
        .collect::<Vec<_>>();
    if !dropped.is_empty() || !gained.is_empty() || !changed.is_empty() {
        record_divergence();
    }
    println!(
        "INDEX {base_label} -> {label}: {name} dropped={} gained={} changed={}",
        dropped.len(),
        gained.len(),
        changed.len()
    );
    for key in dropped.iter().take(limit) {
        emit(format!("  {prefix}_DROPPED {key} = {:?}", base.get(**key)));
    }
    for key in gained.iter().take(limit) {
        emit(format!("  {prefix}_GAINED  {key} = {:?}", other.get(**key)));
    }
    for key in changed.iter().take(limit) {
        emit(format!(
            "  {prefix}_CHANGED {key}
      before = {:?}
      after  = {:?}",
            base.get(**key),
            other.get(**key)
        ));
    }
}

fn diff_index(base_label: &str, base: &IndexSnapshot, label: &str, other: &IndexSnapshot) {
    let filter = std::env::var("DET_FILTER")
        .ok()
        .filter(|value| !value.is_empty());
    let limit = match &filter {
        Some(_) => usize::MAX,
        None => diff_limit(),
    };
    let emit = |line: String| {
        if filter.as_ref().is_none_or(|want| line.contains(want)) {
            println!("{line}");
        }
    };

    for (name, prefix, base_map, other_map) in [
        ("type_caches", "TC", &base.type_caches, &other.type_caches),
        ("type_facts", "TF", &base.type_facts, &other.type_facts),
        (
            "member_owners",
            "MO",
            &base.member_owners,
            &other.member_owners,
        ),
        (
            "super_types",
            "SUPER",
            &base.super_types,
            &other.super_types,
        ),
        ("signatures", "SIG", &base.signatures, &other.signatures),
        ("flow_trees", "FLOW", &base.flow_trees, &other.flow_trees),
        (
            "signature_casts",
            "CAST",
            &base.signature_casts,
            &other.signature_casts,
        ),
        (
            "special_call_effects",
            "SCEFFECT",
            &base.special_call_effects,
            &other.special_call_effects,
        ),
        (
            "special_call_facts",
            "SCFACT",
            &base.special_call_facts,
            &other.special_call_facts,
        ),
        (
            "special_call_write_sites",
            "SCSITE",
            &base.special_call_write_sites,
            &other.special_call_write_sites,
        ),
        (
            "class_members",
            "CM",
            &base.class_members,
            &other.class_members,
        ),
        (
            "definition_sites",
            "DEFSITE",
            &base.definition_sites,
            &other.definition_sites,
        ),
        (
            "inferred_params",
            "PARAM",
            &base.inferred_params,
            &other.inferred_params,
        ),
    ] {
        diff_map(
            base_label, label, name, prefix, base_map, other_map, limit, &emit,
        );
    }

    // Bulky per-file/per-decl indexes: diffed as sorted hash vectors (see
    // `HashDump`) instead of owned-string maps, so a whole-workspace snapshot
    // does not multiply the harness's peak memory.
    for (name, prefix, base_hash, other_hash) in [
        ("decls", "DECL", &base.decls, &other.decls),
        (
            "type_decl_states",
            "TDECL",
            &base.type_decl_states,
            &other.type_decl_states,
        ),
        (
            "type_decl_generic_params",
            "TGEN",
            &base.type_decl_generic_params,
            &other.type_decl_generic_params,
        ),
        (
            "alias_contributions",
            "ALIASCONTRIB",
            &base.alias_contributions,
            &other.alias_contributions,
        ),
        (
            "decl_references",
            "DREF",
            &base.decl_references,
            &other.decl_references,
        ),
        (
            "reference_ranges",
            "REFRANGE",
            &base.reference_ranges,
            &other.reference_ranges,
        ),
        (
            "index_references",
            "IDXREF",
            &base.index_references,
            &other.index_references,
        ),
        (
            "global_references",
            "GLOBREF",
            &base.global_references,
            &other.global_references,
        ),
        (
            "string_references",
            "STRREF",
            &base.string_references,
            &other.string_references,
        ),
        (
            "type_references",
            "TYPEREF",
            &base.type_references,
            &other.type_references,
        ),
        (
            "local_reassignments",
            "LOCALREASSIGN",
            &base.local_reassignments,
            &other.local_reassignments,
        ),
        ("modules", "MOD", &base.modules, &other.modules),
        ("globals", "GLOBAL", &base.globals, &other.globals),
        (
            "diagnostics_index",
            "DIAG",
            &base.diagnostics,
            &other.diagnostics,
        ),
        (
            "diagnostic_actions",
            "DIAGACT",
            &base.diagnostic_actions,
            &other.diagnostic_actions,
        ),
        ("operators", "OP", &base.operators, &other.operators),
        (
            "dependency_sites",
            "DEP",
            &base.dependency_sites,
            &other.dependency_sites,
        ),
        (
            "gmod_load_files",
            "GLOAD",
            &base.gmod_load_files,
            &other.gmod_load_files,
        ),
        (
            "gmod_load_unresolved_edges",
            "GLOADEDGE",
            &base.gmod_load_unresolved_edges,
            &other.gmod_load_unresolved_edges,
        ),
        (
            "gmod_hook_files",
            "GHOOK",
            &base.gmod_hook_files,
            &other.gmod_hook_files,
        ),
        (
            "gmod_system_files",
            "GSYS",
            &base.gmod_system_files,
            &other.gmod_system_files,
        ),
        (
            "gmod_gm_method_realm_annotations",
            "GMETHREALM",
            &base.gmod_gm_method_realm_annotations,
            &other.gmod_gm_method_realm_annotations,
        ),
        (
            "gmod_realm_files",
            "GREALM",
            &base.gmod_realm_files,
            &other.gmod_realm_files,
        ),
        (
            "gmod_scoped_class_info",
            "GSCOPED",
            &base.gmod_scoped_class_info,
            &other.gmod_scoped_class_info,
        ),
        (
            "gmod_network_wire_coverage",
            "GWIRE",
            &base.gmod_network_wire_coverage,
            &other.gmod_network_wire_coverage,
        ),
        (
            "gmod_class_file_metadata",
            "GCLASS",
            &base.gmod_class_file_metadata,
            &other.gmod_class_file_metadata,
        ),
        (
            "dynamic_field_wildcards",
            "DYNWILD",
            &base.dynamic_field_wildcards,
            &other.dynamic_field_wildcards,
        ),
        (
            "dynamic_fields",
            "DYNFIELD",
            &base.dynamic_fields,
            &other.dynamic_fields,
        ),
        (
            "accessor_func_annotations",
            "ACCANN",
            &base.accessor_func_annotations,
            &other.accessor_func_annotations,
        ),
        (
            "accessor_func_annotation_files",
            "ACCFILES",
            &base.accessor_func_annotation_files,
            &other.accessor_func_annotation_files,
        ),
        (
            "numeric_range_file_populations",
            "NUMFILE",
            &base.numeric_range_file_populations,
            &other.numeric_range_file_populations,
        ),
        (
            "numeric_range_global_populations",
            "NUMGLOBAL",
            &base.numeric_range_global_populations,
            &other.numeric_range_global_populations,
        ),
        (
            "accessor_func_calls",
            "ACCCALL",
            &base.accessor_func_calls,
            &other.accessor_func_calls,
        ),
        ("properties", "PROP", &base.properties, &other.properties),
        ("metatables", "METATBL", &base.metatables, &other.metatables),
        (
            "metatable_factory_bindings",
            "METAFACT",
            &base.metatable_factory_bindings,
            &other.metatable_factory_bindings,
        ),
        (
            "call_site_source_signatures",
            "CSSIG",
            &base.call_site_source_signatures,
            &other.call_site_source_signatures,
        ),
        (
            "call_site_previous_source_signatures",
            "CSPREVSIG",
            &base.call_site_previous_source_signatures,
            &other.call_site_previous_source_signatures,
        ),
        (
            "call_site_signatures_by_path",
            "CSSIGPATH",
            &base.call_site_signatures_by_path,
            &other.call_site_signatures_by_path,
        ),
        (
            "call_site_mutated_params",
            "CSMUT",
            &base.call_site_mutated_params,
            &other.call_site_mutated_params,
        ),
        (
            "call_site_contributions",
            "CSCONTRIB",
            &base.call_site_contributions,
            &other.call_site_contributions,
        ),
        (
            "call_site_contributor_files",
            "CSCONTRIBF",
            &base.call_site_contributor_files,
            &other.call_site_contributor_files,
        ),
        (
            "call_site_contributor_signatures",
            "CSCONTRIBS",
            &base.call_site_contributor_signatures,
            &other.call_site_contributor_signatures,
        ),
        (
            "call_site_pending_previous_params",
            "CSPREV",
            &base.call_site_pending_previous_params,
            &other.call_site_pending_previous_params,
        ),
        (
            "call_site_file_return_consumers",
            "CSFCONS",
            &base.call_site_file_return_consumers,
            &other.call_site_file_return_consumers,
        ),
        (
            "call_site_return_consumers",
            "CSCONS",
            &base.call_site_return_consumers,
            &other.call_site_return_consumers,
        ),
        (
            "call_site_return_consumers_by_signature_file",
            "CSCONSSIGF",
            &base.call_site_return_consumers_by_signature_file,
            &other.call_site_return_consumers_by_signature_file,
        ),
        (
            "call_site_callback_files",
            "CSCB",
            &base.call_site_callback_files,
            &other.call_site_callback_files,
        ),
        (
            "call_site_inference_events",
            "CSEVENTS",
            &base.call_site_inference_events,
            &other.call_site_inference_events,
        ),
        (
            "call_site_source_dependencies",
            "CSDEP",
            &base.call_site_source_dependencies,
            &other.call_site_source_dependencies,
        ),
        (
            "call_site_source_dependents",
            "CSSRCDEP",
            &base.call_site_source_dependents,
            &other.call_site_source_dependents,
        ),
        (
            "call_site_source_file_dependents",
            "CSFILEDEP",
            &base.call_site_source_file_dependents,
            &other.call_site_source_file_dependents,
        ),
        (
            "call_site_source_paths",
            "CSPATH",
            &base.call_site_source_paths,
            &other.call_site_source_paths,
        ),
        (
            "call_site_source_path_dependents",
            "CSPATHDEP",
            &base.call_site_source_path_dependents,
            &other.call_site_source_path_dependents,
        ),
        (
            "schema_files",
            "SCHEMA",
            &base.schema_files,
            &other.schema_files,
        ),
    ] {
        diff_hash_dump(
            base_label, label, name, prefix, base_hash, other_hash, limit, &emit,
        );
    }

    let member_dropped = base.members.difference(&other.members).collect::<Vec<_>>();
    let member_gained = other.members.difference(&base.members).collect::<Vec<_>>();
    if !member_dropped.is_empty() || !member_gained.is_empty() {
        record_divergence();
    }
    println!(
        "INDEX {base_label} -> {label}: members dropped={} gained={}",
        member_dropped.len(),
        member_gained.len()
    );
    for entry in member_dropped.iter().take(limit) {
        emit(format!("  MEM_DROPPED {entry}"));
    }
    for entry in member_gained.iter().take(limit) {
        emit(format!("  MEM_GAINED  {entry}"));
    }

    let base_net = base.net_flows.iter().collect::<BTreeSet<_>>();
    let other_net = other.net_flows.iter().collect::<BTreeSet<_>>();
    let net_dropped = base_net.difference(&other_net).collect::<Vec<_>>();
    let net_gained = other_net.difference(&base_net).collect::<Vec<_>>();
    if !net_dropped.is_empty() || !net_gained.is_empty() {
        record_divergence();
    }
    println!(
        "INDEX {base_label} -> {label}: net_flows dropped={} gained={}",
        net_dropped.len(),
        net_gained.len()
    );
    for entry in net_dropped.iter().take(limit) {
        emit(format!("  NET_DROPPED {}", clip(entry, 400)));
    }
    for entry in net_gained.iter().take(limit) {
        emit(format!("  NET_GAINED  {}", clip(entry, 400)));
    }
}

/// Truncates by characters — entries carry workspace paths and net message
/// names, so a byte slice can land mid-codepoint and panic the whole run.
fn clip(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

fn counts(set: &BTreeSet<Snapshot>) -> (usize, usize) {
    let errors = set.iter().filter(|s| s.severity == 1).count();
    let warnings = set.iter().filter(|s| s.severity == 2).count();
    (errors, warnings)
}

fn file_label(analysis: &EmmyLuaAnalysis, file_id: FileId) -> String {
    analysis
        .compilation
        .get_db()
        .get_vfs()
        .get_file_path(&file_id)
        .map(|path| path.display().to_string().replace('\\', "/"))
        .unwrap_or_else(|| format!("{file_id:?}"))
}

fn diff(base_label: &str, base: &BTreeSet<Snapshot>, label: &str, other: &BTreeSet<Snapshot>) {
    let removed = base.difference(other).collect::<Vec<_>>();
    let added = other.difference(base).collect::<Vec<_>>();
    if removed.is_empty() && added.is_empty() {
        println!(
            "DIFF {base_label} -> {label}: IDENTICAL ({} entries)",
            base.len()
        );
        return;
    }
    println!(
        "DIFF {base_label} -> {label}: removed={} added={}",
        removed.len(),
        added.len()
    );
    record_divergence();

    let limit = diff_limit();

    for (bucket, entries) in [("REMOVED", &removed), ("ADDED", &added)] {
        if entries.is_empty() {
            continue;
        }
        let mut by_code: BTreeMap<&str, usize> = BTreeMap::new();
        let mut by_file: BTreeMap<&str, usize> = BTreeMap::new();
        for entry in entries.iter() {
            *by_code.entry(entry.code.as_str()).or_default() += 1;
            *by_file.entry(entry.file.as_str()).or_default() += 1;
        }
        println!("  {bucket} by code: {by_code:?}");
        let mut files = by_file.into_iter().collect::<Vec<_>>();
        files.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        for (file, count) in files.iter().take(15) {
            println!("  {bucket} by file: {count:>4}  {file}");
        }
        for entry in entries.iter().take(limit) {
            println!("  {bucket} {}", entry.render());
        }
        if entries.len() > limit {
            println!("  {bucket} ... {} more", entries.len() - limit);
        }
    }
}

/// Re-index the targets without touching their text and require the index to
/// come back byte-identical.
///
/// Catches drift the diagnostic gates cannot see: re-analysing a file can attach
/// members or settle a decl's type differently and still produce the same
/// diagnostics.
///
/// Builds its own analysis, as `editrevert` does: sharing one with the other
/// index gate lets whichever runs second measure against an already-converged
/// index.
fn run_index_repeat(codebase: &Path, annotations: &Path, targets: &[String]) {
    let analysis = &mut build_analysis(codebase, annotations);
    for target in targets {
        let path = codebase.join(target.replace('/', std::path::MAIN_SEPARATOR_STR));
        let Some(uri) = glua_code_analysis::file_path_to_uri(&path) else {
            eprintln!("[indexrepeat] cannot build uri for {}", path.display());
            continue;
        };
        let Some(file_id) = analysis.get_file_id(&uri) else {
            eprintln!("[indexrepeat] file not indexed: {}", path.display());
            continue;
        };

        let expanded = analysis.expand_reindex_file_ids(vec![file_id]);
        eprintln!(
            "[indexrepeat] {} re-indexes {} files with no text change",
            target,
            expanded.len()
        );
        if std::env::var_os("DET_DUMP_EXPANSION").is_some() {
            for id in &expanded {
                if let Some(path) = analysis.compilation.get_db().get_vfs().get_file_path(id) {
                    eprintln!("[expansion] {}", path.display());
                }
            }
        }

        // Round 1 measures the cold build against a re-index; every later round
        // measures a re-index against the one before it.
        let rounds = std::env::var("DET_INDEXREPEAT_ROUNDS")
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or(1);
        let mut before = collect_index(analysis, "before_indexrepeat");
        let mut before_diagnostics = collect(analysis, "before_indexrepeat");
        for round in 1..=rounds {
            analysis.reindex_files(vec![file_id]);
            let label = format!("after_indexrepeat[{target}]#{round}");
            let after = collect_index(analysis, &label);
            diff_index(
                &format!("before_indexrepeat#{round}"),
                &before,
                &label,
                &after,
            );
            let after_diagnostics = collect(analysis, &label);
            diff(
                &format!("before_indexrepeat#{round}"),
                &before_diagnostics,
                &label,
                &after_diagnostics,
            );
            before_diagnostics = after_diagnostics;
            before = after;
        }
    }
}

/// Applies a semantically-neutral edit pair and lets the analysis settle.
///
/// `at_front` inserts the newline at the start of the file, shifting every
/// offset so the update gate cannot skip the re-index. Otherwise it appends,
/// which the gate can skip outright.
fn noop_edit(analysis: &mut EmmyLuaAnalysis, target: &Path, at_front: bool) -> bool {
    let tag = if at_front { "editmid" } else { "noopedit" };
    let Some(uri) = glua_code_analysis::file_path_to_uri(&target.to_path_buf()) else {
        eprintln!("[{tag}] cannot build uri for {}", target.display());
        return false;
    };
    let Some(file_id) = analysis.get_file_id(&uri) else {
        eprintln!("[{tag}] file not indexed: {}", target.display());
        return false;
    };
    let Some(original) = analysis
        .compilation
        .get_db()
        .get_vfs()
        .get_file_content(&file_id)
        .cloned()
    else {
        eprintln!("[{tag}] no content for {}", target.display());
        return false;
    };

    let expanded = analysis.expand_reindex_file_ids(vec![file_id]);
    eprintln!(
        "[{tag}] {} expands to {} files{}",
        target.display(),
        expanded.len(),
        if at_front {
            ""
        } else {
            " (the re-index the semantic no-op gate must skip, not run)"
        }
    );
    if std::env::var_os("DET_SHOW_EXPANSION").is_some() {
        for expanded_id in &expanded {
            eprintln!("[{tag}]   {}", file_label(analysis, *expanded_id));
        }
    }

    let t = Instant::now();
    let edited = if at_front {
        format!(
            "
{original}"
        )
    } else {
        format!(
            "{original}
"
        )
    };
    analysis
        .update_file_by_uri(&uri, Some(edited))
        .map(|(id, _)| id);
    analysis
        .update_file_by_uri(&uri, Some(original))
        .map(|(id, _)| id);
    eprintln!(
        "[{tag}] no-op newline edit pair on {} ({:.2}s)",
        target.display(),
        t.elapsed().as_secs_f64()
    );
    true
}

/// Single shift-only settle through the production two-phase path, asserting
/// the remap-path invariant and printing the dirty breakdown as diagnostics.
///
/// Applies ONE edit (a blank line at the front, shifting every offset with no
/// token change) via `update_file_text_only` + `self_index_and_diff` +
/// `ripple`, on its own analysis like `editrevert`.
///
/// Asserted (each records a divergence):
/// 1. `remap-loss == 0`. Defensible for this stage specifically because its
///    edit is a pure insertion of a blank line at the front: no fact's
///    referent can disappear and no text a fact anchors on is deleted, so any
///    loss means the validator failed to *see* a fact the shift never touched.
///    This is not a universal claim about remapping: a genuinely deleted
///    `---@class` must still lose its facts (lossy by design, covered by the
///    remap negative case, not this stage).
/// 2. Accounting completeness: every file in `dirty` is attributable to a
///    named source — the edited file itself, a remap loss, or a diff
///    dependent — so a file dirtied by nothing is a bug.
///
/// Deliberately NOT asserted: `export diff empty`. False on drift targets
/// whose owner/accumulator families still move under a shift (`cl_umsgs`' 2
/// member keys, `sh_configuration`'s 1 dynamic-element key); `indexrepeat` is
/// the gate that owns that. The key count and keys print as diagnostics.
///
/// The before/after comparison is a *measurement*, not a gate: a shifted
/// file's own facts legitimately move by a line, so cross-input equality is
/// not expected and the diff prints without recording a divergence. The
/// second identical settle (which must be fully quiet) is the determinism
/// gate: quiet there proves no lingering corruption, so a first-settle fanout
/// is a convergence phenomenon the tail owns.
fn shift_settle(codebase: &Path, annotations: &Path, targets: &[String]) {
    let analysis = &mut build_analysis(codebase, annotations);

    for target in targets {
        let path = codebase.join(target.replace('/', std::path::MAIN_SEPARATOR_STR));
        let Some(uri) = glua_code_analysis::file_path_to_uri(&path) else {
            continue;
        };
        let Some(file_id) = analysis.get_file_id(&uri) else {
            eprintln!("[shiftsettle] file not indexed: {}", path.display());
            continue;
        };
        let Some(original) = analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_content(&file_id)
            .cloned()
        else {
            continue;
        };
        let shifted = format!("\n{original}");

        let before_index = collect_index(analysis, "before_shiftsettle");
        let before = collect(analysis, "before_shiftsettle");

        // Phase 0: the dependency expansion, captured pre-edit like the LSP
        // does. `self_index_and_diff` itself takes no expansion; what it
        // reports is remap-loss plus diff-driven only.
        let expansion = analysis.expand_reindex_file_ids(vec![file_id]);
        eprintln!(
            "[shiftsettle] {target}: dependency expansion is {} file(s)",
            expansion.len()
        );

        let t = Instant::now();
        analysis.update_file_text_only(&uri, shifted.clone());
        let mut dirty: DirtySet = analysis.self_index_and_diff(vec![file_id]);
        let dirty_len = dirty.dirty_len();
        let refresh_len = dirty.textual_refresh_names_len();
        eprintln!(
            "[shiftsettle] {target}: self-index dirty_len={dirty_len} refresh_names={refresh_len} ({:.2}s)",
            t.elapsed().as_secs_f64()
        );
        // Asserted invariant 1: a pure insertion loses no remap identity.
        // Printed as diagnostics either way (sorted ids), asserted by count.
        {
            let mut losses: Vec<_> = dirty.remap_losses().iter().collect();
            losses.sort_unstable();
            eprintln!(
                "[shiftsettle] {target}: remap-loss {} file(s): {losses:?}",
                losses.len()
            );
            if !losses.is_empty() {
                record_divergence();
            }
        }
        // Asserted invariant 2: every dirty file has a named source. The
        // edited file itself is settled by phase 1 and never dirtied, so the
        // union of remap losses and diff dependents must cover `dirty`.
        {
            let mut unattributed: Vec<_> = Vec::new();
            for file in dirty.dirty_files() {
                if *file == file_id
                    || dirty.remap_losses().contains(file)
                    || dirty.diff_dependents().contains(file)
                {
                    continue;
                }
                unattributed.push(file);
            }
            unattributed.sort_unstable();
            let mut dependents: Vec<_> = dirty.diff_dependents().iter().collect();
            dependents.sort_unstable();
            eprintln!(
                "[shiftsettle] {target}: diff-dependents {} file(s): {dependents:?}; unattributed {} file(s): {unattributed:?}",
                dependents.len(),
                unattributed.len()
            );
            if !unattributed.is_empty() {
                record_divergence();
            }
        }
        let phase1_names: Vec<String> = dirty
            .take_textual_refresh_names()
            .into_iter()
            .map(|name| name.as_str().to_string())
            .collect();
        let (rippled, ripple_names) = analysis.ripple_with_refresh_names(dirty);
        let mut phase1_sorted = phase1_names;
        phase1_sorted.sort_unstable();
        let mut ripple_sorted: Vec<String> = ripple_names
            .into_iter()
            .map(|name| name.as_str().to_string())
            .collect();
        ripple_sorted.sort_unstable();
        eprintln!(
            "[shiftsettle] {target}: ripple reanalysed {} file(s), phase1_names={} ripple_names={}",
            rippled.len(),
            phase1_sorted.len(),
            ripple_sorted.len(),
        );
        for name in phase1_sorted.iter().take(60) {
            eprintln!("[shiftsettle] {target}:   phase1 name `{name}`");
        }
        for name in ripple_sorted.iter().take(60) {
            eprintln!("[shiftsettle] {target}:   ripple name `{name}`");
        }
        let label = format!("after_shiftsettle[{target}]");
        let after_index = collect_index(analysis, &label);
        let after = collect(analysis, &label);
        // Cross-input measurement, not a gate: the shifted file's facts
        // legitimately move by a line, so before/after cannot be identical.
        measure(|| {
            diff_index("before_shiftsettle", &before_index, &label, &after_index);
            diff("before_shiftsettle", &before, &label, &after);
        });

        // Determinism check the parent asked for: settle the SAME shifted text
        // a second time through the identical path and compare the loss sets.
        // A repeat loss means the remap path itself drifts rather than the
        // first settle being a one-shot artefact.
        let t = Instant::now();
        analysis.update_file_text_only(&uri, shifted.clone());
        let dirty2: DirtySet = analysis.self_index_and_diff(vec![file_id]);
        let dirty2_len = dirty2.dirty_len();
        eprintln!(
            "[shiftsettle] {target}: second identical settle dirty_len={dirty2_len} ({:.2}s)",
            t.elapsed().as_secs_f64()
        );
        analysis.ripple(dirty2);
        let label2 = format!("after_shiftsettle2[{target}]");
        let after2_index = collect_index(analysis, &label2);
        let after2 = collect(analysis, &label2);
        diff_index(&label, &after_index, &label2, &after2_index);
        diff(&label, &after, &label2, &after2);
    }
}

/// Edit a file through the single-file update path, then put it back.
///
/// The source ends as it started, so the index has to as well. Any difference is
/// drift the edit path introduced: state the edit added and the revert did not
/// take away, or state the edit dropped and the revert did not restore.
///
/// The update-path counterpart of `indexrepeat`, which re-indexes with the text
/// untouched and so never exercises an edit's invalidation. Needs no
/// ground-truth build, because the pre-edit index is the truth.
///
/// Builds its own analysis: both index gates re-index in place and leave a
/// converged index behind, so whichever ran second would measure against the
/// other's converged state.
fn edit_revert(codebase: &Path, annotations: &Path, targets: &[String]) {
    let Ok(find) = std::env::var("DET_EDIT_FIND") else {
        eprintln!("[editrevert] SKIPPED: DET_EDIT_FIND is not set, so no drift gate ran");
        return;
    };
    let replace = std::env::var("DET_EDIT_REPLACE").unwrap_or_default();
    let analysis = &mut build_analysis(codebase, annotations);

    for target in targets {
        let path = codebase.join(target.replace('/', std::path::MAIN_SEPARATOR_STR));
        let Some(uri) = glua_code_analysis::file_path_to_uri(&path) else {
            continue;
        };
        let Some(file_id) = analysis.get_file_id(&uri) else {
            eprintln!("[editrevert] file not indexed: {}", path.display());
            continue;
        };
        let Some(original) = analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_content(&file_id)
            .cloned()
        else {
            continue;
        };
        if !original.contains(find.as_str()) {
            eprintln!("[editrevert] {find:?} not present in {}", path.display());
            continue;
        }
        let edited = original.replace(find.as_str(), replace.as_str());

        let before_index = collect_index(analysis, "before_editrevert");
        let before = collect(analysis, "before_editrevert");

        let t = Instant::now();
        analysis.update_file_text_only(&uri, edited);
        let dirty = analysis.self_index_and_diff(vec![file_id]);
        analysis.ripple(dirty);

        analysis.update_file_text_only(&uri, original);
        let dirty_undo = analysis.self_index_and_diff(vec![file_id]);
        analysis.ripple(dirty_undo);
        eprintln!(
            "[editrevert] {target} edited and reverted ({:.2}s)",
            t.elapsed().as_secs_f64()
        );

        let label = format!("after_editrevert[{target}]");
        let after_index = collect_index(analysis, &label);
        let after = collect(analysis, &label);
        diff_index("before_editrevert", &before_index, &label, &after_index);
        diff("before_editrevert", &before, &label, &after);
    }
}

/// Applies a **real** edit and compares the incremental result against a cold
/// build of the same edited source.
///
/// The only edit stage whose edit changes what the file means, so it also
/// measures how much of the index an edit moves. That bounds what a cheaper
/// re-index mechanism has to reproduce.
fn real_edit(
    analysis: &mut EmmyLuaAnalysis,
    codebase: &Path,
    annotations: &Path,
    targets: &[String],
    cold: &BTreeSet<Snapshot>,
) {
    // In the default stage set, so a missing edit spec must skip loudly rather
    // than abort the run and lose every stage after it.
    let Ok(find) = std::env::var("DET_EDIT_FIND") else {
        eprintln!("[realedit] SKIPPED: DET_EDIT_FIND is not set, so no edit gate ran");
        return;
    };
    let replace = std::env::var("DET_EDIT_REPLACE").unwrap_or_default();
    let cold_index = collect_index(analysis, "cold");

    for target in targets {
        let path = codebase.join(target.replace('/', std::path::MAIN_SEPARATOR_STR));
        let Some(uri) = glua_code_analysis::file_path_to_uri(&path) else {
            continue;
        };
        let Some(file_id) = analysis.get_file_id(&uri) else {
            eprintln!("[realedit] file not indexed: {}", path.display());
            continue;
        };
        let Some(original) = analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_content(&file_id)
            .cloned()
        else {
            continue;
        };
        let matches = original.matches(find.as_str()).count();
        if matches == 0 {
            eprintln!("[realedit] {find:?} not present in {}", path.display());
            continue;
        }
        let edited = original.replace(find.as_str(), replace.as_str());

        let expanded = analysis.expand_reindex_file_ids(vec![file_id]);
        let t = Instant::now();
        analysis
            .update_file_by_uri(&uri, Some(edited.clone()))
            .map(|(id, _)| id);
        eprintln!(
            "[realedit] {} ({matches} site(s)) expands to {} files ({:.2}s)",
            target,
            expanded.len(),
            t.elapsed().as_secs_f64()
        );

        let warm_index = collect_index(analysis, "warm");
        let warm = collect(analysis, "warm");

        let ground_truth = build_analysis_with(
            codebase,
            annotations,
            Order::Natural,
            1,
            &[(path.clone(), edited)],
            None,
        );
        let truth_index = collect_index(&ground_truth, "cold_edited");
        let truth = collect(&ground_truth, "cold_edited");

        // What the edit genuinely moves: the work any mechanism must
        // reproduce. A measurement, not a gate — the original and edited
        // workspaces legitimately differ.
        measure(|| {
            diff_index("cold", &cold_index, "cold_edited", &truth_index);
            diff("cold", cold, "cold_edited", &truth);
        });
        // What today's incremental path gets wrong about it.
        diff_index("cold_edited", &truth_index, "warm", &warm_index);
        diff("cold_edited", &truth, "warm", &warm);
        analysis
            .update_file_by_uri(&uri, Some(original))
            .map(|(id, _)| id);
    }
}

/// Gate how much work one edit costs, rather than what it produces.
///
/// Every other stage compares results, so all of them stay green when an edit
/// still lands on the right answer by re-analysing the entire workspace to get
/// there. That is exactly how a settle path regressed from three files to 1507
/// without a single gate noticing.
///
/// The dirty set names the files the edit invalidated, so its size is the blast
/// radius directly. `DET_BLAST_MAX` is the ceiling per target, defaulting to a
/// value far below any dependency expansion but far above a real edit's fanout.
/// Exceeding it is a hard failure, like any non-identical comparison.
fn blast_radius(analysis: &mut EmmyLuaAnalysis, codebase: &Path, targets: &[String]) -> bool {
    let Ok(find) = std::env::var("DET_EDIT_FIND") else {
        eprintln!("[blast] SKIPPED: DET_EDIT_FIND is not set, so no blast gate ran");
        return true;
    };
    let replace = std::env::var("DET_EDIT_REPLACE").unwrap_or_default();
    let max = std::env::var("DET_BLAST_MAX")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(50);

    let mut ok = true;
    for target in targets {
        let path = codebase.join(target.replace('/', std::path::MAIN_SEPARATOR_STR));
        let Some(uri) = glua_code_analysis::file_path_to_uri(&path) else {
            continue;
        };
        let Some(file_id) = analysis.get_file_id(&uri) else {
            eprintln!("[blast] file not indexed: {}", path.display());
            continue;
        };
        let Some(original) = analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_content(&file_id)
            .cloned()
        else {
            continue;
        };
        if !original.contains(find.as_str()) {
            eprintln!("[blast] {find:?} not present in {}", path.display());
            continue;
        }

        let edited = original.replace(find.as_str(), replace.as_str());
        analysis.update_file_text_only(&uri, edited);
        let dirty = analysis.self_index_and_diff(vec![file_id]);
        let dirty_len = dirty.dirty_len();
        analysis.ripple(dirty);

        if dirty_len > max {
            ok = false;
            eprintln!(
                "[blast] FAIL {target}: the edit dirtied {dirty_len} file(s), over the {max} ceiling"
            );
        } else {
            eprintln!("[blast] PASS {target}: the edit dirtied {dirty_len} file(s) (max {max})");
        }

        analysis
            .update_file_by_uri(&uri, Some(original))
            .map(|(id, _)| id);
    }
    ok
}

/// Gate the deferred ripple: several self-indexes, then one ripple over the
/// dirty sets they accumulated.
///
/// The LSP debounce self-indexes each drained edit on a short timer and owes the
/// dependency ripple afterwards. Deferring that ripple behind a longer idle
/// timer means a typing burst produces several self-indexes before one ripple,
/// so the ripple runs against the union of dirty sets those self-indexes
/// produced — which is what `DebouncedAnalysis::run` accumulates in `owed_ripple`
/// and what this stage reproduces.
///
/// A dirty set names the files that read a fact an edit moved. Accumulating
/// several before rippling can only be correct if a later edit's set does not
/// invalidate what an earlier one already reported, which is what this stage
/// measures. `control` applies the same edits one settle at a time, so a failure
/// separates the deferral from the path underneath it.
fn burst_edit(
    analysis: &mut EmmyLuaAnalysis,
    codebase: &Path,
    annotations: &Path,
    targets: &[String],
    cold: &BTreeSet<Snapshot>,
) {
    let Ok(find) = std::env::var("DET_EDIT_FIND") else {
        eprintln!("[burst] SKIPPED: DET_EDIT_FIND is not set, so no burst gate ran");
        return;
    };
    let replace = std::env::var("DET_EDIT_REPLACE").unwrap_or_default();
    let cold_index = collect_index(analysis, "cold");

    struct Target {
        path: std::path::PathBuf,
        uri: lsp_types::Uri,
        file_id: FileId,
        original: String,
    }

    let mut resolved = Vec::new();
    for target in targets {
        let path = codebase.join(target.replace('/', std::path::MAIN_SEPARATOR_STR));
        let Some(uri) = glua_code_analysis::file_path_to_uri(&path) else {
            continue;
        };
        let Some(file_id) = analysis.get_file_id(&uri) else {
            eprintln!("[burst] file not indexed: {}", path.display());
            continue;
        };
        let Some(original) = analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_content(&file_id)
            .cloned()
        else {
            continue;
        };
        if !original.contains(find.as_str()) {
            eprintln!("[burst] {find:?} not present in {}", path.display());
            continue;
        }
        resolved.push(Target {
            path,
            uri,
            file_id,
            original,
        });
    }

    if resolved.is_empty() {
        eprintln!("[burst] SKIPPED: no target matched, so no burst gate ran");
        return;
    }

    // Three keystroke groups: change meaning, shift offsets only, and introduce
    // a class definition partway through.
    let step_text = move |target: &Target, step: usize| -> String {
        let mut text = target.original.replace(find.as_str(), replace.as_str());
        if step >= 1 {
            text.push_str("\n-- burst\n");
        }
        if step >= 2 {
            text.push_str("\n---@class DetBurstClass\nlocal DetBurst = {}\n");
        }
        text
    };

    // The shape `DebouncedAnalysis::run` drives in production: each drained edit
    // installs its text and self-indexes under one lock, the dirty set it
    // produces joins the one already owed, and a single ripple settles the whole
    // burst once typing stops. Gating the expansion instead measured a path
    // production no longer takes.
    let mut owed: Option<DirtySet> = None;
    let t = Instant::now();
    for step in 0..3 {
        for target in &resolved {
            analysis.update_file_text_only(&target.uri, step_text(target, step));
            let dirty = analysis.self_index_and_diff(vec![target.file_id]);
            match &mut owed {
                Some(owed) => owed.extend(dirty),
                None => owed = Some(dirty),
            }
        }
    }

    let self_indexed = t.elapsed();
    let owed_len = owed.as_ref().map_or(0, DirtySet::dirty_len);
    let t = Instant::now();
    if let Some(owed) = owed {
        analysis.ripple(owed);
    }
    eprintln!(
        "[burst] {} file(s) x 3 edits: {} self-index(es) in {:.2}s, then one ripple over {owed_len} owed file(s) ({:.2}s)",
        resolved.len(),
        resolved.len() * 3,
        self_indexed.as_secs_f64(),
        t.elapsed().as_secs_f64()
    );

    let warm_index = collect_index(analysis, "warm");
    let warm = collect(analysis, "warm");

    // The control: the same three edits through today's path, one ripple each.
    // Localises a failure to the deferral or to the path underneath it.
    for target in &resolved {
        analysis
            .update_file_by_uri(&target.uri, Some(target.original.clone()))
            .map(|(id, _)| id);
    }
    let t = Instant::now();
    for step in 0..3 {
        for target in &resolved {
            analysis
                .update_file_by_uri(&target.uri, Some(step_text(target, step)))
                .map(|(id, _)| id);
        }
    }
    eprintln!(
        "[burst] control: same edits through {} separate ripples ({:.2}s)",
        resolved.len() * 3,
        t.elapsed().as_secs_f64()
    );
    let control_index = collect_index(analysis, "control");
    let control = collect(analysis, "control");

    let overrides = resolved
        .iter()
        .map(|target| (target.path.clone(), step_text(target, 2)))
        .collect::<Vec<_>>();
    let ground_truth =
        build_analysis_with(codebase, annotations, Order::Natural, 1, &overrides, None);
    let truth_index = collect_index(&ground_truth, "cold_burst");
    let truth = collect(&ground_truth, "cold_burst");

    // Context, not a gate: the burst's final text legitimately differs from
    // the original workspace, so this cross-input distance is a measurement.
    measure(|| {
        diff_index("cold", &cold_index, "cold_burst", &truth_index);
        diff("cold", cold, "cold_burst", &truth);
    });
    // The gates: both the deferred burst and the one-settle-at-a-time control
    // must land where a cold build of the same final text lands.
    diff_index("cold_burst", &truth_index, "control", &control_index);
    diff("cold_burst", &truth, "control", &control);
    diff_index("cold_burst", &truth_index, "warm", &warm_index);
    diff("cold_burst", &truth, "warm", &warm);

    for target in &resolved {
        analysis
            .update_file_by_uri(&target.uri, Some(target.original.clone()))
            .map(|(id, _)| id);
    }
}

fn delete_reopen(codebase: &Path, annotations: &Path, targets: &[String]) {
    for target in targets {
        let path = codebase.join(target.replace("/", std::path::MAIN_SEPARATOR_STR));
        let Some(uri) = glua_code_analysis::file_path_to_uri(&path) else {
            eprintln!("[delete] cannot build uri for {}", path.display());
            std::process::exit(2);
        };
        let mut warm = build_analysis(codebase, annotations);
        // A target that resolves to no indexed text exercises neither the
        // delete nor the reopen half, so it is a fatal prerequisite error,
        // not a skipped gate.
        let Some(original) = warm.get_file_id(&uri).and_then(|id| {
            warm.compilation
                .get_db()
                .get_vfs()
                .get_file_content(&id)
                .cloned()
        }) else {
            eprintln!("[delete] target not indexed: {}", path.display());
            std::process::exit(2);
        };
        // Delete: the warm analysis drops the file through the same
        // `update_file_by_uri` path the LSP uses. Its baseline is a cold build
        // that never contained the file — removing it after indexing would
        // only reproduce the warm path, not the state a workspace without the
        // file starts from. The cold build reserves the file's id as an
        // unindexed tombstone (see `build_analysis_with`), so embedded file
        // ids match the warm side's retained allocation. Both the diagnostics
        // and the full index must match.
        warm.update_file_by_uri(&uri, None);
        let cold_deleted = build_analysis_excluding(codebase, annotations, &path);
        let deleted = collect(&warm, "warm_delete");
        let baseline = collect(&cold_deleted, "cold_delete");
        diff("cold_delete", &baseline, "warm_delete", &deleted);
        let deleted_index = collect_index(&warm, "warm_delete");
        let baseline_index = collect_index(&cold_deleted, "cold_delete");
        diff_index(
            "cold_delete",
            &baseline_index,
            "warm_delete",
            &deleted_index,
        );
        // Reopen: the same warm analysis gets the file's text back. Its
        // baseline is the ordinary cold build of the full workspace.
        warm.update_file_by_uri(&uri, Some(original));
        let reopened = collect(&warm, "warm_reopen");
        let cold = build_analysis(codebase, annotations);
        let baseline = collect(&cold, "cold_reopen");
        diff("cold_reopen", &baseline, "warm_reopen", &reopened);
        let reopened_index = collect_index(&warm, "warm_reopen");
        let cold_index = collect_index(&cold, "cold_reopen");
        diff_index("cold_reopen", &cold_index, "warm_reopen", &reopened_index);
    }
}

fn reindex_exact(analysis: &mut EmmyLuaAnalysis, codebase: &Path, relatives: &[String]) -> bool {
    let mut file_ids = Vec::new();
    for relative in relatives {
        let path = codebase.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        let Some(uri) = glua_code_analysis::file_path_to_uri(&path) else {
            continue;
        };
        let Some(file_id) = analysis.get_file_id(&uri) else {
            eprintln!("[exact] not indexed: {}", path.display());
            continue;
        };
        file_ids.push(file_id);
    }
    if file_ids.is_empty() {
        return false;
    }
    let t = Instant::now();
    analysis.reindex_files_without_expansion(file_ids.clone());
    eprintln!(
        "[exact] reindexed {} files ({:.2}s)",
        file_ids.len(),
        t.elapsed().as_secs_f64()
    );
    true
}

/// Analysis recurses over deeply nested syntax. The server runs it on spawned
/// threads, which get a larger stack than a Windows process main thread.
fn main() {
    std::thread::Builder::new()
        .stack_size(glua_code_analysis::ANALYSIS_STACK_SIZE)
        .spawn(run)
        .expect("determinism worker thread should spawn")
        .join()
        .expect("determinism worker thread should not panic");
}

fn run() {
    alloc_sample::init();
    let codebase =
        PathBuf::from(std::env::var("DET_CODEBASE").expect("DET_CODEBASE env var is required"));
    let annotations = PathBuf::from(
        std::env::var("DET_ANNOTATIONS").expect("DET_ANNOTATIONS env var is required"),
    );
    let stages = std::env::var("DET_STAGES")
        .unwrap_or_else(|_| "repeat,noopedit,editrevert,realedit,indexrepeat".to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    let targets = std::env::var("DET_TARGETS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();

    let target_stages = [
        "shiftsettle",
        "noopedit",
        "editmid",
        "editrevert",
        "realedit",
        "blast",
        "burst",
        "exact",
        "batchexpand",
        "indexrepeat",
        "delete",
        "reopen",
    ];
    // Stages that consume DET_EDIT_FIND/DET_EDIT_REPLACE. `shiftsettle` and
    // `editmid` are newline-only edits and are deliberately absent.
    let edit_stages = ["editrevert", "realedit", "blast", "burst"];
    if stages
        .iter()
        .any(|stage| target_stages.iter().any(|want| stage == want))
        && targets.is_empty()
    {
        eprintln!("[determinism] target-dependent stage selected but DET_TARGETS is empty");
        std::process::exit(2);
    }
    if stages
        .iter()
        .any(|stage| edit_stages.iter().any(|want| stage == want))
    {
        let find = std::env::var("DET_EDIT_FIND").ok().filter(|value| !value.is_empty()).unwrap_or_else(|| { eprintln!("[determinism] edit-dependent stage selected but DET_EDIT_FIND is absent or empty"); std::process::exit(2) });
        let matched = targets
            .iter()
            .filter_map(|target| {
                std::fs::read_to_string(
                    codebase.join(target.replace("/", std::path::MAIN_SEPARATOR_STR)),
                )
                .ok()
            })
            .map(|text| text.matches(&find).count())
            .sum::<usize>();
        if matched == 0 {
            eprintln!(
                "[determinism] edit-dependent stage selected but DET_EDIT_FIND matches zero occurrences in selected targets"
            );
            std::process::exit(2);
        }
    }

    let mut analysis = build_analysis(&codebase, &annotations);

    // Every selected target must resolve to indexed text before any stage
    // runs: a misspelled or workspace-excluded DET_TARGETS entry would
    // otherwise let a stage skip its gate and still exit successfully.
    if stages
        .iter()
        .any(|stage| target_stages.iter().any(|want| stage == want))
    {
        for target in &targets {
            let path = codebase.join(target.replace('/', std::path::MAIN_SEPARATOR_STR));
            let resolved = glua_code_analysis::file_path_to_uri(&path)
                .and_then(|uri| analysis.get_file_id(&uri))
                .and_then(|file_id| {
                    analysis
                        .compilation
                        .get_db()
                        .get_vfs()
                        .get_file_content(&file_id)
                });
            if resolved.is_none() {
                eprintln!(
                    "[determinism] DET_TARGETS entry does not resolve to indexed text: {target}"
                );
                std::process::exit(2);
            }
        }
    }
    let cold = collect(&analysis, "cold");

    if let Ok(dump_path) = std::env::var("DET_DUMP") {
        let rendered = cold
            .iter()
            .map(Snapshot::render)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&dump_path, rendered).expect("dump write should succeed");
        eprintln!("[cold] dumped to {dump_path}");
    }

    // A cross-process comparison needs the cold snapshot even when no stage runs
    // that would otherwise collect one.
    if std::env::var_os("DET_DUMP_INDEX").is_some() {
        collect_index(&analysis, "cold");
    }

    let cold_index = (stages
        .iter()
        .any(|stage| matches!(stage.as_str(), "fresh" | "order" | "reindex")))
    .then(|| collect_index(&analysis, "cold"));

    // Repeated collection on the same retained state, with no intervening
    // update: a read-side mutation or cache-dependent second result shows up
    // as a divergence between the two collections. The stage builds its own
    // analysis so neither collection inherits another stage's warm state.
    if stages.iter().any(|s| s == "repeat") {
        analysis = build_analysis(&codebase, &annotations);
        let baseline = collect(&analysis, "repeat_base");
        let baseline_index = collect_index(&analysis, "repeat_base");
        let again = collect(&analysis, "repeat");
        diff("repeat_base", &baseline, "repeat", &again);
        let after_index = collect_index(&analysis, "repeat");
        diff_index("repeat_base", &baseline_index, "repeat", &after_index);
    }

    if stages.iter().any(|s| s == "noopedit") {
        analysis = build_analysis(&codebase, &annotations);
        let mut previous = cold.clone();
        let mut previous_label = "cold".to_string();
        let cold_index = Some(collect_index(&analysis, "cold"));
        for target in &targets {
            let path = codebase.join(target.replace('/', std::path::MAIN_SEPARATOR_STR));
            if !noop_edit(&mut analysis, &path, false) {
                continue;
            }
            let label = format!("after_noopedit[{target}]");
            if let Some(cold_index) = &cold_index {
                let after_index = collect_index(&analysis, &label);
                diff_index("cold", cold_index, &label, &after_index);
            }
            let after = collect(&analysis, &label);
            diff("cold", &cold, &label, &after);
            if previous_label != "cold" {
                diff(&previous_label, &previous, &label, &after);
            }
            previous = after;
            previous_label = label;
        }
    }

    if stages.iter().any(|s| s == "editmid") {
        analysis = build_analysis(&codebase, &annotations);
        let cold_index = Some(collect_index(&analysis, "cold"));
        for target in &targets {
            let path = codebase.join(target.replace('/', std::path::MAIN_SEPARATOR_STR));
            if !noop_edit(&mut analysis, &path, true) {
                continue;
            }
            let label = format!("after_editmid[{target}]");
            if let Some(cold_index) = &cold_index {
                let after_index = collect_index(&analysis, &label);
                diff_index("cold", cold_index, &label, &after_index);
            }
            let after = collect(&analysis, &label);
            diff("cold", &cold, &label, &after);
        }
    }

    if stages.iter().any(|s| s == "exact") {
        analysis = build_analysis(&codebase, &annotations);
        let cold_index = collect_index(&analysis, "cold");
        if reindex_exact(&mut analysis, &codebase, &targets) {
            let after_index = collect_index(&analysis, "after_exact");
            diff_index("cold", &cold_index, "after_exact", &after_index);
            let after = collect(&analysis, "after_exact");
            diff("cold", &cold, "after_exact", &after);
        }
    }

    // Re-run the pipeline over every main-workspace file with the DB already
    // populated. Tests whether the cold build under-resolved because library and
    // std groups were analysed before main-workspace files existed.
    if stages.iter().any(|s| s == "mainreindex") {
        analysis = build_analysis(&codebase, &annotations);
        let cold_index = collect_index(&analysis, "cold");
        let main_ids = analysis.get_main_workspace_file_ids_for_diagnostics();
        let t = Instant::now();
        analysis.reindex_files_without_expansion(main_ids.clone());
        eprintln!(
            "[mainreindex] reindexed {} main files ({:.2}s)",
            main_ids.len(),
            t.elapsed().as_secs_f64()
        );
        let after_index = collect_index(&analysis, "after_mainreindex");
        diff_index("cold", &cold_index, "after_mainreindex", &after_index);
        let after = collect(&analysis, "after_mainreindex");
        diff("cold", &cold, "after_mainreindex", &after);

        let t = Instant::now();
        analysis.reindex_files_without_expansion(main_ids);
        eprintln!(
            "[mainreindex] second pass ({:.2}s)",
            t.elapsed().as_secs_f64()
        );
        let second = collect(&analysis, "after_mainreindex_2");
        diff("after_mainreindex", &after, "after_mainreindex_2", &second);
    }

    // Remove and re-add every file without clearing the index first. Cold and
    // this stage analyse the same file set from the same starting point, so a
    // difference isolates state `remove_index` fails to drop.
    if stages.iter().any(|s| s == "allreindex") {
        analysis = build_analysis(&codebase, &annotations);
        let cold_index = collect_index(&analysis, "cold");
        let all_ids = analysis.compilation.get_db().get_vfs().get_all_file_ids();
        let t = Instant::now();
        let file_count = all_ids.len();
        analysis.reindex_files_without_expansion(all_ids);
        eprintln!(
            "[allreindex] reindexed {file_count} files ({:.2}s)",
            t.elapsed().as_secs_f64()
        );
        let after_index = collect_index(&analysis, "after_allreindex");
        diff_index("cold", &cold_index, "after_allreindex", &after_index);
        let after = collect(&analysis, "after_allreindex");
        diff("cold", &cold, "after_allreindex", &after);
    }

    // Remove and re-index every file through the supported remove-first
    // path, so each file is re-derived against the settled index the
    // previous round left behind. Says whether the retained-state path
    // reaches the cold answer, and whether repeated rounds converge.
    if stages.iter().any(|s| s == "restabilize") {
        analysis = build_analysis(&codebase, &annotations);
        let all_ids = analysis.compilation.get_db().get_vfs().get_all_file_ids();
        // Immutable baselines captured before the first retained-state
        // update: every round compares against the state the cold build left,
        // so index drift and diagnostic-only non-confluence are both visible.
        let baseline_index = collect_index(&analysis, "cold");
        let baseline = collect(&analysis, "cold");
        for round in 0..3 {
            let t = Instant::now();
            analysis.reindex_files_without_expansion(all_ids.clone());
            let label = format!("after_restabilize_{round}");
            eprintln!(
                "[restabilize] round {round} reindexed {} files ({:.2}s)",
                all_ids.len(),
                t.elapsed().as_secs_f64()
            );
            let after_index = collect_index(&analysis, &label);
            diff_index("cold", &baseline_index, &label, &after_index);
            let after = collect(&analysis, &label);
            diff("cold", &baseline, &label, &after);
        }
    }

    // Remove and re-add each file on its own, so every file is re-derived
    // against the complete settled workspace rather than the prefix the cold
    // walk had built when it reached that file.
    if stages.iter().any(|s| s == "perfile") {
        analysis = build_analysis(&codebase, &annotations);
        let all_ids = analysis.compilation.get_db().get_vfs().get_all_file_ids();
        let rounds = std::env::var("DET_PERFILE_ROUNDS")
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .unwrap_or(2);
        let mut previous_index = collect_index(&analysis, "cold");
        let mut previous_diagnostics = cold.clone();
        let mut previous_label = "cold".to_string();
        for round in 1..=rounds {
            let t = Instant::now();
            for file_id in &all_ids {
                analysis.reindex_files_without_expansion(vec![*file_id]);
            }
            let label = format!("after_perfile_{round}");
            eprintln!(
                "[perfile] round {round} over {} files ({:.2}s)",
                all_ids.len(),
                t.elapsed().as_secs_f64()
            );
            let after_index = collect_index(&analysis, &label);
            diff_index(&previous_label, &previous_index, &label, &after_index);
            let after = collect(&analysis, &label);
            diff(&previous_label, &previous_diagnostics, &label, &after);
            previous_index = after_index;
            previous_diagnostics = after;
            previous_label = label;
        }
    }

    // The production incremental path: phase 1 re-indexes the set and reports
    // what it invalidated, phase 2 re-analyses only that.
    if stages.iter().any(|s| s == "mainexpand") {
        analysis = build_analysis(&codebase, &annotations);
        let cold_index = collect_index(&analysis, "cold");
        let main_ids = analysis.get_main_workspace_file_ids_for_diagnostics();
        let t = Instant::now();
        let dirty = analysis.self_index_and_diff(main_ids.clone());
        let dirty_len = dirty.dirty_len();
        analysis.ripple(dirty);
        eprintln!(
            "[mainexpand] re-indexed {} main files, rippled {} more ({:.2}s)",
            main_ids.len(),
            dirty_len,
            t.elapsed().as_secs_f64()
        );
        let after_index = collect_index(&analysis, "after_mainexpand");
        diff_index("cold", &cold_index, "after_mainexpand", &after_index);
        let after = collect(&analysis, "after_mainexpand");
        diff("cold", &cold, "after_mainexpand", &after);
    }

    if stages.iter().any(|s| s == "reindex") {
        analysis = build_analysis(&codebase, &annotations);
        let t = Instant::now();
        analysis.reindex();
        eprintln!("[reindex] full reindex {:.2}s", t.elapsed().as_secs_f64());
        let after = collect(&analysis, "after_full_reindex");
        diff("cold", &cold, "after_full_reindex", &after);
        if let Some(cold_index) = &cold_index {
            let after_index = collect_index(&analysis, "after_full_reindex");
            diff_index("cold", cold_index, "after_full_reindex", &after_index);
        }
    }

    if stages.iter().any(|s| s == "shiftsettle") {
        shift_settle(&codebase, &annotations, &targets);
    }
    if stages.iter().any(|s| s == "editrevert") {
        edit_revert(&codebase, &annotations, &targets);
    }
    if stages.iter().any(|s| s == "realedit") {
        let mut stage_analysis = build_analysis(&codebase, &annotations);
        real_edit(
            &mut stage_analysis,
            &codebase,
            &annotations,
            &targets,
            &cold,
        );
    }

    if stages.iter().any(|s| s == "burst") {
        let mut stage_analysis = build_analysis(&codebase, &annotations);
        burst_edit(
            &mut stage_analysis,
            &codebase,
            &annotations,
            &targets,
            &cold,
        );
    }

    let mut blast_ok = true;
    if stages.iter().any(|s| s == "blast") {
        let mut stage_analysis = build_analysis(&codebase, &annotations);
        blast_ok = blast_radius(&mut stage_analysis, &codebase, &targets);
    }

    // Bisect stage: re-walk the target's expansion in one batch with the text
    // untouched. Localises which sites flip when re-derived against a settled
    // retained neighborhood, without the edit signal `burst` carries.
    if stages.iter().any(|s| s == "batchexpand") {
        analysis = build_analysis(&codebase, &annotations);
        let cold_index = collect_index(&analysis, "cold");
        for target in &targets {
            let path = codebase.join(target.replace('/', std::path::MAIN_SEPARATOR_STR));
            let Some(uri) = glua_code_analysis::file_path_to_uri(&path) else {
                continue;
            };
            let Some(file_id) = analysis.get_file_id(&uri) else {
                continue;
            };
            let expansion = analysis.expand_reindex_file_ids(vec![file_id]);
            let t = Instant::now();
            let expansion_len = expansion.len();
            analysis.reindex_expanded_files(vec![file_id], expansion);
            eprintln!(
                "[batchexpand] {target}: re-walked {expansion_len} files in one batch ({:.2}s)",
                t.elapsed().as_secs_f64()
            );
        }
        let after_index = collect_index(&analysis, "after_batchexpand");
        diff_index("cold", &cold_index, "after_batchexpand", &after_index);
        let after = collect(&analysis, "after_batchexpand");
        diff("cold", &cold, "after_batchexpand", &after);
    }

    if stages.iter().any(|s| s == "delete" || s == "reopen") {
        delete_reopen(&codebase, &annotations, &targets);
    }

    if stages.iter().any(|s| s == "indexrepeat") {
        run_index_repeat(&codebase, &annotations, &targets);
    }

    if stages.iter().any(|s| s == "fresh") {
        let fresh_analysis = build_analysis(&codebase, &annotations);
        let fresh = collect(&fresh_analysis, "fresh_process");
        diff("cold", &cold, "fresh_process", &fresh);
        if let Some(cold_index) = &cold_index {
            let after_index = collect_index(&fresh_analysis, "fresh_process");
            diff_index("cold", cold_index, "fresh_process", &after_index);
        }
    }

    if stages.iter().any(|s| s == "order") {
        let reversed_analysis =
            build_analysis_with(&codebase, &annotations, Order::Reverse, 1, &[], None);
        let reversed = collect(&reversed_analysis, "reverse_order");
        diff("cold", &cold, "reverse_order", &reversed);
        if let Some(cold_index) = &cold_index {
            let after_index = collect_index(&reversed_analysis, "reverse_order");
            diff_index("cold", cold_index, "reverse_order", &after_index);
        }
    }

    if let Some(batches) = stages.iter().find_map(|s| s.strip_prefix("split:")) {
        let batches = batches.parse::<usize>().unwrap_or(2);
        let split_analysis =
            build_analysis_with(&codebase, &annotations, Order::Natural, batches, &[], None);
        let cold_index = collect_index(&analysis, "cold");
        let split_index = collect_index(&split_analysis, "split_batches");
        diff_index("cold", &cold_index, "split_batches", &split_index);
        let split = collect(&split_analysis, "split_batches");
        diff("cold", &cold, "split_batches", &split);
    }

    alloc_sample::report(
        std::env::var("DET_ALLOC_TOP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30),
    );

    // Stages that print a divergence and exit zero are reports, not gates.
    let divergences = DIVERGENCES.load(Ordering::Relaxed);
    if divergences > 0 {
        eprintln!(
            "[determinism] {divergences} comparison(s) were not identical; see the DIFF/INDEX lines above"
        );
    }
    if !blast_ok {
        eprintln!("[blast] gate failed");
    }
    if divergences > 0 || !blast_ok {
        std::process::exit(1);
    }
}
