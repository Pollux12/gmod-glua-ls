#[cfg(test)]
mod test {
    use std::sync::mpsc;
    use std::time::Duration;

    use crate::{Emmyrc, GMOD_CALL_ARG_BUILTINS_FIXTURE, Schedule, VirtualWorkspace};

    /// Every type cache, dynamic field, net flow and the semantic digest of `files`
    /// cold-built in one batch under `schedule`, as sorted text; `None` when
    /// the build does not settle within a generous bound.
    fn settled(schedule: Schedule, files: &[(&str, &str)]) -> Option<Vec<String>> {
        settled_in(schedule, files, false)
    }

    /// [`settled`], over the standard library as well when `std_lib` is set.
    fn settled_in(
        schedule: Schedule,
        files: &[(&str, &str)],
        std_lib: bool,
    ) -> Option<Vec<String>> {
        let files = files
            .iter()
            .map(|(path, text)| (path.to_string(), text.to_string()))
            .collect::<Vec<_>>();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .stack_size(crate::ANALYSIS_STACK_SIZE)
            .spawn(move || {
                let mut ws = if std_lib {
                    VirtualWorkspace::new_with_init_std_lib()
                } else {
                    VirtualWorkspace::new()
                };
                let mut emmyrc = Emmyrc::default();
                emmyrc.gmod.enabled = true;
                ws.update_emmyrc(emmyrc);
                ws.analysis.compilation.set_schedule(schedule);
                ws.def_files(
                    files
                        .iter()
                        .map(|(path, text)| (path.as_str(), text.as_str()))
                        .collect(),
                );
                let db = ws.analysis.compilation.get_db();
                let mut settled = db
                    .get_type_index()
                    .iter_type_caches()
                    .map(|(owner, cache)| format!("{owner:?} = {:?}", cache.as_type()))
                    .chain(
                        db.get_dynamic_field_index()
                            .iter_all_field_definitions()
                            .flat_map(|(owner, fields)| {
                                fields
                                    .keys()
                                    .map(move |field| format!("{owner:?} has {field}"))
                            }),
                    )
                    .collect::<Vec<_>>();
                let network = db.get_gmod_network_index();
                settled.extend(
                    network
                        .iter_send_flows()
                        .map(|(file_id, _, flow)| format!("{file_id:?} sends {flow:?}")),
                );
                settled.extend(
                    network
                        .iter_receive_flows()
                        .map(|(file_id, flow)| format!("{file_id:?} receives {flow:?}")),
                );
                settled.extend(ws.semantic_digest());
                settled.sort();
                let _ = tx.send(settled);
            })
            .expect("analysis thread");
        rx.recv_timeout(Duration::from_secs(30)).ok()
    }

    #[test]
    fn a_param_typed_only_by_its_name_while_its_call_sites_settle_converges() {
        assert!(
            settled_in(
                Schedule::Canonical,
                &[
                    (
                        "lua/aaa_annotations/entity.lua",
                        "---@class Entity
---@return tableof<self>
function Entity:GetTable() end
",
                    ),
                    (
                        "gamemodes/cityrp/entities/weapons/gmod_tool/stools/advresizer.lua",
                        r#"local function ForgetConstraint(ent, RConstraint)
    for k, Constraint in pairs(Constraints) do
        if Constraint ~= RConstraint then
        end
    end
    ent.Constraints = NewTab
end

local function GetConstraintVals(ent, Constraint, Type)
    for Arg, Val in pairs(Constraint:GetTable()) do
        ForgetConstraint(Val, Constraint)
        local Constraints = ent.Constraints
        for Key, Constraint in pairs(Constraints) do
            GetConstraintVals(ent, Constraint, Type)
        end
    end
end
"#,
                    ),
                ],
                true,
            )
            .is_some()
        );
    }

