use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::{
    DbIndex, FileId, InFiled, InferFailReason, InferGuardRef, LuaGenericType, LuaIntersectionType,
    LuaMemberKey, LuaMemberOwner, LuaMergedTableType, LuaObjectType, LuaOperatorMetaMethod,
    LuaOperatorOwner, LuaSemanticDeclId, LuaType, LuaTypeDeclId, LuaUnionType, TypeOps,
    semantic::{
        InferGuard,
        generic::{TypeSubstitutor, instantiate_type_generic},
    },
};

use super::{FindMembersResult, LuaMemberInfo, intersect_member_types, member_key_as_type};
use rowan::TextRange;

/// The index operations of `prefix_type`, read by `file_id` when it is known.
pub fn find_index_operations(
    db: &DbIndex,
    file_id: Option<FileId>,
    prefix_type: &LuaType,
) -> FindMembersResult {
    find_index_operations_guard(db, prefix_type, file_id, &InferGuard::new())
}

pub fn find_index_operations_guard(
    db: &DbIndex,
    prefix_type: &LuaType,
    file_id: Option<FileId>,
    infer_guard: &InferGuardRef,
) -> FindMembersResult {
    match &prefix_type {
        LuaType::TableConst(in_filed) => find_index_table(db, in_filed),
        LuaType::Ref(decl_id) => find_index_custom_type(db, decl_id, file_id, infer_guard),
        LuaType::Def(decl_id) => find_index_custom_type(db, decl_id, file_id, infer_guard),
        LuaType::Array(array_type) => find_index_array(db, file_id, array_type.get_base()),
        LuaType::Object(object) => find_index_object(db, object),
        LuaType::Union(union) => find_index_union(db, union, file_id, infer_guard),
        LuaType::MergedTable(merged_table) => {
            find_index_merged_table(db, merged_table, file_id, infer_guard)
        }
        LuaType::Intersection(intersection) => {
            find_index_intersection(db, intersection, file_id, infer_guard)
        }
        LuaType::Generic(generic) => find_index_generic(db, generic, file_id, infer_guard),
        LuaType::TableGeneric(table_generic) => find_index_table_generic(db, table_generic),
        LuaType::Instance(inst) => {
            let base = inst.get_base();
            find_index_operations_guard(db, base, file_id, infer_guard)
        }
        LuaType::TableOf(inner) => find_index_operations_guard(db, inner, file_id, infer_guard),
        LuaType::ModuleRef(module_id) => {
            let module_info = db.get_module_index().get_module(*module_id);
            if let Some(module_info) = module_info
                && let Some(export_type) = &module_info.export_type
            {
                return find_index_operations_guard(db, export_type, file_id, infer_guard);
            }

            None
        }
        _ => None,
    }
}

fn find_index_table(db: &DbIndex, table_range: &InFiled<TextRange>) -> FindMembersResult {
    let mut members = Vec::new();

    // Check for metatable __index operators
    let metatable = db.get_metatable_index().get(table_range);
    if let Some(metatable) = metatable {
        let meta_owner = LuaOperatorOwner::Table(metatable.clone());
        if let Some(operator_ids) = db
            .get_operator_index()
            .get_operators(&meta_owner, LuaOperatorMetaMethod::Index)
        {
            for operator_id in operator_ids {
                if let Some(operator) = db.get_operator_index().get_operator(operator_id) {
                    let operand = operator.get_operand(db);
                    if let Ok(return_type) = operator.get_result(db) {
                        members.push(LuaMemberInfo {
                            property_owner_id: None,
                            key: LuaMemberKey::ExprType(operand),
                            typ: return_type,
                            feature: None,
                            overload_index: None,
                        });
                    }
                }
            }
        }
    } else {
        // Check for direct table members
        let member_owner = LuaMemberOwner::Element(table_range.clone());
        if let Some(table_members) = db.get_member_index().get_members(&member_owner) {
            for member in table_members {
                let Some(member_key_type) = member_key_as_type(member.get_key()) else {
                    continue;
                };

                let member_type = db
                    .get_type_index()
                    .get_type_cache(&member.get_id().into())
                    .map(|it| it.as_type().clone())
                    .unwrap_or(LuaType::Unknown);

                members.push(LuaMemberInfo {
                    property_owner_id: Some(LuaSemanticDeclId::Member(member.get_id())),
                    key: LuaMemberKey::ExprType(member_key_type),
                    typ: member_type,
                    feature: Some(member.get_feature()),
                    overload_index: None,
                });
            }
        }
    }

    if members.is_empty() {
        None
    } else {
        Some(members)
    }
}

