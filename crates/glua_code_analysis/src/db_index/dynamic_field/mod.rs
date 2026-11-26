use std::hash::Hash;
use std::ops::Deref;

use internment::ArcIntern;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use crate::db_index::cow_shards::CowShards;
use rowan::TextRange;
use smol_str::SmolStr;

use super::traits::LuaIndex;
use crate::{
    DbIndex, FileId, GlobalId, InFiled, LuaMemberId, LuaMemberKey, LuaMemberOwner, LuaTypeDeclId,
    db_index::derivation::{self, FactKey, Fingerprint},
};

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum DynamicFieldOwner {
    Type(LuaTypeDeclId),
    Table(InFiled<TextRange>),
    /// A global path whose table several files bootstrap. Keyed by the path
    /// rather than by one of its literals, so `X.k[v] = w` written against any
    /// of them lands in the bucket every reader of `X.k` consults.
    GlobalPath(GlobalId),
}

/// The one owner a dynamic field written on `owner` belongs to.
///
/// Mirrors [`crate::LuaMemberIndex::canonical_owner`]: a table literal that
/// initialises a global path is not an owner of its own, so a field registered
/// on it belongs to the path (or, once the path's declaration carries a
/// `---@class`, to that class). Literals that initialise nothing keep their
/// range, exactly as before.
pub fn canonical_dynamic_field_owner(db: &DbIndex, owner: DynamicFieldOwner) -> DynamicFieldOwner {
    let DynamicFieldOwner::Table(range) = &owner else {
        return owner;
    };
    match db
        .get_member_index()
        .canonical_owner(LuaMemberOwner::Element(range.clone()))
    {
        LuaMemberOwner::GlobalPath(path) => DynamicFieldOwner::GlobalPath(path),
        LuaMemberOwner::Type(type_id) => DynamicFieldOwner::Type(type_id),
        _ => owner,
    }
}

/// The canonical dynamic-field owner a member owner names.
pub fn dynamic_field_owner_of(db: &DbIndex, owner: &LuaMemberOwner) -> Option<DynamicFieldOwner> {
    match db.get_member_index().canonical_owner(owner.clone()) {
        LuaMemberOwner::Type(type_id) => Some(DynamicFieldOwner::Type(type_id)),
        LuaMemberOwner::GlobalPath(path) => Some(DynamicFieldOwner::GlobalPath(path)),
        LuaMemberOwner::Element(range) => Some(DynamicFieldOwner::Table(range)),
        LuaMemberOwner::LocalUnresolve => None,
    }
}

/// True when a wildcard (computed-key) assignment decides which names `owner`
/// holds: it has no named dynamic fields and no named members an annotation
/// declares. Fields its own literal or assignments write leave it open, as the
/// computed writes add names beside them.
pub fn is_pure_wildcard_registry(db: &DbIndex, owner: &DynamicFieldOwner) -> bool {
    let index = db.get_dynamic_field_index();
    if !index.has_wildcard_definitions(owner) {
        return false;
    }
    if index
        .get_fields(owner)
        .is_some_and(|fields| !fields.is_empty())
    {
        return false;
    }

    let member_owner = match owner {
        DynamicFieldOwner::Type(id) => LuaMemberOwner::Type(id.clone()),
        DynamicFieldOwner::Table(range) => LuaMemberOwner::Element(range.clone()),
        DynamicFieldOwner::GlobalPath(path) => LuaMemberOwner::GlobalPath(path.clone()),
    };
    let member_owner = db.get_member_index().canonical_owner(member_owner);
    db.get_member_index()
        .get_members(&member_owner)
        .is_none_or(|members| {
            !members.iter().any(|member| {
                member.get_feature().is_decl()
                    && matches!(
                        member.get_key(),
                        LuaMemberKey::Name(_) | LuaMemberKey::Integer(_)
                    )
            })
        })
}

/// Sorted, deduplicated definition sites for one `(owner, field_name)` slot.
/// Most slots hold one site, which lives inline with no heap allocation.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct DefSites(DefSitesInner);

#[derive(Clone, Default, PartialEq, Eq, Hash)]
enum DefSitesInner {
    #[default]
    Empty,
    One(InFiled<TextRange>),
    Many(Box<[InFiled<TextRange>]>),
}

const _: () = assert!(std::mem::size_of::<DefSites>() == 24);

impl Deref for DefSites {
    type Target = [InFiled<TextRange>];

    fn deref(&self) -> &[InFiled<TextRange>] {
        match &self.0 {
            DefSitesInner::Empty => &[],
            DefSitesInner::One(site) => std::slice::from_ref(site),
            DefSitesInner::Many(sites) => sites,
        }
    }
}

impl std::fmt::Debug for DefSites {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<'a> IntoIterator for &'a DefSites {
    type Item = &'a InFiled<TextRange>;
    type IntoIter = std::slice::Iter<'a, InFiled<TextRange>>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl DefSites {
    fn from_vec(sites: Vec<InFiled<TextRange>>) -> Self {
        Self(match sites.len() {
            0 => DefSitesInner::Empty,
            1 => DefSitesInner::One(sites[0].clone()),
            _ => DefSitesInner::Many(sites.into_boxed_slice()),
        })
    }

    fn as_slice(&self) -> &[InFiled<TextRange>] {
        self
    }

    fn insert_sorted(&mut self, definition: InFiled<TextRange>) -> bool {
        let at = self.partition_point(|existing| {
            definition_sort_key(existing) < definition_sort_key(&definition)
        });
        if self.get(at) == Some(&definition) {
            return false;
        }
        match &mut self.0 {
            DefSitesInner::Empty => self.0 = DefSitesInner::One(definition),
            DefSitesInner::One(existing) => {
                let existing = existing.clone();
                let pair = if at == 0 {
                    [definition, existing]
                } else {
                    [existing, definition]
                };
                self.0 = DefSitesInner::Many(Box::new(pair));
            }
            DefSitesInner::Many(sites) => {
                let mut vec = Vec::with_capacity(sites.len() + 1);
                vec.extend_from_slice(&sites[..at]);
                vec.push(definition);
                vec.extend_from_slice(&sites[at..]);
                self.0 = DefSitesInner::Many(vec.into_boxed_slice());
            }
        }
        true
    }

    fn remove(&mut self, definition: &InFiled<TextRange>) -> bool {
        match &mut self.0 {
            DefSitesInner::Empty => false,
            DefSitesInner::One(existing) => {
                if existing == definition {
                    self.0 = DefSitesInner::Empty;
                    true
                } else {
                    false
                }
            }
            DefSitesInner::Many(sites) => {
                let Some(at) = sites.iter().position(|existing| existing == definition) else {
                    return false;
                };
                if sites.len() == 2 {
                    self.0 = DefSitesInner::One(sites[1 - at].clone());
                } else {
                    let mut vec = Vec::with_capacity(sites.len() - 1);
                    vec.extend_from_slice(&sites[..at]);
                    vec.extend_from_slice(&sites[at + 1..]);
                    self.0 = DefSitesInner::Many(vec.into_boxed_slice());
                }
                true
            }
        }
    }

    fn merge_from(&mut self, other: DefSites) {
        if other.is_empty() {
            return;
        }
        if self.is_empty() {
            *self = other;
            return;
        }
        let mut merged = Vec::with_capacity(self.len() + other.len());
        merged.extend_from_slice(self);
        merged.extend_from_slice(&other);
        merged.sort_unstable_by_key(definition_sort_key);
        merged.dedup();
        *self = Self::from_vec(merged);
    }
}

/// Index tracking dynamically-assigned fields on typed variables.
///
/// When `gmod.inferDynamicFields` is enabled, field assignments like
/// `player.customField = value` are recorded here so that both
/// `InjectField` and `UndefinedField` diagnostics can be suppressed.
#[derive(Debug, Default, Clone)]
pub struct DynamicFieldIndex {
    /// owner → (field_name → assignment locations); also answers assigning-file queries.
    field_definitions: CowShards<DynamicFieldOwner, HashMap<SmolStr, DefSites>>,
    /// Exact fields collected on their original owner, excluding inherited propagation.
    direct_field_definitions: CowShards<DynamicFieldOwner, HashMap<SmolStr, DefSites>>,
    /// Expression-key members whose complete string domain was resolved to finite names.
    finite_named_members: CowShards<DynamicFieldOwner, HashSet<LuaMemberId>>,
    /// owner → assignment locations for writes through non-literal keys.
    wildcard_definitions: CowShards<DynamicFieldOwner, Vec<InFiled<TextRange>>>,
    /// field_name → write sites (`file`, index-key `range`) whose
    /// `X.field_name = ...` receiver could not be resolved to any owner, so
    /// the write landed nowhere. Names here are known to be assigned
    /// *somewhere* at runtime even though no table in the index can claim
    /// them. Keyed by index key, so two same-name writes in one file each own
    /// their record.
    unattributed_fields: CowShards<SmolStr, HashSet<InFiled<TextRange>>>,
    /// field_name → the types that hold a direct definition of the name,
    /// each with how many it holds, so tasks that each drop one merge to the
    /// count left.
    declaring_types: CowShards<SmolStr, HashMap<LuaTypeDeclId, u32>>,
    /// Write site → what its derivation filed, so retracting the site or
    /// removing its file visits only the owners it touched. Keyed by the
    /// site's statement or call range.
    site_contributions: CowShards<InFiled<TextRange>, SiteContributions>,
    /// Multiset fingerprint of each owner's named slots, maintained by
    /// [`DynamicFieldIndex::settle_named`].
    owner_named_fingerprints: CowShards<DynamicFieldOwner, Fingerprint>,
}

/// What one write site filed, each list in the order its derivation filed
/// it: one derivation fills a site, so the order is that computation's.
/// Most write sites file a single named field, which lives inline.
#[derive(Debug, Clone, Default, PartialEq)]
enum SiteContributions {
    #[default]
    Empty,
    OneNamed(DynamicFieldOwner, ArcIntern<SmolStr>, TextRange),
    Many(Box<SiteContributionsData>),
}

const _: () = assert!(std::mem::size_of::<SiteContributions>() <= 48);

#[derive(Debug, Clone, Default, PartialEq)]
struct SiteContributionsData {
    /// Definitions new to the union of direct and propagated definitions.
    named: Vec<(DynamicFieldOwner, ArcIntern<SmolStr>, TextRange)>,
    wildcards: Vec<(DynamicFieldOwner, TextRange)>,
    finite: Vec<(DynamicFieldOwner, LuaMemberId)>,
    unattributed: Vec<(ArcIntern<SmolStr>, TextRange)>,
}

impl SiteContributions {
    fn is_empty(&self) -> bool {
        match self {
            Self::Empty => true,
            Self::OneNamed(..) => false,
            Self::Many(data) => {
                data.named.is_empty()
                    && data.wildcards.is_empty()
                    && data.finite.is_empty()
                    && data.unattributed.is_empty()
            }
        }
    }

