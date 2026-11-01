use glua_parser::{LuaAstNode, LuaExpr, LuaKind, LuaSyntaxId, LuaSyntaxKind};
use rowan::{TextRange, TextSize};
use rustc_hash::FxHashSet;

use crate::{
    DbIndex, FileId, InFiled, LuaDeclId, LuaDeclTypeKind, LuaDefinitionId, LuaInferenceNodeId,
    LuaInferredGuardOwner, LuaMemberId, LuaOperatorOwner, LuaSemanticDeclId, LuaSignatureId,
    LuaTypeDeclId, LuaTypeOwner,
};

/// One stretch an edit changed: `old_start..old_end` of the old text became
/// `new_start..new_end` of the new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hunk {
    old_start: u32,
    old_end: u32,
    new_start: u32,
    new_end: u32,
}

/// The hunks an edit changed, as byte offsets, in order and apart.
///
/// Offsets before the first hunk are unmoved; an offset between two hunks,
/// or after the last, shifts by how much the hunks before it grew or shrank;
/// offsets inside a hunk have no image. The text between two hunks is the
/// same in both versions.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PositionMap {
    hunks: std::sync::Arc<[Hunk]>,
}

/// The length a map's last kept stretch runs for: to the end of either text.
const OPEN: u32 = u32::MAX;

impl PositionMap {
    /// The map for "nothing moved": every offset is its own image.
    pub fn identity() -> Self {
        Self::default()
    }

    /// The hunks between two versions of a file's text: the lines either side
    /// matched by their content without surrounding whitespace, and each
    /// stretch that differs narrowed to the bytes that do.
    pub fn new(old_text: &str, new_text: &str) -> Self {
        Self::from_hunks(diff::hunks(old_text, new_text))
    }

    /// The map for a file whose whole text was replaced, or that was deleted
    /// (`new_len == 0`). Nothing inside it survives.
    pub fn whole_file(old_len: usize, new_len: usize) -> Self {
        Self::from_hunks(vec![Hunk {
            old_start: 0,
            old_end: old_len as u32,
            new_start: 0,
            new_end: new_len as u32,
        }])
    }

    fn from_hunks(hunks: Vec<Hunk>) -> Self {
        let mut merged: Vec<Hunk> = Vec::with_capacity(hunks.len());
        for hunk in hunks {
            if hunk.old_start == hunk.old_end && hunk.new_start == hunk.new_end {
                continue;
            }
            match merged.last_mut() {
                Some(last) if last.old_end >= hunk.old_start || last.new_end >= hunk.new_start => {
                    last.old_end = hunk.old_end;
                    last.new_end = hunk.new_end;
                }
                _ => merged.push(hunk),
            }
        }
        Self {
            hunks: merged.into(),
        }
    }

    /// The stretches both texts share, as `(old start, new start, length)`,
    /// the last one running to the end of the text.
    fn kept(&self) -> Vec<(u32, u32, u32)> {
        let mut kept = Vec::with_capacity(self.hunks.len() + 1);
        let (mut old, mut new) = (0, 0);
        for hunk in self.hunks.iter() {
            if hunk.old_start > old {
                kept.push((old, new, hunk.old_start - old));
            }
            old = hunk.old_end;
            new = hunk.new_end;
        }
        kept.push((old, new, OPEN));
        kept
    }

    /// The map whose shared stretches are `kept`, in order: the gaps between
    /// them are its hunks.
    fn from_kept(kept: &[(u32, u32, u32)]) -> Self {
        let mut hunks = Vec::with_capacity(kept.len());
        let (mut old, mut new) = (0, 0);
        for &(old_start, new_start, len) in kept {
            hunks.push(Hunk {
                old_start: old,
                old_end: old_start,
                new_start: new,
                new_end: new_start,
            });
            old = old_start.saturating_add(len);
            new = new_start.saturating_add(len);
        }
        Self::from_hunks(hunks)
    }

    /// This edit followed by `later`, an edit of the text this one produced:
    /// an offset maps where `later` maps this edit's image of it.
    pub fn then(&self, later: &Self) -> Self {
        if self.is_identity() {
            return later.clone();
        }
        if later.is_identity() {
            return self.clone();
        }
        let (first, second) = (self.kept(), later.kept());
        let mut kept = Vec::with_capacity(first.len() + second.len());
        let (mut i, mut j) = (0, 0);
        while i < first.len() && j < second.len() {
            let (old, middle, first_len) = first[i];
            let (middle_start, new, second_len) = second[j];
            let first_end = middle.saturating_add(first_len);
            let second_end = middle_start.saturating_add(second_len);
            let start = middle.max(middle_start);
            let end = first_end.min(second_end);
            if start < end {
                kept.push((
                    old + (start - middle),
                    new + (start - middle_start),
                    end - start,
                ));
            }
            if first_end <= second_end {
                i += 1;
            } else {
                j += 1;
            }
        }
        Self::from_kept(&kept)
    }

