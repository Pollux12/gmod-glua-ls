use std::cell::RefCell;

use internment::ArcIntern;
use rustc_hash::FxHashMap;
use smol_str::SmolStr;

/// Per-thread memo in front of `ArcIntern`'s shared pool. Access-path strings
/// (`a.b.c`) and name literals are interned on every expression they appear
/// in, and `ArcIntern::new` pays a SipHash string hash plus a shard write-lock
/// each call; a bounded per-thread map takes that hit once per string per
/// worker instead. Capped: past the cap the pool is consulted directly, which
/// is correct but uncached — the cap bounds per-thread memory, not behavior.
const CAP: usize = 8192;

thread_local! {
    static SMOL_STR: RefCell<FxHashMap<SmolStr, ArcIntern<SmolStr>>> =
        RefCell::new(FxHashMap::default());
}

pub(crate) fn intern_smol_str(name: &str) -> ArcIntern<SmolStr> {
    SMOL_STR.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(interned) = cache.get(name) {
            return interned.clone();
        }
        let interned = ArcIntern::new(SmolStr::new(name));
        if cache.len() < CAP {
            cache.insert(SmolStr::new(name), interned.clone());
        }
        interned
    })
}
