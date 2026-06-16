use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

use aho_corasick::AhoCorasick;
use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAst, LuaAstNode, LuaAstToken, LuaBlock, LuaCallExpr,
    LuaChunk, LuaClosureExpr, LuaComment, LuaCommentOwner, LuaDocDescriptionOwner, LuaDocTag,
    LuaDocTagFileparam, LuaDocTagRealm, LuaElseClauseStat, LuaElseIfClauseStat, LuaExpr,
    LuaForRangeStat, LuaForStat, LuaFuncStat, LuaIfStat, LuaIndexKey, LuaLiteralToken,
    LuaLocalFuncStat, LuaLocalName, LuaLocalStat, LuaNameExpr, LuaRepeatStat, LuaStat,
    LuaSyntaxId, LuaSyntaxNode, LuaTableExpr, LuaVarExpr, LuaWhileStat, NumberResult, PathTrait,
};

use crate::{
    EmmyrcGmodRealm, FileId, GlobalId, GmodClassCallArgSource, GmodClassCallLiteral,
    GmodDermaSkinCallRoles, GmodNamedStringCallRoles, GmodNetworkVarCallRoles,
    GmodScriptedClassCallKind, GmodScriptedClassCallMetadata, GmodScriptedClassFileMetadata,
    GmodVguiPanelCallRoles, InFiled, LuaCallArgRole, LuaDecl, LuaDeclExtra, LuaDeclId,
    LuaDeclLocation, LuaDeclTypeKind, LuaFunctionType, LuaMember, LuaMemberFeature, LuaMemberId,
    LuaMemberKey, LuaSignature, LuaSignatureId, LuaType, LuaTypeCache, LuaTypeDecl, LuaTypeDeclId,
    LuaTypeFlag, LuaTypeOwner,
    compilation::analyzer::{AnalysisPipeline, AnalyzeContext, common::add_member},
    db_index::{
        AsyncState, DbIndex, GmodCallbackSiteMetadata, GmodConVarKind, GmodConVarSiteMetadata,
        GmodConcommandSiteMetadata, GmodFileLoadInfo, GmodHookKind, GmodHookNameIssue,
        GmodHookSiteMetadata, GmodLoadConfidence, GmodLoadEdge, GmodLoadEdgeKind, GmodLoadRoot,
        GmodLoadRootKind, GmodLoadStatus, GmodNamedSiteMetadata, GmodNetReceiveSiteMetadata,
        GmodRealm, GmodRealmFileMetadata, GmodRealmRange, GmodScopedClassInfo, GmodStateMask,
        GmodSystemFileMetadata, GmodTimerKind, GmodTimerSiteMetadata, LuaDependencyKind,
        LuaDependencySite, LuaMemberOwner, NetFlowFrame, NetFlowKind, NetOpEntry, NetOpKind,
        NetReceiveFlow, NetSendFlow, NetSendKind,
    },
    profile::Profile,
};
use rowan::{TextRange, TextSize};

/// Pre-scanned keyword flags for fast gmod_pre skip decisions.
/// Each flag indicates the file source contains the corresponding keyword pattern.
#[derive(Default)]
struct GmodKeywords {
    /// "hook" — hook.Add/Run/Remove call sites, hook method sites
    has_hook: bool,
    /// "net." — net.Start/Receive/Send flows, system call metadata
    has_net: bool,
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

#[derive(Default)]
struct AnnotatedGmodCandidatePresence {
    has_system: bool,
    has_net: bool,
    has_hook: bool,
    has_scripted_class: bool,
    has_load: bool,
}

impl GmodKeywords {
    /// Whether the LuaCallExpr walk in collect_hook_metadata is needed
    fn needs_call_walk(&self) -> bool {
        self.has_hook || self.has_net || self.has_system_call
    }

    /// Whether the LuaFuncStat walk in collect_hook_metadata is needed
    fn needs_func_walk(&self) -> bool {
        self.has_gm_func || self.has_realm_anno
    }

    /// Whether any hook/net metadata collection is needed at all
    fn needs_hook_metadata(&self) -> bool {
        self.needs_call_walk() || self.needs_func_walk()
    }
}

fn scan_gmod_keywords(
    content: &str,
    formatted_hook_prefixes: &[String],
    annotated_global_call_roles: &AnnotatedGmodGlobalCallRoleMap,
) -> GmodKeywords {
    let has_gm_func = content.contains("GM:")
        || content.contains("GAMEMODE:")
        || formatted_hook_prefixes.iter().any(|p| content.contains(p));
    let has_hook_annotation = content.contains("gmod.hook");
    let has_net_annotation = content.contains("gmod.net_message");
    let has_system_annotation = has_net_annotation
        || content.contains("gmod.concommand")
        || content.contains("gmod.convar")
        || content.contains("gmod.timer");
    let has_scripted_class_annotation = content.contains("gmod.vgui_panel")
        || content.contains("gmod.derma_skin")
        || content.contains("gmod.network_var")
        || content.contains("gmod.class_base")
        || content.contains("gmod.gamemode");
    let has_load_annotation = content.contains("gmod.load");
    let annotated_candidates = annotated_global_call_roles.candidate_call_paths_in_content(content);
    GmodKeywords {
        has_hook: content.contains("hook") || has_hook_annotation || annotated_candidates.has_hook,
        has_net: content.contains("net.") || has_net_annotation || annotated_candidates.has_net,
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

/// Pre-analysis phase: runs BEFORE lua_analyze.
/// Collects purely syntactic metadata (hooks, network, realm, scripted class
/// type declarations) so that lua_analyze has correct realm keys and scripted
/// class types available from the start. This avoids the previous architecture
/// where flow analysis used `GmodRealm::Unknown` during lua_analyze and had to
/// recompute everything in the unresolve phase with the correct realm.
pub struct GmodPreAnalysisPipeline;

impl AnalysisPipeline for GmodPreAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        if !db.get_emmyrc().gmod.enabled {
            return;
        }

        let _p = Profile::cond_new("gmod pre-analyze", context.tree_list.len() > 1);
        let tree_list = context.tree_list.clone();
        let do_profile = tree_list.len() > 100 && log::log_enabled!(log::Level::Info);

        // Pre-compute scripted class scope for all files (compile globs once)
        let scripted_scope_files = context.get_or_compute_scripted_scope_files(db).clone();

        let t0 = do_profile.then(std::time::Instant::now);
        let mut branch_realm_ranges: HashMap<FileId, Vec<GmodRealmRange>> = HashMap::new();
        let mut annotation_realms: HashMap<FileId, GmodRealm> = HashMap::new();
        // Wall-clock for the parallel read-only collection pass (hook/system/net
        // flow/realm/fileparam metadata) and the sequential scoped-class merge.
        let mut t_collect = std::time::Duration::ZERO;
        let mut t_scoped = std::time::Duration::ZERO;
        let mut profile = do_profile.then(GmodPreProfile::default);

        // Build a workspace-global registry of helper functions so that
        // net.Read/Write expansion can follow helpers defined in *other* files
        // (DarkRP-style shared helpers like `DarkRP.writeNetDarkRPVarRemoval`).
        // Same-file resolution still takes priority — the registry is only
        // consulted as a fallback.
        //
        // Sources from the VFS rather than `tree_list` because per-file
        // incremental analysis (`update_file_by_uri`) only places the changed
        // file in `tree_list`, but helpers can live in any file.
        let helper_registry = build_helper_registry(db);

        // Pre-format hook method prefixes once to avoid per-file `format!("{p}:")` allocations
        let formatted_hook_prefixes: Vec<String> = db
            .get_emmyrc()
            .gmod
            .hook_mappings
            .method_prefixes
            .iter()
            .map(|p| format!("{p}:"))
            .collect();
        let annotated_global_call_roles = AnnotatedGmodGlobalCallRoleMap::build(db);

        let t_class = do_profile.then(std::time::Instant::now);
        collect_annotated_gmod_call_sites_with(
            db,
            context,
            &formatted_hook_prefixes,
            &annotated_global_call_roles,
        );
        if let Some(t_class) = t_class {
            log::info!(
                "gmod pre: annotated_scripted_class_and_load_calls cost {:?}",
                t_class.elapsed()
            );
        }

        let t_vgui = do_profile.then(std::time::Instant::now);
        let file_ids: Vec<FileId> = tree_list.iter().map(|tree| tree.file_id).collect();
        synthesize_vgui_registrations(db, &file_ids);
        if let Some(t_vgui) = t_vgui {
            log::info!(
                "gmod pre: vgui_registration_bindings cost {:?}",
                t_vgui.elapsed()
            );
        }

        // Per-file metadata collection is read-only against `&DbIndex` (it only
        // reads the reference/decl indexes built by earlier passes plus each
        // file's own AST), so it runs in parallel across files. The collected
        // results are merged into the db sequentially afterward in file order to
        // preserve identical behavior. The scoped-class (`is_in_scope`) work
        // mutates the db and stays in the sequential merge loop.
        let s_collect = do_profile.then(std::time::Instant::now);
        let collect_file_ids: Vec<FileId> =
            tree_list.iter().map(|tree| tree.file_id).collect();
        let collected = super::parallel::map_files_collect(db, &collect_file_ids, |db, file_id| {
            collect_file_gmod_metadata(
                db,
                file_id,
                &helper_registry,
                &formatted_hook_prefixes,
                &annotated_global_call_roles,
            )
        });
        if let Some(s_collect) = s_collect {
            t_collect += s_collect.elapsed();
        }

        for (in_filed_tree, result) in tree_list.iter().zip(collected) {
            let file_id = in_filed_tree.file_id;
            let is_in_scope = scripted_scope_files.contains(&file_id);
            let GmodFileMetadataResult {
                keywords,
                hook_metadata,
                receive_flow_count,
                network_data,
                member_ranges,
                branch_ranges,
                annotation_realm,
                file_params,
            } = result;

            if let Some(profile) = profile.as_mut() {
                profile.files_scanned += 1;
                profile.record_keywords(&keywords, is_in_scope);
            }

            if let Some((hook_sites, system_metadata, gm_method_realms)) = hook_metadata {
                if let Some(profile) = profile.as_mut() {
                    profile.gm_method_realms += gm_method_realms.len();
                    profile.receive_flows += receive_flow_count;
                }
                db.get_gmod_infer_index_mut()
                    .add_hook_sites(file_id, hook_sites);
                db.get_gmod_infer_index_mut()
                    .set_system_file_metadata(file_id, system_metadata);
                if !gm_method_realms.is_empty() {
                    db.get_gmod_infer_index_mut()
                        .set_gm_method_realm_annotations(file_id, gm_method_realms);
                }
            } else if let Some(profile) = profile.as_mut() {
                profile.hook_metadata_skips += 1;
            }

            if let Some(network_data) = network_data {
                db.get_gmod_network_index_mut()
                    .add_file_data(file_id, network_data);
            } else if let Some(profile) = profile.as_mut() {
                profile.netflow_skips += 1;
            }

            if is_in_scope {
                let s = do_profile.then(std::time::Instant::now);
                // Use cached scoped class info from decl phase, or detect if not cached
                let scope_match = db
                    .get_gmod_infer_index()
                    .get_scoped_class_info(&file_id)
                    .map(|info| GmodScopedClassMatch {
                        class_name: info.class_name.clone(),
                        global_name: info.global_name.clone(),
                        class_name_prefix: info.class_name_prefix.clone(),
                    })
                    .or_else(|| {
                        let m = detect_scoped_class_from_path(db, file_id)?;
                        db.get_gmod_infer_index_mut().set_scoped_class_info(
                            file_id,
                            GmodScopedClassInfo {
                                class_name: m.class_name.clone(),
                                global_name: m.global_name.clone(),
                                class_name_prefix: m.class_name_prefix.clone(),
                            },
                        );
                        Some(m)
                    });
                if let Some(scope_match) = scope_match {
                    if let Some(profile) = profile.as_mut() {
                        profile.scoped_class_matches += 1;
                    }
                    ensure_scoped_class_type_decl(
                        db,
                        file_id,
                        &scope_match.class_name,
                        &scope_match.global_name,
                        in_filed_tree.value.syntax().text_range(),
                    );

                    collect_scripted_scope_type_bindings_with(db, file_id, &scope_match);
                    synthesize_scoped_base_assignments_with(
                        db,
                        file_id,
                        in_filed_tree.value.clone(),
                        &scope_match,
                    );
                }
                if let Some(s) = s {
                    t_scoped += s.elapsed();
                }
            }

            if keywords.has_realm_branch {
                if let Some(profile) = profile.as_mut() {
                    profile.branch_realm_ranges += branch_ranges.len();
                }
                if !branch_ranges.is_empty() {
                    branch_realm_ranges.insert(file_id, branch_ranges);
                }
            }
            if keywords.has_realm_anno {
                if let Some(realm) = annotation_realm {
                    annotation_realms.insert(file_id, realm);
                    if let Some(profile) = profile.as_mut() {
                        profile.annotation_realms += 1;
                    }
                }
                if let Some(profile) = profile.as_mut() {
                    profile.member_realm_ranges += member_ranges.len();
                }
                db.get_gmod_infer_index_mut()
                    .set_member_realm_ranges(file_id, member_ranges);
            }

            if let Some(file_params) = file_params
                && !file_params.is_empty()
            {
                db.get_gmod_infer_index_mut()
                    .set_file_params(file_id, file_params);
            }
        }
        if do_profile {
            if let Some(profile) = profile.as_ref() {
                profile.log();
            }
            log::info!(
                "gmod pre: per-file metadata cost {:?} (parallel_collect={:?}, scoped_merge={:?})",
                t0.map(|t0| t0.elapsed()).unwrap_or_default(),
                t_collect,
                t_scoped,
            );
        }

        // Network var wrappers are purely syntactic (AST pattern matching)
        let t1 = do_profile.then(std::time::Instant::now);
        let tree_map: HashMap<FileId, LuaChunk> = tree_list
            .iter()
            .map(|x| (x.file_id, x.value.clone()))
            .collect();
        synthesize_network_var_wrappers(db, &scripted_scope_files, &tree_map);
        if let Some(t1) = t1 {
            log::info!("gmod pre: network_var_wrappers cost {:?}", t1.elapsed());
        }

        let t_load = do_profile.then(std::time::Instant::now);
        rebuild_gmod_load_index(db, &branch_realm_ranges, &file_ids);
        if let Some(t_load) = t_load {
            log::info!(
                "gmod pre: rebuild_gmod_load_index cost {:?}",
                t_load.elapsed()
            );
        }

        let t2 = do_profile.then(std::time::Instant::now);
        rebuild_realm_metadata(db, branch_realm_ranges, annotation_realms, &file_ids);
        if let Some(t2) = t2 {
            log::info!("gmod pre: rebuild_realm_metadata cost {:?}", t2.elapsed());
        }
    }
}

#[derive(Default)]
struct GmodPreProfile {
    files_scanned: usize,
    hook_keyword_files: usize,
    net_keyword_files: usize,
    system_call_keyword_files: usize,
    gm_func_keyword_files: usize,
    realm_branch_keyword_files: usize,
    realm_annotation_keyword_files: usize,
    scoped_files: usize,
    hook_metadata_skips: usize,
    netflow_skips: usize,
    gm_method_realms: usize,
    receive_flows: usize,
    scoped_class_matches: usize,
    branch_realm_ranges: usize,
    annotation_realms: usize,
    member_realm_ranges: usize,
}

impl GmodPreProfile {
    fn record_keywords(&mut self, keywords: &GmodKeywords, is_scoped: bool) {
        self.hook_keyword_files += usize::from(keywords.has_hook);
        self.net_keyword_files += usize::from(keywords.has_net);
        self.system_call_keyword_files += usize::from(keywords.has_system_call);
        self.gm_func_keyword_files += usize::from(keywords.has_gm_func);
        self.realm_branch_keyword_files += usize::from(keywords.has_realm_branch);
        self.realm_annotation_keyword_files += usize::from(keywords.has_realm_anno);
        self.scoped_files += usize::from(is_scoped);
    }

