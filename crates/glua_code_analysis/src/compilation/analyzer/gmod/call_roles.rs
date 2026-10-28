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

impl AnnotatedGmodGlobalCallRoleMap {
    /// Files `path` under its method name while it has roles, and takes it out
    /// when it has none.
    fn set_receiver_path(&mut self, path: &str) {
        let Some(name) = method_name(path) else {
            return;
        };
        if self.roles_by_path.contains_key(path) {
            self.receiver_paths
                .entry(name)
                .or_default()
                .insert(path.to_string());
        } else if let Some(paths) = self.receiver_paths.get_mut(&name) {
            paths.remove(path);
            if paths.is_empty() {
                self.receiver_paths.remove(&name);
            }
        }
    }
}

/// The entries a fold step may have moved.
#[derive(Default)]
struct Touched {
    paths: HashSet<String>,
    net_ops: HashSet<(SmolStr, NetOpDirection)>,
    globals: HashSet<GlobalId>,
    methods: HashSet<SmolStr>,
    signatures: HashSet<LuaSignatureId>,
    /// Whether a file's forwarded calls moved.
    forwarded_calls: bool,
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
            .filter_map(|path| method_name(path).map(CallRoleKey::ReceiverMethod))
            .collect::<HashSet<_>>()
            .into_iter();
        let globals = self.globals.iter().cloned().map(CallRoleKey::Global);
        let methods = self.methods.iter().cloned().map(CallRoleKey::Method);
        let signatures = self.signatures.iter().copied().map(CallRoleKey::Signature);
        let mut keys = paths
            .chain(receiver_methods)
            .chain(globals)
            .chain(methods)
            .chain(signatures)
            .collect::<Vec<_>>();
        keys.sort_by(|a, b| key_order(a).cmp(&key_order(b)));
        keys
    }
}

/// A total order over call-role keys, so their writes do not follow hash order.
fn key_order(key: &CallRoleKey) -> (u8, &str, Option<(FileId, TextSize)>) {
    match key {
        CallRoleKey::Path(path) => (0, path.as_str(), None),
        CallRoleKey::ReceiverMethod(method) => (1, method.as_str(), None),
        CallRoleKey::Global(global_id) => (2, global_id.get_name(), None),
        CallRoleKey::Method(method) => (3, method.as_str(), None),
        CallRoleKey::Signature(signature_id) => (
            4,
            "",
            Some((signature_id.get_file_id(), signature_id.get_position())),
        ),
    }
}

