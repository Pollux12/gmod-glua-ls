use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use hashbrown::HashTable;
use rustc_hash::{FxHashMap, FxHashSet};

use super::paged::{PAGE, Paged};
use super::{DerivationId, DerivationKind, EditMaps, FactKey, Fingerprint, HashedKey};
use crate::FileId;

/// A fact key's slot in a [`KeyTable`].
pub type KeyIdx = u32;
/// A derivation's slot in [`ReaderGraph::derivations`].
type DerivIdx = u32;

/// What one derivation's last evaluation read and wrote, as sorted key slots.
/// The fingerprint a read saw is the reader group the derivation sits in on
/// the key ([`Readers`]).
#[derive(Debug, Clone, Default)]
struct Record {
    /// The reads, then the outputs.
    keys: Box<[KeyIdx]>,
    reads: u32,
}

impl Record {
    fn new(reads: impl ExactSizeIterator<Item = KeyIdx>, outputs: &[KeyIdx]) -> Self {
        let read_count = u32::try_from(reads.len()).expect("fewer than 2^32 reads");
        let mut keys = Vec::with_capacity(reads.len() + outputs.len());
        keys.extend(reads);
        keys.extend_from_slice(outputs);
        Self {
            keys: keys.into_boxed_slice(),
            reads: read_count,
        }
    }

    fn reads(&self) -> &[KeyIdx] {
        &self.keys[..self.reads as usize]
    }

    fn outputs(&self) -> &[KeyIdx] {
        &self.keys[self.reads as usize..]
    }

    /// This record with `reads` in place of its reads.
    fn with_reads(&self, reads: &[KeyIdx]) -> Self {
        Self::new(reads.iter().copied(), self.outputs())
    }

    /// This record with `outputs` in place of its outputs.
    fn with_outputs(&self, outputs: &[KeyIdx]) -> Self {
        Self::new(self.reads().iter().copied(), outputs)
    }
}

/// The files whose positions `key` names, which an edit of one of them
/// renames: its positional file and the files its expression key's type names.
fn positional_files(key: &FactKey) -> impl Iterator<Item = FileId> {
    let referenced = key
        .expr_key_type()
        .map(crate::db_index::r#type::type_referenced_files)
        .unwrap_or_default();
    key.positional_file().into_iter().chain(referenced)
}

/// The live slots of `keys` by each file whose positions their key names.
fn build_positional(keys: &Paged<KeySlot>) -> FxHashMap<FileId, FxHashSet<KeyIdx>> {
    use rayon::prelude::*;
    let mut pairs = keys
        .pages()
        .par_iter()
        .enumerate()
        .flat_map_iter(|(page, slots)| {
            slots.iter().enumerate().flat_map(move |(offset, entry)| {
                let slot =
                    KeyIdx::try_from(page * PAGE + offset).expect("fewer than 2^32 fact keys");
                entry
                    .key
                    .as_ref()
                    .into_iter()
                    .flat_map(|key| positional_files(key))
                    .map(move |file_id| (file_id, slot))
            })
        })
        .collect::<Vec<_>>();
    pairs.par_sort_unstable();
    pairs
        .par_chunk_by(|left, right| left.0 == right.0)
        .map(|group| (group[0].0, group.iter().map(|(_, slot)| *slot).collect()))
        .collect()
}

/// A fact key and the derivations that read or produced it. `key` is `None`
/// while the slot is free.
#[derive(Debug)]
struct KeySlot {
    key: Option<HashedKey>,
    readers: Readers,
    /// The key's first producer ([`NO_PRODUCER`]: none) in the bits under
    /// [`MORE_PRODUCERS`], [`WINDOWED`] and [`HELD`].
    marks: u32,
    /// The key's fingerprint as last taken, and the generation it was taken in
    /// ([`NO_GENERATION`]: none); see [`ReaderGraph::current_fingerprint`].
    current: AtomicU64,
    current_generation: AtomicU32,
    /// The stamp the key was last logged under; see [`KeyTable::log`].
    stamp: u64,
    /// The stamp current when the key was last written.
    written: u64,
}

impl KeySlot {
    fn producer(&self) -> DerivIdx {
        self.marks & NO_PRODUCER
    }

    fn set_producer(&mut self, deriv: DerivIdx) {
        self.marks = (self.marks & !NO_PRODUCER) | deriv;
    }

    fn marked(&self, mark: u32) -> bool {
        (self.marks & mark) != 0
    }

    fn set_mark(&mut self, mark: u32, on: bool) {
        if on {
            self.marks |= mark;
        } else {
            self.marks &= !mark;
        }
    }

    /// Whether the key is a query's output.
    fn is_query(&self) -> bool {
        self.key.as_ref().is_some_and(|key| {
            matches!(
                key.key(),
                FactKey::ReceiverAuthority(_) | FactKey::CallbackParams(_)
            )
        })
    }
}

impl Clone for KeySlot {
    fn clone(&self) -> Self {
        let generation = self.current_generation.load(Ordering::Acquire);
        Self {
            key: self.key.clone(),
            readers: self.readers.clone(),
            marks: self.marks,
            current: AtomicU64::new(self.current.load(Ordering::Relaxed)),
            current_generation: AtomicU32::new(generation),
            stamp: self.stamp,
            written: self.written,
        }
    }
}

impl Default for KeySlot {
    fn default() -> Self {
        Self {
            key: None,
            readers: Readers::default(),
            marks: NO_PRODUCER,
            current: AtomicU64::new(0),
            current_generation: AtomicU32::new(NO_GENERATION),
            stamp: 0,
            written: 0,
        }
    }
}

const NO_GENERATION: u32 = u32::MAX;
const NO_PRODUCER: DerivIdx = (1 << 29) - 1;
/// The mark of a key whose producers past the first are in
/// [`KeyTable::more_producers`].
const MORE_PRODUCERS: u32 = 1 << 29;
/// The mark of a key an evaluation took away to compute again; see
/// [`KeyTable::open_window`].
const WINDOWED: u32 = 1 << 30;
/// The mark of a key a loop the engine cleared took away until it settles
/// again; see [`KeyTable::hold_window`].
const HELD: u32 = 1 << 31;

#[cfg(test)]
mod query_presence_tests {
    use super::*;
    use crate::{FileId, LuaSignatureId};
    use rowan::TextSize;

    #[test]
    fn parallel_transport_query_slot_width_clone_and_recycle() {
        assert_eq!(std::mem::size_of::<KeySlot>(), 120);
        let mut graph = ReaderGraph::default();
        let query = HashedKey::new(FactKey::CallbackParams(LuaSignatureId::new(
            FileId::new(1),
            TextSize::new(5),
        )));
        let slot = graph.intern_read(query);
        assert!(graph.keys().is_query(slot));
        let clone = graph.keys().clone();
        assert!(clone.is_query(slot));
        graph.release_keys(vec![slot]);
        let ordinary = graph.intern_read(HashedKey::new(FactKey::GmodRealm(FileId::new(2))));
        assert_eq!(ordinary, slot);
        assert!(!graph.keys().is_query(slot));
        assert!(clone.is_query(slot));
    }

    #[test]
    fn unlent_read_warming_leaves_query_outputs_to_the_root() {
        let query = HashedKey::new(FactKey::CallbackParams(LuaSignatureId::new(
            FileId::new(1),
            TextSize::new(5),
        )));
        let fact = HashedKey::new(FactKey::GmodRealm(FileId::new(2)));
        let mut graph = ReaderGraph::default();
        let fact_slot = graph.intern_read(fact.clone());
        graph.intern_read(query.clone());
        let _recording = super::super::begin(DerivationId::workspace(DerivationKind::ValidGuards));
        super::super::note_read(|| query.key().clone());
        super::super::note_read(|| fact.key().clone());
        let evaluation = super::super::end();
        evaluation.fingerprint_unlent_reads(&graph, |key| {
            assert_ne!(key, &query, "a query output was fingerprinted off the root");
            17
        });
        assert_eq!(
            graph.slot_fingerprint(fact_slot, |_| panic!("the read was not warmed")),
            17
        );
    }
}

/// A key's readers, grouped by the fingerprint each saw, each group sorted.
/// A derivation reads a key at one value, so it is in one group. Nearly every
/// key has one group, so checking its readers against a new value is one
/// comparison per group rather than one per reader, and one group is kept in
/// the slot.
#[derive(Debug, Clone, Default)]
enum Readers {
    #[default]
    None,
    One(Fingerprint, DerivIdx),
    /// Two or more readers.
    Group(Fingerprint, Vec<DerivIdx>),
    /// Two or more groups.
    Groups(Vec<(Fingerprint, Vec<DerivIdx>)>),
}

const _: () = assert!(std::mem::size_of::<Readers>() <= 32);

impl Readers {
    /// Files `deriv` as having seen `seen`, out of the group it was in.
    fn insert(&mut self, deriv: DerivIdx, seen: Fingerprint) {
        match self {
            Self::None => *self = Self::One(seen, deriv),
            Self::One(group, reader) if *reader == deriv => *group = seen,
            Self::One(group, reader) if *group == seen => {
                let pair = if *reader < deriv {
                    vec![*reader, deriv]
                } else {
                    vec![deriv, *reader]
                };
                *self = Self::Group(seen, pair);
            }
            Self::Group(group, derivs) if *group == seen => {
                if let Err(at) = derivs.binary_search(&deriv) {
                    reserve_readers(derivs, 1);
                    derivs.insert(at, deriv);
                }
            }
            Self::Groups(groups) => {
                for (group, derivs) in groups.iter_mut() {
                    if *group == seen {
                        if let Err(at) = derivs.binary_search(&deriv) {
                            reserve_readers(derivs, 1);
                            derivs.insert(at, deriv);
                        }
                    } else {
                        take_from(derivs, deriv);
                    }
                }
                if !groups.iter().any(|(group, _)| *group == seen) {
                    groups.push((seen, vec![deriv]));
                }
                self.collapse();
            }
            Self::One(group, reader) => {
                *self = Self::Groups(vec![(*group, vec![*reader]), (seen, vec![deriv])]);
            }
            Self::Group(group, derivs) => {
                take_from(derivs, deriv);
                let derivs = std::mem::take(derivs);
                *self = Self::Groups(vec![(*group, derivs), (seen, vec![deriv])]);
                self.collapse();
            }
        }
    }

    fn remove(&mut self, deriv: DerivIdx) {
        match self {
            Self::None => {}
            Self::One(_, reader) => {
                if *reader == deriv {
                    *self = Self::None;
                }
            }
            Self::Group(seen, derivs) => {
                take_from(derivs, deriv);
                if let [reader] = derivs.as_slice() {
                    *self = Self::One(*seen, *reader);
                }
            }
            Self::Groups(groups) => {
                for (_, derivs) in groups.iter_mut() {
                    take_from(derivs, deriv);
                }
                self.collapse();
            }
        }
    }

    /// The fingerprint `deriv`, one of the readers, saw.
    fn seen_by(&self, deriv: DerivIdx) -> Fingerprint {
        match self {
            Self::One(seen, reader) => {
                debug_assert_eq!(*reader, deriv);
                *seen
            }
            Self::Group(seen, derivs) => {
                debug_assert!(derivs.binary_search(&deriv).is_ok());
                *seen
            }
            _ => self
                .groups()
                .find(|(_, derivs)| derivs.binary_search(&deriv).is_ok())
                .map(|(seen, _)| seen)
                .expect("a recorded read is filed among the key's readers"),
        }
    }

    /// Relabels the readers that saw `from` as having seen `to`.
    fn relabel(&mut self, from: Fingerprint, to: Fingerprint) {
        match self {
            Self::None => {}
            Self::One(seen, _) | Self::Group(seen, _) => {
                if *seen == from {
                    *seen = to;
                }
            }
            Self::Groups(groups) => {
                if from == to {
                    return;
                }
                let Some(at) = groups.iter().position(|(seen, _)| *seen == from) else {
                    return;
                };
                let (_, derivs) = groups.swap_remove(at);
                match groups.iter_mut().find(|(seen, _)| *seen == to) {
                    Some((_, into)) => {
                        into.extend(derivs);
                        into.sort_unstable();
                    }
                    None => groups.push((to, derivs)),
                }
                self.collapse();
            }
        }
    }

    /// Applies `ops` as applying each in order would, the readers moving
    /// between groups in one pass over each group they leave or join.
    fn apply_all(&mut self, ops: &[(KeyIdx, ReaderOp)]) {
        if let [(_, op)] = ops {
            op.apply(self);
            return;
        }
        // Groups in order with their sizes, and each touched reader's group
        // before the ops and after those applied so far: a group leaves the
        // order once emptied and joins at its end once its value is first
        // seen again, as one-by-one application leaves them.
        let mut order = self
            .groups()
            .map(|(seen, derivs)| (seen, derivs.len()))
            .collect::<Vec<_>>();
        let mut touched =
            FxHashMap::<DerivIdx, (Option<Fingerprint>, Option<Fingerprint>)>::default();
        for (_, op) in ops {
            let (deriv, to) = match *op {
                ReaderOp::Seen(deriv, seen) | ReaderOp::Insert(deriv, seen) => (deriv, Some(seen)),
                ReaderOp::Remove(deriv) => (deriv, None),
            };
            let (_, now) = touched.entry(deriv).or_insert_with(|| {
                let before = self.group_of(deriv);
                (before, before)
            });
            if *now == to {
                continue;
            }
            if let Some(from) = *now {
                let at = order
                    .iter()
                    .position(|(seen, _)| *seen == from)
                    .expect("a reader's group is in the order");
                order[at].1 -= 1;
                if order[at].1 == 0 {
                    order.remove(at);
                }
            }
            if let Some(to) = to {
                match order.iter_mut().find(|(seen, _)| *seen == to) {
                    Some((_, len)) => *len += 1,
                    None => order.push((to, 1)),
                }
            }
            *now = to;
        }
        let mut leaving = Vec::new();
        let mut joining = Vec::new();
        for (deriv, (before, after)) in touched {
            if before == after {
                continue;
            }
            leaving.extend(before.map(|seen| (seen, deriv)));
            joining.extend(after.map(|seen| (seen, deriv)));
        }
        if leaving.is_empty() && joining.is_empty() {
            return;
        }
        leaving.sort_unstable();
        joining.sort_unstable();
        let mut groups = match std::mem::take(self) {
            Self::None => Vec::new(),
            Self::One(seen, reader) => vec![(seen, vec![reader])],
            Self::Group(seen, derivs) => vec![(seen, derivs)],
            Self::Groups(groups) => groups,
        };
        let of = |moved: &[(Fingerprint, DerivIdx)], seen: Fingerprint| {
            let start = moved.partition_point(|(group, _)| *group < seen);
            let end = moved.partition_point(|(group, _)| *group <= seen);
            moved[start..end]
                .iter()
                .map(|(_, deriv)| *deriv)
                .collect::<Vec<_>>()
        };
        let regrouped = order
            .into_iter()
            .map(|(seen, len)| {
                let mut derivs = groups
                    .iter()
                    .position(|(group, _)| *group == seen)
                    .map(|at| groups.swap_remove(at).1)
                    .unwrap_or_default();
                remove_sorted(&mut derivs, &of(&leaving, seen));
                insert_sorted(&mut derivs, &of(&joining, seen));
                debug_assert_eq!(derivs.len(), len);
                (seen, derivs)
            })
            .collect::<Vec<_>>();
        *self = Self::Groups(regrouped);
        self.collapse();
    }

    /// The group `deriv` reads in, if it is a reader.
    fn group_of(&self, deriv: DerivIdx) -> Option<Fingerprint> {
        self.groups()
            .find(|(_, derivs)| derivs.binary_search(&deriv).is_ok())
            .map(|(seen, _)| seen)
    }

    fn is_empty(&self) -> bool {
        matches!(self, Self::None)
    }

    /// Each group's fingerprint and its readers.
    fn groups(&self) -> impl Iterator<Item = (Fingerprint, &[DerivIdx])> {
        let (single, many) = match self {
            Self::None => (None, &[][..]),
            Self::One(seen, reader) => (Some((*seen, std::slice::from_ref(reader))), &[][..]),
            Self::Group(seen, derivs) => (Some((*seen, derivs.as_slice())), &[][..]),
            Self::Groups(groups) => (None, &groups[..]),
        };
        single
            .into_iter()
            .chain(many.iter().map(|(seen, derivs)| (*seen, derivs.as_slice())))
    }

    fn iter(&self) -> impl Iterator<Item = (Fingerprint, DerivIdx)> + '_ {
        self.groups()
            .flat_map(|(seen, derivs)| derivs.iter().map(move |deriv| (seen, *deriv)))
    }

    /// The heap bytes of the group list and of the reader lists.
    fn heap_bytes(&self) -> (usize, usize) {
        let list = |derivs: &Vec<DerivIdx>| derivs.capacity() * std::mem::size_of::<DerivIdx>();
        match self {
            Self::None | Self::One(..) => (0, 0),
            Self::Group(_, derivs) => (0, list(derivs)),
            Self::Groups(groups) => (
                groups.capacity() * std::mem::size_of::<(Fingerprint, Vec<DerivIdx>)>(),
                groups.iter().map(|(_, derivs)| list(derivs)).sum(),
            ),
        }
    }

    /// Drops emptied groups, and keeps what is left in the smallest form.
    fn collapse(&mut self) {
        let Self::Groups(groups) = self else {
            return;
        };
        groups.retain(|(_, derivs)| !derivs.is_empty());
        if groups.len() > 1 {
            return;
        }
        *self = match groups.pop() {
            None => Self::None,
            Some((seen, derivs)) => match derivs.as_slice() {
                [reader] => Self::One(seen, *reader),
                _ => Self::Group(seen, derivs),
            },
        };
    }
}

