use std::ops::{Deref, DerefMut};

use super::TypeSubstitutor;

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

#[derive(Debug, Clone)]
pub struct InferenceContext {
    substitutor: TypeSubstitutor,
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
        }
    }

    pub fn from_substitutor(substitutor: TypeSubstitutor) -> Self {
        Self { substitutor }
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
