//! The gmod derivations: what each one reads and writes.
//!
//! Workspace derivations fold per-file facts (call roles, the load graph and
//! file realms, valid-guard signatures, vgui parent chains). Per-file
//! derivations collect one file's metadata, call sites, synthesized members
//! and net flows, reading the workspace folds through the index.

use std::sync::Arc;

use rustc_hash::FxHashMap as HashMap;

use super::*;
use crate::db_index::GmodRealmInputs;
use crate::db_index::derivation::CallRoleKey;

/// The workspace call roles and net-helper registry, folded from every file's
/// helper scan.
pub(crate) struct GmodCallRoles {
    pub(super) registry: Arc<HelperRegistry>,
    pub(super) roles: Arc<AnnotatedGmodGlobalCallRoleMap>,
}

impl crate::db_index::derivation::DerivedFact for GmodCallRoles {
    fn fingerprint(&self) -> crate::db_index::derivation::Fingerprint {
        crate::db_index::derivation::fingerprint_of(&(
            self.registry.fingerprint(),
            self.roles.fingerprint(),
        ))
    }
}

impl crate::db_index::derivation::CallRoles for GmodCallRoles {
    fn entries(&self) -> Vec<CallRoleKey> {
        let paths = self
            .roles
            .roles_by_path
            .keys()
            .map(|path| CallRoleKey::Path(SmolStr::new(path)));
        let globals = self
            .registry
            .globals
            .keys()
            .cloned()
            .map(CallRoleKey::Global);
        let methods = self
            .registry
            .methods
            .keys()
            .cloned()
            .map(CallRoleKey::Method);
        let signatures = self
            .registry
            .signatures
            .keys()
            .copied()
            .map(CallRoleKey::Signature);
        paths
            .chain(globals)
            .chain(methods)
            .chain(signatures)
            .collect()
    }

    fn entry_fingerprint(&self, key: &CallRoleKey) -> crate::db_index::derivation::Fingerprint {
        use crate::db_index::derivation::fingerprint_opt;
        let site =
            |(file_id, syntax_id): &(FileId, LuaSyntaxId)| InFiled::new(*file_id, *syntax_id);
        match key {
            CallRoleKey::Path(path) if !crate::db_index::derivation::mapping_back() => self
                .roles
                .path_fingerprints
                .get(path.as_str())
                .copied()
                .unwrap_or(crate::db_index::derivation::ABSENT),
            CallRoleKey::Path(path) => fingerprint_opt(self.roles.roles_by_path.get(path.as_str())),
            CallRoleKey::Global(global_id) => {
                fingerprint_opt(self.registry.globals.get(global_id).map(site).as_ref())
            }
            CallRoleKey::Method(name) => {
                fingerprint_opt(self.registry.methods.get(name).map(site).as_ref())
            }
            CallRoleKey::Signature(signature_id) => fingerprint_opt(
                self.registry
                    .signatures
                    .get(signature_id)
                    .map(site)
                    .as_ref(),
            ),
        }
    }
}

fn empty_call_roles() -> Arc<GmodCallRoles> {
    Arc::new(GmodCallRoles {
        registry: Arc::new(HelperRegistry::default()),
        roles: Arc::new(AnnotatedGmodGlobalCallRoleMap::default()),
    })
}

/// The current call roles for a fold over them, or empty ones before the
/// fold first ran.
pub(super) fn call_roles(db: &DbIndex) -> Arc<GmodCallRoles> {
    db.get_gmod_call_roles::<GmodCallRoles>()
        .unwrap_or_else(empty_call_roles)
}

/// The current call roles for a per-file pass, which reads each entry it
/// looks up, or empty ones before the fold first ran.
pub(super) fn call_role_entries(db: &DbIndex) -> Arc<GmodCallRoles> {
    db.gmod_call_role_entries::<GmodCallRoles>()
        .unwrap_or_else(empty_call_roles)
}

/// Folds every file's helper scan into the call roles and the net-helper
/// registry.
pub(crate) fn evaluate_call_roles(db: &mut DbIndex) {
    if !db.get_emmyrc().gmod.enabled {
        return;
    }
    let (registry, roles) = build_call_roles_and_registry(db);
    db.get_gmod_network_index_mut()
        .set_canonical_ops(roles.canonical_net_ops());
    db.set_gmod_call_roles(Arc::new(GmodCallRoles { registry, roles }));
}

/// The functions that can produce a net operation, directly or through other
/// such functions.
pub(crate) fn evaluate_net_helper_names(db: &mut DbIndex) {
    if !db.get_emmyrc().gmod.enabled {
        return;
    }
    let roles = call_roles(db);
    let op_names = net_operation_names(&roles.roles);
    let mut names = net_producing_function_names(db, &op_names);
    names.extend(op_names);
    db.get_gmod_network_index_mut().set_helper_names(names);
}

/// One file's annotated `load`-style call sites (`include`, `AddCSLuaFile`).
pub(crate) fn evaluate_load_call_sites(db: &mut DbIndex, context: &mut AnalyzeContext) {
    if !db.get_emmyrc().gmod.enabled {
        return;
    }
    let prefixes = formatted_hook_prefixes(db);
    let roles = call_role_entries(db);
    collect_annotated_call_sites_with(db, context, &prefixes, &roles.roles, true, false);
}

