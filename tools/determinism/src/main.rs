//! Determinism of settled language-server workspace updates.
//!
//! Every stage runs production update paths on a copy-on-write fork of one
//! cold base build and compares the settled index, and the diagnostics the
//! language server shows, with a cold build of the same final input (the
//! oracle, or answer key). The base is the oracle of every stage that ends on
//! the text it started from.
//!
//! Shown diagnostics: a warm run starts from the base's diagnostics and, after
//! each settled re-analysis, re-diagnoses the files it reported affected, as
//! the server does; a deleted file's are dropped. A file whose diagnostics
//! changed but were never re-diagnosed therefore differs from the oracle; the
//! `STALE` line names those files.
//!
//! Answer keys are built ahead of the stages by worker threads. `DET_JOBS`
//! (default 2) caps the analyses alive at once, the kept base included, which
//! bounds memory: about 2.7 GB each on CityRP. Each is kept as its snapshot
//! until its last use.
//!
//! Stages (`DET_STAGES`, comma list; default `schedule,edit,revert,noop`):
//! - `edit`: each target's edit, settled at once.
//! - `revert`: the edit, then the original text; the oracle is the base.
//! - `typing`: the edit typed one character at a time as the language server
//!   takes it, through every invalid text in between: each keystroke is
//!   taken into the re-analysis still settling from the one before, after 16
//!   or 256 of its evaluations, and every third keystroke and the last settle
//!   first, as after a pause. Slices alternate threads, and each first puts
//!   the next of the target's readers ahead, as a waiting diagnostics pull
//!   does. Shares the edit's oracle.
//! - `gate`: on one fork, the `typing` stage's keystrokes, the original text
//!   back, then the file deleted and re-created through watched-file events,
//!   each settled in slices, as the language-server drift gate drives the
//!   server; between slices the file is put ahead while it settles, then
//!   diagnosed once mid re-analysis, as its diagnostics task does (the revert, delete and re-create in slices of `DET_SLICE`,
//!   default 16); its oracle is the `recreate` stage's kind.
//! - `burst`: three edits of every target before one re-analysis, ending on
//!   the edited text.
//! - `noop`: re-index each target with unchanged text; the oracle is the base.
//!   Any evaluated file outside the targets is a locality divergence.
//! - `reload`: the settled analysis reloaded as the server reloads its
//!   configuration, to the configuration `reconfigure` makes (an analysis
//!   setting and a parse setting changed), compared with a cold build of it,
//!   then back (`reload+back`), compared with the base.
//! - `exact`: remove and re-add exactly the targets in one batch (a bisect
//!   aid).
//! - `schedule`: cold build taking files in reverse order within each rank,
//!   compared with the base; `schedule:SEED` takes them in an order fixed by
//!   the seed; `schedule:deferred` takes them in order but makes every reader
//!   an evaluation queues wait for the next round. The settled result must
//!   not depend on the order derivations run in.
//! - `split:N`: cold build in N batches, compared with the base.
//! - `rewrite`, `unrewrite`: sweeps of document edits an agent makes:
//!   three globals defined at the top of every swept file and a parameter
//!   added to each of its first three functions, settled at once; `unrewrite`
//!   then puts the original text back and settles again, its oracle the base.
//! - `touch`, `shift`, `delete`, `recreate`, `rename`: sweeps through
//!   watched-file events (delete, recreate, rename) or document edits (touch
//!   appends a comment, shift prepends a newline; any evaluated file outside
//!   the swept files is a locality divergence). The swept files are the
//!   `DET_HUBS` main files the most other files read from, with the files
//!   named in `DET_ONLY`, or else every `DET_STRIDE`-th (default 25) main file
//!   from `DET_STRIDE_OFFSET`. They form one batch with one oracle; with
//!   `DET_EACH=1` each file is its own run and oracle, labelled
//!   `stage[path]`, which a batch cannot mask by re-indexing a reader another
//!   file left stale. `DET_SLICE=N` settles each watched-file batch in slices
//!   of N evaluations, every other slice on a fresh thread, as the language
//!   server settles one; with `DET_FOCUS_AFTER=K` each boundary from the K-th
//!   on puts the batch's files ahead, as a diagnostics task waiting on them
//!   does.
//!
//! Targets: `DET_EDITS` names a file of `path<TAB>find<TAB>replace` lines
//! (`\n`, `\t` and `\` escaped), one edit per target; or `DET_TARGETS`
//! lists paths relative to `DET_CODEBASE`, each edited `DET_EDIT_FIND` ->
//! `DET_EDIT_REPLACE`.
//!
//! Every warm stage prints its reach (files evaluated and reported affected)
//! and a `[waste]` line counting the files whose diagnostics or stored types
//! changed (needed) against those it evaluated and reported affected.
//!
//! `DET_REPLAY=1` also replays the warm run's `update_index` batch log onto a
//! fork of the oracle and compares again: drift that disappears depends on
//! batch composition, drift that survives is an incremental-path bug.
//!
//! Environment: `DET_CODEBASE`, `DET_ANNOTATIONS` (required);
//! `DET_DUMP_INDEX_DETAIL` (index names or `all`, keeps entry text for
//! per-entry diffs); `DET_DUMP_INDEX` (directory for full snapshots);
//! `DET_DUMP` (base diagnostics file); `DET_FILTER`, `DET_LIMIT`. Any
//! divergence exits 1. The run prints its peak memory.

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use glua_code_analysis::{
    EmmyLuaAnalysis, Emmyrc, EmmyrcNonStdSymbol, FileId, Schedule, Slice, UnpickedGamemodes,
    UpdateProgress, WorkspaceFolder, collect_workspace_files, load_configs, scope_workspace_folder,
};
use glua_parser::{LineIndex, LuaParser};
use lsp_types::{DiagnosticSeverity, NumberOrString};
use rayon::prelude::*;
use rowan::NodeCache;
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

/// Comparison key for a workspace file path. `DET_TARGETS` is written by hand,
/// so it has to match the loader's paths regardless of separator or case.
fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

fn file_uri(path: &Path) -> lsp_types::Uri {
    glua_code_analysis::file_path_to_uri(&path.to_path_buf())
        .unwrap_or_else(|| panic!("cannot build URI for {}", path.display()))
}

/// The final input of a cold build: the collected workspace with `overrides`
/// replacing file text and `removed` never present. `created` files get new
/// ids after every collected file's id is reserved, as the watcher path
/// assigns them.
#[derive(Clone, Default, Eq, Hash, PartialEq)]
struct ColdInput {
    overrides: Vec<(PathBuf, String)>,
    removed: Vec<PathBuf>,
    created: Vec<(PathBuf, String)>,
    /// Files moved from the first path to the second with their text, each
    /// keeping its id as the server's move does.
    moved: Vec<(PathBuf, PathBuf, String)>,
    /// Whether the configuration is the one [`reconfigure`] makes.
    reconfigured: bool,
}

/// The codebase's configuration, as the server loads it.
fn workspace_config(codebase: &Path) -> Emmyrc {
    let config_files = discover_config_files(codebase);
    let mut emmyrc = if config_files.is_empty() {
        Emmyrc::default()
    } else {
        load_configs(config_files, None)
    };
    emmyrc.gmod.enabled = true;
    emmyrc.pre_process_emmyrc(codebase);
    emmyrc
}

/// The configuration the `reload` stage moves to: one setting that changes
/// what the index derives and one that changes how every file parses.
fn reconfigure(emmyrc: &mut Emmyrc) {
    emmyrc.strict.array_index = !emmyrc.strict.array_index;
    emmyrc
        .runtime
        .nonstandard_symbol
        .retain(|symbol| *symbol != EmmyrcNonStdSymbol::Continue);
}

/// Adds the annotations, the codebase and the configured libraries as
/// workspaces, and returns every file to index.
fn add_workspaces(
    analysis: &mut EmmyLuaAnalysis,
    codebase: &Path,
    annotations: &Path,
    emmyrc: &Emmyrc,
) -> Vec<(PathBuf, Option<String>)> {
    analysis.add_library_workspace(annotations.to_path_buf());
    let main_folders = scope_workspace_folder(
        &WorkspaceFolder::new(codebase.to_path_buf(), false),
        &emmyrc.gmod,
        UnpickedGamemodes::AllCustom,
        &[],
    );
    for folder in &main_folders {
        analysis.add_main_workspace(folder.root.clone());
    }

    let mut workspace_folders = vec![WorkspaceFolder::new(annotations.to_path_buf(), true)];
    workspace_folders.extend(main_folders);
    for lib in &emmyrc.workspace.library {
        let path = PathBuf::from(lib.get_path().clone());
        if path != annotations {
            analysis.add_library_workspace(path.clone());
            workspace_folders.push(WorkspaceFolder::new(path, true));
        }
    }

    collect_workspace_files(&workspace_folders, &analysis.emmyrc, None, None)
        .into_iter()
        .filter(|file| !file.path.ends_with(".editorconfig"))
        .map(|file| file.into_tuple())
        .collect()
}

