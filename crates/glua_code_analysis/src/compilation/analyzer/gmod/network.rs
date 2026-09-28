//! Net message flow collection.
//!
//! Each file's [`FileNetHelpers`] fact holds what inference decides about its
//! calls (their net role and the function body they expand into) and a
//! syntax-free summary of each of its function bodies. A file's flows walk its
//! own syntax against its own fact and expand helper calls by composing the
//! helpers' summaries, so a helper body is analysed once per change of its
//! file rather than once per evaluation of every file that calls it.
//!
//! Summaries only describe their own body: a nested helper is named, never
//! inlined. The composition (bindings, consensus, cycles) runs in the caller's
//! evaluation, so no summary reads another and derivations cannot cycle.

use std::sync::Arc;

use super::*;

/// A function body: its owning file and the body block's range.
type HelperId = InFiled<TextRange>;

/// [`NetOpItem`]-composition `low` value for an answer that assumed nothing
/// about a still-open helper.
const NO_CYCLE: usize = usize::MAX;

/// What a call does in the net subsystem, resolved purely from the callee's
/// signature metadata. Because resolution goes through the type layer, aliases
/// (`local netStart = net.Start`), cross-file globals, and annotated replacement
/// APIs are all recognized identically to the builtins. Ordinary wrappers are
/// expanded through their bodies and need no annotations of their own.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum NetCallRole {
    /// Begins a message: `call_arg("gmod.net_message", "start")`. Carries the
    /// index of the parameter holding the message name, so a wrapper that takes
    /// it somewhere other than first is read correctly.
    Start { message_idx: usize },
    /// Registers a receiver: `call_arg("gmod.net_message", "receive")`, with the
    /// message-name index and the `callback` role's index when annotated.
    Receive {
        message_idx: usize,
        callback_idx: Option<usize>,
    },
    /// Terminates and sends a message: the `net_send` attribute.
    Send(NetSendKind),
    /// Writes or reads a payload value: the `net_payload` attribute.
    Payload(NetOpDescriptor),
}

/// A string argument as far as it is static: literals and names,
/// concatenated. A name is a parameter that a caller's literal binds.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StringTemplate(Vec<TemplatePart>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TemplatePart {
    Literal(String),
    Name(String),
}

impl StringTemplate {
    fn of(expr: &LuaExpr) -> Option<Self> {
        let mut parts = Vec::new();
        push_template_parts(expr, &mut parts)?;
        Some(Self(parts))
    }

    /// The names the template reads.
    fn names(&self) -> impl Iterator<Item = &str> {
        self.0.iter().filter_map(|part| match part {
            TemplatePart::Name(name) => Some(name.as_str()),
            TemplatePart::Literal(_) => None,
        })
    }

    fn resolve(&self, bindings: &HashMap<String, String>) -> Option<String> {
        let mut value = String::new();
        for part in &self.0 {
            match part {
                TemplatePart::Literal(text) => value.push_str(text),
                TemplatePart::Name(name) => value.push_str(bindings.get(name)?),
            }
        }
        Some(value)
    }
}

fn push_template_parts(expr: &LuaExpr, parts: &mut Vec<TemplatePart>) -> Option<()> {
    match expr {
        LuaExpr::LiteralExpr(_) => parts.push(TemplatePart::Literal(static_literal_string(expr)?)),
        LuaExpr::NameExpr(name_expr) => {
            parts.push(TemplatePart::Name(name_expr.get_name_text()?.to_string()))
        }
        LuaExpr::ParenExpr(paren_expr) => push_template_parts(&paren_expr.get_expr()?, parts)?,
        LuaExpr::BinaryExpr(binary_expr) => {
            if binary_expr.get_op_token()?.get_op() != BinaryOperator::OpConcat {
                return None;
            }
            let (left, right) = binary_expr.get_exprs()?;
            push_template_parts(&left, parts)?;
            push_template_parts(&right, parts)?;
        }
        _ => return None,
    }
    Some(())
}

fn arg_templates(call_expr: &LuaCallExpr) -> Vec<Option<StringTemplate>> {
    call_expr
        .get_args_list()
        .map(|args| {
            args.get_args()
                .map(|arg| StringTemplate::of(&arg))
                .collect()
        })
        .unwrap_or_default()
}

fn resolve_arg(
    args: &[Option<StringTemplate>],
    idx: usize,
    bindings: &HashMap<String, String>,
) -> Option<String> {
    args.get(idx)?.as_ref()?.resolve(bindings)
}

/// The helper's parameters bound to the call's arguments that resolve to a
/// string under the caller's own bindings.
fn helper_bindings(
    params: &[Option<String>],
    args: &[Option<StringTemplate>],
    caller_bindings: &HashMap<String, String>,
) -> HashMap<String, String> {
    params
        .iter()
        .zip(args)
        .filter_map(|(param, arg)| Some((param.clone()?, arg.as_ref()?.resolve(caller_bindings)?)))
        .collect()
}

/// Bindings in a stable order, for memo keys.
fn sorted_bindings(bindings: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut key: Vec<(String, String)> = bindings
        .iter()
        .map(|(param, value)| (param.clone(), value.clone()))
        .collect();
    key.sort();
    key
}

/// What inference decides about one call.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NetCallFact {
    role: Option<NetCallRole>,
    /// The function body the call expands into.
    target: Option<HelperId>,
    /// The payload op's bit-width parameter, as a call argument index.
    bits_param_idx: Option<usize>,
}

/// One step of a payload walk, in evaluation order: an op, or a call into a
/// helper whose own payload walk continues there.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum NetOpItem {
    /// `dynamic` and `flow_path` are relative to the walked block.
    Op(NetOpEntry),
    Helper {
        target: HelperId,
        dynamic: bool,
        flow_path: Vec<NetFlowFrame>,
    },
}

/// A receive callback's reads: the payload walk of its body when found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
struct NetReceiveReads {
    items: Option<Vec<NetOpItem>>,
    opaque: bool,
}

/// One call inside a helper body.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct HelperCall {
    /// Inside a closure nested in the helper: only reachability sees it.
    nested: bool,
    role: Option<NetCallRole>,
    target: Option<HelperId>,
    args: Vec<Option<StringTemplate>>,
    /// A send's recipient snippet.
    send_target: Option<String>,
    /// A receive with a literal message name, already indexed where it is.
    literal_receive: bool,
    receive_reads: NetReceiveReads,
}

/// A `net.Start` .. send transaction written directly in a helper body.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HelperSend {
    start_range: TextRange,
    message: StringTemplate,
    /// The payload walk of each statement between the start and its end.
    stats: Vec<Vec<NetOpItem>>,
    send: Option<(NetSendKind, Option<String>)>,
}

impl Hash for HelperSend {
    fn hash<H: Hasher>(&self, state: &mut H) {
        crate::db_index::derivation::hash_local(&self.start_range, state);
        self.message.hash(state);
        self.stats.hash(state);
        self.send.hash(state);
    }
}

/// What a function body contributes to net flows, without its callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
struct NetHelper {
    /// Each parameter, by position, named when the body's message or argument
    /// templates read it: a parameter nothing reads binds nothing.
    params: Vec<Option<String>>,
    /// Every call in the body, in document order.
    calls: Vec<HelperCall>,
    /// The payload walk of the whole body.
    ops: Vec<NetOpItem>,
    sends: Vec<HelperSend>,
}

/// One file's net call facts and function-body summaries.
#[derive(Debug, Default)]
pub(crate) struct FileNetHelpers {
    calls: HashMap<TextRange, NetCallFact>,
    helpers: HashMap<TextRange, Arc<NetHelper>>,
}

impl FileNetHelpers {
    fn call(&self, call_expr: &LuaCallExpr) -> Option<&NetCallFact> {
        self.calls.get(&call_expr.get_range())
    }
}

