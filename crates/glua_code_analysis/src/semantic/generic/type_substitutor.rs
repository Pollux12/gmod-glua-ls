use std::collections::{HashMap, HashSet};

use super::{
    InferencePriority, InferenceVariance, instantiate_type::instantiate_type_generic,
    tpl_pattern::constant_decay,
};
use crate::{
    DbIndex, GenericTplId, LuaGenericType, LuaType, LuaTypeDeclId, LuaUnionType,
    semantic::type_check::check_type_compact,
};

#[derive(Debug, Clone)]
pub struct TypeSubstitutor {
    tpl_replace_map: HashMap<GenericTplId, SubstitutorValue>,
    type_inferences: HashMap<GenericTplId, TypeInferenceInfo>,
    alias_type_id: Option<LuaTypeDeclId>,
    self_type: Option<LuaType>,
    collect_type_candidates: bool,
}

impl Default for TypeSubstitutor {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeSubstitutor {
    pub fn new() -> Self {
        Self {
            tpl_replace_map: HashMap::new(),
            type_inferences: HashMap::new(),
            alias_type_id: None,
            self_type: None,
            collect_type_candidates: false,
        }
    }

    pub fn from_type_array(type_array: Vec<LuaType>) -> Self {
        let mut tpl_replace_map = HashMap::new();
        for (i, ty) in type_array.into_iter().enumerate() {
            tpl_replace_map.insert(
                GenericTplId::Type(i as u32),
                SubstitutorValue::Type(SubstitutorTypeValue::new(ty, true)),
            );
        }
        Self {
            tpl_replace_map,
            type_inferences: HashMap::new(),
            alias_type_id: None,
            self_type: None,
            collect_type_candidates: false,
        }
    }

    pub fn from_type_array_for_type(
        db: &DbIndex,
        type_decl_id: &LuaTypeDeclId,
        type_array: Vec<LuaType>,
    ) -> Self {
        let mut substitutor = Self::from_type_array(type_array.clone());
        substitutor.insert_decl_type_params(db, type_decl_id, type_array);
        substitutor
    }

    pub fn from_alias(type_array: Vec<LuaType>, alias_type_id: LuaTypeDeclId) -> Self {
        let mut tpl_replace_map = HashMap::new();
        for (i, ty) in type_array.into_iter().enumerate() {
            tpl_replace_map.insert(
                GenericTplId::Type(i as u32),
                SubstitutorValue::Type(SubstitutorTypeValue::new(ty, true)),
            );
        }
        Self {
            tpl_replace_map,
            type_inferences: HashMap::new(),
            alias_type_id: Some(alias_type_id),
            self_type: None,
            collect_type_candidates: false,
        }
    }

    pub fn from_alias_for_type(
        db: &DbIndex,
        type_array: Vec<LuaType>,
        alias_type_id: LuaTypeDeclId,
    ) -> Self {
        let mut substitutor = Self::from_type_array_for_type(db, &alias_type_id, type_array);
        substitutor.alias_type_id = Some(alias_type_id);
        substitutor
    }

    fn insert_decl_type_params(
        &mut self,
        db: &DbIndex,
        type_decl_id: &LuaTypeDeclId,
        type_array: Vec<LuaType>,
    ) {
        let Some(generic_params) = db.get_type_index().get_generic_params(type_decl_id) else {
            return;
        };

        for (i, generic_param) in generic_params.iter().enumerate() {
            let Some(tpl_id) = generic_param.tpl_id else {
                continue;
            };
            let Some(ty) = type_array.get(i) else {
                continue;
            };
            self.tpl_replace_map.insert(
                tpl_id,
                SubstitutorValue::Type(SubstitutorTypeValue::new(ty.clone(), true)),
            );
        }
    }

    pub fn add_need_infer_tpls(&mut self, tpl_ids: HashSet<GenericTplId>) {
        for tpl_id in tpl_ids {
            self.tpl_replace_map
                .entry(tpl_id)
                .or_insert(SubstitutorValue::None);
        }
    }

    pub(super) fn set_type_candidate_collection_enabled(&mut self, enabled: bool) -> bool {
        let previous = self.collect_type_candidates;
        self.collect_type_candidates = enabled;
        previous
    }