/// The sorted `reads` with each source of `moves`, sorted by source, replaced
/// by its target, sorted and without repeats.
fn renamed_slots(reads: &[KeyIdx], moves: &[(KeyIdx, KeyIdx)]) -> Box<[KeyIdx]> {
    let mut kept = Vec::with_capacity(reads.len());
    let mut renamed = Vec::with_capacity(moves.len());
    let mut next = 0;
    for &read in reads {
        while next < moves.len() && moves[next].0 < read {
            next += 1;
        }
        match moves.get(next) {
            Some(&(source, target)) if source == read => renamed.push(target),
            _ => kept.push(read),
        }
    }
    renamed.sort_unstable();
    renamed.dedup();
    let mut merged = Vec::with_capacity(kept.len() + renamed.len());
    let (mut kept_at, mut renamed_at) = (0, 0);
    while kept_at < kept.len() && renamed_at < renamed.len() {
        if kept[kept_at] < renamed[renamed_at] {
            merged.push(kept[kept_at]);
            kept_at += 1;
        } else if kept[kept_at] > renamed[renamed_at] {
            merged.push(renamed[renamed_at]);
            renamed_at += 1;
        } else {
            merged.push(kept[kept_at]);
            kept_at += 1;
            renamed_at += 1;
        }
    }
    merged.extend_from_slice(&kept[kept_at..]);
    merged.extend_from_slice(&renamed[renamed_at..]);
    merged.into_boxed_slice()
}

/// Makes room in `derivs` for `more` readers, growing it by half at least:
/// the spare room a doubling leaves is kept across millions of keys.
fn reserve_readers(derivs: &mut Vec<DerivIdx>, more: usize) {
    if derivs.capacity() - derivs.len() < more {
        derivs.reserve_exact(more.max(derivs.len() / 2).max(4));
    }
}

/// Removes `deriv` from the sorted `derivs`, if there.
fn take_from(derivs: &mut Vec<DerivIdx>, deriv: DerivIdx) {
    if let Ok(index) = derivs.binary_search(&deriv) {
        derivs.remove(index);
    }
}

/// Removes the sorted `gone` from the sorted `derivs`, moving each kept run
/// once.
fn remove_sorted(derivs: &mut Vec<DerivIdx>, gone: &[DerivIdx]) {
    let (mut read, mut write) = (0, 0);
    for deriv in gone {
        let Ok(found) = derivs[read..].binary_search(deriv) else {
            continue;
        };
        let at = read + found;
        derivs.copy_within(read..at, write);
        write += at - read;
        read = at + 1;
    }
    let len = derivs.len();
    derivs.copy_within(read..len, write);
    derivs.truncate(write + len - read);
}

/// Inserts the sorted `new`, none of them in it, into the sorted `derivs`,
/// moving each existing run once.
fn insert_sorted(derivs: &mut Vec<DerivIdx>, new: &[DerivIdx]) {
    reserve_readers(derivs, new.len());
    let mut read_end = derivs.len();
    derivs.resize(read_end + new.len(), 0);
    let mut write_end = derivs.len();
    for deriv in new.iter().rev() {
        let at = derivs[..read_end].partition_point(|existing| existing < deriv);
        let run = read_end - at;
        derivs.copy_within(at..read_end, write_end - run);
        write_end -= run + 1;
        derivs[write_end] = *deriv;
        read_end = at;
    }
}

/// The hash [`ReaderGraph::deriv_slots`] files `id` under.
fn deriv_hash(id: &DerivationId) -> u64 {
    use std::hash::BuildHasher;
    rustc_hash::FxBuildHasher.hash_one(id)
}

#[derive(Debug, Clone)]
struct DerivSlot {
    id: DerivationId,
    record: Option<Record>,
}

/// The reader graph: for every derivation the facts its last evaluation read
/// (with the fingerprint it saw) and the facts it produced, plus the reverse
/// edges from a fact to its readers and producers.
///
/// Keys and derivations are interned to slots, so recording an evaluation
/// again diffs two sorted slot lists and only the edges that changed touch a
/// map. A slot nothing refers to any more is freed for reuse.
/// Fact keys interned to slots. While the engine evaluates a group, the table
/// is lent to the recording thread so a read is interned where it is made.
#[derive(Debug, Clone, Default)]
pub struct KeyTable {
    /// Each live slot, found by its key's hash: the key itself is kept only
    /// in its slot.
    key_slots: HashTable<FiledKey>,
    keys: Paged<KeySlot>,
    free_keys: Vec<KeyIdx>,
    retirement: Option<Arc<AtomicBool>>,
    retirement_started: u64,
    retired: Vec<KeyIdx>,
    /// The keys read out of each base key ([`FactKey::base`]).
    derived: FxHashMap<KeyIdx, DerivedKeys>,
    /// The producers of a key past the one its slot keeps: few keys have more
    /// than one.
    more_producers: FxHashMap<KeyIdx, Vec<DerivIdx>>,
    /// The stamp of the latest write or release of a live slot.
    last_written: u64,
    /// The keys an evaluation took away to compute again, each with the stamp
    /// it was written at before; see [`Self::open_window`].
    windows: FxHashMap<KeyIdx, u64>,
    /// The keys a cleared loop took away until it settles again, each with the
    /// stamp it was written at before; see [`Self::hold_window`].
    held: FxHashMap<KeyIdx, u64>,
    /// The live slots of the keys naming each file's positions, built by the
    /// first edit that renames keys and kept from then on.
    positional: Option<FxHashMap<FileId, FxHashSet<KeyIdx>>>,
}

/// A live slot in [`KeyTable::key_slots`], with the low half of its key's
/// hash: probing and growing the index compare and rehash without reading
/// the slot.
#[derive(Debug, Clone, Copy)]
struct FiledKey {
    slot: KeyIdx,
    hash: u32,
}

impl FiledKey {
    fn half(key: &HashedKey) -> u32 {
        key.hash as u32
    }

    /// The hash the index files `half` under: its low bits are `half`'s, and
    /// its high bits mix all of it.
    fn index_hash(half: u32) -> u64 {
        u64::from(half).wrapping_mul(0x9E37_79B9_7F4A_7C15)
    }
}

/// Whether what a memo read still holds; see [`KeyTable::written_since`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Freshness {
    Unwritten,
    /// Only keys an evaluation is computing again were written since: the
    /// memo holds again if they come back as they were.
    Recomputing,
    Written,
}

impl Freshness {
    /// What two sets of reads say together.
    pub(super) fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Written, _) | (_, Self::Written) => Self::Written,
            (Self::Recomputing, _) | (_, Self::Recomputing) => Self::Recomputing,
            _ => Self::Unwritten,
        }
    }
}

/// The keys read out of one base key, and the base's fingerprint when their
/// readers were last checked ([`UNCHECKED`]: never).
#[derive(Debug)]
struct DerivedKeys {
    slots: Vec<KeyIdx>,
    checked_at: AtomicU64,
}

const UNCHECKED: Fingerprint = Fingerprint::MAX;

impl Default for DerivedKeys {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            checked_at: AtomicU64::new(UNCHECKED),
        }
    }
}

impl Clone for DerivedKeys {
    fn clone(&self) -> Self {
        Self {
            slots: self.slots.clone(),
            checked_at: AtomicU64::new(self.checked_at.load(Ordering::Relaxed)),
        }
    }
}

impl KeyTable {
    pub(crate) fn quarantine_releases(&mut self) -> super::ReleaseQuarantine {
        self.collect_retired();
        assert!(self.retirement.is_none(), "overlapping release quarantines");
        let active = Arc::new(AtomicBool::new(true));
        self.retirement = Some(Arc::clone(&active));
        self.retirement_started = super::new_stamp();
        super::ReleaseQuarantine::new(active, self.retirement_started)
    }

    pub(super) fn finish_quarantine(&mut self, active: &Arc<AtomicBool>) {
        assert!(
            self.retirement
                .as_ref()
                .is_some_and(|owned| Arc::ptr_eq(owned, active))
        );
        active.store(false, Ordering::Release);
        self.collect_retired();
    }

    pub(super) fn quarantines(&self, stamp: u64) -> bool {
        stamp > self.retirement_started
            && self
                .retirement
                .as_ref()
                .is_some_and(|active| active.load(Ordering::Acquire))
    }

    pub(super) fn quarantine_started(&self) -> Option<u64> {
        self.retirement
            .as_ref()
            .filter(|active| active.load(Ordering::Acquire))
            .map(|_| self.retirement_started)
    }

    fn collect_retired(&mut self) {
        if self
            .retirement
            .as_ref()
            .is_some_and(|active| !active.load(Ordering::Acquire))
        {
            self.retirement = None;
            self.retirement_started = 0;
            self.free_keys.append(&mut self.retired);
        }
    }

    fn retire(&mut self, slot: KeyIdx) {
        self.collect_retired();
        if self.retirement.is_some() {
            self.retired.push(slot);
        } else {
            self.free_keys.push(slot);
        }
    }

    pub(super) fn live_key(&self, slot: KeyIdx) -> Option<&HashedKey> {
        self.keys.get(slot as usize)?.key.as_ref()
    }

    pub(super) fn matching_slot(
        &self,
        slot: KeyIdx,
        matches: impl FnOnce(&FactKey) -> bool,
    ) -> Option<KeyIdx> {
        let entry = self.keys.get(slot as usize)?;
        entry
            .key
            .as_ref()
            .filter(|key| matches(key.key()))
            .map(|_| slot)
    }

    /// The slot `key` is interned to.
    pub(super) fn find(&self, key: &HashedKey) -> Option<KeyIdx> {
        let half = FiledKey::half(key);
        self.key_slots
            .find(FiledKey::index_hash(half), |filed| {
                filed.hash == half && self.keys[filed.slot as usize].key.as_ref() == Some(key)
            })
            .map(|filed| filed.slot)
    }

    /// Unfiles the live `slot`, whose key is `key`.
    fn unfile(&mut self, key: &HashedKey, slot: KeyIdx) {
        let index_hash = FiledKey::index_hash(FiledKey::half(key));
        if let Ok(entry) = self
            .key_slots
            .find_entry(index_hash, |filed| filed.slot == slot)
        {
            entry.remove();
        }
        if let Some(positional) = &mut self.positional {
            for file_id in positional_files(key) {
                if let Some(slots) = positional.get_mut(&file_id) {
                    slots.remove(&slot);
                }
            }
        }
    }

    /// The live slots of the keys naming a position in one of `files`, in
    /// slot order.
    fn positional_slots(&mut self, files: &FxHashSet<FileId>) -> Vec<KeyIdx> {
        use rayon::prelude::*;
        let keys = &self.keys;
        let positional = self.positional.get_or_insert_with(|| {
            crate::profile::phase("reader graph: positional index", || build_positional(keys))
        });
        let mut slots = files
            .iter()
            .filter_map(|file_id| positional.get(file_id))
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        slots.par_sort_unstable();
        slots.dedup();
        slots
    }

    /// Interns `key`. A new slot counts as written at stamp `now`: older
    /// memos cannot have read the key it now names.
    fn intern(&mut self, key: HashedKey, now: u64) -> KeyIdx {
        self.intern_created(key, now).0
    }

    /// [`Self::intern`], and for a slot it created, the slot of the key's
    /// base.
    fn intern_created(&mut self, key: HashedKey, now: u64) -> (KeyIdx, Option<Option<KeyIdx>>) {
        self.collect_retired();
        if let Some(slot) = self.find(&key) {
            return (slot, None);
        }
        let slot = match self.free_keys.pop() {
            Some(slot) => slot,
            None => KeyIdx::try_from(self.keys.push(KeySlot::default()))
                .expect("fewer than 2^32 fact keys"),
        };
        let base = key.base();
        let half = FiledKey::half(&key);
        let entry = &mut self.keys[slot as usize];
        if let Some(positional) = &mut self.positional {
            for file_id in positional_files(&key) {
                positional.entry(file_id).or_default().insert(slot);
            }
        }
        entry.key = Some(key);
        entry.stamp = 0;
        entry.written = now;
        entry
            .current_generation
            .store(NO_GENERATION, Ordering::Relaxed);
        self.key_slots.insert_unique(
            FiledKey::index_hash(half),
            FiledKey { slot, hash: half },
            |filed| FiledKey::index_hash(filed.hash),
        );
        let base = base.map(|base| {
            let base = self.intern(HashedKey::new(base), now);
            self.derived.entry(base).or_default().slots.push(slot);
            base
        });
        (slot, Some(base))
    }

    pub(super) fn key(&self, slot: KeyIdx) -> &HashedKey {
        self.keys[slot as usize]
            .key
            .as_ref()
            .expect("a referenced key slot is live")
    }

    pub(super) fn is_query(&self, slot: KeyIdx) -> bool {
        self.keys[slot as usize].is_query()
    }

    pub(super) fn intern_read(&mut self, key: HashedKey, now: u64) -> KeyIdx {
        self.intern(key, now)
    }

    /// Interns a key a worker read that its snapshot lacked: created when the
    /// release epoch began, whichever evaluation of the epoch publishes it first.
    pub(super) fn intern_worker_read(&mut self, key: HashedKey) -> KeyIdx {
        let (slot, created) = self.intern_created(key, self.retirement_started);
        if self.windows.is_empty() && self.held.is_empty() {
            return slot;
        }
        let mut base_slot = match created {
            Some(None) => return slot,
            Some(Some(base_slot)) => base_slot,
            None => {
                let Some(base_slot) = self
                    .key(slot)
                    .base()
                    .and_then(|base| self.find(&HashedKey::new(base)))
                else {
                    return slot;
                };
                base_slot
            }
        };
        loop {
            let windowed = self.windowed(base_slot);
            let held = self.held(base_slot);
            let entry = &mut self.keys[slot as usize];
            if windowed {
                self.windows.entry(slot).or_insert(entry.written);
                entry.set_mark(WINDOWED, true);
            }
            if held {
                self.held.entry(slot).or_insert(entry.written);
                entry.set_mark(HELD, true);
            }
            let Some(base) = self.key(base_slot).base() else {
                return slot;
            };
            base_slot = self
                .find(&HashedKey::new(base))
                .expect("interning a key interns its bases");
        }
    }

    /// Marks `key` written at stamp `now`, if anything ever read it, and every
    /// key read out of it.
    pub(super) fn mark_written(&mut self, key: &HashedKey, now: u64) {
        if let Some(slot) = self.find(key) {
            self.last_written = self.last_written.max(now);
            self.keys[slot as usize].written = now;
            for derived in self.derived.get(&slot).into_iter().flat_map(|d| &d.slots) {
                self.keys[*derived as usize].written = now;
            }
        }
    }

    /// Whether `key` was logged at or after stamp `since`, marking it —
    /// and every key read out of it — written at `now` as [`Self::mark_written`].
    pub(super) fn mark_written_logged(&mut self, key: &HashedKey, since: u64, now: u64) -> bool {
        let Some(slot) = self.find(key) else {
            return false;
        };
        let logged = self.keys[slot as usize].stamp >= since;
        self.last_written = self.last_written.max(now);
        self.keys[slot as usize].written = now;
        for derived in self.derived.get(&slot).into_iter().flat_map(|d| &d.slots) {
            self.keys[*derived as usize].written = now;
        }
        logged
    }

    /// Marks each of `slots` written at stamp `now`, a chunk of slots at a
    /// time on worker threads.
    fn mark_slots_written(&mut self, slots: &mut [KeyIdx], now: u64) {
        use rayon::prelude::*;
        if slots.is_empty() {
            return;
        }
        self.last_written = self.last_written.max(now);
        slots.par_sort_unstable();
        let slots: &[KeyIdx] = slots;
        self.par_reached_chunks(
            slots,
            |slot| *slot,
            |first, chunk, range| {
                for slot in &slots[range] {
                    chunk[*slot as usize - first].written = now;
                }
            },
        );
    }

    /// Marks `key`, and every key read out of it, written at stamp `now` by an
    /// evaluation that computes it again: a memo that read it before then
    /// holds again if [`Self::close_windows`] finds it came back unchanged,
    /// and nothing read from it until then is shared.
    pub(super) fn open_window(&mut self, key: &HashedKey, now: u64) {
        let Some(slot) = self.find(key) else {
            return;
        };
        self.last_written = self.last_written.max(now);
        let derived = self.derived.get(&slot).into_iter().flat_map(|d| &d.slots);
        for slot in std::iter::once(&slot).chain(derived) {
            let entry = &mut self.keys[*slot as usize];
            self.windows.entry(*slot).or_insert(entry.written);
            entry.written = now;
            entry.set_mark(WINDOWED, true);
        }
    }

    /// Closes every window: a key `unchanged` accepts, and each key read out
    /// of it, is written as it was before its window opened.
    pub(super) fn close_windows(&mut self, unchanged: impl Fn(&HashedKey) -> bool) {
        let windows = std::mem::take(&mut self.windows);
        for (slot, before) in &windows {
            let entry = &mut self.keys[*slot as usize];
            entry.set_mark(WINDOWED, false);
            let Some(key) = entry.key.as_ref() else {
                continue;
            };
            if key.base().is_some() || !unchanged(key) {
                continue;
            }
            entry.written = *before;
            let derived = self.derived.get(slot).into_iter().flat_map(|d| &d.slots);
            for derived in derived {
                if let Some(before) = windows.get(derived) {
                    self.keys[*derived as usize].written = *before;
                }
            }
        }
    }

