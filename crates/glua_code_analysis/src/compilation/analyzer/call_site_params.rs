use crate::{
    DbIndex, FileId, LuaDeclExtra, LuaInferCache, LuaSignatureId, LuaType, infer_expr,
    profile::Profile,
};
use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaFuncStat, LuaNameExpr,
    LuaVarExpr, PathTrait,
};
use rowan::TextSize;

use super::{AnalysisPipeline, AnalyzeContext};

pub struct CallSiteParamAnalysisPipeline;

impl AnalysisPipeline for CallSiteParamAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        let _p = Profile::cond_new("call-site param analyze", context.tree_list.len() > 1);
        let mut trees = context.tree_list.clone();
        trees.sort_by_key(|tree| tree.file_id.id);

        let source_signature_updates = trees
            .iter()
            .map(|tree| {
                let root = tree.value.get_root().clone();
                (tree.file_id, source_signatures_in_file(tree.file_id, &root))
            })
            .collect();
        db.get_call_site_param_index_mut()
            .set_files_source_signatures(source_signature_updates);

        // The contribution collection reads only immutable state (signature
        // index, the source-signature map just installed above, decl/reference
        // indexes from earlier passes) and writes to per-file local buffers, so
        // it runs concurrently across files. A fresh per-file infer cache is
        // used (the db is immutable during this pass, so a cold cache yields
        // identical inference). Results merge sequentially in file-id order.
        let file_ids: Vec<FileId> = trees.iter().map(|tree| tree.file_id).collect();
        let contribution_updates =
            super::parallel::map_files_collect(&*db, &file_ids, |db, file_id| {
                let mut contributions = Vec::new();
                let Some(root) = db
                    .get_vfs()
                    .get_syntax_tree(&file_id)
                    .map(|tree| tree.get_chunk_node())
                else {
                    return (file_id, contributions);
                };
                let mut cache = LuaInferCache::new(
                    file_id,
                    crate::CacheOptions {
                        analysis_phase: crate::LuaAnalysisPhase::Force,
                    },
                );
                for call_expr in root.syntax().descendants().filter_map(LuaCallExpr::cast) {
                    collect_call_site_param_types(
                        db,
                        &mut cache,
                        file_id,
                        call_expr,
                        &mut contributions,
                    );
                }
                (file_id, contributions)
            });
        db.get_call_site_param_index_mut()
            .set_files_contributions(contribution_updates);
    }
}

fn source_signatures_in_file(
    file_id: FileId,
    root: &glua_parser::LuaSyntaxNode,
) -> Vec<(String, LuaSignatureId)> {
    let mut funcs = root
        .descendants()
        .filter_map(LuaFuncStat::cast)
        .collect::<Vec<_>>();
    funcs.sort_by_key(|func| func.get_position());
    funcs
        .into_iter()
        .filter_map(|func_stat| {
            let path = func_stat
                .get_func_name()
                .and_then(|func_name| func_name.get_access_path())?;
            let closure = func_stat.get_closure()?;
            Some((
                path.to_string(),
                LuaSignatureId::from_closure(file_id, &closure),
            ))
        })
        .collect()
}

fn collect_call_site_param_types(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    call_expr: LuaCallExpr,
    contributions: &mut Vec<(LuaSignatureId, usize, LuaType)>,
) -> Option<()> {
    let args = call_expr.get_args_list()?;
    let useful_args = args
        .get_args()
        .enumerate()
        .filter(|(_, arg)| is_supported_call_site_arg_shape(db, file_id, arg))
        .collect::<Vec<_>>();
    if useful_args.is_empty() {
        return None;
    }

    let prefix_expr = call_expr.get_prefix_expr()?;
    let signature_id = signature_id_from_call_prefix(db, file_id, &prefix_expr)?;
    if !is_call_site_realm_compatible(db, file_id, call_expr.get_position(), signature_id) {
        return None;
    }

    let signature = db.get_signature_index().get(&signature_id)?;
    let colon_call_arg_shift = usize::from(call_expr.is_colon_call());
    for (arg_idx, arg) in useful_args {
        let param_idx = arg_idx + colon_call_arg_shift;
        if param_idx >= signature.params.len() || signature.param_docs.contains_key(&param_idx) {
            continue;
        }
        let Some(param_name) = signature.params.get(param_idx) else {
            continue;
        };
        if !has_gmod_param_name_hint(db, param_name) {
            continue;
        }
        if source_param_is_mutated(db, signature_id, param_idx) {
            continue;
        }
        let Some(arg_type) = infer_supported_call_site_arg_type(db, cache, file_id, arg) else {
            continue;
        };
        if arg_type.is_unknown() || arg_type.is_never() {
            continue;
        }

        contributions.push((signature_id, param_idx, arg_type));
    }

    Some(())
}

