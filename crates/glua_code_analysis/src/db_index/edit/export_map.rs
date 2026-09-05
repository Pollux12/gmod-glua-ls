use std::hash::{Hash, Hasher};

use rowan::TextRange;
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use smol_str::SmolStr;

use super::position_map::{FileRemap, Remap};
use crate::{
    DbIndex, FileId, GenericParam, InFiled, LuaDeclId, LuaInferenceNodeId, LuaInferredGuardOwner,
    LuaMemberId, LuaMemberKey, LuaMemberOwner, LuaOperatorOwner, LuaSemanticDeclId, LuaSignatureId,
    LuaType, LuaTypeDeclId, LuaTypeOwner, VariadicType,
};

/// Which half of a network message a flow describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetSide {
    Send,
    Receive,
}

/// The symbol a realm is recorded for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RealmSubject {
    Decl(LuaDeclId),
    Member(LuaMemberId),
}

/// One cross-file-visible fact a file contributes.
///
/// Every variant is either an identity another file can name or a whole-file
/// section. Nothing local-only gets a key: a `local` declaration, its inferred
/// type and its flow facts are invisible outside the file, so an edit that
/// touches only those produces no key and dirties nothing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ExportKey {
    Member(LuaMemberId),
    TypeDecl(LuaTypeDeclId),
    TypeCache(LuaTypeOwner),
    Signature(LuaSignatureId),
    ContributedParam(LuaSignatureId, u16),
    InferredGuard(LuaInferredGuardOwner),
    Property(LuaSemanticDeclId),
    Operator(LuaOperatorOwner, SmolStr),
    NetFlow(SmolStr, NetSide),
    Metatable(InFiled<TextRange>),
    Realm(RealmSubject),
    FileRealmMetadata,
    ModuleExport,
    LoadEdges,
    Namespace,
}

pub type ExportMap = FxHashMap<ExportKey, u64>;

/// What changed between two export maps of the same file.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ChangedExports {
    pub changed: Vec<ExportKey>,
    pub added: Vec<ExportKey>,
    pub removed: Vec<ExportKey>,
}

impl ChangedExports {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }

    /// Every key in the diff, whatever way it changed.
    pub fn keys(&self) -> impl Iterator<Item = &ExportKey> {
        self.changed.iter().chain(&self.added).chain(&self.removed)
    }
}

/// `old` must already be expressed in `new`'s coordinates - build it with a
/// [`FileRemap`] carrying the edit's `PositionMap`, and `new` with
/// [`FileRemap::identity`].
pub fn diff_exports(old: &ExportMap, new: &ExportMap) -> ChangedExports {
    let mut diff = ChangedExports::default();
    for (key, hash) in old {
        match new.get(key) {
            Some(new_hash) if new_hash == hash => {}
            Some(_) => diff.changed.push(key.clone()),
            None => diff.removed.push(key.clone()),
        }
    }
    for key in new.keys() {
        if !old.contains_key(key) {
            diff.added.push(key.clone());
        }
    }
    diff
}

/// The cross-file-visible exports of one file, keyed so a diff can name
/// exactly which of them moved.
///
/// Reads no syntax tree: every key and every hashed value comes from the
/// index, and position-carrying identities are rewritten through `remap`
/// rather than normalised against the current tree. A `Lost` identity yields
/// no key, so it reads as a removed export and dirties its dependents.
pub fn export_map(db: &DbIndex, file_id: FileId, remap: &FileRemap) -> ExportMap {
    let mut map = ExportMap::default();
    let files = FxHashSet::from_iter([file_id]);

    members(db, file_id, remap, &mut map);
    type_decls(db, file_id, remap, &mut map);
    type_caches(db, file_id, remap, &mut map);
    signatures(db, file_id, remap, &mut map);
    contributed_params(db, &files, remap, &mut map);
    inferred_guards(db, &files, remap, &mut map);
    properties(db, file_id, remap, &mut map);
    operators(db, file_id, remap, &mut map);
    net_flows(db, file_id, &mut map);
    metatables(db, file_id, remap, &mut map);
    realms(db, file_id, remap, &mut map);
    module_export(db, file_id, remap, &mut map);
    load_edges(db, file_id, &mut map);
    namespaces(db, file_id, &mut map);

    map
}

fn hash_with(build: impl FnOnce(&mut FxHasher)) -> u64 {
    let mut hasher = FxHasher::default();
    build(&mut hasher);
    hasher.finish()
}

// --- 1. Members declared in this file ---
fn members(db: &DbIndex, file_id: FileId, remap: &FileRemap, map: &mut ExportMap) {
    let member_index = db.get_member_index();
    for member in member_index.get_file_members(file_id) {
        let member_id = remapped(remap.member_id(member.get_id()), member.get_id());
        let owner = member_index.get_member_owner(&member.get_id());
        map.insert(
            ExportKey::Member(member_id),
            hash_with(|h| {
                hash_member_key(remap, member.get_key(), h);
                match owner {
                    Some(owner) => hash_member_owner(remap, owner, h),
                    None => "NoOwner".hash(h),
                }
                member.get_feature().hash(h);
                member_index
                    .is_non_overwriting_assignment_member(member.get_id())
                    .hash(h);
            }),
        );
    }
}

