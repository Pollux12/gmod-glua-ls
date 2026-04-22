use std::{
    collections::{HashMap, HashSet},
    sync::OnceLock,
};

use glua_parser::LuaSyntaxId;
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

#[derive(Debug, Default)]
pub struct GmodSystemAggregate {
    known_net_messages: HashSet<String>,
    net_registration_count: HashMap<String, usize>,
    concommand_registration_count: HashMap<String, usize>,
    convar_registration_count: HashMap<String, usize>,
}

impl GmodSystemAggregate {
    pub fn is_known_net_message(&self, name: &str) -> bool {
        self.known_net_messages.contains(name)
    }

    pub fn net_registration_count(&self, name: &str) -> usize {
        self.net_registration_count.get(name).copied().unwrap_or(0)
    }

    pub fn concommand_registration_count(&self, name: &str) -> usize {
        self.concommand_registration_count
            .get(name)
            .copied()
            .unwrap_or(0)
    }

    pub fn convar_registration_count(&self, name: &str) -> usize {
        self.convar_registration_count
            .get(name)
            .copied()
            .unwrap_or(0)
    }
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

/// Cached scoped class detection result: (class_name, global_name).
#[derive(Debug, Clone)]
pub struct GmodScopedClassInfo {
    pub class_name: String,
    pub global_name: String,
    /// The scope's `classNamePrefix` (if any), cached so downstream synthesis
    /// (e.g. parent-name alias for gamemodes) can strip it back off without a
    /// second path scan.
    pub class_name_prefix: Option<String>,
}

#[derive(Debug, Default)]
pub struct GmodInferIndex {
    hook_file_metadata: HashMap<FileId, GmodHookFileMetadata>,
    system_file_metadata: HashMap<FileId, GmodSystemFileMetadata>,
    system_aggregate_cache: OnceLock<GmodSystemAggregate>,
    realm_file_metadata: HashMap<FileId, GmodRealmFileMetadata>,
    gm_method_realm_annotations: HashMap<FileId, Vec<(String, GmodRealm)>>,
    /// `---@realm` ranges over function decls, sorted by start offset.
    /// Used by narrow + diagnostics for O(log n) realm lookup per member.
    member_realm_ranges: HashMap<FileId, Vec<GmodRealmRange>>,
    /// Pre-indexed @fileparam annotations per file: (param_name_lowercase, type_text)
    fileparam_index: HashMap<FileId, Vec<(String, String)>>,
    /// Cached scoped class detection results, computed once during gmod_pre.
    scoped_class_info: HashMap<FileId, GmodScopedClassInfo>,
}

impl GmodInferIndex {
    pub fn new() -> Self {
        Self {
            hook_file_metadata: HashMap::new(),
            system_file_metadata: HashMap::new(),
            system_aggregate_cache: OnceLock::new(),
            realm_file_metadata: HashMap::new(),
            gm_method_realm_annotations: HashMap::new(),
            member_realm_ranges: HashMap::new(),
            fileparam_index: HashMap::new(),
            scoped_class_info: HashMap::new(),
        }
    }

    fn invalidate_system_aggregate_cache(&mut self) {
        let _ = self.system_aggregate_cache.take();
    }

    fn build_system_aggregate(&self) -> GmodSystemAggregate {
        let mut aggregate = GmodSystemAggregate::default();

        for metadata in self.system_file_metadata.values() {
            for site in &metadata.net_add_string_calls {
                if let Some(name) = normalize_system_name(site.name.as_deref()) {
                    aggregate.known_net_messages.insert(name.to_string());
                    *aggregate
                        .net_registration_count
                        .entry(name.to_string())
                        .or_insert(0) += 1;
                }
            }

            for site in &metadata.concommand_add_calls {
                if let Some(name) = normalize_system_name(site.command_name.as_deref()) {
                    *aggregate
                        .concommand_registration_count
                        .entry(name.to_string())
                        .or_insert(0) += 1;
                }
            }

            for site in &metadata.convar_create_calls {
                if let Some(name) = normalize_system_name(site.convar_name.as_deref()) {
                    *aggregate
                        .convar_registration_count
                        .entry(name.to_string())
                        .or_insert(0) += 1;
                }
            }
        }

        aggregate
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
        self.invalidate_system_aggregate_cache();
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
        self.invalidate_system_aggregate_cache();
        self.system_file_metadata
            .entry(file_id)
            .or_default()
            .concommand_add_calls
            .push(site);
    }

