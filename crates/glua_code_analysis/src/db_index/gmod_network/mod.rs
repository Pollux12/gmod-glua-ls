use crate::db_index::cow_shards::{CowCell, CowShards};
use crate::db_index::derivation::{FilePosition, hash_local};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use rowan::{TextRange, TextSize};
use smol_str::SmolStr;

use super::{LuaDeclId, LuaIndex};
use crate::{
    FileId, FileRemap, GmodRealm, Remap,
    db_index::derivation::{self, FactKey, Fingerprint},
};

mod pair;

pub use pair::{
    flows_can_match, is_opposite_strict_realm_pair, is_strict_realm, pair_senders_for_receive,
};

/// Direction of a net payload operation, from the `net_payload` attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetOpDirection {
    Write,
    Read,
}

impl NetOpDirection {
    pub fn opposite(self) -> Self {
        match self {
            Self::Write => Self::Read,
            Self::Read => Self::Write,
        }
    }

    pub fn from_attribute_value(value: &str) -> Option<Self> {
        match value {
            "write" => Some(Self::Write),
            "read" => Some(Self::Read),
            _ => None,
        }
    }
}

/// A net payload operation, derived entirely from the `net_payload` signature
/// attribute rather than from the callee's name.
///
/// `wire_format` is the on-the-wire encoding identifier. A write pairs with a
/// read when both carry the same `wire_format` and opposite directions — that is
/// the single matching rule. It is deliberately independent of the declared Lua
/// type: `net.ReadInt`, `net.ReadUInt`, `net.ReadFloat` and `net.ReadDouble` all
/// declare `@return number`, so only the wire format distinguishes them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NetOpDescriptor {
    pub wire_format: SmolStr,
    pub direction: NetOpDirection,
}

/// Canonical callable for one annotated wire format and direction.
///
/// This is metadata about the selected signature, not about an observed call.
/// In particular, read completion must use the read signature's bit parameter
/// rather than assuming it matches the writer's parameter layout.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanonicalNetOp {
    pub name: String,
    pub has_bits_param: bool,
}

impl NetOpDescriptor {
    pub fn is_write(&self) -> bool {
        matches!(self.direction, NetOpDirection::Write)
    }

    pub fn is_read(&self) -> bool {
        matches!(self.direction, NetOpDirection::Read)
    }

