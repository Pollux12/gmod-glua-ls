use crate::db_index::cow_shards::{CowCell, CowShards};
use crate::db_index::derivation::hash_local;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

use glua_parser::LuaSyntaxId;
use rowan::TextRange;
use smol_str::SmolStr;

use super::{GmodLoadIndex, GmodLoadStatus, GmodStateMask, LuaIndex};
use crate::{
    FileId, InFiled,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodHookSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub kind: GmodHookKind,
    pub hook_name: Option<String>,
    pub name_range: Option<TextRange>,
    pub name_issue: Option<GmodHookNameIssue>,
    pub callback_arg_idx: Option<usize>,
    pub callback_params: Vec<String>,
}

impl Hash for GmodHookSiteMetadata {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self {
            syntax_id,
            kind,
            hook_name,
            name_range,
            name_issue,
            callback_arg_idx,
            callback_params,
        } = self;
        hash_local(syntax_id, state);
        kind.hash(state);
        hook_name.hash(state);
        hash_local(name_range, state);
        name_issue.hash(state);
        callback_arg_idx.hash(state);
        callback_params.hash(state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodNamedSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub name: Option<String>,
    pub name_range: Option<TextRange>,
}

impl Hash for GmodNamedSiteMetadata {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self {
            syntax_id,
            name,
            name_range,
        } = self;
        hash_local(syntax_id, state);
        name.hash(state);
        hash_local(name_range, state);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GmodCallbackSiteMetadata {
    pub syntax_id: Option<LuaSyntaxId>,
    pub callback_range: Option<TextRange>,
}

impl Hash for GmodCallbackSiteMetadata {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self {
            syntax_id,
            callback_range,
        } = self;
        hash_local(syntax_id, state);
        hash_local(callback_range, state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodNetReceiveSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub message_name: Option<String>,
    pub name_range: Option<TextRange>,
    pub callback: GmodCallbackSiteMetadata,
}

impl Hash for GmodNetReceiveSiteMetadata {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self {
            syntax_id,
            message_name,
            name_range,
            callback,
        } = self;
        hash_local(syntax_id, state);
        message_name.hash(state);
        hash_local(name_range, state);
        callback.hash(state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodConcommandSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub command_name: Option<String>,
    pub name_range: Option<TextRange>,
    pub callback: GmodCallbackSiteMetadata,
}

impl Hash for GmodConcommandSiteMetadata {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self {
            syntax_id,
            command_name,
            name_range,
            callback,
        } = self;
        hash_local(syntax_id, state);
        command_name.hash(state);
        hash_local(name_range, state);
        callback.hash(state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodConVarSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub kind: GmodConVarKind,
    pub convar_name: Option<String>,
    pub name_range: Option<TextRange>,
}

impl Hash for GmodConVarSiteMetadata {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self {
            syntax_id,
            kind,
            convar_name,
            name_range,
        } = self;
        hash_local(syntax_id, state);
        kind.hash(state);
        convar_name.hash(state);
        hash_local(name_range, state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodTimerSiteMetadata {
    pub syntax_id: LuaSyntaxId,
    pub kind: GmodTimerKind,
    pub timer_name: Option<String>,
    pub name_range: Option<TextRange>,
    pub callback: GmodCallbackSiteMetadata,
}

impl Hash for GmodTimerSiteMetadata {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self {
            syntax_id,
            kind,
            timer_name,
            name_range,
            callback,
        } = self;
        hash_local(syntax_id, state);
        kind.hash(state);
        timer_name.hash(state);
        hash_local(name_range, state);
        callback.hash(state);
    }
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

/// Records a read of the registrations of `kind` under `name`.
fn note_system_read(kind: GmodSystemRegistrationKind, name: &str) {
    derivation::note_read(|| FactKey::GmodSystem(kind, SmolStr::new(name)));
}

/// Records writes of the registrations `metadata` makes.
fn note_system_writes(metadata: &GmodSystemFileMetadata) {
    let names = metadata
        .net_add_string_calls
        .iter()
        .map(|site| (GmodSystemRegistrationKind::NetMessage, site.name.as_deref()))
        .chain(metadata.concommand_add_calls.iter().map(|site| {
            (
                GmodSystemRegistrationKind::Concommand,
                site.command_name.as_deref(),
            )
        }))
        .chain(metadata.convar_create_calls.iter().map(|site| {
            (
                GmodSystemRegistrationKind::Convar,
                site.convar_name.as_deref(),
            )
        }));
    for (kind, name) in names {
        if let Some(name) = normalize_system_name(name) {
            derivation::note_write(|| FactKey::GmodSystem(kind, SmolStr::new(name)));
        }
    }
}

impl GmodSystemAggregate {
    pub fn is_known_net_message(&self, name: &str) -> bool {
        note_system_read(GmodSystemRegistrationKind::NetMessage, name);
        self.known_net_messages.contains(name)
    }

    pub fn net_registration_count(&self, name: &str) -> usize {
        note_system_read(GmodSystemRegistrationKind::NetMessage, name);
        self.net_registration_count.get(name).copied().unwrap_or(0)
    }

    pub fn concommand_registration_count(&self, name: &str) -> usize {
        note_system_read(GmodSystemRegistrationKind::Concommand, name);
        self.concommand_registration_count
            .get(name)
            .copied()
            .unwrap_or(0)
    }

    pub fn convar_registration_count(&self, name: &str) -> usize {
        note_system_read(GmodSystemRegistrationKind::Convar, name);
        self.convar_registration_count
            .get(name)
            .copied()
            .unwrap_or(0)
    }

    /// The fingerprint of the registrations of `kind` under `name`.
    fn entry_fingerprint(&self, kind: GmodSystemRegistrationKind, name: &str) -> Fingerprint {
        let count = match kind {
            GmodSystemRegistrationKind::NetMessage => self.net_registration_count.get(name),
            GmodSystemRegistrationKind::Concommand => self.concommand_registration_count.get(name),
            GmodSystemRegistrationKind::Convar => self.convar_registration_count.get(name),
        };
        let registrations = self
            .duplicate_registrations
            .get(&(kind, name.to_string()))
            .map(|registrations| {
                registrations
                    .iter()
                    .map(|registration| {
                        (
                            registration.convar_kind,
                            InFiled::new(registration.file_id, registration.name_range),
                        )
                    })
                    .collect::<Vec<_>>()
            });
        if count.is_none() && registrations.is_none() {
            return derivation::ABSENT;
        }
        derivation::fingerprint_of(&(count, registrations))
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
        note_system_read(kind, name);
        self.duplicate_registrations
            .get(&(kind, name.to_string()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// A range within a file that has a narrowed realm (from `if CLIENT then` etc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmodRealmRange {
    pub range: TextRange,
    pub realm: GmodRealm,
}

impl Hash for GmodRealmRange {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self { range, realm } = self;
        hash_local(range, state);
        realm.hash(state);
    }
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    hook_file_metadata: CowShards<FileId, GmodHookFileMetadata>,
    system_file_metadata: CowShards<FileId, GmodSystemFileMetadata>,
    system_aggregate_cache: OnceLock<Arc<GmodSystemAggregate>>,
    realm_file_metadata: CowShards<FileId, GmodRealmFileMetadata>,
    /// Each file's own realm evidence: `if CLIENT`/`if SERVER` block ranges
    /// and its `---@realm` annotation. The workspace realm derivation reads
    /// these to produce `realm_file_metadata`.
    realm_inputs: CowShards<FileId, GmodRealmInputs>,
    gm_method_realm_annotations: CowShards<FileId, Vec<(String, GmodRealm)>>,
    /// Per-file `---@realm` ranges over function decls (sorted, O(log n) lookup).
    member_realm_ranges: CowShards<FileId, Vec<GmodRealmRange>>,
    /// Pre-indexed @fileparam annotations per file: (param_name_lowercase, type_text)
    fileparam_index: CowCell<HashMap<FileId, Vec<(String, String)>>>,
    /// Cached scoped class detection results, computed once during gmod_pre.
    scoped_class_info: CowCell<HashMap<FileId, GmodScopedClassInfo>>,
    /// Incremental multiset fingerprint of the hook sites, system sites and
    /// gamemode-method realm annotations of every file.
    scan_fingerprint: Fingerprint,
}

impl GmodInferIndex {
    pub fn new() -> Self {
        Self {
            hook_file_metadata: Default::default(),
            system_file_metadata: Default::default(),
            system_aggregate_cache: Default::default(),
            realm_file_metadata: Default::default(),
            realm_inputs: Default::default(),
            gm_method_realm_annotations: Default::default(),
            member_realm_ranges: Default::default(),
            fileparam_index: Default::default(),
            scoped_class_info: Default::default(),
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
            FactKey::GmodRealmInputs(file_id) => {
                derivation::fingerprint_opt(self.realm_inputs.get(file_id))
            }
            FactKey::GmodMemberRealmAt(file_id, offset) => derivation::fingerprint_of(
                &member_realm_at(self.member_realm_ranges.get(file_id), *offset),
            ),
            FactKey::GmodRealmAt(file_id, offset) => {
                let metadata = self.realm_file_metadata.get(file_id);
                derivation::fingerprint_of(&(
                    realm_at(metadata, *offset),
                    state_mask_at(metadata, *offset),
                ))
            }
            FactKey::GmodInferScan if derivation::mapping_back() => {
                self.recomputed_scan_fingerprint()
            }
            FactKey::GmodInferScan => self.scan_fingerprint,
            FactKey::GmodSystem(kind, name) => {
                self.get_system_aggregate().entry_fingerprint(*kind, name)
            }
            _ => {
                debug_assert!(false, "{key:?} is not a gmod infer fact");
                derivation::ABSENT
            }
        }
    }

    /// The scan fingerprint summed afresh from its elements, which is what
    /// the incremental `scan_fingerprint` sum holds.
    fn recomputed_scan_fingerprint(&self) -> Fingerprint {
        let hooks = self
            .hook_file_metadata
            .iter()
            .map(|(file_id, metadata)| hook_scan_fingerprint(*file_id, metadata));
        let systems = self
            .system_file_metadata
            .iter()
            .map(|(file_id, metadata)| system_scan_fingerprint(*file_id, metadata));
        let methods = self
            .gm_method_realm_annotations
            .iter()
            .map(|(file_id, method_realms)| gm_method_scan_fingerprint(*file_id, method_realms));
        hooks
            .chain(systems)
            .chain(methods)
            .fold(0, Fingerprint::wrapping_add)
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
        if let Some(previous) = self.system_file_metadata.get(&file_id) {
            note_system_writes(previous);
        }
        note_system_writes(&metadata);
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
        derivation::note_file_fact_read(derivation::FileFact::GmodInferFile, *file_id);
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
        derivation::note_file_fact_read(derivation::FileFact::GmodInferFile, *file_id);
        self.system_file_metadata.get(file_id)
    }

    pub fn iter_system_file_metadata(
        &self,
    ) -> impl Iterator<Item = (&FileId, &GmodSystemFileMetadata)> {
        derivation::note_read(|| FactKey::GmodInferScan);
        self.system_file_metadata.iter()
    }

    /// The system registrations across files, read by kind and name.
    pub fn get_system_aggregate(&self) -> &GmodSystemAggregate {
        self.system_aggregate_cache
            .get_or_init(|| Arc::new(self.build_system_aggregate()))
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
        derivation::note_file_fact_read(derivation::FileFact::GmodRealm, *file_id);
        self.realm_file_metadata.get(file_id)
    }

    /// Get the effective realm at a specific text offset within a file.
    /// If the offset is inside a branch-narrowed block, returns that block's realm.
    /// Otherwise returns the file-level inferred realm, or annotation realm if inferred is Unknown.
    pub fn get_realm_at_offset(&self, file_id: &FileId, offset: rowan::TextSize) -> GmodRealm {
        derivation::note_realm_at_read(*file_id, offset);
        realm_at(self.realm_file_metadata.get(file_id), offset)
    }

    pub fn get_state_mask_at_offset(
        &self,
        file_id: &FileId,
        offset: rowan::TextSize,
    ) -> GmodStateMask {
        derivation::note_realm_at_read(*file_id, offset);
        state_mask_at(self.realm_file_metadata.get(file_id), offset)
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
        self.realm_file_metadata = metadata.into();
    }

    /// Stores `file_id`'s realm evidence. The load fold copies it into the
    /// file's realm metadata (none for a `meta` file), which a re-indexed file
    /// keeps until the fold runs again, so the kept copy is refreshed here.
    pub fn set_realm_inputs(&mut self, file_id: FileId, inputs: GmodRealmInputs, meta: bool) {
        derivation::note_write(|| FactKey::GmodInferFile(file_id));
        if let Some(metadata) = self.realm_file_metadata.get_mut(&file_id) {
            let branch_realm_ranges = if meta {
                Vec::new()
            } else {
                inputs.branch_realm_ranges.clone()
            };
            if metadata.branch_realm_ranges != branch_realm_ranges
                || metadata.annotation_realm != inputs.annotation_realm
            {
                metadata.branch_realm_ranges = branch_realm_ranges;
                metadata.annotation_realm = inputs.annotation_realm;
                derivation::rehoming(|| derivation::note_write(|| FactKey::GmodRealm(file_id)));
            }
        }
        self.realm_inputs.insert(file_id, inputs);
    }

    /// Moves the realm branch ranges of `remap`'s file to where the edit put
    /// them; a range the edit destroyed is dropped, and the file's realm
    /// inputs, which no longer hold, fold it again.
    pub(crate) fn remap_realm_ranges(&mut self, remap: &crate::FileRemap) {
        let Some(metadata) = self.realm_file_metadata.get_mut(&remap.file_id) else {
            return;
        };
        let before = metadata.branch_realm_ranges.clone();
        metadata.branch_realm_ranges.retain_mut(|branch| {
            remap
                .map
                .map_range(branch.range)
                .map(|range| branch.range = range)
                .is_some()
        });
        if metadata.branch_realm_ranges != before {
            derivation::note_write(|| FactKey::GmodRealm(remap.file_id));
        }
    }

    pub fn get_realm_inputs(&self, file_id: &FileId) -> Option<&GmodRealmInputs> {
        derivation::note_realm_inputs_read(*file_id);
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
        derivation::note_file_fact_read(derivation::FileFact::GmodInferFile, *file_id);
        self.member_realm_ranges.contains_key(file_id)
    }

    /// `---@realm` covering decl at `offset`. O(log n).
    pub fn get_member_annotation_realm_at_offset(
        &self,
        file_id: &FileId,
        offset: rowan::TextSize,
    ) -> Option<GmodRealm> {
        derivation::note_member_realm_at_read(*file_id, offset);
        member_realm_at(self.member_realm_ranges.get(file_id), offset)
    }

    pub fn set_file_params(&mut self, file_id: FileId, params: Vec<(String, String)>) {
        if !params.is_empty() {
            derivation::note_write(|| FactKey::GmodInferFile(file_id));
            self.fileparam_index.insert(file_id, params);
        }
    }

    /// O(1) lookup for a @fileparam type text by file and parameter name.
    pub fn get_file_param_type_text(&self, file_id: &FileId, name_lowercase: &str) -> Option<&str> {
        derivation::note_file_fact_read(derivation::FileFact::GmodInferFile, *file_id);
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
        derivation::note_file_fact_read(derivation::FileFact::GmodInferFile, *file_id);
        self.scoped_class_info.get(file_id)
    }

    /// Drops what a file's gmod pre-analysis derived, before it runs again:
    /// everything but the source layer's scoped-class info and the realms
    /// the load graph decided.
    pub(crate) fn reset_file_metadata(&mut self, file_id: FileId) {
        if let Some(previous) = self.system_file_metadata.get(&file_id) {
            note_system_writes(previous);
        }
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
    /// Keeps the load fold's realm metadata of `file_id`: the fold replaces
    /// it whole when it runs again.
    fn remove(&mut self, file_id: FileId) {
        self.reset_file_metadata(file_id);
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
        for metadata in self.system_file_metadata.values() {
            note_system_writes(metadata);
        }
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
    derivation::in_file(Some(file_id), || {
        derivation::fingerprint_of(&(tag, file_id, value))
    })
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

/// The `---@realm` among a file's sorted member realm `ranges` covering
/// `offset`.
fn member_realm_at(
    ranges: Option<&Vec<GmodRealmRange>>,
    offset: rowan::TextSize,
) -> Option<GmodRealm> {
    let ranges = ranges?;
    let idx = match ranges.binary_search_by_key(&offset, |r| r.range.start()) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    };
    [idx, idx.saturating_sub(1)]
        .into_iter()
        .filter_map(|i| ranges.get(i))
        .find(|r| r.range.contains(offset))
        .map(|r| r.realm)
}

/// The realm at `offset` of a file with realm `metadata`: its branch's when
/// the offset sits in a branch-narrowed block, else the file's inferred realm,
/// else its annotation realm.
fn realm_at(metadata: Option<&GmodRealmFileMetadata>, offset: rowan::TextSize) -> GmodRealm {
    let Some(metadata) = metadata else {
        return GmodRealm::Unknown;
    };
    if let Some(range) = metadata
        .branch_realm_ranges
        .iter()
        .find(|range| range.range.contains(offset))
    {
        return range.realm;
    }
    if metadata.inferred_realm != GmodRealm::Unknown {
        return metadata.inferred_realm;
    }
    metadata.annotation_realm.unwrap_or(GmodRealm::Unknown)
}

/// The state mask at `offset` of a file with realm `metadata`: the
/// intersection of every branch-narrowed block containing the offset, within
/// the file's states when they are known, else the file's. An empty mask with
/// no containing block is unknown; with one it is proved unreachable.
fn state_mask_at(
    metadata: Option<&GmodRealmFileMetadata>,
    offset: rowan::TextSize,
) -> GmodStateMask {
    let Some(metadata) = metadata else {
        return GmodStateMask::empty();
    };
    let file_mask = file_state_mask(metadata);
    let mut containing = metadata
        .branch_realm_ranges
        .iter()
        .filter(|range| range.range.contains(offset))
        .map(|range| range.realm.state_mask());
    let Some(first) = containing.next() else {
        return file_mask;
    };
    let branch_mask = containing.fold(first, |acc, mask| acc.intersection(mask));
    if file_mask.is_empty() {
        branch_mask
    } else {
        branch_mask.intersection(file_mask)
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

crate::db_index::cow_shards::task_fork_fields!(GmodInferIndex {
    hook_file_metadata,
    system_file_metadata,
    system_aggregate_cache,
    realm_file_metadata,
    realm_inputs,
    gm_method_realm_annotations,
    member_realm_ranges,
    fileparam_index => |current, frozen, fork| {
        crate::db_index::cow_shards::merge_map(current, frozen, fork, crate::db_index::cow_shards::refuse)
    },
    scoped_class_info,
    scan_fingerprint => crate::db_index::cow_shards::merge_sum,
} then invalidate_system_aggregate_cache);
