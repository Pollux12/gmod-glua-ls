use rustc_hash::{FxHashMap, FxHashSet};

use super::StatementWrite;
use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaFuncStat, LuaIndexExpr,
    LuaIndexKey, LuaLiteralToken, LuaNameExpr, LuaTableExpr, LuaVarExpr, NumberResult, PathTrait,
};
use rowan::TextSize;

use smol_str::SmolStr;

use crate::{
    AccessorFuncCallMetadata, AccessorMember, AccessorOwner, AccessorPrefix, AccessorSignature,
    DbIndex, FileId, GlobalId, GmodClassCallArg, GmodClassCallLiteral, GmodScriptedClassCallKind,
    GmodScriptedClassCallMetadata, InFiled, InferFailReason, LuaMemberKey, LuaMemberOwner,
    LuaOperatorMetaMethod, LuaOperatorOwner, LuaSignatureId, LuaType, LuaTypeDeclId,
    compilation::analyzer::{
        gmod::static_call_arg_string, lua::LuaAnalyzer, unresolve::UnResolveSpecialCall,
    },
    get_member_value_expr,
    semantic::find_members_with_key,
};

#[derive(Debug, Clone, Copy)]
struct SpecialCallDirectBinding {
    file_id: FileId,
    position: TextSize,
}

#[derive(Debug, Default)]
pub(in crate::compilation::analyzer) struct SpecialCallDirectMatcher {
    name_expr_names: FxHashMap<String, Vec<SpecialCallDirectBinding>>,
    access_paths: FxHashMap<String, Vec<SpecialCallDirectBinding>>,
    /// The last segment of every access path in `access_paths`.
    member_names: FxHashSet<String>,
}

impl SpecialCallDirectMatcher {
    fn matches_name(
        &self,
        db: &DbIndex,
        caller_file_id: FileId,
        caller_position: TextSize,
        name: &str,
    ) -> bool {
        visible_binding(
            db,
            self.name_expr_names.get(name),
            caller_file_id,
            caller_position,
        )
    }

    fn matches_access_path(
        &self,
        db: &DbIndex,
        caller_file_id: FileId,
        caller_position: TextSize,
        access_path: &str,
    ) -> bool {
        visible_binding(
            db,
            self.access_paths.get(access_path),
            caller_file_id,
            caller_position,
        )
    }

    /// Whether a member named `member_name` may have a signature the
    /// receiver's call is special through: one with a receiver out-param, or
    /// one bound at an access path ending in it.
    fn may_have_receiver_member_signature(&self, db: &DbIndex, member_name: &str) -> bool {
        db.get_signature_index()
            .has_receiver_out_param_member_name(member_name)
            || self.member_names.contains(member_name)
    }
}

/// Whether one of `bindings` is visible to the caller; the hit is a read of
/// its file's signatures, so the caller goes again once they change.
fn visible_binding(
    db: &DbIndex,
    bindings: Option<&Vec<SpecialCallDirectBinding>>,
    caller_file_id: FileId,
    caller_position: TextSize,
) -> bool {
    bindings
        .into_iter()
        .flatten()
        .find(|binding| binding.is_visible_to(db, caller_file_id, caller_position))
        .inspect(|binding| {
            crate::db_index::derivation::note_read(|| {
                crate::db_index::derivation::FactKey::SignatureFile(binding.file_id)
            })
        })
        .is_some()
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
) -> SpecialCallDirectMatcher {
    let special_signatures = db
        .get_signature_index()
        .iter()
        .filter_map(|(signature_id, signature)| {
            signature.has_special_call_params().then_some(*signature_id)
        })
        .collect::<Vec<_>>();
    let mut matcher = SpecialCallDirectMatcher::default();
    let mut roots = FxHashMap::default();

    for signature_id in special_signatures {
        let file_id = signature_id.get_file_id();
        let root = roots.entry(file_id).or_insert_with(|| {
            db.get_vfs()
                .get_syntax_tree(&file_id)
                .map(|tree| tree.get_chunk_node())
        });
        let Some(closure) = root
            .as_ref()
            .and_then(|root| closure_at(root, signature_id.get_position()))
        else {
            continue;
        };
        collect_direct_special_call_binding(db, file_id, &closure, &mut matcher);
    }

    matcher
}

