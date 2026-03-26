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

        // TODO: 解决枚举值运算结果的推断问题
        // 暂时没有好的方式去处理这个警告, 在 ts 中, 枚举值运算的结果不是实际值, 但我们目前的结果是实际值, 所以难以处理
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

    /// 暂时无法解决的测试
    #[test]
    fn test_error() {
        // let mut ws = VirtualWorkspace::new();

        // 推断类型异常
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

    // 可能需要处理的
    #[test]
    fn test_pending() {
        let mut ws = VirtualWorkspace::new();
        let mut config = Emmyrc::default();
        config.strict.array_index = true;
        ws.analysis.update_config(config.into());

        assert!(ws.check_code_for_namespace(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            ---@class A
            local a = {}

            ---@class B: A
            local b = a
                "#
        ));

        // 允许接受父类.
        // TODO: 接受父类时应该检查是否具有子类的所有非可空成员.
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

        // 数组类型匹配允许可空, 但在初始化赋值时, 不允许直接赋值`nil`(其实是偷懒了, table_expr 推断没有处理边缘情况, 可能后续会做处理允许)
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
        // TODO: 解决枚举值运算结果的推断问题
        // 暂时没有好的方式去处理这个警告, 在 ts 中, 枚举值运算的结果不是实际值, 但我们目前的结果是实际值, 所以难以处理
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
    fn test_never_target_no_assign_mismatch() {
        let mut ws = VirtualWorkspace::new();
        // Assigning to a field whose type resolves to `never` should not produce a diagnostic.
        // `never` indicates a type inference limitation rather than an actual type error.
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
                ---@type never
                local x
                x = 42
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
