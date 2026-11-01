#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, FileId, VirtualWorkspace};
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    const API_PATH: &str = "gamemodes/terrortown/gamemode/sh_api.lua";
    const PATH: &str = "gamemodes/terrortown/entities/weapons/weapon_ttt_teleport.lua";

    const API: &str = r#"
        ---@class Entity
        ---@field GetPos fun(self: Entity): number
        ---@class Player: Entity
        ---@field SetEyeAngles fun(self: Player, angle: number)
        ---@field IsTerror fun(self: Player): boolean

        ---@return Entity
        function GetOwnerEnt() end

        ---@return Entity?
        function MaybeEnt() end

        timer = {}
        ---@param delay number
        ---@param callback fun()
        function timer.Simple(delay, callback) end
    "#;

    fn enable_gmod(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
    }

    fn workspace(source: &str) -> (VirtualWorkspace, FileId) {
        let mut ws = VirtualWorkspace::new();
        enable_gmod(&mut ws);
        ws.def_file(API_PATH, API);
        let file_id = ws.def_file(PATH, source);
        (ws, file_id)
    }

    fn unguarded_child_lines(ws: &mut VirtualWorkspace, file_id: FileId) -> Vec<u32> {
        let code = DiagnosticCode::InferUnguardedChild;
        ws.analysis.diagnostic.enable_only(code);
        let mut lines = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap()
            .iter()
            .filter(|diagnostic| {
                diagnostic.code == Some(NumberOrString::String(code.get_name().to_string()))
            })
            .map(|diagnostic| diagnostic.range.start.line)
            .collect::<Vec<_>>();
        lines.sort_unstable();
        lines
    }

    #[test]
    fn a_forwarded_entity_still_needs_a_guard_in_the_helper() {
        let source = r#"
            local function Hurt(target)
                target:SetEyeAngles(0)
            end

            ---@param ent Entity
            local function Forward(ent)
                Hurt(ent)
            end

            Forward(GetOwnerEnt())

            local function HurtAgain(target)
                target:SetEyeAngles(0)
            end

            local function Relay(ent)
                HurtAgain(ent)
            end

            Relay(GetOwnerEnt())
        "#;
        let (mut ws, file_id) = workspace(source);
        let reads = source
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains("target:SetEyeAngles(0)"))
            .map(|(line, _)| u32::try_from(line).expect("line fits u32"))
            .collect::<Vec<_>>();
        assert_eq!(reads.len(), 2);
        assert_eq!(unguarded_child_lines(&mut ws, file_id), reads);
    }

    #[test]
    fn a_helper_param_typed_by_its_callers_forwards_its_guarded_narrowing() {
        let source = r#"
            local function Use(value)
                local seen = value
                return seen
            end

            local function Relay(ent)
                if ent then
                    Use(ent)
                end
            end

            Relay(MaybeEnt())
        "#;
        let (ws, file_id) = workspace(source);
        let seen = ws.local_ty(file_id, "seen");
        assert_eq!(ws.humanize_type(seen), "Entity");
    }

    const GUARDED: &str = r#"
        local function WasAvoidable(attacker)
            local seen = attacker
            return attacker:IsTerror()
        end

        KARMA = {}
        function KARMA.Killed(attacker)
            if not attacker:IsPlayer() then return end
            return WasAvoidable(attacker)
        end
    "#;

    const PREDICATE_PATH: &str = "lua/includes/extensions/sh_player_predicate.lua";
    const PREDICATE: &str = r#"
        ---@return boolean
        ---@return_cast self Player
        function Entity:IsPlayer() end
    "#;

    #[test]
    fn undocumented_caller_params_forward_asserted_classes_without_caller_guesses() {
        let (mut ws, file_id) = workspace(GUARDED);
        ws.def_file(PREDICATE_PATH, PREDICATE);
        let seen = ws.local_ty(file_id, "seen");
        assert_eq!(ws.humanize_type(seen), "Player");
        for code in [
            DiagnosticCode::InferUnknown,
            DiagnosticCode::InferUnguardedChild,
        ] {
            ws.analysis.diagnostic.enable_only(code);
            assert_eq!(
                ws.analysis
                    .diagnose_file(file_id, CancellationToken::new())
                    .unwrap(),
                vec![],
            );
        }
    }

    #[test]
    fn asserted_forwarded_params_follow_guard_edits_delete_and_reopen() {
        let unguarded = GUARDED.replace("if not attacker:IsPlayer() then return end", "");
        let mut warm = VirtualWorkspace::assert_warm_matches_cold(
            enable_gmod,
            &[
                (API_PATH, API),
                (PREDICATE_PATH, PREDICATE),
                (PATH, GUARDED),
            ],
            &[(PATH, unguarded.as_str()), (PATH, GUARDED)],
        );
        let file = warm
            .analysis
            .get_file_id(&warm.virtual_url_generator.new_uri(PATH))
            .unwrap();
        let seen = warm.local_ty(file, "seen");
        assert_eq!(warm.humanize_type(seen), "Player");
        let no_cast = PREDICATE.replace("---@return_cast self Player", "");
        for predicate in [no_cast.as_str(), PREDICATE] {
            warm.def_file(PREDICATE_PATH, predicate);
            let (mut cold, cold_file) = workspace(GUARDED);
            cold.def_file(PREDICATE_PATH, predicate);
            assert_eq!(
                warm.local_ty(file, "seen"),
                cold.local_ty(cold_file, "seen")
            );
        }
        let predicate_uri = warm.virtual_url_generator.new_uri(PREDICATE_PATH);
        warm.analysis.remove_file_by_uri(&predicate_uri);
        let (cold, cold_file) = workspace(GUARDED);
        assert_eq!(
            warm.local_ty(file, "seen"),
            cold.local_ty(cold_file, "seen")
        );
        warm.def_file(PREDICATE_PATH, PREDICATE);
        let uri = warm.virtual_url_generator.new_uri(PATH);
        warm.analysis.remove_file_by_uri(&uri);
        let reopened = warm.def_file(PATH, GUARDED);
        let seen = warm.local_ty(reopened, "seen");
        assert_eq!(warm.humanize_type(seen), "Player");
    }

    #[test]
    fn a_type_test_can_assert_an_undocumented_forwarded_param() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        enable_gmod(&mut ws);
        let file = ws.def_file(
            "lua/autorun/sh_guarded_forward.lua",
            r#"
            local function Use(value)
                local seen = value
                return value + 1
            end
            function Forward(value)
                if type(value) == "number" then Use(value) end
            end
        "#,
        );
        let seen = ws.local_ty(file, "seen");
        assert_eq!(ws.humanize_type(seen), "number");
    }

    #[test]
    fn captured_assertions_follow_guard_edits_delete_and_reopen() {
        let guarded = GUARDED.replace(
            "return WasAvoidable(attacker)",
            "local function Thunk() return WasAvoidable(attacker) end; return Thunk()",
        );
        let unguarded = guarded.replace("if not attacker:IsPlayer() then return end", "");
        let mut warm = VirtualWorkspace::assert_warm_matches_cold(
            enable_gmod,
            &[
                (API_PATH, API),
                (PREDICATE_PATH, PREDICATE),
                (PATH, guarded.as_str()),
            ],
            &[(PATH, unguarded.as_str()), (PATH, guarded.as_str())],
        );
        let file = warm
            .analysis
            .get_file_id(&warm.virtual_url_generator.new_uri(PATH))
            .unwrap();
        assert_eq!(
            warm.local_ty(file, "seen"),
            crate::LuaType::Ref(crate::LuaTypeDeclId::global("Player"))
        );
        for source in [unguarded.as_str(), guarded.as_str()] {
            warm.def_file(PATH, source);
            let (mut cold, cold_file) = workspace(source);
            cold.def_file(PREDICATE_PATH, PREDICATE);
            assert_eq!(
                warm.local_ty(file, "seen"),
                cold.local_ty(cold_file, "seen")
            );
            assert_eq!(
                unguarded_child_lines(&mut warm, file),
                unguarded_child_lines(&mut cold, cold_file)
            );
        }
        let uri = warm.virtual_url_generator.new_uri(PATH);
        warm.analysis.remove_file_by_uri(&uri);
        let reopened = warm.def_file(PATH, &guarded);
        assert_eq!(
            warm.local_ty(reopened, "seen"),
            crate::LuaType::Ref(crate::LuaTypeDeclId::global("Player"))
        );
        assert_eq!(
            unguarded_child_lines(&mut warm, reopened),
            Vec::<u32>::new()
        );
    }
}
