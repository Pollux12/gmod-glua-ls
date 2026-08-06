use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock},
};

use crate::db_index::{CallSiteReturnConsumer, CallSiteReturnConsumerTarget};
use crate::{
    DbIndex, FileId, InFiled, LuaDeclExtra, LuaDeclId, LuaDependencyKind, LuaInferCache,
    LuaInferenceConfidence, LuaInferenceEventId, LuaInferenceNodeId, LuaInferenceProvenanceKind,
    LuaInferenceStep, LuaMemberId, LuaMemberIndexItem, LuaMemberKey, LuaMemberOwner, LuaObjectType,
    LuaSemanticDeclId, LuaSignatureId, LuaType, LuaTypeDeclId, LuaTypeFact, LuaTypeOwner,
    WorkspaceId, find_signature_attribute_use, get_member_map, get_member_value_expr,
    get_prefix_expr_signature_id, infer_authoritative_method_self_type, infer_expr,
    infer_expr_semantic_decl, profile::Profile,
};
use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaAstToken, LuaCallExpr, LuaClosureExpr, LuaExpr, LuaFuncStat,
    LuaIfStat, LuaIndexKey, LuaLocalFuncStat, LuaLocalStat, LuaNameExpr, LuaReturnStat,
    LuaTableExpr, LuaTableField, LuaVarExpr, PathTrait,
};
use rowan::{TextRange, TextSize};

use super::{
    AnalysisPipeline, AnalyzeContext,
    gmod::{is_vgui_register_table_call, vgui_register_table_type_decl_id},
    unresolve::{UnResolve, UnResolveDecl, UnResolveMember},
};

pub struct CallSiteParamAnalysisPipeline;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignatureLookupKind {
    Direct,
    InferredCallable,
    IncludeReturnedMember,
}

#[derive(Debug)]
struct ReturnedTableInfo {
    member_signatures: Vec<ReturnedMemberSignature>,
}

#[derive(Debug)]
struct ReturnedMemberSignature {
    member_id: LuaMemberId,
    signature_id: LuaSignatureId,
    history: LuaMemberIndexItem,
}

// Shared by all parallel file workers in one analysis pass. Each exact include
// target gets its own once-cell, so different targets initialize concurrently
// while each target's return, members, and histories are scanned at most once,
// including misses.
type ReturnedTableCache = Mutex<HashMap<FileId, Arc<OnceLock<Option<Arc<ReturnedTableInfo>>>>>>;

const RETURN_ALIAS_ATTRIBUTE: &str = "return_alias";

impl AnalysisPipeline for CallSiteParamAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        analyze_call_site_param_files(db, context);
    }
}

fn analyze_call_site_param_files(db: &mut DbIndex, context: &mut AnalyzeContext) {
    let mut file_ids = context
        .tree_list
        .iter()
        .map(|tree| tree.file_id)
        .collect::<Vec<_>>();
    if file_ids.is_empty() {
        return;
    }
    let _p = Profile::cond_new("call-site param analyze", file_ids.len() > 1);
    file_ids.sort_by_key(|file_id| file_id.id);

    let file_metadata = super::parallel::map_files_collect(&*db, &file_ids, |db, file_id| {
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_chunk_node())
        else {
            return (file_id, Vec::new(), HashSet::new());
        };
        (
            file_id,
            source_signatures_in_file(db, file_id, root.syntax()),
            exact_receiver_member_keys_in_file(&root),
        )
    });
    let mut source_signature_updates = Vec::with_capacity(file_metadata.len());
    let mut exact_receiver_member_keys = HashSet::new();
    for (file_id, source_signatures, member_keys) in file_metadata {
        source_signature_updates.push((file_id, source_signatures));
        exact_receiver_member_keys.extend(member_keys);
    }
    db.get_call_site_param_index_mut()
        .set_files_source_signatures(source_signature_updates);

    // The contribution collection reads only immutable state (signature
    // index, the source-signature map just installed above, decl/reference
    // indexes from earlier passes) and writes to per-file local buffers, so
    // it runs concurrently across files. A fresh per-file infer cache is
    // used (the db is immutable during this pass, so a cold cache yields
    // identical inference). Results merge sequentially in file-id order.
    let returned_table_cache = ReturnedTableCache::default();
    let exact_receiver_candidates =
        collect_exact_receiver_candidates(db, exact_receiver_member_keys);
    let contribution_updates =
        super::parallel::map_files_collect(&*db, &file_ids, |db, file_id| {
            let mut contributions = Vec::new();
            let mut receiver_signatures = Vec::new();
            let mut receiver_consumers = Vec::new();
            let mut exact_receiver_eligibility = HashMap::new();
            let mut authoritative_receiver_signatures = HashMap::new();
            let Some(root) = db
                .get_vfs()
                .get_syntax_tree(&file_id)
                .map(|tree| tree.get_chunk_node())
            else {
                return (
                    file_id,
                    contributions,
                    receiver_signatures,
                    receiver_consumers,
                    Default::default(),
                );
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
                    &returned_table_cache,
                    &exact_receiver_candidates,
                    &mut exact_receiver_eligibility,
                    &mut authoritative_receiver_signatures,
                    &mut contributions,
                    &mut receiver_signatures,
                    &mut receiver_consumers,
                );
            }
            collect_vgui_named_callback_receiver_types(
                db,
                &mut cache,
                file_id,
                &root,
                &mut contributions,
            );
            (
                file_id,
                contributions,
                receiver_signatures,
                receiver_consumers,
                cache.take_inferred_guard_dependencies(),
            )
        });
    let mut fact_updates = Vec::with_capacity(contribution_updates.len());
    let mut receiver_signatures = Vec::new();
    let mut return_consumer_updates = Vec::new();
    for (file_id, contributions, file_receiver_signatures, file_consumers, dependencies) in
        contribution_updates
    {
        context.add_inferred_guard_dependencies(file_id, dependencies);
        fact_updates.push((file_id, contributions));
        receiver_signatures.extend(file_receiver_signatures);
        return_consumer_updates.push((file_id, file_consumers));
    }
    let source_file_ids = fact_updates
        .iter()
        .flat_map(|(_, contributions)| contributions)
        .flat_map(|(_, _, fact)| fact.provenance())
        .map(|step| step.event.source.file_id)
        .chain(
            return_consumer_updates
                .iter()
                .flat_map(|(_, consumers)| consumers)
                .map(|consumer| consumer.signature_id.get_file_id()),
        )
        .collect::<HashSet<_>>()
        .into_iter();
    let source_paths = source_file_ids
        .filter_map(|file_id| {
            db.get_vfs()
                .get_file_path(&file_id)
                .cloned()
                .map(|path| (file_id, path))
        })
        .collect::<Vec<_>>();
    db.get_call_site_param_index_mut()
        .record_source_paths(source_paths);
    let changed_signatures = db
        .get_call_site_param_index_mut()
        .set_files_fact_contributions(fact_updates);
    // Deliberately *not* filtered by `changed_signatures`.
    receiver_signatures.extend(changed_signatures);
    receiver_signatures.sort_unstable_by_key(|signature_id| {
        (signature_id.get_file_id().id, signature_id.get_position())
    });
    receiver_signatures.dedup();
    let receiver_signatures = receiver_signatures.into_iter().collect::<HashSet<_>>();
    let stale_return_consumers = return_consumer_updates
        .iter()
        .flat_map(|(_, consumers)| consumers)
        .filter(|consumer| consumer.needs_result_refresh)
        .cloned()
        .collect::<Vec<_>>();
    // Existing consumers are the indexed dependents of producer files in this
    // reindex batch. File ownership stays stable when edits shift a signature's
    // syntax position; a clean workspace build has no previous entries.
    let reindexed_return_consumers = db
        .get_call_site_param_index()
        .get_return_consumers_for_signature_files(&context.analyzed_file_ids());
    let receiver_consumers = {
        let index = db.get_call_site_param_index_mut();
        index.set_files_return_consumers(return_consumer_updates);
        index.get_return_consumers(&receiver_signatures)
    };
    let receiver_consumers = receiver_consumers
        .into_iter()
        .filter_map(|consumer| materialize_call_result_consumer(db, consumer))
        .collect();
    let (requeued_returns, requeued_consumers) =
        context.requeue_call_site_inferred_returns(db, &receiver_signatures, receiver_consumers);
    let mut refresh_return_consumers = stale_return_consumers;
    refresh_return_consumers.extend(
        reindexed_return_consumers
            .into_iter()
            .filter(|consumer| call_result_consumer_target_is_inferred(db, consumer)),
    );
    refresh_return_consumers.sort_unstable_by_key(|consumer| {
        (
            consumer.file_id,
            consumer.call_syntax_id.get_range().start(),
            consumer.ret_idx,
        )
    });
    refresh_return_consumers.dedup_by(|left, right| {
        left.file_id == right.file_id
            && left.call_syntax_id == right.call_syntax_id
            && left.ret_idx == right.ret_idx
            && left.target == right.target
    });
    let refresh_return_consumers = refresh_return_consumers
        .into_iter()
        .filter_map(|consumer| {
            materialize_call_result_consumer(db, consumer)
                .map(|(_, consumer, definition)| (consumer, definition))
        })
        .collect();
    let refreshed_consumers = context.queue_call_site_return_consumers(refresh_return_consumers);
    if std::env::var_os("GLUALS_PROFILE").is_some() {
        eprintln!(
            "[profile] call_site_params receiver_signatures={} requeued_returns={} requeued_consumers={} refreshed_consumers={}",
            receiver_signatures.len(),
            requeued_returns,
            requeued_consumers,
            refreshed_consumers,
        );
    }
}