    /// [`Self::open_window`] for a loop of statements cleared to settle again
    /// from nothing: the window stays open across the evaluations that settle
    /// it, until [`Self::release_held`].
    pub(super) fn hold_window(&mut self, key: &HashedKey, now: u64) {
        let Some(slot) = self.find(key) else {
            return;
        };
        self.last_written = self.last_written.max(now);
        let derived = self.derived.get(&slot).into_iter().flat_map(|d| &d.slots);
        for slot in std::iter::once(&slot).chain(derived) {
            let entry = &mut self.keys[*slot as usize];
            self.held.entry(*slot).or_insert(entry.written);
            entry.written = now;
            entry.set_mark(HELD, true);
        }
    }

    /// Closes every held window: a key `unchanged` accepts, and each key read
    /// out of it, is written as it was before its window opened.
    pub(super) fn release_held(&mut self, unchanged: impl Fn(&HashedKey) -> bool) {
        let held = std::mem::take(&mut self.held);
        for (slot, before) in &held {
            let entry = &mut self.keys[*slot as usize];
            entry.set_mark(HELD, false);
            let Some(key) = entry.key.as_ref() else {
                continue;
            };
            if key.base().is_some() || !unchanged(key) {
                continue;
            }
            entry.written = *before;
            let derived = self.derived.get(slot).into_iter().flat_map(|d| &d.slots);
            for derived in derived {
                if let Some(before) = held.get(derived) {
                    self.keys[*derived as usize].written = *before;
                }
            }
        }
    }

    /// Whether no record reads or produces the key in `slot` and no key is
    /// read out of it.
    fn unreferenced(&self, slot: KeyIdx) -> bool {
        self.unreferenced_after(slot, &FxHashMap::default())
    }

    /// [`Self::unreferenced`] with the derived keys `freed_from` counts freed
    /// out of each base already gone from the base's list.
    fn unreferenced_after(&self, slot: KeyIdx, freed_from: &FxHashMap<KeyIdx, usize>) -> bool {
        let entry = &self.keys[slot as usize];
        entry.readers.is_empty()
            && entry.producer() == NO_PRODUCER
            && self
                .derived
                .get(&slot)
                .is_none_or(|derived| freed_from.get(&slot) == Some(&derived.slots.len()))
    }

    /// Drops the freed slots from the derived keys of each of `bases`, and
    /// the list of one left empty.
    fn drop_freed_derived(&mut self, bases: impl Iterator<Item = KeyIdx>) {
        for base in bases {
            let Some(derived) = self.derived.get_mut(&base) else {
                continue;
            };
            derived
                .slots
                .retain(|slot| self.keys[*slot as usize].key.is_some());
            if derived.slots.is_empty() {
                self.derived.remove(&base);
            }
        }
    }

    fn add_producer(&mut self, slot: KeyIdx, deriv: DerivIdx) {
        let entry = &mut self.keys[slot as usize];
        if entry.producer() == NO_PRODUCER {
            entry.set_producer(deriv);
        } else {
            entry.set_mark(MORE_PRODUCERS, true);
            self.more_producers.entry(slot).or_default().push(deriv);
        }
    }

    fn remove_producer(&mut self, slot: KeyIdx, deriv: DerivIdx) {
        let entry = &mut self.keys[slot as usize];
        if !entry.marked(MORE_PRODUCERS) {
            if entry.producer() == deriv {
                entry.set_producer(NO_PRODUCER);
            }
            return;
        }
        let more = self
            .more_producers
            .get_mut(&slot)
            .expect("a slot marked with more producers has them");
        if entry.producer() == deriv {
            entry.set_producer(more.pop().expect("more producers are never empty"));
        } else if let Some(at) = more.iter().position(|producer| *producer == deriv) {
            more.swap_remove(at);
        }
        if more.is_empty() {
            entry.set_mark(MORE_PRODUCERS, false);
            self.more_producers.remove(&slot);
        }
    }

    fn producers(&self, slot: KeyIdx) -> impl Iterator<Item = DerivIdx> + '_ {
        let entry = &self.keys[slot as usize];
        let more = entry
            .marked(MORE_PRODUCERS)
            .then(|| self.more_producers.get(&slot))
            .flatten();
        (entry.producer() != NO_PRODUCER)
            .then_some(entry.producer())
            .into_iter()
            .chain(more.into_iter().flatten().copied())
    }

    /// The slots of the keys read out of `slot`'s key.
    pub(super) fn derived_slots(&self, slot: KeyIdx) -> &[KeyIdx] {
        self.derived
            .get(&slot)
            .map_or(&[], |derived| &derived.slots)
    }

    /// Whether `slot`'s value is being computed again.
    pub(super) fn windowed(&self, slot: KeyIdx) -> bool {
        self.keys[slot as usize].marked(WINDOWED)
    }

    pub(super) fn held(&self, slot: KeyIdx) -> bool {
        self.keys[slot as usize].marked(HELD)
    }

    /// Whether a read of `slot` sees a value only the evaluation reading it
    /// can rely on: one being computed again, or one a cleared loop holds
    /// until it settles.
    pub(super) fn taints(&self, slot: KeyIdx) -> bool {
        let entry = &self.keys[slot as usize];
        entry.marked(WINDOWED | HELD)
    }

    /// Whether a slot was written or released after stamp `stamp`.
    pub(super) fn written_after(&self, stamp: u64) -> bool {
        self.last_written > stamp
    }

    /// The stamp of the latest write or release of a live slot.
    pub(super) fn last_written(&self) -> u64 {
        self.last_written
    }

    /// Whether any of `slots` was written after stamp `since`.
    pub(super) fn written_since(&self, slots: &[KeyIdx], since: u64) -> Freshness {
        let mut freshness = Freshness::Unwritten;
        for slot in slots {
            let Some(entry) = self.keys.get(*slot as usize) else {
                return Freshness::Written;
            };
            if entry.written <= since {
                continue;
            }
            match self.held.get(slot).or_else(|| self.windows.get(slot)) {
                Some(before) if *before <= since => freshness = Freshness::Recomputing,
                _ => return Freshness::Written,
            }
        }
        freshness
    }

    /// Whether a read of `slot` goes into a log whose innermost open frame
    /// began at stamp `frame`: `None` if it was logged since, else whether
    /// this is its first log since the evaluation began at stamp
    /// `evaluation`. Marks it logged at `frame`, and answers whether the key
    /// is a query.
    pub(super) fn log(
        &mut self,
        slot: KeyIdx,
        frame: u64,
        evaluation: u64,
    ) -> (Option<bool>, bool) {
        let entry = &mut self.keys[slot as usize];
        let is_query = entry.is_query();
        if entry.stamp >= frame {
            return (None, is_query);
        }
        let first = entry.stamp < evaluation;
        entry.stamp = frame;
        (Some(first), is_query)
    }
}

/// The slots to release from which [`ReaderGraph::release_keys`] checks them
/// on worker threads.
const PARALLEL_RELEASE: usize = 1 << 12;

/// What [`ReaderGraph::apply_edits`] did to the recorded reads.
#[derive(Debug, Default)]
pub struct AppliedEdits {
    /// The readers of keys the edits destroyed.
    pub lost: Vec<DerivationId>,
    /// The keys whose reads moved, each read still holding the fingerprint
    /// it saw before the edits.
    pub moved: Vec<HashedKey>,
}

#[derive(Debug, Clone, Default)]
pub struct ReaderGraph {
    table: KeyTable,
    /// Each derivation's slot, found by its id's hash: the id itself is kept
    /// only in its slot.
    deriv_slots: HashTable<DerivIdx>,
    derivations: Paged<DerivSlot>,
    free_derivations: Vec<DerivIdx>,
    by_file: FxHashMap<FileId, FxHashSet<DerivIdx>>,
    /// Derivations that produce keys owned by another file, by that file,
    /// with how many such keys each produces. Removing the file's facts
    /// removes their outputs too, so they must run again when it is
    /// re-analysed.
    foreign_producers: FxHashMap<FileId, FxHashMap<DerivIdx, u32>>,
    /// Cached fingerprints taken in an older generation are forgotten.
    generation: u32,
    /// Slots [`Self::file_viewed_part`] left unreferenced, kept until every
    /// viewed record naming the graph as it stood is filed.
    held_releases: Vec<KeyIdx>,
}

impl ReaderGraph {
    pub(crate) fn quarantine_releases(&mut self) -> super::ReleaseQuarantine {
        self.table.quarantine_releases()
    }

    /// Counts and estimated heap sizes of the graph's parts, for the profile.
    pub fn census(&self) -> String {
        use crate::profile::{hash_table_bytes, megabytes};
        use std::mem::size_of;
        let table = &self.table;
        let live = table.keys.iter().filter(|slot| slot.key.is_some()).count();
        let groups: usize = table
            .keys
            .iter()
            .map(|slot| slot.readers.groups().count())
            .sum();
        let edges: usize = table
            .keys
            .iter()
            .map(|slot| slot.readers.iter().count())
            .sum();
        let (group_bytes, set_bytes) = table
            .keys
            .iter()
            .map(|slot| slot.readers.heap_bytes())
            .fold((0, 0), |(groups, sets), (group, set)| {
                (groups + group, sets + set)
            });
        let producer_bytes = hash_table_bytes(
            table.more_producers.capacity(),
            size_of::<(KeyIdx, Vec<DerivIdx>)>(),
        ) + table
            .more_producers
            .values()
            .map(|more| more.capacity() * size_of::<DerivIdx>())
            .sum::<usize>();
        let (reads, outputs) = self
            .derivations
            .iter()
            .filter_map(|slot| slot.record.as_ref())
            .fold((0, 0), |(reads, outputs), record| {
                (
                    reads + record.reads().len(),
                    outputs + record.outputs().len(),
                )
            });
        let key_map = hash_table_bytes(table.key_slots.capacity(), size_of::<FiledKey>());
        let key_slots = table.keys.capacity() * size_of::<KeySlot>();
        let records = (reads + outputs) * size_of::<KeyIdx>();
        let derivation_maps = hash_table_bytes(self.deriv_slots.capacity(), size_of::<DerivIdx>())
            + self.derivations.capacity() * size_of::<DerivSlot>();
        let total = key_map
            + key_slots
            + group_bytes
            + set_bytes
            + producer_bytes
            + records
            + derivation_maps;
        let mut by_variant =
            FxHashMap::<std::mem::Discriminant<FactKey>, (usize, &FactKey)>::default();
        for key in table.keys.iter().filter_map(|slot| slot.key.as_deref()) {
            by_variant
                .entry(std::mem::discriminant(key))
                .or_insert((0, key))
                .0 += 1;
        }
        let mut by_variant = by_variant
            .into_values()
            .map(|(count, sample)| {
                let name = format!("{sample:?}");
                let name = name.split(['(', ' ']).next().unwrap_or_default().to_owned();
                (count, name)
            })
            .collect::<Vec<_>>();
        by_variant.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let top = by_variant
            .iter()
            .take(12)
            .map(|(count, variant)| format!("{variant} {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "keys {} (live {live}; {top}), reader groups {groups}, edges {edges}, derivations {}, record reads {reads}, outputs {outputs}; MB: key map {}, key slots {}, reader groups {}, reader sets {}, producers {}, records {}, derivation maps {}, total ~{}",
            table.keys.len(),
            self.derivations.len(),
            megabytes(key_map),
            megabytes(key_slots),
            megabytes(group_bytes),
            megabytes(set_bytes),
            megabytes(producer_bytes),
            megabytes(records),
            megabytes(derivation_maps),
            megabytes(total)
        )
    }

    /// Lends the key table out, until [`Self::restore_keys`].
    pub fn lend_keys(&mut self) -> KeyTable {
        std::mem::take(&mut self.table)
    }

    pub fn restore_keys(&mut self, table: KeyTable) {
        debug_assert!(self.table.keys.is_empty(), "keys restored twice");
        self.table = table;
    }

    /// Marks `keys` written now: memos that read them no longer hold, and a
    /// window opened on one later restores to this stamp, not an older one.
    pub(crate) fn mark_written<'a>(
        &mut self,
        keys: impl rayon::iter::IntoParallelIterator<Item = &'a HashedKey>,
    ) {
        use rayon::prelude::*;
        let now = super::new_stamp();
        let table = &self.table;
        let mut slots = keys
            .into_par_iter()
            .filter_map(|key| table.find(key))
            .flat_map_iter(|slot| {
                std::iter::once(slot).chain(table.derived_slots(slot).iter().copied())
            })
            .collect::<Vec<_>>();
        self.table.mark_slots_written(&mut slots, now);
    }

