use std::collections::HashMap;

use rowan::TextRange;
use smol_str::SmolStr;

use super::LuaIndex;
use crate::{FileId, GmodRealm};

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
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetSendKind {
    pub receiver_realm: GmodRealm,
    pub target_arg_idx: Option<usize>,
}

/// Definition-site transaction represented by a complete helper call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetFlowOrigin {
    pub file_id: FileId,
    pub start_range: TextRange,
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
    /// Definition-site flow replaced by this materialized call-site transaction.
    /// The index uses this to hide the definition as a duplicate sender while
    /// retaining it when no statically resolvable call site exists.
    pub materialized_from: Option<NetFlowOrigin>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileNetworkData {
    pub send_flows: Vec<NetSendFlow>,
    pub receive_flows: Vec<NetReceiveFlow>,
}

#[derive(Debug, Default)]
pub struct GmodNetworkIndex {
    file_data: HashMap<FileId, FileNetworkData>,
    send_flows_by_message: HashMap<String, Vec<(FileId, usize)>>,
    receive_flows_by_message: HashMap<String, Vec<(FileId, usize)>>,
    materialized_definition_counts: HashMap<(FileId, TextRange), usize>,
    /// Canonical function metadata per `(wire_format, direction)`, derived from
    /// annotated signatures during analysis. Features that must *emit* a net call
    /// — read completions, "expected `x`, got `y`" messages when one side has no
    /// call to name — resolve it here instead of from a hardcoded table.
    /// Workspace-global and rebuilt per analyze pass, so it is not per-file state.
    canonical_ops: HashMap<(SmolStr, NetOpDirection), CanonicalNetOp>,
}

impl GmodNetworkIndex {
    pub fn new() -> Self {
        Self {
            file_data: HashMap::new(),
            send_flows_by_message: HashMap::new(),
            receive_flows_by_message: HashMap::new(),
            materialized_definition_counts: HashMap::new(),
            canonical_ops: HashMap::new(),
        }
    }

    /// Replaces the canonical op table. Called once per analyze pass with
    /// metadata collected from annotated signatures.
    pub fn set_canonical_ops(
        &mut self,
        canonical_ops: HashMap<(SmolStr, NetOpDirection), CanonicalNetOp>,
    ) {
        self.canonical_ops = canonical_ops;
    }

    pub fn canonical_op(
        &self,
        wire_format: &str,
        direction: NetOpDirection,
    ) -> Option<&CanonicalNetOp> {
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
        let mut coverage: HashMap<SmolStr, (bool, bool)> = HashMap::new();
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

    pub fn add_file_data(&mut self, file_id: FileId, data: FileNetworkData) {
        self.remove_file(file_id);
        self.index_file_data(file_id, &data);
        self.file_data.insert(file_id, data);
    }

    pub fn get_file_data(&self, file_id: FileId) -> Option<&FileNetworkData> {
        self.file_data.get(&file_id)
    }

    pub fn iter_all(&self) -> impl Iterator<Item = (FileId, &FileNetworkData)> {
        self.file_data
            .iter()
            .map(|(file_id, data)| (*file_id, data))
    }

    pub fn iter_send_flows(&self) -> impl Iterator<Item = (FileId, &NetSendFlow)> {
        self.file_data.iter().flat_map(move |(file_id, data)| {
            data.send_flows
                .iter()
                .filter(move |flow| !self.is_replaced_definition(*file_id, flow))
                .map(move |flow| (*file_id, flow))
        })
    }

    pub fn iter_receive_flows(&self) -> impl Iterator<Item = (FileId, &NetReceiveFlow)> {
        self.file_data
            .iter()
            .flat_map(|(file_id, data)| data.receive_flows.iter().map(move |flow| (*file_id, flow)))
    }

    pub fn get_send_flows_for_message(&self, name: &str) -> Vec<(FileId, &NetSendFlow)> {
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
        if let Some(data) = self.file_data.remove(&file_id) {
            self.remove_file_data_indexes(file_id, &data);
        }
    }

    pub fn clear(&mut self) {
        self.file_data.clear();
        self.send_flows_by_message.clear();
        self.receive_flows_by_message.clear();
        self.materialized_definition_counts.clear();
        self.canonical_ops.clear();
    }

    fn index_file_data(&mut self, file_id: FileId, data: &FileNetworkData) {
        for (flow_idx, send_flow) in data.send_flows.iter().enumerate() {
            self.send_flows_by_message
                .entry(send_flow.message_name.clone())
                .or_default()
                .push((file_id, flow_idx));
            if let Some(origin) = send_flow.materialized_from {
                *self
                    .materialized_definition_counts
                    .entry((origin.file_id, origin.start_range))
                    .or_default() += 1;
            }
        }

        for (flow_idx, receive_flow) in data.receive_flows.iter().enumerate() {
            self.receive_flows_by_message
                .entry(receive_flow.message_name.clone())
                .or_default()
                .push((file_id, flow_idx));
        }
    }

    fn remove_file_data_indexes(&mut self, file_id: FileId, data: &FileNetworkData) {
        for send_flow in &data.send_flows {
            if let Some(origin) = send_flow.materialized_from {
                let key = (origin.file_id, origin.start_range);
                if let Some(count) = self.materialized_definition_counts.get_mut(&key) {
                    *count -= 1;
                    if *count == 0 {
                        self.materialized_definition_counts.remove(&key);
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
