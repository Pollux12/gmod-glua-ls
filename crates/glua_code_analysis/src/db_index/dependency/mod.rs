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
    CompileFile,
    AddCSLuaFile,
    IncludeCS,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaDependencySite {
    pub source_file_id: FileId,
    pub target_file_id: Option<FileId>,
    pub kind: LuaDependencyKind,
    pub path: Option<String>,
    /// Resolver-equivalent path keys retained while resolved so target removal
    /// can transition this site into the reverse unresolved index without a scan.
    pub path_keys: Vec<String>,
    pub original_expr: String,
    /// Full range of the annotated load call. `range` remains the path-argument
    /// range used by load diagnostics.
    pub call_range: TextRange,
    pub range: TextRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DependencySiteLocation {
    source_file_id: FileId,
    site_index: usize,
}

#[derive(Debug)]
pub struct LuaDependencyIndex {
    dependencies: HashMap<FileId, HashSet<FileId>>,
    dependency_kinds: HashMap<FileId, HashMap<FileId, HashSet<LuaDependencyKind>>>,
    dependency_callers_by_target: HashMap<FileId, HashSet<FileId>>,
    dependency_sites: HashMap<FileId, Vec<LuaDependencySite>>,
    resolved_sites_by_target: HashMap<FileId, Vec<DependencySiteLocation>>,
    /// Normalized target path key -> unresolved callers.
    unresolved_dependents_by_path_key: HashMap<String, HashSet<FileId>>,
    #[cfg(test)]
    target_transition_site_visits: usize,
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
            dependency_callers_by_target: HashMap::new(),
            dependency_sites: HashMap::new(),
            resolved_sites_by_target: HashMap::new(),
            unresolved_dependents_by_path_key: HashMap::new(),
            #[cfg(test)]
            target_transition_site_visits: 0,
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
        let inserted = self
            .dependencies
            .entry(file_id)
            .or_default()
            .insert(dependency_id);
        if inserted {
            self.dependency_callers_by_target
                .entry(dependency_id)
                .or_default()
                .insert(file_id);
        }
        self.dependency_kinds
            .entry(file_id)
            .or_default()
            .entry(dependency_id)
            .or_default()
            .insert(kind);
    }

    pub fn add_dependency_site(&mut self, site: LuaDependencySite) {
        let is_new = !self
            .dependency_sites
            .get(&site.source_file_id)
            .is_some_and(|sites| sites.iter().any(|existing| existing == &site));
        if !is_new {
            return;
        }

        let site_index = self
            .dependency_sites
            .get(&site.source_file_id)
            .map_or(0, Vec::len);
        if let Some(target_file_id) = site.target_file_id {
            self.add_dependency_file(site.source_file_id, target_file_id, site.kind);
            self.resolved_sites_by_target
                .entry(target_file_id)
                .or_default()
                .push(DependencySiteLocation {
                    source_file_id: site.source_file_id,
                    site_index,
                });
        } else {
            Self::index_unresolved_site(&mut self.unresolved_dependents_by_path_key, &site);
        }
        self.dependency_sites
            .entry(site.source_file_id)
            .or_default()
            .push(site);
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

    pub fn collect_unresolved_path_dependents(
        &self,
        path_keys: impl IntoIterator<Item = String>,
    ) -> Vec<FileId> {
        let mut dependents = HashSet::new();
        for path_key in path_keys {
            if let Some(path_dependents) = self.unresolved_dependents_by_path_key.get(&path_key) {
                dependents.extend(path_dependents.iter().copied());
            }
        }

        let mut dependents = dependents.into_iter().collect::<Vec<_>>();
        dependents.sort_unstable();
        dependents
    }

    fn index_unresolved_site(
        reverse_index: &mut HashMap<String, HashSet<FileId>>,
        site: &LuaDependencySite,
    ) {
        for path_key in &site.path_keys {
            reverse_index
                .entry(path_key.clone())
                .or_default()
                .insert(site.source_file_id);
        }
    }

    fn unindex_unresolved_sites(&mut self, sites: &[LuaDependencySite]) {
        for site in sites {
            if site.target_file_id.is_some() {
                continue;
            }
            for path_key in &site.path_keys {
                let should_remove = self
                    .unresolved_dependents_by_path_key
                    .get_mut(path_key)
                    .is_some_and(|dependents| {
                        dependents.remove(&site.source_file_id);
                        dependents.is_empty()
                    });
                if should_remove {
                    self.unresolved_dependents_by_path_key.remove(path_key);
                }
            }
        }
    }

    fn unindex_source_sites(&mut self, source_file_id: FileId, sites: &[LuaDependencySite]) {
        self.unindex_unresolved_sites(sites);
        let target_file_ids = sites
            .iter()
            .filter_map(|site| site.target_file_id)
            .collect::<HashSet<_>>();
        for target_file_id in target_file_ids {
            let should_remove = self
                .resolved_sites_by_target
                .get_mut(&target_file_id)
                .is_some_and(|locations| {
                    locations.retain(|location| location.source_file_id != source_file_id);
                    locations.is_empty()
                });
            if should_remove {
                self.resolved_sites_by_target.remove(&target_file_id);
            }
        }
    }

    fn remove_source_dependency_edges(&mut self, source_file_id: FileId) {
        let Some(target_file_ids) = self.dependencies.remove(&source_file_id) else {
            self.dependency_kinds.remove(&source_file_id);
            return;
        };
        self.dependency_kinds.remove(&source_file_id);
        for target_file_id in target_file_ids {
            let should_remove = self
                .dependency_callers_by_target
                .get_mut(&target_file_id)
                .is_some_and(|callers| {
                    callers.remove(&source_file_id);
                    callers.is_empty()
                });
            if should_remove {
                self.dependency_callers_by_target.remove(&target_file_id);
            }
        }
    }

    fn remove_incoming_dependency_edges(&mut self, target_file_id: FileId) {
        let Some(callers) = self.dependency_callers_by_target.remove(&target_file_id) else {
            return;
        };
        for source_file_id in callers {
            let remove_dependencies =
                self.dependencies
                    .get_mut(&source_file_id)
                    .is_some_and(|dependencies| {
                        dependencies.remove(&target_file_id);
                        dependencies.is_empty()
                    });
            if remove_dependencies {
                self.dependencies.remove(&source_file_id);
            }

            let remove_dependency_kinds = self
                .dependency_kinds
                .get_mut(&source_file_id)
                .is_some_and(|dependency_kinds| {
                    dependency_kinds.remove(&target_file_id);
                    dependency_kinds.is_empty()
                });
            if remove_dependency_kinds {
                self.dependency_kinds.remove(&source_file_id);
            }
        }
    }

    fn transition_target_sites_to_unresolved(&mut self, target_file_id: FileId) {
        let Some(mut locations) = self.resolved_sites_by_target.remove(&target_file_id) else {
            return;
        };
        locations.sort_unstable();
        #[cfg(test)]
        let mut site_visits = 0;
        let dependency_sites = &mut self.dependency_sites;
        let unresolved_dependents_by_path_key = &mut self.unresolved_dependents_by_path_key;
        for location in locations {
            #[cfg(test)]
            {
                site_visits += 1;
            }
            let Some(site) = dependency_sites
                .get_mut(&location.source_file_id)
                .and_then(|sites| sites.get_mut(location.site_index))
                .filter(|site| site.target_file_id == Some(target_file_id))
            else {
                continue;
            };
            site.target_file_id = None;
            Self::index_unresolved_site(unresolved_dependents_by_path_key, site);
        }
        #[cfg(test)]
        {
            self.target_transition_site_visits += site_visits;
        }
    }

    #[cfg(test)]
    fn target_transition_site_visits(&self) -> usize {
        self.target_transition_site_visits
    }

    pub fn get_file_dependencies<'a>(&'a self) -> FileDependencyRelation<'a> {
        FileDependencyRelation::new(&self.dependencies)
    }
}

