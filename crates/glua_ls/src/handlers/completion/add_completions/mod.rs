mod add_decl_completion;
mod add_member_completion;
mod check_match_word;
mod completion_item_info;

pub use add_decl_completion::add_decl_completion;
pub use add_member_completion::get_index_alias_name;
pub use add_member_completion::{
    CompletionTriggerStatus, add_member_completion_with_description_hint,
};
pub use check_match_word::check_match_word;
pub(crate) use completion_item_info::{
    color_info_from_expr, color_info_from_type, color_label_detail, color_preview_documentation,
    is_color_type,
};
use glua_code_analysis::{
    GlobalId, LuaDeclId, LuaMemberOwner, LuaSemanticDeclId, LuaType, LuaUnionType, RenderLevel,
};
use lsp_types::{CompletionItemKind, CompletionItemTag};

use glua_code_analysis::humanize_type;

use super::completion_builder::CompletionBuilder;

pub fn check_visibility(builder: &mut CompletionBuilder, id: LuaSemanticDeclId) -> Option<()> {
    match id {
        LuaSemanticDeclId::Member(_) => {}
        LuaSemanticDeclId::LuaDecl(_) => {}
        _ => return Some(()),
    }

    if !builder
        .semantic_model
        .is_semantic_visible(builder.trigger_token.clone(), id)
    {
        return None;
    }

    Some(())
}

pub fn get_completion_kind(typ: &LuaType) -> CompletionItemKind {
    match typ {
        LuaType::DocFunction(_) | LuaType::Function | LuaType::Signature(_) => {
            CompletionItemKind::FUNCTION
        }
        LuaType::BooleanConst(_)
        | LuaType::DocBooleanConst(_)
        | LuaType::StringConst(_)
        | LuaType::DocStringConst(_)
        | LuaType::IntegerConst(_)
        | LuaType::DocIntegerConst(_)
        | LuaType::FloatConst(_) => CompletionItemKind::CONSTANT,
        LuaType::Def(_) => CompletionItemKind::CLASS,
        LuaType::Namespace(_) | LuaType::ModuleRef(_) => CompletionItemKind::MODULE,
        LuaType::Table
        | LuaType::TableConst(_)
        | LuaType::MergedTable(_)
        | LuaType::Array(_)
        | LuaType::Tuple(_)
        | LuaType::Object(_)
        | LuaType::TableGeneric(_)
        | LuaType::TableOf(_)
        | LuaType::Ref(_)
        | LuaType::Instance(_)
        | LuaType::Global => CompletionItemKind::STRUCT,
        LuaType::Boolean
        | LuaType::String
        | LuaType::Integer
        | LuaType::Number
        | LuaType::Language(_)
        | LuaType::Userdata
        | LuaType::Thread
        | LuaType::Io => CompletionItemKind::VALUE,
        LuaType::Never => CompletionItemKind::UNIT,
        LuaType::TplRef(_) | LuaType::StrTplRef(_) | LuaType::ConstTplRef(_) => {
            CompletionItemKind::TYPE_PARAMETER
        }
        LuaType::Union(union) => get_union_completion_kind(union.as_ref()),
        LuaType::Intersection(intersection) => get_intersection_completion_kind(intersection),
        LuaType::TypeGuard(inner) => get_completion_kind(inner),
        LuaType::Variadic(variadic) => variadic
            .get_type(0)
            .map(get_completion_kind)
            .unwrap_or(CompletionItemKind::VARIABLE),
        LuaType::MultiLineUnion(_)
        | LuaType::Generic(_)
        | LuaType::Call(_)
        | LuaType::DocAttribute(_)
        | LuaType::Conditional(_)
        | LuaType::ConditionalInfer(_)
        | LuaType::Mapped(_)
        | LuaType::Any
        | LuaType::Unknown
        | LuaType::Nil
        | LuaType::SelfInfer => CompletionItemKind::VARIABLE,
    }
}

pub fn get_decl_completion_kind(
    builder: &CompletionBuilder,
    decl_id: LuaDeclId,
    typ: &LuaType,
) -> CompletionItemKind {
    if is_global_table_namespace_decl(builder, decl_id, typ) {
        CompletionItemKind::CLASS
    } else {
        get_completion_kind(typ)
    }
}

