#[cfg(test)]
mod test {
    use crate::db_index::derivation::DerivationKind;
    use crate::{Emmyrc, FileId, VirtualWorkspace};

    fn gmod_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws
    }

    const HELPERS: &str = "addons/mytest/lua/autorun/server/sv_helpers.lua";
    const CALLER: &str = "addons/mytest/lua/autorun/server/sv_caller.lua";

    const HELPERS_TEXT: &str = "function SendPing(target)
    net.Start(\"Ping\")
    net.WriteString(\"payload\")
    net.Send(target)
end

function SendPong()
    net.Start(\"Pong\")
    net.Broadcast()
end
";
    const CALLER_TEXT: &str = "SendPing(ply)
SendPong()
net.Start(\"Direct\")
net.WriteUInt(3, 8)
net.Broadcast()
net.Receive(\"Direct\", function()
    net.ReadUInt(8)
end)
";

    /// Edits to the helpers and their caller leave the net flows and
    /// diagnostics a cold build of the edited texts has: a helper that writes
    /// more, a shift above the caller's calls, and the revert of both.
    #[test]
    fn net_flows_after_edits_equal_a_cold_build() {
        let wider = HELPERS_TEXT.replace(
            "net.WriteString(\"payload\")",
            "net.WriteString(\"payload\")\n    net.WriteInt(1, 8)",
        );
        let shifted = format!("-- shifted\n{CALLER_TEXT}");

        let mut warm = gmod_workspace();
        warm.def_files(vec![(HELPERS, HELPERS_TEXT), (CALLER, CALLER_TEXT)]);
        warm.def_file(HELPERS, &wider);
        warm.def_file(CALLER, &shifted);
        let mut cold = gmod_workspace();
        cold.def_files(vec![(HELPERS, &wider), (CALLER, &shifted)]);

        let digest = warm.settled_digest();
        assert!(digest.iter().any(|line| line.contains("sends")));
        assert!(digest.iter().any(|line| line.contains("receives")));
        assert_eq!(digest, cold.settled_digest());
        assert!(warm.stale_derivations().is_empty());

        warm.def_file(HELPERS, HELPERS_TEXT);
        warm.def_file(CALLER, CALLER_TEXT);
        let mut original = gmod_workspace();
        original.def_files(vec![(HELPERS, HELPERS_TEXT), (CALLER, CALLER_TEXT)]);

        assert_eq!(warm.settled_digest(), original.settled_digest());
        assert!(warm.stale_derivations().is_empty());
    }

    /// Editing what one call targets re-runs that call's net fact alone, not
    /// the other calls' of its file, and leaves the flows and diagnostics a
    /// cold build of the edited text has.
    #[test]
    fn editing_one_calls_callee_re_runs_only_that_calls_net_fact() {
        const LIB: &str = "lua/autorun/sh_lib.lua";
        const USER: &str = "lua/autorun/sh_user.lua";
        const LIB_TEXT: &str =
            "LibA = {}\nLibA.f = function() end\n\nLibB = {}\nLibB.f = function() end\n";
        const EDITED_LIB_TEXT: &str =
            "LibA = {}\nLibA.f = nil           \n\nLibB = {}\nLibB.f = function() end\n";
        // Each call reads whether the calls before it return, so the edited
        // callee is called last.
        const USER_TEXT: &str = "LibB.f()\nLibB.f()\nLibA.f()\n";

        let mut ws = gmod_workspace();
        ws.def_files(vec![(LIB, LIB_TEXT), (USER, USER_TEXT)]);
        ws.def_file(LIB, EDITED_LIB_TEXT);
        let stats = ws.analysis.compilation.last_stats();
        let (net_calls, net_flows) = (
            stats.evaluations(DerivationKind::NetCall),
            stats.evaluations(DerivationKind::NetFlows),
        );
        let mut cold = gmod_workspace();
        cold.def_files(vec![(LIB, EDITED_LIB_TEXT), (USER, USER_TEXT)]);

        assert_eq!(net_calls, 1);
        assert!(net_flows <= 1, "{net_flows}");
        assert_eq!(ws.settled_digest(), cold.settled_digest());
        assert!(ws.stale_derivations().is_empty());
    }

    const SENDERS: &str = "addons/mytest/lua/autorun/server/sv_senders.lua";
    const SENDER_CALLS: &str = "addons/mytest/lua/autorun/server/sv_sender_calls.lua";
    const SENDERS_TEXT: &str = "function SendA()
    net.Start(\"A\")
    net.Broadcast()
end

function SendB()
    net.Start(\"B\")
    net.Broadcast()
end

function SendC()
    net.Start(\"C\")
    net.Broadcast()
end
";
    const SENDER_CALLS_TEXT: &str = "SendA()\nSendB()\nSendC()\n";

    /// A change to one helper's payload re-runs the flows of the calls into
    /// that helper alone: the edited file's seven calls run again, and of the
    /// three calls into it one does.
    #[test]
    fn editing_one_helper_re_runs_only_the_flows_of_the_calls_into_it() {
        let wider = SENDERS_TEXT.replace(
            "net.Start(\"C\")\n",
            "net.Start(\"C\")\n    net.WriteString(\"c\")\n",
        );

        let mut ws = gmod_workspace();
        ws.def_files(vec![
            (SENDERS, SENDERS_TEXT),
            (SENDER_CALLS, SENDER_CALLS_TEXT),
        ]);
        ws.def_file(SENDERS, &wider);
        let stats = ws.analysis.compilation.last_stats();
        let (net_flows, net_helpers) = (
            stats.evaluations(DerivationKind::NetFlows),
            stats.evaluations(DerivationKind::NetHelper),
        );
        let mut cold = gmod_workspace();
        cold.def_files(vec![(SENDERS, &wider), (SENDER_CALLS, SENDER_CALLS_TEXT)]);

        assert_eq!((net_flows, net_helpers), (7 + 1, 3));
        assert_eq!(ws.settled_digest(), cold.settled_digest());
        assert!(ws.stale_derivations().is_empty());
    }

    const PAYLOAD: &str = "addons/mytest/lua/autorun/server/sv_payload.lua";
    const NOTICES: &str = "addons/mytest/lua/autorun/server/sv_notices.lua";
    const PAYLOAD_TEXT: &str = "function WritePayload(value)
    net.WriteString(value)
    net.WriteBool(true)
end

function SendNotice(ply, text)
    net.Start(\"Notice\")
    WritePayload(text)
    net.Send(ply)
end
";

    fn notices_text(count: usize) -> String {
        (0..count)
            .map(|i| format!("SendNotice(ply, \"notice {i}\")\n"))
            .collect()
    }

    /// A send flow's message and the wire formats it writes.
    fn writes(message: &str, kinds: &[&str]) -> (String, Vec<String>) {
        (
            message.to_string(),
            kinds.iter().map(|kind| kind.to_string()).collect(),
        )
    }

    /// The id of the file at `path`.
    fn path_id(ws: &VirtualWorkspace, path: &str) -> FileId {
        ws.analysis
            .get_file_id(&ws.virtual_url_generator.new_uri(path))
            .expect("a workspace file")
    }

    /// The wire formats each send flow of `file_id` writes, by message.
    fn sent_writes(ws: &VirtualWorkspace, file_id: FileId) -> Vec<(String, Vec<String>)> {
        ws.analysis
            .compilation
            .get_db()
            .get_gmod_network_index()
            .get_file_data(file_id)
            .into_iter()
            .flat_map(|data| &data.send_flows)
            .map(|flow| {
                let writes = flow
                    .writes
                    .iter()
                    .map(|entry| entry.op.wire_format.to_string())
                    .collect();
                (flow.message_name.clone(), writes)
            })
            .collect()
    }

    /// Every call of a wrapper writes the payload of the helper the wrapper
    /// writes through, which the file's calls compose once and share.
    #[test]
    fn every_call_of_a_wrapper_writes_the_payload_its_shared_composition_holds() {
        let mut ws = gmod_workspace();
        let notices = notices_text(8);
        ws.def_files(vec![(PAYLOAD, PAYLOAD_TEXT), (NOTICES, notices.as_str())]);

        assert_eq!(
            sent_writes(&ws, path_id(&ws, NOTICES)),
            vec![writes("Notice", &["string", "bool"]); 8]
        );
        assert!(ws.stale_derivations().is_empty());
    }

    /// Editing the payload helper re-runs the calls that write through it,
    /// which then hold the flows and diagnostics a cold build has.
    #[test]
    fn editing_a_composed_helper_leaves_the_flows_of_a_cold_build() {
        let wider = PAYLOAD_TEXT.replace(
            "net.WriteBool(true)\n",
            "net.WriteBool(true)\n    net.WriteString(\"more\")\n",
        );
        let notices = notices_text(4);

        let mut warm = gmod_workspace();
        warm.def_files(vec![(PAYLOAD, PAYLOAD_TEXT), (NOTICES, notices.as_str())]);
        warm.def_file(PAYLOAD, &wider);
        let mut cold = gmod_workspace();
        cold.def_files(vec![(PAYLOAD, wider.as_str()), (NOTICES, notices.as_str())]);

        assert_eq!(
            sent_writes(&cold, path_id(&cold, NOTICES)),
            vec![writes("Notice", &["string", "bool", "string"]); 4]
        );
        assert_eq!(warm.settled_digest(), cold.settled_digest());
        assert!(warm.stale_derivations().is_empty());
    }

    const WRAPPER: &str = "addons/mytest/lua/autorun/server/sv_wrapper.lua";
    const WRAPPER_TEXT: &str = "function WriteBoth(value)
    WritePayload(value)
end
";
    const BOTH: &str = "addons/mytest/lua/autorun/server/sv_both.lua";
    const BOTH_TEXT: &str = "net.Start(\"Both\")
WriteBoth(\"a\")
net.Broadcast()
";

    /// A helper's composed ops name the positions of the helpers it reaches in
    /// other files: after a shift of one of those files and an edit of the
    /// caller, the caller holds the flows of a cold build.
    #[test]
    fn a_shift_of_a_reached_helpers_file_leaves_its_callers_the_flows_of_a_cold_build() {
        let shifted = format!("-- shifted\n{PAYLOAD_TEXT}");
        let edited_both = format!("-- edited\n{BOTH_TEXT}");

        let mut warm = gmod_workspace();
        warm.def_files(vec![
            (PAYLOAD, PAYLOAD_TEXT),
            (WRAPPER, WRAPPER_TEXT),
            (BOTH, BOTH_TEXT),
        ]);
        warm.def_file(PAYLOAD, &shifted);
        warm.def_file(BOTH, &edited_both);
        let mut cold = gmod_workspace();
        cold.def_files(vec![
            (PAYLOAD, shifted.as_str()),
            (WRAPPER, WRAPPER_TEXT),
            (BOTH, edited_both.as_str()),
        ]);

        assert_eq!(
            sent_writes(&cold, path_id(&cold, BOTH)),
            vec![writes("Both", &["string", "bool"])]
        );
        assert_eq!(warm.settled_digest(), cold.settled_digest());
        assert!(warm.stale_derivations().is_empty());
    }

    const LOOPS: &str = "addons/mytest/lua/autorun/server/sv_loops.lua";
    const LOOP_CALLS: &str = "addons/mytest/lua/autorun/server/sv_loop_calls.lua";
    const LOOPS_TEXT: &str = "function WriteLoopA(n)
    net.WriteString(\"a\")
    if n > 0 then
        WriteLoopB(n - 1)
    end
end

function WriteLoopB(n)
    net.WriteBool(true)
    WriteLoopA(n)
end

function FinishLoopA(ply)
    if ply then
        net.Send(ply)
    end
    FinishLoopB(ply)
end

function FinishLoopB(ply)
    FinishLoopA(ply)
end
";
    const LOOP_CALLS_TEXT: &str = "net.Start(\"LoopA\")
WriteLoopA(3)
FinishLoopA(ply)

net.Start(\"LoopB\")
WriteLoopB(2)
FinishLoopB(ply)
";

    /// Helpers that call each other settle after an edit of one of them and
    /// its revert as a cold build of each text does.
    #[test]
    fn mutually_recursive_helpers_settle_after_an_edit_and_its_revert_as_cold() {
        let wider = LOOPS_TEXT.replace(
            "net.WriteBool(true)\n",
            "net.WriteBool(true)\n    net.WriteString(\"b\")\n",
        );

        let mut warm = gmod_workspace();
        warm.def_files(vec![(LOOPS, LOOPS_TEXT), (LOOP_CALLS, LOOP_CALLS_TEXT)]);
        assert_eq!(
            sent_writes(&warm, path_id(&warm, LOOP_CALLS)),
            vec![
                writes("LoopA", &["string", "bool"]),
                writes("LoopB", &["bool", "string"]),
            ]
        );
        warm.def_file(LOOPS, &wider);
        let mut cold_wider = gmod_workspace();
        cold_wider.def_files(vec![(LOOPS, wider.as_str()), (LOOP_CALLS, LOOP_CALLS_TEXT)]);
        assert_eq!(warm.settled_digest(), cold_wider.settled_digest());
        assert!(warm.stale_derivations().is_empty());

        warm.def_file(LOOPS, LOOPS_TEXT);
        let mut cold = gmod_workspace();
        cold.def_files(vec![(LOOPS, LOOPS_TEXT), (LOOP_CALLS, LOOP_CALLS_TEXT)]);
        assert_eq!(warm.settled_digest(), cold.settled_digest());
        assert!(warm.stale_derivations().is_empty());
    }

    const PANEL_NET: &str = "addons/mytest/lua/autorun/client/cl_panel_net.lua";
    const PANEL_NET_TEXT: &str = "local classes = {}
classes.On = function(pnl, name, fn)
    pnl[name] = fn
end
classes.NetMessage = function(pnl, name, data)
    pnl:On(\"DoClick\", function()
        net.Start(name)
        data(pnl)
        net.SendToServer()
    end)
end
";
    const STORE: &str = "addons/mytest/lua/autorun/client/cl_store.lua";
    const STORE_TEXT: &str = "local button = vgui.Create(\"DButton\")
button:NetMessage(\"OrderAdd\", function()
    net.WriteString(\"chair\")
end)
";
    const ORDERS: &str = "addons/mytest/lua/autorun/server/sv_orders.lua";
    const ORDERS_TEXT: &str = "util.AddNetworkString(\"OrderAdd\")
net.Receive(\"OrderAdd\", function(len, ply)
    net.ReadString()
    net.ReadUInt(16)
end)
";

    /// A writer passed to a helper keeps the flows of a cold build through an
    /// edit of the writer and shifts of the caller and the helper, and its
    /// writes follow a delete and re-create of the helper.
    #[test]
    fn a_passed_writer_keeps_the_flows_of_a_cold_build_through_edits() {
        let wider = STORE_TEXT.replace(
            "net.WriteString(\"chair\")
",
            "net.WriteString(\"chair\")
    net.WriteUInt(2, 16)
",
        );
        let shifted_store = format!(
            "-- shifted
{wider}"
        );
        let shifted_panel = format!(
            "-- shifted
{PANEL_NET_TEXT}"
        );
        let cold = |files: Vec<(&str, &str)>| {
            let mut cold = gmod_workspace();
            cold.def_files(files);
            cold
        };

        let mut warm = gmod_workspace();
        warm.def_files(vec![
            (PANEL_NET, PANEL_NET_TEXT),
            (STORE, STORE_TEXT),
            (ORDERS, ORDERS_TEXT),
        ]);

        warm.def_file(STORE, &wider);
        let edited = cold(vec![
            (PANEL_NET, PANEL_NET_TEXT),
            (STORE, wider.as_str()),
            (ORDERS, ORDERS_TEXT),
        ]);
        assert_eq!(
            sent_writes(&edited, path_id(&edited, STORE)),
            vec![writes("OrderAdd", &["string", "uint"])]
        );
        assert_eq!(warm.settled_digest(), edited.settled_digest());

        warm.def_file(STORE, &shifted_store);
        warm.def_file(PANEL_NET, &shifted_panel);
        let shifted = cold(vec![
            (PANEL_NET, shifted_panel.as_str()),
            (STORE, shifted_store.as_str()),
            (ORDERS, ORDERS_TEXT),
        ]);
        assert_eq!(warm.settled_digest(), shifted.settled_digest());

        warm.analysis
            .remove_file_by_uri(&warm.virtual_url_generator.new_uri(PANEL_NET));
        assert!(sent_writes(&warm, path_id(&warm, STORE)).is_empty());

        warm.def_file(PANEL_NET, &shifted_panel);
        assert_eq!(
            sent_writes(&warm, path_id(&warm, STORE)),
            vec![writes("OrderAdd", &["string", "uint"])]
        );
        assert!(warm.stale_derivations().is_empty());
    }

    const INLINE_WRITER: &str = "addons/mytest/lua/autorun/server/sv_inline_writer.lua";
    const INLINE_WRITER_TEXT: &str = "local function writeWith(write)
    write()
end

function SendInline(ply)
    net.Start(\"Inline\")
    writeWith(function()
        net.WriteBool(true)
    end)
    net.Send(ply)
end
";
    const INLINE_CALLER: &str = "addons/mytest/lua/autorun/server/sv_inline_caller.lua";
    const INLINE_CALLER_TEXT: &str = "SendInline(ply)
";

    /// A closure a helper passes to another helper keeps its writes in the
    /// helper's callers through a shift of the helper's file.
    #[test]
    fn a_closure_a_helper_passes_on_keeps_its_writes_through_a_shift() {
        let shifted = format!(
            "-- shifted
{INLINE_WRITER_TEXT}"
        );

        let mut warm = gmod_workspace();
        warm.def_files(vec![
            (INLINE_WRITER, INLINE_WRITER_TEXT),
            (INLINE_CALLER, INLINE_CALLER_TEXT),
        ]);
        warm.def_file(INLINE_WRITER, &shifted);
        let mut cold = gmod_workspace();
        cold.def_files(vec![
            (INLINE_WRITER, shifted.as_str()),
            (INLINE_CALLER, INLINE_CALLER_TEXT),
        ]);

        assert_eq!(
            sent_writes(&cold, path_id(&cold, INLINE_CALLER)),
            vec![writes("Inline", &["bool"])]
        );
        assert_eq!(warm.settled_digest(), cold.settled_digest());
        assert!(warm.stale_derivations().is_empty());
    }

    const LISTEN: &str = "addons/mytest/lua/autorun/server/sv_listen.lua";
    const LISTEN_TEXT: &str = "function Listen(name, reader)
    net.Receive(name, function(len, ply)
        reader(ply)
    end)
end
";
    const ORDER_LISTENER: &str = "addons/mytest/lua/autorun/server/sv_order_listener.lua";
    const ORDER_LISTENER_TEXT: &str = "Listen(\"OrderAdd\", function(ply)
    net.ReadString()
end)
";

    /// The wire formats each receive flow of `file_id` reads, by message.
    fn received_reads(ws: &VirtualWorkspace, file_id: FileId) -> Vec<(String, Vec<String>)> {
        ws.analysis
            .compilation
            .get_db()
            .get_gmod_network_index()
            .get_file_data(file_id)
            .into_iter()
            .flat_map(|data| &data.receive_flows)
            .map(|flow| {
                let reads = flow
                    .reads
                    .iter()
                    .map(|entry| entry.op.wire_format.to_string())
                    .collect();
                (flow.message_name.clone(), reads)
            })
            .collect()
    }

    /// A reader passed to a receive helper keeps the flows of a cold build
    /// through an edit of the reader and shifts of the caller and the helper,
    /// and its reads follow a delete and re-create of the helper.
    #[test]
    fn a_passed_reader_keeps_the_flows_of_a_cold_build_through_edits() {
        let wider = ORDER_LISTENER_TEXT.replace(
            "net.ReadString()
",
            "net.ReadString()
    net.ReadUInt(16)
",
        );
        let shifted_listener = format!(
            "-- shifted
{wider}"
        );
        let shifted_listen = format!(
            "-- shifted
{LISTEN_TEXT}"
        );
        let cold = |files: Vec<(&str, &str)>| {
            let mut cold = gmod_workspace();
            cold.def_files(files);
            cold
        };

        let mut warm = gmod_workspace();
        warm.def_files(vec![
            (LISTEN, LISTEN_TEXT),
            (ORDER_LISTENER, ORDER_LISTENER_TEXT),
        ]);

        warm.def_file(ORDER_LISTENER, &wider);
        let edited = cold(vec![
            (LISTEN, LISTEN_TEXT),
            (ORDER_LISTENER, wider.as_str()),
        ]);
        assert_eq!(
            received_reads(&edited, path_id(&edited, ORDER_LISTENER)),
            vec![writes("OrderAdd", &["string", "uint"])]
        );
        assert_eq!(warm.settled_digest(), edited.settled_digest());

        warm.def_file(ORDER_LISTENER, &shifted_listener);
        warm.def_file(LISTEN, &shifted_listen);
        let shifted = cold(vec![
            (LISTEN, shifted_listen.as_str()),
            (ORDER_LISTENER, shifted_listener.as_str()),
        ]);
        assert_eq!(warm.settled_digest(), shifted.settled_digest());

        warm.analysis
            .remove_file_by_uri(&warm.virtual_url_generator.new_uri(LISTEN));
        assert!(received_reads(&warm, path_id(&warm, ORDER_LISTENER)).is_empty());

        warm.def_file(LISTEN, &shifted_listen);
        assert_eq!(
            received_reads(&warm, path_id(&warm, ORDER_LISTENER)),
            vec![writes("OrderAdd", &["string", "uint"])]
        );
        assert!(warm.stale_derivations().is_empty());
    }
}