    pub fn add_convar_site(&mut self, file_id: FileId, site: GmodConVarSiteMetadata) {
        self.invalidate_system_aggregate_cache();
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

    pub fn get_system_aggregate(&self) -> &GmodSystemAggregate {
        self.system_aggregate_cache
            .get_or_init(|| self.build_system_aggregate())
    }

    pub fn get_realm_file_metadata(&self, file_id: &FileId) -> Option<&GmodRealmFileMetadata> {
        self.realm_file_metadata.get(file_id)
    }

    /// Get the effective realm at a specific text offset within a file.
    /// If the offset is inside a branch-narrowed block, returns that block's realm.
    /// Otherwise returns the file-level inferred realm, or annotation realm if inferred is Unknown.
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
        // If inferred realm is known, use it
        if metadata.inferred_realm != GmodRealm::Unknown {
            return metadata.inferred_realm;
        }
        // Fall back to annotation realm (for meta/annotation files)
        metadata.annotation_realm.unwrap_or(GmodRealm::Unknown)
    }

    pub fn set_all_realm_file_metadata(
        &mut self,
        metadata: HashMap<FileId, GmodRealmFileMetadata>,
    ) {
        self.realm_file_metadata = metadata;
    }

    pub fn set_gm_method_realm_annotations(
        &mut self,
        file_id: FileId,
        method_realms: Vec<(String, GmodRealm)>,
    ) {
        if method_realms.is_empty() {
            self.gm_method_realm_annotations.remove(&file_id);
            return;
        }

        self.gm_method_realm_annotations
            .insert(file_id, method_realms);
    }

    pub fn iter_gm_method_realm_annotations(
        &self,
    ) -> impl Iterator<Item = (&FileId, &Vec<(String, GmodRealm)>)> {
        self.gm_method_realm_annotations.iter()
    }

    /// Store per-file member realm ranges. Empty Vec clears. Sorted by start.
    pub fn set_member_realm_ranges(&mut self, file_id: FileId, mut ranges: Vec<GmodRealmRange>) {
        if ranges.is_empty() {
            self.member_realm_ranges.remove(&file_id);
            return;
        }
        ranges.sort_by_key(|r| r.range.start());
        self.member_realm_ranges.insert(file_id, ranges);
    }

    /// Look up the `---@realm` covering a member decl at `offset`. O(log n).
    pub fn get_member_annotation_realm_at_offset(
        &self,
        file_id: &FileId,
        offset: rowan::TextSize,
    ) -> Option<GmodRealm> {
        let ranges = self.member_realm_ranges.get(file_id)?;
        let idx = match ranges.binary_search_by_key(&offset, |r| r.range.start()) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        for i in [idx, idx.saturating_sub(1)] {
            if let Some(r) = ranges.get(i)
                && r.range.contains(offset)
            {
                return Some(r.realm);
            }
        }
        None
    }

    pub fn set_file_params(&mut self, file_id: FileId, params: Vec<(String, String)>) {
        if !params.is_empty() {
            self.fileparam_index.insert(file_id, params);
        }
    }

    /// O(1) lookup for a @fileparam type text by file and parameter name.
    pub fn get_file_param_type_text(&self, file_id: &FileId, name_lowercase: &str) -> Option<&str> {
        let params = self.fileparam_index.get(file_id)?;
        params
            .iter()
            .find(|(n, _)| n == name_lowercase)
            .map(|(_, t)| t.as_str())
    }

    pub fn set_scoped_class_info(&mut self, file_id: FileId, info: GmodScopedClassInfo) {
        self.scoped_class_info.insert(file_id, info);
    }

    pub fn get_scoped_class_info(&self, file_id: &FileId) -> Option<&GmodScopedClassInfo> {
        self.scoped_class_info.get(file_id)
    }
}

impl LuaIndex for GmodInferIndex {
    fn remove(&mut self, file_id: FileId) {
        self.hook_file_metadata.remove(&file_id);
        self.system_file_metadata.remove(&file_id);
        self.invalidate_system_aggregate_cache();
        self.realm_file_metadata.remove(&file_id);
        self.gm_method_realm_annotations.remove(&file_id);
        self.member_realm_ranges.remove(&file_id);
        self.fileparam_index.remove(&file_id);
        self.scoped_class_info.remove(&file_id);
    }

    fn clear(&mut self) {
        self.hook_file_metadata.clear();
        self.system_file_metadata.clear();
        self.invalidate_system_aggregate_cache();
        self.realm_file_metadata.clear();
        self.gm_method_realm_annotations.clear();
        self.member_realm_ranges.clear();
        self.fileparam_index.clear();
        self.scoped_class_info.clear();
    }
}

fn normalize_system_name(name: Option<&str>) -> Option<&str> {
    let name = name?;
    let trimmed = name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}
