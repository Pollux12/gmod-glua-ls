//! Each file's function-body summaries: what its functions' syntax and local
//! references decide, built with its source layer so that other files' passes
//! read them instead of walking the bodies.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaAstToken, LuaChunk, LuaClosureExpr, LuaExpr, LuaIndexKey,
    LuaNameExpr, LuaReturnStat, LuaStat, LuaSyntaxId, LuaVarExpr,
};
use rowan::TextSize;
use rustc_hash::{FxHashMap, FxHashSet};

use super::{call_site_params, dynamic_field};
use crate::db_index::derivation::{DerivedFact, Fingerprint, fingerprint_of, hash_local, in_file};
use crate::{DbIndex, FileId, LuaDeclId, LuaOutParamRoot, LuaSignatureId};

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
    /// The methods each function defines on a parameter or its receiver
    /// before it can return: what every call leaves there.
    pub(super) installed_methods: FxHashMap<TextSize, Vec<InstalledMethod>>,
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
            && self.installed_methods.is_empty()
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
        hash_sorted(&self.installed_methods, state);
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

/// A method a function defines on a parameter or its receiver, as a derived
/// panel's `Setup` takes from its base's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstalledMethod {
    pub(crate) root: LuaOutParamRoot,
    pub(crate) name: String,
    /// The function written, whose type the field holds after a call.
    pub(crate) value: LuaSyntaxId,
}

impl Hash for InstalledMethod {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.root.hash(state);
        self.name.hash(state);
        hash_local(&self.value, state);
    }
}

/// The methods `closure` defines on a parameter or its receiver in its
/// top-level statements before any that can return, reading none of them, on
/// fields it sets nowhere else: a field the body may also set conditionally,
/// or on a parameter it reassigned first, holds what the call leaves there
/// only on some paths.
fn installed_methods(
    db: &DbIndex,
    file_id: FileId,
    closure: &LuaClosureExpr,
) -> Vec<InstalledMethod> {
    let Some(references) = db.get_reference_index().get_local_reference(&file_id) else {
        return Vec::new();
    };
    let Some(block) = closure.get_block() else {
        return Vec::new();
    };
    let mut roots = closure
        .get_params_list()
        .into_iter()
        .flat_map(|params| params.get_params())
        .enumerate()
        .filter_map(|(idx, param)| {
            let token = param.get_name_token()?;
            Some((
                LuaDeclId::new(file_id, token.get_range().start()),
                LuaOutParamRoot::Param(idx),
            ))
        })
        .collect::<Vec<_>>();
    roots.extend(
        call_site_params::implicit_receiver_decl_id(file_id, closure)
            .map(|decl_id| (decl_id, LuaOutParamRoot::SelfReceiver)),
    );
    if roots.is_empty() {
        return Vec::new();
    }
    let root_of = |expr: &LuaExpr| {
        let LuaExpr::NameExpr(name_expr) = expr else {
            return None;
        };
        let decl_id = references.get_decl_id(&name_expr.get_range())?;
        roots.iter().find(|(root, _)| *root == decl_id).cloned()
    };
    let mut installed = Vec::<(LuaDeclId, InstalledMethod)>::new();
    let mut elsewhere = FxHashSet::<(LuaDeclId, String)>::default();
    let mut reassigned = FxHashSet::<LuaDeclId>::default();
    let mut may_have_returned = false;
    for stat in block.get_stats() {
        let top_level = match &stat {
            LuaStat::AssignStat(assign) if !may_have_returned => Some(assign.clone()),
            _ => None,
        };
        for assign in stat.descendants::<LuaAssignStat>() {
            if !in_body(&assign, closure) {
                continue;
            }
            let unconditional = top_level.as_ref() == Some(&assign);
            let (vars, values) = assign.get_var_and_expr_list();
            for (var, value) in vars
                .into_iter()
                .zip(values.into_iter().map(Some).chain(std::iter::repeat(None)))
            {
                match var {
                    LuaVarExpr::NameExpr(name_expr) => {
                        if let Some((decl_id, _)) = root_of(&LuaExpr::NameExpr(name_expr)) {
                            reassigned.insert(decl_id);
                        }
                    }
                    LuaVarExpr::IndexExpr(index_expr) => {
                        let Some(LuaIndexKey::Name(name)) = index_expr.get_index_key() else {
                            continue;
                        };
                        let Some((decl_id, root)) = index_expr
                            .get_prefix_expr()
                            .and_then(|prefix| root_of(&prefix))
                        else {
                            continue;
                        };
                        let name = name.get_name_text().to_string();
                        // A method the body defines there is its own to type;
                        // any other value can rest on what the callers pass.
                        let method = value.filter(|value| {
                            matches!(value, LuaExpr::ClosureExpr(_))
                                && !value.descendants::<LuaNameExpr>().any(|name_expr| {
                                    root_of(&LuaExpr::NameExpr(name_expr)).is_some()
                                })
                        });
                        match method {
                            Some(method) if unconditional && !reassigned.contains(&decl_id) => {
                                installed.retain(|(installed_decl, installed)| {
                                    *installed_decl != decl_id || installed.name != name
                                });
                                installed.push((
                                    decl_id,
                                    InstalledMethod {
                                        root,
                                        name,
                                        value: method.get_syntax_id(),
                                    },
                                ));
                            }
                            _ => {
                                elsewhere.insert((decl_id, name));
                            }
                        }
                    }
                }
            }
        }
        for func_stat in stat.descendants::<glua_parser::LuaFuncStat>() {
            if !in_body(&func_stat, closure) {
                continue;
            }
            if let Some(LuaVarExpr::IndexExpr(index_expr)) = func_stat.get_func_name()
                && let Some(LuaIndexKey::Name(name)) = index_expr.get_index_key()
                && let Some((decl_id, _)) = index_expr
                    .get_prefix_expr()
                    .and_then(|prefix| root_of(&prefix))
            {
                elsewhere.insert((decl_id, name.get_name_text().to_string()));
            }
        }
        may_have_returned |= stat
            .descendants::<LuaReturnStat>()
            .any(|ret| in_body(&ret, closure));
    }
    installed
        .into_iter()
        .filter(|(decl_id, method)| !elsewhere.contains(&(*decl_id, method.name.clone())))
        .map(|(_, method)| method)
        .collect()
}

