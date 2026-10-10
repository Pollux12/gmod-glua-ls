use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use log::debug;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use super::checker::{
    FileCalleeRealms, FileRealmAccessPaths, GmMethodRealmMap, OwnFieldRequirements,
    PrecomputedCalleeRealmMap, PrecomputedMissingRequiredFields, PrecomputedRealmCallCandidates,
    SharedDiagnosticData, SortedSendFlowCache, fold_gm_method_realms, fold_required_fields,
    gm_method_realm_annotations, own_field_requirements, own_fields_owner,
    precompute_property_name_candidates, precompute_sorted_send_flows, resolve_file_callee_realms,
};
use crate::db_index::derivation::{
    self, DerivationId, DerivationKind, FactKey, Fingerprint, HashedKey,
};
use crate::{DbIndex, FileId, LuaCompilation, LuaType, LuaTypeDeclId, WorkspaceId};

/// A value derived from the index with the fingerprint of every fact it read:
/// it holds while each of those fingerprints does, which is when computing it
/// again would read the same facts and give the same value.
struct Recorded<T> {
    value: T,
    reads: Box<[(HashedKey, Fingerprint)]>,
}

/// The derivation a unit records its reads as. Units compute on pool threads
/// that may hold another recording, which [`derivation::detached`] sets aside.
fn unit_id() -> DerivationId {
    DerivationId::workspace(DerivationKind::Diagnostics)
}

impl<T> Recorded<T> {
    /// Computes `f`, recording its reads apart from whatever this thread
    /// records, with the fingerprints they read now.
    fn compute(db: &DbIndex, f: impl FnOnce() -> T) -> Self {
        let (value, keys) = derivation::detached(unit_id(), f);
        let keys = keys
            .into_iter()
            .map(read_out_of_base)
            .collect::<FxHashSet<_>>();
        let reads = keys
            .into_iter()
            .map(|key| {
                let seen = db.fact_fingerprint(&key);
                (key, seen)
            })
            .collect();
        Self { value, reads }
    }

    fn holds(&self, db: &DbIndex) -> bool {
        derivation::detached(unit_id(), || {
            self.reads
                .iter()
                .all(|(key, seen)| db.fact_fingerprint(key) == *seen)
        })
        .0
    }
}

/// `key`, or the key it is read out of when that key's fingerprint covers
/// everything a read of `key` observes: one key per file or member instead of
/// one per position read.
fn read_out_of_base(key: HashedKey) -> HashedKey {
    let base = match key.key() {
        FactKey::GmodRealmAt(file_id, _) => FactKey::GmodRealm(*file_id),
        FactKey::GmodMemberRealmAt(file_id, _) => FactKey::GmodInferFile(*file_id),
        FactKey::MemberObject(member_id) | FactKey::MemberMarks(member_id) => {
            FactKey::Member(*member_id)
        }
        _ => return key,
    };
    HashedKey::new(base)
}

/// `value`, as `held` holds it when the two are equal, so a value computed
/// again unchanged keeps its identity for the values built from it.
fn keep_equal<T: PartialEq>(held: Option<&Arc<T>>, value: T) -> Arc<T> {
    match held {
        Some(held) if **held == value => held.clone(),
        _ => Arc::new(value),
    }
}

/// One file's callee realms and access paths, each kept apart so either can
/// stay unchanged while the other moves.
#[derive(Clone)]
struct FileRealms {
    callees: Arc<FileCalleeRealms>,
    access_paths: Arc<FileRealmAccessPaths>,
}

/// One main workspace's realm data, with the file units it was built from.
struct RealmWorkspace {
    files: Vec<(FileId, FileRealms)>,
    gm_method_realms: Arc<GmMethodRealmMap>,
    callee_realms: Arc<PrecomputedCalleeRealmMap>,
    realm_call_candidates: Arc<PrecomputedRealmCallCandidates>,
}

#[derive(Default)]
struct RealmState {
    file_realms: FxHashMap<FileId, Recorded<FileRealms>>,
    workspaces: HashMap<WorkspaceId, RealmWorkspace>,
}

