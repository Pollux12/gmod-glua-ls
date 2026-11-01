use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaAstToken, LuaCommentOwner, LuaDocTag, LuaExpr, LuaForRangeStat,
    LuaForStat, LuaFuncStat, LuaIndexExpr, LuaIndexKey, LuaLocalFuncStat, LuaLocalStat,
    LuaSyntaxId, LuaSyntaxKind, LuaTableExpr, LuaVarExpr, NumberResult, path_segment,
};
use smol_str::SmolStr;

use crate::{
    GlobalId, InFiled, LuaDeclExtra, LuaMemberFeature, LuaMemberId, LuaSemanticDeclId,
    LuaSignatureId, LuaType, LuaTypeCache,
    compilation::analyzer::common::{expr_expands_to_fill_slots, is_nil_literal_expr},
    db_index::{
        LocalAttribute, LuaDecl, LuaDeclInitializer, LuaMember, LuaMemberKey, LuaMemberOwner,
    },
};

use super::{DeclAnalyzer, DeclOp, members::find_index_owner};

pub fn analyze_local_stat(analyzer: &mut DeclAnalyzer, stat: LuaLocalStat) -> Option<()> {
    let local_name_list = stat.get_local_name_list().collect::<Vec<_>>();
    let value_expr_list = stat.get_value_exprs().collect::<Vec<_>>();

    for (index, local_name) in local_name_list.iter().enumerate() {
        let name = if let Some(name_token) = local_name.get_name_token() {
            name_token.get_name_text().to_string()
        } else {
            continue;
        };
        let attrib = if let Some(attrib) = local_name.get_attrib() {
            if attrib.is_const() {
                Some(LocalAttribute::Const)
            } else if attrib.is_close() {
                Some(LocalAttribute::Close)
            } else {
                None
            }
        } else {
            None
        };

        let file_id = analyzer.get_file_id();
        let range = local_name.get_range();
        let expr_id = value_expr_list.get(index).map(|expr| expr.get_syntax_id());
        let last_value_index = value_expr_list.len().saturating_sub(1);
        let initializer = if value_expr_list.is_empty() {
            None
        } else if index < last_value_index {
            value_expr_list
                .get(index)
                .map(|expr| LuaDeclInitializer::new(expr.get_syntax_id(), 0))
        } else {
            value_expr_list
                .last()
                .map(|expr| LuaDeclInitializer::new(expr.get_syntax_id(), index - last_value_index))
        };

        let mut decl = LuaDecl::new(
            &name,
            file_id,
            range,
            LuaDeclExtra::Local {
                kind: local_name.syntax().kind(),
                attrib,
            },
            expr_id,
        );
        decl.set_initializer(initializer);
        analyzer.add_decl(decl);
    }

    Some(())
}

/// The `{}` a global is initialised with: written directly (`X = {}`) or as the
/// fallback branch of the GLua-idiomatic `X = X or {}`.
fn initializer_table_expr(expr: &LuaExpr) -> Option<LuaTableExpr> {
    match expr {
        LuaExpr::TableExpr(table_expr) => Some(table_expr.clone()),
        LuaExpr::BinaryExpr(binary_expr) => {
            let (left, right) = binary_expr.get_exprs()?;
            [left, right].into_iter().find_map(|operand| match operand {
                LuaExpr::TableExpr(table_expr) => Some(table_expr),
                _ => None,
            })
        }
        _ => None,
    }
}