impl Hash for FileNetHelpers {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let mut calls = self.calls.iter().collect::<Vec<_>>();
        calls.sort_by_key(|(range, _)| (range.start(), range.end()));
        calls.len().hash(state);
        for (range, fact) in calls {
            crate::db_index::derivation::hash_local(range, state);
            fact.hash(state);
        }
        let mut helpers = self.helpers.iter().collect::<Vec<_>>();
        helpers.sort_by_key(|(range, _)| (range.start(), range.end()));
        helpers.len().hash(state);
        for (range, helper) in helpers {
            crate::db_index::derivation::hash_local(range, state);
            helper.hash(state);
        }
    }
}

impl NetOpItem {
    fn visit_target(&mut self, visit: &mut impl FnMut(&mut HelperId)) {
        if let NetOpItem::Helper { target, .. } = self {
            visit(target);
        }
    }
}

impl NetHelper {
    /// Visits every helper the summary calls into.
    fn visit_targets(&mut self, visit: &mut impl FnMut(&mut HelperId)) {
        for call in &mut self.calls {
            if let Some(target) = &mut call.target {
                visit(target);
            }
            for item in call.receive_reads.items.iter_mut().flatten() {
                item.visit_target(visit);
            }
        }
        for item in &mut self.ops {
            item.visit_target(visit);
        }
        for item in self
            .sends
            .iter_mut()
            .flat_map(|send| send.stats.iter_mut().flatten())
        {
            item.visit_target(visit);
        }
    }
}

impl crate::db_index::derivation::DerivedFact for FileNetHelpers {
    fn fingerprint(&self) -> crate::db_index::derivation::Fingerprint {
        crate::db_index::derivation::fingerprint_of(self)
    }

    fn part_ranges(&self) -> Vec<TextRange> {
        self.helpers.keys().copied().collect()
    }

    fn part_fingerprint(&self, range: TextRange) -> crate::db_index::derivation::Fingerprint {
        crate::db_index::derivation::fingerprint_opt(
            self.helpers.get(&range).map(|helper| &**helper),
        )
    }

    /// Every helper a summary calls into is some call's target.
    fn named_files(&self, own: FileId) -> rustc_hash::FxHashSet<FileId> {
        self.calls
            .values()
            .filter_map(|fact| fact.target.as_ref())
            .map(|target| target.file_id)
            .filter(|file_id| *file_id != own)
            .collect()
    }

    fn remap(
        &self,
        remap: &crate::FileRemap,
    ) -> crate::Remap<Arc<dyn crate::db_index::derivation::DerivedFact>> {
        let mut moves = HashMap::default();
        for target in self.calls.values().filter_map(|fact| fact.target.as_ref()) {
            let block = LuaSyntaxId::new(
                glua_parser::LuaKind::Syntax(glua_parser::LuaSyntaxKind::Block),
                target.value,
            );
            match remap.syntax_id(target.file_id, block) {
                crate::Remap::Moved(block) if block.get_range() != target.value => {
                    moves.insert(
                        target.clone(),
                        InFiled::new(target.file_id, block.get_range()),
                    );
                }
                crate::Remap::Lost => return crate::Remap::Lost,
                crate::Remap::Moved(_) | crate::Remap::Unrelated => {}
            }
        }
        if moves.is_empty() {
            return crate::Remap::Unrelated;
        }
        let mut move_target = |target: &mut HelperId| {
            if let Some(moved) = moves.get(target) {
                *target = moved.clone();
            }
        };
        let mut calls = self.calls.clone();
        for target in calls.values_mut().filter_map(|fact| fact.target.as_mut()) {
            move_target(target);
        }
        let helpers = self
            .helpers
            .iter()
            .map(|(range, helper)| {
                let mut moved = NetHelper::clone(helper);
                moved.visit_targets(&mut move_target);
                let helper = if moved == **helper {
                    helper.clone()
                } else {
                    Arc::new(moved)
                };
                (*range, helper)
            })
            .collect();
        crate::Remap::Moved(Arc::new(FileNetHelpers { calls, helpers }))
    }
}

/// One file's net call facts and function-body summaries.
pub(crate) fn evaluate_net_helpers(db: &mut DbIndex, file_id: FileId) {
    let helpers = compute_net_helpers(db, file_id);
    apply_net_helpers(db, file_id, helpers);
}

/// A file's net helpers, read off the index; [`apply_net_helpers`] stores them.
pub(crate) fn compute_net_helpers(db: &DbIndex, file_id: FileId) -> Option<FileNetHelpers> {
    if !db.get_emmyrc().gmod.enabled {
        return None;
    }
    let roles = derive::call_role_entries(db);
    build_file_net_helpers(db, file_id, &roles.registry)
}

pub(crate) fn apply_net_helpers(
    db: &mut DbIndex,
    file_id: FileId,
    helpers: Option<FileNetHelpers>,
) {
    if let Some(helpers) = helpers {
        db.set_net_helpers(file_id, Arc::new(helpers));
    }
}

fn build_file_net_helpers(
    db: &DbIndex,
    file_id: FileId,
    helper_registry: &HelperRegistry,
) -> Option<FileNetHelpers> {
    let root = db.get_vfs().get_syntax_tree(&file_id)?.get_chunk_node();
    let functions = FileFunctionMap::build(&root);
    let mut net = NetCallResolver::new(db, file_id);
    let mut calls = HashMap::default();
    for call_expr in root.descendants::<LuaCallExpr>() {
        let role = net.role(db, &call_expr);
        let target = resolve_call_to_function_block(
            file_id,
            &call_expr,
            helper_registry,
            &functions,
            &mut net,
            db,
        );
        let bits_param_idx = match role {
            Some(NetCallRole::Payload(_)) => net.bits_param_idx(db, &call_expr),
            _ => None,
        };
        calls.insert(
            call_expr.get_range(),
            NetCallFact {
                role,
                target,
                bits_param_idx,
            },
        );
    }
    if calls.is_empty() {
        return None;
    }
    let mut file = FileNetHelpers {
        calls,
        helpers: HashMap::default(),
    };
    let site = NetFileSite {
        file_id,
        functions: &functions,
        calls: &file,
    };
    let helpers = root
        .descendants::<LuaClosureExpr>()
        .filter_map(|closure| {
            let block = closure.get_block()?;
            Some((
                block.syntax().text_range(),
                Arc::new(summarize_helper(&site, &closure, &block)),
            ))
        })
        .collect();
    file.helpers = helpers;
    Some(file)
}

/// A file's syntax together with its call facts.
struct NetFileSite<'a> {
    file_id: FileId,
    functions: &'a FileFunctionMap,
    calls: &'a FileNetHelpers,
}

impl NetFileSite<'_> {
    fn call(&self, call_expr: &LuaCallExpr) -> Option<&NetCallFact> {
        self.calls.call(call_expr)
    }

    fn role(&self, call_expr: &LuaCallExpr) -> Option<NetCallRole> {
        self.call(call_expr).and_then(|fact| fact.role.clone())
    }

    fn target(&self, call_expr: &LuaCallExpr) -> Option<HelperId> {
        self.call(call_expr).and_then(|fact| fact.target.clone())
    }
}

fn summarize_helper(
    site: &NetFileSite<'_>,
    closure: &LuaClosureExpr,
    block: &LuaBlock,
) -> NetHelper {
    let calls = block
        .syntax()
        .descendants()
        .filter_map(LuaCallExpr::cast)
        .map(|call_expr| summarize_helper_call(site, block, &call_expr))
        .collect::<Vec<_>>();
    let mut ops = Vec::new();
    op_items(site, block, block.syntax(), &mut ops);
    let sends = helper_sends(site, block);
    NetHelper {
        params: read_params(closure, &calls, &sends),
        calls,
        ops,
        sends,
    }
}

