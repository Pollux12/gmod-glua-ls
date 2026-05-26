use rustc_hash::{FxHashMap, FxHashSet};
use std::time::Duration;

use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaForRangeStat, LuaFuncStat,
    LuaIndexKey, LuaSyntaxKind, LuaSyntaxNode, LuaTableExpr, LuaTableField, LuaVarExpr, PathTrait,
};
use smol_str::SmolStr;

use crate::{
    InFiled, LuaMemberKey, LuaSignatureId, LuaType, VarRefId,
    db_index::{DbIndex, DynamicFieldOwner},
    profile::Profile,
    semantic::{
        find_members_with_key, get_var_expr_var_ref_id, infer_expr, unwrap_paren_to_name_expr,
    },
};

use super::{AnalysisPipeline, AnalyzeContext};

/// Cache key for prefix type inference in dynamic field analysis.
/// Uses VarRefId when available (same variable at different positions hits cache),
/// falls back to TextRange for unnamable expressions (table constructors, etc.).
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum PrefixCacheKey {
    Var(VarRefId),
    Range(rowan::TextRange),
}

impl PrefixCacheKey {
    fn from_expr(db: &DbIndex, cache: &mut crate::LuaInferCache, expr: &LuaExpr) -> Self {
        match get_var_expr_var_ref_id(db, cache, expr.clone()) {
            Some(var_ref_id) => PrefixCacheKey::Var(var_ref_id),
            None => PrefixCacheKey::Range(expr.syntax().text_range()),
        }
    }
}

pub struct DynamicFieldAnalysisPipeline;

impl AnalysisPipeline for DynamicFieldAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        analyze_dynamic_fields(db, context, DynamicFieldAnalysisMode::Full);
    }
}

pub struct EarlyDynamicFieldAnalysisPipeline;

impl AnalysisPipeline for EarlyDynamicFieldAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        analyze_dynamic_fields(db, context, DynamicFieldAnalysisMode::Early);
    }
}

#[derive(Clone, Copy)]
enum DynamicFieldAnalysisMode {
    Early,
    Full,
}

#[derive(Debug, Clone, Copy)]
struct FieldSetterHelper {
    table_param_index: usize,
    key_param_index: usize,
}

#[derive(Default)]
struct FieldSetterHelperCache {
    helpers: FxHashMap<LuaSignatureId, Vec<FieldSetterHelper>>,
    non_helpers: FxHashSet<LuaSignatureId>,
    member_names: FxHashSet<SmolStr>,
}

impl FieldSetterHelperCache {
    fn from_tree_list(
        tree_list: &[InFiled<glua_parser::LuaChunk>],
        enable_member_name_prefilter: bool,
    ) -> Self {
        let (helpers, mut member_names) = collect_field_setter_helpers(tree_list);
        if !enable_member_name_prefilter {
            member_names.clear();
        }
        Self {
            helpers,
            non_helpers: FxHashSet::default(),
            member_names,
        }
    }

    fn patterns_for_signature(
        &mut self,
        db: &DbIndex,
        signature_id: LuaSignatureId,
    ) -> Vec<FieldSetterHelper> {
        if let Some(patterns) = self.helpers.get(&signature_id) {
            return patterns.clone();
        }

        if self.non_helpers.contains(&signature_id) {
            return Vec::new();
        }

        let patterns = collect_field_setter_helpers_for_signature(db, signature_id);
        if patterns.is_empty() {
            self.non_helpers.insert(signature_id);
        } else {
            self.helpers.insert(signature_id, patterns.clone());
        }

        patterns
    }

    fn definitely_not_member_helper_call(&self, prefix_expr: &LuaExpr) -> bool {
        if self.member_names.is_empty() {
            return false;
        }

        let LuaExpr::IndexExpr(index_expr) = prefix_expr else {
            return false;
        };
        let Some(member_name) = simple_index_key_name(index_expr) else {
            return false;
        };

        !self.member_names.contains(&member_name)
    }
}

