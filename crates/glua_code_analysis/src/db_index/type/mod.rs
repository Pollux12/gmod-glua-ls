mod generic_param;
mod humanize_type;
mod inference_fact;
mod test;
mod type_decl;
mod type_ops;
mod type_owner;
mod type_visit_trait;
mod types;

use super::traits::LuaIndex;
use crate::{
    DbIndex, FileId, InFiled, LuaMemberOwner, db_index::r#type::type_decl::LuaTypeIdentifier,
};
pub use generic_param::GenericParam;
pub use humanize_type::{
    DEFAULT_DETAIL_MEMBER_DISPLAY_COUNT, RenderLevel, format_union_type, humanize_member_key_name,
    humanize_type,
};
pub use inference_fact::*;
use rowan::TextRange;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
pub use type_decl::{LuaDeclLocation, LuaDeclTypeKind, LuaTypeDecl, LuaTypeDeclId, LuaTypeFlag};
pub use type_ops::TypeOps;
pub use type_owner::{LuaTypeCache, LuaTypeOwner};
pub use type_visit_trait::TypeVisitTrait;
pub use types::*;

#[derive(Debug, Clone)]
pub struct LuaResolvedAliasType {
    pub alias_id: Option<LuaTypeDeclId>,
    pub typ: LuaType,
}

pub fn resolve_alias_type(db: &DbIndex, typ: &LuaType) -> LuaResolvedAliasType {
    let mut visited_aliases = HashSet::new();
    resolve_alias_type_inner(db, typ, None, &mut visited_aliases)
}

fn resolve_alias_type_inner(
    db: &DbIndex,
    typ: &LuaType,
    alias_id: Option<LuaTypeDeclId>,
    visited_aliases: &mut HashSet<LuaTypeDeclId>,
) -> LuaResolvedAliasType {
    let (LuaType::Ref(type_id) | LuaType::Def(type_id)) = typ else {
        return LuaResolvedAliasType {
            alias_id,
            typ: typ.clone(),
        };
    };

    let Some(type_decl) = db.get_type_index().get_type_decl(type_id) else {
        return LuaResolvedAliasType {
            alias_id,
            typ: typ.clone(),
        };
    };

    if !type_decl.is_alias() || !visited_aliases.insert(type_id.clone()) {
        return LuaResolvedAliasType {
            alias_id,
            typ: typ.clone(),
        };
    }

    let Some(origin) = type_decl.get_alias_origin(db, None) else {
        return LuaResolvedAliasType {
            alias_id,
            typ: typ.clone(),
        };
    };

    let alias_id = alias_id.or_else(|| Some(type_id.clone()));
    resolve_alias_type_inner(db, &origin, alias_id, visited_aliases)
}

fn replace_table_const_in_type(
    typ: &LuaType,
    table_range: &InFiled<TextRange>,
    replacement: &LuaType,
) -> Option<LuaType> {
    match typ {
        LuaType::TableConst(existing_range) if existing_range == table_range => {
            Some(replacement.clone())
        }
        LuaType::Union(union) => {
            let mut changed = false;
            let new_types: Vec<LuaType> = union
                .into_vec()
                .into_iter()
                .map(|sub_type| {
                    replace_table_const_in_type(&sub_type, table_range, replacement)
                        .inspect(|_| changed = true)
                        .unwrap_or(sub_type)
                })
                .collect();
            changed.then(|| LuaType::from_vec(new_types))
        }
        LuaType::Intersection(intersection) => {
            let mut changed = false;
            let new_types: Vec<LuaType> = intersection
                .get_types()
                .iter()
                .map(|sub_type| {
                    replace_table_const_in_type(sub_type, table_range, replacement)
                        .inspect(|_| changed = true)
                        .unwrap_or_else(|| sub_type.clone())
                })
                .collect();
            changed.then(|| LuaType::from_vec(new_types))
        }
        LuaType::MergedTable(merged) => {
            let mut changed = false;
            let new_types: Vec<LuaType> = merged
                .get_types()
                .iter()
                .map(|sub_type| {
                    replace_table_const_in_type(sub_type, table_range, replacement)
                        .inspect(|_| changed = true)
                        .unwrap_or_else(|| sub_type.clone())
                })
                .collect();
            changed.then(|| LuaMergedTableType::new(new_types).into())
        }
        _ => None,
    }
}

fn replace_table_consts_in_type(
    typ: &LuaType,
    replacements: &HashMap<InFiled<TextRange>, LuaType>,
) -> Option<LuaType> {
    match typ {
        LuaType::TableConst(existing_range) => replacements.get(existing_range).cloned(),
        LuaType::Union(union) => {
            let mut changed = false;
            let new_types: Vec<LuaType> = union
                .into_vec()
                .into_iter()
                .map(|sub_type| {
                    replace_table_consts_in_type(&sub_type, replacements)
                        .inspect(|_| changed = true)
                        .unwrap_or(sub_type)
                })
                .collect();
            changed.then(|| LuaType::from_vec(new_types))
        }
        LuaType::Intersection(intersection) => {
            let mut changed = false;
            let new_types: Vec<LuaType> = intersection
                .get_types()
                .iter()
                .map(|sub_type| {
                    replace_table_consts_in_type(sub_type, replacements)
                        .inspect(|_| changed = true)
                        .unwrap_or_else(|| sub_type.clone())
                })
                .collect();
            changed.then(|| LuaType::from_vec(new_types))
        }
        LuaType::MergedTable(merged) => {
            let mut changed = false;
            let new_types: Vec<LuaType> = merged
                .get_types()
                .iter()
                .map(|sub_type| {
                    replace_table_consts_in_type(sub_type, replacements)
                        .inspect(|_| changed = true)
                        .unwrap_or_else(|| sub_type.clone())
                })
                .collect();
            changed.then(|| LuaMergedTableType::new(new_types).into())
        }
        _ => None,
    }
}

