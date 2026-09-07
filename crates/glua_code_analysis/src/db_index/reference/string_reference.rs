use rowan::TextRange;
use rustc_hash::FxHashMap;
use smol_str::SmolStr;

#[derive(Debug)]
pub struct StringReference {
    string_references: FxHashMap<SmolStr, Vec<TextRange>>,
}

impl StringReference {
    pub fn new() -> Self {
        Self {
            string_references: FxHashMap::default(),
        }
    }

    pub fn add_string_reference(&mut self, string: &str, range: TextRange) {
        self.string_references
            .entry(SmolStr::new(string))
            .or_default()
            .push(range);
    }

    /// Reserves capacity for one file's worth of distinct strings.
    /// Capacity only; lengths and contents are unchanged.
    pub fn reserve(&mut self, additional: usize) {
        self.string_references.reserve(additional);
    }

    pub fn get_string_references(&self, string: &str) -> Vec<TextRange> {
        self.string_references
            .get(string)
            .cloned()
            .unwrap_or_default()
    }
}