    /// The map from the new text back to the old one.
    pub fn inverse(&self) -> Self {
        Self {
            hunks: self
                .hunks
                .iter()
                .map(|hunk| Hunk {
                    old_start: hunk.new_start,
                    old_end: hunk.new_end,
                    new_start: hunk.old_start,
                    new_end: hunk.old_end,
                })
                .collect(),
        }
    }

    /// Whether every offset maps to itself, so the remap pass can be skipped.
    pub fn is_identity(&self) -> bool {
        self.hunks.is_empty()
    }

    /// The first offset the edit changed: everything before it is untouched.
    pub fn first_change(&self) -> u32 {
        self.hunks.first().map_or(u32::MAX, |hunk| hunk.old_start)
    }

    /// The hunks as `(old start, old end, new start, new end)`.
    pub fn hunks(&self) -> Vec<(u32, u32, u32, u32)> {
        self.hunks
            .iter()
            .map(|hunk| (hunk.old_start, hunk.old_end, hunk.new_start, hunk.new_end))
            .collect()
    }

    pub fn map(&self, pos: TextSize) -> Option<TextSize> {
        let p = u32::from(pos);
        let at = self.hunks.partition_point(|hunk| hunk.old_start <= p);
        let Some(hunk) = at.checked_sub(1).map(|at| self.hunks[at]) else {
            return Some(pos);
        };
        (p >= hunk.old_end).then(|| TextSize::new(p - hunk.old_end + hunk.new_end))
    }

    /// Where a range starting at `p` starts: one starting where a hunk does
    /// takes the hunk in, and one starting at an insertion begins after it.
    fn start_image(&self, p: u32) -> Option<u32> {
        let at = self.hunks.partition_point(|hunk| hunk.old_start <= p);
        let Some(hunk) = at.checked_sub(1).map(|at| self.hunks[at]) else {
            return Some(p);
        };
        if hunk.old_start == p {
            return Some(if hunk.old_start == hunk.old_end {
                hunk.new_end
            } else {
                hunk.new_start
            });
        }
        (p >= hunk.old_end).then(|| p - hunk.old_end + hunk.new_end)
    }

    /// Where a range ending at `p` ends: one ending where a hunk starts stops
    /// before it, and one ending where a hunk ends takes the hunk in.
    fn end_image(&self, p: u32) -> Option<u32> {
        let at = self.hunks.partition_point(|hunk| hunk.old_start < p);
        let Some(hunk) = at.checked_sub(1).map(|at| self.hunks[at]) else {
            return Some(p);
        };
        (p >= hunk.old_end).then(|| p - hunk.old_end + hunk.new_end)
    }

    /// The image of a range, or `None` when an endpoint fell inside a hunk.
    ///
    /// A range whose endpoints straddle hunks keeps its start and takes the
    /// shift on its end: the edit happened *within* it. Reporting it lost
    /// instead would invalidate every dependent of a hub table literal or a
    /// long function on any edit inside it, while what actually changed is
    /// visible as a per-member or per-signature export change.
    pub fn map_range(&self, range: TextRange) -> Option<TextRange> {
        let start = u32::from(range.start());
        let end = u32::from(range.end());
        if start == end {
            return self
                .end_image(end)
                .map(|at| TextRange::empty(TextSize::new(at)));
        }
        let start = self.start_image(start)?;
        let end = self.end_image(end)?;
        (start <= end).then(|| TextRange::new(TextSize::new(start), TextSize::new(end)))
    }
}

/// The hunks between two texts.
mod diff {
    use rustc_hash::FxHashMap;

    use super::Hunk;

    /// The hunks turning `old` into `new`: lines matched by their content
    /// without surrounding whitespace, then each mismatch narrowed to the
    /// bytes that differ, never inside a character.
    pub(super) fn hunks(old: &str, new: &str) -> Vec<Hunk> {
        if old == new {
            return Vec::new();
        }
        let old_lines = lines(old);
        let new_lines = lines(new);
        let mut matched = Vec::new();
        match_lines(
            old,
            new,
            &old_lines,
            &new_lines,
            (0, old_lines.len()),
            (0, new_lines.len()),
            &mut matched,
        );

        let mut hunks = Vec::new();
        let (mut old_line, mut new_line) = (0, 0);
        for (old_at, new_at) in matched
            .into_iter()
            .chain([(old_lines.len(), new_lines.len())])
        {
            edited_lines(
                old,
                new,
                &old_lines,
                &new_lines,
                (old_line, old_at),
                (new_line, new_at),
                &mut hunks,
            );
            if old_at < old_lines.len() {
                let (old_line_range, new_line_range) = (old_lines[old_at], new_lines[new_at]);
                let (old_content, new_content) =
                    (content(old, old_line_range), content(new, new_line_range));
                narrowed(
                    old,
                    new,
                    (old_line_range.0, old_content.0),
                    (new_line_range.0, new_content.0),
                    &mut hunks,
                );
                narrowed(
                    old,
                    new,
                    (old_content.1, old_line_range.1),
                    (new_content.1, new_line_range.1),
                    &mut hunks,
                );
            }
            old_line = old_at + 1;
            new_line = new_at + 1;
        }
        hunks
    }