    fn assert_schedule_independent(files: &[(&str, &str)]) {
        let canonical = settled(Schedule::Canonical, files).expect("canonical order settles");
        let reversed = settled(Schedule::Reversed, files).expect("reversed order settles");
        let only_canonical = canonical
            .iter()
            .filter(|line| !reversed.contains(line))
            .collect::<Vec<_>>();
        let only_reversed = reversed
            .iter()
            .filter(|line| !canonical.contains(line))
            .collect::<Vec<_>>();
        assert!(
            only_canonical.is_empty() && only_reversed.is_empty(),
            "canonical only: {only_canonical:#?}\nreversed only: {only_reversed:#?}"
        );
    }

    #[test]
    fn a_subclass_bootstrapping_a_field_its_base_bootstraps_settles_the_same_in_either_order() {
        assert_schedule_independent(&[
            ("lua/includes/entity.lua", "---@class Entity\n"),
            (
                "lua/entities/base_glide_car/shared.lua",
                "ENT.FuelDefaults = ENT.FuelDefaults or { fillPort = { ang = 1 } }\n",
            ),
            (
                "lua/entities/base_glide_motorcycle/shared.lua",
                "ENT.Base = \"base_glide_car\"\nENT.FuelDefaults = ENT.FuelDefaults or { engineType = 1 }\n",
            ),
            (
                "lua/entities/fl_ford_explorer.lua",
                "ENT.Base = \"base_glide_car\"\nENT.FuelDefaults = { capacity = 60 }\n",
            ),
        ]);
    }

    #[test]
    fn a_field_written_only_with_its_own_value_settles_the_same_in_either_order() {
        assert_schedule_independent(&[
            (
                "lua/aaa_annotations/entity.lua",
                r#"---@class Entity
---@return tableof<self>
function Entity:GetTable() end

---@generic T : table
---@param metaName `T`
---@return (definition) T
function _G.FindMetaTable(metaName) end
"#,
            ),
            (
                "lua/entities/base_glide_car/init.lua",
                r#"function ENT:OnPostThink(dt, selfTbl)
    ---@type base_glide_car
    selfTbl = selfTbl or getTable(self)
    local isElectric = self:IsElectric()
    selfTbl._isElectric = isElectric
end
"#,
            ),
            (
                "lua/entities/base_glide_car/shared.lua",
                r#"local getTable = FindMetaTable("Entity").GetTable
function ENT:IsElectric(selfTbl)
    selfTbl = selfTbl or getTable(self)
    local cached = selfTbl._isElectric
    selfTbl._isElectric = cached
    return cached
end
"#,
            ),
        ]);
    }

    #[test]
    fn two_entities_incrementing_a_player_counter_settle_in_either_order() {
        assert_schedule_independent(&[
            (
                "lua/aaa_annotations/entity.lua",
                r#"---@class Entity
local Entity = {}

---@param activator Entity
function Entity:Use(activator, caller, useType, value) end
"#,
            ),
            (
                "lua/aaa_annotations/player.lua",
                "---@class (partial) Player : Entity\n",
            ),
            (
                "gamemodes/cityrp/gamemode/core/hooks/sv_hooks.lua",
                "function GM:PlayerInitialSpawn(player)\n    player.cityrp = {}\nend\n",
            ),
            (
                "gamemodes/cityrp/plugins/farming/entities/entities/cityrp_plant_base/init.lua",
                PLANT_USE,
            ),
            (
                "gamemodes/cityrp/plugins/farming/entities/entities/cityrp_plant_ground_base/init.lua",
                PLANT_USE,
            ),
        ]);
    }

    const PLANT_USE: &str = r#"function ENT:Use(activator, caller)
    activator.cityrp._PlantsHarvested = activator.cityrp._PlantsHarvested + 1
end
"#;

