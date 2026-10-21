//! The workspace call roles and net-helper registry, folded from every file's
//! helper scan and kept per contributing file, so a re-scanned file moves only
//! the entries it contributes to.

use std::sync::Arc;

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use super::*;
use crate::db_index::derivation::{self, CallRoleKey, Fingerprint};

/// Where a file's contributions rank: files merge in path order, so the file
/// that wins an entry two files define is a property of the source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FileRank(Arc<(String, String, u32)>);

impl FileRank {
    fn of(db: &DbIndex, file_id: FileId) -> Self {
        let raw_path = db
            .get_vfs()
            .get_file_path(&file_id)
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default();
        Self(Arc::new((
            crate::vfs::normalize_path_for_ordering(&raw_path),
            raw_path,
            file_id.id,
        )))
    }

    /// Whether `file_id` still ranks as `self`: it is at the path ranked.
    fn holds(&self, db: &DbIndex, file_id: &FileId) -> bool {
        db.get_vfs()
            .get_file_path(file_id)
            .map(|path| path.to_string_lossy())
            .unwrap_or_default()
            == self.0.1.as_str()
    }
}

/// Where a helper definition ranks among those naming the same entry: by
/// file, then by body position, global name and signature position.
type DefinitionRank = (FileRank, TextSize, String, TextSize);

/// A helper body.
type Target = (FileId, LuaSyntaxId);

/// Every file's contributions, by the entry they contribute to.
#[derive(Default, Clone)]
struct Contributions {
    /// Each folded file's scan and rank.
    files: HashMap<FileId, (Arc<FileHelperScan>, FileRank)>,
    paths: HashMap<String, Vec<(FileRank, FileId)>>,
    net_ops: HashMap<(SmolStr, NetOpDirection), Vec<(FileRank, FileId)>>,
    globals: HashMap<GlobalId, Vec<(DefinitionRank, Target)>>,
    /// Global functions by the method name their path ends in.
    methods: HashMap<SmolStr, Vec<(DefinitionRank, Target)>>,
    /// Functions stored on a local table, by member name: a method name's
    /// fallback when no global function ends in it.
    local_methods: HashMap<SmolStr, Vec<(DefinitionRank, Target)>>,
    signatures: HashMap<LuaSignatureId, Vec<(DefinitionRank, Target)>>,
}

/// The entries a fold step may have moved.
#[derive(Default)]
struct Touched {
    paths: HashSet<String>,
    net_ops: HashSet<(SmolStr, NetOpDirection)>,
    globals: HashSet<GlobalId>,
    methods: HashSet<SmolStr>,
    signatures: HashSet<LuaSignatureId>,
}

impl Touched {
    fn keys(&self) -> Vec<CallRoleKey> {
        let paths = self
            .paths
            .iter()
            .map(|path| CallRoleKey::Path(SmolStr::new(path)));
        let receiver_methods = self
            .paths
            .iter()
            .filter_map(|path| method_name(&GlobalId::new(path)).map(CallRoleKey::ReceiverMethod))
            .collect::<HashSet<_>>()
            .into_iter();
        let globals = self.globals.iter().cloned().map(CallRoleKey::Global);
        let methods = self.methods.iter().cloned().map(CallRoleKey::Method);
        let signatures = self.signatures.iter().copied().map(CallRoleKey::Signature);
        paths
            .chain(receiver_methods)
            .chain(globals)
            .chain(methods)
            .chain(signatures)
            .collect()
    }
}

/// The method name a global function path ends in, if it names a method.
fn method_name(global_id: &GlobalId) -> Option<SmolStr> {
    let (_, method) = glua_parser::split_last_path_segment(global_id.get_name())?;
    Some(
        glua_parser::path_segment_string_key(method)
            .map_or_else(|| SmolStr::new(method), SmolStr::from),
    )
}

