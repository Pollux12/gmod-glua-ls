#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, Emmyrc, LuaType, VirtualWorkspace};
    use glua_parser::{LuaAstNode, LuaNameExpr};
    use googletest::prelude::*;
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    fn set_gmod_enabled(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
    }

    fn file_has_diagnostic(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        diagnostic_code: DiagnosticCode,
    ) -> bool {
        ws.analysis.diagnostic.enable_only(diagnostic_code);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            diagnostic_code.get_name().to_string(),
        ));
        diagnostics.iter().any(|diagnostic| diagnostic.code == code)
    }

    fn nth_name_expr_type_from_end(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        name: &str,
        nth_from_end: usize,
    ) -> LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let root = semantic_model.get_root();
        let name_exprs = root
            .clone()
            .descendants::<LuaNameExpr>()
            .filter(|expr| expr.get_name_text().as_deref() == Some(name))
            .collect::<Vec<_>>();
        let name_expr = name_exprs
            .into_iter()
            .rev()
            .nth(nth_from_end)
            .expect("expected matching name expression");
        semantic_model
            .get_semantic_info(name_expr.syntax().clone().into())
            .expect("expected semantic info for name expression")
            .typ
    }

    #[test]
    fn test_closure_return() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        --- @generic T, U
        --- @param arr T[]
        --- @param op fun(item: T, index: integer): U
        --- @return U[]
        function map(arr, op)
        end
        "#,
        );

        let ty = ws.expr_ty(
            r#"
        map({ 1, 2, 3 }, function(item, i)
            return tostring(item)
        end)
        "#,
        );
        let expected = ws.ty("string[]");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_issue_140_1() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
        ---@class Object

        ---@class T
        local inject2class ---@type (Object| T)?
        if jsonClass then
            if inject2class then
                A = inject2class
            end
        end
        "#,
        );

        let ty = ws.expr_ty("A");
        let type_desc = ws.humanize_type(ty);
        assert_eq!(type_desc, "T");
    }

    #[test]
    fn test_issue_140_2() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
        local msgBody ---@type { _hgQuiteMsg : 1 }?
        if not msgBody or not msgBody._hgQuiteMsg then
        end
        "#
        ));
    }

    #[test]
    fn test_issue_140_3() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
        local SELF ---@type unknown
        if SELF ~= nil then
            SELF:OnDestroy()
        end
        "#
        ));
    }

    #[test]
    fn test_issue_107() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
        ---@type {bar?: fun():string}
        local props
        if props.bar then
            local foo = props.bar()
        end

        if type(props.bar) == 'function' then
            local foo = props.bar()
        end

        local foo = props.bar and props.bar() or nil
        "#
        ));
    }

    #[test]
    fn test_issue_100() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
        local f = io.open('', 'wb')
        if not f then
            error("Could not open a file")
        end

        f:write('')
        "#
        ));
    }

    #[test]
    fn test_issue_93() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        local text    --- @type string[]?
        if staged then
            local text1 --- @type string[]?
            text = text1
        else
            local text2 --- @type string[]?
            text = text2
        end

        if not text then
            return
        end

        --- @param _a string[]
        local function foo(_a) end

        foo(text)
        "#
        ));
    }

    #[test]
    fn test_null_function_field() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
        ---@class A
        ---@field aaa? fun(a: string)


        local c ---@type A

        if c.aaa then
            c.aaa("aaa")
        end
        "#
        ))
    }

    #[test]
    fn test_issue_162() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            --- @class Foo
            --- @field a? fun()

            --- @param _o Foo
            function bar(_o) end

            bar({})
            "#
        ));
    }

    #[test]
    fn test_redefine() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@class AA
            ---@field b string

            local a = 1
            a = 1

            ---@type AA
            local a

            print(a.b)
            "#
        ));
    }

    #[test]
    fn test_issue_165() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
local a --- @type table?
if not a or #a == 0 then
    return
end

print(a.h)
            "#
        ));
    }

    #[test]
    fn test_issue_160() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
local a --- @type table?

if not a then
    assert(a)
end

print(a.field)
            "#
        ));
    }

    #[test]
    fn test_issue_210() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        --- @class A
        --- @field b integer

        local a = {}

        --- @type A
        a = { b = 1 }

        --- @param _a A
        local function foo(_a) end

        foo(a)
        "#
        ));
    }

    #[test]
    fn test_doc_function_assignment_narrowing0() {
        let mut ws = VirtualWorkspace::new();

        let code = r#"
        local i --- @type integer|fun():string
        i = "str"
        A = i
        "#;

        ws.def(code);
        let a = ws.expr_ty("A");
        let a_desc = ws.humanize_type_detailed(a);
        assert_eq!(a_desc, "\"str\"");
    }

    #[test]
    fn test_doc_member_assignment_prefers_annotation_source() {
        let mut ws = VirtualWorkspace::new();

        let code = r#"
        local t = {}
        t.a = "hello"
        ---@type string|number
        t.a = 1
        b = t.a
        "#;

        ws.def(code);
        assert_eq!(ws.expr_ty("b"), ws.ty("integer"));
    }

    #[test]
    fn test_assignment_narrow_drops_nil_on_mismatch() {
        let mut ws = VirtualWorkspace::new();

        let code = r#"
        local a ---@type string?
        a = 1
        b = a
        "#;

        ws.def(code);
        assert_eq!(ws.expr_ty("b"), LuaType::IntegerConst(1));
    }

    #[test]
    fn test_doc_member_assignment_falls_back_to_annotation() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local t = {}
            ---@type string|number
            t.a = true
            b = t.a
        "#,
        );

        let b = ws.expr_ty("b");
        let expected_ty = ws.ty("string|number");
        let expected = ws.humanize_type(expected_ty);
        assert_eq!(ws.humanize_type(b), expected);
    }

    #[test]
    fn test_doc_function_assignment_narrowing() {
        let mut ws = VirtualWorkspace::new();

        let code = r#"
        local i --- @type integer|fun():string
        i = function() end
        _ = i()
        A = i
        "#;

        ws.def(code);

        assert!(ws.check_code_for(DiagnosticCode::CallNonCallable, code));
        assert!(ws.check_code_for(DiagnosticCode::NeedCheckNil, code));

        let a = ws.expr_ty("A");
        let a_desc = ws.humanize_type_detailed(a);
        assert_eq!(a_desc, "fun()");
    }

    #[test]
    fn test_issue_224() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
        --- @class A

        --- @param opts? A
        --- @return A
        function foo(opts)
            opts = opts or {}
            return opts
        end
        "#
        ));
    }

    #[test]
    fn test_elseif() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
---@class D11
---@field public a string

---@type D11|nil
local a

if not a then
elseif a.a then
    print(a.a)