    /// Holds `keys` written until [`Self::release_held`]; see
    /// [`KeyTable::hold_window`].
    pub fn hold_windows<'a>(&mut self, keys: impl IntoIterator<Item = &'a HashedKey>) {
        let now = super::new_stamp();
        for key in keys {
            self.table.hold_window(key, now);
        }
    }

    /// See [`KeyTable::release_held`].
    pub fn release_held(&mut self, unchanged: impl Fn(&HashedKey) -> bool) {
        self.table.release_held(unchanged);
    }

    /// `key`'s fingerprint, taken by `fingerprint` once per value: a read
    /// key's slot keeps it until [`Self::forget_fingerprint`] or
    /// [`Self::forget_fingerprints`].
    pub fn current_fingerprint(
        &self,
        key: &HashedKey,
        fingerprint: impl FnOnce(&HashedKey) -> Fingerprint,
    ) -> Fingerprint {
        match self.table.find(key) {
            Some(slot) => self.slot_fingerprint(slot, fingerprint),
            None => fingerprint(key),
        }
    }

    /// The fingerprint of the key in `slot`, cached as
    /// [`Self::current_fingerprint`] caches it.
    pub(crate) fn slot_fingerprint(
        &self,
        slot: KeyIdx,
        fingerprint: impl FnOnce(&HashedKey) -> Fingerprint,
    ) -> Fingerprint {
        let entry = &self.table.keys[slot as usize];
        if entry.current_generation.load(Ordering::Acquire) == self.generation {
            return entry.current.load(Ordering::Relaxed);
        }
        let current = fingerprint(self.key(slot));
        entry.current.store(current, Ordering::Relaxed);
        entry
            .current_generation
            .store(self.generation, Ordering::Release);
        current
    }

    /// Forgets the cached fingerprints of `key` and of every key read out of
    /// it: their values may have moved.
    pub fn forget_fingerprint(&self, key: &HashedKey) {
        if let Some(slot) = self.table.find(key) {
            for slot in std::iter::once(&slot).chain(
                self.table
                    .derived
                    .get(&slot)
                    .into_iter()
                    .flat_map(|derived| &derived.slots),
            ) {
                self.table.keys[*slot as usize]
                    .current_generation
                    .store(NO_GENERATION, Ordering::Relaxed);
            }
        }
    }

    /// The keys read out of `key` ([`FactKey::base`]) that something reads.
    pub fn derived_keys(&self, key: &HashedKey) -> Vec<HashedKey> {
        self.table
            .find(key)
            .and_then(|slot| self.table.derived.get(&slot))
            .into_iter()
            .flat_map(|derived| &derived.slots)
            .map(|slot| self.key(*slot).clone())
            .collect()
    }

    /// Whether any key read out of `key` was ever interned.
    pub fn has_derived_keys(&self, key: &HashedKey) -> bool {
        self.table
            .find(key)
            .is_some_and(|slot| self.table.derived.contains_key(&slot))
    }

    /// The keys read out of `key` whose readers have to be checked now that
    /// `key` fingerprints as `now`: none when they were last checked at that
    /// value, since each is a function of it.
    pub fn derived_keys_to_check(&self, key: &HashedKey, now: Fingerprint) -> Vec<HashedKey> {
        let Some(derived) = self
            .table
            .find(key)
            .and_then(|slot| self.table.derived.get(&slot))
        else {
            return Vec::new();
        };
        if derived.checked_at.swap(now, Ordering::Relaxed) == now {
            return Vec::new();
        }
        derived
            .slots
            .iter()
            .map(|slot| self.key(*slot).clone())
            .collect()
    }

    /// Every derivation with a record, sorted.
    pub(crate) fn recorded_derivations(&self) -> Vec<DerivationId> {
        let mut ids = self
            .derivations
            .iter()
            .filter(|slot| slot.record.is_some())
            .map(|slot| slot.id)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    /// Has the next [`Self::derived_keys_to_check`] of each of `keys` check
    /// its derived keys whatever the base's fingerprint.
    pub(crate) fn uncheck_derived<'a>(
        &self,
        keys: impl rayon::iter::IntoParallelIterator<Item = &'a HashedKey>,
    ) {
        use rayon::prelude::*;
        keys.into_par_iter().for_each(|key| {
            if let Some(derived) = self
                .table
                .find(key)
                .and_then(|slot| self.table.derived.get(&slot))
            {
                derived.checked_at.store(UNCHECKED, Ordering::Relaxed);
            }
        });
    }

    /// Forgets every cached fingerprint.
    pub fn forget_fingerprints(&mut self) {
        self.generation = (self.generation + 1) % NO_GENERATION;
    }

    /// Every key whose fingerprint is cached, with it.
    pub fn cached_fingerprints(&self) -> impl Iterator<Item = (&HashedKey, Fingerprint)> {
        self.table.keys.iter().filter_map(|slot| {
            (slot.current_generation.load(Ordering::Acquire) == self.generation)
                .then(|| Some((slot.key.as_ref()?, slot.current.load(Ordering::Relaxed))))?
        })
    }

    /// Replaces `id`'s record with the reads and outputs of its latest
    /// evaluation. Returns the outputs the previous evaluation produced and
    /// this one did not.
    pub fn replace(
        &mut self,
        id: DerivationId,
        reads: Vec<(HashedKey, Option<Fingerprint>)>,
        outputs: Vec<HashedKey>,
        fingerprint: impl Fn(&HashedKey) -> Fingerprint,
    ) -> Vec<HashedKey> {
        let now = super::new_stamp();
        let slots = reads
            .into_iter()
            .map(|(key, seen)| (self.intern_key(key, now), seen))
            .collect();
        let mut released = Vec::new();
        let dropped = self.replace_slots(id, slots, outputs, fingerprint, &mut released, now);
        self.release_keys(released);
        dropped
    }

    /// Files each of `records`, reads made viewing this graph as it stands, as
    /// its derivation's record, then frees the slots none of them keeps. The
    /// new keys are interned in order, and each record is kept and diffed on
    /// worker threads.
    pub fn file_viewed(&mut self, records: Vec<(DerivationId, super::ViewedReads)>) {
        let mut released = Vec::new();
        self.file_viewed_into(records, &mut released);
        self.release_keys(released);
    }

    /// [`Self::file_viewed`], the slots it would free added to `released`
    /// instead, for the caller to free with [`Self::release_keys`] once the
    /// records it still holds, which may name them, are filed too.
    pub fn file_viewed_into(
        &mut self,
        records: Vec<(DerivationId, super::ViewedReads)>,
        released: &mut Vec<KeyIdx>,
    ) {
        released.append(&mut self.held_releases);
        self.keep_viewed(records, released);
    }

    /// [`Self::file_viewed`] for some of the records viewing this graph, the
    /// slots it leaves unreferenced kept for the rest, which may still name
    /// them, until [`Self::file_viewed`] files those.
    pub fn file_viewed_part(&mut self, records: Vec<(DerivationId, super::ViewedReads)>) {
        let mut released = std::mem::take(&mut self.held_releases);
        self.keep_viewed(records, &mut released);
        self.held_releases = released;
    }

    fn keep_viewed(
        &mut self,
        records: Vec<(DerivationId, super::ViewedReads)>,
        released: &mut Vec<KeyIdx>,
    ) {
        use rayon::prelude::*;
        let now = super::new_stamp();
        let records = records
            .into_iter()
            .map(|(id, reads)| {
                let (mut slots, keys) = reads.into_parts();
                slots.extend(
                    keys.into_iter()
                        .map(|(key, seen)| (self.intern_key(key, now), seen)),
                );
                (id, slots)
            })
            .collect::<Vec<_>>();
        let table = &self.table;
        let records = records
            .into_par_iter()
            .map(|(id, slots)| table.keep_viewed(id, slots))
            .collect();
        self.replace_all_kept(records, released, || now);
    }

    /// Frees each slot of `slots` nothing refers to any more. A large batch
    /// is first cut, on worker threads, to the slots that are live and
    /// unreferenced: a slot referenced now is freed only along with its last
    /// derived key, so its own turn changes nothing.
    pub fn release_keys(&mut self, slots: Vec<KeyIdx>) {
        use rayon::prelude::*;
        let slots = if slots.len() < PARALLEL_RELEASE {
            slots
        } else {
            let table = &self.table;
            slots
                .into_par_iter()
                .filter(|slot| table.live_key(*slot).is_some() && table.unreferenced(*slot))
                .collect()
        };
        self.release_slots(slots, &FxHashMap::default());
    }

    /// [`Self::replace`] with the reads already interned at `now`. The slots
    /// of reads the record no longer keeps (see [`super::records_read`]) go to
    /// `released`, to free with [`Self::release_keys`] once every record of
    /// the group sharing the lent table is replaced: another member may hold
    /// the same slot.
    pub fn replace_slots(
        &mut self,
        id: DerivationId,
        slots: Vec<(KeyIdx, Option<Fingerprint>)>,
        outputs: Vec<HashedKey>,
        fingerprint: impl Fn(&HashedKey) -> Fingerprint,
        released: &mut Vec<KeyIdx>,
        now: u64,
    ) -> Vec<HashedKey> {
        let output_set = outputs.iter().collect::<FxHashSet<_>>();
        let (reads, dropped_reads) = kept_reads(
            id,
            slots,
            |slot| self.key(slot),
            |key| output_set.contains(key),
        );
        drop(output_set);
        let reads = reads
            .into_iter()
            .map(|(slot, seen)| {
                let seen = seen.unwrap_or_else(|| self.slot_fingerprint(slot, &fingerprint));
                (slot, seen)
            })
            .collect();
        self.replace_kept(
            KeptRecord {
                id,
                reads,
                dropped_reads,
                outputs: Vec::new(),
                new_outputs: outputs,
            },
            released,
            now,
        )
    }

    /// [`Self::replace_slots`] with a record [`KeyTable::keep_record`]
    /// prepared.
    pub fn replace_kept(
        &mut self,
        record: KeptRecord,
        released: &mut Vec<KeyIdx>,
        now: u64,
    ) -> Vec<HashedKey> {
        let deriv = self.intern_derivation(record.id);
        let prepared = self.prepare_kept(record, deriv, now);
        let diffed = prepared.diff(
            self.derivations[deriv as usize].record.as_ref(),
            Some(&self.table),
        );
        let (dropped, ops) = self.finish_kept(diffed, released);
        for (slot, op) in ops {
            op.apply(&mut self.table.keys[slot as usize].readers);
        }
        dropped
    }

    /// [`Self::replace_kept`] for each of `records` in order, at the stamp
    /// `stamp` gives each. Each record is diffed against the one it replaces
    /// on worker threads, unless the batch files one derivation twice, and
    /// the readers of the keys they read are updated a page of slots at a
    /// time on worker threads.
    pub fn replace_all_kept(
        &mut self,
        records: Vec<KeptRecord>,
        released: &mut Vec<KeyIdx>,
        mut stamp: impl FnMut() -> u64,
    ) {
        use rayon::prelude::*;
        let mut ids = FxHashSet::with_capacity_and_hasher(records.len(), Default::default());
        let distinct = records.iter().all(|record| ids.insert(record.id));
        drop(ids);
        let mut ops = Vec::with_capacity(records.len());
        if distinct {
            let known = records
                .par_iter()
                .map(|record| self.deriv_slot(&record.id))
                .collect::<Vec<_>>();
            let prepared = records
                .into_iter()
                .zip(known)
                .map(|(record, known)| {
                    let now = stamp();
                    let deriv = known.unwrap_or_else(|| self.new_derivation(record.id));
                    self.prepare_kept(record, deriv, now)
                })
                .collect::<Vec<_>>();
            let (derivations, table) = (&self.derivations, &self.table);
            let diffed = prepared
                .into_par_iter()
                .map(|prepared| {
                    let old = derivations[prepared.deriv as usize].record.as_ref();
                    prepared.diff(old, Some(table))
                })
                .collect::<Vec<_>>();
            for diffed in diffed {
                ops.push(self.finish_kept(diffed, released).1);
            }
        } else {
            for record in records {
                // A later record of the same derivation diffs against
                // readers the earlier one's ops have yet to move.
                let now = stamp();
                let deriv = self.intern_derivation(record.id);
                let prepared = self.prepare_kept(record, deriv, now);
                let diffed = prepared.diff(self.derivations[deriv as usize].record.as_ref(), None);
                ops.push(self.finish_kept(diffed, released).1);
            }
        }
        self.table.apply_reader_ops(ops);
    }

    /// Interns the outputs of `record`, which is `deriv`'s, at `now`.
    fn prepare_kept(&mut self, record: KeptRecord, deriv: DerivIdx, now: u64) -> PreparedRecord {
        let KeptRecord {
            id,
            reads,
            dropped_reads,
            mut outputs,
            new_outputs,
        } = record;
        if !new_outputs.is_empty() {
            outputs.extend(new_outputs.into_iter().map(|key| self.intern_key(key, now)));
            outputs.sort_unstable();
            outputs.dedup();
        }
        PreparedRecord {
            id,
            deriv,
            reads,
            dropped_reads,
            outputs,
        }
    }

    /// Files what diffing a record found but for the changes to its reads'
    /// readers, which it returns in the order they apply, with the facts the
    /// record no longer produces.
    fn finish_kept(
        &mut self,
        diffed: DiffedRecord,
        released: &mut Vec<KeyIdx>,
    ) -> (Vec<HashedKey>, Vec<(KeyIdx, ReaderOp)>) {
        let DiffedRecord {
            id,
            deriv,
            ops,
            unread,
            dropped_reads,
            added,
            dropped,
            record,
        } = diffed;
        for key in added {
            self.produce(key, deriv, id.file_id);
        }
        let dropped = dropped
            .into_iter()
            .map(|key| {
                let fact = self.key(key).clone();
                self.unproduce(key, deriv, id.file_id);
                released.push(key);
                fact
            })
            .collect();
        // Only now: a key this evaluation stopped reading may be one it
        // started producing.
        released.extend(unread);
        released.extend(dropped_reads);
        if let Some(record) = record
            && self.derivations[deriv as usize]
                .record
                .replace(record)
                .is_none()
        {
            self.by_file.entry(id.file_id).or_default().insert(deriv);
        }
        (dropped, ops)
    }

    /// Forgets every derivation of `file_ids`, returning each with the facts
    /// it produced, in derivation order.
    pub fn remove_files(&mut self, file_ids: &[FileId]) -> Vec<(DerivationId, Box<[HashedKey]>)> {
        use rayon::prelude::*;
        let derivs = file_ids
            .iter()
            .filter_map(|file_id| self.by_file.remove(file_id))
            .flatten()
            .collect::<Vec<_>>();
        // Each key's readers lose every derivation of the files at once. A key
        // a derivation detached later still reads stays filed until then, as
        // detaching them one at a time leaves it.
        let mut leaving = derivs
            .iter()
            .flat_map(|deriv| {
                self.derivations[*deriv as usize]
                    .record
                    .iter()
                    .flat_map(|record| record.reads().iter())
                    .map(|key| (*key, *deriv))
            })
            .collect::<Vec<_>>();
        leaving.par_sort_unstable();
        let mut still_read = leaving
            .chunk_by(|(left, _), (right, _)| left == right)
            .map(|same_key| (same_key[0].0, same_key.len()))
            .collect::<FxHashMap<_, _>>();
        let ops = leaving
            .into_par_iter()
            .map(|(slot, deriv)| (slot, ReaderOp::Remove(deriv)))
            .collect();
        self.table.apply_reader_ops(vec![ops]);
        let mut removed: Vec<_> = derivs
            .into_iter()
            .map(|deriv| {
                let id = self.derivations[deriv as usize].id;
                (id, self.detach(deriv, &mut still_read))
            })
            .collect();
        removed.sort_by_key(|entry| entry.0);
        removed
    }

    /// The derivations of other files that produced a key owned by `file_id`.
    pub fn foreign_producers(&self, file_id: FileId) -> Vec<DerivationId> {
        let mut ids: Vec<_> = self
            .foreign_producers
            .get(&file_id)
            .into_iter()
            .flat_map(FxHashMap::keys)
            .map(|deriv| self.derivations[*deriv as usize].id)
            .collect();
        ids.sort();
        ids
    }

    /// The derivations whose recorded read of `key` no longer holds:
    /// `holds(seen)` says whether the value a reader saw is still current.
    pub fn stale_readers<'a>(
        &'a self,
        key: &HashedKey,
        holds: impl Fn(Fingerprint) -> bool + 'a,
    ) -> impl Iterator<Item = DerivationId> + 'a {
        self.stale_readers_seen(key, holds).map(|(id, _)| id)
    }

    /// [`Self::stale_readers`], each with the fingerprint it saw.
    pub fn stale_readers_seen<'a>(
        &'a self,
        key: &HashedKey,
        holds: impl Fn(Fingerprint) -> bool + 'a,
    ) -> impl Iterator<Item = (DerivationId, Fingerprint)> + 'a {
        self.table
            .find(key)
            .map(|slot| self.table.keys[slot as usize].readers.groups())
            .into_iter()
            .flatten()
            .filter(move |(seen, _)| !holds(*seen))
            .flat_map(|(seen, derivs)| derivs.iter().map(move |deriv| (deriv, seen)))
            .map(|(deriv, seen)| (self.derivations[*deriv as usize].id, seen))
    }

    /// Takes, on worker threads, the fingerprint of every key the records of
    /// `ids` read, so the checks that follow find each cached. Query outputs
    /// are left to the checks, as taking one may compute the query.
    pub fn warm_reads(
        &self,
        ids: impl Iterator<Item = DerivationId>,
        fingerprint: impl Fn(&HashedKey) -> Fingerprint + Sync,
    ) {
        let slots = ids
            .filter_map(|id| self.record(id))
            .flat_map(|(_, record)| record.reads().iter().copied())
            .collect();
        self.warm_slots(slots, fingerprint);
    }

    fn warm_slots(
        &self,
        mut slots: Vec<KeyIdx>,
        fingerprint: impl Fn(&HashedKey) -> Fingerprint + Sync,
    ) {
        use rayon::prelude::*;
        slots.retain(|slot| !self.table.is_query(*slot));
        slots.sort_unstable();
        slots.dedup();
        slots.par_iter().for_each(|slot| {
            self.slot_fingerprint(*slot, &fingerprint);
        });
    }

    /// Whether any read `id` last made no longer holds: it saw another
    /// fingerprint than the key's current one, and `holds_moved` does not
    /// excuse the difference.
    pub fn is_stale(
        &self,
        id: DerivationId,
        fingerprint: impl Fn(&HashedKey) -> Fingerprint,
        holds_moved: impl Fn(&HashedKey, Fingerprint) -> bool,
    ) -> bool {
        let Some((deriv, record)) = self.record(id) else {
            return true;
        };
        record.reads().iter().any(|slot| {
            let seen = self.seen(*slot, deriv);
            seen != self.slot_fingerprint(*slot, &fingerprint)
                && !holds_moved(self.key(*slot), seen)
        })
    }

    /// For `GLUALS_CHECK_CACHE`: `id`'s recorded reads by slot.
    pub fn recorded_slots(&self, id: DerivationId) -> Vec<(KeyIdx, Fingerprint)> {
        self.record(id)
            .map(|(deriv, record)| {
                record
                    .reads()
                    .iter()
                    .map(|slot| (*slot, self.seen(*slot, deriv)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The keys `id` last read.
    pub fn reads_of(&self, id: DerivationId) -> impl Iterator<Item = &HashedKey> + '_ {
        self.record(id)
            .into_iter()
            .flat_map(|(_, record)| record.reads().iter())
            .map(|key| self.key(*key))
    }

    /// Each of `id`'s recorded reads whose fingerprint differs from the one
    /// `fingerprints` gives it, in [`Self::reads_of`] order, with that one.
    pub fn moved_fingerprints(
        &self,
        id: DerivationId,
        fingerprints: Vec<Fingerprint>,
    ) -> Vec<(KeyIdx, Fingerprint)> {
        let Some((deriv, record)) = self.record(id) else {
            return Vec::new();
        };
        record
            .reads()
            .iter()
            .zip(fingerprints)
            .filter(|(slot, fingerprint)| {
                let entry = &self.table.keys[**slot as usize];
                // A stage rule a reader saw is the stage it ran at, which only
                // a stage barrier re-runs it for.
                entry.readers.seen_by(deriv) != *fingerprint
                    && !entry.key.as_ref().is_some_and(|key| key.is_stage_rule())
            })
            .map(|(slot, fingerprint)| (*slot, fingerprint))
            .collect()
    }

    /// Records that `id` saw each fingerprint of `moved` of its slot.
    pub fn set_fingerprints(&mut self, id: DerivationId, moved: Vec<(KeyIdx, Fingerprint)>) {
        let Some(deriv) = self.deriv_slot(&id) else {
            return;
        };
        for (slot, fingerprint) in moved {
            self.table.keys[slot as usize]
                .readers
                .insert(deriv, fingerprint);
        }
    }

    /// Whether some reader of `key` saw a value other than the one
    /// `current` fingerprints, taken only if something reads `key`.
    pub fn read_other_than(&self, key: &HashedKey, current: impl FnOnce() -> Fingerprint) -> bool {
        let Some(slot) = self.table.find(key) else {
            return false;
        };
        let readers = &self.table.keys[slot as usize].readers;
        if readers.is_empty() {
            return false;
        }
        let current = current();
        readers.groups().any(|(seen, _)| seen != current)
    }

    /// Relabels the reads of `key` that saw `from` as having seen `to`: after
    /// an edit that only moved the key's positions, what they read is the
    /// value as it stands now, and the next edit maps back from here.
    pub fn rebase_readers(&mut self, key: &HashedKey, from: Fingerprint, to: Fingerprint) {
        if let Some(slot) = self.table.find(key) {
            self.table.keys[slot as usize].readers.relabel(from, to);
        }
    }

    /// Renames every recorded read and write of a key `edits` moved to the
    /// key's new position, keeping the fingerprint the reader saw.
    pub fn apply_edits(&mut self, edits: &EditMaps) -> AppliedEdits {
        use rayon::prelude::*;
        let files: FxHashSet<FileId> = edits.files().collect();
        let positional = crate::profile::phase("rename reads: positional slots", || {
            self.table.positional_slots(&files)
        });
        let mapped = crate::profile::phase("rename reads: map keys", || {
            let keys = &self.table.keys;
            positional
                .par_iter()
                .filter_map(|slot| {
                    let entry = &keys[*slot as usize];
                    let key = entry
                        .key
                        .as_ref()
                        .filter(|_| !entry.readers.is_empty() || entry.producer() != NO_PRODUCER)?;
                    match edits.map_key(key) {
                        Some(moved) if moved == **key => None,
                        moved => Some((*slot, moved.map(HashedKey::new))),
                    }
                })
                .collect::<Vec<_>>()
        });
        let mapped_keys = mapped.len();
        let mut sources = Vec::new();
        let mut new_keys = Vec::new();
        let mut lost = Vec::new();
        for (slot, moved) in mapped {
            match moved {
                Some(moved) => {
                    sources.push(slot);
                    new_keys.push(moved);
                }
                None => lost.extend(
                    self.table.keys[slot as usize]
                        .readers
                        .iter()
                        .map(|(_, deriv)| self.derivations[deriv as usize].id),
                ),
            }
        }
        lost.sort();
        lost.dedup();
        let moved_to = new_keys.par_iter().cloned().collect::<Vec<_>>();
        // Every moved key gives up its readers and producers before any takes
        // new ones: positions shift in chains, where one key's new place is
        // another's old one.
        let taken = sources
            .iter()
            .map(|slot| std::mem::take(&mut self.table.keys[*slot as usize].readers))
            .collect::<Vec<_>>();
        let produced = sources
            .iter()
            .map(|slot| {
                let producers = self.table.producers(*slot).collect::<Vec<_>>();
                for deriv in &producers {
                    self.unproduce(*slot, *deriv, self.derivations[*deriv as usize].id.file_id);
                }
                producers
            })
            .collect::<Vec<_>>();
        let now = super::new_stamp();
        let targets = crate::profile::phase("rename reads: intern moved keys", || {
            new_keys
                .into_iter()
                .map(|key| self.intern_key(key, now))
                .collect::<Vec<_>>()
        });
        let (edges, rewritten) = crate::profile::phase("rename reads: rewrite records", || {
            let mut moves = sources
                .par_iter()
                .zip(&targets)
                .zip(&taken)
                .flat_map_iter(|((source, target), readers)| {
                    let (source, target) = (*source, *target);
                    readers
                        .iter()
                        .map(move |(_, deriv)| (deriv, source, target))
                })
                .collect::<Vec<_>>();
            moves.par_sort_unstable();
            let derivations = &self.derivations;
            let rewritten = moves
                .par_chunk_by(|left, right| left.0 == right.0)
                .filter_map(|group| {
                    let deriv = group[0].0;
                    let record = derivations[deriv as usize].record.as_ref()?;
                    let shifts = group
                        .iter()
                        .map(|&(_, source, target)| (source, target))
                        .collect::<Vec<_>>();
                    Some((deriv, renamed_slots(record.reads(), &shifts)))
                })
                .collect::<Vec<_>>();
            (moves.len(), rewritten)
        });
        let mut output_moves = sources
            .iter()
            .zip(&targets)
            .zip(&produced)
            .flat_map(|((source, target), producers)| {
                producers
                    .iter()
                    .map(move |deriv| (*deriv, *source, *target))
            })
            .collect::<Vec<_>>();
        output_moves.sort_unstable();
        for group in output_moves.chunk_by(|left, right| left.0 == right.0) {
            let deriv = group[0].0;
            let shifts = group
                .iter()
                .map(|&(_, source, target)| (source, target))
                .collect::<Vec<_>>();
            if let Some(record) = self.derivations[deriv as usize].record.as_mut() {
                *record = record.with_outputs(&renamed_slots(record.outputs(), &shifts));
            }
        }
        for (target, producers) in targets.iter().zip(produced) {
            for deriv in producers {
                self.produce(*target, deriv, self.derivations[deriv as usize].id.file_id);
            }
        }
        let rewrite_stats = crate::profile::phase_enabled().then(|| {
            let reads = rewritten
                .iter()
                .map(|(_, reads)| reads.len())
                .sum::<usize>();
            (rewritten.len(), reads)
        });
        for (deriv, reads) in rewritten {
            if let Some(record) = self.derivations[deriv as usize].record.as_mut() {
                *record = record.with_reads(&reads);
            }
        }
        let moved = targets.len();
        crate::profile::phase("rename reads: file readers", || {
            for (target, readers) in targets.into_iter().zip(taken) {
                let filed = &mut self.table.keys[target as usize].readers;
                if filed.is_empty() {
                    *filed = readers;
                } else {
                    for (seen, deriv) in readers.iter() {
                        filed.insert(deriv, seen);
                    }
                }
            }
        });
        crate::profile::phase("rename reads: release vacated", || {
            self.release_keys(sources)
        });
        if let Some((records, reads)) = rewrite_stats {
            eprintln!(
                "[phase] rename reads: {} positional keys, {moved} moved, {} lost, {edges} reader edges, {records} records rewritten over {reads} reads, {} key slots ({} free)",
                positional.len(),
                mapped_keys - moved,
                self.table.keys.len(),
                self.table.free_keys.len()
            );
        }
        AppliedEdits {
            lost,
            moved: moved_to,
        }
    }

    /// Every key some derivation read.
    #[cfg(test)]
    pub fn read_keys(&self) -> impl Iterator<Item = &FactKey> {
        self.table
            .keys
            .iter()
            .filter(|slot| !slot.readers.is_empty())
            .filter_map(|slot| slot.key.as_deref())
    }

    /// Every derivation with a record.
    #[cfg(test)]
    pub fn derivations(&self) -> impl Iterator<Item = DerivationId> + '_ {
        self.derivations
            .iter()
            .filter(|slot| slot.record.is_some())
            .map(|slot| slot.id)
    }

    /// The derivations of the kinds `follows` accepts that read one of `keys`,
    /// or read, through what such derivations write, what one of them writes.
    pub fn readers_downstream<'a>(
        &self,
        keys: impl IntoIterator<Item = &'a HashedKey>,
        follows: impl Fn(DerivationKind) -> bool,
    ) -> FxHashSet<DerivationId> {
        let mut marked = vec![false; self.derivations.len()];
        let mut pending = Vec::new();
        for slot in keys.into_iter().filter_map(|key| self.table.find(key)) {
            self.mark_readers(slot, &mut marked, &mut pending, &follows);
        }
        self.mark_downstream(&mut marked, pending, &follows);
        marked
            .iter()
            .enumerate()
            .filter(|(_, marked)| **marked)
            .map(|(deriv, _)| self.derivations[deriv].id)
            .collect()
    }

    /// Marks, from the marked derivations in `pending`, every derivation of
    /// the kinds `follows` accepts that reads what a marked one writes.
    fn mark_downstream(
        &self,
        marked: &mut [bool],
        mut pending: Vec<DerivIdx>,
        follows: &impl Fn(DerivationKind) -> bool,
    ) {
        while let Some(deriv) = pending.pop() {
            let Some(record) = self.derivations[deriv as usize].record.as_ref() else {
                continue;
            };
            for output in record.outputs().iter() {
                self.mark_readers(*output, marked, &mut pending, follows);
            }
        }
    }

    /// Marks the unmarked readers of `slot`, and of the keys read out of it,
    /// of the kinds `follows` accepts, and adds them to `pending`.
    fn mark_readers(
        &self,
        slot: KeyIdx,
        marked: &mut [bool],
        pending: &mut Vec<DerivIdx>,
        follows: &impl Fn(DerivationKind) -> bool,
    ) {
        let derived = self
            .table
            .derived
            .get(&slot)
            .map_or(&[][..], |derived| derived.slots.as_slice());
        for slot in std::iter::once(&slot).chain(derived) {
            for (_, readers) in self.table.keys[*slot as usize].readers.groups() {
                for reader in readers {
                    if !marked[*reader as usize]
                        && follows(self.derivations[*reader as usize].id.kind)
                    {
                        marked[*reader as usize] = true;
                        pending.push(*reader);
                    }
                }
            }
        }
    }

    /// The loops through `roots` among the derivations of the kinds `follows`
    /// accepts that `selects`, and that read, through what they read, from no
    /// other loop it selects: each set of two or more of them that read,
    /// through one another, what each writes, members sorted.
    pub fn read_loops(
        &self,
        roots: impl IntoIterator<Item = DerivationId>,
        follows: impl Fn(DerivationKind) -> bool,
        mut selects: impl FnMut(&[DerivationId]) -> bool,
    ) -> Vec<Vec<DerivationId>> {
        // Tarjan's algorithm along reads, from each root not yet visited:
        // `order` holds each visited derivation's index and lowest reachable
        // index.
        struct Visit {
            deriv: DerivIdx,
            producers: Vec<DerivIdx>,
            next: usize,
        }
        // Each key slot's base slot, found once per search: `NO_BASE` for a
        // key without one, `UNSEEN` until looked up.
        const UNSEEN: KeyIdx = KeyIdx::MAX;
        const NO_BASE: KeyIdx = KeyIdx::MAX - 1;
        let mut base_slots = vec![UNSEEN; self.table.keys.len()];
        const UNVISITED: u32 = u32::MAX;
        let count = self.derivations.len();
        let roots = roots
            .into_iter()
            .filter_map(|root| self.deriv_slot(&root))
            .collect::<Vec<_>>();
        // A loop through a root, and whatever lies between two such loops,
        // is downstream of a root: the search keeps to what is.
        let mut downstream_of_roots = vec![false; count];
        let mut pending = Vec::new();
        for root in &roots {
            if !std::mem::replace(&mut downstream_of_roots[*root as usize], true) {
                pending.push(*root);
            }
        }
        self.mark_downstream(&mut downstream_of_roots, pending, &follows);
        let mut producers_read_by = |deriv: DerivIdx| {
            let mut producers = Vec::new();
            let Some(record) = self.derivations[deriv as usize].record.as_ref() else {
                return producers;
            };
            for slot in record.reads().iter() {
                let base_slot = &mut base_slots[*slot as usize];
                if *base_slot == UNSEEN {
                    *base_slot = self
                        .table
                        .key(*slot)
                        .base()
                        .and_then(|base| self.table.find(&HashedKey::new(base)))
                        .unwrap_or(NO_BASE);
                }
                let base = (*base_slot != NO_BASE).then_some(*base_slot);
                for slot in std::iter::once(*slot).chain(base) {
                    producers.extend(self.table.producers(slot).filter(|producer| {
                        *producer != deriv && downstream_of_roots[*producer as usize]
                    }));
                }
            }
            producers.sort_unstable();
            producers.dedup();
            producers
        };
        // Each visited derivation's index and lowest reachable index.
        let mut order = vec![(UNVISITED, UNVISITED); count];
        let mut next_index = 0;
        let mut stack = Vec::new();
        let mut on_stack = vec![false; count];
        let mut loops = Vec::new();
        // Each finished derivation's component, and whether a selected loop
        // is in or upstream of it. A component finishes after every one it
        // reads from, so theirs are known by then: a derivation that reads
        // from a finished component one is in or below is marked when it
        // does.
        let mut component_of = vec![usize::MAX; count];
        let mut downstream = Vec::<bool>::new();
        let mut reads_selected = vec![false; count];
        for root in roots {
            if order[root as usize].0 != UNVISITED {
                continue;
            }
            let mut visit = |deriv: DerivIdx,
                             order: &mut [(u32, u32)],
                             next_index: &mut u32,
                             stack: &mut Vec<DerivIdx>,
                             on_stack: &mut [bool]| {
                order[deriv as usize] = (*next_index, *next_index);
                *next_index += 1;
                stack.push(deriv);
                on_stack[deriv as usize] = true;
                Visit {
                    deriv,
                    producers: producers_read_by(deriv),
                    next: 0,
                }
            };
            let mut calls = vec![visit(
                root,
                &mut order,
                &mut next_index,
                &mut stack,
                &mut on_stack,
            )];
            while let Some(top) = calls.last_mut() {
                if let Some(producer) = top.producers.get(top.next).copied() {
                    top.next += 1;
                    let reader = top.deriv as usize;
                    let producer_index = producer as usize;
                    if order[producer_index].0 == UNVISITED {
                        let call = visit(
                            producer,
                            &mut order,
                            &mut next_index,
                            &mut stack,
                            &mut on_stack,
                        );
                        calls.push(call);
                    } else if on_stack[producer_index] {
                        order[reader].1 = order[reader].1.min(order[producer_index].0);
                    } else if downstream[component_of[producer_index]] {
                        reads_selected[reader] = true;
                    }
                    continue;
                }
                let finished = calls.pop().expect("a call");
                let (index, low) = order[finished.deriv as usize];
                if let Some(parent) = calls.last() {
                    let parent_low = &mut order[parent.deriv as usize].1;
                    *parent_low = (*parent_low).min(low);
                }
                if index != low {
                    continue;
                }
                let mut members = Vec::new();
                loop {
                    let member = stack.pop().expect("on the stack");
                    on_stack[member as usize] = false;
                    members.push(member);
                    if member == finished.deriv {
                        break;
                    }
                }
                let component_index = downstream.len();
                for member in &members {
                    component_of[*member as usize] = component_index;
                }
                let upstream_selected = members
                    .iter()
                    .any(|member| reads_selected[*member as usize]);
                let mut component = members
                    .iter()
                    .map(|member| self.derivations[*member as usize].id)
                    .collect::<Vec<_>>();
                component.sort_unstable();
                let selected = component.len() > 1 && selects(&component);
                downstream.push(upstream_selected || selected);
                if selected && !upstream_selected {
                    loops.push(component);
                }
                if (upstream_selected || selected)
                    && let Some(parent) = calls.last()
                {
                    reads_selected[parent.deriv as usize] = true;
                }
            }
        }
        loops.sort_unstable();
        loops
    }

    /// Of `loops`, each with a stamp, the latest stamp of the others each
    /// reads from, through keys among `moved` and derivations of the kinds
    /// `follows` accepts, that do not read from it in turn. The search keeps
    /// to what the loops' moved outputs reach.
    pub fn latest_upstream_loops(
        &self,
        loops: &[(&[DerivationId], usize)],
        moved: &FxHashSet<HashedKey>,
        follows: impl Fn(DerivationKind) -> bool,
    ) -> Vec<Option<usize>> {
        if loops.len() < 2 {
            return vec![None; loops.len()];
        }
        let moved = moved
            .iter()
            .filter_map(|key| self.table.find(key))
            .collect::<FxHashSet<_>>();
        // Each loop is one node, numbered by its position; each derivation
        // its moved outputs reach is another, numbered after them.
        let mut node_of = FxHashMap::<DerivIdx, u32>::default();
        let mut derivs_of = Vec::<Vec<DerivIdx>>::with_capacity(loops.len());
        for (lp, _) in loops {
            let node = u32::try_from(derivs_of.len()).expect("fewer than 2^32 loops");
            let members = lp
                .iter()
                .filter_map(|id| self.deriv_slot(id))
                .collect::<Vec<_>>();
            for member in &members {
                node_of.insert(*member, node);
            }
            derivs_of.push(members);
        }
        let mut pending = derivs_of.iter().flatten().copied().collect::<Vec<_>>();
        while let Some(deriv) = pending.pop() {
            let Some(record) = self.derivations[deriv as usize].record.as_ref() else {
                continue;
            };
            for output in record.outputs().iter().filter(|slot| moved.contains(*slot)) {
                for slot in std::iter::once(output).chain(self.table.derived_slots(*output)) {
                    for (_, reader) in self.table.keys[*slot as usize].readers.iter() {
                        if node_of.contains_key(&reader)
                            || !follows(self.derivations[reader as usize].id.kind)
                        {
                            continue;
                        }
                        let node = u32::try_from(derivs_of.len()).expect("fewer than 2^32 nodes");
                        node_of.insert(reader, node);
                        derivs_of.push(vec![reader]);
                        pending.push(reader);
                    }
                }
            }
        }
        let producers_read_by = |node: u32| {
            let mut producers = Vec::new();
            for deriv in &derivs_of[node as usize] {
                let Some(record) = self.derivations[*deriv as usize].record.as_ref() else {
                    continue;
                };
                for slot in record.reads().iter().filter(|slot| moved.contains(*slot)) {
                    let base = self
                        .table
                        .key(*slot)
                        .base()
                        .and_then(|base| self.table.find(&HashedKey::new(base)));
                    for slot in std::iter::once(*slot).chain(base) {
                        producers.extend(
                            self.table
                                .producers(slot)
                                .filter_map(|producer| node_of.get(&producer).copied())
                                .filter(|producer| *producer != node),
                        );
                    }
                }
            }
            producers.sort_unstable();
            producers.dedup();
            producers
        };
        // Tarjan's algorithm along reads from each loop, as in
        // [`Self::read_loops`]: each component carries the latest stamp of the
        // loops in or upstream of it, and each loop the latest its component
        // reads from another.
        struct Visit {
            node: u32,
            producers: Vec<u32>,
            next: usize,
        }
        const UNVISITED: u32 = u32::MAX;
        let count = derivs_of.len();
        let mut order = vec![(UNVISITED, UNVISITED); count];
        let mut next_index = 0;
        let mut stack = Vec::new();
        let mut on_stack = vec![false; count];
        let mut component_of = vec![usize::MAX; count];
        let mut latest = Vec::<Option<usize>>::new();
        let mut reads_latest = vec![None::<usize>; count];
        let mut upstream = vec![None; loops.len()];
        let mut visit = |node: u32, order: &mut [(u32, u32)], stack: &mut Vec<u32>| {
            order[node as usize] = (next_index, next_index);
            next_index += 1;
            stack.push(node);
            Visit {
                node,
                producers: producers_read_by(node),
                next: 0,
            }
        };
        for root in 0..u32::try_from(loops.len()).expect("fewer than 2^32 loops") {
            if order[root as usize].0 != UNVISITED {
                continue;
            }
            on_stack[root as usize] = true;
            let mut calls = vec![visit(root, &mut order, &mut stack)];
            while let Some(top) = calls.last_mut() {
                if let Some(producer) = top.producers.get(top.next).copied() {
                    top.next += 1;
                    let reader = top.node as usize;
                    let at = producer as usize;
                    if order[at].0 == UNVISITED {
                        on_stack[at] = true;
                        let call = visit(producer, &mut order, &mut stack);
                        calls.push(call);
                    } else if on_stack[at] {
                        order[reader].1 = order[reader].1.min(order[at].0);
                    } else {
                        reads_latest[reader] = reads_latest[reader].max(latest[component_of[at]]);
                    }
                    continue;
                }
                let finished = calls.pop().expect("a call");
                let (index, low) = order[finished.node as usize];
                if let Some(parent) = calls.last() {
                    let parent_low = &mut order[parent.node as usize].1;
                    *parent_low = (*parent_low).min(low);
                }
                if index != low {
                    continue;
                }
                let mut members = Vec::new();
                loop {
                    let member = stack.pop().expect("on the stack");
                    on_stack[member as usize] = false;
                    members.push(member);
                    if member == finished.node {
                        break;
                    }
                }
                let component = latest.len();
                for member in &members {
                    component_of[*member as usize] = component;
                }
                let read = members
                    .iter()
                    .map(|member| reads_latest[*member as usize])
                    .max()
                    .flatten();
                let mut reached = read;
                for member in members
                    .iter()
                    .filter(|member| (**member as usize) < loops.len())
                {
                    upstream[*member as usize] = read;
                    reached = reached.max(Some(loops[*member as usize].1));
                }
                latest.push(reached);
                if let Some(parent) = calls.last() {
                    let parent = &mut reads_latest[parent.node as usize];
                    *parent = (*parent).max(reached);
                }
            }
        }
        upstream
    }

    pub fn contains(&self, id: DerivationId) -> bool {
        self.record(id).is_some()
    }

    pub fn outputs_of(&self, id: DerivationId) -> impl Iterator<Item = &HashedKey> + '_ {
        self.record(id)
            .into_iter()
            .flat_map(|(_, record)| record.outputs().iter())
            .map(|key| self.key(*key))
    }

    pub fn producers(&self, key: &HashedKey) -> impl Iterator<Item = DerivationId> + '_ {
        self.table
            .find(key)
            .into_iter()
            .flat_map(|slot| self.table.producers(slot))
            .map(|deriv| self.derivations[deriv as usize].id)
    }

    pub fn derivations_of_file(&self, file_id: FileId) -> impl Iterator<Item = DerivationId> + '_ {
        self.by_file
            .get(&file_id)
            .into_iter()
            .flatten()
            .map(|deriv| self.derivations[*deriv as usize].id)
    }

    /// For every file, the other files whose derivations read a fact it
    /// owns.
    pub fn dependent_files(&self) -> FxHashMap<FileId, FxHashSet<FileId>> {
        let mut dependents: FxHashMap<FileId, FxHashSet<FileId>> = FxHashMap::default();
        for slot in self.table.keys.iter() {
            let Some(owner) = slot.key.as_deref().and_then(FactKey::owning_file) else {
                continue;
            };
            for (_, deriv) in slot.readers.iter() {
                let reader = self.derivations[deriv as usize].id;
                if reader.kind != DerivationKind::Diagnostics
                    && reader.file_id != owner
                    && reader.file_id != FileId::VIRTUAL
                {
                    dependents.entry(owner).or_default().insert(reader.file_id);
                }
            }
        }
        dependents
    }

    fn record(&self, id: DerivationId) -> Option<(DerivIdx, &Record)> {
        let deriv = self.deriv_slot(&id)?;
        Some((deriv, self.derivations[deriv as usize].record.as_ref()?))
    }

    /// The fingerprint `deriv`'s recorded read of `slot` saw.
    fn seen(&self, slot: KeyIdx, deriv: DerivIdx) -> Fingerprint {
        self.table.keys[slot as usize].readers.seen_by(deriv)
    }

    /// The slot `key` is interned to.
    pub(super) fn find_slot(&self, key: &HashedKey) -> Option<KeyIdx> {
        self.table.find(key)
    }

    /// The key table, for checking memoised values against it.
    pub(super) fn keys(&self) -> &KeyTable {
        &self.table
    }

    pub(super) fn key(&self, slot: KeyIdx) -> &HashedKey {
        self.table.key(slot)
    }

    /// What a group member that ended at stamp `ended` saw of each of
    /// `slots`, the key table's windows closed. A key a later member moved
    /// since, other than one the group reset before the member ran, which
    /// `reset_before` accepts, is [`super::UNSEEN`]; any other key it saw as
    /// it stands, which recording takes. A key that came back as it was
    /// before its window reads as unwritten, so only a moved one is unseen.
    pub fn seen_by_member(
        &self,
        slots: Vec<KeyIdx>,
        ended: u64,
        reset_before: impl Fn(&HashedKey) -> bool,
    ) -> Vec<(KeyIdx, Option<Fingerprint>)> {
        slots
            .into_iter()
            .map(|slot| {
                let moved =
                    self.table.keys[slot as usize].written > ended && !reset_before(self.key(slot));
                (slot, moved.then_some(super::UNSEEN))
            })
            .collect()
    }

    /// Interns `key` as `now`-stamped: a record's interns share one stamp,
    /// since a new slot only has to count as written after every live stamp.
    fn intern_key(&mut self, key: HashedKey, now: u64) -> KeyIdx {
        self.table.intern(key, now)
    }

    #[cfg(test)]
    pub fn intern_read(&mut self, key: HashedKey) -> KeyIdx {
        self.table.intern(key, super::new_stamp())
    }

    /// Interns a read made while no key table was lent, stamped `now` so a
    /// batch of interns shares one stamp.
    pub fn intern_read_at(&mut self, key: HashedKey, now: u64) -> KeyIdx {
        self.table.intern(key, now)
    }

    /// Frees `slot` if no record reads or produces it. A freed slot reads as
    /// written after every memo, so none replays it.
    #[cfg(test)]
    fn release_key(&mut self, slot: KeyIdx) {
        self.release_slots([slot], &FxHashMap::default());
    }

    /// Frees each of `slots` that no record reads or produces and no key is
    /// read out of, and the bases that leaves unreferenced, keeping a key
    /// `still_read` counts readers of that have yet to leave it. A base's list
    /// of derived keys drops its freed slots once, after the last is freed.
    fn release_slots(
        &mut self,
        slots: impl IntoIterator<Item = KeyIdx>,
        still_read: &FxHashMap<KeyIdx, usize>,
    ) {
        let mut freed_from = FxHashMap::default();
        for slot in slots {
            self.release_one(slot, still_read, &mut freed_from);
        }
        self.table.drop_freed_derived(freed_from.into_keys());
    }

    /// [`Self::release_slots`] for one slot, counting in `freed_from` the
    /// derived keys freed out of each base.
    fn release_one(
        &mut self,
        slot: KeyIdx,
        still_read: &FxHashMap<KeyIdx, usize>,
        freed_from: &mut FxHashMap<KeyIdx, usize>,
    ) {
        if still_read.get(&slot).is_some_and(|readers| *readers > 0) {
            return;
        }
        if !self.table.unreferenced_after(slot, freed_from) {
            return;
        }
        let entry = &mut self.table.keys[slot as usize];
        let Some(key) = entry.key.take() else {
            return;
        };
        entry.written = u64::MAX;
        entry.set_mark(HELD, false);
        self.table.held.remove(&slot);
        self.table.last_written = super::new_stamp();
        self.table.unfile(&key, slot);
        self.table.retire(slot);
        let Some(base) = key
            .base()
            .and_then(|base| self.table.find(&HashedKey::new(base)))
        else {
            return;
        };
        let Some(derived) = self.table.derived.get(&base) else {
            return;
        };
        let derived_keys = derived.slots.len();
        let freed = freed_from.entry(base).or_default();
        *freed += 1;
        if *freed == derived_keys {
            self.release_one(base, still_read, freed_from);
        }
    }

    /// The slot `id` is interned to.
    fn deriv_slot(&self, id: &DerivationId) -> Option<DerivIdx> {
        self.deriv_slots
            .find(deriv_hash(id), |deriv| {
                self.derivations[*deriv as usize].id == *id
            })
            .copied()
    }

    fn intern_derivation(&mut self, id: DerivationId) -> DerivIdx {
        if let Some(slot) = self.deriv_slot(&id) {
            return slot;
        }
        self.new_derivation(id)
    }

    /// Gives `id`, which has no slot, one.
    fn new_derivation(&mut self, id: DerivationId) -> DerivIdx {
        let entry = DerivSlot { id, record: None };
        let slot = match self.free_derivations.pop() {
            Some(slot) => {
                self.derivations[slot as usize] = entry;
                slot
            }
            None => DerivIdx::try_from(self.derivations.push(entry))
                .ok()
                .filter(|slot| *slot < NO_PRODUCER)
                .expect("fewer than 2^29 derivations"),
        };
        let derivations = &self.derivations;
        self.deriv_slots
            .insert_unique(deriv_hash(&id), slot, |deriv| {
                deriv_hash(&derivations[*deriv as usize].id)
            });
        slot
    }

    /// Adds `deriv`, a derivation of `file_id`, to `key`'s producers unless it
    /// is one.
    fn produce(&mut self, key: KeyIdx, deriv: DerivIdx, file_id: FileId) {
        if self.table.producers(key).any(|producer| producer == deriv) {
            return;
        }
        self.table.add_producer(key, deriv);
        if let Some(owner) = self.key(key).owning_file()
            && owner != file_id
        {
            *self
                .foreign_producers
                .entry(owner)
                .or_default()
                .entry(deriv)
                .or_default() += 1;
        }
    }

    /// Removes `deriv` from `key`'s producers, leaving the slot allocated.
    fn unproduce(&mut self, key: KeyIdx, deriv: DerivIdx, file_id: FileId) {
        self.table.remove_producer(key, deriv);
        if let Some(owner) = self.key(key).owning_file()
            && owner != file_id
            && let Some(derivs) = self.foreign_producers.get_mut(&owner)
            && let Some(count) = derivs.get_mut(&deriv)
        {
            *count -= 1;
            if *count == 0 {
                derivs.remove(&deriv);
                if derivs.is_empty() {
                    self.foreign_producers.remove(&owner);
                }
            }
        }
    }

    /// Removes `deriv`'s record and edges and frees its slot, its reads'
    /// readers having left them, each counted out of `still_read`. Returns the
    /// facts it produced.
    fn detach(
        &mut self,
        deriv: DerivIdx,
        still_read: &mut FxHashMap<KeyIdx, usize>,
    ) -> Box<[HashedKey]> {
        let id = self.derivations[deriv as usize].id;
        let record = self.derivations[deriv as usize]
            .record
            .take()
            .unwrap_or_default();
        for key in record.reads().iter() {
            if let Some(readers) = still_read.get_mut(key) {
                *readers -= 1;
            }
            self.release_slots([*key], still_read);
        }
        let outputs = record
            .outputs()
            .iter()
            .map(|key| {
                let fact = self.key(*key).clone();
                self.unproduce(*key, deriv, id.file_id);
                self.release_slots([*key], still_read);
                fact
            })
            .collect();
        if let Some(derivs) = self.by_file.get_mut(&id.file_id) {
            derivs.remove(&deriv);
            if derivs.is_empty() {
                self.by_file.remove(&id.file_id);
            }
        }
        if let Ok(entry) = self
            .deriv_slots
            .find_entry(deriv_hash(&id), |slot| *slot == deriv)
        {
            entry.remove();
        }
        self.free_derivations.push(deriv);
        outputs
    }
}

