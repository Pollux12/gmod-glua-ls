use super::*;

pub(super) fn collect_numeric_range_table_populations_for_file(
    db: &DbIndex,
    file_id: FileId,
    root: LuaChunk,
) -> Vec<TableNumericRangePopulation> {
    let Some(block) = root.get_block() else {
        return Vec::new();
    };
    let mut local_helpers: HashMap<String, LuaClosureExpr> = HashMap::new();
    let mut populations = Vec::new();
    let mut cache = LuaInferCache::new(file_id, Default::default());

    for stat in block.get_stats() {
        match stat {
            LuaStat::LocalFuncStat(local_func) => {
                if let (Some(name_token), Some(closure)) = (
                    local_func
                        .get_local_name()
                        .and_then(|name| name.get_name_token()),
                    local_func.get_closure(),
                ) {
                    local_helpers.insert(name_token.get_name_text().to_string(), closure);
                }
            }
            LuaStat::FuncStat(func_stat) => {
                if let (Some(name), Some(closure)) =
                    (simple_func_stat_name(&func_stat), func_stat.get_closure())
                {
                    local_helpers.insert(name, closure);
                }
            }
            LuaStat::CallExprStat(call_stat) => {
                let Some(call_expr) = call_stat.get_call_expr() else {
                    continue;
                };
                let Some(helper_name) = call_expr_name(&call_expr) else {
                    local_helpers.clear();
                    continue;
                };
                let Some(closure) = local_helpers.get(&helper_name) else {
                    local_helpers.clear();
                    continue;
                };
                if let Some(population) = numeric_range_population_from_helper_call(
                    db,
                    &mut cache,
                    file_id,
                    closure,
                    &call_expr,
                    &local_helpers,
                ) {
                    populations.push(population);
                } else if let Some(outer_populations) = numeric_range_populations_from_outer_call(
                    db,
                    &mut cache,
                    file_id,
                    closure,
                    &call_expr,
                    &local_helpers,
                ) {
                    populations.extend(outer_populations);
                } else {
                    local_helpers.clear();
                }
            }
            LuaStat::AssignStat(assign_stat) => {
                let (vars, _) = assign_stat.get_var_and_expr_list();
                for var in vars {
                    if let LuaVarExpr::NameExpr(name_expr) = var
                        && let Some(name) = name_expr.get_name_text()
                    {
                        local_helpers.remove(&name);
                    }
                }
            }
            LuaStat::LocalStat(local_stat) => {
                for local_name in local_stat.get_local_name_list() {
                    if let Some(name_token) = local_name.get_name_token() {
                        local_helpers.remove(name_token.get_name_text());
                    }
                }
            }
            _ if helper_invalidated_by_descendant_write(&stat, &local_helpers) => {
                local_helpers.clear();
            }
            _ if helper_invalidated_by_descendant_call(&stat, &local_helpers) => {
                local_helpers.clear();
            }
            _ => {}
        }
    }

    populations
}

fn simple_func_stat_name(func_stat: &LuaFuncStat) -> Option<String> {
    let LuaVarExpr::NameExpr(name_expr) = func_stat.get_func_name()? else {
        return None;
    };
    name_expr.get_name_text()
}

fn numeric_range_populations_from_outer_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    outer_closure: &LuaClosureExpr,
    outer_call_expr: &LuaCallExpr,
    helpers: &HashMap<String, LuaClosureExpr>,
) -> Option<Vec<TableNumericRangePopulation>> {
    let block = outer_closure.get_block()?;
    let mut populations = Vec::new();
    let mut populated_tables: HashSet<String> = HashSet::new();

    for stat in block.get_stats() {
        match stat {
            LuaStat::CallExprStat(call_stat) => {
                let call_expr = call_stat.get_call_expr()?;
                let helper_name = call_expr_name(&call_expr)?;
                if call_name_shadowed_in_closure_before_call(
                    outer_closure,
                    &helper_name,
                    &call_expr,
                ) {
                    return None;
                }
                let fill_closure = helpers.get(&helper_name)?;
                let population = numeric_range_population_from_helper_call(
                    db,
                    cache,
                    file_id,
                    fill_closure,
                    &call_expr,
                    helpers,
                );
                let mut population = population?;
                population.call_range = outer_call_expr.get_range();
                populated_tables.insert(population.table_global.clone());
                populations.push(population);
            }
            LuaStat::AssignStat(assign_stat)
                if !assign_writes_tracked_helper(&assign_stat, helpers)
                    && is_harmless_table_reset(&assign_stat) =>
            {
                for table_name in reset_table_names(&assign_stat) {
                    if populated_tables.contains(&table_name) {
                        return None;
                    }
                }
            }
            LuaStat::LocalStat(local_stat)
                if !local_shadows_tracked_helper(&local_stat, helpers)
                    && is_harmless_local_alias_or_assignment(&local_stat) => {}
            _ => return None,
        }
    }

    if populations.is_empty() {
        None
    } else {
        Some(populations)
    }
}

fn assign_writes_tracked_helper(
    assign_stat: &LuaAssignStat,
    helpers: &HashMap<String, LuaClosureExpr>,
) -> bool {
    let (vars, _) = assign_stat.get_var_and_expr_list();
    vars.into_iter().any(|var| {
        matches!(var, LuaVarExpr::NameExpr(name_expr) if name_expr
            .get_name_text()
            .is_some_and(|name| helpers.contains_key(&name)))
    })
}

fn local_shadows_tracked_helper(
    local_stat: &LuaLocalStat,
    helpers: &HashMap<String, LuaClosureExpr>,
) -> bool {
    local_stat.get_local_name_list().any(|local_name| {
        local_name
            .get_name_token()
            .is_some_and(|name| helpers.contains_key(name.get_name_text()))
    })
}

