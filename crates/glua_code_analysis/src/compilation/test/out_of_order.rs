#[cfg(test)]
mod test {
    use crate::VirtualWorkspace;

    /// `local orig = tonumber` runs before the `function tonumber` statement
    /// below it, so it must capture the std global, not the same-file wrapper.
    /// Binding it to the wrapper makes the wrapper self-recursive and its
    /// return type collapses to `nil`.
    #[test]
    fn test_local_init_reads_global_declared_before_it() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def_file(
            "shadow.lua",
            r#"
            local origtonumber = tonumber
            function tonumber(str, base)
                if str == nil then
                    return nil
                end
                return origtonumber(str, base)
            end
            "#,
        );

        // Without the fix the wrapper is self-recursive and this is `nil`.
        let ty = ws.expr_ty("tonumber('1')");
        assert_eq!(ws.humanize_type(ty), "integer?");
    }

    /// A local statement inside a closure runs when the closure is called,
    /// which is after the whole chunk has executed, so it sees the later
    /// declaration. Only chunk-level local statements may filter it out.
    #[test]
    fn test_closure_local_init_still_sees_later_declarations() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def_file(
            "closure_shadow.lua",
            r#"
            local chunkRead = tonumber
            function ChunkRead(str)
                return chunkRead(str)
            end
            function ClosureRead(str)
                local closureRead = tonumber
                return closureRead(str)
            end
            function tonumber(str, base)
                return "wrapped"
            end
            "#,
        );

        let chunk = ws.expr_ty("ChunkRead('1')");
        let chunk_ty = ws.humanize_type(chunk);
        let closure = ws.expr_ty("ClosureRead('1')");
        let closure_ty = ws.humanize_type(closure);

        // The chunk-level read cannot see the wrapper below it; the closure can.
        assert_eq!(chunk_ty, "number?");
        assert_eq!(closure_ty, "\"wrapped\"");
        assert_ne!(chunk_ty, closure_ty);
    }

    /// Known ceiling: when every declaration of the global is later in this
    /// file, the filter would leave nothing, so it is dropped and the wrapper
    /// binds to itself. Returning "no type" instead would break legitimate
    /// forward references, so this pins the boundary rather than fixing it.
    #[test]
    fn test_single_decl_wrapper_stays_self_recursive() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def_file(
            "only_decl.lua",
            r#"
            local origOnlyHere = OnlyHere
            function OnlyHere(str)
                if str == nil then
                    return nil
                end
                return origOnlyHere(str)
            end
            "#,
        );

        let ty = ws.expr_ty("OnlyHere('1')");
        // The explicit `return nil` and the unresolved self-recursive return
        // both survive: `nil` alone would claim a certainty the body does not
        // have.
        assert_eq!(ws.humanize_type(ty), "unknown?");
    }

    #[test]
    fn test_unorder_analysis() {
        let mut ws = VirtualWorkspace::new();

        let files = vec![
            (
                "rx.lua",
                r#"
            local subject = require("subject")

            local rx = {
                subject = subject,
            }

            return rx
            "#,
            ),
            (
                "subject.lua",
                r#"
            ---@class Subject
            local subject = {}

            ---@return Subject
            function subject.new()

            end

            return subject
            "#,
            ),
        ];

        ws.def_files(files);

        let ty = ws.expr_ty("require('rx').subject.new()");
        let expected = ws.ty("Subject");
        assert_eq!(ty, expected);
    }

    #[test]
    fn test_unorder_analysis_2() {
        let mut ws = VirtualWorkspace::new();

        let files = vec![
            (
                "meta.lua",
                r#"
                vim = {}
                vim.o.a = 1
                "#,
            ),
            (
                "options.lua",
                r#"
                require "meta"
            vim.o = {}
            "#,
            ),
        ];

        ws.def_files(files);

        let o_ty = ws.expr_ty("vim.o");
        println!("{:?}", o_ty);
        let a_ty = ws.expr_ty("vim.o.a");
        println!("{:?}", a_ty);
        // let expected = ws.ty("Subject");
        // assert_eq!(ty, expected);
    }

    /// `SF = {}` lives in a file analysed *after* the one declaring
    /// `SF.Instance`, so `self` inside `function SF.Instance:Build()` is
    /// still `Unknown` during that file's own pass and none of the three
    /// member-write forms can be attached to `SF.Instance`. The
    /// dynamic-field index is what rescues the read sites — and it only
    /// walked `LuaAssignStat`, so the `function self.x() end` form was
    /// reported `undefined-field` while the two assignment forms were fine.
    #[test]
    fn test_self_member_writes_attach_when_global_root_is_in_another_file() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = crate::Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        ws.def_files(vec![
            (
                "lua/autorun/a.lua",
                r#"
                SF.Instance = {}
                SF.Instance.__index = SF.Instance

                function SF.Instance:BuildEnvironment()
                    self.viaAssign = {}
                    self.viaAssignFn = function(x) return x end
                    function self.viaFuncStat(x) return x end
                end

                "#,
            ),
            // Analysed after `a.lua` (file ids follow normalized path order).
            ("lua/starfall/sflib.lua", "SF = {}"),
        ]);

        let instance = ws.expr_ty("SF.Instance");
        assert!(
            matches!(instance, crate::LuaType::TableConst(_)),
            "SF.Instance should be a table const, got {instance:?}"
        );

        for field in ["viaAssign", "viaAssignFn", "viaFuncStat"] {
            let ty = ws.expr_ty(&format!("SF.Instance.{field}"));
            assert!(
                !ty.is_unknown() && !ty.is_nil(),
                "SF.Instance.{field} did not attach: {ty:?}"
            );
        }
    }
}