// --- 3. Type decls defined in this file ---
fn type_decls(db: &DbIndex, file_id: FileId, remap: &FileRemap, map: &mut ExportMap) {
    let type_index = db.get_type_index();
    let Some(decl_ids) = type_index.get_file_type_decl_ids(file_id) else {
        return;
    };
    for decl_id in decl_ids {
        map.insert(
            ExportKey::TypeDecl(decl_id.clone()),
            hash_with(|h| {
                if let Some(type_decl) = type_index.get_type_decl(decl_id) {
                    match type_decl.get_alias_ref() {
                        Some(alias_ref) => hash_type(remap, alias_ref, h),
                        None => "NoAlias".hash(h),
                    }
                    let (kind, flags) = type_decl.kind_and_flags();
                    format!("{kind:?}").hash(h);
                    flags.hash(h);
                    let (extra_type, flat) = type_decl.extra_type();
                    flat.hash(h);
                    match extra_type {
                        Some(extra_type) => hash_type(remap, extra_type, h),
                        None => "NoExtra".hash(h),
                    }
                }
                if let Some(supers) = type_index.get_super_type_entries(decl_id) {
                    for sup in supers.iter().filter(|s| s.file_id == file_id) {
                        hash_type(remap, &sup.value.typ, h);
                    }
                }
                if let Some(params) = type_index.get_generic_params(decl_id) {
                    for param in params {
                        hash_generic_param(remap, param, h);
                    }
                }
            }),
        );
    }
}

// --- 4. Exported type caches ---
fn type_caches(db: &DbIndex, file_id: FileId, remap: &FileRemap, map: &mut ExportMap) {
    let type_index = db.get_type_index();
    let Some(owners) = type_index.file_type_owners(file_id) else {
        return;
    };
    for owner in owners {
        match owner {
            // A local's cached type is not observable from another file, and
            // the cached type of a bare expression is local memoisation.
            LuaTypeOwner::Decl(decl_id) => {
                if db
                    .get_decl_index()
                    .get_decl(decl_id)
                    .is_none_or(|decl| decl.is_local())
                {
                    continue;
                }
            }
            LuaTypeOwner::Member(_) => {}
            LuaTypeOwner::SyntaxId(_) => continue,
        }
        let Some(cache) = type_index.get_type_cache(owner) else {
            continue;
        };
        let key_owner = remapped(remap.type_owner(owner), owner.clone());
        map.insert(
            ExportKey::TypeCache(key_owner),
            hash_with(|h| hash_type(remap, cache.as_type(), h)),
        );
    }
}

// --- 5. Signatures defined in this file ---
fn signatures(db: &DbIndex, file_id: FileId, remap: &FileRemap, map: &mut ExportMap) {
    let signature_index = db.get_signature_index();
    let Some(signature_ids) = signature_index.get_file_signature_ids(file_id) else {
        return;
    };
    for signature_id in signature_ids {
        let key_id = remapped(remap.signature_id(*signature_id), *signature_id);
        map.insert(
            ExportKey::Signature(key_id),
            hash_with(|h| {
                if let Some(sig) = signature_index.get(signature_id) {
                    sig.is_vararg.hash(h);
                    sig.is_colon_define.hash(h);
                    sig.async_state.hash(h);
                    sig.resolve_return.hash(h);
                    format!("{:?}", sig.nodiscard).hash(h);
                    sig.params.hash(h);
                    for param in &sig.generic_params {
                        param.name.hash(h);
                        match &param.constraint {
                            Some(constraint) => hash_type(remap, constraint, h),
                            None => "NoConstraint".hash(h),
                        }
                    }
                    let mut param_indices: Vec<&usize> = sig.param_docs.keys().collect();
                    param_indices.sort_unstable();
                    for idx in param_indices {
                        idx.hash(h);
                        let doc = &sig.param_docs[idx];
                        doc.name.hash(h);
                        doc.nullable.hash(h);
                        doc.description.hash(h);
                        format!("{:?}", doc.default_value).hash(h);
                        format!("{:?}", doc.attributes).hash(h);
                        hash_type(remap, &doc.type_ref, h);
                    }
                    for ret in &sig.return_docs {
                        ret.name.hash(h);
                        ret.description.hash(h);
                        format!("{:?}", ret.default_value).hash(h);
                        format!("{:?}", ret.attributes).hash(h);
                        format!("{:?}", ret.return_kind).hash(h);
                        hash_type(remap, &ret.type_ref, h);
                    }
                    for overload in &sig.overloads {
                        hash_type(remap, &LuaType::DocFunction(overload.clone()), h);
                    }
                    format!("{:?}", sig.require_guard_param()).hash(h);
                    sig.nil_return_guard_params().hash(h);
                    format!("{:?}", sig.return_correlations()).hash(h);
                    format!("{:?}", sig.direct_param_return_alias()).hash(h);
                    format!("{:?}", sig.class_name_param_return_alias()).hash(h);
                    format!("{:?}", sig.falsy_param_nil_free_return_slots()).hash(h);
                    format!("{:?}", sig.falsy_param_return_aliases()).hash(h);
                    for out_param in &sig.out_params {
                        format!("{:?}", out_param.root).hash(h);
                        out_param.field_path.hash(h);
                        hash_type(remap, &out_param.type_ref, h);
                    }
                }
                match signature_index.inferred_positive_guard(signature_id) {
                    Some(guard) => {
                        guard.param_idx.hash(h);
                        hash_type(remap, &guard.narrowed_type, h);
                    }
                    None => "NoInferredGuard".hash(h),
                }
            }),
        );
    }
}

