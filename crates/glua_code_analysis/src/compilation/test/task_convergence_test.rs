#[cfg(test)]
mod test {
    use crate::{Emmyrc, VirtualWorkspace};

    /// A weapon base that keeps a copy of its fire sound and restores it from
    /// another file, as CityRP's `cw_base` does: each statement's read waits
    /// while the other's write is untyped, so tasks that both run from one
    /// snapshot see each other's last state and swap it every superstep.
    const SHARED: &str = r#"
        function SWEP:Initialize()
            if self.FireSound then
                self.FireSound = Sound(CustomizableWeaponry:findFireSound(self.FireSound))
                self.FireSound_Orig = self.FireSound
            end
        end
    "#;
    const GENERAL: &str = r#"
        function SWEP:restoreSound()
            self.FireSound = self.FireSound_Orig
        end
    "#;

    /// What the copy cycle settles to, how many files and stages fell back
    /// to the serial loop, and how many derivations of a cycle ran on the
    /// index.
    fn settle() -> (Vec<String>, usize, usize) {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_files(vec![
            ("entities/weapons/cw_base/shared.lua", SHARED),
            ("entities/weapons/cw_base/sh_general.lua", GENERAL),
        ]);
        let stats = ws.analysis.compilation.last_stats();
        let (fallbacks, cycle_members) = (stats.serial_fallbacks(), stats.cycle_members());
        (ws.settled_digest(), fallbacks, cycle_members)
    }

    /// Two files whose statements copy a field into each other settle in
    /// worker supersteps as the serial loop settles them: the cycle runs on
    /// the index, and no file falls back to the serial loop.
    #[test]
    fn fields_copied_into_each_other_across_files_settle_as_the_serial_loop_settles_them() {
        let (settled, fallbacks, cycle_members) = settle();
        let serial = {
            let _serial = crate::compilation::analyzer::serial_engine();
            settle().0
        };

        assert_eq!((fallbacks, cycle_members > 0, settled), (0, true, serial));
    }

    /// Two loops store their key on a row panel and one also passes it to
    /// `SetText`. Each key is guessed from what the rows' `id` field holds,
    /// which is what the keys stored: read back, each guess undid the other's
    /// and the file's guesses never settled, nor settled alike in two batches.
    #[test]
    fn locals_stored_in_one_dynamic_field_of_panel_rows_settle() {
        let files = [
            (
                "lua/includes/panel.lua",
                "---@meta
---@attribute call_arg(domain: string, role: string, priority: integer?)
---@class Panel
---@field x number
---@field y number
---@field BaseClass Panel?
Panel = Panel or {}

---@class PANEL : Panel
PANEL = Panel

---@generic T : Panel
---@overload fun(self: Panel, panel: Panel): Panel
---@overload fun(self: Panel, panelTable: table): Panel
---@overload fun(self: Panel, className: `T`, parent: Panel): T
---@[call_arg(\"gmod.vgui_panel\", \"reference\")]
---@[call_arg(\"gmod.vgui_panel\", \"parent_self\")]
---@param className `T`
---@return (instance) T
function Panel:Add(className) end

---@param text string
function Panel:SetText(text) end
",
            ),
            (
                "gamemodes/helix/gamemode/core/derma/cl_settings.lua",
                "function PANEL:Init()
    self.canvas = self:Add(\"DScrollPanel\")
end

function PANEL:AddRow(type, category)
    local panel = (IsValid(category) and category or self.canvas):Add(id)
    return panel
end
",
            ),
            (
                "gamemodes/helix/gamemode/core/derma/cl_config.lua",
                "function PANEL:Populate()
    for k, v in SortedPairsByMemberValue(ix.plugin.list, \"name\") do
        local row = self:AddRow(ix.type.bool, self.loadedCategory)
        row.id = k
    end
end

function PANEL:UpdateUnloaded()
    for k, v in SortedPairs(ix.plugin.unloaded) do
        local row = self:AddRow(ix.type.bool, self.unloadedCategory)
        row.id = k
        row:SetText(k)
    end
end
",
            ),
        ];
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_files(files.to_vec());

        crate::compilation::test::schedule_order_test::test::assert_schedule_independent(&files);
    }
}
