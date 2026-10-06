use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::db_index::cow_shards::CowCell;
use rowan::TextRange;

use crate::{
    FileId, LuaDeclId,
    db_index::derivation::{self, FactKey, Fingerprint},
};

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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

impl std::hash::Hash for GmodLoadEdge {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        use crate::db_index::derivation::FilePosition;
        let Self {
            source_file_id,
            target_file_id,
            kind,
            states,
            path,
            original_expr,
            range,
        } = self;
        source_file_id.hash(state);
        target_file_id.hash(state);
        kind.hash(state);
        states.hash(state);
        path.hash(state);
        original_expr.hash(state);
        range.hash_in_file(*source_file_id, state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

    /// Content fingerprint; `edges` and each site's `fields` are hashed
    /// independently of their hash order.
    pub(crate) fn fingerprint(&self) -> Fingerprint {
        let edges = self
            .edges
            .iter()
            .fold(0, |sum: Fingerprint, (decl_id, targets)| {
                sum.wrapping_add(derivation::fingerprint_of(&(decl_id, targets)))
            });
        let sites = self
            .sites
            .iter()
            .map(|site| {
                let mut fields = site.fields.iter().collect::<Vec<_>>();
                fields.sort_unstable();
                let source = match &site.source {
                    GmodExecutionEnvironmentSource::Decl(decl_id) => (Some(*decl_id), None),
                    GmodExecutionEnvironmentSource::Path(path) => (None, Some(path)),
                };
                (source, fields)
            })
            .collect::<Vec<_>>();
        derivation::fingerprint_of(&(edges, &self.seeds, sites))
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

#[derive(Debug, Default, Clone)]
pub struct GmodLoadIndex {
    file_infos: CowCell<HashMap<FileId, GmodFileLoadInfo>>,
    unresolved_edges: CowCell<Vec<GmodLoadEdge>>,
    /// Source file -> the files, sorted, whose incoming edges it holds the
    /// call of.
    edge_targets_by_source: CowCell<HashMap<FileId, Vec<FileId>>>,
    /// Source file -> where its edges sit in `unresolved_edges`.
    unresolved_by_source: CowCell<HashMap<FileId, Vec<usize>>>,
    execution_environments: CowCell<HashMap<FileId, HashSet<String>>>,
    execution_environment_sites: CowCell<HashMap<FileId, HashMap<FileId, HashSet<String>>>>,
}

impl GmodLoadIndex {
    pub fn new() -> Self {
        Self {
            file_infos: Default::default(),
            unresolved_edges: Default::default(),
            edge_targets_by_source: Default::default(),
            unresolved_by_source: Default::default(),
            execution_environments: Default::default(),
            execution_environment_sites: Default::default(),
        }
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        let FactKey::GmodLoadFile(file_id) = key else {
            debug_assert!(false, "{key:?} is not a gmod load fact");
            return derivation::ABSENT;
        };
        let info = self.file_infos.get(file_id);
        let environment = self.execution_environments.get(file_id).map(|fields| {
            let mut fields = fields.iter().collect::<Vec<_>>();
            fields.sort_unstable();
            fields
        });
        if info.is_none() && environment.is_none() {
            return derivation::ABSENT;
        }
        derivation::fingerprint_of(&(info, environment))
    }

    /// Notes a write of every file key this index currently holds.
    fn note_all_file_writes(&self) {
        if !derivation::recording() {
            return;
        }
        for file_id in self
            .file_infos
            .keys()
            .chain(self.execution_environments.keys())
        {
            derivation::note_write(|| FactKey::GmodLoadFile(*file_id));
        }
    }

    pub fn get_file_info(&self, file_id: &FileId) -> Option<&GmodFileLoadInfo> {
        derivation::note_read(|| FactKey::GmodLoadFile(*file_id));
        self.file_infos.get(file_id)
    }

    pub fn iter_file_infos(&self) -> impl Iterator<Item = (&FileId, &GmodFileLoadInfo)> {
        self.file_infos
            .iter()
            .inspect(|(file_id, _)| derivation::note_read(|| FactKey::GmodLoadFile(**file_id)))
    }

    pub fn unresolved_edges(&self) -> &[GmodLoadEdge] {
        for edge in &self.unresolved_edges {
            derivation::note_read(|| FactKey::GmodLoadFile(edge.source_file_id));
        }
        &self.unresolved_edges
    }

    pub fn execution_environment_contains(&self, file_id: FileId, name: &str) -> bool {
        derivation::note_read(|| FactKey::GmodLoadFile(file_id));
        self.execution_environments
            .get(&file_id)
            .is_some_and(|fields| fields.contains(name))
    }

    pub fn set_execution_environment_sites(
        &mut self,
        execution_environment_sites: HashMap<FileId, HashMap<FileId, HashSet<String>>>,
    ) {
        self.note_all_file_writes();
        self.execution_environment_sites = execution_environment_sites.into();
        self.rebuild_execution_environments();
        self.note_all_file_writes();
    }

    fn rebuild_execution_environments(&mut self) {
        // Order-free by construction: this is a set union into `HashSet<String>`
        // per target, so the hash order of `execution_environment_sites` cannot
        // reach the result.
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
        derivation::note_read(|| FactKey::GmodLoadFile(left));
        derivation::note_read(|| FactKey::GmodLoadFile(right));
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
                derivation::note_read(|| FactKey::GmodLoadFile(*file_id));
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
        self.note_all_file_writes();
        let mut edge_targets_by_source: HashMap<FileId, Vec<FileId>> = HashMap::default();
        for (target, info) in &file_infos {
            for edge in &info.incoming_edges {
                edge_targets_by_source
                    .entry(edge.source_file_id)
                    .or_default()
                    .push(*target);
            }
        }
        for targets in edge_targets_by_source.values_mut() {
            targets.sort_unstable();
            targets.dedup();
        }
        let mut unresolved_by_source: HashMap<FileId, Vec<usize>> = HashMap::default();
        for (at, edge) in unresolved_edges.iter().enumerate() {
            unresolved_by_source
                .entry(edge.source_file_id)
                .or_default()
                .push(at);
        }
        self.file_infos = file_infos.into();
        self.unresolved_edges = unresolved_edges.into();
        self.edge_targets_by_source = edge_targets_by_source.into();
        self.unresolved_by_source = unresolved_by_source.into();
        self.note_all_file_writes();
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

impl GmodLoadIndex {
    /// Moves the call sites of the load edges `remap`'s file holds to their
    /// new place. A call the edit destroyed changes the file's load scan, so
    /// the load fold runs again and replaces the edges.
    pub(crate) fn remap_edge_positions(&mut self, remap: &crate::FileRemap) {
        let remap_edge = |edge: &mut GmodLoadEdge| {
            if edge.source_file_id != remap.file_id {
                return false;
            }
            let Some(range) = edge.range else {
                return false;
            };
            let moved = remap.map.map_range(range);
            let changed = moved != Some(range);
            edge.range = moved;
            changed
        };
        let mut changed = Vec::new();
        let targets = self
            .edge_targets_by_source
            .get(&remap.file_id)
            .cloned()
            .unwrap_or_default();
        for file_id in targets {
            let Some(info) = self.file_infos.get_mut(&file_id) else {
                continue;
            };
            let mut moved = false;
            for edge in &mut info.incoming_edges {
                moved |= remap_edge(edge);
            }
            if moved {
                changed.push(file_id);
            }
        }
        let unresolved = self
            .unresolved_by_source
            .get(&remap.file_id)
            .cloned()
            .unwrap_or_default();
        for at in unresolved {
            remap_edge(&mut self.unresolved_edges[at]);
        }
        for file_id in changed {
            derivation::note_write(|| FactKey::GmodLoadFile(file_id));
        }
    }
}

/// The load fold's output only: it replaces all of it when it runs, so a
/// removed file's entries stay until then.
impl LuaIndex for GmodLoadIndex {
    fn remove(&mut self, _file_id: FileId) {}

    fn clear(&mut self) {
        self.note_all_file_writes();
        self.file_infos.clear();
        self.unresolved_edges.clear();
        self.edge_targets_by_source.clear();
        self.unresolved_by_source.clear();
        self.execution_environments.clear();
        self.execution_environment_sites.clear();
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

    use rowan::TextRange;

    use super::{GmodFileLoadInfo, GmodLoadEdge, GmodLoadEdgeKind, GmodLoadIndex, GmodStateMask};
    use crate::{FileId, FileRemap, PositionMap, db_index::LuaIndex};

    fn include_edge(source_file_id: FileId, target_file_id: FileId) -> GmodLoadEdge {
        GmodLoadEdge {
            source_file_id,
            target_file_id: Some(target_file_id),
            kind: GmodLoadEdgeKind::Include,
            states: GmodStateMask::SHARED,
            path: None,
            original_expr: None,
            range: Some(TextRange::new(2.into(), 6.into())),
        }
    }

    /// The load fold replaces this index whole when it runs, so a re-indexed
    /// file's edges stay for it to keep or replace.
    #[test]
    fn removing_a_file_keeps_its_load_edges_for_the_fold() {
        let (source, target) = (FileId::new(1), FileId::new(2));
        let mut index = GmodLoadIndex::new();
        let mut info = GmodFileLoadInfo::fallback_shared();
        info.incoming_edges.push(include_edge(source, target));
        index.file_infos.insert(target, info);

        index.remove(source);

        let info = index
            .get_file_info(&target)
            .expect("the target's info is kept");
        assert_eq!(info.incoming_edges, vec![include_edge(source, target)]);
    }

    /// An edit above a kept edge's call moves the call's range with it.
    #[test]
    fn an_edit_moves_a_kept_edge_to_its_call() {
        let (source, target) = (FileId::new(1), FileId::new(2));
        let mut index = GmodLoadIndex::new();
        let mut info = GmodFileLoadInfo::fallback_shared();
        info.incoming_edges.push(include_edge(source, target));
        index.set_all_file_infos(
            rustc_hash::FxHashMap::from_iter([(target, info)]),
            Vec::new(),
        );

        index.remap_edge_positions(&FileRemap::unvalidated(
            source,
            PositionMap::new(
                "abcdefgh",
                "
abcdefgh",
            ),
        ));

        let range = index
            .get_file_info(&target)
            .expect("the target's info is kept")
            .incoming_edges[0]
            .range;
        assert_eq!(range, Some(TextRange::new(3.into(), 7.into())));
    }

    /// An edit above an unresolved edge's call moves its range too.
    #[test]
    fn an_edit_moves_an_unresolved_edge_to_its_call() {
        let source = FileId::new(1);
        let mut edge = include_edge(source, FileId::new(2));
        edge.target_file_id = None;
        let mut index = GmodLoadIndex::new();
        index.set_all_file_infos(rustc_hash::FxHashMap::default(), vec![edge]);

        index.remap_edge_positions(&FileRemap::unvalidated(
            source,
            PositionMap::new("abcdefgh", "\nabcdefgh"),
        ));

        assert_eq!(
            index.unresolved_edges()[0].range,
            Some(TextRange::new(3.into(), 7.into()))
        );
    }

    #[test]
    fn clearing_load_index_drops_execution_environments() {
        let source = FileId::new(1);
        let target = FileId::new(2);
        let mut index = GmodLoadIndex::new();
        index.set_execution_environment_sites(HashMap::from_iter([(
            source,
            HashMap::from_iter([(target, HashSet::from_iter(["simple".to_string()]))]),
        )]));

        index.clear();

        assert!(!index.execution_environment_contains(target, "simple"));
    }
}

crate::db_index::cow_shards::task_fork_fields!(GmodLoadIndex {
    file_infos,
    unresolved_edges,
    edge_targets_by_source,
    unresolved_by_source,
    execution_environments,
    execution_environment_sites,
});