fn build_cold(
    codebase: &Path,
    annotations: &Path,
    input: &ColdInput,
    batches: usize,
    schedule: Schedule,
) -> EmmyLuaAnalysis {
    let mut emmyrc = workspace_config(codebase);
    if input.reconfigured {
        reconfigure(&mut emmyrc);
    }
    let mut analysis = EmmyLuaAnalysis::new();
    analysis.compilation.set_schedule(schedule);
    analysis.update_config(Arc::new(emmyrc.clone()));
    analysis.init_std_lib();
    let mut files = add_workspaces(&mut analysis, codebase, annotations, &emmyrc);
    for (path, content) in &input.overrides {
        let want = path_key(path);
        let mut matched = 0;
        for file in files.iter_mut().filter(|file| path_key(&file.0) == want) {
            file.1 = Some(content.clone());
            matched += 1;
        }
        // A silently unapplied override would make this build a copy of the
        // baseline, and every diff taken against it would read as "identical".
        assert_eq!(matched, 1, "override did not match one workspace file");
    }
    if !input.removed.is_empty() || !input.created.is_empty() || !input.moved.is_empty() {
        let vfs = analysis.compilation.get_db().get_vfs();
        files.sort_by_cached_key(|(path, _)| vfs.order_key_for_path(path));
        let vfs = analysis.compilation.get_db_mut().get_vfs_mut();
        for (path, _) in &files {
            vfs.set_file_content(&file_uri(path), None);
        }
        let removed = input
            .removed
            .iter()
            .map(|path| path_key(path))
            .collect::<BTreeSet<_>>();
        let before = files.len();
        files.retain(|(path, _)| !removed.contains(&path_key(path)));
        assert_eq!(
            before - files.len(),
            removed.len(),
            "removed path did not match one workspace file"
        );
        for path in &input.removed {
            vfs.remove_file(&file_uri(path));
        }
        for (from, to, text) in &input.moved {
            vfs.move_file(&file_uri(from), &file_uri(to), text.clone())
                .expect("a moved path names one workspace file");
            let file = files
                .iter_mut()
                .find(|(path, _)| path_key(path) == path_key(from))
                .expect("a moved path names one workspace file");
            *file = (to.clone(), Some(text.clone()));
        }
        let mut created = input.created.clone();
        created.sort_by_cached_key(|(path, _)| vfs.order_key_for_path(path));
        for (path, text) in created {
            vfs.set_file_content(&file_uri(&path), None);
            files.push((path, Some(text)));
        }
    }
    let t = Instant::now();
    for group in files.chunks(files.len().div_ceil(batches)) {
        analysis.update_files_by_path(group.to_vec());
    }
    eprintln!(
        "[setup] cold build indexed in {:.2}s",
        t.elapsed().as_secs_f64()
    );
    analysis
}

/// The diagnostics of the main workspace, sorted, each one as often as the
/// server would show it.
fn collect(analysis: &EmmyLuaAnalysis, label: &str) -> Vec<Snapshot> {
    let t = Instant::now();
    let mut out = diagnose(
        analysis,
        analysis.get_main_workspace_file_ids_for_diagnostics(),
    );
    out.sort_unstable();
    let (errors, warnings) = counts(&out);
    eprintln!(
        "[{label}] total={} errors={errors} warnings={warnings} ({:.2}s)",
        out.len(),
        t.elapsed().as_secs_f64()
    );
    out
}

/// The diagnostics of `file_ids`, as the language server reports them.
fn diagnose(analysis: &EmmyLuaAnalysis, file_ids: Vec<FileId>) -> Vec<Snapshot> {
    let shared = analysis.precompute_diagnostic_shared_data();
    file_ids
        .into_par_iter()
        .flat_map_iter(|file_id| {
            let diagnostics = analysis
                .diagnose_file_with_shared(file_id, CancellationToken::new(), shared.clone())
                .unwrap_or_default();
            let file = file_label(analysis, file_id);
            diagnostics.into_iter().map(move |diagnostic| Snapshot {
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
            })
        })
        .collect()
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
    /// Observed call-site param evidence contributed by calls in each file.
    call_site_contributions: HashDumpResult,
    /// Each file's stored helper scan, load scan, net helpers and function
    /// summaries, by the fingerprint they were stored with.
    stored_facts: HashDumpResult,
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
        let want_detail = std::env::var("DET_DUMP_INDEX_DETAIL")
            .is_ok_and(|want| want.split(',').any(|want| want == name || want == "all"));
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

static DIVERGENCES: AtomicUsize = AtomicUsize::new(0);

fn record_divergence() {
    DIVERGENCES.fetch_add(1, Ordering::Relaxed);
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
        println!("INDEX {base_label} -> {label}: {name} dropped={dropped} gained={gained}");
    }
}

fn collect_index(analysis: &EmmyLuaAnalysis, label: &str) -> IndexSnapshot {
    let t = Instant::now();
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
    // Compared by content: the trees hold hash sets whose printed order
    // differs between two builds of the same text.
    for (file_id, tree) in flow.flow_trees() {
        flow_trees.insert(
            file_label(analysis, *file_id),
            format!("{:016x}", tree.content_fingerprint()),
        );
    }
    let mut signature_casts = BTreeMap::new();
    for (signature_id, signature) in db.get_signature_index().iter() {
        if let Some(cast) = &signature.return_cast {
            signature_casts.insert(
                format!(
                    "{}|{signature_id:?}",
                    file_label(analysis, signature_id.get_file_id())
                ),
                format!("{cast:?}"),
            );
        }
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
    for file_id in db.get_vfs().get_all_file_ids() {
        if let Some(sites) = db
            .get_file_dependencies_index()
            .get_dependency_sites(&file_id)
        {
            dependency_sites.insert(file_label(analysis, file_id), format!("{sites:?}"));
        }
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

    // The call-site param index's persisted relation: each file's evidence.
    // The folded params are dumped with the inferred params above.
    let call_site = db.get_call_site_param_index();
    let mut call_site_contributions = HashDump::new("call_site_contributions");
    for (file_id, contributions) in call_site.iter_file_contributions() {
        call_site_contributions
            .insert(file_label(analysis, *file_id), format!("{contributions:?}"));
    }

    let mut stored_facts = HashDump::new("stored_facts");
    for (kind, file_id, fingerprint) in db.stored_fact_fingerprints() {
        stored_facts.insert(
            format!("{kind} {}", file_label(analysis, file_id)),
            fingerprint.to_string(),
        );
    }

    let mut schema_files = HashDump::new("schema_files");
    for (url, schema_file) in db.get_json_schema_index().iter_schema_files() {
        schema_files.insert(url.as_str().to_string(), format!("{schema_file:?}"));
    }

    eprintln!(
        "[{label}] index type_caches={} members={} net_flows={} flow_trees={} signature_casts={} special_call_effects={} special_call_facts={} accessor_annotations={} accessor_annotation_files={} numeric_range_files={} numeric_range_globals={} type_decl_states={} type_decl_generic_params={} ({:.2}s)",
        type_caches.len(),
        members.len(),
        net_flows.len(),
        flow_trees.len(),
        signature_casts.len(),
        special_call_effects.len(),
        special_call_facts.len(),
        accessor_func_annotations.hashes.len(),
        accessor_func_annotation_files.hashes.len(),
        numeric_range_file_populations.hashes.len(),
        numeric_range_global_populations.hashes.len(),
        type_decl_states.hashes.len(),
        type_decl_generic_params.hashes.len(),
        t.elapsed().as_secs_f64()
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
        call_site_contributions: call_site_contributions.finish(),
        stored_facts: stored_facts.finish(),
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
            ("CSCONTRIB", &snapshot.call_site_contributions),
            ("STORED", &snapshot.stored_facts),
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
        println!(
            "INDEX {base_label} -> {label}: {name} dropped={} gained={} changed={}",
            dropped.len(),
            gained.len(),
            changed.len()
        );
    }
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
            "call_site_contributions",
            "CSCONTRIB",
            &base.call_site_contributions,
            &other.call_site_contributions,
        ),
        (
            "stored_facts",
            "STORED",
            &base.stored_facts,
            &other.stored_facts,
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
        println!(
            "INDEX {base_label} -> {label}: members dropped={} gained={}",
            member_dropped.len(),
            member_gained.len()
        );
    }
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
        println!(
            "INDEX {base_label} -> {label}: net_flows dropped={} gained={}",
            net_dropped.len(),
            net_gained.len()
        );
    }
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

fn counts(set: &[Snapshot]) -> (usize, usize) {
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

/// The entries of sorted `of` left once each entry of sorted `without`
/// cancels one equal entry, so a repeated diagnostic counts every time.
fn sorted_difference<'a>(of: &'a [Snapshot], without: &[Snapshot]) -> Vec<&'a Snapshot> {
    let mut without = without.iter().peekable();
    of.iter()
        .filter(|entry| {
            while without.next_if(|other| other < entry).is_some() {}
            without.next_if(|other| other == entry).is_none()
        })
        .collect()
}

