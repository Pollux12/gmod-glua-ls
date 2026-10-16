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

    /// `ply` reaches `TeleportPlayer` through two helper params; the read that
    /// needs a guard is `DoTeleport`'s own `IsTerror`, already reported there.
    const CHAIN: &str = r#"
        local function TeleportPlayer(ply)
            ply:SetEyeAngles(0)
        end

        local function DoTeleport(ply)
            if ply:IsTerror() then
                TeleportPlayer(ply)
            end
        end

        local function StartTeleport(ply)
            DoTeleport(ply)
        end

        StartTeleport(GetOwnerEnt())
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

    fn line_of(source: &str, needle: &str) -> u32 {
        let line = source
            .lines()
            .position(|line| line.contains(needle))
            .expect("needle in source");
        u32::try_from(line).expect("line fits u32")
    }

    #[test]
    fn a_helper_param_typed_only_by_its_callers_does_not_forward_through_another_helper() {
        let (mut ws, file_id) = workspace(CHAIN);
        assert_eq!(
            unguarded_child_lines(&mut ws, file_id),
            vec![line_of(CHAIN, "ply:IsTerror()")]
        );
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

    #[test]
    fn forwarded_helper_params_follow_edits_delete_and_reopen_like_cold_analysis() {
        let documented = CHAIN.replace(
            "local function StartTeleport(ply)",
            "---@param ply Player\n        local function StartTeleport(ply)",
        );
        let expected_chain = vec![line_of(CHAIN, "ply:IsTerror()")];
        let mut ws = VirtualWorkspace::assert_warm_matches_cold(
            enable_gmod,
            &[(API_PATH, API), (PATH, CHAIN)],
            &[
                (PATH, documented.as_str()),
                (PATH, CHAIN),
                (PATH, documented.as_str()),
            ],
        );
        let file_id = ws
            .analysis
            .get_file_id(&ws.virtual_url_generator.new_uri(PATH))
            .expect("edited file");
        assert_eq!(unguarded_child_lines(&mut ws, file_id), Vec::<u32>::new());

        let reverted = ws.def_file(PATH, CHAIN);
        assert_eq!(unguarded_child_lines(&mut ws, reverted), expected_chain);

        let uri = ws.virtual_url_generator.new_uri(PATH);
        let _ = ws.analysis.update_file_by_uri(&uri, None);
        let reopened = ws.def_file(PATH, CHAIN);
        assert_eq!(unguarded_child_lines(&mut ws, reopened), expected_chain);
        let (mut cold, cold_file) = workspace(CHAIN);
        assert_eq!(
            unguarded_child_lines(&mut cold, cold_file),
            unguarded_child_lines(&mut ws, reopened)
        );
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
    fn a_partial_or_overwritten_guard_does_not_authorize_a_forwarded_guess() {
        for guard in [
            "if attacker:IsPlayer() then local valid = attacker end",
            "if not attacker:IsPlayer() then return end; attacker = GetOwnerEnt()",
        ] {
            let source = GUARDED.replace("if not attacker:IsPlayer() then return end", guard);
            let (mut ws, file) = workspace(&source);
            ws.def_file(PREDICATE_PATH, PREDICATE);
            ws.analysis
                .diagnostic
                .enable_only(DiagnosticCode::InferUnknown);
            let actual = ws
                .analysis
                .diagnose_file(file, CancellationToken::new())
                .unwrap()
                .into_iter()
                .map(|diagnostic| (diagnostic.range, diagnostic.message))
                .collect::<Vec<_>>();
            let line = line_of(&source, "return attacker:IsTerror()");
            let column = source
                .lines()
                .nth(line as usize)
                .unwrap()
                .find("attacker:IsTerror")
                .unwrap() as u32;
            assert_eq!(
                actual,
                vec![(
                    lsp_types::Range::new(
                        lsp_types::Position::new(line, column),
                        lsp_types::Position::new(line, column + "attacker:IsTerror".len() as u32)
                    ),
                    "Type `Player` was inferred from usage context and may be incorrect."
                        .to_string(),
                )],
                "{guard}"
            );
        }
    }

    #[test]
    fn captured_params_keep_asserted_types_separate_from_caller_inference() {
        use crate::{CacheOptions, LuaInferCache, LuaType, LuaTypeDeclId};
        use glua_parser::{LuaAstNode, LuaCallExpr};

        for (guard, expected) in [
            ("", None),
            (
                "if not ent:IsPlayer() then return end",
                Some(LuaType::Ref(LuaTypeDeclId::global("Player"))),
            ),
            ("ent = 17", Some(LuaType::IntegerConst(17))),
        ] {
            for capture in [
                "local function Thunk() Use(ent) end; Thunk()",
                "timer.Simple(0, function() Use(ent) end)",
            ] {
                let source = format!(
                    r#"
                    local function Use(value) return value end
                    local function Relay(ent)
                        {guard}
                        {capture}
                    end
                    Relay(GetOwnerEnt())
                "#
                );
                let (mut ws, file) = workspace(&source);
                ws.def_file(PREDICATE_PATH, PREDICATE);
                let model = ws.analysis.compilation.get_semantic_model(file).unwrap();
                let call = model
                    .get_root()
                    .descendants::<LuaCallExpr>()
                    .find(|call| {
                        call.get_prefix_expr()
                            .is_some_and(|prefix| prefix.syntax().text() == "Use")
                    })
                    .unwrap();
                let argument = call.get_args_list().unwrap().get_args().next().unwrap();
                let mut cache = LuaInferCache::new(file, CacheOptions::default());
                let actual = crate::semantic::infer_asserted_argument_type(
                    model.get_db(),
                    &mut cache,
                    argument,
                );
                assert_eq!(actual, expected, "{guard}; {capture}");
            }
        }
    }

    #[test]
    fn a_captured_forwarding_read_does_not_duplicate_the_teleport_warning() {
        for capture in [
            "local function Thunk() TeleportPlayer(ply) end; Thunk()",
            "timer.Simple(0, function() TeleportPlayer(ply) end)",
        ] {
            let source = CHAIN.replace(
                "TeleportPlayer(ply)\n            end",
                &format!("{capture}\n            end"),
            );
            let (mut ws, file) = workspace(&source);
            assert_eq!(
                unguarded_child_lines(&mut ws, file),
                vec![line_of(&source, "ply:IsTerror()")],
                "{capture}"
            );
        }
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
