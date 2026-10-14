use rustc_hash::{FxHashMap, FxHashSet};

use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAstNode, LuaAstToken, LuaCallExpr, LuaClosureExpr, LuaExpr,
    LuaForRangeStat, LuaForStat, LuaFuncStat, LuaIndexKey, LuaLiteralToken, LuaSyntaxId,
    LuaSyntaxKind, LuaSyntaxNode, LuaTableExpr, LuaTableField, LuaVarExpr, PathTrait,
};
use rowan::TextSize;
use smol_str::SmolStr;

use crate::{
    InFiled, LuaDeclId, LuaMemberId, LuaMemberKey, LuaSignatureId, LuaType, LuaTypeOwner, VarRefId,
    db_index::{DbIndex, DynamicFieldOwner, LuaMemberOwner, LuaSemanticDeclId, WorkspaceKind},
    find_signature_attribute_use,
    semantic::{
        contains_gmod_null_type, declaring_for_range, find_members_with_key,
        get_var_expr_var_ref_id, infer_expr, raw_get_access_path,
        try_infer_enclosing_for_range_iter_type, unwrap_paren_to_name_expr,
    },
};

/// The reference a write's prefix names: its variable when it names one
/// (the same variable at different positions is one reference), else the
/// prefix expression itself.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum PrefixKey {
    Var(VarRefId),
    Range(rowan::TextRange),
}

