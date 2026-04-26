use std::{cmp::Reverse, collections::HashMap};

use glua_code_analysis::{
    EmmyLuaAnalysis, FileId, NetFlowKind, NetOpEntry, NetOpKind, NetReceiveFlow, NetSendFlow,
    SemanticModel,
};
use glua_parser::{
    LuaAstNode, LuaAstToken, LuaCallArgList, LuaCallExpr, LuaLiteralExpr, LuaStringToken,
    LuaSyntaxToken, PathTrait,
};
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind};
use rowan::TextRange;

const NET_TRIGGER_CALL_PATHS: &[&str] = &["net.Start", "net.Receive", "util.AddNetworkString"];

pub fn hover_gmod_net_message_string(
    analysis: &EmmyLuaAnalysis,
    semantic_model: &SemanticModel,
    token: &LuaSyntaxToken,
) -> Option<Hover> {
    if !semantic_model.get_emmyrc().gmod.enabled
        || !semantic_model.get_emmyrc().gmod.network.enabled
    {
        return None;
    }

    let string_token = LuaStringToken::cast(token.clone())?;
    let literal_expr = string_token.get_parent::<LuaLiteralExpr>()?;
    let call_expr = literal_expr
        .get_parent::<LuaCallArgList>()?
        .get_parent::<LuaCallExpr>()?;

    if !is_net_message_name_context(&call_expr, &literal_expr) {
        return None;
    }

    let message_name = string_token.get_value();
    let message_name = message_name.trim();
    if message_name.is_empty() {
        return None;
    }

    let db = semantic_model.get_db();
    let network_index = db.get_gmod_network_index();
    let send_flows = network_index.get_send_flows_for_message(message_name);
    let receive_flows = network_index.get_receive_flows_for_message(message_name);

    let markdown = render_net_message_hover(analysis, message_name, &send_flows, &receive_flows);

    let document = semantic_model.get_document();
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: markdown,
        }),
        range: document.to_lsp_range(token.text_range()),
    })
}

fn is_net_message_name_context(call_expr: &LuaCallExpr, literal_expr: &LuaLiteralExpr) -> bool {
    let Some(call_path) = call_expr.get_access_path() else {
        return false;
    };
    if !NET_TRIGGER_CALL_PATHS.iter().any(|p| *p == call_path) {
        return false;
    }
    let Some(args_list) = call_expr.get_args_list() else {
        return false;
    };
    let arg_idx = args_list
        .get_args()
        .position(|arg| arg.get_position() == literal_expr.get_position());
    arg_idx == Some(0)
}

fn render_net_message_hover(
    analysis: &EmmyLuaAnalysis,
    message_name: &str,
    send_flows: &[(FileId, &NetSendFlow)],
    receive_flows: &[(FileId, &NetReceiveFlow)],
) -> String {
    let send_groups = group_send_flows_by_pattern(send_flows);
    let receive_groups = group_receive_flows_by_pattern(receive_flows);

    let mut sections: Vec<String> = Vec::new();
    sections.push(render_header(message_name, &send_groups, &receive_groups));

    if let Some(block) =
        render_side_section(analysis, "Senders", "📤", OpDirection::Write, &send_groups)
    {
        sections.push(block);
    }
    if let Some(block) = render_side_section(
        analysis,
        "Receivers",
        "📥",
        OpDirection::Read,
        &receive_groups,
    ) {
        sections.push(block);
    }

    if send_groups.is_empty() && receive_groups.is_empty() {
        sections.push("_No payload patterns indexed for this message._".to_string());
    }

    sections.join("\n\n---\n\n")
}

fn render_header(
    message_name: &str,
    send_groups: &[PatternGroup],
    receive_groups: &[PatternGroup],
) -> String {
    let send_count: usize = send_groups.iter().map(|g| g.locations.len()).sum();
    let recv_count: usize = receive_groups.iter().map(|g| g.locations.len()).sum();
    let total = send_count + recv_count;

    let mut out = String::new();
    out.push_str(&format!("```lua\n(net) {:?}\n```", message_name));

    let summary = if total == 0 {
        "_no recorded usages_".to_string()
    } else {
        format!(
            "{} usage{} — {} sender{}, {} receiver{}",
            total,
            if total == 1 { "" } else { "s" },
            send_count,
            if send_count == 1 { "" } else { "s" },
            recv_count,
            if recv_count == 1 { "" } else { "s" },
        )
    };
    out.push_str("\n\n");
    out.push_str(&summary);
    out
}

