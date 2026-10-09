#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn test_1() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@generic T: string
            ---@param name  `T` 类名
            ---@return T
            local function meta(name)
                return name
            end

            ---@class Class
            local class = meta("class")
            "#
        ));
    }

    #[test]
    fn test_2() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@class Diagnostic.Test7
            Diagnostic = {}

            ---@param a Diagnostic.Test7
            ---@param b number
            ---@return number
            function Diagnostic:add(a, b)
                return a + b
            end

            local add = Diagnostic.add
            "#
        ));
    }

    #[test]
    fn test_initialized_assignment_chain_does_not_report_assign_type_mismatch() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                WepHolster = {}
                WepHolster.defData = {}
                WepHolster.defData["weapon_pistol"] = {}
                WepHolster.defData["weapon_pistol"].Model = "models/weapons/w_pistol.mdl"
                WepHolster.defData["weapon_pistol"].BoneOffset = {
                    Vector(1, 2, 3),
                    Angle(4, 5, 6),
                }
            "#
        ));
    }

    #[test]
    fn test_inferred_table_literal_fields_can_widen_after_later_assignments() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@class Vector

                ---@return Vector
                local function Vector()
                end

                local cfg = {
                    color = "white",
                    enabled = false,
                }

                cfg.color = Vector()
                cfg.enabled = { reason = "starting up" }
            "#
        ));
    }

    #[test]
    fn test_default_initialized_assignment_chain_skips_inferred_member_mismatch() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                local t = {}
                t.x = {}
                t.x.y = 1
                t.x.y = "x"
            "#
        ));
    }

    // #[test]
    // fn test_3() {
    //     let mut ws = VirtualWorkspace::new();
    //     assert!(ws.check_code_for_namespace(
    //         DiagnosticCode::AssignTypeMismatch,
    //         r#"
    //             ---@param s    string
    //             ---@param i?   integer
    //             ---@param j?   integer
    //             ---@param lax? boolean
    //             ---@return integer?
    //             ---@return integer? errpos
    //             ---@nodiscard
    //             local function get_len(s, i, j, lax) end

    //             local len = 0
    //             ---@diagnostic disable-next-line: need-check-nil
    //             len = len + get_len("", 1, 1, true)
    //         "#
    //     ));
    // }

    #[test]
    fn test_enum() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@enum SubscriberFlags
                local SubscriberFlags = {
                    None = 0,
                    Tracking = 1 << 0,
                    Recursed = 1 << 1,
                    ToCheckDirty = 1 << 3,
                    Dirty = 1 << 4,
                }
                ---@class Subscriber
                ---@field flags SubscriberFlags

                ---@type Subscriber
                local subscriber

                subscriber.flags = subscriber.flags & ~SubscriberFlags.Tracking -- 被推断为`integer`而不是实际整数值, 允许匹配
            "#
        ));

        // TODO: fix the inference of enum value operation results
        // No good way to handle this warning yet: in ts the result of an enum value operation is not the actual value, but ours is, which makes it hard to handle
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@enum SubscriberFlags
                local SubscriberFlags = {
                    None = 0,
                    Tracking = 1 << 0,
                    Recursed = 1 << 1,
                    ToCheckDirty = 1 << 3,
                    Dirty = 1 << 4,
                }
                ---@class Subscriber
                ---@field flags SubscriberFlags

                ---@type Subscriber
                local subscriber

                subscriber.flags = 9 -- 不允许匹配不上的实际值
            "#
        ));
    }

    #[test]
    fn test_intersection_assign_to_class() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            --- @class A
            --- @field x integer
            --- @field y integer

            --- @class B
            --- @field y string
            --- @field z integer

            local c --- @type A & B

            --- @class C
            --- @field x integer
            --- @field y integer
            --- @field z integer

            --- @type C
            _ = c -- missing y
            "#
        ));
    }

    #[test]
    fn test_intersection_assign_from_class() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            --- @class A
            --- @field x integer
            --- @field y integer

            --- @class B
            --- @field y string
            --- @field z integer

            --- @class C
            --- @field x integer
            --- @field y integer
            --- @field z integer

            local v --- @type C

            local c --- @type A & B
            c = v  -- no y in A & B
            "#
        ));
    }

    #[test]
    fn test_intersection_assign_from_class_inherited_members() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@class Base
            ---@field x integer

            ---@class C: Base
            ---@field y integer

            ---@class A
            ---@field x integer

            ---@class B
            ---@field y integer

            local v ---@type C

            local c ---@type A & B
            c = v
            "#
        ));
    }

    #[test]
    fn test_intersection_assign_tableconst_conflict() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@class A
            ---@field y integer

            ---@class B
            ---@field y string

            local c ---@type A & B
            c = { y = 1 } -- no y in A & B
            "#
        ));
    }

    #[test]
    fn test_intersection_assign_tableconst_requires_right_only_members() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@class A
            ---@field y integer

            ---@class B
            ---@field z integer

            local c ---@type A & B
            c = { y = 1 }
            "#
        ));
    }

    #[test]
    fn test_multi_return_local_after_unrelated_branch_assignment() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                local _, height = get_size()

                local x; if true then x = x end

                ---@type { h: number }
                local ghost = { h = height }

                ---@return string, number
                function get_size() return "", 1 end
            "#
        ));
    }

    #[test]
    fn test_three_return_local_after_unrelated_branch_assignment() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                local _, _, height = get_size()

                local x; if true then x = x end

                ---@type { h: number }
                local ghost = { h = height }

                ---@return string, boolean, number
                function get_size() return "", false, 1 end
            "#
        ));
    }

    #[test]
    fn test_issue_193() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                --- @return string?
                --- @return string?
                local function foo() end

                local a, b = foo()
            "#
        ));
    }

    #[test]
    fn test_issue_196() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@class A

                ---@return table
                function foo() end

                ---@type A
                local _ = foo()
            "#
        ));
    }

    #[test]
    fn test_issue_197() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                local a = setmetatable({}, {})
            "#
        ));
    }

    /// Tests that cannot be solved yet
    #[test]
    fn test_error() {
        // let mut ws = VirtualWorkspace::new();

        // Inference anomaly
        // assert!(ws.check_code_for_namespace(
        //     DiagnosticCode::AssignTypeMismatch,
        //     r#"
        // local n

        // if G then
        //     n = {}
        // else
        //     n = nil
        // end

        // local t = {
        //     x = n,
        // }
        //             "#
        // ));
    }

    #[test]
    fn test_valid_cases() {
        let mut ws = VirtualWorkspace::new();

        // Test cases that should pass (no type mismatch)
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
local m = {}
---@type integer[]
m.ints = {}
            "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@class A
---@field x A

---@type A
local t

t.x = {}
            "#
        ));

        // Test cases that should fail (type mismatch)
        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@class A
---@field x integer

---@type A
local t

t.x = true
            "#
        ));

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@class A
---@field x integer

---@type A
local t

---@type boolean
local y

t.x = y
            "#
        ));

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@class A
local m

m.x = 1

---@type A
local t

t.x = true
            "#
        ));

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@class A
local m

---@type integer
m.x = 1

m.x = true
            "#
        ));

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@class A
local mt

---@type integer
mt.x = 1

function mt:init()
    self.x = true
end
            "#
        ));

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@class A
---@field x integer

---@type A
local t = {
    x = true
}
            "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@type boolean[]
local t = {}

t[5] = nil
            "#
        ));
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@type table<string, true>
local t = {}

t['x'] = nil
            "#
        ));

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@type [boolean]
local t = { [1] = nil }

t = nil
            "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
local t = { true }

t[1] = nil
            "#
        ));

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@class A
local t = {
    x = 1
}

t.x = true
            "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@type number
local t

t = 1
            "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@type number
local t

---@type integer
local y

t = y
            "#
        ));

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@class A
local m

---@type number
m.x = 1

m.x = {}
            "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@type boolean[]
local t = {}

---@type boolean?
local x

t[#t+1] = x
            "#
        ));

        // Additional test cases
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@type number
local n
---@type integer
local i

i = n
            "#
        ));

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@type number|boolean
local nb

---@type number
local n

n = nb
            "#
        ));

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@type number
local x = 'aaa'
            "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@class X

---@class A
local mt = G

---@type X
mt._x = nil
            "#
        ));
        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@class A
local a = {}

---@class B
local b = a
            "#
        ));

        // With GMod enabled (default), assigning a supertype value to a subtype
        // variable is allowed to reduce false positives (Entity as generic stand-in).
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@class A
local a = {}
a.__index = a

---@class B: A
local b = setmetatable({}, a)
            "#
        ));

        // Continue with more test cases as needed
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@class A
---@field x number?
local a

---@class B
---@field x number
local b

b.x = a.x
            "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
local mt = {}
mt.x = 1
mt.x = nil
            "#
        ));

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@alias test boolean

---@type test
local test = 4
            "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@class MyClass
local MyClass = {}

function MyClass:new()
    ---@class MyClass
    local myObject = setmetatable({
        initialField = true
    }, self)

    print(myObject.initialField)
end
            "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@class T
local t = {
    x = nil
}

