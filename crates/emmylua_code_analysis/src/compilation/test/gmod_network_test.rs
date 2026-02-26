#[cfg(test)]
mod test {
    use crate::{Emmyrc, NetOpKind, NetReceiveFlow, NetSendFlow, NetSendKind, VirtualWorkspace};
    use googletest::prelude::*;

    fn set_gmod_enabled(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
    }

    fn send_op_kinds(flow: &NetSendFlow) -> Vec<NetOpKind> {
        flow.writes.iter().map(|entry| entry.kind).collect()
    }

    fn receive_op_kinds(flow: &NetReceiveFlow) -> Vec<NetOpKind> {
        flow.reads.iter().map(|entry| entry.kind).collect()
    }

    #[gtest]
    fn test_basic_send_flow_extraction() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file(
            "addons/mytest/lua/autorun/server/net_send.lua",
            r#"
            net.Start("MyMessage")
            net.WriteEntity(ent)
            net.WriteString("hello")
            net.WriteInt(42)
            net.Broadcast()
            "#,
        );

        let data = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(file_id)
            .expect("expected network data");

        assert_that!(data.send_flows.len(), eq(1usize));
        expect_that!(data.receive_flows.len(), eq(0usize));

        let flow = &data.send_flows[0];
        assert_that!(flow.message_name.as_str(), eq("MyMessage"));
        assert_that!(flow.send_kind, eq(NetSendKind::Broadcast));
        assert_that!(
            send_op_kinds(flow),
            eq(&vec![
                NetOpKind::WriteEntity,
                NetOpKind::WriteString,
                NetOpKind::WriteInt,
            ])
        );
    }

    #[gtest]
    fn test_receive_flow_extraction() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file(
            "addons/mytest/lua/autorun/server/net_receive.lua",
            r#"
            net.Receive("MyMessage", function()
                local ent = net.ReadEntity()
                local str = net.ReadString()
                local num = net.ReadInt()
            end)
            "#,
        );

        let data = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(file_id)
            .expect("expected network data");

        assert_that!(data.receive_flows.len(), eq(1usize));
        expect_that!(data.send_flows.len(), eq(0usize));

        let flow = &data.receive_flows[0];
        assert_that!(flow.message_name.as_str(), eq("MyMessage"));
        assert_that!(
            receive_op_kinds(flow),
            eq(&vec![
                NetOpKind::ReadEntity,
                NetOpKind::ReadString,
                NetOpKind::ReadInt,
            ])
        );
    }

    #[gtest]
    fn test_multiple_messages_in_one_file() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file(
            "addons/mytest/lua/autorun/server/net_multi.lua",
            r#"
            net.Start("MsgA")
            net.WriteString("hello")
            net.Send(ply)

            net.Start("MsgB")
            net.WriteBool(true)
            net.WriteFloat(3.14)
            net.SendToServer()
            "#,
        );

        let data = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(file_id)
            .expect("expected network data");

        assert_that!(data.send_flows.len(), eq(2usize));
        expect_that!(data.receive_flows.len(), eq(0usize));

        let flow_a = &data.send_flows[0];
        assert_that!(flow_a.message_name.as_str(), eq("MsgA"));
        assert_that!(flow_a.send_kind, eq(NetSendKind::Send));
        assert_that!(send_op_kinds(flow_a), eq(&vec![NetOpKind::WriteString]));

        let flow_b = &data.send_flows[1];
        assert_that!(flow_b.message_name.as_str(), eq("MsgB"));
        assert_that!(flow_b.send_kind, eq(NetSendKind::SendToServer));
        assert_that!(
            send_op_kinds(flow_b),
            eq(&vec![NetOpKind::WriteBool, NetOpKind::WriteFloat])
        );
    }

    #[gtest]
    fn test_nested_closure_reads_are_not_included_in_parent_callback() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file(
            "addons/mytest/lua/autorun/server/net_nested.lua",
            r#"
            net.Receive("Clean", function()
                local x = net.ReadInt()
                local fn = function()
                    net.ReadString()
                end
            end)
            "#,
        );

        let data = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(file_id)
            .expect("expected network data");

        assert_that!(data.receive_flows.len(), eq(1usize));
        let flow = &data.receive_flows[0];
        assert_that!(flow.message_name.as_str(), eq("Clean"));
        assert_that!(receive_op_kinds(flow), eq(&vec![NetOpKind::ReadInt]));
    }

    #[gtest]
    fn test_dynamic_message_names_are_skipped() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file(
            "addons/mytest/lua/autorun/server/net_dynamic.lua",
            r#"
            local msg = "dynamic"
            net.Start(msg)
            net.WriteString("test")
            net.Broadcast()
            "#,
        );

        let data = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(file_id)
            .expect("expected network data");

        assert_that!(data.send_flows.len(), eq(0usize));
        expect_that!(data.receive_flows.len(), eq(0usize));
    }

    #[gtest]
    fn test_incomplete_send_flow_without_endpoint_is_not_recorded() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file(
            "addons/mytest/lua/autorun/server/net_incomplete.lua",
            r#"
            net.Start("Incomplete")
            net.WriteString("hello")
            "#,
        );

        let data = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(file_id)
            .expect("expected network data");

        assert_that!(data.send_flows.len(), eq(0usize));
        expect_that!(data.receive_flows.len(), eq(0usize));
    }
}
