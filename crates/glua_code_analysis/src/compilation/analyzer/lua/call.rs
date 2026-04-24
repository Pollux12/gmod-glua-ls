use std::collections::{HashMap, HashSet};
use std::path::Path;

use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaFuncStat, LuaIndexExpr,
    LuaIndexKey, LuaLiteralToken, LuaNameExpr, LuaTableExpr, LuaTableField, LuaVarExpr,
    NumberResult, PathTrait,
};
use rowan::TextSize;
use wax::Pattern;

use crate::{
    AccessorFuncCallMetadata, DbIndex, FileId, GlobalId, GmodClassCallArg, GmodClassCallLiteral,
    GmodRealm, GmodScriptedClassCallKind, GmodScriptedClassCallMetadata, InFiled, InferFailReason,
    LuaMemberKey, LuaMemberOwner, LuaOperatorMetaMethod, LuaOperatorOwner, LuaSignatureId, LuaType,
    LuaTypeDeclId,
    compilation::analyzer::{lua::LuaAnalyzer, unresolve::UnResolveSpecialCall},
};

#[derive(Debug, Clone, Copy)]
struct SpecialCallDirectBinding {
    file_id: FileId,
    position: TextSize,
}

#[derive(Debug, Default)]
pub(in crate::compilation::analyzer) struct SpecialCallDirectMatcher {
    name_expr_names: HashMap<String, Vec<SpecialCallDirectBinding>>,
    access_paths: HashMap<String, Vec<SpecialCallDirectBinding>>,
}

impl SpecialCallDirectMatcher {
    fn matches_name(
        &self,
        db: &DbIndex,
        caller_file_id: FileId,
        caller_position: TextSize,
        name: &str,
    ) -> bool {
        self.name_expr_names
            .get(name)
            .into_iter()
            .flatten()
            .any(|binding| binding.is_visible_to(db, caller_file_id, caller_position))
    }

    fn matches_access_path(
        &self,
        db: &DbIndex,
        caller_file_id: FileId,
        caller_position: TextSize,
        access_path: &str,
    ) -> bool {
        self.access_paths
            .get(access_path)
            .into_iter()
            .flatten()
            .any(|binding| binding.is_visible_to(db, caller_file_id, caller_position))
    }
}

impl SpecialCallDirectBinding {
    fn is_visible_to(
        &self,
        db: &DbIndex,
        caller_file_id: FileId,
        caller_position: TextSize,
    ) -> bool {
        if !is_workspace_visible_to(db, caller_file_id, self.file_id) {
            return false;
        }

        is_realm_compatible(
            db,
            caller_file_id,
            caller_position,
            self.file_id,
            self.position,
        )
    }
}

fn is_workspace_visible_to(
    db: &DbIndex,
    caller_file_id: FileId,
    candidate_file_id: FileId,
) -> bool {
    let module_index = db.get_module_index();
    let Some(caller_workspace_id) = module_index.get_workspace_id(caller_file_id) else {
        return true;
    };

    let candidate_workspace_id = module_index
        .get_workspace_id(candidate_file_id)
        .unwrap_or(crate::WorkspaceId::MAIN);
    module_index
        .workspace_resolution_priority(caller_workspace_id, candidate_workspace_id)
        .is_some()
}

pub(in crate::compilation::analyzer) fn build_special_call_direct_matcher(
    db: &DbIndex,
    roots: &std::collections::HashMap<FileId, glua_parser::LuaChunk>,
) -> SpecialCallDirectMatcher {
    let mut matcher = SpecialCallDirectMatcher::default();

    for (file_id, root) in roots {
        for closure in root.descendants::<LuaClosureExpr>() {
            let signature_id = LuaSignatureId::from_closure(*file_id, &closure);
            let Some(signature) = db.get_signature_index().get(&signature_id) else {
                continue;
            };
            if !signature.has_special_call_params() {
                continue;
            }

            collect_direct_special_call_binding(db, *file_id, &closure, &mut matcher);
        }
    }

    matcher
}

fn collect_direct_special_call_binding(
    db: &DbIndex,
    file_id: FileId,
    closure: &LuaClosureExpr,
    matcher: &mut SpecialCallDirectMatcher,
) {
    let binding = SpecialCallDirectBinding {
        file_id,
        position: closure.get_position(),
    };
    if let Some(func_stat) = closure.get_parent::<LuaFuncStat>() {
        let Some(func_name) = func_stat.get_func_name() else {
            return;
        };
        add_direct_special_call_var_expr(db, file_id, &func_name, matcher, binding);
        return;
    }

    let Some(assign_stat) = closure.get_parent::<LuaAssignStat>() else {
        return;
    };
    let (vars, value_exprs) = assign_stat.get_var_and_expr_list();
    let Some(value_idx) = value_exprs
        .iter()
        .position(|expr| expr.get_position() == closure.get_position())
    else {
        return;
    };
    let Some(var_expr) = vars.get(value_idx) else {
        return;
    };
    add_direct_special_call_var_expr(db, file_id, &var_expr, matcher, binding);
}

fn add_direct_special_call_var_expr(
    db: &DbIndex,
    file_id: FileId,
    var_expr: &LuaVarExpr,
    matcher: &mut SpecialCallDirectMatcher,
    binding: SpecialCallDirectBinding,
) {
    match var_expr {
        LuaVarExpr::NameExpr(name_expr) => {
            if is_local_name_expr(db, file_id, name_expr) {
                return;
            }

            let Some(name) = name_expr.get_name_text() else {
                return;
            };
            matcher
                .name_expr_names
                .entry(name.to_string())
                .or_default()
                .push(binding);
        }
        LuaVarExpr::IndexExpr(index_expr) => {
            let Some(access_path) = index_expr.get_access_path() else {
                return;
            };
            matcher
                .access_paths
                .entry(access_path)
                .or_default()
                .push(binding);
        }
    }
}

