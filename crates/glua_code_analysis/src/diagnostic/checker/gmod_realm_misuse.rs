use std::collections::HashMap;

use glua_parser::{
    LuaAstNode, LuaCallExpr, LuaComment, LuaCommentOwner, LuaDocTag, LuaDocTagRealm, LuaExpr,
    LuaFuncStat, LuaIndexExpr, LuaIndexKey, LuaLocalFuncStat, PathTrait,
};
use rowan::{NodeOrToken, TextRange, TextSize};

use crate::{
    DiagnosticCode, FileId, GmodRealm, GmodRealmFileMetadata, LuaMemberKey, LuaSemanticDeclId,
    LuaType, SemanticDeclLevel, SemanticModel, WorkspaceId,
};

use super::{Checker, DiagnosticContext};

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
        let gm_method_realms = collect_annotated_gm_method_realms(context, semantic_model);
        let mut decl_annotation_cache = HashMap::new();

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
enum RealmEvidence {
    ExplicitBranch,
    ExplicitAnnotation,
    InferredFilename,
    InferredDependency,
    InferredDefault,
    Unknown,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct ResolvedRealm {
    realm: GmodRealm,
    evidence: RealmEvidence,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct AnnotatedRealmRange {
    range: TextRange,
    realm: GmodRealm,
}

type GmMethodRealmMap = HashMap<String, Vec<ResolvedRealm>>;
type DeclAnnotationRealmCache = HashMap<FileId, Vec<AnnotatedRealmRange>>;

fn resolve_callee_realms(
    context: &DiagnosticContext,
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    gm_method_realms: &GmMethodRealmMap,
    decl_annotation_cache: &mut DeclAnnotationRealmCache,
) -> Vec<ResolvedRealm> {
    if let Some(realms) = resolve_member_candidate_realms(
        context,
        semantic_model,
        call_expr,
        gm_method_realms,
        decl_annotation_cache,
    ) && !realms.is_empty()
    {
        return realms;
    }

    let mut realms = Vec::new();
    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return realms;
    };
    let semantic_decl = semantic_model.find_decl(
        NodeOrToken::Node(prefix_expr.syntax().clone()),
        SemanticDeclLevel::default(),
    );

    let Some(semantic_decl) = semantic_decl else {
        return realms;
    };

    for realm in resolve_global_name_candidate_realms(
        context,
        semantic_model,
        &prefix_expr,
        &semantic_decl,
        decl_annotation_cache,
    ) {
        if context.is_cancelled() {
            return realms;
        }
        push_unique_realm(&mut realms, realm);
    }

    if let Some(realm) = resolve_decl_realm(
        context,
        semantic_model,
        &semantic_decl,
        decl_annotation_cache,
    ) {
        push_unique_realm(&mut realms, realm);
    }

    if let LuaSemanticDeclId::Member(member_id) = semantic_decl
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

    if let Some(index_key) = index_expr.get_index_key()
        && let Some(member_key) = semantic_model.get_member_key(&index_key)
        && let Some(owner_expr) = index_expr.get_prefix_expr()
        && let Ok(owner_type) = semantic_model.infer_expr(owner_expr)
        && let Some(member_infos) =
            semantic_model.get_member_info_with_key(&owner_type, member_key, true)
    {
        for member_info in member_infos {
            if context.is_cancelled() {
                return Some(realms);
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

fn realm_from_doc_comment(comment: &LuaComment) -> Option<GmodRealm> {
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
        _ => None,
    }
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
        .max_by_key(|realm| evidence_priority(realm.evidence))
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
        .max_by_key(|realm| evidence_priority(realm.evidence))
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
