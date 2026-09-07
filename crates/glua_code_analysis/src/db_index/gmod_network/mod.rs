use rustc_hash::FxHashMap;
use std::collections::HashMap;

use rowan::TextRange;
use smol_str::SmolStr;

use super::{LuaDeclId, LuaIndex};
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

/// What one reference site in one file contributes to the net-helper name
/// closure: the written names of the functions enclosing the sites, and the
/// local declarations those functions are bound to (whose own references carry
/// the chain onward).
#[derive(Debug, Clone, Default)]
pub struct NetNameExpansion {
    pub names: Vec<SmolStr>,
    pub locals: Vec<LuaDeclId>,
}

/// Memo of the per-file step of the net-helper name closure.
///
/// The step is a pure function of one file's syntax tree and one file's own
/// references, so an entry stays valid until that file's reference revision
/// moves — which covers a re-index, a removal, and a late cross-file reference
/// written by a retained unresolve alike.
#[derive(Debug, Default)]
pub struct NetHelperNameMemo {
    files: FxHashMap<FileId, NetHelperFileMemo>,
}

#[derive(Debug, Default)]
struct NetHelperFileMemo {
    revision: u64,
    names: FxHashMap<SmolStr, NetNameExpansion>,
    decls: FxHashMap<LuaDeclId, NetNameExpansion>,
}

impl NetHelperNameMemo {
    fn valid(&self, file_id: FileId, revision: u64) -> Option<&NetHelperFileMemo> {
        self.files
            .get(&file_id)
            .filter(|memo| memo.revision == revision)
    }

    fn entry(&mut self, file_id: FileId, revision: u64) -> &mut NetHelperFileMemo {
        let memo = self.files.entry(file_id).or_default();
        if memo.revision != revision {
            *memo = NetHelperFileMemo {
                revision,
                ..Default::default()
            };
        }
        memo
    }

    pub fn name(
        &self,
        file_id: FileId,
        revision: u64,
        name: &SmolStr,
    ) -> Option<&NetNameExpansion> {
        self.valid(file_id, revision)?.names.get(name)
    }

    pub fn set_name(
        &mut self,
        file_id: FileId,
        revision: u64,
        name: SmolStr,
        expansion: NetNameExpansion,
    ) {
        self.entry(file_id, revision).names.insert(name, expansion);
    }

    pub fn decl(
        &self,
        file_id: FileId,
        revision: u64,
        decl_id: &LuaDeclId,
    ) -> Option<&NetNameExpansion> {
        self.valid(file_id, revision)?.decls.get(decl_id)
    }

    pub fn set_decl(
        &mut self,
        file_id: FileId,
        revision: u64,
        decl_id: LuaDeclId,
        expansion: NetNameExpansion,
    ) {
        self.entry(file_id, revision)
            .decls
            .insert(decl_id, expansion);
    }
}

/// Inputs a file's network flows were collected from (B8).
///
/// The network pass recollects its batch every analyze; files whose own tree,
/// expanded helpers and helper-registry fold are all unchanged keep their
/// stored flows instead. `helper_contents` is sorted by `(file id, range)` so
/// the comparison is deterministic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkFileInputs {
    /// Hash of the file's own content at collect time.
    pub content_hash: u64,
    /// Identity of the helper-registry/call-role fold the collection
    /// resolved helpers through (the fold's `Arc` address: a reused fold
    /// keeps its allocation, a rebuild does not).
    pub registry_identity: usize,
    /// Cross-file helper bodies expanded while collecting, with their
    /// content hashes at collect time. Same-file expansions are covered by
    /// `content_hash`.
    pub helper_contents: Vec<(FileId, TextRange, Option<u64>)>,
}