#[derive(Clone, Copy)]
enum OpDirection {
    Write,
    Read,
}

fn render_side_section(
    analysis: &EmmyLuaAnalysis,
    label: &str,
    icon: &str,
    direction: OpDirection,
    groups: &[PatternGroup],
) -> Option<String> {
    if groups.is_empty() {
        return None;
    }
    let total: usize = groups.iter().map(|g| g.locations.len()).sum();
    let mut out = String::new();

    if groups.len() == 1 {
        let group = &groups[0];
        out.push_str(&format!(
            "{icon} **{label}** · {}\n\n",
            group.locations.len()
        ));
        out.push_str(&render_location_links(analysis, &group.locations));
        out.push_str("\n\n");
        out.push_str(&render_pattern_block(direction, &group.pattern));
    } else {
        out.push_str(&format!(
            "{icon} **{label}** · {} across {} patterns",
            total,
            groups.len()
        ));
        for (i, group) in groups.iter().enumerate() {
            out.push_str(&format!(
                "\n\n_Pattern {}_ · {}\n",
                pattern_label(i),
                group.locations.len(),
            ));
            out.push_str(&render_location_links(analysis, &group.locations));
            out.push_str("\n\n");
            out.push_str(&render_pattern_block(direction, &group.pattern));
        }
    }
    Some(out)
}