fn is_local_name_expr(db: &DbIndex, file_id: FileId, name_expr: &LuaNameExpr) -> bool {
    db.get_reference_index()
        .get_var_reference_decl(&file_id, name_expr.get_range())
        .and_then(|decl_id| db.get_decl_index().get_decl(&decl_id))
        .map(|decl| decl.is_local())
        .unwrap_or(false)
}

pub(super) fn analyze_call(analyzer: &mut LuaAnalyzer, call_expr: LuaCallExpr) -> Option<()> {
    collect_gmod_scripted_class_call(analyzer, &call_expr);
    collect_gmod_vgui_call(analyzer, &call_expr);
    collect_accessorfunc_annotated_call(analyzer, &call_expr);

    let special_call_reason = get_special_call_followup_reason(analyzer, &call_expr);
    if let Some(reason) = special_call_reason {
        analyzer.context.add_unresolve(
            UnResolveSpecialCall {
                file_id: analyzer.file_id,
                call_expr,
            }
            .into(),
            reason,
        );
    }

    Some(())
}

fn get_special_call_followup_reason(
    analyzer: &mut LuaAnalyzer,
    call_expr: &LuaCallExpr,
) -> Option<InferFailReason> {
    let prefix_expr = call_expr.get_prefix_expr()?;
    get_expr_special_call_reason(analyzer, &prefix_expr)
}

fn get_expr_special_call_reason(
    analyzer: &mut LuaAnalyzer,
    expr: &LuaExpr,
) -> Option<InferFailReason> {
    match expr {
        LuaExpr::NameExpr(name_expr) => get_name_expr_special_call_reason(analyzer, name_expr),
        LuaExpr::IndexExpr(index_expr) => get_index_expr_special_call_reason(analyzer, index_expr),
        LuaExpr::ParenExpr(paren_expr) => {
            let inner_expr = paren_expr.get_expr()?;
            get_expr_special_call_reason(analyzer, &inner_expr)
        }
        _ => get_generic_expr_special_call_reason(analyzer, expr),
    }
}

fn get_generic_expr_special_call_reason(
    analyzer: &mut LuaAnalyzer,
    expr: &LuaExpr,
) -> Option<InferFailReason> {
    let typ = analyzer.infer_expr(expr).ok()?;
    if type_has_special_call_signature(analyzer.db, &typ)
        || type_has_special_call_operator_signature(
            analyzer.db,
            analyzer.file_id,
            expr.get_position(),
            &typ,
        )
    {
        return Some(InferFailReason::None);
    }

    None
}

fn get_name_expr_special_call_reason(
    analyzer: &mut LuaAnalyzer,
    name_expr: &LuaNameExpr,
) -> Option<InferFailReason> {
    match resolve_cached_name_expr_type(analyzer, name_expr) {
        Ok(Some(typ)) if type_has_special_call_signature(analyzer.db, &typ) => {
            Some(InferFailReason::None)
        }
        Ok(Some(typ))
            if type_has_special_call_operator_signature(
                analyzer.db,
                analyzer.file_id,
                name_expr.get_position(),
                &typ,
            ) =>
        {
            Some(InferFailReason::None)
        }
        Ok(Some(_)) => None,
        Ok(None) => {
            match local_decl_special_call_state(analyzer, name_expr) {
                Some(true) => return Some(InferFailReason::None),
                Some(false) => return None,
                None => {}
            }

            let name = name_expr.get_name_text()?;
            analyzer
                .special_call_direct_matcher
                .matches_name(
                    analyzer.db,
                    analyzer.file_id,
                    name_expr.get_position(),
                    &name,
                )
                .then_some(InferFailReason::None)
        }
        Err(reason) => unresolved_name_expr_special_call_reason(analyzer, name_expr, reason),
    }
}

fn local_decl_special_call_state(analyzer: &LuaAnalyzer, name_expr: &LuaNameExpr) -> Option<bool> {
    let decl_id = analyzer
        .db
        .get_reference_index()
        .get_var_reference_decl(&analyzer.file_id, name_expr.get_range())?;

    let decl = analyzer.db.get_decl_index().get_decl(&decl_id)?;
    if !decl.is_local() {
        return None;
    }

    Some(decl_value_matches_direct_special_call(
        analyzer,
        decl_id,
        name_expr.get_position(),
    ))
}

fn get_index_expr_special_call_reason(
    analyzer: &mut LuaAnalyzer,
    index_expr: &LuaIndexExpr,
) -> Option<InferFailReason> {
    match resolve_cached_index_expr_type(analyzer, index_expr) {
        Ok(Some(typ)) if type_has_special_call_signature(analyzer.db, &typ) => {
            Some(InferFailReason::None)
        }
        Ok(Some(typ))
            if type_has_special_call_operator_signature(
                analyzer.db,
                analyzer.file_id,
                index_expr.get_position(),
                &typ,
            ) =>
        {
            Some(InferFailReason::None)
        }
        Ok(Some(_)) => None,
        Ok(None) => index_expr
            .get_access_path()
            .filter(|access_path| {
                analyzer.special_call_direct_matcher.matches_access_path(
                    analyzer.db,
                    analyzer.file_id,
                    index_expr.get_position(),
                    access_path,
                )
            })
            .map(|_| InferFailReason::None),
        Err(reason) => index_expr
            .get_access_path()
            .filter(|access_path| {
                analyzer.special_call_direct_matcher.matches_access_path(
                    analyzer.db,
                    analyzer.file_id,
                    index_expr.get_position(),
                    access_path,
                )
            })
            .map(|_| reason),
    }
}

fn unresolved_name_expr_special_call_reason(
    analyzer: &mut LuaAnalyzer,
    name_expr: &LuaNameExpr,
    reason: InferFailReason,
) -> Option<InferFailReason> {
    match local_decl_special_call_state(analyzer, name_expr) {
        Some(true) => return Some(reason),
        Some(false) => return None,
        None => {}
    }

    let name = name_expr.get_name_text()?;
    if analyzer.special_call_direct_matcher.matches_name(
        analyzer.db,
        analyzer.file_id,
        name_expr.get_position(),
        &name,
    ) {
        return Some(reason);
    }

    let decl_id = analyzer
        .db
        .get_reference_index()
        .get_var_reference_decl(&analyzer.file_id, name_expr.get_range())?;
    decl_value_matches_direct_special_call(analyzer, decl_id, name_expr.get_position())
        .then_some(reason)
}

