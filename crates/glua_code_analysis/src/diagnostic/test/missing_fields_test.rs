#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn test_missing_fields() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class test
            ---@field a number

            ---@type test
            local test = {}
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class test1
            ---@field a number

            ---@class test2: test1

            ---@type test
            local test = {}
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class test3
            ---@field a number

            ---@class test4: test3
            ---@field b number

            ---@type test
            local test = {
                a = 1,
                b = 2,
            }
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class test5
            ---@field a? number

            ---@class test6: test5
            ---@field b number

            ---@type test5
            local test = {
                b = 2,
            }
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class test7
            ---@field a number

            local test = {}
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class test8
            ---@field a number
            ---@type test8
            local test
        "#
        ));
    }

    #[test]
    fn test_override_optional() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class test1
            ---@field a? number

            ---@class test2: test1
            ---@field a number

            ---@type test2
            local test = {
            }
        "#
        ));
    }

    #[test]
    fn test_generic() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class test1<T>
            ---@field a number

            ---@type test1<string>
            local test = {
            }
        "#
        ));
    }

    #[test]
    fn test_object_type() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class test1: { a: number }

            ---@type test1
            local test = {
            }
        "#
        ));
    }

    #[test]
    fn test_issue_262() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
--- @class D11.Opts
--- @field field? any

--- @param opts D11.Opts
local function foo(opts) end

foo({})
        "#
        ));
    }

    #[test]
    fn test_1() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
                ---@type table
                local a = {}

                print(a[1])
        "#
        ));
    }

    #[test]
    fn test_issue_296() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
                ---@generic T
                ---@param table table
                ---@param metatable {__index: T}
                ---@return T
                local function abc(table, metatable) end

                ---@class B
                local B

                --- @return B
                function newB()
                    local self = abc({}, { __index = B })
                    self:notmethod()
                    return self
                end
        "#
        ));
    }

    #[test]
    fn test_issue_302() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
                ---@class data
                data = {}
                data.raw = {}
                data.is_demo = false

                --- @param _self data
                function data.extend(_self, _otherdata)
                -- Impl
                end

                data:extend({
                {
                    type = "item",
                    name = "my-item",
                },
                })
        "#
        ));
    }

    #[test]
    fn test_issue_449() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class D31.A
            ---@field public a string

            ---@class D31.B
            ---@field public b string


            ---@param ab D31.A & D31.B
            local function f(ab)
            end

            f({})
        "#
        ));
    }

    #[test]
    fn test_union_table_generic() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
        ---@class RingBuffer<T>
        ---@field a number

        ---@class LiveList<T>
        ---@field list table<integer, T> | RingBuffer<T>
        "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@type LiveList
            local LiveList

            LiveList.list = {}
        "#
        ));
    }

    #[test]
    fn test_union_with_array_keeps_missing_fields_for_record_like_table() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class NeedsA
            ---@field a number

            ---@param v NeedsA | NeedsA[]
            local function takes(v) end

            takes({ b = 1 })
            "#
        ));
    }

    #[test]
    fn test_union_with_array_skips_missing_fields_for_array_like_table() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class NeedsA
            ---@field a number

            ---@param v NeedsA | NeedsA[]
            local function takes(v) end

            takes({ { a = 1 } })
            "#
        ));
    }

    #[test]
    fn test_method_members_do_not_count_as_missing_fields() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class CAMI_PRIVILEGE
            ---@field Name string
            ---@field MinAccess "'user'" | "'admin'" | "'superadmin'"
            ---@field Description string?
            local CAMI_PRIVILEGE = {}

            function CAMI_PRIVILEGE:HasAccess(actor, target)
                return true
            end

            ---@param privilege CAMI_PRIVILEGE
            local function register_privilege(privilege)
            end

            register_privilege({
                Name = "DarkRP_SetMoney",
                MinAccess = "superadmin",
            })
            "#
        ));
    }

    #[test]
    fn test_field_function_members_still_count_as_missing_fields() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class CAMI_PRIVILEGE
            ---@field Name string
            ---@field HasAccess fun(): boolean

            ---@param privilege CAMI_PRIVILEGE
            local function register_privilege(privilege)
            end

            register_privilege({
                Name = "DarkRP_SetMoney",
            })
            "#
        ));
    }

    #[test]
    fn test_defaulted_field_not_required_for_table_literal() {
        let mut ws = VirtualWorkspace::new();
        // Defaulted field should be omit-able in a table literal.
        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class TraceResult
            ---@field Hit boolean=false
            ---@field HitPos number

            ---@type TraceResult
            local tr = { HitPos = 1 }
            "#
        ));
    }

    #[test]
    fn test_defaulted_field_not_required_in_call_argument() {
        let mut ws = VirtualWorkspace::new();
        // Reported shape: partial table to a function param should not
        // trigger missing-fields for defaulted class fields.
        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class FontData
            ---@field font string
            ---@field size number=14
            ---@field weight number=500
            ---@field antialias boolean=true

            ---@param data FontData
            local function CreateFont(data) end

            CreateFont({ font = "Arial", size = 48 })
            "#
        ));
    }

    #[test]
    fn test_defaulted_generic_field_not_required() {
        let mut ws = VirtualWorkspace::new();
        // Defaulted fields on generic classes should also be optional-for-writing.
        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class Container<T>
            ---@field value T
            ---@field count number=0

            ---@type Container<string>
            local c = { value = "hello" }
            "#
        ));
    }

    #[test]
    fn test_child_default_suppresses_parent_required() {
        let mut ws = VirtualWorkspace::new();
        // Child redeclares a parent-required field with a default;
        // the default should make it optional-for-writing.
        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class Base
            ---@field a number

            ---@class Derived: Base
            ---@field a number=0

            ---@type Derived
            local d = {}
            "#
        ));
    }

    #[test]
    fn test_child_required_overrides_parent_default() {
        let mut ws = VirtualWorkspace::new();
        // Child redeclares a parent-defaulted field WITHOUT a default;
        // it should still be required.
        assert!(!ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class Base
            ---@field a number=0

            ---@class Derived: Base
            ---@field a number

            ---@type Derived
            local d = {}
            "#
        ));
    }

    #[test]
    fn test_defaulted_field_still_required_when_provided_is_still_checked() {
        let mut ws = VirtualWorkspace::new();
        // When the defaulted field IS provided, other non-defaulted fields
        // are still required.
        assert!(!ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class Config
            ---@field name string
            ---@field debug boolean=false

            ---@type Config
            local cfg = { debug = true }
            "#
        ));
    }
}
