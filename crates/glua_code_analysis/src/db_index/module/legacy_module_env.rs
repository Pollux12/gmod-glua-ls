use rowan::TextSize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyModuleEnv {
    pub module_path: String,
    pub activation_position: TextSize,
    pub seeall: bool,
}

impl LegacyModuleEnv {
    pub fn package_name(&self) -> &str {
        self.module_path
            .rfind('.')
            .map(|idx| &self.module_path[..=idx])
            .unwrap_or("")
    }
}