    fn normalize(&mut self) {
        if let Self::Many(data) = self {
            if data.wildcards.is_empty() && data.finite.is_empty() && data.unattributed.is_empty() {
                match data.named.len() {
                    0 => *self = Self::Empty,
                    1 => {
                        let (owner, name, range) = data.named.pop().unwrap();
                        *self = Self::OneNamed(owner, name, range);
                    }
                    _ => {}
                }
            }
        }
    }

    fn as_many_mut(&mut self) -> &mut SiteContributionsData {
        match self {
            Self::Empty => {
                *self = Self::Many(Box::default());
                let Self::Many(data) = self else {
                    unreachable!()
                };
                data
            }
            Self::OneNamed(..) => {
                let Self::OneNamed(owner, name, range) = std::mem::take(self) else {
                    unreachable!()
                };
                let mut data = Box::new(SiteContributionsData::default());
                data.named.push((owner, name, range));
                *self = Self::Many(data);
                let Self::Many(data) = self else {
                    unreachable!()
                };
                data
            }
            Self::Many(data) => data,
        }
    }

    fn push_named(&mut self, owner: DynamicFieldOwner, field_name: SmolStr, range: TextRange) {
        let field_name = ArcIntern::new(field_name);
        match self {
            Self::Empty => *self = Self::OneNamed(owner, field_name, range),
            Self::OneNamed(..) | Self::Many(_) => {
                self.as_many_mut().named.push((owner, field_name, range));
            }
        }
    }

    fn push_wildcard(&mut self, owner: DynamicFieldOwner, range: TextRange) {
        let wildcards = &mut self.as_many_mut().wildcards;
        reserve_first(wildcards);
        wildcards.push((owner, range));
    }

    fn push_finite(&mut self, owner: DynamicFieldOwner, member_id: LuaMemberId) {
        let finite = &mut self.as_many_mut().finite;
        reserve_first(finite);
        finite.push((owner, member_id));
    }

    fn push_unattributed(&mut self, field_name: SmolStr, range: TextRange) {
        let unattributed = &mut self.as_many_mut().unattributed;
        reserve_first(unattributed);
        unattributed.push((ArcIntern::new(field_name), range));
    }

    fn remove_finite(&mut self, owner: &DynamicFieldOwner, member_id: &LuaMemberId) {
        if let Self::Many(data) = self {
            data.finite.retain(|(o, m)| o != owner || m != member_id);
            self.normalize();
        }
    }

    fn retain_owners(&mut self, keep: impl Fn(&DynamicFieldOwner) -> bool) -> bool {
        match self {
            Self::Empty => false,
            Self::OneNamed(owner, _, _) => {
                if keep(owner) {
                    false
                } else {
                    *self = Self::Empty;
                    true
                }
            }
            Self::Many(data) => {
                let before = data.named.len() + data.wildcards.len() + data.finite.len();
                data.named.retain(|(owner, _, _)| keep(owner));
                data.wildcards.retain(|(owner, _)| keep(owner));
                data.finite.retain(|(owner, _)| keep(owner));
                let after = data.named.len() + data.wildcards.len() + data.finite.len();
                self.normalize();
                after != before
            }
        }
    }

    fn remap_owners(
        &mut self,
        mut remap: impl FnMut(&DynamicFieldOwner) -> Option<DynamicFieldOwner>,
    ) {
        match self {
            Self::Empty => {}
            Self::OneNamed(owner, _, _) => {
                if let Some(new) = remap(owner) {
                    *owner = new;
                }
            }
            Self::Many(data) => {
                let owners = data
                    .named
                    .iter_mut()
                    .map(|(owner, _, _)| owner)
                    .chain(data.wildcards.iter_mut().map(|(owner, _)| owner))
                    .chain(data.finite.iter_mut().map(|(owner, _)| owner));
                for owner in owners {
                    if let Some(new) = remap(owner) {
                        *owner = new;
                    }
                }
            }
        }
    }
}

/// Ranges hash as positions of the site's file, which fingerprinting sets.
impl std::hash::Hash for SiteContributions {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            Self::Empty => {
                0usize.hash(state);
                0usize.hash(state);
                (&[] as &[(DynamicFieldOwner, LuaMemberId)]).hash(state);
                0usize.hash(state);
            }
            Self::OneNamed(owner, field_name, range) => {
                1usize.hash(state);
                owner.hash(state);
                field_name.as_str().hash(state);
                derivation::hash_local(range, state);
                0usize.hash(state);
                (&[] as &[(DynamicFieldOwner, LuaMemberId)]).hash(state);
                0usize.hash(state);
            }
            Self::Many(data) => {
                data.named.len().hash(state);
                for (owner, field_name, range) in &data.named {
                    owner.hash(state);
                    field_name.as_str().hash(state);
                    derivation::hash_local(range, state);
                }
                data.wildcards.len().hash(state);
                for (owner, range) in &data.wildcards {
                    owner.hash(state);
                    derivation::hash_local(range, state);
                }
                data.finite.as_slice().hash(state);
                data.unattributed.len().hash(state);
                for (field_name, range) in &data.unattributed {
                    field_name.as_str().hash(state);
                    derivation::hash_local(range, state);
                }
            }
        }
    }
}

/// An owner whose named fingerprint an install rebuilds: the fingerprint and
/// the slot fingerprints of the names the install changes, before it.
pub(crate) struct OwnerSlotsToRebuild {
    owner: DynamicFieldOwner,
    digest: Fingerprint,
    slots: Vec<(SmolStr, Fingerprint)>,
}

/// One named slot's contribution to its owner's multiset fingerprint.
fn named_entry(field_name: &str, slot: Fingerprint) -> Fingerprint {
    if slot == derivation::ABSENT {
        0
    } else {
        derivation::fingerprint_of(&(field_name, slot))
    }
}

fn definition_sort_key(definition: &InFiled<TextRange>) -> (u32, u32, u32) {
    (
        definition.file_id.id,
        definition.value.start().into(),
        definition.value.end().into(),
    )
}

/// Merges one owner's named definitions into another's, per field name, so a
/// name both hold keeps the definitions of each.
fn merge_field_definitions(
    into: &mut HashMap<SmolStr, DefSites>,
    from: HashMap<SmolStr, DefSites>,
) {
    for (field_name, definitions) in from {
        // The canonical order and the no-duplicates rule `add_field_inner`
        // maintains on insert have to survive the merge.
        into.entry(field_name).or_default().merge_from(definitions);
    }
}

/// Files `definition` in canonical order unless `into` already holds it,
/// so a file's rebuild restores the list a cold build has. Returns whether
/// it was new.
fn insert_definition(into: &mut Vec<InFiled<TextRange>>, definition: InFiled<TextRange>) -> bool {
    let at = into.partition_point(|existing| {
        definition_sort_key(existing) < definition_sort_key(&definition)
    });
    if into.get(at) == Some(&definition) {
        return false;
    }
    reserve_first(into);
    into.insert(at, definition);
    true
}

/// Most lists hold one entry: the first insert gets room for one, later
/// ones grow as usual.
fn reserve_first<T>(list: &mut Vec<T>) {
    if list.capacity() == 0 {
        list.reserve_exact(1);
    }
}

/// Merges two tasks' changes to one list of definition sites, which stays in
/// canonical order.
fn merge_definitions(
    current: &mut Option<Vec<InFiled<TextRange>>>,
    frozen: Option<&Vec<InFiled<TextRange>>>,
    fork: Option<&Vec<InFiled<TextRange>>>,
) -> crate::db_index::cow_shards::Merged {
    crate::db_index::cow_shards::merge_sorted_by(current, frozen, fork, definition_sort_key)
}

fn merge_def_sites(
    current: &mut Option<DefSites>,
    frozen: Option<&DefSites>,
    fork: Option<&DefSites>,
) -> crate::db_index::cow_shards::Merged {
    let to_vec = |sites: Option<&DefSites>| sites.map(|sites| sites.to_vec());
    let mut merged = to_vec(current.as_ref());
    crate::db_index::cow_shards::merge_sorted_by(
        &mut merged,
        to_vec(frozen).as_ref(),
        to_vec(fork).as_ref(),
        definition_sort_key,
    )?;
    *current = merged.map(DefSites::from_vec);
    Ok(())
}

fn merge_wildcard_definitions(into: &mut Vec<InFiled<TextRange>>, from: Vec<InFiled<TextRange>>) {
    for definition in from {
        insert_definition(into, definition);
    }
}

