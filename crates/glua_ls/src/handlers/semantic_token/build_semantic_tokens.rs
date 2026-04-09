use super::{
    SEMANTIC_TOKEN_MODIFIERS, SEMANTIC_TOKEN_TYPES, semantic_token_builder::SemanticBuilder,
};
use crate::handlers::semantic_token::function_string_highlight::fun_string_highlight;
use crate::handlers::semantic_token::semantic_token_builder::{
    CustomSemanticTokenModifier, CustomSemanticTokenType,
};
use crate::util::parse_desc;
use crate::{context::ClientId, handlers::semantic_token::language_injector::inject_language};
use glua_code_analysis::{
    Emmyrc, LocalAttribute, LuaDecl, LuaDeclExtra, LuaMemberId, LuaMemberOwner, LuaSemanticDeclId,
    LuaType, LuaTypeDeclId, SemanticDeclLevel, SemanticModel, WorkspaceId,
    parse_require_module_info,
};
use glua_parser::{
    LuaAssignStat, LuaAst, LuaAstNode, LuaAstToken, LuaCallArgList, LuaCallExpr, LuaComment,
    LuaDocFieldKey, LuaDocGenericDecl, LuaDocGenericDeclList, LuaDocObjectFieldKey, LuaDocTagOther,
    LuaDocType, LuaExpr, LuaGeneralToken, LuaKind, LuaLiteralToken, LuaNameToken, LuaSyntaxKind,
    LuaSyntaxNode, LuaSyntaxToken, LuaTokenKind, LuaVarExpr, PathTrait,
};
use glua_parser_desc::{CodeBlockHighlightKind, DescItem, DescItemKind};
use lsp_types::{SemanticToken, SemanticTokenModifier, SemanticTokenType};
use rowan::{NodeOrToken, TextRange, TextSize};
use tokio_util::sync::CancellationToken;

pub fn build_semantic_tokens(
    semantic_model: &SemanticModel,
    support_muliline_token: bool,
    client_id: ClientId,
    emmyrc: &Emmyrc,
    cancel_token: &CancellationToken,
) -> Option<Vec<SemanticToken>> {
    let root = semantic_model.get_root();
    let document = semantic_model.get_document();
    let mut builder = SemanticBuilder::new(
        &document,
        support_muliline_token,
        SEMANTIC_TOKEN_TYPES.to_vec(),
        SEMANTIC_TOKEN_MODIFIERS.to_vec(),
    );

    for node_or_token in root.syntax().descendants_with_tokens() {
        if cancel_token.is_cancelled() {
            return None;
        }
        match node_or_token {
            NodeOrToken::Node(node) => {
                build_node_semantic_token(semantic_model, &mut builder, node, emmyrc);
            }
            NodeOrToken::Token(token) => {
                build_tokens_semantic_token(
                    semantic_model,
                    &mut builder,
                    &token,
                    client_id,
                    emmyrc,
                );
            }
        }
    }

    Some(builder.build())
}

fn build_tokens_semantic_token(
    _semantic_model: &SemanticModel,
    builder: &mut SemanticBuilder,
    token: &LuaSyntaxToken,
    client_id: ClientId,
    emmyrc: &Emmyrc,
) {
    match token.kind().into() {
        LuaTokenKind::TkLongString | LuaTokenKind::TkString => {
            if !builder.is_special_string_range(&token.text_range()) {
                builder.push(token, SemanticTokenType::STRING);
            }
        }
        LuaTokenKind::TkAnd
        | LuaTokenKind::TkBreak
        | LuaTokenKind::TkDo
        | LuaTokenKind::TkElse
        | LuaTokenKind::TkElseIf
        | LuaTokenKind::TkEnd
        | LuaTokenKind::TkFor
        | LuaTokenKind::TkFunction
        | LuaTokenKind::TkGoto
        | LuaTokenKind::TkIf
        | LuaTokenKind::TkIn
        | LuaTokenKind::TkNot
        | LuaTokenKind::TkOr
        | LuaTokenKind::TkRepeat
        | LuaTokenKind::TkReturn
        | LuaTokenKind::TkThen
        | LuaTokenKind::TkUntil
        | LuaTokenKind::TkWhile
        // GMod: dead path — Lua 5.5 `global` keyword disabled
        | LuaTokenKind::TkGlobal => {
            builder.push(token, SemanticTokenType::KEYWORD);
        }
        LuaTokenKind::TkLocal => {
            if !client_id.is_vscode() {
                builder.push(token, SemanticTokenType::KEYWORD);
            }
        }
        LuaTokenKind::TkPlus
        | LuaTokenKind::TkMinus
        | LuaTokenKind::TkMul
        | LuaTokenKind::TkDiv
        // GMod: dead paths below — Lua 5.3+ operators disabled
        | LuaTokenKind::TkIDiv
        | LuaTokenKind::TkShl
        | LuaTokenKind::TkShr
        | LuaTokenKind::TkBitAnd
        | LuaTokenKind::TkBitOr
        | LuaTokenKind::TkBitXor
        // end dead paths
        | LuaTokenKind::TkDot
        | LuaTokenKind::TkConcat
        | LuaTokenKind::TkEq
        | LuaTokenKind::TkGe
        | LuaTokenKind::TkLe
        | LuaTokenKind::TkNe
        | LuaTokenKind::TkLt
        | LuaTokenKind::TkGt
        | LuaTokenKind::TkMod
        | LuaTokenKind::TkPow
        | LuaTokenKind::TkLen
        | LuaTokenKind::TkAssign => {
            builder.push(token, SemanticTokenType::OPERATOR);
        }
        LuaTokenKind::TkLeftBrace | LuaTokenKind::TkRightBrace => {
            if let Some(parent) = token.parent()
                && !matches!(
                    parent.kind().into(),
                    LuaSyntaxKind::TableArrayExpr
                        | LuaSyntaxKind::TableEmptyExpr
                        | LuaSyntaxKind::TableObjectExpr
                )
            {
                builder.push(token, SemanticTokenType::OPERATOR);
            }
        }
        LuaTokenKind::TkColon => {
            if let Some(parent) = token.parent()
                && parent.kind() != LuaSyntaxKind::IndexExpr.into()
            {
                builder.push(token, SemanticTokenType::OPERATOR);
            }
        }
        // delimiter
        LuaTokenKind::TkLeftBracket | LuaTokenKind::TkRightBracket => {
            if let Some(parent) = token.parent()
                && matches!(
                    parent.kind().into(),
                    LuaSyntaxKind::TableFieldAssign | LuaSyntaxKind::IndexExpr
                )
            {
                builder.push(token, CustomSemanticTokenType::DELIMITER);
            } else {
                builder.push(token, SemanticTokenType::OPERATOR);
            }
        }
        LuaTokenKind::TkLeftParen | LuaTokenKind::TkRightParen => {
            if let Some(parent) = token.parent()
                && matches!(
                    parent.kind().into(),
                    LuaSyntaxKind::ParamList
                        | LuaSyntaxKind::CallArgList
                        | LuaSyntaxKind::ParenExpr
                )
            {
                builder.push(token, CustomSemanticTokenType::DELIMITER);
            } else {
                builder.push(token, SemanticTokenType::OPERATOR);
            }
        }
        LuaTokenKind::TkTrue | LuaTokenKind::TkFalse | LuaTokenKind::TkNil => {
            builder.push_with_modifier(
                token,
                SemanticTokenType::KEYWORD,
                SemanticTokenModifier::READONLY,
            );
        }
        LuaTokenKind::TkComplex | LuaTokenKind::TkInt | LuaTokenKind::TkFloat => {
            builder.push(token, SemanticTokenType::NUMBER);
        }
        LuaTokenKind::TkTagClass
        | LuaTokenKind::TkTagEnum
        | LuaTokenKind::TkTagInterface
        | LuaTokenKind::TkTagAlias
        | LuaTokenKind::TkTagModule
        | LuaTokenKind::TkTagField
        | LuaTokenKind::TkTagType
        | LuaTokenKind::TkTagParam
        | LuaTokenKind::TkTagReturn
        | LuaTokenKind::TkTagOverload
        | LuaTokenKind::TkTagGeneric
        | LuaTokenKind::TkTagSee
        | LuaTokenKind::TkTagDeprecated
        | LuaTokenKind::TkTagAsync
        | LuaTokenKind::TkTagCast
        | LuaTokenKind::TkTagOther
        | LuaTokenKind::TkTagReadonly
        | LuaTokenKind::TkTagDiagnostic
        | LuaTokenKind::TkTagMeta
        | LuaTokenKind::TkTagVersion
        | LuaTokenKind::TkTagAs
        | LuaTokenKind::TkTagNodiscard
        | LuaTokenKind::TkTagOperator
        | LuaTokenKind::TkTagMapping
        | LuaTokenKind::TkTagNamespace
        | LuaTokenKind::TkTagUsing
        | LuaTokenKind::TkTagSource
        | LuaTokenKind::TkTagRealm
        | LuaTokenKind::TkTagReturnCast
        | LuaTokenKind::TkTagExport
        | LuaTokenKind::TkLanguage
        | LuaTokenKind::TkTagAttribute
        | LuaTokenKind::TKTagSchema => {
            builder.push_with_modifier(
                token,
                SemanticTokenType::KEYWORD,
                SemanticTokenModifier::DOCUMENTATION,
            );
        }
        LuaTokenKind::TkDocKeyOf
        | LuaTokenKind::TkDocExtends
        | LuaTokenKind::TkDocNew
        | LuaTokenKind::TkDocAs
        | LuaTokenKind::TkDocIn
        | LuaTokenKind::TkDocInfer
        | LuaTokenKind::TkDocReadonly => {
            builder.push_with_modifier(
                token,
                SemanticTokenType::KEYWORD,
                SemanticTokenModifier::DOCUMENTATION,
            );
        }
        LuaTokenKind::TkNormalStart | LuaTokenKind::TKNonStdComment => {
            builder.push(token, SemanticTokenType::COMMENT);
        }
        LuaTokenKind::TkDocDetail => {
            // We're rendering a description. If description parsing is enabled,
            // this token will be handled by the corresponding description parser.
            let rendering_description = token
                .parent()
                .is_some_and(|parent| parent.kind() == LuaSyntaxKind::DocDescription.into());
            let description_parsing_is_enabled = emmyrc.semantic_tokens.render_documentation_markup;

            if !(rendering_description && description_parsing_is_enabled) {
                builder.push(token, SemanticTokenType::COMMENT);
            }
        }
        LuaTokenKind::TkDocQuestion | LuaTokenKind::TkDocOr | LuaTokenKind::TkDocAnd => {
            builder.push_with_modifier(
                token,
                SemanticTokenType::OPERATOR,
                SemanticTokenModifier::DOCUMENTATION,
            );
        }
        LuaTokenKind::TkDocVisibility | LuaTokenKind::TkTagVisibility => {
            builder.push_with_modifiers(
                token,
                SemanticTokenType::KEYWORD,
                &[
                    SemanticTokenModifier::MODIFICATION,
                    SemanticTokenModifier::DOCUMENTATION,
                ],
            );
        }
        LuaTokenKind::TkDocVersionNumber => {
            builder.push_with_modifier(
                token,
                SemanticTokenType::NUMBER,
                SemanticTokenModifier::DOCUMENTATION,
            );
        }
        LuaTokenKind::TkStringTemplateType => {
            builder.push_with_modifier(
                token,
                SemanticTokenType::STRING,
                SemanticTokenModifier::DOCUMENTATION,
            );
        }
        LuaTokenKind::TkDocMatch => {
            builder.push_with_modifier(
                token,
                SemanticTokenType::KEYWORD,
                SemanticTokenModifier::DOCUMENTATION,
            );
        }
        LuaTokenKind::TKDocPath | LuaTokenKind::TkDocSeeContent => {
            builder.push_with_modifier(
                token,
                SemanticTokenType::STRING,
                SemanticTokenModifier::DOCUMENTATION,
            );
        }
        LuaTokenKind::TkDocRegion | LuaTokenKind::TkDocEndRegion => {
            builder.push(token, SemanticTokenType::COMMENT);
        }
        LuaTokenKind::TkDocStart | LuaTokenKind::TkDocContinue | LuaTokenKind::TkDocContinueOr => {
            render_doc_at(builder, token)
        }
        _ => {}
    }
}

