use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use glua_parser::{
    LuaAssignStat, LuaAst, LuaAstNode, LuaExpr, LuaIndexKey, LuaStat, LuaTableExpr, LuaTableField,
    LuaVarExpr, PathTrait,
};

use crate::{
    DbIndex, DiagnosticCode, LuaDeclId, LuaMemberFeature, LuaMemberOwner, LuaSemanticDeclId,
    LuaType, LuaTypeCache, LuaTypeDeclId, SemanticModel,
};

use super::{Checker, DiagnosticContext, PrecomputedMissingRequiredFields, humanize_lint_type};
use itertools::Itertools;

pub struct MissingFieldsChecker;

impl Checker for MissingFieldsChecker {
    const CODES: &[DiagnosticCode] = &[DiagnosticCode::MissingFields];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let root = semantic_model.get_root().clone();

        let mut type_cache: HashMap<LuaType, Arc<HashSet<String>>> = HashMap::new();
        for expr in root.descendants::<LuaTableExpr>() {
            if context.is_cancelled() {
                return;
            }
            check_table_expr(context, semantic_model, &expr, &mut type_cache);
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

    // If the table literal is initializing a local variable, only check missing-fields
    // if that local variable declaration has an explicit type annotation.
    // Unannotated local tables (e.g. `local layout = {}`) may be used as out-parameters
    // or buffers whose inferred type comes purely from downstream call references.
    if let Some(LuaAst::LuaLocalStat(local_stat)) = expr.get_parent::<LuaAst>() {
        let num = local_stat
            .get_value_exprs()
            .position(|val| val.get_position() == expr.get_position());
        if let Some(num) = num
            && let Some(local_name) = local_stat.get_local_name_list().nth(num)
        {
            let decl_id = LuaDeclId::new(semantic_model.get_file_id(), local_name.get_position());
            let has_explicit_type = db
                .get_type_index()
                .get_type_cache(&decl_id.into())
                .is_some_and(|tc| tc.is_doc());
            if !has_explicit_type {
                return Some(());
            }
        }
    }

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

    let current_fields = fields
        .iter()
        .filter_map(|field| field.get_field_key().map(|key| key.get_path_part()))
        .collect();

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
                let mut computed_fields = HashSet::new();
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

    let mut missing: HashSet<&str> = required_fields
        .difference(&current_fields)
        .map(String::as_str)
        .collect();
    if missing.is_empty() {
        return Some(());
    }

    remove_fields_completed_after(expr, &mut missing);
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
fn remove_fields_completed_after(expr: &LuaTableExpr, missing: &mut HashSet<&str>) -> Option<()> {
    let stat = expr.ancestors::<LuaStat>().next()?;
    let target = assigned_target_path(expr, &stat)?;

    let mut next = stat.syntax().next_sibling();
    while let Some(node) = next {
        if missing.is_empty() {
            break;
        }

        let mut written_prefixes = Vec::new();
        if let Some(assign) = LuaAssignStat::cast(node.clone()) {
            for var in assign.get_var_and_expr_list().0 {
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
                    missing.remove(key.get_path_part().as_str());
                }
            }
        }

        // Anything else touching the target consumes it while it is still
        // incomplete, so later writes no longer make the literal complete.
        let escapes = node.descendants().filter_map(LuaVarExpr::cast).any(|var| {
            var.get_access_path().as_deref() == Some(target.as_str())
                && !written_prefixes.contains(var.syntax())
        });
        if escapes {
            break;
        }

        next = node.next_sibling();
    }

    Some(())
}

/// The path the literal is assigned to, e.g. `self.portals` or `cfg`.
fn assigned_target_path(expr: &LuaTableExpr, stat: &LuaStat) -> Option<String> {
    match stat {
        LuaStat::AssignStat(assign) => {
            let (vars, exprs) = assign.get_var_and_expr_list();
            let index = exprs
                .iter()
                .position(|value| value.syntax() == expr.syntax())?;
            vars.get(index)?.get_access_path()
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

pub fn precompute_missing_required_fields(db: &DbIndex) -> PrecomputedMissingRequiredFields {
    db.get_type_index()
        .get_all_types()
        .into_iter()
        .filter_map(|type_decl| {
            let type_decl_id = type_decl.get_id();
            let typ = LuaType::Ref(type_decl_id.clone());
            let types = type_decl_id.collect_super_types_with_self(db, typ);
            let required_fields = get_required_fields_for_types(db, &types, || false)?;
            Some((type_decl_id, Arc::new(required_fields)))
        })
        .collect()
}

fn get_precomputed_required_fields(
    context: &DiagnosticContext,
    type_decl_id: &LuaTypeDeclId,
) -> Option<Arc<HashSet<String>>> {
    context
        .get_shared_data_arc()
        .and_then(|shared| shared.missing_required_fields.get(type_decl_id).cloned())
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
    // types 应为广度优先, 子类型会先于父类型被遍历, 而子类型的优先级高于父类型
    types: &[LuaType],
) -> Option<HashSet<String>> {
    let db = context.db;
    get_required_fields_for_types(db, types, || context.is_cancelled())
}

fn get_required_fields_for_types(
    db: &DbIndex,
    // types 应为广度优先, 子类型会先于父类型被遍历, 而子类型的优先级高于父类型
    types: &[LuaType],
    mut is_cancelled: impl FnMut() -> bool,
) -> Option<HashSet<String>> {
    let member_index = db.get_member_index();
    let mut required_fields: HashSet<String> = HashSet::new();

    let mut optional_type = HashSet::new();
    for super_type in types {
        if is_cancelled() {
            return Some(required_fields);
        }
        match super_type {
            LuaType::Ref(type_decl_id) => process_type_decl_id(
                db,
                member_index,
                &mut required_fields,
                &mut optional_type,
                type_decl_id.clone(),
                &mut is_cancelled,
            ),
            LuaType::Generic(generic_type) => process_type_decl_id(
                db,
                member_index,
                &mut required_fields,
                &mut optional_type,
                generic_type.get_base_type_id().clone(),
                &mut is_cancelled,
            ),
            // 处理 ---@class test: { a: number }
            LuaType::Object(object_type) => {
                let fields = object_type.get_fields();
                for (key, decl_type) in fields {
                    if is_cancelled() {
                        return Some(required_fields);
                    }
                    let name = key.to_path();
                    record_required_fields(
                        &mut required_fields,
                        &mut optional_type,
                        name,
                        decl_type.clone(),
                    );
                }
                continue;
            }
            _ => continue,
        };
    }

    fn process_type_decl_id(
        db: &DbIndex,
        member_index: &crate::LuaMemberIndex,
        required_fields: &mut HashSet<String>,
        optional_type: &mut HashSet<String>,
        type_decl_id: LuaTypeDeclId,
        is_cancelled: &mut impl FnMut() -> bool,
    ) -> Option<()> {
        let members = member_index.get_members(&LuaMemberOwner::Type(type_decl_id))?;
        let mut type_required_fields = HashSet::new();
        let mut type_optional_fields = HashSet::new();

        for member in members {
            if is_cancelled() {
                return Some(());
            }
            if matches!(
                member.get_feature(),
                LuaMemberFeature::FileMethodDecl | LuaMemberFeature::MetaMethodDecl
            ) {
                continue;
            }
            let name = member.get_key().to_path();
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
                type_optional_fields.insert(name);
                continue;
            }

            record_required_fields(
                &mut type_required_fields,
                &mut type_optional_fields,
                name,
                decl_type,
            );
        }

        optional_type.extend(type_optional_fields.iter().cloned());
        for name in type_required_fields {
            if !optional_type.contains(&name) {
                required_fields.insert(name);
            }
        }

        Some(())
    }

    Some(required_fields)
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

    use super::get_required_fields;
    use crate::{
        DiagnosticCode, Emmyrc, LuaType, diagnostic::lua_diagnostic_config::LuaDiagnosticConfig,
        test_lib::VirtualWorkspace,
    };

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
