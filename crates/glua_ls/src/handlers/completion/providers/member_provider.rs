use glua_code_analysis::{
    BUILTIN_HOOK_OWNERS, DbIndex, FileId, GmodRealm, GmodStateMask, LuaMemberInfo, LuaMemberKey,
    LuaMemberOwner, LuaSemanticDeclId, LuaType, LuaTypeDeclId, SemanticModel,
    enum_variable_is_param, get_tpl_ref_extend_type, gmod_hook_owner_group,
    gmod_hook_owner_group_of,
};
use glua_parser::{
    LuaAstNode, LuaAstToken, LuaComment, LuaCommentOwner, LuaDocTag, LuaDocTagRealm, LuaExpr,
    LuaFuncStat, LuaIndexExpr, LuaLocalFuncStat, LuaNameExpr, LuaStringToken, PathTrait,
};
use rowan::TextSize;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use smol_str::SmolStr;

use crate::handlers::completion::{
    add_completions::{
        CompletionTriggerStatus, add_member_completion_with_description_hint,
        is_callable_member_type,
    },
    completion_builder::CompletionBuilder,
};

pub fn add_completion(builder: &mut CompletionBuilder) -> Option<()> {
    if builder.is_cancelled() {
        return None;
    }

    let index_expr = LuaIndexExpr::cast(builder.trigger_token.parent()?)?;
    let index_token = index_expr.get_index_token()?;
    let completion_status = if index_token.is_dot() {
        CompletionTriggerStatus::Dot
    } else if index_token.is_colon() {
        CompletionTriggerStatus::Colon
    } else if LuaStringToken::can_cast(builder.trigger_token.kind().into()) {
        CompletionTriggerStatus::InString
    } else {
        CompletionTriggerStatus::LeftBracket
    };

    let prefix_expr = index_expr.get_prefix_expr()?;
    let prefix_type = match builder
        .semantic_model
        .infer_expr(prefix_expr.clone())
        .ok()?
    {
        LuaType::TplRef(tpl) => get_tpl_ref_extend_type(
            builder.semantic_model.get_db(),
            &mut builder.semantic_model.get_cache().borrow_mut(),
            &LuaType::TplRef(tpl.clone()),
            prefix_expr.clone(),
            0,
        )?,
        prefix_type => prefix_type,
    };
    // No completion for an enum type used as a function parameter
    if enum_variable_is_param(
        builder.semantic_model.get_db(),
        &mut builder.semantic_model.get_cache().borrow_mut(),
        &index_expr,
        &prefix_type,
    )
    .is_some()
    {
        return None;
    }

    let mut member_info_map = builder
        .semantic_model
        .get_member_info_map_at_offset(&prefix_type, builder.position_offset)
        .unwrap_or_default();
    extend_global_path_members(builder, &prefix_expr, &mut member_info_map);
    let gmod_owner_name = gmod_hook_owner_name(&prefix_expr, &prefix_type);
    let gmod_fallback_owner = if builder.semantic_model.get_emmyrc().gmod.enabled {
        gmod_owner_name.as_deref().and_then(|owner_name| {
            gmod_hook_fallback_owner(builder.semantic_model.get_db(), owner_name, &prefix_type)
        })
    } else {
        None
    };
    extend_gmod_hook_fallback_members(builder, gmod_fallback_owner, &mut member_info_map);
    dedupe_member_infos(builder, &mut member_info_map);

    add_completions_for_members_with_gmod_owner(
        builder,
        &member_info_map,
        completion_status,
        gmod_fallback_owner,
    )
}

fn extend_global_path_members(
    builder: &CompletionBuilder,
    prefix_expr: &LuaExpr,
    members: &mut HashMap<LuaMemberKey, Vec<LuaMemberInfo>>,
) {
    let Some(prefix_path) = global_expr_access_path(&builder.semantic_model, prefix_expr) else {
        return;
    };

    let namespace_type = LuaType::Namespace(SmolStr::new(prefix_path).into());
    let Some(global_path_members) = builder
        .semantic_model
        .get_member_info_map_at_offset(&namespace_type, builder.position_offset)
    else {
        return;
    };

    // The namespace route is a fallback for keys the prefix type cannot
    // answer — e.g. a guarded bootstrap slot that resolves to an empty
    // literal while the accumulated members live under the global path
    // owner. Its members join the prefix type's own, and superseded
    // same-file writers are collapsed afterwards.
    let mut existing = collect_member_identities(members);

    for (key, infos) in global_path_members {
        for info in infos {
            push_unique_member_info(members, &mut existing, key.clone(), info);
        }
    }
}