    /// True when `self` is a write whose payload `read` consumes.
    pub fn pairs_with(&self, read: &NetOpDescriptor) -> bool {
        self.wire_format == read.wire_format && self.direction == read.direction.opposite()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetOpEntry {
    pub op: NetOpDescriptor,
    /// Source path of the function actually called, e.g. `net.WriteString` or a
    /// wrapper's own name. Captured at index time so diagnostics and hover show
    /// what the developer wrote rather than a canonical builtin name.
    pub display_name: SmolStr,
    /// The file `range` is in: the flow's own, or that of the helper whose
    /// payload walk the op was composed from.
    pub file_id: FileId,
    pub range: TextRange,
    /// True if this op is contained inside a conditional/loop control-flow
    /// statement (if/elseif/else, while, repeat, for, generic-for) relative to
    /// the enclosing send/receive block. Dynamic ops represent `0..N`
    /// occurrences of `kind` rather than a single fixed occurrence.
    pub dynamic: bool,
    /// Bit-width literal, when the op declares a `gmod.net_payload`/`bits`
    /// parameter *and* the argument is a numeric literal. `None` when either is
    /// untrue, so it must not be used to decide whether the op takes a bit count
    /// — use [`NetOpEntry::has_bits_param`] for that. Surfaced in hover so
    /// callers can see at a glance how many bits flow on the wire.
    pub bits: Option<u32>,
    /// Whether the called signature declares a bit-width parameter, independent
    /// of whether the argument was a readable literal. Read completions need
    /// this to offer `net.ReadUInt(${1:bits})` when the writer used a variable.
    pub has_bits_param: bool,
    /// Source text of the value argument for `Write*` ops (the data being sent).
    /// Empty for `Read*` ops since reads have no value argument. Truncated to a
    /// short snippet so the hover can show "what is being written" without
    /// blowing up the popup with multi-line expressions. `None` when the value
    /// arg is missing, multi-line, or otherwise unsuitable for inline display.
    pub value_text: Option<String>,
    /// Stack of enclosing control-flow frames between the send/receive block
    /// and this op, ordered outer-to-inner. Captured at index time so hover
    /// can render the actual `if cond then` / `for k, v in pairs(t) do` /
    /// `while cond do` source text around each op rather than synthesized
    /// labels — gives developers exact control-flow context.
    pub flow_path: Vec<NetFlowFrame>,
}

impl Hash for NetOpEntry {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self {
            op,
            display_name,
            file_id,
            range,
            dynamic,
            bits,
            has_bits_param,
            value_text,
            flow_path,
        } = self;
        op.hash(state);
        display_name.hash(state);
        file_id.hash(state);
        range.hash_in_file(*file_id, state);
        dynamic.hash(state);
        bits.hash(state);
        has_bits_param.hash(state);
        value_text.hash(state);
        flow_path.hash(state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetFlowFrame {
    pub kind: NetFlowKind,
    /// Single-line summary of the statement opener: `if cond then`,
    /// `for i = 1, #items do`, `while running do`, etc. Truncated and
    /// whitespace-collapsed; `None` when the source isn't suitable for
    /// inline display (too long, multi-line, etc.).
    pub header: Option<String>,
    /// Stable id distinguishing two structurally identical frames at the
    /// same source span (e.g. two adjacent `if x then ... end` blocks).
    /// Range start of the statement node — different statements have
    /// different ranges, so equal `id` means literally the same source frame.
    pub id: u32,
    /// The file the statement is in: a composed walk prepends the caller's
    /// frames to the helper's.
    pub file_id: FileId,
}

impl Hash for NetFlowFrame {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self {
            kind,
            header,
            id,
            file_id,
        } = self;
        kind.hash(state);
        header.hash(state);
        file_id.hash(state);
        TextSize::from(*id).hash_in_file(*file_id, state);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetFlowKind {
    If,
    While,
    For,
    ForRange,
    Repeat,
}

impl NetFlowKind {
    pub fn keyword(self) -> &'static str {
        match self {
            NetFlowKind::If => "if",
            NetFlowKind::While => "while",
            NetFlowKind::For => "for",
            NetFlowKind::ForRange => "for",
            NetFlowKind::Repeat => "repeat",
        }
    }

    /// True when the construct may execute its body more than once.
    /// Used by hover to label loops as "may repeat" vs ifs as "may not run".
    pub fn is_loop(self) -> bool {
        matches!(
            self,
            NetFlowKind::While | NetFlowKind::For | NetFlowKind::ForRange | NetFlowKind::Repeat
        )
    }
}

/// A net send terminator, derived from the `net_send` signature attribute.
///
/// `receiver_realm` is the realm the payload arrives in, which is the only fact
/// the pairing logic needs. `target_arg_idx` is the zero-based index of the
/// recipient parameter, located through the `gmod.net_payload`/`target` call-arg
/// role; `None` for terminators with no recipient such as `net.Broadcast`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetSendKind {
    pub receiver_realm: GmodRealm,
    pub target_arg_idx: Option<usize>,
}

/// Definition-site transaction represented by a complete helper call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetFlowOrigin {
    pub file_id: FileId,
    pub start_range: TextRange,
}

impl Hash for NetFlowOrigin {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self {
            file_id,
            start_range,
        } = self;
        file_id.hash(state);
        start_range.hash_in_file(*file_id, state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetSendFlow {
    pub message_name: String,
    pub start_range: TextRange,
    pub writes: Vec<NetOpEntry>,
    pub send_range: TextRange,
    pub send_kind: NetSendKind,
    /// Source path of the send function actually called, e.g. `net.Broadcast`.
    /// Used for code lens labels in place of the deleted name table.
    pub send_display_name: SmolStr,
    /// Single-line snippet of the first argument to the send call (the
    /// recipient expression for `net.Send`/`net.SendOmit`/`net.SendPVS`/
    /// `net.SendPAS`). `None` for `net.Broadcast`/`net.SendToServer` (no
    /// recipient arg) or when the source is not suitable for inline display
    /// (multi-line, too long, missing, etc.). Surfaced in the code lens so
    /// developers can see at a glance who the message is targeted at without
    /// jumping to the call site.
    pub send_target: Option<String>,
    /// True for a conservative helper-definition flow whose complete call-site
    /// transaction is unknown. These flows are used for counterpart existence
    /// checks only; complete helper calls materialized at a call site are false.
    pub is_wrapped: bool,
    /// True when the transaction calls a function it receives as a parameter,
    /// such as a caller-supplied payload writer, so `writes` may be incomplete.
    /// The flow still counts as a sender but is skipped by payload checks.
    pub writes_opaque: bool,
    /// Definition-site flow replaced by this materialized call-site transaction.
    /// The index uses this to hide the definition as a duplicate sender while
    /// retaining it when no statically resolvable call site exists.
    pub materialized_from: Option<NetFlowOrigin>,
}

impl NetSendFlow {
    /// Whether `writes` is the transaction's whole payload, for payload
    /// checks to compare against.
    pub fn payload_known(&self) -> bool {
        !self.is_wrapped && !self.writes_opaque
    }
}

impl Hash for NetSendFlow {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self {
            message_name,
            start_range,
            writes,
            send_range,
            send_kind,
            send_display_name,
            send_target,
            is_wrapped,
            writes_opaque,
            materialized_from,
        } = self;
        message_name.hash(state);
        hash_local(start_range, state);
        writes.hash(state);
        hash_local(send_range, state);
        send_kind.hash(state);
        send_display_name.hash(state);
        send_target.hash(state);
        is_wrapped.hash(state);
        writes_opaque.hash(state);
        materialized_from.hash(state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetReceiveFlow {
    pub message_name: String,
    pub receive_range: TextRange,
    pub reads: Vec<NetOpEntry>,
    /// True when the callback body could not be resolved (e.g. the second
    /// argument is a name reference to a function defined in another file).
    /// Opaque flows are still recorded for counterpart presence checks but
    /// must be skipped for read/write mismatch diagnostics — we cannot
    /// inspect their reads, so any count comparison would be unreliable.
    pub reads_opaque: bool,
}

impl Hash for NetReceiveFlow {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Self {
            message_name,
            receive_range,
            reads,
            reads_opaque,
        } = self;
        message_name.hash(state);
        hash_local(receive_range, state);
        reads.hash(state);
        reads_opaque.hash(state);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct FileNetworkData {
    pub send_flows: Vec<NetSendFlow>,
    pub receive_flows: Vec<NetReceiveFlow>,
}

/// The flows one call starts, receives or materializes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CallFlows {
    pub send_flows: Vec<NetSendFlow>,
    pub receive_flows: Vec<NetReceiveFlow>,
}

impl CallFlows {
    pub fn is_empty(&self) -> bool {
        self.send_flows.is_empty() && self.receive_flows.is_empty()
    }
}

/// How a send flow ranks among the flows starting at one position: a flow
/// written directly, a wrapped definition's, then a materialized call's.
fn send_rank(flow: &NetSendFlow) -> u8 {
    if flow.is_wrapped {
        1
    } else if flow.materialized_from.is_some() {
        2
    } else {
        0
    }
}

/// A file's flows from those of its calls: sends in the order of their start,
/// receives in the order of their registration, each call's flows in the
/// order it filed them.
fn assemble_file_data(calls: &FxHashMap<TextRange, CallFlows>) -> FileNetworkData {
    let mut calls = calls.iter().collect::<Vec<_>>();
    calls.sort_unstable_by_key(|(call, _)| (call.start(), call.end()));
    let mut send_flows = Vec::new();
    let mut receive_flows = Vec::new();
    for (call, flows) in calls {
        let origin = (call.end(), call.start());
        for (idx, flow) in flows.send_flows.iter().enumerate() {
            send_flows.push((
                (flow.start_range.start(), send_rank(flow), origin, idx),
                flow,
            ));
        }
        for (idx, flow) in flows.receive_flows.iter().enumerate() {
            receive_flows.push(((flow.receive_range.start(), origin, idx), flow));
        }
    }
    send_flows.sort_by_key(|(key, _)| *key);
    receive_flows.sort_by_key(|(key, _)| *key);
    FileNetworkData {
        send_flows: send_flows
            .into_iter()
            .map(|(_, flow)| flow.clone())
            .collect(),
        receive_flows: receive_flows
            .into_iter()
            .map(|(_, flow)| flow.clone())
            .collect(),
    }
}

/// What one reference site in one file contributes to the net-helper name
/// closure: the written names of the functions enclosing the sites, and the
/// local declarations those functions are bound to (whose own references carry
/// the chain onward).
#[derive(Debug, Clone, Default)]
pub struct NetNameExpansion {
    pub names: Vec<SmolStr>,
    pub locals: Vec<LuaDeclId>,
}

#[derive(Debug, Default, Clone)]
pub struct GmodNetworkIndex {
    /// What each file's calls filed, by call range.
    call_flows: CowShards<FileId, Arc<FxHashMap<TextRange, CallFlows>>>,
    /// Each file's flows, assembled from its calls'.
    file_data: CowShards<FileId, FileNetworkData>,
    send_flows_by_message: CowShards<String, Vec<(FileId, usize)>>,
    receive_flows_by_message: CowShards<String, Vec<(FileId, usize)>>,
    materialized_definition_counts: CowShards<(FileId, TextRange), usize>,
    /// Each file with the other files whose flows name its positions.
    referencing: CowShards<FileId, FxHashSet<FileId>>,
    /// Canonical function metadata per `(wire_format, direction)`, derived from
    /// annotated signatures during analysis. Features that must *emit* a net call
    /// — read completions, "expected `x`, got `y`" messages when one side has no
    /// call to name — resolve it here instead of from a hardcoded table.
    /// Workspace-global and rebuilt per analyze pass, so it is not per-file state.
    canonical_ops: CowCell<FxHashMap<(SmolStr, NetOpDirection), CanonicalNetOp>>,
    /// The names of functions that can produce a net operation, directly or
    /// through other such functions.
    helper_names: Arc<FxHashSet<SmolStr>>,
    /// The names the net operations are declared under.
    op_names: Arc<FxHashSet<SmolStr>>,
    /// Incremental multiset fingerprint of every live file's network data.
    flows_fingerprint: Fingerprint,
    /// Fingerprint of `canonical_ops`, taken when the table is replaced.
    canonical_ops_fingerprint: Fingerprint,
}

fn note_sends_write(name: &str) {
    derivation::note_write(|| FactKey::NetSends(SmolStr::new(name)));
}

fn note_receives_write(name: &str) {
    derivation::note_write(|| FactKey::NetReceives(SmolStr::new(name)));
}

/// The fingerprint of the flows `indexed` names, each hashed as its file's
/// positions; `flows` picks a file's flows of the direction indexed.
fn messages_fingerprint<T: std::hash::Hash>(
    file_data: &CowShards<FileId, FileNetworkData>,
    indexed: Option<&Vec<(FileId, usize)>>,
    flows: impl Fn(&FileNetworkData) -> &Vec<T>,
) -> Fingerprint {
    indexed
        .into_iter()
        .flatten()
        .filter_map(|(file_id, flow_idx)| {
            let flow = flows(file_data.get(file_id)?).get(*flow_idx)?;
            Some(derivation::in_file(Some(*file_id), || {
                derivation::fingerprint_of(&(file_id, flow))
            }))
        })
        .fold(derivation::ABSENT, Fingerprint::wrapping_add)
}

fn note_definition_uses_write(file_id: FileId, start_range: TextRange) {
    derivation::note_write(|| {
        FactKey::NetDefinitionUses(crate::InFiled::new(file_id, start_range))
    });
}

fn file_data_fingerprint(file_id: FileId, data: &FileNetworkData) -> Fingerprint {
    derivation::in_file(Some(file_id), || {
        derivation::fingerprint_of(&(file_id, data))
    })
}

/// The files other than `file_id` whose positions its flows name: the ops and
/// frames composed from their helpers, and the definitions its calls
/// materialize.
fn named_files(file_id: FileId, data: &FileNetworkData) -> FxHashSet<FileId> {
    let entries = data
        .send_flows
        .iter()
        .flat_map(|flow| &flow.writes)
        .chain(data.receive_flows.iter().flat_map(|flow| &flow.reads));
    let mut files = entries
        .flat_map(|entry| {
            std::iter::once(entry.file_id).chain(entry.flow_path.iter().map(|frame| frame.file_id))
        })
        .chain(
            data.send_flows
                .iter()
                .filter_map(|flow| flow.materialized_from)
                .map(|origin| origin.file_id),
        )
        .collect::<FxHashSet<_>>();
    files.remove(&file_id);
    files
}

/// Whether a remap moved or lost anything it was asked about.
#[derive(Default)]
struct RemapOutcome {
    moved: bool,
    lost: bool,
}

impl RemapOutcome {
    /// Moves the range of a call in `file_id`.
    fn call_range(&mut self, remap: &FileRemap, file_id: FileId, range: &mut TextRange) {
        let call = glua_parser::LuaSyntaxId::new(
            glua_parser::LuaKind::Syntax(glua_parser::LuaSyntaxKind::CallExpr),
            *range,
        );
        match remap.syntax_id(file_id, call) {
            Remap::Moved(call) => {
                self.moved |= call.get_range() != *range;
                *range = call.get_range();
            }
            Remap::Lost => self.lost = true,
            Remap::Unrelated => {}
        }
    }

    fn entry(&mut self, remap: &FileRemap, entry: &mut NetOpEntry) {
        self.call_range(remap, entry.file_id, &mut entry.range);
        for frame in entry
            .flow_path
            .iter_mut()
            .filter(|frame| frame.file_id == remap.file_id)
        {
            match remap.map.map(TextSize::from(frame.id)) {
                Some(id) => {
                    self.moved |= u32::from(id) != frame.id;
                    frame.id = u32::from(id);
                }
                None => self.lost = true,
            }
        }
    }
}

/// `data` with the positions of `remap`'s file it names moved to their new
/// place.
fn remap_call_flows(data: &CallFlows, remap: &FileRemap) -> Remap<CallFlows> {
    let mut data = data.clone();
    let mut outcome = RemapOutcome::default();
    for flow in &mut data.send_flows {
        for entry in &mut flow.writes {
            outcome.entry(remap, entry);
        }
        if let Some(origin) = &mut flow.materialized_from {
            outcome.call_range(remap, origin.file_id, &mut origin.start_range);
        }
    }
    for entry in data
        .receive_flows
        .iter_mut()
        .flat_map(|flow| &mut flow.reads)
    {
        outcome.entry(remap, entry);
    }
    if outcome.lost {
        Remap::Lost
    } else if outcome.moved {
        Remap::Moved(data)
    } else {
        Remap::Unrelated
    }
}

impl GmodNetworkIndex {
    pub(crate) fn message_key(name: &str) -> String {
        name.to_ascii_lowercase()
    }

    pub fn new() -> Self {
        Self {
            call_flows: Default::default(),
            file_data: Default::default(),
            send_flows_by_message: Default::default(),
            receive_flows_by_message: Default::default(),
            materialized_definition_counts: Default::default(),
            referencing: Default::default(),
            canonical_ops: Default::default(),
            helper_names: Default::default(),
            op_names: Default::default(),
            flows_fingerprint: 0,
            canonical_ops_fingerprint: 0,
        }
    }

    pub(crate) fn fact_fingerprint(&self, key: &FactKey) -> Fingerprint {
        match key {
            FactKey::NetworkFile(file_id) => {
                derivation::fingerprint_opt(self.file_data.get(file_id))
            }
            FactKey::NetFlows(site) => self
                .call_flows
                .get(&site.file_id)
                .and_then(|calls| calls.get(&site.value))
                .map_or(derivation::ABSENT, |flows| {
                    derivation::in_file(Some(site.file_id), || {
                        derivation::fingerprint_of(&(site.file_id, flows))
                    })
                }),
            FactKey::NetOperationNames => {
                let mut names = self.op_names.iter().collect::<Vec<_>>();
                names.sort_unstable();
                derivation::fingerprint_of(&names)
            }
            FactKey::NetHelperName(name) => {
                if self.helper_names.contains(name) {
                    derivation::fingerprint_of(name)
                } else {
                    derivation::ABSENT
                }
            }
            FactKey::NetworkScan if derivation::mapping_back() => self
                .file_data
                .iter()
                .fold(0, |sum: Fingerprint, (file_id, data)| {
                    sum.wrapping_add(file_data_fingerprint(*file_id, data))
                })
                .wrapping_add(self.canonical_ops_fingerprint),
            FactKey::NetworkScan => self
                .flows_fingerprint
                .wrapping_add(self.canonical_ops_fingerprint),
            FactKey::NetSends(name) => messages_fingerprint(
                &self.file_data,
                self.send_flows_by_message.get(name.as_str()),
                |data| &data.send_flows,
            ),
            FactKey::NetReceives(name) => messages_fingerprint(
                &self.file_data,
                self.receive_flows_by_message.get(name.as_str()),
                |data| &data.receive_flows,
            ),
            FactKey::NetDefinitionUses(site) => {
                if self
                    .materialized_definition_counts
                    .contains_key(&(site.file_id, site.value))
                {
                    derivation::fingerprint_of(&true)
                } else {
                    derivation::ABSENT
                }
            }
            _ => {
                debug_assert!(false, "{key:?} is not a gmod network fact");
                derivation::ABSENT
            }
        }
    }

    /// Replaces the names the net operations are declared under.
    pub(crate) fn set_op_names(&mut self, names: FxHashSet<SmolStr>) {
        if *self.op_names != names {
            derivation::note_write(|| FactKey::NetOperationNames);
            self.op_names = Arc::new(names);
        }
    }

    /// The names the net operations are declared under.
    pub(crate) fn op_names(&self) -> Arc<FxHashSet<SmolStr>> {
        derivation::note_read(|| FactKey::NetOperationNames);
        Arc::clone(&self.op_names)
    }

    /// Replaces the net-helper names, writing each name that joined or left.
    pub(crate) fn set_helper_names(&mut self, names: FxHashSet<SmolStr>) {
        if derivation::recording() {
            for name in names.symmetric_difference(&self.helper_names) {
                derivation::note_write(|| FactKey::NetHelperName(name.clone()));
            }
        }
        self.helper_names = Arc::new(names);
    }

    /// Whether `name` is a net-helper name, read as that name.
    pub(crate) fn is_helper_name(&self, name: &str) -> bool {
        derivation::note_read(|| FactKey::NetHelperName(SmolStr::new(name)));
        self.helper_names.contains(name)
    }

    /// Replaces the canonical op table. Called once per analyze pass with
    /// metadata collected from annotated signatures.
    pub fn set_canonical_ops(
        &mut self,
        canonical_ops: FxHashMap<(SmolStr, NetOpDirection), CanonicalNetOp>,
    ) {
        derivation::note_write(|| FactKey::NetworkScan);
        self.canonical_ops_fingerprint = canonical_ops.iter().fold(0, |sum: Fingerprint, entry| {
            sum.wrapping_add(derivation::fingerprint_of(&entry))
        });
        self.canonical_ops = canonical_ops.into();
    }

    pub fn canonical_op(
        &self,
        wire_format: &str,
        direction: NetOpDirection,
    ) -> Option<&CanonicalNetOp> {
        derivation::note_read(|| FactKey::NetworkScan);
        self.canonical_ops
            .get(&(SmolStr::new(wire_format), direction))
    }

    pub fn canonical_op_name(&self, wire_format: &str, direction: NetOpDirection) -> Option<&str> {
        self.canonical_op(wire_format, direction)
            .map(|op| op.name.as_str())
    }

    /// Canonical name of the read that consumes `write`, when one is annotated.
    pub fn counterpart_read_name(&self, write: &NetOpDescriptor) -> Option<&str> {
        self.canonical_op_name(&write.wire_format, write.direction.opposite())
    }

    /// Every annotated wire format, paired with whether a write and a read exist
    /// for it. Used by the coverage test that guards against a typo silently
    /// breaking pairing for one op.
    pub fn wire_format_coverage(&self) -> HashMap<SmolStr, (bool, bool)> {
        derivation::note_read(|| FactKey::NetworkScan);
        let mut coverage: HashMap<SmolStr, (bool, bool)> = HashMap::default();
        for (wire_format, direction) in self.canonical_ops.keys() {
            let entry = coverage
                .entry(wire_format.clone())
                .or_insert((false, false));
            match direction {
                NetOpDirection::Write => entry.0 = true,
                NetOpDirection::Read => entry.1 = true,
            }
        }
        coverage
    }

    /// Files the flows the call at `call` of `file_id` starts, receives or
    /// materializes.
    pub fn set_call_flows(&mut self, file_id: FileId, call: TextRange, flows: CallFlows) {
        if flows.is_empty() {
            self.reset_call_flows(file_id, call);
            return;
        }
        self.insert_call_flows(file_id, call, flows);
        self.assemble_file(file_id);
    }

    fn insert_call_flows(&mut self, file_id: FileId, call: TextRange, flows: CallFlows) {
        derivation::note_write(|| FactKey::NetFlows(crate::InFiled::new(file_id, call)));
        Arc::make_mut(self.call_flows.entry(file_id).or_default()).insert(call, flows);
    }

    /// Drops the flows the call at `call` of `file_id` filed, before the
    /// derivation that found them runs again.
    pub(crate) fn reset_call_flows(&mut self, file_id: FileId, call: TextRange) {
        derivation::note_write(|| FactKey::NetFlows(crate::InFiled::new(file_id, call)));
        let Some(calls) = self.call_flows.get_mut(&file_id) else {
            return;
        };
        if !calls.contains_key(&call) {
            return;
        }
        let calls = Arc::make_mut(calls);
        calls.remove(&call);
        if calls.is_empty() {
            self.call_flows.remove(&file_id);
        }
        self.assemble_file(file_id);
    }

    /// Replaces the file's flows with those of its calls.
    fn assemble_file(&mut self, file_id: FileId) {
        derivation::note_write(|| FactKey::NetworkFile(file_id));
        if let Some(data) = self.file_data.remove(&file_id) {
            self.remove_file_data_indexes(file_id, &data);
        }
        let Some(calls) = self.call_flows.get(&file_id) else {
            return;
        };
        let data = assemble_file_data(calls);
        self.index_file_data(file_id, &data);
        self.file_data.insert(file_id, data);
    }

    pub fn get_file_data(&self, file_id: FileId) -> Option<&FileNetworkData> {
        derivation::note_read(|| FactKey::NetworkFile(file_id));
        self.file_data.get(&file_id)
    }

    /// File ids in canonical order. `file_data` is a `RandomState` map, so
    /// iterating it directly yields a different flow order every process run.
    fn ordered_file_ids(&self) -> Vec<FileId> {
        let mut file_ids: Vec<FileId> = self.file_data.keys().copied().collect();
        file_ids.sort_unstable_by_key(|file_id| file_id.id);
        file_ids
    }

    pub fn iter_send_flows(&self) -> impl Iterator<Item = (FileId, usize, &NetSendFlow)> {
        derivation::note_read(|| FactKey::NetworkScan);
        self.ordered_file_ids()
            .into_iter()
            .flat_map(move |file_id| {
                self.file_data
                    .get(&file_id)
                    .into_iter()
                    .flat_map(move |data| {
                        data.send_flows
                            .iter()
                            .enumerate()
                            .filter(move |(_, flow)| !self.is_replaced_definition(file_id, flow))
                            .map(move |(flow_idx, flow)| (file_id, flow_idx, flow))
                    })
            })
    }

    pub fn iter_receive_flows(&self) -> impl Iterator<Item = (FileId, &NetReceiveFlow)> {
        derivation::note_read(|| FactKey::NetworkScan);
        self.ordered_file_ids()
            .into_iter()
            .flat_map(move |file_id| {
                self.file_data
                    .get(&file_id)
                    .into_iter()
                    .flat_map(move |data| {
                        data.receive_flows.iter().map(move |flow| (file_id, flow))
                    })
            })
    }

    /// Notes a read of the flows sent under `name`.
    pub(crate) fn note_sends_read(name: &str) {
        derivation::note_read(|| FactKey::NetSends(SmolStr::new(name)));
    }

    pub fn get_send_flows_for_message(&self, name: &str) -> Vec<(FileId, &NetSendFlow)> {
        Self::note_sends_read(name);
        let mut flows: Vec<_> = self
            .send_flows_by_message
            .get(name)
            .into_iter()
            .flat_map(|indexed_flows| indexed_flows.iter())
            .filter_map(|(file_id, flow_idx)| {
                self.file_data
                    .get(file_id)
                    .and_then(|file_data| file_data.send_flows.get(*flow_idx))
                    .map(|flow| (*file_id, *flow_idx, flow))
            })
            .filter(|(file_id, _, flow)| !self.is_replaced_definition(*file_id, flow))
            .collect();

        flows.sort_by_key(|(file_id, flow_idx, flow)| {
            (
                file_id.id,
                u32::from(flow.start_range.start()),
                u32::from(flow.send_range.start()),
                *flow_idx,
            )
        });
        flows
            .into_iter()
            .map(|(file_id, _flow_idx, flow)| (file_id, flow))
            .collect()
    }

    pub fn get_receive_flows_for_message(&self, name: &str) -> Vec<(FileId, &NetReceiveFlow)> {
        derivation::note_read(|| FactKey::NetReceives(SmolStr::new(name)));
        let mut flows: Vec<_> = self
            .receive_flows_by_message
            .get(name)
            .into_iter()
            .flat_map(|indexed_flows| indexed_flows.iter())
            .filter_map(|(file_id, flow_idx)| {
                self.file_data
                    .get(file_id)
                    .and_then(|file_data| file_data.receive_flows.get(*flow_idx))
                    .map(|flow| (*file_id, *flow_idx, flow))
            })
            .collect();

        flows.sort_by_key(|(file_id, flow_idx, flow)| {
            (file_id.id, u32::from(flow.receive_range.start()), *flow_idx)
        });
        flows
            .into_iter()
            .map(|(file_id, _flow_idx, flow)| (file_id, flow))
            .collect()
    }

    pub fn remove_file(&mut self, file_id: FileId) {
        derivation::note_write(|| FactKey::NetworkFile(file_id));
        if let Some(calls) = self.call_flows.remove(&file_id)
            && derivation::recording()
        {
            for call in calls.keys() {
                derivation::note_write(|| FactKey::NetFlows(crate::InFiled::new(file_id, *call)));
            }
        }
        if let Some(data) = self.file_data.remove(&file_id) {
            self.remove_file_data_indexes(file_id, &data);
        }
    }

    /// Carries the positions of `remap`'s file that other files' flows name
    /// to their new place. Returns the flows of each file that named one the
    /// edit destroyed.
    pub(crate) fn remap_file_positions(&mut self, remap: &FileRemap) -> Vec<crate::LostFact> {
        let mut files = self
            .referencing
            .get(&remap.file_id)
            .map(|files| files.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        files.sort_unstable();
        let mut lost = Vec::new();
        for file_id in files {
            let Some(calls) = self.call_flows.get(&file_id) else {
                continue;
            };
            let mut calls = calls.iter().collect::<Vec<_>>();
            calls.sort_unstable_by_key(|(call, _)| (call.start(), call.end()));
            let mut moved = Vec::new();
            for (call, flows) in calls {
                match remap_call_flows(flows, remap) {
                    Remap::Moved(flows) => moved.push((*call, flows)),
                    Remap::Lost => lost.push(crate::LostFact::new(
                        file_id,
                        FactKey::NetFlows(crate::InFiled::new(file_id, *call)),
                    )),
                    Remap::Unrelated => {}
                }
            }
            if moved.is_empty() {
                continue;
            }
            for (call, flows) in moved {
                self.insert_call_flows(file_id, call, flows);
            }
            self.assemble_file(file_id);
        }
        lost
    }

    pub fn clear(&mut self) {
        if derivation::recording() {
            for (file_id, calls) in self.call_flows.iter() {
                for call in calls.keys() {
                    derivation::note_write(|| {
                        FactKey::NetFlows(crate::InFiled::new(*file_id, *call))
                    });
                }
            }
            for file_id in self.file_data.keys() {
                derivation::note_write(|| FactKey::NetworkFile(*file_id));
            }
            for name in self.send_flows_by_message.keys() {
                note_sends_write(name);
            }
            for name in self.receive_flows_by_message.keys() {
                note_receives_write(name);
            }
            for (file_id, start_range) in self.materialized_definition_counts.keys() {
                note_definition_uses_write(*file_id, *start_range);
            }
        }
        derivation::note_write(|| FactKey::NetworkScan);
        self.flows_fingerprint = 0;
        self.canonical_ops_fingerprint = 0;
        self.call_flows.clear();
        self.file_data.clear();
        self.send_flows_by_message.clear();
        self.receive_flows_by_message.clear();
        self.materialized_definition_counts.clear();
        self.referencing.clear();
        self.canonical_ops.clear();
        if derivation::recording() {
            for name in self.helper_names.iter() {
                derivation::note_write(|| FactKey::NetHelperName(name.clone()));
            }
        }
        self.helper_names = Arc::default();
        if !self.op_names.is_empty() {
            derivation::note_write(|| FactKey::NetOperationNames);
        }
        self.op_names = Arc::default();
    }

    fn index_file_data(&mut self, file_id: FileId, data: &FileNetworkData) {
        derivation::note_write(|| FactKey::NetworkScan);
        self.flows_fingerprint = self
            .flows_fingerprint
            .wrapping_add(file_data_fingerprint(file_id, data));
        for named in named_files(file_id, data) {
            self.referencing.entry(named).or_default().insert(file_id);
        }
        for (flow_idx, send_flow) in data.send_flows.iter().enumerate() {
            note_sends_write(&send_flow.message_name);
            self.send_flows_by_message
                .entry(send_flow.message_name.clone())
                .or_default()
                .push((file_id, flow_idx));
            if let Some(origin) = send_flow.materialized_from {
                let uses = self
                    .materialized_definition_counts
                    .entry((origin.file_id, origin.start_range))
                    .or_default();
                if *uses == 0 {
                    note_definition_uses_write(origin.file_id, origin.start_range);
                }
                *uses += 1;
            }
        }

        for (flow_idx, receive_flow) in data.receive_flows.iter().enumerate() {
            note_receives_write(&receive_flow.message_name);
            self.receive_flows_by_message
                .entry(receive_flow.message_name.clone())
                .or_default()
                .push((file_id, flow_idx));
        }
    }

    fn remove_file_data_indexes(&mut self, file_id: FileId, data: &FileNetworkData) {
        derivation::note_write(|| FactKey::NetworkScan);
        self.flows_fingerprint = self
            .flows_fingerprint
            .wrapping_sub(file_data_fingerprint(file_id, data));
        for named in named_files(file_id, data) {
            if let Some(files) = self.referencing.get_mut(&named) {
                files.remove(&file_id);
                if files.is_empty() {
                    self.referencing.remove(&named);
                }
            }
        }
        for send_flow in &data.send_flows {
            note_sends_write(&send_flow.message_name);
            if let Some(origin) = send_flow.materialized_from {
                let key = (origin.file_id, origin.start_range);
                if let Some(count) = self.materialized_definition_counts.get_mut(&key) {
                    *count -= 1;
                    if *count == 0 {
                        self.materialized_definition_counts.remove(&key);
                        note_definition_uses_write(origin.file_id, origin.start_range);
                    }
                }
            }
            let mut remove_message_entry = false;
            if let Some(indexed_flows) = self.send_flows_by_message.get_mut(&send_flow.message_name)
            {
                indexed_flows.retain(|(candidate_file_id, _)| *candidate_file_id != file_id);
                remove_message_entry = indexed_flows.is_empty();
            }
            if remove_message_entry {
                self.send_flows_by_message.remove(&send_flow.message_name);
            }
        }

        for receive_flow in &data.receive_flows {
            note_receives_write(&receive_flow.message_name);
            let mut remove_message_entry = false;
            if let Some(indexed_flows) = self
                .receive_flows_by_message
                .get_mut(&receive_flow.message_name)
            {
                indexed_flows.retain(|(candidate_file_id, _)| *candidate_file_id != file_id);
                remove_message_entry = indexed_flows.is_empty();
            }
            if remove_message_entry {
                self.receive_flows_by_message
                    .remove(&receive_flow.message_name);
            }
        }
    }

    pub(crate) fn is_replaced_definition(&self, file_id: FileId, flow: &NetSendFlow) -> bool {
        derivation::note_read(|| {
            FactKey::NetDefinitionUses(crate::InFiled::new(file_id, flow.start_range))
        });
        flow.materialized_from.is_none()
            && self
                .materialized_definition_counts
                .contains_key(&(file_id, flow.start_range))
    }
}

impl LuaIndex for GmodNetworkIndex {
    fn remove(&mut self, file_id: FileId) {
        self.remove_file(file_id);
    }

    fn clear(&mut self) {
        GmodNetworkIndex::clear(self);
    }
}

#[cfg(test)]
mod tests {
    use rowan::{TextRange, TextSize};

    use super::*;

    fn range(start: u32) -> TextRange {
        TextRange::new(TextSize::new(start), TextSize::new(start + 1))
    }

    impl GmodNetworkIndex {
        /// Replaces the file's flows with `data`, each flow filed by the call
        /// it starts or registers at.
        fn add_file_data(&mut self, file_id: FileId, data: FileNetworkData) {
            self.remove_file(file_id);
            let mut calls = FxHashMap::<TextRange, CallFlows>::default();
            for flow in data.send_flows {
                calls
                    .entry(flow.start_range)
                    .or_default()
                    .send_flows
                    .push(flow);
            }
            for flow in data.receive_flows {
                calls
                    .entry(flow.receive_range)
                    .or_default()
                    .receive_flows
                    .push(flow);
            }
            let mut calls = calls.into_iter().collect::<Vec<_>>();
            calls.sort_by_key(|(call, _)| call.start());
            for (call, flows) in calls {
                self.set_call_flows(file_id, call, flows);
            }
        }
    }

    fn send_flow(message_name: &str, start: u32) -> NetSendFlow {
        NetSendFlow {
            message_name: message_name.to_string(),
            start_range: range(start),
            writes: Vec::new(),
            send_range: range(start + 10),
            send_kind: NetSendKind {
                receiver_realm: GmodRealm::Client,
                target_arg_idx: None,
            },
            send_display_name: SmolStr::new_static("net.Broadcast"),
            send_target: None,
            is_wrapped: false,
            writes_opaque: false,
            materialized_from: None,
        }
    }

    fn receive_flow(message_name: &str, start: u32) -> NetReceiveFlow {
        NetReceiveFlow {
            message_name: message_name.to_string(),
            receive_range: range(start),
            reads: Vec::new(),
            reads_opaque: false,
        }
    }

    /// The installed state a batch install has to reproduce, in a stable
    /// order; message lists are read as sets, as their readers sort them.
    fn installed_state(index: &GmodNetworkIndex) -> Vec<String> {
        let as_set = |flows: &Vec<(FileId, usize)>| {
            let mut flows = flows.clone();
            flows.sort();
            flows
        };
        let mut state = index
            .send_flows_by_message
            .iter()
            .map(|(name, flows)| format!("send {name} {:?}", as_set(flows)))
            .collect::<Vec<_>>();
        state.extend(
            index
                .receive_flows_by_message
                .iter()
                .map(|(name, flows)| format!("receive {name} {:?}", as_set(flows))),
        );
        state.extend(
            index
                .file_data
                .iter()
                .map(|entry| format!("file {entry:?}")),
        );
        state.push(format!("flows {}", index.flows_fingerprint));
        state.sort();
        state
    }

    #[test]
    fn installing_forks_field_by_field_matches_installing_them_one_by_one() {
        use crate::db_index::cow_shards::TaskFork;
        let mut base = GmodNetworkIndex::new();
        base.add_file_data(
            FileId::new(1),
            FileNetworkData {
                send_flows: vec![send_flow("Shared", 1)],
                receive_flows: vec![receive_flow("Shared", 5)],
            },
        );
        let frozen = base.task_fork();
        let forks = (2..6)
            .map(|file| {
                let mut fork = frozen.task_fork();
                fork.add_file_data(
                    FileId::new(file),
                    FileNetworkData {
                        send_flows: vec![send_flow("Shared", file * 10)],
                        receive_flows: vec![receive_flow("Other", file * 10 + 5)],
                    },
                );
                fork
            })
            .collect::<Vec<_>>();
        let mut one_by_one = base.clone();
        let mut one_by_one_conflicts = vec![Vec::new(); forks.len()];
        for (at, fork) in forks.iter().cloned().enumerate() {
            one_by_one.install_task_reporting(&frozen, fork, &mut one_by_one_conflicts[at]);
        }
        let mut by_field = base.clone();
        let mut by_field_conflicts = vec![Vec::new(); forks.len()];
        by_field.install_tasks_reporting(
            &frozen,
            forks.into_iter().enumerate().collect(),
            &mut by_field_conflicts,
        );

        assert_eq!(
            (installed_state(&by_field), by_field_conflicts),
            (installed_state(&one_by_one), one_by_one_conflicts)
        );
    }

    #[test]
    fn add_file_data_replaces_previous_message_indexes_for_same_file() {
        let file_id = FileId::new(1);
        let mut index = GmodNetworkIndex::new();
        index.add_file_data(
            file_id,
            FileNetworkData {
                send_flows: vec![send_flow("OldMessage", 1)],
                receive_flows: Vec::new(),
            },
        );

        assert_eq!(index.get_send_flows_for_message("OldMessage").len(), 1);

        index.add_file_data(
            file_id,
            FileNetworkData {
                send_flows: vec![send_flow("NewMessage", 20)],
                receive_flows: Vec::new(),
            },
        );

        assert!(index.get_send_flows_for_message("OldMessage").is_empty());
        assert_eq!(index.get_send_flows_for_message("NewMessage").len(), 1);
    }

    #[test]
    fn remove_file_cleans_send_and_receive_indexes() {
        let file_id = FileId::new(2);
        let mut index = GmodNetworkIndex::new();
        index.add_file_data(
            file_id,
            FileNetworkData {
                send_flows: vec![send_flow("CleanupSend", 1)],
                receive_flows: vec![receive_flow("CleanupReceive", 2)],
            },
        );

        assert_eq!(index.get_send_flows_for_message("CleanupSend").len(), 1);
        assert_eq!(
            index.get_receive_flows_for_message("CleanupReceive").len(),
            1
        );

        index.remove_file(file_id);

        assert!(index.get_send_flows_for_message("CleanupSend").is_empty());
        assert!(
            index
                .get_receive_flows_for_message("CleanupReceive")
                .is_empty()
        );
    }

    #[test]
    fn materialized_call_replaces_definition_until_call_file_is_removed() {
        let definition_file_id = FileId::new(3);
        let call_file_id = FileId::new(4);
        let definition_start = range(10);
        let mut materialized = send_flow("WrappedMessage", 30);
        materialized.materialized_from = Some(NetFlowOrigin {
            file_id: definition_file_id,
            start_range: definition_start,
        });

        let mut index = GmodNetworkIndex::new();
        index.add_file_data(
            definition_file_id,
            FileNetworkData {
                send_flows: vec![send_flow("WrappedMessage", 10)],
                receive_flows: Vec::new(),
            },
        );
        index.add_file_data(
            call_file_id,
            FileNetworkData {
                send_flows: vec![materialized],
                receive_flows: Vec::new(),
            },
        );

        let flows = index.get_send_flows_for_message("WrappedMessage");
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].0, call_file_id);

        index.remove_file(call_file_id);
        let flows = index.get_send_flows_for_message("WrappedMessage");
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].0, definition_file_id);
    }

    #[test]
    fn message_lookup_returns_flows_from_multiple_files() {
        let mut index = GmodNetworkIndex::new();
        index.add_file_data(
            FileId::new(1),
            FileNetworkData {
                send_flows: vec![send_flow("SharedMessage", 1)],
                receive_flows: Vec::new(),
            },
        );
        index.add_file_data(
            FileId::new(2),
            FileNetworkData {
                send_flows: vec![send_flow("SharedMessage", 3)],
                receive_flows: Vec::new(),
            },
        );

        let flows = index.get_send_flows_for_message("SharedMessage");
        assert_eq!(flows.len(), 2);
    }
}

crate::db_index::cow_shards::task_fork_fields!(GmodNetworkIndex {
    call_flows,
    file_data,
    send_flows_by_message => crate::db_index::cow_shards::merge_list,
    receive_flows_by_message => crate::db_index::cow_shards::merge_list,
    materialized_definition_counts => crate::db_index::cow_shards::merge_sum,
    referencing => crate::db_index::cow_shards::merge_set,
    canonical_ops,
    helper_names,
    op_names,
    flows_fingerprint => crate::db_index::cow_shards::merge_sum,
    canonical_ops_fingerprint,
} by_field);