fn is_harmless_table_reset(assign_stat: &LuaAssignStat) -> bool {
    let (vars, exprs) = assign_stat.get_var_and_expr_list();
    vars.len() == exprs.len()
        && vars.iter().zip(exprs.iter()).all(|(var, expr)| {
            matches!(var, LuaVarExpr::NameExpr(_)) && matches!(expr, LuaExpr::TableExpr(_))
        })
}

fn reset_table_names(assign_stat: &LuaAssignStat) -> Vec<String> {
    let (vars, _) = assign_stat.get_var_and_expr_list();
    vars.into_iter()
        .filter_map(|var| match var {
            LuaVarExpr::NameExpr(name_expr) => name_expr.get_name_text(),
            _ => None,
        })
        .collect()
}

fn is_harmless_local_alias_or_assignment(local_stat: &LuaLocalStat) -> bool {
    let exprs = local_stat.get_value_exprs().collect::<Vec<_>>();
    exprs
        .iter()
        .all(|expr| matches!(expr, LuaExpr::NameExpr(_) | LuaExpr::TableExpr(_)))
}

fn helper_invalidated_by_descendant_write(
    stat: &LuaStat,
    local_helpers: &HashMap<String, LuaClosureExpr>,
) -> bool {
    if local_helpers.is_empty() {
        return false;
    }
    for assign_stat in stat.descendants::<LuaAssignStat>() {
        if is_node_in_nested_closure(assign_stat.syntax(), stat.syntax()) {
            continue;
        }
        let (vars, _) = assign_stat.get_var_and_expr_list();
        for var in vars {
            if let LuaVarExpr::NameExpr(name_expr) = var
                && let Some(name) = name_expr.get_name_text()
                && local_helpers.contains_key(&name)
            {
                return true;
            }
        }
    }
    false
}

fn helper_invalidated_by_descendant_call(
    stat: &LuaStat,
    local_helpers: &HashMap<String, LuaClosureExpr>,
) -> bool {
    if local_helpers.is_empty() {
        return false;
    }
    stat.descendants::<LuaCallExpr>()
        .any(|call_expr| !is_node_in_nested_closure(call_expr.syntax(), stat.syntax()))
}

fn is_node_in_nested_closure(
    node: &glua_parser::LuaSyntaxNode,
    boundary: &glua_parser::LuaSyntaxNode,
) -> bool {
    node.ancestors()
        .take_while(|ancestor| ancestor != boundary)
        .any(|ancestor| LuaClosureExpr::can_cast(ancestor.kind().into()))
}

fn call_expr_name(call_expr: &LuaCallExpr) -> Option<String> {
    let LuaExpr::NameExpr(name_expr) = call_expr.get_prefix_expr()? else {
        return None;
    };
    name_expr.get_name_text()
}

fn call_name_shadowed_in_closure_before_call(
    closure: &LuaClosureExpr,
    call_name: &str,
    call_expr: &LuaCallExpr,
) -> bool {
    if closure.get_params_list().is_some_and(|params| {
        params
            .get_params()
            .any(|param| param_name_eq(&param, call_name))
    }) {
        return true;
    }

    let Some(block) = closure.get_block() else {
        return true;
    };
    let call_start = call_expr.syntax().text_range().start();

    for local_stat in block.syntax().descendants().filter_map(LuaLocalStat::cast) {
        if is_node_in_nested_closure(local_stat.syntax(), block.syntax())
            || !node_ends_before(local_stat.syntax(), call_start)
        {
            continue;
        }
        if local_stat
            .get_local_name_list()
            .any(|local_name| local_name_name_eq(&local_name, call_name))
        {
            return true;
        }
    }

    for local_func in block
        .syntax()
        .descendants()
        .filter_map(LuaLocalFuncStat::cast)
    {
        if is_node_in_nested_closure(local_func.syntax(), block.syntax())
            || !node_ends_before(local_func.syntax(), call_start)
        {
            continue;
        }
        if local_func
            .get_local_name()
            .is_some_and(|local_name| local_name_name_eq(&local_name, call_name))
        {
            return true;
        }
    }

    false
}

fn node_ends_before(node: &LuaSyntaxNode, position: TextSize) -> bool {
    node.text_range().end() <= position
}

fn param_name_eq(param: &LuaParamName, name: &str) -> bool {
    param
        .get_name_token()
        .is_some_and(|name_token| name_token.get_name_text() == name)
}

fn local_name_name_eq(local_name: &LuaLocalName, name: &str) -> bool {
    local_name
        .get_name_token()
        .is_some_and(|name_token| name_token.get_name_text() == name)
}

