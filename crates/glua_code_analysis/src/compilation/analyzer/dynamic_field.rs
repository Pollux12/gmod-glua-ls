use glua_parser::{LuaAst, LuaAstNode, LuaIndexKey, LuaVarExpr};
use smol_str::SmolStr;

use crate::{LuaType, LuaTypeDeclId, db_index::DbIndex, profile::Profile, semantic::infer_expr};

use super::{AnalysisPipeline, AnalyzeContext, gmod::get_gmod_class_name_for_file};

pub struct DynamicFieldAnalysisPipeline;

impl AnalysisPipeline for DynamicFieldAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        let _p = Profile::cond_new("dynamic field analyze", context.tree_list.len() > 1);
        let tree_list = context.tree_list.clone();
        let mut collected: Vec<(LuaTypeDeclId, SmolStr, crate::FileId, rowan::TextRange)> =
            Vec::new();

        for in_filed_tree in &tree_list {
            let root = in_filed_tree.value.clone();
            let file_id = in_filed_tree.file_id;
            let cache = context.infer_manager.get_infer_cache(file_id);
            // Pre-compute the gmod class for this file (if any) to avoid
            // repeated path lookups inside the inner loop.
            let file_class_type = get_gmod_class_name_for_file(&*db, file_id)
                .map(|name| LuaType::Ref(LuaTypeDeclId::global(&name)));

            for node in root.descendants::<LuaAst>() {
                let LuaAst::LuaAssignStat(assign) = node else {
                    continue;
                };
                let (vars, _) = assign.get_var_and_expr_list();
                for var in vars.iter() {
                    let LuaVarExpr::IndexExpr(index_expr) = var else {
                        continue;
                    };
                    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
                        continue;
                    };
                    let Ok(prefix_type) = infer_expr(&*db, cache, prefix_expr) else {
                        continue;
                    };
                    let Some(field_name) = get_field_name(&index_expr) else {
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
                    let effective_type = if is_unresolved_table_type(&prefix_type) {
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

        // Propagate dynamic fields to parent types so that e.g. a field assigned
        // on `base_glide` (which extends `Entity`) is also visible when the variable
        // is typed as `Entity`.  This avoids false-positive `undefined-field` when
        // user code accesses entity fields through a base-class reference.
        let mut propagated: Vec<(LuaTypeDeclId, SmolStr, crate::FileId, rowan::TextRange)> =
            Vec::new();
        for (type_id, field_name, file_id, range) in &collected {
            let mut super_types = Vec::new();
            type_id.collect_super_types(&*db, &mut super_types);
            for super_type in super_types {
                if let LuaType::Ref(super_id) = &super_type {
                    propagated.push((super_id.clone(), field_name.clone(), *file_id, *range));
                }
            }
        }

        let index = db.get_dynamic_field_index_mut();
        for (type_id, field_name, file_id, range) in &collected {
            index.add_field(type_id.clone(), field_name.clone(), *file_id, *range);
        }
        for (type_id, field_name, file_id, range) in &propagated {
            index.add_field(type_id.clone(), field_name.clone(), *file_id, *range);
        }
    }
}

fn get_field_name(index_expr: &glua_parser::LuaIndexExpr) -> Option<SmolStr> {
    let key = index_expr.get_index_key()?;
    match key {
        LuaIndexKey::Name(name) => Some(name.get_name_text().into()),
        LuaIndexKey::String(s) => Some(s.get_value().into()),
        _ => None,
    }
}

/// Returns true when the type is a generic/unresolved table type that
/// does not carry useful class information.  Matches:
/// - `table` (the bare Ref/Def type)
/// - `table|nil`
/// - `TableConst` (inferred from `return {}` or similar)
fn is_unresolved_table_type(typ: &LuaType) -> bool {
    match typ {
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
    result: &mut Vec<(LuaTypeDeclId, SmolStr, crate::FileId, rowan::TextRange)>,
) {
    match typ {
        LuaType::Ref(id) | LuaType::Def(id) => {
            result.push((id.clone(), field_name.clone(), file_id, range));
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
