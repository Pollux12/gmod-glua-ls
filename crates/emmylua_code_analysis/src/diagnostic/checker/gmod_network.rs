use crate::{
    DiagnosticCode, GmodRealm, NetOpKind, NetReceiveFlow, NetSendFlow, NetSendKind, SemanticModel,
};

use super::{Checker, DiagnosticContext};

pub struct GmodNetworkChecker;

impl Checker for GmodNetworkChecker {
    const CODES: &[DiagnosticCode] = &[
        DiagnosticCode::GmodNetReadWriteTypeMismatch,
        DiagnosticCode::GmodNetReadWriteOrderMismatch,
        DiagnosticCode::GmodNetMissingNetworkCounterpart,
    ];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let emmyrc = semantic_model.get_emmyrc();
        if !emmyrc.gmod.enabled || !emmyrc.gmod.network.enabled {
            return;
        }

        let diagnostics = &emmyrc.gmod.network.diagnostics;
        if !diagnostics.type_mismatch
            && !diagnostics.order_mismatch
            && !diagnostics.missing_counterpart
        {
            return;
        }

        let file_id = semantic_model.get_file_id();
        let db = semantic_model.get_db();
        let network_index = db.get_gmod_network_index();
        let infer_index = db.get_gmod_infer_index();
        let Some(file_data) = network_index.get_file_data(file_id) else {
            return;
        };

        if diagnostics.type_mismatch || diagnostics.order_mismatch {
            check_read_write_mismatch(
                context,
                file_id,
                file_data.receive_flows.as_slice(),
                network_index,
                infer_index,
                diagnostics.type_mismatch,
                diagnostics.order_mismatch,
            );
        }

        if diagnostics.missing_counterpart {
            check_missing_send_counterpart(
                context,
                file_id,
                file_data.send_flows.as_slice(),
                network_index,
                infer_index,
            );
            check_missing_receive_counterpart(
                context,
                file_id,
                file_data.receive_flows.as_slice(),
                network_index,
                infer_index,
            );
        }
    }
}

fn check_read_write_mismatch(
    context: &mut DiagnosticContext,
    file_id: crate::FileId,
    receive_flows: &[NetReceiveFlow],
    network_index: &crate::GmodNetworkIndex,
    infer_index: &crate::GmodInferIndex,
    type_mismatch_enabled: bool,
    order_mismatch_enabled: bool,
) {
    for receive_flow in receive_flows {
        let receive_realm =
            infer_index.get_realm_at_offset(&file_id, receive_flow.receive_range.start());
        if !is_strict_realm(receive_realm) {
            continue;
        }

        let matching_send_flows =
            network_index.get_send_flows_for_message(&receive_flow.message_name);
        let mut has_matching_candidate = false;
        let mut best_mismatch: Option<CandidateMismatch> = None;

        for (send_file_id, send_flow) in matching_send_flows {
            if send_flow.is_wrapped {
                continue;
            }

            let sender_realm =
                infer_index.get_realm_at_offset(&send_file_id, send_flow.start_range.start());
            if !is_strict_realm(sender_realm) {
                continue;
            }

            let Some(expected_receive_realm) = expected_receiver_realm(send_flow.send_kind) else {
                continue;
            };

            if receive_realm != expected_receive_realm {
                continue;
            }

            if !is_opposite_strict_realm_pair(sender_realm, receive_realm) {
                continue;
            }

            has_matching_candidate = true;

            if is_perfect_read_write_match(send_flow, receive_flow) {
                best_mismatch = None;
                break;
            }

            let Some((code, range, message)) = first_mismatch_diagnostic(
                send_flow,
                receive_flow,
                type_mismatch_enabled,
                order_mismatch_enabled,
            ) else {
                continue;
            };

            let candidate = CandidateMismatch {
                code,
                range,
                message,
                common_prefix_len: matching_prefix_len(send_flow, receive_flow),
                write_count: send_flow.writes.len(),
            };

            let replace_best = match best_mismatch.as_ref() {
                Some(current_best) => {
                    candidate.common_prefix_len > current_best.common_prefix_len
                        || (candidate.common_prefix_len == current_best.common_prefix_len
                            && candidate.write_count > current_best.write_count)
                }
                None => true,
            };

            if replace_best {
                best_mismatch = Some(candidate);
            }
        }

        if !has_matching_candidate {
            continue;
        }

        if let Some(mismatch) = best_mismatch {
            context.add_diagnostic(mismatch.code, mismatch.range, mismatch.message, None);
        }
    }
}

