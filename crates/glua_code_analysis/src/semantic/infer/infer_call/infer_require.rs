use glua_parser::{LuaAstNode, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaReturnStat};

use crate::{
    DbIndex, FileId, GmodStateMask, InFiled, InferFailReason, LuaDependencyKind, LuaInferCache,
    LuaType, ModuleInfo, SignatureReturnStatus, TypeOps, VariadicType, infer_expr,
    semantic::infer::InferResult,
};

pub(crate) enum LoadedFileCall {
    Always(FileId),
    Conditional(FileId),
    Skipped,
}

impl LoadedFileCall {
    pub(crate) fn file_id(self) -> Option<FileId> {
        match self {
            Self::Always(file_id) | Self::Conditional(file_id) => Some(file_id),
            Self::Skipped => None,
        }
    }
}

/// What a `require` call returns: the module `path_arg` names.
pub fn infer_require_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
    path_arg: LuaExpr,
) -> InferResult {
    let require_path_type = infer_expr(db, cache, path_arg)?;
    let module_path: String = match &require_path_type {
        LuaType::StringConst(module_path) => module_path.as_ref().to_string(),
        // A computed path names a module the analyzer cannot identify, so the
        // export type is unresolved rather than unconstrained.
        _ => {
            return Ok(LuaType::Unknown);
        }
    };

    let module_info = db
        .get_module_index()
        .find_module_for_file(&module_path, cache.get_file_id())
        .ok_or(InferFailReason::None)?;
    module_export(module_info, cache, call_expr)
}

/// What a call that runs another file returns: the value that file returns.
pub fn infer_loaded_file_call(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
    loaded: LoadedFileCall,
) -> InferResult {
    let (loaded_file_id, conditional) = match loaded {
        LoadedFileCall::Always(file_id) => (file_id, false),
        LoadedFileCall::Conditional(file_id) => (file_id, true),
        LoadedFileCall::Skipped => return Ok(LuaType::Nil),
    };
    let module_info = db
        .get_module_index()
        .get_module(loaded_file_id)
        .ok_or(InferFailReason::None)?;
    if module_info.export_type.is_none() && !returns_a_value(db, loaded_file_id) {
        return Ok(LuaType::Nil);
    }
    let export = module_export(module_info, cache, call_expr)?;
    if conditional && !export.is_nullable() {
        Ok(TypeOps::Union.apply(db, &export, &LuaType::Nil))
    } else {
        Ok(export)
    }
}

/// Whether the chunk of `file_id` has a `return` with a value.
fn returns_a_value(db: &DbIndex, file_id: FileId) -> bool {
    let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) else {
        return false;
    };
    tree.get_chunk_node()
        .descendants::<LuaReturnStat>()
        .any(|return_stat| {
            return_stat.ancestors::<LuaClosureExpr>().next().is_none()
                && return_stat.get_expr_list().next().is_some()
        })
}

/// The file a call runs: the one an `include` loads, or the one a
/// `CompileFile` compiled when its result is called.
pub(crate) fn loaded_file_of_call(
    db: &DbIndex,
    source_file_id: FileId,
    call_expr: &LuaCallExpr,
) -> Option<LoadedFileCall> {
    let compiled_call_range = compiled_function_call(call_expr).map(|call| call.get_range());
    let mut targets: Vec<(FileId, Option<GmodStateMask>)> = db
        .get_file_dependencies_index()
        .get_dependency_sites(&source_file_id)?
        .iter()
        .filter_map(|site| {
            if !site.returns_result {
                return None;
            }
            let runs_target = match site.kind {
                LuaDependencyKind::Include => site.call_range == call_expr.get_range(),
                LuaDependencyKind::CompileFile => compiled_call_range == Some(site.call_range),
                _ => false,
            };
            runs_target
                .then_some(site.target_file_id)
                .flatten()
                .map(|target| (target, site.realms))
        })
        .collect();
    if targets.is_empty() {
        return None;
    }
    let target_file_id = targets[0].0;
    if targets.iter().any(|(id, _)| *id != target_file_id) {
        return None;
    }
    let unconditional = targets.iter().any(|(_, realms)| realms.is_none());
    if unconditional {
        return Some(LoadedFileCall::Always(target_file_id));
    }
    let caller_realm = db
        .get_gmod_infer_index()
        .get_realm_at_offset(&source_file_id, call_expr.get_position());
    let caller_mask = GmodStateMask::from_realm(caller_realm);
    if caller_mask.is_empty() {
        return Some(LoadedFileCall::Always(target_file_id));
    }
    let mut site_mask = GmodStateMask::empty();
    for (_, realms) in targets.drain(..) {
        if let Some(realms) = realms {
            site_mask.insert(realms);
        }
    }
    if site_mask.contains(caller_mask) {
        Some(LoadedFileCall::Always(target_file_id))
    } else if site_mask.intersects(caller_mask) {
        Some(LoadedFileCall::Conditional(target_file_id))
    } else {
        Some(LoadedFileCall::Skipped)
    }
}

/// The call that made the function `call_expr` calls without arguments, as
/// in `CompileFile(path)()`.
fn compiled_function_call(call_expr: &LuaCallExpr) -> Option<LuaCallExpr> {
    if call_expr
        .get_args_list()
        .is_some_and(|args| args.get_args().next().is_some())
    {
        return None;
    }
    let mut prefix_expr = call_expr.get_prefix_expr()?;
    loop {
        match prefix_expr {
            LuaExpr::CallExpr(loader_call) => return Some(loader_call),
            LuaExpr::ParenExpr(paren_expr) => prefix_expr = paren_expr.get_expr()?,
            _ => return None,
        }
    }
}

fn module_export(
    module_info: &ModuleInfo,
    cache: &LuaInferCache,
    call_expr: &LuaCallExpr,
) -> InferResult {
    match &module_info.export_type {
        Some(ty) => match ty {
            LuaType::Def(id) => Ok(LuaType::Ref(id.clone())),
            _ => Ok(ty.clone()),
        },
        None => Err(InferFailReason::UnResolveExpr(InFiled::new(
            cache.get_file_id(),
            call_expr.get_syntax_id(),
        ))),
    }
}

/// Whether the function `call_expr` calls leaves its result unspecified, so a
/// load call returns what the file it runs returns: a declaration documenting
/// no particular return, or a value that is not a known function.
pub fn load_call_result_is_unspecified(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> bool {
    let Some(prefix_expr) = call_expr.get_prefix_expr() else {
        return true;
    };
    let Ok(LuaType::Signature(signature_id)) = infer_expr(db, cache, prefix_expr) else {
        return true;
    };
    let Some(signature) = db.get_signature_index().get(&signature_id) else {
        return true;
    };
    if signature.resolve_return == SignatureReturnStatus::DocResolve {
        return signature
            .return_docs
            .iter()
            .all(|doc| says_nothing(&doc.type_ref));
    }
    let is_declaration = db
        .get_module_index()
        .get_module(signature_id.get_file_id())
        .is_some_and(|module| module.is_meta);
    is_declaration || says_nothing(&signature.get_return_type())
}

fn says_nothing(typ: &LuaType) -> bool {
    match typ {
        LuaType::Any | LuaType::Unknown => true,
        LuaType::Union(union) => {
            union.types().any(says_nothing)
                && union
                    .types()
                    .all(|member| member.is_nil() || says_nothing(member))
        }
        LuaType::Variadic(variadic) => match variadic.as_ref() {
            VariadicType::Base(base) => says_nothing(base),
            VariadicType::Multi(types) => types.iter().all(says_nothing),
        },
        _ => false,
    }
}