/// One type's required fields, with the chain and own requirements they were
/// folded from.
struct RequiredFields {
    chain: Arc<Vec<LuaType>>,
    owns: Vec<Option<Arc<OwnFieldRequirements>>>,
    fields: Arc<HashSet<String>>,
}

#[derive(Default)]
struct RequiredFieldsState {
    chains: FxHashMap<LuaTypeDeclId, Recorded<Arc<Vec<LuaType>>>>,
    own_fields: FxHashMap<LuaTypeDeclId, Recorded<Option<Arc<OwnFieldRequirements>>>>,
    types: FxHashMap<LuaTypeDeclId, RequiredFields>,
    map: Option<Arc<PrecomputedMissingRequiredFields>>,
}

/// What the last refresh computed again rather than reused.
#[derive(Debug, Default, Clone)]
pub(crate) struct Rebuilt {
    pub(crate) realm_files: Vec<FileId>,
    pub(crate) type_chains: usize,
    pub(crate) own_fields: Vec<LuaTypeDeclId>,
    pub(crate) send_flows: bool,
}

#[derive(Default)]
struct CacheState {
    /// The index revision every unit was last found current at.
    checked: Option<u64>,
    shared: Option<Arc<SharedDiagnosticData>>,
    workspace_file_ids: Option<Arc<Vec<FileId>>>,
    realms: RealmState,
    required_fields: RequiredFieldsState,
    sorted_send_flows: Option<Recorded<Arc<SortedSendFlowCache>>>,
    rebuilt: Rebuilt,
}

/// [`SharedDiagnosticData`] kept across diagnostic runs as units that each
/// recorded the facts they read: per-file callee realms, per-type super
/// chains and own field requirements, and the send flows. An index revision
/// the units were already checked at reuses the data whole; any other checks
/// each unit's reads and computes again only the units whose facts moved, then
/// the workspace values built from units that changed. Units hold only while
/// what they read does, so the data equals a cold build's. The property name
/// candidates enumerate the property index, which no fact covers, so they are
/// computed again at every new revision. A poisoned mutex degrades to a cold
/// build, never to a stale answer.
#[derive(Clone, Default)]
pub struct SharedDiagnosticComponentCache {
    state: Arc<Mutex<CacheState>>,
}

impl SharedDiagnosticComponentCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn invalidate(&self) {
        if let Ok(mut state) = self.state.lock() {
            *state = CacheState::default();
        }
    }

    /// True when the data was last built or checked at `compilation`'s
    /// current index revision: it may be reused whole.
    pub fn is_fresh(&self, compilation: &LuaCompilation) -> bool {
        self.state.lock().is_ok_and(|state| {
            state.shared.is_some() && state.checked == Some(compilation.index_revision())
        })
    }

    /// What the last refresh computed again.
    #[cfg(test)]
    pub(crate) fn last_rebuilt(&self) -> Rebuilt {
        self.state
            .lock()
            .map(|state| state.rebuilt.clone())
            .unwrap_or_default()
    }

    /// The shared data for `compilation`'s index as it stands, reusing every
    /// unit whose reads still hold.
    pub(crate) fn shared_data(&self, compilation: &LuaCompilation) -> Arc<SharedDiagnosticData> {
        let revision = compilation.index_revision();
        let mut state = self
            .state
            .lock()
            .map(|mut state| std::mem::take(&mut *state))
            .unwrap_or_default();
        let current = state
            .shared
            .clone()
            .filter(|_| state.checked == Some(revision));
        let shared = match current {
            Some(shared) => shared,
            None => {
                let _phase = crate::profile::PhaseGuard::new("shared diagnostics: refresh");
                let shared = refresh(compilation.get_db(), &mut state);
                state.shared = Some(shared.clone());
                state.checked = Some(revision);
                shared
            }
        };
        if let Ok(mut held) = self.state.lock() {
            *held = state;
        }
        shared
    }
}

