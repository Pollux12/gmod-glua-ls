#[cfg(test)]
mod tests {
    use crate::handlers::test_lib::{ProviderVirtualWorkspace, VirtualLocation, check};
    use glua_code_analysis::EmmyrcLuaVersion;
    use googletest::prelude::*;
    use lsp_types::HoverContents;

    /// Build an `Emmyrc` with Lua 5.1 enabled (needed for `module(...)` semantics).
    fn make_lua51_emmyrc(ws: &ProviderVirtualWorkspace) -> glua_code_analysis::Emmyrc {
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.runtime.version = EmmyrcLuaVersion::Lua51;
        emmyrc
    }

    // ── Hover: bare module name ──────────────────────────────────────────────

    /// Hovering over the bare legacy-module name in another file must NOT
    /// render as `{ includes }`.  The type is a `LuaType::Namespace` which
    /// `humanize_type` must render cleanly as just `includes` (or a simple
    /// global binding), never as the brace-wrapped set literal.
    ///
    /// Regression guard for the `humanize_type` / `LuaType::Namespace` fix.
    #[gtest]
    fn test_legacy_module_bare_name_hover_does_not_render_brace_wrapped() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new_with_init_std_lib();
        ws.update_emmyrc(make_lua51_emmyrc(&ws));

        ws.def_file(
            "includes.lua",
            r#"
module("includes", package.seeall)

---Include a file by path.
---@param path string The file path to include
---@return boolean success Whether the include succeeded
function File(path) end
"#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
local mod = inclu<??>des
"#,
        )?;
        let file_id = ws.def_file("consumer_bare.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover result for bare module name")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup, got {:?}", hover.contents);
        };

        assert!(
            !markup.value.contains("{ includes }"),
            "hover on bare legacy module name must not render as '{{ includes }}', got: {}",
            markup.value
        );
        assert!(
            !markup.value.starts_with("{ "),
            "hover on bare legacy module name must not start with '{{ ', got: {}",
            markup.value
        );

        Ok(())
    }

    // ── Hover: member access ─────────────────────────────────────────────────

    /// Hovering over `includes.File` in an external consumer file must produce
    /// a rich hover containing the function signature and doc comment.
    #[gtest]
    fn test_legacy_module_member_hover_shows_rich_signature() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new_with_init_std_lib();
        ws.update_emmyrc(make_lua51_emmyrc(&ws));

        ws.def_file(
            "includes.lua",
            r#"
module("includes", package.seeall)

---Include a file by path.
---@param path string The file path to include
---@return boolean success Whether the include succeeded
function File(path) end
"#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
includes.Fi<??>le("sv_init.lua")
"#,
        )?;
        let file_id = ws.def_file("consumer_hover.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover result for includes.File")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup, got {:?}", hover.contents);
        };

        // Must show the function name
        assert!(
            markup.value.contains("File"),
            "hover on includes.File must show the function name, got: {}",
            markup.value
        );
        // Must include signature with parameter
        assert!(
            markup.value.contains("path"),
            "hover on includes.File must show the 'path' parameter, got: {}",
            markup.value
        );
        // Must include the doc comment
        assert!(
            markup.value.contains("Include a file by path"),
            "hover on includes.File must include the doc comment, got: {}",
            markup.value
        );
        // Must NOT be a plain global/namespace stub without content
        assert!(
            markup.value.contains("```lua"),
            "hover on includes.File must include a lua code block, got: {}",
            markup.value
        );

        Ok(())
    }

    // ── Hover: method-style member ───────────────────────────────────────────

    /// Hovering over `netstream.Send` (dot-access on method) in an external
    /// consumer file must produce a rich hover with signature and docs.
    #[gtest]
    fn test_legacy_module_method_member_hover_shows_rich_signature() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new_with_init_std_lib();
        ws.update_emmyrc(make_lua51_emmyrc(&ws));

        ws.def_file(
            "netstream.lua",
            r#"
module("netstream", package.seeall)

---Send a net message to a target.
---@param name string The network message name
---@param payload table The data payload to send
---@return boolean success Whether the send succeeded
function Send(name, payload) end
"#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
netstream.Se<??>nd("chat", {})
"#,
        )?;
        let file_id = ws.def_file("consumer_netstream.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover result for netstream.Send")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup, got {:?}", hover.contents);
        };

        assert!(
            markup.value.contains("Send"),
            "hover on netstream.Send must show the function name, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("name"),
            "hover on netstream.Send must show the 'name' parameter, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("Send a net message"),
            "hover on netstream.Send must include the doc comment, got: {}",
            markup.value
        );

        Ok(())
    }

    // ── Goto-definition: static member ──────────────────────────────────────

    /// Goto-definition on `includes.File` in a consumer file must jump to the
    /// function definition line inside `includes.lua`.
    #[gtest]
    fn test_legacy_module_member_goto_definition_resolves_to_definition_file() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new_with_init_std_lib();
        ws.update_emmyrc(make_lua51_emmyrc(&ws));

        ws.def_file(
            "includes.lua",
            r#"
module("includes", package.seeall)

---Include a file by path.
---@param path string The file path to include
---@return boolean success Whether the include succeeded
function File(path) end
"#,
        );

        // check_definition creates an auto-named file for the consumer.
        // The cursor is on "File" in "includes.File".
        // Line 7 (0-indexed) in includes.lua is `function File(path) end`.
        check!(ws.check_definition(
            r#"
includes.Fi<??>le("sv_init.lua")
"#,
            vec![VirtualLocation {
                file: "includes.lua".to_string(),
                line: 6,
            }]
        ));

        Ok(())
    }

    // ── Goto-definition: netstream method ───────────────────────────────────

    /// Goto-definition on `netstream.Send` must resolve to the Send definition
    /// inside `netstream.lua`.
    #[gtest]
    fn test_legacy_module_method_goto_definition_resolves_to_definition_file() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new_with_init_std_lib();
        ws.update_emmyrc(make_lua51_emmyrc(&ws));

        ws.def_file(
            "netstream.lua",
            r#"
module("netstream", package.seeall)

---Send a net message to a target.
---@param name string The network message name
---@param payload table The data payload to send
---@return boolean success Whether the send succeeded
function Send(name, payload) end
"#,
        );

        // "Send" is defined at line 7 (0-indexed) in netstream.lua.
        check!(ws.check_definition(
            r#"
netstream.Se<??>nd("chat", {})
"#,
            vec![VirtualLocation {
                file: "netstream.lua".to_string(),
                line: 7,
            }]
        ));

        Ok(())
    }
}
