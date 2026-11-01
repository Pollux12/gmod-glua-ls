use glua_parser::{
    LuaAst, LuaAstNode, LuaDocTagAttributeUse, LuaDocType, LuaExpr, LuaIndexKey, LuaKind,
    LuaLiteralExpr, LuaLiteralToken, LuaLocalName, LuaSyntaxKind, LuaSyntaxNode, LuaTokenKind,
    LuaVarExpr, NumberResult,
};
use smol_str::SmolStr;

use crate::{
    GMOD_ATTR_EXHAUSTIVE_TYPE_GUARD, GMOD_ATTR_VALID_GUARD, GMOD_ATTR_WRITES_GLOBAL,
    LuaAttributeUse, LuaSemanticDeclId, LuaType,
    compilation::analyzer::doc::{
        DocAnalyzer,
        infer_type::infer_type,
        normalize_doc_string_const,
        tags::{get_owner_id, report_orphan_tag},
    },
};

pub fn analyze_tag_attribute_use(
    analyzer: &mut DocAnalyzer,
    tag_use: LuaDocTagAttributeUse,
) -> Option<()> {
    let owner = attribute_use_get_owner(analyzer, &tag_use);
    let owner_id = match get_owner_id(analyzer, owner.clone(), true) {
        Some(id) => id,
        None => {
            report_orphan_tag(analyzer, &tag_use);
            return None;
        }
    };

    if let Some(owner) = owner.clone() {
        match (owner, &owner_id) {
            (LuaAst::LuaDocTagParam(_), LuaSemanticDeclId::Signature(_))
            | (LuaAst::LuaDocTagReturn(_), LuaSemanticDeclId::Signature(_)) => {
                let attribute_uses = infer_attribute_uses(analyzer, tag_use)?;
                for attribute_use in attribute_uses {
                    if attribute_use.id.get_name() == GMOD_ATTR_VALID_GUARD
                        && let Some(name) = analyzer
                            .comment
                            .get_owner()
                            .as_ref()
                            .and_then(documented_definition_name)
                    {
                        analyzer
                            .db
                            .get_signature_index_mut()
                            .add_valid_guard_name(analyzer.file_id, name);
                    }
                    if matches!(
                        attribute_use.id.get_name(),
                        GMOD_ATTR_WRITES_GLOBAL
                            | GMOD_ATTR_VALID_GUARD
                            | GMOD_ATTR_EXHAUSTIVE_TYPE_GUARD
                    ) {
                        analyzer.db.get_property_index_mut().add_attribute_use(
                            analyzer.file_id,
                            owner_id.clone(),
                            attribute_use,
                        );
                    }
                }
                return Some(());
            }
            _ => {}
        }
    }

    let attribute_uses = infer_attribute_uses(analyzer, tag_use)?;
    for attribute_use in attribute_uses {
        if attribute_use.id.get_name() == GMOD_ATTR_VALID_GUARD
            && let Some(name) = owner.as_ref().and_then(documented_definition_name)
        {
            analyzer
                .db
                .get_signature_index_mut()
                .add_valid_guard_name(analyzer.file_id, name);
        }
        analyzer.db.get_property_index_mut().add_attribute_use(
            analyzer.file_id,
            owner_id.clone(),
            attribute_use,
        );
    }
    Some(())
}

/// The name of the definition a doc comment documents: the function, local,
/// assignment target or table field it is attached to.
fn documented_definition_name(owner: &LuaAst) -> Option<SmolStr> {
    let var_name = |var: LuaVarExpr| match var {
        LuaVarExpr::NameExpr(name) => name.get_name_text().map(SmolStr::new),
        LuaVarExpr::IndexExpr(index) => index_key_name(index.get_index_key()?),
    };
    match owner {
        LuaAst::LuaFuncStat(stat) => var_name(stat.get_func_name()?),
        LuaAst::LuaLocalFuncStat(stat) => Some(
            stat.get_local_name()?
                .get_name_token()?
                .get_name_text()
                .into(),
        ),
        LuaAst::LuaAssignStat(stat) => var_name(stat.child::<LuaVarExpr>()?),
        LuaAst::LuaLocalStat(stat) => Some(
            stat.child::<LuaLocalName>()?
                .get_name_token()?
                .get_name_text()
                .into(),
        ),
        LuaAst::LuaTableField(field) => index_key_name(field.get_field_key()?),
        _ => None,
    }
}

fn index_key_name(key: LuaIndexKey) -> Option<SmolStr> {
    match key {
        LuaIndexKey::Name(name) => Some(name.get_name_text().into()),
        LuaIndexKey::String(string) => Some(string.get_value().into()),
        _ => None,
    }
}