fn global_expr_access_path(
    semantic_model: &SemanticModel,
    expr: &LuaExpr,
) -> Option<smol_str::SmolStr> {
    if !expr_root_is_global(semantic_model, expr) {
        return None;
    }

    match expr {
        LuaExpr::NameExpr(name_expr) => name_expr.get_access_path(),
        LuaExpr::IndexExpr(index_expr) => index_expr.get_access_path(),
        _ => None,
    }
}

fn expr_root_is_global(semantic_model: &SemanticModel, expr: &LuaExpr) -> bool {
    let Some(root_name) = expr_root_name(expr) else {
        return false;
    };

    let db = semantic_model.get_db();
    let Some(decl_id) = db
        .get_reference_index()
        .get_var_reference_decl(&semantic_model.get_file_id(), root_name.get_range())
    else {
        return true;
    };

    db.get_decl_index()
        .get_decl(&decl_id)
        .is_some_and(|decl| decl.is_global() || decl.is_module_scoped())
}

fn expr_root_name(expr: &LuaExpr) -> Option<LuaNameExpr> {
    match expr {
        LuaExpr::NameExpr(name_expr) => Some(name_expr.clone()),
        LuaExpr::IndexExpr(index_expr) => {
            let prefix_expr = index_expr.get_prefix_expr()?;
            expr_root_name(&prefix_expr)
        }
        _ => None,
    }
}

fn extend_gmod_hook_fallback_members(
    builder: &CompletionBuilder,
    fallback_owner: Option<HookOwnerGroup>,
    members: &mut HashMap<LuaMemberKey, Vec<LuaMemberInfo>>,
) {
    let Some(fallback_owner) = fallback_owner else {
        return;
    };

    let mut existing = collect_member_identities(members);

    for owner_candidate in fallback_owner.candidates {
        let owner_type = LuaType::Ref(builder.semantic_model.resolve_global_class(owner_candidate));
        let Some(fallback_map) = builder
            .semantic_model
            .get_member_info_map_at_offset(&owner_type, builder.position_offset)
        else {
            continue;
        };

        for (key, fallback_infos) in fallback_map {
            for info in fallback_infos {
                push_unique_member_info(members, &mut existing, key.clone(), info);
            }
        }
    }
}

/// Collapses each key's candidates to one entry per distinct definition the
/// caller can be looking at: branch-only writes drop out where an unconditional
/// one exists, and what remains is deduplicated by identity.
fn dedupe_member_infos(
    builder: &CompletionBuilder,
    members: &mut HashMap<LuaMemberKey, Vec<LuaMemberInfo>>,
) {
    let db = builder.semantic_model.get_db();
    for infos in members.values_mut() {
        resolve_slot_writes(db, infos);
        let mut seen = HashSet::default();
        infos.retain(|info| seen.insert(MemberInfoIdentity::from(info)));
    }
}

/// What a candidate for one key is, as far as the rule below is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotWrite {
    /// Not a write: a class declaration field (`---@field`) is a contract, and
    /// a dynamic field carries no member at all. Never collapsed.
    NotAWrite,
    /// Runs whenever the statement around it is reached. A write in a function
    /// body counts: reaching the body is the caller's business, not a condition
    /// on the write.
    Unconditional,
    /// Runs only when its branch is taken, so the slot may never hold it.
    Branch,
}

