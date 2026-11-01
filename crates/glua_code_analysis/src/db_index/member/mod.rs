mod function_scopes;
mod lua_member;
mod lua_member_feature;
mod lua_member_item;
mod lua_member_owner;
mod lua_owner_members;
mod slot_ids;
mod test;

use crate::db_index::cow_shards::{CowCell, CowShardSet, CowShards, ShardKey};
use rowan::{TextRange, TextSize};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::cmp::Ordering;
use std::collections::BTreeSet;

use super::traits::LuaIndex;
use crate::db_index::derivation::{
    self, ABSENT, DerivationId, FactKey, Fingerprint, fingerprint_of,
};
use crate::{FileId, GlobalId, db_index::member::lua_owner_members::LuaOwnerMembers};
use function_scopes::FunctionScopes;
use slot_ids::SlotIds;

pub use lua_member::{LuaMember, LuaMemberId, LuaMemberKey};
pub use lua_member_feature::LuaMemberFeature;
pub(crate) use lua_member_item::CallerSite;
pub use lua_member_item::LuaMemberIndexItem;
pub use lua_member_owner::LuaMemberOwner;

/// Members filed by owner and then by key, each slot sorted by
/// [`member_id_sort_key`].
///
/// Two of these are kept. The visible one answers lookups; the history one also
/// keeps members a later write displaced, so a removal can still find where they
/// were filed.
type OwnerKeyMap = CowShards<LuaMemberOwner, HashMap<LuaMemberKey, SlotIds>>;

/// Files `id` in its slot unless the slot already holds it.
fn push_owner_key_id_unique(
    target: &mut OwnerKeyMap,
    owner: LuaMemberOwner,
    key: LuaMemberKey,
    id: LuaMemberId,
) -> bool {
    target
        .entry(owner)
        .or_default()
        .entry(key)
        .or_default()
        .insert_sorted(id)
}

/// Merges two tasks' changes to one owner-key slot, which stays sorted by
/// [`member_id_sort_key`].
fn merge_member_ids(
    current: &mut Option<SlotIds>,
    frozen: Option<&SlotIds>,
    fork: Option<&SlotIds>,
) -> crate::db_index::cow_shards::Merged {
    let to_vec = |ids: Option<&SlotIds>| ids.map(|ids| ids.to_vec());
    let mut merged = to_vec(current.as_ref());
    crate::db_index::cow_shards::merge_sorted_by(
        &mut merged,
        to_vec(frozen).as_ref(),
        to_vec(fork).as_ref(),
        |id| member_id_sort_key(*id),
    )?;
    *current = merged.map(SlotIds::from_vec);
    Ok(())
}

/// The member fingerprints a task install rebuilds.
#[derive(Default)]
pub(crate) struct FingerprintsToRebuild {
    owners: Vec<LuaMemberOwner>,
    keys: Vec<LuaMemberKey>,
    files: Vec<FileId>,
}

/// Drops `id` from its slot, and the slot and owner once they are empty.
fn remove_owner_key_id(
    target: &mut OwnerKeyMap,
    owner: &LuaMemberOwner,
    key: &LuaMemberKey,
    id: LuaMemberId,
) -> bool {
    let Some(owner_items) = target.get_mut(owner) else {
        return false;
    };
    let mut removed = false;
    if let Some(member_ids) = owner_items.get_mut(key) {
        removed = member_ids.remove_sorted(id);
        if member_ids.is_empty() {
            owner_items.remove(key);
        }
    }
    if owner_items.is_empty() {
        target.remove(owner);
    }
    removed
}

/// Adds (`add`) or subtracts one element's fingerprint from `key`'s wrapping
/// multiset sum, dropping the entry once the sum is empty.
fn fold_fingerprint<K: ShardKey + Clone>(
    sums: &mut CowShards<K, Fingerprint>,
    key: K,
    element: Fingerprint,
    add: bool,
) {
    let sum = sums.get(&key).copied().unwrap_or(ABSENT);
    let sum = if add {
        sum.wrapping_add(element)
    } else {
        sum.wrapping_sub(element)
    };
    if sum == ABSENT {
        sums.remove(&key);
    } else {
        sums.insert(key, sum);
    }
}

fn history_element(key: &LuaMemberKey, id: LuaMemberId) -> Fingerprint {
    fingerprint_of(&(1u8, key, id))
}

fn current_history_element(id: LuaMemberId) -> Fingerprint {
    fingerprint_of(&(2u8, id))
}

fn file_member_element(id: LuaMemberId) -> Fingerprint {
    fingerprint_of(&(0u8, id))
}

fn file_scope_element(file_id: FileId, range: TextRange) -> Fingerprint {
    fingerprint_of(&(1u8, crate::InFiled::new(file_id, range)))
}

fn file_element_owner_element(site: &crate::InFiled<TextRange>) -> Fingerprint {
    fingerprint_of(&(2u8, site))
}

/// What a read of a member object alone observes: its key, feature and global
/// id.
fn member_object(member: &LuaMember) -> impl std::hash::Hash + '_ {
    (
        member.get_key(),
        member.get_feature(),
        member.get_global_id(),
    )
}

fn note_slot_write(owner: &LuaMemberOwner, key: &LuaMemberKey) {
    derivation::note_write(|| FactKey::MemberSlot(owner.clone(), key.clone()));
}

/// Notes a change to `owner`'s members. Whether it has a member map at all
/// moves only where the map appears or goes ([`note_owner_presence_write`]).
fn note_owner_map_write(owner: &LuaMemberOwner) {
    derivation::note_write(|| FactKey::MemberOwnerMap(owner.clone()));
    derivation::note_write(|| FactKey::MemberOwnerExprKeys(owner.clone()));
    derivation::note_write(|| FactKey::MemberOwnerFields(owner.clone()));
}

/// Notes that `owner`'s member map appeared or went, or that the owner a
/// name canonicalises to moved: its members and its presence.
fn note_owner_presence_write(owner: &LuaMemberOwner) {
    note_owner_map_write(owner);
    derivation::note_write(|| FactKey::MemberOwnerPresence(owner.clone()));
}

fn note_owner_presence_read(owner: &LuaMemberOwner) {
    derivation::note_read(|| FactKey::MemberOwnerPresence(owner.clone()));
}

fn note_slot_read(owner: &LuaMemberOwner, key: &LuaMemberKey) {
    derivation::note_member_slot_read(owner, key);
}

fn note_owner_map_read(owner: &LuaMemberOwner) {
    derivation::note_read(|| FactKey::MemberOwnerMap(owner.clone()));
}

/// A member as the source layer left it, kept while a derivation has changed
/// it so a reset can restore it.
#[derive(Debug, Clone, PartialEq)]
struct SourceMember {
    /// `None` when the source layer has no such member.
    state: Option<SourceMemberState>,
    /// Every owner a derivation listed the member under.
    derived_owners: Vec<LuaMemberOwner>,
}

#[derive(Debug, Clone, PartialEq)]
struct SourceMemberState {
    member: LuaMember,
    owner: Option<LuaMemberOwner>,
    owner_only: bool,
    provenance: Option<GlobalId>,
    non_overwriting: bool,
    deferred_index_expr: bool,
    function_scope: Option<TextRange>,
}

/// The class a scripted-class synthesis moved a member to; see
/// [`FactKey::MemberTransfer`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MemberTransfer {
    owner: LuaMemberOwner,
    /// Whether a global path's class change leaves the member there.
    pinned: bool,
}

/// Where a transferred member's own writers left it homed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FiledHome {
    owner: Option<LuaMemberOwner>,
    owner_only: bool,
    provenance: Option<GlobalId>,
}

/// Who moves a member: its own writers, whose home is part of its
/// [`FactKey::Member`], or a transfer, which moves only its
/// [`FactKey::MemberHome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Homing {
    Filed,
    Transferred,
}

#[derive(Debug, Clone)]
pub struct LuaMemberIndex {
    members: CowShards<LuaMemberId, LuaMember>,
    source_members: CowShards<LuaMemberId, SourceMember>,
    /// The owners each file's members are filed under. The members themselves
    /// are the file's shard of `members`.
    in_filed: CowShards<FileId, HashSet<LuaMemberOwner>>,
    owner_members: CowShards<LuaMemberOwner, LuaOwnerMembers>,
    member_current_owner: CowShards<LuaMemberId, LuaMemberOwner>,
    member_owner_key_index: OwnerKeyMap,
    member_owner_key_history_index: OwnerKeyMap,
    /// The owners under which a file's members were filed in the two
    /// owner-key indexes. A file sweep only has to visit these owners, rather
    /// than every owner in the workspace. Entries may outlive the members that
    /// caused them; a sweep of an owner that holds none is a no-op.
    owner_key_index_owners_by_file: CowShards<FileId, HashSet<LuaMemberOwner>>,
    current_owner_member_history: CowShards<LuaMemberOwner, SlotIds>,
    current_members_by_key: CowShards<LuaMemberKey, SlotIds>,
    /// The members filed under an expression key that names another file's
    /// positions, by that file.
    expr_key_members_by_file: CowShards<FileId, HashSet<LuaMemberId>>,
    non_overwriting_assignment_members: CowShardSet<LuaMemberId>,
    /// `(owner, key)` slots whose last co-writer readmission merged nothing.
    ///
    /// See [`Self::readmit_preserved_assignment_co_writers`]: every later
    /// scan of such a slot is a proven no-op, so marks skip the walk. Only
    /// `clear` drops entries, with the index they describe — every other
    /// mutation preserves the coverage they record, and any future history
    /// write without a paired item merge must clear its slot here.
    preserved_co_writer_reconciled: CowShardSet<(LuaMemberOwner, LuaMemberKey)>,
    /// Members whose owner was recorded without adding them to the owner's
    /// member list: a write through an instance of a class, not the class.
    /// See [`Self::set_member_owner_only`].
    owner_only_members: CowShardSet<LuaMemberId>,
    /// The class each member a scripted-class synthesis moved went to.
    member_transfers: CowShards<LuaMemberId, MemberTransfer>,
    /// Where each member with a transfer applied was before it: the home its
    /// own writers own.
    filed_homes: CowShards<LuaMemberId, FiledHome>,
    /// Members `try_resolve_member` had to create itself, from a prefix
    /// type read mid-fixpoint.
    deferred_index_expr_members: CowCell<HashSet<LuaMemberId>>,
    function_scope_ranges: CowCell<HashMap<FileId, FunctionScopes>>,
    member_function_scope_ranges: CowShards<LuaMemberId, TextRange>,
    /// The owner a table literal that initialises a global path resolves to:
    /// the `{}` of `X = {}` or of the GLua-idiomatic `X = X or {}`, at any
    /// depth. Held as the owner rather than the path so the normaliser can
    /// hand back a reference. See [`LuaMemberIndex::canonical_owner`].
    definition_site_owner: CowCell<HashMap<crate::InFiled<TextRange>, LuaMemberOwner>>,
    /// The reverse of [`Self::definition_site_owner`], each path's sites sorted
    /// by `(file id, range)`.
    path_definition_sites: CowCell<HashMap<GlobalId, Vec<crate::InFiled<TextRange>>>>,
    definition_sites_by_file: CowCell<HashMap<FileId, Vec<crate::InFiled<TextRange>>>>,
    /// The class a global path's declaration carries (`---@class oslib` on
    /// `os = {}`). Such a path's members belong to the class, which is where
    /// every reader of the annotated type looks for them.
    global_path_class_owner: CowShards<GlobalId, LuaMemberOwner>,
    /// Every `---@class` proposal made for a global path's declaration, by
    /// path. The winner is the set minimum, so two files annotating one
    /// global settle on the same class regardless of analysis order, and a
    /// removed file's proposal can be dropped without losing the others.
    global_path_class_candidates: CowShards<GlobalId, BTreeSet<GlobalPathClassCandidate>>,
    /// Who proposed each candidate: `None` for the source layer, else the
    /// derivation. A candidate leaves with its last holder.
    path_class_holders: CowShards<(GlobalId, GlobalPathClassCandidate), Vec<Option<DerivationId>>>,
    /// The candidates each derivation proposed, retracted before it re-runs.
    derived_path_classes: CowShards<DerivationId, Vec<(GlobalId, GlobalPathClassCandidate)>>,
    /// The global path a member's homing went through: its write named the
    /// path directly, or a table literal that initialises one. Such members
    /// follow the path when its winning `---@class` changes; members homed
    /// onto a class by any other route stay where they are.
    member_path_provenance: CowShards<LuaMemberId, GlobalId>,
    /// Per-owner multiset fingerprint of its history-index entries and
    /// current-owner history, for [`FactKey::MemberOwnerMap`].
    owner_history_fingerprints: CowShards<LuaMemberOwner, Fingerprint>,
    /// Per-key multiset fingerprint of `current_members_by_key`, for
    /// [`FactKey::MemberKey`].
    key_fingerprints: CowShards<LuaMemberKey, Fingerprint>,
    /// Per-file multiset fingerprint of the file's members, function scopes
    /// and element owners, for [`FactKey::MemberFile`].
    file_fingerprints: CowShards<FileId, Fingerprint>,
}

/// One file's `---@class` proposal for a global path's declaration.
///
/// Ordered by class name first — the winner must be name-lexicographic-min so
/// two files annotating one global do not depend on analysis order — then by
/// file id and class identity, which breaks ties between same-named classes
/// deterministically. Deriving [`Eq`] over all three fields makes re-proposing
/// the same class from the same file a no-op.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GlobalPathClassCandidate {
    class_name: Box<str>,
    file_id: FileId,
    class: crate::LuaTypeDeclId,
}

impl GlobalPathClassCandidate {
    /// The file whose annotation proposed the class.
    pub(crate) fn file_id(&self) -> FileId {
        self.file_id
    }
}

impl Ord for GlobalPathClassCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.class_name
            .cmp(&other.class_name)
            .then_with(|| self.file_id.cmp(&other.file_id))
            .then_with(|| self.class.stable_cmp(&other.class))
    }
}

impl PartialOrd for GlobalPathClassCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Default for LuaMemberIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl LuaMemberIndex {
    pub fn new() -> Self {
        Self {
            members: CowShards::default(),
            source_members: CowShards::default(),
            in_filed: Default::default(),
            owner_members: CowShards::default(),
            member_current_owner: Default::default(),
            member_owner_key_index: Default::default(),
            member_owner_key_history_index: Default::default(),
            owner_key_index_owners_by_file: Default::default(),
            current_owner_member_history: CowShards::default(),
            current_members_by_key: CowShards::default(),
            expr_key_members_by_file: Default::default(),
            non_overwriting_assignment_members: Default::default(),
            preserved_co_writer_reconciled: Default::default(),
            owner_only_members: Default::default(),
            member_transfers: CowShards::default(),
            filed_homes: CowShards::default(),
            deferred_index_expr_members: Default::default(),
            function_scope_ranges: Default::default(),
            member_function_scope_ranges: Default::default(),
            definition_site_owner: Default::default(),
            path_definition_sites: Default::default(),
            definition_sites_by_file: Default::default(),
            global_path_class_owner: Default::default(),
            global_path_class_candidates: Default::default(),
            path_class_holders: Default::default(),
            derived_path_classes: Default::default(),
            member_path_provenance: Default::default(),
            owner_history_fingerprints: Default::default(),
            key_fingerprints: Default::default(),
            file_fingerprints: Default::default(),
        }
    }