fn numeric_range_population_from_helper_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    closure: &LuaClosureExpr,
    call_expr: &LuaCallExpr,
    helpers: &HashMap<String, LuaClosureExpr>,
) -> Option<TableNumericRangePopulation> {
    let args = call_expr
        .get_args_list()
        .map(|args| args.get_args().collect::<Vec<_>>())
        .unwrap_or_default();
    let params = closure
        .get_params_list()
        .map(|params| params.get_params().collect::<Vec<_>>())
        .unwrap_or_default();
    let block = closure.get_block()?;
    let stats = block.get_stats().collect::<Vec<_>>();
    let (prelude_stats, for_stat) = helper_prelude_and_single_for(&stats)?;

    let iter_exprs = for_stat.get_iter_expr().collect::<Vec<_>>();
    let [start_expr, end_expr] = iter_exprs.as_slice() else {
        return None;
    };
    let start = integer_const_expr_value(db, cache, start_expr)?;
    let end = integer_const_expr_value(db, cache, end_expr)?;

    let for_var_name = for_stat.get_var_name()?.get_name_text().to_string();
    let for_block = for_stat.get_block()?;
    let for_stats = for_block.get_stats().collect::<Vec<_>>();
    if loop_contains_rejected_control_flow(&for_block) {
        return None;
    }
    let (assign_stat, branchy_loop) = match for_stats.as_slice() {
        [LuaStat::AssignStat(assign_stat)] => (assign_stat.clone(), false),
        _ => (final_loop_target_assignment(&for_stats)?, true),
    };
    let (vars, exprs) = assign_stat.get_var_and_expr_list();
    let ([LuaVarExpr::IndexExpr(index_expr)], [rhs_expr]) = (vars.as_slice(), exprs.as_slice())
    else {
        return None;
    };
    let LuaExpr::NameExpr(prefix_name) = index_expr.get_prefix_expr()? else {
        return None;
    };
    let param_name = prefix_name.get_name_text()?;
    let LuaIndexKey::Expr(LuaExpr::NameExpr(key_name)) = index_expr.get_index_key()? else {
        return None;
    };
    if key_name.get_name_text().as_deref() != Some(&for_var_name) {
        return None;
    }
    let arg_index = params.iter().position(|param| {
        param
            .get_name_token()
            .is_some_and(|name| name.get_name_text() == param_name.as_str())
    })?;
    let LuaExpr::NameExpr(table_name_expr) = args.get(arg_index)? else {
        return None;
    };
    if name_expr_resolves_to_local(db, file_id, table_name_expr) {
        return None;
    }
    let table_global = table_name_expr.get_name_text()?.to_string();
    let protected_names = protected_pre_loop_names(
        helpers,
        &table_global,
        &param_name,
        &for_var_name,
        start_expr,
        end_expr,
    );
    if !prelude_stats.iter().all(|stat| {
        pre_loop_stat_is_harmless(db, cache, file_id, closure, stat, helpers, &protected_names)
    }) {
        return None;
    }
    if branchy_loop
        && !branchy_loop_writes_target_on_continue_paths(
            db,
            cache,
            file_id,
            &for_stats,
            &param_name,
            &for_var_name,
            helpers,
            &protected_names,
        )
    {
        return None;
    }
    if !numeric_range_rhs_is_safe(
        db,
        cache,
        file_id,
        closure,
        rhs_expr,
        helpers,
        &table_global,
    ) {
        return None;
    }
    let mut rhs_type = infer_expr(db, cache, rhs_expr.clone()).ok().or_else(|| {
        infer_tracked_safe_helper_call_return_type(db, cache, file_id, rhs_expr, helpers)
    })?;
    if rhs_type.is_nullable() || rhs_type.is_nil() {
        if let Some(helper_return_type) =
            infer_tracked_safe_helper_call_return_type(db, cache, file_id, rhs_expr, helpers)
        {
            rhs_type = helper_return_type;
        }
    }
    if rhs_type.is_nullable() || rhs_type.is_nil() {
        return None;
    }
    Some(TableNumericRangePopulation {
        table_global,
        start,
        end,
        value_type: rhs_type,
        file_id,
        call_range: call_expr.get_range(),
    })
}

fn helper_prelude_and_single_for(stats: &[LuaStat]) -> Option<(&[LuaStat], LuaForStat)> {
    let for_index = stats
        .iter()
        .position(|stat| matches!(stat, LuaStat::ForStat(_)))?;
    if stats[for_index + 1..]
        .iter()
        .any(|stat| !matches!(stat, LuaStat::EmptyStat(_)))
        || stats[..for_index]
            .iter()
            .any(|stat| matches!(stat, LuaStat::ForStat(_)))
    {
        return None;
    }
    let LuaStat::ForStat(for_stat) = &stats[for_index] else {
        return None;
    };
    Some((&stats[..for_index], for_stat.clone()))
}

fn protected_pre_loop_names(
    helpers: &HashMap<String, LuaClosureExpr>,
    table_global: &str,
    param_name: &str,
    loop_var_name: &str,
    start_expr: &LuaExpr,
    end_expr: &LuaExpr,
) -> HashSet<String> {
    let mut names = helpers.keys().cloned().collect::<HashSet<_>>();
    names.insert(table_global.to_string());
    names.insert(param_name.to_string());
    names.insert(loop_var_name.to_string());
    collect_expr_name_texts(start_expr, &mut names);
    collect_expr_name_texts(end_expr, &mut names);
    names
}

fn collect_expr_name_texts(expr: &LuaExpr, names: &mut HashSet<String>) {
    for name_expr in expr.descendants::<LuaNameExpr>() {
        if let Some(name) = name_expr.get_name_text() {
            names.insert(name);
        }
    }
}

fn pre_loop_stat_is_harmless(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    containing_closure: &LuaClosureExpr,
    stat: &LuaStat,
    helpers: &HashMap<String, LuaClosureExpr>,
    protected_names: &HashSet<String>,
) -> bool {
    match stat {
        LuaStat::EmptyStat(_) => true,
        LuaStat::LocalStat(local_stat) => pre_loop_local_stat_is_harmless(
            db,
            cache,
            file_id,
            containing_closure,
            local_stat,
            helpers,
            protected_names,
        ),
        _ => false,
    }
}

fn pre_loop_local_stat_is_harmless(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    containing_closure: &LuaClosureExpr,
    local_stat: &glua_parser::LuaLocalStat,
    helpers: &HashMap<String, LuaClosureExpr>,
    protected_names: &HashSet<String>,
) -> bool {
    for local_name in local_stat.get_local_name_list() {
        let Some(name_token) = local_name.get_name_token() else {
            return false;
        };
        if protected_names.contains(name_token.get_name_text()) {
            return false;
        }
    }

    local_stat.get_value_exprs().all(|expr| {
        pre_loop_expr_is_harmless(
            db,
            cache,
            file_id,
            containing_closure,
            &expr,
            helpers,
            protected_names,
            &mut HashSet::new(),
        )
    })
}

