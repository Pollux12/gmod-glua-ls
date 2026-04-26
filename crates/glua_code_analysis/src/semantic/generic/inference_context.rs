use std::collections::{HashMap, HashSet};

use super::{
    TypeSubstitutor,
    instantiate_type::instantiate_type_generic,
    type_substitutor::{SubstitutorTypeValue, SubstitutorValue, candidate_assignable_to},
};
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
    type_inferences: HashMap<GenericTplId, TypeInferenceInfo>,
    type_constraints: HashMap<GenericTplId, LuaType>,
    collect_type_candidates: bool,
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
            type_inferences: HashMap::new(),
            type_constraints: HashMap::new(),
            collect_type_candidates: false,
            priority: InferencePriority::None,
            variance: InferenceVariance::Covariant,
        }
    }

    pub fn from_substitutor(substitutor: TypeSubstitutor) -> Self {
        Self {
            substitutor,
            type_inferences: HashMap::new(),
            type_constraints: HashMap::new(),
            collect_type_candidates: false,
            priority: InferencePriority::None,
            variance: InferenceVariance::Covariant,
        }
    }

    pub fn substitutor(&self) -> &TypeSubstitutor {
        &self.substitutor
    }

    pub fn priority(&self) -> InferencePriority {
        self.priority
    }

    pub fn variance(&self) -> InferenceVariance {
        self.variance
    }

    fn record_inferred_type(&mut self, tpl_id: GenericTplId, replace_type: LuaType, decay: bool) {
        let value = SubstitutorTypeValue::new(replace_type, decay);
        if self.collect_type_candidates {
            self.insert_type_candidate(tpl_id, value, self.priority, self.variance);
        } else {
            self.substitutor.set_fixed_type_value(tpl_id, value);
        }
    }

    pub fn infer_type(&mut self, tpl_id: GenericTplId, candidate: LuaType, decay: bool) {
        self.record_inferred_type(tpl_id, candidate, decay);
    }

    pub fn set_explicit_type(&mut self, tpl_id: GenericTplId, replace_type: LuaType, decay: bool) {
        self.substitutor
            .set_fixed_type_value(tpl_id, SubstitutorTypeValue::new(replace_type, decay));
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

    pub fn add_type_constraint(&mut self, tpl_id: GenericTplId, constraint: LuaType) {
        self.type_constraints.entry(tpl_id).or_insert(constraint);
    }

    pub fn is_fully_inferred(&self) -> bool {
        self.substitutor.is_infer_all_tpl()
    }

    pub fn has_non_direct_type_inferences(&self) -> bool {
        self.type_inferences.values().any(|inference| {
            !matches!(
                inference.priority(),
                InferencePriority::None | InferencePriority::Direct
            )
        })
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
        let previous_enabled = self.collect_type_candidates;
        self.collect_type_candidates = enabled;
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
            self.normalize_type_inferences(db);
        }

        self.collect_type_candidates = state.previous_enabled;
        self.priority = state.previous_priority;
        self.variance = state.previous_variance;
    }

    fn insert_type_candidate(
        &mut self,
        tpl_id: GenericTplId,
        value: SubstitutorTypeValue,
        priority: InferencePriority,
        variance: InferenceVariance,
    ) {
        let existing_type = match self.substitutor.get(tpl_id) {
            Some(SubstitutorValue::Type(existing)) => Some(existing.clone()),
            Some(SubstitutorValue::None) | None => None,
            Some(_) => return,
        };
        let include_existing_type =
            !self.type_inferences.contains_key(&tpl_id) && existing_type.is_some();

        let inference = self
            .type_inferences
            .entry(tpl_id)
            .or_insert_with(TypeInferenceInfo::new);

        if include_existing_type && let Some(existing) = existing_type {
            inference.add_candidate(existing, priority, InferenceVariance::Covariant);
        }

        inference.add_candidate(value, priority, variance);
        if let Some(inferred) = inference.inferred() {
            self.substitutor
                .set_inferred_type_value(tpl_id, inferred.clone());
        }
    }

    fn normalize_type_inferences(&mut self, db: &DbIndex) {
        let mut tpl_ids = self.type_inferences.keys().copied().collect::<Vec<_>>();
        tpl_ids.sort_by_key(|tpl_id| {
            let kind = match tpl_id {
                GenericTplId::Type(_) => 0,
                GenericTplId::ScopedType { .. } => 1,
                GenericTplId::Func(_) => 2,
            };
            (kind, tpl_id.get_idx())
        });
        let inference_snapshot = self.type_inferences.clone();
        let constraint_snapshot = self.type_constraints.clone();
        for tpl_id in tpl_ids {
            let constraint = self
                .type_constraints
                .get(&tpl_id)
                .map(|constraint| instantiate_type_generic(db, constraint, &self.substitutor));
            let Some(inference) = self.type_inferences.get_mut(&tpl_id) else {
                continue;
            };
            inference.normalize_with_common_supertype(
                db,
                tpl_id,
                constraint.as_ref(),
                &inference_snapshot,
                &constraint_snapshot,
            );
            if let Some(inferred) = inference.inferred() {
                self.substitutor
                    .set_inferred_type_value(tpl_id, inferred.clone());
            }
        }
    }
}