    /// The fingerprint of what a read of `key` observes now.
    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::Member(id) => self.member_fingerprint(*id),
            FactKey::MemberObject(id) => self
                .members
                .get(id)
                .map_or(ABSENT, |member| fingerprint_of(&member_object(member))),
            FactKey::MemberHome(id) => {
                let owner = self.member_current_owner.get(id);
                let provenance = self.member_path_provenance.get(id);
                let owner_only = self.owner_only_members.contains(id);
                if owner.is_none() && provenance.is_none() && !owner_only {
                    return ABSENT;
                }
                fingerprint_of(&(owner, owner_only, provenance))
            }
            FactKey::MemberTransfer(id) => {
                derivation::fingerprint_opt(self.member_transfers.get(id))
            }
            FactKey::MemberMarks(id) => {
                let (provenance, flags) = self.member_marks(*id);
                let scope = self.member_scope(*id);
                if provenance.is_none() && scope.is_none() && !flags.contains(&true) {
                    return ABSENT;
                }
                fingerprint_of(&(provenance, flags, scope))
            }
            FactKey::MemberSlot(owner, key) => self.slot_fingerprint(owner, key),
            FactKey::FunctionScope(site) => fingerprint_of(&crate::InFiled::new(
                site.file_id,
                self.innermost_function_scope(site.file_id, site.value),
            )),
            FactKey::MemberOwnerPresence(owner) => {
                if self.owner_members.contains_key(owner) {
                    fingerprint_of(&true)
                } else {
                    ABSENT
                }
            }
            FactKey::MemberOwnerFields(owner) => {
                self.owner_members.get(owner).map_or(ABSENT, |members| {
                    fingerprint_of(&self.field_member_ids(members).collect::<Vec<_>>())
                })
            }
            FactKey::MemberOwnerExprKeys(owner) => {
                self.owner_members.get(owner).map_or(ABSENT, |members| {
                    fingerprint_of(&members.sorted_expr_key_member_ids())
                })
            }
            FactKey::MemberOwnerMap(owner) => {
                let items = self.owner_members.get(owner).map_or(ABSENT, |members| {
                    members
                        .fingerprint()
                        .wrapping_add(fingerprint_of("owner-present"))
                });
                items.wrapping_add(self.owner_history_fingerprint(owner))
            }
            FactKey::MemberKey(key) if derivation::mapping_back() => self
                .current_members_by_key
                .get(key)
                .map_or(ABSENT, |members| {
                    members
                        .iter()
                        .map(fingerprint_of)
                        .fold(ABSENT, Fingerprint::wrapping_add)
                }),
            FactKey::MemberKey(key) => self.key_fingerprints.get(key).copied().unwrap_or(ABSENT),
            FactKey::MemberFile(file_id) if derivation::mapping_back() => {
                self.recomputed_file_fingerprint(*file_id)
            }
            FactKey::MemberFile(file_id) => self
                .file_fingerprints
                .get(file_id)
                .copied()
                .unwrap_or(ABSENT),
            FactKey::GlobalPath(path) => {
                let sites = self.path_definition_sites.get(path);
                let class = self.global_path_class_owner.get(path);
                let candidates = self.global_path_class_candidates.get(path);
                if sites.is_none() && class.is_none() && candidates.is_none() {
                    return ABSENT;
                }
                fingerprint_of(&(sites, class, candidates))
            }
            FactKey::PathClassContrib(id) => {
                derivation::fingerprint_opt(self.derived_path_classes.get(id))
            }
            FactKey::DefinitionSite(site) => {
                derivation::fingerprint_opt(self.definition_site_owner.get(site))
            }
            _ => {
                debug_assert!(false, "{key:?} is not a member-index fact");
                ABSENT
            }
        }
    }

    /// The member object, the owner its own writers homed it under, flags,
    /// provenance and scope: a transfer moves none of it.
    fn member_fingerprint(&self, id: LuaMemberId) -> Fingerprint {
        let member = self.members.get(&id).map(member_object);
        let owner = self.filed_home(id).0;
        let (provenance, flags) = self.member_marks(id);
        let scope = self.member_scope(id);
        if member.is_none()
            && owner.is_none()
            && provenance.is_none()
            && scope.is_none()
            && !flags.contains(&true)
        {
            return ABSENT;
        }
        fingerprint_of(&(member, owner, provenance, scope, flags))
    }

    /// A member's function scope, in its file.
    fn member_scope(&self, id: LuaMemberId) -> Option<crate::InFiled<TextRange>> {
        self.member_function_scope_ranges
            .get(&id)
            .map(|range| crate::InFiled::new(id.file_id, *range))
    }

    /// A member's path provenance and its owner-only, deferred and
    /// non-overwriting flags, as its own writers left them.
    fn member_marks(&self, id: LuaMemberId) -> (Option<&GlobalId>, [bool; 3]) {
        let (_, owner_only, provenance) = self.filed_home(id);
        (
            provenance,
            [
                owner_only,
                self.deferred_index_expr_members.contains(&id),
                self.non_overwriting_assignment_members.contains(&id),
            ],
        )
    }

    /// `id`'s owner, owner-only mark and provenance as its own writers left
    /// them: before the transfer applied to it, if one is.
    fn filed_home(&self, id: LuaMemberId) -> (Option<&LuaMemberOwner>, bool, Option<&GlobalId>) {
        match self.filed_homes.get(&id) {
            Some(home) => (
                home.owner.as_ref(),
                home.owner_only,
                home.provenance.as_ref(),
            ),
            None => (
                self.member_current_owner.get(&id),
                self.owner_only_members.contains(&id),
                self.member_path_provenance.get(&id),
            ),
        }
    }

    /// The item listed under `(owner, key)` and both owner-key index slots in
    /// order, each id with whether it is still homed there under `key`.
    fn slot_fingerprint(&self, owner: &LuaMemberOwner, key: &LuaMemberKey) -> Fingerprint {
        let item = self
            .owner_members
            .get(owner)
            .and_then(|members| members.get_member(key));
        let visible = self
            .member_owner_key_index
            .get(owner)
            .and_then(|slots| slots.get(key));
        let history = self
            .member_owner_key_history_index
            .get(owner)
            .and_then(|slots| slots.get(key));
        if item.is_none() && visible.is_none() && history.is_none() {
            return ABSENT;
        }
        let entries = [visible, history].map(|ids| {
            ids.map_or(&[][..], SlotIds::as_slice)
                .iter()
                .map(|id| {
                    let homed = self.member_current_owner.get(id) == Some(owner);
                    let key_matches =
                        homed.then(|| self.members.get(id).map(|member| member.get_key() == key));
                    (id, homed, key_matches)
                })
                .collect::<Vec<_>>()
        });
        fingerprint_of(&(item, entries))
    }

    /// Notes every fact naming `id`'s current home: the member object, its
    /// home, its owner's slot and map, and its key.
    fn note_member_home_write(&self, id: LuaMemberId) {
        self.note_home_write(id, Homing::Filed);
    }

    /// [`Self::note_member_home_write`] for a move by `homing`: a transfer
    /// leaves the member object alone.
    fn note_home_write(&self, id: LuaMemberId, homing: Homing) {
        if !derivation::recording() {
            return;
        }
        if homing == Homing::Filed {
            derivation::note_write(|| FactKey::Member(id));
        }
        derivation::note_write(|| FactKey::MemberHome(id));
        let key = self.members.get(&id).map(LuaMember::get_key);
        if let Some(key) = key {
            derivation::note_write(|| FactKey::MemberKey(key.clone()));
        }
        if let Some(owner) = self.member_current_owner.get(&id) {
            note_owner_map_write(owner);
            if let Some(key) = key {
                note_slot_write(owner, key);
            }
        }
    }

    /// Notes a change to what `path` (and `site`, when given) canonicalises
    /// to: the mapping keys themselves and the owner maps of every owner the
    /// old and new mappings name.
    fn note_canonical_write(
        path: &GlobalId,
        site: Option<&crate::InFiled<TextRange>>,
        classes: &[Option<&crate::LuaTypeDeclId>],
    ) {
        derivation::note_write(|| FactKey::GlobalPath(path.clone()));
        note_owner_presence_write(&LuaMemberOwner::GlobalPath(path.clone()));
        if let Some(site) = site {
            derivation::note_write(|| FactKey::DefinitionSite(site.clone()));
            note_owner_presence_write(&LuaMemberOwner::Element(site.clone()));
        }
        for class in classes.iter().flatten() {
            note_owner_presence_write(&LuaMemberOwner::Type((*class).clone()));
        }
    }

    fn owner_history_fingerprint(&self, owner: &LuaMemberOwner) -> Fingerprint {
        if !derivation::mapping_back() {
            return self
                .owner_history_fingerprints
                .get(owner)
                .copied()
                .unwrap_or(ABSENT);
        }
        self.recomputed_owner_history_fingerprint(owner)
    }

    /// `owner`'s history fingerprint summed afresh, which is what the
    /// incremental `owner_history_fingerprints` sum holds.
    fn recomputed_owner_history_fingerprint(&self, owner: &LuaMemberOwner) -> Fingerprint {
        let keyed = self
            .member_owner_key_history_index
            .get(owner)
            .into_iter()
            .flatten()
            .flat_map(|(key, ids)| ids.iter().map(move |id| history_element(key, *id)));
        let current = self
            .current_owner_member_history
            .get(owner)
            .into_iter()
            .flat_map(|history| history.iter())
            .map(|id| current_history_element(*id));
        keyed
            .chain(current)
            .fold(ABSENT, |sum: Fingerprint, element| {
                sum.wrapping_add(element)
            })
    }

    fn fold_history_fingerprint(
        &mut self,
        owner: &LuaMemberOwner,
        element: Fingerprint,
        add: bool,
    ) {
        note_owner_map_write(owner);
        fold_fingerprint(
            &mut self.owner_history_fingerprints,
            owner.clone(),
            element,
            add,
        );
    }

    /// `file_id`'s member-file fingerprint summed afresh, which is what the
    /// incremental `file_fingerprints` sum holds.
    fn recomputed_file_fingerprint(&self, file_id: FileId) -> Fingerprint {
        let members = self
            .members
            .file_entries(file_id)
            .map(|(id, _)| file_member_element(*id));
        let scopes = self
            .function_scope_ranges
            .get(&file_id)
            .into_iter()
            .flat_map(FunctionScopes::ranges)
            .map(|range| file_scope_element(file_id, *range));
        let element_owners = self
            .owner_members
            .file_entries(file_id)
            .filter_map(|(owner, _)| match owner {
                LuaMemberOwner::Element(site) => Some(file_element_owner_element(site)),
                _ => None,
            });
        members
            .chain(scopes)
            .chain(element_owners)
            .fold(ABSENT, Fingerprint::wrapping_add)
    }

    /// `key`'s fingerprint summed afresh, which is what the incremental
    /// `key_fingerprints` sum holds.
    fn recomputed_key_fingerprint(&self, key: &LuaMemberKey) -> Fingerprint {
        self.current_members_by_key
            .get(key)
            .into_iter()
            .flat_map(|members| members.iter())
            .map(fingerprint_of)
            .fold(ABSENT, Fingerprint::wrapping_add)
    }

    /// The fingerprints both `self` and `fork`, a task fork of `frozen`,
    /// changed: sums over what both changed, rebuilt from the merged state.
    fn fingerprints_to_rebuild(&self, frozen: &Self, fork: &Self) -> FingerprintsToRebuild {
        FingerprintsToRebuild {
            owners: self.owner_history_fingerprints.both_changed_keys(
                &frozen.owner_history_fingerprints,
                &fork.owner_history_fingerprints,
            ),
            keys: self
                .key_fingerprints
                .both_changed_keys(&frozen.key_fingerprints, &fork.key_fingerprints),
            files: self
                .file_fingerprints
                .both_changed_keys(&frozen.file_fingerprints, &fork.file_fingerprints),
        }
    }

    /// Installs the fingerprints of a superstep's `forks` in fork order,
    /// leaving each fork the frozen ones, and returns every fingerprint both
    /// the index and a fork changed. Summed afresh once every fork is in,
    /// each holds what summing it afresh after each such fork leaves: once
    /// summed afresh it differs from `frozen`, so the last fork that changes
    /// it sums it afresh too, and later forks change no slot it sums.
    fn install_fingerprints(
        &mut self,
        frozen: &Self,
        forks: &mut [(usize, Self)],
    ) -> FingerprintsToRebuild {
        fn take_in<K: ShardKey + Clone + Send + Sync + 'static>(
            installed: &mut CowShards<K, Fingerprint>,
            frozen: &CowShards<K, Fingerprint>,
            forked: &mut CowShards<K, Fingerprint>,
        ) {
            let forked = std::mem::replace(forked, frozen.clone());
            let conflicted = crate::db_index::cow_shards::MergeFork::install_merging(
                installed,
                frozen,
                forked,
                crate::db_index::cow_shards::rebuilt,
            );
            debug_assert!(!conflicted, "a rebuilt fingerprint never conflicts");
        }
        let mut rebuild = FingerprintsToRebuild::default();
        for (_, fork) in forks {
            let planned = self.fingerprints_to_rebuild(frozen, fork);
            rebuild.owners.extend(planned.owners);
            rebuild.keys.extend(planned.keys);
            rebuild.files.extend(planned.files);
            take_in(
                &mut self.owner_history_fingerprints,
                &frozen.owner_history_fingerprints,
                &mut fork.owner_history_fingerprints,
            );
            take_in(
                &mut self.key_fingerprints,
                &frozen.key_fingerprints,
                &mut fork.key_fingerprints,
            );
            take_in(
                &mut self.file_fingerprints,
                &frozen.file_fingerprints,
                &mut fork.file_fingerprints,
            );
        }
        rebuild
    }

    fn rebuild_fingerprints(&mut self, rebuild: FingerprintsToRebuild) {
        fn store<K: ShardKey + Clone>(
            sums: &mut CowShards<K, Fingerprint>,
            key: K,
            sum: Fingerprint,
        ) {
            if sum == ABSENT {
                sums.remove(&key);
            } else {
                sums.insert(key, sum);
            }
        }
        for owner in rebuild.owners.into_iter().collect::<HashSet<_>>() {
            let sum = self.recomputed_owner_history_fingerprint(&owner);
            store(&mut self.owner_history_fingerprints, owner, sum);
        }
        for key in rebuild.keys.into_iter().collect::<HashSet<_>>() {
            let sum = self.recomputed_key_fingerprint(&key);
            store(&mut self.key_fingerprints, key, sum);
        }
        for file_id in rebuild.files.into_iter().collect::<HashSet<_>>() {
            let sum = self.recomputed_file_fingerprint(file_id);
            store(&mut self.file_fingerprints, file_id, sum);
        }
    }

    fn fold_file_fingerprint(&mut self, file_id: FileId, element: Fingerprint, add: bool) {
        derivation::note_write(|| FactKey::MemberFile(file_id));
        fold_fingerprint(&mut self.file_fingerprints, file_id, element, add);
    }

    /// Removes `owner`'s member map, keeping the fingerprints that name it.
    fn remove_owner_entry(&mut self, owner: &LuaMemberOwner) -> Option<LuaOwnerMembers> {
        let removed = self.owner_members.remove(owner)?;
        note_owner_presence_write(owner);
        if derivation::recording() {
            for key in removed.get_member_keys() {
                note_slot_write(owner, key);
            }
        }
        if let LuaMemberOwner::Element(range) = owner {
            self.fold_file_fingerprint(range.file_id, file_element_owner_element(range), false);
        }
        Some(removed)
    }

    /// Notes every fact the index currently holds, for a wholesale reset.
    fn note_every_fact_written(&self) {
        let ids = self
            .members
            .keys()
            .chain(self.member_current_owner.keys())
            .chain(self.member_path_provenance.keys())
            .chain(self.member_function_scope_ranges.keys())
            .chain(self.owner_only_members.iter())
            .chain(self.filed_homes.keys())
            .chain(self.deferred_index_expr_members.iter())
            .chain(self.non_overwriting_assignment_members.iter());
        for id in ids {
            derivation::note_write(|| FactKey::Member(*id));
            derivation::note_write(|| FactKey::MemberHome(*id));
        }
        for id in self.member_transfers.keys() {
            derivation::note_write(|| FactKey::MemberTransfer(*id));
        }
        for (owner, members) in &self.owner_members {
            note_owner_presence_write(owner);
            for key in members.get_member_keys() {
                note_slot_write(owner, key);
            }
        }
        for index in [
            &self.member_owner_key_index,
            &self.member_owner_key_history_index,
        ] {
            for (owner, slots) in index {
                note_owner_map_write(owner);
                for key in slots.keys() {
                    note_slot_write(owner, key);
                }
            }
        }
        for owner in self.current_owner_member_history.keys() {
            note_owner_map_write(owner);
        }
        for key in self.current_members_by_key.keys() {
            derivation::note_write(|| FactKey::MemberKey(key.clone()));
        }
        for file_id in self.file_fingerprints.keys() {
            derivation::note_write(|| FactKey::MemberFile(*file_id));
        }
        let paths = self
            .path_definition_sites
            .keys()
            .chain(self.global_path_class_owner.keys())
            .chain(self.global_path_class_candidates.keys());
        for path in paths {
            derivation::note_write(|| FactKey::GlobalPath(path.clone()));
        }
        for site in self.definition_site_owner.keys() {
            derivation::note_write(|| FactKey::DefinitionSite(site.clone()));
        }
    }

    /// Creates `file_id`'s per-file entries when absent. An empty file keeps
    /// empty per-file entries, which all readers treat exactly like absent ones.
    pub fn add_file_entries(&mut self, file_id: FileId) {
        self.in_filed.entry(file_id).or_default();
        self.owner_key_index_owners_by_file
            .entry(file_id)
            .or_default();
        self.function_scope_ranges.entry(file_id).or_default();
    }

    /// A rewrite, flag change, or drop behind the member's back alters the
    /// membership view both its owner and key slots answer for.
    fn note_member_move(&mut self, member_id: LuaMemberId) {
        self.note_member_home_write(member_id);
    }

    /// [`Self::note_member_move`] for writes that leave the owner/key slots
    /// alone — provenance, bookkeeping flags, first filings, owner-slot
    /// bookkeeping — but still change what an id-keyed member read sees.
    fn note_member_object_write(&mut self, member_id: LuaMemberId) {
        self.note_object_write(member_id, Homing::Filed);
    }

    /// [`Self::note_member_object_write`] for a write by `homing`.
    fn note_object_write(&self, member_id: LuaMemberId, homing: Homing) {
        if homing == Homing::Filed {
            derivation::note_write(|| FactKey::Member(member_id));
        }
        derivation::note_write(|| FactKey::MemberHome(member_id));
    }

    /// Keeps the source layer's version of `id` before a derivation first
    /// changes it.
    fn snapshot_member(&mut self, id: LuaMemberId) {
        if derivation::writes_source() {
            return;
        }
        derivation::note_write(|| FactKey::Member(id));
        if self.source_members.contains_key(&id) {
            return;
        }
        let (owner, owner_only, provenance) = self.filed_home(id);
        let state = self.members.get(&id).map(|member| SourceMemberState {
            member: member.clone(),
            owner: owner.cloned(),
            owner_only,
            provenance: provenance.cloned(),
            non_overwriting: self.non_overwriting_assignment_members.contains(&id),
            deferred_index_expr: self.deferred_index_expr_members.contains(&id),
            function_scope: self.member_function_scope_ranges.get(&id).copied(),
        });
        self.source_members.insert(
            id,
            SourceMember {
                state,
                derived_owners: Vec::new(),
            },
        );
    }

    /// Records that a derivation listed `id` under `owner`, and under the
    /// owner that name resolves to now, which is where the listing lands: a
    /// reset must find it there even after the name resolves elsewhere.
    fn note_derived_owner(&mut self, id: LuaMemberId, owner: &LuaMemberOwner) {
        if derivation::writes_source() {
            return;
        }
        self.snapshot_member(id);
        let listed = derivation::untracked(|| {
            let listed = self
                .effective_path_member_owner(id, owner)
                .unwrap_or_else(|| owner.clone());
            self.canonical(&listed).clone()
        });
        if let Some(source) = self.source_members.get_mut(&id) {
            for owner in [owner, &listed] {
                if !source.derived_owners.contains(owner) {
                    source.derived_owners.push(owner.clone());
                }
            }
        }
    }

    /// Restores `id` to what the source layer left: every derived listing is
    /// dropped and the source member, if any, is filed again, under the
    /// transfer that still holds for it.
    pub(crate) fn reset_member(&mut self, id: LuaMemberId) {
        if !self.source_members.contains_key(&id) {
            return;
        }
        self.write_filed(id, |index| index.reset_filed_member(id));
    }

    fn reset_filed_member(&mut self, id: LuaMemberId) {
        let Some(source) = self.source_members.remove(&id) else {
            return;
        };
        let mut owners = source.derived_owners;
        if let Some(owner) = self.member_current_owner.get(&id) {
            owners.push(owner.clone());
        }
        for owner in &owners {
            self.remove_member_from_all_owner_key_indexes(owner, id);
            self.remove_member_from_owner_item(owner, id);
        }
        self.forget_member(id);
        let Some(state) = source.state else {
            return;
        };
        // A member homed through a global path is filed under the path, so
        // it follows whichever class the path resolves to now.
        let owner = match state.provenance {
            Some(path) => LuaMemberOwner::GlobalPath(path),
            None => state.owner.unwrap_or(LuaMemberOwner::LocalUnresolve),
        };
        self.file_member(owner, state.member, state.owner_only);
        if state.non_overwriting {
            self.non_overwriting_assignment_members.insert(id);
        }
        if state.deferred_index_expr {
            self.deferred_index_expr_members.insert(id);
        }
        if let Some(range) = state.function_scope {
            self.member_function_scope_ranges.insert(id, range);
        }
    }

    pub fn add_member(&mut self, owner: LuaMemberOwner, member: LuaMember) -> LuaMemberId {
        self.add_member_with_owner_only(owner, member, false)
    }

    pub fn add_member_with_owner_only(
        &mut self,
        owner: LuaMemberOwner,
        member: LuaMember,
        is_owner_only: bool,
    ) -> LuaMemberId {
        derivation::noting_writes_once(|| {
            if !owner.is_unknown() {
                self.note_derived_owner(member.get_id(), &owner);
            } else {
                self.snapshot_member(member.get_id());
            }
            // Where the member is filed is the index's own placement: a later
            // class binding or site registration re-homes it, so what the
            // writer produced does not depend on the placement state it saw.
            if !owner.is_unknown() {
                note_slot_write(&owner, member.get_key());
                note_owner_map_write(&owner);
            }
            let id = member.get_id();
            derivation::untracked(|| {
                self.write_filed(id, |index| index.file_member(owner, member, is_owner_only))
            })
        })
    }

    fn file_member(
        &mut self,
        owner: LuaMemberOwner,
        member: LuaMember,
        is_owner_only: bool,
    ) -> LuaMemberId {
        let id = member.get_id();
        self.note_member_home_write(id);
        let owner = self
            .effective_path_member_owner(id, &owner)
            .unwrap_or(owner);
        self.note_member_path_provenance(id, &owner, Homing::Filed);
        let owner = self.canonical(&owner).clone();
        let file_id = member.get_file_id();
        let function_scope = self.assignment_file_define_scope_for_member(&member);
        // Re-filing an existing id re-homes whatever the old filing named:
        // the member keeps its identity while its owner and key maps move, so
        // any memo holding the old slot must rebuild. Pure first filings are
        // append-only and leave the revision alone. An identical re-file —
        // same owner, key and member record — changes nothing a membership
        // view or memo can observe, so stamping it would only churn readers
        // (settled passes re-file members every round while converging).
        let mut already_filed_here = false;
        let mut member_object_changed = true;
        if self.members.contains_key(&id) {
            // The insert below overwrites the member record, but nothing else
            // removes the id from the key slot its previous filing named. When
            // the owner is unchanged and only the key moved, that leaves the id
            // listed under two keys of one owner, so `get_members` yields it
            // twice. Drop the stale key slot; a changed owner is a genuine
            // second home (membership accumulates across owners) and is kept.
            let old_owner = self.member_current_owner.get(&id).cloned();
            let same_owner = old_owner.as_ref() == Some(&owner);
            let key_changed = self.get_member(&id).map(|m| m.get_key()) != Some(member.get_key());
            let identical = same_owner
                && !key_changed
                && self.owner_only_members.contains(&id) == is_owner_only
                && self.members.get(&id).is_some_and(|existing| {
                    existing.get_feature() == member.get_feature()
                        && existing.get_global_id() == member.get_global_id()
                });
            member_object_changed = !identical;
            if !identical {
                self.note_member_move(id);
            }
            match old_owner {
                Some(old_owner) if same_owner && key_changed => {
                    self.remove_member_from_owner_slot(&old_owner, id);
                }
                // The old owner keeps listing the id, but it is no longer the
                // member's current home, so its current-only views let it go.
                Some(old_owner) if !same_owner => {
                    self.remove_member_from_visible_owner_key_index(&old_owner, id);
                    self.remove_current_owner_member(&old_owner, id);
                    if key_changed {
                        self.remove_current_member_key(id);
                    }
                }
                _ => {}
            }
            // A re-file under the same owner and key is a rewrite, not a new
            // membership: the owner-key indexes already hold the id, and
            // pushing it again would list the member twice in the slot.
            already_filed_here = same_owner && !key_changed;
        }
        let first_filing = self.members.insert(id, member).is_none();
        if member_object_changed {
            self.note_member_object_write(id);
        }
        self.set_member_function_scope_range(id, function_scope);
        if first_filing {
            self.fold_file_fingerprint(file_id, file_member_element(id), true);
        }
        if !owner.is_unknown() {
            self.member_current_owner.insert(id, owner.clone());
            self.add_current_member_key(id);
            self.insert_current_owner_member(&owner, id);
            self.add_file_owner(file_id, owner.clone());
            if is_owner_only {
                self.owner_only_members.insert(id);
            } else {
                if !already_filed_here {
                    self.add_new_member_to_owner_key_indexes(owner.clone(), id);
                }
                self.list_member(owner.clone(), id, Homing::Filed);
            }
        }
        self.note_member_home_write(id);
        id
    }

    /// Files `id` in `owner`'s current-owner history.
    fn insert_current_owner_member(&mut self, owner: &LuaMemberOwner, id: LuaMemberId) {
        let inserted = self
            .current_owner_member_history
            .entry(owner.clone())
            .or_default()
            .insert_sorted(id);
        if inserted {
            self.fold_history_fingerprint(owner, current_history_element(id), true);
        }
    }

    /// Removes `id` from `owner`'s item slot and the owner-key indexes, reading
    /// the key off the member's current record. Shared by `rekey_member` and
    /// the re-file path in `add_member_with_owner_only`: both move a member
    /// whose stored key still names the slot being vacated.
    fn remove_member_from_owner_slot(&mut self, owner: &LuaMemberOwner, id: LuaMemberId) {
        let Some(key) = self.get_member(&id).map(|member| member.get_key().clone()) else {
            return;
        };
        self.remove_member_from_all_owner_key_indexes(owner, id);
        self.remove_current_member_key(id);
        if let Some(owner_members) = self.owner_members.get_mut(owner)
            && owner_members.remove_id(&key, id)
        {
            note_slot_write(owner, &key);
            note_owner_map_write(owner);
        }
    }

    /// Files an existing member under a different key.
    ///
    /// A dynamically keyed write is filed under the type its key expression
    /// inferred to, and that type is a function of how far the walk had got.
    /// Once the settled passes have taken the key again against the complete
    /// map, the member has to move to the slot the settled key names — the
    /// member's own id is its identity, so moving it keeps the writers of a
    /// slot together instead of orphaning the old key.
    ///
    /// Every removal path reads the key back off the member, so the member and
    /// all key-indexed maps move together; a half-moved member would leave the
    /// old slot behind on the next `remove_files`.
    pub fn rekey_member(&mut self, member_id: LuaMemberId, new_key: LuaMemberKey) -> Option<()> {
        self.write_filed(member_id, |index| {
            index.rekey_filed_member(member_id, new_key)
        })
    }

    fn rekey_filed_member(&mut self, member_id: LuaMemberId, new_key: LuaMemberKey) -> Option<()> {
        self.snapshot_member(member_id);
        let old_key = self.get_member(&member_id)?.get_key().clone();
        if old_key == new_key {
            return Some(());
        }

        self.note_member_move(member_id);
        let owner = self.member_current_owner.get(&member_id).cloned();
        if let Some(owner) = &owner {
            self.remove_member_from_owner_slot(owner, member_id);
        }

        self.members.get_mut(&member_id)?.set_key(new_key.clone());

        if let Some(owner) = owner {
            self.add_current_member_key(member_id);
            self.add_member_to_owner_key_indexes(owner.clone(), member_id);
            self.list_member(owner, member_id, Homing::Filed);
        }
        self.note_member_home_write(member_id);
        Some(())
    }

    fn add_file_owner(&mut self, file_id: FileId, owner: LuaMemberOwner) {
        self.in_filed.entry(file_id).or_default().insert(owner);
    }

    /// Files `id` under `owner`. A slot holds every member written or declared
    /// for its key, ordered by [`member_id_sort_key`]; readers merge them.
    pub fn add_member_to_owner(&mut self, owner: LuaMemberOwner, id: LuaMemberId) -> Option<()> {
        self.write_filed(id, |index| index.list_member(owner, id, Homing::Filed))
    }

    /// [`Self::add_member_to_owner`] for a listing by `homing`.
    fn list_member(
        &mut self,
        owner: LuaMemberOwner,
        id: LuaMemberId,
        homing: Homing,
    ) -> Option<()> {
        if homing == Homing::Filed {
            self.note_derived_owner(id, &owner);
        }
        let owner = self
            .effective_path_member_owner(id, &owner)
            .unwrap_or(owner);
        let owner = self.canonical(&owner).clone();
        let key = self.get_member(&id)?.get_key().clone();
        if self.member_current_owner.get(&id) != Some(&owner) {
            self.add_member_to_owner_key_indexes(owner.clone(), id);
        }
        if self.owner_only_members.remove(&id) {
            self.note_object_write(id, homing);
        }
        self.add_file_owner(id.file_id, owner.clone());
        self.merge_member_into_owner_item(owner, key, id);
        Some(())
    }

    /// Records `owner` as `id`'s owner without listing `id` among the owner's
    /// members: the write went through an instance of a class and does not
    /// define a member on it. The mark keeps later co-writer readmission from
    /// listing it either, which otherwise depended on which writer the batch
    /// walked first.
    pub fn set_member_owner_only(
        &mut self,
        owner: LuaMemberOwner,
        file_id: FileId,
        id: LuaMemberId,
    ) -> Option<()> {
        self.write_filed(id, |index| {
            index.note_derived_owner(id, &owner);
            index.home_member(owner, file_id, id, Homing::Filed)?;
            let owner = index.member_current_owner.get(&id)?.clone();
            index.remove_member_from_owner_item(&owner, id);
            if index.owner_only_members.insert(id) {
                index.note_member_move(id);
            }
            Some(())
        })
    }

    /// Homes a member whose write went through an alias of the global path
    /// `alias_path`, stamping that provenance first.
    ///
    /// `local Repair = Glide.Repair` names the path's table, so the member
    /// must follow the path when its winning `---@class` changes — exactly
    /// what a write through the path itself would do. The provenance is
    /// recorded before the effective-owner redirect so a write that names a
    /// losing candidate class re-homes onto the winner, and the note's drop
    /// branch is skipped so a homing that lands elsewhere keeps the alias's
    /// evidence. `owner_only` files the member without listing it among the
    /// owner's members, matching [`Self::set_member_owner_only`].
    pub fn home_alias_member_with_provenance(
        &mut self,
        owner: LuaMemberOwner,
        file_id: FileId,
        id: LuaMemberId,
        alias_path: GlobalId,
        owner_only: bool,
    ) -> Option<()> {
        self.write_filed(id, |index| {
            index.home_alias_filed_member(owner, file_id, id, alias_path, owner_only)
        })
    }

    fn home_alias_filed_member(
        &mut self,
        owner: LuaMemberOwner,
        file_id: FileId,
        id: LuaMemberId,
        alias_path: GlobalId,
        owner_only: bool,
    ) -> Option<()> {
        self.note_derived_owner(id, &owner);
        self.stamp_member_path_provenance(id, &alias_path, Homing::Filed);
        self.record_member_owner(owner, file_id, id, Homing::Filed)?;
        if owner_only {
            if self.owner_only_members.insert(id) {
                self.note_member_move(id);
            }
            return Some(());
        }
        let key = self.get_member(&id)?.get_key().clone();
        if self.owner_only_members.remove(&id) {
            self.note_member_object_write(id);
        }
        let owner = self.member_current_owner.get(&id)?.clone();
        self.merge_member_into_owner_item(owner, key, id);
        Some(())
    }

    /// Moves `id` to `owner`, the class a scripted-class synthesis decided,
    /// over the home its own writers keep: the move is this transfer's fact,
    /// not the member's. A `pinned` member stays there when
    /// [`Self::set_global_path_class`] moves the path's members onto the
    /// annotated class.
    pub fn transfer_member(&mut self, owner: LuaMemberOwner, id: LuaMemberId, pinned: bool) {
        derivation::note_write(|| FactKey::MemberTransfer(id));
        self.unapply_transfer(id);
        self.member_transfers
            .insert(id, MemberTransfer { owner, pinned });
        self.apply_transfer(id);
    }

    /// Drops the transfer of `id`, which goes back to the home its own
    /// writers left it in.
    pub(crate) fn reset_transfer(&mut self, id: LuaMemberId) {
        self.unapply_transfer(id);
        if self.member_transfers.remove(&id).is_some() {
            derivation::note_write(|| FactKey::MemberTransfer(id));
        }
    }

    /// Runs `write`, a write by `id`'s own writers, against the home they left
    /// it in: the transfer applied to `id` comes off first and goes back on
    /// after, so where `id` ends up never depends on whether its writers or
    /// its transfer ran last.
    fn write_filed<R>(&mut self, id: LuaMemberId, write: impl FnOnce(&mut Self) -> R) -> R {
        self.unapply_transfer(id);
        let written = write(self);
        self.apply_transfer(id);
        written
    }

    /// Moves `id` onto its transfer's owner, keeping its filed home.
    fn apply_transfer(&mut self, id: LuaMemberId) {
        let Some(transfer) = self.member_transfers.get(&id).cloned() else {
            return;
        };
        if !self.members.contains_key(&id) || self.filed_homes.contains_key(&id) {
            return;
        }
        let (owner, owner_only, provenance) = self.filed_home(id);
        let filed = FiledHome {
            owner: owner.cloned(),
            owner_only,
            provenance: provenance.cloned(),
        };
        self.filed_homes.insert(id, filed);
        self.home_member(transfer.owner.clone(), id.file_id, id, Homing::Transferred);
        self.list_member(transfer.owner, id, Homing::Transferred);
    }

    /// Moves `id` from its transfer's owner back to its filed home, undoing
    /// [`Self::apply_transfer`].
    fn unapply_transfer(&mut self, id: LuaMemberId) {
        let Some(filed) = self.filed_homes.remove(&id) else {
            return;
        };
        let current = self.member_current_owner.get(&id).cloned();
        if current != filed.owner {
            self.note_home_write(id, Homing::Transferred);
            if let Some(current) = &current {
                self.remove_member_from_all_owner_key_indexes(current, id);
                self.remove_member_from_owner_item(current, id);
                self.remove_current_owner_member(current, id);
            }
            match &filed.owner {
                Some(owner) => {
                    self.member_current_owner.insert(id, owner.clone());
                    self.insert_current_owner_member(owner, id);
                    self.add_member_to_owner_key_indexes(owner.clone(), id);
                }
                None => {
                    self.member_current_owner.remove(&id);
                }
            }
        }
        if filed.owner_only {
            self.owner_only_members.insert(id);
        } else {
            self.owner_only_members.remove(&id);
        }
        match filed.provenance {
            Some(path) => {
                self.member_path_provenance.insert(id, path);
            }
            None => {
                self.member_path_provenance.remove(&id);
            }
        }
        self.note_home_write(id, Homing::Transferred);
    }

    /// The global path this member reached its owner through, if it named one:
    /// a write through the path itself, an alias of it, or a table literal
    /// initialising it. Members carrying provenance are owned by the
    /// class-flip machinery (`set_global_path_class`), not by the prefix-type
    /// read a settled pass re-derives.
    pub fn member_path_provenance(&self, id: &LuaMemberId) -> Option<&GlobalId> {
        derivation::note_member_marks_read(*id);
        self.member_path_provenance.get(id)
    }

    /// Records that `id` was created by deferred resolution, so its first home
    /// was decided from a mid-fixpoint prefix type. See
    /// [`Self::deferred_index_expr_members`].
    pub fn mark_deferred_index_expr_member(&mut self, id: LuaMemberId) {
        self.snapshot_member(id);
        if self.deferred_index_expr_members.insert(id) {
            self.note_member_move(id);
        }
    }

    pub fn is_deferred_index_expr_member(&self, id: &LuaMemberId) -> bool {
        derivation::note_member_marks_read(*id);
        self.deferred_index_expr_members.contains(id)
    }

    /// Removes `id` from `owner` entirely, including the item
    /// `set_member_owner` leaves behind.
    pub fn detach_member_from_owner(&mut self, owner: &LuaMemberOwner, id: LuaMemberId) {
        self.write_filed(id, |index| index.detach_filed_member(owner, id));
    }

    fn detach_filed_member(&mut self, owner: &LuaMemberOwner, id: LuaMemberId) {
        self.snapshot_member(id);
        let owner = &self.canonical(owner).clone();
        let Some(key) = self.get_member(&id).map(|member| member.get_key().clone()) else {
            return;
        };
        self.drop_member_path_provenance_on_detach(owner, id);
        self.remove_member_from_all_owner_key_indexes(owner, id);
        self.remove_current_owner_member(owner, id);

        let Some(owner_members) = self.owner_members.get_mut(owner) else {
            return;
        };
        if owner_members.remove_id(&key, id) {
            note_slot_write(owner, &key);
            note_owner_map_write(owner);
        }
        if owner_members.is_empty() {
            self.remove_owner_entry(owner);
        }
    }

    /// Makes `id` *also* reachable through `owner`, without ever displacing
    /// what is already there.
    pub fn add_member_alias_to_owner(
        &mut self,
        owner: LuaMemberOwner,
        id: LuaMemberId,
    ) -> Option<()> {
        self.write_filed(id, |index| index.add_filed_member_alias(owner, id))
    }

    fn add_filed_member_alias(&mut self, owner: LuaMemberOwner, id: LuaMemberId) -> Option<()> {
        self.note_derived_owner(id, &owner);
        let owner = self.canonical(&owner).clone();
        let member = self.get_member(&id)?;
        let file_id = member.get_file_id();
        let key = member.get_key().clone();

        if self.member_current_owner.get(&id) != Some(&owner) {
            self.add_member_to_owner_key_indexes(owner.clone(), id);
        }

        self.merge_member_into_owner_item(owner.clone(), key, id);
        self.add_file_owner(file_id, owner);
        Some(())
    }

    /// Adds `id` to the item already stored at `owner`/`key`, keeping the item a
    /// set ordered by `member_id_sort_key`. Never removes an existing id, and is
    /// a no-op when `id` is already present.
    fn merge_member_into_owner_item(
        &mut self,
        owner: LuaMemberOwner,
        key: LuaMemberKey,
        id: LuaMemberId,
    ) {
        note_slot_write(&owner, &key);
        let creates_map = !self.owner_members.contains_key(&owner);
        if creates_map {
            note_owner_presence_write(&owner);
        } else {
            note_owner_map_write(&owner);
        }
        if creates_map && let LuaMemberOwner::Element(range) = &owner {
            self.fold_file_fingerprint(range.file_id, file_element_owner_element(range), true);
        }
        let owner_members = self
            .owner_members
            .entry(owner)
            .or_insert_with(LuaOwnerMembers::new);
        // A slot that already holds `id` must not touch the item:
        // `get_member_mut` invalidates the owner's revision on entry, so a
        // no-op merge would still mark every read recorded against this
        // owner stale.
        let already_present = match owner_members.get_member(&key) {
            Some(LuaMemberIndexItem::One(existing)) => *existing == id,
            Some(LuaMemberIndexItem::Many(ids)) => ids
                .binary_search_by_key(&member_id_sort_key(id), |existing_id| {
                    member_id_sort_key(*existing_id)
                })
                .is_ok(),
            None => false,
        };
        if already_present {
            return;
        }
        if !owner_members.contains_member(&key) {
            owner_members.add_member(key, LuaMemberIndexItem::One(id));
            return;
        }
        owner_members.note_id_added(&key, id);
        let Some(item) = owner_members.get_member_mut(&key) else {
            return;
        };

        match item {
            LuaMemberIndexItem::One(old_id) => {
                if *old_id != id {
                    *item = LuaMemberIndexItem::Many(sorted_member_pair(*old_id, id));
                }
            }
            LuaMemberIndexItem::Many(ids) => {
                // `Many` is always sorted by `member_id_sort_key`: `One`
                // promotes through `sorted_member_pair`, every later insert
                // lands through the paths below, and removals only filter —
                // so the binary search doubles as the duplicate check and no
                // linear scan is needed.
                if ids
                    .last()
                    .is_none_or(|last_id| member_id_sort_key(*last_id) < member_id_sort_key(id))
                {
                    ids.push(id);
                    return;
                }

                match ids.binary_search_by_key(&member_id_sort_key(id), |existing_id| {
                    member_id_sort_key(*existing_id)
                }) {
                    Ok(_) => {}
                    Err(index) => ids.insert(index, id),
                }
            }
        }
    }

    /// Files `id` under its owner and key in both owner-key indexes, ignoring a
    /// slot that already holds it.
    fn add_member_to_owner_key_indexes(&mut self, owner: LuaMemberOwner, id: LuaMemberId) {
        let Some(key) = self.get_member(&id).map(|member| member.get_key().clone()) else {
            return;
        };

        self.record_owner_key_index_owner(id.file_id, &owner);
        let visible_changed = push_owner_key_id_unique(
            &mut self.member_owner_key_index,
            owner.clone(),
            key.clone(),
            id,
        );
        let history_changed = push_owner_key_id_unique(
            &mut self.member_owner_key_history_index,
            owner.clone(),
            key.clone(),
            id,
        );
        if history_changed {
            self.fold_history_fingerprint(&owner, history_element(&key, id), true);
        }
        if visible_changed || history_changed {
            note_slot_write(&owner, &key);
        }
    }

    /// [`add_member_to_owner_key_indexes`](Self::add_member_to_owner_key_indexes)
    /// for a member whose *current* filing is new. The history index keeps
    /// entries a later owner move left behind, and an alias filing
    /// ([`Self::add_member_alias_to_owner`]) lists the id under an owner its
    /// current owner never names, so either slot can already hold the id when
    /// the member returns to a former `(owner, key)`: both pushes dedupe.
    fn add_new_member_to_owner_key_indexes(&mut self, owner: LuaMemberOwner, id: LuaMemberId) {
        let Some(key) = self.get_member(&id).map(|member| member.get_key().clone()) else {
            return;
        };

        self.record_owner_key_index_owner(id.file_id, &owner);
        note_slot_write(&owner, &key);
        push_owner_key_id_unique(
            &mut self.member_owner_key_index,
            owner.clone(),
            key.clone(),
            id,
        );
        if push_owner_key_id_unique(
            &mut self.member_owner_key_history_index,
            owner.clone(),
            key.clone(),
            id,
        ) {
            self.fold_history_fingerprint(&owner, history_element(&key, id), true);
        }
    }

    fn record_owner_key_index_owner(&mut self, file_id: FileId, owner: &LuaMemberOwner) {
        let owners = self
            .owner_key_index_owners_by_file
            .entry(file_id)
            .or_default();
        if !owners.contains(owner) {
            owners.insert(owner.clone());
        }
    }

    /// Drops `id` from the visible index only, leaving the history index to
    /// remember that this owner once held it.
    fn remove_member_from_visible_owner_key_index(
        &mut self,
        owner: &LuaMemberOwner,
        id: LuaMemberId,
    ) {
        let Some(key) = self.get_member(&id).map(|member| member.get_key().clone()) else {
            return;
        };
        if remove_owner_key_id(&mut self.member_owner_key_index, owner, &key, id) {
            note_slot_write(owner, &key);
        }
    }

    fn remove_member_from_all_owner_key_indexes(
        &mut self,
        owner: &LuaMemberOwner,
        id: LuaMemberId,
    ) {
        let Some(key) = self.get_member(&id).map(|member| member.get_key().clone()) else {
            return;
        };
        let visible_changed =
            remove_owner_key_id(&mut self.member_owner_key_index, owner, &key, id);
        let history_changed =
            remove_owner_key_id(&mut self.member_owner_key_history_index, owner, &key, id);
        if history_changed {
            self.fold_history_fingerprint(owner, history_element(&key, id), false);
        }
        if visible_changed || history_changed {
            note_slot_write(owner, &key);
        }
    }

    fn remove_files_members_from_owner_key_indexes(&mut self, removed: &HashSet<FileId>) {
        let owners = removed
            .iter()
            .filter_map(|file_id| self.owner_key_index_owners_by_file.remove(file_id))
            .flatten()
            .collect::<HashSet<_>>();
        let visible = Self::remove_files_members_from_owner_key_map(
            &mut self.member_owner_key_index,
            removed,
            &owners,
        );
        if derivation::recording() {
            for (owner, key, _) in &visible {
                note_slot_write(owner, key);
            }
        }
        let history = Self::remove_files_members_from_owner_key_map(
            &mut self.member_owner_key_history_index,
            removed,
            &owners,
        );
        for (owner, key, id) in &history {
            note_slot_write(owner, key);
            self.fold_history_fingerprint(owner, history_element(key, *id), false);
        }
    }

    /// Drops the removed files' members from `owners`' slots, returning each
    /// `(owner, key, id)` dropped.
    fn remove_files_members_from_owner_key_map(
        owner_key_index: &mut OwnerKeyMap,
        removed: &HashSet<FileId>,
        owners: &HashSet<LuaMemberOwner>,
    ) -> Vec<(LuaMemberOwner, LuaMemberKey, LuaMemberId)> {
        let mut removed_entries = Vec::new();
        for owner in owners {
            let Some(key_members) = owner_key_index.get_mut(owner) else {
                continue;
            };
            key_members.retain(|key, member_ids| {
                member_ids.retain(|member_id| {
                    let keep = !removed.contains(&member_id.file_id);
                    if !keep {
                        removed_entries.push((owner.clone(), key.clone(), *member_id));
                    }
                    keep
                });
                !member_ids.is_empty()
            });
            if key_members.is_empty() {
                owner_key_index.remove(owner);
            }
        }
        removed_entries
    }

    fn is_assignment_file_define_member(&self, id: LuaMemberId) -> bool {
        self.get_member(&id)
            .is_some_and(LuaMember::is_assignment_define)
    }

    fn assignment_file_define_scope_for_member(&self, member: &LuaMember) -> Option<TextRange> {
        if !member.is_assignment_define() {
            return None;
        }

        self.enclosing_function_scope_range(member.get_file_id(), member.get_id().get_position())
    }

    /// Records `owner` as `id`'s current owner, filing its history entry.
    ///
    /// Production callers always follow this with an item merge (see
    /// [`Self::add_member_to_owner`]); a bare call without one would leave a
    /// history entry the item does not list, which must clear that slot in
    /// [`Self::preserved_co_writer_reconciled`].
    ///
    pub fn set_member_owner(
        &mut self,
        owner: LuaMemberOwner,
        file_id: FileId,
        id: LuaMemberId,
    ) -> Option<()> {
        self.write_filed(id, |index| {
            index.home_member(owner, file_id, id, Homing::Filed)
        })
    }

    /// [`Self::set_member_owner`] for a move by `homing`.
    fn home_member(
        &mut self,
        owner: LuaMemberOwner,
        file_id: FileId,
        id: LuaMemberId,
        homing: Homing,
    ) -> Option<()> {
        if homing == Homing::Filed {
            self.note_derived_owner(id, &owner);
        }
        // Where the member is filed is the index's own placement, as in
        // `file_member`: a later class binding or site registration re-homes it.
        derivation::untracked(|| {
            let owner = self
                .effective_path_member_owner(id, &owner)
                .unwrap_or(owner);
            self.note_member_path_provenance(id, &owner, homing);
            self.record_member_owner(owner, file_id, id, homing)
        })
    }

    /// The filing half of [`Self::set_member_owner`], without the provenance
    /// note. Callers that record provenance themselves route through here so
    /// the note's drop branch cannot undo what they just recorded.
    fn record_member_owner(
        &mut self,
        owner: LuaMemberOwner,
        file_id: FileId,
        id: LuaMemberId,
        homing: Homing,
    ) -> Option<()> {
        let owner = self
            .effective_path_member_owner(id, &owner)
            .unwrap_or(owner);
        let owner = self.canonical(&owner).clone();
        // Read before the insert decides the bump: a real move, or a first
        // homing for an id the index already holds (deferred resolution filed
        // the member under an unknown owner and only now learned better).
        // A pure first filing (unknown id) and a same-owner re-home are
        // append-only and must not invalidate memos.
        let already_indexed = self.members.contains_key(&id);
        self.note_home_write(id, homing);
        let previous_owner = self.member_current_owner.insert(id, owner.clone());
        self.note_home_write(id, homing);
        if previous_owner.as_ref() != Some(&owner) {
            self.note_object_write(id, homing);
        }
        match &previous_owner {
            None if already_indexed => {
                self.note_home_write(id, homing);
            }
            Some(previous) if previous != &owner => {
                self.note_home_write(id, homing);
            }
            _ => {}
        }
        if previous_owner.is_none() {
            self.add_current_member_key(id);
        }
        if let Some(previous_owner) = previous_owner
            .as_ref()
            .filter(|previous_owner| *previous_owner != &owner)
        {
            self.remove_member_from_visible_owner_key_index(previous_owner, id);
            self.remove_current_owner_member(previous_owner, id);
        }

        self.insert_current_owner_member(&owner, id);

        self.add_member_to_owner_key_indexes(owner.clone(), id);
        if homing == Homing::Filed
            && self.member_function_scope_range(id).is_none()
            && let Some(member) = self.get_member(&id)
        {
            let function_scope = self.assignment_file_define_scope_for_member(member);
            self.set_member_function_scope_range(id, function_scope);
        }
        self.add_file_owner(file_id, owner);

        Some(())
    }

    pub fn get_member(&self, id: &LuaMemberId) -> Option<&LuaMember> {
        derivation::note_member_object_read(*id);
        self.members.get(id)
    }

    pub fn get_member_mut(&mut self, id: &LuaMemberId) -> Option<&mut LuaMember> {
        self.snapshot_member(*id);
        // The key lives on the member, so a mutable access may move it.
        if self.members.contains_key(id) {
            self.note_member_move(*id);
        }
        self.members.get_mut(id)
    }

    /// Records `site` as a table literal that initialises the global `path`.
    ///
    /// A path's members all live on [`LuaMemberOwner::GlobalPath`]; its literals
    /// are definition sites, not owners. The site is decided by the file that
    /// owns the range, and that file's declaration walk always precedes any
    /// member insertion naming the range, so a member never has to be re-homed:
    /// the decl walk is pre-order, so `X = {}` registers before the literal's
    /// own fields are added, and every later phase runs after the whole batch's
    /// decl phase. A file cannot name another file's literal range
    /// syntactically, so a foreign write is always `a.b.c = v`, which the decl
    /// analyzer already routes to the path.
    pub fn set_definition_site(&mut self, path: GlobalId, site: crate::InFiled<TextRange>) {
        let owner = LuaMemberOwner::GlobalPath(path.clone());
        if self.definition_site_owner.get(&site) == Some(&owner) {
            return;
        }
        self.forget_definition_site(&site);
        Self::note_canonical_write(&path, Some(&site), &[]);
        self.definition_site_owner.insert(site.clone(), owner);
        let sites = self.path_definition_sites.entry(path.clone()).or_default();
        if let Err(at) = sites.binary_search_by_key(&site_sort_key(&site), site_sort_key) {
            sites.insert(at, site.clone());
        }
        self.definition_sites_by_file
            .entry(site.file_id)
            .or_default()
            .push(site.clone());
        // A member filed under the literal before the site was registered was
        // homed onto the Element owner with no path provenance. Stamp it so a
        // later `---@class` flip still takes it along. Idempotent, and rare:
        // the decl walk registers each site before the writes it owns.
        let member_ids: Vec<LuaMemberId> = self
            .owner_members
            .get(&LuaMemberOwner::Element(site))
            .into_iter()
            .flat_map(LuaOwnerMembers::get_member_items)
            .flat_map(member_ids_from_item)
            .collect();
        self.stamp_path_provenance_if_absent(member_ids, &path);
    }

    /// The global path the table literal `site` initialises, if one is
    /// registered for it.
    pub fn definition_site_path(&self, site: &crate::InFiled<TextRange>) -> Option<GlobalId> {
        derivation::note_definition_site_read(site);
        self.definition_site_owner.get(site)?.get_path().cloned()
    }

    /// Whether `path` has at least one `---@class` proposal, so the class-flip
    /// machinery ([`Self::set_global_path_class`]) owns its members' homing.
    /// A path with no candidates can never flip, so provenance on it is inert
    /// and must not exempt a member from settled owner re-derivation.
    pub fn global_path_has_class_candidates(&self, path: &GlobalId) -> bool {
        derivation::note_global_path_read(path);
        self.global_path_class_candidates.contains_key(path)
    }

    /// Whether `path` is a global path the index knows about: it has members
    /// filed under it, a registered definition site, or a `---@class`
    /// proposal. Read-only and cheap, so producers may consult it per write.
    ///
    /// The membership arm is not covered by the `GlobalPath` domain epoch —
    /// filing a member under the path only bumps membership clocks — so when
    /// it decides the answer it records the owner presence read as well.
    pub fn is_known_global_path(&self, path: &GlobalId) -> bool {
        derivation::note_global_path_read(path);
        if self.global_path_class_candidates.contains_key(path)
            || self.path_definition_sites.contains_key(path)
        {
            return true;
        }
        let owner = LuaMemberOwner::GlobalPath(path.clone());
        note_owner_presence_read(&owner);
        self.owner_members.contains_key(&owner)
    }

    /// Records `path` as the provenance of members that do not have one yet.
    ///
    /// Members already carrying provenance keep it: the earliest evidence of
    /// how a member reached its owner is the one its later homings honor, and
    /// re-stamping the same path stays a no-op.
    pub fn stamp_path_provenance_if_absent(
        &mut self,
        member_ids: impl IntoIterator<Item = LuaMemberId>,
        path: &GlobalId,
    ) {
        let member_ids = member_ids.into_iter().collect::<Vec<_>>();
        for id in &member_ids {
            self.snapshot_member(*id);
        }
        for member_id in member_ids {
            self.write_filed(member_id, |index| {
                if let std::collections::hash_map::Entry::Vacant(slot) =
                    index.member_path_provenance.entry(member_id)
                {
                    slot.insert(path.clone());
                    index.note_member_object_write(member_id);
                }
            });
        }
    }

    /// The literals initialising `path`, sorted by `(file id, range)`.
    pub fn definition_sites(&self, path: &GlobalId) -> &[crate::InFiled<TextRange>] {
        derivation::note_global_path_read(path);
        self.path_definition_sites
            .get(path)
            .map_or(&[], |sites| sites.as_slice())
    }

    /// The one owner a table literal's members belong to.
    ///
    /// A literal that initialises a global path is not an owner: its members
    /// belong to the path, alongside every other literal's and every
    /// `path.k = v` write's. Applied wherever an owner enters or is looked up
    /// in this index, so producers keep building [`LuaMemberOwner::Element`]
    /// and never see the difference.
    pub fn canonical_owner(&self, owner: LuaMemberOwner) -> LuaMemberOwner {
        match self.canonical(&owner) {
            canonical if canonical == &owner => owner,
            canonical => canonical.clone(),
        }
    }

    fn canonical<'a>(&'a self, owner: &'a LuaMemberOwner) -> &'a LuaMemberOwner {
        let owner = match owner {
            LuaMemberOwner::Element(range) => {
                derivation::note_definition_site_read(range);
                self.definition_site_owner.get(range).unwrap_or(owner)
            }
            _ => owner,
        };
        match owner {
            LuaMemberOwner::GlobalPath(path) => {
                derivation::note_global_path_read(path);
                self.global_path_class_owner.get(path).unwrap_or(owner)
            }
            _ => owner,
        }
    }

    /// Records or drops `id`'s global-path provenance for a homing decision.
    ///
    /// A write that named global path `path` directly, or that filed the member
    /// under a table literal initialising one, makes the member a path member:
    /// it must follow the path when its winning `---@class` changes. A member
    /// re-homed onto the path's current canonical owner keeps its provenance;
    /// one re-homed anywhere else is no longer a path member and loses it.
    fn note_member_path_provenance(
        &mut self,
        id: LuaMemberId,
        raw_owner: &LuaMemberOwner,
        homing: Homing,
    ) {
        if let Some(path) = raw_owner.get_path() {
            self.stamp_member_path_provenance(id, path, homing);
            return;
        }
        let canonical = self.canonical(raw_owner).clone();
        if let Some(path) = canonical.get_path() {
            self.stamp_member_path_provenance(id, path, homing);
            return;
        }
        let Some(recorded) = self.member_path_provenance.get(&id).cloned() else {
            return;
        };
        let recorded_canonical = self
            .canonical(&LuaMemberOwner::GlobalPath(recorded))
            .clone();
        if recorded_canonical != canonical {
            self.member_path_provenance.remove(&id);
            self.note_object_write(id, homing);
        }
    }

    /// Writes `id`'s provenance when it differs, so a re-file that re-derives
    /// the same path does not stamp the member-object domain.
    fn stamp_member_path_provenance(&mut self, id: LuaMemberId, path: &GlobalId, homing: Homing) {
        if self.member_path_provenance.get(&id) == Some(path) {
            return;
        }
        self.member_path_provenance.insert(id, path.clone());
        self.note_object_write(id, homing);
    }

    /// Drops `id`'s provenance when it is detached from the owner the
    /// provenance names, so a final detachment cannot leave a member the next
    /// class flip would resurrect onto a path it left.
    fn drop_member_path_provenance_on_detach(
        &mut self,
        detached_from: &LuaMemberOwner,
        id: LuaMemberId,
    ) {
        let Some(recorded) = self.member_path_provenance.get(&id).cloned() else {
            return;
        };
        let provenance_canonical = self
            .canonical(&LuaMemberOwner::GlobalPath(recorded))
            .clone();
        if provenance_canonical == *detached_from {
            self.member_path_provenance.remove(&id);
            self.note_member_object_write(id);
        }
    }

    /// The owner a path member's homing actually lands on.
    ///
    /// A path and its annotated classes are one table. A write that homes a
    /// path-provenance member onto a class the path has already outranked
    /// names the same table through a losing name — a re-indexed file still
    /// binding its own (losing) class does this on every ripple — so the
    /// winner decides and the member homes under it instead. Without this the
    /// members surface under both classes. Homing to any other owner is a
    /// genuine move and is left alone.
    fn effective_path_member_owner(
        &self,
        id: LuaMemberId,
        owner: &LuaMemberOwner,
    ) -> Option<LuaMemberOwner> {
        let class = owner.get_type_id()?;
        derivation::note_member_marks_read(id);
        let provenance = self.member_path_provenance.get(&id)?;
        derivation::note_global_path_read(provenance);
        let candidates = self.global_path_class_candidates.get(provenance)?;
        let winner = candidates.iter().next()?.class.clone();
        if *class == winner {
            return None;
        }
        candidates
            .iter()
            .any(|candidate| candidate.class == *class)
            .then(|| LuaMemberOwner::Type(winner))
    }

    /// Records that `path`'s declaration is annotated `---@class`, and moves
    /// the path's members onto the winning class.
    ///
    /// A global with a class annotation has two names for one table, and every
    /// reader of the annotated type looks the members up on the class. Every
    /// proposal is recorded, and the winner is the name-lexicographic-min
    /// candidate (ties broken by file id and class identity), so two files
    /// annotating one global do not depend on which was analysed first. When
    /// the winner changes, the members already on the losing class move to the
    /// winner; members still parked on the path itself move too.
    pub fn set_global_path_class(
        &mut self,
        path: GlobalId,
        class: crate::LuaTypeDeclId,
        file_id: FileId,
    ) {
        let candidate = GlobalPathClassCandidate {
            class_name: class.get_name().into(),
            file_id,
            class,
        };
        let holder = if derivation::writes_source() {
            None
        } else {
            derivation::current()
        };
        if let Some(id) = holder {
            derivation::note_write(|| FactKey::PathClassContrib(id));
            self.derived_path_classes
                .entry(id)
                .or_default()
                .push((path.clone(), candidate.clone()));
        }
        let holders = self
            .path_class_holders
            .entry((path.clone(), candidate.clone()))
            .or_default();
        holders.push(holder);
        if holders.len() > 1 {
            return;
        }
        self.global_path_class_candidates
            .entry(path.clone())
            .or_default()
            .insert(candidate);
        derivation::note_write(|| FactKey::GlobalPath(path.clone()));
        self.reselect_global_path_class(&path);
    }

    /// Withdraws the class candidates derivation `id` proposed, before it
    /// runs again.
    pub(crate) fn retract_path_classes(&mut self, id: &DerivationId) {
        let Some(proposals) = self.derived_path_classes.remove(id) else {
            return;
        };
        let mut affected = Vec::new();
        for (path, candidate) in proposals {
            if self.release_path_class(&path, candidate, Some(*id)) {
                affected.push(path);
            }
        }
        affected.sort_unstable_by(|left, right| left.get_name().cmp(right.get_name()));
        affected.dedup();
        for path in affected {
            self.reselect_global_path_class(&path);
        }
    }

    /// Drops one hold on a candidate; returns whether the candidate left.
    fn release_path_class(
        &mut self,
        path: &GlobalId,
        candidate: GlobalPathClassCandidate,
        holder: Option<DerivationId>,
    ) -> bool {
        let key = (path.clone(), candidate);
        let Some(holders) = self.path_class_holders.get_mut(&key) else {
            return false;
        };
        let Some(at) = holders.iter().position(|existing| *existing == holder) else {
            return false;
        };
        holders.remove(at);
        if !holders.is_empty() {
            return false;
        }
        let (path, candidate) = key;
        self.path_class_holders
            .remove(&(path.clone(), candidate.clone()));
        derivation::note_write(|| FactKey::GlobalPath(path.clone()));
        if let Some(candidates) = self.global_path_class_candidates.get_mut(&path) {
            candidates.remove(&candidate);
            if candidates.is_empty() {
                self.global_path_class_candidates.remove(&path);
            }
        }
        true
    }

    /// Installs the winning candidate as `path`'s class and moves the path's
    /// members to it; with no candidate left the members go back onto the
    /// path itself.
    fn reselect_global_path_class(&mut self, path: &GlobalId) {
        let winner = self
            .global_path_class_candidates
            .get(path)
            .and_then(|candidates| candidates.iter().next())
            .map(|candidate| candidate.class.clone());
        let old = self
            .global_path_class_owner
            .get(path)
            .and_then(LuaMemberOwner::get_type_id)
            .cloned();
        if old == winner {
            return;
        }
        Self::note_canonical_write(path, None, &[old.as_ref(), winner.as_ref()]);
        // Moving the members is the index's own placement: whichever write
        // changed the candidates did not read where they lived, nor produce
        // them.
        derivation::untracked(|| {
            derivation::rehoming(|| match winner {
                Some(winner) => {
                    self.global_path_class_owner
                        .insert(path.clone(), LuaMemberOwner::Type(winner.clone()));
                    self.rehome_global_path_class(
                        path,
                        old.as_ref(),
                        &LuaMemberOwner::Type(winner),
                    );
                }
                None => {
                    self.global_path_class_owner.remove(path);
                    self.rehome_global_path_class(
                        path,
                        old.as_ref(),
                        &LuaMemberOwner::GlobalPath(path.clone()),
                    );
                }
            })
        })
    }

    /// Moves the members of `path` between its two homes: from the path slot
    /// and, when one is losing it, from the losing class, onto `to`.
    ///
    /// Only members that reached their current home through `path` move:
    /// members homed onto the losing class by any other route (a write through
    /// a genuinely class-typed value) stay put, as do members whose owner was
    /// decided by scripted-class synthesis. Owner-only members move only when
    /// their provenance names `path` — a write through the path or an alias of
    /// it — since an owner-only write through a genuinely class-typed value is
    /// not a path member. The moved member's item is pruned from the owner it leaves —
    /// `set_member_owner` rewrites the key indexes but leaves owner items
    /// behind — so the losing class stops listing path members.
    fn rehome_global_path_class(
        &mut self,
        path: &GlobalId,
        from: Option<&crate::LuaTypeDeclId>,
        to: &LuaMemberOwner,
    ) {
        let path_owner = LuaMemberOwner::GlobalPath(path.clone());
        let mut member_ids = self
            .owner_members
            .get(&path_owner)
            .into_iter()
            .flat_map(|members| members.get_member_items())
            .flat_map(member_ids_from_item)
            .filter(|id| {
                !self.is_pinned(id)
                    && self.member_current_owner.get(id) == Some(&path_owner)
                    && (!self.owner_only_members.contains(id)
                        || self.member_path_provenance.get(id) == Some(path))
            })
            .collect::<Vec<_>>();

        if let Some(old) = from {
            let old_owner = LuaMemberOwner::Type(old.clone());
            member_ids.extend(
                self.owner_members
                    .get(&old_owner)
                    .into_iter()
                    .flat_map(|members| members.get_member_items())
                    .flat_map(member_ids_from_item)
                    .filter(|id| {
                        !self.is_pinned(id)
                            && self.member_current_owner.get(id) == Some(&old_owner)
                            && self.member_path_provenance.get(id) == Some(path)
                    }),
            );
        }

        member_ids.sort_unstable_by_key(|id| member_id_sort_key(*id));
        member_ids.dedup();
        for member_id in member_ids {
            self.write_filed(member_id, |index| {
                index.remove_member_from_owner_item(&path_owner, member_id);
                if let Some(old) = from {
                    index.remove_member_from_owner_item(
                        &LuaMemberOwner::Type(old.clone()),
                        member_id,
                    );
                }
                index.home_member(to.clone(), member_id.file_id, member_id, Homing::Filed);
                index.list_member(to.clone(), member_id, Homing::Filed);
            });
        }
        if *to != path_owner {
            self.vacate_bound_path_owner(&path_owner);
        }
    }

    /// Whether a synthesis pinned `id` to the class it transferred it to.
    fn is_pinned(&self, id: &LuaMemberId) -> bool {
        self.member_transfers
            .get(id)
            .is_some_and(|transfer| transfer.pinned)
    }

    /// Drops every item and key-index entry a class-bound path's own key
    /// holds for members homed elsewhere.
    ///
    /// Every reader canonicalises the path to its class, so what is left
    /// under the path itself is only a trace of whether its members were
    /// filed before or after the class was bound.
    fn vacate_bound_path_owner(&mut self, path_owner: &LuaMemberOwner) {
        let listed = self
            .owner_members
            .get(path_owner)
            .into_iter()
            .flat_map(|members| members.get_member_items())
            .flat_map(member_ids_from_item);
        let indexed = [
            &self.member_owner_key_index,
            &self.member_owner_key_history_index,
        ]
        .into_iter()
        .filter_map(|index| index.get(path_owner))
        .flat_map(|slots| slots.values().flatten().copied());
        let mut member_ids = listed
            .chain(indexed)
            .filter(|id| self.member_current_owner.get(id) != Some(path_owner))
            .collect::<Vec<_>>();
        member_ids.sort_unstable_by_key(|id| member_id_sort_key(*id));
        member_ids.dedup();
        for member_id in member_ids {
            self.remove_member_from_all_owner_key_indexes(path_owner, member_id);
            self.remove_member_from_owner_item(path_owner, member_id);
        }
    }

    /// Drops `id` from the member item it occupies under `owner`, and the
    /// owner once its map is empty, without touching the owner-key indexes or
    /// the member itself.
    fn remove_member_from_owner_item(&mut self, owner: &LuaMemberOwner, id: LuaMemberId) {
        let Some(key) = self.get_member(&id).map(|member| member.get_key().clone()) else {
            return;
        };
        let Some(owner_members) = self.owner_members.get_mut(owner) else {
            return;
        };
        if owner_members.remove_id(&key, id) {
            note_slot_write(owner, &key);
            note_owner_map_write(owner);
        }
        if owner_members.is_empty() {
            self.remove_owner_entry(owner);
        }
    }

    fn remove_file_definition_sites(&mut self, file_id: FileId) {
        let Some(sites) = self.definition_sites_by_file.remove(&file_id) else {
            return;
        };
        for site in sites {
            self.forget_definition_site(&site);
        }
    }

    fn forget_definition_site(&mut self, site: &crate::InFiled<TextRange>) {
        let Some(path) = self
            .definition_site_owner
            .remove(site)
            .and_then(|owner| owner.get_path().cloned())
        else {
            return;
        };
        Self::note_canonical_write(&path, Some(site), &[]);
        if let Some(sites) = self.path_definition_sites.get_mut(&path) {
            sites.retain(|held| held != site);
            if sites.is_empty() {
                self.path_definition_sites.remove(&path);
            }
        }
        if let Some(sites) = self.definition_sites_by_file.get_mut(&site.file_id) {
            sites.retain(|held| held != site);
            if sites.is_empty() {
                self.definition_sites_by_file.remove(&site.file_id);
            }
        }
    }

    /// Every path with at least one definition site, and its sites, in a stable
    /// order. Used by the determinism harness to observe that ownership does
    /// not depend on index order.
    pub fn sorted_definition_sites(&self) -> Vec<(&GlobalId, &[crate::InFiled<TextRange>])> {
        let mut entries: Vec<(&GlobalId, &[crate::InFiled<TextRange>])> = self
            .path_definition_sites
            .iter()
            .map(|(path, sites)| (path, sites.as_slice()))
            .collect();
        entries.sort_unstable_by(|left, right| left.0.get_name().cmp(right.0.get_name()));
        entries
    }

    pub fn sorted_global_path_owners(&self) -> Vec<GlobalId> {
        let mut global_ids = self
            .owner_members
            .keys()
            .filter_map(|owner| match owner {
                LuaMemberOwner::GlobalPath(global_id) => Some(global_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        global_ids.sort_unstable_by(|left, right| left.get_name().cmp(right.get_name()));
        global_ids
    }

    /// Every table-literal range in `file_id` that currently owns a member.
    ///
    /// Used when the file is removed: its literals are all gone, and members
    /// other files own on them are not reachable from any file the removal
    /// sweep visits.
    pub fn element_owner_ranges_in_file(&self, file_id: FileId) -> Vec<crate::InFiled<TextRange>> {
        derivation::note_read(|| FactKey::MemberFile(file_id));
        let mut ranges: Vec<crate::InFiled<TextRange>> = self
            .owner_members
            .file_entries(file_id)
            .filter_map(|(owner, _)| match owner {
                LuaMemberOwner::Element(range) => Some(range.clone()),
                _ => None,
            })
            .collect();
        ranges.sort_unstable_by_key(|range| (range.value.start(), range.value.end()));
        ranges
    }

    /// Every table-literal range that currently owns at least one member.
    #[cfg(test)]
    pub(crate) fn element_owner_ranges(&self) -> Vec<crate::InFiled<TextRange>> {
        let mut ranges: Vec<crate::InFiled<TextRange>> = self
            .owner_members
            .iter()
            .filter(|(_, members)| !members.is_empty())
            .filter_map(|(owner, _)| match owner {
                LuaMemberOwner::Element(range) => Some(range.clone()),
                _ => None,
            })
            .collect();
        ranges
            .sort_unstable_by_key(|range| (range.file_id, range.value.start(), range.value.end()));
        ranges
    }

    pub fn get_members(&self, owner: &LuaMemberOwner) -> Option<Vec<&LuaMember>> {
        let owner = self.canonical(owner);
        note_owner_map_read(owner);
        let owner_members = self.owner_members.get(owner)?;
        if owner_members.get_member_len() == 0 {
            return Some(Vec::new());
        }

        if owner_members.get_member_len() == 1 {
            match owner_members.get_member_items().next()? {
                LuaMemberIndexItem::One(id) => {
                    return Some(self.get_member(id).into_iter().collect());
                }
                LuaMemberIndexItem::Many(_) => {}
            }
        }

        Some(
            owner_members
                .sorted_member_ids()
                .iter()
                .filter_map(|member_id| self.get_member(member_id))
                .collect(),
        )
    }

    /// The owner's members that are not methods, in the order
    /// [`Self::get_members`] would yield them.
    pub fn get_field_members(&self, owner: &LuaMemberOwner) -> Option<Vec<&LuaMember>> {
        let owner = self.canonical(owner);
        derivation::note_read(|| FactKey::MemberOwnerFields(owner.clone()));
        let owner_members = self.owner_members.get(owner)?;
        Some(
            self.field_member_ids(owner_members)
                .filter_map(|member_id| self.get_member(&member_id))
                .collect(),
        )
    }

    fn field_member_ids<'a>(
        &'a self,
        owner_members: &'a LuaOwnerMembers,
    ) -> impl Iterator<Item = LuaMemberId> + 'a {
        owner_members
            .sorted_member_ids()
            .iter()
            .copied()
            .filter(|member_id| {
                self.members
                    .get(member_id)
                    .is_some_and(|member| !member.get_feature().is_method_decl())
            })
    }

    /// The owner's members whose key is an expression rather than a name, in
    /// the same order [`Self::get_members`] would yield them.
    pub fn get_expr_key_members(&self, owner: &LuaMemberOwner) -> Option<Vec<&LuaMember>> {
        let owner = self.canonical(owner);
        derivation::note_read(|| FactKey::MemberOwnerExprKeys(owner.clone()));
        let owner_members = self.owner_members.get(owner)?;
        Some(
            owner_members
                .sorted_expr_key_member_ids()
                .iter()
                .filter_map(|member_id| self.get_member(member_id))
                .collect(),
        )
    }

    /// The owner's members under one key, in the same order
    /// [`Self::get_members`] would yield them.
    ///
    /// Returns `None` only when the owner has no member map at all, so a
    /// caller can still tell "no such owner" from "owner without that key".
    pub fn get_members_with_key(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
    ) -> Option<Vec<&LuaMember>> {
        let owner = self.canonical(owner);
        note_slot_read(owner, key);
        let Some(owner_members) = self.owner_members.get(owner) else {
            note_owner_presence_read(owner);
            return None;
        };
        let Some(item) = owner_members.get_member(key) else {
            note_owner_presence_read(owner);
            return Some(Vec::new());
        };
        Some(
            item.member_ids_slice()
                .iter()
                .filter_map(|member_id| self.get_member(member_id))
                .collect(),
        )
    }

    pub fn get_member_keys<'a>(
        &'a self,
        owner: &LuaMemberOwner,
    ) -> impl Iterator<Item = &'a LuaMemberKey> + 'a {
        let owner = self.canonical(owner);
        note_owner_map_read(owner);
        self.owner_members
            .get(owner)
            .into_iter()
            .flat_map(LuaOwnerMembers::get_member_keys)
    }

    pub fn get_member_item_by_member_id(
        &self,
        member_id: LuaMemberId,
    ) -> Option<&LuaMemberIndexItem> {
        derivation::note_member_home_read(member_id);
        derivation::note_member_object_read(member_id);
        let owner = self.member_current_owner.get(&member_id)?;
        let member_key = self.members.get(&member_id)?.get_key();
        note_slot_read(owner, member_key);
        let member_items = self.owner_members.get(owner)?;
        let item = member_items.get_member(member_key)?;
        Some(item)
    }

    pub fn get_member_item(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
    ) -> Option<&LuaMemberIndexItem> {
        let owner = self.canonical(owner);
        note_slot_read(owner, key);
        self.owner_members
            .get(owner)
            .and_then(|map| map.get_member(key))
    }

    pub fn get_member_len(&self, owner: &LuaMemberOwner) -> usize {
        let owner = self.canonical(owner);
        note_owner_map_read(owner);
        self.owner_members
            .get(owner)
            .map_or(0, |map| map.get_member_len())
    }

    /// Whether the owner holds at least one member that still resolves.
    ///
    /// Not the same as a non-zero [`Self::get_member_len`]: removing a member
    /// whose entry is already gone leaves its id behind under the owner, so a
    /// key can outlive the member it points at. Stops at the first live id
    /// rather than materialising the owner's whole member list.
    pub fn has_live_member(&self, owner: &LuaMemberOwner) -> bool {
        let owner = self.canonical(owner);
        note_owner_map_read(owner);
        let Some(owner_members) = self.owner_members.get(owner) else {
            return false;
        };
        owner_members.get_member_items().any(|item| match item {
            LuaMemberIndexItem::One(id) => self.get_member(id).is_some(),
            LuaMemberIndexItem::Many(ids) => ids.iter().any(|id| self.get_member(id).is_some()),
        })
    }

    pub fn get_current_owner(&self, id: &LuaMemberId) -> Option<&LuaMemberOwner> {
        derivation::note_member_home_read(*id);
        self.member_current_owner.get(id)
    }

    /// Whether `id` was written through an instance of its owner class and so
    /// is not listed among the class's members.
    pub fn is_member_owner_only(&self, id: &LuaMemberId) -> bool {
        derivation::note_member_home_read(*id);
        self.owner_only_members.contains(id)
    }

    pub fn get_member_owner(&self, id: &LuaMemberId) -> Option<&LuaMemberOwner> {
        derivation::note_member_home_read(*id);
        self.member_current_owner.get(id)
    }

    pub fn visible_member_count_for_owner_key(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
    ) -> usize {
        let owner = self.canonical(owner);
        note_slot_read(owner, key);
        self.member_owner_key_index
            .get(owner)
            .and_then(|owner_items| owner_items.get(key))
            .map(|member_ids| member_ids.len())
            .unwrap_or(0)
    }

    pub fn has_visible_member_for_owner_key_other_than(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
        excluded_member_id: LuaMemberId,
    ) -> bool {
        let owner = self.canonical(owner);
        note_slot_read(owner, key);
        let Some(member_ids) = self
            .member_owner_key_index
            .get(owner)
            .and_then(|owner_items| owner_items.get(key))
        else {
            return false;
        };

        match member_ids.as_slice() {
            [] => false,
            [member_id] => *member_id != excluded_member_id,
            _ => true,
        }
    }

    pub fn get_members_for_owner_key(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
    ) -> Vec<&LuaMember> {
        let owner = self.canonical(owner);
        note_slot_read(owner, key);
        let Some(owner_items) = self.member_owner_key_index.get(owner) else {
            return Vec::new();
        };
        let Some(member_ids) = owner_items.get(key) else {
            return Vec::new();
        };

        member_ids
            .iter()
            .copied()
            .filter_map(|member_id| {
                self.member_current_owner
                    .get(&member_id)
                    .filter(|current_owner| *current_owner == owner)?;
                self.get_member(&member_id)
            })
            .collect()
    }
    /// The number of live members keyed under `(owner, key)` — the writer
    /// count [`get_members_for_owner_key`](Self::get_members_for_owner_key)
    /// would return, without materialising the list. Read-versioning calls
    /// this every read, so it must not allocate or record a member map.
    pub fn get_member_count_for_owner_key(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
    ) -> usize {
        let owner = self.canonical(owner);
        note_slot_read(owner, key);
        let Some(member_ids) = self
            .member_owner_key_index
            .get(owner)
            .and_then(|owner_items| owner_items.get(key))
        else {
            return 0;
        };
        member_ids
            .iter()
            .filter(|member_id| {
                self.member_current_owner
                    .get(member_id)
                    .is_some_and(|current_owner| current_owner == owner)
                    && self.get_member(member_id).is_some()
            })
            .count()
    }

    /// Every member ever keyed under `owner`, including ones hidden by the
    /// latest-assignment view and ones since re-homed to a concrete owner.
    pub fn get_member_history(&self, owner: &LuaMemberOwner) -> Vec<&LuaMember> {
        let owner = self.canonical(owner);
        note_owner_map_read(owner);
        let Some(owner_items) = self.member_owner_key_history_index.get(owner) else {
            return Vec::new();
        };

        let mut member_ids = owner_items.values().flatten().copied().collect::<Vec<_>>();
        member_ids.sort_unstable_by_key(|member_id| member_id_sort_key(*member_id));
        member_ids.dedup();
        member_ids
            .into_iter()
            .filter_map(|member_id| self.get_member(&member_id))
            .collect()
    }

    /// The subset of [`get_member_history`](Self::get_member_history) whose
    /// own global path is `global_id`, in the same order.
    pub fn get_member_history_for_global_path(
        &self,
        owner: &LuaMemberOwner,
        global_id: &GlobalId,
    ) -> Vec<LuaMemberId> {
        let owner = self.canonical(owner);
        // The member key is the part of the path below the owner, so the members
        // declaring it are one bucket of the owner's history rather than all of
        // it. Reading the whole history to filter it built and sorted every
        // member the owner has ever held, once per resolution event, against
        // paths that gain a member per assignment.
        let Some(owner_items) = self.member_owner_key_history_index.get(owner) else {
            note_owner_map_read(owner);
            return Vec::new();
        };
        let name = global_id.get_name();
        // The segment below the owner: a string key that is no name, such as
        // `a.b` in `T["a.b"] = v`, is one bracketed segment.
        let segment = match owner {
            LuaMemberOwner::GlobalPath(owner_id) => name
                .strip_prefix(owner_id.get_name())
                .and_then(|rest| rest.strip_prefix('.'))
                .unwrap_or(name),
            _ => glua_parser::split_last_path_segment(name).map_or(name, |(_, last)| last),
        };

        let mut matched = Vec::new();
        let collect = |key: &LuaMemberKey, matched: &mut Vec<LuaMemberId>| {
            note_slot_read(owner, key);
            let Some(member_ids) = owner_items.get(key) else {
                return;
            };
            matched.extend(member_ids.iter().copied().filter(|member_id| {
                self.get_member(member_id)
                    .and_then(|member| member.get_global_id())
                    == Some(global_id)
            }));
        };
        match glua_parser::path_segment_string_key(segment) {
            Some(key) => collect(&LuaMemberKey::Name(key.into()), &mut matched),
            None => {
                collect(&LuaMemberKey::Name(segment.into()), &mut matched);
                // A numeric field is keyed by its integer, not by its spelling.
                if let Ok(index) = segment.parse::<i64>() {
                    collect(&LuaMemberKey::Integer(index), &mut matched);
                }
            }
        }

        matched.sort_unstable_by_key(|member_id| member_id_sort_key(*member_id));
        matched.dedup();
        matched
    }

    pub(crate) fn iter_current_owner_keys(
        &self,
    ) -> impl Iterator<Item = (&LuaMemberOwner, &LuaMemberKey)> {
        self.member_owner_key_index
            .iter()
            .flat_map(|(owner, members_by_key)| members_by_key.keys().map(move |key| (owner, key)))
    }

    pub fn add_function_scope_range(&mut self, file_id: FileId, range: TextRange) {
        self.function_scope_ranges
            .entry(file_id)
            .or_default()
            .insert(range);
        self.fold_file_fingerprint(file_id, file_scope_element(file_id, range), true);
    }

    pub fn enclosing_function_scope_range(
        &self,
        file_id: FileId,
        position: TextSize,
    ) -> Option<TextRange> {
        derivation::note_function_scope_read(file_id, position);
        self.innermost_function_scope(file_id, position)
    }

    fn innermost_function_scope(&self, file_id: FileId, position: TextSize) -> Option<TextRange> {
        self.function_scope_ranges
            .get(&file_id)?
            .innermost(position)
    }

    pub fn set_member_function_scope_range(
        &mut self,
        member_id: LuaMemberId,
        range: Option<TextRange>,
    ) {
        self.snapshot_member(member_id);
        let changed = if let Some(range) = range {
            self.member_function_scope_ranges.insert(member_id, range) != Some(range)
        } else {
            self.member_function_scope_ranges
                .remove(&member_id)
                .is_some()
        };
        if changed {
            self.note_member_move(member_id);
        }
    }

    pub fn member_function_scope_range(&self, member_id: LuaMemberId) -> Option<TextRange> {
        derivation::note_member_marks_read(member_id);
        self.member_function_scope_ranges.get(&member_id).copied()
    }

    pub fn mark_non_overwriting_assignment_member(&mut self, member_id: LuaMemberId) {
        self.snapshot_member(member_id);
        if self.non_overwriting_assignment_members.insert(member_id) {
            self.note_member_move(member_id);
        }
        self.readmit_preserved_assignment_co_writers(member_id);
    }

    /// Re-admits the co-writers this member's slot lost before the member was
    /// known to be a guarded self-assignment (`t.k = t.k or {}`).
    ///
    /// A write is marked *after* it is inserted, so a writer that arrives
    /// before its mark can leave a slot its co-writers no longer cover: on a
    /// cold build the co-writer is in the same batch and its own mark merges
    /// it back, while re-indexing one file can drop a co-writer the batch
    /// does not re-analyse, which nothing then restores. Re-applying the
    /// accumulate rule here is the answer a different arrival order would
    /// have produced, so both paths settle on it.
    ///
    /// Once a full scan of a slot merges nothing, later scans merge nothing
    /// either, so the slot is remembered in
    /// [`Self::preserved_co_writer_reconciled`] to skip them: every
    /// production history write pairs with an item merge (`add_member`,
    /// `add_member_to_owner`, `add_member_alias_to_owner`, `rekey_member`,
    /// and `set_member_owner` only through those callers), every removal
    /// touches both sides for the same id, and marking only ever covers an
    /// already-listed member. Any future history write without a paired item
    /// merge must clear that slot's entry.
    ///
    /// Exception: [`Self::set_member_owner_only`] calls `set_member_owner`
    /// without an item merge (see the `set_member_owner_only` body), filing a
    /// history entry the item never lists. That bare write is safe only
    /// because readmission skips [`Self::owner_only_members`] when merging
    /// co-writers back, so the unlisted id can never be merged in later.
    fn readmit_preserved_assignment_co_writers(&mut self, member_id: LuaMemberId) -> Option<()> {
        if !self.is_preserved_assignment_member(member_id) {
            return None;
        }
        let owner = self.member_current_owner.get(&member_id)?.clone();
        let key = self.get_member(&member_id)?.get_key().clone();
        if self
            .preserved_co_writer_reconciled
            .contains(&(owner.clone(), key.clone()))
        {
            return Some(());
        }
        let visible = self
            .owner_members
            .get(&owner)
            .and_then(|owner_members| owner_members.get_member(&key))
            .map(member_ids_from_item)?;
        // Only a slot whose every writer is a guarded assignment accumulates
        // them all; one holding a real writer keeps deciding by the rules that
        // put it there.
        if !visible
            .iter()
            .all(|id| self.is_preserved_assignment_member(*id))
        {
            return None;
        }
        let visible_set: HashSet<LuaMemberId> = visible.iter().copied().collect();
        let mut merged_any = false;
        for co_writer in self.get_current_owner_member_ids_for_key_unsorted(&owner, &key) {
            if co_writer != member_id
                && !visible_set.contains(&co_writer)
                && !self.owner_only_members.contains(&co_writer)
                && self.is_preserved_assignment_member(co_writer)
            {
                self.merge_member_into_owner_item(owner.clone(), key.clone(), co_writer);
                merged_any = true;
            }
        }
        if !merged_any {
            self.preserved_co_writer_reconciled.insert((owner, key));
        }
        Some(())
    }

    fn is_preserved_assignment_member(&self, member_id: LuaMemberId) -> bool {
        self.non_overwriting_assignment_members.contains(&member_id)
            && self.is_assignment_file_define_member(member_id)
    }

    pub fn is_non_overwriting_assignment_member(&self, member_id: LuaMemberId) -> bool {
        derivation::note_member_marks_read(member_id);
        self.non_overwriting_assignment_members.contains(&member_id)
    }

    pub fn get_current_owner_members_for_key(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
    ) -> Vec<&LuaMember> {
        let mut members = self
            .get_current_owner_member_ids_for_key_unsorted(owner, key)
            .into_iter()
            .filter_map(|member_id| self.get_member(&member_id))
            .collect::<Vec<_>>();
        members.sort_by_key(|member| member_id_sort_key(member.get_id()));
        members
    }

    /// History-slot ids still homed under `owner`/`key`, in the slot's id
    /// order.
    ///
    /// The unsorted form of
    /// [`get_current_owner_members_for_key`](Self::get_current_owner_members_for_key):
    /// per-write callers that only need membership or a minimum must not pay
    /// its sort on every write.
    pub fn get_current_owner_member_ids_for_key_unsorted(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
    ) -> Vec<LuaMemberId> {
        let owner = self.canonical(owner);
        note_slot_read(owner, key);
        let Some(member_ids) = self
            .member_owner_key_history_index
            .get(owner)
            .and_then(|owner_items| owner_items.get(key))
        else {
            return Vec::new();
        };

        member_ids
            .iter()
            .copied()
            .filter(|member_id| {
                self.member_current_owner
                    .get(member_id)
                    .is_some_and(|current_owner| current_owner == owner)
                    && self
                        .get_member(member_id)
                        .is_some_and(|member| member.get_key() == key)
            })
            .collect()
    }

    /// Returns every still-current member ever indexed under `owner`, including
    /// assignment history entries hidden by the normal latest-value view.
    ///
    /// This is intended for metadata synthesis that must migrate a bounded
    /// owner region wholesale. Ordinary semantic lookup should continue using
    /// `get_members`, which applies runtime overwrite visibility.
    pub fn get_current_owner_member_history(&self, owner: &LuaMemberOwner) -> Vec<&LuaMember> {
        let owner = self.canonical(owner);
        note_owner_map_read(owner);
        let Some(member_ids) = self.current_owner_member_history.get(owner) else {
            return Vec::new();
        };
        member_ids
            .iter()
            .filter_map(|member_id| self.get_member(member_id))
            .collect()
    }

    fn remove_current_owner_member(&mut self, owner: &LuaMemberOwner, member_id: LuaMemberId) {
        let removed = {
            let Some(members) = self.current_owner_member_history.get_mut(owner) else {
                return;
            };
            let removed = members.remove_sorted(member_id);
            if members.is_empty() {
                self.current_owner_member_history.remove(owner);
            }
            removed
        };
        if removed {
            self.fold_history_fingerprint(owner, current_history_element(member_id), false);
        }
    }

    pub fn get_current_members_for_key(&self, key: &LuaMemberKey) -> Vec<&LuaMember> {
        derivation::note_read(|| FactKey::MemberKey(key.clone()));
        let Some(member_ids) = self.current_members_by_key.get(key) else {
            return Vec::new();
        };

        member_ids
            .iter()
            .filter_map(|member_id| self.get_member(member_id))
            .collect()
    }

    fn add_current_member_key(&mut self, member_id: LuaMemberId) {
        let Some(key) = self
            .get_member(&member_id)
            .map(|member| member.get_key().clone())
        else {
            return;
        };
        for file_id in expr_key_files(&key, member_id) {
            self.expr_key_members_by_file
                .entry(file_id)
                .or_default()
                .insert(member_id);
        }
        if self
            .current_members_by_key
            .entry(key.clone())
            .or_default()
            .insert_sorted(member_id)
        {
            derivation::note_write(|| FactKey::MemberKey(key.clone()));
            fold_fingerprint(
                &mut self.key_fingerprints,
                key.clone(),
                fingerprint_of(&member_id),
                true,
            );
        }
    }

    fn remove_current_member_key(&mut self, member_id: LuaMemberId) {
        let Some(key) = self
            .get_member(&member_id)
            .map(|member| member.get_key().clone())
        else {
            return;
        };
        for file_id in expr_key_files(&key, member_id) {
            if let Some(members) = self.expr_key_members_by_file.get_mut(&file_id) {
                members.remove(&member_id);
                if members.is_empty() {
                    self.expr_key_members_by_file.remove(&file_id);
                }
            }
        }
        let removed = {
            let Some(members) = self.current_members_by_key.get_mut(&key) else {
                return;
            };
            let removed = members.remove_sorted(member_id);
            if members.is_empty() {
                self.current_members_by_key.remove(&key);
            }
            removed
        };
        if removed {
            derivation::note_write(|| FactKey::MemberKey(key.clone()));
            fold_fingerprint(
                &mut self.key_fingerprints,
                key.clone(),
                fingerprint_of(&member_id),
                false,
            );
        }
    }

    /// The members filed under `file_id`, sorted.
    pub(crate) fn file_member_ids(&self, file_id: FileId) -> Vec<LuaMemberId> {
        derivation::note_read(|| FactKey::MemberFile(file_id));
        let mut ids = self
            .members
            .file_entries(file_id)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        ids.sort_by_key(|id| member_id_sort_key(*id));
        ids
    }

    /// The members filed under `file_id`, sorted.
    pub fn get_file_members(&self, file_id: FileId) -> Vec<&LuaMember> {
        derivation::note_read(|| FactKey::MemberFile(file_id));
        let mut members = self
            .members
            .file_entries(file_id)
            .map(|(id, member)| {
                derivation::note_member_object_read(*id);
                member
            })
            .collect::<Vec<_>>();
        members.sort_by_key(|member| member_id_sort_key(member.get_id()));
        members
    }
}

