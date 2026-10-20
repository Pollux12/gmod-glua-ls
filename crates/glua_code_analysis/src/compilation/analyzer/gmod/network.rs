//! Net message flow collection.
//!
//! Each call has a [`NetCallFact`] of what inference decides about it (its net
//! role and the function body it expands into), and each function body a
//! syntax-free [`NetHelper`] summary built from the facts of the calls in it.
//! A call's flows walk its own syntax against the facts of the calls around it
//! and expand helper calls by composing the helpers' summaries, so a helper
//! body is analysed once per change of its summary rather than once per
//! evaluation of every call into it, and a change reaches only the calls and
//! bodies that read it.
//!
//! Summaries only describe their own body: a nested helper is named, never
//! inlined, so no summary reads another and derivations cannot cycle. What a
//! helper's expansion reaches, ends a message with and reads or writes depends
//! on no caller: each is composed once per file and pass in
//! [`NetCompositions`] and reused by every call of the file into the helper,
//! which records the reads the composition made as its own. What a caller's
//! literals bind (message names and the transactions they make concrete)
//! composes in the caller's evaluation, over the helpers that reach the role
//! it asks about.

use std::sync::Arc;

use super::*;
use crate::db_index::derivation::{self, Provenance, Reuse};

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
    Literal(SmolStr),
    Name(SmolStr),
}

impl StringTemplate {
    fn of(expr: &LuaExpr) -> Option<Self> {
        let mut parts = Vec::new();
        push_template_parts(expr, &mut parts)?;
        parts.shrink_to_fit();
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
                TemplatePart::Name(name) => value.push_str(bindings.get(name.as_str())?),
            }
        }
        Some(value)
    }
}

