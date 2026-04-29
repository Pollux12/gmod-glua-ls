use crate::FileId;

use super::{GmodNetworkIndex, NetOpEntry, NetReceiveFlow, NetSendFlow, NetSendKind};
use crate::db_index::gmod_infer::{GmodInferIndex, GmodRealm};

/// True when a sender flow's writes are structurally compatible with a
/// receiver flow's reads — accounting for `dynamic` (control-flow guarded)
/// ops on either side as `0..N` repetitions of their kind.
///
/// Used to pair sender/receiver flows for the same message name when the
/// same `net.MyMessage` is used with multiple distinct read/write patterns
/// across the codebase. Cheap memoized DP, `O(writes.len() * reads.len())`
/// in the worst case but the vectors are typically short (<10 ops).
pub fn flows_can_match(send_flow: &NetSendFlow, receive_flow: &NetReceiveFlow) -> bool {
    if is_perfect_read_write_match(send_flow, receive_flow) {
        return true;
    }

    if !send_flow.writes.iter().any(|w| w.dynamic) && !receive_flow.reads.iter().any(|r| r.dynamic)
    {
        return false;
    }

    let writes = send_flow.writes.as_slice();
    let reads = receive_flow.reads.as_slice();
    let mut memo = vec![vec![None; reads.len() + 1]; writes.len() + 1];
    flow_match_dp(writes, reads, 0, 0, &mut memo)
}

fn is_perfect_read_write_match(send_flow: &NetSendFlow, receive_flow: &NetReceiveFlow) -> bool {
    send_flow.writes.len() == receive_flow.reads.len()
        && send_flow
            .writes
            .iter()
            .zip(receive_flow.reads.iter())
            .all(|(write, read)| write.kind.to_read_counterpart() == Some(read.kind))
}

fn flow_match_dp(
    writes: &[NetOpEntry],
    reads: &[NetOpEntry],
    i: usize,
    j: usize,
    memo: &mut Vec<Vec<Option<bool>>>,
) -> bool {
    if let Some(cached) = memo[i][j] {
        return cached;
    }

    let n = writes.len();
    let m = reads.len();

    let result = if i == n && j == m {
        true
    } else if i == n {
        reads[j..].iter().all(|r| r.dynamic)
    } else if j == m {
        writes[i..].iter().all(|w| w.dynamic)
    } else {
        let w = &writes[i];
        let r = &reads[j];
        let kinds_match = w.kind.to_read_counterpart() == Some(r.kind);

        let mut ok = false;

        if w.dynamic && flow_match_dp(writes, reads, i + 1, j, memo) {
            ok = true;
        }
        if !ok && r.dynamic && flow_match_dp(writes, reads, i, j + 1, memo) {
            ok = true;
        }
        if !ok && kinds_match && flow_match_dp(writes, reads, i + 1, j + 1, memo) {
            ok = true;
        }
        if !ok && kinds_match && w.dynamic && flow_match_dp(writes, reads, i, j + 1, memo) {
            ok = true;
        }
        if !ok && kinds_match && r.dynamic && flow_match_dp(writes, reads, i + 1, j, memo) {
            ok = true;
        }

        ok
    };

    memo[i][j] = Some(result);
    result
}

/// The realm a `net.Send*` call's payload will arrive in. Broadcast/PVS/PAS/
/// SendOmit/Send all target clients; only `SendToServer` targets the server.
pub fn expected_receiver_realm(send_kind: NetSendKind) -> Option<GmodRealm> {
    match send_kind {
        NetSendKind::Send
        | NetSendKind::Broadcast
        | NetSendKind::Omit
        | NetSendKind::PAS
        | NetSendKind::PVS => Some(GmodRealm::Client),
        NetSendKind::SendToServer => Some(GmodRealm::Server),
    }
}

pub fn is_strict_realm(realm: GmodRealm) -> bool {
    matches!(realm, GmodRealm::Client | GmodRealm::Server)
}

pub fn is_opposite_strict_realm_pair(sender_realm: GmodRealm, receiver_realm: GmodRealm) -> bool {
    matches!(
        (sender_realm, receiver_realm),
        (GmodRealm::Server, GmodRealm::Client) | (GmodRealm::Client, GmodRealm::Server)
    )
}

/// Returns the senders of `receive_flow.message_name` that are realistic
/// counterparts: their realm is the opposite of the receiver, the send kind's
/// expected destination realm is this receiver's realm, and (when the
/// receiver's reads are inspectable) the read/write patterns line up via
/// [`flows_can_match`]. Wrapped helper flows are skipped — they have no real
/// send call to attribute. Pattern matching is suppressed when the receiver
/// is opaque (callback couldn't be resolved) so we can still report the
/// realistic candidates.
pub fn pair_senders_for_receive<'a>(
    network_index: &'a GmodNetworkIndex,
    infer_index: &GmodInferIndex,
    receive_file_id: FileId,
    receive_flow: &NetReceiveFlow,
) -> Vec<(FileId, &'a NetSendFlow)> {
    let receive_realm =
        infer_index.get_realm_at_offset(&receive_file_id, receive_flow.receive_range.start());
    if !is_strict_realm(receive_realm) {
        return Vec::new();
    }

    let candidates = network_index.get_send_flows_for_message(&receive_flow.message_name);
    candidates
        .into_iter()
        .filter(|(send_file_id, send_flow)| {
            if send_flow.is_wrapped {
                return false;
            }
            let sender_realm =
                infer_index.get_realm_at_offset(send_file_id, send_flow.start_range.start());
            if !is_strict_realm(sender_realm) {
                return false;
            }
            let Some(expected) = expected_receiver_realm(send_flow.send_kind) else {
                return false;
            };
            if expected != receive_realm {
                return false;
            }
            if !is_opposite_strict_realm_pair(sender_realm, receive_realm) {
                return false;
            }
            // Pattern check is best-effort: when the receiver is opaque we
            // can't inspect its reads, so we accept all realm-matched
            // candidates rather than dropping all of them.
            if receive_flow.reads_opaque {
                return true;
            }
            flows_can_match(send_flow, receive_flow)
        })
        .collect()
}
