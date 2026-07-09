use std::{cmp::Ordering, collections::HashSet, sync::Arc};

use glua_parser::LuaSyntaxId;

use crate::{FileId, InFiled, LuaDeclId, LuaSignatureId};

use super::{LuaType, LuaTypeOwner};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LuaInferenceConfidence {
    Unknown,
    Heuristic,
    Anchored,
    Certain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LuaInferenceProvenanceKind {
    ExplicitAnnotation,
    ConcreteValue,
    ContextualUnknown,
    UnguardedChild,
    Assignment,
    FlowGuard,
    FlowMerge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LuaDefinitionId {
    Declaration(LuaDeclId),
    Assignment {
        file_id: FileId,
        assignment: LuaSyntaxId,
        target_idx: u16,
    },
}

impl LuaDefinitionId {
    pub fn file_id(&self) -> FileId {
        match self {
            Self::Declaration(decl_id) => decl_id.file_id,
            Self::Assignment { file_id, .. } => *file_id,
        }
    }

    pub fn stable_cmp(&self, other: &Self) -> Ordering {
        let variant_order = match (self, other) {
            (Self::Declaration(_), Self::Assignment { .. }) => return Ordering::Less,
            (Self::Assignment { .. }, Self::Declaration(_)) => return Ordering::Greater,
            _ => Ordering::Equal,
        };
        if variant_order != Ordering::Equal {
            return variant_order;
        }

        match (self, other) {
            (Self::Declaration(left), Self::Declaration(right)) => left
                .file_id
                .cmp(&right.file_id)
                .then_with(|| left.position.cmp(&right.position)),
            (
                Self::Assignment {
                    file_id: left_file_id,
                    assignment: left_assignment,
                    target_idx: left_target_idx,
                },
                Self::Assignment {
                    file_id: right_file_id,
                    assignment: right_assignment,
                    target_idx: right_target_idx,
                },
            ) => left_file_id
                .cmp(right_file_id)
                .then_with(|| syntax_id_cmp(left_assignment, right_assignment))
                .then_with(|| left_target_idx.cmp(right_target_idx)),
            _ => Ordering::Equal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LuaInferenceNodeId {
    TypeOwner(LuaTypeOwner),
    Definition(LuaDefinitionId),
    SignatureParam {
        signature_id: LuaSignatureId,
        param_idx: u16,
    },
}

impl LuaInferenceNodeId {
    pub fn file_id(&self) -> FileId {
        match self {
            Self::TypeOwner(owner) => owner.get_file_id(),
            Self::Definition(definition) => definition.file_id(),
            Self::SignatureParam { signature_id, .. } => signature_id.get_file_id(),
        }
    }

    pub fn stable_cmp(&self, other: &Self) -> Ordering {
        let tag = |node: &Self| match node {
            Self::TypeOwner(_) => 0,
            Self::Definition(_) => 1,
            Self::SignatureParam { .. } => 2,
        };

        tag(self)
            .cmp(&tag(other))
            .then_with(|| match (self, other) {
                (Self::TypeOwner(left), Self::TypeOwner(right)) => type_owner_cmp(left, right),
                (Self::Definition(left), Self::Definition(right)) => left.stable_cmp(right),
                (
                    Self::SignatureParam {
                        signature_id: left_signature_id,
                        param_idx: left_param_idx,
                    },
                    Self::SignatureParam {
                        signature_id: right_signature_id,
                        param_idx: right_param_idx,
                    },
                ) => left_signature_id
                    .get_file_id()
                    .cmp(&right_signature_id.get_file_id())
                    .then_with(|| {
                        left_signature_id
                            .get_position()
                            .cmp(&right_signature_id.get_position())
                    })
                    .then_with(|| left_param_idx.cmp(right_param_idx)),
                _ => Ordering::Equal,
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LuaInferenceEventId {
    pub node: LuaInferenceNodeId,
    pub kind: LuaInferenceProvenanceKind,
    pub source: InFiled<LuaSyntaxId>,
}

impl LuaInferenceEventId {
    pub fn stable_cmp(&self, other: &Self) -> Ordering {
        self.node
            .stable_cmp(&other.node)
            .then_with(|| self.kind.cmp(&other.kind))
            .then_with(|| self.source.file_id.cmp(&other.source.file_id))
            .then_with(|| syntax_id_cmp(&self.source.value, &other.source.value))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LuaInferenceStep {
    pub event: LuaInferenceEventId,
    pub support: Arc<[LuaInferenceNodeId]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaTypeFact {
    typ: LuaType,
    confidence: LuaInferenceConfidence,
    base_provenance_kind: Option<LuaInferenceProvenanceKind>,
    provenance: Arc<[LuaInferenceStep]>,
}

impl LuaTypeFact {
    pub fn unknown() -> Self {
        Self {
            typ: LuaType::Unknown,
            confidence: LuaInferenceConfidence::Unknown,
            base_provenance_kind: None,
            provenance: Arc::from([]),
        }
    }

    pub fn certain(typ: LuaType) -> Self {
        Self {
            typ,
            confidence: LuaInferenceConfidence::Certain,
            base_provenance_kind: Some(LuaInferenceProvenanceKind::ConcreteValue),
            provenance: Arc::from([]),
        }
    }

    pub fn new(
        typ: LuaType,
        confidence: LuaInferenceConfidence,
        provenance: Arc<[LuaInferenceStep]>,
    ) -> Self {
        Self::from_normalized_parts(typ, confidence, None, normalize_provenance(provenance))
    }

    pub fn typ(&self) -> &LuaType {
        &self.typ
    }

    pub fn confidence(&self) -> LuaInferenceConfidence {
        self.confidence
    }

    pub fn base_provenance_kind(&self) -> Option<LuaInferenceProvenanceKind> {
        self.base_provenance_kind
    }

    pub fn provenance(&self) -> &[LuaInferenceStep] {
        &self.provenance
    }

    pub fn with_runtime_type(&self, typ: LuaType) -> Self {
        Self {
            typ,
            confidence: self.confidence,
            base_provenance_kind: self.base_provenance_kind,
            provenance: self.provenance.clone(),
        }
    }

    pub fn diagnostic_events(&self) -> impl Iterator<Item = &LuaInferenceEventId> {
        self.provenance.iter().map(|step| &step.event)
    }

    pub(crate) fn from_normalized_parts(
        typ: LuaType,
        confidence: LuaInferenceConfidence,
        base_provenance_kind: Option<LuaInferenceProvenanceKind>,
        provenance: Arc<[LuaInferenceStep]>,
    ) -> Self {
        Self {
            typ,
            confidence,
            base_provenance_kind,
            provenance,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaTypeFactMetadata {
    pub confidence: LuaInferenceConfidence,
    pub base_provenance_kind: Option<LuaInferenceProvenanceKind>,
    pub provenance: Arc<[LuaInferenceStep]>,
}

impl LuaTypeFactMetadata {
    pub fn from_fact(fact: &LuaTypeFact) -> Self {
        Self {
            confidence: fact.confidence,
            base_provenance_kind: fact.base_provenance_kind,
            provenance: fact.provenance.clone(),
        }
    }

    pub fn normalized(&self) -> Self {
        Self {
            confidence: self.confidence,
            base_provenance_kind: self.base_provenance_kind,
            provenance: normalize_provenance(self.provenance.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaInferenceDiagnosticEvent {
    pub event: LuaInferenceEventId,
    pub fact: LuaTypeFact,
}

fn normalize_provenance(provenance: Arc<[LuaInferenceStep]>) -> Arc<[LuaInferenceStep]> {
    let mut seen_events = HashSet::with_capacity(provenance.len());
    let mut unique_steps = Vec::with_capacity(provenance.len());
    for step in provenance.iter() {
        if seen_events.insert(step.event.clone()) {
            unique_steps.push(step.clone());
        }
    }
    unique_steps.into()
}

fn type_owner_cmp(left: &LuaTypeOwner, right: &LuaTypeOwner) -> Ordering {
    let tag = |owner: &LuaTypeOwner| match owner {
        LuaTypeOwner::Decl(_) => 0,
        LuaTypeOwner::Member(_) => 1,
        LuaTypeOwner::SyntaxId(_) => 2,
    };

    tag(left)
        .cmp(&tag(right))
        .then_with(|| match (left, right) {
            (LuaTypeOwner::Decl(left), LuaTypeOwner::Decl(right)) => left
                .file_id
                .cmp(&right.file_id)
                .then_with(|| left.position.cmp(&right.position)),
            (LuaTypeOwner::Member(left), LuaTypeOwner::Member(right)) => left
                .file_id
                .cmp(&right.file_id)
                .then_with(|| syntax_id_cmp(left.get_syntax_id(), right.get_syntax_id())),
            (LuaTypeOwner::SyntaxId(left), LuaTypeOwner::SyntaxId(right)) => left
                .file_id
                .cmp(&right.file_id)
                .then_with(|| syntax_id_cmp(&left.value, &right.value)),
            _ => Ordering::Equal,
        })
}

fn syntax_id_cmp(left: &LuaSyntaxId, right: &LuaSyntaxId) -> Ordering {
    left.get_range()
        .start()
        .cmp(&right.get_range().start())
        .then_with(|| left.get_range().end().cmp(&right.get_range().end()))
        .then_with(|| (left.get_kind() as u16).cmp(&(right.get_kind() as u16)))
}