    /// Adds the hunks of lines no line matched: a line edited in place, at
    /// either end of the stretch, is narrowed on its own, so an edit to it
    /// does not swallow the lines inserted or removed next to it.
    fn edited_lines(
        old: &str,
        new: &str,
        old_lines: &[(usize, usize)],
        new_lines: &[(usize, usize)],
        (mut old_from, mut old_to): (usize, usize),
        (mut new_from, mut new_to): (usize, usize),
        hunks: &mut Vec<Hunk>,
    ) {
        let similar = |old_at: usize, new_at: usize| {
            similar(key(old, old_lines[old_at]), key(new, new_lines[new_at]))
        };
        while old_from < old_to && new_from < new_to && similar(old_from, new_from) {
            narrowed(old, new, old_lines[old_from], new_lines[new_from], hunks);
            old_from += 1;
            new_from += 1;
        }
        let mut tail = Vec::new();
        while old_from < old_to && new_from < new_to && similar(old_to - 1, new_to - 1) {
            old_to -= 1;
            new_to -= 1;
            tail.push((old_lines[old_to], new_lines[new_to]));
        }
        narrowed(
            old,
            new,
            span(old_lines, old_from, old_to, old.len()),
            span(new_lines, new_from, new_to, new.len()),
            hunks,
        );
        for (old_line, new_line) in tail.into_iter().rev() {
            narrowed(old, new, old_line, new_line, hunks);
        }
    }

    /// Whether two lines are one line edited: the bytes they share at either
    /// end are at least half the longer one.
    fn similar(old: &[u8], new: &[u8]) -> bool {
        if old.is_empty() || new.is_empty() {
            return false;
        }
        let shorter = old.len().min(new.len());
        let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take(shorter - prefix)
            .take_while(|(a, b)| a == b)
            .count();
        2 * (prefix + suffix) >= old.len().max(new.len())
    }