pub(crate) fn widen_literal_type_for_assignment(typ: &LuaType) -> LuaType {
    match typ {
        LuaType::IntegerConst(_) => LuaType::Integer,
        LuaType::FloatConst(_) => LuaType::Number,
        LuaType::StringConst(_) => LuaType::String,
        LuaType::BooleanConst(_) => LuaType::Boolean,
        LuaType::Union(union) => LuaType::from_vec(
            union
                .into_vec()
                .into_iter()
                .map(|sub_type| widen_literal_type_for_assignment(&sub_type))
                .collect(),
        ),
        LuaType::MultiLineUnion(multi_union) => LuaType::from_vec(
            multi_union
                .get_unions()
                .iter()
                .map(|(sub_type, _)| widen_literal_type_for_assignment(sub_type))
                .collect(),
        ),
        _ => typ.clone(),
    }
}

pub(crate) fn widen_related_assignment_type(typ: &LuaType, widen_table_literals: bool) -> LuaType {
    if widen_table_literals {
        return widen_table_literals_for_assignment(typ);
    }

    widen_literal_type_for_assignment(typ)
}

fn widen_table_literals_for_assignment(typ: &LuaType) -> LuaType {
    match typ {
        LuaType::TableConst(_) => LuaType::Table,
        LuaType::Union(union) => LuaType::from_vec(
            union
                .into_vec()
                .into_iter()
                .map(|sub_type| widen_table_literals_for_assignment(&sub_type))
                .collect(),
        ),
        _ => widen_literal_type_for_assignment(typ),
    }
}

pub(crate) fn widen_file_define_member_type(typ: &LuaType, widen_table_literals: bool) -> LuaType {
    match typ {
        LuaType::TableConst(_) if widen_table_literals => LuaType::Table,
        _ => widen_literal_type_for_assignment(typ),
    }
}

pub(crate) fn is_table_assignment_merge_type(typ: &LuaType) -> bool {
    matches!(
        typ,
        LuaType::Table
            | LuaType::TableConst(_)
            | LuaType::Object(_)
            | LuaType::MergedTable(_)
            | LuaType::TableOf(_)
    )
}

pub(crate) fn prefer_class_assignment_type(typ: &LuaType) -> Option<LuaType> {
    match typ {
        LuaType::Def(def_id) => Some(LuaType::Def(def_id.clone())),
        LuaType::Ref(ref_id) => Some(LuaType::Ref(ref_id.clone())),
        LuaType::Instance(instance) => prefer_class_assignment_type(instance.get_base()),
        LuaType::TypeGuard(inner) => prefer_class_assignment_type(inner),
        LuaType::Union(union) => prefer_class_assignment_type_from_iter(union.types()),
        LuaType::Intersection(intersection) => {
            prefer_class_assignment_type_from_iter(intersection.get_types().iter())
        }
        LuaType::MultiLineUnion(union) => {
            prefer_class_assignment_type_from_iter(union.get_unions().iter().map(|(typ, _)| typ))
        }
        _ => None,
    }
}

fn prefer_class_assignment_type_from_iter<'a>(
    types: impl Iterator<Item = &'a LuaType>,
) -> Option<LuaType> {
    for typ in types {
        if let Some(class_type) = prefer_class_assignment_type(typ) {
            return Some(class_type);
        }
    }

    None
}

pub(crate) fn is_class_bootstrap_compatible_type(typ: &LuaType, class_type: &LuaType) -> bool {
    if is_same_class_type(typ, class_type) {
        return true;
    }

    match typ {
        LuaType::TypeGuard(inner) => is_class_bootstrap_compatible_type(inner, class_type),
        LuaType::Instance(instance) => {
            is_class_bootstrap_compatible_type(instance.get_base(), class_type)
                || is_table_bootstrap_type(typ)
        }
        LuaType::Union(union) => union
            .types()
            .all(|sub_type| is_class_bootstrap_compatible_type(sub_type, class_type)),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .all(|sub_type| is_class_bootstrap_compatible_type(sub_type, class_type)),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .all(|(sub_type, _)| is_class_bootstrap_compatible_type(sub_type, class_type)),
        _ => is_table_bootstrap_type(typ),
    }
}

pub(crate) fn is_class_neutral_bootstrap_type(typ: &LuaType) -> bool {
    if is_table_bootstrap_type(typ) {
        return true;
    }

    match typ {
        LuaType::TypeGuard(inner) => is_class_neutral_bootstrap_type(inner),
        LuaType::Union(union) => union.types().all(is_class_neutral_bootstrap_type),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .all(is_class_neutral_bootstrap_type),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .all(|(sub_type, _)| is_class_neutral_bootstrap_type(sub_type)),
        _ => false,
    }
}

pub(crate) fn is_same_class_type(left: &LuaType, right: &LuaType) -> bool {
    match (
        class_decl_id_from_type(left),
        class_decl_id_from_type(right),
    ) {
        (Some(left_id), Some(right_id)) => left_id == right_id,
        _ => false,
    }
}

pub(crate) fn class_decl_id_from_type(typ: &LuaType) -> Option<crate::LuaTypeDeclId> {
    match typ {
        LuaType::Def(def_id) | LuaType::Ref(def_id) => Some(def_id.clone()),
        LuaType::Instance(instance) => class_decl_id_from_type(instance.get_base()),
        LuaType::TypeGuard(inner) => class_decl_id_from_type(inner),
        _ => None,
    }
}

pub(crate) fn is_table_bootstrap_type(typ: &LuaType) -> bool {
    typ.is_table() || matches!(typ, LuaType::Unknown | LuaType::Nil | LuaType::Never)
}

pub(crate) fn prune_redundant_guarded_table_bootstrap_type(db: &DbIndex, typ: LuaType) -> LuaType {
    let LuaType::Union(union) = typ else {
        return typ;
    };

    let types = union.into_vec();
    if !types
        .iter()
        .any(|typ| is_informative_guarded_table_branch(db, typ))
    {
        return collapse_guarded_table_bootstrap_branches(db, types);
    }

    merge_guarded_table_bootstrap_result(
        types
            .into_iter()
            .filter(|typ| !is_guarded_table_bootstrap_branch(db, typ))
            .collect(),
    )
}

