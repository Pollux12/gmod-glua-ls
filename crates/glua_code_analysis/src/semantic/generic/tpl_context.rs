use glua_parser::LuaCallExpr;
use rowan::TextRange;

use crate::{DbIndex, LuaInferCache, TypeSubstitutor};

#[derive(Debug)]
pub struct TplContext<'a> {
    pub db: &'a DbIndex,
    pub cache: &'a mut LuaInferCache,
    pub substitutor: &'a mut TypeSubstitutor,
    pub call_expr: Option<LuaCallExpr>,
    pub source_range: TextRange,
}