    fn log(&self) {
        log::info!(
            "gmod pre profile: files={} keyword_files hook={} net={} system={} gm_func={} realm_branch={} realm_anno={} scoped={} hook_skips={} netflow_skips={} gm_method_realms={} receive_flows={} scoped_matches={} branch_ranges={} annotation_realms={} member_ranges={}",
            self.files_scanned,
            self.hook_keyword_files,
            self.net_keyword_files,
            self.system_call_keyword_files,
            self.gm_func_keyword_files,
            self.realm_branch_keyword_files,
            self.realm_annotation_keyword_files,
            self.scoped_files,
            self.hook_metadata_skips,
            self.netflow_skips,
            self.gm_method_realms,
            self.receive_flows,
            self.scoped_class_matches,
            self.branch_realm_ranges,
            self.annotation_realms,
            self.member_realm_ranges,
        );
    }
}

/// Post-analysis phase: runs AFTER lua_analyze.
/// Synthesizes members that depend on metadata collected during lua_analyze
/// (gmod_class_metadata_index: AccessorFunc, NetworkVar, VGUI register calls).
pub struct GmodPostAnalysisPipeline;

impl AnalysisPipeline for GmodPostAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        if !db.get_emmyrc().gmod.enabled {
            return;
        }

        let _p = Profile::cond_new("gmod post-analyze", context.tree_list.len() > 1);
        let file_ids: Vec<FileId> = context.tree_list.iter().map(|x| x.file_id).collect();
        let do_profile = context.tree_list.len() > 100 && log::log_enabled!(log::Level::Info);

        let scripted_scope_files = context.get_or_compute_scripted_scope_files(db).clone();

        // Resolve scripted_ents.GetMember delegations BEFORE synthesizing
        // members so that NetworkVar calls copied from target entities are
        // picked up by synthesize_scripted_class_members.
        let t_deleg = do_profile.then(std::time::Instant::now);
        resolve_getmember_network_var_delegations(db, &scripted_scope_files, context);
        if let Some(t_deleg) = t_deleg {
            log::info!(
                "gmod post: getmember_delegations cost {:?}",
                t_deleg.elapsed()
            );
        }

        let t_class = do_profile.then(std::time::Instant::now);
        collect_annotated_scripted_class_calls(db, context);
        if let Some(t_class) = t_class {
            log::info!(
                "gmod post: annotated_scripted_class_calls cost {:?}",
                t_class.elapsed()
            );
        }

        let t1 = do_profile.then(std::time::Instant::now);
        synthesize_vgui_registrations(db, &file_ids);
        if let Some(t1) = t1 {
            log::info!("gmod post: vgui_registrations cost {:?}", t1.elapsed());
        }

        let t_local_register = do_profile.then(std::time::Instant::now);
        synthesize_scripted_ent_registrations(db, &file_ids);
        if let Some(t_local_register) = t_local_register {
            log::info!(
                "gmod post: scripted_ent_registrations cost {:?}",
                t_local_register.elapsed()
            );
        }

        let t0 = do_profile.then(std::time::Instant::now);
        synthesize_scripted_class_members(db, &scripted_scope_files, &file_ids);
        if let Some(t0) = t0 {
            log::info!("gmod post: scripted_class_members cost {:?}", t0.elapsed());
        }
    }
}

fn collect_annotated_scripted_class_calls(db: &mut DbIndex, context: &AnalyzeContext) {
    let formatted_hook_prefixes: Vec<String> = db
        .get_emmyrc()
        .gmod
        .hook_mappings
        .method_prefixes
        .iter()
        .map(|p| format!("{p}:"))
        .collect();
    let annotated_global_call_roles = AnnotatedGmodGlobalCallRoleMap::build(db);
    collect_annotated_scripted_class_calls_with(
        db,
        context,
        &formatted_hook_prefixes,
        &annotated_global_call_roles,
    );
}

fn collect_annotated_scripted_class_calls_with(
    db: &mut DbIndex,
    context: &AnalyzeContext,
    formatted_hook_prefixes: &[String],
    annotated_global_call_roles: &AnnotatedGmodGlobalCallRoleMap,
) {
    for in_filed_tree in &context.tree_list {
        let keywords = db
            .get_vfs()
            .get_file_content(&in_filed_tree.file_id)
            .map(|content| {
                scan_gmod_keywords(
                    content,
                    formatted_hook_prefixes,
                    annotated_global_call_roles,
                )
            })
            .unwrap_or_default();
        if !keywords.has_scripted_class_call {
            continue;
        }

        let annotated_call_roles = AnnotatedGmodCallRoleMap::build(
            db,
            in_filed_tree.file_id,
            &in_filed_tree.value,
            annotated_global_call_roles,
        );
        for call_expr in in_filed_tree
            .value
            .syntax()
            .descendants()
            .filter_map(LuaCallExpr::cast)
        {
            collect_annotated_scripted_class_call_metadata(
                db,
                in_filed_tree.file_id,
                &annotated_call_roles,
                call_expr,
            );
        }
    }
}

fn collect_annotated_gmod_call_sites_with(
    db: &mut DbIndex,
    context: &AnalyzeContext,
    formatted_hook_prefixes: &[String],
    annotated_global_call_roles: &AnnotatedGmodGlobalCallRoleMap,
) {
    for in_filed_tree in &context.tree_list {
        let keywords = db
            .get_vfs()
            .get_file_content(&in_filed_tree.file_id)
            .map(|content| {
                scan_gmod_keywords(
                    content,
                    formatted_hook_prefixes,
                    annotated_global_call_roles,
                )
            })
            .unwrap_or_default();
        if !keywords.has_scripted_class_call && !keywords.has_load_call {
            continue;
        }

        let annotated_call_roles = AnnotatedGmodCallRoleMap::build(
            db,
            in_filed_tree.file_id,
            &in_filed_tree.value,
            annotated_global_call_roles,
        );
        for call_expr in in_filed_tree
            .value
            .syntax()
            .descendants()
            .filter_map(LuaCallExpr::cast)
        {
            if keywords.has_scripted_class_call {
                collect_annotated_scripted_class_call_metadata(
                    db,
                    in_filed_tree.file_id,
                    &annotated_call_roles,
                    call_expr.clone(),
                );
            }
            if keywords.has_load_call {
                collect_annotated_load_dependency_site(
                    db,
                    in_filed_tree.file_id,
                    &annotated_call_roles,
                    call_expr,
                );
            }
        }
    }
}

fn collect_annotated_load_dependency_site(
    db: &mut DbIndex,
    file_id: FileId,
    annotated_roles: &AnnotatedGmodCallRoleMap,
    call_expr: LuaCallExpr,
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

    db.get_file_dependencies_index_mut()
        .add_dependency_site(LuaDependencySite {
            source_file_id: file_id,
            target_file_id,
            kind,
            path,
            original_expr: call_expr.syntax().text().to_string(),
            range: arg_expr.get_range(),
        });
    Some(())
}

/// Workspace-global registry of helper function definitions, stored as
/// `(FileId, LuaSyntaxId)` rather than live red-tree nodes so the registry is
/// `Send + Sync` and can be shared across the parallel per-file collection
/// workers. Each entry is resolved back to a `(LuaBlock, LuaChunk)` on demand by
/// rebuilding the owning file's red tree from the (Send) green tree in the VFS.
struct HelperRegistry {
    map: HashMap<String, (FileId, LuaSyntaxId)>,
    methods: HashMap<String, (FileId, LuaSyntaxId)>,
}

fn build_helper_registry(db: &DbIndex) -> HelperRegistry {
    let mut map: HashMap<String, (FileId, LuaSyntaxId)> = HashMap::new();
    let mut methods: HashMap<String, (FileId, LuaSyntaxId)> = HashMap::new();
    let mut duplicate_methods = HashSet::new();

    let vfs = db.get_vfs();
    // Filter to files containing "net." BEFORE sorting, to avoid allocating
    // path strings for the vast majority of files that don't contain net helpers.
    let mut candidate_file_ids: Vec<FileId> = vfs
        .get_all_file_ids()
        .into_iter()
        .filter(|file_id| {
            vfs.get_file_content(file_id)
                .is_some_and(|c| c.contains("net."))
        })
        .collect();
    candidate_file_ids.sort_by_cached_key(|file_id| {
        let raw_path = vfs
            .get_file_path(file_id)
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default();
        (
            crate::vfs::normalize_path_for_ordering(&raw_path),
            raw_path,
            file_id.id,
        )
    });

    for file_id in candidate_file_ids {
        let Some(tree) = vfs.get_syntax_tree(&file_id) else {
            continue;
        };
        let chunk = tree.get_chunk_node();

        // Single descendants walk dispatching by node kind. Avoids two
        // separate full-file walks for FuncStat and AssignStat.
        for node in chunk.syntax().descendants() {
            if let Some(func_stat) = LuaFuncStat::cast(node.clone()) {
                let Some(func_name) = func_stat.get_func_name() else {
                    continue;
                };
                let Some(block) = func_stat
                    .get_closure()
                    .and_then(|closure| closure.get_block())
                else {
                    continue;
                };

                let key = match &func_name {
                    LuaVarExpr::NameExpr(name_expr) => name_expr.get_name_text(),
                    LuaVarExpr::IndexExpr(index_expr) => dotted_global_key(index_expr),
                };
                let method_name = match &func_name {
                    LuaVarExpr::IndexExpr(index_expr) => index_field_name(index_expr),
                    _ => None,
                };

                if let Some(key) = key {
                    // Deterministic duplicate winner rule:
                    // the first helper discovered in sorted path order wins.
                    map.entry(key)
                        .or_insert_with(|| (file_id, LuaSyntaxId::from_node(block.syntax())));
                }
                if let Some(method_name) = method_name {
                    if methods
                        .insert(
                            method_name.clone(),
                            (file_id, LuaSyntaxId::from_node(block.syntax())),
                        )
                        .is_some()
                    {
                        duplicate_methods.insert(method_name);
                    }
                }
                continue;
            }

            if let Some(assign_stat) = LuaAssignStat::cast(node) {
                let (vars, values) = assign_stat.get_var_and_expr_list();
                for (idx, var) in vars.iter().enumerate() {
                    let Some(LuaExpr::ClosureExpr(closure_expr)) = values.get(idx) else {
                        continue;
                    };
                    let Some(block) = closure_expr.get_block() else {
                        continue;
                    };

                    let key = match var {
                        LuaVarExpr::NameExpr(name_expr) => name_expr.get_name_text(),
                        LuaVarExpr::IndexExpr(index_expr) => dotted_global_key(index_expr),
                    };
                    let method_name = match var {
                        LuaVarExpr::IndexExpr(index_expr) => index_field_name(index_expr),
                        _ => None,
                    };

                    if let Some(key) = key {
                        // Deterministic duplicate winner rule:
                        // the first helper discovered in sorted path order wins.
                        map.entry(key)
                            .or_insert_with(|| (file_id, LuaSyntaxId::from_node(block.syntax())));
                    }
                    if let Some(method_name) = method_name {
                        if methods
                            .insert(
                                method_name.clone(),
                                (file_id, LuaSyntaxId::from_node(block.syntax())),
                            )
                            .is_some()
                        {
                            duplicate_methods.insert(method_name);
                        }
                    }
                }
            }
        }
    }

    for duplicate_method in duplicate_methods {
        methods.remove(&duplicate_method);
    }

    HelperRegistry { map, methods }
}

/// Per-file function definition lookup. Built once and reused for all
/// helper-resolution queries against the same file's syntax tree.
struct FileFunctionMap {
    /// Function bodies: `function f() end`, `local function f() end`,
    /// `local f = function() end`, `f = function() end`.
    bare: HashMap<String, LuaBlock>,
    /// Method bodies, keyed `"Prefix.Field"`:
    /// `function M.f() end`, `M.f = function() end`.
    dotted: HashMap<String, LuaBlock>,
    /// Colon-callable method bodies keyed by field name:
    /// `function M:f() end`, `function ENT:f() end`, `M.f = function() end`.
    /// Used only as a conservative same-file/cross-file fallback for net flow
    /// expansion through `obj:f()` calls.
    methods: HashMap<String, LuaBlock>,
    /// All top-level function-defining blocks in source order, including
    /// duplicates and unnamed closures. Lets callers that need to scan every
    /// function body in the file skip running 4 separate `descendants` walks.
    all_blocks: Vec<LuaBlock>,
}

impl FileFunctionMap {
    fn build(root: &LuaChunk) -> Self {
        let mut bare: HashMap<String, LuaBlock> = HashMap::new();
        let mut dotted: HashMap<String, LuaBlock> = HashMap::new();
        let mut methods: HashMap<String, LuaBlock> = HashMap::new();
        let mut all_blocks: Vec<LuaBlock> = Vec::new();
        for node in root.syntax().descendants() {
            if let Some(local_func_stat) = LuaLocalFuncStat::cast(node.clone()) {
                if let Some(block) = local_func_stat.get_closure().and_then(|c| c.get_block()) {
                    if let Some(local_name) = local_func_stat
                        .get_local_name()
                        .and_then(|n| n.get_name_token())
                    {
                        bare.entry(local_name.get_name_text().to_string())
                            .or_insert_with(|| block.clone());
                    }
                    all_blocks.push(block);
                }
                continue;
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
                        bare.entry(name_token.get_name_text().to_string())
                            .or_insert_with(|| block.clone());
                    }
                    all_blocks.push(block);
                }
                continue;
            }
            if let Some(func_stat) = LuaFuncStat::cast(node.clone()) {
                let Some(block) = func_stat.get_closure().and_then(|c| c.get_block()) else {
                    continue;
                };
                match func_stat.get_func_name() {
                    Some(LuaVarExpr::NameExpr(name_expr)) => {
                        if let Some(name) = name_expr.get_name_text() {
                            bare.entry(name).or_insert_with(|| block.clone());
                        }
                    }
                    Some(LuaVarExpr::IndexExpr(index_expr)) => {
                        if let Some(key) = dotted_global_key(&index_expr) {
                            dotted.entry(key).or_insert_with(|| block.clone());
                        }
                        if let Some(method_name) = index_field_name(&index_expr) {
                            methods.entry(method_name).or_insert_with(|| block.clone());
                        }
                    }
                    None => {}
                }
                all_blocks.push(block);
                continue;
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
                                if let Some(name) = name_expr.get_name_text() {
                                    bare.entry(name).or_insert_with(|| block.clone());
                                }
                            }
                            LuaVarExpr::IndexExpr(index_expr) => {
                                if let Some(key) = dotted_global_key(index_expr) {
                                    dotted.entry(key).or_insert_with(|| block.clone());
                                }
                                if let Some(method_name) = index_field_name(index_expr) {
                                    methods.entry(method_name).or_insert_with(|| block.clone());
                                }
                            }
                        }
                    }
                    all_blocks.push(block);
                }
            }
        }
        FileFunctionMap {
            bare,
            dotted,
            methods,
            all_blocks,
        }
    }
}

/// Lazy cache of per-file function maps, keyed by chunk text-range. Used so
/// that cross-file helper recursion doesn't rebuild the same map repeatedly.
#[derive(Default)]
struct LocalFnCache {
    cache: HashMap<TextRange, FileFunctionMap>,
}

impl LocalFnCache {
    fn get(&mut self, root: &LuaChunk) -> &FileFunctionMap {
        let key = root.syntax().text_range();
        self.cache
            .entry(key)
            .or_insert_with(|| FileFunctionMap::build(root))
    }
}

fn dotted_global_key(index_expr: &glua_parser::LuaIndexExpr) -> Option<String> {
    let LuaExpr::NameExpr(prefix_name) = index_expr.get_prefix_expr()? else {
        return None;
    };
    let prefix_text = prefix_name.get_name_text()?;
    let LuaIndexKey::Name(field_token) = index_expr.get_index_key()? else {
        return None;
    };
    let field_text = field_token.get_name_text();
    Some(format!("{prefix_text}.{field_text}"))
}

