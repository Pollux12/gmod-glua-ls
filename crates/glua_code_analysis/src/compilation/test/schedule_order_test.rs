#[cfg(test)]
pub(super) mod test {
    use std::sync::mpsc;
    use std::time::Duration;

    use crate::{
        Emmyrc, GMOD_CALL_ARG_BUILTINS_FIXTURE, Schedule, Slice, UpdateProgress, VirtualWorkspace,
    };

    /// How a cold build settles.
    #[derive(Debug, Clone, Copy)]
    enum Build {
        Whole,
        OneEvaluationAtATime,
        /// The earlier half of the files first, the later half's empty, then
        /// every file: a later file's analysis starts from what the first
        /// batch settled.
        TwoBatches,
    }

    /// Every type cache, type declaration, dynamic field, net flow and the semantic
    /// digest of `files`
    /// cold-built in one batch under `schedule`, as sorted text; `None` when
    /// the build does not settle within a generous bound.
    fn settled(schedule: Schedule, files: &[(&str, &str)]) -> Option<Vec<String>> {
        settled_in(schedule, files, false, Build::Whole)
    }

    /// [`settled`], over the standard library as well when `std_lib` is set,
    /// settling as `build` says.
    fn settled_in(
        schedule: Schedule,
        files: &[(&str, &str)],
        std_lib: bool,
        build: Build,
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
                match build {
                    Build::Whole => {
                        ws.def_files(
                            files
                                .iter()
                                .map(|(path, text)| (path.as_str(), text.as_str()))
                                .collect(),
                        );
                    }
                    Build::TwoBatches => {
                        let cut = files.len() / 2;
                        ws.def_files(
                            files
                                .iter()
                                .enumerate()
                                .map(|(at, (path, text))| {
                                    (path.as_str(), if at < cut { text.as_str() } else { "" })
                                })
                                .collect(),
                        );
                        ws.def_files(
                            files[cut..]
                                .iter()
                                .map(|(path, text)| (path.as_str(), text.as_str()))
                                .collect(),
                        );
                    }
                    Build::OneEvaluationAtATime => {
                        let mut texts = files
                            .iter()
                            .map(|(path, text)| (ws.virtual_url_generator.new_uri(path), text))
                            .collect::<Vec<_>>();
                        let vfs = ws.analysis.compilation.get_db().get_vfs();
                        texts.sort_by_cached_key(|(uri, _)| vfs.order_key_for_uri(uri));
                        let file_ids = texts
                            .into_iter()
                            .map(|(uri, text)| {
                                ws.analysis
                                    .update_file_text_only(&uri, text.clone())
                                    .expect("a file id")
                            })
                            .collect();
                        ws.analysis.begin_reindex(file_ids);
                        while !matches!(
                            ws.analysis.advance_reindex(Slice::Evaluations(1)),
                            UpdateProgress::Settled(_)
                        ) {}
                    }
                }
                let _ = tx.send(ws.settled_digest());
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
                Build::Whole,
            )
            .is_some()
        );
    }

    #[test]
    fn a_local_reassigned_after_reading_a_plain_table_field_settles_the_same_in_either_order() {
        assert_schedule_independent(&[
            (
                "lua/aaa_annotations/physenv.lua",
                "physenv = {}
---@return table
function physenv.GetPerformanceSettings() end
",
            ),
            (
                "lua/glide/server/util.lua",
                "local maxVelocity = physenv.GetPerformanceSettings().MaxVelocity
maxVelocity = 4000
",
            ),
        ]);
    }

    #[test]
    fn a_local_whose_initializer_gives_up_and_a_later_assignment_answers_settles_the_same_in_either_order()
     {
        assert_schedule_independent(&[
            (
                "lua/glide/sh_repair.lua",
                "Glide = Glide or {}
",
            ),
            (
                "lua/glide/sh_utils.lua",
                "function Glide.ExpDecay(a, b, decay, dt)
    return b + (a - b) * Exp(-decay * dt)
end
",
            ),
            (
                "lua/entities/base_glide_car/cl_init.lua",
                r#"local ExpDecay = Glide.ExpDecay
local inertia = ExpDecay(lastInertia, self:GetEngineThrottle(), 1, dt)
if inertia < lastInertia and inertia > 0.6 and self.rpmFraction > 0.5 then
    inertia = 0
end
self.turboInertia = inertia
"#,
            ),
        ]);
    }

    #[test]
    fn a_panel_class_two_plugins_register_keeps_both_sites_in_either_order() {
        const REGISTER: &str = "local PANEL = {}
vgui.Register(\"PlayerBanButton\", PANEL, \"DButton\")
";
        assert_schedule_independent(&[
            ("lua/includes/builtins.lua", GMOD_CALL_ARG_BUILTINS_FIXTURE),
            (
                "gamemodes/cityrp/plugins/characterintroductions/ui/cl_admin_buttons.lua",
                REGISTER,
            ),
            (
                "gamemodes/cityrp/plugins/scoreboardplugin/ui/cl_admin_buttons.lua",
                REGISTER,
            ),
        ]);
    }

    /// Each panel's accessor first reads its local before `vgui.Register`
    /// names its class, and files it on the annotated `PANEL` class until it
    /// does: in a parallel build every file's task takes its own accessor back
    /// off `PANEL`, and together they leave it with no members.
    #[test]
    fn panel_accessors_filed_on_the_panel_class_until_registered_settle_the_same_in_either_order() {
        let builtins = format!(
            "{GMOD_CALL_ARG_BUILTINS_FIXTURE}\n---@accessorfunc 3\nfunction AccessorFunc(tab, key, name, force) end\n---@class Panel\nPanel = Panel or {{}}\n---@class PANEL : Panel\nPANEL = Panel\n"
        );
        let panel = |class: &str, accessor: &str| {
            format!(
                "local PANEL = {{}}\nAccessorFunc(PANEL, \"b{accessor}\", \"{accessor}\", FORCE_BOOL)\nvgui.Register(\"{class}\", PANEL, \"DFrame\")\n"
            )
        };
        let paper = panel("ixPaper", "Editable");
        let display = panel("ixCombineDisplay", "MaxLines");
        let data = panel("ixViewData", "CommitOnClose");
        let objectives = panel("ixViewObjectives", "CommitOnClose");
        assert_schedule_independent(&[
            ("lua/includes/builtins.lua", builtins.as_str()),
            (
                "gamemodes/hl2rp/plugins/writing/derma/cl_paper.lua",
                paper.as_str(),
            ),
            (
                "gamemodes/hl2rp/schema/derma/cl_combinedisplay.lua",
                display.as_str(),
            ),
            (
                "gamemodes/hl2rp/schema/derma/cl_viewdata.lua",
                data.as_str(),
            ),
            (
                "gamemodes/hl2rp/schema/derma/cl_viewobjectives.lua",
                objectives.as_str(),
            ),
        ]);
    }

    #[test]
    fn a_global_bootstrapped_in_client_shared_and_server_files_settles_the_same_in_either_order() {
        assert_schedule_independent(&[
            (
                "gamemodes/cityrp/plugins/hitreg/cl_init.lua",
                "LeyHitreg = LeyHitreg or {}
",
            ),
            (
                "gamemodes/cityrp/plugins/hitreg/sh_config.lua",
                "LeyHitreg = LeyHitreg or {}
LeyHitreg.Name = \"x\"
",
            ),
            (
                "gamemodes/cityrp/plugins/hitreg/sv_init.lua",
                "if CLIENT then return end
LeyHitreg = LeyHitreg or {}
",
            ),
        ]);
    }

    #[test]
    fn a_table_element_counted_up_through_a_local_settles() {
        assert!(
            settled(
                Schedule::Canonical,
                &[(
                    "gamemodes/helix/gamemode/core/derma/cl_business.lua",
                    r#"local Shop = {}
function Shop:Init()
    self.cart = {}
end
function Shop:BuyItem(uniqueID)
    local currentCount = self.cart[uniqueID] or 0
    self.cart[uniqueID] = currentCount + 1
end
"#,
                )],
            )
            .is_some()
        );
    }

    /// A global declared in two files answers the same whichever file was
    /// loaded first: the declarations are ordered by where they are, not by
    /// the order the workspace happened to number their files in.
    #[test]
    fn a_global_declared_in_two_files_answers_the_same_in_either_load_order() {
        const TYPES: (&str, &str) = (
            "gamemodes/cityrp/gamemode/core/sh_types.lua",
            "---@class Alpha
---@class Beta
",
        );
        const FIRST_BY_PATH: (&str, &str) = (
            "gamemodes/cityrp/gamemode/core/a_config.lua",
            "---@type Alpha
Config = {}
",
        );
        const LAST_BY_PATH: (&str, &str) = (
            "gamemodes/cityrp/gamemode/core/z_config.lua",
            "---@type Beta
Config = {}
",
        );
        let config_type = |files: &[(&str, &str)]| {
            let mut ws = VirtualWorkspace::new();
            ws.def_file(TYPES.0, TYPES.1);
            for (path, text) in files {
                ws.def_file(path, text);
            }
            ws.def_file(
                "gamemodes/cityrp/gamemode/core/m_uses.lua",
                "usedConfig = Config
",
            );
            let ty = ws.expr_ty("usedConfig");
            ws.humanize_type(ty)
        };

        assert_eq!(config_type(&[FIRST_BY_PATH, LAST_BY_PATH]), "Alpha");
        assert_eq!(config_type(&[LAST_BY_PATH, FIRST_BY_PATH]), "Alpha");
    }

    /// Asserts `files` settle the same cold-built in canonical order, in
    /// reversed order, deferred, and in canonical order one evaluation at a
    /// time.
    pub(crate) fn assert_schedule_independent(files: &[(&str, &str)]) {
        let canonical = settled(Schedule::Canonical, files).expect("canonical order settles");
        let others = [
            (
                "reversed",
                settled(Schedule::Reversed, files).expect("reversed order settles"),
            ),
            (
                "deferred",
                settled(Schedule::Deferred, files).expect("deferred order settles"),
            ),
            (
                "sliced",
                settled_in(
                    Schedule::Canonical,
                    files,
                    false,
                    Build::OneEvaluationAtATime,
                )
                .expect("slices settle"),
            ),
            (
                "two batches",
                settled_in(Schedule::Canonical, files, false, Build::TwoBatches)
                    .expect("two batches settle"),
            ),
        ];
        for (name, other) in others {
            let only_canonical = canonical
                .iter()
                .filter(|line| !other.contains(line))
                .collect::<Vec<_>>();
            let only_other = other
                .iter()
                .filter(|line| !canonical.contains(line))
                .collect::<Vec<_>>();
            assert!(
                only_canonical.is_empty() && only_other.is_empty(),
                "canonical only: {only_canonical:#?}\n{name} only: {only_other:#?}"
            );
        }
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

    const PUSHED_PLAYER: [(&str, &str); 3] = [
        (
            "lua/aaa_annotations/player.lua",
            "---@class Player
",
        ),
        (
            "gamemodes/terrortown/entities/weapons/weapon_ttt_push.lua",
            r#"---@param ply Player
function SWEP:PushPlayer(ply)
    ply.was_pushed = {att = ply, t = CurTime(), wep = self:GetClass()}
end
"#,
        ),
        (
            "gamemodes/terrortown/gamemode/player.lua",
            r#"---@param ent Player
function GM:EntityTakeDamage(ent, owner)
    local push = ent.was_pushed or {}
    if push.att ~= owner then
        push.att = owner
        push.t = CurTime()
        ent.was_pushed = push
    end
end
"#,
        ),
    ];

    #[test]
    fn a_table_assigned_to_a_player_field_other_files_rewrite_keeps_its_own_fields_in_either_order()
    {
        assert_schedule_independent(&PUSHED_PLAYER);
    }

    #[test]
    fn a_table_assigned_to_a_player_field_other_files_rewrite_keeps_its_own_fields_when_its_file_is_analysed_again()
     {
        let gmod = |ws: &mut VirtualWorkspace| {
            let mut emmyrc = Emmyrc::default();
            emmyrc.gmod.enabled = true;
            ws.update_emmyrc(emmyrc);
        };
        let (weapon, text) = PUSHED_PLAYER[1];
        let touched = format!(
            "{text}-- touched
"
        );
        let mut warm = VirtualWorkspace::new();
        gmod(&mut warm);
        warm.def_files(PUSHED_PLAYER.to_vec());
        warm.def_file(weapon, &touched);

        let mut cold = VirtualWorkspace::new();
        gmod(&mut cold);
        let mut files = PUSHED_PLAYER.to_vec();
        files[1].1 = &touched;
        cold.def_files(files);

        assert_eq!(warm.settled_digest(), cold.settled_digest());
    }

    /// A local typed from its uses before an assignment types it.
    const RETYPED_TARGET: [(&str, &str); 3] = [
        (
            "lua/aaa_annotations/player.lua",
            "---@class Player
---@param ply Player
function Kick(ply) end
",
        ),
        (
            "addons/admin/lua/autorun/sh_lib.lua",
            "Lib = {}
function Lib.Make() return Mystery end
Lib.Current = Lib.Make()
",
        ),
        (
            "addons/admin/lua/autorun/server/sv_kick.lua",
            "local target
Kick(target)
target = Lib.Current
",
        ),
    ];

    #[test]
    fn a_local_typed_from_its_uses_before_an_assignment_settles_the_same_in_either_order() {
        assert_schedule_independent(&RETYPED_TARGET);
    }

    /// [`RETYPED_TARGET`] with its library holding `from`, rewritten to `to`:
    /// the same whether the workspace analyses the new library again or cold.
    fn assert_retyped_target_settles_after(from: &str, to: &str) {
        let gmod = |ws: &mut VirtualWorkspace| {
            let mut emmyrc = Emmyrc::default();
            emmyrc.gmod.enabled = true;
            ws.update_emmyrc(emmyrc);
        };
        let (path, _) = RETYPED_TARGET[1];
        let mut files = RETYPED_TARGET.to_vec();
        files[1].1 = from;
        let mut warm = VirtualWorkspace::new();
        gmod(&mut warm);
        warm.def_files(files.clone());
        warm.def_file(path, to);

        let mut cold = VirtualWorkspace::new();
        gmod(&mut cold);
        files[1].1 = to;
        cold.def_files(files);

        assert_eq!(warm.settled_digest(), cold.settled_digest());
    }

    const TYPED_LIB: &str = "Lib = {}
function Lib.Make() return Mystery end
---@type Player
Lib.Current = Lib.Make()
";

    #[test]
    fn a_local_typed_from_its_uses_settles_the_same_when_its_assignment_gains_a_type() {
        assert_retyped_target_settles_after(RETYPED_TARGET[1].1, TYPED_LIB);
    }

    #[test]
    fn a_local_typed_by_its_assignment_settles_the_same_when_the_assignment_loses_its_type() {
        assert_retyped_target_settles_after(TYPED_LIB, RETYPED_TARGET[1].1);
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

    #[test]
    fn a_gamemode_base_alias_its_shared_client_and_server_files_declare_settles_the_same_in_either_order()
     {
        const DERIVE: &str = "DEFINE_BASECLASS(\"gamemode_base\")
function GM:Initialize() end
";
        assert_schedule_independent(&[
            ("gamemodes/sandbox/gamemode/cl_init.lua", DERIVE),
            ("gamemodes/sandbox/gamemode/init.lua", DERIVE),
            ("gamemodes/sandbox/gamemode/shared.lua", DERIVE),
        ]);
    }

    /// Each file's global `PANEL` is its own scratch table: registering it must
    /// not propose the panel's class as the class of every file's `PANEL`,
    /// which sent a file's methods to whichever class won while it was filed.
    /// A `self` field first listed under `PANEL`, while the table still named
    /// it, must leave that listing when its writer runs again.
    #[test]
    fn global_panel_tables_two_files_register_settle_the_same_in_either_order() {
        let builtins = format!(
            "{GMOD_CALL_ARG_BUILTINS_FIXTURE}
---@class Panel
Panel = Panel or {{}}
---@class PANEL : Panel
PANEL = Panel
"
        );
        let panel = |class: &str| {
            format!(
                "PANEL = {{}}
function PANEL:Init()
    self.Label = vgui.Create(\"DLabel\", self)
end
vgui.Register(\"{class}\", PANEL, \"Panel\")
"
            )
        };
        let tips = panel("AnchoredTips");
        let scroll = panel("ScrollLabel");
        assert_schedule_independent(&[
            ("lua/includes/builtins.lua", builtins.as_str()),
            ("gamemodes/terrortown/gamemode/cl_tips.lua", tips.as_str()),
            (
                "gamemodes/terrortown/gamemode/vgui/scrolllabel.lua",
                scroll.as_str(),
            ),
        ]);
    }

    /// The thrown knife reads its `fingerprints` before the closure below
    /// writes it, and that write reads the local back: a loop whose field is
    /// filed only once the statements settled. A batch analysing the knife
    /// after another batch filed every entity's `fingerprints` must not read
    /// them before its own statements settle, as a cold build cannot.
    #[test]
    fn a_field_read_before_its_own_write_settles_the_same_after_a_batch_filed_it_elsewhere() {
        assert_schedule_independent(&[
            (
                "lua/includes/builtins.lua",
                "---@class Entity
---@class Weapon : Entity
---@class Player : Entity
local Player = {}
---@return Weapon
function Player:Give(class) end
ents = {}
---@return Entity
function ents.Create(class) end
",
            ),
            (
                "gamemodes/terrortown/entities/entities/ttt_radio.lua",
                "ENT.Type = \"anim\"
function ENT:Initialize()
   self.fingerprints = {}
end
---@param activator Player
function ENT:UseOverride(activator)
   local prints = self.fingerprints or {}
   local wep = activator:Give(\"weapon_ttt_radio\")
   wep.fingerprints = wep.fingerprints or {}
end
",
            ),
            (
                "gamemodes/terrortown/entities/weapons/weapon_tttbase.lua",
                "SWEP.fingerprints = {}
function SWEP:Equip(newowner)
   self.fingerprints = self.fingerprints or {}
end
",
            ),
            (
                "gamemodes/terrortown/entities/entities/ttt_knife_proj.lua",
                "ENT.Type = \"anim\"
function ENT:StickTo(other)
   local knife = self
   local prints = self.fingerprints
   other.effect_fn = function(rag)
      knife.fingerprints = prints
   end
end
function ENT:BecomeWeapon()
   local wep = ents.Create(\"weapon_ttt_knife\")
   wep.fingerprints = wep.fingerprints or {}
   return wep
end
",
            ),
        ]);
    }
}
