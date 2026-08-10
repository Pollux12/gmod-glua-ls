use std::ops::Deref;

use crate::{DbIndex, LuaType, LuaUnionType, get_real_type};

// Union member *order* is preserved here, but the member *set* is
// canonical.

pub fn union_type(db: &DbIndex, source: LuaType, target: LuaType) -> LuaType {
    let match_source = get_real_type(db, &source).unwrap_or(&source);
    union_type_impl(match_source, &source, &target)
}

/// `union_type` without the alias dereference, for callers that have no `DbIndex`.
pub(crate) fn union_type_shallow(source: &LuaType, target: &LuaType) -> LuaType {
    union_type_impl(source, source, target)
}

/// Normalise a whole member set the same way repeated `union_type` would.
pub(crate) fn union_type_all(types: Vec<LuaType>) -> LuaType {
    // `any` is the one member that deliberately does NOT absorb its
    // siblings here. The pairwise rule collapses `any | T` to `any`, but a
    // declared `---@type any|string` has to keep the `string` arm or param
    // checking stops flagging it (see the any-union family in
    // `param_type_check_test`), and bare `any` is not treated as nullable
    // by `NeedCheckNil`.
    if types.iter().any(|typ| matches!(typ, LuaType::Any)) || can_use_structural_union(&types) {
        return LuaType::from_vec_structural(types);
    }

    let mut result = LuaType::Never;
    for typ in types {
        result = union_type_shallow(&result, &typ);
    }
    result
}

/// Whether `LuaType::from_vec_structural` alone matches the pairwise fold.
fn can_use_structural_union(types: &[LuaType]) -> bool {
    let (mut num, mut num_variant) = (false, false);
    let (mut int, mut int_const) = (false, false);
    let (mut string, mut string_const) = (false, false);
    let (mut boolean, mut bool_consts) = (false, 0u32);
    let (mut table, mut table_const) = (false, false);

    for typ in types {
        match typ {
            LuaType::Never
            | LuaType::Unknown
            | LuaType::Union(_)
            | LuaType::Ref(_)
            | LuaType::MultiLineUnion(_)
            | LuaType::Function
            | LuaType::DocFunction(_)
            | LuaType::Signature(_) => return false,
            LuaType::Number => num = true,
            LuaType::Integer => {
                num_variant = true;
                int = true;
            }
            LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_) => {
                num_variant = true;
                int_const = true;
            }
            LuaType::FloatConst(_) => num_variant = true,
            LuaType::String => string = true,
            LuaType::StringConst(_) | LuaType::DocStringConst(_) => string_const = true,
            LuaType::Boolean => boolean = true,
            LuaType::BooleanConst(_) => bool_consts += 1,
            LuaType::Table => table = true,
            LuaType::TableConst(_) => table_const = true,
            _ => {}
        }

        if num && num_variant
            || int && int_const
            || string && string_const
            || boolean && bool_consts > 0
            || bool_consts > 1
            || table && table_const
        {
            return false;
        }
    }

    true
}

