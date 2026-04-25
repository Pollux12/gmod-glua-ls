use std::collections::{HashMap, HashSet};

use super::tpl_pattern::constant_decay;
use crate::{DbIndex, GenericTplId, LuaType, LuaTypeDeclId, LuaUnionType};

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

    pub fn set_type_candidate_collection_enabled(&mut self, enabled: bool) -> bool {
        let previous = self.collect_type_candidates;
        self.collect_type_candidates = enabled;
        previous
    }

    pub fn is_infer_all_tpl(&self) -> bool {
        for value in self.tpl_replace_map.values() {
            if let SubstitutorValue::None = value {
                return false;
            }
        }
        true
    }

    pub fn insert_type(&mut self, tpl_id: GenericTplId, replace_type: LuaType, decay: bool) {
        self.insert_type_value(tpl_id, SubstitutorTypeValue::new(replace_type, decay));
    }

    fn insert_type_value(&mut self, tpl_id: GenericTplId, value: SubstitutorTypeValue) {
        if self.collect_type_candidates {
            self.insert_type_candidate(tpl_id, value);
            return;
        }

        if !self.can_insert_type(tpl_id) {
            return;
        }

        self.tpl_replace_map
            .insert(tpl_id, SubstitutorValue::Type(value));
    }

    fn insert_type_candidate(&mut self, tpl_id: GenericTplId, value: SubstitutorTypeValue) {
        let existing_type = match self.tpl_replace_map.get(&tpl_id) {
            Some(SubstitutorValue::Type(existing)) => Some(existing.clone()),
            Some(SubstitutorValue::None) | None => None,
            Some(_) => return,
        };

        let inference = self
            .type_inferences
            .entry(tpl_id)
            .or_insert_with(TypeInferenceInfo::new);
        if let Some(existing) = existing_type {
            inference.add_candidate(existing);
        }
        inference.add_candidate(value);

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
    inferred: Option<SubstitutorTypeValue>,
}

impl TypeInferenceInfo {
    fn new() -> Self {
        Self {
            candidates: Vec::new(),
            inferred: None,
        }
    }

    fn add_candidate(&mut self, candidate: SubstitutorTypeValue) {
        if self.candidates.contains(&candidate) {
            return;
        }

        if let Some(inferred) = &mut self.inferred {
            inferred.union_with(candidate.clone());
        } else {
            self.inferred = Some(candidate.clone());
        }
        self.candidates.push(candidate);
    }

    fn inferred(&self) -> Option<&SubstitutorTypeValue> {
        self.inferred.as_ref()
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

fn nullable_any_type() -> LuaType {
    LuaType::Union(LuaUnionType::Nullable(LuaType::Any).into())
}
