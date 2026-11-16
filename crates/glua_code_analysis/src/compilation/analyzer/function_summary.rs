//! Each file's function-body summaries: what its functions' syntax and local
//! references decide, built with its source layer so that other files' passes
//! read them instead of walking the bodies.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use glua_parser::{LuaAstNode, LuaChunk};
use rowan::TextSize;
use rustc_hash::{FxHashMap, FxHashSet};

use super::{call_site_params, dynamic_field};
use crate::db_index::derivation::{DerivedFact, Fingerprint, fingerprint_of, hash_local, in_file};
use crate::{DbIndex, FileId, LuaSignatureId};

/// A file's function summaries, each keyed by its closure's position.
#[derive(Debug, Default)]
pub(crate) struct FileFunctionSummaries {
    /// The writes of each closure whose key is built from its parameters.
    pub(super) field_installers: FxHashMap<TextSize, Vec<dynamic_field::FieldInstaller>>,
    /// The parameters each function statement assigns to.
    pub(super) mutated_params: FxHashMap<TextSize, Vec<usize>>,
    /// The parameters each function installs named members on.
    pub(super) installer_params: FxHashMap<TextSize, Vec<usize>>,
    /// The parameters each function assigns named fields onto.
    pub(super) filled_params: FxHashMap<TextSize, Vec<usize>>,
    /// The closures whose unmutated explicit `self` parameter is returned.
    pub(super) returns_explicit_self: FxHashSet<TextSize>,
    /// The closures whose parameters take their callers' arguments where they
    /// are read: local functions, and those stored on a local table.
    pub(super) locally_called: FxHashSet<TextSize>,
    /// Access path → the signatures of the function statements declaring it,
    /// by position. Left out of the fingerprint: only the file's own passes
    /// read it, and its source layer replaces them.
    pub(super) source_signatures: FxHashMap<String, Vec<LuaSignatureId>>,
}

impl FileFunctionSummaries {
    fn is_empty(&self) -> bool {
        self.field_installers.is_empty()
            && self.mutated_params.is_empty()
            && self.installer_params.is_empty()
            && self.filled_params.is_empty()
            && self.returns_explicit_self.is_empty()
            && self.locally_called.is_empty()
            && self.source_signatures.is_empty()
    }

    /// The last function statement at or before `position` declaring `path`.
    pub(super) fn source_signature_at(
        &self,
        path: &str,
        position: TextSize,
    ) -> Option<LuaSignatureId> {
        self.source_signatures
            .get(path)?
            .iter()
            .rev()
            .copied()
            .find(|signature_id| signature_id.get_position() <= position)
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
        hash_sorted(&self.field_installers, state);
        hash_sorted(&self.mutated_params, state);
        hash_sorted(&self.installer_params, state);
        hash_sorted(&self.filled_params, state);
        for positions in [&self.returns_explicit_self, &self.locally_called] {
            let mut positions = positions.iter().collect::<Vec<_>>();
            positions.sort();
            positions.len().hash(state);
            for position in positions {
                hash_local(position, state);
            }
        }
    }
}

impl DerivedFact for FileFunctionSummaries {
    fn fingerprint(&self) -> Fingerprint {
        fingerprint_of(self)
    }
}

/// `file_id`'s function summaries and their fingerprint, absent when it has
/// none: part of its source layer, after its references are complete.
pub(super) fn collect_function_summaries(
    db: &DbIndex,
    file_id: FileId,
    root: &LuaChunk,
) -> Option<(Arc<FileFunctionSummaries>, Fingerprint)> {
    let (mutated_params, installer_params, filled_params, returns_explicit_self) =
        call_site_params::collect_param_summaries(db, file_id, root);
    let summaries = FileFunctionSummaries {
        field_installers: dynamic_field::collect_field_installers(db, file_id, root),
        mutated_params,
        installer_params,
        filled_params,
        returns_explicit_self,
        locally_called: root
            .descendants::<glua_parser::LuaClosureExpr>()
            .map(|closure| closure.get_position())
            .filter(|position| {
                crate::semantic::takes_local_call_site_args(
                    db,
                    LuaSignatureId::new(file_id, *position),
                )
            })
            .collect(),
        source_signatures: call_site_params::source_signatures_in_file(file_id, root),
    };
    if summaries.is_empty() {
        return None;
    }
    let summaries = Arc::new(summaries);
    let fingerprint = in_file(Some(file_id), || summaries.fingerprint()).max(1);
    Some((summaries, fingerprint))
}

/// `file_id`'s function summaries, recorded as a read.
pub(super) fn function_summaries(
    db: &DbIndex,
    file_id: FileId,
) -> Option<Arc<FileFunctionSummaries>> {
    db.get_function_summaries(file_id)
}
