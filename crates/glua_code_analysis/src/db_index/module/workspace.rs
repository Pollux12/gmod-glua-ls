use std::{fmt, path::PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkspaceKind {
    Std,
    Main,
    Remote,
    Library,
}

impl WorkspaceKind {
    /// Rank for a file that belongs to no known workspace, which sorts after
    /// every kind [`Self::merge_rank`] ranks.
    pub const MERGE_RANK_NONE: u8 = 4;

    /// Where this workspace's contribution sorts when several files document
    /// one class.
    ///
    /// Annotations and other libraries are the baseline a workspace refines, so
    /// they merge before the main workspace and the main workspace's wording
    /// wins. Ties fall through to the file's own ordering key.
    pub fn merge_rank(self) -> u8 {
        match self {
            WorkspaceKind::Std => 0,
            WorkspaceKind::Library => 1,
            WorkspaceKind::Remote => 2,
            WorkspaceKind::Main => 3,
        }
    }
}

#[derive(Debug)]
pub struct Workspace {
    pub root: PathBuf,
    pub id: WorkspaceId,
    pub kind: WorkspaceKind,
}

impl Workspace {
    pub fn new(root: PathBuf, id: WorkspaceId, kind: WorkspaceKind) -> Self {
        Self { root, id, kind }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkspaceId {
    pub id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WorkspaceResolutionKey {
    tier: u8,
    library_order: usize,
    workspace_id: u32,
}

impl WorkspaceResolutionKey {
    pub(crate) fn new(tier: u8, library_order: usize, workspace_id: WorkspaceId) -> Self {
        Self {
            tier,
            library_order,
            workspace_id: workspace_id.id,
        }
    }

    pub(crate) fn broad_tier(self) -> u8 {
        self.tier
    }
}

#[allow(unused)]
impl WorkspaceId {
    pub const STD: WorkspaceId = WorkspaceId { id: 0 };
    pub const MAIN: WorkspaceId = WorkspaceId { id: 1 };
    pub const REMOTE: WorkspaceId = WorkspaceId { id: 2 };

    pub fn is_library(&self) -> bool {
        self.id > 2
    }

    pub fn is_remote(&self) -> bool {
        self.id == 2
    }

    pub fn is_main(&self) -> bool {
        self.id == 1
    }

    pub fn is_std(&self) -> bool {
        self.id == 0
    }
}

impl PartialOrd for WorkspaceId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for WorkspaceId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.id {
            0 => write!(f, "std"),
            1 => write!(f, "main"),
            2 => write!(f, "remote"),
            _ => write!(f, "lib{}", self.id - 2),
        }
    }
}