end

        "#
        ));
    }

    #[test]
    fn test_issue_266() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
        --- @return string
        function baz() end

        local a
        a = baz() -- a has type nil but should be string
        d = a
        "#
        ));

        let d = ws.expr_ty("d");
        let d_desc = ws.humanize_type(d);
        assert_eq!(d_desc, "string");
    }

    #[test]
    fn test_issue_277() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@param t? table
        function myfun3(t)
            if type(t) ~= 'table' then
                return
            end

            a = t
        end
        "#,
        );

        let a = ws.expr_ty("a");
        let a_desc = ws.humanize_type(a);
        assert_eq!(a_desc, "table");
    }

    #[test]
    fn test_docint() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local stack = 0
            if stack ~= 0 then
                a = stack
            end
        "#,
        );

        let a = ws.expr_ty("a");
        let a_desc = ws.humanize_type(a);
        assert_eq!(a_desc, "integer");
    }

    #[test]
    fn test_issue_921_or_with_empty_table() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            --- @class Opts
            --- @field a? string

            local opts --- @type Opts?

            -- Test expression type: opts or {} should narrow to Opts
            E = opts or {}
            "#,
        );

        let e_ty = ws.expr_ty("E");
        assert_eq!(ws.humanize_type(e_ty), "Opts");
    }

    #[test]
    fn test_issue_921_or_with_table_type() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local opts --- @type table?

            -- Test with plain table? type
            E = opts or {}
            "#,
        );

        let e_ty = ws.expr_ty("E");
        assert_eq!(ws.humanize_type(e_ty), "table");
    }

    #[test]
    fn test_issue_921_self_assignment_with_table() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local opts --- @type table?

            opts = opts or {}

            E = opts
            "#,
        );

        let e_ty = ws.expr_ty("E");
        assert_eq!(ws.humanize_type(e_ty), "table");
    }

    #[test]
    fn test_issue_921_self_assignment_with_class_empty_table() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            --- @class Opts
            --- @field a? string

            local opts0 --- @type Opts?
            local opts1 --- @type Opts?

            opts0 = opts0 or {}
            opts1 = opts0 or { a = 'a' }

            E0 = opts0
            E1 = opts1
            "#,
        );

        // After self-assignment opts = opts or {}, opts should be narrowed to Opts
        let e0_ty = ws.expr_ty("E0");
        assert_eq!(ws.humanize_type(e0_ty), "Opts");
        let e1_ty = ws.expr_ty("E1");
        assert_eq!(ws.humanize_type(e1_ty), "Opts");
    }

    #[test]
    fn test_issue_921_and_with_string_nullable() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            --- @class Opts
            --- @field a? string

            local opts --- @type Opts

            -- When opts.a is string?, result should be table|nil
            -- The table {'a'} is inferred as a tuple containing 'a'
            E = opts.a and { 'a' }
            "#,
        );

        let e_ty = ws.expr_ty("E");
        assert_eq!(ws.humanize_type(e_ty), r#"("a")?"#);
    }

    #[test]
    fn test_issue_921_and_with_boolean_nullable_table() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            --- @class Opts
            --- @field b? boolean

            local opts --- @type Opts

            -- When opts.b is boolean?, result should be false|nil|table
            E = opts.b and { 'b' }
            "#,
        );

        let e_ty = ws.expr_ty("E");
        assert_eq!(ws.humanize_type(e_ty), r#"(false|("b"))?"#);
    }

    #[test]
    fn test_issue_921_and_with_boolean_nullable_string() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local bool --- @type boolean?

            -- When bool is boolean?, result should be false|nil|'a'
            E = bool and 'a'
            "#,
        );

        let e_ty = ws.expr_ty("E");
        assert_eq!(ws.humanize_type(e_ty), r#"(false|"a")?"#);
    }

    #[test]
    fn test_issue_147() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local d ---@type string?
            if d then
                local d2 = function(...)
                    e = d
                end
            end

        "#,
        );

        let e = ws.expr_ty("e");
        assert_eq!(e, LuaType::String);
    }

    #[test]
    fn test_issue_325() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        while condition do
            local a ---@type string?
            if not a then
                break
            end
            b = a
        end

        "#,
        );

        let b = ws.expr_ty("b");
        assert_eq!(b, LuaType::String);
    }

    #[test]
    fn test_issue_347() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
        --- @param x 'a'|'b'
        --- @return 'a'|'b'
        function foo(x)
        if x ~= 'a' and x ~= 'b' then
            error('invalid behavior')
        end

        return x
        end
        "#,
        ));
    }

    #[test]
    fn test_issue_339() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        --- @class A

        local a --- @type A|string

        if type(a) == 'table' then
            b = a -- a should be A
        else
            c = a -- a should be string
        end
        "#,
        );

        let b = ws.expr_ty("b");
        let b_expected = ws.ty("A");
        assert_eq!(b, b_expected);

        let c = ws.expr_ty("c");
        let c_expected = ws.ty("string");
        assert_eq!(c, c_expected);
    }

    #[test]
    fn test_narrow_after_error_branches() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        local r --- @type string?
        local a --- @type boolean
        if not r then
            if a then
                error()
            else
                error()
            end
        end

        b = r -- should be string
        "#,
        );

        let b = ws.expr_ty("b");
        assert_eq!(b, LuaType::String);
    }

    #[test]
    fn test_unknown_type() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        local a
        b = a
        "#,
        );

        let b = ws.expr_ty("b");
        let b_expected = ws.ty("nil");
        assert_eq!(b, b_expected);
    }

    #[test]
    fn test_issue_367() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        local files
        local function init()
            if files then
                return
            end
            files = {}
            a = files -- a 与 files 现在均为 nil
        end
        "#,
        );

        let a = ws.expr_ty("a");
        assert!(a != LuaType::Nil);

        ws.def(
            r#"
            ---@alias D10.data
            ---| number
            ---| string
            ---| boolean
            ---| table
            ---| nil

            ---@param data D10.data
            local function init(data)
                ---@cast data table

                b = data -- data 现在仍为 `10.data` 而不是 `table`
            end
            "#,
        );

        let b = ws.expr_ty("b");
        let b_desc = ws.humanize_type(b);
        assert_eq!(b_desc, "table");
    }

    #[test]
    fn test_issue_364() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param k integer
            ---@param t table<integer,integer>
            function foo(k, t)
                if t and t[k] then
                    return t[k]
                end

                if t then
                    -- t is nil -- incorrect
                    t[k] = 1 -- t may be nil -- incorrect
                end
            end
            "#,
        ));
    }

    #[test]
    fn test_issue_382() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Trigger

            ---@class Event
            ---@field private wait_pushing? Trigger[]
            local M


            ---@param trigger Trigger
            function M:add_trigger(trigger)
                if not self.wait_pushing then
                    self.wait_pushing = {}
                end
                self.wait_pushing[1] = trigger
            end

            ---@private
            function M:check_waiting()
                if self.wait_pushing then
                end
            end
            "#,
        ));
    }

    #[test]
    fn test_issue_369() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            --- @enum myenum
            local myenum = { A = 1 }

            --- @param x myenum|{}
            function foo(x)
                if type(x) ~= 'table' then
                    a = x
                else
                    b = x
                end
            end
        "#,
        );

        let a = ws.expr_ty("a");
        let a_expected = ws.ty("myenum");
        assert_eq!(a, a_expected);

        let b = ws.expr_ty("b");
        let b_expected = ws.ty("{}");
        assert_eq!(b, b_expected);
    }

    #[test]
    fn test_issue_373() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            --- @alias myalias string|string[]

            --- @param x myalias
            function foo(x)
                if type(x) == 'string' then
                    a = x
                elseif type(x) == 'table' then
                    b = x
                end
            end
        "#,
        );

        let a = ws.expr_ty("a");
        let a_expected = ws.ty("string");
        assert_eq!(a, a_expected);

        let b = ws.expr_ty("b");
        let b_expected = ws.ty("string[]");
        assert_eq!(b, b_expected);
    }

    #[test]
    fn test_call_cast() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"

            ---@return boolean
            ---@return_cast n integer
            local function isInteger(n)
                return true
            end

            local a ---@type integer | string

            if isInteger(a) then
                d = a
            else
                e = a
            end

        "#,
        );

        let d = ws.expr_ty("d");
        let d_expected = ws.ty("integer");
        assert_eq!(d, d_expected);

        let e = ws.expr_ty("e");
        let e_expected = ws.ty("string");
        assert_eq!(e, e_expected);
    }

    #[test]
    fn test_call_cast2() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"

        ---@class My2

        ---@class My1

        ---@class My3:My2,My1
        local m = {}


        ---@return boolean
        ---@return_cast self My1
        function m:isMy1()
        end

        ---@return boolean
        ---@return_cast self My2
        function m:isMy2()
        end

        if m:isMy1() then
            a = m
        elseif m:isMy2() then
            b = m
        end
        "#,
        );

        let a = ws.expr_ty("a");
        let a_expected = ws.ty("My1");
        assert_eq!(a, a_expected);

        let b = ws.expr_ty("b");
        let b_expected = ws.ty("My2");
        assert_eq!(b, b_expected);
    }

    #[test]
    fn test_issue_423() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
        --- @return string?
        local function bar() end

        --- @param a? string
        function foo(a)
        if not a then
            a = bar()
            assert(a)
        end

        --- @type string
        local _ = a -- incorrect error
        end
        "#,
        ));
    }

    #[test]
    fn test_issue_472() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::UnnecessaryIf,
            r#"
            worldLightLevel = 0
            worldLightColor = 0
            Gmae = {}
            ---@param color integer
            ---@param level integer
            function Game.setWorldLight(color, level)
                local previousColor = worldLightColor
                local previousLevel = worldLightLevel

                worldLightColor = color
                worldLightLevel = level

                if worldLightColor ~= previousColor or worldLightLevel ~= previousLevel then
                    -- Do something...
                end
            end
            "#
        ))
    }

    #[test]
    fn test_issue_478() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
            --- @param line string
            --- @param b boolean
            --- @return string
            function foo(line, b)
                return b and line or line
            end
            "#
        ));
    }

    #[test]
    fn test_issue_491() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
            ---@param srow integer?
            function foo(srow)
                srow = srow or 0

                return function()
                    ---@return integer
                    return function()
                        return srow
                    end
                end
            end
            "#
        ));
    }

    #[test]
    fn test_issue_288() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
                --- @alias MyFun fun(): string[]
                local f --- @type MyFun

                if type(f) == 'function' then
                     _, res = pcall(f)
                end
            "#,
        );

        let res = ws.expr_ty("res");
        let expected_ty = ws.ty("string|string[]");
        assert_eq!(res, expected_ty);
    }

    #[test]
    fn test_issue_480() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.check_code_for(
            DiagnosticCode::UnnecessaryAssert,
            r#"
            --- @param a integer?
            --- @param c boolean
            function foo(a, c)
                if c then
                    a = 1
                end

                assert(a)
            end
            "#,
        );
    }

    #[test]
    fn test_issue_526() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            --- @alias A { kind: 'A'}
            --- @alias B { kind: 'B'}

            local x --- @type A|B

            if x.kind == 'A' then
                a = x
                return
            end

            b = x
            "#,
        );

        let a = ws.expr_ty("a");
        let a_expected = ws.ty("A");
        assert_eq!(a, a_expected);
        let b = ws.expr_ty("b");
        let b_expected = ws.ty("B");
        assert_eq!(b, b_expected);
    }

    #[test]
    fn test_issue_583() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            --- @param sha string
            local function get_hash_color(sha)
            local r, g, b = sha:match('(%x)%x(%x)%x(%x)')
            assert(r and g and b, 'Invalid hash color')
            local _ = r --- @type string
            local _ = g --- @type string
            local _ = b --- @type string
            end
            "#,
        );
    }

    #[test]
    fn test_issue_584() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local function foo()
                for _ in ipairs({}) do
                    break
                end

                local a
                if a == nil then
                    a = 1
                    local _ = a --- @type integer
                end
            end
            "#,
        );
    }

    #[test]
    fn test_feature_inherit_flow_from_const_local() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            local ret --- @type string | nil

            local h = type(ret) == "string"
            if h then
                a = ret
            end

            local e = type(ret)
            if e == "string" then
                b = ret
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let a_expected = ws.ty("string");
        assert_eq!(a, a_expected);
        let b = ws.expr_ty("b");
        let b_expected = ws.ty("string");
        assert_eq!(b, b_expected);
    }

    #[test]
    fn test_feature_generic_type_guard() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@generic T
            ---@param type `T`
            ---@return TypeGuard<T>
            local function instanceOf(inst, type)
                return true
            end

            local ret --- @type string | nil

            if instanceOf(ret, "string") then
                a = ret
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let a_expected = ws.ty("string");
        assert_eq!(a, a_expected);
    }

    #[test]
    fn test_issue_598() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@class A<T>
            A = {}
            ---@class IDisposable
            ---@class B<T>: IDisposable

            ---@class AnonymousObserver<T>: IDisposable

            ---@generic T
            ---@return AnonymousObserver<T>
            function createAnonymousObserver()
            end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
                ---@param observer fun(value: T) | B<T>
                ---@return IDisposable
                function A:subscribe(observer)
                    local typ = type(observer)
                    if typ == 'function' then
                        ---@cast observer fun(value: T)
                        observer = createAnonymousObserver()
                    elseif typ == 'table' then
                        ---@cast observer -function
                        observer = createAnonymousObserver()
                    end

                    return observer
                end
            "#,
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::ReturnTypeMismatch,
            r#"
                ---@param observer fun(value: T) | B<T>
                ---@return IDisposable
                function A:test2(observer)
                    local typ = type(observer)
                    if typ == 'table' then
                        ---@cast observer -function
                        observer = createAnonymousObserver()
                    end

                    return observer
                end
            "#,
        ));
    }

    #[test]
    fn test_issue_524() {
        let mut ws = VirtualWorkspace::new();
        let mut config = Emmyrc::default();
        config.strict.array_index = true;
        ws.analysis.update_config(config.into());

        ws.def(
            r#"
            ---@type string[]
            local d = {}

            if #d == 2 then
                a = d[1]
                b = d[2]
                c = d[3]
            end

            for i = 1, #d do
                e = d[i]
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let a_expected = ws.ty("string");
        assert_eq!(a, a_expected);
        let b = ws.expr_ty("b");
        let b_expected = ws.ty("string");
        assert_eq!(b, b_expected);
        let c = ws.expr_ty("c");
        let c_expected = ws.ty("string?");
        assert_eq!(c, c_expected);
        let e = ws.expr_ty("e");
        let e_expected = ws.ty("string");
        assert_eq!(e, e_expected);
    }

    #[test]
    fn test_issue_600() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Test2
            ---@field test string[]
            ---@field test2? string
            local a = {}
            if a.test[1] and a.test[1].char(123) then

            end
            "#,
        ));
    }

    #[test]
    fn test_issue_585() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local a --- @type type?

            if type(a) == 'string' then
                local _ = a --- @type type
            end
            "#,
        ));
    }

    #[test]
    fn test_issue_627() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class A
            ---@field type "point"
            ---@field handle number

            ---@class B
            ---@field type "unit"
            ---@field handle string

            ---@param a number
            function testA(a)
            end
            ---@param a string
            function testB(a)
            end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
                ---@param target A | B
                function test(target)
                    if target.type == 'point' then
                        testA(target.handle)
                    end
                    if target.type == 'unit' then
                        testB(target.handle)
                    end
                end
            "#,
        ));
    }

    #[test]
    fn test_issue_622() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Test.A
            ---@field base number
            ---@field add number
            T = {}

            ---@enum Test.op
            Op = {
                base = "base",
                add = "add",
            };
            "#,
        );
        ws.def(
            r#"
            ---@param op Test.op
            ---@param value number
            ---@return boolean
            function T:SetValue(op, value)
                local oldValue = self[op]
                if oldValue == value then
                    return false
                end
                A = oldValue
                return true
            end
            "#,
        );
        let a = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(a), "number");
    }

    #[test]
    fn test_nil_1() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@type number?
            local angle

            if angle ~= nil and angle >= 0 then
                A = angle
            end

            "#,
        );
        let a = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(a), "number");
    }

    #[test]
    fn test_type_narrow() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@generic T: table
            ---@param obj T | function
            ---@return T?
            function bindGC(obj)
                if type(obj) == 'table' then
                    A = obj
                end
            end
            "#,
        );

        // Note: we can't use `ws.ty_expr("A")` to get a true type of `A`
        // because `infer_global_type` will not allow generic variables
        // from `bindGC` to escape into global space.
        let db = &ws.analysis.compilation.db;
        let decl_id = db
            .get_global_index()
            .get_global_decl_ids("A")
            .unwrap()
            .first()
            .unwrap()
            .clone();
        let typ = db
            .get_type_index()
            .get_type_cache(&decl_id.into())
            .unwrap()
            .as_type();

        assert_eq!(ws.humanize_type(typ.clone()), "T");
    }

    #[test]
    fn test_issue_630() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@class A
            ---@field Abc string?
            A = {}
            "#,
        );
        ws.def(
            r#"
            function A:test()
                if not rawget(self, 'Abc') then
                    self.Abc = "a"
                end

                B = self.Abc
                C = self
            end
            "#,
        );
        let a = ws.expr_ty("B");
        assert_eq!(ws.humanize_type(a), "string");
        let c = ws.expr_ty("C");
        assert_eq!(ws.humanize_type(c), "A");
    }

    #[test]
    fn test_error_function() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
                ---@class Result
                ---@field value string?
                Result = {}

                function getValue()
                    ---@type Result?
                    local result

                    if result then
                        error(result.value)
                    end
                end
            "#,
        ));
    }

    #[test]
    fn test_array_flow() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            for i = 1, #_G.arg do
                print(_G.arg[i].char())
            end
            "#,
        ));
    }

    #[test]
    fn test_issue_641() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local b --- @type boolean
            local tar = b and 'a' or 'b'

            if tar == 'a' then
            end

            --- @type 'a'|'b'
            local _ = tar
            "#,
        ));
    }

    #[test]
    fn test_self_1() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Node
            ---@field parent? Node

            ---@class Subject<T>: Node
            ---@field package root? Node
            Subject = {}
            "#,
        );
        ws.def(
            r#"
            function Subject:add()
                if self == self.parent then
                    A = self
                end
            end
            "#,
        );
        let a = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(a), "Node");
    }

    #[test]
    fn test_return_cast_multi_file() {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "test.lua",
            r#"
            local M = {}

            --- @return boolean
            --- @return_cast _obj function
            function M.is_callable(_obj) end

            return M
            "#,
        );
        ws.def(
            r#"
            local test = require("test")

            local obj

            if test.is_callable(obj) then
                o = obj
            end
            "#,
        );
        let a = ws.expr_ty("o");
        let expected = LuaType::Function;
        assert_eq!(a, expected);
    }

    #[test]
    fn test_issue_734() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
