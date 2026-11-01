use std::borrow::Borrow;
use std::hash::{Hash, Hasher};

use internment::ArcIntern;
use smol_str::SmolStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalId(pub ArcIntern<SmolStr>);

/// Content hashing: `ArcIntern` hashes the pointer, which forbids borrowed
/// lookups. Two equal ids always hold equal names, so hashing the name keeps
/// the `k1 == k2 => hash(k1) == hash(k2)` contract while letting `&str`
/// queries find the same buckets without interning first.
impl Hash for GlobalId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.get_name().hash(state);
    }
}

impl Borrow<str> for GlobalId {
    fn borrow(&self) -> &str {
        self.get_name()
    }
}

/// Per-thread memo in front of `ArcIntern`'s shared pool, matching the
/// `LuaTypeDeclId` constructors: global-name lookups intern per call, and the
/// pool's SipHash + shard write-lock repeats for every repeat name. Capped;
/// past the cap the pool is consulted directly — the cap bounds per-thread
/// memory, not behavior.
mod global_id_intern {
    use super::GlobalId;
    use rustc_hash::FxHashMap;
    use smol_str::SmolStr;
    use std::cell::RefCell;

    const CAP: usize = 8192;

    thread_local! {
        static CACHE: RefCell<FxHashMap<SmolStr, GlobalId>> =
            RefCell::new(FxHashMap::default());
    }

    pub fn intern(name: &str) -> GlobalId {
        CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if let Some(id) = cache.get(name) {
                return id.clone();
            }
            let id = GlobalId(internment::ArcIntern::new(SmolStr::new(name)));
            if cache.len() < CAP {
                cache.insert(SmolStr::new(name), id.clone());
            }
            id
        })
    }
}

impl GlobalId {
    /// The interned name's address: equal for equal ids while either lives.
    pub(crate) fn interned_address(&self) -> u64 {
        std::ptr::from_ref::<SmolStr>(&*self.0) as usize as u64
    }

    pub fn new(name: &str) -> Self {
        global_id_intern::intern(name)
    }

    pub fn get_name(&self) -> &str {
        self.0.as_ref()
    }

    pub fn get_prev_id(&self) -> Option<GlobalId> {
        glua_parser::split_last_path_segment(self.get_name()).map(|(owner, _)| GlobalId::new(owner))
    }
}