fn decl_value_matches_direct_special_call(
    analyzer: &LuaAnalyzer,
    decl_id: crate::LuaDeclId,
    caller_position: TextSize,
) -> bool {
    let mut visited = HashSet::new();
    decl_value_matches_direct_special_call_inner(analyzer, decl_id, caller_position, &mut visited)
}

fn decl_value_matches_direct_special_call_inner(
    analyzer: &LuaAnalyzer,
    decl_id: crate::LuaDeclId,
    caller_position: TextSize,
    visited: &mut HashSet<crate::LuaDeclId>,
) -> bool {
    if !visited.insert(decl_id) {
        return false;
    }

    let Some(decl) = analyzer.db.get_decl_index().get_decl(&decl_id) else {
        return false;
    };
    let Some(value_syntax_id) = decl.get_value_syntax_id() else {
        return false;
    };
    let Some(root) = analyzer.db.get_vfs().get_syntax_tree(&decl.get_file_id()) else {
        return false;
    };
    let Some(node) = value_syntax_id.to_node_from_root(&root.get_red_root()) else {
        return false;
    };

    expr_matches_direct_special_call(
        analyzer,
        decl.get_file_id(),
        &node,
        caller_position,
        visited,
    )
}

fn expr_matches_direct_special_call(
    analyzer: &LuaAnalyzer,
    expr_file_id: FileId,
    node: &glua_parser::LuaSyntaxNode,
    caller_position: TextSize,
    visited: &mut HashSet<crate::LuaDeclId>,
) -> bool {
    if let Some(closure) = LuaClosureExpr::cast(node.clone()) {
        let signature_id = LuaSignatureId::from_closure(expr_file_id, &closure);
        return analyzer
            .db
            .get_signature_index()
            .get(&signature_id)
            .map(|signature| signature.has_special_call_params())
            .unwrap_or(false);
    }

    if let Some(name_expr) = LuaNameExpr::cast(node.clone())
        && let Some(name) = name_expr.get_name_text()
    {
        if let Some(target_decl_id) = analyzer
            .db
            .get_reference_index()
            .get_var_reference_decl(&expr_file_id, name_expr.get_range())
            && decl_value_matches_direct_special_call_inner(
                analyzer,
                target_decl_id,
                caller_position,
                visited,
            )
        {
            return true;
        }

        return analyzer.special_call_direct_matcher.matches_name(
            analyzer.db,
            analyzer.file_id,
            caller_position,
            &name,
        );
    }

    if let Some(index_expr) = LuaIndexExpr::cast(node.clone()) {
        return index_expr_matches_direct_special_call(
            analyzer,
            expr_file_id,
            &index_expr,
            caller_position,
            visited,
        );
    }

    if let Some(call_expr) = LuaCallExpr::cast(node.clone()) {
        return call_expr_matches_direct_special_call(
            analyzer,
            expr_file_id,
            &call_expr,
            caller_position,
            visited,
        );
    }

    false
}

fn index_expr_matches_direct_special_call(
    analyzer: &LuaAnalyzer,
    expr_file_id: FileId,
    index_expr: &LuaIndexExpr,
    caller_position: TextSize,
    visited: &mut HashSet<crate::LuaDeclId>,
) -> bool {
    if let Some(access_path) = index_expr.get_access_path()
        && analyzer.special_call_direct_matcher.matches_access_path(
            analyzer.db,
            analyzer.file_id,
            caller_position,
            &access_path,
        )
    {
        return true;
    }

    let Some(member_key) = get_static_member_key(index_expr) else {
        return false;
    };
    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return false;
    };

    expr_matches_direct_special_call_with_member_chain(
        analyzer,
        expr_file_id,
        &prefix_expr,
        std::slice::from_ref(&member_key),
        caller_position,
        visited,
    )
}

fn expr_matches_direct_special_call_with_member_chain(
    analyzer: &LuaAnalyzer,
    expr_file_id: FileId,
    expr: &LuaExpr,
    member_chain: &[LuaMemberKey],
    caller_position: TextSize,
    visited: &mut HashSet<crate::LuaDeclId>,
) -> bool {
    if member_chain.is_empty() {
        return expr_matches_direct_special_call(
            analyzer,
            expr_file_id,
            &expr.syntax().clone(),
            caller_position,
            visited,
        );
    }

    match expr {
        LuaExpr::NameExpr(name_expr) => {
            if let Some(target_decl_id) = analyzer
                .db
                .get_reference_index()
                .get_var_reference_decl(&expr_file_id, name_expr.get_range())
                && decl_value_matches_direct_special_call_member_chain(
                    analyzer,
                    target_decl_id,
                    member_chain,
                    caller_position,
                    visited,
                )
            {
                return true;
            }

            let Some(base_path) = name_expr.get_access_path() else {
                return false;
            };
            let Some(access_path) = append_member_chain_to_access_path(&base_path, member_chain)
            else {
                return false;
            };

            analyzer.special_call_direct_matcher.matches_access_path(
                analyzer.db,
                analyzer.file_id,
                caller_position,
                &access_path,
            )
        }
        LuaExpr::IndexExpr(index_expr) => {
            if let Some(base_path) = index_expr.get_access_path()
                && let Some(access_path) =
                    append_member_chain_to_access_path(&base_path, member_chain)
                && analyzer.special_call_direct_matcher.matches_access_path(
                    analyzer.db,
                    analyzer.file_id,
                    caller_position,
                    &access_path,
                )
            {
                return true;
            }

            let Some(this_key) = get_static_member_key(index_expr) else {
                return false;
            };
            let Some(prefix_expr) = index_expr.get_prefix_expr() else {
                return false;
            };

            let mut chained_keys = Vec::with_capacity(member_chain.len() + 1);
            chained_keys.push(this_key);
            chained_keys.extend(member_chain.iter().cloned());
            expr_matches_direct_special_call_with_member_chain(
                analyzer,
                expr_file_id,
                &prefix_expr,
                &chained_keys,
                caller_position,
                visited,
            )
        }
        LuaExpr::TableExpr(table_expr) => table_expr_member_chain_matches_direct_special_call(
            analyzer,
            expr_file_id,
            table_expr,
            member_chain,
            caller_position,
            visited,
        ),
        _ => false,
    }
}

