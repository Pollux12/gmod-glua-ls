use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use glua_parser::{
    LuaAstNode, LuaDocTagField, LuaIndexExpr, LuaStat, LuaSyntaxKind, LuaSyntaxNode,
};

use crate::{
    DiagnosticCode, LuaDecl, LuaDeclExtra, LuaMember, LuaMemberFeature, LuaMemberKey,
    LuaSemanticDeclId, LuaType, LuaTypeDeclId, SemanticDeclLevel, SemanticModel,
};

use super::{Checker, DiagnosticContext};

pub struct DuplicateFieldChecker;

impl Checker for DuplicateFieldChecker {
    const CODES: &[DiagnosticCode] = &[
        DiagnosticCode::DuplicateDocField,
        DiagnosticCode::DuplicateSetField,
    ];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let decl_set = get_decl_set(context, semantic_model);
        if let Some(decl_set) = decl_set {
            for decl_info in decl_set {
                if context.is_cancelled() {
                    return;
                }
                check_decl_duplicate_field(context, semantic_model, &decl_info);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DeclInfo {
    id: LuaTypeDeclId,
    is_require: bool,
}

fn get_decl_set(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
) -> Option<HashSet<DeclInfo>> {
    let file_id = semantic_model.get_file_id();
    let decl_tree = semantic_model
        .get_db()
        .get_decl_index()
        .get_decl_tree(&file_id)?;
    let mut type_decl_id_set = HashSet::default();
    for (decl_id, decl) in decl_tree.get_decls() {
        if context.is_cancelled() {
            return Some(type_decl_id_set);
        }
        if matches!(
            &decl.extra,
            LuaDeclExtra::Local { .. } | LuaDeclExtra::Global { .. }
        ) {
            let decl_type = semantic_model.get_type((*decl_id).into());
            match decl_type {
                LuaType::Def(id) => {
                    type_decl_id_set.insert(DeclInfo {
                        id,
                        is_require: is_require_decl(decl),
                    });
                }
                LuaType::Ref(id) => {
                    if is_require_decl(decl) {
                        type_decl_id_set.insert(DeclInfo {
                            id,
                            is_require: true,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    let Some(buckets) = context.file_buckets() else {
        return Some(type_decl_id_set);
    };
    for tag_class in buckets.doc_tag_classes.iter() {
        if context.is_cancelled() {
            return Some(type_decl_id_set);
        }
        if let Some(class_name) = tag_class.get_name_token() {
            type_decl_id_set.insert(DeclInfo {
                id: LuaTypeDeclId::global(class_name.get_name_text()),
                is_require: false,
            });
        }
    }

    Some(type_decl_id_set)
}

fn is_require_decl(decl: &LuaDecl) -> bool {
    let Some(expr_id) = decl.get_value_syntax_id() else {
        return false;
    };
    expr_id.get_kind() == LuaSyntaxKind::RequireCallExpr
}

struct DiagnosticMemberInfo<'a> {
    typ: LuaType,
    feature: LuaMemberFeature,
    member: &'a LuaMember,
}

fn check_decl_duplicate_field(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    decl_info: &DeclInfo,
) -> Option<()> {
    if context.is_cancelled() {
        return Some(());
    }

    let type_decl = context
        .get_db()
        .get_type_index()
        .get_type_decl(&decl_info.id)?;
    let file_id = context.file_id;

    let members = semantic_model
        .get_db()
        .get_member_index()
        .get_members(&type_decl.get_id().into())?;

    let mut member_map: HashMap<&LuaMemberKey, Vec<&LuaMember>> = HashMap::default();

    for member in members.iter() {
        if context.is_cancelled() {
            return Some(());
        }
        // Filter out signatures defined by meta
        if member.get_feature() == LuaMemberFeature::MetaMethodDecl {
            continue;
        }
        if semantic_model
            .get_db()
            .get_gmod_load_index()
            .files_are_mutually_exclusive_by_load_shadowing(file_id, member.get_file_id())
        {
            continue;
        }

        member_map
            .entry(member.get_key())
            .or_default()
            .push(*member);
    }

    for (key, members) in member_map.iter() {
        if context.is_cancelled() {
            return Some(());
        }
        if members.len() < 2 {
            // Needs special handling: require("a").fun = function() end
            if let Some(member) = members.first() {
                check_one_member(context, semantic_model, member, decl_info.is_require);
            }
            continue;
        }

        let mut member_infos = Vec::with_capacity(members.len());
        for member in members.iter() {
            if context.is_cancelled() {
                return Some(());
            }
            let typ = semantic_model.get_type(member.get_id().into());
            let feature = member.get_feature();
            member_infos.push(DiagnosticMemberInfo {
                typ,
                feature,
                member,
            });
        }

        // 1. Check signature
        let signatures = member_infos.iter().filter(|info| {
            matches!(info.typ, LuaType::Signature(_)) && !info.member.is_assignment_define()
        });
        if signatures.clone().count() > 1 {
            for signature in signatures {
                if context.is_cancelled() {
                    return Some(());
                }
                if signature.member.get_file_id() != file_id {
                    continue;
                }
                context.add_diagnostic(
                    DiagnosticCode::DuplicateSetField,
                    signature.member.get_range(),
                    format!("Duplicate field `{name}`.", name = key.to_path()).to_string(),
                    None,
                );
            }
        }

        // 2. Check ---@field members
        let field_decls = member_infos
            .iter()
            .filter(|info| info.feature.is_field_decl() && is_doc_field_tag(info.member))
            .collect::<Vec<_>>();
        // If there is more than one field_decl, check further
        if field_decls.len() > 1 {
            // Check whether all field_decls are DocFunction
            let all_doc_functions = field_decls
                .iter()
                .all(|info| matches!(info.typ, LuaType::DocFunction(_)));

            // If not all are DocFunction, report an error
            if !all_doc_functions {
                for field_decl in &field_decls {
                    if context.is_cancelled() {
                        return Some(());
                    }
                    if field_decl.member.get_file_id() == file_id {
                        context.add_diagnostic(
                            DiagnosticCode::DuplicateDocField,
                            // TODO: narrow the range to the name instead of the whole ---@field
                            field_decl.member.get_range(),
                            format!("Duplicate field `{name}`.", name = key.to_path()).to_string(),
                            None,
                        );
                    }
                }
            }
        }
    }

    Some(())
}

/// Whether `member` is a field a doc tag declares. Members the analysis
/// synthesizes (a registered panel's `BaseClass`, module and global-path
/// members) share the field-declaration feature but no author wrote them.
fn is_doc_field_tag(member: &LuaMember) -> bool {
    matches!(
        member.get_syntax_id().get_kind(),
        LuaSyntaxKind::DocTagField | LuaSyntaxKind::DocEnumField
    )
}

/// Special handling: require("a").fun = function() end
fn check_one_member(
    context: &mut DiagnosticContext,
    semantic_model: &SemanticModel,
    member: &LuaMember,
    is_require: bool,
) -> Option<()> {
    if context.is_cancelled() {
        return Some(());
    }

    if !is_require {
        return None;
    }
    let key = member.get_key();
    let member_id = member.get_id();
    let typ = semantic_model.get_type(member.get_id().into());
    // No check needed if it is not a signature
    if !matches!(typ, LuaType::Signature(_)) {
        return None;
    }
    let references = semantic_model
        .get_db()
        .get_reference_index()
        .get_index_references(key)?;
    let root = semantic_model.get_root().syntax();
    let property_owner = LuaSemanticDeclId::Member(member_id);

    for in_filed in references {
        if context.is_cancelled() {
            return Some(());
        }
        // Do not check across different files
        if in_filed.file_id != context.file_id {
            continue;
        }
        // No need to check itself
        if in_filed.value == *member_id.get_syntax_id() {
            continue;
        }
        let node = in_filed.value.to_node_from_root(root)?;

        if !semantic_model.is_reference_to(
            node.clone(),
            property_owner.clone(),
            SemanticDeclLevel::default(),
        ) {
            continue;
        }

        // No check needed if it is not an assignment
        if check_function_member_is_set(semantic_model, &node, is_require).is_none() {
            continue;
        }

        context.add_diagnostic(
            DiagnosticCode::DuplicateSetField,
            in_filed.value.get_range(),
            format!("Duplicate field `{name}`.", name = key.to_path()).to_string(),
            None,
        );
    }

    Some(())
}

/// Checks whether it is require("a").member = newValue
fn check_function_member_is_set(
    semantic_model: &SemanticModel,
    node: &LuaSyntaxNode,
    is_require: bool,
) -> Option<()> {
    match node {
        expr_node if LuaIndexExpr::can_cast(expr_node.kind().into()) => {
            let expr = LuaIndexExpr::cast(expr_node.clone())?;
            let prefix_type = semantic_model.infer_expr(expr.get_prefix_expr()?).ok()?;
            if let LuaType::Def(_) = prefix_type {
                return None;
            }
            // Look upwards for the stat node
            let stat = expr.ancestors::<LuaStat>().next()?;
            match stat {
                LuaStat::FuncStat(_) => {
                    return Some(());
                }
                LuaStat::AssignStat(assign_stat) => {
                    // Determine whether it is on the left side
                    let (vars, exprs) = assign_stat.get_var_and_expr_list();
                    for (i, var) in vars.iter().enumerate() {
                        if var
                            .syntax()
                            .text_range()
                            .contains(node.text_range().start())
                        {
                            // If imported via require, treat it as a duplicate field directly
                            if is_require {
                                return Some(());
                            }
                            // Determine whether the right-hand expression is a signature
                            if let Some(expr) = exprs.get(i) {
                                let expr_type = semantic_model.infer_expr(expr.clone()).ok()?;
                                if matches!(expr_type, LuaType::Signature(_)) {
                                    return Some(());
                                }
                            }
                            return None;
                        }
                    }
                    return None;
                }
                _ => {
                    return None;
                }
            }
        }
        tag_field_node if LuaDocTagField::can_cast(tag_field_node.kind().into()) => {
            return Some(());
        }
        _ => {}
    }

    Some(())
}
