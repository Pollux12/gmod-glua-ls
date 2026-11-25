use std::cell::RefCell;

use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaCallArgList, LuaCallExpr, LuaClosureExpr, LuaFuncStat, LuaKind,
    LuaLocalFuncStat, LuaLocalStat, LuaNameExpr, LuaSyntaxId, LuaSyntaxKind, LuaVarExpr,
};
use rowan::TextRange;
use rustc_hash::FxHashSet;

use crate::{
    DbIndex, FileId, GmodRealm, LuaDeclId, LuaInferCache, LuaMemberKey, LuaType, NetOpEntry,
    TypeOps, check_type_compact, pair_senders_for_receive, snapshot_table_type,
    type_is_net_read_op, widen_literal_type_for_assignment,
};

use super::super::{infer_expr, with_file_cache};

thread_local! {
    /// The reads being typed on this thread: a sender that writes what a read
    /// it pairs with received reaches that read again.
    static TYPING: RefCell<FxHashSet<(FileId, TextRange)>> = RefCell::default();
}

/// What the call `call_expr` of `callee`, when that is a net read annotated
/// to return `declared`, receives: the values every sender it pairs with
/// writes at its place in the message, when each sender's place is fixed, its
/// write pairs with the read, and the value fits `declared`. `None` keeps
/// `declared`.
pub(super) fn infer_net_read_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
    callee: &LuaType,
    declared: &LuaType,
) -> Option<LuaType> {
    // A primitive is all a read of one can say, and a server cannot rely on
    // what a client sends: any client can send any message.
    let file_id = cache.get_file_id();
    if declared.is_string()
        || declared.is_number()
        || declared.is_boolean()
        || !type_is_net_read_op(db, callee)
        || db
            .get_gmod_infer_index()
            .get_realm_at_offset(&file_id, call_expr.get_position())
            != GmodRealm::Client
    {
        return None;
    }
    let read = (file_id, call_expr.get_range());
    if !TYPING.with(|typing| typing.borrow_mut().insert(read)) {
        return None;
    }
    let received = received_type(db, cache, call_expr, read, declared);
    TYPING.with(|typing| typing.borrow_mut().remove(&read));
    received
}

fn received_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
    (file_id, range): (FileId, TextRange),
    declared: &LuaType,
) -> Option<LuaType> {
    let network = db.get_gmod_network_index();
    // A helper several receivers call, or one calls more than once, reads at
    // each of those places, so it returns whatever any of them is sent.
    let mut received: Option<LuaType> = None;
    let mut matching_flows = FxHashSet::default();
    for (receive_file_id, flow) in network.receive_flows_reading(file_id) {
        for (place, read) in flow.reads.iter().enumerate() {
            if read.file_id != file_id || read.range != range {
                continue;
            }
            if flow.reads_opaque || !fixed_up_to(&flow.reads, place) {
                return None;
            }
            let senders =
                pair_senders_for_receive(network, db.get_gmod_infer_index(), receive_file_id, flow);
            // A sender outside the workspace may send what this one receives.
            if senders.is_empty() {
                return None;
            }
            // If any potentially client-reaching sender (wrapped helper,
            // shared/unknown-realm sender, or a mismatched-payload sender) was
            // filtered out by `pair_senders_for_receive`, we cannot safely
            // narrow to only the paired subset.
            let has_unpaired_reaching_sender = network
                .get_send_flows_for_message(&flow.message_name)
                .into_iter()
                .any(|(send_file_id, send_flow)| {
                    if send_flow.send_kind.receiver_realm == GmodRealm::Server {
                        return false;
                    }
                    let could_reach_client = send_flow.is_wrapped
                        || db
                            .get_gmod_infer_index()
                            .get_realm_at_offset(&send_file_id, send_flow.start_range.start())
                            != GmodRealm::Client;
                    could_reach_client
                        && !senders.iter().any(|(paired_file_id, paired_flow)| {
                            *paired_file_id == send_file_id
                                && paired_flow.start_range == send_flow.start_range
                        })
                });
            if has_unpaired_reaching_sender {
                return None;
            }
            matching_flows.insert((receive_file_id, flow.receive_range));
            for (_, sender) in senders {
                if !sender.payload_known() || !fixed_up_to(&sender.writes, place) {
                    return None;
                }
                let write = sender.writes.get(place)?;
                if !write.op.pairs_with(&read.op) {
                    return None;
                }
                let written = written_type(db, cache, write, declared)?;
                if check_type_compact(db, declared, &written).is_err() {
                    return None;
                }
                received = Some(match received {
                    Some(received) => TypeOps::Union.apply(db, &received, &written),
                    None => written,
                });
            }
        }
    }
    let received = received?;
    // A read inside a helper can also be reached from a receiver whose
    // callback the network index did not resolve (such as a cross-file named
    // callback, a handler table, or a dynamic registration). Keep `declared`
    // unless every path into the enclosing function comes from `matching_flows`.
    let enclosing = call_expr
        .syntax()
        .ancestors()
        .find_map(LuaClosureExpr::cast)?;
    let mut visited = FxHashSet::default();
    if !all_callers_covered(db, file_id, &enclosing, &matching_flows, &mut visited) {
        return None;
    }
    Some(received)
}