// --- 6. Parameter types this file's call sites are evidence for ---
fn contributed_params(
    db: &DbIndex,
    files: &FxHashSet<FileId>,
    remap: &FileRemap,
    map: &mut ExportMap,
) {
    for ((signature_id, param_idx), typ) in db
        .get_call_site_param_index()
        .inferred_params_for_contributor_files(files)
    {
        let key_id = remapped(remap.signature_id(signature_id), signature_id);
        map.insert(
            ExportKey::ContributedParam(key_id, param_idx as u16),
            hash_with(|h| hash_type(remap, &typ, h)),
        );
    }
}

// --- 7. Inferred guard facts produced by this file ---
fn inferred_guards(
    db: &DbIndex,
    files: &FxHashSet<FileId>,
    remap: &FileRemap,
    map: &mut ExportMap,
) {
    for (owner, guard) in db
        .get_signature_index()
        .inferred_guard_facts_for_files(files)
    {
        let key_owner = remapped(remap.inferred_guard_owner(&owner), owner.clone());
        map.insert(
            ExportKey::InferredGuard(key_owner),
            hash_with(|h| {
                guard.param_idx.hash(h);
                hash_type(remap, &guard.narrowed_type, h);
            }),
        );
    }
}

// --- 8. Annotations on this file's symbols that other files act on ---
fn properties(db: &DbIndex, file_id: FileId, remap: &FileRemap, map: &mut ExportMap) {
    let default = crate::LuaCommonProperty::new();
    let default_acted_on = acted_on(&default);
    for (owner, property) in db.get_property_index().properties_in_file(file_id) {
        // A local declaration is not resolvable from another file, so an
        // annotation on one changes nothing outside it.
        if let LuaSemanticDeclId::LuaDecl(decl_id) = owner
            && db
                .get_decl_index()
                .get_decl(decl_id)
                .is_none_or(|decl| decl.is_local())
        {
            continue;
        }
        // Writing a doc comment creates a property whose acted-on fields are
        // all still default. Registering it would make documenting a symbol an
        // export change.
        let acted_on = acted_on(property);
        if acted_on == default_acted_on {
            continue;
        }
        let key_owner = remapped(remap.semantic_decl_id(owner), owner.clone());
        map.insert(
            ExportKey::Property(key_owner),
            hash_with(|h| acted_on.hash(h)),
        );
    }
}

/// The property fields another file's diagnostics read. The free-text
/// description and source are excluded: a hover reads them from the index when
/// the request arrives, so no dependent holds a copy that can go stale.
fn acted_on(property: &crate::LuaCommonProperty) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        property.visibility,
        property.deprecated,
        property.export,
        property.decl_features,
        property.version_conds,
        property.attribute_uses,
        property.default_value,
        property.tag_content,
    )
}

// --- 9. Metamethods this file declares ---
fn operators(db: &DbIndex, file_id: FileId, remap: &FileRemap, map: &mut ExportMap) {
    for operator in db.get_operator_index().operators_in_file(file_id) {
        let owner = operator.get_owner();
        let key_owner = remapped(remap.operator_owner(owner), owner.clone());
        map.insert(
            ExportKey::Operator(key_owner, format!("{:?}", operator.get_op()).into()),
            hash_with(|h| hash_type(remap, &operator.get_operator_func(db), h)),
        );
    }
}

// --- 10. Network flows this file declares ---
fn net_flows(db: &DbIndex, file_id: FileId, map: &mut ExportMap) {
    fn hash_ops(ops: &[crate::NetOpEntry], hasher: &mut impl Hasher) {
        for entry in ops {
            format!("{:?}", entry.op).hash(hasher);
            entry.display_name.hash(hasher);
            entry.dynamic.hash(hasher);
            format!("{:?}", entry.bits).hash(hasher);
        }
    }
    let Some(network) = db.get_gmod_network_index().get_file_data(file_id) else {
        return;
    };
    // One file can declare several flows for the same message; their hashes
    // are folded in sorted order so the key stays the message name, which is
    // what the peer-file lookup is keyed by.
    let mut per_message: FxHashMap<(SmolStr, NetSide), Vec<u64>> = FxHashMap::default();
    for flow in &network.send_flows {
        per_message
            .entry((flow.message_name.as_str().into(), NetSide::Send))
            .or_default()
            .push(hash_with(|h| {
                format!("{:?}", flow.send_kind).hash(h);
                flow.send_display_name.hash(h);
                flow.send_target.hash(h);
                flow.is_wrapped.hash(h);
                hash_ops(&flow.writes, h);
            }));
    }
    for flow in &network.receive_flows {
        per_message
            .entry((flow.message_name.as_str().into(), NetSide::Receive))
            .or_default()
            .push(hash_with(|h| {
                flow.reads_opaque.hash(h);
                hash_ops(&flow.reads, h);
            }));
    }
    for ((name, side), mut hashes) in per_message {
        hashes.sort_unstable();
        map.insert(
            ExportKey::NetFlow(name, side),
            hash_with(|h| hashes.hash(h)),
        );
    }
}

// --- 11. Metatable bindings this file declares ---
fn metatables(db: &DbIndex, file_id: FileId, remap: &FileRemap, map: &mut ExportMap) {
    for (table, binding) in db.get_metatable_index().iter_bindings() {
        if binding.writer_file_id != file_id {
            continue;
        }
        let key_range = remapped(remap.table_range(table), table.clone());
        let value_range = remapped(
            remap.table_range(&binding.metatable),
            binding.metatable.clone(),
        );
        map.insert(
            ExportKey::Metatable(key_range),
            hash_with(|h| {
                value_range.file_id.hash(h);
                u32::from(value_range.value.start()).hash(h);
                u32::from(value_range.value.end()).hash(h);
            }),
        );
    }
}

