#[cfg(test)]
mod test {
    use crate::db_index::derivation::DerivationKind;
    use crate::{Emmyrc, VirtualWorkspace};

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
}