fn pre_loop_expr_is_harmless(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    containing_closure: &LuaClosureExpr,
    expr: &LuaExpr,
    helpers: &HashMap<String, LuaClosureExpr>,
    protected_names: &HashSet<String>,
    active_helpers: &mut HashSet<String>,
) -> bool {
    expr.descendants::<LuaCallExpr>().all(|call_expr| {
        is_node_in_nested_closure(call_expr.syntax(), expr.syntax())
            || pre_loop_call_is_harmless(
                db,
                cache,
                file_id,
                containing_closure,
                &call_expr,
                helpers,
                protected_names,
                active_helpers,
            )
    })
}

fn pre_loop_call_is_harmless(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    containing_closure: &LuaClosureExpr,
    call_expr: &LuaCallExpr,
    helpers: &HashMap<String, LuaClosureExpr>,
    protected_names: &HashSet<String>,
    active_helpers: &mut HashSet<String>,
) -> bool {
    if side_effect_free_call_is_safe(db, cache, call_expr, &mut HashSet::new()) {
        return true;
    }
    let Some(helper_name) = call_expr_name(call_expr) else {
        return false;
    };
    if call_name_shadowed_in_closure_before_call(containing_closure, &helper_name, call_expr) {
        return false;
    }
    let Some(helper_closure) = helpers.get(&helper_name) else {
        return false;
    };
    pre_loop_helper_body_is_safe(
        db,
        cache,
        file_id,
        &helper_name,
        helper_closure,
        helpers,
        protected_names,
        active_helpers,
    )
}

fn pre_loop_helper_body_is_safe(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    helper_name: &str,
    helper_closure: &LuaClosureExpr,
    helpers: &HashMap<String, LuaClosureExpr>,
    protected_names: &HashSet<String>,
    active_helpers: &mut HashSet<String>,
) -> bool {
    if !active_helpers.insert(helper_name.to_string()) {
        return false;
    }
    let Some(block) = helper_closure.get_block() else {
        active_helpers.remove(helper_name);
        return false;
    };

    for assign_stat in block.syntax().descendants().filter_map(LuaAssignStat::cast) {
        if is_node_in_nested_closure(assign_stat.syntax(), block.syntax()) {
            continue;
        }
        let (vars, _) = assign_stat.get_var_and_expr_list();
        for var in vars {
            match var {
                LuaVarExpr::IndexExpr(_) => {
                    active_helpers.remove(helper_name);
                    return false;
                }
                LuaVarExpr::NameExpr(name_expr) => {
                    if name_expr.get_name_text().is_none_or(|name| {
                        protected_names.contains(&name)
                            || !name_expr_resolves_to_local(db, file_id, &name_expr)
                    }) {
                        active_helpers.remove(helper_name);
                        return false;
                    }
                }
            }
        }
    }

    for call_expr in block.syntax().descendants().filter_map(LuaCallExpr::cast) {
        if is_node_in_nested_closure(call_expr.syntax(), block.syntax()) {
            continue;
        }
        if !pre_loop_call_is_harmless(
            db,
            cache,
            file_id,
            helper_closure,
            &call_expr,
            helpers,
            protected_names,
            active_helpers,
        ) {
            active_helpers.remove(helper_name);
            return false;
        }
    }

    active_helpers.remove(helper_name);
    true
}

fn loop_contains_rejected_control_flow(for_block: &LuaBlock) -> bool {
    for stat in for_block.syntax().descendants().filter_map(LuaStat::cast) {
        if is_node_in_nested_closure(stat.syntax(), for_block.syntax()) {
            continue;
        }
        match stat {
            LuaStat::ReturnStat(_)
            | LuaStat::WhileStat(_)
            | LuaStat::RepeatStat(_)
            | LuaStat::ForStat(_) => return true,
            LuaStat::BreakStat(ref break_stat) if !is_continue_stat(break_stat) => return true,
            _ => {}
        }
    }
    false
}

fn is_continue_stat(break_stat: &LuaBreakStat) -> bool {
    break_stat.syntax().text().to_string().trim() == "continue"
}

fn final_loop_target_assignment(stats: &[LuaStat]) -> Option<LuaAssignStat> {
    let last_non_empty = stats
        .iter()
        .rev()
        .find(|stat| !matches!(stat, LuaStat::EmptyStat(_)))?;
    let LuaStat::AssignStat(assign_stat) = last_non_empty else {
        return None;
    };
    Some(assign_stat.clone())
}

fn branchy_loop_writes_target_on_continue_paths(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    stats: &[LuaStat],
    param_name: &str,
    loop_var_name: &str,
    helpers: &HashMap<String, LuaClosureExpr>,
    protected_names: &HashSet<String>,
) -> bool {
    stats.iter().all(|stat| {
        branchy_stat_writes_target_before_continue(
            db,
            cache,
            file_id,
            stat,
            param_name,
            loop_var_name,
            helpers,
            protected_names,
            false,
        )
    })
}