fn index_field_name(index_expr: &glua_parser::LuaIndexExpr) -> Option<String> {
    let LuaIndexKey::Name(field_token) = index_expr.get_index_key()? else {
        return None;
    };
    Some(field_token.get_name_text().to_string())
}

/// All per-file gmod pre-analysis metadata collected off-thread for one file.
/// Produced by [`collect_file_gmod_metadata`] (read-only against `&DbIndex`) and
/// merged into the db sequentially by the pipeline in file order.
struct GmodFileMetadataResult {
    keywords: GmodKeywords,
    /// `Some` when hook metadata was collected (file had hook-relevant
    /// keywords): (hook sites, system metadata, gm-method realm annotations).
    /// `None` means the hook walk was skipped for this file.
    hook_metadata: Option<(
        Vec<GmodHookSiteMetadata>,
        GmodSystemFileMetadata,
        Vec<(String, GmodRealm)>,
    )>,
    /// Number of net.Receive flows discovered (for profiling parity).
    receive_flow_count: usize,
    /// `Some` when network flow metadata was collected for this file.
    network_data: Option<crate::db_index::FileNetworkData>,
    /// `---@realm` member ranges (only populated when `keywords.has_realm_anno`).
    member_ranges: Vec<GmodRealmRange>,
    /// Branch realm ranges (only populated when `keywords.has_realm_branch`).
    branch_ranges: Vec<GmodRealmRange>,
    /// File-level realm annotation (only when `keywords.has_realm_anno`).
    annotation_realm: Option<GmodRealm>,
    /// `@fileparam` annotations, when the file content mentions `@fileparam`.
    file_params: Option<Vec<(String, String)>>,
}

/// Collect all per-file gmod pre-analysis metadata for `file_id`. Read-only
/// against `&DbIndex`: reads the file's own AST (rebuilt locally from the Send
/// green tree) plus pre-existing immutable index state, so this is safe to run
/// concurrently across files. The returned [`GmodFileMetadataResult`] is merged
/// into the db sequentially by the caller.
fn collect_file_gmod_metadata(
    db: &DbIndex,
    file_id: FileId,
    helper_registry: &HelperRegistry,
    formatted_hook_prefixes: &[String],
    annotated_global_call_roles: &AnnotatedGmodGlobalCallRoleMap,
) -> GmodFileMetadataResult {
    let content = db.get_vfs().get_file_content(&file_id);
    let keywords = content
        .map(|c| scan_gmod_keywords(c, formatted_hook_prefixes, annotated_global_call_roles))
        .unwrap_or_default();

    // Rebuild the red tree locally from the (Send) green tree so no non-Send
    // rowan node crosses the thread boundary.
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_chunk_node())
    else {
        return GmodFileMetadataResult {
            keywords,
            hook_metadata: None,
            receive_flow_count: 0,
            network_data: None,
            member_ranges: Vec::new(),
            branch_ranges: Vec::new(),
            annotation_realm: None,
            file_params: None,
        };
    };

    let mut local_fns = LocalFnCache::default();

    let (hook_metadata, receive_flows) = if keywords.needs_hook_metadata() {
        let (hook_sites, system_metadata, gm_method_realms, receive_flows) = collect_hook_metadata(
            db,
            file_id,
            root.clone(),
            helper_registry,
            annotated_global_call_roles,
            &mut local_fns,
        );
        (
            Some((hook_sites, system_metadata, gm_method_realms)),
            receive_flows,
        )
    } else {
        (None, Vec::new())
    };
    let receive_flow_count = receive_flows.len();

    let network_data = if keywords.has_net || !receive_flows.is_empty() {
        Some(collect_network_flow_metadata(
            db,
            root.clone(),
            receive_flows,
            helper_registry,
            &mut local_fns,
        ))
    } else {
        None
    };

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
        keywords,
        hook_metadata,
        receive_flow_count,
        network_data,
        member_ranges,
        branch_ranges,
        annotation_realm,
        file_params,
    }
}

