mod lua_member;
mod lua_member_feature;
mod lua_member_item;
mod lua_member_owner;
mod lua_owner_members;
mod test;

use rowan::{TextRange, TextSize};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::collections::BTreeMap;

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
    global_path_classes_by_file: HashMap<FileId, Vec<GlobalId>>,
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
            owner_only_members: HashSet::default(),
            synthesized_owner_members: HashSet::default(),
            deferred_index_expr_members: HashSet::default(),
            function_scope_ranges: HashMap::default(),
            member_function_scope_ranges: HashMap::default(),
            definition_site_owner: HashMap::default(),
            path_definition_sites: HashMap::default(),
            definition_sites_by_file: HashMap::default(),
            global_path_class_owner: HashMap::default(),
            global_path_classes_by_file: HashMap::default(),
            missed_member_reads_by_file: HashMap::default(),
            missed_member_readers: HashMap::default(),
        }
    }

    pub fn add_member(&mut self, owner: LuaMemberOwner, member: LuaMember) -> LuaMemberId {
        let owner = self.canonical(&owner).clone();
        let id = member.get_id();
        let file_id = member.get_file_id();
        let function_scope = self.assignment_file_define_scope_for_member(&member);
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
                // `Many` is not guaranteed sorted — `classify_member_insert`
                // appends in arrival order — so the ordered fast paths below
                // cannot themselves rule out a duplicate. Enumerating a member
                // twice is worse than the linear scan; these lists are short.
                if ids.contains(&id) {
                    return;
                }
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
        Self::remove_files_members_from_owner_key_map(
            &mut self.member_owner_key_index,
            removed,
            &owners,
        );
        Self::remove_files_members_from_owner_key_map(
            &mut self.member_owner_key_history_index,
            removed,
            &owners,
        );
    }

    fn remove_files_members_from_owner_key_map(
        owner_key_index: &mut HashMap<LuaMemberOwner, HashMap<LuaMemberKey, Vec<LuaMemberId>>>,
        removed: &HashSet<FileId>,
        owners: &HashSet<LuaMemberOwner>,
    ) {
        for owner in owners {
            let Some(key_members) = owner_key_index.get_mut(owner) else {
                continue;
            };
            key_members.retain(|_, member_ids| {
                member_ids.retain(|member_id| !removed.contains(&member_id.file_id));
                !member_ids.is_empty()
            });
            if key_members.is_empty() {
                owner_key_index.remove(owner);
            }
        }
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

    pub fn set_member_owner(
        &mut self,
        owner: LuaMemberOwner,
        file_id: FileId,
        id: LuaMemberId,
    ) -> Option<()> {
        let owner = self.canonical(&owner).clone();
        let previous_owner = self.member_current_owner.insert(id, owner.clone());
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
        self.forget_definition_site(&site);
        self.definition_site_owner.insert(site.clone(), owner);
        let sites = self.path_definition_sites.entry(path).or_default();
        if let Err(at) = sites.binary_search_by_key(&site_sort_key(&site), site_sort_key) {
            sites.insert(at, site.clone());
        }
        self.definition_sites_by_file
            .entry(site.file_id)
            .or_default()
            .push(site);
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

    /// Records that `path`'s declaration is annotated `---@class`, and moves
    /// the members already filed under the path onto the class.
    ///
    /// A global with a class annotation has two names for one table, and every
    /// reader of the annotated type looks the members up on the class. Which
    /// class wins is decided by name so two files annotating one global do not
    /// depend on which was analysed first.
    pub fn set_global_path_class(
        &mut self,
        path: GlobalId,
        class: crate::LuaTypeDeclId,
        file_id: FileId,
    ) {
        if let Some(current) = self
            .global_path_class_owner
            .get(&path)
            .and_then(LuaMemberOwner::get_type_id)
            && current.get_name() <= class.get_name()
        {
            return;
        }
        self.global_path_class_owner
            .insert(path.clone(), LuaMemberOwner::Type(class.clone()));
        self.global_path_classes_by_file
            .entry(file_id)
            .or_default()
            .push(path.clone());

        let path_owner = LuaMemberOwner::GlobalPath(path);
        let class_owner = LuaMemberOwner::Type(class);
        let mut member_ids = self
            .owner_members
            .get(&path_owner)
            .into_iter()
            .flat_map(|members| members.get_member_items())
            .flat_map(member_ids_from_item)
            .filter(|id| {
                !self.synthesized_owner_members.contains(id)
                    && self.member_current_owner.get(id) == Some(&path_owner)
            })
            .collect::<Vec<_>>();
        member_ids.sort_unstable_by_key(|id| member_id_sort_key(*id));
        member_ids.dedup();
        for member_id in member_ids {
            self.set_member_owner(class_owner.clone(), member_id.file_id, member_id);
            self.add_member_to_owner(class_owner.clone(), member_id);
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
    /// `classify_member_insert` accumulates guarded writers only when every
    /// writer already in the slot is marked, and a write is marked *after* it
    /// is inserted. On a cold build the co-writer is in the same batch and its
    /// own mark merges it back; re-indexing one file collapses the slot to that
    /// file's writer and drops a co-writer the batch does not re-analyse, which
    /// nothing then restores. Re-applying the accumulate rule here is the
    /// answer a different arrival order would have produced, so both paths
    /// settle on it.
    fn readmit_preserved_assignment_co_writers(&mut self, member_id: LuaMemberId) -> Option<()> {
        if !self.is_preserved_assignment_member(member_id) {
            return None;
        }
        let owner = self.member_current_owner.get(&member_id)?.clone();
        let key = self.get_member(&member_id)?.get_key().clone();
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
        let co_writers = self
            .get_current_owner_members_for_key(&owner, &key)
            .into_iter()
            .map(|member| member.get_id())
            .collect::<Vec<_>>();
        for co_writer in co_writers {
            if co_writer != member_id
                && !visible.contains(&co_writer)
                && !self.owner_only_members.contains(&co_writer)
                && self.is_preserved_assignment_member(co_writer)
            {
                self.merge_member_into_owner_item(owner.clone(), key.clone(), co_writer);
            }
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
        let owner = self.canonical(owner);
        let Some(member_ids) = self
            .member_owner_key_history_index
            .get(owner)
            .and_then(|owner_items| owner_items.get(key))
        else {
            return Vec::new();
        };

        let mut members = member_ids
            .iter()
            .copied()
            .filter_map(|member_id| {
                self.member_current_owner
                    .get(&member_id)
                    .filter(|current_owner| *current_owner == owner)?;
                self.get_member(&member_id)
                    .filter(|member| member.get_key() == key)
            })
            .collect::<Vec<_>>();
        members.sort_by_key(|member| member_id_sort_key(member.get_id()));
        members
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
        if let Some(owner) = self.member_current_owner.get(&member_id).cloned() {
            self.remove_member_from_all_owner_key_indexes(&owner, member_id);
            self.remove_current_owner_member(&owner, member_id);
            self.remove_current_member_key(member_id);
        }
        self.members.remove(&member_id);
        self.member_current_owner.remove(&member_id);
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
        for path in self
            .global_path_classes_by_file
            .remove(&file_id)
            .unwrap_or_default()
        {
            self.global_path_class_owner.remove(&path);
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
        self.owner_only_members.clear();
        self.synthesized_owner_members.clear();
        self.deferred_index_expr_members.clear();
        self.function_scope_ranges.clear();
        self.member_function_scope_ranges.clear();
        self.definition_site_owner.clear();
        self.path_definition_sites.clear();
        self.definition_sites_by_file.clear();
        self.global_path_class_owner.clear();
        self.global_path_classes_by_file.clear();
    }
}

fn member_ids_from_item(item: &LuaMemberIndexItem) -> Vec<LuaMemberId> {
    match item {
        LuaMemberIndexItem::One(id) => vec![*id],
        LuaMemberIndexItem::Many(ids) => ids.clone(),
    }
}