impl Contributions {
    fn add(
        &mut self,
        file_id: FileId,
        scan: Arc<FileHelperScan>,
        rank: FileRank,
        touched: &mut Touched,
    ) {
        let role_map = &scan.role_map;
        for path in role_map.roles_by_path.keys() {
            self.paths
                .entry(path.clone())
                .or_default()
                .push((rank.clone(), file_id));
            touched.paths.insert(path.clone());
        }
        for key in role_map.canonical_net_ops.keys() {
            self.net_ops
                .entry(key.clone())
                .or_default()
                .push((rank.clone(), file_id));
            touched.net_ops.insert(key.clone());
        }
        for (signature_id, body_file, syntax_id, global_id, local_member) in &scan.definitions {
            let name = global_id
                .as_ref()
                .map(|id| id.get_name().to_string())
                .unwrap_or_default();
            let definition = (
                (
                    rank.clone(),
                    syntax_id.get_range().start(),
                    name,
                    signature_id.get_position(),
                ),
                (*body_file, *syntax_id),
            );
            self.signatures
                .entry(*signature_id)
                .or_default()
                .push(definition.clone());
            touched.signatures.insert(*signature_id);
            let method = match global_id {
                Some(global_id) => method_name(global_id).map(|name| (&mut self.methods, name)),
                None => local_member
                    .clone()
                    .map(|name| (&mut self.local_methods, name)),
            };
            if let Some((methods, method)) = method {
                methods
                    .entry(method.clone())
                    .or_default()
                    .push(definition.clone());
                touched.methods.insert(method);
            }
            let Some(global_id) = global_id else {
                continue;
            };
            self.globals
                .entry(global_id.clone())
                .or_default()
                .push(definition);
            touched.globals.insert(global_id.clone());
        }
        self.files.insert(file_id, (scan, rank));
    }

    fn remove(&mut self, file_id: FileId, touched: &mut Touched) {
        let Some((scan, _)) = self.files.remove(&file_id) else {
            return;
        };
        fn retract<K: std::hash::Hash + Eq + Clone, R>(
            map: &mut HashMap<K, Vec<R>>,
            key: &K,
            keep: impl FnMut(&R) -> bool,
        ) {
            if let Some(contributors) = map.get_mut(key) {
                contributors.retain(keep);
                if contributors.is_empty() {
                    map.remove(key);
                }
            }
        }
        let role_map = &scan.role_map;
        for path in role_map.roles_by_path.keys() {
            retract(&mut self.paths, path, |(_, file)| *file != file_id);
            touched.paths.insert(path.clone());
        }
        for key in role_map.canonical_net_ops.keys() {
            retract(&mut self.net_ops, key, |(_, file)| *file != file_id);
            touched.net_ops.insert(key.clone());
        }
        let other_file = |(_, (body_file, _)): &(DefinitionRank, Target)| *body_file != file_id;
        for (signature_id, _, _, global_id, local_member) in &scan.definitions {
            retract(&mut self.signatures, signature_id, other_file);
            touched.signatures.insert(*signature_id);
            let method = match global_id {
                Some(global_id) => method_name(global_id).map(|name| (&mut self.methods, name)),
                None => local_member
                    .clone()
                    .map(|name| (&mut self.local_methods, name)),
            };
            if let Some((methods, method)) = method {
                retract(methods, &method, other_file);
                touched.methods.insert(method);
            }
            let Some(global_id) = global_id else {
                continue;
            };
            retract(&mut self.globals, global_id, other_file);
            touched.globals.insert(global_id.clone());
        }
    }

    /// The scan of the file that wins `path`.
    fn path_winner(&self, path: &str) -> Option<&FileHelperScan> {
        let (_, file_id) = self.paths.get(path)?.iter().min()?;
        self.files.get(file_id).map(|(scan, _)| &**scan)
    }

    /// The canonical op `key` resolves to: the lowest rank, the first file on
    /// a tie.
    fn net_op_winner(
        &self,
        key: &(SmolStr, NetOpDirection),
    ) -> Option<&(CanonicalNetOpRank, crate::db_index::CanonicalNetOp)> {
        self.net_ops
            .get(key)?
            .iter()
            .filter_map(|(rank, file_id)| {
                let candidate = self
                    .files
                    .get(file_id)?
                    .0
                    .role_map
                    .canonical_net_ops
                    .get(key)?;
                Some(((&candidate.0, rank), candidate))
            })
            .min_by(|left, right| left.0.cmp(&right.0))
            .map(|(_, candidate)| candidate)
    }
}

fn first_definition(definitions: Option<&Vec<(DefinitionRank, Target)>>) -> Option<Target> {
    definitions?
        .iter()
        .min_by(|left, right| left.0.cmp(&right.0))
        .map(|(_, target)| *target)
}