/// Brings every unit of `state` up to `db` and assembles the shared data.
fn refresh(db: &DbIndex, state: &mut CacheState) -> Arc<SharedDiagnosticData> {
    let module_index = db.get_module_index();
    let workspace_file_ids = module_index.get_main_workspace_file_ids();
    // Callee resolution for a main workspace can select declarations from
    // compatible library and std workspaces, so realms cover every local file
    // while `workspace_file_ids` stays limited to main files.
    let mut local_file_ids = db.get_vfs().get_all_local_file_ids();
    local_file_ids.sort_unstable();
    let mut main_workspace_ids = module_index.get_main_workspace_ids();
    main_workspace_ids.sort_by_key(|workspace_id| workspace_id.id);

    crate::compilation::analyzer::parallel::init_pool();
    let held_realms = std::mem::take(&mut state.realms);
    let held_required_fields = std::mem::take(&mut state.required_fields);
    let held_send_flows = state.sorted_send_flows.take();
    let mut realms = None;
    let mut required_fields = None;
    let mut sorted_send_flows = None;
    let mut property_name_candidates = None;
    rayon::scope(|s| {
        s.spawn(|_| {
            realms = Some(crate::profile::phase("shared diagnostics: realms", || {
                refresh_realms(db, &local_file_ids, &main_workspace_ids, held_realms)
            }));
        });
        s.spawn(|_| {
            required_fields = Some(crate::profile::phase(
                "shared diagnostics: required fields",
                || refresh_required_fields(db, held_required_fields),
            ));
        });
        s.spawn(|_| {
            sorted_send_flows = Some(crate::profile::phase(
                "shared diagnostics: send flows",
                || match held_send_flows {
                    Some(held) if held.holds(db) => (held, false),
                    _ => (
                        Recorded::compute(db, || {
                            Arc::new(precompute_sorted_send_flows(
                                db.get_gmod_network_index(),
                                db.get_vfs(),
                            ))
                        }),
                        true,
                    ),
                },
            ));
        });
        s.spawn(|_| {
            property_name_candidates = Some(crate::profile::phase(
                "shared diagnostics: property names",
                || Arc::new(precompute_property_name_candidates(db)),
            ));
        });
    });
    // `rayon::scope` propagates task panics, so every slot is filled here.
    let (realms, realm_files) = realms.expect("realm refresh ran");
    let (required_fields, type_chains, own_fields) =
        required_fields.expect("required fields refresh ran");
    let (sorted_send_flows, send_flows) = sorted_send_flows.expect("send flow refresh ran");
    let property_name_candidates = property_name_candidates.expect("property name candidates ran");

    state.rebuilt = Rebuilt {
        realm_files,
        type_chains,
        own_fields,
        send_flows,
    };
    debug!(
        "shared diagnostic data: {} file realms, {} type chains, {} own field sets computed again, send flows {}",
        state.rebuilt.realm_files.len(),
        state.rebuilt.type_chains,
        state.rebuilt.own_fields.len(),
        if state.rebuilt.send_flows {
            "computed again"
        } else {
            "reused"
        },
    );

    let workspace_file_ids = match state.workspace_file_ids.take() {
        Some(held) if *held == workspace_file_ids => held,
        _ => Arc::new(workspace_file_ids),
    };
    let mut gm_method_realms = HashMap::default();
    let mut callee_realms_by_workspace = HashMap::default();
    let mut realm_call_candidates_by_workspace = HashMap::default();
    for (workspace_id, workspace) in &realms.workspaces {
        gm_method_realms.insert(*workspace_id, workspace.gm_method_realms.clone());
        callee_realms_by_workspace.insert(*workspace_id, workspace.callee_realms.clone());
        realm_call_candidates_by_workspace
            .insert(*workspace_id, workspace.realm_call_candidates.clone());
    }
    let shared = Arc::new(SharedDiagnosticData {
        workspace_file_ids: workspace_file_ids.clone(),
        gm_method_realms,
        callee_realms_by_workspace,
        realm_call_candidates_by_workspace,
        missing_required_fields: required_fields
            .map
            .clone()
            .expect("required fields refresh builds the map"),
        property_name_candidates,
        sorted_send_flows: sorted_send_flows.value.clone(),
    });
    state.workspace_file_ids = Some(workspace_file_ids);
    state.realms = realms;
    state.required_fields = required_fields;
    state.sorted_send_flows = Some(sorted_send_flows);
    shared
}