/// `closure`'s parameters by position, named where `calls` and `sends` read
/// them, without the unread ones at the end.
fn read_params(
    closure: &LuaClosureExpr,
    calls: &[HelperCall],
    sends: &[HelperSend],
) -> Vec<Option<String>> {
    let read = calls
        .iter()
        .flat_map(|call| call.args.iter().flatten())
        .chain(sends.iter().map(|send| &send.message))
        .flat_map(StringTemplate::names)
        .collect::<HashSet<_>>();
    let mut params = get_closure_param_names(closure)
        .into_iter()
        .map(|param| read.contains(param.as_str()).then_some(param))
        .collect::<Vec<_>>();
    while params.last().is_some_and(Option::is_none) {
        params.pop();
    }
    params
}

fn summarize_helper_call(
    site: &NetFileSite<'_>,
    block: &LuaBlock,
    call_expr: &LuaCallExpr,
) -> HelperCall {
    let (role, target) = site
        .call(call_expr)
        .map(|fact| (fact.role.clone(), fact.target.clone()))
        .unwrap_or_default();
    let mut call = HelperCall {
        nested: is_call_expr_in_nested_closure(block, call_expr),
        role,
        target,
        args: Vec::new(),
        send_target: None,
        literal_receive: false,
        receive_reads: NetReceiveReads::default(),
    };
    if call.nested {
        return call;
    }
    call.args = arg_templates(call_expr);
    match call.role {
        Some(NetCallRole::Send(send_kind)) => {
            call.send_target = extract_send_target_text(call_expr, send_kind);
        }
        Some(NetCallRole::Receive {
            message_idx,
            callback_idx,
        }) => {
            call.literal_receive =
                extract_static_string_arg_value(call_expr, message_idx).is_some();
            if !call.literal_receive {
                call.receive_reads = receive_reads(site, call_expr, callback_idx);
            }
        }
        _ => {}
    }
    call
}

/// The start-to-send transactions written in `helper_block`'s own blocks.
fn helper_sends(site: &NetFileSite<'_>, helper_block: &LuaBlock) -> Vec<HelperSend> {
    let mut sends = Vec::new();
    for block in helper_block
        .syntax()
        .descendants()
        .filter_map(LuaBlock::cast)
    {
        if block.syntax() != helper_block.syntax()
            && is_block_in_nested_closure(helper_block, &block)
        {
            continue;
        }

        let stats = block.get_stats().collect::<Vec<_>>();
        for (index, stat) in stats.iter().enumerate() {
            let Some(start_call) = call_expr_from_stat(stat) else {
                continue;
            };
            let Some(NetCallRole::Start { message_idx }) = site.role(&start_call) else {
                continue;
            };
            let Some(message) = arg_templates(&start_call)
                .into_iter()
                .nth(message_idx)
                .flatten()
            else {
                continue;
            };

            let mut stat_items = Vec::new();
            let mut send = None;
            for next_stat in stats.iter().skip(index + 1) {
                if let Some(next_call) = call_expr_from_stat(next_stat) {
                    match site.role(&next_call) {
                        Some(NetCallRole::Start { .. }) => break,
                        Some(NetCallRole::Send(send_kind)) => {
                            send =
                                Some((send_kind, extract_send_target_text(&next_call, send_kind)));
                            break;
                        }
                        _ => {}
                    }
                }
                let mut items = Vec::new();
                op_items(site, &block, next_stat.syntax(), &mut items);
                stat_items.push(items);
            }
            sends.push(HelperSend {
                start_range: start_call.get_range(),
                message,
                stats: stat_items,
                send,
            });
        }
    }
    sends
}

fn is_block_in_nested_closure(function_block: &LuaBlock, candidate_block: &LuaBlock) -> bool {
    candidate_block
        .syntax()
        .ancestors()
        .take_while(|node| node != function_block.syntax())
        .any(|node| LuaClosureExpr::can_cast(node.kind().into()))
}

/// The payload walk of `subtree`, in argument-first evaluation order, with
/// helper calls left as [`NetOpItem::Helper`] steps.
fn op_items(
    site: &NetFileSite<'_>,
    block: &LuaBlock,
    subtree: &LuaSyntaxNode,
    out: &mut Vec<NetOpItem>,
) {
    let Some(call_expr) = LuaCallExpr::cast(subtree.clone()) else {
        for child in subtree.children() {
            op_items(site, block, &child, out);
        }
        return;
    };
    if is_call_expr_in_nested_closure(block, &call_expr) {
        return;
    }
    if let Some(args) = call_expr.get_args_list() {
        for arg in args.get_args() {
            op_items(site, block, arg.syntax(), out);
        }
    }
    let Some(fact) = site.call(&call_expr) else {
        return;
    };
    // A call the metadata recognizes as a net op is a leaf: it never doubles as
    // a helper to expand into.
    match &fact.role {
        Some(NetCallRole::Payload(op)) => out.push(NetOpItem::Op(NetOpEntry {
            op: op.clone(),
            display_name: call_display_name(&call_expr),
            file_id: site.file_id,
            range: call_expr.get_range(),
            dynamic: is_call_expr_in_dynamic_control_flow(block, &call_expr),
            bits: fact
                .bits_param_idx
                .and_then(|arg_idx| extract_bit_width_arg(&call_expr, arg_idx)),
            has_bits_param: fact.bits_param_idx.is_some(),
            value_text: extract_write_value_text(&call_expr, op),
            flow_path: extract_flow_path(site.file_id, block, &call_expr),
        })),
        Some(_) => {}
        None => {
            if let Some(target) = &fact.target {
                out.push(NetOpItem::Helper {
                    target: target.clone(),
                    dynamic: is_call_expr_in_dynamic_control_flow(block, &call_expr),
                    flow_path: extract_flow_path(site.file_id, block, &call_expr),
                });
            }
        }
    }
}

/// The reads of a receive call's callback.
fn receive_reads(
    site: &NetFileSite<'_>,
    call_expr: &LuaCallExpr,
    callback_idx: Option<usize>,
) -> NetReceiveReads {
    // No annotated `callback` role means we cannot know which argument holds the
    // receiver, so treat the reads as unknown rather than as none — asserting an
    // empty read list here would invent count mismatches against every send.
    let Some(callback_idx) = callback_idx else {
        return NetReceiveReads {
            items: None,
            opaque: true,
        };
    };
    let Some(callback_expr) = call_expr
        .get_args_list()
        .and_then(|args| args.get_args().nth(callback_idx))
    else {
        return NetReceiveReads::default();
    };
    match resolve_callback_block(&callback_expr, site.functions) {
        Some(callback_block) => {
            let mut items = Vec::new();
            op_items(site, &callback_block, callback_block.syntax(), &mut items);
            NetReceiveReads {
                items: Some(items),
                opaque: false,
            }
        }
        // Inline closure that can't yield a block is malformed — but a bare
        // name reference we couldn't resolve in the file is the common case
        // (callback defined elsewhere). Mark opaque so the mismatch checker
        // skips this flow without losing the counterpart record.
        None => NetReceiveReads {
            items: None,
            opaque: !matches!(callback_expr, LuaExpr::ClosureExpr(_)),
        },
    }
}

/// Resolve the callback block for a `net.Receive` second argument. Handles
/// inline closures (`function() ... end`) and same-file local/global function
/// references (`net.Receive("Msg", doRetrieve)` paired with
/// `local function doRetrieve() ... end` or `local doRetrieve = function() ... end`).
fn resolve_callback_block(
    callback_expr: &LuaExpr,
    functions: &FileFunctionMap,
) -> Option<LuaBlock> {
    if let LuaExpr::ClosureExpr(closure_expr) = callback_expr {
        return closure_expr.get_block();
    }
    let LuaExpr::NameExpr(name_expr) = callback_expr else {
        return None;
    };
    functions
        .bare
        .get(name_expr.get_name_text()?.as_str())
        .cloned()
}

/// The body block at a helper-registry entry: the registry files each body by
/// its block, so the entry names it without reading the body.
fn resolve_registry_entry(file_id: &FileId, syntax_id: &LuaSyntaxId) -> HelperId {
    InFiled::new(*file_id, syntax_id.get_range())
}