fn find_index_custom_type(
    db: &DbIndex,
    prefix_type_id: &LuaTypeDeclId,
    file_id: Option<FileId>,
    infer_guard: &InferGuardRef,
) -> FindMembersResult {
    infer_guard.check(prefix_type_id).ok()?;
    let type_index = db.get_type_index();
    let type_decl = type_index.get_type_decl(prefix_type_id)?;

    if type_decl.is_alias() {
        if let Some(origin_type) = type_decl.get_alias_origin(db, None) {
            return find_index_operations_guard(db, &origin_type, file_id, infer_guard);
        }
        return None;
    }

    let mut members = Vec::new();

    // Check for __index operators
    if let Some(index_operator_ids) = db
        .get_operator_index()
        .get_operators(&prefix_type_id.clone().into(), LuaOperatorMetaMethod::Index)
    {
        for operator_id in index_operator_ids {
            if let Some(operator) = db.get_operator_index().get_operator(operator_id) {
                let operand = operator.get_operand(db);
                if let Ok(return_type) = operator.get_result(db) {
                    members.push(LuaMemberInfo {
                        property_owner_id: None,
                        key: LuaMemberKey::ExprType(operand),
                        typ: return_type,
                        feature: None,
                        overload_index: None,
                    });
                }
            }
        }
    }

    // Find index operations in super types
    if type_decl.is_class()
        && let Some(super_types) = type_index.get_super_types(prefix_type_id)
    {
        for super_type in super_types {
            if let Some(super_members) =
                find_index_operations_guard(db, &super_type, file_id, infer_guard)
            {
                members.extend(super_members);
            }
        }
    }

    if members.is_empty() {
        None
    } else {
        Some(members)
    }
}

fn find_index_array(db: &DbIndex, file_id: Option<FileId>, base: &LuaType) -> FindMembersResult {
    let mut members = Vec::new();

    let expression_type = if db.get_optional_file_emmyrc(file_id).strict.array_index {
        TypeOps::Union.apply(db, base, &LuaType::Nil)
    } else {
        base.clone()
    };

    // Array accepts integer indices
    members.push(LuaMemberInfo {
        property_owner_id: None,
        key: LuaMemberKey::ExprType(LuaType::Integer),
        typ: expression_type.clone(),
        feature: None,
        overload_index: None,
    });

    // Array accepts number indices (for compatibility)
    members.push(LuaMemberInfo {
        property_owner_id: None,
        key: LuaMemberKey::ExprType(LuaType::Number),
        typ: expression_type,
        feature: None,
        overload_index: None,
    });

    Some(members)
}

#[allow(unused)]
fn find_index_object(db: &DbIndex, object: &LuaObjectType) -> FindMembersResult {
    let mut members = Vec::new();

    let access_member_type = object.get_index_access();
    for (key, field) in access_member_type {
        members.push(LuaMemberInfo {
            property_owner_id: None,
            key: LuaMemberKey::ExprType(key.clone()),
            typ: field.clone(),
            feature: None,
            overload_index: None,
        });
    }

    if members.is_empty() {
        None
    } else {
        Some(members)
    }
}

fn find_index_union(
    db: &DbIndex,
    union: &LuaUnionType,
    file_id: Option<FileId>,
    infer_guard: &InferGuardRef,
) -> FindMembersResult {
    let mut members = Vec::new();

    for member in union.types() {
        if let Some(sub_members) = find_index_operations_guard(db, member, file_id, infer_guard) {
            members.extend(sub_members);
        }
    }

    if members.is_empty() {
        None
    } else {
        Some(members)
    }
}