#[derive(Debug, Default)]
pub struct GmodNetworkIndex {
    file_data: FxHashMap<FileId, FileNetworkData>,
    file_inputs: FxHashMap<FileId, NetworkFileInputs>,
    /// Data (plus inputs) parked by `remove` while its file is re-analysed.
    /// `remove` runs before every re-index, so dropping here would force the
    /// network pass to recollect files whose inputs never changed; the pass
    /// restores retained entries and prunes the rest (deleted files).
    staged_data: FxHashMap<FileId, FileNetworkData>,
    staged_inputs: FxHashMap<FileId, NetworkFileInputs>,
    send_flows_by_message: FxHashMap<String, Vec<(FileId, usize)>>,
    receive_flows_by_message: FxHashMap<String, Vec<(FileId, usize)>>,
    materialized_definition_counts: FxHashMap<(FileId, TextRange), usize>,
    /// Canonical function metadata per `(wire_format, direction)`, derived from
    /// annotated signatures during analysis. Features that must *emit* a net call
    /// — read completions, "expected `x`, got `y`" messages when one side has no
    /// call to name — resolve it here instead of from a hardcoded table.
    /// Workspace-global and rebuilt per analyze pass, so it is not per-file state.
    canonical_ops: FxHashMap<(SmolStr, NetOpDirection), CanonicalNetOp>,
    /// See [`NetHelperNameMemo`]. Kept here so it survives across analyze
    /// passes; its own revision guard, not this index's lifecycle, decides
    /// whether an entry is usable.
    helper_name_memo: NetHelperNameMemo,
}

impl GmodNetworkIndex {
    pub fn new() -> Self {
        Self {
            file_data: FxHashMap::default(),
            file_inputs: FxHashMap::default(),
            staged_data: FxHashMap::default(),
            staged_inputs: FxHashMap::default(),
            send_flows_by_message: FxHashMap::default(),
            receive_flows_by_message: FxHashMap::default(),
            materialized_definition_counts: FxHashMap::default(),
            canonical_ops: FxHashMap::default(),
            helper_name_memo: NetHelperNameMemo::default(),
        }
    }

    /// Lends the net-helper name memo out for a pass. The index holds it again
    /// through [`Self::restore_helper_name_memo`]; leaving it out only costs the
    /// pass its hits.
    pub fn take_helper_name_memo(&mut self) -> NetHelperNameMemo {
        std::mem::take(&mut self.helper_name_memo)
    }

    pub fn restore_helper_name_memo(&mut self, memo: NetHelperNameMemo) {
        self.helper_name_memo = memo;
    }