/// The function body a call expands into.
fn resolve_call_to_function_block(
    root_file_id: FileId,
    call_expr: &LuaCallExpr,
    helper_registry: &HelperRegistry,
    functions: &FileFunctionMap,
    net: &mut NetCallResolver,
    db: &DbIndex,
) -> Option<HelperId> {
    // Direct global member calls have a stable symbol identity even when
    // duplicate declarations make the inferred signature winner dependent on
    // index order. Resolve that identity through the deterministic registry.
    // Aliases and locals take the signature-identity path below.
    if !call_expr_has_shadowing_local_root(db, root_file_id, call_expr)
        && let Some(call_path) = call_expr.get_access_path()
    {
        let call_path = call_path.strip_prefix("_G.").unwrap_or(&call_path);
        let global_id = GlobalId::new(call_path);
        if let Some((file_id, syntax_id)) = helper_registry.global(&global_id) {
            return Some(resolve_registry_entry(file_id, syntax_id));
        }
    }

    if let Some(signature_id) = net.signature_id(db, call_expr)
        && let Some((file_id, syntax_id)) = helper_registry.signature(&signature_id)
    {
        return Some(resolve_registry_entry(file_id, syntax_id));
    }

    // Pre-analysis can run before every local/global callable type cache is
    // available. Preserve lexical same-file wrapper expansion only when the
    // written bare name identifies exactly one function body in that file.
    if let Some(LuaExpr::NameExpr(name_expr)) = call_expr.get_prefix_expr()
        && let Some(name) = name_expr.get_name_text()
        && let Some(block) = functions.bare.get(name.as_str())
    {
        return Some(InFiled::new(root_file_id, block.syntax().text_range()));
    }

    // Dynamic receivers sometimes have no inferable class in Lua source. Keep
    // existing wrapper support only when the method name maps to exactly one
    // indexed function body workspace-wide; ambiguity is a hard stop.
    if call_expr.is_colon_call()
        && let Some(LuaExpr::IndexExpr(index_expr)) = call_expr.get_prefix_expr()
        && let Some(LuaIndexKey::Name(method_token)) = index_expr.get_index_key()
    {
        let method_name = SmolStr::new(method_token.get_name_text());
        if let Some((file_id, syntax_id)) = helper_registry.method(&method_name) {
            return Some(resolve_registry_entry(file_id, syntax_id));
        }
    }

    None
}

/// Resolves [`NetCallRole`]s of one file's calls from signature metadata.
struct NetCallResolver {
    cache: LuaInferCache,
    /// What the file's calls target.
    targets: Option<Arc<crate::FileCallTargets>>,
    /// The role, the helper resolution and the bit-width lookup all need a
    /// call's signature, so it is looked up once.
    signature_memo: HashMap<TextRange, Option<LuaSignatureId>>,
}

impl NetCallResolver {
    fn new(db: &DbIndex, file_id: FileId) -> Self {
        NetCallResolver {
            cache: LuaInferCache::new(file_id, super::super::call_target::pass_view()),
            targets: db.get_call_target_index().file(file_id),
            signature_memo: HashMap::default(),
        }
    }

    fn role(&mut self, db: &DbIndex, call_expr: &LuaCallExpr) -> Option<NetCallRole> {
        let signature_id = self.signature_id(db, call_expr)?;
        let signature = db.get_signature_index().get(&signature_id)?;
        let call_arg_idx = |param_idx| {
            param_idx_to_call_arg_idx(
                param_idx,
                call_expr.is_colon_call(),
                signature.is_colon_define,
            )
        };

        if let Some(mut send_kind) = crate::db_index::signature_net_send(db, signature_id) {
            send_kind.target_arg_idx = send_kind.target_arg_idx.and_then(call_arg_idx);
            return Some(NetCallRole::Send(send_kind));
        }
        if let Some(descriptor) = crate::db_index::signature_net_payload(db, signature_id) {
            return Some(NetCallRole::Payload(descriptor));
        }

        for param_idx in 0..signature.params.len() {
            let Some(role) = crate::db_index::find_best_call_arg_role_for_param(
                signature,
                param_idx,
                crate::db_index::GMOD_DOMAIN_NET_MESSAGE,
                &["start", "receive"],
            ) else {
                continue;
            };
            return match role.role.as_str() {
                "start" => Some(NetCallRole::Start {
                    message_idx: call_arg_idx(param_idx)?,
                }),
                "receive" => Some(NetCallRole::Receive {
                    message_idx: call_arg_idx(param_idx)?,
                    callback_idx: crate::db_index::signature_net_callback_param_idx(signature)
                        .and_then(call_arg_idx),
                }),
                _ => None,
            };
        }
        None
    }

    /// Index of the bit-width parameter, from the `gmod.net_payload`/`bits` role.
    fn bits_param_idx(&mut self, db: &DbIndex, call_expr: &LuaCallExpr) -> Option<usize> {
        let signature_id = self.signature_id(db, call_expr)?;
        let signature = db.get_signature_index().get(&signature_id)?;
        let param_idx = crate::db_index::signature_net_bits_param_idx(db, signature_id)?;
        param_idx_to_call_arg_idx(
            param_idx,
            call_expr.is_colon_call(),
            signature.is_colon_define,
        )
    }

    fn signature_id(&mut self, db: &DbIndex, call_expr: &LuaCallExpr) -> Option<LuaSignatureId> {
        if let Some(cached) = self.signature_memo.get(&call_expr.get_range()) {
            return *cached;
        }
        let resolved = self.signature_id_uncached(db, call_expr);
        self.signature_memo.insert(call_expr.get_range(), resolved);
        resolved
    }

    fn signature_id_uncached(
        &mut self,
        db: &DbIndex,
        call_expr: &LuaCallExpr,
    ) -> Option<LuaSignatureId> {
        let target = super::super::call_target::call_target(
            db,
            self.targets.as_deref(),
            &mut self.cache,
            call_expr,
        );
        target.declared.or(target.unique)
    }
}

/// Collects the net message flows of the context's files.
///
/// A file's flows read its own [`FileNetHelpers`] and those of the helpers it
/// expands into, so they re-derive when one of those changes.
pub(super) fn collect_network_flows(db: &mut DbIndex, context: &AnalyzeContext) {
    for file_id in context
        .tree_list
        .iter()
        .map(|tree| tree.file_id)
        .collect::<Vec<_>>()
    {
        let network_data = compute_network_flows(db, file_id);
        apply_network_flows(db, file_id, network_data);
    }
}

/// A file's net message flows, read off the index; [`apply_network_flows`]
/// stores them.
pub(crate) fn compute_network_flows(
    db: &DbIndex,
    file_id: FileId,
) -> crate::db_index::FileNetworkData {
    if !db.get_emmyrc().gmod.enabled {
        return crate::db_index::FileNetworkData::default();
    }
    collect_file_network_flows(db, file_id)
}

pub(crate) fn apply_network_flows(
    db: &mut DbIndex,
    file_id: FileId,
    network_data: crate::db_index::FileNetworkData,
) {
    if network_data.send_flows.is_empty() && network_data.receive_flows.is_empty() {
        return;
    }
    db.get_gmod_network_index_mut()
        .add_file_data(file_id, network_data);
}