impl DynamicFieldAnalysisMode {
    fn collect_declared_member_table_fields(self) -> bool {
        true
    }

    fn collect_direct_assignments(self) -> bool {
        matches!(self, Self::Full)
    }

    fn collect_setmetatable_tables(self) -> bool {
        matches!(self, Self::Full)
    }

    fn propagate_to_super_types(self) -> bool {
        matches!(self, Self::Full)
    }

    fn collects_only_declared_member_table_fields(self) -> bool {
        self.collect_declared_member_table_fields()
            && !self.collect_direct_assignments()
            && !self.collect_setmetatable_tables()
            && !self.propagate_to_super_types()
    }
}

fn analyze_dynamic_fields(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    mode: DynamicFieldAnalysisMode,
) {
    let _p = Profile::cond_new("dynamic field analyze", context.tree_list.len() > 1);
    let tree_list = context.tree_list.clone();
    let mut collected: Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)> =
        Vec::new();
    let mut collected_wildcards: Vec<(DynamicFieldOwner, crate::FileId, rowan::TextRange)> =
        Vec::new();
    let profile_enabled = log::log_enabled!(log::Level::Info);
    let mut profile = profile_enabled.then(DynamicFieldProfile::default);
    let mut field_setter_helpers = if mode.collect_direct_assignments() {
        FieldSetterHelperCache::from_tree_list(
            &tree_list,
            context_covers_workspace(&*db, context, &tree_list),
        )
    } else {
        FieldSetterHelperCache::default()
    };

    for in_filed_tree in &tree_list {
        let root = in_filed_tree.value.clone();
        let file_id = in_filed_tree.file_id;
        let cache = context.infer_manager.get_infer_cache(file_id);
        let mut prefix_type_cache: FxHashMap<PrefixCacheKey, Option<LuaType>> =
            FxHashMap::default();
        for assign in root.descendants::<LuaAssignStat>() {
            if let Some(profile) = profile.as_mut() {
                profile.assignments_scanned += 1;
            }
            let (vars, exprs) = assign.get_var_and_expr_list();
            for (idx, var) in vars.iter().enumerate() {
                if let Some(profile) = profile.as_mut() {
                    profile.vars_scanned += 1;
                }
                let LuaVarExpr::IndexExpr(index_expr) = var else {
                    continue;
                };
                if let Some(profile) = profile.as_mut() {
                    profile.index_candidates += 1;
                }
                let value_expr = exprs.get(idx);
                if mode.collects_only_declared_member_table_fields()
                    && !matches!(value_expr, Some(LuaExpr::TableExpr(_)))
                {
                    continue;
                }
                let Some(prefix_expr) = index_expr.get_prefix_expr() else {
                    continue;
                };
                let cache_key = PrefixCacheKey::from_expr(&*db, cache, &prefix_expr);
                let prefix_type = if let Some(cached_type) = prefix_type_cache.get(&cache_key) {
                    if let Some(profile) = profile.as_mut() {
                        profile.owner_cache_hits += 1;
                    }
                    match cached_type {
                        Some(prefix_type) => prefix_type.clone(),
                        None => continue,
                    }
                } else {
                    if let Some(profile) = profile.as_mut() {
                        profile.owner_cache_misses += 1;
                    }
                    let infer_start = profile_enabled.then(std::time::Instant::now);
                    let inferred = infer_expr(&*db, cache, prefix_expr.clone()).ok();
                    if let (Some(profile), Some(infer_start)) = (profile.as_mut(), infer_start) {
                        profile.owner_infer_time += infer_start.elapsed();
                    }
                    prefix_type_cache.insert(cache_key, inferred.clone());
                    let Some(prefix_type) = inferred else {
                        continue;
                    };
                    prefix_type
                };

                let effective_type = if let Some(metatable_type) = infer_setmetatable_target_type(
                    &*db,
                    cache,
                    &prefix_expr,
                    index_expr.get_range(),
                ) {
                    metatable_type
                } else {
                    prefix_type
                };

                let Some(definition_range) = index_expr.get_index_key().and_then(|k| k.get_range())
                else {
                    continue;
                };

                let field_names = get_field_names(db, cache, &index_expr);
                if field_names.is_empty() {
                    if let Some(profile) = profile.as_mut() {
                        profile.no_field_name_skips += 1;
                    }
                    if mode.collect_direct_assignments() && is_dynamic_index_key(&index_expr) {
                        collect_wildcard_for_type(
                            &effective_type,
                            file_id,
                            definition_range,
                            &mut collected_wildcards,
                        );
                    }
                    continue;
                };

                for field_name in field_names {
                    if let Some(profile) = profile.as_mut() {
                        profile.fields_collected += 1;
                    }
                    if mode.collect_declared_member_table_fields()
                        && let Some(value_expr) = value_expr
                    {
                        collect_assigned_table_fields_for_declared_member(
                            &*db,
                            cache,
                            &effective_type,
                            &field_name,
                            value_expr,
                            file_id,
                            &mut collected,
                        );
                    }
                    if mode.collect_direct_assignments() {
                        collect_for_type(
                            &effective_type,
                            &field_name,
                            file_id,
                            definition_range,
                            &mut collected,
                        );
                    }
                }
            }
        }

        if mode.collect_setmetatable_tables() {
            for call_expr in root.descendants::<LuaCallExpr>() {
                if let Some(profile) = profile.as_mut() {
                    profile.calls_scanned += 1;
                }
                collect_field_setter_helper_call_fields(
                    &*db,
                    cache,
                    &call_expr,
                    file_id,
                    &mut field_setter_helpers,
                    &mut collected,
                );
                collect_setmetatable_table_fields(&*db, cache, &call_expr, file_id, &mut collected);
            }
        }
    }

    let propagate_start = profile_enabled.then(std::time::Instant::now);
    // Propagate dynamic fields to parent types so that e.g. a field assigned
    // on `base_glide` (which extends `Entity`) is also visible when the variable
    // is typed as `Entity`.  This avoids false-positive `undefined-field` when
    // user code accesses entity fields through a base-class reference.
    let mut propagated: Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)> =
        Vec::new();
    if mode.propagate_to_super_types() {
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
                    if let Some(profile) = profile.as_mut() {
                        profile.fields_propagated += 1;
                    }
                }
            }
        }
    }
    if let (Some(profile), Some(propagate_start)) = (profile.as_mut(), propagate_start) {
        profile.propagation_time += propagate_start.elapsed();
    }

    let insert_start = profile_enabled.then(std::time::Instant::now);
    let index = db.get_dynamic_field_index_mut();
    for (owner, field_name, file_id, range) in &collected {
        index.add_field(owner.clone(), field_name.clone(), *file_id, *range);
    }
    for (owner, field_name, file_id, range) in &propagated {
        index.add_field(owner.clone(), field_name.clone(), *file_id, *range);
    }
    for (owner, file_id, range) in &collected_wildcards {
        index.add_wildcard_definition(owner.clone(), *file_id, *range);
    }
    if let (Some(profile), Some(insert_start)) = (profile.as_mut(), insert_start) {
        profile.insertion_time += insert_start.elapsed();
    }
    if let Some(profile) = profile {
        profile.log(tree_list.len(), collected.len(), propagated.len());
    }
}