struct CandidateMismatch {
    code: DiagnosticCode,
    range: rowan::TextRange,
    message: String,
    common_prefix_len: usize,
    write_count: usize,
}

fn is_perfect_read_write_match(send_flow: &NetSendFlow, receive_flow: &NetReceiveFlow) -> bool {
    send_flow.writes.len() == receive_flow.reads.len()
        && send_flow
            .writes
            .iter()
            .zip(receive_flow.reads.iter())
            .all(|(write, read)| write.kind.to_read_counterpart() == Some(read.kind))
}

fn matching_prefix_len(send_flow: &NetSendFlow, receive_flow: &NetReceiveFlow) -> usize {
    let mut matched = 0;
    let compared_len = send_flow.writes.len().min(receive_flow.reads.len());

    while matched < compared_len {
        if send_flow.writes[matched].kind.to_read_counterpart()
            != Some(receive_flow.reads[matched].kind)
        {
            break;
        }

        matched += 1;
    }

    matched
}

fn check_missing_send_counterpart(
    context: &mut DiagnosticContext,
    file_id: crate::FileId,
    send_flows: &[NetSendFlow],
    network_index: &crate::GmodNetworkIndex,
    infer_index: &crate::GmodInferIndex,
) {
    for send_flow in send_flows {
        let sender_realm = infer_index.get_realm_at_offset(&file_id, send_flow.start_range.start());
        if !is_strict_realm(sender_realm) {
            continue;
        }

        let Some(expected_realm) = expected_receiver_realm(send_flow.send_kind) else {
            continue;
        };

        let has_counterpart = network_index
            .get_receive_flows_for_message(&send_flow.message_name)
            .into_iter()
            .any(|_| true);
        if has_counterpart {
            continue;
        }

        context.add_diagnostic(
            DiagnosticCode::GmodNetMissingNetworkCounterpart,
            send_flow.start_range,
            t!(
                "No `net.Receive` counterpart found for `%{name}` in %{realm} realm.",
                name = send_flow.message_name,
                realm = realm_label(expected_realm),
            )
            .to_string(),
            None,
        );
    }
}

fn check_missing_receive_counterpart(
    context: &mut DiagnosticContext,
    file_id: crate::FileId,
    receive_flows: &[NetReceiveFlow],
    network_index: &crate::GmodNetworkIndex,
    infer_index: &crate::GmodInferIndex,
) {
    for receive_flow in receive_flows {
        let receive_realm =
            infer_index.get_realm_at_offset(&file_id, receive_flow.receive_range.start());
        if !is_strict_realm(receive_realm) {
            continue;
        }

        let Some(expected_sender_realm) = opposite_realm(receive_realm) else {
            continue;
        };

        let has_counterpart = network_index
            .get_send_flows_for_message(&receive_flow.message_name)
            .into_iter()
            .any(|_| true);

        if has_counterpart {
            continue;
        }

        context.add_diagnostic(
            DiagnosticCode::GmodNetMissingNetworkCounterpart,
            receive_flow.receive_range,
            t!(
                "No sending counterpart found for `%{name}` from %{realm} realm.",
                name = receive_flow.message_name,
                realm = realm_label(expected_sender_realm),
            )
            .to_string(),
            None,
        );
    }
}