t.x = 1
            "#
        ));

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@type {[1]: string, [10]: number, xx: boolean}
local t = {
    true,
    [10] = 's',
    xx = 1,
}
            "#
        ));

        assert!(!ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
---@type boolean[]
local t = { 1, 2, 3 }
            "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
local t = {}
t.a = 1
t.a = 2
return t
            "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local function name()
                return 1, 2
            end
            local x, y
            x, y = name()
            "#
        ));
    }

    // May need handling
    #[test]
    fn test_pending() {
        let mut ws = VirtualWorkspace::new();
        let mut config = Emmyrc::default();
        config.strict.array_index = true;
        ws.analysis.update_config(config.into());

        // With GMod enabled (default), assigning a supertype value to a subtype
        // variable is allowed to reduce false positives (Entity as generic stand-in).
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@class A
            local a = {}

            ---@class B: A
            local b = a
                "#
        ));

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@class Option: string

            ---@param x Option
            local function f(x) end

            ---@type Option
            local x = 'aaa'

            f(x)
                        "#
        ));

        // Array types allow nullable matching, but initialization must not assign `nil` directly (really a shortcut: table_expr inference does not handle the edge case, and may allow it later)
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
        ---@type boolean[]
        local t = { true, false, nil }
        "#
        ));
    }

    #[test]
    fn test_issue_247() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
        local a --- @type boolean
        local b --- @type integer
        b = 1 + (a and 1 or 0)
        "#
        ));
    }

    #[test]
    fn test_issue_246() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
        --- @alias Type1 'add' | 'change' | 'delete'
        --- @alias Type2 'add' | 'change' | 'delete' | 'untracked'

        local ty1 --- @type Type1?

        --- @type Type2
        local _ = ty1 or 'untracked'
        "#
        ));
    }

    #[test]
    fn test_issue_295() {
        let mut ws = VirtualWorkspace::new();
        // TODO: fix the inference of enum value operation results
        // No good way to handle this warning yet: in ts the result of an enum value operation is not the actual value, but ours is, which makes it hard to handle
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"

            ---@enum SubscriberFlags
            local SubscriberFlags = {
                Tracking = 1 << 0,
            }
            ---@class Subscriber
            ---@field flags SubscriberFlags

            ---@type Subscriber
            local subscriber

            subscriber.flags = subscriber.flags & ~SubscriberFlags.Tracking

            subscriber.flags = 9
        "#
        ));
    }

    #[test]
    fn test_issue_285() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                --- @return string, integer
                local function foo() end

                local text, err
                text, err = foo()

                ---@type integer
                local b = err
        "#
        ));
    }

    #[test]
    fn test_issue_338() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local t ---@type 0|-1

            t = -1
        "#
        ));
    }

    #[test]
    fn test_return_self() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@class UI
            ---@overload fun(): self
            local M

            ---@type UI
            local a = M()
        "#
        ));
    }

    #[test]
    fn test_table_pack_in_function() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@param ... any
                local function build(...)
                    local t = table.pack(...)
                end
        "#
        ));
    }

    #[test]
    fn test_assign_field_with_flow() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@class M
                local M

                ---@type 'new' | 'inited' | 'started'
                M.state = 'new'

                function M:test()
                    if self.state ~= 'started' and self.state ~= 'inited' then
                        return
                    end
                    self.state = 'new'
                end
        "#
        ));
    }

    #[test]
    fn test_flow_1() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@class Unit

                ---@class Player

                ---@class CreateData
                ---@field owner? Unit|Player

                ---@param data CreateData
                local function send(data)
                    if not data.owner then
                        data.owner = ""
                    end
                end
        "#
        ));
    }

    #[test]
    fn test_flow_2() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@class Unit

                ---@class Player

                ---@class CreateData
                ---@field owner? Unit|Player

                ---@param data Unit|Player?
                local function send(data)
                    if not data then
                        data = ""
                    end
                end
        "#
        ));
    }

    #[test]
    fn test_table_array() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@type  { [1]: string, [integer]: any }
                local py_event

                ---@type any[]
                local py_args

                py_event = py_args
        "#
        ));
    }

    #[test]
    fn test_issue_330() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@enum MyEnum
            local MyEnum = { A = 1, B = 2 }

            local x --- @type MyEnum?

            ---@type MyEnum
            local a = x or MyEnum.A
        "#
        ));
    }

    #[test]
    fn test_issue_393() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@alias SortByScoreCallback fun(o: any): integer

                ---@param tbl any[]
                ---@param callbacks SortByScoreCallback | SortByScoreCallback[]
                function sortByScore(tbl, callbacks)
                    if type(callbacks) ~= 'table' then
                        callbacks = { callbacks }
                    end
                end
        "#
        ));
    }

    #[test]
    fn test_issue_374() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                --- @param x? integer
                --- @return integer?
                --- @overload fun(): integer
                function bar(x) end

                --- @type integer
                local _ = bar() -- - error cannot assign `integer?` to `integer`
        "#
        ));
    }

    #[test]
    fn test_nesting_table_field_1() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class T1
            ---@field x T2

            ---@class T2
            ---@field xx number
        "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@type T1
            local t = {
                x = {
                    xx = "",
                }
            }
        "#
        ));
    }

    #[test]
    fn test_nesting_table_field_2() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class T1
            ---@field x number
        "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@type T1
            local t = {
                x = {
                    xx = "",
                }
            }
        "#
        ));
    }

    #[test]
    fn test_issue_525() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@type table<integer,true|string>
                local lines
                for lnum = 1, #lines do
                    if lines[lnum] == true then
                        lines[lnum] = ''
                    end
                end
        "#
        ));
    }

    #[test]
    fn test_param_tbale() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@class ability
                ---@field t abilityType

                ---@enum (key) abilityType
                local abilityType = {
                    passive = 1,
                }

                ---@param a ability
                function test(a)

                end

                test({
                    t = ""
                })
        "#
        ));
    }

    #[test]
    fn test_table_field_type_mismatch() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local export = {
                ---@type number?
                vvv = "a"
            }
        "#
        ));
    }

    #[test]
    fn test_object_table() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
        ---@alias A {[string]: string}

        ---@param matchers A
        function name(matchers)
        end
        "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            name({
                toBe = 1,
            })
        "#
        ));
    }

    #[test]
    fn test_generic_array_alias_tuple() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@alias array<T> T[]
        "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@type array<number>
            local list = {
                "2",
            }
        "#
        ));
    }

    #[test]
    fn test_ref_index_key_match_tuple() {
        let mut ws = crate::VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@class Item
                ---@field id int

                ---@class TbItem
                ---@field [int] Item

                ---@type TbItem
                local items = {
                    { id = 1 },
                    { id = 2 },
                    { id = 2 },
                }
            "#,
        ));
    }

    #[test]
    fn test_realm_split_dynamic_command_table_keeps_realm_local_callback_signature() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::AssignTypeMismatch);

        ws.def_file(
            "lua/glide/server/network.lua",
            r#"
            Glide.NetCommands = Glide.NetCommands or {}
            local commands = Glide.NetCommands

            commands[1] = function(ply) end

            ---@param commandId number
            ---@param handler fun(ply: Player)
            function Glide.AddCommandHandler(commandId, handler)
                commands[commandId] = handler
            end
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/glide/client/network.lua",
            r#"
            Glide.NetCommands = Glide.NetCommands or {}
            local commands = Glide.NetCommands

            ---@param commandId number
            ---@param handler fun(len: number)
            function Glide.AddCommandHandler(commandId, handler)
                commands[commandId] = handler
            end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(client_file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::AssignTypeMismatch.get_name().to_string(),
        ));
        assert!(
            !diagnostics.iter().any(|diagnostic| diagnostic.code == code),
            "unexpected assign-type-mismatch diagnostics: {diagnostics:?}"
        );
    }

    #[test]
    fn test_ref_index_access_assign_class_to_object_mismatch() {
        let mut ws = crate::VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@class A
                ---@field [integer] string

                local t ---@type { [integer]: number }
                local a ---@type A

                t = a
            "#,
        ));
    }

    #[test]
    fn test_ref_index_access_assign_object_to_class_mismatch() {
        let mut ws = crate::VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@class A
                ---@field [integer] string

                local t ---@type { [integer]: number }
                local a ---@type A

                a = t
            "#,
        ));
    }

    #[test]
    fn test_no_false_positive_dynamic_table_field_write() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                local params = {}
                params.buoyancy = 6
            "#,
        ));
    }

    #[test]
    fn test_no_false_positive_tuple_literal_for_table_field_param() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@class SoundData
                ---@field pitch (number|table<number>)?

                ---@param data SoundData
                local function add_sound(data)
                end

                add_sound({
                    pitch = {95, 105}
                })
            "#,
        ));
    }

    // Entity:GetTable() should return `table`, and assigning to fields of the returned
    // table should not produce assign-type-mismatch errors (no `never` type).
    #[test]
    fn test_entity_get_table_no_never() {
        let mut ws = VirtualWorkspace::new();

        // Case 1: direct local assignment from GetTable()
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@class Entity
            ---@return table
            function Entity:GetTable() end

            ---@class ENT : Entity
            local ENT = {}

            function ENT:Initialize()
                local selfTbl = self:GetTable()
                selfTbl.spin = 0
                selfTbl.tilt = 0
                selfTbl.isRunning = true
            end
            "#
        ));

        // Case 2: selfTbl = selfTbl or self:GetTable() pattern
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@class Entity
            ---@return table
            function Entity:GetTable() end

            ---@class ENT : Entity
            local ENT = {}

            function ENT:Think(selfTbl)
                selfTbl = selfTbl or self:GetTable()
                selfTbl.spin = 0
                selfTbl.tilt = 0
            end
            "#
        ));
        // Case 3: GetTable NOT defined on Entity - should still not produce false positives
        let mut ws3 = VirtualWorkspace::new();
        assert!(ws3.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@class Entity
            local Entity = {}

            ---@class ENT : Entity
            local ENT = {}

            function ENT:Initialize()
                local selfTbl = self:GetTable()
                selfTbl.spin = 0
                selfTbl.tilt = 0
                selfTbl.isRunning = true
            end
            "#
        ));

        // Case 4: self typed as `any` - calling any method should not produce false positives
        let mut ws4 = VirtualWorkspace::new();
        assert!(ws4.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@type any
            local self = nil

            local selfTbl = self:GetTable()
            selfTbl.spin = 0
            selfTbl.tilt = 0
            "#
        ));
    }

    // When a colon-defined method annotation is assigned via dot syntax, the user's
    // closure with an explicit `self` as first parameter should not produce an
    // assign-type-mismatch error.
    #[test]
    fn test_colon_method_dot_assign_with_explicit_self() {
        let mut ws = VirtualWorkspace::new();

        // panel.Paint = function(self, w, h) should NOT trigger assign-type-mismatch
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@class Panel
            local Panel = {}

            ---@param width number
            ---@param height number
            function Panel:Paint(width, height) end

            ---@type Panel
            local panel = {}

            panel.Paint = function(self, w, h)
            end
            "#
        ));
    }

    // Assigning to a field of a nil-typed variable used to produce an AssignTypeMismatch for
    // `never` (the result of indexing nil). Now that `never` suppresses the diagnostic, no
    // assign-type-mismatch is reported — the inject-field checker handles the nil case instead.
    #[test]
    fn test_nil_field_assign_produces_never_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        // `never` as the target type should suppress assign-type-mismatch (type inference limitation).
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@type nil
            local x
            x.spin = 0
            "#
        ));
    }

    // Reproduce: FindMetaTable("Entity").GetTable pattern - selfTbl should not be `never`
    #[test]
    fn test_rc2_find_meta_table_get_table() {
        let mut ws = VirtualWorkspace::new();

        // Pattern: local getTable = FindMetaTable("Entity").GetTable; local selfTbl = getTable(self)
        ws.def(
            r#"
            ---@class RC2Entity
            ---@return table
            function RC2Entity:GetTable() end

            ---@generic T : table
            ---@param metaName `T`
            ---@return T|nil
            function FindMetaTable2(metaName) end
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@param self RC2Entity
            local function test(self)
                local getTable = FindMetaTable2("RC2Entity").GetTable
                local selfTbl = getTable(self)
                selfTbl.spin = 0
                selfTbl.tilt = 0
            end
            "#
        ));
    }

    #[test]
    fn test_find_meta_table_member_alias_does_not_report_assign_type_mismatch() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Entity
            local Entity = {}

            ---@return string
            function Entity:GetClass()
            end

            ---@generic T : table
            ---@param metaName `T`
            ---@return T
            function FindMetaTable(metaName)
            end
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local ENTITY = FindMetaTable("Entity")
            ENTITY.Test1Alias = ENTITY.GetClass
            "#
        ));
    }

    // Pattern 2: local var without initializer - assigning then accessing fields should not error
    #[test]
    fn test_rc2_local_no_init_then_assign_table() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local active
            active = {}
            active.running = true
            active.spin = 0
            "#
        ));
    }

    // Pattern 2b: local var without initializer used inside function, assigned to {}
    #[test]
    fn test_rc2_local_no_init_inside_func() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local function test()
                local dt
                dt = {}
                dt.x = 1
                dt.y = 2
            end
            "#
        ));
    }

    // Pattern 3: self.field = nil then self.field = {...} across functions
    #[test]
    fn test_rc2_self_field_nil_then_table() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@class Ent3
            local Ent3 = {}

            function Ent3:Initialize()
                self.data = nil
            end

            function Ent3:Think()
                self.data = { running = true }
                self.data.running = false
            end
            "#
        ));
    }

    // Pattern 1b: module-level getTable variable then used inside function
    #[test]
    fn test_rc2_module_level_get_table() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class ModEnt
            ---@return table
            function ModEnt:GetTable() end

            ---@generic T : table
            ---@param metaName `T`
            ---@return T|nil
            function FindMetaTable3(metaName) end
            "#,
        );

        // getTable declared at module level, used inside function
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local getTable = FindMetaTable3("ModEnt").GetTable

            ---@param self ModEnt
            local function test(self)
                local selfTbl = getTable(self)
                selfTbl.spin = 0
                selfTbl.tilt = 0
            end
            "#
        ));
    }

    // Pattern 1c: getTable in one def file, usage in check_code_for file (cross-file)
    #[test]
    fn test_rc2_cross_file_get_table() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class XEnt
            ---@return table
            function XEnt:GetTable() end

            ---@generic T : table
            ---@param metaName `T`
            ---@return T|nil
            function FindMetaTable4(metaName) end

            local getTable = FindMetaTable4("XEnt").GetTable

            ---@param self XEnt
            local function doStuff(self)
                local selfTbl = getTable(self)
                selfTbl.spin = 0
            end
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local getTable2 = FindMetaTable4("XEnt").GetTable

            ---@param self XEnt
            local function test2(self)
                local selfTbl = getTable2(self)
                selfTbl.tilt = 0
            end
            "#
        ));
    }

    // Debug test: verify what type `result` gets when fn() is called with fn: any
    // (This is a documentation test - confirms any() → Unknown, not Nil or Never)
    #[test]
    fn test_any_call_expr_type_is_unknown() {
        // Calling a value typed as `any` returns Unknown (not Nil/Never).
        // The top-level infer_expr converts Err(None) → Ok(Unknown) for call exprs.
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@param fn any
            local function test(fn)
                local result = fn()
                result.field = 0
            end
            "#
        ));
    }

    #[test]
    fn test_unknown_union_member_does_not_poison_assignable_known_members() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "test.lua",
            r#"
            local target = false

            ---@type false|unknown
            local value

            target = value
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .expect("diagnostics should be available");
        let code_string = Some(NumberOrString::String(
            DiagnosticCode::AssignTypeMismatch.get_name().to_string(),
        ));
        let assign_diags = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == code_string)
            .map(|diagnostic| diagnostic.message.clone())
            .collect::<Vec<_>>();
        assert!(
            assign_diags.is_empty(),
            "unexpected assign-type-mismatch diagnostics: {:?}",
            assign_diags
        );
    }

    #[test]
    fn test_unknown_union_member_does_not_hide_incompatible_known_members() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local target = false

            ---@type string|unknown
            local value

            target = value
            "#
        ));
    }

    // RC-2 fix: local var (no init) used as upvalue in closure, assigned in different function.
    // Before fix: active bound to Nil → active.fade = Never → "Cannot assign integer to never"
    // After fix:  active (mutable) not bound to Nil → infer returns Err → no diagnostic
    #[test]
    fn test_rc2_upvalue_assigned_after_closure_decl() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local active
            local function setup()
                active.fade = 0
                active.flash = 1
            end
            active = {}
            setup()
            "#
        ));
    }

    // RC-2 fix: mutable local with nil used in closure hook pattern (mirrors notify.lua)
    #[test]
    fn test_rc2_mutable_upvalue_hook_pattern() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local active
            local function SetupNotification()
                active.fade = 0
                active.flash = 1
                active.x = 0
                active.y = 0
            end
            local function OnAdd(data)
                active = data
                SetupNotification()
            end
            "#
        ));
    }

    #[test]
    fn test_never_value_no_assign_mismatch() {
        let mut ws = VirtualWorkspace::new();
        // A value of type `never` never returns, so it is assignable to every target type.
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@return never
                local function stop() end

                ---@type number
                local value
                value = stop()
            "#
        ));
    }

    #[test]
    fn issue_40_explicit_nil_local_rejects_non_nil_initializer() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@type nil
                local value = 1
            "#
        ));
    }

    #[test]
    fn issue_40_explicit_nil_local_rejects_later_non_nil_assignment() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@type nil
                local value
                value = 1
            "#
        ));
    }

    #[test]
    fn issue_40_explicit_void_local_rejects_value_initializer() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@type void
                local value = 1
            "#
        ));
    }

    #[test]
    fn issue_40_explicit_void_local_rejects_later_value_assignment() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@type void
                local value
                value = 1
            "#
        ));
    }

    #[test]
    fn issue_40_explicit_never_local_rejects_initializer() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@type never
                local value = 1
            "#
        ));
    }

    #[test]
    fn issue_40_explicit_never_local_rejects_later_assignment() {
        let mut ws = VirtualWorkspace::new();

        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@type never
                local value
                value = 1
            "#
        ));
    }

    #[test]
    fn test_inferred_local_reassign_different_type() {
        let mut ws = VirtualWorkspace::new();
        // Inferred type from first assignment should not constrain reassignment
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                local matchesFilter = true
                matchesFilter = 42
            "#
        ));
    }

    #[test]
    fn test_inferred_local_reassign_different_type_strict_flag_restores_warning() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.strict.inferred_type_mismatch = true;
        ws.update_emmyrc(emmyrc);

        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                local matchesFilter = true
                matchesFilter = 42
            "#
        ));
    }

    #[test]
    fn test_annotated_local_reassign_still_errors() {
        let mut ws = VirtualWorkspace::new();
        // Explicitly annotated type SHOULD constrain reassignment
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@type boolean
                local matchesFilter = true
                matchesFilter = 42
            "#
        ));
    }

    // Global variable with an INFERRED type (no ---@type annotation) should NOT produce
    // assign-type-mismatch when reassigned to a different type (e.g. tonumber()).
    // Regression test for the `_` arm in check_name_expr missing `is_infer()` guard.
    #[test]
    fn test_global_inferred_type_tonumber_no_false_positive() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        // value is a global inferred as `string`; reassigning via tonumber() returns `number?`.
        // This should NOT produce a diagnostic — the type was inferred, not explicitly annotated.
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                value = "hello"
                value = tonumber(value)
            "#
        ));
    }

    // Annotated global (---@type string) SHOULD still produce a diagnostic on type mismatch.
    // This confirms the fix does not regress the annotated-global case.
    #[test]
    fn test_global_annotated_type_tonumber_still_errors() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        // value is explicitly annotated as `string`; reassigning to `number?` SHOULD error.
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@type string
                value = "hello"
                value = tonumber(value)
            "#
        ));
    }

    #[test]
    fn test_inferred_member_collection_can_be_reset_and_appended() {
        let mut ws = VirtualWorkspace::new();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::AssignTypeMismatch);
        let file_id = ws.def(
            r#"
                ---@class Seat
                local Seat = {}

                ---@class Vehicle
                local Vehicle = {}

                ---@param seat Seat
                function Vehicle:test(seat)
                    local selfTbl = self

                    self.wheelTraceFilter = { self, "player", "npc_*" }
                    selfTbl.wheelTraceFilter = { self, "player" }
                    selfTbl.wheelTraceFilter[#selfTbl.wheelTraceFilter + 1] = seat
                end
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .expect("diagnostics should be available");
        let code_string = Some(NumberOrString::String(
            DiagnosticCode::AssignTypeMismatch.get_name().to_string(),
        ));
        let assign_diags = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == code_string)
            .map(|diagnostic| diagnostic.message.clone())
            .collect::<Vec<_>>();
        assert!(
            assign_diags.is_empty(),
            "unexpected assign-type-mismatch diagnostics: {:?}",
            assign_diags
        );
    }

    #[test]
    fn test_inferred_member_collection_non_append_write_still_errors() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@class Seat
                local Seat = {}

                ---@class Vehicle
                local Vehicle = {}

                ---@param seat Seat
                function Vehicle:test(seat)
                    self.wheelTraceFilter = { self, "player" }
                    self.wheelTraceFilter[2] = seat
                end
            "#
        ));
    }

    #[test]
    fn test_cross_file_mapstyle_inferred_tuple_collections_do_not_conflict() {
        let mut ws = VirtualWorkspace::new();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::AssignTypeMismatch);

        let file_a = ws.def(
            r#"
                property = property or {}
                property.doors = property.doors or {}

                property.doors[1] = { 1325, 1326, 1330, 1329, 1327, 1328, 1337, 1338 }
                property.doors[2] = { 1825, 1826, 2360, 2359, 1258, 1259, 1827, 1479 }
                property.doors[6] = { 1894 }
            "#,
        );

        let file_b = ws.def(
            r#"
                property = property or {}
                property.doors = property.doors or {}

                property.doors[1] = { 3268, 3267, 3266, 2837, 3542, 3545, 3548, 3549, 3550 }
                property.doors[2] = { 4301, 4302, 4304, 4303, 4305, 4306, 4310, 4309, 4308 }
                property.doors[6] = { 3204, 3203, 3202, 3246 }
            "#,
        );

        for file_id in [file_a, file_b] {
            let diagnostics = ws
                .analysis
                .diagnose_file(file_id, CancellationToken::new())
                .expect("diagnostics should be available");
            let code_string = Some(NumberOrString::String(
                DiagnosticCode::AssignTypeMismatch.get_name().to_string(),
            ));
            let assign_diags = diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.code == code_string)
                .map(|diagnostic| diagnostic.message.clone())
                .collect::<Vec<_>>();
            assert!(
                assign_diags.is_empty(),
                "unexpected assign-type-mismatch diagnostics: {:?}",
                assign_diags
            );
        }
    }

    #[test]
    fn test_inferred_member_collection_keeps_tuple_slot_precision() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@class Vehicle
                local Vehicle = {}

                function Vehicle:test()
                    self.wheelTraceFilter = { self, "player" }

                    ---@type string
                    local tag = self.wheelTraceFilter[2]
                end
            "#
        ));
    }

    #[test]
    fn a_row_written_once_through_a_dynamic_key_keeps_its_positions() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                local quads = {}

                local function add(i)
                    quads[i] = { "a", 55 }
                end

                local function draw(i)
                    local q = quads[i]
                    ---@type string
                    local tag = q[1]
                end
            "#
        ));
    }

    #[test]
    fn rows_of_two_shapes_written_through_a_dynamic_key_keep_their_positions() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                local reports = {}

                local function open(id)
                    reports[id] = { "reporter", 1 }
                end

                local function claim(id)
                    reports[id] = { "reporter", 1, true }
                end

                local function read(id)
                    local report = reports[id]
                    ---@type string
                    local reporter = report[1]
                end
            "#
        ));
    }

    #[test]
    fn a_table_of_any_is_assignable_to_a_keyed_table_shape() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@param data table<any, any>
            local function sync(data)
                ---@type { [string]: { value: any, type: integer } }
                local stored = {}
                stored = data
            end
            "#
        ));
    }

    #[test]
    fn a_table_whose_values_do_not_fit_a_field_is_not_assignable_to_the_shape() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@param data table<string, number>
            local function sync(data)
                ---@type { name: string }
                local stored = { name = "x" }
                stored = data
            end
            "#
        ));
    }

    #[test]
    fn a_write_widens_an_inferred_member_read_through_a_nullable_class() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@class SnipInv
            local META = {}
            META.data = META.data or {}

            ---@param key string
            ---@param dataType? integer
            function META:SetData(key, value, dataType)
                dataType = dataType or 1
                self.data = self.data or {}
                self.data[key] = { value = value, type = dataType }
            end

            ---@return SnipInv?
            local function GetInv(id) return nil end
            ---@return SnipInv
            local function CreateInv(id) return META end

            ---@param data table<any, any>
            local function sync(data)
                local inv = GetInv(1)
                if not inv then
                    inv = CreateInv(1)
                else
                    inv.items = {}
                end
                if data then
                    inv.data = data
                end
            end
            "#
        ));
    }

    #[test]
    fn test_annotated_tuple_member_collection_remains_strict() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@class Seat
                local Seat = {}

                ---@class Vehicle
                ---@field wheelTraceFilter [Vehicle, "player", "npc_*"]
                local Vehicle = {}

                ---@param seat Seat
                function Vehicle:test(seat)
                    self.wheelTraceFilter = { self, "player" }
                    self.wheelTraceFilter[#self.wheelTraceFilter + 1] = seat
                end
            "#
        ));
    }

    #[test]
    fn test_annotated_array_member_collection_remains_strict() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@class Seat
                local Seat = {}

                ---@class Vehicle
                ---@field wheelTraceFilter Vehicle[]
                local Vehicle = {}

                ---@param seat Seat
                function Vehicle:test(seat)
                    self.wheelTraceFilter = { self, self }
                    self.wheelTraceFilter[#self.wheelTraceFilter + 1] = seat
                end
            "#
        ));
    }

    #[test]
    fn test_startup_stale_index_refresh_no_assign_mismatch_before_edit() {
        let mut ws = VirtualWorkspace::new();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::AssignTypeMismatch);

        let uri = ws
            .virtual_url_generator
            .new_uri("startup_stale_index_refresh.lua");
        let content = r#"
            ---@class Foo
            ---@field value integer

            ---@type Foo
            local foo = { value = 1 }
            foo.value = 2
        "#;

        let file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(content.to_string()))
            .map(|(id, _)| id)
            .expect("file id should exist");

        ws.analysis.compilation.clear_index();
        let updated_file_ids = ws
            .analysis
            .update_files_by_uri(vec![(uri, Some(content.to_string()))]);
        assert_eq!(updated_file_ids, vec![file_id]);
        assert!(
            ws.analysis
                .compilation
                .get_db()
                .get_module_index()
                .get_module(file_id)
                .is_some()
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .expect("diagnostics should be available");
        let code_string = Some(NumberOrString::String(
            DiagnosticCode::AssignTypeMismatch.get_name().to_string(),
        ));
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.code != code_string)
        );
    }

    /// When an unannotated main-workspace override shadows an annotated library
    /// function, the annotated return type should still take priority and the
    /// override's parameter overloads should be respected.
    #[test]
    fn test_annotated_library_override_suppresses_assign_mismatch() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        // Library workspace with annotated ents.Create
        let library_root = ws
            .virtual_url_generator
            .new_path("__test_ents_create_library");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("ents.lua")).unwrap();
        ws.analysis
            .update_file_by_uri(
                &library_uri,
                Some(
                    r#"
                ---@class Entity
                ---@param class string
                ---@return Entity
                function ents.Create(class) end
                "#
                    .to_string(),
                ),
            )
            .map(|(id, _)| id);

        // Main workspace: unannotated override + test call
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ents = ents or {}

            function ents.Create(name, ...)
                -- unannotated override, analyzer infers nil return
                return nil
            end

            ---@type Entity
            local ent = ents.Create("letter")
            -- ent should resolve to Entity (from annotation), not nil
            "#
        ));
    }

    /// The main-workspace override of an annotated library function should not
    /// trigger "expected N parameters but found M" when the override accepts
    /// more parameters.
    #[test]
    fn test_annotated_library_override_suppresses_redundant_parameter() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        // Library workspace with annotated ents.Create
        let library_root = ws
            .virtual_url_generator
            .new_path("__test_ents_create_library2");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("ents.lua")).unwrap();
        ws.analysis
            .update_file_by_uri(
                &library_uri,
                Some(
                    r#"
                ---@class Entity
                ---@param class string
                ---@return Entity
                function ents.Create(class) end
                "#
                    .to_string(),
                ),
            )
            .map(|(id, _)| id);

        // Main workspace: override with extra parameters + test call
        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ents = ents or {}

            function ents.Create(name, ...)
                return nil
            end

            local x = ents.Create("foo", "extra_arg")
            -- Should NOT produce "expected 1 parameters but found 2"
            "#
        ));
    }

    /// Repro for non-variadic declaration-style override: extra parameters
    /// accepted by the override must suppress redundant-parameter diagnostics.
    #[test]
    fn test_annotated_library_non_variadic_override_suppresses_redundant_parameter() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        // Library workspace with annotated ents.Create
        let library_root = ws
            .virtual_url_generator
            .new_path("__test_ents_create_library_non_variadic");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("ents.lua")).unwrap();
        ws.analysis
            .update_file_by_uri(
                &library_uri,
                Some(
                    r#"
                ---@class Entity
                ---@param class string
                ---@return Entity
                function ents.Create(class) end
                "#
                    .to_string(),
                ),
            )
            .map(|(id, _)| id);

        // Main workspace: declaration-style non-variadic override with extra parameter.
        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ents = ents or {}

            function ents.Create(name, safety)
                return nil
            end

            local x = ents.Create("foo", "extra_arg")
            -- Should NOT produce "expected 1 parameters but found 2"
            "#
        ));
    }

    /// Assignment-style override should still inherit annotated return type when
    /// called with extra parameters accepted by the override.
    #[test]
    fn test_annotated_library_file_define_override_return_type_with_extra_params() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        // Library workspace with annotated ents.Create
        let library_root = ws
            .virtual_url_generator
            .new_path("__test_ents_create_library_file_define_ret");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("ents.lua")).unwrap();
        ws.analysis
            .update_file_by_uri(
                &library_uri,
                Some(
                    r#"
                ---@class Entity
                ---@param class string
                ---@return Entity
                function ents.Create(class) end
                "#
                    .to_string(),
                ),
            )
            .map(|(id, _)| id);

        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ents = ents or {}
            local originalCreate = ents.Create

            ents.Create = function(name, safety)
                if originalCreate then
                    return originalCreate(name)
                end
                return nil
            end

            ---@type Entity
            local ent = ents.Create("foo", "extra")
            -- Should use annotated return type (Entity), not override's nil inference
            "#
        ));
    }

    /// Isolates call-site checking by assigning a predeclared function into the
    /// override slot (instead of inline closure assignment typing).
    #[test]
    fn test_annotated_library_file_define_named_override_suppresses_redundant_parameter() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let library_root = ws
            .virtual_url_generator
            .new_path("__test_ents_create_library_file_define_named");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("ents.lua")).unwrap();
        ws.analysis
            .update_file_by_uri(
                &library_uri,
                Some(
                    r#"
                ---@class Entity
                ---@param class string
                ---@return Entity
                function ents.Create(class) end
                "#
                    .to_string(),
                ),
            )
            .map(|(id, _)| id);

        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ents = ents or {}
            local originalCreate = ents.Create

            local function create_override(name, safety)
                if originalCreate then
                    return originalCreate(name)
                end
                return nil
            end

            ents.Create = create_override
            local x = ents.Create("foo", "extra_arg")
            "#
        ));
    }

    /// The annotated return type should take priority even when the override's
    /// extra parameters are used (2-param call still returns Entity, not nil).
    #[test]
    fn test_annotated_library_override_return_type_with_extra_params() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        // Library workspace with annotated ents.Create
        let library_root = ws
            .virtual_url_generator
            .new_path("__test_ents_create_library3");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("ents.lua")).unwrap();
        ws.analysis
            .update_file_by_uri(
                &library_uri,
                Some(
                    r#"
                ---@class Entity
                ---@param class string
                ---@return Entity
                function ents.Create(class) end
                "#
                    .to_string(),
                ),
            )
            .map(|(id, _)| id);

        // Main workspace: 2-param call with typed local should NOT produce
        // assign-type-mismatch because the meta return type (Entity) wins.
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ents = ents or {}

            function ents.Create(name, ...)
                return nil
            end

            ---@type Entity
            local ent = ents.Create("foo", "extra")
            -- Should NOT be nil — meta return type (Entity) takes priority
            "#
        ));
    }

    /// Guard against hiding the annotated return entirely (e.g. degrading to
    /// unknown): with library annotation present, assigning override call result
    /// to an incompatible concrete target type should still report mismatch.
    #[test]
    fn test_annotated_library_override_extra_params_mismatch_incompatible_target() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let library_root = ws
            .virtual_url_generator
            .new_path("__test_ents_create_library_incompatible_target");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("ents.lua")).unwrap();
        ws.analysis
            .update_file_by_uri(
                &library_uri,
                Some(
                    r#"
                ---@class Entity
                ---@param class string
                ---@return Entity
                function ents.Create(class) end
                "#
                    .to_string(),
                ),
            )
            .map(|(id, _)| id);

        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ents = ents or {}

            function ents.Create(name, safety)
                return nil
            end

            ---@type string
            local ent = ents.Create("foo", "extra")
            "#
        ));
    }

    /// Without the library annotation, the override's nil return SHOULD trigger
    /// assign-type-mismatch. This proves the library annotation is the fix.
    #[test]
    fn test_unannotated_override_alone_does_produce_assign_mismatch() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        // No library workspace — only the unannotated override exists.
        // The override returns number, so assigning to a string-typed local
        // SHOULD produce assign-type-mismatch.
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ents = ents or {}

            function ents.Create(name, ...)
                return 123
            end

            ---@type string
            local ent = ents.Create("foo")
            "#
        ));
    }

    #[test]
    fn test_server_only_override_does_not_suppress_client_redundant_parameter() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let library_root = ws
            .virtual_url_generator
            .new_path("__test_ents_create_library_realm_split");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("ents.lua")).unwrap();
        ws.analysis
            .update_file_by_uri(
                &library_uri,
                Some(
                    r#"
                ---@class Entity
                ---@realm shared
                ---@param class string
                ---@return Entity
                function ents.Create(class) end
                "#
                    .to_string(),
                ),
            )
            .map(|(id, _)| id);

        ws.def_file(
            "gamemode/modules/workarounds/sv_workarounds.lua",
            r#"
            ents = ents or {}

            function ents.Create(name, safety)
                return nil
            end
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ents = ents or {}
            if SERVER then
                local ok = ents.Create("foo", "extra")
            end
            "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ents = ents or {}
            if CLIENT then
                local ok = ents.Create("foo")
            end
            "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::RedundantParameter,
            r#"
            ents = ents or {}
            if CLIENT then
                local should_error = ents.Create("foo", "extra")
            end
            "#
        ));
    }

    /// Named fields on a shaped literal must stay strict even when the literal
    /// also has integer members. Assigning a wrong type to `.kind` should error.
    #[test]
    fn test_mixed_literal_named_field_stay_strict_with_integer_members() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                local mixed = { { id = 1 }, kind = "metadata" }
                mixed.kind = 42
            "#
        ));
    }

    /// A numeric-key object with named fields must keep named fields strict.
    /// Assigning a wrong type to `.name` should error.
    #[test]
    fn test_numeric_key_object_named_field_stays_strict() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                local obj = { [100] = { id = 1 }, name = "x" }
                obj.name = 42
            "#
        ));
    }

    /// A simple inferred named-field literal should remain lenient even when
    /// later code writes a numeric member. Widening an inferred table to accept
    /// both named fields and numeric-indexed entries must not produce
    /// AssignTypeMismatch for the existing named field.
    #[test]
    fn test_inferred_named_field_literal_lenient_with_later_numeric_member() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@class Vector
                ---@return Vector
                local function Vector() end

                local cfg = {
                    color = "white",
                }

                cfg[1] = { id = 1 }
                cfg.color = Vector()
            "#
        ));
    }
}

