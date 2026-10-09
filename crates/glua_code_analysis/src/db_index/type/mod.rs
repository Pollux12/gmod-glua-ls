mod generic_param;
mod humanize_type;
#[cfg(test)]
mod identity_tests;
mod inference_fact;
mod test;
mod type_decl;
mod type_ops;
mod type_owner;
mod type_visit_trait;
mod types;

use super::traits::LuaIndex;
use crate::db_index::cow_shards::{CowCell, CowShards, ShardKey};
use crate::db_index::derivation::{self, DerivationId, FactKey, Fingerprint};
use crate::{
    DbIndex, FileId, InFiled, LuaDeclId, LuaDeclSourceKey, LuaMemberOwner, LuaSignatureId,
    db_index::r#type::type_decl::LuaTypeIdentifier,
};
use flagset::FlagSet;
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
pub(crate) use type_decl::LuaTypeExtra;
pub use type_decl::{LuaDeclLocation, LuaDeclTypeKind, LuaTypeDecl, LuaTypeDeclId, LuaTypeFlag};
pub use type_ops::TypeOps;
pub(crate) use type_owner::is_undetermined_type;
pub(crate) use type_owner::widens_primitive;
pub use type_owner::{LuaTypeCache, LuaTypeOwner, is_informative_type, leaks_unsubstituted_tpl};
pub use type_visit_trait::TypeVisitTrait;
pub use types::*;

#[derive(Debug, Clone)]
pub struct LuaResolvedAliasType {
    pub alias_id: Option<LuaTypeDeclId>,
    pub typ: LuaType,
}

/// One declaration site's contribution to a shared `---@alias` payload.
///
/// Several files can declare the same alias name; each publishes its own
/// origin and generic params keyed by its name-token site, and the index
/// selects the effective pair atomically by [`LuaDeclSourceKey`] precedence
/// instead of letting the last writer win. The losing contributions stay
/// stored so removing the winner re-selects a survivor rather than dropping
/// the alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaAliasSource {
    /// The `---@alias` name-token site that published this contribution.
    pub site: InFiled<TextRange>,
    /// The origin type inferred at that site.
    pub origin: LuaType,
    /// The site's own generic params — `None` when the tag declared no
    /// `---@generic` list at all.
    pub generic_params: Option<Vec<GenericParam>>,
    /// Deterministic precedence; the smallest key wins.
    pub order_key: LuaDeclSourceKey,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub(crate) struct LuaSuperType {
    pub(crate) source_range: TextRange,
    pub(crate) typ: LuaType,
}

impl derivation::FilePosition for LuaSuperType {
    fn hash_in_file<H: std::hash::Hasher>(&self, file_id: FileId, state: &mut H) {
        use std::hash::Hash;
        self.source_range.hash_in_file(file_id, state);
        self.typ.hash(state);
    }
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

/// The table literals `typ` names where a registration can replace them.
fn collect_table_consts(typ: &LuaType, tables: &mut Vec<InFiled<TextRange>>) {
    match typ {
        LuaType::TableConst(range) => tables.push(range.clone()),
        LuaType::Union(union) => {
            for sub_type in union.into_vec() {
                collect_table_consts(&sub_type, tables);
            }
        }
        LuaType::Intersection(intersection) => {
            for sub_type in intersection.get_types() {
                collect_table_consts(sub_type, tables);
            }
        }
        LuaType::MergedTable(merged) => {
            for sub_type in merged.get_types() {
                collect_table_consts(sub_type, tables);
            }
        }
        _ => {}
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

/// What remapping one stored type through a [`crate::FileRemap`] found.
///
/// `typ` is the rewritten type when something in it moved; `lost` reports a
/// nested identity that did not survive. A value that is both moved and lost
/// reports `lost` with no rewrite: the holder keeps its stored value and its
/// owning file is re-analysed instead of being left on a guessed mix of old
/// and new coordinates.
#[derive(Debug, Clone)]
pub struct TypeRemapResult {
    pub typ: Option<LuaType>,
    pub lost: bool,
}

impl TypeRemapResult {
    fn unchanged() -> Self {
        Self {
            typ: None,
            lost: false,
        }
    }

    fn moved(typ: LuaType) -> Self {
        Self {
            typ: Some(typ),
            lost: false,
        }
    }

    fn lost() -> Self {
        Self {
            typ: None,
            lost: true,
        }
    }

    fn from_child_list(
        remapped: crate::Remap<Vec<LuaType>>,
        rebuild: impl FnOnce(Vec<LuaType>) -> LuaType,
    ) -> Self {
        match remapped {
            crate::Remap::Lost => Self::lost(),
            crate::Remap::Unrelated => Self::unchanged(),
            crate::Remap::Moved(types) => Self::moved(rebuild(types)),
        }
    }

    fn from_single(result: TypeRemapResult, rebuild: impl FnOnce(LuaType) -> LuaType) -> Self {
        if result.lost {
            Self::lost()
        } else if let Some(inner) = result.typ {
            Self::moved(rebuild(inner))
        } else {
            Self::unchanged()
        }
    }
}

/// Remaps one homogeneous child list, sharing the loop every composite arm
/// would otherwise repeat.
///
/// Returns `Lost` as soon as any child is lost (the caller publishes no
/// partial rewrite), `Moved` with the rebuilt child vec when anything moved,
/// and `Unrelated` when nothing changed. Nothing is cloned while the list is
/// unchanged: the prefix is cloned only once the first moved child appears.
fn remap_child_types<'a>(
    children: impl IntoIterator<
        Item = &'a LuaType,
        IntoIter: ExactSizeIterator<Item = &'a LuaType> + Clone,
    >,
    remap: &crate::FileRemap,
) -> crate::Remap<Vec<LuaType>> {
    let source = children.into_iter();
    let len = source.len();
    let mut rebuilt: Option<Vec<LuaType>> = None;
    for (idx, child) in source.clone().enumerate() {
        let result = remap_identities_in_type(child, remap);
        if result.lost {
            return crate::Remap::Lost;
        }
        match (result.typ, rebuilt.as_mut()) {
            (Some(moved), Some(vec)) => vec.push(moved),
            (Some(moved), None) => {
                let mut vec = Vec::with_capacity(len);
                vec.extend(source.clone().take(idx).cloned());
                vec.push(moved);
                rebuilt = Some(vec);
            }
            (None, Some(vec)) => vec.push(child.clone()),
            (None, None) => {}
        }
    }
    match rebuilt {
        Some(vec) => crate::Remap::Moved(vec),
        None => crate::Remap::Unrelated,
    }
}

fn remap_generic_params(
    params: &[GenericParam],
    remap: &crate::FileRemap,
) -> crate::Remap<Vec<GenericParam>> {
    let mut rebuilt = None;
    for (index, param) in params.iter().enumerate() {
        let Some(constraint) = &param.type_constraint else {
            continue;
        };
        let result = remap_identities_in_type(constraint, remap);
        if result.lost {
            return crate::Remap::Lost;
        }
        if let Some(typ) = result.typ {
            rebuilt.get_or_insert_with(|| params.to_vec())[index].type_constraint = Some(typ);
        }
    }
    match rebuilt {
        Some(params) => crate::Remap::Moved(params),
        None => crate::Remap::Unrelated,
    }
}

/// Rewrites every position-carrying identity inside a stored type.
///
/// Identities are resolved directly through `remap` (`signature_id`,
/// `table_range` for both `TableConst` and the `Instance` range), so a nested
/// identity no store enumerates is still moved or reported lost. `lost`
/// propagates out of every nested arm: a holder of a lost identity keeps its
/// stored value and its owning file is dirtied instead.
pub fn remap_identities_in_type(typ: &LuaType, remap: &crate::FileRemap) -> TypeRemapResult {
    match typ {
        LuaType::Signature(old) => match remap.signature_id(*old) {
            crate::Remap::Moved(new) if new != *old => {
                TypeRemapResult::moved(LuaType::Signature(new))
            }
            crate::Remap::Moved(_) | crate::Remap::Unrelated => TypeRemapResult::unchanged(),
            crate::Remap::Lost => TypeRemapResult::lost(),
        },
        LuaType::TableConst(old) => match remap.table_range(old) {
            crate::Remap::Moved(new) if new != *old => {
                TypeRemapResult::moved(LuaType::TableConst(new))
            }
            crate::Remap::Moved(_) | crate::Remap::Unrelated => TypeRemapResult::unchanged(),
            crate::Remap::Lost => TypeRemapResult::lost(),
        },
        LuaType::Instance(inst) => {
            let mut changed = false;
            let mut lost = false;
            let mut new_base = inst.get_base().clone();
            let base = remap_identities_in_type(inst.get_base(), remap);
            if base.lost {
                lost = true;
            } else if let Some(nb) = base.typ {
                new_base = nb;
                changed = true;
            }
            let mut new_range = inst.get_range().clone();
            match remap.table_range(inst.get_range()) {
                crate::Remap::Moved(mapped) if mapped != *inst.get_range() => {
                    new_range = mapped;
                    changed = true;
                }
                crate::Remap::Moved(_) | crate::Remap::Unrelated => {}
                crate::Remap::Lost => lost = true,
            }
            if lost {
                TypeRemapResult::lost()
            } else if changed {
                TypeRemapResult::moved(LuaType::Instance(Arc::new(
                    crate::db_index::r#type::types::LuaInstanceType::new(new_base, new_range),
                )))
            } else {
                TypeRemapResult::unchanged()
            }
        }
        LuaType::Union(union) => match union.as_ref() {
            crate::LuaUnionType::Multi(types) => TypeRemapResult::from_child_list(
                remap_child_types(types.iter(), remap),
                LuaType::from_vec,
            ),
            crate::LuaUnionType::Nullable(inner) => {
                TypeRemapResult::from_single(remap_identities_in_type(inner, remap), |new_inner| {
                    LuaType::from_vec(vec![new_inner, LuaType::Nil])
                })
            }
        },
        LuaType::Intersection(inter) => TypeRemapResult::from_child_list(
            remap_child_types(inter.get_types().iter(), remap),
            |new_types| LuaType::Intersection(Arc::new(crate::LuaIntersectionType::new(new_types))),
        ),
        LuaType::MergedTable(merged) => TypeRemapResult::from_child_list(
            remap_child_types(merged.get_types().iter(), remap),
            |new_types| LuaType::MergedTable(Arc::new(crate::LuaMergedTableType::new(new_types))),
        ),
        LuaType::Array(arr) => TypeRemapResult::from_single(
            remap_identities_in_type(arr.get_base(), remap),
            |new_base| {
                LuaType::Array(Arc::new(crate::LuaArrayType::new(
                    new_base,
                    arr.get_len().clone(),
                )))
            },
        ),
        LuaType::Tuple(tuple) => TypeRemapResult::from_child_list(
            remap_child_types(tuple.get_types().iter(), remap),
            |new_types| LuaType::Tuple(Arc::new(crate::LuaTupleType::new(new_types, tuple.status))),
        ),
        LuaType::Object(obj) => {
            let mut changed = false;
            let mut lost = false;
            let mut new_fields = std::collections::BTreeMap::new();
            for (k, v) in obj.get_fields() {
                let new_key = if let crate::LuaMemberKey::ExprType(typ) = k {
                    let result = remap_identities_in_type(typ, remap);
                    lost |= result.lost;
                    if let Some(typ) = result.typ {
                        changed = true;
                        crate::LuaMemberKey::ExprType(typ)
                    } else {
                        k.clone()
                    }
                } else {
                    k.clone()
                };
                let remapped = remap_identities_in_type(v, remap);
                if remapped.lost {
                    lost = true;
                }
                if let Some(nv) = remapped.typ {
                    changed = true;
                    new_fields.insert(new_key, nv);
                } else {
                    new_fields.insert(new_key, v.clone());
                }
            }
            let mut new_index_access = Vec::with_capacity(obj.get_index_access().len());
            for (k, v) in obj.get_index_access() {
                let remapped_k = remap_identities_in_type(k, remap);
                let remapped_v = remap_identities_in_type(v, remap);
                if remapped_k.lost || remapped_v.lost {
                    lost = true;
                }
                let nk = remapped_k.typ.unwrap_or_else(|| k.clone());
                let nv = remapped_v.typ.unwrap_or_else(|| v.clone());
                if &nk != k || &nv != v {
                    changed = true;
                }
                new_index_access.push((nk, nv));
            }
            if lost {
                TypeRemapResult::lost()
            } else if changed {
                TypeRemapResult::moved(LuaType::Object(Arc::new(
                    crate::LuaObjectType::new_with_fields(new_fields, new_index_access),
                )))
            } else {
                TypeRemapResult::unchanged()
            }
        }
        LuaType::Generic(r#gen) => TypeRemapResult::from_child_list(
            remap_child_types(r#gen.get_params().iter(), remap),
            |new_params| {
                LuaType::Generic(Arc::new(crate::LuaGenericType::new(
                    r#gen.get_base_type_id(),
                    new_params,
                )))
            },
        ),
        LuaType::TableGeneric(params) => TypeRemapResult::from_child_list(
            remap_child_types(params.iter(), remap),
            |new_params| LuaType::TableGeneric(Arc::new(new_params)),
        ),
        LuaType::DocFunction(func) => {
            let mut changed = false;
            let mut lost = false;
            let mut new_params = Vec::with_capacity(func.get_params().len());
            for (name, ty) in func.get_params() {
                if let Some(ty) = ty {
                    let remapped = remap_identities_in_type(ty, remap);
                    if remapped.lost {
                        lost = true;
                    }
                    if let Some(nt) = remapped.typ {
                        changed = true;
                        new_params.push((name.clone(), Some(nt)));
                    } else {
                        new_params.push((name.clone(), Some(ty.clone())));
                    }
                } else {
                    new_params.push((name.clone(), None));
                }
            }
            let remapped_ret = remap_identities_in_type(func.get_ret(), remap);
            if remapped_ret.lost {
                lost = true;
            }
            let new_ret = remapped_ret.typ.unwrap_or_else(|| func.get_ret().clone());
            if &new_ret != func.get_ret() {
                changed = true;
            }
            if lost {
                TypeRemapResult::lost()
            } else if changed {
                let new_func = crate::LuaFunctionType::new(
                    func.get_async_state(),
                    func.is_colon_define(),
                    func.is_variadic(),
                    new_params,
                    new_ret,
                )
                .with_optional_params(func.get_optional_params().to_vec())
                .with_call_arg_roles(func.get_call_arg_roles().to_vec());
                TypeRemapResult::moved(LuaType::DocFunction(Arc::new(new_func)))
            } else {
                TypeRemapResult::unchanged()
            }
        }
        LuaType::Variadic(var) => match var.as_ref() {
            crate::VariadicType::Multi(types) => TypeRemapResult::from_child_list(
                remap_child_types(types.iter(), remap),
                |new_types| LuaType::Variadic(Arc::new(crate::VariadicType::Multi(new_types))),
            ),
            crate::VariadicType::Base(base) => {
                TypeRemapResult::from_single(remap_identities_in_type(base, remap), |new_base| {
                    LuaType::Variadic(Arc::new(crate::VariadicType::Base(new_base)))
                })
            }
        },
        LuaType::MultiLineUnion(mlu) => {
            fn project(pair: &(LuaType, Option<String>)) -> &LuaType {
                &pair.0
            }
            TypeRemapResult::from_child_list(
                remap_child_types(mlu.get_unions().iter().map(project as fn(_) -> _), remap),
                |new_types| {
                    let unions = new_types
                        .into_iter()
                        .zip(mlu.get_unions().iter().map(|(_, doc)| doc.clone()))
                        .collect();
                    LuaType::MultiLineUnion(Arc::new(crate::LuaMultiLineUnion::new(unions)))
                },
            )
        }
        LuaType::TypeGuard(inner) => {
            TypeRemapResult::from_single(remap_identities_in_type(inner, remap), |new_inner| {
                LuaType::TypeGuard(Arc::new(new_inner))
            })
        }
        LuaType::Conditional(cond) => {
            let remapped_params = remap_generic_params(cond.get_infer_params(), remap);
            if matches!(remapped_params, crate::Remap::Lost) {
                return TypeRemapResult::lost();
            }
            let mut lost = false;
            let remapped_cond = remap_identities_in_type(cond.get_condition(), remap);
            let remapped_true = remap_identities_in_type(cond.get_true_type(), remap);
            let remapped_false = remap_identities_in_type(cond.get_false_type(), remap);
            if remapped_cond.lost || remapped_true.lost || remapped_false.lost {
                lost = true;
            }
            let new_cond = remapped_cond
                .typ
                .unwrap_or_else(|| cond.get_condition().clone());
            let new_true = remapped_true
                .typ
                .unwrap_or_else(|| cond.get_true_type().clone());
            let new_false = remapped_false
                .typ
                .unwrap_or_else(|| cond.get_false_type().clone());
            let changed = &new_cond != cond.get_condition()
                || &new_true != cond.get_true_type()
                || &new_false != cond.get_false_type()
                || matches!(remapped_params, crate::Remap::Moved(_));
            if lost {
                TypeRemapResult::lost()
            } else if changed {
                TypeRemapResult::moved(LuaType::Conditional(Arc::new(
                    crate::LuaConditionalType::new(
                        new_cond,
                        new_true,
                        new_false,
                        match remapped_params {
                            crate::Remap::Moved(params) => params,
                            _ => cond.get_infer_params().to_vec(),
                        },
                        cond.has_new,
                    ),
                )))
            } else {
                TypeRemapResult::unchanged()
            }
        }
        LuaType::Mapped(mapped) => {
            let params = remap_generic_params(std::slice::from_ref(&mapped.param.1), remap);
            let value = remap_identities_in_type(&mapped.value, remap);
            if matches!(params, crate::Remap::Lost) || value.lost {
                TypeRemapResult::lost()
            } else if matches!(params, crate::Remap::Moved(_)) || value.typ.is_some() {
                let param = match params {
                    crate::Remap::Moved(mut params) => (mapped.param.0, params.remove(0)),
                    _ => mapped.param.clone(),
                };
                TypeRemapResult::moved(LuaType::Mapped(Arc::new(crate::LuaMappedType::new(
                    param,
                    value.typ.unwrap_or_else(|| mapped.value.clone()),
                    mapped.is_readonly,
                    mapped.is_optional,
                ))))
            } else {
                TypeRemapResult::unchanged()
            }
        }
        LuaType::TplRef(tpl) | LuaType::ConstTplRef(tpl) => {
            let Some(constraint) = tpl.get_constraint() else {
                return TypeRemapResult::unchanged();
            };
            TypeRemapResult::from_single(
                remap_identities_in_type(constraint, remap),
                |constraint| {
                    let tpl = Arc::new(GenericTpl::new(
                        tpl.get_tpl_id(),
                        smol_str::SmolStr::new(tpl.get_name()).into(),
                        Some(constraint),
                    ));
                    if matches!(typ, LuaType::ConstTplRef(_)) {
                        LuaType::ConstTplRef(tpl)
                    } else {
                        LuaType::TplRef(tpl)
                    }
                },
            )
        }
        LuaType::StrTplRef(tpl) => {
            let Some(constraint) = tpl.get_constraint() else {
                return TypeRemapResult::unchanged();
            };
            TypeRemapResult::from_single(remap_identities_in_type(constraint, remap), |typ| {
                LuaType::StrTplRef(Arc::new(LuaStringTplType::new(
                    tpl.get_prefix(),
                    tpl.get_name(),
                    tpl.get_tpl_id(),
                    tpl.get_suffix(),
                    Some(typ),
                )))
            })
        }
        LuaType::DocAttribute(attribute) => {
            let mut params = None;
            for (index, (_, typ)) in attribute.get_params().iter().enumerate() {
                let Some(typ) = typ else { continue };
                let result = remap_identities_in_type(typ, remap);
                if result.lost {
                    return TypeRemapResult::lost();
                }
                if let Some(typ) = result.typ {
                    params.get_or_insert_with(|| attribute.get_params().to_vec())[index].1 =
                        Some(typ);
                }
            }
            match params {
                Some(params) => TypeRemapResult::moved(LuaType::DocAttribute(Arc::new(
                    LuaAttributeType::new(params),
                ))),
                None => TypeRemapResult::unchanged(),
            }
        }
        LuaType::TableOf(inner) => {
            TypeRemapResult::from_single(remap_identities_in_type(inner, remap), |new_inner| {
                LuaType::TableOf(Box::new(new_inner))
            })
        }
        LuaType::Call(call) => TypeRemapResult::from_child_list(
            remap_child_types(call.get_operands().iter(), remap),
            |new_ops| {
                LuaType::Call(Arc::new(crate::LuaAliasCallType::new(
                    call.get_call_kind(),
                    new_ops,
                )))
            },
        ),
        // The remaining variants nest no type that can carry a table literal's
        // range, so there is nothing under them to move. Listed rather than
        // matched with a wildcard so that a new `LuaType` variant fails to
        // compile here, which is what keeps this arm list in step with
        // `hash_type`.
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
        | LuaType::Never
        | LuaType::BooleanConst(_)
        | LuaType::StringConst(_)
        | LuaType::IntegerConst(_)
        | LuaType::FloatConst(_)
        | LuaType::DocStringConst(_)
        | LuaType::DocIntegerConst(_)
        | LuaType::DocBooleanConst(_)
        | LuaType::Ref(_)
        | LuaType::Def(_)
        | LuaType::Namespace(_)
        | LuaType::Language(_)
        | LuaType::ModuleRef(_)
        | LuaType::ConditionalInfer(_) => TypeRemapResult::unchanged(),
    }
}

/// `fact` with every identity of the edited file it names moved: its payload
/// type and its provenance. `None` when it names none; `lost` is set when one
/// did not survive.
pub(crate) fn remap_type_fact(
    fact: &LuaTypeFact,
    remap: &crate::FileRemap,
    lost: &mut bool,
) -> Option<LuaTypeFact> {
    let remapped = remap_identities_in_type(fact.typ(), remap);
    if remapped.lost {
        *lost = true;
    }
    let provenance = remap_provenance(fact.provenance(), remap, lost);
    (remapped.typ.is_some() || provenance.is_some()).then(|| {
        LuaTypeFact::from_normalized_parts(
            remapped.typ.unwrap_or_else(|| fact.typ().clone()),
            fact.confidence(),
            fact.base_provenance_kind(),
            provenance.unwrap_or_else(|| fact.provenance().into()),
        )
    })
}

/// The files `fact` names: its payload type on top of its provenance.
/// Exactly the files [`remap_type_fact`] consults, so a fact whose set skips a
/// file remaps to itself there.
pub(crate) fn type_fact_referenced_files(fact: &LuaTypeFact) -> HashSet<FileId> {
    let mut files = HashSet::default();
    collect_type_files(fact.typ(), &mut files);
    collect_provenance_files(fact.provenance().iter(), &mut files);
    files
}

/// Rewrites the nodes, source sites and types one fact's provenance names.
///
/// Returns `None` when nothing in it belonged to the edited file. `lost` is set
/// when something did belong to it and did not survive - the fact stays as it
/// is and its owner is re-analysed instead.
fn remap_provenance(
    provenance: &[LuaInferenceStep],
    remap: &crate::FileRemap,
    lost: &mut bool,
) -> Option<Arc<[LuaInferenceStep]>> {
    fn node(
        current: &LuaInferenceNodeId,
        remap: &crate::FileRemap,
        changed: &mut bool,
        lost: &mut bool,
    ) -> LuaInferenceNodeId {
        match remap.inference_node(current) {
            crate::Remap::Moved(new) => {
                if &new != current {
                    *changed = true;
                }
                new
            }
            crate::Remap::Unrelated => current.clone(),
            crate::Remap::Lost => {
                *lost = true;
                current.clone()
            }
        }
    }

    fn remap_step_type(
        current: &Arc<LuaType>,
        remap: &crate::FileRemap,
        changed: &mut bool,
        lost: &mut bool,
    ) -> Arc<LuaType> {
        let result = remap_identities_in_type(current, remap);
        if result.lost {
            *lost = true;
        }
        match result.typ {
            Some(new) => {
                *changed = true;
                Arc::new(new)
            }
            None => current.clone(),
        }
    }

    let mut changed = false;
    let mut steps: Vec<LuaInferenceStep> = Vec::with_capacity(provenance.len());
    for step in provenance {
        let event_node = node(&step.event.node, remap, &mut changed, lost);
        let source = match remap.syntax_id(step.event.source.file_id, step.event.source.value) {
            crate::Remap::Moved(value) => {
                if value != step.event.source.value {
                    changed = true;
                }
                InFiled::new(step.event.source.file_id, value)
            }
            crate::Remap::Unrelated => step.event.source.clone(),
            crate::Remap::Lost => {
                *lost = true;
                step.event.source.clone()
            }
        };
        let mut support: Vec<LuaInferenceNodeId> = step
            .support
            .iter()
            .map(|id| node(id, remap, &mut changed, lost))
            .collect();
        // The producers file support in `stable_cmp` order and a remapped
        // position can cross a neighbour, so the order is re-established
        // rather than inherited.
        support.sort_by(|left, right| left.stable_cmp(right));
        let inferred_type = step
            .inferred_type
            .as_ref()
            .map(|typ| remap_step_type(typ, remap, &mut changed, lost));
        let found_type = step
            .found_type
            .as_ref()
            .map(|typ| remap_step_type(typ, remap, &mut changed, lost));
        steps.push(LuaInferenceStep {
            event: LuaInferenceEventId {
                node: event_node,
                kind: step.event.kind,
                source,
            },
            support: support.into(),
            inferred_type,
            found_type,
        });
    }
    changed.then(|| steps.into())
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
            // writers happened to be indexed when the read was taken. They are
            // sorted by `(file id, range)` so the arm order — not just the arm
            // set — is independent of the order the writers were indexed in.
            bootstraps.sort_by_key(|typ| match typ {
                LuaType::TableConst(site) => Some((
                    site.file_id.id,
                    u32::from(site.value.start()),
                    u32::from(site.value.end()),
                )),
                _ => None,
            });
            return LuaType::from_vec(bootstraps);
        }
        retained.push(LuaType::Table);
    }

    merge_table_assignment_types(retained)
}

/// Folds several writers' table types into one answer.
///
/// Table components merge rather than union: a slot several files each assign a
/// table literal holds one table at runtime, and every field any writer spells
/// is a field it can have. Bare `table` drops out whenever a more precise
/// component is present, since it names no field and would only dilute them.
pub(crate) fn merge_table_assignment_types(types: Vec<LuaType>) -> LuaType {
    let mut table_components = Vec::new();
    let mut other_components = Vec::new();

    for typ in types {
        collect_guarded_table_merge_components(typ, &mut table_components, &mut other_components);
    }
    let mut seen: Vec<LuaType> = Vec::new();
    table_components.retain(|component| {
        if seen.contains(component) {
            return false;
        }
        seen.push(component.clone());
        true
    });

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

/// What a cached type points at, as tracked by [`TypeCacheRefIndex`].
///
/// Class references are keyed by declaration id rather than by file: a class's
/// definition sites move as files are indexed, so the file set behind a
/// `Decl` key is resolved from the live declaration at query time.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum TypeCacheRef {
    Table(InFiled<TextRange>),
    Instance(InFiled<TextRange>),
    Signature(LuaSignatureId),
    Module(FileId),
    Decl(LuaTypeDeclId),
}

impl TypeCacheRef {
    /// The file the referenced identity lives in, or `None` for a class, whose
    /// definition sites are resolved from the live declaration instead.
    pub fn file_id(&self) -> Option<FileId> {
        match self {
            Self::Table(range) | Self::Instance(range) => Some(range.file_id),
            Self::Signature(signature_id) => Some(signature_id.get_file_id()),
            Self::Module(file_id) => Some(*file_id),
            Self::Decl(_) => None,
        }
    }
}

/// Reverse map of `referenced thing -> files whose cached types reference it`,
/// so incremental expansion is a lookup instead of a scan over every cache.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct TypeCacheRefIndex {
    owner_refs: CowShards<FileId, HashMap<TypeCacheRef, u32>>,
    ref_owners: CowShards<TypeCacheRef, HashSet<FileId>>,
    refs_by_file: CowShards<FileId, HashSet<TypeCacheRef>>,
}