/// Registers the literal a global path is written with as a definition site of
/// the path, and each of its fields written with a literal as one of the
/// nested path.
///
/// The `configuration = {}` in `cityrp = cityrp or { configuration = {} }`
/// initialises `cityrp.configuration` exactly as `cityrp.configuration = {}`
/// would. Without the nested site a write through the literal, which is what
/// a re-index resolves the prefix to, and a write through the path, which is
/// what a cold walk files it under, would be two owners for one slot.
fn set_definition_sites(
    ops: &mut Vec<DeclOp>,
    path: GlobalId,
    table_expr: &LuaTableExpr,
    file_id: crate::FileId,
) {
    ops.push(DeclOp::DefinitionSite {
        path: path.clone(),
        site: InFiled::new(file_id, table_expr.get_range()),
    });
    for field in table_expr.get_fields() {
        let segment = match field.get_field_key() {
            Some(LuaIndexKey::Name(name)) => SmolStr::new(name.get_name_text()),
            Some(LuaIndexKey::String(text)) => path_segment(&text.get_value()),
            _ => continue,
        };
        let Some(nested) = field
            .get_value_expr()
            .as_ref()
            .and_then(initializer_table_expr)
        else {
            continue;
        };
        let nested_path = GlobalId::new(&format!("{}.{segment}", path.get_name()));
        set_definition_sites(ops, nested_path, &nested, file_id);
    }
}

pub fn analyze_assign_stat(analyzer: &mut DeclAnalyzer, stat: LuaAssignStat) -> Option<()> {
    // An incomplete statement (`SKIN.Field` with no `=`, common mid-edit) also
    // parses as an assignment. It defines no member, so registering one here
    // would add a valueless definition that shadows the real one for that key
    // and makes hover/completion report `unknown`.
    let defines_members = stat.get_assign_op().is_some();

    let (vars, value_exprs) = stat.get_var_and_expr_list();
    for (idx, var) in vars.iter().enumerate() {
        let value_expr_id = value_exprs.get(idx).map(|expr| expr.get_syntax_id());

        match &var {
            LuaVarExpr::NameExpr(name_expr) => {
                let name_token = name_expr.get_name_token()?;
                let position = name_token.get_position();
                let name = name_token.get_name_text();
                let file_id = analyzer.get_file_id();
                let range = name_token.get_range();
                if name == "_" {
                    let decl = LuaDecl::new(
                        name,
                        file_id,
                        range,
                        LuaDeclExtra::Local {
                            kind: name_expr.syntax().kind(),
                            attrib: Some(LocalAttribute::Const),
                        },
                        value_expr_id,
                    );

                    analyzer.add_decl(decl);
                    continue;
                }

                if let Some(decl) = analyzer.find_decl(name, position) {
                    let is_local_decl = !decl.is_global() && !decl.is_module_scoped();
                    let is_same_file_global_like = (decl.is_global() || decl.is_module_scoped())
                        && decl.get_file_id() == file_id;
                    if is_local_decl || is_same_file_global_like {
                        let decl_id = decl.get_id();
                        analyzer.references.add_decl_reference(decl_id, range, true);
                        continue;
                    }
                }

                let decl = LuaDecl::new(
                    name,
                    file_id,
                    range,
                    LuaDeclExtra::Global {
                        kind: LuaSyntaxKind::NameExpr.into(),
                    },
                    value_expr_id,
                );

                let decl_id = analyzer.add_decl(decl);

                // Record the `{}` this global is written with while the value
                // expression is already in hand: it is a definition site of the
                // path, and the path owns the literal's members.
                if let Some(table_expr) = value_exprs.get(idx).and_then(initializer_table_expr) {
                    let path = match analyzer
                        .decl
                        .get_decl(&decl_id)
                        .and_then(LuaDecl::get_module_path)
                    {
                        Some(module_path) => format!("{module_path}.{name}"),
                        None => name.to_string(),
                    };
                    set_definition_sites(
                        &mut analyzer.ops,
                        GlobalId::new(&path),
                        &table_expr,
                        file_id,
                    );
                }
            }
            LuaVarExpr::IndexExpr(index_expr) => {
                if !defines_members {
                    continue;
                }
                let index_key = index_expr.get_index_key()?;
                let key: LuaMemberKey = match index_key {
                    LuaIndexKey::Name(name) => LuaMemberKey::Name(name.get_name_text().into()),
                    LuaIndexKey::String(str) => LuaMemberKey::Name(str.get_value().into()),
                    LuaIndexKey::Integer(i) => {
                        if let NumberResult::Int(idx) = i.get_number_value() {
                            LuaMemberKey::Integer(idx)
                        } else {
                            continue;
                        }
                    }
                    LuaIndexKey::Idx(idx) => LuaMemberKey::Integer(idx as i64),
                    LuaIndexKey::Expr(_) => {
                        continue;
                    }
                };

                let file_id = analyzer.get_file_id();
                let member_id = LuaMemberId::new(index_expr.get_syntax_id(), file_id);
                let decl_feature = if analyzer.is_meta {
                    LuaMemberFeature::MetaDefine
                } else {
                    LuaMemberFeature::FileDefine
                };

                let (owner, global_id) = find_index_owner(analyzer, index_expr.clone());
                // The `{}` this nested global path is written with is a
                // definition site of the path, exactly as the `NameExpr` branch
                // above records a root global's.
                if let Some(global_id) = &global_id
                    && let Some(table_expr) = value_exprs.get(idx).and_then(initializer_table_expr)
                {
                    set_definition_sites(
                        &mut analyzer.ops,
                        global_id.clone(),
                        &table_expr,
                        file_id,
                    );
                }
                let member = LuaMember::new(member_id, key.clone(), decl_feature, global_id);

                // `t[k] = nil` is a delete, not a member write: the value walk
                // deliberately skips it, so a record minted here would sit in
                // the owner set forever unresolvable. A var the value list
                // does not reach gets `nil` the same way unless the tail
                // expression can expand to fill it. A doc tag that binds a
                // type to the var keeps the member: the write is still a
                // runtime delete, but the annotation supplies the member's
                // type cache, so the record resolves.
                let is_nil_delete = match value_exprs.get(idx) {
                    Some(expr) => is_nil_literal_expr(expr),
                    None => !value_exprs.last().is_some_and(expr_expands_to_fill_slots),
                };
                if !is_nil_delete || assign_stat_var_carries_doc_type(&stat, idx) {
                    analyzer.add_member(owner, member);
                }
                if let LuaMemberKey::Name(name) = &key {
                    analyze_maybe_global_index_expr(analyzer, index_expr, name, value_expr_id);
                }
            }
        }
    }

    Some(())
}