#[test]
fn test_strict_inferred_table_assignment() {
    let mut ws = crate::test_lib::VirtualWorkspace::new();
    let mut emmyrc = ws.get_emmyrc();
    emmyrc.strict.inferred_type_mismatch = true;
    ws.update_emmyrc(emmyrc);

    // check_code_for returns FALSE if it FINDS the diagnostic.
    assert!(!ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        local strict = { x = 1 }
        -- This should error because strict mode enforces mismatched types on inferred variables
        strict = { x = "hello" }
        "#
    ));
}

#[test]
fn test_reassigned_inferred_member_does_not_lock_to_last_literal() {
    let mut ws = crate::test_lib::VirtualWorkspace::new();

    assert!(ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class TestClass
        local TestClass = {}

        ---@type TestClass
        local obj

        function TestClass:SetFalse()
            self._testVar = false
        end

        function TestClass:Reset()
            self._testVar = nil
        end

        obj._testVar = true
        "#
    ));
}

#[test]
fn test_doc_string_union_with_legacy_wrapped_literals_accepts_matching_string() {
    let mut ws = crate::test_lib::VirtualWorkspace::new();
    assert!(ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class CAMI_PRIVILEGE
        ---@field MinAccess "'user'" | "'admin'" | "'superadmin'"

        ---@param privilege CAMI_PRIVILEGE
        local function register_privilege(privilege)
        end

        register_privilege({
            MinAccess = "superadmin"
        })
        "#
    ));
}

