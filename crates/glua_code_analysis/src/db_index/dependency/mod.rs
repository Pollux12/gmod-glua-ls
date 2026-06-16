mod file_dependency_relation;

use std::collections::{HashMap, HashSet};

use file_dependency_relation::FileDependencyRelation;
use rowan::TextRange;

use crate::FileId;

use super::LuaIndex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LuaDependencyKind {
    Require,
    Include,
    AddCSLuaFile,
    IncludeCS,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaDependencySite {
    pub source_file_id: FileId,
    pub target_file_id: Option<FileId>,
    pub kind: LuaDependencyKind,
    pub path: Option<String>,
    pub original_expr: String,
    pub range: TextRange,
}

#[derive(Debug)]
pub struct LuaDependencyIndex {
    dependencies: HashMap<FileId, HashSet<FileId>>,
    dependency_kinds: HashMap<FileId, HashMap<FileId, HashSet<LuaDependencyKind>>>,
    dependency_sites: HashMap<FileId, Vec<LuaDependencySite>>,
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
            dependency_sites: HashMap::new(),
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

    pub fn add_dependency_site(&mut self, site: LuaDependencySite) {
        if let Some(target_file_id) = site.target_file_id {
            self.add_dependency_file(site.source_file_id, target_file_id, site.kind);
        }
        let sites = self
            .dependency_sites
            .entry(site.source_file_id)
            .or_default();
        if !sites.iter().any(|existing| existing == &site) {
            sites.push(site);
        }
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

    pub fn get_dependency_sites(&self, file_id: &FileId) -> Option<&[LuaDependencySite]> {
        self.dependency_sites.get(file_id).map(Vec::as_slice)
    }

    pub fn iter_dependency_sites(&self) -> impl Iterator<Item = (&FileId, &[LuaDependencySite])> {
        self.dependency_sites
            .iter()
            .map(|(file_id, sites)| (file_id, sites.as_slice()))
    }

    pub fn get_file_dependencies<'a>(&'a self) -> FileDependencyRelation<'a> {
        FileDependencyRelation::new(&self.dependencies)
    }
}

impl LuaIndex for LuaDependencyIndex {
    fn remove(&mut self, file_id: FileId) {
        self.dependencies.remove(&file_id);
        self.dependency_kinds.remove(&file_id);
        self.dependency_sites.remove(&file_id);

        for dependencies in self.dependencies.values_mut() {
            dependencies.remove(&file_id);
        }
        self.dependencies
            .retain(|_, dependencies| !dependencies.is_empty());

        for dependency_kinds in self.dependency_kinds.values_mut() {
            dependency_kinds.remove(&file_id);
        }
        self.dependency_kinds
            .retain(|_, dependency_kinds| !dependency_kinds.is_empty());

        for sites in self.dependency_sites.values_mut() {
            for site in sites {
                if site.target_file_id == Some(file_id) {
                    site.target_file_id = None;
                }
            }
        }
    }

    fn clear(&mut self) {
        self.dependencies.clear();
        self.dependency_kinds.clear();
        self.dependency_sites.clear();
    }
}