/// Checks the realm unit of every local file a main workspace resolves
/// callees into, computes again those whose reads moved, and rebuilds each
/// main workspace's realm data from units that changed. Returns the files
/// computed again.
fn refresh_realms(
    db: &DbIndex,
    local_file_ids: &[FileId],
    main_workspace_ids: &[WorkspaceId],
    mut held: RealmState,
) -> (RealmState, Vec<FileId>) {
    let module_index = db.get_module_index();
    let includes = |workspace_id: WorkspaceId, file_id: FileId| {
        let candidate_workspace_id = module_index
            .get_workspace_id(file_id)
            .unwrap_or(WorkspaceId::MAIN);
        module_index
            .workspace_resolution_priority(workspace_id, candidate_workspace_id)
            .is_some()
    };
    let resolve_files = local_file_ids
        .iter()
        .copied()
        .filter(|&file_id| {
            main_workspace_ids
                .iter()
                .any(|&workspace_id| includes(workspace_id, file_id))
        })
        .collect::<Vec<_>>();
    let units = resolve_files
        .iter()
        .map(|file_id| (*file_id, held.file_realms.remove(file_id)))
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|(file_id, held)| match held {
            Some(held) if held.holds(db) => (file_id, held, false),
            held => (
                file_id,
                compute_file_realms(db, file_id, held.as_ref()),
                true,
            ),
        })
        .collect::<Vec<_>>();
    let rebuilt = units
        .iter()
        .filter(|(_, _, rebuilt)| *rebuilt)
        .map(|(file_id, _, _)| *file_id)
        .collect();
    let file_realms = units
        .into_iter()
        .map(|(file_id, unit, _)| (file_id, unit))
        .collect::<FxHashMap<_, _>>();

    let annotations = gm_method_realm_annotations(db);
    let mut workspaces = HashMap::default();
    for &workspace_id in main_workspace_ids {
        let files = resolve_files
            .iter()
            .filter(|file_id| includes(workspace_id, **file_id))
            .map(|file_id| (*file_id, file_realms[file_id].value.clone()))
            .collect::<Vec<_>>();
        let held_workspace = held.workspaces.remove(&workspace_id);
        let gm_method_realms = keep_equal(
            held_workspace.as_ref().map(|held| &held.gm_method_realms),
            fold_gm_method_realms(&annotations, |file_id| includes(workspace_id, file_id)),
        );
        let callee_realms = match &held_workspace {
            Some(held) if same_parts(&held.files, &files, |realms| &realms.callees) => {
                held.callee_realms.clone()
            }
            _ => Arc::new(PrecomputedCalleeRealmMap::from_files(
                files
                    .iter()
                    .map(|(file_id, realms)| (*file_id, realms.callees.clone())),
            )),
        };
        let realm_call_candidates = match &held_workspace {
            Some(held)
                if Arc::ptr_eq(&held.gm_method_realms, &gm_method_realms)
                    && same_parts(&held.files, &files, |realms| &realms.access_paths) =>
            {
                held.realm_call_candidates.clone()
            }
            _ => Arc::new(PrecomputedRealmCallCandidates::from_access_paths(
                files.iter().map(|(_, realms)| realms.access_paths.as_ref()),
                &gm_method_realms,
            )),
        };
        workspaces.insert(
            workspace_id,
            RealmWorkspace {
                files,
                gm_method_realms,
                callee_realms,
                realm_call_candidates,
            },
        );
    }
    (
        RealmState {
            file_realms,
            workspaces,
        },
        rebuilt,
    )
}

