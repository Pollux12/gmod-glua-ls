#[cfg(test)]
mod test {
    use crate::{Emmyrc, GmodRealm, NetReceiveFlow, NetSendFlow, VirtualWorkspace};
    use googletest::prelude::*;

    fn set_gmod_enabled(ws: &mut VirtualWorkspace) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        // Net ops are recognized through signature metadata, so the annotated
        // builtins must be present or no flows are collected at all.
        ws.def_gmod_call_arg_builtins();
    }

    /// Ops are identified by their annotated wire format now, not by a closed
    /// enum, so assertions compare the wire format strings. Direction is implied
    /// by which side of the flow the op came from.
    fn send_op_kinds(flow: &NetSendFlow) -> Vec<String> {
        flow.writes
            .iter()
            .map(|entry| entry.op.wire_format.to_string())
            .collect()
    }

    fn receive_op_kinds(flow: &NetReceiveFlow) -> Vec<String> {
        flow.reads
            .iter()
            .map(|entry| entry.op.wire_format.to_string())
            .collect()
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
        assert_that!(flow.send_kind.receiver_realm, eq(GmodRealm::Client));
        assert_that!(flow.send_kind.target_arg_idx, none());
        assert_that!(flow.send_display_name.as_str(), eq("net.Broadcast"));
        assert_that!(flow.is_wrapped, eq(false));
        assert_that!(
            send_op_kinds(flow),
            eq(&vec![
                "entity".to_string(),
                "string".to_string(),
                "int".to_string(),
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
                "entity".to_string(),
                "string".to_string(),
                "int".to_string(),
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
        assert_that!(flow_a.send_kind.receiver_realm, eq(GmodRealm::Client));
        assert_that!(flow_a.send_display_name.as_str(), eq("net.Send"));
        assert_that!(send_op_kinds(flow_a), eq(&vec!["string".to_string()]));

        let flow_b = &data.send_flows[1];
        assert_that!(flow_b.message_name.as_str(), eq("MsgB"));
        assert_that!(flow_b.send_kind.receiver_realm, eq(GmodRealm::Server));
        assert_that!(flow_b.send_display_name.as_str(), eq("net.SendToServer"));
        assert_that!(
            send_op_kinds(flow_b),
            eq(&vec!["bool".to_string(), "float".to_string()])
        );
    }

    #[gtest]
    fn test_extended_send_methods_are_recognized() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file(
            "addons/mytest/lua/autorun/server/net_send_kinds.lua",
            r#"
            net.Start("A")
            net.WriteString("x")
            net.SendOmit(ply)

            net.Start("B")
            net.WriteString("y")
            net.SendPAS(Vector(0,0,0))

            net.Start("C")
            net.WriteString("z")
            net.SendPVS(Vector(0,0,0))

            net.Start("D")
            net.WriteString("w")
            net.Broadcast()

            net.Start("E")
            net.WriteString("u")
            net.SendToServer()
            "#,
        );

        let data = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(file_id)
            .expect("expected network data");

        assert_that!(data.send_flows.len(), eq(5usize));
        let names: Vec<String> = data
            .send_flows
            .iter()
            .map(|f| f.send_display_name.to_string())
            .collect();
        assert_that!(
            names,
            eq(&vec![
                "net.SendOmit".to_string(),
                "net.SendPAS".to_string(),
                "net.SendPVS".to_string(),
                "net.Broadcast".to_string(),
                "net.SendToServer".to_string(),
            ])
        );
        let realms: Vec<GmodRealm> = data
            .send_flows
            .iter()
            .map(|f| f.send_kind.receiver_realm)
            .collect();
        assert_that!(
            realms,
            eq(&vec![
                GmodRealm::Client,
                GmodRealm::Client,
                GmodRealm::Client,
                GmodRealm::Client,
                GmodRealm::Server,
            ])
        );
    }

    #[gtest]
    fn test_send_flow_includes_writes_inside_control_flow_blocks() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file(
            "addons/mytest/lua/autorun/server/net_send_control_flow.lua",
            r#"
            net.Start("Msg")
            net.WriteUInt(1, 8)
            net.WriteString("name")
            if ( true ) then
                for _, wsid in ipairs({ "123" }) do
                    net.WriteString(wsid)
                end
            end
            net.WriteBool(true)
            net.Broadcast()
            "#,
        );

        let data = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(file_id)
            .expect("expected network data");

        assert_that!(data.send_flows.len(), eq(1usize));
        let flow = &data.send_flows[0];
        assert_that!(
            send_op_kinds(flow),
            eq(&vec![
                "uint".to_string(),
                "string".to_string(),
                "string".to_string(),
                "bool".to_string(),
            ])
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
        assert_that!(receive_op_kinds(flow), eq(&vec!["int".to_string()]));
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
            .get_file_data(file_id);

        assert!(
            data.is_none_or(|data| { data.send_flows.is_empty() && data.receive_flows.is_empty() })
        );
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
            .get_file_data(file_id);

        assert!(
            data.is_none_or(|data| { data.send_flows.is_empty() && data.receive_flows.is_empty() })
        );
    }

    #[gtest]
    fn test_wrapped_send_flow_stub_is_recorded_for_function_body_start_without_send() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file(
            "addons/mytest/lua/autorun/server/net_wrapped_stub.lua",
            r#"
            function Glide.StartCommand(id)
                net.Start("glide.command")
                net.WriteUInt(id, 8)
            end

            Glide.StartCommand(1)
            net.Send(Entity(1))
            "#,
        );

        let data = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(file_id)
            .expect("expected network data");

        let wrapped_flows: Vec<_> = data
            .send_flows
            .iter()
            .filter(|flow| flow.message_name == "glide.command" && flow.is_wrapped)
            .collect();

        assert_that!(wrapped_flows.len(), ge(1usize));
        let wrapped_flow = wrapped_flows[0];
        assert_that!(wrapped_flow.writes.len(), eq(0usize));
        // Wrapped stubs record a placeholder realm; only counterpart presence
        // is meaningful for them, so assert the flag rather than the realm.
        assert_that!(wrapped_flow.is_wrapped, eq(true));
        assert_that!(wrapped_flow.send_range, eq(wrapped_flow.start_range));
    }

    #[gtest]
    fn test_send_flow_through_two_local_wrapper_levels() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);

        let file_id = ws.def_file(
            "addons/mytest/lua/autorun/server/net_local_chain.lua",
            r#"
            local function fwd(name)
                net.Start(name)
                net.WriteString("payload")
                net.Broadcast()
            end

            local function api(name)
                fwd(name)
            end

            api("ChainedMessage")
            "#,
        );

        let data = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(file_id)
            .expect("expected network data");

        let chained: Vec<_> = data
            .send_flows
            .iter()
            .filter(|flow| flow.message_name == "ChainedMessage")
            .collect();

        assert_that!(chained.len(), ge(1usize));
        assert_that!(send_op_kinds(chained[0]), eq(&vec!["string".to_string()]));
    }
}