impl PrefixKey {
    fn from_expr(db: &DbIndex, cache: &mut crate::LuaInferCache, expr: &LuaExpr) -> Self {
        match get_var_expr_var_ref_id(db, cache, expr.clone()) {
            Some(var_ref_id) => PrefixKey::Var(var_ref_id),
            None => PrefixKey::Range(expr.syntax().text_range()),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum DynamicFieldAnalysisMode {
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct FieldSetterHelper {
    table_param_index: usize,
    key_param_index: usize,
}

#[derive(Default)]
struct ResolvedDynamicFieldNames {
    names: Vec<SmolStr>,
    may_have_other_string_names: bool,
}

struct ForRangePairsFieldNames {
    names: Vec<SmolStr>,
    iter_decl_id: LuaDeclId,
    may_have_other_string_names: bool,
}

const BUILTIN_ALIAS_ATTRIBUTE: &str = "builtin_alias";

type FiniteNamedMemberEvidence = (
    DynamicFieldOwner,
    LuaMemberId,
    crate::FileId,
    rowan::TextRange,
    bool,
);

fn patterns_for_signature(db: &DbIndex, signature_id: LuaSignatureId) -> Vec<FieldSetterHelper> {
    super::function_summary::function_summaries(db, signature_id.get_file_id())
        .and_then(|summaries| {
            summaries
                .field_setter_helpers
                .get(&signature_id.get_position())
                .cloned()
        })
        .unwrap_or_default()
}

impl DynamicFieldAnalysisMode {
    fn collect_declared_member_table_fields(self) -> bool {
        true
    }

    fn collect_direct_assignments(self) -> bool {
        matches!(self, Self::Full)
    }

    fn collect_finite_direct_assignments(self) -> bool {
        true
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

/// Whether a node is a statement that can define fields.
fn defines_fields(kind: glua_parser::LuaKind) -> bool {
    matches!(
        kind.into(),
        LuaSyntaxKind::AssignStat | LuaSyntaxKind::FuncStat
    )
}

/// A statement's indexed targets, each with its position among the
/// statement's assigned variables: what its syntax alone says can define a
/// field.
fn indexed_targets(node: &LuaSyntaxNode) -> Vec<(usize, glua_parser::LuaIndexExpr)> {
    if let Some(assign) = LuaAssignStat::cast(node.clone()) {
        return assign
            .get_var_and_expr_list()
            .0
            .into_iter()
            .enumerate()
            .filter_map(|(var, target)| match target {
                LuaVarExpr::IndexExpr(index_expr) => Some((var, index_expr)),
                _ => None,
            })
            .collect();
    }
    if let Some(func_stat) = LuaFuncStat::cast(node.clone())
        && let Some(LuaVarExpr::IndexExpr(index_expr)) = func_stat.get_func_name()
        && index_expr
            .get_index_token()
            .is_some_and(|token| !token.is_colon())
    {
        return vec![(0, index_expr)];
    }
    Vec::new()
}

/// The field-definition sites a statement contributes, as `(position among
/// its assigned variables, indexed target, assigned value)`. A function
/// statement defines a field only where its name is no member yet.
fn field_definition_sites(
    db: &DbIndex,
    file_id: crate::FileId,
    node: &LuaSyntaxNode,
) -> Vec<(usize, glua_parser::LuaIndexExpr, Option<LuaExpr>)> {
    if let Some(assign) = LuaAssignStat::cast(node.clone()) {
        let exprs = assign.get_var_and_expr_list().1;
        return indexed_targets(node)
            .into_iter()
            .map(|(var, index_expr)| (var, index_expr, exprs.get(var).cloned()))
            .collect();
    }
    indexed_targets(node)
        .into_iter()
        .filter(|(_, index_expr)| {
            db.get_member_index()
                .get_current_owner(&LuaMemberId::new(index_expr.get_syntax_id(), file_id))
                .is_none()
        })
        .map(|(var, index_expr)| (var, index_expr, None))
        .collect()
}

/// Whether a node is a write site of its own: a statement with an indexed
/// target, or a call with two or more arguments, which can set fields
/// through `setmetatable` or a setter helper.
pub(super) fn is_write_site(node: &LuaSyntaxNode) -> bool {
    if let Some(call) = LuaCallExpr::cast(node.clone()) {
        return call
            .get_args_list()
            .is_some_and(|args| args.get_args().nth(1).is_some());
    }
    defines_fields(node.kind()) && !indexed_targets(node).is_empty()
}

/// One file's indexed write targets and setmetatable calls, read off its
/// syntax once per analysis pass: a pass rebuilds no source layer, so the
/// file's syntax, declarations and local references hold for all of it.
#[derive(Default)]
pub(crate) struct FileWriteSites {
    /// Every indexed target of an assignment or function statement, in the
    /// order a walk of the file meets them.
    targets: Vec<WriteTarget>,
    /// The setmetatable calls of each binding scope, in tree order.
    setmetatables: FxHashMap<LuaSyntaxId, Vec<LuaSyntaxId>>,
    /// The positions each local of the file is reassigned at, sorted.
    reassignments: std::sync::OnceLock<FxHashMap<LuaDeclId, Vec<TextSize>>>,
}

/// An indexed target of an assignment or function statement.
struct WriteTarget {
    statement: LuaSyntaxId,
    /// The target's position among the statement's assigned variables.
    var: usize,
    /// The class of the target's prefix: two prefixes can name the same
    /// variable only within one class. `None` for a prefix that names none.
    prefix: Option<u32>,
}

impl FileWriteSites {
    pub(crate) fn new(root: &glua_parser::LuaChunk) -> Self {
        let mut classes = FxHashMap::<String, u32>::default();
        let mut targets = Vec::new();
        for node in root.syntax().descendants_matching(defines_fields) {
            let statement = LuaSyntaxId::from_node(&node);
            for (var, index_expr) in indexed_targets(&node) {
                let prefix = index_expr
                    .get_prefix_expr()
                    .as_ref()
                    .and_then(prefix_class)
                    .map(|class| {
                        let next = classes.len() as u32;
                        *classes.entry(class).or_insert(next)
                    });
                targets.push(WriteTarget {
                    statement,
                    var,
                    prefix,
                });
            }
        }
        let mut setmetatables = FxHashMap::<LuaSyntaxId, Vec<LuaSyntaxId>>::default();
        for call in root.descendants::<LuaCallExpr>() {
            if !call.is_setmetatable() {
                continue;
            }
            if let Some(scope) = nearest_dynamic_field_binding_scope(call.syntax()) {
                setmetatables
                    .entry(LuaSyntaxId::from_node(&scope))
                    .or_default()
                    .push(call.get_syntax_id());
            }
        }
        Self {
            targets,
            setmetatables,
            reassignments: std::sync::OnceLock::new(),
        }
    }

    /// Where the target at position `var` of `statement` is among the
    /// targets.
    fn target_at(&self, statement: LuaSyntaxId, var: usize) -> Option<usize> {
        let order = |target: &WriteTarget| (target.statement.get_range().start(), target.var);
        let key = (statement.get_range().start(), var);
        let at = self.targets.partition_point(|target| order(target) < key);
        self.targets
            .get(at)
            .is_some_and(|target| target.statement == statement && target.var == var)
            .then_some(at)
    }

    fn reassignments(
        &self,
        db: &DbIndex,
        file_id: crate::FileId,
        root: &glua_parser::LuaChunk,
    ) -> &FxHashMap<LuaDeclId, Vec<TextSize>> {
        self.reassignments
            .get_or_init(|| collect_local_reassignment_positions(db, file_id, root))
    }
}

/// What a prefix's text says of the variable it names: a name, or an access
/// path. Two prefixes that name the same variable have the same class; `None`
/// for a prefix that names no variable.
fn prefix_class(prefix: &LuaExpr) -> Option<String> {
    match prefix {
        LuaExpr::NameExpr(name_expr) => Some(format!("name {}", name_expr.get_name_text()?)),
        LuaExpr::IndexExpr(index_expr) => Some(format!("path {}", index_expr.get_access_path()?)),
        LuaExpr::CallExpr(call_expr) => Some(format!("path {}", raw_get_access_path(call_expr)?)),
        _ => None,
    }
}

/// A target the pass collects from: what it read of the target before
/// inferring its prefix.
struct ReachedTarget {
    prefix_expr: LuaExpr,
    definition_range: rowan::TextRange,
    field_names: ResolvedDynamicFieldNames,
    collect_finite_direct_assignment: bool,
    should_collect_wildcard: bool,
}

/// The target `index_expr`, assigned `value_expr`, when the pass collects
/// from it: it names fields, or writes through a key that may name any.
fn reach_target(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    root: &glua_parser::LuaChunk,
    sites: &FileWriteSites,
    mode: DynamicFieldAnalysisMode,
    index_expr: &glua_parser::LuaIndexExpr,
    value_expr: Option<&LuaExpr>,
) -> Option<ReachedTarget> {
    let prefix_expr = index_expr.get_prefix_expr()?;
    let definition_range = index_expr.get_index_key().and_then(|k| k.get_range())?;
    if value_expr
        .is_some_and(|value_expr| copies_table_into_itself(db, cache, index_expr, value_expr))
    {
        return None;
    }
    let field_names = get_field_names(db, cache, root, sites, index_expr);
    let collect_finite_direct_assignment = mode.collect_finite_direct_assignments()
        && is_dynamic_index_key(index_expr)
        && !field_names.names.is_empty();
    if mode.collects_only_declared_member_table_fields()
        && !matches!(value_expr, Some(LuaExpr::TableExpr(_)))
        && !collect_finite_direct_assignment
    {
        return None;
    }
    let should_collect_wildcard = mode.collect_direct_assignments()
        && is_dynamic_index_key(index_expr)
        && field_names.may_have_other_string_names;
    if field_names.names.is_empty() && !should_collect_wildcard {
        return None;
    }
    Some(ReachedTarget {
        prefix_expr,
        definition_range,
        field_names,
        collect_finite_direct_assignment,
        should_collect_wildcard,
    })
}

/// The type every write through `key` takes its prefix to have: the type
/// inferred at the first target in tree order the pass collects from whose
/// prefix names that reference, the target at `at` included, unless that type
/// says nothing about the value, when this write's own prefix type is used. A
/// prefix that names no variable is a reference of its own.
fn shared_prefix_type(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    root: &glua_parser::LuaChunk,
    sites: &FileWriteSites,
    mode: DynamicFieldAnalysisMode,
    at: Option<usize>,
    key: &PrefixKey,
    prefix_expr: &LuaExpr,
) -> Option<LuaType> {
    let file_id = cache.get_file_id();
    if let (PrefixKey::Var(_), Some(at)) = (key, at)
        && let Some(class) = sites.targets[at].prefix
    {
        for earlier in sites.targets[..at]
            .iter()
            .filter(|target| target.prefix == Some(class))
        {
            let Some(node) = earlier.statement.to_node_from_root(root.syntax()) else {
                continue;
            };
            let Some((_, index_expr, value_expr)) = field_definition_sites(db, file_id, &node)
                .into_iter()
                .find(|(var, _, _)| *var == earlier.var)
            else {
                continue;
            };
            let Some(reached) = reach_target(
                db,
                cache,
                root,
                sites,
                mode,
                &index_expr,
                value_expr.as_ref(),
            ) else {
                continue;
            };
            if PrefixKey::from_expr(db, cache, &reached.prefix_expr) == *key {
                return infer_expr(db, cache, reached.prefix_expr)
                    .ok()
                    .filter(crate::db_index::is_informative_type)
                    .or_else(|| infer_expr(db, cache, prefix_expr.clone()).ok());
            }
        }
    }
    infer_expr(db, cache, prefix_expr.clone()).ok()
}

/// What one write site files: the fields it writes on each owner, those
/// propagated to the owners' super types, its wildcard definitions, its
/// finite-member evidence and the names it writes on no owner.
#[derive(Default)]
pub(super) struct SiteDynamicFields {
    named: Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)>,
    propagated: Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)>,
    wildcards: Vec<(DynamicFieldOwner, crate::FileId, rowan::TextRange)>,
    finite: Vec<FiniteNamedMemberEvidence>,
    unattributed: Vec<(SmolStr, rowan::TextRange)>,
}

/// The fields one statement's targets write. Reads only immutable
/// `&DbIndex` state plus the file's own AST, and writes nothing to the db.
/// Infers through `cache` under [`collecting_view`]; memos it holds from
/// other views are reused only where they consulted none of that view's
/// gates.
fn collect_statement_fields(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    root: &glua_parser::LuaChunk,
    sites: &FileWriteSites,
    mode: DynamicFieldAnalysisMode,
    node: &LuaSyntaxNode,
    fields: &mut SiteDynamicFields,
) {
    let file_id = cache.get_file_id();
    let statement = LuaSyntaxId::from_node(node);
    // The records are what this pass is building. A cold build has none yet,
    // and a re-index of some sites has the rest of the workspace's, with the
    // re-indexed sites' contributions removed: a partial record that names a
    // different table than the complete one will. Neither is read here, so
    // both builds decide the same way.
    for (var, index_expr, value_expr) in field_definition_sites(db, file_id, node) {
        let value_expr = value_expr.as_ref();
        let Some(ReachedTarget {
            prefix_expr,
            definition_range,
            field_names,
            collect_finite_direct_assignment,
            should_collect_wildcard,
        }) = reach_target(db, cache, root, sites, mode, &index_expr, value_expr)
        else {
            continue;
        };

        let key = PrefixKey::from_expr(db, cache, &prefix_expr);
        let at = sites.target_at(statement, var);
        let Some(prefix_type) =
            shared_prefix_type(db, cache, root, sites, mode, at, &key, &prefix_expr)
        else {
            continue;
        };

        let effective_type = if let Some(metatable_type) = infer_setmetatable_target_type(
            db,
            cache,
            root,
            sites,
            &prefix_expr,
            index_expr.get_range(),
        ) {
            metatable_type
        } else {
            prefix_type
        };
        if should_collect_wildcard {
            collect_wildcard_for_type(
                db,
                &effective_type,
                file_id,
                definition_range,
                &mut fields.wildcards,
            );
        }

        if is_dynamic_index_key(&index_expr) && !field_names.names.is_empty() {
            collect_finite_member_for_type(
                db,
                &effective_type,
                LuaMemberId::new(index_expr.get_syntax_id(), file_id),
                file_id,
                definition_range,
                !field_names.may_have_other_string_names,
                &mut fields.finite,
            );
        }

        if field_names.names.is_empty() {
            continue;
        };

        // Target-local buffers: super-type propagation runs per target.
        let mut target_named: Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)> =
            Vec::new();
        for field_name in field_names.names {
            if mode.collect_declared_member_table_fields()
                && let Some(value_expr) = value_expr
            {
                let member_id = LuaMemberId::new(index_expr.get_syntax_id(), file_id);
                collect_assigned_table_fields_for_declared_member(
                    db,
                    cache,
                    &effective_type,
                    &field_name,
                    member_id,
                    value_expr,
                    file_id,
                    &mut target_named,
                    &mut fields.finite,
                );
            }
            if mode.collect_direct_assignments() || collect_finite_direct_assignment {
                let before = target_named.len();
                collect_for_type(
                    db,
                    &effective_type,
                    &field_name,
                    file_id,
                    definition_range,
                    &mut target_named,
                );
                // The write is real but landed on no owner the index can
                // name, so record that the field exists somewhere even
                // though no table claims it.
                if target_named.len() == before {
                    fields
                        .unattributed
                        .push((field_name.clone(), definition_range));
                }
            }
        }
        if mode.propagate_to_super_types() {
            fields
                .propagated
                .extend(propagate_dynamic_fields_to_super_types(db, &target_named));
        }
        fields.named.extend(target_named);
    }
}

/// The fields one call sets through a setter helper or `setmetatable`.
fn collect_call_fields(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    mode: DynamicFieldAnalysisMode,
    call_expr: &LuaCallExpr,
    fields: &mut SiteDynamicFields,
) {
    if !mode.collect_setmetatable_tables() {
        return;
    }
    let file_id = cache.get_file_id();
    collect_field_setter_helper_call_fields(db, cache, call_expr, file_id, &mut fields.named);
    collect_setmetatable_table_fields(
        db,
        cache,
        call_expr,
        file_id,
        &mut fields.named,
        &mut fields.finite,
    );
    if mode.propagate_to_super_types() {
        fields.propagated = propagate_dynamic_fields_to_super_types(db, &fields.named);
    }
}

/// The view dynamic fields are collected under: the records this pass builds
/// are not read while building them.
fn collecting_view() -> crate::CacheOptions {
    crate::CacheOptions {
        analysis_phase: crate::LuaAnalysisPhase::Force,
        dynamic_fields_visible: false,
        call_site_params_visible: false,
    }
}

/// The fields the write site `site` of the cache's file writes, inferred
/// through `cache` (its memo) under [`collecting_view`]; the cache's own view
/// is restored afterwards.
pub(super) fn compute_site_dynamic_fields(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    sites: &FileWriteSites,
    site: LuaSyntaxId,
) -> SiteDynamicFields {
    let mut fields = SiteDynamicFields::default();
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&cache.get_file_id())
        .map(|tree| tree.get_chunk_node())
    else {
        return fields;
    };
    let Some(node) = site.to_node_from_root(root.syntax()) else {
        return fields;
    };
    let view = std::mem::replace(cache.config_mut(), collecting_view());
    let mode = DynamicFieldAnalysisMode::Full;
    match LuaCallExpr::cast(node.clone()) {
        Some(call_expr) => collect_call_fields(db, cache, mode, &call_expr, &mut fields),
        None => collect_statement_fields(db, cache, &root, sites, mode, &node, &mut fields),
    }
    *cache.config_mut() = view;
    fields
}

/// Stores the fields a write site found, filed under the site.
pub(super) fn apply_site_dynamic_fields(
    db: &mut DbIndex,
    site: &InFiled<rowan::TextRange>,
    fields: SiteDynamicFields,
) {
    let SiteDynamicFields {
        named,
        propagated,
        wildcards,
        finite,
        unattributed,
    } = fields;
    // Owners were canonicalized at push, and super-type propagation ran per
    // target.
    let index = db.get_dynamic_field_index_mut();
    for (field_name, range) in unattributed {
        index.add_unattributed_field(site, field_name, range);
    }
    for (owner, field_name, _, range) in named {
        index.add_field(site, owner, field_name, range);
    }
    for (owner, field_name, _, range) in propagated {
        index.add_propagated_field(site, owner, field_name, range);
    }
    let wildcard_definitions = wildcards.iter().cloned().collect::<FxHashSet<_>>();
    for (owner, _, range) in wildcards {
        index.add_wildcard_definition(site, owner, range);
    }
    for (owner, member_id, file_id, range, has_finite_domain) in finite {
        if has_finite_domain && !wildcard_definitions.contains(&(owner.clone(), file_id, range)) {
            index.add_finite_named_member(site, owner, member_id);
        } else {
            index.remove_finite_named_member(site, &owner, member_id);
        }
    }
}

pub(super) fn collect_field_setter_helpers(
    root: &glua_parser::LuaChunk,
) -> FxHashMap<TextSize, Vec<FieldSetterHelper>> {
    let mut helpers: FxHashMap<TextSize, Vec<FieldSetterHelper>> = FxHashMap::default();
    for assign in root.descendants::<LuaAssignStat>() {
        let Some(closure) = assign.ancestors::<LuaClosureExpr>().next() else {
            continue;
        };
        let Some(params_list) = closure.get_params_list() else {
            continue;
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

            helpers
                .entry(closure.get_position())
                .or_default()
                .push(FieldSetterHelper {
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

    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return;
    };
    let helper_patterns = helper_patterns_for_call(db, cache, &prefix_expr);
    if helper_patterns.is_empty() {
        return;
    };
    let mut args = Vec::with_capacity(2 + args_iter.size_hint().0);
    args.push(first_arg);
    args.push(second_arg);
    args.extend(args_iter);
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
                db,
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
) -> Vec<FieldSetterHelper> {
    if let LuaExpr::NameExpr(name_expr) = prefix_expr
        && let Some(signature_id) = direct_name_expr_signature_id(db, cache, name_expr)
    {
        return patterns_for_signature(db, signature_id);
    }

    helper_patterns_for_call_fallback(db, cache, prefix_expr)
}

fn helper_patterns_for_call_fallback(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    prefix_expr: &LuaExpr,
) -> Vec<FieldSetterHelper> {
    let Ok(prefix_type) = infer_expr(db, cache, prefix_expr.clone()) else {
        return Vec::new();
    };

    let mut result = Vec::new();
    collect_helper_patterns_from_type(db, &prefix_type, &mut result);
    result
}

fn collect_helper_patterns_from_type(
    db: &DbIndex,
    typ: &LuaType,
    result: &mut Vec<FieldSetterHelper>,
) {
    match typ {
        LuaType::Signature(signature_id) => {
            result.extend(patterns_for_signature(db, *signature_id));
        }
        LuaType::Union(union_type) => {
            for typ in union_type.types() {
                collect_helper_patterns_from_type(db, typ, result);
            }
        }
        LuaType::TypeGuard(inner) => collect_helper_patterns_from_type(db, inner, result),
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
    (value_syntax_id.get_kind() == LuaSyntaxKind::ClosureExpr)
        .then(|| LuaSignatureId::new(decl.get_file_id(), value_syntax_id.get_range().start()))
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
        .position(|param_name| *param_name == name)
}

fn collect_setmetatable_table_fields(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    call_expr: &LuaCallExpr,
    file_id: crate::FileId,
    collected: &mut Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)>,
    collected_finite_members: &mut Vec<FiniteNamedMemberEvidence>,
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
        collect_nested_table_field(
            db,
            cache,
            &field,
            &target_type,
            file_id,
            collected,
            collected_finite_members,
        );
    }
}

fn collect_nested_table_field(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    field: &LuaTableField,
    owner_type: &LuaType,
    file_id: crate::FileId,
    collected: &mut Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)>,
    collected_finite_members: &mut Vec<FiniteNamedMemberEvidence>,
) {
    let Some(field_key) = field.get_field_key() else {
        return;
    };
    let is_expression_key = matches!(field_key, LuaIndexKey::Expr(_));
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

    if is_expression_key {
        collect_finite_member_for_type(
            db,
            owner_type,
            LuaMemberId::new(field.get_syntax_id(), file_id),
            file_id,
            definition_range,
            true,
            collected_finite_members,
        );
    }

    for field_name in field_names {
        collect_for_type(
            db,
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
            collect_nested_table_field(
                db,
                cache,
                &nested_field,
                &nested_owner,
                file_id,
                collected,
                collected_finite_members,
            );
        }
    }
}

fn collect_assigned_table_fields_for_declared_member(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    owner_type: &LuaType,
    field_name: &SmolStr,
    member_id: LuaMemberId,
    value_expr: &LuaExpr,
    file_id: crate::FileId,
    collected: &mut Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)>,
    collected_finite_members: &mut Vec<FiniteNamedMemberEvidence>,
) {
    let LuaExpr::TableExpr(table_expr) = value_expr else {
        return;
    };

    let Some(member_types) = find_declared_member_types_for_dynamic_field(
        db,
        owner_type,
        LuaMemberKey::Name(field_name.clone()),
        member_id,
    ) else {
        return;
    };

    for member_type in member_types {
        for field in table_expr.get_fields() {
            collect_nested_table_field(
                db,
                cache,
                &field,
                &member_type,
                file_id,
                collected,
                collected_finite_members,
            );
        }
    }
}

fn find_declared_member_types_for_dynamic_field(
    db: &DbIndex,
    owner_type: &LuaType,
    key: LuaMemberKey,
    member_id: LuaMemberId,
) -> Option<Vec<LuaType>> {
    let current_member_type = db
        .get_type_index()
        .get_type_cache(&LuaTypeOwner::Member(member_id))
        .map(|cache| (cache.is_doc(), cache.as_type().clone()));

    if let Some((true, member_type)) = current_member_type.clone() {
        return Some(vec![member_type]);
    }

    if let LuaType::TableConst(table_range) = owner_type {
        if let Some((_, member_type)) = current_member_type {
            return Some(vec![member_type]);
        }

        let owner = LuaMemberOwner::Element(table_range.clone());
        let member_types = db
            .get_member_index()
            .get_members_for_owner_key(&owner, &key)
            .into_iter()
            .filter_map(|member| {
                db.get_type_index()
                    .get_type_cache(&LuaTypeOwner::Member(member.get_id()))
                    .map(|cache| cache.as_type().clone())
            })
            .collect::<Vec<_>>();
        return (!member_types.is_empty()).then_some(member_types);
    }

    if let Some(member_types) =
        find_members_with_key(db, owner_type, key, true).and_then(|member_infos| {
            let member_types = member_infos
                .into_iter()
                .map(|member_info| member_info.typ)
                .collect::<Vec<_>>();
            (!member_types.is_empty()).then_some(member_types)
        })
    {
        return Some(member_types);
    }

    current_member_type.map(|(_, member_type)| vec![member_type])
}

/// The `__index` type of the last `setmetatable` in the prefix's binding
/// scope that ends before the assignment and binds the prefix's variable.
fn infer_setmetatable_target_type(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    root: &glua_parser::LuaChunk,
    sites: &FileWriteSites,
    prefix_expr: &LuaExpr,
    assignment_range: rowan::TextRange,
) -> Option<LuaType> {
    let prefix_var_ref_id = get_var_expr_var_ref_id(db, cache, prefix_expr.clone())?;
    let scope = nearest_dynamic_field_binding_scope(prefix_expr.syntax())?;
    sites
        .setmetatables
        .get(&LuaSyntaxId::from_node(&scope))?
        .iter()
        .rev()
        .filter(|call| call.get_range().end() <= assignment_range.start())
        .filter_map(|call| LuaCallExpr::cast(call.to_node_from_root(root.syntax())?))
        .find_map(|call_expr| setmetatable_binding(db, cache, &call_expr, &prefix_var_ref_id))
}

/// The `__index` type a two-argument `setmetatable` call gives `var` when its
/// first argument is that variable.
fn setmetatable_binding(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    call_expr: &LuaCallExpr,
    var: &VarRefId,
) -> Option<LuaType> {
    let args = call_expr.get_args_list()?.get_args().collect::<Vec<_>>();
    if args.len() != 2 {
        return None;
    }
    if get_var_expr_var_ref_id(db, cache, args[0].clone())? != *var {
        return None;
    }
    infer_metatable_index_type_for_dynamic_field(db, cache, &args[1])
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
        LuaType::Union(union) => union.types().any(is_supported_metatable_index_type),
        LuaType::TypeGuard(inner) => is_supported_metatable_index_type(inner),
        LuaType::Instance(instance) => is_supported_metatable_index_type(instance.get_base()),
        _ => typ.is_table() || typ.is_custom_type() || typ.is_object(),
    }
}

fn get_field_names(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    root: &glua_parser::LuaChunk,
    sites: &FileWriteSites,
    index_expr: &glua_parser::LuaIndexExpr,
) -> ResolvedDynamicFieldNames {
    let Some(key) = index_expr.get_index_key() else {
        return ResolvedDynamicFieldNames::default();
    };
    match key {
        LuaIndexKey::Name(name) => ResolvedDynamicFieldNames {
            names: vec![name.get_name_text().into()],
            may_have_other_string_names: false,
        },
        LuaIndexKey::String(s) => ResolvedDynamicFieldNames {
            names: vec![s.get_value().into()],
            may_have_other_string_names: false,
        },
        LuaIndexKey::Expr(expr) => {
            let for_range_names = field_names_from_for_range_pairs_key(db, cache, expr.clone());
            if let Some(for_range_names) = for_range_names {
                let local_reassignment_positions =
                    sites.reassignments(db, cache.get_file_id(), root);
                let key_position = index_expr
                    .get_index_key()
                    .and_then(|key| key.get_range())
                    .map(|range| range.start())
                    .unwrap_or_else(|| index_expr.get_position());
                return ResolvedDynamicFieldNames {
                    names: for_range_names.names,
                    may_have_other_string_names: for_range_names.may_have_other_string_names
                        || has_local_reassignment_before(
                            local_reassignment_positions,
                            for_range_names.iter_decl_id,
                            key_position,
                        ),
                };
            }

            let names = string_const_names(&computed_key_type(db, cache, &expr));
            if !names.is_empty() {
                return ResolvedDynamicFieldNames {
                    names,
                    // Inferred literal names can come from non-exhaustive call-site evidence.
                    // Keep the wildcard unless syntax proves the key's full finite domain.
                    may_have_other_string_names: true,
                };
            }

            let names = {
                let mut visiting = FxHashSet::default();
                finite_dynamic_key_strings(db, cache, &expr, &mut visiting)
            };
            ResolvedDynamicFieldNames {
                may_have_other_string_names: names.is_empty(),
                names,
            }
        }
        _ => ResolvedDynamicFieldNames::default(),
    }
}

/// A computed key's type as this pass may read it. A loop variable's stored
/// type was settled with the fields this pass builds visible: read back, a
/// field it named would name itself again after whatever first gave it was
/// gone. So its type is inferred from its loop's iterator, under this pass's
/// view.
fn computed_key_type(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    expr: &LuaExpr,
) -> Option<LuaType> {
    if let LuaExpr::NameExpr(name_expr) = expr
        && let Some(decl_id) = db
            .get_reference_index()
            .get_local_reference(&cache.get_file_id())
            .and_then(|references| references.get_decl_id(&name_expr.get_range()))
        && declaring_for_range(name_expr.syntax(), decl_id).is_some()
    {
        return try_infer_enclosing_for_range_iter_type(db, cache, name_expr.syntax(), decl_id);
    }
    infer_expr(db, cache, expr.clone()).ok()
}

fn collect_local_reassignment_positions(
    db: &DbIndex,
    file_id: crate::FileId,
    root: &glua_parser::LuaChunk,
) -> FxHashMap<LuaDeclId, Vec<TextSize>> {
    let Some(decl_tree) = db.get_decl_index().get_decl_tree(&file_id) else {
        return FxHashMap::default();
    };

    let references = db.get_reference_index().get_local_reference(&file_id);
    let mut positions: FxHashMap<LuaDeclId, Vec<TextSize>> = FxHashMap::default();
    for assign in root.descendants::<LuaAssignStat>() {
        let position = assign.get_position();
        for var in assign.get_var_and_expr_list().0 {
            let LuaVarExpr::NameExpr(name_expr) = var else {
                continue;
            };
            let assigned_decl_id = references
                .and_then(|refs| refs.get_decl_id(&name_expr.get_range()))
                .or_else(|| {
                    let name = name_expr.get_name_text()?;
                    decl_tree
                        .find_local_decl(&name, name_expr.get_position())
                        .map(|decl| decl.get_id())
                });
            let Some(assigned_decl_id) = assigned_decl_id else {
                continue;
            };
            if assigned_decl_id.file_id != file_id || position <= assigned_decl_id.position {
                continue;
            }

            positions
                .entry(assigned_decl_id)
                .or_default()
                .push(position);
        }
    }

    for decl_positions in positions.values_mut() {
        decl_positions.sort_unstable();
        decl_positions.dedup();
    }
    positions
}

fn has_local_reassignment_before(
    local_reassignment_positions: &FxHashMap<LuaDeclId, Vec<TextSize>>,
    decl_id: LuaDeclId,
    query_position: TextSize,
) -> bool {
    local_reassignment_positions
        .get(&decl_id)
        .and_then(|positions| positions.first())
        .is_some_and(|position| *position < query_position)
}

fn finite_dynamic_key_strings(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    expr: &LuaExpr,
    visiting: &mut FxHashSet<LuaDeclId>,
) -> Vec<SmolStr> {
    let inferred = string_const_names(&computed_key_type(db, cache, expr));
    if !inferred.is_empty() {
        return inferred;
    }

    match expr {
        LuaExpr::LiteralExpr(literal) => match literal.get_literal() {
            Some(LuaLiteralToken::String(value)) => vec![value.get_value().into()],
            _ => Vec::new(),
        },
        LuaExpr::ParenExpr(paren) => paren
            .get_expr()
            .map(|inner| finite_dynamic_key_strings(db, cache, &inner, visiting))
            .unwrap_or_default(),
        LuaExpr::NameExpr(name_expr) => {
            let Some(decl_id) = db
                .get_reference_index()
                .get_local_reference(&cache.get_file_id())
                .and_then(|references| references.get_decl_id(&name_expr.get_range()))
            else {
                return Vec::new();
            };
            if !visiting.insert(decl_id) {
                return Vec::new();
            }
            let result = local_initializer_expr(db, decl_id)
                .map(|initializer| finite_dynamic_key_strings(db, cache, &initializer, visiting))
                .unwrap_or_default();
            visiting.remove(&decl_id);
            result
        }
        LuaExpr::BinaryExpr(binary) => {
            if binary.get_op_token().map(|token| token.get_op()) != Some(BinaryOperator::OpConcat) {
                return Vec::new();
            }
            let Some((left, right)) = binary.get_exprs() else {
                return Vec::new();
            };
            let left = finite_dynamic_key_strings(db, cache, &left, visiting);
            let right = finite_dynamic_key_strings(db, cache, &right, visiting);
            concatenate_finite_strings(&left, &right)
        }
        LuaExpr::CallExpr(call) => finite_string_transform_call(db, cache, call, visiting),
        LuaExpr::IndexExpr(index_expr) => {
            finite_numeric_loop_table_values(db, cache, index_expr, visiting)
        }
        _ => Vec::new(),
    }
}

fn local_initializer_expr(db: &DbIndex, decl_id: LuaDeclId) -> Option<LuaExpr> {
    match crate::semantic::local_initializer_expr(db, decl_id)? {
        (0, initializer) => Some(initializer),
        _ => None,
    }
}

fn concatenate_finite_strings(left: &[SmolStr], right: &[SmolStr]) -> Vec<SmolStr> {
    if left.len() != 1 && right.len() != 1 {
        return Vec::new();
    }
    let mut result = Vec::with_capacity(left.len().saturating_mul(right.len()));
    for left in left {
        for right in right {
            result.push(SmolStr::new(format!("{left}{right}")));
        }
    }
    result
}

fn finite_string_transform_call(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    call: &LuaCallExpr,
    visiting: &mut FxHashSet<LuaDeclId>,
) -> Vec<SmolStr> {
    if call.get_access_path().as_deref() != Some("string.gsub") {
        return Vec::new();
    }
    let Some(args) = call.get_args_list() else {
        return Vec::new();
    };
    let args = args.get_args().collect::<Vec<_>>();
    if args.len() < 3
        || literal_string(&args[1]).as_deref() != Some("%s+")
        || literal_string(&args[2]).as_deref() != Some("")
    {
        return Vec::new();
    }

    finite_dynamic_key_strings(db, cache, &args[0], visiting)
        .into_iter()
        .map(|value| {
            SmolStr::new(
                value
                    .chars()
                    .filter(|character| !character.is_ascii_whitespace())
                    .collect::<String>(),
            )
        })
        .collect()
}

fn literal_string(expr: &LuaExpr) -> Option<String> {
    let LuaExpr::LiteralExpr(literal) = expr else {
        return None;
    };
    let LuaLiteralToken::String(value) = literal.get_literal()? else {
        return None;
    };
    Some(value.get_value())
}

fn finite_numeric_loop_table_values(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    index_expr: &glua_parser::LuaIndexExpr,
    visiting: &mut FxHashSet<LuaDeclId>,
) -> Vec<SmolStr> {
    let Some(LuaIndexKey::Expr(LuaExpr::NameExpr(key_name))) = index_expr.get_index_key() else {
        return Vec::new();
    };
    let Some(key_text) = key_name.get_name_text() else {
        return Vec::new();
    };
    let Some(for_stat) = index_expr.syntax().ancestors().find_map(LuaForStat::cast) else {
        return Vec::new();
    };
    if for_stat
        .get_var_name()
        .is_none_or(|name| name.get_name_text() != key_text)
    {
        return Vec::new();
    }
    let bounds = for_stat
        .get_iter_expr()
        .take(2)
        .filter_map(|bound| infer_integer_const(db, cache, bound))
        .collect::<Vec<_>>();
    let bounds = (bounds.len() == 2).then(|| (bounds[0], bounds[1]));
    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return Vec::new();
    };
    let Ok(prefix_type) = infer_expr(db, cache, prefix_expr) else {
        return Vec::new();
    };
    let Some(members) = crate::semantic::get_member_map(db, &prefix_type) else {
        return Vec::new();
    };