#[derive(Debug, Clone)]
struct TypeInferenceInfo {
    candidates: Vec<SubstitutorTypeValue>,
    contra_candidates: Vec<SubstitutorTypeValue>,
    inferred: Option<SubstitutorTypeValue>,
    priority: Option<InferencePriority>,
}

impl TypeInferenceInfo {
    fn new() -> Self {
        Self {
            candidates: Vec::new(),
            contra_candidates: Vec::new(),
            inferred: None,
            priority: None,
        }
    }

    fn add_candidate(
        &mut self,
        candidate: SubstitutorTypeValue,
        priority: InferencePriority,
        variance: InferenceVariance,
    ) {
        match self.priority {
            Some(existing) if priority.is_higher_than(existing) => {
                self.candidates.clear();
                self.contra_candidates.clear();
                self.inferred = None;
                self.priority = Some(priority);
            }
            Some(existing) if existing.is_higher_than(priority) => {
                return;
            }
            Some(_) => {}
            None => {
                self.priority = Some(priority);
            }
        }

        let candidates = match variance {
            InferenceVariance::Covariant => &mut self.candidates,
            InferenceVariance::Contravariant => &mut self.contra_candidates,
        };

        if candidates.contains(&candidate) {
            return;
        }

        if let Some(inferred) = &mut self.inferred {
            inferred.union_with(candidate.clone());
        } else {
            self.inferred = Some(candidate.clone());
        }
        candidates.push(candidate);
    }

    fn normalize_with_common_supertype(
        &mut self,
        db: &DbIndex,
        tpl_id: GenericTplId,
        constraint: Option<&LuaType>,
        all_inferences: &HashMap<GenericTplId, TypeInferenceInfo>,
        all_constraints: &HashMap<GenericTplId, LuaType>,
    ) {
        self.inferred =
            self.infer_candidate(db, tpl_id, constraint, all_inferences, all_constraints);
    }

    fn infer_candidate(
        &self,
        db: &DbIndex,
        tpl_id: GenericTplId,
        constraint: Option<&LuaType>,
        all_inferences: &HashMap<GenericTplId, TypeInferenceInfo>,
        all_constraints: &HashMap<GenericTplId, LuaType>,
    ) -> Option<SubstitutorTypeValue> {
        let covariant = self.infer_covariant_candidate(db);
        let contravariant = self.infer_contravariant_candidate(db);

        let (preferred, fallback) = match (covariant, contravariant) {
            (Some(covariant), Some(contravariant)) => {
                if self.prefer_covariant_candidate(
                    db,
                    tpl_id,
                    &covariant,
                    all_inferences,
                    all_constraints,
                ) {
                    (Some(covariant), Some(contravariant))
                } else {
                    (Some(contravariant), Some(covariant))
                }
            }
            (Some(covariant), None) => (Some(covariant), None),
            (None, Some(contravariant)) => (Some(contravariant), None),
            (None, None) => (None, None),
        };

        self.apply_constraint(db, preferred, fallback, constraint)
    }

    fn infer_covariant_candidate(&self, db: &DbIndex) -> Option<SubstitutorTypeValue> {
        let Some((first, rest)) = self.candidates.split_first() else {
            return None;
        };

        let mut inferred = first.clone();
        let combine_candidates = self
            .priority
            .is_some_and(InferencePriority::implies_candidate_combination);
        for candidate in rest {
            if combine_candidates {
                inferred.union_with(candidate.clone());
            } else {
                inferred.combine_with_common_supertype(db, candidate.clone());
            }
        }

        Some(inferred)
    }

    fn infer_contravariant_candidate(&self, db: &DbIndex) -> Option<SubstitutorTypeValue> {
        let Some((first, rest)) = self.contra_candidates.split_first() else {
            return None;
        };

        let mut inferred = first.clone();
        let combine_candidates = self
            .priority
            .is_some_and(InferencePriority::implies_candidate_combination);
        for candidate in rest {
            if combine_candidates {
                inferred.union_with(candidate.clone());
            } else {
                inferred.combine_with_common_subtype(db, candidate.clone());
            }
        }

        Some(inferred)
    }

    fn prefer_covariant_candidate(
        &self,
        db: &DbIndex,
        tpl_id: GenericTplId,
        covariant: &SubstitutorTypeValue,
        all_inferences: &HashMap<GenericTplId, TypeInferenceInfo>,
        all_constraints: &HashMap<GenericTplId, LuaType>,
    ) -> bool {
        if matches!(covariant.default(), LuaType::Any | LuaType::Never) {
            return false;
        }

        let assignable_to_contra = self
            .contra_candidates
            .iter()
            .any(|candidate| candidate_assignable_to(db, covariant.default(), candidate.default()));
        assignable_to_contra
            && dependent_constraint_candidates_accept_covariant(
                db,
                tpl_id,
                covariant,
                all_inferences,
                all_constraints,
            )
    }

