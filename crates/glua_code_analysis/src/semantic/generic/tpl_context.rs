use glua_parser::LuaCallExpr;

use crate::{DbIndex, InferenceContext, InferencePriority, LuaInferCache};

#[derive(Debug)]
pub struct TplContext<'a> {
    pub db: &'a DbIndex,
    pub cache: &'a mut LuaInferCache,
    pub substitutor: &'a mut InferenceContext,
    pub call_expr: Option<LuaCallExpr>,
}

impl TplContext<'_> {
    pub fn with_inference_priority<R>(
        &mut self,
        priority: InferencePriority,
        collect_candidates: bool,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let state = self
            .substitutor
            .begin_candidate_collection(collect_candidates, priority);
        let result = f(self);
        self.substitutor.finish_candidate_collection(self.db, state);
        result
    }
}
