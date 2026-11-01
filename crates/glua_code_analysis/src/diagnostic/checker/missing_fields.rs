use glua_parser::LuaSyntaxNodeExt;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaCallArgList, LuaCallExpr, LuaExpr, LuaIndexKey, LuaStat,
    LuaSyntaxNode, LuaTableExpr, LuaTableField, LuaVarExpr, PathTrait,
};

use crate::{
    DbIndex, DiagnosticCode, FileId, InFiled, LuaMemberFeature, LuaMemberKey, LuaMemberOwner,
    LuaSemanticDeclId, LuaType, LuaTypeCache, LuaTypeDeclId, SemanticModel, VariadicType,
    semantic::{MetatableIndexField, find_metatable_index_field},
};

use super::{Checker, DiagnosticContext, humanize_lint_type};
use itertools::Itertools;

pub struct MissingFieldsChecker;

impl Checker for MissingFieldsChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::MissingFields];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let mut type_cache: HashMap<LuaType, Arc<HashSet<String>>> = HashMap::default();
        let Some(buckets) = context.file_buckets() else {
            return;
        };
        for expr in buckets.table_exprs.iter() {
            if context.is_cancelled() {
                return;
            }
            check_table_expr(context, semantic_model, expr, &mut type_cache);
        }
    }
}

fn check_table_expr(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    expr: &LuaTableExpr,
    type_cache: &mut HashMap<LuaType, Arc<HashSet<String>>>,
) -> Option<()> {
    if context.is_cancelled() {
        return Some(());
    }

    let db = context.db;

    let mut union_has_array_type = false;
    let table_type = match semantic_model.infer_table_should_be(expr.clone())? {
        LuaType::Union(union) => {
            let mut set = HashSet::new();
            for ty in union.types() {
                if context.is_cancelled() {
                    return Some(());
                }
                match ty {
                    LuaType::Ref(_)
                    | LuaType::Object(_)
                    | LuaType::Generic(_)
                    | LuaType::Intersection(_) => {
                        set.insert(ty.clone());
                    }
                    LuaType::Table | LuaType::Userdata => {
                        return Some(());
                    }
                    LuaType::TableGeneric(_) => {
                        return Some(());
                    }
                    // If the union contains an array type (e.g., Entity[]), skip the missing fields check
                    // This is because a table literal being passed as an array shouldn't be checked
                    // against class types in the union
                    LuaType::Array(_) => {
                        union_has_array_type = true;
                    }
                    _ => {}
                }
            }
            match set.len() {
                1 => set.into_iter().next()?.clone(),
                _ => {
                    return Some(());
                }
            }
        }
        LuaType::TableConst(in_file_range) => {
            let file_id = in_file_range.file_id;
            if file_id == semantic_model.get_file_id() {
                let range = in_file_range.value;
                if expr.get_range() == range {
                    return Some(());
                }
            }

            LuaType::TableConst(in_file_range)
        }

        table_type => table_type,
    };

    let fields = expr.get_fields().collect::<Vec<_>>();
    if union_has_array_type && table_literal_looks_array_like(&fields) {
        return Some(());
    }
    if fields.len() > 50 {
        return Some(());
    }

    let mut current_fields: HashSet<String> = fields
        .iter()
        .filter_map(|field| field.get_field_key().map(|key| key.get_path_part()))
        .collect();
    let spread = trailing_spread_slots(semantic_model, &fields);

    let required_fields = match &table_type {
        LuaType::Ref(type_decl_id) => get_precomputed_required_fields(context, type_decl_id)
            .unwrap_or_else(|| {
                type_cache
                    .entry(table_type.clone())
                    .or_insert_with(|| {
                        let types = type_decl_id
                            .collect_super_types_with_self(context.db, table_type.clone());
                        Arc::new(get_required_fields(context, &types).unwrap_or_default())
                    })
                    .clone()
            }),
        LuaType::Generic(generic_type) => {
            let type_decl_id = generic_type.get_base_type_id();
            get_precomputed_required_fields(context, &type_decl_id).unwrap_or_else(|| {
                type_cache
                    .entry(table_type.clone())
                    .or_insert_with(|| {
                        let types = type_decl_id
                            .collect_super_types_with_self(context.db, table_type.clone());
                        Arc::new(get_required_fields(context, &types).unwrap_or_default())
                    })
                    .clone()
            })
        }
        LuaType::Object(_) => type_cache
            .entry(table_type.clone())
            .or_insert_with(|| {
                Arc::new(
                    get_required_fields(context, std::slice::from_ref(&table_type))
                        .unwrap_or_default(),
                )
            })
            .clone(),
        LuaType::Intersection(intersections) => type_cache
            .entry(table_type.clone())
            .or_insert_with(|| {
                let mut computed_fields = HashSet::default();
                for intersection_component in intersections.get_types() {
                    if context.is_cancelled() {
                        return Arc::new(computed_fields);
                    }
                    computed_fields.extend(
                        get_required_fields(context, std::slice::from_ref(&intersection_component))
                            .unwrap_or_default(),
                    );
                }
                Arc::new(computed_fields)
            })
            .clone(),
        _ => return Some(()),
    };

    if let Some(spread) = &spread {
        current_fields.extend(
            required_fields
                .iter()
                .filter(|name| spread.fills(name))
                .cloned(),
        );
    }
    let mut missing: HashSet<&str> = required_fields
        .difference(&current_fields)
        .map(String::as_str)
        .collect();
    if missing.is_empty() {
        return Some(());
    }

    remove_fields_completed_after(semantic_model, &table_type, expr, &mut missing);
    remove_fields_supplied_by_metatable(db, semantic_model.get_file_id(), expr, &mut missing);
    remove_fields_gained_before_use(semantic_model, expr, &mut missing);
    if missing.is_empty() {
        return Some(());
    }

    let missing_fields = missing
        .iter()
        .map(|name| format!("`{}`", name))
        .sorted()
        .join(", ");

    context.add_diagnostic(
        DiagnosticCode::MissingFields,
        expr.get_range(),
        format!(
            "Missing required fields in type `{typ}`: {fields}",
            typ = humanize_lint_type(db, &table_type),
            fields = missing_fields
        )
        .to_string(),
        None,
    );

    Some(())
}

