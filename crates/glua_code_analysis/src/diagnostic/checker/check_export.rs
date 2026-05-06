use std::collections::{HashMap, HashSet};

use glua_parser::{
    LuaAst, LuaAstNode, LuaCallExpr, LuaExpr, LuaIndexExpr, LuaSyntaxKind, LuaVarExpr,
};

use crate::{
    DiagnosticCode, FileId, LuaMemberId, LuaMemberOwner, LuaType, ModuleInfo, SemanticModel,
    parse_require_module_info,
};

use super::{Checker, DiagnosticContext, check_field, humanize_lint_type};

pub struct CheckExportChecker;

type ExportedKeyCache = HashMap<FileId, HashSet<String>>;

impl Checker for CheckExportChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::InjectField, DiagnosticCode::UndefinedField];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let root = semantic_model.get_root().clone();
        let mut checked_index_expr = HashSet::new();
        let mut exported_key_cache: ExportedKeyCache = HashMap::new();
        for node in root.descendants::<LuaAst>() {
            if context.is_cancelled() {
                return;
            }
            match node {
                LuaAst::LuaAssignStat(assign) => {
                    let (vars, _) = assign.get_var_and_expr_list();
                    for var in vars.iter() {
                        if let LuaVarExpr::IndexExpr(index_expr) = var {
                            checked_index_expr.insert(index_expr.syntax().clone());
                            check_export_index_expr(
                                context,
                                semantic_model,
                                index_expr,
                                DiagnosticCode::InjectField,
                                &mut exported_key_cache,
                            );
                        }
                    }
                }
                LuaAst::LuaIndexExpr(index_expr) => {
                    if checked_index_expr.contains(index_expr.syntax()) {
                        continue;
                    }
                    check_export_index_expr(
                        context,
                        semantic_model,
                        &index_expr,
                        DiagnosticCode::UndefinedField,
                        &mut exported_key_cache,
                    );
                }
                _ => {}
            }
        }
    }
}

fn check_export_index_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    index_expr: &LuaIndexExpr,
    code: DiagnosticCode,
    exported_key_cache: &mut ExportedKeyCache,
) -> Option<()> {
    let db = context.db;
    let prefix_expr = index_expr.get_prefix_expr()?;
    if !matches!(prefix_expr, LuaExpr::NameExpr(_) | LuaExpr::CallExpr(_)) {
        return Some(());
    }
    let index_key = index_expr.get_index_key()?;

    if let Some(module_info) = check_require_table_const_with_export(semantic_model, &prefix_expr) {
        let fallback_export_typ;
        let export_typ = if let Some(export_typ) = module_info.export_type.as_ref() {
            export_typ
        } else {
            fallback_export_typ = LuaType::ModuleRef(module_info.file_id);
            &fallback_export_typ
        };

        let has_member = has_export_member(
            semantic_model,
            module_info,
            &index_key,
            LuaMemberId::new(index_expr.get_syntax_id(), semantic_model.get_file_id()),
            exported_key_cache,
        );
        if has_member {
            return Some(());
        }

        let index_name = index_key.get_path_part();
        match code {
            DiagnosticCode::InjectField => {
                context.add_diagnostic(
                    DiagnosticCode::InjectField,
                    index_key.get_range()?,
                    t!(
                        "Fields cannot be injected into the reference of `%{class}` for `%{field}`. ",
                        class = humanize_lint_type(db, export_typ),
                        field = index_name,
                    )
                    .to_string(),
                    None,
                );
            }
            DiagnosticCode::UndefinedField => {
                context.add_diagnostic(
                    DiagnosticCode::UndefinedField,
                    index_key.get_range()?,
                    t!("Undefined field `%{field}`. ", field = index_name,).to_string(),
                    None,
                );
            }
            _ => {}
        }

        return Some(());
    }

    let LuaExpr::NameExpr(name_expr) = &prefix_expr else {
        return Some(());
    };

    let decl_id = semantic_model
        .get_db()
        .get_reference_index()
        .get_local_reference(&semantic_model.get_file_id())
        .and_then(|file_ref| file_ref.get_decl_id(&name_expr.get_range()))?;

    let decl = semantic_model
        .get_db()
        .get_decl_index()
        .get_decl(&decl_id)?;
    if !decl.is_local() {
        return Some(());
    }
    // 且该声明标记了 `export`
    let property = semantic_model
        .get_db()
        .get_property_index()
        .get_property(&decl_id.into())?;
    if property.export().is_none() {
        return Some(());
    }

    let prefix_typ = semantic_model.infer_expr(prefix_expr.clone()).ok()?;
    let LuaType::TableConst(table_const) = &prefix_typ else {
        return Some(());
    };

    // 不是导入表, 且定义位于当前文件中, 则尝试检查本地表.
    if code != DiagnosticCode::UndefinedField && table_const.file_id != semantic_model.get_file_id()
    {
        return Some(());
    }

    if check_field::is_valid_member(
        context,
        semantic_model,
        &prefix_typ,
        index_expr,
        &index_key,
        code,
    )
    .is_some()
    {
        return Some(());
    }

    let index_name = index_key.get_path_part();
    context.add_diagnostic(
        DiagnosticCode::UndefinedField,
        index_key.get_range()?,
        t!("Undefined field `%{field}`. ", field = index_name,).to_string(),
        None,
    );

    Some(())
}