fn branchy_stat_writes_target_before_continue(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    stat: &LuaStat,
    param_name: &str,
    loop_var_name: &str,
    helpers: &HashMap<String, LuaClosureExpr>,
    protected_names: &HashSet<String>,
    target_written: bool,
) -> bool {
    match stat {
        LuaStat::AssignStat(assign_stat) => branchy_assignment_is_safe(
            db,
            cache,
            file_id,
            assign_stat,
            param_name,
            loop_var_name,
            helpers,
            protected_names,
        ),
        LuaStat::BreakStat(break_stat) => !is_continue_stat(break_stat) || target_written,
        LuaStat::IfStat(if_stat) => if_stat_continue_paths_are_safe(
            db,
            cache,
            file_id,
            if_stat,
            param_name,
            loop_var_name,
            helpers,
            protected_names,
            target_written,
        ),
        LuaStat::LocalStat(local_stat) => {
            branchy_local_stat_is_safe(db, cache, local_stat, protected_names)
        }
        LuaStat::EmptyStat(_) => true,
        LuaStat::CallExprStat(_) => false,
        _ => false,
    }
}

fn branchy_local_stat_is_safe(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    local_stat: &glua_parser::LuaLocalStat,
    protected_names: &HashSet<String>,
) -> bool {
    for local_name in local_stat.get_local_name_list() {
        let Some(name_token) = local_name.get_name_token() else {
            return false;
        };
        if protected_names.contains(name_token.get_name_text()) {
            return false;
        }
    }

    local_stat.get_value_exprs().all(|expr| {
        let mut active_calls = HashSet::new();
        expr_calls_are_side_effect_free(db, cache, &expr, &mut active_calls)
    })
}

fn branchy_assignment_is_safe(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    assign_stat: &LuaAssignStat,
    param_name: &str,
    loop_var_name: &str,
    helpers: &HashMap<String, LuaClosureExpr>,
    protected_names: &HashSet<String>,
) -> bool {
    if assignment_writes_target_with_non_nil_rhs(
        db,
        cache,
        file_id,
        assign_stat,
        param_name,
        loop_var_name,
        helpers,
    ) {
        return true;
    }

    let (vars, exprs) = assign_stat.get_var_and_expr_list();
    if vars.is_empty() {
        return false;
    }
    for var in vars {
        let LuaVarExpr::NameExpr(name_expr) = var else {
            return false;
        };
        if name_expr.get_name_text().is_none_or(|name| {
            protected_names.contains(&name) || !name_expr_resolves_to_local(db, file_id, &name_expr)
        }) {
            return false;
        }
    }
    exprs.iter().all(|expr| {
        let mut active_calls = HashSet::new();
        expr_calls_are_side_effect_free(db, cache, expr, &mut active_calls)
    })
}

fn if_stat_continue_paths_are_safe(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    if_stat: &LuaIfStat,
    param_name: &str,
    loop_var_name: &str,
    helpers: &HashMap<String, LuaClosureExpr>,
    protected_names: &HashSet<String>,
    target_written: bool,
) -> bool {
    if !condition_calls_are_safe(db, cache, if_stat.get_condition_expr()) {
        return false;
    }
    if !block_continue_paths_are_safe(
        db,
        cache,
        file_id,
        if_stat.get_block(),
        param_name,
        loop_var_name,
        helpers,
        protected_names,
        target_written,
    ) {
        return false;
    }
    for else_if in if_stat.get_else_if_clause_list() {
        if !condition_calls_are_safe(db, cache, else_if.get_condition_expr())
            || !block_continue_paths_are_safe(
                db,
                cache,
                file_id,
                else_if.get_block(),
                param_name,
                loop_var_name,
                helpers,
                protected_names,
                target_written,
            )
        {
            return false;
        }
    }
    if let Some(else_clause) = if_stat.get_else_clause() {
        block_continue_paths_are_safe(
            db,
            cache,
            file_id,
            else_clause.get_block(),
            param_name,
            loop_var_name,
            helpers,
            protected_names,
            target_written,
        )
    } else {
        true
    }
}

fn condition_calls_are_safe(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    condition: Option<LuaExpr>,
) -> bool {
    condition.is_none_or(|condition| {
        let mut active_calls = HashSet::new();
        expr_calls_are_side_effect_free(db, cache, &condition, &mut active_calls)
    })
}

fn block_continue_paths_are_safe(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    block: Option<LuaBlock>,
    param_name: &str,
    loop_var_name: &str,
    helpers: &HashMap<String, LuaClosureExpr>,
    protected_names: &HashSet<String>,
    mut target_written: bool,
) -> bool {
    let Some(block) = block else {
        return true;
    };
    for stat in block.get_stats() {
        if !branchy_stat_writes_target_before_continue(
            db,
            cache,
            file_id,
            &stat,
            param_name,
            loop_var_name,
            helpers,
            protected_names,
            target_written,
        ) {
            return false;
        }
        if let LuaStat::AssignStat(assign_stat) = &stat
            && assignment_writes_target_with_non_nil_rhs(
                db,
                cache,
                file_id,
                assign_stat,
                param_name,
                loop_var_name,
                helpers,
            )
        {
            target_written = true;
        }
    }
    true
}

fn assignment_writes_target_with_non_nil_rhs(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    assign_stat: &LuaAssignStat,
    param_name: &str,
    loop_var_name: &str,
    helpers: &HashMap<String, LuaClosureExpr>,
) -> bool {
    let (vars, exprs) = assign_stat.get_var_and_expr_list();
    let ([LuaVarExpr::IndexExpr(index_expr)], [rhs_expr]) = (vars.as_slice(), exprs.as_slice())
    else {
        return false;
    };
    if !index_expr_matches_target(index_expr, param_name, loop_var_name) {
        return false;
    }
    early_continue_rhs_calls_are_safe(db, cache, rhs_expr)
        && inferred_rhs_is_non_nil(db, cache, file_id, rhs_expr, helpers)
}