/// True when a doc tag attached to the assign statement will write the type
/// cache of the member bound at var position `var_idx`. Mirrors the doc
/// analyzer's binding rules: `---@type` binds vars positionally, while
/// `---@class`, `---@enum`, `---@module`, and `---@schema` bind the first var
/// only. Other tags (`---@deprecated`, `---@field`, ...) write properties or
/// unrelated owners and never make the member resolvable.
fn assign_stat_var_carries_doc_type(stat: &LuaAssignStat, var_idx: usize) -> bool {
    stat.get_comments()
        .iter()
        .flat_map(|comment| comment.get_doc_tags())
        .any(|tag| match tag {
            LuaDocTag::Type(type_tag) => var_idx < type_tag.get_type_list().count(),
            LuaDocTag::Class(_)
            | LuaDocTag::Enum(_)
            | LuaDocTag::Module(_)
            | LuaDocTag::Schema(_) => var_idx == 0,
            _ => false,
        })
}

/// Declares the global that `_G.name` or `_ENV.name` writes. Returns it, or
/// `None` when the prefix is not the global table.
fn analyze_maybe_global_index_expr(
    analyzer: &mut DeclAnalyzer,
    index_expr: &LuaIndexExpr,
    index_name: &str,
    value_expr_id: Option<LuaSyntaxId>,
) -> Option<crate::LuaDeclId> {
    let file_id = analyzer.get_file_id();
    let prefix = index_expr.get_prefix_expr()?;
    let LuaExpr::NameExpr(name_expr) = prefix else {
        return None;
    };
    let name_token = name_expr.get_name_token()?;
    let name_token_text = name_token.get_name_text();
    if name_token_text != "_G" && name_token_text != "_ENV" {
        return None;
    }
    let range = index_expr.get_range();
    let decl_id = crate::LuaDeclId::new(file_id, range.start());
    if let Some(decl) = analyzer.decl.get_decl(&decl_id)
        && decl.is_global()
    {
        let decl_id = decl.get_id();
        analyzer.references.add_decl_reference(decl_id, range, true);
        return Some(decl_id);
    }
    let decl = LuaDecl::new(
        index_name,
        file_id,
        range,
        LuaDeclExtra::Global {
            kind: LuaSyntaxKind::IndexExpr.into(),
        },
        value_expr_id,
    );
    Some(analyzer.add_global_table_decl(decl))
}

