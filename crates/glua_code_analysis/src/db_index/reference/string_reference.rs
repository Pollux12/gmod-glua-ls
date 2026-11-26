use crate::db_index::{SiteList, intern::InternedName};
use rowan::TextRange;
use rustc_hash::FxHashMap;

#[derive(Debug, Clone)]
pub struct StringReference {
    string_references: FxHashMap<InternedName, SiteList<TextRange>>,
}

impl StringReference {
    pub fn new() -> Self {
        Self {
            string_references: FxHashMap::default(),
        }
    }

    pub fn add_string_reference(&mut self, string: &str, range: TextRange) {
        self.string_references
            .entry(InternedName::new(string))
            .or_default()
            .push(range);
    }

    pub fn get_string_references(&self, string: &str) -> Vec<TextRange> {
        self.string_references
            .get(string)
            .map(|ranges| ranges.to_vec())
            .unwrap_or_default()
    }

    /// Every indexed string literal and its reference ranges, borrowed.
    pub fn iter_string_references(&self) -> impl Iterator<Item = (&str, &[TextRange])> {
        self.string_references
            .iter()
            .map(|(string, ranges)| (string.as_str(), &**ranges))
    }
}