/// A literal is often written empty and filled in by the statements after it, so
/// the fields those statements assign onto the same target are not missing:
/// `self.portals = {}` followed by `self.portals.exterior = ...` is complete.
///
/// Restricted to the literal's own block. An assignment nested in a branch does
/// not always run, and the report is still correct there.
fn remove_fields_completed_after(
    semantic_model: &SemanticModel,
    table_type: &LuaType,
    expr: &LuaTableExpr,
    missing: &mut HashSet<&str>,
) -> Option<()> {
    let stat = expr.ancestors::<LuaStat>().next()?;
    let target = assigned_target_path(expr, &stat)?;

    let mut next = stat.syntax().next_sibling();
    while let Some(node) = next {
        if missing.is_empty() {
            break;
        }

        let mut written_prefixes = Vec::new();
        let mut values = Vec::new();
        if let Some(assign) = LuaAssignStat::cast(node.clone()) {
            let vars;
            (vars, values) = assign.get_var_and_expr_list();
            let mut written_keys = Vec::new();
            for var in vars {
                let LuaVarExpr::IndexExpr(index) = var else {
                    continue;
                };
                let Some(prefix) = index.get_prefix_expr() else {
                    continue;
                };
                if LuaVarExpr::cast(prefix.syntax().clone())
                    .and_then(|prefix| prefix.get_access_path())
                    .as_deref()
                    != Some(target.as_str())
                {
                    continue;
                }
                written_prefixes.push(prefix.syntax().clone());
                if let Some(key) = index.get_index_key() {
                    written_keys.push(key.get_path_part());
                }
            }

            // Values run before their writes; a function value runs later unless called there.
            let consumed_by_values = values
                .iter()
                .flat_map(|value| {
                    value
                        .syntax()
                        .descendants_of::<LuaVarExpr>()
                        .filter(move |var| !runs_after_value(var, value))
                })
                .filter(|var| {
                    var.get_access_path().as_deref() == Some(target.as_str())
                        && !declared_within(semantic_model, var, &node)
                        && hands_to_call(var)
                })
                .collect::<Vec<_>>();
            if !consumed_by_values.is_empty() {
                for var in consumed_by_values {
                    remove_fields_filled_by_callee(semantic_model, table_type, &var, missing);
                }
                break;
            }
            for key in &written_keys {
                missing.remove(key.as_str());
            }
        }

        // Anything else touching the target consumes it while it is still
        // incomplete, so later writes no longer make the literal complete.
        let escaping_vars = node
            .descendants_of::<LuaVarExpr>()
            .filter(|var| {
                var.get_access_path().as_deref() == Some(target.as_str())
                    && !written_prefixes.contains(var.syntax())
                    && !declared_within(semantic_model, var, &node)
                    && !values.iter().any(|value| {
                        value
                            .syntax()
                            .text_range()
                            .contains_range(var.syntax().text_range())
                            && runs_after_value(var, value)
                    })
            })
            .collect::<Vec<_>>();
        if !escaping_vars.is_empty() {
            for var in escaping_vars {
                remove_fields_filled_by_callee(semantic_model, table_type, &var, missing);
            }
            break;
        }

        next = node.next_sibling();
    }

    Some(())
}

