//! Workspace diagnostic determinism harness.
//!
//! Answers one question: does re-analysing a workspace produce the same
//! diagnostics as building it from scratch? It indexes a workspace exactly like
//! `init_analysis` does, then re-analyses it in various ways and diffs both the
//! diagnostic sets and the derived indexes they are read from.
//!
//! The stages are ordered by how much they re-analyse, which is what makes a
//! divergence diagnosable: if `allreindex` matches cold but `mainreindex` does
//! not, re-analysis itself is sound and the gap is in which files a partial
//! re-index covers; if `allreindex` diverges too, per-file removal is leaving
//! state behind. `mainexpand` is the production path — it is the one that has to
//! be identical.
//!
//! Example:
//!   DET_CODEBASE=/path/to/addon DET_ANNOTATIONS=/path/to/annotations/output \
//!     DET_STAGES=repeat,edit,mainexpand DET_TARGETS=lua/autorun/init.lua \
//!     cargo run --release -p determinism
//!
//! Env vars:
//!   DET_CODEBASE    (required) workspace root
//!   DET_ANNOTATIONS (required) GMod annotations root (library workspace)
//!   DET_TARGETS     comma separated workspace-relative paths to no-op edit
//!   DET_STAGES      comma separated subset of:
//!                     repeat    re-collect diagnostics with no change at all
//!                     edit      no-op edit each DET_TARGETS entry, through the
//!                               same `update_file_by_uri` path the LSP uses
//!                     exact     reindex DET_TARGETS with no text change and no
//!                               dependency expansion (bisects which file's
//!                               re-analysis perturbs a fact)
//!                     reindex   full clear + rebuild, the ground truth
//!                     order     rebuild with the file list reversed
//!                     split:N   rebuild in N batches instead of one
//!                     mainreindex  re-analyse every main-workspace file at
//!                               once, deliberately *without* the dependency
//!                               expansion
//!                     mainexpand   same set, but through `reindex_files`, i.e.
//!                               the expansion the LSP actually applies
//!                     allreindex   re-analyse every file, library included,
//!                               via per-file removal rather than `clear_index`
//!                     restabilize  re-run analysis over every file *without*
//!                               removing anything first, three times. This is
//!                               the only stage that gives a full build the same
//!                               retained state a partial re-index inherits. It
//!                               diverges wildly and does not converge, which is
//!                               why "just run it again" is not a fix
//!                     fresh     build a second analysis in-process
//!   DET_INDEX_DIFF  also diff type caches, members, signatures, class members,
//!                   super types, net flows and inferred params
//!   DET_SHOW_EXPANSION  print the reindex expansion set for each edit
//!   DET_DUMP        write the cold diagnostic set to this path
//!   DET_DUMP_FILE_IDS   print the main-workspace file id table
//!   DET_DUMP_CLASS  print the member list of this class at each snapshot
//!   DET_LIMIT       max diff lines printed per bucket (default 40)

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use glua_code_analysis::{
    EmmyLuaAnalysis, Emmyrc, FileId, WorkspaceFolder, collect_workspace_files, load_configs,
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
    build_analysis_with(codebase, annotations, Order::Natural, 1)
}