    /// Each line's byte range, its line break included.
    fn lines(text: &str) -> Vec<(usize, usize)> {
        let mut lines = Vec::new();
        let mut start = 0;
        for (at, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                lines.push((start, at + 1));
                start = at + 1;
            }
        }
        if start < text.len() {
            lines.push((start, text.len()));
        }
        lines
    }

    /// The part of a line between its leading and trailing whitespace.
    fn content(text: &str, (start, end): (usize, usize)) -> (usize, usize) {
        let line = &text.as_bytes()[start..end];
        let leading = line
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
        let trailing = line[leading..]
            .iter()
            .rev()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
        (start + leading, end - trailing)
    }

    fn key(text: &str, line: (usize, usize)) -> &[u8] {
        let (start, end) = content(text, line);
        &text.as_bytes()[start..end]
    }

    /// Whether two lines are the same line, perhaps indented or ended
    /// differently. A blank line has no content to match by, so it matches
    /// only a blank line ended the same way: an insertion before a CRLF
    /// blank line stays an insertion rather than an edit of that line.
    fn same_line(old: &str, new: &str, old_line: (usize, usize), new_line: (usize, usize)) -> bool {
        let (old_key, new_key) = (key(old, old_line), key(new, new_line));
        old_key == new_key
            && (!old_key.is_empty()
                || old.as_bytes()[old_line.0..old_line.1] == new.as_bytes()[new_line.0..new_line.1])
    }

    /// The bytes lines `from..to` cover, or where they would start.
    fn span(lines: &[(usize, usize)], from: usize, to: usize, len: usize) -> (usize, usize) {
        let start = lines.get(from).map_or(len, |line| line.0);
        let end = if to > from { lines[to - 1].1 } else { start };
        (start, end)
    }

    /// Pairs the lines of `old_range` and `new_range` whose content matches,
    /// in order: the shared lines at either end, then lines unique to both
    /// sides that keep their order, then the same for the stretches between.
    fn match_lines(
        old: &str,
        new: &str,
        old_lines: &[(usize, usize)],
        new_lines: &[(usize, usize)],
        (mut old_from, mut old_to): (usize, usize),
        (mut new_from, mut new_to): (usize, usize),
        matched: &mut Vec<(usize, usize)>,
    ) {
        while old_from < old_to
            && new_from < new_to
            && same_line(old, new, old_lines[old_from], new_lines[new_from])
        {
            matched.push((old_from, new_from));
            old_from += 1;
            new_from += 1;
        }
        let mut tail = Vec::new();
        while old_from < old_to
            && new_from < new_to
            && same_line(old, new, old_lines[old_to - 1], new_lines[new_to - 1])
        {
            old_to -= 1;
            new_to -= 1;
            tail.push((old_to, new_to));
        }
        if old_from < old_to && new_from < new_to {
            let anchors = unique_anchors(
                old,
                new,
                old_lines,
                new_lines,
                (old_from, old_to),
                (new_from, new_to),
            );
            if !anchors.is_empty() {
                let (mut old_at, mut new_at) = (old_from, new_from);
                for (old_anchor, new_anchor) in anchors {
                    match_lines(
                        old,
                        new,
                        old_lines,
                        new_lines,
                        (old_at, old_anchor),
                        (new_at, new_anchor),
                        matched,
                    );
                    matched.push((old_anchor, new_anchor));
                    old_at = old_anchor + 1;
                    new_at = new_anchor + 1;
                }
                match_lines(
                    old,
                    new,
                    old_lines,
                    new_lines,
                    (old_at, old_to),
                    (new_at, new_to),
                    matched,
                );
            }
        }
        matched.extend(tail.into_iter().rev());
    }

    /// The lines that occur once on each side of the stretch, paired, keeping
    /// the longest run whose order both sides agree on.
    fn unique_anchors(
        old: &str,
        new: &str,
        old_lines: &[(usize, usize)],
        new_lines: &[(usize, usize)],
        (old_from, old_to): (usize, usize),
        (new_from, new_to): (usize, usize),
    ) -> Vec<(usize, usize)> {
        let mut seen = FxHashMap::<&[u8], (u32, usize, u32, usize)>::default();
        for (at, line) in old_lines.iter().enumerate().take(old_to).skip(old_from) {
            let entry = seen.entry(key(old, *line)).or_insert((0, 0, 0, 0));
            entry.0 += 1;
            entry.1 = at;
        }
        for (at, line) in new_lines.iter().enumerate().take(new_to).skip(new_from) {
            let entry = seen.entry(key(new, *line)).or_insert((0, 0, 0, 0));
            entry.2 += 1;
            entry.3 = at;
        }
        let mut pairs = seen
            .into_iter()
            .filter(|(key, (in_old, _, in_new, _))| !key.is_empty() && *in_old == 1 && *in_new == 1)
            .map(|(_, (_, old_at, _, new_at))| (old_at, new_at))
            .collect::<Vec<_>>();
        pairs.sort_unstable();
        longest_increasing(&pairs)
    }

    /// The longest run of `pairs`, sorted by their first element, whose
    /// second elements increase too.
    fn longest_increasing(pairs: &[(usize, usize)]) -> Vec<(usize, usize)> {
        let mut tails: Vec<usize> = Vec::new();
        let mut previous = vec![usize::MAX; pairs.len()];
        for (at, &(_, new_at)) in pairs.iter().enumerate() {
            let place = tails.partition_point(|&tail| pairs[tail].1 < new_at);
            if place > 0 {
                previous[at] = tails[place - 1];
            }
            if place == tails.len() {
                tails.push(at);
            } else {
                tails[place] = at;
            }
        }
        let mut run = Vec::with_capacity(tails.len());
        let mut at = tails.last().copied().unwrap_or(usize::MAX);
        while at != usize::MAX {
            run.push(pairs[at]);
            at = previous[at];
        }
        run.reverse();
        run
    }

    /// Adds the hunk `old_span` → `new_span` became, less the bytes it shares
    /// at either end, unless it shares them all.
    fn narrowed(
        old: &str,
        new: &str,
        (mut old_start, mut old_end): (usize, usize),
        (mut new_start, mut new_end): (usize, usize),
        hunks: &mut Vec<Hunk>,
    ) {
        let (old_bytes, new_bytes) = (old.as_bytes(), new.as_bytes());
        while old_start < old_end
            && new_start < new_end
            && old_bytes[old_start] == new_bytes[new_start]
        {
            old_start += 1;
            new_start += 1;
        }
        while old_start > 0 && !(old.is_char_boundary(old_start) && new.is_char_boundary(new_start))
        {
            old_start -= 1;
            new_start -= 1;
        }
        while old_end > old_start
            && new_end > new_start
            && old_bytes[old_end - 1] == new_bytes[new_end - 1]
        {
            old_end -= 1;
            new_end -= 1;
        }
        while old_end < old.len()
            && !(old.is_char_boundary(old_end) && new.is_char_boundary(new_end))
        {
            old_end += 1;
            new_end += 1;
        }
        if old_start == old_end && new_start == new_end {
            return;
        }
        hunks.push(Hunk {
            old_start: old_start as u32,
            old_end: old_end as u32,
            new_start: new_start as u32,
            new_end: new_end as u32,
        });
    }
}

