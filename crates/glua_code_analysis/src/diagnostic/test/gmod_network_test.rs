#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};
    use googletest::prelude::*;
    use lsp_types::{Diagnostic, NumberOrString};
    use tokio_util::sync::CancellationToken;

    fn new_gmod_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
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
        emmyrc.diagnostics.disable =
            vec![DiagnosticCode::GmodNetReadWriteTypeMismatch];
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
        emmyrc.diagnostics.disable =
            vec![DiagnosticCode::GmodNetMissingNetworkCounterpart];
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
}