fn call_result_consumer_needs_refresh(
    db: &DbIndex,
    consumer: &CallSiteReturnConsumer,
    return_type: &LuaType,
) -> bool {
    if consumer.ret_idx == 0 {
        return false;
    }
    let LuaType::Variadic(variadic) = return_type else {
        return false;
    };
    let Some(expected) = variadic.get_type(consumer.ret_idx) else {
        return false;
    };
    if expected.is_any() || expected.is_unknown() || expected.is_nil() || expected.is_never() {
        return false;
    }
    let owner = match consumer.target {
        CallSiteReturnConsumerTarget::Decl(decl_id) => LuaTypeOwner::Decl(decl_id),
        CallSiteReturnConsumerTarget::Member(member_id) => LuaTypeOwner::Member(member_id),
    };
    let Some(current) = db.get_type_index().get_type_cache(&owner) else {
        return false;
    };
    if current.is_doc() {
        return false;
    }
    let current = current.as_type();
    // This owner is the exact syntactic target of this call result. Any
    // non-documented disagreement is a stale result cache, including after an
    // incremental producer edit; independent writes have separate definitions.
    current != expected
}

fn call_result_consumer_target_is_inferred(
    db: &DbIndex,
    consumer: &CallSiteReturnConsumer,
) -> bool {
    let owner = match consumer.target {
        CallSiteReturnConsumerTarget::Decl(decl_id) => LuaTypeOwner::Decl(decl_id),
        CallSiteReturnConsumerTarget::Member(member_id) => LuaTypeOwner::Member(member_id),
    };
    db.get_type_index()
        .get_type_cache(&owner)
        .is_some_and(|cache| !cache.is_doc())
}

#[derive(Debug, Clone)]
struct VguiNamedCallbackCandidate {
    signature_id: LuaSignatureId,
    member_key: LuaMemberKey,
    receiver_id: LuaTypeDeclId,
}

fn collect_vgui_named_callback_receiver_types(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    root: &glua_parser::LuaChunk,
    contributions: &mut Vec<(LuaSignatureId, usize, LuaTypeFact)>,
) {
    let Some(metadata) = db
        .get_gmod_class_metadata_index()
        .get_file_metadata(&file_id)
    else {
        return;
    };
    if metadata.vgui_register_table_calls.is_empty() {
        return;
    }
    let red_root = root.syntax().clone();
    let mut candidates: HashMap<LuaDeclId, Option<VguiNamedCallbackCandidate>> = HashMap::new();
    let mut candidate_names = HashSet::new();

    for field in root.syntax().descendants().filter_map(LuaTableField::cast) {
        let Some(LuaExpr::NameExpr(name_expr)) = field.get_value_expr() else {
            continue;
        };
        let Some(index_key) = field.get_field_key() else {
            continue;
        };
        let Ok(member_key) = LuaMemberKey::from_index_key(db, cache, &index_key) else {
            continue;
        };
        let Some((receiver_type, _)) =
            vgui_named_callback_binding(db, cache, file_id, metadata, &name_expr, &member_key)
        else {
            continue;
        };
        let Some(receiver_id) = concrete_vgui_receiver_id(db, &receiver_type) else {
            continue;
        };

        let Some(LuaSemanticDeclId::LuaDecl(decl_id)) = infer_expr_semantic_decl(
            db,
            cache,
            LuaExpr::NameExpr(name_expr.clone()),
            Default::default(),
            Default::default(),
        ) else {
            continue;
        };
        let Some(signature_id) = named_callback_signature_id(db, &red_root, decl_id) else {
            continue;
        };
        let candidate = VguiNamedCallbackCandidate {
            signature_id,
            member_key,
            receiver_id,
        };
        if let Some(name) = name_expr.get_name_text() {
            candidate_names.insert(name);
        }
        candidates
            .entry(decl_id)
            .and_modify(|current| {
                if current.as_ref().is_some_and(|current| {
                    current.member_key != candidate.member_key
                        || current.receiver_id != candidate.receiver_id
                }) {
                    *current = None;
                }
            })
            .or_insert(Some(candidate));
    }

    let mut references_by_decl: HashMap<LuaDeclId, Vec<LuaNameExpr>> = HashMap::new();
    for name_expr in root.syntax().descendants().filter_map(LuaNameExpr::cast) {
        if !name_expr
            .get_name_text()
            .is_some_and(|name| candidate_names.contains(&name))
        {
            continue;
        }
        let Some(LuaSemanticDeclId::LuaDecl(decl_id)) = infer_expr_semantic_decl(
            db,
            cache,
            LuaExpr::NameExpr(name_expr.clone()),
            Default::default(),
            Default::default(),
        ) else {
            continue;
        };
        if candidates.contains_key(&decl_id) {
            references_by_decl
                .entry(decl_id)
                .or_default()
                .push(name_expr);
        }
    }

    let mut candidates = candidates.into_iter().collect::<Vec<_>>();
    candidates.sort_unstable_by_key(|(decl_id, _)| decl_id.position);
    for (decl_id, candidate) in candidates {
        let Some(candidate) = candidate else {
            continue;
        };
        let Some(signature) = db.get_signature_index().get(&candidate.signature_id) else {
            continue;
        };
        if signature.is_colon_define
            || signature.params.is_empty()
            || signature.param_docs.contains_key(&0)
            || db
                .get_call_site_param_index()
                .is_param_mutated(&candidate.signature_id, 0)
        {
            continue;
        }
        let Some(references) = references_by_decl.remove(&decl_id) else {
            continue;
        };

        let mut bindings = Vec::new();
        let mut eligible = true;
        for name_expr in references {
            let Some((receiver_type, source)) = vgui_named_callback_binding(
                db,
                cache,
                file_id,
                metadata,
                &name_expr,
                &candidate.member_key,
            ) else {
                eligible = false;
                break;
            };
            let Some(receiver_id) = concrete_vgui_receiver_id(db, &receiver_type) else {
                eligible = false;
                break;
            };
            if receiver_id != candidate.receiver_id {
                eligible = false;
                break;
            }
            bindings.push(source);
        }
        if !eligible || bindings.is_empty() {
            continue;
        }

        for source in bindings {
            let node = LuaInferenceNodeId::SignatureParam {
                signature_id: candidate.signature_id,
                param_idx: 0,
            };
            let event = LuaInferenceEventId {
                node,
                kind: LuaInferenceProvenanceKind::ConcreteValue,
                source,
            };
            contributions.push((
                candidate.signature_id,
                0,
                LuaTypeFact::new(
                    LuaType::Def(candidate.receiver_id.clone()),
                    LuaInferenceConfidence::Certain,
                    Arc::from([LuaInferenceStep {
                        event,
                        support: Arc::from([]),
                        inferred_type: None,
                        found_type: None,
                    }]),
                ),
            ));
        }
    }
}

fn named_callback_signature_id(
    db: &DbIndex,
    root: &glua_parser::LuaSyntaxNode,
    decl_id: LuaDeclId,
) -> Option<LuaSignatureId> {
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    if !matches!(decl.extra, LuaDeclExtra::Local { .. }) {
        return None;
    }
    let closure = if let Some(value_syntax_id) = decl.get_value_syntax_id() {
        let value_expr = value_syntax_id
            .to_node_from_root(root)
            .and_then(LuaExpr::cast)?;
        let LuaExpr::ClosureExpr(closure) = value_expr else {
            return None;
        };
        closure
    } else {
        root.descendants()
            .filter_map(LuaLocalFuncStat::cast)
            .find(|stat| {
                stat.get_local_name()
                    .is_some_and(|name| name.get_range() == decl.get_range())
            })?
            .get_closure()?
    };
    Some(LuaSignatureId::from_closure(decl_id.file_id, &closure))
}

fn vgui_named_callback_binding(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    metadata: &crate::db_index::GmodScriptedClassFileMetadata,
    name_expr: &LuaNameExpr,
    expected_key: &LuaMemberKey,
) -> Option<(LuaType, InFiled<glua_parser::LuaSyntaxId>)> {
    if let Some(field) = name_expr.get_parent::<LuaTableField>()
        && field
            .get_value_expr()
            .is_some_and(|value| value.syntax() == name_expr.syntax())
    {
        let key = LuaMemberKey::from_index_key(db, cache, &field.get_field_key()?).ok()?;
        if &key != expected_key {
            return None;
        }
        let table_expr = field.get_parent::<LuaTableExpr>()?;
        let table_range = table_expr.get_range();
        let registration = metadata.vgui_register_table_calls.iter().find(|call| {
            call.args
                .get(call.vgui_panel_table_arg_idx(0))
                .is_some_and(|arg| arg.syntax_id.get_range() == table_range)
        })?;
        if !is_vgui_register_table_call(db, file_id, registration) {
            return None;
        }
        let receiver_type = LuaType::Def(vgui_register_table_type_decl_id(file_id, registration));
        return Some((
            receiver_type,
            InFiled::new(file_id, table_expr.get_syntax_id()),
        ));
    }

    let assign = name_expr.ancestors::<LuaAssignStat>().next()?;
    let (vars, exprs) = assign.get_var_and_expr_list();
    let value_idx = exprs
        .iter()
        .position(|expr| expr.syntax() == name_expr.syntax())?;
    let LuaVarExpr::IndexExpr(index_expr) = vars.get(value_idx)? else {
        return None;
    };
    let key = LuaMemberKey::from_index_key(db, cache, &index_expr.get_index_key()?).ok()?;
    if &key != expected_key {
        return None;
    }
    let receiver_expr = index_expr.get_prefix_expr()?;
    let receiver_type = infer_expr(db, cache, receiver_expr.clone()).ok()?;
    Some((
        receiver_type,
        InFiled::new(file_id, receiver_expr.get_syntax_id()),
    ))
}

