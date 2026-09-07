use rustc_hash::FxHashMap;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use log::info;

pub use super::checker::DiagnosticContext;
use super::checker::precompute_await_candidates;
use super::checker::precompute_callee_realm_data_for_workspace;
use super::checker::precompute_gm_method_realms;
use super::checker::precompute_missing_required_fields;
use super::checker::precompute_nodiscard_candidates;
use super::checker::precompute_param_type_candidates;
use super::checker::precompute_property_name_candidates;
use super::checker::precompute_sorted_send_flows;
use super::checker::{
    GmMethodRealmMap, PrecomputedAwaitCandidates, PrecomputedCalleeRealmMap,
    PrecomputedMissingRequiredFields, PrecomputedNoDiscardCandidates,
    PrecomputedParamTypeCandidates, PrecomputedPropertyNameCandidates,
    PrecomputedRealmCallCandidates, SharedDiagnosticData, SortedSendFlowCache,
};
use super::{checker::check_file, lua_diagnostic_config::LuaDiagnosticConfig};
use crate::semantic::LuaAnalysisPhase;
use crate::{DbIndex, DiagnosticCode, Emmyrc, FileId, LuaCompilation, WorkspaceId};
use lsp_types::Diagnostic;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub struct LuaDiagnostic {
    enable: bool,
    config: Arc<LuaDiagnosticConfig>,
    workspace_configs: FxHashMap<WorkspaceId, Arc<LuaDiagnosticConfig>>,
}

impl Default for LuaDiagnostic {
    fn default() -> Self {
        Self::new()
    }
}

/// Revision snapshot gating every index-derived diagnostic component.
///
/// `type_structure` moves on any type/member/signature/module mutation, so a
/// matching snapshot proves none of those moved; the remaining counters tell
/// whole-workspace rebuilds apart at a glance. Indexes without a public
/// revision (properties, network flows, dynamic fields, decls, gmod
/// metadata) are covered transitively: their writes land through re-analysis
/// batches that also move `type_structure` or the file lists. Explicit cache
/// invalidation on reindex stays authoritative; these keys only let a quiet
/// workspace reuse its components.
#[derive(Clone, PartialEq, Eq)]
struct DiagnosticRevisionSnapshot {
    type_structure: u64,
    type_writes: u64,
    member_homing: u64,
    signature_count: usize,
    main_workspace_file_ids: Vec<FileId>,
    local_file_ids: Vec<FileId>,
    main_workspace_ids: Vec<WorkspaceId>,
}

impl DiagnosticRevisionSnapshot {
    fn capture(
        db: &DbIndex,
        main_workspace_file_ids: Vec<FileId>,
        local_file_ids: Vec<FileId>,
        main_workspace_ids: Vec<WorkspaceId>,
    ) -> Self {
        Self {
            type_structure: db.type_structure_revision(),
            type_writes: db.get_type_index().type_writes(),
            member_homing: db.get_member_index().homing_revision(),
            signature_count: db.get_signature_index().indexed_signature_count(),
            main_workspace_file_ids,
            local_file_ids,
            main_workspace_ids,
        }
    }
}

/// One workspace's share of the realm bundle: the file-pure fragment. Each
/// workspace derives from the workspace's own files through the same public
/// per-workspace precomputes as the uncached path, and merging is three map
/// inserts, so fragment-built bundles equal directly-built ones.
#[derive(Clone)]
struct RealmWorkspaceFragment {
    gm_method_realms: Arc<GmMethodRealmMap>,
    callee_realms: Arc<PrecomputedCalleeRealmMap>,
    realm_call_candidates: Arc<PrecomputedRealmCallCandidates>,
}

