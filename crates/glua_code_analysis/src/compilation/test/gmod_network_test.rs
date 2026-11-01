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

    #[test]
    fn network_flows_follow_cross_file_alias_edit_delete_and_reopen() {
        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let alias_path = "addons/mytest/lua/autorun/server/a_alias.lua";
        let alias_id = ws.def_file(alias_path, "WritePayload = net.WriteString");
        let sender_id = ws.def_file(
            "addons/mytest/lua/autorun/server/b_sender.lua",
            "net.Start(\"AliasMessage\")\nWritePayload(1)\nnet.Broadcast()",
        );
        let writes = |ws: &VirtualWorkspace| {
            ws.analysis
                .compilation
                .get_db()
                .get_gmod_network_index()
                .get_file_data(sender_id)
                .into_iter()
                .flat_map(|data| &data.send_flows)
                .flat_map(send_op_kinds)
                .collect::<Vec<_>>()
        };
        assert_eq!(writes(&ws), vec!["string"]);

        let alias_uri = ws.virtual_url_generator.new_uri(alias_path);
        ws.analysis
            .update_file_text_only(&alias_uri, "WritePayload = net.WriteBool".into());
        ws.analysis.reindex_files(vec![alias_id]);
        assert_eq!(writes(&ws), vec!["bool"]);
        let incremental = writes(&ws);
        ws.analysis.reindex();
        assert_eq!(writes(&ws), incremental);

        ws.analysis.update_file_by_uri(&alias_uri, None);
        ws.analysis.reindex_files(vec![alias_id]);
        assert!(writes(&ws).is_empty());

        let reopened_id = ws.def_file(alias_path, "WritePayload = net.WriteString");
        ws.analysis.reindex_files(vec![reopened_id]);
        assert_eq!(writes(&ws), vec!["string"]);
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

    /// What the net derivations of `file_id`'s calls read, as keys.
    fn net_reads(
        db: &crate::DbIndex,
        file_id: crate::FileId,
    ) -> Vec<crate::db_index::derivation::FactKey> {
        use crate::db_index::derivation::DerivationKind;
        db.reader_graph()
            .derivations_of_file(file_id)
            .filter(|id| matches!(id.kind, DerivationKind::NetCall | DerivationKind::NetFlows))
            .flat_map(|id| db.reader_graph().reads_of(id).map(|key| key.key().clone()))
            .collect()
    }

    #[gtest]
    fn a_files_network_flows_read_only_its_own_helper_call_sites() {
        use crate::db_index::derivation::FactKey;

        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        ws.def_file(
            "addons/mytest/lua/autorun/client/helpers.lua",
            r#"
            function SendPing()
                net.Start("Ping")
                net.SendToServer()
            end
            "#,
        );
        let caller = ws.def_file(
            "addons/mytest/lua/autorun/client/caller.lua",
            r#"
            SendPing()
            "#,
        );
        ws.def_file(
            "addons/mytest/lua/autorun/client/other.lua",
            r#"
            SendPing()
            "#,
        );

        let workspace_wide = net_reads(ws.get_db_mut(), caller)
            .into_iter()
            .filter(|key| matches!(key, FactKey::ReferencesByName(_)))
            .collect::<Vec<_>>();
        expect_that!(workspace_wide, is_empty());
    }

    #[gtest]
    fn a_files_network_reads_do_not_grow_with_the_workspace_helper_names() {
        let network_reads = |helper_count: usize| {
            let mut ws = VirtualWorkspace::new();
            set_gmod_enabled(&mut ws);
            let helpers = (0..helper_count)
                .map(|i| {
                    format!(
                        "function SendPing{i}()
    net.Start(\"Ping{i}\")
    net.SendToServer()
end
"
                    )
                })
                .collect::<String>();
            ws.def_file("addons/mytest/lua/autorun/client/helpers.lua", &helpers);
            let caller = ws.def_file(
                "addons/mytest/lua/autorun/client/caller.lua",
                "SendPing0()
",
            );
            net_reads(ws.get_db_mut(), caller).len()
        };

        expect_that!(network_reads(40), eq(network_reads(5)));
    }

    #[gtest]
    fn net_helper_names_expanded_on_workers_match_a_serial_expansion_cold_and_after_an_edit() {
        use crate::compilation::analyzer::parallel;
        use crate::db_index::derivation::{DerivationId, DerivationKind};

        let count = 12;
        let mut files = vec![(
            "addons/mytest/lua/autorun/server/helpers.lua".to_string(),
            "function SendPing()
 net.Start(\"Ping\")
 net.SendToServer()
end
"
            .to_string(),
        )];
        for i in 0..count {
            files.push((
                format!("addons/mytest/lua/autorun/server/wrap{i}.lua"),
                format!(
                    "function Wrap{i}()
 SendPing()
end
"
                ),
            ));
            files.push((
                format!("addons/mytest/lua/autorun/server/outer{i}.lua"),
                format!(
                    "function Outer{i}()
 Wrap{i}()
end
"
                ),
            ));
        }
        let files = files
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect::<Vec<_>>();
        let snapshot = |ws: &mut VirtualWorkspace| {
            let db = ws.get_db_mut();
            let id = DerivationId::workspace(DerivationKind::NetHelperNames);
            let mut reads = db
                .reader_graph()
                .reads_of(id)
                .map(|key| format!("{:?}", key.key()))
                .collect::<Vec<_>>();
            reads.sort();
            let network = db.get_gmod_network_index();
            let names = (0..count)
                .flat_map(|i| [format!("Wrap{i}"), format!("Outer{i}")])
                .map(|name| network.is_helper_name(&name))
                .collect::<Vec<_>>();
            (reads, names)
        };
        let analyse = |threads: usize| {
            parallel::set_analysis_threads(threads);
            let mut ws = VirtualWorkspace::new();
            set_gmod_enabled(&mut ws);
            ws.def_files(files.clone());
            let cold = snapshot(&mut ws);
            ws.def_file(
                "addons/mytest/lua/autorun/server/wrap3.lua",
                "function Wrap3()
end
",
            );
            let warm = snapshot(&mut ws);
            (cold, warm)
        };
        let threads = parallel::THREADS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let serial = analyse(1);
        let parallel_run = analyse(4);
        parallel::set_analysis_threads(0);
        drop(threads);

        expect_that!(serial.0.1.iter().all(|named| *named), eq(true));
        expect_that!(serial.1.1[6], eq(false));
        expect_that!(
            serial.0.0.iter().any(|key| key.starts_with("Syntax(")),
            eq(true)
        );
        expect_that!(&parallel_run, eq(&serial));
    }

    fn define_send_ping_helper(ws: &mut VirtualWorkspace) -> crate::FileId {
        ws.def_file(
            "addons/mytest/lua/autorun/server/helpers.lua",
            r#"
            function SendPing(target)
                net.Start("Ping")
                net.WriteString("payload")
                net.Send(target)
            end
            "#,
        )
    }

    #[gtest]
    fn a_callers_network_flows_read_the_helpers_summary_not_its_body() {
        use crate::db_index::derivation::FactKey;

        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        let helpers = define_send_ping_helper(&mut ws);
        let caller = ws.def_file(
            "addons/mytest/lua/autorun/server/caller.lua",
            "SendPing(ply)\n",
        );

        let db = ws.get_db_mut();
        let flows = db
            .get_gmod_network_index()
            .get_file_data(caller)
            .expect("the caller has network data")
            .send_flows
            .clone();
        assert_that!(flows.len(), eq(1usize));
        expect_that!(flows[0].message_name, eq("Ping"));
        expect_that!(send_op_kinds(&flows[0]), eq(&vec!["string".to_string()]));
        expect_that!(
            flows[0].materialized_from.map(|origin| origin.file_id),
            some(eq(helpers))
        );

        let reads = net_reads(db, caller);
        expect_that!(
            reads
                .iter()
                .any(|key| matches!(key, FactKey::NetHelper(site) if site.file_id == helpers)),
            eq(true)
        );
        expect_that!(
            reads
                .iter()
                .any(|key| matches!(key, FactKey::NetCall(site) if site.file_id == helpers)),
            eq(false)
        );
        expect_that!(reads.contains(&FactKey::FileText(helpers)), eq(false));
    }

    #[gtest]
    fn reindexing_a_caller_does_not_reanalyse_the_helper_body() {
        use crate::db_index::derivation::DerivationKind;

        let mut ws = VirtualWorkspace::new();
        set_gmod_enabled(&mut ws);
        define_send_ping_helper(&mut ws);
        for i in 0..3 {
            ws.def_file(
                &format!("addons/mytest/lua/autorun/server/caller{i}.lua"),
                "SendPing(ply)\n",
            );
        }
        let caller_path = "addons/mytest/lua/autorun/server/caller0.lua";
        let caller = ws.def_file(caller_path, "SendPing(ply)\nSendPing(ply)\n");

        let stats = ws.analysis.compilation.last_stats();
        expect_that!(stats.evaluations(DerivationKind::NetHelper), eq(0));
        expect_that!(stats.evaluations(DerivationKind::NetCall), eq(2));
        let flows = ws
            .get_db_mut()
            .get_gmod_network_index()
            .get_file_data(caller)
            .expect("the caller has network data")
            .send_flows
            .len();
        expect_that!(flows, eq(2usize));
    }
}
