#[derive(Debug, Clone, Default)]
pub struct CacheOptions {
    pub analysis_phase: LuaAnalysisPhase,
}

#[derive(Debug, Clone, Copy, Default)]
pub enum LuaAnalysisPhase {
    // Ordered phase
    #[default]
    Ordered,
    // Unordered phase
    Unordered,
    // Force analysis phase
    Force,
    // Diagnostics phase - types are final, cache everything
    Diagnostics,
}

impl LuaAnalysisPhase {
    pub fn is_ordered(&self) -> bool {
        matches!(self, LuaAnalysisPhase::Ordered)
    }

    pub fn is_unordered(&self) -> bool {
        matches!(self, LuaAnalysisPhase::Unordered)
    }

    pub fn is_force(&self) -> bool {
        matches!(self, LuaAnalysisPhase::Force)
    }

    pub fn is_diagnostics(&self) -> bool {
        matches!(self, LuaAnalysisPhase::Diagnostics)
    }
}