/// `file_id`'s realm unit computed again, keeping the parts of `held` that
/// came out equal.
fn compute_file_realms(
    db: &DbIndex,
    file_id: FileId,
    held: Option<&Recorded<FileRealms>>,
) -> Recorded<FileRealms> {
    let Recorded {
        value: (callees, access_paths),
        reads,
    } = Recorded::compute(db, || resolve_file_callee_realms(db, file_id));
    let held = held.map(|held| &held.value);
    Recorded {
        value: FileRealms {
            callees: keep_equal(held.map(|held| &held.callees), callees),
            access_paths: keep_equal(held.map(|held| &held.access_paths), access_paths),
        },
        reads,
    }
}

/// Whether `held` and `files` list the same files with the same `part` each.
fn same_parts<T>(
    held: &[(FileId, FileRealms)],
    files: &[(FileId, FileRealms)],
    part: impl Fn(&FileRealms) -> &Arc<T>,
) -> bool {
    held.len() == files.len()
        && held
            .iter()
            .zip(files)
            .all(|((held_id, held), (file_id, realms))| {
                held_id == file_id && Arc::ptr_eq(part(held), part(realms))
            })
}

/// Checks every type's super chain and the own requirements of every type a
/// chain names, computes again those whose reads moved, and folds again the
/// required fields of each type whose chain or own requirements changed.
/// Returns how many chains, and which own requirements, were computed again.
fn refresh_required_fields(
    db: &DbIndex,
    mut held: RequiredFieldsState,
) -> (RequiredFieldsState, usize, Vec<LuaTypeDeclId>) {
    let type_ids = db
        .get_type_index()
        .get_all_types()
        .into_iter()
        .map(|type_decl| type_decl.get_id())
        .collect::<Vec<_>>();

    let chains = type_ids
        .iter()
        .map(|type_id| (type_id.clone(), held.chains.remove(type_id)))
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|(type_id, held)| match held {
            Some(held) if held.holds(db) => (type_id, held, false),
            held => {
                let Recorded { value, reads } = Recorded::compute(db, || {
                    type_id.collect_super_types_with_self(db, LuaType::Ref(type_id.clone()))
                });
                let value = keep_equal(held.as_ref().map(|held| &held.value), value);
                (type_id, Recorded { value, reads }, true)
            }
        })
        .collect::<Vec<_>>();
    let rebuilt_chains = chains.iter().filter(|(_, _, rebuilt)| *rebuilt).count();

    let mut owner_ids = FxHashSet::default();
    for (_, chain, _) in &chains {
        owner_ids.extend(chain.value.iter().filter_map(own_fields_owner).cloned());
    }
    let own_fields = owner_ids
        .into_iter()
        .map(|owner_id| {
            let held = held.own_fields.remove(&owner_id);
            (owner_id, held)
        })
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|(owner_id, held)| match held {
            Some(held) if held.holds(db) => (owner_id, held, false),
            held => {
                let Recorded { value, reads } =
                    Recorded::compute(db, || own_field_requirements(db, &owner_id, &mut || false));
                let held = held.and_then(|held| held.value);
                let value = value.map(|own| keep_equal(held.as_ref(), own));
                (owner_id, Recorded { value, reads }, true)
            }
        })
        .collect::<Vec<_>>();
    let mut rebuilt_own_fields = own_fields
        .iter()
        .filter(|(_, _, rebuilt)| *rebuilt)
        .map(|(owner_id, _, _)| owner_id.clone())
        .collect::<Vec<_>>();
    rebuilt_own_fields.sort_unstable_by(|left, right| left.stable_cmp(right));
    let own_fields = own_fields
        .into_iter()
        .map(|(owner_id, unit, _)| (owner_id, unit))
        .collect::<FxHashMap<_, _>>();
    let own =
        |owner_id: &LuaTypeDeclId| own_fields.get(owner_id).and_then(|unit| unit.value.clone());

    let held_types = held.types.len();
    let types = chains
        .iter()
        .map(|(type_id, chain, _)| (type_id, &chain.value, held.types.remove(type_id)))
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|(type_id, chain, held)| {
            let owns = chain
                .iter()
                .map(|typ| own_fields_owner(typ).and_then(&own))
                .collect::<Vec<_>>();
            match held {
                Some(held) if Arc::ptr_eq(&held.chain, chain) && same_owns(&held.owns, &owns) => {
                    (type_id.clone(), held, false)
                }
                held => {
                    let fields =
                        fold_required_fields(chain, &mut || false, |owner_id, _| own(owner_id));
                    let fields = keep_equal(held.as_ref().map(|held| &held.fields), fields);
                    let changed = !held
                        .as_ref()
                        .is_some_and(|held| Arc::ptr_eq(&held.fields, &fields));
                    let entry = RequiredFields {
                        chain: chain.clone(),
                        owns,
                        fields,
                    };
                    (type_id.clone(), entry, changed)
                }
            }
        })
        .collect::<Vec<_>>();

    let unchanged = types.len() == held_types && types.iter().all(|(_, _, changed)| !changed);
    let map = match held.map.take() {
        Some(map) if unchanged => map,
        _ => Arc::new(
            types
                .iter()
                .map(|(type_id, entry, _)| (type_id.clone(), entry.fields.clone()))
                .collect(),
        ),
    };
    let state = RequiredFieldsState {
        chains: chains
            .into_iter()
            .map(|(type_id, chain, _)| (type_id, chain))
            .collect(),
        own_fields,
        types: types
            .into_iter()
            .map(|(type_id, entry, _)| (type_id, entry))
            .collect(),
        map: Some(map),
    };
    (state, rebuilt_chains, rebuilt_own_fields)
}