/// The closure whose signature sits at `position`.
fn closure_at(root: &glua_parser::LuaChunk, position: TextSize) -> Option<LuaClosureExpr> {
    root.syntax()
        .token_at_offset(position)
        .right_biased()?
        .parent_ancestors()
        .filter_map(LuaClosureExpr::cast)
        .find(|closure| closure.get_position() == position)
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
            if let Some(member_name) = access_path.rsplit('.').next()
                && !member_name.is_empty()
            {
                matcher.member_names.insert(member_name.to_string());
            }
            matcher
                .access_paths
                .entry(access_path.to_string())
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
    collect_accessorfunc_annotated_call(analyzer, &call_expr);

    if special_call_followup(analyzer, &call_expr).is_some() {
        analyzer.context.add_unresolve(
            UnResolveSpecialCall {
                file_id: analyzer.file_id,
                call_expr,
            }
            .into(),
        );
    }

    Some(())
}

/// Whether `call_expr` may reach a special call, which a follow-up resolves.
fn special_call_followup(analyzer: &mut LuaAnalyzer, call_expr: &LuaCallExpr) -> Option<()> {
    let prefix_expr = call_expr.get_prefix_expr()?;
    expr_special_call_followup(analyzer, &prefix_expr)
}

fn expr_special_call_followup(analyzer: &mut LuaAnalyzer, expr: &LuaExpr) -> Option<()> {
    match expr {
        LuaExpr::NameExpr(name_expr) => name_expr_special_call_followup(analyzer, name_expr),
        LuaExpr::IndexExpr(index_expr) => index_expr_special_call_followup(analyzer, index_expr),
        LuaExpr::ParenExpr(paren_expr) => {
            let inner_expr = paren_expr.get_expr()?;
            expr_special_call_followup(analyzer, &inner_expr)
        }
        _ => generic_expr_special_call_followup(analyzer, expr),
    }
}

fn generic_expr_special_call_followup(analyzer: &mut LuaAnalyzer, expr: &LuaExpr) -> Option<()> {
    let typ = analyzer.infer_expr(expr).ok()?;
    if type_has_special_call_signature(&analyzer.db, &typ)
        || type_has_special_call_operator_signature(
            &analyzer.db,
            analyzer.file_id,
            expr.get_position(),
            &typ,
        )
    {
        return Some(());
    }

    None
}

fn name_expr_special_call_followup(
    analyzer: &mut LuaAnalyzer,
    name_expr: &LuaNameExpr,
) -> Option<()> {
    match resolve_cached_name_expr_type(analyzer, name_expr) {
        Ok(Some(typ)) if type_has_special_call_signature(&analyzer.db, &typ) => Some(()),
        Ok(Some(typ))
            if type_has_special_call_operator_signature(
                &analyzer.db,
                analyzer.file_id,
                name_expr.get_position(),
                &typ,
            ) =>
        {
            Some(())
        }
        Ok(Some(_)) => None,
        Ok(None) => {
            match local_decl_special_call_state(analyzer, name_expr) {
                Some(true) => return Some(()),
                Some(false) => return None,
                None => {}
            }

            let name = name_expr.get_name_text()?;
            analyzer
                .special_call_direct_matcher
                .matches_name(
                    &analyzer.db,
                    analyzer.file_id,
                    name_expr.get_position(),
                    &name,
                )
                .then_some(())
        }
        Err(_) => unresolved_name_expr_special_call_followup(analyzer, name_expr),
    }
}

fn index_expr_has_receiver_special_call_signature(
    analyzer: &mut LuaAnalyzer,
    index_expr: &LuaIndexExpr,
) -> bool {
    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return false;
    };
    let Some(member_key) = get_static_member_key(index_expr) else {
        return false;
    };
    let LuaMemberKey::Name(member_name) = &member_key else {
        return false;
    };
    if !analyzer
        .special_call_direct_matcher
        .may_have_receiver_member_signature(&analyzer.db, member_name.as_str())
    {
        return false;
    }
    let Ok(receiver_type) = analyzer.infer_expr(&prefix_expr) else {
        return false;
    };
    find_members_with_key(&analyzer.db, &receiver_type, member_key, true).is_some_and(|members| {
        members.into_iter().any(|member_info| {
            type_has_special_call_signature(&analyzer.db, &member_info.typ)
                || member_info
                    .property_owner_id
                    .as_ref()
                    .and_then(|semantic_decl| {
                        type_cache_for_semantic_decl(&analyzer.db, semantic_decl)
                    })
                    .is_some_and(|typ| type_has_special_call_signature(&analyzer.db, &typ))
        })
    })
}