/// Renders the payload as a single Lua-fenced block.
///
/// **Numbering rule:** loops (`for`/`while`/`repeat`) advance the dotted-path
/// counter and create sub-numbered groups (`[2.1]`, `[2.2]`, …). Conditionals
/// (`if`/`elseif`/`else`) DO NOT advance numbering — they appear as
/// un-numbered header lines so the user can read the gating condition
/// without losing the running op count.
///
/// Ops gated by any conditional get a `?` inside their bracket label
/// (`[3?]`) so that, even after scrolling past the `if` header, the marker
/// reminds the reader the byte may not flow.
///
/// Both indentation depth and `end` lines reflect the FULL nesting (loops +
/// conditionals), so the source-shaped block looks like real Lua.
///
/// Labels are right-padded to the widest label so the call column stays
/// aligned across rows of different depths.
fn render_pattern_block(direction: OpDirection, pattern: &[PatternEntry]) -> String {
    if pattern.is_empty() {
        return "_no payload_".to_string();
    }
    let prefix = match direction {
        OpDirection::Write => "Write",
        OpDirection::Read => "Read",
    };

    let mut planned: Vec<PlannedRow> = Vec::new();
    // open_stack tracks ALL currently-open frames (loops + conditionals)
    // so closes/indentation match the source. counters/open_loop_nums are
    // indexed by **loop depth only** — conditionals don't get a number.
    let mut open_stack: Vec<PatternFlowFrame> = Vec::new();
    let mut counters: Vec<u32> = vec![1];
    let mut open_loop_nums: Vec<u32> = Vec::new();

    for entry in pattern {
        let new_path = &entry.flow_path;
        let mut shared = 0usize;
        while shared < open_stack.len()
            && shared < new_path.len()
            && open_stack[shared] == new_path[shared]
        {
            shared += 1;
        }
        let mut close_depth = open_stack.len();
        for popped in open_stack.drain(shared..).rev() {
            close_depth -= 1;
            if popped.kind.is_loop() {
                open_loop_nums.pop();
                counters.pop();
            }
            planned.push(PlannedRow::Close { depth: close_depth });
        }
        while open_stack.len() < new_path.len() {
            let next_idx = open_stack.len();
            let frame = new_path[next_idx].clone();
            let depth = open_stack.len();
            if frame.kind.is_loop() {
                let loop_depth = open_loop_nums.len();
                let n = counters[loop_depth];
                let mut path_nums = open_loop_nums.clone();
                path_nums.push(n);
                planned.push(PlannedRow::Open {
                    depth,
                    path: Some(path_nums),
                    frame: frame.clone(),
                });
                counters[loop_depth] += 1;
                counters.push(1);
                open_loop_nums.push(n);
            } else {
                planned.push(PlannedRow::Open {
                    depth,
                    path: None,
                    frame: frame.clone(),
                });
            }
            open_stack.push(frame);
        }
        let depth = open_stack.len();
        let loop_depth = open_loop_nums.len();
        let n = counters[loop_depth];
        let mut path_nums = open_loop_nums.clone();
        path_nums.push(n);
        let conditional = entry.flow_path.iter().any(|f| !f.kind.is_loop());
        planned.push(PlannedRow::Op {
            depth,
            path: path_nums,
            conditional,
            call: render_call(prefix, entry, direction),
        });
        counters[loop_depth] += 1;
    }
    while let Some(popped) = open_stack.pop() {
        if popped.kind.is_loop() {
            open_loop_nums.pop();
            counters.pop();
        }
        planned.push(PlannedRow::Close {
            depth: open_stack.len(),
        });
    }

    let label_w = planned
        .iter()
        .map(|row| match row {
            PlannedRow::Op {
                path, conditional, ..
            } => format_label_raw(path, *conditional).len(),
            PlannedRow::Open {
                path: Some(path), ..
            } => format_label_raw(path, false).len(),
            _ => 0,
        })
        .max()
        .unwrap_or(0);

    let mut body = String::new();
    for row in &planned {
        match row {
            PlannedRow::Op {
                depth,
                path,
                conditional,
                call,
            } => {
                let label = pad_label(&format_label_raw(path, *conditional), label_w);
                let indent = indent_for_depth(*depth);
                body.push_str(&format!("{label} {indent}{call}\n"));
            }
            PlannedRow::Open {
                depth,
                path: Some(path),
                frame,
            } => {
                let label = pad_label(&format_label_raw(path, false), label_w);
                let indent = indent_for_depth(*depth);
                let header = render_frame_header(frame);
                body.push_str(&format!("{label} {indent}{header}\n"));
            }
            PlannedRow::Open {
                depth,
                path: None,
                frame,
            } => {
                let label = " ".repeat(label_w);
                let indent = indent_for_depth(*depth);
                let header = render_frame_header(frame);
                body.push_str(&format!("{label} {indent}{header}\n"));
            }
            PlannedRow::Close { depth } => {
                let label = " ".repeat(label_w);
                let indent = indent_for_depth(*depth);
                body.push_str(&format!("{label} {indent}end\n"));
            }
        }
    }

    format!("```lua\n{body}```")
}

enum PlannedRow {
    Op {
        depth: usize,
        path: Vec<u32>,
        conditional: bool,
        call: String,
    },
    Open {
        depth: usize,
        /// Loop frames carry a dotted-path number; conditional frames don't,
        /// so this is `None` for `if`/`elseif`/`else` headers.
        path: Option<Vec<u32>>,
        frame: PatternFlowFrame,
    },
    Close {
        depth: usize,
    },
}

/// Two-space indent per loop nesting level.
fn indent_for_depth(depth: usize) -> String {
    "  ".repeat(depth)
}

/// Bare dotted-path label with brackets, no padding. Conditional ops/openers
/// get a trailing `?` inside the brackets so the marker travels with the
/// number rather than sitting in a separate column.
fn format_label_raw(path: &[u32], conditional: bool) -> String {
    let inner: Vec<String> = path.iter().map(|n| n.to_string()).collect();
    let suffix = if conditional { "?" } else { "" };
    format!("[{}{}]", inner.join("."), suffix)
}

/// Pads a label on the right with spaces to `width` so call columns align.
fn pad_label(label: &str, width: usize) -> String {
    if label.len() >= width {
        return label.to_string();
    }
    let mut out = String::with_capacity(width);
    out.push_str(label);
    for _ in label.len()..width {
        out.push(' ');
    }
    out
}

