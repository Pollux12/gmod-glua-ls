use emmylua_parser::{LuaAstNode, LuaCallExpr, LuaIndexExpr, PathTrait};
use rowan::{NodeOrToken, TextSize};

use crate::{
    DiagnosticCode, FileId, GmodRealm, GmodRealmFileMetadata, LuaSemanticDeclId, SemanticDeclLevel,
    SemanticModel,
};

use super::{Checker, DiagnosticContext};

pub struct GmodRealmMisuseChecker;

impl Checker for GmodRealmMisuseChecker {
    const CODES: &[DiagnosticCode] = &[
        DiagnosticCode::GmodRealmMisuse,
        DiagnosticCode::GmodRealmMisuseRisky,
    ];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let file_id = semantic_model.get_file_id();
        let db = semantic_model.get_db();
        let infer_index = db.get_gmod_infer_index();
        let Some(file_realm_metadata) = infer_index.get_realm_file_metadata(&file_id) else {
            return;
        };

        for call_expr in semantic_model.get_root().descendants::<LuaCallExpr>() {
            let call_realm = resolve_realm_at_offset(
                infer_index,
                &file_id,
                file_realm_metadata,
                call_expr.get_range().start(),
            );

            let mut callee_realms = resolve_callee_realms(semantic_model, &call_expr);
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

            if callee_realms
                .iter()
                .any(|callee| !is_cross_realm_misuse(call_realm.realm, callee.realm))
            {
                continue;
            }

            let callee_realm = pick_best_mismatch_candidate(&callee_realms);

            let Some(code) =
                diagnostic_code_for_mismatch(call_realm.evidence, callee_realm.evidence)
            else {
                continue;
            };

            let call_name = call_expr
                .get_access_path()
                .unwrap_or_else(|| "function".to_string());
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

fn resolve_callee_realms(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
) -> Vec<ResolvedRealm> {
    if let Some(realms) = resolve_member_candidate_realms(semantic_model, call_expr)
        && !realms.is_empty()
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

    if let Some(realm) = resolve_decl_realm(semantic_model, &semantic_decl) {
        push_unique_realm(&mut realms, realm);
    }

    if let LuaSemanticDeclId::Member(member_id) = semantic_decl
        && let Some(origin_owner) = semantic_model.get_member_origin_owner(member_id)
        && let Some(realm) = resolve_decl_realm(semantic_model, &origin_owner)
    {
        push_unique_realm(&mut realms, realm);
    }

    realms
}

fn resolve_member_candidate_realms(
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
) -> Option<Vec<ResolvedRealm>> {
    let prefix_expr = call_expr.get_prefix_expr()?;
    let index_expr = LuaIndexExpr::cast(prefix_expr.syntax().clone())?;
    let member_key = semantic_model.get_member_key(&index_expr.get_index_key()?)?;
    let owner_expr = index_expr.get_prefix_expr()?;
    let owner_type = semantic_model.infer_expr(owner_expr).ok()?;
    let member_infos = semantic_model.get_member_info_with_key(&owner_type, member_key, true)?;

    let mut realms = Vec::new();
    for member_info in member_infos {
        let Some(property_owner_id) = member_info.property_owner_id else {
            continue;
        };
        if let Some(realm) = resolve_decl_realm(semantic_model, &property_owner_id) {
            push_unique_realm(&mut realms, realm);
        }
    }

    Some(realms)
}

fn resolve_decl_realm(
    semantic_model: &SemanticModel,
    semantic_decl: &LuaSemanticDeclId,
) -> Option<ResolvedRealm> {
    let (decl_file_id, decl_offset) = semantic_decl_position(semantic_decl)?;
    let infer_index = semantic_model.get_db().get_gmod_infer_index();
    let metadata = infer_index.get_realm_file_metadata(&decl_file_id)?;
    Some(resolve_realm_at_offset(
        infer_index,
        &decl_file_id,
        metadata,
        decl_offset,
    ))
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
        return Some(DiagnosticCode::GmodRealmMisuse);
    }

    if is_known_evidence(call_evidence) && is_known_evidence(callee_evidence) {
        return Some(DiagnosticCode::GmodRealmMisuseRisky);
    }

    None
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
        DiagnosticCode::GmodRealmMisuse => t!(
            "Realm mismatch: calling `%{name}` in %{call_realm} realm but declaration is %{decl_realm}.",
            name = call_name,
            call_realm = call_realm,
            decl_realm = callee_realm,
        )
        .to_string(),
        DiagnosticCode::GmodRealmMisuseRisky => t!(
            "Potential realm mismatch (heuristic): `%{name}` is called in inferred %{call_realm} realm while declaration is inferred %{decl_realm}.",
            name = call_name,
            call_realm = call_realm,
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
