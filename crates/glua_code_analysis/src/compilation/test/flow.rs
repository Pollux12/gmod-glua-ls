#[cfg(test)]
mod test {
    use std::collections::HashSet;

    use crate::{DiagnosticCode, Emmyrc, LuaSemanticDeclId, LuaType, VirtualWorkspace};
    use glua_parser::{LuaAstNode, LuaNameExpr};
    use googletest::prelude::*;
    use lsp_types::{NumberOrString, Uri};
    use tokio_util::sync::CancellationToken;

    fn set_gmod_enabled(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
    }

    fn def_isvalid_guard(ws: &mut VirtualWorkspace) {
        ws.def(
            r#"
            ---@class Entity
            ---@class NULL : Entity
            "#,
        );
        ws.def(
            r#"
            ---@param value any
            ---@return TypeGuard<any>
            ---@return_cast value -NULL
            ---@[valid_guard]
            function IsValid(value) end
            "#,
        );
    }

    fn define_incremental_alias_guard_workspace(
        ws: &mut VirtualWorkspace,
        consumer: String,
    ) -> (Uri, crate::FileId) {
        set_gmod_enabled(ws);
        let guard_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/incremental_alias_guard.lua");
        let consumer_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/incremental_alias_consumer.lua");
        ws.analysis.update_files_by_uri_sorted(vec![
            (
                ws.virtual_url_generator
                    .new_uri("lua/includes/incremental_alias_types.lua"),
                Some(
                    r#"
                    ---@class Entity
                    ---@class NULL: Entity
                    ---@class Player: Entity
                    ---@param value any
                    ---@return TypeGuard<any>
                    ---@return_cast value -NULL
                    function IsValid(value) end
                    ---@return boolean
                    ---@return_cast self Player
                    function Entity:IsPlayer() end
                    "#
                    .to_string(),
                ),
            ),
            (consumer_uri.clone(), Some(consumer)),
        ]);
        let consumer_file_id = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_id(&consumer_uri)
            .expect("incremental alias consumer file id");
        (guard_uri, consumer_file_id)
    }

    fn define_cross_file_predicate_workspace(
        ws: &mut VirtualWorkspace,
        predicate: &str,
    ) -> (Uri, crate::FileId) {
        set_gmod_enabled(ws);
        ws.def_gmod_call_arg_builtins();
        let types_uri = ws
            .virtual_url_generator
            .new_uri("lua/includes/predicate_types.lua");
        let guards_uri = ws
            .virtual_url_generator
            .new_uri("lua/includes/predicate_guards.lua");
        let predicate_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/predicate_incremental.lua");
        let consumer_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/z_consumer_incremental.lua");
        ws.analysis.update_files_by_uri_sorted(vec![
            (
                types_uri,
                Some(
                    r#"
                    ---@class Entity
                    ---@class NULL: Entity
                    ---@class Player: Entity
                    "#
                    .to_string(),
                ),
            ),
            (
                guards_uri,
                Some(
                    r#"
                    ---@param value any
                    ---@return TypeGuard<any>
                    ---@return_cast value -NULL
                    function IsValid(value) end

                    ---@return boolean
                    ---@return_cast self Player
                    function Entity:IsPlayer() end
                    "#
                    .to_string(),
                ),
            ),
            (predicate_uri.clone(), Some(predicate.to_string())),
            (
                consumer_uri.clone(),
                Some(
                    r#"
                    include("predicate_incremental.lua")

                    ---@return Entity
                    local function findEntity() end

                    local function findPlayer()
                        local candidate = findEntity()
                        return IsPlayer(candidate) and candidate or false
                    end

                    local narrowed = findPlayer()
                    print(narrowed)
                    "#
                    .to_string(),
                ),
            ),
        ]);
        let consumer_file_id = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_id(&consumer_uri)
            .expect("consumer file id");
        (predicate_uri, consumer_file_id)
    }

    fn define_assignment_guard_workspace(
        ws: &mut VirtualWorkspace,
        guard_source: &str,
    ) -> (Uri, Uri, crate::FileId) {
        set_gmod_enabled(ws);
        let guard_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/assignment_guard.lua");
        let consumer_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/assignment_guard_consumer.lua");
        ws.analysis.update_files_by_uri_sorted(vec![
            (
                ws.virtual_url_generator
                    .new_uri("lua/includes/assignment_guard_types.lua"),
                Some(
                    r#"
                    ---@class Entity
                    ---@class NULL: Entity
                    ---@class Player: Entity
                    ---@class NPC: Entity
                    ---@param value any
                    ---@return TypeGuard<any>
                    ---@return_cast value -NULL
                    function IsValid(value) end
                    ---@return boolean
                    ---@return_cast self Player
                    function Entity:IsPlayer() end
                    ---@return boolean
                    ---@return_cast self NPC
                    function Entity:IsNPC() end
                    "#
                    .to_string(),
                ),
            ),
            (guard_uri.clone(), Some(guard_source.to_string())),
            (
                consumer_uri.clone(),
                Some(
                    r#"
                    ---@type Entity
                    local ent
                    if Predicates.IsPlayer(ent) then
                        local narrowed = ent
                        print(narrowed)
                    end
                    "#
                    .to_string(),
                ),
            ),
        ]);
        let consumer_file_id = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_id(&consumer_uri)
            .expect("assignment guard consumer file id");
        (guard_uri, consumer_uri, consumer_file_id)
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
            .display_typ()
            .clone()
    }

    fn nth_name_expr_semantic_decl_from_end(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        name: &str,
        nth_from_end: usize,
    ) -> Option<LuaSemanticDeclId> {
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
            .semantic_decl
    }

    fn nth_name_expr_semantic_decl(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        name: &str,
        nth: usize,
    ) -> Option<LuaSemanticDeclId> {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let root = semantic_model.get_root();
        let name_expr = root
            .clone()
            .descendants::<LuaNameExpr>()
            .filter(|expr| expr.get_name_text().as_deref() == Some(name))
            .nth(nth)
            .expect("expected matching name expression");
        semantic_model
            .get_semantic_info(name_expr.syntax().clone().into())
            .expect("expected semantic info for name expression")
            .semantic_decl
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
    fn gmod_zero_delay_timer_initializes_typed_global_for_later_callbacks() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@class MenuPanel
        MenuPanel = {}
        function MenuPanel:Call(js) end

        ---@type MenuPanel
        pnlMainMenu = nil
        "#,
        );

        let file_id = ws.def(
            r#"
        pnlMainMenu = nil

        timer = {}
        function timer.Simple(delay, callback) end

        timer.Simple(0, function()
            pnlMainMenu = MenuPanel
        end)

        function RefreshMenu()
            pnlMainMenu:Call("Update()")
        end
        "#,
        );

