#[cfg(test)]
mod test {
    use glua_parser::{LuaAssignStat, LuaAstNode, LuaSyntaxId};

    use crate::db_index::derivation::{DerivationId, DerivationKind, FactKey};
    use crate::{DynamicFieldOwner, Emmyrc, FileId, LuaDeclId, LuaTypeOwner, VirtualWorkspace};

    fn dynamic_field_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.infer_dynamic_fields = true;
        ws.update_emmyrc(emmyrc);
        ws
    }

    /// Every dynamic-field record, as sorted text.
    fn dynamic_field_state(ws: &VirtualWorkspace) -> Vec<String> {
        let index = ws.analysis.compilation.get_db().get_dynamic_field_index();
        let mut state = Vec::new();
        for (owner, fields) in index.iter_all_field_definitions() {
            for (name, definitions) in fields {
                state.push(format!("definitions {owner:?} {name} {definitions:?}"));
            }
        }
        for (owner, fields) in index.iter_all_direct_field_definitions() {
            for (name, definitions) in fields {
                state.push(format!("direct {owner:?} {name} {definitions:?}"));
            }
        }
        for (owner, fields) in index.iter_all_owner_fields() {
            for (name, files) in fields {
                let mut files = files.iter().copied().collect::<Vec<_>>();
                files.sort_unstable();
                state.push(format!("files {owner:?} {name} {files:?}"));
            }
        }
        for (name, sites) in index.iter_all_unattributed_fields() {
            let mut sites = sites.iter().cloned().collect::<Vec<_>>();
            sites.sort_by_key(|site| (site.file_id, site.value.start()));
            state.push(format!("unattributed {name} {sites:?}"));
        }
        for (owner, members) in index.iter_all_finite_named_members() {
            let mut members = members.iter().copied().collect::<Vec<_>>();
            members
                .sort_by_key(|member| (member.file_id, member.get_syntax_id().get_range().start()));
            state.push(format!("finite {owner:?} {members:?}"));
        }
        state.extend(
            index
                .get_all_wildcard_definitions()
                .into_iter()
                .map(|definition| format!("wildcard {definition:?}")),
        );
        state.sort();
        state
    }

    /// The owner the definitions of `name` are filed on.
    fn owner_of(ws: &VirtualWorkspace, name: &str) -> Vec<DynamicFieldOwner> {
        ws.analysis
            .compilation
            .get_db()
            .get_dynamic_field_index()
            .iter_all_field_definitions()
            .filter(|(_, fields)| fields.contains_key(name))
            .map(|(owner, _)| owner.clone())
            .collect()
    }

    const OWNER_FILE: &str = "lua/autorun/sh_owner.lua";
    const OWNER: &str = r#"---@class Ent
Ent = {}
local t = {}
t.first = 1
t = Ent
t.second = 2
Ent.shared = 1
Ent.fromOwner = true
"#;
    const WRITER_FILE: &str = "lua/autorun/sh_writer.lua";
    const WRITER: &str = r#"Ent.fromWriter = 1
