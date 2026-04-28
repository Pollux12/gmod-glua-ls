#[cfg(test)]
mod tests {
    use crate::handlers::test_lib::{ProviderVirtualWorkspace, VirtualHoverResult, check};
    use googletest::prelude::*;
    use lsp_types::HoverContents;

    #[gtest]
    fn test_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@param a number 参数a
                ---@return number a 返回值a
                local function delete(a)
                end

                local delete2 = delete
                local delete3 = delete2
                local <??>delete4 = delete3
            "#,
            VirtualHoverResult {
                value: "```lua\nlocal function delete(a: number)\n  -> a: number\n\n```\n\n---\n\n@*param* `a` — 参数a\n\n@*return* `a`  — 返回值a".to_string(),
            },
        ));

        check!(ws.check_hover(
            r#"
                -- 删除
                ---@param a number 参数a
                ---@return number a 返回值a
                local function delete(a)
                end

                local delete2 = delete
                local delete3 = delete2
                local delete4 = delete3
                local deleteObj = {
                    <??>aaa = delete4
                }
            "#,
            VirtualHoverResult {
                value: "```lua\nlocal function delete(a: number)\n  -> a: number\n\n```\n\n---\n\n删除\n\n@*param* `a` — 参数a\n\n@*return* `a`  — 返回值a".to_string(),
            },
        ));

        check!(ws.check_hover(
            r#"
                ---@param a number 参数a
                ---@return number a 返回值a
                local function delete(a)
                end

                local delete2 = delete
                local delete3 = delete2
                local delete4 = delete3
                local deleteObj = {
                    aa = delete4
                }

                local deleteObj2 = {
                    <??>aa = deleteObj.aa
                }
            "#,
            VirtualHoverResult {
                value: "```lua\nlocal function delete(a: number)\n  -> a: number\n\n```\n\n---\n\n@*param* `a` — 参数a\n\n@*return* `a`  — 返回值a".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_2() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def(
            r#"
                ---@class Game
                ---@field event fun(self: self, owner: "abc"): any 测试1
                ---@field event fun(self: self, owner: "def"): any 测试2
                local Game = {}

                ---说明
                ---@param key string 参数key
                ---@param value string 参数value
                ---@return number ret @返回值
                function Game:add(key, value)
                    self.aaa = 1
                end
            "#,
        );

        check!(ws.check_hover(
            r#"
                ---@type Game
                local game

                local local_a = game.add
                local <??>local_b = local_a
            "#,
            VirtualHoverResult {
                value: "```lua\n(method) Game:add(key: string, value: string)\n  -> ret: number\n\n```\n\n---\n\n说明\n\n@*param* `key` — 参数key\n\n@*param* `value` — 参数value\n\n@*return* `ret`  — 返回值".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_3() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def(
            r#"
                ---@class Hover.Test3<T>
                ---@field event fun(self: self, event: "A", key: T)
                ---@field event fun(self: self, event: "B", key: T)
                local Test3 = {}
            "#,
        );

        check!(ws.check_hover(
            r#"
                ---@type Hover.Test3<string>
                local test3

                local <??>event = test3.event
            "#,
            VirtualHoverResult {
                value: "```lua\n(method) Test3:event(event: \"B\", key: string)\n```\n\n```lua\n(method) Test3:event(event: \"A\", key: string)\n```\n\n&nbsp;&nbsp;in class `Hover.Test3`".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_union_function() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class Trigger
                ---@class EventTypeA

                ---@class (partial) GameA
                local M

                -- 注册引擎事件
                ---@param event_type EventTypeA
                ---@param ... any
                ---@return Trigger
                function M:<??>event(event_type, ...)
                end

                ---@class (partial) GameA
                ---@field event fun(self: self, event: "游戏-初始化"): Trigger
                ---@field event fun(self: self, event: "游戏-追帧完成"): Trigger
                ---@field event fun(self: self, event: "游戏-逻辑不同步"): Trigger
            "#,
            VirtualHoverResult {
                value: "```lua\n(method) GameA:event(event_type: EventTypeA, ...: any) -> Trigger\n```\n\n```lua\n(method) GameA:event(event: \"游戏-初始化\") -> Trigger\n```\n\n```lua\n(method) GameA:event(event: \"游戏-追帧完成\") -> Trigger\n```\n\n```lua\n(method) GameA:event(event: \"游戏-逻辑不同步\") -> Trigger\n```\n\n---\n\n注册引擎事件".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_4() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class ClosureTest
                ---@field e fun(a: string, b: number)
                local Test

                function Test.<??>e(a, b)
                    A = a
                end
            "#,
            VirtualHoverResult {
                value: "```lua\n(field) ClosureTest.e(a: string, b: number)\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_table_field_function_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class T
                ---@field func fun(self:T) 注释注释

                ---@type T
                local t = {
                    func<??> = function(self)

                    end
                }
            "#,
            VirtualHoverResult {
                value: "```lua\n(method) T:func()\n```\n\n---\n\n注释注释".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_issue_499() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class T
                ---@field a string 注释注释a

                ---@type T
                local t = {
                    a<??> = "a"
                }
            "#,
            VirtualHoverResult {
                value: "```lua\n(field) a: string = \"a\"\n```\n\n---\n\n注释注释a".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_issue_499_2() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class T
                ---@field func fun(self:string) 注释注释

                ---@type T
                local t = {
                    fu<??>nc = function(self)
                    end,
                }
            "#,
            VirtualHoverResult {
                value: "```lua\n(field) T.func(self: string)\n```\n\n---\n\n注释注释".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_issue_499_3() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class T
                ---@field func fun(a:string) 注释1
                ---@field func fun(a:number) 注释2

                ---@type T
                local t = {
                    fu<??>nc = function(a)
                    end,
                }
            "#,
            VirtualHoverResult {
                value: "```lua\n(field) T.func(a: (number|string))\n```\n\n```lua\n(field) T.func(a: string)\n```\n\n```lua\n(field) T.func(a: number)\n```\n\n---\n\n注释1\n\n注释2"
                    .to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_issue_499_4() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class T
                ---@field func fun(a:string) 注释1
                ---@field func fun(a:number) 注释2

                ---@type T
                local t = {
                    func = function(a)
                    end
                }

                t.fu<??>nc(1)
            "#,
            VirtualHoverResult {
                value: "```lua\n(field) T.func(a: number)\n```\n\n---\n\n注释2".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_origin_decl_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class T
                ---@field func fun(a:string) 注释1
                ---@field func fun(a:number) 注释2

                ---@type T
                local t = {
                    func = function(a)
                    end
                }
                local ab<??>c = t.func
            "#,
            VirtualHoverResult {
                value: "```lua\n(field) T.func(a: number)\n```\n\n```lua\n(field) T.func(a: string)\n```\n\n---\n\n注释2\n\n注释1".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_first_generic() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class Reactive
                local M

                ---@generic T: table
                ---@param target T
                ---@return T
                function M.reac<??>tive(target)
                end
            "#,
            VirtualHoverResult {
                value: "```lua\nfunction Reactive.reactive(target: T) -> T\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_table_field_function() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                local export = {}
                ---@type fun()
                export.NO<??>OP = function() end
            "#,
            VirtualHoverResult {
                value: "```lua\nfunction export.NOOP()\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_return_union_function() -> Result<()> {
        // temp remove the test
        // let mut ws = ProviderVirtualWorkspace::new();
        // check!(ws.check_hover(
        //     r#"
        //         ---@generic T
        //         ---@param initialValue? T
        //         ---@return (fun(): T) | (fun(value: T))
        //         local function signal(initialValue)
        //         end

        //         ---测试
        //         local cou<??>nt = signal(1)
        //     "#,
        //     VirtualHoverResult {
        //         value: "```lua\nfunction count(value: 1)\n```\n\n---\n\n测试\n\n---\n\n```lua\nfunction count() -> 1\n```".to_string(),
        //     },
        // ));
        Ok(())
    }

    #[gtest]
    fn test_require_function() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "test.lua",
            r#"
                ---测试
                local function signal()
                end

                return {
                    signal = signal
                }
            "#,
        );
        check!(ws.check_hover(
            r#"
                local test = require("test")
                local si<??>gnal = test.signal
            "#,
            VirtualHoverResult {
                value: "```lua\nlocal function signal()\n```\n\n---\n\n测试".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_generic_function() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "test.lua",
            r#"
                ---@class Observable<T>
                local Observable

                ---@generic R
                ---@param selector fun(value: T, index?: integer): R
                function Observable:select(selector)
                end

                ---@type Observable<integer>
                source = {}
            "#,
        );
        check!(ws.check_hover(
            r#"
                source:<??>select(function(value)
                    return value
                end)
            "#,
            VirtualHoverResult {
                value: "```lua\n(method) Observable:select(selector: fun(value: integer, index: integer?) -> integer)\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_other_file_function() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "a.lua",
            r#"
                ---测试
                local function zipLatest(...)
                end
                return zipLatest
            "#,
        );
        check!(ws.check_hover(
            r#"
                local zipLatest = require("a")
                <??>zipLatest()
            "#,
            VirtualHoverResult {
                value: "```lua\nlocal function zipLatest(...)\n```\n\n---\n\n测试".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_hover_generic_function_params_description() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "a.lua",
            r#"
                ---@class RingBuffer<T>
                local RingBuffer = {}

                ---@param index integer 索引
                ---@return T? item
                function RingBuffer:get(index)
                end
            "#,
        );
        check!(ws.check_hover(
            r#"
                ---@type RingBuffer<string>
                local RingBuffer
                RingBuffer:<??>get(1)
            "#,
            VirtualHoverResult {
                value: "```lua\n(method) RingBuffer:get(index: integer) -> string?\n```\n\n---\n\n@*param* `index` — 索引".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_annotation_search() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "a.lua",
            r#"
                ---@version 5.4
                ---测试
                function test()
                end
            "#,
        );
        check!(ws.check_hover(
            r#"
                <??>test()
            "#,
            VirtualHoverResult {
                value: "```lua\nfunction test()\n```\n\n---\n\n测试".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_field_remove_first() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class A<T>
                ---@field next fun(value: T) # 测试
                local A = {}

                A.<??>next()
            "#,
            VirtualHoverResult {
                value: "```lua\n(field) A.next(value: T)\n```\n\n---\n\n测试".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_first_strtpl() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class Fix
                local Fix

                ---@generic T
                ---@param  name `T`
                function Fix.ad<??>d(name)
                end
            "#,
            VirtualHoverResult {
                value: "```lua\nfunction Fix.add(name: T)\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_call_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class A
                local A
                ---@class B
                local B

                ---@generic T
                ---@param x T
                function A.add(x)
                end

                A.ad<??>d(B)
            "#,
            VirtualHoverResult {
                value: "```lua\nfunction A.add(x: B)\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_fix_method_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
            ---@class ClassControl
            local ClassControl = {}

            ---@generic T
            ---@param name `T`|T
            function ClassControl.ne<??>w(name)
            end
            "#,
            VirtualHoverResult {
                value: "```lua\nfunction ClassControl.new(name: T)\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_fix_global_index_function_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
            M = {}
            function M.te<??>st()
            end

            "#,
            VirtualHoverResult {
                value: "```lua\nfunction M.test()\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_fix_global_index_function_2() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        // TODO: 构建完整的访问路径
        check!(ws.check_hover(
            r#"
            M = {
                K = {}
            }
            M.K.<??>Value = function()
            end
            "#,
            VirtualHoverResult {
                value: "```lua\nfunction Value()\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_fix_ref() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Player
            ---@field name string

            ---@param player Player
            function CreatePlayer(player)
            end
        "#,
        );
        check!(ws.check_hover(
            r#"
            Creat<??>ePlayer({name = "John"})
            "#,
            VirtualHoverResult {
                value: "```lua\nfunction CreatePlayer(player: Player)\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_intersection_type() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def(
            r#"
                ---@class Matchers<T>
                ---@field toBe fun(self: self, expected: any)

                ---@class Assertions<T>: Matchers<T> & number
                Assertions = {}
        "#,
        );
        check!(ws.check_hover(
            r#"
            Assertions:to<??>Be(1)
            "#,
            VirtualHoverResult {
                value: "```lua\n(method) Matchers:toBe(expected: any)\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_table_const_method() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
            local M = {}

            ---@param x number
            function M:abc<??>d(x)
            end

            M:abcd(1)
            "#,
            VirtualHoverResult {
                value: "```lua\n(method) M:abcd(x: number)\n```".to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_inherited_method_docs_on_override() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_hover(
            r#"
                ---@class Entity
                local Entity = {}

                ---Called when touched by another entity.
                ---@param other Entity The touching entity.
                function Entity:Touch(other)
                end

                ---@class MyEntity: Entity
                local ENT = {}

                function ENT:To<??>uch(other)
                end
            "#,
            VirtualHoverResult {
                value: "```lua\n(method) MyEntity:Touch(other: Entity)\n```\n\n```lua\n(method) Entity:Touch(other: Entity)\n```\n\n---\n\nCalled when touched by another entity.\n\n@*param* `other` — The touching entity."
                    .to_string(),
            },
        ));
        Ok(())
    }

    #[gtest]
    fn test_require_module_member_rich_hover() -> Result<()> {
        // Regression test for required module hovers using the module export owner.
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "netstream.lua",
            r#"
                ---@class NetStream
                local NetStream = {}

                ---Send data over the network stream
                ---@param channel string The channel to send on
                ---@param data table The data to send
                ---@return boolean success Whether the send succeeded
                function NetStream:Send(channel, data)
                end

                return NetStream
            "#,
        );

        check!(ws.check_hover(
            r#"
                local netstream = require("netstream")
                local <??>ns = netstream
            "#,
            VirtualHoverResult {
                value: "```lua\nlocal ns: NetStream {\n    Send: function,\n}\n```".to_string(),
            },
        ));

        check!(ws.check_hover(
            r#"
                local netstream = require("netstream")
                local send_<??>fn = netstream.Send
            "#,
            VirtualHoverResult {
                value: "```lua\n(method) NetStream:Send(channel: string, data: table)\n  -> success: boolean\n\n```\n\n---\n\nSend data over the network stream\n\n@*param* `channel` — The channel to send on\n\n@*param* `data` — The data to send\n\n@*return* `success`  — Whether the send succeeded".to_string(),
            },
        ));

        Ok(())
    }

    #[gtest]
    fn test_hover_global_alias_preserves_table_identity() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new_with_init_std_lib();
        ws.def_file(
            "includes.lua",
            r#"
                ---@class Includes
                local Includes = {}

                ---Include a file by path
                ---@param path string The file to include
                ---@return boolean success Whether the include succeeded
                function Includes.File(path)
                end

                _G.includes = Includes
            "#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                local file_fn = includes<??>.File
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
            markup.value.contains("Includes"),
            "expected hover to preserve exported table identity, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("File: function"),
            "expected hover to include exported members, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_global_alias_preserves_static_member_identity() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new_with_init_std_lib();
        ws.def_file(
            "includes.lua",
            r#"
                ---@class Includes
                local Includes = {}

                ---Include a file by path
                ---@param path string The file to include
                ---@return boolean success Whether the include succeeded
                function Includes.File(path)
                end

                _G.includes = Includes
            "#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                includes.Fi<??>le("sv_init.lua")
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
            markup
                .value
                .contains("function Includes.File(path: string)"),
            "expected rich static member hover, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("Include a file by path"),
            "expected member docs to be preserved, got: {}",
            markup.value
        );

        Ok(())
    }

    #[gtest]
    fn test_hover_global_alias_preserves_method_identity() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new_with_init_std_lib();
        ws.def_file(
            "netstream.lua",
            r#"
                ---@class NetStream
                local NetStream = {}

                ---Send a net message
                ---@param name string The message name
                ---@param payload table The payload to send
                ---@return boolean success Whether sending succeeded
                function NetStream:Send(name, payload)
                end

                _G.netstream = NetStream
            "#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                netstream.Se<??>nd("chat", {})
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
            markup
                .value
                .contains("(method) NetStream:Send(name: string, payload: table)"),
            "expected rich method hover, got: {}",
            markup.value
        );
        assert!(
            markup.value.contains("Send a net message"),
            "expected method docs to be preserved, got: {}",
            markup.value
        );

        Ok(())
    }
}
