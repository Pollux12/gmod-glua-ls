#[cfg(test)]
mod test {
    use googletest::prelude::*;
    use lsp_types::{DiagnosticSeverity, NumberOrString};
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

        expect_that!(
            diagnostic.severity,
            eq(Some(DiagnosticSeverity::HINT))
        );
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