/// Whether `var` is handed to a call, as an argument or as a method call's `self`.
fn hands_to_call(var: &LuaVarExpr) -> bool {
    let operand = outermost_value(var.syntax());
    let Some(parent) = operand.parent() else {
        return false;
    };
    LuaCallArgList::can_cast(parent.kind().into())
        || glua_parser::LuaIndexExpr::cast(parent).is_some_and(|index| {
            index
                .get_prefix_expr()
                .is_some_and(|prefix| prefix.syntax() == &operand)
                && index
                    .get_index_token()
                    .is_some_and(|token| token.is_colon())
        })
}

/// `node` together with the parentheses wrapped around it, which pass on exactly its value.
fn outermost_parens(node: &LuaSyntaxNode) -> LuaSyntaxNode {
    let mut operand = node.clone();
    while let Some(paren) = operand.parent().and_then(glua_parser::LuaParenExpr::cast) {
        operand = paren.syntax().clone();
    }
    operand
}

/// `node` together with the parentheses and `and`/`or` operators that may pass its value on.
fn outermost_value(node: &LuaSyntaxNode) -> LuaSyntaxNode {
    let mut operand = node.clone();
    while let Some(parent) = operand.parent() {
        let carries_value = glua_parser::LuaParenExpr::can_cast(parent.kind().into())
            || glua_parser::LuaBinaryExpr::cast(parent.clone()).is_some_and(|binary| {
                binary.get_op_token().is_some_and(|op| {
                    matches!(
                        op.get_op(),
                        glua_parser::BinaryOperator::OpAnd | glua_parser::BinaryOperator::OpOr
                    )
                })
            });
        if !carries_value {
            break;
        }
        operand = parent;
    }
    operand
}

/// Whether `var`'s root variable is declared inside `scope`, shadowing any outer one.
fn declared_within(
    semantic_model: &SemanticModel,
    var: &LuaVarExpr,
    scope: &LuaSyntaxNode,
) -> bool {
    let mut root = LuaExpr::cast(var.syntax().clone());
    while let Some(LuaExpr::IndexExpr(index)) = root {
        root = index.get_prefix_expr();
    }
    let Some(LuaExpr::NameExpr(name)) = root else {
        return false;
    };
    semantic_model
        .get_db()
        .get_reference_index()
        .get_local_reference(&semantic_model.get_file_id())
        .and_then(|references| references.get_decl_id(&name.get_range()))
        .is_some_and(|decl_id| scope.text_range().contains(decl_id.position))
}

/// Whether `var` sits in a function `value` stores for later instead of running it.
fn runs_after_value(var: &LuaVarExpr, value: &LuaExpr) -> bool {
    var.syntax()
        .ancestors()
        .take_while(|node| node != value.syntax())
        .chain(std::iter::once(value.syntax().clone()))
        .filter_map(glua_parser::LuaClosureExpr::cast)
        .any(|closure| !may_run_where_written(&closure))
}

/// Whether `closure` is called where it is written, or handed to a call that may run it.
fn may_run_where_written(closure: &glua_parser::LuaClosureExpr) -> bool {
    let operand = outermost_value(closure.syntax());
    let Some(parent) = operand.parent() else {
        return false;
    };
    LuaCallArgList::can_cast(parent.kind().into())
        || LuaCallExpr::cast(parent)
            .and_then(|call| call.get_prefix_expr())
            .is_some_and(|prefix| prefix.syntax() == &operand)
}

