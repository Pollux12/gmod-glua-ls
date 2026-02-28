use glua_code_analysis::{LuaType, SemanticModel};
use glua_parser::{LuaAstNode, LuaAstToken, LuaCallExpr, LuaStringToken};
use glua_parser_desc::CodeBlockLang;

use crate::handlers::semantic_token::{
    language_injector::process_inject_lang_string_token, semantic_token_builder::SemanticBuilder,
};

pub fn fun_string_highlight(
    builder: &mut SemanticBuilder,
    semantic_model: &SemanticModel,
    call_expr: LuaCallExpr,
    string_token: &LuaStringToken,
) -> Option<()> {
    let func = semantic_model.infer_call_expr_func(call_expr.clone(), None)?;
    let params = func.get_params();
    let mut param_idx = call_expr
        .get_args_list()?
        .get_args()
        .position(|arg| arg.get_position() == string_token.get_position())?;

    let colon_define = func.is_colon_define();
    let colon_call = call_expr.is_colon_call();

    match (colon_define, colon_call) {
        (true, false) => {
            param_idx = param_idx.saturating_sub(1);
        }
        (false, true) => {
            param_idx += 1;
        }
        _ => {}
    }

    let (_, opt_typ) = params.get(param_idx)?;
    let param_type = opt_typ.as_ref()?;
    let lang_name = get_lang_str_from_type(param_type)?;
    match CodeBlockLang::try_parse(&lang_name) {
        Some(lang) => {
            process_inject_lang_string_token(builder, lang, string_token);
        }
        None => {
            // TODO
        }
    }
    Some(())
}

fn get_lang_str_from_type(typ: &LuaType) -> Option<String> {
    match typ {
        LuaType::Language(s) => return Some(s.to_string()),
        LuaType::Union(u) => {
            for sub_type in u.into_vec() {
                if let LuaType::Language(s) = sub_type {
                    return Some(s.to_string());
                }
            }
        }
        _ => {}
    }

    None
}
