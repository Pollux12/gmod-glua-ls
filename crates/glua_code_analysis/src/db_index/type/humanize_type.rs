use std::collections::HashSet;

use itertools::Itertools;

use crate::{
    AsyncState, DbIndex, DynamicFieldOwner, GenericTpl, LuaAliasCallType, LuaConditionalType,
    LuaFunctionType, LuaGenericType, LuaInstanceType, LuaIntersectionType, LuaMemberKey,
    LuaMemberOwner, LuaMergedTableType, LuaObjectType, LuaSignatureId, LuaStringTplType,
    LuaTupleType, LuaType, LuaTypeDeclId, LuaUnionType, TypeSubstitutor, VariadicType,
    semantic::{LuaMemberInfo, find_members},
};

use super::{LuaAliasCallKind, LuaMultiLineUnion};

pub const DEFAULT_DETAIL_MEMBER_DISPLAY_COUNT: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderLevel {
    Documentation,
    /// Like `Detailed`, but with a custom max display count for class members.
    /// Used by the hover verbosity system to show progressively more members.
    DetailedCount(usize),
    Detailed,
    Simple,
    Normal,
    Brief,
    Minimal,
}

impl RenderLevel {
    pub fn next_level(self) -> RenderLevel {
        match self {
            RenderLevel::Documentation => RenderLevel::Simple,
            RenderLevel::DetailedCount(_) => RenderLevel::Simple,
            RenderLevel::Detailed => RenderLevel::Simple,
            RenderLevel::Simple => RenderLevel::Normal,
            RenderLevel::Normal => RenderLevel::Brief,
            RenderLevel::Brief => RenderLevel::Minimal,
            RenderLevel::Minimal => RenderLevel::Minimal,
        }
    }
}

fn hover_escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{07}' => out.push_str("\\a"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0b}' => out.push_str("\\v"),
            '\u{1b}' => out.push_str("\\27"),
            ch if ch.is_control() => {
                let code = ch as u32;
                if code <= 0xFF {
                    out.push_str(&format!("\\x{code:02X}"));
                } else {
                    out.push_str(&format!("\\u{{{code:X}}}"));
                }
            }
            _ => out.push(ch),
        }
    }

    out
}

pub fn humanize_type(db: &DbIndex, ty: &LuaType, level: RenderLevel) -> String {
    match ty {
        LuaType::Any => "any".to_string(),
        LuaType::Nil => "nil".to_string(),
        LuaType::Boolean => "boolean".to_string(),
        LuaType::Number => "number".to_string(),
        LuaType::String => "string".to_string(),
        LuaType::Table => "table".to_string(),
        LuaType::Function => "function".to_string(),
        LuaType::Thread => "thread".to_string(),
        LuaType::Userdata => "userdata".to_string(),
        LuaType::IntegerConst(i) => i.to_string(),
        LuaType::FloatConst(f) => {
            let s = f.to_string();
            // 如果字符串不包含小数点，添加 ".0"
            if !s.contains('.') {
                format!("{}.0", s)
            } else {
                s
            }
        }
        LuaType::TableConst(v) => {
            let member_owner = LuaMemberOwner::Element(v.clone());
            humanize_table_const_type(db, member_owner, level)
        }
        LuaType::Global => "global".to_string(),
        LuaType::Def(id) => humanize_def_type(db, id, level),
        LuaType::Union(union) => humanize_union_type(db, union, level),
        LuaType::Tuple(tuple) => humanize_tuple_type(db, tuple, level),
        LuaType::Unknown => "unknown".to_string(),
        LuaType::Integer => "integer".to_string(),
        LuaType::Io => "io".to_string(),
        LuaType::SelfInfer => "self".to_string(),
        LuaType::BooleanConst(b) => b.to_string(),
        LuaType::StringConst(s) => format!("\"{}\"", hover_escape_string(s)),
        LuaType::DocStringConst(s) => format!("\"{}\"", hover_escape_string(s)),
        LuaType::DocIntegerConst(i) => i.to_string(),
        LuaType::DocBooleanConst(b) => b.to_string(),
        LuaType::Ref(id) => {
            if let Some(type_decl) = db.get_type_index().get_type_decl(id) {
                if type_decl.is_alias() {
                    if let Some(alias_type) = humanize_alias_ref_type(db, ty, level) {
                        return alias_type;
                    }
                }

                let name = type_decl.get_full_name().to_string();
                humanize_simple_type(db, id, &name, level).unwrap_or(name)
            } else {
                id.get_name().to_string()
            }
        }
        LuaType::Array(arr_inner) => humanize_array_type(db, arr_inner.get_base(), level),
        LuaType::Call(alias_call) => humanize_call_type(db, alias_call, level),
        LuaType::DocFunction(lua_func) => humanize_doc_function_type(db, lua_func, level),
        LuaType::Object(object) => humanize_object_type(db, object, level),
        LuaType::Intersection(inter) => humanize_intersect_type(db, inter, level),
        LuaType::MergedTable(merged) => humanize_merged_table_type(db, merged, level),
        LuaType::Generic(generic) => humanize_generic_type(db, generic, level),
        LuaType::TableGeneric(table_generic_params) => {
            humanize_table_generic_type(db, table_generic_params, level)
        }
        LuaType::TplRef(tpl) => humanize_tpl_ref_type(tpl),
        LuaType::StrTplRef(str_tpl) => humanize_str_tpl_ref_type(str_tpl),
        LuaType::Variadic(multi) => humanize_variadic_type(db, multi, level),
        LuaType::Instance(ins) => humanize_instance_type(db, ins, level),
        LuaType::Signature(signature_id) => humanize_signature_type(db, signature_id, level),
        LuaType::Namespace(ns) => ns.to_string(),
        LuaType::MultiLineUnion(multi_union) => {
            humanize_multi_line_union_type(db, multi_union, level)
        }
        LuaType::TypeGuard(inner) => {
            let type_str = humanize_type(db, inner, level.next_level());
            format!("TypeGuard<{}>", type_str)
        }
        LuaType::TableOf(inner) => {
            let type_str = humanize_type(db, inner, level.next_level());
            format!("tableof<{}>", type_str)
        }
        LuaType::ConstTplRef(const_tpl) => humanize_const_tpl_ref_type(const_tpl),
        LuaType::Language(s) => s.to_string(),
        LuaType::Conditional(c) => humanize_conditional_type(db, c, level),
        LuaType::ConditionalInfer(s) => s.to_string(),
        LuaType::Never => "never".to_string(),
        LuaType::ModuleRef(file_id) => {
            if let Some(module_info) = db.get_module_index().get_module(*file_id) {
                humanize_type(
                    db,
                    &module_info.export_type.clone().unwrap_or(LuaType::Any),
                    level,
                )
            } else {
                "module 'unknown'".to_string()
            }
        }
        _ => "unknown".to_string(),
    }
}