/// Whether every call to `closure` (defined in `file_id`) is accounted for by
/// one of `matching_flows`.
fn all_callers_covered(
    db: &DbIndex,
    file_id: FileId,
    closure: &LuaClosureExpr,
    matching_flows: &FxHashSet<(FileId, TextRange)>,
    visited: &mut FxHashSet<(FileId, TextRange)>,
) -> bool {
    // Fast path: inline callback passed directly to a matching receive call.
    if let Some(arg_list) = closure.get_parent::<LuaCallArgList>()
        && let Some(owner_call) = arg_list.get_parent::<LuaCallExpr>()
        && matching_flows.contains(&(file_id, owner_call.get_range()))
    {
        return true;
    }
    if !visited.insert((file_id, closure.get_range())) {
        return true;
    }

    let Some(parent) = closure.syntax().parent() else {
        return false;
    };

    if let Some(local_func) = LuaLocalFuncStat::cast(parent.clone()) {
        let Some(local_name) = local_func.get_local_name() else {
            return false;
        };
        let decl_id = LuaDeclId::new(file_id, local_name.get_position());
        return local_decl_callers_covered(db, file_id, decl_id, matching_flows, visited);
    }

    if let Some(local_stat) = LuaLocalStat::cast(parent.clone()) {
        let Some(pos) = local_stat
            .get_value_exprs()
            .position(|expr| expr.syntax() == closure.syntax())
        else {
            return false;
        };
        let Some(local_name) = local_stat.get_local_name_list().nth(pos) else {
            return false;
        };
        let decl_id = LuaDeclId::new(file_id, local_name.get_position());
        return local_decl_callers_covered(db, file_id, decl_id, matching_flows, visited);
    }

    if let Some(func_stat) = LuaFuncStat::cast(parent) {
        let Some(func_name) = func_stat.get_func_name() else {
            return false;
        };
        return match func_name {
            LuaVarExpr::NameExpr(name_expr) => {
                if db
                    .get_reference_index()
                    .get_var_reference_decl(&file_id, name_expr.get_range())
                    .and_then(|decl_id| db.get_decl_index().get_decl(&decl_id))
                    .is_some_and(|decl| decl.is_local())
                {
                    return false;
                }
                let Some(name) = name_expr.get_name_text() else {
                    return false;
                };
                global_or_member_callers_covered(
                    db,
                    file_id,
                    name_expr.get_range(),
                    &name,
                    true,
                    matching_flows,
                    visited,
                )
            }
            LuaVarExpr::IndexExpr(index_expr) => {
                let Some(name_token) = index_expr.get_name_token() else {
                    return false;
                };
                let is_global_table = matches!(
                    index_expr.get_prefix_expr(),
                    Some(glua_parser::LuaExpr::NameExpr(name))
                        if name.get_name_text().as_deref() == Some("_G")
                );
                global_or_member_callers_covered(
                    db,
                    file_id,
                    index_expr.get_range(),
                    name_token.get_name_text(),
                    is_global_table,
                    matching_flows,
                    visited,
                )
            }
        };
    }

    false
}

fn local_decl_callers_covered(
    db: &DbIndex,
    file_id: FileId,
    decl_id: LuaDeclId,
    matching_flows: &FxHashSet<(FileId, TextRange)>,
    visited: &mut FxHashSet<(FileId, TextRange)>,
) -> bool {
    let Some(decl_refs) = db
        .get_reference_index()
        .get_local_reference(&file_id)
        .and_then(|file_refs| file_refs.get_decl_references(&decl_id))
    else {
        return false;
    };
    if decl_refs.is_written_after_declaration(decl_id.position) {
        return false;
    }
    let mut read_ranges: Vec<TextRange> = decl_refs
        .cells
        .iter()
        .filter(|cell| !cell.is_write)
        .map(|cell| cell.range)
        .collect();
    read_ranges.sort_unstable_by_key(|range| (range.start(), range.end()));
    if read_ranges.is_empty() {
        return false;
    }
    for range in read_ranges {
        let Some(name_expr) = db
            .get_vfs()
            .syntax_tree_around(&file_id, range)
            .and_then(|tree| {
                tree.get_red_root()
                    .covering_element(range)
                    .ancestors()
                    .find_map(LuaNameExpr::cast)
            })
            .filter(|name_expr| name_expr.get_range() == range)
        else {
            return false;
        };
        if !reference_expr_covered(db, file_id, name_expr.syntax(), matching_flows, visited) {
            return false;
        }
    }
    true
}