/// A record whose derivation and outputs are interned, to diff against the
/// record it replaces.
struct PreparedRecord {
    id: DerivationId,
    deriv: DerivIdx,
    reads: Vec<(KeyIdx, Fingerprint)>,
    dropped_reads: Vec<KeyIdx>,
    outputs: Vec<KeyIdx>,
}

/// What a record changes of the one it replaces: its reads' readers, the
/// reads it stopped making, the outputs it added and dropped, and the
/// record to keep, `None` when it is the one replaced.
struct DiffedRecord {
    id: DerivationId,
    deriv: DerivIdx,
    ops: Vec<(KeyIdx, ReaderOp)>,
    unread: Vec<KeyIdx>,
    dropped_reads: Vec<KeyIdx>,
    added: Vec<KeyIdx>,
    dropped: Vec<KeyIdx>,
    record: Option<Record>,
}

impl PreparedRecord {
    /// Diffs against the record `old` it replaces, if the derivation has one.
    /// A read kept at the value its reader group in `readers` holds needs no
    /// op.
    fn diff(self, old: Option<&Record>, readers: Option<&KeyTable>) -> DiffedRecord {
        let Self {
            id,
            deriv,
            reads,
            dropped_reads,
            outputs,
        } = self;
        let none = Record::default();
        let filed = old.is_some();
        let old = old.unwrap_or(&none);
        let mut ops = Vec::new();
        let mut unread = Vec::new();
        let (mut old_reads, mut new_reads) = (
            old.reads().iter().copied().peekable(),
            reads.iter().copied().peekable(),
        );
        loop {
            match (old_reads.peek().copied(), new_reads.peek().copied()) {
                (Some(old_key), Some((new_key, new_seen))) if old_key == new_key => {
                    if readers.is_none_or(|table| {
                        table.keys[new_key as usize].readers.seen_by(deriv) != new_seen
                    }) {
                        ops.push((new_key, ReaderOp::Seen(deriv, new_seen)));
                    }
                    old_reads.next();
                    new_reads.next();
                }
                (Some(old_key), next) if next.is_none_or(|(new_key, _)| old_key < new_key) => {
                    ops.push((old_key, ReaderOp::Remove(deriv)));
                    unread.push(old_key);
                    old_reads.next();
                }
                (_, Some((new_key, new_seen))) => {
                    ops.push((new_key, ReaderOp::Insert(deriv, new_seen)));
                    new_reads.next();
                }
                (None, None) => break,
                (Some(_), None) => unreachable!("handled above"),
            }
        }
        let mut dropped = Vec::new();
        let mut added = Vec::new();
        merge_sorted(
            old.outputs().iter().copied(),
            outputs.iter().copied(),
            |key| dropped.push(key),
            |key| added.push(key),
        );
        let unchanged =
            filed && ops.is_empty() && unread.is_empty() && added.is_empty() && dropped.is_empty();
        let record = (!unchanged).then(|| Record::new(reads.iter().map(|(key, _)| *key), &outputs));
        DiffedRecord {
            id,
            deriv,
            ops,
            unread,
            dropped_reads,
            added,
            dropped,
            record,
        }
    }
}