/// Builds the source-shaped opener for a frame: prefers the captured raw
/// header text (e.g. `if foo > 0 then`) and falls back to a generic synthetic
/// opener when the captured header is unsuitable.
fn render_frame_header(frame: &PatternFlowFrame) -> String {
    if let Some(h) = &frame.header {
        return h.clone();
    }
    match frame.kind {
        NetFlowKind::If => "if ? then".to_string(),
        NetFlowKind::While => "while ? do".to_string(),
        NetFlowKind::For => "for ? do".to_string(),
        NetFlowKind::ForRange => "for ? in ? do".to_string(),
        NetFlowKind::Repeat => "repeat".to_string(),
    }
}

fn render_call(prefix: &str, entry: &PatternEntry, direction: OpDirection) -> String {
    let base = format!("net.{prefix}{}", entry.kind.type_name());
    let is_read = matches!(direction, OpDirection::Read);
    match (is_read, entry.sample_value.as_deref(), entry.bits) {
        (false, Some(value), Some(bits)) => format!("{base}({value}, {bits})"),
        (false, Some(value), None) => format!("{base}({value})"),
        (_, _, Some(bits)) => format!("{base}({bits})"),
        _ => base,
    }
}

fn render_location_links(analysis: &EmmyLuaAnalysis, locations: &[(FileId, TextRange)]) -> String {
    let db = analysis.compilation.get_db();
    let vfs = db.get_vfs();
    let mut shown: Vec<String> = Vec::new();
    for (fid, range) in locations.iter().take(MAX_FILE_LIST_ENTRIES) {
        let label = vfs
            .get_file_path(fid)
            .map(|p| short_path_label(p.to_string_lossy().as_ref()))
            .unwrap_or_else(|| "<unknown>".to_string());

        let line_1based = vfs
            .get_document(fid)
            .and_then(|doc| doc.get_line(range.start()))
            .map(|l| l + 1);

        let uri = vfs.get_uri(fid).map(|u| u.to_string());

        match (uri, line_1based) {
            (Some(uri), Some(line)) => {
                shown.push(format!("[`{label}:{line}`]({uri}#L{line})"));
            }
            (Some(uri), None) => {
                shown.push(format!("[`{label}`]({uri})"));
            }
            (None, Some(line)) => {
                shown.push(format!("`{label}:{line}`"));
            }
            (None, None) => {
                shown.push(format!("`{label}`"));
            }
        }
    }
    let mut out = shown.join(", ");
    if locations.len() > MAX_FILE_LIST_ENTRIES {
        out.push_str(&format!(
            ", _+{} more_",
            locations.len() - MAX_FILE_LIST_ENTRIES
        ));
    }
    out
}

fn pattern_label(idx: usize) -> char {
    char::from_u32(b'A' as u32 + (idx as u32 % 26)).unwrap_or('?')
}

#[derive(Clone)]
struct PatternGroup {
    pattern: Vec<PatternEntry>,
    /// One entry per call site that produced this pattern. Each pair is
    /// `(file, range_of_net.Start_or_net.Receive)` so the hover can render a
    /// clickable jump-to-source link with line number for every site.
    locations: Vec<(FileId, TextRange)>,
}

#[derive(Clone)]
struct PatternEntry {
    kind: NetOpKind,
    dynamic: bool,
    /// Static bit-width literal for `WriteUInt`/`WriteInt`/`ReadUInt`/`ReadInt`.
    /// Part of pattern identity (see `PartialEq`/`Hash` impls below) so two
    /// senders that disagree on bit width surface as distinct payload patterns.
    bits: Option<u32>,
    /// Sample value-arg source text from the first call site that produced
    /// this pattern. NOT part of pattern identity — within a group, sites
    /// may write different values; we just show one example so the user has
    /// some idea of what flows on the wire.
    sample_value: Option<String>,
    /// Stack of enclosing control-flow frames (outer-to-inner). The frame id
    /// is **relabeled per pattern** to be a small dense integer in
    /// first-occurrence order, so two senders with the same structural shape
    /// (same nesting, same headers, same sharing) hash to the same pattern.
    /// Per-source-position uniqueness is preserved within a pattern: two
    /// adjacent `if x then` statements get distinct relabeled ids.
    flow_path: Vec<PatternFlowFrame>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct PatternFlowFrame {
    kind: NetFlowKind,
    header: Option<String>,
    id: u32,
}

impl PartialEq for PatternEntry {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.dynamic == other.dynamic
            && self.bits == other.bits
            && self.flow_path == other.flow_path
    }
}