/// Drops the writes of a key that only happen in a branch, where one outside
/// any branch of the same kind, callable or not, exists.
///
/// This is the only separation available here. Load order across files is
/// decided by member resolution, which also picks the write the caller's realm
/// selects, both before these infos are built; what is left is whether a write
/// is conditional at all. A plain value says nothing about a method a branch
/// defines, which can still be what a `:` call reaches, so only a write of the
/// same kind stands in for a branch write. Writes that are all unconditional —
/// guarded bootstrap siblings (`x = x or {}`) among them — say nothing about
/// each other and all survive.
fn resolve_slot_writes(db: &DbIndex, infos: &mut Vec<LuaMemberInfo>) {
    if infos.len() <= 1 {
        return;
    }

    let writes = infos
        .iter()
        .map(|info| {
            (
                classify_slot_write(db, info),
                is_callable_member_type(db, &info.typ),
            )
        })
        .collect::<Vec<_>>();
    let superseded = writes
        .iter()
        .map(|(write, callable)| {
            *write == SlotWrite::Branch
                && writes.iter().any(|(other, other_callable)| {
                    *other == SlotWrite::Unconditional && other_callable == callable
                })
        })
        .collect::<Vec<_>>();

    let mut superseded = superseded.into_iter();
    infos.retain(|_| superseded.next() != Some(true));
}

fn classify_slot_write(db: &DbIndex, info: &LuaMemberInfo) -> SlotWrite {
    let Some(LuaSemanticDeclId::Member(member_id)) = &info.property_owner_id else {
        return SlotWrite::NotAWrite;
    };
    let member_index = db.get_member_index();
    if matches!(
        member_index.get_member_owner(member_id),
        Some(LuaMemberOwner::Type(_))
    ) {
        return SlotWrite::NotAWrite;
    }
    if glua_code_analysis::is_member_assignment_in_conditional_branch(db, *member_id) {
        SlotWrite::Branch
    } else {
        SlotWrite::Unconditional
    }
}

type MemberIdentityMap = HashMap<LuaMemberKey, HashSet<MemberInfoIdentity>>;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct MemberInfoIdentity {
    property_owner_id: Option<LuaSemanticDeclId>,
    overload_index: Option<usize>,
}

impl From<&LuaMemberInfo> for MemberInfoIdentity {
    fn from(info: &LuaMemberInfo) -> Self {
        Self {
            property_owner_id: info.property_owner_id.clone(),
            overload_index: info.overload_index,
        }
    }
}

fn collect_member_identities(
    members: &HashMap<LuaMemberKey, Vec<LuaMemberInfo>>,
) -> MemberIdentityMap {
    let mut existing: MemberIdentityMap = HashMap::default();
    for (key, infos) in members {
        let entry = existing.entry(key.clone()).or_default();
        entry.extend(infos.iter().map(MemberInfoIdentity::from));
    }
    existing
}

fn push_unique_member_info(
    members: &mut HashMap<LuaMemberKey, Vec<LuaMemberInfo>>,
    existing: &mut MemberIdentityMap,
    key: LuaMemberKey,
    info: LuaMemberInfo,
) {
    let identities = existing.entry(key.clone()).or_default();
    if identities.insert(MemberInfoIdentity::from(&info)) {
        members.entry(key).or_default().push(info);
    }
}

fn gmod_hook_owner_name(prefix_expr: &LuaExpr, prefix_type: &LuaType) -> Option<String> {
    match prefix_type {
        LuaType::Ref(owner_type_decl_id) => Some(owner_type_decl_id.get_simple_name().to_string()),
        _ => match prefix_expr {
            LuaExpr::NameExpr(name_expr) => name_expr.get_name_text().map(Into::into),
            _ => None,
        },
    }
}

/// A hook owner's completion: the hook owner it is written through, and the
/// owners whose hooks it also offers.
#[derive(Clone, Copy)]
struct HookOwnerGroup {
    owner: &'static str,
    candidates: &'static [&'static str],
}

fn gmod_hook_fallback_owner(
    db: &DbIndex,
    owner_name: &str,
    owner_type: &LuaType,
) -> Option<HookOwnerGroup> {
    let candidates = match owner_type {
        LuaType::Ref(owner) | LuaType::Def(owner) => gmod_hook_owner_group_of(db, owner),
        _ => gmod_hook_owner_group(owner_name),
    };
    let owner = gmod_hook_owner_group(owner_name)
        .first()
        .or(candidates.first())?;
    Some(HookOwnerGroup { owner, candidates })
}

pub fn add_completions_for_members(
    builder: &mut CompletionBuilder,
    members: &HashMap<LuaMemberKey, Vec<LuaMemberInfo>>,
    completion_status: CompletionTriggerStatus,
) -> Option<()> {
    add_completions_for_members_with_gmod_owner(builder, members, completion_status, None)
}