fn global_or_member_callers_covered(
    db: &DbIndex,
    def_file_id: FileId,
    def_range: TextRange,
    name: &str,
    include_globals: bool,
    matching_flows: &FxHashSet<(FileId, TextRange)>,
    visited: &mut FxHashSet<(FileId, TextRange)>,
) -> bool {
    let refs = db.get_reference_index();
    let mut sites = Vec::new();
    if include_globals && let Some(global_sites) = refs.get_global_references(name) {
        sites.extend(global_sites);
    }
    let member_key = LuaMemberKey::Name(smol_str::SmolStr::new(name));
    if let Some(index_sites) = refs.get_index_references(&member_key) {
        sites.extend(index_sites);
    }
    sites.sort_unstable_by_key(|site| {
        (
            site.file_id.id,
            site.value.get_range().start(),
            site.value.get_range().end(),
        )
    });
    sites.dedup();

    let mut saw_caller = false;
    for site in sites {
        if site.file_id == def_file_id && site.value.get_range() == def_range {
            continue;
        }
        let Some(node) = db.get_vfs().syntax_node_around(&site.file_id, site.value) else {
            return false;
        };
        if !reference_expr_covered(db, site.file_id, &node, matching_flows, visited) {
            return false;
        }
        saw_caller = true;
    }
    saw_caller
}

/// Whether the reference expression `node` in `file_id` is either a direct call
/// inside a covered closure or passed directly as an argument to a matching
/// receive call.
fn reference_expr_covered(
    db: &DbIndex,
    file_id: FileId,
    node: &glua_parser::LuaSyntaxNode,
    matching_flows: &FxHashSet<(FileId, TextRange)>,
    visited: &mut FxHashSet<(FileId, TextRange)>,
) -> bool {
    // `M.ReadEntry` definitions or reassignments are not covered callers.
    if node.parent().and_then(LuaFuncStat::cast).is_some()
        || node
            .parent()
            .and_then(LuaAssignStat::cast)
            .is_some_and(|assign| {
                assign
                    .get_var_and_expr_list()
                    .0
                    .iter()
                    .any(|var| var.syntax() == node)
            })
    {
        return false;
    }

    let Some(parent) = node.parent() else {
        return false;
    };

    if let Some(call_expr) = LuaCallExpr::cast(parent.clone())
        && call_expr
            .get_prefix_expr()
            .is_some_and(|prefix| prefix.syntax() == node)
    {
        let Some(enclosing) = call_expr
            .syntax()
            .ancestors()
            .find_map(LuaClosureExpr::cast)
        else {
            return false;
        };
        return all_callers_covered(db, file_id, &enclosing, matching_flows, visited);
    }

    if let Some(arg_list) = LuaCallArgList::cast(parent)
        && let Some(owner_call) = arg_list.get_parent::<LuaCallExpr>()
        && matching_flows.contains(&(file_id, owner_call.get_range()))
    {
        return true;
    }

    false
}

/// Whether every op up to `place` occurs once in each message: a branch or
/// loop before it moves what follows to a place each message decides.
fn fixed_up_to(entries: &[NetOpEntry], place: usize) -> bool {
    entries
        .get(..=place)
        .is_some_and(|entries| entries.iter().all(|entry| !entry.dynamic))
}

/// The type of the value `write` sends, inferred in its file, as a read
/// annotated to return `declared` receives it.
fn written_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    write: &NetOpEntry,
    declared: &LuaType,
) -> Option<LuaType> {
    let syntax_id = LuaSyntaxId::new(LuaKind::Syntax(LuaSyntaxKind::CallExpr), write.range);
    let call = LuaCallExpr::cast(db.get_vfs().syntax_node_around(&write.file_id, syntax_id)?)?;
    let value = call.get_args_list()?.get_args().next()?;
    let typ = with_file_cache(cache, write.file_id, |cache| {
        infer_expr(db, cache, value).ok()
    })?;
    if typ.is_unknown() || typ.is_any() || typ.is_never() {
        return None;
    }
    // A read whose annotation rules out nil never returns it, whatever was
    // written.
    let typ = if declared.is_optional() {
        typ
    } else {
        TypeOps::Remove.apply(db, &typ, &LuaType::Nil)
    };
    // The receiver holds a copy: a literal's fields, not the sender's table,
    // whose fields only the sender's realm sees.
    Some(snapshot_table_type(db, &typ).unwrap_or_else(|| widen_literal_type_for_assignment(&typ)))
}
