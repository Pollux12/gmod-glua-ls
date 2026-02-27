use std::collections::HashMap;

use emmylua_parser::LuaSyntaxId;

use super::LuaIndex;
use crate::FileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GmodScriptedClassCallKind {
    DefineBaseClass,
    AccessorFunc,
    NetworkVar,
    NetworkVarElement,
    VguiRegister,
    DermaDefineControl,
}

impl GmodScriptedClassCallKind {
    pub fn from_call_name(call_name: &str) -> Option<Self> {
        match call_name {
            "DEFINE_BASECLASS" => Some(Self::DefineBaseClass),
            "AccessorFunc" => Some(Self::AccessorFunc),
            "NetworkVar" => Some(Self::NetworkVar),
            "NetworkVarElement" => Some(Self::NetworkVarElement),
            _ => None,
        }
    }

    pub fn from_call_path(path: &str) -> Option<Self> {
        if path == "vgui.Register" || path.ends_with(".vgui.Register") {
            return Some(Self::VguiRegister);
        }
        if path == "derma.DefineControl" || path.ends_with(".derma.DefineControl") {
            return Some(Self::DermaDefineControl);
        }
        None
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

#[derive(Debug, Clone, PartialEq)]
pub struct GmodScriptedClassCallMetadata {
    pub syntax_id: LuaSyntaxId,
    pub literal_args: Vec<Option<GmodClassCallLiteral>>,
    pub args: Vec<GmodClassCallArg>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GmodScriptedClassFileMetadata {
    pub define_baseclass_calls: Vec<GmodScriptedClassCallMetadata>,
    pub accessor_func_calls: Vec<GmodScriptedClassCallMetadata>,
    pub network_var_calls: Vec<GmodScriptedClassCallMetadata>,
    pub network_var_element_calls: Vec<GmodScriptedClassCallMetadata>,
    pub vgui_register_calls: Vec<GmodScriptedClassCallMetadata>,
    pub derma_define_control_calls: Vec<GmodScriptedClassCallMetadata>,
}

impl GmodScriptedClassFileMetadata {
    pub fn get_define_baseclass_name(&self) -> Option<&str> {
        self.define_baseclass_calls
            .iter()
            .rev()
            .find_map(|call| match call.literal_args.first() {
                Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => {
                    Some(name.as_str())
                }
                _ => None,
            })
    }

    fn calls_by_kind_mut(
        &mut self,
        kind: GmodScriptedClassCallKind,
    ) -> &mut Vec<GmodScriptedClassCallMetadata> {
        match kind {
            GmodScriptedClassCallKind::DefineBaseClass => &mut self.define_baseclass_calls,
            GmodScriptedClassCallKind::AccessorFunc => &mut self.accessor_func_calls,
            GmodScriptedClassCallKind::NetworkVar => &mut self.network_var_calls,
            GmodScriptedClassCallKind::NetworkVarElement => &mut self.network_var_element_calls,
            GmodScriptedClassCallKind::VguiRegister => &mut self.vgui_register_calls,
            GmodScriptedClassCallKind::DermaDefineControl => &mut self.derma_define_control_calls,
        }
    }
}

#[derive(Debug, Default)]
pub struct GmodClassMetadataIndex {
    file_metadata: HashMap<FileId, GmodScriptedClassFileMetadata>,
    vgui_panels: HashMap<String, Option<String>>,
}

impl GmodClassMetadataIndex {
    pub fn new() -> Self {
        Self {
            file_metadata: HashMap::new(),
            vgui_panels: HashMap::new(),
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

    fn maybe_extract_vgui_panel(
        kind: GmodScriptedClassCallKind,
        call_metadata: &GmodScriptedClassCallMetadata,
    ) -> Option<(String, Option<String>)> {
        let base_arg_index = match kind {
            GmodScriptedClassCallKind::VguiRegister => 2,
            GmodScriptedClassCallKind::DermaDefineControl => 3,
            _ => return None,
        };

        let panel_name = Self::extract_non_empty_string_arg(call_metadata, 0)?;
        let base_name = Self::extract_non_empty_string_arg(call_metadata, base_arg_index);
        Some((panel_name, base_name))
    }

    fn insert_vgui_panel_from_call(
        vgui_panels: &mut HashMap<String, Option<String>>,
        kind: GmodScriptedClassCallKind,
        call_metadata: &GmodScriptedClassCallMetadata,
    ) {
        let Some((panel_name, base_name)) = Self::maybe_extract_vgui_panel(kind, call_metadata)
        else {
            return;
        };

        vgui_panels.insert(panel_name, base_name);
    }

    fn update_vgui_panels_from_call(
        &mut self,
        kind: GmodScriptedClassCallKind,
        call_metadata: &GmodScriptedClassCallMetadata,
    ) {
        Self::insert_vgui_panel_from_call(&mut self.vgui_panels, kind, call_metadata);
    }

    fn recompute_vgui_panels(&mut self) {
        let mut vgui_panels = HashMap::new();

        for file_metadata in self.file_metadata.values() {
            for call in &file_metadata.vgui_register_calls {
                Self::insert_vgui_panel_from_call(
                    &mut vgui_panels,
                    GmodScriptedClassCallKind::VguiRegister,
                    call,
                );
            }
            for call in &file_metadata.derma_define_control_calls {
                Self::insert_vgui_panel_from_call(
                    &mut vgui_panels,
                    GmodScriptedClassCallKind::DermaDefineControl,
                    call,
                );
            }
        }

        self.vgui_panels = vgui_panels;
    }

    pub fn add_call(
        &mut self,
        file_id: FileId,
        kind: GmodScriptedClassCallKind,
        call_metadata: GmodScriptedClassCallMetadata,
    ) {
        self.update_vgui_panels_from_call(kind, &call_metadata);

        self.file_metadata
            .entry(file_id)
            .or_default()
            .calls_by_kind_mut(kind)
            .push(call_metadata);
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
                let Some(Some(GmodClassCallLiteral::String(panel_name))) =
                    call.literal_args.first()
                else {
                    continue;
                };

                if panel_name == name {
                    definitions.push((*file_id, call));
                }
            }
        }

        definitions
    }

    pub fn get_vgui_panel_base(&self, name: &str) -> Option<Option<String>> {
        self.vgui_panels.get(name).cloned()
    }
}

impl LuaIndex for GmodClassMetadataIndex {
    fn remove(&mut self, file_id: FileId) {
        self.file_metadata.remove(&file_id);
        self.recompute_vgui_panels();
    }

    fn clear(&mut self) {
        self.file_metadata.clear();
        self.recompute_vgui_panels();
    }
}