#[test]
fn test_doc_string_union_with_legacy_wrapped_literals_stays_strict() {
    let mut ws = crate::test_lib::VirtualWorkspace::new();
    assert!(!ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class CAMI_PRIVILEGE
        ---@field MinAccess "'user'" | "'admin'" | "'superadmin'"

        ---@param privilege CAMI_PRIVILEGE
        local function register_privilege(privilege)
        end

        register_privilege({
            MinAccess = "root"
        })
        "#
    ));
}

// In GMod, NULL is the "zero value" of Entity. Assigning an Entity subtype
// to a NULL-typed slot is valid — the pattern `filter = { NULL, NULL };
// filter[1] = self` is common and correct.
#[test]
fn test_entity_subtype_assignable_to_null_typed_slot() {
    let mut ws = crate::test_lib::VirtualWorkspace::new();
    assert!(ws.check_code_for_namespace(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class Entity
        local Entity = {}

        ---@class NULL : Entity
        local NULL = nil

        ---@class glide_missile : Entity
        local glide_missile = {}

        ---@type glide_missile
        local self

        local traceData = {
            filter = { NULL, NULL },
        }

        traceData.filter[1] = self
        traceData.filter[2] = self
        "#
    ));
}

// NULL should be assignable to Entity-typed slots (NULL is a valid Entity value).
#[test]
fn test_null_assignable_to_entity_typed_slot() {
    let mut ws = crate::test_lib::VirtualWorkspace::new();
    assert!(ws.check_code_for_namespace(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class Entity
        local Entity = {}

        ---@class NULL : Entity
        local NULL = nil

        ---@type Entity
        local ent

        ent = NULL
        "#
    ));
}