fn concrete_vgui_receiver_id(db: &DbIndex, typ: &LuaType) -> Option<LuaTypeDeclId> {
    match typ {
        LuaType::Ref(type_id) | LuaType::Def(type_id) => {
            crate::semantic::type_decl_is_vgui_panel(db, type_id, 0).then(|| type_id.clone())
        }
        LuaType::Instance(instance) => concrete_vgui_receiver_id(db, instance.get_base()),
        LuaType::Union(union) => {
            let mut receiver_id = None;
            for typ in union.types() {
                if matches!(typ, LuaType::Nil) {
                    continue;
                }
                let candidate_id = concrete_vgui_receiver_id(db, typ)?;
                if receiver_id
                    .as_ref()
                    .is_some_and(|receiver_id| receiver_id != &candidate_id)
                {
                    return None;
                }
                receiver_id = Some(candidate_id);
            }
            receiver_id
        }
        _ => None,
    }
}

fn source_signatures_in_file(
    db: &DbIndex,
    file_id: FileId,
    root: &glua_parser::LuaSyntaxNode,
) -> Vec<(String, LuaSignatureId, Vec<usize>)> {
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
            let is_colon_define = func_stat
                .get_func_name()
                .and_then(|func_name| match func_name {
                    LuaVarExpr::IndexExpr(index_expr) => index_expr.get_index_token(),
                    LuaVarExpr::NameExpr(_) => None,
                })
                .is_some_and(|token| token.is_colon());
            let mutated = get_mutated_params(db, file_id, &closure, is_colon_define);
            Some((
                path.to_string(),
                LuaSignatureId::from_closure(file_id, &closure),
                mutated,
            ))
        })
        .collect()
}

fn get_mutated_params(
    db: &DbIndex,
    file_id: FileId,
    closure: &LuaClosureExpr,
    has_implicit_receiver: bool,
) -> Vec<usize> {
    let Some(params_list) = closure.get_params_list() else {
        return Vec::new();
    };
    let mut mutated = Vec::new();
    let explicit_param_count = params_list.get_params().count();
    let params = params_list
        .get_params()
        .enumerate()
        .filter_map(|(idx, param)| {
            param.get_name_token().map(|token| {
                let name = token.get_name_text().to_string();
                let is_receiver_binding = !has_implicit_receiver && idx == 0 && name == "self";
                let receiver_decl_id =
                    is_receiver_binding.then(|| LuaDeclId::new(file_id, token.get_range().start()));
                (idx, name, is_receiver_binding, receiver_decl_id)
            })
        })
        .collect::<Vec<_>>();

    let params = if has_implicit_receiver {
        let mut params = params;
        params.push((
            explicit_param_count,
            "self".to_string(),
            true,
            implicit_receiver_decl_id(file_id, closure),
        ));
        params
    } else {
        params
    };

    if params.is_empty() {
        return Vec::new();
    }

    let Some(block) = closure.get_block() else {
        return Vec::new();
    };

    let file_refs = db.get_reference_index().get_local_reference(&file_id);
    for assign in block.descendants::<LuaAssignStat>() {
        let is_direct_closure =
            assign.ancestors::<LuaClosureExpr>().next().as_ref() == Some(closure);
        let (vars, _) = assign.get_var_and_expr_list();
        for var in vars {
            for (idx, param_name, is_receiver_binding, receiver_decl_id) in &params {
                let is_mutated = if *is_receiver_binding {
                    let LuaVarExpr::NameExpr(name_expr) = &var else {
                        continue;
                    };
                    if name_expr
                        .get_name_text()
                        .is_none_or(|name| name.as_str() != param_name)
                    {
                        continue;
                    }
                    let write_decl_id =
                        file_refs.and_then(|refs| refs.get_decl_id(&name_expr.get_range()));
                    match (*receiver_decl_id, write_decl_id) {
                        (Some(receiver_decl_id), Some(write_decl_id)) => {
                            receiver_decl_id == write_decl_id
                        }
                        // Declaration identity should normally be available. Preserve the old,
                        // direct-closure behavior as the narrow syntax fallback.
                        _ => is_direct_closure,
                    }
                } else if is_direct_closure {
                    var_writes_param(&var, param_name)
                } else {
                    false
                };
                if is_mutated {
                    if !mutated.contains(idx) {
                        mutated.push(*idx);
                    }
                }
            }
        }
    }

    mutated
}

fn implicit_receiver_decl_id(file_id: FileId, closure: &LuaClosureExpr) -> Option<LuaDeclId> {
    let LuaVarExpr::IndexExpr(func_name) = closure.get_parent::<LuaFuncStat>()?.get_func_name()?
    else {
        return None;
    };
    let index_token = func_name.get_index_token()?;
    if !index_token.is_colon() {
        return None;
    }
    Some(LuaDeclId::new(file_id, index_token.get_range().start()))
}

fn collect_call_site_param_types(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    call_expr: LuaCallExpr,
    returned_table_cache: &ReturnedTableCache,
    exact_receiver_candidates: &HashMap<LuaMemberKey, bool>,
    exact_receiver_eligibility: &mut HashMap<LuaSignatureId, bool>,
    authoritative_receiver_signatures: &mut HashMap<LuaSignatureId, bool>,
    contributions: &mut Vec<(LuaSignatureId, usize, LuaTypeFact)>,
    receiver_signatures: &mut Vec<LuaSignatureId>,
    receiver_consumers: &mut Vec<CallSiteReturnConsumer>,
) -> Option<()> {
    let args = call_expr.get_args_list()?;
    let prefix_expr = call_expr.get_prefix_expr()?;
    let receiver_signature = collect_exact_colon_receiver_type(
        db,
        cache,
        file_id,
        &call_expr,
        &prefix_expr,
        exact_receiver_candidates,
        exact_receiver_eligibility,
        contributions,
    );
    if let Some(signature_id) = receiver_signature {
        receiver_signatures.push(signature_id);
    }

    let call_args = args.get_args().collect::<Vec<_>>();
    let useful_args = call_args
        .iter()
        .cloned()
        .enumerate()
        .filter(|(_, arg)| is_supported_call_site_arg_shape(db, file_id, arg))
        .collect::<Vec<_>>();

    let colon_call_arg_shift = usize::from(call_expr.is_colon_call());
    let first_arg_is_self = useful_args
        .iter()
        .any(|(arg_idx, arg)| *arg_idx == 0 && is_self_name_expr(arg));
    let (mut signature_ids, lookup_kind) = signature_ids_from_call_prefix(
        db,
        cache,
        file_id,
        &prefix_expr,
        call_expr.get_position(),
        first_arg_is_self,
        returned_table_cache,
    );
    if signature_ids.is_empty()
        && call_args
            .iter()
            .any(|arg| matches!(arg, LuaExpr::ClosureExpr(_)))
    {
        if let Some(signature_id) = get_prefix_expr_signature_id(db, cache, &call_expr) {
            signature_ids.push(signature_id);
        } else if let Ok(prefix_type) = infer_expr(db, cache, prefix_expr.clone()) {
            collect_signature_ids(&prefix_type, &mut signature_ids);
        }
        signature_ids.sort_unstable_by_key(|signature_id| {
            (signature_id.get_file_id().id, signature_id.get_position())
        });
        signature_ids.dedup();
    }
    let should_collect_result_consumers =
        receiver_signature.is_some() || call_has_later_result_target(&call_expr);
    let consumer_signature = if should_collect_result_consumers {
        receiver_signature
            .or_else(|| signature_ids.first().copied())
            .or_else(|| get_prefix_expr_signature_id(db, cache, &call_expr))
    } else {
        None
    };
    if let Some(consumer_signature) = consumer_signature {
        receiver_consumers.extend(direct_call_result_consumers(
            db,
            cache,
            file_id,
            &call_expr,
            consumer_signature,
        ));
    }
    for signature_id in signature_ids.iter().copied() {
        if is_call_site_realm_compatible(db, file_id, call_expr.get_position(), signature_id) {
            collect_direct_callback_param_types(
                db,
                cache,
                signature_id,
                &call_expr,
                &call_args,
                contributions,
            );
        }
    }
    if useful_args.is_empty() {
        return Some(());
    }
    for signature_id in signature_ids {
        if !is_call_site_realm_compatible(db, file_id, call_expr.get_position(), signature_id) {
            continue;
        }
        let Some(signature) = db.get_signature_index().get(&signature_id) else {
            continue;
        };
        let has_authoritative_receiver = signature.is_colon_define
            && *authoritative_receiver_signatures
                .entry(signature_id)
                .or_insert_with(|| signature_has_authoritative_receiver(db, signature_id));
        for (arg_idx, arg) in &useful_args {
            let is_implicit_receiver = signature.is_colon_define
                && !call_expr.is_colon_call()
                && *arg_idx == 0
                && is_self_name_expr(arg);
            let is_exact_include_receiver = lookup_kind
                == SignatureLookupKind::IncludeReturnedMember
                && !call_expr.is_colon_call()
                && *arg_idx == 0
                && is_self_name_expr(arg)
                && (signature.is_colon_define
                    || signature
                        .params
                        .first()
                        .is_some_and(|param| param == "self"));
            match lookup_kind {
                SignatureLookupKind::Direct => {}
                SignatureLookupKind::InferredCallable if !is_self_name_expr(arg) => continue,
                SignatureLookupKind::IncludeReturnedMember if !is_exact_include_receiver => {
                    continue;
                }
                _ => {}
            }
            if is_implicit_receiver && has_authoritative_receiver {
                continue;
            }
            let param_idx = if is_implicit_receiver {
                signature.params.len()
            } else if signature.is_colon_define && !call_expr.is_colon_call() {
                let Some(param_idx) = arg_idx.checked_sub(1) else {
                    continue;
                };
                param_idx
            } else if signature.is_colon_define {
                *arg_idx
            } else {
                *arg_idx + colon_call_arg_shift
            };
            if (!is_implicit_receiver && param_idx >= signature.params.len())
                || signature.param_docs.contains_key(&param_idx)
            {
                continue;
            }
            if !is_implicit_receiver && !is_exact_include_receiver {
                let Some(param_name) = signature.params.get(param_idx) else {
                    continue;
                };
                if !has_gmod_param_name_hint(db, param_name) {
                    continue;
                }
            }
            if db
                .get_call_site_param_index()
                .is_param_mutated(&signature_id, param_idx)
            {
                continue;
            }
            let arg_syntax_id = arg.get_syntax_id();
            let Some(arg_type) =
                infer_supported_call_site_arg_type(db, cache, file_id, arg.clone())
            else {
                continue;
            };
            if arg_type.is_unknown() || arg_type.is_never() {
                continue;
            }

            let node = LuaInferenceNodeId::SignatureParam {
                signature_id,
                param_idx: u16::try_from(param_idx).ok()?,
            };
            let event = LuaInferenceEventId {
                node,
                kind: LuaInferenceProvenanceKind::ContextualUnknown,
                source: InFiled::new(file_id, arg_syntax_id),
            };
            contributions.push((
                signature_id,
                param_idx,
                LuaTypeFact::new(
                    arg_type.clone(),
                    LuaInferenceConfidence::Anchored,
                    vec![LuaInferenceStep {
                        event,
                        support: vec![].into(),
                        inferred_type: Some(Arc::new(arg_type)),
                        found_type: None,
                    }]
                    .into(),
                ),
            ));
        }
    }

    Some(())
}

