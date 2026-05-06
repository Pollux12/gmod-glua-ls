use std::collections::HashMap;

use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaCallExpr, LuaExpr, LuaFuncStat, LuaIndexKey, LuaSyntaxKind,
    LuaVarExpr,
};
use smol_str::SmolStr;

use crate::{
    LuaMemberKey, LuaType, LuaTypeDeclId,
    db_index::{DbIndex, DynamicFieldOwner},
    profile::Profile,
    semantic::{
        find_members_with_key, get_var_expr_var_ref_id, infer_expr, unwrap_paren_to_name_expr,
    },
};

use super::{AnalysisPipeline, AnalyzeContext, gmod::get_gmod_class_name_for_file};

pub struct DynamicFieldAnalysisPipeline;

impl AnalysisPipeline for DynamicFieldAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        let _p = Profile::cond_new("dynamic field analyze", context.tree_list.len() > 1);
        let tree_list = context.tree_list.clone();
        let mut collected: Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)> =
            Vec::new();

        for in_filed_tree in &tree_list {
            let root = in_filed_tree.value.clone();
            let file_id = in_filed_tree.file_id;
            let cache = context.infer_manager.get_infer_cache(file_id);
            let mut prefix_type_cache: HashMap<rowan::TextRange, Option<LuaType>> = HashMap::new();
            // Pre-compute the gmod class for this file (if any) to avoid
            // repeated path lookups inside the inner loop.
            let file_class_type = get_gmod_class_name_for_file(&*db, file_id)
                .map(|name| LuaType::Ref(LuaTypeDeclId::global(&name)));

            for assign in root.descendants::<LuaAssignStat>() {
                let (vars, _) = assign.get_var_and_expr_list();
                for var in vars.iter() {
                    let LuaVarExpr::IndexExpr(index_expr) = var else {
                        continue;
                    };
                    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
                        continue;
                    };

                    let prefix_range = prefix_expr.syntax().text_range();
                    let prefix_type =
                        if let Some(cached_type) = prefix_type_cache.get(&prefix_range) {
                            match cached_type {
                                Some(prefix_type) => prefix_type.clone(),
                                None => continue,
                            }
                        } else {
                            let inferred = infer_expr(&*db, cache, prefix_expr.clone()).ok();
                            prefix_type_cache.insert(prefix_range, inferred.clone());
                            let Some(prefix_type) = inferred else {
                                continue;
                            };
                            prefix_type
                        };
                    let field_names = get_field_names(db, cache, &index_expr);
                    if field_names.is_empty() {
                        continue;
                    };

                    // When the prefix resolves to a generic table type (e.g.
                    // from Entity:GetTable()) and the file belongs to a gmod
                    // scripted class, index under the class type so that
                    // `self.field` accesses find these dynamic fields.
                    //
                    // Also handles TableOf(SelfInfer) — when SelfInfer couldn't
                    // be resolved during compilation (e.g. localized GetTable),
                    // we fall back to the file's class type.
                    let effective_type = if let Some(metatable_type) =
                        infer_setmetatable_target_type(
                            &*db,
                            cache,
                            &prefix_expr,
                            index_expr.get_range(),
                        ) {
                        metatable_type
                    } else if is_unresolved_table_type(&prefix_type) {
                        if let Some(class_type) = &file_class_type {
                            class_type.clone()
                        } else {
                            prefix_type
                        }
                    } else if has_unresolved_self_infer(&prefix_type) {
                        if let Some(class_type) = &file_class_type {
                            replace_self_infer(&prefix_type, class_type)
                        } else {
                            prefix_type
                        }
                    } else {
                        prefix_type
                    };

                    for field_name in field_names {
                        collect_for_type(
                            &effective_type,
                            &field_name,
                            file_id,
                            index_expr.get_range(),
                            &mut collected,
                        );
                    }
                }
            }
        }

        // Propagate dynamic fields to parent types so that e.g. a field assigned
        // on `base_glide` (which extends `Entity`) is also visible when the variable
        // is typed as `Entity`.  This avoids false-positive `undefined-field` when
        // user code accesses entity fields through a base-class reference.
        let mut propagated: Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)> =
            Vec::new();
        for (owner, field_name, file_id, range) in &collected {
            let DynamicFieldOwner::Type(type_id) = owner else {
                continue;
            };
            let mut super_types = Vec::new();
            type_id.collect_super_types(&*db, &mut super_types);
            for super_type in super_types {
                if let LuaType::Ref(super_id) = &super_type {
                    propagated.push((
                        DynamicFieldOwner::Type(super_id.clone()),
                        field_name.clone(),
                        *file_id,
                        *range,
                    ));
                }
            }
        }

        let index = db.get_dynamic_field_index_mut();
        for (owner, field_name, file_id, range) in &collected {
            index.add_field(owner.clone(), field_name.clone(), *file_id, *range);
        }
        for (owner, field_name, file_id, range) in &propagated {
            index.add_field(owner.clone(), field_name.clone(), *file_id, *range);
        }
    }
}

