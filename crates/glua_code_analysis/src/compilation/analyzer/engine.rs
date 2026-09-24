//! The derivation engine: evaluates dirty derivations until every stored
//! derived fact agrees with the facts its producer read.
//!
//! A derivation is evaluated together with its co-producers — every
//! derivation that stored a value under one of the same primary keys. Their
//! keys are reset to the source layer first and the group re-runs in canonical
//! order, so a key several derivations write always holds what a cold build's
//! canonical order would leave there. Each evaluation records what it read;
//! a reader whose recorded fingerprint of a touched key no longer matches is
//! queued, and re-checked when reached, so a key that moved and moved back (a
//! burst, a transient) costs nothing. Cold builds and incremental operations
//! both end here, so they cannot disagree about a fact.

use std::collections::BTreeSet;

use rustc_hash::FxHashSet;

use crate::DbIndex;
use crate::db_index::derivation::{self, DerivationId, DerivationKind, FactKey};

/// Why a derivation is on the worklist. Changed work runs before read checks,
/// so a reader is checked after the producers the operation re-ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Reason {
    /// Its site is new or its outputs were dropped: always evaluated.
    Changed,
    /// A fact it read may have changed: evaluated only if one did.
    Read,
}

#[derive(Default)]
pub(crate) struct Engine {
    worklist: BTreeSet<(Reason, DerivationId)>,
    /// Files whose source passes read a fact that moved. A file's source
    /// layer is only ever rebuilt whole, so the caller re-analyses them.
    stale_sources: BTreeSet<crate::FileId>,
    /// Every key whose value may have changed during this engine's runs.
    touched: FxHashSet<FactKey>,
    /// The files whose derivations this engine evaluated.
    evaluated: FxHashSet<crate::FileId>,
    /// Where the edits being re-analysed moved positions: a read of a fact
    /// whose value only moved still holds.
    edits: derivation::EditMaps,
    /// Whether other files' references still have to follow `edits`.
    edits_pending: bool,
}

impl Engine {
    pub(crate) fn with_edits(edits: derivation::EditMaps) -> Self {
        Self {
            edits_pending: !edits.is_empty(),
            edits,
            ..Self::default()
        }
    }

    /// The edits other files' references have yet to follow, once.
    pub(crate) fn edits_to_apply(&mut self) -> Option<derivation::EditMaps> {
        std::mem::take(&mut self.edits_pending).then(|| self.edits.clone())
    }

    /// Whether a read of `key` that saw `seen` still holds: the value is the
    /// same, or the same once the edits' moves are undone.
    fn holds(&self, db: &DbIndex, key: &FactKey, seen: derivation::Fingerprint) -> bool {
        self.key_holds(db, key)(seen)
    }