/// A literal passed as an out-parameter is completed by the callee, not by the
/// caller: `local layout = {}` followed by `Fill(layout)` is complete when
/// `Fill` writes those fields onto its parameter. Only a parameter declared as
/// the very type being checked is read this way, so an ordinary consumer of the
/// value keeps reporting.
fn remove_fields_filled_by_callee(
    semantic_model: &SemanticModel,
    table_type: &LuaType,
    var: &LuaVarExpr,
    missing: &mut HashSet<&str>,
) -> Option<()> {
    let operand = outermost_parens(var.syntax());
    let arg_list = LuaCallArgList::cast(operand.parent()?)?;
    let call_expr = arg_list.get_parent::<LuaCallExpr>()?;
    let arg_index = arg_list
        .get_args()
        .position(|arg| arg.syntax() == &operand)?;

    let prefix_expr = call_expr.get_prefix_expr()?;
    let LuaType::Signature(signature_id) = semantic_model.infer_expr(prefix_expr).ok()? else {
        return None;
    };
    let signature = semantic_model
        .get_db()
        .get_signature_index()
        .get(&signature_id)?;

    // A colon-defined function keeps its receiver out of the parameter list.
    let param_index = match (signature.is_colon_define, call_expr.is_colon_call()) {
        (true, true) | (false, false) => arg_index,
        (false, true) => arg_index + 1,
        (true, false) => arg_index.checked_sub(1)?,
    };
    if signature.get_param_info_by_id(param_index)?.type_ref != *table_type {
        return None;
    }
    let param_name = signature.params.get(param_index)?;

    let closure = semantic_model.get_signature_closure(signature_id)?;
    let block = closure.get_block()?;
    // Only the body's own statements count, the same rule
    // `remove_fields_completed_after` applies to the caller side: a write nested
    // in a branch or a loop does not always run, so it cannot complete the
    // literal.
    for stat in block.get_stats() {
        let LuaStat::AssignStat(assign) = stat else {
            continue;
        };
        for assigned in assign.get_var_and_expr_list().0 {
            let LuaVarExpr::IndexExpr(index) = assigned else {
                continue;
            };
            let Some(LuaExpr::NameExpr(prefix)) = index.get_prefix_expr() else {
                continue;
            };
            if prefix.get_name_text().as_deref() != Some(param_name.as_str()) {
                continue;
            }
            if let Some(key) = index.get_index_key() {
                missing.remove(key.get_path_part().as_str());
            }
        }
    }

    Some(())
}

/// A field the literal's metatable `__index` answers is there when read, so it
/// is not missing; whether its type fits is the type check's to report.
fn remove_fields_supplied_by_metatable(
    db: &DbIndex,
    file_id: FileId,
    expr: &LuaTableExpr,
    missing: &mut HashSet<&str>,
) {
    let table = InFiled::new(file_id, expr.get_range());
    missing.retain(|name| {
        let key = name
            .strip_prefix('[')
            .and_then(|index| index.strip_suffix(']'))
            .and_then(|index| index.parse::<i64>().ok())
            .map_or_else(|| LuaMemberKey::Name((*name).into()), LuaMemberKey::Integer);
        matches!(
            find_metatable_index_field(db, &table, &key),
            MetatableIndexField::Absent
        )
    });
}