fn source_param_is_mutated(db: &DbIndex, signature_id: LuaSignatureId, param_idx: usize) -> bool {
    let Some(tree) = db.get_vfs().get_syntax_tree(&signature_id.get_file_id()) else {
        return true;
    };
    let root = tree.get_red_root();
    let Some(closure) = root
        .descendants()
        .filter_map(LuaClosureExpr::cast)
        .find(|closure| closure.get_position() == signature_id.get_position())
    else {
        return true;
    };
    let Some(param_name) = closure
        .get_params_list()
        .and_then(|params| params.get_params().nth(param_idx))
        .and_then(|param| param.get_name_token())
        .map(|token| token.get_name_text().to_string())
    else {
        return true;
    };
    let Some(block) = closure.get_block() else {
        return true;
    };

    block.descendants::<LuaAssignStat>().any(|assign| {
        if assign.ancestors::<LuaClosureExpr>().next().as_ref() != Some(&closure) {
            return false;
        }

        let (vars, _) = assign.get_var_and_expr_list();
        vars.iter().any(|var| var_writes_param(var, &param_name))
    })
}

fn var_writes_param(var: &LuaVarExpr, param_name: &str) -> bool {
    match var {
        LuaVarExpr::NameExpr(name_expr) => name_expr
            .get_name_text()
            .is_some_and(|name| name.as_str() == param_name),
        LuaVarExpr::IndexExpr(index_expr) => index_expr
            .get_prefix_expr()
            .is_some_and(|expr| expr_reads_param(&expr, param_name)),
    }
}

fn expr_reads_param(expr: &LuaExpr, param_name: &str) -> bool {
    match expr {
        LuaExpr::NameExpr(name_expr) => name_expr
            .get_name_text()
            .is_some_and(|name| name.as_str() == param_name),
        LuaExpr::IndexExpr(index_expr) => index_expr
            .get_prefix_expr()
            .is_some_and(|prefix| expr_reads_param(&prefix, param_name)),
        _ => false,
    }
}

fn has_gmod_param_name_hint(db: &DbIndex, param_name: &str) -> bool {
    if !db.get_emmyrc().gmod.enabled {
        return false;
    }

    let hints = &db.get_emmyrc().gmod.file_param_defaults;
    if hints.is_empty() {
        return false;
    }

    let lowercase_name = param_name.to_ascii_lowercase();
    hints
        .get(param_name)
        .or_else(|| hints.get(&lowercase_name))
        .is_some_and(|hint| !hint.trim().is_empty())
}

fn signature_id_from_call_prefix(
    db: &DbIndex,
    file_id: FileId,
    prefix_expr: &LuaExpr,
) -> Option<LuaSignatureId> {
    match prefix_expr {
        LuaExpr::NameExpr(name_expr) => signature_id_from_name_expr(db, file_id, name_expr),
        LuaExpr::IndexExpr(index_expr) => index_expr.get_access_path().and_then(|path| {
            db.get_call_site_param_index()
                .get_source_signature_for_file(path.as_str(), file_id)
        }),
        _ => None,
    }
}

fn signature_id_from_name_expr(
    db: &DbIndex,
    file_id: FileId,
    name_expr: &LuaNameExpr,
) -> Option<LuaSignatureId> {
    let name = name_expr.get_name_text()?;
    db.get_call_site_param_index()
        .get_source_signature_for_file(name.as_str(), file_id)
}