fn push_template_parts(expr: &LuaExpr, parts: &mut Vec<TemplatePart>) -> Option<()> {
    match expr {
        LuaExpr::LiteralExpr(_) => {
            parts.push(TemplatePart::Literal(static_literal_string(expr)?.into()))
        }
        LuaExpr::NameExpr(name_expr) => parts.push(TemplatePart::Name(name_expr.get_name_text()?)),
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
            let mut templates: Vec<_> = args
                .get_args()
                .map(|arg| StringTemplate::of(&arg))
                .collect();
            templates.shrink_to_fit();
            templates
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
/// string under the caller's own bindings. A colon call passes its receiver
/// first and a colon definition takes it as `self`, so either side may hold
/// one slot the other writes out.
fn helper_bindings(
    helper: &NetHelper,
    args: &[Option<StringTemplate>],
    colon_call: bool,
    caller_bindings: &HashMap<String, String>,
) -> HashMap<String, String> {
    helper
        .params
        .iter()
        .enumerate()
        .filter_map(|(param_idx, param)| {
            let arg_idx = (param_idx + usize::from(helper.colon_define))
                .checked_sub(usize::from(colon_call))?;
            Some((
                param.clone()?,
                args.get(arg_idx)?.as_ref()?.resolve(caller_bindings)?,
            ))
        })
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
    /// Whether the call goes through a name that can reach a net operation,
    /// which makes it a call site of a net helper.
    candidate: bool,
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
    /// Whether the call is written with `:`, passing its receiver first.
    colon_call: bool,
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
    /// Whether a statement calls a function received as a parameter, whose
    /// writes the body cannot show.
    calls_param: bool,
    send: Option<(NetSendKind, Option<String>)>,
}

impl Hash for HelperSend {
    fn hash<H: Hasher>(&self, state: &mut H) {
        crate::db_index::derivation::hash_local(&self.start_range, state);
        self.message.hash(state);
        self.stats.hash(state);
        self.calls_param.hash(state);
        self.send.hash(state);
    }
}

/// What a function body contributes to net flows, without its callers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
struct NetHelper {
    /// Each parameter, by position, named when the body's message or argument
    /// templates read it: a parameter nothing reads binds nothing.
    params: Vec<Option<String>>,
    /// Whether the function is defined with `:`, taking its receiver as `self`.
    colon_define: bool,
    /// Every call in the body, in document order.
    calls: Vec<HelperCall>,
    /// The payload walk of the whole body.
    ops: Vec<NetOpItem>,
    sends: Vec<HelperSend>,
}

impl NetOpItem {
    fn target(&self) -> Option<&HelperId> {
        match self {
            NetOpItem::Helper { target, .. } => Some(target),
            NetOpItem::Op(_) => None,
        }
    }

    fn visit_target(&mut self, visit: &mut impl FnMut(&mut HelperId)) {
        if let NetOpItem::Helper { target, .. } = self {
            visit(target);
        }
    }
}

impl NetHelper {
    /// Every helper the summary calls into.
    fn targets(&self) -> Vec<&HelperId> {
        let mut targets = Vec::new();
        for call in &self.calls {
            targets.extend(call.target.as_ref());
            targets.extend(
                call.receive_reads
                    .items
                    .iter()
                    .flatten()
                    .filter_map(NetOpItem::target),
            );
        }
        targets.extend(self.ops.iter().filter_map(NetOpItem::target));
        targets.extend(
            self.sends
                .iter()
                .flat_map(|send| send.stats.iter().flatten())
                .filter_map(NetOpItem::target),
        );
        targets
    }

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

/// Where the function body at `target` moved to under `remap`.
fn remap_target(remap: &crate::FileRemap, target: &HelperId) -> crate::Remap<HelperId> {
    let block = LuaSyntaxId::new(
        glua_parser::LuaKind::Syntax(glua_parser::LuaSyntaxKind::Block),
        target.value,
    );
    match remap.syntax_id(target.file_id, block) {
        crate::Remap::Moved(block) if block.get_range() != target.value => {
            crate::Remap::Moved(InFiled::new(target.file_id, block.get_range()))
        }
        crate::Remap::Lost => crate::Remap::Lost,
        crate::Remap::Moved(_) | crate::Remap::Unrelated => crate::Remap::Unrelated,
    }
}

impl crate::db_index::derivation::DerivedFact for NetCallFact {
    fn fingerprint(&self) -> crate::db_index::derivation::Fingerprint {
        crate::db_index::derivation::fingerprint_of(self)
    }

    fn named_files(&self, own: FileId) -> rustc_hash::FxHashSet<FileId> {
        self.target
            .iter()
            .map(|target| target.file_id)
            .filter(|file_id| *file_id != own)
            .collect()
    }

    fn remap(
        &self,
        remap: &crate::FileRemap,
    ) -> crate::Remap<Arc<dyn crate::db_index::derivation::DerivedFact>> {
        let Some(target) = &self.target else {
            return crate::Remap::Unrelated;
        };
        match remap_target(remap, target) {
            crate::Remap::Moved(target) => crate::Remap::Moved(Arc::new(NetCallFact {
                target: Some(target),
                ..self.clone()
            })),
            crate::Remap::Lost => crate::Remap::Lost,
            crate::Remap::Unrelated => crate::Remap::Unrelated,
        }
    }
}

impl crate::db_index::derivation::DerivedFact for NetHelper {
    fn fingerprint(&self) -> crate::db_index::derivation::Fingerprint {
        crate::db_index::derivation::fingerprint_of(self)
    }

    /// Every helper a summary calls into is some call's target.
    fn named_files(&self, own: FileId) -> rustc_hash::FxHashSet<FileId> {
        self.targets()
            .into_iter()
            .map(|target| target.file_id)
            .filter(|file_id| *file_id != own)
            .collect()
    }

    fn remap(
        &self,
        remap: &crate::FileRemap,
    ) -> crate::Remap<Arc<dyn crate::db_index::derivation::DerivedFact>> {
        let mut moves = HashMap::default();
        for target in self.targets() {
            match remap_target(remap, target) {
                crate::Remap::Moved(moved) => {
                    moves.insert(target.clone(), moved);
                }
                crate::Remap::Lost => return crate::Remap::Lost,
                crate::Remap::Unrelated => {}
            }
        }
        if moves.is_empty() {
            return crate::Remap::Unrelated;
        }
        let mut moved = self.clone();
        moved.visit_targets(&mut |target: &mut HelperId| {
            if let Some(new) = moves.get(target) {
                *target = new.clone();
            }
        });
        crate::Remap::Moved(Arc::new(moved))
    }
}

/// A file's function lookup and local bindings, read off its syntax once per
/// analysis pass: a pass rebuilds no source layer, so the file's syntax and
/// declarations hold for all of it.
#[derive(Default)]
pub(crate) struct FileNetSyntax {
    /// The body block of each function a bare name defines once in the file.
    bare: HashMap<String, TextRange>,
    /// The body blocks of the functions the file binds to a name or a field.
    named_blocks: HashSet<TextRange>,
    /// The written names of the values bound to each name, sorted.
    bindings: HashMap<SmolStr, Vec<SmolStr>>,
}

impl FileNetSyntax {
    pub(crate) fn new(db: &DbIndex, file_id: FileId) -> Self {
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_chunk_node())
        else {
            return Self::default();
        };
        let functions = FileFunctionMap::build(&root);
        let mut bindings: HashMap<SmolStr, Vec<SmolStr>> = HashMap::default();
        if let Some(decl_tree) = db.get_decl_index().get_decl_tree(&file_id) {
            for decl in decl_tree.get_decls().values() {
                let Some(source) = decl
                    .get_value_syntax_id()
                    .and_then(|syntax_id| syntax_id.to_node_from_root(root.syntax()))
                    .and_then(LuaExpr::cast)
                    .as_ref()
                    .and_then(expr_written_name)
                else {
                    continue;
                };
                bindings
                    .entry(SmolStr::new(decl.get_name()))
                    .or_default()
                    .push(source);
            }
        }
        for sources in bindings.values_mut() {
            sources.sort_unstable();
            sources.dedup();
        }
        Self {
            bare: functions
                .bare
                .iter()
                .map(|(name, block)| (name.clone(), block.syntax().text_range()))
                .collect(),
            named_blocks: functions
                .all_blocks
                .iter()
                .map(|block| block.syntax().text_range())
                .collect(),
            bindings,
        }
    }

    /// Whether `name` is a net-helper name, or is bound to a value written as
    /// one that is.
    fn reaches_helper(
        &self,
        network: &crate::db_index::GmodNetworkIndex,
        name: &SmolStr,
        visited: &mut HashSet<SmolStr>,
    ) -> bool {
        if network.is_helper_name(name) {
            return true;
        }
        if !visited.insert(name.clone()) {
            return false;
        }
        self.bindings.get(name).is_some_and(|sources| {
            sources
                .iter()
                .any(|source| self.reaches_helper(network, source, visited))
        })
    }

    /// Whether `name`, which is not a net-helper name, is bound to a value
    /// written as a net-helper name or as another such alias.
    fn is_alias(&self, network: &crate::db_index::GmodNetworkIndex, name: &SmolStr) -> bool {
        let mut visited = HashSet::default();
        visited.insert(name.clone());
        self.bindings.get(name).is_some_and(|sources| {
            sources
                .iter()
                .any(|source| self.reaches_helper(network, source, &mut visited))
        })
    }
}

/// One call's net fact.
pub(crate) fn evaluate_net_call(
    db: &mut DbIndex,
    id: crate::db_index::derivation::DerivationId,
    syntax: &FileNetSyntax,
) {
    if !db.get_emmyrc().gmod.enabled {
        return;
    }
    let file_id = id.file_id;
    let Some(call_expr) = super::super::call_target::call_at(db, file_id, id.syntax_id()) else {
        return;
    };
    let roles = derive::call_role_entries(db);
    let Some(fact) = net_call_fact(db, file_id, &call_expr, &roles.registry, syntax) else {
        return;
    };
    db.set_net_call(file_id, call_expr.get_range(), Arc::new(fact));
}

/// What `call_expr` is in the net subsystem, `None` for a call that is none.
fn net_call_fact(
    db: &DbIndex,
    file_id: FileId,
    call_expr: &LuaCallExpr,
    helper_registry: &HelperRegistry,
    syntax: &FileNetSyntax,
) -> Option<NetCallFact> {
    let signature_id = super::super::call_target::call_target_of(db, file_id, call_expr)
        .and_then(crate::LuaCallTarget::signature_id);
    let role = signature_id.and_then(|signature_id| net_call_role(db, call_expr, signature_id));
    let target = resolve_call_to_function_block(
        file_id,
        call_expr,
        helper_registry,
        syntax,
        signature_id,
        db,
    );
    if role.is_none() && target.is_none() {
        return None;
    }
    let bits_param_idx = match (&role, signature_id) {
        (Some(NetCallRole::Payload(_)), Some(signature_id)) => {
            bits_param_idx(db, call_expr, signature_id)
        }
        _ => None,
    };
    Some(NetCallFact {
        role,
        target,
        bits_param_idx,
        candidate: is_net_helper_call(db, file_id, call_expr, syntax),
    })
}

/// Whether `call_expr` is a call site of a net helper: the reference index
/// files its callee under a helper name, or the callee is a local bound to a
/// helper name or to a name that is one.
///
/// A lookup rather than a walk: the call sites that can expand into a
/// net-producing helper come from the reference index, which already records
/// every reference to those helpers' names.
fn is_net_helper_call(
    db: &DbIndex,
    file_id: FileId,
    call_expr: &LuaCallExpr,
    syntax: &FileNetSyntax,
) -> bool {
    let Some(prefix) = call_expr.get_prefix_expr() else {
        return false;
    };
    let Some(name) = expr_written_name(&prefix) else {
        return false;
    };
    let network = db.get_gmod_network_index();
    let helper_name = network.is_helper_name(&name);
    if helper_name
        && db
            .get_reference_index()
            .name_sites_in_file([&name], file_id)
            .contains(&LuaSyntaxId::from_node(prefix.syntax()))
    {
        return true;
    }
    // Locals never enter the global reference table, so a helper declared
    // `local function send()` is reached through its own declaration's
    // references instead, and `local sendString = MyLib.SendString` calls the
    // helper under a name the reference index files under the local binding.
    let LuaExpr::NameExpr(name_expr) = &prefix else {
        return false;
    };
    if !helper_name && !syntax.is_alias(network, &name) {
        return false;
    }
    let range = name_expr.get_range();
    let references = db.get_reference_index();
    let Some(decl_id) = references.get_var_reference_decl(&file_id, range) else {
        return false;
    };
    db.get_decl_index()
        .get_decl(&decl_id)
        .is_some_and(|decl| decl.is_local())
        && references
            .decl_references(&decl_id)
            .is_some_and(|references| {
                references
                    .cells
                    .iter()
                    .any(|cell| cell.range == range && !cell.is_write)
            })
}

/// One function body's net summary, from the facts of the calls in it.
pub(crate) fn evaluate_net_helper(
    db: &mut DbIndex,
    id: crate::db_index::derivation::DerivationId,
    syntax: &FileNetSyntax,
) {
    if !db.get_emmyrc().gmod.enabled {
        return;
    }
    let file_id = id.file_id;
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_chunk_node())
    else {
        return;
    };
    let Some(closure) = id
        .syntax_id()
        .to_node_from_root(root.syntax())
        .and_then(LuaClosureExpr::cast)
    else {
        return;
    };
    let Some(block) = closure.get_block() else {
        return;
    };
    let summary = summarize_helper(
        &NetFileSite::new(db, file_id, &root, syntax),
        &closure,
        &block,
    );
    if summary != NetHelper::default() {
        db.set_net_helper(file_id, block.syntax().text_range(), Arc::new(summary));
    }
}

/// A file's syntax together with the net facts of its calls.
struct NetFileSite<'a> {
    db: &'a DbIndex,
    file_id: FileId,
    root: LuaSyntaxNode,
    syntax: &'a FileNetSyntax,
}

