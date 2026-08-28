#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::panic,
        clippy::panic_in_result_fn
    )
)]

mod ast_util;
mod compilation;
mod config;
mod db_index;
mod diagnostic;
mod gamemode_base;
mod library_collision;
pub mod profile;
pub mod progress;
mod resources;
mod semantic;
mod test_lib;
mod vfs;

pub use compilation::*;
pub use config::*;
pub use db_index::*;
pub use diagnostic::*;
pub use gamemode_base::detect_gamemode_base_libraries;
pub use glua_codestyle::*;
use glua_parser::{
    LineIndex, LuaAssignStat, LuaAstNode, LuaAstToken, LuaCallExpr, LuaExpr, LuaIndexKey,
    LuaLocalStat, LuaNameExpr, LuaParenExpr, LuaParser, LuaSyntaxTree, LuaTableExpr, LuaTableField,
};
pub use library_collision::LibraryDefinitionCollision;
use lsp_types::Uri;
pub use profile::Profile;
use resources::load_resource_std;
use schema_to_glua::SchemaConverter;
pub use semantic::*;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Component, Path};
use std::str::FromStr;
use std::{collections::HashSet, path::PathBuf, sync::Arc};
pub use test_lib::{GMOD_CALL_ARG_BUILTINS_FIXTURE, VirtualWorkspace};
use tokio_util::sync::CancellationToken;
use url::Url;
pub use vfs::*;

/// The cross-file facts an edit can invalidate, captured before
/// re-analysis.
#[derive(Clone, Debug, Default)]
pub(crate) struct InferredGuardSnapshot {
    facts: HashMap<LuaInferredGuardOwner, LuaInferredPositiveGuard>,
    consumers: HashMap<LuaInferredGuardOwner, HashSet<FileId>>,
    /// Parameter types inferred from the snapshotted files' call sites, keyed by
    /// the callee signature they belong to.
    inferred_params: HashMap<(LuaSignatureId, usize), LuaType>,
    /// The files the snapshot was taken for, needed to recompute the same set.
    snapshot_file_ids: HashSet<FileId>,
}

#[derive(Default)]
struct InferredGuardReferenceFiles {
    files: HashSet<FileId>,
    alias_calls: HashSet<FileId>,
}

#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct InferredGuardPropagationStats {
    pub changed_facts: usize,
    pub reference_edges: usize,
    pub frontiers: usize,
    pub reindexed_files: usize,
    pub broad_stabilizations: usize,
}

fn sort_inferred_guard_owners(owners: &mut [LuaInferredGuardOwner]) {
    owners.sort_by(|left, right| {
        (left.source_file_id(), left.source_position(), left.path()).cmp(&(
            right.source_file_id(),
            right.source_position(),
            right.path(),
        ))
    });
}

fn hash_member_owner_stable(
    ids: &ExportIdentities,
    owner: &LuaMemberOwner,
    hasher: &mut impl Hasher,
) {
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
            "Element".hash(hasher);
            ids.table_identity(range).hash(hasher);
        }
        LuaMemberOwner::LocalUnresolve => {
            "LocalUnresolve".hash(hasher);
        }
    }
}

fn hash_lua_member_key_export(
    ids: &ExportIdentities,
    key: &LuaMemberKey,
    hasher: &mut impl Hasher,
) {
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
            hash_lua_type_export(ids, typ, hasher);
        }
    }
}

/// Offset-free identities for types.
struct ExportIdentities<'a> {
    db: &'a DbIndex,
    signature_ordinals: std::cell::RefCell<rustc_hash::FxHashMap<FileId, Vec<rowan::TextSize>>>,
    table_ordinals: std::cell::RefCell<rustc_hash::FxHashMap<FileId, Vec<rowan::TextRange>>>,
    table_anchors: std::cell::RefCell<
        rustc_hash::FxHashMap<FileId, rustc_hash::FxHashMap<rowan::TextRange, String>>,
    >,
}

impl<'a> ExportIdentities<'a> {
    fn new(db: &'a DbIndex) -> Self {
        Self {
            db,
            signature_ordinals: std::cell::RefCell::new(rustc_hash::FxHashMap::default()),
            table_ordinals: std::cell::RefCell::new(rustc_hash::FxHashMap::default()),
            table_anchors: std::cell::RefCell::new(rustc_hash::FxHashMap::default()),
        }
    }

    fn signature_ordinal(&self, id: &LuaSignatureId) -> Option<usize> {
        let file_id = id.get_file_id();
        let mut cache = self.signature_ordinals.borrow_mut();
        let positions = cache.entry(file_id).or_insert_with(|| {
            let mut positions: Vec<rowan::TextSize> = self
                .db
                .get_signature_index()
                .get_file_signature_ids(file_id)
                .map(|ids| ids.iter().map(|id| id.get_position()).collect())
                .unwrap_or_default();
            positions.sort_unstable();
            positions
        });
        positions.binary_search(&id.get_position()).ok()
    }

    /// Returns stable identity for a table literal.
    fn table_identity(&self, range: &InFiled<rowan::TextRange>) -> String {
        let file_id = range.file_id;
        let mut cache = self.table_anchors.borrow_mut();
        let anchors = cache.entry(file_id).or_insert_with(|| {
            collect_anchored_map(self.db, file_id)
                .into_iter()
                .filter_map(|(anchor, anchored)| match anchor {
                    // Only a name identifies the logical table across files.
                    TableAnchor::Global(path) => Some((anchored.value, format!("G:{path}"))),
                    _ => None,
                })
                .collect()
        });
        match anchors.get(&range.value) {
            Some(anchor) => anchor.clone(),
            None => {
                drop(cache);
                format!("{}:{:?}", file_id.id, self.table_ordinal(range))
            }
        }
    }

    fn table_ordinal(&self, range: &InFiled<rowan::TextRange>) -> Option<usize> {
        let file_id = range.file_id;
        let mut cache = self.table_ordinals.borrow_mut();
        let ranges = cache.entry(file_id).or_insert_with(|| {
            let Some(tree) = self.db.get_vfs().get_syntax_tree(&file_id) else {
                return Vec::new();
            };
            let mut ranges: Vec<rowan::TextRange> = tree
                .get_chunk_node()
                .descendants::<LuaTableExpr>()
                .map(|table| table.get_range())
                .collect();
            ranges.sort_unstable_by_key(|range| (range.start(), range.end()));
            ranges
        });
        ranges
            .binary_search_by_key(&(range.value.start(), range.value.end()), |range| {
                (range.start(), range.end())
            })
            .ok()
    }
}

/// Hashes a generic parameter, recursing into its constraint so a table
/// literal's range inside one is normalised the same way it is elsewhere.
fn hash_generic_param_export(
    ids: &ExportIdentities,
    param: &GenericParam,
    hasher: &mut impl Hasher,
) {
    param.name.hash(hasher);
    format!("{:?}", param.attributes).hash(hasher);
    match &param.type_constraint {
        Some(constraint) => hash_lua_type_export(ids, constraint, hasher),
        None => "NoConstraint".hash(hasher),
    }
}

/// Hashes a type for export comparison.
fn hash_lua_type_export(ids: &ExportIdentities, typ: &LuaType, hasher: &mut impl Hasher) {
    fn hash_unordered(
        ids: &ExportIdentities,
        tag: &str,
        arms: &[LuaType],
        hasher: &mut impl Hasher,
    ) {
        tag.hash(hasher);
        let mut arm_hashes: Vec<u64> = arms
            .iter()
            .map(|arm| {
                let mut h = rustc_hash::FxHasher::default();
                hash_lua_type_export(ids, arm, &mut h);
                h.finish()
            })
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
            "TableConst".hash(hasher);
            range.file_id.hash(hasher);
            ids.table_ordinal(range).hash(hasher);
        }
        LuaType::Instance(inst) => {
            "Instance".hash(hasher);
            inst.get_range().file_id.hash(hasher);
            ids.table_ordinal(inst.get_range()).hash(hasher);
            hash_lua_type_export(ids, inst.get_base(), hasher);
        }
        LuaType::Signature(id) => {
            "Signature".hash(hasher);
            id.get_file_id().hash(hasher);
            ids.signature_ordinal(id).hash(hasher);
        }
        LuaType::Ref(id) => {
            "Ref".hash(hasher);
            id.get_name().hash(hasher);
        }
        LuaType::Def(id) => {
            "Def".hash(hasher);
            id.get_name().hash(hasher);
        }
        LuaType::Union(union) => hash_unordered(ids, "Union", &union.into_vec(), hasher),
        LuaType::Intersection(inter) => {
            hash_unordered(ids, "Intersection", inter.get_types(), hasher)
        }
        LuaType::MergedTable(merged) => {
            hash_unordered(ids, "MergedTable", merged.get_types(), hasher)
        }
        LuaType::Tuple(tuple) => {
            "Tuple".hash(hasher);
            tuple.status.hash(hasher);
            for sub in tuple.get_types() {
                hash_lua_type_export(ids, sub, hasher);
            }
        }
        LuaType::Array(arr) => {
            "Array".hash(hasher);
            format!("{:?}", arr.get_len()).hash(hasher);
            hash_lua_type_export(ids, arr.get_base(), hasher);
        }
        LuaType::Object(obj) => {
            "Object".hash(hasher);
            for (key, value) in obj.get_fields() {
                hash_lua_member_key_export(ids, key, hasher);
                hash_lua_type_export(ids, value, hasher);
            }
            for (key, value) in obj.get_index_access() {
                hash_lua_type_export(ids, key, hasher);
                hash_lua_type_export(ids, value, hasher);
            }
        }
        LuaType::TableGeneric(params) => {
            "TableGeneric".hash(hasher);
            for param in params.iter() {
                hash_lua_type_export(ids, param, hasher);
            }
        }
        LuaType::TableOf(inner) => {
            "TableOf".hash(hasher);
            hash_lua_type_export(ids, inner, hasher);
        }
        LuaType::TypeGuard(inner) => {
            "TypeGuard".hash(hasher);
            hash_lua_type_export(ids, inner, hasher);
        }
        LuaType::Generic(generic) => {
            "Generic".hash(hasher);
            generic.get_base_type_id().get_name().hash(hasher);
            for param in generic.get_params() {
                hash_lua_type_export(ids, param, hasher);
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
                    Some(param_type) => hash_lua_type_export(ids, param_type, hasher),
                    None => "NoParamType".hash(hasher),
                }
            }
            hash_lua_type_export(ids, func.get_ret(), hasher);
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
                    hash_lua_type_export(ids, base, hasher);
                }
                VariadicType::Multi(types) => {
                    "Multi".hash(hasher);
                    for sub in types {
                        hash_lua_type_export(ids, sub, hasher);
                    }
                }
            }
        }
        LuaType::Call(call) => {
            "Call".hash(hasher);
            format!("{:?}", call.get_call_kind()).hash(hasher);
            for operand in call.get_operands() {
                hash_lua_type_export(ids, operand, hasher);
            }
        }
        LuaType::MultiLineUnion(union) => {
            "MultiLineUnion".hash(hasher);
            for (arm, description) in union.get_unions() {
                description.hash(hasher);
                hash_lua_type_export(ids, arm, hasher);
            }
        }
        LuaType::Conditional(cond) => {
            "Conditional".hash(hasher);
            cond.has_new.hash(hasher);
            for param in cond.get_infer_params() {
                hash_generic_param_export(ids, param, hasher);
            }
            hash_lua_type_export(ids, cond.get_condition(), hasher);
            hash_lua_type_export(ids, cond.get_true_type(), hasher);
            hash_lua_type_export(ids, cond.get_false_type(), hasher);
        }
        LuaType::Mapped(mapped) => {
            "Mapped".hash(hasher);
            format!("{:?}", mapped.param.0).hash(hasher);
            hash_generic_param_export(ids, &mapped.param.1, hasher);
            mapped.is_readonly.hash(hasher);
            mapped.is_optional.hash(hasher);
            hash_lua_type_export(ids, &mapped.value, hasher);
        }
        LuaType::DocAttribute(attribute) => {
            "DocAttribute".hash(hasher);
            for (name, param_type) in attribute.get_params() {
                name.hash(hasher);
                match param_type {
                    Some(param_type) => hash_lua_type_export(ids, param_type, hasher),
                    None => "NoParamType".hash(hasher),
                }
            }
        }
        LuaType::StrTplRef(tpl) => {
            "StrTplRef".hash(hasher);
            tpl.get_prefix().hash(hasher);
            tpl.get_name().hash(hasher);
            tpl.get_suffix().hash(hasher);
            format!("{:?}", tpl.get_tpl_id()).hash(hasher);
            if let Some(constraint) = tpl.get_constraint() {
                hash_lua_type_export(ids, constraint, hasher);
            }
        }
        LuaType::TplRef(tpl) | LuaType::ConstTplRef(tpl) => {
            match typ {
                LuaType::ConstTplRef(_) => "ConstTplRef".hash(hasher),
                _ => "TplRef".hash(hasher),
            }
            format!("{:?}", tpl.get_tpl_id()).hash(hasher);
            tpl.get_name().hash(hasher);
            match tpl.get_constraint() {
                Some(constraint) => hash_lua_type_export(ids, constraint, hasher),
                None => "NoConstraint".hash(hasher),
            }
        }
        other => format!("{:?}", other).hash(hasher),
    }
}

/// Export key for a semantic declaration.
fn semantic_decl_export_key(ids: &ExportIdentities, id: &LuaSemanticDeclId) -> Option<String> {
    let db = ids.db;
    match id {
        LuaSemanticDeclId::TypeDecl(type_decl_id) => Some(format!("T:{}", type_decl_id.get_name())),
        LuaSemanticDeclId::LuaDecl(decl_id) => {
            let decl = db.get_decl_index().get_decl(decl_id)?;
            (!decl.is_local()).then(|| format!("D:{}", decl.get_name()))
        }
        LuaSemanticDeclId::Member(member_id) => {
            let member_index = db.get_member_index();
            let member = member_index.get_member(member_id)?;
            let mut hasher = rustc_hash::FxHasher::default();
            hash_lua_member_key_export(ids, member.get_key(), &mut hasher);
            if let Some(owner) = member_index.get_member_owner(member_id) {
                hash_member_owner_stable(ids, owner, &mut hasher);
            }
            Some(format!("M:{:x}", hasher.finish()))
        }
        LuaSemanticDeclId::Signature(signature_id) => {
            let mut file_signatures: Vec<_> = db
                .get_signature_index()
                .get_file_signature_ids(signature_id.get_file_id())?
                .iter()
                .map(|id| id.get_position())
                .collect();
            file_signatures.sort_unstable();
            let ordinal = file_signatures
                .binary_search(&signature_id.get_position())
                .ok()?;
            Some(format!("S:{ordinal}"))
        }
    }
}

