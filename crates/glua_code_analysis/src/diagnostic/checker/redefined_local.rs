use std::collections::{HashMap, HashSet};

use crate::{
    DbIndex, DiagnosticCode, GmodClassCallArgSource, GmodClassCallLiteral,
    GmodScriptedClassCallMetadata, LuaDecl, LuaDeclId, LuaDeclarationTree, LuaScope, LuaScopeKind,
    ScopeOrDeclId, SemanticModel,
};
use glua_parser::{LuaAstNode, LuaCallExpr, LuaExpr, LuaSyntaxKind, PathTrait};
use rowan::{TextRange, TextSize};

use super::{Checker, DiagnosticContext};

pub struct RedefinedLocalChecker;

impl Checker for RedefinedLocalChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::RedefinedLocal];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let file_id = semantic_model.get_file_id();
        let Some(decl_tree) = semantic_model
            .get_db()
            .get_decl_index()
            .get_decl_tree(&file_id)
        else {
            return;
        };

        let Some(root_scope) = decl_tree.get_root_scope() else {
            return;
        };
        let mut diagnostics = HashSet::new();
        let mut visible_locals = HashMap::new();
        let mut changes = Vec::new();
        let gmod_enabled = semantic_model.get_emmyrc().gmod.enabled;
        let syntax_registrations =
            collect_syntax_vgui_registration_calls(semantic_model.get_db(), file_id);

        check_scope_for_redefined_locals(
            semantic_model.get_db(),
            &file_id,
            decl_tree,
            root_scope,
            &mut visible_locals,
            &mut changes,
            &mut diagnostics,
            gmod_enabled,
            &syntax_registrations,
        );

        // 添加诊断信息
        for decl_id in diagnostics {
            if let Some(decl) = decl_tree.get_decl(&decl_id) {
                context.add_diagnostic(
                    DiagnosticCode::RedefinedLocal,
                    decl.get_range(),
                    format!("Redefined local variable `{name}`", name = decl.get_name())
                        .to_string(),
                    None,
                );
            }
        }
    }
}

fn check_scope_for_redefined_locals(
    db: &DbIndex,
    file_id: &crate::FileId,
    decl_tree: &LuaDeclarationTree,
    scope: &LuaScope,
    visible_locals: &mut HashMap<String, LuaDeclId>,
    changes: &mut Vec<VisibleLocalChange>,
    diagnostics: &mut HashSet<LuaDeclId>,
    gmod_enabled: bool,
    syntax_registrations: &[SyntaxVguiRegistrationCall],
) {
    let should_add_to_parent = should_add_to_parent_scope(scope);
    let scope_change_start = changes.len();

    // 检查当前作用域中的声明
    for child in scope.get_children() {
        if let ScopeOrDeclId::Decl(decl_id) = child
            && let Some(decl) = decl_tree.get_decl(decl_id)
        {
            let name = decl.get_name().to_string();
            if decl.is_local() && name != "..." && !name.starts_with("_") {
                if gmod_enabled && name == "self" {
                    continue;
                }
                if decl.is_seeded_class_local() {
                    continue;
                }
                if visible_locals.contains_key(&name) {
                    let old_decl = visible_locals
                        .get(&name)
                        .and_then(|id| decl_tree.get_decl(id));
                    if var_name_not_conflicts_with_function_param_name(decl, old_decl).is_some() {
                        continue;
                    }
                    if gmod_enabled
                        && let Some(old_decl) = old_decl
                        && gmod_registered_local_reuse(
                            db,
                            file_id,
                            &name,
                            old_decl,
                            decl,
                            syntax_registrations,
                        )
                    {
                        insert_visible_local(visible_locals, changes, name, *decl_id);
                        continue;
                    }

                    // 发现重定义，记录诊断
                    diagnostics.insert(*decl_id);
                }
                // 将当前声明加入映射
                insert_visible_local(visible_locals, changes, name, *decl_id);
            }
        }
    }

    // 检查子作用域
    for child in scope.get_children() {
        if let ScopeOrDeclId::Scope(scope_id) = child
            && let Some(child_scope) = decl_tree.get_scope(scope_id)
        {
            check_scope_for_redefined_locals(
                db,
                file_id,
                decl_tree,
                child_scope,
                visible_locals,
                changes,
                diagnostics,
                gmod_enabled,
                syntax_registrations,
            );
        }
    }

    // 更新到父作用域
    if !should_add_to_parent {
        rollback_visible_locals(visible_locals, changes, scope_change_start);
    }
}

struct SyntaxVguiRegistrationCall {
    table_name: String,
    table_arg_range: TextRange,
    call_range: TextRange,
    call_start: TextSize,
}

struct VisibleLocalChange {
    name: String,
    previous: Option<LuaDeclId>,
}

fn insert_visible_local(
    visible_locals: &mut HashMap<String, LuaDeclId>,
    changes: &mut Vec<VisibleLocalChange>,
    name: String,
    decl_id: LuaDeclId,
) {
    let previous = visible_locals.insert(name.clone(), decl_id);
    changes.push(VisibleLocalChange { name, previous });
}

fn rollback_visible_locals(
    visible_locals: &mut HashMap<String, LuaDeclId>,
    changes: &mut Vec<VisibleLocalChange>,
    scope_change_start: usize,
) {
    while changes.len() > scope_change_start {
        if let Some(change) = changes.pop() {
            if let Some(previous) = change.previous {
                visible_locals.insert(change.name, previous);
            } else {
                visible_locals.remove(&change.name);
            }
        }
    }
}