impl<'a> NetFileSite<'a> {
    fn new(db: &'a DbIndex, file_id: FileId, root: &LuaChunk, syntax: &'a FileNetSyntax) -> Self {
        Self {
            db,
            file_id,
            root: root.syntax().clone(),
            syntax,
        }
    }

    fn call(&self, call_expr: &LuaCallExpr) -> Option<Arc<NetCallFact>> {
        self.db
            .get_net_call::<NetCallFact>(self.file_id, call_expr.get_range())
    }

    fn role(&self, call_expr: &LuaCallExpr) -> Option<NetCallRole> {
        self.call(call_expr).and_then(|fact| fact.role.clone())
    }

    fn target(&self, call_expr: &LuaCallExpr) -> Option<HelperId> {
        self.call(call_expr).and_then(|fact| fact.target.clone())
    }

    /// The body of the function a bare name defines once in the file.
    fn bare_block(&self, name: &str) -> Option<LuaBlock> {
        let block = LuaSyntaxId::new(
            glua_parser::LuaKind::Syntax(glua_parser::LuaSyntaxKind::Block),
            *self.syntax.bare.get(name)?,
        );
        LuaBlock::cast(block.to_node_from_root(&self.root)?)
    }
}

fn summarize_helper(
    site: &NetFileSite<'_>,
    closure: &LuaClosureExpr,
    block: &LuaBlock,
) -> NetHelper {
    let mut calls = block
        .descendants::<LuaCallExpr>()
        .map(|call_expr| summarize_helper_call(site, block, &call_expr))
        .collect::<Vec<_>>();
    let mut ops = Vec::new();
    op_items(site, block, block.syntax(), &mut ops);
    let mut sends = helper_sends(site, block);
    let params = read_params(closure, &calls, &sends);
    // A call with neither a role nor a target is no net step; only its
    // arguments could name a parameter, which `params` has read.
    calls.retain(|call| call.role.is_some() || call.target.is_some());
    calls.shrink_to_fit();
    ops.shrink_to_fit();
    sends.shrink_to_fit();
    let colon_define = site
        .db
        .get_signature_index()
        .get(&LuaSignatureId::from_closure(site.file_id, closure))
        .is_some_and(|signature| signature.is_colon_define);
    NetHelper {
        params,
        colon_define,
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
        colon_call: call_expr.is_colon_call(),
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

/// The `net.Start` .. send transactions in a helper body. A callback the helper
/// defines, such as a click handler, sends once it runs: its transactions
/// count when the helper's own parameters name the message, since a literal
/// name is already indexed where it is written.
fn helper_sends(site: &NetFileSite<'_>, helper_block: &LuaBlock) -> Vec<HelperSend> {
    let mut sends = Vec::new();
    for block in helper_block.descendants::<LuaBlock>() {
        let callbacks = closures_between(helper_block, &block);
        let callback_params = callbacks
            .iter()
            .flat_map(get_closure_param_names)
            .collect::<HashSet<_>>();
        let params = enclosing_param_names(&block);
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
            if !callbacks.is_empty()
                && (message.names().next().is_none()
                    || message.names().any(|name| callback_params.contains(name)))
            {
                continue;
            }

            let mut stat_items = Vec::new();
            let mut calls_param = false;
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
                calls_param |= stat_calls_param(&block, next_stat, &params);
            }
            sends.push(HelperSend {
                start_range: start_call.get_range(),
                message,
                stats: stat_items,
                calls_param,
                send,
            });
        }
    }
    sends
}

