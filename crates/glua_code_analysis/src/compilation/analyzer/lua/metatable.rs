use glua_parser::{
    BinaryOperator, LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaFuncStat, LuaIndexKey,
    LuaNameExpr, LuaTableExpr, LuaTableField, LuaVarExpr,
};

use crate::{
    InFiled, LuaDeclExtra, LuaOperator, LuaOperatorMetaMethod, LuaOperatorOwner, LuaSignatureId,
    OperatorFunction, SetmetatableFactoryBinding,
};

use super::LuaAnalyzer;

pub fn analyze_setmetatable(analyzer: &mut LuaAnalyzer, call_expr: LuaCallExpr) -> Option<()> {
    let arg_list = call_expr.get_args_list()?;
    let args = arg_list.get_args().collect::<Vec<_>>();

    if args.len() != 2 {
        return Some(());
    }

    let table = args[0].clone();
    let metatable = args[1].clone();

    let file_id = analyzer.file_id;
    let Some(metatable_range) = resolve_metatable_backing_table(analyzer, &metatable) else {
        return Some(());
    };

    analyzer.db.get_metatable_index_mut().add(
        InFiled::new(file_id, table.get_range()),
        metatable_range.clone(),
    );

    if let Some(binding) = setmetatable_factory_binding(
        analyzer,
        &call_expr,
        &table,
        &metatable,
        metatable_range.clone(),
    ) {
        analyzer
            .db
            .get_metatable_index_mut()
            .add_factory_binding(binding);
    }

    if let Some(backing_table) = resolve_metatable_backing_table(analyzer, &table) {
        analyzer
            .db
            .get_metatable_index_mut()
            .add(backing_table, metatable_range.clone());
    }

    let metatable_table = match metatable {
        LuaExpr::TableExpr(table) => Some(table),
        _ => table_expr_from_range(analyzer, &metatable_range),
    };
    if let Some(metatable) = metatable_table {
        let operator_owner = LuaOperatorOwner::Table(metatable_range);
        for field in metatable.get_fields() {
            analyze_metable_field(analyzer, &field, &operator_owner);
        }
    }

    Some(())
}

fn setmetatable_factory_binding(
    analyzer: &mut LuaAnalyzer,
    call_expr: &LuaCallExpr,
    table: &LuaExpr,
    metatable: &LuaExpr,
    metatable_range: InFiled<rowan::TextRange>,
) -> Option<SetmetatableFactoryBinding> {
    // The post-UnResolve transfer is intentionally limited to the canonical
    // factory idiom: a direct local table variable is given a direct class table
    // metatable. Inline metatables, aliases, and reassigned locals are handled by
    // ordinary table/metatable inference and are not bridged into class members.
    let LuaExpr::NameExpr(table_name) = table else {
        return None;
    };
    let LuaExpr::NameExpr(_) = metatable else {
        return None;
    };

    let decl_id = analyzer
        .db
        .get_reference_index()
        .get_var_reference_decl(&analyzer.file_id, table_name.get_range())?;
    let decl = analyzer.db.get_decl_index().get_decl(&decl_id)?;
    if !decl.is_local() {
        return None;
    }

    let table_range = resolve_metatable_backing_table(analyzer, table)?;
    let function_scope = analyzer
        .db
        .get_member_index()
        .enclosing_function_scope_range(analyzer.file_id, call_expr.get_position())?;

    Some(SetmetatableFactoryBinding {
        file_id: analyzer.file_id,
        table_range,
        metatable_range,
        local_name: table_name.get_name_text()?.into(),
        call_position: call_expr.get_position(),
        function_scope,
    })
}

fn resolve_metatable_backing_table(
    analyzer: &mut LuaAnalyzer,
    table: &LuaExpr,
) -> Option<InFiled<rowan::TextRange>> {
    table_backing_range_from_expr(analyzer, table)
}

fn table_backing_range_from_expr(
    analyzer: &mut LuaAnalyzer,
    expr: &LuaExpr,
) -> Option<InFiled<rowan::TextRange>> {
    match expr {
        LuaExpr::TableExpr(table_expr) => {
            Some(InFiled::new(analyzer.file_id, table_expr.get_range()))
        }
        LuaExpr::ParenExpr(paren_expr) => {
            table_backing_range_from_expr(analyzer, &paren_expr.get_expr()?)
        }
        LuaExpr::BinaryExpr(binary_expr)
            if binary_expr.get_op_token().map(|op| op.get_op()) == Some(BinaryOperator::OpOr) =>
        {
            let (_, right) = binary_expr.get_exprs()?;
            table_backing_range_from_expr(analyzer, &right)
        }
        LuaExpr::NameExpr(name_expr) => {
            let decl_id = analyzer
                .db
                .get_reference_index()
                .get_var_reference_decl(&analyzer.file_id, name_expr.get_range())?;
            let decl = analyzer.db.get_decl_index().get_decl(&decl_id)?;
            if !decl.is_local() {
                return inferred_table_range(analyzer, expr);
            }

            let receiver_signature_id = match &decl.extra {
                LuaDeclExtra::Param {
                    idx: 0,
                    signature_id,
                    ..
                } => Some(*signature_id),
                LuaDeclExtra::ImplicitSelf { .. } => name_expr
                    .syntax()
                    .ancestors()
                    .find_map(LuaClosureExpr::cast)
                    .map(|closure| LuaSignatureId::from_closure(analyzer.file_id, &closure)),
                _ => None,
            };
            if let Some(signature_id) = receiver_signature_id {
                let is_mutated = analyzer
                    .db
                    .get_reference_index()
                    .get_decl_references(&analyzer.file_id, &decl_id)
                    .is_some_and(|references| references.mutable);
                if is_mutated {
                    return None;
                }

                return receiver_owner_table_range(analyzer, name_expr, signature_id);
            }

            let root = analyzer
                .db
                .get_vfs()
                .get_syntax_tree(&decl_id.file_id)?
                .get_red_root();
            let value_expr = decl
                .get_value_syntax_id()?
                .to_node_from_root(&root)
                .and_then(LuaExpr::cast)?;
            table_backing_range_from_expr(analyzer, &value_expr)
        }
        LuaExpr::IndexExpr(_) => inferred_table_range(analyzer, expr),
        _ => None,
    }
}