fn humanize_alias_ref_type(db: &DbIndex, ty: &LuaType, level: RenderLevel) -> Option<String> {
    if !matches!(
        level,
        RenderLevel::Documentation | RenderLevel::DetailedCount(_) | RenderLevel::Detailed
    ) {
        return None;
    }

    let resolved = super::resolve_alias_type(db, ty);
    let alias_id = resolved.alias_id?;
    if resolved.typ == *ty {
        return None;
    }

    Some(format!(
        "(alias) {} = {}",
        alias_id.get_simple_name(),
        humanize_type(db, &resolved.typ, level)
    ))
}

fn humanize_def_type(db: &DbIndex, id: &LuaTypeDeclId, level: RenderLevel) -> String {
    let type_decl = match db.get_type_index().get_type_decl(id) {
        Some(type_decl) => type_decl,
        None => return id.get_name().to_string(),
    };

    let full_name = type_decl.get_full_name();
    let generic = match db.get_type_index().get_generic_params(id) {
        Some(generic) => generic,
        None => {
            return humanize_simple_type(db, id, full_name, level).unwrap_or(full_name.to_string());
        }
    };

    let generic_names = generic
        .iter()
        .map(|it| it.name.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}<{}>", full_name, generic_names)
}

fn humanize_simple_type(
    db: &DbIndex,
    id: &LuaTypeDeclId,
    name: &str,
    level: RenderLevel,
) -> Option<String> {
    let max_display_count = match level {
        RenderLevel::Documentation => 500,
        RenderLevel::DetailedCount(n) => n,
        RenderLevel::Detailed => DEFAULT_DETAIL_MEMBER_DISPLAY_COUNT,
        _ => return Some(name.to_string()),
    };

    let member_owner = LuaMemberOwner::Type(id.clone());
    let member_index = db.get_member_index();
    let members = member_index.get_sorted_members(&member_owner)?;
    let all_count = members.len();
    let mut member_strings = String::new();
    let mut function_vec = Vec::new();
    let mut count = 0;
    for member in members {
        let member_key = member.get_key();
        let type_cache = db.get_type_index().get_type_cache(&member.get_id().into());
        let type_cache = match type_cache {
            Some(type_cache) => type_cache,
            None => &super::LuaTypeCache::InferType(LuaType::Any),
        };
        if type_cache.is_function() {
            if function_vec.len() < max_display_count {
                function_vec.push(member_key);
            }
        } else {
            let typ = type_cache.as_type();
            let member_string = build_table_member_string(
                db,
                member_key,
                typ,
                humanize_type(db, typ, level.next_level()),
                level,
            );

            member_strings.push_str(&format!("    {},\n", member_string));
            count += 1;
            if count >= max_display_count {
                break;
            }
        }
    }

    if all_count == 0 {
        return Some(name.to_string());
    }

    if count < all_count {
        for function_key in function_vec {
            let member_string = build_table_member_string(
                db,
                function_key,
                &LuaType::Function,
                "function".to_string(),
                level,
            );

            member_strings.push_str(&format!("    {},\n", member_string));
            count += 1;
            if count >= max_display_count {
                break;
            }
        }
    }
    if count >= max_display_count {
        member_strings.push_str(&format!("    ...(+{})\n", all_count - max_display_count));
    }
    Some(format!("{} {{\n{}}}", name, member_strings))
}

