use std::sync::Arc;

use crate::{
    InFiled, LuaInferenceConfidence, LuaInferenceEventId, LuaInferenceNodeId,
    LuaInferenceProvenanceKind, LuaType,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextualTypeEvidence {
    pub candidate: LuaType,
    pub confidence: LuaInferenceConfidence,
    pub event: LuaInferenceEventId,
    pub support: Arc<[LuaInferenceNodeId]>,
}

impl ContextualTypeEvidence {
    pub fn anchored(
        target: LuaInferenceNodeId,
        candidate: LuaType,
        source: InFiled<glua_parser::LuaSyntaxId>,
        support: Arc<[LuaInferenceNodeId]>,
    ) -> Self {
        Self {
            candidate,
            confidence: LuaInferenceConfidence::Anchored,
            event: LuaInferenceEventId {
                node: target,
                kind: LuaInferenceProvenanceKind::ContextualUnknown,
                source,
            },
            support,
        }
    }
}
