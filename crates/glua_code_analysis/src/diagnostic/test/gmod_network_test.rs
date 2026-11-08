#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, GmodRealm, NetOpDirection, VirtualWorkspace};
    use glua_parser::{LuaAstNode, LuaCallExpr};
    use googletest::prelude::*;
    use lsp_types::{Diagnostic, NumberOrString};
    use tokio_util::sync::CancellationToken;

    fn new_gmod_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        // Net ops are recognized through signature metadata, so the annotated
        // builtins must be present or no flows are collected at all.
        ws.def_gmod_call_arg_builtins();
        ws
    }

    fn diagnostic_code(code: DiagnosticCode) -> Option<NumberOrString> {
        Some(NumberOrString::String(code.get_name().to_string()))
    }

    fn file_diagnostics(ws: &mut VirtualWorkspace, file_id: crate::FileId) -> Vec<Diagnostic> {
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
    }

    fn count_diagnostic(diagnostics: &[Diagnostic], code: DiagnosticCode) -> usize {
        let expected_code = diagnostic_code(code);
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == expected_code)
            .count()
    }

    /// The `(line, start column, end column)` of a diagnostic's range.
    fn diagnostic_span(diagnostic: &Diagnostic) -> (u32, u32, u32) {
        let range = diagnostic.range;
        assert_eq!(
            range.start.line, range.end.line,
            "expected a one-line range"
        );
        (range.start.line, range.start.character, range.end.character)
    }

    fn count_network_diagnostics(diagnostics: &[Diagnostic]) -> usize {
        count_diagnostic(diagnostics, DiagnosticCode::GmodNetReadWriteTypeMismatch)
            + count_diagnostic(diagnostics, DiagnosticCode::GmodNetReadWriteOrderMismatch)
            + count_diagnostic(
                diagnostics,
                DiagnosticCode::GmodNetMissingNetworkCounterpart,
            )
    }

    #[gtest]
    fn test_type_mismatch_triggers_warning() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteTypeMismatch);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("Msg")
            net.Start("Msg")
            net.WriteEntity(e)
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("Msg", function()
                local x = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        let mismatch = diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.code == diagnostic_code(DiagnosticCode::GmodNetReadWriteTypeMismatch)
            })
            .expect("expected gmod-net-read-write-type-mismatch diagnostic");

        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteTypeMismatch),
            eq(1usize)
        );
        expect_that!(
            mismatch
                .message
                .contains("expected `net.ReadEntity`, got `net.ReadString`"),
            eq(true)
        );
        // Reported on the read itself, not the whole receive block.
        expect_that!(diagnostic_span(mismatch), eq((2, 26, 42)));
    }

    #[gtest]
    fn test_order_mismatch_triggers_warning() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteOrderMismatch);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("Msg")
            net.Start("Msg")
            net.WriteEntity(e)
            net.WriteString("name")
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("Msg", function()
                local name = net.ReadString()
                local ent = net.ReadEntity()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteOrderMismatch),
            eq(1usize)
        );
        // Reported on the first read out of order, not the whole receive block.
        expect_that!(diagnostic_span(&diagnostics[0]), eq((2, 29, 45)));
    }

    /// A mismatched read composed from a helper in another file has no range
    /// in the receiving file, so the warning stays on the receive block.
    #[gtest]
    fn test_mismatch_from_helper_in_other_file_reports_on_receive_block() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteTypeMismatch);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("Msg")
            net.Start("Msg")
            net.WriteEntity(e)
            net.Broadcast()
            "#,
        );
        ws.def_file(
            "lua/autorun/client/cl_helpers.lua",
            r#"
            function ReadPayloadName()
                return net.ReadString()
            end
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("Msg", function()
                local name = ReadPayloadName()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        let mismatch = diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.code == diagnostic_code(DiagnosticCode::GmodNetReadWriteTypeMismatch)
            })
            .expect("expected gmod-net-read-write-type-mismatch diagnostic");
        expect_that!(mismatch.range.start.line, eq(1));
        expect_that!(mismatch.range.end.line, eq(3));
    }

    #[gtest]
    fn test_missing_receiver_triggers_warning() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let server_file_id = ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("Orphan")
            net.Start("Orphan")
            net.WriteString("hello")
            -- exercise new send method
            net.SendPAS(Vector(0,0,0))
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        assert_that!(
            count_diagnostic(
                &diagnostics,
                DiagnosticCode::GmodNetMissingNetworkCounterpart
            ),
            eq(1usize)
        );
    }

    #[gtest]
    fn test_static_wrapper_reports_one_missing_receiver_at_call_site() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let server_file_id = ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("WrappedOrphan")

            local function sendWrappedOrphan()
                net.Start("WrappedOrphan")
                net.WriteString("hello")
                net.Broadcast()
            end

            sendWrappedOrphan()
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        assert_that!(
            count_diagnostic(
                &diagnostics,
                DiagnosticCode::GmodNetMissingNetworkCounterpart
            ),
            eq(1usize)
        );
    }

    fn missing_counterpart_messages(diagnostics: &[Diagnostic]) -> Vec<&str> {
        let expected_code = diagnostic_code(DiagnosticCode::GmodNetMissingNetworkCounterpart);
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == expected_code)
            .map(|diagnostic| diagnostic.message.as_str())
            .collect()
    }

    #[gtest]
    fn test_server_receiver_does_not_satisfy_server_send() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let server_file_id = ws.def_file(
            "lua/autorun/server/send_and_receive.lua",
            r#"
            util.AddNetworkString("SameRealm")
            net.Start("SameRealm")
            net.WriteUInt(3, 8)
            net.Broadcast()
            net.Receive("SameRealm", function()
                local n = net.ReadUInt(8)
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        let messages = missing_counterpart_messages(&diagnostics);
        assert_that!(messages.len(), eq(2usize));
        expect_that!(
            messages
                .iter()
                .any(|message| message.contains("`net.Receive` counterpart")
                    && message.contains("client realm")),
            eq(true)
        );
        expect_that!(
            messages
                .iter()
                .any(|message| message.contains("sending counterpart")
                    && message.contains("client realm")),
            eq(true)
        );
    }

    #[gtest]
    fn test_client_receiver_does_not_satisfy_send_to_server() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let client_file_id = ws.def_file(
            "lua/autorun/client/send_and_receive.lua",
            r#"
            net.Start("ToServer")
            net.SendToServer()
            net.Receive("ToServer", function() end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        let messages = missing_counterpart_messages(&diagnostics);
        assert_that!(messages.len(), eq(2usize));
        expect_that!(
            messages
                .iter()
                .any(|message| message.contains("`net.Receive` counterpart")
                    && message.contains("server realm")),
            eq(true)
        );
        expect_that!(
            messages
                .iter()
                .any(|message| message.contains("sending counterpart")
                    && message.contains("server realm")),
            eq(true)
        );
    }

    #[gtest]
    fn test_shared_receiver_satisfies_server_send() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let server_file_id = ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("SharedReceiver")
            net.Start("SharedReceiver")
            net.Broadcast()
            "#,
        );
        ws.def_file(
            "lua/autorun/sh_receive.lua",
            r#"
            net.Receive("SharedReceiver", function() end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        assert_that!(
            count_diagnostic(
                &diagnostics,
                DiagnosticCode::GmodNetMissingNetworkCounterpart
            ),
            eq(0usize)
        );
    }

    #[gtest]
    fn test_never_called_sending_function_reports_missing_receiver_once() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let server_file_id = ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("NeverCalled")

            function SendNeverCalled()
                net.Start("NeverCalled")
                net.WriteString("hello")
                net.Broadcast()
            end
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        assert_that!(
            count_diagnostic(
                &diagnostics,
                DiagnosticCode::GmodNetMissingNetworkCounterpart
            ),
            eq(1usize)
        );
    }

    #[gtest]
    fn test_menu_receiver_does_not_satisfy_server_send() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let server_file_id = ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("MenuReceiver")
            net.Start("MenuReceiver")
            net.Broadcast()
            "#,
        );
        ws.def_file(
            "lua/menu/receive.lua",
            r#"
            ---@realm menu

            net.Receive("MenuReceiver", function() end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        assert_that!(missing_counterpart_messages(&diagnostics).len(), eq(1usize));
    }

    #[gtest]
    fn test_menu_receiver_does_not_satisfy_uncalled_start_only_wrapper() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let server_file_id = ws.def_file(
            "lua/autorun/server/sv_prepare.lua",
            r#"
            util.AddNetworkString("Prepared")
            function PrepareMessage()
                net.Start("Prepared")
            end
            "#,
        );
        ws.def_file(
            "lua/menu/receive.lua",
            r#"
            ---@realm menu

            net.Receive("Prepared", function() end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        assert_that!(missing_counterpart_messages(&diagnostics).len(), eq(1usize));
    }

    #[gtest]
    fn test_dead_branch_receiver_does_not_satisfy_uncalled_start_only_wrapper() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let server_file_id = ws.def_file(
            "lua/autorun/server/sv_prepare.lua",
            r#"
            util.AddNetworkString("PreparedDead")
            function PrepareMessage()
                net.Start("PreparedDead")
            end
            "#,
        );
        ws.def_file(
            "lua/autorun/server/sv_dead.lua",
            r#"
            if CLIENT then
                net.Receive("PreparedDead", function() end)
            end
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        assert_that!(missing_counterpart_messages(&diagnostics).len(), eq(1usize));
    }

    #[gtest]
    fn test_unreachable_receiver_with_a_live_pair_is_not_reported() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let send_id = ws.def_file(
            "lua/autorun/server/sv_send.lua",
            r#"
            util.AddNetworkString("DeadBranch")
            net.Start("DeadBranch")
            net.Broadcast()
            "#,
        );
        let receive_id = ws.def_file(
            "lua/autorun/client/cl_receive.lua",
            r#"
            net.Receive("DeadBranch", function() end)
            "#,
        );
        let dead_id = ws.def_file(
            "lua/autorun/server/sv_dead.lua",
            r#"
            if CLIENT then
                net.Receive("DeadBranch", function() end)
            end
            "#,
        );
        let orphan_id = ws.def_file(
            "lua/autorun/client/cl_orphan.lua",
            r#"
            net.Receive("Orphan", function() end)
            "#,
        );

        for file_id in [send_id, receive_id, dead_id] {
            let diagnostics = file_diagnostics(&mut ws, file_id);
            expect_that!(missing_counterpart_messages(&diagnostics).len(), eq(0usize));
        }
        let diagnostics = file_diagnostics(&mut ws, orphan_id);
        expect_that!(missing_counterpart_messages(&diagnostics).len(), eq(1usize));
    }

    fn nested_guard_missing_count(send_path: &str, send_text: &str, receive_text: &str) -> usize {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);
        let send_id = ws.def_file(send_path, send_text);
        ws.def_file("lua/autorun/sh_receive.lua", receive_text);
        let diagnostics = file_diagnostics(&mut ws, send_id);
        missing_counterpart_messages(&diagnostics).len()
    }

    #[gtest]
    fn test_server_inside_client_guard_receiver_does_not_satisfy_server_send() {
        let missing = nested_guard_missing_count(
            "lua/autorun/server/sv_send.lua",
            r#"
            util.AddNetworkString("NestedDead")
            net.Start("NestedDead")
            net.Broadcast()
            "#,
            r#"
            if SERVER then
                if CLIENT then
                    net.Receive("NestedDead", function() end)
                end
            end
            "#,
        );
        expect_that!(missing, eq(1usize));
    }

    #[gtest]
    fn test_client_inside_server_guard_receiver_does_not_satisfy_client_send() {
        let missing = nested_guard_missing_count(
            "lua/autorun/client/cl_send.lua",
            r#"
            net.Start("NestedDeadUp")
            net.SendToServer()
            "#,
            r#"
            if CLIENT then
                if SERVER then
                    net.Receive("NestedDeadUp", function() end)
                end
            end
            "#,
        );
        expect_that!(missing, eq(1usize));
    }

    #[gtest]
    fn test_same_realm_nested_guard_receiver_satisfies_send() {
        let missing = nested_guard_missing_count(
            "lua/autorun/server/sv_send.lua",
            r#"
            util.AddNetworkString("NestedLive")
            net.Start("NestedLive")
            net.Broadcast()
            "#,
            r#"
            if CLIENT then
                if CLIENT then
                    net.Receive("NestedLive", function() end)
                end
            end
            "#,
        );
        expect_that!(missing, eq(0usize));
    }

    #[gtest]
    fn test_unreachable_client_branch_receiver_does_not_satisfy_server_send() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let server_file_id = ws.def_file(
            "lua/autorun/server/sv_send.lua",
            r#"
            util.AddNetworkString("DeadBranch")
            net.Start("DeadBranch")
            net.Broadcast()
            "#,
        );
        ws.def_file(
            "lua/autorun/server/sv_dead.lua",
            r#"
            if CLIENT then
                net.Receive("DeadBranch", function() end)
            end
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        assert_that!(missing_counterpart_messages(&diagnostics).len(), eq(1usize));
    }

    #[gtest]
    fn test_complete_wrapped_server_send_does_not_satisfy_server_receiver() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let server_file_id = ws.def_file(
            "lua/autorun/server/sv_example.lua",
            r#"
            util.AddNetworkString("Direction")
            function SendDirection()
                net.Start("Direction")
                net.Broadcast()
            end
            hook.Add("PlayerInitialSpawn", "Direction", SendDirection)
            net.Receive("Direction", function() end)
            "#,
        );
        ws.def_file(
            "lua/autorun/client/cl_example.lua",
            r#"
            net.Receive("Direction", function() end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        let messages = missing_counterpart_messages(&diagnostics);
        assert_that!(messages.len(), eq(1usize));
        expect_that!(
            messages[0].contains("sending counterpart") && messages[0].contains("client realm"),
            eq(true)
        );
    }

    /// The call in `file_id` whose callee is the name `callee` names two
    /// inferred signatures and no declared or unique one.
    fn assert_ambiguous_callee(ws: &VirtualWorkspace, file_id: crate::FileId, callee: &str) {
        let db = ws.analysis.compilation.get_db();
        let root = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("the file is parsed")
            .get_chunk_node();
        let call = root
            .descendants::<LuaCallExpr>()
            .find(|call| {
                call.get_prefix_expr()
                    .is_some_and(|prefix| prefix.syntax().text() == callee)
            })
            .expect("the callee is called");
        let target = db
            .get_call_target_index()
            .target(file_id, call.get_range())
            .expect("the call's target is stored");
        expect_that!(target.declared, eq(None));
        expect_that!(target.unique, eq(None));
        expect_that!(target.inferred.len(), eq(2usize));
        expect_that!(target.inferred[0] != target.inferred[1], eq(true));
    }

    fn ambiguous_callee_mismatch_count(writers_first: bool, pairs_reversed: bool) -> usize {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteTypeMismatch);

        let put_text = r#"
            ---@param value any
            ---@[net_payload("write", "string")]
            function PutText(value) net.WriteString(value) end
        "#;
        let put_flag = r#"
            ---@param value any
            ---@[net_payload("write", "bool")]
            function PutFlag(value) net.WriteBool(value) end
        "#;
        let get_flag = r#"
            ---@return boolean
            ---@[net_payload("read", "bool")]
            function GetFlag() return net.ReadBool() end
        "#;
        let get_text = r#"
            ---@return string
            ---@[net_payload("read", "string")]
            function GetText() return net.ReadString() end
        "#;
        let (writers, readers) = if pairs_reversed {
            (
                format!("{put_flag}{put_text}"),
                format!("{get_text}{get_flag}"),
            )
        } else {
            (
                format!("{put_text}{put_flag}"),
                format!("{get_flag}{get_text}"),
            )
        };
        let library = if writers_first {
            format!("{writers}{readers}")
        } else {
            format!("{readers}{writers}")
        };
        ws.def_file("lua/autorun/sh_choice_lib.lua", &library);
        let server_file_id = ws.def_file(
            "lua/autorun/server/sv_send.lua",
            r#"
            local useText = math.random() > 0.5
            local writer = useText and PutText or PutFlag
            util.AddNetworkString("Choice")
            net.Start("Choice")
            writer(useText and "hello" or true)
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/cl_receive.lua",
            r#"
            net.Receive("Choice", function()
                local useText = math.random() > 0.5
                local reader = useText and GetText or GetFlag
                local value = reader()
            end)
            "#,
        );

        assert_ambiguous_callee(&ws, server_file_id, "writer");
        assert_ambiguous_callee(&ws, client_file_id, "reader");
        let server = file_diagnostics(&mut ws, server_file_id);
        let client = file_diagnostics(&mut ws, client_file_id);
        count_diagnostic(&server, DiagnosticCode::GmodNetReadWriteTypeMismatch)
            + count_diagnostic(&client, DiagnosticCode::GmodNetReadWriteTypeMismatch)
    }

    #[gtest]
    fn test_ambiguous_callee_alternatives_report_no_mismatch_writers_first() {
        expect_that!(ambiguous_callee_mismatch_count(true, false), eq(0usize));
    }

    #[gtest]
    fn test_ambiguous_callee_alternatives_report_no_mismatch_readers_first_pairs_reversed() {
        expect_that!(ambiguous_callee_mismatch_count(false, true), eq(0usize));
    }

    #[gtest]
    fn test_missing_sender_triggers_warning() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("NoSender", function()
                local x = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(
                &diagnostics,
                DiagnosticCode::GmodNetMissingNetworkCounterpart
            ),
            eq(1usize)
        );
    }

    #[gtest]
    fn test_correct_matching_has_no_network_diagnostics() {
        let mut ws = new_gmod_workspace();

        let server_file_id = ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("Match")
            net.Start("Match")
            net.WriteEntity(e)
            net.WriteString("name")
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("Match", function()
                local ent = net.ReadEntity()
                local name = net.ReadString()
            end)
            "#,
        );

        let server_diagnostics = file_diagnostics(&mut ws, server_file_id);
        let client_diagnostics = file_diagnostics(&mut ws, client_file_id);

        assert_that!(count_network_diagnostics(&server_diagnostics), eq(0usize));
        assert_that!(count_network_diagnostics(&client_diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_multiple_senders_one_matches_receiver_has_no_mismatch_diagnostic() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("Msg")

            net.Start("Msg")
            net.WriteUInt(1, 8)
            net.WriteString("name")
            net.WriteBool(true)
            net.Broadcast()

            net.Start("Msg")
            net.WriteUInt(2, 8)
            net.WriteString("name")
            net.WriteBool(true)
            net.WriteUInt(10, 8)
            net.WriteData("abc", 3)
            net.Send(Entity(1))
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("Msg", function()
                local id = net.ReadUInt(8)
                local name = net.ReadString()
                local ok = net.ReadBool()
                local count = net.ReadUInt(8)
                local payload = net.ReadData(3)
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteTypeMismatch),
            eq(0usize)
        );
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteOrderMismatch),
            eq(0usize)
        );
    }

    #[gtest]
    fn test_multiple_senders_with_control_flow_writer_avoids_false_count_mismatch() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteOrderMismatch);

        ws.def_file(
            "lua/autorun/server/send_simple.lua",
            r#"
            util.AddNetworkString("CopiedDupe")

            net.Start("CopiedDupe")
            net.WriteUInt(1, 1)
            net.WriteVector(Vector(0, 0, 0))
            net.WriteVector(Vector(1, 1, 1))
            net.WriteString("simple")
            net.WriteUInt(10, 24)
            net.WriteUInt(0, 16)
            net.WriteUInt(20, 24)
            net.Broadcast()
            "#,
        );

        ws.def_file(
            "lua/autorun/server/send_control_flow.lua",
            r#"
            net.Start("CopiedDupe")
            net.WriteUInt(1, 1)
            net.WriteVector(Vector(0, 0, 0))
            net.WriteVector(Vector(1, 1, 1))
            net.WriteString("with_addons")
            net.WriteUInt(10, 24)

            local addon_count = 1
            net.WriteUInt(addon_count, 16)
            if ( addon_count > 0 ) then
                for _, wsid in ipairs({ "123456" }) do
                    net.WriteString(wsid)
                end
            end
            net.WriteUInt(20, 24)
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("CopiedDupe", function()
                local can_save = net.ReadUInt(1)
                local mins = net.ReadVector()
                local maxs = net.ReadVector()
                local name = net.ReadString()
                local ent_count = net.ReadUInt(24)
                local workshop_count = net.ReadUInt(16)
                for _ = 1, workshop_count do
                    net.ReadString()
                end
                local constraint_count = net.ReadUInt(24)
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteOrderMismatch),
            eq(0usize)
        );
    }

    #[gtest]
    fn test_diagnostics_disable_suppresses_type_mismatch() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.diagnostics.disable = vec![DiagnosticCode::GmodNetReadWriteTypeMismatch];
        ws.update_emmyrc(emmyrc);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("Msg")
            net.Start("Msg")
            net.WriteEntity(e)
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("Msg", function()
                local x = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteTypeMismatch),
            eq(0usize)
        );
    }

    #[gtest]
    fn test_diagnostics_disable_suppresses_missing_counterpart() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.diagnostics.disable = vec![DiagnosticCode::GmodNetMissingNetworkCounterpart];
        ws.update_emmyrc(emmyrc);

        let server_file_id = ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("Orphan")
            net.Start("Orphan")
            net.WriteString("hello")
            net.SendPAS(Vector(0,0,0))
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        assert_that!(
            count_diagnostic(
                &diagnostics,
                DiagnosticCode::GmodNetMissingNetworkCounterpart
            ),
            eq(0usize)
        );
    }

    #[gtest]
    fn test_dynamic_message_names_do_not_cause_missing_counterpart_diagnostic() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let server_file_id = ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            local msg = "dynamic"
            net.Start(msg)
            net.WriteString("test")
            net.Broadcast()
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        assert_that!(
            count_diagnostic(
                &diagnostics,
                DiagnosticCode::GmodNetMissingNetworkCounterpart
            ),
            eq(0usize)
        );
    }

    #[gtest]
    fn test_wrapped_start_without_send_suppresses_missing_sender_counterpart() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            function Glide.StartCommand(id)
                net.Start("glide.command")
                net.WriteUInt(id, 8)
            end

            Glide.StartCommand(1)
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("glide.command", function()
                local x = net.ReadUInt(8)
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(
                &diagnostics,
                DiagnosticCode::GmodNetMissingNetworkCounterpart
            ),
            eq(0usize)
        );
    }

    #[gtest]
    fn test_table_literal_wrapped_start_suppresses_missing_sender_counterpart() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        ws.def_file(
            "lua/autorun/client/send.lua",
            r#"
            local meta = {
                MsgStart = function(self)
                    net.Start("properties")
                    net.WriteString(self.InternalName)
                end,
                MsgEnd = function(self)
                    net.SendToServer()
                end,
            }

            meta:MsgStart()
            meta:MsgEnd()
            "#,
        );

        let server_file_id = ws.def_file(
            "lua/autorun/server/receive.lua",
            r#"
            util.AddNetworkString("properties")
            net.Receive("properties", function()
                local name = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        assert_that!(
            count_diagnostic(
                &diagnostics,
                DiagnosticCode::GmodNetMissingNetworkCounterpart
            ),
            eq(0usize)
        );
    }

    // ---- Dynamic read/write tests (writes/reads inside if/for/while/repeat
    // are treated as 0..N occurrences of their kind, eliminating false
    // positives when one side uses a runtime-decided loop or branch).

    #[gtest]
    fn test_dynamic_writer_loop_matches_dynamic_reader_loop() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("Inv.Sync")
            net.Start("Inv.Sync")
            net.WriteUInt(3, 16)
            for _, item in ipairs({"a","b","c"}) do
                net.WriteString(item)
            end
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("Inv.Sync", function()
                local count = net.ReadUInt(16)
                for _ = 1, count do
                    local name = net.ReadString()
                end
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_dynamic_writer_loop_matches_fixed_reader_loop_with_count_param() {
        let mut ws = new_gmod_workspace();

        // Writer loops conditionally; reader has only the count read declared
        // before the dynamic for-loop reads. The count itself comes back as
        // a UInt; the body reads strings.
        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("NRP.Inventory:Sync")
            net.Start("NRP.Inventory:Sync")
            net.WriteUInt(2, 16)
            for _, slot in ipairs({"a","b"}) do
                net.WriteString(slot)
                net.WriteUInt(1, 8)
            end
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("NRP.Inventory:Sync", function()
                local n = net.ReadUInt(16)
                for _ = 1, n do
                    local slot = net.ReadString()
                    local qty = net.ReadUInt(8)
                end
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_dynamic_writer_in_if_branch_does_not_trigger_count_mismatch() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("Notify")
            net.Start("Notify")
            net.WriteString("hi")
            local extra = false
            if extra then
                net.WriteString("payload")
            end
            net.Broadcast()
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("Notify", function()
                local msg = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_dynamic_count_only_mismatch_reports_order_mismatch() {
        // Regression: when DP matching rejects a pair but the first mismatch
        // scanner cannot pin a concrete position (dynamic count-only shape
        // mismatch), we should still emit a mismatch diagnostic.
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteOrderMismatch);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("SilentDynMismatch")
            net.Start("SilentDynMismatch")
            net.WriteUInt(1, 8)
            if has_extra then
                net.WriteString("optional")
            end
            net.Broadcast()
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("SilentDynMismatch", function()
                local id = net.ReadUInt(8)
                local s = net.ReadString()
                local ok = net.ReadBool()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteOrderMismatch),
            eq(1usize)
        );
    }

    #[gtest]
    fn test_dynamic_writer_in_if_else_branches_matches_when_present() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("Branchy")
            net.Start("Branchy")
            net.WriteBool(true)
            if SomeCond then
                net.WriteString("x")
            else
                net.WriteString("y")
            end
            net.Broadcast()
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("Branchy", function()
                local ok = net.ReadBool()
                local s = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_dynamic_writer_while_loop_matches_dynamic_reader_while_loop() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("Stream")
            net.Start("Stream")
            net.WriteUInt(1, 8)
            local i = 0
            while i < 3 do
                net.WriteFloat(0.5)
                i = i + 1
            end
            net.WriteBool(true)
            net.Broadcast()
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("Stream", function()
                local kind = net.ReadUInt(8)
                while net.ReadBool() do
                    local v = net.ReadFloat()
                end
            end)
            "#,
        );

        // Reader's `while net.ReadBool() do net.ReadFloat() end` is dynamic on
        // both ops; writer pattern (UInt, dynamic Float, Bool) should still
        // resolve under the regex-style match.
        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_dynamic_writer_repeat_loop_matches_dynamic_reader_loop() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("RepeatMsg")
            net.Start("RepeatMsg")
            net.WriteUInt(2, 8)
            local i = 0
            repeat
                net.WriteString("x")
                i = i + 1
            until i >= 2
            net.Broadcast()
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("RepeatMsg", function()
                local n = net.ReadUInt(8)
                for _ = 1, n do
                    local s = net.ReadString()
                end
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_dynamic_writer_with_nested_if_inside_for() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("NestedDyn")
            net.Start("NestedDyn")
            net.WriteUInt(2, 8)
            for _, p in ipairs({1,2}) do
                if p > 0 then
                    net.WriteString("ok")
                end
            end
            net.Broadcast()
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("NestedDyn", function()
                local n = net.ReadUInt(8)
                for _ = 1, n do
                    local s = net.ReadString()
                end
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_real_world_inventory_sync_pattern() {
        // Mirrors the bug report's `NRP.Inventory:Sync` shape: a fixed header
        // followed by a runtime-counted body of mixed-type entries.
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/inventory_send.lua",
            r#"
            util.AddNetworkString("NRP.Inventory:Sync")
            net.Start("NRP.Inventory:Sync")
            net.WriteEntity(LocalPlayer())
            net.WriteUInt(3, 16)
            for _, slot in ipairs({"a","b","c"}) do
                net.WriteString(slot)
                net.WriteUInt(1, 16)
                net.WriteString("Item")
                net.WriteFloat(0.0)
                net.WriteBool(true)
            end
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/inventory_receive.lua",
            r#"
            net.Receive("NRP.Inventory:Sync", function()
                local owner = net.ReadEntity()
                local count = net.ReadUInt(16)
                for _ = 1, count do
                    local slot = net.ReadString()
                    local id = net.ReadUInt(16)
                    local class = net.ReadString()
                    local dur = net.ReadFloat()
                    local equipped = net.ReadBool()
                end
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_genuine_type_mismatch_still_reported_with_dynamic_present() {
        let mut ws = new_gmod_workspace();

        // Writer's first fixed slot is WriteEntity, reader's first read is
        // ReadString — that's a real, non-dynamic mismatch that should still
        // surface even when later positions contain dynamic ops. The system
        // may classify it as either type or order mismatch (since ReadString
        // does appear later), but at least one network diagnostic must fire.
        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("MismatchPlusDyn")
            net.Start("MismatchPlusDyn")
            net.WriteEntity(e)
            for _ = 1, 2 do
                net.WriteString("x")
            end
            net.Broadcast()
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("MismatchPlusDyn", function()
                local s = net.ReadString()
                for _ = 1, 2 do
                    local x = net.ReadString()
                end
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        let mismatches =
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteTypeMismatch)
                + count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteOrderMismatch);
        assert_that!(mismatches, ge(1usize));
    }

    #[gtest]
    fn test_dynamic_only_reader_with_fixed_header_writer() {
        // Writer never enters its conditional branch (in source) — but reader
        // wraps reads in a `for` over a count. Both sides should resolve as
        // matching since dynamic ops absorb 0..N.
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("HeaderOnly")
            net.Start("HeaderOnly")
            net.WriteUInt(0, 8)
            net.Broadcast()
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("HeaderOnly", function()
                local n = net.ReadUInt(8)
                for _ = 1, n do
                    local s = net.ReadString()
                end
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_dynamic_writer_only_with_fixed_reader_payload() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("DynWriter")
            net.Start("DynWriter")
            net.WriteString("hdr")
            for _ = 1, 0 do
                net.WriteString("x")
            end
            net.WriteBool(true)
            net.Broadcast()
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("DynWriter", function()
                local s = net.ReadString()
                local b = net.ReadBool()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_real_mismatch_with_no_dynamic_still_reported() {
        // Sanity: ensure DP path doesn't accidentally hide real mismatches.
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteOrderMismatch);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("BadFixed")
            net.Start("BadFixed")
            net.WriteString("a")
            net.WriteString("b")
            net.Broadcast()
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("BadFixed", function()
                local s = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteOrderMismatch),
            eq(1usize)
        );
    }

    #[gtest]
    fn test_elseif_chain_with_writes_treated_as_dynamic() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("Elseif")
            net.Start("Elseif")
            net.WriteUInt(1, 4)
            if x == 1 then
                net.WriteString("a")
            elseif x == 2 then
                net.WriteString("b")
                net.WriteString("c")
            else
                net.WriteString("d")
            end
            net.Broadcast()
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("Elseif", function()
                local k = net.ReadUInt(4)
                local s = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_do_block_writes_remain_fixed_not_dynamic() {
        // A bare `do ... end` block is unconditional — writes inside must
        // still count as fixed, so a real count mismatch surfaces.
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteOrderMismatch);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("DoBlock")
            net.Start("DoBlock")
            do
                net.WriteString("a")
                net.WriteString("b")
            end
            net.Broadcast()
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("DoBlock", function()
                local s = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteOrderMismatch),
            eq(1usize)
        );
    }

    #[gtest]
    fn test_wrapper_writer_no_diagnostic_for_dynamic_reader_unique_match() {
        // Single sender pattern with a leading dynamic wrapper: reader's
        // structure should still match.
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("MixedHeader")
            net.Start("MixedHeader")
            if isAdmin then
                net.WriteString("admin")
            end
            net.WriteUInt(1, 8)
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("MixedHeader", function()
                local maybe = net.ReadString()
                local n = net.ReadUInt(8)
            end)
            "#,
        );

        // Reader assumes the admin string is always present. The dynamic
        // writer pattern (0..N strings) plus following UInt admits the
        // reader's fixed (String, UInt) sequence, so no false positive.
        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    // ---- Named-callback resolution tests (the receiver may pass a function
    // reference instead of an inline closure — without resolving these we
    // would see 0 reads and emit a false count-mismatch diagnostic).

    #[gtest]
    fn test_local_function_reference_callback_resolves_reads() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("DarkRP_PlayerVarRemoval")
            net.Start("DarkRP_PlayerVarRemoval")
            net.WriteUInt(1, 16)
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            local function doRetrieveRemoval()
                local userID = net.ReadUInt(16)
            end
            net.Receive("DarkRP_PlayerVarRemoval", doRetrieveRemoval)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_local_var_assigned_closure_callback_resolves_reads() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("DarkRP_PlayerVar")
            net.Start("DarkRP_PlayerVar")
            net.WriteUInt(1, 16)
            net.WriteString("var")
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            local doRetrieve = function()
                local userID = net.ReadUInt(16)
                local var = net.ReadString()
            end
            net.Receive("DarkRP_PlayerVar", doRetrieve)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_global_function_callback_resolves_reads() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("GlobalCb")
            net.Start("GlobalCb")
            net.WriteString("hi")
            net.Broadcast()
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            function HandleGlobalCb()
                local s = net.ReadString()
            end
            net.Receive("GlobalCb", HandleGlobalCb)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_assigned_global_var_closure_callback_resolves_reads() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("AssignedGlobalCb")
            net.Start("AssignedGlobalCb")
            net.WriteString("hi")
            net.Broadcast()
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            HandleCb = function()
                local s = net.ReadString()
            end
            net.Receive("AssignedGlobalCb", HandleCb)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_unresolvable_callback_reference_does_not_emit_diagnostic() {
        // Callback is defined in another file (out of single-file analysis
        // scope). We can't see its reads, so we should NOT emit a count
        // mismatch — the safe fallback is to suppress the diagnostic when
        // the receiver body is opaque.
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("OpaqueCb")
            net.Start("OpaqueCb")
            net.WriteString("hi")
            net.Broadcast()
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("OpaqueCb", SomeFunctionDefinedElsewhere)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteOrderMismatch),
            eq(0usize)
        );
    }

    #[gtest]
    fn test_named_callback_with_dynamic_loop_reads() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("NamedDyn")
            net.Start("NamedDyn")
            net.WriteUInt(2, 8)
            for _, v in ipairs({"a","b"}) do
                net.WriteString(v)
            end
            net.Broadcast()
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            local function onRecv()
                local n = net.ReadUInt(8)
                for _ = 1, n do
                    net.ReadString()
                end
            end
            net.Receive("NamedDyn", onRecv)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_unresolvable_callback_does_not_break_missing_counterpart_check() {
        // Even when the callback body is opaque, the receive flow must still
        // be recorded so missing-counterpart detection works.
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("UnpairedOpaque", SomeOpaqueFn)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(
                &diagnostics,
                DiagnosticCode::GmodNetMissingNetworkCounterpart
            ),
            eq(1usize)
        );
    }

    #[gtest]
    fn test_wrapped_start_without_send_is_excluded_from_read_write_mismatch_checks() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteOrderMismatch);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            function Glide.StartCommand(id)
                net.Start("glide.command")
                net.WriteUInt(id, 8)
            end

            Glide.StartCommand(1)
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("glide.command", function()
                local x = net.ReadUInt(8)
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteOrderMismatch),
            eq(0usize)
        );
    }

    #[gtest]
    fn test_helper_function_expansion_local_function_writer() {
        // A local helper that performs net.Write* calls should be expanded
        // when invoked between net.Start and net.Send.
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            local function writePayload(id, name)
                net.WriteUInt(id, 16)
                net.WriteString(name)
            end

            util.AddNetworkString("HelperWriter")
            net.Start("HelperWriter")
            writePayload(1, "abc")
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("HelperWriter", function()
                local id = net.ReadUInt(16)
                local name = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_helper_function_expansion_dotted_function_writer() {
        // Dotted helpers like `Module.fn(...)` should also expand.
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            DarkRP = DarkRP or {}
            function DarkRP.writeNetDarkRPVar(var, value)
                net.WriteString(var)
                net.WriteString(value)
            end

            util.AddNetworkString("DarkRP_PlayerVar")
            net.Start("DarkRP_PlayerVar")
            DarkRP.writeNetDarkRPVar("money", "100")
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("DarkRP_PlayerVar", function()
                local var = net.ReadString()
                local value = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_helper_function_expansion_reader_inside_callback() {
        // Helpers inside the receive callback should expand on the read side.
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("HelperReader")
            net.Start("HelperReader")
            net.WriteUInt(1, 16)
            net.WriteString("name")
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            local function readPayload()
                local id = net.ReadUInt(16)
                local name = net.ReadString()
            end

            net.Receive("HelperReader", function()
                readPayload()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_helper_function_expansion_dynamic_call_site_marks_helper_dynamic() {
        // When a helper containing a fixed write is called from inside an
        // `if` branch, the writes should be treated as dynamic and the
        // dynamic reader counterpart should match.
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            local function writeOne(value)
                net.WriteString(value)
            end

            util.AddNetworkString("DynamicHelperCall")
            net.Start("DynamicHelperCall")
            net.WriteUInt(0, 8)
            if true then
                writeOne("a")
                writeOne("b")
            end
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("DynamicHelperCall", function()
                local count = net.ReadUInt(8)
                for _ = 1, count do
                    local s = net.ReadString()
                end
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_helper_function_expansion_recursive_helper_chain() {
        // Helpers calling helpers should expand transitively.
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            local function writeInner(name)
                net.WriteString(name)
            end

            local function writeOuter(id, name)
                net.WriteUInt(id, 16)
                writeInner(name)
            end

            util.AddNetworkString("ChainedHelper")
            net.Start("ChainedHelper")
            writeOuter(1, "abc")
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("ChainedHelper", function()
                local id = net.ReadUInt(16)
                local name = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_helper_function_expansion_cycle_does_not_loop_forever() {
        // A self-referential helper must terminate via the visited guard.
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            local recurse
            recurse = function()
                net.WriteString("loop")
                recurse()
            end

            util.AddNetworkString("CycleHelper")
            net.Start("CycleHelper")
            recurse()
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("CycleHelper", function()
                local s = net.ReadString()
            end)
            "#,
        );

        // Just confirm we terminate and do not blow up; mismatch may or may
        // not be reported but the count must finish.
        let _ = file_diagnostics(&mut ws, client_file_id);
    }

    #[gtest]
    fn test_helper_function_expansion_cross_file_helper_is_silently_skipped() {
        // Cross-file helpers that aren't defined anywhere in the workspace
        // still don't resolve; both sides see no writes/reads, counts agree,
        // no diagnostic.
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("CrossFileHelper")
            net.Start("CrossFileHelper")
            CrossModule.writeBlob("payload")
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("CrossFileHelper", function()
                CrossModule.readBlob()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_helper_function_expansion_cross_file_dotted_helper_resolves() {
        // DarkRP-style: the writer/reader live in `sv_*.lua` and `cl_*.lua`,
        // but the actual write/read calls live in a shared module `sh_*.lua`.
        // Cross-file helper resolution should inline the shared helper's ops
        // on both sides, producing matched patterns and zero diagnostics.
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/sh_helpers.lua",
            r#"
            DarkRP = DarkRP or {}
            function DarkRP.writeBlob(s)
                net.WriteString(s)
            end
            function DarkRP.readBlob()
                return net.ReadString()
            end
            "#,
        );

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("CrossFileResolved")
            net.Start("CrossFileResolved")
            DarkRP.writeBlob("payload")
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("CrossFileResolved", function()
                local s = DarkRP.readBlob()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    /// A normal wrapper that ultimately calls the shipped `net.*` functions
    /// needs no annotations of its own. The message name is dynamic inside the
    /// wrapper and becomes concrete only at the cross-file call site.
    #[gtest]
    fn test_unannotated_cross_file_send_wrapper_resolves_call_arguments() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/sh_net_helpers.lua",
            r#"
            MyLib = MyLib or {}

            function MyLib.SendString(messageName, value)
                net.Start(messageName)
                net.WriteString(value)
                net.SendToServer()
            end
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/send.lua",
            r#"
            local sendString = MyLib.SendString
            sendString("WrappedMessage", "payload")
            "#,
        );
        let server_file_id = ws.def_file(
            "lua/autorun/server/receive.lua",
            r#"
            util.AddNetworkString("WrappedMessage")
            net.Receive("WrappedMessage", function()
                local value = net.ReadString()
            end)
            "#,
        );

        let client_diagnostics = file_diagnostics(&mut ws, client_file_id);
        let server_diagnostics = file_diagnostics(&mut ws, server_file_id);
        expect_that!(count_network_diagnostics(&client_diagnostics), eq(0usize));
        expect_that!(count_network_diagnostics(&server_diagnostics), eq(0usize));

        let flow = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(client_file_id)
            .and_then(|data| {
                data.send_flows
                    .iter()
                    .find(|flow| flow.message_name == "WrappedMessage")
            });
        let Some(flow) = flow else {
            panic!("expected the unannotated wrapper call to produce a send flow");
        };
        expect_that!(flow.writes.len(), eq(1usize));
        expect_that!(flow.writes[0].op.wire_format.as_str(), eq("string"));
        expect_that!(flow.send_kind.receiver_realm, eq(GmodRealm::Server));
    }

    #[gtest]
    fn test_unannotated_send_wrapper_participates_in_type_mismatch_checks() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteTypeMismatch);

        ws.def_file(
            "lua/autorun/sh_net_helpers.lua",
            r#"
            MyLib = MyLib or {}

            function MyLib.SendString(messageName, value)
                net.Start(messageName)
                net.WriteString(value)
                net.Broadcast()
            end
            "#,
        );
        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("WrappedMismatch")
            MyLib.SendString("WrappedMismatch", "payload")
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("WrappedMismatch", function()
                local value = net.ReadUInt(8)
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        expect_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteTypeMismatch),
            eq(1usize)
        );
    }

    #[gtest]
    fn test_static_message_wrapper_participates_in_bits_mismatch_checks() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteBitsMismatch);

        let helper_file_id = ws.def_file(
            "lua/autorun/sh_net_helpers.lua",
            r#"
            MyLib = MyLib or {}

            function MyLib.SendId()
                net.Start("WrappedBits")
                net.WriteUInt(1, 16)
                net.Broadcast()
            end
            "#,
        );
        let server_file_id = ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("WrappedBits")
            MyLib.SendId()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("WrappedBits", function()
                local value = net.ReadUInt(8)
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        expect_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteBitsMismatch),
            eq(1usize)
        );

        let send_flows = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_send_flows_for_message("WrappedBits");
        expect_that!(send_flows.len(), eq(1usize));
        expect_that!(send_flows[0].0, eq(server_file_id));
        expect_that!(
            send_flows[0]
                .1
                .materialized_from
                .map(|origin| origin.file_id),
            eq(Some(helper_file_id))
        );
    }

    #[gtest]
    fn test_cross_file_wrapper_cache_keeps_equal_sized_files_distinct() {
        let mut ws = new_gmod_workspace();
        let start_helper = r#"
            LibA = LibA or {}
            local function inner(value)
                net.Start(value) --xxxxx
            end
            function LibA.Call(value)
                inner(value)
            end
            "#;
        let write_helper = r#"
            LibB = LibB or {}
            local function inner(value)
                net.WriteString(value)--
            end
            function LibB.Call(value)
                inner(value)
            end
            "#;
        assert_eq!(
            start_helper.len(),
            write_helper.len(),
            "the regression requires identical chunk ranges"
        );

        ws.def_file("lua/autorun/sh_start_helper.lua", start_helper);
        ws.def_file("lua/autorun/sh_write_helper.lua", write_helper);

        let client_file_id = ws.def_file(
            "lua/autorun/client/send.lua",
            r#"
            LibA.Call("EqualSizedHelpers")
            LibB.Call("payload")
            net.SendToServer()
            "#,
        );
        let server_file_id = ws.def_file(
            "lua/autorun/server/receive.lua",
            r#"
            util.AddNetworkString("EqualSizedHelpers")
            net.Receive("EqualSizedHelpers", function()
                local value = net.ReadString()
            end)
            "#,
        );

        let client_diagnostics = file_diagnostics(&mut ws, client_file_id);
        let server_diagnostics = file_diagnostics(&mut ws, server_file_id);
        expect_that!(count_network_diagnostics(&client_diagnostics), eq(0usize));
        expect_that!(count_network_diagnostics(&server_diagnostics), eq(0usize));

        let flow = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(client_file_id)
            .and_then(|data| {
                data.send_flows
                    .iter()
                    .find(|flow| flow.message_name == "EqualSizedHelpers")
            });
        let Some(flow) = flow else {
            panic!("expected both equal-sized helper files to contribute to the send flow");
        };
        expect_that!(flow.writes.len(), eq(1usize));
        expect_that!(flow.writes[0].op.wire_format.as_str(), eq("string"));
    }

    #[gtest]
    fn test_unannotated_start_and_send_wrappers_form_one_flow() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/sh_net_helpers.lua",
            r#"
            MyLib = MyLib or {}

            function MyLib.Begin(messageName)
                net.Start(messageName)
            end

            function MyLib.Flush()
                net.Broadcast()
            end
            "#,
        );

        let server_file_id = ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("SplitWrapper")
            local begin = MyLib.Begin
            local flush = MyLib.Flush
            begin("SplitWrapper")
            net.WriteString("payload")
            flush()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("SplitWrapper", function()
                local value = net.ReadString()
            end)
            "#,
        );

        let server_diagnostics = file_diagnostics(&mut ws, server_file_id);
        let client_diagnostics = file_diagnostics(&mut ws, client_file_id);
        expect_that!(count_network_diagnostics(&server_diagnostics), eq(0usize));
        expect_that!(count_network_diagnostics(&client_diagnostics), eq(0usize));

        let flow = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(server_file_id)
            .and_then(|data| {
                data.send_flows
                    .iter()
                    .find(|flow| flow.message_name == "SplitWrapper" && !flow.is_wrapped)
            });
        let Some(flow) = flow else {
            panic!("expected split start/send wrappers to produce one send flow");
        };
        expect_that!(flow.writes.len(), eq(1usize));
        expect_that!(flow.writes[0].op.wire_format.as_str(), eq("string"));
        expect_that!(flow.send_kind.receiver_realm, eq(GmodRealm::Client));
    }

    #[gtest]
    fn test_unannotated_cross_file_receive_wrapper_resolves_call_arguments() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/sh_net_helpers.lua",
            r#"
            MyLib = MyLib or {}

            function MyLib.ReceiveString(messageName, callback)
                net.Receive(messageName, function()
                    callback(net.ReadString())
                end)
            end
            "#,
        );

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("WrappedReceive")
            net.Start("WrappedReceive")
            net.WriteString("payload")
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            local receiveString = MyLib.ReceiveString
            receiveString("WrappedReceive", function(value) end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        expect_that!(count_network_diagnostics(&diagnostics), eq(0usize));

        let flow = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(client_file_id)
            .and_then(|data| {
                data.receive_flows
                    .iter()
                    .find(|flow| flow.message_name == "WrappedReceive")
            });
        let Some(flow) = flow else {
            panic!("expected the unannotated wrapper call to produce a receive flow");
        };
        expect_that!(flow.reads.len(), eq(1usize));
        expect_that!(flow.reads[0].op.wire_format.as_str(), eq("string"));
    }

    #[gtest]
    fn test_receiver_expands_colon_method_reads_on_same_scripted_class() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/entities/base_glide/sv_weapons.lua",
            r#"
            util.AddNetworkString("glide.sync_weapon_data")
            function ENT:SendWeaponData()
                net.Start("glide.sync_weapon_data")
                net.WriteUInt(1, 5)
                net.WriteString("weapon")
                net.Send(Entity(1))
            end
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/entities/base_glide/cl_hud.lua",
            r#"
            function ENT:OnSyncWeaponData()
                local slotIndex = net.ReadUInt(5)
                local className = net.ReadString()
            end

            net.Receive("glide.sync_weapon_data", function()
                local vehicle = Glide.currentVehicle
                if IsValid(vehicle) then
                    vehicle:OnSyncWeaponData()
                end
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_helper_function_expansion_cross_file_genuine_mismatch_reported() {
        // Cross-file helpers resolve, so a genuine mismatch via shared helpers
        // (sender writes UInt, receiver reads String) should surface.
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/sh_helpers.lua",
            r#"
            DarkRP = DarkRP or {}
            function DarkRP.writeId(id)
                net.WriteUInt(id, 16)
            end
            function DarkRP.readId()
                return net.ReadString()
            end
            "#,
        );

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("CrossMismatch")
            net.Start("CrossMismatch")
            DarkRP.writeId(1)
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("CrossMismatch", function()
                DarkRP.readId()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        // At least one network diagnostic (type or order mismatch) should fire.
        assert_that!(count_network_diagnostics(&diagnostics), gt(0usize));
    }

    #[gtest]
    fn test_helper_function_expansion_genuine_mismatch_via_helper_still_reported() {
        // A helper writes a String but the receiver reads an Int — the
        // mismatch should still surface via expansion.
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteTypeMismatch);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            local function writeName(name)
                net.WriteString(name)
            end

            util.AddNetworkString("HelperGenuineMismatch")
            net.Start("HelperGenuineMismatch")
            writeName("oops")
            net.Send(Entity(1))
            "#,
        );

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("HelperGenuineMismatch", function()
                local x = net.ReadInt(16)
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteTypeMismatch),
            eq(1usize)
        );
    }

    fn collect_helper_conflict_diagnostics(
        helper_order: &[(&str, &str)],
    ) -> Vec<lsp_types::Diagnostic> {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteTypeMismatch);

        for (file_path, source) in helper_order {
            ws.def_file(file_path, source);
        }

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("HelperConflict")
            net.Start("HelperConflict")
            DarkRP.writeDeterministic("payload")
            net.Send(Entity(1))
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("HelperConflict", function()
                local value = net.ReadString()
            end)
            "#,
        );

        file_diagnostics(&mut ws, client_file_id)
    }

    #[gtest]
    fn test_duplicate_cross_file_helper_uses_deterministic_winner() {
        let helper_a = (
            "lua/autorun/shared/a_helpers.lua",
            r#"
            DarkRP = DarkRP or {}
            function DarkRP.writeDeterministic(value)
                net.WriteString(value)
            end
            "#,
        );
        let helper_z = (
            "lua/autorun/shared/z_helpers.lua",
            r#"
            DarkRP = DarkRP or {}
            function DarkRP.writeDeterministic(value)
                net.WriteUInt(123, 8)
            end
            "#,
        );

        let forward = collect_helper_conflict_diagnostics(&[helper_a, helper_z]);
        let reverse = collect_helper_conflict_diagnostics(&[helper_z, helper_a]);

        assert_that!(
            count_diagnostic(&forward, DiagnosticCode::GmodNetReadWriteTypeMismatch),
            eq(0usize)
        );
        assert_eq!(reverse, forward);
    }

    fn collect_equal_score_tie_message(sender_order: &[(&str, &str)]) -> String {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteTypeMismatch);

        for (file_path, source) in sender_order {
            ws.def_file(file_path, source);
        }

        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("TieBreakMessage", function()
                local value = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.code == diagnostic_code(DiagnosticCode::GmodNetReadWriteTypeMismatch)
            })
            .map(|diagnostic| diagnostic.message.clone())
            .expect("expected type mismatch diagnostic")
    }

    #[gtest]
    fn test_equal_score_network_mismatch_tie_is_deterministic() {
        let sender_a = (
            "lua/autorun/server/a_sender.lua",
            r#"
            util.AddNetworkString("TieBreakMessage")
            net.Start("TieBreakMessage")
            net.WriteInt(1, 8)
            net.Broadcast()
            "#,
        );
        let sender_z = (
            "lua/autorun/server/z_sender.lua",
            r#"
            util.AddNetworkString("TieBreakMessage")
            net.Start("TieBreakMessage")
            net.WriteBool(true)
            net.Broadcast()
            "#,
        );

        let forward_message = collect_equal_score_tie_message(&[sender_a, sender_z]);
        let reverse_message = collect_equal_score_tie_message(&[sender_z, sender_a]);

        assert_eq!(reverse_message, forward_message);
        expect_that!(forward_message.contains("expected `net.ReadInt`"), eq(true));
    }

    #[gtest]
    fn test_bits_mismatch_uint_triggers_warning() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteBitsMismatch);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("BitsMsg")
            net.Start("BitsMsg")
            net.WriteUInt(1, 16)
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("BitsMsg", function()
                local v = net.ReadUInt(8)
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteBitsMismatch),
            eq(1usize)
        );
        let diag = diagnostics
            .iter()
            .find(|d| d.code == diagnostic_code(DiagnosticCode::GmodNetReadWriteBitsMismatch))
            .expect("expected gmod-net-read-write-bits-mismatch diagnostic");
        expect_that!(diag.message.contains("net.WriteUInt"), eq(true));
        expect_that!(diag.message.contains("16"), eq(true));
        expect_that!(diag.message.contains("8"), eq(true));
    }

    #[gtest]
    fn test_bits_match_does_not_trigger_warning() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteBitsMismatch);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("BitsOk")
            net.Start("BitsOk")
            net.WriteUInt(1, 16)
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("BitsOk", function()
                local v = net.ReadUInt(16)
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteBitsMismatch),
            eq(0usize)
        );
    }

    #[gtest]
    fn test_nested_read_argument_evaluates_before_outer_read() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteOrderMismatch);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("NestedReadOrder")
            net.Start("NestedReadOrder")
            net.WriteUInt(#payload, 16)
            net.WriteData(payload, #payload)
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("NestedReadOrder", function()
                local payload = net.ReadData(net.ReadUInt(16))
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteOrderMismatch),
            eq(0usize)
        );
    }

    #[gtest]
    fn test_bits_mismatch_skipped_when_arg_is_non_literal() {
        // Robustness: when either side uses a variable for the bit width,
        // we cannot know its value statically. We must NOT warn here, even
        // though a runtime mismatch is theoretically possible.
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteBitsMismatch);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            local BITS = 16
            util.AddNetworkString("BitsVar")
            net.Start("BitsVar")
            net.WriteUInt(1, BITS)
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("BitsVar", function()
                local v = net.ReadUInt(8)
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteBitsMismatch),
            eq(0usize)
        );
    }

    #[gtest]
    fn test_bits_mismatch_int_triggers_warning() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteBitsMismatch);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("IntBits")
            net.Start("IntBits")
            net.WriteInt(1, 32)
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("IntBits", function()
                local v = net.ReadInt(16)
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteBitsMismatch),
            eq(1usize)
        );
    }

    /// Audit: writer has three writes inside a single `if` block. Reader reads
    /// them in the SAME order (gated by their own bool). Should pass — no
    /// type/order/count diagnostic.
    #[gtest]
    fn test_conditional_block_in_order_reads_match_writer() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteOrderMismatch);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("ConditionalGroup")
            net.Start("ConditionalGroup")
            net.WriteBool(cond)
            if cond then
                net.WriteString(name)
                net.WriteUInt(level, 8)
                net.WriteFloat(score)
            end
            net.Send(Entity(1))
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("ConditionalGroup", function()
                local has = net.ReadBool()
                if has then
                    local name = net.ReadString()
                    local level = net.ReadUInt(8)
                    local score = net.ReadFloat()
                end
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteOrderMismatch),
            eq(0usize)
        );
    }

    /// Audit: writer conditional block has [String, UInt]; reader reads
    /// [UInt, String] (wrong order). Order matters even for conditional ops —
    /// this should be flagged. Documents current behavior.
    #[gtest]
    fn test_conditional_block_out_of_order_reads_flagged() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteOrderMismatch);

        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("OrderInBlock")
            net.Start("OrderInBlock")
            if cond then
                net.WriteString(name)
                net.WriteUInt(level, 8)
            end
            net.Send(Entity(1))
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("OrderInBlock", function()
                local level = net.ReadUInt(8)
                local name = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteOrderMismatch),
            gt(0usize)
        );
    }

    // ---------------------------------------------------------------------
    // Annotation-driven recognition (GitHub issue #43).
    //
    // Recognition resolves the callee's signature metadata, so an alias, a
    // cross-file global, or a user-annotated wrapper is recognized exactly like
    // the builtin it points at.
    // ---------------------------------------------------------------------

    #[gtest]
    fn test_global_alias_send_has_no_missing_counterpart() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let server_file_id = ws.def_file(
            "lua/autorun/server/alias_send.lua",
            r#"
            netStart = net.Start
            netSend = net.Broadcast
            util.AddNetworkString("AliasMsg")
            netStart("AliasMsg")
            net.WriteString("hi")
            netSend()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/alias_recv.lua",
            r#"
            net.Receive("AliasMsg", function()
                local s = net.ReadString()
            end)
            "#,
        );

        let server = file_diagnostics(&mut ws, server_file_id);
        let client = file_diagnostics(&mut ws, client_file_id);
        expect_that!(count_network_diagnostics(&server), eq(0usize));
        expect_that!(count_network_diagnostics(&client), eq(0usize));
    }

    #[gtest]
    fn test_local_alias_send_has_no_missing_counterpart() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        ws.def_file(
            "lua/autorun/server/local_alias_send.lua",
            r#"
            local netStart = net.Start
            local netSend = net.Broadcast
            util.AddNetworkString("LocalAliasMsg")
            netStart("LocalAliasMsg")
            net.WriteString("hi")
            netSend()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/local_alias_recv.lua",
            r#"
            net.Receive("LocalAliasMsg", function()
                local s = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        expect_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    /// The alias is defined in one file and used in another that contains no
    /// `net.` text at all. The structural call gate must admit the calling file,
    /// then resolved signature metadata identifies the operations.
    #[gtest]
    fn test_cross_file_alias_in_file_without_net_text() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        ws.def_file(
            "lua/autorun/sh_aliases.lua",
            r#"
            beginMsg = net.Start
            pushText = net.WriteString
            flushMsg = net.Broadcast
            "#,
        );
        let sender_file_id = ws.def_file(
            "lua/autorun/server/cross_alias_send.lua",
            r#"
            beginMsg("CrossAliasMsg")
            pushText("hello")
            flushMsg()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/cross_alias_recv.lua",
            r#"
            net.Receive("CrossAliasMsg", function()
                local s = net.ReadString()
            end)
            "#,
        );

        let sender = file_diagnostics(&mut ws, sender_file_id);
        let client = file_diagnostics(&mut ws, client_file_id);
        expect_that!(count_network_diagnostics(&sender), eq(0usize));
        expect_that!(count_network_diagnostics(&client), eq(0usize));
    }

    #[gtest]
    fn test_aliased_send_to_server_resolves_realm() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        ws.def_file(
            "lua/autorun/client/alias_sts.lua",
            r#"
            local start = net.Start
            local toServer = net.SendToServer
            start("ClientToServer")
            net.WriteString("hi")
            toServer()
            "#,
        );
        let server_file_id = ws.def_file(
            "lua/autorun/server/alias_sts_recv.lua",
            r#"
            net.Receive("ClientToServer", function()
                local s = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        expect_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_aliased_write_and_read_ops_keep_counts_aligned() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteOrderMismatch);

        ws.def_file(
            "lua/autorun/server/alias_ops_send.lua",
            r#"
            local writeStr = net.WriteString
            util.AddNetworkString("AliasOps")
            net.Start("AliasOps")
            writeStr("a")
            writeStr("b")
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/alias_ops_recv.lua",
            r#"
            local readStr = net.ReadString
            net.Receive("AliasOps", function()
                local a = readStr()
                local b = readStr()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        expect_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_local_alias_receive_is_resolved_from_signature_metadata() {
        let mut ws = new_gmod_workspace();

        ws.def_file(
            "lua/autorun/server/alias_receive_send.lua",
            r#"
            util.AddNetworkString("AliasReceive")
            net.Start("AliasReceive")
            net.WriteString("payload")
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/alias_receive.lua",
            r#"
            local netReceive = net.Receive
            local readString = net.ReadString

            netReceive("AliasReceive", function()
                local value = readString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        expect_that!(count_network_diagnostics(&diagnostics), eq(0usize));

        let flow = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(client_file_id)
            .and_then(|data| {
                data.receive_flows
                    .iter()
                    .find(|flow| flow.message_name == "AliasReceive")
            })
            .expect("aliased receive should produce a network flow");
        expect_that!(flow.reads.len(), eq(1usize));
        expect_that!(flow.reads[0].op.wire_format.as_str(), eq("string"));
    }

    #[gtest]
    fn test_user_annotated_wrapper_without_net_text_is_recognized() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        // The wrapper library declares its own net vocabulary.
        ws.def_file(
            "lua/autorun/sh_wrapperlib.lua",
            r#"
            ---@[call_arg("gmod.net_message", "start")]
            ---@param name string
            function BeginMessage(name) end

            ---@param value string
            ---@[net_payload("write", "string")]
            function PutString(value) end

            ---@[net_send("client")]
            function FlushToClients() end
            "#,
        );
        // This file contains no `net.` text at all. The structural call gate
        // admits it, then signature metadata classifies each operation.
        let sender_file_id = ws.def_file(
            "lua/autorun/server/wrapper_send.lua",
            r#"
            BeginMessage("WrappedOnly")
            PutString("payload")
            FlushToClients()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/wrapper_recv.lua",
            r#"
            net.Receive("WrappedOnly", function()
                local s = net.ReadString()
            end)
            "#,
        );

        let sender = file_diagnostics(&mut ws, sender_file_id);
        let client = file_diagnostics(&mut ws, client_file_id);
        expect_that!(count_network_diagnostics(&sender), eq(0usize));
        expect_that!(count_network_diagnostics(&client), eq(0usize));
    }

    #[gtest]
    fn test_annotated_replacement_api_adjusts_roles_for_colon_calls() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        ws.def_file(
            "lua/autorun/sh_replacement_net.lua",
            r#"
            ---@class ReplacementNet
            ReplacementNet = {}

            ---@param self ReplacementNet
            ---@[call_arg("gmod.net_message", "start")]
            ---@param name string
            function ReplacementNet.Begin(self, name) end

            ---@param self ReplacementNet
            ---@param value string
            ---@[net_payload("write", "string")]
            function ReplacementNet.PutString(self, value) end

            ---@param self ReplacementNet
            ---@[net_send("server")]
            function ReplacementNet.Flush(self) end
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/send.lua",
            r#"
            ReplacementNet:Begin("ColonReplacement")
            ReplacementNet:PutString("payload")
            ReplacementNet:Flush()
            "#,
        );
        let server_file_id = ws.def_file(
            "lua/autorun/server/receive.lua",
            r#"
            util.AddNetworkString("ColonReplacement")
            net.Receive("ColonReplacement", function()
                local value = net.ReadString()
            end)
            "#,
        );

        let client_diagnostics = file_diagnostics(&mut ws, client_file_id);
        let server_diagnostics = file_diagnostics(&mut ws, server_file_id);
        expect_that!(count_network_diagnostics(&client_diagnostics), eq(0usize));
        expect_that!(count_network_diagnostics(&server_diagnostics), eq(0usize));

        let flow = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(client_file_id)
            .and_then(|data| {
                data.send_flows
                    .iter()
                    .find(|flow| flow.message_name == "ColonReplacement")
            });
        let Some(flow) = flow else {
            panic!("expected colon-call argument roles to produce a send flow");
        };
        expect_that!(flow.writes.len(), eq(1usize));
        expect_that!(flow.writes[0].op.wire_format.as_str(), eq("string"));
        expect_that!(flow.send_kind.receiver_realm, eq(GmodRealm::Server));
    }

    /// The message name and the receive callback are located by their annotated
    /// roles, not by position, so a wrapper is free to order its parameters
    /// differently from `net.Start` / `net.Receive`.
    #[gtest]
    fn test_wrapper_with_non_leading_message_and_callback_params_pairs() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        ws.def_file(
            "lua/autorun/sh_offsetlib.lua",
            r#"
            ---@param reliable boolean
            ---@[call_arg("gmod.net_message", "start")]
            ---@param name string
            function Begin(reliable, name) end

            ---@param priority number
            ---@[call_arg("gmod.net_message", "receive")]
            ---@param name string
            ---@[call_arg("gmod.net_message", "callback")]
            ---@param handler function
            function Listen(priority, name, handler) end

            ---@[net_send("client")]
            function Flush() end
            "#,
        );
        let offset_sender_file_id = ws.def_file(
            "lua/autorun/server/offset_send.lua",
            r#"
            Begin(true, "OffsetMsg")
            net.WriteString("payload")
            Flush()
            "#,
        );
        let offset_client_file_id = ws.def_file(
            "lua/autorun/client/offset_recv.lua",
            r#"
            Listen(1, "OffsetMsg", function()
                local s = net.ReadString()
            end)
            "#,
        );

        let offset_sender = file_diagnostics(&mut ws, offset_sender_file_id);
        let offset_client = file_diagnostics(&mut ws, offset_client_file_id);
        expect_that!(count_network_diagnostics(&offset_sender), eq(0usize));
        expect_that!(count_network_diagnostics(&offset_client), eq(0usize));
    }

    #[gtest]
    fn test_previously_unmodelled_ops_now_pair() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteOrderMismatch);

        ws.def_file(
            "lua/autorun/server/new_ops_send.lua",
            r#"
            util.AddNetworkString("NewOps")
            net.Start("NewOps")
            net.WriteMatrix(m)
            net.WritePlayer(ply)
            net.WriteUInt64(v)
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/new_ops_recv.lua",
            r#"
            net.Receive("NewOps", function()
                local m = net.ReadMatrix()
                local p = net.ReadPlayer()
                local v = net.ReadUInt64()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        expect_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    /// The numeric family all declares `number`, so only the wire format keeps
    /// these distinguishable. Losing that axis would silently regress eight ops.
    #[gtest]
    fn test_numeric_family_mismatches_still_detected() {
        for (write_op, read_op) in [
            ("net.WriteFloat(1)", "net.ReadInt(16)"),
            ("net.WriteUInt(1, 8)", "net.ReadInt(8)"),
            ("net.WriteDouble(1)", "net.ReadFloat()"),
        ] {
            let mut ws = new_gmod_workspace();
            ws.analysis
                .diagnostic
                .enable_only(DiagnosticCode::GmodNetReadWriteTypeMismatch);

            ws.def_file(
                "lua/autorun/server/num_send.lua",
                &format!(
                    r#"
                    util.AddNetworkString("NumMsg")
                    net.Start("NumMsg")
                    {write_op}
                    net.Broadcast()
                    "#
                ),
            );
            let client_file_id = ws.def_file(
                "lua/autorun/client/num_recv.lua",
                &format!(
                    r#"
                    net.Receive("NumMsg", function()
                        local v = {read_op}
                    end)
                    "#
                ),
            );

            let diagnostics = file_diagnostics(&mut ws, client_file_id);
            expect_that!(
                count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteTypeMismatch),
                eq(1usize),
                "expected a mismatch for {write_op} paired with {read_op}"
            );
        }
    }

    /// `net.WritePlayer` uses a narrower encoding than `net.WriteEntity`, so the
    /// two carry distinct wire formats and must not pair.
    #[gtest]
    fn test_write_player_read_entity_is_flagged() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteTypeMismatch);

        ws.def_file(
            "lua/autorun/server/player_send.lua",
            r#"
            util.AddNetworkString("PlayerMsg")
            net.Start("PlayerMsg")
            net.WritePlayer(ply)
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/player_recv.lua",
            r#"
            net.Receive("PlayerMsg", function()
                local e = net.ReadEntity()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        expect_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteTypeMismatch),
            eq(1usize)
        );
    }

    /// A user subclass of Entity shares `net.WriteEntity`'s wire format, so it
    /// pairs cleanly without any per-class metadata.
    #[gtest]
    fn test_user_entity_subclass_pairs_through_write_entity() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteTypeMismatch);

        ws.def_file(
            "lua/autorun/sh_mynpc.lua",
            r#"
            ---@class MyNPC : Entity
            local MyNPC = {}
            "#,
        );
        ws.def_file(
            "lua/autorun/server/npc_send.lua",
            r#"
            util.AddNetworkString("NpcMsg")
            net.Start("NpcMsg")
            net.WriteEntity(npc)
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/npc_recv.lua",
            r#"
            net.Receive("NpcMsg", function()
                local e = net.ReadEntity()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        expect_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    /// An unannotated workspace function that merely resembles the builtin must
    /// not be mistaken for it.
    #[gtest]
    fn test_unannotated_lookalike_is_not_recognized() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

        let file_id = ws.def_file(
            "lua/autorun/server/lookalike.lua",
            r#"
            mynet = {}
            function mynet.Start(name) end
            function mynet.Broadcast() end

            mynet.Start("NotARealMessage")
            mynet.Broadcast()
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, file_id);
        expect_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    /// Verifies that the Rust ingestion fixture exposes both payload directions.
    /// The annotations repository separately validates its generated
    /// `output/net.lua`, so shipped metadata is not tested through this copy.
    #[gtest]
    fn test_fixture_every_wire_format_has_a_write_and_a_read() {
        let mut ws = new_gmod_workspace();
        ws.def_file("lua/autorun/server/coverage.lua", "net.Start(\"X\")");

        let coverage = ws
            .get_db_mut()
            .get_gmod_network_index()
            .wire_format_coverage();
        assert_that!(coverage.is_empty(), eq(false));

        let unpaired: Vec<String> = coverage
            .iter()
            .filter(|(_, (has_write, has_read))| !has_write || !has_read)
            .map(|(format, (has_write, has_read))| {
                format!("{format} (write={has_write}, read={has_read})")
            })
            .collect();
        expect_that!(unpaired, eq(&Vec::<String>::new()));
    }

    #[gtest]
    fn test_builtin_canonical_op_name_outranks_workspace_wrapper() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .add_library_workspace(ws.virtual_url_generator.new_path("annotations"));
        ws.def_file("annotations/net.lua", crate::GMOD_CALL_ARG_BUILTINS_FIXTURE);
        ws.def_file(
            "lua/autorun/sh_custom_net.lua",
            r#"
            MyNet = MyNet or {}

            ---@[net_payload("read", "string")]
            function MyNet.ReadString() end
            "#,
        );
        ws.def_file("lua/autorun/server/coverage.lua", "net.Start(\"X\")");

        let canonical = ws
            .get_db_mut()
            .get_gmod_network_index()
            .canonical_op_name("string", NetOpDirection::Read);
        expect_that!(canonical, eq(Some("net.ReadString")));
    }

    #[gtest]
    fn test_sender_tie_break_uses_byte_order_of_path() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteTypeMismatch);

        ws.def_file(
            "lua/autorun/server/alpha.lua",
            r#"
            util.AddNetworkString("Msg")
            net.Start("Msg")
            net.WriteBool(true)
            net.Broadcast()
            "#,
        );
        ws.def_file(
            "lua/autorun/server/Zeta.lua",
            r#"
            net.Start("Msg")
            net.WriteEntity(e)
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("Msg", function()
                local x = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteTypeMismatch),
            eq(1usize)
        );
        expect_that!(
            diagnostics[0]
                .message
                .contains("expected `net.ReadEntity`, got `net.ReadString`"),
            eq(true)
        );
    }

    const PANEL_HELPER_PATH: &str = "gamemodes/cityrp/gamemode/core/libraries/cl_panel_net.lua";
    const PANEL_CALLER_PATH: &str = "gamemodes/cityrp/plugins/deco/derma/cl_store.lua";

    fn method_helper_receiver_realms(helper: &str, call: &str) -> Vec<GmodRealm> {
        let mut ws = new_gmod_workspace();
        ws.def_file(PANEL_HELPER_PATH, helper);
        let client_file_id = ws.def_file(PANEL_CALLER_PATH, call);
        ws.get_db_mut()
            .get_gmod_network_index()
            .get_file_data(client_file_id)
            .map(|data| {
                data.send_flows
                    .iter()
                    .filter(|flow| flow.message_name == "deco_orderadd")
                    .map(|flow| flow.send_kind.receiver_realm)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[gtest]
    fn test_dot_defined_helper_called_as_method_binds_receiver_first() {
        let realms = method_helper_receiver_realms(
            r#"
            PanelNet = {}
            PanelNet.NetMessage = function(pnl, name)
                net.Start(name)
                net.SendToServer()
            end
            "#,
            r#"
            local button = vgui.Create("DButton")
            button:NetMessage("deco_orderadd")
            "#,
        );
        expect_that!(realms, elements_are![eq(&GmodRealm::Server)]);
    }

    #[gtest]
    fn test_colon_defined_helper_called_with_explicit_receiver_binds_self() {
        let realms = method_helper_receiver_realms(
            r#"
            PanelNet = {}
            function PanelNet:NetMessage(name)
                net.Start(name)
                net.SendToServer()
            end
            "#,
            r#"
            local button = vgui.Create("DButton")
            PanelNet.NetMessage(button, "deco_orderadd")
            "#,
        );
        expect_that!(realms, elements_are![eq(&GmodRealm::Server)]);
    }

    #[gtest]
    fn test_helper_stored_on_local_table_resolves_as_method() {
        let realms = method_helper_receiver_realms(
            r#"
            local classes = {}
            classes.NetMessage = function(pnl, name)
                net.Start(name)
                net.SendToServer()
            end
            "#,
            r#"
            local button = vgui.Create("DButton")
            button:NetMessage("deco_orderadd")
            "#,
        );
        expect_that!(realms, elements_are![eq(&GmodRealm::Server)]);
    }

    #[gtest]
    fn test_helper_in_a_local_table_constructor_resolves_as_method() {
        let realms = method_helper_receiver_realms(
            r#"
            local classes = {
                NetMessage = function(pnl, name)
                    net.Start(name)
                    net.SendToServer()
                end,
            }
            "#,
            r#"
            local button = vgui.Create("DButton")
            button:NetMessage("deco_orderadd")
            "#,
        );
        expect_that!(realms, elements_are![eq(&GmodRealm::Server)]);
    }

    #[gtest]
    fn test_a_local_tables_method_leaves_a_unique_global_helper_resolvable() {
        let realms = method_helper_receiver_realms(
            r#"
            PanelNet = {}
            PanelNet.NetMessage = function(pnl, name)
                net.Start(name)
                net.SendToServer()
            end
            local logger = {}
            function logger:NetMessage(text)
                print(text)
            end
            "#,
            r#"
            local button = vgui.Create("DButton")
            button:NetMessage("deco_orderadd")
            "#,
        );
        expect_that!(realms, elements_are![eq(&GmodRealm::Server)]);
    }

    #[gtest]
    fn test_helper_sending_from_click_handler_counts_as_client_send() {
        let mut ws = new_gmod_workspace();
        ws.def_file(
            PANEL_HELPER_PATH,
            r#"
            local classes = {}
            classes.On = function(pnl, name, fn)
                pnl[name] = fn
            end
            classes.NetMessage = function(pnl, name, data)
                pnl:On("DoClick", function()
                    net.Start(name)
                    data(pnl)
                    net.SendToServer()
                end)
            end
            "#,
        );
        ws.def_file(
            PANEL_CALLER_PATH,
            r#"
            local button = vgui.Create("DButton")
            button:NetMessage("deco_orderadd", function() end)
            "#,
        );
        let server_file_id = ws.def_file(
            "gamemodes/cityrp/plugins/deco/sv_plugin.lua",
            r#"
            util.AddNetworkString("deco_orderadd")
            net.Receive("deco_orderadd", function(len, ply) end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        expect_that!(
            count_diagnostic(
                &diagnostics,
                DiagnosticCode::GmodNetMissingNetworkCounterpart
            ),
            eq(0usize)
        );
    }

    #[gtest]
    fn test_click_handler_parameter_shadowing_helper_parameter_does_not_bind() {
        let realms = method_helper_receiver_realms(
            r#"
            local classes = {}
            classes.On = function(pnl, name, fn)
                pnl[name] = fn
            end
            classes.NetMessage = function(pnl, name)
                pnl:On("DoClick", function(name)
                    net.Start(name)
                    net.SendToServer()
                end)
            end
            "#,
            r#"
            local button = vgui.Create("DButton")
            button:NetMessage("deco_orderadd")
            "#,
        );
        expect_that!(realms, is_empty());
    }

    const TDLIB_NET_MESSAGE: &str = r#"
            local classes = {}
            classes.On = function(pnl, name, fn)
                pnl[name] = fn
            end
            classes.NetMessage = function(pnl, name, data)
                pnl:On("DoClick", function()
                    net.Start(name)
                    data(pnl)
                    net.SendToServer()
                end)
            end
            "#;

    const DECO_ORDER_RECEIVER: &str = r#"
            util.AddNetworkString("deco_orderadd")
            net.Receive("deco_orderadd", function(len, ply)
                local item = net.ReadString()
                local quantity = net.ReadUInt(16)
            end)
            "#;

    /// The network diagnostics on the `deco_orderadd` receiver once `helper`
    /// and `caller` are loaded, and the writes of the caller's send flows.
    fn deco_order_check(helper: &str, caller: &str) -> (usize, Vec<Vec<String>>) {
        let mut ws = new_gmod_workspace();
        ws.def_file(PANEL_HELPER_PATH, helper);
        let client_file_id = ws.def_file(PANEL_CALLER_PATH, caller);
        let server_file_id = ws.def_file(
            "gamemodes/cityrp/plugins/deco/sv_plugin.lua",
            DECO_ORDER_RECEIVER,
        );
        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        let writes = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(client_file_id)
            .map(|data| {
                data.send_flows
                    .iter()
                    .filter(|flow| flow.message_name == "deco_orderadd")
                    .map(|flow| {
                        flow.writes
                            .iter()
                            .map(|write| write.op.wire_format.as_str().to_string())
                            .collect()
                    })
                    .collect()
            })
            .unwrap_or_default();
        (count_network_diagnostics(&diagnostics), writes)
    }

    #[gtest]
    fn test_helper_payload_written_by_caller_closure_is_checked() {
        let (diagnostics, writes) = deco_order_check(
            TDLIB_NET_MESSAGE,
            r#"
            local button = vgui.Create("DButton")
            button:NetMessage("deco_orderadd", function()
                net.WriteString("chair")
                net.WriteUInt(2, 16)
            end)
            "#,
        );
        expect_that!(diagnostics, eq(0usize));
        expect_that!(
            writes,
            elements_are![elements_are![eq("string"), eq("uint")]]
        );
    }

    #[gtest]
    fn test_helper_payload_written_by_caller_closure_reports_a_mismatch() {
        let (diagnostics, writes) = deco_order_check(
            TDLIB_NET_MESSAGE,
            r#"
            local button = vgui.Create("DButton")
            button:NetMessage("deco_orderadd", function()
                net.WriteString("chair")
            end)
            "#,
        );
        expect_that!(diagnostics, eq(1usize));
        expect_that!(writes, elements_are![elements_are![eq("string")]]);
    }

    #[gtest]
    fn test_helper_payload_written_by_named_local_function_is_checked() {
        let (diagnostics, writes) = deco_order_check(
            TDLIB_NET_MESSAGE,
            r#"
            local function writeOrder()
                net.WriteString("chair")
            end
            local button = vgui.Create("DButton")
            button:NetMessage("deco_orderadd", writeOrder)
            "#,
        );
        expect_that!(diagnostics, eq(1usize));
        expect_that!(writes, elements_are![elements_are![eq("string")]]);
    }

    #[gtest]
    fn test_helper_payload_written_by_unresolvable_function_is_not_checked() {
        let (diagnostics, _) = deco_order_check(
            TDLIB_NET_MESSAGE,
            r#"
            local PANEL = {}
            function PANEL:Init()
                local button = vgui.Create("DButton", self)
                button:NetMessage("deco_orderadd", self.WriteOrder)
            end
            "#,
        );
        expect_that!(diagnostics, eq(0usize));
    }

    #[gtest]
    fn test_writer_forwarded_through_two_helpers_is_checked() {
        let (diagnostics, writes) = deco_order_check(
            r#"
            local classes = {}
            classes.Send = function(pnl, name, data)
                net.Start(name)
                data(pnl)
                net.SendToServer()
            end
            classes.NetMessage = function(pnl, name, data)
                pnl:Send(name, data)
            end
            "#,
            r#"
            local button = vgui.Create("DButton")
            button:NetMessage("deco_orderadd", function()
                net.WriteString("chair")
            end)
            "#,
        );
        expect_that!(diagnostics, eq(1usize));
        expect_that!(writes, elements_are![elements_are![eq("string")]]);
    }

    #[gtest]
    fn test_writer_passed_to_a_payload_helper_is_checked() {
        let (diagnostics, writes) = deco_order_check(
            r#"
            local function writeWith(write)
                write()
            end
            local classes = {}
            classes.NetMessage = function(pnl, name, data)
                net.Start(name)
                writeWith(data)
                net.SendToServer()
            end
            "#,
            r#"
            local button = vgui.Create("DButton")
            button:NetMessage("deco_orderadd", function()
                net.WriteString("chair")
            end)
            "#,
        );
        expect_that!(diagnostics, eq(1usize));
        expect_that!(writes, elements_are![elements_are![eq("string")]]);
    }

    #[gtest]
    fn test_writer_passed_through_two_payload_helpers_is_checked() {
        let (diagnostics, writes) = deco_order_check(
            r#"
            local function writeWith(write)
                write()
            end
            local function writeVia(write)
                writeWith(write)
            end
            local classes = {}
            classes.NetMessage = function(pnl, name, data)
                net.Start(name)
                writeVia(data)
                net.SendToServer()
            end
            "#,
            r#"
            local button = vgui.Create("DButton")
            button:NetMessage("deco_orderadd", function()
                net.WriteString("chair")
            end)
            "#,
        );
        expect_that!(diagnostics, eq(1usize));
        expect_that!(writes, elements_are![elements_are![eq("string")]]);
    }

    #[gtest]
    fn test_callback_parameter_shadowing_the_writer_is_not_bound() {
        let (diagnostics, _) = deco_order_check(
            r#"
            local classes = {}
            classes.On = function(pnl, name, fn)
                pnl[name] = fn
            end
            classes.NetMessage = function(pnl, name, data)
                pnl:On("DoClick", function(_, _, data)
                    net.Start(name)
                    data(pnl)
                    net.SendToServer()
                end)
            end
            "#,
            r#"
            local button = vgui.Create("DButton")
            button:NetMessage("deco_orderadd", function()
                net.WriteString("chair")
            end)
            "#,
        );
        expect_that!(diagnostics, eq(0usize));
    }

    const LISTEN_HELPER: &str = r#"
            function Listen(name, reader)
                net.Receive(name, function(len, ply)
                    reader(ply)
                end)
            end
            "#;

    const ORDER_SENDER: &str = r#"
            net.Start("OrderPlaced")
            net.WriteString("chair")
            net.SendToServer()
            "#;

    /// The network diagnostics across the files once `helper`, `caller` and a
    /// client sending `OrderPlaced` with one string are loaded, and the reads
    /// and opacity of the caller's `OrderPlaced` receive flows.
    fn order_listen_check(helper: &str, caller: &str) -> (usize, Vec<(Vec<String>, bool)>) {
        let mut ws = new_gmod_workspace();
        let helper_file_id = ws.def_file("lua/autorun/server/sv_listen.lua", helper);
        let caller_file_id = ws.def_file("lua/autorun/server/sv_orders.lua", caller);
        let client_file_id = ws.def_file("lua/autorun/client/cl_order.lua", ORDER_SENDER);
        let diagnostics = [helper_file_id, caller_file_id, client_file_id]
            .into_iter()
            .map(|file_id| count_network_diagnostics(&file_diagnostics(&mut ws, file_id)))
            .sum::<usize>();
        let reads = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(caller_file_id)
            .map(|data| {
                data.receive_flows
                    .iter()
                    .filter(|flow| flow.message_name == "OrderPlaced")
                    .map(|flow| {
                        let reads = flow
                            .reads
                            .iter()
                            .map(|read| read.op.wire_format.as_str().to_string())
                            .collect();
                        (reads, flow.reads_opaque)
                    })
                    .collect()
            })
            .unwrap_or_default();
        (diagnostics, reads)
    }

    #[gtest]
    fn test_receive_reading_through_a_passed_reader_is_checked() {
        let (diagnostics, reads) = order_listen_check(
            LISTEN_HELPER,
            r#"
            Listen("OrderPlaced", function(ply)
                local item = net.ReadString()
            end)
            "#,
        );
        expect_that!(diagnostics, eq(0usize));
        expect_that!(
            reads,
            elements_are![eq(&(vec!["string".to_string()], false))]
        );
    }

    #[gtest]
    fn test_receive_reading_through_a_passed_reader_reports_a_mismatch() {
        let (diagnostics, reads) = order_listen_check(
            LISTEN_HELPER,
            r#"
            Listen("OrderPlaced", function(ply)
                local item = net.ReadString()
                local quantity = net.ReadUInt(16)
            end)
            "#,
        );
        expect_that!(diagnostics, eq(1usize));
        expect_that!(
            reads,
            elements_are![eq(&(vec!["string".to_string(), "uint".to_string()], false))]
        );
    }

    #[gtest]
    fn test_receive_reading_through_a_named_local_reader_is_checked() {
        let (diagnostics, reads) = order_listen_check(
            LISTEN_HELPER,
            r#"
            local function readOrder(ply)
                local item = net.ReadString()
                local quantity = net.ReadUInt(16)
            end
            Listen("OrderPlaced", readOrder)
            "#,
        );
        expect_that!(diagnostics, eq(1usize));
        expect_that!(
            reads,
            elements_are![eq(&(vec!["string".to_string(), "uint".to_string()], false))]
        );
    }

    #[gtest]
    fn test_receive_reading_through_an_unresolvable_reader_is_not_checked() {
        let (diagnostics, reads) = order_listen_check(
            LISTEN_HELPER,
            r#"
            local PANEL = {}
            function PANEL:Init()
                Listen("OrderPlaced", self.ReadOrder)
            end
            "#,
        );
        expect_that!(diagnostics, eq(0usize));
        expect_that!(reads, elements_are![eq(&(Vec::<String>::new(), true))]);
    }

    #[gtest]
    fn test_reader_forwarded_through_two_helpers_is_checked() {
        let (diagnostics, reads) = order_listen_check(
            r#"
            local function register(name, reader)
                net.Receive(name, function(len, ply)
                    reader(ply)
                end)
            end
            function Listen(name, reader)
                register(name, reader)
            end
            "#,
            r#"
            Listen("OrderPlaced", function(ply)
                local item = net.ReadString()
                local quantity = net.ReadUInt(16)
            end)
            "#,
        );
        expect_that!(diagnostics, eq(1usize));
        expect_that!(
            reads,
            elements_are![eq(&(vec!["string".to_string(), "uint".to_string()], false))]
        );
    }

    #[gtest]
    fn test_reader_passed_as_the_receive_callback_is_checked() {
        let (diagnostics, reads) = order_listen_check(
            r#"
            function Listen(name, reader)
                net.Receive(name, reader)
            end
            "#,
            r#"
            Listen("OrderPlaced", function(len, ply)
                local item = net.ReadString()
                local quantity = net.ReadUInt(16)
            end)
            "#,
        );
        expect_that!(diagnostics, eq(1usize));
        expect_that!(
            reads,
            elements_are![eq(&(vec!["string".to_string(), "uint".to_string()], false))]
        );
    }

    #[gtest]
    fn test_send_calling_a_writer_parameter_is_not_counted_as_empty() {
        let mut ws = new_gmod_workspace();
        ws.def_file(
            "lua/autorun/client/cl_order.lua",
            r#"
            function SendOrder(writePayload)
                net.Start("OrderPlaced")
                writePayload()
                net.SendToServer()
            end
            "#,
        );
        let server_file_id = ws.def_file(
            "lua/autorun/server/sv_order.lua",
            r#"
            util.AddNetworkString("OrderPlaced")
            net.Receive("OrderPlaced", function(len, ply)
                local item = net.ReadString()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        expect_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_receive_calling_a_utility_that_runs_an_unbound_callback_is_still_checked() {
        let mut ws = new_gmod_workspace();
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetReadWriteOrderMismatch);
        let server_file_id = ws.def_file(
            "lua/autorun/server/sv_orders.lua",
            r#"
            local function safely(fn)
                fn()
            end
            util.AddNetworkString("OrderPlaced")
            net.Receive("OrderPlaced", function(len, ply)
                safely(ply.OnOrder)
                local item = net.ReadString()
            end)
            "#,
        );
        ws.def_file(
            "lua/autorun/client/cl_order.lua",
            r#"
            net.Start("OrderPlaced")
            net.WriteString("chair")
            net.WriteUInt(2, 16)
            net.SendToServer()
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        expect_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteOrderMismatch),
            eq(1usize)
        );
    }

    #[gtest]
    fn test_a_helper_calling_an_enclosing_functions_writer_leaves_the_payload_unchecked() {
        let mut ws = new_gmod_workspace();
        let server_file_id = ws.def_file(
            "lua/autorun/server/sv_orders.lua",
            r#"
            util.AddNetworkString("OrderPlaced")
            net.Receive("OrderPlaced", function(len, ply)
                local item = net.ReadString()
                local count = net.ReadUInt(16)
            end)
            "#,
        );
        ws.def_file(
            "lua/autorun/client/cl_order.lua",
            r#"
            function SendOrder(writer)
                local function body()
                    net.WriteString("chair")
                    writer()
                end
                net.Start("OrderPlaced")
                body()
                net.SendToServer()
            end
            SendOrder(function() net.WriteUInt(2, 16) end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        expect_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_a_receive_handing_an_enclosing_functions_reader_to_a_utility_is_unchecked() {
        let mut ws = new_gmod_workspace();
        let server_file_id = ws.def_file(
            "lua/autorun/server/sv_orders.lua",
            r#"
            local function safely(fn, ...)
                fn(...)
            end
            util.AddNetworkString("OrderPlaced")
            function Listen(reader)
                net.Receive("OrderPlaced", function(len, ply)
                    safely(reader, ply)
                end)
            end
            "#,
        );
        ws.def_file(
            "lua/autorun/client/cl_order.lua",
            r#"
            net.Start("OrderPlaced")
            net.WriteString("chair")
            net.SendToServer()
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        expect_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_a_start_helper_that_writes_after_its_start_leaves_the_payload_unchecked() {
        let mut ws = new_gmod_workspace();
        let server_file_id = ws.def_file(
            "lua/autorun/server/sv_orders.lua",
            r#"
            util.AddNetworkString("OrderPlaced")
            net.Receive("OrderPlaced", function(len, ply)
                local version = net.ReadUInt(8)
                local item = net.ReadString()
            end)
            "#,
        );
        ws.def_file(
            "lua/autorun/client/cl_order.lua",
            r#"
            local function Begin(name)
                net.Start(name)
                net.WriteUInt(1, 8)
            end
            Begin("OrderPlaced")
            net.WriteString("chair")
            net.SendToServer()
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        expect_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_a_send_helper_that_writes_before_its_send_leaves_the_payload_unchecked() {
        let mut ws = new_gmod_workspace();
        let server_file_id = ws.def_file(
            "lua/autorun/server/sv_orders.lua",
            r#"
            util.AddNetworkString("OrderPlaced")
            net.Receive("OrderPlaced", function(len, ply)
                local item = net.ReadString()
                local count = net.ReadUInt(8)
            end)
            "#,
        );
        ws.def_file(
            "lua/autorun/client/cl_order.lua",
            r#"
            local function Finish()
                net.WriteUInt(1, 8)
                net.SendToServer()
            end
            net.Start("OrderPlaced")
            net.WriteString("chair")
            Finish()
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        expect_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_read_guarded_by_and_or_is_optional() {
        let mut ws = new_gmod_workspace();
        ws.def_file(
            "lua/autorun/client/send.lua",
            r#"
            net.Start("Stage")
            net.WriteUInt(1, 2)
            net.SendToServer()
            "#,
        );
        let server_file_id = ws.def_file(
            "lua/autorun/server/receive.lua",
            r#"
            util.AddNetworkString("Stage")
            net.Receive("Stage", function(_, ply)
                local stage = net.ReadUInt(2)
                local secret = stage == 2 and net.ReadString() or "NO DATA"
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, server_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_read_inside_called_call_result_is_counted() {
        let mut ws = new_gmod_workspace();
        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("Code")
            net.Start("Code")
            net.WriteString("print(1)")
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("Code", function()
                CompileString(net.ReadString(), "code")()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_read_whose_result_is_a_method_receiver_is_counted() {
        let mut ws = new_gmod_workspace();
        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("Remove")
            net.Start("Remove")
            net.WriteEntity(NULL)
            net.Broadcast()
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("Remove", function()
                net.ReadEntity():Remove()
            end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_and_or_message_name_sends_each_literal() {
        let mut ws = new_gmod_workspace();
        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("PickA")
            util.AddNetworkString("PickB")
            local function sendOne(isB)
                net.Start(isB and "PickB" or "PickA")
                net.Broadcast()
            end
            sendOne(true)
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("PickA", function() end)
            net.Receive("PickB", function() end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(
                &diagnostics,
                DiagnosticCode::GmodNetMissingNetworkCounterpart
            ),
            eq(0usize)
        );
    }

    #[gtest]
    fn test_and_or_message_name_payload_is_checked_per_name() {
        let mut ws = new_gmod_workspace();
        ws.def_file(
            "lua/autorun/server/send.lua",
            r#"
            util.AddNetworkString("PickA")
            util.AddNetworkString("PickB")
            local function sendOne(isB)
                net.Start(isB and "PickB" or "PickA")
                net.WriteString("x")
                net.Broadcast()
            end
            sendOne(true)
            "#,
        );
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("PickA", function() local x = net.ReadString() end)
            net.Receive("PickB", function() local x = net.ReadEntity() end)
            "#,
        );

        let diagnostics = file_diagnostics(&mut ws, client_file_id);
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteTypeMismatch),
            eq(1usize)
        );
    }

    const WRAPPED_NET_START: &str = r#"
        net._Start = net._Start or net.Start
        function net.Start(name, unreliable)
            net._Start(name, unreliable)
        end
    "#;

    fn branch_send_receiver_diagnostics(wrapper: Option<&str>, sender: &str) -> Vec<Diagnostic> {
        let mut ws = new_gmod_workspace();
        if let Some(wrapper) = wrapper {
            ws.def_file("lua/autorun/sh_netwrap.lua", wrapper);
        }
        ws.def_file("lua/autorun/server/send.lua", sender);
        let client_file_id = ws.def_file(
            "lua/autorun/client/receive.lua",
            r#"
            net.Receive("Branch", function()
                local x = net.ReadString()
            end)
            "#,
        );
        file_diagnostics(&mut ws, client_file_id)
    }

    const BRANCH_SEND: &str = r#"
        util.AddNetworkString("Branch")
        local function networkBranch(ply)
            net.Start("Branch")
            net.WriteString("x")
            if IsValid(ply) then
                net.Send(ply)
            else
                net.Broadcast()
            end
        end
        networkBranch(NULL)
    "#;

    #[gtest]
    fn test_send_in_if_else_with_wrapped_net_start_finds_counterpart() {
        let diagnostics = branch_send_receiver_diagnostics(Some(WRAPPED_NET_START), BRANCH_SEND);
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_send_in_if_else_with_local_forwarding_wrapper_finds_counterpart() {
        let diagnostics = branch_send_receiver_diagnostics(
            Some(
                r#"
                local netStart = net.Start
                function net.Start(name, unreliable)
                    netStart(name, unreliable)
                end
                "#,
            ),
            BRANCH_SEND,
        );
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_plain_send_with_local_forwarding_wrapper_finds_counterpart() {
        let diagnostics = branch_send_receiver_diagnostics(
            Some(
                r#"
                local netStart = net.Start
                function net.Start(name, unreliable)
                    netStart(name, unreliable)
                end
                "#,
            ),
            r#"
            util.AddNetworkString("Branch")
            net.Start("Branch")
            net.WriteString("x")
            net.Broadcast()
            "#,
        );
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_start_in_a_branch_keeps_the_earlier_message_sent_on_other_paths() {
        let diagnostics = branch_send_receiver_diagnostics(
            None,
            r#"
            util.AddNetworkString("Branch")
            util.AddNetworkString("Other")
            net.Start("Branch")
            if GetConVar("sv_other"):GetBool() then
                net.Start("Other")
            end
            net.WriteString("x")
            net.Broadcast()
            "#,
        );
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_send_after_a_start_in_a_branch_belongs_to_the_started_message() {
        let diagnostics = branch_send_receiver_diagnostics(
            None,
            r#"
            util.AddNetworkString("Branch")
            util.AddNetworkString("Other")
            net.Start("Branch")
            if GetConVar("sv_other"):GetBool() then
                net.Start("Other")
                net.Broadcast()
            end
            "#,
        );
        assert_that!(count_network_diagnostics(&diagnostics), eq(1usize));
    }

    #[gtest]
    fn test_writes_after_a_start_in_a_branch_belong_to_the_started_message() {
        let diagnostics = branch_send_receiver_diagnostics(
            None,
            r#"
            util.AddNetworkString("Branch")
            util.AddNetworkString("Other")
            net.Start("Branch")
            if GetConVar("sv_other"):GetBool() then
                net.Start("Other")
                net.WriteString("x")
            end
            net.Broadcast()
            "#,
        );
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteOrderMismatch),
            eq(1usize)
        );
    }

    #[gtest]
    fn test_writes_before_a_start_in_a_branch_belong_to_the_earlier_message() {
        let diagnostics = branch_send_receiver_diagnostics(
            None,
            r#"
            util.AddNetworkString("Branch")
            util.AddNetworkString("Other")
            net.Start("Branch")
            if GetConVar("sv_other"):GetBool() then
                net.WriteString("x")
                net.Start("Other")
            end
            net.Broadcast()
            "#,
        );
        assert_that!(count_network_diagnostics(&diagnostics), eq(0usize));
    }

    #[gtest]
    fn test_start_in_a_repeat_body_ends_the_earlier_message() {
        let diagnostics = branch_send_receiver_diagnostics(
            None,
            r#"
            util.AddNetworkString("Branch")
            util.AddNetworkString("Other")
            net.Start("Branch")
            repeat
                net.Start("Other")
            until true
            net.WriteString("x")
            net.Broadcast()
            "#,
        );
        assert_that!(count_network_diagnostics(&diagnostics), eq(1usize));
    }

    #[gtest]
    fn test_start_in_a_do_block_ends_the_earlier_message() {
        let diagnostics = branch_send_receiver_diagnostics(
            None,
            r#"
            util.AddNetworkString("Branch")
            util.AddNetworkString("Other")
            net.Start("Branch")
            do
                net.Start("Other")
            end
            net.WriteString("x")
            net.Broadcast()
            "#,
        );
        assert_that!(count_network_diagnostics(&diagnostics), eq(1usize));
    }

    #[gtest]
    fn test_send_in_if_else_checks_payload_count() {
        let diagnostics = branch_send_receiver_diagnostics(
            None,
            r#"
            util.AddNetworkString("Branch")
            local function networkBranch(ply)
                net.Start("Branch")
                if IsValid(ply) then
                    net.Send(ply)
                else
                    net.Broadcast()
                end
            end
            networkBranch(NULL)
            "#,
        );
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteOrderMismatch),
            eq(1usize)
        );
    }
}
