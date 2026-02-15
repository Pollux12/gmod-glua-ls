use std::collections::HashMap;

use emmylua_parser::LuaSyntaxId;
use smol_str::SmolStr;

use super::traits::LuaIndex;
use crate::{FileId, LuaTypeDeclId};

#[derive(Debug, Clone)]
pub struct AccessorFuncAnnotation {
    pub name_param_index: usize,
    pub file_id: FileId,
}

#[derive(Debug, Default)]
pub struct AccessorFuncAnnotationIndex {
    by_name: HashMap<SmolStr, Vec<AccessorFuncAnnotation>>,
    by_file: HashMap<FileId, Vec<SmolStr>>,
}

impl AccessorFuncAnnotationIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, name: SmolStr, annotation: AccessorFuncAnnotation) {
        let file_id = annotation.file_id;
        self.by_name
            .entry(name.clone())
            .or_default()
            .push(annotation);
        self.by_file.entry(file_id).or_default().push(name);
    }

    pub fn contains_name(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    pub fn get_annotations(&self, name: &str) -> Option<&Vec<AccessorFuncAnnotation>> {
        self.by_name.get(name)
    }
}

impl LuaIndex for AccessorFuncAnnotationIndex {
    fn remove(&mut self, file_id: FileId) {
        if let Some(names) = self.by_file.remove(&file_id) {
            for name in names {
                if let Some(annotations) = self.by_name.get_mut(&name) {
                    annotations.retain(|annotation| annotation.file_id != file_id);
                    if annotations.is_empty() {
                        self.by_name.remove(&name);
                    }
                }
            }
        }
    }

    fn clear(&mut self) {
        self.by_name.clear();
        self.by_file.clear();
    }
}

#[derive(Debug, Clone)]
pub struct AccessorFuncCallMetadata {
    pub syntax_id: LuaSyntaxId,
    pub owner_type_id: LuaTypeDeclId,
    pub accessor_name: String,
    pub name_arg_syntax_id: Option<LuaSyntaxId>,
}

#[derive(Debug, Default)]
pub struct AccessorFuncCallIndex {
    calls: HashMap<FileId, Vec<AccessorFuncCallMetadata>>,
}

impl AccessorFuncCallIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_call(&mut self, file_id: FileId, metadata: AccessorFuncCallMetadata) {
        self.calls.entry(file_id).or_default().push(metadata);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&FileId, &Vec<AccessorFuncCallMetadata>)> {
        self.calls.iter()
    }
}

impl LuaIndex for AccessorFuncCallIndex {
    fn remove(&mut self, file_id: FileId) {
        self.calls.remove(&file_id);
    }

    fn clear(&mut self) {
        self.calls.clear();
    }
}