fn decl_value_matches_direct_special_call_member_chain(
    analyzer: &LuaAnalyzer,
    decl_id: crate::LuaDeclId,
    member_chain: &[LuaMemberKey],
    caller_position: TextSize,
    visited: &mut HashSet<crate::LuaDeclId>,
) -> bool {
    if !visited.insert(decl_id) {
        return false;
    }

    let Some(decl) = analyzer.db.get_decl_index().get_decl(&decl_id) else {
        return false;
    };
    let Some(value_syntax_id) = decl.get_value_syntax_id() else {
        return false;
    };
    let Some(root) = analyzer.db.get_vfs().get_syntax_tree(&decl.get_file_id()) else {
        return false;
    };
    let Some(node) = value_syntax_id.to_node_from_root(&root.get_red_root()) else {
        return false;
    };

    if let Some(expr) = LuaExpr::cast(node.clone()) {
        return expr_matches_direct_special_call_with_member_chain(
            analyzer,
            decl.get_file_id(),
            &expr,
            member_chain,
            caller_position,
            visited,
        );
    }

    if let Some(table_expr) = LuaTableExpr::cast(node) {
        return table_expr_member_chain_matches_direct_special_call(
            analyzer,
            decl.get_file_id(),
            &table_expr,
            member_chain,
            caller_position,
            visited,
        );
    }

    false
}

fn table_expr_member_chain_matches_direct_special_call(
    analyzer: &LuaAnalyzer,
    expr_file_id: FileId,
    table_expr: &LuaTableExpr,
    member_chain: &[LuaMemberKey],
    caller_position: TextSize,
    visited: &mut HashSet<crate::LuaDeclId>,
) -> bool {
    let Some((member_key, remaining_chain)) = member_chain.split_first() else {
        return false;
    };
    let owner = LuaMemberOwner::Element(InFiled::new(expr_file_id, table_expr.get_range()));
    let members = analyzer
        .db
        .get_member_index()
        .get_members_for_owner_key(&owner, member_key);

    members.into_iter().any(|member| {
        let Some(value_expr) = get_member_value_expr(analyzer.db, member.get_id()) else {
            return false;
        };
        expr_matches_direct_special_call_with_member_chain(
            analyzer,
            member.get_file_id(),
            &value_expr,
            remaining_chain,
            caller_position,
            visited,
        )
    })
}

fn append_member_chain_to_access_path(
    base_path: &str,
    member_chain: &[LuaMemberKey],
) -> Option<String> {
    let mut access_path = base_path.to_string();
    for member_key in member_chain {
        match member_key {
            LuaMemberKey::Name(name) => {
                access_path.push('.');
                access_path.push_str(name);
            }
            LuaMemberKey::Integer(value) => {
                access_path.push('[');
                access_path.push_str(&value.to_string());
                access_path.push(']');
            }
            LuaMemberKey::None | LuaMemberKey::ExprType(_) => return None,
        }
    }

    Some(access_path)
}

fn get_member_value_expr(db: &DbIndex, member_id: crate::LuaMemberId) -> Option<LuaExpr> {
    let root = db
        .get_vfs()
        .get_syntax_tree(&member_id.file_id)?
        .get_red_root();
    let node = member_id.get_syntax_id().to_node_from_root(&root)?;

    if let Some(field) = LuaTableField::cast(node.clone()) {
        return field.get_value_expr();
    }

    if let Some(index_expr) = LuaIndexExpr::cast(node.clone()) {
        if let Some(assign_stat) = index_expr.get_parent::<LuaAssignStat>() {
            let (vars, value_exprs) = assign_stat.get_var_and_expr_list();
            let value_idx = vars
                .iter()
                .position(|var| var.get_syntax_id() == index_expr.get_syntax_id())?;
            return value_exprs.get(value_idx).cloned();
        }

        if let Some(func_stat) = index_expr.get_parent::<LuaFuncStat>() {
            return func_stat.get_closure().map(LuaExpr::ClosureExpr);
        }
    }

    None
}

fn call_expr_matches_direct_special_call(
    analyzer: &LuaAnalyzer,
    expr_file_id: FileId,
    call_expr: &LuaCallExpr,
    caller_position: TextSize,
    visited: &mut HashSet<crate::LuaDeclId>,
) -> bool {
    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return false;
    };
    let LuaExpr::NameExpr(name_expr) = prefix_expr else {
        return false;
    };
    let Some(prefix_name) = name_expr.get_name_text() else {
        return false;
    };
    if prefix_name != "setmetatable" {
        return false;
    }

    let Some(args_list) = call_expr.get_args_list() else {
        return false;
    };
    let mut args = args_list.get_args();
    let _ = args.next();
    let Some(LuaExpr::TableExpr(metatable)) = args.next() else {
        return false;
    };

    metatable.get_fields().any(|field| {
        let field_name = match field.get_field_key() {
            Some(LuaIndexKey::Name(name)) => name.get_name_text().to_string(),
            Some(LuaIndexKey::String(string)) => string.get_value(),
            _ => return false,
        };
        if field_name != "__call" {
            return false;
        }

        let Some(value_expr) = field.get_value_expr() else {
            return false;
        };

        expr_matches_direct_special_call(
            analyzer,
            expr_file_id,
            &value_expr.syntax().clone(),
            caller_position,
            visited,
        )
    })
}