fn humanize_union_type(db: &DbIndex, union: &LuaUnionType, level: RenderLevel) -> String {
    format_union_type(union, level, |ty, level| {
        humanize_type(db, ty, level.next_level())
    })
}

pub fn format_union_type<F>(
    union: &LuaUnionType,
    level: RenderLevel,
    mut type_formatter: F,
) -> String
where
    F: FnMut(&LuaType, RenderLevel) -> String,
{
    let types = union.into_vec();
    let num = match level {
        RenderLevel::Documentation => 500,
        RenderLevel::DetailedCount(_) | RenderLevel::Detailed => 8,
        RenderLevel::Simple => 6,
        RenderLevel::Normal => 4,
        RenderLevel::Brief => 2,
        RenderLevel::Minimal => 2,
    };
    // Sort before truncation so rendered subsets stay canonical across runs.
    let mut seen = HashSet::new();
    let mut type_strings = Vec::new();
    let mut has_nil = false;
    let mut has_function = false;
    for ty in types.iter() {
        if ty.is_nil() {
            has_nil = true;
            continue;
        } else if ty.is_function() {
            has_function = true;
        }
        let type_str = type_formatter(ty, level.next_level());
        if seen.insert(type_str.clone()) {
            type_strings.push(type_str);
        }
    }
    type_strings.sort_unstable();
    let dots = if type_strings.len() > num { "..." } else { "" };
    let display_types: Vec<_> = type_strings.into_iter().take(num).collect();
    let type_str = display_types.join("|");

    if display_types.len() == 1 {
        if has_function && has_nil {
            format!("({})?", type_str)
        } else {
            format!("{}{}", type_str, if has_nil { "?" } else { "" })
        }
    } else {
        format!("({}{}){}", type_str, dots, if has_nil { "?" } else { "" })
    }
}

fn humanize_multi_line_union_type(
    db: &DbIndex,
    multi_union: &LuaMultiLineUnion,
    level: RenderLevel,
) -> String {
    let mut members = multi_union.get_unions().to_vec();
    members.sort_by_cached_key(|(typ, description)| {
        let type_key = humanize_type(db, typ, level.next_level());
        let description_key = description.as_deref().unwrap_or_default().to_string();
        (type_key, description_key)
    });

    let num = match level {
        RenderLevel::Documentation => 500,
        RenderLevel::DetailedCount(_) | RenderLevel::Detailed => 10,
        RenderLevel::Simple => 8,
        RenderLevel::Normal => 4,
        RenderLevel::Brief => 2,
        RenderLevel::Minimal => 2,
    };
    let dots = if members.len() > num { "..." } else { "" };

    let type_str = members
        .iter()
        .take(num)
        .map(|(ty, _)| humanize_type(db, ty, level.next_level()))
        .collect::<Vec<_>>()
        .join("|");

    let mut text = format!("({}{})", type_str, dots);
    if !matches!(level, RenderLevel::DetailedCount(_) | RenderLevel::Detailed) {
        return text;
    }

    text.push('\n');
    for (typ, description) in &members {
        let type_humanize_text = humanize_type(db, typ, RenderLevel::Minimal);
        if let Some(description) = description {
            text.push_str(&format!(
                "    | {} -- {}\n",
                type_humanize_text,
                description.replace('\n', " ")
            ));
        } else {
            text.push_str(&format!("    | {}\n", type_humanize_text));
        }
    }

    text
}

fn humanize_tuple_type(db: &DbIndex, tuple: &LuaTupleType, level: RenderLevel) -> String {
    let types = tuple.get_types();
    let num = match level {
        RenderLevel::Documentation => 500,
        RenderLevel::DetailedCount(_) | RenderLevel::Detailed => 10,
        RenderLevel::Simple => 8,
        RenderLevel::Normal => 4,
        RenderLevel::Brief => 2,
        RenderLevel::Minimal => 2,
    };

    let dots = if types.len() > num { "..." } else { "" };

    let type_str = types
        .iter()
        .take(num)
        .map(|ty| humanize_type(db, ty, level.next_level()))
        .collect::<Vec<_>>()
        .join(",");
    format!("({}{})", type_str, dots)
}

fn humanize_array_type(db: &DbIndex, inner: &LuaType, level: RenderLevel) -> String {
    let element_type = humanize_type(db, inner, level.next_level());
    format!("{}[]", element_type)
}