fn infer_setmetatable_target_type(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    prefix_expr: &LuaExpr,
    assignment_range: rowan::TextRange,
) -> Option<LuaType> {
    let prefix_var_ref_id = get_var_expr_var_ref_id(db, cache, prefix_expr.clone())?;
    let scope = prefix_expr.syntax().ancestors().find(|node| {
        matches!(
            node.kind().into(),
            LuaSyntaxKind::ClosureExpr | LuaSyntaxKind::FuncStat | LuaSyntaxKind::LocalFuncStat
        )
    })?;

    let mut matched_type = None;
    for node in scope.descendants() {
        let Some(call_expr) = LuaCallExpr::cast(node) else {
            continue;
        };
        let Some(call_scope) = call_expr.syntax().ancestors().find(|node| {
            matches!(
                node.kind().into(),
                LuaSyntaxKind::ClosureExpr | LuaSyntaxKind::FuncStat | LuaSyntaxKind::LocalFuncStat
            )
        }) else {
            continue;
        };
        if call_scope != scope {
            continue;
        }

        if !call_expr.is_setmetatable() || call_expr.get_range().end() > assignment_range.start() {
            continue;
        }

        let Some(arg_list) = call_expr.get_args_list() else {
            continue;
        };
        let args = arg_list.get_args().collect::<Vec<_>>();
        if args.len() != 2 {
            continue;
        }

        let Some(target_var_ref_id) = get_var_expr_var_ref_id(db, cache, args[0].clone()) else {
            continue;
        };
        if target_var_ref_id != prefix_var_ref_id {
            continue;
        }

        if let Some(target_type) = infer_metatable_index_type_for_dynamic_field(db, cache, &args[1])
        {
            matched_type = Some(target_type);
        }
    }

    matched_type
}

fn infer_metatable_index_type_for_dynamic_field(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    metatable_expr: &LuaExpr,
) -> Option<LuaType> {
    if let Some(name_expr) = unwrap_paren_to_name_expr(metatable_expr)
        && name_expr.get_name_text().as_deref() == Some("self")
        && let Some(self_type) = infer_enclosing_method_self_type(db, cache, metatable_expr)
        && let Some(index_type) = infer_index_type_from_metatable_type(db, &self_type)
    {
        return Some(index_type);
    }

    if let LuaExpr::TableExpr(table) = metatable_expr {
        for field in table.get_fields() {
            let Some(field_key) = field.get_field_key() else {
                continue;
            };
            let field_name = match field_key {
                LuaIndexKey::Name(name) => name.get_name_text().to_string(),
                LuaIndexKey::String(string) => string.get_value(),
                _ => continue,
            };
            if field_name != "__index" {
                continue;
            }

            let Some(field_value) = field.get_value_expr() else {
                continue;
            };
            let Ok(index_type) = infer_expr(db, cache, field_value) else {
                continue;
            };
            if is_supported_metatable_index_type(&index_type) {
                return Some(index_type);
            }
        }

        return None;
    }

    let metatable_type = infer_expr(db, cache, metatable_expr.clone()).ok()?;
    infer_index_type_from_metatable_type(db, &metatable_type)
}

fn infer_enclosing_method_self_type(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    metatable_expr: &LuaExpr,
) -> Option<LuaType> {
    let func_stat = metatable_expr
        .syntax()
        .ancestors()
        .find_map(LuaFuncStat::cast)?;
    let func_name = func_stat.get_func_name()?;
    let LuaVarExpr::IndexExpr(index_expr) = func_name else {
        return None;
    };
    let prefix_expr = index_expr.get_prefix_expr()?;
    infer_expr(db, cache, prefix_expr).ok()
}

fn infer_index_type_from_metatable_type(db: &DbIndex, metatable_type: &LuaType) -> Option<LuaType> {
    let index_members = find_members_with_key(
        db,
        metatable_type,
        LuaMemberKey::Name("__index".into()),
        false,
    )?;

    index_members
        .into_iter()
        .find_map(|member| is_supported_metatable_index_type(&member.typ).then_some(member.typ))
}