/// The three answers a remap can give about one stored identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remap<T> {
    /// The identity does not belong to the edited file. Leave the entry alone.
    Unrelated,
    /// The identity's new form. May equal the old one when the edit was below
    /// it.
    Moved(T),
    /// The identity did not survive the edit. The entry cannot be rewritten
    /// and its owning file is dirty.
    Lost,
}

impl<T> Remap<T> {
    pub fn is_lost(&self) -> bool {
        matches!(self, Self::Lost)
    }

    pub fn moved(self) -> Option<T> {
        match self {
            Self::Moved(value) => Some(value),
            _ => None,
        }
    }
}

/// What the edited file holds *after* it was re-indexed.
///
/// A [`PositionMap`] says where an offset would land; it cannot say whether
/// anything is still there. An edit that replaces one function with another of
/// the same length maps every identity onto a position that exists but names a
/// different thing, so every mapped identity is confirmed against these before
/// it is written back.
#[derive(Debug, Default)]
pub struct FileFacts {
    signature_positions: FxHashSet<TextSize>,
    decl_positions: FxHashSet<TextSize>,
    member_ids: FxHashSet<LuaMemberId>,
    syntax_ids: FxHashSet<LuaSyntaxId>,
    /// The ranges an identity keyed on an expression can still name: every
    /// expression in the new tree, and every range the re-indexed file
    /// registers an `Element` owner at. Owners and instances are keyed by
    /// whatever expression they were made on - a table literal, but also a
    /// `vgui.Create(..)` or `ents.Create(..)` call - so the mapped range has to
    /// land on an expression that still exists.
    owner_ranges: FxHashSet<TextRange>,
    /// Same-file type-decl locations with their kinds, backing the
    /// unique-covering-decl admission for synthesized `DocTagClass` ids in
    /// [`FileRemap::syntax_id`]: one entry per declaration site in this file.
    type_decl_spans: Vec<(LuaTypeDeclId, TextRange, LuaDeclTypeKind)>,
}

impl FileFacts {
    pub fn collect(db: &DbIndex, file_id: FileId) -> Self {
        let signature_positions = db
            .get_signature_index()
            .get_file_signature_ids(file_id)
            .map(|ids| ids.iter().map(|id| id.get_position()).collect())
            .unwrap_or_default();
        let decl_positions = db
            .get_decl_index()
            .get_decl_tree(&file_id)
            .map(|tree| tree.get_decls().keys().map(|id| id.position).collect())
            .unwrap_or_default();
        let member_ids = db
            .get_member_index()
            .get_file_members(file_id)
            .into_iter()
            .map(|member| member.get_id())
            .collect();
        let mut owner_ranges = db
            .get_member_index()
            .element_owner_ranges_in_file(file_id)
            .into_iter()
            .map(|range| range.value)
            .collect::<FxHashSet<_>>();
        let mut syntax_ids = FxHashSet::default();
        if let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) {
            for element in tree.get_red_root().descendants_with_tokens() {
                match element {
                    rowan::NodeOrToken::Node(node) => {
                        if LuaExpr::can_cast(node.kind().into()) {
                            owner_ranges.insert(node.text_range());
                        }
                        syntax_ids.insert(LuaSyntaxId::from_node(&node));
                    }
                    rowan::NodeOrToken::Token(token) => {
                        syntax_ids.insert(LuaSyntaxId::from_token(&token));
                    }
                }
            }
        }
        // `contextual_type_support` synthesizes `DocTagClass` ids from a type
        // decl's name-token ranges (`local_inference/mod.rs`), and those nodes
        // ride inside `Comment` trivia the red walk above never yields. The
        // range alone still names the decl site, so they are admitted by
        // geometry: any type decl declared in this file at that range counts.
        // Scoped-class synthesis (`ensure_scoped_class_type_decl`) registers
        // whole-file (chunk) spans instead, which no shift image ever equals;
        // those are admitted in `syntax_id` by unique covering decl (see
        // `FileFacts::unique_covering_decl`). Both arms share this span table.
        let mut type_decl_spans: Vec<(LuaTypeDeclId, TextRange, LuaDeclTypeKind)> = Vec::new();
        if let Some(decl_ids) = db.get_type_index().get_file_type_decl_ids(file_id) {
            for decl_id in decl_ids {
                if let Some(type_decl) = db.get_type_index().get_type_decl(decl_id) {
                    let kind = type_decl.kind_and_flags().0;
                    for location in type_decl.get_locations() {
                        if location.file_id == file_id {
                            syntax_ids.insert(LuaSyntaxId::new(
                                LuaKind::Syntax(LuaSyntaxKind::DocTagClass),
                                location.range,
                            ));
                            type_decl_spans.push((decl_id.clone(), location.range, kind));
                        }
                    }
                }
            }
        }
        Self {
            signature_positions,
            decl_positions,
            member_ids,
            syntax_ids,
            owner_ranges,
            type_decl_spans,
        }
    }

    /// The unique same-file declaration site covering `range`, if exactly one
    /// exists. Backs the synthesized-`DocTagClass` admission in
    /// [`FileRemap::syntax_id`]: identity comes from uniqueness (same file,
    /// a single covering site — a same-span rename keeps one site, a deletion
    /// leaves none, a same-span collision leaves several), never from a name
    /// or geometric guess. Only `Class` sites count: whole-file (chunk) spans
    /// are produced solely by scoped-class synthesis
    /// (`ensure_scoped_class_type_decl`), so a candidate of any other kind
    /// cannot be the synthesis the id names.
    fn unique_covering_decl(&self, range: TextRange) -> Option<TextRange> {
        let mut covering: Option<TextRange> = None;
        for (_, span, kind) in &self.type_decl_spans {
            if *kind != LuaDeclTypeKind::Class {
                continue;
            }
            if span.start() <= range.start() && range.end() <= span.end() {
                if covering.is_some() {
                    return None;
                }
                covering = Some(*span);
            }
        }
        covering
    }
}