fn early_continue_rhs_calls_are_safe(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    rhs_expr: &LuaExpr,
) -> bool {
    let mut active_calls = HashSet::new();
    expr_calls_are_side_effect_free(db, cache, rhs_expr, &mut active_calls)
}

fn index_expr_matches_target(
    index_expr: &glua_parser::LuaIndexExpr,
    param_name: &str,
    loop_var_name: &str,
) -> bool {
    let Some(LuaExpr::NameExpr(prefix_name)) = index_expr.get_prefix_expr() else {
        return false;
    };
    if prefix_name.get_name_text().as_deref() != Some(param_name) {
        return false;
    }
    let Some(LuaIndexKey::Expr(LuaExpr::NameExpr(key_name))) = index_expr.get_index_key() else {
        return false;
    };
    key_name.get_name_text().as_deref() == Some(loop_var_name)
}

fn inferred_rhs_is_non_nil(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    rhs_expr: &LuaExpr,
    helpers: &HashMap<String, LuaClosureExpr>,
) -> bool {
    let mut rhs_type = infer_expr(db, cache, rhs_expr.clone()).ok().or_else(|| {
        infer_tracked_safe_helper_call_return_type(db, cache, file_id, rhs_expr, helpers)
    });
    if rhs_type
        .as_ref()
        .is_some_and(|ty| ty.is_nullable() || ty.is_nil())
    {
        rhs_type =
            infer_tracked_safe_helper_call_return_type(db, cache, file_id, rhs_expr, helpers);
    }
    rhs_type.is_some_and(|ty| !ty.is_nullable() && !ty.is_nil())
}

fn infer_tracked_safe_helper_call_return_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    rhs_expr: &LuaExpr,
    helpers: &HashMap<String, LuaClosureExpr>,
) -> Option<LuaType> {
    // Only recover a non-null RHS type for tracked helpers whose bodies have already
    // passed the numeric-range population safety checks. This is intentionally not a
    // general local return-flow inference path.
    let LuaExpr::CallExpr(call_expr) = rhs_expr else {
        return None;
    };
    let helper_name = call_expr_name(call_expr)?;
    let LuaExpr::NameExpr(name_expr) = call_expr.get_prefix_expr()? else {
        return None;
    };
    if !name_expr_resolves_to_local(db, file_id, &name_expr) {
        return None;
    }

    let helper_closure = helpers.get(&helper_name)?;
    let block = helper_closure.get_block()?;
    let args = call_expr
        .get_args_list()
        .map(|args| args.get_args().collect::<Vec<_>>())
        .unwrap_or_default();
    let params = helper_closure
        .get_params_list()
        .map(|params| params.get_params().collect::<Vec<_>>())
        .unwrap_or_default();
    let param_names = params
        .iter()
        .filter_map(|param| {
            param
                .get_name_token()
                .map(|token| token.get_name_text().to_string())
        })
        .collect::<HashSet<_>>();
    if helper_body_mutates_or_shadows_params(block.syntax(), &param_names) {
        return None;
    }
    let mut return_types = Vec::new();
    for return_stat in block
        .syntax()
        .descendants()
        .filter_map(glua_parser::LuaReturnStat::cast)
    {
        if is_node_in_nested_closure(return_stat.syntax(), block.syntax()) {
            continue;
        }
        let return_exprs = return_stat.get_expr_list().collect::<Vec<_>>();
        let [return_expr] = return_exprs.as_slice() else {
            return None;
        };
        let return_type =
            infer_safe_helper_return_expr_type(db, cache, return_expr, &params, &args)?;
        if return_type.is_nullable() || return_type.is_nil() {
            return None;
        }
        return_types.push(return_type);
    }

    if return_types.is_empty() {
        None
    } else {
        Some(LuaType::from_vec(return_types))
    }
}

fn helper_body_mutates_or_shadows_params(
    block: &LuaSyntaxNode,
    param_names: &HashSet<String>,
) -> bool {
    if param_names.is_empty() {
        return false;
    }

    for assign_stat in block.descendants().filter_map(LuaAssignStat::cast) {
        if is_node_in_nested_closure(assign_stat.syntax(), block) {
            continue;
        }
        let (vars, _) = assign_stat.get_var_and_expr_list();
        if vars.into_iter().any(|var| {
            matches!(var, LuaVarExpr::NameExpr(name_expr) if name_expr
                .get_name_text()
                .is_some_and(|name| param_names.contains(&name)))
        }) {
            return true;
        }
    }

    for local_stat in block.descendants().filter_map(LuaLocalStat::cast) {
        if is_node_in_nested_closure(local_stat.syntax(), block) {
            continue;
        }
        if local_stat.get_local_name_list().any(|local_name| {
            local_name
                .get_name_token()
                .is_some_and(|token| param_names.contains(token.get_name_text()))
        }) {
            return true;
        }
    }

    false
}

fn infer_safe_helper_return_expr_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    return_expr: &LuaExpr,
    params: &[LuaParamName],
    args: &[LuaExpr],
) -> Option<LuaType> {
    if let LuaExpr::NameExpr(name_expr) = return_expr {
        let name = name_expr.get_name_text()?;
        if let Some(arg_index) = params.iter().position(|param| {
            param
                .get_name_token()
                .is_some_and(|token| token.get_name_text() == name)
        }) {
            return infer_expr(db, cache, args.get(arg_index)?.clone()).ok();
        }
    }

    if let LuaExpr::CallExpr(call_expr) = return_expr
        && let Some(signature_id) =
            side_effect_free_call_metadata_signature_id(db, cache, call_expr)
    {
        return db
            .get_signature_index()
            .get(&signature_id)
            .map(|signature| signature.get_return_type());
    }

    infer_expr(db, cache, return_expr.clone()).ok()
}