fn collapse_guarded_table_bootstrap_branches(db: &DbIndex, types: Vec<LuaType>) -> LuaType {
    let mut saw_bootstrap = false;
    let mut retained = Vec::with_capacity(types.len());

    for typ in types {
        if is_guarded_table_bootstrap_branch(db, &typ) {
            saw_bootstrap = true;
        } else {
            retained.push(typ);
        }
    }

    if saw_bootstrap {
        retained.push(LuaType::Table);
    }

    merge_guarded_table_bootstrap_result(retained)
}

fn merge_guarded_table_bootstrap_result(types: Vec<LuaType>) -> LuaType {
    let mut table_components = Vec::new();
    let mut other_components = Vec::new();

    for typ in types {
        collect_guarded_table_merge_components(typ, &mut table_components, &mut other_components);
    }

    if table_components
        .iter()
        .any(|component| !matches!(component, LuaType::Table))
    {
        table_components.retain(|component| !matches!(component, LuaType::Table));
    }

    let merged_table = match table_components.len() {
        0 => None,
        1 => Some(table_components.remove(0)),
        _ => Some(LuaMergedTableType::new(table_components).into()),
    };

    if other_components.is_empty() {
        return merged_table.unwrap_or(LuaType::Nil);
    }

    if let Some(merged_table) = merged_table {
        other_components.push(merged_table);
    }

    LuaType::from_vec(other_components)
}

fn collect_guarded_table_merge_components(
    typ: LuaType,
    table_components: &mut Vec<LuaType>,
    other_components: &mut Vec<LuaType>,
) {
    match typ {
        LuaType::MergedTable(merged) => {
            for component in merged.get_types() {
                collect_guarded_table_merge_components(
                    component.clone(),
                    table_components,
                    other_components,
                );
            }
        }
        LuaType::Table
        | LuaType::TableConst(_)
        | LuaType::Object(_)
        | LuaType::TableGeneric(_)
        | LuaType::TableOf(_) => table_components.push(typ),
        _ => other_components.push(typ),
    }
}

fn is_informative_guarded_table_branch(db: &DbIndex, typ: &LuaType) -> bool {
    match typ {
        LuaType::TableConst(table_id) => {
            db.get_member_index()
                .get_member_len(&LuaMemberOwner::Element(table_id.clone()))
                > 0
        }
        LuaType::Object(object) => {
            !object.get_fields().is_empty() || !object.get_index_access().is_empty()
        }
        LuaType::MergedTable(merged) => merged
            .get_types()
            .iter()
            .any(|typ| is_informative_guarded_table_branch(db, typ)),
        _ => false,
    }
}

fn is_guarded_table_bootstrap_branch(db: &DbIndex, typ: &LuaType) -> bool {
    match typ {
        LuaType::Table => true,
        LuaType::TableConst(table_id) => {
            db.get_member_index()
                .get_member_len(&LuaMemberOwner::Element(table_id.clone()))
                == 0
        }
        LuaType::MergedTable(merged) => merged
            .get_types()
            .iter()
            .all(|typ| is_guarded_table_bootstrap_branch(db, typ)),
        _ => false,
    }
}

#[derive(Debug)]
pub struct LuaTypeIndex {
    file_namespace: HashMap<FileId, String>,
    file_using_namespace: HashMap<FileId, Vec<String>>,
    file_types: HashMap<FileId, Vec<LuaTypeDeclId>>,
    full_name_type_map: HashMap<LuaTypeDeclId, LuaTypeDecl>,
    generic_params: HashMap<LuaTypeDeclId, Vec<GenericParam>>,
    supers: HashMap<LuaTypeDeclId, Vec<InFiled<LuaType>>>,
    types: HashMap<LuaTypeOwner, LuaTypeCache>,
    in_filed_type_owner: HashMap<FileId, HashSet<LuaTypeOwner>>,
    fact_metadata: HashMap<LuaTypeOwner, LuaTypeFactMetadata>,
    definition_facts: HashMap<LuaDefinitionId, LuaTypeFact>,
    inference_events_by_file: HashMap<FileId, Arc<[LuaInferenceDiagnosticEvent]>>,
    support_file_dependents: HashMap<FileId, HashSet<FileId>>,
}

impl Default for LuaTypeIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaTypeIndex {
    pub fn new() -> Self {
        Self {
            file_namespace: HashMap::new(),
            file_using_namespace: HashMap::new(),
            file_types: HashMap::new(),
            full_name_type_map: HashMap::new(),
            generic_params: HashMap::new(),
            supers: HashMap::new(),
            types: HashMap::new(),
            in_filed_type_owner: HashMap::new(),
            fact_metadata: HashMap::new(),
            definition_facts: HashMap::new(),
            inference_events_by_file: HashMap::new(),
            support_file_dependents: HashMap::new(),
        }
    }

    pub fn add_file_namespace(&mut self, file_id: FileId, namespace: String) {
        self.file_namespace.insert(file_id, namespace);
    }

    pub fn get_file_namespace(&self, file_id: &FileId) -> Option<&String> {
        self.file_namespace.get(file_id)
    }

    pub fn add_file_using_namespace(&mut self, file_id: FileId, namespace: String) {
        self.file_using_namespace
            .entry(file_id)
            .or_default()
            .push(namespace);
    }

    pub fn get_file_using_namespace(&self, file_id: &FileId) -> Option<&Vec<String>> {
        self.file_using_namespace.get(file_id)
    }

    /// return previous FileId if exist
    pub fn add_type_decl(&mut self, file_id: FileId, type_decl: LuaTypeDecl) {
        let id = type_decl.get_id();
        self.file_types.entry(file_id).or_default().push(id.clone());

        if let Some(old_decl) = self.full_name_type_map.get_mut(&id) {
            for location in type_decl.get_locations() {
                old_decl.add_location(location.clone());
            }
        } else {
            self.full_name_type_map.insert(id, type_decl);
        }
    }

