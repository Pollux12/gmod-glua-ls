#[derive(Debug, Clone)]
pub struct CacheOptions {
    pub analysis_phase: LuaAnalysisPhase,
    /// When true, name/index expression inference skips flow-sensitive narrowing
    /// and returns the declared type directly. This is much faster for large files
    /// and is safe during the initial analysis phase where precise narrowing is not
    /// needed (the resolve phase and diagnostics re-infer with full precision).
    pub skip_flow_narrowing: bool,
}

impl Default for CacheOptions {
    fn default() -> Self {
        Self {
            analysis_phase: LuaAnalysisPhase::Ordered,
            skip_flow_narrowing: false,
        }
    }
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
