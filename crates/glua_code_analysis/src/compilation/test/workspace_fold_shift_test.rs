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
}