/// A literal an untyped local holds is expected to be what the local's typed call
/// uses take, so like those uses it counts a field its table is given before each
/// of them on the way there: `local trace = { mins = m }` filled with
/// `trace.start = p` ahead of `util.TraceHull(trace)` in the same function.
fn remove_fields_gained_before_use(
    semantic_model: &SemanticModel,
    expr: &LuaTableExpr,
    missing: &mut HashSet<&str>,
) {
    let db = semantic_model.get_db();
    let file_id = semantic_model.get_file_id();
    let table = InFiled::new(file_id, expr.get_range());
    let Some(decl_id) = expr
        .get_parent::<glua_parser::LuaLocalStat>()
        .and_then(|local| {
            let index = local
                .get_value_exprs()
                .position(|value| value.syntax() == expr.syntax())?;
            let name = local.get_local_name_list().nth(index)?;
            Some(crate::LuaDeclId::new(file_id, name.get_position()))
        })
    else {
        return;
    };
    if !db
        .get_type_index()
        .get_type_cache(&decl_id.into())
        .is_some_and(|cache| cache.as_type() == &LuaType::TableConst(table.clone()))
    {
        return;
    }
    let Ok(uses) = crate::semantic::local_call_reference_expectations(
        db,
        &mut semantic_model.get_cache().borrow_mut(),
        decl_id,
    ) else {
        return;
    };
    if uses.is_empty() {
        return;
    }
    let Some(members) = db
        .get_member_index()
        .get_members(&LuaMemberOwner::Element(table))
    else {
        return;
    };
    let root = semantic_model.get_root().syntax().clone();
    let mut writes: HashMap<String, Vec<glua_parser::LuaSyntaxNode>> = HashMap::new();
    for member in members {
        let name = member.get_key().to_path();
        if !missing.contains(name.as_str()) {
            continue;
        }
        if let Some(node) = member.get_id().get_syntax_id().to_node_from_root(&root) {
            writes.entry(name).or_default().push(node);
        }
    }
    missing.retain(|name| {
        let Some(writes) = writes.get(*name) else {
            return true;
        };
        !uses.iter().all(|(use_range, _)| {
            let use_node = root.covering_element(*use_range);
            writes
                .iter()
                .any(|write| write_runs_before_use(write, &use_node, use_range.start()))
        })
    });
}

/// Whether the write `write` always runs before the read at `use_start`: its
/// statement ends earlier in the same function, in a block that also holds the read
/// or one only `do ... end` blocks separate from it.
fn write_runs_before_use(
    write: &glua_parser::LuaSyntaxNode,
    use_node: &rowan::NodeOrToken<glua_parser::LuaSyntaxNode, glua_parser::LuaSyntaxToken>,
    use_start: rowan::TextSize,
) -> bool {
    let function_of = |node: Option<glua_parser::LuaSyntaxNode>| {
        node.and_then(|node| node.ancestors().find_map(glua_parser::LuaClosureExpr::cast))
    };
    let Some(stat) = write.ancestors().find_map(LuaStat::cast) else {
        return false;
    };
    if stat.syntax().text_range().end() > use_start
        || function_of(Some(write.clone())) != function_of(use_node.parent())
    {
        return false;
    }
    let mut block = stat.get_parent::<glua_parser::LuaBlock>();
    while let Some(current) = block {
        if current.syntax().text_range().contains(use_start) {
            return true;
        }
        block = current
            .get_parent::<glua_parser::LuaDoStat>()
            .and_then(|do_stat| do_stat.get_parent::<glua_parser::LuaBlock>());
    }
    false
}

/// The path the literal is assigned to, e.g. `self.portals` or `cfg`.
fn assigned_target_path(expr: &LuaTableExpr, stat: &LuaStat) -> Option<String> {
    match stat {
        LuaStat::AssignStat(assign) => {
            let (vars, exprs) = assign.get_var_and_expr_list();
            let index = exprs
                .iter()
                .position(|value| value.syntax() == expr.syntax())?;
            vars.get(index)?.get_access_path().map(Into::into)
        }
        LuaStat::LocalStat(local) => Some(
            local
                .get_local_name_by_value(LuaExpr::TableExpr(expr.clone()))?
                .get_name_token()?
                .get_name_text()
                .to_string(),
        ),
        _ => None,
    }
}

fn get_precomputed_required_fields(
    context: &DiagnosticContext,
    type_decl_id: &LuaTypeDeclId,
) -> Option<Arc<HashSet<String>>> {
    context
        .get_shared_data_arc()
        .and_then(|shared| shared.missing_required_fields.get(type_decl_id).cloned())
}

/// The values a trailing call or `...` fills the positional slots with, from
/// slot `first` onward.
struct SpreadSlots {
    first: usize,
    values: VariadicType,
}