fn collect_field_setter_helpers(
    tree_list: &[InFiled<glua_parser::LuaChunk>],
) -> (
    FxHashMap<LuaSignatureId, Vec<FieldSetterHelper>>,
    FxHashSet<SmolStr>,
) {
    let mut helpers: FxHashMap<LuaSignatureId, Vec<FieldSetterHelper>> = FxHashMap::default();
    let mut member_names = FxHashSet::default();

    for in_filed_tree in tree_list {
        let file_id = in_filed_tree.file_id;
        let root = in_filed_tree.value.clone();
        for closure in root.descendants::<LuaClosureExpr>() {
            let signature_id = LuaSignatureId::from_closure(file_id, &closure);
            let patterns = collect_field_setter_helpers_in_closure(&closure);
            if !patterns.is_empty() {
                helpers.insert(signature_id, patterns);
                collect_helper_member_names_for_closure(&closure, &mut member_names);
            }
        }
    }

    (helpers, member_names)
}

fn context_covers_workspace(
    db: &DbIndex,
    context: &AnalyzeContext,
    tree_list: &[InFiled<glua_parser::LuaChunk>],
) -> bool {
    let Some(workspace_id) = context.workspace_id else {
        return false;
    };

    let tree_file_ids = tree_list
        .iter()
        .map(|in_filed_tree| in_filed_tree.file_id)
        .collect::<FxHashSet<_>>();
    let mut found_workspace_file = false;
    for file_id in db.get_vfs().get_all_file_ids() {
        if db.get_vfs().get_syntax_tree(&file_id).is_none() {
            continue;
        }

        if db.get_module_index().get_workspace_id(file_id) != Some(workspace_id) {
            continue;
        }

        found_workspace_file = true;
        if !tree_file_ids.contains(&file_id) {
            return false;
        }
    }

    found_workspace_file
}