/// A change a record makes to the readers of one of its reads.
#[derive(Debug, Clone, Copy)]
enum ReaderOp {
    /// The reader now saw this, if it saw something else before.
    Seen(DerivIdx, Fingerprint),
    Insert(DerivIdx, Fingerprint),
    Remove(DerivIdx),
}

impl ReaderOp {
    fn apply(self, readers: &mut Readers) {
        match self {
            Self::Seen(deriv, seen) => {
                if readers.seen_by(deriv) != seen {
                    readers.insert(deriv, seen);
                }
            }
            Self::Insert(deriv, seen) => readers.insert(deriv, seen),
            Self::Remove(deriv) => readers.remove(deriv),
        }
    }
}

/// The slots one worker updates at a time: a page of keys every record reads
/// still spreads over the workers.
const READER_CHUNK: usize = 1 << 10;

const _: () = assert!(PAGE.is_multiple_of(READER_CHUNK));

impl KeyTable {
    /// Applies each record's `ops`, each slot's together in record order:
    /// every op is sorted by slot on worker threads, a slot's keeping record
    /// order, then each chunk of slots they reach takes its ops on a worker.
    fn apply_reader_ops(&mut self, ops: Vec<Vec<(KeyIdx, ReaderOp)>>) {
        use rayon::prelude::*;
        let mut flat = Vec::with_capacity(ops.iter().map(Vec::len).sum::<usize>());
        for record_ops in ops {
            flat.extend(record_ops);
        }
        let mut ops = flat;
        ops.par_sort_by_key(|(slot, _)| *slot);
        let ops: &[(KeyIdx, ReaderOp)] = &ops;
        self.par_reached_chunks(
            ops,
            |(slot, _)| *slot,
            |first, chunk, range| {
                for same_slot in ops[range].chunk_by(|(left, _), (right, _)| left == right) {
                    chunk[same_slot[0].0 as usize - first]
                        .readers
                        .apply_all(same_slot);
                }
            },
        );
    }

    /// Runs `apply` on worker threads for each chunk of slots that `items`,
    /// ascending by the slot `slot_of` gives each, reach, with the chunk's
    /// first slot, the chunk, and the range of `items` in it.
    fn par_reached_chunks<T>(
        &mut self,
        items: &[T],
        slot_of: impl Fn(&T) -> KeyIdx,
        apply: impl Fn(usize, &mut [KeySlot], std::ops::Range<usize>) + Sync,
    ) {
        use rayon::prelude::*;
        let chunk_of = |item: &T| slot_of(item) as usize / READER_CHUNK;
        let mut reached = Vec::new();
        let mut start = 0;
        while let Some(item) = items.get(start) {
            let chunk = chunk_of(item);
            let end = start + items[start..].partition_point(|item| chunk_of(item) == chunk);
            reached.push((chunk, start..end));
            start = end;
        }
        let mut reached = reached.into_iter().peekable();
        let chunks = self
            .keys
            .pages_mut()
            .iter_mut()
            .flat_map(|page| page.chunks_mut(READER_CHUNK))
            .enumerate()
            .filter_map(|(index, chunk)| {
                reached
                    .next_if(|(reached, _)| *reached == index)
                    .map(|(_, range)| (index * READER_CHUNK, chunk, range))
            })
            .collect::<Vec<_>>();
        chunks
            .into_par_iter()
            .for_each(|(first, chunk, range)| apply(first, chunk, range));
    }
}