fn site_sort_key(site: &crate::InFiled<TextRange>) -> (u32, u32, u32) {
    (
        site.file_id.id,
        site.value.start().into(),
        site.value.end().into(),
    )
}

// The owner-level sorted member-id cache depends on these file id, position,
// range end, and kind components remaining immutable for a member's lifetime.
/// The files other than `member_id`'s own whose positions `key`'s expression
/// type names.
fn expr_key_files(key: &LuaMemberKey, member_id: LuaMemberId) -> Vec<FileId> {
    let LuaMemberKey::ExprType(typ) = key else {
        return Vec::new();
    };
    crate::db_index::r#type::type_referenced_files(typ)
        .into_iter()
        .filter(|file_id| *file_id != member_id.file_id)
        .collect()
}

pub(crate) fn member_id_sort_key(member_id: LuaMemberId) -> (u32, u32, u32, u16, u32) {
    let syntax_id = member_id.get_syntax_id();
    (
        member_id.file_id.id,
        u32::from(member_id.get_position()),
        u32::from(syntax_id.get_range().end()),
        syntax_id.get_kind() as u16,
        member_id.get_slot(),
    )
}

fn sorted_member_pair(first: LuaMemberId, second: LuaMemberId) -> Vec<LuaMemberId> {
    if member_id_sort_key(first) <= member_id_sort_key(second) {
        vec![first, second]
    } else {
        vec![second, first]
    }
}