impl TypeCacheRefIndex {
    fn add(&mut self, owner_file_id: FileId, typ: &LuaType) {
        let owner_entry = self.owner_refs.entry(owner_file_id).or_default();
        for type_ref in collect_type_cache_refs(typ) {
            let count = owner_entry.entry(type_ref.clone()).or_insert(0);
            *count += 1;
            if *count == 1 {
                if let Some(file_id) = type_ref.file_id() {
                    self.refs_by_file
                        .entry(file_id)
                        .or_default()
                        .insert(type_ref.clone());
                }
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
        let mut dropped = Vec::new();
        for type_ref in collect_type_cache_refs(typ) {
            let Some(count) = owner_entry.get_mut(&type_ref) else {
                continue;
            };
            *count -= 1;
            if *count > 0 {
                continue;
            }
            owner_entry.remove(&type_ref);
            dropped.push(type_ref);
        }

        if owner_entry.is_empty() {
            self.owner_refs.remove(&owner_file_id);
        }

        for type_ref in dropped {
            self.drop_owner(&type_ref, owner_file_id);
        }
    }

    fn remove_file(&mut self, owner_file_id: FileId) {
        let Some(owner_entry) = self.owner_refs.remove(&owner_file_id) else {
            return;
        };
        for type_ref in owner_entry.into_keys() {
            self.drop_owner(&type_ref, owner_file_id);
        }
    }

    fn drop_owner(&mut self, type_ref: &TypeCacheRef, owner_file_id: FileId) {
        let Some(owners) = self.ref_owners.get_mut(type_ref) else {
            return;
        };
        owners.remove(&owner_file_id);
        if !owners.is_empty() {
            return;
        }
        self.ref_owners.remove(type_ref);
        let Some(file_id) = type_ref.file_id() else {
            return;
        };
        let Some(refs) = self.refs_by_file.get_mut(&file_id) else {
            return;
        };
        refs.remove(type_ref);
        if refs.is_empty() {
            self.refs_by_file.remove(&file_id);
        }
    }

    /// The `(owner file, reference)` counts `fork`, a task fork of `frozen`,
    /// changed: which owners hold a reference follows from the merged counts.
    fn refs_to_rebuild(&self, frozen: &Self, fork: &Self) -> Vec<(FileId, TypeCacheRef)> {
        CowShards::changed_keys(&frozen.owner_refs, &fork.owner_refs)
            .into_iter()
            .flat_map(|file_id| {
                let was = frozen.owner_refs.get(&file_id);
                let now = fork.owner_refs.get(&file_id);
                let count = |refs: Option<&HashMap<TypeCacheRef, u32>>, type_ref: &TypeCacheRef| {
                    refs.and_then(|refs| refs.get(type_ref)).copied()
                };
                was.into_iter()
                    .chain(now)
                    .flat_map(HashMap::keys)
                    .filter(|type_ref| count(was, type_ref) != count(now, type_ref))
                    .map(|type_ref| (file_id, type_ref.clone()))
                    .collect::<HashSet<_>>()
            })
            .collect()
    }

    fn rebuild_refs(&mut self, refs: Vec<(FileId, TypeCacheRef)>) {
        for (owner_file_id, type_ref) in refs {
            let held = self
                .owner_refs
                .get(&owner_file_id)
                .is_some_and(|refs| refs.contains_key(&type_ref));
            if !held {
                self.drop_owner(&type_ref, owner_file_id);
                continue;
            }
            if self
                .ref_owners
                .get(&type_ref)
                .is_some_and(|owners| owners.contains(&owner_file_id))
            {
                continue;
            }
            if let Some(file_id) = type_ref.file_id() {
                self.refs_by_file
                    .entry(file_id)
                    .or_default()
                    .insert(type_ref.clone());
            }
            self.ref_owners
                .entry(type_ref)
                .or_default()
                .insert(owner_file_id);
        }
    }

    fn owners(&self, type_ref: &TypeCacheRef) -> Option<&HashSet<FileId>> {
        self.ref_owners.get(type_ref)
    }

    fn refs_into_file(&self, file_id: FileId) -> impl Iterator<Item = &TypeCacheRef> {
        self.refs_by_file.get(&file_id).into_iter().flatten()
    }

    fn owners_referencing_file(&self, file_id: FileId) -> impl Iterator<Item = FileId> {
        let mut seen = HashSet::default();
        self.refs_into_file(file_id)
            .filter_map(|type_ref| self.ref_owners.get(type_ref))
            .flatten()
            .copied()
            .filter(move |owner_file_id| seen.insert(*owner_file_id))
    }
}

fn super_entry_sort_key(entry: &InFiled<LuaSuperType>) -> (FileId, TextSize, TextSize) {
    (
        entry.file_id,
        entry.value.source_range.start(),
        entry.value.source_range.end(),
    )
}

/// Merges two tasks' changes to one declaration's super edges, which stay in
/// source order.
fn merge_super_entries(
    current: &mut Option<Vec<InFiled<LuaSuperType>>>,
    frozen: Option<&Vec<InFiled<LuaSuperType>>>,
    fork: Option<&Vec<InFiled<LuaSuperType>>>,
) -> crate::db_index::cow_shards::Merged {
    crate::db_index::cow_shards::merge_sorted_by(current, frozen, fork, super_entry_sort_key)
}

/// The supertypes `entries` name, each once, in the order of its first edge.
fn distinct_super_types(entries: &[InFiled<LuaSuperType>]) -> impl Iterator<Item = &LuaType> + '_ {
    entries
        .iter()
        .enumerate()
        .filter_map(move |(index, entry)| {
            entries[..index]
                .iter()
                .all(|previous| previous.value.typ != entry.value.typ)
                .then_some(&entry.value.typ)
        })
}

/// The bases `supers` names a direct subclass of.
fn super_bases(
    supers: Option<&Vec<InFiled<LuaSuperType>>>,
) -> impl Iterator<Item = &LuaTypeDeclId> {
    supers
        .into_iter()
        .flatten()
        .filter_map(|entry| match &entry.value.typ {
            LuaType::Ref(base_id) => Some(base_id),
            _ => None,
        })
}

/// What a task install rebuilds of the type index from the merged state.
pub(crate) struct DerivedToRebuild {
    /// Each declaration whose super edges the fork changed, with every base
    /// its edges named before or after.
    subclasses: Vec<(LuaTypeDeclId, HashSet<LuaTypeDeclId>)>,
    decl_scan: bool,
}

/// Orders subclass declarations by name, breaking ties by declaration id so
/// same-named locals in different files still compare in a total order rather
/// than falling back to hash-map iteration order.
fn sort_type_decls_for_sub_types(sub_types: &mut Vec<&LuaTypeDecl>) {
    sub_types.sort_by(|left, right| {
        left.get_name()
            .cmp(right.get_name())
            .then_with(|| left.get_id().stable_cmp(&right.get_id()))
    });
}

fn collect_type_cache_refs(typ: &LuaType) -> HashSet<TypeCacheRef> {
    let mut refs = HashSet::default();
    typ.visit_type_identities(&mut |inner| {
        match inner {
            LuaType::TableConst(range) => {
                refs.insert(TypeCacheRef::Table(range.clone()));
            }
            LuaType::Instance(instance) => {
                refs.insert(TypeCacheRef::Instance(instance.get_range().clone()));
            }
            LuaType::Signature(signature_id) => {
                refs.insert(TypeCacheRef::Signature(*signature_id));
            }
            LuaType::ModuleRef(file_id) => {
                refs.insert(TypeCacheRef::Module(*file_id));
            }
            LuaType::Ref(type_id) | LuaType::Def(type_id) => {
                refs.insert(TypeCacheRef::Decl(type_id.clone()));
            }
            _ => {}
        };
    });

    refs
}

#[derive(Debug, Clone, PartialEq)]
struct TypeFact {
    cache: LuaTypeCache,
    read: derivation::FactRead,
}

impl std::ops::Deref for TypeFact {
    type Target = LuaTypeCache;

    fn deref(&self) -> &Self::Target {
        &self.cache
    }
}

impl std::hash::Hash for TypeFact {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.cache, state);
    }
}

#[derive(Debug, Clone)]
pub struct LuaTypeIndex {
    file_namespace: CowCell<HashMap<FileId, String>>,
    file_using_namespace: CowCell<HashMap<FileId, Vec<String>>>,
    file_types: CowCell<HashMap<FileId, Vec<LuaTypeDeclId>>>,
    declaration_sites: CowCell<HashMap<FileId, HashMap<TextRange, LuaTypeDeclId>>>,
    full_name_type_map: CowShards<LuaTypeDeclId, LuaTypeDecl>,
    generic_params: CowCell<HashMap<LuaTypeDeclId, Vec<GenericParam>>>,
    /// Per-site alias contributions: decl id → declaration site → payload.
    /// The effective `LuaTypeExtra::Alias::origin` and `generic_params` entry
    /// are the winning site's pair, re-selected on every publish/retract.
    /// Removal walks the removed file's `file_types` decls and drops the
    /// sites it owned — a contribution always lands on the decl the file's
    /// own declaration produced.
    alias_sources: CowCell<HashMap<LuaTypeDeclId, HashMap<InFiled<TextRange>, LuaAliasSource>>>,
    /// Per-site `---@enum` and `---@attribute` payloads with their
    /// precedence; the decl carries the winning site's payload.
    extra_sources: CowCell<
        HashMap<LuaTypeDeclId, HashMap<InFiled<TextRange>, (LuaDeclSourceKey, LuaTypeExtra)>>,
    >,
    supers: CowShards<LuaTypeDeclId, Vec<InFiled<LuaSuperType>>>,
    /// Super-type edges by the file that declared them, so removing a file
    /// visits only its own edges instead of retaining over every edge in the
    /// workspace. One record per [`LuaTypeIndex::add_super_type`] write, kept
    /// in step with [`Self::supers`].
    supers_by_file: CowCell<HashMap<FileId, Vec<(LuaTypeDeclId, InFiled<LuaSuperType>)>>>,
    /// Reverse super-type edges: base class id → direct subclass ids. Updated
    /// on every [`LuaTypeIndex::add_super_type`] write and pruned by declaring
    /// file when [`LuaIndex::remove_files`] drops super-type entries, so
    /// [`LuaTypeIndex::get_sub_types`] is a lookup instead of a scan over
    /// every inheritance edge in the workspace.
    sub_types: CowShards<LuaTypeDeclId, Vec<LuaTypeDeclId>>,
    /// Which class each table-literal declaration's stored type names. Backs
    /// `local_class_table_member_ids`: finding the table literals that back
    /// a class is a lookup of the declarations bound to it instead of a scan
    /// over every declaration in the class's files. Maintained on every
    /// type-cache write and file removal alongside the caches themselves.
    decl_type_binds: CowShards<LuaDeclId, LuaTypeDeclId>,
    type_bound_decls: CowShards<LuaTypeDeclId, HashSet<LuaDeclId>>,
    /// Each file's declarations initialised with a table literal, the only
    /// ones a class binding is kept for; a source fact of the file.
    table_literal_decls: CowCell<HashMap<FileId, HashSet<LuaDeclId>>>,
    types: CowShards<LuaTypeOwner, TypeFact>,
    cache_refs: TypeCacheRefIndex,
    in_filed_type_owner: CowShards<FileId, HashSet<LuaTypeOwner>>,
    fact_metadata: CowShards<LuaTypeOwner, LuaTypeFactMetadata>,
    /// The files one fact's provenance and step types name, per fact owner,
    /// with the reverse map beside it. [`Self::remap_inference_facts`] visits
    /// only the facts that name the edited file instead of every fact in the
    /// workspace; the file set is exactly what `remap` consults (node, source
    /// and support files plus `TableConst`/`Instance`/`Signature` files in the
    /// step types), so a fact outside the edited file's bucket remaps to
    /// itself.
    fact_ref_files: CowShards<LuaTypeOwner, HashSet<FileId>>,
    fact_refs_by_file: CowShards<FileId, HashSet<LuaTypeOwner>>,
    /// The [`Self::fact_ref_files`] counterpart for definition facts, whose
    /// payload type can name another file's literal or signature on top of
    /// the provenance every fact carries.
    definition_fact_ref_files: CowShards<LuaDefinitionId, HashSet<FileId>>,
    definition_fact_refs_by_file: CowShards<FileId, HashSet<LuaDefinitionId>>,
    /// For each decl whose type a write has seeded: that write's source
    /// position, and whether its right-hand side was a call or index read. See
    /// `bind_decl_write`.
    decl_write_claims: CowShards<LuaDeclId, (TextSize, bool)>,
    definition_facts: CowShards<LuaDefinitionId, LuaTypeFact>,
    inference_events_by_file: CowCell<HashMap<FileId, Arc<[LuaInferenceDiagnosticEvent]>>>,
    support_dependents: CowCell<HashMap<LuaInferenceNodeId, HashSet<FileId>>>,
    support_file_dependents: CowCell<HashMap<FileId, HashSet<FileId>>>,
    derived_state_dirty: bool,
    /// Multiset fingerprint of every id in `full_name_type_map`, for
    /// `TypeDeclScan`.
    decl_scan_fingerprint: Fingerprint,
    /// Each owner's cache and metadata as the source layer left them; a reset
    /// restores these. See [`derivation::writes_source`].
    source_types: CowCell<HashMap<LuaTypeOwner, (LuaTypeCache, Option<LuaTypeFactMetadata>)>>,
    /// The locations the source layer declared each type decl at. Doc-time
    /// name resolution only sees these, so a class a derivation registers
    /// never changes how a file's annotations resolve.
    source_decl_locations: CowCell<HashMap<LuaTypeDeclId, Vec<LuaDeclLocation>>>,
    /// The declaration locations and super edges each derivation added, so
    /// they are retracted before it runs again.
    derived_contributions: CowShards<DerivationId, Vec<TypeContribution>>,
    /// Who holds each declaration location, with the flags it gave. A
    /// location's flags are its holders' union; it leaves with its last one.
    location_holders:
        CowShards<(LuaTypeDeclId, FileId, TextRange), Vec<(Holder, FlagSet<LuaTypeFlag>)>>,
    /// Who holds each super edge; an edge leaves with its last holder.
    super_holders: CowShards<(LuaTypeDeclId, InFiled<LuaSuperType>), Vec<Holder>>,
    /// Table literals a registration turned into a class (a vgui panel
    /// table, a scripted entity table), with the derivations registering
    /// each. A stored type that names the literal names the class instead.
    table_classes: CowShards<InFiled<TextRange>, Vec<(DerivationId, LuaType)>>,
    /// Per file, the owners its local fallbacks published a type over, with
    /// what the statements writing each left there: readers see the fallback,
    /// a statement's write lands underneath it, and a reset of the fallbacks
    /// restores it. See [`derivation::writes_fallback_of`].
    fallbacks: CowShards<FileId, HashMap<LuaTypeOwner, StatementType>>,
}

