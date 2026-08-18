#[derive(Debug, Clone)]
pub struct CacheOptions {
    pub analysis_phase: LuaAnalysisPhase,
    /// Whether inference may consult the dynamic-field index.
    pub dynamic_fields_visible: bool,
    /// Whether this cache belongs to the dynamic-field collection pass
    /// itself.
    pub building_dynamic_field_index: bool,
}

impl Default for CacheOptions {
    fn default() -> Self {
        Self {
            analysis_phase: LuaAnalysisPhase::default(),
            dynamic_fields_visible: true,
            building_dynamic_field_index: false,
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