fn resolve_cached_name_expr_type(
    analyzer: &mut LuaAnalyzer,
    name_expr: &LuaNameExpr,
) -> Result<Option<LuaType>, InferFailReason> {
    if let Some(decl_id) = analyzer
        .db
        .get_reference_index()
        .get_var_reference_decl(&analyzer.file_id, name_expr.get_range())
    {
        return resolve_cached_decl_type(analyzer, decl_id, name_expr.get_position());
    }

    let Some(name) = name_expr.get_name_text() else {
        return Ok(None);
    };
    let module_index = analyzer.db.get_module_index();
    let global_index = analyzer.db.get_global_index();
    let candidate_decl_tiers =
        if let Some(workspace_id) = module_index.get_workspace_id(analyzer.file_id) {
            global_index
                .get_global_decl_id_priority_tiers(&name, module_index, workspace_id)
                .unwrap_or_default()
        } else {
            global_index
                .get_global_decl_ids(&name)
                .cloned()
                .map(|decl_ids| vec![(0, decl_ids)])
                .unwrap_or_default()
        };

    for (_, candidate_decl_ids) in candidate_decl_tiers {
        let mut first_cached_type = None;
        let mut first_unresolved_decl = None;
        for decl_id in candidate_decl_ids {
            match resolve_cached_decl_type(analyzer, decl_id, name_expr.get_position()) {
                Ok(Some(typ)) => {
                    if type_has_special_call_signature(analyzer.db, &typ)
                        || type_has_special_call_operator_signature(
                            analyzer.db,
                            analyzer.file_id,
                            name_expr.get_position(),
                            &typ,
                        )
                    {
                        return Ok(Some(typ));
                    }
                    if first_cached_type.is_none() {
                        first_cached_type = Some(typ);
                    }
                }
                Ok(None) => {}
                Err(InferFailReason::UnResolveDeclType(_)) => {
                    if first_unresolved_decl.is_none() {
                        first_unresolved_decl = Some(decl_id);
                    }
                }
                Err(reason) => return Err(reason),
            }
        }

        if let Some(decl_id) = first_unresolved_decl {
            return Err(InferFailReason::UnResolveDeclType(decl_id));
        }
        if let Some(typ) = first_cached_type {
            return Ok(Some(typ));
        }
    }

    Ok(None)
}

fn resolve_cached_decl_type(
    analyzer: &LuaAnalyzer,
    decl_id: crate::LuaDeclId,
    caller_position: TextSize,
) -> Result<Option<LuaType>, InferFailReason> {
    let Some(decl) = analyzer.db.get_decl_index().get_decl(&decl_id) else {
        return Ok(None);
    };
    if !is_realm_compatible(
        analyzer.db,
        analyzer.file_id,
        caller_position,
        decl.get_file_id(),
        decl.get_position(),
    ) {
        return Ok(None);
    }

    if let Some(type_cache) = analyzer.db.get_type_index().get_type_cache(&decl_id.into()) {
        return Ok(Some(type_cache.as_type().clone()));
    }

    if decl.has_initializer() {
        return Err(InferFailReason::UnResolveDeclType(decl_id));
    }

    Ok(None)
}

fn resolve_cached_index_expr_type(
    analyzer: &mut LuaAnalyzer,
    index_expr: &LuaIndexExpr,
) -> Result<Option<LuaType>, InferFailReason> {
    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return Ok(None);
    };
    let Some(prefix_type) = resolve_cached_expr_type(analyzer, &prefix_expr)? else {
        return Ok(None);
    };
    let Some(member_owner) = get_member_owner_for_cached_type(prefix_type) else {
        return Ok(None);
    };
    let Some(member_key) = get_static_member_key(index_expr) else {
        return Ok(None);
    };

    if let Some(member_item) = analyzer
        .db
        .get_member_index()
        .get_member_item(&member_owner, &member_key)
    {
        return member_item
            .resolve_type_with_realm_at_offset(
                analyzer.db,
                &analyzer.file_id,
                index_expr.get_position(),
            )
            .map(Some);
    }

    if let LuaMemberOwner::Type(type_decl_id) = member_owner {
        let global_owner = LuaMemberOwner::GlobalPath(GlobalId::new(type_decl_id.get_name()));
        if let Some(member_item) = analyzer
            .db
            .get_member_index()
            .get_member_item(&global_owner, &member_key)
        {
            return member_item
                .resolve_type_with_realm_at_offset(
                    analyzer.db,
                    &analyzer.file_id,
                    index_expr.get_position(),
                )
                .map(Some);
        }
    }

    Ok(None)
}

fn resolve_cached_expr_type(
    analyzer: &mut LuaAnalyzer,
    expr: &LuaExpr,
) -> Result<Option<LuaType>, InferFailReason> {
    match expr {
        LuaExpr::NameExpr(name_expr) => resolve_cached_name_expr_type(analyzer, name_expr),
        LuaExpr::IndexExpr(index_expr) => resolve_cached_index_expr_type(analyzer, index_expr),
        _ => Ok(None),
    }
}

fn get_static_member_key(index_expr: &LuaIndexExpr) -> Option<LuaMemberKey> {
    match index_expr.get_index_key()? {
        LuaIndexKey::Name(name) => Some(LuaMemberKey::Name(name.get_name_text().into())),
        LuaIndexKey::String(string) => Some(LuaMemberKey::Name(string.get_value().into())),
        LuaIndexKey::Integer(number) => match number.get_number_value() {
            NumberResult::Int(value) => Some(LuaMemberKey::Integer(value)),
            _ => None,
        },
        LuaIndexKey::Idx(idx) => Some(LuaMemberKey::Integer(idx as i64)),
        LuaIndexKey::Expr(_) => None,
    }
}

fn get_member_owner_for_cached_type(prefix_type: LuaType) -> Option<LuaMemberOwner> {
    match prefix_type {
        LuaType::TableConst(in_file_range) => Some(LuaMemberOwner::Element(in_file_range)),
        LuaType::Def(type_decl_id) | LuaType::Ref(type_decl_id) => {
            Some(LuaMemberOwner::Type(type_decl_id))
        }
        LuaType::Instance(instance) => Some(LuaMemberOwner::Element(instance.get_range().clone())),
        LuaType::TypeGuard(inner) => get_member_owner_for_cached_type((*inner).clone()),
        _ => None,
    }
}