fn collect_direct_callback_param_types(
    db: &DbIndex,
    caller_cache: &LuaInferCache,
    callee_signature_id: LuaSignatureId,
    call_expr: &LuaCallExpr,
    call_args: &[LuaExpr],
    contributions: &mut Vec<(LuaSignatureId, usize, LuaTypeFact)>,
) {
    let Some(callee_signature) = db.get_signature_index().get(&callee_signature_id) else {
        return;
    };
    let Some(callee_closure) = exact_signature_closure(db, callee_signature_id) else {
        return;
    };
    let Some(callee_params) = callee_closure.get_params_list() else {
        return;
    };
    let callee_params = callee_params.get_params().collect::<Vec<_>>();
    let source_file_id = callee_signature_id.get_file_id();
    let Some(source_root) = db
        .get_vfs()
        .get_syntax_tree(&source_file_id)
        .map(|tree| tree.get_red_root())
    else {
        return;
    };
    let Some(source_refs) = db
        .get_reference_index()
        .get_local_reference(&source_file_id)
    else {
        return;
    };
    let mut source_cache = LuaInferCache::new(source_file_id, caller_cache.get_config().clone());

    for (arg_idx, arg) in call_args.iter().enumerate() {
        let LuaExpr::ClosureExpr(target_closure) = arg else {
            continue;
        };
        let Some(callee_param_idx) =
            call_arg_to_param_index(callee_signature, call_expr.is_colon_call(), arg_idx)
        else {
            continue;
        };
        if callee_signature.param_docs.contains_key(&callee_param_idx)
            || db
                .get_call_site_param_index()
                .is_param_mutated(&callee_signature_id, callee_param_idx)
        {
            continue;
        }
        let Some(callee_param) = callee_params.get(callee_param_idx) else {
            continue;
        };
        let callee_param_decl_id = LuaDeclId::new(source_file_id, callee_param.get_position());
        let Some(references) = source_refs.get_decl_references(&callee_param_decl_id) else {
            continue;
        };
        let target_signature_id =
            LuaSignatureId::from_closure(caller_cache.get_file_id(), target_closure);
        let Some(target_signature) = db.get_signature_index().get(&target_signature_id) else {
            continue;
        };

        for cell in references.cells.iter().filter(|cell| !cell.is_write) {
            let Some(name_expr) = source_root
                .covering_element(cell.range)
                .ancestors()
                .find_map(LuaNameExpr::cast)
                .filter(|name| name.get_range() == cell.range)
            else {
                continue;
            };
            let Some(callback_call) = name_expr
                .syntax()
                .ancestors()
                .find_map(LuaCallExpr::cast)
                .filter(|call| {
                    call.get_prefix_expr()
                        .is_some_and(|prefix| prefix.syntax() == name_expr.syntax())
                })
            else {
                continue;
            };
            let Some(callback_args) = callback_call.get_args_list() else {
                continue;
            };
            for (target_param_idx, callback_arg) in callback_args.get_args().enumerate() {
                if target_param_idx >= target_signature.params.len()
                    || target_signature.param_docs.contains_key(&target_param_idx)
                    || db
                        .get_call_site_param_index()
                        .is_param_mutated(&target_signature_id, target_param_idx)
                {
                    continue;
                }
                let source_syntax_id = callback_arg.get_syntax_id();
                let Ok(arg_type) = infer_expr(db, &mut source_cache, callback_arg) else {
                    continue;
                };
                let Some(arg_type) = snapshot_callback_table_type(db, &arg_type) else {
                    continue;
                };
                let Ok(param_idx) = u16::try_from(target_param_idx) else {
                    continue;
                };
                let node = LuaInferenceNodeId::SignatureParam {
                    signature_id: target_signature_id,
                    param_idx,
                };
                let event = LuaInferenceEventId {
                    node,
                    kind: LuaInferenceProvenanceKind::ConcreteValue,
                    source: InFiled::new(source_file_id, source_syntax_id),
                };
                contributions.push((
                    target_signature_id,
                    target_param_idx,
                    LuaTypeFact::new(
                        arg_type,
                        LuaInferenceConfidence::Certain,
                        Arc::from([LuaInferenceStep {
                            event,
                            support: Arc::from([]),
                            inferred_type: None,
                            found_type: None,
                        }]),
                    ),
                ));
            }
        }
    }
}

fn snapshot_callback_table_type(db: &DbIndex, typ: &LuaType) -> Option<LuaType> {
    if !matches!(typ, LuaType::TableConst(_)) {
        return None;
    }
    let fields = get_member_map(db, typ)?
        .into_iter()
        .filter_map(|(key, members)| {
            let member_type = LuaType::from_inferred_vec(
                members
                    .into_iter()
                    .map(|member| member.typ)
                    .collect::<Vec<_>>(),
            );
            (!member_type.is_never()).then_some((key, member_type))
        })
        .collect();
    Some(LuaObjectType::new_with_fields(fields, Vec::new()).into())
}

fn call_arg_to_param_index(
    signature: &crate::LuaSignature,
    is_colon_call: bool,
    arg_idx: usize,
) -> Option<usize> {
    if signature.is_colon_define && !is_colon_call {
        arg_idx.checked_sub(1)
    } else if signature.is_colon_define {
        Some(arg_idx)
    } else {
        Some(arg_idx + usize::from(is_colon_call))
    }
}

fn call_has_later_result_target(call_expr: &LuaCallExpr) -> bool {
    let call_range = call_expr.get_range();
    if let Some(assign) = call_expr.ancestors::<LuaAssignStat>().next() {
        let (vars, exprs) = assign.get_var_and_expr_list();
        let Some(expr_idx) = exprs.iter().position(|expr| expr.get_range() == call_range) else {
            return false;
        };
        return expr_idx + 1 == exprs.len() && vars.len() > expr_idx + 1;
    }

    let Some(local) = call_expr.ancestors::<LuaLocalStat>().next() else {
        return false;
    };
    let Some(expr_idx) = local
        .get_value_exprs()
        .position(|expr| expr.get_range() == call_range)
    else {
        return false;
    };
    expr_idx + 1 == local.get_value_exprs().count()
        && local.get_local_name_list().count() > expr_idx + 1
}