Ent.shared = 2
local e = Ent
e.viaLocal = 3
local key = "computed"
e[key] = 4
"#;

    /// Several writes through one reference, across two files: an edit, a
    /// shift and a revert leave the dynamic fields and diagnostics a cold
    /// build has, and a later write through a reference takes the prefix type
    /// its first write inferred.
    #[test]
    fn writes_through_one_reference_settle_as_a_cold_build_after_edits() {
        let mut cold = dynamic_field_workspace();
        cold.def_file(OWNER_FILE, OWNER);
        cold.def_file(WRITER_FILE, WRITER);

        let mut warm = dynamic_field_workspace();
        warm.def_file(OWNER_FILE, OWNER);
        warm.def_file(WRITER_FILE, "Ent.fromWriter = 1\n");
        warm.def_file(WRITER_FILE, WRITER);
        warm.def_file(OWNER_FILE, &format!("-- shifted\n{OWNER}"));
        warm.def_file(OWNER_FILE, OWNER);

        assert_eq!(
            (dynamic_field_state(&warm), warm.settled_digest()),
            (dynamic_field_state(&cold), cold.settled_digest())
        );
        let first = owner_of(&cold, "first");
        assert!(matches!(first.as_slice(), [DynamicFieldOwner::Table(_)]));
        assert_eq!(owner_of(&cold, "second"), first);
    }

    /// The write site of `text`, an assignment of `file_id`.
    fn write_site(ws: &VirtualWorkspace, file_id: FileId, text: &str) -> DerivationId {
        let tree = ws
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("syntax tree");
        let assign = tree
            .get_chunk_node()
            .descendants::<LuaAssignStat>()
            .find(|assign| assign.syntax().text() == text)
            .expect("assignment");
        DerivationId::new(
            DerivationKind::DynamicFieldWrite,
            file_id,
            LuaSyntaxId::from_node(assign.syntax()),
        )
    }

    const MAKER_FILE: &str = "lua/autorun/sh_maker.lua";
    const LIB_FILE: &str = "lua/autorun/sh_lib.lua";
    const SITES_FILE: &str = "lua/autorun/sh_sites.lua";
    const SITES: &str = "local x = MakeX()\nx.a = 1\nYLib.b = 2\nYLib.c = 3\n";

    /// A change one write site reads runs that site again, not the other
    /// sites of its file.
    #[test]
    fn a_change_one_write_site_reads_runs_only_that_site_again() {
        let mut ws = dynamic_field_workspace();
        ws.def_file(MAKER_FILE, "function MakeX() return {} end\n");
        ws.def_file(LIB_FILE, "YLib = {}\n");
        ws.def_file(SITES_FILE, SITES);

        ws.def_file(MAKER_FILE, "function MakeX() return \"x\" end\n");

        let stats = ws.analysis.compilation.last_stats();
        assert_eq!(stats.evaluations(DerivationKind::DynamicFieldWrite), 1);
    }

    /// Each write site reads its own targets' prefixes: one write does not
    /// read what another write's prefix names, so the two close no loop
    /// together.
    #[test]
    fn a_write_site_reads_only_its_own_prefixes() {
        let mut ws = dynamic_field_workspace();
        ws.def_file(MAKER_FILE, "function MakeX() return {} end\n");
        ws.def_file(LIB_FILE, "YLib = {}\n");
        let sites = ws.def_file(SITES_FILE, SITES);
        // `x` is declared at offset 6, after `local `.
        let x = FactKey::TypeCache(LuaTypeOwner::Decl(LuaDeclId::new(
            sites,
            rowan::TextSize::new(6),
        )));
        let reads_x = |site: DerivationId| {
            ws.analysis
                .compilation
                .get_db()
                .reader_graph()
                .reads_of(site)
                .any(|key| *key.key() == x)
        };

        assert_eq!(
            (
                reads_x(write_site(&ws, sites, "x.a = 1")),
                reads_x(write_site(&ws, sites, "YLib.b = 2")),
            ),
            (true, false)
        );
    }

    /// Two instances assigning a class's method give it a union of dynamic
    /// methods; a subclass putting the method back from its base class reads
    /// them together with the method they override, as it does for one.
    #[test]
    fn a_method_read_from_a_base_class_keeps_the_overridden_method_beside_several_dynamic_ones() {
        let mut ws = dynamic_field_workspace();
        ws.def_gmod_call_arg_builtins();
        let annotations = ws.def_file(
            "lua/aaa_annotations/panels.lua",
            "---@class Panel
Panel = Panel or {}
---@class Label : Panel
---@class DLabel : Label
---@class DButton : DLabel
---@class DImageButton : DButton
---@param w number
---@param h number
function Panel:PaintOver(w, h) end
",
        );
        ws.def_file(
            "gamemodes/base/gamemode/cl_scoreboard.lua",
            "---@param mute DImageButton
local function SetupMute(mute)
    mute.PaintOver = function(s, w, h) end
end
---@param deafen DImageButton
local function SetupDeafen(deafen)
    deafen.PaintOver = function(s, w, h) end
end
",
        );
        let wire = ws.def_file(
            "gamemodes/terrortown/entities/entities/ttt_c4/cl_init.lua",
            "local PANEL = {}
function PANEL:DoClick()
    local putBack = self.BaseClass.PaintOver
    self.PaintOver = putBack
end
vgui.Register(\"DisarmWire\", PANEL, \"DImageButton\")
",
        );

        let put_back = ws.local_ty(wire, "putBack");
        let crate::LuaType::Union(union) = &put_back else {
            panic!("expected a union of methods, got {put_back:?}");
        };
        assert!(
            union.types().any(|arm| matches!(
                arm,
                crate::LuaType::Signature(signature) if signature.get_file_id() == annotations
            )),
            "expected the overridden Panel:PaintOver among {put_back:?}"
        );
    }
}
