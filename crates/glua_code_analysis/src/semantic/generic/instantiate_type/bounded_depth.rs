//! A call's instantiated result keeps at most [`MAX_DEPTH`] levels of the
//! structure it builds by value. A value built from itself, such as a registry
//! whose entries derive from other entries, would otherwise nest one level
//! deeper on every pass and never settle. Named types, table literals and
//! instances are references and keep their depth wherever they sit.

use std::sync::Arc;

use crate::{
    LuaArrayType, LuaFunctionType, LuaGenericType, LuaIntersectionType, LuaMergedTableType,
    LuaObjectType, LuaTupleType, LuaType, VariadicType,
};

/// The levels of by-value structure a call's result keeps.
pub(super) const MAX_DEPTH: usize = 6;

/// `typ` with every by-value structure nested deeper than [`MAX_DEPTH`] read as
/// its plain kind: `table` or `function`.
pub(super) fn bounded(typ: &LuaType) -> LuaType {
    bounded_to(typ, MAX_DEPTH)
}

fn bounded_to(typ: &LuaType, depth: usize) -> LuaType {
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
        | LuaType::Mapped(_) => typ.clone(),
        // Alternatives and return lists add no level: neither nests in itself.
        LuaType::Union(union) => LuaType::from_vec(
            union
                .into_vec()
                .iter()
                .map(|member| bounded_to(member, depth))
                .collect(),
        ),
        LuaType::MultiLineUnion(union) => bounded_to(&union.to_union(), depth),
        LuaType::Variadic(variadic) => LuaType::Variadic(Arc::new(match variadic.as_ref() {
            VariadicType::Multi(types) => VariadicType::Multi(
                types
                    .iter()
                    .map(|member| bounded_to(member, depth))
                    .collect(),
            ),
            VariadicType::Base(base) => VariadicType::Base(bounded_to(base, depth)),
        })),
        LuaType::DocFunction(_) if depth == 0 => LuaType::Function,
        LuaType::TypeGuard(_) if depth == 0 => LuaType::Boolean,
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
            LuaType::Table
        }
        LuaType::DocFunction(function) => {
            let inner = |typ: &LuaType| bounded_to(typ, depth - 1);
            LuaType::DocFunction(
                LuaFunctionType::new(
                    function.get_async_state(),
                    function.is_colon_define(),
                    function.is_variadic(),
                    function
                        .get_params()
                        .iter()
                        .map(|(name, typ)| (name.clone(), typ.as_ref().map(inner)))
                        .collect(),
                    inner(function.get_ret()),
                )
                .with_optional_params(function.get_optional_params().to_vec())
                .with_call_arg_roles(function.get_call_arg_roles().to_vec())
                .into(),
            )
        }
        LuaType::Array(array) => LuaType::Array(
            LuaArrayType::new(
                bounded_to(array.get_base(), depth - 1),
                array.get_len().clone(),
            )
            .into(),
        ),
        LuaType::Tuple(tuple) => LuaType::Tuple(
            LuaTupleType::new(
                tuple
                    .get_types()
                    .iter()
                    .map(|member| bounded_to(member, depth - 1))
                    .collect(),
                tuple.status,
            )
            .into(),
        ),
        LuaType::Object(object) => LuaType::Object(
            LuaObjectType::new_with_fields(
                object
                    .get_fields()
                    .iter()
                    .map(|(key, field)| (key.clone(), bounded_to(field, depth - 1)))
                    .collect(),
                object
                    .get_index_access()
                    .iter()
                    .map(|(key, value)| (bounded_to(key, depth - 1), bounded_to(value, depth - 1)))
                    .collect(),
            )
            .into(),
        ),
        LuaType::Intersection(intersection) => LuaType::Intersection(
            LuaIntersectionType::new(
                intersection
                    .get_types()
                    .iter()
                    .map(|member| bounded_to(member, depth - 1))
                    .collect(),
            )
            .into(),
        ),
        LuaType::MergedTable(merged) => LuaType::MergedTable(
            LuaMergedTableType::new(
                merged
                    .get_types()
                    .iter()
                    .map(|member| bounded_to(member, depth - 1))
                    .collect(),
            )
            .into(),
        ),
        LuaType::Generic(generic) => LuaType::Generic(
            LuaGenericType::new(
                generic.get_base_type_id(),
                generic
                    .get_params()
                    .iter()
                    .map(|param| bounded_to(param, depth - 1))
                    .collect(),
            )
            .into(),
        ),
        LuaType::TableGeneric(params) => LuaType::TableGeneric(
            params
                .iter()
                .map(|param| bounded_to(param, depth - 1))
                .collect::<Vec<_>>()
                .into(),
        ),
        LuaType::TableOf(inner) => LuaType::TableOf(Box::new(bounded_to(inner, depth - 1))),
        LuaType::TypeGuard(guarded) => LuaType::TypeGuard(bounded_to(guarded, depth - 1).into()),
    }
}