fn direct_call_result_consumers(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    call_expr: &LuaCallExpr,
    signature_id: LuaSignatureId,
) -> Vec<CallSiteReturnConsumer> {
    let call_range = call_expr.get_range();
    let mut consumers: Vec<CallSiteReturnConsumer> = if let Some(assign) =
        call_expr.ancestors::<LuaAssignStat>().next()
    {
        let (vars, exprs) = assign.get_var_and_expr_list();
        let Some(expr_idx) = exprs.iter().position(|expr| expr.get_range() == call_range) else {
            return Vec::new();
        };
        let target_end = if expr_idx + 1 == exprs.len() {
            vars.len()
        } else {
            (expr_idx + 1).min(vars.len())
        };
        vars[expr_idx..target_end]
            .iter()
            .enumerate()
            .filter_map(|(ret_idx, var)| {
                let target_idx = expr_idx + ret_idx;
                Some(CallSiteReturnConsumer {
                    signature_id,
                    file_id,
                    call_syntax_id: call_expr.get_syntax_id(),
                    ret_idx,
                    target: call_assignment_target(db, file_id, var)?,
                    definition: Some(crate::LuaDefinitionId::Assignment {
                        file_id,
                        assignment: assign.get_syntax_id(),
                        target_idx: u16::try_from(target_idx).ok()?,
                    }),
                    needs_result_refresh: false,
                })
            })
            .collect()
    } else {
        let Some(local) = call_expr.ancestors::<LuaLocalStat>().next() else {
            return Vec::new();
        };
        let Some(expr_idx) = local
            .get_value_exprs()
            .position(|expr| expr.get_range() == call_range)
        else {
            return Vec::new();
        };
        let expr_count = local.get_value_exprs().count();
        let names = local.get_local_name_list().collect::<Vec<_>>();
        let target_end = if expr_idx + 1 == expr_count {
            names.len()
        } else {
            (expr_idx + 1).min(names.len())
        };
        names[expr_idx..target_end]
            .iter()
            .enumerate()
            .map(|(ret_idx, local_name)| CallSiteReturnConsumer {
                signature_id,
                file_id,
                call_syntax_id: call_expr.get_syntax_id(),
                ret_idx,
                target: CallSiteReturnConsumerTarget::Decl(LuaDeclId::new(
                    file_id,
                    local_name.get_position(),
                )),
                definition: None,
                needs_result_refresh: false,
            })
            .collect()
    };
    if consumers.len() > 1
        && let Ok(return_type) = infer_expr(db, cache, LuaExpr::CallExpr(call_expr.clone()))
    {
        for consumer in &mut consumers {
            consumer.needs_result_refresh =
                call_result_consumer_needs_refresh(db, consumer, &return_type);
        }
    }
    consumers
}

fn call_assignment_target(
    db: &DbIndex,
    file_id: FileId,
    var: &LuaVarExpr,
) -> Option<CallSiteReturnConsumerTarget> {
    match var {
        LuaVarExpr::NameExpr(name_expr) => {
            let decl_id = db
                .get_reference_index()
                .get_local_reference(&file_id)
                .and_then(|references| references.get_decl_id(&name_expr.get_range()))
                .unwrap_or_else(|| LuaDeclId::new(file_id, name_expr.get_position()));
            Some(CallSiteReturnConsumerTarget::Decl(decl_id))
        }
        LuaVarExpr::IndexExpr(index_expr) => Some(CallSiteReturnConsumerTarget::Member(
            LuaMemberId::new(index_expr.get_syntax_id(), file_id),
        )),
    }
}

fn materialize_call_result_consumer(
    db: &DbIndex,
    consumer: CallSiteReturnConsumer,
) -> Option<(
    LuaSignatureId,
    UnResolve,
    Option<(crate::LuaDefinitionId, crate::LuaTypeOwner)>,
)> {
    let root = db
        .get_vfs()
        .get_syntax_tree(&consumer.file_id)?
        .get_red_root();
    let call_expr = consumer
        .call_syntax_id
        .to_node_from_root(&root)
        .and_then(LuaCallExpr::cast)?;
    let expr = LuaExpr::CallExpr(call_expr);
    let (unresolve, owner) = match consumer.target {
        CallSiteReturnConsumerTarget::Decl(decl_id) => (
            UnResolveDecl {
                file_id: consumer.file_id,
                decl_id,
                expr,
                ret_idx: consumer.ret_idx,
            }
            .into(),
            crate::LuaTypeOwner::Decl(decl_id),
        ),
        CallSiteReturnConsumerTarget::Member(member_id) => (
            UnResolveMember {
                file_id: consumer.file_id,
                member_id,
                expr: Some(expr),
                prefix: None,
                ret_idx: consumer.ret_idx,
            }
            .into(),
            crate::LuaTypeOwner::Member(member_id),
        ),
    };
    Some((
        consumer.signature_id,
        unresolve,
        consumer.definition.map(|definition| (definition, owner)),
    ))
}

fn collect_exact_colon_receiver_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    call_expr: &LuaCallExpr,
    prefix_expr: &LuaExpr,
    exact_receiver_candidates: &HashMap<LuaMemberKey, bool>,
    exact_receiver_eligibility: &mut HashMap<LuaSignatureId, bool>,
    contributions: &mut Vec<(LuaSignatureId, usize, LuaTypeFact)>,
) -> Option<LuaSignatureId> {
    if !call_expr.is_colon_call() {
        return None;
    }
    let LuaExpr::IndexExpr(index_expr) = prefix_expr else {
        return None;
    };
    let member_key = exact_receiver_member_key(&index_expr)?;
    if exact_receiver_candidates.get(&member_key) == Some(&false) {
        return None;
    }
    let receiver_expr = index_expr.get_prefix_expr()?;
    let signature_id = get_prefix_expr_signature_id(db, cache, call_expr)?;
    if !is_call_site_realm_compatible(db, file_id, call_expr.get_position(), signature_id) {
        return None;
    }

    let signature = db.get_signature_index().get(&signature_id)?;
    if signature.is_colon_define
        || signature.params.first().is_none_or(|param| param != "self")
        || signature.param_docs.contains_key(&0)
        || db
            .get_call_site_param_index()
            .is_param_mutated(&signature_id, 0)
        || !*exact_receiver_eligibility
            .entry(signature_id)
            .or_insert_with(|| exact_explicit_self_param_is_eligible(db, signature_id))
    {
        return None;
    }

    let receiver_type = infer_expr(db, cache, receiver_expr.clone()).ok()?;
    if receiver_type.is_unknown()
        || receiver_type.is_any()
        || receiver_type.is_never()
        || matches!(receiver_type, LuaType::Union(_) | LuaType::Intersection(_))
    {
        return None;
    }

    let node = LuaInferenceNodeId::SignatureParam {
        signature_id,
        param_idx: 0,
    };
    let event = LuaInferenceEventId {
        node,
        kind: LuaInferenceProvenanceKind::ContextualUnknown,
        source: InFiled::new(file_id, receiver_expr.get_syntax_id()),
    };
    contributions.push((
        signature_id,
        0,
        LuaTypeFact::new(
            receiver_type.clone(),
            LuaInferenceConfidence::Anchored,
            vec![LuaInferenceStep {
                event,
                support: vec![].into(),
                inferred_type: Some(Arc::new(receiver_type)),
                found_type: None,
            }]
            .into(),
        ),
    ));
    Some(signature_id)
}

fn collect_exact_receiver_candidates(
    db: &DbIndex,
    keys: HashSet<LuaMemberKey>,
) -> HashMap<LuaMemberKey, bool> {
    keys.into_iter()
        .map(|member_key| {
            let has_candidate = exact_receiver_key_has_candidate(db, &member_key);
            (member_key, has_candidate)
        })
        .collect()
}

fn exact_receiver_member_keys_in_file(root: &glua_parser::LuaChunk) -> HashSet<LuaMemberKey> {
    root.descendants::<LuaCallExpr>()
        .filter(|call_expr| call_expr.is_colon_call())
        .filter_map(|call_expr| match call_expr.get_prefix_expr()? {
            LuaExpr::IndexExpr(index_expr) => exact_receiver_member_key(&index_expr),
            _ => None,
        })
        .collect()
}

fn exact_receiver_key_has_candidate(db: &DbIndex, member_key: &LuaMemberKey) -> bool {
    let mut members = db
        .get_member_index()
        .get_current_members_for_key(member_key)
        .into_iter();
    let Some(first_member) = members.next() else {
        return true;
    };
    member_value_may_have_exact_receiver_signature(db, first_member.get_id(), &mut HashSet::new())
        || members.any(|member| {
            member_value_may_have_exact_receiver_signature(db, member.get_id(), &mut HashSet::new())
        })
}

fn exact_receiver_member_key(index_expr: &glua_parser::LuaIndexExpr) -> Option<LuaMemberKey> {
    match index_expr.get_index_key()? {
        LuaIndexKey::Name(name) => Some(LuaMemberKey::Name(name.get_name_text().into())),
        LuaIndexKey::String(string) => Some(LuaMemberKey::Name(string.get_value().into())),
        _ => None,
    }
}

fn member_value_may_have_exact_receiver_signature(
    db: &DbIndex,
    member_id: LuaMemberId,
    visiting: &mut HashSet<LuaDeclId>,
) -> bool {
    let Some(value_expr) = get_member_value_expr(db, member_id) else {
        return true;
    };
    expr_may_have_exact_receiver_signature(db, member_id.file_id, value_expr, visiting)
}

