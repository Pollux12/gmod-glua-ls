mod complex_type;
mod func_type;
mod generic_type;
mod intersection_utils;
mod ref_type;
mod simple_type;
mod sub_type;
mod test;
mod type_check_context;
mod type_check_fail_reason;
mod type_check_guard;

use std::ops::Deref;

use complex_type::check_complex_type_compact;
use func_type::{check_doc_func_type_compact, check_sig_type_compact};
use generic_type::check_generic_type_compact;
use ref_type::check_ref_type_compact;
use simple_type::check_simple_type_compact;
pub use type_check_fail_reason::TypeCheckFailReason;
use type_check_guard::TypeCheckGuard;

use crate::{
    LuaUnionType,
    db_index::{DbIndex, LuaType},
    semantic::type_check::type_check_context::TypeCheckContext,
};
pub use sub_type::is_sub_type_of;
pub type TypeCheckResult = Result<(), TypeCheckFailReason>;
pub use type_check_context::TypeCheckCheckLevel;

pub fn check_type_compact(
    db: &DbIndex,
    source: &LuaType,
    compact_type: &LuaType,
) -> TypeCheckResult {
    let mut context = TypeCheckContext::new(db, false, TypeCheckCheckLevel::Normal);
    check_general_type_compact(&mut context, source, compact_type, TypeCheckGuard::new())
}

pub fn check_type_compact_detail(
    db: &DbIndex,
    source: &LuaType,
    compact_type: &LuaType,
) -> TypeCheckResult {
    let guard = TypeCheckGuard::new();
    let mut context = TypeCheckContext::new(db, true, TypeCheckCheckLevel::Normal);
    check_general_type_compact(&mut context, source, compact_type, guard)
}

pub fn check_type_compact_with_level(
    db: &DbIndex,
    source: &LuaType,
    compact_type: &LuaType,
    level: TypeCheckCheckLevel,
) -> TypeCheckResult {
    let mut context = TypeCheckContext::new(db, false, level);
    check_general_type_compact(&mut context, source, compact_type, TypeCheckGuard::new())
}

fn check_general_type_compact(
    context: &mut TypeCheckContext,
    source: &LuaType,
    compact_type: &LuaType,
    check_guard: TypeCheckGuard,
) -> TypeCheckResult {
    if is_like_any(compact_type) {
        return Ok(());
    }

    if fast_eq_check(source, compact_type) {
        return Ok(());
    }

    if let Some(origin_type) = escape_type(context.db, compact_type) {
        return check_general_type_compact(
            context,
            source,
            &origin_type,
            check_guard.next_level()?,
        );
    }

    // When the actual value is tableof<T>, unwrap to T for compatibility checking.
    // tableof<T> has the same fields as T, so it should be assignable where T is expected.
    if let LuaType::TableOf(inner) = compact_type {
        return check_general_type_compact(context, source, inner, check_guard.next_level()?);
    }

    if context
        .db
        .get_emmyrc()
        .strict
        .allow_nullable_as_non_nullable
        && let LuaType::Union(union_type) = compact_type
    {
        match union_type.deref() {
            LuaUnionType::Nullable(non_nullable_type) => {
                return check_general_type_compact(
                    context,
                    source,
                    non_nullable_type,
                    check_guard.next_level()?,
                );
            }
            LuaUnionType::Multi(types) if types.contains(&LuaType::Nil) => {
                let non_nil: Vec<LuaType> =
                    types.iter().filter(|t| !matches!(t, LuaType::Nil)).cloned().collect();
                let stripped = if non_nil.len() == 1 {
                    non_nil.into_iter().next().unwrap()
                } else {
                    LuaType::Union(LuaUnionType::Multi(non_nil).into())
                };
                return check_general_type_compact(
                    context,
                    source,
                    &stripped,
                    check_guard.next_level()?,
                );
            }
            _ => {}
        }
    }

    match source {
        LuaType::Unknown | LuaType::Any => Ok(()),
        // simple type
        LuaType::Nil
        | LuaType::Table
        | LuaType::Userdata
        | LuaType::Function
        | LuaType::Thread
        | LuaType::Boolean
        | LuaType::String
        | LuaType::Integer
        | LuaType::Number
        | LuaType::Io
        | LuaType::Global
        | LuaType::BooleanConst(_)
        | LuaType::StringConst(_)
        | LuaType::IntegerConst(_)
        | LuaType::FloatConst(_)
        | LuaType::TableConst(_)
        | LuaType::DocStringConst(_)
        | LuaType::DocIntegerConst(_)
        | LuaType::DocBooleanConst(_)
        | LuaType::TplRef(_)
        | LuaType::StrTplRef(_)
        | LuaType::ConstTplRef(_)
        | LuaType::Namespace(_)
        | LuaType::Variadic(_)
        | LuaType::Language(_) => {
            check_simple_type_compact(context, &source, &compact_type, check_guard)
        }

        // type ref
        LuaType::Ref(type_decl_id) => {
            check_ref_type_compact(context, type_decl_id, &compact_type, check_guard)
        }
        LuaType::Def(type_decl_id) => {
            check_ref_type_compact(context, type_decl_id, &compact_type, check_guard)
        }
        // invaliad source type
        // LuaType::Module(arc_intern) => todo!(),

        // function type
        LuaType::DocFunction(doc_func) => {
            check_doc_func_type_compact(context, doc_func, &compact_type, check_guard)
        }
        // signature type
        LuaType::Signature(sig_id) => {
            check_sig_type_compact(context, sig_id, &compact_type, check_guard)
        }

        // complex type
        LuaType::Array(_)
        | LuaType::Tuple(_)
        | LuaType::Object(_)
        | LuaType::Union(_)
        | LuaType::Intersection(_)
        | LuaType::TableGeneric(_)
        | LuaType::Call(_)
        | LuaType::MultiLineUnion(_) => {
            check_complex_type_compact(context, &source, &compact_type, check_guard)
        }

        // generic type
        LuaType::Generic(generic) => {
            check_generic_type_compact(context, generic, &compact_type, check_guard)
        }
        // invalid source type
        // LuaType::MemberPathExist(_) |
        LuaType::Instance(instantiate) => check_general_type_compact(
            context,
            instantiate.get_base(),
            &compact_type,
            check_guard.next_level()?,
        ),
        LuaType::TypeGuard(_) => {
            if compact_type.is_boolean() {
                return Ok(());
            }
            Err(TypeCheckFailReason::TypeNotMatch)
        }
        LuaType::Never => {
            // never 只能赋值给 never
            if compact_type.is_never() {
                return Ok(());
            }
            Err(TypeCheckFailReason::TypeNotMatch)
        }
        LuaType::ModuleRef(_) => Ok(()),
        // SelfInfer represents `self` — it should accept any class/ref type
        // since `self` is always the owning class instance.
        LuaType::SelfInfer => Ok(()),
        // TableOf(T) acts like T for assignment/parameter checking,
        // since it has all the same fields (just no method calls).
        LuaType::TableOf(inner) => {
            check_general_type_compact(context, inner, &compact_type, check_guard.next_level()?)
        }
        _ => Err(TypeCheckFailReason::TypeNotMatch),
    }
}