fn add_completions_for_members_with_gmod_owner(
    builder: &mut CompletionBuilder,
    members: &HashMap<LuaMemberKey, Vec<LuaMemberInfo>>,
    completion_status: CompletionTriggerStatus,
    gmod_fallback_owner: Option<HookOwnerGroup>,
) -> Option<()> {
    // Sort
    let mut sorted_entries: Vec<_> = members.iter().collect();
    sorted_entries.sort_unstable_by_key(|(name, _)| *name);

    let mut realm_filter = RealmFilter::new(builder);
    for (_, member_infos) in sorted_entries {
        add_resolve_member_infos(
            builder,
            member_infos,
            completion_status,
            gmod_fallback_owner,
            &mut realm_filter,
        );
    }

    Some(())
}

fn add_resolve_member_infos(
    builder: &mut CompletionBuilder,
    member_infos: &Vec<LuaMemberInfo>,
    completion_status: CompletionTriggerStatus,
    gmod_fallback_owner: Option<HookOwnerGroup>,
    realm_filter: &mut RealmFilter,
) -> Option<()> {
    if member_infos.len() == 1 {
        let member_info = &member_infos[0];
        if !realm_filter.accepts(builder, member_info) {
            return Some(());
        }
        let overload_count = match &member_info.typ {
            LuaType::DocFunction(_) => None,
            LuaType::Signature(id) => {
                if let Some(signature) = builder
                    .semantic_model
                    .get_db()
                    .get_signature_index()
                    .get(id)
                {
                    let count = signature.overloads.len();
                    if count == 0 { None } else { Some(count) }
                } else {
                    None
                }
            }
            _ => None,
        };
        let description_hint =
            gmod_fallback_description_hint(builder, gmod_fallback_owner, member_info);
        add_member_completion_with_description_hint(
            builder,
            member_info.clone(),
            completion_status,
            overload_count,
            description_hint.as_deref(),
        );
        return Some(());
    }

    let (filtered_member_infos, overload_count) =
        filter_member_infos(&builder.semantic_model, member_infos)?;

    let resolve_state = get_resolve_state(builder.semantic_model.get_db(), &filtered_member_infos);
    let first_item = builder.get_completion_items_mut().len();

    for member_info in filtered_member_infos {
        if !realm_filter.accepts(builder, member_info) {
            continue;
        }

        match resolve_state {
            MemberResolveState::All => {
                let description_hint =
                    gmod_fallback_description_hint(builder, gmod_fallback_owner, member_info);
                add_member_completion_with_description_hint(
                    builder,
                    member_info.clone(),
                    completion_status,
                    overload_count,
                    description_hint.as_deref(),
                );
            }
            MemberResolveState::Meta => {
                if let Some(feature) = member_info.feature
                    && feature.is_meta_decl()
                {
                    let description_hint =
                        gmod_fallback_description_hint(builder, gmod_fallback_owner, member_info);
                    add_member_completion_with_description_hint(
                        builder,
                        member_info.clone(),
                        completion_status,
                        overload_count,
                        description_hint.as_deref(),
                    );
                }
            }
            MemberResolveState::FileDecl => {
                if let Some(feature) = member_info.feature
                    && feature.is_file_decl()
                {
                    let description_hint =
                        gmod_fallback_description_hint(builder, gmod_fallback_owner, member_info);
                    add_member_completion_with_description_hint(
                        builder,
                        member_info.clone(),
                        completion_status,
                        overload_count,
                        description_hint.as_deref(),
                    );
                }
            }
        }
    }
    drop_identical_items(builder, first_item);

    Some(())
}

/// Drops each item from `first_item` on that renders exactly as an earlier one
/// of them: definitions of one key in several files that show the same are a
/// single entry to the user, while ones that differ, such as overloads, stay.
fn drop_identical_items(builder: &mut CompletionBuilder, first_item: usize) {
    let items = builder.get_completion_items_mut();
    let added = items.split_off(first_item);
    let mut seen = HashSet::default();
    for item in added {
        let details = item.label_details.as_ref();
        let rendered = (
            item.label.clone(),
            format!("{:?}", item.kind),
            details.and_then(|details| details.detail.clone()),
            details.and_then(|details| details.description.clone()),
            item.detail.clone(),
        );
        if seen.insert(rendered) {
            items.push(item);
        }
    }
}