fn collect_hook_metadata(
    db: &DbIndex,
    file_id: FileId,
    root: LuaChunk,
    helper_registry: &HelperRegistry,
    annotated_global_call_roles: &AnnotatedGmodGlobalCallRoleMap,
    local_fns: &mut LocalFnCache,
) -> (
    Vec<GmodHookSiteMetadata>,
    GmodSystemFileMetadata,
    Vec<(String, GmodRealm)>,
    Vec<NetReceiveFlow>,
) {
    let mut hook_sites = Vec::new();
    let mut system_metadata = GmodSystemFileMetadata::default();
    let mut gm_method_realms = Vec::new();
    let mut receive_flows = Vec::new();
    let annotated_call_roles =
        AnnotatedGmodCallRoleMap::build(db, file_id, &root, annotated_global_call_roles);

    // Single descendants walk dispatching by node kind. Avoids two separate
    // O(N) walks for the LuaCallExpr and LuaFuncStat passes.
    for node in root.syntax().descendants() {
        if let Some(call_expr) = LuaCallExpr::cast(node.clone()) {
            if let Some(site) =
                collect_hook_call_site(db, file_id, &annotated_call_roles, call_expr.clone())
            {
                hook_sites.push(site);
            }

            if let Some(receive_flow) =
                collect_net_receive_flow(&root, &call_expr, helper_registry, local_fns, db)
            {
                receive_flows.push(receive_flow);
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
            if let Some(site) = collect_hook_method_site(db, func_stat.clone()) {
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

    receive_flows.sort_by_key(|flow| flow.receive_range.start());
    (hook_sites, system_metadata, gm_method_realms, receive_flows)
}

fn collect_network_flow_metadata(
    db: &DbIndex,
    root: LuaChunk,
    receive_flows: Vec<NetReceiveFlow>,
    helper_registry: &HelperRegistry,
    local_fns: &mut LocalFnCache,
) -> crate::db_index::FileNetworkData {
    let mut send_flows = collect_net_send_flows(&root, helper_registry, local_fns, db);
    send_flows.extend(collect_wrapped_net_send_flows(
        &root,
        helper_registry,
        local_fns,
        db,
    ));
    send_flows.sort_by_key(|flow| flow.start_range.start());

    crate::db_index::FileNetworkData {
        send_flows,
        receive_flows,
    }
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

fn collect_net_send_flows(
    root: &LuaChunk,
    helper_registry: &HelperRegistry,
    local_fns: &mut LocalFnCache,
    db: &DbIndex,
) -> Vec<NetSendFlow> {
    let mut flows = Vec::new();

    for block in root.descendants::<LuaBlock>() {
        let stats: Vec<LuaStat> = block.get_stats().collect();
        for (index, stat) in stats.iter().enumerate() {
            let Some(call_expr) = call_expr_from_stat(stat) else {
                continue;
            };

            let Some(method_name) = get_exact_net_method_name(&call_expr) else {
                continue;
            };

            if method_name != "Start" {
                continue;
            }

            let Some(message_name) = extract_static_string_arg_value(&call_expr, 0) else {
                continue;
            };

            let mut writes = Vec::new();
            let mut send = None;

            for next_stat in stats.iter().skip(index + 1) {
                if let Some(next_call_expr) = call_expr_from_stat(next_stat)
                    && let Some(next_method_name) = get_exact_net_method_name(&next_call_expr)
                {
                    if next_method_name == "Start" {
                        break;
                    }

                    if let Some(send_kind) = net_send_kind_from_method_name(&next_method_name) {
                        let target = extract_send_target_text(&next_call_expr, send_kind);
                        send = Some((next_call_expr.get_range(), send_kind, target));
                        break;
                    }
                }

                collect_net_write_ops_from_stat(
                    root,
                    &block,
                    next_stat,
                    &mut writes,
                    helper_registry,
                    local_fns,
                    db,
                );
            }

            let Some((send_range, send_kind, send_target)) = send else {
                continue;
            };

            flows.push(NetSendFlow {
                message_name,
                start_range: call_expr.get_range(),
                writes,
                send_range,
                send_kind,
                send_target,
                is_wrapped: false,
            });
        }
    }

    flows.sort_by_key(|flow| flow.start_range.start());
    flows
}

fn collect_wrapped_net_send_flows(
    root: &LuaChunk,
    helper_registry: &HelperRegistry,
    local_fns: &mut LocalFnCache,
    db: &DbIndex,
) -> Vec<NetSendFlow> {
    let mut flows = Vec::new();

    // Snapshot the per-file function blocks so we can borrow `local_fns`
    // mutably during the recursive collect call below.
    let blocks: Vec<LuaBlock> = local_fns.get(root).all_blocks.clone();
    for block in &blocks {
        collect_wrapped_net_send_flows_in_function_block(
            root,
            block,
            &mut flows,
            helper_registry,
            local_fns,
            db,
        );
    }

    flows.sort_by_key(|flow| flow.start_range.start());
    flows
}

fn collect_wrapped_net_send_flows_in_function_block(
    root: &LuaChunk,
    function_block: &LuaBlock,
    flows: &mut Vec<NetSendFlow>,
    helper_registry: &HelperRegistry,
    local_fns: &mut LocalFnCache,
    db: &DbIndex,
) {
    for block in function_block
        .syntax()
        .descendants()
        .filter_map(LuaBlock::cast)
    {
        if block.syntax() != function_block.syntax()
            && is_block_in_nested_closure(function_block, &block)
        {
            continue;
        }

        let stats: Vec<LuaStat> = block.get_stats().collect();
        for (index, stat) in stats.iter().enumerate() {
            let Some(call_expr) = call_expr_from_stat(stat) else {
                continue;
            };

            let Some(method_name) = get_exact_net_method_name(&call_expr) else {
                continue;
            };

            if method_name != "Start" {
                continue;
            }

            let Some(message_name) = extract_static_string_arg_value(&call_expr, 0) else {
                continue;
            };

            let mut writes = Vec::new();
            let mut send = None;

            for next_stat in stats.iter().skip(index + 1) {
                if let Some(next_call_expr) = call_expr_from_stat(next_stat)
                    && let Some(next_method_name) = get_exact_net_method_name(&next_call_expr)
                {
                    if next_method_name == "Start" {
                        break;
                    }

                    if let Some(send_kind) = net_send_kind_from_method_name(&next_method_name) {
                        let target = extract_send_target_text(&next_call_expr, send_kind);
                        send = Some((next_call_expr.get_range(), send_kind, target));
                        break;
                    }
                }

                collect_net_write_ops_from_stat(
                    root,
                    &block,
                    next_stat,
                    &mut writes,
                    helper_registry,
                    local_fns,
                    db,
                );
            }

            if let Some((send_range, send_kind, send_target)) = send {
                flows.push(NetSendFlow {
                    message_name,
                    start_range: call_expr.get_range(),
                    writes,
                    send_range,
                    send_kind,
                    send_target,
                    is_wrapped: true,
                });
                continue;
            }

            // Wrapped helper flows can start a net message in one function and send at call-site.
            // Keep a conservative stub so counterpart diagnostics can still resolve by message name.
            flows.push(NetSendFlow {
                message_name,
                start_range: call_expr.get_range(),
                writes: Vec::new(),
                send_range: call_expr.get_range(),
                send_kind: NetSendKind::Broadcast,
                send_target: None,
                is_wrapped: true,
            });
        }
    }
}

fn is_block_in_nested_closure(function_block: &LuaBlock, candidate_block: &LuaBlock) -> bool {
    candidate_block
        .syntax()
        .ancestors()
        .take_while(|node| node != function_block.syntax())
        .any(|node| LuaClosureExpr::can_cast(node.kind().into()))
}

fn collect_net_receive_flow(
    root: &LuaChunk,
    call_expr: &LuaCallExpr,
    helper_registry: &HelperRegistry,
    local_fns: &mut LocalFnCache,
    db: &DbIndex,
) -> Option<NetReceiveFlow> {
    let method_name = get_exact_net_method_name(call_expr)?;
    if method_name != "Receive" {
        return None;
    }

    let message_name = extract_static_string_arg_value(call_expr, 0)?;

    let mut reads = Vec::new();
    let mut reads_opaque = false;
    if let Some(callback_expr) = call_expr
        .get_args_list()
        .and_then(|args| args.get_args().nth(1))
    {
        match resolve_callback_block(root, &callback_expr, local_fns) {
            Some(callback_block) => collect_net_read_ops_from_block(
                root,
                callback_block,
                &mut reads,
                helper_registry,
                local_fns,
                db,
            ),
            None => {
                // Inline closure that can't yield a block is malformed — but a
                // bare name reference we couldn't resolve in the file is the
                // common case (callback defined elsewhere). Mark opaque so the
                // mismatch checker skips this flow without losing the
                // counterpart record.
                if !matches!(callback_expr, LuaExpr::ClosureExpr(_)) {
                    reads_opaque = true;
                }
            }
        }
    }

    Some(NetReceiveFlow {
        message_name,
        receive_range: call_expr.get_range(),
        reads,
        reads_opaque,
    })
}

/// Resolve the callback block for a `net.Receive` second argument. Handles
/// inline closures (`function() ... end`) and same-file local/global function
/// references (`net.Receive("Msg", doRetrieve)` paired with
/// `local function doRetrieve() ... end` or `local doRetrieve = function() ... end`).
/// Cross-file references are out of scope — those resolve at semantic-model
/// time and are not part of the per-file collection pass.
fn resolve_callback_block(
    root: &LuaChunk,
    callback_expr: &LuaExpr,
    local_fns: &mut LocalFnCache,
) -> Option<LuaBlock> {
    if let LuaExpr::ClosureExpr(closure_expr) = callback_expr {
        return closure_expr.get_block();
    }

    let LuaExpr::NameExpr(name_expr) = callback_expr else {
        return None;
    };
    let target_name = name_expr.get_name_text()?;

    local_fns.get(root).bare.get(&target_name).cloned()
}

/// Resolve a call expression to a function definition, returning a
/// stable string key (used for cycle detection), the function body block,
/// and the chunk that owns the body (which becomes the new `root` for
/// further nested helper resolution within that body).
///
/// Same-file resolution takes priority. If that fails, falls back to a
/// workspace-global helper registry (cross-file helpers) — DarkRP-style
/// `Module.fn` helpers defined in shared files are the motivating case.
///
/// Handles bare-name calls (`helperFn(...)`) and dotted calls
/// (`Module.fn(...)`). Method calls (`obj:method(...)`) are out of scope.
/// Resolve a `(FileId, LuaSyntaxId)` helper-registry entry back to its
/// `(LuaBlock, LuaChunk)` by rebuilding the owning file's red tree on demand.
/// Returns an owned `LuaChunk` (cheap clone of a red node) which becomes the new
/// `root` for further nested helper resolution within that body.
fn resolve_registry_entry(
    db: &DbIndex,
    file_id: &FileId,
    syntax_id: &LuaSyntaxId,
) -> Option<(LuaBlock, LuaChunk)> {
    let tree = db.get_vfs().get_syntax_tree(file_id)?;
    let chunk = tree.get_chunk_node();
    let node = syntax_id.to_node_from_root(chunk.syntax())?;
    let block = LuaBlock::cast(node)?;
    Some((block, chunk))
}

fn resolve_call_to_function_block(
    root: &LuaChunk,
    call_expr: &LuaCallExpr,
    helper_registry: &HelperRegistry,
    local_fns: &mut LocalFnCache,
    db: &DbIndex,
) -> Option<(String, LuaBlock, LuaChunk)> {
    let prefix = call_expr.get_prefix_expr()?;

    match prefix {
        LuaExpr::NameExpr(name_expr) => {
            let name = name_expr.get_name_text()?;
            if let Some(block) = local_fns.get(root).bare.get(&name).cloned() {
                return Some((name, block, root.clone()));
            }
            // Cross-file fallback: a bare-name call may reference a global
            // function defined in another file (less common than dotted
            // helpers, but supported for symmetry).
            if let Some((file_id, syntax_id)) = helper_registry.map.get(&name)
                && let Some((block, chunk)) = resolve_registry_entry(db, file_id, syntax_id)
            {
                return Some((name.clone(), block, chunk));
            }
            None
        }
        LuaExpr::IndexExpr(index_expr) => {
            if call_expr.is_colon_call()
                && let Some(method_name) = index_field_name(&index_expr)
            {
                if let Some(block) = local_fns.get(root).methods.get(&method_name).cloned() {
                    return Some((format!(":{method_name}"), block, root.clone()));
                }
                if let Some((file_id, syntax_id)) = helper_registry.methods.get(&method_name)
                    && let Some((block, chunk)) = resolve_registry_entry(db, file_id, syntax_id)
                {
                    return Some((format!(":{method_name}"), block, chunk));
                }
            }
            let LuaExpr::NameExpr(prefix_name) = index_expr.get_prefix_expr()? else {
                return None;
            };
            let prefix_text = prefix_name.get_name_text()?;
            let LuaIndexKey::Name(field_token) = index_expr.get_index_key()? else {
                return None;
            };
            let field_text = field_token.get_name_text().to_string();
            let key = format!("{prefix_text}.{field_text}");
            if let Some(block) = local_fns.get(root).dotted.get(&key).cloned() {
                return Some((key, block, root.clone()));
            }
            if let Some((file_id, syntax_id)) = helper_registry.map.get(&key)
                && let Some((block, chunk)) = resolve_registry_entry(db, file_id, syntax_id)
            {
                return Some((key.clone(), block, chunk));
            }
            None
        }
        _ => None,
    }
}

fn collect_net_read_ops_from_block(
    root: &LuaChunk,
    block: LuaBlock,
    reads: &mut Vec<NetOpEntry>,
    helper_registry: &HelperRegistry,
    local_fns: &mut LocalFnCache,
    db: &DbIndex,
) {
    let mut visited = HashSet::new();
    collect_net_ops_recursive(
        root,
        &block,
        block.syntax(),
        reads,
        &mut visited,
        false,
        NetOpDirection::Read,
        helper_registry,
        &[],
        local_fns,
        db,
    );
}

fn collect_net_write_ops_from_stat(
    root: &LuaChunk,
    block: &LuaBlock,
    stat: &LuaStat,
    writes: &mut Vec<NetOpEntry>,
    helper_registry: &HelperRegistry,
    local_fns: &mut LocalFnCache,
    db: &DbIndex,
) {
    let mut visited = HashSet::new();
    collect_net_ops_recursive(
        root,
        block,
        stat.syntax(),
        writes,
        &mut visited,
        false,
        NetOpDirection::Write,
        helper_registry,
        &[],
        local_fns,
        db,
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetOpDirection {
    Read,
    Write,
}

impl NetOpDirection {
    fn matches(self, kind: NetOpKind) -> bool {
        match self {
            NetOpDirection::Read => kind.is_read(),
            NetOpDirection::Write => kind.is_write(),
        }
    }
}

/// Walk `subtree` for net.Read*/Write* call expressions, treating non-net
/// calls that resolve to a same-file function as helper expansions: we recurse
/// into the helper body so writes/reads it performs participate in the
/// outer flow. Cycles are guarded via `visited`, and dynamic-context propagates
/// from the call site into the helper body.
fn collect_net_ops_recursive(
    root: &LuaChunk,
    enclosing_block: &LuaBlock,
    subtree: &LuaSyntaxNode,
    out: &mut Vec<NetOpEntry>,
    visited: &mut HashSet<String>,
    force_dynamic: bool,
    direction: NetOpDirection,
    helper_registry: &HelperRegistry,
    flow_prefix: &[NetFlowFrame],
    local_fns: &mut LocalFnCache,
    db: &DbIndex,
) {
    for call_expr in subtree.descendants().filter_map(LuaCallExpr::cast) {
        if is_call_expr_in_nested_closure(enclosing_block, &call_expr) {
            continue;
        }

        if let Some(method_name) = get_exact_net_method_name(&call_expr) {
            if let Some(op_kind) = NetOpKind::from_fn_name(method_name.as_str())
                && direction.matches(op_kind)
            {
                let dynamic = force_dynamic
                    || is_call_expr_in_dynamic_control_flow(enclosing_block, &call_expr);
                let bits = extract_bit_width_arg(&call_expr, op_kind);
                let value_text = extract_write_value_text(&call_expr, op_kind);
                let local_path = extract_flow_path(enclosing_block, &call_expr);
                let mut flow_path = Vec::with_capacity(flow_prefix.len() + local_path.len());
                flow_path.extend_from_slice(flow_prefix);
                flow_path.extend(local_path);
                out.push(NetOpEntry {
                    kind: op_kind,
                    range: call_expr.get_range(),
                    dynamic,
                    bits,
                    value_text,
                    flow_path,
                });
            }
            continue;
        }

        let Some((helper_key, helper_block, helper_root)) =
            resolve_call_to_function_block(root, &call_expr, helper_registry, local_fns, db)
        else {
            continue;
        };

        if !visited.insert(helper_key.clone()) {
            continue;
        }

        let helper_force_dynamic =
            force_dynamic || is_call_expr_in_dynamic_control_flow(enclosing_block, &call_expr);
        // Carry the call-site's flow context into the helper so reads/writes
        // performed inside the helper appear under the correct outer
        // `for`/`if`/`while` frames in hover.
        let local_path = extract_flow_path(enclosing_block, &call_expr);
        let mut nested_prefix = Vec::with_capacity(flow_prefix.len() + local_path.len());
        nested_prefix.extend_from_slice(flow_prefix);
        nested_prefix.extend(local_path);
        collect_net_ops_recursive(
            &helper_root,
            &helper_block,
            helper_block.syntax(),
            out,
            visited,
            helper_force_dynamic,
            direction,
            helper_registry,
            &nested_prefix,
            local_fns,
            db,
        );
        visited.remove(&helper_key);
    }
}

fn is_call_expr_in_dynamic_control_flow(block: &LuaBlock, call_expr: &LuaCallExpr) -> bool {
    call_expr
        .syntax()
        .ancestors()
        .take_while(|node| node != block.syntax())
        .any(|node| {
            let kind = node.kind().into();
            LuaIfStat::can_cast(kind)
                || LuaWhileStat::can_cast(kind)
                || LuaForStat::can_cast(kind)
                || LuaForRangeStat::can_cast(kind)
                || LuaRepeatStat::can_cast(kind)
        })
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
fn extract_flow_path(block: &LuaBlock, call_expr: &LuaCallExpr) -> Vec<NetFlowFrame> {
    let mut frames: Vec<NetFlowFrame> = Vec::new();
    // When set, the next ancestor (which we know is the parent LuaIfStat of
    // an elseif/else clause we just captured) should be skipped so we don't
    // double-count the if-chain.
    let mut skip_parent_if = false;
    for node in call_expr
        .syntax()
        .ancestors()
        .take_while(|node| node != block.syntax())
    {
        let kind = node.kind().into();

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
            });
            skip_parent_if = true;
            continue;
        }
        if LuaElseClauseStat::can_cast(kind) {
            frames.push(NetFlowFrame {
                kind: NetFlowKind::If,
                header: Some("else".to_string()),
                id: u32::from(node.text_range().start()),
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

/// Pulls the header text for an `elseif cond then` clause from source.
fn extract_branch_header(node: &LuaSyntaxNode, kind: BranchKind) -> Option<String> {
    const MAX_HEADER_LEN: usize = 80;
    let full = node.text().to_string();
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
    let full = stat_node.text().to_string();
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

fn get_exact_net_method_name(call_expr: &LuaCallExpr) -> Option<String> {
    let LuaExpr::IndexExpr(index_expr) = call_expr.get_prefix_expr()? else {
        return None;
    };

    let LuaExpr::NameExpr(prefix_name_expr) = index_expr.get_prefix_expr()? else {
        return None;
    };

    if prefix_name_expr.get_name_text()? != "net" {
        return None;
    }

    let LuaIndexKey::Name(method_name_token) = index_expr.get_index_key()? else {
        return None;
    };

    Some(method_name_token.get_name_text().to_string())
}

/// Extracts the static bit-width literal from `WriteUInt`/`WriteInt`/`ReadUInt`/`ReadInt`.
/// Returns `None` for op kinds without a bit-width parameter, or when the bits arg is
/// not an integer literal (variable, expression, runtime computation). We deliberately
/// only capture literals — anything else is unknowable at index time and would produce
/// false-positive mismatches if compared.
/// Captures a short snippet of the value-arg source text for a `Write*` op so
/// hover can display *what* is being written (e.g. `net.WriteString("hi")`
/// instead of just `net.WriteString`). Returns `None` for read ops, when the
/// arg is missing, when it spans multiple lines, or when it's too long to
/// render inline — robustness over completeness; we'd rather show the bare
/// op name than blow up the hover popup with a 200-char expression.
fn extract_write_value_text(call_expr: &LuaCallExpr, op_kind: NetOpKind) -> Option<String> {
    if !op_kind.is_write() {
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

fn extract_bit_width_arg(call_expr: &LuaCallExpr, op_kind: NetOpKind) -> Option<u32> {
    let arg_idx = match op_kind {
        NetOpKind::WriteUInt | NetOpKind::WriteInt => 1,
        NetOpKind::ReadUInt | NetOpKind::ReadInt => 0,
        _ => return None,
    };

    let arg_expr = call_expr.get_args_list()?.get_args().nth(arg_idx)?;
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

fn extract_static_string_arg_value(call_expr: &LuaCallExpr, arg_idx: usize) -> Option<String> {
    let arg_expr = call_expr.get_args_list()?.get_args().nth(arg_idx)?;
    let LuaExpr::LiteralExpr(literal_expr) = arg_expr else {
        return None;
    };

    let LuaLiteralToken::String(string_token) = literal_expr.get_literal()? else {
        return None;
    };

    Some(string_token.get_value())
}

fn net_send_kind_from_method_name(method_name: &str) -> Option<NetSendKind> {
    match method_name {
        "Send" => Some(NetSendKind::Send),
        "Broadcast" => Some(NetSendKind::Broadcast),
        "SendToServer" => Some(NetSendKind::SendToServer),
        "SendOmit" => Some(NetSendKind::Omit),
        "SendPAS" => Some(NetSendKind::PAS),
        "SendPVS" => Some(NetSendKind::PVS),
        _ => None,
    }
}

/// Captures the recipient argument of a `net.Send*` call as a single-line
/// snippet for display in code lens. Returns `None` for kinds with no
/// recipient (`Broadcast`, `SendToServer`) or when the source is multi-line,
/// too long, or otherwise unsuitable for inline display. Cheap: only inspects
/// the first arg's source text.
fn extract_send_target_text(call_expr: &LuaCallExpr, send_kind: NetSendKind) -> Option<String> {
    match send_kind {
        NetSendKind::Broadcast | NetSendKind::SendToServer => return None,
        _ => {}
    }

    const MAX_INLINE_LEN: usize = 40;

    let arg_expr = call_expr.get_args_list()?.get_args().next()?;
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
    /// The scope's `classNamePrefix` (if any). Used to derive the stripped
    /// short name for parent-alias synthesis (e.g. `gamemode_sandbox` →
    /// `sandbox` → `Sandbox`).
    pub class_name_prefix: Option<String>,
}

const GMOD_ENT_BASE_TO_ENT: &[&str] = &[
    "base_gmodentity",
    "base_brush",
    "base_anim",
    "base_ai",
    "base_nextbot",
    "base_point",
    "base_filter",
];

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
            if decl.get_name() != scope_match.global_name {
                continue;
            }

            let is_scoped_local = decl.is_local()
                && (decl.is_seeded_class_local() || scope_match.global_name == "PLUGIN");
            if is_scoped_local || decl.is_global() {
                decls.push((decl.get_id(), decl.get_range()));
            }
        }
    }

    if decls.is_empty() {
        return;
    }

    let class_decl_id = ensure_scoped_class_type_decl(
        db,
        file_id,
        &scope_match.class_name,
        &scope_match.global_name,
        decls[0].1,
    );

    for (decl_id, _) in decls {
        let previous_decl_type = db
            .get_type_index()
            .get_type_cache(&decl_id.into())
            .map(|type_cache| type_cache.as_type().clone());

        db.get_type_index_mut().force_bind_type(
            decl_id.into(),
            LuaTypeCache::InferType(LuaType::Def(class_decl_id.clone())),
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
            for member_id in table_member_ids {
                add_member(db, class_member_owner.clone(), member_id);
            }
        }
    }
}

fn ensure_scoped_class_type_decl(
    db: &mut DbIndex,
    file_id: FileId,
    class_name: &str,
    global_name: &str,
    range: rowan::TextRange,
) -> LuaTypeDeclId {
    let class_decl_id = get_scripted_class_type_decl_id(global_name, class_name);
    if db.get_type_index().get_type_decl(&class_decl_id).is_none() {
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
    } else if db
        .get_type_index()
        .get_type_decl(&class_decl_id)
        .is_some_and(|decl| {
            !decl
                .get_locations()
                .iter()
                .any(|loc| loc.file_id == file_id)
        })
    {
        db.get_type_index_mut().add_type_decl_location(
            file_id,
            &class_decl_id,
            LuaDeclLocation {
                file_id,
                range,
                flag: LuaTypeFlag::None.into(),
            },
        );
    }

    for super_type in scoped_class_super_types(global_name) {
        db.get_type_index_mut().add_super_type_if_missing(
            class_decl_id.clone(),
            file_id,
            super_type,
        );
    }
    class_decl_id
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

fn scoped_class_uses_global_namespace(global_name: &str) -> bool {
    matches!(global_name, "TOOL" | "EFFECT")
}

fn scoped_class_super_types(global_name: &str) -> Vec<LuaType> {
    let mut super_types = vec![LuaType::Ref(LuaTypeDeclId::global(global_name))];
    match global_name {
        "TOOL" => super_types.push(LuaType::Ref(LuaTypeDeclId::global("Tool"))),
        "SWEP" => super_types.push(LuaType::Ref(LuaTypeDeclId::global("Weapon"))),
        "ENT" => super_types.push(LuaType::Ref(LuaTypeDeclId::global("Entity"))),
        "PLUGIN" => super_types.push(LuaType::Ref(LuaTypeDeclId::global("GM"))),
        _ => {}
    }

    super_types
}

pub(crate) fn ensure_scoped_class_type_decl_for_file(
    db: &mut DbIndex,
    file_id: FileId,
    range: rowan::TextRange,
) -> Option<LuaTypeDeclId> {
    // Use cached info if available, otherwise detect from path
    let (class_name, global_name) =
        if let Some(info) = db.get_gmod_infer_index().get_scoped_class_info(&file_id) {
            (info.class_name.clone(), info.global_name.clone())
        } else {
            let scope_match = detect_scoped_class_from_path(db, file_id)?;
            (scope_match.class_name, scope_match.global_name)
        };
    Some(ensure_scoped_class_type_decl(
        db,
        file_id,
        &class_name,
        &global_name,
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
    let mut map = HashMap::new();
    let gmod_infer = db.get_gmod_infer_index();
    let vfs = db.get_vfs();
    let all_file_ids = vfs.get_all_file_ids();

    for file_id in all_file_ids {
        if let Some(info) = gmod_infer.get_scoped_class_info(&file_id) {
            let is_init = vfs
                .get_file_path(&file_id)
                .and_then(|p| p.file_name().and_then(|name| name.to_str()))
                .is_some_and(|name| name == "init.lua");
            let file_ids = map.entry(info.class_name.clone()).or_insert_with(Vec::new);
            if is_init {
                file_ids.insert(0, file_id);
            } else {
                file_ids.push(file_id);
            }
        }
    }

    map
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
    let mut getmember_locals: HashMap<String, (String, String)> = HashMap::new();

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

            let Some((target_class, target_method)) = getmember_locals.get(&caller_name) else {
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
    for node in chunk.syntax().descendants().filter_map(LuaCallExpr::cast) {
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
            if let Some((effective_base_name, is_derive)) = resolve_effective_inheritance_base(
                &metadata,
                scope_match.class_name_prefix.as_deref(),
            ) {
                synthesize_inheritance_base(
                    db,
                    file_id,
                    &class_decl_id,
                    &effective_base_name,
                    is_derive,
                    scope_match.class_name_prefix.as_deref(),
                );
            }
            if let Some(effective_call) =
                metadata.define_baseclass_calls.iter().rev().find(|call| {
                    matches!(
                        call.literal_args.first(),
                        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty()
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

        // AccessorFunc: synthesize Get/Set/field members
        if let Some(ref scope_match) = scope_match {
            let class_decl_id =
                get_scripted_class_type_decl_id(&scope_match.global_name, &scope_match.class_name);
            for call in &metadata.accessor_func_calls {
                synthesize_accessor_func(db, file_id, &class_decl_id, call);
            }
        }

        // NetworkVar: synthesize Get/Set members
        if let Some(ref scope_match) = scope_match {
            let class_decl_id =
                get_scripted_class_type_decl_id(&scope_match.global_name, &scope_match.class_name);
            for call in &metadata.network_var_calls {
                synthesize_network_var(db, file_id, &class_decl_id, call);
            }
        }

        // NetworkVarElement: synthesize Get/Set members (always number type)
        if let Some(ref scope_match) = scope_match {
            let class_decl_id =
                get_scripted_class_type_decl_id(&scope_match.global_name, &scope_match.class_name);
            for call in &metadata.network_var_element_calls {
                synthesize_network_var_element(db, file_id, &class_decl_id, call);
            }
        }
    }
}

/// Synthesize vgui.Register / derma.DefineControl class types.
fn synthesize_vgui_registrations(db: &mut DbIndex, file_ids: &[FileId]) {
    struct VguiRegistrationRegion {
        file_id: FileId,
        decl_id: LuaDeclId,
        class_decl_id: LuaTypeDeclId,
        panel_name: String,
        region_start: TextSize,
        region_end: TextSize,
    }

    let mut vgui_registration_regions: Vec<VguiRegistrationRegion> = Vec::new();

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
            if let Some(GmodClassCallLiteral::String(panel_name)) =
                call.value_for_arg_source(&panel_source)
            {
                if let Some(GmodClassCallLiteral::NameRef(table_var)) =
                    call.value_for_arg_source(&table_source)
                    && let Some((decl_id, region_start)) =
                        resolve_local_registration_region(db, file_id, table_var, register_position)
                {
                    vgui_registration_regions.push(VguiRegistrationRegion {
                        file_id,
                        decl_id,
                        class_decl_id: LuaTypeDeclId::global(panel_name),
                        panel_name: panel_name.clone(),
                        region_start,
                        region_end: register_position,
                    });
                }
            }
            synthesize_vgui_register(db, file_id, call);
        }

        for call in &metadata.vgui_register_table_calls {
            let register_position = call.syntax_id.get_range().start();
            let table_source = call.vgui_panel_table_arg_source(0);
            if let Some(GmodClassCallLiteral::NameRef(table_var)) =
                call.value_for_arg_source(&table_source)
                && let Some((decl_id, region_start)) =
                    resolve_local_registration_region(db, file_id, table_var, register_position)
            {
                let class_decl_id = vgui_register_table_type_decl_id(file_id, call);
                vgui_registration_regions.push(VguiRegistrationRegion {
                    file_id,
                    decl_id,
                    panel_name: class_decl_id.get_simple_name().to_string(),
                    class_decl_id,
                    region_start,
                    region_end: register_position,
                });
            }
            synthesize_vgui_register_table(db, file_id, call);
        }

        for call in &metadata.derma_define_control_calls {
            let register_position = call.syntax_id.get_range().start();
            let panel_source = call.vgui_panel_define_arg_source();
            let table_source = call.vgui_panel_table_arg_source(2);
            if let Some(GmodClassCallLiteral::String(panel_name)) =
                call.value_for_arg_source(&panel_source)
            {
                if let Some(GmodClassCallLiteral::NameRef(table_var)) =
                    call.value_for_arg_source(&table_source)
                    && let Some((decl_id, region_start)) =
                        resolve_local_registration_region(db, file_id, table_var, register_position)
                {
                    vgui_registration_regions.push(VguiRegistrationRegion {
                        file_id,
                        decl_id,
                        class_decl_id: LuaTypeDeclId::global(panel_name),
                        panel_name: panel_name.clone(),
                        region_start,
                        region_end: register_position,
                    });
                }
            }
            synthesize_derma_define_control(db, file_id, call);
        }

        for call in &metadata.vgui_register_file_calls {
            if let Some((
                target_file_id,
                decl_id,
                class_decl_id,
                panel_name,
                region_start,
                region_end,
            )) = synthesize_vgui_register_file_target(db, file_id, call)
            {
                vgui_registration_regions.push(VguiRegistrationRegion {
                    file_id: target_file_id,
                    decl_id,
                    class_decl_id,
                    panel_name,
                    region_start,
                    region_end,
                });
            }
        }
    }

    // Synthesize AccessorFunc members for VGUI-registered classes
    for registration in &vgui_registration_regions {
        let metadata = match db
            .get_gmod_class_metadata_index()
            .get_file_metadata(&registration.file_id)
        {
            Some(m) => m.clone(),
            None => continue,
        };

        log::debug!(
            "VGUI AccessorFunc: file {:?} has {} accessor_func_calls for panel={} region={:?}..{:?}",
            registration.file_id,
            metadata.accessor_func_calls.len(),
            registration.panel_name,
            registration.region_start,
            registration.region_end,
        );
        let class_decl_id = registration.class_decl_id.clone();
        for call in &metadata.accessor_func_calls {
            if let Some(Some(GmodClassCallLiteral::NameRef(target_name))) =
                call.literal_args.first()
                && let Some(target_arg) = call.args.first()
            {
                let accessor_position = call.syntax_id.get_range().start();
                let target_decl_id = resolve_local_decl_id_at_position(
                    db,
                    registration.file_id,
                    target_name,
                    target_arg.syntax_id.get_range().start(),
                );

                let matches_registration_target = target_decl_id == Some(registration.decl_id)
                    || (target_decl_id.is_none()
                        && target_name == "PANEL"
                        && registration.decl_id.file_id == registration.file_id);

                if matches_registration_target
                    && accessor_position >= registration.region_start
                    && accessor_position < registration.region_end
                {
                    synthesize_accessor_func(db, registration.file_id, &class_decl_id, call);
                }
            }
        }
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

        for call in &metadata.scripted_ent_register_calls {
            synthesize_scripted_ent_registration(db, file_id, &metadata, call);
        }
    }
}

fn synthesize_scripted_ent_registration(
    db: &mut DbIndex,
    file_id: FileId,
    metadata: &GmodScriptedClassFileMetadata,
    call: &GmodScriptedClassCallMetadata,
) {
    let Some(class_name) = call
        .literal_args
        .get(1)
        .and_then(|arg| arg.as_ref())
        .and_then(|arg| match arg {
            GmodClassCallLiteral::String(name) if !name.is_empty() => Some(name.as_str()),
            _ => None,
        })
    else {
        return;
    };

    let class_decl_id = LuaTypeDeclId::global(class_name);
    if db.get_type_index().get_type_decl(&class_decl_id).is_none() {
        db.get_type_index_mut().add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                call.syntax_id.get_range(),
                class_name.to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::None.into(),
                class_decl_id.clone(),
            ),
        );
    }

    let register_position = call.syntax_id.get_range().start();
    let class_type = LuaType::Def(class_decl_id.clone());

    let (registered_table, region_start, decl_id) = match call
        .literal_args
        .first()
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
                find_registered_table_expr(db, file_id, decl_id, register_position),
                region_start,
                Some(decl_id),
            )
        }
        _ => (
            find_table_expr_for_arg_source(db, file_id, call, &GmodClassCallArgSource::direct(0)),
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
        db.get_type_index_mut().force_bind_type(
            table_syntax_owner,
            LuaTypeCache::InferType(class_type.clone()),
        );
    }

    if let Some(decl_id) = decl_id
        && !decl_has_reassignment(db, file_id, decl_id)
    {
        db.get_type_index_mut()
            .force_bind_type(decl_id.into(), LuaTypeCache::InferType(class_type.clone()));
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
        add_member(db, class_member_owner.clone(), member_id);
    }

    db.get_type_index_mut()
        .replace_table_const_type(&table_range, &class_type);

    let base_name =
        resolve_registered_scripted_ent_base(table_expr.clone(), metadata, register_position);
    if let Some(base_name) = base_name {
        let super_type = LuaType::Ref(LuaTypeDeclId::global(&base_name));
        if super_type != class_type {
            db.get_type_index_mut().add_super_type_if_missing(
                class_decl_id.clone(),
                file_id,
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

fn resolve_registered_scripted_ent_base(
    table_expr: LuaTableExpr,
    metadata: &GmodScriptedClassFileMetadata,
    register_position: TextSize,
) -> Option<String> {
    if let Some(field) = find_table_field_by_name(&table_expr, "Base")
        && let Some(value_expr) = field.get_value_expr()
        && let Some(base_name) = extract_scoped_base_name(&value_expr)
    {
        return Some(base_name);
    }

    metadata
        .define_baseclass_calls
        .iter()
        .rev()
        .find(|call| call.syntax_id.get_range().start() < register_position)
        .and_then(
            |call| match call.literal_args.get(call.inheritance_name_arg_idx()) {
                Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
                    Some(name.clone())
                }
                _ => None,
            },
        )
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

fn synthesize_scoped_base_assignments_with(
    db: &mut DbIndex,
    file_id: FileId,
    root: LuaChunk,
    scope_match: &GmodScopedClassMatch,
) {
    let class_decl_id = ensure_scoped_class_type_decl(
        db,
        file_id,
        &scope_match.class_name,
        &scope_match.global_name,
        root.syntax().text_range(),
    );
    let expected_base_path = format!("{}.Base", scope_match.global_name);

    for assign_stat in root.descendants::<LuaAssignStat>() {
        let (vars, exprs) = assign_stat.get_var_and_expr_list();
        for (idx, var) in vars.into_iter().enumerate() {
            let Some(value_expr) = exprs.get(idx) else {
                continue;
            };

            let Some(access_path) = var.get_access_path() else {
                continue;
            };
            if !access_path.eq_ignore_ascii_case(&expected_base_path) {
                continue;
            }

            let Some(base_name) = extract_scoped_base_name(value_expr) else {
                continue;
            };

            let mapped_base_name = remap_scoped_base_name(&scope_match, &base_name);
            let super_type = LuaType::Ref(LuaTypeDeclId::global(&mapped_base_name));
            if super_type == LuaType::Ref(class_decl_id.clone()) {
                continue;
            }

            let has_super = db
                .get_type_index()
                .get_super_types_iter(&class_decl_id)
                .map(|mut supers| supers.any(|existing_super| existing_super == &super_type))
                .unwrap_or(false);
            if !has_super {
                db.get_type_index_mut()
                    .add_super_type(class_decl_id.clone(), file_id, super_type);
            }
        }
    }
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
            (!value.trim().is_empty()).then_some(value)
        }
        LuaExpr::IndexExpr(index_expr) => {
            let value = index_expr.get_access_path()?;
            (!value.trim().is_empty()).then_some(value)
        }
        _ => None,
    }
}

fn remap_scoped_base_name(scope_match: &GmodScopedClassMatch, base_name: &str) -> String {
    if scope_match.global_name == "ENT"
        && GMOD_ENT_BASE_TO_ENT
            .iter()
            .any(|name| name.eq_ignore_ascii_case(base_name))
    {
        return scope_match.global_name.to_string();
    }

    base_name.to_string()
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

/// Detect wrapper functions that internally call NetworkVar/NetworkVarElement
/// and synthesize Get/Set members from their call sites.
fn synthesize_network_var_wrappers(
    db: &mut DbIndex,
    scripted_scope_files: &HashSet<FileId>,
    tree_map: &HashMap<FileId, LuaChunk>,
) {
    // Sort by FileId for deterministic iteration order
    let mut sorted_entries: Vec<_> = tree_map.iter().collect();
    sorted_entries.sort_by_key(|(fid, _)| fid.id);
    for (file_id, root) in sorted_entries {
        if !scripted_scope_files.contains(file_id) {
            continue;
        }

        // Use cached scoped class info (computed earlier in gmod_pre per-file loop)
        let Some(scope_match) = db
            .get_gmod_infer_index()
            .get_scoped_class_info(file_id)
            .cloned()
        else {
            continue;
        };

        let class_decl_id =
            get_scripted_class_type_decl_id(&scope_match.global_name, &scope_match.class_name);

        // Step 1: Collect wrapper definitions from method definitions
        let wrappers = collect_network_var_wrappers(root, &scope_match.global_name);
        if wrappers.is_empty() {
            continue;
        }

        // Step 2: Find calls to these wrappers and synthesize members
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

            synthesize_from_wrapper_call(db, *file_id, &class_decl_id, wrapper, &call_expr);
        }
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

    for call_expr in block.syntax().descendants().filter_map(LuaCallExpr::cast) {
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
                if let Some(idx) = param_names.iter().position(|p| p == &name) {
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
    db.get_type_index_mut().bind_type(
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(getter_func))),
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
    db.get_type_index_mut().bind_type(
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(setter_func))),
    );
}

fn synthesize_inheritance_base(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    base_name: &str,
    is_derive: bool,
    class_name_prefix: Option<&str>,
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
    db.get_type_index_mut()
        .add_super_type_if_missing(class_decl_id.clone(), file_id, super_type);
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
    ensure_scoped_class_type_decl(db, file_id, base_name, "GM", range);
}

fn resolve_effective_inheritance_call(
    metadata: &GmodScriptedClassFileMetadata,
) -> Option<&GmodScriptedClassCallMetadata> {
    metadata
        .derive_gamemode_calls
        .iter()
        .rev()
        .find(|call| valid_inheritance_literal(call))
        .or_else(|| {
            metadata
                .define_baseclass_calls
                .iter()
                .rev()
                .find(|call| valid_inheritance_literal(call))
        })
}

fn valid_inheritance_literal(call: &GmodScriptedClassCallMetadata) -> bool {
    matches!(
        call.literal_args.get(call.inheritance_name_arg_idx()),
        Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty()
    )
}

fn resolve_effective_inheritance_base(
    metadata: &GmodScriptedClassFileMetadata,
    class_name_prefix: Option<&str>,
) -> Option<(String, bool)> {
    let call = resolve_effective_inheritance_call(metadata)?;
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
        ));
    }

    Some((base_name.to_string(), false))
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
    // let their definition win — don't shadow it with a synthetic decl.
    if db
        .get_member_index()
        .get_member_item(&owner, &member_key)
        .is_some()
    {
        return;
    }

    // Prefer the base-name string argument's syntax id for provenance; fall
    // back to the call itself so hover/goto still lands somewhere useful.
    let syntax_id = call
        .args
        .first()
        .map(|a| a.syntax_id)
        .unwrap_or(call.syntax_id);

    let member_id = LuaMemberId::new(syntax_id, file_id);
    let member = LuaMember::new(member_id, member_key, LuaMemberFeature::FileFieldDecl, None);
    db.get_member_index_mut().add_member(owner, member);
    db.get_type_index_mut().bind_type(
        member_id.into(),
        LuaTypeCache::DocType(LuaType::Ref(LuaTypeDeclId::global(base_name))),
    );
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
            db.get_type_index_mut()
                .bind_type(member_id.into(), LuaTypeCache::DocType(value_type.clone()));
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
        db.get_type_index_mut().bind_type(
            member_id.into(),
            LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(getter_func))),
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
    db.get_type_index_mut().bind_type(
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(setter_func))),
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
        db.get_type_index_mut().bind_type(
            member_id.into(),
            LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(getter_func))),
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
    db.get_type_index_mut().bind_type(
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(setter_func))),
    );
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
        db.get_type_index_mut().bind_type(
            member_id.into(),
            LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(getter_func))),
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
    db.get_type_index_mut().bind_type(
        member_id.into(),
        LuaTypeCache::DocType(LuaType::DocFunction(Arc::new(setter_func))),
    );
}

fn synthesize_vgui_register(
    db: &mut DbIndex,
    file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
) {
    // vgui.Register("PanelName", TABLE, "BasePanel")
    // args[0] = panel name (string)
    // args[1] = table variable (name ref)
    // args[2] = base panel name (string)
    let panel_source = call.vgui_panel_define_arg_source();
    let table_source = call.vgui_panel_table_arg_source(1);
    let base_source = call.vgui_panel_base_arg_source(Some(2));

    let panel_name = match call.value_for_arg_source(&panel_source) {
        Some(GmodClassCallLiteral::String(name)) if !name.is_empty() => name.clone(),
        _ => return,
    };

    let table_var_name = match call.value_for_arg_source(&table_source) {
        Some(GmodClassCallLiteral::NameRef(name)) => Some(name.clone()),
        _ => None,
    };

    let base_panel = match base_source
        .as_ref()
        .and_then(|source| call.value_for_arg_source(source))
    {
        Some(GmodClassCallLiteral::String(name)) if !name.is_empty() => Some(name.clone()),
        _ => None,
    };

    synthesize_panel_class(
        db,
        file_id,
        &panel_name,
        table_var_name.as_deref(),
        base_panel.as_deref(),
        GmodScriptedClassCallKind::VguiRegister,
        call,
    );
}

fn synthesize_derma_define_control(
    db: &mut DbIndex,
    file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
) {
    // derma.DefineControl("ControlName", "description", TABLE, "BasePanel")
    // args[0] = control name (string)
    // args[1] = description (string, ignored)
    // args[2] = table variable (name ref)
    // args[3] = base panel name (string)
    let panel_source = call.vgui_panel_define_arg_source();
    let table_source = call.vgui_panel_table_arg_source(2);
    let base_source = call.vgui_panel_base_arg_source(Some(3));

    let control_name = match call.value_for_arg_source(&panel_source) {
        Some(GmodClassCallLiteral::String(name)) if !name.is_empty() => name.clone(),
        _ => return,
    };

    let table_var_name = match call.value_for_arg_source(&table_source) {
        Some(GmodClassCallLiteral::NameRef(name)) => Some(name.clone()),
        _ => None,
    };

    let base_panel = match base_source
        .as_ref()
        .and_then(|source| call.value_for_arg_source(source))
    {
        Some(GmodClassCallLiteral::String(name)) if !name.is_empty() => Some(name.clone()),
        _ => None,
    };

    synthesize_panel_class(
        db,
        file_id,
        &control_name,
        table_var_name.as_deref(),
        base_panel.as_deref(),
        GmodScriptedClassCallKind::DermaDefineControl,
        call,
    );

    // Register the control name as a global variable with the panel type
    register_global_panel(db, file_id, &control_name, call);
}

fn synthesize_vgui_register_table(
    db: &mut DbIndex,
    file_id: FileId,
    call: &GmodScriptedClassCallMetadata,
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

    let base_panel = match base_source
        .as_ref()
        .and_then(|source| call.value_for_arg_source(source))
    {
        Some(GmodClassCallLiteral::String(name)) if !name.is_empty() => Some(name.clone()),
        _ => None,
    };

    let class_decl_id = vgui_register_table_type_decl_id(file_id, call);
    let class_name = class_decl_id.get_simple_name().to_string();
    synthesize_panel_class_with_id(
        db,
        file_id,
        class_decl_id,
        &class_name,
        table_var_name.as_deref(),
        base_panel.as_deref(),
        GmodScriptedClassCallKind::VguiRegisterTable,
        call,
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
    let base_panel = find_target_panel_base_assignment(db, target_file_id)?;

    let class_decl_id = LuaTypeDeclId::local(
        target_file_id,
        &format!("__gmod_vgui_register_file_{}", target_file_id.id),
    );
    let panel_name = class_decl_id.get_simple_name().to_string();
    let class_type = LuaType::Def(class_decl_id.clone());

    if db.get_type_index().get_type_decl(&class_decl_id).is_none() {
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
                panel_name.clone(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::AutoGenerated.into(),
                class_decl_id.clone(),
            ),
        );
    }

    let super_type = LuaType::Ref(LuaTypeDeclId::global(&base_panel));
    let has_super = db
        .get_type_index()
        .get_super_types_iter(&class_decl_id)
        .map(|mut supers| supers.any(|existing_super| existing_super == &super_type))
        .unwrap_or(false);
    if !has_super {
        db.get_type_index_mut()
            .add_super_type(class_decl_id.clone(), target_file_id, super_type);
    }

    let target_panel_decl_ids = ensure_register_file_panel_decls(db, target_file_id)?;
    let panel_decl_id = *target_panel_decl_ids.first()?;
    for decl_id in target_panel_decl_ids {
        db.get_type_index_mut()
            .force_bind_type(decl_id.into(), LuaTypeCache::InferType(class_type.clone()));
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
        add_member(db, class_owner.clone(), member_id);
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

    if let Some(decl_tree) = db.get_decl_index_mut().get_decl_tree_mut(&file_id) {
        decl_tree.add_decl(panel_decl);
    }
    db.get_global_index_mut().add_global_decl("PANEL", decl_id);

    Some(vec![decl_id])
}

fn find_target_panel_base_assignment(db: &DbIndex, file_id: FileId) -> Option<String> {
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
                base_name = Some(name);
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

fn vgui_register_table_type_decl_id(
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

    // Add the declaration to the declaration tree
    if let Some(decl_tree) = db.get_decl_index_mut().get_decl_tree_mut(&file_id) {
        decl_tree.add_decl(global_decl);
    }

    // Register the global in the global index
    db.get_global_index_mut()
        .add_global_decl(panel_name, decl_id);

    // Bind the panel class type to the global declaration
    db.get_type_index_mut().force_bind_type(
        decl_id.into(),
        LuaTypeCache::InferType(LuaType::Def(class_decl_id)),
    );
}

// REMOVED: find_table_type_for_register — it fell back to the shared decl-level
// type cache, which is exactly the position-insensitive slot that caused
// reassigned-PANEL collapse. Resolution now goes through the concrete table
// expression (find_registered_table_expr) instead.

/// Locate the concrete table-constructor (`{}`) expression that backs the
/// variable being registered, by scanning to the variable's latest write
/// before the register call and taking the matching RHS expression.
///
/// VGUI files commonly reuse a single `local PANEL` decl with repeated plain
/// reassignments (`PANEL = {}`), one per registered class. The class identity
/// belongs to each individual table value, not to the shared decl slot — so we
/// resolve the exact `{}` literal at the latest write position and return its
/// table range plus syntax id. Callers bind the synthesized class to that
/// `SyntaxId`, which the public `infer_expr` override consults, giving correct
/// per-region resolution for hover/diagnostics/CodeLens alike.
///
/// Returns `None` (caller skips SyntaxId binding) when the RHS is not a table
/// literal (e.g. `PANEL = make()`, `PANEL = SomeOther`), keeping behavior
/// conservative for non-literal table values.
fn find_registered_table_expr(
    db: &DbIndex,
    file_id: FileId,
    decl_id: LuaDeclId,
    register_position: TextSize,
) -> Option<LuaTableExpr> {
    // The latest write position is the start of the assigned name range for the
    // most recent plain reassignment (`PANEL = {}`) before the register call.
    //
    // The original `local PANEL = {}` declaration is NOT recorded as a write
    // reference cell (only later assignments are), so for the FIRST region
    // there is no prior write — fall back to the decl's own position, where the
    // enclosing `LuaLocalStat` yields the initializer table RHS.
    let write_position =
        find_latest_decl_write_before_position(db, file_id, decl_id, register_position)
            .unwrap_or(decl_id.position);

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

fn find_latest_decl_write_before_position(
    db: &DbIndex,
    file_id: FileId,
    decl_id: LuaDeclId,
    position: TextSize,
) -> Option<TextSize> {
    db.get_reference_index()
        .get_decl_references(&file_id, &decl_id)
        .and_then(|decl_references| {
            decl_references
                .cells
                .iter()
                .filter(|cell| cell.is_write && cell.range.start() < position)
                .max_by_key(|cell| cell.range.start())
                .map(|cell| cell.range.start())
        })
}

fn synthesize_panel_class(
    db: &mut DbIndex,
    file_id: FileId,
    panel_name: &str,
    table_var_name: Option<&str>,
    base_panel: Option<&str>,
    call_kind: GmodScriptedClassCallKind,
    call: &GmodScriptedClassCallMetadata,
) {
    let class_decl_id = LuaTypeDeclId::global(panel_name);
    synthesize_panel_class_with_id(
        db,
        file_id,
        class_decl_id,
        panel_name,
        table_var_name,
        base_panel,
        call_kind,
        call,
    );
}

fn synthesize_panel_class_with_id(
    db: &mut DbIndex,
    file_id: FileId,
    class_decl_id: LuaTypeDeclId,
    panel_name: &str,
    table_var_name: Option<&str>,
    base_panel: Option<&str>,
    call_kind: GmodScriptedClassCallKind,
    call: &GmodScriptedClassCallMetadata,
) {
    // Create the class type declaration if it doesn't exist
    if db.get_type_index().get_type_decl(&class_decl_id).is_none() {
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
                panel_name.to_string(),
                LuaDeclTypeKind::Class,
                type_flag.into(),
                class_decl_id.clone(),
            ),
        );
    }

    // Set super type from base panel
    if let Some(base_name) = base_panel {
        let super_type = LuaType::Ref(LuaTypeDeclId::global(base_name));
        let has_super = db
            .get_type_index()
            .get_super_types_iter(&class_decl_id)
            .map(|mut supers| supers.any(|existing_super| existing_super == &super_type))
            .unwrap_or(false);
        if !has_super {
            db.get_type_index_mut()
                .add_super_type(class_decl_id.clone(), file_id, super_type);
        }
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
        let Some((decl_id, region_start)) =
            resolve_local_registration_region(db, file_id, var_name, register_position)
        else {
            return;
        };

        let class_type = LuaType::Def(class_decl_id.clone());
        let latest_write_position = Some(region_start);

        // Resolve the concrete `{}` table literal backing this registration.
        let registered_table = find_registered_table_expr(db, file_id, decl_id, register_position);

        if let Some(table_expr) = &registered_table {
            // Bind the class to this exact table-constructor expression.
            // Preserve any user `@as`/cast (DocType) binding already present.
            let table_syntax_owner =
                LuaTypeOwner::SyntaxId(InFiled::new(file_id, table_expr.get_syntax_id()));
            let preserve_doc = db
                .get_type_index()
                .get_type_cache(&table_syntax_owner)
                .is_some_and(|cache| cache.is_doc());
            if !preserve_doc {
                db.get_type_index_mut().force_bind_type(
                    table_syntax_owner,
                    LuaTypeCache::InferType(class_type.clone()),
                );
            }
        }

        if !decl_has_reassignment(db, file_id, decl_id) {
            // For single-panel files the `PANEL` local has one stable identity.
            // Bind the decl slot too so method-self collection during the Lua
            // pass sees the synthesized class before it caches member values.
            // Reassigned locals remain table-literal-only to avoid collapsing
            // distinct registration regions onto one class.
            db.get_type_index_mut()
                .force_bind_type(decl_id.into(), LuaTypeCache::InferType(class_type.clone()));
        }

        // Transfer the members defined in this registration's table region to
        // the class, then rewrite that exact table-const range so persistent
        // type caches (cross-file accesses, exports) resolve to the class.
        if let Some(table_expr) = &registered_table {
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
                collect_panel_member_source_ranges(db, file_id, decl_id, &table_range);

            let mut table_member_ids = HashSet::new();
            for (source_idx, source_range) in member_source_ranges.iter().enumerate() {
                let is_initializer_fallback = source_idx > 0;
                let source_owner = LuaMemberOwner::Element(source_range.clone());
                if let Some(members) = db.get_member_index().get_members(&source_owner) {
                    for member in members {
                        let member_position = member.get_id().get_position();
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
                            table_member_ids.insert(member.get_id());
                        }
                    }
                }
            }

            for member_id in table_member_ids {
                add_member(db, class_member_owner.clone(), member_id);
            }

            // Backfill persistent type caches that still hold this exact
            // table-const identity (scoped to the current range only — never
            // carried forward across registrations).
            db.get_type_index_mut()
                .replace_table_const_type(&table_range, &class_type);
        }
    } else if let Some(table_expr) = find_inline_vgui_panel_table_expr(db, file_id, call_kind, call)
    {
        bind_inline_vgui_panel_table(db, file_id, &class_decl_id, table_expr);
    }
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
    let arg_range = if table_source.field_path.is_empty() {
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
    file_id: FileId,
    class_decl_id: &LuaTypeDeclId,
    table_expr: LuaTableExpr,
) {
    let class_type = LuaType::Def(class_decl_id.clone());
    let table_syntax_owner =
        LuaTypeOwner::SyntaxId(InFiled::new(file_id, table_expr.get_syntax_id()));
    let preserve_doc = db
        .get_type_index()
        .get_type_cache(&table_syntax_owner)
        .is_some_and(|cache| cache.is_doc());
    if !preserve_doc {
        db.get_type_index_mut().force_bind_type(
            table_syntax_owner,
            LuaTypeCache::InferType(class_type.clone()),
        );
    }

    let table_range = InFiled::new(file_id, table_expr.get_range());
    let source_owner = LuaMemberOwner::Element(table_range.clone());
    let class_member_owner = LuaMemberOwner::Type(class_decl_id.clone());
    let table_member_ids: Vec<_> = db
        .get_member_index()
        .get_members(&source_owner)
        .map(|members| members.iter().map(|member| member.get_id()).collect())
        .unwrap_or_default();

    for member_id in table_member_ids {
        add_member(db, class_member_owner.clone(), member_id);
    }

    db.get_type_index_mut()
        .replace_table_const_type(&table_range, &class_type);
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
    // We derive this range from the AST rather than the decl type cache: the
    // cache is rewritten in-place by `replace_table_const_type` as each region
    // is synthesized, so by the second registration the original decl's cache
    // no longer reports its initializer `TableConst`.
    if let Some(initializer_range) = find_decl_initializer_table_range(db, file_id, decl_id)
        && !ranges.iter().any(|existing| existing == &initializer_range)
    {
        ranges.push(initializer_range);
    }

    ranges
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
    let decl_position = decl_id.position;
    db.get_reference_index()
        .get_decl_references(&file_id, &decl_id)
        .map(|decl_references| {
            decl_references
                .cells
                .iter()
                .any(|cell| cell.is_write && cell.range.start() != decl_position)
        })
        .unwrap_or(false)
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
            let (vars, _) = assign_stat.get_var_and_expr_list();
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
    let owner = LuaMemberOwner::Type(class_decl_id.clone());
    let member_key = LuaMemberKey::Name("BaseClass".into());
    if db
        .get_member_index()
        .get_member_item(&owner, &member_key)
        .is_some()
    {
        return;
    }

    let base_arg_source = match call_kind {
        GmodScriptedClassCallKind::VguiRegister => call.vgui_panel_base_arg_source(Some(2)),
        GmodScriptedClassCallKind::VguiRegisterTable => call.vgui_panel_base_arg_source(Some(1)),
        GmodScriptedClassCallKind::DermaDefineControl => call.vgui_panel_base_arg_source(Some(3)),
        _ => return,
    };

    let syntax_id = base_arg_source
        .as_ref()
        .and_then(|source| {
            if source.field_path.is_empty() {
                call.args.get(source.arg_idx).map(|arg| arg.syntax_id)
            } else {
                call.field_args
                    .iter()
                    .find(|arg| &arg.source == source)
                    .map(|arg| arg.syntax_id)
            }
        })
        .unwrap_or(call.syntax_id);
    let member_id = LuaMemberId::new(syntax_id, file_id);
    let member = LuaMember::new(member_id, member_key, LuaMemberFeature::FileFieldDecl, None);
    db.get_member_index_mut().add_member(owner, member);
    db.get_type_index_mut().bind_type(
        member_id.into(),
        LuaTypeCache::DocType(LuaType::Ref(LuaTypeDeclId::global(base_name))),
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
    db.get_emmyrc()
        .gmod
        .scripted_class_scopes
        .detect_class_for_path(file_path)
        .map(|scope_match| GmodScopedClassMatch {
            global_name: scope_match.definition.class_global,
            class_name: scope_match.class_name,
            class_name_prefix: scope_match.definition.class_name_prefix,
        })
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Default)]
struct AnnotatedGmodGlobalCallRoleMap {
    roles_by_path: HashMap<String, AnnotatedGmodCallRoles>,
    candidate_call_path_matcher: Option<AhoCorasick>,
    candidate_call_path_kinds: Vec<AnnotatedGmodCandidatePresence>,
}

struct AnnotatedGmodCallRoleMap<'a> {
    global_roles: &'a AnnotatedGmodGlobalCallRoleMap,
    local_roles_by_decl: HashMap<LuaDeclId, AnnotatedGmodCallRoles>,
    local_roles_by_path: HashMap<(LuaDeclId, String), AnnotatedGmodCallRoles>,
    local_candidate_names: HashSet<String>,
}

#[derive(Clone, Default)]
struct AnnotatedGmodCallArgRole {
    param_idx: usize,
    priority: i64,
    field_path: Vec<String>,
}

impl AnnotatedGmodCallArgRole {
    fn from_role(role: &LuaCallArgRole) -> Self {
        Self {
            param_idx: role.param_idx,
            priority: role.priority.unwrap_or(0),
            field_path: role.field_path.clone(),
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
        })
    }
}

#[derive(Clone, Default)]
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
    inheritance_roles: Vec<(GmodScriptedClassCallKind, AnnotatedGmodCallArgRole)>,
    network_var_kind: Option<GmodScriptedClassCallKind>,
    network_var_type_roles: Vec<AnnotatedGmodCallArgRole>,
    network_var_define_roles: Vec<AnnotatedGmodCallArgRole>,
    vgui_panel_kind: Option<GmodScriptedClassCallKind>,
    vgui_panel_define_roles: Vec<AnnotatedGmodCallArgRole>,
    vgui_panel_table_roles: Vec<AnnotatedGmodCallArgRole>,
    vgui_panel_base_roles: Vec<AnnotatedGmodCallArgRole>,
    derma_skin_define_roles: Vec<AnnotatedGmodCallArgRole>,
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
            ("gmod.derma_skin", "define") => {
                self.derma_skin_define_roles.push(arg_role);
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
        self.derma_skin_define_roles
            .sort_by_key(AnnotatedGmodCallArgRole::sort_key);
    }

    fn has_any_roles(&self) -> bool {
        !self.system_roles.is_empty()
            || !self.system_callback_roles.is_empty()
            || !self.hook_roles.is_empty()
            || !self.hook_callback_roles.is_empty()
            || !self.load_roles.is_empty()
            || !self.inheritance_roles.is_empty()
            || !self.network_var_define_roles.is_empty()
            || !self.vgui_panel_define_roles.is_empty()
            || matches!(
                self.vgui_panel_kind,
                Some(
                    GmodScriptedClassCallKind::VguiRegisterFile
                        | GmodScriptedClassCallKind::VguiRegisterTable
                )
            )
            || !self.derma_skin_define_roles.is_empty()
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
        AnnotatedGmodCandidatePresence {
            has_system: !self.system_roles.is_empty() || !self.system_callback_roles.is_empty(),
            has_net: self.system_roles.iter().any(|(kind, _)| {
                matches!(
                    kind,
                    GmodSystemCallKind::AddNetworkString
                        | GmodSystemCallKind::NetStart
                        | GmodSystemCallKind::NetReceive
                )
            }),
            has_hook: !self.hook_roles.is_empty() || !self.hook_callback_roles.is_empty(),
            has_load: !self.load_roles.is_empty(),
            has_scripted_class: !self.inheritance_roles.is_empty()
                || !self.network_var_define_roles.is_empty()
                || !self.vgui_panel_define_roles.is_empty()
                || matches!(
                    self.vgui_panel_kind,
                    Some(
                        GmodScriptedClassCallKind::VguiRegisterFile
                            | GmodScriptedClassCallKind::VguiRegisterTable
                    )
                )
                || !self.derma_skin_define_roles.is_empty(),
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
        let (kind, role) = self.load_roles.first()?;
        Some((
            *kind,
            param_idx_to_call_arg_idx(role.param_idx, is_colon_call, self.is_colon_define)?,
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
            for typ in union.into_vec() {
                match static_arg_kind_matches_type(kind, &typ) {
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

impl AnnotatedGmodGlobalCallRoleMap {
    fn build(db: &DbIndex) -> Self {
        let mut role_map = Self::default();
        for (signature_id, signature) in db.get_signature_index().iter() {
            if !signature.has_call_arg_roles() {
                continue;
            }
            let Some(closure) = closure_from_signature_id(db, *signature_id) else {
                continue;
            };
            role_map.add_signature_closure(db, *signature_id, &closure);
        }
        role_map.rebuild_candidate_call_path_set();

        role_map
    }

    fn rebuild_candidate_call_path_set(&mut self) {
        let mut call_paths = Vec::new();
        self.candidate_call_path_kinds.clear();

        for (call_path, roles) in &self.roles_by_path {
            let presence = roles.candidate_presence();
            if !presence.has_system
                && !presence.has_net
                && !presence.has_hook
                && !presence.has_scripted_class
                && !presence.has_load
            {
                continue;
            }

            call_paths.push(call_path.as_str());
            self.candidate_call_path_kinds.push(presence);
        }

        self.candidate_call_path_matcher = if call_paths.is_empty() {
            None
        } else {
            AhoCorasick::new(call_paths).ok()
        };
    }

    fn candidate_call_paths_in_content(&self, content: &str) -> AnnotatedGmodCandidatePresence {
        let mut presence = AnnotatedGmodCandidatePresence::default();
        let Some(matcher) = &self.candidate_call_path_matcher else {
            return presence;
        };

        for mat in matcher.find_iter(content) {
            let Some(candidate_presence) =
                self.candidate_call_path_kinds.get(mat.pattern().as_usize())
            else {
                continue;
            };

            presence.has_system |= candidate_presence.has_system;
            presence.has_net |= candidate_presence.has_net;
            presence.has_hook |= candidate_presence.has_hook;
            presence.has_scripted_class |= candidate_presence.has_scripted_class;
            presence.has_load |= candidate_presence.has_load;

            if presence.has_system
                && presence.has_net
                && presence.has_hook
                && presence.has_scripted_class
                && presence.has_load
            {
                break;
            }
        }

        presence
    }

    fn add_signature_closure(
        &mut self,
        db: &DbIndex,
        signature_id: LuaSignatureId,
        closure: &LuaClosureExpr,
    ) {
        let Some(call_path) = global_call_path_for_signature_closure(db, signature_id, closure)
        else {
            return;
        };
        if let Some(roles) = roles_from_signature(db, signature_id) {
            self.roles_by_path.insert(call_path.clone(), roles.clone());
            if let Some(global_path) = call_path.strip_prefix("_G.") {
                self.roles_by_path.insert(global_path.to_string(), roles);
            }
        }
    }

    fn get(&self, call_path: &str) -> Option<AnnotatedGmodCallRoles> {
        self.roles_by_path
            .get(call_path)
            .or_else(|| {
                call_path
                    .strip_prefix("_G.")
                    .and_then(|global_path| self.roles_by_path.get(global_path))
            })
            .cloned()
    }

    fn contains(&self, call_path: &str) -> bool {
        self.roles_by_path.contains_key(call_path)
            || call_path
                .strip_prefix("_G.")
                .is_some_and(|global_path| self.roles_by_path.contains_key(global_path))
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
            local_roles_by_decl: HashMap::new(),
            local_roles_by_path: HashMap::new(),
            local_candidate_names: HashSet::new(),
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
            role_map.add_local_path_roles(root_decl_id, call_path, roles);
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
        if let Some(local_path_roles) =
            annotated_roles_from_local_call_path(self, db, file_id, call_expr, call_path)
        {
            return local_path_roles.and_then(|roles| roles.select_for_call(call_expr));
        }

        if self.global_roles.contains(call_path) {
            if let Some(local_roles) = annotated_roles_from_local_call_prefix(
                self,
                db,
                file_id,
                call_expr.get_prefix_expr(),
            ) {
                return local_roles.and_then(|roles| roles.select_for_call(call_expr));
            }

            if call_expr_has_shadowing_local_root(db, file_id, call_expr) {
                return None;
            }

            return self
                .global_roles
                .get(call_path)
                .and_then(|roles| roles.select_for_call(call_expr));
        }

        if !self.local_candidate_names.contains(call_path) {
            return None;
        }

        annotated_roles_from_local_call_prefix(self, db, file_id, call_expr.get_prefix_expr())?
            .and_then(|roles| roles.select_for_call(call_expr))
    }
}

fn roles_from_signature(
    db: &DbIndex,
    signature_id: LuaSignatureId,
) -> Option<AnnotatedGmodCallRoles> {
    let signature = db.get_signature_index().get(&signature_id)?;
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
        .get_syntax_tree(&signature_id.get_file_id())?
        .get_red_root();
    root.descendants()
        .filter_map(LuaClosureExpr::cast)
        .find(|closure| closure.get_position() == signature_id.get_position())
}

fn global_call_path_for_signature_closure(
    db: &DbIndex,
    signature_id: LuaSignatureId,
    closure: &LuaClosureExpr,
) -> Option<String> {
    let file_id = signature_id.get_file_id();
    if let Some(func_stat) = closure.get_parent::<LuaFuncStat>() {
        let func_name = func_stat.get_func_name()?;
        return var_expr_has_global_root(db, file_id, &func_name)
            .then(|| func_name.get_access_path())?;
    }

    let assign_stat = closure.get_parent::<LuaAssignStat>()?;
    let (vars, value_exprs) = assign_stat.get_var_and_expr_list();
    let value_idx = value_exprs
        .iter()
        .position(|expr| expr.get_position() == closure.get_position())?;
    let var_expr = vars.get(value_idx)?;
    var_expr_has_global_root(db, file_id, var_expr).then(|| var_expr.get_access_path())?
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
    let Some((ret_idx, initializer)) = local_decl_initializer_expr(db, decl_id) else {
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

fn local_decl_initializer_expr(db: &DbIndex, decl_id: LuaDeclId) -> Option<(usize, LuaExpr)> {
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let initializer = decl.get_initializer()?;
    let root = db
        .get_vfs()
        .get_syntax_tree(&decl_id.file_id)?
        .get_red_root();
    let node = initializer.get_expr_syntax_id().to_node_from_root(&root)?;
    Some((initializer.get_ret_idx(), LuaExpr::cast(node)?))
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

    let Some(signature_id) = signature_id_from_decl_value(db, decl_id) else {
        return Some(None);
    };
    Some(roles_from_signature(db, signature_id))
}

fn signature_id_from_decl_value(db: &DbIndex, decl_id: LuaDeclId) -> Option<LuaSignatureId> {
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let value_syntax_id = decl.get_value_syntax_id()?;
    let root = db
        .get_vfs()
        .get_syntax_tree(&decl_id.file_id)?
        .get_red_root();
    let value_node = value_syntax_id.to_node_from_root(&root)?;
    let closure = LuaClosureExpr::cast(value_node)?;
    Some(LuaSignatureId::from_closure(decl_id.file_id, &closure))
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
    db: &mut DbIndex,
    file_id: FileId,
    annotated_roles: &AnnotatedGmodCallRoleMap,
    call_expr: LuaCallExpr,
) -> Option<()> {
    let call_path = call_expr.get_access_path()?;

    if let Some((kind, inheritance_roles)) =
        annotated_roles.inheritance_call(db, file_id, &call_expr, &call_path)
    {
        let (literal_args, args, field_args) =
            extract_gmod_class_call_args(db, file_id, &call_expr, &[]);
        db.get_gmod_class_metadata_index_mut().add_call(
            file_id,
            kind,
            GmodScriptedClassCallMetadata {
                syntax_id: call_expr.get_syntax_id(),
                literal_args,
                args,
                field_args,
                inheritance_roles: Some(inheritance_roles),
                network_var_roles: None,
                vgui_panel_roles: None,
                derma_skin_roles: None,
            },
        );
        return Some(());
    }

    if let Some((kind, network_var_roles)) =
        annotated_roles.network_var_call(db, file_id, &call_expr, &call_path)
    {
        let (literal_args, args, field_args) =
            extract_gmod_class_call_args(db, file_id, &call_expr, &[]);
        db.get_gmod_class_metadata_index_mut().add_call(
            file_id,
            kind,
            GmodScriptedClassCallMetadata {
                syntax_id: call_expr.get_syntax_id(),
                literal_args,
                args,
                field_args,
                inheritance_roles: None,
                network_var_roles: Some(network_var_roles),
                vgui_panel_roles: None,
                derma_skin_roles: None,
            },
        );
        return Some(());
    }

    if let Some((kind, vgui_panel_roles)) =
        annotated_roles.vgui_panel_call(db, file_id, &call_expr, &call_path)
    {
        let field_sources = vgui_panel_field_sources(&vgui_panel_roles);
        let (literal_args, args, field_args) =
            extract_gmod_class_call_args(db, file_id, &call_expr, &field_sources);
        db.get_gmod_class_metadata_index_mut().add_call(
            file_id,
            kind,
            GmodScriptedClassCallMetadata {
                syntax_id: call_expr.get_syntax_id(),
                literal_args,
                args,
                field_args,
                inheritance_roles: None,
                network_var_roles: None,
                vgui_panel_roles: Some(vgui_panel_roles),
                derma_skin_roles: None,
            },
        );
        return Some(());
    }

    if let Some(derma_skin_roles) =
        annotated_roles.derma_skin_call(db, file_id, &call_expr, &call_path)
    {
        let (literal_args, args, field_args) =
            extract_gmod_class_call_args(db, file_id, &call_expr, &[]);
        db.get_gmod_class_metadata_index_mut().add_call(
            file_id,
            GmodScriptedClassCallKind::DermaDefineSkin,
            GmodScriptedClassCallMetadata {
                syntax_id: call_expr.get_syntax_id(),
                literal_args,
                args,
                field_args,
                inheritance_roles: None,
                network_var_roles: None,
                vgui_panel_roles: None,
                derma_skin_roles: Some(derma_skin_roles),
            },
        );
        return Some(());
    }

    None
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
        let value = extract_gmod_class_literal_or_name(arg_expr);
        literal_args.push(value.clone());
        args.push(crate::GmodClassCallArg { syntax_id, value });
    }

    let mut field_args = Vec::new();
    for source in field_sources {
        if source.field_path.is_empty() {
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
        field_args.push(crate::GmodClassCallFieldArg {
            source: source.clone(),
            syntax_id: value_expr.get_syntax_id(),
            value: extract_gmod_class_literal_or_name(&value_expr),
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
        if !source.field_path.is_empty() && !sources.iter().any(|existing| existing == source) {
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
    let (_, initializer) = local_decl_initializer_expr(db, decl_id)?;
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
        mapped_hook_for_emitter_call(db, &call_path, call_expr.clone())
    };
    let (kind, name_arg_idx, callback_arg_idx, mapped_hook_data) =
        if let Some((kind, name_arg_idx, callback_arg_idx)) = annotated_hook {
            (kind, name_arg_idx, callback_arg_idx, None)
        } else if let Some(mapped_hook) = mapped_hook {
            (GmodHookKind::Emit, 0, None, Some(mapped_hook))
        } else {
            return None;
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
    call_path: &str,
    call_expr: LuaCallExpr,
) -> Option<(
    Option<String>,
    Option<rowan::TextRange>,
    Option<GmodHookNameIssue>,
)> {
    for (emitter_path, mapped_hook) in &db.get_emmyrc().gmod.hook_mappings.emitter_to_hook {
        if !matches_configured_call_path(call_path, emitter_path) {
            continue;
        }

        if mapped_hook == "*" {
            return Some(extract_static_hook_name(
                call_expr
                    .get_args_list()
                    .and_then(|args| args.get_args().next()),
            ));
        }

        let trimmed = mapped_hook.trim();
        return Some(if trimmed.is_empty() {
            (None, None, Some(GmodHookNameIssue::Empty))
        } else {
            (Some(trimmed.to_string()), None, None)
        });
    }

    None
}

fn collect_hook_method_site(db: &DbIndex, func_stat: LuaFuncStat) -> Option<GmodHookSiteMetadata> {
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
            || is_configured_method_hook_prefix(db, &prefix_name))
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
                && !is_configured_method_hook_prefix(db, &prefix_name))
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
    matches!(prefix_name, "GM" | "GAMEMODE" | "PLUGIN" | "SANDBOX")
}

fn is_configured_method_hook_prefix(db: &DbIndex, prefix_name: &str) -> bool {
    db.get_emmyrc()
        .gmod
        .hook_mappings
        .method_prefixes
        .iter()
        .any(|configured_prefix| {
            configured_prefix
                .trim()
                .trim_end_matches([':', '.'])
                .eq_ignore_ascii_case(prefix_name)
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
    prefix_name: &str,
    separator: &str,
    method_name: &str,
) -> Option<(Option<String>, Option<GmodHookNameIssue>)> {
    let mappings = &db.get_emmyrc().gmod.hook_mappings.method_to_hook;
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
fn realm_from_condition(expr: &LuaExpr) -> Option<GmodRealm> {
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

fn rebuild_gmod_load_index(
    db: &mut DbIndex,
    branch_realm_ranges: &HashMap<FileId, Vec<GmodRealmRange>>,
    analyzed_file_ids: &[FileId],
) {
    let file_ids = db.get_vfs().get_all_local_file_ids();
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

    let resolved_branch_ranges = file_ids
        .iter()
        .map(|file_id| {
            let ranges = if let Some(ranges) = branch_realm_ranges.get(file_id) {
                ranges.clone()
            } else if analyzed_file_ids.contains(file_id) {
                Vec::new()
            } else {
                previous_realm_metadata
                    .get(file_id)
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
    let mut fallback_masks = HashMap::new();

    for file_id in &file_ids {
        if let Some(realm) = infer_realm_from_load_path_hint(db, *file_id) {
            fallback_masks.insert(*file_id, GmodStateMask::from_realm(realm));
        }

        if let Some((kind, states)) = engine_load_root_for_file(db, *file_id) {
            mark_load_root(&mut file_infos, *file_id, kind, states);
        }
    }

    let dependency_sites = db
        .get_file_dependencies_index()
        .iter_dependency_sites()
        .flat_map(|(_, sites)| sites.iter().cloned())
        .map(|site| resolve_load_dependency_site(db, site))
        .collect::<Vec<_>>();
    let dynamic_loaders = collect_dynamic_loaders(db, &file_ids);

    let mut unresolved_edges = Vec::new();
    for _ in 0..file_ids.len().max(1) {
        let mut changed = false;
        for site in &dependency_sites {
            let source_states = source_states_for_load_site(
                &file_infos,
                &fallback_masks,
                &resolved_branch_ranges,
                site,
            );
            changed |= apply_load_site(&mut file_infos, &mut unresolved_edges, site, source_states);
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

    db.get_gmod_load_index_mut()
        .set_all_file_infos(file_infos, unresolved_edges);
}

struct DynamicLoadPattern {
    source_file_id: FileId,
    glob_base: String,
    has_prefix_dispatch: bool,
    has_addcs: bool,
    has_include: bool,
    range: TextRange,
    targets: Vec<(FileId, String)>,
}

fn collect_dynamic_loaders(db: &DbIndex, file_ids: &[FileId]) -> Vec<DynamicLoadPattern> {
    let relative_paths = file_ids
        .iter()
        .filter_map(|file_id| gmod_relative_path(db, *file_id).map(|path| (*file_id, path)))
        .collect::<HashMap<_, _>>();

    let mut patterns = Vec::new();
    for source_file_id in file_ids {
        let Some(tree) = db.get_vfs().get_syntax_tree(source_file_id) else {
            continue;
        };
        let Some(content) = db.get_vfs().get_file_content(source_file_id) else {
            continue;
        };
        if !content.contains("file.Find") {
            continue;
        }

        let root = tree.get_chunk_node();
        let bindings = collect_static_string_bindings(&root);
        for call_expr in root.descendants::<LuaCallExpr>() {
            if call_expr.get_access_path().as_deref() != Some("file.Find") {
                continue;
            }
            let Some(args) = call_expr.get_args_list() else {
                continue;
            };
            let args = args.get_args().collect::<Vec<_>>();
            let Some(pattern_expr) = args.first() else {
                continue;
            };
            if args.get(1).and_then(static_literal_string).as_deref() != Some("LUA") {
                continue;
            }
            let Some(pattern) = static_string_expr(pattern_expr, &bindings) else {
                continue;
            };
            let Some(glob_base) = lua_file_find_glob_base(&pattern) else {
                continue;
            };
            let targets = relative_paths
                .iter()
                .filter(|(_, target_path)| file_find_glob_matches(&glob_base, target_path))
                .map(|(target_file_id, target_path)| (*target_file_id, target_path.clone()))
                .collect::<Vec<_>>();
            if targets.is_empty() {
                continue;
            }

            patterns.push(DynamicLoadPattern {
                source_file_id: *source_file_id,
                glob_base,
                has_prefix_dispatch: content.contains("\"cl_\"")
                    || content.contains("'cl_'")
                    || content.contains("\"sv_\"")
                    || content.contains("'sv_'")
                    || content.contains("\"sh_\"")
                    || content.contains("'sh_'"),
                has_addcs: content.contains("AddCSLuaFile"),
                has_include: content.contains("include(") || content.contains("IncludeCS"),
                range: call_expr.get_range(),
                targets,
            });
        }
    }

    patterns
}

fn collect_static_string_bindings(root: &LuaChunk) -> HashMap<String, String> {
    let mut bindings = HashMap::new();
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
                    bindings.insert(name, value);
                }
            }
        }
    }
    bindings
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

fn lua_file_find_glob_base(pattern: &str) -> Option<String> {
    let pattern = pattern.replace('\\', "/").to_ascii_lowercase();
    let base = pattern.strip_suffix("/*.lua")?;
    Some(
        base.trim_start_matches("lua/")
            .trim_matches('/')
            .to_string(),
    )
}

fn file_find_glob_matches(glob_base: &str, target_path: &str) -> bool {
    let Some(rest) = target_path.strip_prefix(glob_base) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix('/') else {
        return false;
    };
    !rest.contains('/') && rest.ends_with(".lua")
}

fn apply_dynamic_loaders(
    file_infos: &mut HashMap<FileId, GmodFileLoadInfo>,
    fallback_masks: &HashMap<FileId, GmodStateMask>,
    branch_realm_ranges: &HashMap<FileId, Vec<GmodRealmRange>>,
    dynamic_loaders: &[DynamicLoadPattern],
) -> bool {
    let mut changed = false;
    for loader in dynamic_loaders {
        let site = LuaDependencySite {
            source_file_id: loader.source_file_id,
            target_file_id: None,
            kind: LuaDependencyKind::Include,
            path: Some(format!("{}/*.lua", loader.glob_base)),
            original_expr: "file.Find".to_string(),
            range: loader.range,
        };
        let source_states =
            source_states_for_load_site(file_infos, fallback_masks, branch_realm_ranges, &site);
        if source_states.is_empty() {
            continue;
        }

        for (target_file_id, target_path) in &loader.targets {
            let target_states =
                dynamic_target_states(target_path, source_states, loader.has_prefix_dispatch);
            if target_states.is_empty() {
                continue;
            }

            let target_info = file_infos
                .entry(*target_file_id)
                .or_insert_with(GmodFileLoadInfo::fallback_shared);
            if loader.has_addcs && target_states.intersects(GmodStateMask::CLIENT) {
                target_info.client_send_available = true;
                target_info.add_incoming_edge(GmodLoadEdge {
                    source_file_id: loader.source_file_id,
                    target_file_id: Some(*target_file_id),
                    kind: GmodLoadEdgeKind::DynamicAddCSLuaFile,
                    states: GmodStateMask::CLIENT,
                    path: Some(target_path.clone()),
                    original_expr: Some("file.Find".to_string()),
                    range: Some(loader.range),
                });
            }
            if loader.has_include || target_states.intersects(GmodStateMask::SERVER) {
                target_info.add_incoming_edge(GmodLoadEdge {
                    source_file_id: loader.source_file_id,
                    target_file_id: Some(*target_file_id),
                    kind: GmodLoadEdgeKind::DynamicInclude,
                    states: target_states,
                    path: Some(target_path.clone()),
                    original_expr: Some("file.Find".to_string()),
                    range: Some(loader.range),
                });
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

fn dynamic_target_states(
    target_path: &str,
    source_states: GmodStateMask,
    has_prefix_dispatch: bool,
) -> GmodStateMask {
    if !has_prefix_dispatch {
        return source_states;
    }
    let file_name = target_path.rsplit('/').next().unwrap_or(target_path);
    if file_name.starts_with("cl_") {
        GmodStateMask::CLIENT
    } else if file_name.starts_with("sv_") {
        GmodStateMask::SERVER
    } else if file_name.starts_with("sh_") {
        GmodStateMask::SHARED
    } else {
        source_states
    }
}

fn resolve_load_dependency_site(db: &DbIndex, mut site: LuaDependencySite) -> LuaDependencySite {
    if site.target_file_id.is_some() {
        return site;
    }
    let Some(path) = site.path.as_deref() else {
        return site;
    };
    site.target_file_id = resolve_load_dependency_target(db, site.source_file_id, site.kind, path);
    site
}

fn resolve_load_dependency_target(
    db: &DbIndex,
    source_file_id: FileId,
    dependency_kind: LuaDependencyKind,
    dependency_path: &str,
) -> Option<FileId> {
    let module_index = db.get_module_index();
    match dependency_kind {
        LuaDependencyKind::Require => module_index
            .find_module_for_file(dependency_path, source_file_id)
            .map(|module| module.file_id),
        LuaDependencyKind::Include
        | LuaDependencyKind::AddCSLuaFile
        | LuaDependencyKind::IncludeCS => {
            resolve_load_include_target(db, source_file_id, dependency_path).or_else(|| {
                module_index
                    .find_module_for_file(dependency_path, source_file_id)
                    .map(|module| module.file_id)
            })
        }
    }
}

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
    let root_module_path = normalized_no_ext.replace('/', ".");
    if let Some(module_info) = module_index.find_module_for_file(&root_module_path, source_file_id)
    {
        return Some(module_info.file_id);
    }

    if let Some(path_without_lua_prefix) = normalized_no_ext.strip_prefix("lua/") {
        let module_path = path_without_lua_prefix.replace('/', ".");
        if let Some(module_info) = module_index.find_module_for_file(&module_path, source_file_id) {
            return Some(module_info.file_id);
        }
    }

    let current_file_path = db.get_vfs().get_file_path(&source_file_id)?;
    let parent_dir = current_file_path.parent()?;
    let include_file_path = parent_dir.join(Path::new(normalized_path));
    module_index
        .find_module_by_path_for_file(&include_file_path, source_file_id)
        .map(|module| module.file_id)
}

fn mark_load_root(
    file_infos: &mut HashMap<FileId, GmodFileLoadInfo>,
    file_id: FileId,
    kind: GmodLoadRootKind,
    states: GmodStateMask,
) {
    let info = file_infos
        .entry(file_id)
        .or_insert_with(GmodFileLoadInfo::fallback_shared);
    info.mark_states(
        states,
        GmodLoadStatus::EngineLoaded,
        GmodLoadConfidence::Engine,
    );
    info.add_root(GmodLoadRoot { kind, states });
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

fn apply_load_site(
    file_infos: &mut HashMap<FileId, GmodFileLoadInfo>,
    unresolved_edges: &mut Vec<GmodLoadEdge>,
    site: &LuaDependencySite,
    source_states: GmodStateMask,
) -> bool {
    let edge_kind = GmodLoadEdgeKind::from(site.kind);
    let edge = GmodLoadEdge {
        source_file_id: site.source_file_id,
        target_file_id: site.target_file_id,
        kind: edge_kind,
        states: source_states,
        path: site.path.clone(),
        original_expr: Some(site.original_expr.clone()),
        range: Some(site.range),
    };

    let Some(target_file_id) = site.target_file_id else {
        if !unresolved_edges.contains(&edge) {
            unresolved_edges.push(edge);
        }
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

    target_info.add_incoming_edge(edge);
    changed
}

fn engine_load_root_for_file(
    db: &DbIndex,
    file_id: FileId,
) -> Option<(GmodLoadRootKind, GmodStateMask)> {
    let rel_path = gmod_relative_path(db, file_id)?;
    engine_load_root_for_relative_path(&rel_path)
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
    let default_realm = gmod_config_default_realm(db);
    let detect_filename = db
        .get_emmyrc()
        .gmod
        .detect_realm_from_filename
        .unwrap_or(true);
    let detect_calls = db.get_emmyrc().gmod.detect_realm_from_calls.unwrap_or(true);
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

    if !detect_filename && !detect_calls {
        let realm_metadata = file_ids
            .into_iter()
            .map(|file_id| {
                let ranges = if meta_file_ids.contains(&file_id) {
                    Vec::new()
                } else {
                    resolve_branch_ranges(&file_id)
                };
                let annotation_realm = resolve_annotation_realm(&file_id);
                let is_meta_file = meta_file_ids.contains(&file_id);
                let is_library_file = library_file_ids.contains(&file_id);
                let realm = if is_meta_file || is_library_file {
                    annotation_realm.unwrap_or(GmodRealm::Shared)
                } else {
                    annotation_realm.unwrap_or(default_realm)
                };
                (
                    file_id,
                    GmodRealmFileMetadata {
                        inferred_realm: realm,
                        annotation_realm,
                        branch_realm_ranges: ranges,
                        ..Default::default()
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        db.get_gmod_infer_index_mut()
            .set_all_realm_file_metadata(realm_metadata);
        return;
    }

    let mut realm_metadata = HashMap::new();
    for file_id in file_ids {
        let ranges = if meta_file_ids.contains(&file_id) {
            Vec::new()
        } else {
            resolve_branch_ranges(&file_id)
        };

        let annotation_realm = resolve_annotation_realm(&file_id);
        let is_meta_file = meta_file_ids.contains(&file_id);
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
                .or(filename_hint)
                .or_else(|| {
                    detect_calls
                        .then_some(())
                        .and(load_info)
                        .filter(|info| info.status != GmodLoadStatus::NoKnownLoadPath)
                        .map(|info| info.realm)
                })
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

fn gmod_config_default_realm(db: &DbIndex) -> GmodRealm {
    match db.get_emmyrc().gmod.default_realm {
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
