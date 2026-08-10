use glua_parser::LuaExpr;

use crate::{DbIndex, LuaType, TypeOps, semantic::infer::narrow::narrow_false_or_nil};

/// Special handling for `and` operator with specific patterns
///
/// For `x and y` where `x` is nullable:
/// - If x is truthy, result is y
/// - If x is falsy (nil or false), result is the falsy value
///
/// Examples:
/// - `string? and 'value'` -> `'value' | nil` (string is truthy, nil is falsy)
/// - `boolean? and 'value'` -> `'value' | false | nil` (true is truthy, false/nil are falsy)
pub fn special_and_rule(
    db: &DbIndex,
    left_type: &LuaType,
    right_type: &LuaType,
    _left_expr: LuaExpr,
    right_expr: LuaExpr,
) -> Option<LuaType> {
    match right_expr {
        LuaExpr::TableExpr(_) | LuaExpr::LiteralExpr(_) => {
            let falsy_part = narrow_false_or_nil(db, left_type.clone());

            // If left type has both truthy and falsy parts, result is: falsy_part | right_type
            // The truthy part would evaluate to right_type, falsy part stays as-is
            if !falsy_part.is_never() && !left_type.is_always_falsy() {
                return Some(TypeOps::Union.apply(db, &falsy_part, right_type));
            }
        }
        _ => {}
    }

    None
}

/// Infers the result type of a Lua `and` binary expression.
///
/// In Lua, the `and` operator returns:
/// - The left operand if it's falsy (nil or false)
/// - The right operand if the left is truthy
///
/// This function computes the union of all possible return values.
///
/// # Examples
///
/// - `nil and y` → `nil` (left is always falsy, returns left)
/// - `x and y` where `x: string` → `y` (string is always truthy, returns right)
/// - `x and y` where `x: boolean`, `y: string` → `false | string`
/// - `x and y` where `x: string | nil`, `y: number` → `nil | number`
pub fn infer_binary_expr_and(
    db: &DbIndex,
    left: LuaType,
    right: LuaType,
) -> crate::semantic::infer::InferResult {
    if left.is_always_falsy() {
        return Ok(left);
    } else if left.is_always_truthy() {
        return Ok(right);
    }

    // An unresolved left operand has no enumerable falsy half — `unknown` there
    // means "not settled yet", not "may be false". Unioning the placeholder in
    // would widen every `unresolved and x` to `x|unknown` and, through
    // `is_always_truthy`, keep a `false` arm alive in the enclosing `or`.
    let falsy_left = narrow_false_or_nil(db, left);
    if falsy_left.is_unknown() {
        return Ok(right);
    }

    Ok(TypeOps::Union.apply(db, &falsy_left, &right))
}