fn expr_may_have_exact_receiver_signature(
    db: &DbIndex,
    file_id: FileId,
    expr: LuaExpr,
    visiting: &mut HashSet<LuaDeclId>,
) -> bool {
    match expr {
        LuaExpr::ClosureExpr(closure) => {
            let signature_id = LuaSignatureId::from_closure(file_id, &closure);
            db.get_signature_index()
                .get(&signature_id)
                .is_some_and(exact_receiver_signature_is_candidate)
        }
        LuaExpr::ParenExpr(paren) => paren
            .get_expr()
            .is_none_or(|expr| expr_may_have_exact_receiver_signature(db, file_id, expr, visiting)),
        LuaExpr::NameExpr(name_expr) => {
            let Some(decl_id) = db
                .get_reference_index()
                .get_local_reference(&file_id)
                .and_then(|references| references.get_decl_id(&name_expr.get_range()))
            else {
                return true;
            };
            if !visiting.insert(decl_id)
                || db
                    .get_reference_index()
                    .get_decl_references(&file_id, &decl_id)
                    .is_some_and(|references| references.mutable)
            {
                return true;
            }
            let result = db
                .get_decl_index()
                .get_decl(&decl_id)
                .filter(|decl| matches!(decl.extra, LuaDeclExtra::Local { .. }))
                .and_then(|decl| decl.get_value_syntax_id())
                .and_then(|syntax_id| {
                    db.get_vfs()
                        .get_syntax_tree(&file_id)
                        .and_then(|tree| syntax_id.to_node_from_root(&tree.get_red_root()))
                })
                .and_then(LuaExpr::cast)
                .is_none_or(|value_expr| {
                    expr_may_have_exact_receiver_signature(db, file_id, value_expr, visiting)
                });
            visiting.remove(&decl_id);
            result
        }
        LuaExpr::LiteralExpr(_) | LuaExpr::TableExpr(_) => false,
        _ => true,
    }
}

fn exact_receiver_signature_is_candidate(signature: &crate::LuaSignature) -> bool {
    !signature.is_colon_define
        && signature
            .params
            .first()
            .is_some_and(|param| param == "self")
        && !signature.param_docs.contains_key(&0)
        && matches!(
            signature.resolve_return,
            crate::SignatureReturnStatus::UnResolve | crate::SignatureReturnStatus::InferResolve
        )
}

fn exact_explicit_self_param_is_eligible(db: &DbIndex, signature_id: LuaSignatureId) -> bool {
    let Some(signature) = db.get_signature_index().get(&signature_id) else {
        return false;
    };
    if !matches!(
        signature.resolve_return,
        crate::SignatureReturnStatus::UnResolve | crate::SignatureReturnStatus::InferResolve
    ) {
        return false;
    }

    let Some(closure) = exact_signature_closure(db, signature_id) else {
        return false;
    };
    if get_mutated_params(db, signature_id.get_file_id(), &closure, false).contains(&0) {
        return false;
    }
    let Some(self_decl_id) = closure
        .get_params_list()
        .and_then(|params| params.get_params().next())
        .and_then(|param| param.get_name_token())
        .map(|token| LuaDeclId::new(signature_id.get_file_id(), token.get_range().start()))
    else {
        return false;
    };
    let Some(block) = closure.get_block() else {
        return false;
    };

    block.descendants::<LuaReturnStat>().any(|return_stat| {
        if return_stat.ancestors::<LuaClosureExpr>().next().as_ref() != Some(&closure) {
            return false;
        }
        return_stat.descendants::<LuaNameExpr>().any(|name_expr| {
            db.get_reference_index()
                .get_var_reference_decl(&signature_id.get_file_id(), name_expr.get_range())
                == Some(self_decl_id)
        })
    })
}

fn exact_signature_closure(db: &DbIndex, signature_id: LuaSignatureId) -> Option<LuaClosureExpr> {
    let root = db
        .get_vfs()
        .get_syntax_tree(&signature_id.get_file_id())
        .map(|tree| tree.get_red_root())?;
    root.token_at_offset(signature_id.get_position())
        .right_biased()
        .and_then(|token| token.parent_ancestors().find_map(LuaClosureExpr::cast))
        .filter(|closure| {
            LuaSignatureId::from_closure(signature_id.get_file_id(), closure) == signature_id
        })
}

