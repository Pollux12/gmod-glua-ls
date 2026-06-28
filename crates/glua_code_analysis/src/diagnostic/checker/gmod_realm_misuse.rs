use rustc_hash::FxHashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use glua_parser::{
    LuaAstNode, LuaCallExpr, LuaCommentOwner, LuaExpr, LuaFuncStat, LuaIndexExpr, LuaIndexKey,
    LuaLocalFuncStat, PathTrait,
};
use rowan::{NodeOrToken, TextRange, TextSize};

use crate::{
    DiagnosticCode, FileId, GmodRealm, GmodRealmFileMetadata, GmodStateMask, LuaDeclarationTree,
    LuaMemberId, LuaMemberKey, LuaMemberOwner, LuaSemanticDeclId, LuaType, SemanticDeclLevel,
    SemanticModel, WorkspaceId,
};

use super::{Checker, DiagnosticContext};
use crate::compilation::analyzer::gmod::realm_from_doc_comment;

/// Immutable, workspace-scoped callee realm map keyed by semantic declaration.
pub type PrecomputedCalleeRealmMap = FxHashMap<LuaSemanticDeclId, Vec<ResolvedRealm>>;

#[derive(Debug, Default)]
pub struct PrecomputedRealmCallCandidates {
    realms_by_name: FxHashMap<String, Vec<ResolvedRealm>>,
    realms_by_access_path: FxHashMap<String, Vec<ResolvedRealm>>,
}

impl PrecomputedRealmCallCandidates {
    pub fn insert_realm(&mut self, name: &str, realm: ResolvedRealm) {
        let realms = self.realms_by_name.entry(name.to_string()).or_default();
        push_unique_realm(realms, realm);
    }

    pub fn insert_access_path(&mut self, access_path: &str, realm: ResolvedRealm) {
        let realms = self
            .realms_by_access_path
            .entry(access_path.to_string())
            .or_default();
        push_unique_realm(realms, realm);
    }

    pub fn insert_gm_method_realms(&mut self, gm_method_realms: &GmMethodRealmMap) {
        for (method_name, realms) in gm_method_realms {
            for realm in realms {
                self.insert_realm(method_name, *realm);
                self.insert_access_path(&format!("GM.{method_name}"), *realm);
                self.insert_access_path(&format!("GAMEMODE.{method_name}"), *realm);
            }
        }
    }

    #[cfg(test)]
    fn should_check_call(&self, call_realm: ResolvedRealm, name: &str) -> bool {
        let Some(realms) = self.realms_by_name.get(name) else {
            return true;
        };

        should_check_candidate_realms(call_realm, realms)
    }

    fn should_check_access_path(
        &self,
        call_realm: ResolvedRealm,
        access_path: &str,
    ) -> Option<bool> {
        self.realms_by_access_path
            .get(access_path)
            .map(|realms| should_check_candidate_realms(call_realm, realms))
    }
}

fn should_check_candidate_realms(call_realm: ResolvedRealm, realms: &[ResolvedRealm]) -> bool {
    if realms.is_empty() {
        return true;
    }

    if call_realm.realm == GmodRealm::Unknown && call_realm.evidence == RealmEvidence::Unknown {
        return realms.iter().any(|callee| {
            matches!(callee.realm, GmodRealm::Client | GmodRealm::Server)
                && supports_unknown_realm_diagnostic(callee.evidence)
        });
    }

    realms.iter().any(|callee| {
        is_known_evidence(callee.evidence) && is_cross_realm_misuse(call_realm, *callee)
    })
}

