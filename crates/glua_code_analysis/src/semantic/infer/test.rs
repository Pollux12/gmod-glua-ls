#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn test_custom_binary() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@class AA
        ---@operator pow(number): AA

        ---@type AA
        a = {}
        "#,
        );

        let ty = ws.expr_ty(
            r#"
        a ^ 1
        "#,
        );
        let expected = ws.ty("AA");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_issue_559() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Origin
            ---@operator add(Origin):Origin

            ---@alias AliasType Origin

            ---@type AliasType
            local x1
            ---@type AliasType
            local x2

            A = x1 + x2
        "#,
        );

        let ty = ws.expr_ty("A");
        let expected = ws.ty("Origin");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_issue_867() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local a --- @type { foo? : { bar: { baz: number } } }

            local b = a.foo.bar -- a.foo may be nil (correct)

            c = b.baz -- b may be nil (incorrect)
        "#,
        );

        let ty = ws.expr_ty("c");
        let expected = ws.ty("number");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_isvalid_shadowed_local_no_narrow() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Entity
            ---@field health integer

            ---@return Entity?
            local function get_ent() end

            local IsValid = function(obj)
                return obj ~= nil
            end

            local ent = get_ent()
            if IsValid(ent) then
                local _health = ent.health
            end
            "#
        ));
    }

    #[test]
    fn test_isvalid_false_branch_not_narrowed_to_nil() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Entity

            ---@return Entity?
            local function get_ent() end

            function IsValid(obj)
                return obj ~= nil
            end

            ---@param value nil
            local function expects_nil(value) end

            local ent = get_ent()
            if not IsValid(ent) then
                expects_nil(ent)
            end
            "#
        ));
    }
}