fn collect_file_network_flows(db: &DbIndex, file_id: FileId) -> crate::db_index::FileNetworkData {
    let Some(calls) = db.get_net_helpers::<FileNetHelpers>(file_id) else {
        return crate::db_index::FileNetworkData::default();
    };
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_chunk_node())
    else {
        return crate::db_index::FileNetworkData::default();
    };

    let (functions, file_blocks, callee_names) =
        crate::profile::phase("gmodnet/file_scan", || FileFunctionMap::scan(&root));
    let helper_call_sites = net_helper_call_sites(db, file_id, &callee_names);
    let mut ctx = NetCollectCtx {
        db,
        site: NetFileSite {
            file_id,
            functions: &functions,
            calls: &calls,
        },
        memo: HelperMemo::default(),
    };
    let net_candidates = crate::profile::phase("gmodnet/net_candidates", || {
        net_candidate_call_exprs(db, file_id, &root, &helper_call_sites)
    });

    let mut receive_flows = Vec::new();
    for call_expr in &net_candidates {
        if let Some(receive_flow) = collect_net_receive_flow(&mut ctx, call_expr) {
            receive_flows.push(receive_flow);
        } else if call_has_literal_string_arg(call_expr) {
            collect_receive_flows_from_helper_call(&mut ctx, call_expr, &mut receive_flows);
        }
    }
    receive_flows.sort_by_key(|flow| flow.receive_range.start());

    let mut send_flows = crate::profile::phase("gmodnet/send_direct", || {
        collect_net_send_flows(&mut ctx, &file_blocks)
    });
    send_flows.extend(crate::profile::phase("gmodnet/send_wrapped", || {
        collect_wrapped_net_send_flows(&mut ctx, &file_blocks)
    }));
    send_flows.extend(crate::profile::phase("gmodnet/send_unannotated", || {
        collect_unannotated_net_wrapper_send_flows(&mut ctx, &net_candidates)
    }));
    send_flows.sort_by_key(|flow| flow.start_range.start());
    let mut unique_send_flows = Vec::with_capacity(send_flows.len());
    for flow in send_flows {
        if !unique_send_flows.contains(&flow) {
            unique_send_flows.push(flow);
        }
    }

    crate::db_index::FileNetworkData {
        send_flows: unique_send_flows,
        receive_flows,
    }
}

/// One file's flow collection: its own syntax and facts, the database for
/// helper summaries, and the evaluation's composition memos.
struct NetCollectCtx<'a> {
    db: &'a DbIndex,
    site: NetFileSite<'a>,
    memo: HelperMemo,
}

impl NetCollectCtx<'_> {
    /// A helper's summary, read as that summary alone; a body without one
    /// contributes nothing.
    fn helper(&self, id: &HelperId) -> Arc<NetHelper> {
        self.db
            .get_net_helper::<FileNetHelpers>(id)
            .and_then(|file| file.helpers.get(&id.value).cloned())
            .unwrap_or_default()
    }
}

/// Which `net` role a wrapper expansion is being asked to reach.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NetReachKind {
    Start,
    Receive,
}

type HelperBindingsKey = (HelperId, Vec<(String, String)>);

type HelperSendAnswer = (NetSendKind, Option<String>);

/// A helper-derived send transaction without the call-site origin, which the
/// caller stamps.
#[derive(Clone)]
struct CachedSendBody {
    message_name: String,
    writes: Vec<NetOpEntry>,
    send_kind: NetSendKind,
    send_target: Option<String>,
    materialized_from: Option<crate::db_index::NetFlowOrigin>,
}

/// A helper-derived receive transaction without the call-site range.
#[derive(Clone)]
struct CachedReceiveBody {
    message_name: String,
    reads: Vec<NetOpEntry>,
    reads_opaque: bool,
}

/// One evaluation's composition memos. Only answers that assumed nothing
/// about a helper still open on the composition stack are stored, so a
/// recursion cycle is never answered from a pruned expansion.
#[derive(Default)]
struct HelperMemo {
    reaches_start: HashMap<HelperId, bool>,
    reaches_receive: HashMap<HelperId, bool>,
    /// The consensus `net.Start` message: it can depend on the literals the
    /// caller binds, so the key carries the bindings.
    start_message: HashMap<HelperBindingsKey, Option<String>>,
    send: HashMap<HelperId, Option<HelperSendAnswer>>,
    send_body: HashMap<HelperBindingsKey, Vec<CachedSendBody>>,
    receive_body: HashMap<HelperBindingsKey, Vec<CachedReceiveBody>>,
    /// A helper's payload walk with no dynamic flag or flow prefix of its
    /// caller's; each caller stamps its own.
    ops: HashMap<(HelperId, NetOpDirection), Vec<NetOpEntry>>,
}

impl HelperMemo {
    fn reaches(&mut self, kind: NetReachKind) -> &mut HashMap<HelperId, bool> {
        match kind {
            NetReachKind::Start => &mut self.reaches_start,
            NetReachKind::Receive => &mut self.reaches_receive,
        }
    }
}

fn collect_net_send_flows(ctx: &mut NetCollectCtx<'_>, blocks: &[LuaBlock]) -> Vec<NetSendFlow> {
    let mut flows = Vec::new();

    for block in blocks {
        let stats: Vec<LuaStat> = block.get_stats().collect();
        for (index, stat) in stats.iter().enumerate() {
            let Some(call_expr) = call_expr_from_stat(stat) else {
                continue;
            };

            if !call_has_literal_string_arg(&call_expr) {
                continue;
            }

            let message_name = match ctx.site.role(&call_expr) {
                Some(NetCallRole::Start { message_idx }) => {
                    extract_static_string_arg_value(&call_expr, message_idx)
                }
                Some(_) => None,
                None => resolve_helper_start_message(ctx, &call_expr),
            };
            let Some(message_name) = message_name else {
                continue;
            };

            let mut writes = Vec::new();
            let mut send = None;

            for next_stat in stats.iter().skip(index + 1) {
                if let Some(next_call_expr) = call_expr_from_stat(next_stat) {
                    match ctx.site.role(&next_call_expr) {
                        Some(NetCallRole::Start { .. }) => break,
                        Some(NetCallRole::Send(send_kind)) => {
                            let target = extract_send_target_text(&next_call_expr, send_kind);
                            send = Some((
                                next_call_expr.get_range(),
                                send_kind,
                                target,
                                call_display_name(&next_call_expr),
                            ));
                            break;
                        }
                        Some(_) => {}
                        None => {
                            if resolve_helper_start_message(ctx, &next_call_expr).is_some() {
                                break;
                            }
                            if let Some((send_kind, send_target)) =
                                resolve_helper_send(ctx, &next_call_expr)
                            {
                                send = Some((
                                    next_call_expr.get_range(),
                                    send_kind,
                                    send_target,
                                    call_display_name(&next_call_expr),
                                ));
                                break;
                            }
                        }
                    }
                }

                collect_net_write_ops_from_stat(ctx, block, next_stat, &mut writes);
            }

            let Some((send_range, send_kind, send_target, send_display_name)) = send else {
                continue;
            };

            flows.push(NetSendFlow {
                message_name,
                start_range: call_expr.get_range(),
                writes,
                send_range,
                send_kind,
                send_display_name,
                send_target,
                is_wrapped: false,
                materialized_from: None,
            });
        }
    }

    flows.sort_by_key(|flow| flow.start_range.start());
    flows
}

fn resolve_helper_start_message(
    ctx: &mut NetCollectCtx<'_>,
    call_expr: &LuaCallExpr,
) -> Option<String> {
    let target = ctx.site.target(call_expr)?;
    let args = arg_templates(call_expr);
    helper_start_message(ctx, target, &args, &HashMap::default(), &mut Vec::new()).0
}