// --- 12. Realm per exported symbol, and the file's realm metadata ---
fn realms(db: &DbIndex, file_id: FileId, remap: &FileRemap, map: &mut ExportMap) {
    let gmod_infer = db.get_gmod_infer_index();
    if let Some(decl_tree) = db.get_decl_index().get_decl_tree(&file_id) {
        for (decl_id, decl) in decl_tree.get_decls() {
            if decl.is_local() {
                continue;
            }
            let key_id = remapped(remap.decl_id(*decl_id), *decl_id);
            let realm = gmod_infer.get_realm_at_offset(&file_id, decl_id.position);
            map.insert(
                ExportKey::Realm(RealmSubject::Decl(key_id)),
                hash_with(|h| format!("{realm:?}").hash(h)),
            );
        }
    }
    for member in db.get_member_index().get_file_members(file_id) {
        let member_id = member.get_id();
        let key_id = remapped(remap.member_id(member_id), member_id);
        let realm = gmod_infer.get_realm_at_offset(&file_id, member_id.get_position());
        map.insert(
            ExportKey::Realm(RealmSubject::Member(key_id)),
            hash_with(|h| format!("{realm:?}").hash(h)),
        );
    }

    if let Some(metadata) = gmod_infer.get_realm_file_metadata(&file_id) {
        map.insert(
            ExportKey::FileRealmMetadata,
            hash_with(|h| {
                // Field by field: `branch_realm_ranges` carries the source
                // ranges of the `if CLIENT`/`if SERVER` blocks, and which
                // realms the file narrows to is the observable part, not where
                // the blocks sit.
                format!(
                    "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
                    metadata.inferred_realm,
                    metadata.load_realm,
                    metadata.load_status,
                    metadata.load_state_mask,
                    metadata.filename_hint,
                    metadata.dependency_hints,
                    metadata.annotation_realm,
                )
                .hash(h);
                let branch_realms: Vec<String> = metadata
                    .branch_realm_ranges
                    .iter()
                    .map(|range| format!("{:?}", range.realm))
                    .collect();
                branch_realms.hash(h);
            }),
        );
    }
}

// --- 13. What this file exports as a module ---
fn module_export(db: &DbIndex, file_id: FileId, remap: &FileRemap, map: &mut ExportMap) {
    let Some(module) = db.get_module_index().get_module(file_id) else {
        return;
    };
    map.insert(
        ExportKey::ModuleExport,
        hash_with(|h| {
            module.full_module_name.hash(h);
            module.visible.hash(h);
            module.is_meta.hash(h);
            format!("{:?}", module.workspace_id).hash(h);
            format!("{:?}", module.version_conds).hash(h);
            match &module.export_type {
                Some(export_type) => hash_type(remap, export_type, h),
                None => "NoExport".hash(h),
            }
            match &module.semantic_id {
                Some(id) => match remap.semantic_decl_id(id) {
                    Remap::Moved(mapped) => format!("{mapped:?}").hash(h),
                    Remap::Unrelated => format!("{id:?}").hash(h),
                    Remap::Lost => "LOST".hash(h),
                },
                None => "NoSemanticId".hash(h),
            }
        }),
    );
}

// --- 14. Load edges this file declares ---
fn load_edges(db: &DbIndex, file_id: FileId, map: &mut ExportMap) {
    let dependency_index = db.get_file_dependencies_index();
    let mut sites: Vec<String> = dependency_index
        .get_dependency_sites(&file_id)
        .unwrap_or_default()
        .iter()
        // The call's range is left out: it moves on any edit above it, and the
        // edge is identified by its target and kind.
        .map(|site| {
            format!(
                "{:?}|{:?}|{:?}|{}",
                site.kind, site.target_file_id, site.path, site.original_expr
            )
        })
        .collect();
    sites.sort_unstable();
    let mut required: Vec<u32> = dependency_index
        .get_required_files(&file_id)
        .map(|files| files.iter().map(|file| file.id).collect())
        .unwrap_or_default();
    required.sort_unstable();
    if sites.is_empty() && required.is_empty() {
        return;
    }
    map.insert(
        ExportKey::LoadEdges,
        hash_with(|h| {
            sites.hash(h);
            required.hash(h);
        }),
    );
}

// --- 15. Namespace / using ---
fn namespaces(db: &DbIndex, file_id: FileId, map: &mut ExportMap) {
    let type_index = db.get_type_index();
    let namespace = type_index.get_file_namespace(&file_id);
    let using = type_index.get_file_using_namespace(&file_id);
    if namespace.is_none() && using.is_none() {
        return;
    }
    map.insert(
        ExportKey::Namespace,
        hash_with(|h| {
            namespace.hash(h);
            using.hash(h);
        }),
    );
}

/// The identity to key an entry by: its new form when it moved, and its old
/// form otherwise.
///
/// A lost identity deliberately keeps its old form. The new map is built in
/// the new text's coordinates and cannot contain it, so the key lands in
/// `removed` - and the old form is the one every dependent recorded, so it is
/// also the one the reverse indexes answer to. Dropping the key instead would
/// leave a deleted export with no key on either side and nothing to dirty its
/// readers with.
fn remapped<T>(mapped: Remap<T>, current: T) -> T {
    match mapped {
        Remap::Moved(value) => value,
        Remap::Unrelated | Remap::Lost => current,
    }
}