/// Whether `stat` calls one of `params` outside the closures it defines.
fn stat_calls_param(block: &LuaBlock, stat: &LuaStat, params: &HashSet<String>) -> bool {
    stat.syntax()
        .descendants()
        .filter_map(LuaCallExpr::cast)
        .filter(|call_expr| !is_call_expr_in_nested_closure(block, call_expr))
        .any(|call_expr| match call_expr.get_prefix_expr() {
            Some(LuaExpr::NameExpr(name_expr)) => name_expr
                .get_name_text()
                .is_some_and(|name| params.contains(name.as_str())),
            _ => false,
        })
}

/// The closures between `function_block` and `candidate_block`, innermost first.
fn closures_between(function_block: &LuaBlock, candidate_block: &LuaBlock) -> Vec<LuaClosureExpr> {
    candidate_block
        .syntax()
        .ancestors()
        .take_while(|node| node != function_block.syntax())
        .filter_map(LuaClosureExpr::cast)
        .collect()
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
    match resolve_callback_block(&callback_expr, site) {
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
fn resolve_callback_block(callback_expr: &LuaExpr, site: &NetFileSite<'_>) -> Option<LuaBlock> {
    if let LuaExpr::ClosureExpr(closure_expr) = callback_expr {
        return closure_expr.get_block();
    }
    let LuaExpr::NameExpr(name_expr) = callback_expr else {
        return None;
    };
    site.bare_block(name_expr.get_name_text()?.as_str())
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
    syntax: &FileNetSyntax,
    signature_id: Option<LuaSignatureId>,
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

    if let Some(signature_id) = signature_id
        && let Some((file_id, syntax_id)) = helper_registry.signature(&signature_id)
    {
        return Some(resolve_registry_entry(file_id, syntax_id));
    }

    // Pre-analysis can run before every local/global callable type cache is
    // available. Preserve lexical same-file wrapper expansion only when the
    // written bare name identifies exactly one function body in that file.
    if let Some(LuaExpr::NameExpr(name_expr)) = call_expr.get_prefix_expr()
        && let Some(name) = name_expr.get_name_text()
        && let Some(block) = syntax.bare.get(name.as_str())
    {
        return Some(InFiled::new(root_file_id, *block));
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

/// The net role of `call_expr`, which targets `signature_id`, from signature metadata.
fn net_call_role(
    db: &DbIndex,
    call_expr: &LuaCallExpr,
    signature_id: LuaSignatureId,
) -> Option<NetCallRole> {
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
fn bits_param_idx(
    db: &DbIndex,
    call_expr: &LuaCallExpr,
    signature_id: LuaSignatureId,
) -> Option<usize> {
    let signature = db.get_signature_index().get(&signature_id)?;
    let param_idx = crate::db_index::signature_net_bits_param_idx(db, signature_id)?;
    param_idx_to_call_arg_idx(
        param_idx,
        call_expr.is_colon_call(),
        signature.is_colon_define,
    )
}

/// One call's net message flows.
///
/// The call reads the facts of the calls around it and the summaries of the
/// helpers it expands into, so it runs again when one of those changes.
pub(crate) fn evaluate_net_flows(
    db: &mut DbIndex,
    id: crate::db_index::derivation::DerivationId,
    syntax: &FileNetSyntax,
    compositions: &mut NetCompositions,
) {
    if !db.get_emmyrc().gmod.enabled {
        return;
    }
    let file_id = id.file_id;
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_chunk_node())
    else {
        return;
    };
    let Some(call_expr) = id
        .syntax_id()
        .to_node_from_root(root.syntax())
        .and_then(LuaCallExpr::cast)
    else {
        return;
    };
    let flows = {
        let mut ctx = NetCollectCtx {
            site: NetFileSite::new(db, file_id, &root, syntax),
            memo: HelperMemo::default(),
            compositions,
        };
        collect_call_flows(&mut ctx, &call_expr)
    };
    if !flows.is_empty() {
        db.get_gmod_network_index_mut()
            .set_call_flows(file_id, call_expr.get_range(), flows);
    }
}

/// The flows `call_expr` starts, receives or materializes: the receive it
/// registers, the send written at it, the send of a wrapper that holds it, and
/// the sends of the helper it calls.
fn collect_call_flows(ctx: &mut NetCollectCtx<'_>, call_expr: &LuaCallExpr) -> CallFlows {
    let mut flows = CallFlows::default();
    let candidate = ctx.site.call(call_expr).is_some_and(|fact| fact.candidate);
    if candidate {
        if let Some(receive_flow) = collect_net_receive_flow(ctx, call_expr) {
            flows.receive_flows.push(receive_flow);
        } else if call_has_literal_string_arg(call_expr) {
            collect_receive_flows_from_helper_call(ctx, call_expr, &mut flows.receive_flows);
        }
    }
    let mut send_flows = Vec::new();
    if call_has_literal_string_arg(call_expr)
        && let Some((block, stat)) = call_stat_in_block(call_expr)
    {
        send_flows.extend(collect_net_send_flow(ctx, &block, &stat, call_expr));
        send_flows.extend(collect_wrapped_net_send_flow(ctx, &block, &stat, call_expr));
    }
    if candidate {
        send_flows.extend(collect_unannotated_net_wrapper_send_flows(ctx, call_expr));
    }
    for flow in send_flows {
        if !flows.send_flows.contains(&flow) {
            flows.send_flows.push(flow);
        }
    }
    flows
}

/// The statement `call_expr` is, and the block that holds it.
fn call_stat_in_block(call_expr: &LuaCallExpr) -> Option<(LuaBlock, LuaStat)> {
    let stat = LuaStat::cast(call_expr.syntax().parent()?)?;
    if call_expr_from_stat(&stat)?.syntax() != call_expr.syntax() {
        return None;
    }
    let block = LuaBlock::cast(stat.syntax().parent()?)?;
    Some((block, stat))
}

/// The statements that follow `stat` in its block.
fn stats_after(stat: &LuaStat) -> impl Iterator<Item = LuaStat> {
    std::iter::successors(stat.syntax().next_sibling(), |node| node.next_sibling())
        .filter_map(LuaStat::cast)
}

/// One call's flow collection: its own syntax and the facts around it, the
/// evaluation's compositions under the call's bindings, and the file's
/// compositions that depend on no caller.
struct NetCollectCtx<'a> {
    site: NetFileSite<'a>,
    memo: HelperMemo,
    compositions: &'a mut NetCompositions,
}

impl NetCollectCtx<'_> {
    /// A helper's summary, read as that summary alone.
    fn helper(&self, id: &HelperId) -> Arc<NetHelper> {
        helper_summary(self.site.db, id)
    }

    fn reach(&mut self, id: &HelperId) -> NetReach {
        let db = self.site.db;
        composed(&mut self.compositions.reach, id, || {
            reach_walk(db, id, &mut Vec::new(), &mut HashMap::default()).0
        })
    }

    fn send(&mut self, id: &HelperId) -> Option<HelperSendAnswer> {
        let db = self.site.db;
        composed(&mut self.compositions.send, id, || {
            send_walk(db, id, &mut Vec::new(), &mut HashMap::default()).0
        })
    }

    /// The payload ops of a helper's expansion, in both directions.
    fn ops(&mut self, id: &HelperId) -> Arc<Vec<NetOpEntry>> {
        let db = self.site.db;
        composed(&mut self.compositions.ops, id, || {
            let mut ops = Vec::new();
            expand_helper_ops(
                db,
                id,
                false,
                &[],
                &mut Vec::new(),
                &mut HashMap::default(),
                &mut ops,
            );
            Arc::new(ops)
        })
    }
}

/// A file's compositions of the helpers its calls expand into that depend on
/// no caller, for one pass: each walks the helper from itself as the root, and
/// keeps what it read so a call reusing it records those reads as its own.
#[derive(Default)]
pub(crate) struct NetCompositions {
    reach: HashMap<HelperId, (NetReach, Provenance)>,
    send: HashMap<HelperId, (Option<HelperSendAnswer>, Provenance)>,
    ops: HashMap<HelperId, (Arc<Vec<NetOpEntry>>, Provenance)>,
}

/// `memo`'s composition of `id` while what it read holds, those reads
/// recorded into the evaluation; otherwise `compose`'s, kept with its reads
/// when they vouch for it.
fn composed<V: Clone>(
    memo: &mut HashMap<HelperId, (V, Provenance)>,
    id: &HelperId,
    compose: impl FnOnce() -> V,
) -> V {
    if let Some((value, provenance)) = memo.get(id)
        && derivation::reuse(provenance, 0, &[], None) == Reuse::Held
    {
        return value.clone();
    }
    let frame = derivation::open_frame();
    let value = compose();
    match frame.and_then(|frame| derivation::close_frame(frame, 0)) {
        Some(provenance) => {
            memo.insert(id.clone(), (value.clone(), provenance));
        }
        None => {
            memo.remove(id);
        }
    }
    value
}

/// A helper's summary; a body without one contributes nothing.
fn helper_summary(db: &DbIndex, id: &HelperId) -> Arc<NetHelper> {
    db.get_net_helper::<NetHelper>(id).unwrap_or_default()
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
    /// Whether `writes` may miss what a caller-supplied function writes.
    writes_opaque: bool,
    materialized_from: Option<crate::db_index::NetFlowOrigin>,
}

/// A helper-derived receive transaction without the call-site range.
#[derive(Clone)]
struct CachedReceiveBody {
    message_name: String,
    reads: Vec<NetOpEntry>,
    reads_opaque: bool,
}

/// One evaluation's compositions under the caller's bindings, which can
/// depend on the literals the caller binds, so each key carries the
/// bindings. Only answers that assumed nothing about a helper still open on
/// the composition stack are stored, so a recursion cycle is never answered
/// from a pruned expansion.
#[derive(Default)]
struct HelperMemo {
    /// The consensus `net.Start` message.
    start_message: HashMap<HelperBindingsKey, Option<String>>,
    send_body: HashMap<HelperBindingsKey, Vec<CachedSendBody>>,
    receive_body: HashMap<HelperBindingsKey, Vec<CachedReceiveBody>>,
}

/// The transaction written at `call_expr`, a statement of `block`: the
/// `net.Start` or helper call, the payload statements after it, and the send
/// that ends it.
fn collect_net_send_flow(
    ctx: &mut NetCollectCtx<'_>,
    block: &LuaBlock,
    stat: &LuaStat,
    call_expr: &LuaCallExpr,
) -> Option<NetSendFlow> {
    let message_name = match ctx.site.role(call_expr) {
        Some(NetCallRole::Start { message_idx }) => {
            extract_static_string_arg_value(call_expr, message_idx)
        }
        Some(_) => None,
        None => resolve_helper_start_message(ctx, call_expr),
    }?;

    let params = enclosing_param_names(block);
    let mut writes = Vec::new();
    let mut writes_opaque = false;
    let mut send = None;

    for next_stat in stats_after(stat) {
        if let Some(next_call_expr) = call_expr_from_stat(&next_stat) {
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

        collect_net_write_ops_from_stat(ctx, block, &next_stat, &mut writes);
        writes_opaque |= stat_calls_param(block, &next_stat, &params);
    }

    let (send_range, send_kind, send_target, send_display_name) = send?;
    Some(NetSendFlow {
        message_name,
        start_range: call_expr.get_range(),
        writes,
        send_range,
        send_kind,
        send_display_name,
        send_target,
        is_wrapped: false,
        writes_opaque,
        materialized_from: None,
    })
}

/// The parameter names of every closure enclosing `block`.
fn enclosing_param_names(block: &LuaBlock) -> HashSet<String> {
    block
        .syntax()
        .ancestors()
        .filter_map(LuaClosureExpr::cast)
        .flat_map(|closure| get_closure_param_names(&closure))
        .collect()
}

fn resolve_helper_start_message(
    ctx: &mut NetCollectCtx<'_>,
    call_expr: &LuaCallExpr,
) -> Option<String> {
    let target = ctx.site.target(call_expr)?;
    let args = arg_templates(call_expr);
    helper_start_message(
        ctx,
        target,
        &args,
        call_expr.is_colon_call(),
        &HashMap::default(),
        &mut Vec::new(),
    )
    .0
}

fn helper_start_message(
    ctx: &mut NetCollectCtx<'_>,
    helper_id: HelperId,
    args: &[Option<StringTemplate>],
    colon_call: bool,
    caller_bindings: &HashMap<String, String>,
    stack: &mut Vec<HelperId>,
) -> (Option<String>, usize) {
    // A helper that reaches no `net.Start` names no message, however its
    // calls recurse or its caller binds it.
    if !ctx.reach(&helper_id).start {
        return (None, NO_CYCLE);
    }
    let helper = ctx.helper(&helper_id);
    let bindings = helper_bindings(&helper, args, colon_call, caller_bindings);
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
                let (nested_message, nested_low) = helper_start_message(
                    ctx,
                    target.clone(),
                    &call.args,
                    call.colon_call,
                    &bindings,
                    stack,
                );
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
    if !ctx.reach(&target).send {
        return None;
    }
    ctx.send(&target)
}

/// The transaction a function holds at `call_expr`, a statement of `block`.
///
/// The block's own closure (if any) owns it; a block with no enclosing
/// closure is top-level chunk code, which the direct scan covers. Blocks
/// inside an anonymous callback (e.g. a `net.Receive` receiver) have no named
/// owner and are skipped. A block nested inside another named function is
/// visited under that inner owner.
fn collect_wrapped_net_send_flow(
    ctx: &mut NetCollectCtx<'_>,
    block: &LuaBlock,
    stat: &LuaStat,
    call_expr: &LuaCallExpr,
) -> Option<NetSendFlow> {
    let owner = block
        .syntax()
        .ancestors()
        .find_map(LuaClosureExpr::cast)
        .and_then(|closure| closure.get_block())?;
    if !ctx
        .site
        .syntax
        .named_blocks
        .contains(&owner.syntax().text_range())
    {
        return None;
    }

    let Some(NetCallRole::Start { message_idx }) = ctx.site.role(call_expr) else {
        return None;
    };
    let message_name = extract_static_string_arg_value(call_expr, message_idx)?;

    let mut writes = Vec::new();
    let mut send = None;

    for next_stat in stats_after(stat) {
        if let Some(next_call_expr) = call_expr_from_stat(&next_stat) {
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

        collect_net_write_ops_from_stat(ctx, block, &next_stat, &mut writes);
    }

    if let Some((send_range, send_kind, send_target, send_display_name)) = send {
        return Some(NetSendFlow {
            message_name,
            start_range: call_expr.get_range(),
            writes,
            send_range,
            send_kind,
            send_display_name,
            send_target,
            is_wrapped: true,
            writes_opaque: false,
            materialized_from: None,
        });
    }

    // Wrapped helper flows can start a net message in one function and send at call-site.
    // Keep a conservative stub so counterpart diagnostics can still resolve by message name.
    // The realm is a placeholder: `is_wrapped` flows are used for counterpart
    // presence only and are skipped by every realm-sensitive check.
    Some(NetSendFlow {
        message_name,
        start_range: call_expr.get_range(),
        writes: Vec::new(),
        send_range: call_expr.get_range(),
        send_kind: NetSendKind {
            receiver_realm: GmodRealm::Client,
            target_arg_idx: None,
        },
        send_display_name: call_display_name(call_expr),
        send_target: None,
        is_wrapped: true,
        writes_opaque: false,
        materialized_from: None,
    })
}

/// Collect complete send flows performed by an ordinary, unannotated helper
/// that `call_expr` calls.
///
/// Literal string arguments at the call site bind the helper's parameters, so
/// `net.Start(messageName)` becomes concrete at
/// `MyLib.SendString("Message", value)`. Static message names take this same
/// path, which lets a no-argument helper produce a complete call-site flow.
fn collect_unannotated_net_wrapper_send_flows(
    ctx: &mut NetCollectCtx<'_>,
    call_expr: &LuaCallExpr,
) -> Vec<NetSendFlow> {
    if ctx.site.role(call_expr).is_some() {
        return Vec::new();
    }
    let Some(target) = ctx.site.target(call_expr) else {
        return Vec::new();
    };
    let args = arg_templates(call_expr);
    let (bodies, _) = helper_send_body(
        ctx,
        target,
        &args,
        call_expr.is_colon_call(),
        &HashMap::default(),
        &mut Vec::new(),
    );
    let origin_range = call_expr.get_range();
    let display_name = call_display_name(call_expr);
    bodies
        .into_iter()
        .map(|body| NetSendFlow {
            message_name: body.message_name,
            start_range: origin_range,
            writes: body.writes,
            send_range: origin_range,
            send_kind: body.send_kind,
            send_display_name: display_name.clone(),
            send_target: body.send_target,
            is_wrapped: false,
            writes_opaque: body.writes_opaque,
            materialized_from: body.materialized_from,
        })
        .collect()
}

/// Send transactions a helper performs for one set of caller bindings,
/// without any call-site origin.
fn helper_send_body(
    ctx: &mut NetCollectCtx<'_>,
    helper_id: HelperId,
    args: &[Option<StringTemplate>],
    colon_call: bool,
    caller_bindings: &HashMap<String, String>,
    stack: &mut Vec<HelperId>,
) -> (Vec<CachedSendBody>, usize) {
    let helper = ctx.helper(&helper_id);
    let bindings = helper_bindings(&helper, args, colon_call, caller_bindings);
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
    if !ctx.reach(helper_id).start {
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
            compose_ops(ctx, items, NetOpDirection::Write, &mut writes);
        }
        let Some((send_kind, send_target)) = send.send.clone() else {
            continue;
        };
        bodies.push(CachedSendBody {
            message_name,
            writes,
            send_kind,
            send_target,
            writes_opaque: send.calls_param,
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
        let (nested_bodies, nested_low) = helper_send_body(
            ctx,
            target.clone(),
            &call.args,
            call.colon_call,
            &bindings,
            stack,
        );
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
        compose_ops(ctx, items, NetOpDirection::Read, &mut entries);
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
    let (bodies, _) = helper_receive_body(
        ctx,
        target,
        &args,
        call_expr.is_colon_call(),
        &HashMap::default(),
        &mut Vec::new(),
    );
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
    colon_call: bool,
    caller_bindings: &HashMap<String, String>,
    stack: &mut Vec<HelperId>,
) -> (Vec<CachedReceiveBody>, usize) {
    let helper = ctx.helper(&helper_id);
    let bindings = helper_bindings(&helper, args, colon_call, caller_bindings);
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
    if !ctx.reach(helper_id).receive {
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
                let (nested_bodies, nested_low) = helper_receive_body(
                    ctx,
                    target.clone(),
                    &call.args,
                    call.colon_call,
                    &bindings,
                    stack,
                );
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

fn collect_net_write_ops_from_stat(
    ctx: &mut NetCollectCtx<'_>,
    block: &LuaBlock,
    stat: &LuaStat,
    writes: &mut Vec<NetOpEntry>,
) {
    let mut items = Vec::new();
    op_items(&ctx.site, block, stat.syntax(), &mut items);
    compose_ops(ctx, &items, NetOpDirection::Write, writes);
}

/// The ops of a payload walk in `direction`, with each helper step expanded
/// into that helper's own walk.
fn compose_ops(
    ctx: &mut NetCollectCtx<'_>,
    items: &[NetOpItem],
    direction: NetOpDirection,
    out: &mut Vec<NetOpEntry>,
) {
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
                if !ctx.reach(target).payload {
                    continue;
                }
                let ops = ctx.ops(target);
                stamp_ops(
                    ops.iter().filter(|entry| entry.op.direction == direction),
                    *dynamic,
                    flow_path,
                    out,
                );
            }
        }
    }
}

/// The net roles a helper's expansion can reach: a call with the role in its
/// body, or in a body one of its calls expands into, transitively.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct NetReach {
    start: bool,
    receive: bool,
    send: bool,
    payload: bool,
}

impl NetReach {
    const ALL: Self = Self {
        start: true,
        receive: true,
        send: true,
        payload: true,
    };

    fn of_role(role: Option<&NetCallRole>) -> Self {
        Self {
            start: matches!(role, Some(NetCallRole::Start { .. })),
            receive: matches!(role, Some(NetCallRole::Receive { .. })),
            send: matches!(role, Some(NetCallRole::Send(_))),
            payload: matches!(role, Some(NetCallRole::Payload(_))),
        }
    }

    fn union(self, other: Self) -> Self {
        Self {
            start: self.start || other.start,
            receive: self.receive || other.receive,
            send: self.send || other.send,
            payload: self.payload || other.payload,
        }
    }
}

/// What `helper_id`'s expansion reaches, and the lowest open helper the walk
/// was pruned at. Only answers that assumed nothing about a helper still open
/// on `stack` are memoised, so a cycle is never answered from a pruned walk.
fn reach_walk(
    db: &DbIndex,
    helper_id: &HelperId,
    stack: &mut Vec<HelperId>,
    memo: &mut HashMap<HelperId, NetReach>,
) -> (NetReach, usize) {
    if let Some(cached) = memo.get(helper_id) {
        return (*cached, NO_CYCLE);
    }
    if let Some(open_depth) = stack.iter().position(|open| open == helper_id) {
        return (NetReach::default(), open_depth);
    }
    let depth = stack.len();
    stack.push(helper_id.clone());

    let helper = helper_summary(db, helper_id);
    let mut reach = NetReach::default();
    for call in &helper.calls {
        reach = reach.union(NetReach::of_role(call.role.as_ref()));
    }
    let mut low = NO_CYCLE;
    for target in helper.calls.iter().filter_map(|call| call.target.as_ref()) {
        if reach == NetReach::ALL {
            break;
        }
        let (nested_reach, nested_low) = reach_walk(db, target, stack, memo);
        low = low.min(nested_low);
        reach = reach.union(nested_reach);
    }

    stack.pop();
    if reach == NetReach::ALL || low >= depth {
        memo.insert(helper_id.clone(), reach);
        return (reach, NO_CYCLE);
    }
    (reach, low)
}

/// The send `helper_id`'s expansion ends a message with: the one its sends
/// agree on the kind of; and the lowest open helper the walk was pruned at.
fn send_walk(
    db: &DbIndex,
    helper_id: &HelperId,
    stack: &mut Vec<HelperId>,
    memo: &mut HashMap<HelperId, Option<HelperSendAnswer>>,
) -> (Option<HelperSendAnswer>, usize) {
    if let Some(cached) = memo.get(helper_id) {
        return (cached.clone(), NO_CYCLE);
    }
    if let Some(open_depth) = stack.iter().position(|open| open == helper_id) {
        return (None, open_depth);
    }
    let depth = stack.len();
    stack.push(helper_id.clone());

    let helper = helper_summary(db, helper_id);
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
                let (nested_send, nested_low) = send_walk(db, target, stack, memo);
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
        memo.insert(helper_id.clone(), answer.clone());
        return (answer, NO_CYCLE);
    }
    (answer, low)
}

/// A helper call's payload ops: the helper's own walk with each helper step
/// expanded, stamped with the call site's dynamic context and flow prefix.
/// Returns the lowest open helper the expansion was pruned at.
fn expand_helper_ops(
    db: &DbIndex,
    helper_id: &HelperId,
    force_dynamic: bool,
    prefix: &[NetFlowFrame],
    stack: &mut Vec<HelperId>,
    memo: &mut HashMap<HelperId, Vec<NetOpEntry>>,
    out: &mut Vec<NetOpEntry>,
) -> usize {
    if let Some(cached) = memo.get(helper_id) {
        stamp_ops(cached, force_dynamic, prefix, out);
        return NO_CYCLE;
    }
    if let Some(open_depth) = stack.iter().position(|open| open == helper_id) {
        return open_depth;
    }

    let depth = stack.len();
    stack.push(helper_id.clone());
    let helper = helper_summary(db, helper_id);
    let mut canonical = Vec::new();
    let mut body_low = NO_CYCLE;
    for item in &helper.ops {
        match item {
            NetOpItem::Op(entry) => canonical.push(entry.clone()),
            NetOpItem::Helper {
                target,
                dynamic,
                flow_path,
            } => {
                let helper_low =
                    expand_helper_ops(db, target, *dynamic, flow_path, stack, memo, &mut canonical);
                body_low = body_low.min(helper_low);
            }
        }
    }
    stack.pop();
    stamp_ops(&canonical, force_dynamic, prefix, out);
    // Only cycle-free walks are shared; the current expansion still emits its
    // pruned ops.
    if body_low >= depth {
        memo.insert(helper_id.clone(), canonical);
        return NO_CYCLE;
    }
    body_low
}

/// Stamps a helper's payload walk with the caller's dynamic context: `dynamic`
/// is the OR of the outer flag and the op's own, and the outer prefix is
/// prepended to the op's path.
fn stamp_ops<'a>(
    canonical: impl IntoIterator<Item = &'a NetOpEntry>,
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::db_index::derivation::{DerivationId, DerivationKind, FactKey, ReaderGraph};

    fn call(start: u32) -> DerivationId {
        DerivationId::new(
            DerivationKind::NetFlows,
            FileId::new(2),
            LuaSyntaxId::new(
                glua_parser::LuaSyntaxKind::CallExpr.into(),
                TextRange::new(start.into(), (start + 1).into()),
            ),
        )
    }

    /// The calls of a file compose a helper once: a later call reuses the
    /// composition and records the reads it was composed from as its own.
    #[test]
    fn a_composition_is_computed_once_and_its_reads_recorded_by_each_call() {
        let helper = InFiled::new(FileId::new(1), TextRange::new(0.into(), 10.into()));
        let summary = FactKey::NetHelper(helper.clone());
        let mut memo = HashMap::default();
        let computed = Cell::new(0);
        let mut graph = ReaderGraph::default();
        derivation::lend_keys(graph.lend_keys());
        let calls = [0, 1].map(|start| {
            let _recording = derivation::begin(call(start));
            let value = composed(&mut memo, &helper, || {
                computed.set(computed.get() + 1);
                derivation::note_read(|| summary.clone());
                7
            });
            let (_, reads, _, _) = derivation::end().into_recorded();
            (value, reads.len())
        });
        graph.restore_keys(derivation::take_keys());

        assert_eq!((computed.get(), calls), (1, [(7, 1), (7, 1)]));
    }
}