fn type_has_special_call_signature(db: &DbIndex, typ: &LuaType) -> bool {
    match typ {
        LuaType::Signature(signature_id) => db
            .get_signature_index()
            .get(signature_id)
            .map(|signature| signature.has_special_call_params())
            .unwrap_or(false),
        LuaType::DocFunction(func) => func.get_params().iter().any(|(_, param_type)| {
            param_type
                .as_ref()
                .map(type_contains_str_tpl_ref)
                .unwrap_or(false)
        }),
        LuaType::TypeGuard(inner) => type_has_special_call_signature(db, inner),
        LuaType::Union(union) => union
            .into_vec()
            .iter()
            .any(|union_type| type_has_special_call_signature(db, union_type)),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .any(|intersection_type| type_has_special_call_signature(db, intersection_type)),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .any(|(union_type, _)| type_has_special_call_signature(db, union_type)),
        _ => false,
    }
}

fn type_has_special_call_operator_signature(
    db: &DbIndex,
    file_id: FileId,
    caller_position: TextSize,
    typ: &LuaType,
) -> bool {
    match typ {
        LuaType::TableConst(in_file_range) => {
            let Some(meta_table) = db.get_metatable_index().get(in_file_range) else {
                return false;
            };
            operator_owner_has_special_call_signature(
                db,
                file_id,
                caller_position,
                &LuaOperatorOwner::Table(meta_table.clone()),
            )
        }
        LuaType::Def(type_decl_id) | LuaType::Ref(type_decl_id) => {
            operator_owner_has_special_call_signature(
                db,
                file_id,
                caller_position,
                &LuaOperatorOwner::Type(type_decl_id.clone()),
            )
        }
        LuaType::Instance(instance) => type_has_special_call_operator_signature(
            db,
            file_id,
            caller_position,
            instance.get_base(),
        ),
        LuaType::TypeGuard(inner) => {
            type_has_special_call_operator_signature(db, file_id, caller_position, inner)
        }
        LuaType::Union(union) => union.into_vec().iter().any(|union_type| {
            type_has_special_call_operator_signature(db, file_id, caller_position, union_type)
        }),
        LuaType::Intersection(intersection) => {
            intersection.get_types().iter().any(|intersection_type| {
                type_has_special_call_operator_signature(
                    db,
                    file_id,
                    caller_position,
                    intersection_type,
                )
            })
        }
        LuaType::MultiLineUnion(union) => union.get_unions().iter().any(|(union_type, _)| {
            type_has_special_call_operator_signature(db, file_id, caller_position, union_type)
        }),
        _ => false,
    }
}

fn operator_owner_has_special_call_signature(
    db: &DbIndex,
    file_id: FileId,
    caller_position: TextSize,
    owner: &LuaOperatorOwner,
) -> bool {
    let Some(operator_ids) = db
        .get_operator_index()
        .get_operators(owner, LuaOperatorMetaMethod::Call)
    else {
        return false;
    };

    operator_ids.iter().any(|operator_id| {
        let Some(operator) = db.get_operator_index().get_operator(operator_id) else {
            return false;
        };
        if !is_workspace_visible_to(db, file_id, operator.get_file_id()) {
            return false;
        }

        let operator_position = operator.get_range().start();
        if !is_realm_compatible(
            db,
            file_id,
            caller_position,
            operator.get_file_id(),
            operator_position,
        ) {
            return false;
        }

        type_has_special_call_signature(db, &operator.get_operator_func(db))
    })
}

fn type_contains_str_tpl_ref(typ: &LuaType) -> bool {
    match typ {
        LuaType::StrTplRef(_) => true,
        LuaType::TypeGuard(inner) => type_contains_str_tpl_ref(inner),
        LuaType::Union(union) => union.into_vec().iter().any(type_contains_str_tpl_ref),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .any(type_contains_str_tpl_ref),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .any(|(union_type, _)| type_contains_str_tpl_ref(union_type)),
        _ => false,
    }
}

fn is_realm_compatible(
    db: &DbIndex,
    caller_file_id: FileId,
    caller_position: TextSize,
    candidate_file_id: FileId,
    candidate_position: TextSize,
) -> bool {
    if !db.get_emmyrc().gmod.enabled {
        return true;
    }

    let infer_index = db.get_gmod_infer_index();
    let caller_realm = infer_index.get_realm_at_offset(&caller_file_id, caller_position);
    let candidate_realm = infer_index.get_realm_at_offset(&candidate_file_id, candidate_position);

    !matches!(
        (caller_realm, candidate_realm),
        (GmodRealm::Client, GmodRealm::Server) | (GmodRealm::Server, GmodRealm::Client)
    )
}

