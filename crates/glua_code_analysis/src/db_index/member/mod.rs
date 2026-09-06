mod lua_member;
mod lua_member_feature;
mod lua_member_item;
mod lua_member_owner;
mod lua_owner_members;
mod test;

use rowan::{TextRange, TextSize};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use super::traits::LuaIndex;
use crate::{FileId, GlobalId, db_index::member::lua_owner_members::LuaOwnerMembers};

pub use lua_member::{LuaMember, LuaMemberId, LuaMemberKey};
pub use lua_member_feature::LuaMemberFeature;
pub use lua_member_item::LuaMemberIndexItem;
pub use lua_member_owner::LuaMemberOwner;

/// Members filed by owner and then by key.
///
/// Two of these are kept. The visible one answers lookups; the history one also
/// keeps members a later write displaced, so a removal can still find where they
/// were filed.
type OwnerKeyMap = HashMap<LuaMemberOwner, HashMap<LuaMemberKey, Vec<LuaMemberId>>>;

/// Appends `id` to its slot unless the slot already holds it.
fn push_owner_key_id_unique(
    target: &mut OwnerKeyMap,
    owner: LuaMemberOwner,
    key: LuaMemberKey,
    id: LuaMemberId,
) {
    let member_ids = target.entry(owner).or_default().entry(key).or_default();
    if !member_ids.contains(&id) {
        member_ids.push(id);
    }
}

/// Appends `id` to its slot, which the caller knows does not hold it yet.
fn push_owner_key_id(
    target: &mut OwnerKeyMap,
    owner: LuaMemberOwner,
    key: LuaMemberKey,
    id: LuaMemberId,
) {
    target
        .entry(owner)
        .or_default()
        .entry(key)
        .or_default()
        .push(id);
}

/// Drops `id` from its slot, and the slot and owner once they are empty.
fn remove_owner_key_id(
    target: &mut OwnerKeyMap,
    owner: &LuaMemberOwner,
    key: &LuaMemberKey,
    id: LuaMemberId,
) {
    let Some(owner_items) = target.get_mut(owner) else {
        return;
    };
    if let Some(member_ids) = owner_items.get_mut(key) {
        member_ids.retain(|member_id| *member_id != id);
        if member_ids.is_empty() {
            owner_items.remove(key);
        }
    }
    if owner_items.is_empty() {
        target.remove(owner);
    }
}

