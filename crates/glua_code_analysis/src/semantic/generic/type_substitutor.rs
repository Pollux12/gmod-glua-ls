use std::collections::{HashMap, HashSet};

use super::tpl_pattern::constant_decay;
use crate::{DbIndex, GenericTplId, LuaType, LuaTypeDeclId};

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

    pub fn from_type_decl(
        db: &DbIndex,
        type_array: Vec<LuaType>,
        type_decl_id: LuaTypeDeclId,
    ) -> Self {
        let type_array = type_array.into_iter().map(|ty| (ty, None)).collect();
        Self::from_decl_generic_params(db, type_array, type_decl_id, None)
    }

    pub fn from_alias(
        db: &DbIndex,
        type_array: Vec<LuaType>,
        alias_type_id: LuaTypeDeclId,
    ) -> Self {
        let type_array = type_array.into_iter().map(|ty| (ty, None)).collect();
        Self::from_decl_generic_params(db, type_array, alias_type_id.clone(), Some(alias_type_id))
    }

    pub fn from_alias_with_structural(
        db: &DbIndex,
        type_array: Vec<(LuaType, Option<LuaType>)>,
        alias_type_id: LuaTypeDeclId,
    ) -> Self {
        Self::from_decl_generic_params(db, type_array, alias_type_id.clone(), Some(alias_type_id))
    }

    fn from_decl_generic_params(
        db: &DbIndex,
        type_array: Vec<(LuaType, Option<LuaType>)>,
        type_decl_id: LuaTypeDeclId,
        alias_type_id: Option<LuaTypeDeclId>,
    ) -> Self {
        let mut tpl_replace_map = HashMap::new();
        let decl_tpl_ids = db
            .get_type_index()
            .get_generic_params(&type_decl_id)
            .and_then(|generic_params| {
                let mut tpl_ids = Vec::with_capacity(type_array.len());
                for param in generic_params.iter().take(type_array.len()) {
                    tpl_ids.push(param.tpl_id?);
                }

                (tpl_ids.len() == type_array.len()).then_some(tpl_ids)
            });

        if let Some(tpl_ids) = decl_tpl_ids {
            for (tpl_id, (ty, structural)) in tpl_ids.into_iter().zip(type_array) {
                tpl_replace_map.insert(
                    tpl_id,
                    SubstitutorValue::Type(substitutor_type_value(ty, structural, true)),
                );
            }
        } else {
            for (i, (ty, structural)) in type_array.into_iter().enumerate() {
                tpl_replace_map.insert(
                    GenericTplId::Type(i as u32),
                    SubstitutorValue::Type(substitutor_type_value(ty, structural, true)),
                );
            }
        }

        Self {
            tpl_replace_map,
            alias_type_id,
            self_type: None,
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

    pub fn insert_type(&mut self, tpl_id: GenericTplId, replace_type: LuaType, decay: bool) {
        self.insert_type_value(tpl_id, SubstitutorTypeValue::new(replace_type, decay));
    }

    pub fn insert_type_with_structural(
        &mut self,
        tpl_id: GenericTplId,
        replace_type: LuaType,
        structural_type: LuaType,
        decay: bool,
    ) {
        self.insert_type_value(
            tpl_id,
            SubstitutorTypeValue::new_with_structural(replace_type, structural_type, decay),
        );
    }

    fn insert_type_value(&mut self, tpl_id: GenericTplId, value: SubstitutorTypeValue) {
        if !self.can_insert_type(tpl_id) {
            return;
        }

        self.tpl_replace_map
            .insert(tpl_id, SubstitutorValue::Type(value));
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

    pub fn get_structural_type(&self, tpl_id: GenericTplId) -> Option<&LuaType> {
        match self.tpl_replace_map.get(&tpl_id) {
            Some(SubstitutorValue::Type(ty)) => ty.structural(),
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
pub struct SubstitutorTypeValue {
    raw: LuaType,
    default: LuaType,
    structural: Option<LuaType>,
}

impl SubstitutorTypeValue {
    pub fn new(raw: LuaType, decay: bool) -> Self {
        let raw = into_ref_type(raw);
        let default = if decay {
            into_ref_type(constant_decay(raw.clone()))
        } else {
            raw.clone()
        };
        Self {
            raw,
            default,
            structural: None,
        }
    }

    pub fn new_with_structural(raw: LuaType, structural: LuaType, decay: bool) -> Self {
        let mut value = Self::new(raw, decay);
        value.structural = Some(into_ref_type(structural));
        value
    }

    pub fn raw(&self) -> &LuaType {
        &self.raw
    }

    pub fn default(&self) -> &LuaType {
        &self.default
    }

    pub fn structural(&self) -> Option<&LuaType> {
        self.structural.as_ref()
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

fn substitutor_type_value(
    ty: LuaType,
    structural: Option<LuaType>,
    decay: bool,
) -> SubstitutorTypeValue {
    match structural {
        Some(structural) => SubstitutorTypeValue::new_with_structural(ty, structural, decay),
        None => SubstitutorTypeValue::new(ty, decay),
    }
}

fn into_ref_type(ty: LuaType) -> LuaType {
    match ty {
        LuaType::Def(type_decl_id) => LuaType::Ref(type_decl_id),
        _ => ty,
    }
}