/// A task evaluation's record with the reads it keeps sorted and those it
/// drops set apart, for [`ReaderGraph::replace_kept`].
pub struct KeptRecord {
    id: DerivationId,
    reads: Vec<(KeyIdx, Fingerprint)>,
    dropped_reads: Vec<KeyIdx>,
    /// The slots of the outputs the table already has, sorted.
    outputs: Vec<KeyIdx>,
    /// The outputs it has no slot for, interned in this order.
    new_outputs: Vec<HashedKey>,
}

impl KeyTable {
    /// The query outputs `evaluation`, canonicalised into this table, read.
    pub fn read_queries(&self, evaluation: &super::Evaluation) -> Vec<HashedKey> {
        evaluation
            .read
            .iter()
            .filter(|slot| self.is_query(**slot))
            .map(|slot| self.key(*slot).clone())
            .collect()
    }

    /// The record of `slots`, read by `id` with the fingerprint beside each
    /// and producing nothing.
    fn keep_viewed(&self, id: DerivationId, slots: Vec<(KeyIdx, Fingerprint)>) -> KeptRecord {
        let (reads, dropped_reads) = kept_reads(id, slots, |slot| self.key(slot), |_| false);
        KeptRecord {
            id,
            reads,
            dropped_reads,
            outputs: Vec::new(),
            new_outputs: Vec::new(),
        }
    }

    /// The record of a task's `evaluation`, whose reads were canonicalised
    /// into this table, each with the fingerprint in `seen` at its position,
    /// and the keys the evaluation wrote.
    pub fn keep_record(
        &self,
        evaluation: super::Evaluation,
        seen: Vec<Fingerprint>,
    ) -> (KeptRecord, Vec<HashedKey>) {
        let (id, slots, written) = evaluation.into_recorded_seen(seen);
        let (reads, dropped_reads) = kept_reads(
            id,
            slots,
            |slot| self.key(slot),
            |key| super::contains_hashed(&written, key),
        );
        let mut outputs = Vec::with_capacity(written.len());
        let mut new_outputs = Vec::new();
        for key in &written {
            match self.find(key) {
                Some(slot) => outputs.push(slot),
                None => new_outputs.push(key.clone()),
            }
        }
        outputs.sort_unstable();
        outputs.dedup();
        let record = KeptRecord {
            id,
            reads,
            dropped_reads,
            outputs,
            new_outputs,
        };
        (record, written)
    }
}

/// The reads of `slots` a record of `id` keeps, sorted by slot, and the
/// slots of those it drops; see [`super::records_read`].
fn kept_reads<'a, T>(
    id: DerivationId,
    slots: Vec<(KeyIdx, T)>,
    key: impl Fn(KeyIdx) -> &'a HashedKey,
    is_output: impl Fn(&HashedKey) -> bool,
) -> (Vec<(KeyIdx, T)>, Vec<KeyIdx>) {
    let mut dropped = Vec::new();
    let mut reads = Vec::with_capacity(slots.len());
    for (slot, seen) in slots {
        if super::records_read(id, key(slot), &is_output) {
            reads.push((slot, seen));
        } else {
            dropped.push(slot);
        }
    }
    reads.sort_unstable_by_key(|(slot, _)| *slot);
    (reads, dropped)
}

