mod file_dependency_relation;

use std::collections::{HashMap, HashSet};

use file_dependency_relation::FileDependencyRelation;

use crate::FileId;

use super::LuaIndex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LuaDependencyKind {
    Require,
    Include,
    AddCSLuaFile,
    IncludeCS,
}

#[derive(Debug)]
pub struct LuaDependencyIndex {
    dependencies: HashMap<FileId, HashSet<FileId>>,
    dependency_kinds: HashMap<FileId, HashMap<FileId, HashSet<LuaDependencyKind>>>,
}

impl Default for LuaDependencyIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaDependencyIndex {
    pub fn new() -> Self {
        Self {
            dependencies: HashMap::new(),
            dependency_kinds: HashMap::new(),
        }
    }

    pub fn add_required_file(&mut self, file_id: FileId, dependency_id: FileId) {
        self.add_dependency_file(file_id, dependency_id, LuaDependencyKind::Require);
    }

    pub fn add_dependency_file(
        &mut self,
        file_id: FileId,
        dependency_id: FileId,
        kind: LuaDependencyKind,
    ) {
        self.dependencies
            .entry(file_id)
            .or_default()
            .insert(dependency_id);
        self.dependency_kinds
            .entry(file_id)
            .or_default()
            .entry(dependency_id)
            .or_default()
            .insert(kind);
    }

    pub fn get_required_files(&self, file_id: &FileId) -> Option<&HashSet<FileId>> {
        self.dependencies.get(file_id)
    }

    pub fn get_dependency_kinds(
        &self,
        file_id: &FileId,
        dependency_id: &FileId,
    ) -> Option<&HashSet<LuaDependencyKind>> {
        self.dependency_kinds
            .get(file_id)
            .and_then(|dependencies| dependencies.get(dependency_id))
    }

    pub fn get_file_dependencies<'a>(&'a self) -> FileDependencyRelation<'a> {
        FileDependencyRelation::new(&self.dependencies)
    }
}

impl LuaIndex for LuaDependencyIndex {
    fn remove(&mut self, file_id: FileId) {
        self.dependencies.remove(&file_id);
        self.dependency_kinds.remove(&file_id);
    }

    fn clear(&mut self) {
        self.dependencies.clear();
        self.dependency_kinds.clear();
    }
}