/// Hash of a file's cross-file-visible exports.
pub(crate) fn file_export_fingerprint(db: &DbIndex, file_id: FileId) -> u64 {
    let mut hasher = rustc_hash::FxHasher::default();
    let ids = &ExportIdentities::new(db);

    // --- Members directly declared in this file (owner, key, feature) ---
    let member_index = db.get_member_index();
    let mut members = member_index.get_file_members(file_id);
    members.sort_by_key(|m| crate::db_index::member_id_sort_key(m.get_id()));
    for member in members {
        hash_lua_member_key_export(ids, member.get_key(), &mut hasher);
        if let Some(owner) = member_index.get_member_owner(&member.get_id()) {
            hash_member_owner_stable(ids, owner, &mut hasher);
        }
        member.get_feature().hash(&mut hasher);
    }

    // --- Per-writer assignment contributions ---
    {
        let store = member_index.member_assignment_contributions();
        let keys = store.keys_for_files(&HashSet::from([file_id]));
        let mut bucket_hashes = Vec::new();
        for (owner, key) in keys {
            let mut bh = rustc_hash::FxHasher::default();
            hash_member_owner_stable(ids, &owner, &mut bh);
            hash_lua_member_key_export(ids, &key, &mut bh);
            if let Some(contribs) = store.contributions(&(owner.clone(), key.clone())) {
                let mut contribs_vec: Vec<_> = contribs
                    .iter()
                    .filter(|(mid, _)| mid.file_id == file_id)
                    .collect();
                contribs_vec.sort_by_key(|(mid, _)| crate::db_index::member_id_sort_key(**mid));
                for (_mid, contrib) in contribs_vec {
                    hash_lua_type_export(ids, &contrib.bound_type, &mut bh);
                    hash_lua_type_export(ids, &contrib.source_type, &mut bh);
                    match &contrib.doc_type {
                        Some(doc) => hash_lua_type_export(ids, doc, &mut bh),
                        None => "NoDocType".hash(&mut bh),
                    }
                    contrib.guarded_bootstrap.hash(&mut bh);
                    contrib.preserve_table_literals.hash(&mut bh);
                }
            }
            bucket_hashes.push(bh.finish());
        }
        bucket_hashes.sort_unstable();
        for bh in bucket_hashes {
            bh.hash(&mut hasher);
        }
    }

    // --- Type decls defined in this file ---
    if let Some(decl_ids) = db.get_type_index().get_file_type_decl_ids(file_id) {
        let mut decl_ids_sorted = decl_ids.clone();
        decl_ids_sorted.sort_by(|a, b| a.get_name().cmp(b.get_name()));
        for decl_id in decl_ids_sorted {
            decl_id.get_name().hash(&mut hasher);
            if let Some(type_decl) = db.get_type_index().get_type_decl(&decl_id) {
                match type_decl.get_alias_ref() {
                    Some(alias_ref) => hash_lua_type_export(ids, alias_ref, &mut hasher),
                    None => "NoAlias".hash(&mut hasher),
                }
                let (kind, flags) = type_decl.kind_and_flags();
                format!("{kind:?}").hash(&mut hasher);
                flags.hash(&mut hasher);
                let (extra_type, flat) = type_decl.extra_type();
                flat.hash(&mut hasher);
                match extra_type {
                    Some(extra_type) => hash_lua_type_export(ids, extra_type, &mut hasher),
                    None => "NoExtra".hash(&mut hasher),
                }
            }
            if let Some(supers) = db.get_type_index().get_super_type_entries(&decl_id) {
                for sup in supers.iter().filter(|s| s.file_id == file_id) {
                    hash_lua_type_export(ids, &sup.value.typ, &mut hasher);
                }
            }
            if let Some(params) = db.get_type_index().get_generic_params(&decl_id) {
                for param in params {
                    param.name.hash(&mut hasher);
                    match &param.type_constraint {
                        Some(constraint) => hash_lua_type_export(ids, constraint, &mut hasher),
                        None => "NoConstraint".hash(&mut hasher),
                    }
                }
            }
        }
    }

    // --- Exported type caches (global/member decls, not locals) ---
    if let Some(owners) = db.get_type_index().file_type_owners(file_id) {
        let mut entries: Vec<(String, u32, u64)> = Vec::new();
        for owner in owners.iter() {
            let key = match owner {
                LuaTypeOwner::Decl(decl_id) => {
                    let Some(decl) = db.get_decl_index().get_decl(decl_id) else {
                        continue;
                    };
                    if decl.is_local() {
                        continue;
                    }
                    format!("D:{}", decl.get_name())
                }
                LuaTypeOwner::Member(member_id) => {
                    let member_index = db.get_member_index();
                    let Some(member) = member_index.get_member(member_id) else {
                        continue;
                    };
                    let mut h = rustc_hash::FxHasher::default();
                    hash_lua_member_key_export(ids, member.get_key(), &mut h);
                    if let Some(owner) = member_index.get_member_owner(member_id) {
                        hash_member_owner_stable(ids, owner, &mut h);
                    }
                    format!("M:{:x}", h.finish())
                }
                LuaTypeOwner::SyntaxId(_) => continue,
            };
            let owner_position = match owner {
                LuaTypeOwner::Decl(decl_id) => u32::from(decl_id.position),
                LuaTypeOwner::Member(member_id) => u32::from(member_id.get_position()),
                LuaTypeOwner::SyntaxId(_) => continue,
            };
            let Some(cache) = db.get_type_index().get_type_cache(owner) else {
                continue;
            };
            let mut h = rustc_hash::FxHasher::default();
            hash_lua_type_export(ids, cache.as_type(), &mut h);
            entries.push((key, owner_position, h.finish()));
        }
        entries.sort_unstable();
        for (key, _, type_hash) in entries {
            key.hash(&mut hasher);
            type_hash.hash(&mut hasher);
        }
    }

    // --- Signatures defined in this file ---
    if let Some(sig_ids) = db.get_signature_index().get_file_signature_ids(file_id) {
        let mut sig_ids_sorted: Vec<_> = sig_ids.iter().collect();
        sig_ids_sorted.sort_by_key(|id| id.get_position());
        for sig_id in sig_ids_sorted {
            if let Some(sig) = db.get_signature_index().get(sig_id) {
                sig.is_vararg.hash(&mut hasher);
                sig.is_colon_define.hash(&mut hasher);
                sig.async_state.hash(&mut hasher);
                sig.resolve_return.hash(&mut hasher);
                format!("{:?}", sig.nodiscard).hash(&mut hasher);
                sig.params.hash(&mut hasher);
                for param in &sig.generic_params {
                    param.name.hash(&mut hasher);
                    match &param.constraint {
                        Some(constraint) => hash_lua_type_export(ids, constraint, &mut hasher),
                        None => "NoConstraint".hash(&mut hasher),
                    }
                }
                let mut param_indices: Vec<&usize> = sig.param_docs.keys().collect();
                param_indices.sort_unstable();
                for idx in param_indices {
                    idx.hash(&mut hasher);
                    let doc = &sig.param_docs[idx];
                    doc.name.hash(&mut hasher);
                    doc.nullable.hash(&mut hasher);
                    doc.description.hash(&mut hasher);
                    format!("{:?}", doc.default_value).hash(&mut hasher);
                    format!("{:?}", doc.attributes).hash(&mut hasher);
                    hash_lua_type_export(ids, &doc.type_ref, &mut hasher);
                }
                for ret in &sig.return_docs {
                    ret.name.hash(&mut hasher);
                    ret.description.hash(&mut hasher);
                    format!("{:?}", ret.default_value).hash(&mut hasher);
                    format!("{:?}", ret.attributes).hash(&mut hasher);
                    format!("{:?}", ret.return_kind).hash(&mut hasher);
                    hash_lua_type_export(ids, &ret.type_ref, &mut hasher);
                }
                for overload in &sig.overloads {
                    hash_lua_type_export(ids, &LuaType::DocFunction(overload.clone()), &mut hasher);
                }
                format!("{:?}", sig.require_guard_param()).hash(&mut hasher);
                sig.nil_return_guard_params().hash(&mut hasher);
                format!("{:?}", sig.return_correlations()).hash(&mut hasher);
                format!("{:?}", sig.direct_param_return_alias()).hash(&mut hasher);
                format!("{:?}", sig.class_name_param_return_alias()).hash(&mut hasher);
                format!("{:?}", sig.falsy_param_nil_free_return_slots()).hash(&mut hasher);
                format!("{:?}", sig.falsy_param_return_aliases()).hash(&mut hasher);
                for out_param in &sig.out_params {
                    format!("{:?}", out_param.root).hash(&mut hasher);
                    out_param.field_path.hash(&mut hasher);
                    hash_lua_type_export(ids, &out_param.type_ref, &mut hasher);
                }
            }
            if let Some(guard) = db.get_signature_index().inferred_positive_guard(sig_id) {
                guard.param_idx.hash(&mut hasher);
                hash_lua_type_export(ids, &guard.narrowed_type, &mut hasher);
            }
        }
    }

    // --- Parameter types this file's call sites are evidence for ---
    {
        let contributed = db
            .get_call_site_param_index()
            .inferred_params_for_contributor_files(&HashSet::from([file_id]));
        let mut param_hashes: Vec<u64> = contributed
            .iter()
            .map(|((signature_id, param_idx), typ)| {
                let mut h = rustc_hash::FxHasher::default();
                signature_id.get_file_id().hash(&mut h);
                param_idx.hash(&mut h);
                hash_lua_type_export(ids, typ, &mut h);
                h.finish()
            })
            .collect();
        param_hashes.sort_unstable();
        param_hashes.hash(&mut hasher);
    }

    // --- Inferred guard facts produced by this file ---
    let guard_facts = db
        .get_signature_index()
        .inferred_guard_facts_for_files(&HashSet::from([file_id]));
    if !guard_facts.is_empty() {
        let mut guard_owners: Vec<_> = guard_facts.keys().cloned().collect();
        sort_inferred_guard_owners(&mut guard_owners);
        for owner in guard_owners {
            owner.path().hash(&mut hasher);
            format!("{:?}", owner.state_mask()).hash(&mut hasher);
            owner.source_file_id().hash(&mut hasher);
            if let Some(guard) = guard_facts.get(&owner) {
                guard.param_idx.hash(&mut hasher);
                hash_lua_type_export(ids, &guard.narrowed_type, &mut hasher);
            }
        }
    }

    // --- Annotations on this file's symbols that other files act on ---
    {
        let default = LuaCommonProperty::new();
        let default_property = format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            default.visibility,
            default.deprecated,
            default.export,
            default.decl_features,
            default.version_conds,
            default.attribute_uses,
            default.default_value,
            default.tag_content,
        );
        let mut properties: Vec<(String, String)> = db
            .get_property_index()
            .properties_in_file(file_id)
            .into_iter()
            .filter_map(|(owner, property)| {
                let key = semantic_decl_export_key(ids, owner)?;
                let acted_on = format!(
                    "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
                    property.visibility,
                    property.deprecated,
                    property.export,
                    property.decl_features,
                    property.version_conds,
                    property.attribute_uses,
                    property.default_value,
                    property.tag_content,
                );
                (acted_on != default_property).then_some((key, acted_on))
            })
            .collect();
        properties.sort_unstable();
        properties.hash(&mut hasher);
    }

    // --- Metamethods this file declares ---
    {
        let mut operators: Vec<u64> = db
            .get_operator_index()
            .operators_in_file(file_id)
            .into_iter()
            .map(|operator| {
                let mut h = rustc_hash::FxHasher::default();
                match operator.get_owner() {
                    LuaOperatorOwner::Table(range) => {
                        "Table".hash(&mut h);
                        range.file_id.hash(&mut h);
                        ids.table_ordinal(&range).hash(&mut h);
                    }
                    LuaOperatorOwner::Type(type_decl_id) => {
                        "Type".hash(&mut h);
                        type_decl_id.get_name().hash(&mut h);
                    }
                }
                format!("{:?}", operator.get_op()).hash(&mut h);
                hash_lua_type_export(ids, &operator.get_operator_func(db), &mut h);
                h.finish()
            })
            .collect();
        operators.sort_unstable();
        operators.hash(&mut hasher);
    }

    // --- Network flows this file declares ---
    if let Some(network) = db.get_gmod_network_index().get_file_data(file_id) {
        fn hash_ops(ops: &[NetOpEntry], hasher: &mut impl Hasher) {
            for entry in ops {
                format!("{:?}", entry.op).hash(hasher);
                entry.display_name.hash(hasher);
                entry.dynamic.hash(hasher);
                format!("{:?}", entry.bits).hash(hasher);
            }
        }
        let mut flows: Vec<u64> = Vec::new();
        for flow in &network.send_flows {
            let mut h = rustc_hash::FxHasher::default();
            "Send".hash(&mut h);
            flow.message_name.hash(&mut h);
            format!("{:?}", flow.send_kind).hash(&mut h);
            flow.send_display_name.hash(&mut h);
            flow.send_target.hash(&mut h);
            flow.is_wrapped.hash(&mut h);
            hash_ops(&flow.writes, &mut h);
            flows.push(h.finish());
        }
        for flow in &network.receive_flows {
            let mut h = rustc_hash::FxHasher::default();
            "Receive".hash(&mut h);
            flow.message_name.hash(&mut h);
            flow.reads_opaque.hash(&mut h);
            hash_ops(&flow.reads, &mut h);
            flows.push(h.finish());
        }
        flows.sort_unstable();
        flows.hash(&mut hasher);
    }

    // --- Metatable bindings this file declares ---
    {
        let metatable_index = db.get_metatable_index();
        let mut bindings: Vec<(usize, u32, usize)> = Vec::new();
        if let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) {
            for table in tree.get_chunk_node().descendants::<LuaTableExpr>() {
                let range = InFiled::new(file_id, table.get_range());
                let Some(metatable) = metatable_index.get(&range) else {
                    continue;
                };
                let Some(table_ordinal) = ids.table_ordinal(&range) else {
                    continue;
                };
                bindings.push((
                    table_ordinal,
                    metatable.file_id.id,
                    ids.table_ordinal(metatable).unwrap_or(usize::MAX),
                ));
            }
        }
        bindings.sort_unstable();
        bindings.hash(&mut hasher);
    }

    // --- The realm each exported symbol is declared in ---
    {
        let gmod_infer = db.get_gmod_infer_index();
        let mut realms: Vec<(String, String)> = Vec::new();
        if let Some(decl_tree) = db.get_decl_index().get_decl_tree(&file_id) {
            for (decl_id, decl) in decl_tree.get_decls() {
                if decl.is_local() {
                    continue;
                }
                let realm = gmod_infer.get_realm_at_offset(&file_id, decl_id.position);
                realms.push((format!("D:{}", decl.get_name()), format!("{realm:?}")));
            }
        }
        for member in db.get_member_index().get_file_members(file_id) {
            let mut h = rustc_hash::FxHasher::default();
            hash_lua_member_key_export(ids, member.get_key(), &mut h);
            if let Some(owner) = db.get_member_index().get_member_owner(&member.get_id()) {
                hash_member_owner_stable(ids, owner, &mut h);
            }
            let realm = gmod_infer.get_realm_at_offset(&file_id, member.get_id().get_position());
            realms.push((format!("M:{:x}", h.finish()), format!("{realm:?}")));
        }
        realms.sort_unstable();
        realms.dedup();
        realms.hash(&mut hasher);

        if let Some(metadata) = gmod_infer.get_realm_file_metadata(&file_id) {
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
            .hash(&mut hasher);
            let branch_realms: Vec<String> = metadata
                .branch_realm_ranges
                .iter()
                .map(|range| format!("{:?}", range.realm))
                .collect();
            branch_realms.hash(&mut hasher);
        }
    }

    // --- What this file exports as a module ---
    if let Some(module) = db.get_module_index().get_module(file_id) {
        module.full_module_name.hash(&mut hasher);
        module.visible.hash(&mut hasher);
        module.is_meta.hash(&mut hasher);
        format!("{:?}", module.workspace_id).hash(&mut hasher);
        format!("{:?}", module.version_conds).hash(&mut hasher);
        match &module.export_type {
            Some(export_type) => hash_lua_type_export(ids, export_type, &mut hasher),
            None => "NoExport".hash(&mut hasher),
        }
        match module
            .semantic_id
            .as_ref()
            .and_then(|id| semantic_decl_export_key(ids, id))
        {
            Some(key) => key.hash(&mut hasher),
            None => "NoSemanticId".hash(&mut hasher),
        }
    }

    // --- Load edges this file declares ---
    {
        let dependency_index = db.get_file_dependencies_index();
        let mut sites: Vec<String> = dependency_index
            .get_dependency_sites(&file_id)
            .unwrap_or_default()
            .iter()
            .map(|site| {
                format!(
                    "{:?}|{:?}|{:?}|{}",
                    site.kind, site.target_file_id, site.path, site.original_expr
                )
            })
            .collect();
        sites.sort_unstable();
        sites.hash(&mut hasher);

        let mut required: Vec<u32> = dependency_index
            .get_required_files(&file_id)
            .map(|files| files.iter().map(|file| file.id).collect())
            .unwrap_or_default();
        required.sort_unstable();
        required.hash(&mut hasher);
    }

    // --- Namespace / using (affects type resolution) ---
    if let Some(ns) = db.get_type_index().get_file_namespace(&file_id) {
        ns.hash(&mut hasher);
    }
    if let Some(using) = db.get_type_index().get_file_using_namespace(&file_id) {
        for ns in using {
            ns.hash(&mut hasher);
        }
    }

    hasher.finish()
}

fn global_path_for_expr(expr: &LuaExpr) -> Option<Vec<smol_str::SmolStr>> {
    let mut path = match expr {
        LuaExpr::NameExpr(name_expr) => {
            Some(vec![name_expr.get_name_token()?.get_name_text().into()])
        }
        LuaExpr::IndexExpr(index_expr) => {
            if index_expr.get_index_token()?.is_colon() {
                return None;
            }
            let mut path = global_path_for_expr(&index_expr.get_prefix_expr()?)?;
            let member = match index_expr.get_index_key()? {
                LuaIndexKey::Name(name) => name.get_name_text().into(),
                LuaIndexKey::String(string) => string.get_value().into(),
                _ => return None,
            };
            path.push(member);
            Some(path)
        }
        _ => None,
    }?;
    canonicalize_global_root_path(&mut path);
    Some(path)
}