fn gmod_fallback_description_hint(
    builder: &CompletionBuilder,
    fallback_owner: Option<HookOwnerGroup>,
    member_info: &LuaMemberInfo,
) -> Option<String> {
    let prefix_owner = fallback_owner?.owner;
    let source_owner = member_hook_owner(builder.semantic_model.get_db(), member_info)?;
    (source_owner != prefix_owner).then(|| format!("from {source_owner}"))
}

/// The hook owner whose hooks `info` is among: that of the class or global
/// the member is written on.
fn member_hook_owner(db: &DbIndex, info: &LuaMemberInfo) -> Option<&'static str> {
    let Some(LuaSemanticDeclId::Member(member_id)) = &info.property_owner_id else {
        return None;
    };
    let group = match db.get_member_index().get_current_owner(member_id)? {
        LuaMemberOwner::Type(owner) => gmod_hook_owner_group_of(db, owner),
        LuaMemberOwner::GlobalPath(path) => gmod_hook_owner_group(path.get_name()),
        _ => return None,
    };
    group.first().copied()
}

/// Filters member info, returning the needed member list and the overload count
fn filter_member_infos<'a>(
    semantic_model: &SemanticModel,
    member_infos: &'a Vec<LuaMemberInfo>,
) -> Option<(Vec<&'a LuaMemberInfo>, Option<usize>)> {
    if member_infos.is_empty() {
        return None;
    }

    let mut file_decl_member: Option<&LuaMemberInfo> = None;
    let mut gmod_meta_member: Option<&LuaMemberInfo> = None;
    let mut member_with_owners: Vec<(&LuaMemberInfo, Option<LuaTypeDeclId>)> =
        Vec::with_capacity(member_infos.len());
    let mut all_doc_function = true;
    let mut overload_count = 0;

    // Collect all info in a single pass
    for member_info in member_infos {
        let owner_id = get_owner_type_id(semantic_model.get_db(), member_info);
        member_with_owners.push((member_info, owner_id.clone()));

        // Use the first file_decl as the reference, or the first member if there is none
        if file_decl_member.is_none()
            && let Some(feature) = member_info.feature
            && feature.is_file_decl()
        {
            file_decl_member = Some(member_info);
        }

        if gmod_meta_member.is_none()
            && let Some(feature) = member_info.feature
            && feature.is_meta_decl()
            && is_gmod_hook_member_info(semantic_model.get_db(), member_info)
        {
            gmod_meta_member = Some(member_info);
        }

        // Check whether all are DocFunction while counting overloads
        match &member_info.typ {
            LuaType::DocFunction(_) => {
                overload_count += 1;
            }
            LuaType::Signature(id) => {
                all_doc_function = false;
                overload_count += 1;
                if let Some(signature) = semantic_model.get_db().get_signature_index().get(id) {
                    overload_count += signature.overloads.len();
                }
            }
            _ => {
                all_doc_function = false;
            }
        }
    }

    // Determine the reference owner to use
    let final_reference_owner = if let Some(meta_member_info) = gmod_meta_member {
        get_owner_type_id(semantic_model.get_db(), meta_member_info)
    } else if let Some(file_decl_member_info) = file_decl_member {
        // Type-check against the first member so subclass member types are consistent with parent class member types
        if let Some((first_member, first_owner)) = member_with_owners.first() {
            let type_check_result =
                semantic_model.type_check(&file_decl_member_info.typ, &first_member.typ);
            if type_check_result.is_ok() {
                get_owner_type_id(semantic_model.get_db(), file_decl_member_info)
            } else {
                first_owner.clone()
            }
        } else {
            get_owner_type_id(semantic_model.get_db(), file_decl_member_info)
        }
    } else {
        // No file_decl found, use the first member as the reference
        member_with_owners
            .first()
            .and_then(|(_, owner)| owner.clone())
    };

    // Filter members with the same owner_type_id
    let mut filtered_member_infos: Vec<&LuaMemberInfo> = member_with_owners
        .into_iter()
        .filter_map(|(member_info, owner_id)| {
            if owner_id == final_reference_owner {
                Some(member_info)
            } else {
                None
            }
        })
        .collect();

    // Handle overload counting
    let final_overload_count = if overload_count >= 1 {
        let count = overload_count - 1;
        if count == 0 { None } else { Some(count) }
    } else {
        None
    };

    // If all are DocFunction, keep only the first
    if all_doc_function && !filtered_member_infos.is_empty() {
        filtered_member_infos.truncate(1);
    }

    Some((filtered_member_infos, final_overload_count))
}