fn helper_start_message(
    ctx: &mut NetCollectCtx<'_>,
    helper_id: HelperId,
    args: &[Option<StringTemplate>],
    caller_bindings: &HashMap<String, String>,
    stack: &mut Vec<HelperId>,
) -> (Option<String>, usize) {
    let helper = ctx.helper(&helper_id);
    let bindings = helper_bindings(&helper.params, args, caller_bindings);
    let cache_key = (helper_id, sorted_bindings(&bindings));
    if let Some(cached) = ctx.memo.start_message.get(&cache_key) {
        return (cached.clone(), NO_CYCLE);
    }
    if let Some(open_depth) = stack.iter().position(|open| *open == cache_key.0) {
        return (None, open_depth);
    }
    let depth = stack.len();
    stack.push(cache_key.0.clone());

    let mut messages = Vec::new();
    let mut low = NO_CYCLE;
    for call in helper.calls.iter().filter(|call| !call.nested) {
        match &call.role {
            Some(NetCallRole::Start { message_idx }) => {
                if let Some(message) = resolve_arg(&call.args, *message_idx, &bindings) {
                    messages.push(message);
                }
            }
            Some(_) => {}
            None => {
                let Some(target) = &call.target else {
                    continue;
                };
                let (nested_message, nested_low) =
                    helper_start_message(ctx, target.clone(), &call.args, &bindings, stack);
                low = low.min(nested_low);
                if let Some(message) = nested_message {
                    messages.push(message);
                }
            }
        }
    }

    stack.pop();
    let mut messages = messages.into_iter();
    let answer = match messages.next() {
        Some(message) => messages
            .all(|candidate| candidate == message)
            .then_some(message),
        None => None,
    };
    // Consensus over a pruned branch set is unsound: a cycle-hidden call may
    // disagree with the visible ones, turning this `Some` into `None` once
    // the open helper resolves.
    if low >= depth {
        ctx.memo.start_message.insert(cache_key, answer.clone());
        return (answer, NO_CYCLE);
    }
    (answer, low)
}

fn resolve_helper_send(
    ctx: &mut NetCollectCtx<'_>,
    call_expr: &LuaCallExpr,
) -> Option<HelperSendAnswer> {
    let target = ctx.site.target(call_expr)?;
    helper_send(ctx, target, &mut Vec::new()).0
}

fn helper_send(
    ctx: &mut NetCollectCtx<'_>,
    helper_id: HelperId,
    stack: &mut Vec<HelperId>,
) -> (Option<HelperSendAnswer>, usize) {
    if let Some(cached) = ctx.memo.send.get(&helper_id) {
        return (cached.clone(), NO_CYCLE);
    }
    if let Some(open_depth) = stack.iter().position(|open| *open == helper_id) {
        return (None, open_depth);
    }
    let depth = stack.len();
    stack.push(helper_id.clone());

    let helper = ctx.helper(&helper_id);
    let mut sends = Vec::new();
    let mut low = NO_CYCLE;
    for call in helper.calls.iter().filter(|call| !call.nested) {
        match &call.role {
            Some(NetCallRole::Send(send_kind)) => {
                sends.push((*send_kind, call.send_target.clone()));
            }
            Some(_) => {}
            None => {
                let Some(target) = &call.target else {
                    continue;
                };
                let (nested_send, nested_low) = helper_send(ctx, target.clone(), stack);
                low = low.min(nested_low);
                if let Some(send) = nested_send {
                    sends.push(send);
                }
            }
        }
    }

    stack.pop();
    let mut sends = sends.into_iter();
    let answer = match sends.next() {
        Some(send) => sends.all(|candidate| candidate.0 == send.0).then_some(send),
        None => None,
    };
    if low >= depth {
        ctx.memo.send.insert(helper_id, answer.clone());
        return (answer, NO_CYCLE);
    }
    (answer, low)
}

fn collect_wrapped_net_send_flows(
    ctx: &mut NetCollectCtx<'_>,
    blocks: &[LuaBlock],
) -> Vec<NetSendFlow> {
    let mut flows = Vec::new();

    let owned_ranges: HashSet<TextRange> = ctx
        .site
        .functions
        .all_blocks
        .iter()
        .map(|block| block.syntax().text_range())
        .collect();
    for block in blocks {
        // The block's own closure (if any) owns it; a block with no enclosing
        // closure is top-level chunk code, which the direct scan covers.
        // Blocks inside an anonymous callback (e.g. a `net.Receive` receiver)
        // have no named owner and are skipped. A block nested inside another
        // named function is visited under that inner owner.
        let Some(owner_range) = block
            .syntax()
            .ancestors()
            .find_map(LuaClosureExpr::cast)
            .and_then(|closure| closure.get_block())
            .map(|owner| owner.syntax().text_range())
        else {
            continue;
        };
        if !owned_ranges.contains(&owner_range) {
            continue;
        }
        collect_wrapped_net_send_flows_in_block(ctx, block, &mut flows);
    }

    flows.sort_by_key(|flow| flow.start_range.start());
    flows
}

fn collect_wrapped_net_send_flows_in_block(
    ctx: &mut NetCollectCtx<'_>,
    block: &LuaBlock,
    flows: &mut Vec<NetSendFlow>,
) {
    let stats: Vec<LuaStat> = block.get_stats().collect();
    for (index, stat) in stats.iter().enumerate() {
        let Some(call_expr) = call_expr_from_stat(stat) else {
            continue;
        };

        if !call_has_literal_string_arg(&call_expr) {
            continue;
        }

        let Some(NetCallRole::Start { message_idx }) = ctx.site.role(&call_expr) else {
            continue;
        };
        let Some(message_name) = extract_static_string_arg_value(&call_expr, message_idx) else {
            continue;
        };

        let mut writes = Vec::new();
        let mut send = None;

        for next_stat in stats.iter().skip(index + 1) {
            if let Some(next_call_expr) = call_expr_from_stat(next_stat) {
                match ctx.site.role(&next_call_expr) {
                    Some(NetCallRole::Start { .. }) => break,
                    Some(NetCallRole::Send(send_kind)) => {
                        let target = extract_send_target_text(&next_call_expr, send_kind);
                        send = Some((
                            next_call_expr.get_range(),
                            send_kind,
                            target,
                            call_display_name(&next_call_expr),
                        ));
                        break;
                    }
                    _ => {}
                }
            }

            collect_net_write_ops_from_stat(ctx, block, next_stat, &mut writes);
        }

        if let Some((send_range, send_kind, send_target, send_display_name)) = send {
            flows.push(NetSendFlow {
                message_name,
                start_range: call_expr.get_range(),
                writes,
                send_range,
                send_kind,
                send_display_name,
                send_target,
                is_wrapped: true,
                materialized_from: None,
            });
            continue;
        }

        // Wrapped helper flows can start a net message in one function and send at call-site.
        // Keep a conservative stub so counterpart diagnostics can still resolve by message name.
        // The realm is a placeholder: `is_wrapped` flows are used for counterpart
        // presence only and are skipped by every realm-sensitive check.
        flows.push(NetSendFlow {
            message_name,
            start_range: call_expr.get_range(),
            writes: Vec::new(),
            send_range: call_expr.get_range(),
            send_kind: NetSendKind {
                receiver_realm: GmodRealm::Client,
                target_arg_idx: None,
            },
            send_display_name: call_display_name(&call_expr),
            send_target: None,
            is_wrapped: true,
            materialized_from: None,
        });
    }
}

/// Collect complete send flows performed by ordinary, unannotated helpers.
///
/// Literal string arguments at the call site bind the helper's parameters, so
/// `net.Start(messageName)` becomes concrete at
/// `MyLib.SendString("Message", value)`. Static message names take this same
/// path, which lets a no-argument helper produce a complete call-site flow.
fn collect_unannotated_net_wrapper_send_flows(
    ctx: &mut NetCollectCtx<'_>,
    candidates: &[LuaCallExpr],
) -> Vec<NetSendFlow> {
    let mut flows = Vec::new();
    for call_expr in candidates {
        if ctx.site.role(call_expr).is_some() {
            continue;
        }
        let Some(target) = ctx.site.target(call_expr) else {
            continue;
        };
        let args = arg_templates(call_expr);
        let (bodies, _) =
            helper_send_body(ctx, target, &args, &HashMap::default(), &mut Vec::new());
        let origin_range = call_expr.get_range();
        let display_name = call_display_name(call_expr);
        for body in bodies {
            flows.push(NetSendFlow {
                message_name: body.message_name,
                start_range: origin_range,
                writes: body.writes,
                send_range: origin_range,
                send_kind: body.send_kind,
                send_display_name: display_name.clone(),
                send_target: body.send_target,
                is_wrapped: false,
                materialized_from: body.materialized_from,
            });
        }
    }
    flows
}