fn type_cache_for_semantic_decl(
    db: &DbIndex,
    semantic_decl: &crate::LuaSemanticDeclId,
) -> Option<LuaType> {
    let owner = match semantic_decl {
        crate::LuaSemanticDeclId::LuaDecl(decl_id) => (*decl_id).into(),
        crate::LuaSemanticDeclId::Member(member_id) => (*member_id).into(),
        crate::LuaSemanticDeclId::Signature(signature_id) => {
            return Some(LuaType::Signature(*signature_id));
        }
        crate::LuaSemanticDeclId::TypeDecl(type_decl_id) => {
            return Some(LuaType::Def(type_decl_id.clone()));
        }
    };
    db.get_type_index()
        .get_type_cache(&owner)
        .map(|type_cache| type_cache.as_type().clone())
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

fn index_expr_special_call_followup(
    analyzer: &mut LuaAnalyzer,
    index_expr: &LuaIndexExpr,
) -> Option<()> {
    if index_expr_has_receiver_special_call_signature(analyzer, index_expr) {
        return Some(());
    }

    match resolve_cached_index_expr_type(analyzer, index_expr) {
        Ok(Some(typ)) if type_has_special_call_signature(&analyzer.db, &typ) => Some(()),
        Ok(Some(typ))
            if type_has_special_call_operator_signature(
                &analyzer.db,
                analyzer.file_id,
                index_expr.get_position(),
                &typ,
            ) =>
        {
            Some(())
        }
        Ok(Some(_)) => None,
        Ok(None) | Err(_) => index_expr
            .get_access_path()
            .filter(|access_path| {
                analyzer.special_call_direct_matcher.matches_access_path(
                    &analyzer.db,
                    analyzer.file_id,
                    index_expr.get_position(),
                    access_path,
                )
            })
            .map(|_| ()),
    }
}

fn unresolved_name_expr_special_call_followup(
    analyzer: &mut LuaAnalyzer,
    name_expr: &LuaNameExpr,
) -> Option<()> {
    match local_decl_special_call_state(analyzer, name_expr) {
        Some(true) => return Some(()),
        Some(false) => return None,
        None => {}
    }

    let name = name_expr.get_name_text()?;
    if analyzer.special_call_direct_matcher.matches_name(
        &analyzer.db,
        analyzer.file_id,
        name_expr.get_position(),
        &name,
    ) {
        return Some(());
    }

    let decl_id = analyzer
        .db
        .get_reference_index()
        .get_var_reference_decl(&analyzer.file_id, name_expr.get_range())?;
    decl_value_matches_direct_special_call(analyzer, decl_id, name_expr.get_position())
        .then_some(())
}

fn decl_value_matches_direct_special_call(
    analyzer: &LuaAnalyzer,
    decl_id: crate::LuaDeclId,
    caller_position: TextSize,
) -> bool {
    let mut visited = FxHashSet::default();
    decl_value_matches_direct_special_call_inner(analyzer, decl_id, caller_position, &mut visited)
}

fn decl_value_matches_direct_special_call_inner(
    analyzer: &LuaAnalyzer,
    decl_id: crate::LuaDeclId,
    caller_position: TextSize,
    visited: &mut FxHashSet<crate::LuaDeclId>,
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
    let Some(node) = analyzer
        .db
        .get_vfs()
        .syntax_node_around(&decl.get_file_id(), value_syntax_id)
    else {
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
    visited: &mut FxHashSet<crate::LuaDeclId>,
) -> bool {
    if let Some(closure) = LuaClosureExpr::cast(node.clone()) {
        let signature_id = LuaSignatureId::from_closure(expr_file_id, &closure);
        return analyzer
            .db
            .get_signature_index()
            .has_special_call_params(&signature_id);
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
            &analyzer.db,
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
    visited: &mut FxHashSet<crate::LuaDeclId>,
) -> bool {
    if let Some(access_path) = index_expr.get_access_path()
        && analyzer.special_call_direct_matcher.matches_access_path(
            &analyzer.db,
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
    visited: &mut FxHashSet<crate::LuaDeclId>,
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
                &analyzer.db,
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
                    &analyzer.db,
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
    visited: &mut FxHashSet<crate::LuaDeclId>,
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
    let Some(node) = analyzer
        .db
        .get_vfs()
        .syntax_node_around(&decl.get_file_id(), value_syntax_id)
    else {
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
    visited: &mut FxHashSet<crate::LuaDeclId>,
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
        let Some(value_expr) = get_member_value_expr(&analyzer.db, member.get_id()) else {
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
                access_path.push_str(&glua_parser::path_segment(name));
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

fn call_expr_matches_direct_special_call(
    analyzer: &LuaAnalyzer,
    expr_file_id: FileId,
    call_expr: &LuaCallExpr,
    caller_position: TextSize,
    visited: &mut FxHashSet<crate::LuaDeclId>,
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
                    if type_has_special_call_signature(&analyzer.db, &typ)
                        || type_has_special_call_operator_signature(
                            &analyzer.db,
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
        &analyzer.db,
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
                &analyzer.db,
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
                    &analyzer.db,
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
            .has_special_call_params(signature_id),
        LuaType::DocFunction(func) => func.get_params().iter().any(|(_, param_type)| {
            param_type
                .as_ref()
                .map(type_contains_str_tpl_ref)
                .unwrap_or(false)
        }),
        LuaType::TypeGuard(inner) => type_has_special_call_signature(db, inner),
        LuaType::Union(union) => union
            .types()
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
        LuaType::Union(union) => union.types().any(|union_type| {
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
        LuaType::Union(union) => union.types().any(type_contains_str_tpl_ref),
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
    let caller_mask = infer_index.get_state_mask_at_offset(&caller_file_id, caller_position);
    let candidate_mask =
        infer_index.get_state_mask_at_offset(&candidate_file_id, candidate_position);

    caller_mask.is_compatible_with(candidate_mask)
}

fn collect_accessorfunc_annotated_call(
    analyzer: &mut LuaAnalyzer,
    call_expr: &LuaCallExpr,
) -> Option<()> {
    let prefix_expr = call_expr.get_prefix_expr()?;

    // `obj.Method(...)` takes its receiver from the prefix, `FuncName(...)` from its first argument.
    let (func_name, receiver_is_first_arg) = match &prefix_expr {
        LuaExpr::IndexExpr(index_expr) => {
            let name = match index_expr.get_index_key()? {
                LuaIndexKey::Name(name_token) => name_token.get_name_text().to_string(),
                LuaIndexKey::String(string_token) => string_token.get_value().to_string(),
                _ => return None,
            };
            (name, false)
        }
        LuaExpr::NameExpr(name_expr) => (name_expr.get_name_text()?.to_string(), true),
        _ => return None,
    };

    if !analyzer
        .db
        .get_accessor_func_index()
        .contains_name(&func_name)
    {
        return None;
    }

    let annotation =
        accessor_generator(analyzer.db, analyzer.file_id, &prefix_expr, &func_name)?.clone();

    let args_list = call_expr.get_args_list()?;
    let args = args_list.get_args().collect::<Vec<_>>();
    let name_arg = args.get(annotation.name_param_index)?;
    let accessor_name = extract_string_literal(name_arg)?;
    if accessor_name.is_empty() {
        return None;
    }

    let mut receiver_owner = None;
    let mut made: Vec<(LuaMemberOwner, SmolStr, bool)> = Vec::new();
    for template in &annotation.accessors {
        let owner = match &template.owner {
            AccessorOwner::Receiver => receiver_owner
                .get_or_insert_with(|| {
                    let receiver = if receiver_is_first_arg {
                        args.first().cloned()
                    } else {
                        match &prefix_expr {
                            LuaExpr::IndexExpr(index_expr) => index_expr.get_prefix_expr(),
                            _ => None,
                        }
                    };
                    receiver.and_then(|receiver| find_owner_for_prefix(analyzer, &receiver))
                })
                .clone(),
            AccessorOwner::NamedClass { callee, class_name } => {
                named_class_type_id(analyzer.db, analyzer.file_id, callee, class_name)
                    .map(LuaMemberOwner::Type)
            }
            AccessorOwner::Table(table) => Some(LuaMemberOwner::Element(table.clone())),
        };
        let Some(owner) = owner else {
            continue;
        };
        let prefix: SmolStr = match &template.prefix {
            AccessorPrefix::Literal(prefix) => prefix.clone(),
            AccessorPrefix::ParamOr(index, default) => match args.get(*index) {
                Some(arg) if !crate::compilation::analyzer::is_falsy_literal_expr(arg) => {
                    let Some(prefix) = extract_string_literal(arg) else {
                        continue;
                    };
                    prefix.into()
                }
                _ => default.clone(),
            },
        };
        let name = if template.capitalized {
            SmolStr::from(format!("{prefix}{}", lua_capitalized(&accessor_name)))
        } else {
            SmolStr::from(format!("{prefix}{accessor_name}"))
        };
        if !made
            .iter()
            .any(|(made_owner, made_name, _)| *made_owner == owner && *made_name == name)
        {
            made.push((owner, name, prefix == "Set"));
        }
    }

    // The first getter is declared at the name argument and the first setter at the
    // call; every further member at the call, each in a slot of its own.
    made.sort_by_key(|(_, _, is_setter)| *is_setter);
    let mut getter_at_name = true;
    let mut setter_at_call = true;
    let mut next_slot = 1;
    let members = made
        .into_iter()
        .map(|(owner, name, is_setter)| {
            let (syntax_id, slot) = if !is_setter && std::mem::take(&mut getter_at_name) {
                (name_arg.get_syntax_id(), 0)
            } else if is_setter && std::mem::take(&mut setter_at_call) {
                (call_expr.get_syntax_id(), 0)
            } else {
                next_slot += 1;
                (call_expr.get_syntax_id(), next_slot - 1)
            };
            AccessorMember {
                owner,
                name,
                signature: match (annotation.inferred, is_setter) {
                    (true, _) => AccessorSignature::Any,
                    (false, true) => AccessorSignature::Setter,
                    (false, false) => AccessorSignature::Getter,
                },
                syntax_id,
                slot,
            }
        })
        .collect::<Vec<_>>();
    if members.is_empty() {
        return None;
    }

    analyzer.write(StatementWrite::AddAccessorFuncCall {
        file_id: analyzer.file_id,
        metadata: AccessorFuncCallMetadata {
            syntax_id: call_expr.get_syntax_id(),
            members,
        },
    });

    Some(())
}

/// `name` with its first byte upper-cased, as `name:sub(1, 1):upper() .. name:sub(2)`
/// makes it.
fn lua_capitalized(name: &str) -> String {
    let mut name = name.to_string();
    if let Some(first) = name.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    name
}

/// The class a call to the global `callee` names by the literal `class_name`, when
/// `callee` takes its first parameter as a class-name template and returns that class,
/// as `FindMetaTable` does.
fn named_class_type_id(
    db: &DbIndex,
    file_id: FileId,
    callee: &str,
    class_name: &str,
) -> Option<LuaTypeDeclId> {
    let names_class = db
        .get_global_index()
        .get_global_decl_ids(callee)?
        .iter()
        .any(|decl_id| {
            let Some(LuaType::Signature(signature_id)) = db
                .get_type_index()
                .get_type_cache(&(*decl_id).into())
                .map(|cache| cache.as_type().clone())
            else {
                return false;
            };
            let Some(signature) = db.get_signature_index().get(&signature_id) else {
                return false;
            };
            let Some(LuaType::StrTplRef(class_template)) = signature
                .get_param_info_by_id(0)
                .map(|param| &param.type_ref)
            else {
                return false;
            };
            class_template.get_prefix().is_empty()
                && class_template.get_suffix().is_empty()
                && matches!(
                    signature.get_return_type(),
                    LuaType::TplRef(returned) | LuaType::ConstTplRef(returned)
                        if returned.get_tpl_id() == class_template.get_tpl_id()
                )
        });
    if !names_class {
        return None;
    }
    let type_decl = db.get_type_index().find_type_decl(file_id, class_name)?;
    type_decl.is_class().then(|| type_decl.get_id())
}

/// The function a local that `name_expr` reads was declared with and still holds;
/// `Some(None)` for a local holding something else or reassigned, and `None` for a
/// global name.
/// The accessor generator a call of `func_name` through `prefix_expr`
/// reaches: a name a local binds reaches only the generator that local holds,
/// any other the first registered under the name.
pub(in crate::compilation::analyzer) fn accessor_generator<'a>(
    db: &'a DbIndex,
    file_id: FileId,
    prefix_expr: &LuaExpr,
    func_name: &str,
) -> Option<&'a crate::AccessorFuncAnnotation> {
    let local_callee = match prefix_expr {
        LuaExpr::NameExpr(name_expr) => local_callee_signature(db, file_id, name_expr),
        _ => None,
    };
    db.get_accessor_func_index()
        .get_annotations(func_name)?
        .iter()
        .find(|annotation| match local_callee {
            None => true,
            Some(signature_id) => signature_id.is_some() && annotation.signature_id == signature_id,
        })
}

fn local_callee_signature(
    db: &DbIndex,
    file_id: FileId,
    name_expr: &LuaNameExpr,
) -> Option<Option<LuaSignatureId>> {
    let decl_id = db
        .get_reference_index()
        .get_var_reference_decl(&file_id, name_expr.get_range())?;
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    if decl.is_global() {
        return None;
    }
    if db
        .get_reference_index()
        .get_decl_references(&file_id, &decl_id)
        .is_some_and(|references| references.mutable)
    {
        return Some(None);
    }
    let signature_id = match db
        .get_type_index()
        .get_type_cache(&decl_id.into())
        .map(|cache| cache.as_type())
    {
        Some(LuaType::Signature(signature_id)) => Some(*signature_id),
        _ => decl
            .get_value_syntax_id()
            .filter(|value| value.get_kind() == glua_parser::LuaSyntaxKind::ClosureExpr)
            .map(|value| LuaSignatureId::new(file_id, value.get_range().start())),
    };
    Some(signature_id)
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

fn find_owner_for_prefix(analyzer: &mut LuaAnalyzer, prefix: &LuaExpr) -> Option<LuaMemberOwner> {
    if let Some(type_decl_id) = find_owner_type_for_prefix(analyzer, prefix) {
        return Some(LuaMemberOwner::Type(type_decl_id));
    }

    match analyzer.infer_expr(prefix).ok()? {
        LuaType::TableConst(range) => Some(LuaMemberOwner::Element(range)),
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

    let (literal_args, args) = extract_call_args(analyzer.db, analyzer.file_id, call_expr);

    analyzer.write(StatementWrite::AddGmodClassCall {
        file_id: analyzer.file_id,
        kind,
        metadata: Box::new(GmodScriptedClassCallMetadata {
            syntax_id: call_expr.get_syntax_id(),
            literal_args,
            args,
            field_args: Vec::new(),
            inheritance_roles: None,
            network_var_roles: None,
            vgui_panel_roles: None,
            derma_skin_roles: None,
            register_roles: None,
        }),
    });
}

fn extract_call_args(
    db: &DbIndex,
    file_id: FileId,
    call_expr: &LuaCallExpr,
) -> (Vec<Option<GmodClassCallLiteral>>, Vec<GmodClassCallArg>) {
    let Some(args_list) = call_expr.get_args_list() else {
        return (Vec::new(), Vec::new());
    };

    let mut literal_args = Vec::new();
    let mut args = Vec::new();

    for arg_expr in args_list.get_args() {
        let syntax_id = arg_expr.get_syntax_id();
        let value = extract_literal_or_name(&arg_expr).or_else(|| {
            static_call_arg_string(db, file_id, call_expr, &arg_expr)
                .map(GmodClassCallLiteral::String)
        });
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
        LuaExpr::NameExpr(name_expr) => name_expr
            .get_name_text()
            .map(|name| GmodClassCallLiteral::NameRef(name.to_string())),
        _ => None,
    }
}