        assert!(!file_has_diagnostic(
            &mut ws,
            file_id,
            DiagnosticCode::UncheckedNilAccess
        ));
    }

    #[test]
    fn deferred_closure_keeps_preceding_zero_delay_timer_effect() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@class MenuPanel
        MenuPanel = {}

        ---@type MenuPanel
        pnlMainMenu = nil
        "#,
        );

        let file_id = ws.def(
            r#"
        pnlMainMenu = nil

        timer = {}
        function timer.Simple(delay, callback) end

        timer.Simple(0, function()
            pnlMainMenu = MenuPanel
        end)

        local refresh = function()
            local captured = pnlMainMenu
        end
        "#,
        );

        let captured = nth_name_expr_type_from_end(&mut ws, file_id, "pnlMainMenu", 0);
        assert_eq!(ws.humanize_type(captured), "MenuPanel");
    }

    #[test]
    fn untyped_global_nil_assignment_still_reports_unchecked_access() {
        let mut ws = VirtualWorkspace::new();

        let file_id = ws.def(
            r#"
        pnlMainMenu = nil

        function RefreshMenu()
            pnlMainMenu:Call("Update()")
        end
        "#,
        );

        assert!(file_has_diagnostic(
            &mut ws,
            file_id,
            DiagnosticCode::UncheckedNilAccess
        ));
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
        assert_eq!(ws.humanize_type(e_ty), r#"(("b")|false)?"#);
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
        assert_eq!(ws.humanize_type(e_ty), r#"("a"|false)?"#);
    }

    #[test]
    fn deferred_closure_retains_immutable_local_truthiness_guard() {
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
        assert_eq!(ws.humanize_type(e), "string");
    }

    #[test]
    fn deferred_closure_retains_immutable_parameter_truthiness_guard() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@param value string?
            local function register(value)
                if value then
                    local callback = function()
                        immutable_parameter_capture = value
                    end
                end
            end
            "#,
        );

        let captured = ws.expr_ty("immutable_parameter_capture");
        assert_eq!(ws.humanize_type(captured), "string");
    }

    #[test]
    fn deferred_closure_retains_immutable_local_explicit_nil_guard() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local value ---@type string?
            if value ~= nil then
                local callback = function()
                    explicit_nil_capture = value
                end
            end
            "#,
        );

        let captured = ws.expr_ty("explicit_nil_capture");
        assert_eq!(ws.humanize_type(captured), "string");
    }

    #[test]
    fn deferred_closure_exact_nil_guard_preserves_false_branch() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local value ---@type boolean?
            if value == nil then
                return
            end

            local callback = function()
                exact_nil_capture = value
            end
            "#,
        );

        let captured = ws.expr_ty("exact_nil_capture");
        assert_eq!(captured, LuaType::Boolean);
    }

    #[test]
    fn deferred_closure_exact_false_guard_preserves_false_literal() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local value ---@type boolean?
            if value ~= false then
                return
            end

            local callback = function()
                exact_false_capture = value
            end
            "#,
        );

        let captured = ws.expr_ty("exact_false_capture");
        assert_eq!(captured, LuaType::BooleanConst(false));
    }

    #[test]
    fn false_branch_of_unknown_is_limited_to_runtime_falsy_values() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local value ---@type unknown
            if value then
                return
            end
            unknown_falsy_capture = value
            "#,
        );

        let captured = ws.expr_ty("unknown_falsy_capture");
        let expected = LuaType::from_vec(vec![LuaType::Nil, LuaType::BooleanConst(false)]);
        assert_eq!(captured, expected);
    }

    #[test]
    fn deferred_closure_retains_immutable_unknown_false_branch() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local value ---@type unknown
            if value then
                return
            end

            local callback = function()
                deferred_unknown_falsy_capture = value
            end
            "#,
        );

        let captured = ws.expr_ty("deferred_unknown_falsy_capture");
        let expected = LuaType::from_vec(vec![LuaType::Nil, LuaType::BooleanConst(false)]);
        assert_eq!(captured, expected);
    }

    #[test]
    fn deferred_closure_retains_immutable_local_parenthesized_early_return_guard() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local value ---@type string?
            if not (value) then
                return
            end

            local callback = function()
                early_return_capture = value
            end
            "#,
        );

        let captured = ws.expr_ty("early_return_capture");
        assert_eq!(ws.humanize_type(captured), "string");
    }

    #[test]
    fn deferred_closure_drops_mutable_local_truthiness_guard() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local value ---@type string?
            if value then
                local callback = function()
                    mutable_local_capture = value
                end
            end
            value = nil
            "#,
        );

        let captured = ws.expr_ty("mutable_local_capture");
        assert_eq!(ws.humanize_type(captured), "string?");
    }

    #[test]
    fn deferred_closure_drops_mutable_parameter_truthiness_guard() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@param value string?
            local function register(value)
                if value then
                    local callback = function()
                        mutable_parameter_capture = value
                    end
                end
                value = nil
            end
            "#,
        );

        let captured = ws.expr_ty("mutable_parameter_capture");
        assert_eq!(ws.humanize_type(captured), "string?");
    }

    #[test]
    fn deferred_closure_drops_member_truthiness_guard() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local state ---@type { value: string? }
            if state.value then
                local callback = function()
                    member_capture = state.value
                end
            end
            "#,
        );

        let captured = ws.expr_ty("member_capture");
        assert_eq!(ws.humanize_type(captured), "string?");
    }

    #[test]
    fn deferred_closure_drops_global_truthiness_guard() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@return string?
            local function maybe_string() end

            GLOBAL_VALUE = maybe_string()
            if GLOBAL_VALUE then
                local callback = function()
                    global_capture = GLOBAL_VALUE
                end
            end
            "#,
        );

        let captured = ws.expr_ty("global_capture");
        assert_eq!(ws.humanize_type(captured), "string?");
    }

    #[test]
    fn deferred_closure_retains_immutable_entity_isvalid_guard() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        def_isvalid_guard(&mut ws);

        ws.def(
            r#"
            local entity ---@type Entity|NULL
            if IsValid(entity) then
                local callback = function()
                    isvalid_capture = entity
                end
            end
            "#,
        );

        let captured = ws.expr_ty("isvalid_capture");
        let expected = ws.ty("Entity");
        assert_eq!(captured, expected);
    }

    #[test]
    fn deferred_closure_retains_custom_type_guard() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@param value any
            ---@return TypeGuard<string>
            local function is_string(value) end

            local value ---@type string|number
            if is_string(value) then
                local callback = function()
                    custom_guard_capture = value
                end
            end
            "#,
        );

        let captured = ws.expr_ty("custom_guard_capture");
        let expected = ws.ty("string");
        assert_eq!(captured, expected);
    }

    #[test]
    fn deferred_closure_retains_immutable_return_cast_self_guard() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@param ent Entity
            local function make(ent)
                if ent:IsPlayer() then
                    direct_return_cast_capture = ent
                    local callback = function()
                        closure_return_cast_capture = ent
                    end
                end
            end
            "#,
        );

        assert_eq!(ws.expr_ty("direct_return_cast_capture"), ws.ty("Player"));
        assert_eq!(ws.expr_ty("closure_return_cast_capture"), ws.ty("Player"));
    }

    #[test]
    fn deferred_closure_retains_immutable_named_parameter_return_cast() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity

            ---@param value Entity
            ---@return boolean
            ---@return_cast value Player
            local function isPlayer(value) end

            ---@param ent Entity
            local function make(ent)
                if isPlayer(ent) then
                    local callback = function()
                        named_parameter_return_cast_capture = ent
                    end
                end
            end
            "#,
        );

        assert_eq!(
            ws.expr_ty("named_parameter_return_cast_capture"),
            ws.ty("Player")
        );
    }

    #[test]
    fn deferred_closure_retains_immutable_return_cast_false_branch() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@class NPC: Entity

            ---@return boolean
            ---@return_cast self Player else NPC
            function Entity:IsPlayer() end

            ---@param ent Entity
            local function make(ent)
                if ent:IsPlayer() then
                    return
                end
                local callback = function()
                    return_cast_false_branch_capture = ent
                end
            end
            "#,
        );

        assert_eq!(ws.expr_ty("return_cast_false_branch_capture"), ws.ty("NPC"));
    }

    #[test]
    fn nested_deferred_closures_retain_immutable_return_cast_guard() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@param ent Entity
            local function make(ent)
                if ent:IsPlayer() then
                    local outer = function()
                        local inner = function()
                            nested_return_cast_capture = ent
                        end
                    end
                end
            end
            "#,
        );

        assert_eq!(ws.expr_ty("nested_return_cast_capture"), ws.ty("Player"));
    }

    #[test]
    fn deferred_closure_drops_mutable_return_cast_guard() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@param ent Entity
            ---@param replacement Entity
            local function make(ent, replacement)
                if ent:IsPlayer() then
                    local callback = function()
                        mutable_return_cast_capture = ent
                    end
                end
                ent = replacement
            end
            "#,
        );

        assert_eq!(ws.expr_ty("mutable_return_cast_capture"), ws.ty("Entity"));
    }

    #[test]
    fn deferred_closure_drops_mutable_custom_type_guard() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@param value any
            ---@return TypeGuard<string>
            local function is_string(value) end

            ---@param replacement string|number
            local function make(replacement)
                local value ---@type string|number
                if is_string(value) then
                    local callback = function()
                        mutable_custom_guard_capture = value
                    end
                end
                value = replacement
            end
            "#,
        );

        assert_eq!(
            ws.expr_ty("mutable_custom_guard_capture"),
            ws.ty("string|number")
        );
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
    fn deferred_closure_keeps_preceding_tag_cast() {
        let mut ws = VirtualWorkspace::new();

        let file_id = ws.def(
            r#"
        ---@type string|number
        local value = 1
        ---@cast value string

        local callback = function()
            local captured = value
        end
        "#,
        );

        let captured = nth_name_expr_type_from_end(&mut ws, file_id, "value", 0);
        assert_eq!(ws.humanize_type(captured), "string");
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
    fn test_call_cast_preserves_existing_multiple_inheritance_subtype() {
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

        let source_type = LuaType::Def(crate::LuaTypeDeclId::global("My3"));
        assert_eq!(ws.expr_ty("a"), source_type);
        assert_eq!(ws.expr_ty("b"), source_type);
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
    fn test_gmod_param_name_hint_yields_to_unread_call_site_arg() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let code = r#"
            ---@class Entity
            ---@field GetPos fun(self: Entity): Vector

            ---@class Vector

            local config = { spots = { } }

            local function place(ent)
                B = ent
                for _, v in pairs(ent) do
                    A = v
                end
            end

            place(config.spots)
        "#;

        assert!(ws.check_code_for(DiagnosticCode::UndefinedField, code));

        let param_type = ws.expr_ty("B");
        let param = ws.humanize_type(param_type);
        assert_ne!(
            param, "Entity",
            "a name guess must not beat a call site that fills the param with a table"
        );

        let element_type = ws.expr_ty("A");
        let element = ws.humanize_type(element_type);
        assert!(
            !element.contains("fun("),
            "pairs over that param must not enumerate the guessed class's members: {element}"
        );
    }

    #[test]
    fn test_gmod_func_param_name_hint_infers_function_type() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let code = r#"
            local function run(func)
                A = func
            end
        "#;

        assert!(ws.check_code_for(DiagnosticCode::UndefinedGlobal, code));

        let a = ws.expr_ty("A");
        assert_eq!(ws.humanize_type(a), "function");
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
    fn test_nil_guard_reassignment_join_keeps_non_nil() {
        let mut ws = VirtualWorkspace::new();

        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetText fun(self: Panel): string

            ---@return Panel?
            local function GetRow() end

            ---@return Panel
            local function AddRow() end

            local function use()
                local x = GetRow()
                if x == nil then
                    x = AddRow()
                end
                local narrowed = x
                print(narrowed)
            end
            "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Panel");
    }

    #[gtest]
    fn test_isvalid_guard_reassignment_join_keeps_non_nil() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        def_isvalid_guard(&mut ws);

        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetText fun(self: Panel): string

            ---@return Panel?
            local function GetRow() end

            ---@return Panel
            local function AddRow() end

            local function use()
                local x = GetRow()
                if not IsValid(x) then
                    x = AddRow()
                end
                local narrowed = x
                print(narrowed)
            end
            "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Panel");
    }

    #[gtest]
    fn test_isvalid_guard_reassignment_join_does_not_need_check_nil() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        def_isvalid_guard(&mut ws);

        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetText fun(self: Panel): string

            ---@return Panel?
            local function GetRow() end

            ---@return Panel
            local function AddRow() end

            local function use()
                local x = GetRow()
                if not IsValid(x) then
                    x = AddRow()
                end
                x:GetText()
            end
            "#,
        );

        assert!(!file_has_diagnostic(
            &mut ws,
            file_id,
            DiagnosticCode::NeedCheckNil
        ));
    }

    #[gtest]
    fn test_unannotated_predicate_wrapper_narrows_member_expression_on_true_branch() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        def_isvalid_guard(&mut ws);

        let file_id = ws.def(
            r#"
            ---@class Player: Entity
            ---@field IsFrozen fun(self: Player): boolean

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@class TraceResult
            ---@field Entity Entity?

            function IsPlayer(ent)
                return IsValid(ent) and ent:IsPlayer()
            end

            ---@param tr TraceResult
            local function useTrace(tr)
                if IsPlayer(tr.Entity) then
                    local ply = tr.Entity
                    ply:IsFrozen()
                else
                    local other = tr.Entity
                    print(other)
                end
            end
            "#,
        );

        let inferred_guards = ws
            .analysis
            .compilation
            .get_db()
            .get_signature_index()
            .iter()
            .filter_map(|(signature_id, _)| {
                ws.analysis
                    .compilation
                    .get_db()
                    .get_signature_index()
                    .inferred_positive_guard(signature_id)
            })
            .map(|guard| {
                (
                    guard.param_idx,
                    ws.humanize_type(guard.narrowed_type.clone()),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(inferred_guards, vec![(0, "Player".to_string())]);

        let positive_type = nth_name_expr_type_from_end(&mut ws, file_id, "ply", 0);
        let negative_type = nth_name_expr_type_from_end(&mut ws, file_id, "other", 0);
        assert_that!(ws.humanize_type(positive_type), eq("Player"));
        assert_eq!(negative_type, ws.ty("Entity|nil"));
        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            eq(false)
        );
    }

    #[gtest]
    fn test_cold_batch_ttt_predicate_guard_publishes_consumer_dependency() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let file_ids = ws.def_files(vec![
            (
                "00_types.lua",
                r#"
                ---@class Entity
                ---@field SetNWInt fun(self: Entity, key: string, value: integer)

                ---@class Player: Entity
                ---@field SteamID64 fun(self: Player): string

                ---@param name string
                ---@return table
                function FindMetaTable(name) end
                "#,
            ),
            (
                "01_guards.lua",
                r#"
                ---@class NULL: Entity

                ---@param value any
                ---@return TypeGuard<any>
                ---@return_cast value -NULL
                function IsValid(value) end

                ---@return boolean
                ---@return_cast self Player
                function Entity:IsPlayer() end
                "#,
            ),
            (
                "02_predicate.lua",
                r#"
                ---@type (definition) Player
                local PLAYER = FindMetaTable("Player")

                function PLAYER:IsActive()
                    return true
                end

                function IsPlayer(ent)
                    return IsValid(ent) and ent:IsPlayer()
                end
                "#,
            ),
            (
                "03_consumer.lua",
                r#"
                ---@return Entity
                local function findEntity() end

                local function findActivePlayer()
                    local ent = findEntity()
                    return IsPlayer(ent) and ent:IsActive() and ent or false
                end

                local target = findActivePlayer()
                if target then
                    target:SetNWInt("state", 1)
                    target:SteamID64()
                end
                "#,
            ),
        ]);
        let file_id = file_ids[3];

        let target_type = nth_name_expr_type_from_end(&mut ws, file_id, "target", 0);
        assert_eq!(target_type, ws.ty("Player"));
        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::UndefinedMethod),
            eq(false)
        );
        assert!(
            ws.analysis
                .compilation
                .get_db()
                .get_signature_index()
                .inferred_guard_consumers_for_files(&HashSet::from([file_ids[2]]))
                .contains(&file_id)
        );
    }

    #[gtest]
    fn test_cross_file_inferred_predicate_guard_addition_reindexes_consumer() {
        let mut ws = VirtualWorkspace::new();
        let (predicate_uri, consumer_file_id) = define_cross_file_predicate_workspace(
            &mut ws,
            "function IsPlayer(ent)\n    return true\nend",
        );
        let narrowed_type = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed_type), "Entity");

        ws.analysis
            .update_file_by_uri(
                &predicate_uri,
                Some(
                    "function IsPlayer(ent)\n    return IsValid(ent) and ent:IsPlayer()\nend"
                        .to_string(),
                ),
            )
            .expect("predicate file id after update");

        let narrowed_type = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed_type), "(Player|false)");
    }

    #[gtest]
    fn test_incremental_inferred_guard_addition_reindexes_immutable_alias_calls() {
        let cases = [
            ("function IsPlayer(ent) {body} end", "IsPlayer", "global"),
            (
                "Predicates = Predicates or {}\nfunction Predicates.IsPlayer(ent) {body} end",
                "Predicates.IsPlayer",
                "namespaced",
            ),
            (
                "function _G.IsPlayer(ent) {body} end",
                "_G.IsPlayer",
                "global_root",
            ),
        ];

        for (definition, predicate, case) in cases {
            let mut ws = VirtualWorkspace::new();
            set_gmod_enabled(&mut ws);
            let guard_uri = ws
                .virtual_url_generator
                .new_uri(&format!("lua/autorun/server/{case}_alias_guard.lua"));
            let consumer_uri = ws
                .virtual_url_generator
                .new_uri(&format!("lua/autorun/server/{case}_alias_consumer.lua"));
            ws.analysis.update_files_by_uri_sorted(vec![
                (
                    ws.virtual_url_generator
                        .new_uri(&format!("lua/includes/{case}_alias_types.lua")),
                    Some(
                        r#"
                        ---@class Entity
                        ---@class NULL: Entity
                        ---@class Player: Entity
                        ---@param value any
                        ---@return TypeGuard<any>
                        ---@return_cast value -NULL
                        function IsValid(value) end
                        ---@return boolean
                        ---@return_cast self Player
                        function Entity:IsPlayer() end
                        "#
                        .to_string(),
                    ),
                ),
                (
                    guard_uri.clone(),
                    Some(definition.replace("{body}", "return true")),
                ),
                (
                    consumer_uri.clone(),
                    Some(format!(
                        "---@type Entity\nlocal ent\nlocal Alias = {predicate}\nlocal Alias2 = Alias\nif Alias2(ent) then\n    local narrowed = ent\n    print(narrowed)\nend"
                    )),
                ),
            ]);
            let consumer_file_id = ws
                .analysis
                .compilation
                .get_db()
                .get_vfs()
                .get_file_id(&consumer_uri)
                .unwrap_or_else(|| panic!("{case} alias consumer file id"));
            let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
            assert_eq!(ws.humanize_type(narrowed), "Entity", "{case} before edit");

            ws.analysis
                .update_file_by_uri(
                    &guard_uri,
                    Some(definition.replace("{body}", "return IsValid(ent) and ent:IsPlayer()")),
                )
                .unwrap_or_else(|| panic!("{case} guard addition"));

            let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
            assert_eq!(ws.humanize_type(narrowed), "Player", "{case} after edit");
            assert_eq!(
                ws.analysis.inferred_guard_propagation_stats.reindexed_files,
                1
            );
            assert_eq!(
                ws.analysis
                    .inferred_guard_propagation_stats
                    .broad_stabilizations,
                0
            );
        }
    }

    #[gtest]
    fn test_new_inferred_guard_file_reindexes_existing_immutable_alias_consumer() {
        let mut ws = VirtualWorkspace::new();
        let (guard_uri, consumer_file_id) = define_incremental_alias_guard_workspace(
            &mut ws,
            "---@type Entity\nlocal ent\nlocal Alias = IsPlayer\nif Alias(ent) then\n    local narrowed = ent\n    print(narrowed)\nend".to_string(),
        );
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Entity");

        ws.analysis
            .update_file_by_uri(
                &guard_uri,
                Some(
                    "function IsPlayer(ent) return IsValid(ent) and ent:IsPlayer() end".to_string(),
                ),
            )
            .expect("new guard source file id");

        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Player");
        assert_eq!(
            ws.analysis.inferred_guard_propagation_stats.reindexed_files,
            1
        );
        assert_eq!(
            ws.analysis
                .inferred_guard_propagation_stats
                .broad_stabilizations,
            0
        );
    }

    #[gtest]
    fn test_new_inferred_guard_file_batch_reindexes_existing_immutable_alias_consumer() {
        let mut ws = VirtualWorkspace::new();
        let (guard_uri, consumer_file_id) = define_incremental_alias_guard_workspace(
            &mut ws,
            "---@type Entity\nlocal ent\nlocal Alias = IsPlayer\nif Alias(ent) then\n    local narrowed = ent\n    print(narrowed)\nend".to_string(),
        );

        ws.analysis.update_files_by_uri(vec![(
            guard_uri,
            Some("function IsPlayer(ent) return IsValid(ent) and ent:IsPlayer() end".to_string()),
        )]);

        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Player");
        assert_eq!(
            ws.analysis.inferred_guard_propagation_stats.reindexed_files,
            1
        );
        assert_eq!(
            ws.analysis
                .inferred_guard_propagation_stats
                .broad_stabilizations,
            0
        );
    }

    #[gtest]
    fn test_new_inferred_guard_reindexes_parenthesized_alias_initializers_and_calls() {
        let cases = [
            (
                "---@type Entity\nlocal ent\nlocal Alias = ((IsPlayer))\nif Alias(ent) then\n    local narrowed = ent\n    print(narrowed)\nend",
                "parenthesized initializer",
            ),
            (
                "---@type Entity\nlocal ent\nlocal Alias = (IsPlayer)\nlocal Alias2 = ((Alias))\nif ((Alias2))(ent) then\n    local narrowed = ent\n    print(narrowed)\nend",
                "parenthesized alias chain and call",
            ),
            (
                "---@type Entity\nlocal ent\nif ((IsPlayer))(ent) then\n    local narrowed = ent\n    print(narrowed)\nend",
                "parenthesized direct call",
            ),
        ];

        for (consumer, case) in cases {
            let mut ws = VirtualWorkspace::new();
            let (guard_uri, consumer_file_id) =
                define_incremental_alias_guard_workspace(&mut ws, consumer.to_string());

            ws.analysis
                .update_file_by_uri(
                    &guard_uri,
                    Some(
                        "function IsPlayer(ent) return IsValid(ent) and ent:IsPlayer() end"
                            .to_string(),
                    ),
                )
                .unwrap_or_else(|| panic!("{case} guard source file id"));

            let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
            assert_eq!(ws.humanize_type(narrowed), "Player", "{case}");
            assert_eq!(
                ws.analysis
                    .inferred_guard_propagation_stats
                    .broad_stabilizations,
                0,
                "{case}"
            );
        }
    }

    #[gtest]
    fn test_new_inferred_guard_reindexes_alias_chain_longer_than_eight_declarations() {
        let mut aliases = "local Alias0 = IsPlayer\n".to_string();
        for index in 1..12 {
            aliases.push_str(&format!("local Alias{index} = Alias{}\n", index - 1));
        }
        let consumer = format!(
            "---@type Entity\nlocal ent\n{aliases}if Alias11(ent) then\n    local narrowed = ent\n    print(narrowed)\nend"
        );
        let mut ws = VirtualWorkspace::new();
        let (guard_uri, consumer_file_id) =
            define_incremental_alias_guard_workspace(&mut ws, consumer);

        ws.analysis
            .update_file_by_uri(
                &guard_uri,
                Some(
                    "function IsPlayer(ent) return IsValid(ent) and ent:IsPlayer() end".to_string(),
                ),
            )
            .expect("deep alias guard source file id");

        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Player");
        assert_eq!(
            ws.analysis.inferred_guard_propagation_stats.reindexed_files,
            1
        );
    }

    #[gtest]
    fn test_new_inferred_guard_alias_cycle_terminates_without_narrowing() {
        let mut ws = VirtualWorkspace::new();
        let (guard_uri, consumer_file_id) = define_incremental_alias_guard_workspace(
            &mut ws,
            "---@type Entity\nlocal ent\nlocal Alias = IsPlayer\nlocal Alias2 = Alias\nAlias = Alias2\nAlias2 = Alias\nif Alias2(ent) then\n    local narrowed = ent\n    print(narrowed)\nend".to_string(),
        );

        ws.analysis
            .update_file_by_uri(
                &guard_uri,
                Some(
                    "function IsPlayer(ent) return IsValid(ent) and ent:IsPlayer() end".to_string(),
                ),
            )
            .expect("cyclic alias guard source file id");

        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Entity");
        assert_eq!(
            ws.analysis.inferred_guard_propagation_stats.reindexed_files,
            0
        );
    }

    #[gtest]
    fn test_new_inferred_guard_does_not_reindex_mutable_alias_consumer() {
        let mut ws = VirtualWorkspace::new();
        let (guard_uri, consumer_file_id) = define_incremental_alias_guard_workspace(
            &mut ws,
            "---@type Entity\nlocal ent\nlocal Alias = IsPlayer\nAlias = function() return true end\nif Alias(ent) then\n    local narrowed = ent\n    print(narrowed)\nend".to_string(),
        );

        ws.analysis
            .update_file_by_uri(
                &guard_uri,
                Some(
                    "function IsPlayer(ent) return IsValid(ent) and ent:IsPlayer() end".to_string(),
                ),
            )
            .expect("mutable alias guard source file id");

        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Entity");
        assert_eq!(
            ws.analysis.inferred_guard_propagation_stats.reindexed_files,
            0
        );
    }

    #[gtest]
    fn test_explicit_global_root_inferred_guard_addition_reindexes_all_global_consumers() {
        let definitions = [
            ("function _G.IsPlayer(ent) {body} end", "dot_g_function"),
            ("function _ENV.IsPlayer(ent) {body} end", "dot_env_function"),
            (
                "_G[\"IsPlayer\"] = function(ent) {body} end",
                "indexed_g_assignment",
            ),
            (
                "_ENV[\"IsPlayer\"] = function(ent) {body} end",
                "indexed_env_assignment",
            ),
        ];
        let consumers = [
            "IsPlayer(ent)",
            "_G.IsPlayer(ent)",
            "_ENV.IsPlayer(ent)",
            "_G[\"IsPlayer\"](ent)",
            "_ENV[\"IsPlayer\"](ent)",
        ];

        for (definition, case) in definitions {
            let mut ws = VirtualWorkspace::new();
            set_gmod_enabled(&mut ws);
            let guard_uri = ws
                .virtual_url_generator
                .new_uri(&format!("lua/autorun/server/{case}_guard.lua"));
            let mut files = vec![
                (
                    ws.virtual_url_generator
                        .new_uri(&format!("lua/includes/{case}_types.lua")),
                    Some(
                        r#"
                        ---@class Entity
                        ---@class NULL: Entity
                        ---@class Player: Entity
                        ---@param value any
                        ---@return TypeGuard<any>
                        ---@return_cast value -NULL
                        function IsValid(value) end
                        ---@return boolean
                        ---@return_cast self Player
                        function Entity:IsPlayer() end
                        "#
                        .to_string(),
                    ),
                ),
                (
                    guard_uri.clone(),
                    Some(definition.replace("{body}", "return true")),
                ),
            ];
            let mut consumer_uris = Vec::new();
            for (idx, call) in consumers.iter().enumerate() {
                let uri = ws
                    .virtual_url_generator
                    .new_uri(&format!("lua/autorun/server/{case}_consumer_{idx}.lua"));
                files.push((
                    uri.clone(),
                    Some(format!(
                        "---@type Entity\nlocal ent\nif {call} then\n    local narrowed = ent\n    print(narrowed)\nend"
                    )),
                ));
                consumer_uris.push(uri);
            }
            ws.analysis.update_files_by_uri_sorted(files);

            for consumer_uri in &consumer_uris {
                let consumer_file_id = ws
                    .analysis
                    .compilation
                    .get_db()
                    .get_vfs()
                    .get_file_id(consumer_uri)
                    .unwrap_or_else(|| panic!("{case} consumer file id"));
                let narrowed =
                    nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
                assert_eq!(
                    ws.humanize_type(narrowed),
                    "Entity",
                    "{case}: {consumer_uri:?} before guard addition"
                );
            }

            ws.analysis
                .update_file_by_uri(
                    &guard_uri,
                    Some(definition.replace("{body}", "return IsValid(ent) and ent:IsPlayer()")),
                )
                .unwrap_or_else(|| panic!("{case} guard file id after update"));

            for consumer_uri in consumer_uris {
                let consumer_file_id = ws
                    .analysis
                    .compilation
                    .get_db()
                    .get_vfs()
                    .get_file_id(&consumer_uri)
                    .unwrap_or_else(|| panic!("{case} consumer file id"));
                let narrowed =
                    nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
                assert_eq!(
                    ws.humanize_type(narrowed),
                    "Player",
                    "{case}: {consumer_uri:?} after guard addition"
                );
            }
        }
    }

    #[gtest]
    fn test_incremental_inferred_guard_addition_reaches_three_wrapper_levels() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let uris = (0..6)
            .map(|idx| {
                ws.virtual_url_generator
                    .new_uri(&format!("lua/autorun/server/{idx}_guard_chain.lua"))
            })
            .collect::<Vec<_>>();
        ws.analysis.update_files_by_uri_sorted(vec![
            (
                uris[0].clone(),
                Some(
                    r#"
                    ---@class Entity
                    ---@class NULL: Entity
                    ---@class Player: Entity

                    ---@param value any
                    ---@return TypeGuard<any>
                    ---@return_cast value -NULL
                    function IsValid(value) end

                    ---@return boolean
                    ---@return_cast self Player
                    function Entity:IsPlayer() end
                    "#
                    .to_string(),
                ),
            ),
            (
                uris[1].clone(),
                Some("function GuardA(ent) return true end".to_string()),
            ),
            (
                uris[2].clone(),
                Some("function GuardB(ent) return GuardA(ent) end".to_string()),
            ),
            (
                uris[3].clone(),
                Some("function GuardC(ent) return GuardB(ent) end".to_string()),
            ),
            (
                uris[4].clone(),
                Some("function GuardD(ent) return GuardC(ent) end".to_string()),
            ),
            (
                uris[5].clone(),
                Some(
                    r#"
                    ---@return Entity
                    local function findEntity() end
                    local ent = findEntity()
                    if GuardD(ent) then
                        local narrowed = ent
                        print(narrowed)
                    end
                    "#
                    .to_string(),
                ),
            ),
        ]);
        let consumer_file_id = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_id(&uris[5])
            .expect("consumer file id");
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Entity");

        ws.analysis
            .update_file_by_uri(
                &uris[1],
                Some("function GuardA(ent) return IsValid(ent) and ent:IsPlayer() end".to_string()),
            )
            .expect("GuardA file id after update");

        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Player");
    }

    #[gtest]
    fn test_fact_preserving_guard_reindex_keeps_full_incremental_consumer_chain() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let uris = (0..7)
            .map(|idx| {
                ws.virtual_url_generator
                    .new_uri(&format!("lua/autorun/server/preserved_guard_{idx}.lua"))
            })
            .collect::<Vec<_>>();
        let guard_a = |prefix: &str, predicate: &str| {
            format!("{prefix}function GuardA(ent) return IsValid(ent) and ent:{predicate}() end")
        };
        ws.analysis.update_files_by_uri_sorted(vec![
            (
                uris[0].clone(),
                Some(
                    r#"
                    ---@class Entity
                    ---@class NULL: Entity
                    ---@class Player: Entity
                    ---@class NPC: Entity
                    ---@param value any
                    ---@return TypeGuard<any>
                    ---@return_cast value -NULL
                    function IsValid(value) end
                    ---@return boolean
                    ---@return_cast self Player
                    function Entity:IsPlayer() end
                    ---@return boolean
                    ---@return_cast self NPC
                    function Entity:IsNPC() end
                    "#
                    .to_string(),
                ),
            ),
            (uris[1].clone(), Some(guard_a("", "IsPlayer"))),
            (
                uris[2].clone(),
                Some("function GuardB(ent) return GuardA(ent) end".to_string()),
            ),
            (
                uris[3].clone(),
                Some("function GuardC(ent) return GuardB(ent) end".to_string()),
            ),
            (
                uris[4].clone(),
                Some("function GuardD(ent) return GuardC(ent) end".to_string()),
            ),
            (
                uris[5].clone(),
                Some("function GuardE(ent) return GuardD(ent) end".to_string()),
            ),
            (
                uris[6].clone(),
                Some(
                    r#"
                    ---@type Entity
                    local ent
                    if GuardE(ent) then
                        local narrowed = ent
                        print(narrowed)
                    end
                    "#
                    .to_string(),
                ),
            ),
        ]);
        let consumer_file_id = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_id(&uris[6])
            .expect("preserved guard consumer file id");
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Player");

        ws.analysis
            .update_file_by_uri(&uris[1], Some(guard_a("-- unrelated edit\n\n", "IsPlayer")))
            .expect("fact-preserving guard source reindex");
        assert_eq!(
            ws.analysis.inferred_guard_propagation_stats.changed_facts,
            0
        );

        ws.analysis
            .update_file_by_uri(&uris[1], Some(guard_a("-- unrelated edit\n\n", "IsNPC")))
            .expect("guard type change");
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "NPC");
        assert_eq!(ws.analysis.inferred_guard_propagation_stats.frontiers, 5);
        assert_eq!(
            ws.analysis.inferred_guard_propagation_stats.reindexed_files,
            5
        );
        assert_eq!(
            ws.analysis
                .inferred_guard_propagation_stats
                .broad_stabilizations,
            0
        );

        ws.analysis
            .update_file_by_uri(
                &uris[1],
                Some("-- unrelated edit\n\nfunction GuardA(ent) return true end".to_string()),
            )
            .expect("guard removal");
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Entity");
        assert_eq!(ws.analysis.inferred_guard_propagation_stats.frontiers, 5);
        assert_eq!(
            ws.analysis.inferred_guard_propagation_stats.reindexed_files,
            5
        );
        assert_eq!(
            ws.analysis
                .inferred_guard_propagation_stats
                .broad_stabilizations,
            0
        );
    }

    #[gtest]
    fn test_fact_preserving_guard_batch_keeps_reindexed_consumer_dependency_replacement() {
        for equivalent_prefix in ["", "-- shifted guard owner\n\n"] {
            let mut ws = VirtualWorkspace::new();
            set_gmod_enabled(&mut ws);
            let uris = (0..6)
                .map(|idx| {
                    ws.virtual_url_generator
                        .new_uri(&format!("lua/autorun/server/replaced_guard_{idx}.lua"))
                })
                .collect::<Vec<_>>();
            let guard_a = |prefix: &str, predicate: &str| {
                format!(
                    "{prefix}function GuardA(ent) return IsValid(ent) and ent:{predicate}() end"
                )
            };
            ws.analysis.update_files_by_uri_sorted(vec![
                (
                    uris[0].clone(),
                    Some(
                        r#"
                        ---@class Entity
                        ---@class NULL: Entity
                        ---@class Player: Entity
                        ---@class NPC: Entity
                        ---@param value any
                        ---@return TypeGuard<any>
                        ---@return_cast value -NULL
                        function IsValid(value) end
                        ---@return boolean
                        ---@return_cast self Player
                        function Entity:IsPlayer() end
                        ---@return boolean
                        ---@return_cast self NPC
                        function Entity:IsNPC() end
                        "#
                        .to_string(),
                    ),
                ),
                (uris[1].clone(), Some(guard_a("", "IsPlayer"))),
                (
                    uris[2].clone(),
                    Some("function GuardB(ent) return GuardA(ent) end".to_string()),
                ),
                (
                    uris[3].clone(),
                    Some("function GuardC(ent) return GuardB(ent) end".to_string()),
                ),
                (
                    uris[4].clone(),
                    Some("function GuardD(ent) return GuardA(ent) end".to_string()),
                ),
                (
                    uris[5].clone(),
                    Some(
                        r#"
                        ---@type Entity
                        local ent
                        if GuardD(ent) then
                            local narrowed = ent
                            print(narrowed)
                        end
                        "#
                        .to_string(),
                    ),
                ),
            ]);
            let file_ids = uris
                .iter()
                .map(|uri| {
                    ws.analysis
                        .compilation
                        .get_db()
                        .get_vfs()
                        .get_file_id(uri)
                        .expect("guard replacement file id")
                })
                .collect::<Vec<_>>();

            ws.analysis.update_files_by_uri_sorted(vec![
                (
                    uris[1].clone(),
                    Some(guard_a(equivalent_prefix, "IsPlayer")),
                ),
                (
                    uris[4].clone(),
                    Some("function GuardD(ent) return GuardC(ent) end".to_string()),
                ),
            ]);

            let signature_index = ws.analysis.compilation.get_db().get_signature_index();
            assert_eq!(
                signature_index.inferred_guard_consumers_for_files(&HashSet::from([file_ids[1]])),
                HashSet::from([file_ids[2]])
            );
            assert_eq!(
                signature_index.inferred_guard_consumers_for_files(&HashSet::from([file_ids[2]])),
                HashSet::from([file_ids[3]])
            );
            assert_eq!(
                signature_index.inferred_guard_consumers_for_files(&HashSet::from([file_ids[3]])),
                HashSet::from([file_ids[4]])
            );

            ws.analysis
                .update_file_by_uri(&uris[1], Some(guard_a(equivalent_prefix, "IsNPC")))
                .expect("guard type change");

            let wrapper_types = {
                let signature_index = ws.analysis.compilation.get_db().get_signature_index();
                file_ids[2..=4]
                    .iter()
                    .map(|file_id| {
                        signature_index
                            .inferred_guard_facts_for_files(&HashSet::from([*file_id]))
                            .into_values()
                            .next()
                            .expect("wrapper inferred guard")
                            .narrowed_type
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                wrapper_types
                    .into_iter()
                    .map(|ty| ws.humanize_type(ty))
                    .collect::<Vec<_>>(),
                vec!["NPC", "NPC", "NPC"]
            );
            let narrowed = nth_name_expr_type_from_end(&mut ws, file_ids[5], "narrowed", 0);
            assert_eq!(ws.humanize_type(narrowed), "NPC");
            assert_eq!(ws.analysis.inferred_guard_propagation_stats.frontiers, 4);
            assert_eq!(
                ws.analysis.inferred_guard_propagation_stats.reindexed_files,
                4
            );
        }
    }

    #[gtest]
    fn test_dot_assignment_inferred_guard_cold_index_narrows_consumer() {
        let mut ws = VirtualWorkspace::new();
        let (_, _, consumer_file_id) = define_assignment_guard_workspace(
            &mut ws,
            r#"
            Predicates = {}
            Predicates.IsPlayer = function(ent)
                return IsValid(ent) and ent:IsPlayer()
            end
            "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Player");
    }

    #[gtest]
    fn test_static_string_assignment_inferred_guard_cold_index_narrows_consumer() {
        let mut ws = VirtualWorkspace::new();
        let (_, _, consumer_file_id) = define_assignment_guard_workspace(
            &mut ws,
            r#"
            Predicates = {}
            Predicates["IsPlayer"] = function(ent)
                return IsValid(ent) and ent:IsPlayer()
            end
            "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Player");
    }

    #[gtest]
    fn test_assignment_inferred_guard_uses_matching_target_and_signature() {
        let mut ws = VirtualWorkspace::new();
        let (_, _, consumer_file_id) = define_assignment_guard_workspace(
            &mut ws,
            r#"
            Predicates = {}
            local unused
            unused, Predicates.IsPlayer = true, function(ent)
                return IsValid(ent) and ent:IsPlayer()
            end
            "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Player");
    }

    #[gtest]
    fn test_dot_assignment_inferred_guard_incremental_add_change_remove_and_rename() {
        let mut ws = VirtualWorkspace::new();
        let source = |name: &str, body: &str| {
            format!("Predicates = {{}}\nPredicates.{name} = function(ent) {body} end")
        };
        let (guard_uri, _, consumer_file_id) =
            define_assignment_guard_workspace(&mut ws, &source("IsPlayer", "return true"));
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Entity");

        ws.analysis
            .update_file_by_uri(
                &guard_uri,
                Some(source("IsPlayer", "return IsValid(ent) and ent:IsPlayer()")),
            )
            .expect("dot assignment guard addition");
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Player");

        ws.analysis
            .update_file_by_uri(
                &guard_uri,
                Some(source("IsPlayer", "return IsValid(ent) and ent:IsNPC()")),
            )
            .expect("dot assignment guard type change");
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "NPC");

        ws.analysis
            .update_file_by_uri(&guard_uri, Some(source("IsPlayer", "return true")))
            .expect("dot assignment guard removal");
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Entity");

        ws.analysis
            .update_file_by_uri(
                &guard_uri,
                Some(source("IsPerson", "return IsValid(ent) and ent:IsPlayer()")),
            )
            .expect("dot assignment guard rename");
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Entity");
    }

    #[gtest]
    fn test_static_string_assignment_inferred_guard_incremental_move_and_remove() {
        let mut ws = VirtualWorkspace::new();
        let guard_source = r#"
            Predicates = Predicates or {}
            Predicates["IsPlayer"] = function(ent)
                return IsValid(ent) and ent:IsPlayer()
            end
        "#;
        let (old_uri, _, consumer_file_id) =
            define_assignment_guard_workspace(&mut ws, guard_source);
        let new_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/moved_assignment_guard.lua");

        ws.analysis.update_files_by_uri_sorted(vec![
            (old_uri, None),
            (new_uri.clone(), Some(guard_source.to_string())),
        ]);
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Player");

        ws.analysis
            .update_file_by_uri(
                &new_uri,
                Some(
                    "Predicates = Predicates or {}\nPredicates[\"IsPlayer\"] = function(ent) return true end"
                        .to_string(),
                ),
            )
            .expect("moved static-string assignment guard removal");
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Entity");
    }

    #[gtest]
    fn test_computed_assignment_inferred_guard_is_not_published() {
        let mut ws = VirtualWorkspace::new();
        let (_, _, consumer_file_id) = define_assignment_guard_workspace(
            &mut ws,
            r#"
            Predicates = {}
            local key = "IsPlayer"
            Predicates[key] = function(ent)
                return IsValid(ent) and ent:IsPlayer()
            end
            "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Entity");
    }

    #[gtest]
    fn test_reverse_ordered_same_file_guard_chain_stabilizes_without_reindex_frontiers() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let uris = (0..3)
            .map(|idx| {
                ws.virtual_url_generator
                    .new_uri(&format!("lua/autorun/server/reverse_guard_{idx}.lua"))
            })
            .collect::<Vec<_>>();
        ws.analysis.update_files_by_uri_sorted(vec![
            (
                uris[0].clone(),
                Some(
                    r#"
                    ---@class Entity
                    ---@class NULL: Entity
                    ---@class Player: Entity
                    ---@param value any
                    ---@return TypeGuard<any>
                    ---@return_cast value -NULL
                    function IsValid(value) end
                    ---@return boolean
                    ---@return_cast self Player
                    function Entity:IsPlayer() end
                    "#
                    .to_string(),
                ),
            ),
            (
                uris[1].clone(),
                Some(
                    r#"
                    function GuardD(ent) return GuardC(ent) end
                    function GuardC(ent) return GuardB(ent) end
                    function GuardB(ent) return GuardA(ent) end
                    function GuardA(ent) return true end
                    "#
                    .to_string(),
                ),
            ),
            (
                uris[2].clone(),
                Some(
                    r#"
                    ---@type Entity
                    local ent
                    if GuardD(ent) then
                        local narrowed = ent
                        print(narrowed)
                    end
                    "#
                    .to_string(),
                ),
            ),
        ]);

        ws.analysis
            .update_file_by_uri(
                &uris[1],
                Some(
                    r#"
                    function GuardD(ent) return GuardC(ent) end
                    function GuardC(ent) return GuardB(ent) end
                    function GuardB(ent) return GuardA(ent) end
                    function GuardA(ent) return IsValid(ent) and ent:IsPlayer() end
                    "#
                    .to_string(),
                ),
            )
            .expect("guard chain file id");

        let consumer_file_id = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_id(&uris[2])
            .expect("consumer file id");
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Player");
        assert_eq!(ws.analysis.inferred_guard_propagation_stats.frontiers, 1);
        assert_eq!(
            ws.analysis.inferred_guard_propagation_stats.reindexed_files,
            1
        );
        assert_eq!(
            ws.analysis.inferred_guard_propagation_stats.reference_edges,
            1
        );
        assert_eq!(
            ws.analysis.inferred_guard_propagation_stats.changed_facts,
            4
        );
    }

    #[gtest]
    fn test_namespaced_inferred_guard_cold_index_narrows_external_consumer() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_ids = ws.def_files(vec![
            (
                "lua/includes/namespaced_guard_types.lua",
                r#"
                ---@class Entity
                ---@class NULL: Entity
                ---@class Player: Entity
                ---@param value any
                ---@return TypeGuard<any>
                ---@return_cast value -NULL
                function IsValid(value) end
                ---@return boolean
                ---@return_cast self Player
                function Entity:IsPlayer() end
                "#,
            ),
            (
                "lua/autorun/server/namespaced_guard.lua",
                r#"
                Predicates = {}
                function Predicates.IsPlayer(ent)
                    return IsValid(ent) and ent:IsPlayer()
                end
                "#,
            ),
            (
                "lua/autorun/server/namespaced_guard_consumer.lua",
                r#"
                ---@type Entity
                local ent
                if Predicates.IsPlayer(ent) then
                    local narrowed = ent
                    print(narrowed)
                end
                "#,
            ),
        ]);

        let consumer_file_id = file_ids
            .into_iter()
            .find(|file_id| {
                ws.analysis
                    .compilation
                    .get_db()
                    .get_vfs()
                    .get_file_content(file_id)
                    .is_some_and(|content| content.contains("local narrowed"))
            })
            .expect("consumer file id");
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Player");
    }

    #[gtest]
    fn test_namespaced_inferred_guard_incremental_add_remove_and_rename() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let guard_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/namespaced_incremental_guard.lua");
        let consumer_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/namespaced_incremental_consumer.lua");
        let source = |name: &str, body: &str| {
            format!("Predicates = Predicates or {{}}\nfunction Predicates.{name}(ent) {body} end")
        };
        ws.analysis.update_files_by_uri_sorted(vec![
            (
                ws.virtual_url_generator
                    .new_uri("lua/includes/namespaced_incremental_types.lua"),
                Some(
                    r#"
                    ---@class Entity
                    ---@class NULL: Entity
                    ---@class Player: Entity
                    ---@param value any
                    ---@return TypeGuard<any>
                    ---@return_cast value -NULL
                    function IsValid(value) end
                    ---@return boolean
                    ---@return_cast self Player
                    function Entity:IsPlayer() end
                    "#
                    .to_string(),
                ),
            ),
            (guard_uri.clone(), Some(source("IsPlayer", "return true"))),
            (
                consumer_uri.clone(),
                Some(
                    r#"
                    ---@type Entity
                    local ent
                    if Predicates.IsPlayer(ent) then
                        local narrowed = ent
                        print(narrowed)
                    end
                    "#
                    .to_string(),
                ),
            ),
        ]);
        let consumer_file_id = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_id(&consumer_uri)
            .expect("consumer file id");

        ws.analysis
            .update_file_by_uri(
                &guard_uri,
                Some(source("IsPlayer", "return IsValid(ent) and ent:IsPlayer()")),
            )
            .expect("guard addition");
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Player");

        ws.analysis
            .update_file_by_uri(&guard_uri, Some(source("IsPlayer", "return true")))
            .expect("guard removal");
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Entity");

        ws.analysis
            .update_file_by_uri(
                &guard_uri,
                Some(source("IsPerson", "return IsValid(ent) and ent:IsPlayer()")),
            )
            .expect("guard rename");
        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Entity");
    }

    #[gtest]
    fn test_namespaced_inferred_guard_file_move_rebinds_consumer_dependency() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let old_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/moved_guard_old.lua");
        let new_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/moved_guard_new.lua");
        let consumer_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/moved_guard_consumer.lua");
        let guard_source = r#"
            Predicates = Predicates or {}
            function Predicates.IsPlayer(ent)
                return IsValid(ent) and ent:IsPlayer()
            end
        "#;
        ws.analysis.update_files_by_uri_sorted(vec![
            (
                ws.virtual_url_generator
                    .new_uri("lua/includes/moved_guard_types.lua"),
                Some(
                    r#"
                    ---@class Entity
                    ---@class NULL: Entity
                    ---@class Player: Entity
                    ---@param value any
                    ---@return TypeGuard<any>
                    ---@return_cast value -NULL
                    function IsValid(value) end
                    ---@return boolean
                    ---@return_cast self Player
                    function Entity:IsPlayer() end
                    "#
                    .to_string(),
                ),
            ),
            (old_uri.clone(), Some(guard_source.to_string())),
            (
                consumer_uri.clone(),
                Some(
                    r#"
                    ---@type Entity
                    local ent
                    if Predicates.IsPlayer(ent) then
                        local narrowed = ent
                        print(narrowed)
                    end
                    "#
                    .to_string(),
                ),
            ),
        ]);
        let consumer_file_id = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_id(&consumer_uri)
            .expect("consumer file id");

        ws.analysis.update_files_by_uri_sorted(vec![
            (old_uri, None),
            (new_uri.clone(), Some(guard_source.to_string())),
        ]);

        let narrowed = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Player");
        let new_file_id = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_id(&new_uri)
            .expect("moved guard file id");
        assert!(
            ws.analysis
                .compilation
                .get_db()
                .get_signature_index()
                .inferred_guard_consumers_for_files(&HashSet::from([new_file_id]))
                .contains(&consumer_file_id)
        );
    }

    #[gtest]
    fn test_parenthesized_inferred_predicate_guard_publishes_and_narrows_consumer() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field IsFrozen fun(self: Player): boolean
            ---@param value any
            ---@return TypeGuard<Player>
            function IsValid(value) end
            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            function IsPlayer(ent)
                return (IsValid(ent) and ent:IsPlayer())
            end

            ---@class TraceResult
            ---@field Entity Entity?
            ---@param tr TraceResult
            local function useTrace(tr)
                if IsPlayer(tr.Entity) then
                    local narrowed = tr.Entity
                    narrowed:IsFrozen()
                    print(narrowed)
                end
            end
            "#,
        );

        let inferred_guard_count = ws
            .analysis
            .compilation
            .get_db()
            .get_signature_index()
            .iter()
            .filter(|(signature_id, _)| {
                ws.analysis
                    .compilation
                    .get_db()
                    .get_signature_index()
                    .inferred_positive_guard(signature_id)
                    .is_some()
            })
            .count();
        assert_eq!(inferred_guard_count, 1);
        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Player");
    }

    #[gtest]
    fn test_same_name_realm_guards_keep_distinct_incremental_facts() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        ws.def_gmod_call_arg_builtins();
        let types_uri = ws
            .virtual_url_generator
            .new_uri("lua/includes/realm_guard_types.lua");
        let guards_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/realm_guards.lua");
        let server_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/server/realm_guard_consumer.lua");
        let client_uri = ws
            .virtual_url_generator
            .new_uri("lua/autorun/client/realm_guard_consumer.lua");
        let guard_source = |server_return: &str| {
            format!(
                r#"
                if SERVER then
                    function IsPerson(ent)
                        {server_return}
                    end
                end
                if CLIENT then
                    function IsPerson(ent)
                        return IsValid(ent) and ent:IsNPC()
                    end
                end
                "#,
            )
        };
        ws.analysis.update_files_by_uri_sorted(vec![
            (
                types_uri,
                Some(
                    r#"
                    ---@class Entity
                    ---@class NULL: Entity
                    ---@class Player: Entity
                    ---@class NPC: Entity
                    ---@param value any
                    ---@return TypeGuard<any>
                    ---@return_cast value -NULL
                    function IsValid(value) end
                    ---@return boolean
                    ---@return_cast self Player
                    function Entity:IsPlayer() end
                    ---@return boolean
                    ---@return_cast self NPC
                    function Entity:IsNPC() end
                    "#
                    .to_string(),
                ),
            ),
            (guards_uri.clone(), Some(guard_source("return true"))),
            (
                server_uri.clone(),
                Some(
                    r#"
                    ---@type Entity
                    local ent
                    if IsPerson(ent) then
                        local narrowed = ent
                        print(narrowed)
                    end
                    "#
                    .to_string(),
                ),
            ),
            (
                client_uri.clone(),
                Some(
                    r#"
                    ---@type Entity
                    local ent
                    if IsPerson(ent) then
                        local narrowed = ent
                        print(narrowed)
                    end
                    "#
                    .to_string(),
                ),
            ),
        ]);
        let server_file_id = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_id(&server_uri)
            .expect("server consumer file id");
        let client_file_id = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_id(&client_uri)
            .expect("client consumer file id");
        let client_type = nth_name_expr_type_from_end(&mut ws, client_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(client_type), "NPC");

        ws.analysis
            .update_file_by_uri(
                &guards_uri,
                Some(guard_source("return IsValid(ent) and ent:IsPlayer()")),
            )
            .expect("realm guard file id after update");

        let server_type = nth_name_expr_type_from_end(&mut ws, server_file_id, "narrowed", 0);
        let client_type = nth_name_expr_type_from_end(&mut ws, client_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(server_type), "Player");
        assert_eq!(ws.humanize_type(client_type), "NPC");
        assert_eq!(ws.analysis.inferred_guard_propagation_stats.frontiers, 1);
        assert_eq!(
            ws.analysis.inferred_guard_propagation_stats.reindexed_files,
            1
        );
        assert_eq!(
            ws.analysis.inferred_guard_propagation_stats.reference_edges,
            1
        );
    }

    #[gtest]
    fn test_cross_file_inferred_predicate_guard_offset_shift_reindexes_consumer() {
        let mut ws = VirtualWorkspace::new();
        let (predicate_uri, consumer_file_id) = define_cross_file_predicate_workspace(
            &mut ws,
            "function IsPlayer(ent)\n    return IsValid(ent) and ent:IsPlayer()\nend",
        );
        let narrowed_type = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed_type), "(Player|false)");

        ws.analysis
            .update_file_by_uri(
                &predicate_uri,
                Some("\n\nfunction IsPlayer(ent)\n    return true\nend".to_string()),
            )
            .expect("predicate file id after offset shift");

        let narrowed_type = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed_type), "Entity");
    }

    #[gtest]
    fn test_cross_file_inferred_predicate_guard_rename_reindexes_consumer() {
        let mut ws = VirtualWorkspace::new();
        let (predicate_uri, consumer_file_id) = define_cross_file_predicate_workspace(
            &mut ws,
            "function IsPlayer(ent)\n    return IsValid(ent) and ent:IsPlayer()\nend",
        );
        let narrowed_type = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed_type), "(Player|false)");

        ws.analysis
            .update_file_by_uri(
                &predicate_uri,
                Some(
                    "function IsPerson(ent)\n    return IsValid(ent) and ent:IsPlayer()\nend"
                        .to_string(),
                ),
            )
            .expect("predicate file id after rename");

        let narrowed_type = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed_type), "Entity");
    }

    #[gtest]
    fn test_cross_file_inferred_predicate_guard_deletion_reindexes_consumer() {
        let mut ws = VirtualWorkspace::new();
        let (predicate_uri, consumer_file_id) = define_cross_file_predicate_workspace(
            &mut ws,
            "function IsPlayer(ent)\n    return IsValid(ent) and ent:IsPlayer()\nend",
        );
        let narrowed_type = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed_type), "(Player|false)");

        ws.analysis
            .remove_file_by_uri(&predicate_uri)
            .expect("removed predicate file id");

        let narrowed_type = nth_name_expr_type_from_end(&mut ws, consumer_file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed_type), "Entity");
    }

    #[gtest]
    fn test_mutated_predicate_wrapper_parameter_does_not_narrow_caller() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        def_isvalid_guard(&mut ws);

        let file_id = ws.def(
            r#"
            ---@class Player: Entity

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@return Entity?
            local function findEntity() end

            ---@class TraceResult
            ---@field Entity Entity?

            function IsPlayer(ent)
                ent = findEntity()
                return IsValid(ent) and ent:IsPlayer()
            end

            ---@param tr TraceResult
            local function useTrace(tr)
                if IsPlayer(tr.Entity) then
                    local ply = tr.Entity
                    print(ply)
                end
            end
            "#,
        );

        let inferred_guard_count = ws
            .analysis
            .compilation
            .get_db()
            .get_signature_index()
            .iter()
            .filter(|(signature_id, _)| {
                ws.analysis
                    .compilation
                    .get_db()
                    .get_signature_index()
                    .inferred_positive_guard(signature_id)
                    .is_some()
            })
            .count();
        let ply_type = nth_name_expr_type_from_end(&mut ws, file_id, "ply", 0);
        assert_eq!(inferred_guard_count, 0);
        assert_eq!(ply_type, ws.ty("Entity|nil"));
    }

    #[gtest]
    fn test_inferred_predicate_guard_is_removed_after_incremental_edit() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        def_isvalid_guard(&mut ws);
        let uri = ws
            .virtual_url_generator
            .new_uri("inferred_guard_incremental.lua");
        let source = |predicate: &str| {
            format!(
                r#"
                ---@class Player: Entity

                ---@return boolean
                ---@return_cast self Player
                function Entity:IsPlayer() end

                ---@class TraceResult
                ---@field Entity Entity?

                function IsPlayer(ent)
                    {predicate}
                end

                ---@param tr TraceResult
                local function useTrace(tr)
                    if IsPlayer(tr.Entity) then
                        local ply = tr.Entity
                        print(ply)
                    end
                end
                "#,
            )
        };
        let file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(source("return IsValid(ent) and ent:IsPlayer()")))
            .expect("file id");

        let inferred_guard_count = |ws: &VirtualWorkspace| {
            ws.analysis
                .compilation
                .get_db()
                .get_signature_index()
                .iter()
                .filter(|(signature_id, _)| {
                    ws.analysis
                        .compilation
                        .get_db()
                        .get_signature_index()
                        .inferred_positive_guard(signature_id)
                        .is_some()
                })
                .count()
        };
        assert_eq!(inferred_guard_count(&ws), 1);

        let updated_file_id = ws
            .analysis
            .update_file_by_uri(&uri, Some(source("return true")))
            .expect("file id after update");
        assert_eq!(updated_file_id, file_id);
        assert_eq!(inferred_guard_count(&ws), 0);
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
        ws.def_gmod_type_predicates();

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
        def_isvalid_guard(&mut ws);

        ws.def(
            r#"
            ---@return Entity?
            function maybeEntity()
            end

            local IsValid = IsValid
            local maybe = maybeEntity()
            if IsValid(maybe) then
                a = maybe
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let expected = ws.ty("Entity");
        assert_eq!(a, expected);
    }

    #[test]
    fn test_local_cached_isvalid_keeps_unknown_unresolved() {
        let mut ws = VirtualWorkspace::new();
        def_isvalid_guard(&mut ws);

        ws.def(
            r#"
            local IsValid = IsValid

            ---@return unknown
            function getMaybe()
            end

            local maybe = getMaybe()
            if IsValid(maybe) then
                a = maybe
            end
            "#,
        );

        let a = ws.expr_ty("a");
        // The guard proves the value is valid, not what type it is, so the
        // narrowed type stays unresolved instead of widening to `any`.
        assert_eq!(a, LuaType::Unknown);
    }

    #[test]
    fn test_isvalid_unknown_does_not_force_entity_members() {
        let mut ws = VirtualWorkspace::new();
        def_isvalid_guard(&mut ws);

        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@class Panel
            ---@field Dock fun(self: Panel, mode: any)

            ---@return unknown
            function getMaybe()
            end

            local maybe = getMaybe()
            if IsValid(maybe) then
                maybe:Dock(0)
            end
            "#,
        ));
    }

    #[test]
    fn test_reassigned_field_initialized_local_keeps_own_semantic_identity_after_isvalid() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Weapon
            ---@field activeVehicle nil

            ---@type Weapon
            local wep
            local veh = wep.activeVehicle
            if not IsValid(veh) then
                veh = select(1, wep:GetTargetVehicle())
            end
            if not IsValid(veh) then return end
            local narrowed = veh
            "#,
        );

        let declared_veh = nth_name_expr_semantic_decl(&mut ws, file_id, "veh", 0)
            .expect("expected semantic declaration for local veh");
        let later_veh = nth_name_expr_semantic_decl_from_end(&mut ws, file_id, "veh", 0)
            .expect("expected semantic declaration for later veh use");

        assert_that!(
            declared_veh,
            matches_pattern!(LuaSemanticDeclId::LuaDecl(_))
        );
        assert_eq!(later_veh, declared_veh);
    }

    #[test]
    fn test_local_cached_isfunction_narrows_nil() {
        let mut ws = VirtualWorkspace::new();
        ws.def_gmod_type_predicates();

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
        ws.def_gmod_type_predicates();

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
    fn test_multi_return_local_slot_is_not_treated_as_uninitialized() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@return string, number
            local function get_size()
                return "", 1
            end

            local _, height = get_size()
            if true then
                height = height
            end
            a = height
            "#,
        );

        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_that!(desc, eq("number"));
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
        ws.def_gmod_type_predicates();
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
            ---@class Entity
            ---@field GetClass fun(self: Entity): string

            ---@param x any
            ---@return TypeGuard<any>
            function _G.IsValid(x) end
            "#
                .to_string(),
            ),
        );
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@return Entity?
            function maybeEntity() end

            local maybe = maybeEntity()
            if not IsValid(maybe) then return end
            maybe:GetClass()
            "#,
        ));
    }

    #[gtest]
    fn test_isvalid_conjunction_early_return_narrows_each_operand() {
        let mut ws = VirtualWorkspace::new();
        def_isvalid_guard(&mut ws);

        let file_id = ws.def(
            r#"
            ---@class DLabel
            ---@field Update fun(self: DLabel, value: number)

            ---@return DLabel?
            local function maybeLabel() end

            local dclock = maybeLabel()
            local dwires = maybeLabel()

            local function update(val)
                if not (IsValid(dclock) and IsValid(dwires)) then return end

                dclock:Update(val)
                dwires:Update(val)
                _G.afterGuard = dwires
            end
            "#,
        );

        let dwires_after_guard = nth_name_expr_type_from_end(&mut ws, file_id, "dwires", 0);
        let desc = ws.humanize_type(dwires_after_guard.clone());
        assert_that!(desc.as_str(), not(contains_substring("nil")));
        assert_that!(desc.as_str(), not(contains_substring("NULL")));
        let expected = ws.ty("DLabel");
        assert_that!(ws.check_type(&dwires_after_guard, &expected), eq(true));

        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::NeedCheckNil),
            eq(false)
        );
        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::UncheckedNilAccess),
            eq(false)
        );
    }

    #[gtest]
    fn test_generated_isvalid_conjunction_narrows_later_assigned_captured_local() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        set_gmod_enabled(&mut ws);

        ws.def_file(
            "annotations/global.lua",
            r#"
            ---@realm shared
            ---@param object any
            ---@return TypeGuard<any>
            ---@return_cast object -NULL
            ---@[valid_guard]
            function _G.IsValid(object) end
            "#,
        );

        let file_id = ws.def_file(
            "lua/autorun/client/wire_display.lua",
            r#"
            ---@class DLabel
            ---@field Update fun(self: DLabel, value: number)

            ---@return DLabel
            local function createLabel() end

            local dclock = createLabel()
            local dwires

            local callback
            callback = function(val)
                if not (IsValid(dclock) and IsValid(dwires)) then return end

                dwires:Update(val)
                _G.afterGuard = dwires
            end

            dwires = createLabel()
            "#,
        );

        let dwires_after_guard = nth_name_expr_type_from_end(&mut ws, file_id, "dwires", 1);
        let desc = ws.humanize_type(dwires_after_guard.clone());
        assert_that!(desc.as_str(), eq("DLabel"));
        let expected = ws.ty("DLabel");
        assert_that!(ws.check_type(&dwires_after_guard, &expected), eq(true));
        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::NeedCheckNil),
            eq(false)
        );
    }

    #[gtest]
    fn test_unguarded_later_assigned_captured_local_reports_need_check_nil() {
        let mut ws = VirtualWorkspace::new();
        def_isvalid_guard(&mut ws);
        ws.enable_check(DiagnosticCode::NeedCheckNil);

        let file_id = ws.def(
            r#"
            ---@class DLabel
            ---@field Update fun(self: DLabel, value: number)

            ---@return DLabel
            local function createLabel() end

            local dclock = createLabel()
            local dwires

            local callback
            callback = function(val)
                if not IsValid(dclock) then return end
                dwires:Update(val)
            end

            dwires = createLabel()
            "#,
        );

        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::NeedCheckNil),
            eq(true)
        );
        let dwires_at_access = nth_name_expr_type_from_end(&mut ws, file_id, "dwires", 1);
        let desc = ws.humanize_type(dwires_at_access);
        assert_that!(desc.as_str(), eq("DLabel?"));
    }

    #[gtest]
    fn test_isstring_guard_narrows() {
        // isstring(x) should narrow to remove nil
        let mut ws = VirtualWorkspace::new();
        ws.def_gmod_type_predicates();
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
    fn test_gamemode_hook_type_guard_survives_early_return_merge() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        set_gmod_enabled(&mut ws);
        ws.def_gmod_type_predicates();
        ws.def_gmod_call_arg_builtins();
        ws.def_file(
            "lua/includes/gamemode_hook_docs.lua",
            r#"
            ---@class GM
            GM = {}

            ---@hook ChatTextChanged
            ---@param text string|number
            function GM:ChatTextChanged(text) end
            "#,
        );

        let guarded_file = ws.def_file(
            "gamemodes/base/gamemode/init.lua",
            r#"
            hook.Add("ChatTextChanged", "guarded", function(text)
                if not isstring(text) then return end
                text:sub(1)
                hook_guarded_text_one = text
                text:sub(2)
                hook_guarded_text_two = text
            end)

            function GM:ChatTextChanged(text)
                if not isstring(text) then return end
                text:sub(3)
                gamemode_guarded_text = text
            end
            "#,
        );
        assert!(!file_has_diagnostic(
            &mut ws,
            guarded_file,
            DiagnosticCode::UndefinedMethod
        ));
        let string_type = ws.ty("string");
        for name in [
            "hook_guarded_text_one",
            "hook_guarded_text_two",
            "gamemode_guarded_text",
        ] {
            assert_eq!(ws.expr_ty(name), string_type.clone());
        }

        let unguarded_file = ws.def_file(
            "gamemodes/base/gamemode/cl_init.lua",
            r#"
            hook.Add("ChatTextChanged", "unguarded", function(text)
                text:sub(1)
                unguarded_text = text
            end)
            "#,
        );
        assert_eq!(
            nth_name_expr_type_from_end(&mut ws, unguarded_file, "text", 0),
            ws.ty("string|number")
        );
    }

    #[gtest]
    fn test_isnumber_guard_narrows() {
        // isnumber(x) should narrow to remove nil
        let mut ws = VirtualWorkspace::new();
        ws.def_gmod_type_predicates();
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
        ws.def_gmod_type_predicates();
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
    fn test_type_guard_class_name_narrows_true_branch() {
        let mut ws = VirtualWorkspace::new();
        let code = r#"
            ---@class Dog
            local Dog = {}
            function Dog:Bark() end

            ---@class Cat
            local Cat = {}

            ---@param dog Dog
            local function RequiresDog(dog) end

            ---@param x Dog|Cat
            local function test(x)
                if type(x) == "Dog" then
                    x:Bark()
                    RequiresDog(x)
                end
            end
            "#;

        assert!(ws.check_code_for(DiagnosticCode::ParamTypeMismatch, code));
        assert!(ws.check_code_for(DiagnosticCode::UndefinedField, code));
    }

    #[gtest]
    fn test_type_guard_class_name_not_equals_early_return_narrows_afterward() {
        let mut ws = VirtualWorkspace::new();
        let code = r#"
            ---@class Dog
            local Dog = {}
            function Dog:Bark() end

            ---@class Cat
            local Cat = {}

            ---@param dog Dog
            local function RequiresDog(dog) end

            ---@param x Dog|Cat
            local function test(x)
                if type(x) ~= "Dog" then return end
                x:Bark()
                RequiresDog(x)
            end
            "#;

        assert!(ws.check_code_for(DiagnosticCode::ParamTypeMismatch, code));
        assert!(ws.check_code_for(DiagnosticCode::UndefinedField, code));
    }

    #[gtest]
    fn test_type_guard_class_name_parent_does_not_widen_child() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            ---@class Animal
            local Animal = {}

            ---@class Dog: Animal
            local Dog = {}
            function Dog:Bark() end

            ---@param x Dog
            local function test(x)
                if type(x) == "Animal" then
                    x:Bark()
                end
            end
            "#,
        ));
    }

    #[gtest]
    fn test_type_guard_unknown_class_name_does_not_narrow() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Dog
            local Dog = {}

            ---@class Cat
            local Cat = {}

            ---@param dog Dog
            local function RequiresDog(dog) end

            ---@param x Dog|Cat
            local function test(x)
                if type(x) == "Dgo" then
                    RequiresDog(x)
                end
            end
            "#,
        ));
    }

    #[gtest]
    fn test_type_guard_class_name_preserves_missing_field_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            ---@class Dog
            local Dog = {}

            ---@class Cat
            local Cat = {}

            ---@param x Dog|Cat
            local function test(x)
                if type(x) == "Dog" then
                    x:DefinitelyMissing()
                end
            end
            "#,
        ));
    }

    #[gtest]
    fn test_type_guard_class_name_does_not_narrow_incompatible_primitive() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::UndefinedMethod,
            r#"
            ---@class Dog
            local Dog = {}
            function Dog:Bark() end

            ---@param s string
            local function test(s)
                if type(s) == "Dog" then
                    s:Bark()
                end
            end
            "#,
        ));
    }

    #[gtest]
    fn test_type_guard_alias_name_does_not_narrow() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@class Dog
            local Dog = {}
            ---@alias DogAlias Dog

            ---@class Cat
            local Cat = {}

            ---@param dog Dog
            local function RequiresDog(dog) end

            ---@param x Dog|Cat
            local function test(x)
                if type(x) == "DogAlias" then
                    RequiresDog(x)
                end
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
        ws.def_gmod_type_predicates();
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
        ws.def_gmod_type_predicates();
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
            ---@class Entity
            ---@field GetClass fun(self: Entity): string

            ---@param x any
            ---@return TypeGuard<any>
            function _G.IsValid(x) end
            "#
                .to_string(),
            ),
        );
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local IsValid = IsValid
            ---@return Entity?
            function maybeEntity() end

            local maybe = maybeEntity()
            if IsValid(maybe) then
                maybe:GetClass()
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
            ---@class Entity
            ---@field GetClass fun(self: Entity): string

            ---@param x any
            ---@return TypeGuard<any>
            function _G.IsValid(x) end
            "#
                .to_string(),
            ),
        );
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local iv = IsValid
            ---@return Entity?
            function maybeEntity() end

            local maybe = maybeEntity()
            if iv(maybe) then
                maybe:GetClass()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_renamed_isfunction_alias_still_narrows() {
        let mut ws = VirtualWorkspace::new();
        ws.def_gmod_type_predicates();
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
        // BrokenGlide falsy-overrides IsGlideVehicle, so BaseGlide alone is unsafe;
        // surviving truthy subtypes (GoodGlide) must remain visible.
        assert_that!(desc, contains_substring("GoodGlide"));
        assert_that!(desc.contains("BrokenGlide"), eq(false));
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
    fn test_elseif_type_guard_narrows_after_previous_type_guard_false_branch() {
        let mut ws = VirtualWorkspace::new();
        ws.def_gmod_type_predicates();
        ws.def(
            r#"
            ---@param value table
            local function RequiresTable(value) end
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
            ---@param sfmeshdata string|table
            local function test(sfmeshdata)
                if isstring(sfmeshdata) then
                    return
                elseif istable(sfmeshdata) then
                    RequiresTable(sfmeshdata)
                end
            end
            "#,
        ));
    }

    #[gtest]
    fn test_starfall_mesh_convexes_do_not_keep_string_after_elseif_istable_guard() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        set_gmod_enabled(&mut ws);
        ws.def_gmod_type_predicates();

        let file_id = ws.def_file(
            "lua/entities/starfall_prop/init.lua",
            r#"
            SF = {}

            function SF.Throw(msg, level, uncatchable, userdata)
                local level = 1 + (level or 1)
                error(msg, level)
            end

            util = {}

            ---@param str string
            ---@return string
            function util.Compress(str) end

            ---@param compressedString string
            ---@param maxSize? number
            ---@return string|nil
            function util.Decompress(compressedString, maxSize) end

            ---@class Vector

            ---@return Vector
            function Vector(x, y, z) end

            local function streamToMesh(meshdata)
                local meshConvexes = {}

                meshdata = SF.StringStream(util.Decompress(meshdata, 65536))
                local nConvexes = meshdata:readInt32()
                if nConvexes > maxConvexesPerProp then SF.Throw("Exceeded", 2) end
                for iConvex = 1, nConvexes do
                    local nVertices = meshdata:readInt32()
                    if nVertices > maxVerticesPerConvex then SF.Throw("Exceeded", 2) end
                    local convex = {}
                    for iVertex = 1, nVertices do
                        convex[iVertex] = Vector(meshdata:readFloat(), meshdata:readFloat(), meshdata:readFloat())
                    end
                    meshConvexes[iConvex] = convex
                end

                return meshConvexes
            end

            local function meshToStream(meshConvexes)
                local meshdata = SF.StringStream()
                meshdata:writeInt32(#meshConvexes)
                for _, convex in ipairs(meshConvexes) do
                    meshdata:writeInt32(#convex)
                    for _, vertex in ipairs(convex) do
                        meshdata:writeFloat(vertex[1]) meshdata:writeFloat(vertex[2]) meshdata:writeFloat(vertex[3])
                    end
                end
                return util.Compress(meshdata:getString())
            end

            local function checkMesh(ply, meshConvexes)
                if #meshConvexes > maxConvexesPerProp then SF.Throw("Exceeded", 2) end
                if #meshConvexes <= 0 then SF.Throw("Invalid", 2) end

                local totalVertices = 0
                for _, convex in ipairs(meshConvexes) do
                    if #convex > maxVerticesPerConvex then SF.Throw("Exceeded", 2) end
                    if #convex < 4 then SF.Throw("Invalid", 2) end

                    totalVertices = totalVertices + #convex
                    customPropVertexLimit:checkuse(ply, totalVertices)

                    for k, vertex in ipairs(convex) do
                        for i = 1, k - 1 do
                            if convex[i]:DistToSqr(vertex) < mindist then
                                SF.Throw("No two vertices can have a distance less", 2)
                            end
                        end
                    end
                end
            end

            local function createCustomProp(ply, pos, ang, sfmeshdata)
                local meshConvexes
                if isstring(sfmeshdata) then
                    meshConvexes = streamToMesh(sfmeshdata)
                elseif istable(sfmeshdata) then
                    meshConvexes = sfmeshdata
                    sfmeshdata = meshToStream(meshConvexes)
                else
                    SF.Throw("Invalid sfmeshdata", 2)
                end
                if #sfmeshdata > 65536 then
                    SF.Throw("sfmeshdata is too long!", 2)
                end

                checkMesh(ply, meshConvexes)
                SF.NetBurst:use(ply, #sfmeshdata * 8)

                local propent = ents.Create("starfall_prop")
                propent.sf_physmesh = meshConvexes

                propent.sfmeshdata = sfmeshdata
                propent:Spawn()

                local totalVertices = 0
                for k, v in ipairs(meshConvexes) do
                    totalVertices = totalVertices + #v
                end

                return propent
            end
            "#,
        );

        assert!(!file_has_diagnostic(
            &mut ws,
            file_id,
            DiagnosticCode::ParamTypeMismatch,
        ));

        let mesh_convexes_ty = nth_name_expr_type_from_end(&mut ws, file_id, "meshConvexes", 0);
        assert_that!(ws.humanize_type(mesh_convexes_ty), eq("table"));
    }

    #[gtest]
    fn test_never_returning_call_branch_does_not_leave_uninitialized_local_nil() {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def(
            r#"
            ---@return never
            local function Throw() end

            local value
            if condition then
                value = "ok"
            else
                Throw()
            end

            local result = value
            "#,
        );

        let value_ty = nth_name_expr_type_from_end(&mut ws, file_id, "value", 0);
        assert_that!(ws.humanize_type(value_ty), eq("\"ok\""));
    }

    #[gtest]
    fn test_error_wrapper_call_branch_does_not_leave_uninitialized_local_nil() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def(
            r#"
            local function Throw(message)
                error(message, 2)
            end

            local value
            if condition then
                value = "ok"
            else
                Throw("invalid")
            end

            local result = value
            "#,
        );

        let value_ty = nth_name_expr_type_from_end(&mut ws, file_id, "value", 0);
        assert_that!(ws.humanize_type(value_ty), eq("\"ok\""));
    }

    #[gtest]
    fn test_shadowed_global_never_member_call_does_not_mark_branch_unreachable() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            GlobalApi = {}

            ---@return never
            function GlobalApi.Throw() end
            "#,
        );

        let file_id = ws.def(
            r#"
            local GlobalApi = {}
            function GlobalApi.Throw() end

            local value
            if condition then
                value = "ok"
            else
                GlobalApi.Throw()
            end

            local result = value
            "#,
        );

        let value_ty = nth_name_expr_type_from_end(&mut ws, file_id, "value", 0);
        assert_that!(ws.humanize_type(value_ty), eq("\"ok\"?"));
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
    fn test_scripted_tool_name_collision_does_not_pollute_entity_vehicle_table() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        set_gmod_enabled(&mut ws);

        ws.def_files(vec![
            (
                "lua/includes/gmod_defs.lua",
                r#"
                ---@class Vector

                ---@class Entity
                local Entity = {}
                ---@return Entity
                function Entity:GetParent() end
                ---@return string
                function Entity:GetClass() end
                ---@return Vector
                function Entity:GetForward() end
                ---@return Vector
                function Entity:GetRight() end
                ---@return Vector
                function Entity:GetUp() end
                ---@return Vector
                function Entity:GetLocalPos() end
                ---@return number
                function Entity:EntIndex() end
                ---@return Vector
                function Entity:OBBCenter() end
                ---@param pos Vector
                ---@return Vector
                function Entity:LocalToWorld(pos) end

                ---@class ENT: Entity
                ENT = {}

                ---@class Tool
                local Tool = {}
                function Tool:Allowed() end

                ---@class TOOL: Tool
                TOOL = Tool

                ---@param x any
                ---@return boolean
                function IsValid(x) end

                ---@param id number
                ---@return Entity
                function _G.Entity(id) end
                "#,
            ),
            (
                "lua/entities/base_glide/shared.lua",
                r#"
                ENT.Base = "base_anim"
                ENT.IsGlideVehicle = true
                "#,
            ),
            (
                "lua/entities/glide_missile_launcher.lua",
                r#"
                ENT.Base = "base_anim"
                "#,
            ),
            (
                "lua/entities/glide_projectile_launcher.lua",
                r#"
                ENT.Base = "base_anim"
                "#,
            ),
            (
                "lua/weapons/gmod_tool/stools/glide_missile_launcher.lua",
                r#"
                TOOL.Name = "Missile Launcher"

                local function IsGlideMissileLauncher(ent)
                    return IsValid(ent) and ent:GetClass() == "glide_missile_launcher"
                end

                function TOOL:UpdateMissileLauncher(ent)
                    ent:SetReloadDelay(1)
                end

                function TOOL:LeftClick(trace)
                    return IsGlideMissileLauncher(trace.Entity)
                end

                function TOOL:RightClick(trace)
                    local ent = trace.Entity
                    if not IsGlideMissileLauncher(ent) then return false end
                    self:UpdateMissileLauncher(ent)
                    return true
                end
                "#,
            ),
            (
                "lua/weapons/gmod_tool/stools/glide_projectile_launcher.lua",
                r#"
                TOOL.Name = "Projectile Launcher"

                local function IsGlideProjectileLauncher(ent)
                    return IsValid(ent) and ent:GetClass() == "glide_projectile_launcher"
                end

                function TOOL:UpdateProjectileLauncher(ent)
                    ent:SetReloadDelay(1)
                end

                function TOOL:RightClick(trace)
                    local ent = trace.Entity
                    if not IsGlideProjectileLauncher(ent) then return false end
                    self:UpdateProjectileLauncher(ent)
                    return true
                end
                "#,
            ),
        ]);

        let file_id = ws.def_file(
            "lua/glide/client/debugging.lua",
            r#"
            local vehicles = {}

            local entObj = Entity(1)
            local fw = nil
            local rt = nil
            local up = nil
            if IsValid(entObj) then
                if entObj.IsGlideVehicle then
                    vehicles[1] = entObj
                end

                local parent = entObj:GetParent()
                if IsValid(parent) then
                    local up = parent.GetUp and parent:GetUp()
                    if entObj.GetLocalPos and parent.LocalToWorld then
                        local lp = entObj:GetLocalPos()
                        if lp then
                            local axlePos = parent:LocalToWorld(lp)
                        end
                    end
                    fw = parent.GetForward and parent:GetForward() or fw
                    rt = parent.GetRight and parent:GetRight() or rt
                    up = parent.GetUp and parent:GetUp() or up
                    local vid = parent:EntIndex()
                    vehicles[vid] = parent
                    parent_type_snapshot = parent
                end
            end

            for vid, veh in pairs(vehicles) do
                if not IsValid(veh) then
                    goto continue
                end

                local centerWorld = veh:LocalToWorld(veh:OBBCenter())
                veh_type_snapshot = veh

                ::continue::
            end
            "#,
        );

        let veh_type = ws.expr_ty("veh_type_snapshot");
        let veh_desc = ws.humanize_type(veh_type);
        let parent_type = ws.expr_ty("parent_type_snapshot");
        let parent_desc = ws.humanize_type(parent_type);

        assert_that!(
            parent_desc.as_str(),
            eq("Entity"),
            "field guard on an Entity-owned method should not narrow parent to unrelated subclasses: {}",
            parent_desc
        );
        assert_that!(
            veh_desc.as_str(),
            not(contains_substring("glide_missile_launcher")),
            "vehicle table iteration should not include unrelated tool classes: {}",
            veh_desc
        );
        assert_that!(
            veh_desc.as_str(),
            not(contains_substring("glide_projectile_launcher")),
            "vehicle table iteration should not include unrelated tool classes: {}",
            veh_desc
        );

        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::NeedCheckNil),
            eq(false)
        );
    }

    #[gtest]
    fn test_field_narrow_drops_wrong_realm_subclass_in_serverside_scope() {
        // Realm-aware narrow: in server scope, drop EFFECT (client `Foo`)
        // from a `[EFFECT, ENT]` narrow union; keep ENT (server `Foo`).
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "test.lua",
            r#"
            ---@realm server

            ---@class Entity

            ---@class EFFECT : Entity
            EFFECT = {}

            ---@realm client
            function EFFECT:Foo() end

            ---@class ENT : Entity
            ENT = {}

            ---@realm server
            function ENT:Foo() end

            ---@param ent Entity?
            local function test(ent)
                if ent and ent.Foo then
                    a = ent
                end
            end
            "#,
        );

        assert_that!(
            file_has_diagnostic(&mut ws, file_id, DiagnosticCode::GmodRealmMismatchHeuristic),
            eq(false),
            "realm-aware narrowing must not produce a wrong-realm diagnostic"
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "a", 0);
        let desc = ws.humanize_type(narrowed);
        assert_that!(
            desc.contains("EFFECT"),
            eq(false),
            "narrowed type must drop wrong-realm subclass EFFECT in serverside scope: {}",
            desc
        );
    }

    #[gtest]
    fn test_field_narrow_keeps_nullable_table_generic_on_constant_key() {
        // `if t.x then` on `table<string, integer>?` must not narrow `t` to
        // `nil`: the `nil` union arm infers the member as `Never`, which can
        // never be truthy, so it must not survive as field-exists evidence
        // while the table arm (which has no indexed member for `x`) drops out.
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def_file(
            "test.lua",
            r#"
            ---@type table<string, integer>?
            local t
            if t.x then
                a = t
            end
            "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "a", 0);
        let desc = ws.humanize_type(narrowed);
        assert_that!(
            desc.contains("table"),
            eq(true),
            "field-exists narrowing must keep the table arm, not collapse to nil: {}",
            desc
        );
    }

    #[gtest]
    fn test_field_narrow_with_dynamic_key_ignores_unrelated_dynamic_member_realm() {
        // Dynamic index keys produce `ExprType` member keys, which alias every
        // other dynamic access with the same inferred key type. Field-existence
        // narrowing must not collapse `ent` to whichever subtype happens to
        // contain an unrelated dynamic write (`EFFECT[k] = v`), and the
        // realm-aware filter must not drop candidates based on that unrelated
        // write's `---@realm` annotation.
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let file_id = ws.def_file(
            "test.lua",
            r#"
            ---@realm server

            ---@class Entity

            ---@class EFFECT : Entity
            EFFECT = {}

            ---@realm client
            ---@param k string
            function EFFECT.StoreField(k, v)
                EFFECT[k] = v
            end

            ---@class ENT : Entity
            ENT = {}

            ---@realm server
            ---@param k string
            function ENT.StoreField(k, v)
                ENT[k] = v
            end

            ---@param ent Entity?
            ---@param key string
            local function test(ent, key)
                if ent and ent[key] then
                    b = ent
                end
            end
            "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "b", 0);
        let desc = ws.humanize_type(narrowed);
        assert_that!(
            desc.contains("Entity"),
            eq(true),
            "dynamic-key narrowing must keep the declared base type: {}",
            desc
        );
        assert_that!(
            desc.contains("EFFECT") || desc.contains("ENT"),
            eq(false),
            "dynamic-key narrowing must not collapse to a subtype that merely \
             contains an unrelated dynamic write: {}",
            desc
        );
    }

    #[gtest]
    fn test_field_exist_narrow_skips_server_only_base_method_on_client() {
        // Real pattern: GetNWEntity -> Entity|NULL, Vehicle:GetSteering is server-only,
        // Glide ENT (Entity subclass, not Vehicle) defines shared NetworkVar GetSteering.
        // Field-exist must reverse-lookup owners (not fan out every Entity subclass)
        // and collapse to the most generic realm-compatible definer.
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        ws.def_file(
            "annotations/vehicle.lua",
            r#"
            ---@meta
            ---@class Entity
            ---@class Vehicle : Entity
            local Vehicle = {}
            ---@realm server
            function Vehicle:GetSteering() end

            ---@class Player : Entity
            ---@return Vehicle
            function Player:GetVehicle() end
            ---@return Entity|NULL
            function Entity:GetNWEntity(key, fallback) end
            ---@return Entity?
            function Entity:GetParent() end
            "#,
        );
        ws.def_file(
            "lua/entities/base_glide_car/shared.lua",
            r#"
            ---@class base_glide : Entity
            ---@field IsGlideVehicle boolean
            local ENT = {}
            ENT.Type = "anim"
            ENT.IsGlideVehicle = true
            function ENT:GetVisualSteering()
                return 0
            end

            ---@class base_glide_car : base_glide
            local ENT = {}
            ENT.Base = "base_glide"
            function ENT:SetupDataTables()
                self:NetworkVar("Float", "Steering")
            end

            ---@class base_glide_boat : base_glide
            local ENT = {}
            ENT.Base = "base_glide"
            function ENT:SetupDataTables()
                self:NetworkVar("Float", "Steering")
            end
            "#,
        );
        let file_id = ws.def_file(
            "lua/glide/autoload/steering_indicator.lua",
            r#"
            if CLIENT then
                ---@type Player
                local ply
                local veh = ply:GetNWEntity("GlideVehicle")
                if not IsValid(veh) then
                    local maybeSeat = ply:GetVehicle()
                    if IsValid(maybeSeat) then
                        local parent = maybeSeat:GetParent()
                        if IsValid(parent) and parent.IsGlideVehicle then
                            veh = parent
                        elseif maybeSeat.IsGlideVehicle then
                            veh = maybeSeat
                        end
                    end
                end
                if veh.GetVisualSteering then
                    a = veh
                    local _ = veh:GetVisualSteering()
                elseif veh.GetSteering then
                    b = veh
                    local steeringNorm = veh:GetSteering() or 0
                    print(steeringNorm)
                end
            end
            "#,
        );

        let a_ty = nth_name_expr_type_from_end(&mut ws, file_id, "a", 0);
        let a_desc = ws.humanize_type(a_ty);
        assert_that!(
            a_desc.as_str(),
            eq("base_glide"),
            "GetVisualSteering should collapse to most generic definer base_glide, got {a_desc}"
        );

        let b_ty = nth_name_expr_type_from_end(&mut ws, file_id, "b", 0);
        let b_desc = ws.humanize_type(b_ty);
        assert_that!(
            b_desc.contains("base_glide_car") || b_desc.contains("base_glide_boat"),
            eq(true),
            "elseif veh.GetSteering should narrow to realm-compatible NetworkVar owners, got {b_desc}"
        );
        assert_that!(
            b_desc.contains("Vehicle") || b_desc.contains("Entity") || b_desc.contains("NULL"),
            eq(false),
            "must not fan out open Entity/Vehicle bases, got {b_desc}"
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let realm_mismatch = diagnostics.iter().any(|d| {
            d.message.contains("GetSteering")
                && (d.message.contains("Realm mismatch") || d.message.contains("realm"))
        });
        assert_that!(
            realm_mismatch,
            eq(false),
            "realm-compatible narrow should not report GetSteering mismatch: {diagnostics:?}"
        );
    }

    #[gtest]
    fn test_field_narrow_does_not_pick_subclass_when_base_directly_defines_field() {
        // `if x.EndTouch then` on `Entity` must not collapse to `EFFECT`
        // override — the base already defines the field.
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Entity
            ---@field EndTouch fun(self: Entity, entity: Entity)

            ---@class EFFECT : Entity
            ---@field EndTouch fun(self: EFFECT)

            ---@param ent Entity?
            local function test(ent)
                if ent and ent.EndTouch then
                    a = ent
                end
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_that!(
            desc,
            eq("Entity"),
            "narrowing Entity via field-existence must not collapse to subclass override: {}",
            desc
        );
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

    #[gtest]
    fn test_unknown_key_read_from_untyped_registry_table_is_not_hard_nil() {
        // A global registry initialized as an empty table can be populated
        // elsewhere. Indexing it with an unknown runtime key must not prove
        // the result is `nil`.
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            Glide = Glide or {}
            Glide.WeaponRegistry = Glide.WeaponRegistry or {}

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
            "unknown registry key read collapsed to nil: {}",
            desc
        );
    }

    #[gtest]
    fn test_string_key_read_from_untyped_registry_table_is_not_hard_nil() {
        // Same open-registry shape, but with a known broad string key type.
        // This must not fall through the exact-member lookup path into `nil`.
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            Glide = Glide or {}
            Glide.WeaponRegistry = Glide.WeaponRegistry or {}

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
            "string registry key read collapsed to nil: {}",
            desc
        );
    }

    #[gtest]
    fn test_unknown_key_read_from_shaped_table_does_not_fallback_to_any() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            local shaped = {
                known = true,
            }

            ---@param index number
            local function Read(index)
                a = shaped[index]
            end
            "#,
        );

        let typ = ws.expr_ty("a");
        assert_eq!(ws.humanize_type(typ), "nil");
    }

    #[gtest]
    fn test_undefined_global_guard_after_index_truthy_promotes_to_any() {
        // Reading `tmysql.Version` in an `if` condition implies `tmysql` is
        // non-nil/non-false in the truthy branch, so we promote the
        // undefined-global base to `any` rather than keeping it nil/unknown.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "test.lua",
            r#"
            if tmysql.Version then
                a = tmysql
            end
        "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "tmysql", 0);
        assert_eq!(narrowed, LuaType::Any);
    }

    #[gtest]
    fn test_undefined_global_guard_after_truthy_stays_unknown_without_index_evidence() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "test.lua",
            r#"
            if tmysql then
                a = tmysql
            end
        "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "tmysql", 0);
        assert_eq!(narrowed, LuaType::Unknown);
    }

    #[gtest]
    fn test_undefined_global_guard_after_deep_index_truthy_promotes_to_any() {
        // Deep index chains: prefix is itself an IndexExpr.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "test.lua",
            r#"
            if tmysql.Version.Foo then
                a = tmysql
            end
        "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "tmysql", 0);
        assert_eq!(narrowed, LuaType::Any);
    }

    #[gtest]
    fn test_undefined_global_guard_after_index_comparison_promotes_to_any() {
        // Comparison on indexed read (e.g. `tmysql.Version < 4.1`) implies
        // the indexed base is non-nil in the truthy branch, so it promotes to `Any`.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "test.lua",
            r#"
            if tmysql.Version < 4.1 then
                a = tmysql
            end
        "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "tmysql", 0);
        assert_eq!(narrowed, LuaType::Any);
    }

    #[gtest]
    fn test_undefined_global_guard_in_else_after_index_promotes_to_any() {
        // The else-branch of `if tmysql.Version then ... else ... end` is only
        // reached if the index access succeeded (i.e. tmysql was non-nil),
        // so tmysql should narrow and become `Any`.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "test.lua",
            r#"
            if tmysql.Version then
                local _x
            else
                a = tmysql
            end
        "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "tmysql", 0);
        assert_eq!(narrowed, LuaType::Any);
    }

    #[test]
    fn test_unknown_local_istable_guard_is_scoped() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_gmod_type_predicates();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file(
            "test.lua",
            r#"
            local x ---@type unknown
            if istable(x) then
                print(x) -- 1st from end
            end
            print(x) -- 0th from end
            "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "x", 1);
        assert_eq!(ws.humanize_type(narrowed), "table");

        let not_narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "x", 0);
        assert_eq!(ws.humanize_type(not_narrowed), "unknown");
    }

    #[gtest]
    fn test_unknown_local_indexed_guard_promoted_to_any_within_scope() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "test.lua",
            r#"
            local x ---@type unknown
            if x.Version then
                print(x) -- 1st from end
            end
            print(x) -- 0th from end
        "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "x", 1);
        assert_eq!(narrowed, LuaType::Any);

        let not_narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "x", 0);
        assert_eq!(not_narrowed, LuaType::Unknown);
    }

    #[gtest]
    fn test_inferred_unknown_alias_direct_index_guard_preserves_dynamic_member_type() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def_file(
            "test.lua",
            r#"
            ---@class DynamicValue
            local DynamicValue = {}

            ---@return DynamicValue
            local function makeValue() end

            ENT = {}

            function ENT:Init()
                self.values = {}
                self.values.field = makeValue()
            end

            function ENT:Use()
                local alias = self.values
                if alias.field then
                    local guarded = alias.field
                    print(guarded)
                end
            end
        "#,
        );

        let guarded = nth_name_expr_type_from_end(&mut ws, file_id, "guarded", 0);
        assert_eq!(ws.humanize_type(guarded), "DynamicValue");
    }

    #[gtest]
    fn test_inferred_unknown_alias_deep_index_guard_preserves_dynamic_member_type() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def_file(
            "test.lua",
            r#"
            ---@class DynamicValue
            local DynamicValue = {}

            ---@return DynamicValue
            local function makeValue() end

            ENT = {}

            function ENT:Init()
                self.values = {}
                self.values.a = { b = makeValue() }
            end

            function ENT:Use()
                local alias = self.values
                if alias.a.b then
                    local guarded = alias.a.b
                    print(guarded)
                end
            end
        "#,
        );

        let guarded = nth_name_expr_type_from_end(&mut ws, file_id, "guarded", 0);
        assert_eq!(ws.humanize_type(guarded), "DynamicValue");
    }

    #[gtest]
    fn test_inferred_unknown_alias_comparison_guard_preserves_dynamic_member_type() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def_file(
            "test.lua",
            r#"
            ENT = {}

            function ENT:Init()
                self.values = {}
                self.values.field = 1
            end

            function ENT:Use()
                local alias = self.values
                if alias.field < 10 then
                    local guarded = alias.field
                    print(guarded)
                end
            end
        "#,
        );

        let guarded = nth_name_expr_type_from_end(&mut ws, file_id, "guarded", 0);
        assert_eq!(ws.humanize_type(guarded), "1");
    }

    #[gtest]
    fn test_unknown_local_binary_guard_promoted_to_any_within_scope() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let file_id = ws.def_file(
            "test.lua",
            r#"
            local y ---@type unknown
            if y.Version < 4.1 then
                print(y) -- 1st from end
            end
            print(y) -- 0th from end
        "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "y", 1);
        assert_eq!(narrowed, LuaType::Any);

        let not_narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "y", 0);
        assert_eq!(not_narrowed, LuaType::Unknown);
    }

    // ── Table-guard / self-coalescing regression guards ────────────────────

    #[gtest]
    fn test_self_coalescing_table_or_keeps_table_behavior() {
        // `opts = opts or {}` then `opts.foo = "x"` must keep table behavior;
        // opts must NOT collapse to string after the second assignment.
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param opts table|nil
            function foo(opts)
                opts = opts or {}
                opts.foo = "x"
                a = opts
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_that!(
            desc,
            not(contains_substring("string")),
            "table-guard self-coalescing must not leak string into opts: {}",
            desc
        );
    }

    #[gtest]
    fn test_self_coalescing_registry_table_of_over_bare_table_unchanged() {
        // `Glide = Glide or {}` and `Glide.Registry = Glide.Registry or {}`
        // must keep the table-of-over-bare-table preference unchanged.
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            Glide = Glide or {}
            Glide.Registry = Glide.Registry or {}
            a = Glide
            b = Glide.Registry
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let a_desc = ws.humanize_type(a_ty);
        assert_that!(
            a_desc,
            not(contains_substring("string")),
            "Glide self-coalescing must keep table type: {}",
            a_desc
        );

        let b_ty = ws.expr_ty("b");
        let b_desc = ws.humanize_type(b_ty);
        assert_that!(
            b_desc,
            not(contains_substring("string")),
            "Glide.Registry self-coalescing must keep table type: {}",
            b_desc
        );
    }

    /// Regression: a wrong-realm assignment (e.g. inside `if SERVER`) must NOT
    /// kill an inferred default for a client-side use site when used as a
    /// generic string template argument.
    #[gtest]
    fn test_inferred_default_survives_wrong_realm_assignment() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def(
            r#"
            ---@class Panel
            ---@class DPanel: Panel
            ---@class ServerPanel: Panel

            ---@generic T: Panel
            ---@param classname `T`
            ---@return T
            function create_panel(classname)
            end
            "#,
        );

        let file_id = ws.def_file(
            "lua/autorun/client/test.lua",
            r#"
            ---@param panelClass string|nil
            local function create(panelClass)
                panelClass = panelClass or "DPanel"
                if SERVER then
                    panelClass = "ServerPanel"
                end
                result = create_panel(panelClass)
            end
            "#,
        );

        // At the client use site, panelClass's inferred default "DPanel"
        // should survive because the SERVER-only reassignment must be
        // filtered out by realm-aware reachability.
        let ty = nth_name_expr_type_from_end(&mut ws, file_id, "result", 0);
        let desc = ws.humanize_type(ty);
        assert_that!(
            desc,
            contains_substring("DPanel"),
            "inferred default must survive wrong-realm assignment for generic binding, got: {}",
            desc
        );
    }

    /// Regression: a wrong-realm assignment must NOT kill an explicit param
    /// default for a client-side use site when used as a generic string
    /// template argument.
    #[gtest]
    fn test_explicit_default_survives_wrong_realm_assignment() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def(
            r#"
            ---@class Panel
            ---@class DPanel: Panel
            ---@class ServerPanel: Panel

            ---@generic T: Panel
            ---@param classname `T`
            ---@return T
            function create_panel(classname)
            end
            "#,
        );

        let file_id = ws.def_file(
            "lua/autorun/client/test.lua",
            r#"
            ---@param panelClass string="DPanel"
            local function create(panelClass)
                if SERVER then
                    panelClass = "ServerPanel"
                end
                result = create_panel(panelClass)
            end
            "#,
        );

        // Inside the function body, at the client use site, panelClass's
        // explicit default "DPanel" should survive because the SERVER-only
        // reassignment must be filtered out.
        let ty = nth_name_expr_type_from_end(&mut ws, file_id, "result", 0);
        let desc = ws.humanize_type(ty);
        assert_that!(
            desc,
            contains_substring("DPanel"),
            "explicit default must survive wrong-realm assignment for generic binding, got: {}",
            desc
        );
    }

    /// A `while` loop exits only when its condition is false, so the code
    /// after the loop must see the variable narrowed by the negated condition.
    #[gtest]
    fn test_while_not_isvalid_exit_narrows() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        def_isvalid_guard(&mut ws);

        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetText fun(self: Panel): string

            ---@return Panel?
            local function GetRow() end

            local function use()
                local p = GetRow()
                while not IsValid(p) do
                    print(p)
                end
                local narrowed = p
                print(narrowed)
            end
            "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Panel");
    }

    /// Same for a plain `== nil` loop condition.
    #[gtest]
    fn test_while_nil_compare_exit_narrows() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        def_isvalid_guard(&mut ws);

        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetText fun(self: Panel): string

            ---@return Panel?
            local function GetRow() end

            local function use()
                local q = GetRow()
                while q == nil do
                    print(q)
                end
                local narrowed = q
                print(narrowed)
            end
            "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Panel");
    }

    /// The loop body may reassign the variable back to a nullable value; the
    /// exit edge still guarantees exactly what the negated condition says.
    #[gtest]
    fn test_while_exit_narrows_after_body_reassignment() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        def_isvalid_guard(&mut ws);

        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetText fun(self: Panel): string

            ---@return Panel?
            local function GetRow() end

            local function use()
                local p = GetRow()
                while not IsValid(p) do
                    p = GetRow()
                end
                local narrowed = p
                print(narrowed)
            end
            "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "narrowed", 0);
        assert_eq!(ws.humanize_type(narrowed), "Panel");
    }

    /// The retry-loop shape must not report a nil-check diagnostic afterwards.
    #[gtest]
    fn test_while_exit_narrow_does_not_need_check_nil() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        def_isvalid_guard(&mut ws);

        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetText fun(self: Panel): string

            ---@return Panel?
            local function GetRow() end

            local function use()
                local p = GetRow()
                while not IsValid(p) do
                    p = GetRow()
                end
                p:GetText()
            end
            "#,
        );

        assert!(!file_has_diagnostic(
            &mut ws,
            file_id,
            DiagnosticCode::NeedCheckNil
        ));
    }

    /// Control: a `while true do ... break end` loop exits without proving
    /// anything about the variable, so it must stay nullable.
    #[gtest]
    fn test_while_true_break_does_not_narrow() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        def_isvalid_guard(&mut ws);

        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetText fun(self: Panel): string

            ---@return Panel?
            local function GetRow() end

            local function use()
                local p = GetRow()
                while true do
                    print(p)
                    break
                end
                local narrowed = p
                print(narrowed)
            end
            "#,
        );

        let narrowed = nth_name_expr_type_from_end(&mut ws, file_id, "narrowed", 0);
        let desc = ws.humanize_type(narrowed);
        assert!(
            desc.contains("nil") || desc.contains('?'),
            "while true/break must not narrow, got: {}",
            desc
        );
    }
}