fn is_call_site_realm_compatible(
    db: &DbIndex,
    caller_file_id: FileId,
    caller_position: TextSize,
    signature_id: LuaSignatureId,
) -> bool {
    if !db.get_emmyrc().gmod.enabled {
        return true;
    }

    let infer_index = db.get_gmod_infer_index();
    let caller_mask = infer_index.get_state_mask_at_offset(&caller_file_id, caller_position);
    let candidate_mask = infer_index
        .get_state_mask_at_offset(&signature_id.get_file_id(), signature_id.get_position());
    caller_mask.is_compatible_with(candidate_mask)
}

fn infer_supported_call_site_arg_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    arg: LuaExpr,
) -> Option<LuaType> {
    match &arg {
        LuaExpr::LiteralExpr(_) => infer_expr(db, cache, arg).ok(),
        LuaExpr::CallExpr(call_expr) if is_zero_arg_call(call_expr) => {
            infer_expr(db, cache, arg).ok()
        }
        LuaExpr::NameExpr(name_expr) => {
            if is_mutable_local_name_arg(db, file_id, &arg) {
                return None;
            }

            let decl_id = db
                .get_reference_index()
                .get_local_reference(&file_id)
                .and_then(|refs| refs.get_decl_id(&name_expr.get_range()))?;
            let decl = db.get_decl_index().get_decl(&decl_id)?;
            if !matches!(decl.extra, LuaDeclExtra::Local { .. }) {
                return None;
            }
            let root = db.get_vfs().get_syntax_tree(&file_id)?.get_red_root();
            let value_node = decl.get_value_syntax_id()?.to_node_from_root(&root)?;
            let value_expr = LuaExpr::cast(value_node)?;
            match &value_expr {
                LuaExpr::LiteralExpr(_) => {}
                LuaExpr::CallExpr(call_expr) if is_zero_arg_call(call_expr) => {}
                _ => return None,
            }
            infer_expr(db, cache, value_expr).ok()
        }
        _ => None,
    }
}

fn is_supported_call_site_arg_shape(db: &DbIndex, file_id: FileId, arg: &LuaExpr) -> bool {
    match arg {
        LuaExpr::LiteralExpr(_) => true,
        LuaExpr::CallExpr(call_expr) => is_zero_arg_call(call_expr),
        LuaExpr::NameExpr(name_expr) => {
            if is_mutable_local_name_arg(db, file_id, arg) {
                return false;
            }
            let Some(decl_id) = db
                .get_reference_index()
                .get_local_reference(&file_id)
                .and_then(|refs| refs.get_decl_id(&name_expr.get_range()))
            else {
                return false;
            };
            let Some(decl) = db.get_decl_index().get_decl(&decl_id) else {
                return false;
            };
            if !matches!(decl.extra, LuaDeclExtra::Local { .. }) {
                return false;
            }
            let Some(root) = db
                .get_vfs()
                .get_syntax_tree(&file_id)
                .map(|tree| tree.get_red_root())
            else {
                return false;
            };
            let Some(value_expr) = decl
                .get_value_syntax_id()
                .and_then(|syntax_id| syntax_id.to_node_from_root(&root))
                .and_then(LuaExpr::cast)
            else {
                return false;
            };
            matches!(value_expr, LuaExpr::LiteralExpr(_))
                || matches!(&value_expr, LuaExpr::CallExpr(call_expr) if is_zero_arg_call(call_expr))
        }
        _ => false,
    }
}

fn is_mutable_local_name_arg(db: &DbIndex, file_id: FileId, arg: &LuaExpr) -> bool {
    let LuaExpr::NameExpr(name_expr) = arg else {
        return false;
    };
    let Some(file_refs) = db.get_reference_index().get_local_reference(&file_id) else {
        return false;
    };
    let Some(decl_id) = file_refs.get_decl_id(&name_expr.get_range()) else {
        return false;
    };

    file_refs
        .get_decl_references(&decl_id)
        .is_some_and(|decl_refs| decl_refs.mutable)
}

fn is_zero_arg_call(call_expr: &LuaCallExpr) -> bool {
    call_expr
        .get_args_list()
        .is_none_or(|args| args.get_args().next().is_none())
}