/// The workspace call roles and net-helper registry, folded from every file's
/// helper scan.
#[derive(Default, Clone)]
pub(crate) struct GmodCallRoles {
    pub(super) registry: Arc<HelperRegistry>,
    pub(super) roles: Arc<AnnotatedGmodGlobalCallRoleMap>,
    contributions: Arc<Contributions>,
    /// The sum of every entry's [`entry_term`], kept as entries move.
    entries_fingerprint: Fingerprint,
}

/// One entry's share of the call roles' fingerprint.
fn entry_term(key: &CallRoleKey, fingerprint: Fingerprint) -> Fingerprint {
    if fingerprint == derivation::ABSENT {
        0
    } else {
        derivation::fingerprint_of(&(key, fingerprint))
    }
}

/// What a fold step moved besides the call-role entries.
pub(super) struct CallRoleChanges {
    /// The entries that may have moved.
    pub keys: Vec<CallRoleKey>,
    /// Whether a net-op role or canonical op may have moved.
    pub net_ops: bool,
}

impl GmodCallRoles {
    /// Folds in the current helper scan of every file, re-folding only the
    /// files whose scan was replaced or that moved since the last step.
    pub(super) fn fold(&mut self, db: &DbIndex) -> CallRoleChanges {
        let scans = db
            .helper_scan_files()
            .into_iter()
            .filter_map(|file_id| Some((file_id, db.get_helper_scan::<FileHelperScan>(file_id)?)))
            .collect::<Vec<_>>();
        let contributions = Arc::make_mut(&mut self.contributions);
        let current = scans
            .iter()
            .map(|(file_id, _)| *file_id)
            .collect::<HashSet<_>>();
        let mut replaced = contributions
            .files
            .iter()
            .filter(|(file_id, (folded, rank))| {
                !current.contains(file_id)
                    || scans
                        .binary_search_by_key(file_id, |(file_id, _)| file_id)
                        .is_ok_and(|at| !Arc::ptr_eq(&scans[at].1, folded))
                    || !rank.holds(db, file_id)
            })
            .map(|(file_id, _)| *file_id)
            .collect::<Vec<_>>();
        replaced.sort_unstable();
        let mut touched = Touched::default();
        for file_id in replaced {
            contributions.remove(file_id, &mut touched);
        }
        for (file_id, scan) in scans {
            if !contributions.files.contains_key(&file_id) {
                let rank = FileRank::of(db, file_id);
                contributions.add(file_id, scan, rank, &mut touched);
            }
        }

        let keys = touched.keys();
        let before = keys
            .iter()
            .map(|key| entry_term(key, self.unmapped_entry_fingerprint(key)))
            .fold(0, Fingerprint::wrapping_add);
        self.apply(&touched);
        let after = keys
            .iter()
            .map(|key| entry_term(key, self.unmapped_entry_fingerprint(key)))
            .fold(0, Fingerprint::wrapping_add);
        self.entries_fingerprint = self
            .entries_fingerprint
            .wrapping_sub(before)
            .wrapping_add(after);
        CallRoleChanges {
            net_ops: !touched.net_ops.is_empty() || !touched.paths.is_empty(),
            keys,
        }
    }