fn integer_const_expr_value(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: &LuaExpr,
) -> Option<i64> {
    match infer_expr(db, cache, expr.clone()).ok()? {
        LuaType::IntegerConst(value) | LuaType::DocIntegerConst(value) => Some(value),
        _ => None,
    }
}

fn numeric_range_rhs_is_safe(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    fill_closure: &LuaClosureExpr,
    rhs_expr: &LuaExpr,
    helpers: &HashMap<String, LuaClosureExpr>,
    table_global: &str,
) -> bool {
    let calls = rhs_expr.descendants::<LuaCallExpr>().collect::<Vec<_>>();
    if calls.is_empty() {
        return true;
    }

    let mut active_helpers = HashSet::new();
    calls.into_iter().all(|call_expr| {
        numeric_range_call_is_safe(
            db,
            cache,
            file_id,
            fill_closure,
            &call_expr,
            helpers,
            table_global,
            &mut active_helpers,
        )
    })
}

fn numeric_range_call_is_safe(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    containing_closure: &LuaClosureExpr,
    call_expr: &LuaCallExpr,
    helpers: &HashMap<String, LuaClosureExpr>,
    table_global: &str,
    active_helpers: &mut HashSet<String>,
) -> bool {
    if let Some(helper_name) = call_expr_name(call_expr)
        && call_expr.get_prefix_expr().is_some_and(|prefix| {
            matches!(
                prefix,
                LuaExpr::NameExpr(ref name_expr)
                    if name_expr_resolves_to_local(db, file_id, name_expr)
            )
        })
    {
        if let Some(helper_closure) = helpers.get(&helper_name) {
            return !call_name_shadowed_in_closure_before_call(
                containing_closure,
                &helper_name,
                call_expr,
            ) && call_args_are_side_effect_free(db, cache, call_expr)
                && rhs_helper_body_is_safe(
                    db,
                    cache,
                    file_id,
                    &helper_name,
                    helper_closure,
                    helpers,
                    table_global,
                    active_helpers,
                );
        }
    }

    side_effect_free_call_is_safe(db, cache, call_expr, &mut HashSet::new())
}