    #[test]
    fn a_cached_entity_field_read_through_a_documented_table_settles_the_same_in_either_order() {
        assert_schedule_independent(&[
            (
                "lua/aaa_annotations/entity.lua",
                r#"---@class Entity
---@return tableof<self>
function Entity:GetTable() end

---@generic T : table
---@param metaName `T`
---@return (definition) T
function _G.FindMetaTable(metaName) end
"#,
            ),
            (
                "lua/entities/base_glide_car/init.lua",
                r#"function ENT:OnPostThink(dt, selfTbl)
    ---@type base_glide_car
    selfTbl = selfTbl or getTable(self)
    local isElectric = self:IsElectric()
    selfTbl._isElectric = isElectric
end
"#,
            ),
            (
                "lua/entities/base_glide_car/shared.lua",
                r#"local getTable = FindMetaTable("Entity").GetTable
--- @param selfTbl table|nil Optional cached entity table from GetTable()
function ENT:IsElectric(selfTbl)
    selfTbl = selfTbl or getTable(self)
    local cached = selfTbl._isElectric
    if cached ~= nil then
        return cached
    end
    cached = self:GetEngineTypeId() == 1
    selfTbl._isElectric = cached
    return cached
end
"#,
            ),
        ]);
    }

    #[test]
    fn a_panel_two_files_register_settles_the_same_in_either_order() {
        const REGISTER: &str =
            "local PANEL = {}\nvgui.Register(\"PlayerFrame\", PANEL, \"Panel\")\n";
        assert_schedule_independent(&[
            ("lua/includes/builtins.lua", GMOD_CALL_ARG_BUILTINS_FIXTURE),
            (
                "gamemodes/cityrp/plugins/characters/ui/cl_player_frame.lua",
                REGISTER,
            ),
            (
                "gamemodes/cityrp/plugins/scoreboard/ui/cl_player_frame.lua",
                REGISTER,
            ),
        ]);
    }

    #[test]
    fn a_weapon_field_copied_back_from_its_saved_original_settles_the_same_in_either_order() {
        assert_schedule_independent(&[
            (
                "lua/weapons/cw_base/shared.lua",
                r#"SWEP.ZoomAmount = 15
function SWEP:Initialize()
    self.ZoomAmount_Orig = self.ZoomAmount
end
"#,
            ),
            (
                "lua/weapons/cw_base/sh_attachments.lua",
                r#"function SWEP:resetPostDetach(att, attCategory)
    self.ZoomAmount = self.ZoomAmount_Orig
end
"#,
            ),
            (
                "lua/weapons/cw_base/cl_playerbindpress.lua",
                r#"function SWEP.PlayerBindPress(ply, b, p)
    local wep = ply:GetActiveWeapon()
    wep.ZoomAmount = math.Clamp(wep.ZoomAmount - 15, wep.MinZoom, wep.MaxZoom)
end
"#,
            ),
        ]);
    }

    #[test]
    fn a_hook_two_plugins_override_settles_the_same_in_either_order() {
        assert_schedule_independent(&[
            (
                "lua/aaa_annotations/global.lua",
                r#"---@generic T : table
---@param metaName `T`
---@return (definition) T
function _G.FindMetaTable(metaName) end
"#,
            ),
            (
                "lua/aaa_annotations/gm.lua",
                "---@param victim Player
function GM:PlayerDeath(victim, inflictor, attacker) end
",
            ),
            (
                "lua/aaa_annotations/net.lua",
                r#"---@[net_send("client")]
function net.Broadcast() end
---@[call_arg("gmod.net_message", "start")]
---@param messageName string
function net.Start(messageName, unreliable) end
"#,
            ),
            (
                "lua/aaa_annotations/player.lua",
                "---@class (partial) Player : Entity
",
            ),
            (
                "gamemodes/cityrp/gamemode/core/libraries/sh_plugin.lua",
                "--- @class PLUGIN
",
            ),
            (
                "gamemodes/cityrp/plugins/accessories/sv_plugin.lua",
                r#"---@type PLUGIN
local PLUGIN = PLUGIN
local meta = FindMetaTable("Player")
function meta:ResetParts()
    net.Start("cityrpPartReset")
    net.Broadcast()
end
function PLUGIN:PlayerDeath(client)
    client:ResetParts()
end
"#,
            ),
            (
                "gamemodes/cityrp/plugins/kevlar/sv_init.lua",
                "---@type PLUGIN
local PLUGIN = PLUGIN
function PLUGIN:PlayerDeath(ply)
end
",
            ),
        ]);
    }
}