// NULL should NOT be assignable to unrelated types (e.g., string).
#[test]
fn test_null_not_assignable_to_unrelated_type() {
    let mut ws = crate::test_lib::VirtualWorkspace::new();
    assert!(!ws.check_code_for_namespace(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class Entity
        local Entity = {}

        ---@class NULL : Entity
        local NULL = nil

        ---@type string
        local s

        s = NULL
        "#
    ));
}

// Non-Entity types should NOT be assignable to NULL-typed slots.
#[test]
fn test_non_entity_not_assignable_to_null_slot() {
    let mut ws = crate::test_lib::VirtualWorkspace::new();
    assert!(!ws.check_code_for_namespace(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class Entity
        local Entity = {}

        ---@class NULL : Entity
        local NULL = nil

        ---@type NULL
        local slot

        ---@type string
        local s

        slot = s
        "#
    ));
}

// Direct Entity subtype → NULL local variable assignment should be valid.
#[test]
fn test_entity_subtype_assignable_to_null_local() {
    let mut ws = crate::test_lib::VirtualWorkspace::new();
    assert!(ws.check_code_for_namespace(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class Entity
        local Entity = {}

        ---@class NULL : Entity
        local NULL = nil

        ---@class glide_missile : Entity
        local glide_missile = {}

        ---@type NULL
        local slot

        ---@type glide_missile
        local self

        slot = self
        "#
    ));
}

