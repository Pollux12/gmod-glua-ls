use glua_diagnostic_macro::LuaDiagnosticMacro;
use glua_parser::LuaLanguageLevel;
use lsp_types::DiagnosticSeverity;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, LuaDiagnosticMacro,
)]
#[serde(rename_all = "kebab-case")]
pub enum DiagnosticCode {
    /// Syntax error
    SyntaxError,
    /// Doc syntax error
    DocSyntaxError,
    /// Type not found
    TypeNotFound,
    /// Missing return statement
    MissingReturn,
    /// Param Type not match
    ParamTypeMismatch,
    /// Missing parameter
    MissingParameter,
    /// Redundant parameter
    RedundantParameter,
    /// Unreachable code
    UnreachableCode,
    /// Unused
    Unused,
    /// Unused implicit self parameter
    UnusedSelf,
    /// Undefined global
    UndefinedGlobal,
    /// Deprecated
    Deprecated,
    /// Access invisible
    AccessInvisible,
    /// Discard return value
    DiscardReturns,
    /// Undefined field
    UndefinedField,
    /// Local const reassign
    LocalConstReassign,
    /// Iter variable reassign
    IterVariableReassign,
    /// Duplicate type
    DuplicateType,
    /// Redefined local
    RedefinedLocal,
    /// Redefined label
    RedefinedLabel,
    /// Code style check
    CodeStyleCheck,
    /// Need check nil
    NeedCheckNil,
    /// Await in sync
    AwaitInSync,
    /// Doc tag usage error
    AnnotationUsageError,
    /// Return type mismatch
    ReturnTypeMismatch,
    /// Missing return value
    MissingReturnValue,
    /// Redundant return value
    RedundantReturnValue,
    /// Undefined Doc Param
    UndefinedDocParam,
    /// Duplicate doc field
    DuplicateDocField,
    /// Unknown doc annotation
    UnknownDocTag,
    /// Missing fields
    MissingFields,
    /// Inject Field
    InjectField,
    /// Circle Doc Class
    CircleDocClass,
    /// Incomplete signature doc
    IncompleteSignatureDoc,
    /// Missing global doc
    MissingGlobalDoc,
    /// Assign type mismatch
    AssignTypeMismatch,
    /// Duplicate require
    DuplicateRequire,
    /// non-literal-expressions-in-assert
    NonLiteralExpressionsInAssert,
    /// Unbalanced assignments
    UnbalancedAssignments,
    /// unnecessary-assert
    UnnecessaryAssert,
    /// unnecessary-if
    UnnecessaryIf,
    /// duplicate-set-field
    DuplicateSetField,
    /// duplicate-index
    DuplicateIndex,
    /// generic-constraint-mismatch
    GenericConstraintMismatch,
    /// cast-type-mismatch
    CastTypeMismatch,
    /// require-module-not-visible
    RequireModuleNotVisible,
    /// enum-value-mismatch
    EnumValueMismatch,
    /// preferred-local-alias
    PreferredLocalAlias,
    /// readonly
    ReadOnly,
    /// Global variable defined in non-module scope
    GlobalInNonModule,
    /// attribute-param-type-mismatch
    AttributeParamTypeMismatch,
    /// attribute-missing-parameter
    AttributeMissingParameter,
    /// attribute-redundant-parameter
    AttributeRedundantParameter,
    /// invert-if
    InvertIf,
    /// Call to a non-callable value
    CallNonCallable,
    /// gmod-invalid-hook-name
    GmodInvalidHookName,
    /// gmod-realm-mismatch (strict realm mismatch)
    #[serde(alias = "gmod-realm-misuse")]
    GmodRealmMismatch,
    /// gmod-realm-mismatch-heuristic (heuristic realm mismatch)
    #[serde(alias = "gmod-realm-misuse-risky")]
    GmodRealmMismatchHeuristic,
    /// gmod-unknown-realm (realm could not be resolved)
    GmodUnknownRealm,
    /// gmod-unknown-net-message
    GmodUnknownNetMessage,
    /// gmod-net-read-write-type-mismatch
    GmodNetReadWriteTypeMismatch,
    /// gmod-net-read-write-order-mismatch
    GmodNetReadWriteOrderMismatch,
    /// gmod-net-missing-network-counterpart
    GmodNetMissingNetworkCounterpart,
    /// gmod-duplicate-system-registration
    GmodDuplicateSystemRegistration,
    #[serde(other)]
    None,
}