fn get_global_rooted_index_depth(
    semantic_model: &glua_code_analysis::SemanticModel,
    index_expr: &glua_parser::LuaIndexExpr,
) -> Option<usize> {
    let mut current = index_expr.clone();
    let mut depth = 1;
    while let Some(prefix) = current.get_prefix_expr() {
        if let glua_parser::LuaExpr::NameExpr(name_expr) = prefix {
            if let Some(glua_code_analysis::LuaSemanticDeclId::LuaDecl(id)) = semantic_model
                .find_decl(
                    name_expr.syntax().clone().into(),
                    glua_code_analysis::SemanticDeclLevel::NoTrace,
                )
            {
                if semantic_model
                    .get_db()
                    .get_decl_index()
                    .get_decl(&id)
                    .is_some_and(|d| d.is_global())
                {
                    return Some(depth);
                }
            }
            return None;
        } else if let glua_parser::LuaExpr::IndexExpr(expr) = prefix {
            current = expr;
            depth += 1;
        } else {
            return None;
        }
    }
    None
}

fn build_node_semantic_token(
    semantic_model: &SemanticModel,
    builder: &mut SemanticBuilder,
    node: LuaSyntaxNode,
    emmyrc: &Emmyrc,
) -> Option<()> {
    match LuaAst::cast(node)? {
        LuaAst::LuaDocTagClass(doc_class) => {
            if let Some(name) = doc_class.get_name_token() {
                builder.push_with_modifier(
                    name.syntax(),
                    SemanticTokenType::CLASS,
                    SemanticTokenModifier::DECLARATION,
                );
            }
            if let Some(attribs) = doc_class.get_type_flag() {
                for token in attribs.tokens::<LuaGeneralToken>() {
                    builder.push(token.syntax(), SemanticTokenType::DECORATOR);
                }
            }
            if let Some(generic_list) = doc_class.get_generic_decl() {
                render_type_parameter_list(builder, &generic_list);
            }
        }
        LuaAst::LuaDocTagEnum(doc_enum) => {
            let name = doc_enum.get_name_token()?;
            builder.push_with_modifier(
                name.syntax(),
                SemanticTokenType::ENUM,
                SemanticTokenModifier::DECLARATION,
            );
            if let Some(attribs) = doc_enum.get_type_flag() {
                for token in attribs.tokens::<LuaGeneralToken>() {
                    builder.push(token.syntax(), SemanticTokenType::DECORATOR);
                }
            }
        }
        LuaAst::LuaDocTagAlias(doc_alias) => {
            let name = doc_alias.get_name_token()?;
            builder.push_with_modifier(
                name.syntax(),
                SemanticTokenType::TYPE,
                SemanticTokenModifier::DECLARATION,
            );
            if let Some(generic_decl_list) = doc_alias.get_generic_decl_list() {
                render_type_parameter_list(builder, &generic_decl_list);
            }
        }
        LuaAst::LuaDocTagField(doc_field) => {
            if let Some(LuaDocFieldKey::Name(name)) = doc_field.get_field_key() {
                builder.push_with_modifier(
                    name.syntax(),
                    SemanticTokenType::PROPERTY,
                    SemanticTokenModifier::DECLARATION,
                );
            }
        }
        LuaAst::LuaDocTagDiagnostic(doc_diagnostic) => {
            let name = doc_diagnostic.get_action_token()?;
            builder.push(name.syntax(), SemanticTokenType::PROPERTY);
            if let Some(code_list) = doc_diagnostic.get_code_list() {
                for code in code_list.get_codes() {
                    builder.push(code.syntax(), SemanticTokenType::REGEXP);
                }
            }
        }
        LuaAst::LuaDocTagParam(doc_param) => {
            let name = doc_param.get_name_token()?;
            builder.push_with_modifier(
                name.syntax(),
                SemanticTokenType::PARAMETER,
                SemanticTokenModifier::DECLARATION,
            );
        }
        LuaAst::LuaDocTagRealm(doc_realm) => {
            if let Some(realm) = doc_realm.get_name_token() {
                builder.push_with_modifier(
                    realm.syntax(),
                    SemanticTokenType::ENUM_MEMBER,
                    SemanticTokenModifier::DECLARATION,
                );
            }
        }
        LuaAst::LuaDocTagFileparam(doc_fileparam) => {
            if let Some(name) = doc_fileparam.get_name_token() {
                builder.push_with_modifier(
                    name.syntax(),
                    SemanticTokenType::PARAMETER,
                    SemanticTokenModifier::DECLARATION,
                );
            }
        }
        LuaAst::LuaDocTagReturn(doc_return) => {
            let type_name_list = doc_return.get_info_list();
            for (_, name) in type_name_list {
                if let Some(name) = name {
                    builder.push(name.syntax(), SemanticTokenType::VARIABLE);
                }
            }
        }
        LuaAst::LuaDocTagCast(doc_cast) => {
            if let Some(target_expr) = doc_cast.get_key_expr() {
                match target_expr {
                    LuaExpr::NameExpr(name_expr) => {
                        builder.push(
                            name_expr.get_name_token()?.syntax(),
                            SemanticTokenType::VARIABLE,
                        );
                    }
                    LuaExpr::IndexExpr(index_expr) => {
                        let position = index_expr.syntax().text_range().start();
                        let len = index_expr.syntax().text_range().len();
                        builder.push_at_position(
                            position,
                            len.into(),
                            SemanticTokenType::VARIABLE,
                            None,
                        );
                    }
                    _ => {}
                }
            }
            if let Some(NodeOrToken::Token(token)) = doc_cast.syntax().prev_sibling_or_token()
                && token.kind() == LuaKind::Token(LuaTokenKind::TkDocLongStart)
            {
                render_doc_at(builder, &token);
            }
        }
        LuaAst::LuaDocTagAs(doc_as) => {
            if let Some(NodeOrToken::Token(token)) = doc_as.syntax().prev_sibling_or_token()
                && token.kind() == LuaKind::Token(LuaTokenKind::TkDocLongStart)
            {
                render_doc_at(builder, &token);
            }
        }
        LuaAst::LuaDocTagGeneric(doc_generic) => {
            let type_parameter_list = doc_generic.get_generic_decl_list()?;
            render_type_parameter_list(builder, &type_parameter_list);
        }
        LuaAst::LuaDocTagNamespace(doc_namespace) => {
            let name = doc_namespace.get_name_token()?;
            builder.push_with_modifier(
                name.syntax(),
                SemanticTokenType::NAMESPACE,
                SemanticTokenModifier::DECLARATION,
            );
        }
        LuaAst::LuaDocTagUsing(doc_using) => {
            let name = doc_using.get_name_token()?;
            builder.push(name.syntax(), SemanticTokenType::NAMESPACE);
        }
        LuaAst::LuaDocTagExport(doc_export) => {
            let name = doc_export.get_name_token()?;
            builder.push_with_modifier(
                name.syntax(),
                SemanticTokenType::NAMESPACE,
                SemanticTokenModifier::MODIFICATION,
            );
        }
        LuaAst::LuaParamName(param_name) => {
            let name_token = param_name.get_name_token()?;
            if builder.contains_token(name_token.syntax()) {
                return Some(());
            }
            handle_name_node(semantic_model, builder, param_name.syntax(), &name_token);
        }
        LuaAst::LuaLocalName(local_name) => {
            let name_token = local_name.get_name_token()?;
            if builder.contains_token(name_token.syntax()) {
                return Some(());
            }
            handle_name_node(semantic_model, builder, local_name.syntax(), &name_token);
        }
        LuaAst::LuaNameExpr(name_expr) => {
            let name_token = name_expr.get_name_token()?;
            if builder.contains_token(name_token.syntax()) {
                return Some(());
            }
            handle_name_node(semantic_model, builder, name_expr.syntax(), &name_token)
                .unwrap_or_else(|| {
                    // 改进：为未知名称提供更好的默认分类
                    let name_text = name_token.get_name_text();
                    builder.push(
                        name_token.syntax(),
                        default_identifier_token_type(name_text),
                    );
                });
        }
        LuaAst::LuaForRangeStat(for_range_stat) => {
            for name in for_range_stat.get_var_name_list() {
                builder.push_with_modifiers(
                    name.syntax(),
                    SemanticTokenType::VARIABLE,
                    &[
                        SemanticTokenModifier::DECLARATION,
                        SemanticTokenModifier::READONLY,
                        CustomSemanticTokenModifier::LOCAL,
                    ],
                );
            }
        }
        LuaAst::LuaForStat(for_stat) => {
            let name = for_stat.get_var_name()?;
            builder.push_with_modifiers(
                name.syntax(),
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DECLARATION,
                    SemanticTokenModifier::READONLY,
                    CustomSemanticTokenModifier::LOCAL,
                ],
            );
        }
        LuaAst::LuaLocalFuncStat(local_func_stat) => {
            let name = local_func_stat.get_local_name()?.get_name_token()?;
            let mut modifiers = vec![SemanticTokenModifier::DECLARATION];
            if let Some(semantic_decl) = semantic_model.find_decl(
                local_func_stat.get_local_name()?.syntax().clone().into(),
                glua_code_analysis::SemanticDeclLevel::NoTrace,
            ) {
                let decl_type = match semantic_decl {
                    glua_code_analysis::LuaSemanticDeclId::Member(member_id) => {
                        semantic_model.get_type(member_id.into())
                    }
                    glua_code_analysis::LuaSemanticDeclId::LuaDecl(decl_id) => {
                        semantic_model.get_type(decl_id.into())
                    }
                    _ => glua_code_analysis::LuaType::Unknown,
                };
                enrich_modifiers_from_decl(
                    semantic_model,
                    &semantic_decl,
                    &decl_type,
                    &mut modifiers,
                );
            }
            builder.push_with_modifiers(name.syntax(), SemanticTokenType::FUNCTION, &modifiers);
        }
        LuaAst::LuaFuncStat(func_stat) => {
            let func_name = func_stat.get_func_name()?;
            match func_name {
                LuaVarExpr::NameExpr(name_expr) => {
                    let name = name_expr.get_name_token()?;
                    let mut modifiers = vec![SemanticTokenModifier::DECLARATION];
                    if let Some(semantic_decl) = semantic_model.find_decl(
                        name_expr.syntax().clone().into(),
                        glua_code_analysis::SemanticDeclLevel::NoTrace,
                    ) {
                        let decl_type = match semantic_decl {
                            glua_code_analysis::LuaSemanticDeclId::Member(member_id) => {
                                semantic_model.get_type(member_id.into())
                            }
                            glua_code_analysis::LuaSemanticDeclId::LuaDecl(decl_id) => {
                                semantic_model.get_type(decl_id.into())
                            }
                            _ => glua_code_analysis::LuaType::Unknown,
                        };
                        enrich_modifiers_from_decl(
                            semantic_model,
                            &semantic_decl,
                            &decl_type,
                            &mut modifiers,
                        );
                    }
                    builder.push_with_modifiers(
                        name.syntax(),
                        SemanticTokenType::FUNCTION,
                        &modifiers,
                    );
                }
                LuaVarExpr::IndexExpr(index_expr) => {
                    let name = index_expr.get_index_name_token()?;
                    let mut modifiers = vec![SemanticTokenModifier::DECLARATION];
                    if let Some(semantic_decl) = semantic_model.find_decl(
                        index_expr.syntax().clone().into(),
                        glua_code_analysis::SemanticDeclLevel::NoTrace,
                    ) {
                        let decl_type = match semantic_decl {
                            glua_code_analysis::LuaSemanticDeclId::Member(member_id) => {
                                semantic_model.get_type(member_id.into())
                            }
                            glua_code_analysis::LuaSemanticDeclId::LuaDecl(decl_id) => {
                                semantic_model.get_type(decl_id.into())
                            }
                            _ => glua_code_analysis::LuaType::Unknown,
                        };
                        enrich_modifiers_from_decl(
                            semantic_model,
                            &semantic_decl,
                            &decl_type,
                            &mut modifiers,
                        );
                    }
                    builder.push_with_modifiers(&name, SemanticTokenType::METHOD, &modifiers);
                }
            }
        }
        // GMod: dead path — Lua 5.4 <const>/<close> attributes disabled
        LuaAst::LuaLocalAttribute(local_attribute) => {
            let name = local_attribute.get_name_token()?;
            builder.push(name.syntax(), SemanticTokenType::KEYWORD);
        }
        LuaAst::LuaCallExpr(call_expr) => {
            let prefix = call_expr.get_prefix_expr()?;
            match prefix {
                LuaExpr::NameExpr(ref name_expr) => {
                    let name = name_expr.get_name_token()?;
                    if builder.contains_token(name.syntax()) {
                        return Some(());
                    }
                    let name_text = name.get_name_text();
                    let prefix_type = semantic_model.infer_expr(prefix).ok();
                    render_callable_name_token(
                        semantic_model,
                        builder,
                        name.syntax(),
                        name_text,
                        prefix_type,
                    )?;
                }
                LuaExpr::IndexExpr(ref index_expr) => {
                    let name = index_expr.get_name_token()?;
                    if builder.contains_token(name.syntax()) {
                        return Some(());
                    }
                    builder.push_with_modifier(
                        name.syntax(),
                        SemanticTokenType::METHOD,
                        CustomSemanticTokenModifier::CALLABLE,
                    );
                }
                _ => {}
            }
        }
        LuaAst::LuaDocNameType(doc_name_type) => {
            let name = doc_name_type.get_name_token()?;
            let name_text = name.get_name_text();
            if name_text == "self"
                || name_text == "nil"
                || name_text == "boolean"
                || name_text == "number"
                || name_text == "string"
                || name_text == "table"
                || name_text == "function"
                || name_text == "userdata"
                || name_text == "thread"
            {
                // Lua内置类型
                builder.push_with_modifier(
                    name.syntax(),
                    SemanticTokenType::TYPE,
                    SemanticTokenModifier::DEFAULT_LIBRARY,
                );
            } else {
                builder.push(name.syntax(), SemanticTokenType::TYPE);
            }
        }
        LuaAst::LuaDocObjectType(doc_object_type) => {
            let fields = doc_object_type.get_fields();
            for field in fields {
                if let Some(field_key) = field.get_field_key()
                    && let LuaDocObjectFieldKey::Name(name) = &field_key
                {
                    builder.push(name.syntax(), CustomSemanticTokenType::FIELD);
                }
            }
        }
        LuaAst::LuaDocFuncType(doc_func_type) => {
            for name_token in doc_func_type.tokens::<LuaNameToken>() {
                match name_token.get_name_text() {
                    "fun" => {
                        builder.push(name_token.syntax(), SemanticTokenType::KEYWORD);
                    }
                    "async" => {
                        builder.push_with_modifier(
                            name_token.syntax(),
                            SemanticTokenType::KEYWORD,
                            SemanticTokenModifier::ASYNC,
                        );
                    }
                    _ => {}
                }
            }

            for param in doc_func_type.get_params() {
                let name = param.get_name_token()?;
                builder.push(name.syntax(), SemanticTokenType::PARAMETER);
            }
        }
        LuaAst::LuaIndexExpr(index_expr) => {
            // 处理模块前缀
            if let Some(LuaExpr::NameExpr(prefix_name_expr)) = index_expr.get_prefix_expr()
                && let Some(prefix_name) = prefix_name_expr.get_name_token()
                && !builder.contains_token(prefix_name.syntax())
                && let Some(LuaSemanticDeclId::LuaDecl(prefix_decl_id)) = semantic_model.find_decl(
                    prefix_name_expr.syntax().clone().into(),
                    SemanticDeclLevel::NoTrace,
                )
                && let Some(prefix_decl) = semantic_model
                    .get_db()
                    .get_decl_index()
                    .get_decl(&prefix_decl_id)
                && is_require_decl(semantic_model, prefix_decl)
            {
                builder.push(prefix_name.syntax(), SemanticTokenType::NAMESPACE);
            }

            let name = index_expr.get_name_token()?;
            if builder.contains_token(name.syntax()) {
                return Some(());
            }
            let semantic_decl = semantic_model
                .find_decl(name.syntax().clone().into(), SemanticDeclLevel::default());
            if let Some(property_owner) = semantic_decl
                && let LuaSemanticDeclId::Member(member_id) = property_owner
            {
                let decl_type = semantic_model.get_type(member_id.into());
                if decl_type.is_function() {
                    let mut modifiers = vec![];
                    enrich_modifiers_from_decl(
                        semantic_model,
                        &property_owner,
                        &decl_type,
                        &mut modifiers,
                    );
                    push_name_or_syntax_with_context_modifiers(
                        builder,
                        name.syntax(),
                        index_expr.syntax(),
                        SemanticTokenType::METHOD,
                        &modifiers,
                    );
                    return Some(());
                }
                if decl_type.is_def() {
                    builder.push_with_modifier(
                        name.syntax(),
                        SemanticTokenType::CLASS,
                        SemanticTokenModifier::READONLY,
                    );
                    return Some(());
                }

                let global_depth = get_global_rooted_index_depth(semantic_model, &index_expr);

                let owner_id = semantic_model
                    .get_db()
                    .get_member_index()
                    .get_current_owner(&member_id);
                if let Some(glua_code_analysis::LuaMemberOwner::Type(type_id)) = owner_id
                    && let Some(type_decl) = semantic_model
                        .get_db()
                        .get_type_index()
                        .get_type_decl(type_id)
                    && type_decl.is_enum()
                {
                    builder.push_with_modifier(
                        name.syntax(),
                        SemanticTokenType::ENUM_MEMBER,
                        SemanticTokenModifier::READONLY,
                    );
                    return Some(());
                }

                let mut is_class_like = is_table_like_type(&decl_type);

                if !is_class_like && global_depth == Some(1) {
                    if let Some(member) = semantic_model
                        .get_db()
                        .get_member_index()
                        .get_member(&member_id)
                    {
                        if let Some(global_id) = member.get_global_id() {
                            use glua_code_analysis::{
                                LuaMemberFeature, LuaMemberOwner, LuaTypeOwner,
                            };
                            let owner = LuaMemberOwner::GlobalPath(global_id.clone());
                            if let Some(child_members) = semantic_model
                                .get_db()
                                .get_member_index()
                                .get_members(&owner)
                            {
                                let mut callable_count = 0;
                                for child in child_members {
                                    let feature = child.get_feature();

                                    let is_callable = if feature == LuaMemberFeature::FileMethodDecl
                                        || feature == LuaMemberFeature::MetaMethodDecl
                                        || feature == LuaMemberFeature::MetaDefine
                                    {
                                        true
                                    } else if let Some(type_cache) = semantic_model
                                        .get_db()
                                        .get_type_index()
                                        .get_type_cache(&LuaTypeOwner::Member(child.get_id()))
                                    {
                                        type_cache.is_function()
                                    } else {
                                        false
                                    };

                                    if is_callable {
                                        callable_count += 1;
                                        if callable_count >= 2 {
                                            is_class_like = true;
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                if is_class_like {
                    // Only highlight as CLASS if it is the first segment (depth == 1) of a global path.
                    // This prevents local table fields or deeper segments from becoming CLASS.
                    if global_depth == Some(1) {
                        push_name_or_syntax_with_context_modifiers(
                            builder,
                            name.syntax(),
                            index_expr.syntax(),
                            SemanticTokenType::CLASS,
                            &[],
                        );
                        return Some(());
                    }
                }

                if is_function_like_type(&decl_type) {
                    push_name_or_syntax_with_context_modifiers(
                        builder,
                        name.syntax(),
                        index_expr.syntax(),
                        SemanticTokenType::PROPERTY,
                        &[CustomSemanticTokenModifier::CALLABLE],
                    );
                } else {
                    push_name_or_syntax_with_context_modifiers(
                        builder,
                        name.syntax(),
                        index_expr.syntax(),
                        CustomSemanticTokenType::FIELD,
                        &[],
                    );
                }
                return Some(());
            }

            // 默认情况：检查是否在调用上下文中
            if index_expr
                .syntax()
                .parent()
                .is_some_and(|p| p.kind() == LuaSyntaxKind::CallExpr.into())
            {
                push_name_or_syntax_with_context_modifiers(
                    builder,
                    name.syntax(),
                    index_expr.syntax(),
                    SemanticTokenType::METHOD,
                    &[CustomSemanticTokenModifier::CALLABLE],
                );
            } else {
                push_name_or_syntax_with_context_modifiers(
                    builder,
                    name.syntax(),
                    index_expr.syntax(),
                    CustomSemanticTokenType::FIELD,
                    &[],
                );
            }
        }
        LuaAst::LuaTableField(table_field) => {
            let owner_id =
                LuaMemberId::new(table_field.get_syntax_id(), semantic_model.get_file_id());
            if let Some(member) = semantic_model
                .get_db()
                .get_member_index()
                .get_member(&owner_id)
            {
                let owner_id = semantic_model
                    .get_db()
                    .get_member_index()
                    .get_current_owner(&member.get_id());
                if let Some(LuaMemberOwner::Type(type_id)) = owner_id
                    && let Some(type_decl) = semantic_model
                        .get_db()
                        .get_type_index()
                        .get_type_decl(type_id)
                    && type_decl.is_enum()
                {
                    if let Some(field_name) = table_field.get_field_key()?.get_name() {
                        builder.push_with_modifier(
                            field_name.syntax(),
                            SemanticTokenType::ENUM_MEMBER,
                            SemanticTokenModifier::DECLARATION,
                        );
                    }
                    return Some(());
                }
            }

            let value_type = semantic_model
                .infer_expr(table_field.get_value_expr()?.clone())
                .ok()?;
            match value_type {
                LuaType::Signature(_) | LuaType::DocFunction(_) => {
                    if let Some(field_name) = table_field.get_field_key()?.get_name() {
                        let mut modifiers = vec![SemanticTokenModifier::DECLARATION];
                        if let Some(member) = semantic_model
                            .get_db()
                            .get_member_index()
                            .get_member(&owner_id)
                        {
                            enrich_modifiers_from_decl(
                                semantic_model,
                                &glua_code_analysis::LuaSemanticDeclId::Member(member.get_id()),
                                &value_type,
                                &mut modifiers,
                            );
                        }
                        builder.push_with_modifiers(
                            field_name.syntax(),
                            SemanticTokenType::METHOD,
                            &modifiers,
                        );
                    }
                }
                LuaType::Union(union) if union.into_vec().iter().any(|typ| typ.is_function()) => {
                    if let Some(field_name) = table_field.get_field_key()?.get_name() {
                        builder.push_with_modifiers(
                            field_name.syntax(),
                            CustomSemanticTokenType::FIELD,
                            &[
                                SemanticTokenModifier::DECLARATION,
                                CustomSemanticTokenModifier::CALLABLE,
                            ],
                        );
                    }
                }
                _ => {
                    if let Some(field_name) = table_field.get_field_key()?.get_name() {
                        builder.push_with_modifier(
                            field_name.syntax(),
                            CustomSemanticTokenType::FIELD,
                            SemanticTokenModifier::DECLARATION,
                        );
                    }
                }
            }
        }
        LuaAst::LuaDocLiteralType(literal) => {
            if let LuaLiteralToken::Bool(bool_token) = &literal.get_literal()? {
                builder.push_with_modifier(
                    bool_token.syntax(),
                    SemanticTokenType::KEYWORD,
                    SemanticTokenModifier::DOCUMENTATION,
                );
            }
        }
        LuaAst::LuaDocDescription(description) => {
            if let Some(parent) = description.syntax().parent() {
                if let Some(doc_other) = LuaDocTagOther::cast(parent.clone()) {
                    if doc_other.get_tag_name().as_deref() == Some("hook") {
                        for token in description.tokens::<LuaGeneralToken>() {
                            builder.push(token.syntax(), SemanticTokenType::FUNCTION);
                        }
                        return Some(());
                    }
                }
            }

            if !emmyrc.semantic_tokens.render_documentation_markup {
                for token in description.tokens::<LuaGeneralToken>() {
                    if matches!(
                        token.get_token_kind(),
                        LuaTokenKind::TkDocDetail | LuaTokenKind::TkNormalStart
                    ) {
                        builder.push(token.syntax(), SemanticTokenType::COMMENT);
                    }
                }
                return None;
            }
            // 如果文档的开始是 #, 则需要将其渲染为注释而不是文档
            if let Some(start_token) = description.tokens::<LuaGeneralToken>().next() {
                if start_token.get_text().starts_with('#') {
                    builder.push_at_position(
                        start_token.get_range().start(),
                        1,
                        SemanticTokenType::COMMENT,
                        None,
                    );
                }
            }

            let desc_range = description.get_range();
            let document = semantic_model.get_document();
            let text = document.get_text();
            let items = parse_desc(
                semantic_model
                    .get_module()
                    .map(|m| m.workspace_id)
                    .unwrap_or(WorkspaceId::MAIN),
                emmyrc,
                text,
                description,
                None,
            );
            render_desc_ranges(builder, text, items, desc_range);
        }
        LuaAst::LuaDocTagLanguage(language) => {
            let name = language.get_name_token()?;
            builder.push(name.syntax(), SemanticTokenType::STRING);
            let language_text = name.get_name_text();
            let comment = language.ancestors::<LuaComment>().next()?;

            inject_language(builder, language_text, comment);
        }
        LuaAst::LuaLiteralExpr(literal_expr) => {
            let call_expr = literal_expr
                .get_parent::<LuaCallArgList>()?
                .get_parent::<LuaCallExpr>()?;
            let literal_token = literal_expr.get_literal()?;
            if let LuaLiteralToken::String(string_token) = literal_token
                && !builder.is_special_string_range(&string_token.get_range())
            {
                highlight_semantic_string_literal(builder, &call_expr, &string_token);
                fun_string_highlight(builder, semantic_model, call_expr, &string_token);
            }
        }
        LuaAst::LuaDocTagAttributeUse(tag_use) => {
            // 给 `@[` 染色
            if let Some(token) = tag_use.token_by_kind(LuaTokenKind::TkDocAttributeUse) {
                builder.push(token.syntax(), SemanticTokenType::KEYWORD);
            }
            // `]`染色
            if let Some(token) = tag_use.syntax().last_token() {
                builder.push(&token, SemanticTokenType::KEYWORD);
            }
            // 名称染色
            for attribute_use in tag_use.get_attribute_uses() {
                if let Some(token) = attribute_use.get_type()?.get_name_token() {
                    builder.push_with_modifiers(
                        token.syntax(),
                        SemanticTokenType::DECORATOR,
                        &[
                            SemanticTokenModifier::DECLARATION,
                            SemanticTokenModifier::DEFAULT_LIBRARY,
                        ],
                    );
                }
            }
        }
        LuaAst::LuaDocTagAttribute(tag_attribute) => {
            if let Some(name) = tag_attribute.get_name_token() {
                builder.push_with_modifier(
                    name.syntax(),
                    SemanticTokenType::TYPE,
                    SemanticTokenModifier::DECLARATION,
                );
            }
            if let Some(LuaDocType::Attribute(attribute)) = tag_attribute.get_type() {
                for param in attribute.get_params() {
                    if let Some(name) = param.get_name_token() {
                        builder.push(name.syntax(), SemanticTokenType::PARAMETER);
                    }
                }
            }
        }
        LuaAst::LuaDocInferType(infer_type) => {
            // 推断出的泛型定义
            if let Some(gen_decl) = infer_type.get_generic_decl() {
                render_type_parameter(builder, &gen_decl);
            }
            if let Some(name) = infer_type.token::<LuaNameToken>() {
                // 应该单独设置颜色
                if name.get_name_text() == "infer" {
                    builder.push(name.syntax(), SemanticTokenType::COMMENT);
                }
            }
        }
        _ => {}
    }

    Some(())
}

// 处理`local a = class``local a = class.method/field`
fn handle_name_node(
    semantic_model: &SemanticModel,
    builder: &mut SemanticBuilder,
    node: &LuaSyntaxNode,
    name_token: &LuaNameToken,
) -> Option<()> {
    let name_text = name_token.get_name_text();

    if name_text == "self" {
        builder.push_with_modifier(
            name_token.syntax(),
            SemanticTokenType::VARIABLE,
            SemanticTokenModifier::DEFINITION,
        );
        return Some(());
    }

    if is_scoped_scripted_class_name(semantic_model, name_text) {
        builder.push(name_token.syntax(), SemanticTokenType::CLASS);
        return Some(());
    }

    // 先查找声明，如果找不到声明再检查是否是 Lua 内置全局变量
    let semantic_decl = semantic_model.find_decl(node.clone().into(), SemanticDeclLevel::NoTrace);
    if semantic_decl.is_none() {
        if is_builtin_global_function(name_text) {
            builder.push_with_modifiers(
                name_token.syntax(),
                SemanticTokenType::FUNCTION,
                &[
                    SemanticTokenModifier::DEFAULT_LIBRARY,
                    SemanticTokenModifier::READONLY,
                ],
            );
            return Some(());
        }
        if is_builtin_global_constant(name_text) {
            builder.push_with_modifiers(
                name_token.syntax(),
                SemanticTokenType::VARIABLE,
                &[
                    SemanticTokenModifier::DEFAULT_LIBRARY,
                    SemanticTokenModifier::READONLY,
                    CustomSemanticTokenModifier::GLOBAL,
                ],
            );
            return Some(());
        }
        if is_builtin_global_namespace(name_text) {
            builder.push_with_modifier(
                name_token.syntax(),
                SemanticTokenType::NAMESPACE,
                SemanticTokenModifier::DEFAULT_LIBRARY,
            );
            return Some(());
        }
        if is_scoped_scripted_class_name(semantic_model, name_text) {
            builder.push(name_token.syntax(), SemanticTokenType::CLASS);
            return Some(());
        }
    }
    let semantic_decl = semantic_decl?;
    match semantic_decl {
        LuaSemanticDeclId::Member(member_id) => {
            let decl_type = semantic_model.get_type(member_id.into());
            let mut modifiers = vec![];
            enrich_modifiers_from_decl(semantic_model, &semantic_decl, &decl_type, &mut modifiers);
            if matches!(decl_type, LuaType::Signature(_)) {
                push_name_or_syntax_with_context_modifiers(
                    builder,
                    name_token.syntax(),
                    node,
                    SemanticTokenType::FUNCTION,
                    &modifiers,
                );
                return Some(());
            }
        }

        LuaSemanticDeclId::LuaDecl(decl_id) => {
            let decl = semantic_model
                .get_db()
                .get_decl_index()
                .get_decl(&decl_id)?;
            let decl_type = semantic_model.get_type(decl_id.into());
            let decl_is_default_library = is_default_library_decl(semantic_model, decl);
            let is_declaration = decl.get_range() == name_token.syntax().text_range();
            let is_scoped_class_global = is_scoped_scripted_class_global(semantic_model, decl);

            if decl.is_global() && decl_is_default_library {
                if should_treat_default_library_global_as_namespace(name_text, &decl_type) {
                    builder.push_with_modifier(
                        name_token.syntax(),
                        SemanticTokenType::NAMESPACE,
                        SemanticTokenModifier::DEFAULT_LIBRARY,
                    );
                    return Some(());
                }

                if is_builtin_global_constant(name_text) {
                    builder.push_with_modifiers(
                        name_token.syntax(),
                        SemanticTokenType::VARIABLE,
                        &[
                            SemanticTokenModifier::DEFAULT_LIBRARY,
                            SemanticTokenModifier::READONLY,
                            CustomSemanticTokenModifier::GLOBAL,
                        ],
                    );
                    return Some(());
                }
            }

            let base_type = if is_scoped_class_global {
                SemanticTokenType::CLASS
            } else if decl.is_param() {
                SemanticTokenType::PARAMETER
            } else {
                SemanticTokenType::VARIABLE
            };
            let param_callable = callable_parameter_type(semantic_model, decl);
            let decl_is_callable = is_function_like_type(&decl_type);

            if is_require_decl(semantic_model, decl) {
                let mut modifiers = vec![SemanticTokenModifier::READONLY];
                if is_declaration {
                    modifiers.push(SemanticTokenModifier::DECLARATION);
                }
                builder.push_with_modifiers(
                    name_token.syntax(),
                    SemanticTokenType::NAMESPACE,
                    &modifiers,
                );
                return Some(());
            }

            // GMod namespace pattern: user-defined global tables acting as namespace
            // containers (e.g. cityrp in cityrp.vehicle = cityrp.vehicle or {}).
            // When a global variable with an unresolved/generic type is used as an index
            // expression prefix, color it as NAMESPACE for better theme compatibility —
            // NAMESPACE maps to entity.name.namespace which is styled in most VS Code themes.
            if decl.is_global()
                && !decl_is_callable
                && !is_builtin_global_constant(name_text)
                && !matches!(
                    decl_type,
                    glua_code_analysis::LuaType::Def(_)
                        | glua_code_analysis::LuaType::Ref(_)
                        | glua_code_analysis::LuaType::Namespace(_)
                        | glua_code_analysis::LuaType::ModuleRef(_)
                )
                && is_index_expr_prefix(node)
            {
                builder.push_with_modifier(
                    name_token.syntax(),
                    SemanticTokenType::NAMESPACE,
                    CustomSemanticTokenModifier::GLOBAL,
                );
                return Some(());
            }

            let mut modifiers = vec![];
            enrich_modifiers_from_decl(semantic_model, &semantic_decl, &decl_type, &mut modifiers);
            let (token_type, mut modifier) = match &decl_type {
                LuaType::Def(type_id) => (
                    semantic_token_type_for_type_decl(semantic_model, type_id)
                        .unwrap_or(SemanticTokenType::CLASS),
                    None,
                ),
                LuaType::Ref(ref_id) => {
                    if check_ref_is_require_def(semantic_model, decl, ref_id).unwrap_or(false) {
                        (
                            SemanticTokenType::CLASS,
                            Some(SemanticTokenModifier::READONLY),
                        )
                    } else {
                        let token_type = if decl.is_global() || is_scoped_class_global {
                            semantic_token_type_for_type_decl(semantic_model, ref_id)
                                .unwrap_or(base_type)
                        } else {
                            base_type
                        };
                        (token_type, None)
                    }
                }
                LuaType::Namespace(_) => (
                    SemanticTokenType::NAMESPACE,
                    decl_is_default_library.then_some(SemanticTokenModifier::DEFAULT_LIBRARY),
                ),
                LuaType::Signature(signature) => {
                    let is_meta = semantic_model
                        .get_db()
                        .get_module_index()
                        .is_meta_file(&signature.get_file_id());
                    let token_type = if decl.is_param() || decl.get_value_syntax_id().is_some() {
                        base_type
                    } else {
                        SemanticTokenType::FUNCTION
                    };
                    (
                        token_type,
                        is_meta.then_some(SemanticTokenModifier::DEFAULT_LIBRARY),
                    )
                }
                LuaType::DocFunction(_) | LuaType::Union(_) => (base_type, None),
                _ => (base_type, None),
            };

            if modifier.is_none() && is_decl_readonly(decl) {
                modifier = Some(SemanticTokenModifier::READONLY);
            }

            if is_declaration {
                modifiers.push(SemanticTokenModifier::DECLARATION);
            }
            if is_modification_target(node) {
                modifiers.push(SemanticTokenModifier::MODIFICATION);
            }
            if param_callable || decl_is_callable {
                modifiers.push(CustomSemanticTokenModifier::CALLABLE);
            }
            if token_type == SemanticTokenType::VARIABLE
                || token_type == SemanticTokenType::PARAMETER
            {
                if decl.is_global() {
                    modifiers.push(CustomSemanticTokenModifier::GLOBAL);
                } else if !decl.is_param() && !is_scoped_class_global {
                    modifiers.push(CustomSemanticTokenModifier::LOCAL);
                }

                if is_object_like_decl_type(semantic_model, decl, &decl_type) {
                    modifiers.push(CustomSemanticTokenModifier::OBJECT);
                }
            }
            if let Some(modifier) = modifier {
                modifiers.push(modifier);
            }

            if !modifiers.is_empty() {
                builder.push_with_modifiers(name_token.syntax(), token_type, &modifiers);
            } else {
                builder.push(name_token.syntax(), token_type);
            }
            return Some(());
        }

        _ => {}
    }

    // 默认情况：如果不能确定类型，根据名称约定推断
    builder.push(
        name_token.syntax(),
        default_identifier_token_type(name_text),
    );
    Some(())
}

fn default_identifier_token_type(name_text: &str) -> SemanticTokenType {
    let _ = name_text;
    SemanticTokenType::VARIABLE
}

fn render_callable_name_token(
    semantic_model: &SemanticModel,
    builder: &mut SemanticBuilder,
    token: &LuaSyntaxToken,
    name_text: &str,
    value_type: Option<LuaType>,
) -> Option<()> {
    if is_builtin_global_function(name_text) {
        return builder.push_with_modifiers(
            token,
            SemanticTokenType::FUNCTION,
            &[
                SemanticTokenModifier::DEFAULT_LIBRARY,
                SemanticTokenModifier::READONLY,
            ],
        );
    }

    match value_type {
        Some(LuaType::Signature(signature)) => {
            let is_meta = semantic_model
                .get_db()
                .get_module_index()
                .is_meta_file(&signature.get_file_id());
            if is_meta {
                builder.push_with_modifiers(
                    token,
                    SemanticTokenType::FUNCTION,
                    &[
                        SemanticTokenModifier::DEFAULT_LIBRARY,
                        SemanticTokenModifier::READONLY,
                    ],
                )
            } else {
                builder.push_with_modifier(
                    token,
                    SemanticTokenType::FUNCTION,
                    CustomSemanticTokenModifier::CALLABLE,
                )
            }
        }
        Some(LuaType::DocFunction(_)) => builder.push_with_modifier(
            token,
            SemanticTokenType::FUNCTION,
            CustomSemanticTokenModifier::CALLABLE,
        ),
        Some(LuaType::Union(union)) if union.into_vec().iter().any(|typ| typ.is_function()) => {
            builder.push_with_modifier(
                token,
                SemanticTokenType::FUNCTION,
                CustomSemanticTokenModifier::CALLABLE,
            )
        }
        Some(other) if other.is_function() => builder.push_with_modifier(
            token,
            SemanticTokenType::FUNCTION,
            CustomSemanticTokenModifier::CALLABLE,
        ),
        _ => builder.push_with_modifier(
            token,
            SemanticTokenType::FUNCTION,
            CustomSemanticTokenModifier::CALLABLE,
        ),
    }
}

fn callable_parameter_type(semantic_model: &SemanticModel, decl: &LuaDecl) -> bool {
    match &decl.extra {
        LuaDeclExtra::Param {
            idx, signature_id, ..
        } => semantic_model
            .get_db()
            .get_signature_index()
            .get(signature_id)
            .and_then(|signature| signature.get_param_info_by_id(*idx))
            .is_some_and(|param_info| is_function_like_type(&param_info.type_ref)),
        _ => false,
    }
}

fn is_builtin_global_constant(name_text: &str) -> bool {
    matches!(name_text, "CLIENT" | "SERVER" | "MENU_DLL" | "_VERSION")
}

fn is_builtin_global_namespace(name_text: &str) -> bool {
    matches!(
        name_text,
        "_G" | "_ENV"
            | "arg"
            | "package"
            | "coroutine"
            | "string"
            | "utf8"
            | "table"
            | "math"
            | "io"
            | "os"
            | "debug"
            | "bit32"
    )
}

fn is_default_library_decl(semantic_model: &SemanticModel, decl: &LuaDecl) -> bool {
    let module_index = semantic_model.get_db().get_module_index();
    let file_id = decl.get_file_id();
    module_index.is_std(&file_id)
        || module_index.is_library(&file_id)
        || module_index.is_meta_file(&file_id)
}

fn should_treat_default_library_global_as_namespace(name_text: &str, decl_type: &LuaType) -> bool {
    if is_builtin_global_constant(name_text) || is_builtin_global_function(name_text) {
        return false;
    }

    if is_builtin_global_namespace(name_text) {
        return true;
    }

    !matches!(decl_type, LuaType::Def(_)) && !is_function_like_type(decl_type)
}

fn semantic_token_type_for_type_decl(
    semantic_model: &SemanticModel,
    type_id: &LuaTypeDeclId,
) -> Option<SemanticTokenType> {
    let type_decl = semantic_model
        .get_db()
        .get_type_index()
        .get_type_decl(type_id)?;

    if type_decl.is_enum() {
        Some(SemanticTokenType::ENUM)
    } else if type_decl.is_class() {
        Some(SemanticTokenType::CLASS)
    } else if type_decl.is_alias() {
        Some(SemanticTokenType::TYPE)
    } else {
        None
    }
}

fn is_object_like_decl_type(
    semantic_model: &SemanticModel,
    decl: &LuaDecl,
    decl_type: &LuaType,
) -> bool {
    if decl.is_global() {
        return false;
    }

    match decl_type {
        LuaType::Ref(type_id) => semantic_model
            .get_db()
            .get_type_index()
            .get_type_decl(type_id)
            .is_some_and(|type_decl| type_decl.is_class()),
        LuaType::Instance(_) => true,
        _ => false,
    }
}

fn is_scoped_scripted_class_global(semantic_model: &SemanticModel, decl: &LuaDecl) -> bool {
    is_scoped_scripted_class_name(semantic_model, decl.get_name())
}

fn is_scoped_scripted_class_name(semantic_model: &SemanticModel, name: &str) -> bool {
    if !name.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
        return false;
    }

    let db = semantic_model.get_db();
    if !db.get_emmyrc().gmod.enabled {
        return false;
    }

    let Some(file_path) = db.get_vfs().get_file_path(&semantic_model.get_file_id()) else {
        return false;
    };

    db.get_emmyrc()
        .gmod
        .scripted_class_scopes
        .detect_class_for_path(file_path)
        .is_some_and(|scope_match| scope_match.definition.class_global == name)
}

fn is_builtin_global_function(name_text: &str) -> bool {
    matches!(
        name_text,
        "require"
            | "load"
            | "loadfile"
            | "dofile"
            | "print"
            | "assert"
            | "error"
            | "warn"
            | "type"
            | "getmetatable"
            | "setmetatable"
            | "rawget"
            | "rawset"
            | "rawequal"
            | "rawlen"
            | "next"
            | "pairs"
            | "ipairs"
            | "tostring"
            | "tonumber"
            | "select"
            | "unpack"
            | "pcall"
            | "xpcall"
            | "collectgarbage"
    )
}

fn is_function_like_type(decl_type: &LuaType) -> bool {
    match decl_type {
        LuaType::DocFunction(_) => true,
        LuaType::Union(union) => union.into_vec().iter().any(|typ| typ.is_function()),
        _ => decl_type.is_function(),
    }
}

fn push_name_or_syntax_with_context_modifiers(
    builder: &mut SemanticBuilder,
    token: &LuaSyntaxToken,
    syntax: &LuaSyntaxNode,
    token_type: SemanticTokenType,
    modifiers: &[SemanticTokenModifier],
) -> Option<()> {
    let mut contextual_modifiers = modifiers.to_vec();
    if is_modification_target(syntax) {
        contextual_modifiers.push(SemanticTokenModifier::MODIFICATION);
    }

    if contextual_modifiers.is_empty() {
        builder.push(token, token_type)
    } else {
        builder.push_with_modifiers(token, token_type, &contextual_modifiers)
    }
}

fn is_modification_target(node: &LuaSyntaxNode) -> bool {
    let Some(name_expr) = LuaExpr::cast(node.clone()) else {
        return false;
    };
    let Some(assign_stat) = node.ancestors().find_map(LuaAssignStat::cast) else {
        return false;
    };
    let (vars, _) = assign_stat.get_var_and_expr_list();
    vars.into_iter()
        .any(|var| var.syntax() == name_expr.syntax())
}

fn highlight_semantic_string_literal(
    builder: &mut SemanticBuilder,
    call_expr: &LuaCallExpr,
    string_token: &glua_parser::LuaStringToken,
) {
    let Some(call_path) = call_expr.get_access_path() else {
        return;
    };

    let token = string_token.syntax();
    match call_path.as_str() {
        "hook.Add" | "hook.Run" | "hook.Call" => {
            let _ = builder.push(token, SemanticTokenType::EVENT);
        }
        "vgui.Register" | "derma.DefineControl" => {
            let _ = builder.push(token, SemanticTokenType::CLASS);
        }
        _ => {}
    }
}

fn is_decl_readonly(decl: &LuaDecl) -> bool {
    matches!(
        &decl.extra,
        LuaDeclExtra::Local {
            attrib: Some(LocalAttribute::Const | LocalAttribute::IterConst),
            ..
        } | LuaDeclExtra::ImplicitSelf { .. }
    )
}

/// Returns true if node is the prefix expression of an IndexExpr.
/// E.g., for cityrp.vehicle, the cityrp NameExpr node returns true.
fn is_index_expr_prefix(node: &glua_parser::LuaSyntaxNode) -> bool {
    node.parent()
        .is_some_and(|p| p.kind() == glua_parser::LuaSyntaxKind::IndexExpr.into())
}

/// Returns true for table-like types that commonly act as class/module containers
/// in GMod Lua (e.g., plain tables, table literals, object types).
fn is_table_like_type(decl_type: &glua_code_analysis::LuaType) -> bool {
    matches!(
        decl_type,
        glua_code_analysis::LuaType::Table
            | glua_code_analysis::LuaType::TableConst(_)
            | glua_code_analysis::LuaType::Object(_)
    )
}

fn render_doc_at(builder: &mut SemanticBuilder, token: &LuaSyntaxToken) {
    let text = token.text();
    // find '@'/'|'
    let mut start = 0;
    let mut len = 0;
    for (i, c) in text.char_indices() {
        if matches!(c, '@' | '|') {
            start = i;
            if c == '|' && text[i + c.len_utf8()..].starts_with(['+', '>']) {
                len = 2;
            } else {
                len = 1;
            }
            break;
        }
    }

    builder.push_at_range(
        &text[..start],
        TextRange::at(token.text_range().start(), TextSize::new(start as u32)),
        SemanticTokenType::COMMENT,
        &[],
    );

    builder.push_at_range(
        &text[start..start + len],
        TextRange::at(
            token.text_range().start() + TextSize::new(start as u32),
            TextSize::new(len as u32),
        ),
        SemanticTokenType::KEYWORD,
        &[SemanticTokenModifier::DOCUMENTATION],
    );
}

fn render_desc_ranges(
    builder: &mut SemanticBuilder,
    text: &str,
    items: Vec<DescItem>,
    desc_range: TextRange,
) {
    let mut pos = desc_range.start();

    for item in items {
        if item.range.start() > pos {
            // Ensure that we override IDE's default comment parsing algorithm.
            let detail_range = TextRange::new(pos, item.range.start());
            builder.push_at_range(
                &text[detail_range],
                detail_range,
                SemanticTokenType::COMMENT,
                &[],
            );
        }
        let token_text = &text[item.range];
        match item.kind {
            DescItemKind::Code | DescItemKind::CodeBlock | DescItemKind::Ref => {
                builder.push_at_range(
                    token_text,
                    item.range,
                    SemanticTokenType::VARIABLE,
                    &[SemanticTokenModifier::DOCUMENTATION],
                );
                pos = item.range.end();
            }
            DescItemKind::Link | DescItemKind::JavadocLink => {
                builder.push_at_range(
                    token_text,
                    item.range,
                    SemanticTokenType::STRING,
                    &[SemanticTokenModifier::DOCUMENTATION],
                );
                pos = item.range.end();
            }
            DescItemKind::Markup | DescItemKind::Arg => {
                builder.push_at_range(
                    token_text,
                    item.range,
                    SemanticTokenType::OPERATOR,
                    &[SemanticTokenModifier::DOCUMENTATION],
                );
                pos = item.range.end();
            }
            DescItemKind::CodeBlockHl(highlight_kind) => {
                let token_type = match highlight_kind {
                    CodeBlockHighlightKind::Keyword => SemanticTokenType::KEYWORD,
                    CodeBlockHighlightKind::String => SemanticTokenType::STRING,
                    CodeBlockHighlightKind::Number => SemanticTokenType::NUMBER,
                    CodeBlockHighlightKind::Comment => SemanticTokenType::COMMENT,
                    CodeBlockHighlightKind::Function => SemanticTokenType::FUNCTION,
                    CodeBlockHighlightKind::Class => SemanticTokenType::CLASS,
                    CodeBlockHighlightKind::Enum => SemanticTokenType::ENUM,
                    CodeBlockHighlightKind::Variable => SemanticTokenType::VARIABLE,
                    CodeBlockHighlightKind::Property => SemanticTokenType::PROPERTY,
                    CodeBlockHighlightKind::Decorator => SemanticTokenType::DECORATOR,
                    CodeBlockHighlightKind::Operators => SemanticTokenType::OPERATOR,
                    _ => continue, // Fallback for other kinds
                };
                builder.push_at_range(token_text, item.range, token_type, &[]);
                pos = item.range.end();
            }
            _ => {}
        }
    }

    if pos < desc_range.end() {
        let detail_range = TextRange::new(pos, desc_range.end());
        builder.push_at_range(
            &text[detail_range],
            detail_range,
            SemanticTokenType::COMMENT,
            &[],
        );
    }
}

// 检查导入语句是否是类定义
fn check_ref_is_require_def(
    semantic_model: &SemanticModel,
    decl: &LuaDecl,
    ref_id: &LuaTypeDeclId,
) -> Option<bool> {
    let module_info = parse_require_module_info(semantic_model, decl)?;
    match &module_info.export_type {
        Some(ty) => match ty {
            LuaType::Def(id) => Some(id == ref_id),
            _ => Some(false),
        },
        None => None,
    }
}

/// 是否为 `local x = require(...)` 的导入别名
fn is_require_decl(semantic_model: &SemanticModel, decl: &LuaDecl) -> bool {
    parse_require_module_info(semantic_model, decl).is_some()
}

fn render_type_parameter_list(
    builder: &mut SemanticBuilder,
    type_parameter_list: &LuaDocGenericDeclList,
) {
    for type_decl in type_parameter_list.get_generic_decl() {
        render_type_parameter(builder, &type_decl);
    }
}

fn render_type_parameter(builder: &mut SemanticBuilder, type_decl: &LuaDocGenericDecl) {
    if let Some(name) = type_decl.get_name_token() {
        builder.push_with_modifier(
            name.syntax(),
            SemanticTokenType::TYPE,
            SemanticTokenModifier::DECLARATION,
        );
    }
}

fn enrich_modifiers_from_decl(
    semantic_model: &glua_code_analysis::SemanticModel,
    semantic_decl: &glua_code_analysis::LuaSemanticDeclId,
    decl_type: &glua_code_analysis::LuaType,
    modifiers: &mut Vec<lsp_types::SemanticTokenModifier>,
) {
    let enrich_from_property =
        |property_id: &glua_code_analysis::LuaSemanticDeclId,
         mods: &mut Vec<lsp_types::SemanticTokenModifier>| {
            if let Some(property) = semantic_model
                .get_db()
                .get_property_index()
                .get_property(property_id)
            {
                if property.deprecated().is_some() {
                    mods.push(lsp_types::SemanticTokenModifier::DEPRECATED);
                }
                if property
                    .decl_features
                    .has_feature(glua_code_analysis::PropertyDeclFeature::ReadOnly)
                {
                    mods.push(lsp_types::SemanticTokenModifier::READONLY);
                }
            }
        };

    enrich_from_property(semantic_decl, modifiers);

    match decl_type {
        glua_code_analysis::LuaType::Signature(signature_id) => {
            if let Some(signature) = semantic_model
                .get_db()
                .get_signature_index()
                .get(signature_id)
            {
                if signature.async_state == glua_code_analysis::AsyncState::Async {
                    modifiers.push(lsp_types::SemanticTokenModifier::ASYNC);
                }
            }

            let sig_decl_id = glua_code_analysis::LuaSemanticDeclId::Signature(*signature_id);
            if sig_decl_id != *semantic_decl {
                enrich_from_property(&sig_decl_id, modifiers);
            }
        }
        _ => {}
    }
}