fn diff(base_label: &str, base: &[Snapshot], label: &str, other: &[Snapshot]) {
    let removed = sorted_difference(base, other);
    let added = sorted_difference(other, base);
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

fn target(
    analysis: &EmmyLuaAnalysis,
    codebase: &Path,
    relative: &str,
) -> (PathBuf, lsp_types::Uri, FileId, String) {
    let path = codebase.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    let uri = file_uri(&path);
    let file_id = analysis
        .get_file_id(&uri)
        .unwrap_or_else(|| panic!("target is not indexed: {}", path.display()));
    let text = analysis
        .compilation
        .get_db()
        .get_vfs()
        .get_file_content(&file_id)
        .unwrap_or_else(|| panic!("target has no indexed text: {}", path.display()))
        .clone();
    (path, uri, file_id, text)
}

/// The texts typing `find` into `replace` passes through: the part of `find`
/// that differs deleted at once, then the part of `replace` that differs typed
/// one character at a time. The last is [`edited`]'s text.
fn typed_texts(text: &str, find: &str, replace: &str) -> Vec<String> {
    let prefix: usize = find
        .chars()
        .zip(replace.chars())
        .take_while(|(from, to)| from == to)
        .map(|(from, _)| from.len_utf8())
        .sum();
    let suffix: usize = find[prefix..]
        .chars()
        .rev()
        .zip(replace[prefix..].chars().rev())
        .take_while(|(from, to)| from == to)
        .map(|(from, _)| from.len_utf8())
        .sum();
    let head = &replace[..prefix];
    let tail = &replace[replace.len() - suffix..];
    let typed = &replace[prefix..replace.len() - suffix];
    let mut texts = Vec::new();
    if find.len() - suffix > prefix || typed.is_empty() {
        texts.push(edited(text, find, &format!("{head}{tail}")));
    }
    let ends = typed
        .char_indices()
        .map(|(at, _)| at)
        .skip(1)
        .chain(std::iter::once(typed.len()));
    for end in ends.filter(|end| *end > 0) {
        texts.push(edited(
            text,
            find,
            &format!("{head}{}{tail}", &typed[..end]),
        ));
    }
    texts
}

fn edited(text: &str, find: &str, replace: &str) -> String {
    assert!(
        text.contains(find),
        "DET_EDIT_FIND does not occur in target"
    );
    let result = text.replace(find, replace);
    assert_ne!(result, text, "edit must change the target text");
    result
}

fn apply_text(
    analysis: &mut EmmyLuaAnalysis,
    uri: &lsp_types::Uri,
    file_id: FileId,
    text: String,
    version: i32,
) {
    let mut cache = NodeCache::default();
    let config = analysis.get_emmyrc();
    let line_index = LineIndex::parse(&text);
    let tree = LuaParser::parse(&text, config.get_parse_config(&mut cache));
    let (updated, old) = analysis
        .update_file_preparsed_deferred(uri.clone(), Some(text), tree, line_index, Some(version))
        .expect("document update must apply");
    assert_eq!(updated, file_id);
    drop(old);
}

/// The files a warm run's re-analyses evaluated and the files whose
/// diagnostics they reported affected.
#[derive(Default)]
struct Reach {
    evaluated: Vec<FileId>,
    affected: Vec<FileId>,
}

impl Reach {
    fn extend(&mut self, other: Reach) {
        self.evaluated.extend(other.evaluated);
        self.affected.extend(other.affected);
    }

    fn of(analysis: &EmmyLuaAnalysis, affected: Vec<FileId>) -> Self {
        Self {
            evaluated: analysis.compilation.last_evaluated_files().to_vec(),
            affected,
        }
    }
}

/// A fork of the base updated as the language server updates it, with the
/// diagnostics the server shows: the base's, a file's replaced each time a
/// settled re-analysis reports it affected, and a deleted file's dropped.
struct Warm {
    analysis: EmmyLuaAnalysis,
    shown: BTreeMap<String, Vec<Snapshot>>,
}

impl std::ops::Deref for Warm {
    type Target = EmmyLuaAnalysis;

    fn deref(&self) -> &EmmyLuaAnalysis {
        &self.analysis
    }
}

impl std::ops::DerefMut for Warm {
    fn deref_mut(&mut self) -> &mut EmmyLuaAnalysis {
        &mut self.analysis
    }
}

impl Warm {
    /// Takes in a settled re-analysis that reported `affected` as the server
    /// does, and returns its reach.
    fn settled(&mut self, affected: Vec<FileId>) -> Reach {
        let live = self
            .analysis
            .get_main_workspace_file_ids_for_diagnostics()
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        let live_files = live
            .iter()
            .map(|file_id| file_label(&self.analysis, *file_id))
            .collect::<BTreeSet<_>>();
        self.shown.retain(|file, _| live_files.contains(file));
        let rediagnosed = affected
            .iter()
            .filter(|file_id| live.contains(file_id))
            .copied()
            .collect::<BTreeSet<_>>();
        for file_id in &rediagnosed {
            self.shown.remove(&file_label(&self.analysis, *file_id));
        }
        for diagnostic in diagnose(&self.analysis, rediagnosed.into_iter().collect()) {
            self.shown
                .entry(diagnostic.file.clone())
                .or_default()
                .push(diagnostic);
        }
        Reach::of(&self.analysis, affected)
    }

    /// The diagnostics the server shows now.
    fn shown(&self) -> Vec<Snapshot> {
        let mut shown = self.shown.values().flatten().cloned().collect::<Vec<_>>();
        shown.sort_unstable();
        shown
    }
}

/// The cold input the `reload` stage compares with before reloading back.
fn reload_input() -> ColdInput {
    ColdInput {
        reconfigured: true,
        ..ColdInput::default()
    }
}

/// Reloads `warm` with `emmyrc` as the server reloads its configuration: the
/// configuration applied, the workspaces added again and the collected files
/// submitted, in one rebuild when the change reaches the index; then every
/// file diagnosed again.
fn reload_with(warm: &mut Warm, codebase: &Path, annotations: &Path, emmyrc: Emmyrc) -> Reach {
    let rebuild = warm.apply_config(Arc::new(emmyrc.clone())).affects_index();
    let changes = add_workspaces(warm, codebase, annotations, &emmyrc)
        .into_iter()
        .map(|(path, text)| (file_uri(&path), text))
        .collect::<Vec<_>>();
    if rebuild {
        warm.stage_file_system_changes(changes);
        warm.begin_full_reindex();
        warm.advance_reindex(Slice::Whole);
    } else {
        warm.update_files_by_uri(changes);
    }
    let every_file = warm.get_main_workspace_file_ids_for_diagnostics();
    warm.settled(every_file)
}

fn settle(
    warm: &mut Warm,
    uri: &lsp_types::Uri,
    file_id: FileId,
    text: String,
    version: i32,
) -> Reach {
    apply_text(warm, uri, file_id, text, version);
    let affected = warm.reindex_files(vec![file_id]);
    warm.settled(affected)
}

/// Settles the re-analysis in progress in slices of `evaluations`, every
/// other slice on a fresh thread, calling `between` after each slice that
/// left work. Returns the files it reported affected.
fn settle_in_slices(
    analysis: &mut EmmyLuaAnalysis,
    evaluations: usize,
    mut between: impl FnMut(&mut EmmyLuaAnalysis),
) -> Vec<FileId> {
    for slice in 0.. {
        let progress = if slice % 2 == 0 {
            analysis.advance_reindex(Slice::Evaluations(evaluations))
        } else {
            std::thread::scope(|scope| {
                std::thread::Builder::new()
                    .stack_size(glua_code_analysis::ANALYSIS_STACK_SIZE)
                    .spawn_scoped(scope, || {
                        analysis.advance_reindex(Slice::Evaluations(evaluations))
                    })
                    .expect("slice thread should spawn")
                    .join()
                    .expect("slice thread should not panic")
            })
        };
        match progress {
            UpdateProgress::Settled(affected) => return affected,
            UpdateProgress::Settling => between(analysis),
            UpdateProgress::TextChanged => panic!("a slice found text it had not taken"),
        }
    }
    unreachable!("slices go on until the re-analysis settles")
}

/// Evaluations per slice when a stage settles a re-analysis in slices.
const SLICE_EVALUATIONS: usize = 16;

/// Between two slices, what a diagnostics task waiting on `file_ids` does:
/// puts them ahead while they settle, then diagnoses the live ones once,
/// recording their reads mid re-analysis.
fn pull_when_settled(analysis: &mut EmmyLuaAnalysis, file_ids: &[FileId], pulled: &mut bool) {
    if *pulled {
        return;
    }
    if file_ids
        .iter()
        .any(|file_id| analysis.compilation.is_settling(*file_id))
    {
        analysis.compilation.prioritise_update(file_ids);
        return;
    }
    *pulled = true;
    let live = file_ids
        .iter()
        .copied()
        .filter(|file_id| analysis.get_uri(*file_id).is_some())
        .collect::<Vec<_>>();
    diagnose(analysis, live);
}

/// What typing an edit into a warm fork reached.
struct Typed {
    reach: Reach,
    most_evaluated: usize,
    most_affected: usize,
    taken_mid: usize,
    /// The document version the last keystroke took.
    version: i32,
}

/// Types `edit` into `file_id` one character at a time as the language server
/// takes it, every keystroke into the re-analysis still settling from the one
/// before; see the module docs.
fn type_edit(
    warm: &mut Warm,
    uri: &lsp_types::Uri,
    file_id: FileId,
    original: &str,
    edit: &Edit,
    readers: &[FileId],
) -> Typed {
    let mut next_reader = readers.iter().cycle();
    let mut typed = Typed {
        reach: Reach::default(),
        most_evaluated: 0,
        most_affected: 0,
        taken_mid: 0,
        version: 0,
    };
    let texts = typed_texts(original, &edit.find, &edit.replace);
    let keystrokes = texts.len();
    for (version, text) in (1..).zip(texts) {
        typed.version = version;
        apply_text(warm, uri, file_id, text, version);
        warm.begin_reindex(vec![file_id]);
        let pause = version % 3 == 0 || version as usize == keystrokes;
        let affected = if pause {
            Some(settle_in_slices(warm, SLICE_EVALUATIONS, |warm| {
                if let Some(reader) = next_reader.next() {
                    warm.compilation.prioritise_update(&[*reader]);
                }
            }))
        } else {
            let evaluations = if version % 3 == 1 { 16 } else { 256 };
            match warm.advance_reindex(Slice::Evaluations(evaluations)) {
                UpdateProgress::Settled(affected) => Some(affected),
                UpdateProgress::Settling => {
                    typed.taken_mid += 1;
                    None
                }
                UpdateProgress::TextChanged => panic!("a slice found text it had not taken"),
            }
        };
        if let Some(mut affected) = affected {
            affected.push(file_id);
            let step = warm.settled(affected);
            typed.most_evaluated = typed.most_evaluated.max(step.evaluated.len());
            typed.most_affected = typed.most_affected.max(step.affected.len());
            typed.reach.extend(step);
        }
    }
    typed
}

/// Prints how far a warm run reached. With `strict`, any evaluated file
/// outside `edited` is a locality divergence.
fn report_reach(
    analysis: &EmmyLuaAnalysis,
    label: &str,
    edited: &[FileId],
    reach: &Reach,
    strict: bool,
) {
    let evaluated = reach.evaluated.iter().collect::<BTreeSet<_>>();
    let affected = reach.affected.iter().collect::<BTreeSet<_>>();
    let beyond = evaluated
        .iter()
        .filter(|file_id| !edited.contains(file_id))
        .collect::<BTreeSet<_>>();
    println!(
        "[reach {label}] evaluated={} affected={} beyond_edited={}",
        evaluated.len(),
        affected.len(),
        beyond.len()
    );
    if strict && !beyond.is_empty() {
        record_divergence();
        println!(
            "LOCALITY {label}: {} file(s) beyond the edited set",
            beyond.len()
        );
        for file_id in beyond.iter().take(diff_limit()) {
            println!("  BEYOND {}", file_label(analysis, ***file_id));
        }
    }
}

/// A warm run's reach by file label: the files it edited, evaluated and
/// reported affected.
struct Reached {
    edited: BTreeSet<String>,
    evaluated: BTreeSet<String>,
    affected: BTreeSet<String>,
}

impl Reached {
    /// Labels each file by its path in the base and in the warm run: a file
    /// the run moved keeps its id and goes by both.
    fn of(
        base: &EmmyLuaAnalysis,
        warm: &EmmyLuaAnalysis,
        edited: &[FileId],
        reach: &Reach,
    ) -> Self {
        let labels = |file_ids: &[FileId]| {
            file_ids
                .iter()
                .flat_map(|file_id| {
                    let paths = [base, warm]
                        .into_iter()
                        .filter_map(|analysis| {
                            analysis
                                .compilation
                                .get_db()
                                .get_vfs()
                                .get_file_path(file_id)
                                .map(|path| path.display().to_string().replace('\\', "/"))
                        })
                        .collect::<Vec<_>>();
                    if paths.is_empty() {
                        vec![format!("{file_id:?}")]
                    } else {
                        paths
                    }
                })
                .collect()
        };
        Self {
            edited: labels(edited),
            evaluated: labels(&reach.evaluated),
            affected: labels(&reach.affected),
        }
    }
}

/// Checks a warm run's reach against what its edit changed between the base
/// and the oracle, and names the files the language server would leave
/// stale: their diagnostics changed, but the run neither edited them nor
/// reported them affected. The diagnostics comparison already counts those
/// files as differing; this says why. Also reports the files evaluated or
/// reported affected with nothing of theirs changing, and the files whose
/// facts changed without being evaluated.
fn check_reach(label: &str, base: &Snap, expected: &Snap, reached: &Reached) {
    let diagnostics_changed =
        files_with_changed_diagnostics(&base.diagnostics, &expected.diagnostics);
    let stale = diagnostics_changed
        .iter()
        .filter(|file| !reached.edited.contains(*file) && !reached.affected.contains(*file))
        .collect::<Vec<_>>();
    let beyond_edited = |files: &BTreeSet<String>| {
        files
            .iter()
            .filter(|file| !reached.edited.contains(*file))
            .cloned()
            .collect::<BTreeSet<_>>()
    };
    let mut needed = files_with_changed_types(&base.index, &expected.index);
    needed.extend(diagnostics_changed.iter().cloned());
    let needed = beyond_edited(&needed);
    let evaluated = beyond_edited(&reached.evaluated);
    let diagnostics_changed = beyond_edited(&diagnostics_changed);
    let affected = beyond_edited(&reached.affected);
    println!(
        "[waste {label}] needed={} evaluated={} wasted={} missed={}; diagnostics changed={} affected={} wasted={}",
        needed.len(),
        evaluated.len(),
        evaluated.difference(&needed).count(),
        needed.difference(&evaluated).count(),
        diagnostics_changed.len(),
        affected.len(),
        affected.difference(&diagnostics_changed).count(),
    );
    for file in needed.difference(&evaluated).take(diff_limit()) {
        println!("  missed {file}");
    }
    if !stale.is_empty() {
        println!(
            "STALE {label}: {} file(s) whose diagnostics changed were not reported affected",
            stale.len()
        );
        for file in stale.iter().take(diff_limit()) {
            println!("  STALE {file}");
        }
    }
}

/// The files whose diagnostics differ between `before` and `after`.
fn files_with_changed_diagnostics(before: &[Snapshot], after: &[Snapshot]) -> BTreeSet<String> {
    let by_file = |diagnostics: &[Snapshot]| {
        let mut by_file = BTreeMap::<String, Vec<Snapshot>>::new();
        for diagnostic in diagnostics {
            by_file
                .entry(diagnostic.file.clone())
                .or_default()
                .push(diagnostic.clone());
        }
        by_file
    };
    let (before, after) = (by_file(before), by_file(after));
    before
        .keys()
        .chain(after.keys())
        .filter(|file| before.get(*file) != after.get(*file))
        .cloned()
        .collect()
}

/// The files whose stored types differ between `before` and `after`, from
/// the type caches and type facts keyed `file|owner`. A type fact's trailing
/// `/wN` counts the writers sharing the member's slot across the workspace,
/// which is no fact of the file itself.
fn files_with_changed_types(before: &IndexSnapshot, after: &IndexSnapshot) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    for (before, after, counts_writers) in [
        (&before.type_caches, &after.type_caches, false),
        (&before.type_facts, &after.type_facts, true),
    ] {
        let own = |value: &String| {
            if counts_writers {
                value
                    .rsplit_once("/w")
                    .map_or(value.as_str(), |(fact, _)| fact)
            } else {
                value.as_str()
            }
            .to_string()
        };
        for key in before.keys().chain(after.keys()) {
            if before.get(key).map(own) != after.get(key).map(own)
                && let Some((file, _)) = key.split_once('|')
            {
                files.insert(file.to_string());
            }
        }
    }
    files
}