    let mut values = Vec::new();
    for (key, member_infos) in members {
        let LuaMemberKey::Integer(key) = key else {
            continue;
        };
        if bounds.is_some_and(|(start, end)| key < start || key > end) {
            continue;
        }
        for member_info in member_infos {
            if let Some(crate::LuaSemanticDeclId::Member(member_id)) = member_info.property_owner_id
                && let Some(value_expr) = crate::semantic::get_member_value_expr(db, member_id)
            {
                values.extend(finite_dynamic_key_strings(db, cache, &value_expr, visiting));
            }
            collect_string_const_names(&member_info.typ, &mut values);
        }
    }
    values.sort();
    values.dedup();
    values
}

fn infer_integer_const(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    expr: LuaExpr,
) -> Option<i64> {
    match infer_expr(db, cache, expr).ok()? {
        LuaType::IntegerConst(value) | LuaType::DocIntegerConst(value) => Some(value),
        _ => None,
    }
}

/// The single argument of the `pairs(..)` a `for ... in` loop iterates, when
/// `name_expr` names that loop's `var_index`-th variable.
pub(crate) struct ForRangePairsSource {
    pub(crate) iter_decl_id: LuaDeclId,
    pub(crate) source_expr: LuaExpr,
}

pub(crate) fn for_range_pairs_source_for_var(
    db: &DbIndex,
    file_id: crate::FileId,
    name_expr: &glua_parser::LuaNameExpr,
    var_index: usize,
) -> Option<ForRangePairsSource> {
    let name_decl_id = db
        .get_reference_index()
        .get_local_reference(&file_id)?
        .get_decl_id(&name_expr.get_range())?;
    let (for_range, iter_decl_id) = name_expr
        .syntax()
        .ancestors()
        .filter_map(LuaForRangeStat::cast)
        .find_map(|for_range| {
            let iter_name = for_range.get_var_name_list().nth(var_index)?;
            let iter_decl_id = LuaDeclId::new(file_id, iter_name.get_position());
            (iter_decl_id == name_decl_id).then_some((for_range, iter_decl_id))
        })?;

    let mut iter_exprs = for_range.get_expr_list();
    let Some(LuaExpr::CallExpr(call_expr)) = iter_exprs.next() else {
        return None;
    };
    if iter_exprs.next().is_some() || !is_provably_builtin_pairs_call(db, file_id, &call_expr) {
        return None;
    }

    let args_list = call_expr.get_args_list()?;
    let source_expr = if args_list.is_single_arg_no_parens() {
        match args_list.get_single_arg_expr()? {
            glua_parser::LuaSingleArgExpr::TableExpr(table_expr) => LuaExpr::TableExpr(table_expr),
            glua_parser::LuaSingleArgExpr::LiteralExpr(literal) => LuaExpr::LiteralExpr(literal),
        }
    } else {
        let mut args = args_list.get_args();
        let source_expr = args.next()?;
        if args.next().is_some() {
            return None;
        }
        source_expr
    };

    Some(ForRangePairsSource {
        iter_decl_id,
        source_expr,
    })
}

