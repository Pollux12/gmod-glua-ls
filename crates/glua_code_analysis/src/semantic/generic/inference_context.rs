use std::ops::{Deref, DerefMut};

use super::TypeSubstitutor;
use crate::{DbIndex, GenericTplId, LuaType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferencePriority {
    None,
    Direct,
    ContextualReturn,
    HomomorphicMappedType,
    PartialHomomorphicMappedType,
    MappedTypeConstraint,
    NakedUnionFallback,
}

impl InferencePriority {
    pub fn is_higher_than(self, other: Self) -> bool {
        self.rank() < other.rank()
    }

    pub fn implies_candidate_combination(self) -> bool {
        matches!(
            self,
            InferencePriority::ContextualReturn | InferencePriority::MappedTypeConstraint
        )
    }

    fn rank(self) -> u16 {
        match self {
            InferencePriority::None | InferencePriority::Direct => 0,
            InferencePriority::NakedUnionFallback => 1,
            InferencePriority::HomomorphicMappedType => 8,
            InferencePriority::PartialHomomorphicMappedType => 16,
            InferencePriority::MappedTypeConstraint => 32,
            InferencePriority::ContextualReturn => 128,
        }
    }
}

#[derive(Debug, Clone)]
pub struct InferenceContext {
    substitutor: TypeSubstitutor,
    priority: InferencePriority,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct CandidateCollectionState {
    enabled: bool,
    previous_enabled: bool,
    previous_priority: InferencePriority,
}

impl Default for InferenceContext {
    fn default() -> Self {
        Self::new()
    }
}

impl InferenceContext {
    pub fn new() -> Self {
        Self {
            substitutor: TypeSubstitutor::new(),
            priority: InferencePriority::None,
        }
    }

    pub fn from_substitutor(substitutor: TypeSubstitutor) -> Self {
        Self {
            substitutor,
            priority: InferencePriority::None,
        }
    }

    pub fn into_substitutor(self) -> TypeSubstitutor {
        self.substitutor
    }

    pub fn substitutor(&self) -> &TypeSubstitutor {
        &self.substitutor
    }

    pub fn substitutor_mut(&mut self) -> &mut TypeSubstitutor {
        &mut self.substitutor
    }

    pub fn priority(&self) -> InferencePriority {
        self.priority
    }

    pub fn insert_type(&mut self, tpl_id: GenericTplId, replace_type: LuaType, decay: bool) {
        self.substitutor
            .insert_type_with_priority(tpl_id, replace_type, decay, self.priority);
    }

    pub(super) fn begin_candidate_collection(
        &mut self,
        enabled: bool,
        priority: InferencePriority,
    ) -> CandidateCollectionState {
        let previous_priority = self.priority;
        let previous_enabled = self
            .substitutor
            .set_type_candidate_collection_enabled(enabled);
        self.priority = if enabled {
            priority
        } else {
            InferencePriority::None
        };

        CandidateCollectionState {
            enabled,
            previous_enabled,
            previous_priority,
        }
    }

    pub(super) fn finish_candidate_collection(
        &mut self,
        db: &DbIndex,
        state: CandidateCollectionState,
    ) {
        if state.enabled {
            self.substitutor.normalize_type_inferences(db);
        }

        self.substitutor
            .set_type_candidate_collection_enabled(state.previous_enabled);
        self.priority = state.previous_priority;
    }
}

impl Deref for InferenceContext {
    type Target = TypeSubstitutor;

    fn deref(&self) -> &Self::Target {
        &self.substitutor
    }
}

impl DerefMut for InferenceContext {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.substitutor
    }
}
