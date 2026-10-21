use std::cmp::Reverse;

use rowan::{TextRange, TextSize};

/// One file's function scopes, sorted by start with an enclosing scope before
/// the scopes it nests, each linked to the innermost scope enclosing it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct FunctionScopes {
    ranges: Vec<TextRange>,
    parents: Vec<Option<u32>>,
}

fn order(range: TextRange) -> (TextSize, Reverse<TextSize>) {
    (range.start(), Reverse(range.end()))
}

impl FunctionScopes {
    pub(super) fn ranges(&self) -> &[TextRange] {
        &self.ranges
    }

    pub(super) fn insert(&mut self, range: TextRange) {
        let at = self
            .ranges
            .partition_point(|held| order(*held) <= order(range));
        self.ranges.insert(at, range);
        if at + 1 == self.ranges.len() {
            let parent = self.enclosing(at.checked_sub(1), |held| held.contains_range(range));
            self.parents.push(parent);
        } else {
            self.parents.clear();
            for index in 0..self.ranges.len() {
                let range = self.ranges[index];
                let parent =
                    self.enclosing(index.checked_sub(1), |held| held.contains_range(range));
                self.parents.push(parent);
            }
        }
    }

    /// The innermost scope holding `position`.
    pub(super) fn innermost(&self, position: TextSize) -> Option<TextRange> {
        let after = self
            .ranges
            .partition_point(|range| range.start() <= position);
        self.enclosing(after.checked_sub(1), |held| held.contains(position))
            .map(|index| self.ranges[index as usize])
    }

    /// The first scope `holds` accepts among `from` and the scopes enclosing
    /// it. Scopes nest, so one between `from` and its parent ends before
    /// `from` does and cannot hold what `from` does not.
    fn enclosing(&self, from: Option<usize>, holds: impl Fn(TextRange) -> bool) -> Option<u32> {
        let mut candidate = from;
        while let Some(index) = candidate {
            if holds(self.ranges[index]) {
                return Some(index as u32);
            }
            candidate = self.parents[index].map(|parent| parent as usize);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u32, end: u32) -> TextRange {
        TextRange::new(TextSize::new(start), TextSize::new(end))
    }

    fn scopes(ranges: &[TextRange]) -> FunctionScopes {
        let mut scopes = FunctionScopes::default();
        for range in ranges {
            scopes.insert(*range);
        }
        scopes
    }

    #[test]
    fn a_position_after_nested_siblings_is_held_by_their_enclosing_scope() {
        let outer = range(0, 100);
        let found = scopes(&[outer, range(10, 20), range(12, 18), range(30, 40)])
            .innermost(TextSize::new(50));
        assert_eq!(found, Some(outer));
    }

    #[test]
    fn scopes_sharing_a_start_find_the_inner_one_in_any_insertion_order() {
        let (outer, inner) = (range(0, 100), range(0, 50));
        let found = [[outer, inner], [inner, outer]]
            .map(|ranges| scopes(&ranges).innermost(TextSize::new(10)));
        assert_eq!(found, [Some(inner), Some(inner)]);
    }

    #[test]
    fn a_scope_inserted_before_others_relinks_them() {
        let (outer, inner) = (range(0, 100), range(60, 70));
        let found = scopes(&[inner, range(80, 90), outer]).innermost(TextSize::new(75));
        assert_eq!(found, Some(outer));
    }

    #[test]
    fn a_position_outside_every_scope_has_none() {
        let found = scopes(&[range(10, 20), range(30, 40)]).innermost(TextSize::new(25));
        assert_eq!(found, None);
    }
}
