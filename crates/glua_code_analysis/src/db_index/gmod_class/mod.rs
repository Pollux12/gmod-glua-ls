use crate::db_index::cow_shards::{CowCell, CowShards};
use crate::db_index::derivation::hash_local;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::hash::{Hash, Hasher};

use glua_parser::LuaSyntaxId;
use rowan::TextSize;
use smol_str::SmolStr;

use super::LuaIndex;
use crate::{
    FileId, LuaTypeDeclId,
    db_index::derivation::{self, FactKey, Fingerprint},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GmodScriptedClassCallKind {
    DefineBaseClass,
    DeriveGamemode,
    AccessorFunc,
    NetworkVar,
    NetworkVarElement,
    ScriptedEntRegister,
    VguiRegister,
    VguiRegisterFile,
    VguiRegisterTable,
    DermaDefineControl,
    DermaDefineSkin,
}

impl GmodScriptedClassCallKind {
    pub fn from_call_name(call_name: &str) -> Option<Self> {
        match call_name {
            "DEFINE_BASECLASS" => Some(Self::DefineBaseClass),
            "DeriveGamemode" => Some(Self::DeriveGamemode),
            "AccessorFunc" => Some(Self::AccessorFunc),
            "NetworkVar" => Some(Self::NetworkVar),
            "NetworkVarElement" => Some(Self::NetworkVarElement),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum GmodClassCallLiteral {
    String(String),
    Integer(i64),
    Unsigned(u64),
    Float(f64),
    Boolean(bool),
    Nil,
    NameRef(String),
}

impl std::hash::Hash for GmodClassCallLiteral {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::String(value) | Self::NameRef(value) => value.hash(state),
            Self::Integer(value) => value.hash(state),
            Self::Unsigned(value) => value.hash(state),
            Self::Float(value) => value.to_bits().hash(state),
            Self::Boolean(value) => value.hash(state),
            Self::Nil => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GmodClassCallArg {
    pub syntax_id: LuaSyntaxId,
    pub value: Option<GmodClassCallLiteral>,
}

impl Hash for GmodClassCallArg {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self { syntax_id, value } = self;
        hash_local(syntax_id, state);
        value.hash(state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GmodClassCallArgSource {
    pub arg_idx: usize,
    pub field_path: Vec<String>,
}

impl GmodClassCallArgSource {
    pub fn direct(arg_idx: usize) -> Self {
        Self {
            arg_idx,
            field_path: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GmodClassCallFieldArg {
    pub source: GmodClassCallArgSource,
    pub syntax_id: LuaSyntaxId,
    pub value: Option<GmodClassCallLiteral>,
}

impl Hash for GmodClassCallFieldArg {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self {
            source,
            syntax_id,
            value,
        } = self;
        source.hash(state);
        hash_local(syntax_id, state);
        value.hash(state);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GmodScriptedClassCallMetadata {
    pub syntax_id: LuaSyntaxId,
    pub literal_args: Vec<Option<GmodClassCallLiteral>>,
    pub args: Vec<GmodClassCallArg>,
    pub field_args: Vec<GmodClassCallFieldArg>,
    pub inheritance_roles: Option<GmodNamedStringCallRoles>,
    pub network_var_roles: Option<GmodNetworkVarCallRoles>,
    pub vgui_panel_roles: Option<GmodVguiPanelCallRoles>,
    pub derma_skin_roles: Option<GmodDermaSkinCallRoles>,
}

impl Hash for GmodScriptedClassCallMetadata {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self {
            syntax_id,
            literal_args,
            args,
            field_args,
            inheritance_roles,
            network_var_roles,
            vgui_panel_roles,
            derma_skin_roles,
        } = self;
        hash_local(syntax_id, state);
        literal_args.hash(state);
        args.hash(state);
        field_args.hash(state);
        inheritance_roles.hash(state);
        network_var_roles.hash(state);
        vgui_panel_roles.hash(state);
        derma_skin_roles.hash(state);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GmodNamedStringCallRoles {
    pub name_arg_idx: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GmodNetworkVarCallRoles {
    pub type_arg_idx: Option<usize>,
    pub name_arg_idx: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GmodVguiPanelCallRoles {
    pub define: GmodClassCallArgSource,
    pub table: Option<GmodClassCallArgSource>,
    pub base: Option<GmodClassCallArgSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GmodDermaSkinCallRoles {
    pub define_arg_idx: usize,
}

impl GmodScriptedClassCallMetadata {
    pub fn inheritance_name_arg_idx(&self) -> usize {
        self.inheritance_roles
            .map(|roles| roles.name_arg_idx)
            .unwrap_or(0)
    }

    pub fn network_var_type_arg_idx(&self) -> Option<usize> {
        self.network_var_roles
            .and_then(|roles| roles.type_arg_idx)
            .or(Some(0))
    }

    pub fn network_var_name_arg_idx(&self) -> Option<usize> {
        self.network_var_roles.map(|roles| roles.name_arg_idx)
    }

    pub fn vgui_panel_define_arg_idx(&self) -> usize {
        self.vgui_panel_roles
            .as_ref()
            .map(|roles| roles.define.arg_idx)
            .unwrap_or(0)
    }

    pub fn vgui_panel_table_arg_idx(&self, default_arg_idx: usize) -> usize {
        self.vgui_panel_roles
            .as_ref()
            .and_then(|roles| roles.table.as_ref().map(|source| source.arg_idx))
            .unwrap_or(default_arg_idx)
    }

    pub fn vgui_panel_base_arg_idx(&self, default_arg_idx: Option<usize>) -> Option<usize> {
        self.vgui_panel_roles
            .as_ref()
            .and_then(|roles| roles.base.as_ref().map(|source| source.arg_idx))
            .or(default_arg_idx)
    }

    pub fn vgui_panel_define_arg_source(&self) -> GmodClassCallArgSource {
        self.vgui_panel_roles
            .as_ref()
            .map(|roles| roles.define.clone())
            .unwrap_or_else(|| GmodClassCallArgSource::direct(0))
    }

    pub fn vgui_panel_table_arg_source(&self, default_arg_idx: usize) -> GmodClassCallArgSource {
        self.vgui_panel_roles
            .as_ref()
            .and_then(|roles| roles.table.clone())
            .unwrap_or_else(|| GmodClassCallArgSource::direct(default_arg_idx))
    }

    pub fn vgui_panel_base_arg_source(
        &self,
        default_arg_idx: Option<usize>,
    ) -> Option<GmodClassCallArgSource> {
        self.vgui_panel_roles
            .as_ref()
            .and_then(|roles| roles.base.clone())
            .or_else(|| default_arg_idx.map(GmodClassCallArgSource::direct))
    }

    pub fn value_for_arg_source(
        &self,
        source: &GmodClassCallArgSource,
    ) -> Option<&GmodClassCallLiteral> {
        if source.field_path.is_empty() {
            return self.args.get(source.arg_idx)?.value.as_ref();
        }

        self.field_args
            .iter()
            .find(|arg| &arg.source == source)
            .and_then(|arg| arg.value.as_ref())
    }

    pub fn derma_skin_define_arg_idx(&self) -> usize {
        self.derma_skin_roles
            .map(|roles| roles.define_arg_idx)
            .unwrap_or(0)
    }

    pub fn define_arg_range(&self, kind: GmodScriptedClassCallKind) -> rowan::TextRange {
        let arg_idx = match kind {
            GmodScriptedClassCallKind::DefineBaseClass
            | GmodScriptedClassCallKind::DeriveGamemode => self.inheritance_name_arg_idx(),
            GmodScriptedClassCallKind::NetworkVar
            | GmodScriptedClassCallKind::NetworkVarElement => {
                self.network_var_name_arg_idx().unwrap_or(0)
            }
            GmodScriptedClassCallKind::ScriptedEntRegister => 1,
            GmodScriptedClassCallKind::DermaDefineSkin => self.derma_skin_define_arg_idx(),
            _ => self.vgui_panel_define_arg_idx(),
        };
        self.args
            .get(arg_idx)
            .map(|arg| arg.syntax_id.get_range())
            .unwrap_or_else(|| self.syntax_id.get_range())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Hash)]
pub struct GmodScriptedClassFileMetadata {
    pub define_baseclass_calls: Vec<GmodScriptedClassCallMetadata>,
    pub derive_gamemode_calls: Vec<GmodScriptedClassCallMetadata>,
    pub accessor_func_calls: Vec<GmodScriptedClassCallMetadata>,
    pub network_var_calls: Vec<GmodScriptedClassCallMetadata>,
    pub network_var_element_calls: Vec<GmodScriptedClassCallMetadata>,
    pub scripted_ent_register_calls: Vec<GmodScriptedClassCallMetadata>,
    pub vgui_register_calls: Vec<GmodScriptedClassCallMetadata>,
    pub vgui_register_file_calls: Vec<GmodScriptedClassCallMetadata>,
    pub vgui_register_table_calls: Vec<GmodScriptedClassCallMetadata>,
    pub derma_define_control_calls: Vec<GmodScriptedClassCallMetadata>,
    pub derma_define_skin_calls: Vec<GmodScriptedClassCallMetadata>,
    pub vgui_parent_calls: Vec<GmodVguiParentCallMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GmodVguiParentSource {
    LiteralName(String),
    Expr(LuaSyntaxId),
    Receiver,
    ReceiverField(Vec<String>),
    Unknown,
}

impl Hash for GmodVguiParentSource {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::LiteralName(name) => name.hash(state),
            Self::Expr(syntax_id) => hash_local(syntax_id, state),
            Self::ReceiverField(fields) => fields.hash(state),
            Self::Receiver | Self::Unknown => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GmodVguiParentRelation {
    pub child_type_id: LuaTypeDeclId,
    pub parent_chain: Vec<LuaTypeDeclId>,
    pub parent_chain_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodVguiParentCallMetadata {
    pub syntax_id: LuaSyntaxId,
    pub child: GmodVguiParentSource,
    pub parent: GmodVguiParentSource,
}

impl Hash for GmodVguiParentCallMetadata {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self {
            syntax_id,
            child,
            parent,
        } = self;
        hash_local(syntax_id, state);
        child.hash(state);
        parent.hash(state);
    }
}

/// How a vgui parent call names its parent, resolved to type ids.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum GmodVguiParentSourceResolution {
    Direct(Vec<LuaTypeDeclId>),
    AssignedField {
        field_type_ids: Vec<LuaTypeDeclId>,
        assignment_parent_type_ids: Vec<LuaTypeDeclId>,
    },
    ReceiverField {
        field_type_ids: Vec<LuaTypeDeclId>,
        receiver_type_ids: Vec<LuaTypeDeclId>,
        receiver_field_parent_type_ids: Option<Vec<LuaTypeDeclId>>,
    },
}

/// One vgui parent call with its child and parent resolved to type ids.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GmodVguiResolvedParentCall {
    pub child_type_ids: Vec<LuaTypeDeclId>,
    pub parent: GmodVguiParentSourceResolution,
}

/// What one file's own vgui parent calls resolve to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct GmodVguiFileRelations {
    /// The forwarding parents the file's calls propose, sorted by owner and
    /// method: `None` where two of its calls disagree.
    pub forwarding_candidates: Vec<((LuaTypeDeclId, String), Option<Vec<LuaTypeDeclId>>)>,
    /// The file's calls resolved, in source order.
    pub calls: Vec<GmodVguiResolvedParentCall>,
}

impl GmodScriptedClassFileMetadata {
    fn calls_by_kind_mut(
        &mut self,
        kind: GmodScriptedClassCallKind,
    ) -> &mut Vec<GmodScriptedClassCallMetadata> {
        match kind {
            GmodScriptedClassCallKind::DefineBaseClass => &mut self.define_baseclass_calls,
            GmodScriptedClassCallKind::DeriveGamemode => &mut self.derive_gamemode_calls,
            GmodScriptedClassCallKind::AccessorFunc => &mut self.accessor_func_calls,
            GmodScriptedClassCallKind::NetworkVar => &mut self.network_var_calls,
            GmodScriptedClassCallKind::NetworkVarElement => &mut self.network_var_element_calls,
            GmodScriptedClassCallKind::ScriptedEntRegister => &mut self.scripted_ent_register_calls,
            GmodScriptedClassCallKind::VguiRegister => &mut self.vgui_register_calls,
            GmodScriptedClassCallKind::VguiRegisterFile => &mut self.vgui_register_file_calls,
            GmodScriptedClassCallKind::VguiRegisterTable => &mut self.vgui_register_table_calls,
            GmodScriptedClassCallKind::DermaDefineControl => &mut self.derma_define_control_calls,
            GmodScriptedClassCallKind::DermaDefineSkin => &mut self.derma_define_skin_calls,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct GmodClassMetadataIndex {
    file_metadata: CowShards<FileId, GmodScriptedClassFileMetadata>,
    /// The set of files whose metadata has vgui parent calls.
    vgui_parent_call_files: derivation::SetFingerprint,
    vgui_panels: CowShards<String, Vec<VguiPanelDefinition>>,
    derma_skins: CowShards<String, Vec<DermaSkinDefinition>>,
    vgui_panel_parent_chains: CowCell<HashMap<LuaTypeDeclId, Vec<LuaTypeDeclId>>>,
    incomplete_vgui_panel_parent_chains: CowCell<HashSet<LuaTypeDeclId>>,
    /// Incremental multiset fingerprint, per panel/skin name, of the
    /// registration calls a name-keyed read can observe.
    meta_fingerprints: CowShards<SmolStr, Fingerprint>,
    /// What each file's own vgui parent calls resolve to.
    vgui_file_relations: CowShards<FileId, std::sync::Arc<GmodVguiFileRelations>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct VguiPanelDefinition {
    file_id: FileId,
    range_start: TextSize,
    base_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DermaSkinDefinition {
    file_id: FileId,
    range_start: TextSize,
}

impl GmodClassMetadataIndex {
    pub fn new() -> Self {
        Self {
            file_metadata: Default::default(),
            vgui_parent_call_files: derivation::SetFingerprint::default(),
            vgui_panels: Default::default(),
            derma_skins: Default::default(),
            vgui_panel_parent_chains: Default::default(),
            incomplete_vgui_panel_parent_chains: Default::default(),
            meta_fingerprints: Default::default(),
            vgui_file_relations: Default::default(),
        }
    }

    /// The names under which name-keyed reads observe `call`: the literal
    /// define argument (`find_*_definitions`) and the cached panel/skin name.
    fn meta_names(
        kind: GmodScriptedClassCallKind,
        call: &GmodScriptedClassCallMetadata,
    ) -> Vec<SmolStr> {
        let (define_arg_index, cached_name) = match kind {
            GmodScriptedClassCallKind::VguiRegister
            | GmodScriptedClassCallKind::DermaDefineControl => (
                call.vgui_panel_define_arg_idx(),
                Self::maybe_extract_vgui_panel(kind, call).map(|(name, _)| name),
            ),
            GmodScriptedClassCallKind::DermaDefineSkin => (
                call.derma_skin_define_arg_idx(),
                Self::extract_non_empty_string_arg(call, call.derma_skin_define_arg_idx()),
            ),
            _ => return Vec::new(),
        };
        let mut names = Vec::new();
        if let Some(Some(GmodClassCallLiteral::String(name))) =
            call.literal_args.get(define_arg_index)
        {
            names.push(SmolStr::new(name));
        }
        if let Some(name) = cached_name
            && !names.iter().any(|existing| existing.as_str() == name)
        {
            names.push(SmolStr::new(name));
        }
        names
    }

    fn meta_fingerprint(
        kind: GmodScriptedClassCallKind,
        file_id: FileId,
        call: &GmodScriptedClassCallMetadata,
    ) -> Fingerprint {
        derivation::in_file(Some(file_id), || {
            derivation::fingerprint_of(&(kind, file_id, call))
        })
    }

    /// `name`'s meta fingerprint summed afresh from every file's calls, which
    /// is what the incremental `meta_fingerprints` sum holds.
    fn recomputed_meta_fingerprint(&self, name: &SmolStr) -> Fingerprint {
        let mut sum: Fingerprint = 0;
        for (file_id, metadata) in &self.file_metadata {
            for (kind, calls) in [
                (
                    GmodScriptedClassCallKind::VguiRegister,
                    &metadata.vgui_register_calls,
                ),
                (
                    GmodScriptedClassCallKind::DermaDefineControl,
                    &metadata.derma_define_control_calls,
                ),
                (
                    GmodScriptedClassCallKind::DermaDefineSkin,
                    &metadata.derma_define_skin_calls,
                ),
            ] {
                for call in calls {
                    if Self::meta_names(kind, call).contains(name) {
                        sum = sum.wrapping_add(Self::meta_fingerprint(kind, *file_id, call));
                    }
                }
            }
        }
        sum
    }

    /// Adds (`add`) or removes `call`'s contribution to the name-keyed
    /// fingerprints.
    fn update_meta_fingerprints(
        meta_fingerprints: &mut CowShards<SmolStr, Fingerprint>,
        file_id: FileId,
        kind: GmodScriptedClassCallKind,
        call: &GmodScriptedClassCallMetadata,
        add: bool,
    ) {
        let names = Self::meta_names(kind, call);
        if names.is_empty() {
            return;
        }
        let fingerprint = Self::meta_fingerprint(kind, file_id, call);
        for name in names {
            derivation::note_write(|| FactKey::GmodMeta(name.clone()));
            let previous = meta_fingerprints.get(&name).copied().unwrap_or_default();
            let value = if add {
                previous.wrapping_add(fingerprint)
            } else {
                previous.wrapping_sub(fingerprint)
            };
            if value == 0 {
                meta_fingerprints.remove(&name);
            } else {
                meta_fingerprints.insert(name, value);
            }
        }
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::GmodMeta(name) if derivation::mapping_back() => {
                self.recomputed_meta_fingerprint(name)
            }
            FactKey::GmodMeta(name) => self
                .meta_fingerprints
                .get(name)
                .copied()
                .unwrap_or(derivation::ABSENT),
            FactKey::GmodClassFile(file_id) => {
                derivation::fingerprint_opt(self.file_metadata.get(file_id))
            }
            FactKey::GmodVguiRegistrations(file_id) => {
                let mut calls = self.vgui_registrations(file_id).peekable();
                if calls.peek().is_none() {
                    return derivation::ABSENT;
                }
                derivation::fingerprint_of(&calls.collect::<Vec<_>>())
            }
            FactKey::VguiParentCallFiles => self.vgui_parent_call_files.get(),
            FactKey::VguiRelations(file_id) => {
                derivation::fingerprint_opt(self.vgui_file_relations.get(file_id))
            }
            FactKey::VguiChain(type_id) => {
                let chain = self.vgui_panel_parent_chains.get(type_id);
                let incomplete = self.incomplete_vgui_panel_parent_chains.contains(type_id);
                if chain.is_none() && !incomplete {
                    derivation::ABSENT
                } else {
                    derivation::fingerprint_of(&(chain, incomplete))
                }
            }
            _ => {
                debug_assert!(false, "{key:?} is not a gmod class fact");
                derivation::ABSENT
            }
        }
    }

    fn extract_non_empty_string_literal(literal: &GmodClassCallLiteral) -> Option<String> {
        match literal {
            GmodClassCallLiteral::String(value) if !value.is_empty() => Some(value.clone()),
            _ => None,
        }
    }

    fn extract_non_empty_string_arg(
        call_metadata: &GmodScriptedClassCallMetadata,
        arg_index: usize,
    ) -> Option<String> {
        call_metadata
            .args
            .get(arg_index)
            .and_then(|arg| arg.value.as_ref())
            .and_then(Self::extract_non_empty_string_literal)
    }

    fn extract_non_empty_string_arg_source(
        call_metadata: &GmodScriptedClassCallMetadata,
        source: &GmodClassCallArgSource,
    ) -> Option<String> {
        call_metadata
            .value_for_arg_source(source)
            .and_then(Self::extract_non_empty_string_literal)
    }

    fn maybe_extract_vgui_panel(
        kind: GmodScriptedClassCallKind,
        call_metadata: &GmodScriptedClassCallMetadata,
    ) -> Option<(String, Option<String>)> {
        let default_base_arg_index = match kind {
            GmodScriptedClassCallKind::VguiRegister => Some(2),
            GmodScriptedClassCallKind::DermaDefineControl => Some(3),
            _ => None,
        };

        let define_arg_index = call_metadata.vgui_panel_define_arg_idx();
        let base_arg_source = call_metadata.vgui_panel_base_arg_source(default_base_arg_index);

        let panel_name = Self::extract_non_empty_string_arg(call_metadata, define_arg_index)?;
        let base_name = base_arg_source
            .as_ref()
            .and_then(|source| Self::extract_non_empty_string_arg_source(call_metadata, source));
        Some((panel_name, base_name))
    }

    fn insert_vgui_panel_from_call(
        vgui_panels: &mut CowShards<String, Vec<VguiPanelDefinition>>,
        file_id: FileId,
        kind: GmodScriptedClassCallKind,
        call_metadata: &GmodScriptedClassCallMetadata,
    ) {
        let Some((panel_name, base_name)) = Self::maybe_extract_vgui_panel(kind, call_metadata)
        else {
            return;
        };

        vgui_panels
            .entry(panel_name)
            .or_default()
            .push(VguiPanelDefinition {
                file_id,
                range_start: call_metadata.syntax_id.get_range().start(),
                base_name,
            });
    }

    fn update_vgui_panels_from_call(
        &mut self,
        file_id: FileId,
        kind: GmodScriptedClassCallKind,
        call_metadata: &GmodScriptedClassCallMetadata,
    ) {
        if matches!(
            kind,
            GmodScriptedClassCallKind::VguiRegister | GmodScriptedClassCallKind::DermaDefineControl
        ) {
            Self::insert_vgui_panel_from_call(&mut self.vgui_panels, file_id, kind, call_metadata);
        }
    }

    fn insert_derma_skin_from_call(
        derma_skins: &mut CowShards<String, Vec<DermaSkinDefinition>>,
        file_id: FileId,
        call_metadata: &GmodScriptedClassCallMetadata,
    ) {
        let Some(skin_name) = Self::extract_non_empty_string_arg(
            call_metadata,
            call_metadata.derma_skin_define_arg_idx(),
        ) else {
            return;
        };

        derma_skins
            .entry(skin_name)
            .or_default()
            .push(DermaSkinDefinition {
                file_id,
                range_start: call_metadata.syntax_id.get_range().start(),
            });
    }

    fn update_derma_skins_from_call(
        &mut self,
        file_id: FileId,
        kind: GmodScriptedClassCallKind,
        call_metadata: &GmodScriptedClassCallMetadata,
    ) {
        if kind == GmodScriptedClassCallKind::DermaDefineSkin {
            Self::insert_derma_skin_from_call(&mut self.derma_skins, file_id, call_metadata);
        }
    }

    /// Incrementally refresh the panel/skin caches for a single re-added call
    /// site. Both the old and new metadata share the same syntax range (they
    /// describe the same call), so the previous contribution is located by
    /// `(file_id, range_start)` and removed before the new one is inserted.
    fn replace_cached_call(
        &mut self,
        file_id: FileId,
        kind: GmodScriptedClassCallKind,
        old_metadata: &GmodScriptedClassCallMetadata,
        new_metadata: &GmodScriptedClassCallMetadata,
    ) {
        let range_start = old_metadata.syntax_id.get_range().start();
        match kind {
            GmodScriptedClassCallKind::VguiRegister
            | GmodScriptedClassCallKind::DermaDefineControl => {
                if let Some((panel_name, _)) = Self::maybe_extract_vgui_panel(kind, old_metadata) {
                    Self::remove_cached_entry(
                        &mut self.vgui_panels,
                        &panel_name,
                        file_id,
                        range_start,
                        |definition| (definition.file_id, definition.range_start),
                    );
                }
                self.update_vgui_panels_from_call(file_id, kind, new_metadata);
            }
            GmodScriptedClassCallKind::DermaDefineSkin => {
                if let Some(skin_name) = Self::extract_non_empty_string_arg(
                    old_metadata,
                    old_metadata.derma_skin_define_arg_idx(),
                ) {
                    Self::remove_cached_entry(
                        &mut self.derma_skins,
                        &skin_name,
                        file_id,
                        range_start,
                        |definition| (definition.file_id, definition.range_start),
                    );
                }
                self.update_derma_skins_from_call(file_id, kind, new_metadata);
            }
            _ => {}
        }
    }

    /// Remove the cache entry for a single call site from its name bucket,
    /// matching on `(file_id, range_start)`. Touches only the one bucket, so
    /// the cost is bounded by the number of definitions sharing the name
    /// rather than the whole cache.
    fn remove_cached_entry<T: Clone>(
        cache: &mut CowShards<String, Vec<T>>,
        name: &str,
        file_id: FileId,
        range_start: TextSize,
        key: impl Fn(&T) -> (FileId, TextSize),
    ) {
        if let Some(definitions) = cache.get_mut(name) {
            definitions.retain(|definition| key(definition) != (file_id, range_start));
            if definitions.is_empty() {
                cache.remove(name);
            }
        }
    }

    fn recompute_derived_caches(&mut self) {
        let mut vgui_panels = CowShards::default();
        let mut derma_skins = CowShards::default();

        for (file_id, file_metadata) in &self.file_metadata {
            for call in &file_metadata.vgui_register_calls {
                Self::insert_vgui_panel_from_call(
                    &mut vgui_panels,
                    *file_id,
                    GmodScriptedClassCallKind::VguiRegister,
                    call,
                );
            }
            for call in &file_metadata.derma_define_control_calls {
                Self::insert_vgui_panel_from_call(
                    &mut vgui_panels,
                    *file_id,
                    GmodScriptedClassCallKind::DermaDefineControl,
                    call,
                );
            }
            for call in &file_metadata.derma_define_skin_calls {
                Self::insert_derma_skin_from_call(&mut derma_skins, *file_id, call);
            }
        }

        self.vgui_panels = vgui_panels;
        self.derma_skins = derma_skins;
    }

    pub fn add_call(
        &mut self,
        file_id: FileId,
        kind: GmodScriptedClassCallKind,
        call_metadata: GmodScriptedClassCallMetadata,
    ) {
        derivation::note_write(|| FactKey::GmodClassFile(file_id));
        {
            let calls = self
                .file_metadata
                .entry(file_id)
                .or_default()
                .calls_by_kind_mut(kind);
            if let Some(existing) = calls
                .iter_mut()
                .find(|existing| existing.syntax_id == call_metadata.syntax_id)
            {
                // Re-adding the same call site (the pre- and post-analysis
                // passes both visit every file). Overwrite the stored metadata
                // in place and refresh only this call's cache contribution
                // instead of rebuilding every file's panels/skins, which would
                // make a full analysis O(calls * total_calls).
                let old_metadata = std::mem::replace(existing, call_metadata.clone());
                Self::update_meta_fingerprints(
                    &mut self.meta_fingerprints,
                    file_id,
                    kind,
                    &old_metadata,
                    false,
                );
                Self::update_meta_fingerprints(
                    &mut self.meta_fingerprints,
                    file_id,
                    kind,
                    &call_metadata,
                    true,
                );
                self.replace_cached_call(file_id, kind, &old_metadata, &call_metadata);
                return;
            }
        }

        self.update_vgui_panels_from_call(file_id, kind, &call_metadata);
        self.update_derma_skins_from_call(file_id, kind, &call_metadata);
        Self::update_meta_fingerprints(
            &mut self.meta_fingerprints,
            file_id,
            kind,
            &call_metadata,
            true,
        );
        self.file_metadata
            .entry(file_id)
            .or_default()
            .calls_by_kind_mut(kind)
            .push(call_metadata);
    }

    pub fn add_vgui_parent_call(&mut self, file_id: FileId, call: GmodVguiParentCallMetadata) {
        derivation::note_write(|| FactKey::GmodClassFile(file_id));
        let calls = &mut self
            .file_metadata
            .entry(file_id)
            .or_default()
            .vgui_parent_calls;
        if calls.is_empty() {
            self.vgui_parent_call_files.insert(&file_id);
            derivation::note_write(|| FactKey::VguiParentCallFiles);
        }
        if let Some(existing) = calls
            .iter_mut()
            .find(|existing| existing.syntax_id == call.syntax_id)
        {
            *existing = call;
        } else {
            // Kept in source order: calls arrive in whatever order the scan
            // and the forwarding passes produce them, which differs between a
            // cold build and a re-index of the same file.
            let insert_at = calls.partition_point(|existing| {
                (
                    existing.syntax_id.get_range().start(),
                    existing.syntax_id.get_range().end(),
                ) < (
                    call.syntax_id.get_range().start(),
                    call.syntax_id.get_range().end(),
                )
            });
            calls.insert(insert_at, call);
        }
    }

    pub fn has_annotated_vgui_parent_calls(&self, file_id: FileId) -> bool {
        derivation::note_read(|| FactKey::GmodClassFile(file_id));
        self.file_metadata
            .get(&file_id)
            .is_some_and(|metadata| !metadata.vgui_parent_calls.is_empty())
    }

    pub fn get_vgui_parent_calls(&self, file_id: &FileId) -> &[GmodVguiParentCallMetadata] {
        derivation::note_read(|| FactKey::GmodClassFile(*file_id));
        self.file_metadata
            .get(file_id)
            .map(|metadata| metadata.vgui_parent_calls.as_slice())
            .unwrap_or_default()
    }

    pub fn get_vgui_panel_parent_chain(
        &self,
        child_type_id: &LuaTypeDeclId,
    ) -> Option<&[LuaTypeDeclId]> {
        derivation::note_read(|| FactKey::VguiChain(child_type_id.clone()));
        self.vgui_panel_parent_chains
            .get(child_type_id)
            .map(Vec::as_slice)
    }

    pub fn vgui_panel_parent_chain_is_complete(&self, child_type_id: &LuaTypeDeclId) -> bool {
        derivation::note_read(|| FactKey::VguiChain(child_type_id.clone()));
        !self
            .incomplete_vgui_panel_parent_chains
            .contains(child_type_id)
    }

    /// Stores what `file_id`'s own vgui parent calls resolve to.
    pub fn set_vgui_file_relations(&mut self, file_id: FileId, relations: GmodVguiFileRelations) {
        derivation::note_write(|| FactKey::VguiRelations(file_id));
        self.vgui_file_relations
            .insert(file_id, std::sync::Arc::new(relations));
    }

    /// What `file_id`'s own vgui parent calls resolve to.
    pub fn get_vgui_file_relations(&self, file_id: FileId) -> Option<&GmodVguiFileRelations> {
        derivation::note_read(|| FactKey::VguiRelations(file_id));
        self.vgui_file_relations
            .get(&file_id)
            .map(|relations| &**relations)
    }

    /// Drops what `file_id`'s own vgui parent calls resolve to.
    pub fn remove_vgui_file_relations(&mut self, file_id: FileId) {
        if self.vgui_file_relations.remove(&file_id).is_some() {
            derivation::note_write(|| FactKey::VguiRelations(file_id));
        }
    }

    /// Replaces the parent chains with those `relations` agree on. A child
    /// with an incomplete relation or two disagreeing chains has none.
    pub fn set_vgui_parent_relations(&mut self, relations: &[GmodVguiParentRelation]) {
        let mut parent_chains = HashMap::<LuaTypeDeclId, Vec<LuaTypeDeclId>>::default();
        let mut incomplete = HashSet::default();
        {
            {
                for relation in relations {
                    if !relation.parent_chain_complete || relation.parent_chain.is_empty() {
                        incomplete.insert(relation.child_type_id.clone());
                        continue;
                    }
                    match parent_chains.get(&relation.child_type_id) {
                        Some(existing) if existing != &relation.parent_chain => {
                            incomplete.insert(relation.child_type_id.clone());
                        }
                        Some(_) => {}
                        None => {
                            parent_chains.insert(
                                relation.child_type_id.clone(),
                                relation.parent_chain.clone(),
                            );
                        }
                    }
                }
            }
        }
        for type_id in &incomplete {
            parent_chains.remove(type_id);
        }
        if *self.vgui_panel_parent_chains != parent_chains
            || *self.incomplete_vgui_panel_parent_chains != incomplete
        {
            // Stamp every child whose chain or completeness could have moved —
            // old and new keys both, so removed and gained chains stale their
            // readers alike.
            for type_id in self
                .vgui_panel_parent_chains
                .keys()
                .chain(parent_chains.keys())
                .chain(self.incomplete_vgui_panel_parent_chains.iter())
                .chain(incomplete.iter())
            {
                derivation::note_write(|| FactKey::VguiChain(type_id.clone()));
            }
        }
        self.vgui_panel_parent_chains = parent_chains.into();
        self.incomplete_vgui_panel_parent_chains = incomplete.into();
    }

    /// The files whose removal is still holding their children's chains
    /// incomplete.
    pub fn get_file_metadata(&self, file_id: &FileId) -> Option<&GmodScriptedClassFileMetadata> {
        derivation::note_read(|| FactKey::GmodClassFile(*file_id));
        self.file_metadata.get(file_id)
    }

    /// A file's `derma.DefineControl` calls, then its `vgui.Register` calls.
    pub fn get_vgui_registrations(
        &self,
        file_id: &FileId,
    ) -> impl Iterator<Item = &GmodScriptedClassCallMetadata> {
        derivation::note_read(|| FactKey::GmodVguiRegistrations(*file_id));
        self.vgui_registrations(file_id)
    }

    fn vgui_registrations(
        &self,
        file_id: &FileId,
    ) -> impl Iterator<Item = &GmodScriptedClassCallMetadata> {
        self.file_metadata
            .get(file_id)
            .into_iter()
            .flat_map(|metadata| {
                metadata
                    .derma_define_control_calls
                    .iter()
                    .chain(&metadata.vgui_register_calls)
            })
    }

    /// Every file's metadata, unordered and unrecorded: for editor requests,
    /// not analysis.
    pub fn iter_file_metadata(
        &self,
    ) -> impl Iterator<Item = (&FileId, &GmodScriptedClassFileMetadata)> {
        self.file_metadata.iter()
    }

    /// The files that have vgui parent calls, sorted.
    pub fn vgui_parent_call_files(&self) -> Vec<FileId> {
        derivation::note_read(|| FactKey::VguiParentCallFiles);
        let mut file_ids = self
            .file_metadata
            .iter()
            .filter(|(_, metadata)| !metadata.vgui_parent_calls.is_empty())
            .map(|(file_id, _)| *file_id)
            .collect::<Vec<_>>();
        file_ids.sort_unstable();
        file_ids
    }

    pub fn find_vgui_panel_definitions(
        &self,
        name: &str,
    ) -> Vec<(FileId, &GmodScriptedClassCallMetadata)> {
        derivation::note_read(|| FactKey::GmodMeta(SmolStr::new(name)));
        if name.trim().is_empty() {
            return Vec::new();
        }

        let mut definitions = Vec::new();
        for (file_id, file_metadata) in &self.file_metadata {
            for call in file_metadata
                .vgui_register_calls
                .iter()
                .chain(file_metadata.derma_define_control_calls.iter())
            {
                let define_arg_index = call.vgui_panel_define_arg_idx();
                let Some(Some(GmodClassCallLiteral::String(panel_name))) =
                    call.literal_args.get(define_arg_index)
                else {
                    continue;
                };

                if panel_name == name {
                    definitions.push((*file_id, call));
                }
            }
        }

        definitions.sort_by_key(|(file_id, call)| (file_id.id, call.syntax_id.get_range().start()));
        definitions
    }

    pub fn find_derma_skin_definitions(
        &self,
        name: &str,
    ) -> Vec<(FileId, &GmodScriptedClassCallMetadata)> {
        derivation::note_read(|| FactKey::GmodMeta(SmolStr::new(name)));
        if name.trim().is_empty() {
            return Vec::new();
        }

        let mut definitions = Vec::new();
        for (file_id, file_metadata) in &self.file_metadata {
            for call in &file_metadata.derma_define_skin_calls {
                let define_arg_index = call.derma_skin_define_arg_idx();
                let Some(Some(GmodClassCallLiteral::String(skin_name))) =
                    call.literal_args.get(define_arg_index)
                else {
                    continue;
                };

                if skin_name == name {
                    definitions.push((*file_id, call));
                }
            }
        }

        definitions.sort_by_key(|(file_id, call)| (file_id.id, call.syntax_id.get_range().start()));
        definitions
    }

    pub fn get_vgui_panel_base(&self, name: &str) -> Option<Option<String>> {
        derivation::note_read(|| FactKey::GmodMeta(SmolStr::new(name)));
        self.vgui_panels.get(name).map(|definitions| {
            definitions
                .iter()
                .min_by_key(|definition| (definition.file_id.id, definition.range_start))
                .and_then(|definition| definition.base_name.clone())
        })
    }
}

impl LuaIndex for GmodClassMetadataIndex {
    fn remove(&mut self, file_id: FileId) {
        self.remove_files(std::slice::from_ref(&file_id));
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        for &file_id in file_ids {
            self.remove_vgui_file_relations(file_id);
        }
        self.remove_file_metadata(file_ids);
    }

    fn clear(&mut self) {
        for file_id in self.vgui_file_relations.keys() {
            derivation::note_write(|| FactKey::VguiRelations(*file_id));
        }
        self.vgui_file_relations.clear();
        self.clear_metadata();
    }
}

impl GmodClassMetadataIndex {
    /// Drops the call metadata of `file_ids`, which their call-site passes
    /// write; what their vgui parent calls resolve to stays.
    pub fn remove_file_metadata(&mut self, file_ids: &[FileId]) {
        for &file_id in file_ids {
            let Some(metadata) = self.file_metadata.remove(&file_id) else {
                continue;
            };
            derivation::note_write(|| FactKey::GmodClassFile(file_id));
            if !metadata.vgui_parent_calls.is_empty() {
                self.vgui_parent_call_files.remove(&file_id);
                derivation::note_write(|| FactKey::VguiParentCallFiles);
            }
            for (kind, calls) in [
                (
                    GmodScriptedClassCallKind::VguiRegister,
                    &metadata.vgui_register_calls,
                ),
                (
                    GmodScriptedClassCallKind::DermaDefineControl,
                    &metadata.derma_define_control_calls,
                ),
                (
                    GmodScriptedClassCallKind::DermaDefineSkin,
                    &metadata.derma_define_skin_calls,
                ),
            ] {
                for call in calls {
                    Self::update_meta_fingerprints(
                        &mut self.meta_fingerprints,
                        file_id,
                        kind,
                        call,
                        false,
                    );
                }
            }
        }
        self.recompute_derived_caches();
    }

    fn clear_metadata(&mut self) {
        for file_id in self.file_metadata.keys() {
            derivation::note_write(|| FactKey::GmodClassFile(*file_id));
        }
        derivation::note_write(|| FactKey::VguiParentCallFiles);
        for type_id in self
            .vgui_panel_parent_chains
            .keys()
            .chain(self.incomplete_vgui_panel_parent_chains.iter())
        {
            derivation::note_write(|| FactKey::VguiChain(type_id.clone()));
        }
        for name in self.meta_fingerprints.keys() {
            derivation::note_write(|| FactKey::GmodMeta(name.clone()));
        }
        self.meta_fingerprints.clear();
        self.file_metadata.clear();
        self.vgui_parent_call_files = derivation::SetFingerprint::default();
        self.vgui_panels.clear();
        self.derma_skins.clear();
        self.vgui_panel_parent_chains.clear();
        self.incomplete_vgui_panel_parent_chains.clear();
    }
}

#[cfg(test)]
mod tests {
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    use super::LuaIndex;
    use super::{
        GmodClassCallArg, GmodClassCallLiteral, GmodClassMetadataIndex, GmodScriptedClassCallKind,
        GmodScriptedClassCallMetadata,
    };
    use crate::FileId;

    fn range(start: u32) -> TextRange {
        TextRange::new(TextSize::new(start), TextSize::new(start + 1))
    }

    fn arg(start: u32, value: Option<GmodClassCallLiteral>) -> GmodClassCallArg {
        GmodClassCallArg {
            syntax_id: LuaSyntaxId::new(LuaSyntaxKind::NameExpr.into(), range(start)),
            value,
        }
    }

    fn vgui_register_call(panel: &str, base: &str, start: u32) -> GmodScriptedClassCallMetadata {
        GmodScriptedClassCallMetadata {
            syntax_id: LuaSyntaxId::new(LuaSyntaxKind::CallExpr.into(), range(start)),
            literal_args: vec![
                Some(GmodClassCallLiteral::String(panel.to_string())),
                None,
                Some(GmodClassCallLiteral::String(base.to_string())),
            ],
            args: vec![
                arg(
                    start + 1,
                    Some(GmodClassCallLiteral::String(panel.to_string())),
                ),
                arg(start + 2, None),
                arg(
                    start + 3,
                    Some(GmodClassCallLiteral::String(base.to_string())),
                ),
            ],
            field_args: Vec::new(),
            inheritance_roles: None,
            network_var_roles: None,
            vgui_panel_roles: None,
            derma_skin_roles: None,
        }
    }

    #[test]
    fn duplicate_vgui_panel_base_prefers_deterministic_file_order() {
        let mut index = GmodClassMetadataIndex::new();
        let canonical_file = FileId::new(1);
        let shadow_file = FileId::new(2);

        index.add_call(
            canonical_file,
            GmodScriptedClassCallKind::VguiRegister,
            vgui_register_call("DeterministicPanel", "DFrame", 10),
        );
        index.add_call(
            shadow_file,
            GmodScriptedClassCallKind::VguiRegister,
            vgui_register_call("DeterministicPanel", "EditablePanel", 20),
        );

        assert_eq!(
            index.get_vgui_panel_base("DeterministicPanel"),
            Some(Some("DFrame".to_string()))
        );
    }

    #[test]
    fn re_adding_same_call_site_keeps_panel_cache_stable() {
        // The pre- and post-analysis passes both add every call, so the second
        // add of the same syntax range must update in place rather than
        // accumulate a duplicate cache entry.
        let mut index = GmodClassMetadataIndex::new();
        let file = FileId::new(1);

        index.add_call(
            file,
            GmodScriptedClassCallKind::VguiRegister,
            vgui_register_call("Panel", "DFrame", 10),
        );
        index.add_call(
            file,
            GmodScriptedClassCallKind::VguiRegister,
            vgui_register_call("Panel", "DFrame", 10),
        );

        assert_eq!(
            index.get_vgui_panel_base("Panel"),
            Some(Some("DFrame".to_string()))
        );
        assert_eq!(index.find_vgui_panel_definitions("Panel").len(), 1);
    }

    #[test]
    fn re_adding_call_site_with_changed_name_moves_cache_bucket() {
        // If the post pass resolves a different panel name for the same call
        // site, the stale bucket must be vacated so lookups for the old name
        // no longer match.
        let mut index = GmodClassMetadataIndex::new();
        let file = FileId::new(1);

        index.add_call(
            file,
            GmodScriptedClassCallKind::VguiRegister,
            vgui_register_call("OldPanel", "DFrame", 10),
        );
        index.add_call(
            file,
            GmodScriptedClassCallKind::VguiRegister,
            vgui_register_call("NewPanel", "EditablePanel", 10),
        );

        assert_eq!(index.get_vgui_panel_base("OldPanel"), None);
        assert_eq!(
            index.get_vgui_panel_base("NewPanel"),
            Some(Some("EditablePanel".to_string()))
        );
    }

    #[test]
    fn batch_removal_matches_rebuilding_with_surviving_file_metadata() {
        let removed_first = FileId::new(1);
        let surviving = FileId::new(2);
        let removed_last = FileId::new(3);

        let mut index = GmodClassMetadataIndex::new();
        index.add_call(
            removed_first,
            GmodScriptedClassCallKind::VguiRegister,
            vgui_register_call("RemovedFirst", "DFrame", 10),
        );
        index.add_call(
            surviving,
            GmodScriptedClassCallKind::VguiRegister,
            vgui_register_call("Surviving", "EditablePanel", 20),
        );
        index.add_call(
            removed_last,
            GmodScriptedClassCallKind::VguiRegister,
            vgui_register_call("RemovedLast", "DPanel", 30),
        );
        index.add_call(
            removed_first,
            GmodScriptedClassCallKind::DermaDefineSkin,
            vgui_register_call("RemovedSkin", "", 40),
        );
        index.add_call(
            surviving,
            GmodScriptedClassCallKind::DermaDefineSkin,
            vgui_register_call("SurvivingSkin", "", 50),
        );

        index.remove_files(&[removed_last, removed_first]);

        let mut expected = GmodClassMetadataIndex::new();
        expected.add_call(
            surviving,
            GmodScriptedClassCallKind::VguiRegister,
            vgui_register_call("Surviving", "EditablePanel", 20),
        );
        expected.add_call(
            surviving,
            GmodScriptedClassCallKind::DermaDefineSkin,
            vgui_register_call("SurvivingSkin", "", 50),
        );

        assert_eq!(index.file_metadata, expected.file_metadata);
        assert_eq!(index.vgui_panels, expected.vgui_panels);
        assert_eq!(index.derma_skins, expected.derma_skins);
    }

    /// Readers of a file's panel registrations record those, read out of the
    /// file's class calls: another kind of call landing holds them, and they
    /// move with their own calls.
    #[test]
    fn a_class_call_of_another_kind_holds_readers_of_the_registrations() {
        use crate::db_index::derivation::{self, FactKey};

        let mut index = GmodClassMetadataIndex::new();
        let file = FileId::new(1);
        let _recording = derivation::begin(derivation::DerivationId::workspace(
            derivation::DerivationKind::GmodLoad,
        ));
        assert!(index.get_vgui_registrations(&file).next().is_none());
        let reads = derivation::end().into_recorded().2;
        let file_calls = FactKey::GmodClassFile(file);
        assert!(file_calls.is_base());
        assert_eq!(reads.len(), 1);
        assert!(
            reads
                .iter()
                .all(|read| read.base() == Some(file_calls.clone()))
        );
        let fingerprints = |index: &GmodClassMetadataIndex| {
            reads
                .iter()
                .map(|read| derivation::in_file(Some(file), || index.fact_fingerprint(read)))
                .collect::<Vec<_>>()
        };
        let seen = fingerprints(&index);

        index.add_call(
            file,
            GmodScriptedClassCallKind::AccessorFunc,
            vgui_register_call("GetValue", "Value", 10),
        );
        assert_ne!(
            derivation::in_file(Some(file), || index.fact_fingerprint(&file_calls)),
            derivation::ABSENT
        );
        assert_eq!(fingerprints(&index), seen);

        index.add_call(
            file,
            GmodScriptedClassCallKind::DefineBaseClass,
            vgui_register_call("base_anim", "", 20),
        );
        let moved = |before: &[u64], after: &[u64]| {
            before.iter().zip(after).filter(|(b, a)| b != a).count()
        };
        let based = fingerprints(&index);
        assert_eq!(moved(&seen, &based), 0);

        index.add_call(
            file,
            GmodScriptedClassCallKind::VguiRegister,
            vgui_register_call("Panel", "DFrame", 30),
        );
        assert_eq!(moved(&based, &fingerprints(&index)), 1);
        assert_eq!(index.get_vgui_registrations(&file).count(), 1);
    }
}

crate::db_index::cow_shards::task_fork_fields!(GmodClassMetadataIndex {
    file_metadata,
    vgui_parent_call_files,
    vgui_panels => crate::db_index::cow_shards::merge_list,
    derma_skins,
    vgui_panel_parent_chains,
    incomplete_vgui_panel_parent_chains,
    meta_fingerprints => crate::db_index::cow_shards::merge_sum,
    vgui_file_relations,
});