/// A [`PositionMap`] bound to the file it describes, so a caller cannot apply
/// it to an identity from another file.
#[derive(Debug)]
pub struct FileRemap {
    pub file_id: FileId,
    pub map: PositionMap,
    /// Absent while the new index does not exist yet - the pre-edit export map
    /// is built before the file is re-indexed, and an identity that maps onto
    /// nothing simply reads as a removed export there.
    facts: Option<FileFacts>,
}

impl FileRemap {
    /// A remap that moves nothing, for reading a file that was not edited.
    pub fn identity(file_id: FileId) -> Self {
        Self {
            file_id,
            map: PositionMap::identity(),
            facts: None,
        }
    }

    /// A remap whose results are taken on trust, for use before the file has
    /// been re-indexed.
    pub fn unvalidated(file_id: FileId, map: PositionMap) -> Self {
        Self {
            file_id,
            map,
            facts: None,
        }
    }

    /// A remap that confirms every mapped identity against the file's current
    /// index and tree. Use after `update_index(file_id)`.
    pub fn validated(db: &DbIndex, file_id: FileId, map: PositionMap) -> Self {
        Self {
            file_id,
            facts: (!map.is_identity()).then(|| FileFacts::collect(db, file_id)),
            map,
        }
    }

    fn owns(&self, file_id: FileId) -> bool {
        file_id == self.file_id
    }

    fn pos(&self, file_id: FileId, position: TextSize) -> Remap<TextSize> {
        if !self.owns(file_id) {
            return Remap::Unrelated;
        }
        match self.map.map(position) {
            Some(mapped) => Remap::Moved(mapped),
            None => Remap::Lost,
        }
    }

    fn range(&self, file_id: FileId, range: TextRange) -> Remap<TextRange> {
        if !self.owns(file_id) {
            return Remap::Unrelated;
        }
        match self.map.map_range(range) {
            Some(mapped) => Remap::Moved(mapped),
            None => Remap::Lost,
        }
    }

    /// Whether the position sits entirely in the edit's unchanged prefix.
    ///
    /// `map` returns an offset unchanged exactly when it lies before the
    /// hunk, and those bytes are identical in both texts, so the code the
    /// identity names is still there whatever the index happens to enumerate.
    /// Checking existence for it would reject live identities the facts
    /// cannot see - a `DocTagClass` node, say, which the syntax tree's
    /// `descendants_with_tokens` walk does not reach. Judged by geometry, not
    /// by whether the map happens to return the original value: a replacement
    /// that swaps one function for another at the same offset also maps the
    /// offset to itself, and that identity is *not* in the unchanged prefix.
    fn pos_is_before_the_hunk(&self, position: TextSize) -> bool {
        u32::from(position) < self.map.first_change()
    }

    /// The range counterpart, which also needs the byte after the range
    /// unchanged: a node ending where an insertion starts keeps its bytes but
    /// may have grown into the insertion, so its old range is checked against
    /// the index.
    fn range_is_before_the_hunk(&self, range: TextRange) -> bool {
        u32::from(range.end()) < self.map.first_change()
    }