local a --- @type string[]

assert(#a >= 1)

--- @type string
_ = a[1]

assert(#a == 1)

--- @type string
_ = a[1]

--- @type string
_2 = a[1]
            "#
        ));
    }

    #[test]
    fn test_return_cast_with_fallback() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Creature

            ---@class Player: Creature

            ---@class Monster: Creature

            ---@return boolean
            ---@return_cast creature Player else Monster
            local function isPlayer(creature)
                return true
            end

            local creature ---@type Creature

            if isPlayer(creature) then
                a = creature
            else
                b = creature
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let a_expected = ws.ty("Player");
        assert_eq!(a, a_expected);

        let b = ws.expr_ty("b");
        let b_expected = ws.ty("Monster");
        assert_eq!(b, b_expected);
    }

    #[test]
    fn test_return_cast_with_fallback_self() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Creature

            ---@class Player: Creature

            ---@class Monster: Creature
            local m = {}

            ---@return boolean
            ---@return_cast self Player else Monster
            function m:isPlayer()
            end

            if m:isPlayer() then
                a = m
            else
                b = m
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let a_expected = ws.ty("Player");
        assert_eq!(a, a_expected);

        let b = ws.expr_ty("b");
        let b_expected = ws.ty("Monster");
        assert_eq!(b, b_expected);
    }

    #[test]
    fn test_return_cast_backward_compatibility() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@return boolean
            ---@return_cast n integer
            local function isInteger(n)
                return true
            end

            local a ---@type integer | string

            if isInteger(a) then
                d = a
            else
                e = a
            end
            "#,
        );

        let d = ws.expr_ty("d");
        let d_expected = ws.ty("integer");
        assert_eq!(d, d_expected);

        // Should still use the original behavior (remove integer from union)
        let e = ws.expr_ty("e");
        let e_expected = ws.ty("string");
        assert_eq!(e, e_expected);
    }

    #[test]
    fn test_issue_868() {
        let mut ws = VirtualWorkspace::new();

        ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local a --- @type string|{foo:boolean, bar:string}

            if a.foo then
                --- @type string
                local _ = a.bar
            end
            "#,
        );
    }

    #[test]
    fn test_or_empty_table_non_table_compatible() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local a --- @type string?

            -- When left type is NOT table-compatible, should not narrow
            E = a or {}
            "#,
        );

        let e_ty = ws.expr_ty("E");
        // string? or {} results in string|table (empty table becomes table)
        assert_eq!(ws.humanize_type(e_ty), "(string|table)");
    }

    #[test]
    fn test_or_empty_table_with_nonempty_class() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            --- @class MyClass
            --- @field x number

            local obj --- @type MyClass?

            E = obj or {}
            "#,
        );

        let e_ty = ws.expr_ty("E");
        assert_eq!(ws.humanize_type(e_ty), "(MyClass|table)");
    }

    #[test]
    fn test_or_empty_table_union_of_tables() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            --- @class A
            --- @field a number

            --- @class B
            --- @field b string

            local obj --- @type (A|B)?

            -- Union of class types is table-compatible
            E = obj or {}
            "#,
        );

        let e_ty = ws.expr_ty("E");
        let type_str = ws.humanize_type_detailed(e_ty);
        assert_eq!(type_str, "(A|B|table)");
    }

    #[test]
    fn test_builtin_gmod_param_name_fallback_infers_common_params() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let code = r#"
            ---@class Player
            ---@field Nick fun(self: Player): string

            ---@class Entity
            ---@field EntIndex fun(self: Entity): integer

            local function enter(ply, ent)
                A = ply
                B = ent
                C = ply:Nick()
                D = ent:EntIndex()
            end
        "#;

        assert!(ws.check_code_for(DiagnosticCode::UndefinedField, code));

        let a = ws.expr_ty("A");
        let b = ws.expr_ty("B");
        let c = ws.expr_ty("C");
        let d = ws.expr_ty("D");

        assert_eq!(ws.humanize_type(a), "Player");
        assert_eq!(ws.humanize_type(b), "Entity");
        assert_eq!(ws.humanize_type(c), "string");
        assert_eq!(ws.humanize_type(d), "integer");
    }

    #[test]
    fn test_gmod_param_name_hint_infers_unannotated_param_type() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc
            .gmod
            .file_param_defaults
            .insert("veh".to_string(), "HintVehicle".to_string());
        ws.update_emmyrc(emmyrc);

        let code = r#"
            ---@class HintVehicle
            ---@field GetFreeSeat fun(self: HintVehicle): Entity

            ---@class Entity

            local function enter(veh)
                local seat = veh:GetFreeSeat()
                A = veh
                B = seat
            end
        "#;

        assert!(ws.check_code_for(DiagnosticCode::UndefinedField, code));

        let a = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(a), "HintVehicle");
        let b = ws.expr_ty("B");
        assert_eq!(ws.humanize_type(b), "Entity");
    }

    #[test]
    fn test_explicit_param_annotation_overrides_gmod_name_fallback() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let code = r#"
            ---@class Player
            ---@class CustomPlayer: Player

            ---@param ply CustomPlayer
            local function enter(ply)
                A = ply
            end
        "#;

        assert!(ws.check_code_for(DiagnosticCode::UndefinedField, code));

        let a = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(a), "CustomPlayer");
    }

    #[test]
    fn test_file_level_param_hint_overrides_inferred_defaults() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc
            .gmod
            .file_param_defaults
            .insert("vehicle".to_string(), "Entity".to_string());
        ws.update_emmyrc(emmyrc);

        let code = r#"
            ---@class Entity

            ---@class base_glide: Entity
            ---@field GetFreeSeat fun(self: base_glide): Entity

            ---@fileparam vehicle base_glide
            local function enter(vehicle)
                local seat = vehicle:GetFreeSeat()
                A = vehicle
                B = seat
            end
        "#;

        assert!(ws.check_code_for(DiagnosticCode::UndefinedField, code));

        let a = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(a), "base_glide");
        let b = ws.expr_ty("B");
        assert_eq!(ws.humanize_type(b), "Entity");
    }

    #[test]
    fn test_fileparam_annotation_works() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let code = r#"
            ---@class Vehicle
            ---@field GetClass fun(self: Vehicle): string

            ---@fileparam vehicle Vehicle
            local function check(vehicle)
                local class = vehicle:GetClass()
                A = vehicle
                B = class
            end
        "#;

        assert!(ws.check_code_for(DiagnosticCode::UndefinedField, code));

        let a = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(a), "Vehicle");
        let b = ws.expr_ty("B");
        assert_eq!(ws.humanize_type(b), "string");
    }

    #[test]
    fn test_explicit_param_overrides_fileparam() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let code = r#"
            ---@class BaseClass
            ---@class OverrideClass: BaseClass

            ---@fileparam v BaseClass

            ---@param v OverrideClass
            local function check(v)
                A = v
            end
        "#;

        assert!(ws.check_code_for(DiagnosticCode::UndefinedField, code));

        let a = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(a), "OverrideClass");
    }

    #[test]
    fn test_gmod_field_guard_narrows_base_entity_to_subtype_members() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let code = r#"
            ---@class Entity

            ---@class base_glide: Entity
            ---@field IsGlideVehicle boolean
            ---@field GetFreeSeat fun(self: base_glide): Entity?

            local function EnterVehicle(ply, veh)
                return true
            end

            ---@class PlayerMeta
            local PlayerMeta = {}

            ---@param vehicle Entity
            function PlayerMeta:EnterVehicle(vehicle)
                if vehicle.IsGlideVehicle and isfunction(vehicle.GetFreeSeat) then
                    local seat = vehicle:GetFreeSeat()
                    if not IsValid(seat) then
                        return
                    end

                    return EnterVehicle(self, seat)
                end

                return EnterVehicle(self, vehicle)
            end
        "#;

        assert!(ws.check_code_for(DiagnosticCode::UndefinedField, code));
    }

    #[test]
    fn test_isfunction_member_guard_narrows_base_entity_to_callable_member_subtypes() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let code = r#"
            ---@class Entity

            ---@class base_glide: Entity
            ---@field GetFreeSeat fun(self: base_glide): Entity?

            ---@param vehicle Entity
            local function enter(vehicle)
                if isfunction(vehicle.GetFreeSeat) then
                    local seat = vehicle:GetFreeSeat()
                    A = seat
                end
            end
        "#;

        assert!(ws.check_code_for(DiagnosticCode::UndefinedField, code));
    }

    #[gtest]
    fn test_getclass_guard_narrows_entity_to_matching_class() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let file_id = ws.def_file(
            "test.lua",
            r#"
            ---@class Entity
            ---@field GetClass fun(self: Entity): string

            ---@class edit_sky: Entity
            ---@field SetTopColor fun(self: edit_sky, v: number)

            ---@class prop_physics: Entity

            ---@param ent Entity
            local function CopySky(ent)
                if ent:GetClass() ~= "edit_sky" then return end
                ent:SetTopColor(1)
                a = ent
            end
        "#,
        );

        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::UndefinedField),
            eq(false)
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "a", 0);
        assert_that!(ws.humanize_type(narrowed), eq("edit_sky"));
    }

    #[gtest]
    fn test_type_name_method_guard_supports_literal_left_comparison() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let file_id = ws.def_file(
            "test.lua",
            r#"
            ---@class Entity
            ---@field TypeName fun(self: Entity): string

            ---@class edit_sky: Entity
            ---@field SetTopColor fun(self: edit_sky, v: number)

            ---@class prop_physics: Entity

            ---@param ent Entity
            local function CopySky(ent)
                if "edit_sky" == ent:TypeName() then
                    ent:SetTopColor(1)
                    a = ent
                end
            end
        "#,
        );

        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::UndefinedField),
            eq(false)
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "a", 0);
        assert_that!(ws.humanize_type(narrowed), eq("edit_sky"));
    }

    #[gtest]
    fn test_type_name_method_guard_does_not_widen_existing_specific_type() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let file_id = ws.def_file(
            "test.lua",
            r#"
            ---@class Entity
            ---@field TypeName fun(self: Entity): string

            ---@class edit_sky: Entity
            ---@field SetTopColor fun(self: edit_sky, v: number)

            ---@param ent edit_sky
            local function KeepSpecific(ent)
                if ent:TypeName() == "Entity" then
                    ent:SetTopColor(1)
                    a = ent
                end
            end
        "#,
        );

        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::UndefinedField),
            eq(false)
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "a", 0);
        assert_that!(ws.humanize_type(narrowed), eq("edit_sky"));
    }

    #[gtest]
    fn test_type_name_method_guard_false_branch_removes_target_class() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        let file_id = ws.def_file(
            "test.lua",
            r#"
            ---@class Entity
            ---@field TypeName fun(self: Entity): string

            ---@class edit_sky: Entity
            ---@field SetTopColor fun(self: edit_sky, v: number)

            ---@class prop_physics: Entity
            ---@field GetMass fun(self: prop_physics): number

            ---@param ent edit_sky|prop_physics
            local function Handle(ent)
                if ent:TypeName() == "edit_sky" then return end
                local mass = ent:GetMass()
                a = mass
                b = ent
            end
        "#,
        );

        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::UndefinedField),
            eq(false)
        );

        let ent_ty = nth_name_expr_type_from_end(&mut ws, file_id, "b", 0);
        assert_that!(ws.humanize_type(ent_ty), eq("prop_physics"));
    }

    #[test]
    fn test_isfunction_simple_var_narrows_nil() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@type function?
            local func = function() end
            if isfunction(func) then
                a = func
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let expected = ws.ty("function");
        assert_eq!(a, expected);
    }

    #[test]
    fn test_local_cached_isvalid_narrows_nil() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local IsValid = IsValid
            ---@type string?
            local maybe = "string"
            if IsValid(maybe) then
                a = maybe
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let expected = ws.ty("string");
        assert_eq!(a, expected);
    }

    #[test]
    fn test_local_cached_isfunction_narrows_nil() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local isfunction = isfunction
            ---@type function?
            local func = function() end
            if isfunction(func) then
                a = func
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let expected = ws.ty("function");
        assert_eq!(a, expected);
    }

    #[test]
    fn test_isstring_simple_var_narrows_nil() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@type string?
            local maybe = "hello"
            if isstring(maybe) then
                a = maybe
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let expected = ws.ty("string");
        assert_eq!(a, expected);
    }

    /// Regression test: calling a method on an UNRELATED local variable in an early-return
    /// guard should NOT corrupt the type of another variable (`ent` in this case).
    /// When `parent:GetIsLocked()` cannot be inferred (e.g. method not in API), the
    /// FieldNotFound error must not propagate and wipe out `ent`'s type.
    #[test]
    fn test_early_return_on_unrelated_method_call_does_not_corrupt_type() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class MyEntity
            ---@field GetParent fun(self): MyEntity
            ---@field GetAbsVelocity fun(self): void

            ---@return MyEntity
            local function Entity(idx) end

            local ent = Entity(1)
            local parent = ent:GetParent()

            -- GetIsLocked is intentionally NOT defined on MyEntity,
            -- simulating a method that is absent from the API definitions.
            if not parent:GetIsLocked() then return end

            -- 'ent' must still be MyEntity here, not unknown
            a = ent
            "#,
        );

        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_eq!(desc, "MyEntity");
    }

    #[gtest]
    fn test_field_narrow_collapses_to_common_base() {
        // Field narrowing should collapse to the base class that defines the field,
        // not list every subtype
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity

            ---@class BaseGlide: Entity
            ---@field IsGlideVehicle boolean

            ---@class GlideCar: BaseGlide

            ---@class GlideAirboat: BaseGlide

            ---@param parent Entity
            function test(parent)
                if not parent.IsGlideVehicle then return end
                a = parent
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        // Should be just BaseGlide (common base), not BaseGlide | GlideCar | GlideAirboat
        assert_eq!(desc, "BaseGlide");
    }

    #[gtest]
    fn test_field_narrow_preserves_multiple_unrelated_bases() {
        // When multiple unrelated types define the same field, both should be kept
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity

            ---@class TypeA: Entity
            ---@field HasFeature boolean

            ---@class TypeB: Entity
            ---@field HasFeature boolean

            ---@param ent Entity
            function test(ent)
                if not ent.HasFeature then return end
                a = ent
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        // Both TypeA and TypeB define HasFeature independently
        assert_that!(desc, contains_substring("TypeA"));
        assert_that!(desc, contains_substring("TypeB"));
    }

    #[gtest]
    fn test_uninitialized_local_branch_merge_produces_nullable() {
        // `local x; if cond then x = value end` should produce `value_type | nil`
        // after the branch, not "unknown"
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@param cond boolean
            local function setup(cond)
                local testFunc
                if cond then
                    testFunc = function(var) print(var) end
                end
                a = testFunc
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        // Should remain nullable (from the uninitialized branch), not "unknown"
        assert_that!(
            desc,
            contains_substring("?"),
            "Expected nullable type: {}",
            desc
        );
        assert_that!(desc, not(eq("unknown")), "Should not be unknown: {}", desc);
    }

    #[gtest]
    fn test_uninitialized_local_table_branch_merge() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@param cond boolean
            local function setup(cond)
                local testTbl
                if cond then
                    testTbl = {}
                end
                a = testTbl
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        // Should remain nullable (from the uninitialized path), not "unknown" or "any"
        assert_that!(
            desc,
            contains_substring("?"),
            "Expected nullable type: {}",
            desc
        );
        assert_that!(desc, not(eq("unknown")), "Should not be unknown: {}", desc);
    }

    /// Same pattern but wrapped in an outer conditional, matching the exact
    /// shape reported in the bug report.
    #[test]
    fn test_early_return_on_unrelated_method_call_nested_does_not_corrupt_type() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class MyEntity
            ---@field GetParent fun(self): MyEntity
            ---@field GetAbsVelocity fun(self): void

            ---@return MyEntity
            local function Entity(idx) end

            local SERVER = true

            if SERVER then
                local ent = Entity(1)
                local parent = ent:GetParent()

                if not parent:GetIsLocked() then return end

                if not ent then return end

                if ent then
                    a = ent
                end
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_eq!(desc, "MyEntity");
    }

    // ================================================================
    // Inference regression tests — based on real production GMod code
    // ================================================================

    #[gtest]
    fn test_type_guard_narrows_to_string() {
        // Regression: `type(s) ~= "string"` guard with early return should narrow s to string
        // Reproduction from Glide.FromJSON
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param s any
            local function test(s)
                if type(s) ~= "string" or s == "" then
                    return {}
                end
                a = s
            end
            "#,
        );
        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_that!(desc, eq("string"));
    }

    #[gtest]
    fn test_type_guard_narrows_simple() {
        // Simple type() guard without or operator
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param s any
            local function test(s)
                if type(s) ~= "string" then return end
                a = s
            end
            "#,
        );
        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_that!(desc, eq("string"));
    }

    #[gtest]
    fn test_if_else_branch_merge_no_nil() {
        // Regression: if-else with both branches assigning should NOT produce nullable type
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            local str
            if true then
                str = "server"
            else
                str = "client"
            end
            a = str
            "#,
        );
        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_that!(
            desc,
            not(contains_substring("nil")),
            "if-else with both branches assigning should not produce nil: {}",
            desc
        );
    }

    #[gtest]
    fn test_if_else_literal_string_accepted_as_string_param() {
        // Regression: "server" | "client" should be assignable to string parameter
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param str string
            local function RequiresString(str) end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local str
            if true then
                str = "server"
            else
                str = "client"
            end
            RequiresString(str)
            "#,
        ));
    }

    #[gtest]
    fn test_server_file_if_server_branch_does_not_keep_client_literal_or_nil() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file(
            "addons/cityrp-vehicle-base/lua/glide/server/events.lua",
            r#"
            ---@param str string
            local function ThisFunctionRequiresString(str) end

            local str
            if SERVER then
                str = "server"
            else
                str = "client"
            end

            ThisFunctionRequiresString(str)
            a = str
            "#,
        );

        let typ = nth_name_expr_type_from_end(&mut ws, file_id, "str", 0);
        let desc = ws.humanize_type(typ.clone());
        assert_that!(desc.as_str(), not(contains_substring("client")));
        assert_that!(desc.as_str(), not(contains_substring("nil")));

        let expected = ws.ty("string");
        assert_that!(ws.check_type(&typ, &expected), eq(true));

        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::ParamTypeMismatch),
            eq(false)
        );
    }

    #[gtest]
    fn test_realistic_glide_mode_branch_merge_has_no_nil() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file(
            "addons/cityrp-vehicle-base/lua/glide/server/events.lua",
            r#"
            ---@class ENT

            ---@param self ENT
            ---@return boolean
            local function HasExternalLighting(self) end

            ---@param mode string
            local function RequiresString(mode) end

            --- Sync Gear to Photon Vehicle.Transmission channel.
            ---@param self ENT
            ---@param name string
            ---@param old number
            ---@param value number
            function OnGearChangePhoton(self, name, old, value)
                if not HasExternalLighting(self) then return end

                local mode
                if value == -1 then
                    mode = "REVERSE"
                elseif value == 0 then
                    mode = "PARK"
                else
                    mode = "DRIVE"
                end

                a = mode
                RequiresString(mode)
            end
            "#,
        );

        let typ = nth_name_expr_type_from_end(&mut ws, file_id, "mode", 0);
        let desc = ws.humanize_type(typ.clone());
        assert_that!(desc.as_str(), not(contains_substring("nil")));

        let expected = ws.ty("string");
        assert_that!(ws.check_type(&typ, &expected), eq(true));

        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::ParamTypeMismatch),
            eq(false)
        );
    }

    #[gtest]
    fn test_shared_file_later_server_guard_keeps_server_only_branch_merge() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file(
            "addons/cityrp-vehicle-base/lua/glide/sh_events.lua",
            r#"
            ---@param str string
            local function ThisFunctionRequiresString(str) end

            local str
            if SERVER then
                str = "server"
            else
                str = "client"
            end

            if SERVER then
                ThisFunctionRequiresString(str)
                a = str
            end
            "#,
        );

        let typ = nth_name_expr_type_from_end(&mut ws, file_id, "str", 0);
        let desc = ws.humanize_type(typ.clone());
        assert_that!(desc.as_str(), not(contains_substring("client")));
        assert_that!(desc.as_str(), not(contains_substring("nil")));

        let expected = ws.ty("string");
        assert_that!(ws.check_type(&typ, &expected), eq(true));

        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::ParamTypeMismatch),
            eq(false)
        );
    }

    #[gtest]
    fn test_method_return_type_not_unknown() {
        // Regression: seat:GetParent() should return Entity, not unknown
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Entity
            ---@field GetParent fun(self: Entity): Entity

            ---@param seat Entity
            function test(seat)
                local parent = seat:GetParent()
                a = parent
            end
            "#,
        );
        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_that!(desc, eq("Entity"));
    }

    #[gtest]
    fn test_uninitialized_local_with_if_true_is_nullable() {
        // `local x; if true then x = val end` should produce `val_type | nil`
        // because flow graph doesn't evaluate constant conditions
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            local testFunc
            if true then
                testFunc = function(var) print(var) end
            end
            a = testFunc
            "#,
        );
        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_that!(
            desc,
            contains_substring("?"),
            "Should remain nullable since else branch has no assignment: {}",
            desc
        );
        assert_that!(desc, not(eq("unknown")), "Should not be unknown: {}", desc);
    }

    #[gtest]
    fn test_isfunction_narrows_uninitialized_local() {
        // After isfunction(testFunc), testFunc should be non-nil (callable without need-check-nil)
        let mut ws = VirtualWorkspace::new();
        // need-check-nil is enabled so the diagnostic runs
        let result = ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param cond boolean
            local function test(cond)
                local testFunc
                if cond then
                    testFunc = function(var) print(var) end
                end
                if isfunction(testFunc) then
                    testFunc("hi")
                end
            end
            "#,
        );
        assert_that!(
            result,
            eq(true),
            "isfunction guard should prevent need-check-nil on testFunc call"
        );
    }

    #[gtest]
    fn test_unresolved_initializer_branch_merge_does_not_fall_back_to_nil() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "addons/cityrp-vehicle-base/lua/glide/server/unresolved_init.lua",
            r#"
            ---@param cond boolean
            local function test(cond)
                local mode = MissingMode()
                if cond then
                    mode = "DRIVE"
                end

                a = mode
            end
            "#,
        );

        let typ = nth_name_expr_type_from_end(&mut ws, file_id, "mode", 0);
        let desc = ws.humanize_type(typ);
        assert_that!(desc.as_str(), not(contains_substring("?")));
        assert_that!(desc.as_str(), not(contains_substring("nil")));
    }

    #[gtest]
    fn test_istable_narrows_uninitialized_local() {
        // After istable(testTbl), testTbl should be non-nil (indexable without need-check-nil)
        let mut ws = VirtualWorkspace::new();
        let result = ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param cond boolean
            local function test(cond)
                local testTbl
                if cond then
                    testTbl = {}
                end
                if istable(testTbl) then
                    local x = testTbl.foo
                end
            end
            "#,
        );
        assert_that!(
            result,
            eq(true),
            "istable guard should prevent need-check-nil on testTbl access"
        );
    }

    #[gtest]
    fn test_type_narrowing_or_with_empty_string_check() {
        // type(s) ~= "string" or s == "" returns early
        // After this, s should be narrowed to string AND s ~= ""
        // At minimum, s should be string (not nil)
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param str string
            local function RequiresString(str) end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            function test(s)
                if type(s) ~= "string" or s == "" then
                    return {}
                end
                RequiresString(s)
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isvalid_then_method_call_chain() {
        // Full production pattern: IsValid check, field narrow, method call
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@class Entity
            ---@field GetParent fun(self: Entity): Entity
            ---@field IsValid fun(self: Entity): boolean
            ---@field GetIsLocked fun(self: Entity): boolean

            ---@class BaseGlide: Entity
            ---@field IsGlideVehicle boolean
            ---@field GetIsLocked fun(self: BaseGlide): boolean

            ---@param seat Entity
            function test(seat)
                local parent = seat:GetParent()
                if not IsValid(parent) then return end
                if not parent.IsGlideVehicle then return end
                a = parent
            end
            "#,
        );
        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_that!(
            desc,
            contains_substring("BaseGlide"),
            "After field narrow, parent should include BaseGlide: {}",
            desc
        );
    }

    #[gtest]
    fn test_isvalid_prevents_nil_on_method_after_field_narrow() {
        // After IsValid(parent) + field narrow, parent:GetIsLocked() should NOT have nil diagnostic
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@class Entity
            ---@field GetParent fun(self: Entity): Entity
            ---@field IsValid fun(self: Entity): boolean

            ---@class BaseGlide: Entity
            ---@field IsGlideVehicle boolean
            ---@field GetIsLocked fun(self: BaseGlide): boolean
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param seat Entity
            function test(seat)
                local parent = seat:GetParent()
                if not IsValid(parent) then return end
                if not parent.IsGlideVehicle then return end
                parent:GetIsLocked()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_param_with_conditional_body_no_nil() {
        // Function parameter used after type() guard should not become nil
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param str string
            local function RequiresString(str) end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            function test(s)
                if type(s) ~= "string" then return end
                RequiresString(s)
            end
            "#,
        ));
    }

    #[gtest]
    fn test_param_with_or_condition_guard() {
        // type(s) ~= "string" or s == "" — param should still be string after guard
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param str string
            local function RequiresString(str) end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            function test(s)
                if type(s) ~= "string" or s == "" then return end
                RequiresString(s)
            end
            "#,
        ));
    }

    // === Comprehensive inference regression tests ===

    #[gtest]
    fn test_type_guard_with_or_condition() {
        // type(s) ~= "string" or s == "" with return {} — s should still be string after
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param str string
            local function RequiresString(str) end
            "#,
        );
        // Does `return {}` in the if body break the narrowing?
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            function test(s)
                if type(s) ~= "string" or s == "" then
                    return {}
                end
                RequiresString(s)
            end
            "#,
        ));
    }

    #[gtest]
    fn test_type_guard_with_or_condition_and_or_return() {
        // Full Glide.FromJSON pattern — check each variant
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param s string
            ---@return table
            function util_JSONToTable(s) return {} end
            "#,
        );
        // Without or in return — should pass
        assert!(
            ws.check_code_for(
                DiagnosticCode::ParamTypeMismatch,
                r#"
            function FromJSON_a(s)
                if type(s) ~= "string" or s == "" then
                    return {}
                end
                return util_JSONToTable(s)
            end
            "#,
            ),
            "util_JSONToTable(s) without or should not trigger ParamTypeMismatch"
        );
    }

    #[gtest]
    fn test_or_in_return_value_does_not_break_narrowing() {
        // Test param type checking with narrowed type - same file
        let mut ws = VirtualWorkspace::new();
        // Define function as global in the SAME file as check
        assert!(
            ws.check_code_for(
                DiagnosticCode::ParamTypeMismatch,
                r#"
            ---@param s string
            ---@return table
            function util_JSONToTable(s) return {} end

            function test_a(s)
                if type(s) ~= "string" then return end
                util_JSONToTable(s)
            end
            "#,
            ),
            "same file: narrowed param should match"
        );
    }

    #[gtest]
    fn test_or_in_return_value_inline() {
        // Test param type checking with narrowed type - separate file
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param s string
            ---@return table
            function util_JSONToTable(s) return {} end
            "#,
        );
        assert!(
            ws.check_code_for(
                DiagnosticCode::ParamTypeMismatch,
                r#"
            function test_b(s)
                if type(s) ~= "string" then return end
                util_JSONToTable(s)
            end
            "#,
            ),
            "separate file: narrowed param should match"
        );
    }

    #[gtest]
    fn test_param_guard_with_global_function() {
        // Check if RequiresString as GLOBAL (not local) still works
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param str string
            function RequiresStringGlobal(str) end
            "#,
        );
        assert!(
            ws.check_code_for(
                DiagnosticCode::ParamTypeMismatch,
                r#"
            function test_c(s)
                if type(s) ~= "string" then return end
                RequiresStringGlobal(s)
            end
            "#,
            ),
            "global function: narrowed param should match"
        );
    }

    #[gtest]
    fn test_param_any_to_string_no_guard() {
        // Does passing an untyped param to string param trigger diagnostic?
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param str string
            function RequiresStringGlobal(str) end
            "#,
        );
        let result = ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            function test(s)
                RequiresStringGlobal(s)
            end
            "#,
        );
        assert!(result, "untyped param should be accepted without guard");
    }

    #[gtest]
    fn test_param_annotated_string() {
        // Does passing an annotated string param work?
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param str string
            function RequiresStringGlobal(str) end
            "#,
        );
        assert!(
            ws.check_code_for(
                DiagnosticCode::ParamTypeMismatch,
                r#"
            ---@param s string
            function test(s)
                RequiresStringGlobal(s)
            end
            "#,
            ),
            "annotated string param should match"
        );
    }

    #[gtest]
    fn test_param_annotated_string_with_guard() {
        // Annotated string + guard - does the guard change the type?
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param str string
            function RequiresStringGlobal(str) end
            "#,
        );
        assert!(
            ws.check_code_for(
                DiagnosticCode::ParamTypeMismatch,
                r#"
            ---@param s string
            function test(s)
                if type(s) ~= "string" then return end
                RequiresStringGlobal(s)
            end
            "#,
            ),
            "annotated string + guard should still match"
        );
    }

    #[gtest]
    fn test_param_annotated_nullable_with_guard() {
        // Annotated string? + guard narrows to string
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param str string
            function RequiresStringGlobal(str) end
            "#,
        );
        assert!(
            ws.check_code_for(
                DiagnosticCode::ParamTypeMismatch,
                r#"
            ---@param s string?
            function test(s)
                if type(s) ~= "string" then return end
                RequiresStringGlobal(s)
            end
            "#,
            ),
            "string? narrowed to string should match"
        );
    }

    #[gtest]
    fn test_or_in_condition_and_return_value() {
        // Both or in condition and return — the full Glide.FromJSON pattern
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param s string
            ---@return table
            function util_JSONToTable(s) return {} end
            "#,
        );
        // Variant B: or in condition + or in return
        assert!(
            ws.check_code_for(
                DiagnosticCode::ParamTypeMismatch,
                r#"
            function test_b(s)
                if type(s) ~= "string" or s == "" then
                    return {}
                end
                return util_JSONToTable(s) or {}
            end
            "#,
            ),
            "compound guard + or return should work"
        );
    }

    #[gtest]
    fn test_literal_string_accepted_as_string_param() {
        // A variable assigned a literal string should be accepted as `string` param
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param str string
            local function RequiresString(str) end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            local function test()
                local str
                if true then
                    str = "server"
                else
                    str = "client"
                end
                RequiresString(str)
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isvalid_early_return_narrows() {
        // `if not IsValid(x) then return end` should narrow x to non-nil
        let mut ws = VirtualWorkspace::new();
        let library_root = ws.virtual_url_generator.new_path("__test_library_isvalid");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("isvalid.lua")).unwrap();
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
            ---@param x any
            ---@return boolean
            function _G.IsValid(x) end
            "#
                .to_string(),
            ),
        );
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@type string?
            local maybe = "hello"
            if not IsValid(maybe) then return end
            maybe:reverse()
            "#,
        ));
    }

    #[gtest]
    fn test_isstring_guard_narrows() {
        // isstring(x) should narrow to remove nil
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@type string?
            local s = "hello"
            if isstring(s) then
                s:lower()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isnumber_guard_narrows() {
        // isnumber(x) should narrow to remove nil
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param cond boolean
            local function test(cond)
                ---@type number?
                local n = 42
                if isnumber(n) then
                    local x = n + 1
                end
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isbool_guard_narrows() {
        // isbool(x) should narrow to remove nil
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@type boolean?
            local b = true
            if isbool(b) then
                local x = not b
            end
            "#,
        ));
    }

    #[gtest]
    fn test_type_guard_equals_string() {
        // type(x) == "string" positive branch narrows to string
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param str string
            local function RequiresString(str) end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param x any
            local function test(x)
                if type(x) == "string" then
                    RequiresString(x)
                end
            end
            "#,
        ));
    }

    #[gtest]
    fn test_type_guard_not_equals_with_early_return() {
        // type(x) ~= "number" with early return narrows x to number
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param n number
            local function RequiresNumber(n) end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param x any
            local function test(x)
                if type(x) ~= "number" then return end
                RequiresNumber(x)
            end
            "#,
        ));
    }

    #[gtest]
    fn test_if_else_both_branches_assign_no_nil() {
        // When both if/else branches assign, the variable should NOT be nil
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            local function setup(cond)
                local val
                if cond then
                    val = 42
                else
                    val = 0
                end
                a = val
            end
            "#,
        );
        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_that!(
            desc,
            not(contains_substring("nil")),
            "Both branches assign, should not be nil: {}",
            desc
        );
    }

    #[gtest]
    fn test_if_only_then_branch_assigns_is_nullable() {
        // When only the then branch assigns, the variable should be nil-able
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            local function setup(cond)
                local val
                if cond then
                    val = 42
                end
                a = val
            end
            "#,
        );
        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_that!(
            desc,
            contains_substring("?"),
            "Only one branch assigns, should remain nullable: {}",
            desc
        );
    }

    #[gtest]
    fn test_isfunction_then_call_no_diagnostic() {
        // Common GMod pattern: guard with isfunction before calling
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local function test()
                local callback
                if true then
                    callback = function() end
                end
                if isfunction(callback) then
                    callback()
                end
            end
            "#,
        ));
    }

    #[gtest]
    fn test_istable_then_access_no_diagnostic() {
        // Common pattern: guard with istable before accessing
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local function test()
                local data
                if true then
                    data = { x = 1 }
                end
                if istable(data) then
                    local x = data.x
                end
            end
            "#,
        ));
    }

    #[gtest]
    fn test_local_isvalid_cache_pattern() {
        // GMod pattern: `local IsValid = IsValid` (caching global as local)
        let mut ws = VirtualWorkspace::new();
        let library_root = ws.virtual_url_generator.new_path("__test_library_isvalid");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("isvalid.lua")).unwrap();
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
            ---@param x any
            ---@return boolean
            function _G.IsValid(x) end
            "#
                .to_string(),
            ),
        );
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local IsValid = IsValid
            ---@type string?
            local maybe = "hello"
            if IsValid(maybe) then
                maybe:reverse()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_renamed_isvalid_alias_still_narrows() {
        let mut ws = VirtualWorkspace::new();
        let library_root = ws
            .virtual_url_generator
            .new_path("__test_library_isvalid_alias");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("isvalid.lua")).unwrap();
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
            ---@param x any
            ---@return boolean
            function _G.IsValid(x) end
            "#
                .to_string(),
            ),
        );
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local iv = IsValid
            ---@type string?
            local maybe = "hello"
            if iv(maybe) then
                maybe:reverse()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_renamed_isfunction_alias_still_narrows() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local is_fn = isfunction
            ---@type function?
            local maybe = function() end
            if is_fn(maybe) then
                maybe()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_shadowed_local_isvalid_alias_does_not_narrow() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local function IsValid(_) return true end
            local iv = IsValid
            ---@type string?
            local maybe = "hello"
            if iv(maybe) then
                maybe:reverse()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_shadowed_local_isfunction_alias_does_not_narrow() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local function isfunction(_) return true end
            local is_fn = isfunction
            ---@type string?
            local maybe = "hello"
            if is_fn(maybe) then
                maybe:reverse()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_reassigned_isvalid_alias_does_not_narrow() {
        let mut ws = VirtualWorkspace::new();
        let library_root = ws
            .virtual_url_generator
            .new_path("__test_library_isvalid_reassigned");
        ws.analysis.add_library_workspace(library_root.clone());
        let library_uri =
            lsp_types::Uri::parse_from_file_path(&library_root.join("isvalid.lua")).unwrap();
        ws.analysis.update_file_by_uri(
            &library_uri,
            Some(
                r#"
            ---@param x any
            ---@return boolean
            function _G.IsValid(x) end
            "#
                .to_string(),
            ),
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local iv = IsValid
            iv = function(_) return true end
            ---@type string?
            local maybe = "hello"
            if iv(maybe) then
                maybe:reverse()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_shadowed_local_isvalid_does_not_narrow() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local IsValid = function(_) return true end
            ---@type string?
            local maybe = "hello"
            if IsValid(maybe) then
                maybe:reverse()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_field_collapse_keeps_surviving_overrides_visible() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@class Entity

            ---@class BaseGlide: Entity
            ---@field IsGlideVehicle boolean

            ---@class GoodGlide: BaseGlide

            ---@class BrokenGlide: BaseGlide
            ---@field IsGlideVehicle false

            ---@param parent Entity
            function test(parent)
                if not parent.IsGlideVehicle then return end
                a = parent
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_that!(desc, contains_substring("GoodGlide"));
    }

    #[gtest]
    fn test_shadowed_local_isfunction_does_not_narrow() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local isfunction = function(_) return true end
            ---@type string?
            local maybe = "hello"
            if isfunction(maybe) then
                maybe:reverse()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_user_defined_global_isvalid_does_not_narrow() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            function IsValid(_) return true end
            ---@type string?
            local maybe = "hello"
            if IsValid(maybe) then
                maybe:reverse()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_nested_type_guards_compound() {
        // Multiple type guards in sequence should all narrow
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param s string
            local function RequiresString(s) end
            ---@param n number
            local function RequiresNumber(n) end
            "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param x any
            ---@param y any
            local function test(x, y)
                if type(x) ~= "string" then return end
                if type(y) ~= "number" then return end
                RequiresString(x)
                RequiresNumber(y)
            end
            "#,
        ));
    }

    #[gtest]
    fn test_method_return_type_resolved() {
        // Method calls should resolve to the correct return type
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Entity
            ---@field GetParent fun(self: Entity): Entity

            ---@param ent Entity
            local function test(ent)
                local parent = ent:GetParent()
                a = parent
            end
            "#,
        );
        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_that!(
            desc,
            eq("Entity"),
            "GetParent should return Entity, got: {}",
            desc
        );
    }

    #[gtest]
    fn test_field_narrow_selects_definer_not_all_subtypes() {
        // Field truthiness narrowing should select the type that DEFINES the field,
        // not list every subtype that inherits it
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Animal
            ---@class Dog: Animal
            ---@field CanBark boolean
            ---@class Poodle: Dog
            ---@class Labrador: Dog
            ---@class Cat: Animal

            ---@param x Animal
            local function test(x)
                if not x.CanBark then return end
                a = x
            end
            "#,
        );
        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        // Should narrow to Dog (which defines CanBark), not Dog|Poodle|Labrador
        assert_that!(
            desc,
            eq("Dog"),
            "Field narrow should select definer only: {}",
            desc
        );
    }

    #[gtest]
    fn test_if_elseif_else_all_assignments_do_not_leave_nil() {
        // Real-world shape: if / elseif / else all assign a string value.
        // This must not produce a nullable type at callsite.
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param mode string
            local function RequiresString(mode) end
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param value integer
            local function test(value)
                local mode
                if value == -1 then
                    mode = "REVERSE"
                elseif value == 0 then
                    mode = "PARK"
                else
                    mode = "DRIVE"
                end

                RequiresString(mode)
            end
            "#,
        ));
    }

    #[gtest]
    fn test_realistic_registry_lookup_keeps_value_type_for_followup_field_access() {
        let mut ws = VirtualWorkspace::new();

        ws.def_file(
            "addons/cityrp-vehicle-base/lua/glide/sh_registry.lua",
            r#"
            ---@class WeaponClass
            ---@field Base string

            Glide = Glide or {}

            ---@type table<string, WeaponClass>
            Glide.WeaponRegistry = {}
            "#,
        );

        let file_id = ws.def_file(
            "addons/cityrp-vehicle-base/lua/glide/server/weapon_inheritance.lua",
            r#"
            local function RefreshInheritance(className)
                if className == "base" then return end

                local class = Glide.WeaponRegistry[className]
                local baseClassName = class.Base

                a = class
                b = baseClassName
            end
            "#,
        );

        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::NeedCheckNil),
            eq(false)
        );

        let class_type = ws.expr_ty("a");
        let weapon_class = ws.ty("WeaponClass");
        assert_that!(ws.check_type(&class_type, &weapon_class), eq(true));

        let base_type = ws.expr_ty("b");
        let string_type = ws.ty("string");
        assert_that!(ws.check_type(&base_type, &string_type), eq(true));
    }

    #[gtest]
    fn test_realistic_scripted_class_field_narrow_keeps_only_base_glide() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        set_gmod_enabled(&mut ws);

        ws.def_files(vec![
            (
                "addons/cityrp-vehicle-base/lua/includes/entity_defs.lua",
                r#"
                ---@class Entity
                ---@field GetParent fun(self: Entity): Entity
                ---@field NetworkVar fun(self: Entity, valueType: string, name: string)

                ---@class ENTITY: Entity
                local ENTITY = {}

                ---@class ENT: ENTITY
                local ENT = {}

                ---@param x any
                ---@return boolean
                function IsValid(x) end
                "#,
            ),
            (
                "addons/cityrp-vehicle-base/lua/entities/base_glide/shared.lua",
                r#"
                ENT.Type = "anim"
                ENT.Base = "base_anim"
                ENT.IsGlideVehicle = true

                function ENT:SetupDataTables()
                    self:NetworkVar("Bool", "IsLocked")
                end
                "#,
            ),
        ]);

        let file_id = ws.def_file(
            "addons/cityrp-vehicle-base/lua/glide/server/events.lua",
            r#"
            ---@param seat Entity
            local function test(seat)
                local parent = seat:GetParent()
                if not IsValid(parent) then return end
                if not parent.IsGlideVehicle then return end

                a = parent

                if not parent:GetIsLocked() then return end
            end
            "#,
        );

        let narrowed = ws.expr_ty("a");
        let desc = ws.humanize_type(narrowed);
        assert_that!(desc.as_str(), eq("base_glide"));

        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::NeedCheckNil),
            eq(false)
        );
    }

    #[gtest]
    fn test_field_narrow_prefers_most_specific_definer_over_parent_union() {
        // Repro shape from GMod hierarchy (Entity <- ENT <- base_glide):
        // after `if not parent.IsGlideVehicle then return end`, parent should
        // narrow to base_glide only, not `base_glide|ENT`.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@class Entity
            ---@class ENT: Entity
            ---@field GetParent fun(self: ENT): ENT

            ---@class base_glide: ENT
            ---@field IsGlideVehicle boolean
            ---@field GetIsLocked fun(self: base_glide): boolean

            ---@param seat ENT
            local function test(seat)
                local parent = seat:GetParent()
                if not parent.IsGlideVehicle then return end
                a = parent
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_that!(
            desc,
            eq("base_glide"),
            "narrowing should keep only most specific definer: {}",
            desc
        );
    }

    #[gtest]
    fn test_isvalid_plus_field_narrow_keeps_method_non_nil_in_ent_hierarchy() {
        // Ensure IsValid nil-removal survives additional field narrowing and
        // does not regress into need-check-nil for method calls.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def(
            r#"
            ---@class Entity
            ---@class ENT: Entity
            ---@field GetParent fun(self: ENT): ENT

            ---@class base_glide: ENT
            ---@field IsGlideVehicle boolean
            ---@field GetIsLocked fun(self: base_glide): boolean
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param x any
            ---@return boolean
            function IsValid(x) end

            ---@param seat ENT
            local function test(seat)
                local parent = seat:GetParent()
                if not IsValid(parent) then return end
                if not parent.IsGlideVehicle then return end
                if not parent:GetIsLocked() then return end
            end
            "#,
        ));
    }

    #[gtest]
    fn test_table_index_read_from_typed_registry_is_not_hard_nil() {
        // Regression guard: reading from a typed table by string key should
        // not collapse the local value to `nil`.
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class WeaponClass
            ---@field Base string

            ---@class GlideNamespace
            ---@field WeaponRegistry table<string, WeaponClass>
            Glide = {}

            ---@type table<string, WeaponClass>
            Glide.WeaponRegistry = {}

            ---@param className string
            local function RefreshInheritance(className)
                local class = Glide.WeaponRegistry[className]
                a = class
            end
            "#,
        );

        let typ = ws.expr_ty("a");
        let desc = ws.humanize_type(typ);
        assert_that!(
            desc,
            not(eq("nil")),
            "table index read collapsed to nil: {}",
            desc
        );
    }
}