impl SpreadSlots {
    fn fills(&self, field_name: &str) -> bool {
        field_name
            .strip_prefix('[')
            .and_then(|name| name.strip_suffix(']'))
            .and_then(|slot| slot.parse::<usize>().ok())
            .and_then(|slot| slot.checked_sub(self.first))
            .is_some_and(|offset| self.values.get_type(offset).is_some())
    }
}

fn trailing_spread_slots(
    semantic_model: &SemanticModel,
    fields: &[LuaTableField],
) -> Option<SpreadSlots> {
    let (last, preceding) = fields.split_last()?;
    if !last.is_value_field() {
        return None;
    }
    let fact = semantic_model.infer_expr_fact(last.get_value_expr()?);
    let LuaType::Variadic(values) = fact.typ() else {
        return None;
    };
    let first = preceding
        .iter()
        .filter(|field| field.is_value_field())
        .count()
        + 1;
    Some(SpreadSlots {
        first,
        values: values.as_ref().clone(),
    })
}

fn table_literal_looks_array_like(fields: &[LuaTableField]) -> bool {
    if fields.is_empty() {
        return false;
    }

    fields.iter().all(|field| {
        if field.is_value_field() {
            return true;
        }

        matches!(
            field.get_field_key(),
            Some(LuaIndexKey::Idx(_) | LuaIndexKey::Integer(_))
        )
    })
}

fn get_required_fields(
    context: &mut DiagnosticContext,
    // types should be breadth-first: subtypes are visited before supertypes, and subtypes have higher priority than supertypes
    types: &[LuaType],
) -> Option<HashSet<String>> {
    let db = context.db;
    get_required_fields_for_types(db, types, || context.is_cancelled())
}

fn get_required_fields_for_types(
    db: &DbIndex,
    // types should be breadth-first: subtypes are visited before supertypes, and subtypes have higher priority than supertypes
    types: &[LuaType],
    mut is_cancelled: impl FnMut() -> bool,
) -> Option<HashSet<String>> {
    Some(fold_required_fields(
        types,
        &mut is_cancelled,
        |type_decl_id, is_cancelled| {
            own_field_requirements(db, type_decl_id, is_cancelled).map(Arc::new)
        },
    ))
}

/// What one type's own members require of a table literal, and the names
/// they leave optional to write.
#[derive(Debug, Default, PartialEq)]
pub struct OwnFieldRequirements {
    required: HashSet<String>,
    optional: HashSet<String>,
}