fn is_supported_metatable_index_type(typ: &LuaType) -> bool {
    match typ {
        LuaType::Union(union) => union
            .into_vec()
            .iter()
            .any(is_supported_metatable_index_type),
        LuaType::TypeGuard(inner) => is_supported_metatable_index_type(inner),
        LuaType::Instance(instance) => is_supported_metatable_index_type(instance.get_base()),
        _ => typ.is_table() || typ.is_custom_type() || typ.is_object(),
    }
}

fn get_field_names(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    index_expr: &glua_parser::LuaIndexExpr,
) -> Vec<SmolStr> {
    let Some(key) = index_expr.get_index_key() else {
        return Vec::new();
    };
    match key {
        LuaIndexKey::Name(name) => vec![name.get_name_text().into()],
        LuaIndexKey::String(s) => vec![s.get_value().into()],
        LuaIndexKey::Expr(expr) => string_const_names(&infer_expr(db, cache, expr).ok()),
        _ => Vec::new(),
    }
}

fn string_const_names(typ: &Option<LuaType>) -> Vec<SmolStr> {
    match typ {
        Some(LuaType::StringConst(name)) | Some(LuaType::DocStringConst(name)) => {
            vec![name.as_ref().clone()]
        }
        Some(LuaType::Union(union_type)) => union_type
            .into_vec()
            .iter()
            .flat_map(|typ| string_const_names(&Some(typ.clone())))
            .collect(),
        _ => Vec::new(),
    }
}

/// Returns true when the type is a generic/unresolved table type that
/// does not carry useful class information.  Matches:
/// - `table` (the bare Ref/Def type)
/// - `table|nil`
/// - `TableConst` (inferred from `return {}` or similar)
fn is_unresolved_table_type(typ: &LuaType) -> bool {
    match typ {
        LuaType::Table => true,
        LuaType::Ref(id) | LuaType::Def(id) => id.get_name() == "table",
        LuaType::TableConst(_) => true,
        LuaType::Union(union_type) => {
            let types = union_type.into_vec();
            types.iter().any(is_unresolved_table_type)
                && types
                    .iter()
                    .all(|t| is_unresolved_table_type(t) || matches!(t, LuaType::Nil))
        }
        _ => false,
    }
}

/// Returns true when the type contains an unresolved SelfInfer,
/// e.g. `TableOf(SelfInfer)` from an unresolved localized GetTable call.
fn has_unresolved_self_infer(typ: &LuaType) -> bool {
    match typ {
        LuaType::SelfInfer => true,
        LuaType::TableOf(inner) => has_unresolved_self_infer(inner),
        LuaType::Union(union_type) => union_type.into_vec().iter().any(has_unresolved_self_infer),
        _ => false,
    }
}

/// Replaces SelfInfer in a type with the given class type.
fn replace_self_infer(typ: &LuaType, class_type: &LuaType) -> LuaType {
    match typ {
        LuaType::SelfInfer => class_type.clone(),
        LuaType::TableOf(inner) => {
            LuaType::TableOf(Box::new(replace_self_infer(inner, class_type)))
        }
        LuaType::Union(union_type) => {
            let new_types: Vec<LuaType> = union_type
                .into_vec()
                .iter()
                .map(|t| replace_self_infer(t, class_type))
                .collect();
            LuaType::from_vec(new_types)
        }
        _ => typ.clone(),
    }
}

fn collect_for_type(
    typ: &LuaType,
    field_name: &SmolStr,
    file_id: crate::FileId,
    range: rowan::TextRange,
    result: &mut Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)>,
) {
    match typ {
        LuaType::Ref(id) | LuaType::Def(id) => {
            result.push((
                DynamicFieldOwner::Type(id.clone()),
                field_name.clone(),
                file_id,
                range,
            ));
        }
        LuaType::TableConst(table_range) => {
            result.push((
                DynamicFieldOwner::Table(table_range.clone()),
                field_name.clone(),
                file_id,
                range,
            ));
        }
        LuaType::Instance(instance) => {
            collect_for_type(instance.get_base(), field_name, file_id, range, result);
        }
        LuaType::TableOf(inner) => {
            collect_for_type(inner, field_name, file_id, range, result);
        }
        LuaType::Union(union_type) => {
            for t in union_type.into_vec() {
                collect_for_type(&t, field_name, file_id, range, result);
            }
        }
        _ => {}
    }
}
