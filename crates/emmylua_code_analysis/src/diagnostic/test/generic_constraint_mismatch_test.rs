#[cfg(test)]
mod test {
    use googletest::prelude::*;
    use lsp_types::{DiagnosticSeverity, NumberOrString};
    use tokio_util::sync::CancellationToken;

    use crate::{DiagnosticCode, VirtualWorkspace};

    fn first_diagnostic_for_code(
        ws: &mut VirtualWorkspace,
        diagnostic_code: DiagnosticCode,
        block_str: &str,
    ) -> Option<lsp_types::Diagnostic> {
        ws.analysis.diagnostic.enable_only(diagnostic_code);
        let file_id = ws.def(block_str);
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())?;
        let code_string = Some(NumberOrString::String(diagnostic_code.get_name().to_string()));
        diagnostics.into_iter().find(|diagnostic| diagnostic.code == code_string)
    }

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
    fn test_str_tpl_ref_missing_type_with_constraint_reports_hint_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        let code = r#"
                    ---@class Entity

                    ents = {}

                    ---@generic T: Entity
                    ---@param class `T`
                    ---@return T
                    function ents.Create(class)
                    end

                    ents.Create("sent_custom")
                "#;

        expect_that!(
            ws.check_code_for(DiagnosticCode::GenericConstraintMismatch, code),
            eq(false)
        );

        let diagnostic = first_diagnostic_for_code(
            &mut ws,
            DiagnosticCode::GenericConstraintMismatch,
            code,
        );
        expect_that!(diagnostic.is_some(), eq(true));
        expect_that!(
            diagnostic.and_then(|diagnostic| diagnostic.severity),
            eq(Some(DiagnosticSeverity::HINT))
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
}