impl LuaMemberIndex {
    /// Drops every trace of one member.
    ///
    /// The single place that knows the full set of side tables a member
    /// appears in. Removal paths call this rather than listing the tables
    /// themselves, so a new table cannot be added to the index and forgotten
    /// by one path but not the other.
    fn forget_member(&mut self, member_id: LuaMemberId) {
        if self.members.contains_key(&member_id) {
            self.note_member_move(member_id);
        } else {
            self.note_member_home_write(member_id);
        }
        if let Some(owner) = self.member_current_owner.get(&member_id).cloned() {
            self.remove_member_from_all_owner_key_indexes(&owner, member_id);
            self.remove_current_owner_member(&owner, member_id);
            self.remove_current_member_key(member_id);
        }
        let removed = self.members.remove(&member_id).is_some();
        self.source_members.remove(&member_id);
        self.member_current_owner.remove(&member_id);
        self.member_path_provenance.remove(&member_id);
        self.non_overwriting_assignment_members.remove(&member_id);
        self.owner_only_members.remove(&member_id);
        self.filed_homes.remove(&member_id);
        self.note_member_object_write(member_id);
        self.deferred_index_expr_members.remove(&member_id);
        self.member_function_scope_ranges.remove(&member_id);
        if removed {
            self.fold_file_fingerprint(member_id.file_id, file_member_element(member_id), false);
        }
    }

