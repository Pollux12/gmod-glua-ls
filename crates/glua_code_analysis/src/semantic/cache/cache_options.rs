/// What inference may see. Statements see everything; a pass sees nothing it
/// produces or feeds: dynamic fields for the dynamic-field pass, call-site
/// parameters for the call-target, call-site-parameter and net-helper passes.
#[derive(Debug, Clone)]
pub struct CacheOptions {
    pub analysis_phase: LuaAnalysisPhase,
    /// Whether inference may consult the dynamic-field index.
    pub dynamic_fields_visible: bool,
    /// Whether inference may consult call-site parameter and receiver facts.
    pub call_site_params_visible: bool,
}

impl Default for CacheOptions {
    fn default() -> Self {
        Self {
            analysis_phase: LuaAnalysisPhase::default(),
            dynamic_fields_visible: true,
            call_site_params_visible: true,
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