fn is_like_any(ty: &LuaType) -> bool {
    matches!(
        ty,
        LuaType::Any
            | LuaType::Unknown
            | LuaType::SelfInfer
            | LuaType::TplRef(_)
            | LuaType::StrTplRef(_)
            | LuaType::ConstTplRef(_)
    )
}

fn fast_eq_check(a: &LuaType, b: &LuaType) -> bool {
    match (a, b) {
        (LuaType::Nil, LuaType::Nil)
        | (LuaType::Table, LuaType::Table)
        | (LuaType::Userdata, LuaType::Userdata)
        | (LuaType::Function, LuaType::Function)
        | (LuaType::Thread, LuaType::Thread)
        | (LuaType::Boolean, LuaType::Boolean)
        | (LuaType::String, LuaType::String)
        | (LuaType::Integer, LuaType::Integer)
        | (LuaType::Number, LuaType::Number)
        | (LuaType::Io, LuaType::Io)
        | (LuaType::Global, LuaType::Global)
        | (LuaType::Unknown, LuaType::Unknown)
        | (LuaType::Any, LuaType::Any) => true,
        (LuaType::Ref(type_id_left), LuaType::Ref(type_id_right)) => type_id_left == type_id_right,
        (LuaType::Union(u), LuaType::Ref(type_id_right)) => {
            if let LuaUnionType::Nullable(LuaType::Ref(type_id_left)) = u.deref() {
                return type_id_left == type_id_right;
            }
            false
        }
        _ => false,
    }
}

fn escape_type(db: &DbIndex, typ: &LuaType) -> Option<LuaType> {
    match typ {
        LuaType::Ref(type_id) => {
            let type_decl = db.get_type_index().get_type_decl(type_id)?;
            if type_decl.is_alias()
                && let Some(origin_type) = type_decl.get_alias_origin(db, None)
            {
                return Some(origin_type.clone());
            }
        }
        // todo donot escape
        LuaType::Instance(inst) => {
            let base = inst.get_base();
            return Some(base.clone());
        }
        LuaType::MultiLineUnion(multi_union) => {
            let union = multi_union.to_union();
            return Some(union);
        }
        LuaType::TypeGuard(_) => return Some(LuaType::Boolean),
        LuaType::ModuleRef(file_id) => {
            let module_info = db.get_module_index().get_module(*file_id)?;
            if let Some(export_type) = &module_info.export_type {
                return Some(export_type.clone());
            }
        }
        _ => {}
    }

    None
}

#[cfg(test)]
mod nullable_leniency_test {
    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn allow_nullable_as_non_nullable_enabled_allows_nullable_argument() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Vector

            ---@return Vector?
            local function get_vec()
            end

            ---@param v Vector
            local function takes_vec(v)
            end

            takes_vec(get_vec())
            "#,
        ));
    }

    #[test]
    fn allow_nullable_as_non_nullable_disabled_reports_nullable_argument() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.strict.allow_nullable_as_non_nullable = false;
        ws.update_emmyrc(emmyrc);

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Vector

            ---@return Vector?
            local function get_vec()
            end

            ---@param v Vector
            local function takes_vec(v)
            end

            takes_vec(get_vec())
            "#,
        ));
    }

    #[test]
    fn allow_nullable_as_non_nullable_does_not_allow_bare_nil() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Vector

            ---@param v Vector
            local function takes_vec(v)
            end

            takes_vec(nil)
            "#,
        ));
    }
}