fn build_analysis_with(
    codebase: &Path,
    annotations: &Path,
    order: Order,
    batches: usize,
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
    if order == Order::Reverse {
        files.reverse();
    }
    eprintln!(
        "[setup] collected {} files (order={} batches={batches})",
        files.len(),
        if order == Order::Reverse {
            "reverse"
        } else {
            "natural"
        }
    );

    let t = Instant::now();
    if batches <= 1 {
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
    members: BTreeSet<String>,
    net_flows: Vec<String>,
    inferred_params: BTreeMap<String, String>,
    /// Class hierarchy. A `---@class A : B` link that survives a cold build but
    /// not a partial re-index silently breaks inherited-member lookup, which
    /// then falls back to a by-name search and can land on a sibling class.
    super_types: BTreeMap<String, String>,
    class_members: BTreeMap<String, String>,
    signatures: BTreeMap<String, String>,
}

fn collect_index(analysis: &EmmyLuaAnalysis, label: &str) -> IndexSnapshot {
    let db = analysis.compilation.get_db();
    let type_index = db.get_type_index();
    let mut type_caches = BTreeMap::new();
    for (owner, cache) in type_index.iter_type_caches() {
        type_caches.insert(
            format!("{}|{owner:?}", file_label(analysis, owner.get_file_id())),
            format!("{:?}", cache.as_type()),
        );
    }

    let member_index = db.get_member_index();
    let mut members = BTreeSet::new();
    for file_id in db.get_vfs().get_all_file_ids() {
        let file = file_label(analysis, file_id);
        for member in member_index.get_file_members(file_id) {
            let member_id = member.get_id();
            members.insert(format!(
                "{file}|{:?}|owner={:?}",
                member.get_key(),
                member_index.get_member_owner(&member_id),
            ));
        }
    }
    // Net flows are stored per file; keep raw insertion order so ordering
    // differences (not just membership) show up in the diff.
    let network = db.get_gmod_network_index();
    let mut net_flows = Vec::new();
    for (file_id, flow) in network.iter_send_flows() {
        net_flows.push(format!("send {}|{flow:?}", file_label(analysis, file_id)));
    }
    for (file_id, flow) in network.iter_receive_flows() {
        net_flows.push(format!("recv {}|{flow:?}", file_label(analysis, file_id)));
    }

    let mut super_types = BTreeMap::new();
    // Ordered member list per class. Two members can share a key (a doc `@field`
    // and a file define, or two partial-class contributions); which one wins is
    // decided by this order, so drift here silently changes overload selection
    // without changing the member set at all.
    let mut class_members = BTreeMap::new();
    for type_decl in type_index.get_all_types() {
        let decl_id = type_decl.get_id();
        if let Some(supers) = type_index.get_super_types(&decl_id) {
            super_types.insert(format!("{decl_id:?}"), format!("{supers:?}"));
        }
        let owner = glua_code_analysis::LuaMemberOwner::Type(decl_id.clone());
        if let Some(members) = db.get_member_index().get_members(&owner) {
            let ordered = members
                .iter()
                .map(|member| format!("{:?}@{:?}", member.get_key(), member.get_id()))
                .collect::<Vec<_>>();
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

    // Resolved signature shapes. `inferred_params` only covers call-site-derived
    // facts; the signature's own param and return types are a separate index and
    // drift there changes overload selection without moving a single member.
    let mut signatures = BTreeMap::new();
    for (signature_id, signature) in db.get_signature_index().iter() {
        signatures.insert(
            format!(
                "{}|{signature_id:?}",
                file_label(analysis, signature_id.get_file_id())
            ),
            format!(
                "{:?} -> {:?}",
                signature.get_type_params(),
                signature.get_return_type()
            ),
        );
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

    if let Ok(want) = std::env::var("DET_DUMP_MEMBERS") {
        for entry in &members {
            if entry.contains(&want) {
                println!("  [{label}] member {entry}");
            }
        }
    }

    if let Ok(want) = std::env::var("DET_DUMP_PARAMS") {
        for (key, value) in &inferred_params {
            if key.contains(&want) {
                println!("  [{label}] param {key} = {value}");
            }
        }
    }

    eprintln!(
        "[{label}] index type_caches={} members={} net_flows={}",
        type_caches.len(),
        members.len(),
        net_flows.len()
    );
    IndexSnapshot {
        type_caches,
        members,
        net_flows,
        inferred_params,
        super_types,
        class_members,
        signatures,
    }
}

fn diff_index(base_label: &str, base: &IndexSnapshot, label: &str, other: &IndexSnapshot) {
    let limit = std::env::var("DET_LIMIT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(40);

    let base_keys = base.type_caches.keys().collect::<BTreeSet<_>>();
    let other_keys = other.type_caches.keys().collect::<BTreeSet<_>>();
    let dropped = base_keys.difference(&other_keys).collect::<Vec<_>>();
    let gained = other_keys.difference(&base_keys).collect::<Vec<_>>();
    let changed = base_keys
        .intersection(&other_keys)
        .filter(|key| base.type_caches.get(**key) != other.type_caches.get(**key))
        .collect::<Vec<_>>();
    println!(
        "INDEX {base_label} -> {label}: type_caches dropped={} gained={} changed={}",
        dropped.len(),
        gained.len(),
        changed.len()
    );
    for key in dropped.iter().take(limit) {
        println!("  TC_DROPPED {key} = {:?}", base.type_caches.get(**key));
    }
    for key in gained.iter().take(limit) {
        println!("  TC_GAINED  {key} = {:?}", other.type_caches.get(**key));
    }
    for key in changed.iter().take(limit) {
        println!(
            "  TC_CHANGED {key}\n      before = {:?}\n      after  = {:?}",
            base.type_caches.get(**key),
            other.type_caches.get(**key)
        );
    }

    let member_dropped = base.members.difference(&other.members).collect::<Vec<_>>();
    let member_gained = other.members.difference(&base.members).collect::<Vec<_>>();
    println!(
        "INDEX {base_label} -> {label}: members dropped={} gained={}",
        member_dropped.len(),
        member_gained.len()
    );
    for entry in member_dropped.iter().take(limit) {
        println!("  MEM_DROPPED {entry}");
    }
    for entry in member_gained.iter().take(limit) {
        println!("  MEM_GAINED  {entry}");
    }

    let base_supers = base.super_types.keys().collect::<BTreeSet<_>>();
    let other_supers = other.super_types.keys().collect::<BTreeSet<_>>();
    let super_dropped = base_supers.difference(&other_supers).collect::<Vec<_>>();
    let super_gained = other_supers.difference(&base_supers).collect::<Vec<_>>();
    let super_changed = base_supers
        .intersection(&other_supers)
        .filter(|key| base.super_types.get(**key) != other.super_types.get(**key))
        .collect::<Vec<_>>();
    println!(
        "INDEX {base_label} -> {label}: super_types dropped={} gained={} changed={}",
        super_dropped.len(),
        super_gained.len(),
        super_changed.len()
    );
    for key in super_dropped.iter().take(limit) {
        println!("  SUPER_DROPPED {key} = {:?}", base.super_types.get(**key));
    }
    for key in super_gained.iter().take(limit) {
        println!("  SUPER_GAINED  {key} = {:?}", other.super_types.get(**key));
    }
    for key in super_changed.iter().take(limit) {
        println!(
            "  SUPER_CHANGED {key}\n      before = {:?}\n      after  = {:?}",
            base.super_types.get(**key),
            other.super_types.get(**key)
        );
    }

    let base_sig = base.signatures.keys().collect::<BTreeSet<_>>();
    let other_sig = other.signatures.keys().collect::<BTreeSet<_>>();
    let sig_changed = base_sig
        .intersection(&other_sig)
        .filter(|key| base.signatures.get(**key) != other.signatures.get(**key))
        .collect::<Vec<_>>();
    println!(
        "INDEX {base_label} -> {label}: signatures dropped={} gained={} changed={}",
        base_sig.difference(&other_sig).count(),
        other_sig.difference(&base_sig).count(),
        sig_changed.len()
    );
    for key in sig_changed.iter().take(limit) {
        println!(
            "  SIG_CHANGED {key}\n      before = {:?}\n      after  = {:?}",
            base.signatures.get(**key),
            other.signatures.get(**key)
        );
    }

    let base_cm = base.class_members.keys().collect::<BTreeSet<_>>();
    let other_cm = other.class_members.keys().collect::<BTreeSet<_>>();
    let cm_changed = base_cm
        .intersection(&other_cm)
        .filter(|key| base.class_members.get(**key) != other.class_members.get(**key))
        .collect::<Vec<_>>();
    println!(
        "INDEX {base_label} -> {label}: class_members changed={}",
        cm_changed.len()
    );
    for key in cm_changed.iter().take(limit) {
        println!(
            "  CM_CHANGED {key}
      before = {:?}
      after  = {:?}",
            base.class_members.get(**key),
            other.class_members.get(**key)
        );
    }

    let base_params = base.inferred_params.keys().collect::<BTreeSet<_>>();
    let other_params = other.inferred_params.keys().collect::<BTreeSet<_>>();
    let param_dropped = base_params.difference(&other_params).collect::<Vec<_>>();
    let param_gained = other_params.difference(&base_params).collect::<Vec<_>>();
    let param_changed = base_params
        .intersection(&other_params)
        .filter(|key| base.inferred_params.get(**key) != other.inferred_params.get(**key))
        .collect::<Vec<_>>();
    println!(
        "INDEX {base_label} -> {label}: inferred_params dropped={} gained={} changed={}",
        param_dropped.len(),
        param_gained.len(),
        param_changed.len()
    );
    for key in param_dropped.iter().chain(param_gained.iter()).take(limit) {
        println!("  PARAM_SET {key}");
    }
    for key in param_changed.iter().take(limit) {
        println!(
            "  PARAM_CHANGED {key}
      before = {:?}
      after  = {:?}",
            base.inferred_params.get(**key),
            other.inferred_params.get(**key)
        );
    }

    let base_net = base.net_flows.iter().collect::<BTreeSet<_>>();
    let other_net = other.net_flows.iter().collect::<BTreeSet<_>>();
    let net_dropped = base_net.difference(&other_net).collect::<Vec<_>>();
    let net_gained = other_net.difference(&base_net).collect::<Vec<_>>();
    let net_reordered = base.net_flows != other.net_flows;
    println!(
        "INDEX {base_label} -> {label}: net_flows dropped={} gained={} reordered={net_reordered}",
        net_dropped.len(),
        net_gained.len()
    );
    for entry in net_dropped.iter().take(limit) {
        println!("  NET_DROPPED {}", &entry[..entry.len().min(400)]);
    }
    for entry in net_gained.iter().take(limit) {
        println!("  NET_GAINED  {}", &entry[..entry.len().min(400)]);
    }
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

    let limit = std::env::var("DET_LIMIT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(40);

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

fn noop_edit(analysis: &mut EmmyLuaAnalysis, target: &Path) -> bool {
    let Some(uri) = glua_code_analysis::file_path_to_uri(&target.to_path_buf()) else {
        eprintln!("[edit] cannot build uri for {}", target.display());
        return false;
    };
    let Some(file_id) = analysis.get_file_id(&uri) else {
        eprintln!("[edit] file not indexed: {}", target.display());
        return false;
    };
    let Some(original) = analysis
        .compilation
        .get_db()
        .get_vfs()
        .get_file_content(&file_id)
        .cloned()
    else {
        eprintln!("[edit] no content for {}", target.display());
        return false;
    };

    let expanded = analysis.diagnostic_reindex_scope(vec![file_id]);
    eprintln!(
        "[edit] {} expands to {} files",
        target.display(),
        expanded.len()
    );
    if std::env::var_os("DET_SHOW_EXPANSION").is_some() {
        for expanded_id in &expanded {
            eprintln!("[edit]   {}", file_label(analysis, *expanded_id));
        }
    }

    let t = Instant::now();
    analysis.update_file_by_uri(&uri, Some(format!("{original}\n")));
    analysis.update_file_by_uri(&uri, Some(original));
    eprintln!(
        "[edit] no-op add+remove newline on {} ({:.2}s)",
        target.display(),
        t.elapsed().as_secs_f64()
    );
    true
}

/// Reindex an explicit set of workspace-relative paths without any text change.
/// Isolates "reindexing this set loses information" from "the edit changed text".
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

fn main() {
    let codebase =
        PathBuf::from(std::env::var("DET_CODEBASE").expect("DET_CODEBASE env var is required"));
    let annotations = PathBuf::from(
        std::env::var("DET_ANNOTATIONS").expect("DET_ANNOTATIONS env var is required"),
    );
    let stages = std::env::var("DET_STAGES")
        .unwrap_or_else(|_| "repeat,edit".to_string())
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

    let mut analysis = build_analysis(&codebase, &annotations);
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

    if stages.iter().any(|s| s == "repeat") {
        let again = collect(&analysis, "repeat");
        diff("cold", &cold, "repeat", &again);
    }

    if stages.iter().any(|s| s == "edit") {
        let mut previous = cold.clone();
        let mut previous_label = "cold".to_string();
        let cold_index =
            std::env::var_os("DET_INDEX_DIFF").map(|_| collect_index(&analysis, "cold"));
        for target in &targets {
            let path = codebase.join(target.replace('/', std::path::MAIN_SEPARATOR_STR));
            if !noop_edit(&mut analysis, &path) {
                continue;
            }
            let label = format!("after_edit[{target}]");
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

    if stages.iter().any(|s| s == "exact") {
        let cold_index = collect_index(&analysis, "cold");
        if reindex_exact(&mut analysis, &codebase, &targets) {
            let after_index = collect_index(&analysis, "after_exact");
            diff_index("cold", &cold_index, "after_exact", &after_index);
            let after = collect(&analysis, "after_exact");
            diff("cold", &cold, "after_exact", &after);
        }
    }

    // Re-run the pipeline over every main-workspace file with the full DB
    // already populated. Tests whether the cold build under-resolved because
    // library/std workspace groups were analysed before main-workspace files
    // existed in the index.
    if stages.iter().any(|s| s == "mainreindex") {
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

    // Remove and re-add *every* file, without clearing the index first. Cold and
    // this stage then analyse exactly the same file set from exactly the same
    // (empty, for those files) starting point, so any difference isolates state
    // that `remove_index` fails to drop — as opposed to `mainreindex`, which
    // legitimately leaves the library workspace resolved.
    if stages.iter().any(|s| s == "allreindex") {
        let cold_index = collect_index(&analysis, "cold");
        let all_ids = analysis.compilation.get_db().get_vfs().get_all_file_ids();
        let t = Instant::now();
        analysis.reindex_files_without_expansion(all_ids);
        eprintln!("[allreindex] {:.2}s", t.elapsed().as_secs_f64());
        let after_index = collect_index(&analysis, "after_allreindex");
        diff_index("cold", &cold_index, "after_allreindex", &after_index);
        let after = collect(&analysis, "after_allreindex");
        diff("cold", &cold, "after_allreindex", &after);
    }

    // Re-run analysis over every file *without* removing anything first, so each
    // file re-infers against the settled index while every existing fact is
    // retained. This is the only stage that gives a full build the same
    // advantage a partial re-index gets from the state it inherits, so it says
    // whether the cold answer is simply under-converged.
    if stages.iter().any(|s| s == "restabilize") {
        let all_ids = analysis.compilation.get_db().get_vfs().get_all_file_ids();
        for round in 0..3 {
            let t = Instant::now();
            analysis.compilation.update_index(all_ids.clone());
            let label = format!("after_restabilize_{round}");
            eprintln!(
                "[restabilize] round {round} over {} files ({:.2}s)",
                all_ids.len(),
                t.elapsed().as_secs_f64()
            );
            let after = collect(&analysis, &label);
            diff("cold", &cold, &label, &after);
        }
    }

    // The production incremental path: `reindex_files` runs the same re-analysis
    // as `mainreindex` but first widens the set through `expand_reindex_file_ids`.
    // Comparing the two says whether the expansion is what closes the gap, i.e.
    // whether `mainreindex`'s divergence is a real defect or an artefact of the
    // harness bypassing the safety net.
    if stages.iter().any(|s| s == "mainexpand") {
        let cold_index = collect_index(&analysis, "cold");
        let main_ids = analysis.get_main_workspace_file_ids_for_diagnostics();
        let t = Instant::now();
        analysis.reindex_files(main_ids.clone());
        eprintln!(
            "[mainexpand] reindexed {} main files through the production expansion ({:.2}s)",
            main_ids.len(),
            t.elapsed().as_secs_f64()
        );
        let after_index = collect_index(&analysis, "after_mainexpand");
        diff_index("cold", &cold_index, "after_mainexpand", &after_index);
        let after = collect(&analysis, "after_mainexpand");
        diff("cold", &cold, "after_mainexpand", &after);
    }

    // Re-run the pipeline over *every* file, library included, with the
    // full DB already populated.
    if stages.iter().any(|s| s == "allreindex") {
        let cold_index = collect_index(&analysis, "cold");
        let all_ids = analysis.compilation.get_db().get_vfs().get_all_file_ids();
        let t = Instant::now();
        analysis.reindex_files_without_expansion(all_ids.clone());
        eprintln!(
            "[allreindex] reindexed {} files ({:.2}s)",
            all_ids.len(),
            t.elapsed().as_secs_f64()
        );
        let after_index = collect_index(&analysis, "after_allreindex");
        diff_index("cold", &cold_index, "after_allreindex", &after_index);
        let after = collect(&analysis, "after_allreindex");
        diff("cold", &cold, "after_allreindex", &after);
    }

    if stages.iter().any(|s| s == "reindex") {
        let t = Instant::now();
        analysis.reindex();
        eprintln!("[reindex] full reindex {:.2}s", t.elapsed().as_secs_f64());
        let after = collect(&analysis, "after_full_reindex");
        diff("cold", &cold, "after_full_reindex", &after);
    }

    if stages.iter().any(|s| s == "fresh") {
        let fresh_analysis = build_analysis(&codebase, &annotations);
        let fresh = collect(&fresh_analysis, "fresh_process");
        diff("cold", &cold, "fresh_process", &fresh);
    }

    if stages.iter().any(|s| s == "order") {
        let reversed_analysis = build_analysis_with(&codebase, &annotations, Order::Reverse, 1);
        let reversed = collect(&reversed_analysis, "reverse_order");
        diff("cold", &cold, "reverse_order", &reversed);
    }

    if let Some(batches) = stages.iter().find_map(|s| s.strip_prefix("split:")) {
        let batches = batches.parse::<usize>().unwrap_or(2);
        let split_analysis = build_analysis_with(&codebase, &annotations, Order::Natural, batches);
        if std::env::var_os("DET_INDEX_DIFF").is_some() {
            let cold_index = collect_index(&analysis, "cold");
            let split_index = collect_index(&split_analysis, "split_batches");
            diff_index("cold", &cold_index, "split_batches", &split_index);
        }
        let split = collect(&split_analysis, "split_batches");
        diff("cold", &cold, "split_batches", &split);
    }
}