    /// Replaces the canonical op table. Called once per analyze pass with
    /// metadata collected from annotated signatures.
    pub fn set_canonical_ops(
        &mut self,
        canonical_ops: FxHashMap<(SmolStr, NetOpDirection), CanonicalNetOp>,
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

    pub fn add_file_data(&mut self, file_id: FileId, data: FileNetworkData) {
        self.remove_file(file_id);
        // Fresh facts supersede anything parked for this file: without this a
        // recollect would leave its own pre-edit data staged behind it.
        self.staged_data.remove(&file_id);
        self.staged_inputs.remove(&file_id);
        self.index_file_data(file_id, &data);
        self.file_data.insert(file_id, data);
    }

    pub fn get_file_data(&self, file_id: FileId) -> Option<&FileNetworkData> {
        self.file_data.get(&file_id)
    }

    /// Records the inputs `data` was collected from. Always paired with
    /// [`Self::add_file_data`] (including for empty data, so files without
    /// flows are retainable too).
    pub fn set_file_inputs(&mut self, file_id: FileId, inputs: NetworkFileInputs) {
        self.file_inputs.insert(file_id, inputs);
    }

    pub fn get_file_inputs(&self, file_id: FileId) -> Option<&NetworkFileInputs> {
        self.file_inputs.get(&file_id)
    }

    /// Inputs parked by [`Self::remove_file`] while the file is re-analysed.
    pub fn staged_file_inputs(&self, file_id: FileId) -> Option<&NetworkFileInputs> {
        self.staged_inputs.get(&file_id)
    }

    /// Restores data parked by [`Self::remove_file`]. The staged entry is
    /// re-indexed exactly as [`Self::add_file_data`] would index it, so
    /// `materialized_definition_counts` (a cross-file refcount) and the
    /// per-message indexes come back whole. Returns whether anything was
    /// restored.
    pub fn restore_staged_file(&mut self, file_id: FileId) -> bool {
        let Some(data) = self.staged_data.remove(&file_id) else {
            // No parked data, but parked inputs may still describe a file
            // whose last collection found no flows — adopt them so the file
            // stays retainable without recollecting.
            if let Some(inputs) = self.staged_inputs.remove(&file_id) {
                self.file_inputs.insert(file_id, inputs);
            }
            return false;
        };
        let inputs = self.staged_inputs.remove(&file_id);
        self.index_file_data(file_id, &data);
        self.file_data.insert(file_id, data);
        if let Some(inputs) = inputs {
            self.file_inputs.insert(file_id, inputs);
        }
        true
    }

    /// Drops a parked entry without restoring it, after a fresh collection
    /// (or a retain decision that found nothing to keep) superseded it.
    pub fn drop_staged_file(&mut self, file_id: FileId) {
        self.staged_data.remove(&file_id);
        self.staged_inputs.remove(&file_id);
    }

    /// Drops parked entries for files that no longer exist. Called at the end
    /// of the network pass: anything still parked then belongs to a deleted
    /// file (live files are either recollected or restored above).
    pub fn prune_staged_dead_files(&mut self, is_live: impl Fn(FileId) -> bool) {
        self.staged_data.retain(|file_id, _| is_live(*file_id));
        self.staged_inputs.retain(|file_id, _| is_live(*file_id));
    }

    /// File ids in canonical order. `file_data` is a `RandomState` map, so
    /// iterating it directly yields a different flow order every process run.
    fn ordered_file_ids(&self) -> Vec<FileId> {
        let mut file_ids: Vec<FileId> = self.file_data.keys().copied().collect();
        file_ids.sort_unstable_by_key(|file_id| file_id.id);
        file_ids
    }

    pub fn iter_send_flows(&self) -> impl Iterator<Item = (FileId, usize, &NetSendFlow)> {
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
        // Park rather than drop: `remove` runs before every re-index, and the
        // network pass restores the entry when the file's inputs prove it
        // still describes the file (see `restore_staged_file`). De-indexing
        // first keeps `materialized_definition_counts` exact while parked —
        // restoring re-indexes — and `add_file_data` stays idempotent per
        // file: it re-parks (a no-op, nothing is live) and then clears the
        // parked entry it just superseded.
        if let Some(data) = self.file_data.remove(&file_id) {
            self.remove_file_data_indexes(file_id, &data);
            self.staged_data.insert(file_id, data);
        }
        if let Some(inputs) = self.file_inputs.remove(&file_id) {
            self.staged_inputs.insert(file_id, inputs);
        }
    }

    pub fn clear(&mut self) {
        self.file_data.clear();
        self.file_inputs.clear();
        self.staged_data.clear();
        self.staged_inputs.clear();
        self.send_flows_by_message.clear();
        self.receive_flows_by_message.clear();
        self.materialized_definition_counts.clear();
        self.canonical_ops.clear();
        self.helper_name_memo.files.clear();
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
        // Memory hygiene only: the memo's revision guard already rejects a stale
        // entry, this stops a removed file's entry from outliving the file.
        self.helper_name_memo.files.remove(&file_id);
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
    fn helper_name_memo_entries_only_answer_for_the_revision_they_were_stored_at() {
        let file_id = FileId::new(1);
        let name: SmolStr = "Send".into();
        let expansion = NetNameExpansion {
            names: vec![SmolStr::new("SendThing")],
            locals: Vec::new(),
        };
        let mut memo = NetHelperNameMemo::default();

        memo.set_name(file_id, 7, name.clone(), expansion);
        assert!(memo.name(file_id, 7, &name).is_some());
        assert!(memo.name(file_id, 8, &name).is_none());
        assert!(memo.name(FileId::new(2), 7, &name).is_none());

        // Storing at a newer revision drops everything the old one held.
        memo.set_name(
            file_id,
            8,
            SmolStr::new("Other"),
            NetNameExpansion::default(),
        );
        assert!(memo.name(file_id, 8, &name).is_none());
        assert!(memo.name(file_id, 8, &SmolStr::new("Other")).is_some());
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

    fn test_inputs(content_hash: u64) -> NetworkFileInputs {
        NetworkFileInputs {
            content_hash,
            registry_identity: 1,
            helper_contents: Vec::new(),
        }
    }

    #[test]
    fn remove_parks_data_and_inputs_for_restore() {
        let file_id = FileId::new(11);
        let mut index = GmodNetworkIndex::new();
        index.add_file_data(
            file_id,
            FileNetworkData {
                send_flows: vec![send_flow("Parked", 1)],
                receive_flows: Vec::new(),
            },
        );
        index.set_file_inputs(file_id, test_inputs(7));

        index.remove_file(file_id);
        // Parked entries stay invisible to every reader.
        assert!(index.get_file_data(file_id).is_none());
        assert!(index.get_file_inputs(file_id).is_none());
        assert!(index.get_send_flows_for_message("Parked").is_empty());

        assert!(index.restore_staged_file(file_id));
        assert_eq!(
            index
                .get_file_data(file_id)
                .expect("restored data")
                .send_flows
                .len(),
            1
        );
        assert_eq!(
            index
                .get_file_inputs(file_id)
                .map(|inputs| inputs.content_hash),
            Some(7)
        );
        assert_eq!(index.get_send_flows_for_message("Parked").len(), 1);
    }

    #[test]
    fn restore_reindexes_materialized_definition_counts() {
        let definition_file_id = FileId::new(12);
        let call_file_id = FileId::new(13);
        let definition_start = range(10);
        let mut materialized = send_flow("StagedWrapped", 30);
        materialized.materialized_from = Some(NetFlowOrigin {
            file_id: definition_file_id,
            start_range: definition_start,
        });

        let mut index = GmodNetworkIndex::new();
        index.add_file_data(
            definition_file_id,
            FileNetworkData {
                send_flows: vec![send_flow("StagedWrapped", 10)],
                receive_flows: Vec::new(),
            },
        );
        index.set_file_inputs(definition_file_id, test_inputs(1));
        index.add_file_data(
            call_file_id,
            FileNetworkData {
                send_flows: vec![materialized],
                receive_flows: Vec::new(),
            },
        );
        index.set_file_inputs(call_file_id, test_inputs(2));

        // Parking the call file drops its refcount, so the definition shows
        // again; restoring brings the replacement back exactly once.
        index.remove_file(call_file_id);
        let flows = index.get_send_flows_for_message("StagedWrapped");
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].0, definition_file_id);

        assert!(index.restore_staged_file(call_file_id));
        let flows = index.get_send_flows_for_message("StagedWrapped");
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].0, call_file_id);