/// The calls in a file that can take part in net-flow collection.
///
/// A lookup rather than a walk: the call sites that can expand into a
/// net-producing helper come from the reference index, which already records
/// every reference to those helpers' names.
fn net_candidate_call_exprs(
    db: &DbIndex,
    file_id: FileId,
    root: &LuaChunk,
    helper_call_sites: &NetHelperCallSites<'_>,
) -> Vec<LuaCallExpr> {
    let root_syntax = root.syntax().clone();
    // Locals never enter the global reference table, so a helper declared
    // `local function send()` is reached through its own declaration's
    // references instead.
    let mut local_sites: Vec<LuaSyntaxId> = Vec::new();
    if let Some(decl_tree) = db.get_decl_index().get_decl_tree(&file_id) {
        // `local sendString = MyLib.SendString` calls the helper under a name
        // the reference index files under the local binding rather than under
        // the helper, so this file's own bindings of a helper name count as
        // call sites too. Chains settle by iterating, bounded by the number of
        // bindings in the file.
        let mut aliases: HashSet<SmolStr> = HashSet::default();
        let bindings = decl_tree
            .get_decls()
            .values()
            .filter_map(|decl| {
                let source = decl
                    .get_value_syntax_id()?
                    .to_node_from_root(&root_syntax)
                    .and_then(LuaExpr::cast)
                    .as_ref()
                    .and_then(expr_written_name)?;
                Some((SmolStr::new(decl.get_name()), source))
            })
            .collect::<Vec<_>>();
        loop {
            let mut added = false;
            for (bound, source) in &bindings {
                if (helper_call_sites.is_helper_name(source) || aliases.contains(source))
                    && !helper_call_sites.is_helper_name(bound)
                    && aliases.insert(bound.clone())
                {
                    added = true;
                }
            }
            if !added {
                break;
            }
        }

        for decl in decl_tree.get_decls().values() {
            let name = decl.get_name();
            if !decl.is_local()
                || !helper_call_sites.is_called(name)
                || !(helper_call_sites.is_helper_name(name) || aliases.contains(name))
            {
                continue;
            }
            let Some(references) = db
                .get_reference_index()
                .get_decl_references(&file_id, &decl.get_id())
            else {
                continue;
            };
            local_sites.extend(
                references
                    .cells
                    .iter()
                    .filter(|cell| !cell.is_write)
                    .map(|cell| {
                        LuaSyntaxId::new(glua_parser::LuaSyntaxKind::NameExpr.into(), cell.range)
                    }),
            );
        }
    }
    let mut calls = helper_call_sites
        .sites
        .iter()
        .chain(local_sites.iter())
        .filter_map(|syntax_id| syntax_id.to_node_from_root(&root_syntax))
        .filter_map(|node| {
            let call_expr = LuaCallExpr::cast(node.parent()?)?;
            // The reference is the callee only when it is the call's prefix;
            // `f(SendThing)` passes it as an argument instead.
            (call_expr.get_prefix_expr()?.syntax() == &node).then_some(call_expr)
        })
        .collect::<Vec<_>>();

    // Source order. The key is the whole range so that equal calls reached
    // through both the name and the local-declaration lookup end up adjacent
    // and the dedup can see them.
    calls.sort_by_key(|call_expr| {
        let range = call_expr.get_range();
        (range.start(), range.end())
    });
    calls.dedup_by_key(|call_expr| call_expr.get_range());
    calls
}

/// Send transactions a helper performs for one set of caller bindings,
/// without any call-site origin.
fn helper_send_body(
    ctx: &mut NetCollectCtx<'_>,
    helper_id: HelperId,
    args: &[Option<StringTemplate>],
    caller_bindings: &HashMap<String, String>,
    stack: &mut Vec<HelperId>,
) -> (Vec<CachedSendBody>, usize) {
    let helper = ctx.helper(&helper_id);
    let bindings = helper_bindings(&helper.params, args, caller_bindings);
    let cache_key = (helper_id, sorted_bindings(&bindings));
    if let Some(cached) = ctx.memo.send_body.get(&cache_key) {
        return (cached.clone(), NO_CYCLE);
    }
    let helper_id = &cache_key.0;
    if let Some(open_depth) = stack.iter().position(|open| open == helper_id) {
        return (Vec::new(), open_depth);
    }

    // A send flow always starts at a `net.Start` somewhere in the expansion, so
    // a helper that cannot reach one contributes nothing however it is called.
    let (reaches_start, _) =
        helper_reaches_net_role(ctx, helper_id, NetReachKind::Start, &mut Vec::new());
    if !reaches_start {
        return (Vec::new(), NO_CYCLE);
    }

    let depth = stack.len();
    stack.push(helper_id.clone());

    let mut bodies = Vec::new();
    let mut low = NO_CYCLE;
    for send in &helper.sends {
        let Some(message_name) = send.message.resolve(&bindings) else {
            continue;
        };
        let mut writes = Vec::new();
        for items in &send.stats {
            compose_ops(
                ctx,
                items,
                NetOpDirection::Write,
                &mut Vec::new(),
                &mut writes,
            );
        }
        let Some((send_kind, send_target)) = send.send.clone() else {
            continue;
        };
        bodies.push(CachedSendBody {
            message_name,
            writes,
            send_kind,
            send_target,
            materialized_from: Some(crate::db_index::NetFlowOrigin {
                file_id: helper_id.file_id,
                start_range: send.start_range,
            }),
        });
    }

    // Follow wrapper chains as well: an outer helper can delegate the complete
    // transaction to an inner helper while forwarding its message parameter.
    for call in helper
        .calls
        .iter()
        .filter(|call| !call.nested && call.role.is_none())
    {
        let Some(target) = &call.target else {
            continue;
        };
        let (nested_bodies, nested_low) =
            helper_send_body(ctx, target.clone(), &call.args, &bindings, stack);
        low = low.min(nested_low);
        bodies.extend(nested_bodies);
    }

    stack.pop();
    // A pruned branch may hide a conflicting transaction, so only cycle-free
    // expansions are shared.
    if low >= depth {
        ctx.memo.send_body.insert(cache_key, bodies.clone());
        return (bodies, NO_CYCLE);
    }
    (bodies, low)
}

fn collect_net_receive_flow(
    ctx: &mut NetCollectCtx<'_>,
    call_expr: &LuaCallExpr,
) -> Option<NetReceiveFlow> {
    if !call_has_literal_string_arg(call_expr) {
        return None;
    }

    let Some(NetCallRole::Receive {
        message_idx,
        callback_idx,
    }) = ctx.site.role(call_expr)
    else {
        return None;
    };
    let message_name = extract_static_string_arg_value(call_expr, message_idx)?;
    let reads = receive_reads(&ctx.site, call_expr, callback_idx);
    let (reads, reads_opaque) = compose_receive_reads(ctx, &reads);
    Some(NetReceiveFlow {
        message_name,
        receive_range: call_expr.get_range(),
        reads,
        reads_opaque,
    })
}

fn compose_receive_reads(
    ctx: &mut NetCollectCtx<'_>,
    reads: &NetReceiveReads,
) -> (Vec<NetOpEntry>, bool) {
    let mut entries = Vec::new();
    if let Some(items) = &reads.items {
        compose_ops(
            ctx,
            items,
            NetOpDirection::Read,
            &mut Vec::new(),
            &mut entries,
        );
    }
    (entries, reads.opaque)
}

