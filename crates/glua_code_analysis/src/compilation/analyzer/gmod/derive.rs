//! The gmod derivations: what each one reads and writes.
//!
//! Workspace derivations fold per-file facts (call roles, the load graph and
//! file realms, valid-guard signatures, vgui parent chains). Per-file
//! derivations collect one file's metadata, call sites, synthesized members
//! and net-helper names, reading the workspace folds through the index.

use std::sync::Arc;

use rustc_hash::FxHashMap as HashMap;

use super::*;
use crate::db_index::GmodRealmInputs;

/// The current call roles for a per-file pass, which reads each entry it
/// looks up, or empty ones before the fold first ran.
pub(super) fn call_role_entries(db: &DbIndex) -> Arc<GmodCallRoles> {
    db.gmod_call_role_entries::<GmodCallRoles>()
        .unwrap_or_default()
}

/// Folds every file's helper scan into the call roles and the net-helper
/// registry.
pub(crate) fn evaluate_call_roles(db: &mut DbIndex) {
    if !db.get_emmyrc().gmod.enabled {
        return;
    }
    let mut roles = db
        .take_gmod_call_roles::<GmodCallRoles>()
        .map(Arc::unwrap_or_clone)
        .unwrap_or_default();
    let changes = roles.fold(db);
    if changes.net_ops {
        let network = db.get_gmod_network_index_mut();
        network.set_canonical_ops(roles.roles.canonical_net_ops());
        network.set_op_names(net_operation_names(&roles.roles));
    }
    db.update_gmod_call_roles(Arc::new(roles), &changes.keys);
}

/// The functions that can produce a net operation, directly or through other
/// such functions.
pub(crate) fn evaluate_net_helper_names(db: &mut DbIndex) {
    if !db.get_emmyrc().gmod.enabled {
        return;
    }
    let op_names = db.get_gmod_network_index().op_names();
    let mut names = net_producing_function_names(db, &op_names);
    names.extend(op_names.iter().cloned());
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

/// One file's hook, system and realm metadata, read off the index; `None`
/// with gmod disabled, with what its scripted-class scope binds. [`apply_pre`]
/// stores it.
pub(crate) fn compute_pre(
    db: &DbIndex,
    file_id: FileId,
    scopes: &super::super::ScriptedScopes,
) -> Option<GmodFileMetadataResult> {
    if !db.get_emmyrc().gmod.enabled {
        return None;
    }
    let roles = call_role_entries(db);
    let prefixes = formatted_hook_prefixes(db);
    let mut metadata = collect_file_gmod_metadata(db, file_id, &prefixes, &roles.roles);
    metadata.scope = collect_scoped_file_plan(db, file_id, &scopes.files);
    Some(metadata)
}

/// Stores the metadata of `context`'s one file with its scoped-class
/// bindings, after binding its vgui registrations, and synthesizes its
/// network-var wrappers.
pub(crate) fn apply_pre(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    metadata: Option<GmodFileMetadataResult>,
) {
    let Some(GmodFileMetadataResult {
        hook_metadata,
        member_ranges,
        branch_ranges,
        annotation_realm,
        file_params,
        scope,
        published_bases,
    }) = metadata
    else {
        return;
    };
    let Some(in_filed_tree) = context.tree_list.first().cloned() else {
        return;
    };
    let file_id = in_filed_tree.file_id;
    synthesize_vgui_registrations(db, context, &[file_id], VguiSynthesisPhase::Bind);
    bind_published_scope_bases(db, file_id, &published_bases);

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
    let wrappers = match scope {
        Some(ScopedFilePlan {
            scope_match,
            base_assignments,
            wrappers,
        }) => {
            if let Some(scope_match) = scope_match {
                let range = in_filed_tree.value.syntax().text_range();
                let class_decl_id = ensure_scoped_class_type_decl(
                    db,
                    file_id,
                    &scope_match.class_name,
                    &scope_match.global_name,
                    &scope_match.super_types,
                    range,
                );
                for super_type in scoped_class_super_types(
                    &scope_match.global_name,
                    &scope_match.class_name,
                    &scope_match.super_types,
                ) {
                    if let LuaType::Ref(base_id) = super_type {
                        add_data_table_super_type(db, file_id, &class_decl_id, &base_id, range);
                    }
                }
                collect_scripted_scope_type_bindings_with(db, file_id, &scope_match);
                synthesize_scoped_base_assignments_with(
                    db,
                    file_id,
                    in_filed_tree.value.syntax().text_range(),
                    &scope_match,
                    &base_assignments,
                );
            }
            wrappers
        }
        None => Vec::new(),
    };
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

    synthesize_network_var_wrappers(db, file_id, &in_filed_tree.value, &wrappers);
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

/// What one file's own vgui parent calls resolve to.
pub(crate) fn evaluate_vgui_relations(db: &mut DbIndex, context: &mut AnalyzeContext) {
    if !db.get_emmyrc().gmod.enabled {
        return;
    }
    let Some(file_id) = context.tree_list.first().map(|tree| tree.file_id) else {
        return;
    };
    if let Some(relations) = resolve_vgui_file_relations(db, context, file_id) {
        db.get_gmod_class_metadata_index_mut()
            .set_vgui_file_relations(file_id, relations);
    }
}

/// The workspace vgui parent chains, from every file's parent calls.
pub(crate) fn evaluate_vgui_chains(db: &mut DbIndex, context: &mut AnalyzeContext) {
    if !db.get_emmyrc().gmod.enabled {
        return;
    }
    resolve_vgui_parent_relations(db, context);
}