fn get_intersection_completion_kind(
    intersection: &glua_code_analysis::LuaIntersectionType,
) -> CompletionItemKind {
    let mut fallback = CompletionItemKind::VARIABLE;
    for kind in intersection.get_types().iter().map(get_completion_kind) {
        if kind == CompletionItemKind::FUNCTION {
            return CompletionItemKind::FUNCTION;
        }
        if fallback == CompletionItemKind::VARIABLE && kind != CompletionItemKind::VARIABLE {
            fallback = kind;
        }
    }

    fallback
}

pub fn is_table_namespace_type(typ: &LuaType) -> bool {
    match typ {
        LuaType::Table
        | LuaType::TableConst(_)
        | LuaType::MergedTable(_)
        | LuaType::TableGeneric(_)
        | LuaType::TableOf(_)
        | LuaType::Object(_)
        | LuaType::Global => true,
        LuaType::Union(union) => match union.as_ref() {
            LuaUnionType::Nullable(typ) => is_table_namespace_type(typ),
            LuaUnionType::Multi(types) => {
                let mut non_nil_types = types.iter().filter(|typ| !matches!(typ, LuaType::Nil));
                non_nil_types.next().is_some_and(is_table_namespace_type)
                    && non_nil_types.all(is_table_namespace_type)
            }
        },
        LuaType::TypeGuard(inner) => is_table_namespace_type(inner),
        _ => false,
    }
}

fn is_global_table_namespace_decl(
    builder: &CompletionBuilder,
    decl_id: LuaDeclId,
    typ: &LuaType,
) -> bool {
    if !is_table_namespace_type(typ) {
        return false;
    }

    let db = builder.semantic_model.get_db();
    let Some(decl) = db.get_decl_index().get_decl(&decl_id) else {
        return false;
    };

    decl.is_global()
        && db
            .get_member_index()
            .get_member_len(&LuaMemberOwner::GlobalPath(GlobalId::new(decl.get_name())))
            > 0
}

pub fn get_completion_tags(
    builder: &CompletionBuilder,
    deprecated: Option<bool>,
) -> Option<Vec<CompletionItemTag>> {
    (deprecated.unwrap_or(false) && builder.supports_deprecated_completion_tags())
        .then_some(vec![CompletionItemTag::DEPRECATED])
}

fn get_union_completion_kind(union: &LuaUnionType) -> CompletionItemKind {
    let kinds = match union {
        LuaUnionType::Nullable(typ) => return get_completion_kind(typ),
        LuaUnionType::Multi(types) => types
            .iter()
            .filter(|typ| !matches!(typ, LuaType::Nil))
            .map(get_completion_kind)
            .collect::<Vec<_>>(),
    };

    let Some(first) = kinds.first().copied() else {
        return CompletionItemKind::UNIT;
    };

    if kinds.iter().all(|kind| *kind == first) {
        first
    } else if kinds.iter().all(|kind| {
        matches!(
            *kind,
            CompletionItemKind::CONSTANT | CompletionItemKind::VALUE
        )
    }) {
        CompletionItemKind::VALUE
    } else {
        CompletionItemKind::VARIABLE
    }
}

