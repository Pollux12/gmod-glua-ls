#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, LuaType, VirtualWorkspace};

    // ── @return (instance) ──────────────────────────────────────────────

    #[test]
    fn test_return_instance_creates_instance_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class InstancePanel
                ---@field name string

                ---@return (instance) InstancePanel
                local function Create() return {} end

                A = Create()
            "#,
        );
        let ty = ws.expr_ty("A");
        assert!(
            matches!(ty, LuaType::Instance(_)),
            "expected Instance type, got {:?}",
            ty
        );
    }

    #[test]
    fn test_return_instance_humanizes_as_base_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class InstanceHumanize
                ---@field name string

                ---@return (instance) InstanceHumanize
                local function Create() return {} end

                A = Create()
            "#,
        );
        let ty = ws.expr_ty("A");
        let humanized = ws.humanize_type(ty);
        assert_eq!(humanized, "InstanceHumanize");
    }

    #[test]
    fn test_return_instance_member_no_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        // Adding a field to an Instance should NOT produce undefined-field.
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class InstanceFieldPanel
                ---@field name string

                ---@return (instance) InstanceFieldPanel
                local function Create() return {} end

                local row = Create()
                row.customField = 42
            "#
        ));
    }

    #[test]
    fn test_return_instance_method_no_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        // Defining a method on an Instance should NOT produce undefined-field.
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class InstanceMethodPanel
                ---@field name string

                ---@return (instance) InstanceMethodPanel
                local function Create() return {} end

                local row = Create()

                function row:Refresh()
                end

                row:Refresh()
            "#
        ));
    }

    #[test]
    fn test_return_instance_no_global_pollution() {
        let mut ws = VirtualWorkspace::new();
        // Members added to one Instance must NOT appear on another instance.
        // Use (exact) base class and READ from the second instance.
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class (exact) InstancePollutionPanel
                ---@field name string

                ---@return (instance) InstancePollutionPanel
                local function Create() return {} end

                local a = Create()
                function a:LocalOnlyMethod() end

                local b = Create()
                b:LocalOnlyMethod()
            "#
        ));
    }

    #[test]
    fn test_return_instance_passes_type_check_for_base() {
        let mut ws = VirtualWorkspace::new();
        // Instance<Foo> should be accepted where Foo is expected.
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@class InstanceTypeCheckPanel
                ---@field name string

                ---@return (instance) InstanceTypeCheckPanel
                local function Create() return {} end

                ---@param p InstanceTypeCheckPanel
                local function accept(p) end

                local row = Create()
                accept(row)
            "#
        ));
    }

    // ── @type (instance) ────────────────────────────────────────────────

    #[test]
    fn test_type_instance_creates_instance_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class TypeInstancePanel
                ---@field name string

                ---@type (instance) TypeInstancePanel
                local row = {}
                A = row
            "#,
        );
        let ty = ws.expr_ty("A");
        assert!(
            matches!(ty, LuaType::Instance(_)),
            "expected Instance type from @type (instance), got {:?}",
            ty
        );
    }

    #[test]
    fn test_type_instance_member_no_undefined_field() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class TypeInstanceFieldPanel
                ---@field name string

                ---@type (instance) TypeInstanceFieldPanel
                local row = {}
                row.myField = 42
            "#
        ));
    }

    #[test]
    fn test_type_instance_no_global_pollution() {
        let mut ws = VirtualWorkspace::new();
        // Members on a @type (instance) should not leak to another instance.
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class (exact) TypeInstancePollutionPanel
                ---@field name string

                ---@type (instance) TypeInstancePollutionPanel
                local a = {}
                function a:LocalOnly() end

                ---@type TypeInstancePollutionPanel
                local b = {}
                b:LocalOnly()
            "#
        ));
    }

    // ── @return (definition) ────────────────────────────────────────────

    #[test]
    fn test_return_definition_creates_def_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class DefinitionRetPanel
                ---@field name string

                ---@return (definition) DefinitionRetPanel
                local function GetDef() return {} end

                A = GetDef()
            "#,
        );
        let ty = ws.expr_ty("A");
        assert!(
            matches!(ty, LuaType::Def(_)),
            "expected Def type from @return (definition), got {:?}",
            ty
        );
    }

    #[test]
    fn test_type_definition_creates_def_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class TypeDefinitionPanel
                ---@field name string

                ---@type (definition) TypeDefinitionPanel
                local panel = {}
                A = panel
            "#,
        );
        let ty = ws.expr_ty("A");
        assert!(
            matches!(ty, LuaType::Def(_)),
            "expected Def type from @type (definition), got {:?}",
            ty
        );
    }

    // ── Default behavior unchanged (no modifier) ────────────────────────

    #[test]
    fn test_return_default_is_ref_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class DefaultRefPanel
                ---@field name string

                ---@return DefaultRefPanel
                local function Create() return {} end

                A = Create()
            "#,
        );
        let ty = ws.expr_ty("A");
        assert!(
            matches!(ty, LuaType::Ref(_)),
            "expected Ref type (default return), got {:?}",
            ty
        );
    }

    #[test]
    fn test_type_default_is_ref_type() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class DefaultRefTypePanel
                ---@field name string

                ---@type DefaultRefTypePanel
                local panel = {}
                A = panel
            "#,
        );
        let ty = ws.expr_ty("A");
        assert!(
            matches!(ty, LuaType::Ref(_)),
            "expected Ref type (default @type), got {:?}",
            ty
        );
    }

    // ── Parenthesized type expressions still parse correctly ────────────

    #[test]
    fn test_parenthesized_type_not_confused_with_flag() {
        let mut ws = VirtualWorkspace::new();
        // Ensure that (number|string) in @type is NOT treated as a flag.
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@type (number|string)
                local x = 42

                ---@param val number|string
                local function accept(val) end

                accept(x)
            "#
        ));
    }

    #[test]
    fn test_nullable_literal_union_in_paren_not_confused_with_flag() {
        let mut ws = VirtualWorkspace::new();
        // Regression: (0.5|1|2|3|5)? must be parsed as type, not flag.
        ws.def(
            r#"
                ---@type (0.5|1|2|3|5)?
                local delay
                A = delay
            "#,
        );
        let ty = ws.expr_ty("A");
        // Should NOT be Unknown/nil — it should be a valid union or nullable type.
        assert!(
            !ty.is_unknown(),
            "parenthesized literal union should parse correctly, got Unknown"
        );
    }

    // ── Instance method func-stat + call on instance ────────────────────

    #[test]
    fn test_instance_func_stat_method_and_call() {
        let mut ws = VirtualWorkspace::new();
        // Full GMod pattern: factory → instance → func-stat → call
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class GMInstancePanel
                ---@field name string

                ---@return (instance) GMInstancePanel
                local function CreatePanel() return {} end

                local row = CreatePanel()

                function row:RefreshLayout()
                end

                function row:RefreshFieldVisibility()
                    self:RefreshLayout()
                end

                row:RefreshFieldVisibility()
            "#
        ));
    }

    #[test]
    fn test_instance_base_class_fields_still_accessible() {
        let mut ws = VirtualWorkspace::new();
        // Instance should still have access to base class fields.
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class InstanceBasePanel
                ---@field name string
                ---@field width number

                ---@return (instance) InstanceBasePanel
                local function Create() return {} end

                local row = Create()
                local _ = row.name
                local _ = row.width
            "#
        ));
    }

    #[test]
    fn test_instance_base_undefined_field_still_reported() {
        let mut ws = VirtualWorkspace::new();
        // Instance should still report undefined-field for fields not on the
        // base class and not locally defined.
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@class (exact) InstanceExactPanel
                ---@field name string

                ---@return (instance) InstanceExactPanel
                local function Create() return {} end

                local row = Create()
                local _ = row.nonExistentField
            "#
        ));
    }

    // ── Nullable instance type ──────────────────────────────────────────

    #[test]
    fn test_return_nullable_instance_creates_instance() {
        let mut ws = VirtualWorkspace::new();
        // @return (instance) MyClass? should produce an Instance-wrapped union.
        ws.def(
            r#"
                ---@class NullableInstancePanel
                ---@field name string

                ---@return (instance) NullableInstancePanel?
                local function MaybeCreate() return {} end

                A = MaybeCreate()
            "#,
        );
        let ty = ws.expr_ty("A");
        // Should not be Unknown — the type system should handle the nullable instance.
        assert!(
            !ty.is_unknown(),
            "nullable instance return should parse correctly, got Unknown"
        );
    }

    // ── Whitespace tolerance in flag parsing ─────────────────────────────

    #[test]
    fn test_return_instance_flag_with_extra_whitespace() {
        let mut ws = VirtualWorkspace::new();
        // Flag with spaces around keyword: ( instance ) should still work.
        ws.def(
            r#"
                ---@class WhitespaceInstancePanel
                ---@field name string

                ---@return ( instance ) WhitespaceInstancePanel
                local function Create() return {} end

                A = Create()
            "#,
        );
        let ty = ws.expr_ty("A");
        assert!(
            matches!(ty, LuaType::Instance(_)),
            "expected Instance type with whitespace in flag, got {:?}",
            ty
        );
    }
}