fn replay(analysis: &mut EmmyLuaAnalysis, log: &[Vec<FileId>]) {
    for batch in log {
        analysis.compilation.remove_index(batch.clone());
        analysis.compilation.update_index(batch.clone());
    }
}

/// Index and diagnostics of one analysis.
struct Snap {
    index: IndexSnapshot,
    diagnostics: Vec<Snapshot>,
}

fn snapshot(analysis: &EmmyLuaAnalysis, label: &str) -> Snap {
    let (index, diagnostics) = rayon::join(
        || collect_index(analysis, label),
        || collect(analysis, label),
    );
    Snap { index, diagnostics }
}

fn compare(expected_label: &str, expected: &Snap, label: &str, actual: &Snap) {
    let before = DIVERGENCES.load(Ordering::Relaxed);
    diff_index(expected_label, &expected.index, label, &actual.index);
    diff(
        expected_label,
        &expected.diagnostics,
        label,
        &actual.diagnostics,
    );
    if DIVERGENCES.load(Ordering::Relaxed) == before {
        println!("[{label}] +0 index, +0 diagnostics");
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// One `apply_file_system_changes` batch: each URI with its new text, or
/// `None` for a deletion.
type WatchEvents = Vec<(lsp_types::Uri, Option<String>)>;

/// A sweep file: its path, URI, id and base text.
type SweepFile = (PathBuf, lsp_types::Uri, FileId, String);

/// A target file and the edit the edit stages make to it.
struct Target {
    relative: String,
    edit: Option<Edit>,
}

/// Replaces `find`, which must occur, with `replace`.
struct Edit {
    find: String,
    replace: String,
}

impl Target {
    fn edit(&self) -> &Edit {
        self.edit
            .as_ref()
            .unwrap_or_else(|| panic!("no edit is given for {}", self.relative))
    }
}

/// The targets and their edits: each line of the `DET_EDITS` file is
/// `path<TAB>find<TAB>replace`, with `\n`, `\t` and `\\` escaped; or the
/// `DET_TARGETS` files, each with the `DET_EDIT_FIND` -> `DET_EDIT_REPLACE`
/// edit when both are set.
fn targets_from_env() -> Vec<Target> {
    let unescape = |text: &str| {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            match (c, c == '\\') {
                (_, true) => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(other) => out.push(other),
                    None => out.push('\\'),
                },
                (c, false) => out.push(c),
            }
        }
        out
    };
    if let Ok(path) = std::env::var("DET_EDITS") {
        let text = std::fs::read_to_string(&path).expect("DET_EDITS should be readable");
        return text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let mut fields = line.split('\t');
                let (Some(relative), Some(find), Some(replace)) =
                    (fields.next(), fields.next(), fields.next())
                else {
                    panic!("DET_EDITS line is not path<TAB>find<TAB>replace: {line}");
                };
                Target {
                    relative: relative.trim().to_string(),
                    edit: Some(Edit {
                        find: unescape(find),
                        replace: unescape(replace),
                    }),
                }
            })
            .collect();
    }
    let edit = match (
        std::env::var("DET_EDIT_FIND"),
        std::env::var("DET_EDIT_REPLACE"),
    ) {
        (Ok(find), Ok(replace)) => Some((find, replace)),
        _ => None,
    };
    std::env::var("DET_TARGETS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|relative| !relative.is_empty())
        .map(|relative| Target {
            relative: relative.to_string(),
            edit: edit.as_ref().map(|(find, replace)| Edit {
                find: find.clone(),
                replace: replace.clone(),
            }),
        })
        .collect()
}