fn has_export_member(
    semantic_model: &SemanticModel,
    module_info: &ModuleInfo,
    index_key: &glua_parser::LuaIndexKey,
    current_member_id: LuaMemberId,
    exported_key_cache: &mut ExportedKeyCache,
) -> bool {
    let Some(member_key) = semantic_model.get_member_key(index_key) else {
        return false;
    };
    let member_key_path = member_key.to_path();

    let db = semantic_model.get_db();
    let Some(export_type) = module_info.export_type.as_ref() else {
        return false;
    };
    let owner = match export_type {
        LuaType::TableConst(table_id) => Some(LuaMemberOwner::Element(table_id.clone())),
        LuaType::Instance(instance) => Some(LuaMemberOwner::Element(instance.get_range().clone())),
        LuaType::Ref(type_id) | LuaType::Def(type_id) => {
            Some(LuaMemberOwner::Type(type_id.clone()))
        }
        _ => None,
    };

    owner.is_some_and(|owner| {
        let member_index = db.get_member_index();
        let current_owner_ids = member_index
            .get_members_for_owner_key(&owner, &member_key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        let owner_item_ids = member_index
            .get_member_item(&owner, &member_key)
            .map(|item| item.get_member_ids())
            .unwrap_or_default();

        if current_owner_ids
            .iter()
            .copied()
            .any(|member_id| member_id != current_member_id)
        {
            return true;
        }

        if owner_item_ids
            .into_iter()
            .any(|member_id| member_id != current_member_id)
        {
            return true;
        }

        // Some imported exports update owner-key indexes with only the current write site.
        // Fallback to a single-pass module scan to detect keys explicitly declared by export source.
        module_source_declares_exported_key(
            semantic_model,
            module_info,
            &member_key_path,
            exported_key_cache,
        )
    })
}

fn module_source_declares_exported_key(
    semantic_model: &SemanticModel,
    module_info: &ModuleInfo,
    key_path: &str,
    exported_key_cache: &mut ExportedKeyCache,
) -> bool {
    if let Some(keys) = exported_key_cache.get(&module_info.file_id) {
        return keys.contains(key_path);
    }

    let db = semantic_model.get_db();
    let Some(module_root) = db
        .get_vfs()
        .get_syntax_tree(&module_info.file_id)
        .map(|tree| tree.get_red_root())
    else {
        return false;
    };

    let mut exported_local_names = HashSet::new();
    let mut exported_keys = HashSet::new();
    let mut local_table_init_keys: HashMap<String, HashSet<String>> = HashMap::new();
    let mut local_assigned_keys: HashMap<String, HashSet<String>> = HashMap::new();

    for node in module_root.descendants().filter_map(LuaAst::cast) {
        match node {
            LuaAst::LuaReturnStat(return_stat) => {
                if return_stat
                    .ancestors::<glua_parser::LuaClosureExpr>()
                    .next()
                    .is_some()
                {
                    continue;
                }

                let Some(first_expr) = return_stat.get_expr_list().next() else {
                    continue;
                };

                match first_expr {
                    LuaExpr::TableExpr(table_expr) => {
                        exported_keys.extend(
                            table_expr
                                .get_fields()
                                .filter_map(|field| field.get_field_key())
                                .map(|key| key.get_path_part()),
                        );
                    }
                    LuaExpr::NameExpr(name_expr) => {
                        if let Some(name) = name_expr.get_name_text() {
                            exported_local_names.insert(name);
                        }
                    }
                    _ => {}
                }
            }
            LuaAst::LuaLocalStat(local_stat) => {
                let local_names = local_stat.get_local_name_list().collect::<Vec<_>>();
                let value_exprs = local_stat.get_value_exprs().collect::<Vec<_>>();
                for (idx, local_name) in local_names.iter().enumerate() {
                    let Some(name_token) = local_name.get_name_token() else {
                        continue;
                    };
                    let Some(value_expr) = value_exprs.get(idx) else {
                        continue;
                    };
                    let LuaExpr::TableExpr(table_expr) = value_expr.clone() else {
                        continue;
                    };

                    let keys = table_expr
                        .get_fields()
                        .filter_map(|field| field.get_field_key())
                        .map(|key| key.get_path_part())
                        .collect::<HashSet<_>>();
                    local_table_init_keys
                        .entry(name_token.get_name_text().to_string())
                        .or_default()
                        .extend(keys);
                }
            }
            LuaAst::LuaAssignStat(assign_stat) => {
                let (vars, _) = assign_stat.get_var_and_expr_list();
                for var in vars {
                    let LuaVarExpr::IndexExpr(index_expr) = var else {
                        continue;
                    };
                    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
                        continue;
                    };
                    let LuaExpr::NameExpr(prefix_name) = prefix_expr else {
                        continue;
                    };
                    let Some(prefix_name_text) = prefix_name.get_name_text() else {
                        continue;
                    };
                    let Some(index_key) = index_expr.get_index_key() else {
                        continue;
                    };

                    local_assigned_keys
                        .entry(prefix_name_text)
                        .or_default()
                        .insert(index_key.get_path_part());
                }
            }
            LuaAst::LuaFuncStat(func_stat) => {
                let Some(func_name) = func_stat.get_func_name() else {
                    continue;
                };
                let LuaVarExpr::IndexExpr(index_expr) = func_name else {
                    continue;
                };
                let Some(prefix_expr) = index_expr.get_prefix_expr() else {
                    continue;
                };
                let LuaExpr::NameExpr(prefix_name) = prefix_expr else {
                    continue;
                };
                let Some(prefix_name_text) = prefix_name.get_name_text() else {
                    continue;
                };
                let Some(index_key) = index_expr.get_index_key() else {
                    continue;
                };

                local_assigned_keys
                    .entry(prefix_name_text)
                    .or_default()
                    .insert(index_key.get_path_part());
            }
            _ => {}
        }
    }

    if !exported_local_names.is_empty() {
        for name in exported_local_names {
            if let Some(keys) = local_table_init_keys.get(&name) {
                exported_keys.extend(keys.iter().cloned());
            }
            if let Some(keys) = local_assigned_keys.get(&name) {
                exported_keys.extend(keys.iter().cloned());
            }
        }
    }

    let contains_key = exported_keys.contains(key_path);
    exported_key_cache.insert(module_info.file_id, exported_keys);
    contains_key
}

