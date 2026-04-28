use std::collections::HashMap;
use std::sync::Arc;

use glua_parser::{
    LuaAstNode, LuaCallExpr, LuaCommentOwner, LuaExpr, LuaFuncStat, LuaIndexExpr, LuaIndexKey,
    LuaLocalFuncStat, PathTrait,
};
use rowan::{NodeOrToken, TextRange, TextSize};

use crate::{
    DiagnosticCode, FileId, GmodRealm, GmodRealmFileMetadata, LuaDiagnosticConfig, LuaInferCache,
    LuaMemberKey, LuaMemberOwner, LuaSemanticDeclId, LuaType, SemanticDeclLevel, SemanticModel,
    WorkspaceId,
};

use super::{Checker, DiagnosticContext};
use crate::compilation::analyzer::gmod::realm_from_doc_comment;

/// Immutable, workspace-scoped callee realm map keyed by semantic declaration.
pub type PrecomputedCalleeRealmMap = HashMap<LuaSemanticDeclId, Vec<ResolvedRealm>>;

pub struct GmodRealmMisuseChecker;

impl Checker for GmodRealmMisuseChecker {
    const CODES: &[DiagnosticCode] = &[
        DiagnosticCode::GmodRealmMismatch,
        DiagnosticCode::GmodRealmMismatchHeuristic,
        DiagnosticCode::GmodUnknownRealm,
    ];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let file_id = semantic_model.get_file_id();
        let db = semantic_model.get_db();
        let infer_index = db.get_gmod_infer_index();
        let Some(file_realm_metadata) = infer_index.get_realm_file_metadata(&file_id) else {
            return;
        };

        // Clone Arc to shared data upfront to avoid borrow conflicts with context
        let shared_data = context.get_shared_data_arc();

        // Use precomputed workspace data if available, otherwise compute per-file.
        let workspace_id = db
            .get_module_index()
            .get_workspace_id(file_id)
            .unwrap_or(WorkspaceId::MAIN);
        let precomputed = shared_data
            .as_ref()
            .and_then(|s| s.gm_method_realms.get(&workspace_id).cloned());
        let fallback;
        let gm_method_realms: &GmMethodRealmMap = match &precomputed {
            Some(pre) => pre.as_ref(),
            None => {
                fallback = collect_annotated_gm_method_realms(context, semantic_model);
                &fallback
            }
        };

        let mut decl_annotation_cache = HashMap::new();
        let mut callee_realm_cache: CalleeRealmCache = HashMap::new();
        let precomputed_callee_realms = shared_data
            .as_ref()
            .and_then(|s| s.callee_realms_by_workspace.get(&workspace_id))
            .map(Arc::as_ref);