pub struct PrecomputedCalleeRealmData {
    pub callee_realms: PrecomputedCalleeRealmMap,
    pub realm_call_candidates: PrecomputedRealmCallCandidates,
}

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
        if file_realm_metadata.branch_realm_ranges.is_empty()
            && resolve_file_realm(file_realm_metadata).is_universal_runtime_caller()
        {
            return;
        }

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

        let mut decl_annotation_cache: DeclAnnotationRealmCache = FxHashMap::default();
        let mut callee_realm_cache: CalleeRealmCache = FxHashMap::default();
        let mut member_candidate_cache: MemberCandidateCache = FxHashMap::default();
        let mut owner_key_member_candidate_cache: OwnerKeyMemberCandidateCache =
            FxHashMap::default();
        let mut owner_expansion_cache: OwnerExpansionCache = FxHashMap::default();
        let mut member_realms_cache: MemberRealmsCache = FxHashMap::default();
        let mut decl_realm_cache: DeclRealmCache = FxHashMap::default();
        let precomputed_callee_realms = shared_data
            .as_ref()
            .and_then(|s| s.callee_realms_by_workspace.get(&workspace_id))
            .map(Arc::as_ref);
        let realm_call_candidates = shared_data
            .as_ref()
            .and_then(|s| s.realm_call_candidates_by_workspace.get(&workspace_id))
            .map(Arc::as_ref);
        let decl_tree = db.get_decl_index().get_decl_tree(&file_id);
        let profile_enabled = log::log_enabled!(log::Level::Info);
        let mut profile = profile_enabled.then(GmodRealmMisuseProfile::default);

        for call_expr in semantic_model.get_root().descendants::<LuaCallExpr>() {
            if context.is_cancelled() {
                return;
            }
            if let Some(profile) = profile.as_mut() {
                profile.calls_scanned += 1;
            }
            let realm_start = profile_enabled.then(Instant::now);
            let call_realm =
                resolve_realm_at_position(file_realm_metadata, call_expr.get_range().start());
            if let (Some(profile), Some(realm_start)) = (profile.as_mut(), realm_start) {
                profile.call_realm_resolution_time += realm_start.elapsed();
            }
            if call_realm.is_universal_runtime_caller() {
                if let Some(profile) = profile.as_mut() {
                    profile.shared_call_skips += 1;
                }
                continue;
            }

            if let Some(profile) = profile.as_mut() {
                profile.calls_checked += 1;
            }
            if let Some(candidates) = realm_call_candidates
                && let Some(should_check) =
                    should_check_static_realm_call(&call_expr, call_realm, candidates, decl_tree)
                && !should_check
            {
                if let Some(profile) = profile.as_mut() {
                    profile.static_candidate_skips += 1;
                }
                continue;
            }
            let resolve_start = profile_enabled.then(Instant::now);
            let mut callee_realms = resolve_callee_realms(
                context,
                semantic_model,
                &call_expr,
                call_realm,
                &gm_method_realms,
                &mut decl_annotation_cache,
                &mut decl_realm_cache,
                &mut callee_realm_cache,
                &mut member_candidate_cache,
                &mut owner_key_member_candidate_cache,
                &mut owner_expansion_cache,
                &mut member_realms_cache,
                precomputed_callee_realms,
                profile.as_mut(),
            );
            if let (Some(profile), Some(resolve_start)) = (profile.as_mut(), resolve_start) {
                profile.callee_resolution_time += resolve_start.elapsed();
            }
            if callee_realms.is_empty() {
                if let Some(profile) = profile.as_mut() {
                    profile.empty_callee_realms += 1;
                }
                continue;
            }

            // If a function is defined in both client and server realms, treat it as shared
            let has_client = callee_realms.iter().any(|r| r.realm == GmodRealm::Client);
            let has_server = callee_realms.iter().any(|r| r.realm == GmodRealm::Server);
            if has_client && has_server {
                push_unique_realm(
                    &mut callee_realms,
                    ResolvedRealm::new(GmodRealm::Shared, RealmEvidence::InferredDependency),
                );
            }

            if let Some(callee_realm) = unknown_realm_candidate(call_realm, &callee_realms) {
                let call_name = call_expr
                    .get_access_path()
                    .unwrap_or_else(|| "function".to_string());
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
                if let Some(profile) = profile.as_mut() {
                    profile.diagnostics_emitted += 1;
                }
                continue;
            }

            let Some(callee_realm) =
                pick_best_mismatch_candidate_for_call(call_realm, &callee_realms)
            else {
                continue;
            };
            let compatible_candidate = callee_realms
                .iter()
                .copied()
                .filter(|callee| call_realm.is_compatible_with(*callee))
                .max_by_key(|realm| evidence_priority(realm.evidence));

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

            let call_name = call_expr
                .get_access_path()
                .unwrap_or_else(|| "function".to_string());
            context.add_diagnostic(
                code,
                call_expr.get_range(),
                mismatch_message(code, &call_name, call_realm.realm, callee_realm.realm),
                None,
            );
            if let Some(profile) = profile.as_mut() {
                profile.diagnostics_emitted += 1;
            }
        }

        if let Some(mut profile) = profile {
            profile.annotation_cache_files_loaded = decl_annotation_cache.len();
            profile.log(file_id);
        }
    }
}

#[derive(Default)]
struct GmodRealmMisuseProfile {
    calls_scanned: usize,
    calls_checked: usize,
    shared_call_skips: usize,
    empty_callee_realms: usize,
    diagnostics_emitted: usize,
    gm_method_fast_hits: usize,
    static_candidate_skips: usize,
    decl_cache_hits: usize,
    decl_cache_misses: usize,
    precomputed_callee_hits: usize,
    member_realms_cache_hits: usize,
    member_candidate_cache_hits: usize,
    member_candidate_cache_misses: usize,
    member_candidate_time: Duration,
    call_realm_resolution_time: Duration,
    annotation_cache_files_loaded: usize,
    callee_resolution_time: Duration,
}

impl GmodRealmMisuseProfile {
    fn log(&self, file_id: FileId) {
        log::info!(
            "gmod realm misuse profile: file={:?} calls_scanned={} calls_checked={} shared_skips={} static_skips={} empty_callee={} diagnostics={} gm_fast_hits={} decl_cache_hits={} decl_cache_misses={} precomputed_hits={} member_realms_cache_hits={} member_cache_hits={} member_cache_misses={} member_time={:?} call_realm_time={:?} annotation_files_loaded={} callee_time={:?}",
            file_id,
            self.calls_scanned,
            self.calls_checked,
            self.shared_call_skips,
            self.static_candidate_skips,
            self.empty_callee_realms,
            self.diagnostics_emitted,
            self.gm_method_fast_hits,
            self.decl_cache_hits,
            self.decl_cache_misses,
            self.precomputed_callee_hits,
            self.member_realms_cache_hits,
            self.member_candidate_cache_hits,
            self.member_candidate_cache_misses,
            self.member_candidate_time,
            self.call_realm_resolution_time,
            self.annotation_cache_files_loaded,
            self.callee_resolution_time,
        );
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
    pub(crate) state_mask: GmodStateMask,
    pub(crate) evidence: RealmEvidence,
}

impl ResolvedRealm {
    fn new(realm: GmodRealm, evidence: RealmEvidence) -> Self {
        Self {
            realm,
            state_mask: realm.state_mask(),
            evidence,
        }
    }