/// Walks two sorted slot lists, calling `only_old` for each slot only `old`
/// has and `only_new` for each slot only `new` has.
fn merge_sorted(
    old: impl Iterator<Item = KeyIdx>,
    new: impl Iterator<Item = KeyIdx>,
    mut only_old: impl FnMut(KeyIdx),
    mut only_new: impl FnMut(KeyIdx),
) {
    let mut old = old.peekable();
    let mut new = new.peekable();
    loop {
        match (old.peek().copied(), new.peek().copied()) {
            (Some(left), Some(right)) if left == right => {
                old.next();
                new.next();
            }
            (Some(left), Some(right)) if left < right => {
                only_old(left);
                old.next();
            }
            (_, Some(right)) => {
                only_new(right);
                new.next();
            }
            (Some(left), None) => {
                only_old(left);
                old.next();
            }
            (None, None) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use rowan::{TextRange, TextSize};

    use super::*;
    use crate::db_index::derivation::DerivationKind;

    fn id(file: u32, start: u32) -> DerivationId {
        DerivationId::new(
            DerivationKind::LocalStat,
            FileId::new(file),
            LuaSyntaxId::new(
                LuaSyntaxKind::LocalStat.into(),
                TextRange::at(TextSize::new(start), TextSize::new(1)),
            ),
        )
    }

    fn key(file: u32) -> HashedKey {
        HashedKey::new(FactKey::DeclTree(FileId::new(file)))
    }

    #[test]
    fn stored_fact_handles_reject_reused_slots_and_diverged_forks() {
        let mut graph = ReaderGraph::default();
        let slot = graph.intern_read(key(1));
        let mut fork = graph.clone();
        graph.release_key(slot);
        fork.release_key(slot);
        let original = |fact: &FactKey| fact == key(1).key();
        assert_eq!(graph.keys().matching_slot(slot, original), None);
        assert_eq!(graph.intern_read(key(2)), slot);
        assert_eq!(fork.intern_read(key(3)), slot);
        assert_eq!(graph.keys().matching_slot(slot, original), None);
        let fork_key = |fact: &FactKey| fact == key(3).key();
        assert_eq!(graph.keys().matching_slot(slot, fork_key), None);
        assert_eq!(fork.keys().matching_slot(slot, fork_key), Some(slot));
    }

    #[test]
    fn stale_readers_are_those_that_saw_another_fingerprint() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(1, 0), vec![(key(9), Some(5))], vec![key(1)], |_| 0);
        graph.replace(id(2, 0), vec![(key(9), Some(6))], vec![key(2)], |_| 0);

        let stale: Vec<_> = graph.stale_readers(&key(9), |seen| seen == 6).collect();

        assert_eq!(stale, vec![id(1, 0)]);
    }

    #[test]
    fn replace_drops_old_edges_and_reports_outputs_no_longer_produced() {
        let mut graph = ReaderGraph::default();
        graph.replace(
            id(1, 0),
            vec![(key(9), Some(5))],
            vec![key(1), key(2)],
            |_| 0,
        );

        let dropped = graph.replace(id(1, 0), vec![(key(8), Some(1))], vec![key(2)], |_| 0);

        assert_eq!(dropped, vec![key(1)]);
        assert_eq!(graph.stale_readers(&key(9), |seen| seen == 0).count(), 0);
        assert_eq!(graph.producers(&key(1)).count(), 0);
        assert_eq!(graph.producers(&key(2)).collect::<Vec<_>>(), vec![id(1, 0)]);
    }

    #[test]
    fn a_read_kept_across_evaluations_holds_the_latest_fingerprint() {
        let mut graph = ReaderGraph::default();
        graph.replace(
            id(1, 0),
            vec![(key(9), Some(5)), (key(8), Some(1))],
            vec![],
            |_| 0,
        );

        graph.replace(id(1, 0), vec![(key(9), Some(7))], vec![], |_| 0);

        assert_eq!(graph.stale_readers(&key(9), |seen| seen == 7).count(), 0);
        assert_eq!(
            graph
                .stale_readers(&key(9), |seen| seen == 5)
                .collect::<Vec<_>>(),
            vec![id(1, 0)]
        );
        assert_eq!(graph.stale_readers(&key(8), |_| false).count(), 0);
        assert_eq!(graph.reads_of(id(1, 0)).collect::<Vec<_>>(), vec![&key(9)]);
    }

    #[test]
    fn a_key_nothing_refers_to_is_forgotten_and_its_slot_reused() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(1, 0), vec![(key(9), Some(5))], vec![key(1)], |_| 0);
        graph.replace(id(1, 0), vec![], vec![], |_| 0);

        assert_eq!(graph.read_keys().count(), 0);
        assert_eq!(graph.producers(&key(1)).count(), 0);

        graph.replace(id(1, 0), vec![(key(7), Some(1))], vec![key(6)], |_| 0);

        assert_eq!(graph.table.keys.len(), 2);
    }

    #[test]
    fn remove_file_returns_outputs_and_forgets_its_readers() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(1, 0), vec![(key(9), Some(5))], vec![key(1)], |_| 0);
        graph.replace(id(2, 0), vec![(key(1), Some(5))], vec![key(2)], |_| 0);

        let removed = graph.remove_files(&[FileId::new(1)]);

        assert_eq!(removed.len(), 1);
        assert_eq!(&*removed[0].1, &[key(1)]);
        assert_eq!(graph.stale_readers(&key(9), |seen| seen == 0).count(), 0);
        assert_eq!(
            graph
                .stale_readers(&key(1), |seen| seen == 0)
                .collect::<Vec<_>>(),
            vec![id(2, 0)]
        );
        assert_eq!(graph.derivations_of_file(FileId::new(1)).count(), 0);
    }

    #[test]
    fn a_derivation_writing_another_files_key_is_its_foreign_producer() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(2, 0), vec![], vec![key(1), key(2)], |_| 0);

        assert_eq!(graph.foreign_producers(FileId::new(1)), vec![id(2, 0)]);
        assert!(graph.foreign_producers(FileId::new(2)).is_empty());

        graph.replace(id(2, 0), vec![], vec![key(2)], |_| 0);

        assert!(graph.foreign_producers(FileId::new(1)).is_empty());
    }

    #[test]
    fn a_derivation_still_writing_another_files_key_stays_its_foreign_producer() {
        let other_key = HashedKey::new(FactKey::LoadSites(FileId::new(1)));
        let mut kept = ReaderGraph::default();
        kept.replace(id(2, 0), vec![], vec![key(1), other_key.clone()], |_| 0);
        kept.replace(id(2, 0), vec![], vec![other_key.clone()], |_| 0);
        let mut swapped = ReaderGraph::default();
        swapped.replace(id(2, 0), vec![], vec![key(1)], |_| 0);
        swapped.replace(id(2, 0), vec![], vec![other_key], |_| 0);

        assert_eq!(kept.foreign_producers(FileId::new(1)), vec![id(2, 0)]);
        assert_eq!(swapped.foreign_producers(FileId::new(1)), vec![id(2, 0)]);
    }

    #[test]
    fn a_slot_one_member_drops_stays_live_for_another_member_of_its_group() {
        let mut graph = ReaderGraph::default();
        let mut table = graph.lend_keys();
        let slot = table.intern_read(key(9), 0);
        assert!(table.log(slot, 1, 1).0.is_some());
        assert_eq!(table.intern_read(key(9), 0), slot);
        assert!(table.log(slot, 2, 2).0.is_some());
        graph.restore_keys(table);
        let producer = DerivationId::new(
            DerivationKind::LocalStat,
            FileId::new(9),
            LuaSyntaxId::new(
                LuaSyntaxKind::LocalStat.into(),
                TextRange::at(TextSize::new(0), TextSize::new(1)),
            ),
        );

        let mut released = Vec::new();
        graph.replace_slots(
            producer,
            vec![(slot, None)],
            vec![key(9)],
            |_| 0,
            &mut released,
            super::super::new_stamp(),
        );
        graph.replace_slots(
            id(2, 0),
            vec![(slot, None)],
            vec![],
            |_| 5,
            &mut released,
            super::super::new_stamp(),
        );
        graph.release_keys(released);

        assert_eq!(graph.reads_of(id(2, 0)).collect::<Vec<_>>(), vec![&key(9)]);
        assert_eq!(graph.reads_of(producer).count(), 0);
    }

    #[test]
    fn a_slot_one_member_stops_producing_stays_live_for_another_member_of_its_group() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(1, 0), vec![], vec![key(9)], |_| 0);
        let mut table = graph.lend_keys();
        let slot = table.intern_read(key(9), 1);
        graph.restore_keys(table);

        let mut released = Vec::new();
        graph.replace_slots(
            id(1, 0),
            vec![],
            vec![],
            |_| 0,
            &mut released,
            super::super::new_stamp(),
        );
        graph.replace_slots(
            id(2, 0),
            vec![(slot, None)],
            vec![],
            |_| 5,
            &mut released,
            super::super::new_stamp(),
        );
        graph.release_keys(released);

        assert_eq!(graph.reads_of(id(2, 0)).collect::<Vec<_>>(), vec![&key(9)]);
        assert_eq!(graph.producers(&key(9)).count(), 0);
    }

    #[test]
    fn an_evaluation_begun_inside_another_suspends_it_until_it_ends() {
        use super::super::{begin, end};

        let _recording = begin(id(1, 0));
        let _recording = begin(id(2, 0));

        assert_eq!(end().id, id(2, 0));
        assert_eq!(end().id, id(1, 0));
    }

    fn groups(readers: &Readers) -> Vec<(Fingerprint, Vec<DerivIdx>)> {
        readers
            .groups()
            .map(|(seen, derivs)| (seen, derivs.to_vec()))
            .collect()
    }

    /// Ops applied together leave the readers exactly as applying them one at
    /// a time does: the same groups, in the same order and form, over random
    /// sequences that move readers between several values, insert, see and
    /// remove the same reader more than once, and empty and refill groups.
    #[test]
    fn reader_ops_applied_together_match_them_applied_one_at_a_time() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        for case in 0..2000 {
            let mut start = Readers::default();
            for _ in 0..next(40) {
                start.insert(next(30) as DerivIdx, next(4));
            }
            let mut one_by_one = start.clone();
            let mut ops = Vec::new();
            for _ in 0..1 + next(30) {
                let deriv = next(30) as DerivIdx;
                let op = match next(3) {
                    // Seen names a current reader, as a record's diff does.
                    0 if one_by_one.group_of(deriv).is_some() => ReaderOp::Seen(deriv, next(4)),
                    0 | 1 => ReaderOp::Insert(deriv, next(4)),
                    _ => ReaderOp::Remove(deriv),
                };
                op.apply(&mut one_by_one);
                ops.push((0, op));
            }
            let mut together = start;
            together.apply_all(&ops);

            assert_eq!(
                format!("{together:?}"),
                format!("{one_by_one:?}"),
                "case {case}: {ops:?}"
            );
        }
    }

    #[test]
    fn a_reader_that_sees_the_value_the_others_saw_rejoins_their_group() {
        let mut readers = Readers::default();
        readers.insert(1, 5);
        readers.insert(2, 5);
        readers.insert(3, 6);

        readers.insert(3, 5);

        assert_eq!(groups(&readers), vec![(5, vec![1, 2, 3])]);
    }

    #[test]
    fn relabelling_one_group_as_another_merges_them() {
        let mut readers = Readers::default();
        readers.insert(1, 5);
        readers.insert(2, 6);
        readers.insert(3, 6);

        readers.relabel(6, 5);

        assert_eq!(groups(&readers), vec![(5, vec![1, 2, 3])]);
    }

    #[test]
    fn removing_the_only_reader_of_a_value_leaves_the_others_group() {
        let mut readers = Readers::default();
        readers.insert(1, 5);
        readers.insert(2, 5);
        readers.insert(3, 6);

        readers.remove(3);

        assert_eq!(
            (groups(&readers), readers.seen_by(2)),
            (vec![(5, vec![1, 2])], 5)
        );
    }

    /// Deleting text moves a key after the deletion onto the place of one
    /// inside it: a derivation that read both reads one key, at one value.
    #[test]
    fn a_reader_of_two_keys_an_edit_merges_leaves_no_reader_behind() {
        use crate::{LuaDeclId, PositionMap};
        let decl = |position: u32| {
            HashedKey::new(FactKey::Decl(LuaDeclId::new(
                FileId::new(1),
                TextSize::new(position),
            )))
        };
        let mut graph = ReaderGraph::default();
        graph.replace(
            id(2, 0),
            vec![(decl(20), Some(5)), (decl(30), Some(6))],
            vec![],
            |_| 0,
        );
        let old_text = format!("{}{}{}", "a".repeat(20), "b".repeat(10), "c".repeat(10));
        let new_text = format!("{}{}", "a".repeat(20), "c".repeat(10));
        graph.apply_edits(&EditMaps::new([(
            FileId::new(1),
            PositionMap::new(&old_text, &new_text),
        )]));

        graph.remove_files(&[FileId::new(2)]);

        assert_eq!(graph.stale_readers(&decl(20), |_| false).count(), 0);
    }

    /// A key a derivation writes follows an edit that moves it, as its reads
    /// do, even through a chain where one key moves onto another's place.
    #[test]
    fn a_written_key_moves_with_its_producer_when_an_edit_shifts_it() {
        use crate::{LuaDeclId, PositionMap};
        let decl = |position: u32| {
            HashedKey::new(FactKey::Decl(LuaDeclId::new(
                FileId::new(1),
                TextSize::new(position),
            )))
        };
        let mut graph = ReaderGraph::default();
        graph.replace(id(2, 0), vec![], vec![decl(20), decl(30)], |_| 0);
        let old_text = "a".repeat(40);
        let new_text = format!("{}{}", "b".repeat(10), "a".repeat(40));

        graph.apply_edits(&EditMaps::new([(
            FileId::new(1),
            PositionMap::new(&old_text, &new_text),
        )]));

        let mut outputs = graph.outputs_of(id(2, 0)).cloned().collect::<Vec<_>>();
        outputs.sort_by_key(|key| format!("{key:?}"));
        let producers = [20, 30, 40].map(|at| graph.producers(&decl(at)).collect::<Vec<_>>());
        assert_eq!(
            (outputs, producers),
            (
                vec![decl(30), decl(40)],
                [vec![], vec![id(2, 0)], vec![id(2, 0)]]
            )
        );
    }

    /// A key read after an edit built the positional index is renamed by the
    /// next edit like every key read before it.
    #[test]
    fn a_key_read_after_an_edit_is_renamed_by_the_next() {
        use crate::{LuaDeclId, PositionMap};
        let decl = |position: u32| {
            HashedKey::new(FactKey::Decl(LuaDeclId::new(
                FileId::new(1),
                TextSize::new(position),
            )))
        };
        let insert = |at: usize| {
            let old_text = "a".repeat(40);
            let new_text = format!("{}b{}", "a".repeat(at), "a".repeat(40 - at));
            EditMaps::new([(FileId::new(1), PositionMap::new(&old_text, &new_text))])
        };
        let mut graph = ReaderGraph::default();
        graph.replace(id(2, 0), vec![(decl(20), Some(5))], vec![], |_| 0);
        graph.apply_edits(&insert(0));
        graph.replace(id(3, 0), vec![(decl(30), Some(6))], vec![], |_| 0);

        let applied = graph.apply_edits(&insert(0));

        assert!(applied.moved.contains(&decl(31)), "{:?}", applied.moved);
        assert_eq!(graph.stale_readers(&decl(31), |_| false).count(), 1);
    }

    #[test]
    fn a_key_keeps_its_other_producers_when_its_first_stops() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(1, 0), vec![], vec![key(9)], |_| 0);
        graph.replace(id(2, 0), vec![], vec![key(9)], |_| 0);
        graph.replace(id(3, 0), vec![], vec![key(9)], |_| 0);

        graph.replace(id(1, 0), vec![], vec![], |_| 0);

        let mut producers = graph.producers(&key(9)).collect::<Vec<_>>();
        producers.sort();
        assert_eq!(producers, vec![id(2, 0), id(3, 0)]);
    }

    #[test]
    fn a_key_read_before_and_produced_now_stays_live() {
        let mut graph = ReaderGraph::default();
        graph.replace(id(1, 0), vec![(key(9), Some(5))], vec![], |_| 0);

        graph.replace(id(1, 0), vec![], vec![key(9)], |_| 0);

        assert_eq!(graph.producers(&key(9)).collect::<Vec<_>>(), vec![id(1, 0)]);
        assert_eq!(
            graph.outputs_of(id(1, 0)).collect::<Vec<_>>(),
            vec![&key(9)]
        );
    }

    /// The records of `id` and the readers and producers of `keys`, as the
    /// graph holds them.
    fn filed_state(graph: &ReaderGraph, id: DerivationId, keys: &[HashedKey]) -> String {
        let keyed = keys
            .iter()
            .map(|key| {
                let slot = graph.table.find(key).expect("a filed key is interned");
                (
                    format!("{:?}", graph.table.keys[slot as usize].readers),
                    graph.producers(key).collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        format!(
            "{:?} {:?} {} {keyed:?}",
            graph.reads_of(id).collect::<Vec<_>>(),
            graph.outputs_of(id).collect::<Vec<_>>(),
            graph.derivations_of_file(id.file_id).count()
        )
    }

    #[test]
    fn filing_an_unchanged_record_again_leaves_the_graph_as_filing_it_once_does() {
        let keys = [key(100), key(101), key(102)];
        let fresh = || {
            let mut graph = ReaderGraph::default();
            let slots = keys.each_ref().map(|key| graph.intern_read(key.clone()));
            (graph, slots)
        };
        let file = |graph: &mut ReaderGraph, slots: &[KeyIdx; 3], seen: Fingerprint| {
            let record = KeptRecord {
                id: id(1, 0),
                reads: vec![(slots[0], seen), (slots[1], seen)],
                dropped_reads: Vec::new(),
                outputs: vec![slots[2]],
                new_outputs: Vec::new(),
            };
            let mut released = Vec::new();
            graph.replace_all_kept(vec![record], &mut released, super::super::new_stamp);
            released
        };
        let (mut once, slots) = fresh();
        file(&mut once, &slots, 5);
        let (mut twice, _) = fresh();
        file(&mut twice, &slots, 5);
        let reads_filed = |graph: &ReaderGraph| {
            graph.derivations[0]
                .record
                .as_ref()
                .expect("the record is filed")
                .reads()
                .as_ptr()
        };
        let filed = reads_filed(&twice);

        let released = file(&mut twice, &slots, 5);

        assert!(released.is_empty());
        assert_eq!(reads_filed(&twice), filed, "the record was replaced");
        assert_eq!(
            filed_state(&twice, id(1, 0), &keys),
            filed_state(&once, id(1, 0), &keys)
        );

        file(&mut twice, &slots, 6);
        let (mut other, _) = fresh();
        file(&mut other, &slots, 6);

        assert_eq!(
            filed_state(&twice, id(1, 0), &keys),
            filed_state(&other, id(1, 0), &keys)
        );
        assert_eq!(twice.stale_readers(&keys[0], |seen| seen == 6).count(), 0);
        assert_eq!(twice.stale_readers(&keys[0], |seen| seen == 5).count(), 1);
    }

    /// A large batch frees the slots one at a time would, in the same order,
    /// whichever of its slots repeat, are read, or are already free.
    #[test]
    fn a_large_batch_of_released_slots_frees_the_slots_one_at_a_time_frees() {
        let mut graph = ReaderGraph::default();
        let slots = (0..2 * PARALLEL_RELEASE as u32)
            .map(|file| graph.intern_read(key(1000 + file)))
            .collect::<Vec<_>>();
        let mut released = Vec::new();
        let mut file = |graph: &mut ReaderGraph, id, slots: Vec<KeyIdx>| {
            graph.replace_slots(
                id,
                slots.into_iter().map(|slot| (slot, Some(1))).collect(),
                vec![],
                |_| 0,
                &mut released,
                super::super::new_stamp(),
            );
        };
        file(&mut graph, id(1, 0), slots.clone());
        file(
            &mut graph,
            id(2, 0),
            slots.iter().step_by(3).copied().collect(),
        );
        file(&mut graph, id(1, 0), Vec::new());
        assert!(released.len() >= PARALLEL_RELEASE);
        released.extend(released.clone());
        let mut one_at_a_time = graph.clone();
        for slot in released.iter().copied() {
            one_at_a_time.release_key(slot);
        }

        graph.release_keys(released);

        assert_eq!(graph.table.free_keys, one_at_a_time.table.free_keys);
        assert_eq!(graph.read_keys().count(), one_at_a_time.read_keys().count());
        assert_eq!(graph.read_keys().count(), slots.len().div_ceil(3));
    }

    /// Slots across several chunks are marked written at one stamp, and the
    /// others keep theirs.
    #[test]
    fn marking_keys_written_stamps_exactly_their_slots() {
        let mut graph = ReaderGraph::default();
        let keys = (0..3 * READER_CHUNK as u32)
            .map(|file| key(1000 + file))
            .collect::<Vec<_>>();
        let slots = keys
            .iter()
            .map(|key| graph.intern_read(key.clone()))
            .collect::<Vec<_>>();
        let marked = keys.iter().step_by(2).cloned().collect::<Vec<_>>();

        graph.mark_written(&marked);

        let now = graph.table.last_written();
        for (position, slot) in slots.iter().enumerate() {
            let written = graph.table.keys[*slot as usize].written;
            assert_eq!(written == now, position % 2 == 0, "slot {slot}");
        }
    }

    /// Records `id` reading the keys of `reads` and writing those of `writes`.
    fn record(graph: &mut ReaderGraph, id: DerivationId, reads: &[u32], writes: &[u32]) {
        graph.replace(
            id,
            reads.iter().map(|read| (key(*read), Some(0))).collect(),
            writes.iter().map(|write| key(*write)).collect(),
            |_| 0,
        );
    }

    /// Two loops, the second reading the first through a pass between them.
    fn two_loops(graph: &mut ReaderGraph, first_reads: &[u32]) -> [Vec<DerivationId>; 2] {
        record(graph, id(1, 0), first_reads, &[11]);
        record(graph, id(1, 1), &[11], &[12]);
        record(graph, id(2, 0), &[11], &[20]);
        record(graph, id(3, 0), &[20, 32], &[31]);
        record(graph, id(3, 1), &[31], &[32]);
        [vec![id(1, 0), id(1, 1)], vec![id(3, 0), id(3, 1)]]
    }

    #[test]
    fn a_loop_takes_the_latest_stamp_of_those_reaching_it_through_moved_keys_alone() {
        let mut graph = ReaderGraph::default();
        let [first, second] = two_loops(&mut graph, &[12]);
        let loops = [(first.as_slice(), 2), (second.as_slice(), 1)];
        let all = [11, 12, 20, 31, 32]
            .map(key)
            .into_iter()
            .collect::<FxHashSet<_>>();
        let pass_held = all
            .iter()
            .filter(|moved| **moved != key(20))
            .cloned()
            .collect::<FxHashSet<_>>();

        assert_eq!(
            graph.latest_upstream_loops(&loops, &all, |_| true),
            [None, Some(2)]
        );
        assert_eq!(
            graph.latest_upstream_loops(&loops, &pass_held, |_| true),
            [None, None]
        );
        assert_eq!(
            graph.latest_upstream_loops(&loops, &all, |kind| kind != DerivationKind::LocalStat),
            [None, None]
        );
    }

    #[test]
    fn loops_that_reach_each_other_through_moved_keys_take_no_stamp_of_each_other() {
        let mut graph = ReaderGraph::default();
        let [first, second] = two_loops(&mut graph, &[12, 40]);
        record(&mut graph, id(4, 0), &[31], &[40]);
        let loops = [(first.as_slice(), 1), (second.as_slice(), 2)];
        let all = [11, 12, 20, 31, 32, 40]
            .map(key)
            .into_iter()
            .collect::<FxHashSet<_>>();

        assert_eq!(
            graph.latest_upstream_loops(&loops, &all, |_| true),
            [None, None]
        );
    }

    /// The slots, derived keys, free slots and records of `graph`, as filed.
    fn filed_graph(graph: &ReaderGraph) -> String {
        let keys = (0..graph.table.keys.len())
            .map(|slot| {
                let entry = &graph.table.keys[slot];
                format!(
                    "{slot} {:?} {:?}",
                    entry.key.as_ref().map(HashedKey::key),
                    entry.readers
                )
            })
            .collect::<Vec<_>>();
        let records = graph
            .derivations
            .iter()
            .map(|slot| {
                format!(
                    "{:?} {:?}",
                    slot.id,
                    slot.record.as_ref().map(Record::reads)
                )
            })
            .collect::<Vec<_>>();
        let mut derived = graph
            .table
            .derived
            .iter()
            .map(|(base, derived)| (*base, derived.slots.clone()))
            .collect::<Vec<_>>();
        derived.sort();
        format!(
            "{keys:#?}\n{records:#?}\n{derived:?}\n{:?}",
            graph.table.free_keys
        )
    }

    fn decl_key(file: u32, position: u32) -> HashedKey {
        HashedKey::new(FactKey::Decl(crate::LuaDeclId::new(
            FileId::new(file),
            TextSize::new(position),
        )))
    }

    /// Derivations reading several declarations of file 1, at positions that
    /// neighbour each other, each at a value that varies with the reader.
    fn crowded_decl_graph() -> ReaderGraph {
        let mut graph = ReaderGraph::default();
        for reader in 0..600u32 {
            let mut positions = (1..=5u32)
                .map(|step| (reader * (2 * step + 5)) % 400)
                .collect::<Vec<_>>();
            positions.sort_unstable();
            positions.dedup();
            let mut reads = positions
                .into_iter()
                .map(|position| {
                    let seen = u64::from((reader + position) % 3) + 1;
                    (decl_key(1, position), Some(seen))
                })
                .collect::<Vec<_>>();
            reads.push((decl_key(2, reader % 50), Some(7)));
            graph.replace(id(2, reader), reads, vec![], |_| 0);
        }
        graph
    }

    /// Renames the reads of the keys `edits` moved one key at a time.
    fn renamed_one_at_a_time(graph: &mut ReaderGraph, edits: &EditMaps) -> AppliedEdits {
        let files: FxHashSet<FileId> = edits.files().collect();
        let mut lost = Vec::new();
        let mut renames = Vec::new();
        for slot in graph.table.positional_slots(&files) {
            let entry = &graph.table.keys[slot as usize];
            let Some(key) = entry.key.as_ref().filter(|_| !entry.readers.is_empty()) else {
                continue;
            };
            match edits.map_key(key) {
                Some(new) if new == **key => {}
                Some(new) => renames.push((slot, HashedKey::new(new))),
                None => lost.extend(
                    entry
                        .readers
                        .iter()
                        .map(|(_, deriv)| graph.derivations[deriv as usize].id),
                ),
            }
        }
        let taken = renames
            .iter()
            .map(|(slot, _)| std::mem::take(&mut graph.table.keys[*slot as usize].readers))
            .collect::<Vec<_>>();
        let now = super::super::new_stamp();
        let mut moved = Vec::new();
        let mut rewrites: FxHashMap<DerivIdx, FxHashMap<KeyIdx, KeyIdx>> = FxHashMap::default();
        for ((slot, new_key), readers) in renames.iter().zip(&taken) {
            moved.push(new_key.clone());
            let target = graph.intern_key(new_key.clone(), now);
            for (seen, deriv) in readers.iter() {
                graph.table.keys[target as usize]
                    .readers
                    .insert(deriv, seen);
                rewrites.entry(deriv).or_default().insert(*slot, target);
            }
        }
        for (deriv, rewrite) in rewrites {
            if let Some(record) = graph.derivations[deriv as usize].record.as_mut() {
                let mut reads = record
                    .reads()
                    .iter()
                    .map(|key| rewrite.get(key).copied().unwrap_or(*key))
                    .collect::<Vec<_>>();
                reads.sort_unstable();
                reads.dedup();
                *record = record.with_reads(&reads);
            }
        }
        for (slot, _) in &renames {
            graph.release_key(*slot);
        }
        lost.sort();
        lost.dedup();
        AppliedEdits { lost, moved }
    }

    /// An insertion shifts the declarations after it in chains onto each
    /// other's places, a deletion merges the ones after it onto the places of
    /// the ones it removed.
    #[test]
    fn applying_edits_files_the_graph_as_renaming_one_key_at_a_time_does() {
        use crate::PositionMap;
        let plain = "a".repeat(400);
        let inserted = format!("{}x{}", "a".repeat(150), "a".repeat(250));
        let with_gap = format!("{}{}{}", "a".repeat(100), "b".repeat(10), "c".repeat(290));
        let deleted = format!("{}{}", "a".repeat(100), "c".repeat(290));
        for (old, new) in [(plain, inserted), (with_gap, deleted)] {
            let edits = EditMaps::new([(FileId::new(1), PositionMap::new(&old, &new))]);
            let mut graph = crowded_decl_graph();
            let mut reference = graph.clone();

            let applied = graph.apply_edits(&edits);
            let expected = renamed_one_at_a_time(&mut reference, &edits);

            assert!(!applied.moved.is_empty());
            assert_eq!(applied.lost, expected.lost);
            assert_eq!(applied.moved, expected.moved);
            assert_eq!(filed_graph(&graph), filed_graph(&reference));
        }
    }

    /// Typing and undoing in a file leaves the key table the size it was.
    #[test]
    fn edits_that_cancel_leave_the_key_table_the_size_it_was() {
        use crate::PositionMap;
        let text = "a".repeat(40);
        let grown = format!("b{text}");
        let grow = EditMaps::new([(FileId::new(1), PositionMap::new(&text, &grown))]);
        let shrink = EditMaps::new([(FileId::new(1), PositionMap::new(&grown, &text))]);
        let mut graph = ReaderGraph::default();
        for reader in 0..30 {
            graph.replace(
                id(2, reader),
                vec![(decl_key(1, 5 + reader), Some(5))],
                vec![],
                |_| 0,
            );
        }
        graph.apply_edits(&grow);
        graph.apply_edits(&shrink);
        let settled = (graph.table.keys.len(), graph.table.free_keys.len());

        for _ in 0..10 {
            graph.apply_edits(&grow);
            graph.apply_edits(&shrink);
        }

        assert_eq!(
            (graph.table.keys.len(), graph.table.free_keys.len()),
            settled
        );
        assert_eq!(graph.read_keys().count(), 30);
        assert!((5..35).all(|position| graph.table.find(&decl_key(1, position)).is_some()));
    }

    /// Freeing keys read out of one base together drops them from its list
    /// once, and frees the base after the last, as freeing them singly does.
    #[test]
    fn releasing_derived_keys_in_a_batch_frees_what_releasing_them_singly_frees() {
        let references = |position: u32| {
            HashedKey::new(FactKey::DeclReferences(crate::LuaDeclId::new(
                FileId::new(1),
                TextSize::new(position),
            )))
        };
        let base = HashedKey::new(FactKey::References(FileId::new(1)));
        let mut graph = ReaderGraph::default();
        let slots = (0..40)
            .map(|position| graph.intern_read(references(position)))
            .collect::<Vec<_>>();
        let evens = slots.iter().copied().step_by(2).collect::<Vec<_>>();
        let odds = slots.iter().copied().skip(1).step_by(2).collect::<Vec<_>>();
        let mut batched = graph.clone();
        let mut singly = graph.clone();

        batched.release_keys(evens.clone());
        for slot in &evens {
            singly.release_key(*slot);
        }

        assert_eq!(filed_graph(&batched), filed_graph(&singly));
        assert_eq!(
            batched.derived_keys(&base),
            (0..40)
                .skip(1)
                .step_by(2)
                .map(references)
                .collect::<Vec<_>>()
        );

        batched.release_keys(odds.clone());
        for slot in &odds {
            singly.release_key(*slot);
        }

        assert_eq!(filed_graph(&batched), filed_graph(&singly));
        assert!(batched.find_slot(&base).is_none());
    }
}