    pub fn add_type_decl_location(
        &mut self,
        file_id: FileId,
        decl_id: &LuaTypeDeclId,
        location: LuaDeclLocation,
    ) {
        if let Some(decl) = self.full_name_type_map.get_mut(decl_id) {
            decl.add_location(location);
            self.file_types
                .entry(file_id)
                .or_default()
                .push(decl_id.clone());
        }
    }

    pub fn find_type_decl(&self, file_id: FileId, name: &str) -> Option<&LuaTypeDecl> {
        if let Some(ns) = self.get_file_namespace(&file_id) {
            let full_name = LuaTypeDeclId::global(&format!("{}.{}", ns, name));
            if let Some(decl) = self.full_name_type_map.get(&full_name) {
                return Some(decl);
            }
        }
        if let Some(usings) = self.get_file_using_namespace(&file_id) {
            for ns in usings {
                let full_name = LuaTypeDeclId::global(&format!("{}.{}", ns, name));
                if let Some(decl) = self.full_name_type_map.get(&full_name) {
                    return Some(decl);
                }
            }
        }

        let local_id = LuaTypeDeclId::local(file_id, name);
        if let Some(decl) = self.full_name_type_map.get(&local_id) {
            return Some(decl);
        }

        let global_id = LuaTypeDeclId::global(name);
        self.full_name_type_map.get(&global_id)
    }

    pub fn find_type_decls(
        &self,
        file_id: FileId,
        prefix: &str,
    ) -> HashMap<String, Option<LuaTypeDeclId>> {
        let mut result = HashMap::new();
        let all_type_ids = self.full_name_type_map.keys().collect::<Vec<_>>();
        if let Some(ns) = self.get_file_namespace(&file_id) {
            let prefix = &format!("{}.{}", ns, prefix);
            for id in all_type_ids.clone() {
                let id_name = id.get_name();

                if let Some(rest_name) = id_name.strip_prefix(prefix) {
                    if let Some(i) = rest_name.find('.') {
                        let name = rest_name[..i].to_string();
                        result.entry(name).or_insert(None);
                    } else {
                        result.insert(rest_name.to_string(), Some(id.clone()));
                    }
                }
            }
        }

        if let Some(usings) = self.get_file_using_namespace(&file_id) {
            for ns in usings {
                let prefix = &format!("{}.{}", ns, prefix);
                for id in all_type_ids.clone() {
                    let id_name = id.get_name();

                    if let Some(rest_name) = id_name.strip_prefix(prefix) {
                        if let Some(i) = rest_name.find('.') {
                            let name = rest_name[..i].to_string();
                            result.entry(name).or_insert(None);
                        } else {
                            result.insert(rest_name.to_string(), Some(id.clone()));
                        }
                    }
                }
            }
        }

        for id in all_type_ids {
            let id_name = match id.get_id() {
                LuaTypeIdentifier::Local(f_id, name) => {
                    if f_id != &file_id {
                        continue;
                    }
                    name
                }
                LuaTypeIdentifier::Global(name) => name,
            };
            if id_name.starts_with(prefix)
                && let Some(rest_name) = id_name.strip_prefix(prefix)
            {
                if let Some(i) = rest_name.find('.') {
                    let name = rest_name[..i].to_string();
                    result.entry(name).or_insert(None);
                } else {
                    result.insert(rest_name.to_string(), Some(id.clone()));
                }
            }
        }

        result
    }

    pub fn add_generic_params(&mut self, decl_id: LuaTypeDeclId, params: Vec<GenericParam>) {
        self.generic_params.insert(decl_id, params);
    }

    pub fn get_generic_params(&self, decl_id: &LuaTypeDeclId) -> Option<&Vec<GenericParam>> {
        self.generic_params.get(decl_id)
    }

    pub fn add_super_type(&mut self, decl_id: LuaTypeDeclId, file_id: FileId, super_type: LuaType) {
        self.supers
            .entry(decl_id)
            .or_default()
            .push(InFiled::new(file_id, super_type));
    }

    pub fn has_super_type_in_file(
        &self,
        decl_id: &LuaTypeDeclId,
        file_id: FileId,
        super_type: &LuaType,
    ) -> bool {
        self.supers.get(decl_id).is_some_and(|supers| {
            supers
                .iter()
                .any(|entry| entry.file_id == file_id && &entry.value == super_type)
        })
    }

    pub fn add_super_type_if_missing(
        &mut self,
        decl_id: LuaTypeDeclId,
        file_id: FileId,
        super_type: LuaType,
    ) {
        if self.has_super_type_in_file(&decl_id, file_id, &super_type) {
            return;
        }

        self.add_super_type(decl_id, file_id, super_type);
    }

    pub fn get_super_types(&self, decl_id: &LuaTypeDeclId) -> Option<Vec<LuaType>> {
        self.supers
            .get(decl_id)
            .map(|supers| supers.iter().map(|s| s.value.clone()).collect())
    }