    /// The per-file half of removal: erase every entry keyed or owned by
    /// `file_id`. The whole-index owner-key sweeps are done once per batch in
    /// `remove_files`, not here — running them per file made removal cost
    /// O(files × index) and dominated incremental edits of high fan-in files.
    fn remove_file_owned_entries(&mut self, file_id: FileId) {
        let owners = self.in_filed.remove(&file_id);
        let member_ids = self
            .members
            .file_entries(file_id)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for member_id in member_ids {
            self.forget_member(member_id);
        }
        let transferred = self
            .member_transfers
            .file_entries(file_id)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for member_id in transferred {
            self.member_transfers.remove(&member_id);
            derivation::note_write(|| FactKey::MemberTransfer(member_id));
        }
        if let Some(owners) = owners {
            self.unlist_from_owners(owners, |id| id.file_id == file_id);
        }
        let scopes = self
            .function_scope_ranges
            .remove(&file_id)
            .unwrap_or_default();
        for &range in scopes.ranges() {
            self.fold_file_fingerprint(file_id, file_scope_element(file_id, range), false);
        }
        self.remove_file_definition_sites(file_id);
        self.recompute_global_path_class_winners(file_id);
    }

    /// Unlists every id `gone` accepts from `owners`' items, dropping the keys
    /// and owners left empty.
    fn unlist_from_owners(
        &mut self,
        owners: impl IntoIterator<Item = LuaMemberOwner>,
        gone: impl Fn(LuaMemberId) -> bool,
    ) {
        for owner in owners {
            // Taking an owner mutably invalidates its revision, so an owner
            // listing none of the ids must not be touched: an immutable peek
            // finds what to unlist and keeps the others' reads fresh.
            let unlisted = self
                .owner_members
                .get(&owner)
                .into_iter()
                .flat_map(LuaOwnerMembers::iter)
                .flat_map(|(key, item)| {
                    item.member_ids_slice()
                        .iter()
                        .filter(|id| gone(**id))
                        .map(move |id| (key.clone(), *id))
                })
                .collect::<Vec<_>>();
            if unlisted.is_empty() {
                continue;
            }
            let Some(member_items) = self.owner_members.get_mut(&owner) else {
                continue;
            };
            for (key, id) in &unlisted {
                note_slot_write(&owner, key);
                member_items.remove_id(key, *id);
            }
            note_owner_map_write(&owner);
            if member_items.is_empty() {
                self.remove_owner_entry(&owner);
            }
        }
    }

