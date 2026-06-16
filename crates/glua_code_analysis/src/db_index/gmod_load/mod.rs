use std::collections::HashMap;

use rowan::TextRange;

use crate::FileId;

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
}

#[derive(Debug, Default)]
pub struct GmodLoadIndex {
    file_infos: HashMap<FileId, GmodFileLoadInfo>,
    unresolved_edges: Vec<GmodLoadEdge>,
}

impl GmodLoadIndex {
    pub fn new() -> Self {
        Self {
            file_infos: HashMap::new(),
            unresolved_edges: Vec::new(),
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
        self.unresolved_edges
            .retain(|edge| edge.source_file_id != file_id && edge.target_file_id != Some(file_id));
        for info in self.file_infos.values_mut() {
            info.incoming_edges.retain(|edge| {
                edge.source_file_id != file_id && edge.target_file_id != Some(file_id)
            });
        }
    }

    fn clear(&mut self) {
        self.file_infos.clear();
        self.unresolved_edges.clear();
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
