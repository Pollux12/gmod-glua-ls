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

pub(crate) fn clear_smol_str_intern_cache() {
    SMOL_STR.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.clear();
        cache.shrink_to_fit();
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InternedName(pub(super) ArcIntern<SmolStr>);

impl InternedName {
    pub(super) fn new(name: &str) -> Self {
        Self(intern_smol_str(name))
    }
}

impl std::ops::Deref for InternedName {
    type Target = SmolStr;

    fn deref(&self) -> &SmolStr {
        &self.0
    }
}

impl std::borrow::Borrow<str> for InternedName {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl std::hash::Hash for InternedName {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}
