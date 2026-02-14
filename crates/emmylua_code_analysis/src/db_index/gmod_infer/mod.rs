use std::collections::HashMap;

use emmylua_parser::LuaSyntaxId;
use rowan::TextRange;

use super::LuaIndex;
use crate::FileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GmodRealm {
    Client,
    Server,
    Shared,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GmodHookKind {
    Add,
    GamemodeMethod,
    Emit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GmodHookNameIssue {
    Empty,
    NonStringLiteral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GmodConVarKind {
    Server,
    Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GmodTimerKind {
    Create,
    Simple,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodHookSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub kind: GmodHookKind,
    pub hook_name: Option<String>,
    pub name_range: Option<TextRange>,
    pub name_issue: Option<GmodHookNameIssue>,
    pub callback_params: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodNamedSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub name: Option<String>,
    pub name_range: Option<TextRange>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GmodCallbackSiteMetadata {
    pub syntax_id: Option<LuaSyntaxId>,
    pub callback_range: Option<TextRange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodNetReceiveSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub message_name: Option<String>,
    pub name_range: Option<TextRange>,
    pub callback: GmodCallbackSiteMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodConcommandSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub command_name: Option<String>,
    pub name_range: Option<TextRange>,
    pub callback: GmodCallbackSiteMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodConVarSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub kind: GmodConVarKind,
    pub convar_name: Option<String>,
    pub name_range: Option<TextRange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodTimerSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub kind: GmodTimerKind,
    pub timer_name: Option<String>,
    pub name_range: Option<TextRange>,
    pub callback: GmodCallbackSiteMetadata,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GmodHookFileMetadata {
    pub sites: Vec<GmodHookSiteMetadata>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GmodSystemFileMetadata {
    pub net_add_string_calls: Vec<GmodNamedSiteMetadata>,
    pub net_start_calls: Vec<GmodNamedSiteMetadata>,
    pub net_receive_calls: Vec<GmodNetReceiveSiteMetadata>,
    pub concommand_add_calls: Vec<GmodConcommandSiteMetadata>,
    pub convar_create_calls: Vec<GmodConVarSiteMetadata>,
    pub timer_calls: Vec<GmodTimerSiteMetadata>,
}

/// A range within a file that has a narrowed realm (from `if CLIENT then` etc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodRealmRange {
    pub range: TextRange,
    pub realm: GmodRealm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodRealmFileMetadata {
    pub inferred_realm: GmodRealm,
    pub filename_hint: Option<GmodRealm>,
    pub dependency_hints: Vec<GmodRealm>,
    /// Realm set explicitly via `---@realm client|server|shared`.
    pub annotation_realm: Option<GmodRealm>,
    /// Block-level realm narrowing from `if CLIENT then`/`if SERVER then`.
    pub branch_realm_ranges: Vec<GmodRealmRange>,
}

impl Default for GmodRealmFileMetadata {
    fn default() -> Self {
        Self {
            inferred_realm: GmodRealm::Unknown,
            filename_hint: None,
            dependency_hints: Vec::new(),
            annotation_realm: None,
            branch_realm_ranges: Vec::new(),
        }
    }
}

#[derive(Debug, Default)]
pub struct GmodInferIndex {
    hook_file_metadata: HashMap<FileId, GmodHookFileMetadata>,
    system_file_metadata: HashMap<FileId, GmodSystemFileMetadata>,
    realm_file_metadata: HashMap<FileId, GmodRealmFileMetadata>,
}

impl GmodInferIndex {
    pub fn new() -> Self {
        Self {
            hook_file_metadata: HashMap::new(),
            system_file_metadata: HashMap::new(),
            realm_file_metadata: HashMap::new(),
        }
    }

    pub fn add_hook_site(&mut self, file_id: FileId, site: GmodHookSiteMetadata) {
        self.hook_file_metadata
            .entry(file_id)
            .or_default()
            .sites
            .push(site);
    }

    pub fn get_hook_file_metadata(&self, file_id: &FileId) -> Option<&GmodHookFileMetadata> {
        self.hook_file_metadata.get(file_id)
    }

    pub fn iter_hook_file_metadata(
        &self,
    ) -> impl Iterator<Item = (&FileId, &GmodHookFileMetadata)> {
        self.hook_file_metadata.iter()
    }

    pub fn add_net_message_registration(&mut self, file_id: FileId, site: GmodNamedSiteMetadata) {
        self.system_file_metadata
            .entry(file_id)
            .or_default()
            .net_add_string_calls
            .push(site);
    }

    pub fn add_net_start_site(&mut self, file_id: FileId, site: GmodNamedSiteMetadata) {
        self.system_file_metadata
            .entry(file_id)
            .or_default()
            .net_start_calls
            .push(site);
    }

    pub fn add_net_receive_site(&mut self, file_id: FileId, site: GmodNetReceiveSiteMetadata) {
        self.system_file_metadata
            .entry(file_id)
            .or_default()
            .net_receive_calls
            .push(site);
    }

    pub fn add_concommand_site(&mut self, file_id: FileId, site: GmodConcommandSiteMetadata) {
        self.system_file_metadata
            .entry(file_id)
            .or_default()
            .concommand_add_calls
            .push(site);
    }

    pub fn add_convar_site(&mut self, file_id: FileId, site: GmodConVarSiteMetadata) {
        self.system_file_metadata
            .entry(file_id)
            .or_default()
            .convar_create_calls
            .push(site);
    }

    pub fn add_timer_site(&mut self, file_id: FileId, site: GmodTimerSiteMetadata) {
        self.system_file_metadata
            .entry(file_id)
            .or_default()
            .timer_calls
            .push(site);
    }

    pub fn get_system_file_metadata(&self, file_id: &FileId) -> Option<&GmodSystemFileMetadata> {
        self.system_file_metadata.get(file_id)
    }

    pub fn iter_system_file_metadata(
        &self,
    ) -> impl Iterator<Item = (&FileId, &GmodSystemFileMetadata)> {
        self.system_file_metadata.iter()
    }

    pub fn get_realm_file_metadata(&self, file_id: &FileId) -> Option<&GmodRealmFileMetadata> {
        self.realm_file_metadata.get(file_id)
    }

    /// Get the effective realm at a specific text offset within a file.
    /// If the offset is inside a branch-narrowed block, returns that block's realm.
    /// Otherwise returns the file-level inferred realm.
    pub fn get_realm_at_offset(&self, file_id: &FileId, offset: rowan::TextSize) -> GmodRealm {
        let Some(metadata) = self.realm_file_metadata.get(file_id) else {
            return GmodRealm::Unknown;
        };
        // Check branch ranges (most specific first)
        for range in &metadata.branch_realm_ranges {
            if range.range.contains(offset) {
                return range.realm;
            }
        }
        metadata.inferred_realm
    }

    pub fn set_all_realm_file_metadata(
        &mut self,
        metadata: HashMap<FileId, GmodRealmFileMetadata>,
    ) {
        self.realm_file_metadata = metadata;
    }
}

impl LuaIndex for GmodInferIndex {
    fn remove(&mut self, file_id: FileId) {
        self.hook_file_metadata.remove(&file_id);
        self.system_file_metadata.remove(&file_id);
        self.realm_file_metadata.remove(&file_id);
    }

    fn clear(&mut self) {
        self.hook_file_metadata.clear();
        self.system_file_metadata.clear();
        self.realm_file_metadata.clear();
    }
}