#[allow(unused)]
fn humanize_call_type(db: &DbIndex, inner: &LuaAliasCallType, level: RenderLevel) -> String {
    let basic = match inner.get_call_kind() {
        LuaAliasCallKind::Sub => "sub",
        LuaAliasCallKind::Add => "add",
        LuaAliasCallKind::KeyOf => "keyof",
        LuaAliasCallKind::Extends => "extends",
        LuaAliasCallKind::Select => "select",
        LuaAliasCallKind::Unpack => "unpack",
        LuaAliasCallKind::Index => "index",
        LuaAliasCallKind::RawGet => "rawget",
        LuaAliasCallKind::Merge => "Merge",
        LuaAliasCallKind::Split => "split",
    };
    let operands = inner
        .get_operands()
        .iter()
        .map(|ty| humanize_type(db, ty, level.next_level()))
        .collect::<Vec<_>>()
        .join(",");

    format!("{}<{}>", basic, operands)
}

fn humanize_doc_function_type(
    db: &DbIndex,
    lua_func: &LuaFunctionType,
    level: RenderLevel,
) -> String {
    if level == RenderLevel::Minimal {
        return "fun(...) -> ...".to_string();
    }

    let prev = match lua_func.get_async_state() {
        AsyncState::None => "fun",
        AsyncState::Async => "async fun",
        AsyncState::Sync => "sync fun",
    };
    let params = lua_func
        .get_params()
        .iter()
        .map(|param| {
            let name = param.0.clone();
            if let Some(ty) = &param.1 {
                format!("{}: {}", name, humanize_type(db, ty, level.next_level()))
            } else {
                name.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");

    let ret_type = lua_func.get_ret();
    let return_nil = match ret_type {
        LuaType::Variadic(variadic) => matches!(variadic.get_type(0), Some(LuaType::Nil)),
        _ => ret_type.is_nil(),
    };

    if return_nil {
        return format!("{}({})", prev, params);
    }

    let ret_str = humanize_type(db, ret_type, level.next_level());

    format!("{}({}) -> {}", prev, params, ret_str)
}

fn humanize_object_type(db: &DbIndex, object: &LuaObjectType, level: RenderLevel) -> String {
    let num = match level {
        RenderLevel::Documentation => 500,
        RenderLevel::DetailedCount(_) | RenderLevel::Detailed => 10,
        RenderLevel::Simple => 8,
        RenderLevel::Normal => 4,
        RenderLevel::Brief => 2,
        RenderLevel::Minimal => {
            return "{...}".to_string();
        }
    };

    let dots = if object.get_fields().len() > num {
        ", ..."
    } else {
        ""
    };

    let fields = object
        .get_fields()
        .iter()
        .sorted_by(|a, b| a.0.cmp(b.0))
        .take(num)
        .map(|field| {
            let name = field.0.clone();
            let ty_str = humanize_type(db, field.1, level.next_level());
            match name {
                LuaMemberKey::Integer(i) => format!("[{}]: {}", i, ty_str),
                LuaMemberKey::Name(s) => {
                    format!("{}: {}", humanize_member_key_name(s.as_str()), ty_str)
                }
                LuaMemberKey::None => ty_str,
                LuaMemberKey::ExprType(_) => ty_str,
            }
        })
        .collect::<Vec<_>>()
        .join(", ");

    let access = object
        .get_index_access()
        .iter()
        .map(|(key, value)| {
            let key_str = humanize_type(db, key, level.next_level());
            let value_str = humanize_type(db, value, level.next_level());
            format!("[{}]: {}", key_str, value_str)
        })
        .collect::<Vec<_>>()
        .join(",");

    if access.is_empty() {
        return format!("{{ {}{} }}", fields, dots);
    } else if fields.is_empty() {
        return format!("{{ {}{} }}", access, dots);
    }
    format!("{{ {}, {}{} }}", fields, access, dots)
}

fn humanize_intersect_type(
    db: &DbIndex,
    inter: &LuaIntersectionType,
    level: RenderLevel,
) -> String {
    let num = match level {
        RenderLevel::Documentation => 500,
        RenderLevel::DetailedCount(_) | RenderLevel::Detailed => 10,
        RenderLevel::Simple => 8,
        RenderLevel::Normal => 4,
        RenderLevel::Brief => 2,
        RenderLevel::Minimal => 2,
    };

    let types = inter.get_types();
    let dots = if types.len() > num { ", ..." } else { "" };

    let type_str = types
        .iter()
        .take(num)
        .map(|ty| humanize_type(db, ty, level.next_level()))
        .collect::<Vec<_>>()
        .join(" & ");
    format!("({}{})", type_str, dots)
}

fn humanize_generic_type(db: &DbIndex, generic: &LuaGenericType, level: RenderLevel) -> String {
    let base_id = generic.get_base_type_id();
    let type_decl = match db.get_type_index().get_type_decl(&base_id) {
        Some(type_decl) => type_decl,
        None => return base_id.get_name().to_string(),
    };

    let full_name = type_decl.get_full_name();

    let generic_inst_params = generic
        .get_params()
        .iter()
        .map(|ty| humanize_type(db, ty, level.next_level()))
        .collect::<Vec<_>>()
        .join(",");

    let generic_base = format!("{}<{}>", full_name, generic_inst_params);
    if matches!(
        level,
        RenderLevel::Documentation | RenderLevel::DetailedCount(_) | RenderLevel::Detailed
    ) && type_decl.is_alias()
    {
        let substituor = TypeSubstitutor::from_type_array(generic.get_params().clone());
        if let Some(origin_type) = type_decl.get_alias_origin(db, Some(&substituor)) {
            // prevent infinite recursion
            let origin_type_str = humanize_type(db, &origin_type, level.next_level());
            return format!("{} = {}", generic_base, origin_type_str);
        }
    }

    generic_base
}

/// How a table-like type should be laid out at a given [`RenderLevel`].
///
/// `Detailed` is the multi-line block form; `Compact` is the inline
/// `{ a, b }` form. Returns `None` for levels that should collapse the table to
/// a bare `table` (Brief/Minimal), which also terminates nested recursion.
#[derive(Clone, Copy)]
enum TableLayout {
    Detailed,
    Compact,
}

impl TableLayout {
    fn from_level(level: RenderLevel) -> Option<Self> {
        match level {
            RenderLevel::Documentation | RenderLevel::DetailedCount(_) | RenderLevel::Detailed => {
                Some(Self::Detailed)
            }
            // `Normal` reuses the compact inline form (rather than collapsing to
            // a bare `table`) so nested table rows inside a `Simple`-rendered
            // parent — e.g. field hovers, whose members render one level down at
            // `Normal` — still show their shape instead of `table`. Recursion
            // still terminates because `Normal.next_level()` is `Brief`, which
            // maps to `None` below.
            RenderLevel::Simple | RenderLevel::Normal => Some(Self::Compact),
            RenderLevel::Brief | RenderLevel::Minimal => None,
        }
    }
}

fn humanize_table_const_type_detail_and_simple(
    db: &DbIndex,
    member_owned: LuaMemberOwner,
    level: RenderLevel,
) -> Option<String> {
    let layout = TableLayout::from_level(level)?;
    let member_index = db.get_member_index();
    let members = member_index.get_sorted_members(&member_owned)?;

    // Use the custom count from DetailedCount, or the compact default for Detailed.
    let detailed_max = match level {
        RenderLevel::DetailedCount(n) => n,
        _ => DEFAULT_DETAIL_MEMBER_DISPLAY_COUNT,
    };

    let mut total_length = 0;
    let mut total_line = 0;
    let mut members_string = String::new();
    let has_dynamic_wildcard = owner_has_dynamic_wildcard(db, &member_owned);
    let mut has_dynamic_member = false;
    let mut truncated = false;
    for member in members {
        let key = member.get_key();
        if matches!(key, LuaMemberKey::ExprType(typ) if typ.is_unknown()) {
            has_dynamic_member = true;
        }
        let type_cache = db.get_type_index().get_type_cache(&member.get_id().into());
        let type_cache = match type_cache {
            Some(type_cache) => type_cache,
            None => &super::LuaTypeCache::InferType(LuaType::Any),
        };
        let member_string = build_table_member_string(
            db,
            key,
            type_cache.as_type(),
            humanize_type(db, type_cache.as_type(), level.next_level()),
            level,
        );

        match layout {
            TableLayout::Detailed => {
                total_line += 1;
                members_string.push_str(&format!("    {},\n", member_string));
                if total_line >= detailed_max {
                    members_string.push_str("    ...\n");
                    break;
                }
            }
            TableLayout::Compact => {
                let member_string_len = member_string.chars().count();
                if total_length != 0 {
                    members_string.push_str(", ");
                    total_length += 2; // account for ", "
                }

                total_length += member_string_len;
                members_string.push_str(&member_string);
                if total_length > 54 {
                    members_string.push_str(", ...");
                    truncated = true;
                    break;
                }
            }
        }
    }

    if has_dynamic_wildcard && !has_dynamic_member && !truncated {
        let dynamic_member_string = build_table_member_string(
            db,
            &LuaMemberKey::ExprType(LuaType::Unknown),
            &LuaType::Any,
            humanize_type(db, &LuaType::Any, level.next_level()),
            level,
        );
        match layout {
            TableLayout::Detailed => {
                if total_line < detailed_max {
                    members_string.push_str(&format!("    {},\n", dynamic_member_string));
                }
            }
            TableLayout::Compact => {
                let dynamic_member_string_len = dynamic_member_string.chars().count();
                if total_length != 0 {
                    members_string.push_str(", ");
                    total_length += 2;
                }
                total_length += dynamic_member_string_len;
                members_string.push_str(&dynamic_member_string);
                if total_length > 54 {
                    members_string.push_str(", ...");
                }
            }
        }
    }

    Some(match layout {
        TableLayout::Detailed => format!("{{\n{}}}", members_string),
        TableLayout::Compact => format!("{{ {} }}", members_string),
    })
}

fn owner_has_dynamic_wildcard(db: &DbIndex, owner: &LuaMemberOwner) -> bool {
    let dynamic_owner = match owner {
        LuaMemberOwner::Type(type_id) => DynamicFieldOwner::Type(type_id.clone()),
        LuaMemberOwner::Element(table_range) => DynamicFieldOwner::Table(table_range.clone()),
        _ => return false,
    };

    let index = db.get_dynamic_field_index();
    index
        .get_fields(&dynamic_owner)
        .is_some_and(|fields| !fields.is_empty())
        && !index.get_wildcard_definitions(&dynamic_owner).is_empty()
}

fn humanize_table_const_type(
    db: &DbIndex,
    member_owned: LuaMemberOwner,
    level: RenderLevel,
) -> String {
    if TableLayout::from_level(level).is_none() {
        return "table".to_string();
    }
    humanize_table_const_type_detail_and_simple(db, member_owned, level)
        .unwrap_or("table".to_string())
}

fn humanize_merged_table_type(
    db: &DbIndex,
    merged: &LuaMergedTableType,
    level: RenderLevel,
) -> String {
    match level {
        RenderLevel::DetailedCount(_) | RenderLevel::Detailed | RenderLevel::Simple => {
            let typ = LuaType::MergedTable(merged.clone().into());
            let Some(members) = find_members(db, &typ) else {
                return "table".to_string();
            };
            humanize_member_list_as_table(db, members, level).unwrap_or("table".to_string())
        }
        _ => "table".to_string(),
    }
}

fn humanize_member_list_as_table(
    db: &DbIndex,
    members: Vec<LuaMemberInfo>,
    level: RenderLevel,
) -> Option<String> {
    let mut total_length = 0;
    let mut total_line = 0;
    let mut members_string = String::new();
    for member in members {
        let member_string = build_table_member_string(
            db,
            &member.key,
            &member.typ,
            humanize_type(db, &member.typ, level.next_level()),
            level,
        );

        match level {
            RenderLevel::DetailedCount(n) => {
                total_line += 1;
                members_string.push_str(&format!("    {},\n", member_string));
                if total_line >= n {
                    members_string.push_str("    ...\n");
                    break;
                }
            }
            RenderLevel::Detailed => {
                total_line += 1;
                members_string.push_str(&format!("    {},\n", member_string));
                if total_line >= DEFAULT_DETAIL_MEMBER_DISPLAY_COUNT {
                    members_string.push_str("    ...\n");
                    break;
                }
            }
            RenderLevel::Simple => {
                let member_string_len = member_string.chars().count();
                if total_length != 0 {
                    members_string.push_str(", ");
                    total_length += 2;
                }

                total_length += member_string_len;
                members_string.push_str(&member_string);
                if total_length > 54 {
                    members_string.push_str(", ...");
                    break;
                }
            }
            _ => return None,
        }
    }

    match level {
        RenderLevel::DetailedCount(_) | RenderLevel::Detailed => {
            Some(format!("{{\n{}}}", members_string))
        }
        RenderLevel::Simple => Some(format!("{{ {} }}", members_string)),
        _ => None,
    }
}

fn humanize_table_generic_type(
    db: &DbIndex,
    table_generic_params: &[LuaType],
    level: RenderLevel,
) -> String {
    let num = match level {
        RenderLevel::Documentation => 500,
        RenderLevel::DetailedCount(_) | RenderLevel::Detailed => 10,
        RenderLevel::Simple => 8,
        RenderLevel::Normal => 4,
        RenderLevel::Brief => 2,
        RenderLevel::Minimal => {
            return "table<...>".to_string();
        }
    };

    let dots = if table_generic_params.len() > num {
        ", ..."
    } else {
        ""
    };

    let generic_params = table_generic_params
        .iter()
        .take(num)
        .map(|ty| humanize_type(db, ty, level.next_level()))
        .collect::<Vec<_>>()
        .join(",");

    format!("table<{}{}>", generic_params, dots)
}

fn humanize_tpl_ref_type(tpl: &GenericTpl) -> String {
    tpl.get_name().to_string()
}

fn humanize_const_tpl_ref_type(const_tpl: &GenericTpl) -> String {
    const_tpl.get_name().to_string()
}

fn humanize_conditional_type(
    db: &DbIndex,
    conditional: &LuaConditionalType,
    level: RenderLevel,
) -> String {
    let check_type = humanize_type(db, conditional.get_condition(), level.next_level());
    let true_type = humanize_type(db, conditional.get_true_type(), level.next_level());
    let false_type = humanize_type(db, conditional.get_false_type(), level.next_level());

    format!("{} and {} or {}", check_type, true_type, false_type)
}

fn humanize_str_tpl_ref_type(str_tpl: &LuaStringTplType) -> String {
    let prefix = str_tpl.get_prefix();
    if prefix.is_empty() {
        str_tpl.get_name().to_string()
    } else {
        format!("{}`{}`", prefix, str_tpl.get_name())
    }
}

fn humanize_variadic_type(db: &DbIndex, multi: &VariadicType, level: RenderLevel) -> String {
    match multi {
        VariadicType::Base(base) => {
            let base_str = humanize_type(db, base, level);
            format!("{} ...", base_str)
        }
        VariadicType::Multi(types) => {
            let max_num = match level {
                RenderLevel::Documentation => 500,
                RenderLevel::DetailedCount(_) | RenderLevel::Detailed => 10,
                RenderLevel::Simple => 8,
                RenderLevel::Normal => 4,
                RenderLevel::Brief => 2,
                RenderLevel::Minimal => {
                    return "multi<...>".to_string();
                }
            };

            let dots = if types.len() > max_num { ", ..." } else { "" };
            let type_str = types
                .iter()
                .take(max_num)
                .map(|ty| humanize_type(db, ty, level.next_level()))
                .collect::<Vec<_>>()
                .join(",");
            format!("({}{})", type_str, dots)
        }
    }
}

fn humanize_instance_type(db: &DbIndex, ins: &LuaInstanceType, level: RenderLevel) -> String {
    humanize_type(db, ins.get_base(), level)
}

fn humanize_signature_type(
    db: &DbIndex,
    signature_id: &LuaSignatureId,
    level: RenderLevel,
) -> String {
    if level == RenderLevel::Minimal {
        return "fun(...) -> ...".to_string();
    }

    let signature = match db.get_signature_index().get(signature_id) {
        Some(sig) => sig,
        None => return "unknown".to_string(),
    };

    let params = signature
        .get_type_params()
        .iter()
        .map(|param| {
            let name = param.0.clone();
            if let Some(ty) = &param.1 {
                format!("{}: {}", name, humanize_type(db, ty, level.next_level()))
            } else {
                name.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");

    let generics = signature
        .generic_params
        .iter()
        .map(|generic_param| generic_param.name.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    let generic_str = if generics.is_empty() {
        "".to_string()
    } else {
        format!("<{}>", generics)
    };

    let ret_str = {
        let ret_type = signature.get_return_type();
        let return_nil = match ret_type {
            LuaType::Variadic(variadic) => matches!(variadic.get_type(0), Some(LuaType::Nil)),
            _ => ret_type.is_nil(),
        };

        if return_nil {
            "".to_string()
        } else {
            let rets = signature
                .return_docs
                .iter()
                .map(|ret| humanize_type(db, &ret.type_ref, level.next_level()))
                .collect::<Vec<_>>();
            if rets.is_empty() {
                "".to_string()
            } else {
                format!(" -> {}", rets.join(","))
            }
        }
    };

    format!("fun{}({}){}", generic_str, params, ret_str)
}

fn build_table_member_string(
    db: &DbIndex,
    member_key: &LuaMemberKey,
    ty: &LuaType,
    member_value_string: String,
    level: RenderLevel,
) -> String {
    let (member_value, separator) =
        if matches!(level, RenderLevel::DetailedCount(_) | RenderLevel::Detailed) {
            let val = match ty {
                LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_) => {
                    format!("integer = {member_value_string}")
                }
                LuaType::FloatConst(_) => format!("number = {member_value_string}"),
                LuaType::StringConst(_) | LuaType::DocStringConst(_) => {
                    format!("string = {member_value_string}")
                }
                LuaType::BooleanConst(_) => format!("boolean = {member_value_string}"),
                _ => member_value_string,
            };
            (val, ": ")
        } else {
            (member_value_string, " = ")
        };

    match member_key {
        LuaMemberKey::Name(name) => {
            format!(
                "{}{separator}{member_value}",
                humanize_member_key_name(name.as_str())
            )
        }
        LuaMemberKey::Integer(i) => format!("[{i}]{separator}{member_value}"),
        LuaMemberKey::None => member_value,
        LuaMemberKey::ExprType(LuaType::Integer) => member_value,
        LuaMemberKey::ExprType(typ) if typ.is_unknown() => {
            format!("[dynamic]{separator}{member_value}")
        }
        LuaMemberKey::ExprType(typ) => {
            let key_type = humanize_type(db, typ, level.next_level());
            format!("[{key_type}]{separator}{member_value}")
        }
    }
}

pub fn humanize_member_key_name(name: &str) -> String {
    if is_lua_identifier(name) && !is_lua_keyword(name) {
        name.to_string()
    } else {
        format!("[\"{}\"]", hover_escape_string(name))
    }
}

fn is_lua_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn is_lua_keyword(name: &str) -> bool {
    matches!(
        name,
        "and"
            | "break"
            | "do"
            | "else"
            | "elseif"
            | "end"
            | "false"
            | "for"
            | "function"
            | "goto"
            | "if"
            | "in"
            | "local"
            | "nil"
            | "not"
            | "or"
            | "repeat"
            | "return"
            | "then"
            | "true"
            | "until"
            | "while"
    )
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use googletest::prelude::*;

    use smol_str::SmolStr;

    use crate::{DbIndex, LuaMemberKey, LuaObjectType, LuaType, LuaUnionType};

    use super::{
        RenderLevel, build_table_member_string, format_union_type, humanize_member_key_name,
        humanize_type,
    };

    fn simple_type_label(ty: &LuaType) -> String {
        match ty {
            LuaType::String => "string".to_string(),
            LuaType::Number => "number".to_string(),
            LuaType::Boolean => "boolean".to_string(),
            LuaType::Function => "function".to_string(),
            LuaType::Nil => "nil".to_string(),
            _ => "other".to_string(),
        }
    }

    #[gtest]
    fn format_union_type_sorts_members_consistently() {
        let left = LuaUnionType::from_vec(vec![LuaType::String, LuaType::Number, LuaType::Boolean]);
        let right =
            LuaUnionType::from_vec(vec![LuaType::Boolean, LuaType::String, LuaType::Number]);

        let left_render =
            format_union_type(&left, RenderLevel::Detailed, |ty, _| simple_type_label(ty));
        let right_render =
            format_union_type(&right, RenderLevel::Detailed, |ty, _| simple_type_label(ty));

        expect_eq!(left_render, right_render);
        expect_eq!(left_render, "(boolean|number|string)");
    }

    #[gtest]
    fn format_union_type_keeps_nullable_suffix_with_canonical_order() {
        let union = LuaType::Union(Arc::new(LuaUnionType::from_vec(vec![
            LuaType::String,
            LuaType::Number,
            LuaType::Nil,
        ])));

        let rendered = if let LuaType::Union(inner) = union {
            format_union_type(inner.as_ref(), RenderLevel::Detailed, |ty, _| {
                simple_type_label(ty)
            })
        } else {
            unreachable!("expected union type")
        };

        expect_eq!(rendered, "(number|string)?");
    }

    #[gtest]
    fn humanize_member_key_name_uses_bare_names_only_for_valid_identifiers() {
        expect_eq!(humanize_member_key_name("valid_name1"), "valid_name1");
        expect_eq!(humanize_member_key_name("end"), "[\"end\"]");
        expect_eq!(humanize_member_key_name("not valid"), "[\"not valid\"]");
    }

    #[gtest]
    fn table_member_string_escapes_control_character_keys() {
        let db = DbIndex::default();
        let cases = [
            ("\u{07}", "\\a", r#"["\a"]: string = "\\a""#),
            ("\u{08}", "\\b", r#"["\b"]: string = "\\b""#),
            ("\u{0c}", "\\f", r#"["\f"]: string = "\\f""#),
            ("\n", "\\n", r#"["\n"]: string = "\\n""#),
            ("\r", "\\r", r#"["\r"]: string = "\\r""#),
            ("\t", "\\t", r#"["\t"]: string = "\\t""#),
            ("\u{0b}", "\\v", r#"["\v"]: string = "\\v""#),
            ("\\", "\\\\", r#"["\\"]: string = "\\\\""#),
            ("\"", "\\\"", r#"["\""]: string = "\\\"""#),
            ("'", "\\'", r#"["'"]: string = "\\'""#),
        ];

        for (key, value, expected) in cases {
            let value_type = LuaType::StringConst(SmolStr::new(value).into());
            let rendered = build_table_member_string(
                &db,
                &LuaMemberKey::Name(key.into()),
                &value_type,
                humanize_type(&db, &value_type, RenderLevel::Detailed),
                RenderLevel::Detailed,
            );
            expect_eq!(rendered, expected);
        }
    }

    #[gtest]
    fn object_type_escapes_control_character_field_names() {
        let db = DbIndex::default();
        let object = LuaType::Object(
            LuaObjectType::new_with_fields(
                HashMap::from([(
                    LuaMemberKey::Name("\n".into()),
                    LuaType::StringConst(SmolStr::new("\\n").into()),
                )]),
                Vec::new(),
            )
            .into(),
        );

        expect_eq!(
            humanize_type(&db, &object, RenderLevel::Detailed),
            r#"{ ["\n"]: "\\n" }"#
        );
    }
}