pub fn analyze_for_stat(analyzer: &mut DeclAnalyzer, stat: LuaForStat) -> Option<()> {
    let it_var = stat.get_var_name()?;
    let name = it_var.get_name_text();
    let file_id = analyzer.get_file_id();
    let range = it_var.get_range();
    let decl = LuaDecl::new(
        name,
        file_id,
        range,
        LuaDeclExtra::Local {
            kind: it_var.syntax().kind(),
            attrib: Some(LocalAttribute::IterConst),
        },
        None,
    );
    let decl_id = decl.get_id();
    analyzer.add_decl(decl);
    analyzer.ops.push(DeclOp::BindType {
        owner: decl_id.into(),
        cache: LuaTypeCache::DocType(LuaType::Integer),
    });

    Some(())
}

pub fn analyze_for_range_stat(analyzer: &mut DeclAnalyzer, stat: LuaForRangeStat) {
    let var_list = stat.get_var_name_list();
    let file_id = analyzer.get_file_id();
    for var in var_list {
        let name = var.get_name_text();
        let range = var.get_range();

        let decl = LuaDecl::new(
            name,
            file_id,
            range,
            LuaDeclExtra::Local {
                kind: var.syntax().kind(),
                attrib: Some(LocalAttribute::IterConst),
            },
            None,
        );

        analyzer.add_decl(decl);
    }
}

