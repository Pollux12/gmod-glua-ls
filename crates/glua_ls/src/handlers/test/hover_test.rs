#[cfg(test)]
mod tests {
    use crate::handlers::test_lib::{ProviderVirtualWorkspace, VirtualHoverResult, check};
    use glua_code_analysis::EmmyrcGmodScriptedClassScopeEntry;
    use googletest::prelude::*;
    use lsp_types::HoverContents;

    fn legacy_scope(pattern: &str) -> EmmyrcGmodScriptedClassScopeEntry {
        EmmyrcGmodScriptedClassScopeEntry::LegacyGlob(pattern.to_string())
    }

    fn dedent(input: &str) -> String {
        let lines: Vec<&str> = input.lines().collect();
        let mut min_indent = usize::MAX;
        for line in &lines {
            if line.trim().is_empty() {
                continue;
            }
            let indent = line.chars().take_while(|c| *c == ' ').count();
            min_indent = min_indent.min(indent);
        }
        if min_indent == usize::MAX {
            return String::new();
        }
        let mut out = String::new();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = if line.len() >= min_indent {
                &line[min_indent..]
            } else {
                line
            };
            out.push_str(trimmed);
            if i + 1 < lines.len() {
                out.push('\n');
            }
        }
        out.trim_start_matches('\n').trim_end().to_string()
    }

    #[gtest]
    fn test_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class <??>A
                ---@field a number
                ---@field b string
                ---@field c boolean
            "#,
            VirtualHoverResult {
                value:
                    "```lua\n(class) A {\n    a: number,\n    b: string,\n    c: boolean,\n}\n```"
                        .to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_right_to_left() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        // check!(ws.check_hover(
        //     r#"
        //         ---@class H4
        //         local m = {
        //             x = 1
        //         }

        //         ---@type H4
        //         local m1

        //         m1.x = {}
        //         m1.<??>x = {}
        //     "#,
        //     VirtualHoverResult {
        //         value: "```lua\n(field) x: integer = 1\n```".to_string(),
        //     },
        // ));

        check!(ws.check_hover(
            r#"
                ---@class Node
                ---@field x number
                ---@field right Node?

                ---@return Node
                local function createRBNode()
                end

                ---@type Node
                local node

                if node.right then
                else
                    node.<??>right = createRBNode()
                end
            "#,
            VirtualHoverResult {
                value: "```lua\n(field) right: Node\n```".to_string(),
            },
        ));

        check!(ws.check_hover(
            r#"
                 ---@class Node1
                ---@field x number

                ---@return Node1
                local function createRBNode()
                end

                ---@type Node1?
                local node

                if node then
                else
                    <??>node = createRBNode()
                end
            "#,
            VirtualHoverResult {
                value: "```lua\nlocal node: Node1 {\n    x: number,\n}\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_hover_nil() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class A
                ---@field a? number

                ---@type A
                local a

                local d = a.<??>a
            "#,
            VirtualHoverResult {
                value: "```lua\n(field) a: number?\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_hover_decl_shows_inheritance_chain() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class BaseEntity
                ---@class Entity: BaseEntity
                ---@class Player: Entity

                ---@type Player
                local <??>ply
            "#,
            VirtualHoverResult {
                value: "```lua\nlocal ply: Player : Entity : BaseEntity\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_hover_decl_shows_full_deep_inheritance_chain() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class A
                ---@class B: A
                ---@class C: B
                ---@class D: C
                ---@class E: D
                ---@class F: E

                ---@type F
                local <??>value
            "#,
            VirtualHoverResult {
                value: "```lua\nlocal value: F : E : D : C : B : A\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_function_infer_return_val() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                local function <??>f(a, b)
                    a = 1
                end
            "#,
            VirtualHoverResult {
                value: "```lua\nlocal function f(a, b)\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_hover_param_string() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@param n string doc
                function foo(<??>n)
                end
            "#,
            VirtualHoverResult {
                value: dedent(
                    r#"
                    ```lua
                    local n: string
                    ```

                    ---

                    doc
                    "#
                )
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_hover_param_func() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@param n fun():boolean doc
                function foo(<??>n)
                end
            "#,
            VirtualHoverResult {
                value: dedent(
                    r#"
                    ```lua
                    local function n() -> boolean
                    ```

                    ---

                    doc
                    "#
                )
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_hover_narrowed_function_type() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@param n integer|fun():boolean
                function _G.foo(n)
                    local f = n
                    if type(f) ~= 'function' then
                        f = function()
                            return true
                        end
                    end
                    local _ = <??>f
                end
            "#,
            VirtualHoverResult {
                value: dedent(
                    r#"
                    ```lua
                    local function n() -> boolean
                    ```
                    "#
                ),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_decl_desc() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class Buff.AddData
                ---@field pulse? number 心跳周期

                ---@type Buff.AddData
                local data

                data.pu<??>lse
            "#,
            VirtualHoverResult {
                value: "```lua\n(field) pulse: number?\n```\n\n&nbsp;&nbsp;in class `Buff.AddData`\n\n---\n\n心跳周期".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_issue_535() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@type table<string, number>
                local t

                ---@class T1
                local a

                function a:init(p)
                    self._c<??>fg = t[p]
                end
            "#,
            VirtualHoverResult {
                value: "```lua\n(field) _cfg: number\n```".to_string(),
            },
        ));

        check!(ws.check_hover(
            r#"
                ---@type table<string, number>
                local t = {
                }
                ---@class T2
                local a = {}

                function a:init(p)
                    self._cfg = t[p]
                end

                ---@param p T2
                function fun(p)
                    local x = p._c<??>fg
                end
            "#,
            VirtualHoverResult {
                value: "```lua\n(field) _cfg: number\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_signature_desc() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                -- # A
                local function a<??>bc()
                end
            "#,
            VirtualHoverResult {
                value: "```lua\nlocal function abc()\n```\n\n---\n\n# A".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_class_desc() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---A1
                ---@class AB<??>C
                ---A2
            "#,
            VirtualHoverResult {
                value: "```lua\n(class) ABC\n```\n\n---\n\nA1".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_alias_desc() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@alias Tes<??>Alias
                ---| 'A' # A1
                ---| 'B' # A2
            "#,
            VirtualHoverResult {
                value: "```lua\n(alias) TesAlias = (\"A\"|\"B\")\n    | \"A\" -- A1\n    | \"B\" -- A2\n\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_type_desc() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                local export = {
                    ---@type number? activeSub
                    vvv = nil
                }

                export.v<??>vv
            "#,
            VirtualHoverResult {
                value: "```lua\n(field) vvv: number?\n```\n\n---\n\nactiveSub".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_field_key() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def(
            r#"
                ---@class ObserverParams
                ---@field next fun() # 测试

                ---@param params fun() | ObserverParams
                function test(params)
                end
            "#,
        );
        check!(ws.check_hover(
            r#"
                test({
                    <??>next = function()
                    end
                })
            "#,
            VirtualHoverResult {
                value: "```lua\n(field) ObserverParams.next()\n```\n\n---\n\n测试".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_field_key_for_generic() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def(
            r#"
                ---@class ObserverParams<T>
                ---@field next fun() # 测试

                ---@generic T
                ---@param params fun() | ObserverParams<T>
                function test(params)
                end
            "#,
        );
        check!(ws.check_hover(
            r#"
                test({
                    <??>next = function()
                    end
                })
            "#,
            VirtualHoverResult {
                value: "```lua\n(field) ObserverParams.next()\n```\n\n---\n\n测试".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_before_dot_returns_object_info() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def(
            r#"
                ---@class Node
                ---@field field number?
                ---@field method fun(self: Node)

                ---@type Node
                node = {}

                function node.method() end
            "#,
        );

        check!(ws.check_hover(
            r#"
                node<??>.field = nil
            "#,
            VirtualHoverResult {
                value: "```lua\n(global) node: Node {\n    field: number?,\n    method: function,\n}\n```".to_string(),
            },
        ));

        check!(ws.check_hover(
            r#"
                node<??>:method()
            "#,
            VirtualHoverResult {
                value: "```lua\n(global) node: Node {\n    field: number?,\n    method: function,\n}\n```".to_string(),
            },
        ));

        check!(ws.check_hover(
            r#"
                node<??>["key"] = "value"
            "#,
            VirtualHoverResult {
                value: "```lua\n(global) node: Node {\n    field: number?,\n    method: function,\n}\n```".to_string(),
            },
        ));

        check!(ws.check_hover(
            r#"
                node["key"<??>] = "value"
            "#,
            VirtualHoverResult {
                value: "\"key\"".to_string(),
            },
        ));

        Ok(())
    }

    #[gtest]
    fn test_see_tag() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                --- Description
                ---
                --- @see a.b.c
                local function te<??>st() end
            "#,
            VirtualHoverResult {
                value: "```lua\nlocal function test()\n```\n\n---\n\nDescription\n\n---\n\n@*see* a.b.c".to_string(),
            },
        ));

        check!(ws.check_hover(
            r#"
                --- Description
                ---
                --- @see a.b.c see description
                local function te<??>st() end
            "#,
            VirtualHoverResult {
                value: "```lua\nlocal function test()\n```\n\n---\n\nDescription\n\n---\n\n@*see* a.b.c see description".to_string(),
            },
        ));

        Ok(())
    }

    #[gtest]
    fn test_other_tag() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                --- Description
                ---
                --- @xyz content
                local function te<??>st() end
            "#,
            VirtualHoverResult {
                value: "```lua\nlocal function test()\n```\n\n---\n\nDescription\n\n---\n\n@*xyz* content".to_string(),
            },
        ));

        Ok(())
    }

    #[gtest]
    fn test_class_with_nil() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def(
            r#"
            ---@class A
            ---@field aAnnotation? string a标签

            ---@class B
            ---@field bAnnotation? string b标签
            "#,
        );
        check!(ws.check_hover(
            r#"
            ---@type A|B|nil
            local defaultOpt = {
                aAnnota<??>tion = "a",
            }
            "#,
            VirtualHoverResult {
                value:
                    "```lua\n(field) aAnnotation: string = \"a\"\n```\n\n---\n\na标签".to_string(),
            },
        ));

        Ok(())
    }

    #[gtest]
    fn test_hover_plugin_local_decl_uses_scoped_class_type() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("plugins/**")];
        emmyrc.gmod.hook_mappings.method_prefixes = vec!["PLUGIN".to_string()];
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                local <??>PLUGIN = PLUGIN ---@diagnostic disable-line: undefined-global

                function PLUGIN:PlayerSpawn(client)
                end
            "#,
        )?;
        let file_id = ws.def_file("cityrp/plugins/vehicles/sh_plugin.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup.value.contains("vehicles"),
            "expected hover to include inferred plugin class 'vehicles', got: {}",
            markup.value
        );
        assert!(
            !markup.value.contains("unknown"),
            "expected hover to avoid unknown type, got: {}",
            markup.value
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                local PLUGIN = <??>PLUGIN ---@diagnostic disable-line: undefined-global

                function PLUGIN:PlayerSpawn(client)
                end
            "#,
        )?;
        let file_id = ws.def_file("cityrp/plugins/vehicles/sh_plugin.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup.value.contains("vehicles"),
            "expected RHS PLUGIN hover to include inferred plugin class 'vehicles', got: {}",
            markup.value
        );
        assert!(
            !markup.value.contains("unknown"),
            "expected RHS PLUGIN hover to avoid unknown type, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_entity_ent_uses_scoped_class_type() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                <??>ENT.Type = "anim"
                ENT.Base = "base_gmodentity"
            "#,
        )?;
        let file_id = ws.def_file(
            "cityrp/entities/entities/cityrp_money/sh_init.lua",
            &content,
        );
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup.value.contains("cityrp_money"),
            "expected ENT hover to include scoped class 'cityrp_money', got: {}",
            markup.value
        );
        assert!(
            !markup.value.contains("ENT: ENT"),
            "expected ENT hover to avoid base global type ENT, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_entity_ent_without_base_assignment_uses_scoped_class_type() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                function <??>ENT:Initialize()
                end
            "#,
        )?;
        let file_id = ws.def_file(
            "cityrp/entities/entities/cityrp_inventory/init.lua",
            &content,
        );
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup.value.contains("cityrp_inventory"),
            "expected ENT hover to include scoped class 'cityrp_inventory', got: {}",
            markup.value
        );
        assert!(
            !markup.value.contains("(global) ENT"),
            "expected ENT hover to avoid plain global ENT declaration, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_gm_hook_method_uses_sandbox_docs() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "library/lua/includes/extensions/sandbox_hooks.lua",
            r#"
                ---@class GM
                ---@type GM
                GM = GM or {}

                ---@class SANDBOX
                ---@type SANDBOX
                SANDBOX = SANDBOX or {}

                ---Called when a player attempts to spawn a SENT.
                ---@param ply Player
                ---@param class string
                ---@return boolean
                function SANDBOX:PlayerSpawnSENT(ply, class)
                end
            "#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                function GM:PlayerSpawnSE<??>NT(ply, class)
                    return true
                end
            "#,
        )?;
        let file_id = ws.def_file("gamemode/init.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup
                .value
                .contains("Called when a player attempts to spawn a SENT"),
            "expected hover to include SANDBOX hook docs, got: {}",
            markup.value
        );
        let has_inline_realm_badge = markup.value.contains(
            "![(Shared)](https://github.com/user-attachments/assets/a356f942-57d7-4915-a8cc-559870a980fc)",
        ) || markup.value.contains(
            "![(Server)](https://github.com/user-attachments/assets/d8fbe13a-6305-4e16-8698-5be874721ca1)",
        ) || markup.value.contains(
            "![(Client)](https://github.com/user-attachments/assets/a5f6ba64-374d-42f0-b2f4-50e5c964e808)",
        );
        assert!(
            has_inline_realm_badge,
            "expected hover to include a realm badge, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("**SHARED**")
                || markup.value.contains("**SERVER**")
                || markup.value.contains("**CLIENT**"),
            "expected hover to include explicit realm label text, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("```lua\n(method)"),
            "expected hover to keep syntax-highlighted lua signature, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("PlayerSpawnSENT"),
            "expected hover to include hook function signature, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_hook_add_string_uses_registered_hook_docs() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "library/lua/includes/extensions/sandbox_hooks.lua",
            r#"
                ---@class SANDBOX
                ---@type SANDBOX
                SANDBOX = SANDBOX or {}

                ---Called when a player attempts to spawn a SENT.
                ---@param ply Player
                ---@param class string
                ---@return boolean
                function SANDBOX:PlayerSpawnSENT(ply, class)
                end
            "#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                hook.Add("PlayerSpawnSE<??>NT", "test", function() end)
            "#,
        )?;
        let file_id = ws.def_file("gamemode/init.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup.value.contains("(method) SANDBOX:PlayerSpawnSENT"),
            "expected hook.Add hook-name hover to include SANDBOX method signature, got: {}",
            markup.value
        );
        assert!(
            markup
                .value
                .contains("Called when a player attempts to spawn a SENT"),
            "expected hook.Add hook-name hover to include hook description, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_hook_add_callback_parameter_usage_shows_inferred_type() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@class Entity

                ---@class GM
                ---@type GM
                GM = GM or {}

                ---@param ent Entity
                ---@param input string
                ---@param activator Entity
                ---@param caller Entity
                ---@param value any
                ---@return boolean
                function GM:AcceptInput(ent, input, activator, caller, value) end

                hook = {}
                ---@param eventName string
                ---@param identifier any
                ---@param func function
                function hook.Add(eventName, identifier, func) end

                hook.Add("AcceptInput", "test", function(ent, input, activator, caller, value)
                    print(in<??>put)
                end)
            "#,
        )?;
        let file_id = ws.def_file("gamemode/init.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup.value.contains("input: string"),
            "expected inferred hook callback parameter type in hover, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_gm_hook_method_shows_realm_badge_without_description() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "library/lua/includes/extensions/sandbox_hooks.lua",
            r#"
                ---@class SANDBOX
                ---@type SANDBOX
                SANDBOX = SANDBOX or {}

                function SANDBOX:PlayerSpawnSENT(ply, class)
                end
            "#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                function SANDBOX:PlayerSpawnSE<??>NT(ply, class)
                end
            "#,
        )?;
        let file_id = ws.def_file("gamemode/init.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup
                .value
                .contains("![(Shared)](https://github.com/user-attachments/assets/a356f942-57d7-4915-a8cc-559870a980fc)"),
            "expected hover to include shared realm badge without text description, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("**SHARED**"),
            "expected hover to include SHARED label text with realm badge, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("```lua\n(method)"),
            "expected hover to keep syntax-highlighted lua signature, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_badge_prefers_annotation_realm_over_inferred_realm() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@realm client

                ---@class SANDBOX
                ---@type SANDBOX
                SANDBOX = SANDBOX or {}

                ---Annotation should win for badge realm.
                if SERVER then
                    function SANDBOX:PlayerSpawnSE<??>NT(ply, class)
                    end
                end
            "#,
        )?;
        let file_id = ws.def_file("sv_badge_priority.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup
                .value
                .contains("![(Client)](https://github.com/user-attachments/assets/a5f6ba64-374d-42f0-b2f4-50e5c964e808)"),
            "expected client badge from annotation realm precedence, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("**CLIENT**"),
            "expected hover to include CLIENT label text with realm badge, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("```lua\n(method)"),
            "expected hover to keep syntax-highlighted lua signature, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_gm_method_annotation_realm_overrides_shared_file_realm() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "library/lua/includes/extensions/sh_meta_hooks.lua",
            r#"
                ---@class GM
                ---@type GM
                GM = GM or {}

                ---@realm client
                function GM:AnnotatedMetaHook(ply)
                end
            "#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                function GM:AnnotatedMetaHo<??>ok(ply)
                end
            "#,
        )?;
        let file_id = ws.def_file("gamemode/shared.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup
                .value
                .contains("![(Client)](https://github.com/user-attachments/assets/a5f6ba64-374d-42f0-b2f4-50e5c964e808)"),
            "expected CLIENT badge from declaration annotation, got: {}",
            markup.value
        );
        assert!(
            !markup
                .value
                .contains("![(Shared)](https://github.com/user-attachments/assets/a356f942-57d7-4915-a8cc-559870a980fc)"),
            "did not expect SHARED badge when declaration has ---@realm client, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("**CLIENT**"),
            "expected CLIENT label text with realm badge, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_table_method_annotation_realm_overrides_shared_file_realm() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@class testTbl
                ---@type testTbl
                testTbl = testTbl or {}

                if SERVER then
                    ---@realm client
                    function testTbl:TestMe<??>thod()
                    end
                end
            "#,
        )?;
        let file_id = ws.def_file("sh_test_tbl.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup
                .value
                .contains("![(Client)](https://github.com/user-attachments/assets/a5f6ba64-374d-42f0-b2f4-50e5c964e808)"),
            "expected CLIENT badge for annotated table method, got: {}",
            markup.value
        );
        assert!(
            !markup
                .value
                .contains("![(Shared)](https://github.com/user-attachments/assets/a356f942-57d7-4915-a8cc-559870a980fc)"),
            "did not expect SHARED badge for annotated table method, got: {}",
            markup.value
        );
        assert!(
            !markup
                .value
                .contains("![(Server)](https://github.com/user-attachments/assets/d8fbe13a-6305-4e16-8698-5be874721ca1)"),
            "did not expect SERVER badge for annotated table method, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_global_function_annotation_realm_overrides_shared_file_realm() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                if SERVER then
                    ---@realm client
                    function TestFun<??>ction()
                    end
                end
            "#,
        )?;
        let file_id = ws.def_file("sh_global_function.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup
                .value
                .contains("![(Client)](https://github.com/user-attachments/assets/a5f6ba64-374d-42f0-b2f4-50e5c964e808)"),
            "expected CLIENT badge for annotated global function, got: {}",
            markup.value
        );
        assert!(
            !markup
                .value
                .contains("![(Shared)](https://github.com/user-attachments/assets/a356f942-57d7-4915-a8cc-559870a980fc)"),
            "did not expect SHARED badge for annotated global function, got: {}",
            markup.value
        );
        assert!(
            !markup
                .value
                .contains("![(Server)](https://github.com/user-attachments/assets/d8fbe13a-6305-4e16-8698-5be874721ca1)"),
            "did not expect SERVER badge for annotated global function, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_variable_with_comment_does_not_show_realm_badge() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---Variable docs should not gain realm badges.
                local testVa<??>r = 123
            "#,
        )?;
        let file_id = ws.def_file("gamemode/shared.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup
                .value
                .contains("Variable docs should not gain realm badges"),
            "expected hover to include variable description, got: {}",
            markup.value
        );
        assert!(
            !markup
                .value
                .contains("![(Shared)](https://github.com/user-attachments/assets/a356f942-57d7-4915-a8cc-559870a980fc)"),
            "did not expect SHARED badge for variable hover, got: {}",
            markup.value
        );
        assert!(
            !markup
                .value
                .contains("![(Server)](https://github.com/user-attachments/assets/d8fbe13a-6305-4e16-8698-5be874721ca1)"),
            "did not expect SERVER badge for variable hover, got: {}",
            markup.value
        );
        assert!(
            !markup
                .value
                .contains("![(Client)](https://github.com/user-attachments/assets/a5f6ba64-374d-42f0-b2f4-50e5c964e808)"),
            "did not expect CLIENT badge for variable hover, got: {}",
            markup.value
        );
        assert!(
            !markup.value.contains("**SHARED**")
                && !markup.value.contains("**SERVER**")
                && !markup.value.contains("**CLIENT**"),
            "did not expect realm label text for variable hover, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_dynamic_field_uses_field_style_output() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        check!(ws.check_hover(
            r#"
                ---@class HoverDyn.Entity

                ---@type HoverDyn.Entity
                local ent
                ent.testVar = true

                local x = ent.te<??>stVar
            "#,
            VirtualHoverResult {
                value: "```lua\n(infer) testVar: true\n```".to_string(),
            },
        ));

        Ok(())
    }

    /// Hovering the `function` keyword in a hook.Add callback should show the anonymous callback
    /// signature (e.g. `function(ply: Player, seat: Vehicle) -> boolean`) and NOT the generic
    /// "The function keyword is used to define a function..." keyword docs.
    #[gtest]
    fn test_hover_hook_add_callback_function_keyword_shows_hook_signature() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@class Player
                ---@class Vehicle

                ---@class GM
                ---@type GM
                GM = GM or {}

                ---Called when a player tries to enter a vehicle.
                ---@param ply Player
                ---@param veh Vehicle
                ---@return boolean
                function GM:CanPlayerEnterVehicle(ply, veh) end

                hook = {}
                ---@param eventName string
                ---@param identifier any
                ---@param func function
                function hook.Add(eventName, identifier, func) end

                hook.Add("CanPlayerEnterVehicle", "test", fu<??>nction(ply, veh) end)
            "#,
        )?;
        let file_id = ws.def_file("gamemode/init.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        // Must NOT show keyword docs
        assert!(
            !markup.value.contains("The function keyword"),
            "hover should not show keyword docs for hook callback `function`, got: {}",
            markup.value
        );

        // Must show an anonymous callback signature with param types
        assert!(
            markup.value.contains("function("),
            "hover should show anonymous function signature, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("Player"),
            "hover should show Player param type in callback signature, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("Vehicle"),
            "hover should show Vehicle param type in callback signature, got: {}",
            markup.value
        );

        // Must show the hook description
        assert!(
            markup
                .value
                .contains("Called when a player tries to enter a vehicle"),
            "hover should include hook description, got: {}",
            markup.value
        );

        // Must show the return type from the hook's @return annotation
        assert!(
            markup.value.contains("-> boolean"),
            "hover should show `-> boolean` return type from hook annotation, got: {}",
            markup.value
        );

        Ok(())
    }

    /// Hovering the `function` keyword in a hook.Add callback should include the hook's return
    /// type in the anonymous signature (e.g. `function(...) -> boolean`).
    #[gtest]
    fn test_hover_hook_add_callback_function_keyword_includes_return_type() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@class Player

                ---@class GM
                ---@type GM
                GM = GM or {}

                ---@param ply Player
                ---@return boolean
                function GM:PlayerConnect(ply) end

                hook = {}
                ---@param eventName string
                ---@param identifier any
                ---@param func function
                function hook.Add(eventName, identifier, func) end

                hook.Add("PlayerConnect", "test", fu<??>nction(ply) end)
            "#,
        )?;
        let file_id = ws.def_file("gamemode/init.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup.value.contains("-> boolean"),
            "hover should include return type `-> boolean` in anonymous callback signature, got: {}",
            markup.value
        );

        Ok(())
    }

    /// Regression: hovering the hook-name string in hook.Add should still work correctly.
    #[gtest]
    fn test_hover_hook_add_string_still_works_after_callback_hover_fix() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@class GM
                ---@type GM
                GM = GM or {}

                ---@param ply Player
                ---@return boolean
                function GM:SomeHook(ply) end

                hook = {}
                ---@param eventName string
                ---@param identifier any
                ---@param func function
                function hook.Add(eventName, identifier, func) end

                hook.Add("SomeHo<??>ok", "test", function(ply) end)
            "#,
        )?;
        let file_id = ws.def_file("gamemode/init.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup.value.contains("SomeHook"),
            "hook-name hover should still show hook function signature, got: {}",
            markup.value
        );

        Ok(())
    }

    /// When `gmod.enabled` is false, hovering the `function` keyword in a hook.Add call
    /// should fall back to the generic keyword documentation.
    #[gtest]
    fn test_hover_hook_add_callback_function_keyword_fallback_when_gmod_disabled() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = false;
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@class GM
                ---@type GM
                GM = GM or {}

                ---@param ply Player
                ---@return boolean
                function GM:SomeHook(ply) end

                hook = {}
                ---@param eventName string
                ---@param identifier any
                ---@param func function
                function hook.Add(eventName, identifier, func) end

                hook.Add("SomeHook", "test", fu<??>nction(ply) end)
            "#,
        )?;
        let file_id = ws.def_file("gamemode/init.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        // With gmod disabled, should show generic keyword hover, not hook-specific hover.
        // The keyword docs contain the specific phrase from the locale file.
        assert!(
            markup
                .value
                .contains("The `function` keyword is used to define a function"),
            "expected generic keyword docs when gmod is disabled, got: {}",
            markup.value
        );
        // Should NOT show hook-specific anonymous signature (no param types)
        assert!(
            !markup.value.contains("Player"),
            "expected no hook-specific param types when gmod is disabled, got: {}",
            markup.value
        );

        Ok(())
    }

    /// A `function` keyword that is NOT inside a hook.Add callback (e.g. a standalone named
    /// function) should still show generic keyword docs even when `gmod.enabled = true`.
    #[gtest]
    fn test_hover_function_keyword_outside_hook_add_shows_keyword_docs() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                fu<??>nction standalone()
                end
            "#,
        )?;
        let file_id = ws.def_file("gamemode/init.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup
                .value
                .contains("The `function` keyword is used to define a function"),
            "standalone `function` keyword should show generic keyword docs, got: {}",
            markup.value
        );

        Ok(())
    }

    /// Hovering the `function` keyword in hook.Add where the hook name is not registered in any
    /// GM/GAMEMODE table should fall back to generic keyword docs (not crash or return no hover).
    #[gtest]
    fn test_hover_hook_add_callback_function_keyword_unregistered_hook_falls_back_to_keyword_docs()
    -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                hook = {}
                function hook.Add(eventName, identifier, func) end

                -- "NonExistentHook" is not defined on GM/GAMEMODE, so no hook doc is available.
                hook.Add("NonExistentHook", "test", fu<??>nction(ply) end)
            "#,
        )?;
        let file_id = ws.def_file("gamemode/init.lua", &content);
        // When the hook is not registered, hover_gmod_hook_callback_function returns None and
        // the dispatch falls through to the generic keyword hover — always Some(...).
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected keyword fallback hover for unregistered hook")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };
        // Should show generic keyword docs, not hook-specific content.
        assert!(
            markup
                .value
                .contains("The `function` keyword is used to define a function"),
            "unregistered hook should show generic keyword docs, got: {}",
            markup.value
        );
        Ok(())
    }

    /// A hook declared with only `@return` and no `@param` annotations must still show
    /// the return type in the anonymous callback signature (e.g. `function() -> boolean`).
    /// Previously `filter_signature_type` would skip the signature when `param_docs` is empty,
    /// silently degrading return-only hooks to keyword docs.
    #[gtest]
    fn test_hover_hook_add_callback_function_keyword_return_only_hook_shows_return_type()
    -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@class GM
                ---@type GM
                GM = GM or {}

                ---@return boolean
                function GM:ReturnOnlyHook() end

                hook = {}
                ---@param eventName string
                ---@param identifier any
                ---@param func function
                function hook.Add(eventName, identifier, func) end

                hook.Add("ReturnOnlyHook", "test", fu<??>nction() end)
            "#,
        )?;
        let file_id = ws.def_file("gamemode/init.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover for return-only hook")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };
        assert!(
            markup.value.contains("-> boolean"),
            "return-only hook should show `-> boolean` in anonymous callback signature, got: {}",
            markup.value
        );
        assert!(
            !markup
                .value
                .contains("The `function` keyword is used to define a function"),
            "should not fall back to generic keyword docs, got: {}",
            markup.value
        );
        Ok(())
    }
}
