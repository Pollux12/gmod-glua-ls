//! A call's instantiated result keeps at most [`MAX_DEPTH`] levels of the
//! structure it builds by value. A value built from itself, such as a registry
//! whose entries derive from other entries, would otherwise nest one level
//! deeper on every pass and never settle. Named types, table literals and
//! instances are references and keep their depth wherever they sit.

use std::sync::Arc;

use crate::{
    LuaArrayType, LuaFunctionType, LuaGenericType, LuaIntersectionType, LuaMergedTableType,
    LuaMultiLineUnion, LuaObjectType, LuaTupleType, LuaType, VariadicType,
};

/// The levels of by-value structure a call's result keeps.
pub(super) const MAX_DEPTH: usize = 6;

/// `typ` with every by-value structure nested deeper than [`MAX_DEPTH`] read as
/// its plain kind: `table`, `function` or `boolean`.
pub(super) fn bounded(typ: &LuaType) -> LuaType {
    cut(typ, MAX_DEPTH).unwrap_or_else(|| typ.clone())
}

/// `typ` cut to `depth` levels, or `None` when it is already within them.
fn cut(typ: &LuaType, depth: usize) -> Option<LuaType> {
    match typ {
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
        | LuaType::TableConst(_)
        | LuaType::Ref(_)
        | LuaType::Def(_)
        | LuaType::TplRef(_)
        | LuaType::StrTplRef(_)
        | LuaType::ConstTplRef(_)
        | LuaType::Signature(_)
        | LuaType::Instance(_)
        | LuaType::DocStringConst(_)
        | LuaType::DocIntegerConst(_)
        | LuaType::DocBooleanConst(_)
        | LuaType::Namespace(_)
        | LuaType::Call(_)
        | LuaType::Language(_)
        | LuaType::ModuleRef(_)
        | LuaType::DocAttribute(_)
        | LuaType::Conditional(_)
        | LuaType::ConditionalInfer(_)
        | LuaType::Mapped(_) => None,
        // Alternatives and return lists add no level: neither nests in itself.
        LuaType::Union(union) => cut_all(union.types(), depth).map(LuaType::from_vec),
        LuaType::MultiLineUnion(union) => {
            cut_all(union.get_unions().iter().map(|(member, _)| member), depth).map(|members| {
                LuaType::MultiLineUnion(
                    LuaMultiLineUnion::new(
                        members
                            .into_iter()
                            .zip(union.get_unions())
                            .map(|(member, (_, description))| (member, description.clone()))
                            .collect(),
                    )
                    .into(),
                )
            })
        }
        LuaType::Variadic(variadic) => match variadic.as_ref() {
            VariadicType::Multi(types) => cut_all(types.iter(), depth)
                .map(|types| LuaType::Variadic(Arc::new(VariadicType::Multi(types)))),
            VariadicType::Base(base) => {
                cut(base, depth).map(|base| LuaType::Variadic(Arc::new(VariadicType::Base(base))))
            }
        },
        LuaType::DocFunction(_) if depth == 0 => Some(LuaType::Function),
        LuaType::TypeGuard(_) if depth == 0 => Some(LuaType::Boolean),
        LuaType::Array(_)
        | LuaType::Tuple(_)
        | LuaType::Object(_)
        | LuaType::Intersection(_)
        | LuaType::MergedTable(_)
        | LuaType::Generic(_)
        | LuaType::TableGeneric(_)
        | LuaType::TableOf(_)
            if depth == 0 =>
        {
            Some(LuaType::Table)
        }
        LuaType::DocFunction(function) => {
            let declared = function
                .get_params()
                .iter()
                .filter_map(|(_, typ)| typ.as_ref());
            let cut_params = cut_all(declared, depth - 1);
            let cut_ret = cut(function.get_ret(), depth - 1);
            if cut_params.is_none() && cut_ret.is_none() {
                return None;
            }
            let params = match cut_params {
                Some(cut_params) => {
                    let mut cut_params = cut_params.into_iter();
                    function
                        .get_params()
                        .iter()
                        .map(|(name, typ)| {
                            (name.clone(), typ.as_ref().and_then(|_| cut_params.next()))
                        })
                        .collect()
                }
                None => function.get_params().to_vec(),
            };
            Some(LuaType::DocFunction(
                LuaFunctionType::new(
                    function.get_async_state(),
                    function.is_colon_define(),
                    function.is_variadic(),
                    params,
                    cut_ret.unwrap_or_else(|| function.get_ret().clone()),
                )
                .with_optional_params(function.get_optional_params().to_vec())
                .with_call_arg_roles(function.get_call_arg_roles().to_vec())
                .into(),
            ))
        }
        LuaType::Array(array) => cut(array.get_base(), depth - 1)
            .map(|base| LuaType::Array(LuaArrayType::new(base, array.get_len().clone()).into())),
        LuaType::Tuple(tuple) => cut_all(tuple.get_types().iter(), depth - 1)
            .map(|types| LuaType::Tuple(LuaTupleType::new(types, tuple.status).into())),
        LuaType::Object(object) => {
            let cut_fields = cut_all(object.get_fields().values(), depth - 1);
            let entries = object
                .get_index_access()
                .iter()
                .flat_map(|(key, value)| [key, value]);
            let cut_entries = cut_all(entries, depth - 1);
            if cut_fields.is_none() && cut_entries.is_none() {
                return None;
            }
            let fields = match cut_fields {
                Some(fields) => object.get_fields().keys().cloned().zip(fields).collect(),
                None => object.get_fields().clone(),
            };
            let index_access = match cut_entries {
                Some(entries) => entries
                    .chunks(2)
                    .map(|pair| (pair[0].clone(), pair[1].clone()))
                    .collect(),
                None => object.get_index_access().to_vec(),
            };
            Some(LuaType::Object(
                LuaObjectType::new_with_fields(fields, index_access).into(),
            ))
        }
        LuaType::Intersection(intersection) => cut_all(intersection.get_types().iter(), depth - 1)
            .map(|types| LuaType::Intersection(LuaIntersectionType::new(types).into())),
        LuaType::MergedTable(merged) => cut_all(merged.get_types().iter(), depth - 1)
            .map(|types| LuaType::MergedTable(LuaMergedTableType::new(types).into())),
        LuaType::Generic(generic) => {
            cut_all(generic.get_params().iter(), depth - 1).map(|params| {
                LuaType::Generic(LuaGenericType::new(generic.get_base_type_id(), params).into())
            })
        }
        LuaType::TableGeneric(params) => {
            cut_all(params.iter(), depth - 1).map(|params| LuaType::TableGeneric(params.into()))
        }
        LuaType::TableOf(inner) => {
            cut(inner, depth - 1).map(|inner| LuaType::TableOf(Box::new(inner)))
        }
        LuaType::TypeGuard(guarded) => {
            cut(guarded, depth - 1).map(|guarded| LuaType::TypeGuard(guarded.into()))
        }
    }
}

/// `types` each cut to `depth` levels, or `None` when every one is within
/// them; nothing is copied until the first that is not.
fn cut_all<'a>(
    types: impl Iterator<Item = &'a LuaType> + Clone,
    depth: usize,
) -> Option<Vec<LuaType>> {
    let (first, first_cut) = types
        .clone()
        .enumerate()
        .find_map(|(index, typ)| cut(typ, depth).map(|cut| (index, cut)))?;
    let mut first_cut = Some(first_cut);
    Some(
        types
            .enumerate()
            .map(|(index, typ)| match index {
                _ if index < first => typ.clone(),
                _ if index == first => first_cut.take().unwrap_or_else(|| typ.clone()),
                _ => cut(typ, depth).unwrap_or_else(|| typ.clone()),
            })
            .collect(),
    )
}