/// Hashes an identity the edit may have moved.
///
/// A `Lost` identity folds in a sentinel, which forces the enclosing key to
/// compare as changed - the value genuinely names something that is gone.
fn hash_remapped_identity<T: std::fmt::Debug>(
    tag: &str,
    mapped: Remap<T>,
    current: &T,
    hasher: &mut impl Hasher,
) {
    tag.hash(hasher);
    match mapped {
        Remap::Moved(value) => format!("{value:?}").hash(hasher),
        Remap::Unrelated => format!("{current:?}").hash(hasher),
        Remap::Lost => "LOST".hash(hasher),
    }
}

fn hash_member_owner(remap: &FileRemap, owner: &LuaMemberOwner, hasher: &mut impl Hasher) {
    match owner {
        LuaMemberOwner::GlobalPath(gid) => {
            "GlobalPath".hash(hasher);
            gid.get_name().hash(hasher);
        }
        LuaMemberOwner::Type(tid) => {
            "Type".hash(hasher);
            tid.get_name().hash(hasher);
        }
        LuaMemberOwner::Element(range) => {
            hash_remapped_identity("Element", remap.table_range(range), range, hasher);
        }
        LuaMemberOwner::LocalUnresolve => {
            "LocalUnresolve".hash(hasher);
        }
    }
}

fn hash_member_key(remap: &FileRemap, key: &LuaMemberKey, hasher: &mut impl Hasher) {
    match key {
        LuaMemberKey::Name(name) => {
            "Name".hash(hasher);
            name.hash(hasher);
        }
        LuaMemberKey::Integer(i) => {
            "Integer".hash(hasher);
            i.hash(hasher);
        }
        LuaMemberKey::None => {
            "None".hash(hasher);
        }
        LuaMemberKey::ExprType(typ) => {
            "ExprType".hash(hasher);
            hash_type(remap, typ, hasher);
        }
    }
}

fn hash_generic_param(remap: &FileRemap, param: &GenericParam, hasher: &mut impl Hasher) {
    param.name.hash(hasher);
    format!("{:?}", param.attributes).hash(hasher);
    match &param.type_constraint {
        Some(constraint) => hash_type(remap, constraint, hasher),
        None => "NoConstraint".hash(hasher),
    }
}

