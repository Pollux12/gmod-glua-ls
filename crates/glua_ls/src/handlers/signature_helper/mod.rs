mod build_signature_helper;
mod signature_helper_builder;

use crate::context::ServerContextSnapshot;
pub use build_signature_helper::get_current_param_index;
use build_signature_helper::{build_callback_signature_helper, build_signature_helper};
use glua_code_analysis::{EmmyLuaAnalysis, FileId};
use glua_parser::{LuaAstNode, LuaCallExpr, LuaSyntaxKind, LuaTokenKind};
use lsp_types::Position;
use lsp_types::{
    ClientCapabilities, ServerCapabilities, SignatureHelp, SignatureHelpContext,
    SignatureHelpOptions, SignatureHelpParams, SignatureHelpTriggerKind,
};
use rowan::TokenAtOffset;
use tokio_util::sync::CancellationToken;

use super::RegisterCapabilities;

pub async fn on_signature_helper_handler(
    context: ServerContextSnapshot,
    params: SignatureHelpParams,
    cancel_token: CancellationToken,
) -> Option<SignatureHelp> {
    if cancel_token.is_cancelled() {
        return None;
    }
    let uri = params.text_document_position_params.text_document.uri;
    let analysis = context.analysis().read().await;

    if cancel_token.is_cancelled() {
        return None;
    }

    let file_id = analysis.get_file_id(&uri)?;
    let position = params.text_document_position_params.position;
    let param_context = params.context.unwrap_or(SignatureHelpContext {
        trigger_kind: SignatureHelpTriggerKind::INVOKED,
        trigger_character: None,
        is_retrigger: false,
        active_signature_help: None,
    });

    signature_help(&analysis, file_id, position, param_context)
}

pub fn signature_help(
    analysis: &EmmyLuaAnalysis,
    file_id: FileId,
    position: Position,
    param_context: SignatureHelpContext,
) -> Option<SignatureHelp> {
    let semantic_model = analysis.compilation.get_semantic_model(file_id)?;
    let root = semantic_model.get_root();
    let position_offset = {
        let document = semantic_model.get_document();
        document.get_offset(position.line as usize, position.character as usize)?
    };

    if position_offset > root.syntax().text_range().end() {
        return None;
    }

    let token = match root.syntax().token_at_offset(position_offset) {
        TokenAtOffset::Single(token) => token,
        TokenAtOffset::Between(left, _) => left,
        TokenAtOffset::None => {
            return None;
        }
    };
    if param_context.is_retrigger
        && matches!(
            token.kind().into(),
            LuaTokenKind::TkWhitespace | LuaTokenKind::TkEndOfLine
        )
    {
        let parent_kind = token.parent().map(|p| p.kind().into());
        if parent_kind == Some(LuaSyntaxKind::CallArgList)
            || parent_kind == Some(LuaSyntaxKind::ParamList)
        {
            // We don't return the active signature help here because we want to re-evaluate
            // the signature help based on the current context (e.g., which parameter we are on).
            // Returning the active signature help directly might cause the active parameter index
            // to be stale.
        } else {
            return None;
        }
    }

    let node = token.parent_ancestors().find(|node| {
        matches!(
            node.kind().into(),
            LuaSyntaxKind::CallArgList
                | LuaSyntaxKind::ParamList
                | LuaSyntaxKind::TypeGeneric
                | LuaSyntaxKind::DocTypeList
        )
    })?;
    match node.kind().into() {
        LuaSyntaxKind::CallArgList => {
            let call_expr = LuaCallExpr::cast(node.parent()?)?;
            build_signature_helper(&semantic_model, &analysis.compilation, call_expr, token)
        }
        LuaSyntaxKind::ParamList => {
            let closure_expr = glua_parser::LuaClosureExpr::cast(node.parent()?)?;
            build_callback_signature_helper(
                &semantic_model,
                &analysis.compilation,
                closure_expr,
                token,
            )
        }
        // todo
        LuaSyntaxKind::TypeGeneric | LuaSyntaxKind::DocTypeList => None,
        _ => None,
    }
}

pub struct SignatureHelperCapabilities;

impl RegisterCapabilities for SignatureHelperCapabilities {
    fn register_capabilities(server_capabilities: &mut ServerCapabilities, _: &ClientCapabilities) {
        server_capabilities.signature_help_provider = Some(SignatureHelpOptions {
            trigger_characters: Some(["(", ","].iter().map(|s| s.to_string()).collect()),
            retrigger_characters: Some(["(", ","].iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        });
    }
}
