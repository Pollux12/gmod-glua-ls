use std::collections::{HashMap, HashSet};

use rowan::TextRange;

use crate::{FileId, LuaDeclId};

use super::{GmodRealm, LuaDependencyKind, LuaIndex};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct GmodStateMask(u8);

impl GmodStateMask {
    const CLIENT_BIT: u8 = 0b001;
    const SERVER_BIT: u8 = 0b010;
    const MENU_BIT: u8 = 0b100;

    pub const EMPTY: Self = Self(0);
    pub const CLIENT: Self = Self(Self::CLIENT_BIT);
    pub const SERVER: Self = Self(Self::SERVER_BIT);
    pub const MENU: Self = Self(Self::MENU_BIT);
    pub const SHARED: Self = Self(Self::CLIENT_BIT | Self::SERVER_BIT);

    pub fn empty() -> Self {
        Self::EMPTY
    }

    pub fn from_realm(realm: GmodRealm) -> Self {
        match realm {
            GmodRealm::Client => Self::CLIENT,
            GmodRealm::Server => Self::SERVER,
            GmodRealm::Shared => Self::SHARED,
            GmodRealm::Menu => Self::MENU,
            GmodRealm::Unknown => Self::EMPTY,
        }
    }

    pub fn bits(self) -> u8 {
        self.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn intersects(self, other: Self) -> bool {
        (self.0 & other.0) != 0
    }

    pub fn is_compatible_with(self, other: Self) -> bool {
        self.is_empty()
            || other.is_empty()
            || self.intersects(other)
            || self.as_caller_compatibility_mask().intersects(other)
    }

    pub fn is_strictly_incompatible_with(self, other: Self) -> bool {
        !self.is_compatible_with(other)
    }

    pub fn as_caller_compatibility_mask(self) -> Self {
        if self.contains(Self::MENU) {
            self.union(Self::CLIENT)
        } else {
            self
        }
    }

    pub fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn without_menu(self) -> Self {
        Self(self.0 & !Self::MENU_BIT)
    }

    pub fn to_realm(self, fallback: GmodRealm) -> GmodRealm {
        if self.is_empty() {
            return fallback;
        }

        let runtime = self.without_menu();
        if runtime.contains(Self::SHARED) {
            return GmodRealm::Shared;
        }
        if runtime.contains(Self::CLIENT) {
            return GmodRealm::Client;
        }
        if runtime.contains(Self::SERVER) {
            return GmodRealm::Server;
        }
        if self.contains(Self::MENU) {
            return GmodRealm::Menu;
        }

        fallback
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GmodLoadStatus {
    EngineLoaded,
    ReachableByLoadEdge,
    MaybeDynamic,
    NoKnownLoadPath,
    KnownUnloaded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GmodLoadConfidence {
    Fallback,
    Dynamic,
    Static,
    Engine,
    Explicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GmodLoadRootKind {
    IncludesInit,
    IncludesInitMenu,
    DermaInit,
    MenuMain,
    Autorun,
    AutorunClient,
    AutorunServer,
    GamemodeInit,
    GamemodeClientInit,
    GamemodeShared,
    ScriptedEntity,
    ScriptedWeapon,
    ScriptedEffect,
    Stool,
    Vgui,
    PostProcess,
    MatProxy,
    Skin,
    FallbackDefault,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GmodLoadOrderKey {
    pub phase: u16,
    pub path_sort_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GmodLoadEdgeKind {
    Include,
    CompileFile,
    AddCSLuaFile,
    IncludeCS,
    Require,
    WrapperInclude,
    WrapperAddCSLuaFile,
    DynamicInclude,
    DynamicAddCSLuaFile,
    EngineAutoload,
}

impl From<LuaDependencyKind> for GmodLoadEdgeKind {
    fn from(value: LuaDependencyKind) -> Self {
        match value {
            LuaDependencyKind::Require => Self::Require,
            LuaDependencyKind::Include => Self::Include,
            LuaDependencyKind::CompileFile => Self::CompileFile,
            LuaDependencyKind::AddCSLuaFile => Self::AddCSLuaFile,
            LuaDependencyKind::IncludeCS => Self::IncludeCS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodLoadRoot {
    pub kind: GmodLoadRootKind,
    pub states: GmodStateMask,
    pub path_sort_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodLoadEdge {
    pub source_file_id: FileId,
    pub target_file_id: Option<FileId>,
    pub kind: GmodLoadEdgeKind,
    pub states: GmodStateMask,
    pub path: Option<String>,
    pub original_expr: Option<String>,
    pub range: Option<TextRange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodFileLoadInfo {
    pub status: GmodLoadStatus,
    pub realm: GmodRealm,
    pub state_mask: GmodStateMask,
    pub confidence: GmodLoadConfidence,
    pub roots: Vec<GmodLoadRoot>,
    pub incoming_edges: Vec<GmodLoadEdge>,
    pub client_send_available: bool,
    pub shadowed_by: Option<FileId>,
}

impl GmodFileLoadInfo {
    pub fn fallback_shared() -> Self {
        Self {
            status: GmodLoadStatus::NoKnownLoadPath,
            realm: GmodRealm::Shared,
            state_mask: GmodStateMask::EMPTY,
            confidence: GmodLoadConfidence::Fallback,
            roots: Vec::new(),
            incoming_edges: Vec::new(),
            client_send_available: false,
            shadowed_by: None,
        }
    }

    pub fn mark_states(
        &mut self,
        states: GmodStateMask,
        status: GmodLoadStatus,
        confidence: GmodLoadConfidence,
    ) -> bool {
        let previous = self.state_mask;
        self.state_mask.insert(states);
        if status_rank(status) >= status_rank(self.status) {
            self.status = status;
        }
        if confidence > self.confidence {
            self.confidence = confidence;
        }
        self.realm = self.state_mask.to_realm(self.realm);
        previous != self.state_mask
    }

    pub fn add_root(&mut self, root: GmodLoadRoot) {
        if !self.roots.contains(&root) {
            self.roots.push(root);
        }
    }

    pub fn add_incoming_edge(&mut self, edge: GmodLoadEdge) {
        if self.incoming_edges.contains(&edge) {
            return;
        }
        self.incoming_edges.push(edge);
    }

    pub fn mark_shadowed_by(&mut self, winning_file_id: FileId) {
        self.shadowed_by = Some(winning_file_id);
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct GmodExecutionEnvironmentFileFlow {
    pub(crate) edges: HashMap<LuaDeclId, Vec<LuaDeclId>>,
    pub(crate) seeds: Vec<(LuaDeclId, String)>,
    pub(crate) sites: Vec<GmodExecutionEnvironmentSite>,
}

impl GmodExecutionEnvironmentFileFlow {
    pub(crate) fn is_empty(&self) -> bool {
        self.edges.is_empty() && self.seeds.is_empty() && self.sites.is_empty()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct GmodExecutionEnvironmentSite {
    pub(crate) source: GmodExecutionEnvironmentSource,
    pub(crate) fields: HashSet<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum GmodExecutionEnvironmentSource {
    Decl(LuaDeclId),
    Path(String),
}

#[derive(Debug, Default)]
pub struct GmodLoadIndex {
    file_infos: HashMap<FileId, GmodFileLoadInfo>,
    unresolved_edges: Vec<GmodLoadEdge>,
    execution_environments: HashMap<FileId, HashSet<String>>,
    execution_environment_sites: HashMap<FileId, HashMap<FileId, HashSet<String>>>,
    execution_environment_file_flows: HashMap<FileId, GmodExecutionEnvironmentFileFlow>,
    execution_environment_role_sources: HashSet<FileId>,
    execution_environment_roles_dirty: bool,
}

impl GmodLoadIndex {
    pub fn new() -> Self {
        Self {
            file_infos: HashMap::new(),
            unresolved_edges: Vec::new(),
            execution_environments: HashMap::new(),
            execution_environment_sites: HashMap::new(),
            execution_environment_file_flows: HashMap::new(),
            execution_environment_role_sources: HashSet::new(),
            execution_environment_roles_dirty: false,
        }
    }

    pub fn get_file_info(&self, file_id: &FileId) -> Option<&GmodFileLoadInfo> {
        self.file_infos.get(file_id)
    }

    pub fn iter_file_infos(&self) -> impl Iterator<Item = (&FileId, &GmodFileLoadInfo)> {
        self.file_infos.iter()
    }

    pub fn unresolved_edges(&self) -> &[GmodLoadEdge] {
        &self.unresolved_edges
    }

    pub fn execution_environment_contains(&self, file_id: FileId, name: &str) -> bool {
        self.execution_environments
            .get(&file_id)
            .is_some_and(|fields| fields.contains(name))
    }

    pub fn set_execution_environment_sites(
        &mut self,
        execution_environment_sites: HashMap<FileId, HashMap<FileId, HashSet<String>>>,
    ) {
        self.execution_environment_sites = execution_environment_sites;
        self.rebuild_execution_environments();
    }

    pub(crate) fn set_execution_environment_file_flow(
        &mut self,
        file_id: FileId,
        flow: GmodExecutionEnvironmentFileFlow,
    ) {
        if flow.is_empty() {
            self.execution_environment_file_flows.remove(&file_id);
        } else {
            self.execution_environment_file_flows.insert(file_id, flow);
        }
    }

    pub(crate) fn iter_execution_environment_file_flows(
        &self,
    ) -> impl Iterator<Item = (FileId, &GmodExecutionEnvironmentFileFlow)> {
        self.execution_environment_file_flows
            .iter()
            .map(|(file_id, flow)| (*file_id, flow))
    }

    pub(crate) fn execution_environment_roles_changed(
        &self,
        current_role_sources: &HashSet<FileId>,
        analyzed_file_ids: &HashSet<FileId>,
    ) -> bool {
        self.execution_environment_roles_dirty
            || self.execution_environment_role_sources != *current_role_sources
            || analyzed_file_ids.iter().any(|file_id| {
                self.execution_environment_role_sources.contains(file_id)
                    || current_role_sources.contains(file_id)
            })
    }

    pub(crate) fn set_execution_environment_role_sources(&mut self, role_sources: HashSet<FileId>) {
        self.execution_environment_role_sources = role_sources;
        self.execution_environment_roles_dirty = false;
    }

    fn rebuild_execution_environments(&mut self) {
        self.execution_environments.clear();
        for targets in self.execution_environment_sites.values() {
            for (target, fields) in targets {
                self.execution_environments
                    .entry(*target)
                    .or_default()
                    .extend(fields.iter().cloned());
            }
        }
    }

    pub fn files_are_mutually_exclusive_by_load_shadowing(
        &self,
        left: FileId,
        right: FileId,
    ) -> bool {
        let Some(left_info) = self.file_infos.get(&left) else {
            return false;
        };
        let Some(right_info) = self.file_infos.get(&right) else {
            return false;
        };

        left_info.shadowed_by == Some(right) || right_info.shadowed_by == Some(left)
    }

    pub fn engine_roots_in_load_order(
        &self,
        state: GmodStateMask,
    ) -> Vec<(FileId, GmodLoadRootKind, GmodLoadOrderKey)> {
        let mut roots = self
            .file_infos
            .iter()
            .flat_map(|(file_id, info)| {
                info.roots.iter().filter_map(|root| {
                    if !root.states.intersects(state) {
                        return None;
                    }
                    let phase = root.kind.load_order_phase(state, &root.path_sort_key)?;
                    Some((
                        *file_id,
                        root.kind,
                        GmodLoadOrderKey {
                            phase,
                            path_sort_key: root.path_sort_key.clone(),
                        },
                    ))
                })
            })
            .collect::<Vec<_>>();

        roots.sort_by(
            |(left_file_id, left_kind, left_key), (right_file_id, right_kind, right_key)| {
                left_key
                    .cmp(right_key)
                    .then_with(|| {
                        left_kind
                            .load_order_tie_breaker()
                            .cmp(&right_kind.load_order_tie_breaker())
                    })
                    .then_with(|| left_file_id.cmp(right_file_id))
            },
        );
        roots
    }

    pub fn set_all_file_infos(
        &mut self,
        file_infos: HashMap<FileId, GmodFileLoadInfo>,
        unresolved_edges: Vec<GmodLoadEdge>,
    ) {
        self.file_infos = file_infos;
        self.unresolved_edges = unresolved_edges;
    }
}

impl GmodLoadRootKind {
    fn load_order_phase(self, state: GmodStateMask, path_sort_key: &str) -> Option<u16> {
        if state.contains(GmodStateMask::MENU) && !state.intersects(GmodStateMask::SHARED) {
            return self.menu_load_order_phase();
        }
        if state.contains(GmodStateMask::SERVER) && !state.contains(GmodStateMask::CLIENT) {
            return self.server_load_order_phase(path_sort_key);
        }
        self.client_load_order_phase(path_sort_key)
    }

    fn client_load_order_phase(self, path_sort_key: &str) -> Option<u16> {
        match self {
            Self::IncludesInit => Some(10),
            Self::DermaInit => Some(20),
            Self::GamemodeShared if is_base_gamemode_path(path_sort_key) => Some(29),
            Self::GamemodeClientInit if is_base_gamemode_path(path_sort_key) => Some(30),
            Self::Autorun => Some(40),
            Self::AutorunClient => Some(41),
            Self::PostProcess => Some(50),
            Self::Vgui => Some(60),
            Self::MatProxy => Some(70),
            Self::Skin => Some(80),
            Self::GamemodeShared => Some(89),
            Self::GamemodeClientInit => Some(90),
            Self::ScriptedWeapon => Some(100),
            Self::Stool => Some(101),
            Self::ScriptedEntity => Some(110),
            Self::ScriptedEffect => Some(120),
            Self::FallbackDefault => Some(1000),
            _ => None,
        }
    }

    fn server_load_order_phase(self, path_sort_key: &str) -> Option<u16> {
        match self {
            Self::IncludesInit => Some(10),
            Self::GamemodeShared if is_base_gamemode_path(path_sort_key) => Some(19),
            Self::GamemodeInit if is_base_gamemode_path(path_sort_key) => Some(20),
            Self::Autorun => Some(30),
            Self::AutorunServer => Some(31),
            Self::GamemodeShared => Some(49),
            Self::GamemodeInit => Some(50),
            Self::ScriptedWeapon => Some(60),
            Self::Stool => Some(61),
            Self::ScriptedEntity => Some(70),
            Self::ScriptedEffect => Some(80),
            Self::FallbackDefault => Some(1000),
            _ => None,
        }
    }

    fn menu_load_order_phase(self) -> Option<u16> {
        match self {
            Self::IncludesInitMenu => Some(10),
            Self::DermaInit => Some(20),
            Self::MenuMain => Some(30),
            Self::Vgui => Some(40),
            Self::FallbackDefault => Some(1000),
            _ => None,
        }
    }

    fn load_order_tie_breaker(self) -> u16 {
        match self {
            Self::IncludesInit => 0,
            Self::IncludesInitMenu => 1,
            Self::DermaInit => 2,
            Self::MenuMain => 3,
            Self::Autorun => 4,
            Self::AutorunClient => 5,
            Self::AutorunServer => 6,
            Self::GamemodeShared => 7,
            Self::GamemodeInit => 8,
            Self::GamemodeClientInit => 9,
            Self::ScriptedWeapon => 10,
            Self::Stool => 11,
            Self::ScriptedEntity => 12,
            Self::ScriptedEffect => 13,
            Self::Vgui => 14,
            Self::PostProcess => 15,
            Self::MatProxy => 16,
            Self::Skin => 17,
            Self::FallbackDefault => 18,
        }
    }
}

fn is_base_gamemode_path(path_sort_key: &str) -> bool {
    path_sort_key.starts_with("gamemodes/base/gamemode/")
}

impl LuaIndex for GmodLoadIndex {
    fn remove(&mut self, file_id: FileId) {
        self.file_infos.remove(&file_id);
        self.execution_environment_file_flows.remove(&file_id);
        if self.execution_environment_role_sources.remove(&file_id) {
            self.execution_environment_roles_dirty = true;
            self.execution_environment_file_flows.clear();
            self.execution_environment_sites.clear();
            self.execution_environments.clear();
        }
        self.execution_environments.remove(&file_id);
        self.execution_environment_sites.remove(&file_id);
        for targets in self.execution_environment_sites.values_mut() {
            targets.remove(&file_id);
        }
        self.rebuild_execution_environments();
        self.unresolved_edges
            .retain(|edge| edge.source_file_id != file_id && edge.target_file_id != Some(file_id));
        for info in self.file_infos.values_mut() {
            info.incoming_edges.retain(|edge| {
                edge.source_file_id != file_id && edge.target_file_id != Some(file_id)
            });
            if info.shadowed_by == Some(file_id) {
                info.shadowed_by = None;
            }
        }
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        let removed_file_ids = file_ids.iter().copied().collect::<HashSet<_>>();
        self.file_infos
            .retain(|file_id, _| !removed_file_ids.contains(file_id));
        self.execution_environment_file_flows
            .retain(|file_id, _| !removed_file_ids.contains(file_id));
        let previous_role_source_count = self.execution_environment_role_sources.len();
        self.execution_environment_role_sources
            .retain(|file_id| !removed_file_ids.contains(file_id));
        let removed_role_source =
            previous_role_source_count != self.execution_environment_role_sources.len();
        self.execution_environment_roles_dirty |= removed_role_source;
        if removed_role_source {
            self.execution_environment_file_flows.clear();
            self.execution_environment_sites.clear();
            self.execution_environments.clear();
        }
        self.execution_environments
            .retain(|file_id, _| !removed_file_ids.contains(file_id));
        self.execution_environment_sites
            .retain(|source, _| !removed_file_ids.contains(source));
        for targets in self.execution_environment_sites.values_mut() {
            targets.retain(|target, _| !removed_file_ids.contains(target));
        }
        self.rebuild_execution_environments();
        self.unresolved_edges.retain(|edge| {
            !removed_file_ids.contains(&edge.source_file_id)
                && !edge
                    .target_file_id
                    .is_some_and(|target_file_id| removed_file_ids.contains(&target_file_id))
        });
        for info in self.file_infos.values_mut() {
            info.incoming_edges.retain(|edge| {
                !removed_file_ids.contains(&edge.source_file_id)
                    && !edge
                        .target_file_id
                        .is_some_and(|target_file_id| removed_file_ids.contains(&target_file_id))
            });
            if info
                .shadowed_by
                .is_some_and(|file_id| removed_file_ids.contains(&file_id))
            {
                info.shadowed_by = None;
            }
        }
    }

    fn clear(&mut self) {
        self.file_infos.clear();
        self.unresolved_edges.clear();
        self.execution_environments.clear();
        self.execution_environment_sites.clear();
        self.execution_environment_file_flows.clear();
        self.execution_environment_role_sources.clear();
        self.execution_environment_roles_dirty = false;
    }
}

fn status_rank(status: GmodLoadStatus) -> u8 {
    match status {
        GmodLoadStatus::KnownUnloaded => 0,
        GmodLoadStatus::NoKnownLoadPath => 1,
        GmodLoadStatus::MaybeDynamic => 2,
        GmodLoadStatus::ReachableByLoadEdge => 3,
        GmodLoadStatus::EngineLoaded => 4,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::{GmodFileLoadInfo, GmodLoadEdge, GmodLoadEdgeKind, GmodLoadIndex, GmodStateMask};
    use crate::{FileId, db_index::LuaIndex};

    #[test]
    fn batch_removal_keeps_only_surviving_load_edges() {
        let removed = FileId::new(1);
        let other_removed = FileId::new(2);
        let surviving = FileId::new(3);
        let external = FileId::new(4);
        let edge = |source_file_id, target_file_id| GmodLoadEdge {
            source_file_id,
            target_file_id,
            kind: GmodLoadEdgeKind::Include,
            states: GmodStateMask::SHARED,
            path: None,
            original_expr: None,
            range: None,
        };
        let mut index = GmodLoadIndex::new();
        let mut info = GmodFileLoadInfo::fallback_shared();
        info.incoming_edges.push(edge(external, Some(surviving)));
        info.incoming_edges.push(edge(removed, Some(surviving)));
        info.shadowed_by = Some(removed);
        index.file_infos.insert(surviving, info);
        index.unresolved_edges.extend([
            edge(surviving, Some(external)),
            edge(surviving, Some(removed)),
        ]);

        index.remove_files(&[other_removed, removed, other_removed]);

        let info = index.get_file_info(&surviving).unwrap();
        assert_eq!(info.incoming_edges, vec![edge(external, Some(surviving))]);
        assert_eq!(info.shadowed_by, None);
        assert_eq!(index.unresolved_edges(), &[edge(surviving, Some(external))]);
    }

    #[test]
    fn removing_environment_source_drops_its_target_fields() {
        let source = FileId::new(1);
        let target = FileId::new(2);
        let mut index = GmodLoadIndex::new();
        index.set_execution_environment_sites(HashMap::from([(
            source,
            HashMap::from([(target, HashSet::from(["simple".to_string()]))]),
        )]));

        index.remove(source);

        assert!(!index.execution_environment_contains(target, "simple"));
    }

    #[test]
    fn clearing_load_index_drops_execution_environments() {
        let source = FileId::new(1);
        let target = FileId::new(2);
        let mut index = GmodLoadIndex::new();
        index.set_execution_environment_sites(HashMap::from([(
            source,
            HashMap::from([(target, HashSet::from(["simple".to_string()]))]),
        )]));

        index.clear();

        assert!(!index.execution_environment_contains(target, "simple"));
    }
}
