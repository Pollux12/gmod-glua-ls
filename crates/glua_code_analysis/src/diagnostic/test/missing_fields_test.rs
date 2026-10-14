#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, VirtualWorkspace};

    /// A literal completed by the statements right after it is complete, on every
    /// path that reaches the check. The last two must keep reporting: nothing
    /// completes them before the value is used.
    #[test]
    fn literal_completed_before_first_use_is_complete() {
        let mut ws = VirtualWorkspace::new();

        assert!(
            ws.check_code_for(
                DiagnosticCode::MissingFields,
                r#"
            ---@class pairT
            ---@field exterior string
            ---@field interior string

            ---@type pairT
            local p = {}
            p.exterior = "a"
            p.interior = "b"
        "#
            ),
            "---@type path"
        );

        assert!(
            ws.check_code_for(
                DiagnosticCode::MissingFields,
                r#"
            ---@class pairF
            ---@field exterior string
            ---@field interior string

            ---@class holderF
            ---@field portals pairF
            local H = {}

            function H:make()
                self.portals = {}
                self.portals.exterior = "a"
                self.portals.interior = "b"
            end
        "#
            ),
            "declared-field path"
        );

        assert!(
            ws.check_code_for(
                DiagnosticCode::MissingFields,
                r#"
            ---@class pairL
            ---@field exterior string
            ---@field interior string

            ---@param v pairL
            local function take(v) return v end

            local t = {}
            t.exterior = "a"
            t.interior = "b"
            take(t)
        "#
            ),
            "plain local path"
        );

        assert!(
            !ws.check_code_for(
                DiagnosticCode::MissingFields,
                r#"
            ---@class pairN
            ---@field exterior string
            ---@field interior string

            ---@type pairN
            local p = {}
            p.exterior = "a"
        "#
            ),
            "still missing `interior`"
        );

        assert!(
            !ws.check_code_for(
                DiagnosticCode::MissingFields,
                r#"
            ---@class pairB
            ---@field exterior string
            ---@field interior string

            ---@param cond boolean
            local function build(cond)
                ---@type pairB
                local p = {}
                if cond then
                    p.exterior = "a"
                    p.interior = "b"
                end
                return p
            end
        "#
            ),
            "a nested branch does not always run"
        );
    }

    /// A documented default says the value arrives before anything reads it, so
    /// the literal may leave it out while the read type stays non-nullable — the
    /// reason the `@field` docs point users at `= value` rather than `?`. The
    /// `---@type` and call-argument paths are covered above; the declared-field
    /// assignment path and the read type are not.
    #[test]
    fn documented_default_satisfies_the_check_without_nulling_the_read() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class defcfg
            ---@field a number
            ---@field b number = 2

            ---@class defowner
            ---@field cfg defcfg
            local O = {}

            O.cfg = { a = 1 }
        "#
        ));

        let mut read_ws = VirtualWorkspace::new();
        read_ws.def(
            r#"
            ---@class defread
            ---@field a number
            ---@field b number = 2

            ---@type defread
            local v = { a = 1 }
            default_read_out = v.b
        "#,
        );
        let read_type = read_ws.expr_ty("default_read_out");
        assert_eq!(read_ws.humanize_type(read_type), "number");
    }

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
            DiagnosticCode::UndefinedMethod,
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
    fn test_later_partial_optional_field_suppresses_same_class_required() {
        let mut ws = VirtualWorkspace::new();
        // Generated annotations can be followed by custom partial overrides.
        // A later optional field for the same class should make the field
        // optional-for-writing without relying on source-order-specific table
        // construction inference.
        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class (partial) DrawTextData
            ---@field text string
            ---@field pos table

            ---@class (partial) DrawTextData
            ---@field text? string
            ---@field pos? table

            ---@type DrawTextData
            local text = {}
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

    #[test]
    fn inferred_nullable_api_result_accepts_partial_fallback_table() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class DebugInfo
            ---@field short_src string
            ---@field currentline integer
            ---@field source string

            ---@return DebugInfo?
            local function getinfo() end

            local info = getinfo()
            if not info then
                info = {
                    short_src = "",
                    currentline = 0,
                }
            end
            "#,
        ));
    }

    #[test]
    fn explicitly_typed_nullable_local_rejects_partial_fallback_table() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class DebugInfo
            ---@field short_src string
            ---@field currentline integer
            ---@field source string

            ---@return DebugInfo?
            local function getinfo() end

            ---@type DebugInfo?
            local info = getinfo()
            if not info then
                info = {
                    short_src = "",
                    currentline = 0,
                }
            end
            "#,
        ));
    }

    #[test]
    fn table_completed_after_typed_use_still_reports_missing_fields() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class Full
            ---@field required string

            ---@param value Full
            local function consume_typed(value) end

            local value = {}
            consume_typed(value)
            value.required = "ready"
            "#,
        ));
    }

    #[test]
    fn unannotated_local_table_out_param_skips_missing_fields() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class GlideHUDStackLayout
            ---@field width number
            ---@field height number

            ---@param out GlideHUDStackLayout
            local function GetHUDStackLayout(out)
                out.width = 100
                out.height = 50
            end

            local layout = {}
            GetHUDStackLayout(layout)
            "#,
        ));
    }

    /// A callee only completes the fields it always writes. A branch-nested
    /// write may not run, which is the same rule the caller-side scan applies.
    #[test]
    fn out_param_field_written_only_in_a_branch_still_reports() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class Branchy
            ---@field width number
            ---@field height number

            ---@param out Branchy
            local function fill(out, flag)
                out.width = 100
                if flag then
                    out.height = 50
                end
            end

            local box = {}
            fill(box, true)
            "#,
        ));
    }

    #[test]
    fn unannotated_local_table_filled_before_its_use_has_no_missing_fields() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class Ray
            ---@field start number
            ---@field endpos number
            ---@field mins number

            ---@param ray Ray
            local function trace(ray) return ray end

            local ray = { mins = 1 }
            function Fire(pos)
                ray.start = pos
                ray.endpos = pos + 1
                return trace(ray)
            end
            "#,
        ));
    }

    #[test]
    fn a_field_written_only_where_it_may_not_run_before_the_use_is_still_missing() {
        let mut ws = VirtualWorkspace::new();

        for completion in [
            "local function prepare() value.required = \"ready\" end",
            "if flag then value.required = \"ready\" end",
        ] {
            assert!(
                !ws.check_code_for(
                    DiagnosticCode::MissingFields,
                    &format!(
                        r#"
            ---@class Full
            ---@field required string

            ---@param value Full
            local function consume(value) end

            local value = {{}}
            {completion}
            consume(value)
            "#
                    ),
                ),
                "{completion}"
            );
        }
    }

    #[test]
    fn a_field_written_from_its_own_use_is_still_missing_there() {
        for before in ["observe(value)", ""] {
            let mut ws = VirtualWorkspace::new();

            assert!(
                !ws.check_code_for(
                    DiagnosticCode::MissingFields,
                    &format!(
                        r#"
            ---@class Full
            ---@field required string

            ---@param value any
            local function observe(value) end

            ---@param value Full
            ---@return string
            local function consume(value) return value.required end

            local value = {{}}
            {before}
            value.required = consume(value)
            "#
                    ),
                ),
                "{before}"
            );
        }
    }

    #[test]
    fn a_field_written_from_another_field_of_the_table_is_given() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class Size
            ---@field w number
            ---@field h number

            ---@type Size
            local size = {}
            size.w = 10
            size.h = size.w * 2
            "#,
        ));
    }

    #[test]
    fn a_function_written_onto_the_table_it_uses_is_given() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class Button
            ---@field DoClick fun()

            ---@type Button
            local button = {}
            button.DoClick = function() print(button) end
            "#,
        ));
    }

    #[test]
    fn a_table_of_functions_written_onto_the_table_they_use_is_given() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class Callbacks
            ---@field run fun()

            ---@class Job
            ---@field callbacks Callbacks

            ---@type Job
            local job = {}
            job.callbacks = { run = function() print(job) end }
            "#,
        ));
    }

    #[test]
    fn a_field_written_in_a_do_block_before_the_use_is_given() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class Full
            ---@field required string

            ---@param value any
            local function observe(value) end

            ---@param value Full
            local function consume(value) end

            local value = {}
            observe(value)
            do value.required = "ready" end
            consume(value)
            "#,
        ));
    }

    #[test]
    fn an_untyped_use_before_the_completion_does_not_cut_it_off() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class Full
            ---@field required string

            ---@param value any
            local function observe(value) end

            ---@param value Full
            local function consume(value) end

            local value = {}
            observe(value)
            value.required = "ready"
            consume(value)
            "#,
        ));
    }

    #[test]
    fn unannotated_local_table_passed_to_reader_reports_missing_fields() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class Sized
            ---@field width number
            ---@field height number

            ---@param size Sized
            local function measure(size) return size end

            local sized = { width = 100 }
            measure(sized)
            "#,
        ));
    }

    /// A field attached to a container element at runtime is not part of the
    /// class contract, so it must never become a *required* member of it. The
    /// same write at file scope already promotes nothing; sitting inside a block
    /// must not change the answer.
    #[test]
    fn undeclared_field_written_to_a_container_element_is_not_required() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            r#"
            ---@class Fires
            ---@field ID string

            ---@type table<string, Fires>
            local storeF = {}

            ---@param t Fires
            local function makeF(t) return t.ID end

            makeF({ ID = "a" })
            for _, v in pairs(storeF) do v.X = 5 end
            "#,
        ));
    }

    const MULTI_RETURNS: &str = r#"
        ---@return number, string, boolean
        local function m() return 1, "a", true end
        ---@return number, string
        local function m2() return 1, "a" end
        ---@return string, number
        local function g() return "a", 1 end
        ---@return number
        local function one() return 1 end
        ---@return number...
        local function va() return 1 end
    "#;

    #[test]
    fn trailing_call_fills_every_positional_slot_it_returns() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            &format!(
                r#"{MULTI_RETURNS}
                ---@type {{ [1]: number, [2]: string, [3]: boolean }}
                local t = {{ m() }}
                ---@type {{ [1]: number, [2]: string, [3]: boolean }}
                local u = {{ 1, m2() }}
                "#
            ),
        ));
    }

    #[test]
    fn trailing_call_after_named_field_fills_positional_slots() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            &format!(
                r#"{MULTI_RETURNS}
                ---@type {{ name: string, [1]: string, [2]: number }}
                local t = {{ name = "x", g() }}
                "#
            ),
        ));
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            &format!(
                r#"{MULTI_RETURNS}
                ---@type {{ name: string, [1]: string, [2]: number }}
                local t = {{ name = "x", g() }}
                "#
            ),
        ));
    }

    #[test]
    fn trailing_variadic_call_may_fill_remaining_slots() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::MissingFields,
            &format!(
                r#"{MULTI_RETURNS}
                ---@type {{ [1]: number, [2]: number, [3]: number }}
                local t = {{ va() }}
                "#
            ),
        ));
    }

    #[test]
    fn trailing_call_returning_too_few_values_is_still_missing() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::MissingFields,
            &format!(
                r#"{MULTI_RETURNS}
                ---@type {{ [1]: number, [2]: string, [3]: boolean }}
                local t = {{ one() }}
                "#
            ),
        ));
        assert!(!ws.check_code_for(
            DiagnosticCode::MissingFields,
            &format!(
                r#"{MULTI_RETURNS}
                ---@type {{ [1]: number, [2]: string, [3]: boolean }}
                local t = {{ m2() }}
                "#
            ),
        ));
    }

    /// Whether the literal `index_table` backs through `setmetatable` is
    /// reported missing a required field.
    fn reports_missing_with_index_table(index_table: &str) -> bool {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        !ws.check_code_for(
            DiagnosticCode::MissingFields,
            &format!(
                r#"
                ---@class MetaRequired
                ---@field foo string

                ---@param value MetaRequired
                local function consume(value) end

                local value = {{}}
                setmetatable(value, {{ __index = {index_table} }})
                consume(value)
                "#
            ),
        )
    }

    /// A field the metatable's `__index` table supplies is there when read;
    /// one it lacks stays missing. A supplied field of the wrong type is not
    /// missing: the type check reports it.
    #[test]
    fn a_field_an_index_table_supplies_is_not_missing() {
        assert_eq!(
            [r#"{ foo = "yes" }"#, "{ foo = 123 }", r#"{ bar = "yes" }"#]
                .map(reports_missing_with_index_table),
            [false, false, true]
        );
    }
}