    /// Re-picks the winner of every touched entry.
    fn apply(&mut self, touched: &Touched) {
        let contributions = &*self.contributions;
        let roles = Arc::make_mut(&mut self.roles);
        for path in &touched.paths {
            match contributions.path_winner(path) {
                Some(scan) => {
                    if let Some(path_roles) = scan.role_map.roles_by_path.get(path) {
                        roles.roles_by_path.insert(path.clone(), path_roles.clone());
                    }
                    match scan.role_map.path_fingerprints.get(path) {
                        Some(fingerprint) => {
                            roles.path_fingerprints.insert(path.clone(), *fingerprint);
                        }
                        None => {
                            roles.path_fingerprints.remove(path);
                        }
                    }
                }
                None => {
                    roles.roles_by_path.remove(path);
                    roles.path_fingerprints.remove(path);
                }
            }
            if let Some(name) = method_name(&GlobalId::new(path)) {
                if roles.roles_by_path.contains_key(path) {
                    roles
                        .receiver_paths
                        .entry(name)
                        .or_default()
                        .insert(path.clone());
                } else if let Some(paths) = roles.receiver_paths.get_mut(&name) {
                    paths.remove(path);
                    if paths.is_empty() {
                        roles.receiver_paths.remove(&name);
                    }
                }
            }
        }
        for key in &touched.net_ops {
            match contributions.net_op_winner(key) {
                Some(candidate) => {
                    roles
                        .canonical_net_ops
                        .insert(key.clone(), candidate.clone());
                }
                None => {
                    roles.canonical_net_ops.remove(key);
                }
            }
        }
        let registry = Arc::make_mut(&mut self.registry);
        for global_id in &touched.globals {
            match first_definition(contributions.globals.get(global_id)) {
                Some(target) => registry.globals.insert(global_id.clone(), target),
                None => registry.globals.remove(global_id),
            };
        }
        for method in &touched.methods {
            let definitions = contributions
                .methods
                .get(method)
                .or_else(|| contributions.local_methods.get(method));
            match definitions.map(Vec::as_slice) {
                Some([(_, target)]) => registry.methods.insert(method.clone(), *target),
                _ => registry.methods.remove(method),
            };
        }
        for signature_id in &touched.signatures {
            match first_definition(contributions.signatures.get(signature_id)) {
                Some(target) => registry.signatures.insert(*signature_id, target),
                None => registry.signatures.remove(signature_id),
            };
        }
    }

    fn unmapped_entry_fingerprint(&self, key: &CallRoleKey) -> Fingerprint {
        derivation::unmapped(|| derivation::CallRoles::entry_fingerprint(self, key))
    }
}

impl derivation::DerivedFact for GmodCallRoles {
    /// The sum of every entry's term; recomputed when edits are mapped back,
    /// since the kept sum names positions as they stand.
    fn fingerprint(&self) -> Fingerprint {
        if !derivation::mapping_back() {
            return self.entries_fingerprint;
        }
        derivation::CallRoles::entries(self)
            .iter()
            .map(|key| entry_term(key, derivation::CallRoles::entry_fingerprint(self, key)))
            .fold(0, Fingerprint::wrapping_add)
    }
}

impl derivation::CallRoles for GmodCallRoles {
    fn entries(&self) -> Vec<CallRoleKey> {
        let paths = self
            .roles
            .roles_by_path
            .keys()
            .map(|path| CallRoleKey::Path(SmolStr::new(path)));
        let receiver_methods = self
            .roles
            .receiver_paths
            .keys()
            .cloned()
            .map(CallRoleKey::ReceiverMethod);
        let globals = self
            .registry
            .globals
            .keys()
            .cloned()
            .map(CallRoleKey::Global);
        let methods = self
            .registry
            .methods
            .keys()
            .cloned()
            .map(CallRoleKey::Method);
        let signatures = self
            .registry
            .signatures
            .keys()
            .copied()
            .map(CallRoleKey::Signature);
        paths
            .chain(receiver_methods)
            .chain(globals)
            .chain(methods)
            .chain(signatures)
            .collect()
    }

    fn entry_fingerprint(&self, key: &CallRoleKey) -> Fingerprint {
        use derivation::fingerprint_opt;
        let site =
            |(file_id, syntax_id): &(FileId, LuaSyntaxId)| InFiled::new(*file_id, *syntax_id);
        match key {
            CallRoleKey::ReceiverMethod(name) => self
                .roles
                .receiver_paths
                .get(name)
                .map_or(derivation::ABSENT, |_| {
                    derivation::fingerprint_of(&self.roles.receiver_presence_value(name))
                }),
            CallRoleKey::Path(path) if !derivation::mapping_back() => self
                .roles
                .path_fingerprints
                .get(path.as_str())
                .copied()
                .unwrap_or(derivation::ABSENT),
            CallRoleKey::Path(path) => fingerprint_opt(self.roles.roles_by_path.get(path.as_str())),
            CallRoleKey::Global(global_id) => {
                fingerprint_opt(self.registry.globals.get(global_id).map(site).as_ref())
            }
            CallRoleKey::Method(name) => {
                fingerprint_opt(self.registry.methods.get(name).map(site).as_ref())
            }
            CallRoleKey::Signature(signature_id) => fingerprint_opt(
                self.registry
                    .signatures
                    .get(signature_id)
                    .map(site)
                    .as_ref(),
            ),
        }
    }
}
