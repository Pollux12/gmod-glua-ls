use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::{collections::HashMap, sync::Arc};

use glua_parser::{LuaAstNode, LuaClosureExpr, LuaDocFuncType};
use rowan::TextSize;

use crate::db_index::signature::async_state::AsyncState;
use crate::{DbIndex, LuaAttributeUse, SemanticModel, VariadicType, first_param_may_not_self};
use crate::{
    FileId,
    db_index::{LuaFunctionType, LuaType},
};

pub const CALL_ARG_ATTRIBUTE: &str = "call_arg";

#[derive(Debug)]
pub struct LuaSignature {
    pub generic_params: Vec<Arc<LuaGenericParamInfo>>,
    pub overloads: Vec<Arc<LuaFunctionType>>,
    pub param_docs: HashMap<usize, LuaDocParamInfo>,
    pub out_params: Vec<LuaOutParamInfo>,
    pub params: Vec<String>,
    pub return_docs: Vec<LuaDocReturnInfo>,
    pub resolve_return: SignatureReturnStatus,
    pub is_colon_define: bool,
    pub async_state: AsyncState,
    pub nodiscard: Option<LuaNoDiscard>,
    pub is_vararg: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaCallArgRole {
    pub param_idx: usize,
    pub domain: String,
    pub role: String,
    pub priority: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LuaNoDiscard {
    NoDiscard,
    NoDiscardWithMessage(Box<String>),
}

impl Default for LuaSignature {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaSignature {
    pub fn new() -> Self {
        Self {
            generic_params: Vec::new(),
            overloads: Vec::new(),
            param_docs: HashMap::new(),
            out_params: Vec::new(),
            params: Vec::new(),
            return_docs: Vec::new(),
            resolve_return: SignatureReturnStatus::UnResolve,
            is_colon_define: false,
            async_state: AsyncState::None,
            nodiscard: None,
            is_vararg: false,
        }
    }

    pub fn is_generic(&self) -> bool {
        !self.generic_params.is_empty()
    }

    pub fn is_resolve_return(&self) -> bool {
        self.resolve_return != SignatureReturnStatus::UnResolve
    }

    pub fn has_special_call_params(&self) -> bool {
        self.param_docs.values().any(|param_info| {
            type_contains_str_tpl_ref(&param_info.type_ref)
                || param_info.get_attribute_by_name("constructor").is_some()
        }) || !self.out_params.is_empty()
            || self
                .overloads
                .iter()
                .any(|overload| overload_has_special_call_params(overload.as_ref()))
    }

    pub fn has_call_arg_roles(&self) -> bool {
        self.param_docs.values().any(|param_info| {
            param_info
                .get_attribute_by_name(CALL_ARG_ATTRIBUTE)
                .is_some()
        })
    }

    pub fn call_arg_roles_for_param(&self, param_idx: usize) -> Vec<LuaCallArgRole> {
        let mut roles = Vec::new();
        self.visit_call_arg_roles_for_param(param_idx, &mut |role| roles.push(role.clone()));
        roles
    }

    pub fn visit_call_arg_roles_for_param<F>(&self, param_idx: usize, visitor: &mut F)
    where
        F: FnMut(&LuaCallArgRole),
    {
        if let Some(param_info) = self.get_param_info_by_id(param_idx) {
            visit_call_arg_roles_from_param_attribute(param_idx, param_info, visitor);
        }
    }

    pub fn call_arg_roles(&self) -> Vec<LuaCallArgRole> {
        let mut roles = Vec::new();
        for (param_idx, param_info) in &self.param_docs {
            visit_call_arg_roles_from_param_attribute(*param_idx, param_info, &mut |role| {
                roles.push(role.clone());
            });
        }
        roles.sort_by_key(|role| {
            (
                role.param_idx,
                std::cmp::Reverse(role.priority.unwrap_or(0)),
            )
        });
        roles
    }

    pub fn get_type_params(&self) -> Vec<(String, Option<LuaType>)> {
        let mut type_params = Vec::new();
        for (idx, param_name) in self.params.iter().enumerate() {
            if let Some(param_info) = self.param_docs.get(&idx) {
                type_params.push((param_name.clone(), Some(param_info.type_ref.clone())));
            } else {
                type_params.push((param_name.clone(), None));
            }
        }

        type_params
    }

    pub fn get_param_optional_flags(&self) -> Vec<bool> {
        (0..self.params.len())
            .map(|idx| {
                self.param_docs
                    .get(&idx)
                    .is_some_and(|param_info| param_info.default_value.is_some())
            })
            .collect()
    }

    pub fn find_param_idx(&self, param_name: &str) -> Option<usize> {
        self.params.iter().position(|name| name == param_name)
    }

    pub fn get_param_info_by_name(&self, param_name: &str) -> Option<&LuaDocParamInfo> {
        // fast enough
        let idx = self.params.iter().position(|name| name == param_name)?;
        self.param_docs.get(&idx)
    }

    pub fn get_param_name_by_id(&self, idx: usize) -> Option<String> {
        if idx < self.params.len() {
            return Some(self.params[idx].clone());
        } else if let Some(name) = self.params.last()
            && name == "..."
        {
            return Some(name.clone());
        }

        None
    }

    pub fn get_param_info_by_id(&self, idx: usize) -> Option<&LuaDocParamInfo> {
        if idx < self.params.len() {
            return self.param_docs.get(&idx);
        } else if let Some(name) = self.params.last()
            && name == "..."
        {
            return self.param_docs.get(&(self.params.len() - 1));
        }

        None
    }

    pub fn get_return_type(&self) -> LuaType {
        match self.return_docs.len() {
            0 => LuaType::Nil,
            1 => self.return_docs[0].type_ref.clone(),
            _ => LuaType::Variadic(
                VariadicType::Multi(
                    self.return_docs
                        .iter()
                        .map(|info| info.type_ref.clone())
                        .collect(),
                )
                .into(),
            ),
        }
    }

    pub fn is_method(&self, semantic_model: &SemanticModel, owner_type: Option<&LuaType>) -> bool {
        if self.is_colon_define {
            return true;
        }

        if let Some(param_info) = self.get_param_info_by_id(0) {
            let param_type = &param_info.type_ref;
            if param_type.is_self_infer() {
                return true;
            }
            match owner_type {
                Some(owner_type) => {
                    // 一些类型不应该被视为 method
                    if matches!(owner_type, LuaType::Ref(_) | LuaType::Def(_))
                        && first_param_may_not_self(param_type)
                    {
                        return false;
                    }

                    semantic_model
                        .type_check(owner_type, &param_info.type_ref)
                        .is_ok()
                }
                None => param_info.name == "self",
            }
        } else {
            false
        }
    }

    pub fn to_doc_func_type(&self) -> Arc<LuaFunctionType> {
        let params = self.get_type_params();
        let return_type = self.get_return_type();
        let is_vararg = self.is_vararg;
        let func_type = LuaFunctionType::new(
            self.async_state,
            self.is_colon_define,
            is_vararg,
            params,
            return_type,
        )
        .with_optional_params(self.get_param_optional_flags());
        Arc::new(func_type)
    }

    pub fn to_call_operator_func_type(&self) -> Arc<LuaFunctionType> {
        let mut params = self.get_type_params();
        if !params.is_empty() && !self.is_colon_define {
            params.remove(0);
        }

        let return_type = self.get_return_type();
        let mut optional_params = self.get_param_optional_flags();
        if !optional_params.is_empty() && !self.is_colon_define {
            optional_params.remove(0);
        }
        let func_type =
            LuaFunctionType::new(self.async_state, false, self.is_vararg, params, return_type)
                .with_optional_params(optional_params);
        Arc::new(func_type)
    }
}

fn visit_call_arg_roles_from_param_attribute<F>(
    param_idx: usize,
    param_info: &LuaDocParamInfo,
    visitor: &mut F,
) where
    F: FnMut(&LuaCallArgRole),
{
    for attribute_use in param_info.iter_attributes_by_name(CALL_ARG_ATTRIBUTE) {
        let Some(domain) = attribute_string_arg(attribute_use, "domain") else {
            continue;
        };
        let Some(role) = attribute_string_arg(attribute_use, "role") else {
            continue;
        };
        let priority = attribute_integer_arg(attribute_use, "priority");
        visitor(&LuaCallArgRole {
            param_idx,
            domain,
            role,
            priority,
        });
    }
}

fn attribute_string_arg(attribute_use: &LuaAttributeUse, name: &str) -> Option<String> {
    match attribute_use.get_param_by_name(name)? {
        LuaType::DocStringConst(value) | LuaType::StringConst(value) => Some(value.to_string()),
        _ => None,
    }
}

fn attribute_integer_arg(attribute_use: &LuaAttributeUse, name: &str) -> Option<i64> {
    match attribute_use.get_param_by_name(name)? {
        LuaType::DocIntegerConst(value) | LuaType::IntegerConst(value) => Some(*value),
        _ => None,
    }
}

pub fn visit_call_arg_roles_from_type<F>(
    db: &DbIndex,
    typ: &LuaType,
    arg_idx: usize,
    visitor: &mut F,
) where
    F: FnMut(&LuaCallArgRole),
{
    match typ {
        LuaType::Signature(signature_id) => {
            if let Some(signature) = db.get_signature_index().get(signature_id) {
                signature.visit_call_arg_roles_for_param(arg_idx, visitor);
            }
        }
        LuaType::TypeGuard(inner) => {
            visit_call_arg_roles_from_type(db, inner, arg_idx, visitor);
        }
        LuaType::TableOf(inner) => {
            visit_call_arg_roles_from_type(db, inner, arg_idx, visitor);
        }
        LuaType::Instance(instance) => {
            visit_call_arg_roles_from_type(db, instance.get_base(), arg_idx, visitor);
        }
        LuaType::Union(union) => match union.as_ref() {
            crate::db_index::LuaUnionType::Nullable(inner) => {
                visit_call_arg_roles_from_type(db, inner, arg_idx, visitor);
            }
            crate::db_index::LuaUnionType::Multi(types) => {
                for typ in types {
                    visit_call_arg_roles_from_type(db, typ, arg_idx, visitor);
                }
            }
        },
        LuaType::Intersection(intersection) => {
            for typ in intersection.get_types() {
                visit_call_arg_roles_from_type(db, typ, arg_idx, visitor);
            }
        }
        LuaType::MultiLineUnion(union) => {
            for (typ, _) in union.get_unions() {
                visit_call_arg_roles_from_type(db, typ, arg_idx, visitor);
            }
        }
        _ => {}
    }
}

pub fn find_call_arg_role_from_type(
    db: &DbIndex,
    typ: &LuaType,
    arg_idx: usize,
    domain: &str,
    roles: &[&str],
) -> Option<LuaCallArgRole> {
    let mut best: Option<LuaCallArgRole> = None;
    visit_call_arg_roles_from_type(db, typ, arg_idx, &mut |role| {
        if role.domain != domain || !roles.iter().any(|candidate| *candidate == role.role) {
            return;
        }

        if best
            .as_ref()
            .is_none_or(|current| role.priority.unwrap_or(0) > current.priority.unwrap_or(0))
        {
            best = Some(role.clone());
        }
    });
    best
}

fn type_contains_str_tpl_ref(typ: &LuaType) -> bool {
    match typ {
        LuaType::StrTplRef(_) => true,
        LuaType::TypeGuard(inner) => type_contains_str_tpl_ref(inner),
        LuaType::Union(union) => union.into_vec().iter().any(type_contains_str_tpl_ref),
        LuaType::Intersection(intersection) => intersection
            .get_types()
            .iter()
            .any(type_contains_str_tpl_ref),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .any(|(union_type, _)| type_contains_str_tpl_ref(union_type)),
        _ => false,
    }
}

fn overload_has_special_call_params(func: &LuaFunctionType) -> bool {
    func.get_params().iter().any(|(_, param_type)| {
        param_type
            .as_ref()
            .map(type_contains_str_tpl_ref)
            .unwrap_or(false)
    })
}

#[derive(Debug)]
pub struct LuaDocParamInfo {
    pub name: String,
    pub type_ref: LuaType,
    pub default_value: Option<LuaDocDefaultValue>,
    pub nullable: bool,
    pub description: Option<String>,
    pub attributes: Option<Vec<LuaAttributeUse>>,
}

impl LuaDocParamInfo {
    pub fn get_attribute_by_name(&self, name: &str) -> Option<&LuaAttributeUse> {
        self.attributes
            .iter()
            .flatten()
            .find(|attr| attr.id.get_name() == name)
    }

    pub fn iter_attributes_by_name<'a>(
        &'a self,
        name: &'a str,
    ) -> impl Iterator<Item = &'a LuaAttributeUse> + 'a {
        self.attributes
            .iter()
            .flatten()
            .filter(move |attr| attr.id.get_name() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::{CALL_ARG_ATTRIBUTE, LuaDocParamInfo, LuaSignature};
    use crate::{LuaAttributeUse, LuaType, LuaTypeDeclId};
    use smol_str::SmolStr;

    fn call_arg_attribute(domain: &str, role: &str) -> LuaAttributeUse {
        LuaAttributeUse::new(
            LuaTypeDeclId::global(CALL_ARG_ATTRIBUTE),
            vec![
                (
                    "domain".to_string(),
                    Some(LuaType::DocStringConst(SmolStr::new(domain).into())),
                ),
                (
                    "role".to_string(),
                    Some(LuaType::DocStringConst(SmolStr::new(role).into())),
                ),
            ],
        )
    }

    #[test]
    fn call_arg_roles_for_param_keeps_multiple_attributes() {
        let mut signature = LuaSignature::new();
        signature.params.push("name".to_string());
        signature.param_docs.insert(
            0,
            LuaDocParamInfo {
                name: "name".to_string(),
                type_ref: LuaType::String,
                default_value: None,
                nullable: false,
                description: None,
                attributes: Some(vec![
                    call_arg_attribute("gmod.vgui_panel", "define"),
                    call_arg_attribute("gmod.derma_skin", "reference"),
                ]),
            },
        );

        let roles = signature.call_arg_roles_for_param(0);

        assert_eq!(roles.len(), 2);
        assert_eq!(roles[0].domain, "gmod.vgui_panel");
        assert_eq!(roles[0].role, "define");
        assert_eq!(roles[0].param_idx, 0);
        assert_eq!(roles[1].domain, "gmod.derma_skin");
        assert_eq!(roles[1].role, "reference");
        assert_eq!(roles[1].param_idx, 0);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaOutParamInfo {
    pub param_idx: usize,
    pub field_path: Vec<String>,
    pub type_ref: LuaType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReturnTypeKind {
    #[default]
    Reference,
    Instance,
    Definition,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LuaDocDefaultValue {
    Nil,
    Boolean(bool),
    Number(String),
    String(String),
}

#[derive(Debug, Clone)]
pub struct LuaDocReturnInfo {
    pub name: Option<String>,
    pub type_ref: LuaType,
    pub default_value: Option<LuaDocDefaultValue>,
    pub description: Option<String>,
    pub attributes: Option<Vec<LuaAttributeUse>>,
    pub return_kind: ReturnTypeKind,
}

#[derive(Debug, Hash, Eq, PartialEq, Clone, Copy)]
pub struct LuaSignatureId {
    file_id: FileId,
    position: TextSize,
}

impl Serialize for LuaSignatureId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let value = format!("{}|{}", self.file_id.id, u32::from(self.position));
        serializer.serialize_str(&value)
    }
}

impl<'de> Deserialize<'de> for LuaSignatureId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LuaSignatureIdVisitor;

        impl<'de> Visitor<'de> for LuaSignatureIdVisitor {
            type Value = LuaSignatureId;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string with format 'file_id:position'")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let parts: Vec<&str> = value.split('|').collect();
                if parts.len() != 2 {
                    return Err(E::custom("expected format 'file_id:position'"));
                }

                let file_id = FileId {
                    id: parts[0]
                        .parse()
                        .map_err(|e| E::custom(format!("invalid file_id: {}", e)))?,
                };
                let position = TextSize::new(
                    parts[1]
                        .parse()
                        .map_err(|e| E::custom(format!("invalid position: {}", e)))?,
                );

                Ok(LuaSignatureId { file_id, position })
            }
        }

        deserializer.deserialize_str(LuaSignatureIdVisitor)
    }
}

impl LuaSignatureId {
    pub fn from_closure(file_id: FileId, closure: &LuaClosureExpr) -> Self {
        Self {
            file_id,
            position: closure.get_position(),
        }
    }

    pub fn from_doc_func(file_id: FileId, func_type: &LuaDocFuncType) -> Self {
        Self {
            file_id,
            position: func_type.get_position(),
        }
    }

    pub fn get_file_id(&self) -> FileId {
        self.file_id
    }

    pub fn get_position(&self) -> TextSize {
        self.position
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignatureReturnStatus {
    UnResolve,
    DocResolve,
    InferResolve,
}

#[derive(Debug, Clone)]
pub struct LuaGenericParamInfo {
    pub name: String,
    pub constraint: Option<LuaType>,
    pub attributes: Option<Vec<LuaAttributeUse>>,
}

impl LuaGenericParamInfo {
    pub fn new(
        name: String,
        constraint: Option<LuaType>,
        attributes: Option<Vec<LuaAttributeUse>>,
    ) -> Self {
        Self {
            name,
            constraint,
            attributes,
        }
    }
}
