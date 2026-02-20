use emmylua_parser::{LuaAst, LuaAstNode, LuaIndexKey, LuaVarExpr};
use smol_str::SmolStr;

use crate::{LuaType, LuaTypeDeclId, db_index::DbIndex, profile::Profile, semantic::infer_expr};

use super::{AnalysisPipeline, AnalyzeContext};

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
                    collect_for_type(
                        &prefix_type,
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
        for (type_id, field_name, file_id, range) in collected {
            index.add_field(type_id, field_name, file_id, range);
        }
        for (type_id, field_name, file_id, range) in propagated {
            index.add_field(type_id, field_name, file_id, range);
        }
    }
}

fn get_field_name(index_expr: &emmylua_parser::LuaIndexExpr) -> Option<SmolStr> {
    let key = index_expr.get_index_key()?;
    match key {
        LuaIndexKey::Name(name) => Some(name.get_name_text().into()),
        LuaIndexKey::String(s) => Some(s.get_value().into()),
        _ => None,
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
        LuaType::Union(union_type) => {
            for t in union_type.into_vec() {
                collect_for_type(&t, field_name, file_id, range, result);
            }
        }
        _ => {}
    }
}
