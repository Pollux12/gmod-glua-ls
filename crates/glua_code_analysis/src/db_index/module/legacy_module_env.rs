use crate::db_index::derivation::hash_local;
use rowan::TextSize;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyModuleEnv {
    pub module_path: String,
    pub activation_position: TextSize,
    pub seeall: bool,
}

impl Hash for LegacyModuleEnv {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self {
            module_path,
            activation_position,
            seeall,
        } = self;
        module_path.hash(state);
        hash_local(activation_position, state);
        seeall.hash(state);
    }
}

impl LegacyModuleEnv {
    pub fn package_name(&self) -> &str {
        self.module_path
            .rfind('.')
            .map(|idx| &self.module_path[..=idx])
            .unwrap_or("")
    }
}