impl DynamicFieldIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether no write site filed anything.
    pub(crate) fn is_empty(&self) -> bool {
        self.site_contributions.is_empty()
    }

    /// The record of what `site` filed, created on its first contribution.
    fn site_record(&mut self, site: &InFiled<TextRange>) -> &mut SiteContributions {
        self.site_contributions.entry(site.clone()).or_default()
    }

    /// Files a direct definition of `field_name` on `owner` at the index key
    /// `range`, written by `site`. Returns whether the definition site was new
    /// to the index — to the union of definitions or to this owner's direct
    /// set.
    pub fn add_field(
        &mut self,
        site: &InFiled<TextRange>,
        owner: DynamicFieldOwner,
        field_name: SmolStr,
        range: TextRange,
    ) -> bool {
        derivation::note_write(|| FactKey::DynamicFieldSite(site.clone()));
        let old_slot = self.named_slot_fingerprint(&owner, &field_name);
        let definition = InFiled::new(site.file_id, range);
        let direct_definitions = self
            .direct_field_definitions
            .entry(owner.clone())
            .or_default()
            .entry(field_name.clone())
            .or_default();
        let is_new_direct = direct_definitions.insert_sorted(definition);
        if is_new_direct && let DynamicFieldOwner::Type(type_id) = &owner {
            let held = self
                .declaring_types
                .entry(field_name.clone())
                .or_default()
                .entry(type_id.clone())
                .or_default();
            *held += 1;
            if *held == 1 {
                derivation::note_write(|| FactKey::DynamicFieldName(field_name.as_str().into()));
            }
        }
        let is_new_definition =
            self.add_field_inner(site, owner.clone(), field_name.clone(), range);
        self.settle_named(&owner, &field_name, old_slot);
        if is_new_direct && !is_new_definition {
            // The union already held the definition through propagation, so
            // `add_field_inner` filed nothing — but direct membership is a
            // novel fact for the slot's readers all the same.
            self.note_dynamic_write(owner, Some(field_name));
        }
        is_new_direct || is_new_definition
    }

    /// [`Self::add_field`] for a definition propagated to a super type: it
    /// joins the union of definitions only.
    pub fn add_propagated_field(
        &mut self,
        site: &InFiled<TextRange>,
        owner: DynamicFieldOwner,
        field_name: SmolStr,
        range: TextRange,
    ) -> bool {
        derivation::note_write(|| FactKey::DynamicFieldSite(site.clone()));
        let old_slot = self.named_slot_fingerprint(&owner, &field_name);
        let is_new_definition =
            self.add_field_inner(site, owner.clone(), field_name.clone(), range);
        self.settle_named(&owner, &field_name, old_slot);
        is_new_definition
    }

    fn add_field_inner(
        &mut self,
        site: &InFiled<TextRange>,
        owner: DynamicFieldOwner,
        field_name: SmolStr,
        range: TextRange,
    ) -> bool {
        let field_definitions = self
            .field_definitions
            .entry(owner.clone())
            .or_default()
            .entry(field_name.clone())
            .or_default();
        let is_new_definition = field_definitions.insert_sorted(InFiled::new(site.file_id, range));

        if is_new_definition {
            self.note_dynamic_write(owner.clone(), Some(field_name.clone()));
            self.site_record(site).push_named(owner, field_name, range);
        }
        is_new_definition
    }

    /// Returns whether the definition site was new to the index.
    pub fn add_wildcard_definition(
        &mut self,
        site: &InFiled<TextRange>,
        owner: DynamicFieldOwner,
        range: TextRange,
    ) -> bool {
        derivation::note_write(|| FactKey::DynamicFieldSite(site.clone()));
        let definitions = self.wildcard_definitions.entry(owner.clone()).or_default();
        let definition = InFiled::new(site.file_id, range);
        let is_new_definition = insert_definition(definitions, definition);
        if is_new_definition {
            derivation::note_write(|| FactKey::DynamicFieldWildcards(owner.clone()));
            self.site_record(site).push_wildcard(owner, range);
        }
        is_new_definition
    }

    /// Records a dynamic-field definition change.
    pub(crate) fn note_dynamic_write(
        &mut self,
        owner: DynamicFieldOwner,
        field_name: Option<SmolStr>,
    ) {
        derivation::note_write(|| FactKey::dynamic_field(owner, field_name.map(Into::into)));
    }

    /// Returns whether the unattributed record was new to the index.
    ///
    /// `range` is the write's index-key range; with the site's file it names
    /// the write, so two same-name writes in one file hold independent
    /// records.
    pub fn add_unattributed_field(
        &mut self,
        site: &InFiled<TextRange>,
        field_name: SmolStr,
        range: TextRange,
    ) -> bool {
        derivation::note_write(|| FactKey::DynamicFieldSite(site.clone()));
        let is_new = self
            .unattributed_fields
            .entry(field_name.clone())
            .or_default()
            .insert(InFiled::new(site.file_id, range));
        if is_new {
            derivation::note_write(|| FactKey::UnattributedField(field_name.as_str().into()));
            self.site_record(site).push_unattributed(field_name, range);
        }
        is_new
    }

    /// Withdraws everything `site` filed, before its derivation runs again or
    /// with its file. Each contribution is one entry `site` alone produced:
    /// an index key belongs to one write site.
    pub fn retract_site(&mut self, site: &InFiled<TextRange>) {
        let Some(contributions) = self.site_contributions.remove(site) else {
            return;
        };
        let file_id = site.file_id;
        match contributions {
            SiteContributions::Empty => {}
            SiteContributions::OneNamed(owner, field_name, range) => {
                self.retract_named(file_id, &owner, &field_name, range);
            }
            SiteContributions::Many(data) => {
                for (owner, field_name, range) in data.named {
                    self.retract_named(file_id, &owner, &field_name, range);
                }
                for (owner, range) in data.wildcards {
                    let definition = InFiled::new(file_id, range);
                    if let Some(definitions) = self.wildcard_definitions.get_mut(&owner) {
                        definitions.retain(|existing| existing != &definition);
                        if definitions.is_empty() {
                            self.wildcard_definitions.remove(&owner);
                        }
                    }
                    derivation::note_write(|| FactKey::DynamicFieldWildcards(owner.clone()));
                }
                for (owner, member_id) in data.finite {
                    derivation::note_write(|| FactKey::DynamicFieldFinite(owner.clone()));
                    if let Some(members) = self.finite_named_members.get_mut(&owner) {
                        members.remove(&member_id);
                        if members.is_empty() {
                            self.finite_named_members.remove(&owner);
                        }
                    }
                }
                for (field_name, range) in data.unattributed {
                    derivation::note_write(|| {
                        FactKey::UnattributedField(field_name.as_str().into())
                    });
                    let definition = InFiled::new(file_id, range);
                    if let Some(sites) = self.unattributed_fields.get_mut(field_name.as_str()) {
                        sites.remove(&definition);
                        if sites.is_empty() {
                            self.unattributed_fields.remove(field_name.as_str());
                        }
                    }
                }
            }
        }
    }

    /// Withdraws one named definition from the union and the direct set.
    fn retract_named(
        &mut self,
        file_id: FileId,
        owner: &DynamicFieldOwner,
        field_name: &SmolStr,
        range: TextRange,
    ) {
        let old_slot = self.named_slot_fingerprint(owner, field_name);
        let definition = InFiled::new(file_id, range);
        remove_field_definition(&mut self.field_definitions, owner, field_name, &definition);
        let removed_direct = remove_field_definition(
            &mut self.direct_field_definitions,
            owner,
            field_name,
            &definition,
        );
        if removed_direct
            && let DynamicFieldOwner::Type(type_id) = owner
            && let Some(types) = self.declaring_types.get_mut(field_name.as_str())
            && let Some(held) = types.get_mut(type_id)
        {
            *held -= 1;
            if *held == 0 {
                types.remove(type_id);
                derivation::note_write(|| FactKey::DynamicFieldName(field_name.as_str().into()));
            }
            if types.is_empty() {
                self.declaring_types.remove(field_name.as_str());
            }
        }
        // Draining a definition is a novel fact for the slot's readers,
        // mirroring the signature index's removal tombstones.
        self.note_dynamic_write(owner.clone(), Some(field_name.clone()));
        self.settle_named(owner, field_name, old_slot);
    }

    /// Re-keys the table-literal owners in the edited file and drops the
    /// groups whose literal the edit destroyed.
    ///
    /// Returns the dynamic fields of each file that contributed a field to a
    /// destroyed group: a dynamic field is filed under the literal's range and
    /// a write from another file is not re-collected when this one is
    /// re-indexed, so nothing else rebuilds it.
    pub fn remap_file_table_owners(&mut self, remap: &crate::FileRemap) -> Vec<crate::LostFact> {
        // A table literal's owner lives in its file's shard, so the file's
        // entries are exactly its literals' owners.
        let file_id = remap.file_id;
        let owners: HashSet<DynamicFieldOwner> = self
            .field_definitions
            .file_entries(file_id)
            .map(|(owner, _)| owner)
            .chain(
                self.direct_field_definitions
                    .file_entries(file_id)
                    .map(|(owner, _)| owner),
            )
            .chain(
                self.finite_named_members
                    .file_entries(file_id)
                    .map(|(owner, _)| owner),
            )
            .chain(
                self.wildcard_definitions
                    .file_entries(file_id)
                    .map(|(owner, _)| owner),
            )
            .filter(|owner| {
                matches!(owner, DynamicFieldOwner::Table(range) if range.file_id == file_id)
            })
            .cloned()
            .collect();

        let mut moved = rustc_hash::FxHashMap::default();
        let mut lost: HashSet<DynamicFieldOwner> = HashSet::default();
        for owner in owners {
            let DynamicFieldOwner::Table(range) = &owner else {
                continue;
            };
            match remap.table_range(range) {
                crate::Remap::Moved(new) => {
                    if &new != range {
                        moved.insert(range.clone(), new);
                    }
                }
                crate::Remap::Unrelated => {}
                crate::Remap::Lost => {
                    lost.insert(owner);
                }
            }
        }

        self.remap_table_ranges(&moved);

        let mut dirty = Vec::new();
        if !lost.is_empty() {
            // Ticked in a stable owner order: the counter values must not
            // depend on hash iteration. Comparison-only, but deterministic
            // anyway.
            let mut lost = lost.into_iter().collect::<Vec<_>>();
            lost.sort_by_cached_key(|owner| format!("{owner:?}"));
            let contributors = self.owners_contributors(&lost);
            for owner in &lost {
                self.note_whole_owner_write(owner);
                self.owner_named_fingerprints.remove(owner);
                // Dropping the group is a novel fact for its readers.
                let field_names = self
                    .field_definitions
                    .get(owner)
                    .map(|fields| fields.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default();
                let has_wildcards = self.wildcard_definitions.contains_key(owner);
                for field_name in field_names {
                    self.note_dynamic_write(owner.clone(), Some(field_name));
                }
                if has_wildcards {
                    derivation::note_write(|| FactKey::DynamicFieldWildcards(owner.clone()));
                }
                self.field_definitions.remove(owner);
                self.direct_field_definitions.remove(owner);
                self.finite_named_members.remove(owner);
                self.wildcard_definitions.remove(owner);
            }
            for site in self.sites_of(&contributors) {
                let Some(entries) = self.site_contributions.get_mut(&site) else {
                    continue;
                };
                if entries.retain_owners(|owner| !lost.contains(owner)) {
                    dirty.push(crate::LostFact::new(
                        site.file_id,
                        FactKey::DynamicFieldSite(site.clone()),
                    ));
                }
                if entries.is_empty() {
                    self.site_contributions.remove(&site);
                }
            }
        }
        dirty
    }

    fn holds_owner(&self, owner: &DynamicFieldOwner) -> bool {
        self.field_definitions.contains_key(owner)
            || self.direct_field_definitions.contains_key(owner)
            || self.finite_named_members.contains_key(owner)
            || self.wildcard_definitions.contains_key(owner)
    }

    /// The files whose contribution lists can name one of `owners`: each
    /// contribution is filed with the definition or mark it produced, so the
    /// owner-keyed stores name every contributor. Sorted.
    fn owners_contributors(&self, owners: &[DynamicFieldOwner]) -> Vec<FileId> {
        let mut files = owners
            .iter()
            .flat_map(|owner| {
                let named = [&self.field_definitions, &self.direct_field_definitions]
                    .into_iter()
                    .filter_map(|definitions| definitions.get(owner))
                    .flat_map(HashMap::values)
                    .flatten()
                    .map(|definition| definition.file_id);
                let wildcards = self
                    .wildcard_definitions
                    .get(owner)
                    .into_iter()
                    .flatten()
                    .map(|definition| definition.file_id);
                let finite = self
                    .finite_named_members
                    .get(owner)
                    .into_iter()
                    .flatten()
                    .map(|member_id| member_id.file_id);
                named.chain(wildcards).chain(finite)
            })
            .collect::<Vec<_>>();
        files.sort_unstable();
        files.dedup();
        files
    }

    /// Re-keys owners whose table literal shifted offset.
    ///
    /// A dynamic field is filed under the literal's range, and a write from
    /// another file is not re-collected when this one is re-indexed, so a
    /// stale key makes the field unreachable from the type the literal has.
    pub fn remap_table_ranges(
        &mut self,
        map: &rustc_hash::FxHashMap<InFiled<TextRange>, InFiled<TextRange>>,
    ) {
        fn remap_owner(
            owner: &DynamicFieldOwner,
            map: &rustc_hash::FxHashMap<InFiled<TextRange>, InFiled<TextRange>>,
        ) -> Option<DynamicFieldOwner> {
            match owner {
                DynamicFieldOwner::Table(range) => map
                    .get(range)
                    .map(|new| DynamicFieldOwner::Table(new.clone())),
                DynamicFieldOwner::Type(_) | DynamicFieldOwner::GlobalPath(_) => None,
            }
        }

        // The moved group is merged into whatever the target key already
        // holds rather than replacing it. Only literals with a stable anchor
        // are in `map`, so an unanchored literal's cross-file entry can already
        // sit on the range an anchored one moves onto; `extend` on the nested
        // maps would drop that entry instead of merging it.
        let mut sorted = map.iter().collect::<Vec<_>>();
        sorted.sort_unstable_by_key(|(old, _)| (old.file_id, old.value.start(), old.value.end()));
        let moves: Vec<(DynamicFieldOwner, DynamicFieldOwner)> = sorted
            .into_iter()
            .map(|(old, new)| {
                (
                    DynamicFieldOwner::Table(old.clone()),
                    DynamicFieldOwner::Table(new.clone()),
                )
            })
            .filter(|(old, _)| self.holds_owner(old))
            .collect();
        let contributors =
            self.owners_contributors(&moves.iter().map(|(old, _)| old.clone()).collect::<Vec<_>>());

        macro_rules! remap_owner_keyed {
            ($field:expr, |$slot:ident, $group:ident| $merge:block) => {{
                // Detached before any is re-filed: one literal's new range can
                // be another's old one.
                let detached: Vec<_> = moves
                    .iter()
                    .filter_map(|(old, new)| Some((new.clone(), $field.remove(old)?)))
                    .collect();
                for (new, group) in detached {
                    let $slot = $field.entry(new).or_default();
                    let $group = group;
                    $merge
                }
            }};
        }

        let remapped_owners: Vec<DynamicFieldOwner> = moves
            .iter()
            .flat_map(|(old, new)| [old.clone(), new.clone()])
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        for owner in &remapped_owners {
            self.note_whole_owner_write(owner);
        }

        remap_owner_keyed!(self.field_definitions, |slot, group| {
            merge_field_definitions(slot, group)
        });
        remap_owner_keyed!(self.direct_field_definitions, |slot, group| {
            merge_field_definitions(slot, group)
        });
        remap_owner_keyed!(self.finite_named_members, |slot, group| {
            slot.extend(group)
        });
        remap_owner_keyed!(self.wildcard_definitions, |slot, group| {
            merge_wildcard_definitions(slot, group)
        });
        for owner in &remapped_owners {
            self.recompute_owner_named(owner);
        }

        for site in self.sites_of(&contributors) {
            let Some(entries) = self.site_contributions.get_mut(&site) else {
                continue;
            };
            entries.remap_owners(|owner| remap_owner(owner, map));
        }
    }

    /// The write sites of `files`, sorted.
    fn sites_of(&self, files: &[FileId]) -> Vec<InFiled<TextRange>> {
        let mut sites = files
            .iter()
            .flat_map(|file_id| self.site_contributions.file_entries(*file_id))
            .map(|(site, _)| site.clone())
            .collect::<Vec<_>>();
        sites.sort_unstable_by_key(definition_sort_key);
        sites
    }

    /// Every table-literal range this index is keyed by.
    #[cfg(test)]
    pub(crate) fn table_ranges(&self) -> Vec<InFiled<TextRange>> {
        fn owner_range(owner: &DynamicFieldOwner) -> Option<InFiled<TextRange>> {
            match owner {
                DynamicFieldOwner::Table(range) => Some(range.clone()),
                DynamicFieldOwner::Type(_) | DynamicFieldOwner::GlobalPath(_) => None,
            }
        }
        let mut ranges: Vec<_> = self
            .field_definitions
            .keys()
            .chain(self.direct_field_definitions.keys())
            .chain(self.finite_named_members.keys())
            .chain(self.wildcard_definitions.keys())
            .filter_map(owner_range)
            .collect();
        for entries in self.site_contributions.values() {
            match entries {
                SiteContributions::Empty => {}
                SiteContributions::OneNamed(owner, _, _) => {
                    if let Some(range) = owner_range(owner) {
                        ranges.push(range);
                    }
                }
                SiteContributions::Many(data) => {
                    ranges.extend(
                        data.named
                            .iter()
                            .filter_map(|(owner, _, _)| owner_range(owner))
                            .chain(
                                data.wildcards
                                    .iter()
                                    .filter_map(|(owner, _)| owner_range(owner)),
                            ),
                    );
                }
            }
        }
        ranges
    }

    pub fn has_unattributed_field(&self, field_name: &str) -> bool {
        derivation::note_read(|| FactKey::UnattributedField(field_name.into()));
        self.unattributed_fields.contains_key(field_name)
    }

    /// The types with a direct definition of `field_name`, ordered by name.
    pub fn types_declaring_field(&self, field_name: &str) -> Vec<LuaTypeDeclId> {
        derivation::note_read(|| FactKey::DynamicFieldName(field_name.into()));
        let mut types = self
            .declaring_types
            .get(field_name)
            .map(|types| types.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        types.sort_by(|left, right| {
            left.get_name()
                .cmp(right.get_name())
                .then_with(|| left.stable_cmp(right))
        });
        types
    }

    pub fn has_field(&self, owner: &DynamicFieldOwner, field_name: &str) -> bool {
        derivation::note_dynamic_field_read(owner, Some(field_name));
        self.field_definitions
            .get(owner)
            .is_some_and(|fields| fields.contains_key(field_name))
    }

    pub fn has_field_in_file(
        &self,
        owner: &DynamicFieldOwner,
        field_name: &str,
        file_id: FileId,
    ) -> bool {
        derivation::note_dynamic_field_read(owner, Some(field_name));
        self.field_definitions
            .get(owner)
            .and_then(|fields| fields.get(field_name))
            .is_some_and(|defs| defs.iter().any(|def| def.file_id == file_id))
    }

    pub fn get_fields(&self, owner: &DynamicFieldOwner) -> Option<&HashMap<SmolStr, DefSites>> {
        derivation::note_dynamic_field_read(owner, None);
        self.field_definitions.get(owner)
    }

    /// Whether `owner` has a direct definition of `field_name`: read as that
    /// field alone, not the owner's whole field set.
    pub fn has_direct_field(&self, owner: &DynamicFieldOwner, field_name: &str) -> bool {
        derivation::note_dynamic_field_read(owner, Some(field_name));
        self.direct_field_definitions
            .get(owner)
            .is_some_and(|fields| fields.contains_key(field_name))
    }

    pub fn get_direct_fields(
        &self,
        owner: &DynamicFieldOwner,
    ) -> Option<&HashMap<SmolStr, DefSites>> {
        derivation::note_dynamic_field_read(owner, None);
        self.direct_field_definitions.get(owner)
    }

    /// Marks `member_id`, written by `site`, as named finitely on `owner`.
    /// Returns whether the mark was new to the index.
    pub fn add_finite_named_member(
        &mut self,
        site: &InFiled<TextRange>,
        owner: DynamicFieldOwner,
        member_id: LuaMemberId,
    ) -> bool {
        derivation::note_write(|| FactKey::DynamicFieldSite(site.clone()));
        let is_new = self
            .finite_named_members
            .entry(owner.clone())
            .or_default()
            .insert(member_id);
        if is_new {
            derivation::note_write(|| FactKey::DynamicFieldFinite(owner.clone()));
            self.site_record(site).push_finite(owner, member_id);
        }
        is_new
    }

    /// Withdraws the mark `site` filed for `member_id` on `owner`.
    pub fn remove_finite_named_member(
        &mut self,
        site: &InFiled<TextRange>,
        owner: &DynamicFieldOwner,
        member_id: LuaMemberId,
    ) {
        derivation::note_write(|| FactKey::DynamicFieldSite(site.clone()));
        let Some(members) = self.finite_named_members.get_mut(owner) else {
            return;
        };
        derivation::note_write(|| FactKey::DynamicFieldFinite(owner.clone()));
        members.remove(&member_id);
        if members.is_empty() {
            self.finite_named_members.remove(owner);
        }
        if let Some(contributions) = self.site_contributions.get_mut(site) {
            contributions.remove_finite(owner, &member_id);
            if contributions.is_empty() {
                self.site_contributions.remove(site);
            }
        }
    }

    pub fn member_has_finite_named_definition(
        &self,
        owner: &DynamicFieldOwner,
        member_id: LuaMemberId,
    ) -> bool {
        derivation::note_read(|| FactKey::DynamicFieldFinite(owner.clone()));
        self.finite_named_members
            .get(owner)
            .is_some_and(|members| members.contains(&member_id))
    }

    pub fn owner_has_finite_named_members(&self, owner: &DynamicFieldOwner) -> bool {
        derivation::note_read(|| FactKey::DynamicFieldFinite(owner.clone()));
        self.finite_named_members
            .get(owner)
            .is_some_and(|members| !members.is_empty())
    }

    pub fn get_fields_in_file(&self, owner: &DynamicFieldOwner, file_id: FileId) -> Vec<&SmolStr> {
        derivation::note_dynamic_field_read(owner, None);
        self.field_definitions
            .get(owner)
            .map(|fields| {
                fields
                    .iter()
                    .filter_map(|(name, defs)| {
                        defs.iter()
                            .any(|def| def.file_id == file_id)
                            .then_some(name)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every recorded definition of one field, in canonical order.
    pub fn field_definitions(
        &self,
        owner: &DynamicFieldOwner,
        field_name: &str,
    ) -> &[InFiled<TextRange>] {
        derivation::note_dynamic_field_read(owner, Some(field_name));
        self.field_definitions
            .get(owner)
            .and_then(|fields| fields.get(field_name))
            .map_or(&[], DefSites::as_slice)
    }

    /// The definitions of one field written on `owner` itself, in canonical
    /// order, leaving out those propagated to it from other owners.
    pub fn direct_definitions_of(
        &self,
        owner: &DynamicFieldOwner,
        field_name: &str,
    ) -> &[InFiled<TextRange>] {
        derivation::note_dynamic_field_read(owner, Some(field_name));
        self.direct_field_definitions
            .get(owner)
            .and_then(|fields| fields.get(field_name))
            .map_or(&[], DefSites::as_slice)
    }

    pub fn get_wildcard_definitions(&self, owner: &DynamicFieldOwner) -> Vec<InFiled<TextRange>> {
        derivation::note_read(|| FactKey::DynamicFieldWildcards(owner.clone()));
        self.wildcard_definitions
            .get(owner)
            .cloned()
            .unwrap_or_default()
    }

    pub fn has_wildcard_definitions(&self, owner: &DynamicFieldOwner) -> bool {
        derivation::note_read(|| FactKey::DynamicFieldWildcards(owner.clone()));
        self.wildcard_definitions
            .get(owner)
            .is_some_and(|definitions| !definitions.is_empty())
    }

    /// Every owner's field-definition sites, for whole-index snapshots.
    pub fn iter_all_field_definitions(
        &self,
    ) -> impl Iterator<Item = (&DynamicFieldOwner, &HashMap<SmolStr, DefSites>)> {
        self.field_definitions.iter()
    }

    /// Every owner's direct field-definition sites, for whole-index
    /// snapshots. Direct membership is a distinct fact from the union
    /// `iter_all_field_definitions` reports — a site whose write moved
    /// between the direct and propagated tiers leaves the union unchanged.
    pub fn iter_all_direct_field_definitions(
        &self,
    ) -> impl Iterator<Item = (&DynamicFieldOwner, &HashMap<SmolStr, DefSites>)> {
        self.direct_field_definitions.iter()
    }

    /// Every unattributed-field record, for whole-index snapshots.
    pub fn iter_all_unattributed_fields(
        &self,
    ) -> impl Iterator<Item = (&SmolStr, &HashSet<InFiled<TextRange>>)> {
        self.unattributed_fields.iter()
    }

    /// Every finite-named-member record, for whole-index snapshots.
    pub fn iter_all_finite_named_members(
        &self,
    ) -> impl Iterator<Item = (&DynamicFieldOwner, &HashSet<LuaMemberId>)> {
        self.finite_named_members.iter()
    }

    pub fn get_all_wildcard_definitions(&self) -> Vec<InFiled<TextRange>> {
        let mut definitions = self
            .wildcard_definitions
            .values()
            .flat_map(|definitions| definitions.iter().cloned())
            .collect::<Vec<_>>();
        definitions.sort_by_key(|definition| (definition.file_id, definition.value.start()));
        definitions.dedup();
        definitions
    }

    /// What a read of `DynamicField(owner, Some(field_name))` observes: the
    /// definitions and the direct definitions.
    fn named_slot_fingerprint(&self, owner: &DynamicFieldOwner, field_name: &str) -> Fingerprint {
        let definitions = self
            .field_definitions
            .get(owner)
            .and_then(|fields| fields.get(field_name));
        let direct = self
            .direct_field_definitions
            .get(owner)
            .and_then(|fields| fields.get(field_name));
        if definitions.is_none() && direct.is_none() {
            return derivation::ABSENT;
        }
        derivation::fingerprint_of(&(
            definitions.map(DefSites::as_slice),
            direct.map(DefSites::as_slice),
        ))
    }

    /// Notes a named slot's change since `old_slot` and folds it into the
    /// owner's multiset fingerprint.
    fn settle_named(&mut self, owner: &DynamicFieldOwner, field_name: &str, old_slot: Fingerprint) {
        let new_slot = self.named_slot_fingerprint(owner, field_name);
        if new_slot == old_slot {
            return;
        }
        derivation::note_write(|| FactKey::dynamic_field(owner.clone(), Some(field_name.into())));
        derivation::note_write(|| FactKey::dynamic_field(owner.clone(), None));
        let digest = self
            .owner_named_fingerprints
            .entry(owner.clone())
            .or_default();
        *digest = digest
            .wrapping_sub(named_entry(field_name, old_slot))
            .wrapping_add(named_entry(field_name, new_slot));
        if *digest == 0 {
            self.owner_named_fingerprints.remove(owner);
        }
    }

    fn owner_field_names(&self, owner: &DynamicFieldOwner) -> HashSet<SmolStr> {
        [&self.field_definitions, &self.direct_field_definitions]
            .into_iter()
            .filter_map(|map| map.get(owner))
            .flat_map(|fields| fields.keys().cloned())
            .collect()
    }

    /// Notes every key of `owner` ahead of a whole-owner rewrite.
    fn note_whole_owner_write(&self, owner: &DynamicFieldOwner) {
        if !derivation::recording() {
            return;
        }
        derivation::note_write(|| FactKey::dynamic_field(owner.clone(), None));
        derivation::note_write(|| FactKey::DynamicFieldWildcards(owner.clone()));
        derivation::note_write(|| FactKey::DynamicFieldFinite(owner.clone()));
        for field_name in self.owner_field_names(owner) {
            derivation::note_write(|| {
                FactKey::dynamic_field(owner.clone(), Some(field_name.into()))
            });
        }
    }

    /// `owner`'s named slots summed afresh, which is what the incremental
    /// `owner_named_fingerprints` sum holds.
    fn owner_named_digest(&self, owner: &DynamicFieldOwner) -> Fingerprint {
        self.owner_field_names(owner)
            .iter()
            .map(|field_name| {
                named_entry(field_name, self.named_slot_fingerprint(owner, field_name))
            })
            .fold(0u64, u64::wrapping_add)
    }

    /// The names of `owner` whose slot `fork`, a task fork of `frozen`,
    /// changed.
    fn changed_names<'a>(
        frozen: &'a Self,
        fork: &'a Self,
        owner: &DynamicFieldOwner,
        names: &mut HashSet<&'a SmolStr>,
    ) {
        fn changed<'a, V: PartialEq>(
            frozen: Option<&'a HashMap<SmolStr, V>>,
            fork: Option<&'a HashMap<SmolStr, V>>,
            names: &mut HashSet<&'a SmolStr>,
        ) {
            let value = |map: Option<&'a HashMap<SmolStr, V>>, name: &SmolStr| {
                map.and_then(|map| map.get(name))
            };
            for name in frozen.into_iter().chain(fork).flat_map(HashMap::keys) {
                if value(frozen, name) != value(fork, name) {
                    names.insert(name);
                }
            }
        }
        changed(
            frozen.field_definitions.get(owner),
            fork.field_definitions.get(owner),
            names,
        );
        changed(
            frozen.direct_field_definitions.get(owner),
            fork.direct_field_definitions.get(owner),
            names,
        );
    }

    /// `owner`'s fingerprint in `self` with the slot fingerprints of `names`,
    /// for [`Self::rebuild_owner_named`].
    fn owner_slots(
        &self,
        owner: DynamicFieldOwner,
        digest: Fingerprint,
        names: HashSet<&SmolStr>,
    ) -> OwnerSlotsToRebuild {
        let slots = names
            .into_iter()
            .map(|name| (name.clone(), self.named_slot_fingerprint(&owner, name)))
            .collect();
        OwnerSlotsToRebuild {
            owner,
            digest,
            slots,
        }
    }

    /// The owners whose named fingerprint both `self` and `fork`, a task fork
    /// of `frozen`, changed, each with the names the fork changed: the merge
    /// changes only those slots.
    fn owners_to_rebuild(&self, frozen: &Self, fork: &Self) -> Vec<OwnerSlotsToRebuild> {
        self.owner_named_fingerprints
            .both_changed_keys(
                &frozen.owner_named_fingerprints,
                &fork.owner_named_fingerprints,
            )
            .into_iter()
            .map(|owner| {
                let mut names = HashSet::default();
                Self::changed_names(frozen, fork, &owner, &mut names);
                let digest = self
                    .owner_named_fingerprints
                    .get(&owner)
                    .copied()
                    .unwrap_or_default();
                self.owner_slots(owner, digest, names)
            })
            .collect()
    }

    /// Installs the named fingerprints of a superstep's `forks` in fork
    /// order, leaving each fork the frozen ones, and returns each owner whose
    /// fingerprint any fork changed where the index had too, with its
    /// fingerprint and the slots of every name a fork changed before the
    /// first fork: those are the only slots the forks change, so folding
    /// them in once every fork is in gives the owner's merged fingerprint.
    fn install_owner_named(
        &mut self,
        frozen: &Self,
        forks: &mut [(usize, Self)],
    ) -> Vec<OwnerSlotsToRebuild> {
        let mut changed_by: HashMap<DynamicFieldOwner, (Fingerprint, Vec<usize>)> =
            HashMap::default();
        let mut rebuilt = Vec::new();
        for (at, (_, fork)) in forks.iter_mut().enumerate() {
            for owner in CowShards::changed_keys(
                &frozen.owner_named_fingerprints,
                &fork.owner_named_fingerprints,
            ) {
                let digest = self.owner_named_fingerprints.get(&owner).copied();
                changed_by
                    .entry(owner)
                    .or_insert_with(|| (digest.unwrap_or_default(), Vec::new()))
                    .1
                    .push(at);
            }
            rebuilt.extend(self.owner_named_fingerprints.both_changed_keys(
                &frozen.owner_named_fingerprints,
                &fork.owner_named_fingerprints,
            ));
            let forked = std::mem::replace(
                &mut fork.owner_named_fingerprints,
                frozen.owner_named_fingerprints.clone(),
            );
            let conflicted = crate::db_index::cow_shards::MergeFork::install_merging(
                &mut self.owner_named_fingerprints,
                &frozen.owner_named_fingerprints,
                forked,
                crate::db_index::cow_shards::rebuilt,
            );
            debug_assert!(!conflicted, "a rebuilt fingerprint never conflicts");
        }
        rebuilt
            .into_iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .map(|owner| {
                let (digest, changers) = &changed_by[&owner];
                let mut names = HashSet::default();
                for at in changers {
                    Self::changed_names(frozen, &forks[*at].1, &owner, &mut names);
                }
                self.owner_slots(owner.clone(), *digest, names)
            })
            .collect()
    }

    /// Sets each owner's fingerprint to its fingerprint before the install
    /// with the merged slots folded in, in place of those it held before.
    fn rebuild_owner_named(&mut self, owners: Vec<OwnerSlotsToRebuild>) {
        for OwnerSlotsToRebuild {
            owner,
            digest,
            slots,
        } in owners
        {
            let digest = slots.iter().fold(digest, |digest, (name, old_slot)| {
                let new_slot = self.named_slot_fingerprint(&owner, name);
                digest
                    .wrapping_sub(named_entry(name, *old_slot))
                    .wrapping_add(named_entry(name, new_slot))
            });
            if digest == 0 {
                self.owner_named_fingerprints.remove(&owner);
            } else {
                self.owner_named_fingerprints.insert(owner, digest);
            }
        }
    }

    /// Rebuilds `owner`'s multiset fingerprint after a whole-owner rewrite.
    fn recompute_owner_named(&mut self, owner: &DynamicFieldOwner) {
        let digest = self.owner_named_digest(owner);
        if digest == 0 {
            self.owner_named_fingerprints.remove(owner);
        } else {
            self.owner_named_fingerprints.insert(owner.clone(), digest);
        }
    }

    /// `DynamicField(owner, None)` covers the owner-wide reads: wildcard
    /// definitions, finite-named members and every named slot.
    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::DynamicFieldSite(site) => {
                derivation::fingerprint_opt(self.site_contributions.get(site))
            }
            FactKey::DynamicField(boxed) => match (&boxed.0, boxed.1.as_deref()) {
                (owner, Some(field_name)) => self.named_slot_fingerprint(owner, field_name),
                (owner, None) => {
                    let named = if derivation::mapping_back() {
                        Some(self.owner_named_digest(owner)).filter(|digest| *digest != 0)
                    } else {
                        self.owner_named_fingerprints.get(owner).copied()
                    };
                    derivation::fingerprint_opt(named.as_ref())
                }
            },
            FactKey::DynamicFieldWildcards(owner) => {
                derivation::fingerprint_opt(self.wildcard_definitions.get(owner))
            }
            FactKey::DynamicFieldFinite(owner) => {
                self.finite_named_members
                    .get(owner)
                    .map_or(derivation::ABSENT, |members| {
                        derivation::fingerprint_of(
                            &members
                                .iter()
                                .map(derivation::fingerprint_of)
                                .fold(0u64, u64::wrapping_add),
                        )
                    })
            }
            FactKey::DynamicFieldName(field_name) => self
                .declaring_types
                .get(field_name.as_str())
                .map_or(derivation::ABSENT, |types| {
                    derivation::fingerprint_of(
                        &types
                            .keys()
                            .map(derivation::fingerprint_of)
                            .fold(0u64, u64::wrapping_add),
                    )
                }),
            FactKey::UnattributedField(field_name) => {
                if self.unattributed_fields.contains_key(field_name.as_str()) {
                    derivation::fingerprint_of(&true)
                } else {
                    derivation::ABSENT
                }
            }
            _ => {
                debug_assert!(false, "{key:?} is not a dynamic-field fact");
                derivation::ABSENT
            }
        }
    }
}

/// Drops one definition entry filed under `(owner, field_name)`, pruning the
/// emptied levels so no owner or field outlives its last definition.
/// Drops `definition` from `owner`'s `field_name`. Returns whether it was
/// there.
fn remove_field_definition(
    definitions: &mut CowShards<DynamicFieldOwner, HashMap<SmolStr, DefSites>>,
    owner: &DynamicFieldOwner,
    field_name: &str,
    definition: &InFiled<TextRange>,
) -> bool {
    let Some(fields) = definitions.get_mut(owner) else {
        return false;
    };
    let Some(entries) = fields.get_mut(field_name) else {
        return false;
    };
    let removed = entries.remove(definition);
    if entries.is_empty() {
        fields.remove(field_name);
    }
    if fields.is_empty() {
        definitions.remove(owner);
    }
    removed
}

#[cfg(test)]
fn normalize_field_definitions(
    definitions: &CowShards<DynamicFieldOwner, HashMap<SmolStr, DefSites>>,
) -> HashMap<DynamicFieldOwner, HashMap<SmolStr, HashMap<InFiled<TextRange>, usize>>> {
    definitions
        .iter()
        .map(|(owner, fields)| {
            let normalized_fields = fields
                .iter()
                .map(|(field_name, definitions)| {
                    let definition_counts = definitions.iter().cloned().fold(
                        HashMap::<InFiled<TextRange>, usize>::default(),
                        |mut counts, definition| {
                            *counts.entry(definition).or_default() += 1;
                            counts
                        },
                    );
                    (field_name.clone(), definition_counts)
                })
                .collect();
            (owner.clone(), normalized_fields)
        })
        .collect()
}

impl LuaIndex for DynamicFieldIndex {
    fn remove(&mut self, file_id: FileId) {
        self.remove_files(&[file_id]);
    }

    fn remove_files(&mut self, file_ids: &[FileId]) {
        // Every primary map is pruned through the write sites' records: only
        // the owners the removed files' sites contributed to are visited,
        // never the whole index. Each contribution corresponds to exactly one
        // entry, so retracting a file's sites removes precisely its entries;
        // the surviving entries keep their canonical order.
        for site in self.sites_of(file_ids) {
            self.retract_site(&site);
        }
    }

    fn clear(&mut self) {
        if derivation::recording() {
            let owners: HashSet<&DynamicFieldOwner> = self
                .field_definitions
                .keys()
                .chain(self.direct_field_definitions.keys())
                .chain(self.finite_named_members.keys())
                .chain(self.wildcard_definitions.keys())
                .collect();
            for owner in owners {
                self.note_whole_owner_write(owner);
            }
            for field_name in self.unattributed_fields.keys() {
                derivation::note_write(|| FactKey::UnattributedField(field_name.as_str().into()));
            }
            for field_name in self.declaring_types.keys() {
                derivation::note_write(|| FactKey::DynamicFieldName(field_name.as_str().into()));
            }
        }
        self.owner_named_fingerprints.clear();
        self.field_definitions.clear();
        self.direct_field_definitions.clear();
        self.finite_named_members.clear();
        self.wildcard_definitions.clear();
        self.unattributed_fields.clear();
        self.declaring_types.clear();
        self.site_contributions.clear();
    }
}

#[cfg(test)]
mod tests {
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};
    use smol_str::SmolStr;

    use super::*;
    use crate::{InFiled, LuaTypeDeclId};

    fn range(start: u32, end: u32) -> TextRange {
        TextRange::new(TextSize::from(start), TextSize::from(end))
    }

    /// The site a finite-member mark is filed by in these tests: its member.
    fn member_site(member_id: LuaMemberId) -> InFiled<TextRange> {
        InFiled::new(member_id.file_id, member_id.get_syntax_id().get_range())
    }

    /// The sites `file_id`'s writes are filed under.
    fn file_sites(index: &DynamicFieldIndex, file_id: FileId) -> Vec<InFiled<TextRange>> {
        index.sites_of(&[file_id])
    }

    fn shift(
        file_id: FileId,
        from: TextRange,
        to: TextRange,
    ) -> rustc_hash::FxHashMap<InFiled<TextRange>, InFiled<TextRange>> {
        let mut map = rustc_hash::FxHashMap::default();
        map.insert(InFiled::new(file_id, from), InFiled::new(file_id, to));
        map
    }

    /// The installed state a batch install has to reproduce, in a stable
    /// order, with each owner's fingerprint checked against its slots.
    fn installed_state(index: &DynamicFieldIndex) -> Vec<String> {
        let mut state = index
            .owner_named_fingerprints
            .iter()
            .map(|(owner, digest)| {
                assert_eq!(*digest, index.owner_named_digest(owner));
                format!("digest {owner:?} {digest}")
            })
            .collect::<Vec<_>>();
        state.extend(index.field_definitions.iter().flat_map(|(owner, fields)| {
            fields.iter().map(move |(name, definitions)| {
                format!("definitions {owner:?} {name} {definitions:?}")
            })
        }));
        state.sort();
        state
    }

    #[test]
    fn installing_forks_field_by_field_matches_installing_them_one_by_one() {
        use crate::db_index::cow_shards::TaskFork;
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("Player"));
        let mut base = DynamicFieldIndex::new();
        base.add_field(
            &InFiled::new(FileId::new(1), range(1, 2)),
            owner.clone(),
            SmolStr::new("a"),
            range(1, 2),
        );
        let frozen = base.task_fork();
        let mut retracting = frozen.task_fork();
        retracting.retract_site(&InFiled::new(FileId::new(1), range(1, 2)));
        let mut forks = vec![retracting];
        for file in 2..6 {
            let mut fork = frozen.task_fork();
            let name = if file % 2 == 0 { "a" } else { "b" };
            fork.add_field(
                &InFiled::new(FileId::new(file), range(file * 10, file * 10 + 1)),
                owner.clone(),
                SmolStr::new(name),
                range(file * 10, file * 10 + 1),
            );
            forks.push(fork);
        }
        let mut one_by_one = base.clone();
        let mut one_by_one_conflicts = vec![Vec::new(); forks.len()];
        for (at, fork) in forks.iter().cloned().enumerate() {
            one_by_one.install_task_reporting(&frozen, fork, &mut one_by_one_conflicts[at]);
        }
        let mut by_field = base.clone();
        let mut by_field_conflicts = vec![Vec::new(); forks.len()];
        by_field.install_tasks_reporting(
            &frozen,
            forks.into_iter().enumerate().collect(),
            &mut by_field_conflicts,
        );

        assert_eq!(
            (installed_state(&by_field), by_field_conflicts),
            (installed_state(&one_by_one), one_by_one_conflicts)
        );
    }

    #[test]
    fn tasks_retracting_a_types_last_two_definitions_leave_it_declaring_nothing() {
        use crate::db_index::cow_shards::TaskFork;
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("Player"));
        let sites = [1, 2].map(|file| InFiled::new(FileId::new(file), range(file, file + 1)));
        let mut base = DynamicFieldIndex::new();
        for site in &sites {
            base.add_field(site, owner.clone(), SmolStr::new("a"), site.value);
        }
        let frozen = base.task_fork();
        let forks = sites
            .iter()
            .map(|site| {
                let mut fork = frozen.task_fork();
                fork.retract_site(site);
                fork
            })
            .enumerate()
            .collect::<Vec<_>>();
        let mut index = base.clone();
        let mut conflicts = vec![Vec::new(); forks.len()];

        index.install_tasks_reporting(&frozen, forks, &mut conflicts);

        assert_eq!(
            index.types_declaring_field("a"),
            Vec::<LuaTypeDeclId>::new()
        );
    }

    /// Every owner-keyed store has to move together. `owner_fields` backs
    /// `has_field`, so leaving it behind strands the field on a range no type
    /// resolves to any more.
    #[test]
    fn remapping_a_literal_moves_the_field_lookup_with_it() {
        let edited = FileId::new(1);
        let contributor = FileId::new(2);
        let old = DynamicFieldOwner::Table(InFiled::new(edited, range(0, 10)));
        let new = DynamicFieldOwner::Table(InFiled::new(edited, range(20, 30)));

        let mut index = DynamicFieldIndex::new();
        index.add_field(
            &InFiled::new(contributor, range(1, 2)),
            old.clone(),
            SmolStr::new("f"),
            range(1, 2),
        );
        index.remap_table_ranges(&shift(edited, range(0, 10), range(20, 30)));

        assert!(index.has_field(&new, "f"));
        assert!(!index.has_field(&old, "f"));
        assert_eq!(index.field_definitions(&new, "f").len(), 1);
    }

    /// Only anchored literals are remapped, so an unanchored one's cross-file
    /// entry can already sit on the range an anchored one moves onto. Merging
    /// has to keep both, per field name.
    #[test]
    fn remapping_onto_an_occupied_range_keeps_both_owners_definitions() {
        let edited = FileId::new(1);
        let contributor = FileId::new(2);
        let moved_from = DynamicFieldOwner::Table(InFiled::new(edited, range(0, 10)));
        let occupied = DynamicFieldOwner::Table(InFiled::new(edited, range(20, 30)));

        let mut index = DynamicFieldIndex::new();
        index.add_field(
            &InFiled::new(contributor, range(1, 2)),
            moved_from.clone(),
            SmolStr::new("shared"),
            range(1, 2),
        );
        index.add_field(
            &InFiled::new(contributor, range(3, 4)),
            occupied.clone(),
            SmolStr::new("shared"),
            range(3, 4),
        );
        index.remap_table_ranges(&shift(edited, range(0, 10), range(20, 30)));

        assert_eq!(
            index.field_definitions(&occupied, "shared"),
            [
                InFiled::new(contributor, range(1, 2)),
                InFiled::new(contributor, range(3, 4)),
            ]
        );
    }

    /// A literal the edit destroyed takes the fields other files filed on it
    /// with it, and those files are analysed again; a moved one keeps its
    /// contributors' entries under its new range.
    #[test]
    fn a_destroyed_literal_drops_only_its_contributors_entries() {
        let (edited, contributor, bystander) = (FileId::new(1), FileId::new(2), FileId::new(3));
        let lost = DynamicFieldOwner::Table(InFiled::new(edited, range(2, 6)));
        let kept = DynamicFieldOwner::Table(InFiled::new(edited, range(20, 30)));
        let moved = DynamicFieldOwner::Table(InFiled::new(edited, range(16, 26)));
        let mut index = DynamicFieldIndex::new();
        index.add_field(
            &InFiled::new(contributor, range(1, 2)),
            lost.clone(),
            SmolStr::new("f"),
            range(1, 2),
        );
        index.add_wildcard_definition(
            &InFiled::new(contributor, range(3, 4)),
            lost.clone(),
            range(3, 4),
        );
        index.add_field(
            &InFiled::new(bystander, range(5, 6)),
            kept.clone(),
            SmolStr::new("g"),
            range(5, 6),
        );

        let dirty = index.remap_file_table_owners(&crate::FileRemap::unvalidated(
            edited,
            crate::PositionMap::new("abcdefgh", "abXh"),
        ));

        assert_eq!(
            dirty,
            [range(1, 2), range(3, 4)].map(|site| crate::LostFact::new(
                contributor,
                FactKey::DynamicFieldSite(InFiled::new(contributor, site))
            ))
        );
        assert!(!index.has_field(&lost, "f"));
        assert!(file_sites(&index, contributor).is_empty());
        assert!(index.has_field(&moved, "g"));
        assert_eq!(
            index
                .site_contributions
                .get(&InFiled::new(bystander, range(5, 6)))
                .and_then(|entries| match entries {
                    SiteContributions::OneNamed(owner, _, _) => Some(owner),
                    SiteContributions::Many(data) => data.named.first().map(|(owner, _, _)| owner),
                    SiteContributions::Empty => None,
                }),
            Some(&moved)
        );
    }

    /// A site whose only contribution to a destroyed literal is a finite mark
    /// is analysed again like any other.
    #[test]
    fn a_destroyed_literal_reports_a_site_that_only_marked_it_finite() {
        let (edited, contributor) = (FileId::new(1), FileId::new(2));
        let lost = DynamicFieldOwner::Table(InFiled::new(edited, range(2, 6)));
        let site = InFiled::new(contributor, range(1, 2));
        let member_id = LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range(1, 2)),
            contributor,
        );
        let mut index = DynamicFieldIndex::new();
        index.add_finite_named_member(&site, lost.clone(), member_id);

        let dirty = index.remap_file_table_owners(&crate::FileRemap::unvalidated(
            edited,
            crate::PositionMap::new("abcdefgh", "abXh"),
        ));

        assert_eq!(
            dirty,
            [crate::LostFact::new(
                contributor,
                FactKey::DynamicFieldSite(site)
            )]
        );
    }

    /// Removal is driven by the per-file contribution indexes, so state is
    /// built through the same `add_*` entry points production uses.
    #[test]
    fn remove_prunes_definitions_leaving_no_orphaned_entries() {
        let file_to_remove = FileId::new(1);
        let remaining_file = FileId::new(2);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let field = SmolStr::new("value");

        let mut index = DynamicFieldIndex::new();
        index.add_field(
            &InFiled::new(file_to_remove, range(1, 2)),
            owner.clone(),
            field.clone(),
            range(1, 2),
        );
        index.add_field(
            &InFiled::new(remaining_file, range(3, 4)),
            owner.clone(),
            field.clone(),
            range(3, 4),
        );
        index.add_wildcard_definition(
            &InFiled::new(file_to_remove, range(5, 6)),
            owner.clone(),
            range(5, 6),
        );
        index.add_wildcard_definition(
            &InFiled::new(remaining_file, range(7, 8)),
            owner.clone(),
            range(7, 8),
        );

        index.remove(file_to_remove);

        assert_eq!(index.field_definitions(&owner, &field).len(), 1);
        assert_eq!(
            index.field_definitions(&owner, &field)[0].file_id,
            remaining_file
        );
        assert_eq!(index.get_wildcard_definitions(&owner).len(), 1);
        assert_eq!(
            index.get_wildcard_definitions(&owner)[0].file_id,
            remaining_file
        );
        assert_eq!(
            index.get_fields_in_file(&owner, file_to_remove),
            Vec::<&SmolStr>::new()
        );
        assert_eq!(
            index.get_fields_in_file(&owner, remaining_file),
            vec![&field]
        );
        assert!(file_sites(&index, file_to_remove).is_empty());
        assert_eq!(file_sites(&index, remaining_file).len(), 2);
    }

    #[test]
    fn remove_keeps_other_file_field_then_prunes_last_file() {
        let file_a = FileId::new(1);
        let file_b = FileId::new(2);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let field = SmolStr::new("value");

        let mut index = DynamicFieldIndex::new();
        index.add_field(
            &InFiled::new(file_a, range(1, 2)),
            owner.clone(),
            field.clone(),
            range(1, 2),
        );
        index.add_field(
            &InFiled::new(file_b, range(3, 4)),
            owner.clone(),
            field.clone(),
            range(3, 4),
        );

        index.remove(file_a);

        assert!(index.has_field(&owner, &field));
        assert_eq!(
            index.get_fields_in_file(&owner, file_a),
            Vec::<&SmolStr>::new()
        );
        assert_eq!(index.get_fields_in_file(&owner, file_b), vec![&field]);

        index.remove(file_b);

        assert!(!index.has_field(&owner, &field));
        assert!(index.get_fields(&owner).is_none());
        assert!(index.field_definitions(&owner, &field).is_empty());
    }

    #[test]
    fn field_definitions_are_canonically_ordered_regardless_of_insertion_order() {
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let field = SmolStr::new("value");
        let inserts = [
            (FileId::new(2), range(5, 6)),
            (FileId::new(1), range(9, 10)),
            (FileId::new(1), range(3, 4)),
        ];

        let mut forward = DynamicFieldIndex::new();
        for (file_id, range) in inserts {
            forward.add_field(
                &InFiled::new(file_id, range),
                owner.clone(),
                field.clone(),
                range,
            );
        }
        let mut reverse = DynamicFieldIndex::new();
        for (file_id, range) in inserts.into_iter().rev() {
            reverse.add_field(
                &InFiled::new(file_id, range),
                owner.clone(),
                field.clone(),
                range,
            );
        }

        assert_eq!(
            forward.field_definitions(&owner, &field),
            vec![
                InFiled::new(FileId::new(1), range(3, 4)),
                InFiled::new(FileId::new(1), range(9, 10)),
                InFiled::new(FileId::new(2), range(5, 6)),
            ]
        );
        assert_eq!(
            forward.field_definitions(&owner, &field),
            reverse.field_definitions(&owner, &field)
        );
    }

    #[test]
    fn keyed_file_lookup_only_matches_contributing_files() {
        let file_a = FileId::new(1);
        let file_b = FileId::new(2);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let field = SmolStr::new("value");
        let mut index = DynamicFieldIndex::new();
        index.add_field(
            &InFiled::new(file_a, range(1, 2)),
            owner.clone(),
            field.clone(),
            range(1, 2),
        );

        assert!(index.has_field(&owner, &field));
        assert!(index.has_field_in_file(&owner, &field, file_a));
        assert!(!index.has_field_in_file(&owner, &field, file_b));
        assert!(!index.has_field_in_file(&owner, "missing", file_a));
    }

    #[test]
    fn finite_named_member_lookups_track_lifecycle() {
        let file_id = FileId::new(1);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let mut index = DynamicFieldIndex::new();
        let direct_member = LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range(0, 2)),
            file_id,
        );
        let wildcard_member = LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range(2, 4)),
            file_id,
        );
        let containing_member = LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range(0, 4)),
            file_id,
        );
        index.add_finite_named_member(&member_site(direct_member), owner.clone(), direct_member);

        assert!(index.member_has_finite_named_definition(&owner, direct_member));
        assert!(!index.member_has_finite_named_definition(&owner, wildcard_member));
        assert!(!index.member_has_finite_named_definition(&owner, containing_member));

        index.remove_finite_named_member(&member_site(direct_member), &owner, direct_member);
        assert!(!index.member_has_finite_named_definition(&owner, direct_member));
        index.add_finite_named_member(&member_site(direct_member), owner.clone(), direct_member);

        index.remove(file_id);
        assert!(!index.member_has_finite_named_definition(&owner, direct_member));

        index.add_finite_named_member(&member_site(direct_member), owner.clone(), direct_member);
        index.clear();
        assert!(!index.member_has_finite_named_definition(&owner, direct_member));
    }

    #[test]
    fn remove_tolerates_same_file_multiple_ranges_for_same_field() {
        let file_id = FileId::new(1);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let field = SmolStr::new("value");

        let mut index = DynamicFieldIndex::new();
        index.add_field(
            &InFiled::new(file_id, range(1, 2)),
            owner.clone(),
            field.clone(),
            range(1, 2),
        );
        index.add_field(
            &InFiled::new(file_id, range(3, 4)),
            owner.clone(),
            field.clone(),
            range(3, 4),
        );

        assert_eq!(file_sites(&index, file_id).len(), 2);

        index.remove(file_id);

        assert!(!index.has_field(&owner, &field));
        assert!(index.get_fields(&owner).is_none());
        assert!(file_sites(&index, file_id).is_empty());
    }

    #[test]
    fn remove_prunes_wildcard_contributions_for_type_and_table_owners() {
        let file_to_remove = FileId::new(1);
        let remaining_file = FileId::new(2);
        let type_owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("TypeOwner"));
        let table_owner = DynamicFieldOwner::Table(InFiled::new(file_to_remove, range(20, 30)));

        let mut index = DynamicFieldIndex::new();
        index.add_wildcard_definition(
            &InFiled::new(file_to_remove, range(1, 2)),
            type_owner.clone(),
            range(1, 2),
        );
        index.add_wildcard_definition(
            &InFiled::new(remaining_file, range(3, 4)),
            type_owner.clone(),
            range(3, 4),
        );
        index.add_wildcard_definition(
            &InFiled::new(file_to_remove, range(5, 6)),
            table_owner.clone(),
            range(5, 6),
        );

        index.remove(file_to_remove);

        assert_eq!(
            index.get_wildcard_definitions(&type_owner),
            vec![InFiled::new(remaining_file, range(3, 4))]
        );
        assert!(index.get_wildcard_definitions(&table_owner).is_empty());
        assert!(file_sites(&index, file_to_remove).is_empty());
        assert_eq!(file_sites(&index, remaining_file).len(), 1);
    }

    #[test]
    fn remove_missing_file_is_no_op() {
        let existing_file = FileId::new(1);
        let missing_file = FileId::new(99);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let field = SmolStr::new("value");

        let mut index = DynamicFieldIndex::new();
        index.add_field(
            &InFiled::new(existing_file, range(1, 2)),
            owner.clone(),
            field.clone(),
            range(1, 2),
        );
        index.add_wildcard_definition(
            &InFiled::new(existing_file, range(3, 4)),
            owner.clone(),
            range(3, 4),
        );

        let expected_field_definitions = index.field_definitions.clone();
        let expected_wildcard_definitions = index.wildcard_definitions.clone();
        let expected_site_contributions = index.site_contributions.clone();

        index.remove(missing_file);

        assert_eq!(index.field_definitions, expected_field_definitions);
        assert_eq!(index.wildcard_definitions, expected_wildcard_definitions);
        assert_eq!(index.site_contributions, expected_site_contributions);
    }

    #[test]
    fn remove_then_readd_matches_fresh_state() {
        let removed_file = FileId::new(1);
        let remaining_file = FileId::new(2);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let table_owner = DynamicFieldOwner::Table(InFiled::new(removed_file, range(30, 40)));
        let field = SmolStr::new("value");

        let mut index = DynamicFieldIndex::new();
        index.add_field(
            &InFiled::new(removed_file, range(1, 2)),
            owner.clone(),
            field.clone(),
            range(1, 2),
        );
        index.add_field(
            &InFiled::new(remaining_file, range(3, 4)),
            owner.clone(),
            field.clone(),
            range(3, 4),
        );
        index.add_wildcard_definition(
            &InFiled::new(removed_file, range(5, 6)),
            owner.clone(),
            range(5, 6),
        );
        index.add_wildcard_definition(
            &InFiled::new(removed_file, range(7, 8)),
            table_owner.clone(),
            range(7, 8),
        );

        index.remove(removed_file);
        index.add_field(
            &InFiled::new(removed_file, range(1, 2)),
            owner.clone(),
            field.clone(),
            range(1, 2),
        );
        index.add_wildcard_definition(
            &InFiled::new(removed_file, range(5, 6)),
            owner.clone(),
            range(5, 6),
        );
        index.add_wildcard_definition(
            &InFiled::new(removed_file, range(7, 8)),
            table_owner.clone(),
            range(7, 8),
        );

        let mut fresh = DynamicFieldIndex::new();
        fresh.add_field(
            &InFiled::new(removed_file, range(1, 2)),
            owner.clone(),
            field.clone(),
            range(1, 2),
        );
        fresh.add_field(
            &InFiled::new(remaining_file, range(3, 4)),
            owner.clone(),
            field.clone(),
            range(3, 4),
        );
        fresh.add_wildcard_definition(
            &InFiled::new(removed_file, range(5, 6)),
            owner.clone(),
            range(5, 6),
        );
        fresh.add_wildcard_definition(
            &InFiled::new(removed_file, range(7, 8)),
            table_owner.clone(),
            range(7, 8),
        );

        assert_eq!(
            normalize_field_definitions(&index.field_definitions),
            normalize_field_definitions(&fresh.field_definitions)
        );
        assert_eq!(index.wildcard_definitions, fresh.wildcard_definitions);
        assert_eq!(index.site_contributions, fresh.site_contributions);
    }

    /// Contributions-driven removal leaves exactly the state a fresh index
    /// built from the surviving files' inputs holds, finite-named members
    /// included.
    #[test]
    fn remove_matches_fresh_state_including_finite_members() {
        use glua_parser::LuaSyntaxKind;

        fn member(file_id: FileId, start: u32, end: u32) -> LuaMemberId {
            LuaMemberId::new(
                LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range(start, end)),
                file_id,
            )
        }

        let removed_file = FileId::new(1);
        let remaining_file = FileId::new(2);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("DynFieldTest"));
        let field = SmolStr::new("value");
        let removed_member = member(removed_file, 1, 2);
        let remaining_member = member(remaining_file, 3, 4);

        fn build(
            removed_file: FileId,
            remaining_file: FileId,
            removed_member: LuaMemberId,
            remaining_member: LuaMemberId,
            owner: &DynamicFieldOwner,
            field: &SmolStr,
            files: &[FileId],
        ) -> DynamicFieldIndex {
            let mut index = DynamicFieldIndex::new();
            if files.contains(&removed_file) {
                index.add_field(
                    &InFiled::new(removed_file, range(1, 2)),
                    owner.clone(),
                    field.clone(),
                    range(1, 2),
                );
                index.add_wildcard_definition(
                    &InFiled::new(removed_file, range(5, 6)),
                    owner.clone(),
                    range(5, 6),
                );
                index.add_finite_named_member(
                    &member_site(removed_member),
                    owner.clone(),
                    removed_member,
                );
                index.add_unattributed_field(
                    &InFiled::new(removed_file, range(9, 10)),
                    SmolStr::new("loose"),
                    range(9, 10),
                );
            }
            if files.contains(&remaining_file) {
                index.add_field(
                    &InFiled::new(remaining_file, range(3, 4)),
                    owner.clone(),
                    field.clone(),
                    range(3, 4),
                );
                index.add_wildcard_definition(
                    &InFiled::new(remaining_file, range(7, 8)),
                    owner.clone(),
                    range(7, 8),
                );
                index.add_finite_named_member(
                    &member_site(remaining_member),
                    owner.clone(),
                    remaining_member,
                );
                index.add_unattributed_field(
                    &InFiled::new(remaining_file, range(11, 12)),
                    SmolStr::new("loose"),
                    range(11, 12),
                );
            }
            index
        }

        let mut index = build(
            removed_file,
            remaining_file,
            removed_member,
            remaining_member,
            &owner,
            &field,
            &[removed_file, remaining_file],
        );
        index.remove(removed_file);
        let fresh = build(
            removed_file,
            remaining_file,
            removed_member,
            remaining_member,
            &owner,
            &field,
            &[remaining_file],
        );

        assert_eq!(
            normalize_field_definitions(&index.field_definitions),
            normalize_field_definitions(&fresh.field_definitions)
        );
        assert_eq!(index.wildcard_definitions, fresh.wildcard_definitions);
        assert_eq!(index.site_contributions, fresh.site_contributions);
        assert_eq!(index.finite_named_members, fresh.finite_named_members);
        assert_eq!(index.unattributed_fields, fresh.unattributed_fields);
    }

    /// Retracting a site withdraws what it filed alone: a sibling site
    /// writing the same field onto the same owner from another key keeps its
    /// definition, and the file keeps its slot through it.
    #[test]
    fn retracting_a_site_withdraws_only_its_own_definitions() {
        let file_id = FileId::new(1);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("OwnerA"));
        let super_owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("Super"));
        let site = InFiled::new(file_id, range(0, 10));
        let sibling = InFiled::new(file_id, range(20, 30));

        let mut index = DynamicFieldIndex::new();
        index.add_field(&site, owner.clone(), SmolStr::new("f"), range(1, 2));
        index.add_field(&site, owner.clone(), SmolStr::new("g"), range(3, 4));
        index.add_propagated_field(&site, super_owner.clone(), SmolStr::new("g"), range(3, 4));
        index.add_wildcard_definition(&site, owner.clone(), range(5, 6));
        index.add_field(&sibling, owner.clone(), SmolStr::new("f"), range(21, 22));

        index.retract_site(&site);

        assert_eq!(
            index.field_definitions(&owner, "f"),
            [InFiled::new(file_id, range(21, 22))]
        );
        assert!(!index.has_field(&owner, "g"));
        assert!(!index.has_field(&super_owner, "g"));
        assert!(index.get_wildcard_definitions(&owner).is_empty());
        assert_eq!(
            index.get_fields_in_file(&owner, file_id),
            vec![&SmolStr::new("f")]
        );
        assert_eq!(file_sites(&index, file_id), [sibling]);
    }

    /// A site filed again after its retraction leaves the state a build that
    /// filed every site once has, whatever order the sites were filed in.
    #[test]
    fn a_site_filed_again_after_its_retraction_restores_the_same_state() {
        let file_id = FileId::new(1);
        let owner = DynamicFieldOwner::Type(LuaTypeDeclId::global("OwnerA"));
        let member_id = LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range(1, 4)),
            file_id,
        );
        let site = InFiled::new(file_id, range(0, 10));
        let sibling = InFiled::new(file_id, range(20, 30));
        let file_site = |index: &mut DynamicFieldIndex| {
            index.add_unattributed_field(&site, SmolStr::new("loose"), range(5, 6));
            index.add_field(&site, owner.clone(), SmolStr::new("f"), range(1, 2));
            index.add_finite_named_member(&site, owner.clone(), member_id);
        };
        let file_sibling = |index: &mut DynamicFieldIndex| {
            index.add_field(&sibling, owner.clone(), SmolStr::new("f"), range(21, 22));
        };

        let mut cold = DynamicFieldIndex::new();
        file_site(&mut cold);
        file_sibling(&mut cold);
        let mut warm = DynamicFieldIndex::new();
        file_sibling(&mut warm);
        file_site(&mut warm);
        warm.retract_site(&site);
        file_site(&mut warm);

        assert_eq!(
            (
                installed_state(&warm),
                warm.site_contributions.clone(),
                warm.finite_named_members.clone(),
                warm.unattributed_fields.clone(),
            ),
            (
                installed_state(&cold),
                cold.site_contributions.clone(),
                cold.finite_named_members.clone(),
                cold.unattributed_fields.clone(),
            )
        );
    }

    /// Unattributed records are owned by the write, not by
    /// `(field_name, file)`: retracting one site withdraws only its record
    /// while a sibling's under the same name survives, and removing the file
    /// drains both.
    #[test]
    fn unattributed_records_are_site_owned() {
        let file_id = FileId::new(1);
        let site = InFiled::new(file_id, range(1, 2));
        let sibling = InFiled::new(file_id, range(5, 6));

        let mut index = DynamicFieldIndex::new();
        index.add_unattributed_field(&site, SmolStr::new("shared"), range(1, 2));
        index.add_unattributed_field(&sibling, SmolStr::new("shared"), range(5, 6));

        index.retract_site(&site);
        assert!(index.has_unattributed_field("shared"));

        index.remove(file_id);
        assert!(!index.has_unattributed_field("shared"));
        assert!(file_sites(&index, file_id).is_empty());
    }
}

crate::db_index::cow_shards::task_fork_fields!(DynamicFieldIndex {
    field_definitions => |current, frozen, fork| {
        crate::db_index::cow_shards::merge_map(current, frozen, fork, merge_def_sites)
    },
    direct_field_definitions => |current, frozen, fork| {
        crate::db_index::cow_shards::merge_map(current, frozen, fork, merge_def_sites)
    },
    finite_named_members => crate::db_index::cow_shards::merge_set,
    wildcard_definitions => merge_definitions,
    unattributed_fields => crate::db_index::cow_shards::merge_set,
    declaring_types => |current, frozen, fork| {
        crate::db_index::cow_shards::merge_map(current, frozen, fork, crate::db_index::cow_shards::merge_sum)
    },
    site_contributions,
    owner_named_fingerprints => crate::db_index::cow_shards::rebuilt,
} prepare owners_to_rebuild then rebuild_owner_named
  by_field install_owner_named => rebuild_owner_named);