/// The `pairs` loop a write `t[k]` takes its key from, when `k` is that
/// loop's key variable, not reassigned before the write, and every table the
/// loop can iterate is `t`: each key the write names is one `t` already has.
pub(crate) fn writes_own_pairs_key(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    index_expr: &glua_parser::LuaIndexExpr,
) -> Option<ForRangePairsSource> {
    let file_id = cache.get_file_id();
    let Some(LuaIndexKey::Expr(LuaExpr::NameExpr(key_name))) = index_expr.get_index_key() else {
        return None;
    };
    let key = for_range_pairs_source_for_var(db, file_id, &key_name, 0)?;
    if !unreassigned_before(db, file_id, &key.iter_decl_id, index_expr) {
        return None;
    }
    let member_index = db.get_member_index();
    let target = member_index
        .get_member_owner(&LuaMemberId::new(index_expr.get_syntax_id(), file_id))
        .cloned()?;
    let target = member_index.canonical_owner(target);
    let source_type = infer_expr(db, cache, key.source_expr.clone()).ok()?;
    let mut sources = Vec::new();
    (collect_pairs_source_owners(&source_type, &mut sources)
        && !sources.is_empty()
        && sources
            .into_iter()
            .all(|source| member_index.canonical_owner(source) == target))
    .then_some(key)
}