fn first_mismatch_diagnostic(
    send_flow: &NetSendFlow,
    receive_flow: &NetReceiveFlow,
    type_mismatch_enabled: bool,
    order_mismatch_enabled: bool,
) -> Option<(DiagnosticCode, rowan::TextRange, String)> {
    if order_mismatch_enabled && send_flow.writes.len() != receive_flow.reads.len() {
        return Some((
            DiagnosticCode::GmodNetReadWriteOrderMismatch,
            receive_flow.receive_range,
            t!(
                "Read/write count mismatch for `%{name}`: writer has %{write_count} values, receiver reads %{read_count} values.",
                name = receive_flow.message_name,
                write_count = send_flow.writes.len(),
                read_count = receive_flow.reads.len(),
            )
            .to_string(),
        ));
    }

    let compared_len = send_flow.writes.len().min(receive_flow.reads.len());
    for index in 0..compared_len {
        let Some(expected_read_kind) = send_flow.writes[index].kind.to_read_counterpart() else {
            continue;
        };

        let actual_read_kind = receive_flow.reads[index].kind;
        if expected_read_kind == actual_read_kind {
            continue;
        }

        if order_mismatch_enabled && is_mispositioned_read(send_flow, index, actual_read_kind) {
            return Some((
                DiagnosticCode::GmodNetReadWriteOrderMismatch,
                receive_flow.receive_range,
                t!(
                    "Read/write order mismatch for `%{name}` at position %{position}: expected `%{expected}`, got `%{actual}`.",
                    name = receive_flow.message_name,
                    position = index + 1,
                    expected = expected_read_kind.to_fn_name(),
                    actual = actual_read_kind.to_fn_name(),
                )
                .to_string(),
            ));
        }

        if type_mismatch_enabled {
            return Some((
                DiagnosticCode::GmodNetReadWriteTypeMismatch,
                receive_flow.receive_range,
                t!(
                    "Read/write type mismatch for `%{name}` at position %{position}: expected `%{expected}`, got `%{actual}`.",
                    name = receive_flow.message_name,
                    position = index + 1,
                    expected = expected_read_kind.to_fn_name(),
                    actual = actual_read_kind.to_fn_name(),
                )
                .to_string(),
            ));
        }
    }

    None
}

fn is_mispositioned_read(
    send_flow: &NetSendFlow,
    current_index: usize,
    actual_read_kind: NetOpKind,
) -> bool {
    send_flow.writes.iter().enumerate().any(|(index, write)| {
        index != current_index && write.kind.to_read_counterpart() == Some(actual_read_kind)
    })
}

fn expected_receiver_realm(send_kind: NetSendKind) -> Option<GmodRealm> {
    match send_kind {
        NetSendKind::Send | NetSendKind::Broadcast => Some(GmodRealm::Client),
        NetSendKind::SendToServer => Some(GmodRealm::Server),
    }
}

fn opposite_realm(realm: GmodRealm) -> Option<GmodRealm> {
    match realm {
        GmodRealm::Client => Some(GmodRealm::Server),
        GmodRealm::Server => Some(GmodRealm::Client),
        GmodRealm::Shared | GmodRealm::Unknown => None,
    }
}

fn is_strict_realm(realm: GmodRealm) -> bool {
    matches!(realm, GmodRealm::Client | GmodRealm::Server)
}

fn is_opposite_strict_realm_pair(sender_realm: GmodRealm, receiver_realm: GmodRealm) -> bool {
    matches!(
        (sender_realm, receiver_realm),
        (GmodRealm::Server, GmodRealm::Client) | (GmodRealm::Client, GmodRealm::Server)
    )
}

fn realm_label(realm: GmodRealm) -> &'static str {
    match realm {
        GmodRealm::Client => "client",
        GmodRealm::Server => "server",
        GmodRealm::Shared => "shared",
        GmodRealm::Unknown => "unknown",
    }
}