/// Whether `node` runs as part of `closure`'s body, not a function within it.
fn in_body<N: LuaAstNode>(node: &N, closure: &LuaClosureExpr) -> bool {
    node.ancestors::<LuaClosureExpr>().next().as_ref() == Some(closure)
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
        installed_methods: root
            .descendants::<LuaClosureExpr>()
            .filter_map(|closure| {
                let methods = installed_methods(db, file_id, &closure);
                (!methods.is_empty()).then(|| (closure.get_position(), methods))
            })
            .collect(),
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

/// Names the methods of `file_id` that define methods on their receiver, as
/// an `---@outparam self.<field>` does, so a call through a receiver whose
/// type is not known yet is still followed up.
pub(super) fn register_receiver_writers(
    db: &mut DbIndex,
    file_id: FileId,
    root: &LuaChunk,
    summaries: &FileFunctionSummaries,
) {
    for closure in root.descendants::<LuaClosureExpr>() {
        let installs_on_receiver = summaries
            .installed_methods
            .get(&closure.get_position())
            .is_some_and(|methods| {
                methods
                    .iter()
                    .any(|method| method.root == LuaOutParamRoot::SelfReceiver)
            });
        if installs_on_receiver
            && let Some(name) = super::doc::receiver_member_name_for_closure(&closure)
        {
            db.get_signature_index_mut()
                .add_receiver_out_param_member_name(file_id, name);
        }
    }
}

/// `file_id`'s function summaries, recorded as a read.
pub(super) fn function_summaries(
    db: &DbIndex,
    file_id: FileId,
) -> Option<Arc<FileFunctionSummaries>> {
    db.get_function_summaries(file_id)
}

/// The methods the function `signature_id` declares defines on a parameter
/// or its receiver on every call, recorded as a read of its file's summaries.
pub(crate) fn installed_methods_of(
    db: &DbIndex,
    signature_id: LuaSignatureId,
) -> Vec<InstalledMethod> {
    function_summaries(db, signature_id.get_file_id())
        .and_then(|summaries| {
            summaries
                .installed_methods
                .get(&signature_id.get_position())
                .cloned()
        })
        .unwrap_or_default()
}
