#[cfg(test)]
mod test {
    use googletest::prelude::*;
    use lsp_types::{DiagnosticSeverity, NumberOrString, Position};
    use tokio_util::sync::CancellationToken;

    use crate::{DiagnosticCode, VirtualWorkspace};

    #[test]
    fn test_1() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
                ---@class Component
                ---@class G.A
                ---@class G.B: Component

                ---@generic T: Component
                ---@param name `T`
                ---@return T
                local function new(name)
                    return name
                end

                new("G.A")
        "#
        ));
    }

    #[test]
    fn test_2() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
                ---@class Component
                ---@class G.A
                ---@class G.B: Component

                ---@generic T: Component
                ---@param name T
                ---@return T
                local function new(name)
                    return name
                end

                new("G.A")
        "#
        ));
    }

    #[test]
    fn test_3() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
            local nargs = select('#')
        "#
        ));
    }

    #[test]
    fn test_4() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
                ---@class Component
                ---@class G.A
                ---@class G.B: Component
                ---@class G.C: G.B

                ---@generic T: Component
                ---@param name `T`
                ---@return T
                local function new(name)
                    return name
                end

                new("G.C")
        "#
        ));
    }

    #[test]
    fn test_class_1() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
                ---@class Component
                ---@class G.A
                ---@class G.B: Component

                ---@class GenericTest<T: Component>
                local M = {}

                ---@param a T
                function M.new(a)
                end

                ---@type G.A
                local a

                M.new(a)
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"

                ---@type G.B
                local b

                ---@type GenericTest
                local gt

                gt.new(b)
        "#
        ));
    }

    #[test]
    fn test_class_2() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
            ---@class Component
            ---@class G.A
            ---@class G.B: Component

            ---@class GenericTest<T: Component>
            local M = {}

            ---@param a T
            function M.new(a)
            end

            ---@type GenericTest<G.A>
            local a
        "#
        ));
    }

    #[test]
    fn test_extend_string() {
        let mut ws = VirtualWorkspace::new();
        assert!(!ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
                ---@class ABC1

                ---@generic T: string
                ---@param t `T`
                ---@return T
                local function test(t)
                end

                test("ABC1")
        "#
        ));
    }

    #[test]
    fn test_str_tpl_ref_param() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
                ---@generic T
                ---@param a `T`
                local function bar(a)
                end

                ---@generic T
                ---@param a `T`
                local function foo(a)
                    bar(a)
                end
        "#
        ));
    }

    #[gtest]
    fn test_str_tpl_ref_declared_type_with_constraint_no_diagnostic() {
        let mut ws = VirtualWorkspace::new();

        expect_that!(
            ws.check_code_for(
                DiagnosticCode::GenericConstraintMismatch,
                r#"
                    ---@class Entity
                    ---@class sent_npc: Entity

                    ents = {}

                    ---@generic T: Entity
                    ---@param class `T`
                    ---@return T
                    function ents.Create(class)
                    end

                    ents.Create("sent_npc")
                "#
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_str_tpl_ref_overload_match_does_not_check_base_generic_signature() {
        let mut ws = VirtualWorkspace::new();

        expect_that!(
            ws.check_code_for(
                DiagnosticCode::GenericConstraintMismatch,
                r#"
                    ---@class Panel

                    ---@generic T: Panel
                    ---@overload fun(self: Panel, panelTable: table): Panel
                    ---@param className `T`
                    ---@return T
                    function Panel:Add(className) end

                    local PANEL = {}
                    local parent ---@type Panel
                    parent:Add(PANEL)
                "#
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_str_tpl_ref_forwarded_generic_class_name_no_diagnostic() {
        let mut ws = VirtualWorkspace::new();

        expect_that!(
            ws.check_code_for(
                DiagnosticCode::GenericConstraintMismatch,
                r#"
                    ---@class Panel

                    ---@generic T: Panel
                    ---@param className `T`
                    ---@return T
                    function create_x(className) end

                    ---@generic T: Panel
                    ---@param className `T`
                    ---@return T
                    function create(className)
                        return create_x(className)
                    end
                "#
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_str_tpl_ref_string_union_field_with_constraint_no_diagnostic() {
        let mut ws = VirtualWorkspace::new();

        expect_that!(
            ws.check_file_for(
                DiagnosticCode::GenericConstraintMismatch,
                "gamemodes/terrortown/entities/entities/ttt_random_weapon.lua",
                r#"
                    ---@class Entity
                    ---@class NULL: Entity
                    ---@class item_ammo_smg1: Entity
                    ---@class item_ammo_pistol: Entity

                    ents = {}

                    ---@generic T: Entity
                    ---@param class `T`
                    ---@return T|NULL
                    function ents.Create(class)
                    end

                    ---@class TttRandomWeapon
                    ---@field AmmoEnt "item_ammo_smg1"|"item_ammo_pistol"

                    ---@type TttRandomWeapon
                    local ent

                    local ammo = ents.Create(ent.AmmoEnt)
                "#,
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_str_tpl_ref_missing_type_with_constraint_reports_hint_for_auto_created_type() {
        let mut ws = VirtualWorkspace::new();
        ws.enable_check(DiagnosticCode::GenericConstraintMismatch);

        let file_id = ws.def(
            r#"
                    ---@class Entity

                    ents = {}

                    ---@generic T: Entity
                    ---@param class `T`
                    ---@return T
                    function ents.Create(class)
                    end

                    ents.Create("sent_custom")
                "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let code = Some(NumberOrString::String(
            DiagnosticCode::GenericConstraintMismatch
                .get_name()
                .to_string(),
        ));
        let diagnostic = diagnostics
            .iter()
            .find(|diag| diag.code == code)
            .expect("expected generic-constraint-mismatch diagnostic");

        expect_that!(diagnostic.severity, eq(Some(DiagnosticSeverity::HINT)));
        expect_that!(
            diagnostic.message.contains(
                "Type `sent_custom` is not explicitly defined; auto-created inheriting `Entity`"
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_str_tpl_ref_missing_type_without_constraint_reports_diagnostic() {
        let mut ws = VirtualWorkspace::new();

        expect_that!(
            ws.check_code_for(
                DiagnosticCode::GenericConstraintMismatch,
                r#"
                    ents = {}

                    ---@generic T
                    ---@param class `T`
                    ---@return T
                    function ents.Create(class)
                    end

                    ents.Create("sent_custom")
                "#
            ),
            eq(false)
        );
    }

    #[test]
    fn test_issue_516() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
                ---@generic T: table
                ---@param t T
                ---@return T
                local function wrap(t)
                    return t
                end

                local a --- @type string[]?
                wrap(assert(a))
        "#
        ));
    }

    #[test]
    fn test_union() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class ab

            ---@generic T
            ---@param a `T`|T
            ---@return T
            function name(a)
                return a
            end
        "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
            ---@type ab
            local a

            name(a)
        "#
        ));
        assert!(ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
            name("ab")
        "#
        ));

        assert!(!ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
            name("a")
        "#
        ));
    }

    #[test]
    fn test_union_2() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@generic T: table
            ---@param obj T
            function add(obj)
            end

            ---@class GCNode
        "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
            ---@generic T: table
            ---@param obj T | string
            ---@return T?
            function bindGC(obj)
                if type(obj) == "string" then
                    ---@type GCNode
                    obj = {}
                end

                return add(obj)
            end
        "#
        ));
    }

    #[test]
    fn test_union_3() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@generic T: table
            ---@param obj T
            function add(obj)
            end


        "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"

            ---@class GCNode<T: table>
            GCNode = {}

            ---@param obj T
            ---@return T?
            function GCNode:bindGC(obj)
                return add(obj)
            end
        "#
        ));
    }

    #[test]
    fn test_union_str_tpl_with_constraint_panel_known_type_no_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Panel

            ---@generic T: Panel
            ---@param name `T`|T
            ---@return T
            function create_panel(name)
                return name
            end
        "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
            create_panel("Panel")
        "#
        ));
    }

    #[test]
    fn test_union_str_tpl_with_constraint_panel_missing_type_reports_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Panel

            ---@generic T: Panel
            ---@param name `T`|T
            ---@return T
            function create_panel(name)
                return name
            end
        "#,
        );

        assert!(!ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
            create_panel("MissingPanel")
        "#
        ));
    }

    #[gtest]
    fn test_str_tpl_union_all_valid_panel_subtypes_no_diagnostic() {
        let mut ws = VirtualWorkspace::new();

        expect_that!(
            ws.check_code_for(
                DiagnosticCode::GenericConstraintMismatch,
                r#"
                    ---@class Panel
                    ---@class DPanel: Panel
                    ---@class DButton: Panel

                    ---@generic T: Panel
                    ---@param name `T`
                    ---@return T
                    function create_panel(name) end

                    ---@type "DPanel"|"DButton"
                    local panel_type

                    create_panel(panel_type)
                "#
            ),
            eq(true)
        );
    }

    #[gtest]
    fn test_str_tpl_union_mixed_valid_and_missing_emits_no_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        ws.enable_check(DiagnosticCode::GenericConstraintMismatch);

        let file_id = ws.def(
            r#"
                    ---@class Panel
                    ---@class DPanel: Panel

                    ---@generic T: Panel
                    ---@param name `T`
                    ---@return T
                    function create_panel(name) end

                    ---@type "MissingPanel"|"DPanel"
                    local panel_type

                    create_panel(panel_type)
                "#,
        );

        let diagnostics = generic_constraint_mismatch_diagnostics(&mut ws, file_id);

        expect_that!(diagnostics.len(), eq(0));
    }

    #[gtest]
    fn test_str_tpl_union_all_missing_emits_single_hint_at_call_range() {
        let mut ws = VirtualWorkspace::new();
        ws.enable_check(DiagnosticCode::GenericConstraintMismatch);

        let code = r#"
                    ---@class Panel

                    ---@generic T: Panel
                    ---@param name `T`
                    ---@return T
                    function create_panel(name) end

                    ---@type "MissingPanel"|"OtherMissingPanel"
                    local panel_type

                    create_panel(panel_type)
                "#;
        let file_id = ws.def(code);

        let diagnostics = generic_constraint_mismatch_diagnostics(&mut ws, file_id);
        let expected_start = position_of(code, "panel_type)");
        let diagnostic = diagnostics
            .first()
            .expect("expected one generic-constraint-mismatch diagnostic");

        expect_that!(diagnostics.len(), eq(1));
        expect_that!(diagnostic.severity, eq(Some(DiagnosticSeverity::HINT)));
        expect_that!(diagnostic.range.start, eq(expected_start));
    }

    #[gtest]
    fn test_str_tpl_union_all_non_string_emits_single_hard_mismatch() {
        let mut ws = VirtualWorkspace::new();
        ws.enable_check(DiagnosticCode::GenericConstraintMismatch);

        let file_id = ws.def(
            r#"
                    ---@class Panel

                    ---@generic T: Panel
                    ---@param name `T`
                    ---@return T
                    function create_panel(name) end

                    ---@type integer|boolean
                    local panel_type

                    create_panel(panel_type)
                "#,
        );

        let diagnostics = generic_constraint_mismatch_diagnostics(&mut ws, file_id);

        expect_that!(diagnostics.len(), eq(1));
        expect_that!(
            diagnostics[0].severity,
            not(eq(Some(DiagnosticSeverity::HINT)))
        );
        expect_that!(
            diagnostics[0].message.as_str(),
            eq("the string template type must be a string constant")
        );
    }

    #[test]
    fn test_generic_keyof_param_scope() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@generic T, K extends keyof T
                ---@param object T
                ---@param key K
                ---@return std.RawGet<T, K>
                function pick(object, key)
                end

                ---@class Person
                ---@field name string
            "#,
        );
        assert!(!ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
            ---@type Person
            local person

            pick(person, "abc")
        "#
        ));

        assert!(ws.check_code_for(
            DiagnosticCode::GenericConstraintMismatch,
            r#"
            ---@type Person
            local person

            pick(person, "name")
        "#
        ));
    }

    fn generic_constraint_mismatch_diagnostics(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
    ) -> Vec<lsp_types::Diagnostic> {
        let code = Some(NumberOrString::String(
            DiagnosticCode::GenericConstraintMismatch
                .get_name()
                .to_string(),
        ));

        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diag| diag.code == code)
            .collect()
    }

    fn position_of(source: &str, needle: &str) -> Position {
        let offset = source.find(needle).expect("needle should exist in source");
        let mut line = 0;
        let mut character = 0;
        for ch in source[..offset].chars() {
            if ch == '\n' {
                line += 1;
                character = 0;
            } else {
                character += 1;
            }
        }

        Position::new(line, character)
    }
}
