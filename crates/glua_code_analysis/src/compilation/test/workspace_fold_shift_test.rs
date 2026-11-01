//! An edit that only moves a file's statements leaves every read of them
//! holding. The workspace folds read other files' statements and survive the
//! edit, so each must come out of it settled and as a cold build has it.
#[cfg(test)]
mod test {
    use std::collections::BTreeSet;

    use crate::db_index::derivation::{DerivationId, DerivationKind, FactKey};
    use crate::{Emmyrc, VirtualWorkspace};

    const FILES: &[(&str, &str)] = &[
        (
            "lua/autorun/sh_net_helper.lua",
            r#"
            function SendThing(name, ply)
                net.Start(name)
                net.Send(ply)
            end
            util.AddNetworkString("thing")
            "#,
        ),
        (
            "lua/autorun/sh_hook_helper.lua",
            r#"
            function AddMyHook(name, fn)
                hook.Add(name, "my_hook", fn)
            end
            "#,
        ),
        (
            "lua/autorun/sh_loader.lua",
            r#"
            AddCSLuaFile("myaddon/cl_panel.lua")
            include("myaddon/sh_shared.lua")
            "#,
        ),
        (
            "lua/myaddon/sh_shared.lua",
            r#"
            MyAddon = MyAddon or {}
            function MyAddon.Ready() return true end
            "#,
        ),
        (
            "lua/myaddon/cl_panel.lua",
            r#"
            local PANEL = {}
            function PANEL:Init() self.ready = true end
            vgui.Register("MyAddonPanel", PANEL, "DPanel")
            "#,
        ),
        (
            "lua/autorun/client/cl_use_panel.lua",
            r#"
            local panel = vgui.Create("MyAddonPanel")
            panel:Init()
            "#,
        ),
        (
            "lua/autorun/sh_valid.lua",
            r#"
            function IsUsable(ent)
                return IsValid(ent) and ent.usable
            end
            "#,
        ),
        (
            "lua/autorun/server/sv_use.lua",
            r#"
            AddMyHook("Think", function() end)
            SendThing("thing", nil)
            local target = Entity(1)
            if IsUsable(target) then print(target.usable) end
            "#,
        ),
    ];