enum MemberResolveState {
    All,
    Meta,
    FileDecl,
}

fn get_owner_type_id(db: &DbIndex, info: &LuaMemberInfo) -> Option<LuaTypeDeclId> {
    match &info.property_owner_id {
        Some(LuaSemanticDeclId::Member(member_id)) => {
            if let Some(owner) = db.get_member_index().get_current_owner(member_id) {
                return owner.get_type_id().cloned();
            }
            None
        }
        _ => None,
    }
}

fn get_resolve_state(db: &DbIndex, member_infos: &Vec<&LuaMemberInfo>) -> MemberResolveState {
    let mut resolve_state = MemberResolveState::All;
    if db.get_emmyrc().strict.meta_override_file_define {
        for member_info in member_infos.iter() {
            if let Some(feature) = member_info.feature {
                if feature.is_meta_decl() {
                    resolve_state = MemberResolveState::Meta;
                    break;
                } else if feature.is_file_decl() {
                    resolve_state = MemberResolveState::FileDecl;
                }
            }
        }
    }
    resolve_state
}

fn is_gmod_hook_member_info(db: &DbIndex, info: &LuaMemberInfo) -> bool {
    let Some(owner_type_id) = get_owner_type_id(db, info) else {
        return false;
    };

    let owner_name = owner_type_id.get_simple_name();
    BUILTIN_HOOK_OWNERS
        .iter()
        .any(|hook_owner| hook_owner.eq_ignore_ascii_case(owner_name))
}

/// Realm filtering state shared by every candidate member of one request.
///
/// The call-site mask depends only on the request's own position, and a file's
/// `---@realm` annotations are the same for every member declared in it. Both
/// used to be re-derived per member, which meant walking the declaring file's
/// entire syntax tree once per candidate — 98ms of a 200ms completion on a
/// gamemode workspace. The analyzer indexes those ranges for every file it
/// analyses, so this reads its binary search; the cached walk below only covers
/// a file that never reached the analyzer at all.
struct RealmFilter {
    enabled: bool,
    call_mask: GmodStateMask,
    walked: HashMap<FileId, Vec<(rowan::TextRange, GmodRealm)>>,
}

impl RealmFilter {
    fn new(builder: &CompletionBuilder) -> Self {
        let enabled = builder.semantic_model.get_emmyrc().gmod.enabled;
        let call_mask = if enabled {
            builder
                .semantic_model
                .get_db()
                .get_gmod_infer_index()
                .get_state_mask_at_offset(
                    &builder.semantic_model.get_file_id(),
                    builder.position_offset,
                )
        } else {
            GmodStateMask::empty()
        };
        Self {
            enabled,
            call_mask,
            walked: HashMap::default(),
        }
    }

    fn annotation_realm(
        &mut self,
        semantic_model: &SemanticModel,
        file_id: &FileId,
        offset: TextSize,
    ) -> Option<GmodRealm> {
        let infer_index = semantic_model.get_db().get_gmod_infer_index();
        if infer_index.has_member_realm_ranges(file_id) {
            return infer_index.get_member_annotation_realm_at_offset(file_id, offset);
        }

        let ranges = match self.walked.get(file_id) {
            Some(ranges) => ranges,
            None => {
                let ranges = collect_decl_annotation_realms(semantic_model, file_id);
                self.walked.entry(*file_id).or_insert(ranges)
            }
        };
        ranges
            .iter()
            .find(|(range, _)| range.contains(offset))
            .map(|(_, realm)| *realm)
    }

    fn accepts(&mut self, builder: &CompletionBuilder, info: &LuaMemberInfo) -> bool {
        if !self.enabled {
            return true;
        }

        let Some(property_owner_id) = &info.property_owner_id else {
            return true;
        };
        let Some((decl_file_id, decl_offset)) = semantic_decl_position(property_owner_id) else {
            return true;
        };

        let decl_mask = self
            .annotation_realm(&builder.semantic_model, &decl_file_id, decl_offset)
            .or_else(|| {
                resolve_decl_realm_without_annotation(&builder.semantic_model, property_owner_id)
            })
            .map(GmodRealm::state_mask)
            .unwrap_or_else(|| {
                builder
                    .semantic_model
                    .get_db()
                    .get_gmod_infer_index()
                    .get_state_mask_at_offset(&decl_file_id, decl_offset)
            });
        self.call_mask.is_compatible_with(decl_mask)
    }
}