fn same_owns(
    held: &[Option<Arc<OwnFieldRequirements>>],
    owns: &[Option<Arc<OwnFieldRequirements>>],
) -> bool {
    held.len() == owns.len()
        && held.iter().zip(owns).all(|pair| match pair {
            (Some(held), Some(own)) => Arc::ptr_eq(held, own),
            (None, None) => true,
            _ => false,
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lsp_types::{Diagnostic, Uri};
    use tokio_util::sync::CancellationToken;

    use super::SharedDiagnosticComponentCache;
    use crate::{
        DiagnosticCode, Emmyrc, FileId, LuaTypeDeclId, SharedDiagnosticData, VirtualWorkspace,
    };

    fn workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.diagnostics.enables = vec![
            DiagnosticCode::MissingFields,
            DiagnosticCode::GmodRealmMismatch,
            DiagnosticCode::GmodRealmMismatchHeuristic,
        ];
        ws.update_emmyrc(emmyrc);
        ws
    }

    fn write(ws: &mut VirtualWorkspace, uri: &Uri, text: &str) -> FileId {
        ws.analysis
            .update_file_by_uri(uri, Some(text.to_string()))
            .map(|(id, _)| id)
            .expect("file indexed")
    }

    fn diagnose(
        ws: &VirtualWorkspace,
        files: &[FileId],
        shared: &Arc<SharedDiagnosticData>,
    ) -> Vec<Vec<Diagnostic>> {
        files
            .iter()
            .map(|file_id| {
                ws.analysis
                    .diagnose_file_with_shared(*file_id, CancellationToken::new(), shared.clone())
                    .unwrap_or_default()
            })
            .collect()
    }

    /// Builds the shared data through `cache` and checks every file diagnoses
    /// the same against it as against a cold build.
    fn build_matching_cold(
        ws: &VirtualWorkspace,
        cache: &SharedDiagnosticComponentCache,
        files: &[FileId],
    ) -> Vec<Vec<Diagnostic>> {
        let warm = ws
            .analysis
            .diagnostic
            .precompute_shared_data_with_cache(&ws.analysis.compilation, cache);
        let cold = ws.analysis.precompute_diagnostic_shared_data();
        let diagnostics = diagnose(ws, files, &warm);
        assert_eq!(
            diagnostics,
            diagnose(ws, files, &cold),
            "diagnostics against reused shared data equal a cold build's",
        );
        diagnostics
    }

    fn point() -> LuaTypeDeclId {
        LuaTypeDeclId::global("Point")
    }

    #[test]
    fn an_edit_computes_again_only_the_units_whose_reads_moved() {
        let mut ws = workspace();
        let point_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/shared/point.lua");
        let point_file = write(
            &mut ws,
            &point_uri,
            "---@class Point\n---@field x number\nlocal Point = {}\n",
        );
        let user_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/shared/user.lua");
        let user_file = write(&mut ws, &user_uri, "---@type Point\nlocal p = {}\n");
        let other_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/shared/other.lua");
        let other_file = write(&mut ws, &other_uri, "function Gamma() return 1 end\n");
        let api_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/sv_api.lua");
        let api_file = write(
            &mut ws,
            &api_uri,
            "function ServerOnlyApi() return true end\n",
        );
        let caller_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/client/cl_use.lua");
        let caller_file = write(&mut ws, &caller_uri, "ServerOnlyApi()\n");
        let files = [point_file, user_file, other_file, api_file, caller_file];

        let cache = SharedDiagnosticComponentCache::new();
        let cold = build_matching_cold(&ws, &cache, &files);
        assert!(!cold[1].is_empty(), "the literal misses `x`");
        assert!(!cold[4].is_empty(), "the client calls a server function");
        let rebuilt = cache.last_rebuilt();
        assert!(files.iter().all(|file| rebuilt.realm_files.contains(file)));
        assert!(rebuilt.own_fields.contains(&point()));
        assert!(rebuilt.send_flows);
        assert!(cache.is_fresh(&ws.analysis.compilation));

        write(
            &mut ws,
            &other_uri,
            "function Gamma() return 2 end\nfunction Delta() return 3 end\n",
        );
        assert!(!cache.is_fresh(&ws.analysis.compilation));
        assert_eq!(build_matching_cold(&ws, &cache, &files), cold);
        let rebuilt = cache.last_rebuilt();
        assert!(rebuilt.realm_files.contains(&other_file));
        for file in [point_file, user_file, api_file, caller_file] {
            assert!(!rebuilt.realm_files.contains(&file));
        }
        assert!(rebuilt.own_fields.is_empty(), "no type's members moved");
        assert!(!rebuilt.send_flows, "no net flow moved");

        write(
            &mut ws,
            &point_uri,
            "---@class Point\n---@field x number\n---@field y number\nlocal Point = {}\n",
        );
        let edited = build_matching_cold(&ws, &cache, &files);
        assert_ne!(edited[1], cold[1], "the literal now misses `y` too");
        let rebuilt = cache.last_rebuilt();
        assert!(rebuilt.own_fields.contains(&point()));
        assert!(rebuilt.realm_files.contains(&point_file));
        assert!(!rebuilt.realm_files.contains(&other_file));
    }

    #[test]
    fn a_net_send_computes_the_send_flows_again() {
        let mut ws = workspace();
        ws.def_gmod_call_arg_builtins();
        let sender_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/sv_send.lua");
        let sender_file = write(&mut ws, &sender_uri, "local x = 1\n");
        let files = [sender_file];
        let cache = SharedDiagnosticComponentCache::new();
        build_matching_cold(&ws, &cache, &files);

        write(
            &mut ws,
            &sender_uri,
            "util.AddNetworkString(\"Ping\")\nnet.Start(\"Ping\")\nnet.WriteString(\"a\")\nnet.Broadcast()\n",
        );
        build_matching_cold(&ws, &cache, &files);
        assert!(cache.last_rebuilt().send_flows);
        let shared = ws
            .analysis
            .diagnostic
            .precompute_shared_data_with_cache(&ws.analysis.compilation, &cache);
        assert!(shared.sorted_send_flows.contains_key("Ping"));
    }

    #[test]
    fn invalidation_drops_every_unit() {
        let mut ws = workspace();
        let uri = ws.virtual_url_generator.new_uri("lua/autorun/shared/a.lua");
        let file = write(&mut ws, &uri, "function Alpha() return 1 end\n");
        let cache = SharedDiagnosticComponentCache::new();
        build_matching_cold(&ws, &cache, &[file]);
        cache.invalidate();
        assert!(!cache.is_fresh(&ws.analysis.compilation));
        build_matching_cold(&ws, &cache, &[file]);
        assert!(cache.last_rebuilt().realm_files.contains(&file));
    }
}