/// Whether a write `t[k] = v` copies `t` into itself: `k` and `v` are the key
/// and value variables of one builtin `pairs` loop, neither reassigned before
/// the write, and every table that loop can iterate is `t`. Each field is
/// written back onto itself, so the copy gives no field a value it does not
/// already hold.
pub(crate) fn copies_table_into_itself(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    index_expr: &glua_parser::LuaIndexExpr,
    value_expr: &LuaExpr,
) -> bool {
    let file_id = cache.get_file_id();
    let LuaExpr::NameExpr(value_name) = value_expr else {
        return false;
    };
    let Some(value) = for_range_pairs_source_for_var(db, file_id, value_name, 1) else {
        return false;
    };
    unreassigned_before(db, file_id, &value.iter_decl_id, index_expr)
        && writes_own_pairs_key(db, cache, index_expr)
            .is_some_and(|key| key.source_expr.get_range() == value.source_expr.get_range())
}

/// Whether the local `decl_id` is not reassigned before `index_expr`.
fn unreassigned_before(
    db: &DbIndex,
    file_id: crate::FileId,
    decl_id: &LuaDeclId,
    index_expr: &glua_parser::LuaIndexExpr,
) -> bool {
    db.get_reference_index()
        .first_local_reassignment(file_id, decl_id)
        .is_none_or(|position| position >= index_expr.get_position())
}