fn collect_helper_member_names_for_closure(
    closure: &LuaClosureExpr,
    member_names: &mut FxHashSet<SmolStr>,
) {
    for ancestor in closure.syntax().ancestors().skip(1) {
        if let Some(func_stat) = LuaFuncStat::cast(ancestor.clone()) {
            if let Some(func_name) = func_stat.get_func_name() {
                collect_helper_member_name_from_var(&func_name, member_names);
            }
            return;
        }

        if let Some(assign) = LuaAssignStat::cast(ancestor.clone()) {
            let (vars, exprs) = assign.get_var_and_expr_list();
            for (var, expr) in vars.iter().zip(exprs.iter()) {
                if expr.syntax() == closure.syntax() {
                    collect_helper_member_name_from_var(var, member_names);
                }
            }
            return;
        }

        if LuaTableField::can_cast(ancestor.kind().into()) {
            if let Some(field) = LuaTableField::cast(ancestor) {
                if let Some(field_key) = field.get_field_key() {
                    collect_helper_member_name_from_key(&field_key, member_names);
                }
            }
            return;
        }
    }
}

fn collect_helper_member_name_from_var(var: &LuaVarExpr, member_names: &mut FxHashSet<SmolStr>) {
    let LuaVarExpr::IndexExpr(index_expr) = var else {
        return;
    };
    if let Some(member_name) = simple_index_key_name(index_expr) {
        member_names.insert(member_name);
    }
}

fn collect_helper_member_name_from_key(key: &LuaIndexKey, member_names: &mut FxHashSet<SmolStr>) {
    if let Some(member_name) = simple_key_name(key) {
        member_names.insert(member_name);
    }
}

fn collect_field_setter_helpers_in_closure(closure: &LuaClosureExpr) -> Vec<FieldSetterHelper> {
    let Some(params_list) = closure.get_params_list() else {
        return Vec::new();
    };
    let param_names = params_list
        .get_params()
        .filter_map(|param| {
            param
                .get_name_token()
                .map(|token| token.get_name_text().to_string())
        })
        .collect::<Vec<_>>();
    if param_names.len() < 2 {
        return Vec::new();
    }

    let Some(block) = closure.get_block() else {
        return Vec::new();
    };

    let mut helpers = Vec::new();
    for assign in block.descendants::<LuaAssignStat>() {
        if assign.ancestors::<LuaClosureExpr>().next().as_ref() != Some(closure) {
            continue;
        }

        let (vars, _) = assign.get_var_and_expr_list();
        for var in vars.iter() {
            let LuaVarExpr::IndexExpr(index_expr) = var else {
                continue;
            };
            let Some(table_param_index) = index_expr
                .get_prefix_expr()
                .and_then(|expr| param_expr_index(&expr, &param_names))
            else {
                continue;
            };
            let Some(key_param_index) = index_expr
                .get_index_key()
                .and_then(|key| param_index_key_index(&key, &param_names))
            else {
                continue;
            };

            helpers.push(FieldSetterHelper {
                table_param_index,
                key_param_index,
            });
        }
    }

    helpers
}

