//! Under `GLUALS_PROFILE`: how many inferences and flow walks were computed,
//! against how many distinct ones, per derivation kind.
//!
//! Each thread counts into its own table, registered once, so counting takes
//! only that thread's uncontended lock; [`report`] merges the tables.

use std::collections::BTreeMap;
use std::hash::{BuildHasher, Hash};
use std::sync::{Arc, Mutex, MutexGuard};

use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};

use crate::db_index::derivation::{self, DerivationKind};

/// What was computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Work {
    /// An expression inferred on a memo miss.
    Infer,
    /// A flow walk run on a memo miss.
    Flow,
}

#[derive(Default)]
struct Bucket {
    computed: u64,
    distinct: FxHashSet<u64>,
}

/// Per work and derivation kind; `None` is work outside any derivation.
type Table = FxHashMap<(Work, Option<DerivationKind>), Bucket>;

static TABLES: Mutex<Vec<Arc<Mutex<Table>>>> = Mutex::new(Vec::new());

/// The distinct keys the last `index` report saw, so later reports can say
/// how much of their work indexing had already done.
static INDEXED: Mutex<BTreeMap<Work, FxHashSet<u64>>> = Mutex::new(BTreeMap::new());

thread_local! {
    static TABLE: Arc<Mutex<Table>> = {
        let table = Arc::<Mutex<Table>>::default();
        lock(&TABLES).push(Arc::clone(&table));
        table
    };
}

#[cfg(test)]
thread_local! {
    static FORCED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[inline]
fn counting() -> bool {
    #[cfg(test)]
    if FORCED.with(std::cell::Cell::get) {
        return true;
    }
    super::profiling()
}

/// Counts one `work` computed for `key`, under the current derivation's kind.
#[inline]
pub(crate) fn computed<K: Hash>(work: Work, key: impl FnOnce() -> K) {
    if !counting() {
        return;
    }
    let hash = FxBuildHasher.hash_one(key());
    let kind = derivation::current().map(|id| id.kind);
    TABLE.with(|table| {
        let mut table = lock(table);
        let bucket = table.entry((work, kind)).or_default();
        bucket.computed += 1;
        bucket.distinct.insert(hash);
    });
}

/// Prints and resets every thread's counts under `GLUALS_PROFILE`. The
/// `index` report is remembered, so later ones count work indexing had done.
pub fn report(label: &str) {
    if !counting() {
        return;
    }
    let mut merged = BTreeMap::<(Work, Option<DerivationKind>), Bucket>::new();
    for table in lock(&TABLES).iter() {
        for (key, bucket) in std::mem::take(&mut *lock(table)) {
            let entry = merged.entry(key).or_default();
            entry.computed += bucket.computed;
            entry.distinct.extend(bucket.distinct);
        }
    }
    let mut indexed = lock(&INDEXED);
    for work in [Work::Infer, Work::Flow] {
        let mut rows = merged
            .iter()
            .filter(|((row_work, _), _)| *row_work == work)
            .collect::<Vec<_>>();
        rows.sort_by_key(|(_, bucket)| std::cmp::Reverse(bucket.computed));
        let mut union = FxHashSet::default();
        let mut computed = 0;
        for ((_, kind), bucket) in rows {
            eprintln!(
                "[work] {label} {work:?} {kind:?}: computed {} distinct {} ({:.2}x)",
                bucket.computed,
                bucket.distinct.len(),
                ratio(bucket.computed, bucket.distinct.len())
            );
            computed += bucket.computed;
            union.extend(bucket.distinct.iter().copied());
        }
        let done_by_indexing = indexed.get(&work).map_or(0, |keys| {
            union.iter().filter(|key| keys.contains(key)).count()
        });
        eprintln!(
            "[work] {label} {work:?} total: computed {computed} distinct {} ({:.2}x); indexing computed {done_by_indexing} of them",
            union.len(),
            ratio(computed, union.len())
        );
        if label == "index" {
            indexed.insert(work, union);
        }
    }
}

fn ratio(computed: u64, distinct: usize) -> f64 {
    computed as f64 / distinct.max(1) as f64
}

/// Counts this thread's work while `f` runs, and returns `f`'s result with
/// the counts, per work and derivation kind: `(computed, distinct)`.
#[cfg(test)]
pub(crate) fn counted<T>(
    f: impl FnOnce() -> T,
) -> (T, BTreeMap<(Work, Option<DerivationKind>), (u64, usize)>) {
    FORCED.with(|forced| forced.set(true));
    TABLE.with(|table| lock(table).clear());
    let result = f();
    FORCED.with(|forced| forced.set(false));
    let counts = TABLE.with(|table| {
        std::mem::take(&mut *lock(table))
            .into_iter()
            .map(|(key, bucket)| (key, (bucket.computed, bucket.distinct.len())))
            .collect()
    });
    (result, counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inference_is_counted_under_the_derivation_kind_that_computed_it() {
        let mut ws = crate::VirtualWorkspace::new();
        let (_, counts) = counted(|| {
            ws.def_file(
                "lua/autorun/counted.lua",
                "local a = { x = 1 }\nlocal b = a.x\n",
            )
        });
        let (computed, distinct) = counts
            .get(&(Work::Infer, Some(DerivationKind::LocalStat)))
            .copied()
            .unwrap_or_default();
        assert!(distinct > 0 && computed >= distinct as u64);
    }
}