pub fn is_deprecated(builder: &CompletionBuilder, id: LuaSemanticDeclId) -> bool {
    let property = builder
        .semantic_model
        .get_db()
        .get_property_index()
        .get_property(&id);

    if let Some(property) = property
        && property.deprecated().is_some()
    {
        return true;
    }

    false
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CallDisplay {
    None,
    AddSelf,
    RemoveFirst,
}

pub fn get_detail(
    builder: &CompletionBuilder,
    typ: &LuaType,
    display: CallDisplay,
) -> Option<String> {
    match typ {
        LuaType::Signature(signature_id) => {
            let signature = builder
                .semantic_model
                .get_db()
                .get_signature_index()
                .get(signature_id)?;

            let mut params_str = signature
                .get_type_params()
                .iter()
                .map(|param| param.0.clone())
                .collect::<Vec<_>>();

            match display {
                CallDisplay::AddSelf => {
                    params_str.insert(0, "self".to_string());
                }
                CallDisplay::RemoveFirst => {
                    if !params_str.is_empty() {
                        params_str.remove(0);
                    }
                }
                _ => {}
            }
            let rets = &signature.return_docs;
            let rets_detail = if rets.len() == 1 {
                let detail = humanize_type(
                    builder.semantic_model.get_db(),
                    &rets[0].type_ref,
                    RenderLevel::Minimal,
                );
                format!(" -> {}", detail)
            } else if rets.len() > 1 {
                let detail = humanize_type(
                    builder.semantic_model.get_db(),
                    &rets[0].type_ref,
                    RenderLevel::Minimal,
                );
                format!(" -> {} ...", detail)
            } else {
                "".to_string()
            };

            Some(format!("({}){}", params_str.join(", "), rets_detail))
        }
        LuaType::DocFunction(f) => {
            let mut params_str = f
                .get_params()
                .iter()
                .map(|param| param.0.clone())
                .collect::<Vec<_>>();

            match display {
                CallDisplay::AddSelf => {
                    params_str.insert(0, "self".to_string());
                }
                CallDisplay::RemoveFirst => {
                    if !params_str.is_empty() {
                        params_str.remove(0);
                    }
                }
                _ => {}
            }
            let ret_type = f.get_ret();
            let rets_detail = match ret_type {
                LuaType::Nil => "".to_string(),
                _ => {
                    let type_detail = humanize_type(
                        builder.semantic_model.get_db(),
                        ret_type,
                        RenderLevel::Minimal,
                    );
                    format!("-> {}", type_detail)
                }
            };
            Some(format!("({}){}", params_str.join(", "), rets_detail))
        }
        _ => None,
    }
}

pub fn get_function_snippet(
    builder: &CompletionBuilder,
    label: &str,
    typ: &LuaType,
    display: CallDisplay,
) -> Option<String> {
    match typ {
        LuaType::Signature(signature_id) => {
            let signature = builder
                .semantic_model
                .get_db()
                .get_signature_index()
                .get(signature_id)?;

            let mut params_str = signature
                .get_type_params()
                .iter()
                .map(|param| param.0.clone())
                .collect::<Vec<_>>();

            match display {
                CallDisplay::AddSelf => {
                    params_str.insert(0, "self".to_string());
                }
                CallDisplay::RemoveFirst => {
                    if !params_str.is_empty() {
                        params_str.remove(0);
                    }
                }
                _ => {}
            }

            Some(format!(
                "{}({})",
                label,
                params_str
                    .iter()
                    .enumerate()
                    .map(|(i, name)| format!("${{{}:{}}}", i + 1, name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
        LuaType::DocFunction(f) => {
            let mut params_str = f
                .get_params()
                .iter()
                .map(|param| param.0.clone())
                .collect::<Vec<_>>();

            match display {
                CallDisplay::AddSelf => {
                    params_str.insert(0, "self".to_string());
                }
                CallDisplay::RemoveFirst => {
                    if !params_str.is_empty() {
                        params_str.remove(0);
                    }
                }
                _ => {}
            }

            Some(format!(
                "{}({})",
                label,
                params_str
                    .iter()
                    .enumerate()
                    .map(|(i, name)| format!("${{{}:{}}}", i + 1, name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
        _ => None,
    }
}

#[allow(unused)]
fn truncate_with_ellipsis(s: &str, max_len: usize) -> String {
    if s.chars().count() > max_len {
        let truncated: String = s.chars().take(max_len).collect();
        format!("   {}...", truncated)
    } else {
        format!("   {}", s)
    }
}

fn get_description(builder: &CompletionBuilder, typ: &LuaType) -> Option<String> {
    match typ {
        LuaType::Signature(_) => None,
        LuaType::DocFunction(_) => None,
        _ if typ.is_unknown() => None,
        _ => Some(humanize_type(
            builder.semantic_model.get_db(),
            typ,
            RenderLevel::Minimal,
        )),
    }
}