    /// Recomputes the class winner of every global path this file proposed.
    ///
    /// A proposal is a vote: removing the file drops its vote and the winner
    /// is recomputed from the survivors. A lost winner moves the path's
    /// members onto the surviving class; a lost last proposal removes the
    /// mapping and moves the members back onto the path itself, so nothing is
    /// left stranded under a class no file declares. Per-file rehomes run
    /// before the batch owner-key sweep in [`Self::remove_files`], so the
    /// surviving members' re-written key indexes are never swept away.
    fn recompute_global_path_class_winners(&mut self, file_id: FileId) {
        let mut sourced = self
            .path_class_holders
            .file_entries(file_id)
            .filter(|(_, holders)| holders.contains(&None))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        sourced.sort_unstable_by(|left, right| {
            left.0
                .get_name()
                .cmp(right.0.get_name())
                .then_with(|| left.1.cmp(&right.1))
        });
        let mut affected: Vec<GlobalId> = Vec::new();
        for (path, candidate) in sourced {
            while self
                .path_class_holders
                .get(&(path.clone(), candidate.clone()))
                .is_some_and(|holders| holders.contains(&None))
            {
                if self.release_path_class(&path, candidate.clone(), None) {
                    affected.push(path.clone());
                }
            }
        }
        if affected.is_empty() {
            return;
        }
        affected.dedup();
        for path in affected {
            self.reselect_global_path_class(&path);
        }
    }