    fn workspace(files: &[(&str, &str)]) -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.def_gmod_type_predicates();
        ws.enable_full_diagnostic();
        ws.def_files(files.to_vec());
        ws
    }

    /// The workspace folds that read another file's statement.
    fn syntax_readers(ws: &VirtualWorkspace) -> BTreeSet<DerivationKind> {
        let graph = ws.analysis.compilation.get_db().reader_graph();
        graph
            .derivations()
            .filter(|id| id.file_id == crate::FileId::VIRTUAL)
            .filter(|id| {
                graph
                    .reads_of(*id)
                    .any(|key| matches!(key.key(), FactKey::Syntax(_)))
            })
            .map(|id| id.kind)
            .collect()
    }

    fn assert_settled_as_cold(ws: &VirtualWorkspace, texts: &[(&str, String)], step: &str) {
        assert_eq!(ws.stale_derivations(), Vec::new(), "{step}: stale");
        assert_eq!(ws.phantom_derivations(), Vec::new(), "{step}: phantom");
        let texts = texts
            .iter()
            .map(|(path, text)| (*path, text.as_str()))
            .collect::<Vec<_>>();
        let cold = workspace(&texts);
        assert_eq!(ws.settled_digest(), cold.settled_digest(), "{step}");
    }

    #[test]
    fn moved_statements_leave_workspace_folds_settled() {
        let mut ws = workspace(FILES);
        let graph = ws.analysis.compilation.get_db().reader_graph();
        for kind in [
            DerivationKind::NetHelperNames,
            DerivationKind::GmodCallRoles,
            DerivationKind::GmodLoad,
            DerivationKind::VguiChains,
            DerivationKind::ValidGuards,
        ] {
            assert!(graph.contains(DerivationId::workspace(kind)), "{kind:?}");
        }
        assert!(syntax_readers(&ws).contains(&DerivationKind::NetHelperNames));

        let mut texts = FILES
            .iter()
            .map(|(path, text)| (*path, text.to_string()))
            .collect::<Vec<_>>();
        texts[0].1.insert(0, '\n');
        ws.def_file(texts[0].0, &texts[0].1);
        assert_settled_as_cold(&ws, &texts, "one file moved");

        for (_, text) in &mut texts {
            text.insert(0, '\n');
        }
        ws.def_files(
            texts
                .iter()
                .map(|(path, text)| (*path, text.as_str()))
                .collect(),
        );
        assert_settled_as_cold(&ws, &texts, "every file moved");
    }

    const VGUI_FILES: &[(&str, &str)] = &[
        (
            "lua/vgui/tab_button.lua",
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class DPanel: Panel
            ---@class TabButton: Panel
            local TabButton = {}
            function TabButton:Click() end
            vgui.Register("TabButton", TabButton, "Panel")
            "#,
        ),
        (
            "lua/vgui/tabbed_frame.lua",
            r#"
            ---@class TabbedFrame: Panel
            local TabbedFrame = {}
            function TabbedFrame:Init()
                self.tabList = vgui.Create("DPanel", self)
            end
            function TabbedFrame:AddTab()
                local button = vgui.Create("TabButton", self.tabList)
            end
            vgui.Register("TabbedFrame", TabbedFrame, "Panel")
            "#,
        ),
        ("lua/autorun/sh_other.lua", "local value = 1\n"),
    ];

    fn tab_button_chain(ws: &VirtualWorkspace) -> Option<Vec<String>> {
        ws.analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index()
            .get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("TabButton"))
            .map(|chain| chain.iter().map(|id| id.get_name().to_string()).collect())
    }

    fn evaluations(ws: &VirtualWorkspace, kind: DerivationKind) -> usize {
        ws.analysis.compilation.last_stats().evaluations(kind)
    }

    #[test]
    fn an_edit_re_resolves_only_its_own_files_vgui_parent_calls() {
        let mut ws = workspace(VGUI_FILES);
        assert_eq!(
            tab_button_chain(&ws),
            Some(vec!["DPanel".to_string(), "TabbedFrame".to_string()])
        );
        let mut texts = VGUI_FILES
            .iter()
            .map(|(path, text)| (*path, text.to_string()))
            .collect::<Vec<_>>();

        texts[2].1.insert(0, '\n');
        ws.def_file(texts[2].0, &texts[2].1);
        assert_eq!(evaluations(&ws, DerivationKind::VguiRelations), 1);
        assert_eq!(evaluations(&ws, DerivationKind::VguiChains), 0);
        assert_settled_as_cold(&ws, &texts, "unrelated file moved");

        texts[1].1.insert(0, '\n');
        ws.def_file(texts[1].0, &texts[1].1);
        assert_eq!(evaluations(&ws, DerivationKind::VguiChains), 0);
        assert_settled_as_cold(&ws, &texts, "parent calls moved");

        texts[1].1 = texts[1].1.replace("self.tabList)", "self)");
        ws.def_file(texts[1].0, &texts[1].1);
        assert_eq!(tab_button_chain(&ws), Some(vec!["TabbedFrame".to_string()]));
        assert_settled_as_cold(&ws, &texts, "parent call changed");
    }

    const LOCAL_HELPER: (&str, &str) = (
        "lua/autorun/sh_local_helper.lua",
        r#"
            local function send(name)
                net.Start(name)
                net.Broadcast()
            end
            function Broadcast(name)
                send(name)
            end
            "#,
    );

    fn is_helper_name(ws: &VirtualWorkspace, name: &str) -> bool {
        ws.analysis
            .compilation
            .get_db()
            .get_gmod_network_index()
            .is_helper_name(name)
    }

    #[test]
    fn an_edit_that_leaves_net_helpers_alone_does_not_fold_them_again() {
        let mut files = FILES.to_vec();
        files.push(LOCAL_HELPER);
        let mut ws = workspace(&files);
        assert!(is_helper_name(&ws, "Broadcast"));
        let mut texts = files
            .iter()
            .map(|(path, text)| (*path, text.to_string()))
            .collect::<Vec<_>>();
        let helper = texts.len() - 1;

        texts[helper]
            .1
            .insert_str(0, "local unrelated = 1\nprint(unrelated)\n");
        ws.def_file(texts[helper].0, &texts[helper].1);
        assert_eq!(evaluations(&ws, DerivationKind::NetHelperNames), 0);
        assert_settled_as_cold(&ws, &texts, "unrelated local added");

        texts[helper]
            .1
            .push_str("function Relay(name) Broadcast(name) end\n");
        ws.def_file(texts[helper].0, &texts[helper].1);
        assert!(is_helper_name(&ws, "Relay"));
        assert_settled_as_cold(&ws, &texts, "helper added");
    }

    const REALM_BRANCHES: (&str, &str) = (
        "lua/myaddon/sh_branches.lua",
        r#"
            if SERVER then
                ServerOnly = true
            end
            "#,
    );

    #[test]
    fn an_edit_that_only_moves_load_and_realm_facts_does_not_fold_them_again() {
        let mut files = FILES.to_vec();
        files.push(REALM_BRANCHES);
        let mut ws = workspace(&files);
        let mut texts = files
            .iter()
            .map(|(path, text)| (*path, text.to_string()))
            .collect::<Vec<_>>();
        let (loader, branches) = (2, texts.len() - 1);

        for file in [loader, branches] {
            texts[file].1.insert_str(0, "local moved = 1\n");
            ws.def_file(texts[file].0, &texts[file].1);
            assert_eq!(evaluations(&ws, DerivationKind::GmodLoad), 0);
            assert_settled_as_cold(&ws, &texts, texts[file].0);
        }

        texts[branches]
            .1
            .push_str("if CLIENT then ClientOnly = true end\n");
        ws.def_file(texts[branches].0, &texts[branches].1);
        assert_settled_as_cold(&ws, &texts, "branch added");
    }

    fn call_role_digest(ws: &VirtualWorkspace) -> Vec<String> {
        ws.analysis.compilation.get_db().call_role_digest()
    }

    #[test]
    fn call_roles_follow_a_helper_added_and_removed_as_a_cold_build_has_them() {
        let mut ws = workspace(FILES);
        let original = FILES
            .iter()
            .map(|(path, text)| (*path, text.to_string()))
            .collect::<Vec<_>>();
        let mut texts = original.clone();
        texts[0].1.insert_str(
            0,
            "function SendOther(name, ply) SendThing(name, ply) end\n",
        );

        ws.def_file(texts[0].0, &texts[0].1);
        let digest = call_role_digest(&ws);
        assert!(digest.iter().any(|entry| entry.contains("SendOther")));
        let cold = workspace(
            &texts
                .iter()
                .map(|(path, text)| (*path, text.as_str()))
                .collect::<Vec<_>>(),
        );
        assert_eq!(digest, call_role_digest(&cold));
        assert_settled_as_cold(&ws, &texts, "helper added");

        ws.def_file(original[0].0, &original[0].1);
        assert_eq!(call_role_digest(&ws), call_role_digest(&workspace(FILES)));
        assert_settled_as_cold(&ws, &original, "helper removed");
    }
}
