#[cfg(test)]
mod test {
    use std::sync::mpsc;
    use std::time::Duration;

    use crate::{Emmyrc, Schedule, VirtualWorkspace};

    /// Every type cache, dynamic field and the semantic digest of `files`
    /// cold-built in one batch under `schedule`, as sorted text; `None` when
    /// the build does not settle within a generous bound.
    fn settled(schedule: Schedule, files: &[(&str, &str)]) -> Option<Vec<String>> {
        let files = files
            .iter()
            .map(|(path, text)| (path.to_string(), text.to_string()))
            .collect::<Vec<_>>();
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .stack_size(crate::ANALYSIS_STACK_SIZE)
            .spawn(move || {
                let mut ws = VirtualWorkspace::new();
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
                settled.extend(ws.semantic_digest());
                settled.sort();
                let _ = tx.send(settled);
            })
            .expect("analysis thread");
        rx.recv_timeout(Duration::from_secs(30)).ok()
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
    #[ignore = "a member cycle keeps whichever seed it first saw, `nil` or an `unknown` from an input not analysed yet"]
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
    #[ignore = "a member cycle keeps an `any` read before `getTable` resolved"]
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
}
