use glua_parser::LuaCallExpr;

use crate::{DbIndex, InferenceContext, LuaInferCache};

#[derive(Debug)]
pub struct TplContext<'a> {
    pub db: &'a DbIndex,
    pub cache: &'a mut LuaInferCache,
    pub substitutor: &'a mut InferenceContext,
    pub call_expr: Option<LuaCallExpr>,
}