fn collect_receive_flows_from_helper_call(
    ctx: &mut NetCollectCtx<'_>,
    call_expr: &LuaCallExpr,
    flows: &mut Vec<NetReceiveFlow>,
) {
    let Some(target) = ctx.site.target(call_expr) else {
        return;
    };
    let args = arg_templates(call_expr);
    let (bodies, _) = helper_receive_body(ctx, target, &args, &HashMap::default(), &mut Vec::new());
    let origin_range = call_expr.get_range();
    for body in bodies {
        flows.push(NetReceiveFlow {
            message_name: body.message_name,
            receive_range: origin_range,
            reads: body.reads,
            reads_opaque: body.reads_opaque,
        });
    }
}

/// Receive registrations a helper makes for one set of caller bindings. Only
/// registrations whose message the bindings make concrete count: literal ones
/// are already indexed in the helper's own file.
fn helper_receive_body(
    ctx: &mut NetCollectCtx<'_>,
    helper_id: HelperId,
    args: &[Option<StringTemplate>],
    caller_bindings: &HashMap<String, String>,
    stack: &mut Vec<HelperId>,
) -> (Vec<CachedReceiveBody>, usize) {
    let helper = ctx.helper(&helper_id);
    let bindings = helper_bindings(&helper.params, args, caller_bindings);
    let cache_key = (helper_id, sorted_bindings(&bindings));
    if let Some(cached) = ctx.memo.receive_body.get(&cache_key) {
        return (cached.clone(), NO_CYCLE);
    }
    let helper_id = &cache_key.0;
    if let Some(open_depth) = stack.iter().position(|open| open == helper_id) {
        return (Vec::new(), open_depth);
    }

    // A receive flow always originates at a `net.Receive` somewhere in the
    // expansion, so a helper that cannot reach one contributes nothing.
    let (reaches_receive, _) =
        helper_reaches_net_role(ctx, helper_id, NetReachKind::Receive, &mut Vec::new());
    if !reaches_receive {
        return (Vec::new(), NO_CYCLE);
    }

    let depth = stack.len();
    stack.push(helper_id.clone());

    let mut bodies = Vec::new();
    let mut low = NO_CYCLE;
    for call in helper.calls.iter().filter(|call| !call.nested) {
        match &call.role {
            Some(NetCallRole::Receive { message_idx, .. }) => {
                if call.literal_receive {
                    continue;
                }
                let Some(message_name) = resolve_arg(&call.args, *message_idx, &bindings) else {
                    continue;
                };
                let (reads, reads_opaque) = compose_receive_reads(ctx, &call.receive_reads);
                bodies.push(CachedReceiveBody {
                    message_name,
                    reads,
                    reads_opaque,
                });
            }
            Some(_) => {}
            None => {
                let Some(target) = &call.target else {
                    continue;
                };
                let (nested_bodies, nested_low) =
                    helper_receive_body(ctx, target.clone(), &call.args, &bindings, stack);
                low = low.min(nested_low);
                bodies.extend(nested_bodies);
            }
        }
    }

    stack.pop();
    if low >= depth {
        ctx.memo.receive_body.insert(cache_key, bodies.clone());
        return (bodies, NO_CYCLE);
    }
    (bodies, low)
}

/// Whether expanding a helper can reach a `net.Start` / `net.Receive`.
fn helper_reaches_net_role(
    ctx: &mut NetCollectCtx<'_>,
    helper_id: &HelperId,
    kind: NetReachKind,
    stack: &mut Vec<HelperId>,
) -> (bool, usize) {
    if let Some(cached) = ctx.memo.reaches(kind).get(helper_id) {
        return (*cached, NO_CYCLE);
    }
    if let Some(open_depth) = stack.iter().position(|open| open == helper_id) {
        return (false, open_depth);
    }
    let depth = stack.len();
    stack.push(helper_id.clone());

    let helper = ctx.helper(helper_id);
    let hit = |call: &&HelperCall| match kind {
        NetReachKind::Start => matches!(call.role, Some(NetCallRole::Start { .. })),
        NetReachKind::Receive => matches!(call.role, Some(NetCallRole::Receive { .. })),
    };
    let mut reaches = helper.calls.iter().any(|call| hit(&call));
    let mut low = NO_CYCLE;
    if !reaches {
        for target in helper.calls.iter().filter_map(|call| call.target.as_ref()) {
            let (nested_reaches, nested_low) = helper_reaches_net_role(ctx, target, kind, stack);
            low = low.min(nested_low);
            if nested_reaches {
                reaches = true;
                break;
            }
        }
    }

    stack.pop();
    if reaches || low >= depth {
        ctx.memo.reaches(kind).insert(helper_id.clone(), reaches);
        return (reaches, NO_CYCLE);
    }
    (reaches, low)
}

fn collect_net_write_ops_from_stat(
    ctx: &mut NetCollectCtx<'_>,
    block: &LuaBlock,
    stat: &LuaStat,
    writes: &mut Vec<NetOpEntry>,
) {
    let mut items = Vec::new();
    op_items(&ctx.site, block, stat.syntax(), &mut items);
    compose_ops(ctx, &items, NetOpDirection::Write, &mut Vec::new(), writes);
}

/// The ops of a payload walk in `direction`, with each helper step expanded
/// into that helper's own walk. Returns the lowest open helper the expansion
/// was pruned at.
fn compose_ops(
    ctx: &mut NetCollectCtx<'_>,
    items: &[NetOpItem],
    direction: NetOpDirection,
    stack: &mut Vec<HelperId>,
    out: &mut Vec<NetOpEntry>,
) -> usize {
    let mut low = NO_CYCLE;
    for item in items {
        match item {
            NetOpItem::Op(entry) => {
                if entry.op.direction == direction {
                    out.push(entry.clone());
                }
            }
            NetOpItem::Helper {
                target,
                dynamic,
                flow_path,
            } => {
                let helper_low =
                    expand_helper_ops(ctx, target, direction, *dynamic, flow_path, stack, out);
                low = low.min(helper_low);
            }
        }
    }
    low
}

/// A helper call's payload ops: the helper's own walk, stamped with the call
/// site's dynamic context and flow prefix.
fn expand_helper_ops(
    ctx: &mut NetCollectCtx<'_>,
    helper_id: &HelperId,
    direction: NetOpDirection,
    force_dynamic: bool,
    prefix: &[NetFlowFrame],
    stack: &mut Vec<HelperId>,
    out: &mut Vec<NetOpEntry>,
) -> usize {
    let ops_key = (helper_id.clone(), direction);
    if let Some(cached) = ctx.memo.ops.get(&ops_key) {
        stamp_ops(cached, force_dynamic, prefix, out);
        return NO_CYCLE;
    }
    if let Some(open_depth) = stack.iter().position(|open| open == helper_id) {
        return open_depth;
    }

    let depth = stack.len();
    stack.push(helper_id.clone());
    let helper = ctx.helper(helper_id);
    let mut canonical = Vec::new();
    let body_low = compose_ops(ctx, &helper.ops, direction, stack, &mut canonical);
    stack.pop();
    stamp_ops(&canonical, force_dynamic, prefix, out);
    // Only cycle-free walks are shared; the current expansion still emits its
    // pruned ops.
    if body_low >= depth {
        ctx.memo.ops.insert(ops_key, canonical);
        return NO_CYCLE;
    }
    body_low
}

/// Stamps a helper's payload walk with the caller's dynamic context: `dynamic`
/// is the OR of the outer flag and the op's own, and the outer prefix is
/// prepended to the op's path.
fn stamp_ops(
    canonical: &[NetOpEntry],
    force_dynamic: bool,
    prefix: &[NetFlowFrame],
    out: &mut Vec<NetOpEntry>,
) {
    for entry in canonical {
        let mut stamped = entry.clone();
        stamped.dynamic = force_dynamic || entry.dynamic;
        if !prefix.is_empty() {
            let mut flow_path = Vec::with_capacity(prefix.len() + entry.flow_path.len());
            flow_path.extend_from_slice(prefix);
            flow_path.extend_from_slice(&entry.flow_path);
            stamped.flow_path = flow_path;
        }
        out.push(stamped);
    }
}