/// Hashes everything about a type that another file can observe.
///
/// The three position-carrying identities - a table literal's range, an
/// instance's range and a signature id - go through `remap`, so a value that
/// merely shifted hashes the same before and after the edit while a value
/// repointed at a different literal or function does not.
pub(crate) fn hash_type(remap: &FileRemap, typ: &LuaType, hasher: &mut impl Hasher) {
    // Arm order is not guaranteed for the set-like composites, so their arm
    // hashes are sorted before they are folded in.
    fn hash_unordered(remap: &FileRemap, tag: &str, arms: &[LuaType], hasher: &mut impl Hasher) {
        tag.hash(hasher);
        let mut arm_hashes: Vec<u64> = arms
            .iter()
            .map(|arm| hash_with(|h| hash_type(remap, arm, h)))
            .collect();
        arm_hashes.sort_unstable();
        arm_hashes.hash(hasher);
    }

    match typ {
        LuaType::StringConst(s) | LuaType::DocStringConst(s) => {
            "StringConst".hash(hasher);
            s.as_str().hash(hasher);
        }
        LuaType::IntegerConst(i) | LuaType::DocIntegerConst(i) => {
            "IntegerConst".hash(hasher);
            i.hash(hasher);
        }
        LuaType::FloatConst(f) => {
            "FloatConst".hash(hasher);
            f.to_bits().hash(hasher);
        }
        LuaType::BooleanConst(b) | LuaType::DocBooleanConst(b) => {
            "BooleanConst".hash(hasher);
            b.hash(hasher);
        }
        LuaType::TableConst(range) => {
            hash_remapped_identity("TableConst", remap.table_range(range), range, hasher);
        }
        LuaType::Instance(inst) => {
            let range = inst.get_range();
            hash_remapped_identity("Instance", remap.table_range(range), range, hasher);
            hash_type(remap, inst.get_base(), hasher);
        }
        LuaType::Signature(id) => {
            hash_remapped_identity("Signature", remap.signature_id(*id), id, hasher);
        }
        LuaType::Ref(id) => {
            "Ref".hash(hasher);
            id.get_name().hash(hasher);
        }
        LuaType::Def(id) => {
            "Def".hash(hasher);
            id.get_name().hash(hasher);
        }
        LuaType::Union(union) => hash_unordered(remap, "Union", &union.into_vec(), hasher),
        LuaType::Intersection(inter) => {
            hash_unordered(remap, "Intersection", inter.get_types(), hasher)
        }
        LuaType::MergedTable(merged) => {
            hash_unordered(remap, "MergedTable", merged.get_types(), hasher)
        }
        LuaType::Tuple(tuple) => {
            "Tuple".hash(hasher);
            tuple.status.hash(hasher);
            for sub in tuple.get_types() {
                hash_type(remap, sub, hasher);
            }
        }
        LuaType::Array(arr) => {
            "Array".hash(hasher);
            format!("{:?}", arr.get_len()).hash(hasher);
            hash_type(remap, arr.get_base(), hasher);
        }
        LuaType::Object(obj) => {
            "Object".hash(hasher);
            for (key, value) in obj.get_fields() {
                hash_member_key(remap, key, hasher);
                hash_type(remap, value, hasher);
            }
            for (key, value) in obj.get_index_access() {
                hash_type(remap, key, hasher);
                hash_type(remap, value, hasher);
            }
        }
        LuaType::TableGeneric(params) => {
            "TableGeneric".hash(hasher);
            for param in params.iter() {
                hash_type(remap, param, hasher);
            }
        }
        LuaType::TableOf(inner) => {
            "TableOf".hash(hasher);
            hash_type(remap, inner, hasher);
        }
        LuaType::TypeGuard(inner) => {
            "TypeGuard".hash(hasher);
            hash_type(remap, inner, hasher);
        }
        LuaType::Generic(generic) => {
            "Generic".hash(hasher);
            generic.get_base_type_id().get_name().hash(hasher);
            for param in generic.get_params() {
                hash_type(remap, param, hasher);
            }
        }
        LuaType::DocFunction(func) => {
            "DocFunction".hash(hasher);
            func.is_colon_define().hash(hasher);
            func.get_async_state().hash(hasher);
            func.is_variadic().hash(hasher);
            func.get_optional_params().hash(hasher);
            for (name, param_type) in func.get_params() {
                name.hash(hasher);
                match param_type {
                    Some(param_type) => hash_type(remap, param_type, hasher),
                    None => "NoParamType".hash(hasher),
                }
            }
            hash_type(remap, func.get_ret(), hasher);
        }
        LuaType::ModuleRef(file_id) => {
            "ModuleRef".hash(hasher);
            file_id.hash(hasher);
        }
        LuaType::Variadic(variadic) => {
            "Variadic".hash(hasher);
            match variadic.as_ref() {
                VariadicType::Base(base) => {
                    "Base".hash(hasher);
                    hash_type(remap, base, hasher);
                }
                VariadicType::Multi(types) => {
                    "Multi".hash(hasher);
                    for sub in types {
                        hash_type(remap, sub, hasher);
                    }
                }
            }
        }
        LuaType::Call(call) => {
            "Call".hash(hasher);
            format!("{:?}", call.get_call_kind()).hash(hasher);
            for operand in call.get_operands() {
                hash_type(remap, operand, hasher);
            }
        }
        LuaType::MultiLineUnion(union) => {
            "MultiLineUnion".hash(hasher);
            for (arm, description) in union.get_unions() {
                description.hash(hasher);
                hash_type(remap, arm, hasher);
            }
        }
        LuaType::Conditional(cond) => {
            "Conditional".hash(hasher);
            cond.has_new.hash(hasher);
            for param in cond.get_infer_params() {
                hash_generic_param(remap, param, hasher);
            }
            hash_type(remap, cond.get_condition(), hasher);
            hash_type(remap, cond.get_true_type(), hasher);
            hash_type(remap, cond.get_false_type(), hasher);
        }
        LuaType::Mapped(mapped) => {
            "Mapped".hash(hasher);
            format!("{:?}", mapped.param.0).hash(hasher);
            hash_generic_param(remap, &mapped.param.1, hasher);
            mapped.is_readonly.hash(hasher);
            mapped.is_optional.hash(hasher);
            hash_type(remap, &mapped.value, hasher);
        }
        LuaType::DocAttribute(attribute) => {
            "DocAttribute".hash(hasher);
            for (name, param_type) in attribute.get_params() {
                name.hash(hasher);
                match param_type {
                    Some(param_type) => hash_type(remap, param_type, hasher),
                    None => "NoParamType".hash(hasher),
                }
            }
        }
        LuaType::StrTplRef(tpl) => {
            "StrTplRef".hash(hasher);
            tpl.get_prefix().hash(hasher);
            tpl.get_name().hash(hasher);
            tpl.get_suffix().hash(hasher);
            tpl.get_tpl_id().hash(hasher);
            if let Some(constraint) = tpl.get_constraint() {
                hash_type(remap, constraint, hasher);
            }
        }
        LuaType::TplRef(tpl) => hash_tpl_ref("TplRef", remap, tpl, hasher),
        LuaType::ConstTplRef(tpl) => hash_tpl_ref("ConstTplRef", remap, tpl, hasher),
        // An interned name, with no nested type and no source position: the
        // name is the whole identity.
        LuaType::Namespace(name) => {
            "Namespace".hash(hasher);
            name.hash(hasher);
        }
        LuaType::Language(name) => {
            "Language".hash(hasher);
            name.hash(hasher);
        }
        LuaType::ConditionalInfer(name) => {
            "ConditionalInfer".hash(hasher);
            name.hash(hasher);
        }
        // Payload-free variants: the discriminant is the whole identity. Listed
        // rather than matched with a wildcard so that a new `LuaType` variant
        // fails to compile here instead of being hashed as if it carried
        // nothing — a variant holding a range or a nested type would then hash
        // equal across an edit that moved it, and its dependents would never be
        // re-analysed.
        LuaType::Unknown
        | LuaType::Any
        | LuaType::Nil
        | LuaType::Table
        | LuaType::Userdata
        | LuaType::Function
        | LuaType::Thread
        | LuaType::Boolean
        | LuaType::String
        | LuaType::Integer
        | LuaType::Number
        | LuaType::Io
        | LuaType::SelfInfer
        | LuaType::Global
        | LuaType::Never => std::mem::discriminant(typ).hash(hasher),
    }
}

