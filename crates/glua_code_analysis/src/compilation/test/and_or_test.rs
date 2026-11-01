#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, VirtualWorkspace};
    use glua_parser::{LuaAstNode, LuaBinaryExpr};
    use googletest::prelude::*;

    #[test]
    fn test_issue_221() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
        --- @param opts table|nil
        --- @return any
        function foo(opts)
        opts = opts or {}
        return opts.field
        end

        --- @param opts table?
        --- @return any
        function bar(opts)
        opts = opts or {}
        return opts.field
        end
            "#,
        ));
    }

    #[test]
    fn test_issue_222() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local a --- @type boolean
            local b --- @type true?
            a = b or false
            "#,
        ));
    }

    #[test]
    fn test_issue_230() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local b = true ---@type boolean
            a = b and 2 or nil
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let a_humanize = ws.humanize_type(a_ty);
        let a_expected = "2?";
        assert_eq!(a_humanize, a_expected);
    }

    #[test]
    fn test_issue_258() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            local a            --- @type string|nil
            local b            --- @type string|nil
            c = a or b
            "#,
        );

        let c = ws.expr_ty("c");
        let c_desc = ws.humanize_type(c);
        assert_eq!(c_desc, "string?");
    }

    #[test]
    fn test_issue_470() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class Player : PlayerBase

            ---@class PlayerBase
            local Player = {}

            ---@return boolean
            ---@return_cast self Player
            function Player:isPlayer()
                return true
            end

            local topCreature ---@type PlayerBase?


            if not topCreature or not topCreature:isPlayer() then
                return
            end

            a = topCreature
            "#,
        );

        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_eq!(desc, "Player");
    }

    #[test]
    fn test_issue_482() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
            local command_provider --- @type {commands: string[]}?

            --- @type string[]
            local commands = type(command_provider) == 'table' and command_provider.commands or {}
            "#,
        ));
    }

    #[test]
    fn test_issue_644() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            --- @alias mapfn fun()

            local f1 --- @type string|mapfn
            local _ = type(f1) ~= 'function' and f1 or f1()
            "#,
        ));
    }

    // ── Inferred string default: type must stay string ─────────────────

    #[gtest]
    fn test_string_or_literal_keeps_string_type_annotated() {
        // With `---@param panelClass string|nil` then `panelClass = panelClass or "DScrollPanel"`,
        // the inferred type must be `string` (not the literal, not a union).
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param panelClass string|nil
            function foo(panelClass)
                panelClass = panelClass or "DScrollPanel"
                a = panelClass
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_eq!(desc, "string");
    }

    #[gtest]
    fn test_string_or_literal_keeps_string_type_unannotated() {
        // Without annotation, `panelClass = panelClass or "DScrollPanel"`
        // must still infer `string`.
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            function foo(panelClass)
                panelClass = panelClass or "DScrollPanel"
                a = panelClass
            end
            "#,
        );

        let a = ws.expr_ty("a");
        let desc = ws.humanize_type(a);
        assert_eq!(desc, "string");
    }

    #[gtest]
    fn test_self_referential_table_bootstrap_ignores_sibling_type() {
        // `X.a = X.a or {}` defines the table; the self-read on the left cannot inform
        // its own definition, so an already resolved type must not be folded in. Without
        // this the fold depends on what the left read happens to resolve to, which
        // differs between a cold build and a warm partial re-index.
        let mut ws = VirtualWorkspace::new();

        ws.def_file(
            "a.lua",
            r#"
            ---@class Order
            ---@field id number

            ---@class Reg
            ---@field entry Order

            ---@type Reg
            Registry = {}
            "#,
        );

        let file_id = ws.def_file(
            "b.lua",
            r#"
            function Bootstrap()
                Registry.entry = Registry.entry or {}
            end
            "#,
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let binary_expr = semantic_model
            .get_root()
            .descendants::<LuaBinaryExpr>()
            .next()
            .expect("expected the or-expression");
        let folded = semantic_model
            .get_semantic_info(binary_expr.syntax().clone().into())
            .expect("expected semantic info")
            .display_typ()
            .clone();

        assert_eq!(ws.humanize_type(folded), "table");
    }

    #[gtest]
    fn test_resolved_local_or_empty_table_keeps_declared_type() {
        // `x = x or {}` over a resolved, non-optional local is a narrowing idiom,
        // not a table bootstrap: the declared type must survive the fold.
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            local x --- @type string
            x = x or {}
            a = x
            "#,
        );

        let a = ws.expr_ty("a");
        assert_eq!(ws.humanize_type(a), "string");
    }

    #[gtest]
    fn test_resolved_local_or_empty_table_string_call_no_diagnostic() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::UndefinedField,
            r#"
            local x --- @type string
            x = x or {}
            local y = x:upper()
            "#,
        ));
    }

    #[gtest]
    fn test_unresolved_global_or_empty_table_still_bootstraps_table() {
        // An undeclared global has nothing to resolve to, so `Foo = Foo or {}`
        // remains a bootstrap: the fold yields the fresh table.
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            Foo = Foo or {}
            a = Foo
            "#,
        );

        let a = ws.expr_ty("a");
        assert_eq!(ws.humanize_type(a), "table");
    }

    #[gtest]
    fn test_string_param_or_empty_table_keeps_string() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@param x string
            function f(x)
                x = x or {}
                a = x
            end
            "#,
        );

        let a = ws.expr_ty("a");
        assert_eq!(ws.humanize_type(a), "string");
    }

    #[test]
    fn arithmetic_over_constants_folds_and_over_a_field_read_does_not() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            local SIZE = 4
            local PREFIX = "gm_"
            area = SIZE * SIZE
            menu = PREFIX .. "menu"
            local counts = {}
            counts.total = 1
            next_total = counts.total + 1
            "#,
        );

        let area = ws.expr_ty("area");
        let menu = ws.expr_ty("menu");
        let next_total = ws.expr_ty("next_total");
        assert_eq!(
            (
                ws.humanize_type(area),
                ws.humanize_type(menu),
                ws.humanize_type(next_total)
            ),
            (
                "16".to_string(),
                "\"gm_menu\"".to_string(),
                "integer".to_string()
            )
        );
    }

    /// Locals each built from the one before, read twice: `a{steps}` holds
    /// `2^steps`.
    fn doubling_chain(steps: usize) -> String {
        let mut text = String::from("local a0 = 1\n");
        for i in 1..=steps {
            text.push_str(&format!("local a{i} = a{} + a{}\n", i - 1, i - 1));
        }
        text.push_str(&format!("chain_result = a{steps}\n"));
        text
    }

    /// Classifying an operand reads each local's initializer, and every local
    /// here is read twice by the next one, so without a memo the work doubles
    /// per local. Counted rather than timed so the guard cannot flake.
    #[test]
    fn a_chain_of_locals_read_twice_each_is_classified_once_per_local() {
        let classified = |steps: usize| {
            crate::semantic::CONSTANT_EXPR_CLASSIFICATIONS.with(|count| count.set(0));
            let mut ws = VirtualWorkspace::new();
            ws.def(&doubling_chain(steps));
            let result = ws.expr_ty("chain_result");
            (
                ws.humanize_type(result),
                crate::semantic::CONSTANT_EXPR_CLASSIFICATIONS.with(|count| count.get()),
            )
        };

        let (small_result, small) = classified(12);
        let (large_result, large) = classified(24);

        assert_eq!(
            (small_result, large_result),
            ("4096".to_string(), "16777216".to_string())
        );
        assert!(small > 0, "no classification ran; the guard is vacuous");
        assert!(
            large <= small * 4,
            "classifications grew from {small} to {large} when the chain doubled"
        );
    }

    #[test]
    fn a_chain_of_locals_widens_once_an_edit_reassigns_a_link() {
        let path = "lua/autorun/sh_chain.lua";
        let chain = doubling_chain(6);
        let edited = format!("{chain}a3 = 5\n");
        let mut ws = VirtualWorkspace::assert_warm_matches_cold(
            |_| {},
            &[(path, chain.as_str())],
            &[(path, edited.as_str())],
        );

        let result = ws.expr_ty("chain_result");
        assert_eq!(ws.humanize_type(result), "integer");
    }
}