/// One file's hook, system, realm and scoped-class metadata, its vgui
/// registrations and network-var wrappers.
pub(crate) fn evaluate_pre(db: &mut DbIndex, context: &mut AnalyzeContext) {
    if !db.get_emmyrc().gmod.enabled {
        return;
    }
    let roles = call_role_entries(db);
    let prefixes = formatted_hook_prefixes(db);
    let scripted_scope_files = context.get_or_compute_scripted_scope_files(db);
    let tree_list = context.tree_list.clone();
    let file_ids: Vec<FileId> = tree_list.iter().map(|tree| tree.file_id).collect();
    synthesize_vgui_registrations(db, context, &file_ids, VguiSynthesisPhase::Bind);

    for in_filed_tree in &tree_list {
        let file_id = in_filed_tree.file_id;
        let GmodFileMetadataResult {
            hook_metadata,
            member_ranges,
            branch_ranges,
            annotation_realm,
            file_params,
        } = collect_file_gmod_metadata(db, file_id, &prefixes, &roles.roles);
        if let Some((hook_sites, system_metadata, gm_method_realms)) = hook_metadata {
            db.get_gmod_infer_index_mut()
                .add_hook_sites(file_id, hook_sites);
            db.get_gmod_infer_index_mut()
                .set_system_file_metadata(file_id, system_metadata);
            if !gm_method_realms.is_empty() {
                db.get_gmod_infer_index_mut()
                    .set_gm_method_realm_annotations(file_id, gm_method_realms);
            }
        }
        if scripted_scope_files.contains(&file_id) {
            let scope_match = db
                .get_gmod_infer_index()
                .get_scoped_class_info(&file_id)
                .map(|info| GmodScopedClassMatch {
                    class_name: info.class_name.clone(),
                    global_name: info.global_name.clone(),
                    is_global_singleton: info.is_global_singleton,
                    aliases: info.aliases.clone(),
                    super_types: info.super_types.clone(),
                    class_name_prefix: info.class_name_prefix.clone(),
                })
                .or_else(|| detect_scoped_class_from_path(db, file_id));
            if let Some(scope_match) = scope_match {
                ensure_scoped_class_type_decl(
                    db,
                    file_id,
                    &scope_match.class_name,
                    &scope_match.global_name,
                    &scope_match.super_types,
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
        }
        db.get_gmod_infer_index_mut().set_realm_inputs(
            file_id,
            GmodRealmInputs {
                branch_realm_ranges: branch_ranges,
                annotation_realm,
            },
            context.metas.contains(&file_id),
        );
        db.get_gmod_infer_index_mut()
            .set_member_realm_ranges(file_id, member_ranges);
        if let Some(file_params) = file_params
            && !file_params.is_empty()
        {
            db.get_gmod_infer_index_mut()
                .set_file_params(file_id, file_params);
        }
    }

    let tree_map: HashMap<FileId, LuaChunk> = tree_list
        .iter()
        .map(|tree| (tree.file_id, tree.value.clone()))
        .collect();
    synthesize_network_var_wrappers(db, &scripted_scope_files, &tree_map);
}

/// The workspace load graph, execution environments and file realms.
pub(crate) fn evaluate_load(db: &mut DbIndex) {
    if !db.get_emmyrc().gmod.enabled {
        return;
    }
    let file_ids = db.get_vfs().get_all_local_file_ids();
    let mut branch_realm_ranges = HashMap::default();
    let mut annotation_realms = HashMap::default();
    for file_id in &file_ids {
        let Some(inputs) = db.get_gmod_infer_index().get_realm_inputs(file_id) else {
            continue;
        };
        branch_realm_ranges.insert(*file_id, inputs.branch_realm_ranges.clone());
        if let Some(realm) = inputs.annotation_realm {
            annotation_realms.insert(*file_id, realm);
        }
    }
    rebuild_compilefile_execution_environments(db);
    rebuild_gmod_load_index(db, &branch_realm_ranges, &file_ids);
    rebuild_realm_metadata(db, branch_realm_ranges, annotation_realms, &file_ids);
}

/// The workspace effective valid-guard signatures.
pub(crate) fn evaluate_valid_guards(db: &mut DbIndex) {
    rebuild_effective_valid_guard_signatures(db);
}

/// One file's annotated scripted-class and vgui parent call sites.
pub(crate) fn evaluate_class_call_sites(db: &mut DbIndex, context: &mut AnalyzeContext) {
    if !db.get_emmyrc().gmod.enabled {
        return;
    }
    let prefixes = formatted_hook_prefixes(db);
    let roles = call_role_entries(db);
    collect_annotated_call_sites_with(db, context, &prefixes, &roles.roles, false, true);
}

/// One file's synthesized scripted-class members, registrations and numeric
/// populations.
pub(crate) fn evaluate_post(db: &mut DbIndex, context: &mut AnalyzeContext) {
    if !db.get_emmyrc().gmod.enabled {
        return;
    }
    let scripted_scope_files = context.get_or_compute_scripted_scope_files(db);
    let file_ids: Vec<FileId> = context.tree_list.iter().map(|tree| tree.file_id).collect();
    resolve_getmember_network_var_delegations(db, &scripted_scope_files, context);
    synthesize_vgui_registrations(db, context, &file_ids, VguiSynthesisPhase::TransferMembers);
    synthesize_scripted_ent_registrations(db, &file_ids);
    synthesize_scripted_class_members(db, &scripted_scope_files, &file_ids);
    collect_numeric_range_table_populations(db, context);
}

/// The workspace vgui parent chains, from every file's parent calls.
pub(crate) fn evaluate_vgui_chains(db: &mut DbIndex, context: &mut AnalyzeContext) {
    if !db.get_emmyrc().gmod.enabled {
        return;
    }
    resolve_vgui_parent_relations(db, context);
}

/// One file's net message flows.
pub(crate) fn evaluate_network(db: &mut DbIndex, context: &mut AnalyzeContext) {
    if !db.get_emmyrc().gmod.enabled {
        return;
    }
    network::collect_network_flows(db, context);
}