pub fn analyze_func_stat(analyzer: &mut DeclAnalyzer, stat: LuaFuncStat) -> Option<()> {
    let func_name = stat.get_func_name()?;
    let file_id = analyzer.get_file_id();
    let property_owner_id = match func_name {
        LuaVarExpr::NameExpr(name_expr) => {
            let name_token = name_expr.get_name_token()?;
            let position = name_token.get_position();
            let name = name_token.get_name_text();
            let range = name_token.get_range();
            let decl_id = if !analyzer.is_meta {
                if let Some(decl) = analyzer.find_decl(name, position) {
                    if !decl.is_global() {
                        let decl_id = decl.get_id();
                        analyzer.references.add_decl_reference(decl_id, range, true);
                        decl_id
                    } else {
                        let decl = LuaDecl::new(
                            name,
                            file_id,
                            range,
                            LuaDeclExtra::Global {
                                kind: LuaSyntaxKind::NameExpr.into(),
                            },
                            None,
                        );

                        analyzer.add_decl(decl)
                    }
                } else {
                    let decl = LuaDecl::new(
                        name,
                        file_id,
                        range,
                        LuaDeclExtra::Global {
                            kind: LuaSyntaxKind::NameExpr.into(),
                        },
                        None,
                    );

                    analyzer.add_decl(decl)
                }
            } else {
                let decl = LuaDecl::new(
                    name,
                    file_id,
                    range,
                    LuaDeclExtra::Global {
                        kind: LuaSyntaxKind::NameExpr.into(),
                    },
                    None,
                );

                analyzer.add_decl(decl)
            };

            LuaSemanticDeclId::LuaDecl(decl_id)
        }
        LuaVarExpr::IndexExpr(index_expr) => {
            let index_key = index_expr.get_index_key()?;
            let key: LuaMemberKey = match index_key {
                LuaIndexKey::Name(name) => LuaMemberKey::Name(name.get_name_text().into()),
                LuaIndexKey::String(str) => LuaMemberKey::Name(str.get_value().into()),
                LuaIndexKey::Integer(i) => {
                    if let NumberResult::Int(idx) = i.get_number_value() {
                        LuaMemberKey::Integer(idx)
                    } else {
                        return None;
                    }
                }
                LuaIndexKey::Idx(idx) => LuaMemberKey::Integer(idx as i64),
                LuaIndexKey::Expr(_) => {
                    return None;
                }
            };

            let file_id = analyzer.get_file_id();
            let member_id = LuaMemberId::new(index_expr.get_syntax_id(), file_id);
            let decl_feature = if analyzer.is_meta {
                LuaMemberFeature::MetaMethodDecl
            } else {
                LuaMemberFeature::FileMethodDecl
            };

            let (owner_id, global_id) = find_index_owner(analyzer, index_expr.clone());
            let early_owner_decl = if matches!(owner_id, LuaMemberOwner::LocalUnresolve) {
                index_expr
                    .get_prefix_expr()
                    .and_then(|prefix| match prefix {
                        LuaExpr::NameExpr(name_expr) => {
                            let name = name_expr.get_name_text()?;
                            analyzer
                                .find_decl(&name, name_expr.get_position())
                                .map(|decl| decl.get_id())
                        }
                        _ => None,
                    })
            } else {
                None
            };
            let member = LuaMember::new(member_id, key.clone(), decl_feature, global_id);
            let member_id = analyzer.add_member(owner_id, member);

            if let Some(decl_id) = early_owner_decl {
                analyzer
                    .delta
                    .early_member_owner_candidates
                    .push((member_id, decl_id));
            }

            // `function _G.name()` declares the global `name` as well as the
            // member: its type is the signature from the start, like the
            // member's, so no read of the name sees it untyped, and it shares
            // the signature's docs and realm, as the member does.
            if let LuaMemberKey::Name(name) = &key
                && let Some(global_decl_id) =
                    analyze_maybe_global_index_expr(analyzer, &index_expr, name, None)
                && let Some(closure) = stat.get_closure()
            {
                analyzer.ops.push(DeclOp::OwnerMap {
                    source: LuaSemanticDeclId::Member(member_id),
                    same: LuaSemanticDeclId::LuaDecl(global_decl_id),
                });
                analyzer.delta.early_callable_signatures.push((
                    global_decl_id.into(),
                    LuaSignatureId::from_closure(analyzer.get_file_id(), &closure),
                ));
            }
            LuaSemanticDeclId::Member(member_id)
        }
    };

    let closure = stat.get_closure()?;
    let file_id = analyzer.get_file_id();
    let closure_owner_id =
        LuaSemanticDeclId::Signature(LuaSignatureId::from_closure(file_id, &closure));
    analyzer.ops.push(DeclOp::OwnerMap {
        source: property_owner_id.clone(),
        same: closure_owner_id,
    });
    let type_owner = match property_owner_id {
        LuaSemanticDeclId::LuaDecl(decl_id) => decl_id.into(),
        LuaSemanticDeclId::Member(member_id) => member_id.into(),
        _ => return Some(()),
    };
    analyzer
        .delta
        .early_callable_signatures
        .push((type_owner, LuaSignatureId::from_closure(file_id, &closure)));

    Some(())
}

pub fn analyze_local_func_stat(analyzer: &mut DeclAnalyzer, stat: LuaLocalFuncStat) -> Option<()> {
    let local_name = stat.get_local_name()?;
    let name_token = local_name.get_name_token()?;
    let name = name_token.get_name_text();
    let range = local_name.get_range();
    let file_id = analyzer.get_file_id();
    let decl = LuaDecl::new(
        name,
        file_id,
        range,
        LuaDeclExtra::Local {
            kind: local_name.syntax().kind(),
            attrib: None,
        },
        None,
    );

    let decl_id = analyzer.add_decl(decl);
    let closure = stat.get_closure()?;
    let signature_id = LuaSignatureId::from_closure(file_id, &closure);
    let closure_owner_id = LuaSemanticDeclId::Signature(signature_id);
    let property_decl_id = LuaSemanticDeclId::LuaDecl(decl_id);
    analyzer.ops.push(DeclOp::OwnerMap {
        source: property_decl_id,
        same: closure_owner_id,
    });
    analyzer.ops.push(DeclOp::BindLocalFuncDecl {
        signature_id,
        decl_id,
    });
    analyzer
        .delta
        .early_callable_signatures
        .push((decl_id.into(), signature_id));

    Some(())
}
