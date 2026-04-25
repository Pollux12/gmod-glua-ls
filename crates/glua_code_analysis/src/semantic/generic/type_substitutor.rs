use std::collections::{HashMap, HashSet};

use super::tpl_pattern::constant_decay;
use crate::{
    DbIndex, GenericTplId, LuaType, LuaTypeDeclId, LuaUnionType,
    semantic::type_check::{check_type_compact, is_sub_type_of},
};

#[derive(Debug, Clone)]
pub struct TypeSubstitutor {
    tpl_replace_map: HashMap<GenericTplId, SubstitutorValue>,
    alias_type_id: Option<LuaTypeDeclId>,
    self_type: Option<LuaType>,
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
            alias_type_id: None,
            self_type: None,
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
            alias_type_id: None,
            self_type: None,
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
            alias_type_id: Some(alias_type_id),
            self_type: None,
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

    pub fn is_infer_all_tpl(&self) -> bool {
        for value in self.tpl_replace_map.values() {
            if let SubstitutorValue::None = value {
                return false;
            }
        }
        true
    }

    pub(super) fn set_fixed_type(
        &mut self,
        tpl_id: GenericTplId,
        replace_type: LuaType,
        decay: bool,
    ) {
        self.set_fixed_type_value(tpl_id, SubstitutorTypeValue::new(replace_type, decay));
    }

    pub(super) fn set_fixed_type_value(
        &mut self,
        tpl_id: GenericTplId,
        value: SubstitutorTypeValue,
    ) {
        if !self.can_insert_type(tpl_id) {
            return;
        }

        self.tpl_replace_map
            .insert(tpl_id, SubstitutorValue::Type(value));
    }

    pub(super) fn set_inferred_type_value(
        &mut self,
        tpl_id: GenericTplId,
        value: SubstitutorTypeValue,
    ) {
        self.tpl_replace_map
            .insert(tpl_id, SubstitutorValue::Type(value));
    }

    pub(super) fn can_insert_type(&self, tpl_id: GenericTplId) -> bool {
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

    pub(super) fn union_with(&mut self, other: SubstitutorTypeValue) {
        self.raw = union_candidate_type(self.raw.clone(), other.raw);
        self.default = union_candidate_type(self.default.clone(), other.default);
    }

    pub(super) fn combine_with_common_supertype(
        &mut self,
        db: &DbIndex,
        other: SubstitutorTypeValue,
    ) {
        self.raw = common_candidate_type(db, self.raw.clone(), other.raw);
        self.default = common_candidate_type(db, self.default.clone(), other.default);
    }

    pub(super) fn combine_with_common_subtype(
        &mut self,
        db: &DbIndex,
        other: SubstitutorTypeValue,
    ) {
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
        return left;
    }

    if check_type_compact(db, &right, &left).is_ok() {
        return right;
    }

    if literal_types_with_same_base(&left, &right) {
        return union_candidate_type(left, right);
    }

    if object_or_array_literal_candidate(&left) && object_or_array_literal_candidate(&right) {
        return union_candidate_type(left, right);
    }

    left
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

    if check_type_compact(db, &left, &right).is_ok() {
        return right;
    }

    if check_type_compact(db, &right, &left).is_ok() {
        return left;
    }

    left
}

pub(super) fn candidate_assignable_to(db: &DbIndex, source: &LuaType, target: &LuaType) -> bool {
    if source == target {
        return true;
    }

    match (source, target) {
        (
            LuaType::Ref(source_id) | LuaType::Def(source_id),
            LuaType::Ref(target_id) | LuaType::Def(target_id),
        ) => is_sub_type_of(db, source_id, target_id),
        (LuaType::Generic(source_generic), LuaType::Ref(target_id) | LuaType::Def(target_id)) => {
            let source_id = source_generic.get_base_type_id_ref();
            source_id == target_id || is_sub_type_of(db, source_id, target_id)
        }
        (LuaType::Ref(source_id) | LuaType::Def(source_id), LuaType::Generic(target_generic)) => {
            let target_id = target_generic.get_base_type_id_ref();
            source_id == target_id || is_sub_type_of(db, source_id, target_id)
        }
        (LuaType::Generic(source_generic), LuaType::Generic(target_generic)) => {
            source_generic.get_base_type_id_ref() == target_generic.get_base_type_id_ref()
                && source_generic.get_params().len() == target_generic.get_params().len()
                && source_generic
                    .get_params()
                    .iter()
                    .zip(target_generic.get_params().iter())
                    .all(|(source_param, target_param)| {
                        candidate_assignable_to(db, source_param, target_param)
                    })
        }
        _ => check_type_compact(db, target, source).is_ok(),
    }
}

fn nullable_any_type() -> LuaType {
    LuaType::Union(LuaUnionType::Nullable(LuaType::Any).into())
}

fn literal_types_with_same_base(left: &LuaType, right: &LuaType) -> bool {
    match (left, right) {
        (
            LuaType::StringConst(_) | LuaType::DocStringConst(_),
            LuaType::StringConst(_) | LuaType::DocStringConst(_),
        )
        | (
            LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_) | LuaType::FloatConst(_),
            LuaType::IntegerConst(_) | LuaType::DocIntegerConst(_) | LuaType::FloatConst(_),
        )
        | (
            LuaType::BooleanConst(_) | LuaType::DocBooleanConst(_),
            LuaType::BooleanConst(_) | LuaType::DocBooleanConst(_),
        ) => true,
        _ => false,
    }
}

fn object_or_array_literal_candidate(ty: &LuaType) -> bool {
    matches!(
        ty,
        LuaType::Object(_) | LuaType::TableConst(_) | LuaType::Array(_) | LuaType::Tuple(_)
    )
}