    /// [`Self::holds`] for many readers of one key: each fingerprint is taken
    /// once.
    fn key_holds<'a>(
        &'a self,
        db: &'a DbIndex,
        key: &'a FactKey,
    ) -> impl Fn(derivation::Fingerprint) -> bool + 'a {
        let current = db.fact_fingerprint(key);
        let before_edits = std::cell::OnceCell::new();
        move |seen| {
            seen == current
                || (!self.edits.is_empty()
                    && seen
                        == *before_edits.get_or_init(|| {
                            derivation::fingerprint_before_edits(&self.edits, || {
                                db.fact_fingerprint(key)
                            })
                        }))
        }
    }

    pub(crate) fn changed(&mut self, id: DerivationId) {
        self.worklist.insert((Reason::Changed, id));
    }

    /// Queues the readers of `keys` whose recorded fingerprint differs from
    /// the current value.
    pub(crate) fn readers_of<'a>(
        &mut self,
        db: &DbIndex,
        keys: impl IntoIterator<Item = &'a FactKey>,
    ) {
        for key in keys {
            self.touched.insert(key.clone());
            let readers = db
                .reader_graph()
                .stale_readers(key, self.key_holds(db, key))
                .collect::<Vec<_>>();
            for reader in readers {
                match reader.kind {
                    DerivationKind::Diagnostics => {}
                    DerivationKind::Source => {
                        self.stale_sources.insert(reader.file_id);
                    }
                    _ => {
                        self.worklist.insert((Reason::Read, reader));
                    }
                }
            }
        }
    }

    /// Marks `file_id`'s source layer as needing a rebuild.
    pub(crate) fn source_stale(&mut self, file_id: crate::FileId) {
        self.stale_sources.insert(file_id);
    }

    /// Every key whose value may have changed, emptied.
    pub(crate) fn take_touched(&mut self) -> FxHashSet<FactKey> {
        std::mem::take(&mut self.touched)
    }

    /// Notes that `file_ids`' source layers were rebuilt.
    pub(crate) fn note_rebuilt(&mut self, file_ids: &[crate::FileId]) {
        self.evaluated.extend(file_ids.iter().copied());
    }

    /// The files whose derivations were evaluated, sorted.
    pub(crate) fn evaluated_files(&self) -> Vec<crate::FileId> {
        let mut files = self.evaluated.iter().copied().collect::<Vec<_>>();
        files.sort_unstable();
        files
    }

    /// The files whose source layer must be rebuilt, emptied.
    pub(crate) fn take_stale_sources(&mut self) -> BTreeSet<crate::FileId> {
        std::mem::take(&mut self.stale_sources)
    }

    /// Evaluates the worklist to quiescence.
    pub(crate) fn run(
        &mut self,
        db: &mut DbIndex,
        recipe: &mut dyn FnMut(&mut DbIndex, DerivationId),
    ) {
        while let Some((reason, id)) = self.worklist.pop_first() {
            if reason == Reason::Read
                && !db
                    .reader_graph()
                    .is_stale(id, |key, seen| self.holds(db, key, seen))
            {
                if !self.edits.is_empty() {
                    let fingerprints = db
                        .reader_graph()
                        .reads_of(id)
                        .iter()
                        .map(|key| db.fact_fingerprint(key))
                        .collect();
                    db.reader_graph_mut().set_fingerprints(id, fingerprints);
                }
                continue;
            }
            let group = co_producers(db, id);
            for member in &group {
                self.worklist.remove(&(Reason::Changed, *member));
                self.worklist.remove(&(Reason::Read, *member));
            }
            self.evaluated.extend(
                group
                    .iter()
                    .map(|member| member.file_id)
                    .filter(|file_id| *file_id != crate::FileId::VIRTUAL),
            );
            let touched = evaluate_group(db, &group, recipe);
            // A member that now shares a key with a derivation outside the
            // group ran without it; the next evaluation takes both.
            let widened = group.iter().find_map(|member| {
                db.reader_graph()
                    .outputs_of(*member)
                    .iter()
                    .find_map(|key| {
                        if !key.is_primary() {
                            return None;
                        }
                        db.reader_graph()
                            .producers(key)
                            .find(|producer| {
                                producer.kind != DerivationKind::Source && !group.contains(producer)
                            })
                            .map(|producer| (key.clone(), producer))
                    })
            });
            if widened.is_some() {
                self.changed(id);
            }
            self.readers_of(db, touched.iter());
        }
    }
}

/// `id` and every derivation that shares a primary output key with it,
/// transitively, in canonical order.
fn co_producers(db: &DbIndex, id: DerivationId) -> Vec<DerivationId> {
    let graph = db.reader_graph();
    let mut group = BTreeSet::from([id]);
    let mut pending = vec![id];
    while let Some(member) = pending.pop() {
        for key in graph
            .outputs_of(member)
            .iter()
            .filter(|key| key.is_primary())
        {
            // The source layer is what a reset restores, not a co-producer.
            for producer in graph
                .producers(key)
                .filter(|producer| producer.kind != DerivationKind::Source)
            {
                if group.insert(producer) {
                    pending.push(producer);
                }
            }
        }
    }
    group.into_iter().collect()
}

/// Resets the group's primary outputs and re-runs it. Returns every key the
/// group touched: its previous outputs and its new ones.
fn evaluate_group(
    db: &mut DbIndex,
    group: &[DerivationId],
    recipe: &mut dyn FnMut(&mut DbIndex, DerivationId),
) -> FxHashSet<FactKey> {
    let mut touched = FxHashSet::default();
    for member in group {
        touched.extend(db.reader_graph().outputs_of(*member).iter().cloned());
    }
    for key in touched.iter().filter(|key| key.is_primary()) {
        db.reset_fact(key);
    }
    // Fingerprints are taken once the whole group has run: the group always
    // re-runs as a unit, so a member's read of what a later member writes is
    // internal to it, not a stale input.
    let mut evaluations = Vec::with_capacity(group.len());
    for member in group {
        derivation::begin(*member);
        recipe(db, *member);
        evaluations.push(derivation::end());
    }
    for evaluation in evaluations {
        let (id, reads, outputs) = evaluation.into_parts(|key| db.fact_fingerprint(key));
        touched.extend(outputs.iter().cloned());
        db.reader_graph_mut().replace(id, reads, outputs);
    }
    touched
}
