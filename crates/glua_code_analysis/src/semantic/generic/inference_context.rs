use std::{
    collections::HashSet,
    ops::{Deref, DerefMut},
};

use super::{type_substitutor::SubstitutorValue, TypeSubstitutor};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceVariance {
    Covariant,
    Contravariant,
}

#[derive(Debug, Clone)]
pub struct InferenceContext {
    substitutor: TypeSubstitutor,
    priority: InferencePriority,
    variance: InferenceVariance,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct CandidateCollectionState {
    enabled: bool,
    previous_enabled: bool,
    previous_priority: InferencePriority,
    previous_variance: InferenceVariance,
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
            variance: InferenceVariance::Covariant,
        }
    }

    pub fn from_substitutor(substitutor: TypeSubstitutor) -> Self {
        Self {
            substitutor,
            priority: InferencePriority::None,
            variance: InferenceVariance::Covariant,
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

    pub fn variance(&self) -> InferenceVariance {
        self.variance
    }

    pub fn insert_type(&mut self, tpl_id: GenericTplId, replace_type: LuaType, decay: bool) {
        self.substitutor.insert_type_with_priority(
            tpl_id,
            replace_type,
            decay,
            self.priority,
            self.variance,
        );
    }

    pub fn infer_type(&mut self, tpl_id: GenericTplId, candidate: LuaType, decay: bool) {
        self.insert_type(tpl_id, candidate, decay);
    }

    pub fn set_explicit_type(&mut self, tpl_id: GenericTplId, replace_type: LuaType, decay: bool) {
        self.insert_type(tpl_id, replace_type, decay);
    }

    pub fn infer_params(&mut self, tpl_id: GenericTplId, params: Vec<(String, Option<LuaType>)>) {
        self.substitutor.insert_params(tpl_id, params);
    }

    pub fn infer_multi_types(&mut self, tpl_id: GenericTplId, types: Vec<LuaType>) {
        self.substitutor.insert_multi_types(tpl_id, types);
    }

    pub fn infer_multi_base(&mut self, tpl_id: GenericTplId, type_base: LuaType) {
        self.substitutor.insert_multi_base(tpl_id, type_base);
    }

    pub fn add_pending_type_parameters(&mut self, tpl_ids: HashSet<GenericTplId>) {
        self.substitutor.add_need_infer_tpls(tpl_ids);
    }

    pub fn is_fully_inferred(&self) -> bool {
        self.substitutor.is_infer_all_tpl()
    }

    pub fn has_non_direct_type_inferences(&self) -> bool {
        self.substitutor.has_non_direct_type_inferences()
    }

    pub fn add_self_type(&mut self, self_type: LuaType) {
        self.substitutor.add_self_type(self_type);
    }

    pub fn get_self_type(&self) -> Option<&LuaType> {
        self.substitutor.get_self_type()
    }

    pub fn get(&self, tpl_id: GenericTplId) -> Option<&SubstitutorValue> {
        self.substitutor.get(tpl_id)
    }

    pub(super) fn begin_candidate_collection(
        &mut self,
        enabled: bool,
        priority: InferencePriority,
        variance: InferenceVariance,
    ) -> CandidateCollectionState {
        let previous_priority = self.priority;
        let previous_variance = self.variance;
        let previous_enabled = self
            .substitutor
            .set_type_candidate_collection_enabled(enabled);
        self.priority = if enabled {
            priority
        } else {
            InferencePriority::None
        };
        self.variance = if enabled {
            variance
        } else {
            InferenceVariance::Covariant
        };

        CandidateCollectionState {
            enabled,
            previous_enabled,
            previous_priority,
            previous_variance,
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
        self.variance = state.previous_variance;
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