fn hash_tpl_ref(tag: &str, remap: &FileRemap, tpl: &crate::GenericTpl, hasher: &mut impl Hasher) {
    tag.hash(hasher);
    tpl.get_tpl_id().hash(hasher);
    tpl.get_name().hash(hasher);
    match tpl.get_constraint() {
        Some(constraint) => hash_type(remap, constraint, hasher),
        None => "NoConstraint".hash(hasher),
    }
}

/// The identities a symbol's cached type names, as reverse-index keys.
///
/// A file that read this symbol cached the *value*, so its own cache names
/// those identities: the table literal, the signature, the class. Looking them
/// up finds exactly the readers, without dirtying everyone who merely holds a
/// member on the same owner.
fn push_cached_type_refs(db: &DbIndex, owner: &LuaTypeOwner, out: &mut Vec<crate::TypeCacheRef>) {
    let Some(cache) = db.get_type_index().get_type_cache(owner) else {
        return;
    };
    crate::TypeVisitTrait::visit_type(cache.as_type(), &mut |inner| match inner {
        LuaType::TableConst(range) => out.push(crate::TypeCacheRef::Table(range.clone())),
        LuaType::Instance(instance) => {
            out.push(crate::TypeCacheRef::Instance(instance.get_range().clone()))
        }
        LuaType::Signature(signature_id) => out.push(crate::TypeCacheRef::Signature(*signature_id)),
        LuaType::Ref(type_id) | LuaType::Def(type_id) => {
            out.push(crate::TypeCacheRef::Decl(type_id.clone()))
        }
        _ => {}
    });
}