/// `type_decl_id`'s own field requirements: `None` when it has no member map
/// or the walk was cancelled.
pub fn own_field_requirements(
    db: &DbIndex,
    type_decl_id: &LuaTypeDeclId,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Option<OwnFieldRequirements> {
    let members = db
        .get_member_index()
        .get_members(&LuaMemberOwner::Type(type_decl_id.clone()))?;
    let mut own = OwnFieldRequirements::default();
    for member in members {
        if is_cancelled() {
            return None;
        }
        if matches!(
            member.get_feature(),
            LuaMemberFeature::FileMethodDecl | LuaMemberFeature::MetaMethodDecl
        ) {
            continue;
        }
        let name = member.get_key().to_path();

        // A field the class never declared, attached by a plain `v.X = ...`
        // write somewhere, is an addition to a value — not part of the
        // contract a constructor has to satisfy. Only what the class
        // declares can be required of a literal; runtime writes make the
        // field available to read, at most optional to write.
        if member.is_assignment_define() {
            own.optional.insert(name);
            continue;
        }
        let decl_type = db
            .get_type_index()
            .get_type_cache(&member.get_id().into())
            .unwrap_or(&LuaTypeCache::InferType(LuaType::Unknown))
            .as_type()
            .clone();

        // Treat fields with documented defaults as optional-for-writing,
        // matching the semantic table-compatibility rule in type_check.
        let has_default = {
            let owner_id = LuaSemanticDeclId::Member(member.get_id());
            db.get_property_index()
                .get_property(&owner_id)
                .is_some_and(|p| p.default_value().is_some())
                || match &decl_type {
                    LuaType::Signature(sig_id) => db
                        .get_property_index()
                        .get_property(&LuaSemanticDeclId::Signature(sig_id.clone()))
                        .is_some_and(|p| p.default_value().is_some()),
                    _ => false,
                }
        };
        if has_default {
            own.optional.insert(name);
            continue;
        }

        record_required_fields(&mut own.required, &mut own.optional, name, decl_type);
    }
    Some(own)
}

/// The type whose own requirements a super chain entry contributes.
pub fn own_fields_owner(typ: &LuaType) -> Option<&LuaTypeDeclId> {
    match typ {
        LuaType::Ref(type_decl_id) => Some(type_decl_id),
        LuaType::Generic(generic_type) => Some(generic_type.get_base_type_id_ref()),
        _ => None,
    }
}

/// The fields a table literal of `types` must set, folding each type's own
/// requirements (`own`) in order: earlier types take priority.
pub fn fold_required_fields<C: FnMut() -> bool>(
    types: &[LuaType],
    is_cancelled: &mut C,
    mut own: impl FnMut(&LuaTypeDeclId, &mut C) -> Option<Arc<OwnFieldRequirements>>,
) -> HashSet<String> {
    let mut required_fields: HashSet<String> = HashSet::default();
    let mut optional_type = HashSet::default();
    for super_type in types {
        if is_cancelled() {
            return required_fields;
        }
        // Handle ---@class test: { a: number }
        if let LuaType::Object(object_type) = super_type {
            for (key, decl_type) in object_type.get_fields() {
                if is_cancelled() {
                    return required_fields;
                }
                record_required_fields(
                    &mut required_fields,
                    &mut optional_type,
                    key.to_path(),
                    decl_type.clone(),
                );
            }
            continue;
        }
        let Some(owned) = own_fields_owner(super_type).and_then(|owner| own(owner, is_cancelled))
        else {
            continue;
        };
        optional_type.extend(owned.optional.iter().cloned());
        for name in &owned.required {
            if !optional_type.contains(name) {
                required_fields.insert(name.clone());
            }
        }
    }
    required_fields
}

fn record_required_fields(
    required_fields: &mut HashSet<String>,
    optional_type: &mut HashSet<String>,
    name: String,
    decl_type: LuaType,
) {
    if name.is_empty() {
        return;
    }

    if decl_type.is_nullable() || decl_type.is_any() {
        optional_type.insert(name);
        return;
    }
    if optional_type.contains(&name) {
        return;
    }

    required_fields.insert(name);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use googletest::prelude::*;
    use tokio_util::sync::CancellationToken;

    use super::{SpreadSlots, get_required_fields};
    use crate::{
        DiagnosticCode, Emmyrc, LuaType, VariadicType,
        diagnostic::lua_diagnostic_config::LuaDiagnosticConfig, test_lib::VirtualWorkspace,
    };

    /// A spread short of the end of the values gives one of them, as the type
    /// check reads the values; only the last spread reaches every later slot.
    #[test]
    fn a_spread_short_of_the_end_fills_one_slot() {
        let slots = SpreadSlots {
            first: 2,
            values: VariadicType::Multi(vec![
                LuaType::Variadic(Arc::new(VariadicType::Base(LuaType::Number))),
                LuaType::String,
            ]),
        };
        assert!(!slots.fills("[1]"));
        assert!(slots.fills("[2]"));
        assert!(slots.fills("[3]"));
        assert!(!slots.fills("[4]"));
    }

    #[gtest]
    fn get_required_fields_stops_when_cancelled() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
---@class Parent
---@field foo number

---@class Child: Parent
local value = {} ---@type Child
"#,
        );
        let child_type = ws.ty("Child");
        let LuaType::Ref(type_decl_id) = &child_type else {
            panic!("expected Child to resolve to a ref type");
        };

        let mut emmyrc = Emmyrc::default();
        emmyrc
            .diagnostics
            .enables
            .push(DiagnosticCode::MissingFields);
        let cancel_token = CancellationToken::new();
        cancel_token.cancel();
        let db = ws.analysis.compilation.get_db();
        let mut context = super::DiagnosticContext::new(
            file_id,
            db,
            Arc::new(LuaDiagnosticConfig::new(&emmyrc)),
            cancel_token,
        );
        let super_types = type_decl_id.collect_super_types_with_self(db, child_type.clone());

        let required_fields = get_required_fields(&mut context, &super_types)
            .expect("cancelled traversal should still return partial results");

        assert_that!(required_fields, is_empty());
    }
}