pub fn infer_attribute_uses(
    analyzer: &mut DocAnalyzer,
    tag_use: LuaDocTagAttributeUse,
) -> Option<Vec<LuaAttributeUse>> {
    let attribute_uses = tag_use.get_attribute_uses();
    let mut result = Vec::new();
    for attribute_use in attribute_uses {
        let attribute_type = infer_type(analyzer, LuaDocType::Name(attribute_use.get_type()?));
        if let LuaType::Ref(type_id) = attribute_type {
            let arg_types: Vec<LuaType> = attribute_use
                .get_arg_list()
                .map(|arg_list| arg_list.get_args().map(infer_attribute_arg_type).collect())
                .unwrap_or_default();
            let param_names = analyzer
                .db
                .get_type_index()
                .get_source_type_decl(&type_id)
                .and_then(|decl| decl.get_attribute_type())
                .and_then(|typ| match typ {
                    LuaType::DocAttribute(attr_type) => Some(
                        attr_type
                            .get_params()
                            .iter()
                            .map(|(name, _)| name.clone())
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                })
                .unwrap_or_default();

            let mut params = Vec::new();
            for (idx, arg_type) in arg_types.into_iter().enumerate() {
                let param_name = param_names
                    .get(idx)
                    .cloned()
                    .or_else(|| {
                        param_names.last().and_then(|last| {
                            if last == "..." {
                                Some(last.clone())
                            } else {
                                None
                            }
                        })
                    })
                    .unwrap_or_default();
                params.push((param_name, Some(arg_type)));
            }

            result.push(LuaAttributeUse::new(type_id, params));
        }
    }
    Some(result)
}

fn infer_attribute_arg_type(expr: LuaLiteralExpr) -> LuaType {
    if let Some(literal_token) = expr.get_literal() {
        match literal_token {
            LuaLiteralToken::String(str_token) => {
                let value = normalize_doc_string_const(str_token.get_value());
                return LuaType::DocStringConst(SmolStr::new(value).into());
            }
            LuaLiteralToken::Number(number_token) => {
                if let NumberResult::Int(i) = number_token.get_number_value() {
                    return LuaType::DocIntegerConst(i);
                } else {
                    return LuaType::Number;
                }
            }
            LuaLiteralToken::Bool(bool_token) => {
                return LuaType::DocBooleanConst(bool_token.is_true());
            }
            LuaLiteralToken::Nil(_) => return LuaType::Nil,
            LuaLiteralToken::Dots(_) => return LuaType::Any,
            LuaLiteralToken::Question(_) => return LuaType::Nil,
        }
    }
    LuaType::Unknown
}

/// Finds the owner of an attribute
fn attribute_use_get_owner(
    analyzer: &mut DocAnalyzer,
    attribute_use: &LuaDocTagAttributeUse,
) -> Option<LuaAst> {
    if let Some(attached_node) = attribute_find_doc(&attribute_use.syntax()) {
        return LuaAst::cast(attached_node);
    }
    analyzer.comment.get_owner()
}

fn attribute_find_doc(comment: &LuaSyntaxNode) -> Option<LuaSyntaxNode> {
    let mut next_sibling = comment.next_sibling_or_token();
    loop {
        next_sibling.as_ref()?;
        if let Some(sibling) = &next_sibling {
            match sibling.kind() {
                LuaKind::Syntax(
                    LuaSyntaxKind::DocTagField
                    | LuaSyntaxKind::DocTagOverload
                    | LuaSyntaxKind::DocTagParam
                    | LuaSyntaxKind::DocTagReturn,
                ) => {
                    if let Some(node) = sibling.as_node() {
                        return Some(node.clone());
                    }
                }
                LuaKind::Syntax(LuaSyntaxKind::Comment) => {
                    return None;
                }
                LuaKind::Syntax(LuaSyntaxKind::Block) => {
                    return None;
                }
                _ => {
                    if LuaExpr::can_cast(sibling.kind().into()) {
                        return None;
                    }
                }
            }
            next_sibling = sibling.next_sibling_or_token();
        }
    }
}

fn find_up_attribute(
    comment: &LuaSyntaxNode,
    result: &mut Vec<LuaDocTagAttributeUse>,
    stop_by_continue: bool,
) -> Option<()> {
    let mut next_sibling = comment.prev_sibling_or_token();
    loop {
        next_sibling.as_ref()?;
        if let Some(sibling) = &next_sibling {
            match sibling.kind() {
                LuaKind::Syntax(LuaSyntaxKind::DocTagAttributeUse) => {
                    if let Some(node) = sibling.as_node() {
                        if let Some(node) = LuaDocTagAttributeUse::cast(node.clone()) {
                            result.push(node);
                        }
                    }
                }
                // In some cases the search must stop at a --- separator
                LuaKind::Token(LuaTokenKind::TkDocContinue) => {
                    if stop_by_continue {
                        return None;
                    }
                }
                LuaKind::Syntax(LuaSyntaxKind::DocDescription) => {}
                LuaKind::Syntax(_) => {
                    return None;
                }
                _ => {}
            }
            next_sibling = sibling.prev_sibling_or_token();
        }
    }
}

pub fn find_attach_attribute(ast: LuaAst) -> Option<Vec<LuaDocTagAttributeUse>> {
    match ast {
        LuaAst::LuaDocTagParam(param) => {
            let mut result = Vec::new();
            find_up_attribute(param.syntax(), &mut result, true);
            Some(result)
        }
        LuaAst::LuaDocTagOverload(overload) => {
            let mut result = Vec::new();
            find_up_attribute(overload.syntax(), &mut result, true);
            Some(result)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use glua_parser::{LuaAstNode, LuaClosureExpr};

    use crate::{GmodRealm, LuaSignatureId, VirtualWorkspace, signature_is_valid_guard_in_realm};

    #[test]
    fn valid_guard_metadata_survives_parameter_and_return_tag_attachment() {
        for docs in [
            "---@[valid_guard]\n---@param value any\n---@return boolean",
            "---@param value any\n---@[valid_guard]\n---@return boolean",
            "---@param value any\n---@return boolean\n---@[valid_guard]",
        ] {
            let mut ws = VirtualWorkspace::new();
            let file = ws.def_file(
                "annotations/valid_guard_placement.lua",
                &format!("---@attribute valid_guard()\n\n{docs}\nfunction present(value) end"),
            );
            let db = ws.analysis.compilation.get_db();
            let root = db
                .get_vfs()
                .get_syntax_tree(&file)
                .unwrap()
                .get_chunk_node();
            let closure = root.descendants::<LuaClosureExpr>().next().unwrap();
            let signature = LuaSignatureId::from_closure(file, &closure);
            assert!(
                signature_is_valid_guard_in_realm(db, signature, GmodRealm::Shared),
                "{docs}"
            );
        }
    }
}