/// The files that read any of the facts in `diff`.
///
/// Queries are batched per reverse index rather than issued per key: the
/// lookups are set unions, and one call per key would turn a diff of a few
/// hundred keys into a few hundred index walks.
pub fn dependents_of(db: &DbIndex, file_id: FileId, diff: &ChangedExports) -> FxHashSet<FileId> {
    // A `---@module` reader caches `LuaType::ModuleRef(file_id)`, filed in the
    // reverse index under the providing file rather than under any symbol it
    // exports. No export key names that identity, so seeding the file itself is
    // the only thing that reaches those readers. It cannot be narrowed to the
    // keys that moved: a module can hand back the same table identity and still
    // change a member's type, leaving every key the reader named untouched.
    let mut type_refs: Vec<crate::TypeCacheRef> = vec![crate::TypeCacheRef::Module(file_id)];
    let mut nodes: Vec<LuaInferenceNodeId> = Vec::new();
    let mut signatures: Vec<LuaSignatureId> = Vec::new();
    // Each owner, and whether any key naming it appeared or vanished. One
    // entry per owner: a file with a thousand writes to one slot names that
    // owner a thousand times, and the member enumeration below is per owner,
    // so a vector here makes the whole pass quadratic in the slot's writers.
    let mut owners: FxHashMap<LuaMemberOwner, bool> = FxHashMap::default();
    let mut net_messages: Vec<SmolStr> = Vec::new();
    let mut file_level = false;

    // A key that appeared or vanished also reaches files that read through its
    // owner and found nothing: a failed lookup records no dependency edge, and
    // it may now hit. A key whose hash merely changed has no such readers - the
    // ones it has are named by the edges below.
    let structural: FxHashSet<&ExportKey> = diff.added.iter().chain(&diff.removed).collect();
    let mut missed_readers: Vec<FileId> = Vec::new();

    for key in diff.keys() {
        match key {
            ExportKey::Member(member_id) => {
                let owner = LuaTypeOwner::Member(*member_id);
                push_cached_type_refs(db, &owner, &mut type_refs);
                nodes.push(LuaInferenceNodeId::TypeOwner(owner));
                let member_index = db.get_member_index();
                if let Some(owner) = member_index.get_member_owner(member_id) {
                    record_owner_dependency(&mut owners, owner, structural.contains(key));
                    // A reader whose lookup of this key found nothing cached
                    // no type, so no reverse index below names it.
                    if structural.contains(key)
                        && let Some(member) = member_index.get_member(member_id)
                    {
                        missed_readers
                            .extend(member_index.missed_member_readers(owner, member.get_key()));
                    }
                }
            }
            ExportKey::TypeDecl(type_decl_id) => {
                type_refs.push(crate::TypeCacheRef::Decl(type_decl_id.clone()));
            }
            ExportKey::TypeCache(owner) => {
                push_cached_type_refs(db, owner, &mut type_refs);
                nodes.push(LuaInferenceNodeId::TypeOwner(owner.clone()));
                if let LuaTypeOwner::Member(member_id) = owner
                    && let Some(owner) = db.get_member_index().get_member_owner(member_id)
                {
                    record_owner_dependency(&mut owners, owner, structural.contains(key));
                }
            }
            ExportKey::Signature(signature_id) => {
                type_refs.push(crate::TypeCacheRef::Signature(*signature_id));
                signatures.push(*signature_id);
            }
            ExportKey::ContributedParam(signature_id, param_idx) => {
                nodes.push(LuaInferenceNodeId::SignatureParam {
                    signature_id: *signature_id,
                    param_idx: *param_idx,
                });
            }
            ExportKey::InferredGuard(owner) => {
                signatures.push(owner.signature_id());
            }
            ExportKey::Property(semantic_id) => match semantic_id {
                LuaSemanticDeclId::TypeDecl(type_decl_id) => {
                    type_refs.push(crate::TypeCacheRef::Decl(type_decl_id.clone()));
                }
                LuaSemanticDeclId::Member(member_id) => {
                    nodes.push(LuaInferenceNodeId::TypeOwner(LuaTypeOwner::Member(
                        *member_id,
                    )));
                }
                LuaSemanticDeclId::LuaDecl(decl_id) => {
                    nodes.push(LuaInferenceNodeId::TypeOwner(LuaTypeOwner::Decl(*decl_id)));
                }
                LuaSemanticDeclId::Signature(signature_id) => {
                    type_refs.push(crate::TypeCacheRef::Signature(*signature_id));
                    signatures.push(*signature_id);
                }
            },
            ExportKey::Operator(owner, _) => match owner {
                LuaOperatorOwner::Table(range) => {
                    type_refs.push(crate::TypeCacheRef::Table(range.clone()));
                    type_refs.push(crate::TypeCacheRef::Instance(range.clone()));
                }
                LuaOperatorOwner::Type(type_decl_id) => {
                    type_refs.push(crate::TypeCacheRef::Decl(type_decl_id.clone()));
                }
            },
            ExportKey::NetFlow(name, _) => net_messages.push(name.clone()),
            ExportKey::Metatable(range) => {
                type_refs.push(crate::TypeCacheRef::Table(range.clone()));
                type_refs.push(crate::TypeCacheRef::Instance(range.clone()));
                record_owner_dependency(&mut owners, &LuaMemberOwner::Element(range.clone()), true);
            }
            ExportKey::Realm(RealmSubject::Decl(decl_id)) => {
                nodes.push(LuaInferenceNodeId::TypeOwner(LuaTypeOwner::Decl(*decl_id)));
            }
            ExportKey::Realm(RealmSubject::Member(member_id)) => {
                nodes.push(LuaInferenceNodeId::TypeOwner(LuaTypeOwner::Member(
                    *member_id,
                )));
            }
            ExportKey::FileRealmMetadata
            | ExportKey::ModuleExport
            | ExportKey::LoadEdges
            | ExportKey::Namespace => file_level = true,
        }
    }

    let member_index = db.get_member_index();
    let mut dependents: FxHashSet<FileId> = missed_readers.into_iter().collect();
    for (owner, structural) in &owners {
        // A reader that resolved a member through this owner cached the owner
        // itself - the class it named, the literal it indexed - so the reverse
        // index finds it whether the member appeared or merely changed.
        match owner {
            LuaMemberOwner::Element(range) => {
                type_refs.push(crate::TypeCacheRef::Table(range.clone()));
                type_refs.push(crate::TypeCacheRef::Instance(range.clone()));
            }
            LuaMemberOwner::Type(type_decl_id) => {
                type_refs.push(crate::TypeCacheRef::Decl(type_decl_id.clone()));
            }
            // No reverse index names a global path. Every file that already
            // resolved some member of the path is the population that can see
            // a sibling appear or vanish; a file whose only read of the path
            // failed is not reachable, which is the same gap as today.
            LuaMemberOwner::GlobalPath(_) => {}
            LuaMemberOwner::LocalUnresolve => continue,
        }
        // Walking every member of the owner is only for a key that appeared or
        // vanished: a sibling read that found nothing recorded no edge at all.
        // A member whose value merely changed is reached through the ref above,
        // and enumerating a hub owner's members here dirties hundreds of files
        // that read none of it.
        if !structural {
            continue;
        }
        for member in member_index.get_members(owner).unwrap_or_default() {
            let member_id = member.get_id();
            dependents.insert(member_id.file_id);
            nodes.push(LuaInferenceNodeId::TypeOwner(LuaTypeOwner::Member(
                member_id,
            )));
        }
    }

    let type_index = db.get_type_index();
    dependents.extend(type_index.files_with_type_caches_referencing(&type_refs));
    dependents.extend(type_index.files_depending_on_inference_nodes(&nodes));

    let call_site_index = db.get_call_site_param_index();
    dependents.extend(call_site_index.collect_signature_contributor_files(&signatures));
    dependents.extend(
        db.get_signature_index()
            .settled_read_dependents_for_signatures(&signatures),
    );
    let sources: Vec<crate::CallSiteSourceId> = signatures
        .iter()
        .map(|id| crate::CallSiteSourceId::Signature(*id))
        .collect();
    dependents.extend(call_site_index.collect_source_node_dependents(&sources));
    // A parameter's declared type is read by the callee itself.
    dependents.extend(signatures.iter().map(|id| id.get_file_id()));

    let network_index = db.get_gmod_network_index();
    for name in &net_messages {
        dependents.extend(
            network_index
                .get_send_flows_for_message(name)
                .into_iter()
                .map(|(file_id, _)| file_id),
        );
        dependents.extend(
            network_index
                .get_receive_flows_for_message(name)
                .into_iter()
                .map(|(file_id, _)| file_id),
        );
    }

    if file_level {
        dependents.extend(
            db.get_file_dependencies_index()
                .get_file_dependencies()
                .collect_file_dependents(vec![file_id]),
        );
    }

    dependents.remove(&file_id);
    dependents
}

/// Files `owner` as the home of a changed export, OR-ing whether any key
/// naming it appeared or vanished: one enumeration in `dependents_of` covers
/// every key of the owner either way.
fn record_owner_dependency(
    owners: &mut FxHashMap<LuaMemberOwner, bool>,
    owner: &LuaMemberOwner,
    structural: bool,
) {
    owners
        .entry(owner.clone())
        .and_modify(|known| *known |= structural)
        .or_insert(structural);
}