// Update functions to match enum variants
pub fn get_default_severity(code: DiagnosticCode) -> DiagnosticSeverity {
    match code {
        DiagnosticCode::SyntaxError => DiagnosticSeverity::ERROR,
        DiagnosticCode::DocSyntaxError => DiagnosticSeverity::ERROR,
        DiagnosticCode::TypeNotFound => DiagnosticSeverity::WARNING,
        DiagnosticCode::MissingReturn => DiagnosticSeverity::WARNING,
        DiagnosticCode::ParamTypeMismatch => DiagnosticSeverity::WARNING,
        DiagnosticCode::MissingParameter => DiagnosticSeverity::WARNING,
        DiagnosticCode::UnreachableCode => DiagnosticSeverity::HINT,
        DiagnosticCode::Unused => DiagnosticSeverity::HINT,
        DiagnosticCode::UnusedSelf => DiagnosticSeverity::HINT,
        DiagnosticCode::UndefinedGlobal => DiagnosticSeverity::ERROR,
        DiagnosticCode::Deprecated => DiagnosticSeverity::HINT,
        DiagnosticCode::AccessInvisible => DiagnosticSeverity::WARNING,
        DiagnosticCode::DiscardReturns => DiagnosticSeverity::WARNING,
        DiagnosticCode::UndefinedField => DiagnosticSeverity::WARNING,
        DiagnosticCode::LocalConstReassign => DiagnosticSeverity::ERROR,
        DiagnosticCode::DuplicateType => DiagnosticSeverity::WARNING,
        DiagnosticCode::AnnotationUsageError => DiagnosticSeverity::ERROR,
        DiagnosticCode::RedefinedLocal => DiagnosticSeverity::HINT,
        DiagnosticCode::DuplicateRequire => DiagnosticSeverity::HINT,
        DiagnosticCode::IterVariableReassign => DiagnosticSeverity::ERROR,
        DiagnosticCode::PreferredLocalAlias => DiagnosticSeverity::HINT,
        DiagnosticCode::CallNonCallable => DiagnosticSeverity::WARNING,
        DiagnosticCode::NeedCheckNil => DiagnosticSeverity::HINT,
        DiagnosticCode::GenericConstraintMismatch => DiagnosticSeverity::INFORMATION,
        DiagnosticCode::GmodInvalidHookName => DiagnosticSeverity::WARNING,
        DiagnosticCode::GmodRealmMismatch => DiagnosticSeverity::ERROR,
        DiagnosticCode::GmodRealmMismatchHeuristic => DiagnosticSeverity::ERROR,
        DiagnosticCode::GmodUnknownRealm => DiagnosticSeverity::HINT,
        DiagnosticCode::GmodUnknownNetMessage => DiagnosticSeverity::WARNING,
        DiagnosticCode::GmodNetReadWriteTypeMismatch => DiagnosticSeverity::WARNING,
        DiagnosticCode::GmodNetReadWriteOrderMismatch => DiagnosticSeverity::WARNING,
        DiagnosticCode::GmodNetMissingNetworkCounterpart => DiagnosticSeverity::WARNING,
        DiagnosticCode::GmodDuplicateSystemRegistration => DiagnosticSeverity::HINT,
        _ => DiagnosticSeverity::WARNING,
    }
}