#[test]
fn test_initialized_dot_placeholder_replaced_by_function_still_reports() {
    let mut ws = crate::test_lib::VirtualWorkspace::new();

    assert!(!ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        cityrp = {}
        cityrp.eventManager = {}
        cityrp.eventManager.comDo = {}

        function cityrp.eventManager.comDo(admin, text, icon, broadcast, hidden)
        end
        "#
    ));
}

#[test]
fn repeated_initialized_index_member_assignments_diagnose_quick_smoke() {
    let mut ws = crate::VirtualWorkspace::new();
    let mut body = String::from("WepHolster = {}\nWepHolster.defData = {}\n");
    for i in 0..250 {
        body.push_str(&format!(
            r#"
                WepHolster.defData["weapon_{i}"] = {{}}
                WepHolster.defData["weapon_{i}"].Model = "models/weapons/w_{i}.mdl"
                WepHolster.defData["weapon_{i}"].Bone = "ValveBiped.Bip01_Spine"
                WepHolster.defData["weapon_{i}"].BoneOffset = {{Vector(0, 0, 0), Angle(0, 0, 0)}}
"#
        ));
    }

    ws.analysis
        .diagnostic
        .enable_only(crate::DiagnosticCode::AssignTypeMismatch);
    let file_id = ws.def(&body);
    let diagnostics = ws
        .analysis
        .diagnose_file(file_id, tokio_util::sync::CancellationToken::new())
        .unwrap_or_default();

    assert!(
        diagnostics.is_empty(),
        "unexpected assign-type diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn declared_array_empty_initializer_keeps_declared_owner_on_append() {
    let mut ws = crate::VirtualWorkspace::new();
    assert!(ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class Thing
        ---@field icon string
        local _Thing = {}

        ---@type Thing[]
        local values = {}
        values[#values + 1] = { icon = "" }
        "#
    ));
}

#[test]
fn declared_map_empty_slot_keeps_declared_collection_type() {
    let mut ws = crate::VirtualWorkspace::new();
    assert!(ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class Thing

        ---@type table<Thing, Thing[]|false>
        local cache = {}

        ---@param owner Thing
        ---@param member Thing
        local function add(owner, member)
            if not cache[owner] then cache[owner] = {} end
            local group = cache[owner]
            if group then group[#group + 1] = member end
            cache[owner] = false
        end
        "#
    ));
}

#[test]
fn declared_nested_container_empty_slot_preserves_all_value_arms() {
    let mut ws = crate::VirtualWorkspace::new();
    assert!(ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class Ent
        ---@class Portal

        ---@type table<Portal, table<Ent, Ent[]|false>>
        local cache = {}

        ---@param key Ent
        ---@param portal Portal
        ---@param group Ent[]?
        local function store(key, portal, group)
            if not cache[portal] then cache[portal] = {} end
            local portalCache = cache[portal]
            if group then
                portalCache[key] = group
            else
                portalCache[key] = false
            end
        end
        "#
    ));
}

#[test]
fn declared_index_signature_empty_slot_keeps_named_field_type() {
    let mut ws = crate::VirtualWorkspace::new();
    assert!(ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class cat
        ---@field icon string?
        ---@field [string] { icon: string, real: string }

        ---@type table<string, cat>
        local cats = {}

        ---@param category string
        ---@param displayName string
        ---@param surfaceId string
        ---@param categoryIcon string?
        local function add(category, displayName, surfaceId, categoryIcon)
            if not cats[category] then cats[category] = {} end
            local entry = cats[category]
            entry.icon = categoryIcon or entry.icon or ""
            entry[displayName] = { icon = entry.icon or "", real = surfaceId }
        end
        "#
    ));
}

#[test]
fn declared_array_invalid_append_still_reports() {
    let mut ws = crate::VirtualWorkspace::new();
    assert!(!ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class Thing

        ---@type Thing[]
        local values = {}
        values[#values + 1] = "not a Thing"
        "#
    ));
}

#[test]
fn declared_empty_container_controls_remain_clean() {
    let mut ws = crate::VirtualWorkspace::new_with_init_std_lib();
    assert!(ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class Ent
        ---@class Portal

        ---@type Ent[]
        local viaInsert = {}
        table.insert(viaInsert, {})

        ---@type Ent[]
        local neverFilled = {}

        ---@type table<Ent, Ent[]|false>
        local seed = {}

        ---@type table<Portal, table<Ent, Ent[]|false>>
        local cache = {}

        ---@param key Ent
        ---@param portal Portal
        ---@param group Ent[]
        local function controls(key, portal, group)
            if not cache[portal] then cache[portal] = seed end
            local typed = cache[portal]
            typed[key] = group
            typed[key] = false

            if not cache[portal] then cache[portal] = {} end
            local singleWrite = cache[portal]
            singleWrite[key] = group
        end
        "#
    ));
}

/// An empty `{}` is a valid value for any container type. A `[k] = nil` write in
/// a *second* closure bound to the same `fun(self: T)` slot made the element
/// type nilable, and that re-inference reached back and rejected the seed.
#[test]
fn empty_table_seed_survives_a_sibling_closure_clearing_an_element() {
    let mut ws = crate::VirtualWorkspace::new();

    assert!(ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class element
        ---@class holder
        ---@field map table<any, element>

        ---@param f fun(self: holder)
        local function hook(f) end

        hook(function(self) self.map = {} end)
        hook(function(self) self.map["k"] = nil end)
        "#
    ));
}

/// `if not t.k then t.k = {} end` runs exactly when `t.k` is missing, so `t.k`
/// is a table on every path out of the `if` — the same seed a plain
/// `t.k = {}` gives. Keeping that fact inside the branch left the writes after
/// it checked as if the table had never been seeded, so each was reported
/// against whichever sibling write the walk happened to record last.
#[test]
fn absence_guarded_seed_initialises_the_table_for_later_writes() {
    for seed in [
        "if not ext.slots then ext.slots = {} end",
        "if ext.slots == nil then ext.slots = {} end",
        "ext.slots = ext.slots or {}",
    ] {
        let mut ws = crate::VirtualWorkspace::new();
        assert!(
            ws.check_code_for(
                crate::DiagnosticCode::AssignTypeMismatch,
                &format!(
                    r#"
                    ---@class SeedPart
                    ---@class SeedExt
                    ---@param ext SeedExt
                    ---@param part SeedPart
                    local function use(ext, part)
                        {seed}
                        if part then ext.slots[1] = part end
                        ext.slots[1] = false
                    end
                    "#
                )
            ),
            "seed: {seed}"
        );
    }
}

/// Order must not decide it either: the same two writes the other way round
/// have to stay clean, or the slot's type is whichever writer the walk saw last
/// rather than the union of both.
#[test]
fn absence_guarded_seed_is_order_independent() {
    let mut ws = crate::VirtualWorkspace::new();

    assert!(ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class OrdPart
        ---@class OrdExt
        ---@param ext OrdExt
        ---@param part OrdPart
        local function use(ext, part)
            if not ext.slots then ext.slots = {} end
            ext.slots[1] = false
            ext.slots[1] = part
        end
        "#
    ));
}

/// A plain `if cond then t.k = {} end` guarantees nothing afterwards, so it
/// must not count as a seed.
#[test]
fn conditional_seed_that_is_not_an_absence_guard_is_not_an_initialiser() {
    let mut ws = crate::VirtualWorkspace::new();

    assert!(!ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class CondOwner
        ---@field slots integer[]
        local O = {}

        ---@param cond boolean
        function O:seed(cond)
            if cond then self.slots = {} end
            self.slots[1] = "not an integer"
        end
        "#
    ));
}

/// A `false` seed in a table literal is an inferred "not set yet" placeholder,
/// not a declared contract, so a later write of another type is not a mismatch.
#[test]
fn inferred_false_seed_is_lenient() {
    let mut ws = crate::VirtualWorkspace::new();

    assert!(ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class FalseSeed.Player
        local game = { betting = { curPlayer = false } }

        ---@param ply FalseSeed.Player
        local function setTurn(ply)
            game.betting.curPlayer = ply
        end
        "#
    ));
}

/// A declared `boolean` field is a contract and still reports.
#[test]
fn declared_boolean_field_still_reports() {
    let mut ws = crate::VirtualWorkspace::new();

    assert!(!ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@class DeclaredBool.Player
        ---@class DeclaredBool.Betting
        ---@field curPlayer boolean
        ---@type DeclaredBool.Betting
        local betting

        ---@param ply DeclaredBool.Player
        local function setTurn(ply)
            betting.curPlayer = ply
        end
        "#
    ));
}

#[test]
fn library_doc_field_not_tightened_by_cross_file_inferred_writers() {
    let mut ws = crate::VirtualWorkspace::new();

    // The doc class lives in a library workspace (like Doors' sh_api.lua
    // being a workspace.library for TARDIS), while the field's writers live
    // in the main workspace.
    let library_root = ws.virtual_url_generator.new_path("__test_sound_opts_lib");
    ws.analysis.add_library_workspace(library_root.clone());
    let library_uri = lsp_types::Uri::parse_from_file_path(&library_root.join("opts.lua")).unwrap();
    ws.analysis
        .update_file_by_uri(
            &library_uri,
            Some(
                r#"
                ---@class LibSoundOpts
                ---@field owner LibBase?
                "#
                .to_string(),
            ),
        )
        .expect("library file");

    ws.def_file(
        "lua/types.lua",
        r#"
        ---@class LibBase
        ---@class LibSubA: LibBase
        ---@class LibSubB: LibBase
    "#,
    );

    let a_id = ws.def_file(
        "lua/a.lua",
        r#"
        ---@param opts LibSoundOpts
        ---@param a LibSubA
        local function fa(opts, a)
            opts.owner = opts.owner or a
        end
    "#,
    );

    let b_id = ws.def_file(
        "lua/b.lua",
        r#"
        ---@param opts LibSoundOpts
        ---@param b LibSubB
        local function fb(opts, b)
            opts.owner = opts.owner or b
        end
    "#,
    );

    // `owner` is documented `LibBase?`; an inferred writer union must not
    // tighten it, so neither write may be flagged.
    for file_id in [a_id, b_id] {
        ws.analysis
            .diagnostic
            .enable_only(crate::DiagnosticCode::AssignTypeMismatch);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, tokio_util::sync::CancellationToken::new())
            .unwrap_or_default();
        let mismatches: Vec<_> = diagnostics
            .into_iter()
            .filter(|d| {
                d.code
                    == Some(lsp_types::NumberOrString::String(
                        crate::DiagnosticCode::AssignTypeMismatch
                            .get_name()
                            .to_string(),
                    ))
            })
            .collect();
        assert!(mismatches.is_empty(), "{mismatches:#?}");
    }
}