    fn apply_constraint(
        &self,
        db: &DbIndex,
        preferred: Option<SubstitutorTypeValue>,
        fallback: Option<SubstitutorTypeValue>,
        constraint: Option<&LuaType>,
    ) -> Option<SubstitutorTypeValue> {
        let Some(constraint) = constraint else {
            return preferred;
        };

        if let Some(candidate) = preferred {
            if candidate_satisfies_constraint(db, &candidate, constraint) {
                return Some(candidate);
            }
            if self.priority() == InferencePriority::ContextualReturn
                && let Some(filtered) = filter_candidate_by_constraint(db, &candidate, constraint)
            {
                return Some(filtered);
            }
        }

        if let Some(candidate) = fallback
            && candidate_satisfies_constraint(db, &candidate, constraint)
        {
            return Some(candidate);
        }

        Some(SubstitutorTypeValue::new(constraint.clone(), false))
    }

    fn inferred(&self) -> Option<&SubstitutorTypeValue> {
        self.inferred.as_ref()
    }

    fn priority(&self) -> InferencePriority {
        self.priority.unwrap_or(InferencePriority::Direct)
    }
}

fn candidate_satisfies_constraint(
    db: &DbIndex,
    candidate: &SubstitutorTypeValue,
    constraint: &LuaType,
) -> bool {
    candidate_type_satisfies_constraint(db, candidate.raw(), constraint)
        || candidate_type_satisfies_constraint(db, candidate.default(), constraint)
}

fn filter_candidate_by_constraint(
    db: &DbIndex,
    candidate: &SubstitutorTypeValue,
    constraint: &LuaType,
) -> Option<SubstitutorTypeValue> {
    let raw = filter_type_by_constraint(db, candidate.raw(), constraint)?;
    let default = filter_type_by_constraint(db, candidate.default(), constraint)
        .unwrap_or_else(|| raw.clone());
    Some(SubstitutorTypeValue::with_raw_default(raw, default))
}

fn filter_type_by_constraint(
    db: &DbIndex,
    candidate: &LuaType,
    constraint: &LuaType,
) -> Option<LuaType> {
    if candidate_type_satisfies_constraint(db, candidate, constraint) {
        return Some(candidate.clone());
    }

    match candidate {
        LuaType::Union(union) => {
            let filtered = union
                .into_vec()
                .into_iter()
                .filter(|member| candidate_type_satisfies_constraint(db, member, constraint))
                .collect::<Vec<_>>();
            if filtered.is_empty() {
                None
            } else {
                Some(LuaType::from_vec(filtered))
            }
        }
        _ => None,
    }
}

fn dependent_constraint_candidates_accept_covariant(
    db: &DbIndex,
    tpl_id: GenericTplId,
    covariant: &SubstitutorTypeValue,
    all_inferences: &HashMap<GenericTplId, TypeInferenceInfo>,
    all_constraints: &HashMap<GenericTplId, LuaType>,
) -> bool {
    all_inferences.iter().all(|(other_tpl_id, inference)| {
        if *other_tpl_id == tpl_id {
            return true;
        }

        let Some(constraint) = all_constraints.get(other_tpl_id) else {
            return true;
        };
        if !constraint_is_direct_tpl_ref(constraint, tpl_id) {
            return true;
        }

        inference
            .candidates
            .iter()
            .all(|candidate| candidate_satisfies_constraint(db, candidate, covariant.default()))
    })
}

fn constraint_is_direct_tpl_ref(constraint: &LuaType, tpl_id: GenericTplId) -> bool {
    match constraint {
        LuaType::TplRef(tpl) | LuaType::ConstTplRef(tpl) => tpl.get_tpl_id() == tpl_id,
        _ => false,
    }
}

fn candidate_type_satisfies_constraint(
    db: &DbIndex,
    candidate: &LuaType,
    constraint: &LuaType,
) -> bool {
    if candidate_is_pending_str_tpl_ref(db, candidate, constraint) {
        return true;
    }

    if candidate_assignable_to(db, candidate, constraint) {
        return true;
    }

    if let LuaType::Tuple(tuple) = constraint {
        return tuple
            .get_types()
            .iter()
            .any(|member| candidate_assignable_to(db, candidate, member));
    }

    false
}

fn candidate_is_pending_str_tpl_ref(
    db: &DbIndex,
    candidate: &LuaType,
    constraint: &LuaType,
) -> bool {
    let LuaType::Ref(candidate_id) = candidate else {
        return false;
    };
    if db.get_type_index().get_type_decl(candidate_id).is_some() {
        return false;
    }

    let (LuaType::Ref(constraint_id) | LuaType::Def(constraint_id)) = constraint else {
        return false;
    };
    db.get_type_index()
        .get_type_decl(constraint_id)
        .is_some_and(|decl| decl.is_class())
}