    pub(super) fn normalize_type_inferences(&mut self, db: &DbIndex) {
        for (tpl_id, inference) in self.type_inferences.iter_mut() {
            inference.normalize_with_common_supertype(db);
            if let Some(inferred) = inference.inferred() {
                self.tpl_replace_map
                    .insert(*tpl_id, SubstitutorValue::Type(inferred.clone()));
            }
        }
    }

    pub fn is_infer_all_tpl(&self) -> bool {
        for value in self.tpl_replace_map.values() {
            if let SubstitutorValue::None = value {
                return false;
            }
        }
        true
    }

    pub fn has_non_direct_type_inferences(&self) -> bool {
        self.type_inferences.values().any(|inference| {
            !matches!(
                inference.priority(),
                InferencePriority::None | InferencePriority::Direct
            )
        })
    }

    pub fn insert_type(&mut self, tpl_id: GenericTplId, replace_type: LuaType, decay: bool) {
        self.insert_type_with_priority(
            tpl_id,
            replace_type,
            decay,
            InferencePriority::Direct,
            InferenceVariance::Covariant,
        );
    }

    pub(super) fn insert_type_with_priority(
        &mut self,
        tpl_id: GenericTplId,
        replace_type: LuaType,
        decay: bool,
        priority: InferencePriority,
        variance: InferenceVariance,
    ) {
        self.insert_type_value(
            tpl_id,
            SubstitutorTypeValue::new(replace_type, decay),
            priority,
            variance,
        );
    }

    fn insert_type_value(
        &mut self,
        tpl_id: GenericTplId,
        value: SubstitutorTypeValue,
        priority: InferencePriority,
        variance: InferenceVariance,
    ) {
        if self.collect_type_candidates {
            self.insert_type_candidate(tpl_id, value, priority, variance);
            return;
        }

        if !self.can_insert_type(tpl_id) {
            return;
        }

        self.tpl_replace_map
            .insert(tpl_id, SubstitutorValue::Type(value));
    }

    fn insert_type_candidate(
        &mut self,
        tpl_id: GenericTplId,
        value: SubstitutorTypeValue,
        priority: InferencePriority,
        variance: InferenceVariance,
    ) {
        let existing_type = match self.tpl_replace_map.get(&tpl_id) {
            Some(SubstitutorValue::Type(existing)) => Some(existing.clone()),
            Some(SubstitutorValue::None) | None => None,
            Some(_) => return,
        };
        let had_inference = self.type_inferences.contains_key(&tpl_id);
        let include_existing_type = self
            .type_inferences
            .get(&tpl_id)
            .is_none_or(|inference| !priority.is_higher_than(inference.priority()));

        let inference = self
            .type_inferences
            .entry(tpl_id)
            .or_insert_with(TypeInferenceInfo::new);
        if include_existing_type && let Some(existing) = existing_type {
            let existing_variance = if had_inference {
                variance
            } else {
                InferenceVariance::Covariant
            };
            inference.add_candidate(existing, priority, existing_variance);
        }
        inference.add_candidate(value, priority, variance);

        if let Some(inferred) = inference.inferred() {
            self.tpl_replace_map
                .insert(tpl_id, SubstitutorValue::Type(inferred.clone()));
        }
    }

    fn can_insert_type(&self, tpl_id: GenericTplId) -> bool {
        if let Some(value) = self.tpl_replace_map.get(&tpl_id) {
            return value.is_none();
        }

        true
    }

    pub fn insert_params(&mut self, tpl_id: GenericTplId, params: Vec<(String, Option<LuaType>)>) {
        if !self.can_insert_type(tpl_id) {
            return;
        }

        let params = params
            .into_iter()
            .map(|(name, ty)| (name, ty.map(into_ref_type)))
            .collect();

        self.tpl_replace_map
            .insert(tpl_id, SubstitutorValue::Params(params));
    }

    pub fn insert_multi_types(&mut self, tpl_id: GenericTplId, types: Vec<LuaType>) {
        if !self.can_insert_type(tpl_id) {
            return;
        }

        self.tpl_replace_map
            .insert(tpl_id, SubstitutorValue::MultiTypes(types));
    }

    pub fn insert_multi_base(&mut self, tpl_id: GenericTplId, type_base: LuaType) {
        if !self.can_insert_type(tpl_id) {
            return;
        }

        self.tpl_replace_map
            .insert(tpl_id, SubstitutorValue::MultiBase(type_base));
    }