fn union_type_impl(match_source: &LuaType, source: &LuaType, target: &LuaType) -> LuaType {
    // `any` absorbs everything, except that an explicit nil on the other side
    // survives: bare `any` is not treated as nullable by diagnostics like
    // NeedCheckNil, so collapsing `any | nil` to `any` loses information.
    match (match_source, target) {
        (LuaType::Any, right) if right.is_nullable() => return nullable_any_type(),
        (left, LuaType::Any) if left.is_nullable() => return nullable_any_type(),
        (LuaType::Any, _) | (_, LuaType::Any) => return LuaType::Any,
        _ => {}
    }

    if let Some(merged) = try_collapse(match_source, source, target) {
        return merged;
    }

    match (match_source, target) {
        (LuaType::MultiLineUnion(left), right) => {
            let include = match right {
                LuaType::StringConst(v) => {
                    left.get_unions().iter().any(|(t, _)| match (t, right) {
                        (LuaType::DocStringConst(a), _) => a == v,
                        _ => false,
                    })
                }
                LuaType::IntegerConst(v) => {
                    left.get_unions().iter().any(|(t, _)| match (t, right) {
                        (LuaType::DocIntegerConst(a), _) => a == v,
                        _ => false,
                    })
                }
                _ => false,
            };

            if include {
                return source.clone();
            }
            LuaType::from_vec_structural(vec![source.clone(), target.clone()])
        }
        // union
        (LuaType::Union(left), right) if !right.is_union() => {
            let mut members = left.deref().clone().into_vec();
            absorb(&mut members, right.clone());
            LuaType::from_vec_structural(members)
        }
        // The *source* joins the union, not the dereferenced view of it:
        // the alias is what the caller passed and what has to survive into
        // the rendered type, exactly as the sibling arms keep their
        // operands. The dereference is only ever a matching aid (see
        // `try_collapse`), and `absorb` matches an alias by identity, which
        // is the same answer the other two union arms give.
        (left, LuaType::Union(right)) if !left.is_union() => {
            let mut members = right.deref().clone().into_vec();
            absorb(&mut members, source.clone());
            LuaType::from_vec_structural(members)
        }
        // two union
        (LuaType::Union(left), LuaType::Union(right)) => {
            let mut members = left.into_vec();
            for member in right.into_vec() {
                absorb(&mut members, member);
            }
            LuaType::from_vec_structural(members)
        }

        _ => LuaType::from_vec_structural(vec![source.clone(), target.clone()]),
    }
}

/// The pairwise rules that collapse two union members into a single type.
fn try_collapse(match_source: &LuaType, source: &LuaType, target: &LuaType) -> Option<LuaType> {
    Some(match (match_source, target) {
        (LuaType::Never, _) => target.clone(),
        (_, LuaType::Never) => source.clone(),
        // int | int const
        (LuaType::Integer, LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_)) => {
            LuaType::Integer
        }
        (LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_), LuaType::Integer) => {
            LuaType::Integer
        }
        // float | float const
        (LuaType::Number, right) if right.is_number() => LuaType::Number,
        (left, LuaType::Number) if left.is_number() => LuaType::Number,
        // string | string const
        (LuaType::String, LuaType::StringConst(_) | LuaType::DocStringConst(_)) => LuaType::String,
        (LuaType::StringConst(_) | LuaType::DocStringConst(_), LuaType::String) => LuaType::String,
        // boolean | boolean const
        (LuaType::Boolean, LuaType::BooleanConst(_)) => LuaType::Boolean,
        (LuaType::BooleanConst(_), LuaType::Boolean) => LuaType::Boolean,
        (LuaType::BooleanConst(left), LuaType::BooleanConst(right)) => {
            if left == right {
                LuaType::BooleanConst(*left)
            } else {
                LuaType::Boolean
            }
        }
        // table | table const
        (LuaType::Table, LuaType::TableConst(_)) => LuaType::Table,
        (LuaType::TableConst(_), LuaType::Table) => LuaType::Table,
        // function | function const
        (LuaType::Function, LuaType::DocFunction(_) | LuaType::Signature(_)) => LuaType::Function,
        (LuaType::DocFunction(_) | LuaType::Signature(_), LuaType::Function) => LuaType::Function,
        // class references
        (LuaType::Ref(id1), LuaType::Ref(id2)) if id1 == id2 => source.clone(),
        // same type
        (left, right) if *left == *right => source.clone(),
        _ => return None,
    })
}

/// Add `ty` to an existing union's member list, applying absorption rules.
fn absorb(members: &mut Vec<LuaType>, ty: LuaType) {
    let mut ty = ty;
    // A merged member keeps the slot of the member it merged with, so absorbing
    // never reorders the survivors. Member order is semantic for overloads and
    // template dispatch, which the structural constructor deliberately leaves
    // alone.
    let mut slot = members.len();

    'restart: loop {
        for i in 0..members.len() {
            if let Some(merged) = try_collapse(&members[i], &members[i], &ty) {
                members.remove(i);
                ty = merged;
                slot = slot.min(i);
                continue 'restart;
            }
        }
        break;
    }

    members.insert(slot.min(members.len()), ty);
}

fn nullable_any_type() -> LuaType {
    LuaType::Union(LuaUnionType::Nullable(LuaType::Any).into())
}