fn collect_field_setter_helper_call_fields(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    call_expr: &LuaCallExpr,
    file_id: crate::FileId,
    helpers: &mut FieldSetterHelperCache,
    collected: &mut Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)>,
) {
    let Some(args_list) = call_expr.get_args_list() else {
        return;
    };
    let mut args_iter = args_list.get_args();
    let Some(first_arg) = args_iter.next() else {
        return;
    };
    let Some(second_arg) = args_iter.next() else {
        return;
    };
    let mut args = Vec::with_capacity(2 + args_iter.size_hint().0);
    args.push(first_arg);
    args.push(second_arg);
    args.extend(args_iter);

    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return;
    };
    let helper_patterns = helper_patterns_for_call(db, cache, &prefix_expr, helpers);
    if helper_patterns.is_empty() {
        return;
    };
    for helper in helper_patterns {
        let Some(table_arg) = args.get(helper.table_param_index) else {
            continue;
        };
        let Some(key_arg) = args.get(helper.key_param_index) else {
            continue;
        };
        let field_names = field_names_from_key_arg(db, cache, key_arg);
        if field_names.is_empty() {
            continue;
        }
        let definition_range = key_arg.syntax().text_range();
        let Ok(table_type) = infer_expr(db, cache, table_arg.clone()) else {
            continue;
        };

        for field_name in field_names {
            collect_for_type(
                &table_type,
                &field_name,
                file_id,
                definition_range,
                collected,
            );
        }
    }
}

fn field_names_from_key_arg(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    key_arg: &LuaExpr,
) -> Vec<SmolStr> {
    if let LuaExpr::LiteralExpr(literal_expr) = key_arg
        && let Some(glua_parser::LuaLiteralToken::String(string_token)) = literal_expr.get_literal()
    {
        return vec![string_token.get_value().into()];
    }

    string_const_names(&infer_expr(db, cache, key_arg.clone()).ok())
}

fn helper_patterns_for_call(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    prefix_expr: &LuaExpr,
    helpers: &mut FieldSetterHelperCache,
) -> Vec<FieldSetterHelper> {
    if let LuaExpr::NameExpr(name_expr) = prefix_expr
        && let Some(signature_id) = direct_name_expr_signature_id(db, cache, name_expr)
    {
        return helpers.patterns_for_signature(db, signature_id);
    }

    if helpers.definitely_not_member_helper_call(prefix_expr) {
        return Vec::new();
    }

    let Ok(prefix_type) = infer_expr(db, cache, prefix_expr.clone()) else {
        return Vec::new();
    };

    let mut result = Vec::new();
    collect_helper_patterns_from_type(db, &prefix_type, helpers, &mut result);
    result
}

fn collect_helper_patterns_from_type(
    db: &DbIndex,
    typ: &LuaType,
    helpers: &mut FieldSetterHelperCache,
    result: &mut Vec<FieldSetterHelper>,
) {
    match typ {
        LuaType::Signature(signature_id) => {
            result.extend(helpers.patterns_for_signature(db, *signature_id));
        }
        LuaType::Union(union_type) => {
            for typ in union_type.into_vec() {
                collect_helper_patterns_from_type(db, &typ, helpers, result);
            }
        }
        LuaType::TypeGuard(inner) => collect_helper_patterns_from_type(db, inner, helpers, result),
        _ => {}
    }
}

fn direct_name_expr_signature_id(
    db: &DbIndex,
    cache: &crate::LuaInferCache,
    name_expr: &glua_parser::LuaNameExpr,
) -> Option<LuaSignatureId> {
    let decl_id = db
        .get_reference_index()
        .get_var_reference_decl(&cache.get_file_id(), name_expr.get_range())?;
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let value_syntax_id = decl.get_value_syntax_id()?;
    let root = db.get_vfs().get_syntax_tree(&decl.get_file_id())?;
    let value_expr = LuaExpr::cast(value_syntax_id.to_node_from_root(&root.get_red_root())?)?;
    let LuaExpr::ClosureExpr(closure) = value_expr else {
        return None;
    };

    Some(LuaSignatureId::from_closure(decl.get_file_id(), &closure))
}