fn field_names_from_for_range_pairs_key(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    key_expr: LuaExpr,
) -> Option<ForRangePairsFieldNames> {
    let LuaExpr::NameExpr(name_expr) = key_expr else {
        return None;
    };
    let ForRangePairsSource {
        iter_decl_id,
        source_expr,
    } = for_range_pairs_source_for_var(db, cache.get_file_id(), &name_expr, 0)?;

    let (mut names, may_have_other_string_names) = match source_expr {
        LuaExpr::TableExpr(table_expr) => field_names_from_pairs_table_expr_keys(&table_expr),
        // The keys a table was written with are read off its members, not
        // off the loop variable's type: that type is the union of those keys
        // only until a computed-key write joins the table, when it collapses
        // to `string` and names nothing -- and whether that write has joined
        // yet is a property of how far the batch has settled.
        source_expr => {
            let source_type = infer_expr(db, cache, source_expr).ok()?;
            field_names_from_member_keys(db, &source_type)?
        }
    };
    names.sort();
    names.dedup();
    Some(ForRangePairsFieldNames {
        names,
        iter_decl_id,
        may_have_other_string_names,
    })
}

/// The names a table's members were written with, read off the syntax of
/// each write, plus whether any member was written through a computed key.
///
/// A member minted under the name a computed key happened to resolve to does
/// not count as a name the source states: it is one of the "other" names.
fn field_names_from_member_keys(
    db: &DbIndex,
    source_type: &LuaType,
) -> Option<(Vec<SmolStr>, bool)> {
    let mut owners = Vec::new();
    if !collect_pairs_source_owners(source_type, &mut owners) {
        return None;
    }
    let member_index = db.get_member_index();
    let mut names = Vec::new();
    let mut may_have_other_string_names = false;
    for owner in owners {
        let Some(members) = member_index.get_members(&owner) else {
            may_have_other_string_names = true;
            continue;
        };
        for member in members {
            match member.get_key() {
                LuaMemberKey::Name(name) if member_key_is_written_literally(db, member) => {
                    names.push(SmolStr::from(name.as_str()));
                }
                LuaMemberKey::Name(_) | LuaMemberKey::ExprType(_) => {
                    may_have_other_string_names = true;
                }
                LuaMemberKey::Integer(_) | LuaMemberKey::None => {}
            }
        }
    }
    Some((names, may_have_other_string_names))
}

/// The owners whose members a `pairs` loop over `typ` enumerates, or `false`
/// when an arm names no table the index holds.
fn collect_pairs_source_owners(typ: &LuaType, owners: &mut Vec<LuaMemberOwner>) -> bool {
    match typ {
        LuaType::TableConst(range) => {
            owners.push(LuaMemberOwner::Element(range.clone()));
            true
        }
        LuaType::Instance(instance) => {
            owners.push(LuaMemberOwner::Element(instance.get_range().clone()));
            true
        }
        LuaType::Def(type_id) | LuaType::Ref(type_id) => {
            owners.push(LuaMemberOwner::Type(type_id.clone()));
            true
        }
        LuaType::MergedTable(merged) => merged
            .get_types()
            .iter()
            .all(|arm| collect_pairs_source_owners(arm, owners)),
        LuaType::Union(union) => union
            .types()
            .filter(|arm| !arm.is_nil())
            .all(|arm| collect_pairs_source_owners(arm, owners)),
        _ => false,
    }
}

/// Whether a member's key is spelled literally at its write.
fn member_key_is_written_literally(db: &DbIndex, member: &crate::LuaMember) -> bool {
    let Some(root) = db
        .get_vfs()
        .syntax_tree_around(&member.get_file_id(), member.get_syntax_id().get_range())
        .map(|tree| tree.get_red_root())
    else {
        return false;
    };
    let Some(node) = member.get_syntax_id().to_node_from_root(&root) else {
        return false;
    };
    let key = if let Some(index_expr) = glua_parser::LuaIndexExpr::cast(node.clone()) {
        index_expr.get_index_key()
    } else if let Some(field) = glua_parser::LuaTableField::cast(node) {
        field.get_field_key()
    } else {
        // Declared in an annotation or synthesized: the name is the source's.
        return true;
    };
    match key {
        Some(LuaIndexKey::Name(_) | LuaIndexKey::String(_)) => true,
        Some(LuaIndexKey::Expr(LuaExpr::LiteralExpr(literal))) => {
            matches!(literal.get_literal(), Some(LuaLiteralToken::String(_)))
        }
        _ => false,
    }
}

pub(crate) fn is_provably_builtin_pairs_call(
    db: &DbIndex,
    file_id: crate::FileId,
    call_expr: &LuaCallExpr,
) -> bool {
    is_provably_builtin_global_call(db, file_id, call_expr, "pairs")
}

/// [`is_provably_builtin_pairs_call`] proved for the other builtin iterator
/// whose generic bindings the for-range path retrieves (`ipairs`).
pub(crate) fn is_provably_builtin_iterator_call(
    db: &DbIndex,
    file_id: crate::FileId,
    call_expr: &LuaCallExpr,
) -> bool {
    ["pairs", "ipairs"]
        .into_iter()
        .any(|builtin_name| is_provably_builtin_global_call(db, file_id, call_expr, builtin_name))
}

fn is_provably_builtin_global_call(
    db: &DbIndex,
    file_id: crate::FileId,
    call_expr: &LuaCallExpr,
    builtin_name: &str,
) -> bool {
    if call_expr.get_access_path().as_deref() != Some(builtin_name) {
        return false;
    }
    let Some(LuaExpr::NameExpr(call_name)) = call_expr.get_prefix_expr() else {
        return false;
    };
    if let Some(decl_id) = db
        .get_reference_index()
        .get_local_reference(&file_id)
        .and_then(|references| references.get_decl_id(&call_name.get_range()))
        && !aliases_global(db, file_id, decl_id, builtin_name)
    {
        return false;
    }

    let module_index = db.get_module_index();
    let Some(current_workspace_id) = module_index.get_workspace_id(file_id) else {
        return false;
    };
    let Some(priority_tiers) = db.get_global_index().get_global_decl_id_priority_tiers(
        builtin_name,
        module_index,
        current_workspace_id,
    ) else {
        return false;
    };
    let Some((_, decl_ids)) = priority_tiers
        .into_iter()
        .find(|(_, decl_ids)| !decl_ids.is_empty())
    else {
        return false;
    };

    decl_ids.iter().all(|decl_id| {
        module_index
            .get_workspace_id(decl_id.file_id)
            .is_some_and(|workspace_id| {
                module_index.get_workspace_kind(workspace_id) == WorkspaceKind::Std
            })
            || decl_preserves_builtin_semantics(db, *decl_id, builtin_name)
    })
}