fn immutable_local_alias_decl(
    db: &DbIndex,
    file_id: FileId,
    alias_value: &LuaExpr,
) -> Option<LuaDeclId> {
    let alias_value = enclosing_parenthesized_expr(alias_value);
    let local_stat = alias_value.get_parent::<LuaLocalStat>()?;
    let local_name = local_stat.get_local_name_by_value(alias_value.clone())?;
    let decl_id = LuaDeclId::new(file_id, local_name.get_position());
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    if !matches!(decl.extra, LuaDeclExtra::Local { .. })
        || decl.get_value_syntax_id() != Some(alias_value.get_syntax_id())
        || db
            .get_reference_index()
            .get_decl_references(&file_id, &decl_id)
            .is_none_or(|references| references.mutable)
    {
        return None;
    }
    Some(decl_id)
}

fn enclosing_parenthesized_expr(expr: &LuaExpr) -> LuaExpr {
    let mut expr = expr.clone();
    while let Some(paren_expr) = expr.get_parent::<LuaParenExpr>() {
        if paren_expr
            .get_expr()
            .is_none_or(|inner| inner.get_syntax_id() != expr.get_syntax_id())
        {
            break;
        }
        expr = LuaExpr::ParenExpr(paren_expr);
    }
    expr
}

fn is_call_prefix(expr: &LuaExpr) -> bool {
    let expr = enclosing_parenthesized_expr(expr);
    expr.get_parent::<LuaCallExpr>()
        .and_then(|call| call.get_prefix_expr())
        .is_some_and(|prefix| prefix.get_syntax_id() == expr.get_syntax_id())
}

fn expr_resolves_to_inferred_guard_owner(
    db: &DbIndex,
    caches: &mut HashMap<FileId, LuaInferCache>,
    owner: &LuaInferredGuardOwner,
    file_id: FileId,
    expr: &LuaExpr,
) -> bool {
    let cache = caches
        .entry(file_id)
        .or_insert_with(|| LuaInferCache::new(file_id, Default::default()));
    semantic::infer_expr(db, cache, expr.clone()).ok()
        == Some(LuaType::Signature(owner.signature_id()))
}

fn call_resolves_to_inferred_guard_owner(
    db: &DbIndex,
    caches: &mut HashMap<FileId, LuaInferCache>,
    owner: &LuaInferredGuardOwner,
    file_id: FileId,
    prefix_expr: &LuaExpr,
) -> bool {
    let prefix_expr = enclosing_parenthesized_expr(prefix_expr);
    let Some(call) = prefix_expr.get_parent::<LuaCallExpr>() else {
        return false;
    };
    if call
        .get_prefix_expr()
        .is_none_or(|prefix| prefix.get_syntax_id() != prefix_expr.get_syntax_id())
    {
        return false;
    }
    let cache = caches
        .entry(file_id)
        .or_insert_with(|| LuaInferCache::new(file_id, Default::default()));
    semantic::get_prefix_expr_signature_id(db, cache, &call) == Some(owner.signature_id())
}

/// True when `call_expr` is an annotated net operation.
pub fn call_expr_is_net_op(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> bool {
    let Some(signature_id) = semantic::get_prefix_expr_signature_id(db, cache, call_expr) else {
        return false;
    };
    db_index::signature_has_net_op_metadata(db, signature_id)
}

pub async fn fetch_schema_urls(urls: Vec<Url>) -> HashMap<Url, String> {
    let mut url_contents = HashMap::new();
    for url in urls {
        if url.scheme() == "file" {
            if let Ok(path) = url.to_file_path()
                && path.exists()
            {
                let result = read_file_with_encoding(&path, "utf-8");
                if let Some(content) = result {
                    url_contents.insert(url, content);
                } else {
                    log::error!("Failed to read schema file: {:?}", path);
                }
            }
        } else {
            let result = reqwest::get(url.as_str()).await;
            if let Ok(response) = result {
                if let Ok(content) = response.text().await {
                    url_contents.insert(url, content);
                } else {
                    log::error!("Failed to read schema content from URL: {:?}", url);
                }
            } else {
                log::error!("Failed to fetch schema from URL: {:?}", url);
            }
        }
    }

    url_contents
}

/// Normalize workspace root for VFS path matching.
fn normalize_workspace_root(root: PathBuf) -> PathBuf {
    file_path_to_uri(&root)
        .and_then(|uri| uri_to_file_path(&uri))
        .unwrap_or(root)
}

pub(crate) fn dependency_site_path_keys(
    db: &DbIndex,
    source_file_id: FileId,
    dependency_path: &str,
) -> Vec<String> {
    let dependency_path = normalize_dependency_path(dependency_path);
    if dependency_path.is_empty() {
        return Vec::new();
    }

    let mut keys = HashSet::new();
    insert_dependency_path_key_variants(&mut keys, dependency_path.clone());

    if let Some(source_parent) = db
        .get_vfs()
        .get_file_path(&source_file_id)
        .and_then(|source_path| source_path.parent())
    {
        let relative_candidate =
            lexically_normalize_path(&source_parent.join(Path::new(&dependency_path)));
        insert_dependency_path_key_variants(&mut keys, normalize_file_path(&relative_candidate));
    }

    let mut keys = keys.into_iter().collect::<Vec<_>>();
    keys.sort_unstable();
    keys
}

fn dependency_path_keys_for_target(db: &DbIndex, target_path: &Path) -> Vec<String> {
    let mut keys = HashSet::new();
    let Some(target_path_text) = target_path.to_str() else {
        return Vec::new();
    };

    let normalized_target_path = normalize_file_path(target_path);
    insert_dependency_path_key_variants(&mut keys, normalized_target_path.clone());

    if let Some(lua_idx) = normalized_target_path.find("/lua/") {
        let lua_relative = normalized_target_path[(lua_idx + 1)..].to_string();
        insert_dependency_path_key_variants(&mut keys, lua_relative.clone());
        insert_dependency_path_key_variants(&mut keys, lua_relative.replace('/', "."));
        if let Some(without_lua) = lua_relative.strip_prefix("lua/") {
            insert_dependency_path_key_variants(&mut keys, without_lua.to_string());
            insert_dependency_path_key_variants(&mut keys, without_lua.replace('/', "."));
        }
    }

    if let Some((module_path, _)) = db.get_module_index().extract_module_path(target_path_text) {
        let module_path = normalize_dependency_path(&module_path.replace('\\', "/"));
        insert_dependency_path_key_variants(&mut keys, module_path.replace('.', "/"));
        insert_dependency_path_key_variants(&mut keys, module_path);
    }

    let mut keys = keys.into_iter().collect::<Vec<_>>();
    keys.sort_unstable();
    keys
}

fn insert_dependency_path_key_variants(keys: &mut HashSet<String>, path: String) {
    let normalized = normalize_dependency_path(&path);
    if normalized.is_empty() {
        return;
    }
    keys.insert(normalized.clone());
    if let Some(without_lua_ext) = normalized.strip_suffix(".lua") {
        keys.insert(without_lua_ext.to_string());
    } else {
        keys.insert(format!("{normalized}.lua"));
    }
}

fn normalize_dependency_path(path: &str) -> String {
    let mut normalized = normalize_path_case(path.replace('\\', "/"));
    while let Some(stripped) = normalized.strip_prefix("./") {
        normalized = stripped.to_string();
    }
    normalized.trim_matches('/').to_string()
}

fn normalize_file_path(path: &Path) -> String {
    normalize_path_case(path.to_string_lossy().replace('\\', "/"))
        .trim_end_matches('/')
        .to_string()
}

fn normalize_path_case(path: String) -> String {
    #[cfg(target_os = "windows")]
    {
        path.to_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        path
    }
}

fn lexically_normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(crate) enum TableAnchor {
    Global(String),
    Local {
        decl_name: String,
        path: String,
        /// Field names declared by the table.
        fields: Vec<String>,
    },
    /// Literal passed as call argument, keyed by call path.
    CallArgument {
        path: String,
        labels: Vec<String>,
        arg_index: usize,
    },
    /// Literal with no name or call, keyed by field names.
    Fields(Vec<String>),
}

fn expr_path_strings(expr: &LuaExpr) -> Option<Vec<String>> {
    match expr {
        LuaExpr::NameExpr(name_expr) => Some(vec![name_expr.get_name_text()?.to_string()]),
        LuaExpr::IndexExpr(index_expr) => {
            if index_expr.get_index_token()?.is_colon() {
                return None;
            }
            let mut path = expr_path_strings(&index_expr.get_prefix_expr()?)?;
            let name = match index_expr.get_index_key()? {
                LuaIndexKey::Name(n) => n.get_name_text().to_string(),
                LuaIndexKey::String(s) => s.get_value().to_string(),
                LuaIndexKey::Integer(i) => i.syntax().text().to_string(),
                LuaIndexKey::Expr(_) | LuaIndexKey::Idx(_) => return None,
            };
            path.push(name);
            Some(path)
        }
        _ => None,
    }
}

fn var_path_strings(var: &glua_parser::LuaVarExpr) -> Option<Vec<String>> {
    match var {
        glua_parser::LuaVarExpr::NameExpr(n) => Some(vec![n.get_name_text()?.to_string()]),
        glua_parser::LuaVarExpr::IndexExpr(idx) => {
            let expr: LuaExpr = LuaExpr::IndexExpr(idx.clone());
            expr_path_strings(&expr)
        }
    }
}

fn table_global_path_recursive(table: LuaTableExpr) -> Option<String> {
    if let Some(field) = table.get_parent::<LuaTableField>() {
        let key = field.get_field_key()?;
        let key_str = match key {
            glua_parser::LuaIndexKey::Name(n) => n.get_name_text().to_string(),
            glua_parser::LuaIndexKey::String(s) => s.get_value().to_string(),
            _ => return None,
        };
        let parent_table = field.get_parent::<LuaTableExpr>()?;
        let parent_path = table_global_path_recursive(parent_table)?;
        return Some(format!("{}.{}", parent_path, key_str));
    }
    let mut current = table.syntax().clone();
    while let Some(parent) = current.parent() {
        if let Some(assign) = LuaAssignStat::cast(parent.clone()) {
            let (vars, exprs) = assign.get_var_and_expr_list();
            for (var, expr) in vars.iter().zip(exprs.iter()) {
                if expr.get_range() == table.get_range() {
                    if let Some(mut path) = var_path_strings(var) {
                        if path.len() > 1 && matches!(path[0].as_str(), "_G" | "_ENV") {
                            path.remove(0);
                        }
                        return Some(path.join("."));
                    }
                }
            }
            break;
        }
        if glua_parser::LuaLocalStat::can_cast(parent.kind().into()) {
            break;
        }
        current = parent;
    }
    None
}

/// Sorted field names declared by a table literal.
fn table_field_names(table: &LuaTableExpr) -> Vec<String> {
    let mut fields: Vec<String> = table
        .get_fields()
        .filter_map(|field| match field.get_field_key()? {
            glua_parser::LuaIndexKey::Name(name) => Some(name.get_name_text().to_string()),
            glua_parser::LuaIndexKey::String(text) => Some(text.get_value().to_string()),
            glua_parser::LuaIndexKey::Integer(number) => Some(format!("{number:?}")),
            _ => None,
        })
        .collect();
    fields.sort_unstable();
    fields
}

fn table_local_anchor(db: &DbIndex, file_id: FileId, table: LuaTableExpr) -> Option<TableAnchor> {
    let fields = table_field_names(&table);
    let mut parts: Vec<String> = Vec::new();
    let mut cur = table;
    loop {
        if let Some(field) = cur.get_parent::<LuaTableField>() {
            let key = field.get_field_key()?;
            let key_str = match key {
                glua_parser::LuaIndexKey::Name(n) => n.get_name_text().to_string(),
                glua_parser::LuaIndexKey::String(s) => s.get_value().to_string(),
                _ => return None,
            };
            parts.push(key_str);
            if let Some(parent_table) = field.get_parent::<LuaTableExpr>() {
                cur = parent_table;
                continue;
            } else {
                return None;
            }
        }
        let parent = cur.syntax().parent()?;
        if let Some(local) = glua_parser::LuaLocalStat::cast(parent.clone()) {
            let values = local.get_value_exprs().collect::<Vec<_>>();
            let idx = values
                .iter()
                .position(|v| v.get_range() == cur.get_range())?;
            let names = local.get_local_name_list().collect::<Vec<_>>();
            let name = names.get(idx)?;
            parts.reverse();
            let path = parts.join(".");
            let name_text = name.get_name_token()?.get_name_text().to_string();
            return Some(TableAnchor::Local {
                decl_name: name_text,
                fields,
                path,
            });
        }
        if let Some(assign) = LuaAssignStat::cast(parent.clone()) {
            let (vars, exprs) = assign.get_var_and_expr_list();
            let idx = exprs
                .iter()
                .position(|e| e.get_range() == cur.get_range())?;
            let var = vars.get(idx)?;
            match var {
                glua_parser::LuaVarExpr::NameExpr(name_expr) => {
                    let decl_tree = db.get_decl_index().get_decl_tree(&file_id)?;
                    let name_text = name_expr.get_name_text()?;
                    let decl =
                        decl_tree.find_local_decl(name_text.as_str(), name_expr.get_position())?;
                    parts.reverse();
                    let path = parts.join(".");
                    return Some(TableAnchor::Local {
                        decl_name: decl.get_name().to_string(),
                        fields,
                        path,
                    });
                }
                glua_parser::LuaVarExpr::IndexExpr(_) => {
                    let var_path = var_path_strings(var)?;
                    let root_name = var_path.first()?.clone();
                    let root_name_expr = var
                        .syntax()
                        .descendants()
                        .find_map(glua_parser::LuaNameExpr::cast)?;
                    let decl_tree = db.get_decl_index().get_decl_tree(&file_id)?;
                    let decl =
                        decl_tree.find_local_decl(&root_name, root_name_expr.get_position())?;
                    let suffix = if var_path.len() > 1 {
                        var_path[1..].join(".")
                    } else {
                        String::new()
                    };
                    parts.reverse();
                    let inner = parts.join(".");
                    let mut combined = Vec::new();
                    if !suffix.is_empty() {
                        combined.push(suffix);
                    }
                    if !inner.is_empty() {
                        combined.push(inner);
                    }
                    let final_path = combined.join(".");
                    return Some(TableAnchor::Local {
                        decl_name: decl.get_name().to_string(),
                        fields,
                        path: final_path,
                    });
                }
            }
        }
        return None;
    }
}

/// Call containing a table literal argument.
fn table_call_argument_anchor(table: &LuaTableExpr) -> Option<TableAnchor> {
    let arg_list = table.syntax().parent()?;
    let call = LuaCallExpr::cast(arg_list.parent()?)?;
    let path = var_path_strings(&glua_parser::LuaVarExpr::cast(
        call.get_prefix_expr()?.syntax().clone(),
    )?)?
    .join(".");

    let mut arg_index = 0;
    let mut labels = Vec::new();
    for (index, arg) in call.get_args_list()?.get_args().enumerate() {
        match &arg {
            LuaExpr::LiteralExpr(literal) => {
                if let Some(glua_parser::LuaLiteralToken::String(text)) = literal.get_literal() {
                    labels.push(format!("{index}:{}", text.get_value()));
                }
            }
            _ => {
                if arg.get_range() == table.get_range() {
                    arg_index = index;
                }
            }
        }
    }
    (!labels.is_empty()).then_some(TableAnchor::CallArgument {
        path,
        labels,
        arg_index,
    })
}

type AnchorMaps =
    rustc_hash::FxHashMap<FileId, rustc_hash::FxHashMap<TableAnchor, InFiled<rowan::TextRange>>>;

pub(crate) fn collect_anchored_map(
    db: &DbIndex,
    file_id: FileId,
) -> rustc_hash::FxHashMap<TableAnchor, InFiled<rowan::TextRange>> {
    use rustc_hash::FxHashMap;
    let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) else {
        return FxHashMap::default();
    };
    let chunk = tree.get_chunk_node();

    // Two passes: keep only anchors that uniquely identify a literal.
    let candidates: Vec<(Option<TableAnchor>, LuaTableExpr)> = chunk
        .descendants::<LuaTableExpr>()
        .map(|table| {
            let named = table_global_path_recursive(table.clone())
                .map(TableAnchor::Global)
                .or_else(|| table_local_anchor(db, file_id, table.clone()))
                .or_else(|| table_call_argument_anchor(&table))
                .or_else(|| {
                    let fields = table_field_names(&table);
                    (!fields.is_empty()).then_some(TableAnchor::Fields(fields))
                });
            (named, table)
        })
        .collect();

    let mut counts: FxHashMap<&TableAnchor, usize> = FxHashMap::default();
    for (anchor, _) in &candidates {
        if let Some(anchor) = anchor {
            *counts.entry(anchor).or_default() += 1;
        }
    }
    let ambiguous: rustc_hash::FxHashSet<TableAnchor> = counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(anchor, _)| anchor.clone())
        .collect();

    // Skip literals with no unique anchor.
    let mut map: FxHashMap<TableAnchor, InFiled<rowan::TextRange>> = FxHashMap::default();
    for (anchor, table) in candidates {
        let Some(anchor) = anchor.filter(|anchor| !ambiguous.contains(anchor)) else {
            continue;
        };
        map.insert(anchor, InFiled::new(file_id, table.get_range()));
    }
    map
}

