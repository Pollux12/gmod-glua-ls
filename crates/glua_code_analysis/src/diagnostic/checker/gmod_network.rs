use crate::{
    DiagnosticCode, GmodRealm, NetOpKind, NetReceiveFlow, NetSendFlow,
    SemanticModel, expected_receiver_realm, flows_can_match, is_opposite_strict_realm_pair,
    is_strict_realm,
};

use super::{Checker, DiagnosticContext};

pub struct GmodNetworkChecker;

impl Checker for GmodNetworkChecker {
    const CODES: &[DiagnosticCode] = &[
        DiagnosticCode::GmodNetReadWriteTypeMismatch,
        DiagnosticCode::GmodNetReadWriteOrderMismatch,
        DiagnosticCode::GmodNetMissingNetworkCounterpart,
        DiagnosticCode::GmodNetReadWriteBitsMismatch,
    ];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let emmyrc = semantic_model.get_emmyrc();
        if !emmyrc.gmod.enabled || !emmyrc.gmod.network.enabled {
            return;
        }

        let file_id = semantic_model.get_file_id();
        let db = semantic_model.get_db();
        let network_index = db.get_gmod_network_index();
        let infer_index = db.get_gmod_infer_index();
        let Some(file_data) = network_index.get_file_data(file_id) else {
            return;
        };

        check_read_write_mismatch(
            context,
            file_id,
            file_data.receive_flows.as_slice(),
            network_index,
            infer_index,
        );

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

        check_bits_mismatch(
            context,
            file_id,
            file_data.receive_flows.as_slice(),
            network_index,
            infer_index,
        );
    }
}

fn check_read_write_mismatch(
    context: &mut DiagnosticContext,
    file_id: crate::FileId,
    receive_flows: &[NetReceiveFlow],
    network_index: &crate::GmodNetworkIndex,
    infer_index: &crate::GmodInferIndex,
) {
    for receive_flow in receive_flows {
        if receive_flow.reads_opaque {
            // Callback body could not be inspected — counts/types unknown.
            continue;
        }

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

            if flows_can_match(send_flow, receive_flow) {
                best_mismatch = None;
                break;
            }

            let (code, range, message) = first_mismatch_diagnostic(send_flow, receive_flow)
                .unwrap_or_else(|| {
                    (
                        DiagnosticCode::GmodNetReadWriteOrderMismatch,
                        receive_flow.receive_range,
                        t!(
                            "Read/write structure mismatch for `%{name}`: writer and receiver flows cannot be aligned.",
                            name = receive_flow.message_name,
                        )
                        .to_string(),
                    )
                });

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
) -> Option<(DiagnosticCode, rowan::TextRange, String)> {
    let has_dynamic = send_flow.writes.iter().any(|w| w.dynamic)
        || receive_flow.reads.iter().any(|r| r.dynamic);

    // Count mismatch is meaningless when either side contains dynamic ops
    // (their effective count is variable). The DP-based matcher already
    // rejected this pair, so the failure is a real structural mismatch — but
    // emitting a precise position is unreliable, so we fall through to the
    // per-position type/order checks below and skip the raw count message.
    if !has_dynamic
        && send_flow.writes.len() != receive_flow.reads.len()
    {
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

        if is_mispositioned_read(send_flow, index, actual_read_kind) {
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

fn opposite_realm(realm: GmodRealm) -> Option<GmodRealm> {
    match realm {
        GmodRealm::Client => Some(GmodRealm::Server),
        GmodRealm::Server => Some(GmodRealm::Client),
        GmodRealm::Shared | GmodRealm::Unknown => None,
    }
}

fn realm_label(realm: GmodRealm) -> &'static str {
    match realm {
        GmodRealm::Client => "client",
        GmodRealm::Server => "server",
        GmodRealm::Shared => "shared",
        GmodRealm::Unknown => "unknown",
    }
}

/// Bit-width mismatch checker. Only fires when:
///   - Both writer and reader use *literal* bit-width arguments (anything
///     non-literal is unknowable at index time, so we silently skip — emitting
///     would produce false positives for projects that use named constants
///     or runtime values).
///   - The matched send/receive pair would otherwise be valid (kinds line up
///     via `flows_can_match`). If types/order are already broken, the
///     dedicated checkers report that and a bits diagnostic would be noise.
///   - The literal widths actually differ at the same position, where the op
///     pair has matching kinds AND neither side is dynamic at that index.
///
/// We dedupe by (position, expected, actual) so multiple senders that all
/// disagree the same way only produce one warning.
fn check_bits_mismatch(
    context: &mut DiagnosticContext,
    file_id: crate::FileId,
    receive_flows: &[NetReceiveFlow],
    network_index: &crate::GmodNetworkIndex,
    infer_index: &crate::GmodInferIndex,
) {
    use std::collections::HashSet;

    for receive_flow in receive_flows {
        if receive_flow.reads_opaque {
            continue;
        }

        let receive_realm =
            infer_index.get_realm_at_offset(&file_id, receive_flow.receive_range.start());
        if !is_strict_realm(receive_realm) {
            continue;
        }

        let matching_send_flows =
            network_index.get_send_flows_for_message(&receive_flow.message_name);

        let mut reported: HashSet<(usize, u32, u32)> = HashSet::new();

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

            // Only compare bits when the broader flow shape matches; otherwise
            // type/order diagnostics own this pair.
            if !flows_can_match(send_flow, receive_flow) {
                continue;
            }

            let compared_len = send_flow.writes.len().min(receive_flow.reads.len());
            for index in 0..compared_len {
                let write = &send_flow.writes[index];
                let read = &receive_flow.reads[index];

                // Skip dynamic ops — their position alignment is not guaranteed.
                if write.dynamic || read.dynamic {
                    continue;
                }

                if write.kind.to_read_counterpart() != Some(read.kind) {
                    continue;
                }

                let (Some(expected_bits), Some(actual_bits)) = (write.bits, read.bits) else {
                    continue;
                };

                if expected_bits == actual_bits {
                    continue;
                }

                if !reported.insert((index, expected_bits, actual_bits)) {
                    continue;
                }

                context.add_diagnostic(
                    DiagnosticCode::GmodNetReadWriteBitsMismatch,
                    receive_flow.receive_range,
                    t!(
                        "Bit-width mismatch for `%{name}` at position %{position}: writer uses `%{op}(%{expected})`, reader uses `%{rop}(%{actual})`.",
                        name = receive_flow.message_name,
                        position = index + 1,
                        op = write.kind.to_fn_name(),
                        rop = read.kind.to_fn_name(),
                        expected = expected_bits,
                        actual = actual_bits,
                    )
                    .to_string(),
                    None,
                );
            }
        }
    }
}