/// Whether the local `decl_id` is `local name = name` for the global `name`,
/// never reassigned: it names the global wherever it is read.
fn aliases_global(db: &DbIndex, file_id: crate::FileId, decl_id: LuaDeclId, name: &str) -> bool {
    let Some(LuaExpr::NameExpr(initializer)) = local_initializer_expr(db, decl_id) else {
        return false;
    };
    initializer.get_name_text().as_deref() == Some(name)
        && db
            .get_reference_index()
            .get_local_reference(&file_id)
            .and_then(|references| references.get_decl_id(&initializer.get_range()))
            .is_none()
        && db
            .get_reference_index()
            .first_local_reassignment(file_id, &decl_id)
            .is_none()
}

fn decl_preserves_builtin_semantics(db: &DbIndex, decl_id: LuaDeclId, builtin_name: &str) -> bool {
    let semantic_decl = LuaSemanticDeclId::LuaDecl(decl_id);
    let signature_id = db
        .get_property_index()
        .get_signature_owner(&semantic_decl)
        .or_else(|| {
            db.get_type_index()
                .get_type_cache(&decl_id.into())
                .and_then(|type_cache| match type_cache.as_type() {
                    LuaType::Signature(signature_id) => Some(*signature_id),
                    _ => None,
                })
        });
    let Some(signature_id) = signature_id else {
        return false;
    };
    let Some(attribute) = find_signature_attribute_use(db, signature_id, BUILTIN_ALIAS_ATTRIBUTE)
    else {
        return false;
    };
    matches!(
        attribute
            .get_param_by_name("name")
            .or_else(|| attribute.args.first().and_then(|(_, typ)| typ.as_ref())),
        Some(LuaType::StringConst(name) | LuaType::DocStringConst(name))
            if name.as_ref() == builtin_name
    )
}

fn field_names_from_pairs_table_expr_keys(table_expr: &LuaTableExpr) -> (Vec<SmolStr>, bool) {
    let mut names = Vec::new();
    let mut may_have_other_string_names = false;
    for field in table_expr.get_fields() {
        match field.get_field_key() {
            Some(LuaIndexKey::Name(name)) => names.push(name.get_name_text().into()),
            Some(LuaIndexKey::String(string)) => names.push(string.get_value().into()),
            Some(LuaIndexKey::Integer(_) | LuaIndexKey::Idx(_)) | None => {}
            Some(LuaIndexKey::Expr(LuaExpr::LiteralExpr(literal))) => match literal.get_literal() {
                Some(LuaLiteralToken::String(string)) => names.push(string.get_value().into()),
                Some(
                    LuaLiteralToken::Number(_) | LuaLiteralToken::Bool(_) | LuaLiteralToken::Nil(_),
                ) => {}
                _ => may_have_other_string_names = true,
            },
            Some(LuaIndexKey::Expr(_)) => may_have_other_string_names = true,
        }
    }
    (names, may_have_other_string_names)
}

fn is_dynamic_index_key(index_expr: &glua_parser::LuaIndexExpr) -> bool {
    matches!(index_expr.get_index_key(), Some(LuaIndexKey::Expr(_)))
}

fn string_const_names(typ: &Option<LuaType>) -> Vec<SmolStr> {
    let mut names = Vec::new();
    if let Some(typ) = typ {
        collect_string_const_names(typ, &mut names);
    }
    names
}

fn collect_string_const_names(typ: &LuaType, names: &mut Vec<SmolStr>) {
    match typ {
        LuaType::StringConst(name) | LuaType::DocStringConst(name) => {
            names.push(name.as_ref().clone());
        }
        LuaType::Union(union_type) => {
            for member in union_type.types() {
                collect_string_const_names(member, names);
            }
        }
        _ => {}
    }
}

/// Propagates collected dynamic fields to the super types of their owners so
/// that e.g. a field assigned on `base_glide` (which extends `Entity`) is also
/// visible when the variable is typed as `Entity`.  This avoids false-positive
/// `undefined-field` when user code accesses entity fields through a
/// base-class reference.  Shared by the initial pass and the settled
/// re-derivation so both file the same propagated field set.
fn propagate_dynamic_fields_to_super_types(
    db: &DbIndex,
    collected: &[(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)],
) -> Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)> {
    // The authoring globals of scripted-class scopes (`ENT`, `SWEP`,
    // `TOOL`, `GM`, and their aliases) are supers of *every* per-file
    // scoped class, so propagating there pools each file's fields onto
    // one table the whole workspace writes. Those scopes are per-file
    // by design: a `---@type SWEP` read is a read of the generic
    // authoring table, not of any one weapon, so it should see the
    // annotated `SWEP` fields and not every weapon's private ones.
    let scoped_globals: FxHashSet<&str> = db
        .get_emmyrc()
        .gmod
        .scripted_class_scopes
        .resolved_definitions_slice()
        .iter()
        .flat_map(|definition| {
            std::iter::once(definition.class_global.as_str())
                .chain(definition.aliases.iter().map(String::as_str))
        })
        .collect();
    let mut propagated = Vec::new();
    for (owner, field_name, file_id, range) in collected {
        let DynamicFieldOwner::Type(type_id) = owner else {
            continue;
        };
        let mut super_types = Vec::new();
        type_id.collect_super_types(db, &mut super_types);
        for super_type in super_types {
            if let LuaType::Ref(super_id) = &super_type
                && !scoped_globals.contains(super_id.get_name())
            {
                propagated.push((
                    DynamicFieldOwner::Type(super_id.clone()),
                    field_name.clone(),
                    *file_id,
                    *range,
                ));
            }
        }
    }
    propagated
}

fn collect_wildcard_for_type(
    db: &DbIndex,
    typ: &LuaType,
    file_id: crate::FileId,
    range: rowan::TextRange,
    result: &mut Vec<(DynamicFieldOwner, crate::FileId, rowan::TextRange)>,
) {
    let mut visited = rustc_hash::FxHashSet::default();
    collect_wildcard_for_type_inner(db, typ, file_id, range, result, &mut visited);
}

fn collect_wildcard_for_type_inner(
    db: &DbIndex,
    typ: &LuaType,
    file_id: crate::FileId,
    range: rowan::TextRange,
    result: &mut Vec<(DynamicFieldOwner, crate::FileId, rowan::TextRange)>,
    visited: &mut rustc_hash::FxHashSet<LuaType>,
) {
    if !visited.insert(typ.clone()) {
        return;
    }
    match typ {
        // `NULL` holds no fields: a write through it errors at runtime.
        LuaType::Ref(id) | LuaType::Def(id) if !contains_gmod_null_type(db, typ) => {
            result.push((DynamicFieldOwner::Type(id.clone()), file_id, range));
        }
        // Canonicalized at push, like `collect_for_type`: the
        // `definition_site_owner` read is noted during collection so a
        // late literal→path binding re-derives it.
        LuaType::TableConst(table_range) => {
            result.push((
                crate::canonical_dynamic_field_owner(
                    db,
                    DynamicFieldOwner::Table(table_range.clone()),
                ),
                file_id,
                range,
            ));
        }
        LuaType::Instance(instance) => {
            collect_wildcard_for_type_inner(
                db,
                instance.get_base(),
                file_id,
                range,
                result,
                visited,
            );
        }
        LuaType::TableOf(inner) => {
            collect_wildcard_for_type_inner(db, inner, file_id, range, result, visited);
        }
        LuaType::Union(union_type) => {
            for t in union_type.types() {
                collect_wildcard_for_type_inner(db, t, file_id, range, result, visited);
            }
        }
        // See `collect_for_type`: a merged table stands for its backing
        // literals.
        LuaType::MergedTable(merged) => {
            for t in merged.get_types() {
                collect_wildcard_for_type_inner(db, t, file_id, range, result, visited);
            }
        }
        _ => {}
    }
}

fn collect_for_type(
    db: &DbIndex,
    typ: &LuaType,
    field_name: &SmolStr,
    file_id: crate::FileId,
    range: rowan::TextRange,
    result: &mut Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)>,
) {
    let mut visited = rustc_hash::FxHashSet::default();
    collect_for_type_inner(db, typ, field_name, file_id, range, result, &mut visited);
}