    pub fn get(&self, tpl_id: GenericTplId) -> Option<&SubstitutorValue> {
        self.tpl_replace_map.get(&tpl_id)
    }

    pub fn get_raw_type(&self, tpl_id: GenericTplId) -> Option<&LuaType> {
        match self.tpl_replace_map.get(&tpl_id) {
            Some(SubstitutorValue::Type(ty)) => Some(ty.raw()),
            _ => None,
        }
    }

    pub fn check_recursion(&self, type_id: &LuaTypeDeclId) -> bool {
        if let Some(alias_type_id) = &self.alias_type_id
            && alias_type_id == type_id
        {
            return true;
        }

        false
    }

    pub fn add_self_type(&mut self, self_type: LuaType) {
        self.self_type = Some(self_type);
    }

    pub fn get_self_type(&self) -> Option<&LuaType> {
        self.self_type.as_ref()
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

    fn normalize_with_common_supertype(&mut self, db: &DbIndex) {
        self.inferred = self.infer_candidate(db);
    }

    fn infer_candidate(&self, db: &DbIndex) -> Option<SubstitutorTypeValue> {
        let covariant = self.infer_covariant_candidate(db);
        let contravariant = self.infer_contravariant_candidate(db);

        match (covariant, contravariant) {
            (Some(covariant), Some(contravariant)) => {
                if self.prefer_covariant_candidate(db, &covariant) {
                    Some(covariant)
                } else {
                    Some(contravariant)
                }
            }
            (Some(covariant), None) => Some(covariant),
            (None, Some(contravariant)) => Some(contravariant),
            (None, None) => None,
        }
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

    fn prefer_covariant_candidate(&self, db: &DbIndex, covariant: &SubstitutorTypeValue) -> bool {
        self.contra_candidates.iter().any(|candidate| {
            check_type_compact(db, covariant.default(), candidate.default()).is_ok()
        })
    }

    fn inferred(&self) -> Option<&SubstitutorTypeValue> {
        self.inferred.as_ref()
    }

    fn priority(&self) -> InferencePriority {
        self.priority.unwrap_or(InferencePriority::Direct)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubstitutorTypeValue {
    raw: LuaType,
    default: LuaType,
}

impl SubstitutorTypeValue {
    pub fn new(raw: LuaType, decay: bool) -> Self {
        let raw = into_ref_type(raw);
        let default = if decay {
            into_ref_type(constant_decay(raw.clone()))
        } else {
            raw.clone()
        };
        Self { raw, default }
    }

    pub fn raw(&self) -> &LuaType {
        &self.raw
    }

    pub fn default(&self) -> &LuaType {
        &self.default
    }

    fn union_with(&mut self, other: SubstitutorTypeValue) {
        self.raw = union_candidate_type(self.raw.clone(), other.raw);
        self.default = union_candidate_type(self.default.clone(), other.default);
    }

    fn combine_with_common_supertype(&mut self, db: &DbIndex, other: SubstitutorTypeValue) {
        self.raw = common_candidate_type(db, self.raw.clone(), other.raw);
        self.default = common_candidate_type(db, self.default.clone(), other.default);
    }

    fn combine_with_common_subtype(&mut self, db: &DbIndex, other: SubstitutorTypeValue) {
        self.raw = common_subtype_candidate_type(db, self.raw.clone(), other.raw);
        self.default = common_subtype_candidate_type(db, self.default.clone(), other.default);
    }
}

#[derive(Debug, Clone)]
pub enum SubstitutorValue {
    None,
    Type(SubstitutorTypeValue),
    Params(Vec<(String, Option<LuaType>)>),
    MultiTypes(Vec<LuaType>),
    MultiBase(LuaType),
}

impl SubstitutorValue {
    pub fn is_none(&self) -> bool {
        matches!(self, SubstitutorValue::None)
    }
}

fn into_ref_type(ty: LuaType) -> LuaType {
    match ty {
        LuaType::Def(type_decl_id) => LuaType::Ref(type_decl_id),
        _ => ty,
    }
}

pub(super) fn union_candidate_type(left: LuaType, right: LuaType) -> LuaType {
    if left == right {
        return left;
    }

    match (&left, &right) {
        (LuaType::Any, right) if right.is_nullable() => nullable_any_type(),
        (left, LuaType::Any) if left.is_nullable() => nullable_any_type(),
        (LuaType::Any, _) | (_, LuaType::Any) => LuaType::Any,
        (LuaType::Never, _) => right,
        (_, LuaType::Never) => left,
        (LuaType::Unknown, _) => right,
        (_, LuaType::Unknown) => left,
        (LuaType::Integer, LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_))
        | (LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_), LuaType::Integer) => {
            LuaType::Integer
        }
        (LuaType::Number, right) if right.is_number() => LuaType::Number,
        (left, LuaType::Number) if left.is_number() => LuaType::Number,
        (LuaType::String, LuaType::StringConst(_) | LuaType::DocStringConst(_))
        | (LuaType::StringConst(_) | LuaType::DocStringConst(_), LuaType::String) => {
            LuaType::String
        }
        (LuaType::Boolean, LuaType::BooleanConst(_) | LuaType::DocBooleanConst(_))
        | (LuaType::BooleanConst(_) | LuaType::DocBooleanConst(_), LuaType::Boolean) => {
            LuaType::Boolean
        }
        (LuaType::BooleanConst(left), LuaType::BooleanConst(right)) => {
            if left == right {
                LuaType::BooleanConst(*left)
            } else {
                LuaType::Boolean
            }
        }
        (LuaType::DocBooleanConst(left), LuaType::DocBooleanConst(right)) => {
            if left == right {
                LuaType::DocBooleanConst(*left)
            } else {
                LuaType::Boolean
            }
        }
        (LuaType::Table, LuaType::TableConst(_)) | (LuaType::TableConst(_), LuaType::Table) => {
            LuaType::Table
        }
        (LuaType::Function, LuaType::DocFunction(_) | LuaType::Signature(_))
        | (LuaType::DocFunction(_) | LuaType::Signature(_), LuaType::Function) => LuaType::Function,
        (LuaType::Union(left_union), right) if !right.is_union() => {
            let mut types = left_union.into_vec();
            if !types.contains(right) {
                types.push(right.clone());
            }
            LuaType::from_vec(types)
        }
        (left, LuaType::Union(right_union)) if !left.is_union() => {
            let mut types = right_union.into_vec();
            if !types.contains(left) {
                types.push(left.clone());
            }
            LuaType::from_vec(types)
        }
        (LuaType::Union(left_union), LuaType::Union(right_union)) => {
            let mut types = left_union.into_vec();
            types.extend(right_union.into_vec());
            LuaType::from_vec(types)
        }
        _ => LuaType::from_vec(vec![left, right]),
    }
}

fn common_candidate_type(db: &DbIndex, left: LuaType, right: LuaType) -> LuaType {
    if left == right {
        return left;
    }

    match (&left, &right) {
        (LuaType::Any, right) if right.is_nullable() => return nullable_any_type(),
        (left, LuaType::Any) if left.is_nullable() => return nullable_any_type(),
        (LuaType::Any, _) | (_, LuaType::Any) => return LuaType::Any,
        (LuaType::Never, _) => return right,
        (_, LuaType::Never) => return left,
        (LuaType::Unknown, _) => return right,
        (_, LuaType::Unknown) => return left,
        _ => {}
    }

    if check_type_compact(db, &left, &right).is_ok() {
        return right;
    }

    if check_type_compact(db, &right, &left).is_ok() {
        return left;
    }

    if let Some(common) = common_nominal_supertype(db, &left, &right) {
        return common;
    }

    union_candidate_type(left, right)
}

fn common_subtype_candidate_type(db: &DbIndex, left: LuaType, right: LuaType) -> LuaType {
    if left == right {
        return left;
    }

    match (&left, &right) {
        (LuaType::Any, right) if right.is_nullable() => return nullable_any_type(),
        (left, LuaType::Any) if left.is_nullable() => return nullable_any_type(),
        (LuaType::Any, _) => return right,
        (_, LuaType::Any) => return left,
        (LuaType::Never, _) | (_, LuaType::Never) => return LuaType::Never,
        (LuaType::Unknown, _) => return right,
        (_, LuaType::Unknown) => return left,
        _ => {}
    }

    if check_type_compact(db, &right, &left).is_ok() {
        return right;
    }

    left
}

fn common_nominal_supertype(db: &DbIndex, left: &LuaType, right: &LuaType) -> Option<LuaType> {
    let left_supers = nominal_super_types_with_self(db, left)?;
    let right_supers = nominal_super_types_with_self(db, right)?;

    for left_candidate in &left_supers {
        for right_candidate in &right_supers {
            if let Some(common) = common_generic_candidate_type(db, left_candidate, right_candidate)
            {
                return Some(common);
            }

            if left_candidate == right_candidate {
                return Some(left_candidate.clone());
            }
        }
    }

    None
}

fn nominal_super_types_with_self(db: &DbIndex, ty: &LuaType) -> Option<Vec<LuaType>> {
    let mut super_types = Vec::new();
    let mut visited = HashSet::new();

    match ty {
        LuaType::Ref(type_id) | LuaType::Def(type_id) => {
            push_nominal_super_type(&mut super_types, LuaType::Ref(type_id.clone()));
            collect_decl_super_types(db, type_id, None, &mut super_types, &mut visited);
            Some(super_types)
        }
        LuaType::Generic(generic) => {
            push_nominal_super_type(&mut super_types, LuaType::Generic(generic.clone().into()));

            let base_type_id = generic.get_base_type_id_ref();
            let substitutor = TypeSubstitutor::from_type_array_for_type(
                db,
                base_type_id,
                generic.get_params().clone(),
            );
            collect_decl_super_types(
                db,
                base_type_id,
                Some(&substitutor),
                &mut super_types,
                &mut visited,
            );
            Some(super_types)
        }
        _ => None,
    }
}

fn collect_decl_super_types(
    db: &DbIndex,
    type_id: &LuaTypeDeclId,
    substitutor: Option<&TypeSubstitutor>,
    super_types: &mut Vec<LuaType>,
    visited: &mut HashSet<LuaTypeDeclId>,
) {
    if !visited.insert(type_id.clone()) {
        return;
    }

    let Some(decl_super_types) = db.get_type_index().get_super_types(type_id) else {
        return;
    };

    for super_type in decl_super_types {
        let instantiated_super = match substitutor {
            Some(substitutor) => instantiate_type_generic(db, &super_type, substitutor),
            None => super_type,
        };

        push_nominal_super_type(super_types, instantiated_super.clone());

        match instantiated_super {
            LuaType::Ref(super_type_id) | LuaType::Def(super_type_id) => {
                collect_decl_super_types(db, &super_type_id, None, super_types, visited);
            }
            LuaType::Generic(generic) => {
                let base_type_id = generic.get_base_type_id_ref();
                let substitutor = TypeSubstitutor::from_type_array_for_type(
                    db,
                    base_type_id,
                    generic.get_params().clone(),
                );
                collect_decl_super_types(
                    db,
                    base_type_id,
                    Some(&substitutor),
                    super_types,
                    visited,
                );
            }
            _ => {}
        }
    }
}

fn push_nominal_super_type(super_types: &mut Vec<LuaType>, ty: LuaType) {
    if !super_types.contains(&ty) {
        super_types.push(ty.clone());
    }

    if let LuaType::Generic(generic) = ty {
        let base = LuaType::Ref(generic.get_base_type_id());
        if !super_types.contains(&base) {
            super_types.push(base);
        }
    }
}

fn common_generic_candidate_type(db: &DbIndex, left: &LuaType, right: &LuaType) -> Option<LuaType> {
    let (LuaType::Generic(left_generic), LuaType::Generic(right_generic)) = (left, right) else {
        return None;
    };

    if left_generic.get_base_type_id_ref() != right_generic.get_base_type_id_ref() {
        return None;
    }

    let left_params = left_generic.get_params();
    let right_params = right_generic.get_params();
    if left_params.len() != right_params.len() {
        return None;
    }

    let params = left_params
        .iter()
        .zip(right_params.iter())
        .map(|(left_param, right_param)| {
            common_candidate_type(db, left_param.clone(), right_param.clone())
        })
        .collect();

    Some(LuaGenericType::new(left_generic.get_base_type_id(), params).into())
}

fn nullable_any_type() -> LuaType {
    LuaType::Union(LuaUnionType::Nullable(LuaType::Any).into())
}
