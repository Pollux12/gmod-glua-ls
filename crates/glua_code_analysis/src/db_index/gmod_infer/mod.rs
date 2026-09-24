use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::sync::OnceLock;

use glua_parser::LuaSyntaxId;
use rowan::TextRange;

use super::{GmodLoadIndex, GmodLoadStatus, GmodStateMask, LuaIndex};
use crate::{
    FileId,
    db_index::derivation::{self, FactKey, Fingerprint},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GmodRealm {
    Client,
    Server,
    Shared,
    Menu,
    Unknown,
}

impl GmodRealm {
    pub fn state_mask(self) -> GmodStateMask {
        GmodStateMask::from_realm(self)
    }

    pub fn is_compatible_with(self, other: Self) -> bool {
        self.state_mask().is_compatible_with(other.state_mask())
    }

    pub fn is_strictly_incompatible_with(self, other: Self) -> bool {
        self.state_mask()
            .is_strictly_incompatible_with(other.state_mask())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GmodHookKind {
    Add,
    GamemodeMethod,
    Emit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GmodHookNameIssue {
    Empty,
    NonStringLiteral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GmodConVarKind {
    Server,
    Client,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GmodTimerKind {
    Create,
    Simple,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GmodHookSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub kind: GmodHookKind,
    pub hook_name: Option<String>,
    pub name_range: Option<TextRange>,
    pub name_issue: Option<GmodHookNameIssue>,
    pub callback_arg_idx: Option<usize>,
    pub callback_params: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GmodNamedSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub name: Option<String>,
    pub name_range: Option<TextRange>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct GmodCallbackSiteMetadata {
    pub syntax_id: Option<LuaSyntaxId>,
    pub callback_range: Option<TextRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GmodNetReceiveSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub message_name: Option<String>,
    pub name_range: Option<TextRange>,
    pub callback: GmodCallbackSiteMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GmodConcommandSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub command_name: Option<String>,
    pub name_range: Option<TextRange>,
    pub callback: GmodCallbackSiteMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GmodConVarSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub kind: GmodConVarKind,
    pub convar_name: Option<String>,
    pub name_range: Option<TextRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GmodTimerSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub kind: GmodTimerKind,
    pub timer_name: Option<String>,
    pub name_range: Option<TextRange>,
    pub callback: GmodCallbackSiteMetadata,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct GmodHookFileMetadata {
    pub sites: Vec<GmodHookSiteMetadata>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct GmodSystemFileMetadata {
    pub net_add_string_calls: Vec<GmodNamedSiteMetadata>,
    pub net_start_calls: Vec<GmodNamedSiteMetadata>,
    pub net_receive_calls: Vec<GmodNetReceiveSiteMetadata>,
    pub concommand_add_calls: Vec<GmodConcommandSiteMetadata>,
    pub convar_create_calls: Vec<GmodConVarSiteMetadata>,
    pub timer_calls: Vec<GmodTimerSiteMetadata>,
}

#[derive(Debug, Default, Clone)]
pub struct GmodSystemAggregate {
    known_net_messages: HashSet<String>,
    net_registration_count: HashMap<String, usize>,
    concommand_registration_count: HashMap<String, usize>,
    convar_registration_count: HashMap<String, usize>,
    duplicate_registrations:
        HashMap<(GmodSystemRegistrationKind, String), Vec<GmodSystemRegistration>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GmodSystemRegistrationKind {
    NetMessage,
    Concommand,
    Convar,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodSystemRegistration {
    pub kind: GmodSystemRegistrationKind,
    pub convar_kind: Option<GmodConVarKind>,
    pub name: String,
    pub file_id: FileId,
    pub name_range: TextRange,
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

    pub fn convar_registrations(&self, name: &str) -> &[GmodSystemRegistration] {
        self.registrations(GmodSystemRegistrationKind::Convar, name)
    }

    fn add_registration(
        &mut self,
        kind: GmodSystemRegistrationKind,
        convar_kind: Option<GmodConVarKind>,
        name: &str,
        file_id: FileId,
        name_range: TextRange,
    ) {
        let registrations = self
            .duplicate_registrations
            .entry((kind, name.to_string()))
            .or_default();
        registrations.push(GmodSystemRegistration {
            kind,
            convar_kind,
            name: name.to_string(),
            file_id,
            name_range,
        });
        // Which registration a duplicate report calls the original is read off
        // this list, so it has to come from source position rather than from
        // the order the files happened to be analysed in.
        registrations
            .sort_by_key(|registration| (registration.file_id, registration.name_range.start()));
    }

    pub fn registrations(
        &self,
        kind: GmodSystemRegistrationKind,
        name: &str,
    ) -> &[GmodSystemRegistration] {
        self.duplicate_registrations
            .get(&(kind, name.to_string()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// A range within a file that has a narrowed realm (from `if CLIENT then` etc).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GmodRealmRange {
    pub range: TextRange,
    pub realm: GmodRealm,
}

/// A file's own realm evidence, before the workspace load graph combines it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct GmodRealmInputs {
    pub branch_realm_ranges: Vec<GmodRealmRange>,
    pub annotation_realm: Option<GmodRealm>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GmodRealmFileMetadata {
    pub inferred_realm: GmodRealm,
    pub load_realm: Option<GmodRealm>,
    pub load_status: Option<GmodLoadStatus>,
    pub load_state_mask: GmodStateMask,
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
            load_realm: None,
            load_status: None,
            load_state_mask: GmodStateMask::empty(),
            filename_hint: None,
            dependency_hints: Vec::new(),
            annotation_realm: None,
            branch_realm_ranges: Vec::new(),
        }
    }
}

/// Cached scoped class detection result and its configured semantic metadata.
#[derive(Debug, Clone, Hash)]
pub struct GmodScopedClassInfo {
    pub class_name: String,
    pub global_name: String,
    pub is_global_singleton: bool,
    pub aliases: Vec<String>,
    pub super_types: Vec<String>,
    /// The scope's `classNamePrefix` (if any), cached so downstream synthesis
    /// (e.g. parent-name alias for gamemodes) can strip it back off without a
    /// second path scan.
    pub class_name_prefix: Option<String>,
}

/// Workspace-global registry of helper function definitions, used as a
/// fallback when same-file helper resolution doesn't find a definition.
///
/// Keyed by:
/// - bare-name globals: `"helperFn"` for `function helperFn() end` or
///   `helperFn = function() end` at module top-level
/// - dotted globals: `"Module.fn"` for `function Module.fn() end` or
///   `Module.fn = function() end`
///
/// Locals are intentionally excluded — they can only be referenced from
/// within their defining file, so the same-file lookup already covers them.
#[derive(Debug, Default, Clone)]
pub struct GmodInferIndex {
    hook_file_metadata: HashMap<FileId, GmodHookFileMetadata>,
    system_file_metadata: HashMap<FileId, GmodSystemFileMetadata>,
    system_aggregate_cache: OnceLock<GmodSystemAggregate>,
    realm_file_metadata: HashMap<FileId, GmodRealmFileMetadata>,
    /// Each file's own realm evidence: `if CLIENT`/`if SERVER` block ranges
    /// and its `---@realm` annotation. The workspace realm derivation reads
    /// these to produce `realm_file_metadata`.
    realm_inputs: HashMap<FileId, GmodRealmInputs>,
    gm_method_realm_annotations: HashMap<FileId, Vec<(String, GmodRealm)>>,
    /// Per-file `---@realm` ranges over function decls (sorted, O(log n) lookup).
    member_realm_ranges: HashMap<FileId, Vec<GmodRealmRange>>,
    /// Pre-indexed @fileparam annotations per file: (param_name_lowercase, type_text)
    fileparam_index: HashMap<FileId, Vec<(String, String)>>,
    /// Cached scoped class detection results, computed once during gmod_pre.
    scoped_class_info: HashMap<FileId, GmodScopedClassInfo>,
    /// Incremental multiset fingerprint of the hook sites, system sites and
    /// gamemode-method realm annotations of every file.
    scan_fingerprint: Fingerprint,
}

impl GmodInferIndex {
    pub fn new() -> Self {
        Self {
            hook_file_metadata: HashMap::default(),
            system_file_metadata: HashMap::default(),
            system_aggregate_cache: OnceLock::new(),
            realm_file_metadata: HashMap::default(),
            realm_inputs: HashMap::default(),
            gm_method_realm_annotations: HashMap::default(),
            member_realm_ranges: HashMap::default(),
            fileparam_index: HashMap::default(),
            scoped_class_info: HashMap::default(),
            scan_fingerprint: 0,
        }
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::GmodInferFile(file_id) => {
                let slots = (
                    self.hook_file_metadata.get(file_id),
                    self.system_file_metadata.get(file_id),
                    self.realm_inputs.get(file_id),
                    self.gm_method_realm_annotations.get(file_id),
                    self.member_realm_ranges.get(file_id),
                    self.fileparam_index.get(file_id),
                    self.scoped_class_info.get(file_id),
                );
                if slots.0.is_none()
                    && slots.1.is_none()
                    && slots.2.is_none()
                    && slots.3.is_none()
                    && slots.4.is_none()
                    && slots.5.is_none()
                    && slots.6.is_none()
                {
                    derivation::ABSENT
                } else {
                    derivation::fingerprint_of(&slots)
                }
            }
            FactKey::GmodRealm(file_id) => {
                derivation::fingerprint_opt(self.realm_file_metadata.get(file_id))
            }
            FactKey::GmodInferScan => self.scan_fingerprint,
            _ => {
                debug_assert!(false, "{key:?} is not a gmod infer fact");
                derivation::ABSENT
            }
        }
    }

    fn note_scan_write(&mut self, file_id: FileId, added: Fingerprint, removed: Fingerprint) {
        derivation::note_write(|| FactKey::GmodInferFile(file_id));
        derivation::note_write(|| FactKey::GmodInferScan);
        self.scan_fingerprint = self
            .scan_fingerprint
            .wrapping_add(added)
            .wrapping_sub(removed);
    }

    fn invalidate_system_aggregate_cache(&mut self) {
        let _ = self.system_aggregate_cache.take();
    }

    fn build_system_aggregate(&self) -> GmodSystemAggregate {
        let mut aggregate = GmodSystemAggregate::default();

        for (file_id, metadata) in &self.system_file_metadata {
            for site in &metadata.net_add_string_calls {
                if let Some(name) = normalize_system_name(site.name.as_deref()) {
                    aggregate.known_net_messages.insert(name.to_string());
                    *aggregate
                        .net_registration_count
                        .entry(name.to_string())
                        .or_insert(0) += 1;
                    if let Some(name_range) = site.name_range {
                        aggregate.add_registration(
                            GmodSystemRegistrationKind::NetMessage,
                            None,
                            name,
                            *file_id,
                            name_range,
                        );
                    }
                }
            }

            for site in &metadata.concommand_add_calls {
                if let Some(name) = normalize_system_name(site.command_name.as_deref()) {
                    *aggregate
                        .concommand_registration_count
                        .entry(name.to_string())
                        .or_insert(0) += 1;
                    if let Some(name_range) = site.name_range {
                        aggregate.add_registration(
                            GmodSystemRegistrationKind::Concommand,
                            None,
                            name,
                            *file_id,
                            name_range,
                        );
                    }
                }
            }

            for site in &metadata.convar_create_calls {
                if let Some(name) = normalize_system_name(site.convar_name.as_deref()) {
                    *aggregate
                        .convar_registration_count
                        .entry(name.to_string())
                        .or_insert(0) += 1;
                    if let Some(name_range) = site.name_range {
                        aggregate.add_registration(
                            GmodSystemRegistrationKind::Convar,
                            Some(site.kind),
                            name,
                            *file_id,
                            name_range,
                        );
                    }
                }
            }
        }

        aggregate
    }

    pub fn add_hook_site(&mut self, file_id: FileId, site: GmodHookSiteMetadata) {
        self.note_scan_write(file_id, scan_element(HOOK_SITE, file_id, &site), 0);
        self.hook_file_metadata
            .entry(file_id)
            .or_default()
            .sites
            .push(site);
    }

    /// Bulk-add hook sites collected for a file (preserving order). Used by the
    /// parallel gmod-pre collection, which gathers a file's sites off-thread and
    /// merges them sequentially. No-op when `sites` is empty.
    pub fn add_hook_sites(&mut self, file_id: FileId, sites: Vec<GmodHookSiteMetadata>) {
        if sites.is_empty() {
            return;
        }
        let added = sites.iter().fold(0, |sum: Fingerprint, site| {
            sum.wrapping_add(scan_element(HOOK_SITE, file_id, site))
        });
        self.note_scan_write(file_id, added, 0);
        self.hook_file_metadata
            .entry(file_id)
            .or_default()
            .sites
            .extend(sites);
    }

    /// Replace a file's collected system metadata (net/concommand/convar/timer
    /// sites) wholesale. Used by the parallel gmod-pre collection. Invalidates
    /// the system aggregate cache since registration counts may change.
    pub fn set_system_file_metadata(&mut self, file_id: FileId, metadata: GmodSystemFileMetadata) {
        let removed = self
            .system_file_metadata
            .get(&file_id)
            .map_or(0, |previous| system_scan_fingerprint(file_id, previous));
        self.note_scan_write(
            file_id,
            system_scan_fingerprint(file_id, &metadata),
            removed,
        );
        if metadata == GmodSystemFileMetadata::default() {
            if self.system_file_metadata.remove(&file_id).is_some() {
                self.invalidate_system_aggregate_cache();
            }
            return;
        }
        self.invalidate_system_aggregate_cache();
        self.system_file_metadata.insert(file_id, metadata);
    }

    pub fn get_hook_file_metadata(&self, file_id: &FileId) -> Option<&GmodHookFileMetadata> {
        derivation::note_read(|| FactKey::GmodInferFile(*file_id));
        self.hook_file_metadata.get(file_id)
    }

    pub fn iter_hook_file_metadata(
        &self,
    ) -> impl Iterator<Item = (&FileId, &GmodHookFileMetadata)> {
        derivation::note_read(|| FactKey::GmodInferScan);
        self.hook_file_metadata.iter()
    }

    pub fn add_net_message_registration(&mut self, file_id: FileId, site: GmodNamedSiteMetadata) {
        self.note_scan_write(file_id, scan_element(NET_ADD_STRING, file_id, &site), 0);
        self.invalidate_system_aggregate_cache();
        self.system_file_metadata
            .entry(file_id)
            .or_default()
            .net_add_string_calls
            .push(site);
    }

    pub fn add_net_start_site(&mut self, file_id: FileId, site: GmodNamedSiteMetadata) {
        self.note_scan_write(file_id, scan_element(NET_START, file_id, &site), 0);
        self.system_file_metadata
            .entry(file_id)
            .or_default()
            .net_start_calls
            .push(site);
    }

    pub fn add_net_receive_site(&mut self, file_id: FileId, site: GmodNetReceiveSiteMetadata) {
        self.note_scan_write(file_id, scan_element(NET_RECEIVE, file_id, &site), 0);
        self.system_file_metadata
            .entry(file_id)
            .or_default()
            .net_receive_calls
            .push(site);
    }

    pub fn add_concommand_site(&mut self, file_id: FileId, site: GmodConcommandSiteMetadata) {
        self.note_scan_write(file_id, scan_element(CONCOMMAND, file_id, &site), 0);
        self.invalidate_system_aggregate_cache();
        self.system_file_metadata
            .entry(file_id)
            .or_default()
            .concommand_add_calls
            .push(site);
    }

    pub fn add_convar_site(&mut self, file_id: FileId, site: GmodConVarSiteMetadata) {
        self.note_scan_write(file_id, scan_element(CONVAR, file_id, &site), 0);
        self.invalidate_system_aggregate_cache();
        self.system_file_metadata
            .entry(file_id)
            .or_default()
            .convar_create_calls
            .push(site);
    }

    pub fn add_timer_site(&mut self, file_id: FileId, site: GmodTimerSiteMetadata) {
        self.note_scan_write(file_id, scan_element(TIMER, file_id, &site), 0);
        self.system_file_metadata
            .entry(file_id)
            .or_default()
            .timer_calls
            .push(site);
    }

    pub fn get_system_file_metadata(&self, file_id: &FileId) -> Option<&GmodSystemFileMetadata> {
        derivation::note_read(|| FactKey::GmodInferFile(*file_id));
        self.system_file_metadata.get(file_id)
    }

    pub fn iter_system_file_metadata(
        &self,
    ) -> impl Iterator<Item = (&FileId, &GmodSystemFileMetadata)> {
        derivation::note_read(|| FactKey::GmodInferScan);
        self.system_file_metadata.iter()
    }

    pub fn get_system_aggregate(&self) -> &GmodSystemAggregate {
        derivation::note_read(|| FactKey::GmodInferScan);
        self.system_aggregate_cache
            .get_or_init(|| self.build_system_aggregate())
    }

    pub fn has_compatible_duplicate_registration(
        &self,
        load_index: &GmodLoadIndex,
        kind: GmodSystemRegistrationKind,
        name: &str,
        file_id: &FileId,
        name_range: TextRange,
    ) -> bool {
        self.get_system_aggregate()
            .registrations(kind, name)
            .iter()
            .any(|registration| {
                (registration.file_id != *file_id || registration.name_range != name_range)
                    && !load_index.files_are_mutually_exclusive_by_load_shadowing(
                        *file_id,
                        registration.file_id,
                    )
                    && self.are_offsets_compatible(
                        file_id,
                        name_range.start(),
                        &registration.file_id,
                        registration.name_range.start(),
                    )
            })
    }

    pub fn get_realm_file_metadata(&self, file_id: &FileId) -> Option<&GmodRealmFileMetadata> {
        derivation::note_read(|| FactKey::GmodRealm(*file_id));
        self.realm_file_metadata.get(file_id)
    }

    /// Get the effective realm at a specific text offset within a file.
    /// If the offset is inside a branch-narrowed block, returns that block's realm.
    /// Otherwise returns the file-level inferred realm, or annotation realm if inferred is Unknown.
    pub fn get_realm_at_offset(&self, file_id: &FileId, offset: rowan::TextSize) -> GmodRealm {
        derivation::note_read(|| FactKey::GmodRealm(*file_id));
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

    pub fn get_state_mask_at_offset(
        &self,
        file_id: &FileId,
        offset: rowan::TextSize,
    ) -> GmodStateMask {
        derivation::note_read(|| FactKey::GmodRealm(*file_id));
        let Some(metadata) = self.realm_file_metadata.get(file_id) else {
            return GmodStateMask::empty();
        };
        let file_mask = file_state_mask(metadata);

        for range in &metadata.branch_realm_ranges {
            if range.range.contains(offset) {
                let branch_mask = range.realm.state_mask();
                if file_mask.is_empty() {
                    return branch_mask;
                }

                return branch_mask.intersection(file_mask);
            }
        }

        file_mask
    }

    pub fn are_state_masks_compatible(left: GmodStateMask, right: GmodStateMask) -> bool {
        left.is_compatible_with(right)
    }

    pub fn are_realms_compatible(left: GmodRealm, right: GmodRealm) -> bool {
        left.is_compatible_with(right)
    }

    pub fn are_offsets_compatible(
        &self,
        left_file_id: &FileId,
        left_offset: rowan::TextSize,
        right_file_id: &FileId,
        right_offset: rowan::TextSize,
    ) -> bool {
        Self::are_state_masks_compatible(
            self.get_state_mask_at_offset(left_file_id, left_offset),
            self.get_state_mask_at_offset(right_file_id, right_offset),
        )
    }

    pub fn set_all_realm_file_metadata(
        &mut self,
        metadata: HashMap<FileId, GmodRealmFileMetadata>,
    ) {
        if derivation::recording() {
            for file_id in self.realm_file_metadata.keys().chain(metadata.keys()) {
                derivation::note_write(|| FactKey::GmodRealm(*file_id));
            }
        }
        self.realm_file_metadata = metadata;
    }

    pub fn set_realm_inputs(&mut self, file_id: FileId, inputs: GmodRealmInputs) {
        derivation::note_write(|| FactKey::GmodInferFile(file_id));
        self.realm_inputs.insert(file_id, inputs);
    }

    pub fn get_realm_inputs(&self, file_id: &FileId) -> Option<&GmodRealmInputs> {
        derivation::note_read(|| FactKey::GmodInferFile(*file_id));
        self.realm_inputs.get(file_id)
    }

    pub fn set_gm_method_realm_annotations(
        &mut self,
        file_id: FileId,
        method_realms: Vec<(String, GmodRealm)>,
    ) {
        let removed = self
            .gm_method_realm_annotations
            .get(&file_id)
            .map_or(0, |previous| gm_method_scan_fingerprint(file_id, previous));
        self.note_scan_write(
            file_id,
            gm_method_scan_fingerprint(file_id, &method_realms),
            removed,
        );
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
        derivation::note_read(|| FactKey::GmodInferScan);
        self.gm_method_realm_annotations.iter()
    }

    /// Records this file's `---@realm` decl ranges, sorted.
    ///
    /// An empty vector is recorded rather than dropped: the entry is what says
    /// the file was scanned, and a file with no annotation is the common case,
    /// not an absent answer. Dropping it made
    /// [`has_member_realm_ranges`](Self::has_member_realm_ranges) report the
    /// scanned-and-empty file as never scanned, which is the one thing it exists
    /// to tell apart. Removal of the file's entries is
    /// [`LuaIndex::remove`](crate::LuaIndex::remove)'s job.
    pub fn set_member_realm_ranges(&mut self, file_id: FileId, mut ranges: Vec<GmodRealmRange>) {
        derivation::note_write(|| FactKey::GmodInferFile(file_id));
        ranges.sort_by_key(|r| r.range.start());
        self.member_realm_ranges.insert(file_id, ranges);
    }

    /// Returns `true` if gmod_pre already populated member realm ranges for this file.
    /// Use as a guard before calling `get_member_annotation_realm_at_offset` to distinguish
    /// "no ranges for this file" (pre ran, file had none) from "pre not yet run" (fall back).
    pub fn has_member_realm_ranges(&self, file_id: &FileId) -> bool {
        derivation::note_read(|| FactKey::GmodInferFile(*file_id));
        self.member_realm_ranges.contains_key(file_id)
    }

    /// `---@realm` covering decl at `offset`. O(log n).
    pub fn get_member_annotation_realm_at_offset(
        &self,
        file_id: &FileId,
        offset: rowan::TextSize,
    ) -> Option<GmodRealm> {
        derivation::note_read(|| FactKey::GmodInferFile(*file_id));
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
            derivation::note_write(|| FactKey::GmodInferFile(file_id));
            self.fileparam_index.insert(file_id, params);
        }
    }

    /// O(1) lookup for a @fileparam type text by file and parameter name.
    pub fn get_file_param_type_text(&self, file_id: &FileId, name_lowercase: &str) -> Option<&str> {
        derivation::note_read(|| FactKey::GmodInferFile(*file_id));
        let params = self.fileparam_index.get(file_id)?;
        params
            .iter()
            .find(|(n, _)| n == name_lowercase)
            .map(|(_, t)| t.as_str())
    }

    pub fn set_scoped_class_info(&mut self, file_id: FileId, info: GmodScopedClassInfo) {
        derivation::note_write(|| FactKey::GmodInferFile(file_id));
        self.scoped_class_info.insert(file_id, info);
    }

    pub fn get_scoped_class_info(&self, file_id: &FileId) -> Option<&GmodScopedClassInfo> {
        derivation::note_read(|| FactKey::GmodInferFile(*file_id));
        self.scoped_class_info.get(file_id)
    }

    /// Drops what a file's gmod pre-analysis derived, before it runs again:
    /// everything but the source layer's scoped-class info and the realms
    /// the load graph decided.
    pub(crate) fn reset_file_metadata(&mut self, file_id: FileId) {
        let removed = self
            .hook_file_metadata
            .remove(&file_id)
            .map_or(0, |metadata| hook_scan_fingerprint(file_id, &metadata))
            .wrapping_add(
                self.system_file_metadata
                    .remove(&file_id)
                    .map_or(0, |metadata| system_scan_fingerprint(file_id, &metadata)),
            )
            .wrapping_add(
                self.gm_method_realm_annotations
                    .get(&file_id)
                    .map_or(0, |method_realms| {
                        gm_method_scan_fingerprint(file_id, method_realms)
                    }),
            );
        self.note_scan_write(file_id, 0, removed);
        self.invalidate_system_aggregate_cache();
        derivation::note_write(|| FactKey::GmodInferFile(file_id));
        self.realm_inputs.remove(&file_id);
        self.gm_method_realm_annotations.remove(&file_id);
        self.member_realm_ranges.remove(&file_id);
        self.fileparam_index.remove(&file_id);
    }
}

impl LuaIndex for GmodInferIndex {
    fn remove(&mut self, file_id: FileId) {
        self.reset_file_metadata(file_id);
        derivation::note_write(|| FactKey::GmodRealm(file_id));
        self.realm_file_metadata.remove(&file_id);
        self.scoped_class_info.remove(&file_id);
    }

    fn clear(&mut self) {
        if derivation::recording() {
            for file_id in self
                .hook_file_metadata
                .keys()
                .chain(self.system_file_metadata.keys())
                .chain(self.realm_file_metadata.keys())
                .chain(self.gm_method_realm_annotations.keys())
                .chain(self.member_realm_ranges.keys())
                .chain(self.fileparam_index.keys())
                .chain(self.scoped_class_info.keys())
            {
                derivation::note_write(|| FactKey::GmodInferFile(*file_id));
                derivation::note_write(|| FactKey::GmodRealm(*file_id));
            }
        }
        derivation::note_write(|| FactKey::GmodInferScan);
        self.scan_fingerprint = 0;
        self.hook_file_metadata.clear();
        self.system_file_metadata.clear();
        self.invalidate_system_aggregate_cache();
        self.realm_file_metadata.clear();
        self.realm_inputs.clear();
        self.gm_method_realm_annotations.clear();
        self.member_realm_ranges.clear();
        self.fileparam_index.clear();
        self.scoped_class_info.clear();
    }
}

fn file_state_mask(metadata: &GmodRealmFileMetadata) -> GmodStateMask {
    if let Some(annotation_realm) = metadata.annotation_realm {
        return annotation_realm.state_mask();
    }
    if !metadata.load_state_mask.is_empty() {
        return metadata.load_state_mask;
    }
    if metadata.inferred_realm != GmodRealm::Unknown {
        return metadata.inferred_realm.state_mask();
    }

    GmodStateMask::empty()
}

const HOOK_SITE: u8 = 0;
const NET_ADD_STRING: u8 = 1;
const NET_START: u8 = 2;
const NET_RECEIVE: u8 = 3;
const CONCOMMAND: u8 = 4;
const CONVAR: u8 = 5;
const TIMER: u8 = 6;
const GM_METHOD_REALM: u8 = 7;

/// One element of the [`FactKey::GmodInferScan`] multiset.
fn scan_element<T: std::hash::Hash>(tag: u8, file_id: FileId, value: &T) -> Fingerprint {
    derivation::fingerprint_of(&(tag, file_id, value))
}

fn scan_elements<T: std::hash::Hash>(tag: u8, file_id: FileId, values: &[T]) -> Fingerprint {
    values.iter().fold(0, |sum: Fingerprint, value| {
        sum.wrapping_add(scan_element(tag, file_id, value))
    })
}

fn hook_scan_fingerprint(file_id: FileId, metadata: &GmodHookFileMetadata) -> Fingerprint {
    scan_elements(HOOK_SITE, file_id, &metadata.sites)
}

fn system_scan_fingerprint(file_id: FileId, metadata: &GmodSystemFileMetadata) -> Fingerprint {
    scan_elements(NET_ADD_STRING, file_id, &metadata.net_add_string_calls)
        .wrapping_add(scan_elements(NET_START, file_id, &metadata.net_start_calls))
        .wrapping_add(scan_elements(
            NET_RECEIVE,
            file_id,
            &metadata.net_receive_calls,
        ))
        .wrapping_add(scan_elements(
            CONCOMMAND,
            file_id,
            &metadata.concommand_add_calls,
        ))
        .wrapping_add(scan_elements(
            CONVAR,
            file_id,
            &metadata.convar_create_calls,
        ))
        .wrapping_add(scan_elements(TIMER, file_id, &metadata.timer_calls))
}

fn gm_method_scan_fingerprint(
    file_id: FileId,
    method_realms: &[(String, GmodRealm)],
) -> Fingerprint {
    scan_elements(GM_METHOD_REALM, file_id, method_realms)
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