impl Eq for PatternEntry {}

impl std::hash::Hash for PatternEntry {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.kind.hash(state);
        self.dynamic.hash(state);
        self.bits.hash(state);
        self.flow_path.hash(state);
    }
}

fn pattern_from_ops(ops: &[NetOpEntry]) -> Vec<PatternEntry> {
    // Relabel source-frame ids to dense per-pattern ids in first-occurrence
    // order. Two ops sharing the same source frame keep the same relabeled id;
    // distinct frames (even with identical kind/header) get distinct ids.
    let mut id_map: HashMap<u32, u32> = HashMap::new();
    let mut next_id: u32 = 0;
    ops.iter()
        .map(|op| {
            let flow_path = op
                .flow_path
                .iter()
                .map(|f| {
                    let relabeled = *id_map.entry(f.id).or_insert_with(|| {
                        let v = next_id;
                        next_id += 1;
                        v
                    });
                    PatternFlowFrame {
                        kind: f.kind,
                        header: f.header.clone(),
                        id: relabeled,
                    }
                })
                .collect();
            PatternEntry {
                kind: op.kind,
                dynamic: op.dynamic,
                bits: op.bits,
                sample_value: op.value_text.clone(),
                flow_path,
            }
        })
        .collect()
}

fn group_send_flows_by_pattern(flows: &[(FileId, &NetSendFlow)]) -> Vec<PatternGroup> {
    let mut groups: HashMap<Vec<PatternEntry>, PatternGroup> = HashMap::new();
    for (file_id, flow) in flows {
        if flow.is_wrapped {
            continue;
        }
        let pattern = pattern_from_ops(&flow.writes);
        let entry = groups
            .entry(pattern.clone())
            .or_insert_with(|| PatternGroup {
                pattern: pattern.clone(),
                locations: Vec::new(),
            });
        entry.locations.push((*file_id, flow.start_range));
    }
    let mut sorted: Vec<_> = groups.into_values().collect();
    sorted.sort_by_key(|group| Reverse(group.locations.len()));
    sorted
}

fn group_receive_flows_by_pattern(flows: &[(FileId, &NetReceiveFlow)]) -> Vec<PatternGroup> {
    let mut groups: HashMap<Vec<PatternEntry>, PatternGroup> = HashMap::new();
    for (file_id, flow) in flows {
        if flow.reads_opaque {
            continue;
        }
        let pattern = pattern_from_ops(&flow.reads);
        let entry = groups
            .entry(pattern.clone())
            .or_insert_with(|| PatternGroup {
                pattern: pattern.clone(),
                locations: Vec::new(),
            });
        entry.locations.push((*file_id, flow.receive_range));
    }
    let mut sorted: Vec<_> = groups.into_values().collect();
    sorted.sort_by_key(|group| Reverse(group.locations.len()));
    sorted
}

const MAX_FILE_LIST_ENTRIES: usize = 6;

fn short_path_label(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if let Some(idx) = normalized.rfind("/lua/") {
        return normalized[idx + 1..].to_string();
    }
    if let Some(idx) = normalized.rfind('/') {
        return normalized[idx + 1..].to_string();
    }
    normalized
}

trait NetOpKindExt {
    fn type_name(&self) -> &'static str;
}

impl NetOpKindExt for NetOpKind {
    fn type_name(&self) -> &'static str {
        let n = self.to_fn_name();
        n.strip_prefix("net.Write")
            .or_else(|| n.strip_prefix("net.Read"))
            .unwrap_or(n)
    }
}