    pub fn get_super_types_iter(
        &self,
        decl_id: &LuaTypeDeclId,
    ) -> Option<impl Iterator<Item = &LuaType> + '_> {
        self.supers
            .get(decl_id)
            .map(|supers| supers.iter().map(|s| &s.value))
    }

    pub(crate) fn get_super_types_with_file_iter(
        &self,
        decl_id: &LuaTypeDeclId,
    ) -> Option<impl Iterator<Item = &InFiled<LuaType>> + '_> {
        self.supers.get(decl_id).map(|supers| supers.iter())
    }

    /// Get all direct subclasses of a given type
    /// Returns a vector of type declarations that directly inherit from the given type
    pub fn get_sub_types(&self, decl_id: &LuaTypeDeclId) -> Vec<&LuaTypeDecl> {
        let mut sub_types = Vec::new();

        // Iterate through all types and check their super types
        for (type_id, supers) in &self.supers {
            for super_filed in supers {
                // Check if this super type references our target type
                if let LuaType::Ref(super_id) = &super_filed.value {
                    if super_id == decl_id {
                        // Found a subclass
                        if let Some(sub_decl) = self.full_name_type_map.get(type_id) {
                            sub_types.push(sub_decl);
                        }
                        break; // No need to check other supers of this type
                    }
                }
            }
        }

        // Sort to ensure deterministic ordering regardless of HashMap iteration order
        sub_types.sort_by(|a, b| a.get_name().cmp(b.get_name()));
        sub_types
    }

    /// Get all subclasses (direct and indirect) of a given type recursively
    /// Returns a vector of type declarations in the inheritance hierarchy
    pub fn get_all_sub_types(&self, decl_id: &LuaTypeDeclId) -> Vec<&LuaTypeDecl> {
        let mut all_sub_types = Vec::new();
        let mut visited = HashSet::new();
        let mut queue = vec![decl_id.clone()];

        while let Some(current_id) = queue.pop() {
            if !visited.insert(current_id.clone()) {
                continue;
            }

            // Find direct subclasses of current_id
            let direct_subs = self.get_sub_types(&current_id);
            for sub_decl in direct_subs {
                let sub_id = sub_decl.get_id();
                if !visited.contains(&sub_id) {
                    all_sub_types.push(sub_decl);
                    queue.push(sub_id);
                }
            }
        }

        all_sub_types
    }

    pub fn get_type_decl(&self, decl_id: &LuaTypeDeclId) -> Option<&LuaTypeDecl> {
        self.full_name_type_map.get(decl_id)
    }

    pub fn get_all_types(&self) -> Vec<&LuaTypeDecl> {
        self.full_name_type_map.values().collect()
    }

    pub fn get_file_namespaces(&self) -> Vec<String> {
        self.file_namespace
            .values()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn get_type_decl_mut(&mut self, decl_id: &LuaTypeDeclId) -> Option<&mut LuaTypeDecl> {
        self.full_name_type_map.get_mut(decl_id)
    }

    pub fn bind_type(&mut self, owner: LuaTypeOwner, cache: LuaTypeCache) {
        if self.types.contains_key(&owner) {
            return;
        }
        let file_id = owner.get_file_id();
        self.types.insert(owner.clone(), cache);
        self.in_filed_type_owner
            .entry(file_id)
            .or_default()
            .insert(owner);
    }

    pub fn force_bind_type(&mut self, owner: LuaTypeOwner, cache: LuaTypeCache) {
        let file_id = owner.get_file_id();
        self.types.insert(owner.clone(), cache);
        self.in_filed_type_owner
            .entry(file_id)
            .or_default()
            .insert(owner.clone());
        if self.fact_metadata.remove(&owner).is_some() {
            self.rebuild_inference_derived_state(&HashSet::from([file_id]));
        }
    }

    pub fn bind_type_fact(
        &mut self,
        owner: LuaTypeOwner,
        cache: LuaTypeCache,
        metadata: LuaTypeFactMetadata,
    ) {
        if self.types.contains_key(&owner) {
            return;
        }

        let file_id = owner.get_file_id();
        let metadata = metadata.normalized();
        self.types.insert(owner.clone(), cache);
        self.in_filed_type_owner
            .entry(file_id)
            .or_default()
            .insert(owner.clone());
        self.fact_metadata.insert(owner, metadata);
        self.rebuild_inference_derived_state(&HashSet::from([file_id]));
    }

    pub fn force_bind_type_fact(
        &mut self,
        owner: LuaTypeOwner,
        cache: LuaTypeCache,
        metadata: LuaTypeFactMetadata,
    ) {
        let file_id = self.force_bind_type_fact_unchecked(owner, cache, metadata);
        self.rebuild_inference_derived_state(&HashSet::from([file_id]));
    }

    pub fn get_type_fact(&self, owner: &LuaTypeOwner) -> Option<LuaTypeFact> {
        let cache = self.types.get(owner)?;
        let fact = match self.fact_metadata.get(owner) {
            Some(metadata) => LuaTypeFact::from_normalized_parts(
                cache.as_type().clone(),
                metadata.confidence,
                metadata.base_provenance_kind,
                metadata.provenance.clone(),
            ),
            None => plain_cache_fact(cache),
        };
        Some(fact)
    }

    pub fn bind_definition_fact(&mut self, definition: LuaDefinitionId, fact: LuaTypeFact) {
        let file_id = self.bind_definition_fact_unchecked(definition, fact);
        self.rebuild_inference_derived_state(&HashSet::from([file_id]));
    }

    pub fn get_definition_fact(&self, definition: &LuaDefinitionId) -> Option<&LuaTypeFact> {
        self.definition_facts.get(definition)
    }

    pub fn get_inference_events_for_file(&self, file_id: FileId) -> &[LuaInferenceDiagnosticEvent] {
        self.inference_events_by_file
            .get(&file_id)
            .map(AsRef::as_ref)
            .unwrap_or_default()
    }

    pub fn files_depending_on_inference_support(
        &self,
        file_ids: &HashSet<FileId>,
    ) -> HashSet<FileId> {
        let mut dependents = HashSet::new();
        for file_id in file_ids {
            if let Some(files) = self.support_file_dependents.get(file_id) {
                dependents.extend(files.iter().copied());
            }
        }
        dependents
    }

    pub fn get_type_cache(&self, owner: &LuaTypeOwner) -> Option<&LuaTypeCache> {
        self.types.get(owner)
    }

    pub(crate) fn force_bind_type_fact_unchecked(
        &mut self,
        owner: LuaTypeOwner,
        cache: LuaTypeCache,
        metadata: LuaTypeFactMetadata,
    ) -> FileId {
        let file_id = owner.get_file_id();
        let metadata = metadata.normalized();
        self.types.insert(owner.clone(), cache);
        self.in_filed_type_owner
            .entry(file_id)
            .or_default()
            .insert(owner.clone());
        self.fact_metadata.insert(owner, metadata);
        file_id
    }

    pub(crate) fn bind_definition_fact_unchecked(
        &mut self,
        definition: LuaDefinitionId,
        fact: LuaTypeFact,
    ) -> FileId {
        let file_id = definition.file_id();
        self.definition_facts.insert(definition, fact);
        file_id
    }

    pub(crate) fn rebuild_inference_derived_state(&mut self, changed_files: &HashSet<FileId>) {
        if changed_files.is_empty() {
            return;
        }

        let mut events_by_file: HashMap<FileId, Vec<LuaInferenceDiagnosticEvent>> = HashMap::new();
        let mut support_file_dependents = HashMap::new();

        for (owner, metadata) in &self.fact_metadata {
            let Some(cache) = self.types.get(owner) else {
                continue;
            };
            let fact = LuaTypeFact::from_normalized_parts(
                cache.as_type().clone(),
                metadata.confidence,
                metadata.base_provenance_kind,
                metadata.provenance.clone(),
            );
            collect_fact_derived_state(
                owner.get_file_id(),
                &fact,
                &mut events_by_file,
                &mut support_file_dependents,
            );
        }

        for (definition, fact) in &self.definition_facts {
            collect_fact_derived_state(
                definition.file_id(),
                fact,
                &mut events_by_file,
                &mut support_file_dependents,
            );
        }

        self.inference_events_by_file = events_by_file
            .into_iter()
            .map(|(file_id, mut events)| {
                events.sort_by(|left, right| left.event.stable_cmp(&right.event));
                events.dedup_by(|left, right| left.event == right.event);
                (file_id, events.into())
            })
            .collect();
        self.support_file_dependents = support_file_dependents;
    }

    pub fn iter_type_caches(&self) -> impl Iterator<Item = (&LuaTypeOwner, &LuaTypeCache)> {
        self.types.iter()
    }

    pub fn replace_table_const_type(
        &mut self,
        table_range: &InFiled<TextRange>,
        replacement: &LuaType,
    ) {
        let updates: Vec<(LuaTypeOwner, LuaTypeCache)> = self
            .types
            .iter()
            .filter_map(|(owner, cache)| {
                replace_table_const_in_type(cache.as_type(), table_range, replacement).map(
                    |new_type| {
                        let new_cache = match cache {
                            LuaTypeCache::DocType(_) => LuaTypeCache::DocType(new_type),
                            LuaTypeCache::InferType(_) => LuaTypeCache::InferType(new_type),
                        };
                        (owner.clone(), new_cache)
                    },
                )
            })
            .collect();

        let mut changed_files = HashSet::new();
        for (owner, new_cache) in updates {
            changed_files.insert(owner.get_file_id());
            self.types.insert(owner, new_cache);
        }
        self.rebuild_inference_derived_state(&changed_files);
    }

    pub fn replace_table_const_types(
        &mut self,
        replacements: &HashMap<InFiled<TextRange>, LuaType>,
    ) {
        if replacements.is_empty() {
            return;
        }

        let updates: Vec<(LuaTypeOwner, LuaTypeCache)> = self
            .types
            .iter()
            .filter_map(|(owner, cache)| {
                replace_table_consts_in_type(cache.as_type(), replacements).map(|new_type| {
                    let new_cache = match cache {
                        LuaTypeCache::DocType(_) => LuaTypeCache::DocType(new_type),
                        LuaTypeCache::InferType(_) => LuaTypeCache::InferType(new_type),
                    };
                    (owner.clone(), new_cache)
                })
            })
            .collect();

        let mut changed_files = HashSet::new();
        for (owner, new_cache) in updates {
            changed_files.insert(owner.get_file_id());
            self.types.insert(owner, new_cache);
        }
        self.rebuild_inference_derived_state(&changed_files);
    }

    pub fn files_with_type_caches_referencing_files(
        &self,
        file_ids: &HashSet<FileId>,
    ) -> HashSet<FileId> {
        let mut dependent_files = HashSet::new();
        for (owner, cache) in &self.types {
            let owner_file_id = owner.get_file_id();
            if file_ids.contains(&owner_file_id) {
                continue;
            }

            if self.type_references_any_file(cache.as_type(), file_ids, owner_file_id) {
                dependent_files.insert(owner_file_id);
            }
        }

        dependent_files
    }

    pub fn files_with_cross_file_type_caches_referencing_files(
        &self,
        file_ids: &HashSet<FileId>,
    ) -> HashSet<FileId> {
        let mut dependent_files = HashSet::new();
        for (owner, cache) in &self.types {
            let owner_file_id = owner.get_file_id();
            if self.type_references_other_file(cache.as_type(), file_ids, owner_file_id) {
                dependent_files.insert(owner_file_id);
            }
        }

        dependent_files
    }
    fn type_references_any_file(
        &self,
        typ: &LuaType,
        file_ids: &HashSet<FileId>,
        owner_file_id: FileId,
    ) -> bool {
        let mut references_file = false;
        typ.visit_type(&mut |inner| {
            if references_file {
                return;
            }

            references_file = match inner {
                LuaType::TableConst(range) => file_ids.contains(&range.file_id),
                LuaType::Instance(instance) => file_ids.contains(&instance.get_range().file_id),
                LuaType::Signature(signature_id) => file_ids.contains(&signature_id.get_file_id()),
                LuaType::ModuleRef(file_id) => file_ids.contains(file_id),
                LuaType::Ref(type_id) | LuaType::Def(type_id) => {
                    self.get_type_decl(type_id).is_some_and(|decl| {
                        let locations = decl.get_locations();
                        !locations
                            .iter()
                            .any(|location| location.file_id == owner_file_id)
                            && locations
                                .iter()
                                .any(|location| file_ids.contains(&location.file_id))
                    })
                }
                _ => false,
            };
        });

        references_file
    }

    fn type_references_other_file(
        &self,
        typ: &LuaType,
        file_ids: &HashSet<FileId>,
        owner_file_id: FileId,
    ) -> bool {
        let references_changed_file =
            |file_id: FileId| file_id != owner_file_id && file_ids.contains(&file_id);
        let mut references_file = false;
        typ.visit_type(&mut |inner| {
            if references_file {
                return;
            }

            references_file = match inner {
                LuaType::TableConst(range) => references_changed_file(range.file_id),
                LuaType::Instance(instance) => {
                    references_changed_file(instance.get_range().file_id)
                }
                LuaType::Signature(signature_id) => {
                    references_changed_file(signature_id.get_file_id())
                }
                LuaType::ModuleRef(file_id) => references_changed_file(*file_id),
                LuaType::Ref(type_id) | LuaType::Def(type_id) => {
                    self.get_type_decl(type_id).is_some_and(|decl| {
                        let locations = decl.get_locations();
                        !locations
                            .iter()
                            .any(|location| location.file_id == owner_file_id)
                            && locations
                                .iter()
                                .any(|location| references_changed_file(location.file_id))
                    })
                }
                _ => false,
            };
        });

        references_file
    }
}