/// 处理 a = function(a)
fn var_name_not_conflicts_with_function_param_name(
    current_decl: &LuaDecl,
    old_decl: Option<&LuaDecl>,
) -> Option<()> {
    let old_decl = old_decl?;
    if old_decl.is_param() || !current_decl.is_param() {
        return None;
    }
    if let Some(value_syntax_id) = old_decl.get_value_syntax_id() {
        if value_syntax_id.get_kind() != LuaSyntaxKind::ClosureExpr {
            return None;
        }
        if let crate::LuaDeclExtra::Param { signature_id, .. } = current_decl.extra
            && value_syntax_id.get_range().start() == signature_id.get_position()
        {
            return Some(()); // 不冲突
        }
    }

    None
}

fn gmod_registered_local_reuse(
    db: &DbIndex,
    file_id: &crate::FileId,
    name: &str,
    old_decl: &LuaDecl,
    current_decl: &LuaDecl,
    syntax_registrations: &[SyntaxVguiRegistrationCall],
) -> bool {
    if db
        .get_gmod_class_metadata_index()
        .get_file_metadata(file_id)
        .is_some_and(|metadata| {
            metadata.vgui_register_calls.iter().any(|call| {
                gmod_registration_consumes_decl_before_reuse(
                    db,
                    *file_id,
                    name,
                    old_decl,
                    current_decl,
                    call,
                    call.vgui_panel_table_arg_source(1),
                )
            }) || metadata.vgui_register_table_calls.iter().any(|call| {
                gmod_registration_consumes_decl_before_reuse(
                    db,
                    *file_id,
                    name,
                    old_decl,
                    current_decl,
                    call,
                    call.vgui_panel_table_arg_source(0),
                )
            }) || metadata.derma_define_control_calls.iter().any(|call| {
                gmod_registration_consumes_decl_before_reuse(
                    db,
                    *file_id,
                    name,
                    old_decl,
                    current_decl,
                    call,
                    call.vgui_panel_table_arg_source(2),
                )
            })
        })
    {
        return true;
    }

    syntax_vgui_registration_consumes_decl_before_reuse(
        db,
        *file_id,
        name,
        old_decl,
        current_decl,
        syntax_registrations,
    )
}

fn collect_syntax_vgui_registration_calls(
    db: &DbIndex,
    file_id: crate::FileId,
) -> Vec<SyntaxVguiRegistrationCall> {
    let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) else {
        return Vec::new();
    };

    tree.get_chunk_node()
        .descendants::<LuaCallExpr>()
        .filter_map(|call_expr| {
            let table_arg_idx = match call_expr.get_access_path().as_deref() {
                Some("vgui.Register") => 1,
                Some("vgui.RegisterTable") => 0,
                Some("derma.DefineControl") => 2,
                _ => return None,
            };
            let args_list = call_expr.get_args_list()?;
            let table_arg = args_list.get_args().nth(table_arg_idx)?;
            let LuaExpr::NameExpr(name_expr) = table_arg else {
                return None;
            };
            Some(SyntaxVguiRegistrationCall {
                table_name: name_expr.get_name_text()?.to_string(),
                table_arg_range: name_expr.get_range(),
                call_range: call_expr.get_range(),
                call_start: call_expr.get_range().start(),
            })
        })
        .collect()
}

fn syntax_vgui_registration_consumes_decl_before_reuse(
    db: &DbIndex,
    file_id: crate::FileId,
    name: &str,
    old_decl: &LuaDecl,
    current_decl: &LuaDecl,
    syntax_registrations: &[SyntaxVguiRegistrationCall],
) -> bool {
    syntax_registrations.iter().any(|call| {
        call.table_name == name
            && db
                .get_reference_index()
                .get_var_reference_decl(&file_id, call.table_arg_range)
                == Some(old_decl.get_id())
            && (call.call_start < current_decl.get_range().start()
                || current_decl
                    .get_value_syntax_id()
                    .is_some_and(|value_syntax_id| value_syntax_id.get_range() == call.call_range))
    })
}

fn gmod_registration_consumes_decl_before_reuse(
    db: &DbIndex,
    file_id: crate::FileId,
    name: &str,
    old_decl: &LuaDecl,
    current_decl: &LuaDecl,
    call: &GmodScriptedClassCallMetadata,
    table_source: GmodClassCallArgSource,
) -> bool {
    if !matches!(
        call.value_for_arg_source(&table_source),
        Some(GmodClassCallLiteral::NameRef(table_name)) if table_name == name
    ) {
        return false;
    }

    let Some(table_arg_range) = table_arg_range(call, &table_source) else {
        return false;
    };
    if db
        .get_reference_index()
        .get_var_reference_decl(&file_id, table_arg_range)
        != Some(old_decl.get_id())
    {
        return false;
    }

    let call_start = call.syntax_id.get_range().start();
    call_start < current_decl.get_range().start()
        || current_decl
            .get_value_syntax_id()
            .is_some_and(|value_syntax_id| value_syntax_id == call.syntax_id)
}

fn table_arg_range(
    call: &GmodScriptedClassCallMetadata,
    table_source: &GmodClassCallArgSource,
) -> Option<rowan::TextRange> {
    if table_source.field_path.is_empty() {
        return call
            .args
            .get(table_source.arg_idx)
            .map(|arg| arg.syntax_id.get_range());
    }

    call.field_args
        .iter()
        .find(|arg| &arg.source == table_source)
        .map(|arg| arg.syntax_id.get_range())
}

/// 检查是否需要加入到父作用域
fn should_add_to_parent_scope(scope: &LuaScope) -> bool {
    scope.get_kind() == LuaScopeKind::FuncStat
        || scope.get_kind() == LuaScopeKind::LocalOrAssignStat
        || scope.get_kind() == LuaScopeKind::Repeat
        || scope.get_kind() == LuaScopeKind::MethodStat
}