        // Restoring twice must not double-count the refcount.
        assert!(!index.restore_staged_file(call_file_id));
        let flows = index.get_send_flows_for_message("StagedWrapped");
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].0, call_file_id);
    }

    #[test]
    fn fresh_add_supersedes_parked_entry() {
        let file_id = FileId::new(14);
        let mut index = GmodNetworkIndex::new();
        index.add_file_data(
            file_id,
            FileNetworkData {
                send_flows: vec![send_flow("OldParked", 1)],
                receive_flows: Vec::new(),
            },
        );
        index.set_file_inputs(file_id, test_inputs(3));
        index.remove_file(file_id);

        index.add_file_data(
            file_id,
            FileNetworkData {
                send_flows: vec![send_flow("NewLive", 5)],
                receive_flows: Vec::new(),
            },
        );
        assert!(index.get_send_flows_for_message("OldParked").is_empty());
        assert_eq!(index.get_send_flows_for_message("NewLive").len(), 1);
        assert!(!index.restore_staged_file(file_id));
    }

    #[test]
    fn prune_drops_only_dead_parked_files() {
        let live_id = FileId::new(15);
        let dead_id = FileId::new(16);
        let mut index = GmodNetworkIndex::new();
        for file_id in [live_id, dead_id] {
            index.add_file_data(
                file_id,
                FileNetworkData {
                    send_flows: vec![send_flow("PruneMe", file_id.id)],
                    receive_flows: Vec::new(),
                },
            );
            index.set_file_inputs(file_id, test_inputs(u64::from(file_id.id)));
            index.remove_file(file_id);
        }

        index.prune_staged_dead_files(|file_id| file_id == live_id);
        assert!(index.restore_staged_file(live_id));
        assert!(!index.restore_staged_file(dead_id));
        assert!(index.staged_file_inputs(dead_id).is_none());
    }
}