impl LuaIndex for LuaTypeIndex {
    fn remove(&mut self, file_id: FileId) {
        self.remove_files(std::slice::from_ref(&file_id));
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        let mut changed_files = HashSet::new();
        for &file_id in file_ids {
            if changed_files.insert(file_id) {
                self.remove_file_raw(file_id);
            }
        }

        self.definition_facts
            .retain(|definition, _| !changed_files.contains(&definition.file_id()));

        self.rebuild_inference_derived_state(&changed_files);
    }

    fn clear(&mut self) {
        self.file_namespace.clear();
        self.file_using_namespace.clear();
        self.file_types.clear();
        self.full_name_type_map.clear();
        self.generic_params.clear();
        self.supers.clear();
        self.types.clear();
        self.in_filed_type_owner.clear();
        self.fact_metadata.clear();
        self.definition_facts.clear();
        self.inference_events_by_file.clear();
        self.support_file_dependents.clear();
    }
}

impl LuaTypeIndex {
    fn remove_file_raw(&mut self, file_id: FileId) {
        self.file_namespace.remove(&file_id);
        self.file_using_namespace.remove(&file_id);
        if let Some(type_id_list) = self.file_types.remove(&file_id) {
            for id in type_id_list {
                let mut remove_type = false;
                if let Some(decl) = self.full_name_type_map.get_mut(&id) {
                    decl.get_mut_locations()
                        .retain(|loc| loc.file_id != file_id);
                    if decl.get_mut_locations().is_empty() {
                        self.full_name_type_map.remove(&id);
                        remove_type = true;
                        log::info!(
                            "type_index: type '{}' fully removed (file_id={:?})",
                            id.get_simple_name(),
                            file_id,
                        );
                    }
                }

                if let Some(supers) = self.supers.get_mut(&id) {
                    supers.retain(|s| s.file_id != file_id);
                    if supers.is_empty() {
                        self.supers.remove(&id);
                    }
                }

                if remove_type {
                    self.generic_params.remove(&id);
                }
            }
        }

        if let Some(type_owners) = self.in_filed_type_owner.remove(&file_id) {
            for type_owner in type_owners {
                self.types.remove(&type_owner);
                self.fact_metadata.remove(&type_owner);
            }
        }
    }
}