/// What the statements writing an owner's type left there, under the type a
/// local fallback published over it.
#[derive(Debug, Clone, Default, PartialEq, Hash)]
struct StatementType {
    cache: Option<LuaTypeCache>,
    metadata: Option<LuaTypeFactMetadata>,
}

/// The fact holding the type `owner`'s statements wrote.
fn statement_fact(owner: &LuaTypeOwner) -> crate::LostFact {
    crate::LostFact::new(owner.get_file_id(), FactKey::TypeCache(owner.clone()))
}

/// Notes a write of `owner`'s stored type. A file's local fallbacks move it
/// without producing it: what they publish over the statements is theirs, a
/// [`FactKey::TypeFallbacks`], and the statements stay its producers.
fn note_type_written(owner: &LuaTypeOwner) {
    let file_id = owner.get_file_id();
    if derivation::writes_fallback_of(file_id) {
        derivation::rehoming(|| derivation::note_write(|| FactKey::TypeCache(owner.clone())));
        derivation::note_write(|| FactKey::TypeFallbacks(file_id));
    } else {
        derivation::note_write(|| FactKey::TypeCache(owner.clone()));
    }
}

/// The layer holding a type-index entry: `None` for the source layer, else
/// the derivation that added it.
type Holder = Option<DerivationId>;

fn current_holder() -> Holder {
    if derivation::writes_source() {
        None
    } else {
        derivation::current()
    }
}

fn union_flags(holders: &[(Holder, FlagSet<LuaTypeFlag>)]) -> FlagSet<LuaTypeFlag> {
    holders
        .iter()
        .fold(FlagSet::default(), |flags, (_, flag)| flags | *flag)
}

/// A type-index entry a derivation added on top of the source layer.
#[derive(Debug, Clone, Hash, PartialEq)]
enum TypeContribution {
    Location {
        decl_id: LuaTypeDeclId,
        location: LuaDeclLocation,
    },
    Super {
        decl_id: LuaTypeDeclId,
        entry: InFiled<LuaSuperType>,
    },
    TableClass {
        table: InFiled<TextRange>,
    },
}

impl Default for LuaTypeIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaTypeIndex {
    pub fn new() -> Self {
        Self {
            file_namespace: Default::default(),
            file_using_namespace: Default::default(),
            file_types: Default::default(),
            declaration_sites: Default::default(),
            full_name_type_map: Default::default(),
            alias_sources: Default::default(),
            extra_sources: Default::default(),
            generic_params: Default::default(),
            supers: Default::default(),
            supers_by_file: Default::default(),
            sub_types: Default::default(),
            decl_type_binds: Default::default(),
            type_bound_decls: Default::default(),
            table_literal_decls: Default::default(),
            types: CowShards::default(),
            cache_refs: Default::default(),
            in_filed_type_owner: Default::default(),
            fact_ref_files: Default::default(),
            fact_refs_by_file: Default::default(),
            definition_fact_ref_files: Default::default(),
            definition_fact_refs_by_file: Default::default(),
            fact_metadata: CowShards::default(),
            decl_write_claims: Default::default(),
            definition_facts: Default::default(),
            inference_events_by_file: Default::default(),
            support_dependents: Default::default(),
            support_file_dependents: Default::default(),
            derived_state_dirty: false,
            decl_scan_fingerprint: derivation::ABSENT,
            source_types: Default::default(),
            source_decl_locations: Default::default(),
            derived_contributions: Default::default(),
            location_holders: Default::default(),
            super_holders: Default::default(),
            table_classes: Default::default(),
            fallbacks: Default::default(),
        }
    }

    pub fn add_file_namespace(&mut self, file_id: FileId, namespace: String) {
        self.file_namespace.insert(file_id, namespace);
        derivation::note_write(|| FactKey::TypeFile(file_id));
    }

    pub fn get_file_namespace(&self, file_id: &FileId) -> Option<&String> {
        derivation::note_read(|| FactKey::TypeFile(*file_id));
        self.file_namespace.get(file_id)
    }

    pub fn add_file_using_namespace(&mut self, file_id: FileId, namespace: String) {
        self.file_using_namespace
            .entry(file_id)
            .or_default()
            .push(namespace);
        derivation::note_write(|| FactKey::TypeFile(file_id));
    }

    pub fn get_file_using_namespace(&self, file_id: &FileId) -> Option<&Vec<String>> {
        derivation::note_read(|| FactKey::TypeFile(*file_id));
        self.file_using_namespace.get(file_id)
    }

    /// return previous FileId if exist
    fn record_contribution(&mut self, contribution: TypeContribution) {
        let Some(id) = derivation::current() else {
            return;
        };
        derivation::note_write(|| FactKey::TypeContrib(id));
        self.derived_contributions
            .entry(id)
            .or_default()
            .push(contribution);
    }

    /// Removes what derivation `id` added, before it runs again or after its
    /// file is gone.
    pub(crate) fn retract_contributions(&mut self, id: &DerivationId) {
        let Some(contributions) = self.derived_contributions.remove(id) else {
            return;
        };
        for contribution in contributions.into_iter().rev() {
            match contribution {
                TypeContribution::Location { decl_id, location } => {
                    self.release_location(&decl_id, &location, Some(*id))
                }
                TypeContribution::Super { decl_id, entry } => {
                    self.release_super_entry(&decl_id, &entry, Some(*id))
                }
                TypeContribution::TableClass { table } => self.release_table_class(&table, id),
            }
        }
    }

    /// Files a source-layer location in canonical order, so a file's rebuild
    /// restores the list a cold build has.
    fn add_source_location(&mut self, decl_id: &LuaTypeDeclId, location: LuaDeclLocation) {
        derivation::note_write(|| FactKey::SourceTypeDecl(decl_id.clone()));
        let locations = self
            .source_decl_locations
            .entry(decl_id.clone())
            .or_default();
        let at = locations.partition_point(|existing| {
            (
                existing.file_id,
                existing.range.start(),
                existing.range.end(),
            ) <= (
                location.file_id,
                location.range.start(),
                location.range.end(),
            )
        });
        locations.insert(at, location);
    }

    fn hold_location(
        &mut self,
        decl_id: &LuaTypeDeclId,
        location: &LuaDeclLocation,
        holder: Holder,
    ) {
        self.location_holders
            .entry((decl_id.clone(), location.file_id, location.range))
            .or_default()
            .push((holder, location.flag));
    }

    /// Drops `holder`'s hold on `location`, removing it once nothing holds it.
    fn release_location(
        &mut self,
        decl_id: &LuaTypeDeclId,
        location: &LuaDeclLocation,
        holder: Holder,
    ) {
        let file_id = location.file_id;
        let site = (decl_id.clone(), file_id, location.range);
        let Some(holders) = self.location_holders.get_mut(&site) else {
            return;
        };
        let Some(at) = holders.iter().position(|(existing, _)| *existing == holder) else {
            return;
        };
        holders.remove(at);
        let remaining = (!holders.is_empty()).then(|| union_flags(holders));
        if remaining.is_none() {
            self.location_holders.remove(&site);
        }
        let Some(decl) = self.full_name_type_map.get_mut(decl_id) else {
            return;
        };
        let locations = decl.get_mut_locations();
        let Some(at) = locations
            .iter()
            .position(|existing| existing.file_id == file_id && existing.range == location.range)
        else {
            return;
        };
        if let Some(flag) = remaining {
            if locations[at].flag != flag {
                locations[at].flag = flag;
                note_decl_written(decl_id);
            }
            return;
        }
        locations.remove(at);
        let emptied = locations.is_empty();
        note_decl_written(decl_id);
        if emptied {
            self.full_name_type_map.remove(decl_id);
            self.remove_from_decl_scan(decl_id);
            if self.generic_params.remove(decl_id).is_some() {
                derivation::note_write(|| FactKey::TypeGenericParams(decl_id.clone()));
            }
            self.alias_sources.remove(decl_id);
        }
    }

    /// Drops `holder`'s hold on a super edge, removing it once nothing holds it.
    fn release_super_entry(
        &mut self,
        decl_id: &LuaTypeDeclId,
        entry: &InFiled<LuaSuperType>,
        holder: Holder,
    ) {
        let edge = (decl_id.clone(), entry.clone());
        let Some(holders) = self.super_holders.get_mut(&edge) else {
            return;
        };
        let Some(at) = holders.iter().position(|existing| *existing == holder) else {
            return;
        };
        holders.remove(at);
        if !holders.is_empty() {
            return;
        }
        self.super_holders.remove(&edge);
        let Some(supers) = self.supers.get_mut(decl_id) else {
            return;
        };
        let Some(at) = supers.iter().position(|existing| existing == entry) else {
            return;
        };
        supers.remove(at);
        if supers.is_empty() {
            self.supers.remove(decl_id);
        }
        note_supers_written(decl_id, entry.file_id);
        if let LuaType::Ref(base_id) = &entry.value.typ {
            let still_derives = self.supers.get(decl_id).is_some_and(|supers| {
                supers
                    .iter()
                    .any(|other| matches!(&other.value.typ, LuaType::Ref(base) if base == base_id))
            });
            if !still_derives && let Some(children) = self.sub_types.get_mut(base_id) {
                children.retain(|child| child != decl_id);
                if children.is_empty() {
                    self.sub_types.remove(base_id);
                }
                derivation::note_write(|| FactKey::SubTypes(base_id.clone()));
            }
        }
    }

    pub fn add_type_decl(&mut self, file_id: FileId, type_decl: LuaTypeDecl) {
        let id = type_decl.get_id();
        note_decl_written(&id);
        let holder = current_holder();
        for location in type_decl.locations() {
            self.hold_location(&id, location, holder);
        }
        if derivation::writes_source() {
            for location in type_decl.locations() {
                self.add_source_location(&id, location.clone());
            }
            self.add_source_site(file_id, &id, type_decl.locations());
        } else {
            for location in type_decl.locations() {
                self.record_contribution(TypeContribution::Location {
                    decl_id: id.clone(),
                    location: location.clone(),
                });
            }
        }

        if let Some(old_decl) = self.full_name_type_map.get_mut(&id) {
            for location in type_decl.locations() {
                old_decl.add_location(location.clone());
            }
        } else {
            self.add_to_decl_scan(&id);
            self.full_name_type_map.insert(id, type_decl);
        }
    }

    pub fn add_type_decl_location(
        &mut self,
        file_id: FileId,
        decl_id: &LuaTypeDeclId,
        location: LuaDeclLocation,
    ) {
        if !self.full_name_type_map.contains_key(decl_id) {
            return;
        }
        let holder = current_holder();
        if holder.is_some() {
            self.record_contribution(TypeContribution::Location {
                decl_id: decl_id.clone(),
                location: location.clone(),
            });
        } else if derivation::writes_source() {
            self.add_source_location(decl_id, location.clone());
            self.add_source_site(file_id, decl_id, std::slice::from_ref(&location));
        }
        self.hold_location(decl_id, &location, holder);
        if let Some(decl) = self.full_name_type_map.get_mut(decl_id) {
            note_decl_written(decl_id);
            decl.add_location(location);
        }
    }

    /// Files a source-layer declaration under its sites and its file: what a
    /// file declares, as its own doc passes look it up.
    fn add_source_site(
        &mut self,
        file_id: FileId,
        decl_id: &LuaTypeDeclId,
        locations: &[LuaDeclLocation],
    ) {
        derivation::note_write(|| FactKey::TypeFile(file_id));
        let sites = self.declaration_sites.entry(file_id).or_default();
        for location in locations {
            sites.insert(location.range, decl_id.clone());
        }
        self.file_types
            .entry(file_id)
            .or_default()
            .push(decl_id.clone());
    }

    /// [`Self::find_type_decl`] over the source layer's declarations only.
    /// Doc annotations resolve through this, so their meaning depends only on
    /// what files declare, never on what analysis synthesized.
    pub fn find_source_type_decl(&self, file_id: FileId, name: &str) -> Option<&LuaTypeDecl> {
        let namespaced = |ns: &str| LuaTypeDeclId::global(&format!("{}.{}", ns, name));
        let candidates = self
            .file_namespace
            .get(&file_id)
            .map(|ns| namespaced(ns))
            .into_iter()
            .chain(
                self.file_using_namespace
                    .get(&file_id)
                    .into_iter()
                    .flatten()
                    .map(|ns| namespaced(ns)),
            )
            .chain([
                LuaTypeDeclId::local(file_id, name),
                LuaTypeDeclId::global(name),
            ]);
        for id in candidates {
            if let Some(decl) = self.get_source_type_decl(&id) {
                return Some(decl);
            }
        }
        None
    }

    /// The type decl `id` when the source layer declares it.
    pub fn get_source_type_decl(&self, id: &LuaTypeDeclId) -> Option<&LuaTypeDecl> {
        derivation::note_read(|| FactKey::SourceTypeDecl(id.clone()));
        if self
            .source_decl_locations
            .get(id)
            .is_none_or(|locations| locations.is_empty())
        {
            return None;
        }
        self.full_name_type_map.get(id)
    }

    pub fn find_type_decl(&self, file_id: FileId, name: &str) -> Option<&LuaTypeDecl> {
        if let Some(ns) = self.get_file_namespace(&file_id) {
            let full_name = LuaTypeDeclId::global(&format!("{}.{}", ns, name));
            derivation::note_type_decl_read(&full_name);
            if let Some(decl) = self.full_name_type_map.get(&full_name) {
                return Some(decl);
            }
        }
        if let Some(usings) = self.get_file_using_namespace(&file_id) {
            for ns in usings {
                let full_name = LuaTypeDeclId::global(&format!("{}.{}", ns, name));
                derivation::note_type_decl_read(&full_name);
                if let Some(decl) = self.full_name_type_map.get(&full_name) {
                    return Some(decl);
                }
            }
        }

        let local_id = LuaTypeDeclId::local(file_id, name);
        derivation::note_type_decl_read(&local_id);
        if let Some(decl) = self.full_name_type_map.get(&local_id) {
            return Some(decl);
        }

        let global_id = LuaTypeDeclId::global(name);
        derivation::note_type_decl_read(&global_id);
        self.full_name_type_map.get(&global_id)
    }

    /// Finds the declaration registered by this file at the given name-token range.
    /// Registration fixes namespace and visibility at the declaration site;
    /// later namespace tags and using directives must not retarget it.
    pub fn find_type_decl_by_site(
        &self,
        file_id: FileId,
        range: TextRange,
    ) -> Option<&LuaTypeDecl> {
        derivation::note_read(|| FactKey::TypeFile(file_id));
        let id = self.declaration_sites.get(&file_id)?.get(&range)?;
        self.full_name_type_map.get(id)
    }

    pub fn find_type_decls(
        &self,
        file_id: FileId,
        prefix: &str,
    ) -> HashMap<String, Option<LuaTypeDeclId>> {
        derivation::note_read(|| FactKey::TypeDeclScan);
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
        derivation::note_write(|| FactKey::TypeGenericParams(decl_id.clone()));
        self.generic_params.insert(decl_id.clone(), params);
    }

    pub fn get_generic_params(&self, decl_id: &LuaTypeDeclId) -> Option<&Vec<GenericParam>> {
        derivation::note_type_generic_params_read(decl_id);
        self.generic_params.get(decl_id)
    }

    /// Publishes one declaration site's alias payload and re-selects the
    /// effective origin/generic-params pair.
    ///
    /// The contribution is keyed by the site's `(file, name-token range)`, so
    /// re-analyzing a file replaces its own site instead of appending, and
    /// the winner is chosen by [`LuaDeclSourceKey`] precedence rather than
    /// write order — two files declaring the same alias no longer oscillate
    /// with ingestion order. Re-publishing an identical source is a no-op.
    pub fn set_alias_source(&mut self, decl_id: LuaTypeDeclId, source: LuaAliasSource) {
        let site = source.site.clone();
        if self
            .alias_sources
            .get(&decl_id)
            .and_then(|sources| sources.get(&site))
            == Some(&source)
        {
            return;
        }
        self.alias_sources
            .entry(decl_id.clone())
            .or_default()
            .insert(site, source);
        self.reselect_alias_source(&decl_id);
    }

    /// Publishes one declaration site's `---@enum` or `---@attribute` payload.
    pub fn set_extra_source(
        &mut self,
        decl_id: LuaTypeDeclId,
        site: InFiled<TextRange>,
        order_key: LuaDeclSourceKey,
        extra: LuaTypeExtra,
    ) {
        self.extra_sources
            .entry(decl_id.clone())
            .or_default()
            .insert(site, (order_key, extra));
        self.reselect_extra_source(&decl_id);
    }

    /// Ranks the `---@alias`, `---@enum` and `---@attribute` sources of each of
    /// `order_keys`' files by its new order key, re-selecting each winner.
    pub(crate) fn rekey_files(&mut self, order_keys: &[(FileId, Arc<str>)]) {
        let keys = order_keys.iter().cloned().collect::<HashMap<_, _>>();
        let rekeys = |site: &InFiled<TextRange>, order_key: &LuaDeclSourceKey| {
            keys.get(&site.file_id)
                .is_some_and(|key| order_key.file_order_key != *key)
        };
        let aliases = self
            .alias_sources
            .iter()
            .filter(|(_, sources)| {
                sources
                    .iter()
                    .any(|(site, source)| rekeys(site, &source.order_key))
            })
            .map(|(decl_id, _)| decl_id.clone())
            .collect::<Vec<_>>();
        let extras = self
            .extra_sources
            .iter()
            .filter(|(_, sources)| {
                sources
                    .iter()
                    .any(|(site, (order_key, _))| rekeys(site, order_key))
            })
            .map(|(decl_id, _)| decl_id.clone())
            .collect::<Vec<_>>();
        for decl_id in aliases {
            if let Some(sources) = self.alias_sources.get_mut(&decl_id) {
                for (site, source) in sources.iter_mut() {
                    if let Some(key) = keys.get(&site.file_id) {
                        source.order_key.file_order_key = key.clone();
                    }
                }
            }
            self.reselect_alias_source(&decl_id);
        }
        for decl_id in extras {
            if let Some(sources) = self.extra_sources.get_mut(&decl_id) {
                for (site, (order_key, _)) in sources.iter_mut() {
                    if let Some(key) = keys.get(&site.file_id) {
                        order_key.file_order_key = key.clone();
                    }
                }
            }
            self.reselect_extra_source(&decl_id);
        }
    }

    fn reselect_extra_source(&mut self, decl_id: &LuaTypeDeclId) {
        let winner = self.extra_sources.get(decl_id).and_then(|sources| {
            sources
                .values()
                .min_by(|a, b| a.0.cmp(&b.0))
                .map(|(_, extra)| extra)
        });
        let Some(decl) = self.full_name_type_map.get_mut(decl_id) else {
            return;
        };
        if decl.install_site_extra(winner) {
            derivation::note_write(|| FactKey::TypeDecl(decl_id.clone()));
        }
    }