    fn with_state_mask(
        realm: GmodRealm,
        state_mask: GmodStateMask,
        evidence: RealmEvidence,
    ) -> Self {
        Self {
            realm,
            state_mask,
            evidence,
        }
    }

    fn is_compatible_with(self, callee: Self) -> bool {
        self.state_mask.is_compatible_with(callee.state_mask)
    }

    fn is_strictly_incompatible_with(self, callee: Self) -> bool {
        self.state_mask
            .is_strictly_incompatible_with(callee.state_mask)
    }

    fn is_universal_runtime_caller(self) -> bool {
        let runtime_mask = self.state_mask.without_menu();
        runtime_mask.contains(GmodStateMask::SHARED)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct AnnotatedRealmRange {
    pub range: TextRange,
    pub realm: GmodRealm,
}

pub type GmMethodRealmMap = FxHashMap<String, Vec<ResolvedRealm>>;
type DeclAnnotationRealmCache = FxHashMap<FileId, Vec<AnnotatedRealmRange>>;
type CalleeRealmCache = FxHashMap<LuaSemanticDeclId, Vec<ResolvedRealm>>;
type MemberCandidateCache = FxHashMap<(LuaType, LuaMemberKey), Vec<LuaMemberId>>;
type OwnerKeyMemberCandidateCache = FxHashMap<(LuaMemberOwner, LuaMemberKey), Vec<LuaMemberId>>;
type OwnerExpansionCache = FxHashMap<LuaType, Vec<LuaMemberOwner>>;
/// Cache for the final resolved realm set for member calls, keyed by (owner_type, member_key).
/// Sound because owner_type + member_key fully determines which member candidates are found
/// and their realms — independent of call-site syntax. Many different call expressions with
/// the same receiver type (e.g. `Entity`) and member name share a cached result.
type MemberRealmsCache = FxHashMap<(LuaType, LuaMemberKey), Vec<ResolvedRealm>>;
/// Cache for per-decl resolved realm, avoiding repeated file metadata + annotation lookups.
/// `None` stored as `Option::None` sentinel (use `Option<Option<ResolvedRealm>>`).
type DeclRealmCache = FxHashMap<LuaSemanticDeclId, Option<ResolvedRealm>>;

fn should_check_static_realm_call(
    call_expr: &LuaCallExpr,
    call_realm: ResolvedRealm,
    candidates: &PrecomputedRealmCallCandidates,
    decl_tree: Option<&LuaDeclarationTree>,
) -> Option<bool> {
    let access_path = call_expr.get_access_path()?;
    let root_name = access_path_root_name(&access_path)?;
    if decl_tree
        .and_then(|tree| tree.find_local_decl(root_name, call_expr.get_position()))
        .is_some()
    {
        return None;
    }

    candidates.should_check_access_path(call_realm, &access_path)
}

fn access_path_root_name(access_path: &str) -> Option<&str> {
    let root_end = access_path
        .find('.')
        .or_else(|| access_path.find('['))
        .unwrap_or(access_path.len());
    (root_end > 0).then(|| &access_path[..root_end])
}

fn resolve_callee_realms(
    context: &DiagnosticContext,
    semantic_model: &SemanticModel,
    call_expr: &LuaCallExpr,
    call_realm: ResolvedRealm,
    gm_method_realms: &GmMethodRealmMap,
    decl_annotation_cache: &mut DeclAnnotationRealmCache,
    decl_realm_cache: &mut DeclRealmCache,
    callee_realm_cache: &mut CalleeRealmCache,
    member_candidate_cache: &mut MemberCandidateCache,
    owner_key_member_candidate_cache: &mut OwnerKeyMemberCandidateCache,
    owner_expansion_cache: &mut OwnerExpansionCache,
    member_realms_cache: &mut MemberRealmsCache,
    precomputed_callee_realms: Option<&PrecomputedCalleeRealmMap>,
    mut profile: Option<&mut GmodRealmMisuseProfile>,
) -> Vec<ResolvedRealm> {
    // Resolve declaration — needed both as cache key and for non-member resolution paths
    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return Vec::new();
    };

    let is_bare_name_call = matches!(prefix_expr, LuaExpr::NameExpr(_));

    // Fast path: GM method annotations (O(1) HashMap lookup, no inference needed).
    // Only applies to member calls (index expressions like GM:Method or GAMEMODE.Method).
    if !is_bare_name_call {
        if let Some(index_expr) = LuaIndexExpr::cast(prefix_expr.syntax().clone()) {
            let gm_realms = resolve_annotated_gm_method_realms(&index_expr, gm_method_realms);
            if !gm_realms.is_empty() {
                if let Some(profile) = profile.as_mut() {
                    profile.gm_method_fast_hits += 1;
                }
                return gm_realms;
            }
        }
    }

    // Dynamic member calls (`tbl[expr]()` where `expr` is not a compile-time
    // constant) produce `LuaMemberKey::ExprType` keys. Those keys match *any*
    // other dynamic access whose key infers to the same type — e.g. an
    // unrelated `ent[k] = v` in a server file — never the member actually
    // being called, so realm evidence resolved through them is meaningless.
    if let Some(index_expr) = LuaIndexExpr::cast(prefix_expr.syntax().clone())
        && let Some(index_key) = index_expr.get_index_key()
        && LuaMemberKey::index_key_is_dynamic(
            semantic_model.get_db(),
            &mut semantic_model.get_cache().borrow_mut(),
            &index_key,
        )
    {
        return Vec::new();
    }

    let semantic_decl = semantic_model.find_decl(
        NodeOrToken::Node(prefix_expr.syntax().clone()),
        SemanticDeclLevel::default(),
    );

    // Check file-local cache first for bare declarations. Member calls are
    // call-site-sensitive: different owner/key expressions can resolve to the
    // same semantic declaration after assignment collapsing, but still need
    // different realm candidate sets.
    if let Some(ref decl) = semantic_decl {
        if is_bare_name_call && let Some(cached) = callee_realm_cache.get(decl) {
            if let Some(profile) = profile.as_mut() {
                profile.decl_cache_hits += 1;
            }
            return cached.clone();
        }
        if let Some(profile) = profile.as_mut() {
            profile.decl_cache_misses += 1;
        }
    }

    // Resolve realms: try precomputed first (O(1) HashMap lookup), then
    // fall back to expensive member/global resolution paths.
    let mut realms = Vec::new();

    // Try precomputed callee realms first — this is the authoritative workspace-
    // scoped answer and avoids expensive type graph traversal.
    // However, for bare-name global calls we still need to check global name candidates
    // because globals can be redefined across files with different realms.

    if let Some(ref decl) = semantic_decl {
        if let Some(precomputed) = precomputed_callee_realms.and_then(|map| map.get(decl)) {
            if let Some(profile) = profile.as_mut() {
                profile.precomputed_callee_hits += 1;
            }
            for realm in precomputed {
                push_unique_realm(&mut realms, *realm);
            }
        }
    }

    // For bare-name global calls, always check global name candidates to find
    // redefinitions with different realms (e.g., shared + client redefinitions).
    if is_bare_name_call {
        if let Some(ref decl) = semantic_decl {
            for realm in resolve_global_name_candidate_realms(
                context,
                semantic_model,
                &prefix_expr,
                decl,
                decl_annotation_cache,
                decl_realm_cache,
                precomputed_callee_realms,
            ) {
                if context.is_cancelled() {
                    return realms;
                }
                push_unique_realm(&mut realms, realm);
            }
        }
    }

    // Member calls may resolve through a collapsed assignment in the precomputed
    // cache. Merge owner/key candidates only when the normally resolved callee
    // is missing or incompatible; a known compatible callee suppresses any
    // mismatch from alternate candidates later anyway.
    if !is_bare_name_call && !has_known_compatible_realm(call_realm, &realms) {
        if let Some(member_realms) = resolve_member_candidate_realms(
            context,
            semantic_model,
            call_expr,
            semantic_decl.as_ref(),
            decl_annotation_cache,
            decl_realm_cache,
            member_candidate_cache,
            owner_key_member_candidate_cache,
            owner_expansion_cache,
            member_realms_cache,
            precomputed_callee_realms,
            profile,
        ) && !member_realms.is_empty()
        {
            for realm in member_realms {
                push_unique_realm(&mut realms, realm);
            }
        }
    }

    // Final fallback: decl resolution for non-globals
    if realms.is_empty() && !is_bare_name_call {
        if let Some(ref decl) = semantic_decl {
            if let Some(realm) = resolve_decl_realm_cached(
                context,
                semantic_model,
                decl,
                decl_annotation_cache,
                decl_realm_cache,
                precomputed_callee_realms,
            ) {
                push_unique_realm(&mut realms, realm);
            }

            if let LuaSemanticDeclId::Member(member_id) = decl.clone()
                && let Some(origin_owner) = semantic_model.get_member_origin_owner(member_id)
            {
                if let Some(realm) = resolve_decl_realm_cached(
                    context,
                    semantic_model,
                    &origin_owner,
                    decl_annotation_cache,
                    decl_realm_cache,
                    precomputed_callee_realms,
                ) {
                    push_unique_realm(&mut realms, realm);
                }
            }
        }
    }

    // Cache bare declaration results in file-local cache only.
    if is_bare_name_call && let Some(decl) = semantic_decl {
        callee_realm_cache.insert(decl, realms.clone());
    }
    realms
}

fn has_known_compatible_realm(call_realm: ResolvedRealm, callee_realms: &[ResolvedRealm]) -> bool {
    callee_realms
        .iter()
        .any(|callee| call_realm.is_compatible_with(*callee) && is_known_evidence(callee.evidence))
}

fn resolve_global_name_candidate_realms(
    context: &DiagnosticContext,
    semantic_model: &SemanticModel,
    prefix_expr: &LuaExpr,
    semantic_decl: &LuaSemanticDeclId,
    decl_annotation_cache: &mut DeclAnnotationRealmCache,
    decl_realm_cache: &mut DeclRealmCache,
    precomputed_callee_realms: Option<&PrecomputedCalleeRealmMap>,
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
            if let Some(realm) = resolve_decl_realm_cached(
                context,
                semantic_model,
                &property_owner_id,
                decl_annotation_cache,
                decl_realm_cache,
                precomputed_callee_realms,
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
    semantic_decl: Option<&LuaSemanticDeclId>,
    decl_annotation_cache: &mut DeclAnnotationRealmCache,
    decl_realm_cache: &mut DeclRealmCache,
    member_candidate_cache: &mut MemberCandidateCache,
    owner_key_member_candidate_cache: &mut OwnerKeyMemberCandidateCache,
    owner_expansion_cache: &mut OwnerExpansionCache,
    member_realms_cache: &mut MemberRealmsCache,
    precomputed_callee_realms: Option<&PrecomputedCalleeRealmMap>,
    mut profile: Option<&mut GmodRealmMisuseProfile>,
) -> Option<Vec<ResolvedRealm>> {
    // NOTE: GM method fast-path is already handled in resolve_callee_realms before
    // this function is called, so we don't need to repeat it here.
    let prefix_expr = call_expr.get_prefix_expr()?;
    let index_expr = LuaIndexExpr::cast(prefix_expr.syntax().clone())?;

    // Extract the member key cheaply (no type inference yet).
    let index_key = index_expr.get_index_key()?;
    let member_key = semantic_model.get_member_key(&index_key)?;
    let owner_expr = index_expr.get_prefix_expr()?;

    // Infer the owner type — this is memoized by syntax ID in LuaInferCache, so
    // repeated calls on the same expression node are O(1) after the first.
    let Ok(owner_type) = semantic_model.infer_expr(owner_expr) else {
        return Some(Vec::new());
    };

    // Check member_realms_cache: many call expressions with the same owner type
    // (e.g. `Entity`) and member key share a single computed realm set.
    let cache_key = (owner_type.clone(), member_key.clone());
    if let Some(cached) = member_realms_cache.get(&cache_key) {
        if let Some(profile) = profile.as_mut() {
            profile.member_realms_cache_hits += 1;
        }
        return Some(cached.clone());
    }

    // For realm diagnostics we need ALL candidate declarations across
    // every workspace priority tier. The normal member-resolution path
    // `get_member_info_with_key` returns only realm-compatible members.
    let member_start = profile.is_some().then(Instant::now);
    let mut all_member_ids = collect_all_member_ids_for_type_key(
        semantic_model,
        &owner_type,
        &member_key,
        member_candidate_cache,
        owner_key_member_candidate_cache,
        owner_expansion_cache,
        #[allow(clippy::needless_option_as_deref)]
        profile.as_deref_mut(),
    );
    if let (Some(profile), Some(member_start)) = (profile.as_mut(), member_start) {
        profile.member_candidate_time += member_start.elapsed();
    }

    let db = semantic_model.get_db();
    let member_index = db.get_member_index();
    if let Some(LuaSemanticDeclId::Member(resolved_member_id)) = semantic_decl
        && let Some(resolved_member) = member_index.get_member(resolved_member_id)
        && resolved_member.get_key() == &member_key
        && let Some(resolved_owner) = member_index.get_current_owner(resolved_member_id)
    {
        let mut seen: HashSet<LuaMemberId> = all_member_ids.iter().copied().collect();
        push_cached_member_ids_for_owner_key(
            member_index,
            resolved_owner,
            &member_key,
            owner_key_member_candidate_cache,
            &mut all_member_ids,
            &mut seen,
        );
    }

    let mut realms = Vec::new();
    for member_id in all_member_ids {
        if context.is_cancelled() {
            return Some(realms);
        }
        let property_owner_id = LuaSemanticDeclId::Member(member_id);
        if let Some(realm) = resolve_decl_realm_cached(
            context,
            semantic_model,
            &property_owner_id,
            decl_annotation_cache,
            decl_realm_cache,
            precomputed_callee_realms,
        ) {
            push_unique_realm(&mut realms, realm);
        }
    }

    // Store result in member_realms_cache before returning
    member_realms_cache.insert(cache_key, realms.clone());
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
        return Some(ResolvedRealm::new(
            annotation_realm,
            RealmEvidence::ExplicitAnnotation,
        ));
    }

    Some(resolve_realm_at_position(metadata, decl_offset))
}

/// Cached wrapper around `resolve_decl_realm`. The result (including `None`) is memoized
/// per `LuaSemanticDeclId` so that the same member/decl is never resolved twice in a file pass.
fn resolve_decl_realm_cached(
    context: &DiagnosticContext,
    semantic_model: &SemanticModel,
    semantic_decl: &LuaSemanticDeclId,
    decl_annotation_cache: &mut DeclAnnotationRealmCache,
    decl_realm_cache: &mut DeclRealmCache,
    precomputed_callee_realms: Option<&PrecomputedCalleeRealmMap>,
) -> Option<ResolvedRealm> {
    // Use entry API to avoid double-lookup: if present return the stored value (Some or None).
    if let Some(cached) = decl_realm_cache.get(semantic_decl) {
        return *cached;
    }
    if let Some(resolved) = precomputed_callee_realms
        .and_then(|map| map.get(semantic_decl))
        .and_then(|realms| realms.first())
        .copied()
    {
        decl_realm_cache.insert(semantic_decl.clone(), Some(resolved));
        return Some(resolved);
    }
    let result = resolve_decl_realm(
        context,
        semantic_model,
        semantic_decl,
        decl_annotation_cache,
    );
    decl_realm_cache.insert(semantic_decl.clone(), result);
    result
}

fn resolve_decl_annotation_realm_at_offset(
    context: &DiagnosticContext,
    semantic_model: &SemanticModel,
    file_id: &FileId,
    offset: TextSize,
    decl_annotation_cache: &mut DeclAnnotationRealmCache,
) -> Option<GmodRealm> {
    if let Some(file_entries) = context
        .get_shared_data()
        .and_then(|shared_data| shared_data.decl_annotation_realms.get(file_id))
    {
        return file_entries
            .iter()
            .find(|entry| entry.range.contains(offset))
            .map(|entry| entry.realm);
    }

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

pub fn collect_decl_annotation_realms_for_file_precompute(
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
    member_candidate_cache: &mut MemberCandidateCache,
    owner_key_member_candidate_cache: &mut OwnerKeyMemberCandidateCache,
    owner_expansion_cache: &mut OwnerExpansionCache,
    mut profile: Option<&mut GmodRealmMisuseProfile>,
) -> Vec<LuaMemberId> {
    let cache_key = (owner_type.clone(), member_key.clone());
    if let Some(cached) = member_candidate_cache.get(&cache_key) {
        if let Some(profile) = profile.as_mut() {
            profile.member_candidate_cache_hits += 1;
        }
        return cached.clone();
    }
    if let Some(profile) = profile.as_mut() {
        profile.member_candidate_cache_misses += 1;
    }

    let db = semantic_model.get_db();
    let member_index = db.get_member_index();

    // Resolve the LuaMemberOwner from the type, using cached expansion.
    let owners = if let Some(cached) = owner_expansion_cache.get(owner_type) {
        cached.clone()
    } else {
        let owners = owner_type_to_member_owners(owner_type, db);
        owner_expansion_cache.insert(owner_type.clone(), owners.clone());
        owners
    };
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for owner in owners {
        push_cached_member_ids_for_owner_key(
            member_index,
            &owner,
            member_key,
            owner_key_member_candidate_cache,
            &mut result,
            &mut seen,
        );
    }

    member_candidate_cache.insert(cache_key, result.clone());
    result
}

fn push_cached_member_ids_for_owner_key(
    member_index: &crate::LuaMemberIndex,
    owner: &LuaMemberOwner,
    member_key: &LuaMemberKey,
    owner_key_member_candidate_cache: &mut OwnerKeyMemberCandidateCache,
    result: &mut Vec<LuaMemberId>,
    seen: &mut HashSet<LuaMemberId>,
) -> bool {
    let cache_key = (owner.clone(), member_key.clone());
    if let Some(member_ids) = owner_key_member_candidate_cache.get(&cache_key) {
        if member_ids.is_empty() {
            return false;
        }

        for member_id in member_ids {
            if seen.insert(*member_id) {
                result.push(*member_id);
            }
        }
        return true;
    }

    let mut owner_key_member_ids = Vec::new();
    let found = push_member_ids_for_owner_key(
        member_index,
        owner,
        member_key,
        &mut owner_key_member_ids,
        &mut HashSet::new(),
    );
    owner_key_member_candidate_cache.insert(cache_key, owner_key_member_ids.clone());
    if !found {
        return false;
    }

    for member_id in owner_key_member_ids {
        if seen.insert(member_id) {
            result.push(member_id);
        }
    }
    true
}

fn push_member_ids_for_owner_key(
    member_index: &crate::LuaMemberIndex,
    owner: &LuaMemberOwner,
    member_key: &LuaMemberKey,
    result: &mut Vec<LuaMemberId>,
    seen: &mut HashSet<LuaMemberId>,
) -> bool {
    let indexed_members = member_index.get_members_for_owner_key(owner, member_key);
    if indexed_members.is_empty() {
        return false;
    }

    let may_have_collapsed_assignment_history = indexed_members.iter().any(|member| {
        member.get_feature().is_file_define()
            && member.get_syntax_id().get_kind() == glua_parser::LuaSyntaxKind::IndexExpr
    });

    for member in indexed_members {
        let member_id = member.get_id();
        if seen.insert(member_id) {
            result.push(member_id);
        }
    }

    if may_have_collapsed_assignment_history {
        for member in member_index.get_current_owner_members_for_key(owner, member_key) {
            let member_id = member.get_id();
            if seen.insert(member_id) {
                result.push(member_id);
            }
        }
    }

    true
}

/// Convert a `LuaType` into one or more `LuaMemberOwner` values to look up
/// members in the index. Mirrors the owner-resolution intent of
/// `find_members_guard` / `find_*_members` in `semantic::member` but returns
/// raw owners (no realm/workspace filtering, no alias/generic instantiation,
/// no member-info construction). Recursion is bounded by `depth` to guard
/// against pathological self-referential type graphs.
fn owner_type_to_member_owners(typ: &LuaType, db: &crate::DbIndex) -> Vec<LuaMemberOwner> {
    let mut visited = HashSet::new();
    owner_type_to_member_owners_inner(typ, db, &mut visited)
}

fn owner_type_to_member_owners_inner(
    typ: &LuaType,
    db: &crate::DbIndex,
    visited: &mut HashSet<LuaType>,
) -> Vec<LuaMemberOwner> {
    if !visited.insert(typ.clone()) {
        return Vec::new();
    }
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
                visited,
            ));
            owners
        }
        LuaType::TableOf(inner) => owner_type_to_member_owners_inner(inner, db, visited),
        LuaType::Union(union_type) => {
            let mut owners = Vec::new();
            for sub in union_type.types() {
                owners.extend(owner_type_to_member_owners_inner(sub, db, visited));
            }
            owners
        }
        LuaType::MultiLineUnion(multi_union) => {
            owner_type_to_member_owners_inner(&multi_union.to_union(), db, visited)
        }
        LuaType::Intersection(intersection_type) => {
            let mut owners = Vec::new();
            for sub in intersection_type.get_types().iter() {
                owners.extend(owner_type_to_member_owners_inner(sub, db, visited));
            }
            owners
        }
        LuaType::ModuleRef(file_id) => {
            if let Some(module_info) = db.get_module_index().get_module(*file_id)
                && let Some(export_type) = &module_info.export_type
            {
                owner_type_to_member_owners_inner(export_type, db, visited)
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
    let mut gm_method_realms = FxHashMap::default();

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
                ResolvedRealm::new(*realm, RealmEvidence::ExplicitAnnotation),
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

#[cfg(test)]
fn pick_best_mismatch_candidate(realms: &[ResolvedRealm]) -> ResolvedRealm {
    *realms
        .iter()
        .max_by_key(|realm| mismatch_candidate_sort_key(realm))
        .unwrap_or(&ResolvedRealm::new(
            GmodRealm::Unknown,
            RealmEvidence::Unknown,
        ))
}

fn pick_best_mismatch_candidate_for_call(
    call_realm: ResolvedRealm,
    realms: &[ResolvedRealm],
) -> Option<ResolvedRealm> {
    realms
        .iter()
        .copied()
        .filter(|callee| is_cross_realm_misuse(call_realm, *callee))
        .max_by_key(mismatch_candidate_sort_key)
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
        GmodRealm::Menu => 3,
        GmodRealm::Unknown => 4,
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

fn resolve_realm_at_position(metadata: &GmodRealmFileMetadata, offset: TextSize) -> ResolvedRealm {
    let file_realm = resolve_file_realm(metadata);
    if let Some(branch_realm) = metadata
        .branch_realm_ranges
        .iter()
        .find(|range| range.range.contains(offset))
        .map(|range| range.realm)
    {
        let branch_mask = branch_realm.state_mask();
        let state_mask = if file_realm.state_mask.is_empty() {
            branch_mask
        } else {
            branch_mask.intersection(file_realm.state_mask)
        };
        return ResolvedRealm::with_state_mask(
            branch_realm,
            state_mask,
            RealmEvidence::ExplicitBranch,
        );
    }

    file_realm
}

fn resolve_file_realm(metadata: &GmodRealmFileMetadata) -> ResolvedRealm {
    let realm = if metadata.inferred_realm != GmodRealm::Unknown {
        metadata.inferred_realm
    } else {
        metadata.annotation_realm.unwrap_or(GmodRealm::Unknown)
    };
    let state_mask = if let Some(annotation_realm) = metadata.annotation_realm {
        annotation_realm.state_mask()
    } else if !metadata.load_state_mask.is_empty() {
        metadata.load_state_mask
    } else if metadata.inferred_realm != GmodRealm::Unknown {
        metadata.inferred_realm.state_mask()
    } else {
        GmodStateMask::empty()
    };

    ResolvedRealm::with_state_mask(realm, state_mask, file_realm_evidence(metadata))
}

fn file_realm_evidence(metadata: &GmodRealmFileMetadata) -> RealmEvidence {
    if metadata.annotation_realm.is_some() {
        return RealmEvidence::ExplicitAnnotation;
    }

    if metadata
        .load_status
        .is_some_and(|status| status != crate::GmodLoadStatus::NoKnownLoadPath)
        && metadata.load_realm.is_some()
    {
        return RealmEvidence::InferredDependency;
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

fn is_cross_realm_misuse(call_realm: ResolvedRealm, callee_realm: ResolvedRealm) -> bool {
    call_realm.is_strictly_incompatible_with(callee_realm)
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
        DiagnosticCode::GmodRealmMismatch => format!(
            "Realm mismatch: calling `{name}` in {call_realm} realm but declaration is {decl_realm}.",
            name = call_name,
            call_realm = call_realm,
            decl_realm = callee_realm,
        )
        .to_string(),
        DiagnosticCode::GmodRealmMismatchHeuristic => format!(
            "Potential realm mismatch (heuristic): `{name}` is called in inferred {call_realm} realm while declaration is inferred {decl_realm}.",
            name = call_name,
            call_realm = call_realm,
            decl_realm = callee_realm,
        )
        .to_string(),
        DiagnosticCode::GmodUnknownRealm => format!(
            "Unable to resolve call realm for `{name}`; declaration appears to be {decl_realm}.",
            name = call_name,
            decl_realm = callee_realm,
        )
        .to_string(),
        _ => format!("Realm mismatch for `{name}`.", name = call_name).to_string(),
    }
}

fn realm_label(realm: GmodRealm) -> &'static str {
    match realm {
        GmodRealm::Client => "client",
        GmodRealm::Server => "server",
        GmodRealm::Shared => "shared",
        GmodRealm::Menu => "menu",
        GmodRealm::Unknown => "unknown",
    }
}

fn resolve_precomputed_decl_realm(
    db: &crate::DbIndex,
    semantic_decl: &LuaSemanticDeclId,
    decl_annotation_cache: &mut DeclAnnotationRealmCache,
) -> Option<ResolvedRealm> {
    let (decl_file_id, decl_offset) = semantic_decl_position(semantic_decl)?;
    let metadata = db
        .get_gmod_infer_index()
        .get_realm_file_metadata(&decl_file_id)?;
    if let Some(annotation_realm) = resolve_decl_annotation_realm_at_offset_from_db(
        db,
        &decl_file_id,
        decl_offset,
        decl_annotation_cache,
    ) {
        return Some(ResolvedRealm::new(
            annotation_realm,
            RealmEvidence::ExplicitAnnotation,
        ));
    }

    Some(resolve_realm_at_position(metadata, decl_offset))
}

/// Precompute declaration/member/signature realm facts for workspace diagnostics.
pub fn precompute_callee_realm_data_for_workspace(
    db: &crate::DbIndex,
    workspace_id: WorkspaceId,
    workspace_file_ids: &[FileId],
) -> PrecomputedCalleeRealmData {
    let module_index = db.get_module_index();
    let mut callee_realms = FxHashMap::default();
    let mut realm_call_candidates = PrecomputedRealmCallCandidates::default();
    let mut decl_annotation_cache = FxHashMap::default();
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
                    if let Some(decl) = db.get_decl_index().get_decl(&decl_id) {
                        realm_call_candidates.insert_realm(decl.get_name(), resolved);
                        if decl.is_global() {
                            realm_call_candidates.insert_access_path(decl.get_name(), resolved);
                        }
                    }
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
                if let Some(member) = db.get_member_index().get_member(&member_id)
                    && let Some(name) = member.get_key().get_name()
                {
                    realm_call_candidates.insert_realm(name, resolved);
                    if let Some(global_id) = member.get_global_id() {
                        realm_call_candidates.insert_access_path(global_id.get_name(), resolved);
                    }
                }
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

    PrecomputedCalleeRealmData {
        callee_realms,
        realm_call_candidates,
    }
}

/// Precompute GM method realm annotations for a specific workspace.
/// This is the same data that `collect_annotated_gm_method_realms` computes per-file,
/// but extracted to be called once per workspace during batch diagnostics.
pub fn precompute_gm_method_realms(
    db: &crate::db_index::DbIndex,
    workspace_id: WorkspaceId,
) -> GmMethodRealmMap {
    let mut gm_method_realms = FxHashMap::default();
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
                ResolvedRealm::new(*realm, RealmEvidence::ExplicitAnnotation),
            );
        }
    }

    gm_method_realms
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;

    use super::{
        PrecomputedRealmCallCandidates, RealmEvidence, ResolvedRealm, pick_best_mismatch_candidate,
        unknown_realm_candidate,
    };
    use crate::GmodRealm;

    #[gtest]
    fn pick_best_mismatch_candidate_uses_realm_tiebreaker_for_equal_evidence() {
        let candidates = vec![
            ResolvedRealm::new(GmodRealm::Client, RealmEvidence::ExplicitAnnotation),
            ResolvedRealm::new(GmodRealm::Server, RealmEvidence::ExplicitAnnotation),
        ];

        assert_that!(
            pick_best_mismatch_candidate(&candidates),
            eq(ResolvedRealm::new(
                GmodRealm::Server,
                RealmEvidence::ExplicitAnnotation
            ))
        );
    }

    #[gtest]
    fn pick_best_mismatch_candidate_keeps_evidence_priority_dominant() {
        let candidates = vec![
            ResolvedRealm::new(GmodRealm::Client, RealmEvidence::ExplicitBranch),
            ResolvedRealm::new(GmodRealm::Server, RealmEvidence::ExplicitAnnotation),
        ];

        assert_that!(
            pick_best_mismatch_candidate(&candidates),
            eq(ResolvedRealm::new(
                GmodRealm::Client,
                RealmEvidence::ExplicitBranch
            ))
        );
    }

    #[gtest]
    fn unknown_realm_candidate_uses_realm_tiebreaker_for_equal_evidence() {
        let call_realm = ResolvedRealm::new(GmodRealm::Unknown, RealmEvidence::Unknown);
        let callee_realms = vec![
            ResolvedRealm::new(GmodRealm::Client, RealmEvidence::ExplicitAnnotation),
            ResolvedRealm::new(GmodRealm::Server, RealmEvidence::ExplicitAnnotation),
        ];

        assert_that!(
            unknown_realm_candidate(call_realm, &callee_realms),
            some(eq(ResolvedRealm::new(
                GmodRealm::Server,
                RealmEvidence::ExplicitAnnotation
            )))
        );
    }

    #[gtest]
    fn precomputed_realm_call_candidates_skip_only_known_compatible_static_names() {
        let mut candidates = PrecomputedRealmCallCandidates::default();
        candidates.insert_realm(
            "SharedOnly",
            ResolvedRealm::new(GmodRealm::Shared, RealmEvidence::InferredDependency),
        );
        candidates.insert_realm(
            "ClientOnly",
            ResolvedRealm::new(GmodRealm::Client, RealmEvidence::InferredFilename),
        );
        let server_call = ResolvedRealm::new(GmodRealm::Server, RealmEvidence::InferredFilename);

        assert_that!(
            candidates.should_check_call(server_call, "SharedOnly"),
            eq(false)
        );
        assert_that!(
            candidates.should_check_call(server_call, "ClientOnly"),
            eq(true)
        );
        assert_that!(
            candidates.should_check_call(server_call, "Missing"),
            eq(true)
        );
    }
}