/// The order a cold build takes files in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Order {
    Canonical,
    Reversed,
    Shuffled(u64),
    Deferred,
}

impl Order {
    fn schedule(self) -> Schedule {
        match self {
            Self::Canonical => Schedule::Canonical,
            Self::Reversed => Schedule::Reversed,
            Self::Shuffled(seed) => Schedule::Shuffled(seed),
            Self::Deferred => Schedule::Deferred,
        }
    }
}

/// A cold build to compare with: its final input, how many batches it is
/// fed in, and the order it takes files in.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ColdKey {
    input: ColdInput,
    batches: usize,
    order: Order,
}

impl ColdKey {
    /// The cold build of `input`, as the oracle of a warm run.
    fn oracle(input: ColdInput) -> Self {
        Self {
            input,
            batches: 1,
            order: Order::Canonical,
        }
    }

    fn label(&self) -> String {
        match (self.order, self.batches) {
            (Order::Canonical, 1) => "oracle".into(),
            (Order::Canonical, batches) => format!("split:{batches}"),
            (Order::Reversed, _) => "schedule".into(),
            (Order::Shuffled(seed), _) => format!("schedule:{seed}"),
            (Order::Deferred, _) => "schedule:deferred".into(),
        }
    }
}

/// A built answer key, or why its build failed.
type Built = Result<Arc<Snap>, String>;

/// Cold builds to compare with, made by worker threads ahead of the stages
/// that need them. At most `limit` analyses are alive at once, the kept base
/// included, which bounds memory. Each build is kept as its snapshot until
/// its last planned use.
struct AnswerKeys {
    state: std::sync::Mutex<Keys>,
    changed: std::sync::Condvar,
    limit: usize,
}

#[derive(Default)]
struct Keys {
    queued: std::collections::VecDeque<ColdKey>,
    building: std::collections::HashSet<ColdKey>,
    /// Analyses alive: builds in progress and the kept base.
    builds: usize,
    built: HashMap<ColdKey, Built>,
    /// Planned uses still to come.
    uses: HashMap<ColdKey, usize>,
    closed: bool,
}

impl AnswerKeys {
    fn new(limit: usize) -> Self {
        Self {
            state: std::sync::Mutex::default(),
            changed: std::sync::Condvar::new(),
            limit,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Keys> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn wait<'a>(&self, state: std::sync::MutexGuard<'a, Keys>) -> std::sync::MutexGuard<'a, Keys> {
        self.changed
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Queues `keys` in the order the stages use them, counting each use.
    fn plan(&self, keys: impl IntoIterator<Item = ColdKey>) {
        let mut state = self.lock();
        for key in keys {
            let uses = state.uses.entry(key.clone()).or_default();
            *uses += 1;
            if *uses == 1 {
                state.queued.push_back(key);
            }
        }
        self.changed.notify_all();
    }

    /// Runs `build`, a cold build made outside the workers, counted against
    /// the limit while it runs.
    fn building<T>(&self, build: impl FnOnce() -> T) -> T {
        let built = self.keeping(build);
        self.lock().builds -= 1;
        self.changed.notify_all();
        built
    }

    /// Runs `build` for an analysis kept alive from then on, the base,
    /// counted against the limit for as long as the workers run.
    fn keeping<T>(&self, build: impl FnOnce() -> T) -> T {
        let mut state = self.lock();
        while state.builds >= self.limit {
            state = self.wait(state);
        }
        state.builds += 1;
        drop(state);
        build()
    }

    /// Builds queued keys until the plan is closed and none is left.
    fn work(&self, build: &(dyn Fn(&ColdKey) -> Snap + Sync)) {
        let mut state = self.lock();
        loop {
            if state.builds < self.limit
                && let Some(key) = state.queued.pop_front()
            {
                state.builds += 1;
                state.building.insert(key.clone());
                drop(state);
                let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build(&key)))
                    .map(Arc::new)
                    .map_err(|panic| {
                        panic
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
                            .unwrap_or_else(|| "build panicked".to_owned())
                    });
                state = self.lock();
                state.builds -= 1;
                state.building.remove(&key);
                state.built.insert(key, built);
                self.changed.notify_all();
                continue;
            }
            if state.closed && state.queued.is_empty() {
                return;
            }
            state = self.wait(state);
        }
    }

    /// Waits for `key`'s snapshot, queuing it first if it was not planned,
    /// and lets it go after its last planned use.
    fn take(&self, key: &ColdKey) -> Arc<Snap> {
        let mut state = self.lock();
        let planned = state.uses.contains_key(key);
        if !planned && !state.building.contains(key) && !state.built.contains_key(key) {
            state.queued.push_front(key.clone());
            self.changed.notify_all();
        }
        loop {
            if let Some(built) = state.built.get(key) {
                let built = built.clone();
                let left = state.uses.get_mut(key).map(|uses| {
                    *uses -= 1;
                    *uses
                });
                if left.unwrap_or(0) == 0 {
                    state.built.remove(key);
                    state.uses.remove(key);
                }
                return built
                    .unwrap_or_else(|reason| panic!("{} build failed: {reason}", key.label()));
            }
            state = self.wait(state);
        }
    }

    /// Drops the builds not yet started and lets the workers finish.
    fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        state.queued.clear();
        drop(state);
        self.changed.notify_all();
    }
}

/// Closes the answer keys when dropped, a stage's panic included, so the
/// workers stop instead of waiting for work.
struct CloseOnDrop<'a>(&'a AnswerKeys);

impl Drop for CloseOnDrop<'_> {
    fn drop(&mut self) {
        self.0.close();
    }
}

struct Harness<'k> {
    codebase: PathBuf,
    annotations: PathBuf,
    replay: bool,
    /// Cold build of the unmodified workspace; every warm run starts as a
    /// fork of it.
    base: EmmyLuaAnalysis,
    /// The base's snapshot, the oracle of every run that ends on the text it
    /// started from.
    base_snap: Arc<Snap>,
    /// The base's diagnostics by file, where every warm run's shown
    /// diagnostics start.
    base_shown: BTreeMap<String, Vec<Snapshot>>,
    keys: &'k AnswerKeys,
    /// The most recent non-base oracle, kept only for replay.
    oracle: Option<(ColdInput, EmmyLuaAnalysis)>,
    sweep: std::cell::OnceCell<Vec<SweepFile>>,
    warm_started: Instant,
}