fn find_index_merged_table(
    db: &DbIndex,
    merged_table: &LuaMergedTableType,
    file_id: Option<FileId>,
    infer_guard: &InferGuardRef,
) -> FindMembersResult {
    let mut members = Vec::new();

    for component in merged_table.get_types() {
        if let Some(component_members) =
            find_index_operations_guard(db, component, file_id, infer_guard)
        {
            members.extend(component_members);
        }
    }

    if members.is_empty() {
        None
    } else {
        Some(members)
    }
}

fn find_index_intersection(
    db: &DbIndex,
    intersection: &LuaIntersectionType,
    file_id: Option<FileId>,
    infer_guard: &InferGuardRef,
) -> FindMembersResult {
    let mut order: Vec<LuaMemberKey> = Vec::new();
    let mut members: HashMap<LuaMemberKey, LuaType> = HashMap::default();

    for member in intersection.get_types() {
        let Some(sub_members) = find_index_operations_guard(db, member, file_id, infer_guard)
        else {
            continue;
        };

        // Within a single component type, treat duplicate keys as overrides (first wins).
        let mut component_seen: HashSet<LuaMemberKey> = HashSet::default();
        for member in sub_members {
            if !component_seen.insert(member.key.clone()) {
                continue;
            }

            match members.entry(member.key.clone()) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    order.push(member.key.clone());
                    entry.insert(member.typ.clone());
                }
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    let merged =
                        intersect_member_types(db, entry.get().clone(), member.typ.clone());
                    entry.insert(merged);
                }
            }
        }
    }

    if members.is_empty() {
        None
    } else {
        let mut result: Vec<LuaMemberInfo> = Vec::new();
        for key in order {
            let Some(typ) = members.get(&key) else {
                continue;
            };
            result.push(LuaMemberInfo {
                property_owner_id: None,
                key,
                typ: typ.clone(),
                feature: None,
                overload_index: None,
            });
        }

        Some(result)
    }
}

fn find_index_generic(
    db: &DbIndex,
    generic: &LuaGenericType,
    file_id: Option<FileId>,
    infer_guard: &InferGuardRef,
) -> FindMembersResult {
    let base_type = generic.get_base_type();
    let type_decl_id = if let LuaType::Ref(id) = base_type {
        id
    } else {
        return None;
    };

    let generic_params = generic.get_params();
    let substitutor = TypeSubstitutor::from_type_array(generic_params.clone());
    let type_index = db.get_type_index();
    let type_decl = type_index.get_type_decl(&type_decl_id)?;

    if type_decl.is_alias() {
        if let Some(origin_type) = type_decl.get_alias_origin(db, Some(&substitutor)) {
            let instantiated_type = instantiate_type_generic(db, &origin_type, &substitutor);
            return find_index_operations_guard(db, &instantiated_type, file_id, infer_guard);
        }
        return None;
    }

    let mut members = Vec::new();

    // Check for __index operators with generic substitution
    let operator_index = db.get_operator_index();
    if let Some(index_operator_ids) =
        operator_index.get_operators(&type_decl_id.clone().into(), LuaOperatorMetaMethod::Index)
    {
        for index_operator_id in index_operator_ids {
            if let Some(index_operator) = operator_index.get_operator(index_operator_id) {
                let operand = index_operator.get_operand(db);
                let instantiated_operand = instantiate_type_generic(db, &operand, &substitutor);

                if let Ok(return_type) = index_operator.get_result(db) {
                    let instantiated_return_type =
                        instantiate_type_generic(db, &return_type, &substitutor);

                    members.push(LuaMemberInfo {
                        property_owner_id: None,
                        key: LuaMemberKey::ExprType(instantiated_operand),
                        typ: instantiated_return_type,
                        feature: None,
                        overload_index: None,
                    });
                }
            }
        }
    }

    // Find index operations in super types
    if let Some(supers) = type_index.get_super_types(&type_decl_id) {
        for super_type in supers {
            let instantiated_super = instantiate_type_generic(db, &super_type, &substitutor);
            if let Some(super_members) =
                find_index_operations_guard(db, &instantiated_super, file_id, infer_guard)
            {
                members.extend(super_members);
            }
        }
    }

    if members.is_empty() {
        None
    } else {
        Some(members)
    }
}