/// Every `---@realm` covered range in a file, in one walk.
fn collect_decl_annotation_realms(
    semantic_model: &SemanticModel,
    file_id: &FileId,
) -> Vec<(rowan::TextRange, GmodRealm)> {
    let Some(tree) = semantic_model.get_db().get_vfs().get_syntax_tree(file_id) else {
        return Vec::new();
    };
    let mut ranges = Vec::new();
    for node in tree.get_chunk_node().syntax().descendants() {
        if let Some(func_stat) = LuaFuncStat::cast(node.clone()) {
            if let Some(comment) = func_stat.get_left_comment()
                && let Some(realm) = realm_from_doc_comment(&comment)
            {
                ranges.push((func_stat.get_range(), realm));
            }
            continue;
        }
        if let Some(local_func_stat) = LuaLocalFuncStat::cast(node)
            && let Some(comment) = local_func_stat.get_left_comment()
            && let Some(realm) = realm_from_doc_comment(&comment)
        {
            ranges.push((local_func_stat.get_range(), realm));
        }
    }
    ranges
}

fn semantic_decl_position(property_owner_id: &LuaSemanticDeclId) -> Option<(FileId, TextSize)> {
    match property_owner_id {
        LuaSemanticDeclId::LuaDecl(decl_id) => Some((decl_id.file_id, decl_id.position)),
        LuaSemanticDeclId::Member(member_id) => Some((member_id.file_id, member_id.get_position())),
        LuaSemanticDeclId::Signature(signature_id) => {
            Some((signature_id.get_file_id(), signature_id.get_position()))
        }
        LuaSemanticDeclId::TypeDecl(_) => None,
    }
}

/// The declaration's realm once its `---@realm` annotation has been ruled out.
fn resolve_decl_realm_without_annotation(
    semantic_model: &SemanticModel,
    property_owner_id: &LuaSemanticDeclId,
) -> Option<GmodRealm> {
    let (decl_file_id, decl_offset) = semantic_decl_position(property_owner_id)?;
    Some(
        semantic_model
            .get_db()
            .get_gmod_infer_index()
            .get_realm_at_offset(&decl_file_id, decl_offset),
    )
}

fn realm_from_doc_comment(comment: &LuaComment) -> Option<GmodRealm> {
    for tag in comment.get_doc_tags() {
        if let LuaDocTag::Realm(realm_tag) = tag
            && let Some(realm) = realm_from_doc_tag(&realm_tag)
        {
            return Some(realm);
        }
    }

    None
}

fn realm_from_doc_tag(tag: &LuaDocTagRealm) -> Option<GmodRealm> {
    let name = tag.get_name_token()?;
    match name.get_name_text() {
        "client" => Some(GmodRealm::Client),
        "server" => Some(GmodRealm::Server),
        "shared" => Some(GmodRealm::Shared),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;

    use super::*;

    fn function_member(key: LuaMemberKey, overload_index: Option<usize>) -> LuaMemberInfo {
        LuaMemberInfo {
            property_owner_id: None,
            key,
            typ: LuaType::Function,
            feature: None,
            overload_index,
        }
    }

    #[gtest]
    fn push_unique_member_info_keeps_distinct_overload_indices() -> Result<()> {
        let key = LuaMemberKey::Name("lookup".into());
        let mut members = HashMap::default();
        let mut existing = collect_member_identities(&members);

        push_unique_member_info(
            &mut members,
            &mut existing,
            key.clone(),
            function_member(key.clone(), Some(0)),
        );
        push_unique_member_info(
            &mut members,
            &mut existing,
            key.clone(),
            function_member(key.clone(), Some(1)),
        );
        push_unique_member_info(
            &mut members,
            &mut existing,
            key.clone(),
            function_member(key.clone(), Some(1)),
        );

        let infos = members.get(&key).ok_or("missing member infos").or_fail()?;
        verify_eq!(infos.len(), 2)
    }
}