/// `self.Primary` is unresolvable on the receiver class, so the write's
/// right-hand side cannot produce a value. The member it feeds must record
/// `unknown`, not `never`: a `never` member is reserved for declared shapes
/// that contradict themselves and fails any structural check.
#[test]
fn member_write_of_unresolvable_read_is_not_never() {
    let mut ws = crate::VirtualWorkspace::new_with_init_std_lib();
    ws.analysis
        .diagnostic
        .enable_only(crate::DiagnosticCode::ParamTypeMismatch);
    let file_id = ws.def(
        r#"
            ---@class (partial) BulletT
            ---@field Num? number
            ---@field Damage? number

            ---@class BulletWeapCls

            ---@param b BulletT
            local function fire(b) end

            ---@param self BulletWeapCls
            local function shoot(self)
                local bullet = {}
                bullet.Num = 1
                bullet.Damage = self.Primary.Damage
                fire(bullet)
            end
        "#,
    );
    let diagnostics = ws
        .analysis
        .diagnose_file(file_id, tokio_util::sync::CancellationToken::new())
        .unwrap_or_default();
    let mismatches: Vec<_> = diagnostics
        .into_iter()
        .filter(|d| {
            d.code
                == Some(lsp_types::NumberOrString::String(
                    crate::DiagnosticCode::ParamTypeMismatch
                        .get_name()
                        .to_string(),
                ))
        })
        .collect();
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

/// Same shape but `self.Primary` resolves to a table type that lacks `Damage`:
/// the missing-field read floors to `nil`, which must not produce a member
/// mismatch against an optional field either.
#[test]
fn member_write_of_missing_doc_field_is_nil_not_mismatch() {
    let mut ws = crate::VirtualWorkspace::new_with_init_std_lib();
    ws.analysis
        .diagnostic
        .enable_only(crate::DiagnosticCode::ParamTypeMismatch);
    let file_id = ws.def(
        r#"
            ---@class (partial) BulletT2
            ---@field Num? number
            ---@field Damage? number

            ---@class AmmoT2
            ---@field ClipSize number

            ---@class WeapCls2
            ---@field Primary AmmoT2

            ---@param b BulletT2
            local function fire(b) end

            ---@param self WeapCls2
            local function shoot(self)
                local bullet = {}
                bullet.Num = 1
                bullet.Damage = self.Primary.Damage
                fire(bullet)
            end
        "#,
    );
    let diagnostics = ws
        .analysis
        .diagnose_file(file_id, tokio_util::sync::CancellationToken::new())
        .unwrap_or_default();
    let mismatches: Vec<_> = diagnostics
        .into_iter()
        .filter(|d| {
            d.code
                == Some(lsp_types::NumberOrString::String(
                    crate::DiagnosticCode::ParamTypeMismatch
                        .get_name()
                        .to_string(),
                ))
        })
        .collect();
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

/// Same shape with `Damage` *required* on the target class: the floored `nil`
/// must report with a real member reason rather than silently mismatching.
#[test]
fn member_write_of_missing_doc_field_reports_member_reason() {
    let mut ws = crate::VirtualWorkspace::new_with_init_std_lib();
    ws.analysis
        .diagnostic
        .enable_only(crate::DiagnosticCode::ParamTypeMismatch);
    let file_id = ws.def(
        r#"
            ---@class BulletT3
            ---@field Num number
            ---@field Damage number

            ---@class AmmoT3
            ---@field ClipSize number

            ---@class WeapCls3
            ---@field Primary AmmoT3

            ---@param b BulletT3
            local function fire(b) end

            ---@param self WeapCls3
            local function shoot(self)
                local bullet = {}
                bullet.Num = 1
                bullet.Damage = self.Primary.Damage
                fire(bullet)
            end
        "#,
    );
    let diagnostics = ws
        .analysis
        .diagnose_file(file_id, tokio_util::sync::CancellationToken::new())
        .unwrap_or_default();
    let mismatches: Vec<_> = diagnostics
        .into_iter()
        .filter(|d| {
            d.code
                == Some(lsp_types::NumberOrString::String(
                    crate::DiagnosticCode::ParamTypeMismatch
                        .get_name()
                        .to_string(),
                ))
        })
        .collect();
    assert_eq!(mismatches.len(), 1, "{mismatches:#?}");
    assert!(mismatches[0].message.contains("Damage"), "{mismatches:#?}");
}
/// A global table built by member writes across files infers as `MergedTable`;
/// passing it to a class parameter must member-check the merged structure
/// rather than failing with a bare `TypeNotMatch`.
#[test]
fn merged_table_across_files_satisfies_partial_class() {
    let mut ws = crate::VirtualWorkspace::new_with_init_std_lib();
    let mismatches = merged_table_param_diagnostics(
        &mut ws,
        r#"
            ---@class (partial) BulletT4
            ---@field Num? number
            ---@field Damage? number
        "#,
        r#"
            bullet.Damage = 20
        "#,
    );
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

/// The merged-table check must still report a genuinely incompatible member.
#[test]
fn merged_table_across_files_reports_required_member_mismatch() {
    let mut ws = crate::VirtualWorkspace::new_with_init_std_lib();
    let mismatches = merged_table_param_diagnostics(
        &mut ws,
        r#"
            ---@class (partial) BulletT4
            ---@field Num? number
            ---@field Damage number
        "#,
        r#"
            bullet.Damage = "not a number"
        "#,
    );
    assert_eq!(mismatches.len(), 1, "{mismatches:#?}");
}

#[cfg(test)]
fn merged_table_param_diagnostics(
    ws: &mut crate::VirtualWorkspace,
    class_def: &str,
    other_file: &str,
) -> Vec<lsp_types::Diagnostic> {
    ws.analysis
        .diagnostic
        .enable_only(crate::DiagnosticCode::ParamTypeMismatch);
    let file_ids = ws.def_files(vec![
        (
            "a.lua",
            &format!(
                r#"
                {class_def}

                ---@param b BulletT4
                function fire(b) end

                bullet = {{}}
                bullet.Num = 1
                bullet.Damage = 10
                fire(bullet)
            "#
            ),
        ),
        (
            "b.lua",
            &format!(
                r#"
                bullet = {{}}
                bullet.Num = 2
                {other_file}
            "#
            ),
        ),
    ]);
    ws.analysis
        .diagnose_file(file_ids[0], tokio_util::sync::CancellationToken::new())
        .unwrap_or_default()
        .into_iter()
        .filter(|d| {
            d.code
                == Some(lsp_types::NumberOrString::String(
                    crate::DiagnosticCode::ParamTypeMismatch
                        .get_name()
                        .to_string(),
                ))
        })
        .collect()
}

/// Regression (CityRP `cl_tooltip.lua`): an AccessorFunc backing field with no
/// force type is synthesized as an uninformative inferred member — a
/// placeholder — while a real assignment writes a concrete type. The
/// placeholder must not absorb the writer's evidence: `entity` resolves `E4`,
/// so `min`/`max` narrow back to number through the `ToScreen().x`
/// reassignment and `math.max` stays clean.
#[test]
fn accessor_func_any_field_does_not_mask_inferred_writes() {
    let mut ws = crate::VirtualWorkspace::new_with_init_std_lib();
    ws.analysis
        .diagnostic
        .enable_only(crate::DiagnosticCode::ParamTypeMismatch);
    ws.def_gmod_call_arg_builtins();
    let file_id = ws.def(
        r#"
            ---@class TSD4
            ---@field x number
            ---@field y number

            ---@class V4
            local V4 = {}
            ---@return TSD4
            function V4:ToScreen() end

            ---@class E4
            local E4 = {}
            ---@return V4
            function E4:OBBMins() end
            ---@return V4
            function E4:OBBMaxs() end
            ---@return V4, V4
            function E4:GetRotatedAABB(mins, maxs) end
            ---@param lpos V4
            ---@return V4
            function E4:LocalToWorld(lpos) end

            ---@class P4

            DEFINE_BASECLASS("P4")
            PANEL = {} ---@class TT4 : P4

            AccessorFunc(PANEL, "entity", "Entity")

            ---@param entity E4
            function PANEL:SetEntity(entity)
                self.entity = entity
            end

            function PANEL:Think()
                if IsValid(self.entity) then
                    local entity = self.entity
                    local min, max = entity:GetRotatedAABB(entity:OBBMins() * 0.5, entity:OBBMaxs() * 0.5)
                    min = entity:LocalToWorld(min):ToScreen().x
                    max = entity:LocalToWorld(max):ToScreen().x
                    local m = math.max(min, max)
                end
            end

            vgui.Register("TT4", PANEL, "P4")
        "#,
    );
    let diagnostics = ws
        .analysis
        .diagnose_file(file_id, tokio_util::sync::CancellationToken::new())
        .unwrap_or_default();
    let mismatches: Vec<_> = diagnostics
        .into_iter()
        .filter(|d| {
            d.code
                == Some(lsp_types::NumberOrString::String(
                    crate::DiagnosticCode::ParamTypeMismatch
                        .get_name()
                        .to_string(),
                ))
        })
        .collect();
    assert!(
        mismatches.is_empty(),
        "uninformative AccessorFunc field must not mask the inferred writer: {mismatches:#?}"
    );

    // Root cause: the synthesized backing field declares existence, not a
    // type, so the `self.entity` read must resolve the concrete `SetEntity`
    // write rather than the placeholder.
    assert_eq!(infer_first_index_ty(&ws, file_id, "self.entity"), "E4");
}

/// An unforced `AccessorFunc` backing field with no writes still resolves
/// `any` — existence without a type.
#[test]
fn accessor_func_any_field_without_writes_resolves_any() {
    let mut ws = crate::VirtualWorkspace::new_with_init_std_lib();
    ws.def_gmod_call_arg_builtins();
    let file_id = ws.def(
        r#"
            ---@class P5

            DEFINE_BASECLASS("P5")
            PANEL = {} ---@class TT5 : P5

            AccessorFunc(PANEL, "entity", "Entity")

            function PANEL:Think()
                local entity = self.entity
            end

            vgui.Register("TT5", PANEL, "P5")
        "#,
    );
    assert_eq!(infer_first_index_ty(&ws, file_id, "self.entity"), "any");
}

#[cfg(test)]
fn infer_first_index_ty(
    ws: &crate::VirtualWorkspace,
    file_id: crate::FileId,
    field_access: &str,
) -> String {
    use glua_parser::{LuaAstNode, LuaExpr, LuaIndexExpr};

    let semantic_model = ws
        .analysis
        .compilation
        .get_semantic_model(file_id)
        .expect("semantic model");
    let target = semantic_model
        .get_root()
        .descendants::<LuaIndexExpr>()
        .find(|expr| expr.syntax().text() == field_access)
        .expect("field access should exist");
    let ty = semantic_model
        .infer_expr(LuaExpr::IndexExpr(target))
        .unwrap_or(crate::LuaType::Unknown);
    crate::humanize_type(
        ws.analysis.compilation.get_db(),
        &ty,
        crate::RenderLevel::Simple,
    )
}

/// Regression (CityRP `cl_vgui3d2d.lua`): `{ ... }` stores the pack as a
/// `Tuple([Variadic(T)])`; indexing `key[1]` read the stored element verbatim
/// and surfaced `Variadic(T)` — a multi-value container — where a single value
/// sits. The element at that position is the spread's first value, `T`.
#[test]
fn vararg_pack_table_index_unwraps_variadic_element() {
    let mut ws = crate::VirtualWorkspace::new_with_init_std_lib();
    ws.def(
        r#"
            ---@class Panel
            Panel = {}
            ---@param keyCode number
            function Panel:OnMouseReleased(keyCode) end
        "#,
    );
    let file_id = ws.def(
        r#"
            local usedpanel = {}

            ---@param pnl Panel
            local function postPanelEvent(pnl, event, ...)
                if pnl[event] then
                    pnl[event](pnl, ...)
                    usedpanel[pnl] = { ... }
                    return true
                end
                return false
            end

            local function release()
                for pnl, key in pairs(usedpanel) do
                    if pnl["OnMouseReleased"] then
                        pnl["OnMouseReleased"](pnl, key[1])
                    end
                end
            end
        "#,
    );

    let mismatches: Vec<_> = ws
        .analysis
        .diagnose_file(file_id, tokio_util::sync::CancellationToken::new())
        .unwrap_or_default()
        .into_iter()
        .filter(|d| {
            d.code
                == Some(lsp_types::NumberOrString::String(
                    crate::DiagnosticCode::ParamTypeMismatch
                        .get_name()
                        .to_string(),
                ))
        })
        .collect();
    assert!(
        mismatches.is_empty(),
        "uninformative vararg element must not flag a number param: {mismatches:#?}"
    );

    use glua_parser::{LuaAstNode, LuaExpr};
    let (key_ty, elem_ty) = {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let mut key_ty = crate::LuaType::Unknown;
        let mut elem_ty = crate::LuaType::Unknown;
        for expr in semantic_model.get_root().descendants::<LuaExpr>() {
            let text = expr.syntax().text().to_string();
            if text == "key[1]" {
                elem_ty = semantic_model
                    .infer_expr(expr)
                    .unwrap_or(crate::LuaType::Unknown);
            } else if text == "key" && key_ty.is_unknown() {
                // the `for pnl, key` loop variable
                key_ty = semantic_model
                    .infer_expr(expr)
                    .unwrap_or(crate::LuaType::Unknown);
            }
        }
        (key_ty, elem_ty)
    };
    assert_eq!(
        ws.humanize_type(elem_ty),
        "unknown",
        "indexing a vararg-packed table yields the element type, not the pack; key={}",
        ws.humanize_type(key_ty)
    );
}

/// Forwarding `...` through a chain of locals must keep the pack open for the
/// callee's vararg: `pack`'s `T...` binds the forwarded pack as `T ...`, not a
/// closed one-element tuple, so `arg[2]` stays a valid read.
#[test]
fn forwarded_dots_binds_open_variadic_param() {
    let mut ws = crate::VirtualWorkspace::new_with_init_std_lib();
    let file_id = ws.def(
        r#"
            local pack = table.pack or function(...) return { n = 1, ... } end
            local function date_from(...)
                local arg = pack(...)
                local y, m, d = arg[1], arg[2], arg[3]
                return y, m, d
            end
            local function date(...)
                return date_from(...)
            end
            date("x")
        "#,
    );
    let diags: Vec<_> = ws
        .analysis
        .diagnose_file(file_id, tokio_util::sync::CancellationToken::new())
        .unwrap_or_default()
        .into_iter()
        .filter(|d| {
            d.code
                == Some(lsp_types::NumberOrString::String(
                    crate::DiagnosticCode::UndefinedField.get_name().to_string(),
                ))
        })
        .collect();
    assert!(
        diags.is_empty(),
        "a forwarded vararg pack must keep its open arity: {diags:#?}"
    );
}

#[cfg(test)]
fn def_player_meta_library(ws: &mut crate::VirtualWorkspace) {
    ws.def(
        r#"
        ---@meta
        ---@class Entity

        ---@class Player : Entity
        ---@field Money number

        ---@generic T : table
        ---@param metaName `T`
        ---@return (definition) T
        function _G.FindMetaTable(metaName) end

        ---@return Player
        function LocalPlayer() end
        "#,
    );
    ws.def_file(
        "gamemode/core/metatables/sh_player.lua",
        r#"
        ---@param name string
        ---@param prefix string?
        function PlayerNSReader(name, prefix)
            local pMeta = FindMetaTable("Player")
            local fn = (prefix or "Get") .. name
            pMeta[fn] = function(self, def)
                return def
            end
        end
        "#,
    );
}

/// Regression (CityRP `cl_hooks.lua`): a computed-key write on an entity lands
/// in its Lua table under whatever key it names. The `pMeta[fn]` accessors
/// share only a `string` key type with it, not a slot, so they state no type
/// the write can mismatch.
#[test]
fn computed_key_write_on_class_is_not_held_to_other_computed_key_writes() {
    let mut ws = crate::VirtualWorkspace::new();
    def_player_meta_library(&mut ws);
    assert!(ws.check_file_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        "gamemode/core/hooks/cl_hooks.lua",
        r#"
        ---@param key string
        ---@param value number
        local function storeLocalVariable(key, value)
            LocalPlayer()[key] = value
        end
        "#,
    ));
}

/// A literal key names a declared field, so a wrong value is still reported.
#[test]
fn literal_key_write_on_class_still_reports_declared_field_mismatch() {
    let mut ws = crate::VirtualWorkspace::new();
    def_player_meta_library(&mut ws);
    assert!(!ws.check_file_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        "gamemode/core/hooks/cl_money_name.lua",
        r#"
        LocalPlayer().Money = "broke"
        "#,
    ));
    assert!(!ws.check_file_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        "gamemode/core/hooks/cl_money_string.lua",
        r#"
        LocalPlayer()["Money"] = "broke"
        "#,
    ));
}

