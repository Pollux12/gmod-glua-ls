use std::cell::RefCell;

use glua_parser::{LuaAstNode, LuaCallExpr, LuaKind, LuaSyntaxId, LuaSyntaxKind};
use rowan::TextRange;
use rustc_hash::FxHashSet;

use crate::{
    DbIndex, FileId, GmodRealm, LuaInferCache, LuaType, NetOpEntry, TypeOps, check_type_compact,
    pair_senders_for_receive, snapshot_table_type, type_is_net_read_op,
    widen_literal_type_for_assignment,
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
    let received = received_type(db, cache, read, declared);
    TYPING.with(|typing| typing.borrow_mut().remove(&read));
    received
}

fn received_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    (file_id, range): (FileId, TextRange),
    declared: &LuaType,
) -> Option<LuaType> {
    let network = db.get_gmod_network_index();
    let (flow, place) = network
        .get_file_data(file_id)?
        .receive_flows
        .iter()
        .find_map(|flow| {
            let place = flow
                .reads
                .iter()
                .position(|entry| entry.file_id == file_id && entry.range == range)?;
            Some((flow, place))
        })?;
    if flow.reads_opaque || !fixed_up_to(&flow.reads, place) {
        return None;
    }
    let read = &flow.reads[place];
    let senders = pair_senders_for_receive(network, db.get_gmod_infer_index(), file_id, flow);
    let mut received: Option<LuaType> = None;
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
    received
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