    /// Resolves a mapped identity against the re-indexed file.
    ///
    /// An identity entirely in the unchanged prefix was untouched by the edit,
    /// so its stored form is already correct whatever the index enumerates.
    /// Otherwise the identity moves onto its mapped position: if that position
    /// is occupied in the new index the reference is rewritten to it (a
    /// collision - what used to sit at the old offset now sits at its image,
    /// even when the old offset is occupied again by something else), and if
    /// it is not, the identity is lost.
    ///
    /// The one exception is an identity the file's own index still holds at
    /// its original position while the image is empty. Such an entry has not
    /// been rebuilt by the re-index yet, so it and the reference to it must
    /// stay in step rather than one being rewritten and the other not.
    fn resolve<T>(
        &self,
        mapped: Remap<T>,
        before_the_hunk: bool,
        original_occupied: bool,
        exists: impl Fn(&FileFacts, &T) -> bool,
    ) -> Remap<T> {
        let Some(facts) = &self.facts else {
            return mapped;
        };
        // `Unrelated` here means the identity is not in this file at all, so
        // the file's own facts say nothing about it.
        if matches!(mapped, Remap::Unrelated) || before_the_hunk {
            return mapped;
        }
        match &mapped {
            Remap::Moved(value) if exists(facts, value) => mapped,
            Remap::Moved(_) | Remap::Lost if original_occupied => Remap::Unrelated,
            Remap::Moved(_) | Remap::Lost => Remap::Lost,
            Remap::Unrelated => Remap::Unrelated,
        }
    }

    pub fn signature_id(&self, id: LuaSignatureId) -> Remap<LuaSignatureId> {
        let mapped = match self.pos(id.get_file_id(), id.get_position()) {
            Remap::Moved(position) => Remap::Moved(LuaSignatureId::new(id.get_file_id(), position)),
            Remap::Unrelated => Remap::Unrelated,
            Remap::Lost => Remap::Lost,
        };
        self.resolve(
            mapped,
            self.pos_is_before_the_hunk(id.get_position()),
            self.facts
                .as_ref()
                .is_some_and(|facts| facts.signature_positions.contains(&id.get_position())),
            |facts, id| facts.signature_positions.contains(&id.get_position()),
        )
    }

    pub fn decl_id(&self, id: LuaDeclId) -> Remap<LuaDeclId> {
        let mapped = match self.pos(id.file_id, id.position) {
            Remap::Moved(position) => Remap::Moved(LuaDeclId::new(id.file_id, position)),
            Remap::Unrelated => Remap::Unrelated,
            Remap::Lost => Remap::Lost,
        };
        self.resolve(
            mapped,
            self.pos_is_before_the_hunk(id.position),
            self.facts
                .as_ref()
                .is_some_and(|facts| facts.decl_positions.contains(&id.position)),
            |facts, id| facts.decl_positions.contains(&id.position),
        )
    }

    pub fn syntax_id(&self, file_id: FileId, id: LuaSyntaxId) -> Remap<LuaSyntaxId> {
        let mapped = match self.range(file_id, id.get_range()) {
            Remap::Moved(range) => Remap::Moved(with_range(id, range)),
            Remap::Unrelated => Remap::Unrelated,
            Remap::Lost => Remap::Lost,
        };
        // Synthesized `DocTagClass` ids from whole-file (chunk) decl spans
        // (`ensure_scoped_class_type_decl`) never equal their shift image, so
        // the exact admission in `FileFacts::collect` cannot see them. When
        // the mapped range has exactly one covering declaration site in this
        // file, the fact is re-homed onto that site's location: uniqueness is
        // the identity (a same-span rename keeps one site, a deletion leaves
        // none, a same-span collision leaves several — both stay `Lost`).
        if !id.is_token() && id.get_kind() == LuaSyntaxKind::DocTagClass && self.owns(file_id) {
            if let Remap::Moved(moved) = &mapped {
                let mapped_range = moved.get_range();
                if !self
                    .facts
                    .as_ref()
                    .is_some_and(|facts| facts.syntax_ids.contains(moved))
                    && let Some(cover) = self
                        .facts
                        .as_ref()
                        .and_then(|facts| facts.unique_covering_decl(mapped_range))
                {
                    return Remap::Moved(with_range(id, cover));
                }
            }
        }
        self.resolve(
            mapped,
            self.range_is_before_the_hunk(id.get_range()),
            self.facts
                .as_ref()
                .is_some_and(|facts| facts.syntax_ids.contains(&id)),
            |facts, value| facts.syntax_ids.contains(value),
        )
    }

    pub fn member_id(&self, id: LuaMemberId) -> Remap<LuaMemberId> {
        let mapped = match self.range(id.file_id, id.get_syntax_id().get_range()) {
            Remap::Moved(range) => Remap::Moved(LuaMemberId::new_in_slot(
                with_range(*id.get_syntax_id(), range),
                id.file_id,
                id.get_slot(),
            )),
            Remap::Unrelated => Remap::Unrelated,
            Remap::Lost => Remap::Lost,
        };
        self.resolve(
            mapped,
            self.range_is_before_the_hunk(id.get_syntax_id().get_range()),
            self.facts
                .as_ref()
                .is_some_and(|facts| facts.member_ids.contains(&id)),
            |facts, id| facts.member_ids.contains(id),
        )
    }