    /// Installs the winning site's origin and generic params on the decl.
    ///
    /// Both slots move together from the same contribution, so an origin is
    /// never instantiated against another file's params. An unchanged
    /// effective value skips the semantic write — hidden contributions still
    /// update — so a losing publish does not churn `generic_params` readers.
    fn reselect_alias_source(&mut self, decl_id: &LuaTypeDeclId) {
        let Some(sources) = self.alias_sources.get(decl_id) else {
            return;
        };
        let winner = sources
            .values()
            .min_by(|a, b| a.order_key.cmp(&b.order_key));
        let new_origin = winner.map(|source| &source.origin);
        let new_params = winner.and_then(|source| source.generic_params.as_ref());

        let origin_changed = self
            .full_name_type_map
            .get(decl_id)
            .is_some_and(|decl| decl.get_alias_ref() != new_origin);
        let params_changed = self.generic_params.get(decl_id) != new_params;
        // Clone only the slots that actually move; the `alias_sources` borrow
        // ends here so the writes below can take `&mut self`.
        let new_origin = if origin_changed {
            new_origin.cloned()
        } else {
            None
        };
        let new_params = if params_changed {
            new_params.cloned()
        } else {
            None
        };

        if origin_changed {
            derivation::note_write(|| FactKey::TypeDecl(decl_id.clone()));
        }
        if params_changed {
            derivation::note_write(|| FactKey::TypeGenericParams(decl_id.clone()));
        }
        if origin_changed {
            if let Some(decl) = self.full_name_type_map.get_mut(decl_id) {
                decl.set_alias_origin(new_origin);
            }
        }
        if params_changed {
            match new_params {
                Some(params) => {
                    self.generic_params.insert(decl_id.clone(), params);
                }
                None => {
                    self.generic_params.remove(decl_id);
                }
            }
        }
    }
    /// Every stored alias contribution, borrowed, for deterministic snapshot
    /// dumps of the complete index — the consumer sorts the records itself.
    pub fn iter_alias_sources(&self) -> impl Iterator<Item = (&LuaTypeDeclId, &LuaAliasSource)> {
        self.alias_sources
            .iter()
            .flat_map(|(decl_id, sources)| sources.values().map(move |source| (decl_id, source)))
    }

    pub fn add_super_type(
        &mut self,
        decl_id: LuaTypeDeclId,
        file_id: FileId,
        source_range: TextRange,
        super_type: LuaType,
    ) -> bool {
        // Kept in source order rather than the order the files happened to be
        // analysed in: an incremental re-analysis visits the declaring files in
        // a different order than a cold build, and the first entry decides
        // which declaration a consumer resolves against.
        let base_id = match &super_type {
            LuaType::Ref(base_id) => Some(base_id.clone()),
            _ => None,
        };
        let entry = InFiled::new(
            file_id,
            LuaSuperType {
                source_range,
                typ: super_type,
            },
        );
        let holder = current_holder();
        if holder.is_some() {
            self.record_contribution(TypeContribution::Super {
                decl_id: decl_id.clone(),
                entry: entry.clone(),
            });
        } else {
            self.supers_by_file
                .entry(file_id)
                .or_default()
                .push((decl_id.clone(), entry.clone()));
        }
        let holders = self
            .super_holders
            .entry((decl_id.clone(), entry.clone()))
            .or_default();
        holders.push(holder);
        if holders.len() > 1 {
            return false;
        }
        note_supers_written(&decl_id, file_id);
        let supers = self.supers.entry(decl_id.clone()).or_default();
        let at = supers.partition_point(|existing| {
            super_entry_sort_key(existing) <= super_entry_sort_key(&entry)
        });
        supers.insert(at, entry);
        if let Some(base_id) = base_id {
            let children = self.sub_types.entry(base_id.clone()).or_default();
            if !children.contains(&decl_id) {
                children.push(decl_id.clone());
                derivation::note_write(|| FactKey::SubTypes(base_id.clone()));
            }
        }
        true
    }

    pub fn get_super_types(&self, decl_id: &LuaTypeDeclId) -> Option<Vec<LuaType>> {
        self.get_super_types_iter(decl_id)
            .map(|super_types| super_types.cloned().collect())
    }