    /// Drops an owner entry from the files of `member_ids`, the members it
    /// held.
    ///
    /// An owner is registered under each contributing member's file, so a
    /// single Element owner can be registered by several files at once.
    fn remove_owner_from_files(
        &mut self,
        owner: &LuaMemberOwner,
        member_ids: impl IntoIterator<Item = LuaMemberId>,
    ) {
        let mut files = member_ids
            .into_iter()
            .map(|id| id.file_id)
            .collect::<Vec<_>>();
        files.sort_unstable();
        files.dedup();
        for file_id in files {
            if let Some(owners) = self.in_filed.get_mut(&file_id) {
                owners.remove(owner);
                if owners.is_empty() {
                    self.in_filed.remove(&file_id);
                }
            }
        }
    }

    /// Re-homes every table-literal owner in the edited file onto its new
    /// range, and purges the ones the edit destroyed.
    ///
    /// Returns the members forgotten with a destroyed owner: their cached
    /// types have to come off with them, and their writers have to take them
    /// again.
    pub fn remap_file_element_owners(&mut self, remap: &crate::FileRemap) -> Vec<LuaMemberId> {
        let mut moved: rustc_hash::FxHashMap<crate::InFiled<TextRange>, crate::InFiled<TextRange>> =
            rustc_hash::FxHashMap::default();
        let mut deleted: Vec<crate::InFiled<TextRange>> = Vec::new();
        for range in self.element_owner_ranges_in_file(remap.file_id) {
            match remap.table_range(&range) {
                crate::Remap::Moved(new) => {
                    if new != range {
                        moved.insert(range, new);
                    }
                }
                crate::Remap::Unrelated => {}
                crate::Remap::Lost => deleted.push(range),
            }
        }

        // A definition site is a literal range in the edited file too, and the
        // edit moves it exactly as it moves an element owner's. It owns no
        // members of its own, so a site the edit destroyed dirties nothing:
        // the path keeps every other file's members, and this file re-registers
        // its own sites when it is re-indexed.
        let mut deleted_sites: Vec<crate::InFiled<TextRange>> = Vec::new();
        let sites = self
            .definition_sites_by_file
            .get(&remap.file_id)
            .cloned()
            .unwrap_or_default();
        for site in sites {
            match remap.table_range(&site) {
                crate::Remap::Moved(new) => {
                    if new != site {
                        moved.insert(site, new);
                    }
                }
                crate::Remap::Unrelated => {}
                crate::Remap::Lost => deleted_sites.push(site),
            }
        }
        for site in &deleted_sites {
            self.forget_definition_site(site);
        }

        self.remap_elements(&moved);
        self.remove_deleted_element_owners(&deleted)
    }