    /// A table literal's range, the identity behind `TableConst`, `Instance`
    /// and `LuaMemberOwner::Element`.
    pub fn table_range(&self, range: &InFiled<TextRange>) -> Remap<InFiled<TextRange>> {
        let mapped = match self.range(range.file_id, range.value) {
            Remap::Moved(value) => Remap::Moved(InFiled::new(range.file_id, value)),
            Remap::Unrelated => Remap::Unrelated,
            Remap::Lost => Remap::Lost,
        };
        self.resolve(
            mapped,
            self.range_is_before_the_hunk(range.value),
            self.facts
                .as_ref()
                .is_some_and(|facts| facts.owner_ranges.contains(&range.value)),
            |facts, range| facts.owner_ranges.contains(&range.value),
        )
    }

    pub fn type_owner(&self, owner: &LuaTypeOwner) -> Remap<LuaTypeOwner> {
        match owner {
            LuaTypeOwner::Decl(id) => map_into(self.decl_id(*id), LuaTypeOwner::Decl),
            LuaTypeOwner::Member(id) => map_into(self.member_id(*id), LuaTypeOwner::Member),
            LuaTypeOwner::SyntaxId(id) => map_into(self.syntax_id(id.file_id, id.value), |value| {
                LuaTypeOwner::SyntaxId(InFiled::new(id.file_id, value))
            }),
        }
    }

    pub fn definition_id(&self, id: &LuaDefinitionId) -> Remap<LuaDefinitionId> {
        match id {
            LuaDefinitionId::Declaration(decl_id) => {
                map_into(self.decl_id(*decl_id), LuaDefinitionId::Declaration)
            }
            LuaDefinitionId::Assignment {
                file_id,
                assignment,
                target_idx,
            } => map_into(self.syntax_id(*file_id, *assignment), |assignment| {
                LuaDefinitionId::Assignment {
                    file_id: *file_id,
                    assignment,
                    target_idx: *target_idx,
                }
            }),
        }
    }

    pub fn inference_node(&self, node: &LuaInferenceNodeId) -> Remap<LuaInferenceNodeId> {
        match node {
            LuaInferenceNodeId::TypeOwner(owner) => {
                map_into(self.type_owner(owner), LuaInferenceNodeId::TypeOwner)
            }
            LuaInferenceNodeId::Definition(definition) => map_into(
                self.definition_id(definition),
                LuaInferenceNodeId::Definition,
            ),
            LuaInferenceNodeId::SignatureParam {
                signature_id,
                param_idx,
            } => map_into(self.signature_id(*signature_id), |signature_id| {
                LuaInferenceNodeId::SignatureParam {
                    signature_id,
                    param_idx: *param_idx,
                }
            }),
        }
    }

    pub fn semantic_decl_id(&self, id: &LuaSemanticDeclId) -> Remap<LuaSemanticDeclId> {
        match id {
            // A type decl is named, not positioned.
            LuaSemanticDeclId::TypeDecl(_) => Remap::Unrelated,
            LuaSemanticDeclId::Member(member_id) => {
                map_into(self.member_id(*member_id), LuaSemanticDeclId::Member)
            }
            LuaSemanticDeclId::LuaDecl(decl_id) => {
                map_into(self.decl_id(*decl_id), LuaSemanticDeclId::LuaDecl)
            }
            LuaSemanticDeclId::Signature(signature_id) => map_into(
                self.signature_id(*signature_id),
                LuaSemanticDeclId::Signature,
            ),
        }
    }

    pub fn operator_owner(&self, owner: &LuaOperatorOwner) -> Remap<LuaOperatorOwner> {
        match owner {
            LuaOperatorOwner::Table(range) => {
                map_into(self.table_range(range), LuaOperatorOwner::Table)
            }
            LuaOperatorOwner::Type(_) => Remap::Unrelated,
        }
    }

    pub fn inferred_guard_owner(
        &self,
        owner: &LuaInferredGuardOwner,
    ) -> Remap<LuaInferredGuardOwner> {
        match owner {
            LuaInferredGuardOwner::GlobalPath {
                signature_id,
                state_mask,
                path,
            } => map_into(self.signature_id(*signature_id), |signature_id| {
                LuaInferredGuardOwner::GlobalPath {
                    signature_id,
                    state_mask: *state_mask,
                    path: path.clone(),
                }
            }),
        }
    }
}

fn map_into<T, U>(remap: Remap<T>, build: impl FnOnce(T) -> U) -> Remap<U> {
    match remap {
        Remap::Moved(value) => Remap::Moved(build(value)),
        Remap::Unrelated => Remap::Unrelated,
        Remap::Lost => Remap::Lost,
    }
}

/// The same node kind at a different range.
///
/// `LuaSyntaxId`'s kind field is private and its two accessors each read half
/// of it, so the round trip goes through whichever half `is_token` selects.
fn with_range(id: LuaSyntaxId, range: TextRange) -> LuaSyntaxId {
    let kind: LuaKind = if id.is_token() {
        id.get_token_kind().into()
    } else {
        id.get_kind().into()
    };
    LuaSyntaxId::new(kind, range)
}