fn table_expr_from_range(
    analyzer: &LuaAnalyzer,
    range: &InFiled<rowan::TextRange>,
) -> Option<LuaTableExpr> {
    if range.file_id != analyzer.file_id {
        return None;
    }

    let root = analyzer
        .db
        .get_vfs()
        .get_syntax_tree(&range.file_id)?
        .get_red_root();
    root.token_at_offset(range.value.start())
        .right_biased()?
        .parent_ancestors()
        .find_map(LuaTableExpr::cast)
        .filter(|table| table.get_range() == range.value)
}

fn inferred_table_range(
    analyzer: &mut LuaAnalyzer,
    expr: &LuaExpr,
) -> Option<InFiled<rowan::TextRange>> {
    match analyzer.infer_expr(expr).ok()? {
        crate::LuaType::TableConst(range) => Some(range),
        crate::LuaType::Instance(instance) => Some(instance.get_range().clone()),
        _ => None,
    }
}

fn receiver_owner_table_range(
    analyzer: &mut LuaAnalyzer,
    name_expr: &LuaNameExpr,
    signature_id: LuaSignatureId,
) -> Option<InFiled<rowan::TextRange>> {
    let signature = analyzer.db.get_signature_index().get(&signature_id)?;
    if signature.is_colon_define {
        let func_stat = name_expr.syntax().ancestors().find_map(LuaFuncStat::cast)?;
        let LuaVarExpr::IndexExpr(func_name) = func_stat.get_func_name()? else {
            return None;
        };
        return table_backing_range_from_expr(analyzer, &func_name.get_prefix_expr()?);
    }

    let closure = name_expr.syntax().ancestors().find_map(|node| {
        let closure = LuaClosureExpr::cast(node)?;
        (LuaSignatureId::from_closure(analyzer.file_id, &closure) == signature_id)
            .then_some(closure)
    })?;
    let field = closure.syntax().parent().and_then(LuaTableField::cast)?;
    let LuaIndexKey::Name(key) = field.get_field_key()? else {
        return None;
    };
    if !matches!(
        LuaOperatorMetaMethod::from_metatable_name(key.get_name_text()),
        Some(LuaOperatorMetaMethod::Call)
    ) {
        return None;
    }

    let table = field.syntax().parent().and_then(LuaTableExpr::cast)?;
    Some(InFiled::new(analyzer.file_id, table.get_range()))
}

fn analyze_metable_field(
    analyzer: &mut LuaAnalyzer,
    field: &LuaTableField,
    operator_owner: &LuaOperatorOwner,
) -> Option<()> {
    let field_name = match field.get_field_key()? {
        LuaIndexKey::Name(n) => n.get_name_text().to_string(),
        LuaIndexKey::String(s) => s.get_value(),
        _ => return None,
    };

    let meta_method = LuaOperatorMetaMethod::from_metatable_name(&field_name)?;
    let field_value = field.get_value_expr()?;
    let file_id = analyzer.file_id;

    let signature_id = match field_value {
        LuaExpr::ClosureExpr(closure) => LuaSignatureId::from_closure(file_id, &closure),
        _ => {
            let operator_func = match analyzer.infer_expr(&field_value).ok()? {
                crate::LuaType::Signature(signature_id) => {
                    OperatorFunction::Signature(signature_id)
                }
                crate::LuaType::DocFunction(func) => OperatorFunction::Func(func),
                _ => return None,
            };

            let operator = LuaOperator::new(
                operator_owner.clone(),
                meta_method,
                file_id,
                field.get_range(),
                operator_func,
            );
            analyzer.db.get_operator_index_mut().add_operator(operator);
            return Some(());
        }
    };

    let operator = LuaOperator::new(
        operator_owner.clone(),
        meta_method,
        file_id,
        field.get_range(),
        OperatorFunction::Signature(signature_id),
    );
    analyzer.db.get_operator_index_mut().add_operator(operator);

    Some(())
}