impl Harness<'_> {
    /// A fork of the base that records its `update_index` batches.
    fn warm(&mut self) -> Warm {
        self.warm_started = Instant::now();
        let mut analysis = self.base.fork();
        analysis.compilation.start_batch_log();
        Warm {
            analysis,
            shown: self.base_shown.clone(),
        }
    }

    fn oracle_analysis(&mut self, input: &ColdInput) -> &EmmyLuaAnalysis {
        if *input == ColdInput::default() {
            return &self.base;
        }
        if self.oracle.as_ref().is_none_or(|(kept, _)| kept != input) {
            self.oracle = None;
            let built = self.keys.building(|| {
                build_cold(
                    &self.codebase,
                    &self.annotations,
                    input,
                    1,
                    Schedule::Canonical,
                )
            });
            self.oracle = Some((input.clone(), built));
        }
        &self.oracle.as_ref().expect("oracle was just built").1
    }

    fn expected(&self, input: &ColdInput) -> Arc<Snap> {
        if *input == ColdInput::default() {
            return Arc::clone(&self.base_snap);
        }
        self.keys.take(&ColdKey::oracle(input.clone()))
    }

    /// Reports how far the warm run reached and returns its batch log.
    fn observe(
        &self,
        label: &str,
        warm: &mut Warm,
        edited: &[FileId],
        reach: &Reach,
        strict: bool,
    ) -> Vec<Vec<FileId>> {
        eprintln!(
            "[{label}] warm run {:.2}s",
            self.warm_started.elapsed().as_secs_f64()
        );
        let log = warm.compilation.take_batch_log();
        report_reach(warm, label, edited, reach, strict);
        log
    }

    /// Compares the warm run, its index and the diagnostics the server shows,
    /// with the cold oracle of `input` and, with replay, with that oracle
    /// after replaying the warm run's batch log. Then checks the run's reach
    /// against what the edit changed; see [`check_reach`].
    fn check(
        &mut self,
        label: &str,
        warm: Warm,
        log: &[Vec<FileId>],
        input: &ColdInput,
        edited: &[FileId],
        reach: &Reach,
    ) {
        let reached = Reached::of(&self.base, &warm, edited, reach);
        let actual = Snap {
            index: collect_index(&warm, label),
            diagnostics: warm.shown(),
        };
        drop(warm);
        let expected = self.expected(input);
        compare("expected", &expected, label, &actual);
        check_reach(label, &self.base_snap, &expected, &reached);
        if self.replay {
            let mut replayed = self.oracle_analysis(input).fork();
            replay(&mut replayed, log);
            let replay_label = format!("{label}+replay");
            let expected = snapshot(&replayed, &format!("expected_{replay_label}"));
            drop(replayed);
            compare("replayed", &expected, &replay_label, &actual);
        }
    }

    /// [`Self::observe`], then [`Self::check`].
    fn finish(
        &mut self,
        label: &str,
        mut warm: Warm,
        edited: &[FileId],
        reach: &Reach,
        strict: bool,
        input: &ColdInput,
    ) {
        let log = self.observe(label, &mut warm, edited, reach, strict);
        self.check(label, warm, &log, input, edited, reach);
    }

    /// The cold input with each of `targets` edited.
    /// The cold input a `gate` run of `target_file` ends on: the file removed
    /// and created again with its original text.
    fn recreated_input(&self, target_file: &Target) -> ColdInput {
        let (path, _, _, original) = target(&self.base, &self.codebase, &target_file.relative);
        ColdInput {
            removed: vec![path.clone()],
            created: vec![(path, original)],
            ..ColdInput::default()
        }
    }

    fn edit_input(&self, targets: &[Target]) -> ColdInput {
        ColdInput {
            overrides: targets
                .iter()
                .map(|target_file| {
                    let (path, _, _, original) =
                        target(&self.base, &self.codebase, &target_file.relative);
                    let edit = target_file.edit();
                    (path, edited(&original, &edit.find, &edit.replace))
                })
                .collect(),
            ..ColdInput::default()
        }
    }

    /// The oracles the stages compare with, other than the base, in the
    /// order they use them. Orders and splits are planned before the base.
    fn plan(&self, stages: &[&str], targets: &[Target]) -> Vec<ColdKey> {
        let mut inputs = Vec::new();
        for stage in stages {
            match *stage {
                "edit" | "typing" => inputs.extend(
                    targets
                        .iter()
                        .map(|target_file| self.edit_input(std::slice::from_ref(target_file))),
                ),
                "burst" => inputs.push(self.edit_input(targets)),
                "reload" => inputs.push(reload_input()),
                "gate" => inputs.extend(
                    targets
                        .iter()
                        .map(|target_file| self.recreated_input(target_file)),
                ),
                "touch" | "shift" | "rewrite" | "unrewrite" | "delete" | "recreate" | "rename" => {
                    inputs.extend(
                        self.sweep_batches(stage)
                            .iter()
                            .map(|(_, files)| sweep_input(stage, files)),
                    )
                }
                _ => {}
            }
        }
        inputs
            .into_iter()
            .filter(|input| *input != ColdInput::default())
            .map(ColdKey::oracle)
            .collect()
    }

    fn edit(&mut self, targets: &[Target]) {
        for target_file in targets {
            let relative = &target_file.relative;
            let edit = target_file.edit();
            let (_, uri, file_id, original) = target(&self.base, &self.codebase, relative);
            let mut warm = self.warm();
            let changed = edited(&original, &edit.find, &edit.replace);
            let reach = settle(&mut warm, &uri, file_id, changed, 1);
            let input = self.edit_input(std::slice::from_ref(target_file));
            self.finish(
                &format!("edit[{relative}]"),
                warm,
                &[file_id],
                &reach,
                false,
                &input,
            );
        }
    }

    fn revert(&mut self, targets: &[Target]) {
        for target_file in targets {
            let relative = &target_file.relative;
            let edit = target_file.edit();
            let (_, uri, file_id, original) = target(&self.base, &self.codebase, relative);
            let mut warm = self.warm();
            let changed = edited(&original, &edit.find, &edit.replace);
            let mut reach = settle(&mut warm, &uri, file_id, changed, 1);
            reach.extend(settle(&mut warm, &uri, file_id, original, 2));
            self.finish(
                &format!("revert[{relative}]"),
                warm,
                &[file_id],
                &reach,
                false,
                &ColdInput::default(),
            );
        }
    }

    fn burst(&mut self, targets: &[Target]) {
        let resolved = targets
            .iter()
            .map(|target_file| {
                (
                    target(&self.base, &self.codebase, &target_file.relative),
                    target_file.edit(),
                )
            })
            .collect::<Vec<_>>();
        let mut warm = self.warm();
        for step in 0..3 {
            for ((_, uri, file_id, original), edit) in &resolved {
                let text = match step {
                    0 => original.replace(&edit.find, ""),
                    1 => format!("\n{}", edited(original, &edit.find, &edit.replace)),
                    _ => edited(original, &edit.find, &edit.replace),
                };
                apply_text(&mut warm, uri, *file_id, text, step + 1);
            }
        }
        let ids = resolved
            .iter()
            .map(|((_, _, id, _), _)| *id)
            .collect::<Vec<_>>();
        let affected = warm.reindex_files(ids.clone());
        let reach = warm.settled(affected);
        let input = self.edit_input(targets);
        self.finish("burst", warm, &ids, &reach, false, &input);
    }

    /// Reloads the settled workspace as the server does when its
    /// configuration changes, to the configuration [`reconfigure`] makes and
    /// back. The oracles are a cold build of that configuration, then the
    /// base.
    fn reload(&mut self) {
        let emmyrc = workspace_config(&self.codebase);
        let mut reconfigured = emmyrc.clone();
        reconfigure(&mut reconfigured);
        assert!(
            emmyrc.change_to(&reconfigured).affects_index(),
            "the reload stage's configuration change should reach the index"
        );
        let mut warm = self.warm();
        let reach = reload_with(&mut warm, &self.codebase, &self.annotations, reconfigured);
        let mut back = Warm {
            analysis: warm.fork(),
            shown: warm.shown.clone(),
        };
        back.compilation.start_batch_log();
        self.finish("reload", warm, &[], &reach, false, &reload_input());
        self.warm_started = Instant::now();
        let reach = reload_with(&mut back, &self.codebase, &self.annotations, emmyrc);
        self.finish(
            "reload+back",
            back,
            &[],
            &reach,
            false,
            &ColdInput::default(),
        );
    }

    /// Types the edit into each target as the language server takes it; see
    /// the module docs.
    fn typing(&mut self, targets: &[Target]) {
        let dependents = self.base.dependent_files();
        for target_file in targets {
            let relative = &target_file.relative;
            let (_, uri, file_id, original) = target(&self.base, &self.codebase, relative);
            let readers = dependents.get(&file_id).cloned().unwrap_or_default();
            let mut warm = self.warm();
            let typed = type_edit(
                &mut warm,
                &uri,
                file_id,
                &original,
                target_file.edit(),
                &readers,
            );
            println!(
                "[reach typing[{relative}] per settle] evaluated<={} affected<={}; {} keystroke(s) taken mid re-analysis",
                typed.most_evaluated, typed.most_affected, typed.taken_mid
            );
            let input = self.edit_input(std::slice::from_ref(target_file));
            self.finish(
                &format!("typing[{relative}]"),
                warm,
                &[file_id],
                &typed.reach,
                false,
                &input,
            );
        }
    }

    /// Drives each target as the language-server drift gate does, on one
    /// fork: the edit typed, the original text back, then the file deleted
    /// and re-created through watched-file events, each settled in slices.
    fn gate(&mut self, targets: &[Target]) {
        let dependents = self.base.dependent_files();
        let slice = env_usize("DET_SLICE", SLICE_EVALUATIONS);
        for target_file in targets {
            let relative = &target_file.relative;
            let (_, uri, file_id, original) = target(&self.base, &self.codebase, relative);
            let readers = dependents.get(&file_id).cloned().unwrap_or_default();
            let mut warm = self.warm();
            let typed = type_edit(
                &mut warm,
                &uri,
                file_id,
                &original,
                target_file.edit(),
                &readers,
            );
            let mut reach = typed.reach;
            apply_text(
                &mut warm,
                &uri,
                file_id,
                original.clone(),
                typed.version + 1,
            );
            warm.begin_reindex(vec![file_id]);
            let mut pulled = false;
            let mut affected = settle_in_slices(&mut warm, slice, |analysis| {
                pull_when_settled(analysis, &[file_id], &mut pulled)
            });
            affected.push(file_id);
            reach.extend(warm.settled(affected));
            for text in [None, Some(original.clone())] {
                let changed = warm.stage_file_system_changes(vec![(uri.clone(), text)]);
                warm.begin_reindex(changed.clone());
                let mut pulled = false;
                let mut affected = settle_in_slices(&mut warm, slice, |analysis| {
                    pull_when_settled(analysis, &changed, &mut pulled)
                });
                affected.extend(changed);
                reach.extend(warm.settled(affected));
            }
            let input = self.recreated_input(target_file);
            self.finish(
                &format!("gate[{relative}]"),
                warm,
                &[file_id],
                &reach,
                false,
                &input,
            );
        }
    }

    /// Applies watched-file event batches in order to a warm fork.
    fn watched(
        &mut self,
        label: &str,
        steps: Vec<WatchEvents>,
        edited: &[FileId],
        input: &ColdInput,
    ) {
        let mut warm = self.warm();
        let mut reach = Reach::default();
        let slice = env_usize("DET_SLICE", 0);
        for events in steps {
            let affected = if slice == 0 {
                warm.apply_file_system_changes(events)
            } else {
                let changed = warm.stage_file_system_changes(events);
                if changed.is_empty() {
                    Vec::new()
                } else {
                    warm.begin_reindex(changed.clone());
                    let focus_after = std::env::var("DET_FOCUS_AFTER")
                        .ok()
                        .and_then(|value| value.parse::<usize>().ok());
                    let mut boundaries = 0;
                    let mut affected = settle_in_slices(&mut warm, slice, |analysis| {
                        if focus_after.is_some_and(|after| boundaries >= after) {
                            analysis.compilation.prioritise_update(&changed);
                        }
                        boundaries += 1;
                    });
                    affected.extend(changed);
                    affected
                }
            };
            reach.extend(warm.settled(affected));
        }
        self.finish(label, warm, edited, &reach, false, input);
    }

    fn noop(&mut self, targets: &[Target]) {
        for relative in targets.iter().map(|target_file| &target_file.relative) {
            let (_, _, file_id, _) = target(&self.base, &self.codebase, relative);
            let mut warm = self.warm();
            let affected = warm.reindex_files(vec![file_id]);
            let reach = warm.settled(affected);
            self.finish(
                &format!("noop[{relative}]"),
                warm,
                &[file_id],
                &reach,
                true,
                &ColdInput::default(),
            );
        }
    }

    fn exact(&mut self, targets: &[Target]) {
        let ids = targets
            .iter()
            .map(|target_file| target(&self.base, &self.codebase, &target_file.relative).2)
            .collect::<Vec<_>>();
        let mut warm = self.warm();
        let affected = warm.reindex_files(ids.clone());
        let reach = warm.settled(affected);
        self.finish("exact", warm, &ids, &reach, false, &ColdInput::default());
    }

    /// Compares the cold build of the unmodified workspace taken in `order`
    /// or fed in `batches` with the base.
    fn cold_order(&mut self, label: &str, batches: usize, order: Order) {
        let actual = self.keys.take(&ColdKey {
            input: ColdInput::default(),
            batches,
            order,
        });
        compare("base", &self.base_snap, label, &actual);
    }

    /// `path` relative to the codebase, with forward slashes.
    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.codebase)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// The `DET_HUBS` main files the most other files read from, printed with
    /// their dependent counts.
    fn hubs(&self) -> Vec<FileId> {
        let count = env_usize("DET_HUBS", 0);
        if count == 0 {
            return Vec::new();
        }
        let dependents = self.base.dependent_files();
        let vfs = self.base.compilation.get_db().get_vfs();
        let mut ranked = self
            .base
            .get_main_workspace_file_ids_for_diagnostics()
            .into_iter()
            .filter_map(|file_id| {
                let relative = self.relative(vfs.get_file_path(&file_id)?);
                Some((
                    dependents.get(&file_id).map_or(0, Vec::len),
                    relative,
                    file_id,
                ))
            })
            .collect::<Vec<_>>();
        ranked.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        ranked.truncate(count);
        for (dependents, relative, _) in &ranked {
            eprintln!("[hubs] {dependents:5} dependents  {relative}");
        }
        ranked.into_iter().map(|(.., file_id)| file_id).collect()
    }

    /// The sweep's files: every `DET_STRIDE`-th main file from
    /// `DET_STRIDE_OFFSET`, or the `DET_HUBS` hubs together with the files
    /// whose path ends with an entry of `DET_ONLY`.
    fn sweep_files(&self) -> &[SweepFile] {
        self.sweep.get_or_init(|| {
            let stride = env_usize("DET_STRIDE", 25);
            let offset = env_usize("DET_STRIDE_OFFSET", 0);
            let only = std::env::var("DET_ONLY").ok().map(|value| {
                value
                    .split(',')
                    .map(|entry| entry.trim().replace('\\', "/").to_lowercase())
                    .filter(|entry| !entry.is_empty())
                    .collect::<Vec<_>>()
            });
            let hubs = self.hubs();
            let vfs = self.base.compilation.get_db().get_vfs();
            let mut ids = self.base.get_main_workspace_file_ids_for_diagnostics();
            ids.sort_unstable();
            let files = ids
                .into_iter()
                .enumerate()
                .filter_map(|(i, file_id)| {
                    let path = vfs.get_file_path(&file_id)?;
                    let selected = if only.is_none() && hubs.is_empty() {
                        i % stride == offset
                    } else {
                        hubs.contains(&file_id)
                            || only
                                .iter()
                                .flatten()
                                .any(|entry| path_key(path).ends_with(entry.as_str()))
                    };
                    selected.then(|| {
                        let text = vfs.get_file_content(&file_id)?.clone();
                        Some((path.clone(), file_uri(path), file_id, text))
                    })?
                })
                .collect::<Vec<_>>();
            assert!(
                !files.is_empty(),
                "the sweep selected no file (DET_ONLY={:?}, DET_HUBS={}, DET_STRIDE={stride}, DET_STRIDE_OFFSET={offset})",
                std::env::var("DET_ONLY").unwrap_or_default(),
                hubs.len(),
            );
            eprintln!("[sweep] {} file(s)", files.len());
            files
        })
    }

    /// The sweep's batches with their labels: one batch of every sweep file,
    /// or each file alone with `DET_EACH=1`, so no edited file re-indexes the
    /// readers another edited file left stale.
    fn sweep_batches(&self, stage: &str) -> Vec<(String, Vec<SweepFile>)> {
        let files = self.sweep_files().to_vec();
        if !std::env::var("DET_EACH").is_ok_and(|value| value == "1") {
            return vec![(stage.to_owned(), files)];
        }
        files
            .into_iter()
            .map(|file| (format!("{stage}[{}]", self.relative(&file.0)), vec![file]))
            .collect()
    }

    /// Edits each batch's files as `stage` does and settles them together.
    fn sweep_edit(&mut self, stage: &str) {
        for (label, files) in self.sweep_batches(stage) {
            let mut warm = self.warm();
            for (_, uri, file_id, text) in &files {
                apply_text(&mut warm, uri, *file_id, sweep_edited(stage, text), 1);
            }
            let ids = files.iter().map(|(_, _, id, _)| *id).collect::<Vec<_>>();
            let mut affected = warm.reindex_files(ids.clone());
            if stage == "unrewrite" {
                for (_, uri, file_id, text) in &files {
                    apply_text(&mut warm, uri, *file_id, text.clone(), 2);
                }
                affected.extend(warm.reindex_files(ids.clone()));
            }
            let reach = warm.settled(affected);
            // A rewrite changes what the files export, so its readers run.
            let strict = matches!(stage, "touch" | "shift");
            self.finish(
                &label,
                warm,
                &ids,
                &reach,
                strict,
                &sweep_input(stage, &files),
            );
        }
    }

    fn sweep_delete(&mut self) {
        for (label, files) in self.sweep_batches("delete") {
            let ids = files.iter().map(|(_, _, id, _)| *id).collect::<Vec<_>>();
            let input = sweep_input("delete", &files);
            let events = files.into_iter().map(|(_, uri, ..)| (uri, None)).collect();
            self.watched(&label, vec![events], &ids, &input);
        }
    }

    fn sweep_recreate(&mut self) {
        for (label, files) in self.sweep_batches("recreate") {
            let ids = files.iter().map(|(_, _, id, _)| *id).collect::<Vec<_>>();
            let input = sweep_input("recreate", &files);
            let deletes = files
                .iter()
                .map(|(_, uri, ..)| (uri.clone(), None))
                .collect();
            let creates = files
                .into_iter()
                .map(|(_, uri, _, text)| (uri, Some(text)))
                .collect();
            self.watched(&label, vec![deletes, creates], &ids, &input);
        }
    }

    fn sweep_rename(&mut self) {
        for (label, files) in self.sweep_batches("rename") {
            let ids = files.iter().map(|(_, _, id, _)| *id).collect::<Vec<_>>();
            let input = sweep_input("rename", &files);
            let mut events = files
                .iter()
                .map(|(_, uri, ..)| (uri.clone(), None))
                .collect::<Vec<_>>();
            events.extend(
                input
                    .moved
                    .iter()
                    .map(|(_, path, text)| (file_uri(path), Some(text.clone()))),
            );
            self.watched(&label, vec![events], &ids, &input);
        }
    }
}