fn collect_fact_derived_state(
    owner_file_id: FileId,
    fact: &LuaTypeFact,
    events_by_file: &mut HashMap<FileId, Vec<LuaInferenceDiagnosticEvent>>,
    support_file_dependents: &mut HashMap<FileId, HashSet<FileId>>,
) {
    for step in fact.provenance() {
        events_by_file
            .entry(step.event.source.file_id)
            .or_default()
            .push(LuaInferenceDiagnosticEvent {
                event: step.event.clone(),
                fact: fact.clone(),
            });
        for support in step.support.iter() {
            support_file_dependents
                .entry(support.file_id())
                .or_default()
                .insert(owner_file_id);
        }
    }
}

fn plain_cache_fact(cache: &LuaTypeCache) -> LuaTypeFact {
    let typ = cache.as_type().clone();
    match cache {
        LuaTypeCache::DocType(_) => LuaTypeFact::from_normalized_parts(
            typ,
            LuaInferenceConfidence::Certain,
            Some(LuaInferenceProvenanceKind::ExplicitAnnotation),
            Arc::from([]),
        ),
        LuaTypeCache::InferType(LuaType::Unknown) => LuaTypeFact::unknown(),
        LuaTypeCache::InferType(LuaType::Any) => LuaTypeFact::from_normalized_parts(
            typ,
            LuaInferenceConfidence::Unknown,
            None,
            Arc::from([]),
        ),
        LuaTypeCache::InferType(_) => LuaTypeFact::from_normalized_parts(
            typ,
            LuaInferenceConfidence::Certain,
            Some(LuaInferenceProvenanceKind::ConcreteValue),
            Arc::from([]),
        ),
    }
}

pub fn get_real_type<'a>(db: &'a DbIndex, typ: &'a LuaType) -> Option<&'a LuaType> {
    get_real_type_with_depth(db, typ, 0)
}

fn get_real_type_with_depth<'a>(
    db: &'a DbIndex,
    typ: &'a LuaType,
    depth: u32,
) -> Option<&'a LuaType> {
    const MAX_RECURSION_DEPTH: u32 = 10;

    if depth >= MAX_RECURSION_DEPTH {
        return Some(typ);
    }

    match typ {
        LuaType::Ref(type_decl_id) => {
            let type_decl = db.get_type_index().get_type_decl(type_decl_id)?;
            if type_decl.is_alias() {
                return get_real_type_with_depth(db, type_decl.get_alias_ref()?, depth + 1);
            }
            Some(typ)
        }
        _ => Some(typ),
    }
}

// 第一个参数是否不应该视为 self
pub fn first_param_may_not_self(typ: &LuaType) -> bool {
    if typ.is_table()
        || matches!(
            typ,
            LuaType::TplRef(_) | LuaType::StrTplRef(_) | LuaType::Any
        )
    {
        return true;
    }

    if let LuaType::Union(u) = typ {
        return u.types().any(first_param_may_not_self);
    }
    false
}