fn collect_accessorfunc_annotated_call(
    analyzer: &mut LuaAnalyzer,
    call_expr: &LuaCallExpr,
) -> Option<()> {
    let prefix_expr = call_expr.get_prefix_expr()?;

    // Extract function name: handle both `obj.Method(...)` (IndexExpr) and `FuncName(...)` (NameExpr)
    let (func_name, owner_arg_index) = match &prefix_expr {
        LuaExpr::IndexExpr(index_expr) => {
            let name = match index_expr.get_index_key()? {
                LuaIndexKey::Name(name_token) => name_token.get_name_text().to_string(),
                LuaIndexKey::String(string_token) => string_token.get_value().to_string(),
                _ => return None,
            };
            (name, None) // owner comes from prefix
        }
        LuaExpr::NameExpr(name_expr) => {
            let name = name_expr.get_name_text()?.to_string();
            (name, Some(0usize)) // owner is the first argument
        }
        _ => return None,
    };

    if !analyzer
        .db
        .get_accessor_func_index()
        .contains_name(&func_name)
    {
        return None;
    }

    let name_param_index = analyzer
        .db
        .get_accessor_func_index()
        .get_annotations(&func_name)
        .and_then(|annotations| {
            annotations
                .first()
                .map(|annotation| annotation.name_param_index)
        })
        .unwrap_or(0);

    let args_list = call_expr.get_args_list()?;
    let args = args_list.get_args().collect::<Vec<_>>();
    let name_arg = args.get(name_param_index)?;
    let accessor_name = extract_string_literal(name_arg)?;
    if accessor_name.is_empty() {
        return None;
    }

    let call_syntax_id = call_expr.get_syntax_id();
    let name_arg_syntax_id = Some(name_arg.get_syntax_id());

    // Find owner type: from first argument (global call) or from prefix expression (method call)
    let owner_type_id = if let Some(arg_idx) = owner_arg_index {
        let owner_arg = args.get(arg_idx)?;
        find_owner_type_for_prefix(analyzer, owner_arg)?
    } else if let LuaExpr::IndexExpr(index_expr) = &prefix_expr {
        find_owner_type_for_prefix(analyzer, &index_expr.get_prefix_expr()?)?
    } else {
        return None;
    };

    analyzer.db.get_accessor_func_call_index_mut().add_call(
        analyzer.file_id,
        AccessorFuncCallMetadata {
            syntax_id: call_syntax_id,
            owner_type_id,
            accessor_name,
            name_arg_syntax_id,
        },
    );

    Some(())
}

fn extract_string_literal(expr: &LuaExpr) -> Option<String> {
    match expr {
        LuaExpr::LiteralExpr(literal_expr) => match literal_expr.get_literal()? {
            LuaLiteralToken::String(string_token) => Some(string_token.get_value().to_string()),
            _ => None,
        },
        _ => None,
    }
}

fn find_owner_type_for_prefix(
    analyzer: &mut LuaAnalyzer,
    prefix: &LuaExpr,
) -> Option<LuaTypeDeclId> {
    if let Ok(prefix_type) = analyzer.infer_expr(prefix)
        && let Some(type_decl_id) = find_decl_id_from_type(&prefix_type)
    {
        return Some(type_decl_id);
    }

    if let LuaExpr::NameExpr(name_expr) = prefix {
        let name = name_expr.get_name_text()?;
        let type_decl_id = LuaTypeDeclId::global(&name);
        if analyzer
            .db
            .get_type_index()
            .get_type_decl(&type_decl_id)
            .is_some()
        {
            return Some(type_decl_id);
        }
    }

    None
}

fn find_decl_id_from_type(typ: &LuaType) -> Option<LuaTypeDeclId> {
    match typ {
        LuaType::Def(type_decl_id) | LuaType::Ref(type_decl_id) => Some(type_decl_id.clone()),
        LuaType::Instance(instance) => find_decl_id_from_type(instance.get_base()),
        LuaType::TypeGuard(inner) => find_decl_id_from_type(inner),
        _ => None,
    }
}

fn collect_gmod_scripted_class_call(analyzer: &mut LuaAnalyzer, call_expr: &LuaCallExpr) {
    if !analyzer.gmod_enabled {
        return;
    }

    // Inline name check to avoid String allocation per call expression
    let prefix_expr = call_expr.get_prefix_expr();
    let kind = match prefix_expr.as_ref() {
        Some(LuaExpr::NameExpr(name_expr)) => name_expr
            .get_name_token()
            .and_then(|t| GmodScriptedClassCallKind::from_call_name(t.get_name_text())),
        Some(LuaExpr::IndexExpr(index_expr)) => {
            index_expr.get_index_key().and_then(|key| match &key {
                LuaIndexKey::Name(name_token) => {
                    GmodScriptedClassCallKind::from_call_name(name_token.get_name_text())
                }
                LuaIndexKey::String(string_token) => {
                    GmodScriptedClassCallKind::from_call_name(&string_token.get_value())
                }
                _ => None,
            })
        }
        _ => None,
    };

    let Some(kind) = kind else {
        return;
    };

    // AccessorFunc can be used anywhere (VGUI panels, etc.), not just in scripted class scopes.
    // DEFINE_BASECLASS / DeriveGamemode also need to work outside scripted scopes.
    // Only NetworkVar/NetworkVarElement are entity-specific and require scripted scope.
    if kind != GmodScriptedClassCallKind::DefineBaseClass
        && kind != GmodScriptedClassCallKind::DeriveGamemode
        && kind != GmodScriptedClassCallKind::AccessorFunc
        && !analyzer.is_scripted_class_scope
    {
        return;
    }

    let (literal_args, args) = extract_call_args(call_expr);

    analyzer.db.get_gmod_class_metadata_index_mut().add_call(
        analyzer.file_id,
        kind,
        GmodScriptedClassCallMetadata {
            syntax_id: call_expr.get_syntax_id(),
            literal_args,
            args,
        },
    );
}

fn collect_gmod_vgui_call(analyzer: &mut LuaAnalyzer, call_expr: &LuaCallExpr) {
    if !analyzer.gmod_enabled {
        return;
    }

    // Fast path: vgui.Register / derma.DefineControl are always dotted accesses.
    // Skip the expensive get_access_path() for the 99.9% of calls that can't match.
    let Some(LuaExpr::IndexExpr(index_expr)) = call_expr.get_prefix_expr() else {
        return;
    };

    let Some(LuaIndexKey::Name(key_token)) = index_expr.get_index_key() else {
        return;
    };
    let key_name = key_token.get_name_text();
    if key_name != "Register" && key_name != "DefineControl" {
        return;
    }

    // Check the base object: handle both direct (vgui.Register) and nested paths
    let kind = match index_expr.get_prefix_expr() {
        Some(LuaExpr::NameExpr(base)) => {
            let base_name = base.get_name_token().map(|t| t.get_name_text().to_string());
            match base_name.as_deref() {
                Some("vgui") if key_name == "Register" => GmodScriptedClassCallKind::VguiRegister,
                Some("derma") if key_name == "DefineControl" => {
                    GmodScriptedClassCallKind::DermaDefineControl
                }
                _ => return,
            }
        }
        Some(_) => {
            // Nested path like something.vgui.Register - fall back to full path
            let Some(call_path) = call_expr.get_access_path() else {
                return;
            };
            let Some(kind) = GmodScriptedClassCallKind::from_call_path(&call_path) else {
                return;
            };
            kind
        }
        None => return,
    };

    let (literal_args, args) = extract_call_args(call_expr);

    analyzer.db.get_gmod_class_metadata_index_mut().add_call(
        analyzer.file_id,
        kind,
        GmodScriptedClassCallMetadata {
            syntax_id: call_expr.get_syntax_id(),
            literal_args,
            args,
        },
    );
}