/// A sweep file's text after the `touch`, `shift` or `rewrite` edit.
fn sweep_edited(stage: &str, text: &str) -> String {
    match stage {
        "touch" => format!("{text}\n-- touch\n"),
        "shift" => format!("\n{text}"),
        "rewrite" | "unrewrite" => agent_rewrite(text),
        _ => unreachable!("{stage} edits no text"),
    }
}

/// What an agent's light rewrite of a file does in the language-server
/// scenarios: three globals defined at the top, and a parameter added to each
/// of the first three `function name(...)` lines that take a fixed list.
fn agent_rewrite(text: &str) -> String {
    let mut added = 0;
    let mut body = String::with_capacity(text.len() + 64);
    for line in text.split_inclusive('\n') {
        let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
        let signature = line[indent..]
            .strip_prefix("function")
            .filter(|after| after.starts_with([' ', '\t']))
            .and_then(|after| {
                let name = after.trim_start_matches([' ', '\t']);
                let open = name.find('(')?;
                let valid = name[..open].trim_end().split(['.', ':']).all(|part| {
                    part.chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                        && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                });
                let close = name[open..].find(')')? + open;
                let at = line.len() - name.len();
                valid.then_some((at + open, at + close))
            });
        match signature {
            Some((open, close))
                if added < 3 && !line[open + 1..close].trim_end().ends_with("...") =>
            {
                added += 1;
                let params = if line[open + 1..close].trim().is_empty() {
                    "_scn".to_string()
                } else {
                    format!("{}, _scn", &line[open + 1..close])
                };
                body.push_str(&line[..open + 1]);
                body.push_str(&params);
                body.push_str(&line[close..]);
            }
            _ => body.push_str(line),
        }
    }
    format!(
        "function ScnA_det(a, b) return a + b end\nfunction ScnB_det(self, n) local r = {{}} for i = 1, n do r[i] = i end return r end\nScnV_det = {{ name = \"scn\", count = 3 }}\n{body}"
    )
}