#[derive(Debug)]
pub struct EmmyLuaAnalysis {
    pub compilation: LuaCompilation,
    pub diagnostic: LuaDiagnostic,
    pub emmyrc: Arc<Emmyrc>,
    #[cfg(test)]
    pub(crate) inferred_guard_propagation_stats: InferredGuardPropagationStats,
    #[cfg(test)]
    cross_file_stabilization_invocations: usize,
    /// Guard facts before self-index.
    pending_guard_snapshot: Option<InferredGuardSnapshot>,
    pending_export_fingerprints: rustc_hash::FxHashMap<FileId, u64>,
    pending_table_ranges: rustc_hash::FxHashMap<
        FileId,
        rustc_hash::FxHashMap<TableAnchor, InFiled<rowan::TextRange>>,
    >,
}

impl EmmyLuaAnalysis {
    pub fn new() -> Self {
        let emmyrc = Arc::new(Emmyrc::default());
        Self {
            compilation: LuaCompilation::new(emmyrc.clone()),
            diagnostic: LuaDiagnostic::new(),
            emmyrc,
            #[cfg(test)]
            inferred_guard_propagation_stats: InferredGuardPropagationStats::default(),
            #[cfg(test)]
            cross_file_stabilization_invocations: 0,
            pending_guard_snapshot: None,
            pending_export_fingerprints: rustc_hash::FxHashMap::default(),
            pending_table_ranges: rustc_hash::FxHashMap::default(),
        }
    }

    pub fn init_std_lib(&mut self) {
        let is_jit = matches!(self.emmyrc.runtime.version, EmmyrcLuaVersion::LuaJIT);
        let (std_root, files) = load_resource_std(is_jit);
        // Normalize drive-letter casing for VFS matching.
        let std_root = normalize_workspace_root(std_root);
        self.init_std_lib_from_files(std_root, files);
    }