#[derive(Default)]
struct DiagnosticComponentSlots {
    workspace_file_ids: Option<(Vec<FileId>, Arc<Vec<FileId>>)>,
    realm_fragments: Option<(
        DiagnosticRevisionSnapshot,
        HashMap<WorkspaceId, RealmWorkspaceFragment>,
    )>,
    missing_required_fields: Option<(
        DiagnosticRevisionSnapshot,
        Arc<PrecomputedMissingRequiredFields>,
    )>,
    await_candidates: Option<(DiagnosticRevisionSnapshot, Arc<PrecomputedAwaitCandidates>)>,
    param_type_candidates: Option<(
        DiagnosticRevisionSnapshot,
        Arc<PrecomputedParamTypeCandidates>,
    )>,
    nodiscard_candidates: Option<(
        DiagnosticRevisionSnapshot,
        Arc<PrecomputedNoDiscardCandidates>,
    )>,
    property_name_candidates: Option<(
        DiagnosticRevisionSnapshot,
        Arc<PrecomputedPropertyNameCandidates>,
    )>,
    sorted_send_flows: Option<(DiagnosticRevisionSnapshot, Arc<SortedSendFlowCache>)>,
}

/// Components already fresh for a snapshot, cloned out from under one lock
/// before the missing ones compute.
#[derive(Default)]
struct FreshComponents {
    workspace_file_ids: Option<Arc<Vec<FileId>>>,
    realm_fragments: Option<HashMap<WorkspaceId, RealmWorkspaceFragment>>,
    missing_required_fields: Option<Arc<PrecomputedMissingRequiredFields>>,
    await_candidates: Option<Arc<PrecomputedAwaitCandidates>>,
    param_type_candidates: Option<Arc<PrecomputedParamTypeCandidates>>,
    nodiscard_candidates: Option<Arc<PrecomputedNoDiscardCandidates>>,
    property_name_candidates: Option<Arc<PrecomputedPropertyNameCandidates>>,
    sorted_send_flows: Option<Arc<SortedSendFlowCache>>,
}

/// Independently cached [`SharedDiagnosticData`] components keyed by
/// [`DiagnosticRevisionSnapshot`]. Shared across diagnostic runs: a quiet
/// workspace reuses every component, an edit recomputes only what its key
/// mismatches (today: everything but the file list, whose key is the exact
/// file set). A poisoned mutex degrades to cold recomputation, never to a
/// stale answer.
#[derive(Clone, Default)]
pub struct SharedDiagnosticComponentCache {
    slots: Arc<Mutex<DiagnosticComponentSlots>>,
}

impl SharedDiagnosticComponentCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn invalidate(&self) {
        if let Ok(mut slots) = self.slots.lock() {
            *slots = DiagnosticComponentSlots::default();
        }
    }

    /// True when every slot is populated for `db`'s current snapshot: the
    /// cached bundle may be reused whole.
    pub fn is_fresh(&self, db: &DbIndex) -> bool {
        let module_index = db.get_module_index();
        let mut workspace_file_ids = module_index.get_main_workspace_file_ids();
        workspace_file_ids.sort_unstable();
        let mut local_file_ids = db.get_vfs().get_all_local_file_ids();
        local_file_ids.sort_unstable();
        let mut main_workspace_ids = module_index.get_main_workspace_ids();
        main_workspace_ids.sort_by_key(|workspace_id| workspace_id.id);
        let snapshot = DiagnosticRevisionSnapshot::capture(
            db,
            workspace_file_ids.clone(),
            local_file_ids,
            main_workspace_ids.clone(),
        );
        let Ok(slots) = self.slots.lock() else {
            return false;
        };
        slots
            .workspace_file_ids
            .as_ref()
            .is_some_and(|(file_ids, _)| *file_ids == workspace_file_ids)
            && slots
                .realm_fragments
                .as_ref()
                .is_some_and(|(key, fragments)| {
                    *key == snapshot
                        && main_workspace_ids
                            .iter()
                            .all(|id| fragments.contains_key(id))
                })
            && slots
                .missing_required_fields
                .as_ref()
                .is_some_and(|(key, _)| *key == snapshot)
            && slots
                .await_candidates
                .as_ref()
                .is_some_and(|(key, _)| *key == snapshot)
            && slots
                .param_type_candidates
                .as_ref()
                .is_some_and(|(key, _)| *key == snapshot)
            && slots
                .nodiscard_candidates
                .as_ref()
                .is_some_and(|(key, _)| *key == snapshot)
            && slots
                .property_name_candidates
                .as_ref()
                .is_some_and(|(key, _)| *key == snapshot)
            && slots
                .sorted_send_flows
                .as_ref()
                .is_some_and(|(key, _)| *key == snapshot)
    }

    fn take_fresh(
        &self,
        snapshot: &DiagnosticRevisionSnapshot,
        workspace_file_ids: &[FileId],
    ) -> FreshComponents {
        let Ok(slots) = self.slots.lock() else {
            return FreshComponents::default();
        };
        FreshComponents {
            workspace_file_ids: slots
                .workspace_file_ids
                .as_ref()
                .and_then(|(file_ids, shared)| {
                    (*file_ids == workspace_file_ids).then(|| shared.clone())
                }),
            realm_fragments: slots
                .realm_fragments
                .as_ref()
                .and_then(|(key, fragments)| (*key == *snapshot).then(|| fragments.clone())),
            missing_required_fields: slots
                .missing_required_fields
                .as_ref()
                .and_then(|(key, shared)| (*key == *snapshot).then(|| shared.clone())),
            await_candidates: slots
                .await_candidates
                .as_ref()
                .and_then(|(key, shared)| (*key == *snapshot).then(|| shared.clone())),
            param_type_candidates: slots
                .param_type_candidates
                .as_ref()
                .and_then(|(key, shared)| (*key == *snapshot).then(|| shared.clone())),
            nodiscard_candidates: slots
                .nodiscard_candidates
                .as_ref()
                .and_then(|(key, shared)| (*key == *snapshot).then(|| shared.clone())),
            property_name_candidates: slots
                .property_name_candidates
                .as_ref()
                .and_then(|(key, shared)| (*key == *snapshot).then(|| shared.clone())),
            sorted_send_flows: slots
                .sorted_send_flows
                .as_ref()
                .and_then(|(key, shared)| (*key == *snapshot).then(|| shared.clone())),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn store(
        &self,
        snapshot: &DiagnosticRevisionSnapshot,
        workspace_file_ids: &Arc<Vec<FileId>>,
        realm_fragments: HashMap<WorkspaceId, RealmWorkspaceFragment>,
        missing_required_fields: &Arc<PrecomputedMissingRequiredFields>,
        await_candidates: &Arc<PrecomputedAwaitCandidates>,
        param_type_candidates: &Arc<PrecomputedParamTypeCandidates>,
        nodiscard_candidates: &Arc<PrecomputedNoDiscardCandidates>,
        property_name_candidates: &Arc<PrecomputedPropertyNameCandidates>,
        sorted_send_flows: &Arc<SortedSendFlowCache>,
    ) {
        let Ok(mut slots) = self.slots.lock() else {
            return;
        };
        // The workspace file list is keyed by the exact file set rather than
        // the revision snapshot: file membership is its only input.
        slots.workspace_file_ids = Some((workspace_file_ids.to_vec(), workspace_file_ids.clone()));
        slots.realm_fragments = Some((snapshot.clone(), realm_fragments));
        slots.missing_required_fields = Some((snapshot.clone(), missing_required_fields.clone()));
        slots.await_candidates = Some((snapshot.clone(), await_candidates.clone()));
        slots.param_type_candidates = Some((snapshot.clone(), param_type_candidates.clone()));
        slots.nodiscard_candidates = Some((snapshot.clone(), nodiscard_candidates.clone()));
        slots.property_name_candidates = Some((snapshot.clone(), property_name_candidates.clone()));
        slots.sorted_send_flows = Some((snapshot.clone(), sorted_send_flows.clone()));
    }

    /// Arc identities per slot, in a fixed order, for pinning reuse behavior
    /// in tests. Realm fragments report per workspace, sorted by workspace id.
    #[cfg(test)]
    fn identity_snapshot(&self) -> Vec<usize> {
        fn addr<T>(arc: &Arc<T>) -> usize {
            Arc::as_ptr(arc) as usize
        }
        let Ok(slots) = self.slots.lock() else {
            return Vec::new();
        };
        let mut identities = Vec::new();
        identities.push(
            slots
                .workspace_file_ids
                .as_ref()
                .map_or(0, |(_, shared)| addr(shared)),
        );
        if let Some((_, fragments)) = slots.realm_fragments.as_ref() {
            let mut workspace_ids: Vec<_> = fragments.keys().collect();
            workspace_ids.sort_by_key(|workspace_id| workspace_id.id);
            for workspace_id in workspace_ids {
                let fragment = &fragments[workspace_id];
                identities.push(addr(&fragment.gm_method_realms));
                identities.push(addr(&fragment.callee_realms));
                identities.push(addr(&fragment.realm_call_candidates));
            }
        }
        identities.push(
            slots
                .missing_required_fields
                .as_ref()
                .map_or(0, |(_, shared)| addr(shared)),
        );
        identities.push(
            slots
                .await_candidates
                .as_ref()
                .map_or(0, |(_, shared)| addr(shared)),
        );
        identities.push(
            slots
                .param_type_candidates
                .as_ref()
                .map_or(0, |(_, shared)| addr(shared)),
        );
        identities.push(
            slots
                .nodiscard_candidates
                .as_ref()
                .map_or(0, |(_, shared)| addr(shared)),
        );
        identities.push(
            slots
                .property_name_candidates
                .as_ref()
                .map_or(0, |(_, shared)| addr(shared)),
        );
        identities.push(
            slots
                .sorted_send_flows
                .as_ref()
                .map_or(0, |(_, shared)| addr(shared)),
        );
        identities
    }
}

impl LuaDiagnostic {
    pub fn new() -> Self {
        Self {
            enable: true,
            config: Arc::new(LuaDiagnosticConfig::default()),
            workspace_configs: FxHashMap::default(),
        }
    }

    pub fn update_config(&mut self, emmyrc: Arc<Emmyrc>) {
        self.enable = emmyrc.diagnostics.enable;
        self.config = LuaDiagnosticConfig::new(&emmyrc).into();
        self.workspace_configs.clear();
    }

    pub fn set_workspace_configs(
        &mut self,
        configs: HashMap<WorkspaceId, Arc<LuaDiagnosticConfig>>,
    ) {
        self.workspace_configs = configs.into_iter().collect();
    }

    // 只开启指定的诊断
    pub fn enable_only(&mut self, code: DiagnosticCode) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.diagnostics.enables.push(code);
        for diagnostic_code in DiagnosticCode::all().iter() {
            if *diagnostic_code != code {
                emmyrc.diagnostics.disable.push(*diagnostic_code);
            }
        }
        self.config = LuaDiagnosticConfig::new(&emmyrc).into();
    }

    fn get_config_for_file(
        &self,
        compilation: &LuaCompilation,
        file_id: FileId,
    ) -> Arc<LuaDiagnosticConfig> {
        if !self.workspace_configs.is_empty() {
            let db = compilation.get_db();
            if let Some(workspace_id) = db.get_module_index().get_workspace_id(file_id) {
                if let Some(config) = self.workspace_configs.get(&workspace_id) {
                    return config.clone();
                }
            }
        }
        self.config.clone()
    }

    pub fn diagnose_file(
        &self,
        compilation: &LuaCompilation,
        file_id: FileId,
        cancel_token: CancellationToken,
    ) -> Option<Vec<Diagnostic>> {
        self.diagnose_file_inner(compilation, file_id, cancel_token, None)
    }

    pub fn diagnose_file_with_shared(
        &self,
        compilation: &LuaCompilation,
        file_id: FileId,
        cancel_token: CancellationToken,
        shared_data: Arc<SharedDiagnosticData>,
    ) -> Option<Vec<Diagnostic>> {
        self.diagnose_file_inner(compilation, file_id, cancel_token, Some(shared_data))
    }

    /// Precompute shared diagnostic data once for use across all files.
    /// This avoids per-file recomputation of workspace-wide annotations.
    pub fn precompute_shared_data(
        &self,
        compilation: &LuaCompilation,
    ) -> Arc<SharedDiagnosticData> {
        self.precompute_shared_data_with_cache(compilation, &SharedDiagnosticComponentCache::new())
    }

    /// [`precompute_shared_data`](Self::precompute_shared_data) reusing the
    /// components of a previous run whose revision keys still match. Each of
    /// the ten components carries its own key, so an edit that moves no
    /// relevant index still reuses every component, and a cold cache computes
    /// the missing components with the same parallel fan-out as the
    /// uncached path. Assembled values are identical either way: caching only
    /// skips recomputation, never re-derives.
    pub fn precompute_shared_data_with_cache(
        &self,
        compilation: &LuaCompilation,
        cache: &SharedDiagnosticComponentCache,
    ) -> Arc<SharedDiagnosticData> {
        let db = compilation.get_db();
        let module_index = db.get_module_index();
        let mut workspace_file_ids = module_index.get_main_workspace_file_ids();
        workspace_file_ids.sort_unstable();
        // Callee resolution for a main workspace can select declarations from
        // compatible library and std workspaces. Include those files in the
        // realm lookup precompute while keeping `workspace_file_ids` limited to
        // main files for diagnostics that intentionally scan only user code.
        let mut realm_candidate_file_ids = db.get_vfs().get_all_local_file_ids();
        realm_candidate_file_ids.sort_unstable();
        let mut main_workspace_ids = module_index.get_main_workspace_ids();
        main_workspace_ids.sort_by_key(|workspace_id| workspace_id.id);
        let snapshot = DiagnosticRevisionSnapshot::capture(
            db,
            workspace_file_ids.clone(),
            realm_candidate_file_ids.clone(),
            main_workspace_ids.clone(),
        );

        let fresh = cache.take_fresh(&snapshot, &workspace_file_ids);
        crate::compilation::analyzer::parallel::init_pool();
        let mut workspace_realm_data = None;
        let mut missing_required_fields = None;
        let mut await_candidates = None;
        let mut param_type_candidates = None;
        let mut nodiscard_candidates = None;
        let mut sorted_send_flows = None;
        let mut property_name_candidates = None;
        let realm_fragments = fresh.realm_fragments;
        let realm_candidate_file_ids_ref = &realm_candidate_file_ids;
        rayon::scope(|s| {
            s.spawn(|_| {
                // The per-workspace realm bundles are the file-pure fragments:
                // each workspace derives from the workspace's own files through
                // the same public per-workspace precomputes, and merging is
                // three map inserts. A finer per-file split is not exactly
                // reproducible from outside the checker (private per-file
                // resolution plus unmergeable candidate maps), so fragments
                // stay workspace-sized and reuse is gated on the shared key.
                let mut gm_method_realms = HashMap::default();
                let mut callee_realms_by_workspace = HashMap::default();
                let mut realm_call_candidates_by_workspace = HashMap::default();
                let mut fragments = realm_fragments.unwrap_or_default();
                for workspace_id in module_index.get_main_workspace_ids() {
                    if let Some(fragment) = fragments.get(&workspace_id) {
                        gm_method_realms.insert(workspace_id, fragment.gm_method_realms.clone());
                        callee_realms_by_workspace
                            .insert(workspace_id, fragment.callee_realms.clone());
                        realm_call_candidates_by_workspace
                            .insert(workspace_id, fragment.realm_call_candidates.clone());
                        continue;
                    }
                    let realms = Arc::new(precompute_gm_method_realms(db, workspace_id));
                    let mut callee_realm_data = precompute_callee_realm_data_for_workspace(
                        db,
                        workspace_id,
                        realm_candidate_file_ids_ref,
                    );
                    callee_realm_data
                        .realm_call_candidates
                        .insert_gm_method_realms(realms.as_ref());
                    gm_method_realms.insert(workspace_id, realms.clone());
                    let callee_realms = Arc::new(callee_realm_data.callee_realms);
                    let realm_call_candidates = Arc::new(callee_realm_data.realm_call_candidates);
                    callee_realms_by_workspace.insert(workspace_id, callee_realms.clone());
                    realm_call_candidates_by_workspace
                        .insert(workspace_id, realm_call_candidates.clone());
                    fragments.insert(
                        workspace_id,
                        RealmWorkspaceFragment {
                            gm_method_realms: realms,
                            callee_realms,
                            realm_call_candidates,
                        },
                    );
                }
                workspace_realm_data = Some((
                    gm_method_realms,
                    callee_realms_by_workspace,
                    realm_call_candidates_by_workspace,
                    fragments,
                ));
            });
            s.spawn(|_| {
                missing_required_fields = Some(
                    fresh
                        .missing_required_fields
                        .unwrap_or_else(|| Arc::new(precompute_missing_required_fields(db))),
                );
            });
            s.spawn(|_| {
                await_candidates = Some(
                    fresh
                        .await_candidates
                        .unwrap_or_else(|| Arc::new(precompute_await_candidates(db))),
                );
            });
            s.spawn(|_| {
                param_type_candidates = Some(
                    fresh
                        .param_type_candidates
                        .unwrap_or_else(|| Arc::new(precompute_param_type_candidates(db))),
                );
            });
            s.spawn(|_| {
                nodiscard_candidates = Some(
                    fresh
                        .nodiscard_candidates
                        .unwrap_or_else(|| Arc::new(precompute_nodiscard_candidates(db))),
                );
            });
            s.spawn(|_| {
                property_name_candidates = Some(
                    fresh
                        .property_name_candidates
                        .unwrap_or_else(|| Arc::new(precompute_property_name_candidates(db))),
                );
            });
            s.spawn(|_| {
                sorted_send_flows = Some(fresh.sorted_send_flows.unwrap_or_else(|| {
                    Arc::new(precompute_sorted_send_flows(
                        db.get_gmod_network_index(),
                        db.get_vfs(),
                    ))
                }));
            });
        });
        // `rayon::scope` propagates task panics, so every slot is filled here.
        let workspace_realm_data = workspace_realm_data.expect("workspace realm precompute ran");
        let missing_required_fields =
            missing_required_fields.expect("precompute_missing_required_fields ran");
        let await_candidates = await_candidates.expect("precompute_await_candidates ran");
        let param_type_candidates =
            param_type_candidates.expect("precompute_param_type_candidates ran");
        let nodiscard_candidates =
            nodiscard_candidates.expect("precompute_nodiscard_candidates ran");
        let sorted_send_flows = sorted_send_flows.expect("precompute_sorted_send_flows ran");
        let property_name_candidates =
            property_name_candidates.expect("precompute_property_name_candidates ran");
        let (
            gm_method_realms,
            callee_realms_by_workspace,
            realm_call_candidates_by_workspace,
            realm_fragments,
        ) = workspace_realm_data;
        // The workspace file list is keyed by the exact file set rather than
        // the revision snapshot: file membership is its only input.
        let workspace_file_ids = fresh
            .workspace_file_ids
            .unwrap_or_else(|| Arc::new(workspace_file_ids));
        cache.store(
            &snapshot,
            &workspace_file_ids,
            realm_fragments,
            &missing_required_fields,
            &await_candidates,
            &param_type_candidates,
            &nodiscard_candidates,
            &property_name_candidates,
            &sorted_send_flows,
        );
        Arc::new(SharedDiagnosticData {
            workspace_file_ids,
            gm_method_realms,
            callee_realms_by_workspace,
            realm_call_candidates_by_workspace,
            missing_required_fields,
            await_candidates,
            param_type_candidates,
            nodiscard_candidates,
            property_name_candidates,
            sorted_send_flows,
        })
    }

    fn diagnose_file_inner(
        &self,
        compilation: &LuaCompilation,
        file_id: FileId,
        cancel_token: CancellationToken,
        shared_data: Option<Arc<SharedDiagnosticData>>,
    ) -> Option<Vec<Diagnostic>> {
        if !self.enable {
            return None;
        }

        if cancel_token.is_cancelled() {
            return None;
        }

        let db = compilation.get_db();
        if let Some(workspace_id) = db.get_module_index().get_workspace_id(file_id)
            && !db.get_module_index().is_main_workspace_id(workspace_id)
        {
            return None;
        }

        let config = self.get_config_for_file(compilation, file_id);

        let slow_log_enabled = log::log_enabled!(log::Level::Info);
        let sem_start = slow_log_enabled.then(Instant::now);
        let semantic_model = compilation.get_semantic_model(file_id)?;
        // Set diagnostics phase so error results are cached (types are final,
        // no subsequent unresolve pass will change them). This prevents expensive
        // recomputation across diagnostic checkers.
        semantic_model.set_analysis_phase(LuaAnalysisPhase::Diagnostics);
        if let Some(sem_start) = sem_start {
            let sem_elapsed = sem_start.elapsed();
            if sem_elapsed.as_millis() > 10 {
                info!(
                    "diagnose_file: get_semantic_model cost {:?} for {:?}",
                    sem_elapsed, file_id
                );
            }
        }

        let mut context = if let Some(shared) = shared_data {
            DiagnosticContext::new_with_shared(file_id, db, config, cancel_token.clone(), shared)
        } else {
            DiagnosticContext::new(file_id, db, config, cancel_token.clone())
        };

        check_file(&mut context, &semantic_model, &cancel_token);

        if cancel_token.is_cancelled() {
            return None;
        }

        Some(context.get_diagnostics())
    }
}