/// The method name a global function path ends in, if it names a method.
pub(super) fn method_name(path: &str) -> Option<SmolStr> {
    let (_, method) = glua_parser::split_last_path_segment(path)?;
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
        touched.forwarded_calls |= !scan.forwarded_calls.is_empty();
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
                Some(global_id) => {
                    method_name(global_id.get_name()).map(|name| (&mut self.methods, name))
                }
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
        touched.forwarded_calls |= !scan.forwarded_calls.is_empty();
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
                Some(global_id) => {
                    method_name(global_id.get_name()).map(|name| (&mut self.methods, name))
                }
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
    /// The paths whose roles come from the calls their function forwards
    /// rather than from a file's own contribution.
    forwarded_paths: HashSet<String>,
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

        let net_ops = !touched.net_ops.is_empty() || !touched.paths.is_empty();
        let mut keys = self.apply_moving_fingerprint(&touched, Self::apply);
        if touched.forwarded_calls || !touched.paths.is_empty() {
            let desired = self.derive_forwarded_roles();
            let moved = self.forwarded_changes(&desired);
            keys.extend(self.apply_moving_fingerprint(&moved, |roles, moved| {
                roles.apply_forwarded(moved, &desired)
            }));
            keys.sort_by(|a, b| key_order(a).cmp(&key_order(b)));
            keys.dedup_by(|a, b| key_order(a) == key_order(b));
        }
        CallRoleChanges { net_ops, keys }
    }

    /// Applies `touched` through `apply`, keeping the sum of entry terms.
    /// Returns the keys the step may have moved.
    fn apply_moving_fingerprint(
        &mut self,
        touched: &Touched,
        apply: impl FnOnce(&mut Self, &Touched),
    ) -> Vec<CallRoleKey> {
        let keys = touched.keys();
        let before = keys
            .iter()
            .map(|key| entry_term(key, self.unmapped_entry_fingerprint(key)))
            .fold(0, Fingerprint::wrapping_add);
        apply(self, touched);
        let after = keys
            .iter()
            .map(|key| entry_term(key, self.unmapped_entry_fingerprint(key)))
            .fold(0, Fingerprint::wrapping_add);
        self.entries_fingerprint = self
            .entries_fingerprint
            .wrapping_sub(before)
            .wrapping_add(after);
        keys
    }

    /// The roles of every function that registers a panel by passing its
    /// parameters on, for the paths no file contributes roles for. Wrappers of
    /// wrappers settle round by round.
    fn derive_forwarded_roles(&self) -> HashMap<String, (AnnotatedGmodCallRoles, Fingerprint)> {
        let contributions = &*self.contributions;
        let mut files = contributions
            .files
            .values()
            .filter(|(scan, _)| !scan.forwarded_calls.is_empty())
            .collect::<Vec<_>>();
        files.sort_by(|left, right| left.1.cmp(&right.1));
        let forwarded_calls = files
            .iter()
            .flat_map(|(scan, _)| scan.forwarded_calls.iter())
            .collect::<Vec<_>>();

        let mut derived: HashMap<String, AnnotatedGmodCallRoles> = HashMap::default();
        loop {
            let mut grew = false;
            for forward in &forwarded_calls {
                let path = &forward.wrapper_path;
                if derived.contains_key(path) || contributions.path_winner(path).is_some() {
                    continue;
                }
                let callee = forward
                    .callee_path
                    .strip_prefix("_G.")
                    .unwrap_or(&forward.callee_path);
                let inner = derived.get(callee).or_else(|| {
                    contributions
                        .path_winner(callee)
                        .and_then(|_| self.roles.roles_by_path.get(callee))
                });
                let Some(roles) =
                    inner.and_then(|inner| inner.forwarded_vgui_panel_registration(forward))
                else {
                    continue;
                };
                derived.insert(path.clone(), roles);
                grew = true;
            }
            if !grew {
                break;
            }
        }
        derived
            .into_iter()
            .map(|(path, roles)| {
                let fingerprint = derivation::fingerprint_of(&roles);
                (path, (roles, fingerprint))
            })
            .collect()
    }

    /// The paths whose forwarded roles differ from those in place.
    fn forwarded_changes(
        &self,
        desired: &HashMap<String, (AnnotatedGmodCallRoles, Fingerprint)>,
    ) -> Touched {
        let mut moved = Touched::default();
        for path in &self.forwarded_paths {
            if !desired.contains_key(path) {
                moved.paths.insert(path.clone());
            }
        }
        for (path, (_, fingerprint)) in desired {
            if self.roles.path_fingerprints.get(path) != Some(fingerprint) {
                moved.paths.insert(path.clone());
            }
        }
        moved
    }

    /// Puts the forwarded roles of the paths in `moved` in place, or takes the
    /// ones that no longer hold out, unless a file now contributes the path.
    fn apply_forwarded(
        &mut self,
        moved: &Touched,
        desired: &HashMap<String, (AnnotatedGmodCallRoles, Fingerprint)>,
    ) {
        let contributions = &*self.contributions;
        let roles = Arc::make_mut(&mut self.roles);
        for path in &moved.paths {
            match desired.get(path) {
                Some((path_roles, fingerprint)) => {
                    roles.roles_by_path.insert(path.clone(), path_roles.clone());
                    roles.path_fingerprints.insert(path.clone(), *fingerprint);
                    self.forwarded_paths.insert(path.clone());
                }
                None => {
                    if contributions.path_winner(path).is_none() {
                        roles.roles_by_path.remove(path);
                        roles.path_fingerprints.remove(path);
                    }
                    self.forwarded_paths.remove(path);
                }
            }
            roles.set_receiver_path(path);
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
            roles.set_receiver_path(path);
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