/// The cold input a sweep `stage` ends on for `files`.
fn sweep_input(stage: &str, files: &[SweepFile]) -> ColdInput {
    let removed = || files.iter().map(|(path, ..)| path.clone()).collect();
    match stage {
        "unrewrite" => ColdInput::default(),
        "touch" | "shift" | "rewrite" => ColdInput {
            overrides: files
                .iter()
                .map(|(path, _, _, text)| (path.clone(), sweep_edited(stage, text)))
                .collect(),
            ..ColdInput::default()
        },
        "delete" => ColdInput {
            removed: removed(),
            ..ColdInput::default()
        },
        "recreate" => ColdInput {
            removed: removed(),
            created: files
                .iter()
                .map(|(path, _, _, text)| (path.clone(), text.clone()))
                .collect(),
            ..ColdInput::default()
        },
        "rename" => ColdInput {
            moved: files
                .iter()
                .map(|(path, _, _, text)| {
                    let stem = path
                        .file_stem()
                        .expect("workspace file has a name")
                        .to_string_lossy();
                    let renamed = path.with_file_name(format!("{stem}_rn.lua"));
                    (path.clone(), renamed, text.clone())
                })
                .collect(),
            ..ColdInput::default()
        },
        _ => unreachable!("{stage} is not a sweep"),
    }
}

fn main() {
    std::thread::Builder::new()
        .stack_size(glua_code_analysis::ANALYSIS_STACK_SIZE)
        .spawn(run)
        .expect("determinism worker thread should spawn")
        .join()
        .expect("determinism worker thread should not panic");
}

fn run() {
    let codebase = PathBuf::from(std::env::var("DET_CODEBASE").expect("DET_CODEBASE is required"));
    let annotations =
        PathBuf::from(std::env::var("DET_ANNOTATIONS").expect("DET_ANNOTATIONS is required"));
    let stages = std::env::var("DET_STAGES").unwrap_or_else(|_| "schedule,edit,revert,noop".into());
    let stages: Vec<_> = stages
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    assert!(
        !stages.is_empty(),
        "DET_STAGES must select at least one check"
    );
    let order_of = |stage: &str| -> Option<(usize, Order)> {
        if let Some(batches) = stage
            .strip_prefix("split:")
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| *n > 0)
        {
            return Some((batches, Order::Canonical));
        }
        match stage {
            "schedule" => Some((1, Order::Reversed)),
            "schedule:deferred" => Some((1, Order::Deferred)),
            _ => stage
                .strip_prefix("schedule:")
                .and_then(|seed| seed.parse::<u64>().ok())
                .map(|seed| (1, Order::Shuffled(seed))),
        }
    };
    for stage in &stages {
        assert!(
            order_of(stage).is_some()
                || matches!(
                    *stage,
                    "edit"
                        | "revert"
                        | "burst"
                        | "typing"
                        | "gate"
                        | "noop"
                        | "reload"
                        | "exact"
                        | "touch"
                        | "shift"
                        | "rewrite"
                        | "unrewrite"
                        | "delete"
                        | "recreate"
                        | "rename"
                ),
            "unknown check: {stage}"
        );
    }
    let needs_targets = stages.iter().any(|s| {
        matches!(
            *s,
            "edit" | "revert" | "burst" | "typing" | "gate" | "noop" | "exact"
        )
    });
    let targets = targets_from_env();
    assert!(
        !needs_targets || !targets.is_empty(),
        "DET_TARGETS or DET_EDITS must name indexed files"
    );
    let needs_edit = stages
        .iter()
        .any(|s| matches!(*s, "edit" | "revert" | "burst" | "typing"));
    assert!(
        !needs_edit
            || targets.iter().all(|target_file| target_file
                .edit
                .as_ref()
                .is_some_and(|edit| !edit.find.is_empty())),
        "the edit stages need a nonempty edit for every target (DET_EDIT_FIND/DET_EDIT_REPLACE or DET_EDITS)"
    );

    let started = Instant::now();
    let jobs = env_usize("DET_JOBS", 2).max(1);
    // Every other stage compares with a cold build other than the base.
    let base_only = |stage: &str| matches!(stage, "revert" | "noop" | "exact" | "unrewrite");
    assert!(
        jobs >= 2 || stages.iter().all(|stage| base_only(stage)),
        "a cold-build comparison stage needs DET_JOBS of at least 2: its cold build waits for a slot the live base holds"
    );
    let keys = AnswerKeys::new(jobs);
    let build_key = |key: &ColdKey| {
        let built = build_cold(
            &codebase,
            &annotations,
            &key.input,
            key.batches,
            key.order.schedule(),
        );
        snapshot(&built, &key.label())
    };
    let divergences = std::thread::scope(|scope| {
        let _close = CloseOnDrop(&keys);
        for _ in 0..keys.limit {
            std::thread::Builder::new()
                .stack_size(glua_code_analysis::ANALYSIS_STACK_SIZE)
                .spawn_scoped(scope, || keys.work(&build_key))
                .expect("answer key worker should spawn");
        }
        keys.plan(stages.iter().filter_map(|stage| {
            let (batches, order) = order_of(stage)?;
            Some(ColdKey {
                input: ColdInput::default(),
                batches,
                order,
            })
        }));
        let base = keys.keeping(|| {
            build_cold(
                &codebase,
                &annotations,
                &ColdInput::default(),
                1,
                Schedule::Canonical,
            )
        });
        // Diagnosing the base records what each file's diagnostics read, so
        // every warm run reports affected files from the same starting point.
        let base_snap = Arc::new(snapshot(&base, "base"));
        let mut base_shown = BTreeMap::<String, Vec<Snapshot>>::new();
        for diagnostic in &base_snap.diagnostics {
            base_shown
                .entry(diagnostic.file.clone())
                .or_default()
                .push(diagnostic.clone());
        }
        let mut harness = Harness {
            replay: std::env::var("DET_REPLAY").is_ok_and(|value| value == "1"),
            codebase: codebase.clone(),
            annotations: annotations.clone(),
            base,
            base_snap,
            base_shown,
            keys: &keys,
            oracle: None,
            sweep: std::cell::OnceCell::new(),
            warm_started: Instant::now(),
        };
        keys.plan(harness.plan(&stages, &targets));
        println!("[time] setup {:.1}s", started.elapsed().as_secs_f64());
        if let Ok(dump_path) = std::env::var("DET_DUMP") {
            let rendered = harness
                .base_snap
                .diagnostics
                .iter()
                .map(Snapshot::render)
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(dump_path, rendered).expect("diagnostic dump should succeed");
        }
        for stage in &stages {
            let t = Instant::now();
            match *stage {
                "edit" => harness.edit(&targets),
                "revert" => harness.revert(&targets),
                "burst" => harness.burst(&targets),
                "typing" => harness.typing(&targets),
                "gate" => harness.gate(&targets),
                "noop" => harness.noop(&targets),
                "reload" => harness.reload(),
                "exact" => harness.exact(&targets),
                "touch" | "shift" | "rewrite" | "unrewrite" => harness.sweep_edit(stage),
                "delete" => harness.sweep_delete(),
                "recreate" => harness.sweep_recreate(),
                "rename" => harness.sweep_rename(),
                _ => {
                    let (batches, order) = order_of(stage).expect("validated above");
                    harness.cold_order(stage, batches, order);
                }
            }
            println!("[time] {stage} {:.1}s", t.elapsed().as_secs_f64());
        }
        DIVERGENCES.load(Ordering::Relaxed)
    });
    println!("[time] total {:.1}s", started.elapsed().as_secs_f64());
    glua_code_analysis::profile::memory::report_peak();
    if divergences != 0 {
        eprintln!("[determinism] {divergences} comparison(s) differed");
        std::process::exit(1);
    }
}
