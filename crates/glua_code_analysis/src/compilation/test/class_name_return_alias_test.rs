#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};
    use glua_parser::{LuaAstNode, LuaCallExpr, LuaExpr};
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    const ANNOTATIONS_PATH: &str = "annotations/gmod.lua";
    const ANNOTATIONS: &str = r#"
        ---@meta
        ---@class Entity
        ---@class NULL : Entity
        ---@type NULL
        NULL = nil
        ---@class sent_crate : Entity
        function sent_crate:OpenCrate() end

        ---@class Panel
        function Panel:Dock() end
        ---@class DPanel : Panel
        ---@class DScrollPanel : DPanel
        function DScrollPanel:AddItem(pnl) end

        vgui = {}
        ents = {}

        ---@generic T : Panel
        ---@param classname `T`
        ---@param parent? Panel
        ---@param name? string
        ---@return (instance) T?
        function vgui.Create(classname, parent, name) end

        ---@generic T : Entity
        ---@param class `T`
        ---@return (instance) T|NULL
        function ents.Create(class) end

        ---@generic T : table
        ---@param metaName `T`
        ---@return (definition) T
        function FindMetaTable(metaName) end
    "#;

    const TDLIB_PATH: &str = "gamemodes/cityrp/gamemode/core/libraries/thirdparty/cl_tdlib.lua";
    const TDLIB: &str = r#"
        local classes = {}

        local meta = FindMetaTable("Panel")

        function meta:TDLib()
            self.Class = function(pnl, name, ...)
                local class = classes[name]
                class(pnl, ...)
                return pnl
            end

            for k, v in pairs(classes) do
                self[k] = function(s, ...) return s:Class(k, ...) end
            end

            return self
        end

        function TDLib(c, p, n)
            local pnl = vgui.Create(c, p, n)
            return pnl:TDLib()
        end
    "#;

    fn workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_file(ANNOTATIONS_PATH, ANNOTATIONS);
        ws
    }

    fn local_type(ws: &VirtualWorkspace, file_id: crate::FileId, name: &str) -> String {
        ws.humanize_type(ws.local_ty(file_id, name))
    }

    fn call_type(ws: &VirtualWorkspace, file_id: crate::FileId, prefix: &str) -> String {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let call = semantic_model
            .get_root()
            .descendants::<LuaCallExpr>()
            .find(|call| {
                call.get_prefix_expr()
                    .is_some_and(|prefix_expr| prefix_expr.syntax().text() == prefix)
            })
            .expect("call expression");
        ws.humanize_type(
            semantic_model
                .infer_expr(LuaExpr::CallExpr(call))
                .expect("call type"),
        )
    }

    fn diagnostic_count(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        code: DiagnosticCode,
    ) -> usize {
        ws.analysis.diagnostic.enable_only(code);
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .iter()
            .filter(|diagnostic| {
                diagnostic.code == Some(NumberOrString::String(code.get_name().to_string()))
            })
            .count()
    }

    #[test]
    fn direct_vgui_create_wrapper_returns_the_named_panel() {
        let mut ws = workspace();
        let file_id = ws.def_file(
            "lua/autorun/client/wrapper.lua",
            r#"
            local function W(c, p)
                return vgui.Create(c, p)
            end

            local direct = vgui.Create("DScrollPanel")
            local wrapped = W("DScrollPanel")
            "#,
        );

        assert_eq!(call_type(&ws, file_id, "vgui.Create"), "DScrollPanel?");
        assert_eq!(call_type(&ws, file_id, "W"), "DScrollPanel?");
        assert_eq!(
            local_type(&ws, file_id, "wrapped"),
            local_type(&ws, file_id, "direct")
        );
    }

    #[test]
    fn wrapper_through_receiver_returning_method_returns_the_named_panel() {
        let mut ws = workspace();
        ws.def_file(TDLIB_PATH, TDLIB);
        let file_id = ws.def_file(
            "gamemodes/cityrp/gamemode/modules/menu/cl_menu.lua",
            r#"
            local function build(parent)
                local scroll = TDLib("DScrollPanel", parent)
                scroll:AddItem(parent)
            end
            "#,
        );

        assert_eq!(call_type(&ws, file_id, "TDLib"), "DScrollPanel?");
        assert_eq!(local_type(&ws, file_id, "scroll"), "DScrollPanel?");
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn wrapper_around_entity_factory_keeps_its_null_return_shape() {
        let mut ws = workspace();
        let file_id = ws.def_file(
            "lua/autorun/server/spawn.lua",
            r#"
            local function Spawn(class)
                local ent = ents.Create(class)
                return ent
            end

            local direct = ents.Create("sent_crate")
            local wrapped = Spawn("sent_crate")
            "#,
        );

        let direct = call_type(&ws, file_id, "ents.Create");
        assert!(direct.contains("NULL"), "{direct}");
        assert_eq!(call_type(&ws, file_id, "Spawn"), direct);
    }

    #[test]
    fn editing_the_wrapper_body_retypes_its_callers() {
        let setup = |ws: &mut VirtualWorkspace| {
            let mut emmyrc = Emmyrc::default();
            emmyrc.gmod.enabled = true;
            ws.update_emmyrc(emmyrc);
            ws.def_file(ANNOTATIONS_PATH, ANNOTATIONS);
        };
        let caller_path = "gamemodes/cityrp/gamemode/modules/menu/cl_menu.lua";
        let caller = r#"
            local scroll = TDLib("DScrollPanel")
            print(scroll)
        "#;
        let edited_tdlib = TDLIB.replace("return pnl:TDLib()", "pnl:TDLib()");

        let ws = VirtualWorkspace::assert_warm_matches_cold(
            setup,
            &[(TDLIB_PATH, TDLIB), (caller_path, caller)],
            &[(TDLIB_PATH, edited_tdlib.as_str())],
        );
        let caller_id = ws
            .analysis
            .get_file_id(&ws.virtual_url_generator.new_uri(caller_path))
            .expect("caller file");
        assert_ne!(call_type(&ws, caller_id, "TDLib"), "DScrollPanel?");

        let ws = VirtualWorkspace::assert_warm_matches_cold(
            setup,
            &[(TDLIB_PATH, edited_tdlib.as_str()), (caller_path, caller)],
            &[(TDLIB_PATH, TDLIB)],
        );
        let caller_id = ws
            .analysis
            .get_file_id(&ws.virtual_url_generator.new_uri(caller_path))
            .expect("caller file");
        assert_eq!(call_type(&ws, caller_id, "TDLib"), "DScrollPanel?");
    }

    #[test]
    fn editing_the_receiver_method_retypes_wrapper_callers() {
        let setup = |ws: &mut VirtualWorkspace| {
            let mut emmyrc = Emmyrc::default();
            emmyrc.gmod.enabled = true;
            ws.update_emmyrc(emmyrc);
            ws.def_file(ANNOTATIONS_PATH, ANNOTATIONS);
        };
        let method_path = "gamemodes/cityrp/gamemode/core/libraries/cl_panel_meta.lua";
        let method = r#"
            local meta = FindMetaTable("Panel")
            function meta:Styled()
                self.Hook = function(s) return s end
                return self
            end
        "#;
        let edited_method = method.replace("return self", "return {}");
        let wrapper_path = "gamemodes/cityrp/gamemode/core/libraries/cl_styled.lua";
        let wrapper = r#"
            function Styled(c, p)
                local pnl = vgui.Create(c, p)
                return pnl:Styled()
            end
        "#;
        let caller_path = "gamemodes/cityrp/gamemode/modules/menu/cl_menu.lua";
        let caller = r#"
            local scroll = Styled("DScrollPanel")
            print(scroll)
        "#;

        let ws = VirtualWorkspace::assert_warm_matches_cold(
            setup,
            &[
                (method_path, method),
                (wrapper_path, wrapper),
                (caller_path, caller),
            ],
            &[(method_path, edited_method.as_str())],
        );
        let caller_id = ws
            .analysis
            .get_file_id(&ws.virtual_url_generator.new_uri(caller_path))
            .expect("caller file");
        assert_ne!(call_type(&ws, caller_id, "Styled"), "DScrollPanel?");

        let ws = VirtualWorkspace::assert_warm_matches_cold(
            setup,
            &[
                (method_path, edited_method.as_str()),
                (wrapper_path, wrapper),
                (caller_path, caller),
            ],
            &[(method_path, method)],
        );
        let caller_id = ws
            .analysis
            .get_file_id(&ws.virtual_url_generator.new_uri(caller_path))
            .expect("caller file");
        assert_eq!(call_type(&ws, caller_id, "Styled"), "DScrollPanel?");
    }

    const PANEL_CLASSES_PATH: &str = "annotations/panels.lua";
    const PANEL_CLASSES: &str = r#"
        ---@meta
        ---@class Panel
        function Panel:Dock() end
        ---@class DPanel : Panel
        ---@class DScrollPanel : DPanel
        function DScrollPanel:AddItem(pnl) end

        ---@generic T : table
        ---@param metaName `T`
        ---@return (definition) T
        function FindMetaTable(metaName) end
    "#;
    const REGISTERED_PANEL_PATH: &str = "gamemodes/cityrp/gamemode/derma/cl_requirement.lua";
    const REGISTERED_PANEL: &str = r#"
        local PANEL = {}

        function PANEL:SetRequirement(item, amount) end

        vgui.Register("cityrp_requirementItem", PANEL, "DPanel")
    "#;
    const WRAPPER_CALLER_PATH: &str = "gamemodes/cityrp/gamemode/derma/cl_crafting.lua";
    const WRAPPER_CALLER: &str = r#"
        local function Plain(c, p)
            return vgui.Create(c, p)
        end

        local directScroll = vgui.Create("DScrollPanel")
        local wrappedScroll = TDLib("DScrollPanel", directScroll)
        local plainScroll = Plain("DScrollPanel", directScroll)
        wrappedScroll:AddItem(plainScroll)
        plainScroll:AddItem(wrappedScroll)

        local directItem = vgui.Create("cityrp_requirementItem", directScroll)
        local wrappedItem = TDLib("cityrp_requirementItem", wrappedScroll)
        wrappedItem:SetRequirement("wood", 2)
        directItem:SetRequirement("wood", 2)
    "#;

    fn wrapper_files() -> [(&'static str, &'static str); 5] {
        [
            (
                "lua/includes/glua_ls_gmod_call_arg_builtins.lua",
                crate::GMOD_CALL_ARG_BUILTINS_FIXTURE,
            ),
            (PANEL_CLASSES_PATH, PANEL_CLASSES),
            (TDLIB_PATH, TDLIB),
            (REGISTERED_PANEL_PATH, REGISTERED_PANEL),
            (WRAPPER_CALLER_PATH, WRAPPER_CALLER),
        ]
    }

    #[test]
    fn wrapper_types_known_panels_exactly_as_a_direct_create() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_files(wrapper_files().to_vec());
        let file_id = ws
            .analysis
            .get_file_id(&ws.virtual_url_generator.new_uri(WRAPPER_CALLER_PATH))
            .expect("caller file");

        let direct = local_type(&ws, file_id, "directScroll");
        assert_eq!(direct, "DScrollPanel");
        assert_eq!(local_type(&ws, file_id, "wrappedScroll"), direct);
        assert_eq!(local_type(&ws, file_id, "plainScroll"), direct);
        assert_eq!(
            local_type(&ws, file_id, "wrappedItem"),
            local_type(&ws, file_id, "directItem")
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::NeedCheckNil),
            0
        );
        assert_eq!(
            diagnostic_count(&mut ws, file_id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }

    #[test]
    fn wrapper_types_settle_independent_of_schedule() {
        crate::compilation::test::schedule_order_test::test::assert_schedule_independent(
            &wrapper_files(),
        );
    }
}

#[cfg(test)]
mod conditional_class_name {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};

    #[test]
    fn conditional_class_name_creates_either_registered_panel() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.def(
            "---@class Panel
---@class DPanel : Panel
",
        );
        assert!(ws.check_file_for(
            DiagnosticCode::UndefinedMethod,
            "gamemodes/helix/gamemode/core/derma/cl_tooltip.lua",
            r#"
            local PANEL = {}
            function PANEL:SetEntity(entity) end
            vgui.Register("ixTooltip", PANEL, "DPanel")
            local MINIMAL = {}
            vgui.Register("ixTooltipMinimal", MINIMAL, "ixTooltip")
            local function Show(minimal)
                local tooltip = vgui.Create(minimal and "ixTooltipMinimal" or "ixTooltip")
                tooltip:SetEntity(1)
            end
            "#,
        ));
    }
}