        for call_expr in semantic_model.get_root().descendants::<LuaCallExpr>() {
            if context.is_cancelled() {
                return;
            }
            let call_realm = resolve_realm_at_offset(
                infer_index,
                &file_id,
                file_realm_metadata,
                call_expr.get_range().start(),
            );

            let mut callee_realms = resolve_callee_realms(
                context,
                semantic_model,
                &call_expr,
                &gm_method_realms,
                &mut decl_annotation_cache,
                &mut callee_realm_cache,
                precomputed_callee_realms,
            );
            if callee_realms.is_empty() {
                continue;
            }

            // If a function is defined in both client and server realms, treat it as shared
            let has_client = callee_realms.iter().any(|r| r.realm == GmodRealm::Client);
            let has_server = callee_realms.iter().any(|r| r.realm == GmodRealm::Server);
            if has_client && has_server {
                push_unique_realm(
                    &mut callee_realms,
                    ResolvedRealm {
                        realm: GmodRealm::Shared,
                        evidence: RealmEvidence::InferredDependency,
                    },
                );
            }

            let call_name = call_expr
                .get_access_path()
                .unwrap_or_else(|| "function".to_string());
            if let Some(callee_realm) = unknown_realm_candidate(call_realm, &callee_realms) {
                context.add_diagnostic(
                    DiagnosticCode::GmodUnknownRealm,
                    call_expr.get_range(),
                    mismatch_message(
                        DiagnosticCode::GmodUnknownRealm,
                        &call_name,
                        call_realm.realm,
                        callee_realm.realm,
                    ),
                    None,
                );
                continue;
            }

            let mismatch_candidates: Vec<ResolvedRealm> = callee_realms
                .iter()
                .copied()
                .filter(|callee| is_cross_realm_misuse(call_realm.realm, callee.realm))
                .collect();
            if mismatch_candidates.is_empty() {
                continue;
            }
            let compatible_candidate = callee_realms
                .iter()
                .copied()
                .filter(|callee| !is_cross_realm_misuse(call_realm.realm, callee.realm))
                .max_by_key(|realm| evidence_priority(realm.evidence));

            let callee_realm = pick_best_mismatch_candidate(&mismatch_candidates);
            if compatible_candidate.is_some_and(|candidate| {
                // When the mismatch comes from an explicit `---@realm` annotation the
                // developer deliberately restricted the function.  Only suppress if the
                // compatible candidate has equally strong (or stronger) evidence.
                // For every other evidence kind (branch, filename, dependency) it means
                // the function simply *exists* in another realm – any concrete compatible
                // definition should suppress the diagnostic.
                if callee_realm.evidence == RealmEvidence::ExplicitAnnotation {
                    evidence_priority(candidate.evidence)
                        >= evidence_priority(callee_realm.evidence)
                } else {
                    candidate.evidence != RealmEvidence::Unknown
                }
            }) {
                continue;
            }

            let Some(code) =
                diagnostic_code_for_mismatch(call_realm.evidence, callee_realm.evidence)
            else {
                continue;
            };

            context.add_diagnostic(
                code,
                call_expr.get_range(),
                mismatch_message(code, &call_name, call_realm.realm, callee_realm.realm),
                None,
            );
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum RealmEvidence {
    ExplicitBranch,
    ExplicitAnnotation,
    InferredFilename,
    InferredDependency,
    InferredDefault,
    Unknown,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ResolvedRealm {
    pub(crate) realm: GmodRealm,
    pub(crate) evidence: RealmEvidence,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct AnnotatedRealmRange {
    range: TextRange,
    realm: GmodRealm,
}

pub type GmMethodRealmMap = HashMap<String, Vec<ResolvedRealm>>;
type DeclAnnotationRealmCache = HashMap<FileId, Vec<AnnotatedRealmRange>>;
type CalleeRealmCache = HashMap<LuaSemanticDeclId, Vec<ResolvedRealm>>;

fn resolve_callee_realms(
    context: &DiagnosticContext,
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    gm_method_realms: &GmMethodRealmMap,
    decl_annotation_cache: &mut DeclAnnotationRealmCache,
    callee_realm_cache: &mut CalleeRealmCache,
    precomputed_callee_realms: Option<&PrecomputedCalleeRealmMap>,
) -> Vec<ResolvedRealm> {
    // Fast path: GM method annotations (O(1) HashMap lookup, no inference needed)
    if let Some(prefix_expr) = call_expr.get_prefix_expr()
        && let Some(index_expr) = LuaIndexExpr::cast(prefix_expr.syntax().clone())
    {
        let gm_realms = resolve_annotated_gm_method_realms(&index_expr, gm_method_realms);
        if !gm_realms.is_empty() {
            return gm_realms;
        }
    }

    // Resolve declaration — needed both as cache key and for non-member resolution paths
    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return Vec::new();
    };
    let semantic_decl = semantic_model.find_decl(
        NodeOrToken::Node(prefix_expr.syntax().clone()),
        SemanticDeclLevel::default(),
    );

    // Check caches — local first, then immutable workspace precompute.
    if let Some(ref decl) = semantic_decl {
        if let Some(cached) = callee_realm_cache.get(decl) {
            return cached.clone();
        }
        if let Some(precomputed) = precomputed_callee_realms
            && let Some(cached) = precomputed.get(decl)
        {
            let result = cached.clone();
            callee_realm_cache.insert(decl.clone(), result.clone());
            return result;
        }
    }

    // Resolve realms: try member candidate path first, then fallback paths
    let mut realms = Vec::new();

    if let Some(member_realms) = resolve_member_candidate_realms(
        context,
        semantic_model,
        call_expr,
        gm_method_realms,
        decl_annotation_cache,
    ) && !member_realms.is_empty()
    {
        realms = member_realms;
    } else if let Some(ref decl) = semantic_decl {
        for realm in resolve_global_name_candidate_realms(
            context,
            semantic_model,
            &prefix_expr,
            decl,
            decl_annotation_cache,
        ) {
            if context.is_cancelled() {
                return realms;
            }
            push_unique_realm(&mut realms, realm);
        }

        if let Some(realm) =
            resolve_decl_realm(context, semantic_model, decl, decl_annotation_cache)
        {
            push_unique_realm(&mut realms, realm);
        }

        if let LuaSemanticDeclId::Member(member_id) = decl.clone()
            && let Some(origin_owner) = semantic_model.get_member_origin_owner(member_id)
            && let Some(realm) = resolve_decl_realm(
                context,
                semantic_model,
                &origin_owner,
                decl_annotation_cache,
            )
        {
            push_unique_realm(&mut realms, realm);
        }
    }

    // Cache result in file-local cache only.
    if let Some(decl) = semantic_decl {
        callee_realm_cache.insert(decl, realms.clone());
    }
    realms
}

fn resolve_global_name_candidate_realms(
    context: &DiagnosticContext,
    semantic_model: &SemanticModel,
    prefix_expr: &LuaExpr,
    semantic_decl: &LuaSemanticDeclId,
    decl_annotation_cache: &mut DeclAnnotationRealmCache,
) -> Vec<ResolvedRealm> {
    let LuaSemanticDeclId::LuaDecl(decl_id) = semantic_decl else {
        return Vec::new();
    };
    let Some(decl) = semantic_model.get_db().get_decl_index().get_decl(decl_id) else {
        return Vec::new();
    };
    if !decl.is_global() {
        return Vec::new();
    }

    let LuaExpr::NameExpr(name_expr) = prefix_expr else {
        return Vec::new();
    };
    let Some(name) = name_expr.get_name_text() else {
        return Vec::new();
    };

    let mut realms = Vec::new();
    let member_key = LuaMemberKey::Name(name.into());
    if let Some(member_infos) =
        semantic_model.get_member_info_with_key(&LuaType::Global, member_key, true)
    {
        for member_info in member_infos {
            if context.is_cancelled() {
                return realms;
            }
            let Some(property_owner_id) = member_info.property_owner_id else {
                continue;
            };
            if let Some(realm) = resolve_decl_realm(
                context,
                semantic_model,
                &property_owner_id,
                decl_annotation_cache,
            ) {
                push_unique_realm(&mut realms, realm);
            }
        }
    }

    realms
}

fn resolve_member_candidate_realms(
    context: &DiagnosticContext,
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    gm_method_realms: &GmMethodRealmMap,
    decl_annotation_cache: &mut DeclAnnotationRealmCache,
) -> Option<Vec<ResolvedRealm>> {
    let prefix_expr = call_expr.get_prefix_expr()?;
    let index_expr = LuaIndexExpr::cast(prefix_expr.syntax().clone())?;
    let mut realms = resolve_annotated_gm_method_realms(&index_expr, gm_method_realms);

    // If we already have explicit GM method realm annotations, skip expensive inference.
    // The precomputed annotations are authoritative for GM.*/GAMEMODE.* calls.
    if !realms.is_empty() {
        return Some(realms);
    }

    if let Some(index_key) = index_expr.get_index_key()
        && let Some(member_key) = semantic_model.get_member_key(&index_key)
        && let Some(owner_expr) = index_expr.get_prefix_expr()
        && let Ok(owner_type) = semantic_model.infer_expr(owner_expr)
    {
        // For realm diagnostics we need ALL candidate declarations across
        // every workspace priority tier. The normal member-resolution path
        // `get_member_info_with_key` returns only realm-compatible members.
        let all_member_ids =
            collect_all_member_ids_for_type_key(semantic_model, &owner_type, &member_key);
        for member_id in all_member_ids {
            if context.is_cancelled() {
                return Some(realms);
            }
            let property_owner_id = LuaSemanticDeclId::Member(member_id);
            if let Some(realm) = resolve_decl_realm(
                context,
                semantic_model,
                &property_owner_id,
                decl_annotation_cache,
            ) {
                push_unique_realm(&mut realms, realm);
            }
        }
    }

    Some(realms)
}

fn resolve_decl_realm(
    context: &DiagnosticContext,
    semantic_model: &SemanticModel,
    semantic_decl: &LuaSemanticDeclId,
    decl_annotation_cache: &mut DeclAnnotationRealmCache,
) -> Option<ResolvedRealm> {
    let (decl_file_id, decl_offset) = semantic_decl_position(semantic_decl)?;
    let infer_index = semantic_model.get_db().get_gmod_infer_index();
    let metadata = infer_index.get_realm_file_metadata(&decl_file_id)?;
    if let Some(annotation_realm) = resolve_decl_annotation_realm_at_offset(
        context,
        semantic_model,
        &decl_file_id,
        decl_offset,
        decl_annotation_cache,
    ) {
        return Some(ResolvedRealm {
            realm: annotation_realm,
            evidence: RealmEvidence::ExplicitAnnotation,
        });
    }

    Some(resolve_realm_at_offset(
        infer_index,
        &decl_file_id,
        metadata,
        decl_offset,
    ))
}

fn resolve_decl_annotation_realm_at_offset(
    context: &DiagnosticContext,
    semantic_model: &SemanticModel,
    file_id: &FileId,
    offset: TextSize,
    decl_annotation_cache: &mut DeclAnnotationRealmCache,
) -> Option<GmodRealm> {
    let file_entries = decl_annotation_cache
        .entry(file_id.clone())
        .or_insert_with(|| {
            collect_decl_annotation_realms_for_file(context, semantic_model, file_id)
        });

    file_entries
        .iter()
        .find(|entry| entry.range.contains(offset))
        .map(|entry| entry.realm)
}

fn resolve_decl_annotation_realm_at_offset_from_db(
    db: &crate::DbIndex,
    file_id: &FileId,
    offset: TextSize,
    decl_annotation_cache: &mut DeclAnnotationRealmCache,
) -> Option<GmodRealm> {
    let file_entries = decl_annotation_cache
        .entry(*file_id)
        .or_insert_with(|| collect_decl_annotation_realms_for_file_from_db(db, file_id));

    file_entries
        .iter()
        .find(|entry| entry.range.contains(offset))
        .map(|entry| entry.realm)
}

fn collect_decl_annotation_realms_for_file(
    context: &DiagnosticContext,
    semantic_model: &SemanticModel,
    file_id: &FileId,
) -> Vec<AnnotatedRealmRange> {
    let Some(tree) = semantic_model.get_db().get_vfs().get_syntax_tree(file_id) else {
        return Vec::new();
    };

    let mut realms = Vec::new();
    for func_stat in tree.get_chunk_node().descendants::<LuaFuncStat>() {
        if context.is_cancelled() {
            return realms;
        }
        if let Some(comment) = func_stat.get_left_comment()
            && let Some(realm) = realm_from_doc_comment(&comment)
        {
            realms.push(AnnotatedRealmRange {
                range: func_stat.get_range(),
                realm,
            });
        }
    }

    for local_func_stat in tree.get_chunk_node().descendants::<LuaLocalFuncStat>() {
        if context.is_cancelled() {
            return realms;
        }
        if let Some(comment) = local_func_stat.get_left_comment()
            && let Some(realm) = realm_from_doc_comment(&comment)
        {
            realms.push(AnnotatedRealmRange {
                range: local_func_stat.get_range(),
                realm,
            });
        }
    }

    realms
}

fn collect_decl_annotation_realms_for_file_from_db(
    db: &crate::DbIndex,
    file_id: &FileId,
) -> Vec<AnnotatedRealmRange> {
    let Some(tree) = db.get_vfs().get_syntax_tree(file_id) else {
        return Vec::new();
    };

    let mut realms = Vec::new();
    for func_stat in tree.get_chunk_node().descendants::<LuaFuncStat>() {
        if let Some(comment) = func_stat.get_left_comment()
            && let Some(realm) = realm_from_doc_comment(&comment)
        {
            realms.push(AnnotatedRealmRange {
                range: func_stat.get_range(),
                realm,
            });
        }
    }

    for local_func_stat in tree.get_chunk_node().descendants::<LuaLocalFuncStat>() {
        if let Some(comment) = local_func_stat.get_left_comment()
            && let Some(realm) = realm_from_doc_comment(&comment)
        {
            realms.push(AnnotatedRealmRange {
                range: local_func_stat.get_range(),
                realm,
            });
        }
    }

    realms
}

fn resolve_annotated_gm_method_realms(
    index_expr: &LuaIndexExpr,
    gm_method_realms: &GmMethodRealmMap,
) -> Vec<ResolvedRealm> {
    let Some(LuaExpr::NameExpr(prefix_name)) = index_expr.get_prefix_expr() else {
        return Vec::new();
    };
    let Some(prefix_text) = prefix_name.get_name_text() else {
        return Vec::new();
    };
    if !matches!(prefix_text.as_str(), "GM" | "GAMEMODE") {
        return Vec::new();
    }
    let Some(LuaIndexKey::Name(target_method_name)) = index_expr.get_index_key() else {
        return Vec::new();
    };
    let target_method_name = target_method_name.get_name_text().to_string();

    gm_method_realms
        .get(&target_method_name)
        .cloned()
        .unwrap_or_default()
}

/// Collect ALL member IDs for a given type and key, bypassing the
/// workspace-priority-tier and realm-compatibility filtering that
/// `get_member_info_with_key` applies.
fn collect_all_member_ids_for_type_key(
    semantic_model: &SemanticModel,
    owner_type: &LuaType,
    member_key: &LuaMemberKey,
) -> Vec<crate::LuaMemberId> {
    let db = semantic_model.get_db();
    let member_index = db.get_member_index();

    // Resolve the LuaMemberOwner from the type.
    let owners = owner_type_to_member_owners(owner_type, db);
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for owner in owners {
        if let Some(member_item) = member_index.get_member_item(&owner, member_key) {
            for member_id in member_item.get_member_ids() {
                if seen.insert(member_id) {
                    result.push(member_id);
                }
            }
        }
    }

    result
}

/// Convert a `LuaType` into one or more `LuaMemberOwner` values to look up
/// members in the index. Mirrors the owner-resolution intent of
/// `find_members_guard` / `find_*_members` in `semantic::member` but returns
/// raw owners (no realm/workspace filtering, no alias/generic instantiation,
/// no member-info construction). Recursion is bounded by `depth` to guard
/// against pathological self-referential type graphs.
fn owner_type_to_member_owners(typ: &LuaType, db: &crate::DbIndex) -> Vec<LuaMemberOwner> {
    owner_type_to_member_owners_inner(typ, db, 0)
}

fn owner_type_to_member_owners_inner(
    typ: &LuaType,
    db: &crate::DbIndex,
    depth: u8,
) -> Vec<LuaMemberOwner> {
    if depth > 8 {
        return Vec::new();
    }
    let next_depth = depth + 1;
    match typ {
        LuaType::TableConst(id) => vec![LuaMemberOwner::Element(id.clone())],
        LuaType::Ref(type_decl_id) | LuaType::Def(type_decl_id) => {
            vec![LuaMemberOwner::Type(type_decl_id.clone())]
        }
        LuaType::Generic(generic_type) => {
            vec![LuaMemberOwner::Type(generic_type.get_base_type_id())]
        }
        LuaType::Instance(inst) => {
            let mut owners = Vec::new();
            owners.push(LuaMemberOwner::Element(inst.get_range().clone()));
            owners.extend(owner_type_to_member_owners_inner(
                inst.get_base(),
                db,
                next_depth,
            ));
            owners
        }
        LuaType::TableOf(inner) => owner_type_to_member_owners_inner(inner, db, next_depth),
        LuaType::Union(union_type) => {
            let mut owners = Vec::new();
            for sub in union_type.into_vec() {
                owners.extend(owner_type_to_member_owners_inner(&sub, db, next_depth));
            }
            owners
        }
        LuaType::MultiLineUnion(multi_union) => {
            owner_type_to_member_owners_inner(&multi_union.to_union(), db, next_depth)
        }
        LuaType::Intersection(intersection_type) => {
            let mut owners = Vec::new();
            for sub in intersection_type.get_types().iter() {
                owners.extend(owner_type_to_member_owners_inner(sub, db, next_depth));
            }
            owners
        }
        LuaType::ModuleRef(file_id) => {
            if let Some(module_info) = db.get_module_index().get_module(*file_id)
                && let Some(export_type) = &module_info.export_type
            {
                owner_type_to_member_owners_inner(export_type, db, next_depth)
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

fn collect_annotated_gm_method_realms(
    context: &DiagnosticContext,
    semantic_model: &SemanticModel,
) -> GmMethodRealmMap {
    let mut gm_method_realms = HashMap::new();

    let db = semantic_model.get_db();
    let module_index = db.get_module_index();
    let current_workspace_id = module_index.get_workspace_id(semantic_model.get_file_id());
    for (file_id, method_realms) in db.get_gmod_infer_index().iter_gm_method_realm_annotations() {
        if context.is_cancelled() {
            return gm_method_realms;
        }
        let file_id = *file_id;
        if let Some(current_workspace_id) = current_workspace_id {
            let candidate_workspace_id = module_index
                .get_workspace_id(file_id)
                .unwrap_or(WorkspaceId::MAIN);
            if module_index
                .workspace_resolution_priority(current_workspace_id, candidate_workspace_id)
                .is_none()
            {
                continue;
            }
        }

        for (method_name, realm) in method_realms {
            let entry = gm_method_realms
                .entry(method_name.clone())
                .or_insert_with(Vec::new);
            push_unique_realm(
                entry,
                ResolvedRealm {
                    realm: *realm,
                    evidence: RealmEvidence::ExplicitAnnotation,
                },
            );
        }
    }

    gm_method_realms
}

fn push_unique_realm(realms: &mut Vec<ResolvedRealm>, realm: ResolvedRealm) {
    if realms.iter().any(|existing| existing == &realm) {
        return;
    }

    realms.push(realm);
}

fn pick_best_mismatch_candidate(realms: &[ResolvedRealm]) -> ResolvedRealm {
    *realms
        .iter()
        .max_by_key(|realm| mismatch_candidate_sort_key(realm))
        .unwrap_or(&ResolvedRealm {
            realm: GmodRealm::Unknown,
            evidence: RealmEvidence::Unknown,
        })
}

fn evidence_priority(evidence: RealmEvidence) -> u8 {
    match evidence {
        RealmEvidence::ExplicitBranch => 5,
        RealmEvidence::ExplicitAnnotation => 4,
        RealmEvidence::InferredFilename => 3,
        RealmEvidence::InferredDependency => 2,
        RealmEvidence::InferredDefault => 1,
        RealmEvidence::Unknown => 0,
    }
}

fn mismatch_candidate_sort_key(realm: &ResolvedRealm) -> (u8, u8, u8) {
    (
        evidence_priority(realm.evidence),
        realm_ordinal(realm.realm),
        evidence_ordinal(realm.evidence),
    )
}

fn realm_ordinal(realm: GmodRealm) -> u8 {
    match realm {
        GmodRealm::Client => 0,
        GmodRealm::Server => 1,
        GmodRealm::Shared => 2,
        GmodRealm::Unknown => 3,
    }
}

fn evidence_ordinal(evidence: RealmEvidence) -> u8 {
    match evidence {
        RealmEvidence::ExplicitBranch => 5,
        RealmEvidence::ExplicitAnnotation => 4,
        RealmEvidence::InferredFilename => 3,
        RealmEvidence::InferredDependency => 2,
        RealmEvidence::InferredDefault => 1,
        RealmEvidence::Unknown => 0,
    }
}

fn semantic_decl_position(semantic_decl: &LuaSemanticDeclId) -> Option<(FileId, TextSize)> {
    match semantic_decl {
        LuaSemanticDeclId::LuaDecl(decl_id) => Some((decl_id.file_id, decl_id.position)),
        LuaSemanticDeclId::Member(member_id) => Some((member_id.file_id, member_id.get_position())),
        LuaSemanticDeclId::Signature(signature_id) => {
            Some((signature_id.get_file_id(), signature_id.get_position()))
        }
        LuaSemanticDeclId::TypeDecl(_) => None,
    }
}

fn resolve_realm_at_offset(
    infer_index: &crate::GmodInferIndex,
    file_id: &FileId,
    metadata: &GmodRealmFileMetadata,
    offset: TextSize,
) -> ResolvedRealm {
    ResolvedRealm {
        realm: infer_index.get_realm_at_offset(file_id, offset),
        evidence: realm_evidence_at_offset(metadata, offset),
    }
}

fn realm_evidence_at_offset(metadata: &GmodRealmFileMetadata, offset: TextSize) -> RealmEvidence {
    if metadata
        .branch_realm_ranges
        .iter()
        .any(|range| range.range.contains(offset))
    {
        return RealmEvidence::ExplicitBranch;
    }

    if metadata.annotation_realm.is_some() {
        return RealmEvidence::ExplicitAnnotation;
    }

    if metadata.filename_hint.is_some() {
        return RealmEvidence::InferredFilename;
    }

    if !metadata.dependency_hints.is_empty() {
        return RealmEvidence::InferredDependency;
    }

    if metadata.inferred_realm != GmodRealm::Unknown {
        return RealmEvidence::InferredDefault;
    }

    RealmEvidence::Unknown
}

fn diagnostic_code_for_mismatch(
    call_evidence: RealmEvidence,
    callee_evidence: RealmEvidence,
) -> Option<DiagnosticCode> {
    if is_strict_evidence(call_evidence) && is_strict_evidence(callee_evidence) {
        return Some(DiagnosticCode::GmodRealmMismatch);
    }

    if is_known_evidence(call_evidence) && is_known_evidence(callee_evidence) {
        return Some(DiagnosticCode::GmodRealmMismatchHeuristic);
    }

    None
}

fn unknown_realm_candidate(
    call_realm: ResolvedRealm,
    callee_realms: &[ResolvedRealm],
) -> Option<ResolvedRealm> {
    if call_realm.realm != GmodRealm::Unknown || call_realm.evidence != RealmEvidence::Unknown {
        return None;
    }

    callee_realms
        .iter()
        .copied()
        .filter(|callee| matches!(callee.realm, GmodRealm::Client | GmodRealm::Server))
        .filter(|callee| supports_unknown_realm_diagnostic(callee.evidence))
        .max_by_key(mismatch_candidate_sort_key)
}

fn supports_unknown_realm_diagnostic(evidence: RealmEvidence) -> bool {
    matches!(
        evidence,
        RealmEvidence::ExplicitBranch
            | RealmEvidence::ExplicitAnnotation
            | RealmEvidence::InferredFilename
    )
}

fn is_known_evidence(evidence: RealmEvidence) -> bool {
    evidence != RealmEvidence::Unknown
}

fn is_strict_evidence(evidence: RealmEvidence) -> bool {
    matches!(
        evidence,
        RealmEvidence::ExplicitBranch | RealmEvidence::ExplicitAnnotation
    )
}

fn is_cross_realm_misuse(call_realm: GmodRealm, callee_realm: GmodRealm) -> bool {
    matches!(
        (call_realm, callee_realm),
        (GmodRealm::Client, GmodRealm::Server) | (GmodRealm::Server, GmodRealm::Client)
    )
}

fn mismatch_message(
    code: DiagnosticCode,
    call_name: &str,
    call_realm: GmodRealm,
    callee_realm: GmodRealm,
) -> String {
    let call_realm = realm_label(call_realm);
    let callee_realm = realm_label(callee_realm);

    match code {
        DiagnosticCode::GmodRealmMismatch => t!(
            "Realm mismatch: calling `%{name}` in %{call_realm} realm but declaration is %{decl_realm}.",
            name = call_name,
            call_realm = call_realm,
            decl_realm = callee_realm,
        )
        .to_string(),
        DiagnosticCode::GmodRealmMismatchHeuristic => t!(
            "Potential realm mismatch (heuristic): `%{name}` is called in inferred %{call_realm} realm while declaration is inferred %{decl_realm}.",
            name = call_name,
            call_realm = call_realm,
            decl_realm = callee_realm,
        )
        .to_string(),
        DiagnosticCode::GmodUnknownRealm => t!(
            "Unable to resolve call realm for `%{name}`; declaration appears to be %{decl_realm}.",
            name = call_name,
            decl_realm = callee_realm,
        )
        .to_string(),
        _ => t!("Realm mismatch for `%{name}`.", name = call_name).to_string(),
    }
}

fn realm_label(realm: GmodRealm) -> &'static str {
    match realm {
        GmodRealm::Client => "client",
        GmodRealm::Server => "server",
        GmodRealm::Shared => "shared",
        GmodRealm::Unknown => "unknown",
    }
}

fn resolve_precomputed_decl_realm(
    db: &crate::DbIndex,
    semantic_decl: &LuaSemanticDeclId,
    decl_annotation_cache: &mut DeclAnnotationRealmCache,
) -> Option<ResolvedRealm> {
    let (decl_file_id, decl_offset) = semantic_decl_position(semantic_decl)?;
    let infer_index = db.get_gmod_infer_index();
    let metadata = infer_index.get_realm_file_metadata(&decl_file_id)?;
    if let Some(annotation_realm) = resolve_decl_annotation_realm_at_offset_from_db(
        db,
        &decl_file_id,
        decl_offset,
        decl_annotation_cache,
    ) {
        return Some(ResolvedRealm {
            realm: annotation_realm,
            evidence: RealmEvidence::ExplicitAnnotation,
        });
    }

    Some(resolve_realm_at_offset(
        infer_index,
        &decl_file_id,
        metadata,
        decl_offset,
    ))
}

/// Precompute declaration/member/signature realm facts for workspace diagnostics.
pub fn precompute_callee_realms_for_workspace(
    db: &crate::DbIndex,
    workspace_id: WorkspaceId,
    workspace_file_ids: &[FileId],
    gm_method_realms: &GmMethodRealmMap,
) -> PrecomputedCalleeRealmMap {
    let module_index = db.get_module_index();
    let mut callee_realms = HashMap::new();
    let mut decl_annotation_cache = HashMap::new();
    let precompute_config = Arc::new(LuaDiagnosticConfig::default());
    let mut workspace_callee_realm_cache: CalleeRealmCache = HashMap::new();

    for &file_id in workspace_file_ids {
        let candidate_workspace_id = module_index
            .get_workspace_id(file_id)
            .unwrap_or(WorkspaceId::MAIN);
        if module_index
            .workspace_resolution_priority(workspace_id, candidate_workspace_id)
            .is_none()
        {
            continue;
        }

        if let Some(decl_tree) = db.get_decl_index().get_decl_tree(&file_id) {
            let mut decl_ids: Vec<_> = decl_tree.get_decls().keys().copied().collect();
            decl_ids.sort_unstable_by_key(|decl_id| decl_id.position);
            for decl_id in decl_ids {
                let semantic_decl = LuaSemanticDeclId::LuaDecl(decl_id);
                if let Some(resolved) =
                    resolve_precomputed_decl_realm(db, &semantic_decl, &mut decl_annotation_cache)
                {
                    callee_realms.insert(semantic_decl, vec![resolved]);
                }
            }
        }

        let mut member_ids: Vec<_> = db
            .get_member_index()
            .get_file_members(file_id)
            .into_iter()
            .map(|member| member.get_id())
            .collect();
        member_ids.sort_unstable_by_key(|member_id| member_id.get_position());
        for member_id in member_ids {
            let semantic_decl = LuaSemanticDeclId::Member(member_id);
            if let Some(resolved) =
                resolve_precomputed_decl_realm(db, &semantic_decl, &mut decl_annotation_cache)
            {
                callee_realms.insert(semantic_decl, vec![resolved]);
            }
        }
    }

    let mut signature_ids: Vec<_> = db
        .get_signature_index()
        .iter()
        .map(|(signature_id, _)| *signature_id)
        .filter(|signature_id| {
            let candidate_workspace_id = module_index
                .get_workspace_id(signature_id.get_file_id())
                .unwrap_or(WorkspaceId::MAIN);
            module_index
                .workspace_resolution_priority(workspace_id, candidate_workspace_id)
                .is_some()
        })
        .collect();
    signature_ids.sort_unstable_by_key(|signature_id| {
        (signature_id.get_file_id(), signature_id.get_position())
    });
    for signature_id in signature_ids {
        let semantic_decl = LuaSemanticDeclId::Signature(signature_id);
        if let Some(resolved) =
            resolve_precomputed_decl_realm(db, &semantic_decl, &mut decl_annotation_cache)
        {
            callee_realms.insert(semantic_decl, vec![resolved]);
        }
    }

    // Enrich declaration realms with the same multi-source callee resolution used by
    // diagnostic-time resolution (member candidates, global-name candidates, origin owners, etc).
    // This keeps immutable precompute payloads aligned with the legacy per-file cache semantics.
    for &file_id in workspace_file_ids {
        let candidate_workspace_id = module_index
            .get_workspace_id(file_id)
            .unwrap_or(WorkspaceId::MAIN);
        if module_index
            .workspace_resolution_priority(workspace_id, candidate_workspace_id)
            .is_none()
        {
            continue;
        }
        let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) else {
            continue;
        };
        let semantic_model = SemanticModel::new(
            file_id,
            db,
            LuaInferCache::new(file_id, Default::default()),
            Arc::new(db.get_emmyrc().clone()),
            tree.get_chunk_node(),
        );
        let context = DiagnosticContext::new(
            file_id,
            db,
            precompute_config.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        for call_expr in semantic_model.get_root().descendants::<LuaCallExpr>() {
            let Some(prefix_expr) = call_expr.get_prefix_expr() else {
                continue;
            };
            let Some(semantic_decl) = semantic_model.find_decl(
                NodeOrToken::Node(prefix_expr.syntax().clone()),
                SemanticDeclLevel::default(),
            ) else {
                continue;
            };

            let resolved_realms = resolve_callee_realms(
                &context,
                &semantic_model,
                &call_expr,
                gm_method_realms,
                &mut decl_annotation_cache,
                &mut workspace_callee_realm_cache,
                None,
            );
            if resolved_realms.is_empty() {
                continue;
            }
            let entry = callee_realms.entry(semantic_decl).or_insert_with(Vec::new);
            for resolved in resolved_realms {
                push_unique_realm(entry, resolved);
            }
        }
    }

    callee_realms
}

/// Precompute GM method realm annotations for a specific workspace.
/// This is the same data that `collect_annotated_gm_method_realms` computes per-file,
/// but extracted to be called once per workspace during batch diagnostics.
pub fn precompute_gm_method_realms(
    db: &crate::db_index::DbIndex,
    workspace_id: WorkspaceId,
) -> GmMethodRealmMap {
    let mut gm_method_realms = HashMap::new();
    let module_index = db.get_module_index();

    for (file_id, method_realms) in db.get_gmod_infer_index().iter_gm_method_realm_annotations() {
        let file_id = *file_id;
        let candidate_workspace_id = module_index
            .get_workspace_id(file_id)
            .unwrap_or(WorkspaceId::MAIN);
        if module_index
            .workspace_resolution_priority(workspace_id, candidate_workspace_id)
            .is_none()
        {
            continue;
        }

        for (method_name, realm) in method_realms {
            let entry = gm_method_realms
                .entry(method_name.clone())
                .or_insert_with(Vec::new);
            push_unique_realm(
                entry,
                ResolvedRealm {
                    realm: *realm,
                    evidence: RealmEvidence::ExplicitAnnotation,
                },
            );
        }
    }

    gm_method_realms
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;

    use super::{
        RealmEvidence, ResolvedRealm, pick_best_mismatch_candidate, unknown_realm_candidate,
    };
    use crate::GmodRealm;

    #[gtest]
    fn pick_best_mismatch_candidate_uses_realm_tiebreaker_for_equal_evidence() {
        let candidates = vec![
            ResolvedRealm {
                realm: GmodRealm::Client,
                evidence: RealmEvidence::ExplicitAnnotation,
            },
            ResolvedRealm {
                realm: GmodRealm::Server,
                evidence: RealmEvidence::ExplicitAnnotation,
            },
        ];

        assert_that!(
            pick_best_mismatch_candidate(&candidates),
            eq(ResolvedRealm {
                realm: GmodRealm::Server,
                evidence: RealmEvidence::ExplicitAnnotation,
            })
        );
    }

    #[gtest]
    fn pick_best_mismatch_candidate_keeps_evidence_priority_dominant() {
        let candidates = vec![
            ResolvedRealm {
                realm: GmodRealm::Client,
                evidence: RealmEvidence::ExplicitBranch,
            },
            ResolvedRealm {
                realm: GmodRealm::Server,
                evidence: RealmEvidence::ExplicitAnnotation,
            },
        ];

        assert_that!(
            pick_best_mismatch_candidate(&candidates),
            eq(ResolvedRealm {
                realm: GmodRealm::Client,
                evidence: RealmEvidence::ExplicitBranch,
            })
        );
    }

    #[gtest]
    fn unknown_realm_candidate_uses_realm_tiebreaker_for_equal_evidence() {
        let call_realm = ResolvedRealm {
            realm: GmodRealm::Unknown,
            evidence: RealmEvidence::Unknown,
        };
        let callee_realms = vec![
            ResolvedRealm {
                realm: GmodRealm::Client,
                evidence: RealmEvidence::ExplicitAnnotation,
            },
            ResolvedRealm {
                realm: GmodRealm::Server,
                evidence: RealmEvidence::ExplicitAnnotation,
            },
        ];

        assert_that!(
            unknown_realm_candidate(call_realm, &callee_realms),
            some(eq(ResolvedRealm {
                realm: GmodRealm::Server,
                evidence: RealmEvidence::ExplicitAnnotation,
            }))
        );
    }
}