/// Pre-compute which files are in the scripted class scope.
/// Compiles glob patterns once and checks all files, avoiding per-file compilation.
pub(in crate::compilation::analyzer) fn compute_scripted_class_files(
    db: &DbIndex,
    file_ids: &[FileId],
) -> HashSet<FileId> {
    let scopes = &db.get_emmyrc().gmod.scripted_class_scopes;
    let include_patterns = scopes.include_patterns();
    let exclude_patterns = scopes.exclude_patterns();
    if include_patterns.is_empty() && exclude_patterns.is_empty() {
        return file_ids.iter().copied().collect();
    }

    let include_glob = if !include_patterns.is_empty() {
        match wax::any(
            include_patterns
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        ) {
            Ok(g) => Some(g),
            Err(err) => {
                log::warn!("Invalid gmod.scriptedClassScopes.include pattern: {err}");
                return file_ids.iter().copied().collect();
            }
        }
    } else {
        None
    };

    let exclude_glob = if !exclude_patterns.is_empty() {
        match wax::any(
            exclude_patterns
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        ) {
            Ok(g) => Some(g),
            Err(err) => {
                log::warn!("Invalid gmod.scriptedClassScopes.exclude pattern: {err}");
                return HashSet::new();
            }
        }
    } else {
        None
    };

    file_ids
        .iter()
        .copied()
        .filter(|file_id| check_file_in_scope(db, *file_id, &include_glob, &exclude_glob))
        .collect()
}

fn check_file_in_scope(
    db: &DbIndex,
    file_id: FileId,
    include_glob: &Option<wax::Any>,
    exclude_glob: &Option<wax::Any>,
) -> bool {
    let Some(file_path) = db.get_vfs().get_file_path(&file_id) else {
        return include_glob.is_none();
    };

    let normalized_path = file_path.to_string_lossy().replace('\\', "/");
    let mut candidate_paths = Vec::new();
    push_path_candidates(&mut candidate_paths, &normalized_path);
    let normalized_lower = normalized_path.to_ascii_lowercase();
    if let Some(lua_idx) = normalized_lower.find("/lua/") {
        let lua_relative_path = normalized_path[lua_idx + 1..].to_string();
        push_path_candidates(&mut candidate_paths, &lua_relative_path);
        if let Some(stripped) = lua_relative_path.strip_prefix("lua/") {
            push_path_candidates(&mut candidate_paths, stripped);
        }
    }
    if let Some(file_name) = file_path.file_name().and_then(|name| name.to_str()) {
        push_candidate_path(&mut candidate_paths, file_name);
    }

    if let Some(include) = include_glob {
        if !candidate_paths
            .iter()
            .any(|path| include.is_match(Path::new(path)))
        {
            return false;
        }
    }

    if let Some(exclude) = exclude_glob {
        if candidate_paths
            .iter()
            .any(|path| exclude.is_match(Path::new(path)))
        {
            return false;
        }
    }

    true
}

fn push_path_candidates(candidate_paths: &mut Vec<String>, path: &str) {
    push_candidate_path(candidate_paths, path);

    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    for idx in 0..segments.len() {
        push_candidate_path(candidate_paths, &segments[idx..].join("/"));
    }
}

fn push_candidate_path(candidate_paths: &mut Vec<String>, candidate: &str) {
    if candidate.is_empty() {
        return;
    }

    if candidate_paths.iter().any(|existing| existing == candidate) {
        return;
    }

    candidate_paths.push(candidate.to_string());
}

fn extract_call_args(
    call_expr: &LuaCallExpr,
) -> (Vec<Option<GmodClassCallLiteral>>, Vec<GmodClassCallArg>) {
    let Some(args_list) = call_expr.get_args_list() else {
        return (Vec::new(), Vec::new());
    };

    let mut literal_args = Vec::new();
    let mut args = Vec::new();

    for arg_expr in args_list.get_args() {
        let syntax_id = arg_expr.get_syntax_id();
        let value = extract_literal_or_name(&arg_expr);
        literal_args.push(value.clone());
        args.push(GmodClassCallArg { syntax_id, value });
    }

    (literal_args, args)
}

fn extract_literal_or_name(expr: &LuaExpr) -> Option<GmodClassCallLiteral> {
    match expr {
        LuaExpr::LiteralExpr(literal_expr) => match literal_expr.get_literal()? {
            LuaLiteralToken::String(string_token) => Some(GmodClassCallLiteral::String(
                string_token.get_value().to_string(),
            )),
            LuaLiteralToken::Number(number_token) => match number_token.get_number_value() {
                NumberResult::Int(value) => Some(GmodClassCallLiteral::Integer(value)),
                NumberResult::Uint(value) => Some(GmodClassCallLiteral::Unsigned(value)),
                NumberResult::Float(value) => Some(GmodClassCallLiteral::Float(value)),
            },
            LuaLiteralToken::Bool(bool_token) => {
                Some(GmodClassCallLiteral::Boolean(bool_token.is_true()))
            }
            LuaLiteralToken::Nil(_) => Some(GmodClassCallLiteral::Nil),
            _ => None,
        },
        LuaExpr::NameExpr(name_expr) => {
            name_expr.get_name_text().map(GmodClassCallLiteral::NameRef)
        }
        _ => None,
    }
}