fn simple_index_key_name(index_expr: &glua_parser::LuaIndexExpr) -> Option<SmolStr> {
    simple_key_name(&index_expr.get_index_key()?)
}

fn simple_key_name(key: &LuaIndexKey) -> Option<SmolStr> {
    match key {
        LuaIndexKey::Name(name) => Some(name.get_name_text().into()),
        LuaIndexKey::String(string) => Some(string.get_value().into()),
        _ => None,
    }
}

fn collect_field_setter_helpers_for_signature(
    db: &DbIndex,
    signature_id: LuaSignatureId,
) -> Vec<FieldSetterHelper> {
    let Some(tree) = db.get_vfs().get_syntax_tree(&signature_id.get_file_id()) else {
        return Vec::new();
    };

    tree.get_chunk_node()
        .descendants::<LuaClosureExpr>()
        .find(|closure| closure.get_position() == signature_id.get_position())
        .map(|closure| collect_field_setter_helpers_in_closure(&closure))
        .unwrap_or_default()
}

fn param_index_key_index(key: &LuaIndexKey, param_names: &[String]) -> Option<usize> {
    match key {
        LuaIndexKey::Expr(expr) => param_expr_index(expr, param_names),
        _ => None,
    }
}

fn param_expr_index(expr: &LuaExpr, param_names: &[String]) -> Option<usize> {
    let LuaExpr::NameExpr(name_expr) = expr else {
        return None;
    };
    let name = name_expr.get_name_text()?;
    param_names
        .iter()
        .position(|param_name| param_name == &name)
}

#[derive(Default)]
struct DynamicFieldProfile {
    assignments_scanned: usize,
    vars_scanned: usize,
    index_candidates: usize,
    no_field_name_skips: usize,
    calls_scanned: usize,
    fields_collected: usize,
    fields_propagated: usize,
    owner_cache_hits: usize,
    owner_cache_misses: usize,
    owner_infer_time: Duration,
    propagation_time: Duration,
    insertion_time: Duration,
}

impl DynamicFieldProfile {
    fn log(&self, file_count: usize, collected: usize, propagated: usize) {
        log::info!(
            "dynamic field profile: files={} assignments={} vars={} index_candidates={} no_field_name_skips={} calls={} fields_collected={} collected_entries={} propagated={} owner_cache_hits={} owner_cache_misses={} owner_infer_time={:?} propagation_time={:?} insertion_time={:?}",
            file_count,
            self.assignments_scanned,
            self.vars_scanned,
            self.index_candidates,
            self.no_field_name_skips,
            self.calls_scanned,
            self.fields_collected,
            collected,
            propagated,
            self.owner_cache_hits,
            self.owner_cache_misses,
            self.owner_infer_time,
            self.propagation_time,
            self.insertion_time,
        );
    }
}

fn collect_setmetatable_table_fields(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    call_expr: &LuaCallExpr,
    file_id: crate::FileId,
    collected: &mut Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)>,
) {
    if !call_expr.is_setmetatable() {
        return;
    }

    let Some(arg_list) = call_expr.get_args_list() else {
        return;
    };
    let args = arg_list.get_args().collect::<Vec<_>>();
    if args.len() != 2 {
        return;
    }

    let LuaExpr::TableExpr(table_expr) = &args[0] else {
        return;
    };
    let Some(target_type) = infer_metatable_index_type_for_dynamic_field(db, cache, &args[1])
    else {
        return;
    };

    for field in table_expr.get_fields() {
        collect_nested_table_field(db, cache, &field, &target_type, file_id, collected);
    }
}

