//! Each file's function-body summaries: what its functions' syntax and local
//! references decide, built with its source layer so that other files' passes
//! read them instead of walking the bodies.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use glua_parser::LuaChunk;
use rowan::TextSize;
use rustc_hash::{FxHashMap, FxHashSet};

use super::{call_site_params, dynamic_field};
use crate::db_index::derivation::{DerivedFact, Fingerprint, fingerprint_of, hash_local};
use crate::{DbIndex, FileId};

/// A file's function summaries, each keyed by its closure's position.
#[derive(Debug, Default)]
pub(crate) struct FileFunctionSummaries {
    /// The closures whose body writes `table_param[key_param]`.
    pub(super) field_setter_helpers: FxHashMap<TextSize, Vec<dynamic_field::FieldSetterHelper>>,
    /// The parameters each function statement assigns to.
    pub(super) mutated_params: FxHashMap<TextSize, Vec<usize>>,
    /// The closures whose unmutated explicit `self` parameter is returned.
    pub(super) returns_explicit_self: FxHashSet<TextSize>,
}

impl FileFunctionSummaries {
    fn is_empty(&self) -> bool {
        self.field_setter_helpers.is_empty()
            && self.mutated_params.is_empty()
            && self.returns_explicit_self.is_empty()
    }
}

fn hash_sorted<H: Hasher, V: Hash>(entries: &FxHashMap<TextSize, V>, state: &mut H) {
    let mut entries = entries.iter().collect::<Vec<_>>();
    entries.sort_by_key(|(position, _)| **position);
    entries.len().hash(state);
    for (position, value) in entries {
        hash_local(position, state);
        value.hash(state);
    }
}

impl Hash for FileFunctionSummaries {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_sorted(&self.field_setter_helpers, state);
        hash_sorted(&self.mutated_params, state);
        let mut returns_explicit_self = self.returns_explicit_self.iter().collect::<Vec<_>>();
        returns_explicit_self.sort();
        returns_explicit_self.len().hash(state);
        for position in returns_explicit_self {
            hash_local(position, state);
        }
    }
}

impl DerivedFact for FileFunctionSummaries {
    fn fingerprint(&self) -> Fingerprint {
        fingerprint_of(self)
    }
}

/// `file_id`'s function summaries, absent when it has none: part of its
/// source layer, after its references are complete.
pub(super) fn collect_function_summaries(
    db: &DbIndex,
    file_id: FileId,
    root: &LuaChunk,
) -> Option<Arc<FileFunctionSummaries>> {
    let (mutated_params, returns_explicit_self) =
        call_site_params::collect_param_summaries(db, file_id, root);
    let summaries = FileFunctionSummaries {
        field_setter_helpers: dynamic_field::collect_field_setter_helpers(root),
        mutated_params,
        returns_explicit_self,
    };
    (!summaries.is_empty()).then(|| Arc::new(summaries))
}

/// `file_id`'s function summaries, recorded as a read.
pub(super) fn function_summaries(
    db: &DbIndex,
    file_id: FileId,
) -> Option<Arc<FileFunctionSummaries>> {
    db.get_function_summaries(file_id)
}