#[allow(unused)]
fn find_index_table_generic(db: &DbIndex, table_params: &[LuaType]) -> FindMembersResult {
    if table_params.len() != 2 {
        return None;
    }

    let mut members = Vec::new();
    let key_type = &table_params[0];
    let value_type = &table_params[1];

    members.push(LuaMemberInfo {
        property_owner_id: None,
        key: LuaMemberKey::ExprType(key_type.clone()),
        typ: value_type.clone(),
        feature: None,
        overload_index: None,
    });

    Some(members)
}

/// What a read of `key` that `table` lacks finds through its metatable's
/// `__index`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MetatableIndexField {
    /// Nothing answers the key: there is no `__index`, or the one it has is a
    /// table without the key.
    Absent,
    /// The `__index` answers the key with a value of this type.
    Present(LuaType),
    /// The `__index` answers some keys, but which cannot be told.
    Unknown,
}

/// What `table`'s metatable `__index` supplies for `key`, as a read through it
/// resolves: every outcome the read can reach through `__index` values, each
/// value answering with its own field when it has one and through its own
/// metatable when it lacks it, and a function with its declared result for a
/// key its key parameter takes. A function's inferred result merges what it
/// returns for every key, so it says nothing of one key's value.
pub(crate) fn find_metatable_index_field(
    db: &DbIndex,
    table: &InFiled<TextRange>,
    key: &LuaMemberKey,
) -> MetatableIndexField {
    let mut lookup = IndexLookup {
        db,
        file_id: Some(table.file_id),
        key,
        tables: HashSet::default(),
        aliases: HashSet::default(),
        found: Vec::new(),
        absent: false,
        unknown: false,
    };
    lookup.through_metatable(table);
    lookup.field()
}

/// One key's read through a graph of `__index` values, gathering the outcomes
/// it reaches. Joining outcomes is order free and a value reached again adds
/// nothing, so each table and alias is looked through once, a cycle included.
struct IndexLookup<'a> {
    db: &'a DbIndex,
    file_id: Option<FileId>,
    key: &'a LuaMemberKey,
    tables: HashSet<InFiled<TextRange>>,
    aliases: HashSet<LuaTypeDeclId>,
    found: Vec<LuaType>,
    absent: bool,
    unknown: bool,
}

