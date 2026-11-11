use glua_parser::LuaSyntaxNodeExt;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::{
    collections::VecDeque,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
};

use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAst, LuaAstNode, LuaAstToken, LuaBinaryExpr, LuaBlock,
    LuaCallExpr, LuaChunk, LuaClosureExpr, LuaComment, LuaCommentOwner, LuaDocDescriptionOwner,
    LuaDocTag, LuaDocTagFileparam, LuaDocTagRealm, LuaElseClauseStat, LuaElseIfClauseStat, LuaExpr,
    LuaForRangeStat, LuaForStat, LuaFuncStat, LuaIfStat, LuaIndexKey, LuaLiteralToken,
    LuaLocalFuncStat, LuaLocalName, LuaLocalStat, LuaNameExpr, LuaParamName, LuaRepeatStat,
    LuaStat, LuaSyntaxId, LuaSyntaxNode, LuaTableExpr, LuaTableField, LuaVarExpr, LuaWhileStat,
    NumberResult, PathTrait,
};

use crate::semantic::{local_initializer_expr, semantic_decl_closure_signature_id};
use crate::{
    EmmyrcGmodRealm, FileId, GlobalId, GmodClassCallArg, GmodClassCallArgSource,
    GmodClassCallLiteral, GmodDermaSkinCallRoles, GmodNamedStringCallRoles,
    GmodNetworkVarCallRoles, GmodScriptedClassCallKind, GmodScriptedClassCallMetadata,
    GmodScriptedClassFileMetadata, GmodScriptedClassRegisterRoles, GmodVguiPanelCallRoles,
    GmodVguiParentCallMetadata, GmodVguiParentSource, InFiled, LuaCallArgRole, LuaDecl,
    LuaDeclExtra, LuaDeclId, LuaDeclTypeKind, LuaFunctionType, LuaInferCache, LuaMember,
    LuaMemberFeature, LuaMemberId, LuaMemberKey, LuaSignature, LuaSignatureId, LuaType,
    LuaTypeCache, LuaTypeDecl, LuaTypeDeclId, LuaTypeFlag, LuaTypeOwner,
    compilation::analyzer::{
        AnalyzeContext,
        common::{TypeCacheWriteMode, write_type_cache},
    },
    db_index::rebuild_effective_valid_guard_signatures,
    db_index::{
        AsyncState, CallFlows, DbIndex, GmodCallbackSiteMetadata, GmodConVarKind,
        GmodConVarSiteMetadata, GmodConcommandSiteMetadata, GmodExecutionEnvironmentFileFlow,
        GmodExecutionEnvironmentSite, GmodExecutionEnvironmentSource, GmodFileLoadInfo,
        GmodHookKind, GmodHookNameIssue, GmodHookSiteMetadata, GmodLoadConfidence, GmodLoadEdge,
        GmodLoadEdgeKind, GmodLoadRoot, GmodLoadRootKind, GmodLoadStatus, GmodNamedSiteMetadata,
        GmodNetReceiveSiteMetadata, GmodRealm, GmodRealmFileMetadata, GmodRealmRange,
        GmodScopedClassInfo, GmodStateMask, GmodSystemFileMetadata, GmodTimerKind,
        GmodTimerSiteMetadata, LuaDependencyKind, LuaDependencySite, LuaMemberOwner, NetFlowFrame,
        NetFlowKind, NetNameExpansion, NetOpDescriptor, NetOpDirection, NetOpEntry, NetReceiveFlow,
        NetSendFlow, NetSendKind, TableNumericRangePopulation, WorkspaceKind,
    },
    infer_expr,
    semantic::find_latest_decl_write_before_position,
};
use rowan::{TextRange, TextSize};
use smol_str::SmolStr;

mod call_roles;
mod derive;
mod network;
mod numeric_range_population;

use call_roles::GmodCallRoles;
pub(crate) use derive::{
    apply_pre, compute_pre, evaluate_call_roles, evaluate_class_call_sites, evaluate_load,
    evaluate_load_call_sites, evaluate_net_helper_names, evaluate_post, evaluate_valid_guards,
    evaluate_vgui_chains, evaluate_vgui_relations,
};
pub(crate) use network::{
    FileNetSyntax, NetCompositions, evaluate_net_call, evaluate_net_flows, evaluate_net_helper,
};

/// Cheap pre-analysis flags used to skip unrelated GMod collectors.
#[derive(Default)]
struct GmodKeywords {
    /// "hook" — hook.Add/Run/Remove call sites, hook method sites
    has_hook: bool,
    /// timer/concommand/ConVar/AddNetworkString — system call metadata
    has_system_call: bool,
    /// Annotated scripted-class wrappers (VGUI, Derma, NetworkVar, inheritance)
    has_scripted_class_call: bool,
    /// Annotated load wrappers (`include`, `AddCSLuaFile`, `IncludeCS`, `require`)
    has_load_call: bool,
    /// "GM:" or "GAMEMODE:" — GM/GAMEMODE method sites
    has_gm_func: bool,
    /// "CLIENT", "SERVER", or "MENU_DLL" — branch realm ranges.
    has_realm_branch: bool,
    /// "@realm" — file-level realm annotation
    has_realm_anno: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
struct AnnotatedGmodCandidatePresence {
    has_system: bool,
    has_hook: bool,
    has_scripted_class: bool,
    has_load: bool,
    has_environment: bool,
    has_file_find: bool,
}

impl AnnotatedGmodCandidatePresence {
    fn merge(&mut self, other: Self) {
        self.has_system |= other.has_system;
        self.has_hook |= other.has_hook;
        self.has_scripted_class |= other.has_scripted_class;
        self.has_load |= other.has_load;
        self.has_environment |= other.has_environment;
        self.has_file_find |= other.has_file_find;
    }
}

impl GmodKeywords {
    /// Whether any non-network metadata collection is needed.
    fn needs_hook_metadata(&self) -> bool {
        self.has_hook || self.has_system_call || self.has_gm_func || self.has_realm_anno
    }
}

fn scan_gmod_keywords(
    db: &DbIndex,
    file_id: FileId,
    content: &str,
    formatted_hook_prefixes: &[String],
    annotated_global_call_roles: &AnnotatedGmodGlobalCallRoleMap,
) -> GmodKeywords {
    let has_gm_func = content.contains("GM:")
        || content.contains("GAMEMODE:")
        || formatted_hook_prefixes.iter().any(|p| content.contains(p));
    let has_hook_annotation = content.contains("gmod.hook");
    let has_system_annotation = content.contains("gmod.concommand")
        || content.contains("gmod.convar")
        || content.contains("gmod.timer");
    let has_scripted_class_annotation = content.contains("gmod.vgui_panel")
        || content.contains("gmod.derma_skin")
        || content.contains("gmod.network_var")
        || content.contains("gmod.class_base")
        || content.contains("gmod.gamemode");
    let has_load_annotation = content.contains("gmod.load");
    let annotated_candidates = annotated_global_call_roles.candidate_presence(db, file_id);
    let calls_configured_emitter = db
        .get_file_emmyrc(file_id)
        .gmod
        .hook_mappings
        .emitter_to_hook
        .keys()
        .any(|emitter_path| content.contains(configured_call_name(emitter_path)));
    GmodKeywords {
        has_hook: content.contains("hook")
            || has_hook_annotation
            || annotated_candidates.has_hook
            || calls_configured_emitter,
        has_system_call: content.contains("timer.")
            || content.contains("concommand")
            || content.contains("ConVar")
            || content.contains("AddNetworkString")
            || has_system_annotation
            || annotated_candidates.has_system,
        has_scripted_class_call: has_scripted_class_annotation
            || annotated_candidates.has_scripted_class,
        has_load_call: has_load_annotation || annotated_candidates.has_load,
        has_gm_func,
        has_realm_branch: content.contains("CLIENT")
            || content.contains("SERVER")
            || content.contains("MENU_DLL"),
        has_realm_anno: content.contains("@realm"),
    }
}

fn collect_numeric_range_table_populations(db: &mut DbIndex, context: &AnalyzeContext) {
    for in_filed_tree in &context.tree_list {
        let file_id = in_filed_tree.file_id;
        let populations =
            numeric_range_population::collect_numeric_range_table_populations_for_file(
                db,
                file_id,
                in_filed_tree.value.clone(),
            );
        db.get_numeric_range_population_index_mut()
            .set_file_populations(file_id, populations);
    }
}

/// Hook method prefixes, pre-formatted once to avoid a per-file
/// `format!("{prefix}:")` allocation in the call-site scan.
fn formatted_hook_prefixes(db: &DbIndex, file_id: FileId) -> Vec<String> {
    let gmod = &db.get_file_emmyrc(file_id).gmod;
    gmod.hook_mappings
        .method_prefixes
        .iter()
        .cloned()
        .chain(gmod.scripted_class_scopes.hook_owner_globals())
        .map(|prefix| format!("{prefix}:"))
        .collect()
}

/// A db write produced by the per-file annotated call-site scan.
///
/// The scan itself reads only the file's own AST plus immutable db state; the
/// writes are buffered so the scan can run off the caller's thread and be
/// applied afterwards in the original file-then-call order.
pub(crate) enum PendingCallSite {
    VguiParent(GmodVguiParentCallMetadata),
    ScriptedClass(
        GmodScriptedClassCallKind,
        Box<GmodScriptedClassCallMetadata>,
    ),
    Dependency(LuaDependencySite),
    LoadScan(GmodFileLoadScan),
}

/// A file's own contribution to the load graph, scanned once from its text:
/// its `CompileFile` execution-environment flow and its dynamic loaders. The
/// GmodLoad fold resolves them against the workspace.
#[derive(Default)]
pub(crate) struct GmodFileLoadScan {
    flow: GmodExecutionEnvironmentFileFlow,
    dynamic_loaders: Vec<DynamicLoadCandidate>,
}

impl GmodFileLoadScan {
    fn is_empty(&self) -> bool {
        self.flow.is_empty() && self.dynamic_loaders.is_empty()
    }
}

impl crate::db_index::derivation::DerivedFact for GmodFileLoadScan {
    fn fingerprint(&self) -> crate::db_index::derivation::Fingerprint {
        crate::db_index::derivation::fingerprint_of(&(
            self.flow.fingerprint(),
            &self.dynamic_loaders,
        ))
    }
}

/// Scripted-class registration (and, when `include_load`, `load`-style
/// dependency) call sites for every file in one workspace group.
fn collect_annotated_call_sites_with(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    annotated_global_call_roles: &AnnotatedGmodGlobalCallRoleMap,
    include_load: bool,
    include_class_calls: bool,
) {
    let file_ids = context
        .tree_list
        .iter()
        .map(|in_filed_tree| in_filed_tree.file_id)
        .collect::<Vec<_>>();
    let per_file = super::parallel::map_files_collect(db, &file_ids, |db, file_id| {
        file_annotated_call_sites(
            db,
            file_id,
            annotated_global_call_roles,
            include_load,
            include_class_calls,
        )
    });
    for (file_id, pending) in file_ids.iter().zip(per_file) {
        apply_annotated_call_sites(db, *file_id, pending);
    }
}

/// A file's annotated `load`-style or scripted-class call sites, read off the
/// index; [`apply_annotated_call_sites`] stores them.
pub(crate) fn compute_annotated_call_sites(
    db: &DbIndex,
    file_id: FileId,
    include_load: bool,
    include_class_calls: bool,
) -> Vec<PendingCallSite> {
    if !db.get_emmyrc().gmod.enabled {
        return Vec::new();
    }
    let roles = derive::call_role_entries(db);
    file_annotated_call_sites(db, file_id, &roles.roles, include_load, include_class_calls)
}

fn file_annotated_call_sites(
    db: &DbIndex,
    file_id: FileId,
    annotated_global_call_roles: &AnnotatedGmodGlobalCallRoleMap,
    include_load: bool,
    include_class_calls: bool,
) -> Vec<PendingCallSite> {
    let formatted_hook_prefixes = &formatted_hook_prefixes(db, file_id);
    let mut pending = Vec::new();
    if include_load {
        pending.push(PendingCallSite::LoadScan(GmodFileLoadScan {
            flow: collect_compilefile_execution_environment_flow(
                db,
                file_id,
                annotated_global_call_roles,
            ),
            dynamic_loaders: collect_file_dynamic_loaders(db, file_id, annotated_global_call_roles),
        }));
    }
    let keywords = db
        .get_vfs()
        .get_file_content(&file_id)
        .map(|content| {
            scan_gmod_keywords(
                db,
                file_id,
                content,
                formatted_hook_prefixes,
                annotated_global_call_roles,
            )
        })
        .unwrap_or_default();
    let scan_load = include_load && keywords.has_load_call;
    let scan_class_calls = include_class_calls && keywords.has_scripted_class_call;
    if !scan_class_calls && !scan_load {
        return pending;
    }
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_chunk_node())
    else {
        return pending;
    };

    let annotated_call_roles =
        AnnotatedGmodCallRoleMap::build(db, file_id, &root, annotated_global_call_roles);
    for call_expr in root.descendants::<LuaCallExpr>() {
        if scan_class_calls {
            collect_annotated_scripted_class_call_metadata(
                db,
                file_id,
                &annotated_call_roles,
                call_expr.clone(),
                &mut pending,
            );
        }
        if scan_load {
            collect_annotated_load_dependency_site(
                db,
                file_id,
                &annotated_call_roles,
                call_expr,
                &mut pending,
            );
        }
    }
    pending
}

pub(crate) fn apply_annotated_call_sites(
    db: &mut DbIndex,
    file_id: FileId,
    pending: Vec<PendingCallSite>,
) {
    for write in pending {
        match write {
            PendingCallSite::VguiParent(call) => db
                .get_gmod_class_metadata_index_mut()
                .add_vgui_parent_call(file_id, call),
            PendingCallSite::ScriptedClass(kind, call) => db
                .get_gmod_class_metadata_index_mut()
                .add_call(file_id, kind, *call),
            PendingCallSite::Dependency(site) => db
                .get_file_dependencies_index_mut()
                .add_dependency_site(site),
            PendingCallSite::LoadScan(scan) if scan.is_empty() => db.remove_load_scan(file_id),
            PendingCallSite::LoadScan(scan) => db.set_load_scan(file_id, Arc::new(scan)),
        }
    }
}

fn collect_annotated_load_dependency_site(
    db: &DbIndex,
    file_id: FileId,
    annotated_roles: &AnnotatedGmodCallRoleMap,
    call_expr: LuaCallExpr,
    pending: &mut Vec<PendingCallSite>,
) -> Option<()> {
    let call_path = call_expr.get_access_path()?;
    let (kind, path_arg_idx) = annotated_roles.load_call(db, file_id, &call_expr, &call_path)?;
    let arg_expr = call_expr
        .get_args_list()
        .and_then(|args| args.get_args().nth(path_arg_idx))?;
    let path = static_literal_string(&arg_expr);
    let target_file_id = path
        .as_deref()
        .and_then(|path| resolve_load_dependency_target(db, file_id, kind, path));
    let path_keys = path
        .as_deref()
        .map(|path| crate::dependency_site_path_keys(db, file_id, path))
        .unwrap_or_default();

    pending.push(PendingCallSite::Dependency(LuaDependencySite {
        source_file_id: file_id,
        target_file_id,
        kind,
        path,
        path_keys,
        original_expr: call_expr.syntax().text().to_string(),
        call_range: call_expr.get_range(),
        range: arg_expr.get_range(),
    }));
    Some(())
}

/// Workspace-global registry of helper function definitions, stored as
/// `(FileId, LuaSyntaxId)` rather than live red-tree nodes so the registry is
/// `Send + Sync` and can be shared across the parallel per-file collection
/// workers. Each entry is resolved back to a `(LuaBlock, LuaChunk)` on demand by
/// rebuilding the owning file's red tree from the (Send) green tree in the VFS.
#[derive(Default, Clone)]
pub(crate) struct HelperRegistry {
    /// Function bodies keyed by the language server's canonical global symbol
    /// identity. This is a call-graph lookup, not a net-op recognizer: the body
    /// is still inspected through annotated signatures only.
    globals: HashMap<GlobalId, (FileId, LuaSyntaxId)>,
    /// Unique method names provide a conservative fallback for dynamic Lua
    /// receivers whose class cannot be inferred. Ambiguous method names are
    /// deliberately removed during construction.
    methods: HashMap<SmolStr, (FileId, LuaSyntaxId)>,
    signatures: HashMap<LuaSignatureId, (FileId, LuaSyntaxId)>,
}

impl HelperRegistry {
    /// The helper body a global function names, read as that entry.
    fn global(&self, global_id: &GlobalId) -> Option<&(FileId, LuaSyntaxId)> {
        crate::db_index::derivation::note_read(|| {
            crate::db_index::derivation::FactKey::CallRole(
                crate::db_index::derivation::CallRoleKey::Global(global_id.clone()),
            )
        });
        self.globals.get(global_id)
    }

    /// The helper body a method name uniquely names, read as that entry.
    fn method(&self, name: &SmolStr) -> Option<&(FileId, LuaSyntaxId)> {
        crate::db_index::derivation::note_read(|| {
            crate::db_index::derivation::FactKey::CallRole(
                crate::db_index::derivation::CallRoleKey::Method(name.clone()),
            )
        });
        self.methods.get(name)
    }

    /// The helper body a signature resolves to, read as that entry.
    fn signature(&self, signature_id: &LuaSignatureId) -> Option<&(FileId, LuaSyntaxId)> {
        crate::db_index::derivation::note_read(|| {
            crate::db_index::derivation::FactKey::CallRole(
                crate::db_index::derivation::CallRoleKey::Signature(*signature_id),
            )
        });
        self.signatures.get(signature_id)
    }
}

/// A helper body: its signature, where it is, the global it defines, and the
/// member it is stored under on a table with no global root.
type IndexedHelperDefinition = (
    LuaSignatureId,
    FileId,
    LuaSyntaxId,
    Option<GlobalId>,
    Option<SmolStr>,
);

/// One file's contribution to the gmod call roles and net-helper registry.
pub(crate) struct FileHelperScan {
    role_map: AnnotatedGmodGlobalCallRoleMap,
    definitions: Vec<IndexedHelperDefinition>,
    forwarded_calls: Vec<ForwardedCall>,
}

impl crate::db_index::derivation::DerivedFact for FileHelperScan {
    fn fingerprint(&self) -> crate::db_index::derivation::Fingerprint {
        let definitions = self
            .definitions
            .iter()
            .map(|(signature_id, file_id, syntax_id, global, member)| {
                (
                    signature_id,
                    InFiled::new(*file_id, *syntax_id),
                    global,
                    member,
                )
            })
            .collect::<Vec<_>>();
        crate::db_index::derivation::fingerprint_of(&(
            self.role_map.fingerprint(),
            definitions,
            &self.forwarded_calls,
        ))
    }
}

/// Scans one file's signatures for call roles and net-helper definitions;
/// [`apply_helper_scan`] stores the scan. A file without signatures has none.
pub(crate) fn compute_helper_scan(db: &DbIndex, file_id: FileId) -> Option<FileHelperScan> {
    if !db.get_emmyrc().gmod.enabled {
        return None;
    }
    let mut signature_ids = db
        .get_signature_index()
        .get_file_signature_ids(file_id)
        .map(|ids| ids.iter().copied().collect::<Vec<_>>())
        .unwrap_or_default();
    if signature_ids.is_empty() {
        return None;
    }
    signature_ids.sort_unstable_by_key(|signature_id| signature_id.get_position());
    let has_calls = db.get_vfs().get_syntax_tree(&file_id).is_some_and(|tree| {
        tree.get_chunk_node()
            .descendants::<LuaCallExpr>()
            .next()
            .is_some()
    });
    Some(AnnotatedGmodGlobalCallRoleMap::build_for_file(
        db,
        &signature_ids,
        has_calls,
    ))
}

pub(crate) fn apply_helper_scan(db: &mut DbIndex, file_id: FileId, scan: Option<FileHelperScan>) {
    match scan {
        Some(scan) => db.set_helper_scan(file_id, Arc::new(scan)),
        None => db.remove_helper_scan(file_id),
    }
}

/// The global paths `file_id`'s syntax names, absent with gmod disabled:
/// part of its source layer, after its references are complete.
pub(crate) fn collect_role_mentions(
    db: &DbIndex,
    file_id: FileId,
    root: &LuaChunk,
) -> Option<Arc<[SmolStr]>> {
    db.get_emmyrc()
        .gmod
        .enabled
        .then(|| call_role_mentions(db, file_id, root))
}

/// Sorted global paths and `*.<method>` receiver mentions that call-role lookups
/// can read; locals name no global unless they localise it, as `local vgui = vgui` does.
fn call_role_mentions(db: &DbIndex, file_id: FileId, root: &LuaChunk) -> Arc<[SmolStr]> {
    let references = db.get_reference_index();
    let decl_tree = db.get_decl_index().get_decl_tree(&file_id);
    let names_no_global = |name_expr: &LuaNameExpr| {
        references
            .get_var_reference_decl(&file_id, name_expr.get_range())
            .filter(|decl_id| {
                decl_tree
                    .and_then(|tree| tree.get_decl(decl_id))
                    .is_some_and(|decl| decl.is_local())
            })
            .is_some_and(|decl_id| {
                !name_expr
                    .get_name_text()
                    .is_some_and(|name| local_decl_aliases_global_name(db, decl_id, &name))
            })
    };
    let mut paths = Vec::new();
    for name_expr in root.descendants::<LuaNameExpr>() {
        let global_root = !names_no_global(&name_expr);
        let Some(name) = name_expr.get_name_text() else {
            continue;
        };
        if global_root {
            paths.push(name);
        }
        let mut node = name_expr.syntax().clone();
        while let Some(parent) = node.parent() {
            let is_prefix = if let Some(index_expr) =
                glua_parser::LuaIndexExpr::cast(parent.clone())
            {
                let is_prefix = index_expr
                    .get_prefix_expr()
                    .is_some_and(|prefix| *prefix.syntax() == node);
                if is_prefix
                    && global_root
                    && let Some(path) = index_expr.get_access_path()
                {
                    paths.push(path);
                }
                is_prefix
            } else if let Some(call_expr) = LuaCallExpr::cast(parent.clone()) {
                let is_prefix = call_expr
                    .get_prefix_expr()
                    .is_some_and(|prefix| *prefix.syntax() == node);
                if is_prefix && let Some(index_expr) = glua_parser::LuaIndexExpr::cast(node.clone())
                {
                    let mention = match index_expr.get_index_key() {
                        Some(LuaIndexKey::Name(name)) => {
                            Some(SmolStr::new(format!("*.{}", name.get_name_text())))
                        }
                        Some(LuaIndexKey::String(name)) => {
                            Some(SmolStr::new(format!("*.{}", name.get_value())))
                        }
                        _ => None,
                    };
                    if let Some(mention) = mention {
                        paths.push(mention);
                    }
                }
                is_prefix
            } else {
                false
            };
            if !is_prefix {
                break;
            }
            node = parent;
        }
    }
    paths.sort_unstable();
    paths.dedup();
    paths.into()
}

#[derive(Default)]
struct HelperRegistryBuilder {
    definitions: Vec<IndexedHelperDefinition>,
    forwarded_calls: Vec<ForwardedCall>,
    /// Whether the scanned file contains any call expression at all. Computed
    /// once per file so annotation libraries full of empty stubs are rejected
    /// before resolving every signature back to a red-tree closure.
    file_has_calls: bool,
}

impl HelperRegistryBuilder {
    fn add_signature(&mut self, db: &DbIndex, signature_id: LuaSignatureId) {
        if !self.file_has_calls {
            return;
        }
        let file_id = signature_id.get_file_id();

        let Some(closure) = closure_from_signature_id(db, signature_id) else {
            return;
        };
        let Some(block) = closure.get_block() else {
            return;
        };
        // Annotation libraries contain thousands of empty function stubs.
        // They can carry net metadata, but they cannot be wrapper bodies.
        // Checking for a first statement is constant-time and avoids walking
        // every empty stub's red subtree during the signature scan.
        if block.get_stats().next().is_none() {
            return;
        }
        let global_path = global_call_path_for_signature_closure(db, signature_id, &closure)
            .map(|path| path.strip_prefix("_G.").unwrap_or(&path).to_string());
        if let Some(path) = &global_path {
            self.forwarded_calls
                .extend(collect_forwarded_calls(db, signature_id, &block, path));
        }
        let global_id = global_path.as_deref().map(GlobalId::new);
        let local_member = global_id
            .is_none()
            .then(|| closure_member_name(&closure))
            .flatten();
        self.definitions.push((
            signature_id,
            file_id,
            LuaSyntaxId::from_node(block.syntax()),
            global_id,
            local_member,
        ));
    }
}

/// A call inside a global function that passes the function's parameters on.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ForwardedCall {
    /// The path the function is called by.
    wrapper_path: String,
    callee_path: String,
    callee_is_colon: bool,
    args: Vec<ForwardedArg>,
}

/// An argument of a [`ForwardedCall`], written in terms of the function's
/// parameters.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ForwardedArg {
    /// A parameter, or a field read from it, with fixed text around it.
    Param {
        param_idx: usize,
        field_path: Vec<String>,
        prefix: String,
        suffix: String,
    },
    Other,
}

/// The calls in a global function's body that pass its parameters on. A colon
/// function is left out: where its `self` falls among the arguments depends on
/// how it is called.
fn collect_forwarded_calls(
    db: &DbIndex,
    signature_id: LuaSignatureId,
    block: &LuaBlock,
    wrapper_path: &str,
) -> Vec<ForwardedCall> {
    let file_id = signature_id.get_file_id();
    let Some(signature) = db.get_signature_index().get_declared(&signature_id) else {
        return Vec::new();
    };
    if signature.is_colon_define || signature.params.is_empty() {
        return Vec::new();
    }
    let Some(closure) = block.get_parent::<LuaClosureExpr>() else {
        return Vec::new();
    };

    let mut forwarded_calls = Vec::new();
    for call_expr in block.descendants::<LuaCallExpr>() {
        if call_expr.ancestors::<LuaClosureExpr>().next().as_ref() != Some(&closure) {
            continue;
        }
        let Some(callee_path) = call_expr.get_access_path() else {
            continue;
        };
        if call_expr_local_root_decl_id(db, file_id, &call_expr).is_some() {
            continue;
        }
        let Some(args_list) = call_expr.get_args_list() else {
            continue;
        };
        let args = args_list
            .get_args()
            .map(|arg| forwarded_arg(&arg, &signature.params))
            .collect::<Vec<_>>();
        if args.iter().all(|arg| matches!(arg, ForwardedArg::Other)) {
            continue;
        }
        forwarded_calls.push(ForwardedCall {
            wrapper_path: wrapper_path.to_string(),
            callee_path: callee_path.to_string(),
            callee_is_colon: call_expr.is_colon_call(),
            args,
        });
    }
    forwarded_calls
}

fn forwarded_arg(expr: &LuaExpr, params: &[SmolStr]) -> ForwardedArg {
    enum Piece {
        Text(String),
        Param(usize, Vec<String>),
        Other,
    }

    fn push_pieces(expr: &LuaExpr, params: &[SmolStr], pieces: &mut Vec<Piece>) {
        let param_idx = |name: &str| params.iter().position(|param| param == name);
        match expr {
            LuaExpr::LiteralExpr(_) => {
                pieces.push(static_literal_string(expr).map_or(Piece::Other, Piece::Text));
            }
            LuaExpr::ParenExpr(paren_expr) => match paren_expr.get_expr() {
                Some(inner) => push_pieces(&inner, params, pieces),
                None => pieces.push(Piece::Other),
            },
            LuaExpr::BinaryExpr(binary_expr)
                if binary_expr
                    .get_op_token()
                    .is_some_and(|op| op.get_op() == BinaryOperator::OpConcat) =>
            {
                match binary_expr.get_exprs() {
                    Some((left, right)) => {
                        push_pieces(&left, params, pieces);
                        push_pieces(&right, params, pieces);
                    }
                    None => pieces.push(Piece::Other),
                }
            }
            LuaExpr::NameExpr(name_expr) => pieces.push(
                name_expr
                    .get_name_text()
                    .and_then(|name| param_idx(&name))
                    .map_or(Piece::Other, |idx| Piece::Param(idx, Vec::new())),
            ),
            LuaExpr::IndexExpr(index_expr) => {
                pieces.push(match index_expr_root_and_field_path(index_expr) {
                    Some((LuaExpr::NameExpr(root), field_path)) => root
                        .get_name_text()
                        .and_then(|name| param_idx(&name))
                        .map_or(Piece::Other, |idx| Piece::Param(idx, field_path)),
                    _ => Piece::Other,
                });
            }
            _ => pieces.push(Piece::Other),
        }
    }

    let mut pieces = Vec::new();
    push_pieces(expr, params, &mut pieces);
    let mut prefix = String::new();
    let mut suffix = String::new();
    let mut param = None;
    for piece in pieces {
        match (piece, &param) {
            (Piece::Text(text), None) => prefix.push_str(&text),
            (Piece::Text(text), Some(_)) => suffix.push_str(&text),
            (Piece::Param(param_idx, field_path), None) => param = Some((param_idx, field_path)),
            _ => return ForwardedArg::Other,
        }
    }
    match param {
        Some((param_idx, field_path)) => ForwardedArg::Param {
            param_idx,
            field_path,
            prefix,
            suffix,
        },
        None => ForwardedArg::Other,
    }
}

impl ForwardedArg {
    /// The role of the wrapper's parameter that supplies what `inner` reads
    /// from this argument.
    fn forward_role(&self, inner: &AnnotatedGmodCallArgRole) -> Option<AnnotatedGmodCallArgRole> {
        let Self::Param {
            param_idx,
            field_path,
            prefix,
            suffix,
        } = self
        else {
            return None;
        };
        if !inner.field_path.is_empty() && (!prefix.is_empty() || !suffix.is_empty()) {
            return None;
        }
        Some(AnnotatedGmodCallArgRole {
            param_idx: *param_idx,
            priority: inner.priority,
            field_path: field_path
                .iter()
                .chain(&inner.field_path)
                .cloned()
                .collect(),
            prefix: format!("{}{prefix}", inner.prefix),
            suffix: format!("{suffix}{}", inner.suffix),
        })
    }
}

/// The final written name of a value expression, for alias discovery.
fn expr_written_name(expr: &LuaExpr) -> Option<SmolStr> {
    match expr {
        LuaExpr::NameExpr(name_expr) => name_expr.get_name_text(),
        LuaExpr::IndexExpr(index_expr) => match index_expr.get_index_key()? {
            LuaIndexKey::Name(name) => Some(SmolStr::new(name.get_name_text())),
            LuaIndexKey::String(string) => Some(SmolStr::new(string.get_value())),
            _ => None,
        },
        _ => None,
    }
}

/// The names the shipped net operations are declared under (`Start`,
/// `Receive`, and any annotated wrapper of them).
///
/// Read straight out of the annotated call-role map the pre-analysis pass
/// already built, which is keyed by access path and already tags these calls
/// `NetStart`/`NetReceive`. The pre-pass records op *call sites* by path and so
/// cannot see `local recv = net.Receive`; carrying the op names lets the same
/// reference lookup and per-file binding closure that finds helper calls follow
/// that alias to its call sites.
pub(super) fn net_operation_names(
    annotated_roles: &AnnotatedGmodGlobalCallRoleMap,
) -> HashSet<SmolStr> {
    annotated_roles
        .roles_by_path
        .iter()
        .filter(|(_, roles)| {
            roles.system_roles.iter().any(|(kind, _)| {
                matches!(
                    kind,
                    GmodSystemCallKind::NetStart | GmodSystemCallKind::NetReceive
                )
            })
        })
        .map(|(path, _)| {
            SmolStr::new(
                path.rsplit_once('.')
                    .map_or(path.as_str(), |(_, last)| last),
            )
        })
        .collect()
}

/// The written name of a registry entry's declaration, used to decide which
/// call sites could reach it.
fn closure_declared_name(closure: &LuaClosureExpr) -> Option<SmolStr> {
    if let Some(func_stat) = closure.get_parent::<LuaFuncStat>()
        && let Some(func_name) = func_stat.get_func_name()
    {
        return var_expr_written_name(&func_name);
    }
    if let Some(local_func_stat) = closure.get_parent::<LuaLocalFuncStat>() {
        return local_func_stat
            .get_local_name()
            .and_then(|local_name| local_name.get_name_token())
            .map(|token| SmolStr::new(token.get_name_text()));
    }
    if let Some(assign_stat) = closure.get_parent::<LuaAssignStat>() {
        let (vars, value_exprs) = assign_stat.get_var_and_expr_list();
        let idx = value_exprs
            .iter()
            .position(|expr| expr.get_position() == closure.get_position())?;
        return var_expr_written_name(vars.get(idx)?);
    }
    if let Some(table_field) = closure.get_parent::<LuaTableField>() {
        return table_field_name(&table_field);
    }
    if let Some(local_stat) = closure.get_parent::<LuaLocalStat>() {
        let idx = local_stat
            .get_value_exprs()
            .position(|expr| expr.get_position() == closure.get_position())?;
        return local_stat
            .get_local_name_list()
            .nth(idx)
            .and_then(|local_name| local_name.get_name_token())
            .map(|token| SmolStr::new(token.get_name_text()));
    }
    None
}

/// The member `closure` is stored under when written as `X.name`, `X:name`
/// or a `name = closure` table field.
fn closure_member_name(closure: &LuaClosureExpr) -> Option<SmolStr> {
    let var_expr = if let Some(func_stat) = closure.get_parent::<LuaFuncStat>() {
        func_stat.get_func_name()?
    } else if let Some(table_field) = closure.get_parent::<LuaTableField>() {
        return table_field_name(&table_field);
    } else {
        let assign_stat = closure.get_parent::<LuaAssignStat>()?;
        let (vars, value_exprs) = assign_stat.get_var_and_expr_list();
        let idx = value_exprs
            .iter()
            .position(|expr| expr.get_position() == closure.get_position())?;
        vars.get(idx)?.clone()
    };
    match var_expr {
        LuaVarExpr::IndexExpr(_) => var_expr_written_name(&var_expr),
        LuaVarExpr::NameExpr(_) => None,
    }
}

fn table_field_name(table_field: &LuaTableField) -> Option<SmolStr> {
    match table_field.get_field_key()? {
        LuaIndexKey::Name(name) => Some(SmolStr::new(name.get_name_text())),
        LuaIndexKey::String(string) => Some(SmolStr::new(string.get_value())),
        _ => None,
    }
}

fn var_expr_written_name(var_expr: &LuaVarExpr) -> Option<SmolStr> {
    match var_expr {
        LuaVarExpr::NameExpr(name_expr) => name_expr.get_name_text(),
        LuaVarExpr::IndexExpr(index_expr) => match index_expr.get_index_key()? {
            LuaIndexKey::Name(name) => Some(SmolStr::new(name.get_name_text())),
            LuaIndexKey::String(string) => Some(SmolStr::new(string.get_value())),
            _ => None,
        },
    }
}

/// The names a call has to be written with for it to expand into a helper that
/// can reach a `net.Start`.
///
/// The helpers that can answer yes are a small fixed set, and resolution
/// matches declarations by written name, so a call written with a name no such
/// helper carries cannot expand into one.
///
/// Seeded from the net operations' own references, then grown outward: each
/// site is walked *up* to the function containing it, and that function's own
/// call sites come from the reference index, whose enclosing functions are the
/// next level. The set settles when a round adds no new site.
///
/// Nothing is scanned. The cost is proportional to how much net code the
/// workspace actually has, not to its size.
pub(super) fn net_producing_function_names(
    db: &DbIndex,
    op_names: &HashSet<SmolStr>,
) -> HashSet<SmolStr> {
    let reference_index = db.get_reference_index();
    let mut names: HashSet<SmolStr> = HashSet::default();
    let mut visited_decls: HashSet<LuaDeclId> = HashSet::default();
    // Seeded from the net operations' own references rather than from the
    // pre-pass's recorded call sites: that record is only written for files
    // that also need hook metadata, so it is not a complete list of net ops.
    // The reference index records every reference unconditionally.
    let mut expanded_names: HashSet<SmolStr> = op_names.clone();
    let mut pending_names: Vec<SmolStr> = op_names.iter().cloned().collect();
    let mut pending_decls: Vec<LuaDeclId> = Vec::new();

    while !pending_names.is_empty() || !pending_decls.is_empty() {
        // Every pending name and declaration is one level of the closure; the
        // set it settles on does not depend on the order they are expanded in.
        pending_names.sort_unstable();
        pending_decls
            .sort_unstable_by_key(|decl_id| (decl_id.file_id.id, u32::from(decl_id.position)));
        let mut sites: Vec<(FileId, Vec<LuaSyntaxId>)> = Vec::new();
        for name in pending_names.drain(..) {
            let mut files = reference_index
                .files_referencing_name(&name)
                .into_iter()
                .collect::<Vec<_>>();
            files.sort_unstable();
            sites.extend(files.into_iter().map(|file_id| {
                (
                    file_id,
                    reference_index.name_references_in_file(&name, file_id),
                )
            }));
        }
        sites.extend(
            pending_decls
                .drain(..)
                .map(|decl_id| (decl_id.file_id, decl_read_sites(db, decl_id))),
        );
        let expansions = super::parallel::map_items_recorded(&sites, |(file_id, sites)| {
            expand_sites(db, *file_id, sites)
        });

        for expansion in expansions {
            // A local enters neither name-keyed reference table, so a chain
            // through local wrappers only continues if the next level comes
            // from the declaration's own references.
            for decl_id in expansion.locals {
                if visited_decls.insert(decl_id) {
                    pending_decls.push(decl_id);
                }
            }
            // A newly named function's callers are the next level, and the
            // reference index already knows where they are.
            for name in expansion.names {
                names.insert(name.clone());
                if expanded_names.insert(name.clone()) {
                    pending_names.push(name);
                }
            }
        }
    }

    names
}

/// One file's contribution for a set of reference sites: the written name of
/// each site's innermost named enclosing closure, past the callbacks it
/// defines, and that closure's local binding when it has one.
///
fn expand_sites(db: &DbIndex, file_id: FileId, sites: &[LuaSyntaxId]) -> NetNameExpansion {
    let mut expansion = NetNameExpansion::default();
    for syntax_id in sites {
        // The site's closure and the statement naming it lie within the
        // top-level statement around the site.
        let Some(root) = db
            .get_vfs()
            .syntax_tree_around(&file_id, syntax_id.get_range())
            .map(|tree| tree.get_red_root())
        else {
            continue;
        };
        let Some(node) = syntax_id.to_node_from_root(&root) else {
            continue;
        };
        let Some((closure, name)) = node
            .ancestors()
            .filter_map(LuaClosureExpr::cast)
            .find_map(|closure| closure_declared_name(&closure).map(|name| (closure, name)))
        else {
            continue;
        };
        if let Some(decl_id) = closure_local_decl_id(file_id, &closure) {
            expansion.locals.push(decl_id);
        }
        expansion.names.push(name);
    }
    expansion
}

/// Every place a local declaration is read, from the reference index.
fn decl_read_sites(db: &DbIndex, decl_id: LuaDeclId) -> Vec<LuaSyntaxId> {
    let Some(references) = db.get_reference_index().decl_references(&decl_id) else {
        return Vec::new();
    };
    references
        .cells
        .iter()
        .filter(|cell| !cell.is_write)
        .map(|cell| LuaSyntaxId::new(glua_parser::LuaSyntaxKind::NameExpr.into(), cell.range))
        .collect()
}

/// The declaration a closure is bound to, when that binding is a local.
///
/// A declaration is identified by the position of its declared name, so the two
/// local binding forms yield it without a lookup.
fn closure_local_decl_id(file_id: FileId, closure: &LuaClosureExpr) -> Option<LuaDeclId> {
    if let Some(local_func_stat) = closure.get_parent::<LuaLocalFuncStat>() {
        return Some(LuaDeclId::new(
            file_id,
            local_func_stat.get_local_name()?.get_position(),
        ));
    }
    let local_stat = closure.get_parent::<LuaLocalStat>()?;
    let idx = local_stat
        .get_value_exprs()
        .position(|expr| expr.get_position() == closure.get_position())?;
    Some(LuaDeclId::new(
        file_id,
        local_stat.get_local_name_list().nth(idx)?.get_position(),
    ))
}

/// Per-file function definition lookup. Built once and reused for all
/// helper-resolution queries against the same file's syntax tree.
struct FileFunctionMap {
    /// Function bodies: `function f() end`, `local function f() end`,
    /// `local f = function() end`, `f = function() end`.
    bare: HashMap<String, LuaBlock>,
    /// All top-level function-defining blocks in source order, including
    /// duplicates and unnamed closures. Lets callers that need to scan every
    /// function body in the file skip running 4 separate `descendants` walks.
    all_blocks: Vec<LuaBlock>,
}

impl FileFunctionMap {
    fn build(root: &LuaChunk) -> Self {
        let mut scan = FileTreeScan::default();
        for node in root.syntax().descendants() {
            scan.visit(&node);
        }
        scan.finish_map()
    }
}

/// One-pass per-file tree scan backing [`FileFunctionMap::build`].
#[derive(Default)]
struct FileTreeScan {
    bare: HashMap<String, LuaBlock>,
    duplicate_bare: HashSet<String>,
    all_blocks: Vec<LuaBlock>,
}

impl FileTreeScan {
    fn visit(&mut self, node: &LuaSyntaxNode) {
        if let Some(local_func_stat) = LuaLocalFuncStat::cast(node.clone()) {
            if let Some(block) = local_func_stat.get_closure().and_then(|c| c.get_block()) {
                if let Some(local_name) = local_func_stat
                    .get_local_name()
                    .and_then(|n| n.get_name_token())
                {
                    let name = local_name.get_name_text().to_string();
                    if self.bare.insert(name.clone(), block.clone()).is_some() {
                        self.duplicate_bare.insert(name);
                    }
                }
                self.all_blocks.push(block);
            }
            return;
        }
        if let Some(local_stat) = LuaLocalStat::cast(node.clone()) {
            let names: Vec<_> = local_stat.get_local_name_list().collect();
            let values: Vec<_> = local_stat.get_value_exprs().collect();
            for (idx, value) in values.iter().enumerate() {
                let LuaExpr::ClosureExpr(closure) = value else {
                    continue;
                };
                let Some(block) = closure.get_block() else {
                    continue;
                };
                if let Some(name_token) = names.get(idx).and_then(|n| n.get_name_token()) {
                    let name = name_token.get_name_text().to_string();
                    if self.bare.insert(name.clone(), block.clone()).is_some() {
                        self.duplicate_bare.insert(name);
                    }
                }
                self.all_blocks.push(block);
            }
            return;
        }
        if let Some(func_stat) = LuaFuncStat::cast(node.clone()) {
            let Some(block) = func_stat.get_closure().and_then(|c| c.get_block()) else {
                return;
            };
            match func_stat.get_func_name() {
                Some(LuaVarExpr::NameExpr(name_expr)) => {
                    if let Some(name) = name_expr.get_name_text().map(String::from) {
                        if self.bare.insert(name.clone(), block.clone()).is_some() {
                            self.duplicate_bare.insert(name);
                        }
                    }
                }
                Some(LuaVarExpr::IndexExpr(_)) => {}
                None => {}
            }
            self.all_blocks.push(block);
            return;
        }
        if let Some(assign_stat) = LuaAssignStat::cast(node.clone()) {
            let (vars, values) = assign_stat.get_var_and_expr_list();
            for (idx, value) in values.iter().enumerate() {
                let LuaExpr::ClosureExpr(closure) = value else {
                    continue;
                };
                let Some(block) = closure.get_block() else {
                    continue;
                };
                if let Some(var) = vars.get(idx) {
                    match var {
                        LuaVarExpr::NameExpr(name_expr) => {
                            if let Some(name) = name_expr.get_name_text().map(String::from) {
                                if self.bare.insert(name.clone(), block.clone()).is_some() {
                                    self.duplicate_bare.insert(name);
                                }
                            }
                        }
                        LuaVarExpr::IndexExpr(_) => {}
                    }
                }
                self.all_blocks.push(block);
            }
        }
        if let Some(table_field) = LuaTableField::cast(node.clone()) {
            let Some(LuaExpr::ClosureExpr(closure)) = table_field.get_value_expr() else {
                return;
            };
            let Some(block) = closure.get_block() else {
                return;
            };
            self.all_blocks.push(block);
        }
    }

    fn finish_map(mut self) -> FileFunctionMap {
        for name in self.duplicate_bare {
            self.bare.remove(&name);
        }
        FileFunctionMap {
            bare: self.bare,
            all_blocks: self.all_blocks,
        }
    }
}

/// All per-file gmod pre-analysis metadata collected off-thread for one file.
/// Produced by [`collect_file_gmod_metadata`] (read-only against `&DbIndex`) and
/// merged into the db by [`apply_pre`].
pub(crate) struct GmodFileMetadataResult {
    /// `Some` when hook metadata was collected (file had hook-relevant
    /// keywords): (hook sites, system metadata, gm-method realm annotations).
    /// `None` means the hook walk was skipped for this file.
    hook_metadata: Option<(
        Vec<GmodHookSiteMetadata>,
        GmodSystemFileMetadata,
        Vec<(String, GmodRealm)>,
    )>,
    /// `---@realm` member ranges (only populated when `keywords.has_realm_anno`).
    member_ranges: Vec<GmodRealmRange>,
    /// Branch realm ranges (only populated when `keywords.has_realm_branch`).
    branch_ranges: Vec<GmodRealmRange>,
    /// File-level realm annotation (only when `keywords.has_realm_anno`).
    annotation_realm: Option<GmodRealm>,
    /// `@fileparam` annotations, when the file content mentions `@fileparam`.
    file_params: Option<Vec<(String, String)>>,
    /// What the file's scripted-class scope binds, read off its syntax.
    scope: Option<ScopedFilePlan>,
    /// The local tables the file publishes into `_G` under a scripted-class global.
    published_bases: Vec<(LuaDeclId, String)>,
}

/// The syntax-derived inputs of binding a scripted-scope file's class.
struct ScopedFilePlan {
    /// The scope the file's cached class info, else its path, names.
    scope_match: Option<GmodScopedClassMatch>,
    /// The `<global>.Base` and `ENT.Type` assignments, in source order.
    base_assignments: Vec<ScopedBaseAssignment>,
    /// The network-var wrappers the file defines, when its class info is cached.
    wrappers: Vec<NetworkVarWrapper>,
}

enum ScopedBaseAssignment {
    Base {
        name: String,
        range: TextRange,
        syntax_id: LuaSyntaxId,
    },
    EntityType {
        super_name: &'static str,
        range: TextRange,
    },
}

/// Collect all per-file gmod pre-analysis metadata for `file_id`. Read-only
/// against `&DbIndex`: reads the file's own AST (rebuilt locally from the Send
/// green tree) plus pre-existing immutable index state, so this is safe to run
/// concurrently across files. The returned [`GmodFileMetadataResult`] is merged
/// into the db sequentially by the caller.
fn collect_file_gmod_metadata(
    db: &DbIndex,
    file_id: FileId,
    formatted_hook_prefixes: &[String],
    annotated_global_call_roles: &AnnotatedGmodGlobalCallRoleMap,
) -> GmodFileMetadataResult {
    let content = db.get_vfs().get_file_content(&file_id);
    let keywords = content
        .map(|content| {
            scan_gmod_keywords(
                db,
                file_id,
                content,
                formatted_hook_prefixes,
                annotated_global_call_roles,
            )
        })
        .unwrap_or_default();

    // Rebuild the red tree locally from the (Send) green tree so no non-Send
    // rowan node crosses the thread boundary.
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_chunk_node())
    else {
        return GmodFileMetadataResult {
            hook_metadata: None,
            member_ranges: Vec::new(),
            branch_ranges: Vec::new(),
            annotation_realm: None,
            file_params: None,
            scope: None,
            published_bases: Vec::new(),
        };
    };

    // Network flows are collected by the network derivation, so a file that
    // needs no hook metadata skips the walk whole.
    let hook_metadata = keywords
        .needs_hook_metadata()
        .then(|| collect_hook_metadata(db, file_id, &root, annotated_global_call_roles));

    let branch_ranges = if keywords.has_realm_branch {
        collect_branch_realm_ranges(&root)
    } else {
        Vec::new()
    };

    let (annotation_realm, member_ranges) = if keywords.has_realm_anno {
        (
            collect_realm_annotation(&root),
            collect_member_realm_ranges(&root),
        )
    } else {
        (None, Vec::new())
    };

    // @fileparam is extremely rare; only scan if file content contains it.
    let file_params = if content.is_some_and(|c| c.contains("@fileparam")) {
        Some(collect_file_params(&root))
    } else {
        None
    };

    GmodFileMetadataResult {
        hook_metadata,
        member_ranges,
        branch_ranges,
        annotation_realm,
        file_params,
        scope: None,
        published_bases: collect_published_scope_bases(db, file_id, &root),
    }
}

/// The local tables `root` publishes into `_G` under a scripted-class global, with that
/// global: `_G["PLUGIN"] = PLUGIN`, or a loader's `_G[variable] = PLUGIN` whose `variable`
/// defaults to `"PLUGIN"`. Such a table is what every class of that scope starts from.
fn collect_published_scope_bases(
    db: &DbIndex,
    file_id: FileId,
    root: &LuaChunk,
) -> Vec<(LuaDeclId, String)> {
    let Some(references) = db.get_reference_index().get_local_reference(&file_id) else {
        return Vec::new();
    };
    let definitions = db
        .get_file_emmyrc(file_id)
        .gmod
        .scripted_class_scopes
        .resolved_definitions_slice();
    let mut bases = Vec::new();
    for assign in root.descendants::<LuaAssignStat>() {
        let (vars, values) = assign.get_var_and_expr_list();
        for (var, value) in vars.iter().zip(values.iter()) {
            let LuaVarExpr::IndexExpr(index_expr) = var else {
                continue;
            };
            let Some(LuaExpr::NameExpr(prefix)) = index_expr.get_prefix_expr() else {
                continue;
            };
            if !matches!(prefix.get_name_text().as_deref(), Some("_G" | "_ENV"))
                || name_expr_resolves_to_local(db, file_id, &prefix)
            {
                continue;
            }
            let keys = match index_expr.get_index_key() {
                Some(LuaIndexKey::String(key)) => vec![key.get_value()],
                Some(LuaIndexKey::Expr(LuaExpr::NameExpr(key))) => {
                    literal_string_values(db, file_id, root, &key, assign.get_position())
                }
                _ => Vec::new(),
            };
            let globals = keys
                .into_iter()
                .filter(|key| {
                    definitions
                        .iter()
                        .any(|definition| definition.class_global == *key)
                })
                .collect::<Vec<_>>();
            if globals.is_empty() {
                continue;
            }
            let LuaExpr::NameExpr(table_name) = value else {
                continue;
            };
            let Some(decl) = references
                .get_decl_id(&table_name.get_range())
                .and_then(|decl_id| db.get_decl_index().get_decl(&decl_id))
            else {
                continue;
            };
            let initializes_table = matches!(decl.extra, LuaDeclExtra::Local { .. })
                && decl
                    .get_value_syntax_id()
                    .and_then(|syntax_id| syntax_id.to_node_from_root(root.syntax()))
                    .and_then(LuaExpr::cast)
                    .is_some_and(|value| matches!(value, LuaExpr::TableExpr(_)));
            if initializes_table {
                bases.extend(globals.into_iter().map(|global| (decl.get_id(), global)));
            }
        }
    }
    bases
}

/// The strings `key` may hold before `before`, sorted: what its initializer and earlier
/// reassignments give it as literals, or as the default of `name or "literal"`.
fn literal_string_values(
    db: &DbIndex,
    file_id: FileId,
    root: &LuaChunk,
    key: &LuaNameExpr,
    before: TextSize,
) -> Vec<String> {
    let Some(references) = db.get_reference_index().get_local_reference(&file_id) else {
        return Vec::new();
    };
    let Some(decl) = references
        .get_decl_id(&key.get_range())
        .and_then(|decl_id| db.get_decl_index().get_decl(&decl_id))
    else {
        return Vec::new();
    };
    let decl_id = decl.get_id();
    let initializer = decl
        .get_value_syntax_id()
        .and_then(|syntax_id| syntax_id.to_node_from_root(root.syntax()))
        .and_then(LuaExpr::cast);
    let reassigned = root
        .descendants::<LuaAssignStat>()
        .filter(|assign| assign.get_position() < before)
        .flat_map(|assign| {
            let (vars, values) = assign.get_var_and_expr_list();
            vars.into_iter()
                .zip(values)
                .filter(|(var, _)| {
                    matches!(var, LuaVarExpr::NameExpr(name)
                        if references.get_decl_id(&name.get_range()) == Some(decl_id))
                })
                .map(|(_, value)| value)
                .collect::<Vec<_>>()
        });
    let mut strings = initializer
        .into_iter()
        .chain(reassigned)
        .filter_map(|value| {
            let literal = match value {
                LuaExpr::BinaryExpr(binary)
                    if binary
                        .get_op_token()
                        .is_some_and(|token| token.get_op() == BinaryOperator::OpOr) =>
                {
                    binary.get_exprs()?.1
                }
                value => value,
            };
            match literal {
                LuaExpr::LiteralExpr(literal) => match literal.get_literal()? {
                    LuaLiteralToken::String(string) => Some(string.get_value()),
                    _ => None,
                },
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    strings.sort();
    strings.dedup();
    strings
}

/// The scope inputs of `file_id` when it is a scripted-scope file.
fn collect_scoped_file_plan(
    db: &DbIndex,
    file_id: FileId,
    scripted_scope_files: &HashSet<FileId>,
) -> Option<ScopedFilePlan> {
    if !scripted_scope_files.contains(&file_id) {
        return None;
    }
    let root = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_chunk_node())?;
    let cached = db.get_gmod_infer_index().get_scoped_class_info(&file_id);
    let wrappers = cached
        .map(|info| collect_network_var_wrappers(&root, &info.global_name))
        .unwrap_or_default();
    let scope_match = cached
        .map(|info| GmodScopedClassMatch {
            class_name: info.class_name.clone(),
            global_name: info.global_name.clone(),
            is_global_singleton: info.is_global_singleton,
            aliases: info.aliases.clone(),
            super_types: info.super_types.clone(),
            class_name_prefix: info.class_name_prefix.clone(),
        })
        .or_else(|| detect_scoped_class_from_path(db, file_id));
    let base_assignments = scope_match
        .as_ref()
        .map(|scope_match| collect_scoped_base_assignments(&root, &scope_match.global_name))
        .unwrap_or_default();
    Some(ScopedFilePlan {
        scope_match,
        base_assignments,
        wrappers,
    })
}

fn collect_hook_metadata(
    db: &DbIndex,
    file_id: FileId,
    root: &LuaChunk,
    annotated_global_call_roles: &AnnotatedGmodGlobalCallRoleMap,
) -> (
    Vec<GmodHookSiteMetadata>,
    GmodSystemFileMetadata,
    Vec<(String, GmodRealm)>,
) {
    let mut hook_sites = Vec::new();
    let mut system_metadata = GmodSystemFileMetadata::default();
    let mut gm_method_realms = Vec::new();
    let annotated_call_roles =
        AnnotatedGmodCallRoleMap::build(db, file_id, root, annotated_global_call_roles);

    for node in root.syntax().descendants() {
        if let Some(call_expr) = LuaCallExpr::cast(node.clone()) {
            if let Some(site) =
                collect_hook_call_site(db, file_id, &annotated_call_roles, call_expr.clone())
            {
                hook_sites.push(site);
            }
            collect_system_call_metadata_into(
                db,
                file_id,
                &annotated_call_roles,
                call_expr,
                &mut system_metadata,
            );
            continue;
        }

        if let Some(func_stat) = LuaFuncStat::cast(node) {
            if let Some(site) = collect_hook_method_site(db, file_id, func_stat.clone()) {
                hook_sites.push(site);
            }

            if let Some((method_name, realm)) = collect_gm_method_realm_annotation(&func_stat)
                && !gm_method_realms
                    .iter()
                    .any(|(existing_name, existing_realm)| {
                        existing_name == &method_name && *existing_realm == realm
                    })
            {
                gm_method_realms.push((method_name, realm));
            }
        }
    }

    (hook_sites, system_metadata, gm_method_realms)
}

fn collect_gm_method_realm_annotation(func_stat: &LuaFuncStat) -> Option<(String, GmodRealm)> {
    let LuaVarExpr::IndexExpr(function_name_expr) = func_stat.get_func_name()? else {
        return None;
    };
    let LuaExpr::NameExpr(function_prefix_name) = function_name_expr.get_prefix_expr()? else {
        return None;
    };
    let function_prefix_text = function_prefix_name.get_name_text()?;
    if !matches!(function_prefix_text.as_str(), "GM" | "GAMEMODE") {
        return None;
    }
    let LuaIndexKey::Name(function_method_name) = function_name_expr.get_index_key()? else {
        return None;
    };
    let comment = func_stat.get_left_comment()?;
    let realm = realm_from_doc_comment(&comment)?;
    let method_name = function_method_name.get_name_text().to_string();
    Some((method_name, realm))
}

fn is_call_expr_in_dynamic_control_flow(block: &LuaBlock, call_expr: &LuaCallExpr) -> bool {
    let mut child = call_expr.syntax().clone();
    for node in call_expr
        .syntax()
        .ancestors()
        .skip(1)
        .take_while(|node| node != block.syntax())
    {
        let kind = node.kind().into();
        if LuaIfStat::can_cast(kind)
            || LuaWhileStat::can_cast(kind)
            || LuaForStat::can_cast(kind)
            || LuaForRangeStat::can_cast(kind)
            || LuaRepeatStat::can_cast(kind)
            || short_circuit_guard(&node, &child).is_some()
        {
            return true;
        }
        child = node;
    }
    false
}

/// The `and`/`or` expression `node` is when `child` is its right operand,
/// which runs only when the left operand lets it, as an `if` body does.
fn short_circuit_guard(node: &LuaSyntaxNode, child: &LuaSyntaxNode) -> Option<LuaBinaryExpr> {
    let binary_expr = LuaBinaryExpr::cast(node.clone())?;
    let op = binary_expr.get_op_token()?.get_op();
    if !matches!(op, BinaryOperator::OpAnd | BinaryOperator::OpOr) {
        return None;
    }
    let (_, right) = binary_expr.get_exprs()?;
    (right.syntax() == child).then_some(binary_expr)
}

/// Walks ancestors from `call_expr` up to (but not including) `block`,
/// collecting one `NetFlowFrame` per enclosing if/while/for/repeat. Frames
/// are returned outer-to-inner so the renderer can nest them naturally.
///
/// `if`/`elseif`/`else` are folded into a single frame per if-chain branch:
/// when the op lives inside an `elseif cond then ... end` clause, that frame
/// records `elseif cond then` (instead of the outer `if cond then`) so the
/// developer sees the actual branch the op is gated by. Same for `else`. The
/// frame's id is the clause's source range so two ops in different branches
/// of the same if are distinct frames (different patterns can result).
///
/// The header text is a single-line trimmed summary of the statement opener
/// (e.g. `if cond then`, `for i = 1, #items do`). Multi-line headers and
/// excessively long ones are stored as `None` to keep hover popups compact.
fn extract_flow_path(
    file_id: FileId,
    block: &LuaBlock,
    call_expr: &LuaCallExpr,
) -> Vec<NetFlowFrame> {
    let mut frames: Vec<NetFlowFrame> = Vec::new();
    // When set, the next ancestor (which we know is the parent LuaIfStat of
    // an elseif/else clause we just captured) should be skipped so we don't
    // double-count the if-chain.
    let mut skip_parent_if = false;
    let mut child = call_expr.syntax().clone();
    for node in call_expr
        .syntax()
        .ancestors()
        .take_while(|node| node != block.syntax())
    {
        let kind = node.kind().into();
        let guard = short_circuit_guard(&node, &child);
        child = node.clone();

        if let Some(binary_expr) = guard {
            frames.push(NetFlowFrame {
                kind: NetFlowKind::If,
                header: extract_short_circuit_header(&binary_expr),
                id: u32::from(node.text_range().start()),
                file_id,
            });
            continue;
        }

        if skip_parent_if && LuaIfStat::can_cast(kind) {
            skip_parent_if = false;
            continue;
        }

        if LuaElseIfClauseStat::can_cast(kind) {
            let header = extract_branch_header(&node, BranchKind::ElseIf);
            frames.push(NetFlowFrame {
                kind: NetFlowKind::If,
                header,
                id: u32::from(node.text_range().start()),
                file_id,
            });
            skip_parent_if = true;
            continue;
        }
        if LuaElseClauseStat::can_cast(kind) {
            frames.push(NetFlowFrame {
                kind: NetFlowKind::If,
                header: Some("else".to_string()),
                id: u32::from(node.text_range().start()),
                file_id,
            });
            skip_parent_if = true;
            continue;
        }

        let flow_kind = if LuaIfStat::can_cast(kind) {
            NetFlowKind::If
        } else if LuaWhileStat::can_cast(kind) {
            NetFlowKind::While
        } else if LuaForStat::can_cast(kind) {
            NetFlowKind::For
        } else if LuaForRangeStat::can_cast(kind) {
            NetFlowKind::ForRange
        } else if LuaRepeatStat::can_cast(kind) {
            NetFlowKind::Repeat
        } else {
            continue;
        };
        let header = extract_flow_header(&node, flow_kind);
        let id: u32 = u32::from(node.text_range().start());
        frames.push(NetFlowFrame {
            kind: flow_kind,
            header,
            id,
            file_id,
        });
    }
    // ancestors() yields inner-to-outer; flip so outer is first.
    frames.reverse();
    frames
}

#[derive(Clone, Copy)]
enum BranchKind {
    ElseIf,
}

/// The node's text up to its first newline.
///
/// `node.text().to_string()` walks and concatenates every token underneath, so
/// asking an `if` statement for its header used to materialise the statement's
/// whole body — thousands of lines for a large branch — to read one line of it.
/// Network flow analysis does that for every branch around every `net` call in
/// every re-analysed file, which made it one of the more expensive things a
/// keystroke paid for.
fn first_line_text(node: &LuaSyntaxNode) -> String {
    let mut line = String::new();
    for token in node
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
    {
        let text = token.text();
        match text.find('\n') {
            Some(end) => {
                line.push_str(&text[..end]);
                break;
            }
            None => line.push_str(text),
        }
    }
    line
}

/// The guard of a short-circuited operand as a header, `cond and` or
/// `cond or`, when its left operand fits on one short line.
fn extract_short_circuit_header(binary_expr: &LuaBinaryExpr) -> Option<String> {
    const MAX_HEADER_LEN: usize = 80;
    let (left, _) = binary_expr.get_exprs()?;
    let op = match binary_expr.get_op_token()?.get_op() {
        BinaryOperator::OpAnd => "and",
        _ => "or",
    };
    let left_text = left.syntax().text().to_string();
    if left_text.contains('\n') {
        return None;
    }
    let header = format!(
        "{} {op}",
        left_text.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    (header.len() <= MAX_HEADER_LEN).then_some(header)
}

/// Pulls the header text for an `elseif cond then` clause from source.
fn extract_branch_header(node: &LuaSyntaxNode, kind: BranchKind) -> Option<String> {
    const MAX_HEADER_LEN: usize = 80;
    let full = first_line_text(node);
    let trimmed = full.trim_start();
    let nl_idx = trimmed.find('\n').unwrap_or(trimmed.len());
    let first_line = &trimmed[..nl_idx];
    let bytes = first_line.as_bytes();
    let _ = kind;
    // Locate the standalone `then` keyword and slice through it.
    let mut from = 0usize;
    let term_end = loop {
        let rel = first_line[from..].find("then")?;
        let abs = from + rel;
        let end = abs + 4;
        let left_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric();
        let right_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if left_ok && right_ok {
            break end;
        }
        from = end;
    };
    let collapsed: String = first_line[..term_end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() || collapsed.len() > MAX_HEADER_LEN {
        return None;
    }
    Some(collapsed)
}

/// Pulls a compact single-line summary of a control-flow statement's opener
/// straight from the source — e.g. `if foo > 0 then`, `for i = 1, n do`.
/// Returns `None` for multi-line or oversized headers; the renderer falls
/// back to a generic label in that case.
fn extract_flow_header(stat_node: &LuaSyntaxNode, kind: NetFlowKind) -> Option<String> {
    const MAX_HEADER_LEN: usize = 80;
    // Only the opener is ever read, and it bails on a multi-line one, so there
    // is no reason to materialise the statement's whole body first.
    let full = first_line_text(stat_node);
    let header_raw = match kind {
        NetFlowKind::Repeat => {
            // `repeat` itself has no condition until `until` at the end.
            "repeat".to_string()
        }
        _ => {
            // Take from start through the first `then` or `do`, whichever
            // marks the opener's end. Everything after is the body.
            let terminator = match kind {
                NetFlowKind::If => "then",
                _ => "do",
            };
            let trimmed = full.trim_start();
            // Find terminator on the first line containing it. If the opener
            // breaks across lines (e.g. condition split over multiple lines)
            // we bail to keep the hover compact.
            let nl_idx = trimmed.find('\n').unwrap_or(trimmed.len());
            let first_line_slice = &trimmed[..nl_idx];
            // Terminator must be a standalone keyword: preceded by whitespace
            // (not mid-identifier) or appear at start of line, and bounded by
            // a non-alphanumeric char on the right (or be at end-of-line).
            let bytes = first_line_slice.as_bytes();
            let mut search_from = 0usize;
            let term_end = loop {
                let rel = first_line_slice[search_from..].find(terminator)?;
                let abs = search_from + rel;
                let end = abs + terminator.len();
                let left_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric();
                let right_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
                if left_ok && right_ok {
                    break end;
                }
                search_from = end;
            };
            first_line_slice[..term_end].to_string()
        }
    };
    let collapsed: String = header_raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() || collapsed.len() > MAX_HEADER_LEN {
        return None;
    }
    Some(collapsed)
}

fn is_call_expr_in_nested_closure(block: &LuaBlock, call_expr: &LuaCallExpr) -> bool {
    call_expr
        .syntax()
        .ancestors()
        .take_while(|node| node != block.syntax())
        .any(|node| LuaClosureExpr::can_cast(node.kind().into()))
}

fn call_expr_from_stat(stat: &LuaStat) -> Option<LuaCallExpr> {
    let LuaStat::CallExprStat(call_expr_stat) = stat else {
        return None;
    };

    call_expr_stat.get_call_expr()
}

/// Source text of the called function, used for display in diagnostics, hover
/// and code lens. Prefers what the developer actually wrote so an aliased or
/// wrapped call reports its own name.
///
/// Computed at flow-construction time only (never speculatively per candidate),
/// in a single pass that drops whitespace without the intermediate `String` +
/// `Vec<&str>` + join of the old implementation.
fn call_display_name(call_expr: &LuaCallExpr) -> SmolStr {
    call_expr
        .get_prefix_expr()
        .map(|prefix| {
            let text = prefix.syntax().text().to_string();
            if !text.bytes().any(|b| b.is_ascii_whitespace()) {
                return SmolStr::new(text);
            }
            SmolStr::new(
                text.chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>(),
            )
        })
        .unwrap_or_else(|| SmolStr::new_static("<net call>"))
}

/// Captures a short snippet of the value-arg source text for a write op so
/// hover can display *what* is being written (e.g. `net.WriteString("hi")`
/// instead of just `net.WriteString`). Returns `None` for read ops, when the
/// arg is missing, when it spans multiple lines, or when it's too long to
/// render inline — robustness over completeness; we'd rather show the bare
/// op name than blow up the hover popup with a 200-char expression.
fn extract_write_value_text(call_expr: &LuaCallExpr, op: &NetOpDescriptor) -> Option<String> {
    if !op.is_write() {
        return None;
    }

    const MAX_INLINE_LEN: usize = 40;

    let arg_expr = call_expr.get_args_list()?.get_args().next()?;
    let raw = arg_expr.syntax().text().to_string();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains('\n') || trimmed.contains('\r') {
        return None;
    }
    if trimmed.len() > MAX_INLINE_LEN {
        return None;
    }
    Some(trimmed.to_string())
}

/// Extracts the static bit-width literal from a payload op that declares a
/// `gmod.net_payload`/`bits` parameter. Returns `None` for ops with no such
/// parameter, or when the argument is not an integer literal (variable,
/// expression, runtime computation) — anything else is unknowable at index time
/// and would produce false-positive mismatches if compared.
fn extract_bit_width_arg(call_expr: &LuaCallExpr, bits_arg_idx: usize) -> Option<u32> {
    let arg_expr = call_expr.get_args_list()?.get_args().nth(bits_arg_idx)?;
    let LuaExpr::LiteralExpr(literal_expr) = arg_expr else {
        return None;
    };
    let LuaLiteralToken::Number(number_token) = literal_expr.get_literal()? else {
        return None;
    };

    let value = match number_token.get_number_value() {
        NumberResult::Int(v) if v > 0 => v as u64,
        NumberResult::Uint(v) if v > 0 => v,
        _ => return None,
    };

    if value > u32::MAX as u64 {
        return None;
    }
    Some(value as u32)
}

/// The message names argument `arg_idx` can hold: a literal, or literals
/// chosen with `and`/`or`. Empty when it can hold a name that is not static.
fn extract_static_string_arg_values(call_expr: &LuaCallExpr, arg_idx: usize) -> Vec<String> {
    call_expr
        .get_args_list()
        .and_then(|args| args.get_args().nth(arg_idx))
        .and_then(|arg| static_string_values(&arg))
        .unwrap_or_default()
}

/// The strings `expr` can evaluate to, `None` when one of them is not static.
/// `a and b` yields a string only through `b`, since a string is never falsy;
/// `a or b` yields either side.
fn static_string_values(expr: &LuaExpr) -> Option<Vec<String>> {
    match expr {
        LuaExpr::LiteralExpr(literal_expr) => match literal_expr.get_literal()? {
            LuaLiteralToken::String(string) => Some(vec![string.get_value()]),
            LuaLiteralToken::Nil(_) | LuaLiteralToken::Bool(_) | LuaLiteralToken::Number(_) => {
                Some(Vec::new())
            }
            _ => None,
        },
        LuaExpr::ParenExpr(paren_expr) => static_string_values(&paren_expr.get_expr()?),
        LuaExpr::BinaryExpr(binary_expr) => {
            let (left, right) = binary_expr.get_exprs()?;
            match binary_expr.get_op_token()?.get_op() {
                BinaryOperator::OpAnd => static_string_values(&right),
                BinaryOperator::OpOr => {
                    let mut values = static_string_values(&left)?;
                    for value in static_string_values(&right)? {
                        if !values.contains(&value) {
                            values.push(value);
                        }
                    }
                    Some(values)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Cheap syntactic gate for the flow collectors, which run over every
/// statement-level call in a candidate file. A tracked flow always names its
/// message with string literals, so a call carrying none can never start or
/// receive one, and is rejected here before the far more expensive signature
/// resolution runs.
///
/// Deliberately index-agnostic: the message parameter's position comes from the
/// annotation and is not fixed at zero. Ordering only — every call that would
/// have produced a flow still reaches the resolver.
fn call_has_literal_string_arg(call_expr: &LuaCallExpr) -> bool {
    let Some(args_list) = call_expr.get_args_list() else {
        return false;
    };
    args_list
        .get_args()
        .any(|arg| static_string_values(&arg).is_some_and(|values| !values.is_empty()))
}

/// Captures the recipient argument of a send terminator as a single-line snippet
/// for display in code lens. The argument position comes from the
/// `gmod.net_payload`/`target` call-arg role, so terminators with no recipient
/// (`net.Broadcast`, `net.SendToServer`) yield `None` without a name check.
/// Returns `None` when the source is multi-line or too long to render inline.
fn extract_send_target_text(call_expr: &LuaCallExpr, send_kind: NetSendKind) -> Option<String> {
    const MAX_INLINE_LEN: usize = 40;

    let target_arg_idx = send_kind.target_arg_idx?;
    let arg_expr = call_expr.get_args_list()?.get_args().nth(target_arg_idx)?;
    let raw = arg_expr.syntax().text().to_string();
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains('\n') || trimmed.contains('\r') {
        return None;
    }
    if trimmed.len() > MAX_INLINE_LEN {
        return None;
    }
    Some(trimmed.to_string())
}

#[derive(Debug, Clone)]
pub(crate) struct GmodScopedClassMatch {
    pub global_name: String,
    pub class_name: String,
    pub is_global_singleton: bool,
    pub aliases: Vec<String>,
    pub super_types: Vec<String>,
    /// The scope's `classNamePrefix` (if any). Used to derive the stripped
    /// short name for parent-alias synthesis (e.g. `gamemode_sandbox` →
    /// `sandbox` → `Sandbox`).
    pub class_name_prefix: Option<String>,
}

fn collect_scripted_scope_type_bindings_with(
    db: &mut DbIndex,
    file_id: FileId,
    scope_match: &GmodScopedClassMatch,
) {
    let mut decls = Vec::new();
    {
        let Some(decl_tree) = db.get_decl_index().get_decl_tree(&file_id) else {
            return;
        };

        for decl in decl_tree.get_decls().values() {
            if decl.get_name() != scope_match.global_name
                && !scope_match
                    .aliases
                    .iter()
                    .any(|alias| alias == decl.get_name())
            {
                continue;
            }

            let is_scoped_local =
                decl.is_local() && scoped_class_authored_as_local(&scope_match.global_name);
            if is_scoped_local || decl.is_global() {
                decls.push((decl.get_id(), decl.get_range()));
            }
        }
    }

    if decls.is_empty() {
        return;
    }
    // The class is anchored on the first declaration, so which one that is
    // must not depend on the order the decl map happens to iterate in.
    decls.sort_by_key(|(_, range)| (range.start(), range.end()));

    let class_decl_id = ensure_scoped_class_type_decl(
        db,
        file_id,
        &scope_match.class_name,
        &scope_match.global_name,
        &scope_match.super_types,
        decls[0].1,
    );

    for (decl_id, _) in decls {
        let previous_decl_type = db
            .get_type_index()
            .get_type_cache(&decl_id.into())
            .map(|type_cache| type_cache.as_type().clone());

        write_type_cache(
            db,
            decl_id.into(),
            LuaTypeCache::InferType(LuaType::Def(class_decl_id.clone())),
            TypeCacheWriteMode::ForceOverwrite,
        );

        if let Some(LuaType::TableConst(table_range)) = previous_decl_type {
            let table_member_owner = LuaMemberOwner::Element(table_range);
            let class_member_owner = LuaMemberOwner::Type(class_decl_id.clone());
            let table_member_ids = db
                .get_member_index()
                .get_members(&table_member_owner)
                .map(|members| {
                    members
                        .iter()
                        .map(|member| member.get_id())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let mut table_member_ids = table_member_ids;
            table_member_ids
                .sort_unstable_by_key(|member_id| (member_id.file_id.id, member_id.get_position()));
            for member_id in table_member_ids {
                db.get_member_index_mut().transfer_member(
                    class_member_owner.clone(),
                    member_id,
                    false,
                );
            }
        }
    }
}

fn ensure_scoped_class_type_decl(
    db: &mut DbIndex,
    file_id: FileId,
    class_name: &str,
    global_name: &str,
    configured_super_types: &[String],
    range: rowan::TextRange,
) -> LuaTypeDeclId {
    let class_decl_id = get_scripted_class_type_decl_id(global_name, class_name);
    db.get_type_index_mut().add_type_decl(
        file_id,
        LuaTypeDecl::new(
            file_id,
            range,
            class_decl_id.get_simple_name().to_string(),
            LuaDeclTypeKind::Class,
            LuaTypeFlag::None.into(),
            class_decl_id.clone(),
        ),
    );

    for super_type in scoped_class_super_types(global_name, class_name, configured_super_types) {
        db.get_type_index_mut()
            .add_super_type(class_decl_id.clone(), file_id, range, super_type);
    }
    if let Some(base_class) =
        manifest_base_class(db.get_file_emmyrc(file_id), global_name, class_name)
    {
        db.get_type_index_mut().add_super_type(
            class_decl_id.clone(),
            file_id,
            range,
            LuaType::Ref(get_scripted_class_type_decl_id(global_name, &base_class)),
        );
    }
    class_decl_id
}

/// The class of the gamemode the manifest of gamemode class `class_name`
/// derives from, as `gamemode.Register` inherits it.
fn manifest_base_class(
    emmyrc: &crate::Emmyrc,
    global_name: &str,
    class_name: &str,
) -> Option<String> {
    if global_name != "GM" {
        return None;
    }
    let prefix = emmyrc
        .gmod
        .scripted_class_scopes
        .resolved_definitions_slice()
        .iter()
        .find(|definition| definition.id == "gamemodes")?
        .class_name_prefix
        .clone()?;
    let child = class_name.strip_prefix(prefix.as_str())?;
    let base = emmyrc.gmod.gamemode_bases.get(child)?;
    Some(format!("{prefix}{base}"))
}

/// Declares the shared class of each scope whose global `file_id` publishes a local table
/// as, and types that table as the class, unless an annotation already types it.
pub(crate) fn bind_published_scope_bases(
    db: &mut DbIndex,
    file_id: FileId,
    published_bases: &[(LuaDeclId, String)],
) {
    let emmyrc = db.get_file_emmyrc(file_id).clone();
    // Decided before any is typed: the loop types each table it publishes,
    // which is not an annotation of it.
    let annotated = published_bases
        .iter()
        .map(|(decl_id, _)| *decl_id)
        .filter(|decl_id| {
            db.get_type_index()
                .get_type_cache(&(*decl_id).into())
                .is_some_and(|cache| cache.is_doc())
        })
        .collect::<HashSet<_>>();
    for (decl_id, global) in published_bases {
        if annotated.contains(decl_id) {
            continue;
        }
        let Some(range) = db
            .get_decl_index()
            .get_decl(decl_id)
            .map(|decl| decl.get_range())
        else {
            continue;
        };
        let configured_super_types = emmyrc
            .gmod
            .scripted_class_scopes
            .resolved_definitions_slice()
            .iter()
            .filter(|definition| definition.class_global == *global)
            .flat_map(|definition| definition.super_types.iter().cloned())
            .collect::<Vec<_>>();
        let class_decl_id = LuaTypeDeclId::global(global);
        db.get_type_index_mut().add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                range,
                global.clone(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::None.into(),
                class_decl_id.clone(),
            ),
        );
        for super_type in scoped_class_super_types(global, global, &configured_super_types) {
            db.get_type_index_mut().add_super_type(
                class_decl_id.clone(),
                file_id,
                range,
                super_type,
            );
        }
        write_type_cache(
            db,
            (*decl_id).into(),
            LuaTypeCache::DocType(LuaType::Def(class_decl_id)),
            TypeCacheWriteMode::InsertOnly,
        );
    }
}

pub(crate) fn get_scripted_class_type_decl_id(
    global_name: &str,
    class_name: &str,
) -> LuaTypeDeclId {
    if scoped_class_uses_global_namespace(global_name) {
        LuaTypeDeclId::global(&format!("{global_name}.{class_name}"))
    } else {
        LuaTypeDeclId::global(class_name)
    }
}

pub(crate) fn resolve_scoped_authoring_type(
    db: &DbIndex,
    file_id: FileId,
    name: &str,
) -> Option<LuaTypeDeclId> {
    if !db.get_emmyrc().gmod.enabled {
        return None;
    }

    let info = db.get_gmod_infer_index().get_scoped_class_info(&file_id)?;
    (info.global_name == name
        || info.aliases.iter().any(|alias| alias == name)
        || (info.global_name == "GM" && name == "GAMEMODE"))
        .then(|| get_scripted_class_type_decl_id(&info.global_name, &info.class_name))
}

use crate::GmodVguiParentSourceResolution as ResolvedVguiParentSource;

use crate::GmodVguiResolvedParentCall as ResolvedVguiParentRelation;

#[derive(Clone)]
enum VguiParentChainResolution {
    None,
    Complete(Vec<LuaTypeDeclId>),
    Incomplete,
}

struct VguiFieldAssignmentParent {
    owner_type_ids: Vec<LuaTypeDeclId>,
    parent_type_ids: Vec<LuaTypeDeclId>,
}

enum ForwardingParentCandidate {
    Consistent(Vec<LuaTypeDeclId>),
    Conflicted,
}

/// What `file_id`'s own vgui parent calls resolve to, `None` without any.
pub(super) fn resolve_vgui_file_relations(
    db: &DbIndex,
    context: &mut AnalyzeContext,
    file_id: FileId,
) -> Option<crate::GmodVguiFileRelations> {
    let calls = db
        .get_gmod_class_metadata_index()
        .get_vgui_parent_calls(&file_id)
        .to_vec();
    if calls.is_empty() {
        return None;
    }
    let root = db.get_vfs().get_syntax_tree(&file_id)?.get_red_root();
    let cache = context.infer_manager.get_infer_cache(file_id);
    let mut candidates = HashMap::default();
    index_vgui_forwarding_parent_candidates(db, cache, file_id, &root, &calls, &mut candidates);
    let mut forwarding_candidates = candidates
        .into_iter()
        .map(|(key, candidate)| {
            let parent_type_ids = match candidate {
                ForwardingParentCandidate::Consistent(parent_type_ids) => Some(parent_type_ids),
                ForwardingParentCandidate::Conflicted => None,
            };
            (key, parent_type_ids)
        })
        .collect::<Vec<_>>();
    forwarding_candidates
        .sort_by_cached_key(|((owner, method), _)| (format!("{owner:?}"), method.clone()));
    Some(crate::GmodVguiFileRelations {
        forwarding_candidates,
        calls: resolve_vgui_parent_calls(db, cache, &root, &calls),
    })
}

/// `calls` of the file whose tree is `root`, with their child and parent
/// resolved to type ids.
fn resolve_vgui_parent_calls(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaSyntaxNode,
    calls: &[GmodVguiParentCallMetadata],
) -> Vec<ResolvedVguiParentRelation> {
    let field_assignment_parents = index_vgui_field_assignment_parents(db, cache, root);
    calls
        .iter()
        .filter_map(|call| {
            let call_expr = call
                .syntax_id
                .to_node_from_root(root)
                .and_then(LuaCallExpr::cast)?;
            let child_type_ids = resolve_vgui_parent_source_type_ids(
                db,
                cache,
                root,
                &field_assignment_parents,
                &call_expr,
                &call.child,
            );
            let parent = resolve_vgui_parent_source(
                db,
                cache,
                root,
                &field_assignment_parents,
                &call_expr,
                &call.parent,
            );
            Some(ResolvedVguiParentRelation {
                child_type_ids,
                parent,
            })
        })
        .collect()
}

/// Folds every file's resolved vgui parent calls, and the calls that forward
/// to a parent the files agree on, into the workspace parent chains.
fn resolve_vgui_parent_relations(db: &mut DbIndex, context: &mut AnalyzeContext) {
    let file_ids = db.get_gmod_class_metadata_index().vgui_parent_call_files();
    let mut forwarding_parent_candidates =
        HashMap::<(LuaTypeDeclId, String), ForwardingParentCandidate>::default();
    let mut relations_by_file = Vec::with_capacity(file_ids.len());
    for file_id in &file_ids {
        let Some(relations) = db
            .get_gmod_class_metadata_index()
            .get_vgui_file_relations(*file_id)
        else {
            continue;
        };
        for (key, parent_type_ids) in &relations.forwarding_candidates {
            match parent_type_ids {
                Some(parent_type_ids) => record_vgui_forwarding_parent_candidate(
                    &mut forwarding_parent_candidates,
                    key.clone(),
                    parent_type_ids.clone(),
                ),
                None => {
                    forwarding_parent_candidates
                        .insert(key.clone(), ForwardingParentCandidate::Conflicted);
                }
            }
        }
        relations_by_file.push(relations.calls.clone());
    }
    let forwarding_parents =
        finalize_vgui_forwarding_parent_candidates(forwarding_parent_candidates);
    for (file_id, forwarded) in
        collect_vgui_forwarding_parent_calls(db, context, &forwarding_parents)
    {
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_red_root())
        else {
            continue;
        };
        let cache = context.infer_manager.get_infer_cache(file_id);
        relations_by_file.push(resolve_vgui_parent_calls(db, cache, &root, &forwarded));
    }

    let mut direct_parents_by_child = HashMap::<LuaTypeDeclId, Vec<Vec<LuaTypeDeclId>>>::default();
    let mut relations_by_child = HashMap::<LuaTypeDeclId, Vec<ResolvedVguiParentSource>>::default();
    for relations in &relations_by_file {
        for relation in relations {
            for child_type_id in &relation.child_type_ids {
                if let ResolvedVguiParentSource::Direct(parent_type_ids) = &relation.parent {
                    direct_parents_by_child
                        .entry(child_type_id.clone())
                        .or_default()
                        .push(parent_type_ids.clone());
                }
                relations_by_child
                    .entry(child_type_id.clone())
                    .or_default()
                    .push(relation.parent.clone());
            }
        }
    }

    let mut direct_chain_memo = HashMap::default();
    let mut resolved = Vec::new();
    for relations in relations_by_file {
        for relation in relations {
            resolved.extend(relation.child_type_ids.into_iter().map(|child_type_id| {
                let resolution = resolve_vgui_parent_chain(
                    &child_type_id,
                    &relations_by_child,
                    &direct_parents_by_child,
                    &mut direct_chain_memo,
                );
                let (parent_chain, parent_chain_complete) = match resolution {
                    VguiParentChainResolution::Complete(chain) => (chain, true),
                    VguiParentChainResolution::None | VguiParentChainResolution::Incomplete => {
                        (Vec::new(), false)
                    }
                };
                crate::GmodVguiParentRelation {
                    child_type_id,
                    parent_chain,
                    parent_chain_complete,
                }
            }));
        }
    }
    db.get_gmod_class_metadata_index_mut()
        .set_vgui_parent_relations(&resolved);
}

fn index_vgui_forwarding_parent_candidates(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    root: &LuaSyntaxNode,
    calls: &[GmodVguiParentCallMetadata],
    candidates: &mut HashMap<(LuaTypeDeclId, String), ForwardingParentCandidate>,
) {
    for call in calls {
        let GmodVguiParentSource::Expr(child_syntax_id) = &call.child else {
            continue;
        };
        let Some(LuaExpr::NameExpr(child_name)) = child_syntax_id
            .to_node_from_root(root)
            .and_then(LuaExpr::cast)
        else {
            continue;
        };
        let Some(child_decl_id) = db
            .get_reference_index()
            .get_local_reference(&file_id)
            .and_then(|references| references.get_decl_id(&child_name.get_range()))
        else {
            continue;
        };
        if !db
            .get_decl_index()
            .get_decl(&child_decl_id)
            .is_some_and(crate::LuaDecl::is_param)
        {
            continue;
        }
        let Some(call_expr) = call
            .syntax_id
            .to_node_from_root(root)
            .and_then(LuaCallExpr::cast)
        else {
            continue;
        };
        let Some(function) = call_expr.ancestors::<LuaFuncStat>().next() else {
            continue;
        };
        let Some(LuaVarExpr::IndexExpr(function_name)) = function.get_func_name() else {
            continue;
        };
        let Some(method_name) = function_name.get_index_key().map(|key| key.get_path_part()) else {
            continue;
        };
        let Some(LuaExpr::IndexExpr(method_index)) = call_expr.get_prefix_expr() else {
            continue;
        };
        let Some(LuaExpr::IndexExpr(field_index)) = method_index.get_prefix_expr() else {
            continue;
        };
        let Some(owner) = field_index.get_prefix_expr() else {
            continue;
        };
        let owner_type_ids = resolve_vgui_parent_expr_type_ids(db, cache, owner);
        let [owner_type_id] = owner_type_ids.as_slice() else {
            continue;
        };
        let parent_type_ids = resolve_vgui_parent_source_type_ids(
            db,
            cache,
            root,
            &HashMap::default(),
            &call_expr,
            &call.parent,
        );
        if parent_type_ids.is_empty() {
            continue;
        }
        record_vgui_forwarding_parent_candidate(
            candidates,
            (owner_type_id.clone(), method_name),
            parent_type_ids,
        );
    }
}

fn record_vgui_forwarding_parent_candidate(
    candidates: &mut HashMap<(LuaTypeDeclId, String), ForwardingParentCandidate>,
    key: (LuaTypeDeclId, String),
    parent_type_ids: Vec<LuaTypeDeclId>,
) {
    match candidates.entry(key) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(ForwardingParentCandidate::Consistent(parent_type_ids));
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            if matches!(
                entry.get(),
                ForwardingParentCandidate::Consistent(existing) if existing != &parent_type_ids
            ) {
                entry.insert(ForwardingParentCandidate::Conflicted);
            }
        }
    }
}

fn finalize_vgui_forwarding_parent_candidates(
    candidates: HashMap<(LuaTypeDeclId, String), ForwardingParentCandidate>,
) -> HashMap<(LuaTypeDeclId, String), Vec<LuaTypeDeclId>> {
    candidates
        .into_iter()
        .filter_map(|(key, candidate)| {
            let ForwardingParentCandidate::Consistent(parent_type_ids) = candidate else {
                return None;
            };
            Some((key, parent_type_ids))
        })
        .collect()
}

fn collect_vgui_forwarding_parent_calls(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    forwarding_parents: &HashMap<(LuaTypeDeclId, String), Vec<LuaTypeDeclId>>,
) -> Vec<(FileId, Vec<GmodVguiParentCallMetadata>)> {
    if forwarding_parents.is_empty() {
        return Vec::new();
    }
    let forwarding_method_names = forwarding_parents
        .keys()
        .map(|(_, method_name)| method_name.as_str())
        .collect::<HashSet<_>>();
    let mut file_ids = HashSet::default();
    for method_name in &forwarding_method_names {
        file_ids.extend(
            db.get_reference_index()
                .files_with_index_references(&SmolStr::new(method_name)),
        );
    }
    let mut file_ids = file_ids.into_iter().collect::<Vec<_>>();
    file_ids.sort_unstable();
    let files = file_ids
        .into_iter()
        .filter_map(|file_id| {
            db.get_vfs()
                .get_syntax_tree(&file_id)
                .map(|tree| (file_id, tree.get_red_root()))
        })
        .collect::<Vec<_>>();
    let mut forwarded = Vec::new();
    for (file_id, root) in files {
        let existing_call_syntax_ids = db
            .get_gmod_class_metadata_index()
            .get_vgui_parent_calls(&file_id)
            .iter()
            .map(|call| call.syntax_id)
            .collect::<HashSet<_>>();
        let cache = context.infer_manager.get_infer_cache(file_id);
        let mut calls = Vec::new();
        for call_expr in root.descendants_of::<LuaCallExpr>() {
            if existing_call_syntax_ids.contains(&call_expr.get_syntax_id()) {
                continue;
            }
            if !call_expr.is_colon_call() {
                continue;
            }
            let Some(LuaExpr::IndexExpr(method_index)) = call_expr.get_prefix_expr() else {
                continue;
            };
            let Some(receiver) = method_index.get_prefix_expr() else {
                continue;
            };
            let Some(method_name) = method_index.get_index_key().map(|key| key.get_path_part())
            else {
                continue;
            };
            if !forwarding_method_names.contains(method_name.as_str()) {
                continue;
            }
            let receiver_type_ids = resolve_vgui_parent_expr_type_ids(db, cache, receiver);
            let [receiver_type_id] = receiver_type_ids.as_slice() else {
                continue;
            };
            let Some(parent_type_ids) =
                forwarding_parents.get(&(receiver_type_id.clone(), method_name))
            else {
                continue;
            };
            let [parent_type_id] = parent_type_ids.as_slice() else {
                continue;
            };
            let Some(child) = call_expr
                .get_args_list()
                .and_then(|args| args.get_args().next())
            else {
                continue;
            };
            calls.push(GmodVguiParentCallMetadata {
                syntax_id: call_expr.get_syntax_id(),
                child: GmodVguiParentSource::Expr(child.get_syntax_id()),
                parent: GmodVguiParentSource::LiteralName(parent_type_id.get_name().to_string()),
            });
        }
        if !calls.is_empty() {
            forwarded.push((file_id, calls));
        }
    }
    forwarded
}

#[cfg(test)]
mod forwarding_parent_candidate_tests {
    use std::collections::HashMap;

    use super::{
        ForwardingParentCandidate, finalize_vgui_forwarding_parent_candidates,
        record_vgui_forwarding_parent_candidate,
    };
    use crate::LuaTypeDeclId;

    #[test]
    fn identical_forwarding_candidates_remain_consistent() {
        let key = (LuaTypeDeclId::global("Container"), "Add".to_string());
        let parent_type_ids = vec![LuaTypeDeclId::global("DTileLayout")];
        let mut candidates = HashMap::default();

        record_vgui_forwarding_parent_candidate(
            &mut candidates,
            key.clone(),
            parent_type_ids.clone(),
        );
        record_vgui_forwarding_parent_candidate(
            &mut candidates,
            key.clone(),
            parent_type_ids.clone(),
        );

        let finalized = finalize_vgui_forwarding_parent_candidates(candidates);
        assert_eq!(finalized.get(&key), Some(&parent_type_ids));
    }

    #[test]
    fn disagreeing_forwarding_candidates_remain_conflicted() {
        let key = (LuaTypeDeclId::global("Container"), "Add".to_string());
        let mut candidates = HashMap::default();

        record_vgui_forwarding_parent_candidate(
            &mut candidates,
            key.clone(),
            vec![LuaTypeDeclId::global("DTileLayout")],
        );
        record_vgui_forwarding_parent_candidate(
            &mut candidates,
            key.clone(),
            vec![LuaTypeDeclId::global("DIconLayout")],
        );
        record_vgui_forwarding_parent_candidate(
            &mut candidates,
            key.clone(),
            vec![LuaTypeDeclId::global("DTileLayout")],
        );

        assert!(matches!(
            candidates.get(&key),
            Some(ForwardingParentCandidate::Conflicted)
        ));
        assert!(finalize_vgui_forwarding_parent_candidates(candidates).is_empty());
    }
}

#[cfg(test)]
mod edit_tests {
    use crate::{Emmyrc, VirtualWorkspace};

    fn set_gmod_enabled(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        // Net ops and load calls resolve through signature metadata, so the
        // annotated builtins must be present or no flows or sites exist.
        ws.def_gmod_call_arg_builtins();
    }

    /// Adding and removing an `include(...)` moves the load index.
    #[test]
    fn adding_and_removing_include_moves_load_index() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let helper_id = ws.def_file("lua/b6_helper.lua", "return {}\n");
        let main_id = ws.def_file("lua/b6_main.lua", "return {}\n");

        let has_edge_from_main = |ws: &mut VirtualWorkspace| {
            ws.get_db_mut()
                .get_gmod_load_index()
                .get_file_info(&helper_id)
                .is_some_and(|info| {
                    info.incoming_edges
                        .iter()
                        .any(|edge| edge.source_file_id == main_id)
                })
        };
        assert!(
            !has_edge_from_main(&mut ws),
            "no include yet, so no load edge is expected"
        );

        ws.def_file("lua/b6_main.lua", "include(\"b6_helper.lua\")\n");
        assert!(
            has_edge_from_main(&mut ws),
            "adding include should move the load index"
        );
        ws.def_file("lua/b6_main.lua", "return {}\n");
        assert!(
            !has_edge_from_main(&mut ws),
            "removing include should move the load index back"
        );
    }

    /// Network flows survive an unrelated edit and follow a helper edit.
    #[test]
    fn network_flows_retained_across_unrelated_edit_and_updated_on_helper_edit() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        ws.def_file(
            "lua/b8_helper.lua",
            "function SendGreeting()\nnet.Start(\"B8Msg\")\nnet.Broadcast()\nend\n",
        );
        let caller_id = ws.def_file("lua/b8_caller.lua", "SendGreeting()\n");
        ws.def_file("lua/b8_plain.lua", "local y = 1\n");

        let caller_messages = |ws: &mut VirtualWorkspace| {
            ws.get_db_mut()
                .get_gmod_network_index()
                .get_file_data(caller_id)
                .map(|data| {
                    data.send_flows
                        .iter()
                        .map(|flow| flow.message_name.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        assert_eq!(caller_messages(&mut ws), vec!["B8Msg".to_string()]);
        // A scanless, flowless edit cannot move any network input.
        ws.def_file("lua/b8_plain.lua", "local y = 2\n");
        assert_eq!(
            caller_messages(&mut ws),
            vec!["B8Msg".to_string()],
            "unrelated edit should keep network flows"
        );
        // Editing the expanded helper must invalidate the caller.
        ws.def_file(
            "lua/b8_helper.lua",
            "function SendGreeting()\nnet.Start(\"B8Msg2\")\nnet.Broadcast()\nend\n",
        );
        assert_eq!(
            caller_messages(&mut ws),
            vec!["B8Msg2".to_string()],
            "helper edit should update the caller's network flows"
        );
    }
}

#[cfg(test)]
mod net_candidate_gate_tests {
    use crate::{Emmyrc, VirtualWorkspace};

    fn set_gmod_enabled(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        // Net ops resolve through signature metadata, so the annotated
        // builtins must be present or no flows are collected at all.
        ws.def_gmod_call_arg_builtins();
    }

    /// N4 gate: a file with no candidate call expressions skips the
    /// candidate-driven walks (receive + unannotated-send) with a result
    /// identical to running them, while a network file still produces flows.
    /// The direct and wrapped scans intentionally keep running for every file:
    /// they resolve through signatures, so an empty candidate list alone can
    /// never prove they would find nothing (see the N4 note in
    /// `collect_hook_and_receive_metadata`).
    #[test]
    fn empty_candidates_skip_preserves_flows() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let net_id = ws.def_file(
            "addons/mytest/lua/autorun/server/gate_net.lua",
            r#"
            net.Start("GateMsg")
            net.WriteString("hello")
            net.Broadcast()
            "#,
        );
        let plain_id = ws.def_file(
            "addons/mytest/lua/autorun/server/gate_plain.lua",
            "local x = 1\nreturn x\n",
        );

        let data = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(net_id)
            .expect("expected network data");
        assert_eq!(data.send_flows.len(), 1);
        assert_eq!(data.send_flows[0].message_name.as_str(), "GateMsg");

        // The plain file takes every N4 early-out (no `by_file` entry, empty
        // candidate list): nothing is stored for it, exactly what an ungated
        // walk that found no flows would produce.
        assert!(
            ws.get_db_mut()
                .get_gmod_network_index()
                .get_file_data(plain_id)
                .is_none()
        );
    }

    /// N1 gate: a diamond helper (`top` -> `left`/`right` -> `leaf`) must give
    /// the same flows as an unmemoised walk, while the shared `leaf` body is
    /// walked once per distinct bindings (second visit hits the shared memo).
    /// Calling the same param-forwarding helper with two literals also guards
    /// the bindings key: a helper-only key would return the first message for
    /// both call sites.
    ///
    /// N2 extends the same diamond to body-scan sharing: full-transaction
    /// helpers (`send_body`), receive wrappers (`receive_body`) and payload
    /// helpers (`ops_body`) memoise the body-derived output per
    /// `(helper, bindings)` (or `(helper, direction)` for ops) and stamp the
    /// call-site origin per hit. The assertions below guard both halves: the
    /// bindings key (different literals give different messages) and the
    /// origin stamp (same literal at two sites gives two flows with distinct
    /// ranges but identical bodies).
    #[test]
    fn diamond_helper_memo_preserves_flows() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let start_id = ws.def_file(
            "addons/mytest/lua/autorun/server/diamond_start.lua",
            r#"
            local function leaf(msg)
                net.Start(msg)
            end
            local function left(msg)
                leaf(msg)
            end
            local function right(msg)
                leaf(msg)
            end
            local function top(msg)
                left(msg)
                right(msg)
            end
            top("DiamondA")
            net.WriteString("a")
            net.Broadcast()
            top("DiamondB")
            net.WriteString("b")
            net.Broadcast()
            "#,
        );
        let send_id = ws.def_file(
            "addons/mytest/lua/autorun/server/diamond_send.lua",
            r#"
            local function leafSend()
                net.Broadcast()
            end
            local function leftSend()
                leafSend()
            end
            local function rightSend()
                leafSend()
            end
            local function topSend()
                leftSend()
                rightSend()
            end
            net.Start("DiamondSend")
            net.WriteString("x")
            topSend()
            "#,
        );

        let start_data = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(start_id)
            .expect("expected network data");
        assert_eq!(start_data.send_flows.len(), 2);
        assert_eq!(start_data.send_flows[0].message_name.as_str(), "DiamondA");
        assert_eq!(start_data.send_flows[1].message_name.as_str(), "DiamondB");

        let send_data = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(send_id)
            .expect("expected network data");
        assert_eq!(send_data.send_flows.len(), 1);
        assert_eq!(send_data.send_flows[0].message_name.as_str(), "DiamondSend");
        // Sends through a helper materialize at the call site, so the display
        // name is the wrapper call rather than the inner `net.Broadcast`.
        assert_eq!(
            send_data.send_flows[0].send_display_name.as_str(),
            "topSend"
        );

        // N2: full-transaction bodies share one scan per `(helper, bindings)`
        // while stamping the origin per call site. The diamond (`topBody` ->
        // `leftBody`/`rightBody` -> `fullBody`) exercises the shared scan;
        // the two literals guard the bindings key; the repeated `SameMsg`
        // calls guard the origin stamp.
        let body_id = ws.def_file(
            "addons/mytest/lua/autorun/server/diamond_body.lua",
            r#"
            local function fullBody(msg)
                net.Start(msg)
                net.WriteString("payload")
                net.Broadcast()
            end
            local function leftBody(msg)
                fullBody(msg)
            end
            local function rightBody(msg)
                fullBody(msg)
            end
            local function topBody(msg)
                leftBody(msg)
                rightBody(msg)
            end
            topBody("BodyShareA")
            topBody("BodyShareB")
            fullBody("SameMsg")
            fullBody("SameMsg")
            "#,
        );
        let body_data = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(body_id)
            .expect("expected network data");
        assert_eq!(body_data.send_flows.len(), 4);
        assert_eq!(body_data.send_flows[0].message_name.as_str(), "BodyShareA");
        assert_eq!(body_data.send_flows[1].message_name.as_str(), "BodyShareB");
        assert_eq!(body_data.send_flows[2].message_name.as_str(), "SameMsg");
        assert_eq!(body_data.send_flows[3].message_name.as_str(), "SameMsg");
        // Same literal at two sites: distinct origins, identical bodies.
        assert_ne!(
            body_data.send_flows[2].start_range,
            body_data.send_flows[3].start_range
        );
        assert_eq!(
            body_data.send_flows[2].materialized_from,
            body_data.send_flows[3].materialized_from
        );
        assert_eq!(
            body_data.send_flows[2].writes,
            body_data.send_flows[3].writes
        );
        assert_eq!(body_data.send_flows[2].writes.len(), 1);
        // Diamond duplicates collapse: each `topBody` call expands to two
        // identical bodies (left + right) with the same origin, deduped to one.
        assert_ne!(
            body_data.send_flows[0].start_range,
            body_data.send_flows[1].start_range
        );

        // N2: receive bodies share per `(helper, bindings)` the same way.
        // Each `recvTop` call reaches the leaf twice (left + right), so one
        // call yields two identical flows; two literals guard the bindings key.
        let recv_id = ws.def_file(
            "addons/mytest/lua/autorun/server/diamond_recv.lua",
            r#"
            local function recvLeaf(msg)
                net.Receive(msg, function()
                    net.ReadString()
                end)
            end
            local function recvLeft(msg)
                recvLeaf(msg)
            end
            local function recvRight(msg)
                recvLeaf(msg)
            end
            local function recvTop(msg)
                recvLeft(msg)
                recvRight(msg)
            end
            recvTop("RecvShareA")
            recvTop("RecvShareB")
            "#,
        );
        let recv_data = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(recv_id)
            .expect("expected network data");
        assert_eq!(recv_data.receive_flows.len(), 4);
        assert_eq!(
            recv_data.receive_flows[0].message_name.as_str(),
            "RecvShareA"
        );
        assert_eq!(
            recv_data.receive_flows[1].message_name.as_str(),
            "RecvShareA"
        );
        assert_eq!(
            recv_data.receive_flows[2].message_name.as_str(),
            "RecvShareB"
        );
        assert_eq!(
            recv_data.receive_flows[3].message_name.as_str(),
            "RecvShareB"
        );
        assert_eq!(
            recv_data.receive_flows[0].receive_range,
            recv_data.receive_flows[1].receive_range
        );
        assert!(!recv_data.receive_flows[0].reads.is_empty());

        // N2: payload-op bodies share per `(helper, direction)` with per-caller
        // stamping of `dynamic` and the flow prefix. `writePayload` is reached
        // from two top-level transactions (shared baseline, both static);
        // `writeCond` is reached from inside an `if` (same baseline shape,
        // stamped dynamic with the outer frame).
        let ops_id = ws.def_file(
            "addons/mytest/lua/autorun/server/diamond_ops.lua",
            r#"
            local function writePayload()
                net.WriteString("shared")
            end
            local function writeCond()
                net.WriteString("cond")
            end
            net.Start("OpsShareA")
            writePayload()
            net.Broadcast()
            net.Start("OpsShareB")
            writePayload()
            net.Broadcast()
            net.Start("OpsCond")
            if foo then
                writeCond()
            end
            net.Broadcast()
            "#,
        );
        let ops_data = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(ops_id)
            .expect("expected network data");
        assert_eq!(ops_data.send_flows.len(), 3);
        assert_eq!(ops_data.send_flows[0].message_name.as_str(), "OpsShareA");
        assert_eq!(ops_data.send_flows[1].message_name.as_str(), "OpsShareB");
        assert_eq!(ops_data.send_flows[2].message_name.as_str(), "OpsCond");
        assert_eq!(ops_data.send_flows[0].writes.len(), 1);
        assert_eq!(ops_data.send_flows[1].writes.len(), 1);
        assert_eq!(ops_data.send_flows[0].writes, ops_data.send_flows[1].writes);
        assert!(!ops_data.send_flows[0].writes[0].dynamic);
        assert!(ops_data.send_flows[0].writes[0].flow_path.is_empty());
        assert_eq!(ops_data.send_flows[2].writes.len(), 1);
        assert!(ops_data.send_flows[2].writes[0].dynamic);
        assert!(!ops_data.send_flows[2].writes[0].flow_path.is_empty());
    }
}

fn resolve_vgui_parent_chain(
    type_id: &LuaTypeDeclId,
    relations_by_child: &HashMap<LuaTypeDeclId, Vec<ResolvedVguiParentSource>>,
    direct_parents_by_child: &HashMap<LuaTypeDeclId, Vec<Vec<LuaTypeDeclId>>>,
    direct_chain_memo: &mut HashMap<LuaTypeDeclId, VguiParentChainResolution>,
) -> VguiParentChainResolution {
    let Some(parents) = relations_by_child.get(type_id) else {
        return VguiParentChainResolution::None;
    };

    let mut chain = None;
    for parent in parents {
        let parent_chain = match parent {
            ResolvedVguiParentSource::Direct(type_ids) => {
                let Some(parent_chain) = resolve_vgui_direct_parent_chain(
                    type_ids,
                    direct_parents_by_child,
                    direct_chain_memo,
                ) else {
                    return VguiParentChainResolution::Incomplete;
                };
                parent_chain
            }
            ResolvedVguiParentSource::AssignedField {
                field_type_ids,
                assignment_parent_type_ids,
            } => {
                let [field_type_id] = field_type_ids.as_slice() else {
                    return VguiParentChainResolution::Incomplete;
                };
                let Some(mut parent_chain) = resolve_vgui_direct_parent_chain(
                    assignment_parent_type_ids,
                    direct_parents_by_child,
                    direct_chain_memo,
                ) else {
                    return VguiParentChainResolution::Incomplete;
                };
                // The field assignment identifies this edge's owner. Do not walk
                // type-level direct relations for the intermediate field panel.
                parent_chain.insert(0, field_type_id.clone());
                parent_chain
            }
            ResolvedVguiParentSource::ReceiverField {
                field_type_ids,
                receiver_type_ids,
                receiver_field_parent_type_ids,
            } => {
                let ([field_type_id], [receiver_type_id]) =
                    (field_type_ids.as_slice(), receiver_type_ids.as_slice())
                else {
                    return VguiParentChainResolution::Incomplete;
                };
                let Some(assignment_parent_chain) = resolve_vgui_direct_parent_chain(
                    receiver_field_parent_type_ids
                        .as_deref()
                        .unwrap_or_default(),
                    direct_parents_by_child,
                    direct_chain_memo,
                ) else {
                    return VguiParentChainResolution::Incomplete;
                };
                // Receiver-field ownership is edge-specific. Only follow the field's
                // assigned vgui.Create parent through type-level direct relations.
                let mut parent_chain = vec![field_type_id.clone(), receiver_type_id.clone()];
                parent_chain.extend(assignment_parent_chain);
                parent_chain
            }
        };
        match &chain {
            Some(existing) if existing != &parent_chain => {
                return VguiParentChainResolution::Incomplete;
            }
            Some(_) => {}
            None => chain = Some(parent_chain),
        }
    }
    chain
        .map(VguiParentChainResolution::Complete)
        .unwrap_or(VguiParentChainResolution::Incomplete)
}

fn resolve_vgui_direct_parent_chain(
    type_ids: &[LuaTypeDeclId],
    direct_parents_by_child: &HashMap<LuaTypeDeclId, Vec<Vec<LuaTypeDeclId>>>,
    memo: &mut HashMap<LuaTypeDeclId, VguiParentChainResolution>,
) -> Option<Vec<LuaTypeDeclId>> {
    resolve_vgui_direct_parent_chain_with_visiting(
        type_ids,
        direct_parents_by_child,
        memo,
        &mut HashSet::default(),
    )
}

fn resolve_vgui_direct_parent_chain_with_visiting(
    type_ids: &[LuaTypeDeclId],
    direct_parents_by_child: &HashMap<LuaTypeDeclId, Vec<Vec<LuaTypeDeclId>>>,
    memo: &mut HashMap<LuaTypeDeclId, VguiParentChainResolution>,
    visiting: &mut HashSet<LuaTypeDeclId>,
) -> Option<Vec<LuaTypeDeclId>> {
    let [type_id] = type_ids else {
        return None;
    };
    match resolve_vgui_direct_parent_chain_for_type(
        type_id,
        direct_parents_by_child,
        memo,
        visiting,
    ) {
        VguiParentChainResolution::None => Some(vec![type_id.clone()]),
        VguiParentChainResolution::Complete(mut chain) => {
            chain.insert(0, type_id.clone());
            Some(chain)
        }
        VguiParentChainResolution::Incomplete => None,
    }
}

fn resolve_vgui_direct_parent_chain_for_type(
    type_id: &LuaTypeDeclId,
    direct_parents_by_child: &HashMap<LuaTypeDeclId, Vec<Vec<LuaTypeDeclId>>>,
    memo: &mut HashMap<LuaTypeDeclId, VguiParentChainResolution>,
    visiting: &mut HashSet<LuaTypeDeclId>,
) -> VguiParentChainResolution {
    if let Some(resolution) = memo.get(type_id) {
        return resolution.clone();
    }
    let Some(parents) = direct_parents_by_child.get(type_id) else {
        return VguiParentChainResolution::None;
    };
    if !visiting.insert(type_id.clone()) {
        return VguiParentChainResolution::Incomplete;
    }

    let mut chain = None;
    for parent_type_ids in parents {
        let Some(parent_chain) = resolve_vgui_direct_parent_chain_with_visiting(
            parent_type_ids,
            direct_parents_by_child,
            memo,
            visiting,
        ) else {
            visiting.remove(type_id);
            memo.insert(type_id.clone(), VguiParentChainResolution::Incomplete);
            return VguiParentChainResolution::Incomplete;
        };
        match &chain {
            Some(existing) if existing != &parent_chain => {
                visiting.remove(type_id);
                memo.insert(type_id.clone(), VguiParentChainResolution::Incomplete);
                return VguiParentChainResolution::Incomplete;
            }
            Some(_) => {}
            None => chain = Some(parent_chain),
        }
    }
    visiting.remove(type_id);
    let resolution = chain
        .map(VguiParentChainResolution::Complete)
        .unwrap_or(VguiParentChainResolution::Incomplete);
    memo.insert(type_id.clone(), resolution.clone());
    resolution
}

fn resolve_vgui_parent_source_type_ids(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaSyntaxNode,
    field_assignment_parents: &HashMap<String, Vec<VguiFieldAssignmentParent>>,
    call_expr: &LuaCallExpr,
    source: &GmodVguiParentSource,
) -> Vec<LuaTypeDeclId> {
    match resolve_vgui_parent_source(db, cache, root, field_assignment_parents, call_expr, source) {
        ResolvedVguiParentSource::Direct(type_ids) => type_ids,
        ResolvedVguiParentSource::AssignedField { field_type_ids, .. }
        | ResolvedVguiParentSource::ReceiverField { field_type_ids, .. } => field_type_ids,
    }
}

fn resolve_vgui_parent_source(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaSyntaxNode,
    field_assignment_parents: &HashMap<String, Vec<VguiFieldAssignmentParent>>,
    call_expr: &LuaCallExpr,
    source: &GmodVguiParentSource,
) -> ResolvedVguiParentSource {
    let resolve_type_ids = |typ: LuaType| {
        let mut type_ids = Vec::new();
        collect_panel_type_ids(db, &typ, &mut type_ids);
        type_ids.sort_by(|left, right| left.get_name().cmp(right.get_name()));
        type_ids.dedup();
        type_ids
    };
    match source {
        GmodVguiParentSource::LiteralName(name) => {
            let type_id = LuaTypeDeclId::global(name);
            let type_ids = if db.get_type_index().get_type_decl(&type_id).is_some() {
                resolve_type_ids(LuaType::Ref(type_id))
            } else {
                Vec::new()
            };
            ResolvedVguiParentSource::Direct(type_ids)
        }
        GmodVguiParentSource::Expr(syntax_id) => {
            let Some(expr) = syntax_id.to_node_from_root(root).and_then(LuaExpr::cast) else {
                return ResolvedVguiParentSource::Direct(Vec::new());
            };
            let field_type_ids = resolve_vgui_parent_expr_type_ids(db, cache, expr.clone());
            if matches!(expr, LuaExpr::IndexExpr(_))
                && let Some(assignment_parent_type_ids) =
                    resolve_vgui_field_assignment_parent_type_ids(
                        db,
                        cache,
                        &expr,
                        &field_type_ids,
                        field_assignment_parents,
                    )
            {
                return ResolvedVguiParentSource::AssignedField {
                    field_type_ids,
                    assignment_parent_type_ids,
                };
            }
            ResolvedVguiParentSource::Direct(field_type_ids)
        }
        GmodVguiParentSource::Unknown => ResolvedVguiParentSource::Direct(Vec::new()),
        GmodVguiParentSource::Receiver | GmodVguiParentSource::ReceiverField(_) => {
            let receiver = call_expr.get_prefix_expr().and_then(|prefix| match prefix {
                LuaExpr::IndexExpr(index_expr) => index_expr.get_prefix_expr(),
                _ => None,
            });
            let receiver_type_ids = receiver
                .clone()
                .map(|expr| resolve_vgui_parent_expr_type_ids(db, cache, expr))
                .unwrap_or_default();
            let receiver_type = match receiver_type_ids.as_slice() {
                [type_id] => Some(LuaType::Ref(type_id.clone())),
                _ => receiver
                    .clone()
                    .and_then(|expr| infer_expr(db, cache, expr).ok()),
            };
            match source {
                GmodVguiParentSource::Receiver => {
                    ResolvedVguiParentSource::Direct(receiver_type_ids)
                }
                GmodVguiParentSource::ReceiverField(field_path) => {
                    let field_type = receiver_type.and_then(|initial| {
                        field_path.iter().try_fold(initial, |typ, field| {
                            crate::semantic::infer_raw_member_type_with_cache(
                                db,
                                cache,
                                &typ,
                                &LuaMemberKey::Name(field.as_str().into()),
                            )
                            .ok()
                        })
                    });
                    let field_type_ids = field_type.map(resolve_type_ids).unwrap_or_default();
                    let receiver_field_parent_type_ids = receiver.as_ref().and_then(|receiver| {
                        resolve_vgui_field_assignment_parent_type_ids(
                            db,
                            cache,
                            receiver,
                            &receiver_type_ids,
                            field_assignment_parents,
                        )
                    });
                    ResolvedVguiParentSource::ReceiverField {
                        field_type_ids,
                        receiver_type_ids,
                        receiver_field_parent_type_ids,
                    }
                }
                _ => unreachable!(),
            }
        }
    }
}

fn resolve_vgui_field_assignment_parent_type_ids(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    field_expr: &LuaExpr,
    field_type_ids: &[LuaTypeDeclId],
    field_assignment_parents: &HashMap<String, Vec<VguiFieldAssignmentParent>>,
) -> Option<Vec<LuaTypeDeclId>> {
    let LuaExpr::IndexExpr(field_expr) = field_expr else {
        return None;
    };
    let field_path = field_expr.get_access_path()?;
    let owner = field_expr.get_prefix_expr()?;
    let owner_type_ids = resolve_vgui_parent_expr_type_ids(db, cache, owner);
    let mut candidates = field_assignment_parents
        .get(field_path.as_str())?
        .iter()
        .filter(|assignment| {
            !field_type_ids.is_empty() && assignment.owner_type_ids == owner_type_ids
        });
    let parent_type_ids = candidates.next()?;
    if candidates.next().is_some() {
        return None;
    }
    Some(parent_type_ids.parent_type_ids.clone())
}

fn index_vgui_field_assignment_parents(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    root: &LuaSyntaxNode,
) -> HashMap<String, Vec<VguiFieldAssignmentParent>> {
    let mut assignments = HashMap::default();
    for assign in root.descendants_of::<LuaAssignStat>() {
        let (vars, exprs) = assign.get_var_and_expr_list();
        for (target, value) in vars.iter().zip(exprs) {
            let LuaVarExpr::IndexExpr(target) = target else {
                continue;
            };
            let Some(field_path) = target.get_access_path() else {
                continue;
            };
            let Some(owner) = target.get_prefix_expr() else {
                continue;
            };
            let LuaExpr::CallExpr(create_call) = value else {
                continue;
            };
            if create_call.get_access_path().as_deref() != Some("vgui.Create") {
                continue;
            }
            let Some(parent) = create_call
                .get_args_list()
                .and_then(|args| args.get_args().nth(1))
            else {
                continue;
            };
            let parent_type_ids = resolve_vgui_parent_expr_type_ids(db, cache, parent);
            if parent_type_ids.is_empty() {
                continue;
            }
            assignments
                .entry(field_path.to_string())
                .or_insert_with(Vec::new)
                .push(VguiFieldAssignmentParent {
                    owner_type_ids: resolve_vgui_parent_expr_type_ids(db, cache, owner),
                    parent_type_ids,
                });
        }
    }
    assignments
}

fn resolve_vgui_parent_expr_type_ids(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: LuaExpr,
) -> Vec<LuaTypeDeclId> {
    if let LuaExpr::NameExpr(name_expr) = &expr
        && name_expr.get_name_text().as_deref() == Some("self")
    {
        if let Some(context) =
            crate::semantic::resolve_registered_vgui_method_context(db, cache, name_expr)
        {
            // A resolved vgui.Register/derma.DefineControl table is necessarily
            // a Panel even while its inherited base class is still stabilizing.
            return vec![LuaTypeDeclId::global(&context.panel_name)];
        }
        for function in name_expr.ancestors::<LuaFuncStat>() {
            let Some(LuaVarExpr::IndexExpr(index_expr)) = function.get_func_name() else {
                continue;
            };
            let Some(LuaExpr::NameExpr(class_name)) = index_expr.get_prefix_expr() else {
                continue;
            };
            let Some(class_name) = class_name.get_name_text() else {
                continue;
            };
            let type_id = LuaTypeDeclId::global(&class_name);
            if db.get_type_index().get_type_decl(&type_id).is_some() {
                let mut type_ids = Vec::new();
                collect_panel_type_ids(db, &LuaType::Ref(type_id), &mut type_ids);
                if !type_ids.is_empty() {
                    return type_ids;
                }
            }
        }
    }
    infer_expr(db, cache, expr)
        .map(|typ| {
            let mut type_ids = Vec::new();
            collect_panel_type_ids(db, &typ, &mut type_ids);
            type_ids.sort_by(|left, right| left.get_name().cmp(right.get_name()));
            type_ids.dedup();
            type_ids
        })
        .unwrap_or_default()
}

fn collect_panel_type_ids(db: &DbIndex, typ: &LuaType, type_ids: &mut Vec<LuaTypeDeclId>) {
    match typ {
        LuaType::Instance(instance) => collect_panel_type_ids(db, instance.get_base(), type_ids),
        LuaType::Union(union) => {
            for typ in union.types() {
                collect_panel_type_ids(db, typ, type_ids);
            }
        }
        LuaType::Def(type_id) | LuaType::Ref(type_id) => {
            let panel_id = LuaTypeDeclId::global("Panel");
            if *type_id == panel_id || crate::semantic::is_sub_type_of(db, type_id, &panel_id) {
                type_ids.push(type_id.clone());
            }
        }
        _ => {}
    }
}

pub(crate) fn name_expr_resolves_to_scoped_authoring_table(
    db: &DbIndex,
    file_id: FileId,
    name_expr: &LuaNameExpr,
) -> Option<LuaTypeDeclId> {
    let name = name_expr.get_name_text()?;
    let class_decl_id = resolve_scoped_authoring_type(db, file_id, &name)?;

    let local_decl = db
        .get_reference_index()
        .get_local_reference(&file_id)
        .and_then(|file_ref| file_ref.get_decl_id(&name_expr.get_range()))
        .filter(|decl_id| db.get_decl_index().get_decl(decl_id).is_some());

    let Some(decl_id) = local_decl else {
        return Some(class_decl_id);
    };

    scoped_authoring_decl_has_type(db, decl_id, &class_decl_id).then_some(class_decl_id)
}

pub(crate) fn scoped_authoring_local_overrides_runtime_table(
    db: &DbIndex,
    file_id: FileId,
    name: &str,
    decl_id: LuaDeclId,
) -> bool {
    if scoped_class_authored_as_local(name) {
        return false;
    }

    let Some(class_decl_id) = resolve_scoped_authoring_type(db, file_id, name) else {
        return false;
    };

    let Some(decl) = db.get_decl_index().get_decl(&decl_id) else {
        return false;
    };
    if !decl.is_local() {
        return false;
    }

    !scoped_authoring_decl_has_type(db, decl_id, &class_decl_id)
}

fn scoped_authoring_decl_has_type(
    db: &DbIndex,
    decl_id: LuaDeclId,
    class_decl_id: &LuaTypeDeclId,
) -> bool {
    let Some(decl) = db.get_decl_index().get_decl(&decl_id) else {
        return false;
    };
    if !decl.is_local() {
        return false;
    }

    db.get_type_index()
        .get_type_cache(&decl_id.into())
        .is_some_and(|type_cache| type_cache.as_type() == &LuaType::Def(class_decl_id.clone()))
}

fn scoped_class_uses_global_namespace(global_name: &str) -> bool {
    matches!(global_name, "TOOL" | "EFFECT")
}

/// Scopes whose authoring table is conventionally declared as a `local`
/// (e.g. `local PLUGIN = {}`, `local PLAYER = {}`) rather than a bare global.
/// For these, an explicit local declaration with the scope's global name is
/// treated as the scoped class table even without the synthetic seed.
pub(crate) fn scoped_class_authored_as_local(global_name: &str) -> bool {
    matches!(global_name, "PLUGIN" | "PLAYER")
}

fn scoped_class_super_types(
    global_name: &str,
    class_name: &str,
    configured: &[String],
) -> Vec<LuaType> {
    // PLAYER is special: the runtime authoring table is named `PLAYER`, but that
    // identifier is already a GMod enum alias (`PLAYER_IDLE`, ... in enums.lua),
    // so the authoring-class annotation cannot use it. The shared player-class
    // fields live on the `PlayerClass` annotation class instead. The player-class
    // table is NOT itself a Player entity (methods use `self.Player:...`), so it
    // inherits only `PlayerClass`.
    if global_name == "PLAYER" {
        return vec![LuaType::Ref(LuaTypeDeclId::global("PlayerClass"))];
    }

    let mut super_types = Vec::new();
    if class_name != global_name {
        super_types.push(LuaType::Ref(LuaTypeDeclId::global(global_name)));
    }
    match global_name {
        "TOOL" => super_types.push(LuaType::Ref(LuaTypeDeclId::global("Tool"))),
        "SWEP" => super_types.push(LuaType::Ref(LuaTypeDeclId::global("Weapon"))),
        "ENT" => super_types.push(LuaType::Ref(LuaTypeDeclId::global("Entity"))),
        "PLUGIN" => super_types.push(LuaType::Ref(LuaTypeDeclId::global("GM"))),
        _ => {}
    }

    for super_type in configured {
        let super_type = LuaType::Ref(LuaTypeDeclId::global(super_type));
        if !super_types.contains(&super_type) {
            super_types.push(super_type);
        }
    }

    super_types
}

pub(crate) fn ensure_scoped_class_type_decl_for_file(
    db: &mut DbIndex,
    file_id: FileId,
    range: rowan::TextRange,
) -> Option<LuaTypeDeclId> {
    // Use cached info if available, otherwise detect from path
    let (class_name, global_name, super_types) =
        if let Some(info) = db.get_gmod_infer_index().get_scoped_class_info(&file_id) {
            (
                info.class_name.clone(),
                info.global_name.clone(),
                info.super_types.clone(),
            )
        } else {
            let scope_match = detect_scoped_class_from_path(db, file_id)?;
            (
                scope_match.class_name,
                scope_match.global_name,
                scope_match.super_types,
            )
        };
    Some(ensure_scoped_class_type_decl(
        db,
        file_id,
        &class_name,
        &global_name,
        &super_types,
        range,
    ))
}

/// Resolve scripted_ents.GetMember("class", "method") delegation patterns.
///
/// Detects patterns like:
/// ```lua
/// function ENT:SetupDataTables()
///     local f = scripted_ents.GetMember("target_class", "SetupDataTables")
///     f(self)
/// end
/// ```
///
/// When such a delegation is found, NetworkVar calls from the target entity's
/// metadata are copied into the current entity's metadata so that
/// `synthesize_scripted_class_members` will produce Get/Set members for them.
fn resolve_getmember_network_var_delegations(
    db: &mut DbIndex,
    scripted_scope_files: &HashSet<FileId>,
    context: &AnalyzeContext,
) {
    // Collect files to process: only scripted scope files whose source
    // contains "scripted_ents.GetMember".  Collect into owned structures
    // so we can drop the immutable VFS borrow before mutable db access.
    let candidate_files: Vec<(FileId, LuaChunk, LuaTypeDeclId)> = {
        let vfs = db.get_vfs();
        context
            .tree_list
            .iter()
            .filter(|t| scripted_scope_files.contains(&t.file_id))
            .filter(|t| {
                vfs.get_file_content(&t.file_id)
                    .is_some_and(|c| c.contains("scripted_ents.GetMember"))
            })
            .filter_map(|t| {
                let scope_match = db
                    .get_gmod_infer_index()
                    .get_scoped_class_info(&t.file_id)
                    .cloned()?;
                let class_decl_id = get_scripted_class_type_decl_id(
                    &scope_match.global_name,
                    &scope_match.class_name,
                );
                Some((t.file_id, t.value.clone(), class_decl_id))
            })
            .collect()
    };

    if candidate_files.is_empty() {
        return;
    }

    // Build class_name -> file_ids reverse mapping only when there are
    // delegating files to resolve; this avoids a full VFS scan on ordinary edits.
    let class_file_map = build_class_file_map(db);

    for (file_id, chunk, class_decl_id) in &candidate_files {
        find_and_resolve_getmember_delegations(db, *file_id, class_decl_id, chunk, &class_file_map);
    }
}

/// Build a mapping from class_name to all file ids for known scripted entity classes.
fn build_class_file_map(db: &DbIndex) -> HashMap<String, Vec<FileId>> {
    let mut map = HashMap::default();
    let gmod_infer = db.get_gmod_infer_index();
    let vfs = db.get_vfs();
    let all_file_ids = vfs.get_all_file_ids();

    for file_id in all_file_ids {
        if let Some(info) = gmod_infer.get_scoped_class_info(&file_id) {
            let is_init = vfs
                .get_file_path(&file_id)
                .and_then(|p| p.file_name().and_then(|name| name.to_str()))
                .is_some_and(|name| name == "init.lua");
            let file_ids = map
                .entry(info.class_name.clone())
                .or_insert_with(Vec::<(bool, FileId)>::new);
            file_ids.push((is_init, file_id));
        }
    }

    // The per-class vector is replayed into `add_call`, which dedups on a `LuaSyntaxId` carrying
    // no file id, so the order decides which file's call survives a collision. File-id order is
    // not stable between a cold build and an incremental session; normalized path is.
    map.into_iter()
        .map(|(class_name, mut file_ids)| {
            file_ids
                .sort_by_cached_key(|(is_init, file_id)| (!*is_init, vfs.file_order_key(file_id)));
            (
                class_name,
                file_ids.into_iter().map(|(_, file_id)| file_id).collect(),
            )
        })
        .collect()
}

/// Walk a scripted class file's AST looking for `scripted_ents.GetMember` delegation
/// patterns. When found, copy NetworkVar calls from the target class into this file's
/// metadata.
fn find_and_resolve_getmember_delegations(
    db: &mut DbIndex,
    current_file_id: FileId,
    current_class_decl_id: &LuaTypeDeclId,
    chunk: &LuaChunk,
    class_file_map: &HashMap<String, Vec<FileId>>,
) {
    // Collect local variable names assigned from scripted_ents.GetMember calls.
    // Map: local_name -> (target_class_name, target_method_name)
    let mut getmember_locals: HashMap<String, (String, String)> = HashMap::default();

    for node in chunk.syntax().descendants() {
        // Match: local Name = scripted_ents.GetMember("class", "method")
        if let Some(local_stat) = LuaLocalStat::cast(node.clone()) {
            let local_names: Vec<LuaLocalName> = local_stat.get_local_name_list().collect();
            let value_exprs: Vec<LuaExpr> = local_stat.get_value_exprs().collect();
            for (i, local_name) in local_names.iter().enumerate() {
                let Some(local_name_text) = local_name
                    .get_name_token()
                    .map(|t| t.get_name_text().to_string())
                else {
                    continue;
                };

                let Some(value_expr) = value_exprs.get(i) else {
                    continue;
                };

                let LuaExpr::CallExpr(call_expr) = value_expr else {
                    continue;
                };

                if let Some((target_class, target_method)) =
                    extract_getmember_call(&call_expr, false)
                {
                    getmember_locals.insert(local_name_text, (target_class, target_method));
                }
            }
        }

        // Match: f(self) or f(self, ...) where f is a tracked local
        if let Some(call_expr) = LuaCallExpr::cast(node) {
            let Some(LuaExpr::NameExpr(name_expr)) = call_expr.get_prefix_expr() else {
                continue;
            };
            let Some(caller_name) = name_expr.get_name_text() else {
                continue;
            };

            let Some((target_class, target_method)) = getmember_locals.get(caller_name.as_str())
            else {
                continue;
            };
            if target_method != "SetupDataTables" {
                continue;
            }

            // Verify the first argument is "self"
            let Some(args_list) = call_expr.get_args_list() else {
                continue;
            };
            let first_arg = args_list.get_args().next();
            if !matches!(
                first_arg.as_ref().map(|a| a.syntax().text()),
                Some(t) if t == "self"
            ) {
                continue;
            };

            // Also check as a statement: f(self) as a statement
            // Actually the descendant walk will hit both LuaCallExpr and
            // LuaCallExprStat, and the LuaCallExpr inside a LuaCallExprStat
            // will match either way.

            // Look up the target class
            if let Some(target_file_ids) = class_file_map.get(target_class) {
                copy_network_var_calls_from(
                    db,
                    current_file_id,
                    current_class_decl_id,
                    target_file_ids,
                );
            }
        }
    }

    // Also check direct calls: scripted_ents.GetMember("class", "method")(self)
    for node in chunk.descendants::<LuaCallExpr>() {
        let Some(LuaExpr::CallExpr(inner_call)) = node.get_prefix_expr() else {
            continue;
        };
        if let Some((target_class, target_method)) = extract_getmember_call(&inner_call, true) {
            if target_method != "SetupDataTables" {
                continue;
            }

            let Some(args_list) = node.get_args_list() else {
                continue;
            };
            let first_arg = args_list.get_args().next();
            if !matches!(
                first_arg.as_ref().map(|a| a.syntax().text()),
                Some(t) if t == "self"
            ) {
                continue;
            };

            if let Some(target_file_ids) = class_file_map.get(&target_class) {
                copy_network_var_calls_from(
                    db,
                    current_file_id,
                    current_class_decl_id,
                    target_file_ids,
                );
            }
        }
    }
}

/// Extract (class_name, method_name) from a `scripted_ents.GetMember` call expression.
/// `reject_parenthesized` controls whether parenthesized calls are rejected.
fn extract_getmember_call(
    call_expr: &LuaCallExpr,
    reject_parenthesized: bool,
) -> Option<(String, String)> {
    let prefix_expr = call_expr.get_prefix_expr()?;

    // Check for parenthesized: (scripted_ents.GetMember)(...)
    if let LuaExpr::ParenExpr(paren_expr) = &prefix_expr {
        let inner = paren_expr.get_expr()?;
        if !matches!(inner, LuaExpr::IndexExpr(_)) {
            return None;
        }
        if reject_parenthesized {
            return None;
        }
    }

    let index_expr = match &prefix_expr {
        LuaExpr::IndexExpr(idx) => idx.clone(),
        LuaExpr::ParenExpr(paren) => {
            let inner = paren.get_expr()?;
            if let LuaExpr::IndexExpr(idx) = inner {
                idx.clone()
            } else {
                return None;
            }
        }
        _ => return None,
    };

    // Check the index key is "GetMember"
    let key_match = match index_expr.get_index_key() {
        Some(LuaIndexKey::Name(name_token)) => name_token.get_name_text() == "GetMember",
        Some(LuaIndexKey::String(string_token)) => string_token.get_value() == "GetMember",
        _ => false,
    };
    if !key_match {
        return None;
    }

    // Check the prefix is "scripted_ents"
    let prefix_match = match index_expr.get_prefix_expr() {
        Some(LuaExpr::NameExpr(name_expr)) => {
            name_expr.get_name_text().as_deref() == Some("scripted_ents")
        }
        _ => false,
    };
    if !prefix_match {
        return None;
    }

    // Extract string literal arguments
    let args_list = call_expr.get_args_list()?;
    let args: Vec<LuaExpr> = args_list.get_args().collect();
    let class_name = extract_string_literal(args.first()?)?;
    let method_name = extract_string_literal(args.get(1)?)?;

    Some((class_name, method_name))
}

/// Extract a string literal value from an expression, supporting parenthesized literals.
fn extract_string_literal(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::LiteralExpr(literal) => match literal.get_literal() {
            Some(LuaLiteralToken::String(s)) => Some(s.get_value().to_string()),
            _ => None,
        },
        LuaExpr::ParenExpr(paren) => {
            let inner = paren.get_expr()?;
            extract_string_literal(&inner)
        }
        _ => None,
    }
}

/// Copy NetworkVar and NetworkVarElement calls from the target entity's metadata
/// into the current entity's metadata so they get synthesized as Get/Set members.
fn copy_network_var_calls_from(
    db: &mut DbIndex,
    current_file_id: FileId,
    _current_class_decl_id: &LuaTypeDeclId,
    target_file_ids: &[FileId],
) {
    let target_metadata: Vec<_> = target_file_ids
        .iter()
        .filter_map(|target_file_id| {
            db.get_gmod_class_metadata_index()
                .get_file_metadata(target_file_id)
                .cloned()
        })
        .collect();
    if target_metadata.is_empty() {
        return;
    }

    let metadata_index = db.get_gmod_class_metadata_index_mut();

    for target_metadata in &target_metadata {
        for nv_call in &target_metadata.network_var_calls {
            metadata_index.add_call(
                current_file_id,
                GmodScriptedClassCallKind::NetworkVar,
                nv_call.clone(),
            );
        }

        for nve_call in &target_metadata.network_var_element_calls {
            metadata_index.add_call(
                current_file_id,
                GmodScriptedClassCallKind::NetworkVarElement,
                nve_call.clone(),
            );
        }
    }
}

/// Synthesize typed members from AccessorFunc, NetworkVar, and DEFINE_BASECLASS
/// calls for all files that have scripted class metadata.
fn synthesize_scripted_class_members(
    db: &mut DbIndex,
    scripted_scope_files: &HashSet<FileId>,
    file_ids: &[FileId],
) {
    for file_id in file_ids.iter().copied() {
        // Use cached scoped class info (computed during gmod_pre phase)
        let scope_match = if scripted_scope_files.contains(&file_id) {
            db.get_gmod_infer_index()
                .get_scoped_class_info(&file_id)
                .cloned()
        } else {
            None
        };

        let metadata = match db
            .get_gmod_class_metadata_index()
            .get_file_metadata(&file_id)
        {
            Some(m) => m.clone(),
            None => continue,
        };

        if let Some(ref scope_match) = scope_match {
            let class_decl_id =
                get_scripted_class_type_decl_id(&scope_match.global_name, &scope_match.class_name);
            if let Some((effective_base_name, is_derive, source_syntax_id)) =
                resolve_effective_inheritance_base(
                    &metadata,
                    scope_match.class_name_prefix.as_deref(),
                )
            {
                add_data_table_super_type(
                    db,
                    file_id,
                    &class_decl_id,
                    &LuaTypeDeclId::global(&effective_base_name),
                    source_syntax_id.get_range(),
                );
                synthesize_inheritance_base(
                    db,
                    file_id,
                    &class_decl_id,
                    &effective_base_name,
                    is_derive,
                    scope_match.class_name_prefix.as_deref(),
                    source_syntax_id,
                );
            }
            if let Some(effective_call) =
                metadata.define_baseclass_calls.iter().rev().find(|call| {
                    valid_inheritance_literal(call)
                        && define_baseclass_targets_scope_class(
                            call,
                            scope_match.class_name_prefix.as_deref(),
                        )
                })
            {
                synthesize_define_baseclass_parent_alias(
                    db,
                    file_id,
                    &class_decl_id,
                    scope_match.class_name_prefix.as_deref(),
                    effective_call,
                );
            }
        }

        // AccessorFunc/NetworkVar/NetworkVarElement: synthesize Get/Set/field
        // members — but only for calls that actually target the scope's
        // authoring table. A scoped file can author other scripted tables
        // (`AccessorFunc(PANEL, ...)` in a gamemode derma file belongs to the
        // panel, not to `gamemode_*`).
        if let Some(ref scope_match) = scope_match {
            let class_decl_id =
                get_scripted_class_type_decl_id(&scope_match.global_name, &scope_match.class_name);
            for call in &metadata.accessor_func_calls {
                if scripted_call_targets_scope_class(db, file_id, scope_match, call) {
                    synthesize_accessor_func(db, file_id, &class_decl_id, call);
                }
            }
            for call in &metadata.network_var_calls {
                if scripted_call_targets_scope_class(db, file_id, scope_match, call) {
                    synthesize_network_var(db, file_id, &class_decl_id, call);
                }
            }
            for call in &metadata.network_var_element_calls {
                if scripted_call_targets_scope_class(db, file_id, scope_match, call) {
                    synthesize_network_var_element(db, file_id, &class_decl_id, call);
                }
            }
        }
    }
}

/// Whether a scripted-class helper call (`AccessorFunc`, `NetworkVar`,
/// `NetworkVarElement`) targets the file's scope class. These helpers can
/// define members on any table — `AccessorFunc(PANEL, ...)` inside a
/// gamemode file authors the panel the file registers, not `GM` — so a call
/// only applies to the scope class when its target resolves to the scope's
/// authoring table (`GM`, `GAMEMODE`, aliases, or `self` inside a `GM:`
/// method).
///
/// The target is the receiver of a `:` call (`self:NetworkVar(...)` targets
/// `self`, whatever table it is — annotated wrappers invoked with `:` behave
/// the same way). For a plain call the target is arg0 when the callee is an
/// accessor generator whose accessors land on the table passed first
/// (`AccessorFunc` and `@accessorfunc` wrappers); any other callee, such as
/// an annotated network-var wrapper (`addNetworkVar("Enabled", "Bool")`),
/// implicitly authors the file's scripted class and carries no target
/// argument.
///
/// When the target cannot be resolved — including calls copied in from
/// another file's metadata by `copy_network_var_calls_from`, whose
/// `syntax_id` does not exist in this file's tree — the call is kept.
fn scripted_call_targets_scope_class(
    db: &DbIndex,
    file_id: FileId,
    scope_match: &GmodScopedClassInfo,
    call: &GmodScriptedClassCallMetadata,
) -> bool {
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_red_root())
    else {
        return true;
    };
    let Some(call_expr) = call
        .syntax_id
        .to_node_from_root(&root)
        .and_then(LuaCallExpr::cast)
        .filter(|expr| expr.get_range() == call.syntax_id.get_range())
    else {
        return true;
    };

    let target_expr = if call_expr.is_colon_call() {
        // `x:Helper(...)`: the receiver is the target table.
        match call_expr.get_prefix_expr() {
            Some(LuaExpr::IndexExpr(index_expr)) => index_expr.get_prefix_expr(),
            _ => None,
        }
    } else {
        match call_expr.get_prefix_expr() {
            // `Helper(target, ...)`: arg0 names the target table when the
            // callee's accessors land on the table passed first.
            Some(prefix @ LuaExpr::NameExpr(_))
                if accessors_take_target_first(db, file_id, &prefix) =>
            {
                call.args
                    .first()
                    .and_then(|arg| arg.syntax_id.to_node_from_root(&root))
                    .and_then(LuaExpr::cast)
            }
            _ => return true,
        }
    };

    match target_expr {
        Some(LuaExpr::NameExpr(name_expr)) => {
            let Some(name) = name_expr.get_name_text() else {
                return true;
            };
            if &*name == "self" {
                // `self` targets the enclosing method's table — the scope
                // class only when that method is defined on its authoring
                // global.
                scope_authoring_names(scope_match).iter().any(|var_name| {
                    member_defined_via_variable(
                        db,
                        file_id,
                        call.syntax_id.get_range().start(),
                        var_name,
                    )
                })
            } else {
                name_expr_resolves_to_scoped_authoring_table(db, file_id, &name_expr).is_some()
            }
        }
        // A resolvable non-name target (e.g. `lib.GetTable():NetworkVar`) is
        // never the scope's authoring table.
        Some(_) => false,
        // No target expression at all — keep the member.
        None => true,
    }
}

/// Whether a plain call through `prefix` reaches an accessor generator whose
/// accessors land on the table it is passed first.
fn accessors_take_target_first(db: &DbIndex, file_id: FileId, prefix: &LuaExpr) -> bool {
    let LuaExpr::NameExpr(name_expr) = prefix else {
        return false;
    };
    name_expr.get_name_text().is_some_and(|name| {
        crate::compilation::analyzer::lua::call::accessor_generator(db, file_id, prefix, &name)
            .is_some_and(|generator| {
                generator
                    .accessors
                    .iter()
                    .any(|template| template.owner == crate::AccessorOwner::Receiver)
            })
    })
}

/// The globals a scoped file's `self`/name references author members on:
/// the scope's class global plus its aliases (and `GAMEMODE` for gamemodes,
/// mirroring `resolve_scoped_authoring_type`).
fn scope_authoring_names(scope_match: &GmodScopedClassInfo) -> Vec<&str> {
    let mut names = Vec::with_capacity(scope_match.aliases.len() + 2);
    names.push(scope_match.global_name.as_str());
    names.extend(scope_match.aliases.iter().map(String::as_str));
    if scope_match.global_name == "GM" && !names.contains(&"GAMEMODE") {
        names.push("GAMEMODE");
    }
    names
}

/// Synthesize vgui.Register / derma.DefineControl class types.
#[derive(Clone, Copy)]
struct ResolvedVguiRegistrationRegion {
    decl_id: LuaDeclId,
    region_start: TextSize,
}

#[derive(Default)]
struct VguiSynthesisCache {
    registered_table_exprs: HashMap<(FileId, u32), Option<LuaTableExpr>>,
    decl_has_reassignment: HashMap<(FileId, u32), bool>,
    initializer_table_ranges: HashMap<(FileId, u32), Option<InFiled<TextRange>>>,
    table_const_replacements: HashMap<InFiled<TextRange>, LuaType>,
}

/// Which half of a vgui registration a pass synthesizes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum VguiSynthesisPhase {
    /// The class, its supers and the table's binding to it, which the file's
    /// statements read: produced before them.
    Bind,
    /// The table's members moved onto the class, which the statements
    /// produce: moved after them.
    TransferMembers,
}

fn synthesize_vgui_registrations(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    file_ids: &[FileId],
    phase: VguiSynthesisPhase,
) {
    struct VguiRegistrationRegion {
        file_id: FileId,
        decl_id: LuaDeclId,
        class_decl_id: LuaTypeDeclId,
        region_start: TextSize,
        region_end: TextSize,
    }

    let mut vgui_registration_regions: Vec<VguiRegistrationRegion> = Vec::new();
    let mut synthesis_cache = VguiSynthesisCache::default();
    // Tracks local table regions that have already been registered via
    // `vgui.RegisterTable` so that subsequent `vgui.CreateFromTable` calls
    // referencing the same region do not trigger a second class synthesis.
    let mut registered_table_regions: HashSet<(LuaDeclId, TextSize)> = HashSet::default();

    for file_id in file_ids.iter().copied() {
        // Borrow first and skip files with no VGUI-relevant calls before paying
        // for the (multi-Vec) metadata clone. The vast majority of files have
        // class metadata but no VGUI register/derma calls.
        let has_vgui_work = match db
            .get_gmod_class_metadata_index()
            .get_file_metadata(&file_id)
        {
            Some(m) => {
                !m.vgui_register_calls.is_empty()
                    || !m.vgui_register_table_calls.is_empty()
                    || !m.derma_define_control_calls.is_empty()
                    || !m.vgui_register_file_calls.is_empty()
            }
            None => continue,
        };
        if !has_vgui_work {
            continue;
        }
        let metadata = db
            .get_gmod_class_metadata_index()
            .get_file_metadata(&file_id)
            .expect("metadata present (checked above)")
            .clone();

        for call in &metadata.vgui_register_calls {
            let register_position = call.syntax_id.get_range().start();
            let panel_source = call.vgui_panel_define_arg_source();
            let table_source = call.vgui_panel_table_arg_source(1);
            let panel_name = resolve_vgui_registration_name(
                db,
                context.infer_manager.get_infer_cache(file_id),
                file_id,
                call,
                &panel_source,
            );
            let mut resolved_registration = None;
            if let Some(panel_name) = panel_name.as_deref() {
                if let Some(GmodClassCallLiteral::NameRef(table_var)) =
                    call.value_for_arg_source(&table_source)
                    && let Some((decl_id, region_start)) =
                        resolve_local_registration_region(db, file_id, table_var, register_position)
                {
                    resolved_registration = Some(ResolvedVguiRegistrationRegion {
                        decl_id,
                        region_start,
                    });
                    vgui_registration_regions.push(VguiRegistrationRegion {
                        file_id,
                        decl_id,
                        class_decl_id: LuaTypeDeclId::global(panel_name),
                        region_start,
                        region_end: register_position,
                    });
                }
                synthesize_vgui_register(
                    db,
                    &mut synthesis_cache,
                    file_id,
                    call,
                    panel_name,
                    resolved_registration,
                    phase,
                );
            }
        }

        let actual_register_table_positions: HashSet<_> = metadata
            .vgui_register_table_calls
            .iter()
            .filter(|call| is_vgui_register_table_call(db, file_id, call))
            .map(|call| call.syntax_id.get_range().start())
            .collect();

        for call in &metadata.vgui_register_table_calls {
            let register_position = call.syntax_id.get_range().start();
            let table_source = call.vgui_panel_table_arg_source(0);
            let is_actual_registration =
                actual_register_table_positions.contains(&register_position);
            if !is_actual_registration
                && vgui_table_arg_is_registered_result(
                    db,
                    file_id,
                    call,
                    &table_source,
                    register_position,
                    &actual_register_table_positions,
                )
            {
                continue;
            }
            let mut resolved_registration = None;
            if let Some(GmodClassCallLiteral::NameRef(table_var)) =
                call.value_for_arg_source(&table_source)
                && let Some((decl_id, region_start)) =
                    resolve_local_registration_region(db, file_id, table_var, register_position)
            {
                // Skip synthesis when this table is already registered via a
                // prior `vgui.RegisterTable` call. `vgui.CreateFromTable` uses
                // the same `register_table` call_arg kind, which means it also
                // lands in `vgui_register_table_calls`. Without this guard, the
                // `CreateFromTable` call synthesizes a SECOND class at its own
                // position, overwriting the first registration's binding and
                // producing false-positive `undefined-field` /
                // `unchecked-nil-access` on the original panel's `self.Field`
                // accesses.
                //
                // Only actual `vgui.RegisterTable` calls populate the dedup
                // set. A `CreateFromTable` call that appears before the real
                // `RegisterTable` must not insert a key, otherwise the later
                // `RegisterTable` can lose its base/type synthesis. The key is
                // region-specific so reused locals can register later table
                // regions without being blocked by earlier registrations.
                let registration_key = (decl_id, region_start);
                if registered_table_regions.contains(&registration_key) {
                    continue;
                }
                resolved_registration = Some(ResolvedVguiRegistrationRegion {
                    decl_id,
                    region_start,
                });
                let class_decl_id = vgui_register_table_type_decl_id(file_id, call);
                vgui_registration_regions.push(VguiRegistrationRegion {
                    file_id,
                    decl_id,
                    class_decl_id,
                    region_start,
                    region_end: register_position,
                });
                if is_actual_registration {
                    registered_table_regions.insert(registration_key);
                }
            }
            synthesize_vgui_register_table(
                db,
                &mut synthesis_cache,
                file_id,
                call,
                resolved_registration,
                phase,
            );
        }

        for call in &metadata.derma_define_control_calls {
            let register_position = call.syntax_id.get_range().start();
            let panel_source = call.vgui_panel_define_arg_source();
            let table_source = call.vgui_panel_table_arg_source(2);
            let panel_name = resolve_vgui_registration_name(
                db,
                context.infer_manager.get_infer_cache(file_id),
                file_id,
                call,
                &panel_source,
            );
            let mut resolved_registration = None;
            if let Some(panel_name) = panel_name.as_deref() {
                if let Some(GmodClassCallLiteral::NameRef(table_var)) =
                    call.value_for_arg_source(&table_source)
                    && let Some((decl_id, region_start)) =
                        resolve_local_registration_region(db, file_id, table_var, register_position)
                {
                    resolved_registration = Some(ResolvedVguiRegistrationRegion {
                        decl_id,
                        region_start,
                    });
                    vgui_registration_regions.push(VguiRegistrationRegion {
                        file_id,
                        decl_id,
                        class_decl_id: LuaTypeDeclId::global(panel_name),
                        region_start,
                        region_end: register_position,
                    });
                }
                synthesize_derma_define_control(
                    db,
                    &mut synthesis_cache,
                    file_id,
                    call,
                    panel_name,
                    resolved_registration,
                    phase,
                );
            }
        }

        if phase == VguiSynthesisPhase::TransferMembers {
            continue;
        }
        for call in &metadata.vgui_register_file_calls {
            if let Some((
                target_file_id,
                decl_id,
                class_decl_id,
                _panel_name,
                region_start,
                region_end,
            )) = synthesize_vgui_register_file_target(db, file_id, call)
            {
                vgui_registration_regions.push(VguiRegistrationRegion {
                    file_id: target_file_id,
                    decl_id,
                    class_decl_id,
                    region_start,
                    region_end,
                });
            }
        }
    }

    if phase == VguiSynthesisPhase::TransferMembers {
        return;
    }
    flush_vgui_table_const_replacements(db, &mut synthesis_cache);

    // Synthesize AccessorFunc members for VGUI-registered classes. Group by
    // file so each accessor target is resolved once instead of once per
    // registration in that file.
    let mut registrations_by_file: HashMap<FileId, Vec<&VguiRegistrationRegion>> =
        HashMap::default();
    for registration in &vgui_registration_regions {
        registrations_by_file
            .entry(registration.file_id)
            .or_default()
            .push(registration);
    }

    for (file_id, registrations) in registrations_by_file {
        let metadata = match db
            .get_gmod_class_metadata_index()
            .get_file_metadata(&file_id)
        {
            Some(m) if !m.accessor_func_calls.is_empty() => m.clone(),
            _ => continue,
        };

        log::debug!(
            "VGUI AccessorFunc: file {:?} has {} accessor_func_calls across {} registrations",
            file_id,
            metadata.accessor_func_calls.len(),
            registrations.len(),
        );

        for call in &metadata.accessor_func_calls {
            let Some(Some(GmodClassCallLiteral::NameRef(target_name))) = call.literal_args.first()
            else {
                continue;
            };
            let Some(target_arg) = call.args.first() else {
                continue;
            };

            let accessor_position = call.syntax_id.get_range().start();
            let target_decl_id = resolve_local_decl_id_at_position(
                db,
                file_id,
                target_name,
                target_arg.syntax_id.get_range().start(),
            );

            for registration in &registrations {
                if accessor_position < registration.region_start
                    || accessor_position >= registration.region_end
                {
                    continue;
                }

                let matches_registration_target = target_decl_id == Some(registration.decl_id)
                    || (target_decl_id.is_none()
                        && target_name == "PANEL"
                        && registration.decl_id.file_id == file_id);

                if matches_registration_target {
                    synthesize_accessor_func(db, file_id, &registration.class_decl_id, call);
                }
            }
        }
    }
}

fn flush_vgui_table_const_replacements(db: &mut DbIndex, cache: &mut VguiSynthesisCache) {
    if cache.table_const_replacements.is_empty() {
        return;
    }

    let mut replacements = std::mem::take(&mut cache.table_const_replacements)
        .into_iter()
        .collect::<Vec<_>>();
    replacements.sort_by_key(|(table, _)| (table.file_id, table.value.start(), table.value.end()));
    for (table, class_type) in replacements {
        db.get_type_index_mut()
            .register_table_class(table, class_type);
    }
}

fn resolve_vgui_registration_name(
    db: &DbIndex,
    infer_cache: &mut LuaInferCache,
    file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
    source: &GmodClassCallArgSource,
) -> Option<String> {
    if let Some(GmodClassCallLiteral::String(name)) = call.value_for_arg_source(source) {
        return (!name.is_empty()).then(|| name.clone());
    }

    let syntax_id = if !source.is_derived() {
        call.args.get(source.arg_idx)?.syntax_id
    } else {
        call.field_args
            .iter()
            .find(|arg| &arg.source == source)?
            .syntax_id
    };
    let root = db.get_vfs().get_syntax_tree(&file_id)?.get_red_root();
    let expr = syntax_id.to_node_from_root(&root).and_then(LuaExpr::cast)?;
    match infer_expr(db, infer_cache, expr).ok()? {
        LuaType::StringConst(name) if !name.is_empty() => Some(name.to_string()),
        _ => None,
    }
}

fn synthesize_scripted_ent_registrations(db: &mut DbIndex, file_ids: &[FileId]) {
    for file_id in file_ids.iter().copied() {
        let metadata = match db
            .get_gmod_class_metadata_index()
            .get_file_metadata(&file_id)
        {
            Some(metadata) => metadata.clone(),
            None => continue,
        };

        for (kind, calls) in [
            (
                GmodScriptedClassCallKind::ScriptedEntRegister,
                &metadata.scripted_ent_register_calls,
            ),
            (
                GmodScriptedClassCallKind::ScriptedWeaponRegister,
                &metadata.scripted_weapon_register_calls,
            ),
        ] {
            for call in calls {
                synthesize_scripted_class_registration(db, file_id, &metadata, call, kind);
            }
        }
    }
}

fn synthesize_scripted_class_registration(
    db: &mut DbIndex,
    file_id: FileId,
    metadata: &GmodScriptedClassFileMetadata,
    call: &GmodScriptedClassCallMetadata,
    kind: GmodScriptedClassCallKind,
) {
    let Some(class_name) = call
        .literal_args
        .get(call.register_name_arg_idx())
        .and_then(|arg| arg.as_ref())
        .and_then(|arg| match arg {
            GmodClassCallLiteral::String(name) if !name.is_empty() => Some(name.as_str()),
            _ => None,
        })
    else {
        return;
    };

    let class_decl_id = LuaTypeDeclId::global(class_name);
    // Every registering site adds its location unless a doc tag declares the
    // class: whichever registration ran first must not own the declaration.
    if db
        .get_type_index()
        .get_source_type_decl(&class_decl_id)
        .is_none()
    {
        db.get_type_index_mut().add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                call.syntax_id.get_range(),
                class_decl_id.get_simple_name().to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::None.into(),
                class_decl_id.clone(),
            ),
        );
    }

    let register_position = call.syntax_id.get_range().start();
    let class_type = LuaType::Def(class_decl_id.clone());

    let table_arg_idx = call.register_table_arg_idx();
    let (registered_table, region_start, decl_id) = match call
        .literal_args
        .get(table_arg_idx)
        .and_then(|arg| arg.as_ref())
    {
        Some(GmodClassCallLiteral::NameRef(var_name)) => {
            if db
                .get_gmod_infer_index()
                .get_scoped_class_info(&file_id)
                .is_some_and(|info| info.global_name == var_name.as_str())
            {
                return;
            }
            let Some((decl_id, region_start)) =
                resolve_local_registration_region(db, file_id, var_name, register_position)
            else {
                return;
            };
            (
                find_registered_table_expr_at_write_position(db, file_id, region_start),
                region_start,
                Some(decl_id),
            )
        }
        _ => (
            find_table_expr_for_arg_source(
                db,
                file_id,
                call,
                &GmodClassCallArgSource::direct(table_arg_idx),
            ),
            TextSize::new(0),
            None,
        ),
    };

    let Some(table_expr) = registered_table else {
        return;
    };

    let table_range = InFiled::new(file_id, table_expr.get_range());
    let table_syntax_owner =
        LuaTypeOwner::SyntaxId(InFiled::new(file_id, table_expr.get_syntax_id()));
    let preserve_doc = db
        .get_type_index()
        .get_type_cache(&table_syntax_owner)
        .is_some_and(|cache| cache.is_doc());
    if !preserve_doc {
        write_type_cache(
            db,
            table_syntax_owner,
            LuaTypeCache::InferType(class_type.clone()),
            TypeCacheWriteMode::ForceOverwrite,
        );
    }

    if let Some(decl_id) = decl_id
        && !decl_has_reassignment(db, file_id, decl_id)
    {
        write_type_cache(
            db,
            decl_id.into(),
            LuaTypeCache::InferType(class_type.clone()),
            TypeCacheWriteMode::ForceOverwrite,
        );
    }

    let source_owner = LuaMemberOwner::Element(table_range.clone());
    let class_member_owner = LuaMemberOwner::Type(class_decl_id.clone());
    let table_member_ids: Vec<_> = db
        .get_member_index()
        .get_members(&source_owner)
        .map(|members| {
            members
                .iter()
                .filter(|member| member.get_key().get_name() != Some("BaseClass"))
                .map(|member| member.get_id())
                .collect()
        })
        .unwrap_or_default();
    for member_id in table_member_ids {
        db.get_member_index_mut()
            .transfer_member(class_member_owner.clone(), member_id, false);
    }

    db.get_type_index_mut()
        .register_table_class(table_range.clone(), class_type.clone());

    let entity_type = (kind == GmodScriptedClassCallKind::ScriptedEntRegister)
        .then(|| registered_table_string_field(db, &source_owner, "Type", register_position))
        .flatten();
    let base = resolve_registered_class_base(
        db,
        &source_owner,
        metadata,
        register_position,
        kind,
        call.syntax_id.get_range(),
        entity_type.as_ref(),
    );
    if let Some((base_name, source_range)) = base {
        let super_type = LuaType::Ref(LuaTypeDeclId::global(&base_name));
        if super_type != class_type {
            add_data_table_super_type(
                db,
                file_id,
                &class_decl_id,
                &LuaTypeDeclId::global(&base_name),
                source_range,
            );
            db.get_type_index_mut().add_super_type(
                class_decl_id.clone(),
                file_id,
                source_range,
                super_type,
            );
        }
        synthesize_baseclass_member(db, file_id, &class_decl_id, &base_name, call.syntax_id);
    }

    // Inject extra super-types based on `ENT.Type`. The `Type` field selects
    // the engine-side entity framework (e.g. `"nextbot"` provides `NextBot`
    // methods like `StartActivity`, `loco`, `MoveToPos` via C++ metatable
    // injection). Without this, `self:StartActivity()` on a `base_nextbot`
    // entity produces false-positive `undefined-field` diagnostics because
    // the synthesized `base_nextbot` class doesn't inherit from `NextBot`.
    if let Some((type_name, source_range)) = entity_type
        && let Some(super_name) = super_type_for_entity_type(&type_name)
    {
        let super_type = LuaType::Ref(LuaTypeDeclId::global(&super_name));
        if super_type != class_type {
            db.get_type_index_mut().add_super_type(
                class_decl_id.clone(),
                file_id,
                source_range,
                super_type,
            );
        }
    }

    synthesize_registered_scripted_ent_region_members(
        db,
        file_id,
        &class_decl_id,
        metadata,
        region_start,
        register_position,
    );
}

/// The base a registration call gives the table: its `Base`, the last
/// `DEFINE_BASECLASS` before the call, or the default of its kind (a weapon's
/// `weapon_base`, an entity's base by its `Type`).
fn resolve_registered_class_base(
    db: &DbIndex,
    table_owner: &LuaMemberOwner,
    metadata: &GmodScriptedClassFileMetadata,
    register_position: TextSize,
    kind: GmodScriptedClassCallKind,
    register_range: TextRange,
    entity_type: Option<&(String, TextRange)>,
) -> Option<(String, TextRange)> {
    if let Some(base) = registered_table_string_field(db, table_owner, "Base", register_position) {
        return Some(base);
    }

    metadata
        .define_baseclass_calls
        .iter()
        .rev()
        .find(|call| call.syntax_id.get_range().start() < register_position)
        .and_then(
            |call| match call.literal_args.get(call.inheritance_name_arg_idx()) {
                Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
                    Some((name.clone(), call.syntax_id.get_range()))
                }
                _ => None,
            },
        )
        .or_else(|| {
            if kind == GmodScriptedClassCallKind::ScriptedWeaponRegister {
                return Some(("weapon_base".to_string(), register_range));
            }
            let (type_name, source_range) = entity_type?;
            default_base_for_entity_type(type_name).map(|base| (base.to_string(), *source_range))
        })
}

/// The name a registered table holds under `field` when
/// `scripted_ents.Register` reads it: the last write before the call, a
/// constructor field or an assignment.
fn registered_table_string_field(
    db: &DbIndex,
    table_owner: &LuaMemberOwner,
    field: &str,
    register_position: TextSize,
) -> Option<(String, TextRange)> {
    let member_id = db
        .get_member_index()
        .get_members(table_owner)?
        .iter()
        .filter(|member| member.get_key().get_name() == Some(field))
        .map(|member| member.get_id())
        .filter(|member_id| member_id.get_position() < register_position)
        .max_by_key(|member_id| member_id.get_position())?;
    let value_expr = crate::get_member_value_expr(db, member_id)?;
    extract_scoped_base_name(&value_expr).map(|name| (name, member_id.get_syntax_id().get_range()))
}

/// The base `scripted_ents.Register` falls back to for a table without
/// `Base`, by its `Type`.
fn default_base_for_entity_type(type_name: &str) -> Option<&'static str> {
    match type_name {
        "anim" => Some("base_anim"),
        "point" => Some("base_point"),
        "brush" => Some("base_brush"),
        "filter" => Some("base_filter"),
        _ => None,
    }
}

/// Maps `ENT.Type` values to the annotation class that provides the
/// engine-side framework methods. C++ metatable injection makes these
/// methods available at runtime; we model them via super-types.
fn super_type_for_entity_type(type_name: &str) -> Option<&'static str> {
    match type_name {
        "nextbot" => Some("NextBot"),
        _ => None,
    }
}

fn synthesize_registered_scripted_ent_region_members(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    metadata: &GmodScriptedClassFileMetadata,
    region_start: TextSize,
    register_position: TextSize,
) {
    let in_region = |call: &GmodScriptedClassCallMetadata| {
        let position = call.syntax_id.get_range().start();
        position >= region_start && position < register_position
    };

    for call in metadata
        .accessor_func_calls
        .iter()
        .filter(|call| in_region(call))
    {
        synthesize_accessor_func(db, file_id, class_decl_id, call);
    }

    for call in metadata
        .network_var_calls
        .iter()
        .filter(|call| in_region(call))
    {
        synthesize_network_var(db, file_id, class_decl_id, call);
    }

    for call in metadata
        .network_var_element_calls
        .iter()
        .filter(|call| in_region(call))
    {
        synthesize_network_var_element(db, file_id, class_decl_id, call);
    }
}

fn resolve_local_registration_region(
    db: &DbIndex,
    file_id: FileId,
    var_name: &str,
    register_position: TextSize,
) -> Option<(LuaDeclId, TextSize)> {
    let decl_id = resolve_local_decl_id_at_position(db, file_id, var_name, register_position)?;
    let region_start =
        find_latest_decl_write_before_position(db, file_id, decl_id, register_position)
            .unwrap_or(decl_id.position);
    Some((decl_id, region_start))
}

fn resolve_local_decl_id_at_position(
    db: &DbIndex,
    file_id: FileId,
    var_name: &str,
    position: TextSize,
) -> Option<LuaDeclId> {
    db.get_decl_index()
        .get_decl_tree(&file_id)?
        .find_local_decl(var_name, position)
        .map(|decl| decl.get_id())
}

fn text_size_key(position: TextSize) -> u32 {
    u32::from(position)
}

fn cached_registered_table_expr(
    cache: &mut VguiSynthesisCache,
    db: &DbIndex,
    file_id: FileId,
    write_position: TextSize,
) -> Option<LuaTableExpr> {
    let key = (file_id, text_size_key(write_position));
    if let Some(table_expr) = cache.registered_table_exprs.get(&key) {
        return table_expr.clone();
    }

    let table_expr = find_registered_table_expr_at_write_position(db, file_id, write_position);
    cache.registered_table_exprs.insert(key, table_expr.clone());
    table_expr
}

fn cached_decl_has_reassignment(
    cache: &mut VguiSynthesisCache,
    db: &DbIndex,
    file_id: FileId,
    decl_id: LuaDeclId,
) -> bool {
    let key = (file_id, text_size_key(decl_id.position));
    if let Some(has_reassignment) = cache.decl_has_reassignment.get(&key) {
        return *has_reassignment;
    }

    let has_reassignment = decl_has_reassignment(db, file_id, decl_id);
    cache.decl_has_reassignment.insert(key, has_reassignment);
    has_reassignment
}

fn synthesize_scoped_base_assignments_with(
    db: &mut DbIndex,
    file_id: FileId,
    root_range: TextRange,
    scope_match: &GmodScopedClassMatch,
    assignments: &[ScopedBaseAssignment],
) {
    let class_decl_id = ensure_scoped_class_type_decl(
        db,
        file_id,
        &scope_match.class_name,
        &scope_match.global_name,
        &scope_match.super_types,
        root_range,
    );
    for assignment in assignments {
        match assignment {
            ScopedBaseAssignment::Base {
                name,
                range,
                syntax_id,
            } => {
                add_scoped_super_type_if_missing(db, &class_decl_id, file_id, *range, name);
                synthesize_baseclass_member(db, file_id, &class_decl_id, name, *syntax_id);
            }
            ScopedBaseAssignment::EntityType { super_name, range } => {
                add_scoped_super_type_if_missing(db, &class_decl_id, file_id, *range, super_name);
            }
        }
    }
}

/// The `<global>.Base` assignments of a scripted-scope file, and for `ENT` its
/// `ENT.Type` assignments, in source order.
fn collect_scoped_base_assignments(
    root: &LuaChunk,
    global_name: &str,
) -> Vec<ScopedBaseAssignment> {
    let expected_base_path = format!("{global_name}.Base");

    // ENT.Type selects the engine-side entity framework (e.g. "nextbot"
    // provides NextBot methods). Only entities use this field - SWEP, TOOL,
    // PLAYER, etc. have their own Type field with different semantics.
    let expected_type_path = (global_name == "ENT").then(|| format!("{global_name}.Type"));

    let mut assignments = Vec::new();
    for assign_stat in root.descendants::<LuaAssignStat>() {
        let (vars, exprs) = assign_stat.get_var_and_expr_list();
        for (idx, var) in vars.into_iter().enumerate() {
            let Some(value_expr) = exprs.get(idx) else {
                continue;
            };

            let Some(access_path) = var.get_access_path() else {
                continue;
            };

            if access_path.eq_ignore_ascii_case(&expected_base_path) {
                let Some(name) = extract_scoped_base_name(value_expr) else {
                    continue;
                };
                assignments.push(ScopedBaseAssignment::Base {
                    name,
                    range: value_expr.get_range(),
                    syntax_id: value_expr.get_syntax_id(),
                });
            } else if let Some(ref type_path) = expected_type_path
                && access_path.eq_ignore_ascii_case(type_path)
            {
                // ENT.Type = "nextbot" injects NextBot as a super-type so
                // engine-side framework methods (StartActivity, loco, etc.)
                // are visible on the synthesized class.
                let Some(type_name) = extract_scoped_base_name(value_expr) else {
                    continue;
                };
                let Some(super_name) = super_type_for_entity_type(&type_name) else {
                    continue;
                };
                assignments.push(ScopedBaseAssignment::EntityType {
                    super_name,
                    range: value_expr.get_range(),
                });
            }
        }
    }
    assignments
}

/// Adds `super_name` as a global super-type of `class_decl_id` if it is not
/// already present and is not the class itself.
fn add_scoped_super_type_if_missing(
    db: &mut DbIndex,
    class_decl_id: &LuaTypeDeclId,
    file_id: FileId,
    source_range: TextRange,
    super_name: &str,
) {
    let super_type = LuaType::Ref(LuaTypeDeclId::global(super_name));
    if super_type == LuaType::Ref(class_decl_id.clone()) {
        return;
    }
    add_data_table_super_type(
        db,
        file_id,
        class_decl_id,
        &LuaTypeDeclId::global(super_name),
        source_range,
    );
    db.get_type_index_mut().add_super_type(
        class_decl_id.clone(),
        file_id,
        source_range,
        super_type,
    );
}

fn extract_scoped_base_name(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::LiteralExpr(literal_expr) => match literal_expr.get_literal() {
            Some(LuaLiteralToken::String(string_token)) => {
                let value = string_token.get_value();
                (!value.trim().is_empty()).then_some(value)
            }
            _ => None,
        },
        LuaExpr::NameExpr(name_expr) => {
            let value = name_expr.get_name_text()?;
            (!value.trim().is_empty()).then(|| value.to_string())
        }
        LuaExpr::IndexExpr(index_expr) => {
            let value = index_expr.get_access_path()?;
            (!value.trim().is_empty()).then(|| value.to_string())
        }
        _ => None,
    }
}

/// A wrapper function that internally calls NetworkVar or NetworkVarElement.
/// For example:
/// ```lua
/// function ENT:SetupNW(type, name)
///     self:NetworkVar(type, 0, name)
/// end
/// ```
#[derive(Debug, Clone)]
struct NetworkVarWrapper {
    /// The method name of the wrapper (e.g. "SetupNW")
    method_name: String,
    /// Fixed type name if the type arg is a string literal in the wrapper body
    fixed_type: Option<String>,
    /// Index of the wrapper parameter that provides the type arg (if not fixed)
    type_param_index: Option<usize>,
    /// Index of the wrapper parameter that provides the name arg
    name_param_index: Option<usize>,
    /// Fixed name if the name arg is a string literal in the wrapper body
    fixed_name: Option<String>,
    /// Whether this wraps NetworkVarElement (always number return type)
    is_element: bool,
    /// Whether the wrapper is a local function (`local function Foo(...)`)
    is_local: bool,
}

/// Synthesizes the members a scripted-scope file's network-var wrapper calls
/// define.
fn synthesize_network_var_wrappers(
    db: &mut DbIndex,
    file_id: FileId,
    root: &LuaChunk,
    wrappers: &[NetworkVarWrapper],
) {
    if wrappers.is_empty() {
        return;
    }
    // Use cached scoped class info (computed earlier in gmod_pre)
    let Some(scope_match) = db
        .get_gmod_infer_index()
        .get_scoped_class_info(&file_id)
        .cloned()
    else {
        return;
    };

    let class_decl_id =
        get_scripted_class_type_decl_id(&scope_match.global_name, &scope_match.class_name);

    // Find calls to these wrappers and synthesize members
    for call_expr in root.descendants::<LuaCallExpr>() {
        let (method_name, is_local_call) = match call_expr.get_prefix_expr() {
            Some(LuaExpr::IndexExpr(index_expr)) => {
                let Some(LuaIndexKey::Name(name_token)) = index_expr.get_index_key() else {
                    continue;
                };
                (name_token.get_name_text().to_string(), false)
            }
            Some(LuaExpr::NameExpr(name_expr)) => {
                let Some(name_text) = name_expr.get_name_text() else {
                    continue;
                };
                (name_text.to_string(), true)
            }
            _ => continue,
        };

        let Some(wrapper) = wrappers
            .iter()
            .find(|w| w.method_name == method_name && w.is_local == is_local_call)
        else {
            continue;
        };

        synthesize_from_wrapper_call(db, file_id, &class_decl_id, wrapper, &call_expr);
    }
}

/// Scan function definitions in a file for methods that internally call
/// NetworkVar or NetworkVarElement, and map their parameters.
fn collect_network_var_wrappers(root: &LuaChunk, global_name: &str) -> Vec<NetworkVarWrapper> {
    let mut wrappers = Vec::new();

    // Single descendants walk dispatching by node kind. Avoids two separate
    // O(N) walks for FuncStat and LocalFuncStat.
    for node in root.syntax().descendants() {
        if let Some(func_stat) = LuaFuncStat::cast(node.clone()) {
            // Only methods on the entity global: function ENT:MethodName(...)
            let Some(LuaVarExpr::IndexExpr(index_expr)) = func_stat.get_func_name() else {
                continue;
            };

            // Check the prefix is the entity global name
            let Some(LuaExpr::NameExpr(prefix_name)) = index_expr.get_prefix_expr() else {
                continue;
            };
            let Some(prefix_text) = prefix_name.get_name_text() else {
                continue;
            };
            if prefix_text != global_name {
                continue;
            }

            let Some(LuaIndexKey::Name(method_name_token)) = index_expr.get_index_key() else {
                continue;
            };
            let method_name = method_name_token.get_name_text().to_string();

            // Skip known call kinds that are already handled directly
            if GmodScriptedClassCallKind::from_call_name(&method_name).is_some() {
                continue;
            }

            let Some(closure) = func_stat.get_closure() else {
                continue;
            };

            // Collect parameter names for mapping
            let param_names: Vec<String> = get_closure_param_names(&closure);

            // Walk the closure body looking for NetworkVar/NetworkVarElement calls
            if let Some(wrapper) = find_networkvar_in_closure(&closure, &method_name, &param_names)
            {
                wrappers.push(wrapper);
            }
            continue;
        }

        if let Some(local_func_stat) = LuaLocalFuncStat::cast(node) {
            let Some(local_name) = local_func_stat.get_local_name() else {
                continue;
            };
            let Some(name_token) = local_name.get_name_token() else {
                continue;
            };
            let method_name = name_token.get_name_text().to_string();

            if GmodScriptedClassCallKind::from_call_name(&method_name).is_some() {
                continue;
            }

            let Some(closure) = local_func_stat.get_closure() else {
                continue;
            };

            let param_names: Vec<String> = get_closure_param_names(&closure);

            if let Some(mut wrapper) =
                find_networkvar_in_closure(&closure, &method_name, &param_names)
            {
                wrapper.is_local = true;
                wrappers.push(wrapper);
            }
        }
    }

    wrappers
}

/// Get the parameter names of a closure (excluding `self` for methods).
fn get_closure_param_names(closure: &LuaClosureExpr) -> Vec<String> {
    let Some(params_list) = closure.get_params_list() else {
        return Vec::new();
    };

    params_list
        .get_params()
        .filter_map(|param| {
            if param.is_dots() {
                return None;
            }
            Some(param.get_name_token()?.get_name_text().to_string())
        })
        .collect()
}

/// Look inside a closure body for NetworkVar/NetworkVarElement calls and map
/// their arguments back to the closure's parameter list.
fn find_networkvar_in_closure(
    closure: &LuaClosureExpr,
    wrapper_method_name: &str,
    param_names: &[String],
) -> Option<NetworkVarWrapper> {
    let block = closure.get_block()?;

    for call_expr in block.descendants::<LuaCallExpr>() {
        let Some(LuaExpr::IndexExpr(inner_index)) = call_expr.get_prefix_expr() else {
            continue;
        };

        let Some(LuaIndexKey::Name(inner_name_token)) = inner_index.get_index_key() else {
            continue;
        };

        let inner_method = inner_name_token.get_name_text();
        let is_element = match inner_method {
            "NetworkVar" => false,
            "NetworkVarElement" => true,
            _ => continue,
        };

        // Found a NetworkVar/NetworkVarElement call inside the wrapper.
        // Collect the arguments and map them.
        let Some(args_list) = call_expr.get_args_list() else {
            continue;
        };
        let inner_args: Vec<LuaExpr> = args_list.get_args().collect();

        // Determine the type argument (first arg to NetworkVar)
        let (fixed_type, type_param_index) =
            resolve_wrapper_arg_mapping(&inner_args, 0, param_names);

        // Determine the name argument — find the last string-like argument
        // For 3-arg NetworkVar: name is at index 2
        // For 2-arg NetworkVar: name is at index 1
        // For 4-arg NetworkVarElement: name is at index 3
        // Try from the end to find the name position
        let name_indices: &[usize] = if is_element { &[3, 2, 1] } else { &[2, 1] };

        let mut fixed_name = None;
        let mut name_param_index = None;

        for &idx in name_indices {
            if idx >= inner_args.len() {
                continue;
            }
            let (fixed, param_idx) = resolve_wrapper_arg_mapping(&inner_args, idx, param_names);
            if fixed.is_some() || param_idx.is_some() {
                fixed_name = fixed;
                name_param_index = param_idx;
                break;
            }
        }

        // Must have either a fixed name or a parameter mapping for the name
        if fixed_name.is_none() && name_param_index.is_none() {
            continue;
        }

        return Some(NetworkVarWrapper {
            method_name: wrapper_method_name.to_string(),
            fixed_type,
            type_param_index,
            name_param_index,
            fixed_name,
            is_element,
            is_local: false,
        });
    }

    None
}

/// Given a call argument expression and the wrapper's parameter names,
/// determine if the argument is a fixed string literal or a reference to
/// one of the wrapper's parameters.
fn resolve_wrapper_arg_mapping(
    inner_args: &[LuaExpr],
    arg_index: usize,
    param_names: &[String],
) -> (Option<String>, Option<usize>) {
    let Some(arg) = inner_args.get(arg_index) else {
        return (None, None);
    };

    match arg {
        LuaExpr::LiteralExpr(literal) => {
            if let Some(LuaLiteralToken::String(s)) = literal.get_literal() {
                let value = s.get_value();
                if !value.is_empty() {
                    return (Some(value), None);
                }
            }
            (None, None)
        }
        LuaExpr::NameExpr(name_expr) => {
            if let Some(name) = name_expr.get_name_text() {
                if let Some(idx) = param_names.iter().position(|p| *p == name) {
                    return (None, Some(idx));
                }
            }
            (None, None)
        }
        _ => (None, None),
    }
}

/// Given a call to a known wrapper method and the wrapper's parameter mapping,
/// resolve the concrete type and name from the call arguments and synthesize
/// Get/Set members.
fn synthesize_from_wrapper_call(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    wrapper: &NetworkVarWrapper,
    call_expr: &LuaCallExpr,
) {
    let Some(args_list) = call_expr.get_args_list() else {
        return;
    };
    let call_args: Vec<LuaExpr> = args_list.get_args().collect();

    // Resolve the type name
    let type_name = if let Some(ref fixed) = wrapper.fixed_type {
        fixed.clone()
    } else if let Some(idx) = wrapper.type_param_index {
        match call_args.get(idx) {
            Some(LuaExpr::LiteralExpr(lit)) => {
                if let Some(LuaLiteralToken::String(s)) = lit.get_literal() {
                    s.get_value()
                } else {
                    return;
                }
            }
            _ => return,
        }
    } else {
        return;
    };

    // Resolve the property name
    let (prop_name, prop_name_expr) = if let Some(ref fixed) = wrapper.fixed_name {
        (fixed.clone(), None)
    } else if let Some(idx) = wrapper.name_param_index {
        match call_args.get(idx) {
            Some(LuaExpr::LiteralExpr(lit)) => {
                if let Some(LuaLiteralToken::String(s)) = lit.get_literal() {
                    let value = s.get_value();
                    if value.is_empty() {
                        return;
                    }
                    (value, Some(call_args[idx].clone()))
                } else {
                    return;
                }
            }
            _ => return,
        }
    } else {
        return;
    };

    let value_type = if wrapper.is_element {
        LuaType::Number
    } else {
        resolve_networkvar_type(&type_name)
    };
    let owner = LuaMemberOwner::Type(class_decl_id.clone());

    // Use the name expression's syntax id for the getter if available,
    // otherwise use the call expression's syntax id.
    let getter_syntax_id = prop_name_expr
        .as_ref()
        .map(|e| e.get_syntax_id())
        .unwrap_or_else(|| call_expr.get_syntax_id());

    let getter_name = format!("Get{prop_name}");
    let getter_func =
        LuaFunctionType::new(AsyncState::None, true, false, vec![], value_type.clone());
    let member_id = LuaMemberId::new(getter_syntax_id, file_id);
    let member = LuaMember::new(
        member_id,
        LuaMemberKey::Name(getter_name.as_str().into()),
        LuaMemberFeature::FileMethodDecl,
        None,
    );
    db.get_member_index_mut().add_member(owner.clone(), member);
    write_type_cache(
        db,
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(getter_func))),
        TypeCacheWriteMode::InsertOnly,
    );

    // Setter
    let setter_syntax_id = call_expr.get_syntax_id();
    let setter_name = format!("Set{prop_name}");
    let setter_func = LuaFunctionType::new(
        AsyncState::None,
        true,
        false,
        vec![("value".to_string(), Some(value_type))],
        LuaType::Nil,
    );
    let member_id = LuaMemberId::new(setter_syntax_id, file_id);
    let member = LuaMember::new(
        member_id,
        LuaMemberKey::Name(setter_name.as_str().into()),
        LuaMemberFeature::FileMethodDecl,
        None,
    );
    db.get_member_index_mut().add_member(owner.clone(), member);
    write_type_cache(
        db,
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(setter_func))),
        TypeCacheWriteMode::InsertOnly,
    );
}

fn synthesize_inheritance_base(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    base_name: &str,
    is_derive: bool,
    class_name_prefix: Option<&str>,
    source_syntax_id: LuaSyntaxId,
) {
    if base_name.is_empty() {
        return;
    }

    let effective_base_name = if is_derive {
        let Some(prefix) = class_name_prefix else {
            return;
        };
        if base_name.starts_with(prefix) {
            base_name.to_string()
        } else {
            format!("{prefix}{base_name}")
        }
    } else {
        base_name.to_string()
    };

    materialize_scoped_gamemode_base(db, file_id, class_name_prefix, &effective_base_name);

    let super_type = LuaType::Ref(LuaTypeDeclId::global(&effective_base_name));
    db.get_type_index_mut().add_super_type(
        class_decl_id.clone(),
        file_id,
        source_syntax_id.get_range(),
        super_type,
    );
    synthesize_baseclass_member(
        db,
        file_id,
        class_decl_id,
        &effective_base_name,
        source_syntax_id,
    );
}

fn materialize_scoped_gamemode_base(
    db: &mut DbIndex,
    file_id: FileId,
    class_name_prefix: Option<&str>,
    base_name: &str,
) {
    let Some("gamemode_") = class_name_prefix else {
        return;
    };

    let Some(stripped) = base_name.strip_prefix("gamemode_") else {
        return;
    };
    if stripped.is_empty() {
        return;
    }

    let range = rowan::TextRange::default();
    ensure_scoped_class_type_decl(db, file_id, base_name, "GM", &[], range);
}

fn resolve_effective_inheritance_call<'a>(
    metadata: &'a GmodScriptedClassFileMetadata,
    class_name_prefix: Option<&str>,
) -> Option<&'a GmodScriptedClassCallMetadata> {
    metadata
        .derive_gamemode_calls
        .iter()
        .rev()
        .find(|call| valid_inheritance_literal(call))
        .or_else(|| {
            metadata.define_baseclass_calls.iter().rev().find(|call| {
                valid_inheritance_literal(call)
                    && define_baseclass_targets_scope_class(call, class_name_prefix)
            })
        })
}

fn valid_inheritance_literal(call: &GmodScriptedClassCallMetadata) -> bool {
    matches!(
        call.literal_args.get(call.inheritance_name_arg_idx()),
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty()
    )
}

/// `DEFINE_BASECLASS` names the base of whatever scripted table the file
/// registers. When the scope names its classes `<prefix><name>` (gamemodes
/// use `gamemode_`), a base outside that family — `DEFINE_BASECLASS("Panel")`
/// in a derma file — is the registered panel's base, not the scope class's.
fn define_baseclass_targets_scope_class(
    call: &GmodScriptedClassCallMetadata,
    class_name_prefix: Option<&str>,
) -> bool {
    let Some(prefix) = class_name_prefix else {
        return true;
    };
    match call.literal_args.get(call.inheritance_name_arg_idx()) {
        Some(Some(GmodClassCallLiteral::String(name))) => name.starts_with(prefix),
        _ => false,
    }
}

fn resolve_effective_inheritance_base(
    metadata: &GmodScriptedClassFileMetadata,
    class_name_prefix: Option<&str>,
) -> Option<(String, bool, LuaSyntaxId)> {
    let call = resolve_effective_inheritance_call(metadata, class_name_prefix)?;
    let base_name = match call.literal_args.get(call.inheritance_name_arg_idx()) {
        Some(Some(GmodClassCallLiteral::String(name))) => name.as_str(),
        _ => return None,
    };

    if metadata
        .derive_gamemode_calls
        .iter()
        .any(|candidate| std::ptr::eq(candidate, call))
    {
        let prefix = class_name_prefix?;
        return Some((
            if base_name.starts_with(prefix) {
                base_name.to_string()
            } else {
                format!("{prefix}{base_name}")
            },
            true,
            call.syntax_id,
        ));
    }

    Some((base_name.to_string(), false, call.syntax_id))
}

/// Synthesize a parent-name alias member on a derived scripted class.
///
/// In Garry's Mod, derived gamemodes can access their inherited base via a
/// field named after the parent's short (prefix-stripped) folder name. For
/// example, a DarkRP gamemode inheriting from Sandbox uses `self.Sandbox` to
/// reach the base gamemode table. The runtime exposes this field, but the
/// analyzer would otherwise have no type for it, which breaks hover, goto,
/// and completion on `self.<ShortParentName>.<member>`.
///
/// Rules (mirroring the oracle-approved design):
/// - Only applies when the scope declares a non-empty `classNamePrefix`.
/// - The parent class name must start with that prefix, and the remainder
///   must be non-empty (otherwise we skip silently to avoid bogus aliases
///   on malformed or cross-scope base names).
/// - If the derived class already has a member with the alias name (for
///   example, because the user wrote `GM.Sandbox = BaseClass` themselves),
///   the explicit field wins and we do not synthesize a duplicate.
fn synthesize_define_baseclass_parent_alias(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    class_name_prefix: Option<&str>,
    call: &GmodScriptedClassCallMetadata,
) {
    let prefix = match class_name_prefix {
        Some(p) if !p.is_empty() => p,
        _ => return,
    };

    let base_name = match call.literal_args.get(call.inheritance_name_arg_idx()) {
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => name.as_str(),
        _ => return,
    };

    // Parent must belong to the same prefix-scoped class family, otherwise
    // the short-name convention does not apply.
    let Some(stripped) = base_name.strip_prefix(prefix) else {
        return;
    };
    if stripped.is_empty() {
        return;
    }

    let alias_name = capitalize_ascii_first(stripped);
    if alias_name.is_empty() {
        return;
    }

    let owner = LuaMemberOwner::Type(class_decl_id.clone());
    let member_key = LuaMemberKey::Name(alias_name.as_str().into());

    // If the user already defined this field (e.g. `GM.Sandbox = BaseClass`),
    // let their definition win — don't shadow it with a synthetic decl. The
    // check must be realm-scoped, not bare existence: a write in the other
    // realm (e.g. a second DEFINE_BASECLASS entry file) is not a definition
    // this realm's readers can see. It must NOT be position-scoped though: a
    // write later in this same file is still the user's definition. Aliases
    // other files synthesized are not the user's definition: counting them
    // would keep whichever entry files happened to synthesize first.
    if let Some(item) = db.get_member_index().get_member_item(&owner, &member_key)
        && item
            .visible_member_ids_with_realm(db, &file_id)
            .iter()
            .any(|member_id| !is_define_baseclass_parent_alias(db, member_id))
    {
        return;
    }

    let member_id = LuaMemberId::new(define_baseclass_parent_alias_syntax_id(call), file_id);
    let member = LuaMember::new(member_id, member_key, LuaMemberFeature::FileFieldDecl, None);
    db.get_member_index_mut().add_member(owner, member);
    write_type_cache(
        db,
        member_id.into(),
        LuaTypeCache::DocType(LuaType::Ref(LuaTypeDeclId::global(base_name))),
        TypeCacheWriteMode::InsertOnly,
    );
}

/// The syntax id a `DEFINE_BASECLASS` call's parent alias is declared at: the
/// base-name argument, or the call itself when it has none.
fn define_baseclass_parent_alias_syntax_id(call: &GmodScriptedClassCallMetadata) -> LuaSyntaxId {
    call.args
        .first()
        .map(|a| a.syntax_id)
        .unwrap_or(call.syntax_id)
}

/// Whether `member_id` is a parent alias synthesized from a `DEFINE_BASECLASS` call.
fn is_define_baseclass_parent_alias(db: &DbIndex, member_id: &LuaMemberId) -> bool {
    db.get_gmod_class_metadata_index()
        .get_file_metadata(&member_id.file_id)
        .is_some_and(|metadata| {
            metadata.define_baseclass_calls.iter().any(|call| {
                define_baseclass_parent_alias_syntax_id(call) == *member_id.get_syntax_id()
            })
        })
}

/// Uppercase the first ASCII letter of `s`, leaving the rest untouched.
/// Non-ASCII leading bytes are preserved as-is (GMod class names are ASCII
/// in practice, so this keeps the implementation simple and allocation-light).
fn capitalize_ascii_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => {
            let mut out = String::with_capacity(s.len());
            for c in first.to_uppercase() {
                out.push(c);
            }
            out.extend(chars);
            out
        }
        None => String::new(),
    }
}

fn synthesize_accessor_func(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    call: &GmodScriptedClassCallMetadata,
) {
    // AccessorFunc(target, "m_VarKey", "Name", forceType)
    // args[0] = target (ENT etc) - non-literal name ref
    // args[1] = backing field name (string)
    // args[2] = accessor name (string)
    // args[3] = force type (FORCE_STRING, number, bool, etc)

    let accessor_name = match call.literal_args.get(2) {
        Some(Some(GmodClassCallLiteral::String(name))) => name.clone(),
        _ => return,
    };

    if accessor_name.is_empty() {
        return;
    }

    let var_key = match call.literal_args.get(1) {
        Some(Some(GmodClassCallLiteral::String(name))) => Some(name.clone()),
        _ => None,
    };

    let force_type = call.literal_args.get(3).and_then(|arg| arg.as_ref());
    let value_type = resolve_accessor_force_type(force_type);
    let setter_input_type = resolve_accessor_setter_input_type(force_type);
    let owner = LuaMemberOwner::Type(class_decl_id.clone());

    // Synthesize the backing field if var_key is present
    if let Some(ref var_key_name) = var_key {
        if let Some(field_syntax_id) = call.args.get(1).map(|a| a.syntax_id) {
            let member_id = LuaMemberId::new(field_syntax_id, file_id);
            let member = LuaMember::new(
                member_id,
                LuaMemberKey::Name(var_key_name.as_str().into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            );
            db.get_member_index_mut().add_member(owner.clone(), member);
            // Without a force type the accessor declares the field's existence,
            // not its type: `DocType` would carry annotation authority and let
            // the placeholder `any` displace types inferred from real writes.
            let field_cache = if value_type.is_any() {
                LuaTypeCache::InferType(LuaType::Any)
            } else {
                LuaTypeCache::DocType(value_type.clone())
            };
            write_type_cache(
                db,
                member_id.into(),
                field_cache,
                TypeCacheWriteMode::InsertOnly,
            );
        }
    }

    // Synthesize the getter: GetName(self: Class): valueType
    if let Some(getter_syntax_id) = call.args.get(2).map(|a| a.syntax_id) {
        let getter_name = format!("Get{accessor_name}");
        let getter_func =
            LuaFunctionType::new(AsyncState::None, true, false, vec![], value_type.clone());
        let member_id = LuaMemberId::new(getter_syntax_id, file_id);
        let member = LuaMember::new(
            member_id,
            LuaMemberKey::Name(getter_name.as_str().into()),
            LuaMemberFeature::FileMethodDecl,
            None,
        );
        db.get_member_index_mut().add_member(owner.clone(), member);
        // An annotated accessor function synthesizes the same getter without
        // reading the force argument; whichever ran first must not decide it.
        write_type_cache(
            db,
            member_id.into(),
            LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(getter_func))),
            TypeCacheWriteMode::ForceOverwrite,
        );
    }

    // Synthesize the setter: SetName(self: Class, value: valueType)
    let setter_syntax_id = call.syntax_id;
    let setter_name = format!("Set{accessor_name}");
    let setter_func = LuaFunctionType::new(
        AsyncState::None,
        true,
        false,
        vec![("value".to_string(), Some(setter_input_type))],
        LuaType::Nil,
    );
    let member_id = LuaMemberId::new(setter_syntax_id, file_id);
    let member = LuaMember::new(
        member_id,
        LuaMemberKey::Name(setter_name.as_str().into()),
        LuaMemberFeature::FileMethodDecl,
        None,
    );
    db.get_member_index_mut().add_member(owner.clone(), member);
    write_type_cache(
        db,
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(setter_func))),
        TypeCacheWriteMode::InsertOnly,
    );
}

fn synthesize_network_var(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    call: &GmodScriptedClassCallMetadata,
) {
    // ENT:NetworkVar("Type", slot, "Name") — 3-arg form
    // ENT:NetworkVar("Type", "Name")       — 2-arg form (slot omitted)
    // args[0] = type name (string)
    // args[1] = slot (integer) OR name (string, if 2-arg form)
    // args[2] = name (string, if 3-arg form)

    let type_arg_idx = call.network_var_type_arg_idx().unwrap_or(0);
    let type_name = match call.literal_args.get(type_arg_idx) {
        Some(Some(GmodClassCallLiteral::String(name))) => name.clone(),
        _ => return,
    };

    let (prop_name, prop_name_arg_idx) = if let Some(name_arg_idx) = call.network_var_name_arg_idx()
    {
        match call.literal_args.get(name_arg_idx) {
            Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
                (name.clone(), name_arg_idx)
            }
            _ => return,
        }
    } else {
        // Try index 2 first (3-arg form), then index 1 (2-arg form)
        match call.literal_args.get(2) {
            Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
                (name.clone(), 2usize)
            }
            _ => match call.literal_args.get(1) {
                Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
                    (name.clone(), 1usize)
                }
                _ => return,
            },
        }
    };

    let value_type = resolve_networkvar_type(&type_name);
    synthesize_data_table_slot(db, file_id, class_decl_id, call, &prop_name, &value_type);
    if call.network_var_roles.is_some_and(|roles| roles.slot_only) {
        return;
    }
    let owner = LuaMemberOwner::Type(class_decl_id.clone());

    // Synthesize getter: GetPropName(self: Class): valueType
    if let Some(getter_syntax_id) = call.args.get(prop_name_arg_idx).map(|a| a.syntax_id) {
        let getter_name = format!("Get{prop_name}");
        let getter_func =
            LuaFunctionType::new(AsyncState::None, true, false, vec![], value_type.clone());
        let member_id = LuaMemberId::new(getter_syntax_id, file_id);
        let member = LuaMember::new(
            member_id,
            LuaMemberKey::Name(getter_name.as_str().into()),
            LuaMemberFeature::FileMethodDecl,
            None,
        );
        db.get_member_index_mut().add_member(owner.clone(), member);
        write_type_cache(
            db,
            member_id.into(),
            LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(getter_func))),
            TypeCacheWriteMode::InsertOnly,
        );
    }

    // Synthesize setter: SetPropName(self: Class, value: valueType)
    let setter_syntax_id = call.syntax_id;
    let setter_name = format!("Set{prop_name}");
    let setter_func = LuaFunctionType::new(
        AsyncState::None,
        true,
        false,
        vec![("value".to_string(), Some(value_type))],
        LuaType::Nil,
    );
    let member_id = LuaMemberId::new(setter_syntax_id, file_id);
    let member = LuaMember::new(
        member_id,
        LuaMemberKey::Name(setter_name.as_str().into()),
        LuaMemberFeature::FileMethodDecl,
        None,
    );
    db.get_member_index_mut().add_member(owner.clone(), member);
    write_type_cache(
        db,
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(setter_func))),
        TypeCacheWriteMode::InsertOnly,
    );
}

/// The nominal slot-table type belonging to a scripted class.
fn data_table_type_decl_id(class_decl_id: &LuaTypeDeclId) -> LuaTypeDeclId {
    LuaTypeDeclId::global(&format!("{}.dt", class_decl_id.get_name()))
}

/// Mirrors a scripted class's inheritance edge on its slot-table type.
fn add_data_table_super_type(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    base_id: &LuaTypeDeclId,
    range: TextRange,
) {
    let dt_id = ensure_data_table_type_decl(db, file_id, class_decl_id, range);
    db.get_type_index_mut().add_super_type(
        dt_id,
        file_id,
        range,
        LuaType::Ref(data_table_type_decl_id(base_id)),
    );
}

/// Declares the scripted class's slot-table type at its contributing source.
fn ensure_data_table_type_decl(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    range: TextRange,
) -> LuaTypeDeclId {
    let dt_id = data_table_type_decl_id(class_decl_id);
    db.get_type_index_mut().add_type_decl(
        file_id,
        LuaTypeDecl::new(
            file_id,
            range,
            dt_id.get_simple_name().to_string(),
            LuaDeclTypeKind::Class,
            LuaTypeFlag::None.into(),
            dt_id.clone(),
        ),
    );
    dt_id
}

/// Adds a typed slot and the scripted class's `dt` member from a declared call role.
fn synthesize_data_table_slot(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    call: &GmodScriptedClassCallMetadata,
    name: &str,
    value_type: &LuaType,
) {
    let range = call.syntax_id.get_range();
    let dt_id = ensure_data_table_type_decl(db, file_id, class_decl_id, range);
    for (kind, owner, key, typ) in [
        (
            glua_parser::LuaSyntaxKind::TableFieldAssign,
            LuaMemberOwner::Type(class_decl_id.clone()),
            "dt",
            LuaType::Ref(dt_id.clone()),
        ),
        (
            glua_parser::LuaSyntaxKind::IndexExpr,
            LuaMemberOwner::Type(dt_id),
            name,
            value_type.clone(),
        ),
    ] {
        let member_id = LuaMemberId::new(LuaSyntaxId::new(kind.into(), range), file_id);
        db.get_member_index_mut().add_member(
            owner,
            LuaMember::new(
                member_id,
                LuaMemberKey::Name(key.into()),
                LuaMemberFeature::FileFieldDecl,
                None,
            ),
        );
        write_type_cache(
            db,
            member_id.into(),
            LuaTypeCache::DocType(typ),
            TypeCacheWriteMode::InsertOnly,
        );
    }
}

fn synthesize_network_var_element(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    call: &GmodScriptedClassCallMetadata,
) {
    // ENT:NetworkVarElement("Type", slot, element, "Name") — 4-arg form
    // ENT:NetworkVarElement("Type", slot, "Name")          — 3-arg form
    // ENT:NetworkVarElement("Type", "Name")                — 2-arg form
    // The value type is always `number` for element access.
    // args[0] = type name (string) — used only for validation, not for type
    // args[1] = slot or name
    // args[2] = element or name
    // args[3] = name (if 4-arg form)

    let type_arg_idx = call.network_var_type_arg_idx().unwrap_or(0);
    if call
        .literal_args
        .get(type_arg_idx)
        .and_then(|a| a.as_ref())
        .is_none()
    {
        return;
    }

    let (prop_name, prop_name_arg_idx) = if let Some(name_arg_idx) = call.network_var_name_arg_idx()
    {
        match call.literal_args.get(name_arg_idx) {
            Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
                (name.clone(), name_arg_idx)
            }
            _ => return,
        }
    } else {
        // Find the property name: try index 3, then 2, then 1
        match call.literal_args.get(3) {
            Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
                (name.clone(), 3usize)
            }
            _ => match call.literal_args.get(2) {
                Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
                    (name.clone(), 2usize)
                }
                _ => match call.literal_args.get(1) {
                    Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
                        (name.clone(), 1usize)
                    }
                    _ => return,
                },
            },
        }
    };

    // NetworkVarElement always produces number accessors
    let value_type = LuaType::Number;
    let owner = LuaMemberOwner::Type(class_decl_id.clone());

    // Synthesize getter: GetPropName(self: Class): number
    if let Some(getter_syntax_id) = call.args.get(prop_name_arg_idx).map(|a| a.syntax_id) {
        let getter_name = format!("Get{prop_name}");
        let getter_func =
            LuaFunctionType::new(AsyncState::None, true, false, vec![], value_type.clone());
        let member_id = LuaMemberId::new(getter_syntax_id, file_id);
        let member = LuaMember::new(
            member_id,
            LuaMemberKey::Name(getter_name.as_str().into()),
            LuaMemberFeature::FileMethodDecl,
            None,
        );
        db.get_member_index_mut().add_member(owner.clone(), member);
        write_type_cache(
            db,
            member_id.into(),
            LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(getter_func))),
            TypeCacheWriteMode::InsertOnly,
        );
    }

    // Synthesize setter: SetPropName(self: Class, value: number)
    let setter_syntax_id = call.syntax_id;
    let setter_name = format!("Set{prop_name}");
    let setter_func = LuaFunctionType::new(
        AsyncState::None,
        true,
        false,
        vec![("value".to_string(), Some(value_type))],
        LuaType::Nil,
    );
    let member_id = LuaMemberId::new(setter_syntax_id, file_id);
    let member = LuaMember::new(
        member_id,
        LuaMemberKey::Name(setter_name.as_str().into()),
        LuaMemberFeature::FileMethodDecl,
        None,
    );
    db.get_member_index_mut().add_member(owner.clone(), member);
    write_type_cache(
        db,
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(setter_func))),
        TypeCacheWriteMode::InsertOnly,
    );
}

/// The base a vgui registration gives its table: the literal it passes, or
/// `Panel` when it passes none, as GMod's `base or "Panel"` defaults it.
fn registered_panel_base(
    call: &GmodScriptedClassCallMetadata,
    base_source: Option<&GmodClassCallArgSource>,
) -> Option<String> {
    let argument = base_source.and_then(|source| {
        if source.is_derived() {
            call.field_args
                .iter()
                .find(|arg| &arg.source == source)
                .map(|arg| arg.value.as_ref())
        } else {
            call.args.get(source.arg_idx).map(|arg| arg.value.as_ref())
        }
    });
    match argument {
        None
        | Some(Some(GmodClassCallLiteral::Nil))
        | Some(Some(GmodClassCallLiteral::Boolean(false))) => Some("Panel".to_string()),
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => Some(name.clone()),
        Some(_) => None,
    }
}

fn synthesize_vgui_register(
    db: &mut DbIndex,
    cache: &mut VguiSynthesisCache,
    file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
    panel_name: &str,
    resolved_registration: Option<ResolvedVguiRegistrationRegion>,
    phase: VguiSynthesisPhase,
) {
    // vgui.Register("PanelName", TABLE, "BasePanel")
    // args[0] = panel name (string)
    // args[1] = table variable (name ref)
    // args[2] = base panel name (string)
    let table_source = call.vgui_panel_table_arg_source(1);
    let base_source = call.vgui_panel_base_arg_source(Some(2));

    let table_var_name = match call.value_for_arg_source(&table_source) {
        Some(GmodClassCallLiteral::NameRef(name)) => Some(name.clone()),
        _ => None,
    };

    let base_panel = registered_panel_base(call, base_source.as_ref());

    synthesize_panel_class(
        db,
        cache,
        file_id,
        panel_name,
        table_var_name.as_deref(),
        base_panel.as_deref(),
        GmodScriptedClassCallKind::VguiRegister,
        call,
        resolved_registration,
        phase,
    );
}

fn synthesize_derma_define_control(
    db: &mut DbIndex,
    cache: &mut VguiSynthesisCache,
    file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
    control_name: &str,
    resolved_registration: Option<ResolvedVguiRegistrationRegion>,
    phase: VguiSynthesisPhase,
) {
    // derma.DefineControl("ControlName", "description", TABLE, "BasePanel")
    // args[0] = control name (string)
    // args[1] = description (string, ignored)
    // args[2] = table variable (name ref)
    // args[3] = base panel name (string)
    let table_source = call.vgui_panel_table_arg_source(2);
    let base_source = call.vgui_panel_base_arg_source(Some(3));

    let table_var_name = match call.value_for_arg_source(&table_source) {
        Some(GmodClassCallLiteral::NameRef(name)) => Some(name.clone()),
        _ => None,
    };

    let base_panel = registered_panel_base(call, base_source.as_ref());

    synthesize_panel_class(
        db,
        cache,
        file_id,
        control_name,
        table_var_name.as_deref(),
        base_panel.as_deref(),
        GmodScriptedClassCallKind::DermaDefineControl,
        call,
        resolved_registration,
        phase,
    );

    if phase == VguiSynthesisPhase::Bind {
        register_global_panel(db, file_id, control_name, call);
    }
}

fn synthesize_vgui_register_table(
    db: &mut DbIndex,
    cache: &mut VguiSynthesisCache,
    file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
    resolved_registration: Option<ResolvedVguiRegistrationRegion>,
    phase: VguiSynthesisPhase,
) {
    // vgui.RegisterTable(TABLE, "BasePanel")
    // args[0] = table variable (name ref)
    // args[1] = base panel name (string)
    let table_source = call.vgui_panel_table_arg_source(0);
    let base_source = call.vgui_panel_base_arg_source(Some(1));

    let table_var_name = match call.value_for_arg_source(&table_source) {
        Some(GmodClassCallLiteral::NameRef(name)) => Some(name.clone()),
        _ => None,
    };

    let base_panel = registered_panel_base(call, base_source.as_ref());

    let class_decl_id = vgui_register_table_type_decl_id(file_id, call);
    synthesize_panel_class_with_id(
        db,
        cache,
        file_id,
        class_decl_id,
        table_var_name.as_deref(),
        base_panel.as_deref(),
        GmodScriptedClassCallKind::VguiRegisterTable,
        call,
        resolved_registration,
        phase,
    );
}

fn synthesize_vgui_register_file_target(
    db: &mut DbIndex,
    source_file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
) -> Option<(FileId, LuaDeclId, LuaTypeDeclId, String, TextSize, TextSize)> {
    // vgui.RegisterFile("path/to/panel.lua") includes a file with a temporary
    // global PANEL table. The file itself is not a named VGUI class, but its
    // methods should still see PANEL.Base inheritance while it is being loaded.
    let panel_source = call.vgui_panel_define_arg_source();
    let GmodClassCallLiteral::String(path) = call.value_for_arg_source(&panel_source)? else {
        return None;
    };
    let target_file_id =
        resolve_load_dependency_target(db, source_file_id, LuaDependencyKind::Include, path)?;
    // `vgui.RegisterFile` gives a temporary PANEL table the same runtime
    // default as `vgui.Register`: without an explicit Base it derives Panel.
    let (base_panel, base_source_range) = find_target_panel_base_assignment(db, target_file_id)
        .unwrap_or_else(|| {
            let file_range = db
                .get_vfs()
                .get_syntax_tree(&target_file_id)
                .map(|tree| tree.get_chunk_node().syntax().text_range())
                .unwrap_or_default();
            ("Panel".to_string(), file_range)
        });

    let class_decl_id = LuaTypeDeclId::local(
        target_file_id,
        &format!("__gmod_vgui_register_file_{}", target_file_id.id),
    );
    let panel_name = class_decl_id.get_simple_name().to_string();
    let class_type = LuaType::Def(class_decl_id.clone());

    // `vgui.RegisterFile` returns the temporary PANEL table it loaded. Bind
    // that call expression to the synthesized class so a subsequent
    // `vgui.CreateFromTable(result)` preserves the file's PANEL members.
    write_type_cache(
        db,
        LuaTypeOwner::SyntaxId(InFiled::new(source_file_id, call.syntax_id)),
        LuaTypeCache::InferType(class_type.clone()),
        TypeCacheWriteMode::ForceOverwrite,
    );
    if let Some(decl_id) = local_decl_for_call_result(db, source_file_id, call.syntax_id) {
        write_type_cache(
            db,
            decl_id.into(),
            LuaTypeCache::InferType(class_type.clone()),
            TypeCacheWriteMode::ForceOverwrite,
        );
    }

    // Every registering site adds its location unless a doc tag declares the
    // class: whichever registration ran first must not own the declaration.
    if db
        .get_type_index()
        .get_source_type_decl(&class_decl_id)
        .is_none()
    {
        let range = db
            .get_vfs()
            .get_syntax_tree(&target_file_id)
            .map(|tree| tree.get_chunk_node().syntax().text_range())
            .unwrap_or_else(|| call.syntax_id.get_range());
        db.get_type_index_mut().add_type_decl(
            target_file_id,
            LuaTypeDecl::new(
                target_file_id,
                range,
                class_decl_id.get_simple_name().to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::AutoGenerated.into(),
                class_decl_id.clone(),
            ),
        );
    }

    let super_type = LuaType::Ref(LuaTypeDeclId::global(&base_panel));
    db.get_type_index_mut().add_super_type(
        class_decl_id.clone(),
        target_file_id,
        base_source_range,
        super_type,
    );

    let target_panel_decl_ids = ensure_register_file_panel_decls(db, target_file_id)?;
    let panel_decl_id = *target_panel_decl_ids.first()?;
    for decl_id in target_panel_decl_ids {
        bind_panel_table_decl(db, decl_id, &class_type);
    }

    let panel_owner = LuaMemberOwner::GlobalPath(GlobalId::new("PANEL"));
    let class_owner = LuaMemberOwner::Type(class_decl_id.clone());
    let member_ids = db
        .get_member_index()
        .get_members(&panel_owner)
        .map(|members| {
            members
                .iter()
                .filter(|member| member.get_file_id() == target_file_id)
                .map(|member| member.get_id())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for member_id in member_ids {
        db.get_member_index_mut()
            .transfer_member(class_owner.clone(), member_id, false);
    }

    let range = db
        .get_vfs()
        .get_syntax_tree(&target_file_id)?
        .get_chunk_node()
        .syntax()
        .text_range();
    Some((
        target_file_id,
        panel_decl_id,
        class_decl_id,
        panel_name,
        range.start(),
        range.end(),
    ))
}

fn local_decl_for_call_result(
    db: &DbIndex,
    file_id: FileId,
    call_syntax_id: LuaSyntaxId,
) -> Option<LuaDeclId> {
    let root = db.get_vfs().get_syntax_tree(&file_id)?.get_chunk_node();
    let call_range = call_syntax_id.get_range();
    for local_stat in root.descendants::<LuaLocalStat>() {
        let Some(value_idx) = local_stat
            .get_value_exprs()
            .position(|value| value.get_range() == call_range)
        else {
            continue;
        };
        if let Some(local_name) = local_stat.get_local_name_list().nth(value_idx) {
            return Some(LuaDeclId::new(file_id, local_name.get_position()));
        }
    }
    None
}

fn ensure_register_file_panel_decls(db: &mut DbIndex, file_id: FileId) -> Option<Vec<LuaDeclId>> {
    let existing_decl_ids = db
        .get_global_index()
        .get_global_decl_ids("PANEL")
        .map(|decl_ids| {
            decl_ids
                .iter()
                .copied()
                .filter(|decl_id| decl_id.file_id == file_id)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !existing_decl_ids.is_empty() {
        return Some(existing_decl_ids);
    }

    let range = db
        .get_vfs()
        .get_syntax_tree(&file_id)?
        .get_chunk_node()
        .syntax()
        .text_range();
    let insert_range = TextRange::empty(range.start());
    let panel_decl = LuaDecl::new(
        "PANEL",
        file_id,
        insert_range,
        LuaDeclExtra::Global {
            kind: glua_parser::LuaSyntaxKind::NameExpr.into(),
        },
        None,
    );
    let decl_id = panel_decl.get_id();

    db.get_decl_index_mut().add_derived_decl(panel_decl);
    let file_order_key = db.get_vfs().file_order_key(&decl_id.file_id);
    db.get_global_index_mut()
        .add_global_decl("PANEL", decl_id, file_order_key);

    Some(vec![decl_id])
}

fn find_target_panel_base_assignment(db: &DbIndex, file_id: FileId) -> Option<(String, TextRange)> {
    let tree = db.get_vfs().get_syntax_tree(&file_id)?;
    let chunk = tree.get_chunk_node();
    let mut base_name = None;

    for assign_stat in chunk.descendants::<LuaAssignStat>() {
        let (vars, exprs) = assign_stat.get_var_and_expr_list();
        for (idx, var) in vars.iter().enumerate() {
            let LuaVarExpr::IndexExpr(index_expr) = var else {
                continue;
            };
            if !index_expr_prefix_matches(index_expr, "PANEL") {
                continue;
            }
            let Some(index_key) = index_expr.get_index_key() else {
                continue;
            };
            if index_key.get_path_part() != "Base" {
                continue;
            }
            if let Some(name) = exprs.get(idx).and_then(lua_expr_string_literal) {
                base_name = Some((name, index_expr.get_range()));
            }
        }
    }

    base_name
}

fn lua_expr_string_literal(expr: &LuaExpr) -> Option<String> {
    let mut current = expr.clone();
    loop {
        match current {
            LuaExpr::LiteralExpr(literal_expr) => {
                let LuaLiteralToken::String(string_token) = literal_expr.get_literal()? else {
                    return None;
                };
                return Some(string_token.get_value().to_string());
            }
            LuaExpr::ParenExpr(paren_expr) => {
                current = paren_expr.get_expr()?;
            }
            _ => return None,
        }
    }
}

pub(crate) fn vgui_register_table_type_decl_id(
    file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
) -> LuaTypeDeclId {
    LuaTypeDeclId::local(
        file_id,
        &format!(
            "__gmod_vgui_register_table_{}",
            u32::from(call.syntax_id.get_range().start())
        ),
    )
}

/// Register a panel name as a global variable with the panel class type.
fn register_global_panel(
    db: &mut DbIndex,
    file_id: FileId,
    panel_name: &str,
    call: &GmodScriptedClassCallMetadata,
) {
    use glua_parser::LuaSyntaxKind;

    let class_decl_id = LuaTypeDeclId::global(panel_name);
    let call_range = call.syntax_id.get_range();

    // Create a global declaration for the panel name
    let global_decl = LuaDecl::new(
        panel_name,
        file_id,
        call_range,
        LuaDeclExtra::Global {
            kind: LuaSyntaxKind::NameExpr.into(),
        },
        None,
    );

    let decl_id = global_decl.get_id();

    db.get_decl_index_mut().add_derived_decl(global_decl);

    // Register the global in the global index
    let file_order_key = db.get_vfs().file_order_key(&decl_id.file_id);
    db.get_global_index_mut()
        .add_global_decl(panel_name, decl_id, file_order_key);

    // Bind the panel class type to the global declaration
    write_type_cache(
        db,
        decl_id.into(),
        LuaTypeCache::InferType(LuaType::Def(class_decl_id)),
        TypeCacheWriteMode::ForceOverwrite,
    );
}

/// Checks whether the call at the given metadata is `vgui.RegisterTable`
/// (not `vgui.CreateFromTable`). Both use the `register_table` call_arg
/// kind and land in `vgui_register_table_calls`, but only `RegisterTable`
/// actually registers a panel class. `CreateFromTable` instantiates from
/// an already-registered table and should not populate the dedup set.
pub(crate) fn is_vgui_register_table_call(
    db: &DbIndex,
    file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
) -> bool {
    let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) else {
        return true; // conservative: assume RegisterTable when we can't check
    };
    let range = call.syntax_id.get_range();
    let Some(token) = tree
        .get_chunk_node()
        .syntax()
        .token_at_offset(range.start())
        .right_biased()
    else {
        return true;
    };
    let Some(call_expr) = token.parent_ancestors().find_map(LuaCallExpr::cast) else {
        return true;
    };
    let Some(prefix) = call_expr.get_prefix_expr() else {
        return true;
    };
    match prefix {
        LuaExpr::IndexExpr(index_expr) => match index_expr.get_index_key() {
            Some(LuaIndexKey::Name(name)) => name.get_name_text() == "RegisterTable",
            _ => true,
        },
        _ => true,
    }
}

fn vgui_table_arg_is_registered_result(
    db: &DbIndex,
    file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
    table_source: &GmodClassCallArgSource,
    position: TextSize,
    actual_register_table_positions: &HashSet<TextSize>,
) -> bool {
    let Some(GmodClassCallLiteral::NameRef(table_var)) = call.value_for_arg_source(table_source)
    else {
        return false;
    };
    let Some((decl_id, region_start)) =
        resolve_local_registration_region(db, file_id, table_var, position)
    else {
        return false;
    };
    if region_start != decl_id.position {
        return false;
    }
    let Some((0, LuaExpr::CallExpr(register_call))) = local_initializer_expr(db, decl_id) else {
        return false;
    };

    actual_register_table_positions.contains(&register_call.get_range().start())
}

fn find_registered_table_expr_at_write_position(
    db: &DbIndex,
    file_id: FileId,
    write_position: TextSize,
) -> Option<LuaTableExpr> {
    let tree = db.get_vfs().get_syntax_tree(&file_id)?;
    let chunk = tree.get_chunk_node();

    // Find the name node at the write position, then walk up to its enclosing
    // statement and select the RHS expression at the matching variable index.
    let name_token = chunk
        .syntax()
        .token_at_offset(write_position)
        .right_biased()?;

    for ancestor in name_token.parent_ancestors() {
        if let Some(local_stat) = LuaLocalStat::cast(ancestor.clone()) {
            let names: Vec<LuaLocalName> = local_stat.get_local_name_list().collect();
            let values: Vec<LuaExpr> = local_stat.get_value_exprs().collect();
            let var_index = names.iter().position(|name| {
                name.get_name_token()
                    .is_some_and(|tok| tok.syntax().text_range().start() == write_position)
            })?;
            return value_expr_as_table(values.get(var_index)?);
        }

        if let Some(assign_stat) = LuaAssignStat::cast(ancestor.clone()) {
            let (vars, exprs) = assign_stat.get_var_and_expr_list();
            let var_index = vars
                .iter()
                .position(|var| var.syntax().text_range().start() == write_position)?;
            return value_expr_as_table(exprs.get(var_index)?);
        }
    }

    None
}

/// Unwrap parenthesized expressions and require a table constructor.
fn value_expr_as_table(expr: &LuaExpr) -> Option<LuaTableExpr> {
    let mut current = expr.clone();
    loop {
        match current {
            LuaExpr::TableExpr(table_expr) => return Some(table_expr),
            LuaExpr::ParenExpr(paren_expr) => {
                current = paren_expr.get_expr()?;
            }
            _ => return None,
        }
    }
}

fn synthesize_panel_class(
    db: &mut DbIndex,
    cache: &mut VguiSynthesisCache,
    file_id: FileId,
    panel_name: &str,
    table_var_name: Option<&str>,
    base_panel: Option<&str>,
    call_kind: GmodScriptedClassCallKind,
    call: &GmodScriptedClassCallMetadata,
    resolved_registration: Option<ResolvedVguiRegistrationRegion>,
    phase: VguiSynthesisPhase,
) {
    let class_decl_id = LuaTypeDeclId::global(panel_name);
    synthesize_panel_class_with_id(
        db,
        cache,
        file_id,
        class_decl_id,
        table_var_name,
        base_panel,
        call_kind,
        call,
        resolved_registration,
        phase,
    );
}

fn synthesize_panel_class_with_id(
    db: &mut DbIndex,
    cache: &mut VguiSynthesisCache,
    file_id: FileId,
    class_decl_id: LuaTypeDeclId,
    table_var_name: Option<&str>,
    base_panel: Option<&str>,
    call_kind: GmodScriptedClassCallKind,
    call: &GmodScriptedClassCallMetadata,
    resolved_registration: Option<ResolvedVguiRegistrationRegion>,
    phase: VguiSynthesisPhase,
) {
    let bind = phase == VguiSynthesisPhase::Bind;
    // Every registering site adds its location unless a doc tag declares the
    // class: whichever registration ran first must not own the declaration.
    if bind
        && db
            .get_type_index()
            .get_source_type_decl(&class_decl_id)
            .is_none()
    {
        let type_flag = if call_kind == GmodScriptedClassCallKind::VguiRegisterTable {
            LuaTypeFlag::AutoGenerated
        } else {
            LuaTypeFlag::None
        };
        db.get_type_index_mut().add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                call.syntax_id.get_range(),
                class_decl_id.get_simple_name().to_string(),
                LuaDeclTypeKind::Class,
                type_flag.into(),
                class_decl_id.clone(),
            ),
        );
    }

    // Set super type from base panel
    if let Some(base_name) = base_panel.filter(|_| bind) {
        let super_type = LuaType::Ref(LuaTypeDeclId::global(base_name));
        db.get_type_index_mut().add_super_type(
            class_decl_id.clone(),
            file_id,
            call.syntax_id.get_range(),
            super_type,
        );
        synthesize_panel_baseclass_member(db, file_id, &class_decl_id, base_name, call_kind, call);
    }

    // Bind the table variable to the panel class.
    //
    // VGUI files reuse a single `local PANEL` decl with repeated plain
    // reassignments (`PANEL = {}`), one per registered class. The class
    // identity belongs to each concrete table value (the `{}` literal), NOT to
    // the shared decl slot. Binding the decl slot collapses every region onto a
    // single class (last-write-wins), which is the root cause of the
    // reassigned-PANEL mis-binding. Instead we bind the class to the exact
    // table-constructor expression via `LuaTypeOwner::SyntaxId`, which the
    // public `infer_expr` override consults — yielding correct per-region
    // resolution for hover, diagnostics, completion and CodeLens uniformly.
    if let Some(var_name) = table_var_name {
        let register_position = call.syntax_id.get_range().start();
        let Some(resolved_registration) = resolved_registration.or_else(|| {
            resolve_local_registration_region(db, file_id, var_name, register_position).map(
                |(decl_id, region_start)| ResolvedVguiRegistrationRegion {
                    decl_id,
                    region_start,
                },
            )
        }) else {
            return;
        };
        let decl_id = resolved_registration.decl_id;
        let region_start = resolved_registration.region_start;

        let class_type = LuaType::Def(class_decl_id.clone());
        let latest_write_position = Some(region_start);

        // Resolve the concrete `{}` table literal backing this registration.
        let registered_table = cached_registered_table_expr(cache, db, file_id, region_start);

        if let Some(table_expr) = registered_table.as_ref().filter(|_| bind) {
            // Bind the class to this exact table-constructor expression.
            // Preserve any user `@as`/cast (DocType) binding already present.
            let table_syntax_owner =
                LuaTypeOwner::SyntaxId(InFiled::new(file_id, table_expr.get_syntax_id()));
            let preserve_doc = db
                .get_type_index()
                .get_type_cache(&table_syntax_owner)
                .is_some_and(|cache| cache.is_doc());
            if !preserve_doc {
                write_type_cache(
                    db,
                    table_syntax_owner,
                    LuaTypeCache::InferType(class_type.clone()),
                    TypeCacheWriteMode::ForceOverwrite,
                );
            }
            // Rewrite that exact table-const range so persistent type caches
            // (cross-file accesses, exports) resolve to the class; scoped to
            // this range only, never carried forward across registrations.
            cache.table_const_replacements.insert(
                InFiled::new(file_id, table_expr.get_range()),
                class_type.clone(),
            );
        }

        if bind && !cached_decl_has_reassignment(cache, db, file_id, decl_id) {
            // For single-panel files the `PANEL` local has one stable identity.
            // Bind the decl slot too so method-self collection during the Lua
            // pass sees the synthesized class before it caches member values.
            // Reassigned locals remain table-literal-only to avoid collapsing
            // distinct registration regions onto one class.
            bind_panel_table_decl(db, decl_id, &class_type);
        }

        // Transfer the members defined in this registration's table region to
        // the class.
        if let Some(table_expr) = registered_table.as_ref().filter(|_| !bind) {
            let table_range = InFiled::new(file_id, table_expr.get_range());
            let class_member_owner = LuaMemberOwner::Type(class_decl_id.clone());

            // Members defined via `function PANEL:Method()` / `PANEL.Field =`
            // are collected during the `lua` analysis pass — which runs BEFORE
            // this gmod post-analysis SyntaxId binding exists. At that point the
            // flow inference of the reused `PANEL` local resolves to its
            // *initializer* table literal, so EVERY region's members accumulate
            // under that single `Element` owner, differentiated only by source
            // position. The per-region table literal's own `Element` owner is
            // therefore usually empty.
            //
            // To bridge synthesis (which knows the per-region boundary) with
            // collection (which keyed everything on the initializer table), we
            // gather all candidate member-source `Element` owners and slice them
            // by source position `[latest_write_position, register_position)`.
            // This stays correct if a future flow-aware collector starts keying
            // members under the per-region literal instead.
            let member_source_ranges =
                collect_panel_member_source_ranges(cache, db, file_id, decl_id, &table_range);

            let mut table_member_ids = HashSet::default();
            for (source_idx, source_range) in member_source_ranges.iter().enumerate() {
                let is_initializer_fallback = source_idx > 0;
                let source_owner = LuaMemberOwner::Element(source_range.clone());
                let members = db
                    .get_member_index()
                    .get_current_owner_member_history(&source_owner);
                if !members.is_empty() {
                    for member in members {
                        let member_id = member.get_id();
                        // The `Element` owner canonicalizes through the global
                        // path's class binding (e.g. a global `PANEL = {}`
                        // literal resolves to `Type(PANEL)`), so the history
                        // can contain members from *other* files. Region
                        // boundaries are text positions and only comparable
                        // within this file.
                        if member_id.file_id != file_id {
                            continue;
                        }
                        let member_position = member_id.get_position();
                        if member_position < register_position
                            && latest_write_position
                                .map(|write_position| member_position >= write_position)
                                .unwrap_or(true)
                        {
                            // For the initializer table fallback, verify the member
                            // was defined using the registered variable name. Members
                            // defined through aliases (e.g. `local OLD = PANEL;
                            // function OLD:Method()`) must not be transferred to the
                            // new panel class.
                            if is_initializer_fallback
                                && !member_defined_via_variable(
                                    db,
                                    file_id,
                                    member_position,
                                    var_name,
                                )
                            {
                                continue;
                            }
                            table_member_ids.insert(member_id);
                        }
                    }
                }
            }

            // A derma file conventionally uses the *global* `PANEL` scratch
            // table (`PANEL = {}` … `function PANEL:Paint()` …
            // `vgui.Register("X", PANEL, "DButton")`). At runtime that
            // table is consumed by the register call and the next file
            // overwrites the global, so each file's `PANEL` is a separate
            // class — exactly like `ENT`/`SWEP`, which are modelled as
            // scoped class globals.
            for global_owner in global_panel_member_owners(db, var_name) {
                let members = db
                    .get_member_index()
                    .get_current_owner_member_history(&global_owner);
                for member in members {
                    let member_id = member.get_id();
                    if member_id.file_id != file_id {
                        continue;
                    }
                    let member_position = member_id.get_position();
                    if member_position >= register_position {
                        continue;
                    }
                    if latest_write_position
                        .is_some_and(|write_position| member_position < write_position)
                    {
                        continue;
                    }
                    if !member_defined_via_variable(db, file_id, member_position, var_name) {
                        continue;
                    }
                    table_member_ids.insert(member_id);
                }
            }

            let mut table_member_ids = table_member_ids.into_iter().collect::<Vec<_>>();
            table_member_ids
                .sort_unstable_by_key(|member_id| (member_id.file_id.id, member_id.get_position()));
            for member_id in table_member_ids {
                db.get_member_index_mut().transfer_member(
                    class_member_owner.clone(),
                    member_id,
                    true,
                );
            }
        }
    } else if let Some(table_expr) = find_inline_vgui_panel_table_expr(db, file_id, call_kind, call)
    {
        bind_inline_vgui_panel_table(db, cache, file_id, &class_decl_id, table_expr, phase);
        // The local the call initialises is the registered table: its methods
        // define the class's members, as a `local PANEL = {}` declaration's do.
        if phase == VguiSynthesisPhase::Bind
            && let Some(decl_id) = local_decl_for_call_result(db, file_id, call.syntax_id)
            && !cached_decl_has_reassignment(cache, db, file_id, decl_id)
        {
            bind_panel_table_decl(db, decl_id, &LuaType::Def(class_decl_id.clone()));
        }
    }
}

/// Types a panel table's declaration with the class registered from it.
///
/// A global `PANEL` here is one file's scratch table, not a name every file's
/// `PANEL` shares, so the class is not proposed as the global path's class:
/// a losing proposal would send this file's members to the path's winning
/// class whenever it was on record when they were filed.
fn bind_panel_table_decl(db: &mut DbIndex, decl_id: LuaDeclId, class_type: &LuaType) {
    db.get_type_index_mut()
        .force_bind_type(decl_id.into(), LuaTypeCache::InferType(class_type.clone()));
}

fn find_inline_vgui_panel_table_expr(
    db: &DbIndex,
    file_id: FileId,
    call_kind: GmodScriptedClassCallKind,
    call: &GmodScriptedClassCallMetadata,
) -> Option<LuaTableExpr> {
    let table_source = match call_kind {
        GmodScriptedClassCallKind::VguiRegister => call.vgui_panel_table_arg_source(1),
        GmodScriptedClassCallKind::VguiRegisterTable => call.vgui_panel_table_arg_source(0),
        GmodScriptedClassCallKind::DermaDefineControl => call.vgui_panel_table_arg_source(2),
        _ => return None,
    };

    find_table_expr_for_arg_source(db, file_id, call, &table_source)
}

fn find_table_expr_for_arg_source(
    db: &DbIndex,
    file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
    table_source: &GmodClassCallArgSource,
) -> Option<LuaTableExpr> {
    let arg_range = if !table_source.is_derived() {
        call.args.get(table_source.arg_idx)?.syntax_id.get_range()
    } else {
        call.field_args
            .iter()
            .find(|arg| &arg.source == table_source)?
            .syntax_id
            .get_range()
    };

    let tree = db.get_vfs().get_syntax_tree(&file_id)?;
    let chunk = tree.get_chunk_node();
    chunk.descendants::<LuaTableExpr>().find(|table_expr| {
        let table_range = table_expr.get_range();
        table_range == arg_range
            || (arg_range.start() <= table_range.start() && table_range.end() <= arg_range.end())
    })
}

fn bind_inline_vgui_panel_table(
    db: &mut DbIndex,
    cache: &mut VguiSynthesisCache,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    table_expr: LuaTableExpr,
    phase: VguiSynthesisPhase,
) {
    let table_range = InFiled::new(file_id, table_expr.get_range());
    if phase == VguiSynthesisPhase::TransferMembers {
        let source_owner = LuaMemberOwner::Element(table_range);
        let class_member_owner = LuaMemberOwner::Type(class_decl_id.clone());
        let table_member_ids: Vec<_> = db
            .get_member_index()
            .get_members(&source_owner)
            .map(|members| members.iter().map(|member| member.get_id()).collect())
            .unwrap_or_default();
        for member_id in table_member_ids {
            db.get_member_index_mut()
                .transfer_member(class_member_owner.clone(), member_id, false);
        }
        return;
    }

    let class_type = LuaType::Def(class_decl_id.clone());
    let table_syntax_owner =
        LuaTypeOwner::SyntaxId(InFiled::new(file_id, table_expr.get_syntax_id()));
    let preserve_doc = db
        .get_type_index()
        .get_type_cache(&table_syntax_owner)
        .is_some_and(|cache| cache.is_doc());
    if !preserve_doc {
        write_type_cache(
            db,
            table_syntax_owner,
            LuaTypeCache::InferType(class_type.clone()),
            TypeCacheWriteMode::ForceOverwrite,
        );
    }
    cache
        .table_const_replacements
        .insert(table_range, class_type);
}

/// Owners a *global* panel-table variable's members can be sitting on.
///
/// Decl analysis parks `PANEL.Field` / `function PANEL:Method()` under
/// `GlobalPath("PANEL")`; once the path's `---@class` is recorded, the canonical
/// owner of that path becomes the class, which for GMod workspaces is the
/// annotation `@class PANEL`. Both are checked so the lookup works whichever
/// side of that the member is on.
fn global_panel_member_owners(db: &DbIndex, var_name: &str) -> Vec<LuaMemberOwner> {
    let mut owners = vec![LuaMemberOwner::GlobalPath(GlobalId::new(var_name))];
    let type_decl_id = LuaTypeDeclId::global(var_name);
    if db.get_type_index().get_type_decl(&type_decl_id).is_some() {
        owners.push(LuaMemberOwner::Type(type_decl_id));
    }
    owners
}

/// Collect the candidate `Element` owner ranges that may hold this
/// registration region's members, deduped and most-specific first.
///
/// `function PANEL:Method()` member collection happens in the `lua` pass before
/// the gmod-post SyntaxId binding exists, so members of reused locals end up
/// under the local's *initializer* table `Element` owner rather than each
/// region's own table literal. We therefore consider:
///
/// 1. the exact per-region table literal range (precise / future-proof), and
/// 2. the original local declaration's initializer `TableConst` range (where
///    the lua pass actually accumulated the members today).
///
/// Callers slice the resulting members by source position to attribute them to
/// the correct region.
fn collect_panel_member_source_ranges(
    cache: &mut VguiSynthesisCache,
    db: &DbIndex,
    file_id: FileId,
    decl_id: LuaDeclId,
    region_table_range: &InFiled<TextRange>,
) -> Vec<InFiled<TextRange>> {
    let mut ranges: Vec<InFiled<TextRange>> = Vec::with_capacity(2);
    ranges.push(region_table_range.clone());

    // The original local decl's initializer table literal (`local PANEL = {}`)
    // is the `Element` owner the lua pass keyed all reused-local members under.
    //
    // We derive this range from the AST rather than the decl type cache: VGUI
    // synthesis rewrites table-const caches after collecting region members, so
    // cache state is intentionally not the source of truth here.
    if let Some(initializer_range) =
        cached_decl_initializer_table_range(cache, db, file_id, decl_id)
        && !ranges.iter().any(|existing| existing == &initializer_range)
    {
        ranges.push(initializer_range);
    }

    ranges
}

fn cached_decl_initializer_table_range(
    cache: &mut VguiSynthesisCache,
    db: &DbIndex,
    file_id: FileId,
    decl_id: LuaDeclId,
) -> Option<InFiled<TextRange>> {
    let key = (file_id, text_size_key(decl_id.position));
    if let Some(initializer_range) = cache.initializer_table_ranges.get(&key) {
        return initializer_range.clone();
    }

    let initializer_range = find_decl_initializer_table_range(db, file_id, decl_id);
    cache
        .initializer_table_ranges
        .insert(key, initializer_range.clone());
    initializer_range
}

/// Find the range of the table literal in a local declaration's initializer
/// (`local PANEL = {}` -> range of `{}`), derived purely from the AST so it is
/// stable against type-cache mutation during synthesis.
fn find_decl_initializer_table_range(
    db: &DbIndex,
    file_id: FileId,
    decl_id: LuaDeclId,
) -> Option<InFiled<TextRange>> {
    let tree = db.get_vfs().get_syntax_tree(&file_id)?;
    let chunk = tree.get_chunk_node();
    let name_token = chunk
        .syntax()
        .token_at_offset(decl_id.position)
        .right_biased()?;

    for ancestor in name_token.parent_ancestors() {
        if let Some(local_stat) = LuaLocalStat::cast(ancestor.clone()) {
            let names: Vec<LuaLocalName> = local_stat.get_local_name_list().collect();
            let values: Vec<LuaExpr> = local_stat.get_value_exprs().collect();
            let var_index = names.iter().position(|name| {
                name.get_name_token()
                    .is_some_and(|tok| tok.syntax().text_range().start() == decl_id.position)
            })?;
            let table_expr = value_expr_as_table(values.get(var_index)?)?;
            return Some(InFiled::new(file_id, table_expr.get_range()));
        }
    }

    None
}

/// Returns true when the local decl has at least one write that is not its
/// initial declaration position — i.e. it is reassigned (`PANEL = {}`) after
/// the original `local PANEL`. Used to keep the single-panel decl-binding
/// compatibility path from contaminating reused locals.
fn decl_has_reassignment(db: &DbIndex, file_id: FileId, decl_id: LuaDeclId) -> bool {
    db.get_reference_index()
        .get_decl_references(&file_id, &decl_id)
        .is_some_and(|decl_references| {
            decl_references.is_written_after_declaration(decl_id.position)
        })
}

/// Check if a member at the given position was defined using a specific
/// variable name. Walks up from the member's syntax position to find the
/// enclosing `function VAR:Method()` / `VAR.Field = value` and checks the
/// prefix variable name.
///
/// Returns `true` (conservative include) when the variable name cannot be
/// determined, so callers don't accidentally drop members they can't trace.
fn member_defined_via_variable(
    db: &DbIndex,
    file_id: FileId,
    member_position: TextSize,
    var_name: &str,
) -> bool {
    let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) else {
        return true;
    };
    let chunk = tree.get_chunk_node();
    let Some(token) = chunk
        .syntax()
        .token_at_offset(member_position)
        .right_biased()
    else {
        return true;
    };

    for ancestor in token.parent_ancestors() {
        if let Some(func_stat) = LuaFuncStat::cast(ancestor.clone()) {
            if let Some(LuaVarExpr::IndexExpr(index_expr)) = func_stat.get_func_name() {
                return index_expr_prefix_matches(&index_expr, var_name);
            }
            return false;
        }
        if let Some(assign_stat) = LuaAssignStat::cast(ancestor.clone()) {
            let (vars, exprs) = assign_stat.get_var_and_expr_list();
            if !vars.iter().any(|var| var.get_range().contains(member_position))
                && !exprs.iter().any(|expr| matches!(expr, LuaExpr::ClosureExpr(closure) if closure.get_range().contains(member_position)))
            {
                continue;
            }
            for var in vars {
                if let LuaVarExpr::IndexExpr(index_expr) = &var {
                    if index_expr_prefix_matches(index_expr, var_name) {
                        return true;
                    }
                }
            }
            return false;
        }
    }

    true
}

fn index_expr_prefix_matches(index_expr: &glua_parser::LuaIndexExpr, var_name: &str) -> bool {
    if let Some(LuaExpr::NameExpr(prefix)) = index_expr.get_prefix_expr() {
        prefix.get_name_text().as_deref() == Some(var_name)
    } else {
        false
    }
}

fn synthesize_panel_baseclass_member(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    base_name: &str,
    call_kind: GmodScriptedClassCallKind,
    call: &GmodScriptedClassCallMetadata,
) {
    let base_arg_source = match call_kind {
        GmodScriptedClassCallKind::VguiRegister => call.vgui_panel_base_arg_source(Some(2)),
        GmodScriptedClassCallKind::VguiRegisterTable => call.vgui_panel_base_arg_source(Some(1)),
        GmodScriptedClassCallKind::DermaDefineControl => call.vgui_panel_base_arg_source(Some(3)),
        _ => return,
    };

    let syntax_id = base_arg_source
        .as_ref()
        .and_then(|source| {
            if !source.is_derived() {
                call.args.get(source.arg_idx).map(|arg| arg.syntax_id)
            } else {
                call.field_args
                    .iter()
                    .find(|arg| &arg.source == source)
                    .map(|arg| arg.syntax_id)
            }
        })
        .unwrap_or(call.syntax_id);
    synthesize_baseclass_member(db, file_id, class_decl_id, base_name, syntax_id);
}

fn synthesize_baseclass_member(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    base_name: &str,
    syntax_id: LuaSyntaxId,
) {
    if base_name.is_empty() {
        return;
    }

    // Every site that names the base declares `BaseClass` itself: skipping one
    // that an earlier site's member already covers would let whichever file
    // analysed first own the record, and its realm or position then decide
    // which readers see the base.
    let owner = LuaMemberOwner::Type(class_decl_id.clone());
    let member_key = LuaMemberKey::Name("BaseClass".into());
    let member_id = LuaMemberId::new(syntax_id, file_id);
    let member = LuaMember::new(member_id, member_key, LuaMemberFeature::FileFieldDecl, None);
    db.get_member_index_mut().add_member(owner, member);
    write_type_cache(
        db,
        member_id.into(),
        LuaTypeCache::DocType(LuaType::Ref(LuaTypeDeclId::global(base_name))),
        TypeCacheWriteMode::InsertOnly,
    );
}

/// Resolve AccessorFunc force type argument to a LuaType.
fn resolve_accessor_force_type(force_arg: Option<&GmodClassCallLiteral>) -> LuaType {
    match force_arg {
        Some(GmodClassCallLiteral::NameRef(name)) => match name.as_str() {
            "FORCE_STRING" => LuaType::String,
            "FORCE_NUMBER" => LuaType::Number,
            "FORCE_BOOL" => LuaType::Boolean,
            "FORCE_ANGLE" => LuaType::Ref(LuaTypeDeclId::global("Angle")),
            "FORCE_COLOR" => LuaType::Ref(LuaTypeDeclId::global("Color")),
            "FORCE_VECTOR" => LuaType::Ref(LuaTypeDeclId::global("Vector")),
            _ => LuaType::Any,
        },
        Some(GmodClassCallLiteral::Integer(n)) => match *n {
            1 => LuaType::String,
            2 => LuaType::Number,
            3 => LuaType::Boolean,
            4 => LuaType::Ref(LuaTypeDeclId::global("Angle")),
            5 => LuaType::Ref(LuaTypeDeclId::global("Color")),
            6 => LuaType::Ref(LuaTypeDeclId::global("Vector")),
            _ => LuaType::Any,
        },
        Some(GmodClassCallLiteral::Unsigned(n)) => match *n {
            1 => LuaType::String,
            2 => LuaType::Number,
            3 => LuaType::Boolean,
            4 => LuaType::Ref(LuaTypeDeclId::global("Angle")),
            5 => LuaType::Ref(LuaTypeDeclId::global("Color")),
            6 => LuaType::Ref(LuaTypeDeclId::global("Vector")),
            _ => LuaType::Any,
        },
        Some(GmodClassCallLiteral::Boolean(true)) => LuaType::Boolean,
        _ => LuaType::Any,
    }
}

fn resolve_accessor_setter_input_type(force_arg: Option<&GmodClassCallLiteral>) -> LuaType {
    if force_arg.is_some() {
        LuaType::Any
    } else {
        resolve_accessor_force_type(force_arg)
    }
}

/// Resolve NetworkVar type name to a LuaType.
fn resolve_networkvar_type(type_name: &str) -> LuaType {
    match type_name {
        "String" => LuaType::String,
        "Bool" => LuaType::Boolean,
        "Float" | "Double" => LuaType::Number,
        "Int" | "UInt" => LuaType::Integer,
        "Vector" => LuaType::Ref(LuaTypeDeclId::global("Vector")),
        "Angle" => LuaType::Ref(LuaTypeDeclId::global("Angle")),
        "Entity" => LuaType::Ref(LuaTypeDeclId::global("Entity")),
        "Color" => LuaType::Ref(LuaTypeDeclId::global("Color")),
        _ => {
            log::warn!(
                "Unknown NetworkVar type '{}', defaulting to Any. Valid types are: \
                String, Bool, Float, Double, Int, UInt, Vector, Angle, Entity, Color",
                type_name
            );
            LuaType::Any
        }
    }
}

fn detect_scoped_class_from_path(db: &DbIndex, file_id: FileId) -> Option<GmodScopedClassMatch> {
    let file_path = db.get_vfs().get_file_path(&file_id)?;
    if let Some(scope_match) = db
        .get_file_emmyrc(file_id)
        .gmod
        .scripted_class_scopes
        .detect_class_for_path(file_path)
    {
        return Some(GmodScopedClassMatch {
            global_name: scope_match.definition.class_global,
            class_name: scope_match.class_name,
            is_global_singleton: scope_match.definition.is_global_singleton,
            aliases: scope_match.definition.aliases,
            super_types: scope_match.definition.super_types,
            class_name_prefix: scope_match.definition.class_name_prefix,
        });
    }

    let (gamemode_dir, scope_match) = detect_root_gamemode_scope(db, file_id)?;
    file_path.starts_with(&gamemode_dir).then_some(scope_match)
}

/// [`root_gamemode_scope`] of `file_id`'s workspace.
pub(crate) fn detect_root_gamemode_scope(
    db: &DbIndex,
    file_id: FileId,
) -> Option<(PathBuf, GmodScopedClassMatch)> {
    root_gamemode_scope(db, db.get_module_index().get_workspace_id(file_id)?)
}

/// The gamemode `file_id` belongs to: the `gamemodes/<name>` folder its path
/// runs through below its main workspace root, else the workspace root
/// itself when that is a gamemode folder ([`root_gamemode_scope`]).
pub(crate) fn detect_file_gamemode_scope(
    db: &DbIndex,
    file_id: FileId,
) -> Option<(PathBuf, GmodScopedClassMatch)> {
    let module_index = db.get_module_index();
    let workspace_id = module_index.get_workspace_id(file_id)?;
    if !module_index.is_main_workspace_id(workspace_id) {
        return None;
    }
    let root = module_index.get_workspace_root(workspace_id)?;
    let folder_scope = db
        .get_vfs()
        .get_file_path(&file_id)
        .and_then(|path| path.strip_prefix(root).ok())
        .and_then(|relative| {
            let parts = relative.iter().collect::<Vec<_>>();
            let at = parts
                .windows(3)
                .rposition(|window| window[0].eq_ignore_ascii_case("gamemodes"))?;
            let folder = root.join(parts[..at + 2].iter().collect::<PathBuf>());
            gamemode_folder_scope(db, db.get_workspace_emmyrc(workspace_id), &folder)
        });
    folder_scope.or_else(|| root_gamemode_scope(db, workspace_id))
}

/// Detect the "workspace root *is* a gamemode folder" layout: `gamemode/`
/// sits directly under the workspace root (e.g. opening a `gamemodes/<name>`
/// checkout itself in the editor), so no `gamemodes/<name>` segment exists
/// for the `gamemodes` scope path rule to anchor on.
///
/// Returns the gamemode directory and a scope match named
/// `gamemode_<root dir name>` (the same identifier `gamemode.Register`
/// derives from the folder at runtime). The layout is told by an entry file
/// being there, a probe of each entry path that creating, deleting or moving
/// a file there re-runs. Callers decide whether a given file lives under the
/// returned directory; files already claimed by another scope (e.g.
/// `gamemode/schema.lua` → `SCHEMA`) keep their match.
pub(crate) fn root_gamemode_scope(
    db: &DbIndex,
    workspace_id: crate::WorkspaceId,
) -> Option<(PathBuf, GmodScopedClassMatch)> {
    if !db.get_module_index().is_main_workspace_id(workspace_id) {
        return None;
    }
    let root = db.get_module_index().get_workspace_root(workspace_id)?;
    gamemode_folder_scope(db, db.get_workspace_emmyrc(workspace_id), &root)
}

/// The scope match of the gamemode folder `folder` when an entry file sits in
/// its `gamemode/` directory, with that directory.
fn gamemode_folder_scope(
    db: &DbIndex,
    emmyrc: &crate::Emmyrc,
    folder: &Path,
) -> Option<(PathBuf, GmodScopedClassMatch)> {
    let gamemode_dir = folder.join("gamemode");
    let has_entry = ["init.lua", "shared.lua", "cl_init.lua"]
        .iter()
        .any(|name| {
            crate::vfs::file_path_to_uri(&gamemode_dir.join(name))
                .and_then(|uri| db.get_vfs().file_at(&uri))
                .is_some()
        });
    if !has_entry {
        return None;
    }

    let name = folder.file_name()?.to_str()?;
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }

    let definition = emmyrc
        .gmod
        .scripted_class_scopes
        .resolved_definitions_slice()
        .iter()
        .find(|definition| definition.id == "gamemodes")?
        .clone();
    let class_name = match definition.class_name_prefix.as_deref() {
        Some(prefix) => format!("{prefix}{name}"),
        None => name.to_string(),
    };

    Some((
        gamemode_dir,
        GmodScopedClassMatch {
            global_name: definition.class_global,
            class_name,
            is_global_singleton: definition.is_global_singleton,
            aliases: definition.aliases,
            super_types: definition.super_types,
            class_name_prefix: definition.class_name_prefix,
        },
    ))
}

/// Returns the scripted class info `(class_name, global_name)` for a file, if it belongs to a
/// GMod scripted class scope.  `global_name` is the well-known table name used in the file
/// (e.g. `"ENT"`, `"SWEP"`, `"TOOL"`, `"EFFECT"`, `"PLUGIN"`).
/// Uses cached scoped class info when available, falling back to path detection.
pub fn get_scripted_class_info_for_file(db: &DbIndex, file_id: FileId) -> Option<(String, String)> {
    get_scripted_class_info_with_prefix(db, file_id).map(|(c, g, _)| (c, g))
}

/// Like [`get_scripted_class_info_for_file`] but also returns the scope's
/// `class_name_prefix`, so callers can correctly strip it to recover the
/// folder short-name (used for parent-alias synthesis on inherited classes).
pub(crate) fn get_scripted_class_info_with_prefix(
    db: &DbIndex,
    file_id: FileId,
) -> Option<(String, String, Option<String>)> {
    if let Some(info) = db.get_gmod_infer_index().get_scoped_class_info(&file_id) {
        return Some((
            info.class_name.clone(),
            info.global_name.clone(),
            info.class_name_prefix.clone(),
        ));
    }
    detect_scoped_class_from_path(db, file_id)
        .map(|m| (m.class_name, m.global_name, m.class_name_prefix))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum GmodSystemCallKind {
    AddNetworkString,
    NetStart,
    NetReceive,
    ConcommandAdd,
    CreateConVar,
    CreateClientConVar,
    TimerCreate,
    TimerSimple,
}

#[derive(Debug, Clone, Copy)]
struct GmodSystemCallSite {
    kind: GmodSystemCallKind,
    name_arg_idx: Option<usize>,
    callback_arg_idx: Option<usize>,
}

impl std::fmt::Debug for AnnotatedGmodGlobalCallRoleMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The contents are a large derived lookup table; a summary keeps
        // `AnalyzeContext`'s `Debug` useful without dumping thousands of rows.
        f.debug_struct("AnnotatedGmodGlobalCallRoleMap")
            .field("paths", &self.roles_by_path.len())
            .finish_non_exhaustive()
    }
}

#[derive(Default, Clone)]
pub(crate) struct AnnotatedGmodGlobalCallRoleMap {
    roles_by_path: HashMap<String, AnnotatedGmodCallRoles>,
    receiver_paths: HashMap<SmolStr, HashSet<String>>,
    /// Each path's roles fingerprinted once, when its file's scan is built.
    path_fingerprints: HashMap<String, crate::db_index::derivation::Fingerprint>,
    /// Canonical function metadata per `(wire_format, direction)`, published to
    /// the network index for features that must emit a net call. Values keep
    /// their precedence rank so the winner does not depend on the signature
    /// index's iteration order.
    canonical_net_ops:
        HashMap<(SmolStr, NetOpDirection), (CanonicalNetOpRank, crate::db_index::CanonicalNetOp)>,
}

struct AnnotatedGmodCallRoleMap<'a> {
    global_roles: &'a AnnotatedGmodGlobalCallRoleMap,
    local_roles_by_decl: HashMap<LuaDeclId, AnnotatedGmodCallRoles>,
    local_roles_by_path: HashMap<(LuaDeclId, String), AnnotatedGmodCallRoles>,
    local_candidate_names: HashSet<String>,
    file_id: FileId,
}

#[derive(Clone, Default, Hash)]
struct AnnotatedGmodCallArgRole {
    param_idx: usize,
    priority: i64,
    field_path: Vec<String>,
    prefix: String,
    suffix: String,
}

enum AnnotatedVguiParentSource {
    Arg(GmodClassCallArgSource),
    Receiver {
        field_path: Vec<String>,
        dot_source: GmodClassCallArgSource,
    },
}

struct AnnotatedVguiParentCallRoles {
    child: AnnotatedVguiParentSource,
    parent: AnnotatedVguiParentSource,
}

impl AnnotatedGmodCallArgRole {
    fn from_role(role: &LuaCallArgRole) -> Self {
        Self {
            param_idx: role.param_idx,
            priority: role.priority.unwrap_or(0),
            field_path: role.field_path.clone(),
            prefix: String::new(),
            suffix: String::new(),
        }
    }

    fn sort_key(&self) -> (std::cmp::Reverse<i64>, usize) {
        (std::cmp::Reverse(self.priority), self.param_idx)
    }

    fn to_arg_source(
        &self,
        is_colon_call: bool,
        is_colon_define: bool,
    ) -> Option<crate::GmodClassCallArgSource> {
        Some(crate::GmodClassCallArgSource {
            arg_idx: param_idx_to_call_arg_idx(self.param_idx, is_colon_call, is_colon_define)?,
            field_path: self.field_path.clone(),
            prefix: self.prefix.clone(),
            suffix: self.suffix.clone(),
        })
    }
}

#[derive(Clone, Default, Hash)]
struct AnnotatedGmodCallRoles {
    is_colon_define: bool,
    params: Vec<Option<LuaType>>,
    optional_params: Vec<bool>,
    is_variadic: bool,
    overloads: Vec<AnnotatedGmodCallRoles>,
    system_roles: Vec<(GmodSystemCallKind, AnnotatedGmodCallArgRole)>,
    system_callback_roles: Vec<(GmodSystemCallKind, AnnotatedGmodCallArgRole)>,
    hook_roles: Vec<(GmodHookKind, AnnotatedGmodCallArgRole)>,
    hook_callback_roles: Vec<AnnotatedGmodCallArgRole>,
    load_roles: Vec<(LuaDependencyKind, AnnotatedGmodCallArgRole)>,
    compilefile_roles: Vec<AnnotatedGmodCallArgRole>,
    environment_target_roles: Vec<AnnotatedGmodCallArgRole>,
    environment_table_roles: Vec<AnnotatedGmodCallArgRole>,
    file_find_glob_roles: Vec<AnnotatedGmodCallArgRole>,
    file_find_search_path_roles: Vec<AnnotatedGmodCallArgRole>,
    inheritance_roles: Vec<(GmodScriptedClassCallKind, AnnotatedGmodCallArgRole)>,
    network_var_kind: Option<GmodScriptedClassCallKind>,
    network_var_slot_only: bool,
    network_var_type_roles: Vec<AnnotatedGmodCallArgRole>,
    network_var_define_roles: Vec<AnnotatedGmodCallArgRole>,
    vgui_panel_kind: Option<GmodScriptedClassCallKind>,
    vgui_panel_define_roles: Vec<AnnotatedGmodCallArgRole>,
    vgui_panel_table_roles: Vec<AnnotatedGmodCallArgRole>,
    vgui_panel_base_roles: Vec<AnnotatedGmodCallArgRole>,
    vgui_panel_reference_roles: Vec<AnnotatedGmodCallArgRole>,
    vgui_panel_child_self_roles: Vec<AnnotatedGmodCallArgRole>,
    vgui_panel_parent_roles: Vec<AnnotatedGmodCallArgRole>,
    vgui_panel_parent_self_roles: Vec<AnnotatedGmodCallArgRole>,
    derma_skin_define_roles: Vec<AnnotatedGmodCallArgRole>,
    scripted_class_kind: Option<GmodScriptedClassCallKind>,
    scripted_class_define_roles: Vec<AnnotatedGmodCallArgRole>,
    scripted_class_table_roles: Vec<AnnotatedGmodCallArgRole>,
}

impl AnnotatedGmodCallRoles {
    fn from_signature_shape(signature: &LuaSignature) -> Self {
        Self {
            is_colon_define: signature.is_colon_define,
            params: signature
                .params
                .iter()
                .enumerate()
                .map(|(idx, _)| {
                    signature
                        .param_docs
                        .get(&idx)
                        .map(|param| param.type_ref.clone())
                })
                .collect(),
            optional_params: signature.get_param_optional_flags(),
            is_variadic: signature.is_vararg,
            ..Self::default()
        }
    }

    fn from_function_shape(func: &LuaFunctionType) -> Self {
        Self {
            is_colon_define: func.is_colon_define(),
            params: func
                .get_params()
                .iter()
                .map(|(_, typ)| typ.clone())
                .collect(),
            optional_params: func.get_optional_params().to_vec(),
            is_variadic: func.is_variadic(),
            ..Self::default()
        }
    }

    fn add_call_arg_role(&mut self, role: &LuaCallArgRole) {
        let arg_role = AnnotatedGmodCallArgRole::from_role(role);
        match (role.domain.as_str(), role.role.as_str()) {
            ("gmod.net_message", "define") => self
                .system_roles
                .push((GmodSystemCallKind::AddNetworkString, arg_role)),
            ("gmod.net_message", "start") => {
                self.system_roles
                    .push((GmodSystemCallKind::NetStart, arg_role));
            }
            ("gmod.net_message", "receive") => {
                self.system_roles
                    .push((GmodSystemCallKind::NetReceive, arg_role));
            }
            ("gmod.net_message", "callback") => self
                .system_callback_roles
                .push((GmodSystemCallKind::NetReceive, arg_role)),
            ("gmod.concommand", "define") => self
                .system_roles
                .push((GmodSystemCallKind::ConcommandAdd, arg_role)),
            ("gmod.concommand", "callback") => self
                .system_callback_roles
                .push((GmodSystemCallKind::ConcommandAdd, arg_role)),
            ("gmod.convar", "define") | ("gmod.convar", "define_server") => self
                .system_roles
                .push((GmodSystemCallKind::CreateConVar, arg_role)),
            ("gmod.convar", "define_client") => self
                .system_roles
                .push((GmodSystemCallKind::CreateClientConVar, arg_role)),
            ("gmod.timer", "define") => self
                .system_roles
                .push((GmodSystemCallKind::TimerCreate, arg_role)),
            ("gmod.timer", "callback") => self
                .system_callback_roles
                .push((GmodSystemCallKind::TimerCreate, arg_role)),
            ("gmod.timer", "simple") => self
                .system_callback_roles
                .push((GmodSystemCallKind::TimerSimple, arg_role)),
            ("gmod.hook", "add") => self.hook_roles.push((GmodHookKind::Add, arg_role)),
            ("gmod.hook", "emit") => self.hook_roles.push((GmodHookKind::Emit, arg_role)),
            ("gmod.hook", "callback") => {
                self.hook_callback_roles.push(arg_role);
            }
            ("gmod.load", "require") => {
                self.load_roles.push((LuaDependencyKind::Require, arg_role))
            }
            ("gmod.load", "include") => {
                self.load_roles.push((LuaDependencyKind::Include, arg_role))
            }
            ("gmod.load", "addcsluafile") | ("gmod.load", "add_cs_lua_file") => self
                .load_roles
                .push((LuaDependencyKind::AddCSLuaFile, arg_role)),
            ("gmod.load", "includecs") | ("gmod.load", "include_cs") => self
                .load_roles
                .push((LuaDependencyKind::IncludeCS, arg_role)),
            ("gmod.load", "compilefile") => self.compilefile_roles.push(arg_role),
            ("gmod.environment", "target") => self.environment_target_roles.push(arg_role),
            ("gmod.environment", "environment") => self.environment_table_roles.push(arg_role),
            ("gmod.file_find", "glob") => {
                self.file_find_glob_roles.push(arg_role);
            }
            ("gmod.file_find", "search_path") | ("gmod.file_find", "path") => {
                self.file_find_search_path_roles.push(arg_role);
            }
            ("gmod.class_base", "reference") => self
                .inheritance_roles
                .push((GmodScriptedClassCallKind::DefineBaseClass, arg_role)),
            ("gmod.gamemode", "reference") => self
                .inheritance_roles
                .push((GmodScriptedClassCallKind::DeriveGamemode, arg_role)),
            ("gmod.network_var", "type") => {
                self.network_var_type_roles.push(arg_role);
            }
            ("gmod.network_var", "define") => {
                self.network_var_kind = self
                    .network_var_kind
                    .or(Some(GmodScriptedClassCallKind::NetworkVar));
                self.network_var_define_roles.push(arg_role);
            }
            ("gmod.network_var", "define_element") => {
                self.network_var_kind = Some(GmodScriptedClassCallKind::NetworkVarElement);
                self.network_var_define_roles.push(arg_role);
            }
            ("gmod.network_var", "define_slot") => {
                self.network_var_slot_only = true;
                self.network_var_define_roles.push(arg_role);
            }
            ("gmod.vgui_panel", "define") => {
                self.vgui_panel_kind = self
                    .vgui_panel_kind
                    .or(Some(GmodScriptedClassCallKind::VguiRegister));
                self.vgui_panel_define_roles.push(arg_role);
            }
            ("gmod.vgui_panel", "define_control") => {
                self.vgui_panel_kind = Some(GmodScriptedClassCallKind::DermaDefineControl);
                self.vgui_panel_define_roles.push(arg_role);
            }
            ("gmod.vgui_panel", "register_file") => {
                self.vgui_panel_kind = Some(GmodScriptedClassCallKind::VguiRegisterFile);
                self.vgui_panel_define_roles.push(arg_role);
            }
            ("gmod.vgui_panel", "register_table") => {
                self.vgui_panel_kind = Some(GmodScriptedClassCallKind::VguiRegisterTable);
                self.vgui_panel_table_roles.push(arg_role);
            }
            ("gmod.vgui_panel", "table") => {
                self.vgui_panel_table_roles.push(arg_role);
            }
            ("gmod.vgui_panel", "base") => {
                self.vgui_panel_base_roles.push(arg_role);
            }
            ("gmod.vgui_panel", crate::GMOD_ROLE_REFERENCE) => {
                self.vgui_panel_reference_roles.push(arg_role);
            }
            ("gmod.vgui_panel", crate::GMOD_ROLE_VGUI_CHILD_SELF) => {
                self.vgui_panel_child_self_roles.push(arg_role);
            }
            ("gmod.vgui_panel", crate::GMOD_ROLE_VGUI_PARENT) => {
                self.vgui_panel_parent_roles.push(arg_role);
            }
            ("gmod.vgui_panel", crate::GMOD_ROLE_VGUI_PARENT_SELF) => {
                self.vgui_panel_parent_self_roles.push(arg_role);
            }
            ("gmod.derma_skin", "define") => {
                self.derma_skin_define_roles.push(arg_role);
            }
            ("gmod.scripted_class", "define") => {
                self.scripted_class_define_roles.push(arg_role);
            }
            ("gmod.scripted_class", "entity_table") => {
                self.scripted_class_kind = Some(GmodScriptedClassCallKind::ScriptedEntRegister);
                self.scripted_class_table_roles.push(arg_role);
            }
            ("gmod.scripted_class", "weapon_table") => {
                self.scripted_class_kind = Some(GmodScriptedClassCallKind::ScriptedWeaponRegister);
                self.scripted_class_table_roles.push(arg_role);
            }
            _ => {}
        }
    }

    fn sort_roles(&mut self) {
        self.system_roles.sort_by_key(|(_, role)| role.sort_key());
        self.system_callback_roles
            .sort_by_key(|(_, role)| role.sort_key());
        self.hook_roles.sort_by_key(|(_, role)| role.sort_key());
        self.hook_callback_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.load_roles.sort_by_key(|(_, role)| role.sort_key());
        self.compilefile_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.environment_target_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.environment_table_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.file_find_glob_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.file_find_search_path_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.inheritance_roles
            .sort_by_key(|(_, role)| role.sort_key());
        self.network_var_type_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.network_var_define_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.vgui_panel_define_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.vgui_panel_table_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.vgui_panel_base_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.vgui_panel_reference_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.vgui_panel_child_self_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.vgui_panel_parent_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.vgui_panel_parent_self_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.derma_skin_define_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.scripted_class_define_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
        self.scripted_class_table_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
    }

    fn has_any_roles(&self) -> bool {
        !self.system_roles.is_empty()
            || !self.system_callback_roles.is_empty()
            || !self.hook_roles.is_empty()
            || !self.hook_callback_roles.is_empty()
            || !self.load_roles.is_empty()
            || !self.compilefile_roles.is_empty()
            || (!self.environment_target_roles.is_empty()
                && !self.environment_table_roles.is_empty())
            || !self.file_find_glob_roles.is_empty()
            || !self.file_find_search_path_roles.is_empty()
            || !self.inheritance_roles.is_empty()
            || !self.network_var_define_roles.is_empty()
            || !self.vgui_panel_define_roles.is_empty()
            || (!self.vgui_panel_reference_roles.is_empty()
                && (!self.vgui_panel_parent_roles.is_empty()
                    || !self.vgui_panel_parent_self_roles.is_empty()))
            || (!self.vgui_panel_child_self_roles.is_empty()
                && (!self.vgui_panel_parent_roles.is_empty()
                    || !self.vgui_panel_parent_self_roles.is_empty()))
            || matches!(
                self.vgui_panel_kind,
                Some(
                    GmodScriptedClassCallKind::VguiRegisterFile
                        | GmodScriptedClassCallKind::VguiRegisterTable
                )
            )
            || !self.derma_skin_define_roles.is_empty()
            || !self.scripted_class_table_roles.is_empty()
    }

    fn select_for_call(&self, call_expr: &LuaCallExpr) -> Option<AnnotatedGmodCallRoles> {
        let mut best_roles = None;
        let mut best_score = i32::MIN;

        for roles in std::iter::once(self).chain(self.overloads.iter()) {
            let Some(score) = roles.match_score(call_expr) else {
                continue;
            };
            if score > best_score {
                best_score = score;
                best_roles = Some(roles.clone_without_overloads());
            }
        }

        best_roles.or_else(|| Some(self.clone_without_overloads()))
    }

    fn clone_without_overloads(&self) -> AnnotatedGmodCallRoles {
        let mut roles = self.clone();
        roles.overloads.clear();
        roles
    }

    fn match_score(&self, call_expr: &LuaCallExpr) -> Option<i32> {
        if self.params.is_empty() && self.optional_params.is_empty() && !self.is_variadic {
            return Some(0);
        }

        let args = call_expr.get_args_list()?.get_args().collect::<Vec<_>>();
        let effective_arg_count =
            args.len() + usize::from(call_expr.is_colon_call() && !self.is_colon_define);
        let required_count = self
            .params
            .iter()
            .enumerate()
            .filter(|(idx, _)| !self.optional_params.get(*idx).copied().unwrap_or(false))
            .count();

        if effective_arg_count < required_count {
            return None;
        }
        if !self.is_variadic && effective_arg_count > self.params.len() {
            return None;
        }

        let first_param_offset = usize::from(call_expr.is_colon_call() && !self.is_colon_define);
        let mut score = 0;
        for (arg_idx, arg) in args.iter().enumerate() {
            let param_idx = arg_idx + first_param_offset;
            let Some(Some(param_type)) = self.params.get(param_idx) else {
                continue;
            };
            match static_arg_matches_type(arg, param_type) {
                StaticArgTypeMatch::Match => score += 2,
                StaticArgTypeMatch::Unknown => {}
                StaticArgTypeMatch::Mismatch => return None,
            }
        }

        Some(score)
    }

    fn system_call_site(&self) -> Option<GmodSystemCallSite> {
        if let Some((kind, role)) = self.system_roles.first() {
            return Some(GmodSystemCallSite {
                kind: *kind,
                name_arg_idx: Some(role.param_idx),
                callback_arg_idx: self.callback_arg_idx_for_kind(*kind),
            });
        }

        let (kind, callback_role) = self
            .system_callback_roles
            .iter()
            .find(|(kind, _)| *kind == GmodSystemCallKind::TimerSimple)?;

        Some(GmodSystemCallSite {
            kind: *kind,
            name_arg_idx: None,
            callback_arg_idx: Some(callback_role.param_idx),
        })
    }

    fn callback_arg_idx_for_kind(&self, call_kind: GmodSystemCallKind) -> Option<usize> {
        self.system_callback_roles
            .iter()
            .find(|(kind, _)| *kind == call_kind)
            .map(|(_, role)| role.param_idx)
    }

    fn candidate_presence(&self) -> AnnotatedGmodCandidatePresence {
        let mut presence = self.direct_candidate_presence();
        for overload in &self.overloads {
            presence.merge(overload.direct_candidate_presence());
        }
        presence
    }

    fn direct_candidate_presence(&self) -> AnnotatedGmodCandidatePresence {
        AnnotatedGmodCandidatePresence {
            has_system: !self.system_roles.is_empty() || !self.system_callback_roles.is_empty(),
            has_hook: !self.hook_roles.is_empty() || !self.hook_callback_roles.is_empty(),
            has_load: !self.load_roles.is_empty() || !self.compilefile_roles.is_empty(),
            has_environment: !self.compilefile_roles.is_empty()
                || (!self.environment_target_roles.is_empty()
                    && !self.environment_table_roles.is_empty()),
            has_file_find: !self.file_find_glob_roles.is_empty()
                || !self.file_find_search_path_roles.is_empty(),
            has_scripted_class: !self.inheritance_roles.is_empty()
                || !self.network_var_define_roles.is_empty()
                || !self.vgui_panel_define_roles.is_empty()
                || (!self.vgui_panel_reference_roles.is_empty()
                    && (!self.vgui_panel_parent_roles.is_empty()
                        || !self.vgui_panel_parent_self_roles.is_empty()))
                || (!self.vgui_panel_child_self_roles.is_empty()
                    && (!self.vgui_panel_parent_roles.is_empty()
                        || !self.vgui_panel_parent_self_roles.is_empty()))
                || matches!(
                    self.vgui_panel_kind,
                    Some(
                        GmodScriptedClassCallKind::VguiRegisterFile
                            | GmodScriptedClassCallKind::VguiRegisterTable
                    )
                )
                || !self.derma_skin_define_roles.is_empty()
                || !self.scripted_class_table_roles.is_empty(),
        }
    }

    fn inheritance_call(
        &self,
        is_colon_call: bool,
    ) -> Option<(GmodScriptedClassCallKind, GmodNamedStringCallRoles)> {
        let (kind, role) = self.inheritance_roles.first()?;
        Some((
            *kind,
            GmodNamedStringCallRoles {
                name_arg_idx: param_idx_to_call_arg_idx(
                    role.param_idx,
                    is_colon_call,
                    self.is_colon_define,
                )?,
            },
        ))
    }

    fn load_call(&self, is_colon_call: bool) -> Option<(LuaDependencyKind, usize)> {
        self.load_roles
            .first()
            .and_then(|(kind, role)| {
                Some((
                    *kind,
                    param_idx_to_call_arg_idx(role.param_idx, is_colon_call, self.is_colon_define)?,
                ))
            })
            .or_else(|| {
                self.compilefile_call(is_colon_call)
                    .map(|path_arg_idx| (LuaDependencyKind::CompileFile, path_arg_idx))
            })
    }

    fn compilefile_call(&self, is_colon_call: bool) -> Option<usize> {
        let role = self.compilefile_roles.first()?;
        param_idx_to_call_arg_idx(role.param_idx, is_colon_call, self.is_colon_define)
    }

    fn environment_call(&self, is_colon_call: bool) -> Option<(usize, usize)> {
        let target = self.environment_target_roles.first()?;
        let environment = self.environment_table_roles.first()?;
        Some((
            param_idx_to_call_arg_idx(target.param_idx, is_colon_call, self.is_colon_define)?,
            param_idx_to_call_arg_idx(environment.param_idx, is_colon_call, self.is_colon_define)?,
        ))
    }

    fn load_alias(&self, is_colon_call: bool) -> Option<DynamicLoadAlias> {
        self.load_call(is_colon_call)
            .and_then(|(kind, path_arg_idx)| {
                DynamicLoadAlias::from_dependency_kind(kind, path_arg_idx)
            })
            .or_else(|| {
                self.overloads
                    .iter()
                    .find_map(|overload| overload.load_alias(is_colon_call))
            })
    }

    fn file_find_call(&self, is_colon_call: bool) -> Option<(usize, usize)> {
        let glob_role = self.file_find_glob_roles.first()?;
        let search_path_role = self.file_find_search_path_roles.first()?;
        Some((
            param_idx_to_call_arg_idx(glob_role.param_idx, is_colon_call, self.is_colon_define)?,
            param_idx_to_call_arg_idx(
                search_path_role.param_idx,
                is_colon_call,
                self.is_colon_define,
            )?,
        ))
    }

    fn network_var_call(
        &self,
        is_colon_call: bool,
    ) -> Option<(GmodScriptedClassCallKind, GmodNetworkVarCallRoles)> {
        let define_role = self.network_var_define_roles.first()?;
        let kind = self
            .network_var_kind
            .unwrap_or(GmodScriptedClassCallKind::NetworkVar);
        Some((
            kind,
            GmodNetworkVarCallRoles {
                slot_only: self.network_var_slot_only,
                type_arg_idx: self.network_var_type_roles.first().and_then(|role| {
                    param_idx_to_call_arg_idx(role.param_idx, is_colon_call, self.is_colon_define)
                }),
                name_arg_idx: param_idx_to_call_arg_idx(
                    define_role.param_idx,
                    is_colon_call,
                    self.is_colon_define,
                )?,
            },
        ))
    }

    /// The roles of a function that registers a panel by passing its
    /// parameters to a call with these roles.
    fn forwarded_vgui_panel_registration(&self, forward: &ForwardedCall) -> Option<Self> {
        let kind = self.vgui_panel_kind?;
        if kind == GmodScriptedClassCallKind::VguiRegisterFile
            || (self.vgui_panel_define_roles.is_empty()
                && kind != GmodScriptedClassCallKind::VguiRegisterTable)
        {
            return None;
        }
        let forwarded = |role: &AnnotatedGmodCallArgRole| {
            let arg_idx = param_idx_to_call_arg_idx(
                role.param_idx,
                forward.callee_is_colon,
                self.is_colon_define,
            )?;
            forward.args.get(arg_idx)?.forward_role(role)
        };
        let table = forwarded(self.vgui_panel_table_roles.first()?).filter(|role| {
            role.field_path.is_empty() && role.prefix.is_empty() && role.suffix.is_empty()
        })?;
        let define = match self.vgui_panel_define_roles.first() {
            Some(role) => vec![forwarded(role)?],
            None => Vec::new(),
        };
        let base = self
            .vgui_panel_base_roles
            .first()
            .and_then(forwarded)
            .into_iter()
            .collect();
        Some(Self {
            vgui_panel_kind: self.vgui_panel_kind,
            vgui_panel_define_roles: define,
            vgui_panel_table_roles: vec![table],
            vgui_panel_base_roles: base,
            ..Self::default()
        })
    }

    fn vgui_panel_call(
        &self,
        is_colon_call: bool,
    ) -> Option<(GmodScriptedClassCallKind, GmodVguiPanelCallRoles)> {
        let kind = self
            .vgui_panel_kind
            .unwrap_or(GmodScriptedClassCallKind::VguiRegister);
        let define = if let Some(role) = self.vgui_panel_define_roles.first() {
            role.to_arg_source(is_colon_call, self.is_colon_define)?
        } else if kind == GmodScriptedClassCallKind::VguiRegisterTable {
            self.vgui_panel_table_roles
                .first()?
                .to_arg_source(is_colon_call, self.is_colon_define)?
        } else {
            return None;
        };

        Some((
            kind,
            GmodVguiPanelCallRoles {
                define,
                table: self
                    .vgui_panel_table_roles
                    .first()
                    .and_then(|role| role.to_arg_source(is_colon_call, self.is_colon_define)),
                base: self
                    .vgui_panel_base_roles
                    .first()
                    .and_then(|role| role.to_arg_source(is_colon_call, self.is_colon_define)),
            },
        ))
    }

    fn vgui_parent_call(&self, is_colon_call: bool) -> Option<AnnotatedVguiParentCallRoles> {
        let child = if let Some(role) = self.vgui_panel_reference_roles.first() {
            AnnotatedVguiParentSource::Arg(role.to_arg_source(is_colon_call, self.is_colon_define)?)
        } else {
            let role = self.vgui_panel_child_self_roles.first()?;
            AnnotatedVguiParentSource::Receiver {
                field_path: role.field_path.clone(),
                dot_source: role.to_arg_source(false, self.is_colon_define)?,
            }
        };
        let parent = if let Some(role) = self.vgui_panel_parent_roles.first() {
            AnnotatedVguiParentSource::Arg(role.to_arg_source(is_colon_call, self.is_colon_define)?)
        } else {
            let role = self.vgui_panel_parent_self_roles.first()?;
            AnnotatedVguiParentSource::Receiver {
                field_path: role.field_path.clone(),
                dot_source: role.to_arg_source(false, self.is_colon_define)?,
            }
        };
        Some(AnnotatedVguiParentCallRoles { child, parent })
    }

    fn scripted_class_register_call(
        &self,
        is_colon_call: bool,
    ) -> Option<(GmodScriptedClassCallKind, GmodScriptedClassRegisterRoles)> {
        let kind = self.scripted_class_kind?;
        let table_role = self.scripted_class_table_roles.first()?;
        let define_role = self.scripted_class_define_roles.first()?;
        Some((
            kind,
            GmodScriptedClassRegisterRoles {
                table_arg_idx: param_idx_to_call_arg_idx(
                    table_role.param_idx,
                    is_colon_call,
                    self.is_colon_define,
                )?,
                name_arg_idx: param_idx_to_call_arg_idx(
                    define_role.param_idx,
                    is_colon_call,
                    self.is_colon_define,
                )?,
            },
        ))
    }

    fn derma_skin_call_roles(&self, is_colon_call: bool) -> Option<GmodDermaSkinCallRoles> {
        let define_role = self.derma_skin_define_roles.first()?;
        Some(GmodDermaSkinCallRoles {
            define_arg_idx: param_idx_to_call_arg_idx(
                define_role.param_idx,
                is_colon_call,
                self.is_colon_define,
            )?,
        })
    }
}

fn param_idx_to_call_arg_idx(
    param_idx: usize,
    is_colon_call: bool,
    is_colon_define: bool,
) -> Option<usize> {
    if is_colon_call && !is_colon_define {
        param_idx.checked_sub(1)
    } else {
        Some(param_idx)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaticArgTypeMatch {
    Match,
    Mismatch,
    Unknown,
}

fn static_arg_matches_type(arg: &LuaExpr, param_type: &LuaType) -> StaticArgTypeMatch {
    let Some(kind) = static_arg_kind(arg) else {
        return StaticArgTypeMatch::Unknown;
    };

    static_arg_kind_matches_type(kind, param_type)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StaticArgKind {
    String,
    Number,
    Boolean,
    Table,
    Function,
    Nil,
}

fn static_arg_kind(arg: &LuaExpr) -> Option<StaticArgKind> {
    match arg {
        LuaExpr::LiteralExpr(literal) => match literal.get_literal()? {
            LuaLiteralToken::String(_) => Some(StaticArgKind::String),
            LuaLiteralToken::Number(_) => Some(StaticArgKind::Number),
            LuaLiteralToken::Bool(_) => Some(StaticArgKind::Boolean),
            LuaLiteralToken::Nil(_) => Some(StaticArgKind::Nil),
            LuaLiteralToken::Dots(_) | LuaLiteralToken::Question(_) => None,
        },
        LuaExpr::TableExpr(_) => Some(StaticArgKind::Table),
        LuaExpr::ClosureExpr(_) => Some(StaticArgKind::Function),
        _ => None,
    }
}

fn static_arg_kind_matches_type(kind: StaticArgKind, param_type: &LuaType) -> StaticArgTypeMatch {
    match param_type {
        LuaType::Any | LuaType::Unknown => StaticArgTypeMatch::Unknown,
        LuaType::String | LuaType::StringConst(_) | LuaType::DocStringConst(_) => {
            match_bool(kind == StaticArgKind::String)
        }
        LuaType::Number
        | LuaType::Integer
        | LuaType::IntegerConst(_)
        | LuaType::DocIntegerConst(_) => match_bool(kind == StaticArgKind::Number),
        LuaType::Boolean | LuaType::BooleanConst(_) | LuaType::DocBooleanConst(_) => {
            match_bool(kind == StaticArgKind::Boolean)
        }
        LuaType::Table | LuaType::Object(_) | LuaType::TableConst(_) => {
            match_bool(kind == StaticArgKind::Table)
        }
        LuaType::DocFunction(_) | LuaType::Signature(_) | LuaType::Function => {
            match_bool(kind == StaticArgKind::Function)
        }
        LuaType::Nil => match_bool(kind == StaticArgKind::Nil),
        LuaType::Union(union) => {
            let mut saw_unknown = false;
            for typ in union.types() {
                match static_arg_kind_matches_type(kind, typ) {
                    StaticArgTypeMatch::Match => return StaticArgTypeMatch::Match,
                    StaticArgTypeMatch::Unknown => saw_unknown = true,
                    StaticArgTypeMatch::Mismatch => {}
                }
            }
            if saw_unknown {
                StaticArgTypeMatch::Unknown
            } else {
                StaticArgTypeMatch::Mismatch
            }
        }
        LuaType::MultiLineUnion(union) => {
            let mut saw_unknown = false;
            for (typ, _) in union.get_unions() {
                match static_arg_kind_matches_type(kind, typ) {
                    StaticArgTypeMatch::Match => return StaticArgTypeMatch::Match,
                    StaticArgTypeMatch::Unknown => saw_unknown = true,
                    StaticArgTypeMatch::Mismatch => {}
                }
            }
            if saw_unknown {
                StaticArgTypeMatch::Unknown
            } else {
                StaticArgTypeMatch::Mismatch
            }
        }
        LuaType::TypeGuard(inner) => static_arg_kind_matches_type(kind, inner),
        LuaType::TableOf(inner) => static_arg_kind_matches_type(kind, inner),
        LuaType::Instance(instance) => static_arg_kind_matches_type(kind, instance.get_base()),
        LuaType::Variadic(variadic) => match variadic.as_ref() {
            crate::db_index::VariadicType::Base(inner) => static_arg_kind_matches_type(kind, inner),
            crate::db_index::VariadicType::Multi(types) => {
                if types
                    .iter()
                    .any(|typ| static_arg_kind_matches_type(kind, typ) == StaticArgTypeMatch::Match)
                {
                    StaticArgTypeMatch::Match
                } else {
                    StaticArgTypeMatch::Unknown
                }
            }
        },
        _ => StaticArgTypeMatch::Unknown,
    }
}

fn match_bool(matches: bool) -> StaticArgTypeMatch {
    if matches {
        StaticArgTypeMatch::Match
    } else {
        StaticArgTypeMatch::Mismatch
    }
}

/// Total precedence of canonical net metadata, lowest wins. Workspace class is
/// the meaningful preference; path and position make equal-name duplicates
/// deterministic even when their metadata conflicts.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct CanonicalNetOpRank {
    workspace: u8,
    normalized_path: String,
    raw_path: String,
    position: TextSize,
}

fn canonical_net_op_rank(db: &DbIndex, signature_id: LuaSignatureId) -> CanonicalNetOpRank {
    let file_id = signature_id.get_file_id();
    let workspace_id = db
        .get_module_index()
        .get_workspace_id(file_id)
        .unwrap_or(crate::WorkspaceId::MAIN);
    let workspace = match workspace_id {
        crate::WorkspaceId::STD => 0,
        id if id.is_library() => 1,
        id if id.is_remote() => 2,
        _ => 3,
    };
    let raw_path = db
        .get_vfs()
        .get_file_path(&file_id)
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_default();
    CanonicalNetOpRank {
        workspace,
        normalized_path: crate::vfs::normalize_path_for_ordering(&raw_path),
        raw_path,
        position: signature_id.get_position(),
    }
}

impl AnnotatedGmodGlobalCallRoleMap {
    /// A value fingerprint of the role map: its entries in a stable order.
    fn fingerprint(&self) -> crate::db_index::derivation::Fingerprint {
        let mut roles = self.roles_by_path.iter().collect::<Vec<_>>();
        roles.sort_by(|a, b| a.0.cmp(b.0));
        // Readers see the selected op. Its rank only orders duplicates within
        // one file, which an edit that shifts them cannot reorder.
        let mut net_ops = self
            .canonical_net_ops
            .iter()
            .map(|((name, direction), (_, op))| (name, format!("{direction:?}"), op))
            .collect::<Vec<_>>();
        net_ops.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        crate::db_index::derivation::fingerprint_of(&(roles, net_ops))
    }

    /// One file's contribution to the role map, plus the helper definitions its
    /// signatures produced. Cached on the db and re-derived only when the file
    /// is re-analysed — see `DbIndex::get_cached_file_helper_scan`.
    fn build_for_file(
        db: &DbIndex,
        signature_ids: &[LuaSignatureId],
        file_has_calls: bool,
    ) -> FileHelperScan {
        let mut builder = HelperRegistryBuilder {
            file_has_calls,
            ..Default::default()
        };
        let mut role_map = Self::default();
        for signature_id in signature_ids {
            builder.add_signature(db, *signature_id);
            role_map.add_signature_net_op(db, *signature_id);

            let Some(signature) = db.get_signature_index().get_declared(signature_id) else {
                continue;
            };
            if !signature.has_call_arg_roles() {
                continue;
            }
            let Some(closure) = closure_from_signature_id(db, *signature_id) else {
                continue;
            };
            role_map.add_signature_closure(db, *signature_id, &closure);
        }

        FileHelperScan {
            role_map,
            definitions: builder.definitions,
            forwarded_calls: builder.forwarded_calls,
        }
    }

    /// Records the canonical function name for an annotated payload op.
    fn add_signature_net_op(&mut self, db: &DbIndex, signature_id: LuaSignatureId) {
        let descriptor = crate::db_index::signature_net_payload(db, signature_id);
        let is_send =
            descriptor.is_none() && crate::db_index::signature_net_send(db, signature_id).is_some();
        if descriptor.is_none() && !is_send {
            return;
        }

        let Some(closure) = closure_from_signature_id(db, signature_id) else {
            return;
        };
        let Some(call_path) = global_call_path_for_signature_closure(db, signature_id, &closure)
        else {
            return;
        };
        let display_path = call_path
            .strip_prefix("_G.")
            .unwrap_or(&call_path)
            .to_string();

        if let Some(descriptor) = descriptor {
            // Wrappers are expected to share a wire format with the builtin they
            // wrap, so collisions here are normal rather than exceptional. The
            // signature index is a `HashMap`, so its iteration order varies per
            // process; ranking the candidates keeps the published name stable.
            let candidate = (
                canonical_net_op_rank(db, signature_id),
                crate::db_index::CanonicalNetOp {
                    name: display_path,
                    has_bits_param: crate::db_index::signature_net_bits_param_idx(db, signature_id)
                        .is_some(),
                },
            );
            self.canonical_net_ops
                .entry((descriptor.wire_format, descriptor.direction))
                .and_modify(|current| {
                    if candidate.0 < current.0 {
                        *current = candidate.clone();
                    }
                })
                .or_insert(candidate);
        }
    }

    /// Canonical op metadata with its ranking dropped, ready to publish.
    fn canonical_net_ops(
        &self,
    ) -> HashMap<(SmolStr, NetOpDirection), crate::db_index::CanonicalNetOp> {
        self.canonical_net_ops
            .iter()
            .map(|(key, (_, op))| (key.clone(), op.clone()))
            .collect()
    }

    /// Whether the global paths `file_id`'s syntax names include a call role
    /// of each kind. Every path is looked up, present or absent.
    fn candidate_presence(&self, db: &DbIndex, file_id: FileId) -> AnnotatedGmodCandidatePresence {
        let mut presence = AnnotatedGmodCandidatePresence::default();
        for path in db.role_mentions(file_id).iter() {
            if let Some(name) = path.strip_prefix("*.") {
                presence.merge(self.receiver_presence(name));
                continue;
            }
            if let Some(roles) = self.roles(path) {
                presence.merge(roles.candidate_presence());
            }
        }
        presence
    }

    fn receiver_presence(&self, name: &str) -> AnnotatedGmodCandidatePresence {
        crate::db_index::derivation::note_read(|| {
            crate::db_index::derivation::FactKey::CallRole(
                crate::db_index::derivation::CallRoleKey::ReceiverMethod(SmolStr::new(name)),
            )
        });
        self.receiver_presence_value(name)
    }

    fn receiver_presence_value(&self, name: &str) -> AnnotatedGmodCandidatePresence {
        self.receiver_paths.get(name).into_iter().flatten().fold(
            AnnotatedGmodCandidatePresence::default(),
            |mut presence, path| {
                if let Some(roles) = self.roles_by_path.get(path) {
                    presence.merge(roles.candidate_presence());
                }
                presence
            },
        )
    }

    fn add_signature_closure(
        &mut self,
        db: &DbIndex,
        signature_id: LuaSignatureId,
        closure: &LuaClosureExpr,
    ) {
        let Some(call_path) = global_call_path_for_signature_closure(db, signature_id, closure)
            .or_else(|| class_method_path_for_signature_closure(db, signature_id, closure))
        else {
            return;
        };
        if let Some(roles) = roles_from_signature(db, signature_id) {
            let fingerprint = crate::db_index::derivation::fingerprint_of(&roles);
            self.roles_by_path.insert(call_path.clone(), roles.clone());
            self.path_fingerprints
                .insert(call_path.clone(), fingerprint);
            if let Some(global_path) = call_path.strip_prefix("_G.") {
                self.roles_by_path.insert(global_path.to_string(), roles);
                self.path_fingerprints
                    .insert(global_path.to_string(), fingerprint);
            }
        }
    }

    /// The roles of `call_path`, or of the path under `_G.`. Reads each path it
    /// consults as its entry, present or absent.
    fn roles(&self, call_path: &str) -> Option<&AnnotatedGmodCallRoles> {
        let entry = |path: &str| {
            crate::db_index::derivation::note_read(|| {
                crate::db_index::derivation::FactKey::CallRole(
                    crate::db_index::derivation::CallRoleKey::Path(SmolStr::new(path)),
                )
            });
            self.roles_by_path.get(path)
        };
        entry(call_path).or_else(|| entry(call_path.strip_prefix("_G.")?))
    }

    fn get(&self, call_path: &str) -> Option<AnnotatedGmodCallRoles> {
        self.roles(call_path).cloned()
    }

    fn contains(&self, call_path: &str) -> bool {
        self.roles(call_path).is_some()
    }
}

impl<'a> AnnotatedGmodCallRoleMap<'a> {
    fn build(
        db: &DbIndex,
        file_id: FileId,
        root: &LuaChunk,
        global_roles: &'a AnnotatedGmodGlobalCallRoleMap,
    ) -> Self {
        let mut role_map = Self {
            global_roles,
            local_roles_by_decl: HashMap::default(),
            local_roles_by_path: HashMap::default(),
            local_candidate_names: HashSet::default(),
            file_id,
        };

        for func_stat in root.descendants::<LuaFuncStat>() {
            let Some(func_name) = func_stat.get_func_name() else {
                continue;
            };
            let Some(root_name_expr) = var_expr_root_name(&func_name) else {
                continue;
            };
            let Some(root_name) = root_name_expr.get_name_text() else {
                continue;
            };
            let Some(root_decl) =
                db.get_decl_index()
                    .get_decl_tree(&file_id)
                    .and_then(|decl_tree| {
                        decl_tree.find_local_decl(&root_name, root_name_expr.get_position())
                    })
            else {
                continue;
            };
            if !root_decl.is_local() {
                continue;
            }
            let root_decl_id = root_decl.get_id();
            let Some(closure) = func_stat.get_closure() else {
                continue;
            };
            let signature_id = LuaSignatureId::from_closure(file_id, &closure);
            let Some(roles) = roles_from_signature(db, signature_id) else {
                continue;
            };
            let Some(call_path) = func_name.get_access_path() else {
                continue;
            };
            role_map.add_local_path_roles(root_decl_id, call_path.to_string(), roles);
        }

        for local_func_stat in root.descendants::<LuaLocalFuncStat>() {
            let Some(name_token) = local_func_stat
                .get_local_name()
                .and_then(|local_name| local_name.get_name_token())
            else {
                continue;
            };
            let Some(closure) = local_func_stat.get_closure() else {
                continue;
            };
            let signature_id = LuaSignatureId::from_closure(file_id, &closure);
            let Some(roles) = roles_from_signature(db, signature_id) else {
                continue;
            };
            role_map.add_local_decl_roles(
                LuaDeclId::new(file_id, name_token.get_range().start()),
                name_token.get_name_text().to_string(),
                roles,
            );
        }

        role_map
    }

    fn add_local_decl_roles(
        &mut self,
        decl_id: LuaDeclId,
        name: String,
        roles: AnnotatedGmodCallRoles,
    ) {
        self.local_roles_by_decl.insert(decl_id, roles);
        self.local_candidate_names.insert(name);
    }

    fn add_local_path_roles(
        &mut self,
        root_decl_id: LuaDeclId,
        call_path: String,
        roles: AnnotatedGmodCallRoles,
    ) {
        self.local_candidate_names.insert(call_path.clone());
        self.local_roles_by_path
            .insert((root_decl_id, call_path), roles);
    }

    fn system_call(
        &self,
        db: &DbIndex,
        file_id: FileId,
        call_expr: &LuaCallExpr,
        call_path: &str,
    ) -> Option<GmodSystemCallSite> {
        self.roles_for_call(db, file_id, call_expr, call_path)
            .and_then(|roles| roles.system_call_site())
    }

    fn hook_call(
        &self,
        db: &DbIndex,
        file_id: FileId,
        call_expr: &LuaCallExpr,
        call_path: &str,
    ) -> Option<(GmodHookKind, usize, Option<usize>)> {
        self.roles_for_call(db, file_id, call_expr, call_path)
            .and_then(|roles| {
                let (kind, role) = roles.hook_roles.first()?;
                Some((
                    *kind,
                    role.param_idx,
                    roles.hook_callback_roles.first().and_then(|callback_role| {
                        param_idx_to_call_arg_idx(
                            callback_role.param_idx,
                            call_expr.is_colon_call(),
                            roles.is_colon_define,
                        )
                    }),
                ))
            })
    }

    fn load_call(
        &self,
        db: &DbIndex,
        file_id: FileId,
        call_expr: &LuaCallExpr,
        call_path: &str,
    ) -> Option<(LuaDependencyKind, usize)> {
        self.roles_for_call(db, file_id, call_expr, call_path)
            .and_then(|roles| roles.load_call(call_expr.is_colon_call()))
    }

    fn compilefile_call(
        &self,
        db: &DbIndex,
        file_id: FileId,
        call_expr: &LuaCallExpr,
        call_path: &str,
    ) -> Option<usize> {
        self.roles_for_call(db, file_id, call_expr, call_path)
            .and_then(|roles| roles.compilefile_call(call_expr.is_colon_call()))
    }

    fn environment_call(
        &self,
        db: &DbIndex,
        file_id: FileId,
        call_expr: &LuaCallExpr,
        call_path: &str,
    ) -> Option<(usize, usize)> {
        self.roles_for_call(db, file_id, call_expr, call_path)
            .and_then(|roles| roles.environment_call(call_expr.is_colon_call()))
    }

    fn load_alias_for_call(
        &self,
        db: &DbIndex,
        file_id: FileId,
        call_expr: &LuaCallExpr,
        call_path: &str,
    ) -> Option<DynamicLoadAlias> {
        self.roles_for_call(db, file_id, call_expr, call_path)
            .and_then(|roles| roles.load_alias(call_expr.is_colon_call()))
    }

    fn load_alias_for_reference_expr(
        &self,
        db: &DbIndex,
        file_id: FileId,
        expr: &LuaExpr,
    ) -> Option<DynamicLoadAlias> {
        match expr {
            LuaExpr::NameExpr(name_expr) => {
                let name = name_expr.get_name_text()?;
                if let Some(local_roles) =
                    annotated_roles_from_local_name_expr(self, db, file_id, name_expr)
                {
                    return local_roles.and_then(|roles| roles.load_alias(false));
                }
                self.global_roles
                    .get(name.as_str())
                    .and_then(|roles| roles.load_alias(false))
            }
            LuaExpr::IndexExpr(index_expr) => {
                let path = index_expr.get_access_path()?;
                let has_shadowing_local_root = index_expr_root_name(index_expr)
                    .as_ref()
                    .is_some_and(|root| name_expr_resolves_to_shadowing_local(db, file_id, root));
                if has_shadowing_local_root {
                    return None;
                }
                self.global_roles
                    .get(&path)
                    .and_then(|roles| roles.load_alias(false))
            }
            LuaExpr::ParenExpr(paren_expr) => {
                self.load_alias_for_reference_expr(db, file_id, &paren_expr.get_expr()?)
            }
            _ => None,
        }
    }

    fn file_find_call(
        &self,
        db: &DbIndex,
        file_id: FileId,
        call_expr: &LuaCallExpr,
        call_path: &str,
    ) -> Option<(usize, usize)> {
        self.roles_for_call(db, file_id, call_expr, call_path)
            .and_then(|roles| roles.file_find_call(call_expr.is_colon_call()))
    }

    fn vgui_panel_call(
        &self,
        db: &DbIndex,
        file_id: FileId,
        call_expr: &LuaCallExpr,
        call_path: &str,
    ) -> Option<(GmodScriptedClassCallKind, GmodVguiPanelCallRoles)> {
        self.roles_for_call(db, file_id, call_expr, call_path)
            .and_then(|roles| roles.vgui_panel_call(call_expr.is_colon_call()))
    }

    fn vgui_parent_call(
        &self,
        db: &DbIndex,
        file_id: FileId,
        call_expr: &LuaCallExpr,
        call_path: &str,
    ) -> Option<AnnotatedVguiParentCallRoles> {
        self.roles_for_call(db, file_id, call_expr, call_path)
            .and_then(|roles| roles.vgui_parent_call(call_expr.is_colon_call()))
    }

    fn inheritance_call(
        &self,
        db: &DbIndex,
        file_id: FileId,
        call_expr: &LuaCallExpr,
        call_path: &str,
    ) -> Option<(GmodScriptedClassCallKind, GmodNamedStringCallRoles)> {
        self.roles_for_call(db, file_id, call_expr, call_path)
            .and_then(|roles| roles.inheritance_call(call_expr.is_colon_call()))
    }

    fn network_var_call(
        &self,
        db: &DbIndex,
        file_id: FileId,
        call_expr: &LuaCallExpr,
        call_path: &str,
    ) -> Option<(GmodScriptedClassCallKind, GmodNetworkVarCallRoles)> {
        self.roles_for_call(db, file_id, call_expr, call_path)
            .and_then(|roles| roles.network_var_call(call_expr.is_colon_call()))
    }

    fn scripted_class_register_call(
        &self,
        db: &DbIndex,
        file_id: FileId,
        call_expr: &LuaCallExpr,
        call_path: &str,
    ) -> Option<(GmodScriptedClassCallKind, GmodScriptedClassRegisterRoles)> {
        self.roles_for_call(db, file_id, call_expr, call_path)
            .and_then(|roles| roles.scripted_class_register_call(call_expr.is_colon_call()))
    }

    fn derma_skin_call(
        &self,
        db: &DbIndex,
        file_id: FileId,
        call_expr: &LuaCallExpr,
        call_path: &str,
    ) -> Option<GmodDermaSkinCallRoles> {
        self.roles_for_call(db, file_id, call_expr, call_path)
            .and_then(|roles| roles.derma_skin_call_roles(call_expr.is_colon_call()))
    }

    fn roles_for_call(
        &self,
        db: &DbIndex,
        file_id: FileId,
        call_expr: &LuaCallExpr,
        call_path: &str,
    ) -> Option<AnnotatedGmodCallRoles> {
        if call_expr_local_root_decl_id(db, file_id, call_expr).is_some_and(|decl_id| {
            db.get_reference_index()
                .get_decl_references(&file_id, &decl_id)
                .is_some_and(|references| references.mutable)
        }) {
            return self.roles_from_receiver_target(db, call_expr, call_path);
        }

        if let Some(Some(local_path_roles)) =
            annotated_roles_from_local_call_path(self, db, file_id, call_expr, call_path)
        {
            return local_path_roles.select_for_call(call_expr);
        }

        if self.global_roles.contains(call_path) {
            if let Some(Some(local_roles)) = annotated_roles_from_local_call_prefix(
                self,
                db,
                file_id,
                call_expr.get_prefix_expr(),
            ) {
                return local_roles.select_for_call(call_expr);
            }

            if call_expr_has_shadowing_local_root(db, file_id, call_expr) {
                return self.roles_from_receiver_target(db, call_expr, call_path);
            }

            return self
                .global_roles
                .get(call_path)
                .and_then(|roles| roles.select_for_call(call_expr));
        }

        if self.local_candidate_names.contains(call_path)
            && let Some(Some(local_roles)) = annotated_roles_from_local_call_prefix(
                self,
                db,
                file_id,
                call_expr.get_prefix_expr(),
            )
        {
            return local_roles.select_for_call(call_expr);
        }

        self.roles_from_receiver_target(db, call_expr, call_path)
    }

    /// The roles of the method `call_expr`'s receiver resolves to, from the
    /// call's target. Those resolve after the statements, so a pass that runs
    /// before them finds none and runs again once they land.
    fn roles_from_receiver_target(
        &self,
        db: &DbIndex,
        call_expr: &LuaCallExpr,
        call_path: &str,
    ) -> Option<AnnotatedGmodCallRoles> {
        if !matches!(call_expr.get_prefix_expr(), Some(LuaExpr::IndexExpr(_)))
            || self
                .global_roles
                .receiver_presence(&call_roles::method_name(call_path)?)
                == AnnotatedGmodCandidatePresence::default()
        {
            return None;
        }
        let signature_id = db
            .get_call_target_index()
            .target(self.file_id, call_expr.get_range())?
            .declared?;
        roles_from_signature(db, signature_id)?.select_for_call(call_expr)
    }
}

fn call_expr_local_root_decl_id(
    db: &DbIndex,
    file_id: FileId,
    call_expr: &LuaCallExpr,
) -> Option<LuaDeclId> {
    match call_expr.get_prefix_expr()? {
        LuaExpr::NameExpr(name_expr) => name_expr_local_decl_id(db, file_id, &name_expr),
        LuaExpr::IndexExpr(index_expr) => index_expr_root_name(&index_expr)
            .and_then(|name_expr| name_expr_local_decl_id(db, file_id, &name_expr)),
        _ => None,
    }
}

fn roles_from_signature(
    db: &DbIndex,
    signature_id: LuaSignatureId,
) -> Option<AnnotatedGmodCallRoles> {
    let signature = db.get_signature_index().get_declared(&signature_id)?;
    if !signature.has_call_arg_roles() {
        return None;
    }

    let mut roles = AnnotatedGmodCallRoles::from_signature_shape(signature);
    for role in signature.call_arg_roles() {
        roles.add_call_arg_role(&role);
    }

    for overload in &signature.overloads {
        if overload.get_call_arg_roles().is_empty() {
            continue;
        }
        let mut overload_roles = AnnotatedGmodCallRoles::from_function_shape(overload);
        for role in overload.get_call_arg_roles() {
            overload_roles.add_call_arg_role(role);
        }
        overload_roles.sort_roles();
        if overload_roles.has_any_roles() {
            roles.overloads.push(overload_roles);
        }
    }

    roles.sort_roles();

    (roles.has_any_roles() || !roles.overloads.is_empty()).then_some(roles)
}

fn closure_from_signature_id(db: &DbIndex, signature_id: LuaSignatureId) -> Option<LuaClosureExpr> {
    let root = db
        .get_vfs()
        .syntax_tree_around(
            &signature_id.get_file_id(),
            TextRange::empty(signature_id.get_position()),
        )?
        .get_red_root();
    // A signature's position is the offset its closure starts at, so descend to
    // that offset rather than scanning every node in the file.
    root.token_at_offset(signature_id.get_position())
        .right_biased()?
        .parent_ancestors()
        .filter_map(LuaClosureExpr::cast)
        .find(|closure| closure.get_position() == signature_id.get_position())
}

pub(crate) fn global_call_path_for_signature_closure(
    db: &DbIndex,
    signature_id: LuaSignatureId,
    closure: &LuaClosureExpr,
) -> Option<String> {
    let file_id = signature_id.get_file_id();
    if let Some(func_stat) = closure.get_parent::<LuaFuncStat>() {
        let func_name = func_stat.get_func_name()?;
        return var_expr_has_global_root(db, file_id, &func_name)
            .then(|| func_name.get_access_path().map(Into::into))?;
    }

    let assign_stat = closure.get_parent::<LuaAssignStat>()?;
    let (vars, value_exprs) = assign_stat.get_var_and_expr_list();
    let value_idx = value_exprs
        .iter()
        .position(|expr| expr.get_position() == closure.get_position())?;
    let var_expr = vars.get(value_idx)?;
    var_expr_has_global_root(db, file_id, var_expr)
        .then(|| var_expr.get_access_path().map(Into::into))?
}

fn class_method_path_for_signature_closure(
    db: &DbIndex,
    signature_id: LuaSignatureId,
    closure: &LuaClosureExpr,
) -> Option<String> {
    let func_name = closure.get_parent::<LuaFuncStat>()?.get_func_name()?;
    let LuaVarExpr::IndexExpr(index_expr) = func_name else {
        return None;
    };
    let LuaExpr::NameExpr(root) = index_expr.get_prefix_expr()? else {
        return None;
    };
    let decl_id = name_expr_local_decl_id(db, signature_id.get_file_id(), &root)?;
    let type_cache = db.get_type_index().get_type_cache(&decl_id.into())?;
    let LuaType::Def(class_id) = type_cache.as_type() else {
        return None;
    };
    let method = match index_expr.get_index_key()? {
        LuaIndexKey::Name(name) => name.get_name_text().to_string(),
        LuaIndexKey::String(name) => name.get_value(),
        _ => return None,
    };
    Some(format!("{}.{method}", class_id.get_name()))
}

fn var_expr_has_global_root(db: &DbIndex, file_id: FileId, var_expr: &LuaVarExpr) -> bool {
    match var_expr {
        LuaVarExpr::NameExpr(name_expr) => !name_expr_resolves_to_local(db, file_id, name_expr),
        LuaVarExpr::IndexExpr(index_expr) => index_expr_root_name(index_expr)
            .as_ref()
            .is_none_or(|name_expr| !name_expr_resolves_to_local(db, file_id, name_expr)),
    }
}

fn call_expr_has_shadowing_local_root(
    db: &DbIndex,
    file_id: FileId,
    call_expr: &LuaCallExpr,
) -> bool {
    match call_expr.get_prefix_expr() {
        Some(LuaExpr::NameExpr(name_expr)) => {
            name_expr_resolves_to_shadowing_local(db, file_id, &name_expr)
        }
        Some(LuaExpr::IndexExpr(index_expr)) => index_expr_root_name(&index_expr)
            .as_ref()
            .is_some_and(|name_expr| name_expr_resolves_to_shadowing_local(db, file_id, name_expr)),
        _ => false,
    }
}

fn index_expr_root_name(index_expr: &glua_parser::LuaIndexExpr) -> Option<LuaNameExpr> {
    match index_expr.get_prefix_expr()? {
        LuaExpr::NameExpr(name_expr) => Some(name_expr),
        LuaExpr::IndexExpr(prefix_index_expr) => index_expr_root_name(&prefix_index_expr),
        _ => None,
    }
}

fn var_expr_root_name(var_expr: &LuaVarExpr) -> Option<LuaNameExpr> {
    match var_expr {
        LuaVarExpr::NameExpr(name_expr) => Some(name_expr.clone()),
        LuaVarExpr::IndexExpr(index_expr) => index_expr_root_name(index_expr),
    }
}

fn name_expr_resolves_to_local(db: &DbIndex, file_id: FileId, name_expr: &LuaNameExpr) -> bool {
    name_expr_local_decl_id(db, file_id, name_expr).is_some()
}

fn name_expr_resolves_to_shadowing_local(
    db: &DbIndex,
    file_id: FileId,
    name_expr: &LuaNameExpr,
) -> bool {
    let Some(decl_id) = name_expr_local_decl_id(db, file_id, name_expr) else {
        return false;
    };
    let Some(name) = name_expr.get_name_text() else {
        return true;
    };
    !local_decl_aliases_global_name(db, decl_id, &name)
}

fn name_expr_local_decl_id(
    db: &DbIndex,
    file_id: FileId,
    name_expr: &LuaNameExpr,
) -> Option<LuaDeclId> {
    db.get_reference_index()
        .get_var_reference_decl(&file_id, name_expr.get_range())
        .filter(|decl_id| {
            db.get_decl_index()
                .get_decl(decl_id)
                .is_some_and(|decl| decl.is_local())
        })
}

fn local_decl_aliases_global_name(db: &DbIndex, decl_id: LuaDeclId, global_name: &str) -> bool {
    let Some((ret_idx, initializer)) = local_initializer_expr(db, decl_id) else {
        return false;
    };
    if ret_idx != 0 {
        return false;
    }

    match initializer {
        LuaExpr::NameExpr(name_expr) => {
            name_expr.get_name_text().as_deref() == Some(global_name)
                && !name_expr_resolves_to_local(db, decl_id.file_id, &name_expr)
        }
        LuaExpr::IndexExpr(index_expr) => {
            index_expr
                .get_access_path()
                .as_deref()
                .and_then(|path| path.strip_prefix("_G."))
                == Some(global_name)
                && index_expr_root_name(&index_expr)
                    .as_ref()
                    .is_none_or(|root| !name_expr_resolves_to_local(db, decl_id.file_id, root))
        }
        _ => false,
    }
}

fn annotated_roles_from_local_call_prefix(
    role_map: &AnnotatedGmodCallRoleMap,
    db: &DbIndex,
    file_id: FileId,
    prefix_expr: Option<LuaExpr>,
) -> Option<Option<AnnotatedGmodCallRoles>> {
    let LuaExpr::NameExpr(name_expr) = prefix_expr? else {
        return None;
    };
    annotated_roles_from_local_name_expr(role_map, db, file_id, &name_expr)
}

fn annotated_roles_from_local_call_path(
    role_map: &AnnotatedGmodCallRoleMap,
    db: &DbIndex,
    file_id: FileId,
    call_expr: &LuaCallExpr,
    call_path: &str,
) -> Option<Option<AnnotatedGmodCallRoles>> {
    let LuaExpr::IndexExpr(index_expr) = call_expr.get_prefix_expr()? else {
        return None;
    };
    let root_name_expr = index_expr_root_name(&index_expr)?;
    let decl_id = db
        .get_reference_index()
        .get_var_reference_decl(&file_id, root_name_expr.get_range())?;
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    if !decl.is_local() {
        return None;
    }
    if root_name_expr
        .get_name_text()
        .is_some_and(|root_name| local_decl_aliases_global_name(db, decl_id, &root_name))
    {
        return None;
    }

    Some(
        role_map
            .local_roles_by_path
            .get(&(decl_id, call_path.to_string()))
            .cloned(),
    )
}

fn annotated_roles_from_local_name_expr(
    role_map: &AnnotatedGmodCallRoleMap,
    db: &DbIndex,
    file_id: FileId,
    name_expr: &LuaNameExpr,
) -> Option<Option<AnnotatedGmodCallRoles>> {
    let decl_id = db
        .get_reference_index()
        .get_var_reference_decl(&file_id, name_expr.get_range())?;
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    if !decl.is_local() {
        return None;
    }
    if name_expr
        .get_name_text()
        .is_some_and(|name| local_decl_aliases_global_name(db, decl_id, &name))
    {
        return None;
    }
    if let Some(roles) = role_map.local_roles_by_decl.get(&decl_id) {
        return Some(Some(roles.clone()));
    }

    let Some(signature_id) =
        semantic_decl_closure_signature_id(db, crate::LuaSemanticDeclId::LuaDecl(decl_id))
    else {
        return Some(None);
    };
    Some(roles_from_signature(db, signature_id))
}

fn collect_system_call_metadata_into(
    db: &DbIndex,
    file_id: FileId,
    annotated_roles: &AnnotatedGmodCallRoleMap,
    call_expr: LuaCallExpr,
    out: &mut GmodSystemFileMetadata,
) -> Option<()> {
    let call_path = call_expr.get_access_path()?;
    let call_site = annotated_roles.system_call(db, file_id, &call_expr, &call_path)?;
    let kind = call_site.kind;

    match kind {
        GmodSystemCallKind::AddNetworkString => {
            let name_arg_idx = call_site.name_arg_idx?;
            let (name, name_range) = extract_static_string_arg(call_expr.clone(), name_arg_idx);
            out.net_add_string_calls.push(GmodNamedSiteMetadata {
                syntax_id: call_expr.get_syntax_id(),
                name,
                name_range,
            });
        }
        GmodSystemCallKind::NetStart => {
            let name_arg_idx = call_site.name_arg_idx?;
            let (name, name_range) = extract_static_string_arg(call_expr.clone(), name_arg_idx);
            out.net_start_calls.push(GmodNamedSiteMetadata {
                syntax_id: call_expr.get_syntax_id(),
                name,
                name_range,
            });
        }
        GmodSystemCallKind::NetReceive => {
            let name_arg_idx = call_site.name_arg_idx?;
            let (message_name, name_range) =
                extract_static_string_arg(call_expr.clone(), name_arg_idx);
            let callback = call_site
                .callback_arg_idx
                .and_then(|arg_idx| extract_callback_arg(call_expr.clone(), arg_idx))
                .or_else(|| extract_callback_arg(call_expr.clone(), name_arg_idx + 1))
                .unwrap_or_else(|| {
                    extract_first_callback_arg_after(call_expr.clone(), name_arg_idx)
                });
            out.net_receive_calls.push(GmodNetReceiveSiteMetadata {
                syntax_id: call_expr.get_syntax_id(),
                message_name,
                name_range,
                callback,
            });
        }
        GmodSystemCallKind::ConcommandAdd => {
            let name_arg_idx = call_site.name_arg_idx?;
            let (command_name, name_range) =
                extract_static_string_arg(call_expr.clone(), name_arg_idx);
            let callback = call_site
                .callback_arg_idx
                .and_then(|arg_idx| extract_callback_arg(call_expr.clone(), arg_idx))
                .or_else(|| extract_callback_arg(call_expr.clone(), name_arg_idx + 1))
                .unwrap_or_else(|| {
                    extract_first_callback_arg_after(call_expr.clone(), name_arg_idx)
                });
            out.concommand_add_calls.push(GmodConcommandSiteMetadata {
                syntax_id: call_expr.get_syntax_id(),
                command_name,
                name_range,
                callback,
            });
        }
        GmodSystemCallKind::CreateConVar | GmodSystemCallKind::CreateClientConVar => {
            let name_arg_idx = call_site.name_arg_idx?;
            let (convar_name, name_range) =
                extract_static_string_arg(call_expr.clone(), name_arg_idx);
            out.convar_create_calls.push(GmodConVarSiteMetadata {
                syntax_id: call_expr.get_syntax_id(),
                kind: if kind == GmodSystemCallKind::CreateClientConVar {
                    GmodConVarKind::Client
                } else {
                    GmodConVarKind::Server
                },
                convar_name,
                name_range,
            });
        }
        GmodSystemCallKind::TimerCreate => {
            let name_arg_idx = call_site.name_arg_idx?;
            let (timer_name, name_range) =
                extract_static_string_arg(call_expr.clone(), name_arg_idx);
            let callback = call_site
                .callback_arg_idx
                .and_then(|arg_idx| extract_callback_arg(call_expr.clone(), arg_idx))
                .or_else(|| extract_first_callback_arg_after_opt(call_expr.clone(), name_arg_idx))
                .unwrap_or_default();
            out.timer_calls.push(GmodTimerSiteMetadata {
                syntax_id: call_expr.get_syntax_id(),
                kind: GmodTimerKind::Create,
                timer_name,
                name_range,
                callback,
            });
        }
        GmodSystemCallKind::TimerSimple => {
            let callback = call_site
                .callback_arg_idx
                .and_then(|arg_idx| extract_callback_arg(call_expr.clone(), arg_idx))
                .unwrap_or_default();
            out.timer_calls.push(GmodTimerSiteMetadata {
                syntax_id: call_expr.get_syntax_id(),
                kind: GmodTimerKind::Simple,
                timer_name: None,
                name_range: None,
                callback,
            });
        }
    }

    Some(())
}

fn collect_annotated_scripted_class_call_metadata(
    db: &DbIndex,
    file_id: FileId,
    annotated_roles: &AnnotatedGmodCallRoleMap,
    call_expr: LuaCallExpr,
    pending: &mut Vec<PendingCallSite>,
) -> Option<()> {
    let call_path = call_expr.get_access_path()?;

    if let Some(roles) = annotated_roles.vgui_parent_call(db, file_id, &call_expr, &call_path) {
        let field_sources = vgui_parent_field_sources(&roles);
        let (_, args, field_args) =
            extract_gmod_class_call_args(db, file_id, &call_expr, &field_sources);
        let child =
            vgui_parent_call_source(&roles.child, call_expr.is_colon_call(), &args, &field_args);
        let parent =
            vgui_parent_call_source(&roles.parent, call_expr.is_colon_call(), &args, &field_args)
                .or_else(|| {
                    matches!(
                        child,
                        Some(
                            GmodVguiParentSource::Receiver | GmodVguiParentSource::ReceiverField(_)
                        )
                    )
                    .then_some(GmodVguiParentSource::Unknown)
                });
        if let (Some(child), Some(parent)) = (child, parent) {
            pending.push(PendingCallSite::VguiParent(GmodVguiParentCallMetadata {
                syntax_id: call_expr.get_syntax_id(),
                child,
                parent,
            }));
        }
    }

    if let Some((kind, inheritance_roles)) =
        annotated_roles.inheritance_call(db, file_id, &call_expr, &call_path)
    {
        let (literal_args, args, field_args) =
            extract_gmod_class_call_args(db, file_id, &call_expr, &[]);
        pending.push(PendingCallSite::ScriptedClass(
            kind,
            Box::new(GmodScriptedClassCallMetadata {
                syntax_id: call_expr.get_syntax_id(),
                literal_args,
                args,
                field_args,
                inheritance_roles: Some(inheritance_roles),
                network_var_roles: None,
                vgui_panel_roles: None,
                derma_skin_roles: None,
                register_roles: None,
            }),
        ));
        return Some(());
    }

    if let Some((kind, network_var_roles)) =
        annotated_roles.network_var_call(db, file_id, &call_expr, &call_path)
    {
        let (literal_args, args, field_args) =
            extract_gmod_class_call_args(db, file_id, &call_expr, &[]);
        pending.push(PendingCallSite::ScriptedClass(
            kind,
            Box::new(GmodScriptedClassCallMetadata {
                syntax_id: call_expr.get_syntax_id(),
                literal_args,
                args,
                field_args,
                inheritance_roles: None,
                network_var_roles: Some(network_var_roles),
                vgui_panel_roles: None,
                derma_skin_roles: None,
                register_roles: None,
            }),
        ));
        return Some(());
    }

    if let Some((kind, vgui_panel_roles)) =
        annotated_roles.vgui_panel_call(db, file_id, &call_expr, &call_path)
    {
        let field_sources = vgui_panel_field_sources(&vgui_panel_roles);
        let (literal_args, args, field_args) =
            extract_gmod_class_call_args(db, file_id, &call_expr, &field_sources);
        pending.push(PendingCallSite::ScriptedClass(
            kind,
            Box::new(GmodScriptedClassCallMetadata {
                syntax_id: call_expr.get_syntax_id(),
                literal_args,
                args,
                field_args,
                inheritance_roles: None,
                network_var_roles: None,
                vgui_panel_roles: Some(vgui_panel_roles),
                derma_skin_roles: None,
                register_roles: None,
            }),
        ));
        return Some(());
    }

    if let Some((kind, register_roles)) =
        annotated_roles.scripted_class_register_call(db, file_id, &call_expr, &call_path)
    {
        let (literal_args, args, field_args) =
            extract_gmod_class_call_args(db, file_id, &call_expr, &[]);
        pending.push(PendingCallSite::ScriptedClass(
            kind,
            Box::new(GmodScriptedClassCallMetadata {
                syntax_id: call_expr.get_syntax_id(),
                literal_args,
                args,
                field_args,
                inheritance_roles: None,
                network_var_roles: None,
                vgui_panel_roles: None,
                derma_skin_roles: None,
                register_roles: Some(register_roles),
            }),
        ));
        return Some(());
    }

    if let Some(derma_skin_roles) =
        annotated_roles.derma_skin_call(db, file_id, &call_expr, &call_path)
    {
        let (literal_args, args, field_args) =
            extract_gmod_class_call_args(db, file_id, &call_expr, &[]);
        pending.push(PendingCallSite::ScriptedClass(
            GmodScriptedClassCallKind::DermaDefineSkin,
            Box::new(GmodScriptedClassCallMetadata {
                syntax_id: call_expr.get_syntax_id(),
                literal_args,
                args,
                field_args,
                inheritance_roles: None,
                network_var_roles: None,
                vgui_panel_roles: None,
                derma_skin_roles: Some(derma_skin_roles),
                register_roles: None,
            }),
        ));
        return Some(());
    }

    None
}

fn vgui_parent_field_sources(roles: &AnnotatedVguiParentCallRoles) -> Vec<GmodClassCallArgSource> {
    let mut sources = Vec::new();
    for source in [&roles.child, &roles.parent] {
        let AnnotatedVguiParentSource::Arg(source) = source else {
            continue;
        };
        if source.is_derived() && !sources.iter().any(|existing| existing == source) {
            sources.push(source.clone());
        }
    }
    sources
}

fn vgui_parent_call_source(
    source: &AnnotatedVguiParentSource,
    is_colon_call: bool,
    args: &[GmodClassCallArg],
    field_args: &[crate::GmodClassCallFieldArg],
) -> Option<GmodVguiParentSource> {
    match source {
        AnnotatedVguiParentSource::Arg(source) => {
            let arg = if !source.is_derived() {
                args.get(source.arg_idx)
                    .map(|arg| (arg.syntax_id, arg.value.as_ref()))
            } else {
                field_args
                    .iter()
                    .find(|arg| arg.source == *source)
                    .map(|arg| (arg.syntax_id, arg.value.as_ref()))
            }?;
            match arg.1 {
                Some(GmodClassCallLiteral::String(name)) if !name.is_empty() => {
                    Some(GmodVguiParentSource::LiteralName(name.clone()))
                }
                _ => Some(GmodVguiParentSource::Expr(arg.0)),
            }
        }
        AnnotatedVguiParentSource::Receiver {
            field_path,
            dot_source,
        } if is_colon_call => {
            if field_path.is_empty() {
                Some(GmodVguiParentSource::Receiver)
            } else {
                Some(GmodVguiParentSource::ReceiverField(field_path.clone()))
            }
        }
        AnnotatedVguiParentSource::Receiver { dot_source, .. } => vgui_parent_call_source(
            &AnnotatedVguiParentSource::Arg(dot_source.clone()),
            false,
            args,
            field_args,
        ),
    }
}

fn configured_call_name(path: &str) -> &str {
    path.rsplit(['.', ':']).next().unwrap_or(path)
}

fn matches_configured_call_path(path: &str, target: &str) -> bool {
    path == target
        || path
            .strip_suffix(target)
            .is_some_and(|prefix| prefix.ends_with('.') || prefix.ends_with(':'))
}

fn extract_static_string_arg(
    call_expr: LuaCallExpr,
    arg_idx: usize,
) -> (Option<String>, Option<rowan::TextRange>) {
    let Some(arg_expr) = call_expr
        .get_args_list()
        .and_then(|args| args.get_args().nth(arg_idx))
    else {
        return (None, None);
    };

    let LuaExpr::LiteralExpr(literal_expr) = arg_expr else {
        return (None, None);
    };

    match literal_expr.get_literal() {
        Some(LuaLiteralToken::String(string_token)) => (
            Some(string_token.get_value()),
            Some(string_token.get_range()),
        ),
        Some(_) => (None, Some(literal_expr.get_range())),
        None => (None, Some(literal_expr.get_range())),
    }
}

fn extract_gmod_class_call_args(
    db: &DbIndex,
    file_id: FileId,
    call_expr: &LuaCallExpr,
    field_sources: &[crate::GmodClassCallArgSource],
) -> (
    Vec<Option<GmodClassCallLiteral>>,
    Vec<crate::GmodClassCallArg>,
    Vec<crate::GmodClassCallFieldArg>,
) {
    let Some(args_list) = call_expr.get_args_list() else {
        return (Vec::new(), Vec::new(), Vec::new());
    };

    let mut literal_args = Vec::new();
    let mut args = Vec::new();
    let arg_exprs = args_list.get_args().collect::<Vec<_>>();
    for arg_expr in &arg_exprs {
        let syntax_id = arg_expr.get_syntax_id();
        let value = extract_gmod_class_literal_or_name(arg_expr).or_else(|| {
            static_call_arg_string(db, file_id, call_expr, arg_expr)
                .map(GmodClassCallLiteral::String)
        });
        literal_args.push(value.clone());
        args.push(crate::GmodClassCallArg { syntax_id, value });
    }

    let mut field_args = Vec::new();
    for source in field_sources {
        if !source.is_derived() {
            continue;
        }
        let Some(arg_expr) = arg_exprs.get(source.arg_idx).cloned() else {
            continue;
        };
        let Some(value_expr) =
            resolve_static_field_path_expr(db, file_id, call_expr, arg_expr, &source.field_path)
        else {
            continue;
        };
        let static_string = || static_call_arg_string(db, file_id, call_expr, &value_expr);
        let value = if source.prefix.is_empty() && source.suffix.is_empty() {
            extract_gmod_class_literal_or_name(&value_expr)
                .or_else(|| static_string().map(GmodClassCallLiteral::String))
        } else {
            static_string().map(|text| {
                GmodClassCallLiteral::String(format!("{}{text}{}", source.prefix, source.suffix))
            })
        };
        field_args.push(crate::GmodClassCallFieldArg {
            source: source.clone(),
            syntax_id: value_expr.get_syntax_id(),
            value,
        });
    }

    (literal_args, args, field_args)
}

fn vgui_panel_field_sources(roles: &GmodVguiPanelCallRoles) -> Vec<crate::GmodClassCallArgSource> {
    let mut sources = Vec::new();
    for source in std::iter::once(&roles.define)
        .chain(roles.table.as_ref())
        .chain(roles.base.as_ref())
    {
        if source.is_derived() && !sources.iter().any(|existing| existing == source) {
            sources.push(source.clone());
        }
    }
    sources
}

fn resolve_static_field_path_expr(
    db: &DbIndex,
    file_id: FileId,
    call_expr: &LuaCallExpr,
    expr: LuaExpr,
    field_path: &[String],
) -> Option<LuaExpr> {
    if field_path.is_empty() {
        return Some(expr);
    }

    match expr {
        LuaExpr::TableExpr(table_expr) => {
            resolve_table_field_path_expr(table_expr, field_path, call_expr.get_position())
        }
        LuaExpr::ParenExpr(paren_expr) => resolve_static_field_path_expr(
            db,
            file_id,
            call_expr,
            paren_expr.get_expr()?,
            field_path,
        ),
        LuaExpr::NameExpr(name_expr) => {
            let root_path = name_expr.get_access_path()?;
            let root_decl_id = name_expr_local_decl_id(db, file_id, &name_expr);
            match find_prior_static_field_assignment(
                db,
                file_id,
                call_expr,
                &root_path,
                root_decl_id,
                field_path,
            ) {
                StaticFieldLookup::Value(value_expr) => return Some(value_expr),
                StaticFieldLookup::Blocked => return None,
                StaticFieldLookup::NoEvidence => {}
            }
            if let Some(value_expr) =
                resolve_name_initializer_field_path_expr(db, file_id, &name_expr, field_path)
            {
                return Some(value_expr);
            }
            None
        }
        LuaExpr::IndexExpr(index_expr) => {
            let root_path = index_expr.get_access_path()?;
            let root_decl_id = index_expr_root_name(&index_expr)
                .as_ref()
                .and_then(|name_expr| name_expr_local_decl_id(db, file_id, name_expr));
            match find_prior_static_field_assignment(
                db,
                file_id,
                call_expr,
                &root_path,
                root_decl_id,
                field_path,
            ) {
                StaticFieldLookup::Value(value_expr) => Some(value_expr),
                StaticFieldLookup::Blocked | StaticFieldLookup::NoEvidence => None,
            }
        }
        _ => None,
    }
}

fn resolve_name_initializer_field_path_expr(
    db: &DbIndex,
    file_id: FileId,
    name_expr: &LuaNameExpr,
    field_path: &[String],
) -> Option<LuaExpr> {
    let decl_id = db
        .get_reference_index()
        .get_var_reference_decl(&file_id, name_expr.get_range())?;
    let (_, initializer) = local_initializer_expr(db, decl_id)?;
    resolve_static_initializer_field_path_expr(initializer, field_path, name_expr.get_position())
}

fn resolve_static_initializer_field_path_expr(
    initializer: LuaExpr,
    field_path: &[String],
    before: TextSize,
) -> Option<LuaExpr> {
    match initializer {
        LuaExpr::TableExpr(table_expr) => {
            resolve_table_field_path_expr(table_expr, field_path, before)
        }
        LuaExpr::ParenExpr(paren_expr) => {
            resolve_static_initializer_field_path_expr(paren_expr.get_expr()?, field_path, before)
        }
        _ => None,
    }
}

fn resolve_table_field_path_expr(
    table_expr: LuaTableExpr,
    field_path: &[String],
    before: TextSize,
) -> Option<LuaExpr> {
    if table_expr.get_position() >= before {
        return None;
    }
    let field = find_table_field_by_name(&table_expr, &field_path[0])?;
    let value_expr = field.get_value_expr()?;
    if field_path.len() == 1 {
        return Some(value_expr);
    }
    resolve_static_initializer_field_path_expr(value_expr, &field_path[1..], before)
}

enum StaticFieldLookup {
    NoEvidence,
    Value(LuaExpr),
    Blocked,
}

fn find_prior_static_field_assignment(
    db: &DbIndex,
    file_id: FileId,
    call_expr: &LuaCallExpr,
    root_path: &str,
    root_decl_id: Option<LuaDeclId>,
    field_path: &[String],
) -> StaticFieldLookup {
    let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) else {
        return StaticFieldLookup::NoEvidence;
    };
    let root = tree.get_red_root();
    let Some(chunk) = LuaChunk::cast(root) else {
        return StaticFieldLookup::NoEvidence;
    };
    let call_blocks = call_expr
        .ancestors::<LuaBlock>()
        .map(|block| block.syntax().clone())
        .collect::<Vec<_>>();
    let call_position = call_expr.get_position();
    let target_path = format!("{root_path}.{}", field_path.join("."));
    let mut best = StaticFieldLookup::NoEvidence;

    for assign_stat in chunk.descendants::<LuaAssignStat>() {
        if assign_stat.get_position() >= call_position {
            continue;
        }
        let Some(assign_block) = assign_stat.ancestors::<LuaBlock>().next() else {
            continue;
        };
        if !call_blocks
            .iter()
            .any(|call_block| call_block == assign_block.syntax())
        {
            continue;
        }

        let (vars, exprs) = assign_stat.get_var_and_expr_list();
        for (idx, var_expr) in vars.iter().enumerate() {
            if !assignment_root_matches_target(db, file_id, var_expr, root_decl_id) {
                continue;
            }

            let Some(var_path) = var_expr.get_access_path() else {
                continue;
            };
            if var_path == target_path {
                best = exprs
                    .get(idx)
                    .cloned()
                    .map(StaticFieldLookup::Value)
                    .unwrap_or(StaticFieldLookup::Blocked);
                continue;
            }

            if var_path == root_path {
                best = match exprs.get(idx).cloned() {
                    Some(expr) => {
                        resolve_static_initializer_field_path_expr(expr, field_path, call_position)
                            .map(StaticFieldLookup::Value)
                            .unwrap_or(StaticFieldLookup::Blocked)
                    }
                    None => StaticFieldLookup::Blocked,
                }
            }
        }
    }

    best
}

fn assignment_root_matches_target(
    db: &DbIndex,
    file_id: FileId,
    var_expr: &LuaVarExpr,
    target_root_decl_id: Option<LuaDeclId>,
) -> bool {
    let Some(root_name) = var_expr_root_name(var_expr) else {
        return false;
    };
    let assignment_root_decl_id = name_expr_local_decl_id(db, file_id, &root_name);
    assignment_root_decl_id == target_root_decl_id
}

fn find_table_field_by_name(
    table_expr: &LuaTableExpr,
    field_name: &str,
) -> Option<glua_parser::LuaTableField> {
    table_expr
        .get_fields()
        .filter(|field| match field.get_field_key() {
            Some(LuaIndexKey::Name(name)) => name.get_name_text() == field_name,
            Some(LuaIndexKey::String(string)) => string.get_value() == field_name,
            _ => false,
        })
        .last()
}

fn extract_gmod_class_literal_or_name(expr: &LuaExpr) -> Option<GmodClassCallLiteral> {
    match expr {
        LuaExpr::LiteralExpr(literal_expr) => match literal_expr.get_literal()? {
            LuaLiteralToken::String(string_token) => Some(GmodClassCallLiteral::String(
                string_token.get_value().to_string(),
            )),
            LuaLiteralToken::Number(number_token) => match number_token.get_number_value() {
                NumberResult::Int(value) => Some(GmodClassCallLiteral::Integer(value)),
                NumberResult::Uint(value) => Some(GmodClassCallLiteral::Unsigned(value)),
                NumberResult::Float(value) => Some(GmodClassCallLiteral::Float(value)),
            },
            LuaLiteralToken::Bool(boolean_token) => {
                Some(GmodClassCallLiteral::Boolean(boolean_token.is_true()))
            }
            LuaLiteralToken::Nil(_) => Some(GmodClassCallLiteral::Nil),
            _ => None,
        },
        LuaExpr::NameExpr(name_expr) => name_expr
            .get_name_text()
            .map(|name| GmodClassCallLiteral::NameRef(name.to_string())),
        LuaExpr::ParenExpr(paren_expr) => {
            let inner = paren_expr.get_expr()?;
            extract_gmod_class_literal_or_name(&inner)
        }
        _ => None,
    }
}

fn extract_callback_arg(
    call_expr: LuaCallExpr,
    arg_idx: usize,
) -> Option<GmodCallbackSiteMetadata> {
    let callback_expr = call_expr
        .get_args_list()
        .and_then(|args| args.get_args().nth(arg_idx))?;

    Some(GmodCallbackSiteMetadata {
        syntax_id: Some(callback_expr.get_syntax_id()),
        callback_range: Some(callback_expr.get_range()),
    })
}

fn extract_first_callback_arg_after(
    call_expr: LuaCallExpr,
    arg_idx: usize,
) -> GmodCallbackSiteMetadata {
    extract_first_callback_arg_after_opt(call_expr, arg_idx).unwrap_or_default()
}

fn extract_first_callback_arg_after_opt(
    call_expr: LuaCallExpr,
    arg_idx: usize,
) -> Option<GmodCallbackSiteMetadata> {
    let args_list = call_expr.get_args_list()?;

    args_list
        .get_args()
        .skip(arg_idx + 1)
        .find(|arg_expr| matches!(arg_expr, LuaExpr::ClosureExpr(_)))
        .map(|callback_expr| GmodCallbackSiteMetadata {
            syntax_id: Some(callback_expr.get_syntax_id()),
            callback_range: Some(callback_expr.get_range()),
        })
}

fn collect_hook_call_site(
    db: &DbIndex,
    file_id: FileId,
    annotated_roles: &AnnotatedGmodCallRoleMap,
    call_expr: LuaCallExpr,
) -> Option<GmodHookSiteMetadata> {
    let call_path = call_expr.get_access_path()?;
    let has_shadowing_local_root = call_expr_has_shadowing_local_root(db, file_id, &call_expr);
    let annotated_hook = annotated_roles.hook_call(db, file_id, &call_expr, &call_path);
    let mapped_hook = if has_shadowing_local_root {
        None
    } else {
        mapped_hook_for_emitter_call(db, file_id, &call_path, call_expr.clone())
    };
    let (kind, name_arg_idx, callback_arg_idx, mapped_hook_data) =
        if let Some((kind, name_arg_idx, callback_arg_idx)) = annotated_hook {
            (kind, name_arg_idx, callback_arg_idx, None)
        } else {
            (GmodHookKind::Emit, 0, None, Some(mapped_hook?))
        };
    let (hook_name, name_range, name_issue) = mapped_hook_data.unwrap_or_else(|| {
        extract_static_hook_name(
            call_expr
                .get_args_list()
                .and_then(|args| args.get_args().nth(name_arg_idx)),
        )
    });

    let callback_arg_idx = if kind == GmodHookKind::Add {
        callback_arg_idx.or_else(|| find_first_callback_arg_idx_after(&call_expr, name_arg_idx))
    } else {
        callback_arg_idx
    };

    Some(GmodHookSiteMetadata {
        syntax_id: call_expr.get_syntax_id(),
        kind,
        hook_name,
        name_range,
        name_issue,
        callback_arg_idx,
        callback_params: if kind == GmodHookKind::Add {
            extract_hook_callback_params_from_call(&call_expr, name_arg_idx, callback_arg_idx)
        } else {
            Vec::new()
        },
    })
}

fn mapped_hook_for_emitter_call(
    db: &DbIndex,
    file_id: FileId,
    call_path: &str,
    call_expr: LuaCallExpr,
) -> Option<(
    Option<String>,
    Option<rowan::TextRange>,
    Option<GmodHookNameIssue>,
)> {
    let (_, mapped_hook) = db
        .get_file_emmyrc(file_id)
        .gmod
        .hook_mappings
        .emitter_to_hook
        .iter()
        .filter(|(emitter_path, _)| matches_configured_call_path(call_path, emitter_path))
        .max_by(|(left, _), (right, _)| {
            left.len().cmp(&right.len()).then_with(|| right.cmp(left))
        })?;

    if mapped_hook == "*" {
        return Some(extract_static_hook_name(
            call_expr
                .get_args_list()
                .and_then(|args| args.get_args().next()),
        ));
    }

    let trimmed = mapped_hook.trim();
    Some(if trimmed.is_empty() {
        (None, None, Some(GmodHookNameIssue::Empty))
    } else {
        (Some(trimmed.to_string()), None, None)
    })
}

fn collect_hook_method_site(
    db: &DbIndex,
    file_id: FileId,
    func_stat: LuaFuncStat,
) -> Option<GmodHookSiteMetadata> {
    let LuaVarExpr::IndexExpr(index_expr) = func_stat.get_func_name()? else {
        return None;
    };
    let is_colon = index_expr.get_index_token()?.is_colon();

    let LuaExpr::NameExpr(prefix_name_expr) = index_expr.get_prefix_expr()? else {
        return None;
    };

    let prefix_name = prefix_name_expr.get_name_text()?;
    let separator = if is_colon { ":" } else { "." };

    let (method_name, name_range) = match index_expr.get_index_key()? {
        LuaIndexKey::Name(name_token) => (
            Some(name_token.get_name_text().to_string()),
            Some(name_token.get_range()),
        ),
        LuaIndexKey::String(string_token) => (
            Some(string_token.get_value()),
            Some(string_token.get_range()),
        ),
        _ => (None, None),
    };

    let mapped_method_hook = method_mapped_hook_name(
        db,
        file_id,
        &prefix_name,
        separator,
        method_name.as_deref().unwrap_or_default(),
    );
    let annotation = hook_annotation_from_doc(&func_stat);
    let trimmed_method_name = method_name
        .as_ref()
        .map(|name| name.trim().to_string())
        .unwrap_or_default();
    let (hook_name, mut name_issue) = if let Some((hook_name, name_issue)) = mapped_method_hook {
        (hook_name, name_issue)
    } else if let Some(annotation_hook) = annotation
        && (is_builtin_method_hook_prefix(&prefix_name)
            || is_configured_method_hook_prefix(db, file_id, &prefix_name))
    {
        let hook_name = annotation_hook.hook_name.or_else(|| {
            (!trimmed_method_name.is_empty()).then_some(trimmed_method_name.to_string())
        });
        let name_issue = if hook_name.is_none() {
            Some(GmodHookNameIssue::Empty)
        } else {
            annotation_hook.name_issue
        };
        (hook_name, name_issue)
    } else {
        if !is_colon
            || (!is_builtin_method_hook_prefix(&prefix_name)
                && !is_configured_method_hook_prefix(db, file_id, &prefix_name))
        {
            return None;
        }
        let name_issue = trimmed_method_name
            .is_empty()
            .then_some(GmodHookNameIssue::Empty);
        let hook_name = (!trimmed_method_name.is_empty()).then_some(trimmed_method_name);
        (hook_name, name_issue)
    };

    let hook_name = normalize_gamemode_hook_name(hook_name);
    if hook_name.is_none() && name_issue.is_none() {
        name_issue = Some(GmodHookNameIssue::Empty);
    }

    Some(GmodHookSiteMetadata {
        syntax_id: index_expr.get_syntax_id(),
        kind: GmodHookKind::GamemodeMethod,
        hook_name,
        name_range,
        name_issue,
        callback_arg_idx: None,
        callback_params: extract_hook_callback_params_from_method(&func_stat),
    })
}

fn extract_hook_callback_params_from_call(
    call_expr: &LuaCallExpr,
    name_arg_idx: usize,
    callback_arg_idx: Option<usize>,
) -> Vec<String> {
    let Some(args_list) = call_expr.get_args_list() else {
        return Vec::new();
    };

    let callback_expr = if let Some(callback_arg_idx) = callback_arg_idx {
        args_list.get_args().nth(callback_arg_idx)
    } else {
        args_list
            .get_args()
            .skip(name_arg_idx + 1)
            .find(|arg_expr| matches!(arg_expr, LuaExpr::ClosureExpr(_)))
    };
    let Some(callback_expr) = callback_expr else {
        return Vec::new();
    };
    let LuaExpr::ClosureExpr(closure_expr) = callback_expr else {
        return Vec::new();
    };

    extract_param_names_from_closure(closure_expr)
}

fn find_first_callback_arg_idx_after(call_expr: &LuaCallExpr, arg_idx: usize) -> Option<usize> {
    call_expr
        .get_args_list()?
        .get_args()
        .enumerate()
        .skip(arg_idx + 1)
        .find(|(_, arg_expr)| matches!(arg_expr, LuaExpr::ClosureExpr(_)))
        .map(|(idx, _)| idx)
}

fn extract_hook_callback_params_from_method(func_stat: &LuaFuncStat) -> Vec<String> {
    let Some(closure_expr) = func_stat.get_closure() else {
        return Vec::new();
    };

    extract_param_names_from_closure(closure_expr)
}

fn extract_param_names_from_closure(closure_expr: glua_parser::LuaClosureExpr) -> Vec<String> {
    let Some(params_list) = closure_expr.get_params_list() else {
        return Vec::new();
    };

    params_list
        .get_params()
        .filter_map(|param| {
            if param.is_dots() {
                Some("...".to_string())
            } else {
                Some(param.get_name_token()?.get_name_text().to_string())
            }
        })
        .collect()
}

fn is_builtin_method_hook_prefix(prefix_name: &str) -> bool {
    crate::BUILTIN_HOOK_OWNERS.contains(&prefix_name)
}

fn is_configured_method_hook_prefix(db: &DbIndex, file_id: FileId, prefix_name: &str) -> bool {
    let gmod = &db.get_file_emmyrc(file_id).gmod;
    gmod.hook_mappings
        .method_prefixes
        .iter()
        .any(|configured_prefix| {
            configured_prefix
                .trim()
                .trim_end_matches([':', '.'])
                .eq_ignore_ascii_case(prefix_name)
        })
        || gmod
            .scripted_class_scopes
            .resolved_definitions_slice()
            .iter()
            .any(|definition| {
                (definition.hook_owner
                    || definition
                        .super_types
                        .iter()
                        .any(|super_type| super_type.eq_ignore_ascii_case("GM")))
                    && (definition.class_global.eq_ignore_ascii_case(prefix_name)
                        || definition
                            .aliases
                            .iter()
                            .any(|alias| alias.eq_ignore_ascii_case(prefix_name)))
            })
}

#[derive(Debug, Clone)]
struct HookAnnotationMetadata {
    hook_name: Option<String>,
    name_issue: Option<GmodHookNameIssue>,
}

fn hook_annotation_from_doc(func_stat: &LuaFuncStat) -> Option<HookAnnotationMetadata> {
    let comment = func_stat.get_left_comment()?;
    for tag in comment.get_doc_tags() {
        let LuaDocTag::Other(other_tag) = tag else {
            continue;
        };
        let tag_name = other_tag.get_tag_name()?;
        if !tag_name
            .trim_start_matches('@')
            .eq_ignore_ascii_case("hook")
        {
            continue;
        }

        let annotation_value = other_tag
            .get_description()
            .map(|description| description.get_description_text())
            .unwrap_or_default();
        let normalized = annotation_value.trim();
        let hook_name = (!normalized.is_empty()).then_some(normalized.to_string());

        return Some(HookAnnotationMetadata {
            hook_name,
            name_issue: None,
        });
    }

    None
}

fn method_mapped_hook_name(
    db: &DbIndex,
    file_id: FileId,
    prefix_name: &str,
    separator: &str,
    method_name: &str,
) -> Option<(Option<String>, Option<GmodHookNameIssue>)> {
    let mappings = &db
        .get_file_emmyrc(file_id)
        .gmod
        .hook_mappings
        .method_to_hook;
    let method_name = method_name.trim();
    let mut candidates = vec![format!("{prefix_name}{separator}{method_name}")];
    if separator == ":" {
        candidates.push(format!("{prefix_name}.{method_name}"));
    } else {
        candidates.push(format!("{prefix_name}:{method_name}"));
    }

    for candidate in candidates {
        let Some(mapped_hook) = mappings.get(&candidate) else {
            continue;
        };

        if mapped_hook == "*" {
            return Some((
                (!method_name.is_empty()).then_some(method_name.to_string()),
                method_name.is_empty().then_some(GmodHookNameIssue::Empty),
            ));
        }

        let trimmed = mapped_hook.trim();
        return Some(if trimmed.is_empty() {
            (None, Some(GmodHookNameIssue::Empty))
        } else {
            (Some(trimmed.to_string()), None)
        });
    }

    None
}

fn normalize_gamemode_hook_name(hook_name: Option<String>) -> Option<String> {
    let hook_name = hook_name?;
    let trimmed = hook_name.trim();
    if trimmed.is_empty() {
        return None;
    }

    let normalized = strip_builtin_method_hook_prefix(trimmed)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(trimmed);

    Some(normalized.to_string())
}

fn strip_builtin_method_hook_prefix(name: &str) -> Option<&str> {
    for separator in [':', '.'] {
        let Some((prefix, remainder)) = name.split_once(separator) else {
            continue;
        };

        if is_builtin_method_hook_prefix(prefix.trim()) {
            return Some(remainder);
        }
    }

    None
}

fn extract_static_hook_name(
    first_arg: Option<LuaExpr>,
) -> (
    Option<String>,
    Option<rowan::TextRange>,
    Option<GmodHookNameIssue>,
) {
    let Some(first_arg) = first_arg else {
        return (None, None, None);
    };

    let LuaExpr::LiteralExpr(literal_expr) = first_arg else {
        return (None, None, None);
    };

    match literal_expr.get_literal() {
        Some(LuaLiteralToken::String(string_token)) => {
            let hook_name = string_token.get_value();
            let issue = hook_name
                .trim()
                .is_empty()
                .then_some(GmodHookNameIssue::Empty);
            (Some(hook_name), Some(string_token.get_range()), issue)
        }
        Some(_) => (
            None,
            Some(literal_expr.get_range()),
            Some(GmodHookNameIssue::NonStringLiteral),
        ),
        None => (
            None,
            Some(literal_expr.get_range()),
            Some(GmodHookNameIssue::NonStringLiteral),
        ),
    }
}

/// Detect `if CLIENT then`/`if SERVER then` blocks and return realm-narrowed ranges.
fn collect_branch_realm_ranges(root: &LuaChunk) -> Vec<GmodRealmRange> {
    let mut ranges = Vec::new();
    for if_stat in root.descendants::<LuaIfStat>() {
        collect_if_realm_ranges(&if_stat, &mut ranges);
    }
    ranges.sort_by_key(|range| (range.range.len(), range.range.start()));
    ranges
}

/// Collect the first `---@realm client|server|shared` annotation from a file.
fn collect_realm_annotation(root: &LuaChunk) -> Option<GmodRealm> {
    for comment in root.descendants::<LuaComment>() {
        let is_file_level = matches!(comment.get_owner(), None | Some(LuaAst::LuaChunk(_)));
        if !is_file_level {
            continue;
        }

        if let Some(realm) = realm_from_doc_comment(&comment) {
            return Some(realm);
        }
    }

    None
}

pub(crate) fn realm_from_doc_comment(comment: &LuaComment) -> Option<GmodRealm> {
    for tag in comment.get_doc_tags() {
        if let LuaDocTag::Realm(realm_tag) = tag
            && let Some(realm) = realm_from_doc_tag(&realm_tag)
        {
            return Some(realm);
        }
    }

    None
}

fn realm_from_doc_tag(tag: &LuaDocTagRealm) -> Option<GmodRealm> {
    let name = tag.get_name_token()?;
    match name.get_name_text() {
        "client" => Some(GmodRealm::Client),
        "server" => Some(GmodRealm::Server),
        "shared" => Some(GmodRealm::Shared),
        "menu" => Some(GmodRealm::Menu),
        _ => None,
    }
}

/// Collect `---@realm` ranges from func/local-func decls in `root`.
fn collect_member_realm_ranges(root: &LuaChunk) -> Vec<GmodRealmRange> {
    let mut ranges = Vec::new();
    // Single descendants walk: FuncStat and LocalFuncStat both contribute.
    for node in root.syntax().descendants() {
        if let Some(func_stat) = LuaFuncStat::cast(node.clone()) {
            if let Some(comment) = func_stat.get_left_comment()
                && let Some(realm) = realm_from_doc_comment(&comment)
            {
                ranges.push(GmodRealmRange {
                    range: func_stat.get_range(),
                    realm,
                });
            }
            continue;
        }
        if let Some(local_func_stat) = LuaLocalFuncStat::cast(node) {
            if let Some(comment) = local_func_stat.get_left_comment()
                && let Some(realm) = realm_from_doc_comment(&comment)
            {
                ranges.push(GmodRealmRange {
                    range: local_func_stat.get_range(),
                    realm,
                });
            }
        }
    }
    ranges
}

/// Extract realm narrowing from a single if-statement, handling if/elseif/else clauses.
/// Also handles early-return guards like `if not CLIENT then return end` which narrows
/// the realm of code after the if-statement to the complementary realm.
fn collect_if_realm_ranges(if_stat: &LuaIfStat, ranges: &mut Vec<GmodRealmRange>) {
    let condition_realm = if_stat
        .get_condition_expr()
        .as_ref()
        .and_then(realm_from_condition);

    if let Some(realm) = condition_realm {
        if let Some(block) = if_stat.get_block() {
            let range = block.syntax().text_range();
            ranges.push(GmodRealmRange { range, realm });
        } else {
            // Empty block (e.g., comment-only if-body): still record the realm
            // so that realm-awareness checks (like AddCSLuaFile CLIENT detection) work.
            // Use a zero-width range at the start of the if-statement as a marker.
            let pos = if_stat.syntax().text_range().start();
            ranges.push(GmodRealmRange {
                range: TextRange::new(pos, pos),
                realm,
            });
        }

        // Identify the complementary realm for else block
        let complement = match realm {
            GmodRealm::Client => Some(GmodRealm::Server),
            GmodRealm::Server => Some(GmodRealm::Client),
            _ => None,
        };

        // Handle elseif/else clauses
        let mut has_elseif = false;
        for clause in if_stat.get_all_clause() {
            match &clause {
                glua_parser::LuaIfClauseStat::ElseIf(elseif) => {
                    has_elseif = true;
                    if let Some(elseif_realm) = elseif
                        .get_condition_expr()
                        .as_ref()
                        .and_then(realm_from_condition)
                    {
                        if let Some(block) = elseif.get_block() {
                            ranges.push(GmodRealmRange {
                                range: block.syntax().text_range(),
                                realm: elseif_realm,
                            });
                        }
                    }
                }
                glua_parser::LuaIfClauseStat::Else(else_clause) => {
                    // Only assign complement realm if there's no elseif
                    // (with elseif, else block realm is ambiguous)
                    if !has_elseif {
                        if let Some(complement_realm) = complement {
                            if let Some(block) = else_clause.get_block() {
                                ranges.push(GmodRealmRange {
                                    range: block.syntax().text_range(),
                                    realm: complement_realm,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Check for early-return guard: `if not REALM then return end` or `if REALM then return end`
        // This should narrow the code AFTER the if-statement to the complement realm
        if let Some(block) = if_stat.get_block() {
            if is_early_return_block(&block) {
                if let Some(parent_block) = find_parent_block(if_stat.syntax()) {
                    let if_end = if_stat.syntax().text_range().end();
                    let block_end = parent_block.syntax().text_range().end();
                    let after_range = TextRange::new(if_end, block_end);

                    let after_realm = if let Some(expr) = if_stat.get_condition_expr() {
                        if is_not_condition(&expr) {
                            // `if not CLIENT then return end` → code after is Client
                            get_original_realm_from_complement(realm)
                        } else {
                            // `if CLIENT then return end` → code after is Server
                            complement
                        }
                    } else {
                        None
                    };

                    if let Some(after_realm) = after_realm {
                        ranges.push(GmodRealmRange {
                            range: after_range,
                            realm: after_realm,
                        });
                    }
                }
            }
        }
    }
}

/// Check if a condition expression is a "not" unary expression
fn is_not_condition(expr: &LuaExpr) -> bool {
    match expr {
        LuaExpr::ParenExpr(paren_expr) => {
            // Handle `(not CLIENT)` - check inside parentheses
            if let Some(inner) = paren_expr.get_expr() {
                return is_not_condition(&inner);
            }
            false
        }
        LuaExpr::UnaryExpr(unary_expr) => {
            let op = unary_expr.get_op_token();
            if let Some(op) = op {
                let op_kind = op.get_op();
                return op_kind == glua_parser::UnaryOperator::OpNot;
            }
            false
        }
        _ => false,
    }
}

/// Given a complement realm (e.g., Server from `not CLIENT`), get the original realm (Client)
fn get_original_realm_from_complement(complement: GmodRealm) -> Option<GmodRealm> {
    match complement {
        GmodRealm::Client => Some(GmodRealm::Server),
        GmodRealm::Server => Some(GmodRealm::Client),
        _ => None,
    }
}

/// Check if a block contains only a return statement (early-return guard pattern)
fn is_early_return_block(block: &LuaBlock) -> bool {
    let mut stats = block.get_stats().peekable();

    // Check if there's exactly one statement
    let first_stat = stats.next();
    if first_stat.is_none() || stats.peek().is_some() {
        return false;
    }

    // Check if that statement is a return statement
    matches!(first_stat, Some(LuaStat::ReturnStat(_)))
}

/// Find the parent block containing a syntax node
fn find_parent_block(node: &LuaSyntaxNode) -> Option<LuaBlock> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if let Some(block) = LuaBlock::cast(parent.clone()) {
            return Some(block);
        }
        current = parent.parent();
    }
    None
}

/// Match condition expressions to realms.
/// Handles: `CLIENT`, `SERVER`, `not CLIENT`, `not SERVER`, `(CLIENT)`, `(SERVER)`
pub(crate) fn realm_from_condition(expr: &LuaExpr) -> Option<GmodRealm> {
    match expr {
        // Handle parentheses: extract inner expression and recurse
        LuaExpr::ParenExpr(paren_expr) => paren_expr
            .get_expr()
            .as_ref()
            .and_then(realm_from_condition),
        LuaExpr::NameExpr(name_expr) => match name_expr.get_name_text()?.as_str() {
            "CLIENT" => Some(GmodRealm::Client),
            "SERVER" => Some(GmodRealm::Server),
            "MENU_DLL" => Some(GmodRealm::Menu),
            _ => None,
        },
        LuaExpr::UnaryExpr(unary_expr) => {
            let op = unary_expr.get_op_token()?;
            let op_kind = op.get_op();
            if op_kind == glua_parser::UnaryOperator::OpNot {
                let inner = unary_expr.get_expr()?;
                let inner_realm = realm_from_condition(&inner)?;
                match inner_realm {
                    GmodRealm::Client => Some(GmodRealm::Server),
                    GmodRealm::Server => Some(GmodRealm::Client),
                    GmodRealm::Menu => None,
                    _ => None,
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompilefileTargetState {
    Unique(FileId),
    Ambiguous,
}

fn collect_compilefile_execution_environment_flow(
    db: &DbIndex,
    file_id: FileId,
    annotated_global_call_roles: &AnnotatedGmodGlobalCallRoleMap,
) -> GmodExecutionEnvironmentFileFlow {
    let Some(content) = db.get_vfs().get_file_content(&file_id) else {
        return GmodExecutionEnvironmentFileFlow::default();
    };
    let candidates = annotated_global_call_roles.candidate_presence(db, file_id);
    if !candidates.has_environment
        && !content.contains("gmod.environment")
        && !content.contains("compilefile")
    {
        return GmodExecutionEnvironmentFileFlow::default();
    }
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_chunk_node())
    else {
        return GmodExecutionEnvironmentFileFlow::default();
    };
    let roles = AnnotatedGmodCallRoleMap::build(db, file_id, &root, annotated_global_call_roles);
    let reassigned_decls = collect_reassigned_local_decls(db, file_id, &root);
    let reassigned_decls = &reassigned_decls;
    let mut flow = GmodExecutionEnvironmentFileFlow::default();

    let local_functions = root
        .descendants::<LuaLocalFuncStat>()
        .filter_map(|stat| {
            let local_name = stat.get_local_name()?;
            let function_decl = LuaDeclId::new(file_id, local_name.get_position());
            let closure = stat.get_closure()?;
            let params = closure
                .get_params_list()
                .into_iter()
                .flat_map(|params| params.get_params())
                .filter(|param| !param.is_dots())
                .map(|param| LuaDeclId::new(file_id, param.get_position()))
                .collect::<Vec<_>>();
            Some((function_decl, params))
        })
        .collect::<HashMap<_, _>>();

    for local_stat in root.descendants::<LuaLocalStat>() {
        let names = local_stat.get_local_name_list().collect::<Vec<_>>();
        let values = local_stat.get_value_exprs().collect::<Vec<_>>();
        for (idx, local_name) in names.iter().enumerate() {
            let Some(value) = values.get(idx) else {
                continue;
            };
            let destination = LuaDeclId::new(file_id, local_name.get_position());
            if reassigned_decls.contains(&destination) {
                continue;
            }
            if let Some(source) = compilefile_chunk_source_from_expr(
                db,
                file_id,
                &roles,
                value,
                reassigned_decls,
                &mut HashSet::default(),
            ) {
                add_compilefile_flow(&mut flow, destination, source);
            }
        }
    }

    for call_expr in root.descendants::<LuaCallExpr>() {
        let Some(call_path) = call_expr.get_access_path() else {
            continue;
        };
        let args = call_expr
            .get_args_list()
            .map(|args| args.get_args().collect::<Vec<_>>())
            .unwrap_or_default();

        if let Some((target_idx, environment_idx)) =
            roles.environment_call(db, file_id, &call_expr, &call_path)
        {
            let Some(target_expr) = args.get(target_idx) else {
                continue;
            };
            let Some(environment_expr) = args.get(environment_idx) else {
                continue;
            };
            let Some(source) = compilefile_chunk_source_from_expr(
                db,
                file_id,
                &roles,
                target_expr,
                reassigned_decls,
                &mut HashSet::default(),
            ) else {
                continue;
            };
            let Some(fields) = static_environment_fields(
                db,
                file_id,
                environment_expr,
                reassigned_decls,
                &mut HashSet::default(),
            ) else {
                continue;
            };
            flow.sites
                .push(GmodExecutionEnvironmentSite { source, fields });
            continue;
        }

        let Some(callee_decl) = call_expr.get_prefix_expr().and_then(|expr| match expr {
            LuaExpr::NameExpr(name_expr) => name_expr_local_decl_id(db, file_id, &name_expr),
            LuaExpr::ParenExpr(paren) => paren.get_expr().and_then(|expr| match expr {
                LuaExpr::NameExpr(name_expr) => name_expr_local_decl_id(db, file_id, &name_expr),
                _ => None,
            }),
            _ => None,
        }) else {
            continue;
        };
        let Some(params) = local_functions.get(&callee_decl) else {
            continue;
        };
        for (param, arg) in params.iter().zip(&args) {
            if reassigned_decls.contains(param) {
                continue;
            }
            if let Some(source) = compilefile_chunk_source_from_expr(
                db,
                file_id,
                &roles,
                arg,
                reassigned_decls,
                &mut HashSet::default(),
            ) {
                add_compilefile_flow(&mut flow, *param, source);
            }
        }
    }

    flow
}

/// Every `CompileFile` target's execution environment, resolved from the
/// files' load scans.
pub(super) fn rebuild_compilefile_execution_environments(db: &mut DbIndex) {
    let mut environments = HashMap::<FileId, HashMap<FileId, HashSet<String>>>::default();
    let mut cached_source_files = 0usize;
    let mut edge_count = 0usize;
    let mut seed_count = 0usize;
    let mut site_count = 0usize;
    for (source_file_id, scan) in db.load_scans::<GmodFileLoadScan>() {
        let flow = &scan.flow;
        if flow.is_empty() {
            continue;
        }
        cached_source_files += 1;
        edge_count += flow.edges.values().map(Vec::len).sum::<usize>();
        seed_count += flow.seeds.len();
        site_count += flow.sites.len();

        let mut targets = HashMap::<LuaDeclId, CompilefileTargetState>::default();
        let mut queue = VecDeque::new();
        for (decl_id, path) in &flow.seeds {
            let Some(target) = resolve_compilefile_target(db, source_file_id, path) else {
                continue;
            };
            if merge_compilefile_target(
                &mut targets,
                *decl_id,
                CompilefileTargetState::Unique(target),
            ) {
                queue.push_back(*decl_id);
            }
        }
        while let Some(source) = queue.pop_front() {
            let Some(target) = targets.get(&source).copied() else {
                continue;
            };
            for destination in flow.edges.get(&source).into_iter().flatten() {
                if merge_compilefile_target(&mut targets, *destination, target) {
                    queue.push_back(*destination);
                }
            }
        }

        for site in &flow.sites {
            let target = match &site.source {
                GmodExecutionEnvironmentSource::Path(path) => {
                    resolve_compilefile_target(db, source_file_id, path)
                }
                GmodExecutionEnvironmentSource::Decl(decl_id) => {
                    match targets.get(decl_id).copied() {
                        Some(CompilefileTargetState::Unique(file_id)) => Some(file_id),
                        Some(CompilefileTargetState::Ambiguous) | None => None,
                    }
                }
            };
            if let Some(target) = target {
                environments
                    .entry(source_file_id)
                    .or_default()
                    .entry(target)
                    .or_default()
                    .extend(site.fields.iter().cloned());
            }
        }
    }

    if std::env::var_os("GLUALS_PROFILE").is_some() {
        eprintln!(
            "[profile] compilefile_environments source_files={cached_source_files} edges={edge_count} seeds={seed_count} sites={site_count} sources={} targets={}",
            environments.len(),
            environments.values().map(HashMap::len).sum::<usize>(),
        );
    }

    db.get_gmod_load_index_mut()
        .set_execution_environment_sites(environments);
}

fn merge_compilefile_target(
    targets: &mut HashMap<LuaDeclId, CompilefileTargetState>,
    decl_id: LuaDeclId,
    incoming: CompilefileTargetState,
) -> bool {
    let Some(current) = targets.get_mut(&decl_id) else {
        targets.insert(decl_id, incoming);
        return true;
    };
    let merged = match (*current, incoming) {
        (CompilefileTargetState::Ambiguous, _) | (_, CompilefileTargetState::Ambiguous) => {
            CompilefileTargetState::Ambiguous
        }
        (CompilefileTargetState::Unique(left), CompilefileTargetState::Unique(right))
            if left != right =>
        {
            CompilefileTargetState::Ambiguous
        }
        _ => *current,
    };
    if *current == merged {
        false
    } else {
        *current = merged;
        true
    }
}

fn add_compilefile_flow(
    flow: &mut GmodExecutionEnvironmentFileFlow,
    destination: LuaDeclId,
    source: GmodExecutionEnvironmentSource,
) {
    match source {
        GmodExecutionEnvironmentSource::Path(path) => flow.seeds.push((destination, path)),
        GmodExecutionEnvironmentSource::Decl(source) => {
            flow.edges.entry(source).or_default().push(destination)
        }
    }
}

fn compilefile_chunk_source_from_expr(
    db: &DbIndex,
    file_id: FileId,
    roles: &AnnotatedGmodCallRoleMap<'_>,
    expr: &LuaExpr,
    reassigned_decls: &HashSet<LuaDeclId>,
    visiting_strings: &mut HashSet<LuaDeclId>,
) -> Option<GmodExecutionEnvironmentSource> {
    match expr {
        LuaExpr::NameExpr(name_expr) => {
            let decl_id = name_expr_local_decl_id(db, file_id, name_expr)?;
            (!reassigned_decls.contains(&decl_id))
                .then_some(GmodExecutionEnvironmentSource::Decl(decl_id))
        }
        LuaExpr::ParenExpr(paren) => compilefile_chunk_source_from_expr(
            db,
            file_id,
            roles,
            &paren.get_expr()?,
            reassigned_decls,
            visiting_strings,
        ),
        LuaExpr::CallExpr(call_expr) => {
            let call_path = call_expr.get_access_path()?;
            let path_idx = roles.compilefile_call(db, file_id, call_expr, &call_path)?;
            let path_expr = call_expr.get_args_list()?.get_args().nth(path_idx)?;
            let path = static_compilefile_path(
                db,
                file_id,
                &path_expr,
                reassigned_decls,
                visiting_strings,
            )?;
            Some(GmodExecutionEnvironmentSource::Path(path))
        }
        _ => None,
    }
}

fn resolve_compilefile_target(
    db: &DbIndex,
    source_file_id: FileId,
    dependency_path: &str,
) -> Option<FileId> {
    let normalized_path = dependency_path.replace('\\', "/");
    let normalized_path = normalized_path.trim_start_matches("./");
    let path = Path::new(normalized_path);
    if normalized_path.is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return None;
    }
    let normalized_no_ext = normalized_path
        .strip_suffix(".lua")
        .unwrap_or(normalized_path);
    let root_module_path = normalized_no_ext.replace('/', ".");
    let requested_path = normalized_path.to_ascii_lowercase();
    let module_index = db.get_module_index();
    let source_workspace = module_index.get_workspace_id(source_file_id);
    let module_paths = [format!("lua.{root_module_path}"), root_module_path];
    for module_path in module_paths {
        if let Some(module_file_id) =
            module_index.find_module_file_for_file(&module_path, source_file_id)
            && compilefile_target_path_matches(
                db,
                module_file_id,
                &requested_path,
                normalized_path == normalized_no_ext,
            )
        {
            return Some(module_file_id);
        }

        let target = module_index
            .find_module_node(&module_path)
            .into_iter()
            .flat_map(|node| node.file_ids.iter().copied())
            .filter(|file_id| {
                source_workspace.is_none_or(|workspace_id| {
                    module_index.get_workspace_id(*file_id) == Some(workspace_id)
                })
            })
            .filter(|file_id| {
                compilefile_target_path_matches(
                    db,
                    *file_id,
                    &requested_path,
                    normalized_path == normalized_no_ext,
                )
            })
            .min_by(|left, right| {
                db.get_vfs()
                    .get_file_path(left)
                    .cmp(&db.get_vfs().get_file_path(right))
                    .then_with(|| left.cmp(right))
            });
        if target.is_some() {
            return target;
        }
    }

    None
}

fn compilefile_target_path_matches(
    db: &DbIndex,
    target_file_id: FileId,
    requested_path: &str,
    extension_was_omitted: bool,
) -> bool {
    let Some(target_path) = gmod_relative_path(db, target_file_id) else {
        return false;
    };
    target_path == requested_path
        || (extension_was_omitted && target_path.strip_suffix(".lua") == Some(requested_path))
}

fn static_compilefile_path(
    db: &DbIndex,
    file_id: FileId,
    expr: &LuaExpr,
    reassigned_decls: &HashSet<LuaDeclId>,
    visiting: &mut HashSet<LuaDeclId>,
) -> Option<String> {
    match expr {
        LuaExpr::LiteralExpr(_) => static_literal_string(expr),
        LuaExpr::ParenExpr(paren) => {
            static_compilefile_path(db, file_id, &paren.get_expr()?, reassigned_decls, visiting)
        }
        LuaExpr::NameExpr(name_expr) => {
            let decl_id = name_expr_local_decl_id(db, file_id, name_expr)?;
            if reassigned_decls.contains(&decl_id) || !visiting.insert(decl_id) {
                return None;
            }
            let (_, initializer) = local_initializer_expr(db, decl_id)?;
            let result =
                static_compilefile_path(db, file_id, &initializer, reassigned_decls, visiting);
            visiting.remove(&decl_id);
            result
        }
        LuaExpr::BinaryExpr(binary_expr)
            if binary_expr.get_op_token()?.get_op() == BinaryOperator::OpConcat =>
        {
            let (left, right) = binary_expr.get_exprs()?;
            Some(format!(
                "{}{}",
                static_compilefile_path(db, file_id, &left, reassigned_decls, visiting)?,
                static_compilefile_path(db, file_id, &right, reassigned_decls, visiting)?
            ))
        }
        _ => None,
    }
}

fn static_environment_fields(
    db: &DbIndex,
    file_id: FileId,
    expr: &LuaExpr,
    reassigned_decls: &HashSet<LuaDeclId>,
    visiting: &mut HashSet<LuaDeclId>,
) -> Option<HashSet<String>> {
    match expr {
        LuaExpr::TableExpr(table) => Some(
            table
                .get_fields()
                .filter_map(|field| match field.get_field_key()? {
                    LuaIndexKey::Name(name) => Some(name.get_name_text().to_string()),
                    LuaIndexKey::String(string) => Some(string.get_value()),
                    _ => None,
                })
                .collect(),
        ),
        LuaExpr::ParenExpr(paren) => {
            static_environment_fields(db, file_id, &paren.get_expr()?, reassigned_decls, visiting)
        }
        LuaExpr::NameExpr(name_expr) => {
            let decl_id = name_expr_local_decl_id(db, file_id, name_expr)?;
            if reassigned_decls.contains(&decl_id) || !visiting.insert(decl_id) {
                return None;
            }
            let (_, initializer) = local_initializer_expr(db, decl_id)?;
            let result =
                static_environment_fields(db, file_id, &initializer, reassigned_decls, visiting);
            visiting.remove(&decl_id);
            result
        }
        _ => None,
    }
}

fn collect_reassigned_local_decls(
    db: &DbIndex,
    file_id: FileId,
    root: &LuaChunk,
) -> HashSet<LuaDeclId> {
    root.descendants::<LuaAssignStat>()
        .flat_map(|assign| assign.get_var_and_expr_list().0)
        .filter_map(|var| match var {
            LuaVarExpr::NameExpr(name_expr) => {
                let name = name_expr.get_name_text()?;
                resolve_local_decl_id_at_position(db, file_id, &name, name_expr.get_position())
            }
            LuaVarExpr::IndexExpr(_) => None,
        })
        .collect()
}

fn rebuild_gmod_load_index(
    db: &mut DbIndex,
    branch_realm_ranges: &HashMap<FileId, Vec<GmodRealmRange>>,
    analyzed_file_ids: &[FileId],
) {
    let _p_prep = crate::profile::PhaseGuard::new("gmodload/prep");
    let file_ids = db.get_vfs().get_all_local_file_ids();
    let analyzed_file_ids: HashSet<FileId> = analyzed_file_ids.iter().copied().collect();
    let resolved_branch_ranges = file_ids
        .iter()
        .map(|file_id| {
            let ranges = if let Some(ranges) = branch_realm_ranges.get(file_id) {
                ranges.clone()
            } else if analyzed_file_ids.contains(file_id) {
                Vec::new()
            } else {
                db.get_gmod_infer_index()
                    .get_realm_file_metadata(file_id)
                    .map(|metadata| metadata.branch_realm_ranges.clone())
                    .unwrap_or_default()
            };
            (*file_id, ranges)
        })
        .collect::<HashMap<_, _>>();

    let mut file_infos = file_ids
        .iter()
        .map(|file_id| (*file_id, GmodFileLoadInfo::fallback_shared()))
        .collect::<HashMap<_, _>>();
    let mut fallback_masks = HashMap::default();

    for file_id in &file_ids {
        if let Some(realm) = infer_realm_from_load_path_hint(db, *file_id) {
            fallback_masks.insert(*file_id, GmodStateMask::from_realm(realm));
        }

        if let Some((kind, states, path_sort_key)) = engine_load_root_for_file(db, *file_id) {
            mark_load_root(&mut file_infos, *file_id, kind, states, path_sort_key);
        }
    }

    drop(_p_prep);
    let _p_sites = crate::profile::PhaseGuard::new("gmodload/resolve_sites");
    let dependencies = db.get_file_dependencies_index();
    let sites = db
        .get_vfs()
        .get_all_file_ids()
        .iter()
        .flat_map(|file_id| {
            dependencies
                .get_dependency_sites(file_id)
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    let mut dependency_sites = super::parallel::map_items_recorded(&sites, |site| {
        resolve_load_dependency_site(db, (**site).clone())
    });
    // This vector is the visit order of the load fixpoint below, so it decides both the edge
    // order and (through the `states` stamp) the edge set. Order it by a property of the
    // source text.
    dependency_sites.sort_by_cached_key(|site| load_site_sort_key(db, site));
    drop(_p_sites);
    let _p_dyn = crate::profile::PhaseGuard::new("gmodload/dynamic_loaders");
    let dynamic_loaders = resolve_dynamic_loaders(db, &file_ids);
    drop(_p_dyn);

    let _p_fix = crate::profile::PhaseGuard::new("gmodload/fixpoint");
    for _ in 0..file_ids.len().max(1) {
        let mut changed = false;
        for site in &dependency_sites {
            let source_states = source_states_for_load_site(
                &file_infos,
                &fallback_masks,
                &resolved_branch_ranges,
                site,
            );
            changed |= apply_load_site(&mut file_infos, site, source_states);
        }
        changed |= apply_dynamic_loaders(
            &mut file_infos,
            &fallback_masks,
            &resolved_branch_ranges,
            &dynamic_loaders,
        );
        if !changed {
            break;
        }
    }

    // Edges are recorded once, after convergence: a site visited before its source's realm was
    // established would otherwise record an extra edge stamped with the partial state mask.
    let mut unresolved_edges = Vec::new();
    for site in &dependency_sites {
        let source_states = source_states_for_load_site(
            &file_infos,
            &fallback_masks,
            &resolved_branch_ranges,
            site,
        );
        record_load_site_edge(&mut file_infos, &mut unresolved_edges, site, source_states);
    }
    record_dynamic_loader_edges(
        &mut file_infos,
        &fallback_masks,
        &resolved_branch_ranges,
        &dynamic_loaders,
    );
    drop(_p_fix);

    let _p_shadow = crate::profile::PhaseGuard::new("gmodload/shadows_and_publish");
    mark_main_workspace_load_shadows(db, &mut file_infos, &file_ids);

    db.get_gmod_load_index_mut()
        .set_all_file_infos(file_infos, unresolved_edges);
}

fn mark_main_workspace_load_shadows(
    db: &DbIndex,
    file_infos: &mut HashMap<FileId, GmodFileLoadInfo>,
    file_ids: &[FileId],
) {
    let module_index = db.get_module_index();
    let mut files_by_load_identity: HashMap<String, Vec<FileId>> = HashMap::default();

    for file_id in file_ids {
        let Some(info) = file_infos.get(file_id) else {
            continue;
        };
        if !is_static_load_shadow_candidate(info) {
            continue;
        }
        let Some(workspace_id) = module_index.get_workspace_id(*file_id) else {
            continue;
        };
        let workspace_kind = module_index.get_workspace_kind(workspace_id);
        if !matches!(workspace_kind, WorkspaceKind::Main | WorkspaceKind::Library) {
            continue;
        }
        let Some(load_identity) = gmod_relative_path(db, *file_id) else {
            continue;
        };
        files_by_load_identity
            .entry(load_identity)
            .or_default()
            .push(*file_id);
    }

    for files in files_by_load_identity.values() {
        let main_files = files
            .iter()
            .copied()
            .filter(|file_id| {
                module_index
                    .get_workspace_id(*file_id)
                    .is_some_and(|workspace_id| {
                        module_index.get_workspace_kind(workspace_id) == WorkspaceKind::Main
                    })
            })
            .collect::<Vec<_>>();

        if main_files.len() != 1 {
            continue;
        }
        let winning_file_id = main_files[0];
        let Some(winning_info) = file_infos.get(&winning_file_id) else {
            continue;
        };
        let winning_info = winning_info.clone();

        for file_id in files {
            if *file_id == winning_file_id {
                continue;
            }
            let Some(workspace_id) = module_index.get_workspace_id(*file_id) else {
                continue;
            };
            if module_index.get_workspace_kind(workspace_id) != WorkspaceKind::Library {
                continue;
            }
            let Some(info) = file_infos.get(file_id) else {
                continue;
            };
            if !has_matching_load_evidence(&winning_info, info) {
                continue;
            }
            if let Some(info) = file_infos.get_mut(file_id) {
                info.mark_shadowed_by(winning_file_id);
            }
        }
    }
}

fn is_static_load_shadow_candidate(info: &GmodFileLoadInfo) -> bool {
    match info.status {
        GmodLoadStatus::EngineLoaded => !info.roots.is_empty(),
        GmodLoadStatus::ReachableByLoadEdge => {
            info.confidence >= GmodLoadConfidence::Static && !info.state_mask.is_empty()
        }
        _ => false,
    }
}

fn has_matching_load_evidence(left: &GmodFileLoadInfo, right: &GmodFileLoadInfo) -> bool {
    match (left.status, right.status) {
        (GmodLoadStatus::EngineLoaded, GmodLoadStatus::EngineLoaded) => {
            has_matching_load_root(&left.roots, &right.roots)
        }
        (GmodLoadStatus::ReachableByLoadEdge, GmodLoadStatus::ReachableByLoadEdge) => {
            left.state_mask.intersects(right.state_mask)
        }
        _ => false,
    }
}

fn has_matching_load_root(left: &[GmodLoadRoot], right: &[GmodLoadRoot]) -> bool {
    left.iter().any(|left_root| {
        right.iter().any(|right_root| {
            left_root.kind == right_root.kind && left_root.states.intersects(right_root.states)
        })
    })
}

struct DynamicLoadPattern {
    source_file_id: FileId,
    result_kind: DynamicFileFindResultKind,
    glob: DynamicLoadGlob,
    dispatch: DynamicLoadDispatch,
    operations: Vec<DynamicLoadOperation>,
    range: TextRange,
    targets: Vec<(FileId, String)>,
}

struct DynamicFileFindPattern {
    range: TextRange,
    bindings: DynamicFileFindBindings,
    scope: Option<TextRange>,
    glob: DynamicLoadGlob,
}

#[derive(Clone, Default)]
struct DynamicFileFindBindings {
    files: Option<String>,
    directories: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum DynamicFileFindResultKind {
    Files,
    Directories,
}

enum DynamicFileFindLoopSource {
    Direct(TextRange),
    Binding {
        name: String,
        scope: Option<TextRange>,
    },
}

struct DynamicBindingWrite {
    name: String,
    scope: Option<TextRange>,
    range: TextRange,
}

#[derive(Clone, Hash)]
struct DynamicLoadGlob {
    base: String,
    file_prefix: Option<String>,
}

#[derive(Clone)]
struct DynamicLoadWrapper {
    params: Vec<String>,
    block: LuaBlock,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
struct DynamicLoadDispatch {
    prefix: bool,
    folder: bool,
    entrypoint: bool,
}

impl DynamicLoadDispatch {
    fn merge(&mut self, other: Self) {
        self.prefix |= other.prefix;
        self.folder |= other.folder;
        self.entrypoint |= other.entrypoint;
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum DynamicLoadOperationKind {
    Include,
    AddCSLuaFile,
}

#[derive(Clone)]
struct DynamicLoadOperation {
    kind: DynamicLoadOperationKind,
    ranges: Vec<TextRange>,
    path_hints: Vec<DynamicLoadPathHint>,
}

impl std::hash::Hash for DynamicLoadOperation {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let Self {
            kind,
            ranges,
            path_hints,
        } = self;
        kind.hash(state);
        crate::db_index::derivation::hash_local(ranges, state);
        path_hints.hash(state);
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct DynamicLoadPathHint {
    suffix_after_result: String,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct DynamicLoadAlias {
    dispatch: DynamicLoadDispatch,
    has_addcs: bool,
    has_include: bool,
    path_arg_idx: Option<usize>,
}

impl DynamicLoadAlias {
    fn from_dependency_kind(kind: LuaDependencyKind, path_arg_idx: usize) -> Option<Self> {
        match kind {
            LuaDependencyKind::Include => Some(Self {
                has_include: true,
                path_arg_idx: Some(path_arg_idx),
                ..Self::default()
            }),
            LuaDependencyKind::AddCSLuaFile => Some(Self {
                has_addcs: true,
                path_arg_idx: Some(path_arg_idx),
                ..Self::default()
            }),
            LuaDependencyKind::IncludeCS => Some(Self {
                has_addcs: true,
                has_include: true,
                path_arg_idx: Some(path_arg_idx),
                ..Self::default()
            }),
            LuaDependencyKind::Require | LuaDependencyKind::CompileFile => None,
        }
    }

    fn merge(&mut self, other: Self) {
        self.dispatch.merge(other.dispatch);
        self.has_addcs |= other.has_addcs;
        self.has_include |= other.has_include;
        if self.path_arg_idx != other.path_arg_idx {
            self.path_arg_idx = None;
        }
    }

    fn into_usage_at(
        self,
        range: TextRange,
        path_hints: Vec<DynamicLoadPathHint>,
    ) -> DynamicLoadUsage {
        let mut usage = DynamicLoadUsage {
            dispatch: self.dispatch,
            operations: Vec::new(),
        };
        if self.has_addcs {
            usage.operations.push(DynamicLoadOperation {
                kind: DynamicLoadOperationKind::AddCSLuaFile,
                ranges: vec![range],
                path_hints: path_hints.clone(),
            });
        }
        if self.has_include {
            usage.operations.push(DynamicLoadOperation {
                kind: DynamicLoadOperationKind::Include,
                ranges: vec![range],
                path_hints,
            });
        }
        usage
    }
}

#[derive(Clone, Default, Hash)]
struct DynamicLoadUsage {
    dispatch: DynamicLoadDispatch,
    operations: Vec<DynamicLoadOperation>,
}

impl DynamicLoadUsage {
    fn has_load_call(&self) -> bool {
        !self.operations.is_empty()
    }

    fn merge(&mut self, other: DynamicLoadUsage) {
        self.dispatch.merge(other.dispatch);
        self.operations.extend(other.operations);
    }

    fn add_context_range(&mut self, range: TextRange) {
        for operation in &mut self.operations {
            operation.ranges.push(range);
        }
    }
}

/// A `file.Find` result the file loads, before it is resolved against the
/// workspace's files.
struct DynamicLoadCandidate {
    result_kind: DynamicFileFindResultKind,
    glob: DynamicLoadGlob,
    usage: DynamicLoadUsage,
    range: TextRange,
}

impl std::hash::Hash for DynamicLoadCandidate {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let Self {
            result_kind,
            glob,
            usage,
            range,
        } = self;
        result_kind.hash(state);
        glob.hash(state);
        usage.hash(state);
        crate::db_index::derivation::hash_local(range, state);
    }
}

/// The dynamic loaders one file's text contains.
fn collect_file_dynamic_loaders(
    db: &DbIndex,
    source_file_id: FileId,
    annotated_global_call_roles: &AnnotatedGmodGlobalCallRoleMap,
) -> Vec<DynamicLoadCandidate> {
    let mut candidates = Vec::new();
    let Some(tree) = db.get_vfs().get_syntax_tree(&source_file_id) else {
        return candidates;
    };
    let Some(content) = db.get_vfs().get_file_content(&source_file_id) else {
        return candidates;
    };
    let annotated_candidates = annotated_global_call_roles.candidate_presence(db, source_file_id);
    if !content.contains("gmod.file_find") && !annotated_candidates.has_file_find {
        return candidates;
    }

    let root = tree.get_chunk_node();
    let annotated_call_roles =
        AnnotatedGmodCallRoleMap::build(db, source_file_id, &root, annotated_global_call_roles);
    let bindings = collect_static_string_bindings(&root);
    let wrappers = collect_dynamic_load_wrappers(&root);

    let file_find_patterns = collect_dynamic_file_find_patterns(
        db,
        source_file_id,
        &root,
        &bindings,
        &annotated_call_roles,
    );
    if file_find_patterns.is_empty() {
        return candidates;
    }

    let usages = collect_dynamic_load_usages(
        db,
        source_file_id,
        &root,
        &file_find_patterns,
        &wrappers,
        &annotated_call_roles,
    );
    for (file_find_pattern, usages) in file_find_patterns.into_iter().zip(usages) {
        for (result_kind, usage) in usages {
            if usage.has_load_call() {
                candidates.push(DynamicLoadCandidate {
                    result_kind,
                    glob: file_find_pattern.glob.clone(),
                    usage,
                    range: file_find_pattern.range,
                });
            }
        }
    }
    candidates
}

/// Every local file's dynamic loaders, resolved against the files they find.
fn resolve_dynamic_loaders(db: &DbIndex, file_ids: &[FileId]) -> Vec<DynamicLoadPattern> {
    let mut relative_paths_by_parent: HashMap<String, Vec<(FileId, String)>> = HashMap::default();
    for file_id in file_ids {
        let Some(path) = gmod_relative_path(db, *file_id) else {
            continue;
        };
        let parent = path
            .rsplit_once('/')
            .map(|(parent, _)| parent.to_string())
            .unwrap_or_default();
        relative_paths_by_parent
            .entry(parent)
            .or_default()
            .push((*file_id, path));
    }
    let local_files = file_ids.iter().copied().collect::<HashSet<_>>();

    let mut patterns = Vec::new();
    for (source_file_id, scan) in db.load_scans::<GmodFileLoadScan>() {
        if !local_files.contains(&source_file_id) {
            continue;
        }
        for candidate in &scan.dynamic_loaders {
            let targets = dynamic_file_find_targets(
                &candidate.glob,
                candidate.result_kind,
                &candidate.usage,
                &relative_paths_by_parent,
            );
            if targets.is_empty() {
                continue;
            }
            let mut dispatch = candidate.usage.dispatch;
            dispatch.prefix |= candidate
                .glob
                .file_prefix
                .as_deref()
                .is_some_and(is_realm_file_prefix);

            patterns.push(DynamicLoadPattern {
                source_file_id,
                result_kind: candidate.result_kind,
                glob: candidate.glob.clone(),
                dispatch,
                operations: candidate.usage.operations.clone(),
                range: candidate.range,
                targets,
            });
        }
    }
    patterns
}

fn collect_dynamic_file_find_patterns(
    db: &DbIndex,
    file_id: FileId,
    root: &LuaChunk,
    bindings: &HashMap<String, String>,
    annotated_roles: &AnnotatedGmodCallRoleMap,
) -> Vec<DynamicFileFindPattern> {
    let mut patterns = Vec::new();

    for call_expr in root.descendants::<LuaCallExpr>() {
        let Some(call_path) = call_expr.get_access_path() else {
            continue;
        };
        let Some((pattern_arg_idx, search_path_arg_idx)) =
            annotated_roles.file_find_call(db, file_id, &call_expr, &call_path)
        else {
            continue;
        };
        let Some(args) = call_expr.get_args_list() else {
            continue;
        };
        let args = args.get_args().collect::<Vec<_>>();
        let Some(pattern_expr) = args.get(pattern_arg_idx) else {
            continue;
        };
        if args
            .get(search_path_arg_idx)
            .and_then(static_literal_string)
            .as_deref()
            != Some("LUA")
        {
            continue;
        }
        let Some(pattern) = static_string_expr(pattern_expr, bindings) else {
            continue;
        };
        let Some(glob) = lua_file_find_glob(&pattern) else {
            continue;
        };

        patterns.push(DynamicFileFindPattern {
            range: call_expr.get_range(),
            bindings: file_find_result_bindings(&call_expr),
            scope: enclosing_closure_range(call_expr.syntax()),
            glob,
        });
    }

    patterns
}

fn collect_dynamic_load_usages(
    db: &DbIndex,
    file_id: FileId,
    root: &LuaChunk,
    file_find_patterns: &[DynamicFileFindPattern],
    wrappers: &HashMap<String, DynamicLoadWrapper>,
    annotated_roles: &AnnotatedGmodCallRoleMap,
) -> Vec<Vec<(DynamicFileFindResultKind, DynamicLoadUsage)>> {
    let inherited_aliases =
        collect_top_level_dynamic_load_call_aliases(db, file_id, root, annotated_roles);
    let binding_writes = collect_dynamic_binding_writes(root);
    let mut usages = vec![Vec::new(); file_find_patterns.len()];

    for for_range in root.descendants::<LuaForRangeStat>() {
        let Some(source) = dynamic_file_find_loop_source(db, file_id, &for_range, annotated_roles)
        else {
            continue;
        };
        let Some((pattern_idx, result_kind)) = resolve_dynamic_file_find_loop_source(
            &source,
            file_find_patterns,
            &binding_writes,
            for_range.syntax().text_range().start(),
        ) else {
            continue;
        };
        let Some(file_name_var) = for_range_file_name_var(&for_range) else {
            continue;
        };
        let Some(block) = for_range.get_block() else {
            continue;
        };

        merge_dynamic_load_usage(
            &mut usages[pattern_idx],
            result_kind,
            collect_dynamic_load_usage_in_block(
                &block,
                HashSet::from_iter([file_name_var]),
                db,
                file_id,
                &wrappers,
                &inherited_aliases,
                annotated_roles,
                0,
            ),
        );
    }

    usages
}

fn merge_dynamic_load_usage(
    usages: &mut Vec<(DynamicFileFindResultKind, DynamicLoadUsage)>,
    result_kind: DynamicFileFindResultKind,
    usage: DynamicLoadUsage,
) {
    if let Some((_, existing_usage)) = usages
        .iter_mut()
        .find(|(existing_kind, _)| *existing_kind == result_kind)
    {
        existing_usage.merge(usage);
    } else {
        usages.push((result_kind, usage));
    }
}

const DYNAMIC_LOAD_WRAPPER_DEPTH_LIMIT: usize = 4;

fn collect_dynamic_load_usage_in_block(
    block: &LuaBlock,
    initial_file_name_vars: HashSet<String>,
    db: &DbIndex,
    file_id: FileId,
    wrappers: &HashMap<String, DynamicLoadWrapper>,
    inherited_aliases: &HashMap<String, DynamicLoadAlias>,
    annotated_roles: &AnnotatedGmodCallRoleMap,
    wrapper_depth: usize,
) -> DynamicLoadUsage {
    let mut usage = DynamicLoadUsage {
        dispatch: DynamicLoadDispatch {
            prefix: block_has_dynamic_file_prefix_dispatch(block),
            folder: block_has_dynamic_file_folder_dispatch(block),
            entrypoint: block_has_dynamic_file_entrypoint_dispatch(block),
        },
        ..DynamicLoadUsage::default()
    };
    let file_name_vars = collect_dynamic_file_name_vars(block, initial_file_name_vars);
    let mut load_call_aliases = inherited_aliases.clone();
    for (alias_path, alias_usage) in
        collect_dynamic_load_call_aliases(db, file_id, block, annotated_roles)
    {
        merge_dynamic_load_alias(&mut load_call_aliases, alias_path, alias_usage);
    }

    for call_expr in block.descendants::<LuaCallExpr>() {
        let Some(path) = call_expr.get_access_path() else {
            continue;
        };

        if let Some(load_usage) = dynamic_load_usage_for_call(
            db,
            file_id,
            annotated_roles,
            &call_expr,
            &path,
            &load_call_aliases,
            &file_name_vars,
        ) {
            usage.merge(load_usage);
            continue;
        }

        if wrapper_depth < DYNAMIC_LOAD_WRAPPER_DEPTH_LIMIT {
            usage.merge(collect_dynamic_wrapper_call_usage(
                &call_expr,
                &file_name_vars,
                db,
                file_id,
                wrappers,
                inherited_aliases,
                annotated_roles,
                wrapper_depth + 1,
            ));
        }
    }

    usage
}

fn collect_dynamic_wrapper_call_usage(
    call_expr: &LuaCallExpr,
    file_name_vars: &HashSet<String>,
    db: &DbIndex,
    file_id: FileId,
    wrappers: &HashMap<String, DynamicLoadWrapper>,
    inherited_aliases: &HashMap<String, DynamicLoadAlias>,
    annotated_roles: &AnnotatedGmodCallRoleMap,
    wrapper_depth: usize,
) -> DynamicLoadUsage {
    let Some(path) = call_expr.get_access_path() else {
        return DynamicLoadUsage::default();
    };
    let Some(wrapper) = wrappers.get(path.as_str()) else {
        return DynamicLoadUsage::default();
    };
    let Some(args_list) = call_expr.get_args_list() else {
        return DynamicLoadUsage::default();
    };

    let wrapper_file_vars = args_list
        .get_args()
        .enumerate()
        .filter(|(_, arg)| expr_references_any_name(arg, file_name_vars))
        .filter_map(|(idx, _)| wrapper.params.get(idx).cloned())
        .collect::<HashSet<_>>();
    if wrapper_file_vars.is_empty() {
        return DynamicLoadUsage::default();
    }

    let mut usage = collect_dynamic_load_usage_in_block(
        &wrapper.block,
        wrapper_file_vars,
        db,
        file_id,
        wrappers,
        inherited_aliases,
        annotated_roles,
        wrapper_depth,
    );
    usage.add_context_range(call_expr.get_range());
    usage
}

fn collect_dynamic_load_wrappers(root: &LuaChunk) -> HashMap<String, DynamicLoadWrapper> {
    let mut wrappers = HashMap::default();

    for local_func_stat in root.descendants::<LuaLocalFuncStat>() {
        let Some(name) = local_func_stat
            .get_local_name()
            .and_then(|name| name.get_name_token())
            .map(|token| token.get_name_text().to_string())
        else {
            continue;
        };
        let Some(wrapper) = local_func_stat
            .get_closure()
            .and_then(dynamic_load_wrapper_from_closure)
        else {
            continue;
        };
        wrappers.insert(name, wrapper);
    }

    for func_stat in root.descendants::<LuaFuncStat>() {
        let Some(name) = func_stat
            .get_func_name()
            .and_then(|func_name| func_name.get_access_path())
        else {
            continue;
        };
        let Some(wrapper) = func_stat
            .get_closure()
            .and_then(dynamic_load_wrapper_from_closure)
        else {
            continue;
        };
        wrappers.insert(name.to_string(), wrapper);
    }

    wrappers
}

fn dynamic_load_wrapper_from_closure(closure: LuaClosureExpr) -> Option<DynamicLoadWrapper> {
    let params = closure
        .get_params_list()?
        .get_params()
        .filter(|param| !param.is_dots())
        .filter_map(|param| {
            param
                .get_name_token()
                .map(|token| token.get_name_text().to_string())
        })
        .collect::<Vec<_>>();
    let block = closure.get_block()?;
    Some(DynamicLoadWrapper { params, block })
}

fn collect_top_level_dynamic_load_call_aliases(
    db: &DbIndex,
    file_id: FileId,
    root: &LuaChunk,
    annotated_roles: &AnnotatedGmodCallRoleMap,
) -> HashMap<String, DynamicLoadAlias> {
    let Some(block) = root.get_block() else {
        return HashMap::default();
    };

    let mut aliases = HashMap::default();
    let mut changed = true;
    while changed {
        changed = false;

        for stat in block.get_stats() {
            match stat {
                LuaStat::LocalStat(local_stat) => {
                    changed |= collect_dynamic_load_aliases_from_local_stat(
                        db,
                        file_id,
                        annotated_roles,
                        &mut aliases,
                        &local_stat,
                    );
                }
                LuaStat::AssignStat(assign_stat) => {
                    changed |= collect_dynamic_load_aliases_from_assign_stat(
                        db,
                        file_id,
                        annotated_roles,
                        &mut aliases,
                        &assign_stat,
                    );
                }
                _ => {}
            }
        }
    }

    aliases
}

fn collect_dynamic_load_call_aliases(
    db: &DbIndex,
    file_id: FileId,
    block: &LuaBlock,
    annotated_roles: &AnnotatedGmodCallRoleMap,
) -> HashMap<String, DynamicLoadAlias> {
    let mut aliases = HashMap::default();
    let mut changed = true;
    while changed {
        changed = false;

        for local_stat in block.descendants::<LuaLocalStat>() {
            changed |= collect_dynamic_load_aliases_from_local_stat(
                db,
                file_id,
                annotated_roles,
                &mut aliases,
                &local_stat,
            );
        }

        for assign_stat in block.descendants::<LuaAssignStat>() {
            changed |= collect_dynamic_load_aliases_from_assign_stat(
                db,
                file_id,
                annotated_roles,
                &mut aliases,
                &assign_stat,
            );
        }
    }
    aliases
}

fn collect_dynamic_load_aliases_from_local_stat(
    db: &DbIndex,
    file_id: FileId,
    annotated_roles: &AnnotatedGmodCallRoleMap,
    aliases: &mut HashMap<String, DynamicLoadAlias>,
    local_stat: &LuaLocalStat,
) -> bool {
    let mut changed = false;
    let names = local_stat.get_local_name_list().collect::<Vec<_>>();
    let values = local_stat.get_value_exprs().collect::<Vec<_>>();
    for (idx, value) in values.iter().enumerate() {
        let Some(load_alias) =
            dynamic_load_alias_for_expr(db, file_id, annotated_roles, value, aliases)
        else {
            continue;
        };
        let Some(name) = names
            .get(idx)
            .and_then(|name| name.get_name_token())
            .map(|token| token.get_name_text().to_string())
        else {
            continue;
        };
        changed |= merge_dynamic_load_alias(aliases, name, load_alias);
    }
    changed
}

fn collect_dynamic_load_aliases_from_assign_stat(
    db: &DbIndex,
    file_id: FileId,
    annotated_roles: &AnnotatedGmodCallRoleMap,
    aliases: &mut HashMap<String, DynamicLoadAlias>,
    assign_stat: &LuaAssignStat,
) -> bool {
    let mut changed = false;
    let (vars_exprs, values) = assign_stat.get_var_and_expr_list();
    for (idx, value) in values.iter().enumerate() {
        let Some(load_alias) =
            dynamic_load_alias_for_expr(db, file_id, annotated_roles, value, aliases)
        else {
            continue;
        };
        let Some(path) = vars_exprs
            .get(idx)
            .and_then(|var_expr| var_expr.get_access_path())
        else {
            continue;
        };
        changed |= merge_dynamic_load_alias(aliases, path.to_string(), load_alias);
    }
    changed
}

fn merge_dynamic_load_alias(
    aliases: &mut HashMap<String, DynamicLoadAlias>,
    name: String,
    load_alias: DynamicLoadAlias,
) -> bool {
    match aliases.get_mut(&name) {
        Some(existing) => {
            let before = *existing;
            existing.merge(load_alias);
            before.has_addcs != existing.has_addcs
                || before.has_include != existing.has_include
                || before.dispatch != existing.dispatch
                || before.path_arg_idx != existing.path_arg_idx
        }
        None => {
            aliases.insert(name, load_alias);
            true
        }
    }
}

fn dynamic_load_alias_for_expr(
    db: &DbIndex,
    file_id: FileId,
    annotated_roles: &AnnotatedGmodCallRoleMap,
    expr: &LuaExpr,
    aliases: &HashMap<String, DynamicLoadAlias>,
) -> Option<DynamicLoadAlias> {
    match expr {
        LuaExpr::NameExpr(name_expr) => {
            let name = name_expr.get_name_text()?;
            aliases
                .get(name.as_str())
                .copied()
                .or_else(|| annotated_roles.load_alias_for_reference_expr(db, file_id, expr))
        }
        LuaExpr::IndexExpr(index_expr) => {
            let path = index_expr.get_access_path()?;
            aliases
                .get(path.as_str())
                .copied()
                .or_else(|| annotated_roles.load_alias_for_reference_expr(db, file_id, expr))
        }
        LuaExpr::ParenExpr(paren_expr) => dynamic_load_alias_for_expr(
            db,
            file_id,
            annotated_roles,
            &paren_expr.get_expr()?,
            aliases,
        ),
        LuaExpr::BinaryExpr(binary_expr) => {
            let op = binary_expr.get_op_token()?.get_op();
            if !matches!(op, BinaryOperator::OpAnd | BinaryOperator::OpOr) {
                return None;
            }

            let (left, right) = binary_expr.get_exprs()?;
            merge_optional_dynamic_load_alias(
                dynamic_load_alias_for_expr(db, file_id, annotated_roles, &left, aliases),
                dynamic_load_alias_for_expr(db, file_id, annotated_roles, &right, aliases),
            )
        }
        _ => None,
    }
}

fn merge_optional_dynamic_load_alias(
    left: Option<DynamicLoadAlias>,
    right: Option<DynamicLoadAlias>,
) -> Option<DynamicLoadAlias> {
    match (left, right) {
        (Some(mut left), Some(right)) => {
            left.merge(right);
            Some(left)
        }
        (Some(alias), None) | (None, Some(alias)) => Some(alias),
        (None, None) => None,
    }
}

fn dynamic_load_usage_for_call(
    db: &DbIndex,
    file_id: FileId,
    annotated_roles: &AnnotatedGmodCallRoleMap,
    call_expr: &LuaCallExpr,
    path: &str,
    aliases: &HashMap<String, DynamicLoadAlias>,
    file_name_vars: &HashSet<String>,
) -> Option<DynamicLoadUsage> {
    let alias = aliases
        .get(path)
        .copied()
        .or_else(|| annotated_roles.load_alias_for_call(db, file_id, call_expr, path))?;
    if !load_call_references_file_name(call_expr, file_name_vars, alias.path_arg_idx) {
        return None;
    }
    let path_hints =
        dynamic_load_path_hints_for_call(call_expr, file_name_vars, alias.path_arg_idx);
    Some(alias.into_usage_at(call_expr.get_range(), path_hints))
}

fn file_find_result_bindings(file_find_call: &LuaCallExpr) -> DynamicFileFindBindings {
    let call_range = file_find_call.get_range();

    if let Some(local_stat) = file_find_call.ancestors::<LuaLocalStat>().next() {
        let values = local_stat.get_value_exprs().collect::<Vec<_>>();
        if let Some(idx) = values
            .iter()
            .position(|expr| expr.get_range() == call_range)
        {
            let names = local_stat.get_local_name_list().collect::<Vec<_>>();
            return file_find_bindings_from_names(
                names
                    .iter()
                    .filter_map(|name| name.get_name_token())
                    .map(|token| token.get_name_text().to_string())
                    .collect::<Vec<_>>()
                    .as_slice(),
                idx,
                idx + 1 == values.len(),
            );
        }
    }

    if let Some(assign_stat) = file_find_call.ancestors::<LuaAssignStat>().next() {
        let (vars, values) = assign_stat.get_var_and_expr_list();
        if let Some(idx) = values
            .iter()
            .position(|expr| expr.get_range() == call_range)
        {
            let names = vars
                .iter()
                .map(|var_expr| match var_expr {
                    LuaVarExpr::NameExpr(name_expr) => name_expr
                        .get_name_text()
                        .map(|name| name.as_str().to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            return file_find_bindings_from_optional_names(&names, idx, idx + 1 == values.len());
        }
    }

    DynamicFileFindBindings::default()
}

fn file_find_bindings_from_names(
    names: &[String],
    value_idx: usize,
    value_can_return_multiple: bool,
) -> DynamicFileFindBindings {
    let optional_names = names
        .iter()
        .map(|name| Some(name.clone()))
        .collect::<Vec<_>>();
    file_find_bindings_from_optional_names(&optional_names, value_idx, value_can_return_multiple)
}

fn file_find_bindings_from_optional_names(
    names: &[Option<String>],
    value_idx: usize,
    value_can_return_multiple: bool,
) -> DynamicFileFindBindings {
    DynamicFileFindBindings {
        files: names.get(value_idx).and_then(binding_name),
        directories: value_can_return_multiple
            .then(|| names.get(value_idx + 1).and_then(binding_name))
            .flatten(),
    }
}

fn binding_name(name: &Option<String>) -> Option<String> {
    name.as_ref().filter(|name| name.as_str() != "_").cloned()
}

fn enclosing_closure_range(node: &LuaSyntaxNode) -> Option<TextRange> {
    node.ancestors()
        .skip(1)
        .find_map(LuaClosureExpr::cast)
        .map(|closure| closure.syntax().text_range())
}

fn for_range_file_name_var(for_range: &LuaForRangeStat) -> Option<String> {
    for_range
        .get_var_name_list()
        .filter_map(|name| {
            let name = name.get_name_text().to_string();
            (name != "_").then_some(name)
        })
        .last()
}

fn dynamic_file_find_loop_source(
    db: &DbIndex,
    file_id: FileId,
    for_range: &LuaForRangeStat,
    annotated_roles: &AnnotatedGmodCallRoleMap,
) -> Option<DynamicFileFindLoopSource> {
    let exprs = for_range.get_expr_list().collect::<Vec<_>>();
    if let Some(range) = exprs
        .iter()
        .find_map(|expr| file_find_call_range_in_iterator_expr(db, file_id, annotated_roles, expr))
    {
        return Some(DynamicFileFindLoopSource::Direct(range));
    }

    exprs
        .iter()
        .find_map(file_find_binding_reference_in_iterator_expr)
        .map(|name| DynamicFileFindLoopSource::Binding {
            name,
            scope: enclosing_closure_range(for_range.syntax()),
        })
}

fn file_find_call_range_in_iterator_expr(
    db: &DbIndex,
    file_id: FileId,
    annotated_roles: &AnnotatedGmodCallRoleMap,
    expr: &LuaExpr,
) -> Option<TextRange> {
    match expr {
        LuaExpr::CallExpr(call_expr) => {
            let path = call_expr.get_access_path()?;
            if annotated_roles
                .file_find_call(db, file_id, call_expr, &path)
                .is_some()
            {
                return Some(call_expr.get_range());
            }
            if !file_find_result_iterator_path(&path) {
                return None;
            }
            call_expr.get_args_list()?.get_args().find_map(|arg| {
                file_find_call_range_in_iterator_expr(db, file_id, annotated_roles, &arg)
            })
        }
        LuaExpr::ParenExpr(paren_expr) => file_find_call_range_in_iterator_expr(
            db,
            file_id,
            annotated_roles,
            &paren_expr.get_expr()?,
        ),
        _ => None,
    }
}

fn file_find_binding_reference_in_iterator_expr(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::NameExpr(name_expr) => Some(name_expr.get_name_text()?.as_str().to_string()),
        LuaExpr::CallExpr(call_expr) => {
            let path = call_expr.get_access_path()?;
            if !file_find_result_iterator_path(&path) {
                return None;
            }
            call_expr
                .get_args_list()?
                .get_args()
                .find_map(|arg| file_find_binding_reference_in_iterator_expr(&arg))
        }
        LuaExpr::ParenExpr(paren_expr) => {
            file_find_binding_reference_in_iterator_expr(&paren_expr.get_expr()?)
        }
        _ => None,
    }
}

fn resolve_dynamic_file_find_loop_source(
    source: &DynamicFileFindLoopSource,
    file_find_patterns: &[DynamicFileFindPattern],
    binding_writes: &[DynamicBindingWrite],
    loop_start: TextSize,
) -> Option<(usize, DynamicFileFindResultKind)> {
    match source {
        DynamicFileFindLoopSource::Direct(range) => file_find_patterns
            .iter()
            .position(|pattern| pattern.range == *range)
            .map(|idx| (idx, DynamicFileFindResultKind::Files)),
        DynamicFileFindLoopSource::Binding { name, scope } => file_find_patterns
            .iter()
            .enumerate()
            .rev()
            .find_map(|(idx, pattern)| {
                if pattern.scope != *scope
                    || pattern.range.end() >= loop_start
                    || binding_written_between(
                        binding_writes,
                        name,
                        pattern.range.end(),
                        loop_start,
                        *scope,
                    )
                {
                    return None;
                }
                if pattern.bindings.files.as_deref() == Some(name.as_str()) {
                    return Some((idx, DynamicFileFindResultKind::Files));
                }
                if pattern.bindings.directories.as_deref() == Some(name.as_str()) {
                    return Some((idx, DynamicFileFindResultKind::Directories));
                }
                None
            }),
    }
}

fn collect_dynamic_binding_writes(root: &LuaChunk) -> Vec<DynamicBindingWrite> {
    let mut writes = Vec::new();

    for local_stat in root.descendants::<LuaLocalStat>() {
        let range = local_stat.syntax().text_range();
        let scope = enclosing_closure_range(local_stat.syntax());
        for local_name in local_stat.get_local_name_list() {
            let Some(name_token) = local_name.get_name_token() else {
                continue;
            };
            writes.push(DynamicBindingWrite {
                name: name_token.get_name_text().to_string(),
                scope,
                range,
            });
        }
    }

    for assign_stat in root.descendants::<LuaAssignStat>() {
        let range = assign_stat.syntax().text_range();
        let scope = enclosing_closure_range(assign_stat.syntax());
        let (vars_exprs, _) = assign_stat.get_var_and_expr_list();
        for var_expr in vars_exprs {
            let Some(path) = var_expr.get_access_path() else {
                continue;
            };
            writes.push(DynamicBindingWrite {
                name: path.to_string(),
                scope,
                range,
            });
        }
    }

    writes
}

fn binding_written_between(
    writes: &[DynamicBindingWrite],
    binding_name: &str,
    start: TextSize,
    end: TextSize,
    scope: Option<TextRange>,
) -> bool {
    writes.iter().any(|write| {
        write.name == binding_name
            && write.scope == scope
            && write.range.start() > start
            && write.range.start() < end
    })
}

fn block_has_dynamic_file_prefix_dispatch(block: &LuaBlock) -> bool {
    let text = block.syntax().text().to_string();
    text.contains("\"cl_\"")
        || text.contains("'cl_'")
        || text.contains("\"sv_\"")
        || text.contains("'sv_'")
        || text.contains("\"sh_\"")
        || text.contains("'sh_'")
}

fn block_has_dynamic_file_folder_dispatch(block: &LuaBlock) -> bool {
    let text = block.syntax().text().to_string();
    text.contains("\"client\"")
        || text.contains("'client'")
        || text.contains("\"server\"")
        || text.contains("'server'")
        || text.contains("\"shared\"")
        || text.contains("'shared'")
}

fn block_has_dynamic_file_entrypoint_dispatch(block: &LuaBlock) -> bool {
    let text = block.syntax().text().to_string();
    text.contains("\"cl_init.lua\"")
        || text.contains("'cl_init.lua'")
        || text.contains("\"init.lua\"")
        || text.contains("'init.lua'")
        || text.contains("\"shared.lua\"")
        || text.contains("'shared.lua'")
}

fn file_find_result_iterator_path(path: &str) -> bool {
    matches!(
        path,
        "ipairs" | "pairs" | "SortedPairs" | "SortedPairsByMemberValue" | "SortedPairsByValue"
    )
}

fn collect_dynamic_file_name_vars(
    block: &LuaBlock,
    initial_file_name_vars: HashSet<String>,
) -> HashSet<String> {
    let mut vars = initial_file_name_vars;
    let mut changed = true;
    while changed {
        changed = false;

        for local_stat in block.descendants::<LuaLocalStat>() {
            let names = local_stat.get_local_name_list().collect::<Vec<_>>();
            let values = local_stat.get_value_exprs().collect::<Vec<_>>();
            for (idx, value) in values.iter().enumerate() {
                if !expr_references_any_name(value, &vars) {
                    continue;
                }
                let Some(name) = names
                    .get(idx)
                    .and_then(|name| name.get_name_token())
                    .map(|token| token.get_name_text().to_string())
                else {
                    continue;
                };
                changed |= vars.insert(name);
            }
        }

        for assign_stat in block.descendants::<LuaAssignStat>() {
            let (vars_exprs, values) = assign_stat.get_var_and_expr_list();
            for (idx, value) in values.iter().enumerate() {
                if !expr_references_any_name(value, &vars) {
                    continue;
                }
                let Some(LuaVarExpr::NameExpr(name_expr)) = vars_exprs.get(idx) else {
                    continue;
                };
                if let Some(name) = name_expr.get_name_text() {
                    changed |= vars.insert(name.as_str().to_string());
                }
            }
        }
    }
    vars
}

fn load_call_references_file_name(
    call_expr: &LuaCallExpr,
    file_name_vars: &HashSet<String>,
    path_arg_idx: Option<usize>,
) -> bool {
    dynamic_load_path_arg_exprs(call_expr, path_arg_idx)
        .iter()
        .any(|arg| expr_references_any_name(arg, file_name_vars))
}

fn dynamic_load_path_hints_for_call(
    call_expr: &LuaCallExpr,
    file_name_vars: &HashSet<String>,
    path_arg_idx: Option<usize>,
) -> Vec<DynamicLoadPathHint> {
    dynamic_load_path_arg_exprs(call_expr, path_arg_idx)
        .iter()
        .filter_map(|arg| dynamic_load_path_hint_for_expr(&arg, file_name_vars))
        .collect()
}

fn dynamic_load_path_arg_exprs(
    call_expr: &LuaCallExpr,
    path_arg_idx: Option<usize>,
) -> Vec<LuaExpr> {
    let Some(args) = call_expr.get_args_list() else {
        return Vec::new();
    };
    let args = args.get_args().collect::<Vec<_>>();
    if let Some(path_arg_idx) = path_arg_idx {
        args.get(path_arg_idx).cloned().into_iter().collect()
    } else {
        args
    }
}

enum DynamicPathPart {
    Static(String),
    ResultName,
    Other,
}

fn dynamic_load_path_hint_for_expr(
    expr: &LuaExpr,
    file_name_vars: &HashSet<String>,
) -> Option<DynamicLoadPathHint> {
    if let LuaExpr::NameExpr(name_expr) = expr
        && name_expr
            .get_name_text()
            .is_some_and(|name| file_name_vars.contains(name.as_str()))
    {
        return Some(DynamicLoadPathHint {
            suffix_after_result: String::new(),
        });
    }

    let mut parts = Vec::new();
    flatten_dynamic_path_expr(expr, file_name_vars, &mut parts)?;
    let result_idx = parts
        .iter()
        .position(|part| matches!(part, DynamicPathPart::ResultName))?;

    let mut suffix = String::new();
    for part in &parts[result_idx + 1..] {
        match part {
            DynamicPathPart::Static(value) => suffix.push_str(value),
            DynamicPathPart::ResultName | DynamicPathPart::Other => return None,
        }
    }

    Some(DynamicLoadPathHint {
        suffix_after_result: normalize_dynamic_path_suffix(&suffix),
    })
}

fn flatten_dynamic_path_expr(
    expr: &LuaExpr,
    file_name_vars: &HashSet<String>,
    parts: &mut Vec<DynamicPathPart>,
) -> Option<()> {
    match expr {
        LuaExpr::LiteralExpr(_) => {
            parts.push(DynamicPathPart::Static(static_literal_string(expr)?));
        }
        LuaExpr::NameExpr(name_expr) => {
            let Some(name) = name_expr.get_name_text() else {
                parts.push(DynamicPathPart::Other);
                return Some(());
            };
            if file_name_vars.contains(name.as_str()) {
                parts.push(DynamicPathPart::ResultName);
            } else {
                parts.push(DynamicPathPart::Other);
            }
        }
        LuaExpr::ParenExpr(paren_expr) => {
            flatten_dynamic_path_expr(&paren_expr.get_expr()?, file_name_vars, parts)?;
        }
        LuaExpr::BinaryExpr(binary_expr) => {
            if binary_expr.get_op_token()?.get_op() != BinaryOperator::OpConcat {
                parts.push(DynamicPathPart::Other);
                return Some(());
            }
            let (left, right) = binary_expr.get_exprs()?;
            flatten_dynamic_path_expr(&left, file_name_vars, parts)?;
            flatten_dynamic_path_expr(&right, file_name_vars, parts)?;
        }
        _ => parts.push(DynamicPathPart::Other),
    }
    Some(())
}

fn normalize_dynamic_path_suffix(suffix: &str) -> String {
    let suffix = suffix.replace('\\', "/").to_ascii_lowercase();
    if suffix.is_empty() || suffix.starts_with('/') {
        suffix
    } else {
        format!("/{suffix}")
    }
}

fn expr_references_any_name(expr: &LuaExpr, expected_names: &HashSet<String>) -> bool {
    expected_names
        .iter()
        .any(|expected_name| expr_references_name(expr, expected_name))
}

fn expr_references_name(expr: &LuaExpr, expected_name: &str) -> bool {
    if let LuaExpr::NameExpr(name_expr) = expr
        && name_expr
            .get_name_text()
            .is_some_and(|name| name.as_str() == expected_name)
    {
        return true;
    }

    expr.descendants::<LuaNameExpr>().any(|name_expr| {
        name_expr
            .get_name_text()
            .is_some_and(|name| name.as_str() == expected_name)
    })
}

fn collect_static_string_bindings(root: &LuaChunk) -> HashMap<String, String> {
    let mut bindings = HashMap::default();
    for node in root.syntax().descendants() {
        if let Some(local_stat) = LuaLocalStat::cast(node.clone()) {
            let names = local_stat.get_local_name_list().collect::<Vec<_>>();
            let values = local_stat.get_value_exprs().collect::<Vec<_>>();
            for (idx, local_name) in names.iter().enumerate() {
                let Some(name_token) = local_name.get_name_token() else {
                    continue;
                };
                let Some(value) = values.get(idx) else {
                    continue;
                };
                if let Some(value) = static_string_expr(value, &bindings) {
                    bindings.insert(name_token.get_name_text().to_string(), value);
                }
            }
            continue;
        }

        if let Some(assign_stat) = LuaAssignStat::cast(node) {
            let (vars, values) = assign_stat.get_var_and_expr_list();
            for (idx, var) in vars.iter().enumerate() {
                let LuaVarExpr::NameExpr(name_expr) = var else {
                    continue;
                };
                let Some(name) = name_expr.get_name_text() else {
                    continue;
                };
                let Some(value) = values.get(idx) else {
                    continue;
                };
                if let Some(value) = static_string_expr(value, &bindings) {
                    bindings.insert(name.to_string(), value);
                }
            }
        }
    }
    bindings
}

/// The string `expr` holds, read from the file's text alone: a string literal,
/// a field of a table assigned one before `call_expr`, or a concatenation of
/// those.
pub(crate) fn static_call_arg_string(
    db: &DbIndex,
    file_id: FileId,
    call_expr: &LuaCallExpr,
    expr: &LuaExpr,
) -> Option<String> {
    static_string_value(db, file_id, call_expr, expr, &mut Vec::new())
}

fn static_string_value(
    db: &DbIndex,
    file_id: FileId,
    call_expr: &LuaCallExpr,
    expr: &LuaExpr,
    visiting: &mut Vec<LuaSyntaxId>,
) -> Option<String> {
    match expr {
        LuaExpr::LiteralExpr(_) => static_literal_string(expr),
        LuaExpr::ParenExpr(paren_expr) => {
            static_string_value(db, file_id, call_expr, &paren_expr.get_expr()?, visiting)
        }
        LuaExpr::BinaryExpr(binary_expr)
            if binary_expr.get_op_token()?.get_op() == BinaryOperator::OpConcat =>
        {
            let (left, right) = binary_expr.get_exprs()?;
            Some(format!(
                "{}{}",
                static_string_value(db, file_id, call_expr, &left, visiting)?,
                static_string_value(db, file_id, call_expr, &right, visiting)?
            ))
        }
        LuaExpr::IndexExpr(index_expr) => {
            let syntax_id = index_expr.get_syntax_id();
            if visiting.contains(&syntax_id) {
                return None;
            }
            let (root, field_path) = index_expr_root_and_field_path(index_expr)?;
            let value = resolve_static_field_path_expr(db, file_id, call_expr, root, &field_path)?;
            visiting.push(syntax_id);
            let string = static_string_value(db, file_id, call_expr, &value, visiting);
            visiting.pop();
            string
        }
        _ => None,
    }
}

/// The name an index chain starts at and the keys it reads from there.
fn index_expr_root_and_field_path(
    index_expr: &glua_parser::LuaIndexExpr,
) -> Option<(LuaExpr, Vec<String>)> {
    let mut field_path = Vec::new();
    let mut current = index_expr.clone();
    loop {
        field_path.push(match current.get_index_key()? {
            LuaIndexKey::Name(name) => name.get_name_text().to_string(),
            LuaIndexKey::String(string) => string.get_value(),
            _ => return None,
        });
        match current.get_prefix_expr()? {
            LuaExpr::IndexExpr(prefix) => current = prefix,
            root @ LuaExpr::NameExpr(_) => {
                field_path.reverse();
                return Some((root, field_path));
            }
            _ => return None,
        }
    }
}

fn static_literal_string(expr: &LuaExpr) -> Option<String> {
    let LuaExpr::LiteralExpr(literal) = expr else {
        return None;
    };
    let LuaLiteralToken::String(string) = literal.get_literal()? else {
        return None;
    };
    Some(string.get_value())
}

fn static_string_expr(expr: &LuaExpr, bindings: &HashMap<String, String>) -> Option<String> {
    match expr {
        LuaExpr::LiteralExpr(_) => static_literal_string(expr),
        LuaExpr::NameExpr(name_expr) => bindings.get(name_expr.get_name_text()?.as_str()).cloned(),
        LuaExpr::ParenExpr(paren_expr) => static_string_expr(&paren_expr.get_expr()?, bindings),
        LuaExpr::BinaryExpr(binary_expr) => {
            if binary_expr.get_op_token()?.get_op() != BinaryOperator::OpConcat {
                return None;
            }
            let (left, right) = binary_expr.get_exprs()?;
            let left = static_string_expr(&left, bindings)?;
            let right = static_string_expr(&right, bindings)?;
            Some(format!("{left}{right}"))
        }
        _ => None,
    }
}

fn lua_file_find_glob(pattern: &str) -> Option<DynamicLoadGlob> {
    let normalized = normalize_dynamic_lua_path(pattern);
    let (base, file_pattern) = normalized.rsplit_once('/')?;
    let file_prefix = if file_pattern == "*.lua" || file_pattern == "*" {
        None
    } else {
        Some(
            file_pattern
                .strip_suffix("*.lua")
                .filter(|prefix| !prefix.is_empty())?
                .to_string(),
        )
    };

    Some(DynamicLoadGlob {
        base: base.trim_matches('/').to_string(),
        file_prefix,
    })
}

fn normalize_dynamic_lua_path(path: &str) -> String {
    path.replace('\\', "/")
        .to_ascii_lowercase()
        .trim_start_matches("lua/")
        .trim_matches('/')
        .to_string()
}

fn dynamic_file_find_targets(
    glob: &DynamicLoadGlob,
    result_kind: DynamicFileFindResultKind,
    usage: &DynamicLoadUsage,
    relative_paths_by_parent: &HashMap<String, Vec<(FileId, String)>>,
) -> Vec<(FileId, String)> {
    // `targets` is consumed in order by `apply_dynamic_loaders`, which feeds the
    // load-graph fixpoint, so the order has to be a property of the source. The
    // directory branch walks a `HashSet` of suffixes and a `HashMap` keyed by
    // parent path — doubly hash-random per process. Same policy as
    // `build_call_roles_and_registry`: order by normalized path, then file id.
    let mut targets =
        dynamic_file_find_targets_unordered(glob, result_kind, usage, relative_paths_by_parent);
    targets.sort_by_cached_key(|(file_id, target_path)| {
        (
            crate::vfs::normalize_path_for_ordering(target_path),
            file_id.id,
        )
    });
    targets
}

fn dynamic_file_find_targets_unordered(
    glob: &DynamicLoadGlob,
    result_kind: DynamicFileFindResultKind,
    usage: &DynamicLoadUsage,
    relative_paths_by_parent: &HashMap<String, Vec<(FileId, String)>>,
) -> Vec<(FileId, String)> {
    match result_kind {
        DynamicFileFindResultKind::Files => relative_paths_by_parent
            .get(&glob.base)
            .into_iter()
            .flat_map(|candidate_paths| candidate_paths.iter())
            .filter(|(_, target_path)| file_find_file_glob_matches(glob, target_path))
            .map(|(target_file_id, target_path)| (*target_file_id, target_path.clone()))
            .collect(),
        DynamicFileFindResultKind::Directories => {
            let suffixes = usage
                .operations
                .iter()
                .flat_map(|operation| operation.path_hints.iter())
                .map(|hint| hint.suffix_after_result.as_str())
                .filter(|suffix| !suffix.is_empty())
                .collect::<HashSet<_>>();
            if suffixes.is_empty() {
                return Vec::new();
            }

            suffixes
                .into_iter()
                .flat_map(|suffix| {
                    file_find_directory_targets(glob, suffix, relative_paths_by_parent)
                })
                .collect()
        }
    }
}

fn file_find_file_glob_matches(glob: &DynamicLoadGlob, target_path: &str) -> bool {
    let rest = if glob.base.is_empty() {
        target_path
    } else {
        let Some(rest) = target_path.strip_prefix(&glob.base) else {
            return false;
        };
        let Some(rest) = rest.strip_prefix('/') else {
            return false;
        };
        rest
    };
    if rest.contains('/') || !rest.ends_with(".lua") {
        return false;
    }
    glob.file_prefix
        .as_deref()
        .is_none_or(|prefix| rest.starts_with(prefix))
}

fn file_find_directory_glob_matches(
    glob: &DynamicLoadGlob,
    target_path: &str,
    suffix_after_result: &str,
) -> bool {
    let rest = if glob.base.is_empty() {
        target_path
    } else {
        let Some(rest) = target_path.strip_prefix(&glob.base) else {
            return false;
        };
        let Some(rest) = rest.strip_prefix('/') else {
            return false;
        };
        rest
    };
    let Some((directory_name, path_inside_directory)) = rest.split_once('/') else {
        return false;
    };
    if directory_name.is_empty() || path_inside_directory.is_empty() {
        return false;
    }
    glob.file_prefix
        .as_deref()
        .is_none_or(|prefix| directory_name.starts_with(prefix))
        && suffix_after_result.trim_start_matches('/') == path_inside_directory
}

fn file_find_directory_targets(
    glob: &DynamicLoadGlob,
    suffix_after_result: &str,
    relative_paths_by_parent: &HashMap<String, Vec<(FileId, String)>>,
) -> Vec<(FileId, String)> {
    let suffix = suffix_after_result.trim_start_matches('/');
    let (suffix_parent, suffix_file_name) = suffix.rsplit_once('/').unwrap_or(("", suffix));
    if suffix_file_name.is_empty() {
        return Vec::new();
    }

    relative_paths_by_parent
        .iter()
        .filter(|(parent, _)| file_find_directory_parent_matches(glob, parent, suffix_parent))
        .flat_map(|(_, candidates)| candidates.iter())
        .filter(|(_, target_path)| {
            target_path
                .rsplit_once('/')
                .map(|(_, file_name)| file_name == suffix_file_name)
                .unwrap_or(false)
        })
        .map(|(target_file_id, target_path)| (*target_file_id, target_path.clone()))
        .collect()
}

fn file_find_directory_parent_matches(
    glob: &DynamicLoadGlob,
    parent: &str,
    suffix_parent: &str,
) -> bool {
    let rest = if glob.base.is_empty() {
        parent
    } else {
        let Some(rest) = parent.strip_prefix(&glob.base) else {
            return false;
        };
        let Some(rest) = rest.strip_prefix('/') else {
            return false;
        };
        rest
    };

    let (directory_name, path_inside_directory) = rest.split_once('/').unwrap_or((rest, ""));
    if directory_name.is_empty() {
        return false;
    }
    glob.file_prefix
        .as_deref()
        .is_none_or(|prefix| directory_name.starts_with(prefix))
        && path_inside_directory == suffix_parent
}

fn apply_dynamic_loaders(
    file_infos: &mut HashMap<FileId, GmodFileLoadInfo>,
    fallback_masks: &HashMap<FileId, GmodStateMask>,
    branch_realm_ranges: &HashMap<FileId, Vec<GmodRealmRange>>,
    dynamic_loaders: &[DynamicLoadPattern],
) -> bool {
    let mut changed = false;
    for loader in dynamic_loaders {
        for (target_file_id, _, operation, target_states) in
            dynamic_loader_reaches(file_infos, fallback_masks, branch_realm_ranges, loader)
        {
            let target_info = file_infos
                .entry(target_file_id)
                .or_insert_with(GmodFileLoadInfo::fallback_shared);
            if operation.kind == DynamicLoadOperationKind::AddCSLuaFile {
                target_info.client_send_available = true;
            }
            changed |= target_info.mark_states(
                target_states,
                GmodLoadStatus::MaybeDynamic,
                GmodLoadConfidence::Dynamic,
            );
        }
    }
    changed
}

/// Records the edge of each target the dynamic loaders reach, with the states
/// their sources settled on.
fn record_dynamic_loader_edges(
    file_infos: &mut HashMap<FileId, GmodFileLoadInfo>,
    fallback_masks: &HashMap<FileId, GmodStateMask>,
    branch_realm_ranges: &HashMap<FileId, Vec<GmodRealmRange>>,
    dynamic_loaders: &[DynamicLoadPattern],
) {
    for loader in dynamic_loaders {
        for (target_file_id, target_path, operation, target_states) in
            dynamic_loader_reaches(file_infos, fallback_masks, branch_realm_ranges, loader)
        {
            file_infos
                .entry(target_file_id)
                .or_insert_with(GmodFileLoadInfo::fallback_shared)
                .add_incoming_edge(GmodLoadEdge {
                    source_file_id: loader.source_file_id,
                    target_file_id: Some(target_file_id),
                    kind: dynamic_load_edge_kind(operation.kind),
                    states: target_states,
                    path: Some(target_path.clone()),
                    original_expr: Some("gmod.file_find".to_string()),
                    range: operation.ranges.first().copied().or(Some(loader.range)),
                });
        }
    }
}

/// Each target `loader` reaches, with its path, the operation that reaches it
/// and the states it is loaded in, from its source's states as `file_infos`
/// holds them.
fn dynamic_loader_reaches<'a>(
    file_infos: &HashMap<FileId, GmodFileLoadInfo>,
    fallback_masks: &HashMap<FileId, GmodStateMask>,
    branch_realm_ranges: &HashMap<FileId, Vec<GmodRealmRange>>,
    loader: &'a DynamicLoadPattern,
) -> Vec<(FileId, &'a String, &'a DynamicLoadOperation, GmodStateMask)> {
    let operation_source_states = loader
        .operations
        .iter()
        .filter_map(|operation| {
            let source_states = source_states_for_dynamic_operation(
                file_infos,
                fallback_masks,
                branch_realm_ranges,
                loader.source_file_id,
                &operation.ranges,
            );
            (!source_states.is_empty()).then_some((operation, source_states))
        })
        .collect::<Vec<_>>();
    let mut reaches = Vec::new();
    for (target_file_id, target_path) in &loader.targets {
        for (operation, source_states) in &operation_source_states {
            if !dynamic_operation_matches_target(target_path, loader, operation) {
                continue;
            }
            let target_states =
                dynamic_operation_target_states(target_path, *source_states, loader, operation);
            if !target_states.is_empty() {
                reaches.push((*target_file_id, target_path, *operation, target_states));
            }
        }
    }
    reaches
}

fn dynamic_operation_matches_target(
    target_path: &str,
    loader: &DynamicLoadPattern,
    operation: &DynamicLoadOperation,
) -> bool {
    match loader.result_kind {
        DynamicFileFindResultKind::Files => true,
        DynamicFileFindResultKind::Directories => operation.path_hints.iter().any(|hint| {
            !hint.suffix_after_result.is_empty()
                && file_find_directory_glob_matches(
                    &loader.glob,
                    target_path,
                    &hint.suffix_after_result,
                )
        }),
    }
}

fn source_states_for_dynamic_operation(
    file_infos: &HashMap<FileId, GmodFileLoadInfo>,
    fallback_masks: &HashMap<FileId, GmodStateMask>,
    branch_realm_ranges: &HashMap<FileId, Vec<GmodRealmRange>>,
    source_file_id: FileId,
    ranges: &[TextRange],
) -> GmodStateMask {
    let mut source_states = file_infos
        .get(&source_file_id)
        .map(|info| info.state_mask)
        .filter(|states| !states.is_empty())
        .or_else(|| fallback_masks.get(&source_file_id).copied())
        .unwrap_or_else(GmodStateMask::empty);

    let Some(branch_ranges) = branch_realm_ranges.get(&source_file_id) else {
        return source_states;
    };

    for range in ranges {
        let Some(branch_realm) = branch_ranges
            .iter()
            .find(|branch_range| branch_range.range.contains(range.start()))
            .map(|branch_range| branch_range.realm)
        else {
            continue;
        };
        let branch_states = GmodStateMask::from_realm(branch_realm);
        source_states = if source_states.is_empty() {
            branch_states
        } else {
            source_states.intersection(branch_states)
        };
    }

    source_states
}

fn dynamic_operation_target_states(
    target_path: &str,
    source_states: GmodStateMask,
    loader: &DynamicLoadPattern,
    operation: &DynamicLoadOperation,
) -> GmodStateMask {
    let Some(dispatch_states) = dynamic_target_dispatch_states(target_path, loader) else {
        return match operation.kind {
            DynamicLoadOperationKind::Include => source_states,
            DynamicLoadOperationKind::AddCSLuaFile => GmodStateMask::CLIENT,
        };
    };

    match operation.kind {
        DynamicLoadOperationKind::Include => dispatch_states.intersection(source_states),
        DynamicLoadOperationKind::AddCSLuaFile => {
            if dispatch_states.intersects(GmodStateMask::CLIENT) {
                GmodStateMask::CLIENT
            } else {
                GmodStateMask::empty()
            }
        }
    }
}

fn dynamic_load_edge_kind(kind: DynamicLoadOperationKind) -> GmodLoadEdgeKind {
    match kind {
        DynamicLoadOperationKind::Include => GmodLoadEdgeKind::DynamicInclude,
        DynamicLoadOperationKind::AddCSLuaFile => GmodLoadEdgeKind::DynamicAddCSLuaFile,
    }
}

fn dynamic_target_dispatch_states(
    target_path: &str,
    loader: &DynamicLoadPattern,
) -> Option<GmodStateMask> {
    let dispatch = loader.dispatch;
    let file_name = target_path.rsplit('/').next().unwrap_or(target_path);

    if dispatch.prefix {
        if file_name.starts_with("cl_") {
            return Some(GmodStateMask::CLIENT);
        }
        if file_name.starts_with("sv_") {
            return Some(GmodStateMask::SERVER);
        }
        if file_name.starts_with("sh_") {
            return Some(GmodStateMask::SHARED);
        }
    }

    if dispatch.folder {
        if let Some(states) = dynamic_target_folder_dispatch_states(target_path, loader) {
            return Some(states);
        }
    }

    if dispatch.entrypoint {
        match file_name {
            "cl_init.lua" => return Some(GmodStateMask::CLIENT),
            "init.lua" => return Some(GmodStateMask::SERVER),
            "shared.lua" => return Some(GmodStateMask::SHARED),
            _ => {}
        }
    }

    None
}

fn dynamic_target_folder_dispatch_states(
    target_path: &str,
    loader: &DynamicLoadPattern,
) -> Option<GmodStateMask> {
    if loader.result_kind == DynamicFileFindResultKind::Files {
        if let Some(states) = loader
            .glob
            .base
            .rsplit('/')
            .next()
            .and_then(realm_folder_states)
        {
            return Some(states);
        }
    }

    let glob = &loader.glob;
    let rest = if glob.base.is_empty() {
        target_path
    } else {
        let rest = target_path.strip_prefix(&glob.base)?;
        rest.strip_prefix('/')?
    };

    realm_folder_states(rest.split('/').next()?)
}

fn realm_folder_states(segment: &str) -> Option<GmodStateMask> {
    match segment {
        "client" | "cl" => Some(GmodStateMask::CLIENT),
        "server" | "sv" => Some(GmodStateMask::SERVER),
        "shared" | "sh" => Some(GmodStateMask::SHARED),
        _ => None,
    }
}

fn is_realm_file_prefix(prefix: &str) -> bool {
    matches!(prefix, "cl_" | "sv_" | "sh_")
}

fn resolve_load_dependency_site(db: &DbIndex, mut site: LuaDependencySite) -> LuaDependencySite {
    let Some(path) = site.path.as_deref() else {
        return site;
    };
    site.target_file_id = resolve_load_dependency_target(db, site.source_file_id, site.kind, path);
    site
}

/// The file a `require`, `include`, `AddCSLuaFile`, `CompileFile` or
/// `IncludeCS` of `dependency_path` in `source_file_id` loads.
pub(crate) fn resolve_load_dependency_target(
    db: &DbIndex,
    source_file_id: FileId,
    dependency_kind: LuaDependencyKind,
    dependency_path: &str,
) -> Option<FileId> {
    let module_index = db.get_module_index();
    match dependency_kind {
        LuaDependencyKind::Require => {
            module_index.find_module_file_for_file(dependency_path, source_file_id)
        }
        LuaDependencyKind::Include
        | LuaDependencyKind::CompileFile
        | LuaDependencyKind::AddCSLuaFile
        | LuaDependencyKind::IncludeCS => {
            resolve_load_include_target(db, source_file_id, dependency_path)
                .or_else(|| module_index.find_module_file_for_file(dependency_path, source_file_id))
        }
    }
}

/// Garry's Mod looks a loaded path up next to the including file first, then
/// under the `lua/` roots.
fn resolve_load_include_target(
    db: &DbIndex,
    source_file_id: FileId,
    dependency_path: &str,
) -> Option<FileId> {
    let normalized_path = dependency_path.replace('\\', "/");
    let normalized_path = normalized_path.trim_start_matches("./");
    let normalized_no_ext = normalized_path
        .strip_suffix(".lua")
        .unwrap_or(normalized_path);

    let module_index = db.get_module_index();
    if let Some(module_file_id) = db
        .get_vfs()
        .get_file_path(&source_file_id)
        .and_then(|path| path.parent())
        .and_then(|parent_dir| {
            module_index.find_module_file_by_path_for_file(
                &parent_dir.join(Path::new(normalized_path)),
                source_file_id,
            )
        })
    {
        return Some(module_file_id);
    }

    let root_module_path = normalized_no_ext.replace('/', ".");
    if let Some(module_file_id) =
        module_index.find_module_file_for_file(&root_module_path, source_file_id)
    {
        return Some(module_file_id);
    }

    let path_without_lua_prefix = normalized_no_ext.strip_prefix("lua/")?;
    module_index
        .find_module_file_for_file(&path_without_lua_prefix.replace('/', "."), source_file_id)
}

fn mark_load_root(
    file_infos: &mut HashMap<FileId, GmodFileLoadInfo>,
    file_id: FileId,
    kind: GmodLoadRootKind,
    states: GmodStateMask,
    path_sort_key: String,
) {
    let info = file_infos
        .entry(file_id)
        .or_insert_with(GmodFileLoadInfo::fallback_shared);
    info.mark_states(
        states,
        GmodLoadStatus::EngineLoaded,
        GmodLoadConfidence::Engine,
    );
    info.add_root(GmodLoadRoot {
        kind,
        states,
        path_sort_key,
    });
}

fn source_states_for_load_site(
    file_infos: &HashMap<FileId, GmodFileLoadInfo>,
    fallback_masks: &HashMap<FileId, GmodStateMask>,
    branch_realm_ranges: &HashMap<FileId, Vec<GmodRealmRange>>,
    site: &LuaDependencySite,
) -> GmodStateMask {
    let source_states = file_infos
        .get(&site.source_file_id)
        .map(|info| info.state_mask)
        .filter(|states| !states.is_empty())
        .or_else(|| fallback_masks.get(&site.source_file_id).copied())
        .unwrap_or_else(GmodStateMask::empty);

    let Some(ranges) = branch_realm_ranges.get(&site.source_file_id) else {
        return source_states;
    };

    let Some(branch_realm) = ranges
        .iter()
        .find(|range| range.range.contains(site.range.start()))
        .map(|range| range.realm)
    else {
        return source_states;
    };

    let branch_states = GmodStateMask::from_realm(branch_realm);
    if source_states.is_empty() {
        branch_states
    } else {
        source_states.intersection(branch_states)
    }
}

fn load_site_sort_key(
    db: &DbIndex,
    site: &LuaDependencySite,
) -> (std::sync::Arc<str>, u32, u32, u8, String) {
    let source_path = db.get_vfs().file_order_key(&site.source_file_id);
    let kind_rank = match site.kind {
        LuaDependencyKind::Require => 0,
        LuaDependencyKind::Include => 1,
        LuaDependencyKind::CompileFile => 2,
        LuaDependencyKind::AddCSLuaFile => 3,
        LuaDependencyKind::IncludeCS => 4,
    };
    (
        source_path,
        site.range.start().into(),
        site.range.end().into(),
        kind_rank,
        site.path.clone().unwrap_or_default(),
    )
}

fn record_load_site_edge(
    file_infos: &mut HashMap<FileId, GmodFileLoadInfo>,
    unresolved_edges: &mut Vec<GmodLoadEdge>,
    site: &LuaDependencySite,
    source_states: GmodStateMask,
) {
    let edge = GmodLoadEdge {
        source_file_id: site.source_file_id,
        target_file_id: site.target_file_id,
        kind: GmodLoadEdgeKind::from(site.kind),
        states: source_states,
        path: site.path.clone(),
        original_expr: Some(site.original_expr.clone()),
        range: Some(site.range),
    };

    let Some(target_file_id) = site.target_file_id else {
        if !unresolved_edges.contains(&edge) {
            unresolved_edges.push(edge);
        }
        return;
    };

    file_infos
        .entry(target_file_id)
        .or_insert_with(GmodFileLoadInfo::fallback_shared)
        .add_incoming_edge(edge);
}

fn apply_load_site(
    file_infos: &mut HashMap<FileId, GmodFileLoadInfo>,
    site: &LuaDependencySite,
    source_states: GmodStateMask,
) -> bool {
    let Some(target_file_id) = site.target_file_id else {
        return false;
    };

    let mut changed = false;
    let target_info = file_infos
        .entry(target_file_id)
        .or_insert_with(GmodFileLoadInfo::fallback_shared);

    match site.kind {
        LuaDependencyKind::AddCSLuaFile => {
            target_info.client_send_available = true;
            changed |= target_info.mark_states(
                GmodStateMask::CLIENT,
                GmodLoadStatus::ReachableByLoadEdge,
                GmodLoadConfidence::Static,
            );
            if target_file_id == site.source_file_id {
                let self_source_states = if source_states.is_empty() {
                    GmodStateMask::SERVER
                } else {
                    source_states
                };
                changed |= target_info.mark_states(
                    self_source_states,
                    GmodLoadStatus::ReachableByLoadEdge,
                    GmodLoadConfidence::Static,
                );
            }
        }
        LuaDependencyKind::Include => {
            if !source_states.is_empty() {
                changed |= target_info.mark_states(
                    source_states,
                    GmodLoadStatus::ReachableByLoadEdge,
                    GmodLoadConfidence::Static,
                );
            }
        }
        LuaDependencyKind::CompileFile => {}
        LuaDependencyKind::IncludeCS => {
            target_info.client_send_available = true;
            changed |= target_info.mark_states(
                GmodStateMask::CLIENT,
                GmodLoadStatus::ReachableByLoadEdge,
                GmodLoadConfidence::Static,
            );
            if !source_states.is_empty() {
                changed |= target_info.mark_states(
                    source_states,
                    GmodLoadStatus::ReachableByLoadEdge,
                    GmodLoadConfidence::Static,
                );
            }
        }
        LuaDependencyKind::Require => {
            changed |= target_info.mark_states(
                GmodStateMask::SHARED,
                GmodLoadStatus::ReachableByLoadEdge,
                GmodLoadConfidence::Static,
            );
        }
    }

    changed
}

fn engine_load_root_for_file(
    db: &DbIndex,
    file_id: FileId,
) -> Option<(GmodLoadRootKind, GmodStateMask, String)> {
    let rel_path = gmod_relative_path(db, file_id)?;
    engine_load_root_for_relative_path(&rel_path).map(|(kind, states)| (kind, states, rel_path))
}

fn engine_load_root_for_relative_path(rel_path: &str) -> Option<(GmodLoadRootKind, GmodStateMask)> {
    let rel_path = rel_path.trim_start_matches('/');
    let parts = rel_path.split('/').collect::<Vec<_>>();

    match rel_path {
        "includes/init.lua" => {
            return Some((GmodLoadRootKind::IncludesInit, GmodStateMask::SHARED));
        }
        "includes/init_menu.lua" => {
            return Some((GmodLoadRootKind::IncludesInitMenu, GmodStateMask::MENU));
        }
        "derma/init.lua" => {
            return Some((
                GmodLoadRootKind::DermaInit,
                GmodStateMask::CLIENT.union(GmodStateMask::MENU),
            ));
        }
        "menu/menu.lua" => return Some((GmodLoadRootKind::MenuMain, GmodStateMask::MENU)),
        _ => {}
    }

    if rel_path.starts_with("autorun/client/") {
        return Some((GmodLoadRootKind::AutorunClient, GmodStateMask::CLIENT));
    }
    if rel_path.starts_with("autorun/server/") {
        return Some((GmodLoadRootKind::AutorunServer, GmodStateMask::SERVER));
    }
    if rel_path.starts_with("autorun/") {
        return Some((GmodLoadRootKind::Autorun, GmodStateMask::SHARED));
    }
    if rel_path.starts_with("vgui/") {
        return Some((
            GmodLoadRootKind::Vgui,
            GmodStateMask::CLIENT.union(GmodStateMask::MENU),
        ));
    }
    if rel_path.starts_with("postprocess/") {
        return Some((GmodLoadRootKind::PostProcess, GmodStateMask::CLIENT));
    }
    if rel_path.starts_with("matproxy/") {
        return Some((GmodLoadRootKind::MatProxy, GmodStateMask::CLIENT));
    }
    if rel_path.starts_with("skins/") {
        return Some((GmodLoadRootKind::Skin, GmodStateMask::CLIENT));
    }
    if is_effect_path(&parts) {
        return Some((GmodLoadRootKind::ScriptedEffect, GmodStateMask::SHARED));
    }
    if is_stool_path(&parts) {
        return Some((GmodLoadRootKind::Stool, GmodStateMask::SHARED));
    }
    if let Some(root) = gamemode_root_for_parts(&parts) {
        return Some(root);
    }
    if let Some(root) = scripted_class_root_for_parts(&parts) {
        return Some(root);
    }

    None
}

fn gamemode_root_for_parts(parts: &[&str]) -> Option<(GmodLoadRootKind, GmodStateMask)> {
    let gamemode_idx = parts.iter().rposition(|part| *part == "gamemode")?;
    let file_name = *parts.get(gamemode_idx + 1)?;
    if parts.get(gamemode_idx + 2).is_some() {
        return None;
    }
    match file_name {
        "init.lua" => Some((GmodLoadRootKind::GamemodeInit, GmodStateMask::SERVER)),
        "cl_init.lua" => Some((GmodLoadRootKind::GamemodeClientInit, GmodStateMask::CLIENT)),
        "shared.lua" => Some((GmodLoadRootKind::GamemodeShared, GmodStateMask::SHARED)),
        _ => None,
    }
}

fn scripted_class_root_for_parts(parts: &[&str]) -> Option<(GmodLoadRootKind, GmodStateMask)> {
    let file_name = *parts.last()?;
    if let Some(kind) = scripted_folder_kind(parts) {
        return match file_name {
            "init.lua" => Some((kind, GmodStateMask::SERVER)),
            "cl_init.lua" => Some((kind, GmodStateMask::CLIENT)),
            "shared.lua" => None,
            _ => None,
        };
    }

    if !file_name.ends_with(".lua")
        || matches!(file_name, "init.lua" | "cl_init.lua" | "shared.lua")
    {
        return None;
    }

    let parent = parts.get(parts.len().saturating_sub(2)).copied();
    match parent {
        Some("weapons") => Some((GmodLoadRootKind::ScriptedWeapon, GmodStateMask::SHARED)),
        Some("entities") => Some((GmodLoadRootKind::ScriptedEntity, GmodStateMask::SHARED)),
        _ => None,
    }
}

fn scripted_folder_kind(parts: &[&str]) -> Option<GmodLoadRootKind> {
    if parts.len() < 3 {
        return None;
    }
    let class_parent = parts.get(parts.len() - 3).copied()?;
    match class_parent {
        "weapons" => Some(GmodLoadRootKind::ScriptedWeapon),
        "entities" => Some(GmodLoadRootKind::ScriptedEntity),
        _ => None,
    }
}

fn is_effect_path(parts: &[&str]) -> bool {
    parts
        .windows(2)
        .any(|window| window == ["entities", "effects"])
        || parts.first().copied() == Some("effects")
}

fn is_stool_path(parts: &[&str]) -> bool {
    parts.contains(&"stools")
}

fn infer_realm_from_load_path_hint(db: &DbIndex, file_id: FileId) -> Option<GmodRealm> {
    let file_path = db.get_vfs().get_file_path(&file_id)?;
    let file_name = file_path
        .file_name()?
        .to_string_lossy()
        .to_ascii_lowercase();
    if file_name.starts_with("cl_") {
        return Some(GmodRealm::Client);
    }
    if file_name.starts_with("sv_") {
        return Some(GmodRealm::Server);
    }
    if file_name.starts_with("sh_") {
        return Some(GmodRealm::Shared);
    }

    let rel_path = gmod_relative_path(db, file_id)?;
    let parts = rel_path.split('/').collect::<Vec<_>>();
    if rel_path.contains("/client/") || rel_path.contains("/cl/") {
        return Some(GmodRealm::Client);
    }
    if rel_path.contains("/server/") || rel_path.contains("/sv/") {
        return Some(GmodRealm::Server);
    }
    if rel_path.contains("/shared/") || rel_path.contains("/sh/") {
        return Some(GmodRealm::Shared);
    }
    if let Some((_, states)) = engine_load_root_for_relative_path(&rel_path) {
        return Some(states.to_realm(GmodRealm::Shared));
    }
    if is_menu_relative_path(&rel_path) {
        return None;
    }
    if scripted_folder_kind(&parts).is_some() {
        return match file_name.as_str() {
            "init.lua" => Some(GmodRealm::Server),
            "cl_init.lua" => Some(GmodRealm::Client),
            "shared.lua" => Some(GmodRealm::Shared),
            _ => None,
        };
    }
    match file_name.as_str() {
        "cl_init.lua" => Some(GmodRealm::Client),
        "shared.lua" => Some(GmodRealm::Shared),
        _ => None,
    }
}

/// Whether the file sits under `lua/menu/`, which the menu state loads by convention; it names the
/// realm only of a file no load path reaches, since an include from another state runs it there.
fn is_menu_directory_path(db: &DbIndex, file_id: FileId) -> bool {
    gmod_relative_path(db, file_id).is_some_and(|path| is_menu_relative_path(&path))
}

fn is_menu_relative_path(rel_path: &str) -> bool {
    rel_path.starts_with("menu/")
}

fn gmod_relative_path(db: &DbIndex, file_id: FileId) -> Option<String> {
    let path = db
        .get_vfs()
        .get_file_path(&file_id)?
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();

    if let Some(idx) = path.rfind("/lua/") {
        return Some(path[idx + "/lua/".len()..].to_string());
    }

    for anchor in [
        "/gamemodes/",
        "/gamemode/",
        "/entities/",
        "/weapons/",
        "/effects/",
    ] {
        if let Some(idx) = path.rfind(anchor) {
            return Some(path[idx + 1..].to_string());
        }
    }

    None
}

fn rebuild_realm_metadata(
    db: &mut DbIndex,
    branch_realm_ranges: HashMap<FileId, Vec<GmodRealmRange>>,
    annotation_realms: HashMap<FileId, GmodRealm>,
    analyzed_file_ids: &[FileId],
) {
    let file_ids = db.get_vfs().get_all_local_file_ids();
    let meta_file_ids: HashSet<FileId> = {
        let module_index = db.get_module_index();
        file_ids
            .iter()
            .copied()
            .filter(|file_id| module_index.is_meta_file(file_id))
            .collect()
    };
    let library_file_ids: HashSet<FileId> = {
        let module_index = db.get_module_index();
        file_ids
            .iter()
            .copied()
            .filter(|file_id| {
                module_index
                    .get_workspace_id(*file_id)
                    .map(|ws_id| module_index.is_library_workspace_id(ws_id))
                    .unwrap_or(false)
            })
            .collect()
    };
    let analyzed_file_ids: HashSet<FileId> = analyzed_file_ids.iter().copied().collect();
    let previous_realm_metadata: HashMap<FileId, GmodRealmFileMetadata> = file_ids
        .iter()
        .filter_map(|file_id| {
            db.get_gmod_infer_index()
                .get_realm_file_metadata(file_id)
                .cloned()
                .map(|metadata| (*file_id, metadata))
        })
        .collect();

    let resolve_branch_ranges = |file_id: &FileId| {
        if let Some(ranges) = branch_realm_ranges.get(file_id) {
            return ranges.clone();
        }
        if analyzed_file_ids.contains(file_id) {
            return Vec::new();
        }
        previous_realm_metadata
            .get(file_id)
            .map(|metadata| metadata.branch_realm_ranges.clone())
            .unwrap_or_default()
    };

    let resolve_annotation_realm = |file_id: &FileId| {
        if let Some(realm) = annotation_realms.get(file_id) {
            return Some(*realm);
        }
        if analyzed_file_ids.contains(file_id) {
            return None;
        }
        previous_realm_metadata
            .get(file_id)
            .and_then(|metadata| metadata.annotation_realm)
    };

    let mut realm_metadata = rustc_hash::FxHashMap::default();
    for file_id in file_ids {
        let ranges = if meta_file_ids.contains(&file_id) {
            Vec::new()
        } else {
            resolve_branch_ranges(&file_id)
        };

        let annotation_realm = resolve_annotation_realm(&file_id);
        let is_meta_file = meta_file_ids.contains(&file_id);
        let gmod = &db.get_file_emmyrc(file_id).gmod;
        let default_realm = gmod_config_default_realm(&gmod.default_realm);
        let detect_filename = gmod.detect_realm_from_filename.unwrap_or(true);
        let detect_calls = gmod.detect_realm_from_calls.unwrap_or(true);
        if !detect_filename && !detect_calls {
            let realm = if is_meta_file || library_file_ids.contains(&file_id) {
                annotation_realm.unwrap_or(GmodRealm::Shared)
            } else {
                annotation_realm.unwrap_or(default_realm)
            };
            realm_metadata.insert(
                file_id,
                GmodRealmFileMetadata {
                    inferred_realm: realm,
                    annotation_realm,
                    branch_realm_ranges: ranges,
                    ..Default::default()
                },
            );
            continue;
        }
        let load_info = db.get_gmod_load_index().get_file_info(&file_id);
        let load_realm = load_info.map(|info| info.realm);
        let load_status = load_info.map(|info| info.status);
        let load_state_mask = load_info
            .map(|info| info.state_mask)
            .unwrap_or_else(GmodStateMask::empty);
        let filename_hint = if !is_meta_file && detect_filename {
            infer_realm_from_filename(db, file_id)
        } else {
            None
        };
        let hints = if is_meta_file || !detect_calls {
            Vec::new()
        } else {
            let mut hints = load_info
                .filter(|info| info.status != GmodLoadStatus::NoKnownLoadPath)
                .map(|info| {
                    info.incoming_edges
                        .iter()
                        .map(|edge| edge.states.to_realm(info.realm))
                        .filter(|realm| *realm != GmodRealm::Unknown)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if let Some(info) = load_info
                && info.status != GmodLoadStatus::NoKnownLoadPath
                && info.realm != GmodRealm::Unknown
            {
                hints.push(info.realm);
            }
            hints.sort_by_key(|realm| realm_sort_key(*realm));
            hints.dedup();
            hints
        };

        let is_library_file = library_file_ids.contains(&file_id);

        let final_realm = if is_meta_file {
            // Meta files default to Shared unless explicitly annotated otherwise
            annotation_realm.unwrap_or(GmodRealm::Shared)
        } else if is_library_file {
            // Library files (annotations) default to Shared since they define cross-realm APIs
            annotation_realm.unwrap_or(GmodRealm::Shared)
        } else {
            annotation_realm
                .or_else(|| {
                    detect_calls
                        .then_some(())
                        .and(load_info)
                        .filter(|info| info.status == GmodLoadStatus::EngineLoaded)
                        .map(|info| info.realm)
                })
                .or(filename_hint.filter(|realm| *realm != GmodRealm::Menu))
                .or_else(|| {
                    detect_calls
                        .then_some(())
                        .and(load_info)
                        .filter(|info| info.status != GmodLoadStatus::NoKnownLoadPath)
                        .map(|info| info.realm)
                })
                .or(filename_hint)
                .unwrap_or(default_realm)
        };

        realm_metadata.insert(
            file_id,
            GmodRealmFileMetadata {
                inferred_realm: final_realm,
                load_realm,
                load_status,
                load_state_mask,
                filename_hint,
                dependency_hints: hints,
                annotation_realm,
                branch_realm_ranges: ranges,
            },
        );
    }

    db.get_gmod_infer_index_mut()
        .set_all_realm_file_metadata(realm_metadata);
}

fn gmod_config_default_realm(realm: &EmmyrcGmodRealm) -> GmodRealm {
    match realm {
        EmmyrcGmodRealm::Client => GmodRealm::Client,
        EmmyrcGmodRealm::Server => GmodRealm::Server,
        EmmyrcGmodRealm::Shared => GmodRealm::Shared,
        EmmyrcGmodRealm::Menu => GmodRealm::Menu,
    }
}

fn infer_realm_from_filename(db: &DbIndex, file_id: FileId) -> Option<GmodRealm> {
    let file_path = db.get_vfs().get_file_path(&file_id)?;
    let file_name = file_path
        .file_name()?
        .to_string_lossy()
        .to_ascii_lowercase();

    // 1. Check filename prefixes FIRST (highest confidence)
    if file_name.starts_with("cl_") {
        return Some(GmodRealm::Client);
    }
    if file_name.starts_with("sv_") {
        return Some(GmodRealm::Server);
    }
    if file_name.starts_with("sh_") {
        return Some(GmodRealm::Shared);
    }

    // 2. Check parent directory names for realm hints SECOND
    // Prefer the path segment after the last `/lua/` anchor to avoid false realm hints
    // from unrelated parent directory names (e.g. a user home directory named "server").
    // If there is no `/lua/` anchor, still allow inference for known GMod workspace layouts
    // such as addon-root (`lua/...`) and gamemode-root (`gamemode/...`, `entities/...`).
    let path_str = file_path.to_string_lossy().to_ascii_lowercase();
    let path_str = path_str.replace('\\', "/");
    let components = file_path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect::<Vec<_>>();

    // Try to find /lua/ anchor first
    let search_str = if let Some(idx) = path_str.rfind("/lua/") {
        &path_str[idx..]
    } else {
        // Fall back to full-path detection only for known GMod-like trees.
        let is_gmod_tree = components.iter().any(|segment| {
            matches!(
                segment.as_str(),
                "addons"
                    | "gamemodes"
                    | "lua"
                    | "gamemode"
                    | "entities"
                    | "weapons"
                    | "effects"
                    | "postprocess"
                    | "vgui"
                    | "matproxy"
                    | "skins"
                    | "autorun"
                    | "includes"
            )
        });
        if !is_gmod_tree {
            return None;
        }
        &path_str
    };

    if search_str.contains("/client/") || search_str.contains("/cl/") {
        return Some(GmodRealm::Client);
    }
    if search_str.contains("/server/") || search_str.contains("/sv/") {
        return Some(GmodRealm::Server);
    }
    if search_str.contains("/shared/") || search_str.contains("/sh/") {
        return Some(GmodRealm::Shared);
    }

    if is_menu_directory_path(db, file_id) {
        return Some(GmodRealm::Menu);
    }

    // 3. Check GMod special directory patterns (engine-defined realm behavior per GMod loading order)
    // These MUST come before the init.lua/shared.lua filename checks because e.g.
    // effects/init.lua should be Shared (effects load on both realms), not Server.
    if search_str.contains("/effects/") {
        return Some(GmodRealm::Shared);
    }
    if search_str.contains("/vgui/") {
        return Some(GmodRealm::Client);
    }
    if search_str.contains("/postprocess/") {
        return Some(GmodRealm::Client);
    }
    if search_str.contains("/matproxy/") {
        return Some(GmodRealm::Client);
    }
    if search_str.contains("/skins/") {
        return Some(GmodRealm::Client);
    }
    if search_str.contains("/autorun/") {
        // Note: autorun/server/ and autorun/client/ are already caught above
        // by the /server/ and /client/ directory checks.
        return Some(GmodRealm::Shared);
    }
    if search_str.contains("/includes/") {
        return Some(GmodRealm::Shared);
    }
    if search_str.contains("/stools/") {
        return Some(GmodRealm::Shared);
    }

    // 4. Check specific filenames LAST (lowest confidence)
    if file_name == "cl_init.lua" {
        return Some(GmodRealm::Client);
    }
    if file_name == "init.lua" {
        return Some(GmodRealm::Server);
    }
    if file_name == "shared.lua" {
        return Some(GmodRealm::Shared);
    }

    None
}

fn realm_sort_key(realm: GmodRealm) -> u8 {
    match realm {
        GmodRealm::Client => 0,
        GmodRealm::Server => 1,
        GmodRealm::Shared => 2,
        GmodRealm::Menu => 3,
        GmodRealm::Unknown => 4,
    }
}

/// Collect @fileparam annotations from a chunk, returning (name_lowercase, type_text) pairs.
fn collect_file_params(chunk: &LuaChunk) -> Vec<(String, String)> {
    let mut params = Vec::new();
    for descendant in chunk.syntax().descendants() {
        if LuaDocTagFileparam::can_cast(descendant.kind().into()) {
            if let Some(fileparam) = LuaDocTagFileparam::cast(descendant) {
                if let Some(name_token) = fileparam.get_name_token() {
                    if let Some(typ) = fileparam.get_type() {
                        let name = name_token.get_name_text().to_ascii_lowercase();
                        let type_text = typ.syntax().text().to_string();
                        params.push((name, type_text));
                    }
                }
            }
        }
    }
    params
}