/// A declared `[string]: V` index field is the contract for computed-key writes.
#[test]
fn computed_key_write_on_class_is_held_to_declared_index_field() {
    let mut ws = crate::VirtualWorkspace::new();
    ws.def(
        r#"
        ---@class NetVarStore
        ---@field [string] number
        "#,
    );
    assert!(!ws.check_file_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        "gamemode/core/libraries/sh_netvar_bad.lua",
        r#"
        ---@param store NetVarStore
        ---@param key string
        local function set(store, key)
            store[key] = "x"
        end
        "#,
    ));
    assert!(ws.check_file_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        "gamemode/core/libraries/sh_netvar_ok.lua",
        r#"
        ---@param store NetVarStore
        ---@param key string
        local function set(store, key)
            store[key] = 1
        end
        "#,
    ));
}

#[test]
fn trailing_call_return_values_are_all_checked_against_table_elements() {
    let mut ws = crate::VirtualWorkspace::new();
    assert!(!ws.check_code_for(
        crate::DiagnosticCode::AssignTypeMismatch,
        r#"
        ---@return number, string
        local function f() end

        ---@type table<integer, string>
        local t = { "a", f() }
        "#,
    ));
}

const HOOK_CLASS: &str = r#"
---@class HookPanel
local HookPanel = {}

---@return boolean
function HookPanel:Paint(width, height) end

---@param keyCode number
---@return boolean
function HookPanel:OnMousePressed(keyCode) end

---@param cb fun(): boolean
local function takes_callback(cb) end

---@type HookPanel
local p
"#;

#[test]
fn closure_without_returns_may_be_assigned_to_a_method_field() {
    let mut ws = crate::VirtualWorkspace::new();
    let code = format!(
        "{HOOK_CLASS}\np.Paint = function(s, w, h) end\np.OnMousePressed = function(s, code) end"
    );
    assert!(ws.check_code_for(crate::DiagnosticCode::AssignTypeMismatch, &code));
}

#[test]
fn closure_returning_the_wrong_type_is_still_reported_for_a_method_field() {
    let mut ws = crate::VirtualWorkspace::new();
    let code = format!("{HOOK_CLASS}\np.Paint = function(s, w, h) return \"x\" end");
    assert!(!ws.check_code_for(crate::DiagnosticCode::AssignTypeMismatch, &code));
}

#[test]
fn closure_without_returns_stays_rejected_as_a_callback_argument() {
    let mut ws = crate::VirtualWorkspace::new();
    let code = format!("{HOOK_CLASS}\ntakes_callback(function() end)");
    assert!(!ws.check_code_for(crate::DiagnosticCode::ParamTypeMismatch, &code));
}
