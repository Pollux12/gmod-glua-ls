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
    fn test_config_toggle_disables_type_mismatch() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.network.diagnostics.type_mismatch = false;
        ws.update_emmyrc(emmyrc);
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
        assert_that!(
            count_diagnostic(&diagnostics, DiagnosticCode::GmodNetReadWriteTypeMismatch),
            eq(0usize)
        );
    }

    #[gtest]
    fn test_config_toggle_disables_missing_counterpart() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        emmyrc.gmod.network.diagnostics.missing_counterpart = false;
        ws.update_emmyrc(emmyrc);
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::GmodNetMissingNetworkCounterpart);

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
}
