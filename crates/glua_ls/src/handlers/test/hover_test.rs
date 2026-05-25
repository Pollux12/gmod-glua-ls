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
    fn test_hover_outparam_updates_trace_output_field() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class TraceResult
                ---@field Hit boolean
                local TraceResult = {}

                util = {}

                ---@outparam traceConfig.output TraceResult
                ---@param traceConfig table
                function util.TraceLine(traceConfig) end

                local ray = {}
                local traceData = {
                    output = ray,
                }

                util.TraceLine(traceData)

                local hit = traceData.<??>output.Hit
            "#,
            VirtualHoverResult {
                value: "```lua\n(field) output: TraceResult\n```".to_string(),
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
    fn test_hover_undefined_global_isstring_guard_narrows_to_string() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new_with_init_std_lib();
        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                if isstring(testVar2) then ---@diagnostic disable-line: undefined-global
                    print(<??>testVar2) ---@diagnostic disable-line: undefined-global
                end
            "#,
        )?;
        let file_id = ws.def(&content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup.value.contains("string"),
            "expected hover to narrow to string, got: {}",
            markup.value
        );
        assert!(
            !markup.value.contains("any"),
            "expected hover to avoid any after narrowing, got: {}",
            markup.value
        );
        assert!(
            !markup.value.contains("unknown"),
            "expected hover to avoid unknown after narrowing, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_undefined_global_field_access_shows_nil_not_any() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new_with_init_std_lib();
        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                print(test.<??>meow) ---@diagnostic disable-line: undefined-global
            "#,
        )?;
        let file_id = ws.def(&content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup.value.contains("nil"),
            "expected hover to show nil for invalid field access, got: {}",
            markup.value
        );
        assert!(
            !markup.value.contains("any"),
            "expected hover to avoid any for invalid field access, got: {}",
            markup.value
        );
        assert!(
            !markup.value.contains("unknown"),
            "expected hover to avoid unknown for invalid field access, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_invalid_boolean_field_access_shows_nil_not_any() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new_with_init_std_lib();
        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                local test = true
                print(test.<??>meow)
            "#,
        )?;
        let file_id = ws.def(&content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup.value.contains("nil"),
            "expected hover to show nil for invalid field access, got: {}",
            markup.value
        );
        assert!(
            !markup.value.contains("any"),
            "expected hover to avoid any for invalid field access, got: {}",
            markup.value
        );
        assert!(
            !markup.value.contains("unknown"),
            "expected hover to avoid unknown for invalid field access, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_plain_table_missing_field_shows_nil() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new_with_init_std_lib();
        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                local test = {}
                print(test.<??>meow)
            "#,
        )?;
        let file_id = ws.def(&content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;

        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup.value.contains("nil"),
            "expected hover to show nil for unresolved plain table lookup, got: {}",
            markup.value
        );

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
    fn test_source_tag_renders_clickable_link() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                --- Description
                ---
                ---@source https://wiki.facepunch.com/gmod/Entity:SetPos
                local function te<??>st() end
            "#,
            VirtualHoverResult {
                value: "```lua\nlocal function test()\n```\n\n---\n\nDescription\n\n---\n\n**Source:** <https://wiki.facepunch.com/gmod/Entity:SetPos>".to_string(),
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
                value: "```lua\n(field) testVar: true\n```\n\n&nbsp;&nbsp;in class `HoverDyn.Entity`"
                    .to_string(),
            },
        ));

        Ok(())
    }

    #[gtest]
    fn test_hover_dynamic_field_for_metatable_instance() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        check!(ws.check_hover(
            r#"
                local LOCATION = {}
                LOCATION.__index = LOCATION

                function LOCATION:Init()
                    local instance = {}
                    setmetatable(instance, self)
                    instance._OriginalName = true
                    return instance
                end

                function LOCATION:GetOriginalName()
                    return self._Origi<??>nalName
                end
            "#,
            VirtualHoverResult {
                value: "```lua\n(infer) _OriginalName: true\n```".to_string(),
            },
        ));

        Ok(())
    }

    #[gtest]
    fn test_hover_dynamic_field_respects_file_scope() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        emmyrc.gmod.dynamic_fields_global = false;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "assign.lua",
            "---@class HoverDynScoped.Entity\n---@type HoverDynScoped.Entity\nlocal ent\nent.testVar = true\n",
        );
        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@type HoverDynScoped.Entity
                local ent2
                local x = ent2.te<??>stVar
            "#,
        )?;
        let file_id = ws.def_file("use.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;
        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };
        assert!(
            !markup.value.contains("(infer) testVar"),
            "dynamic field hover should not leak across files when dynamic_fields_global=false, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_dynamic_field_for_tableof() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@class HoverTbl.Entity
                local HoverTbl = {}

                ---@return tableof<self>
                function HoverTbl:GetTable() end

                function HoverTbl:Init()
                    local tbl = self:GetTable()
                    tbl.customData = true
                    return tbl.cus<??>tomData
                end
            "#,
        )?;
        let file_id = ws.def(&content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;
        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };
        assert!(
            markup.value.contains("customData"),
            "expected tableof dynamic field hover to resolve field name, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("HoverTbl.Entity"),
            "expected tableof dynamic field hover to stay on class context, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_dynamic_table_field_assignment_prefers_concrete_string_over_any() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r##"
                ---@return table|nil
                local function FromJSON()
                end

                ---@return table
                local function ReadTable()
                    return FromJSON() or {}
                end

                ---@param value string
                ---@return string
                local function FirstChar(value)
                    return value
                end

                ---@param phrase string
                ---@return string
                local function GetPhrase(phrase)
                    return phrase
                end

                local data = ReadTable()

                if FirstChar(data.text) == "#" then
                    data.te<??>xt = GetPhrase(data.text)
                end
            "##,
        )?;
        let file_id = ws.def(&content);
        let value = extract_hover_markdown(&ws, file_id, position);
        assert!(
            value.contains("text: string"),
            "expected concrete string hover for dynamic table field assignment, got: {}",
            value
        );
        assert!(
            !value.contains("any"),
            "expected hover to avoid retaining open-table any in the field type, got: {}",
            value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_dynamic_table_field_read_before_assignment_stays_open_table_any() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r##"
                ---@return table|nil
                local function FromJSON()
                end

                ---@return table
                local function ReadTable()
                    return FromJSON() or {}
                end

                ---@param value string
                ---@return string
                local function FirstChar(value)
                    return value
                end

                ---@param phrase string
                ---@return string
                local function GetPhrase(phrase)
                    return phrase
                end

                local data = ReadTable()

                if FirstChar(data.text) == "#" then
                    data.text = GetPhrase(data.te<??>xt)
                end
            "##,
        )?;
        let file_id = ws.def(&content);
        let value = extract_hover_markdown(&ws, file_id, position);
        assert!(
            value.contains("any?"),
            "expected pre-assignment open-table field read to stay broad, got: {}",
            value
        );
        assert!(
            !value.contains("string"),
            "expected future assignment not to type a prior field read, got: {}",
            value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_dynamic_table_field_call_arg_before_assignment_stays_open_table_any() -> Result<()>
    {
        let mut ws = ProviderVirtualWorkspace::new();

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r##"
                util = {}

                ---@return table?
                function util.JSONToTable(s)
                end

                ---@return table
                local function ReadTable()
                    return util.JSONToTable("") or {}
                end

                ---@param value string
                ---@return string
                local function FirstChar(value)
                    return value
                end

                ---@param phrase string
                ---@return string
                local function GetPhrase(phrase)
                    return phrase
                end

                local data = ReadTable()

                if FirstChar(data.te<??>xt) == "#" then
                    data.text = GetPhrase(data.text)
                end
            "##,
        )?;
        let file_id = ws.def(&content);
        let value = extract_hover_markdown(&ws, file_id, position);
        assert!(
            value.contains("any?"),
            "expected call-site pre-assignment field read to stay broad, got: {}",
            value
        );
        assert!(
            !value.contains("string"),
            "expected future assignment not to type the earlier call argument, got: {}",
            value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_glide_readtable_field_call_arg_stays_open_table_any_after_reindex() -> Result<()>
    {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        let (network_content, position) = ProviderVirtualWorkspace::handle_file_content(
            r##"
                Glide.NetCommands = Glide.NetCommands or {}
                local commands = Glide.NetCommands

                commands[Glide.CMD_NOTIFY] = function()
                    local data = Glide.ReadTable()

                    if string.sub(data.te<??>xt, 1, 1) == "#" then
                        data.text = language.GetPhrase(data.text)
                    end
                end
            "##,
        )?;

        ws.def_files(vec![
            (
                "lua/autorun/sh_glide.lua",
                r#"
                ---@class Glide
                Glide = Glide or {}
                Glide.CMD_NOTIFY = 1

                function Glide.FromJSON(s)
                    if type(s) ~= "string" or s == "" then
                        return {}
                    end

                    return util.JSONToTable(s) or {}
                end
                "#,
            ),
            (
                "lua/glide/sh_network.lua",
                r#"
                function Glide.ReadTable()
                    local data = net.ReadData(1)
                    return Glide.FromJSON(data)
                end
                "#,
            ),
            (
                "lua/includes/util.lua",
                r#"
                util = {}

                ---@return table?
                function util.JSONToTable(json)
                end
                "#,
            ),
            (
                "lua/includes/net.lua",
                r#"
                net = {}

                ---@return string
                function net.ReadData(length)
                end
                "#,
            ),
            (
                "lua/includes/language.lua",
                r#"
                language = {}

                ---@param phrase string
                ---@return string
                function language.GetPhrase(phrase)
                end
                "#,
            ),
            (
                "lua/includes/string.lua",
                r#"
                string = {}

                ---@param s string
                ---@param i integer
                ---@param j integer?
                ---@return string
                function string.sub(s, i, j)
                end
                "#,
            ),
            ("lua/glide/client/network.lua", &network_content),
        ]);
        let uri = ws
            .virtual_url_generator
            .new_uri("lua/glide/client/network.lua");
        let file_id = ws
            .analysis
            .get_file_id(&uri)
            .expect("expected network.lua file id");

        let initial = extract_hover_markdown(&ws, file_id, position);
        assert!(
            initial.contains("any?"),
            "expected initial hover to stay broad, got: {}",
            initial
        );

        ws.analysis
            .update_file_text_only(&uri, format!("{network_content}\n"));

        ws.analysis.reindex_files(vec![file_id]);

        let after_reindex = extract_hover_markdown(&ws, file_id, position);
        assert!(
            after_reindex.contains("any?"),
            "expected post-reindex hover to stay broad, got: {}",
            after_reindex
        );
        assert!(
            !after_reindex.contains("string"),
            "expected future assignment not to type the earlier call argument after reindex, got: {}",
            after_reindex
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_table_field_rhs_does_not_use_lhs_assignment_member() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r##"
                ---@param phrase string
                ---@return string
                local function GetPhrase(phrase)
                    return phrase
                end

                ---@return table<any, any>
                local function ReadTable()
                    return {}
                end

                local data = ReadTable()
                data.text = GetPhrase(data.te<??>xt)
            "##,
        )?;
        let file_id = ws.def_file("lua/glide/client/network.lua", &content);

        let hover = extract_hover_markdown(&ws, file_id, position);
        assert!(
            hover.contains("any"),
            "expected RHS read not to use the LHS assignment member, got: {}",
            hover
        );
        assert!(
            !hover.contains("string"),
            "expected RHS read to remain broad before assignment value is applied, got: {}",
            hover
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_class_field_read_does_not_use_later_same_file_assignment() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r##"
                ---@class ReindexData

                ---@type ReindexData
                local data = {}

                local before = data.te<??>xt
                data.text = "later"
            "##,
        )?;
        let file_name = "lua/glide/client/network.lua";
        let file_id = ws.def_file(file_name, &content);

        let initial = extract_hover_markdown(&ws, file_id, position);
        assert!(
            !initial.contains("(field)") && !initial.contains("string"),
            "expected initial hover not to use the later assignment, got: {}",
            initial
        );

        let uri = ws.virtual_url_generator.new_uri(file_name);
        ws.analysis
            .update_file_text_only(&uri, format!("{content}\n"));
        ws.analysis.reindex_files(vec![file_id]);

        let after_reindex = extract_hover_markdown(&ws, file_id, position);
        assert!(
            !after_reindex.contains("(field)") && !after_reindex.contains("string"),
            "expected post-reindex hover not to use the later assignment, got: {}",
            after_reindex
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_dynamic_table_field_read_after_assignment_uses_assigned_string() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r##"
                ---@return table|nil
                local function FromJSON()
                end

                ---@return table
                local function ReadTable()
                    return FromJSON() or {}
                end

                ---@param value string
                ---@return string
                local function GetPhrase(value)
                    return value
                end

                local data = ReadTable()
                data.text = GetPhrase(data.text)
                local text = data.te<??>xt
            "##,
        )?;
        let file_id = ws.def(&content);
        let value = extract_hover_markdown(&ws, file_id, position);
        assert!(
            value.contains("string"),
            "expected post-assignment field read to use assigned string type, got: {}",
            value
        );
        assert!(
            !value.contains("any"),
            "expected post-assignment field read not to retain open-table any, got: {}",
            value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_branch_initialized_indexed_record_assignment() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/autorun/sh_glide.lua",
            r#"
            ---@class Glide
            Glide = Glide or {}
        "#,
        );
        ws.def_file(
            "lua/glide/sh_network.lua",
            r#"
            Glide.DebugNetwork = Glide.DebugNetwork or {}

            if CLIENT then
                local DebugNet = Glide.DebugNetwork

                function DebugNet.ReadSnapshot()
                    local entId = net.ReadUInt(16)
                    local vehicleId = nil
                    local fields = {}
                    return entId, vehicleId, fields
                end
            end
        "#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
            local DebugNet = Glide.DebugNetwork

            local function receive()
                local entId, vehicleId, fields = DebugNet.ReadSnapshot()
                if not entId then return end

                Glide.DebugSnapshots = Glide.DebugSnapshots or {}
                local rec = Glide.DebugSnapshots[entId]
                if not rec then
                    re<??>c = { data = {}, t = SysTime() }
                    Glide.DebugSnapshots[entId] = rec
                end

                local data = rec.data
                print(data, rec)
            end
        "#,
        )?;
        let file_id = ws.def_file("lua/glide/client/network.lua", &content);
        let value = extract_hover_markdown(&ws, file_id, position);
        assert!(
            value.contains("data"),
            "expected assignment hover to include record table shape, got: {}",
            value
        );
        assert!(
            !value.contains("any"),
            "expected assignment hover not to collapse record table to any, got: {}",
            value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_dynamic_key_table_shape_omits_unnamed_wildcard_value_row() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@class Vector
                local Vector = {}
                ---@return Vector
                function _G.Vector() end

                net = {}
                ---@return string
                function net.ReadString() end

                local function readValue()
                    if flag == 1 then
                        return Vector()
                    end

                    if flag == 2 then
                        return "text"
                    end

                    return 1
                end

                local rec = { data = {}, t = 0 }
                local data = rec.data
                data.vehicle = 1

                local key = net.ReadString()
                data[key] = readValue()

                local d = rec.da<??>ta
                print(d)
            "#,
        )?;
        let file_id = ws.def_file("lua/glide/client/network.lua", &content);
        let value = extract_hover_markdown(&ws, file_id, position);

        assert!(
            value.contains("vehicle"),
            "expected hover to retain concrete named fields, got: {value}"
        );
        assert!(
            !value.contains("\n    ("),
            "dynamic-key value type should not render as an unnamed table field, got: {value}"
        );
        assert!(
            value.contains("[string]"),
            "expected dynamic string-key values to render as an indexed table entry, got: {value}"
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_local_gettable_call_uses_scoped_receiver_type() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(&dedent(
            r#"
                    ---@class Entity
                    local Entity = {}

                    ---@return tableof<self>
                    function Entity:GetTable() end

                    ---@generic T : table
                    ---@param metaName `T`
                    ---@return T
                    function FindMetaTable(metaName) end

                    local getTable = FindMetaTable("Entity").GetTable

                    function ENT:Think(selfTbl)
                        selfTbl = selfTbl or getTa<??>ble(self)
                    end
                "#,
        ))?;
        let file_id = ws.def_file("cityrp/entities/entities/glide_wheel/init.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;
        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup.value.contains("glide_wheel:GetTable"),
            "expected aliased GetTable hover to use scripted class receiver, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("tableof<glide_wheel>"),
            "expected aliased GetTable hover to specialize return type, got: {}",
            markup.value
        );
        assert!(
            !markup.value.contains("tableof<Entity>"),
            "expected aliased GetTable hover to avoid base Entity return type, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_local_gettable_nested_state_field_keeps_declared_type() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(&dedent(
            r#"
                    ---@class Entity
                    local Entity = {}

                    ---@return tableof<self>
                    function Entity:GetTable() end

                    ---@generic T : table
                    ---@param metaName `T`
                    ---@return T
                    function FindMetaTable(metaName) end

                    ---@class GlideTraceData
                    ---@field start number

                    ---@class GlideWheelState
                    ---@field traceData GlideTraceData

                    local getTable = FindMetaTable("Entity").GetTable

                    function ENT:Initialize()
                        ---@type GlideWheelState
                        self.state = {
                            traceData = {
                                start = 1,
                            },
                        }
                    end

                    function ENT:Think(selfTbl)
                        selfTbl = selfTbl or getTable(self)
                        local traceData = selfTbl.state.traceData
                        return tra<??>ceData
                    end
                "#,
        ))?;
        let file_id = ws.def_file("cityrp/entities/entities/glide_wheel/init.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;
        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup.value.contains("GlideTraceData"),
            "expected nested traceData hover to keep declared type, got: {}",
            markup.value
        );
        assert!(
            !markup.value.contains("never"),
            "expected nested traceData hover to avoid never, got: {}",
            markup.value
        );
        assert!(
            !markup.value.contains(": nil"),
            "expected nested traceData hover to avoid nil, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_local_gettable_state_field_keeps_typed_state() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(&dedent(
            r#"
                    ---@class Entity
                    local Entity = {}

                    ---@return tableof<self>
                    function Entity:GetTable() end

                    ---@generic T : table
                    ---@param metaName `T`
                    ---@return T
                    function FindMetaTable(metaName) end

                    ---@class GlideTraceData
                    ---@field start number

                    ---@class GlideWheelState
                    ---@field traceData GlideTraceData

                    local getTable = FindMetaTable("Entity").GetTable

                    function ENT:Initialize()
                        ---@type GlideWheelState
                        self.state = {
                            traceData = {
                                start = 1,
                            },
                        }
                    end

                    function ENT:Think(selfTbl)
                        selfTbl = selfTbl or getTable(self)
                        return selfTbl.sta<??>te
                    end
                "#,
        ))?;
        let file_id = ws.def_file("cityrp/entities/entities/glide_wheel/init.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position)
            .ok_or("expected hover")
            .or_fail()?;
        let HoverContents::Markup(markup) = hover.contents else {
            return fail!("expected HoverContents::Markup");
        };

        assert!(
            markup.value.contains("GlideWheelState"),
            "expected state hover to keep GlideWheelState, got: {}",
            markup.value
        );
        assert!(
            !markup.value.contains(": any"),
            "expected state hover to avoid any, got: {}",
            markup.value
        );

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

    fn enable_gmod_workspace() -> ProviderVirtualWorkspace {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws
    }

    fn extract_hover_markdown(
        ws: &ProviderVirtualWorkspace,
        file_id: glua_code_analysis::FileId,
        position: lsp_types::Position,
    ) -> String {
        let hover =
            crate::handlers::hover::hover(&ws.analysis, file_id, position).expect("expected hover");
        let HoverContents::Markup(markup) = hover.contents else {
            panic!("expected HoverContents::Markup");
        };
        markup.value
    }

    #[gtest]
    fn test_hover_net_message_on_net_start_shows_send_and_receive_patterns() -> Result<()> {
        let mut ws = enable_gmod_workspace();

        ws.def_file(
            "lua/autorun/client/recv.lua",
            r#"
                net.Receive("MyMessage", function()
                    local id = net.ReadUInt(16)
                    local name = net.ReadString()
                end)
            "#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                util.AddNetworkString("MyMessage")
                net.Start("MyMes<??>sage")
                net.WriteUInt(1, 16)
                net.WriteString("hello")
                net.Send(Entity(1))
            "#,
        )?;
        let file_id = ws.def_file("lua/autorun/server/send.lua", &content);
        let value = extract_hover_markdown(&ws, file_id, position);

        assert!(
            value.contains("(net) \"MyMessage\""),
            "expected typed header, got: {value}"
        );
        assert!(value.contains("**Senders**"), "got: {value}");
        assert!(value.contains("**Receivers**"), "got: {value}");
        assert!(value.contains("net.WriteUInt(1, 16)"), "got: {value}");
        assert!(value.contains("net.WriteString(\"hello\")"), "got: {value}");
        assert!(value.contains("net.ReadUInt(16)"), "got: {value}");
        assert!(value.contains("net.ReadString"), "got: {value}");
        // File names should appear as clickable links pointing to a line.
        assert!(
            value.contains("send.lua") && value.contains("recv.lua"),
            "expected file names in links, got: {value}"
        );
        assert!(
            value.contains("#L"),
            "expected #L<line> in clickable links, got: {value}"
        );
        Ok(())
    }

    #[gtest]
    fn test_hover_net_message_on_net_receive_shows_info() -> Result<()> {
        let mut ws = enable_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
                util.AddNetworkString("Ping")
                net.Start("Ping")
                net.WriteString("hi")
                net.Send(Entity(1))
            "#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                net.Receive("Pin<??>g", function()
                    local s = net.ReadString()
                end)
            "#,
        )?;
        let file_id = ws.def_file("lua/autorun/client/recv.lua", &content);
        let value = extract_hover_markdown(&ws, file_id, position);

        assert!(
            value.contains("(net) \"Ping\""),
            "expected typed header, got: {value}"
        );
        assert!(value.contains("**Senders**"), "got: {value}");
        assert!(value.contains("**Receivers**"), "got: {value}");
        assert!(value.contains("net.WriteString"), "got: {value}");
        assert!(value.contains("net.ReadString"), "got: {value}");
        Ok(())
    }

    #[gtest]
    fn test_hover_net_message_on_util_add_network_string_shows_info() -> Result<()> {
        let mut ws = enable_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
                net.Start("Registered")
                net.WriteFloat(0.5)
                net.Send(Entity(1))
            "#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                util.AddNetworkString("Regis<??>tered")
            "#,
        )?;
        let file_id = ws.def_file("lua/autorun/server/init.lua", &content);
        let value = extract_hover_markdown(&ws, file_id, position);

        assert!(
            value.contains("(net) \"Registered\""),
            "expected typed header, got: {value}"
        );
        assert!(value.contains("**Senders**"), "got: {value}");
        assert!(value.contains("net.WriteFloat"), "got: {value}");
        Ok(())
    }

    #[gtest]
    fn test_hover_net_message_groups_distinct_send_patterns() -> Result<()> {
        let mut ws = enable_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send_a.lua",
            r#"
                net.Start("MultiPattern")
                net.WriteUInt(1, 16)
                net.WriteString("a")
                net.Send(Entity(1))
            "#,
        );
        ws.def_file(
            "lua/autorun/server/send_b.lua",
            r#"
                net.Start("MultiPattern")
                net.WriteFloat(0.5)
                net.Send(Entity(1))
            "#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                net.Receive("MultiPa<??>ttern", function() end)
            "#,
        )?;
        let file_id = ws.def_file("lua/autorun/client/recv.lua", &content);
        let value = extract_hover_markdown(&ws, file_id, position);

        assert!(
            value.contains("net.WriteUInt"),
            "expected first pattern entry, got: {value}"
        );
        assert!(
            value.contains("net.WriteString"),
            "expected first pattern entry, got: {value}"
        );
        assert!(
            value.contains("net.WriteFloat"),
            "expected second pattern entry, got: {value}"
        );
        // Two distinct send patterns should produce a multi-pattern Senders section.
        assert!(
            value.contains("across 2 patterns"),
            "expected multi-pattern label, got: {value}"
        );
        assert!(
            value.contains("Pattern A") && value.contains("Pattern B"),
            "expected Pattern A and B labels, got: {value}"
        );
        Ok(())
    }

    #[gtest]
    fn test_hover_net_message_marks_dynamic_writes() -> Result<()> {
        let mut ws = enable_gmod_workspace();

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                net.Start("DynLoop<??>")
                for _ = 1, 3 do
                    net.WriteString("x")
                end
                net.Send(Entity(1))
            "#,
        )?;
        let file_id = ws.def_file("lua/autorun/server/send.lua", &content);
        let value = extract_hover_markdown(&ws, file_id, position);

        assert!(
            value.contains("net.WriteString")
                && value.contains("for _ = 1, 3 do")
                && value.contains("end"),
            "expected WriteString nested under the `for ... do` source frame, got: {value}"
        );
        Ok(())
    }

    #[gtest]
    fn test_hover_net_message_nested_loops_via_named_callback() -> Result<()> {
        let mut ws = enable_gmod_workspace();

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                local function InitVars(len)
                    local plyCount = net.ReadUInt(8)
                    for i = 1, plyCount, 1 do
                        local userID = net.ReadUInt(16)
                        local varCount = net.ReadUInt(8)
                        for j = 1, varCount, 1 do
                            local v = net.ReadString()
                        end
                    end
                end
                net.Receive("NestedVars<??>", InitVars)
            "#,
        )?;
        let file_id = ws.def_file("lua/autorun/client/init.lua", &content);
        let value = extract_hover_markdown(&ws, file_id, position);

        assert!(
            value.contains("net.ReadUInt") && value.contains("net.ReadString"),
            "expected reads from both loop levels, got: {value}"
        );
        assert!(
            value.contains("for i = 1, plyCount, 1 do")
                && value.contains("for j = 1, varCount, 1 do"),
            "expected both for-loop headers, got: {value}"
        );
        Ok(())
    }

    #[gtest]
    fn test_hover_net_message_helper_call_inherits_outer_flow() -> Result<()> {
        let mut ws = enable_gmod_workspace();

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                local function readPair()
                    local k = net.ReadString()
                    local v = net.ReadString()
                end
                local function recv(len)
                    local n = net.ReadUInt(8)
                    for i = 1, n, 1 do
                        readPair()
                    end
                end
                net.Receive("HelperLoop<??>", recv)
            "#,
        )?;
        let file_id = ws.def_file("lua/autorun/client/helper.lua", &content);
        let value = extract_hover_markdown(&ws, file_id, position);

        // Reads inside readPair() should appear inside a Lua code fence that
        // follows a styled scope-open row for the outer for-loop — the
        // helper-recursion flow_path-prefix fix carries the call site's loop
        // into the helper body's reads.
        assert!(
            value.contains("net.ReadString") && value.contains("for i = 1, n, 1 do"),
            "expected ReadString and outer for-loop header both rendered, got: {value}"
        );
        let Some(header_idx) = value.find("for i = 1, n, 1 do") else {
            panic!("expected for-loop scope row, got: {value}");
        };
        let Some(read_idx) = value.find("net.ReadString") else {
            panic!("expected ReadString rendered, got: {value}");
        };
        assert!(
            read_idx > header_idx,
            "expected ReadString rendered after the for-loop scope-open row, got: {value}"
        );
        Ok(())
    }

    #[gtest]
    fn test_hover_net_message_no_counterpart_indexed_message() -> Result<()> {
        let mut ws = enable_gmod_workspace();

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                util.AddNetworkString("Lonely<??>")
            "#,
        )?;
        let file_id = ws.def_file("lua/autorun/server/init.lua", &content);
        let value = extract_hover_markdown(&ws, file_id, position);

        assert!(
            value.contains("(net) \"Lonely\""),
            "expected typed header even with no usages, got: {value}"
        );
        assert!(
            value.contains("no recorded usages") || value.contains("No payload patterns indexed"),
            "expected an empty-state hint, got: {value}"
        );
        Ok(())
    }

    #[gtest]
    fn test_hover_net_message_does_not_trigger_for_unrelated_string() -> Result<()> {
        let mut ws = enable_gmod_workspace();

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                local x = "Hel<??>lo"
            "#,
        )?;
        let file_id = ws.def_file("lua/autorun/server/init.lua", &content);
        let hover = crate::handlers::hover::hover(&ws.analysis, file_id, position);
        if let Some(hover) = hover {
            let HoverContents::Markup(markup) = hover.contents else {
                return Ok(());
            };
            assert!(
                !markup.value.contains("(net)"),
                "unrelated string hover should not show net-message info, got: {}",
                markup.value
            );
        }
        Ok(())
    }

    #[gtest]
    fn test_hover_branched_dynamic_field_unions_vector_real_shape() -> Result<()> {
        // Repro of cityrp-vehicle-base/init.lua bug:
        //   if exitPos then
        //       seat.GlideExitPos = Vector(...)
        //   else
        //       seat.GlideExitPos = nil
        //   end
        // Reading `seat.GlideExitPos` later must hover as `Vector?` (i.e. Vector|nil),
        // NOT bare `nil`. Pre-fix, `retain_only_member_for_owner_key` dropped the
        // Vector-branch member because the `= nil` branch ran later.
        let mut ws = enable_gmod_workspace();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.infer_dynamic_fields = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@class Vector
                local Vector = {}
                ---@param x? number
                ---@param y? number
                ---@param z? number
                ---@return Vector
                function _G.Vector(x, y, z) end

                ---@param ent any
                ---@return boolean
                function _G.IsValid(ent) end

                ---@class NULL
                NULL = {}

                _G.ents = {}
                ---@generic T : Entity
                ---@param class `T`
                ---@return T|NULL
                function _G.ents.Create(class) end

                ---@class Entity
                local Entity = {}

                function ENT:CreateSeat(exitPos)
                    local seat = ents.Create("prop_vehicle_prisoner_pod")
                    if not IsValid(seat) then return end
                    if exitPos then
                        seat.GlideExitPos = Vector(exitPos[1], exitPos[2], exitPos[3])
                    else
                        seat.GlideExitPos = nil
                    end
                end

                function ENT:ReadSeat()
                    local seat = ents.Create("prop_vehicle_prisoner_pod")
                    if not IsValid(seat) then return end
                    local v = seat.Glide<??>ExitPos
                    return v
                end
            "#,
        )?;
        let file_id = ws.def_file("lua/entities/base_glide/init.lua", &content);
        let value = extract_hover_markdown(&ws, file_id, position);

        let has_vector = value.contains("Vector");
        let has_nil_marker = value.contains("nil") || value.contains('?');
        assert!(
            has_vector && has_nil_marker,
            "expected hover on read site to include Vector + nil marker, got: {value}"
        );
        Ok(())
    }

    /// Hover at the LHS of `seat.GlideExitPos = nil` (the offending line the
    /// user is hovering on). Pre-fix this displayed `(field) GlideExitPos: nil`
    /// because `get_hover_type` rewrote the displayed type to the RHS being
    /// assigned. Post-fix, `lhs_indexed_member_union_type` recovers the
    /// field's full union from the branched assignments so the hover correctly
    /// shows `Vector?` (i.e. `Vector | nil`).
    #[gtest]
    fn test_hover_branched_dynamic_field_lhs_assign_does_not_collapse_field_type() -> Result<()> {
        let mut ws = enable_gmod_workspace();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.infer_dynamic_fields = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        // Hover directly on the `GlideExitPos` token in the `= nil` branch.
        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@class Vector
                local Vector = {}
                ---@param x? number
                ---@param y? number
                ---@param z? number
                ---@return Vector
                function _G.Vector(x, y, z) end

                ---@param ent any
                ---@return boolean
                function _G.IsValid(ent) end

                ---@class NULL
                NULL = {}

                _G.ents = {}
                ---@generic T : Entity
                ---@param class `T`
                ---@return T|NULL
                function _G.ents.Create(class) end

                ---@class Entity
                local Entity = {}

                function ENT:CreateSeat(exitPos)
                    local seat = ents.Create("prop_vehicle_prisoner_pod")
                    if not IsValid(seat) then return end
                    if exitPos then
                        seat.GlideExitPos = Vector(exitPos[1], exitPos[2], exitPos[3])
                    else
                        seat.Glide<??>ExitPos = nil
                    end
                end
            "#,
        )?;
        let file_id = ws.def_file("lua/entities/base_glide/init.lua", &content);
        let value = extract_hover_markdown(&ws, file_id, position);

        // Post-fix the LHS hover must show the field's full union, not bare
        // `nil` or `never`. Accept either `Vector?` rendering or explicit
        // `Vector | nil` / `Vector|nil`.
        assert!(
            value.contains("GlideExitPos"),
            "hover should include field name, got: {value}"
        );
        assert!(
            value.contains("Vector"),
            "LHS hover must include `Vector` from the other branch, got: {value}"
        );
        let has_nil_marker = value.contains("Vector?")
            || value.contains("Vector | nil")
            || value.contains("Vector|nil")
            || value.contains("nil");
        assert!(
            has_nil_marker,
            "LHS hover must include a nil marker (`?` or `| nil`), got: {value}"
        );
        assert!(
            !value.contains("GlideExitPos: nil") && !value.contains("GlideExitPos: never"),
            "LHS hover must not collapse to bare `nil` or `never`, got: {value}"
        );
        Ok(())
    }

    /// Read-site hover where the entity local comes from `self.seats[index]`
    /// in a different method than the branched assignment. Mirrors
    /// `cityrp-vehicle-base/init.lua:559` (`local seat = self.seats[index]`
    /// then `seat.GlideExitPos[1]`). Failing red test pre-fix produced bare
    /// `nil` or `never` because the branched dynamic-field assignment
    /// collapsed to the last `= nil` branch.
    #[gtest]
    fn test_hover_branched_dynamic_field_read_via_self_seats_array() -> Result<()> {
        let mut ws = enable_gmod_workspace();
        let mut emmyrc = ws.get_emmyrc();
        emmyrc.gmod.infer_dynamic_fields = true;
        emmyrc.gmod.scripted_class_scopes.include = vec![legacy_scope("entities/**")];
        ws.update_emmyrc(emmyrc);

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@class Vector
                local Vector = {}
                ---@param x? number
                ---@param y? number
                ---@param z? number
                ---@return Vector
                function _G.Vector(x, y, z) end

                ---@param ent any
                ---@return boolean
                function _G.IsValid(ent) end

                ---@class NULL
                NULL = {}

                _G.ents = {}
                ---@generic T : Entity
                ---@param class `T`
                ---@return T|NULL
                function _G.ents.Create(class) end

                ---@class Entity
                local Entity = {}

                ---@class base_glide
                ---@field seats prop_vehicle_prisoner_pod[]
                local ENTCLASS = {}

                function ENT:CreateSeat(exitPos, index)
                    local seat = ents.Create("prop_vehicle_prisoner_pod")
                    if not IsValid(seat) then return end
                    if exitPos then
                        seat.GlideExitPos = Vector(exitPos[1], exitPos[2], exitPos[3])
                    else
                        seat.GlideExitPos = nil
                    end
                    self.seats[index] = seat
                end

                function ENT:GetSeatExitPos(index)
                    local seat = self.seats[index]
                    if not IsValid(seat) then return end
                    return seat.Glide<??>ExitPos
                end
            "#,
        )?;
        let file_id = ws.def_file("lua/entities/base_glide/init.lua", &content);
        let value = extract_hover_markdown(&ws, file_id, position);

        let has_vector = value.contains("Vector");
        let has_nil_marker = value.contains("nil") || value.contains('?');
        assert!(
            has_vector && has_nil_marker,
            "real-shape read via self.seats[i] must hover Vector|nil, got: {value}"
        );
        Ok(())
    }
}
