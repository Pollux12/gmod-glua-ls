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
    DbIndex, FileId, InFiled, LuaDeclId, LuaMemberOwner,
    db_index::r#type::type_decl::LuaTypeIdentifier,
};
pub use generic_param::GenericParam;
pub use humanize_type::{
    DEFAULT_DETAIL_MEMBER_DISPLAY_COUNT, RenderLevel, format_union_type, humanize_member_key_name,
    humanize_type,
};
pub use inference_fact::*;
use rowan::{TextRange, TextSize};
// The type index is the hottest hashing site in the analyzer: `LuaTypeOwner`
// hashing alone was 3.9% of all CPU under the default SipHash.
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::sync::Arc;
pub use type_decl::{LuaDeclLocation, LuaDeclTypeKind, LuaTypeDecl, LuaTypeDeclId, LuaTypeFlag};
pub use type_ops::TypeOps;
pub(crate) use type_owner::is_undetermined_type;
pub use type_owner::{LuaTypeCache, LuaTypeOwner, is_informative_type};
pub use type_visit_trait::TypeVisitTrait;
pub use types::*;

#[derive(Debug, Clone)]
pub struct LuaResolvedAliasType {
    pub alias_id: Option<LuaTypeDeclId>,
    pub typ: LuaType,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(crate) struct LuaSuperType {
    pub(crate) source_range: TextRange,
    pub(crate) typ: LuaType,
}

pub fn resolve_alias_type(db: &DbIndex, typ: &LuaType) -> LuaResolvedAliasType {
    let mut visited_aliases = HashSet::default();
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

fn replace_table_consts_in_type<S: std::hash::BuildHasher>(
    typ: &LuaType,
    replacements: &std::collections::HashMap<InFiled<TextRange>, LuaType, S>,
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

/// Every table-literal range a type names, directly or nested.
///
/// The mirror of [`remap_table_ranges_in_type`]: a test can use it to assert
/// that no store was left holding a range the remap should have moved.
#[cfg(test)]
pub(crate) fn table_ranges_in_type(typ: &LuaType) -> Vec<InFiled<TextRange>> {
    let mut ranges = Vec::new();
    TypeVisitTrait::visit_type(typ, &mut |inner| match inner {
        LuaType::TableConst(range) => ranges.push(range.clone()),
        LuaType::Instance(instance) => ranges.push(instance.get_range().clone()),
        _ => {}
    });
    ranges
}

pub(crate) fn remap_table_ranges_in_type(
    typ: &LuaType,
    map: &rustc_hash::FxHashMap<InFiled<TextRange>, InFiled<TextRange>>,
) -> Option<LuaType> {
    match typ {
        LuaType::TableConst(old) => map.get(old).map(|new| LuaType::TableConst(new.clone())),
        LuaType::Instance(inst) => {
            let mut changed = false;
            let mut new_base = inst.get_base().clone();
            if let Some(nb) = remap_table_ranges_in_type(inst.get_base(), map) {
                new_base = nb;
                changed = true;
            }
            let mut new_range = inst.get_range().clone();
            if let Some(mapped) = map.get(inst.get_range()) {
                new_range = mapped.clone();
                changed = true;
            }
            if changed {
                Some(LuaType::Instance(Arc::new(
                    crate::db_index::r#type::types::LuaInstanceType::new(new_base, new_range),
                )))
            } else {
                None
            }
        }
        LuaType::Union(union) => {
            let mut changed = false;
            let new_types: Vec<LuaType> = union
                .into_vec()
                .into_iter()
                .map(|sub| {
                    remap_table_ranges_in_type(&sub, map)
                        .inspect(|_| changed = true)
                        .unwrap_or(sub)
                })
                .collect();
            changed.then(|| LuaType::from_vec(new_types))
        }
        LuaType::Intersection(inter) => {
            let mut changed = false;
            let new_types: Vec<LuaType> = inter
                .get_types()
                .iter()
                .map(|sub| {
                    remap_table_ranges_in_type(sub, map)
                        .inspect(|_| changed = true)
                        .unwrap_or_else(|| sub.clone())
                })
                .collect();
            changed.then(|| {
                LuaType::Intersection(Arc::new(crate::LuaIntersectionType::new(new_types)))
            })
        }
        LuaType::MergedTable(merged) => {
            let mut changed = false;
            let new_types: Vec<LuaType> = merged
                .get_types()
                .iter()
                .map(|sub| {
                    remap_table_ranges_in_type(sub, map)
                        .inspect(|_| changed = true)
                        .unwrap_or_else(|| sub.clone())
                })
                .collect();
            changed
                .then(|| LuaType::MergedTable(Arc::new(crate::LuaMergedTableType::new(new_types))))
        }
        LuaType::Array(arr) => remap_table_ranges_in_type(arr.get_base(), map).map(|new_base| {
            LuaType::Array(Arc::new(crate::LuaArrayType::new(
                new_base,
                arr.get_len().clone(),
            )))
        }),
        LuaType::Tuple(tuple) => {
            let mut changed = false;
            let new_types: Vec<LuaType> = tuple
                .get_types()
                .iter()
                .map(|sub| {
                    remap_table_ranges_in_type(sub, map)
                        .inspect(|_| changed = true)
                        .unwrap_or_else(|| sub.clone())
                })
                .collect();
            if changed {
                Some(LuaType::Tuple(Arc::new(crate::LuaTupleType::new(
                    new_types,
                    tuple.status,
                ))))
            } else {
                None
            }
        }
        LuaType::Object(obj) => {
            let mut changed = false;
            let mut new_fields = std::collections::BTreeMap::new();
            for (k, v) in obj.get_fields() {
                if let Some(nv) = remap_table_ranges_in_type(v, map) {
                    changed = true;
                    new_fields.insert(k.clone(), nv);
                } else {
                    new_fields.insert(k.clone(), v.clone());
                }
            }
            let mut new_index_access = Vec::new();
            for (k, v) in obj.get_index_access() {
                let nk = remap_table_ranges_in_type(k, map).unwrap_or_else(|| k.clone());
                let nv = remap_table_ranges_in_type(v, map).unwrap_or_else(|| v.clone());
                if &nk != k || &nv != v {
                    changed = true;
                }
                new_index_access.push((nk, nv));
            }
            if changed {
                Some(LuaType::Object(Arc::new(
                    crate::LuaObjectType::new_with_fields(new_fields, new_index_access),
                )))
            } else {
                None
            }
        }
        LuaType::Generic(r#gen) => {
            let mut changed = false;
            let new_params: Vec<LuaType> = r#gen
                .get_params()
                .iter()
                .map(|p| {
                    remap_table_ranges_in_type(p, map)
                        .inspect(|_| changed = true)
                        .unwrap_or_else(|| p.clone())
                })
                .collect();
            if changed {
                Some(LuaType::Generic(Arc::new(crate::LuaGenericType::new(
                    r#gen.get_base_type_id(),
                    new_params,
                ))))
            } else {
                None
            }
        }
        LuaType::TableGeneric(params) => {
            let mut changed = false;
            let new_params: Vec<LuaType> = params
                .iter()
                .map(|p| {
                    remap_table_ranges_in_type(p, map)
                        .inspect(|_| changed = true)
                        .unwrap_or_else(|| p.clone())
                })
                .collect();
            changed.then(|| LuaType::TableGeneric(Arc::new(new_params)))
        }
        LuaType::DocFunction(func) => {
            let mut changed = false;
            let mut new_params = Vec::new();
            for (name, ty) in func.get_params() {
                if let Some(ty) = ty {
                    if let Some(nt) = remap_table_ranges_in_type(ty, map) {
                        changed = true;
                        new_params.push((name.clone(), Some(nt)));
                    } else {
                        new_params.push((name.clone(), Some(ty.clone())));
                    }
                } else {
                    new_params.push((name.clone(), None));
                }
            }
            let new_ret = remap_table_ranges_in_type(func.get_ret(), map)
                .inspect(|_| changed = true)
                .unwrap_or_else(|| func.get_ret().clone());
            if &new_ret != func.get_ret() {
                changed = true;
            }
            if changed {
                let new_func = crate::LuaFunctionType::new(
                    func.get_async_state(),
                    func.is_colon_define(),
                    func.is_variadic(),
                    new_params,
                    new_ret,
                )
                .with_optional_params(func.get_optional_params().to_vec())
                .with_call_arg_roles(func.get_call_arg_roles().to_vec());
                Some(LuaType::DocFunction(Arc::new(new_func)))
            } else {
                None
            }
        }
        LuaType::Variadic(var) => match var.as_ref() {
            crate::VariadicType::Multi(types) => {
                let mut changed = false;
                let new_types: Vec<LuaType> = types
                    .iter()
                    .map(|t| {
                        remap_table_ranges_in_type(t, map)
                            .inspect(|_| changed = true)
                            .unwrap_or_else(|| t.clone())
                    })
                    .collect();
                changed.then(|| LuaType::Variadic(Arc::new(crate::VariadicType::Multi(new_types))))
            }
            crate::VariadicType::Base(base) => remap_table_ranges_in_type(base, map)
                .map(|nb| LuaType::Variadic(Arc::new(crate::VariadicType::Base(nb)))),
        },
        LuaType::MultiLineUnion(mlu) => {
            let mut changed = false;
            let new_unions: Vec<(LuaType, Option<String>)> = mlu
                .get_unions()
                .iter()
                .map(|(ty, doc)| {
                    if let Some(nt) = remap_table_ranges_in_type(ty, map) {
                        changed = true;
                        (nt, doc.clone())
                    } else {
                        (ty.clone(), doc.clone())
                    }
                })
                .collect();
            changed.then(|| {
                LuaType::MultiLineUnion(Arc::new(crate::LuaMultiLineUnion::new(new_unions)))
            })
        }
        LuaType::TypeGuard(inner) => {
            remap_table_ranges_in_type(inner, map).map(|nt| LuaType::TypeGuard(Arc::new(nt)))
        }
        LuaType::Conditional(cond) => {
            let mut changed = false;
            let new_cond = remap_table_ranges_in_type(cond.get_condition(), map)
                .inspect(|_| changed = true)
                .unwrap_or_else(|| cond.get_condition().clone());
            let new_true = remap_table_ranges_in_type(cond.get_true_type(), map)
                .inspect(|_| changed = true)
                .unwrap_or_else(|| cond.get_true_type().clone());
            let new_false = remap_table_ranges_in_type(cond.get_false_type(), map)
                .inspect(|_| changed = true)
                .unwrap_or_else(|| cond.get_false_type().clone());
            if changed {
                Some(LuaType::Conditional(Arc::new(
                    crate::LuaConditionalType::new(
                        new_cond,
                        new_true,
                        new_false,
                        cond.get_infer_params().to_vec(),
                        cond.has_new,
                    ),
                )))
            } else {
                None
            }
        }
        LuaType::Mapped(mapped) => remap_table_ranges_in_type(&mapped.value, map).map(|nv| {
            LuaType::Mapped(Arc::new(crate::LuaMappedType::new(
                mapped.param.clone(),
                nv,
                mapped.is_readonly,
                mapped.is_optional,
            )))
        }),
        LuaType::TableOf(inner) => {
            remap_table_ranges_in_type(inner, map).map(|nt| LuaType::TableOf(Box::new(nt)))
        }
        LuaType::Call(call) => {
            let mut changed = false;
            let new_ops: Vec<LuaType> = call
                .get_operands()
                .iter()
                .map(|op| {
                    remap_table_ranges_in_type(op, map)
                        .inspect(|_| changed = true)
                        .unwrap_or_else(|| op.clone())
                })
                .collect();
            if changed {
                Some(LuaType::Call(Arc::new(crate::LuaAliasCallType::new(
                    call.get_call_kind(),
                    new_ops,
                ))))
            } else {
                None
            }
        }
        // The remaining variants nest no type that can carry a table literal's
        // range, so there is nothing under them to move.
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
    match typ {
        LuaType::Table
        | LuaType::TableConst(_)
        | LuaType::Object(_)
        | LuaType::MergedTable(_)
        | LuaType::TableGeneric(_)
        | LuaType::TableOf(_) => true,
        LuaType::Union(union) => union
            .types()
            .all(|t| matches!(t, LuaType::Nil) || is_table_assignment_merge_type(t)),
        LuaType::MultiLineUnion(multi) => multi
            .get_unions()
            .iter()
            .all(|(t, _)| matches!(t, LuaType::Nil) || is_table_assignment_merge_type(t)),
        _ => false,
    }
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

    merge_table_assignment_types(
        db,
        types
            .into_iter()
            .filter(|typ| !is_guarded_table_bootstrap_branch(db, typ))
            .collect(),
    )
}

fn collapse_guarded_table_bootstrap_branches(db: &DbIndex, types: Vec<LuaType>) -> LuaType {
    let mut saw_bootstrap = false;
    let mut bootstraps = Vec::new();
    let mut retained = Vec::with_capacity(types.len());

    for typ in types {
        if is_guarded_table_bootstrap_branch(db, &typ) {
            saw_bootstrap = true;
            bootstraps.push(typ);
        } else {
            retained.push(typ);
        }
    }

    if saw_bootstrap {
        if retained.is_empty() {
            // Nothing but bootstrap branches: they all name the same table, and
            // answering bare `table` would throw away the one thing they carry —
            // which literal that is. A slot with a single such writer keeps it
            // (the `One` arm returns the cache verbatim), so a slot with several
            // has to as well, or a member's owner would depend on how many
            // writers happened to be indexed when the read was taken.
            return LuaType::from_vec(bootstraps);
        }
        retained.push(LuaType::Table);
    }

    merge_table_assignment_types(db, retained)
}

/// Folds several writers' table types into one answer.
///
/// Table components merge rather than union: a slot several files each assign a
/// table literal holds one table at runtime, and every field any writer spells
/// is a field it can have. Bare `table` drops out whenever a more precise
/// component is present, since it names no field and would only dilute them.
pub(crate) fn merge_table_assignment_types(db: &DbIndex, types: Vec<LuaType>) -> LuaType {
    let mut table_components = Vec::new();
    let mut other_components = Vec::new();

    for typ in types {
        collect_guarded_table_merge_components(typ, &mut table_components, &mut other_components);
    }

    if table_components
        .iter()
        .any(|component| is_informative_guarded_table_branch(db, component))
    {
        table_components.retain(|component| is_informative_guarded_table_branch(db, component));
    } else if table_components
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
        LuaType::Union(union) => {
            for component in union.types() {
                collect_guarded_table_merge_components(
                    component.clone(),
                    table_components,
                    other_components,
                );
            }
        }
        LuaType::MultiLineUnion(multi_line) => {
            for (component, _) in multi_line.get_unions() {
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
        | LuaType::TableOf(_) => {
            if !table_components.contains(&typ) {
                table_components.push(typ);
            }
        }
        _ => {
            if !other_components.contains(&typ) {
                other_components.push(typ);
            }
        }
    }
}

fn is_informative_guarded_table_branch(db: &DbIndex, typ: &LuaType) -> bool {
    match typ {
        LuaType::TableConst(table_id) => {
            let member_index = db.get_member_index();
            let owner = LuaMemberOwner::Element(table_id.clone());
            if let Some(members) = member_index.get_members(&owner) {
                members
                    .iter()
                    .any(|m| matches!(m.get_key(), crate::LuaMemberKey::Name(_)))
            } else {
                false
            }
        }
        LuaType::Object(object) => !object.get_fields().is_empty(),
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

/// What a cached type points at, as tracked by [`TypeCacheRefIndex`].
///
/// Class references are keyed by declaration id rather than by file: a class's
/// definition sites move as files are indexed, so the file set behind a
/// `Decl` key is resolved from the live declaration at query time.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum TypeCacheRef {
    File(FileId),
    Decl(LuaTypeDeclId),
}

/// Reverse map of `referenced thing -> files whose cached types reference it`,
/// so incremental expansion is a lookup instead of a scan over every cache.
#[derive(Debug, Default, PartialEq, Eq)]
struct TypeCacheRefIndex {
    owner_refs: HashMap<FileId, HashMap<TypeCacheRef, u32>>,
    ref_owners: HashMap<TypeCacheRef, HashSet<FileId>>,
}

impl TypeCacheRefIndex {
    fn add(&mut self, owner_file_id: FileId, typ: &LuaType) {
        let owner_entry = self.owner_refs.entry(owner_file_id).or_default();
        for type_ref in collect_type_cache_refs(typ) {
            let count = owner_entry.entry(type_ref.clone()).or_insert(0);
            *count += 1;
            if *count == 1 {
                self.ref_owners
                    .entry(type_ref)
                    .or_default()
                    .insert(owner_file_id);
            }
        }
    }

    fn remove(&mut self, owner_file_id: FileId, typ: &LuaType) {
        let Some(owner_entry) = self.owner_refs.get_mut(&owner_file_id) else {
            return;
        };
        for type_ref in collect_type_cache_refs(typ) {
            let Some(count) = owner_entry.get_mut(&type_ref) else {
                continue;
            };
            *count -= 1;
            if *count > 0 {
                continue;
            }
            owner_entry.remove(&type_ref);
            if let Some(owners) = self.ref_owners.get_mut(&type_ref) {
                owners.remove(&owner_file_id);
                if owners.is_empty() {
                    self.ref_owners.remove(&type_ref);
                }
            }
        }

        if owner_entry.is_empty() {
            self.owner_refs.remove(&owner_file_id);
        }
    }

    fn remove_file(&mut self, owner_file_id: FileId) {
        let Some(owner_entry) = self.owner_refs.remove(&owner_file_id) else {
            return;
        };
        for type_ref in owner_entry.into_keys() {
            if let Some(owners) = self.ref_owners.get_mut(&type_ref) {
                owners.remove(&owner_file_id);
                if owners.is_empty() {
                    self.ref_owners.remove(&type_ref);
                }
            }
        }
    }

    fn owners(&self, type_ref: &TypeCacheRef) -> Option<&HashSet<FileId>> {
        self.ref_owners.get(type_ref)
    }
}

fn collect_type_cache_refs(typ: &LuaType) -> HashSet<TypeCacheRef> {
    let mut refs = HashSet::default();
    typ.visit_type(&mut |inner| {
        match inner {
            LuaType::TableConst(range) => {
                refs.insert(TypeCacheRef::File(range.file_id));
            }
            LuaType::Instance(instance) => {
                refs.insert(TypeCacheRef::File(instance.get_range().file_id));
            }
            LuaType::Signature(signature_id) => {
                refs.insert(TypeCacheRef::File(signature_id.get_file_id()));
            }
            LuaType::ModuleRef(file_id) => {
                refs.insert(TypeCacheRef::File(*file_id));
            }
            LuaType::Ref(type_id) | LuaType::Def(type_id) => {
                refs.insert(TypeCacheRef::Decl(type_id.clone()));
            }
            _ => {}
        };
    });

    refs
}

#[derive(Debug)]
pub struct LuaTypeIndex {
    file_namespace: HashMap<FileId, String>,
    file_using_namespace: HashMap<FileId, Vec<String>>,
    file_types: HashMap<FileId, Vec<LuaTypeDeclId>>,
    full_name_type_map: HashMap<LuaTypeDeclId, LuaTypeDecl>,
    generic_params: HashMap<LuaTypeDeclId, Vec<GenericParam>>,
    supers: HashMap<LuaTypeDeclId, Vec<InFiled<LuaSuperType>>>,
    types: HashMap<LuaTypeOwner, LuaTypeCache>,
    cache_refs: TypeCacheRefIndex,
    in_filed_type_owner: HashMap<FileId, HashSet<LuaTypeOwner>>,
    fact_metadata: HashMap<LuaTypeOwner, LuaTypeFactMetadata>,
    /// For each decl whose type a write has seeded: that write's source
    /// position, and whether its right-hand side was a call or index read. See
    /// `bind_decl_write`.
    decl_write_claims: HashMap<LuaDeclId, (TextSize, bool)>,
    /// Counts stored types that actually moved, so a caller can tell a no-op
    /// write from one that invalidates memoised inference.
    type_writes: u64,
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
            file_namespace: HashMap::default(),
            file_using_namespace: HashMap::default(),
            file_types: HashMap::default(),
            full_name_type_map: HashMap::default(),
            generic_params: HashMap::default(),
            supers: HashMap::default(),
            types: HashMap::default(),
            cache_refs: TypeCacheRefIndex::default(),
            in_filed_type_owner: HashMap::default(),
            fact_metadata: HashMap::default(),
            decl_write_claims: HashMap::default(),
            type_writes: 0,
            definition_facts: HashMap::default(),
            inference_events_by_file: HashMap::default(),
            support_file_dependents: HashMap::default(),
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
        let mut result = HashMap::default();
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

    pub fn add_super_type(
        &mut self,
        decl_id: LuaTypeDeclId,
        file_id: FileId,
        source_range: TextRange,
        super_type: LuaType,
    ) {
        self.supers.entry(decl_id).or_default().push(InFiled::new(
            file_id,
            LuaSuperType {
                source_range,
                typ: super_type,
            },
        ));
    }

    fn has_super_type_at_source(
        &self,
        decl_id: &LuaTypeDeclId,
        file_id: FileId,
        source_range: TextRange,
        super_type: &LuaType,
    ) -> bool {
        self.supers.get(decl_id).is_some_and(|supers| {
            supers.iter().any(|entry| {
                entry.file_id == file_id
                    && entry.value.source_range == source_range
                    && &entry.value.typ == super_type
            })
        })
    }

    pub fn add_super_type_if_missing(
        &mut self,
        decl_id: LuaTypeDeclId,
        file_id: FileId,
        source_range: TextRange,
        super_type: LuaType,
    ) {
        if self.has_super_type_at_source(&decl_id, file_id, source_range, &super_type) {
            return;
        }

        self.add_super_type(decl_id, file_id, source_range, super_type);
    }

    pub fn get_super_types(&self, decl_id: &LuaTypeDeclId) -> Option<Vec<LuaType>> {
        self.get_super_types_iter(decl_id)
            .map(|super_types| super_types.cloned().collect())
    }

    pub fn get_super_types_iter(
        &self,
        decl_id: &LuaTypeDeclId,
    ) -> Option<impl Iterator<Item = &LuaType> + '_> {
        self.supers.get(decl_id).map(|supers| {
            supers
                .iter()
                .enumerate()
                .filter_map(move |(index, super_type)| {
                    supers[..index]
                        .iter()
                        .all(|previous| previous.value.typ != super_type.value.typ)
                        .then_some(&super_type.value.typ)
                })
        })
    }

    pub(crate) fn get_super_type_entries(
        &self,
        decl_id: &LuaTypeDeclId,
    ) -> Option<&[InFiled<LuaSuperType>]> {
        self.supers.get(decl_id).map(Vec::as_slice)
    }

    /// Get all direct subclasses of a given type
    /// Returns a vector of type declarations that directly inherit from the given type
    pub fn get_sub_types(&self, decl_id: &LuaTypeDeclId) -> Vec<&LuaTypeDecl> {
        let mut sub_types = Vec::new();

        // Iterate through all types and check their super types
        for (type_id, supers) in &self.supers {
            for super_filed in supers {
                // Check if this super type references our target type
                if let LuaType::Ref(super_id) = &super_filed.value.typ {
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
        let mut visited = HashSet::default();
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

    /// Stores `cache` for `owner`, keeping any type already bound there unless
    /// `cache` supersedes it.
    ///
    /// A superseding write discards the old type, so the metadata derived from
    /// it has to go with it; otherwise [`get_type_fact`](Self::get_type_fact)
    /// pairs the new type with the old confidence and provenance.
    pub fn bind_type(&mut self, owner: LuaTypeOwner, cache: LuaTypeCache) {
        if let Some(existing) = self.types.get(&owner)
            && !cache.supersedes(existing)
        {
            return;
        }
        self.commit_type_cache(owner, cache);
    }

    /// See [`LuaTypeIndex::type_writes`]. Compare it across an operation to
    /// learn whether that operation moved any stored type.
    pub fn type_writes(&self) -> u64 {
        self.type_writes
    }

    /// The write that seeded `decl_id`'s type: its source position, and whether
    /// its right-hand side read through a call or index.
    pub fn decl_write_claim(&self, decl_id: &LuaDeclId) -> Option<(TextSize, bool)> {
        self.decl_write_claims.get(decl_id).copied()
    }

    pub fn record_decl_write_claim(
        &mut self,
        decl_id: LuaDeclId,
        position: TextSize,
        reads_through_call_or_index: bool,
    ) {
        self.decl_write_claims
            .insert(decl_id, (position, reads_through_call_or_index));
    }

    fn commit_type_cache(&mut self, owner: LuaTypeOwner, cache: LuaTypeCache) {
        let file_id = owner.get_file_id();
        let replaced = self.insert_type_cache(owner.clone(), cache);
        self.in_filed_type_owner
            .entry(file_id)
            .or_default()
            .insert(owner.clone());
        if replaced && self.fact_metadata.remove(&owner).is_some() {
            self.rebuild_inference_derived_state(&[file_id].into_iter().collect::<HashSet<_>>());
        }
    }

    /// The type-cache owners recorded for a file, used to re-derive a file's
    /// decls after a late index (e.g. vgui parent chains) makes a broad fallback
    /// resolvable.
    pub fn file_type_owners(&self, file_id: FileId) -> Option<&HashSet<LuaTypeOwner>> {
        self.in_filed_type_owner.get(&file_id)
    }

    pub fn get_file_type_decl_ids(&self, file_id: FileId) -> Option<&Vec<LuaTypeDeclId>> {
        self.file_types.get(&file_id)
    }

    pub fn force_bind_type(&mut self, owner: LuaTypeOwner, cache: LuaTypeCache) {
        let file_id = owner.get_file_id();
        self.insert_type_cache(owner.clone(), cache);
        self.in_filed_type_owner
            .entry(file_id)
            .or_default()
            .insert(owner.clone());
        if self.fact_metadata.remove(&owner).is_some() {
            self.rebuild_inference_derived_state(&[file_id].into_iter().collect::<HashSet<_>>());
        }
    }

    /// Fact-carrying counterpart of [`bind_type`](Self::bind_type); the same
    /// bottom-placeholder exception applies.
    pub fn bind_type_fact(
        &mut self,
        owner: LuaTypeOwner,
        cache: LuaTypeCache,
        metadata: LuaTypeFactMetadata,
    ) {
        if let Some(existing) = self.types.get(&owner)
            && !cache.supersedes(existing)
        {
            return;
        }

        let file_id = owner.get_file_id();
        let metadata = metadata.normalized();
        self.insert_type_cache(owner.clone(), cache);
        self.in_filed_type_owner
            .entry(file_id)
            .or_default()
            .insert(owner.clone());
        self.fact_metadata.insert(owner, metadata);
        self.rebuild_inference_derived_state(&[file_id].into_iter().collect::<HashSet<_>>());
    }

    pub fn force_bind_type_fact(
        &mut self,
        owner: LuaTypeOwner,
        cache: LuaTypeCache,
        metadata: LuaTypeFactMetadata,
    ) {
        let file_id = self.force_bind_type_fact_unchecked(owner, cache, metadata);
        self.rebuild_inference_derived_state(&[file_id].into_iter().collect::<HashSet<_>>());
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
        self.rebuild_inference_derived_state(&[file_id].into_iter().collect::<HashSet<_>>());
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

    pub fn files_depending_on_inference_support<S: std::hash::BuildHasher>(
        &self,
        file_ids: &std::collections::HashSet<FileId, S>,
    ) -> HashSet<FileId> {
        let mut dependents = HashSet::default();
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
        self.insert_type_cache(owner.clone(), cache);
        self.in_filed_type_owner
            .entry(file_id)
            .or_default()
            .insert(owner.clone());
        self.fact_metadata.insert(owner, metadata);
        file_id
    }

    /// Stores `cache`, keeping [`Self::cache_refs`] in step, and reports
    /// whether a cache was replaced.
    fn insert_type_cache(&mut self, owner: LuaTypeOwner, cache: LuaTypeCache) -> bool {
        if let Ok(want) = std::env::var("GLUALS_TRACE_WRITE") {
            let key = format!("{:?}", owner);
            if key.contains(&want) {
                eprintln!(
                    "WRITE {} <- {:?} (was {:?})",
                    key,
                    cache.as_type(),
                    self.types.get(&owner).map(|c| c.as_type().clone())
                );
                if std::env::var("GLUALS_TRACE_BT").is_ok() {
                    eprintln!("{}", std::backtrace::Backtrace::force_capture());
                }
            }
        }
        if self
            .types
            .get(&owner)
            .is_none_or(|existing| existing.as_type() != cache.as_type())
        {
            self.type_writes += 1;
        }
        let file_id = owner.get_file_id();
        self.cache_refs.add(file_id, cache.as_type());
        let Some(previous) = self.types.insert(owner, cache) else {
            return false;
        };
        self.cache_refs.remove(file_id, previous.as_type());
        true
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

    pub(crate) fn rebuild_inference_derived_state<S: std::hash::BuildHasher>(
        &mut self,
        changed_files: &std::collections::HashSet<FileId, S>,
    ) {
        if changed_files.is_empty() {
            return;
        }

        let mut events_by_file: HashMap<FileId, Vec<LuaInferenceDiagnosticEvent>> =
            HashMap::default();
        let mut support_file_dependents = HashMap::default();

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

        let mut changed_files = HashSet::default();
        for (owner, new_cache) in updates {
            changed_files.insert(owner.get_file_id());
            self.insert_type_cache(owner, new_cache);
        }
        self.rebuild_inference_derived_state(&changed_files);
    }

    pub fn replace_table_const_types<S: std::hash::BuildHasher>(
        &mut self,
        replacements: &std::collections::HashMap<InFiled<TextRange>, LuaType, S>,
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

        let mut changed_files = HashSet::default();
        for (owner, new_cache) in updates {
            changed_files.insert(owner.get_file_id());
            self.insert_type_cache(owner, new_cache);
        }
        self.rebuild_inference_derived_state(&changed_files);
    }

    /// Drops the cached types of members that were removed on their own,
    /// rather than as part of a file sweep.
    ///
    /// A member whose owning table literal is gone leaves a cache entry no
    /// re-index will reach, because the file it belongs to is not being
    /// re-analysed.
    pub fn remove_member_type_caches(&mut self, member_ids: &[crate::LuaMemberId]) {
        for member_id in member_ids {
            let owner = LuaTypeOwner::Member(*member_id);
            if let Some(set) = self.in_filed_type_owner.get_mut(&member_id.file_id) {
                set.remove(&owner);
            }
            // `cache_refs` has to come off with the cache, the way
            // `insert_type_cache` and the file sweep both keep them in step.
            if let Some(previous) = self.types.remove(&owner) {
                self.cache_refs
                    .remove(member_id.file_id, previous.as_type());
            }
            self.fact_metadata.remove(&owner);
        }
    }

    pub fn remap_table_const(
        &mut self,
        map: &rustc_hash::FxHashMap<InFiled<TextRange>, InFiled<TextRange>>,
    ) {
        if map.is_empty() {
            return;
        }
        // Only caches that actually name a table literal in one of the edited
        // files can contain a range the map moves, and `cache_refs` already
        // records which files those are. Scanning every cache in the workspace
        // here would put a full-index walk on the per-keystroke path.
        let source_files: HashSet<FileId> = map.keys().map(|range| range.file_id).collect();
        let candidate_owners: HashSet<&LuaTypeOwner> = source_files
            .iter()
            .filter_map(|file_id| self.cache_refs.owners(&TypeCacheRef::File(*file_id)))
            .flatten()
            .filter_map(|owner_file_id| self.in_filed_type_owner.get(owner_file_id))
            .flatten()
            .collect();

        let mut updates = Vec::new();
        for owner in candidate_owners {
            let Some(cache) = self.types.get(owner) else {
                continue;
            };
            if let Some(new_type) = remap_table_ranges_in_type(cache.as_type(), map) {
                let new_cache = match cache {
                    LuaTypeCache::DocType(_) => LuaTypeCache::DocType(new_type),
                    LuaTypeCache::InferType(_) => LuaTypeCache::InferType(new_type),
                };
                updates.push((owner.clone(), new_cache));
            }
        }
        // `candidate_owners` comes out of a hash set, so the writes are ordered
        // before they are applied.
        updates.sort_unstable_by_key(|(owner, _)| format!("{:?}", owner));
        let mut changed_files = HashSet::default();
        for (owner, new_cache) in updates {
            changed_files.insert(owner.get_file_id());
            self.insert_type_cache(owner, new_cache);
        }
        self.rebuild_inference_derived_state(&changed_files);
    }

    pub fn files_with_type_caches_referencing_files<S: std::hash::BuildHasher>(
        &self,
        file_ids: &std::collections::HashSet<FileId, S>,
    ) -> HashSet<FileId> {
        let mut dependent_files = HashSet::default();
        let mut visited_decls = HashSet::default();
        for file_id in file_ids {
            if let Some(owners) = self.cache_refs.owners(&TypeCacheRef::File(*file_id)) {
                dependent_files.extend(owners.iter().copied().filter(|o| !file_ids.contains(o)));
            }

            // A file that only *names* a class still has to be re-analysed when
            // a changed file is one of that class's definition sites: its
            // inference reads the class's full member set, which that file
            // contributes to.
            let Some(decl_ids) = self.file_types.get(file_id) else {
                continue;
            };
            for decl_id in decl_ids {
                if !visited_decls.insert(decl_id) {
                    continue;
                }

                let Some(owners) = self.cache_refs.owners(&TypeCacheRef::Decl(decl_id.clone()))
                else {
                    continue;
                };
                let Some(decl) = self.full_name_type_map.get(decl_id) else {
                    continue;
                };
                let locations = decl.get_locations();
                if !locations
                    .iter()
                    .any(|location| file_ids.contains(&location.file_id))
                {
                    continue;
                }

                dependent_files.extend(owners.iter().copied().filter(|owner_file_id| {
                    !file_ids.contains(owner_file_id)
                        && !locations
                            .iter()
                            .any(|location| location.file_id == *owner_file_id)
                }));
            }
        }

        #[cfg(feature = "verify_type_cache_refs")]
        assert_eq!(
            dependent_files,
            self.files_with_type_caches_referencing_files_by_scan(file_ids),
            "type cache reverse index disagrees with a full scan"
        );

        dependent_files
    }

    pub fn files_with_cross_file_type_caches_referencing_files<S: std::hash::BuildHasher>(
        &self,
        file_ids: &std::collections::HashSet<FileId, S>,
    ) -> HashSet<FileId> {
        let mut dependent_files = HashSet::default();
        for (owner, cache) in &self.types {
            let owner_file_id = owner.get_file_id();
            if self.type_references_other_file(cache.as_type(), file_ids, owner_file_id) {
                dependent_files.insert(owner_file_id);
            }
        }

        dependent_files
    }
    /// Reference implementation of
    /// [`files_with_type_caches_referencing_files`](Self::files_with_type_caches_referencing_files):
    /// the same answer by scanning every cached type. Kept so tests can pin the
    /// indexed lookup to it.
    #[cfg(any(test, feature = "verify_type_cache_refs"))]
    pub fn files_with_type_caches_referencing_files_by_scan<S: std::hash::BuildHasher>(
        &self,
        file_ids: &std::collections::HashSet<FileId, S>,
    ) -> HashSet<FileId> {
        let mut dependent_files = HashSet::default();
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

    #[cfg(any(test, feature = "verify_type_cache_refs"))]
    fn type_references_any_file<S: std::hash::BuildHasher>(
        &self,
        typ: &LuaType,
        file_ids: &std::collections::HashSet<FileId, S>,
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
                // A file that only *names* a class still has to be re-analysed
                // when a changed file is one of that class's definition sites:
                // its inference reads the class's full member set, which that
                // file contributes to. Dropping this arm under-invalidates
                // badly (measured: 300 removed / 332 added on CityRP).
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

    fn type_references_other_file<S: std::hash::BuildHasher>(
        &self,
        typ: &LuaType,
        file_ids: &std::collections::HashSet<FileId, S>,
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
        let mut changed_files = HashSet::default();
        for &file_id in file_ids {
            if changed_files.insert(file_id) {
                self.remove_file_raw(file_id);
            }
        }
        self.supers.retain(|_, supers| {
            supers.retain(|super_type| !changed_files.contains(&super_type.file_id));
            !supers.is_empty()
        });

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
        self.cache_refs = TypeCacheRefIndex::default();
        self.in_filed_type_owner.clear();
        self.fact_metadata.clear();
        self.decl_write_claims.clear();
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

                if remove_type {
                    self.generic_params.remove(&id);
                }
            }
        }

        if let Some(type_owners) = self.in_filed_type_owner.remove(&file_id) {
            for type_owner in type_owners {
                if let LuaTypeOwner::Decl(decl_id) = &type_owner {
                    self.decl_write_claims.remove(decl_id);
                }
                self.types.remove(&type_owner);
                self.fact_metadata.remove(&type_owner);
            }
        }

        self.cache_refs.remove_file(file_id);
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
mod super_type_tests {
    use rowan::TextRange;

    use super::*;

    /// A superseding write replaces the type, so the metadata derived from the
    /// discarded type must not survive to be reported against the new one.
    #[test]
    fn superseding_write_does_not_keep_the_discarded_types_metadata() {
        use crate::db_index::LuaDeclId;
        use crate::db_index::r#type::inference_fact::{
            LuaInferenceConfidence, LuaTypeFactMetadata,
        };

        let mut index = LuaTypeIndex::new();
        let owner: LuaTypeOwner = LuaDeclId::new(FileId::new(1), 0.into()).into();

        // A bottom placeholder carrying anchored evidence about the nil.
        index.bind_type_fact(
            owner.clone(),
            LuaTypeCache::InferType(LuaType::Nil),
            LuaTypeFactMetadata {
                confidence: LuaInferenceConfidence::Anchored,
                base_provenance_kind: None,
                provenance: std::sync::Arc::from([]),
            },
        );

        // A real type supersedes the placeholder.
        index.bind_type(owner.clone(), LuaTypeCache::InferType(LuaType::String));

        let fact = index.get_type_fact(&owner).expect("fact");
        assert_eq!(fact.typ(), &LuaType::String, "the new type must win");
        assert_ne!(
            fact.confidence(),
            LuaInferenceConfidence::Anchored,
            "confidence describing the discarded nil must not be reported for the new type"
        );
    }

    #[test]
    fn logical_super_type_accessors_deduplicate_source_edges() {
        let mut index = LuaTypeIndex::new();
        let child_id = LuaTypeDeclId::global("Child");
        let parent_type = LuaType::Ref(LuaTypeDeclId::global("Parent"));
        let edge_file = FileId::new(1);

        index.add_super_type(
            child_id.clone(),
            edge_file,
            TextRange::new(10.into(), 20.into()),
            parent_type.clone(),
        );
        index.add_super_type(
            child_id.clone(),
            edge_file,
            TextRange::new(30.into(), 40.into()),
            parent_type.clone(),
        );

        let source_edge_count = index
            .get_super_type_entries(&child_id)
            .map_or(0, <[_]>::len);
        let super_types = index.get_super_types(&child_id).unwrap_or_default();
        let iter_super_types = index
            .get_super_types_iter(&child_id)
            .map(|types| types.cloned().collect::<Vec<_>>())
            .unwrap_or_default();

        assert_eq!(source_edge_count, 2);
        assert_eq!(super_types, vec![parent_type.clone()]);
        assert_eq!(iter_super_types, vec![parent_type]);
    }
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
                inferred_type: None,
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
        index.add_super_type(
            shared.clone(),
            second,
            TextRange::default(),
            LuaType::String,
        );
        index.add_super_type(shared, survivor, TextRange::default(), LuaType::Number);

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

    #[test]
    fn removing_edge_only_file_removes_its_super_types() {
        let declaring_file = file_id(1);
        let edge_file = file_id(2);
        let type_id = LuaTypeDeclId::global("Shared");
        let mut index = LuaTypeIndex::new();
        add_type(&mut index, declaring_file, "Shared");
        index.add_super_type(
            type_id.clone(),
            edge_file,
            TextRange::new(10.into(), 20.into()),
            LuaType::String,
        );

        index.remove_files(&[edge_file]);

        assert!(index.get_super_type_entries(&type_id).is_none());
        assert!(index.get_type_decl(&type_id).is_some());
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
        assert_eq!(left.cache_refs, right.cache_refs);
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
            batched
                .files_depending_on_inference_support(&[first].into_iter().collect::<HashSet<_>>()),
            [survivor].into_iter().collect::<HashSet<_>>()
        );
    }
}