fn signature_has_authoritative_receiver(db: &DbIndex, signature_id: LuaSignatureId) -> bool {
    let Some(func_stat) = exact_signature_closure(db, signature_id)
        .and_then(|closure| closure.get_parent::<LuaFuncStat>())
    else {
        return false;
    };
    let mut cache = LuaInferCache::new(
        signature_id.get_file_id(),
        crate::CacheOptions {
            analysis_phase: crate::LuaAnalysisPhase::Force,
        },
    );
    infer_authoritative_method_self_type(db, &mut cache, &func_stat).is_some()
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

fn signature_ids_from_call_prefix(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    prefix_expr: &LuaExpr,
    call_position: TextSize,
    first_arg_is_self: bool,
    returned_table_cache: &ReturnedTableCache,
) -> (Vec<LuaSignatureId>, SignatureLookupKind) {
    let direct = match prefix_expr {
        LuaExpr::NameExpr(name_expr) => {
            signature_id_from_name_expr(db, file_id, name_expr, call_position)
        }
        LuaExpr::IndexExpr(index_expr) => index_expr.get_access_path().and_then(|path| {
            db.get_call_site_param_index()
                .get_source_signature_for_file_at(path.as_str(), file_id, call_position)
        }),
        _ => None,
    };
    if let Some(signature_id) = direct {
        return (vec![signature_id], SignatureLookupKind::Direct);
    }
    if !first_arg_is_self {
        return (Vec::new(), SignatureLookupKind::InferredCallable);
    }

    let mut signature_ids = Vec::new();
    if let Ok(prefix_type) = infer_expr(db, cache, prefix_expr.clone()) {
        collect_signature_ids(&prefix_type, &mut signature_ids);
    }
    let mut lookup_kind = SignatureLookupKind::InferredCallable;
    if let LuaExpr::NameExpr(name_expr) = prefix_expr {
        let include_signature_ids = signature_ids_from_include_returned_table_member(
            db,
            cache,
            file_id,
            name_expr,
            call_position,
            returned_table_cache,
        );
        if !include_signature_ids.is_empty() {
            signature_ids = include_signature_ids;
            lookup_kind = SignatureLookupKind::IncludeReturnedMember;
        }
    }
    signature_ids.sort_unstable_by_key(|signature_id| {
        (signature_id.get_file_id().id, signature_id.get_position())
    });
    signature_ids.dedup();
    (signature_ids, lookup_kind)
}

fn signature_ids_from_include_returned_table_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    name_expr: &LuaNameExpr,
    call_position: TextSize,
    returned_table_cache: &ReturnedTableCache,
) -> Vec<LuaSignatureId> {
    let Some(file_refs) = db.get_reference_index().get_local_reference(&file_id) else {
        return Vec::new();
    };
    let Some(decl_id) = file_refs.get_decl_id(&name_expr.get_range()) else {
        return Vec::new();
    };
    let Some(decl) = db.get_decl_index().get_decl(&decl_id) else {
        return Vec::new();
    };
    if !matches!(decl.extra, LuaDeclExtra::Local { .. }) || decl.get_position() > call_position {
        return Vec::new();
    }
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_red_root())
    else {
        return Vec::new();
    };
    let Some(LuaExpr::IndexExpr(index_expr)) = decl
        .get_value_syntax_id()
        .and_then(|syntax_id| syntax_id.to_node_from_root(&root))
        .and_then(LuaExpr::cast)
    else {
        return Vec::new();
    };
    if file_refs
        .get_decl_references(&decl_id)
        .is_some_and(|references| {
            references.cells.iter().any(|cell| {
                cell.is_write
                    && cell.range.start() >= index_expr.get_range().end()
                    && cell.range.start() < call_position
                    && !write_only_replaces_absent_callback(&root, cell.range, name_expr)
            })
        })
    {
        return Vec::new();
    }
    if !matches!(index_expr.get_index_key(), Some(LuaIndexKey::Expr(_))) {
        return Vec::new();
    }
    let Some(source_expr) = index_expr.get_prefix_expr() else {
        return Vec::new();
    };

    let Some(member_id) = exact_source_member_id(db, cache, &source_expr) else {
        return Vec::new();
    };
    let mut visited = HashSet::new();
    let target_file_ids =
        include_targets_from_member(db, cache, file_id, call_position, member_id, &mut visited);
    let [target_file_id] = target_file_ids.as_slice() else {
        return Vec::new();
    };

    returned_table_info(db, returned_table_cache, *target_file_id)
        .into_iter()
        .flat_map(|returned_table| {
            returned_table
                .member_signatures
                .iter()
                .filter_map(|member| {
                    (stable_visible_member_id_from_history(
                        db,
                        &member.history,
                        file_id,
                        call_position,
                    ) == Some(member.member_id))
                    .then_some(member.signature_id)
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn exact_source_member_id(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    source_expr: &LuaExpr,
) -> Option<LuaMemberId> {
    let member_id = match source_expr {
        LuaExpr::IndexExpr(index_expr) => {
            let semantic_decl = infer_expr_semantic_decl(
                db,
                cache,
                LuaExpr::IndexExpr(index_expr.clone()),
                Default::default(),
                Default::default(),
            );
            match semantic_decl {
                Some(LuaSemanticDeclId::Member(member_id)) => member_id,
                Some(_) => return None,
                None => exact_index_member_candidate(db, cache, index_expr)?,
            }
        }
        LuaExpr::ParenExpr(paren_expr) => {
            exact_source_member_id(db, cache, &paren_expr.get_expr()?)?
        }
        LuaExpr::CallExpr(call_expr) => {
            let signature_id = get_prefix_expr_signature_id(db, cache, call_expr)?;
            let guarded_arg_idx = exact_returned_arg_idx(db, call_expr, signature_id)?;
            let guarded_arg = call_expr.get_args_list()?.get_args().nth(guarded_arg_idx)?;
            exact_source_member_id(db, cache, &guarded_arg)?
        }
        _ => return None,
    };

    Some(member_id)
}

fn exact_index_member_candidate(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    index_expr: &glua_parser::LuaIndexExpr,
) -> Option<LuaMemberId> {
    let prefix_type = infer_expr(db, cache, index_expr.get_prefix_expr()?).ok()?;
    let owner = match prefix_type {
        LuaType::TableConst(table_id) => LuaMemberOwner::Element(table_id),
        LuaType::Def(type_id) | LuaType::Ref(type_id) => LuaMemberOwner::Type(type_id),
        _ => return None,
    };
    let member_key =
        crate::LuaMemberKey::from_index_key(db, cache, &index_expr.get_index_key()?).ok()?;
    let member_ids = db
        .get_member_index()
        .get_member_item(&owner, &member_key)?
        .get_member_ids();
    member_ids
        .into_iter()
        .min_by_key(|member_id| (member_id.file_id.id, u32::from(member_id.get_position())))
}

fn exact_returned_arg_idx(
    db: &DbIndex,
    call_expr: &LuaCallExpr,
    signature_id: LuaSignatureId,
) -> Option<usize> {
    if let Some(attribute) = find_signature_attribute_use(db, signature_id, RETURN_ALIAS_ATTRIBUTE)
    {
        let param = attribute
            .get_param_by_name("param")
            .or_else(|| attribute.args.first().and_then(|(_, typ)| typ.as_ref()));
        if let Some(LuaType::IntegerConst(param) | LuaType::DocIntegerConst(param)) = param {
            return usize::try_from(*param).ok();
        }
    }

    // Template correlation (`f<T>(value: T): T`) does not prove value identity.
    // Keep the embedded standard-library `assert` fallback for compatibility;
    // external declarations must opt in explicitly with `return_alias`.
    (call_expr.is_assert()
        && db
            .get_module_index()
            .get_workspace_id(signature_id.get_file_id())
            == Some(WorkspaceId::STD))
    .then_some(0)
}

fn stable_visible_member_id_at_call(
    db: &DbIndex,
    member_id: LuaMemberId,
    caller_file_id: FileId,
    call_position: TextSize,
) -> Option<LuaMemberId> {
    let history_item = member_history_item(db, member_id)?;
    stable_visible_member_id_from_history(db, &history_item, caller_file_id, call_position)
}

fn member_history_item(db: &DbIndex, member_id: LuaMemberId) -> Option<LuaMemberIndexItem> {
    let member = db.get_member_index().get_member(&member_id)?;
    let owner = db.get_member_index().get_current_owner(&member_id)?;
    let historical_ids = db
        .get_member_index()
        .get_current_owner_members_for_key(owner, member.get_key())
        .into_iter()
        .map(|member| member.get_id())
        .collect::<Vec<_>>();
    let history_item = match historical_ids.as_slice() {
        [] => return None,
        [member_id] => LuaMemberIndexItem::One(*member_id),
        _ => LuaMemberIndexItem::Many(historical_ids),
    };
    Some(history_item)
}

fn stable_visible_member_id_from_history(
    db: &DbIndex,
    history_item: &LuaMemberIndexItem,
    caller_file_id: FileId,
    call_position: TextSize,
) -> Option<LuaMemberId> {
    let visible_ids = history_item.visible_member_ids_with_realm_at_offset_from_history(
        db,
        &caller_file_id,
        call_position,
    );
    match visible_ids.as_slice() {
        [visible_id] => Some(*visible_id),
        [] => {
            // Same-file members assigned in a realm branch are hidden by the
            // general control-flow visibility rule once execution leaves that
            // branch. For this exact-source lookup, the caller's realm is
            // sufficient to recover a single earlier assignment. Multiple
            // compatible writes remain ambiguous and are rejected.
            let caller_mask = db
                .get_gmod_infer_index()
                .get_state_mask_at_offset(&caller_file_id, call_position);
            let compatible_ids = history_item
                .get_member_ids()
                .into_iter()
                .filter(|candidate_id| {
                    (candidate_id.file_id != caller_file_id
                        || candidate_id.get_position() < call_position)
                        && member_has_narrower_enclosing_realm(db, *candidate_id)
                        && caller_mask.is_compatible_with(
                            db.get_gmod_infer_index().get_state_mask_at_offset(
                                &candidate_id.file_id,
                                candidate_id.get_position(),
                            ),
                        )
                })
                .collect::<Vec<_>>();
            let [compatible_id] = compatible_ids.as_slice() else {
                return None;
            };
            Some(*compatible_id)
        }
        _ => None,
    }
}

fn member_has_narrower_enclosing_realm(db: &DbIndex, member_id: LuaMemberId) -> bool {
    let infer_index = db.get_gmod_infer_index();
    let member_mask =
        infer_index.get_state_mask_at_offset(&member_id.file_id, member_id.get_position());
    if member_mask.is_empty() {
        return false;
    }
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&member_id.file_id)
        .map(|tree| tree.get_red_root())
    else {
        return false;
    };
    let Some(node) = member_id.get_syntax_id().to_node_from_root(&root) else {
        return false;
    };

    node.ancestors().filter_map(LuaIfStat::cast).any(|if_stat| {
        infer_index.get_state_mask_at_offset(&member_id.file_id, if_stat.get_position())
            != member_mask
    })
}

fn include_targets_from_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    caller_file_id: FileId,
    call_position: TextSize,
    member_id: LuaMemberId,
    visited: &mut HashSet<LuaMemberId>,
) -> Vec<FileId> {
    let Some(history) = member_history_item(db, member_id) else {
        return Vec::new();
    };
    let candidate_member_ids =
        history.visible_member_ids_with_realm_at_offset(db, &caller_file_id, call_position);
    let candidate_member_ids = possible_member_assignments_by_scope(db, candidate_member_ids);
    let mut targets = Vec::new();
    for candidate_member_id in candidate_member_ids {
        targets.extend(include_targets_from_exact_member(
            db,
            cache,
            caller_file_id,
            call_position,
            candidate_member_id,
            visited,
        ));
    }
    targets.sort_unstable();
    targets.dedup();
    targets
}

fn possible_member_assignments_by_scope(
    db: &DbIndex,
    mut member_ids: Vec<LuaMemberId>,
) -> Vec<LuaMemberId> {
    member_ids.sort_unstable_by_key(|member_id| {
        let scope = db
            .get_member_index()
            .member_function_scope_range(*member_id);
        (
            member_id.file_id.id,
            scope.map(|range| range.start()),
            scope.map(|range| range.end()),
            member_id.get_position(),
        )
    });

    let mut possible_ids = Vec::with_capacity(member_ids.len());
    let mut current_scope = None;
    let mut current_scope_start = 0;
    for member_id in member_ids {
        let scope = (
            member_id.file_id,
            db.get_member_index().member_function_scope_range(member_id),
        );
        if current_scope != Some(scope) {
            current_scope = Some(scope);
            current_scope_start = possible_ids.len();
        }

        if !db
            .get_member_index()
            .is_non_overwriting_assignment_member(member_id)
        {
            possible_ids.truncate(current_scope_start);
        }
        possible_ids.push(member_id);
    }
    possible_ids
}

fn include_targets_from_exact_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    caller_file_id: FileId,
    call_position: TextSize,
    member_id: LuaMemberId,
    visited: &mut HashSet<LuaMemberId>,
) -> Vec<FileId> {
    if !visited.insert(member_id) {
        return Vec::new();
    }

    let Some(value_expr) = get_member_value_expr(db, member_id) else {
        return Vec::new();
    };
    let mut targets = match value_expr {
        LuaExpr::CallExpr(call_expr) => {
            if let Some(target_file_id) =
                dependency_target_from_value_call(db, member_id.file_id, &call_expr)
            {
                vec![target_file_id]
            } else if let Some(signature_id) = get_prefix_expr_signature_id(db, cache, &call_expr)
                && let Some(returned_arg_idx) = exact_returned_arg_idx(db, &call_expr, signature_id)
                && let Some(returned_arg) = call_expr
                    .get_args_list()
                    .and_then(|args| args.get_args().nth(returned_arg_idx))
                && let Some(next_member_id) = exact_source_member_id(db, cache, &returned_arg)
            {
                include_targets_from_member(
                    db,
                    cache,
                    caller_file_id,
                    call_position,
                    next_member_id,
                    visited,
                )
            } else {
                Vec::new()
            }
        }
        LuaExpr::IndexExpr(index_expr) => {
            if matches!(index_expr.get_index_key(), Some(LuaIndexKey::Expr(_))) {
                include_targets_from_dynamic_table_members(
                    db,
                    cache,
                    caller_file_id,
                    call_position,
                    &index_expr,
                    visited,
                )
            } else {
                let Some(LuaSemanticDeclId::Member(next_member_id)) = infer_expr_semantic_decl(
                    db,
                    cache,
                    LuaExpr::IndexExpr(index_expr),
                    Default::default(),
                    Default::default(),
                ) else {
                    return Vec::new();
                };
                include_targets_from_member(
                    db,
                    cache,
                    caller_file_id,
                    call_position,
                    next_member_id,
                    visited,
                )
            }
        }
        _ => Vec::new(),
    };
    targets.sort_unstable();
    targets.dedup();
    targets
}

fn include_targets_from_dynamic_table_members(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    caller_file_id: FileId,
    call_position: TextSize,
    index_expr: &glua_parser::LuaIndexExpr,
    visited: &mut HashSet<LuaMemberId>,
) -> Vec<FileId> {
    let Some(source_table_expr) = index_expr.get_prefix_expr() else {
        return Vec::new();
    };
    let Some(source_member_id) = exact_source_member_id(db, cache, &source_table_expr) else {
        return Vec::new();
    };
    let Some(source_history) = member_history_item(db, source_member_id) else {
        return Vec::new();
    };
    if source_history.get_member_ids().len() != 1
        || !matches!(
            get_member_value_expr(db, source_member_id),
            Some(LuaExpr::TableExpr(_))
        )
    {
        return Vec::new();
    }

    let Ok(LuaType::TableConst(table_id)) = infer_expr(db, cache, source_table_expr) else {
        return Vec::new();
    };
    let Some(members) = db
        .get_member_index()
        .get_members(&LuaMemberOwner::Element(table_id))
    else {
        return Vec::new();
    };
    let mut member_ids = members
        .into_iter()
        .map(|member| member.get_id())
        .collect::<Vec<_>>();
    member_ids.sort_unstable_by_key(|member_id| (member_id.file_id.id, member_id.get_position()));
    member_ids.dedup();

    let mut targets = Vec::new();
    for member_id in member_ids {
        if stable_visible_member_id_at_call(db, member_id, caller_file_id, call_position)
            != Some(member_id)
        {
            continue;
        }
        targets.extend(include_targets_from_member(
            db,
            cache,
            caller_file_id,
            call_position,
            member_id,
            visited,
        ));
    }
    targets.sort_unstable();
    targets.dedup();
    targets
}

fn dependency_target_from_value_call(
    db: &DbIndex,
    source_file_id: FileId,
    value_call: &LuaCallExpr,
) -> Option<FileId> {
    let compilefile_call_range = direct_called_prefix(value_call).map(|call| call.get_range());
    let mut target_file_ids = db
        .get_file_dependencies_index()
        .get_dependency_sites(&source_file_id)?
        .iter()
        .filter_map(|site| {
            let is_exact_call = match site.kind {
                LuaDependencyKind::Include => site.call_range == value_call.get_range(),
                LuaDependencyKind::CompileFile => compilefile_call_range == Some(site.call_range),
                _ => false,
            };
            is_exact_call.then_some(site.target_file_id).flatten()
        })
        .collect::<Vec<_>>();
    target_file_ids.sort_unstable();
    target_file_ids.dedup();
    let [target_file_id] = target_file_ids.as_slice() else {
        return None;
    };
    Some(*target_file_id)
}

fn write_only_replaces_absent_callback(
    root: &glua_parser::LuaSyntaxNode,
    write_range: TextRange,
    callback_name: &LuaNameExpr,
) -> bool {
    let Some(callback_name) = callback_name.get_name_text() else {
        return false;
    };
    let Some(write_name) = root
        .covering_element(write_range)
        .ancestors()
        .find_map(LuaNameExpr::cast)
        .filter(|name_expr| name_expr.get_range() == write_range)
    else {
        return false;
    };

    // `if not callback then callback = fallback end` cannot replace the mixin callback on
    // a path where that callback is subsequently invoked.
    write_name.ancestors::<LuaIfStat>().any(|if_stat| {
        let Some(block_range) = if_stat.get_block().map(|block| block.get_range()) else {
            return false;
        };
        if block_range.start() > write_range.start() || block_range.end() < write_range.end() {
            return false;
        }
        let Some(LuaExpr::UnaryExpr(condition)) = if_stat.get_condition_expr() else {
            return false;
        };
        condition
            .get_op_token()
            .is_some_and(|op| op.get_op() == glua_parser::UnaryOperator::OpNot)
            && matches!(
                condition.get_expr(),
                Some(LuaExpr::NameExpr(name_expr))
                    if name_expr.get_name_text().as_deref() == Some(callback_name.as_str())
            )
    })
}

fn direct_called_prefix(call_expr: &LuaCallExpr) -> Option<LuaCallExpr> {
    if !is_zero_arg_call(call_expr) {
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

fn returned_table_info(
    db: &DbIndex,
    cache: &ReturnedTableCache,
    target_file_id: FileId,
) -> Option<Arc<ReturnedTableInfo>> {
    let entry = {
        let mut entries = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries
            .entry(target_file_id)
            .or_insert_with(|| Arc::new(OnceLock::new()))
            .clone()
    };

    entry
        .get_or_init(|| {
            returned_local_table_owner(db, target_file_id).and_then(|owner| {
                let members = db.get_member_index().get_members(&owner)?;
                let member_signatures = members
                    .into_iter()
                    .filter_map(|member| {
                        let member_id = member.get_id();
                        let LuaExpr::ClosureExpr(closure) = get_member_value_expr(db, member_id)?
                        else {
                            return None;
                        };
                        let signature_id = LuaSignatureId::from_closure(target_file_id, &closure);
                        let signature = db.get_signature_index().get(&signature_id)?;
                        let receiver_param_idx = if signature.is_colon_define {
                            signature.params.len()
                        } else if signature
                            .params
                            .first()
                            .is_some_and(|param| param == "self")
                        {
                            0
                        } else {
                            return None;
                        };
                        if get_mutated_params(
                            db,
                            target_file_id,
                            &closure,
                            signature.is_colon_define,
                        )
                        .contains(&receiver_param_idx)
                        {
                            return None;
                        }
                        Some(ReturnedMemberSignature {
                            member_id,
                            signature_id,
                            history: member_history_item(db, member_id)?,
                        })
                    })
                    .collect();
                Some(Arc::new(ReturnedTableInfo { member_signatures }))
            })
        })
        .clone()
}

fn returned_local_table_owner(db: &DbIndex, file_id: FileId) -> Option<LuaMemberOwner> {
    let root = db.get_vfs().get_syntax_tree(&file_id)?.get_red_root();
    let mut return_stats = root
        .descendants()
        .filter_map(LuaReturnStat::cast)
        .filter(|return_stat| return_stat.ancestors::<LuaClosureExpr>().next().is_none());
    let return_stat = return_stats.next()?;
    if return_stats.next().is_some() {
        return None;
    }
    let mut return_exprs = return_stat.get_expr_list();
    let LuaExpr::NameExpr(return_name) = return_exprs.next()? else {
        return None;
    };
    if return_exprs.next().is_some() {
        return None;
    }

    let file_refs = db.get_reference_index().get_local_reference(&file_id)?;
    let decl_id = file_refs.get_decl_id(&return_name.get_range())?;
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    if !matches!(decl.extra, LuaDeclExtra::Local { .. })
        || file_refs
            .get_decl_references(&decl_id)
            .is_some_and(|references| references.mutable)
    {
        return None;
    }
    let LuaExpr::TableExpr(table_expr) = decl
        .get_value_syntax_id()
        .and_then(|syntax_id| syntax_id.to_node_from_root(&root))
        .and_then(LuaExpr::cast)?
    else {
        return None;
    };
    Some(LuaMemberOwner::Element(InFiled::new(
        file_id,
        table_expr.get_range(),
    )))
}

fn is_self_name_expr(expr: &LuaExpr) -> bool {
    matches!(expr, LuaExpr::NameExpr(name) if name.get_name_text().as_deref() == Some("self"))
}

fn collect_signature_ids(typ: &LuaType, signature_ids: &mut Vec<LuaSignatureId>) {
    match typ {
        LuaType::Signature(signature_id) => signature_ids.push(*signature_id),
        LuaType::Instance(instance) => collect_signature_ids(instance.get_base(), signature_ids),
        LuaType::Union(union) => {
            for typ in union.types() {
                collect_signature_ids(typ, signature_ids);
            }
        }
        LuaType::MultiLineUnion(union) => {
            for (typ, _) in union.get_unions() {
                collect_signature_ids(typ, signature_ids);
            }
        }
        _ => {}
    }
}

fn signature_id_from_name_expr(
    db: &DbIndex,
    file_id: FileId,
    name_expr: &LuaNameExpr,
    call_position: TextSize,
) -> Option<LuaSignatureId> {
    let name = name_expr.get_name_text()?;
    let signature_id = db
        .get_call_site_param_index()
        .get_source_signature_for_file_at(name.as_str(), file_id, call_position)?;

    if name_expr_resolves_to_different_local_decl(db, file_id, name_expr, signature_id) {
        return None;
    }

    Some(signature_id)
}

fn name_expr_resolves_to_different_local_decl(
    db: &DbIndex,
    file_id: FileId,
    name_expr: &LuaNameExpr,
    signature_id: LuaSignatureId,
) -> bool {
    let Some(call_decl_id) = db
        .get_reference_index()
        .get_local_reference(&file_id)
        .and_then(|refs| refs.get_decl_id(&name_expr.get_range()))
    else {
        return false;
    };

    let Some(call_decl) = db.get_decl_index().get_decl(&call_decl_id) else {
        return false;
    };
    // Source signatures in the raw path map come from function statements, so this
    // currently rejects any local shadow. The signature comparison keeps the rule
    // correct if local-function signatures are ever added to the same map.
    matches!(call_decl.extra, LuaDeclExtra::Local { .. })
        && db.get_signature_index().local_func_decl_for(&signature_id) != Some(call_decl_id)
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
            if name_expr.get_name_text().as_deref() == Some("self") {
                let typ = infer_expr(db, cache, arg).ok()?;
                return (!typ.is_unknown() && !typ.is_any() && !typ.is_never()).then_some(typ);
            }
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
            if name_expr.get_name_text().as_deref() == Some("self") {
                return true;
            }
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