fn call_args_are_side_effect_free(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> bool {
    call_expr.get_args_list().is_none_or(|args| {
        let mut active_calls = HashSet::new();
        args.get_args()
            .all(|arg| expr_calls_are_side_effect_free(db, cache, &arg, &mut active_calls))
    })
}

fn rhs_helper_body_is_safe(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    helper_name: &str,
    helper_closure: &LuaClosureExpr,
    helpers: &HashMap<String, LuaClosureExpr>,
    table_global: &str,
    active_helpers: &mut HashSet<String>,
) -> bool {
    if !active_helpers.insert(helper_name.to_string()) {
        return false;
    }

    let Some(block) = helper_closure.get_block() else {
        active_helpers.remove(helper_name);
        return false;
    };

    for call_expr in block.syntax().descendants().filter_map(LuaCallExpr::cast) {
        if is_node_in_nested_closure(call_expr.syntax(), block.syntax()) {
            continue;
        }
        if !numeric_range_call_is_safe(
            db,
            cache,
            file_id,
            helper_closure,
            &call_expr,
            helpers,
            table_global,
            active_helpers,
        ) {
            active_helpers.remove(helper_name);
            return false;
        }
    }

    let mut protected_names = helpers.keys().map(String::as_str).collect::<HashSet<_>>();
    protected_names.insert(table_global);

    for assign_stat in block.syntax().descendants().filter_map(LuaAssignStat::cast) {
        if is_node_in_nested_closure(assign_stat.syntax(), block.syntax()) {
            continue;
        }
        let (vars, _) = assign_stat.get_var_and_expr_list();
        if vars
            .iter()
            .any(|var| matches!(var, LuaVarExpr::IndexExpr(_)))
        {
            active_helpers.remove(helper_name);
            return false;
        }
        if vars.into_iter().any(|var| {
            var_writes_protected_numeric_range_identity(db, file_id, &var, &protected_names)
        }) {
            active_helpers.remove(helper_name);
            return false;
        }
    }

    for local_stat in block.syntax().descendants().filter_map(LuaLocalStat::cast) {
        if is_node_in_nested_closure(local_stat.syntax(), block.syntax()) {
            continue;
        }
        if local_shadows_protected_numeric_range_identity(&local_stat, &protected_names) {
            active_helpers.remove(helper_name);
            return false;
        }
    }

    active_helpers.remove(helper_name);
    true
}

fn side_effect_free_call_is_safe(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
    active_calls: &mut HashSet<TextRange>,
) -> bool {
    let call_range = call_expr.syntax().text_range();
    if !active_calls.insert(call_range) {
        return false;
    }

    let is_safe = side_effect_free_call_metadata_signature_id(db, cache, call_expr).is_some()
        && call_expr.get_args_list().is_none_or(|args| {
            args.get_args()
                .all(|arg| expr_calls_are_side_effect_free(db, cache, &arg, active_calls))
        });

    active_calls.remove(&call_range);
    is_safe
}

fn expr_calls_are_side_effect_free(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    expr: &LuaExpr,
    active_calls: &mut HashSet<TextRange>,
) -> bool {
    expr.descendants::<LuaCallExpr>().all(|nested_call| {
        is_node_in_nested_closure(nested_call.syntax(), expr.syntax())
            || side_effect_free_call_is_safe(db, cache, &nested_call, active_calls)
    })
}

fn side_effect_free_call_metadata_signature_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> Option<LuaSignatureId> {
    let (signature_id, semantic_decl) =
        side_effect_free_call_signature_and_decl(db, cache, call_expr)?;
    if signature_is_side_effect_free(db, signature_id)
        || semantic_decl_has_side_effect_free_attribute(db, semantic_decl)
    {
        Some(signature_id)
    } else {
        None
    }
}

fn side_effect_free_call_signature_and_decl(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> Option<(LuaSignatureId, crate::LuaSemanticDeclId)> {
    let prefix_expr = call_expr.get_prefix_expr()?;
    if let LuaExpr::NameExpr(name_expr) = &prefix_expr
        && let Some(signature_id) = get_local_name_signature_id(db, cache, name_expr)
    {
        return Some((
            signature_id,
            crate::LuaSemanticDeclId::Signature(signature_id),
        ));
    }

    let semantic_decl = infer_expr_semantic_decl(
        db,
        cache,
        prefix_expr,
        SemanticDeclGuard::default(),
        SemanticDeclLevel::default(),
    )?;
    Some((
        get_signature_id_from_semantic_decl_value_expr(db, semantic_decl.clone())?,
        semantic_decl,
    ))
}

fn semantic_decl_has_side_effect_free_attribute(
    db: &DbIndex,
    semantic_decl: crate::LuaSemanticDeclId,
) -> bool {
    db.get_property_index()
        .get_property(&semantic_decl)
        .is_some_and(|property| {
            property
                .find_attribute_use(GMOD_ATTR_SIDE_EFFECT_FREE)
                .is_some()
        })
}

fn get_local_name_signature_id(
    db: &DbIndex,
    cache: &LuaInferCache,
    name_expr: &LuaNameExpr,
) -> Option<LuaSignatureId> {
    let decl_id = db
        .get_reference_index()
        .get_var_reference_decl(&cache.get_file_id(), name_expr.get_range())?;
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let value_syntax_id = decl.get_value_syntax_id()?;
    let root = db.get_vfs().get_syntax_tree(&decl.get_file_id())?;
    let closure = LuaExpr::cast(value_syntax_id.to_node_from_root(&root.get_red_root())?)?;
    let LuaExpr::ClosureExpr(closure) = closure else {
        return None;
    };
    Some(LuaSignatureId::from_closure(decl.get_file_id(), &closure))
}

fn get_signature_id_from_semantic_decl_value_expr(
    db: &DbIndex,
    semantic_decl: crate::LuaSemanticDeclId,
) -> Option<LuaSignatureId> {
    if let Some(signature_id) = db.get_property_index().get_signature_owner(&semantic_decl) {
        return Some(signature_id);
    }
    let file_id = match semantic_decl {
        crate::LuaSemanticDeclId::LuaDecl(decl_id) => {
            if let Some(LuaType::Signature(signature_id)) = db
                .get_type_index()
                .get_type_cache(&decl_id.into())
                .map(|type_cache| type_cache.as_type())
            {
                return Some(*signature_id);
            }
            decl_id.file_id
        }
        crate::LuaSemanticDeclId::Member(member_id) => {
            if let Some(LuaType::Signature(signature_id)) = db
                .get_type_index()
                .get_type_cache(&member_id.into())
                .map(|type_cache| type_cache.as_type())
            {
                return Some(*signature_id);
            }
            member_id.file_id
        }
        crate::LuaSemanticDeclId::Signature(signature_id) => return Some(signature_id),
        crate::LuaSemanticDeclId::TypeDecl(_) => return None,
    };
    let LuaExpr::ClosureExpr(closure) = get_semantic_decl_value_expr(db, semantic_decl)? else {
        return None;
    };
    Some(LuaSignatureId::from_closure(file_id, &closure))
}

fn get_semantic_decl_value_expr(
    db: &DbIndex,
    semantic_decl: crate::LuaSemanticDeclId,
) -> Option<LuaExpr> {
    match semantic_decl {
        crate::LuaSemanticDeclId::LuaDecl(decl_id) => {
            let decl = db.get_decl_index().get_decl(&decl_id)?;
            let value_syntax_id = decl.get_value_syntax_id()?;
            let root = db.get_vfs().get_syntax_tree(&decl.get_file_id())?;
            LuaExpr::cast(value_syntax_id.to_node_from_root(&root.get_red_root())?)
        }
        crate::LuaSemanticDeclId::Member(member_id) => get_member_value_expr(db, member_id),
        crate::LuaSemanticDeclId::Signature(_) | crate::LuaSemanticDeclId::TypeDecl(_) => None,
    }
}

fn local_shadows_protected_numeric_range_identity(
    local_stat: &LuaLocalStat,
    protected_names: &HashSet<&str>,
) -> bool {
    local_stat.get_local_name_list().any(|local_name| {
        local_name
            .get_name_token()
            .is_some_and(|name| protected_names.contains(name.get_name_text()))
    })
}

fn var_writes_protected_numeric_range_identity(
    db: &DbIndex,
    file_id: FileId,
    var: &LuaVarExpr,
    protected_names: &HashSet<&str>,
) -> bool {
    match var {
        LuaVarExpr::NameExpr(name_expr) => name_expr.get_name_text().is_some_and(|name| {
            protected_names.contains(name.as_str())
                || !name_expr_resolves_to_local(db, file_id, name_expr)
        }),
        LuaVarExpr::IndexExpr(index_expr) => index_expr.get_prefix_expr().is_some_and(|prefix| {
            expr_reads_protected_numeric_range_identity(&prefix, protected_names)
        }),
    }
}

fn expr_reads_protected_numeric_range_identity(
    expr: &LuaExpr,
    protected_names: &HashSet<&str>,
) -> bool {
    expr.descendants::<LuaNameExpr>().any(|name_expr| {
        name_expr
            .get_name_text()
            .is_some_and(|name| protected_names.contains(name.as_str()))
    })
}
