//! A value derivations on a cycle hold only for each other goes when the
//! support that brought it in goes: after every update, the index is what a
//! cold build of the same texts leaves.
#[cfg(test)]
mod test {
    use crate::{Emmyrc, Slice, UpdateProgress, VirtualWorkspace};

    #[derive(Clone, Copy)]
    enum Step<'a> {
        Write(&'a str, &'a str),
        /// A write settled one evaluation at a time, a different file put
        /// ahead after each, as diagnostics pulls do while the server types.
        WriteInSlices(&'a str, &'a str),
        Delete(&'a str),
    }

    fn workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws
    }

    /// The settled index and semantic digest, every file id spelled as its
    /// path so workspaces holding different files compare.
    fn digest(ws: &VirtualWorkspace) -> Vec<String> {
        let db = ws.analysis.compilation.get_db();
        let vfs = db.get_vfs();
        let mut paths = vfs
            .get_all_file_ids()
            .into_iter()
            .filter_map(|file_id| {
                let path = vfs.get_file_path(&file_id)?;
                Some((
                    format!("FileId {{ id: {} }}", file_id.id),
                    path.to_string_lossy().replace('\\', "/"),
                ))
            })
            .collect::<Vec<_>>();
        // Longer ids first, so `id: 1 }` never rewrites inside `id: 12 }`.
        paths.sort_by_key(|(id, _)| std::cmp::Reverse(id.len()));
        let mut digest = ws
            .settled_digest()
            .into_iter()
            .filter(|line| !line.starts_with("LuaTypeDeclId"))
            .collect::<Vec<_>>();
        digest.extend(db.get_type_index().iter_type_decls().map(|decl| {
            let mut locations = decl
                .get_locations()
                .iter()
                .map(|location| {
                    let path = vfs
                        .get_file_path(&location.file_id)
                        .map(|path| path.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_default();
                    format!("{path}@{:?}", location.range)
                })
                .collect::<Vec<_>>();
            locations.sort();
            format!("{:?} at {locations:?}", decl.get_id())
        }));
        digest.extend(
            db.get_member_index()
                .iter_current_owner_keys()
                .map(|(owner, key)| format!("{owner:?} owns {key:?}")),
        );
        let mut digest = digest
            .into_iter()
            .map(|line| {
                paths
                    .iter()
                    .fold(line, |line, (id, path)| line.replace(id.as_str(), path))
            })
            .collect::<Vec<_>>();
        digest.sort();
        digest
    }

    /// Applies `steps` to a workspace holding `files` and, after each, asserts
    /// it matches a cold build of the texts as they stand.
    fn settles_as_cold(files: &[(&str, &str)], steps: &[Step]) {
        let mut warm = workspace();
        warm.def_files(files.to_vec());
        let mut texts = files.to_vec();
        for (number, step) in steps.iter().enumerate() {
            match *step {
                Step::Write(path, text) => {
                    warm.def_file(path, text);
                    match texts.iter_mut().find(|(existing, _)| *existing == path) {
                        Some(entry) => entry.1 = text,
                        None => texts.push((path, text)),
                    }
                }
                Step::WriteInSlices(path, text) => {
                    let uri = warm.virtual_url_generator.new_uri(path);
                    let file_id = warm
                        .analysis
                        .update_file_text_only(&uri, text.to_string())
                        .expect("a workspace file");
                    let ahead = texts
                        .iter()
                        .filter_map(|(path, _)| {
                            let uri = warm.virtual_url_generator.new_uri(path);
                            warm.analysis.get_file_id(&uri)
                        })
                        .collect::<Vec<_>>();
                    warm.analysis.begin_reindex(vec![file_id]);
                    for next in ahead.iter().cycle() {
                        match warm.analysis.advance_reindex(Slice::Evaluations(1)) {
                            UpdateProgress::Settled(_) => break,
                            UpdateProgress::Settling => {
                                warm.analysis.compilation.prioritise_update(&[*next]);
                            }
                            UpdateProgress::TextChanged => panic!("no text changed"),
                        }
                    }
                    if let Some(entry) = texts.iter_mut().find(|(existing, _)| *existing == path) {
                        entry.1 = text;
                    }
                }
                Step::Delete(path) => {
                    let uri = warm.virtual_url_generator.new_uri(path);
                    warm.analysis.remove_file_by_uri(&uri);
                    texts.retain(|(existing, _)| *existing != path);
                }
            }
            let mut cold = workspace();
            cold.def_files(texts.clone());
            let (warm, cold) = (digest(&warm), digest(&cold));
            let only_warm = warm
                .iter()
                .filter(|line| !cold.contains(line))
                .collect::<Vec<_>>();
            let only_cold = cold
                .iter()
                .filter(|line| !warm.contains(line))
                .collect::<Vec<_>>();
            assert!(
                only_warm.is_empty() && only_cold.is_empty(),
                "step {number}: only warm {only_warm:#?}\nonly cold {only_cold:#?}"
            );
        }
    }

    const ANNOTATIONS: (&str, &str) = (
        "lua/aaa_annotations/player.lua",
        "---@class Entity
---@class Weapon : Entity
---@class Player : Entity
---@return Weapon[]
function Player:GetWeapons() end
---@param class string
---@return Weapon
function Player:Give(class) end
",
    );

    const SHARED: (&str, &str) = (
        "gamemodes/helix/gamemode/shared.lua",
        "ix = ix or {meta = {}}
ix.item = ix.item or {}
",
    );

    const ITEM_META: (&str, &str) = (
        "gamemodes/helix/gamemode/core/meta/sh_item.lua",
        "local ITEM = ix.meta.item or {}
ITEM.__index = ITEM
ITEM.name = \"Undefined\"

function ITEM:GetName()
    return self.name
end

ix.meta.item = ITEM
",
    );

    const ITEM_LIB: &str = "gamemodes/helix/gamemode/core/libs/sh_item.lua";

    const ITEM_REGISTER: &str = "function ix.item.Register(uniqueID)
    local meta = ix.meta.item
    ITEM = setmetatable({}, meta)
    ITEM.uniqueID = uniqueID
end
";

    const WEAPON_BASE: (&str, &str) = (
        "gamemodes/helix/gamemode/items/base/sh_weapons.lua",
        "---@param weapon Weapon
function ITEM:Equip(weapon)
    weapon.ixItem = self
end

---@param weapon Weapon
function ITEM:Unequip(weapon)
    weapon.ixItem = nil
end
",
    );

    const RESTRICT: (&str, &str) = (
        "gamemodes/helix/gamemode/core/meta/sh_player.lua",
        "---@param client Player
function ix_Restrict(client)
    client.ixRestrictWeps = client.ixRestrictWeps or {}

    for _, v in ipairs(client:GetWeapons()) do
        client.ixRestrictWeps[#client.ixRestrictWeps + 1] = {
            class = \"weapon\",
            item = v.ixItem,
        }
    end
end

---@param client Player
function ix_Unrestrict(client)
    if (client.ixRestrictWeps) then
        for _, v in ipairs(client.ixRestrictWeps) do
            local weapon = client:Give(v.class)

            if (v.item) then
                weapon.ixItem = v.item
            end
        end

        client.ixRestrictWeps = nil
    end
end
",
    );

    #[test]
    fn a_saved_and_restored_field_loses_a_type_whose_only_supplier_is_deleted() {
        settles_as_cold(
            &[
                ANNOTATIONS,
                SHARED,
                ITEM_META,
                (ITEM_LIB, ITEM_REGISTER),
                WEAPON_BASE,
                RESTRICT,
            ],
            &[Step::Delete(ITEM_LIB), Step::Write(ITEM_LIB, ITEM_REGISTER)],
        );
    }

    const NET: (&str, &str) = (
        "lua/aaa_annotations/net.lua",
        "---@meta
net = {}

---@param name string
---@param callback fun(len: integer)
function net.Receive(name, callback) end

---@return table<any, any>
function net.ReadTable() end

---@return string
function net.ReadString() end

---@return boolean
function net.ReadBool() end
",
    );

    const PLUGIN_LIB: (&str, &str) = (
        "gamemodes/helix/gamemode/core/libs/sh_plugin.lua",
        "ix.plugin = ix.plugin or {}
ix.plugin.unloaded = ix.plugin.unloaded or {}

---@return string|any
function ix_LoadUnloaded() end

function ix.plugin.Initialize()
    if SERVER then
        ix.plugin.unloaded = ix_LoadUnloaded()
    end
end

function ix.plugin.SetUnloaded(uniqueID, state)
    if (state) then
        ix.plugin.unloaded[uniqueID] = true
    elseif (ix.plugin.unloaded[uniqueID]) then
        ix.plugin.unloaded[uniqueID] = false
    end
end
",
    );

    const PLUGIN_CONFIG: (&str, &str) = (
        "gamemodes/helix/gamemode/core/sh_config.lua",
        "if CLIENT then
    net.Receive(\"ixConfigUnloadedList\", function()
        ix.plugin.unloaded = net.ReadTable()
    end)

    net.Receive(\"ixConfigPluginToggle\", function()
        local uniqueID = net.ReadString()

        if (net.ReadBool()) then
            ix.plugin.unloaded[uniqueID] = false
        else
            ix.plugin.unloaded[uniqueID] = true
        end
    end)
end
",
    );

    #[test]
    fn writes_through_a_guarded_bootstrap_stop_homing_on_it_when_its_other_writer_is_deleted() {
        settles_as_cold(
            &[NET, SHARED, PLUGIN_LIB, PLUGIN_CONFIG],
            &[
                Step::Delete(PLUGIN_CONFIG.0),
                Step::Write(PLUGIN_CONFIG.0, PLUGIN_CONFIG.1),
            ],
        );
    }

    const CONFIG_PANEL: (&str, &str) = (
        "gamemodes/helix/gamemode/core/derma/cl_config.lua",
        "function PANEL:Populate()
	for k, v in SortedPairsByMemberValue(ix.plugin.list, \"name\") do
		local row = self:AddRow(ix.type.bool, self.loadedCategory)
		row.id = k
	end
end

function PANEL:UpdateUnloaded(bNoSizeToContents)
	for k, v in SortedPairs(ix.plugin.unloaded) do
		local row = self:AddRow(ix.type.bool, self.unloadedCategory)
		row.id = k
		row:SetText(k)
	end
end
",
    );

    const PLUGIN_LIST: (&str, &str) = (
        "gamemodes/helix/gamemode/core/libs/sh_plugin.lua",
        "ix.plugin = ix.plugin or {}
ix.plugin.unloaded = ix.plugin.unloaded or {}",
    );

    const UNLOADED_LIST: (&str, &str) = (
        "gamemodes/helix/gamemode/core/sh_config.lua",
        "if (value) then
	ix.plugin.unloaded = net.ReadTable()
end
",
    );

    const SETTINGS_PANEL: (&str, &str) = (
        "gamemodes/helix/gamemode/core/derma/cl_settings.lua",
        "PANEL = {}

function PANEL:Init()
	self.canvas = self:Add(\"DScrollPanel\")
end

function PANEL:AddRow(type, category)
	local panel = (IsValid(category) and category or self.canvas):Add(id)
	return panel
end

vgui.Register(\"ixSettings\", PANEL, \"Panel\")
",
    );

    const GAMEMODE_INIT: (&str, &str) = (
        "gamemodes/helix/gamemode/init.lua",
        "ix = ix or {util = {}, meta = {}}",
    );

    const GLOBAL_ANNOTATIONS: (&str, &str) = (
        "lua/aaa_annotations/global.lua",
        "---@generic K, V
---@param table table<K, V> | V[] | {[K]: V} # The table to sort
---@return fun(tbl: any, key: K?): K, V # Iterator function
function _G.SortedPairs(table, desc) end",
    );

    const PANEL_ANNOTATIONS: (&str, &str) = (
        "lua/aaa_annotations/panel.lua",
        "---@class Panel
Panel = Panel or {}
---@generic T : Panel
---@param className `T` The panel class name to create and add.
---@return (instance) T # The created panel.
function Panel:Add(className) end
---@param text string The text value to set.
function Panel:SetText(text) end",
    );

    const VGUI_ANNOTATIONS: (&str, &str) = (
        "lua/aaa_annotations/vgui.lua",
        "---@[call_arg(\"gmod.vgui_panel\", \"define\")]
---@param classname string Classname of the panel to register.
function vgui.Register(classname, panelTable, baseName) end",
    );

    /// Both loops store their key into `row.id`, the field's only writers, and
    /// each key is guessed from its uses: the one read back from `row.id` is
    /// the other key's guess, so the guesses supported each other after the
    /// unknown keys that started them were deleted.
    #[test]
    fn locals_stored_into_one_field_keep_no_guess_they_gave_each_other_once_their_keys_go() {
        settles_as_cold(
            &[
                CONFIG_PANEL,
                PLUGIN_LIST,
                UNLOADED_LIST,
                SETTINGS_PANEL,
                GAMEMODE_INIT,
                GLOBAL_ANNOTATIONS,
                PANEL_ANNOTATIONS,
                VGUI_ANNOTATIONS,
            ],
            &[
                Step::Delete(UNLOADED_LIST.0),
                Step::Write(UNLOADED_LIST.0, UNLOADED_LIST.1),
            ],
        );
    }

    const ITEM_REGISTRY: &str = "ix.item.list = ix.item.list or {}
ix.item.base = ix.item.base or {}

function ix.item.Register(uniqueID, baseID, isBaseItem)
    local meta = ix.meta.item

    ITEM = (isBaseItem and ix.item.base or ix.item.list)[uniqueID] or setmetatable({}, meta)
    ITEM.uniqueID = uniqueID;

    (isBaseItem and ix.item.base or ix.item.list)[ITEM.uniqueID] = ITEM
end
";

    #[test]
    fn a_registered_item_keeps_no_fallback_table_once_its_metatable_returns() {
        settles_as_cold(
            &[SHARED, ITEM_META, (ITEM_LIB, ITEM_REGISTRY)],
            &[
                Step::Delete(ITEM_META.0),
                Step::Write(ITEM_META.0, ITEM_META.1),
            ],
        );
    }

    const EQUIP_AND_SAVE: (&str, &str) = (
        "gamemodes/helix/gamemode/items/base/sh_weapons.lua",
        "---@param weapon Weapon
function ITEM:Equip(weapon)
    weapon.ixItem = self
end

---@param client Player
function ix_Restrict(client)
    client.ixRestrictWeps = client.ixRestrictWeps or {}

    for _, v in ipairs(client:GetWeapons()) do
        client.ixRestrictWeps[#client.ixRestrictWeps + 1] = {
            class = \"weapon\",
            item = v.ixItem,
        }
    end
end
",
    );

    const UNRESTRICT: (&str, &str) = (
        "gamemodes/helix/gamemode/core/meta/sh_player.lua",
        "---@param client Player
function ix_Unrestrict(client)
    if (client.ixRestrictWeps) then
        for _, v in ipairs(client.ixRestrictWeps) do
            local weapon = client:Give(v.class)

            if (v.item) then
                weapon.ixItem = v.item
            end
        end

        client.ixRestrictWeps = nil
    end
end
",
    );

    #[test]
    fn a_saved_field_loses_a_type_when_the_file_that_saves_it_stops_supplying_it() {
        let without_supplier = EQUIP_AND_SAVE.1.replace(
            "    weapon.ixItem = self
",
            "",
        );
        settles_as_cold(
            &[
                ANNOTATIONS,
                SHARED,
                ITEM_META,
                (ITEM_LIB, ITEM_REGISTER),
                EQUIP_AND_SAVE,
                UNRESTRICT,
            ],
            &[
                Step::Write(EQUIP_AND_SAVE.0, &without_supplier),
                Step::Write(EQUIP_AND_SAVE.0, EQUIP_AND_SAVE.1),
            ],
        );
    }

    #[test]
    fn a_saved_and_restored_field_settles_as_cold_while_files_are_put_ahead_mid_reanalysis() {
        settles_as_cold(
            &[
                ANNOTATIONS,
                SHARED,
                ITEM_META,
                (ITEM_LIB, ITEM_REGISTER),
                WEAPON_BASE,
                RESTRICT,
            ],
            &[
                Step::WriteInSlices(ITEM_LIB, ""),
                Step::WriteInSlices(ITEM_LIB, ITEM_REGISTER),
            ],
        );
    }

    #[test]
    fn a_saved_field_settles_as_cold_while_files_are_put_ahead_mid_reanalysis() {
        let without_supplier = EQUIP_AND_SAVE.1.replace(
            "    weapon.ixItem = self
",
            "",
        );
        settles_as_cold(
            &[
                ANNOTATIONS,
                SHARED,
                ITEM_META,
                (ITEM_LIB, ITEM_REGISTER),
                EQUIP_AND_SAVE,
                UNRESTRICT,
            ],
            &[
                Step::WriteInSlices(EQUIP_AND_SAVE.0, &without_supplier),
                Step::WriteInSlices(EQUIP_AND_SAVE.0, EQUIP_AND_SAVE.1),
            ],
        );
    }

    const GLIDE_ANNOTATIONS: (&str, &str) = (
        "lua/aaa_annotations/glide.lua",
        "---@meta
---@param value number
---@param min number
---@param max number
---@return number
function math.Clamp(value, min, max) end

---@[call_arg(\"gmod.class_base\", \"reference\")]
---@param value string
function _G.DEFINE_BASECLASS(value) end

---@class Angle
---@field [integer] number
Angle = {}

---@return Angle
function _G.Angle() end
",
    );

    const GLIDE_INPUT: (&str, &str) = (
        "lua/entities/base_glide/sv_input.lua",
        "local math_Clamp = math.Clamp
function ENT:GetInputFloat(seatIndex, action, entTbl)
    return math_Clamp(value, -1, 1)
end
",
    );

    const GLIDE_CAR_STEERING: (&str, &str) = (
        "lua/entities/base_glide_car/init.lua",
        "function ENT:UpdateSteering(dt, selfTbl)
    selfTbl = selfTbl or {}
    local inputSteer = self:GetInputFloat(1, \"steer\")
    local absInputSteer = inputSteer < 0 and -inputSteer or inputSteer
    if avgSideSlip < -1 then sideSlip = -1 elseif avgSideSlip > 1 then sideSlip = 1 else sideSlip = avgSideSlip end
    local totalSpeed = selfTbl.totalSpeed or 0
    local steerConeFactor = totalSpeed / 1000
    local counterSteer = sideSlip * steerConeFactor * (1 - absInputSteer)
    inputSteer = inputSteer + counterSteer
end
",
    );

    const GLIDE_CAR_SHARED: (&str, &str) = (
        "lua/entities/base_glide_car/shared.lua",
        "DEFINE_BASECLASS(\"base_glide\")
local boneAng = Angle()
function ENT:UpdateWheelBones()
    local spin = self:GetWheelSpin()
    if self.spinInvert then spin = -spin end
    boneAng[3] = spin
end
",
    );

    /// `inputSteer`, `absInputSteer` and `counterSteer` hold one another's
    /// type through `inputSteer = inputSteer + counterSteer`; only the local's
    /// declaration, outside that loop, reads the deleted base method.
    #[test]
    fn a_loop_held_up_by_a_deleted_base_method_settles_as_cold() {
        settles_as_cold(
            &[
                GLIDE_ANNOTATIONS,
                GLIDE_INPUT,
                GLIDE_CAR_STEERING,
                GLIDE_CAR_SHARED,
            ],
            &[
                Step::Delete(GLIDE_INPUT.0),
                Step::Write(GLIDE_INPUT.0, GLIDE_INPUT.1),
            ],
        );
    }
}
