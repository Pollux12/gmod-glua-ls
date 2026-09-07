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

impl GlobalId {
    pub fn new(name: &str) -> Self {
        Self(ArcIntern::new(SmolStr::new(name)))
    }

    pub fn get_name(&self) -> &str {
        self.0.as_ref()
    }

    pub fn get_prev_id(&self) -> Option<GlobalId> {
        let name = self.get_name();
        if let Some(pos) = name.rfind('.') {
            let new_name = &name[..pos];
            return Some(GlobalId::new(new_name));
        }

        None
    }
}