    pub fn get_super_types_iter(
        &self,
        decl_id: &LuaTypeDeclId,
    ) -> Option<impl Iterator<Item = &LuaType> + '_> {
        derivation::note_type_super_types_read(decl_id);
        self.supers
            .get(decl_id)
            .map(|supers| distinct_super_types(supers))
    }

    /// The super edges of `decl_id` in order. A reader observes an edge's
    /// `source_range` only through the realm at its start, which it notes with
    /// [`derivation::note_type_super_realms_read`].
    pub(crate) fn get_super_type_entries(
        &self,
        decl_id: &LuaTypeDeclId,
    ) -> Option<&[InFiled<LuaSuperType>]> {
        derivation::note_type_super_edges_read(decl_id);
        self.supers.get(decl_id).map(Vec::as_slice)
    }

    /// The super edges of `decl_id` in `file_id`, in order: records no read.
    pub(crate) fn super_edges_in_file<'a>(
        &'a self,
        decl_id: &LuaTypeDeclId,
        file_id: FileId,
    ) -> impl Iterator<Item = &'a InFiled<LuaSuperType>> + 'a {
        self.supers
            .get(decl_id)
            .into_iter()
            .flatten()
            .filter(move |entry| entry.file_id == file_id)
    }

    /// The declarations in `files` whose stored type is a direct reference to
    /// `type_id`. Only the files the caller can observe get read stamps — a
    /// bind in any other file cannot move the filtered result.
    pub fn decls_bound_to_type<'a>(
        &'a self,
        type_id: &'a LuaTypeDeclId,
        files: &'a HashSet<FileId>,
    ) -> impl Iterator<Item = LuaDeclId> + 'a {
        for file_id in files {
            derivation::note_read(|| FactKey::BoundDecls(type_id.clone(), *file_id));
        }
        self.type_bound_decls
            .get(type_id)
            .into_iter()
            .flatten()
            .filter(move |decl_id| files.contains(&decl_id.file_id))
            .copied()
    }

    /// Sets `file_id`'s declarations initialised with a table literal, before
    /// any type of them is stored.
    pub(crate) fn set_table_literal_decls(&mut self, file_id: FileId, decls: HashSet<LuaDeclId>) {
        if decls.is_empty() {
            self.table_literal_decls.remove(&file_id);
        } else {
            self.table_literal_decls.insert(file_id, decls);
        }
    }

    fn is_table_literal_decl(&self, decl_id: &LuaDeclId) -> bool {
        self.table_literal_decls
            .get(&decl_id.file_id)
            .is_some_and(|decls| decls.contains(decl_id))
    }

    fn bind_decl_to_type(&mut self, decl_id: LuaDeclId, type_id: LuaTypeDeclId) {
        if self.decl_type_binds.get(&decl_id) == Some(&type_id) {
            return;
        }
        self.unbind_decl(&decl_id);
        derivation::note_write(|| FactKey::BoundDecls(type_id.clone(), decl_id.file_id));
        self.decl_type_binds.insert(decl_id, type_id.clone());
        self.type_bound_decls
            .entry(type_id.clone())
            .or_default()
            .insert(decl_id);
    }

    fn unbind_decl(&mut self, decl_id: &LuaDeclId) {
        let Some(type_id) = self.decl_type_binds.remove(decl_id) else {
            return;
        };
        derivation::note_write(|| FactKey::BoundDecls(type_id.clone(), decl_id.file_id));
        if let Some(decls) = self.type_bound_decls.get_mut(&type_id) {
            decls.remove(decl_id);
            if decls.is_empty() {
                self.type_bound_decls.remove(&type_id);
            }
        }
    }

    /// The direct subclasses of `decl_id`, unordered. Each child's declaration
    /// is read only when the iterator reaches it.
    pub fn direct_sub_types(&self, decl_id: &LuaTypeDeclId) -> impl Iterator<Item = &LuaTypeDecl> {
        derivation::note_read(|| FactKey::SubTypes(decl_id.clone()));
        self.sub_types
            .get(decl_id)
            .into_iter()
            .flatten()
            .filter_map(|child_id| {
                derivation::note_type_decl_read(child_id);
                self.full_name_type_map.get(child_id)
            })
    }

    /// Get all direct subclasses of a given type
    /// Returns a vector of type declarations that directly inherit from the given type
    pub fn get_sub_types(&self, decl_id: &LuaTypeDeclId) -> Vec<&LuaTypeDecl> {
        let mut sub_types = self.direct_sub_types(decl_id).collect::<Vec<_>>();

        // Sort to ensure deterministic ordering regardless of HashMap iteration order
        sort_type_decls_for_sub_types(&mut sub_types);
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
        derivation::note_type_decl_read(&decl_id);
        self.full_name_type_map.get(decl_id)
    }

    /// Whether some non-auto declaration already owns this name.
    ///
    /// Synthesizers that attach edges to a class they are materializing (the
    /// call-site str-tpl paths, `scripted_ents.Register`/panel classes) must
    /// ask this before writing, because their own decision would otherwise
    /// depend on whether a *different* pass created the declaration first:
    /// the same call would attach an edge in one pass order and skip it in
    /// another, and the edge's `(file, range, type)` dedup key makes the
    /// replay novel. Attaching only to a name nothing else has declared makes
    /// the result a function of the settled declaration set instead of the
    /// order in which it was built.
    ///
    /// This is deliberately kind-agnostic: an `---@alias`/`---@enum` that
    /// shadowed the name is as much an owner as a `---@class`, and the caller
    /// has no class to attach a super to in that case anyway.
    pub fn has_non_auto_declaration(&self, decl_id: &LuaTypeDeclId) -> bool {
        self.get_type_decl(decl_id).is_some_and(|decl| {
            decl.get_location_flags()
                .any(|(_, flag)| !flag.contains(LuaTypeFlag::AutoGenerated))
        })
    }

    /// Every type declaration, ordered by [`LuaTypeDeclId::stable_cmp`].
    pub fn get_all_types(&self) -> Vec<&LuaTypeDecl> {
        let mut types: Vec<&LuaTypeDecl> = self.iter_type_decls().collect();
        types.sort_unstable_by(|left, right| left.get_id().stable_cmp(&right.get_id()));
        types
    }

    /// [`get_all_types`](Self::get_all_types) without collecting, for the
    /// analyzer paths that only scan and would otherwise allocate a vector of
    /// every type in the workspace on each call.
    pub fn iter_type_decls(&self) -> impl Iterator<Item = &LuaTypeDecl> {
        derivation::note_read(|| FactKey::TypeDeclScan);
        self.full_name_type_map.values().inspect(|decl| {
            derivation::note_read(|| FactKey::TypeDecl(decl.get_id()));
        })
    }

    pub fn get_file_namespaces(&self) -> Vec<String> {
        self.file_namespace
            .values()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect()
    }

    /// Stores `cache` for `owner`, keeping any type already bound there unless
    /// `cache` supersedes it.
    ///
    /// A superseding write discards the old type, so the metadata derived from
    /// it has to go with it; otherwise [`get_type_fact`](Self::get_type_fact)
    /// pairs the new type with the old confidence and provenance.
    pub fn bind_type(&mut self, owner: LuaTypeOwner, cache: LuaTypeCache) {
        if self.write_under_fallback(&owner, |index, statement| {
            if statement
                .cache
                .as_ref()
                .is_some_and(|existing| !cache.supersedes(existing))
            {
                return None;
            }
            Some(StatementType {
                cache: Some(index.normalized_cache(&owner, cache.clone())),
                metadata: statement
                    .metadata
                    .clone()
                    .filter(|_| statement.cache.is_none()),
            })
        }) {
            return;
        }
        if let Some(existing) = self.types.get(&owner)
            && !cache.supersedes(existing)
        {
            return;
        }
        self.commit_type_cache(owner.clone(), cache);
        self.record_source_write(&owner);
    }

    /// The write that seeded `decl_id`'s type: its source position, and whether
    /// its right-hand side read through a call or index.
    pub fn decl_write_claim(&self, decl_id: &LuaDeclId) -> Option<(TextSize, bool)> {
        derivation::note_read(|| FactKey::DeclClaim(*decl_id));
        self.decl_write_claims.get(decl_id).copied()
    }

    pub fn record_decl_write_claim(
        &mut self,
        decl_id: LuaDeclId,
        position: TextSize,
        reads_through_call_or_index: bool,
    ) {
        if self
            .decl_write_claims
            .insert(decl_id, (position, reads_through_call_or_index))
            != Some((position, reads_through_call_or_index))
        {
            derivation::note_write(|| FactKey::DeclClaim(decl_id));
        }
    }

    fn commit_type_cache(&mut self, owner: LuaTypeOwner, cache: LuaTypeCache) {
        let file_id = owner.get_file_id();
        let replaced = self.insert_type_cache(owner.clone(), cache);
        self.in_filed_type_owner
            .entry(file_id)
            .or_default()
            .insert(owner.clone());
        if replaced && self.fact_metadata.remove(&owner).is_some() {
            self.drop_fact_refs(&owner);
            self.mark_inference_derived_state_dirty();
        }
    }

    /// The type-cache owners recorded for a file, used to re-derive a file's
    /// decls after a late index (e.g. vgui parent chains) makes a broad fallback
    /// resolvable.
    pub fn file_type_owners(&self, file_id: FileId) -> Option<&HashSet<LuaTypeOwner>> {
        self.in_filed_type_owner.get(&file_id)
    }

    pub fn get_file_type_decl_ids(&self, file_id: FileId) -> Option<&Vec<LuaTypeDeclId>> {
        derivation::note_read(|| FactKey::TypeFile(file_id));
        self.file_types.get(&file_id)
    }

    pub fn force_bind_type(&mut self, owner: LuaTypeOwner, cache: LuaTypeCache) {
        if self.write_under_fallback(&owner, |index, _| {
            Some(StatementType {
                cache: Some(index.normalized_cache(&owner, cache.clone())),
                metadata: None,
            })
        }) {
            return;
        }
        let file_id = owner.get_file_id();
        self.insert_type_cache(owner.clone(), cache);
        self.in_filed_type_owner
            .entry(file_id)
            .or_default()
            .insert(owner.clone());
        if self.fact_metadata.remove(&owner).is_some() {
            self.drop_fact_refs(&owner);
            self.mark_inference_derived_state_dirty();
        }
        self.record_source_write(&owner);
    }

    /// Fact-carrying counterpart of [`bind_type`](Self::bind_type); the same
    /// bottom-placeholder exception applies.
    pub fn bind_type_fact(
        &mut self,
        owner: LuaTypeOwner,
        cache: LuaTypeCache,
        metadata: LuaTypeFactMetadata,
    ) {
        let metadata = metadata.normalized();
        if self.write_under_fallback(&owner, |index, statement| {
            if statement
                .cache
                .as_ref()
                .is_some_and(|existing| !cache.supersedes(existing))
            {
                return None;
            }
            Some(StatementType {
                cache: Some(index.normalized_cache(&owner, cache.clone())),
                metadata: Some(metadata.clone()),
            })
        }) {
            return;
        }
        if let Some(existing) = self.types.get(&owner)
            && !cache.supersedes(existing)
        {
            return;
        }

        let file_id = owner.get_file_id();
        self.insert_type_cache(owner.clone(), cache);
        self.in_filed_type_owner
            .entry(file_id)
            .or_default()
            .insert(owner.clone());
        self.fact_metadata.insert(owner.clone(), metadata);
        self.refresh_fact_refs(&owner);
        self.mark_inference_derived_state_dirty();
        self.record_source_write(&owner);
    }

    pub fn force_bind_type_fact(
        &mut self,
        owner: LuaTypeOwner,
        cache: LuaTypeCache,
        metadata: LuaTypeFactMetadata,
    ) {
        self.force_bind_type_fact_unchecked(owner, cache, metadata);
        self.mark_inference_derived_state_dirty();
    }

    /// Whether `owner`'s type is a contextual fallback: read off the slots its
    /// uses feed rather than off anything assigned to it.
    pub fn holds_contextual_fallback(&self, owner: &LuaTypeOwner) -> bool {
        self.get_type_fact(owner).is_some_and(|fact| {
            fact.provenance()
                .iter()
                .any(|step| step.event.kind == LuaInferenceProvenanceKind::ContextualUnknown)
        })
    }

    pub fn get_type_fact(&self, owner: &LuaTypeOwner) -> Option<LuaTypeFact> {
        let (cache, metadata) = self.read_type(owner);
        let cache = cache?;
        let fact = match metadata {
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
        self.bind_definition_fact_unchecked(definition, fact);
        self.mark_inference_derived_state_dirty();
    }

    pub fn get_definition_fact(&self, definition: &LuaDefinitionId) -> Option<&LuaTypeFact> {
        derivation::note_read(|| FactKey::DefinitionFact(*definition));
        self.definition_facts.get(definition)
    }

    pub fn get_inference_events_for_file(&self, file_id: FileId) -> &[LuaInferenceDiagnosticEvent] {
        debug_assert!(
            !self.derived_state_dirty,
            "inference events read before a flush"
        );
        self.inference_events_by_file
            .get(&file_id)
            .map(AsRef::as_ref)
            .unwrap_or_default()
    }

    pub fn files_depending_on_inference_support<S: std::hash::BuildHasher>(
        &self,
        file_ids: &std::collections::HashSet<FileId, S>,
    ) -> HashSet<FileId> {
        debug_assert!(
            !self.derived_state_dirty,
            "support dependents read before a flush"
        );
        let mut dependents = HashSet::default();
        for file_id in file_ids {
            if let Some(files) = self.support_file_dependents.get(file_id) {
                dependents.extend(files.iter().copied());
            }
        }
        dependents
    }

    /// The files whose inference read one of `nodes` as supporting evidence.
    pub fn files_depending_on_inference_nodes(
        &self,
        nodes: &[LuaInferenceNodeId],
    ) -> HashSet<FileId> {
        debug_assert!(
            !self.derived_state_dirty,
            "support dependents read before a flush"
        );
        let mut dependents = HashSet::default();
        for node in nodes {
            if let Some(files) = self.support_dependents.get(node) {
                dependents.extend(files.iter().copied());
            }
        }
        dependents
    }

    pub fn get_type_cache(&self, owner: &LuaTypeOwner) -> Option<&LuaTypeCache> {
        self.read_type_cache(owner)
    }

    fn read_type_cache(&self, owner: &LuaTypeOwner) -> Option<&LuaTypeCache> {
        self.read_type(owner).0
    }

    /// The one way an inference reads a stored type and its metadata, so every
    /// such read is noted as a derivation read. Anything reaching `types` on
    /// behalf of a caller goes through here. The write paths compare against
    /// what they write over directly: it is not something they depend on.
    ///
    /// An evaluation computing `owner`'s type again reads what the statements
    /// left under a fallback covering it: a statement reads the type the
    /// others write, never the guess made from it.
    fn read_type(
        &self,
        owner: &LuaTypeOwner,
    ) -> (Option<&LuaTypeCache>, Option<&LuaTypeFactMetadata>) {
        let stored = self.types.get(owner);
        if let Some(stored) = stored {
            if !stored.read.note(
                || FactKey::TypeCache(owner.clone()),
                |key| matches!(key, FactKey::TypeCache(stored) if stored == owner),
            ) {
                derivation::note_type_cache_read(owner);
            }
        } else {
            derivation::note_type_cache_read(owner);
        }
        match self.statement_type(owner).filter(|_| {
            stored.map_or_else(
                || derivation::recomputes(FactKey::TypeCache(owner.clone())),
                |stored| stored.read.recomputes(owner),
            )
        }) {
            Some(statement) => (statement.cache.as_ref(), statement.metadata.as_ref()),
            None => (
                stored.map(|stored| &stored.cache),
                self.fact_metadata.get(owner),
            ),
        }
    }

    /// What the statements left under the fallback covering `owner`, if one
    /// does.
    fn statement_type(&self, owner: &LuaTypeOwner) -> Option<&StatementType> {
        self.fallbacks.get(&owner.get_file_id())?.get(owner)
    }

    /// Lands a write of `owner`'s type under the fallback covering it, unless
    /// the writer is that fallback's: `write` maps what the statements left
    /// to what they leave now, `None` for no write. Returns whether it landed
    /// there; otherwise the caller writes the stored type, which this first
    /// covers when the writer is `owner`'s file's local fallbacks.
    ///
    /// A writer that was not computing `owner`'s type again read the fallback
    /// over it, so it runs again: it now produces the type, and reads what
    /// the statements leave.
    fn write_under_fallback(
        &mut self,
        owner: &LuaTypeOwner,
        write: impl FnOnce(&Self, &StatementType) -> Option<StatementType>,
    ) -> bool {
        if derivation::writes_fallback_of(owner.get_file_id()) {
            self.cover_with_fallback(owner);
            return false;
        }
        let Some(statement) = self.statement_type(owner) else {
            return false;
        };
        let Some(written) = write(self, statement) else {
            return true;
        };
        self.set_statement_type(owner, written);
        if !derivation::recomputes(FactKey::TypeCache(owner.clone())) {
            derivation::rerun_current();
        }
        true
    }

    /// Keeps what the statements left for `owner` under the fallback about to
    /// be written over it, the first time.
    fn cover_with_fallback(&mut self, owner: &LuaTypeOwner) {
        let file_id = owner.get_file_id();
        if self
            .fallbacks
            .get(&file_id)
            .is_some_and(|covered| covered.contains_key(owner))
        {
            return;
        }
        let statement = StatementType {
            cache: self.types.get(owner).map(|stored| stored.cache.clone()),
            metadata: self.fact_metadata.get(owner).cloned(),
        };
        if let Some(cache) = &statement.cache {
            self.cache_refs.add(file_id, cache.as_type());
        }
        self.fallbacks
            .entry(file_id)
            .or_default()
            .insert(owner.clone(), statement);
        derivation::note_write(|| FactKey::TypeFallbacks(file_id));
    }

    /// Replaces what the statements left under the fallback covering `owner`.
    fn set_statement_type(&mut self, owner: &LuaTypeOwner, written: StatementType) {
        let file_id = owner.get_file_id();
        let Some(statement) = self
            .fallbacks
            .get_mut(&file_id)
            .and_then(|covered| covered.get_mut(owner))
        else {
            return;
        };
        if let Some(cache) = &written.cache {
            self.cache_refs.add(file_id, cache.as_type());
        }
        let metadata_moved = statement.metadata != written.metadata;
        let previous = std::mem::replace(statement, written);
        if let Some(cache) = &previous.cache {
            self.cache_refs.remove(file_id, cache.as_type());
        }
        if metadata_moved {
            self.refresh_fact_refs(owner);
        }
        derivation::note_write(|| FactKey::TypeCache(owner.clone()));
    }

    /// Drops the types `file_id`'s local fallbacks published, restoring what
    /// the statements left under each.
    pub(crate) fn retract_fallbacks(&mut self, file_id: FileId) {
        let Some(covered) = self.fallbacks.remove(&file_id) else {
            return;
        };
        derivation::note_write(|| FactKey::TypeFallbacks(file_id));
        let mut covered = covered.into_iter().collect::<Vec<_>>();
        covered.sort_by(|(left, _), (right, _)| left.stable_cmp(right));
        for (owner, statement) in covered {
            if let Some(cache) = &statement.cache {
                self.cache_refs.remove(file_id, cache.as_type());
            }
            self.restore_type(
                &owner,
                statement.cache.map(|cache| (cache, statement.metadata)),
            );
        }
    }

    pub(crate) fn force_bind_type_fact_unchecked(
        &mut self,
        owner: LuaTypeOwner,
        cache: LuaTypeCache,
        metadata: LuaTypeFactMetadata,
    ) -> FileId {
        let file_id = owner.get_file_id();
        let metadata = metadata.normalized();
        if self.write_under_fallback(&owner, |index, _| {
            Some(StatementType {
                cache: Some(index.normalized_cache(&owner, cache.clone())),
                metadata: Some(metadata.clone()),
            })
        }) {
            return file_id;
        }
        self.insert_type_cache(owner.clone(), cache);
        self.in_filed_type_owner
            .entry(file_id)
            .or_default()
            .insert(owner.clone());
        self.fact_metadata.insert(owner.clone(), metadata);
        self.refresh_fact_refs(&owner);
        self.record_source_write(&owner);
        file_id
    }

    /// Stores `cache`, keeping [`Self::cache_refs`] in step, and reports
    /// whether a cache was replaced.
    fn insert_type_cache(&mut self, owner: LuaTypeOwner, cache: LuaTypeCache) -> bool {
        let cache = self.normalized_cache(&owner, cache);
        note_type_written(&owner);
        match (&owner, cache.as_type()) {
            (LuaTypeOwner::Decl(decl_id), LuaType::Ref(type_id) | LuaType::Def(type_id))
                if self.is_table_literal_decl(decl_id) =>
            {
                self.bind_decl_to_type(*decl_id, type_id.clone());
            }
            (LuaTypeOwner::Decl(decl_id), _) => {
                self.unbind_decl(decl_id);
            }
            _ => {}
        }
        let file_id = owner.get_file_id();
        self.cache_refs.add(file_id, cache.as_type());
        let previous = match self.types.entry(owner) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                std::mem::replace(&mut entry.get_mut().cache, cache)
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(TypeFact {
                    cache,
                    read: derivation::FactRead::default(),
                });
                return false;
            }
        };
        self.cache_refs.remove(file_id, previous.as_type());
        true
    }

    /// `cache` as `owner` stores it.
    fn normalized_cache(&self, owner: &LuaTypeOwner, cache: LuaTypeCache) -> LuaTypeCache {
        // `never` describes an expression that cannot produce a value — indexing
        // a `nil` a failed read floored to, a branch the analyzer proved
        // unreachable. A member's cache instead states what the field may hold,
        // and a write whose right-hand side cannot produce a value contributes
        // no evidence: `unknown` is the honest record. An inferred `never`
        // member would render the field uninhabited, which member-level checks
        // read as a declared contradiction (`integer & string`) — a category
        // error when the `never` came out of inference rather than a `---@field
        // x never` doc (those arrive as `DocType` caches and stay).
        let cache = match (owner, &cache) {
            (LuaTypeOwner::Member(_), LuaTypeCache::InferType(LuaType::Never)) => {
                LuaTypeCache::InferType(LuaType::Unknown)
            }
            _ => cache,
        };
        match self.with_table_classes(cache.as_type()) {
            Some(typ) => match cache {
                LuaTypeCache::DocType(_) => LuaTypeCache::DocType(typ),
                LuaTypeCache::InferType(_) => LuaTypeCache::InferType(typ),
            },
            None => cache,
        }
    }

    /// Keeps `owner`'s current value as its source value when the write that
    /// just landed belongs to the source layer.
    fn record_source_write(&mut self, owner: &LuaTypeOwner) {
        if !derivation::writes_source() {
            return;
        }
        match self.types.get(owner) {
            Some(cache) => {
                let metadata = self.fact_metadata.get(owner).cloned();
                self.source_types
                    .insert(owner.clone(), (cache.cache.clone(), metadata));
            }
            None => {
                self.source_types.remove(owner);
            }
        }
    }

    /// Restores `owner` to its source value, dropping whatever derivations
    /// wrote over it; under a fallback covering it, what the statements left.
    pub(crate) fn reset_type_cache(&mut self, owner: &LuaTypeOwner) {
        let source = self.source_types.get(owner).cloned();
        if self.statement_type(owner).is_some() {
            let (cache, metadata) =
                source.map_or((None, None), |(cache, metadata)| (Some(cache), metadata));
            self.set_statement_type(owner, StatementType { cache, metadata });
            return;
        }
        self.restore_type(owner, source);
    }

    /// Stores `value` as `owner`'s type and metadata, or none.
    fn restore_type(
        &mut self,
        owner: &LuaTypeOwner,
        value: Option<(LuaTypeCache, Option<LuaTypeFactMetadata>)>,
    ) {
        derivation::note_write(|| FactKey::TypeCache(owner.clone()));
        match value {
            Some((cache, metadata)) => {
                self.insert_type_cache(owner.clone(), cache);
                match metadata {
                    Some(metadata) => {
                        self.fact_metadata.insert(owner.clone(), metadata);
                        self.refresh_fact_refs(owner);
                    }
                    None => {
                        if self.fact_metadata.remove(owner).is_some() {
                            self.drop_fact_refs(owner);
                        }
                    }
                }
            }
            None => {
                if let Some(set) = self.in_filed_type_owner.get_mut(&owner.get_file_id()) {
                    set.remove(owner);
                }
                if let LuaTypeOwner::Decl(decl_id) = owner {
                    self.unbind_decl(decl_id);
                }
                if let Some(previous) = self.types.remove(owner) {
                    self.cache_refs
                        .remove(owner.get_file_id(), previous.as_type());
                }
                if self.fact_metadata.remove(owner).is_some() {
                    self.drop_fact_refs(owner);
                }
            }
        }
        self.mark_inference_derived_state_dirty();
    }

    pub(crate) fn reset_decl_claim(&mut self, decl_id: &LuaDeclId) {
        if self.decl_write_claims.remove(decl_id).is_some() {
            derivation::note_write(|| FactKey::DeclClaim(*decl_id));
        }
    }

    pub(crate) fn reset_definition_fact(&mut self, definition: &LuaDefinitionId) {
        if self.definition_facts.remove(definition).is_some() {
            self.drop_definition_fact_refs(definition);
            derivation::note_write(|| FactKey::DefinitionFact(*definition));
        }
    }

    pub(crate) fn bind_definition_fact_unchecked(
        &mut self,
        definition: LuaDefinitionId,
        fact: LuaTypeFact,
    ) -> FileId {
        let file_id = definition.file_id();
        if self.definition_facts.get(&definition) != Some(&fact) {
            self.definition_facts.insert(definition, fact);
            self.refresh_definition_fact_refs(&definition);
            derivation::note_write(|| FactKey::DefinitionFact(definition));
        }
        file_id
    }

    /// The files `metadata` names: every step's event node, source and
    /// support files, plus the files the step types point at. Exactly the
    /// files [`remap_provenance`] consults through `remap`, so a fact whose
    /// set skips a file remaps to itself there.
    fn metadata_referenced_files(metadata: &LuaTypeFactMetadata) -> HashSet<FileId> {
        let mut files = HashSet::default();
        collect_provenance_files(metadata.provenance.iter(), &mut files);
        files
    }

    /// Re-points the reverse edges of `owner`'s fact at what the stored
    /// metadata now names. Call on every metadata write; the remap only moves
    /// identities within one file, so it never changes the file set and skips
    /// this.
    fn refresh_fact_refs(&mut self, owner: &LuaTypeOwner) {
        let mut files = self
            .fact_metadata
            .get(owner)
            .map(Self::metadata_referenced_files)
            .unwrap_or_default();
        if let Some(metadata) = self
            .statement_type(owner)
            .and_then(|statement| statement.metadata.as_ref())
        {
            files.extend(Self::metadata_referenced_files(metadata));
        }
        replace_reverse_edges(
            &mut self.fact_refs_by_file,
            &mut self.fact_ref_files,
            owner.clone(),
            files,
        );
    }

    fn drop_fact_refs(&mut self, owner: &LuaTypeOwner) {
        if self.statement_type(owner).is_some() {
            self.refresh_fact_refs(owner);
            return;
        }
        remove_reverse_edges(&mut self.fact_refs_by_file, &mut self.fact_ref_files, owner);
    }

    fn refresh_definition_fact_refs(&mut self, definition: &LuaDefinitionId) {
        let files = self
            .definition_facts
            .get(definition)
            .map(type_fact_referenced_files)
            .unwrap_or_default();
        replace_reverse_edges(
            &mut self.definition_fact_refs_by_file,
            &mut self.definition_fact_ref_files,
            *definition,
            files,
        );
    }

    fn drop_definition_fact_refs(&mut self, definition: &LuaDefinitionId) {
        remove_reverse_edges(
            &mut self.definition_fact_refs_by_file,
            &mut self.definition_fact_ref_files,
            definition,
        );
    }

    /// Marks the inference-derived state stale.
    ///
    /// The rebuild reads every fact in the index, so it is deferred to
    /// [`flush_inference_derived_state`](Self::flush_inference_derived_state)
    /// rather than run once per binding. No reader runs inside a slice of an
    /// engine run, and each slice ends with a flush: the events are read by
    /// the inference-trust diagnostic and the support dependents by reindex
    /// expansion.
    pub(crate) fn mark_inference_derived_state_dirty(&mut self) {
        self.derived_state_dirty = true;
    }

    /// Whether a binding left the inference-derived state for a flush.
    pub(crate) fn inference_derived_state_is_stale(&self) -> bool {
        self.derived_state_dirty
    }

    /// Rebuilds the inference-derived state if any binding marked it stale.
    pub(crate) fn flush_inference_derived_state(&mut self) {
        if !self.derived_state_dirty {
            return;
        }
        self.derived_state_dirty = false;

        let mut events_by_file: HashMap<FileId, Vec<OwnedEvent>> = HashMap::default();
        let mut support_dependents: HashMap<LuaInferenceNodeId, HashSet<FileId>> =
            HashMap::default();

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
                Some(owner),
                &fact,
                &mut events_by_file,
                &mut support_dependents,
            );
        }

        for (definition, fact) in &self.definition_facts {
            collect_fact_derived_state(
                definition.file_id(),
                None,
                fact,
                &mut events_by_file,
                &mut support_dependents,
            );
        }

        self.inference_events_by_file = events_by_file
            .into_iter()
            .map(|(file_id, mut events)| {
                events.sort_by(|left, right| {
                    left.0.event.stable_cmp(&right.0.event).then_with(|| {
                        match (&left.1, &right.1) {
                            (Some(left), Some(right)) => left.stable_cmp(right),
                            (Some(_), None) => std::cmp::Ordering::Less,
                            (None, Some(_)) => std::cmp::Ordering::Greater,
                            (None, None) => std::cmp::Ordering::Equal,
                        }
                    })
                });
                events.dedup_by(|left, right| left.0.event == right.0.event);
                (
                    file_id,
                    events.into_iter().map(|(event, _)| event).collect(),
                )
            })
            .collect();
        self.support_file_dependents = Default::default();
        for (node, dependents) in &support_dependents {
            self.support_file_dependents
                .entry(node.file_id())
                .or_default()
                .extend(dependents.iter().copied());
        }
        self.support_dependents = support_dependents.into();
    }

    pub fn iter_type_caches(&self) -> impl Iterator<Item = (&LuaTypeOwner, &LuaTypeCache)> {
        self.types.iter().map(|(owner, fact)| (owner, &fact.cache))
    }

    /// Registers the class a table literal became. Stored types that name
    /// the literal name the class instead, and their writers run again.
    pub(crate) fn register_table_class(&mut self, table: InFiled<TextRange>, class_type: LuaType) {
        let Some(id) = derivation::current() else {
            return;
        };
        self.record_contribution(TypeContribution::TableClass {
            table: table.clone(),
        });
        derivation::note_write(|| FactKey::TableClass(table.clone()));
        let registrations = self.table_classes.entry(table).or_default();
        let at = registrations.partition_point(|(existing, _)| *existing <= id);
        registrations.insert(at, (id, class_type));
    }

    fn release_table_class(&mut self, table: &InFiled<TextRange>, id: &DerivationId) {
        let Some(registrations) = self.table_classes.get_mut(table) else {
            return;
        };
        let Some(at) = registrations
            .iter()
            .position(|(existing, _)| existing == id)
        else {
            return;
        };
        registrations.remove(at);
        if registrations.is_empty() {
            self.table_classes.remove(table);
        }
        derivation::note_write(|| FactKey::TableClass(table.clone()));
    }

    /// `typ` with every table literal a registration turned into a class
    /// replaced by the class.
    fn with_table_classes(&self, typ: &LuaType) -> Option<LuaType> {
        let mut tables = Vec::new();
        collect_table_consts(typ, &mut tables);
        if tables.is_empty() {
            return None;
        }
        let mut replacements = std::collections::HashMap::new();
        for table in tables {
            derivation::note_read(|| FactKey::TableClass(table.clone()));
            if let Some((_, class_type)) = self.table_classes.get(&table).and_then(|r| r.first()) {
                replacements.insert(table, class_type.clone());
            }
        }
        replace_table_consts_in_type(typ, &replacements)
    }

    /// Drops the cached types of members that were removed on their own,
    /// rather than as part of a file sweep.
    ///
    /// A member whose owning table literal is gone leaves a cache entry no
    /// re-index of its own file will reach. Returns the owners that had one,
    /// for their producers to compute again.
    pub fn remove_member_type_caches(
        &mut self,
        member_ids: &[crate::LuaMemberId],
    ) -> Vec<LuaTypeOwner> {
        let mut removed = Vec::new();
        for member_id in member_ids {
            let owner = LuaTypeOwner::Member(*member_id);
            derivation::note_write(|| FactKey::TypeCache(owner.clone()));
            if let Some(set) = self.in_filed_type_owner.get_mut(&member_id.file_id) {
                set.remove(&owner);
            }
            // `cache_refs` has to come off with the cache, the way
            // `insert_type_cache` and the file sweep both keep them in step.
            if let Some(previous) = self.types.remove(&owner) {
                self.cache_refs
                    .remove(member_id.file_id, previous.as_type());
                removed.push(owner.clone());
            }
            self.fact_metadata.remove(&owner);
            self.drop_fact_refs(&owner);
            self.source_types.remove(&owner);
        }
        removed
    }

    /// Rewrites every cached type, definition fact and inference-support array
    /// that names an identity in the edited file, except the types of
    /// `rebuilt`, which the file's re-indexed source layer wrote at the new
    /// positions already.
    ///
    /// Returns the facts whose entry named something the edit destroyed: those
    /// cannot be rewritten and have to be computed again instead.
    pub fn remap_file_identities(
        &mut self,
        remap: &crate::FileRemap,
        rebuilt: &HashSet<LuaTypeOwner>,
    ) -> Vec<crate::LostFact> {
        let mut lost = Vec::new();

        // Only caches that actually name an identity in the edited file can
        // contain something the edit moved or destroyed, and `cache_refs`
        // already records which files those are. Scanning every cache in the
        // workspace here would put a full-index walk on the per-keystroke path.
        let candidate_owners: Vec<LuaTypeOwner> = self
            .cache_refs
            .owners_referencing_file(remap.file_id)
            .filter_map(|owner_file_id| self.in_filed_type_owner.get(&owner_file_id))
            .flatten()
            .cloned()
            .collect();

        let mut updates = Vec::new();
        let mut statement_updates = Vec::new();
        for owner in candidate_owners {
            if rebuilt.contains(&owner) {
                continue;
            }
            if let Some(statement) = self.statement_type(&owner)
                && let Some(cache) = &statement.cache
            {
                let result = remap_identities_in_type(cache.as_type(), remap);
                if result.lost {
                    lost.push(statement_fact(&owner));
                } else if let Some(new_type) = result.typ {
                    let cache = match cache {
                        LuaTypeCache::DocType(_) => LuaTypeCache::DocType(new_type),
                        LuaTypeCache::InferType(_) => LuaTypeCache::InferType(new_type),
                    };
                    statement_updates.push((
                        owner.clone(),
                        StatementType {
                            cache: Some(cache),
                            metadata: statement.metadata.clone(),
                        },
                    ));
                }
            }
            let Some(cache) = self.types.get(&owner) else {
                continue;
            };
            let result = remap_identities_in_type(cache.as_type(), remap);
            if result.lost {
                // A lost nested identity is left in place; the owner's type is
                // computed again instead of being rewritten to a guessed value.
                lost.push(self.visible_type_fact(&owner));
            } else if let Some(new_type) = result.typ {
                let new_cache = match &cache.cache {
                    LuaTypeCache::DocType(_) => LuaTypeCache::DocType(new_type),
                    LuaTypeCache::InferType(_) => LuaTypeCache::InferType(new_type),
                };
                updates.push((owner, new_cache));
            }
        }
        // `candidate_owners` comes out of a hash set, so the writes are ordered
        // by a real key before they are applied.
        statement_updates.sort_by(|left, right| left.0.stable_cmp(&right.0));
        for (owner, statement) in statement_updates {
            self.set_statement_type(&owner, statement);
        }
        updates.sort_by(|left, right| left.0.stable_cmp(&right.0));
        let mut changed_files = HashSet::default();
        for (owner, new_cache) in updates {
            changed_files.insert(owner.get_file_id());
            self.insert_type_cache(owner, new_cache);
        }
        if !changed_files.is_empty() {
            self.mark_inference_derived_state_dirty();
        }
        lost.extend(self.remap_inference_facts(remap));
        lost
    }

    /// The fact holding `owner`'s visible type: its file's local fallbacks
    /// when they cover it, else the type the statements wrote.
    fn visible_type_fact(&self, owner: &LuaTypeOwner) -> crate::LostFact {
        if self.statement_type(owner).is_some() {
            let file_id = owner.get_file_id();
            crate::LostFact::new(file_id, FactKey::TypeFallbacks(file_id))
        } else {
            statement_fact(owner)
        }
    }

    /// Rewrites the provenance of every stored fact, and the payload of every
    /// definition fact, through `remap`.
    ///
    /// Only the facts that name the edited file are visited, located through
    /// the reverse reference maps rather than by scanning every fact: any
    /// other fact remaps each of its nodes, sources and nested identities to
    /// itself. A definition fact's payload comes from inference and can name
    /// another file's table literal or signature; its provenance names the
    /// inference nodes the fact was derived from, which is what
    /// `files_depending_on_inference_nodes` indexes.
    fn remap_inference_facts(&mut self, remap: &crate::FileRemap) -> Vec<crate::LostFact> {
        let mut lost_facts = Vec::new();
        let mut metadata_updates = Vec::new();
        let candidates = self
            .fact_refs_by_file
            .get(&remap.file_id)
            .map(|owners| owners.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut statement_updates = Vec::new();
        for owner in candidates {
            let remapped = |metadata: &LuaTypeFactMetadata, lost: &mut bool| {
                remap_provenance(&metadata.provenance, remap, lost).map(|provenance| {
                    LuaTypeFactMetadata {
                        confidence: metadata.confidence,
                        base_provenance_kind: metadata.base_provenance_kind,
                        provenance,
                    }
                })
            };
            let mut statement_lost = false;
            if let Some(statement) = self.statement_type(&owner)
                && let Some(metadata) = &statement.metadata
                && let Some(metadata) = remapped(metadata, &mut statement_lost)
            {
                statement_updates.push((
                    owner.clone(),
                    StatementType {
                        cache: statement.cache.clone(),
                        metadata: Some(metadata),
                    },
                ));
            }
            let mut visible_lost = false;
            if let Some(metadata) = self.fact_metadata.get(&owner)
                && let Some(metadata) = remapped(metadata, &mut visible_lost)
            {
                metadata_updates.push((owner.clone(), metadata));
            }
            if statement_lost {
                lost_facts.push(statement_fact(&owner));
            }
            if visible_lost {
                lost_facts.push(self.visible_type_fact(&owner));
            }
        }
        statement_updates.sort_by(|left, right| left.0.stable_cmp(&right.0));
        for (owner, statement) in statement_updates {
            self.set_statement_type(&owner, statement);
        }
        // Ordered by a real key rather than hash-map iteration order.
        metadata_updates.sort_by(|left, right| left.0.stable_cmp(&right.0));
        for (owner, metadata) in metadata_updates {
            derivation::note_write(|| FactKey::TypeCache(owner.clone()));
            self.fact_metadata.insert(owner, metadata);
        }

        let mut fact_updates = Vec::new();
        let candidates = self
            .definition_fact_refs_by_file
            .get(&remap.file_id)
            .map(|definitions| definitions.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        for definition in candidates {
            let Some(fact) = self.definition_facts.get(&definition) else {
                continue;
            };
            let mut lost = false;
            if let Some(fact) = remap_type_fact(fact, remap, &mut lost) {
                fact_updates.push((definition, fact));
            }
            if lost {
                lost_facts.push(crate::LostFact::new(
                    definition.file_id(),
                    FactKey::DefinitionFact(definition),
                ));
            }
        }
        fact_updates.sort_by(|left, right| left.0.stable_cmp(&right.0));
        for (definition, fact) in fact_updates {
            self.definition_facts.insert(definition, fact);
            derivation::note_write(|| FactKey::DefinitionFact(definition));
        }
        lost_facts
    }

    /// What `fork`, a task fork of `frozen`, changed that the merged state
    /// decides: subclass lists follow the merged super edges, and the
    /// declaration scan the merged declarations when both changed it.
    fn derived_to_rebuild(&self, frozen: &Self, fork: &Self) -> DerivedToRebuild {
        let subclasses = CowShards::changed_keys(&frozen.supers, &fork.supers)
            .into_iter()
            .map(|decl_id| {
                let bases = [self, frozen, fork]
                    .into_iter()
                    .flat_map(|index| super_bases(index.supers.get(&decl_id)))
                    .cloned()
                    .collect();
                (decl_id, bases)
            })
            .collect();
        let decl_scan = fork.decl_scan_fingerprint != frozen.decl_scan_fingerprint
            && self.decl_scan_fingerprint != frozen.decl_scan_fingerprint
            && self.decl_scan_fingerprint != fork.decl_scan_fingerprint;
        DerivedToRebuild {
            subclasses,
            decl_scan,
        }
    }

    /// Installs the declaration scan of a superstep's `forks` in fork order,
    /// leaving each fork the frozen one, and returns what the merged state
    /// rebuilds once every fork is in: every declaration a fork changed the
    /// super edges of, with every base its edges named in the index, `frozen`
    /// or a fork, and the scan when any fork changed it where the index had
    /// too. Rebuilt once, each holds what rebuilding it after each such fork
    /// leaves: a subclass list follows the final super edges, and once the
    /// scan is summed afresh it differs from `frozen`, so the last fork that
    /// changes it sums it afresh too.
    fn install_decl_scan(
        &mut self,
        frozen: &Self,
        forks: &mut [(usize, Self)],
    ) -> DerivedToRebuild {
        let mut subclasses: HashMap<LuaTypeDeclId, HashSet<LuaTypeDeclId>> = HashMap::default();
        let mut decl_scan = false;
        for (_, fork) in forks {
            let planned = self.derived_to_rebuild(frozen, fork);
            for (decl_id, bases) in planned.subclasses {
                subclasses.entry(decl_id).or_default().extend(bases);
            }
            decl_scan |= planned.decl_scan;
            let forked = std::mem::replace(
                &mut fork.decl_scan_fingerprint,
                frozen.decl_scan_fingerprint,
            );
            let conflicted = crate::db_index::cow_shards::MergeFork::install_merging(
                &mut self.decl_scan_fingerprint,
                &frozen.decl_scan_fingerprint,
                forked,
                crate::db_index::cow_shards::rebuilt,
            );
            debug_assert!(!conflicted, "a rebuilt scan never conflicts");
        }
        let mut subclasses = subclasses.into_iter().collect::<Vec<_>>();
        subclasses.sort_by(|left, right| left.0.stable_cmp(&right.0));
        DerivedToRebuild {
            subclasses,
            decl_scan,
        }
    }

    fn rebuild_derived(&mut self, rebuild: DerivedToRebuild) {
        for (decl_id, bases) in rebuild.subclasses {
            let derives = super_bases(self.supers.get(&decl_id))
                .cloned()
                .collect::<HashSet<_>>();
            for base_id in bases {
                let listed = self
                    .sub_types
                    .get(&base_id)
                    .is_some_and(|children| children.contains(&decl_id));
                if derives.contains(&base_id) && !listed {
                    self.sub_types
                        .entry(base_id)
                        .or_default()
                        .push(decl_id.clone());
                } else if !derives.contains(&base_id)
                    && listed
                    && let Some(children) = self.sub_types.get_mut(&base_id)
                {
                    children.retain(|child| *child != decl_id);
                    if children.is_empty() {
                        self.sub_types.remove(&base_id);
                    }
                }
            }
        }
        if rebuild.decl_scan {
            self.decl_scan_fingerprint = self
                .full_name_type_map
                .keys()
                .map(|decl_id| derivation::fingerprint_of(decl_id.get_id()))
                .fold(derivation::ABSENT, Fingerprint::wrapping_add);
        }
    }

    fn add_to_decl_scan(&mut self, decl_id: &LuaTypeDeclId) {
        self.decl_scan_fingerprint = self
            .decl_scan_fingerprint
            .wrapping_add(derivation::fingerprint_of(decl_id.get_id()));
        derivation::note_write(|| FactKey::TypeDeclScan);
    }

    fn remove_from_decl_scan(&mut self, decl_id: &LuaTypeDeclId) {
        self.decl_scan_fingerprint = self
            .decl_scan_fingerprint
            .wrapping_sub(derivation::fingerprint_of(decl_id.get_id()));
        derivation::note_write(|| FactKey::TypeDeclScan);
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        if let FactKey::TableClass(table) = key {
            return derivation::fingerprint_opt(
                self.table_classes
                    .get(table)
                    .and_then(|registrations| registrations.first())
                    .map(|(_, class_type)| class_type),
            );
        }
        if let FactKey::TypeContrib(id) = key {
            return derivation::fingerprint_opt(self.derived_contributions.get(id));
        }
        if let FactKey::SourceTypeDecl(id) = key {
            if self
                .source_decl_locations
                .get(id)
                .is_none_or(|locations| locations.is_empty())
            {
                return derivation::ABSENT;
            }
            let decl = self.full_name_type_map.get(id);
            return derivation::fingerprint_of(
                &decl.map(|decl| (decl.kind(), decl.get_attribute_type())),
            );
        }
        match key {
            FactKey::TypeCache(owner) => {
                let Some(cache) = self.types.get(owner) else {
                    return derivation::ABSENT;
                };
                let metadata = self.fact_metadata.get(owner);
                // What the statements left under a fallback moves the key
                // too: the fallback was guessed from it.
                match self.statement_type(owner).filter(|statement| {
                    statement.cache.as_ref() != Some(&cache.cache)
                        || statement.metadata.as_ref() != metadata
                }) {
                    Some(statement) => derivation::fingerprint_of(&(cache, metadata, statement)),
                    None => derivation::fingerprint_of(&(cache, metadata)),
                }
            }
            FactKey::TypeFallbacks(file_id) => {
                self.fallbacks
                    .get(file_id)
                    .map_or(derivation::ABSENT, |covered| {
                        covered
                            .keys()
                            .fold(derivation::ABSENT, |sum, owner| {
                                sum.wrapping_add(derivation::fingerprint_of(&(
                                    owner,
                                    self.types.get(owner),
                                    self.fact_metadata.get(owner),
                                )))
                            })
                            .max(1)
                    })
            }
            FactKey::DefinitionFact(definition) => {
                derivation::fingerprint_opt(self.definition_facts.get(definition))
            }
            FactKey::TypeDecl(decl_id) => self
                .full_name_type_map
                .get(decl_id)
                .map_or(derivation::ABSENT, |decl| {
                    derivation::fingerprint_of(&decl.declaration())
                }),
            FactKey::TypeDeclLocations(decl_id) => self
                .full_name_type_map
                .get(decl_id)
                .map_or(derivation::ABSENT, |decl| {
                    derivation::fingerprint_of(decl.locations())
                }),
            FactKey::TypeDeclFiles(decl_id) => self
                .full_name_type_map
                .get(decl_id)
                .map_or(derivation::ABSENT, |decl| {
                    derivation::fingerprint_of(&decl.sorted_location_flags())
                }),
            FactKey::TypeSupers(decl_id) => derivation::fingerprint_opt(self.supers.get(decl_id)),
            FactKey::TypeSuperTypes(decl_id) => {
                self.supers
                    .get(decl_id)
                    .map_or(derivation::ABSENT, |supers| {
                        derivation::fingerprint_of(
                            &distinct_super_types(supers).collect::<Vec<_>>(),
                        )
                    })
            }
            FactKey::TypeSuperEdges(decl_id) => {
                self.supers
                    .get(decl_id)
                    .map_or(derivation::ABSENT, |supers| {
                        derivation::fingerprint_of(
                            &supers
                                .iter()
                                .map(|entry| (entry.file_id, &entry.value.typ))
                                .collect::<Vec<_>>(),
                        )
                    })
            }
            FactKey::TypeGenericParams(decl_id) => {
                derivation::fingerprint_opt(self.generic_params.get(decl_id))
            }
            FactKey::SubTypes(decl_id) => {
                let Some(children) = self.sub_types.get(decl_id) else {
                    return derivation::ABSENT;
                };
                let mut children = children.iter().collect::<Vec<_>>();
                children.sort_by(|left, right| left.stable_cmp(right));
                let children = children
                    .into_iter()
                    .map(LuaTypeDeclId::get_id)
                    .collect::<Vec<_>>();
                derivation::fingerprint_of(&children)
            }
            FactKey::BoundDecls(type_id, file_id) => {
                let mut decls = self
                    .type_bound_decls
                    .get(type_id)
                    .into_iter()
                    .flatten()
                    .filter(|decl_id| decl_id.file_id == *file_id)
                    .collect::<Vec<_>>();
                if decls.is_empty() {
                    return derivation::ABSENT;
                }
                decls.sort_unstable_by_key(|decl_id| decl_id.position);
                derivation::fingerprint_of(&decls)
            }
            FactKey::DeclClaim(decl_id) => self.decl_write_claims.get(decl_id).map_or(
                derivation::ABSENT,
                |(position, may_narrow)| {
                    derivation::fingerprint_of(&(derivation::Local(position), *may_narrow))
                },
            ),
            FactKey::TypeDeclScan => self.decl_scan_fingerprint,
            FactKey::TypeFile(file_id) => {
                let namespace = self.file_namespace.get(file_id);
                let using = self.file_using_namespace.get(file_id);
                let types = self.file_types.get(file_id);
                let sites = self.declaration_sites.get(file_id).map(|sites| {
                    let mut sites = sites
                        .iter()
                        .map(|(range, decl_id)| (InFiled::new(*file_id, *range), decl_id))
                        .collect::<Vec<_>>();
                    sites.sort_unstable_by_key(|(site, _)| (site.value.start(), site.value.end()));
                    sites
                });
                if namespace.is_none() && using.is_none() && types.is_none() && sites.is_none() {
                    return derivation::ABSENT;
                }
                derivation::fingerprint_of(&(namespace, using, types, sites))
            }
            _ => {
                debug_assert!(false, "{key:?} is not a type fact");
                derivation::ABSENT
            }
        }
    }

    pub fn files_with_type_caches_referencing_files<S: std::hash::BuildHasher>(
        &self,
        file_ids: &std::collections::HashSet<FileId, S>,
    ) -> HashSet<FileId> {
        let mut dependent_files = HashSet::default();
        let mut visited_decls = HashSet::default();
        for file_id in file_ids {
            dependent_files.extend(
                self.cache_refs
                    .owners_referencing_file(*file_id)
                    .filter(|owner_file_id| !file_ids.contains(owner_file_id)),
            );

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
                let locations = decl.locations();
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

    /// The files whose cached types reference any of `refs`.
    ///
    /// Unlike [`files_with_type_caches_referencing_files`](Self::files_with_type_caches_referencing_files)
    /// this is the raw reverse lookup: a [`TypeCacheRef::Decl`] answers the
    /// files that name the class, without consulting its definition sites.
    pub fn files_with_type_caches_referencing(&self, refs: &[TypeCacheRef]) -> HashSet<FileId> {
        let mut dependent_files = HashSet::default();
        for type_ref in refs {
            if let Some(owners) = self.cache_refs.owners(type_ref) {
                dependent_files.extend(owners.iter().copied());
            }
        }
        dependent_files
    }

    /// The identities declared in `file_id` that some cached type references.
    pub fn type_cache_refs_into_file(
        &self,
        file_id: FileId,
    ) -> impl Iterator<Item = &TypeCacheRef> {
        self.cache_refs.refs_into_file(file_id)
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
        typ.visit_type_identities(&mut |inner| {
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
                        let locations = decl.locations();
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
                        let locations = decl.locations();
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
        let mut ordered_removed = Vec::new();
        for &file_id in file_ids {
            if changed_files.insert(file_id) {
                ordered_removed.push(file_id);
                self.remove_file_raw(file_id);
            }
        }
        // Only the edges the removed files declared are visited, through the
        // declaring-file index, instead of retaining over every edge.
        let mut pruned_edges = 0;
        let mut affected_bases: HashSet<LuaTypeDeclId> = HashSet::default();
        for file_id in ordered_removed {
            let Some(records) = self.supers_by_file.remove(&file_id) else {
                continue;
            };
            for (child_id, entry) in records {
                let edge = (child_id.clone(), entry.clone());
                if let Some(holders) = self.super_holders.get_mut(&edge)
                    && let Some(at) = holders.iter().position(Option::is_none)
                {
                    holders.remove(at);
                    if !holders.is_empty() {
                        continue;
                    }
                    self.super_holders.remove(&edge);
                }
                if let LuaType::Ref(base_id) = &entry.value.typ {
                    affected_bases.insert(base_id.clone());
                }
                let Some(supers) = self.supers.get_mut(&child_id) else {
                    continue;
                };
                if let Some(at) = supers.iter().position(|existing| existing == &entry) {
                    supers.remove(at);
                    pruned_edges += 1;
                    note_supers_written(&child_id, entry.file_id);
                }
                if supers.is_empty() {
                    self.supers.remove(&child_id);
                }
            }
        }
        if pruned_edges > 0 {
            // The child lists stay consistent with the forward edges: a child
            // leaves a base's list only when no surviving edge names it, and
            // each touched list is re-sorted, matching the rebuilt index.
            let mut bases = affected_bases.into_iter().collect::<Vec<_>>();
            bases.sort_by(LuaTypeDeclId::stable_cmp);
            for base_id in bases {
                if let Some(children) = self.sub_types.get_mut(&base_id) {
                    children.retain(|child_id| {
                        self.supers.get(child_id).is_some_and(|supers| {
                            supers.iter().any(|entry| {
                                matches!(&entry.value.typ, LuaType::Ref(base) if base == &base_id)
                            })
                        })
                    });
                    children.sort_by(LuaTypeDeclId::stable_cmp);
                    if children.is_empty() {
                        self.sub_types.remove(&base_id);
                    }
                }
                derivation::note_write(|| FactKey::SubTypes(base_id.clone()));
            }
            // Pruning dropped an edge the cached map may have been derived
            // from; the next read rebuilds it lazily.
        }

        let mut dropped_definitions = Vec::new();
        self.definition_facts.retain(|definition, _| {
            if changed_files.contains(&definition.file_id()) {
                dropped_definitions.push(*definition);
                false
            } else {
                true
            }
        });
        for definition in dropped_definitions {
            self.drop_definition_fact_refs(&definition);
            derivation::note_write(|| FactKey::DefinitionFact(definition));
        }

        if !changed_files.is_empty() {
            self.mark_inference_derived_state_dirty();
        }
        self.flush_inference_derived_state();
    }

    fn clear(&mut self) {
        self.note_clear_writes();
        self.decl_scan_fingerprint = derivation::ABSENT;
        self.file_types.clear();
        self.declaration_sites.clear();
        self.file_namespace.clear();
        self.file_using_namespace.clear();
        self.full_name_type_map.clear();
        self.generic_params.clear();
        self.alias_sources.clear();
        self.extra_sources.clear();
        self.supers.clear();
        self.supers_by_file.clear();
        self.sub_types.clear();
        self.types.clear();
        self.source_types.clear();
        self.source_decl_locations.clear();
        self.derived_contributions.clear();
        self.location_holders.clear();
        self.super_holders.clear();
        self.table_classes.clear();
        self.fallbacks.clear();
        self.cache_refs = Default::default();
        self.in_filed_type_owner.clear();
        self.fact_metadata.clear();
        self.fact_ref_files.clear();
        self.fact_refs_by_file.clear();
        self.decl_write_claims.clear();
        self.decl_type_binds.clear();
        self.type_bound_decls.clear();
        self.table_literal_decls.clear();
        self.definition_facts.clear();
        self.definition_fact_ref_files.clear();
        self.definition_fact_refs_by_file.clear();
        self.inference_events_by_file.clear();
        self.support_dependents.clear();
        self.support_file_dependents.clear();
        self.derived_state_dirty = false;
    }
}

/// Notes a change to the super edges `decl_id` has in `file_id`, and so to
/// the realms at them.
fn note_supers_written(decl_id: &LuaTypeDeclId, file_id: FileId) {
    derivation::note_write(|| FactKey::TypeSupers(decl_id.clone()));
    derivation::note_write(|| FactKey::TypeSuperRealms(decl_id.clone(), file_id));
}

/// Notes a write of a type declaration that may move where it is declared.
fn note_decl_written(decl_id: &LuaTypeDeclId) {
    derivation::note_write(|| FactKey::TypeDecl(decl_id.clone()));
    derivation::note_write(|| FactKey::TypeDeclLocations(decl_id.clone()));
}

impl LuaTypeIndex {
    fn remove_file_raw(&mut self, file_id: FileId) {
        self.table_literal_decls.remove(&file_id);
        self.file_namespace.remove(&file_id);
        self.file_using_namespace.remove(&file_id);
        derivation::note_write(|| FactKey::TypeFile(file_id));
        self.declaration_sites.remove(&file_id);
        if let Some(type_id_list) = self.file_types.remove(&file_id) {
            for id in type_id_list {
                note_decl_written(&id);
                if let Some(locations) = self.source_decl_locations.get_mut(&id) {
                    derivation::note_write(|| FactKey::SourceTypeDecl(id.clone()));
                    locations.retain(|loc| loc.file_id != file_id);
                    if locations.is_empty() {
                        self.source_decl_locations.remove(&id);
                    }
                }
                let mut remove_type = false;
                if let Some(decl) = self.full_name_type_map.get_mut(&id) {
                    let location_holders = &mut self.location_holders;
                    decl.get_mut_locations().retain_mut(|loc| {
                        if loc.file_id != file_id {
                            return true;
                        }
                        let site = (id.clone(), file_id, loc.range);
                        let Some(holders) = location_holders.get_mut(&site) else {
                            return false;
                        };
                        holders.retain(|(holder, _)| holder.is_some());
                        if holders.is_empty() {
                            location_holders.remove(&site);
                            return false;
                        }
                        loc.flag = union_flags(holders);
                        true
                    });
                    if decl.get_mut_locations().is_empty() {
                        self.full_name_type_map.remove(&id);
                        self.remove_from_decl_scan(&id);
                        remove_type = true;
                        log::info!(
                            "type_index: type '{}' fully removed (file_id={:?})",
                            id.get_simple_name(),
                            file_id,
                        );
                    }
                }

                if let Some(sources) = self.extra_sources.get_mut(&id) {
                    sources.retain(|site, _| site.file_id != file_id);
                    if remove_type || sources.is_empty() {
                        self.extra_sources.remove(&id);
                    }
                    if !remove_type {
                        self.reselect_extra_source(&id);
                    }
                }
                if remove_type {
                    if self.generic_params.remove(&id).is_some() {
                        derivation::note_write(|| FactKey::TypeGenericParams(id.clone()));
                    }
                    self.alias_sources.remove(&id);
                } else if self.alias_sources.contains_key(&id) {
                    // The decl survives on other files' locations; drop this
                    // file's sites and re-select — an emptied map clears the
                    // effective pair, a surviving winner stays put.
                    if let Some(sources) = self.alias_sources.get_mut(&id) {
                        sources.retain(|site, _| site.file_id != file_id);
                    }
                    self.reselect_alias_source(&id);
                    if self
                        .alias_sources
                        .get(&id)
                        .is_some_and(|sources| sources.is_empty())
                    {
                        self.alias_sources.remove(&id);
                    }
                }
            }
        }

        if self.fallbacks.remove(&file_id).is_some() {
            derivation::note_write(|| FactKey::TypeFallbacks(file_id));
        }
        if let Some(type_owners) = self.in_filed_type_owner.remove(&file_id) {
            for type_owner in type_owners {
                derivation::note_write(|| FactKey::TypeCache(type_owner.clone()));
                if let LuaTypeOwner::Decl(decl_id) = &type_owner {
                    if self.decl_write_claims.remove(decl_id).is_some() {
                        derivation::note_write(|| FactKey::DeclClaim(*decl_id));
                    }
                    self.unbind_decl(decl_id);
                }
                self.types.remove(&type_owner);
                self.fact_metadata.remove(&type_owner);
                self.drop_fact_refs(&type_owner);
                self.source_types.remove(&type_owner);
            }
        }

        self.cache_refs.remove_file(file_id);
    }

    /// Notes every fact [`LuaIndex::clear`] is about to drop.
    fn note_clear_writes(&self) {
        if !derivation::recording() {
            return;
        }
        derivation::note_write(|| FactKey::TypeDeclScan);
        for file_id in self
            .file_types
            .keys()
            .chain(self.file_namespace.keys())
            .chain(self.file_using_namespace.keys())
            .chain(self.declaration_sites.keys())
        {
            derivation::note_write(|| FactKey::TypeFile(*file_id));
        }
        for decl_id in self.full_name_type_map.keys() {
            note_decl_written(decl_id);
        }
        for (decl_id, supers) in &self.supers {
            for entry in supers {
                note_supers_written(decl_id, entry.file_id);
            }
        }
        for decl_id in self.generic_params.keys() {
            derivation::note_write(|| FactKey::TypeGenericParams(decl_id.clone()));
        }
        for decl_id in self.sub_types.keys() {
            derivation::note_write(|| FactKey::SubTypes(decl_id.clone()));
        }
        for (type_id, decls) in &self.type_bound_decls {
            for decl_id in decls {
                derivation::note_write(|| FactKey::BoundDecls(type_id.clone(), decl_id.file_id));
            }
        }
        for owner in self.types.keys() {
            derivation::note_write(|| FactKey::TypeCache(owner.clone()));
        }
        for file_id in self.fallbacks.keys() {
            derivation::note_write(|| FactKey::TypeFallbacks(*file_id));
        }
        for decl_id in self.decl_write_claims.keys() {
            derivation::note_write(|| FactKey::DeclClaim(*decl_id));
        }
        for definition in self.definition_facts.keys() {
            derivation::note_write(|| FactKey::DefinitionFact(*definition));
        }
    }
}

/// The files one provenance names: event node, source and support files,
/// plus the files the step types point at. Mirrors what `remap_provenance`
/// consults, so the reverse index built from it visits exactly the facts an
/// edit to `file_id` can move.
fn collect_provenance_files<'a>(
    provenance: impl Iterator<Item = &'a LuaInferenceStep>,
    files: &mut HashSet<FileId>,
) {
    for step in provenance {
        files.insert(step.event.node.file_id());
        files.insert(step.event.source.file_id);
        for support in step.support.iter() {
            files.insert(support.file_id());
        }
        if let Some(typ) = step.inferred_type.as_deref() {
            collect_type_files(typ, files);
        }
        if let Some(typ) = step.found_type.as_deref() {
            collect_type_files(typ, files);
        }
    }
}

/// The files one stored type points at through a positioned identity: the
/// arms `remap_identities_in_type` rewrites. Named references (`Ref`, `Def`)
/// and module links carry no position and never move under a remap.
/// The files whose positions `typ` names, exactly those
/// [`remap_identities_in_type`] consults.
pub(crate) fn type_referenced_files(typ: &LuaType) -> HashSet<FileId> {
    let mut files = HashSet::default();
    collect_type_files(typ, &mut files);
    files
}

fn collect_type_files(typ: &LuaType, files: &mut HashSet<FileId>) {
    typ.visit_type_identities(&mut |inner| match inner {
        LuaType::TableConst(range) => {
            files.insert(range.file_id);
        }
        LuaType::Instance(instance) => {
            files.insert(instance.get_range().file_id);
        }
        LuaType::Signature(signature_id) => {
            files.insert(signature_id.get_file_id());
        }
        _ => {}
    });
}

/// Re-points `key`'s reverse edges at `files`, dropping the stale ones.
fn replace_reverse_edges<K>(
    by_file: &mut CowShards<FileId, HashSet<K>>,
    forward: &mut CowShards<K, HashSet<FileId>>,
    key: K,
    files: HashSet<FileId>,
) where
    K: ShardKey + Clone,
{
    if let Some(previous) = forward.insert(key.clone(), files.clone()) {
        for file_id in previous.difference(&files) {
            if let Some(keys) = by_file.get_mut(file_id) {
                keys.remove(&key);
                if keys.is_empty() {
                    by_file.remove(file_id);
                }
            }
        }
    }
    for file_id in &files {
        by_file.entry(*file_id).or_default().insert(key.clone());
    }
    if files.is_empty() {
        forward.remove(&key);
    }
}

/// Drops `key`'s reverse edges entirely.
fn remove_reverse_edges<K>(
    by_file: &mut CowShards<FileId, HashSet<K>>,
    forward: &mut CowShards<K, HashSet<FileId>>,
    key: &K,
) where
    K: ShardKey + Clone,
{
    let Some(files) = forward.remove(key) else {
        return;
    };
    for file_id in files {
        if let Some(keys) = by_file.get_mut(&file_id) {
            keys.remove(key);
            if keys.is_empty() {
                by_file.remove(&file_id);
            }
        }
    }
}

/// An inference event with the type owner whose fact carried it, `None` for a
/// definition fact; duplicates of one event keep the first in owner order.
type OwnedEvent = (LuaInferenceDiagnosticEvent, Option<LuaTypeOwner>);

fn collect_fact_derived_state(
    owner_file_id: FileId,
    owner: Option<&LuaTypeOwner>,
    fact: &LuaTypeFact,
    events_by_file: &mut HashMap<FileId, Vec<OwnedEvent>>,
    support_dependents: &mut HashMap<LuaInferenceNodeId, HashSet<FileId>>,
) {
    for step in fact.provenance() {
        events_by_file
            .entry(step.event.source.file_id)
            .or_default()
            .push((
                LuaInferenceDiagnosticEvent {
                    event: step.event.clone(),
                    fact: fact.clone(),
                },
                owner.cloned(),
            ));
        for support in step.support.iter() {
            support_dependents
                .entry(support.clone())
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

// Whether the first parameter should not be treated as self
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
    use crate::db_index::cow_shards::TaskFork;
    use crate::db_index::traits::LuaIndex;

    fn class_ref(name: &str) -> LuaType {
        LuaType::Ref(LuaTypeDeclId::global(name))
    }

    fn derivation_in(file: u32) -> DerivationId {
        DerivationId::new(
            derivation::DerivationKind::GmodPre,
            FileId::new(file),
            glua_parser::LuaSyntaxId::new(
                glua_parser::LuaSyntaxKind::Chunk.into(),
                TextRange::default(),
            ),
        )
    }

    /// Declares class `name` in `file`, as that file's derivation.
    fn declare(index: &mut LuaTypeIndex, name: &str, file: u32) {
        let _recording = derivation::begin(derivation_in(file));
        index.add_type_decl(
            FileId::new(file),
            LuaTypeDecl::new(
                FileId::new(file),
                TextRange::default(),
                name.to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::None.into(),
                LuaTypeDeclId::global(name),
            ),
        );
        derivation::end();
    }

    /// The installed state a batch install has to reproduce, in a stable
    /// order; subclass lists are read as sets.
    fn installed_state(index: &LuaTypeIndex) -> Vec<String> {
        let mut state = index
            .sub_types
            .iter()
            .map(|(base, children)| {
                let mut children = children
                    .iter()
                    .map(|child| format!("{child:?}"))
                    .collect::<Vec<_>>();
                children.sort();
                format!("sub {base:?} {children:?}")
            })
            .collect::<Vec<_>>();
        state.extend(index.supers.iter().map(|entry| format!("supers {entry:?}")));
        state.extend(
            index
                .full_name_type_map
                .iter()
                .map(|entry| format!("decl {entry:?}")),
        );
        state.push(format!("scan {}", index.decl_scan_fingerprint));
        state.sort();
        state
    }

    #[test]
    fn installing_forks_field_by_field_matches_installing_them_one_by_one() {
        let mut base = LuaTypeIndex::new();
        derive_from_base(&mut base, "Child", 1);
        declare(&mut base, "Shared", 1);
        let frozen = base.task_fork();
        let mut dropping = frozen.task_fork();
        dropping.retract_contributions(&derivation_in(1));
        let mut forks = vec![dropping];
        for file in 2..5 {
            let mut fork = frozen.task_fork();
            derive_from_base(&mut fork, if file == 2 { "Child" } else { "Other" }, file);
            declare(&mut fork, "Fresh", file);
            forks.push(fork);
        }
        let mut one_by_one = base.clone();
        let mut one_by_one_conflicts = vec![Vec::new(); forks.len()];
        for (at, fork) in forks.iter().cloned().enumerate() {
            one_by_one.install_task_reporting(&frozen, fork, &mut one_by_one_conflicts[at]);
        }
        let mut by_field = base.clone();
        let mut by_field_conflicts = vec![Vec::new(); forks.len()];
        by_field.install_tasks_reporting(
            &frozen,
            forks.into_iter().enumerate().collect(),
            &mut by_field_conflicts,
        );

        assert_eq!(
            (installed_state(&by_field), by_field_conflicts),
            (installed_state(&one_by_one), one_by_one_conflicts)
        );
    }

    /// Adds `child : Base` declared in `file`, as that file's derivation.
    fn derive_from_base(index: &mut LuaTypeIndex, child: &str, file: u32) {
        let _recording = derivation::begin(derivation_in(file));
        index.add_super_type(
            LuaTypeDeclId::global(child),
            FileId::new(file),
            TextRange::default(),
            class_ref("Base"),
        );
        derivation::end();
    }

    fn install(
        index: &mut LuaTypeIndex,
        frozen: &LuaTypeIndex,
        fork: LuaTypeIndex,
    ) -> Vec<&'static str> {
        let mut conflicts = Vec::new();
        index.install_task_reporting(frozen, fork, &mut conflicts);
        conflicts
    }

    #[test]
    fn a_subclass_another_task_still_derives_stays_listed_after_the_install() {
        let mut index = LuaTypeIndex::new();
        derive_from_base(&mut index, "Child", 1);
        let frozen = index.task_fork();
        let mut dropping = frozen.task_fork();
        dropping.retract_contributions(&derivation_in(1));
        let mut adding = frozen.task_fork();
        derive_from_base(&mut adding, "Child", 2);

        let conflicts = [
            install(&mut index, &frozen, dropping),
            install(&mut index, &frozen, adding),
        ]
        .concat();
        assert_eq!(
            (
                conflicts,
                index.sub_types.get(&LuaTypeDeclId::global("Base"))
            ),
            (Vec::new(), Some(&vec![LuaTypeDeclId::global("Child")]))
        );
    }

    #[test]
    fn a_subclass_no_task_derives_any_more_is_dropped_after_the_install() {
        let mut index = LuaTypeIndex::new();
        derive_from_base(&mut index, "Child", 1);
        let frozen = index.task_fork();
        let mut dropping = frozen.task_fork();
        dropping.retract_contributions(&derivation_in(1));
        let mut other = frozen.task_fork();
        derive_from_base(&mut other, "Other", 2);

        let conflicts = [
            install(&mut index, &frozen, other),
            install(&mut index, &frozen, dropping),
        ]
        .concat();
        assert_eq!(
            (
                conflicts,
                index.sub_types.get(&LuaTypeDeclId::global("Base"))
            ),
            (Vec::new(), Some(&vec![LuaTypeDeclId::global("Other")]))
        );
    }

    #[test]
    fn a_reference_owner_follows_the_merged_counts() {
        let signature = LuaSignatureId::new(FileId::new(9), 0.into());
        let typ = LuaType::Signature(signature);
        let type_ref = TypeCacheRef::Signature(signature);
        let owner_file = FileId::new(1);
        let mut refs = TypeCacheRefIndex::default();
        refs.add(owner_file, &typ);
        let frozen = refs.task_fork();
        let mut dropping = frozen.task_fork();
        dropping.remove(owner_file, &typ);
        let mut adding = frozen.task_fork();
        adding.add(owner_file, &typ);

        assert!(!refs.install_task(&frozen, dropping));
        assert!(!refs.install_task(&frozen, adding));
        assert_eq!(
            (
                refs.owner_refs
                    .get(&owner_file)
                    .and_then(|counts| counts.get(&type_ref)),
                refs.owners(&type_ref)
                    .is_some_and(|owners| owners.contains(&owner_file)),
                refs.refs_into_file(FileId::new(9))
                    .any(|held| *held == type_ref),
            ),
            (Some(&1), true, true)
        );
    }

    #[test]
    fn a_reference_no_owner_holds_any_more_is_dropped() {
        let signature = LuaSignatureId::new(FileId::new(9), 0.into());
        let typ = LuaType::Signature(signature);
        let type_ref = TypeCacheRef::Signature(signature);
        let mut refs = TypeCacheRefIndex::default();
        refs.add(FileId::new(1), &typ);
        refs.add(FileId::new(2), &typ);
        let frozen = refs.task_fork();
        let mut first = frozen.task_fork();
        first.remove(FileId::new(1), &typ);
        let mut second = frozen.task_fork();
        second.remove(FileId::new(2), &typ);

        assert!(!refs.install_task(&frozen, first));
        assert!(!refs.install_task(&frozen, second));
        assert_eq!(
            (
                refs.owners(&type_ref),
                refs.refs_into_file(FileId::new(9)).count()
            ),
            (None, 0)
        );
    }

    /// A superseding write replaces the type, so the metadata derived from the
    /// discarded type must not survive to be reported against the new one.
    #[test]
    fn type_fact_reads_survive_absence_value_changes_and_removal() {
        let mut index = LuaTypeIndex::new();
        let owner = LuaTypeOwner::Decl(LuaDeclId::new(FileId::new(1), 0.into()));
        let id = DerivationId::workspace(derivation::DerivationKind::GmodLoad);
        derivation::lend_keys(derivation::KeyTable::default());
        let _recording = derivation::begin(id);
        assert!(index.get_type_cache(&owner).is_none());
        let absent = derivation::end().into_recorded().1;
        index.force_bind_type(owner.clone(), LuaTypeCache::InferType(LuaType::String));
        let _recording = derivation::begin(id);
        assert_eq!(
            index.get_type_cache(&owner).map(LuaTypeCache::as_type),
            Some(&LuaType::String)
        );
        let present = derivation::end().into_recorded().1;
        index.force_bind_type(owner.clone(), LuaTypeCache::InferType(LuaType::Number));
        let _recording = derivation::begin(id);
        assert_eq!(
            index.get_type_cache(&owner).map(LuaTypeCache::as_type),
            Some(&LuaType::Number)
        );
        let changed = derivation::end().into_recorded().1;
        index.remove_files(&[FileId::new(1)]);
        let _recording = derivation::begin(id);
        assert!(index.get_type_cache(&owner).is_none());
        let removed = derivation::end().into_recorded().1;
        derivation::take_keys();
        assert_eq!(absent, present);
        assert_eq!(present, changed);
        assert_eq!(changed, removed);
        assert_eq!(absent.len(), 1);
    }

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

    /// The reverse sub-type index answers from the writes, not from a scan,
    /// and file removal prunes its edges along with the forward ones.
    #[test]
    fn sub_type_index_answers_writes_and_prunes_removed_edges() {
        let mut index = LuaTypeIndex::new();
        let parent_id = LuaTypeDeclId::global("Parent");
        let parent_type = LuaType::Ref(parent_id.clone());
        let (top_file, mid_file) = (FileId::new(1), FileId::new(2));

        for name in ["Zebra", "Apple", "Mango"] {
            index.add_type_decl(
                top_file,
                LuaTypeDecl::new(
                    top_file,
                    TextRange::new(0.into(), 1.into()),
                    name.to_owned(),
                    LuaDeclTypeKind::Class,
                    LuaTypeFlag::None.into(),
                    LuaTypeDeclId::global(name),
                ),
            );
        }
        // Insert out of name order: the answer still comes back sorted.
        for (file_id, name, start) in [
            (mid_file, "Zebra", 10),
            (top_file, "Apple", 20),
            (mid_file, "Mango", 30),
        ] {
            index.add_super_type(
                LuaTypeDeclId::global(name),
                file_id,
                TextRange::new(start.into(), (start + 5).into()),
                parent_type.clone(),
            );
        }
        // A non-`Ref` super edge is not an inheritance edge and stays out.
        index.add_super_type(
            LuaTypeDeclId::global("Apple"),
            top_file,
            TextRange::new(40.into(), 45.into()),
            LuaType::String,
        );

        let names = index
            .get_sub_types(&parent_id)
            .iter()
            .map(|decl| decl.get_name().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Apple", "Mango", "Zebra"]);
        assert_eq!(index.get_all_sub_types(&parent_id).len(), 3);

        // Removing the edge-only file prunes exactly its edges.
        index.remove_files(&[mid_file]);
        let names = index
            .get_sub_types(&parent_id)
            .iter()
            .map(|decl| decl.get_name().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Apple"]);
        assert_eq!(index.get_all_sub_types(&parent_id).len(), 1);
    }

    /// Runs `read` as a derivation and returns what it gave with the one fact
    /// it recorded a read of.
    fn read_by_a_derivation<T>(read: impl FnOnce() -> T) -> (T, FactKey) {
        let _recording = derivation::begin(DerivationId::workspace(
            derivation::DerivationKind::GmodLoad,
        ));
        let value = read();
        let reads = derivation::end().into_recorded().2;
        let [key] = reads.as_slice() else {
            panic!("expected one read, got {reads:?}");
        };
        (value, key.key().clone())
    }

    /// Adds `child : base` to `file`'s derivation, the edge named at `start`.
    fn derive_super_at(index: &mut LuaTypeIndex, child: &str, file: u32, start: u32, base: &str) {
        let _recording = derivation::begin(derivation_in(file));
        index.add_super_type(
            LuaTypeDeclId::global(child),
            FileId::new(file),
            TextRange::new(start.into(), (start + 5).into()),
            class_ref(base),
        );
        derivation::end();
    }

    /// Declares class `name` in `file`'s derivation, the declaration named at
    /// `start`.
    fn declare_at(index: &mut LuaTypeIndex, name: &str, file: u32, start: u32) {
        let _recording = derivation::begin(derivation_in(file));
        index.add_type_decl(
            FileId::new(file),
            LuaTypeDecl::new(
                FileId::new(file),
                TextRange::new(start.into(), (start + 5).into()),
                name.to_string(),
                LuaDeclTypeKind::Class,
                LuaTypeFlag::None.into(),
                LuaTypeDeclId::global(name),
            ),
        );
        derivation::end();
    }

    /// A reader of a type's supertypes records them alone, read out of its
    /// super edges: an edge from another file to a type already derived holds
    /// it, and a new supertype moves it.
    #[test]
    fn a_repeated_super_edge_holds_a_reader_of_the_supertypes() {
        let mut index = LuaTypeIndex::new();
        let child = LuaTypeDeclId::global("Child");
        derive_super_at(&mut index, "Child", 1, 10, "Base");

        let (supers, read) = read_by_a_derivation(|| index.get_super_types(&child));

        assert_eq!(supers, Some(vec![class_ref("Base")]));
        let edges = FactKey::TypeSupers(child);
        assert!(edges.is_base());
        assert_eq!(read.base(), Some(edges.clone()));
        let (edges_seen, read_seen) = (
            index.fact_fingerprint(&edges),
            index.fact_fingerprint(&read),
        );

        derive_super_at(&mut index, "Child", 2, 30, "Base");
        assert_ne!(index.fact_fingerprint(&edges), edges_seen);
        assert_eq!(index.fact_fingerprint(&read), read_seen);

        derive_super_at(&mut index, "Child", 2, 40, "Other");
        assert_ne!(index.fact_fingerprint(&read), read_seen);
    }

    /// An edge that moves within its file changes where its supertype is
    /// named, not which supertypes the type has.
    #[test]
    fn a_super_edge_moving_in_its_file_holds_a_reader_of_the_supertypes() {
        let mut index = LuaTypeIndex::new();
        let child = LuaTypeDeclId::global("Child");
        derive_super_at(&mut index, "Child", 1, 10, "Base");
        let (_, read) = read_by_a_derivation(|| index.get_super_types(&child));
        let edges = FactKey::TypeSupers(child);
        let (edges_seen, read_seen) = (
            index.fact_fingerprint(&edges),
            index.fact_fingerprint(&read),
        );

        index.retract_contributions(&derivation_in(1));
        derive_super_at(&mut index, "Child", 1, 50, "Base");

        assert_ne!(index.fact_fingerprint(&edges), edges_seen);
        assert_eq!(index.fact_fingerprint(&read), read_seen);
    }

    /// A reader of a type's super edges records the file and type of each,
    /// read out of its super edges: an edge moving in its file holds it, and
    /// an edge from another file moves it.
    #[test]
    fn a_super_edge_moving_in_its_file_holds_a_reader_of_the_edges() {
        let mut index = LuaTypeIndex::new();
        let child = LuaTypeDeclId::global("Child");
        derive_super_at(&mut index, "Child", 1, 10, "Base");

        let (edges_read, read) =
            read_by_a_derivation(|| index.get_super_type_entries(&child).map(<[_]>::len));

        assert_eq!(edges_read, Some(1));
        let edges = FactKey::TypeSupers(child);
        assert_eq!(read.base(), Some(edges.clone()));
        let (edges_seen, read_seen) = (
            index.fact_fingerprint(&edges),
            index.fact_fingerprint(&read),
        );

        index.retract_contributions(&derivation_in(1));
        derive_super_at(&mut index, "Child", 1, 50, "Base");
        assert_ne!(index.fact_fingerprint(&edges), edges_seen);
        assert_eq!(index.fact_fingerprint(&read), read_seen);

        derive_super_at(&mut index, "Child", 2, 30, "Base");
        assert_ne!(index.fact_fingerprint(&read), read_seen);
    }

    /// A reader of the files a type is declared in records them alone, read
    /// out of where it is declared: the declaration moving in its file holds
    /// it, and another file declaring the type or a location's flags changing
    /// moves it.
    #[test]
    fn a_declaration_moving_in_its_file_holds_a_reader_of_the_declaring_files() {
        let mut index = LuaTypeIndex::new();
        let moved = LuaTypeDeclId::global("Moved");
        declare_at(&mut index, "Moved", 1, 0);

        let (flags, read) = read_by_a_derivation(|| {
            index
                .full_name_type_map
                .get(&moved)
                .map(|decl| decl.get_location_flags().count())
        });

        assert_eq!(flags, Some(1));
        let locations = FactKey::TypeDeclLocations(moved.clone());
        assert!(locations.is_base());
        assert_eq!(read.base(), Some(locations.clone()));
        let (locations_seen, read_seen) = (
            index.fact_fingerprint(&locations),
            index.fact_fingerprint(&read),
        );

        index.retract_contributions(&derivation_in(1));
        declare_at(&mut index, "Moved", 1, 20);
        assert_ne!(index.fact_fingerprint(&locations), locations_seen);
        assert_eq!(index.fact_fingerprint(&read), read_seen);

        let _recording = derivation::begin(derivation_in(2));
        index.add_type_decl_location(
            FileId::new(2),
            &moved,
            LuaDeclLocation {
                file_id: FileId::new(2),
                range: TextRange::new(0.into(), 5.into()),
                flag: LuaTypeFlag::Partial.into(),
            },
        );
        derivation::end();
        assert_ne!(index.fact_fingerprint(&read), read_seen);
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
        assert_eq!(left.supers_by_file, right.supers_by_file);
        assert_eq!(left.sub_types, right.sub_types);
        assert_eq!(left.decl_type_binds, right.decl_type_binds);
        assert_eq!(left.type_bound_decls, right.type_bound_decls);
        assert_eq!(type_cache_types(left), type_cache_types(right));
        assert_eq!(left.in_filed_type_owner, right.in_filed_type_owner);
        assert_eq!(left.fact_metadata, right.fact_metadata);
        assert_eq!(left.definition_facts, right.definition_facts);
        assert_eq!(
            left.inference_events_by_file,
            right.inference_events_by_file
        );
        assert_eq!(left.support_dependents, right.support_dependents);
        assert_eq!(left.support_file_dependents, right.support_file_dependents);
        assert_eq!(left.cache_refs, right.cache_refs);
        assert_eq!(left.fact_ref_files, right.fact_ref_files);
        assert_eq!(left.fact_refs_by_file, right.fact_refs_by_file);
        assert_eq!(
            left.definition_fact_ref_files,
            right.definition_fact_ref_files
        );
        assert_eq!(
            left.definition_fact_refs_by_file,
            right.definition_fact_refs_by_file
        );
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

    /// The reverse lookup maps stay consistent with the primary maps: every
    /// forward entry recomputes to the same file set, and every reverse bucket
    /// is exactly the forward entries naming it.
    fn assert_reverse_maps_consistent(index: &LuaTypeIndex) {
        for (owner, metadata) in &index.fact_metadata {
            let expected = LuaTypeIndex::metadata_referenced_files(metadata);
            assert_eq!(
                index.fact_ref_files.get(owner).cloned().unwrap_or_default(),
                expected,
                "fact refs for {owner:?}"
            );
        }
        let mut expected_by_file: HashMap<FileId, HashSet<LuaTypeOwner>> = HashMap::default();
        for (owner, files) in &index.fact_ref_files {
            for file_id in files {
                expected_by_file
                    .entry(*file_id)
                    .or_default()
                    .insert(owner.clone());
            }
        }
        assert_eq!(index.fact_refs_by_file, CowShards::from(expected_by_file));

        for (definition, fact) in &index.definition_facts {
            let expected = type_fact_referenced_files(fact);
            assert_eq!(
                index
                    .definition_fact_ref_files
                    .get(definition)
                    .cloned()
                    .unwrap_or_default(),
                expected,
                "definition fact refs for {definition:?}"
            );
        }
        let mut expected_definitions: HashMap<FileId, HashSet<LuaDefinitionId>> =
            HashMap::default();
        for (definition, files) in &index.definition_fact_ref_files {
            for file_id in files {
                expected_definitions
                    .entry(*file_id)
                    .or_default()
                    .insert(*definition);
            }
        }
        assert_eq!(
            index.definition_fact_refs_by_file,
            CowShards::from(expected_definitions)
        );

        let mut expected_edges: HashMap<FileId, Vec<(LuaTypeDeclId, InFiled<LuaSuperType>)>> =
            HashMap::default();
        for (child_id, supers) in &index.supers {
            for entry in supers {
                expected_edges
                    .entry(entry.file_id)
                    .or_default()
                    .push((child_id.clone(), entry.clone()));
            }
        }
        for edges in expected_edges.values_mut() {
            edges.sort_by(|left, right| {
                left.0.stable_cmp(&right.0).then_with(|| {
                    super_entry_sort_key(&left.1).cmp(&super_entry_sort_key(&right.1))
                })
            });
        }
        let mut actual_edges = index.supers_by_file.clone();
        for edges in actual_edges.values_mut() {
            edges.sort_by(|left, right| {
                left.0.stable_cmp(&right.0).then_with(|| {
                    super_entry_sort_key(&left.1).cmp(&super_entry_sort_key(&right.1))
                })
            });
        }
        assert_eq!(*actual_edges, expected_edges);
    }

    /// Removing a file leaves exactly the state a fresh index built from the
    /// surviving files' inputs holds. The surviving facts keep provenance
    /// naming the removed file (only re-analysis rewrites that), so the fresh
    /// index is built with the same dangling references.
    #[test]
    fn removal_matches_fresh_index_for_surviving_derived_state() {
        let first = file_id(1);
        let second = file_id(2);

        fn build(files: &[FileId]) -> LuaTypeIndex {
            let source_file = file_id(4);
            let mut index = LuaTypeIndex::new();
            for target in files {
                let name = format!("Class{}", target.id);
                index.add_type_decl(
                    *target,
                    LuaTypeDecl::new(
                        *target,
                        TextRange::new(0.into(), 1.into()),
                        name.clone(),
                        LuaDeclTypeKind::Class,
                        LuaTypeFlag::None.into(),
                        LuaTypeDeclId::global(name.as_str()),
                    ),
                );
                let fact_owner = owner(*target, 10);
                index.force_bind_type_fact(
                    fact_owner.clone(),
                    LuaTypeCache::InferType(LuaType::String),
                    metadata(fact_owner, source_file, file_id(1)),
                );
                index.bind_definition_fact(
                    LuaDefinitionId::Declaration(LuaDeclId::new(*target, 40.into())),
                    LuaTypeFact::certain(LuaType::Number),
                );
            }
            index.flush_inference_derived_state();
            index
        }

        let mut removed = build(&[first, second, file_id(3)]);
        assert_reverse_maps_consistent(&removed);
        removed.remove_files(&[second, first]);
        assert_reverse_maps_consistent(&removed);

        let fresh = build(&[file_id(3)]);

        // The revision counts operations rather than state, and the write
        // counters depend on how many owners were ever bound: both differ by
        // construction and are excluded.
        assert_eq!(removed.file_namespace, fresh.file_namespace);
        assert_eq!(removed.file_using_namespace, fresh.file_using_namespace);
        assert_eq!(removed.file_types, fresh.file_types);
        assert_eq!(removed.full_name_type_map, fresh.full_name_type_map);
        assert_eq!(removed.generic_params, fresh.generic_params);
        assert_eq!(removed.supers, fresh.supers);
        assert_eq!(removed.supers_by_file, fresh.supers_by_file);
        assert_eq!(removed.sub_types, fresh.sub_types);
        assert_eq!(type_cache_types(&removed), type_cache_types(&fresh));
        assert_eq!(removed.in_filed_type_owner, fresh.in_filed_type_owner);
        assert_eq!(removed.fact_metadata, fresh.fact_metadata);
        assert_eq!(removed.fact_ref_files, fresh.fact_ref_files);
        assert_eq!(removed.fact_refs_by_file, fresh.fact_refs_by_file);
        assert_eq!(removed.definition_facts, fresh.definition_facts);
        assert_eq!(
            removed.definition_fact_ref_files,
            fresh.definition_fact_ref_files
        );
        assert_eq!(
            removed.definition_fact_refs_by_file,
            fresh.definition_fact_refs_by_file
        );
        assert_eq!(
            removed.inference_events_by_file,
            fresh.inference_events_by_file
        );
        assert_eq!(removed.support_dependents, fresh.support_dependents);
        assert_eq!(
            removed.support_file_dependents,
            fresh.support_file_dependents
        );
        assert_eq!(removed.cache_refs, fresh.cache_refs);
    }

    /// Remapping one file visits only the facts that name it: an unrelated
    /// file's facts and payloads come out byte-identical.
    #[test]
    fn remap_visits_only_facts_naming_the_edited_file() {
        use crate::db_index::edit::{FileRemap, PositionMap};

        let edited = file_id(1);
        let unrelated = file_id(2);
        let mut index = LuaTypeIndex::new();
        let edited_owner = owner(edited, 10);
        let unrelated_owner = owner(unrelated, 10);
        index.force_bind_type_fact(
            edited_owner.clone(),
            LuaTypeCache::InferType(LuaType::String),
            metadata(edited_owner.clone(), edited, edited),
        );
        // No reference to the edited file anywhere: empty provenance and a
        // payload-free cached type.
        index.force_bind_type_fact(
            unrelated_owner.clone(),
            LuaTypeCache::InferType(LuaType::String),
            LuaTypeFactMetadata {
                confidence: LuaInferenceConfidence::Certain,
                base_provenance_kind: None,
                provenance: Arc::from([]),
            },
        );
        index.bind_definition_fact(
            LuaDefinitionId::Declaration(LuaDeclId::new(unrelated, 40.into())),
            LuaTypeFact::certain(LuaType::Number),
        );
        assert_reverse_maps_consistent(&index);
        assert!(
            index
                .fact_refs_by_file
                .get(&edited)
                .is_none_or(|owners| !owners.contains(&unrelated_owner))
        );

        let before_metadata = index.fact_metadata.clone();
        let before_definitions = index.definition_facts.clone();

        // An insertion at offset 3 shifts everything after it by two.
        let remap = FileRemap::unvalidated(edited, PositionMap::new("abcdef", "abcXXdef"));
        index.remap_file_identities(&remap, &HashSet::default());
        assert_reverse_maps_consistent(&index);

        assert_eq!(
            index.fact_metadata.get(&unrelated_owner),
            before_metadata.get(&unrelated_owner)
        );
        assert_eq!(index.definition_facts, before_definitions);
    }
}

#[cfg(test)]
mod remap_walker_tests {
    use rowan::{TextRange, TextSize};

    use super::*;
    use crate::db_index::edit::{FileRemap, PositionMap};

    fn file_id(id: u32) -> FileId {
        FileId::new(id)
    }

    fn table(file: u32, start: u32, end: u32) -> LuaType {
        LuaType::TableConst(InFiled::new(
            file_id(file),
            TextRange::new(TextSize::new(start), TextSize::new(end)),
        ))
    }

    fn signature(file: u32, position: u32) -> LuaType {
        LuaType::Signature(LuaSignatureId::new(file_id(file), TextSize::new(position)))
    }

    #[test]
    fn definition_fact_remaps_instance_base_after_edit_and_reports_deletion() {
        let source = file_id(1);
        let consumer = file_id(2);
        let definition = LuaDefinitionId::Declaration(LuaDeclId::new(consumer, 0.into()));
        let instance = |start, end| {
            LuaType::Instance(Arc::new(LuaInstanceType::new(
                table(1, start, end),
                InFiled::new(consumer, TextRange::new(0.into(), 2.into())),
            )))
        };
        let mut index = LuaTypeIndex::new();
        index.bind_definition_fact(definition, LuaTypeFact::certain(instance(3, 5)));
        let insertion = FileRemap::unvalidated(source, PositionMap::new("abcde", "XXabcde"));
        assert!(
            index
                .remap_file_identities(&insertion, &HashSet::default())
                .is_empty()
        );
        assert_eq!(
            index.get_definition_fact(&definition).unwrap().typ(),
            &instance(5, 7),
        );

        let deletion = FileRemap::unvalidated(source, PositionMap::new("XXabcde", "XXa"));
        assert_eq!(
            index.remap_file_identities(&deletion, &HashSet::default()),
            [crate::LostFact::new(
                consumer,
                FactKey::DefinitionFact(definition)
            )],
        );
    }

    #[test]
    fn moved_table_and_signature_nested_in_union_are_rewritten() {
        let edited = file_id(1);
        // Insertion at offset 3 shifts everything after it by two.
        let remap = FileRemap::unvalidated(edited, PositionMap::new("abcdef", "abcXXdef"));
        let composite = LuaType::from_vec(vec![table(1, 4, 6), signature(1, 5), LuaType::String]);

        let result = remap_identities_in_type(&composite, &remap);
        assert!(!result.lost, "a shift must not report loss");
        let rewritten = result.typ.expect("a shift must rewrite the composite");
        let expected = LuaType::from_vec(vec![table(1, 6, 8), signature(1, 7), LuaType::String]);
        assert_eq!(rewritten, expected);
    }

    #[test]
    fn lost_table_and_signature_nested_in_union_are_reported_without_rewrite() {
        let edited = file_id(1);
        // Deletion of offsets 3..5 destroys what sits inside it.
        let remap = FileRemap::unvalidated(edited, PositionMap::new("abcXXdef", "abcdef"));
        let composite = LuaType::from_vec(vec![table(1, 3, 4), signature(1, 4)]);

        let result = remap_identities_in_type(&composite, &remap);
        assert!(result.lost, "a destroyed identity must report loss");
        assert!(
            result.typ.is_none(),
            "a lost identity must not be rewritten to a guessed value"
        );
    }

    #[test]
    fn unrelated_identities_are_unchanged() {
        let edited = file_id(1);
        let remap = FileRemap::unvalidated(edited, PositionMap::new("abcdef", "abcXXdef"));
        let composite = LuaType::from_vec(vec![table(2, 4, 6), signature(2, 5), LuaType::String]);

        let result = remap_identities_in_type(&composite, &remap);
        assert!(!result.lost, "another file's identities are never lost");
        assert!(
            result.typ.is_none(),
            "another file's identities must remain unchanged"
        );
    }
}

crate::db_index::cow_shards::task_fork_fields!(LuaTypeIndex {
    file_namespace,
    file_using_namespace,
    file_types,
    declaration_sites,
    full_name_type_map => LuaTypeDecl::merge,
    generic_params => |current, frozen, fork| {
        crate::db_index::cow_shards::merge_map(current, frozen, fork, crate::db_index::cow_shards::refuse)
    },
    alias_sources => |current, frozen, fork| {
        crate::db_index::cow_shards::merge_map(current, frozen, fork, crate::db_index::cow_shards::refuse)
    },
    extra_sources,
    supers => merge_super_entries,
    supers_by_file,
    sub_types => crate::db_index::cow_shards::rebuilt,
    decl_type_binds,
    type_bound_decls => crate::db_index::cow_shards::merge_set,
    table_literal_decls,
    types,
    cache_refs,
    in_filed_type_owner,
    fact_metadata,
    fact_ref_files,
    fact_refs_by_file => crate::db_index::cow_shards::merge_set,
    definition_fact_ref_files,
    definition_fact_refs_by_file => crate::db_index::cow_shards::merge_set,
    decl_write_claims,
    definition_facts,
    inference_events_by_file,
    support_dependents,
    support_file_dependents,
    derived_state_dirty,
    decl_scan_fingerprint => crate::db_index::cow_shards::rebuilt,
    source_types,
    source_decl_locations,
    derived_contributions,
    location_holders,
    super_holders,
    table_classes,
    fallbacks,
} prepare derived_to_rebuild then rebuild_derived
  by_field install_decl_scan => rebuild_derived);

crate::db_index::cow_shards::task_fork_fields!(TypeCacheRefIndex {
    owner_refs => |current, frozen, fork| {
        crate::db_index::cow_shards::merge_map(current, frozen, fork, crate::db_index::cow_shards::merge_sum)
    },
    ref_owners => crate::db_index::cow_shards::rebuilt,
    refs_by_file => crate::db_index::cow_shards::rebuilt,
} prepare refs_to_rebuild then rebuild_refs);