#[derive(Debug)]
pub struct LuaMemberIndex {
    members: HashMap<LuaMemberId, LuaMember>,
    in_filed: HashMap<FileId, HashSet<MemberOrOwner>>,
    owner_members: HashMap<LuaMemberOwner, LuaOwnerMembers>,
    member_current_owner: HashMap<LuaMemberId, LuaMemberOwner>,
    member_owner_key_index: OwnerKeyMap,
    member_owner_key_history_index: OwnerKeyMap,
    /// The owners under which a file's members were filed in the two
    /// owner-key indexes. A file sweep only has to visit these owners, rather
    /// than every owner in the workspace. Entries may outlive the members that
    /// caused them; a sweep of an owner that holds none is a no-op.
    owner_key_index_owners_by_file: HashMap<FileId, HashSet<LuaMemberOwner>>,
    current_owner_member_history:
        HashMap<LuaMemberOwner, BTreeMap<(u32, u32, u32, u16), LuaMemberId>>,
    current_members_by_key: HashMap<LuaMemberKey, BTreeMap<(u32, u32, u32, u16), LuaMemberId>>,
    non_overwriting_assignment_members: HashSet<LuaMemberId>,
    /// `(owner, key)` slots whose last co-writer readmission merged nothing.
    ///
    /// See [`Self::readmit_preserved_assignment_co_writers`]: every later
    /// scan of such a slot is a proven no-op, so marks skip the walk. Only
    /// `clear` drops entries, with the index they describe — every other
    /// mutation preserves the coverage they record, and any future history
    /// write without a paired item merge must clear its slot here.
    preserved_co_writer_reconciled: HashSet<(LuaMemberOwner, LuaMemberKey)>,
    /// Monotonic homing generation for per-`(owner, key)` memos.
    ///
    /// The history index retains ids a later owner move left behind, so a raw
    /// length plus a still-homed decider cannot prove the filtered set is
    /// unchanged: moving a non-deciding writer away, or moving a historical
    /// writer back, leaves both checks green while the truth changed. Every
    /// mutation that can move an id between canonical slots, change what a
    /// canonical slot names, or otherwise edit history membership bumps this
    /// counter; pure appends (`add_member` first filings,
    /// `add_member_to_owner`, `add_member_alias_to_owner`) do not. Memo
    /// holders record the revision they walked and suffix-fold only on a
    /// match, rebuilding from scratch otherwise.
    homing_revision: u64,
    /// Members whose owner was recorded without adding them to the owner's
    /// member list: a write through an instance of a class, not the class.
    /// See [`Self::set_member_owner_only`].
    owner_only_members: HashSet<LuaMemberId>,
    /// Members whose owner was decided by scripted-class synthesis rather
    /// than by name resolution.
    synthesized_owner_members: HashSet<LuaMemberId>,
    /// Members `try_resolve_member` had to create itself, from a prefix
    /// type read mid-fixpoint.
    deferred_index_expr_members: HashSet<LuaMemberId>,
    function_scope_ranges: HashMap<FileId, Vec<TextRange>>,
    member_function_scope_ranges: HashMap<LuaMemberId, TextRange>,
    /// The owner a table literal that initialises a global path resolves to:
    /// the `{}` of `X = {}` or of the GLua-idiomatic `X = X or {}`, at any
    /// depth. Held as the owner rather than the path so the normaliser can
    /// hand back a reference. See [`LuaMemberIndex::canonical_owner`].
    definition_site_owner: HashMap<crate::InFiled<TextRange>, LuaMemberOwner>,
    /// The reverse of [`Self::definition_site_owner`], each path's sites sorted
    /// by `(file id, range)`.
    path_definition_sites: HashMap<GlobalId, Vec<crate::InFiled<TextRange>>>,
    definition_sites_by_file: HashMap<FileId, Vec<crate::InFiled<TextRange>>>,
    /// The class a global path's declaration carries (`---@class oslib` on
    /// `os = {}`). Such a path's members belong to the class, which is where
    /// every reader of the annotated type looks for them.
    global_path_class_owner: HashMap<GlobalId, LuaMemberOwner>,
    /// Every `---@class` proposal made for a global path's declaration, by
    /// path. The winner is the set minimum, so two files annotating one
    /// global settle on the same class regardless of analysis order, and a
    /// removed file's proposal can be dropped without losing the others.
    global_path_class_candidates: HashMap<GlobalId, BTreeSet<GlobalPathClassCandidate>>,
    /// The global path a member's homing went through: its write named the
    /// path directly, or a table literal that initialises one. Such members
    /// follow the path when its winning `---@class` changes; members homed
    /// onto a class by any other route stay where they are.
    member_path_provenance: HashMap<LuaMemberId, GlobalId>,
    /// Member reads that found nothing, by the file that read. A file whose
    /// read failed cached no type and so left no dependency edge; when another
    /// file later defines that key, this is how the reader is found.
    missed_member_reads_by_file: HashMap<FileId, Vec<(LuaMemberOwner, LuaMemberKey)>>,
    missed_member_readers: HashMap<(LuaMemberOwner, LuaMemberKey), HashSet<FileId>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum MemberOrOwner {
    Member(LuaMemberId),
    Owner(LuaMemberOwner),
}

/// One file's `---@class` proposal for a global path's declaration.
///
/// Ordered by class name first — the winner must be name-lexicographic-min so
/// two files annotating one global do not depend on analysis order — then by
/// file id and class identity, which breaks ties between same-named classes
/// deterministically. Deriving [`Eq`] over all three fields makes re-proposing
/// the same class from the same file a no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GlobalPathClassCandidate {
    class_name: Box<str>,
    file_id: FileId,
    class: crate::LuaTypeDeclId,
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
            members: HashMap::default(),
            in_filed: HashMap::default(),
            owner_members: HashMap::default(),
            member_current_owner: HashMap::default(),
            member_owner_key_index: HashMap::default(),
            member_owner_key_history_index: HashMap::default(),
            owner_key_index_owners_by_file: HashMap::default(),
            current_owner_member_history: HashMap::default(),
            current_members_by_key: HashMap::default(),
            non_overwriting_assignment_members: HashSet::default(),
            preserved_co_writer_reconciled: HashSet::default(),
            homing_revision: 0,
            owner_only_members: HashSet::default(),
            synthesized_owner_members: HashSet::default(),
            deferred_index_expr_members: HashSet::default(),
            function_scope_ranges: HashMap::default(),
            member_function_scope_ranges: HashMap::default(),
            definition_site_owner: HashMap::default(),
            path_definition_sites: HashMap::default(),
            definition_sites_by_file: HashMap::default(),
            global_path_class_owner: HashMap::default(),
            global_path_class_candidates: HashMap::default(),
            member_path_provenance: HashMap::default(),
            missed_member_reads_by_file: HashMap::default(),
            missed_member_readers: HashMap::default(),
        }
    }

    /// The current homing generation. Per-`(owner, key)` memos record this
    /// alongside what they walked; a mismatch proves an owner, key, or
    /// canonical mapping moved under them, so only a full rebuild answers.
    pub fn homing_revision(&self) -> u64 {
        self.homing_revision
    }

    fn bump_homing_revision(&mut self) {
        self.homing_revision = self.homing_revision.wrapping_add(1);
    }

    pub fn add_member(&mut self, owner: LuaMemberOwner, member: LuaMember) -> LuaMemberId {
        let id = member.get_id();
        let owner = self
            .effective_path_member_owner(id, &owner)
            .unwrap_or(owner);
        self.note_member_path_provenance(id, &owner);
        let owner = self.canonical(&owner).clone();
        let file_id = member.get_file_id();
        let function_scope = self.assignment_file_define_scope_for_member(&member);
        // Re-filing an existing id re-homes whatever the old filing named:
        // the member keeps its identity while its owner and key maps move, so
        // any memo holding the old slot must rebuild. Pure first filings are
        // append-only and leave the revision alone.
        if self.members.contains_key(&id) {
            self.bump_homing_revision();
        }
        self.members.insert(id, member);
        self.set_member_function_scope_range(id, function_scope);
        self.add_in_file_object(file_id, MemberOrOwner::Member(id));
        if !owner.is_unknown() {
            self.member_current_owner.insert(id, owner.clone());
            self.add_current_member_key(id);
            self.current_owner_member_history
                .entry(owner.clone())
                .or_default()
                .insert(member_id_sort_key(id), id);
            self.add_in_file_object(file_id, MemberOrOwner::Owner(owner.clone()));
            self.add_new_member_to_owner_key_indexes(owner.clone(), id);
            self.add_member_to_owner(owner.clone(), id);
        }
        id
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
        let old_key = self.get_member(&member_id)?.get_key().clone();
        if old_key == new_key {
            return Some(());
        }

        // The key names the canonical slot, so the old slot's history keeps a
        // left-behind entry at the same raw length while the filtered set
        // changed: memos must rebuild rather than suffix-fold.
        self.bump_homing_revision();
        let owner = self.member_current_owner.get(&member_id).cloned();
        if let Some(owner) = &owner {
            self.remove_member_from_all_owner_key_indexes(owner, member_id);
            self.remove_current_member_key(member_id);
            if let Some(owner_members) = self.owner_members.get_mut(owner) {
                let slot_empty = match owner_members.get_member_mut(&old_key) {
                    Some(LuaMemberIndexItem::One(id)) => *id == member_id,
                    Some(LuaMemberIndexItem::Many(ids)) => {
                        ids.retain(|id| *id != member_id);
                        ids.is_empty()
                    }
                    None => false,
                };
                if slot_empty {
                    owner_members.remove_member(&old_key);
                }
            }
        }

        self.members.get_mut(&member_id)?.set_key(new_key.clone());

        if let Some(owner) = owner {
            self.add_current_member_key(member_id);
            self.add_member_to_owner_key_indexes(owner.clone(), member_id);
            self.add_member_to_owner(owner, member_id);
        }
        Some(())
    }

    fn add_in_file_object(&mut self, file_id: FileId, member_or_owner: MemberOrOwner) {
        self.in_filed
            .entry(file_id)
            .or_default()
            .insert(member_or_owner);
    }

    /// Files `id` under `owner`. A slot holds every member written or declared
    /// for its key, ordered by [`member_id_sort_key`]; readers merge them.
    pub fn add_member_to_owner(&mut self, owner: LuaMemberOwner, id: LuaMemberId) -> Option<()> {
        let owner = self
            .effective_path_member_owner(id, &owner)
            .unwrap_or(owner);
        let owner = self.canonical(&owner).clone();
        let key = self.get_member(&id)?.get_key().clone();
        if self.member_current_owner.get(&id) != Some(&owner) {
            self.add_member_to_owner_key_indexes(owner.clone(), id);
        }
        self.owner_only_members.remove(&id);
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
        self.set_member_owner(owner, file_id, id)?;
        self.owner_only_members.insert(id);
        Some(())
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
        self.member_path_provenance.insert(id, alias_path);
        self.record_member_owner(owner, file_id, id)?;
        if owner_only {
            self.owner_only_members.insert(id);
            return Some(());
        }
        let key = self.get_member(&id)?.get_key().clone();
        self.owner_only_members.remove(&id);
        let owner = self.member_current_owner.get(&id)?.clone();
        self.merge_member_into_owner_item(owner, key, id);
        Some(())
    }

    /// Records that `id`'s owner was decided by scripted-class synthesis, so
    /// [`Self::set_global_path_class`] leaves it where it is instead of moving
    /// it onto the annotated class.
    pub fn pin_synthesized_owner(&mut self, id: LuaMemberId) {
        self.synthesized_owner_members.insert(id);
    }

    pub fn has_synthesized_owner(&self, id: &LuaMemberId) -> bool {
        self.synthesized_owner_members.contains(id)
    }

    /// Records that `id` was created by deferred resolution, so its first home
    /// was decided from a mid-fixpoint prefix type. See
    /// [`Self::deferred_index_expr_members`].
    pub fn mark_deferred_index_expr_member(&mut self, id: LuaMemberId) {
        self.deferred_index_expr_members.insert(id);
    }

    pub fn is_deferred_index_expr_member(&self, id: &LuaMemberId) -> bool {
        self.deferred_index_expr_members.contains(id)
    }

    /// Removes `id` from `owner` entirely, including the item
    /// `set_member_owner` leaves behind.
    pub fn detach_member_from_owner(&mut self, owner: &LuaMemberOwner, id: LuaMemberId) {
        let owner = &self.canonical(owner).clone();
        let Some(key) = self.get_member(&id).map(|member| member.get_key().clone()) else {
            return;
        };
        self.bump_homing_revision();
        self.drop_member_path_provenance_on_detach(owner, id);
        self.remove_member_from_all_owner_key_indexes(owner, id);
        self.remove_current_owner_member(owner, id);

        let Some(owner_members) = self.owner_members.get_mut(owner) else {
            return;
        };
        let drop_key = match owner_members.get_member_mut(&key) {
            Some(LuaMemberIndexItem::One(existing)) => *existing == id,
            Some(LuaMemberIndexItem::Many(ids)) => {
                ids.retain(|member_id| *member_id != id);
                ids.is_empty()
            }
            None => false,
        };
        if drop_key {
            owner_members.remove_member(&key);
        }
        if owner_members.is_empty() {
            self.owner_members.remove(owner);
        }
    }

    /// Makes `id` *also* reachable through `owner`, without ever displacing
    /// what is already there.
    pub fn add_member_alias_to_owner(
        &mut self,
        owner: LuaMemberOwner,
        id: LuaMemberId,
    ) -> Option<()> {
        let owner = self.canonical(&owner).clone();
        let member = self.get_member(&id)?;
        let file_id = member.get_file_id();
        let key = member.get_key().clone();

        if self.member_current_owner.get(&id) != Some(&owner) {
            self.add_member_to_owner_key_indexes(owner.clone(), id);
        }

        self.merge_member_into_owner_item(owner.clone(), key, id);
        self.add_in_file_object(file_id, MemberOrOwner::Owner(owner));
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
        let owner_members = self
            .owner_members
            .entry(owner)
            .or_insert_with(LuaOwnerMembers::new);
        let Some(item) = owner_members.get_member_mut(&key) else {
            owner_members.add_member(key, LuaMemberIndexItem::One(id));
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
        push_owner_key_id_unique(
            &mut self.member_owner_key_index,
            owner.clone(),
            key.clone(),
            id,
        );
        push_owner_key_id_unique(&mut self.member_owner_key_history_index, owner, key, id);
    }

    /// [`add_member_to_owner_key_indexes`](Self::add_member_to_owner_key_indexes)
    /// for a member the caller knows neither index holds yet.
    fn add_new_member_to_owner_key_indexes(&mut self, owner: LuaMemberOwner, id: LuaMemberId) {
        let Some(key) = self.get_member(&id).map(|member| member.get_key().clone()) else {
            return;
        };

        self.record_owner_key_index_owner(id.file_id, &owner);
        push_owner_key_id(
            &mut self.member_owner_key_index,
            owner.clone(),
            key.clone(),
            id,
        );
        push_owner_key_id(&mut self.member_owner_key_history_index, owner, key, id);
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
        remove_owner_key_id(&mut self.member_owner_key_index, owner, &key, id);
    }

    fn remove_member_from_all_owner_key_indexes(
        &mut self,
        owner: &LuaMemberOwner,
        id: LuaMemberId,
    ) {
        let Some(key) = self.get_member(&id).map(|member| member.get_key().clone()) else {
            return;
        };
        remove_owner_key_id(&mut self.member_owner_key_index, owner, &key, id);
        remove_owner_key_id(&mut self.member_owner_key_history_index, owner, &key, id);
    }

    fn remove_files_members_from_owner_key_indexes(&mut self, removed: &HashSet<FileId>) {
        let owners = removed
            .iter()
            .filter_map(|file_id| self.owner_key_index_owners_by_file.remove(file_id))
            .flatten()
            .collect::<HashSet<_>>();
        let visible_removed = Self::remove_files_members_from_owner_key_map(
            &mut self.member_owner_key_index,
            removed,
            &owners,
        );
        let history_removed = Self::remove_files_members_from_owner_key_map(
            &mut self.member_owner_key_history_index,
            removed,
            &owners,
        );
        // The sweep edits history membership behind every memo's back: a slot
        // that lost an entry shrinks, but one that lost nothing keeps its raw
        // length, so only bump when an entry actually left.
        if visible_removed || history_removed {
            self.bump_homing_revision();
        }
    }

    fn remove_files_members_from_owner_key_map(
        owner_key_index: &mut HashMap<LuaMemberOwner, HashMap<LuaMemberKey, Vec<LuaMemberId>>>,
        removed: &HashSet<FileId>,
        owners: &HashSet<LuaMemberOwner>,
    ) -> bool {
        let mut removed_any = false;
        for owner in owners {
            let Some(key_members) = owner_key_index.get_mut(owner) else {
                continue;
            };
            key_members.retain(|_, member_ids| {
                let before = member_ids.len();
                member_ids.retain(|member_id| !removed.contains(&member_id.file_id));
                if member_ids.len() != before {
                    removed_any = true;
                }
                !member_ids.is_empty()
            });
            if key_members.is_empty() {
                owner_key_index.remove(owner);
            }
        }
        removed_any
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
    /// Homing invariant: a real owner move leaves the old slot's history
    /// holding a left-behind entry, so its raw length is unchanged while its
    /// filtered set shrank; a deferred first homing (no previous owner, but
    /// the id is already indexed) grows a slot whose history already listed
    /// the id without changing its raw length either. Both cases bump
    /// [`Self::homing_revision`] so per-`(owner, key)` memos rebuild instead
    /// of trusting a length check. Re-homing onto the same owner, or homing
    /// a brand-new id for the first time, is append-only and leaves the
    /// revision alone.
    pub fn set_member_owner(
        &mut self,
        owner: LuaMemberOwner,
        file_id: FileId,
        id: LuaMemberId,
    ) -> Option<()> {
        let owner = self
            .effective_path_member_owner(id, &owner)
            .unwrap_or(owner);
        self.note_member_path_provenance(id, &owner);
        self.record_member_owner(owner, file_id, id)
    }

    /// The filing half of [`Self::set_member_owner`], without the provenance
    /// note. Callers that record provenance themselves route through here so
    /// the note's drop branch cannot undo what they just recorded.
    fn record_member_owner(
        &mut self,
        owner: LuaMemberOwner,
        file_id: FileId,
        id: LuaMemberId,
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
        let previous_owner = self.member_current_owner.insert(id, owner.clone());
        match &previous_owner {
            None if already_indexed => self.bump_homing_revision(),
            Some(previous) if previous != &owner => self.bump_homing_revision(),
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

        self.current_owner_member_history
            .entry(owner.clone())
            .or_default()
            .insert(member_id_sort_key(id), id);

        self.add_member_to_owner_key_indexes(owner.clone(), id);
        if self.member_function_scope_range(id).is_none()
            && let Some(member) = self.get_member(&id)
        {
            let function_scope = self.assignment_file_define_scope_for_member(member);
            self.set_member_function_scope_range(id, function_scope);
        }
        self.add_in_file_object(file_id, MemberOrOwner::Owner(owner));

        Some(())
    }

    pub fn get_member(&self, id: &LuaMemberId) -> Option<&LuaMember> {
        self.members.get(id)
    }

    pub fn get_member_mut(&mut self, id: &LuaMemberId) -> Option<&mut LuaMember> {
        // Defensive: the key lives on the member, so any mutable access could
        // re-home it behind every memo's back. No caller does so today except
        // through `rekey_member` (which bumps itself), but a future direct
        // `set_key` would otherwise read stale suffix-folded answers.
        if self.members.contains_key(id) {
            self.bump_homing_revision();
        }
        self.members.get_mut(id)
    }

    /// Every global path that currently has members parked on it, in a
    /// stable order.
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
        // Remapping what a canonical slot names re-homes every member filed
        // under the affected literal: memos keyed by the old canonical slot
        // must rebuild. `forget_definition_site` bumps for the unmap; bump
        // again for the remap itself (a fresh site has no unmap to bump on).
        self.forget_definition_site(&site);
        self.bump_homing_revision();
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
        self.definition_site_owner.get(site)?.get_path().cloned()
    }

    /// Whether `path` is a global path the index knows about: it has members
    /// filed under it, a registered definition site, or a `---@class`
    /// proposal. Read-only and cheap, so producers may consult it per write.
    pub fn is_known_global_path(&self, path: &GlobalId) -> bool {
        self.owner_members
            .contains_key(&LuaMemberOwner::GlobalPath(path.clone()))
            || self.global_path_class_candidates.contains_key(path)
            || self.path_definition_sites.contains_key(path)
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
        for member_id in member_ids {
            self.member_path_provenance
                .entry(member_id)
                .or_insert_with(|| path.clone());
        }
    }

    /// The literals initialising `path`, sorted by `(file id, range)`.
    pub fn definition_sites(&self, path: &GlobalId) -> &[crate::InFiled<TextRange>] {
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
                self.definition_site_owner.get(range).unwrap_or(owner)
            }
            _ => owner,
        };
        match owner {
            LuaMemberOwner::GlobalPath(path) => {
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
    fn note_member_path_provenance(&mut self, id: LuaMemberId, raw_owner: &LuaMemberOwner) {
        if let Some(path) = raw_owner.get_path() {
            self.member_path_provenance.insert(id, path.clone());
            return;
        }
        let canonical = self.canonical(raw_owner).clone();
        if let Some(path) = canonical.get_path() {
            self.member_path_provenance.insert(id, path.clone());
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
        }
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
        let provenance = self.member_path_provenance.get(&id)?;
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
        let winner = {
            let candidates = self
                .global_path_class_candidates
                .entry(path.clone())
                .or_default();
            candidates.insert(GlobalPathClassCandidate {
                class_name: class.get_name().into(),
                file_id,
                class: class.clone(),
            });
            candidates
                .iter()
                .next()
                .map(|candidate| candidate.class.clone())
        };
        let Some(winner) = winner else {
            return;
        };
        let old = self
            .global_path_class_owner
            .get(&path)
            .and_then(LuaMemberOwner::get_type_id)
            .cloned();
        if old.as_ref() == Some(&winner) {
            return;
        }
        self.global_path_class_owner
            .insert(path.clone(), LuaMemberOwner::Type(winner.clone()));
        // The canonical mapping itself changed what the path slot names, on
        // top of the per-member moves in `rehome_global_path_class` (each
        // bumps on its own).
        self.bump_homing_revision();
        self.rehome_global_path_class(&path, old.as_ref(), &LuaMemberOwner::Type(winner));
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
                !self.synthesized_owner_members.contains(id)
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
                        !self.synthesized_owner_members.contains(id)
                            && self.member_current_owner.get(id) == Some(&old_owner)
                            && self.member_path_provenance.get(id) == Some(path)
                    }),
            );
        }

        member_ids.sort_unstable_by_key(|id| member_id_sort_key(*id));
        member_ids.dedup();
        for member_id in member_ids {
            self.remove_member_from_owner_item(&path_owner, member_id);
            if let Some(old) = from {
                self.remove_member_from_owner_item(&LuaMemberOwner::Type(old.clone()), member_id);
            }
            self.set_member_owner(to.clone(), member_id.file_id, member_id);
            self.add_member_to_owner(to.clone(), member_id);
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
        let drop_key = match owner_members.get_member_mut(&key) {
            Some(LuaMemberIndexItem::One(existing)) => *existing == id,
            Some(LuaMemberIndexItem::Many(ids)) => {
                ids.retain(|held| *held != id);
                ids.is_empty()
            }
            None => false,
        };
        if drop_key {
            owner_members.remove_member(&key);
        }
        if owner_members.is_empty() {
            self.owner_members.remove(owner);
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
        // Unmapping what a canonical slot names re-homes the literal's
        // members back onto the literal itself.
        self.bump_homing_revision();
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
        let mut ranges: Vec<crate::InFiled<TextRange>> = self
            .owner_members
            .keys()
            .filter_map(|owner| match owner {
                LuaMemberOwner::Element(range) if range.file_id == file_id => Some(range.clone()),
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

    /// The owner's members whose key is an expression rather than a name, in
    /// the same order [`Self::get_members`] would yield them.
    pub fn get_expr_key_members(&self, owner: &LuaMemberOwner) -> Option<Vec<&LuaMember>> {
        let owner = self.canonical(owner);
        let owner_members = self.owner_members.get(owner)?;
        let mut member_ids = Vec::new();
        for key in owner_members.expr_keys() {
            match owner_members.get_member(key) {
                Some(LuaMemberIndexItem::One(id)) => member_ids.push(*id),
                Some(LuaMemberIndexItem::Many(ids)) => member_ids.extend(ids.iter().copied()),
                None => {}
            }
        }
        member_ids.sort_by_key(|member_id| member_id_sort_key(*member_id));
        Some(
            member_ids
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
        let owner_members = self.owner_members.get(owner)?;
        let mut member_ids = match owner_members.get_member(key) {
            Some(LuaMemberIndexItem::One(id)) => vec![*id],
            Some(LuaMemberIndexItem::Many(ids)) => ids.clone(),
            None => return Some(Vec::new()),
        };
        member_ids.sort_by_key(|member_id| member_id_sort_key(*member_id));
        Some(
            member_ids
                .iter()
                .filter_map(|member_id| self.get_member(member_id))
                .collect(),
        )
    }

    pub fn get_member_keys<'a>(
        &'a self,
        owner: &LuaMemberOwner,
    ) -> impl Iterator<Item = &'a LuaMemberKey> + 'a {
        self.owner_members
            .get(self.canonical(owner))
            .into_iter()
            .flat_map(LuaOwnerMembers::get_member_keys)
    }

    pub fn get_member_item_by_member_id(
        &self,
        member_id: LuaMemberId,
    ) -> Option<&LuaMemberIndexItem> {
        let owner = self.member_current_owner.get(&member_id)?;
        let member_key = self.members.get(&member_id)?.get_key();
        let member_items = self.owner_members.get(owner)?;
        let item = member_items.get_member(member_key)?;
        Some(item)
    }

    pub fn get_sorted_members(&self, owner: &LuaMemberOwner) -> Option<Vec<&LuaMember>> {
        self.get_members(owner)
    }

    pub fn get_member_item(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
    ) -> Option<&LuaMemberIndexItem> {
        let owner = self.canonical(owner);
        self.owner_members
            .get(owner)
            .and_then(|map| map.get_member(key))
    }

    pub fn get_member_len(&self, owner: &LuaMemberOwner) -> usize {
        let owner = self.canonical(owner);
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
        let Some(owner_members) = self.owner_members.get(owner) else {
            return false;
        };
        owner_members.get_member_items().any(|item| match item {
            LuaMemberIndexItem::One(id) => self.get_member(id).is_some(),
            LuaMemberIndexItem::Many(ids) => ids.iter().any(|id| self.get_member(id).is_some()),
        })
    }

    pub fn get_current_owner(&self, id: &LuaMemberId) -> Option<&LuaMemberOwner> {
        self.member_current_owner.get(id)
    }

    pub fn get_member_owner(&self, id: &LuaMemberId) -> Option<&LuaMemberOwner> {
        self.member_current_owner.get(id)
    }

    pub fn visible_member_count_for_owner_key(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
    ) -> usize {
        let owner = self.canonical(owner);
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

    /// Every member ever keyed under `owner`, including ones hidden by the
    /// latest-assignment view and ones since re-homed to a concrete owner.
    pub fn get_member_history(&self, owner: &LuaMemberOwner) -> Vec<&LuaMember> {
        let owner = self.canonical(owner);
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
            return Vec::new();
        };
        let name = global_id.get_name();
        // Not the last dotted segment: a bracketed string key keeps its dots, so
        // `T["a.b"] = v` stores one member keyed `a.b` under `T`.
        let key_text = match owner {
            LuaMemberOwner::GlobalPath(owner_id) => name
                .strip_prefix(owner_id.get_name())
                .and_then(|rest| rest.strip_prefix('.'))
                .unwrap_or(name),
            _ => name.rsplit_once('.').map_or(name, |(_, last)| last),
        };

        let mut matched = Vec::new();
        let collect = |key: &LuaMemberKey, matched: &mut Vec<LuaMemberId>| {
            let Some(member_ids) = owner_items.get(key) else {
                return;
            };
            matched.extend(member_ids.iter().copied().filter(|member_id| {
                self.get_member(member_id)
                    .and_then(|member| member.get_global_id())
                    == Some(global_id)
            }));
        };
        collect(&LuaMemberKey::Name(key_text.into()), &mut matched);
        // A numeric field is keyed by its integer, not by its spelling.
        if let Ok(index) = key_text.parse::<i64>() {
            collect(&LuaMemberKey::Integer(index), &mut matched);
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
        let ranges = self.function_scope_ranges.entry(file_id).or_default();
        match ranges.binary_search_by_key(&range.start(), |range| range.start()) {
            Ok(index) | Err(index) => ranges.insert(index, range),
        }
    }

    pub fn enclosing_function_scope_range(
        &self,
        file_id: FileId,
        position: TextSize,
    ) -> Option<TextRange> {
        let ranges = self.function_scope_ranges.get(&file_id)?;
        let mut index = ranges.partition_point(|range| range.start() <= position);
        while index > 0 {
            index -= 1;
            let range = ranges[index];
            if range.contains(position) {
                return Some(range);
            }
        }
        None
    }

    pub fn set_member_function_scope_range(
        &mut self,
        member_id: LuaMemberId,
        range: Option<TextRange>,
    ) {
        if let Some(range) = range {
            self.member_function_scope_ranges.insert(member_id, range);
        } else {
            self.member_function_scope_ranges.remove(&member_id);
        }
    }

    pub fn member_function_scope_range(&self, member_id: LuaMemberId) -> Option<TextRange> {
        self.member_function_scope_ranges.get(&member_id).copied()
    }

    pub fn mark_non_overwriting_assignment_member(&mut self, member_id: LuaMemberId) {
        self.non_overwriting_assignment_members.insert(member_id);
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

    /// History-slot ids still homed under `owner`/`key`, in insertion order.
    ///
    /// The unsorted form of
    /// [`get_current_owner_members_for_key`](Self::get_current_owner_members_for_key):
    /// per-write callers that only need membership or a minimum must not pay
    /// that sort on every write.
    pub fn get_current_owner_member_ids_for_key_unsorted(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
    ) -> Vec<LuaMemberId> {
        self.get_current_owner_member_ids_for_key_unsorted_from(owner, key, 0)
    }

    /// The tail of the history slot
    /// [`get_current_owner_member_ids_for_key_unsorted`](Self::get_current_owner_member_ids_for_key_unsorted)
    /// would return: same insertion order and same homed-under-`owner`/`key`
    /// filter, skipping the first `skip` raw history entries.
    ///
    /// Per-write callers that memoize the slot walk the new suffix only, so N
    /// appends cost O(N) total instead of O(N^2). The caller must key its memo
    /// off the canonical `(owner, key)` and the raw length from
    /// [`owner_key_history_len`](Self::owner_key_history_len): any owner or key
    /// mutation files the member under a different canonical slot (a miss), and
    /// removals and rekeys never run while a per-pass memo is alive.
    pub fn get_current_owner_member_ids_for_key_unsorted_from(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
        skip: usize,
    ) -> Vec<LuaMemberId> {
        let owner = self.canonical(owner);
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
            .skip(skip)
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

    /// Raw history-slot length for `owner`/`key`: O(1), no filtering or allocation.
    ///
    /// Counts every id ever filed under the canonical slot, including entries a
    /// later owner move left behind (those fail the homed filter above). A memo
    /// hit on this length plus the homed checks its caller performs means the
    /// filtered set is unchanged: within one analysis pass history slots only
    /// grow by append.
    pub fn owner_key_history_len(&self, owner: &LuaMemberOwner, key: &LuaMemberKey) -> usize {
        let owner = self.canonical(owner);
        self.member_owner_key_history_index
            .get(owner)
            .and_then(|owner_items| owner_items.get(key))
            .map(|member_ids| member_ids.len())
            .unwrap_or(0)
    }

    /// Returns every still-current member ever indexed under `owner`, including
    /// assignment history entries hidden by the normal latest-value view.
    ///
    /// This is intended for metadata synthesis that must migrate a bounded
    /// owner region wholesale. Ordinary semantic lookup should continue using
    /// `get_members`, which applies runtime overwrite visibility.
    pub fn get_current_owner_member_history(&self, owner: &LuaMemberOwner) -> Vec<&LuaMember> {
        let owner = self.canonical(owner);
        let Some(member_ids) = self.current_owner_member_history.get(owner) else {
            return Vec::new();
        };
        member_ids
            .values()
            .filter_map(|member_id| self.get_member(member_id))
            .collect()
    }

    fn remove_current_owner_member(&mut self, owner: &LuaMemberOwner, member_id: LuaMemberId) {
        let Some(members) = self.current_owner_member_history.get_mut(owner) else {
            return;
        };
        members.remove(&member_id_sort_key(member_id));
        if members.is_empty() {
            self.current_owner_member_history.remove(owner);
        }
    }

    pub fn get_current_members_for_key(&self, key: &LuaMemberKey) -> Vec<&LuaMember> {
        let Some(member_ids) = self.current_members_by_key.get(key) else {
            return Vec::new();
        };

        member_ids
            .values()
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
        self.current_members_by_key
            .entry(key)
            .or_default()
            .insert(member_id_sort_key(member_id), member_id);
    }

    fn remove_current_member_key(&mut self, member_id: LuaMemberId) {
        let Some(key) = self
            .get_member(&member_id)
            .map(|member| member.get_key().clone())
        else {
            return;
        };
        let Some(members) = self.current_members_by_key.get_mut(&key) else {
            return;
        };
        members.remove(&member_id_sort_key(member_id));
        if members.is_empty() {
            self.current_members_by_key.remove(&key);
        }
    }

    pub fn get_file_members(&self, file_id: FileId) -> Vec<&LuaMember> {
        let Some(member_or_owners) = self.in_filed.get(&file_id) else {
            return Vec::new();
        };

        member_or_owners
            .iter()
            .filter_map(|entry| match entry {
                MemberOrOwner::Member(member_id) => self.get_member(member_id),
                MemberOrOwner::Owner(_) => None,
            })
            .collect()
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
pub(crate) fn member_id_sort_key(member_id: LuaMemberId) -> (u32, u32, u32, u16) {
    let syntax_id = member_id.get_syntax_id();
    (
        member_id.file_id.id,
        u32::from(member_id.get_position()),
        u32::from(syntax_id.get_range().end()),
        syntax_id.get_kind() as u16,
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
        // Dropping the member edits the history membership its old slot's
        // memos walked: the raw slot keeps no tombstone for it, but a memo
        // holding the old length would otherwise trust a prefix that lost an
        // entry in the middle.
        if self.members.contains_key(&member_id) {
            self.bump_homing_revision();
        }
        if let Some(owner) = self.member_current_owner.get(&member_id).cloned() {
            self.remove_member_from_all_owner_key_indexes(&owner, member_id);
            self.remove_current_owner_member(&owner, member_id);
            self.remove_current_member_key(member_id);
        }
        self.members.remove(&member_id);
        self.member_current_owner.remove(&member_id);
        self.member_path_provenance.remove(&member_id);
        self.non_overwriting_assignment_members.remove(&member_id);
        self.owner_only_members.remove(&member_id);
        self.synthesized_owner_members.remove(&member_id);
        self.deferred_index_expr_members.remove(&member_id);
        self.member_function_scope_ranges.remove(&member_id);
        if let Some(set) = self.in_filed.get_mut(&member_id.file_id) {
            set.remove(&MemberOrOwner::Member(member_id));
            if set.is_empty() {
                self.in_filed.remove(&member_id.file_id);
            }
        }
    }

    /// The per-file half of removal: erase every entry keyed or owned by
    /// `file_id`. The whole-index owner-key sweeps are done once per batch in
    /// `remove_files`, not here — running them per file made removal cost
    /// O(files × index) and dominated incremental edits of high fan-in files.
    fn remove_file_owned_entries(&mut self, file_id: FileId) {
        if let Some(member_ids) = self.in_filed.remove(&file_id) {
            let mut owners = HashSet::default();
            for member_id_or_owner in member_ids {
                match member_id_or_owner {
                    MemberOrOwner::Member(member_id) => self.forget_member(member_id),
                    MemberOrOwner::Owner(owner) => {
                        owners.insert(owner);
                    }
                }
            }

            let mut need_removed_owner = Vec::new();
            for owner in owners {
                if let Some(member_items) = self.owner_members.get_mut(&owner) {
                    let mut need_removed_key = Vec::new();
                    for (key, item) in member_items.iter_mut() {
                        match item {
                            LuaMemberIndexItem::One(id) => {
                                if id.file_id == file_id {
                                    need_removed_key.push(key.clone());
                                }
                            }
                            LuaMemberIndexItem::Many(ids) => {
                                ids.retain(|id| id.file_id != file_id);
                                if ids.is_empty() {
                                    need_removed_key.push(key.clone());
                                }
                            }
                        }
                    }

                    for key in need_removed_key {
                        member_items.remove_member(&key);
                    }

                    if member_items.is_empty() {
                        need_removed_owner.push(owner);
                    }
                }
            }

            for owner in need_removed_owner {
                self.owner_members.remove(&owner);
            }
        }
        self.function_scope_ranges.remove(&file_id);
        self.remove_file_definition_sites(file_id);
        self.recompute_global_path_class_winners(file_id);
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
        let mut affected: Vec<GlobalId> = Vec::new();
        for (path, candidates) in self.global_path_class_candidates.iter_mut() {
            let before = candidates.len();
            candidates.retain(|candidate| candidate.file_id != file_id);
            if candidates.len() != before {
                affected.push(path.clone());
            }
        }
        if affected.is_empty() {
            return;
        }
        affected.sort_unstable_by(|left, right| left.get_name().cmp(right.get_name()));

        for path in affected {
            if self
                .global_path_class_candidates
                .get(&path)
                .is_some_and(BTreeSet::is_empty)
            {
                self.global_path_class_candidates.remove(&path);
            }
            let winner = self
                .global_path_class_candidates
                .get(&path)
                .and_then(|candidates| candidates.iter().next())
                .map(|candidate| candidate.class.clone());
            let old = self
                .global_path_class_owner
                .get(&path)
                .and_then(LuaMemberOwner::get_type_id)
                .cloned();
            match winner {
                Some(winner) => {
                    if old.as_ref() != Some(&winner) {
                        self.global_path_class_owner
                            .insert(path.clone(), LuaMemberOwner::Type(winner.clone()));
                        self.bump_homing_revision();
                        self.rehome_global_path_class(
                            &path,
                            old.as_ref(),
                            &LuaMemberOwner::Type(winner),
                        );
                    }
                }
                None => {
                    if let Some(old) = old {
                        self.global_path_class_owner.remove(&path);
                        self.bump_homing_revision();
                        self.rehome_global_path_class(
                            &path,
                            Some(&old),
                            &LuaMemberOwner::GlobalPath(path.clone()),
                        );
                    }
                }
            }
        }
    }

    /// Drops an owner entry from every file that registered it.
    ///
    /// `set_member_owner` files it under the contributing member's file, so a
    /// single Element owner can be registered by several files at once.
    fn remove_owner_from_all_files(&mut self, owner: &LuaMemberOwner) {
        let entry = MemberOrOwner::Owner(owner.clone());
        self.in_filed.retain(|_, set| {
            set.remove(&entry);
            !set.is_empty()
        });
    }

    /// Re-homes every table-literal owner in the edited file onto its new
    /// range, and purges the ones the edit destroyed.
    ///
    /// Returns the members forgotten with a destroyed owner - their cached
    /// types have to come off with them - and the files that had registered a
    /// member on one, which nothing else will rebuild.
    pub fn remap_file_element_owners(
        &mut self,
        remap: &crate::FileRemap,
    ) -> (Vec<LuaMemberId>, HashSet<FileId>) {
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

        let mut dirty = HashSet::default();
        dirty.extend(self.remap_missed_member_reads(remap));
        for range in &deleted {
            let owner = LuaMemberOwner::Element(range.clone());
            for member in self.get_members(&owner).unwrap_or_default() {
                dirty.insert(member.get_id().file_id);
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
        let forgotten = self.remove_deleted_element_owners(&deleted);
        dirty.remove(&remap.file_id);
        (forgotten, dirty)
    }

    fn remap_missed_member_reads(&mut self, remap: &crate::FileRemap) -> HashSet<FileId> {
        let updates = self
            .missed_member_readers
            .keys()
            .filter_map(|(owner, key)| {
                let LuaMemberOwner::Element(range) = owner else {
                    return None;
                };
                match remap.table_range(range) {
                    crate::Remap::Moved(new) if new != *range => Some((
                        (owner.clone(), key.clone()),
                        Some((LuaMemberOwner::Element(new), key.clone())),
                    )),
                    crate::Remap::Lost => Some(((owner.clone(), key.clone()), None)),
                    crate::Remap::Moved(_) | crate::Remap::Unrelated => None,
                }
            })
            .collect::<Vec<_>>();
        let mut dirty = HashSet::default();
        let mut affected_readers = HashSet::default();

        for (old, new) in updates {
            let readers = self.missed_member_readers.remove(&old).unwrap_or_default();
            affected_readers.extend(readers.iter().copied());
            if let Some(new) = &new {
                self.missed_member_readers
                    .entry(new.clone())
                    .or_default()
                    .extend(readers.iter().copied());
            } else {
                dirty.extend(readers.iter().copied());
            }
            for reader in readers {
                let Some(reads) = self.missed_member_reads_by_file.get_mut(&reader) else {
                    continue;
                };
                reads.retain(|read| read != &old);
                if let Some(new) = &new {
                    reads.push(new.clone());
                }
            }
        }

        for reader in affected_readers {
            if let Some(reads) = self.missed_member_reads_by_file.get_mut(&reader) {
                reads.sort_unstable_by_key(|read| format!("{read:?}"));
                reads.dedup();
            }
        }
        dirty
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
        for (member_id, old_owner, new_owner) in to_move {
            self.detach_member_from_owner(&old_owner, member_id);
            self.set_member_owner(new_owner.clone(), member_id.file_id, member_id);
            self.add_member_to_owner(new_owner, member_id);
            if matches!(old_owner, LuaMemberOwner::Element(_))
                && self
                    .owner_members
                    .get(&old_owner)
                    .is_none_or(|m| m.is_empty())
            {
                self.owner_members.remove(&old_owner);
                // `set_member_owner` files the owner entry under the *member's*
                // file, not the range's. For the cross-file case this pass
                // exists for they are different files, and clearing the wrong
                // one leaves a dead owner in the member file's set for the next
                // sweep to act on.
                self.remove_owner_from_all_files(&old_owner);
            }
        }
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
            if let Some(member_items) = self.owner_members.remove(&owner) {
                let member_ids: Vec<LuaMemberId> = member_items
                    .get_member_items()
                    .flat_map(|item| match item {
                        LuaMemberIndexItem::One(id) => vec![*id],
                        LuaMemberIndexItem::Many(ids) => ids.clone(),
                    })
                    .collect();
                for id in member_ids {
                    self.forget_member(id);
                    forgotten.push(id);
                }
            }
            self.member_owner_key_index.remove(&owner);
            self.member_owner_key_history_index.remove(&owner);
            self.current_owner_member_history.remove(&owner);
            self.remove_owner_from_all_files(&owner);
        }
        forgotten
    }
}

impl LuaMemberIndex {
    /// Replaces the member reads of `file_id` that found nothing.
    pub fn set_missed_member_reads(
        &mut self,
        file_id: FileId,
        reads: impl IntoIterator<Item = (LuaMemberOwner, LuaMemberKey)>,
    ) {
        self.clear_missed_member_reads(file_id);
        let mut reads: Vec<_> = reads
            .into_iter()
            .map(|(owner, key)| (self.canonical_owner(owner), key))
            .collect();
        if reads.is_empty() {
            return;
        }
        reads.sort_unstable_by_key(|read| format!("{read:?}"));
        reads.dedup();
        for read in &reads {
            self.missed_member_readers
                .entry(read.clone())
                .or_default()
                .insert(file_id);
        }
        self.missed_member_reads_by_file.insert(file_id, reads);
    }

    fn clear_missed_member_reads(&mut self, file_id: FileId) {
        let Some(reads) = self.missed_member_reads_by_file.remove(&file_id) else {
            return;
        };
        for read in reads {
            if let Some(readers) = self.missed_member_readers.get_mut(&read) {
                readers.remove(&file_id);
                if readers.is_empty() {
                    self.missed_member_readers.remove(&read);
                }
            }
        }
    }

    /// Files whose read of `key` on `owner` found nothing when they were
    /// analysed.
    pub fn missed_member_readers(
        &self,
        owner: &LuaMemberOwner,
        key: &LuaMemberKey,
    ) -> impl Iterator<Item = FileId> + '_ {
        self.missed_member_readers
            .get(&(owner.clone(), key.clone()))
            .into_iter()
            .flat_map(|readers| readers.iter().copied())
    }
}

impl LuaIndex for LuaMemberIndex {
    fn remove(&mut self, file_id: FileId) {
        self.remove_files(&[file_id]);
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        for &file_id in file_ids {
            self.remove_file_owned_entries(file_id);
            self.clear_missed_member_reads(file_id);
        }
        let removed: HashSet<FileId> = file_ids.iter().copied().collect();
        self.remove_files_members_from_owner_key_indexes(&removed);
        self.member_function_scope_ranges
            .retain(|member_id, _| !removed.contains(&member_id.file_id));
    }

    fn clear(&mut self) {
        self.members.clear();
        self.in_filed.clear();
        self.owner_members.clear();
        self.missed_member_reads_by_file.clear();
        self.missed_member_readers.clear();
        self.member_current_owner.clear();
        self.member_owner_key_index.clear();
        self.member_owner_key_history_index.clear();
        self.owner_key_index_owners_by_file.clear();
        self.current_owner_member_history.clear();
        self.current_members_by_key.clear();
        self.non_overwriting_assignment_members.clear();
        self.preserved_co_writer_reconciled.clear();
        self.homing_revision = 0;
        self.owner_only_members.clear();
        self.synthesized_owner_members.clear();
        self.deferred_index_expr_members.clear();
        self.function_scope_ranges.clear();
        self.member_function_scope_ranges.clear();
        self.definition_site_owner.clear();
        self.path_definition_sites.clear();
        self.definition_sites_by_file.clear();
        self.global_path_class_owner.clear();
        self.global_path_class_candidates.clear();
        self.member_path_provenance.clear();
    }
}

fn member_ids_from_item(item: &LuaMemberIndexItem) -> Vec<LuaMemberId> {
    match item {
        LuaMemberIndexItem::One(id) => vec![*id],
        LuaMemberIndexItem::Many(ids) => ids.clone(),
    }
}