fn collect_for_type_inner(
    db: &DbIndex,
    typ: &LuaType,
    field_name: &SmolStr,
    file_id: crate::FileId,
    range: rowan::TextRange,
    result: &mut Vec<(DynamicFieldOwner, SmolStr, crate::FileId, rowan::TextRange)>,
    visited: &mut rustc_hash::FxHashSet<LuaType>,
) {
    if !visited.insert(typ.clone()) {
        return;
    }
    match typ {
        // `NULL` holds no fields: a write through it errors at runtime.
        LuaType::Ref(id) | LuaType::Def(id) if !contains_gmod_null_type(db, typ) => {
            result.push((
                DynamicFieldOwner::Type(id.clone()),
                field_name.clone(),
                file_id,
                range,
            ));
        }
        // The owner is canonicalized here, during the site's collection
        // scope: a literal that initializes a global path belongs to the
        // path, and the `definition_site_owner` read that decides it is what
        // re-derives this site when the binding lands after collection.
        // Canonicalizing at merge instead would file the same owner but
        // record the read nowhere, leaving the site frozen at the literal.
        LuaType::TableConst(table_range) => {
            result.push((
                crate::canonical_dynamic_field_owner(
                    db,
                    DynamicFieldOwner::Table(table_range.clone()),
                ),
                field_name.clone(),
                file_id,
                range,
            ));
        }
        LuaType::Instance(instance) => {
            collect_for_type_inner(
                db,
                instance.get_base(),
                field_name,
                file_id,
                range,
                result,
                visited,
            );
        }
        LuaType::TableOf(inner) => {
            collect_for_type_inner(db, inner, field_name, file_id, range, result, visited);
        }
        LuaType::Union(union_type) => {
            for t in union_type.types() {
                collect_for_type_inner(db, t, field_name, file_id, range, result, visited);
            }
        }
        // A merged table is the same runtime table seen through several
        // backing literals; the write belongs to each of them, exactly as it
        // does when inference answers with the literals unmerged.
        LuaType::MergedTable(merged) => {
            for t in merged.get_types() {
                collect_for_type_inner(db, t, field_name, file_id, range, result, visited);
            }
        }
        _ => {}
    }
}

fn collect_finite_member_for_type(
    db: &DbIndex,
    typ: &LuaType,
    member_id: LuaMemberId,
    file_id: crate::FileId,
    range: rowan::TextRange,
    has_finite_domain: bool,
    result: &mut Vec<FiniteNamedMemberEvidence>,
) {
    let mut visited = rustc_hash::FxHashSet::default();
    collect_finite_member_for_type_inner(
        db,
        typ,
        member_id,
        file_id,
        range,
        has_finite_domain,
        result,
        &mut visited,
    );
}

fn collect_finite_member_for_type_inner(
    db: &DbIndex,
    typ: &LuaType,
    member_id: LuaMemberId,
    file_id: crate::FileId,
    range: rowan::TextRange,
    has_finite_domain: bool,
    result: &mut Vec<FiniteNamedMemberEvidence>,
    visited: &mut rustc_hash::FxHashSet<LuaType>,
) {
    if !visited.insert(typ.clone()) {
        return;
    }
    match typ {
        LuaType::Ref(id) | LuaType::Def(id) => {
            result.push((
                DynamicFieldOwner::Type(id.clone()),
                member_id,
                file_id,
                range,
                has_finite_domain,
            ));
        }
        // Canonicalized at push, like `collect_for_type`: the mark must land
        // on the owner readers consult, and the `definition_site_owner` read
        // is noted during collection so a late literal→path binding
        // re-derives it.
        LuaType::TableConst(table_range) => {
            result.push((
                crate::canonical_dynamic_field_owner(
                    db,
                    DynamicFieldOwner::Table(table_range.clone()),
                ),
                member_id,
                file_id,
                range,
                has_finite_domain,
            ));
        }
        LuaType::Instance(instance) => {
            collect_finite_member_for_type_inner(
                db,
                instance.get_base(),
                member_id,
                file_id,
                range,
                has_finite_domain,
                result,
                visited,
            );
        }
        LuaType::TableOf(inner) => {
            collect_finite_member_for_type_inner(
                db,
                inner,
                member_id,
                file_id,
                range,
                has_finite_domain,
                result,
                visited,
            );
        }
        LuaType::Union(union_type) => {
            for typ in union_type.types() {
                collect_finite_member_for_type_inner(
                    db,
                    typ,
                    member_id,
                    file_id,
                    range,
                    has_finite_domain,
                    result,
                    visited,
                );
            }
        }
        // See `collect_for_type`: a merged table stands for its backing
        // literals.
        LuaType::MergedTable(merged) => {
            for typ in merged.get_types() {
                collect_finite_member_for_type_inner(
                    db,
                    typ,
                    member_id,
                    file_id,
                    range,
                    has_finite_domain,
                    result,
                    visited,
                );
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    use crate::{
        DbIndex, FileId, InFiled, LuaMember, LuaMemberFeature, LuaMemberId, LuaMemberKey,
        LuaMemberOwner, LuaType, LuaTypeCache, LuaTypeDeclId, LuaTypeOwner,
    };

    use super::find_declared_member_types_for_dynamic_field;

    fn member_id_at(start: u32) -> LuaMemberId {
        let range = TextRange::new(TextSize::new(start), TextSize::new(start + 1));
        LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range),
            FileId::new(0),
        )
    }

    fn table_range_at(start: u32) -> InFiled<TextRange> {
        InFiled::new(
            FileId::new(0),
            TextRange::new(TextSize::new(start), TextSize::new(start + 1)),
        )
    }

    fn add_typed_member(
        db: &mut DbIndex,
        owner: LuaMemberOwner,
        member_id: LuaMemberId,
        key: LuaMemberKey,
        typ: LuaType,
    ) {
        db.get_member_index_mut().add_member(
            owner,
            LuaMember::new(member_id, key, LuaMemberFeature::FileDefine, None),
        );
        db.get_type_index_mut().bind_type(
            LuaTypeOwner::Member(member_id),
            LuaTypeCache::InferType(typ),
        );
    }

    #[test]
    fn declared_member_field_collection_uses_current_member_type_for_ref_owners() {
        let mut db = DbIndex::new();
        let member_id = member_id_at(1);
        db.get_type_index_mut().bind_type(
            LuaTypeOwner::Member(member_id),
            LuaTypeCache::InferType(LuaType::Table),
        );

        let member_types = find_declared_member_types_for_dynamic_field(
            &db,
            &LuaType::Ref(LuaTypeDeclId::global("DynamicOwner")),
            LuaMemberKey::from("field"),
            member_id,
        )
        .expect("current member type cache should be enough for non-table owners");

        assert_eq!(member_types, vec![LuaType::Table]);
    }

    #[test]
    fn declared_member_field_collection_uses_current_member_type_before_owner_key_fallback() {
        let mut db = DbIndex::new();
        let table_range = table_range_at(10);
        let owner = LuaMemberOwner::Element(table_range.clone());
        add_typed_member(
            &mut db,
            owner,
            member_id_at(1),
            LuaMemberKey::from("field"),
            LuaType::String,
        );
        let current_member_id = member_id_at(3);
        db.get_type_index_mut().bind_type(
            LuaTypeOwner::Member(current_member_id),
            LuaTypeCache::InferType(LuaType::Boolean),
        );

        let member_types = find_declared_member_types_for_dynamic_field(
            &db,
            &LuaType::TableConst(table_range),
            LuaMemberKey::from("field"),
            current_member_id,
        )
        .expect("current member type cache should take precedence");

        assert_eq!(member_types, vec![LuaType::Boolean]);
    }

    #[test]
    fn declared_member_field_collection_keeps_table_owner_fallback_without_current_cache() {
        let mut db = DbIndex::new();
        let table_range = table_range_at(10);
        let owner = LuaMemberOwner::Element(table_range.clone());
        add_typed_member(
            &mut db,
            owner,
            member_id_at(1),
            LuaMemberKey::from("field"),
            LuaType::String,
        );

        let member_types = find_declared_member_types_for_dynamic_field(
            &db,
            &LuaType::TableConst(table_range),
            LuaMemberKey::from("field"),
            member_id_at(99),
        )
        .expect("table owner/key fallback should still provide member types");

        assert_eq!(member_types, vec![LuaType::String]);
    }
}
