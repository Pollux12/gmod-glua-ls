#[cfg(test)]
mod tests {
    use crate::handlers::test_lib::{ProviderVirtualWorkspace, VirtualLocation, check};
    use glua_code_analysis::{DocSyntax, Emmyrc};
    use googletest::prelude::*;
    use lsp_types::GotoDefinitionResponse;

    type Expected = VirtualLocation;

    #[gtest]
    fn test_basic_definition() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_definition(
            r#"
                ---@generic T
                ---@param name `T`
                ---@return T
                local function new(name)
                    return name
                end

                ---@class Ability

                local a = new("<??>Ability")
            "#,
            vec![Expected {
                file: "".to_string(),
                line: 8
            }]
        ));
        Ok(())
    }

    #[gtest]
    fn test_table_field_definition_1() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_definition(
            r#"
                ---@class T
                ---@field func fun(self:string)

                ---@type T
                local t = {
                    f<??>unc = function(self)
                    end
                }
            "#,
            vec![
                Expected {
                    file: "".to_string(),
                    line: 2
                },
                Expected {
                    file: "".to_string(),
                    line: 6
                },
            ]
        ));
        Ok(())
    }

    #[gtest]
    fn test_table_field_definition_2() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_definition(
            r#"
                ---@class T
                ---@field func fun(self: T) 注释注释

                ---@type T
                local t = {
                    func = function(self)
                    end,
                    a = 1,
                }

                t:func<??>()
            "#,
            vec![Expected {
                file: "".to_string(),
                line: 2
            }]
        ));
        Ok(())
    }

    #[gtest]
    fn test_goto_field() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_definition(
            r#"
                local t = {}
                function t:test(a)
                    self.abc = a
                end

                print(t.abc<??>)
            "#,
            vec![Expected {
                file: "".to_string(),
                line: 3
            }]
        ));
        Ok(())
    }

    #[gtest]
    fn test_goto_overload() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "test.lua",
            r#"
                ---@class Goto1
                ---@class Goto2
                ---@class Goto3

                ---@class T
                ---@field func fun(a:Goto1) # 1
                ---@field func fun(a:Goto2) # 2
                ---@field func fun(a:Goto3) # 3
                local T = {}

                function T:func(a)
                end
            "#,
        );

        check!(ws.check_definition(
            r#"
                ---@type Goto2
                local Goto2

                ---@type T
                local t
                t.fu<??>nc(Goto2)
             "#,
            vec![
                Expected {
                    file: "test.lua".to_string(),
                    line: 6,
                },
                Expected {
                    file: "test.lua".to_string(),
                    line: 7,
                },
            ]
        ));

        check!(ws.check_definition(
            r#"
                ---@type T
                local t
                t.fu<??>nc()
             "#,
            vec![
                Expected {
                    file: "test.lua".to_string(),
                    line: 6,
                },
                Expected {
                    file: "test.lua".to_string(),
                    line: 7,
                },
                Expected {
                    file: "test.lua".to_string(),
                    line: 8,
                },
                Expected {
                    file: "test.lua".to_string(),
                    line: 11,
                },
            ]
        ));
        Ok(())
    }

    #[gtest]
    fn test_goto_return_field() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "test.lua",
            r#"
                local function test()

                end

                return {
                    test = test,
                }
            "#,
        );
        check!(ws.check_definition(
            r#"
                local t = require("test")
                local test = t.test
                te<??>st()
            "#,
            vec![VirtualLocation {
                file: "test.lua".to_string(),
                line: 1
            }]
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_return_field_2() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new_with_init_std_lib();
        ws.def_file(
            "test.lua",
            r#"
                ---@export
                ---@class Export
                local export = {}
                ---@generic T
                ---@param name `T`|T
                ---@param tbl? table
                ---@return T
                local function new(name, tbl)
                end

                export.new = new
                return export
            "#,
        );
        check!(ws.check_definition(
            r#"
                local new = require("test").new
                new<??>("A")
            "#,
            vec![Expected {
                file: "test.lua".to_string(),
                line: 8
            }]
        ));
        Ok(())
    }

    #[gtest]
    fn test_goto_global_alias_static_member_definition() -> Result<()> {
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

        check!(ws.check_definition(
            r#"
                includes.Fi<??>le("sv_init.lua")
            "#,
            vec![Expected {
                file: "includes.lua".to_string(),
                line: 7,
            }]
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_global_alias_method_definition() -> Result<()> {
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

        check!(ws.check_definition(
            r#"
                netstream:Se<??>nd("chat", {})
            "#,
            vec![Expected {
                file: "netstream.lua".to_string(),
                line: 8,
            }]
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_require_return_table_fallback() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "reported-tasks.lua",
            r#"
            ---@class TestModule
            local TestModule = {}
            function TestModule:getA()
            end
            return {
                TestModule = TestModule,
            }
        "#,
        );

        check!(ws.check_definition(
            r#"
                local reportedTasks = require("reported-tasks")
                reported<??>Tasks.TestModule:getA()
            "#,
            vec![Expected {
                file: "".to_string(),
                line: 1,
            }]
        ));
        Ok(())
    }

    #[gtest]
    fn test_goto_generic_type() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "1.lua",
            r#"
                ---@generic T
                ---@param name `T`|T
                ---@return T
                function new(name)
                end
            "#,
        );
        ws.def_file(
            "2.lua",
            r#"
                ---@namespace AAA
                ---@class BBB<T>
            "#,
        );
        check!(ws.check_definition(
            r#"
                new("AAA.BBB<??>")
            "#,
            vec![Expected {
                file: "2.lua".to_string(),
                line: 2
            }]
        ));
        Ok(())
    }

    #[gtest]
    fn test_goto_export_function() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "a.lua",
            r#"
                local function create()
                end

                return create
            "#,
        );
        check!(ws.check_definition(
            r#"
                local create = require('a')
                create<??>()
            "#,
            vec![Expected {
                file: "a.lua".to_string(),
                line: 1
            }]
        ));
        Ok(())
    }

    #[gtest]
    fn test_goto_forward_declared_function_call_prefers_function_definition() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_definition(
            r#"
                local create_initial_simplex4

                function create_initial_simplex4(points, thread_yield)
                    return { points, thread_yield }
                end

                local faces = create_initial_simplex4<??>({}, nil)
            "#,
            vec![Expected {
                file: "".to_string(),
                line: 3,
            }]
        ));
        Ok(())
    }

    #[gtest]
    fn test_goto_export_function_2() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "a.lua",
            r#"
                local function testA()
                end

                local function create()
                end

                return create
            "#,
        );
        ws.def_file(
            "b.lua",
            r#"
                local Rxlua = {}
                local create = require('a')

                Rxlua.create = create
                return Rxlua
            "#,
        );
        check!(ws.check_definition(
            r#"
                local create = require('b').create
                create<??>()
            "#,
            vec![Expected {
                file: "a.lua".to_string(),
                line: 4
            }]
        ));
        Ok(())
    }

    #[gtest]
    fn test_doc_resolve() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();

        let mut emmyrc = Emmyrc::default();
        emmyrc.doc.syntax = DocSyntax::Myst;
        ws.analysis.update_config(emmyrc.into());

        ws.def_file(
            "a.lua",
            r#"
                --- @class X
                --- @field a string

                --- @class ns.Y
                --- @field b string
            "#,
        );

        check!(ws.check_definition(
            r#"
                --- {lua:obj}`X<??>`
            "#,
            vec![Expected {
                file: "a.lua".to_string(),
                line: 1
            }]
        ));

        check!(ws.check_definition(
            r#"
                --- {lua:obj}`X<??>.a`
            "#,
            vec![Expected {
                file: "a.lua".to_string(),
                line: 1
            }]
        ));

        check!(ws.check_definition(
            r#"
                --- {lua:obj}`X.a<??>`
            "#,
            vec![Expected {
                file: "a.lua".to_string(),
                line: 2
            }]
        ));

        check!(ws.check_definition(
            r#"
                --- @using ns

                --- {lua:obj}`X<??>`
            "#,
            vec![Expected {
                file: "a.lua".to_string(),
                line: 1
            }]
        ));

        check!(ws.check_definition(
            r#"
                --- @using ns

                --- {lua:obj}`Y<??>`
            "#,
            vec![Expected {
                file: "a.lua".to_string(),
                line: 4
            }]
        ));

        check!(ws.check_definition(
            r#"
                --- @using ns

                --- {lua:obj}`ns.Y<??>`
            "#,
            vec![Expected {
                file: "a.lua".to_string(),
                line: 4
            }]
        ));

        check!(ws.check_definition(
            r#"
                --- {lua:obj}`c<??>`
                --- @class Z
                --- @field c string
            "#,
            vec![Expected {
                file: "".to_string(),
                line: 3
            }]
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_variable_param() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "a.lua",
            r#"
                ---@class Observable<T>

                ---test
                local function zipLatest(...)
                end
                return zipLatest
            "#,
        );
        ws.def_file(
            "b.lua",
            r#"
            local export = {}
            local zipLatest = require('a')
            export.zipLatest = zipLatest
            return export
            "#,
        );
        check!(ws.check_definition(
            r#"
                local zipLatest = require('b').zipLatest
                zipLatest<??>()
            "#,
            vec![Expected {
                file: "a.lua".to_string(),
                line: 4,
            }],
        ));
        Ok(())
    }

    #[gtest]
    fn test_goto_see() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.doc.syntax = DocSyntax::Myst;
        ws.analysis.update_config(emmyrc.into());

        ws.def_file(
            "a.lua",
            r#"
                ---@class Meep
            "#,
        );

        check!(ws.check_definition(
            r#"
                --- @see Mee<??>p
            "#,
            vec![Expected {
                file: "a.lua".to_string(),
                line: 1,
            }],
        ));

        check!(ws.check_definition(
            r#"
                --- @class Foo
                --- @field bar int
                local Foo = {}

                --- @see b<??>ar
                Foo.xxx = 0
            "#,
            vec![Expected {
                file: "".to_string(),
                line: 2,
            }],
        ));

        Ok(())
    }

    #[gtest]
    fn test_accessors() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class A
            ---@[field_accessor]
            ---@field age number
            local A

            ---@private
            function A:getAge()
            end

            ---@private
            function A:setAge(value)
            end
            "#,
        );

        check!(ws.check_definition(
            r#"
                ---@type A
                local obj
                obj.age<??> = 1
            "#,
            vec![
                Expected {
                    file: "".to_string(),
                    line: 3,
                },
                Expected {
                    file: "".to_string(),
                    line: 7,
                },
                Expected {
                    file: "".to_string(),
                    line: 11,
                }
            ],
        ));

        Ok(())
    }

    #[gtest]
    fn test_intersection() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def(
            r#"
        ---@class Matchers
        ---@field toBe fun(expected: any)

        ---@class Inverse
        ---@field negate number
        "#,
        );
        check!(ws.check_definition(
            r#"
            ---@type Matchers & Inverse
            local a
            a.ne<??>gate = 0
            "#,
            vec![Expected {
                file: "".to_string(),
                line: 5,
            },],
        ));
        Ok(())
    }

    #[gtest]
    fn test_goto_inferred_dynamic_field_definition() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.analysis.update_config(emmyrc.into());

        ws.def_file(
            "assign.lua",
            "---@class DynGoto.Entity\n---@type DynGoto.Entity\nlocal ent\nent.testVar = true\n",
        );

        check!(ws.check_definition(
            r#"
                ---@type DynGoto.Entity
                local ent2
                ent2.te<??>stVar
            "#,
            vec![Expected {
                file: "assign.lua".to_string(),
                line: 3,
            }],
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_inferred_dynamic_field_definition_for_metatable_instance() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.analysis.update_config(emmyrc.into());

        check!(ws.check_definition(
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
            vec![Expected {
                file: "".to_string(),
                line: 7,
            }],
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_inferred_dynamic_field_definition_for_top_level_setmetatable_binding() -> Result<()>
    {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.analysis.update_config(emmyrc.into());

        check!(ws.check_definition(
            r#"
                local LOCATION = {}
                LOCATION.__index = LOCATION

                local instance = {}
                setmetatable(instance, LOCATION)
                instance._OriginalName = true

                function LOCATION:GetOriginalName()
                    return self._Origi<??>nalName
                end
            "#,
            vec![Expected {
                file: "".to_string(),
                line: 6,
            }],
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_inferred_dynamic_field_definition_keeps_future_same_file_assignment() -> Result<()>
    {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.analysis.update_config(emmyrc.into());

        check!(ws.check_definition(
            r#"
                ---@class DynGotoFuture.Entity
                ---@type DynGotoFuture.Entity
                local ent
                local value = ent.te<??>stVar
                ent.testVar = true
            "#,
            vec![Expected {
                file: "".to_string(),
                line: 5,
            }],
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_inferred_dynamic_field_definition_ignores_same_line_assignment_lhs() -> Result<()>
    {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.analysis.update_config(emmyrc.into());

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@class DynGotoSameLine.Entity
                ---@type DynGotoSameLine.Entity
                local ent
                ent.testVar = ent.te<??>stVar
            "#,
        )?;
        let file_id = ws.def(&content);
        let result = crate::handlers::definition::definition(&ws.analysis, file_id, position);
        verify_that!(result, none())?;

        Ok(())
    }

    #[gtest]
    fn test_goto_inferred_dynamic_field_definition_keeps_prior_same_line_assignment() -> Result<()>
    {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.analysis.update_config(emmyrc.into());

        check!(ws.check_definition(
            r#"
                ---@class DynGotoPriorSameLine.Entity
                ---@type DynGotoPriorSameLine.Entity
                local ent
                ent.testVar = true; local value = ent.te<??>stVar
            "#,
            vec![Expected {
                file: "".to_string(),
                line: 4,
            }],
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_inferred_dynamic_field_definition_respects_file_scope() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        emmyrc.gmod.dynamic_fields_global = false;
        ws.analysis.update_config(emmyrc.into());

        ws.def_file(
            "assign.lua",
            "---@class DynGotoScoped.Entity\n---@type DynGotoScoped.Entity\nlocal ent\nent.testVar = true\n",
        );
        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@type DynGotoScoped.Entity
                local ent2
                ent2.te<??>stVar
            "#,
        )?;
        let file_id = ws.def_file("use.lua", &content);
        let result = crate::handlers::definition::definition(&ws.analysis, file_id, position);
        verify_that!(result, none())?;

        Ok(())
    }

    #[gtest]
    fn test_goto_inferred_dynamic_field_definition_for_tableof() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.analysis.update_config(emmyrc.into());

        check!(ws.check_definition(
            r#"
                ---@class TblDef.Entity
                local TblDef = {}

                ---@return tableof<self>
                function TblDef:GetTable() end

                function TblDef:Init()
                    local tbl = self:GetTable()
                    tbl.customData = true
                    return tbl.cus<??>tomData
                end
            "#,
            vec![Expected {
                file: "".to_string(),
                line: 9,
            }],
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_inferred_dynamic_field_definition_through_table_alias() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.analysis.update_config(emmyrc.into());

        check!(ws.check_definition(
            r#"
                local rec = { data = {} }
                local data = rec.data
                for key, value in pairs({ forwardSlip = 1, sideSlip = 2 }) do
                    data[key] = value
                end
                local d = rec.data
                math.abs(d.forw<??>ardSlip or 0)
            "#,
            vec![Expected {
                file: "".to_string(),
                line: 4,
            }],
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_definition_does_not_treat_unknown_dynamic_key_as_named_field_definition()
    -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.analysis.update_config(emmyrc.into());

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                local rec = { data = {} }
                local data = rec.data
                local key = net.ReadString()
                data[key] = 1
                local d = rec.data
                print(d.forw<??>ardSlip)
            "#,
        )?;
        let file_id = ws.def(&content);
        let result = crate::handlers::definition::definition(&ws.analysis, file_id, position);
        verify_that!(result, none())?;

        Ok(())
    }

    #[gtest]
    fn test_goto_prefers_same_file_scripted_class_field_definition() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.analysis.update_config(emmyrc.into());

        ws.def_file(
            "lua/entities/base_glide/cl_init.lua",
            r#"
                local ENT = {}

                function ENT:InitWeapons()
                    self.weapons = {}
                end
            "#,
        );

        let (server_content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                local ENT = {}

                function ENT:InitWeapons()
                    self.weapons = {}
                end

                function ENT:ClearWeapons()
                    local myWeapons = self.we<??>apons
                end
            "#,
        )?;
        let server_file_id = ws.def_file("lua/entities/base_glide/sv_weapons.lua", &server_content);

        let result =
            crate::handlers::definition::definition(&ws.analysis, server_file_id, position)
                .ok_or("failed to get go to definition response")
                .or_fail()?;
        let locations = match result {
            GotoDefinitionResponse::Scalar(location) => vec![location],
            GotoDefinitionResponse::Array(locations) => locations,
            GotoDefinitionResponse::Link(_) => {
                return fail!("unexpected go to definition response");
            }
        };
        let first = locations
            .first()
            .ok_or("missing definition result")
            .or_fail()?;

        let file_name = first
            .uri
            .get_file_path()
            .or_fail()?
            .file_name()
            .or_fail()?
            .to_string_lossy()
            .to_string();

        verify_eq!(file_name, "sv_weapons.lua".to_string())?;
        verify_eq!(first.range.start.line, 4)?;

        Ok(())
    }

    #[gtest]
    fn test_goto_prefers_same_file_in_included_server_scripted_class_file() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.analysis.update_config(emmyrc.into());

        ws.def_file(
            "lua/entities/base_glide/shared.lua",
            r#"
                ENT.Type = "anim"
                ENT.Base = "base_anim"
            "#,
        );
        ws.def_file(
            "lua/entities/base_glide/init.lua",
            r#"
                AddCSLuaFile("shared.lua")
                AddCSLuaFile("cl_init.lua")

                include("shared.lua")
                include("sv_weapons.lua")
            "#,
        );
        ws.def_file(
            "lua/entities/base_glide/cl_init.lua",
            r#"
                include("shared.lua")

                function ENT:Initialize()
                    self.weapons = {}
                end
            "#,
        );

        let (server_content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                function ENT:WeaponInit()
                    self.weapons = {}
                end

                function ENT:ClearWeapons()
                    local myWeapons = self.we<??>apons
                end
            "#,
        )?;
        let server_file_id = ws.def_file("lua/entities/base_glide/sv_weapons.lua", &server_content);

        let result =
            crate::handlers::definition::definition(&ws.analysis, server_file_id, position)
                .ok_or("failed to get go to definition response")
                .or_fail()?;
        let locations = match result {
            GotoDefinitionResponse::Scalar(location) => vec![location],
            GotoDefinitionResponse::Array(locations) => locations,
            GotoDefinitionResponse::Link(_) => {
                return fail!("unexpected go to definition response");
            }
        };
        check!(ProviderVirtualWorkspace::assert_locations(
            locations,
            vec![Expected {
                file: "sv_weapons.lua".to_string(),
                line: 2,
            }],
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_scripted_class_field_definitions_survive_load_order_changes() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.analysis.update_config(emmyrc.into());

        ws.def_file(
            "lua/entities/base_glide/shared.lua",
            r#"
                ENT.Type = "anim"
                ENT.Base = "base_anim"
            "#,
        );
        ws.def_file(
            "lua/entities/base_glide/init.lua",
            r#"
                AddCSLuaFile("shared.lua")
                AddCSLuaFile("cl_init.lua")

                include("shared.lua")
                include("sv_weapons.lua")
            "#,
        );

        let (server_content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                function ENT:WeaponInit()
                    self.weapons = {}
                end

                function ENT:ClearWeapons()
                    local myWeapons = self.we<??>apons
                    if not myWeapons then return end
                    self.weapons = {}
                end
            "#,
        )?;
        let server_file_id = ws.def_file("lua/entities/base_glide/sv_weapons.lua", &server_content);

        ws.def_file(
            "lua/entities/base_glide/cl_init.lua",
            r#"
                include("shared.lua")

                function ENT:Initialize()
                    self.weapons = {}
                end
            "#,
        );

        let result =
            crate::handlers::definition::definition(&ws.analysis, server_file_id, position)
                .ok_or("failed to get go to definition response")
                .or_fail()?;
        let locations = match result {
            GotoDefinitionResponse::Scalar(location) => vec![location],
            GotoDefinitionResponse::Array(locations) => locations,
            GotoDefinitionResponse::Link(_) => {
                return fail!("unexpected go to definition response");
            }
        };
        let virtual_locations = locations
            .iter()
            .map(|location| {
                Ok(Expected {
                    file: location
                        .uri
                        .get_file_path()
                        .or_fail()?
                        .file_name()
                        .or_fail()?
                        .to_string_lossy()
                        .to_string(),
                    line: location.range.start.line,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let first_location = virtual_locations
            .first()
            .ok_or("missing goto definition results")
            .or_fail()?;
        assert_eq!(first_location.file, "sv_weapons.lua".to_string());
        assert_eq!(first_location.line, 2);
        assert!(
            virtual_locations
                .iter()
                .any(|location| location.file == "sv_weapons.lua" && location.line == 8),
            "expected goto definition results to include the later same-file server assignment: {virtual_locations:?}"
        );

        Ok(())
    }

    #[gtest]
    fn test_goto_gmod_member_prefers_real_definition_over_source_uri() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();

        ws.def_file(
            "glua-api-snippets/output/hook.lua",
            r#"
                hook = {}
                ---@source https://wiki.facepunch.com/gmod/hook.Run
                ---@param eventName string
                function hook.Run(eventName, ...)
                end
            "#,
        );

        check!(ws.check_definition(
            r#"
                hook.Ru<??>n("Think")
            "#,
            vec![Expected {
                file: "hook.lua".to_string(),
                line: 4,
            }],
        ));

        check!(ws.check_definition(
            r#"
                local run = hook.Ru<??>n
            "#,
            vec![Expected {
                file: "hook.lua".to_string(),
                line: 4,
            }],
        ));

        check!(ws.check_definition(
            r#"
                local run = hook.Run
                ru<??>n("Think")
            "#,
            vec![
                Expected {
                    file: "".to_string(),
                    line: 1,
                },
                Expected {
                    file: "hook.lua".to_string(),
                    line: 4,
                },
            ],
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_source_file_uri_redirects_before_stub_definition() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();

        ws.def_file(
            "annotations/generated.lua",
            r#"
                api = {}
                ---@source file:///real/source.lua#7:3
                function api.Stub()
                end
            "#,
        );

        check!(ws.check_definition(
            r#"
                api.St<??>ub()
            "#,
            vec![Expected {
                file: "source.lua".to_string(),
                line: 7,
            }],
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_vgui_panel_definition_from_string() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.analysis.update_config(emmyrc.into());

        ws.def_file(
            "panels.lua",
            r#"
                local PANEL = {}
                vgui.Register("MyPanel", PANEL, "DPanel")
            "#,
        );

        check!(ws.check_definition(
            r#"
                local pnl = vgui.Create("MyPa<??>nel")
            "#,
            vec![Expected {
                file: "panels.lua".to_string(),
                line: 2,
            }],
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_net_message_definition_from_start_string() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.analysis.update_config(emmyrc.into());

        ws.def_file(
            "receive.lua",
            r#"
                net.Receive("MyMessage", function() end)
            "#,
        );

        check!(ws.check_definition(
            r#"
                net.Start("MyMes<??>sage")
            "#,
            vec![Expected {
                file: "receive.lua".to_string(),
                line: 1,
            }],
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_net_message_definition_from_receive_string() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.analysis.update_config(emmyrc.into());

        ws.def_file(
            "send.lua",
            r#"
                net.Start("MyMessage")
                net.Broadcast()
            "#,
        );

        check!(ws.check_definition(
            r#"
                net.Receive("MyMes<??>sage", function() end)
            "#,
            vec![Expected {
                file: "send.lua".to_string(),
                line: 1,
            }],
        ));

        Ok(())
    }

    #[gtest]
    fn test_goto_dynamic_field_definition_targets_key_range() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.analysis.update_config(emmyrc.into());

        ws.def_file(
            "assign.lua",
            r#"
                ---@class DynKeyRange.Entity
                ---@type DynKeyRange.Entity
                local selfTbl
                selfTbl.forwardSpeed = 100
            "#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@type DynKeyRange.Entity
                local ent
                ent.forw<??>ardSpeed
            "#,
        )?;
        let caller_file_id = ws.def_file("use.lua", &content);
        let result =
            crate::handlers::definition::definition(&ws.analysis, caller_file_id, position)
                .ok_or("failed to get go to definition response")
                .or_fail()?;

        let locations = match result {
            GotoDefinitionResponse::Scalar(location) => vec![location],
            GotoDefinitionResponse::Array(locations) => locations,
            GotoDefinitionResponse::Link(_) => {
                return fail!("unexpected go to definition response");
            }
        };

        let first = locations
            .first()
            .ok_or("missing definition result")
            .or_fail()?;

        let file_name = first
            .uri
            .get_file_path()
            .or_fail()?
            .file_name()
            .or_fail()?
            .to_string_lossy()
            .to_string();
        verify_eq!(file_name, "assign.lua".to_string())?;
        verify_eq!(first.range.start.line, 4)?;

        // The definition range should cover only the field key "forwardSpeed",
        // not the full prefix expression "selfTbl.forwardSpeed".
        // Line 4: "                selfTbl.forwardSpeed = 100"
        //          0123456789012345678901234567890123456
        // The dot-key token "forwardSpeed" starts at char 24, ends at char 36.
        verify_eq!(first.range.start.character, 24u32)?;
        verify_eq!(first.range.end.character, 36u32)?;

        Ok(())
    }

    #[gtest]
    fn test_goto_inferred_dynamic_field_bracket_key() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.analysis.update_config(emmyrc.into());

        ws.def_file(
            "bracket_assign.lua",
            r#"
                ---@class DynBracket.Entity
                ---@type DynBracket.Entity
                local tbl
                tbl["forwardSpeed"] = 500
            "#,
        );

        let (content, position) = ProviderVirtualWorkspace::handle_file_content(
            r#"
                ---@type DynBracket.Entity
                local ent
                ent.forw<??>ardSpeed
            "#,
        )?;
        let caller_file_id = ws.def_file("bracket_use.lua", &content);
        let result =
            crate::handlers::definition::definition(&ws.analysis, caller_file_id, position)
                .ok_or("failed to get go to definition response")
                .or_fail()?;

        let locations = match result {
            GotoDefinitionResponse::Scalar(location) => vec![location],
            GotoDefinitionResponse::Array(locations) => locations,
            GotoDefinitionResponse::Link(_) => {
                return fail!("unexpected go to definition response");
            }
        };

        let first = locations
            .first()
            .ok_or("missing definition result")
            .or_fail()?;

        let file_name = first
            .uri
            .get_file_path()
            .or_fail()?
            .file_name()
            .or_fail()?
            .to_string_lossy()
            .to_string();
        verify_eq!(file_name, "bracket_assign.lua".to_string())?;
        verify_eq!(first.range.start.line, 4)?;

        // The definition range should cover only the string key token
        // "forwardSpeed" (including quotes), not the full bracket expression.
        // Line 4: "                tbl[\"forwardSpeed\"] = 500"
        //          0123456789012345678901234567890123456789
        // The string token "forwardSpeed" starts at char 20, ends at char 34.
        verify_eq!(first.range.start.character, 20u32)?;
        verify_eq!(first.range.end.character, 34u32)?;

        Ok(())
    }

    #[gtest]
    fn test_member_variable_definition_navigates_to_origin() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        check!(ws.check_definition(
            r#"
                ---@class MyClass
                ---@field myField number

                ---@type MyClass
                local obj

                obj.my<??>Field = 42
            "#,
            vec![Expected {
                file: "".to_string(),
                line: 2,
            }]
        ));
        Ok(())
    }

    #[gtest]
    fn test_member_variable_definition_via_typed_instance() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        ws.def_file(
            "inner.lua",
            r#"
                ---@class Inner
                ---@field isRedlining boolean
            "#,
        );
        check!(ws.check_definition(
            r#"
                local Inner = require("inner")

                ---@type Inner
                local stream

                stream.isRed<??>lining = true
            "#,
            vec![Expected {
                file: "inner.lua".to_string(),
                line: 2,
            }]
        ));
        Ok(())
    }

    // Goto-definition for `self.stream.isRedlining` in a consumer method
    // where `self.stream` was assigned from a factory function that creates
    // a local `stream` table and returns `setmetatable(stream, EngineStream)`.
    // The cursor on `isRedlining` in the consumer assignment should resolve
    // to the `isRedlining = false` field in the factory's table literal.
    #[gtest]
    fn test_goto_metatable_factory_stream_field_definition_from_consumer_assignment() -> Result<()>
    {
        let mut ws = ProviderVirtualWorkspace::new();

        ws.def_file(
            "glide_engine.lua",
            r#"
                local Glide = {}

                local EngineStream = {}
                EngineStream.__index = EngineStream

                function Glide.CreateEngineStream(ply, active)
                    local stream = {
                        isRedlining = false,
                        inputs = {
                            redline = 0,
                        },
                    }
                    return setmetatable(stream, EngineStream)
                end

                return Glide
            "#,
        );

        check!(ws.check_definition(
            r#"
                local Glide = require("glide_engine")

                local MyComponent = {}
                MyComponent.__index = MyComponent

                function MyComponent:Init()
                    self.stream = Glide.CreateEngineStream(LocalPlayer(), true)
                end

                function MyComponent:Update(isRedlining)
                    self.stream.isRed<??>lining = isRedlining
                end
            "#,
            vec![Expected {
                file: "glide_engine.lua".to_string(),
                line: 8,
            }]
        ));

        Ok(())
    }

    // Regression: goto-definition for `self.stream.isRedlining` in a
    // metatable-created object should navigate to the `isRedlining` field
    // in the `stream` sub-table, NOT to the `redline` field in `inputs`.
    // The analyzer must correctly resolve the nested field path through
    // `self.stream` rather than confusing `isRedlining` (boolean) with
    // `redline` (number) in a sibling sub-table.
    #[gtest]
    fn test_metatable_stream_is_redlining_goto_resolves_stream_field() -> Result<()> {
        let mut ws = ProviderVirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.analysis.update_config(emmyrc.into());

        check!(ws.check_definition(
            r#"
                ---@class StreamSystem
                local StreamSystem = {}
                StreamSystem.__index = StreamSystem

                function StreamSystem:new()
                    local obj = setmetatable({
                        stream = {
                            isRedlining = false,
                        },
                        inputs = {
                            redline = 0,
                        },
                    }, self)
                    return obj
                end

                function StreamSystem:update(isRedlining)
                    self.stream.isRed<??>lining = isRedlining
                end
            "#,
            vec![Expected {
                file: "".to_string(),
                line: 8,
            }]
        ));
        Ok(())
    }
}
