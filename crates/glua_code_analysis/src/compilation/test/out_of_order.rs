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
}
