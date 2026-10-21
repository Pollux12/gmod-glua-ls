use rustc_hash::FxHashSet;

use crate::{DbIndex, LuaType};

/// Whether a parameter of this type accepts an omitted argument: nil, unknown or any, through aliases and unions.
pub(crate) fn is_nullable(db: &DbIndex, typ: &LuaType) -> bool {
    admits_nil(db, typ, false)
}

/// Whether a parameter of type `typ` may be left out: it is nullable or
/// variadic.
pub(crate) fn is_optional_param_type(db: &DbIndex, typ: &LuaType) -> bool {
    admits_nil(db, typ, true)
}

fn admits_nil(db: &DbIndex, typ: &LuaType, variadic_admits: bool) -> bool {
    let mut stack: Vec<LuaType> = Vec::new();
    stack.push(typ.clone());
    let mut visited = FxHashSet::default();
    while let Some(typ) = stack.pop() {
        if visited.contains(&typ) {
            continue;
        }
        visited.insert(typ.clone());
        match typ {
            LuaType::Any | LuaType::Unknown | LuaType::Nil => return true,
            LuaType::Variadic(_) if variadic_admits => return true,
            LuaType::Ref(decl_id) => {
                if let Some(decl) = db.get_type_index().get_type_decl(&decl_id)
                    && decl.is_alias()
                    && let Some(alias_origin) = decl.get_alias_ref()
                {
                    stack.push(alias_origin.clone());
                }
            }
            LuaType::Union(u) => {
                for t in u.into_vec() {
                    stack.push(t);
                }
            }
            LuaType::MultiLineUnion(m) => {
                for (t, _) in m.get_unions() {
                    stack.push(t.clone());
                }
            }
            _ => {}
        }
    }
    false
}
