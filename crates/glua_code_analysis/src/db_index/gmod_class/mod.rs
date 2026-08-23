use std::collections::{HashMap, HashSet};

use glua_parser::LuaSyntaxId;
use rowan::TextSize;

use super::LuaIndex;
use crate::{FileId, LuaTypeDeclId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq)]
pub struct GmodClassCallArg {
    pub syntax_id: LuaSyntaxId,
    pub value: Option<GmodClassCallLiteral>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GmodNamedStringCallRoles {
    pub name_arg_idx: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GmodNetworkVarCallRoles {
    pub type_arg_idx: Option<usize>,
    pub name_arg_idx: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodVguiPanelCallRoles {
    pub define: GmodClassCallArgSource,
    pub table: Option<GmodClassCallArgSource>,
    pub base: Option<GmodClassCallArgSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Default, PartialEq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodVguiParentRelation {
    pub child_type_id: LuaTypeDeclId,
    pub parent_chain: Vec<LuaTypeDeclId>,
    pub parent_chain_complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GmodVguiParentCallOrigin {
    Annotated,
    Forwarded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodVguiParentCallMetadata {
    pub syntax_id: LuaSyntaxId,
    pub child: GmodVguiParentSource,
    pub parent: GmodVguiParentSource,
    pub relations: Vec<GmodVguiParentRelation>,
    pub origin: GmodVguiParentCallOrigin,
    /// This call resolved against its file's syntax tree, before the
    /// inheritance chain is walked.
    ///
    /// The chain walk is global and has to see every call, but resolving a call
    /// means walking the declaring file's syntax tree — and re-analysing one
    /// file cannot change what another file's call resolves to on its own.
    /// Caching it here means a re-analysis only walks the files it rebuilt:
    /// analysis creates calls with this empty, so a rebuilt file recomputes and
    /// an untouched file reuses, with no dirty set to keep in step.
    pub resolved_source: Option<GmodVguiResolvedParentSource>,
}

/// How a vgui parent call names its parent, resolved to type ids.
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodVguiResolvedParentSource {
    pub child_type_ids: Vec<LuaTypeDeclId>,
    pub parent: GmodVguiParentSourceResolution,
}

impl GmodScriptedClassFileMetadata {
    pub fn get_define_baseclass_name(&self) -> Option<&str> {
        self.define_baseclass_calls.iter().rev().find_map(|call| {
            match call.literal_args.get(call.inheritance_name_arg_idx()) {
                Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
                    Some(name.as_str())
                }
                _ => None,
            }
        })
    }

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

#[derive(Debug, Default)]
pub struct GmodClassMetadataIndex {
    file_metadata: HashMap<FileId, GmodScriptedClassFileMetadata>,
    vgui_panels: HashMap<String, Vec<VguiPanelDefinition>>,
    derma_skins: HashMap<String, Vec<DermaSkinDefinition>>,
    vgui_forwarding_parents: HashMap<(LuaTypeDeclId, String), Vec<LuaTypeDeclId>>,
    vgui_panel_parent_chains: HashMap<LuaTypeDeclId, Vec<LuaTypeDeclId>>,
    incomplete_vgui_panel_parent_chains: HashSet<LuaTypeDeclId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
            file_metadata: HashMap::new(),
            vgui_panels: HashMap::new(),
            derma_skins: HashMap::new(),
            vgui_forwarding_parents: HashMap::new(),
            vgui_panel_parent_chains: HashMap::new(),
            incomplete_vgui_panel_parent_chains: HashSet::new(),
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
        vgui_panels: &mut HashMap<String, Vec<VguiPanelDefinition>>,
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
        derma_skins: &mut HashMap<String, Vec<DermaSkinDefinition>>,
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
    fn remove_cached_entry<T>(
        cache: &mut HashMap<String, Vec<T>>,
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
        let mut vgui_panels = HashMap::new();
        let mut derma_skins = HashMap::new();

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
        self.recompute_vgui_panel_parent_chains();
    }

    pub fn add_call(
        &mut self,
        file_id: FileId,
        kind: GmodScriptedClassCallKind,
        call_metadata: GmodScriptedClassCallMetadata,
    ) {
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
                self.replace_cached_call(file_id, kind, &old_metadata, &call_metadata);
                return;
            }
        }

        self.update_vgui_panels_from_call(file_id, kind, &call_metadata);
        self.update_derma_skins_from_call(file_id, kind, &call_metadata);
        self.file_metadata
            .entry(file_id)
            .or_default()
            .calls_by_kind_mut(kind)
            .push(call_metadata);
    }

    pub fn add_vgui_parent_call(&mut self, file_id: FileId, call: GmodVguiParentCallMetadata) {
        let calls = &mut self
            .file_metadata
            .entry(file_id)
            .or_default()
            .vgui_parent_calls;
        if let Some(existing) = calls
            .iter_mut()
            .find(|existing| existing.syntax_id == call.syntax_id)
        {
            *existing = call;
        } else {
            calls.push(call);
        }
    }

    pub fn clear_forwarded_vgui_parent_calls(&mut self) {
        for metadata in self.file_metadata.values_mut() {
            metadata
                .vgui_parent_calls
                .retain(|call| call.origin != GmodVguiParentCallOrigin::Forwarded);
        }
    }

    pub fn clear_forwarded_vgui_parent_calls_for_files(&mut self, file_ids: &[FileId]) {
        for file_id in file_ids {
            let Some(metadata) = self.file_metadata.get_mut(file_id) else {
                continue;
            };
            metadata
                .vgui_parent_calls
                .retain(|call| call.origin != GmodVguiParentCallOrigin::Forwarded);
        }
    }

    pub fn has_annotated_vgui_parent_calls(&self, file_id: FileId) -> bool {
        self.file_metadata.get(&file_id).is_some_and(|metadata| {
            metadata
                .vgui_parent_calls
                .iter()
                .any(|call| call.origin == GmodVguiParentCallOrigin::Annotated)
        })
    }

    pub fn update_vgui_forwarding_parents(
        &mut self,
        forwarding_parents: &HashMap<(LuaTypeDeclId, String), Vec<LuaTypeDeclId>>,
    ) -> bool {
        if &self.vgui_forwarding_parents == forwarding_parents {
            return false;
        }
        self.vgui_forwarding_parents.clone_from(forwarding_parents);
        true
    }

    pub fn get_vgui_parent_calls(&self, file_id: &FileId) -> &[GmodVguiParentCallMetadata] {
        self.file_metadata
            .get(file_id)
            .map(|metadata| metadata.vgui_parent_calls.as_slice())
            .unwrap_or_default()
    }

    /// Cache each call's pre-chain resolution, so the next re-analysis only
    /// walks the syntax trees of files it rebuilt.
    pub fn set_vgui_resolved_parent_sources(
        &mut self,
        resolved_by_file: &[(FileId, Vec<(LuaSyntaxId, GmodVguiResolvedParentSource)>)],
    ) {
        for (file_id, resolved) in resolved_by_file {
            let Some(metadata) = self.file_metadata.get_mut(file_id) else {
                continue;
            };
            for call in &mut metadata.vgui_parent_calls {
                call.resolved_source = resolved
                    .iter()
                    .find(|(syntax_id, _)| *syntax_id == call.syntax_id)
                    .map(|(_, source)| source.clone());
            }
        }
    }

    pub fn set_vgui_parent_relations(
        &mut self,
        resolved_by_file: Vec<(FileId, Vec<(LuaSyntaxId, Vec<GmodVguiParentRelation>)>)>,
    ) {
        for (file_id, resolved) in resolved_by_file {
            let Some(metadata) = self.file_metadata.get_mut(&file_id) else {
                continue;
            };
            for call in &mut metadata.vgui_parent_calls {
                call.relations = resolved
                    .iter()
                    .find(|(syntax_id, _)| *syntax_id == call.syntax_id)
                    .map(|(_, relations)| relations.clone())
                    .unwrap_or_default();
            }
        }
        self.recompute_vgui_panel_parent_chains();
    }

    pub fn get_vgui_panel_parent_chain(
        &self,
        child_type_id: &LuaTypeDeclId,
    ) -> Option<&[LuaTypeDeclId]> {
        self.vgui_panel_parent_chains
            .get(child_type_id)
            .map(Vec::as_slice)
    }

    pub fn vgui_panel_parent_chain_is_complete(&self, child_type_id: &LuaTypeDeclId) -> bool {
        !self
            .incomplete_vgui_panel_parent_chains
            .contains(child_type_id)
    }

    fn recompute_vgui_panel_parent_chains(&mut self) {
        // Order-free despite the hash-ordered walk: a chain survives only if
        // every relation agrees on it — any disagreement marks the child
        // incomplete and the entry is dropped below, whichever one landed first.
        let mut parent_chains = HashMap::<LuaTypeDeclId, Vec<LuaTypeDeclId>>::new();
        let mut incomplete = HashSet::new();
        for metadata in self.file_metadata.values() {
            for call in &metadata.vgui_parent_calls {
                for relation in &call.relations {
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
        self.vgui_panel_parent_chains = parent_chains;
        self.incomplete_vgui_panel_parent_chains = incomplete;
    }

    pub fn get_file_metadata(&self, file_id: &FileId) -> Option<&GmodScriptedClassFileMetadata> {
        self.file_metadata.get(file_id)
    }

    pub fn get_define_baseclass_name(&self, file_id: &FileId) -> Option<&str> {
        self.get_file_metadata(file_id)?.get_define_baseclass_name()
    }

    pub fn iter_file_metadata(
        &self,
    ) -> impl Iterator<Item = (&FileId, &GmodScriptedClassFileMetadata)> {
        self.file_metadata.iter()
    }

    pub fn find_vgui_panel_definitions(
        &self,
        name: &str,
    ) -> Vec<(FileId, &GmodScriptedClassCallMetadata)> {
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
            self.file_metadata.remove(&file_id);
        }
        self.recompute_derived_caches();
    }

    fn clear(&mut self) {
        self.file_metadata.clear();
        self.vgui_panels.clear();
        self.derma_skins.clear();
        self.vgui_forwarding_parents.clear();
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
}