    /// Register a pre-built set of embedded std files directly into the analysis
    /// workspace without going through the resource-loading pipeline.
    pub(crate) fn init_std_lib_from_files(&mut self, std_root: PathBuf, files: Vec<LuaFileInfo>) {
        self.compilation
            .get_db_mut()
            .get_module_index_mut()
            .add_workspace_root_with_kind(std_root, WorkspaceId::STD, WorkspaceKind::Std);

        let files = files
            .into_iter()
            .filter_map(|file| {
                if file.path.ends_with(".lua") {
                    Some((PathBuf::from(file.path), Some(file.content)))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        self.update_files_by_path(files);
    }

    pub fn get_file_id(&self, uri: &Uri) -> Option<FileId> {
        self.compilation.get_db().get_vfs().get_file_id(uri)
    }

    pub fn get_uri(&self, file_id: FileId) -> Option<Uri> {
        self.compilation.get_db().get_vfs().get_uri(&file_id)
    }

    pub fn add_main_workspace(&mut self, root: PathBuf) {
        let root = normalize_workspace_root(root);
        let module_index = self.compilation.get_db_mut().get_module_index_mut();
        let id = WorkspaceId {
            id: module_index.next_main_workspace_id(),
        };
        module_index.add_workspace_root_with_kind(root, id, WorkspaceKind::Main);
    }

    pub fn add_library_workspace(&mut self, root: PathBuf) {
        let root = normalize_workspace_root(root);
        let module_index = self.compilation.get_db_mut().get_module_index_mut();
        let id = WorkspaceId {
            id: module_index.next_library_workspace_id(),
        };
        module_index.add_workspace_root_with_kind(root, id, WorkspaceKind::Library);
    }

    pub fn update_file_by_uri(&mut self, uri: &Uri, text: Option<String>) -> Option<FileId> {
        let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(uri);
        if let Some(file_id) = existing_file_id {
            if let (Some(new_text), Some(old_text)) = (
                text.as_deref(),
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_file_content(&file_id)
                    .map(String::as_str),
            ) && old_text == new_text
            {
                // Text unchanged — if the index is already built (has module info),
                if self
                    .compilation
                    .get_db()
                    .get_module_index()
                    .get_module(file_id)
                    .is_some()
                {
                    return Some(file_id);
                }

                // Index was cleared — fall through to rebuild it.
                self.compilation.remove_index(vec![file_id]);
                self.compilation.update_index(vec![file_id]);
                return Some(file_id);
            }
        } else if text.is_none() {
            return None;
        }

        // An edit whose significant token stream is unchanged — same kinds,
        if let (Some(file_id), Some(new_text)) = (existing_file_id, text.as_deref())
            && self
                .compilation
                .get_db()
                .get_module_index()
                .get_module(file_id)
                .is_some()
            && self
                .compilation
                .get_db_mut()
                .get_vfs_mut()
                .content_semantically_matches(file_id, new_text)
        {
            self.compilation
                .get_db_mut()
                .get_vfs_mut()
                .set_file_content(uri, text);
            return Some(file_id);
        }

        // Change-aware incremental edit: only expand to dependents when the
        if let Some(existing) = existing_file_id.filter(|_| text.is_some()) {
            // Both are taken before the VFS mutation. A dependent is a file
            let before_fp = self.take_pre_edit_fingerprint(existing);
            let before_expansion = self.expand_reindex_file_ids(vec![existing]);
            let old_guard_snapshot = self
                .inferred_guard_snapshot(&before_expansion.iter().copied().collect::<HashSet<_>>());
            // Inferred guards and VGUI forwarding are derived from state the
            let is_special = {
                let db = self.compilation.get_db();
                !db.get_signature_index()
                    .inferred_guard_facts_for_files(&HashSet::from([existing]))
                    .is_empty()
                    || db
                        .get_gmod_class_metadata_index()
                        .has_annotated_vgui_parent_calls(existing)
            };
            if is_special {
                let old_maps = self.take_old_anchor_maps(&[existing]);
                self.pending_export_fingerprints.remove(&existing);
                let file_id = self
                    .compilation
                    .get_db_mut()
                    .get_vfs_mut()
                    .set_file_content(uri, text);
                self.reindex_expanded_files_with_old_snapshot(
                    vec![file_id],
                    before_expansion,
                    old_guard_snapshot,
                );
                self.apply_table_remap(old_maps, &[file_id]);
                profile::phase_report("update_file_by_uri");
                return Some(file_id);
            }
            self.stash_pre_edit_anchors(existing);
            let file_id = self
                .compilation
                .get_db_mut()
                .get_vfs_mut()
                .set_file_content(uri, text);
            // Self-index the edited file so its entries match its text (all a
            profile::phase("edit/self-index", || {
                self.self_index_files(vec![file_id]);
                // The self-index derives this file's cross-file reads in
                self.stabilize_cross_file_type_caches(&[file_id]);
            });
            let after_fp = file_export_fingerprint(self.compilation.get_db(), file_id);
            if before_fp == after_fp {
                profile::phase_report("update_file_by_uri (no-ripple)");
                return Some(file_id);
            }
            profile::phase("edit/ripple", || {
                self.reindex_expanded_files_with_old_snapshot(
                    vec![file_id],
                    before_expansion,
                    old_guard_snapshot,
                )
            });
            profile::phase_report("update_file_by_uri");
            return Some(file_id);
        }

        let old_maps = existing_file_id
            .map(|file_id| self.take_old_anchor_maps(&[file_id]))
            .unwrap_or_default();
        let file_id = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content(uri, text);
        let expansion = self.expand_reindex_file_ids(vec![file_id]);
        profile::phase("edit/reindex", || {
            self.reindex_expanded_files(vec![file_id], expansion)
        });
        // A deleted file has no tree, so every one of its literals is gone and
        self.apply_table_remap(old_maps, &[file_id]);
        profile::phase_report("update_file_by_uri");

        Some(file_id)
    }

    pub fn update_file_preparsed(
        &mut self,
        uri: Uri,
        text: Option<String>,
        tree: LuaSyntaxTree,
        line_index: LineIndex,
        version: Option<i32>,
        trigger_reindex: bool,
    ) -> Option<FileId> {
        let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(&uri);
        if let Some(file_id) = existing_file_id {
            if let (Some(incoming_version), Some(current_version)) = (
                version,
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_file_version(&file_id),
            ) && incoming_version < current_version
            {
                return None;
            }

            if let (Some(new_text), Some(old_text)) = (
                text.as_deref(),
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_file_content(&file_id)
                    .map(String::as_str),
            ) && old_text == new_text
            {
                if self
                    .compilation
                    .get_db()
                    .get_module_index()
                    .get_module(file_id)
                    .is_some()
                {
                    self.compilation
                        .get_db_mut()
                        .get_vfs_mut()
                        .update_file_version(&file_id, version);
                    return Some(file_id);
                }

                if trigger_reindex {
                    // Through `self_index_files`, so the anchor stash an
                    self.self_index_files(vec![file_id]);
                    self.pending_export_fingerprints.remove(&file_id);
                }

                self.compilation
                    .get_db_mut()
                    .get_vfs_mut()
                    .update_file_version(&file_id, version);
                return Some(file_id);
            }
        } else if text.is_none() {
            return None;
        }

        let is_removed = text.is_none();
        let (existing_reindex_file_ids, old_guard_facts) = if trigger_reindex {
            let removed_file_ids = existing_file_id
                .filter(|_| is_removed)
                .into_iter()
                .collect::<HashSet<_>>();
            let mut reindex_file_ids =
                existing_file_id.map(|file_id| self.expand_reindex_file_ids(vec![file_id]));
            if let Some(reindex_file_ids) = &mut reindex_file_ids {
                self.add_vgui_forwarding_removal_seed(&removed_file_ids, reindex_file_ids);
            }
            let old_guard_fact_file_ids = reindex_file_ids
                .iter()
                .flatten()
                .copied()
                .collect::<HashSet<_>>();
            (
                reindex_file_ids,
                self.inferred_guard_snapshot(&old_guard_fact_file_ids),
            )
        } else {
            (None, InferredGuardSnapshot::default())
        };

        // The anchors have to be read before the VFS mutation drops the old
        let old_maps = match existing_file_id {
            Some(fid) if trigger_reindex => self.take_old_anchor_maps(&[fid]),
            Some(fid) => {
                self.stash_pre_edit_state(fid);
                AnchorMaps::default()
            }
            None => AnchorMaps::default(),
        };

        let file_id = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content_preparsed(&uri, text, tree, line_index, version)?;
        let incremental_source_file_ids = HashSet::from([file_id]);

        if trigger_reindex {
            let reindex_file_ids = existing_reindex_file_ids
                .unwrap_or_else(|| self.expand_reindex_file_ids(vec![file_id]));
            self.compilation.remove_index(reindex_file_ids.clone());

            let update_file_ids = reindex_file_ids
                .iter()
                .copied()
                .filter(|id| !is_removed || *id != file_id)
                .collect::<Vec<_>>();
            if !update_file_ids.is_empty() {
                self.compilation.update_index(update_file_ids);
            }
            self.compilation
                .get_db_mut()
                .get_call_site_param_index_mut()
                .refresh_file_source_dependencies(file_id);
            self.reindex_changed_inferred_guard_references(
                &reindex_file_ids.iter().copied().collect(),
                &old_guard_facts,
                &reindex_file_ids,
                &incremental_source_file_ids,
            );
            self.reindex_changed_inferred_param_consumers(&old_guard_facts, &reindex_file_ids);
            self.apply_table_remap(old_maps, &[file_id]);
            for reindexed in &reindex_file_ids {
                self.pending_export_fingerprints.remove(reindexed);
            }
        }

        Some(file_id)
    }

    pub fn update_file_preparsed_deferred(
        &mut self,
        uri: Uri,
        text: Option<String>,
        tree: LuaSyntaxTree,
        line_index: LineIndex,
        version: Option<i32>,
    ) -> Option<(FileId, DeferredVfsDrop)> {
        let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(&uri);
        if let Some(file_id) = existing_file_id {
            if let (Some(incoming_version), Some(current_version)) = (
                version,
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_file_version(&file_id),
            ) && incoming_version < current_version
            {
                return None;
            }

            if let (Some(new_text), Some(old_text)) = (
                text.as_deref(),
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_file_content(&file_id)
                    .map(String::as_str),
            ) && old_text == new_text
            {
                self.compilation
                    .get_db_mut()
                    .get_vfs_mut()
                    .update_file_version(&file_id, version);
                return Some((file_id, DeferredVfsDrop::default()));
            }
        } else if text.is_none() {
            return None;
        }

        if let Some(fid) = existing_file_id {
            self.stash_pre_edit_state(fid);
        }

        self.compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content_preparsed_deferred(&uri, text, tree, line_index, version)
    }

    /// VFS-only update: parse and store the new text without touching the index.
    pub fn update_file_text_only(&mut self, uri: &Uri, text: String) -> Option<FileId> {
        let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(uri);
        if let Some(file_id) = existing_file_id {
            if let Some(old_text) = self
                .compilation
                .get_db()
                .get_vfs()
                .get_file_content(&file_id)
                .map(String::as_str)
            {
                if old_text == text.as_str() {
                    return Some(file_id);
                }
            }
            self.stash_pre_edit_state(file_id);
        }

        let file_id = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content(uri, Some(text));

        Some(file_id)
    }

    /// See implementation.
    pub fn reindex_files(&mut self, file_ids: Vec<FileId>) {
        let expansion = self.expand_reindex_file_ids(file_ids.clone());
        self.reindex_expanded_files(file_ids, expansion);
    }

    /// See implementation.
    pub fn reindex_expanded_files(&mut self, file_ids: Vec<FileId>, expansion: Vec<FileId>) {
        self.reindex_expanded_files_inner(file_ids, expansion, None);
    }

    pub(crate) fn reindex_expanded_files_with_old_snapshot(
        &mut self,
        file_ids: Vec<FileId>,
        expansion: Vec<FileId>,
        old_snapshot: InferredGuardSnapshot,
    ) {
        self.reindex_expanded_files_inner(file_ids, expansion, Some(old_snapshot));
    }

    /// Re-analyses `expansion` with `file_ids` as the files that changed.
    fn reindex_expanded_files_inner(
        &mut self,
        file_ids: Vec<FileId>,
        expansion: Vec<FileId>,
        old_snapshot: Option<InferredGuardSnapshot>,
    ) {
        let incremental_source_file_ids = file_ids.iter().copied().collect::<HashSet<_>>();
        let removed_file_ids = file_ids
            .iter()
            .copied()
            .filter(|file_id| {
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_syntax_tree(file_id)
                    .is_none()
            })
            .collect::<HashSet<_>>();

        let mut file_ids = expansion;
        self.add_vgui_forwarding_removal_seed(&removed_file_ids, &mut file_ids);
        let guard_fact_file_ids = file_ids.iter().copied().collect::<HashSet<_>>();
        // A self-index may already have overwritten the guard facts this
        let old_guard_facts = old_snapshot
            .or_else(|| self.pending_guard_snapshot.take())
            .unwrap_or_else(|| self.inferred_guard_snapshot(&guard_fact_file_ids));
        self.compilation.remove_index(file_ids.clone());
        let update_file_ids = file_ids
            .iter()
            .copied()
            .filter(|file_id| !removed_file_ids.contains(file_id))
            .collect::<Vec<_>>();
        if !update_file_ids.is_empty() {
            self.compilation.update_index(update_file_ids.clone());
            self.stabilize_cross_file_type_caches(&update_file_ids);
        }
        for file_id in &file_ids {
            self.pending_export_fingerprints.remove(file_id);
        }
        for file_id in &incremental_source_file_ids {
            self.compilation
                .get_db_mut()
                .get_call_site_param_index_mut()
                .refresh_file_source_dependencies(*file_id);
        }
        self.reindex_changed_inferred_guard_references(
            &guard_fact_file_ids,
            &old_guard_facts,
            &file_ids,
            &incremental_source_file_ids,
        );
        self.reindex_changed_inferred_param_consumers(&old_guard_facts, &file_ids);
    }

    /// Records the anchors the index's stored `Element` ranges correspond to,
    fn stash_pre_edit_anchors(&mut self, file_id: FileId) {
        if self.pending_table_ranges.contains_key(&file_id) {
            return;
        }
        let anchored = collect_anchored_map(self.compilation.get_db(), file_id);
        if !anchored.is_empty() {
            self.pending_table_ranges.insert(file_id, anchored);
        }
    }

    /// See implementation.
    fn stash_pre_edit_state(&mut self, file_id: FileId) {
        self.stash_pre_edit_anchors(file_id);
        // Independent of the anchors: a file with no table literals stashes no
        // anchors, and one stash must not suppress the other.
        if !self.pending_export_fingerprints.contains_key(&file_id) {
            let fingerprint = file_export_fingerprint(self.compilation.get_db(), file_id);
            self.pending_export_fingerprints
                .insert(file_id, fingerprint);
        }
    }

    /// See implementation.
    fn take_pre_edit_fingerprint(&mut self, file_id: FileId) -> u64 {
        self.pending_export_fingerprints
            .remove(&file_id)
            .unwrap_or_else(|| file_export_fingerprint(self.compilation.get_db(), file_id))
    }

    /// The anchor map the index's stored `Element` ranges correspond to.
    fn take_old_anchor_maps(&mut self, file_ids: &[FileId]) -> AnchorMaps {
        let mut old_maps = AnchorMaps::default();
        for fid in file_ids {
            if let Some(pending) = self.pending_table_ranges.remove(fid) {
                old_maps.insert(*fid, pending);
            } else {
                let m = collect_anchored_map(self.compilation.get_db(), *fid);
                if !m.is_empty() {
                    old_maps.insert(*fid, m);
                }
            }
        }
        old_maps
    }

    /// See implementation.
    fn apply_table_remap(&mut self, mut old_maps: AnchorMaps, file_ids: &[FileId]) {
        let mut global_remap: rustc_hash::FxHashMap<
            InFiled<rowan::TextRange>,
            InFiled<rowan::TextRange>,
        > = rustc_hash::FxHashMap::default();
        let mut deleted: Vec<InFiled<rowan::TextRange>> = Vec::new();
        for fid in file_ids.iter().copied() {
            let old_map = old_maps.remove(&fid).unwrap_or_default();
            // A file with no tree has been removed. Only then does an anchor
            let file_removed = self
                .compilation
                .get_db()
                .get_vfs()
                .get_syntax_tree(&fid)
                .is_none();
            if old_map.is_empty() && !file_removed {
                // Nothing stashed and the file is still there, so there is no
                continue;
            }
            if file_removed {
                // Every literal in it is gone, not just the ones an anchor
                deleted.extend(
                    self.compilation
                        .get_db()
                        .get_member_index()
                        .element_owner_ranges_in_file(fid),
                );
            }
            let new_map = collect_anchored_map(self.compilation.get_db(), fid);
            for (anchor, old_range) in old_map {
                match new_map.get(&anchor) {
                    Some(new_range) if &old_range != new_range => {
                        global_remap.insert(old_range, new_range.clone());
                    }
                    Some(_) => {}
                    None if file_removed => deleted.push(old_range),
                    None => {}
                }
            }
        }

        if !global_remap.is_empty() {
            let db = self.compilation.get_db_mut();
            db.get_member_index_mut().remap_elements(&global_remap);
            db.get_type_index_mut().remap_table_const(&global_remap);
            // Beyond the type cache and the member owner, the one store that
            db.get_dynamic_field_index_mut()
                .remap_table_ranges(&global_remap);
        }
        if !deleted.is_empty() {
            let db = self.compilation.get_db_mut();
            let forgotten = db
                .get_member_index_mut()
                .remove_deleted_element_owners(&deleted);
            // Their cached types would otherwise outlive them: the files those
            // members belong to are not the ones being re-analysed.
            db.get_type_index_mut()
                .remove_member_type_caches(&forgotten);
        }
    }

    /// Rebuilds only these files' own index entries.
    pub fn self_index_files(&mut self, file_ids: Vec<FileId>) {
        let old_maps = self.take_old_anchor_maps(&file_ids);
        self.compilation.remove_index(file_ids.clone());
        self.compilation.update_index(file_ids.clone());
        self.apply_table_remap(old_maps, &file_ids);
    }

    pub fn self_index_files_and_get_ripple_with_changed(
        &mut self,
        file_ids: Vec<FileId>,
    ) -> (Vec<FileId>, Vec<FileId>) {
        let has_special = file_ids.iter().any(|fid| {
            let db = self.compilation.get_db();
            !db.get_signature_index()
                .inferred_guard_facts_for_files(&HashSet::from([*fid]))
                .is_empty()
                || db
                    .get_gmod_class_metadata_index()
                    .has_annotated_vgui_parent_calls(*fid)
        });
        if has_special {
            let expansion = self.expand_reindex_file_ids(file_ids.clone());
            let snapshot =
                self.inferred_guard_snapshot(&expansion.iter().copied().collect::<HashSet<_>>());
            self.pending_guard_snapshot.get_or_insert(snapshot);
            self.self_index_files(file_ids.clone());
            return (file_ids, expansion);
        }

        // The text is already written by the time the editor path reaches
        let mut before_fps = HashMap::new();
        for fid in &file_ids {
            before_fps.insert(*fid, self.take_pre_edit_fingerprint(*fid));
        }
        // Expansion must be captured before self_index, or dependents that
        // reference the old exports are missed.
        let before_expansion = self.expand_reindex_file_ids(file_ids.clone());
        // The oldest snapshot in a burst is the one the ripple has to diff
        let snapshot =
            self.inferred_guard_snapshot(&before_expansion.iter().copied().collect::<HashSet<_>>());
        self.pending_guard_snapshot.get_or_insert(snapshot);
        self.self_index_files(file_ids.clone());
        self.stabilize_cross_file_type_caches(&file_ids);
        let mut changed = Vec::new();
        for fid in &file_ids {
            let after = file_export_fingerprint(self.compilation.get_db(), *fid);
            if before_fps.get(fid) != Some(&after) {
                changed.push(*fid);
            }
        }
        if changed.is_empty() {
            return (Vec::new(), Vec::new());
        }
        // The before expansion already contains the dependents of the changed
        (changed, before_expansion)
    }

    /// Re-analyses exactly `file_ids`, skipping dependency expansion.
    pub fn reindex_files_without_expansion(&mut self, file_ids: Vec<FileId>) {
        self.compilation.remove_index(file_ids.clone());
        self.compilation.update_index(file_ids.clone());
        self.stabilize_cross_file_type_caches(&file_ids);
    }

    pub fn expand_reindex_file_ids(&self, file_ids: Vec<FileId>) -> Vec<FileId> {
        let _p = Profile::new("expand_reindex_file_ids");
        let mut expanded = file_ids.into_iter().collect::<HashSet<_>>();
        loop {
            // Include/require callers must be rebuilt with their changed target.
            // Traverse the indexed dependency graph; never rescan workspace ASTs.
            let dependency_dependents = self
                .compilation
                .get_db()
                .get_file_dependencies_index()
                .get_file_dependencies()
                .collect_file_dependents(expanded.iter().copied().collect());
            let unresolved_path_dependents = self.unresolved_path_dependency_dependents(&expanded);
            let dependent_files = self
                .compilation
                .get_db()
                .get_type_index()
                .files_with_type_caches_referencing_files(&expanded);
            let inference_dependents = self
                .compilation
                .get_db()
                .get_type_index()
                .files_depending_on_inference_support(&expanded);
            let callback_dependents = self
                .compilation
                .get_db()
                .get_call_site_param_index()
                .collect_source_dependents(&expanded);
            let callback_source_paths = expanded
                .iter()
                .filter_map(|file_id| self.compilation.get_db().get_vfs().get_file_path(file_id))
                .collect::<Vec<_>>();
            let callback_path_dependents = self
                .compilation
                .get_db()
                .get_call_site_param_index()
                .collect_source_path_dependents(callback_source_paths);
            let mut added = false;
            for file_id in dependency_dependents
                .into_iter()
                .chain(unresolved_path_dependents)
                .chain(dependent_files)
                .chain(inference_dependents)
                .chain(callback_dependents)
                .chain(callback_path_dependents)
            {
                added |= expanded.insert(file_id);
            }

            if !added {
                break;
            }
        }

        let mut expanded = expanded.into_iter().collect::<Vec<_>>();
        expanded.sort_unstable();
        expanded
    }

    fn add_vgui_forwarding_removal_seed(
        &self,
        removed_file_ids: &HashSet<FileId>,
        reindex_file_ids: &mut Vec<FileId>,
    ) {
        if removed_file_ids.is_empty() {
            return;
        }
        let db = self.compilation.get_db();
        let vfs = db.get_vfs();
        let module_index = db.get_module_index();
        let gmod_index = db.get_gmod_class_metadata_index();
        let affected_workspace_id = reindex_file_ids
            .iter()
            .filter(|file_id| removed_file_ids.contains(file_id))
            .find_map(|file_id| {
                gmod_index
                    .has_annotated_vgui_parent_calls(*file_id)
                    .then(|| module_index.get_workspace_id(*file_id))
                    .flatten()
            });
        let Some(affected_workspace_id) = affected_workspace_id else {
            return;
        };
        if reindex_file_ids.iter().any(|file_id| {
            !removed_file_ids.contains(file_id) && vfs.get_syntax_tree(file_id).is_some()
        }) {
            return;
        }

        let all_file_ids = vfs.get_all_file_ids();
        let seed_file_id = all_file_ids
            .iter()
            .copied()
            .filter(|file_id| {
                !removed_file_ids.contains(file_id) && vfs.get_syntax_tree(file_id).is_some()
            })
            .find(|file_id| module_index.get_workspace_id(*file_id) == Some(affected_workspace_id))
            .or_else(|| {
                all_file_ids.iter().copied().find(|file_id| {
                    !removed_file_ids.contains(file_id) && vfs.get_syntax_tree(file_id).is_some()
                })
            });
        let Some(seed_file_id) = seed_file_id else {
            return;
        };
        reindex_file_ids.push(seed_file_id);
        reindex_file_ids.sort_unstable();
        reindex_file_ids.dedup();
    }

    fn unresolved_path_dependency_dependents(&self, file_ids: &HashSet<FileId>) -> Vec<FileId> {
        let db = self.compilation.get_db();
        let target_path_keys = file_ids
            .iter()
            .filter_map(|file_id| db.get_vfs().get_file_path(file_id).cloned())
            .flat_map(|target_path| dependency_path_keys_for_target(db, &target_path))
            .collect::<HashSet<_>>();

        db.get_file_dependencies_index()
            .collect_unresolved_path_dependents(target_path_keys)
    }

    fn reindex_changed_inferred_guard_references(
        &mut self,
        source_file_ids: &HashSet<FileId>,
        old_snapshot: &InferredGuardSnapshot,
        already_reindexed: &[FileId],
        incremental_source_file_ids: &HashSet<FileId>,
    ) {
        #[cfg(test)]
        let initial_stabilization_invocations = self.cross_file_stabilization_invocations;
        let profile_enabled = std::env::var_os("GLUALS_PROFILE").is_some();
        let mut profile_changed_facts = 0usize;
        let mut profile_reference_edges = 0usize;
        let mut profile_waves = 0usize;
        let mut profile_reindexed_files = 0usize;
        let mut propagation_reindexed_files = source_file_ids
            .iter()
            .copied()
            .chain(already_reindexed.iter().copied())
            .collect::<HashSet<_>>();
        let mut new_facts = self
            .compilation
            .get_db()
            .get_signature_index()
            .inferred_guard_facts_for_files(source_file_ids);
        let equivalent_owners = self.reconcile_equivalent_inferred_guard_owners(
            old_snapshot,
            &new_facts,
            &propagation_reindexed_files,
        );
        let old_facts = &old_snapshot.facts;
        let mut changed_owners = old_facts
            .keys()
            .chain(new_facts.keys())
            .filter(|owner| {
                !equivalent_owners.contains(*owner)
                    && old_facts.get(*owner) != new_facts.get(*owner)
            })
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if changed_owners.is_empty() {
            #[cfg(test)]
            {
                self.inferred_guard_propagation_stats = InferredGuardPropagationStats::default();
            }
            if profile_enabled {
                eprintln!(
                    "[profile] inferred_guard_incremental changed_facts=0 reference_edges=0 waves=0 reindexed_files=0"
                );
            }
            return;
        }
        profile_changed_facts += changed_owners.len();
        sort_inferred_guard_owners(&mut changed_owners);
        let mut frontier_old_facts = old_snapshot.facts.clone();
        let mut frontier_old_consumers = old_snapshot.consumers.clone();

        while !changed_owners.is_empty() {
            let mut reference_files = HashSet::new();
            for owner in &changed_owners {
                let newly_added =
                    !frontier_old_facts.contains_key(owner) && new_facts.contains_key(owner);
                let old_consumers = frontier_old_consumers
                    .get(owner)
                    .into_iter()
                    .flatten()
                    .copied();
                let current_consumers = self
                    .compilation
                    .get_db()
                    .get_signature_index()
                    .inferred_guard_consumers(owner);
                for file_id in old_consumers.chain(current_consumers) {
                    if !propagation_reindexed_files.contains(&file_id) {
                        profile_reference_edges += 1;
                        reference_files.insert(file_id);
                    }
                }
                if newly_added {
                    let allow_alias_retry =
                        incremental_source_file_ids.contains(&owner.source_file_id());
                    let discovered = self.resolve_inferred_guard_reference_files(owner, true);
                    for file_id in discovered.files {
                        let alias_retry = allow_alias_retry
                            && discovered.alias_calls.contains(&file_id)
                            && file_id != owner.source_file_id();
                        if !propagation_reindexed_files.contains(&file_id) || alias_retry {
                            profile_reference_edges += 1;
                            reference_files.insert(file_id);
                        }
                    }
                }
            }
            if reference_files.is_empty() {
                break;
            }

            let mut reindex_file_ids = reference_files.into_iter().collect::<Vec<_>>();
            reindex_file_ids.sort_unstable();
            let wave_file_ids = reindex_file_ids.iter().copied().collect::<HashSet<_>>();
            let old_wave_snapshot = self.inferred_guard_snapshot(&wave_file_ids);
            self.compilation.remove_index(reindex_file_ids.clone());
            let update_file_ids = reindex_file_ids
                .into_iter()
                .filter(|file_id| {
                    self.compilation
                        .get_db()
                        .get_vfs()
                        .get_syntax_tree(file_id)
                        .is_some()
                })
                .collect::<Vec<_>>();
            if update_file_ids.is_empty() {
                break;
            }
            profile_waves += 1;
            profile_reindexed_files += update_file_ids.len();
            propagation_reindexed_files.extend(wave_file_ids.iter().copied());
            self.compilation.update_index(update_file_ids.clone());

            new_facts = self
                .compilation
                .get_db()
                .get_signature_index()
                .inferred_guard_facts_for_files(&wave_file_ids);
            let equivalent_owners = self.reconcile_equivalent_inferred_guard_owners(
                &old_wave_snapshot,
                &new_facts,
                &propagation_reindexed_files,
            );
            changed_owners = old_wave_snapshot
                .facts
                .keys()
                .chain(new_facts.keys())
                .filter(|owner| {
                    !equivalent_owners.contains(*owner)
                        && old_wave_snapshot.facts.get(*owner) != new_facts.get(*owner)
                })
                .cloned()
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            frontier_old_facts = old_wave_snapshot.facts;
            frontier_old_consumers = old_wave_snapshot.consumers;
            profile_changed_facts += changed_owners.len();
            sort_inferred_guard_owners(&mut changed_owners);
        }
        if profile_enabled {
            eprintln!(
                "[profile] inferred_guard_incremental changed_facts={} reference_edges={} waves={} reindexed_files={}",
                profile_changed_facts,
                profile_reference_edges,
                profile_waves,
                profile_reindexed_files
            );
        }
        #[cfg(test)]
        {
            self.inferred_guard_propagation_stats = InferredGuardPropagationStats {
                changed_facts: profile_changed_facts,
                reference_edges: profile_reference_edges,
                frontiers: profile_waves,
                reindexed_files: profile_reindexed_files,
                broad_stabilizations: self
                    .cross_file_stabilization_invocations
                    .saturating_sub(initial_stabilization_invocations),
            };
        }
    }

    fn inferred_guard_snapshot(&self, file_ids: &HashSet<FileId>) -> InferredGuardSnapshot {
        let signature_index = self.compilation.get_db().get_signature_index();
        let facts = signature_index.inferred_guard_facts_for_files(file_ids);
        let consumers = facts
            .keys()
            .map(|owner| {
                (
                    owner.clone(),
                    signature_index.inferred_guard_consumers(owner).collect(),
                )
            })
            .collect();
        let inferred_params = self
            .compilation
            .get_db()
            .get_call_site_param_index()
            .inferred_params_for_contributor_files(file_ids);
        InferredGuardSnapshot {
            facts,
            consumers,
            inferred_params,
            snapshot_file_ids: file_ids.clone(),
        }
    }

    /// Re-analyses callee files whose call-site-inferred parameter types
    /// changed.
    fn reindex_changed_inferred_param_consumers(
        &mut self,
        old_snapshot: &InferredGuardSnapshot,
        already_reindexed: &[FileId],
    ) {
        let new_params = self
            .compilation
            .get_db()
            .get_call_site_param_index()
            .inferred_params_for_contributor_files(&old_snapshot.snapshot_file_ids);
        let old_params = &old_snapshot.inferred_params;
        if old_params.is_empty() && new_params.is_empty() {
            return;
        }

        let already_reindexed = already_reindexed
            .iter()
            .copied()
            .chain(old_snapshot.snapshot_file_ids.iter().copied())
            .collect::<HashSet<_>>();
        let mut changed_files = old_params
            .keys()
            .chain(new_params.keys())
            .filter(|key| old_params.get(*key) != new_params.get(*key))
            .map(|(signature_id, _)| signature_id.get_file_id())
            .filter(|file_id| !already_reindexed.contains(file_id))
            .collect::<Vec<_>>();
        changed_files.sort_unstable();
        changed_files.dedup();
        if changed_files.is_empty() {
            return;
        }

        let expanded = self.expand_reindex_file_ids(changed_files);
        let expanded = expanded
            .into_iter()
            .filter(|file_id| {
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_syntax_tree(file_id)
                    .is_some()
            })
            .collect::<Vec<_>>();
        if expanded.is_empty() {
            return;
        }
        self.compilation.remove_index(expanded.clone());
        self.compilation.update_index(expanded);
    }

    fn reconcile_equivalent_inferred_guard_owners(
        &mut self,
        old_snapshot: &InferredGuardSnapshot,
        new_facts: &HashMap<LuaInferredGuardOwner, LuaInferredPositiveGuard>,
        reindexed_file_ids: &HashSet<FileId>,
    ) -> HashSet<LuaInferredGuardOwner> {
        let mut reconciled = HashSet::new();
        for owner in old_snapshot
            .facts
            .keys()
            .filter(|owner| old_snapshot.facts.get(*owner) == new_facts.get(*owner))
        {
            if let Some(consumers) = old_snapshot.consumers.get(owner) {
                self.compilation
                    .get_db_mut()
                    .get_signature_index_mut()
                    .migrate_inferred_guard_consumers(owner.clone(), consumers, reindexed_file_ids);
            }
            reconciled.insert(owner.clone());
        }

        let mut old_owners = old_snapshot
            .facts
            .keys()
            .filter(|owner| !new_facts.contains_key(*owner))
            .cloned()
            .collect::<Vec<_>>();
        let mut new_owners = new_facts
            .keys()
            .filter(|owner| !old_snapshot.facts.contains_key(*owner))
            .cloned()
            .collect::<Vec<_>>();
        sort_inferred_guard_owners(&mut old_owners);
        sort_inferred_guard_owners(&mut new_owners);

        for old_owner in old_owners {
            let Some(new_idx) = new_owners.iter().position(|new_owner| {
                old_owner.source_file_id() == new_owner.source_file_id()
                    && old_owner.path() == new_owner.path()
                    && old_owner.state_mask() == new_owner.state_mask()
                    && old_snapshot.facts.get(&old_owner) == new_facts.get(new_owner)
            }) else {
                continue;
            };
            let new_owner = new_owners.remove(new_idx);
            if let Some(consumers) = old_snapshot.consumers.get(&old_owner) {
                self.compilation
                    .get_db_mut()
                    .get_signature_index_mut()
                    .migrate_inferred_guard_consumers(
                        new_owner.clone(),
                        consumers,
                        reindexed_file_ids,
                    );
            }
            reconciled.insert(old_owner);
            reconciled.insert(new_owner);
        }
        reconciled
    }

    fn resolve_inferred_guard_reference_files(
        &self,
        owner: &LuaInferredGuardOwner,
        discover_aliases: bool,
    ) -> InferredGuardReferenceFiles {
        let Some(member_name) = owner.path().last() else {
            return InferredGuardReferenceFiles::default();
        };
        let references = if owner.path().len() == 1 {
            self.compilation
                .get_db()
                .get_reference_index()
                .get_global_references(member_name)
        } else {
            self.compilation
                .get_db()
                .get_reference_index()
                .get_index_references(&LuaMemberKey::Name(member_name.clone()))
        };
        let Some(references) = references else {
            return InferredGuardReferenceFiles::default();
        };

        let db = self.compilation.get_db();
        let mut caches = HashMap::<FileId, LuaInferCache>::new();
        let mut matching_references = references
            .into_iter()
            .filter_map(|reference| {
                let root = db
                    .get_vfs()
                    .get_syntax_tree(&reference.file_id)?
                    .get_red_root();
                let expr = LuaExpr::cast(reference.value.to_node_from_root(&root)?)?;
                (global_path_for_expr(&expr).as_deref() == Some(owner.path())
                    && db.get_gmod_infer_index().are_offsets_compatible(
                        &reference.file_id,
                        expr.get_range().start(),
                        &owner.source_file_id(),
                        owner.signature_id().get_position(),
                    ))
                .then_some((reference.file_id, expr))
            })
            .collect::<Vec<_>>();
        matching_references.sort_by_key(|(file_id, expr)| (*file_id, expr.get_range().start()));

        let mut result = InferredGuardReferenceFiles::default();
        let mut alias_queue = VecDeque::new();
        let mut visited_aliases = HashSet::new();
        for (file_id, expr) in matching_references {
            if call_resolves_to_inferred_guard_owner(db, &mut caches, owner, file_id, &expr) {
                result.files.insert(file_id);
            }
            if discover_aliases
                && expr_resolves_to_inferred_guard_owner(db, &mut caches, owner, file_id, &expr)
                && let Some(decl_id) = immutable_local_alias_decl(db, file_id, &expr)
            {
                alias_queue.push_back(decl_id);
            }
        }

        while let Some(decl_id) = alias_queue.pop_front() {
            if !visited_aliases.insert(decl_id) {
                continue;
            }
            let Some(root) = db
                .get_vfs()
                .get_syntax_tree(&decl_id.file_id)
                .map(|tree| tree.get_red_root())
            else {
                continue;
            };
            let Some(decl_references) = db
                .get_reference_index()
                .get_decl_references(&decl_id.file_id, &decl_id)
            else {
                continue;
            };
            let mut cells = decl_references.cells.clone();
            cells.sort_by_key(|cell| cell.range.start());
            for cell in cells {
                if cell.is_write {
                    continue;
                }
                let Some(name_expr) = root
                    .covering_element(cell.range)
                    .ancestors()
                    .find_map(LuaNameExpr::cast)
                    .filter(|name_expr| name_expr.get_range() == cell.range)
                else {
                    continue;
                };
                let expr = LuaExpr::NameExpr(name_expr);
                if !db.get_gmod_infer_index().are_offsets_compatible(
                    &decl_id.file_id,
                    expr.get_range().start(),
                    &owner.source_file_id(),
                    owner.signature_id().get_position(),
                ) {
                    continue;
                }
                if is_call_prefix(&expr) {
                    result.files.insert(decl_id.file_id);
                    result.alias_calls.insert(decl_id.file_id);
                }
                if let Some(next_decl_id) = immutable_local_alias_decl(db, decl_id.file_id, &expr) {
                    alias_queue.push_back(next_decl_id);
                }
            }
        }

        result
    }

    fn stabilize_cross_file_type_caches(&mut self, file_ids: &[FileId]) {
        #[cfg(test)]
        {
            self.cross_file_stabilization_invocations += 1;
        }
        if file_ids.is_empty() {
            return;
        }

        let changed = file_ids.iter().copied().collect::<HashSet<_>>();
        let all_dependents = self
            .compilation
            .get_db()
            .get_type_index()
            .files_with_cross_file_type_caches_referencing_files(&changed);
        let dependents = select_cross_file_stabilization_dependents(all_dependents, &changed)
            .into_iter()
            .filter(|file_id| {
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_syntax_tree(file_id)
                    .is_some()
            })
            .collect::<Vec<_>>();
        if dependents.is_empty() {
            return;
        }

        self.compilation.remove_index(dependents.clone());
        self.compilation.update_index(dependents);
    }

    pub fn update_remote_file_by_uri(&mut self, uri: &Uri, text: Option<String>) -> FileId {
        let is_removed = text.is_none();
        let fid = self
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_remote_file_content(uri, text);

        let removed_file_ids = is_removed
            .then_some(fid)
            .into_iter()
            .collect::<HashSet<_>>();
        let mut reindex_file_ids = vec![fid];
        self.add_vgui_forwarding_removal_seed(&removed_file_ids, &mut reindex_file_ids);
        self.compilation.remove_index(reindex_file_ids.clone());
        let update_file_ids = reindex_file_ids
            .into_iter()
            .filter(|file_id| !removed_file_ids.contains(file_id))
            .collect::<Vec<_>>();
        if !update_file_ids.is_empty() {
            self.compilation.update_index(update_file_ids);
        }
        fid
    }

    pub fn update_file_by_path(&mut self, path: &PathBuf, text: Option<String>) -> Option<FileId> {
        let uri = file_path_to_uri(path)?;
        self.update_file_by_uri(&uri, text)
    }

    pub fn update_files_by_uri(&mut self, files: Vec<(Uri, Option<String>)>) -> Vec<FileId> {
        let mut removed_files = HashSet::new();
        let mut updated_files = HashSet::new();
        let mut files = files;
        files.sort_by_cached_key(|(uri, _)| {
            uri_to_file_path(uri)
                .map(|path| crate::vfs::normalize_path_for_ordering(&path.to_string_lossy()))
                .unwrap_or_else(|| uri.as_str().to_string())
        });
        let old_source_file_ids = files
            .iter()
            .filter_map(|(uri, _)| self.compilation.get_db().get_vfs().get_file_id(uri))
            .collect::<HashSet<_>>();
        let removed_source_file_ids = files
            .iter()
            .filter(|(_, text)| text.is_none())
            .filter_map(|(uri, _)| self.compilation.get_db().get_vfs().get_file_id(uri))
            .collect::<HashSet<_>>();
        // Taken before the writes below, as on every other edit path: the
        let mut remap_source_file_ids: Vec<FileId> = old_source_file_ids.iter().copied().collect();
        remap_source_file_ids.sort_unstable();
        let old_anchor_maps = self.take_old_anchor_maps(&remap_source_file_ids);
        let mut old_guard_fact_file_ids =
            self.expand_reindex_file_ids(old_source_file_ids.iter().copied().collect());
        self.add_vgui_forwarding_removal_seed(
            &removed_source_file_ids,
            &mut old_guard_fact_file_ids,
        );
        let old_guard_fact_file_ids = old_guard_fact_file_ids.into_iter().collect::<HashSet<_>>();
        let old_guard_facts = self.inferred_guard_snapshot(&old_guard_fact_file_ids);

        // Separate files into: unchanged (skip), to-remove, and to-parse
        let mut to_parse: Vec<(Uri, String)> = Vec::new();
        {
            let _p = Profile::new("update files: classify");
            for (uri, text) in files {
                let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(&uri);
                if let Some(file_id) = existing_file_id {
                    if let (Some(new_text), Some(old_text)) = (
                        text.as_deref(),
                        self.compilation
                            .get_db()
                            .get_vfs()
                            .get_file_content(&file_id)
                            .map(String::as_str),
                    ) && old_text == new_text
                    {
                        removed_files.insert(file_id);
                        updated_files.insert(file_id);
                        continue;
                    }
                } else if text.is_none() {
                    continue;
                }

                if let Some(text) = text {
                    to_parse.push((uri, text));
                } else {
                    // File removal: assign ID and mark for removal
                    let file_id = self
                        .compilation
                        .get_db_mut()
                        .get_vfs_mut()
                        .set_file_content(&uri, None);
                    removed_files.insert(file_id);
                }
            }
        }

        // Parse files — parallel when enough files to benefit
        const PARALLEL_THRESHOLD: usize = 50;
        {
            let _p = Profile::new("update files: parse");
            if to_parse.len() >= PARALLEL_THRESHOLD {
                // Pre-assign file IDs (sequential, fast)
                let file_ids: Vec<FileId> = to_parse
                    .iter()
                    .map(|(uri, _)| self.compilation.get_db_mut().get_vfs_mut().file_id(uri))
                    .collect();

                // Parse in parallel
                let config = self.emmyrc.clone();
                let n_threads = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
                    .min(16);
                let next_idx = std::sync::atomic::AtomicUsize::new(0);

                // Each slot stores the parsed result
                let parsed: Vec<std::sync::Mutex<Option<(LuaSyntaxTree, LineIndex)>>> = (0
                    ..to_parse.len())
                    .map(|_| std::sync::Mutex::new(None))
                    .collect();

                std::thread::scope(|s| {
                    for _ in 0..n_threads {
                        let next = &next_idx;
                        let files = &to_parse;
                        let results = &parsed;
                        let cfg = &config;
                        s.spawn(move || {
                            let mut node_cache = rowan::NodeCache::default();
                            loop {
                                let idx = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if idx >= files.len() {
                                    break;
                                }
                                let (_, text) = &files[idx];
                                let parse_config = cfg.get_parse_config(&mut node_cache);
                                let tree = LuaParser::parse(text, parse_config);
                                let line_index = LineIndex::parse(text);
                                *results[idx].lock().expect("mutex poisoned") =
                                    Some((tree, line_index));
                            }
                        });
                    }
                });

                // Insert pre-parsed results (sequential, fast HashMap inserts)
                let vfs = self.compilation.get_db_mut().get_vfs_mut();
                for (i, ((_uri, text), file_id)) in
                    to_parse.into_iter().zip(file_ids.iter()).enumerate()
                {
                    let (tree, line_index) = parsed[i]
                        .lock()
                        .expect("mutex poisoned")
                        .take()
                        .expect("parsed result missing");
                    vfs.insert_preparsed(*file_id, text, tree, line_index);
                    removed_files.insert(*file_id);
                    updated_files.insert(*file_id);
                }
            } else {
                for (uri, text) in to_parse {
                    let file_id = self
                        .compilation
                        .get_db_mut()
                        .get_vfs_mut()
                        .set_file_content(&uri, Some(text));
                    removed_files.insert(file_id);
                    updated_files.insert(file_id);
                }
            }
        }

        if removed_files.is_empty() {
            return Vec::new();
        }

        let mut removed_files = self.expand_reindex_file_ids(removed_files.into_iter().collect());
        self.add_vgui_forwarding_removal_seed(&removed_source_file_ids, &mut removed_files);
        let guard_fact_file_ids = removed_files.iter().copied().collect::<HashSet<_>>();
        self.compilation.remove_index(removed_files.clone());
        updated_files.extend(removed_files.into_iter().filter(|file_id| {
            self.compilation
                .get_db()
                .get_vfs()
                .get_syntax_tree(file_id)
                .is_some()
        }));
        let mut updated_files: Vec<FileId> = updated_files.into_iter().collect();
        updated_files.sort();
        self.compilation.update_index(updated_files.clone());
        {
            let _p = Profile::new("post: stabilize_cross_file_type_caches");
            self.stabilize_cross_file_type_caches(&updated_files);
        }
        self.apply_table_remap(old_anchor_maps, &remap_source_file_ids);
        {
            let _p = Profile::new("post: refresh_file_source_dependencies");
            for file_id in &old_source_file_ids {
                self.compilation
                    .get_db_mut()
                    .get_call_site_param_index_mut()
                    .refresh_file_source_dependencies(*file_id);
            }
        }
        {
            let _p = Profile::new("post: reindex_changed_inferred_guard_references");
            self.reindex_changed_inferred_guard_references(
                &guard_fact_file_ids,
                &old_guard_facts,
                &updated_files,
                &old_source_file_ids,
            );
        }
        {
            let _p = Profile::new("post: reindex_changed_inferred_param_consumers");
            self.reindex_changed_inferred_param_consumers(&old_guard_facts, &updated_files);
        }
        updated_files
    }

    #[allow(unused)]
    pub(crate) fn update_files_by_uri_sorted(
        &mut self,
        files: Vec<(Uri, Option<String>)>,
    ) -> Vec<FileId> {
        let mut files = files;
        files.sort_by_cached_key(|(uri, _)| {
            uri_to_file_path(uri)
                .map(|path| crate::vfs::normalize_path_for_ordering(&path.to_string_lossy()))
                .unwrap_or_else(|| uri.as_str().to_string())
        });
        let old_source_file_ids = files
            .iter()
            .filter_map(|(uri, _)| self.compilation.get_db().get_vfs().get_file_id(uri))
            .collect::<HashSet<_>>();
        let removed_source_file_ids = files
            .iter()
            .filter(|(_, text)| text.is_none())
            .filter_map(|(uri, _)| self.compilation.get_db().get_vfs().get_file_id(uri))
            .collect::<HashSet<_>>();
        let mut old_guard_fact_file_ids =
            self.expand_reindex_file_ids(old_source_file_ids.iter().copied().collect());
        self.add_vgui_forwarding_removal_seed(
            &removed_source_file_ids,
            &mut old_guard_fact_file_ids,
        );
        let old_guard_fact_file_ids = old_guard_fact_file_ids.into_iter().collect::<HashSet<_>>();
        let old_guard_facts = self.inferred_guard_snapshot(&old_guard_fact_file_ids);
        let mut removed_files = HashSet::new();
        let mut updated_files = HashSet::new();
        {
            let _p = Profile::new("update files");
            for (uri, text) in files {
                let existing_file_id = self.compilation.get_db().get_vfs().get_file_id(&uri);
                if let Some(file_id) = existing_file_id {
                    if let (Some(new_text), Some(old_text)) = (
                        text.as_deref(),
                        self.compilation
                            .get_db()
                            .get_vfs()
                            .get_file_content(&file_id)
                            .map(String::as_str),
                    ) && old_text == new_text
                    {
                        removed_files.insert(file_id);
                        updated_files.insert(file_id);
                        continue;
                    }
                } else if text.is_none() {
                    continue;
                }

                let is_new_text = text.is_some();
                let file_id = self
                    .compilation
                    .get_db_mut()
                    .get_vfs_mut()
                    .set_file_content(&uri, text);
                removed_files.insert(file_id);
                if is_new_text {
                    updated_files.insert(file_id);
                }
            }
        }
        if removed_files.is_empty() {
            return Vec::new();
        }

        let mut removed_files = self.expand_reindex_file_ids(removed_files.into_iter().collect());
        self.add_vgui_forwarding_removal_seed(&removed_source_file_ids, &mut removed_files);
        let guard_fact_file_ids = removed_files.iter().copied().collect::<HashSet<_>>();
        self.compilation.remove_index(removed_files.clone());
        updated_files.extend(removed_files.into_iter().filter(|file_id| {
            self.compilation
                .get_db()
                .get_vfs()
                .get_syntax_tree(file_id)
                .is_some()
        }));
        let mut updated_files: Vec<FileId> = updated_files.into_iter().collect();
        updated_files.sort();
        self.compilation.update_index(updated_files.clone());
        self.stabilize_cross_file_type_caches(&updated_files);
        for file_id in &old_source_file_ids {
            self.compilation
                .get_db_mut()
                .get_call_site_param_index_mut()
                .refresh_file_source_dependencies(*file_id);
        }
        self.reindex_changed_inferred_guard_references(
            &guard_fact_file_ids,
            &old_guard_facts,
            &updated_files,
            &old_source_file_ids,
        );
        self.reindex_changed_inferred_param_consumers(&old_guard_facts, &updated_files);
        updated_files
    }

    pub fn remove_file_by_uri(&mut self, uri: &Uri) -> Option<FileId> {
        if let Some(file_id) = self.compilation.get_db().get_vfs().get_file_id(uri) {
            let mut reindex_file_ids = self.expand_reindex_file_ids(vec![file_id]);
            reindex_file_ids.extend(
                self.compilation
                    .get_db()
                    .get_call_site_param_index()
                    .collect_contribution_signature_files(&HashSet::from([file_id])),
            );
            reindex_file_ids.sort_unstable();
            reindex_file_ids.dedup();
            let removed_file_ids = HashSet::from([file_id]);
            self.add_vgui_forwarding_removal_seed(&removed_file_ids, &mut reindex_file_ids);
            let guard_fact_file_ids = reindex_file_ids.iter().copied().collect::<HashSet<_>>();
            let old_guard_facts = self.inferred_guard_snapshot(&guard_fact_file_ids);
            // Members other files own on this file's table literals are filed
            // under *their* file, so `remove_index` never reaches them.
            let old_maps = self.take_old_anchor_maps(&[file_id]);
            self.compilation
                .get_db_mut()
                .get_vfs_mut()
                .remove_file(uri)?;
            log::info!(
                "remove_file_by_uri: uri={} file_id={:?}",
                uri.as_str(),
                file_id
            );
            self.compilation.remove_index(reindex_file_ids.clone());
            let update_file_ids = reindex_file_ids
                .iter()
                .copied()
                .filter(|id| *id != file_id)
                .collect::<Vec<_>>();
            if !update_file_ids.is_empty() {
                self.compilation.update_index(update_file_ids);
            }
            self.compilation
                .get_db_mut()
                .get_call_site_param_index_mut()
                .refresh_file_source_dependencies(file_id);
            self.reindex_changed_inferred_guard_references(
                &guard_fact_file_ids,
                &old_guard_facts,
                &reindex_file_ids,
                &HashSet::new(),
            );
            self.apply_table_remap(old_maps, &[file_id]);
            self.pending_export_fingerprints.remove(&file_id);
            return Some(file_id);
        }

        None
    }

    pub fn update_files_by_path(&mut self, files: Vec<(PathBuf, Option<String>)>) -> Vec<FileId> {
        let files = files
            .into_iter()
            .filter_map(|(path, text)| {
                let uri = file_path_to_uri(&path)?;
                Some((uri, text))
            })
            .collect();
        self.update_files_by_uri(files)
    }

    pub fn update_config(&mut self, config: Arc<Emmyrc>) {
        let mut refreshed_config = (*config).clone();
        refreshed_config
            .gmod
            .scripted_class_scopes
            .refresh_resolved_definitions();
        let config = Arc::new(refreshed_config);
        self.emmyrc = config.clone();
        self.compilation.update_config(config.clone());
        self.diagnostic.update_config(config);
    }

    pub fn set_workspace_diagnostic_configs(
        &mut self,
        configs: HashMap<WorkspaceId, Arc<LuaDiagnosticConfig>>,
    ) {
        self.diagnostic.set_workspace_configs(configs);
    }

    pub fn get_workspace_id_for_root(&self, root: &Path) -> Option<WorkspaceId> {
        self.compilation
            .get_db()
            .get_module_index()
            .get_workspace_id_for_root(root)
    }

    pub fn get_emmyrc(&self) -> Arc<Emmyrc> {
        self.emmyrc.clone()
    }

    pub fn diagnose_file(
        &self,
        file_id: FileId,
        cancel_token: CancellationToken,
    ) -> Option<Vec<lsp_types::Diagnostic>> {
        self.diagnostic
            .diagnose_file(&self.compilation, file_id, cancel_token)
    }

    pub fn diagnose_file_with_shared(
        &self,
        file_id: FileId,
        cancel_token: CancellationToken,
        shared_data: std::sync::Arc<diagnostic::SharedDiagnosticData>,
    ) -> Option<Vec<lsp_types::Diagnostic>> {
        self.diagnostic.diagnose_file_with_shared(
            &self.compilation,
            file_id,
            cancel_token,
            shared_data,
        )
    }

    pub fn precompute_diagnostic_shared_data(
        &self,
    ) -> std::sync::Arc<diagnostic::SharedDiagnosticData> {
        self.diagnostic.precompute_shared_data(&self.compilation)
    }

    /// Return main-workspace files in an order that keeps parallel diagnostic
    pub fn get_main_workspace_file_ids_for_diagnostics(&self) -> Vec<FileId> {
        let db = self.compilation.get_db();
        let vfs = db.get_vfs();
        let mut file_ids = db.get_module_index().get_main_workspace_file_ids();
        file_ids.sort_unstable_by(|left, right| {
            let left_len = vfs.get_file_content(left).map_or(0, String::len);
            let right_len = vfs.get_file_content(right).map_or(0, String::len);
            right_len.cmp(&left_len).then_with(|| left.cmp(right))
        });
        file_ids
    }

    pub fn reindex(&mut self) {
        let file_ids = self.compilation.get_db().get_vfs().get_all_file_ids();
        self.compilation.clear_index();
        self.compilation.update_index(file_ids);
    }

    /// 清理文件系统中不再存在的文件
    pub fn cleanup_nonexistent_files(&mut self) {
        let mut files_to_remove = Vec::new();

        // 获取所有当前在VFS中的文件
        let vfs = self.compilation.get_db().get_vfs();
        for file_id in vfs.get_all_local_file_ids() {
            if self
                .compilation
                .get_db()
                .get_module_index()
                .is_std(&file_id)
            {
                continue;
            }
            if let Some(path) = vfs.get_file_path(&file_id).filter(|path| !path.exists())
                && let Some(uri) = file_path_to_uri(path)
            {
                log::info!(
                    "cleanup_nonexistent_files: removing file_id={:?} path={}",
                    file_id,
                    path.display(),
                );
                files_to_remove.push(uri);
            }
        }

        if !files_to_remove.is_empty() {
            log::info!(
                "cleanup_nonexistent_files: removing {} files total",
                files_to_remove.len()
            );
        }

        // 移除不存在的文件
        for uri in files_to_remove {
            self.remove_file_by_uri(&uri);
        }
    }

    pub fn check_schema_update(&self) -> bool {
        self.compilation
            .get_db()
            .get_json_schema_index()
            .has_need_resolve_schemas()
    }

    pub fn get_schemas_to_fetch(&self) -> Vec<Url> {
        self.compilation
            .get_db()
            .get_json_schema_index()
            .get_need_resolve_schemas()
    }

    pub fn apply_fetched_schemas(&mut self, url_contents: HashMap<Url, String>) {
        if url_contents.is_empty() {
            return;
        }

        let converter = SchemaConverter::new(true);
        for (url, json_content) in url_contents {
            // let short_name = get_schema_short_name(&url);
            match converter.convert_from_str(&json_content) {
                Ok(convert_result) => {
                    let uri = match Uri::from_str(url.as_str()) {
                        Ok(uri) => uri,
                        Err(e) => {
                            log::error!("Failed to convert URL to URI {:?}: {}", url, e);
                            continue;
                        }
                    };
                    let file_id =
                        self.update_remote_file_by_uri(&uri, Some(convert_result.annotation_text));
                    if let Some(f) = self
                        .compilation
                        .get_db_mut()
                        .get_json_schema_index_mut()
                        .get_schema_file_mut(&url)
                    {
                        *f = JsonSchemaFile::Resolved(LuaTypeDeclId::local(
                            file_id,
                            &convert_result.root_type_name,
                        ));
                    }
                }
                Err(e) => {
                    log::error!("Failed to convert schema from URL {:?}: {}", url, e);
                }
            }
        }

        self.compilation
            .get_db_mut()
            .get_json_schema_index_mut()
            .reset_rest_schemas();
    }

    pub async fn update_schema(&mut self) {
        let urls = self.get_schemas_to_fetch();
        let url_contents = fetch_schema_urls(urls).await;
        self.apply_fetched_schemas(url_contents);
    }
}

fn select_cross_file_stabilization_dependents(
    all_dependents: impl IntoIterator<Item = FileId>,
    changed: &HashSet<FileId>,
) -> Vec<FileId> {
    let mut dependents = all_dependents
        .into_iter()
        .filter(|file_id| !changed.contains(file_id))
        .collect::<Vec<_>>();
    dependents.sort_unstable();
    dependents.dedup();
    dependents
}

impl Default for EmmyLuaAnalysis {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;

    use glua_parser::LuaSyntaxId;
    use lsp_types::Uri;
    use tokio_util::sync::CancellationToken;

    use crate::{
        EmmyLuaAnalysis, FileId, GmodVguiParentCallMetadata, GmodVguiParentCallOrigin,
        GmodVguiParentSource, LuaDependencyKind, select_cross_file_stabilization_dependents,
    };

    #[test]
    fn reindex_expansion_includes_indexed_file_dependents() {
        let changed = FileId { id: 1 };
        let direct_caller = FileId { id: 2 };
        let transitive_caller = FileId { id: 3 };
        let unrelated = FileId { id: 4 };
        let mut analysis = EmmyLuaAnalysis::new();
        let dependencies = analysis
            .compilation
            .get_db_mut()
            .get_file_dependencies_index_mut();
        dependencies.add_dependency_file(direct_caller, changed, LuaDependencyKind::Include);
        dependencies.add_dependency_file(
            transitive_caller,
            direct_caller,
            LuaDependencyKind::Include,
        );
        dependencies.add_dependency_file(unrelated, unrelated, LuaDependencyKind::Include);

        assert_eq!(
            analysis.expand_reindex_file_ids(vec![changed]),
            vec![changed, direct_caller, transitive_caller]
        );
    }

    #[test]
    fn reindex_expansion_includes_unresolved_path_dependents_for_reopened_file() {
        let workspace = std::env::temp_dir().join("gmod_glua_ls_reopen_dependency_workspace");
        let uri = |name: &str| {
            Uri::parse_from_file_path(&workspace.join(name)).expect("uri should parse")
        };
        let target_uri = uri("lua/mixins/reopened.lua");
        let caller_uri = uri("lua/autorun/reopen_consumer.lua");
        let mut analysis = EmmyLuaAnalysis::new();
        analysis.add_main_workspace(workspace);
        let old_target = analysis
            .update_file_by_uri(&target_uri, Some("return {}".to_string()))
            .expect("target should be created");
        let caller = analysis
            .update_file_by_uri(
                &caller_uri,
                Some(r#"local reopened = include("mixins/reopened.lua")"#.to_string()),
            )
            .expect("caller should be created");

        analysis
            .remove_file_by_uri(&target_uri)
            .expect("target should be removed");
        assert_ne!(
            analysis
                .compilation
                .get_db()
                .get_vfs()
                .get_file_id(&target_uri),
            Some(old_target)
        );

        let reopened_target = analysis
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content(&target_uri, Some("return {}".to_string()));

        assert!(
            analysis
                .expand_reindex_file_ids(vec![reopened_target])
                .contains(&caller)
        );
    }

    fn reopen_path_expands_to_caller(
        target_path: &str,
        caller_path: &str,
        dependency_expr: &str,
    ) -> bool {
        let workspace = std::env::temp_dir().join(format!(
            "gmod_glua_ls_reopen_path_variants_{}",
            target_path.replace(['/', '.'], "_")
        ));
        let uri = |name: &str| {
            Uri::parse_from_file_path(&workspace.join(name)).expect("uri should parse")
        };
        let target_uri = uri(target_path);
        let caller_uri = uri(caller_path);
        let mut analysis = EmmyLuaAnalysis::new();
        analysis.add_main_workspace(workspace);
        analysis
            .update_file_by_uri(&target_uri, Some("return {}".to_string()))
            .expect("target should be created");
        let caller = analysis
            .update_file_by_uri(
                &caller_uri,
                Some(format!("local reopened = {dependency_expr}")),
            )
            .expect("caller should be created");

        analysis
            .remove_file_by_uri(&target_uri)
            .expect("target should be removed");
        let reopened_target = analysis
            .compilation
            .get_db_mut()
            .get_vfs_mut()
            .set_file_content(&target_uri, Some("return {}".to_string()));

        analysis
            .expand_reindex_file_ids(vec![reopened_target])
            .contains(&caller)
    }

    #[test]
    fn reindex_expansion_matches_reopened_dependency_path_variants() {
        for (target_path, caller_path, dependency_expr) in [
            (
                "lua/autorun/mixins/parent.lua",
                "lua/autorun/sub/parent_consumer.lua",
                r#"include("../mixins/parent.lua")"#,
            ),
            (
                "lua/mixins/lua_prefixed.lua",
                "lua/autorun/lua_prefixed_consumer.lua",
                r#"include("lua/mixins/lua_prefixed.lua")"#,
            ),
            (
                "lua/mixins/extensionless.lua",
                "lua/autorun/extensionless_consumer.lua",
                r#"include("mixins/extensionless")"#,
            ),
            (
                "lua/mixins/required.lua",
                "lua/autorun/required_consumer.lua",
                r#"require("mixins.required")"#,
            ),
        ] {
            assert!(
                reopen_path_expands_to_caller(target_path, caller_path, dependency_expr),
                "{dependency_expr} should match {target_path}"
            );
        }
    }

    #[test]
    fn reindex_expansion_path_case_matches_vfs_platform_semantics() {
        let matched = reopen_path_expands_to_caller(
            "lua/mixins/CaseSensitive.lua",
            "lua/autorun/case_consumer.lua",
            r#"include("mixins/casesensitive.lua")"#,
        );
        assert_eq!(matched, cfg!(target_os = "windows"));
    }

    #[test]
    fn stabilization_dependents_exclude_files_already_analyzed_in_batch() {
        let changed_a = FileId { id: 1 };
        let changed_b = FileId { id: 2 };
        let unchanged_dependent = FileId { id: 3 };
        let changed = HashSet::from([changed_a, changed_b]);

        assert_eq!(
            select_cross_file_stabilization_dependents(
                [
                    changed_b,
                    unchanged_dependent,
                    changed_a,
                    unchanged_dependent
                ],
                &changed,
            ),
            vec![unchanged_dependent]
        );
    }

    #[test]
    fn diagnostic_file_ids_are_prioritized_by_source_size_then_file_id() {
        let workspace = std::env::temp_dir().join("gmod_glua_ls_diagnostic_priority_workspace");
        let make_uri = |name: &str| {
            Uri::parse_from_file_path(&workspace.join(name)).expect("uri should parse")
        };
        let small_a = make_uri("small_a.lua");
        let large = make_uri("large.lua");
        let small_b = make_uri("small_b.lua");

        let mut analysis = EmmyLuaAnalysis::new();
        analysis.add_main_workspace(workspace);
        analysis.update_files_by_uri(vec![
            (small_b.clone(), Some("return 1".to_string())),
            (
                large.clone(),
                Some("local value = { one = 1, two = 2, three = 3 }\nreturn value".to_string()),
            ),
            (small_a.clone(), Some("return 2".to_string())),
        ]);

        let large_id = analysis
            .get_file_id(&large)
            .expect("large file should exist");
        let mut small_ids = [
            analysis
                .get_file_id(&small_a)
                .expect("small_a file should exist"),
            analysis
                .get_file_id(&small_b)
                .expect("small_b file should exist"),
        ];
        small_ids.sort_unstable();

        assert_eq!(
            analysis.get_main_workspace_file_ids_for_diagnostics(),
            vec![large_id, small_ids[0], small_ids[1]]
        );
    }

    fn test_workspace_and_uri() -> (PathBuf, Uri) {
        let workspace = std::env::temp_dir().join("gmod_glua_ls_analysis_test_workspace");
        let test_file = workspace.join("test.lua");
        let uri = Uri::parse_from_file_path(&test_file).expect("uri should parse");
        (workspace, uri)
    }

    #[test]
    fn unchanged_update_file_by_uri_rebuilds_index() {
        let mut analysis = EmmyLuaAnalysis::new();
        let (workspace, uri) = test_workspace_and_uri();
        analysis.add_main_workspace(workspace);

        let content = "local IsValid = IsValid";
        let file_id = analysis
            .update_file_by_uri(&uri, Some(content.to_string()))
            .expect("file id should exist");

        analysis.compilation.clear_index();
        assert!(
            analysis
                .compilation
                .get_db()
                .get_module_index()
                .get_module(file_id)
                .is_none()
        );

        analysis.update_file_by_uri(&uri, Some(content.to_string()));
        assert!(
            analysis
                .compilation
                .get_db()
                .get_module_index()
                .get_module(file_id)
                .is_some()
        );
    }

    #[test]
    fn unchanged_update_files_by_uri_rebuilds_index() {
        let mut analysis = EmmyLuaAnalysis::new();
        let (workspace, uri) = test_workspace_and_uri();
        analysis.add_main_workspace(workspace);

        let content = "local IsValid = IsValid";
        let file_id = analysis
            .update_file_by_uri(&uri, Some(content.to_string()))
            .expect("file id should exist");

        analysis.compilation.clear_index();
        let updated = analysis.update_files_by_uri(vec![(uri, Some(content.to_string()))]);
        assert_eq!(updated, vec![file_id]);
        assert!(
            analysis
                .compilation
                .get_db()
                .get_module_index()
                .get_module(file_id)
                .is_some()
        );
    }

    #[test]
    fn vfs_update_files_by_uri_assigns_stable_file_ids_for_new_files() {
        let make_uri = |root: &PathBuf, name: &str| {
            let file = root.join(name);
            Uri::parse_from_file_path(&file).expect("uri should parse")
        };

        let workspace_a = std::env::temp_dir().join("gmod_glua_ls_stable_ids_a");
        let workspace_b = std::env::temp_dir().join("gmod_glua_ls_stable_ids_b");

        let mut analysis_a = EmmyLuaAnalysis::new();
        analysis_a.add_main_workspace(workspace_a.clone());
        let a1 = make_uri(&workspace_a, "a.lua");
        let b1 = make_uri(&workspace_a, "b.lua");
        let ids_a = analysis_a.update_files_by_uri(vec![
            (b1.clone(), Some("return 'b'".to_string())),
            (a1.clone(), Some("return 'a'".to_string())),
        ]);
        assert_eq!(ids_a.len(), 2);
        let a1_id = analysis_a
            .get_file_id(&a1)
            .expect("a.lua should have stable file id");
        let b1_id = analysis_a
            .get_file_id(&b1)
            .expect("b.lua should have stable file id");

        let mut analysis_b = EmmyLuaAnalysis::new();
        analysis_b.add_main_workspace(workspace_b.clone());
        let a2 = make_uri(&workspace_b, "a.lua");
        let b2 = make_uri(&workspace_b, "b.lua");
        let ids_b = analysis_b.update_files_by_uri(vec![
            (a2.clone(), Some("return 'a'".to_string())),
            (b2.clone(), Some("return 'b'".to_string())),
        ]);
        assert_eq!(ids_b.len(), 2);
        let a2_id = analysis_b
            .get_file_id(&a2)
            .expect("a.lua should have stable file id");
        let b2_id = analysis_b
            .get_file_id(&b2)
            .expect("b.lua should have stable file id");

        assert_eq!(
            a1_id, a2_id,
            "a.lua file id should be input-order independent"
        );
        assert_eq!(
            b1_id, b2_id,
            "b.lua file id should be input-order independent"
        );
    }

    #[test]
    fn vgui_forwarding_removal_seed_falls_back_to_another_workspace() {
        let root = std::env::temp_dir().join("gmod_glua_ls_vgui_forwarding_seed_workspace");
        let main_workspace = root.join("main");
        let library_workspace = root.join("library");
        let main_uri = Uri::parse_from_file_path(&main_workspace.join("consumer.lua"))
            .expect("uri should parse");
        let helper_uri = Uri::parse_from_file_path(&library_workspace.join("helper.lua"))
            .expect("uri should parse");

        let mut analysis = EmmyLuaAnalysis::new();
        analysis.add_main_workspace(main_workspace);
        analysis.add_library_workspace(library_workspace);
        let main_file_id = analysis
            .update_file_by_uri(&main_uri, Some("return true".to_string()))
            .expect("main file should be indexed");
        let helper_file_id = analysis
            .update_file_by_uri(&helper_uri, Some("return true".to_string()))
            .expect("helper file should be indexed");
        let helper_syntax_id = LuaSyntaxId::from_node(
            &analysis
                .compilation
                .get_db()
                .get_vfs()
                .get_syntax_tree(&helper_file_id)
                .expect("helper syntax tree should exist")
                .get_red_root(),
        );
        analysis
            .compilation
            .get_db_mut()
            .get_gmod_class_metadata_index_mut()
            .add_vgui_parent_call(
                helper_file_id,
                GmodVguiParentCallMetadata {
                    syntax_id: helper_syntax_id,
                    child: GmodVguiParentSource::Unknown,
                    parent: GmodVguiParentSource::Unknown,
                    relations: Vec::new(),
                    resolved_source: None,
                    origin: GmodVguiParentCallOrigin::Annotated,
                },
            );

        let removed_file_ids = HashSet::from([helper_file_id]);
        let mut reindex_file_ids = vec![helper_file_id];
        analysis.add_vgui_forwarding_removal_seed(&removed_file_ids, &mut reindex_file_ids);

        assert_eq!(reindex_file_ids, vec![main_file_id, helper_file_id]);
    }

    /// See implementation.
    #[test]
    fn two_phase_reindex_matches_single_phase_diagnostics() {
        // A class definition plus a consumer whose inferred type references it:
        let producer_source = |member: &str| {
            format!(
                "---@class Thing
local Thing = {{}}
function Thing:{member}() end
return Thing
"
            )
        };
        let consumer_source = "---@type Thing
local thing
thing:name()
consume(thing)
";
        let helper_source = "function consume(value) end
";

        let build = |dir: &str| {
            let workspace = std::env::temp_dir().join(dir);
            let uri = |name: &str| {
                Uri::parse_from_file_path(&workspace.join(name)).expect("uri should parse")
            };
            let uris = [uri("producer.lua"), uri("consumer.lua"), uri("helper.lua")];
            let mut analysis = EmmyLuaAnalysis::new();
            analysis.add_main_workspace(workspace);
            analysis.update_files_by_uri(vec![
                (uris[0].clone(), Some(producer_source("name"))),
                (uris[1].clone(), Some(consumer_source.to_string())),
                (uris[2].clone(), Some(helper_source.to_string())),
            ]);
            (analysis, uris)
        };

        let snapshot = |analysis: &EmmyLuaAnalysis, uris: &[Uri; 3]| {
            let shared = analysis.precompute_diagnostic_shared_data();
            uris.iter()
                .map(|uri| {
                    let file_id = analysis.get_file_id(uri).expect("file should be indexed");
                    analysis
                        .diagnose_file_with_shared(
                            file_id,
                            CancellationToken::new(),
                            shared.clone(),
                        )
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
        };

        // Renaming the produced field is a real change: the consumer reads the
        // old name, so the edit has to reach it.
        let (mut single, single_uris) = build("gmod_glua_ls_two_phase_single");
        let single_producer = single
            .update_file_text_only(&single_uris[0], producer_source("title"))
            .expect("producer should exist");
        single.reindex_files(vec![single_producer]);

        let (mut split, split_uris) = build("gmod_glua_ls_two_phase_split");
        let split_producer = split
            .get_file_id(&split_uris[0])
            .expect("producer should be indexed");
        let expansion = split.expand_reindex_file_ids(vec![split_producer]);
        split
            .update_file_text_only(&split_uris[0], producer_source("title"))
            .expect("producer should exist");
        split.self_index_files(vec![split_producer]);
        split.reindex_expanded_files(vec![split_producer], expansion);

        let single_diagnostics = snapshot(&single, &single_uris);
        assert!(
            single_diagnostics
                .iter()
                .any(|diagnostics| !diagnostics.is_empty()),
            "fixture should exercise observable diagnostics"
        );
        assert_eq!(
            snapshot(&split, &split_uris),
            single_diagnostics,
            "self-indexing the edited file first must not change the outcome"
        );
    }

    #[test]
    fn multi_file_batch_reindex_matches_clean_build_diagnostics() {
        let incremental_workspace =
            std::env::temp_dir().join("gmod_glua_ls_batch_reindex_incremental_workspace");
        let clean_workspace =
            std::env::temp_dir().join("gmod_glua_ls_batch_reindex_clean_workspace");
        let uris = |workspace: &PathBuf| {
            ["producer.lua", "consumer.lua", "helper.lua"].map(|name| {
                Uri::parse_from_file_path(&workspace.join(name)).expect("uri should parse")
            })
        };
        let incremental_uris = uris(&incremental_workspace);
        let clean_uris = uris(&clean_workspace);

        let initial_producer = r#"
            Registry = Registry or {}
            Registry.item = { name = "old" }
            Registry.legacy = { value = 1 }
        "#;
        let initial_consumer = r#"
            local name = Registry.item.name
            local legacy = Registry.legacy.value
            consume(name, legacy)
        "#;
        let changed_producer = r#"
            Registry = Registry or {}
            Registry.item = { title = "new" }
        "#;
        let changed_consumer = r#"
            local title = Registry.item.title
            local removed = Registry.legacy.value
            consume(title, removed)
        "#;
        let initial_helper = "function consume(name, value) end";
        let changed_helper = "function consume(title, removed, required) end";

        let mut analysis = EmmyLuaAnalysis::new();
        analysis.add_main_workspace(incremental_workspace);
        analysis.update_files_by_uri(vec![
            (
                incremental_uris[1].clone(),
                Some(initial_consumer.to_string()),
            ),
            (
                incremental_uris[0].clone(),
                Some(initial_producer.to_string()),
            ),
            (
                incremental_uris[2].clone(),
                Some(initial_helper.to_string()),
            ),
        ]);

        let mut file_ids = incremental_uris
            .iter()
            .map(|uri| analysis.get_file_id(&uri).expect("file should be indexed"))
            .collect::<Vec<FileId>>();
        file_ids.sort_unstable();

        analysis.update_file_text_only(&incremental_uris[0], changed_producer.to_string());
        analysis.update_file_text_only(&incremental_uris[1], changed_consumer.to_string());
        analysis.update_file_text_only(&incremental_uris[2], changed_helper.to_string());
        analysis.reindex_files(vec![file_ids[2], file_ids[0], file_ids[1], file_ids[2]]);

        let mut clean_analysis = EmmyLuaAnalysis::new();
        clean_analysis.add_main_workspace(clean_workspace);
        clean_analysis.update_files_by_uri(vec![
            (clean_uris[2].clone(), Some(changed_helper.to_string())),
            (clean_uris[0].clone(), Some(changed_producer.to_string())),
            (clean_uris[1].clone(), Some(changed_consumer.to_string())),
        ]);

        let snapshot = |analysis: &EmmyLuaAnalysis, file_uris: &[Uri]| {
            let shared = analysis.precompute_diagnostic_shared_data();
            file_uris
                .iter()
                .map(|uri| {
                    let file_id = analysis.get_file_id(uri).expect("file should be indexed");
                    analysis
                        .diagnose_file_with_shared(
                            file_id,
                            CancellationToken::new(),
                            shared.clone(),
                        )
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
        };

        let clean_diagnostics = snapshot(&clean_analysis, &clean_uris);
        assert!(
            clean_diagnostics
                .iter()
                .any(|diagnostics| !diagnostics.is_empty()),
            "fixture should exercise observable diagnostics"
        );

        assert_eq!(snapshot(&analysis, &incremental_uris), clean_diagnostics);
        for (incremental_uri, clean_uri) in incremental_uris.iter().zip(&clean_uris) {
            let incremental_file_id = analysis
                .get_file_id(incremental_uri)
                .expect("incremental file should be indexed");
            let clean_file_id = clean_analysis
                .get_file_id(clean_uri)
                .expect("clean file should be indexed");
            assert!(
                analysis
                    .compilation
                    .get_db()
                    .get_module_index()
                    .get_module(incremental_file_id)
                    .is_some(),
                "batch reindex should restore module ownership for {incremental_file_id:?}"
            );
            assert!(
                clean_analysis
                    .compilation
                    .get_db()
                    .get_module_index()
                    .get_module(clean_file_id)
                    .is_some(),
                "clean build should index module ownership for {clean_file_id:?}"
            );
        }
    }
}