impl LuaIndex for LuaDependencyIndex {
    fn remove(&mut self, file_id: FileId) {
        if let Some(sites) = self.dependency_sites.remove(&file_id) {
            self.unindex_source_sites(file_id, &sites);
        }
        self.remove_source_dependency_edges(file_id);
        self.transition_target_sites_to_unresolved(file_id);
        self.remove_incoming_dependency_edges(file_id);
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        let mut file_ids = file_ids.to_vec();
        file_ids.sort_unstable();
        file_ids.dedup();
        for file_id in file_ids {
            self.remove(file_id);
        }
    }

    fn clear(&mut self) {
        self.dependencies.clear();
        self.dependency_kinds.clear();
        self.dependency_callers_by_target.clear();
        self.dependency_sites.clear();
        self.resolved_sites_by_target.clear();
        self.unresolved_dependents_by_path_key.clear();
        #[cfg(test)]
        {
            self.target_transition_site_visits = 0;
        }
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
            path: Some("removed.lua".to_string()),
            path_keys: vec!["removed.lua".to_string()],
            original_expr: "include(\"removed\")".to_string(),
            call_range: TextRange::new(TextSize::new(0), TextSize::new(1)),
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
        assert_eq!(
            index.collect_unresolved_path_dependents(["removed.lua".to_string()]),
            vec![surviving]
        );
    }