#[cfg(test)]
mod batch_removal_tests {
    use glua_parser::{LuaKind, LuaSyntaxId, LuaSyntaxKind};
    use rowan::TextRange;

    use super::*;
    use crate::{LuaDeclId, LuaDefinitionId};

    fn file_id(id: u32) -> FileId {
        FileId::new(id)
    }

    fn owner(file_id: FileId, position: u32) -> LuaTypeOwner {
        LuaTypeOwner::Decl(LuaDeclId::new(file_id, position.into()))
    }

    fn source(file_id: FileId, position: u32) -> InFiled<LuaSyntaxId> {
        InFiled::new(
            file_id,
            LuaSyntaxId::new(
                LuaKind::Syntax(LuaSyntaxKind::LocalName),
                TextRange::new(position.into(), (position + 1).into()),
            ),
        )
    }

    fn metadata(
        fact_owner: LuaTypeOwner,
        source_file: FileId,
        support_file: FileId,
    ) -> LuaTypeFactMetadata {
        LuaTypeFactMetadata {
            confidence: LuaInferenceConfidence::Anchored,
            base_provenance_kind: None,
            provenance: Arc::from([LuaInferenceStep {
                event: LuaInferenceEventId {
                    node: LuaInferenceNodeId::TypeOwner(fact_owner),
                    kind: LuaInferenceProvenanceKind::ContextualUnknown,
                    source: source(source_file, 20),
                },
                found_type: None,
                support: Arc::from([LuaInferenceNodeId::TypeOwner(owner(support_file, 30))]),
            }]),
        }
    }

    fn add_type(index: &mut LuaTypeIndex, file_id: FileId, name: &str) {
        index.add_type_decl(
            file_id,
            LuaTypeDecl::new(
                file_id,
                TextRange::new(0.into(), 1.into()),
                name.to_owned(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::None.into(),
                LuaTypeDeclId::global(name),
            ),
        );
    }

    fn populated_index() -> LuaTypeIndex {
        let first = file_id(1);
        let second = file_id(2);
        let survivor = file_id(3);
        let source_file = file_id(4);
        let mut index = LuaTypeIndex::new();

        index.add_file_namespace(first, "first".to_owned());
        index.add_file_using_namespace(second, "second".to_owned());
        index.add_file_namespace(survivor, "survivor".to_owned());
        add_type(&mut index, first, "Removed");
        add_type(&mut index, second, "Shared");
        add_type(&mut index, survivor, "Shared");
        add_type(&mut index, survivor, "Survivor");

        let shared = LuaTypeDeclId::global("Shared");
        index.add_super_type(shared.clone(), second, LuaType::String);
        index.add_super_type(shared, survivor, LuaType::Number);

        let first_owner = owner(first, 10);
        let second_owner = owner(second, 10);
        let survivor_owner = owner(survivor, 10);
        index.force_bind_type_fact(
            first_owner.clone(),
            LuaTypeCache::InferType(LuaType::String),
            metadata(first_owner, source_file, first),
        );
        index.force_bind_type_fact(
            second_owner.clone(),
            LuaTypeCache::InferType(LuaType::Number),
            metadata(second_owner, source_file, second),
        );
        index.force_bind_type_fact(
            survivor_owner.clone(),
            LuaTypeCache::InferType(LuaType::Boolean),
            metadata(survivor_owner, source_file, first),
        );

        index.bind_definition_fact(
            LuaDefinitionId::Declaration(LuaDeclId::new(first, 40.into())),
            LuaTypeFact::certain(LuaType::String),
        );
        index.bind_definition_fact(
            LuaDefinitionId::Declaration(LuaDeclId::new(survivor, 40.into())),
            LuaTypeFact::certain(LuaType::Number),
        );
        index
    }

    fn type_cache_types(index: &LuaTypeIndex) -> HashMap<LuaTypeOwner, LuaType> {
        index
            .types
            .iter()
            .map(|(owner, cache)| (owner.clone(), cache.as_type().clone()))
            .collect()
    }

    fn assert_same_removal_state(left: &LuaTypeIndex, right: &LuaTypeIndex) {
        assert_eq!(left.file_namespace, right.file_namespace);
        assert_eq!(left.file_using_namespace, right.file_using_namespace);
        assert_eq!(left.file_types, right.file_types);
        assert_eq!(left.full_name_type_map, right.full_name_type_map);
        assert_eq!(left.generic_params, right.generic_params);
        assert_eq!(left.supers, right.supers);
        assert_eq!(type_cache_types(left), type_cache_types(right));
        assert_eq!(left.in_filed_type_owner, right.in_filed_type_owner);
        assert_eq!(left.fact_metadata, right.fact_metadata);
        assert_eq!(left.definition_facts, right.definition_facts);
        assert_eq!(
            left.inference_events_by_file,
            right.inference_events_by_file
        );
        assert_eq!(left.support_file_dependents, right.support_file_dependents);
    }

    #[test]
    fn batch_removal_matches_sequential_removal_for_surviving_inference_state() {
        let first = file_id(1);
        let second = file_id(2);
        let survivor = file_id(3);
        let source_file = file_id(4);
        let survivor_owner = owner(survivor, 10);
        let survivor_definition = LuaDefinitionId::Declaration(LuaDeclId::new(survivor, 40.into()));

        let mut sequential = populated_index();
        sequential.remove(first);
        sequential.remove(second);

        let mut batched = populated_index();
        batched.remove_files(&[second, first, second]);

        assert_same_removal_state(&sequential, &batched);
        assert_eq!(
            batched.get_type_fact(&survivor_owner).unwrap().typ(),
            &LuaType::Boolean
        );
        assert_eq!(
            batched.get_definition_fact(&survivor_definition),
            Some(&LuaTypeFact::certain(LuaType::Number))
        );
        assert_eq!(batched.get_inference_events_for_file(source_file).len(), 1);
        assert_eq!(
            batched.files_depending_on_inference_support(&HashSet::from([first])),
            HashSet::from([survivor])
        );
    }
}