#[cfg(test)]
mod component_cache_tests {
    use crate::VirtualWorkspace;

    use super::SharedDiagnosticComponentCache;

    fn build_shared(
        ws: &VirtualWorkspace,
        cache: &SharedDiagnosticComponentCache,
    ) -> std::sync::Arc<crate::SharedDiagnosticData> {
        ws.analysis
            .diagnostic
            .precompute_shared_data_with_cache(&ws.analysis.compilation, cache)
    }

    /// Components reuse their arcs while their keys match; an edit that moves
    /// the indexes recomputes the index-derived components while the exact
    /// file-set key keeps the workspace file list shared.
    #[test]
    fn component_cache_reuses_only_fresh_components() {
        let mut ws = VirtualWorkspace::new();
        let uri_a = ws.virtual_url_generator.new_uri("lua/autorun/shared/a.lua");
        ws.analysis
            .update_file_by_uri(&uri_a, Some("function Alpha() return 1 end".to_string()))
            .map(|(id, _)| id)
            .expect("first file indexed");
        let uri_b = ws.virtual_url_generator.new_uri("lua/autorun/shared/b.lua");
        ws.analysis
            .update_file_by_uri(&uri_b, Some("function Beta() return 2 end".to_string()))
            .map(|(id, _)| id)
            .expect("second file indexed");

        let cache = SharedDiagnosticComponentCache::new();
        let _first = build_shared(&ws, &cache);
        let cold_identities = cache.identity_snapshot();
        assert!(
            cold_identities.iter().all(|identity| *identity != 0),
            "a cold build populates every slot",
        );

        let _second = build_shared(&ws, &cache);
        assert_eq!(
            cold_identities,
            cache.identity_snapshot(),
            "a quiet workspace reuses every component",
        );

        ws.analysis
            .update_file_by_uri(
                &uri_a,
                Some("function Alpha() return 1 end\nfunction Gamma() return 3 end".to_string()),
            )
            .map(|(id, _)| id)
            .expect("edited file indexed");
        let _third = build_shared(&ws, &cache);
        let edited_identities = cache.identity_snapshot();
        assert_ne!(
            cold_identities, edited_identities,
            "editing a file recomputes the index-derived components",
        );
        assert_eq!(
            cold_identities[0], edited_identities[0],
            "the file set did not change, so the workspace file list stays shared",
        );
        assert!(
            cold_identities[1..] != edited_identities[1..],
            "the revision-gated components are rebuilt after the edit",
        );

        cache.invalidate();
        let _fourth = build_shared(&ws, &cache);
        assert_ne!(
            edited_identities,
            cache.identity_snapshot(),
            "invalidation drops every component",
        );
    }
}