    #[test]
    fn unresolved_path_dependents_are_collected_deterministically() {
        let first = FileId::new(1);
        let second = FileId::new(2);
        let resolved = FileId::new(3);
        let target = FileId::new(4);
        let mut index = LuaDependencyIndex::new();
        let site = |source_file_id, target_file_id, path: Option<&str>| LuaDependencySite {
            source_file_id,
            target_file_id,
            kind: LuaDependencyKind::Include,
            path: path.map(str::to_string),
            path_keys: path.into_iter().map(str::to_string).collect(),
            original_expr: "include(...)".to_string(),
            call_range: TextRange::new(TextSize::new(0), TextSize::new(1)),
            range: TextRange::new(TextSize::new(0), TextSize::new(1)),
        };
        index.add_dependency_site(site(second, None, Some("target.lua")));
        index.add_dependency_site(site(first, None, Some("target.lua")));
        index.add_dependency_site(site(resolved, Some(target), Some("target.lua")));
        index.add_dependency_site(site(FileId::new(5), None, Some("other.lua")));
        index.add_dependency_site(site(FileId::new(6), None, None));

        assert_eq!(
            index.collect_unresolved_path_dependents(["target.lua".to_string()]),
            vec![first, second]
        );
    }

    #[test]
    fn unresolved_reverse_index_tracks_add_update_remove_and_clear() {
        let source = FileId::new(1);
        let target = FileId::new(2);
        let site = |target_file_id, path: &str| LuaDependencySite {
            source_file_id: source,
            target_file_id,
            kind: LuaDependencyKind::Include,
            path: Some(path.to_string()),
            path_keys: vec![path.to_string()],
            original_expr: "include(...)".to_string(),
            call_range: TextRange::new(TextSize::new(0), TextSize::new(1)),
            range: TextRange::new(TextSize::new(0), TextSize::new(1)),
        };
        let lookup = |index: &LuaDependencyIndex, path: &str| {
            index.collect_unresolved_path_dependents([path.to_string()])
        };

        let mut index = LuaDependencyIndex::new();
        index.add_dependency_site(site(None, "old.lua"));
        assert_eq!(lookup(&index, "old.lua"), vec![source]);

        index.remove(source);
        index.add_dependency_site(site(None, "new.lua"));
        assert!(lookup(&index, "old.lua").is_empty());
        assert_eq!(lookup(&index, "new.lua"), vec![source]);

        index.remove(source);
        index.add_dependency_site(site(Some(target), "resolved.lua"));
        assert!(lookup(&index, "resolved.lua").is_empty());
        index.remove(target);
        assert_eq!(lookup(&index, "resolved.lua"), vec![source]);

        index.clear();
        assert!(lookup(&index, "resolved.lua").is_empty());
    }

    #[test]
    fn unresolved_reverse_lookup_does_not_visit_unrelated_sites() {
        let mut index = LuaDependencyIndex::new();
        for id in 0..2_000 {
            let path = format!("unrelated/{id}.lua");
            index.add_dependency_site(LuaDependencySite {
                source_file_id: FileId::new(id),
                target_file_id: None,
                kind: LuaDependencyKind::Include,
                path: Some(path.clone()),
                path_keys: vec![path],
                original_expr: "include(...)".to_string(),
                call_range: TextRange::new(TextSize::new(0), TextSize::new(1)),
                range: TextRange::new(TextSize::new(0), TextSize::new(1)),
            });
        }
        let matched = FileId::new(2_001);
        index.add_dependency_site(LuaDependencySite {
            source_file_id: matched,
            target_file_id: None,
            kind: LuaDependencyKind::Include,
            path: Some("target.lua".to_string()),
            path_keys: vec!["target.lua".to_string()],
            original_expr: "include(...)".to_string(),
            call_range: TextRange::new(TextSize::new(0), TextSize::new(1)),
            range: TextRange::new(TextSize::new(0), TextSize::new(1)),
        });

        assert_eq!(
            index.collect_unresolved_path_dependents(["target.lua".to_string()]),
            vec![matched]
        );
    }

    #[test]
    fn resolved_target_removal_visits_only_reverse_indexed_sites() {
        let mut index = LuaDependencyIndex::new();
        let range = TextRange::new(TextSize::new(0), TextSize::new(1));
        for id in 0..2_000 {
            let path = format!("unrelated/{id}.lua");
            index.add_dependency_site(LuaDependencySite {
                source_file_id: FileId::new(id),
                target_file_id: Some(FileId::new(10_000 + id)),
                kind: LuaDependencyKind::Include,
                path: Some(path.clone()),
                path_keys: vec![path],
                original_expr: "include(...)".to_string(),
                call_range: range,
                range,
            });
        }

        let matched_source = FileId::new(2_001);
        let matched_target = FileId::new(12_001);
        index.add_dependency_site(LuaDependencySite {
            source_file_id: matched_source,
            target_file_id: Some(matched_target),
            kind: LuaDependencyKind::CompileFile,
            path: Some("target.lua".to_string()),
            path_keys: vec!["target.lua".to_string()],
            original_expr: "CompileFile(... )()".to_string(),
            call_range: range,
            range,
        });

        index.remove(matched_target);

        assert_eq!(index.target_transition_site_visits(), 1);
        assert_eq!(
            index.get_dependency_sites(&matched_source).unwrap()[0].target_file_id,
            None
        );
        assert_eq!(
            index.get_dependency_sites(&FileId::new(1_999)).unwrap()[0].target_file_id,
            Some(FileId::new(11_999))
        );
    }
}