fn check_require_table_const_with_export<'a>(
    semantic_model: &'a SemanticModel,
    prefix_expr: &LuaExpr,
) -> Option<&'a ModuleInfo> {
    if let LuaExpr::CallExpr(call_expr) = prefix_expr {
        let module_info = parse_require_expr_module_info(semantic_model, call_expr)?;
        if module_info.is_export(semantic_model.get_db()) {
            return Some(module_info);
        }
        return None;
    }

    let LuaExpr::NameExpr(name_expr) = prefix_expr else {
        return None;
    };

    let decl_id = semantic_model
        .get_db()
        .get_reference_index()
        .get_local_reference(&semantic_model.get_file_id())
        .and_then(|file_ref| file_ref.get_decl_id(&name_expr.get_range()))?;

    // 获取声明
    let decl = semantic_model
        .get_db()
        .get_decl_index()
        .get_decl(&decl_id)?;
    if decl
        .get_value_syntax_id()
        .is_none_or(|syntax_id| syntax_id.get_kind() != LuaSyntaxKind::RequireCallExpr)
    {
        return None;
    }

    let module_info = parse_require_module_info(semantic_model, &decl)?;
    if module_info.is_export(semantic_model.get_db()) {
        return Some(module_info);
    }
    None
}

fn parse_require_expr_module_info<'a>(
    semantic_model: &'a SemanticModel,
    call_expr: &LuaCallExpr,
) -> Option<&'a ModuleInfo> {
    let arg_list = call_expr.get_args_list()?;
    let first_arg = arg_list.get_args().next()?;
    let require_path_type = semantic_model.infer_expr(first_arg.clone()).ok()?;
    let module_path: String = match &require_path_type {
        LuaType::StringConst(module_path) => module_path.as_ref().to_string(),
        _ => return None,
    };

    semantic_model
        .get_db()
        .get_module_index()
        .find_module_for_file(&module_path, semantic_model.get_file_id())
}