impl IndexLookup<'_> {
    fn add(&mut self, field: MetatableIndexField) {
        match field {
            MetatableIndexField::Present(typ) => self.found.push(typ),
            MetatableIndexField::Absent => self.absent = true,
            MetatableIndexField::Unknown => self.unknown = true,
        }
    }

    /// The field the gathered outcomes make: a value found on some reads and
    /// not on others may be nil.
    fn field(mut self) -> MetatableIndexField {
        if self.unknown {
            return MetatableIndexField::Unknown;
        }
        if self.found.is_empty() {
            return MetatableIndexField::Absent;
        }
        if self.absent {
            self.found.push(LuaType::Nil);
        }
        MetatableIndexField::Present(TypeOps::union_all(self.db, self.found))
    }

    fn through_metatable(&mut self, table: &InFiled<TextRange>) {
        if !self.tables.insert(table.clone()) {
            return;
        }
        let db = self.db;
        let Some(metatable) = db.get_metatable_index().get(table) else {
            self.add(MetatableIndexField::Absent);
            return;
        };
        if let Some(index_item) = db.get_member_index().get_member_item(
            &crate::semantic::metatable_member_owner(db, metatable),
            &LuaMemberKey::Name("__index".into()),
        ) {
            let Ok(index_type) = index_item.resolve_type(db) else {
                self.add(MetatableIndexField::Unknown);
                return;
            };
            if !index_type.is_function() {
                self.through_value(&index_type);
                return;
            }
        }
        let field = index_operator_field(db, metatable, self.key);
        self.add(field);
    }

    fn through_value(&mut self, index_type: &LuaType) {
        let db = self.db;
        match index_type {
            LuaType::Nil | LuaType::BooleanConst(false) => self.add(MetatableIndexField::Absent),
            LuaType::Union(union) => {
                for alternative in union.types() {
                    self.through_value(alternative);
                }
            }
            LuaType::TypeGuard(inner) => self.through_value(inner),
            LuaType::Ref(type_id) | LuaType::Def(type_id)
                if let Some(origin) = db
                    .get_type_index()
                    .get_type_decl(type_id)
                    .filter(|decl| decl.is_alias())
                    .and_then(|decl| decl.get_alias_origin(db, None)) =>
            {
                if self.aliases.insert(type_id.clone()) {
                    self.through_value(&origin);
                }
            }
            LuaType::TableConst(table) => {
                match super::infer_raw_member_type(db, self.file_id, index_type, self.key) {
                    Ok(typ) => self.add(MetatableIndexField::Present(typ)),
                    Err(InferFailReason::FieldNotFound) => self.through_metatable(table),
                    Err(_) => self.add(MetatableIndexField::Unknown),
                }
            }
            // A table given a metatable whose `__index` is the instance's base:
            // its own fields first, then what the metatable it holds answers,
            // or the base when none is indexed for it.
            LuaType::Instance(instance) => {
                let range = instance.get_range();
                let own = LuaType::TableConst(range.clone());
                match super::infer_raw_member_type(db, self.file_id, &own, self.key) {
                    Ok(typ) => self.add(MetatableIndexField::Present(typ)),
                    Err(InferFailReason::FieldNotFound)
                        if db.get_metatable_index().get(range).is_some() =>
                    {
                        self.through_metatable(range)
                    }
                    Err(InferFailReason::FieldNotFound) => self.through_value(instance.get_base()),
                    Err(_) => self.add(MetatableIndexField::Unknown),
                }
            }
            _ => {
                let field =
                    match super::infer_raw_member_type(db, self.file_id, index_type, self.key) {
                        Ok(typ) => MetatableIndexField::Present(typ),
                        Err(InferFailReason::FieldNotFound) => {
                            index_signature_field(db, self.file_id, index_type, self.key)
                        }
                        Err(_) => MetatableIndexField::Unknown,
                    };
                self.add(field);
            }
        }
    }
}

/// What the `__index` functions of `metatable` answer for `key`: the declared
/// result of the first whose key parameter takes it.
fn index_operator_field(
    db: &DbIndex,
    metatable: &InFiled<TextRange>,
    key: &LuaMemberKey,
) -> MetatableIndexField {
    let Some(key_type) = member_key_as_type(key) else {
        return MetatableIndexField::Unknown;
    };
    let operator_index = db.get_operator_index();
    let Some(operator_ids) = operator_index.get_operators(
        &LuaOperatorOwner::Table(metatable.clone()),
        LuaOperatorMetaMethod::Index,
    ) else {
        return MetatableIndexField::Absent;
    };
    for operator in operator_ids
        .iter()
        .filter_map(|operator_id| operator_index.get_operator(operator_id))
    {
        if crate::check_type_compact(db, &operator.get_operand(db), &key_type).is_ok() {
            if !operator.declares_result(db) {
                return MetatableIndexField::Unknown;
            }
            return operator
                .get_result(db)
                .map_or(MetatableIndexField::Unknown, MetatableIndexField::Present);
        }
    }
    MetatableIndexField::Absent
}

/// What the index signatures of `typ`, a class or an instantiated generic
/// class, answer for a `key` none of its named members has.
fn index_signature_field(
    db: &DbIndex,
    file_id: Option<FileId>,
    typ: &LuaType,
    key: &LuaMemberKey,
) -> MetatableIndexField {
    if !matches!(typ, LuaType::Ref(_) | LuaType::Def(_) | LuaType::Generic(_)) {
        return MetatableIndexField::Absent;
    }
    let Some(key_type) = member_key_as_type(key) else {
        return MetatableIndexField::Unknown;
    };
    find_index_operations(db, file_id, typ)
        .into_iter()
        .flatten()
        .find(|operation| {
            matches!(&operation.key, LuaMemberKey::ExprType(index_key)
                if crate::check_type_compact(db, index_key, &key_type).is_ok())
        })
        .map_or(MetatableIndexField::Absent, |operation| {
            MetatableIndexField::Present(operation.typ)
        })
}