pub fn is_code_default_enable(code: &DiagnosticCode, level: LuaLanguageLevel) -> bool {
    match code {
        DiagnosticCode::IterVariableReassign => level >= LuaLanguageLevel::Lua55,
        DiagnosticCode::CodeStyleCheck => false,
        DiagnosticCode::IncompleteSignatureDoc => false,
        DiagnosticCode::MissingGlobalDoc => false,
        DiagnosticCode::UnknownDocTag => false,
        DiagnosticCode::InjectField => false,
        DiagnosticCode::UnnecessaryIf => false,
        DiagnosticCode::RedundantReturnValue => false,
        DiagnosticCode::UnnecessaryAssert => false,
        DiagnosticCode::GlobalInNonModule => false,
        DiagnosticCode::UnusedSelf => false,
        DiagnosticCode::MissingReturn => false,
        DiagnosticCode::DuplicateType => false,
        DiagnosticCode::ReturnTypeMismatch => false,
        DiagnosticCode::DuplicateSetField => false,
        DiagnosticCode::CallNonCallable => false,
        DiagnosticCode::InvertIf => false,

        // gmod diagnostics
        DiagnosticCode::GmodRealmMismatch => true,
        DiagnosticCode::GmodRealmMismatchHeuristic => true,
        DiagnosticCode::GmodUnknownRealm => true,
        DiagnosticCode::GmodDuplicateSystemRegistration => true,
        DiagnosticCode::GmodUnknownNetMessage => true,
        DiagnosticCode::GmodNetReadWriteTypeMismatch => true,
        DiagnosticCode::GmodNetReadWriteOrderMismatch => true,
        DiagnosticCode::GmodNetMissingNetworkCounterpart => true,
        DiagnosticCode::GmodInvalidHookName => true,

        // neovim-code-style
        DiagnosticCode::NonLiteralExpressionsInAssert => false,

        _ => true,
    }
}

impl DiagnosticCode {
    pub fn from_name_or_legacy(name: &str) -> Self {
        match name {
            "gmod-realm-misuse" => DiagnosticCode::GmodRealmMismatch,
            "gmod-realm-misuse-risky" => DiagnosticCode::GmodRealmMismatchHeuristic,
            _ => name.parse().unwrap_or(DiagnosticCode::None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{get_default_severity, is_code_default_enable};
    use crate::{DiagnosticCode, Emmyrc};
    use glua_parser::LuaLanguageLevel;
    use googletest::prelude::*;
    use lsp_types::DiagnosticSeverity;
    use serde_json::json;

    #[gtest]
    fn legacy_realm_diagnostic_codes_map_to_renamed_codes() {
        assert_eq!(
            DiagnosticCode::from_name_or_legacy("gmod-realm-misuse"),
            DiagnosticCode::GmodRealmMismatch
        );
        assert_eq!(
            DiagnosticCode::from_name_or_legacy("gmod-realm-misuse-risky"),
            DiagnosticCode::GmodRealmMismatchHeuristic
        );
    }

    #[gtest]
    fn legacy_realm_codes_are_accepted_in_config_diagnostic_lists() {
        let parsed: Emmyrc = serde_json::from_value(json!({
            "diagnostics": {
                "disable": ["gmod-realm-misuse"],
                "enables": ["gmod-realm-misuse-risky"]
            }
        }))
        .expect("valid config");

        assert_that!(
            parsed.diagnostics.disable,
            contains(eq(&DiagnosticCode::GmodRealmMismatch))
        );
        assert_that!(
            parsed.diagnostics.enables,
            contains(eq(&DiagnosticCode::GmodRealmMismatchHeuristic))
        );
    }

    #[gtest]
    fn gmod_diagnostics_are_default_enabled() {
        let level = LuaLanguageLevel::Lua54;
        assert_that!(
            is_code_default_enable(&DiagnosticCode::GmodRealmMismatch, level),
            eq(true)
        );
        assert_that!(
            is_code_default_enable(&DiagnosticCode::GmodRealmMismatchHeuristic, level),
            eq(true)
        );
        assert_that!(
            is_code_default_enable(&DiagnosticCode::GmodUnknownRealm, level),
            eq(true)
        );
        assert_that!(
            is_code_default_enable(&DiagnosticCode::GmodUnknownNetMessage, level),
            eq(true)
        );
        assert_that!(
            is_code_default_enable(&DiagnosticCode::GmodDuplicateSystemRegistration, level),
            eq(true)
        );
        assert_that!(
            is_code_default_enable(&DiagnosticCode::GmodInvalidHookName, level),
            eq(true)
        );
    }

    #[gtest]
    fn gmod_realm_default_severity_matches_expected_levels() {
        assert_that!(
            get_default_severity(DiagnosticCode::GmodRealmMismatch),
            eq(DiagnosticSeverity::ERROR)
        );
        assert_that!(
            get_default_severity(DiagnosticCode::GmodRealmMismatchHeuristic),
            eq(DiagnosticSeverity::ERROR)
        );
        assert_that!(
            get_default_severity(DiagnosticCode::GmodUnknownRealm),
            eq(DiagnosticSeverity::HINT)
        );
    }
}
