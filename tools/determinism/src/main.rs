//! Determinism of settled language-server workspace updates.
//!
//! Four workflows mirror the server: a single edit, a reverted edit, a typing
//! burst, and watched-file deletion/recreation. Each compares diagnostics and
//! derived index state with an equivalent cold or independently settled state.
//!
//! DET_CODEBASE and DET_ANNOTATIONS select the workspaces. DET_TARGETS names
//! comma-separated files relative to DET_CODEBASE. DET_EDIT_FIND and
//! DET_EDIT_REPLACE specify the edit used by the first three workflows.
//! DET_STAGES may select edit,revert,burst,watch (default: all four).

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use glua_code_analysis::{
    DirtySet, EmmyLuaAnalysis, Emmyrc, FileId, WorkspaceFolder, collect_workspace_files,
    load_configs,
};
use glua_parser::{LineIndex, LuaParser};
use lsp_types::{DiagnosticSeverity, NumberOrString};
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

fn build_analysis(codebase: &Path, annotations: &Path) -> EmmyLuaAnalysis {
    build_analysis_with(codebase, annotations, &[], None)
}

/// Cold build of a workspace that never contained `exclude`: the file is
/// dropped from the collected list before indexing, which is not the same
/// state as indexing it and then removing it.
fn build_analysis_excluding(
    codebase: &Path,
    annotations: &Path,
    exclude: &Path,
) -> EmmyLuaAnalysis {
    build_analysis_with(codebase, annotations, &[], Some(exclude))
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
        // Reserve the normal file IDs before indexing the surviving files.
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
    } else {
        analysis.update_files_by_path(files);
    }

    eprintln!("[setup] indexed in {:.2}s", t.elapsed().as_secs_f64());
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

