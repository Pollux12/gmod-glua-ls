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

    fn remove_files(&mut self, file_ids: &[FileId]) {
        let removed_file_ids = file_ids.iter().copied().collect::<HashSet<_>>();
        self.dependencies.retain(|file_id, dependencies| {
            !removed_file_ids.contains(file_id) && {
                dependencies.retain(|dependency_id| !removed_file_ids.contains(dependency_id));
                !dependencies.is_empty()
            }
        });
        self.dependency_kinds.retain(|file_id, dependency_kinds| {
            !removed_file_ids.contains(file_id) && {
                dependency_kinds
                    .retain(|dependency_id, _| !removed_file_ids.contains(dependency_id));
                !dependency_kinds.is_empty()
            }
        });
        self.dependency_sites.retain(|file_id, sites| {
            if removed_file_ids.contains(file_id) {
                return false;
            }

            for site in sites {
                if site
                    .target_file_id
                    .is_some_and(|target_file_id| removed_file_ids.contains(&target_file_id))
                {
                    site.target_file_id = None;
                }
            }
            true
        });
    }

    fn clear(&mut self) {
        self.dependencies.clear();
        self.dependency_kinds.clear();
        self.dependency_sites.clear();
    }
}

#[cfg(test)]
mod tests {
    use rowan::{TextRange, TextSize};

    use super::{LuaDependencyIndex, LuaDependencyKind, LuaDependencySite};
    use crate::{FileId, db_index::LuaIndex};

    #[test]
    fn batch_removal_preserves_surviving_dependency_edges() {
        let removed = FileId::new(1);
        let other_removed = FileId::new(2);
        let surviving = FileId::new(3);
        let external = FileId::new(4);
        let mut index = LuaDependencyIndex::new();
        index.add_required_file(surviving, removed);
        index.add_required_file(surviving, external);
        index.add_required_file(removed, external);
        index.add_dependency_site(LuaDependencySite {
            source_file_id: surviving,
            target_file_id: Some(removed),
            kind: LuaDependencyKind::Include,
            path: None,
            original_expr: "include(\"removed\")".to_string(),
            range: TextRange::new(TextSize::new(0), TextSize::new(1)),
        });

        index.remove_files(&[other_removed, removed, other_removed]);

        assert_eq!(index.get_required_files(&surviving).unwrap().len(), 1);
        assert!(
            index
                .get_required_files(&surviving)
                .unwrap()
                .contains(&external)
        );
        assert!(index.get_required_files(&removed).is_none());
        assert_eq!(
            index.get_dependency_sites(&surviving).unwrap()[0].target_file_id,
            None
        );
    }
}