fn collect_nested_table_field(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    field: &LuaTableField,
    owner_type: &LuaType,
    file_id: crate::FileId,
    collected: &mut Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)>,
) {
    let Some(field_key) = field.get_field_key() else {
        return;
    };
    let field_names = match field_key {
        LuaIndexKey::Name(ref name) => vec![name.get_name_text().into()],
        LuaIndexKey::String(ref string) => vec![string.get_value().into()],
        LuaIndexKey::Expr(ref expr) => {
            string_const_names(&infer_expr(db, cache, expr.clone()).ok())
        }
        _ => Vec::new(),
    };
    if field_names.is_empty() {
        return;
    }

    let Some(definition_range) = field_key.get_range() else {
        return;
    };

    for field_name in field_names {
        collect_for_type(
            owner_type,
            &field_name,
            file_id,
            definition_range,
            collected,
        );
    }

    if let Some(LuaExpr::TableExpr(table_expr)) = field.get_value_expr() {
        let nested_owner = LuaType::TableConst(InFiled::new(file_id, table_expr.get_range()));
        for nested_field in table_expr.get_fields() {
            collect_nested_table_field(db, cache, &nested_field, &nested_owner, file_id, collected);
        }
    }
}

fn collect_assigned_table_fields_for_declared_member(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    owner_type: &LuaType,
    field_name: &SmolStr,
    value_expr: &LuaExpr,
    file_id: crate::FileId,
    collected: &mut Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)>,
) {
    let LuaExpr::TableExpr(table_expr) = value_expr else {
        return;
    };

    let Some(member_infos) =
        find_members_with_key(db, owner_type, LuaMemberKey::Name(field_name.clone()), true)
    else {
        return;
    };

    for member_info in member_infos {
        for field in table_expr.get_fields() {
            collect_nested_table_field(db, cache, &field, &member_info.typ, file_id, collected);
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
    if matches!(prefix_var_ref_id, VarRefId::GlobalName(_, _)) {
        return collect_setmetatable_bindings(db, cache, prefix_expr, prefix_var_ref_id)
            .into_iter()
            .take_while(|(range, _)| range.end() <= assignment_range.start())
            .last()
            .map(|(_, target_type)| target_type);
    }

    if !cache
        .dynamic_field_metatable_cache
        .contains_key(&prefix_var_ref_id)
    {
        let bindings =
            collect_setmetatable_bindings(db, cache, prefix_expr, prefix_var_ref_id.clone());
        cache
            .dynamic_field_metatable_cache
            .insert(prefix_var_ref_id.clone(), bindings);
    }

    cache
        .dynamic_field_metatable_cache
        .get(&prefix_var_ref_id)?
        .iter()
        .take_while(|(range, _)| range.end() <= assignment_range.start())
        .last()
        .map(|(_, target_type)| target_type.clone())
}

fn collect_setmetatable_bindings(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    prefix_expr: &LuaExpr,
    prefix_var_ref_id: VarRefId,
) -> Vec<(rowan::TextRange, LuaType)> {
    let Some(scope) = nearest_dynamic_field_binding_scope(prefix_expr.syntax()) else {
        return Vec::new();
    };

    let mut bindings = Vec::new();
    for node in scope.descendants() {
        let Some(call_expr) = LuaCallExpr::cast(node) else {
            continue;
        };
        let Some(call_scope) = nearest_dynamic_field_binding_scope(call_expr.syntax()) else {
            continue;
        };
        if call_scope != scope {
            continue;
        }

        if !call_expr.is_setmetatable() {
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
            bindings.push((call_expr.get_range(), target_type));
        }
    }

    bindings.sort_by_key(|(range, _)| range.start());
    bindings
}

fn nearest_dynamic_field_binding_scope(node: &LuaSyntaxNode) -> Option<LuaSyntaxNode> {
    node.ancestors().find(|ancestor| {
        matches!(
            ancestor.kind().into(),
            LuaSyntaxKind::Chunk
                | LuaSyntaxKind::ClosureExpr
                | LuaSyntaxKind::FuncStat
                | LuaSyntaxKind::LocalFuncStat
        )
    })
}

fn infer_metatable_index_type_for_dynamic_field(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    metatable_expr: &LuaExpr,
) -> Option<LuaType> {
    if let Some(name_expr) = unwrap_paren_to_name_expr(metatable_expr)
        && name_expr.get_name_text().as_deref() == Some("self")
    {
        if let Some(self_type) = infer_enclosing_method_self_type(db, cache, metatable_expr) {
            if self_type.is_custom_type() {
                return Some(self_type);
            }

            if let Some(index_type) = infer_index_type_from_metatable_type(db, &self_type) {
                return Some(index_type);
            }
        }
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
        LuaIndexKey::Expr(expr) => {
            let names = string_const_names(&infer_expr(db, cache, expr.clone()).ok());
            if names.is_empty() {
                field_names_from_for_range_pairs_key(expr)
            } else {
                names
            }
        }
        _ => Vec::new(),
    }
}

fn field_names_from_for_range_pairs_key(key_expr: LuaExpr) -> Vec<SmolStr> {
    let LuaExpr::NameExpr(name_expr) = key_expr else {
        return Vec::new();
    };
    let Some(name_text) = name_expr.get_name_text() else {
        return Vec::new();
    };
    let Some(for_range) = name_expr
        .syntax()
        .ancestors()
        .find_map(LuaForRangeStat::cast)
    else {
        return Vec::new();
    };

    let is_first_iter_var = for_range
        .get_var_name_list()
        .next()
        .is_some_and(|iter_name| iter_name.get_name_text() == name_text);
    if !is_first_iter_var {
        return Vec::new();
    }

    let mut iter_exprs = for_range.get_expr_list();
    let Some(LuaExpr::CallExpr(call_expr)) = iter_exprs.next() else {
        return Vec::new();
    };
    if iter_exprs.next().is_some() || call_expr.get_access_path().as_deref() != Some("pairs") {
        return Vec::new();
    }

    let Some(args_list) = call_expr.get_args_list() else {
        return Vec::new();
    };
    let mut args = args_list.get_args();
    let Some(LuaExpr::TableExpr(table_expr)) = args.next() else {
        return Vec::new();
    };
    if args.next().is_some() {
        return Vec::new();
    }

    field_names_from_table_expr_keys(&table_expr)
}

fn field_names_from_table_expr_keys(table_expr: &LuaTableExpr) -> Vec<SmolStr> {
    table_expr
        .get_fields()
        .filter_map(|field| {
            let field_key = field.get_field_key()?;
            match field_key {
                LuaIndexKey::Name(name) => Some(name.get_name_text().into()),
                LuaIndexKey::String(string) => Some(string.get_value().into()),
                _ => None,
            }
        })
        .collect()
}

fn is_dynamic_index_key(index_expr: &glua_parser::LuaIndexExpr) -> bool {
    matches!(index_expr.get_index_key(), Some(LuaIndexKey::Expr(_)))
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

fn collect_wildcard_for_type(
    typ: &LuaType,
    file_id: crate::FileId,
    range: rowan::TextRange,
    result: &mut Vec<(DynamicFieldOwner, crate::FileId, rowan::TextRange)>,
) {
    match typ {
        LuaType::Ref(id) | LuaType::Def(id) => {
            result.push((DynamicFieldOwner::Type(id.clone()), file_id, range));
        }
        LuaType::TableConst(table_range) => {
            result.push((
                DynamicFieldOwner::Table(table_range.clone()),
                file_id,
                range,
            ));
        }
        LuaType::Instance(instance) => {
            collect_wildcard_for_type(instance.get_base(), file_id, range, result);
        }
        LuaType::TableOf(inner) => {
            collect_wildcard_for_type(inner, file_id, range, result);
        }
        LuaType::Union(union_type) => {
            for t in union_type.into_vec() {
                collect_wildcard_for_type(&t, file_id, range, result);
            }
        }
        _ => {}
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