    pub fn remap_elements(
        &mut self,
        map: &rustc_hash::FxHashMap<crate::InFiled<TextRange>, crate::InFiled<TextRange>>,
    ) {
        if map.is_empty() {
            return;
        }
        // Driven off the map rather than a scan of every member in the
        // workspace: `owner_members` already indexes members by owner, and the
        // map holds only the literals one edited file shifted.
        // Applied in a fixed order. Where one literal moves onto the range
        // another is vacating, the resulting state depends on which move ran
        // first, and hash-map iteration order is not stable.
        let mut moves: Vec<(&crate::InFiled<TextRange>, &crate::InFiled<TextRange>)> =
            map.iter().collect();
        moves.sort_unstable_by_key(|(old, _)| (old.file_id, old.value.start(), old.value.end()));
        let to_move: Vec<(LuaMemberId, LuaMemberOwner, LuaMemberOwner)> = moves
            .into_iter()
            .flat_map(|(old, new)| {
                let old_owner = LuaMemberOwner::Element(old.clone());
                let new_owner = LuaMemberOwner::Element(new.clone());
                self.owner_members
                    .get(&old_owner)
                    .into_iter()
                    .flat_map(|items| items.get_member_items())
                    .flat_map(|item| match item {
                        LuaMemberIndexItem::One(id) => vec![*id],
                        LuaMemberIndexItem::Many(ids) => ids.clone(),
                    })
                    .map(move |id| (id, old_owner.clone(), new_owner.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        self.remap_definition_sites(map);
        let mut moved_from: HashMap<LuaMemberOwner, Vec<LuaMemberId>> = HashMap::default();
        for (member_id, old_owner, _) in &to_move {
            moved_from
                .entry(old_owner.clone())
                .or_default()
                .push(*member_id);
        }
        for (member_id, old_owner, new_owner) in to_move {
            self.write_filed(member_id, |index| {
                index.detach_filed_member(&old_owner, member_id);
                index.home_member(
                    new_owner.clone(),
                    member_id.file_id,
                    member_id,
                    Homing::Filed,
                );
                index.list_member(new_owner.clone(), member_id, Homing::Filed);
            });
            if matches!(old_owner, LuaMemberOwner::Element(_))
                && self
                    .owner_members
                    .get(&old_owner)
                    .is_none_or(|m| m.is_empty())
            {
                self.remove_owner_entry(&old_owner);
                // `set_member_owner` files the owner entry under the *member's*
                // file, not the range's. For the cross-file case this pass
                // exists for they are different files, and clearing the wrong
                // one leaves a dead owner in the member file's set for the next
                // sweep to act on.
                let moved = moved_from.remove(&old_owner).unwrap_or_default();
                self.remove_owner_from_files(&old_owner, moved);
            }
        }
    }

    /// Re-keys every member filed under an expression key naming a position of
    /// `remap`'s file the edit moved. Returns the members whose key named a
    /// position the edit destroyed: their writers have to take it again.
    pub fn remap_expr_keys(&mut self, remap: &crate::FileRemap) -> Vec<LuaMemberId> {
        let mut lost = Vec::new();
        for member_id in self.members_with_moved_expr_keys(remap) {
            let Some(LuaMemberKey::ExprType(typ)) =
                self.members.get(&member_id).map(LuaMember::get_key)
            else {
                continue;
            };
            let result = crate::db_index::remap_identities_in_type(typ, remap);
            if result.lost {
                lost.push(member_id);
            } else if let Some(moved) = result.typ {
                self.rekey_member(member_id, LuaMemberKey::ExprType(moved));
            }
        }
        lost
    }

    /// The members filed under an expression key naming a position of
    /// `remap`'s file that the edit moved or destroyed.
    fn members_with_moved_expr_keys(&self, remap: &crate::FileRemap) -> Vec<LuaMemberId> {
        let mut members = self
            .expr_key_members_by_file
            .get(&remap.file_id)
            .into_iter()
            .flatten()
            .copied()
            .filter(
                |member_id| match self.members.get(member_id).map(LuaMember::get_key) {
                    Some(LuaMemberKey::ExprType(typ)) => {
                        let result = crate::db_index::remap_identities_in_type(typ, remap);
                        result.lost || result.typ.is_some()
                    }
                    _ => false,
                },
            )
            .collect::<Vec<_>>();
        members.sort_unstable_by_key(|member_id| member_id_sort_key(*member_id));
        members
    }

    /// Re-keys the definition sites an edit moved.
    fn remap_definition_sites(
        &mut self,
        map: &rustc_hash::FxHashMap<crate::InFiled<TextRange>, crate::InFiled<TextRange>>,
    ) {
        let mut moves: Vec<(crate::InFiled<TextRange>, GlobalId)> = map
            .keys()
            .filter_map(|old| {
                let path = self.definition_site_owner.get(old)?.get_path()?.clone();
                Some((old.clone(), path))
            })
            .collect();
        moves.sort_unstable_by_key(|(old, _)| site_sort_key(old));
        for (old, _) in &moves {
            self.forget_definition_site(old);
        }
        for (old, path) in moves {
            self.set_definition_site(path, map[&old].clone());
        }
    }

    /// Drops the owners for table literals an edit removed, and every member
    /// filed under them.
    ///
    /// Those members can belong to files the edit does not re-index, so
    /// nothing else will clean them up.
    pub fn remove_deleted_element_owners(
        &mut self,
        deleted: &[crate::InFiled<TextRange>],
    ) -> Vec<LuaMemberId> {
        let mut forgotten = Vec::new();
        for range in deleted {
            // A file removal hands its literals here wholesale; a definition
            // site among them leaves the store with the file, not with the
            // path's members.
            self.forget_definition_site(range);
            let owner = LuaMemberOwner::Element(range.clone());
            let mut held = Vec::new();
            if let Some(member_items) = self.remove_owner_entry(&owner) {
                let member_ids: Vec<LuaMemberId> = member_items
                    .get_member_items()
                    .flat_map(|item| item.member_ids_slice().iter().copied())
                    .collect();
                for id in member_ids {
                    self.forget_member(id);
                    forgotten.push(id);
                    held.push(id);
                }
            }
            let visible = self.member_owner_key_index.remove(&owner);
            let history = self.member_owner_key_history_index.remove(&owner);
            if let Some(current) = self.current_owner_member_history.remove(&owner) {
                held.extend(current.iter().copied());
            }
            if derivation::recording() {
                for key in visible.iter().chain(history.iter()).flat_map(HashMap::keys) {
                    note_slot_write(&owner, key);
                }
            }
            note_owner_map_write(&owner);
            self.owner_history_fingerprints.remove(&owner);
            self.remove_owner_from_files(&owner, held);
        }
        // Members forgotten with their literal may still be listed under
        // other owners, which are registered under the members' files.
        let gone = forgotten.iter().copied().collect::<HashSet<_>>();
        let mut files = forgotten.iter().map(|id| id.file_id).collect::<Vec<_>>();
        files.sort_unstable();
        files.dedup();
        for file_id in files {
            let owners = self
                .in_filed
                .get(&file_id)
                .map(|owners| owners.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            self.unlist_from_owners(owners, |id| gone.contains(&id));
        }
        forgotten
    }
}

impl LuaMemberIndex {}

impl LuaIndex for LuaMemberIndex {
    fn remove(&mut self, file_id: FileId) {
        self.remove_files(&[file_id]);
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        for &file_id in file_ids {
            self.remove_file_owned_entries(file_id);
        }
        let removed: HashSet<FileId> = file_ids.iter().copied().collect();
        self.remove_files_members_from_owner_key_indexes(&removed);
        for &file_id in file_ids {
            let scoped = self
                .member_function_scope_ranges
                .file_entries(file_id)
                .map(|(member_id, _)| *member_id)
                .collect::<Vec<_>>();
            for member_id in scoped {
                derivation::note_write(|| FactKey::Member(member_id));
                self.member_function_scope_ranges.remove(&member_id);
            }
        }
    }

    fn clear(&mut self) {
        if derivation::recording() {
            self.note_every_fact_written();
        }
        self.owner_history_fingerprints.clear();
        self.key_fingerprints.clear();
        self.file_fingerprints.clear();
        self.members.clear();
        self.source_members.clear();
        self.in_filed.clear();
        self.owner_members.clear();
        self.member_current_owner.clear();
        self.member_owner_key_index.clear();
        self.member_owner_key_history_index.clear();
        self.owner_key_index_owners_by_file.clear();
        self.current_owner_member_history.clear();
        self.current_members_by_key.clear();
        self.expr_key_members_by_file.clear();
        self.non_overwriting_assignment_members.clear();
        self.preserved_co_writer_reconciled.clear();
        self.owner_only_members.clear();
        self.member_transfers.clear();
        self.filed_homes.clear();
        self.deferred_index_expr_members.clear();
        self.function_scope_ranges.clear();
        self.member_function_scope_ranges.clear();
        self.definition_site_owner.clear();
        self.path_definition_sites.clear();
        self.definition_sites_by_file.clear();
        self.global_path_class_owner.clear();
        self.global_path_class_candidates.clear();
        self.path_class_holders.clear();
        self.derived_path_classes.clear();
        self.member_path_provenance.clear();
    }
}

fn member_ids_from_item(item: &LuaMemberIndexItem) -> Vec<LuaMemberId> {
    match item {
        LuaMemberIndexItem::One(id) => vec![*id],
        LuaMemberIndexItem::Many(ids) => ids.clone(),
    }
}

crate::db_index::cow_shards::task_fork_fields!(LuaMemberIndex {
    members,
    source_members,
    in_filed => crate::db_index::cow_shards::merge_set,
    owner_members => LuaOwnerMembers::merge,
    member_current_owner,
    member_owner_key_index => |current, frozen, fork| {
        crate::db_index::cow_shards::merge_map(current, frozen, fork, merge_member_ids)
    },
    member_owner_key_history_index => |current, frozen, fork| {
        crate::db_index::cow_shards::merge_map(current, frozen, fork, merge_member_ids)
    },
    owner_key_index_owners_by_file => crate::db_index::cow_shards::merge_set,
    current_owner_member_history => merge_member_ids,
    current_members_by_key => merge_member_ids,
    expr_key_members_by_file => crate::db_index::cow_shards::merge_set,
    non_overwriting_assignment_members,
    preserved_co_writer_reconciled,
    owner_only_members,
    member_transfers,
    filed_homes,
    deferred_index_expr_members => crate::db_index::cow_shards::merge_set,
    function_scope_ranges,
    member_function_scope_ranges,
    definition_site_owner,
    path_definition_sites,
    definition_sites_by_file,
    global_path_class_owner,
    global_path_class_candidates,
    path_class_holders,
    derived_path_classes,
    member_path_provenance,
    owner_history_fingerprints => crate::db_index::cow_shards::rebuilt,
    key_fingerprints => crate::db_index::cow_shards::rebuilt,
    file_fingerprints => crate::db_index::cow_shards::rebuilt,
} prepare fingerprints_to_rebuild then rebuild_fingerprints
  by_field install_fingerprints => rebuild_fingerprints);