fn target(
    analysis: &EmmyLuaAnalysis,
    codebase: &Path,
    relative: &str,
) -> (PathBuf, lsp_types::Uri, FileId, String) {
    let path = codebase.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    let uri = glua_code_analysis::file_path_to_uri(&path)
        .unwrap_or_else(|| panic!("cannot build URI for {}", path.display()));
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

fn settle(
    analysis: &mut EmmyLuaAnalysis,
    uri: &lsp_types::Uri,
    file_id: FileId,
    text: String,
    version: i32,
) {
    apply_text(analysis, uri, file_id, text, version);
    let dirty = analysis.self_index_and_diff(vec![file_id]);
    analysis.ripple(dirty);
}

fn compare(truth: &EmmyLuaAnalysis, actual: &EmmyLuaAnalysis, label: &str) {
    let before = DIVERGENCES.load(Ordering::Relaxed);
    let expected_index = collect_index(truth, &format!("expected_{label}"));
    let actual_index = collect_index(actual, label);
    diff_index("expected", &expected_index, label, &actual_index);
    let expected = collect(truth, "expected");
    let observed = collect(actual, label);
    diff("expected", &expected, label, &observed);
    if DIVERGENCES.load(Ordering::Relaxed) == before {
        println!("[{label}] +0 index, +0 diagnostics");
    }
}

fn edit(codebase: &Path, annotations: &Path, targets: &[String], find: &str, replace: &str) {
    for relative in targets {
        let mut warm = build_analysis(codebase, annotations);
        let (path, uri, file_id, original) = target(&warm, codebase, relative);
        let changed = edited(&original, find, replace);
        settle(&mut warm, &uri, file_id, changed.clone(), 1);
        let truth = build_analysis_with(codebase, annotations, &[(path, changed)], None);
        compare(&truth, &warm, &format!("edit[{relative}]"));
    }
}

fn revert(codebase: &Path, annotations: &Path, targets: &[String], find: &str, replace: &str) {
    for relative in targets {
        let mut warm = build_analysis(codebase, annotations);
        let (_, uri, file_id, original) = target(&warm, codebase, relative);
        let before_index = collect_index(&warm, "before_revert");
        let before = collect(&warm, "before_revert");
        settle(
            &mut warm,
            &uri,
            file_id,
            edited(&original, find, replace),
            1,
        );
        settle(&mut warm, &uri, file_id, original, 2);
        let label = format!("revert[{relative}]");
        let after_index = collect_index(&warm, &label);
        diff_index("before_revert", &before_index, &label, &after_index);
        let after = collect(&warm, &label);
        diff("before_revert", &before, &label, &after);
    }
}

fn burst(codebase: &Path, annotations: &Path, targets: &[String], find: &str, replace: &str) {
    let mut warm = build_analysis(codebase, annotations);
    let resolved: Vec<_> = targets
        .iter()
        .map(|relative| target(&warm, codebase, relative))
        .collect();
    let final_texts: Vec<_> = resolved
        .iter()
        .map(|(path, _, _, original)| {
            let changed = edited(original, find, replace);
            (path.clone(), format!("\n{changed}"))
        })
        .collect();
    let mut owed: Option<DirtySet> = None;
    for step in 0..3 {
        for (path, uri, file_id, original) in &resolved {
            let text = match step {
                0 => original.replace(find, ""),
                1 => edited(original, find, replace),
                _ => final_texts
                    .iter()
                    .find(|(candidate, _)| candidate == path)
                    .unwrap()
                    .1
                    .clone(),
            };
            apply_text(&mut warm, uri, *file_id, text, step + 1);
            let dirty = warm.self_index_and_diff(vec![*file_id]);
            match &mut owed {
                Some(owed) => owed.extend(dirty),
                None => owed = Some(dirty),
            }
        }
    }
    warm.ripple(owed.expect("a burst has edited files"));
    let truth = build_analysis_with(codebase, annotations, &final_texts, None);
    compare(&truth, &warm, "burst");
}

fn watch(codebase: &Path, annotations: &Path, targets: &[String]) {
    for relative in targets {
        let mut warm = build_analysis(codebase, annotations);
        let (path, uri, _, original) = target(&warm, codebase, relative);
        let mut absent = build_analysis_excluding(codebase, annotations, &path);
        absent
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .remove_file(&uri);
        warm.apply_file_system_changes(vec![(uri.clone(), None)]);
        compare(&absent, &warm, &format!("watch_deleted[{relative}]"));
        warm.apply_file_system_changes(vec![(uri.clone(), Some(original.clone()))]);
        absent.apply_file_system_changes(vec![(uri, Some(original))]);
        compare(&absent, &warm, &format!("watch_created[{relative}]"));
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
    let stages = std::env::var("DET_STAGES").unwrap_or_else(|_| "edit,revert,burst,watch".into());
    let stages: Vec<_> = stages
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    assert!(
        !stages.is_empty(),
        "DET_STAGES must select at least one check"
    );
    for stage in &stages {
        assert!(
            matches!(*stage, "edit" | "revert" | "burst" | "watch"),
            "unknown check: {stage}"
        );
    }
    let targets = std::env::var("DET_TARGETS").expect("DET_TARGETS is required");
    let targets: Vec<_> = targets
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    assert!(!targets.is_empty(), "DET_TARGETS must name indexed files");
    let needs_edit = stages
        .iter()
        .any(|s| matches!(*s, "edit" | "revert" | "burst"));
    let find = if needs_edit {
        std::env::var("DET_EDIT_FIND").expect("DET_EDIT_FIND is required")
    } else {
        String::new()
    };
    let replace = if needs_edit {
        std::env::var("DET_EDIT_REPLACE").expect("DET_EDIT_REPLACE is required")
    } else {
        String::new()
    };
    assert!(
        !needs_edit || !find.is_empty(),
        "DET_EDIT_FIND must be nonempty"
    );
    if let Ok(dump_path) = std::env::var("DET_DUMP") {
        let cold = build_analysis(&codebase, &annotations);
        let rendered = collect(&cold, "cold")
            .iter()
            .map(Snapshot::render)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dump_path, rendered).expect("diagnostic dump should succeed");
    }
    for stage in stages {
        match stage {
            "edit" => edit(&codebase, &annotations, &targets, &find, &replace),
            "revert" => revert(&codebase, &annotations, &targets, &find, &replace),
            "burst" => burst(&codebase, &annotations, &targets, &find, &replace),
            "watch" => watch(&codebase, &annotations, &targets),
            _ => unreachable!(),
        }
    }
    let divergences = DIVERGENCES.load(Ordering::Relaxed);
    if divergences != 0 {
        eprintln!("[determinism] {divergences} comparison(s) differed");
        std::process::exit(1);
    }
}
