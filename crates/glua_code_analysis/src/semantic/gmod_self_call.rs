//! Hook contracts refine getter classes; each body/getter memo miss builds its class-wide call graph.

use std::{cell::RefCell, sync::Arc};

use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAstNode, LuaBinaryExpr, LuaCallExpr, LuaClosureExpr,
    LuaElseIfClauseStat, LuaExpr, LuaFuncStat, LuaIfStat, LuaIndexExpr, LuaIndexKey, LuaNameExpr,
    LuaParenExpr, LuaSyntaxId, LuaUnaryExpr, LuaVarExpr, UnaryOperator,
};
use rustc_hash::{FxHashMap, FxHashSet};
use smol_str::SmolStr;

use crate::{
    DbIndex, FileId, GMOD_ATTR_DEFAULT_SELF_CALL, GMOD_ATTR_NOT_ENGINE_CALLED,
    GMOD_ATTR_SELF_CALL_TYPE, GMOD_ATTR_SELF_CALL_VALID, GmodRealm, InFiled, LuaAttributeUse,
    LuaInferCache, LuaMemberFeature, LuaMemberId, LuaMemberKey, LuaMemberOwner, LuaSemanticDeclId,
    LuaSignatureId, LuaType, LuaTypeDeclId, semantic::infer_expr,
};

thread_local! {
    static REFERENCE_INFERENCE: RefCell<FxHashMap<InFiled<LuaSyntaxId>, crate::db_index::derivation::ScopeMark>> = RefCell::default();
}

pub(crate) struct ReferenceInference(InFiled<LuaSyntaxId>);

impl ReferenceInference {
    pub(crate) fn enter(reference: InFiled<LuaSyntaxId>) -> Option<Self> {
        REFERENCE_INFERENCE.with(|active| {
            let mut active = active.borrow_mut();
            if let Some(mark) = active.get(&reference) {
                crate::db_index::derivation::consult_cycle(*mark);
                return None;
            }
            active.insert(reference.clone(), crate::db_index::derivation::scope_mark());
            Some(Self(reference))
        })
    }
}

impl Drop for ReferenceInference {
    fn drop(&mut self) {
        REFERENCE_INFERENCE.with(|active| active.borrow_mut().remove(&self.0));
    }
}

/// A getter's class contract in an optional realm, independent of validity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelfCallContract {
    method: SmolStr,
    class: LuaTypeDeclId,
    realm: Option<GmodRealm>,
}

impl SelfCallContract {
    fn holds_in(&self, realm: GmodRealm) -> bool {
        match self.realm {
            None => true,
            Some(contract_realm) => {
                matches!(realm, GmodRealm::Shared | GmodRealm::Unknown) || realm == contract_realm
            }
        }
    }
}

/// An entry into a method body: a described contract, or one nothing describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OwnerEntry {
    Contract {
        contract: SelfCallContract,
        valid: bool,
    },
    Unknown,
}

/// Refines synchronous getter calls while retaining invalid arms unless the hook proves validity.
pub(crate) fn self_call_contract_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
    declared: &LuaType,
) -> Option<LuaType> {
    let file_id = cache.get_file_id();
    let (method, body) = synchronous_self_call(db, file_id, call_expr)?;
    let (class_id, key) = method_class_and_key(db, cache, &body)?;
    let entries = owner_entries(db, cache, &class_id, &key, &method);
    if entries.is_empty()
        || entries
            .iter()
            .any(|entry| matches!(entry, OwnerEntry::Unknown))
    {
        return None;
    }
    let realm = db
        .get_gmod_infer_index()
        .get_realm_at_offset(&file_id, call_expr.get_position());
    let mut classes = entries
        .iter()
        .filter_map(|entry| match entry {
            OwnerEntry::Contract { contract, .. }
                if contract.method == method && contract.holds_in(realm) =>
            {
                Some(contract.class.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if classes.is_empty() {
        return None;
    }
    classes.sort_by(|left, right| left.get_name().cmp(right.get_name()));
    classes.dedup();
    let mut types = classes.into_iter().map(LuaType::Ref).collect::<Vec<_>>();
    if !entries.iter().all(|entry| match entry {
        OwnerEntry::Contract { contract, valid } => !contract.holds_in(realm) || *valid,
        OwnerEntry::Unknown => false,
    }) {
        types.extend(invalid_arms(declared));
    }
    Some(LuaType::from_vec(types))
}

/// The nil and `NULL` arms of a declared return.
pub(crate) fn invalid_arms(declared: &LuaType) -> Vec<LuaType> {
    let is_invalid = |typ: &LuaType| {
        typ.is_nil()
            || matches!(typ, LuaType::Ref(id) | LuaType::Def(id) if id == &LuaTypeDeclId::global("NULL"))
    };
    match declared {
        LuaType::Union(union) => union
            .types()
            .filter(|typ| is_invalid(typ))
            .cloned()
            .collect(),
        typ if is_invalid(typ) => vec![typ.clone()],
        _ => Vec::new(),
    }
}

/// Finds zero-argument calls on an unchanged self in the current method body.
fn synchronous_self_call(
    db: &DbIndex,
    file_id: FileId,
    call_expr: &LuaCallExpr,
) -> Option<(SmolStr, LuaClosureExpr)> {
    if call_expr
        .get_args_list()
        .is_none_or(|args| args.get_args().next().is_some())
    {
        return None;
    }
    let call = self_call(db, file_id, call_expr)?;
    if call.deferred || self_is_reassigned(db, file_id, &call.receiver) {
        return None;
    }
    Some((call.method, call.body))
}

/// A colon call on the self parameter of an enclosing method.
struct SelfCall {
    method: SmolStr,
    body: LuaClosureExpr,
    index: LuaSyntaxId,
    receiver: LuaNameExpr,
    /// Whether a nested closure defers the call.
    deferred: bool,
}

fn self_call(db: &DbIndex, file_id: FileId, call_expr: &LuaCallExpr) -> Option<SelfCall> {
    let LuaExpr::IndexExpr(index_expr) = call_expr.get_prefix_expr()? else {
        return None;
    };
    if !index_expr.get_index_token()?.is_colon() {
        return None;
    }
    let LuaExpr::NameExpr(receiver) = index_expr.get_prefix_expr()? else {
        return None;
    };
    if receiver.get_name_text()? != "self" {
        return None;
    }
    let method = match index_expr.get_index_key()? {
        LuaIndexKey::Name(name) => SmolStr::new(name.get_name_text()),
        _ => return None,
    };
    let self_decl = db
        .get_decl_index()
        .get_decl_tree(&file_id)?
        .find_local_decl("self", receiver.get_position())?;
    let body = call_expr
        .ancestors::<LuaClosureExpr>()
        .find(|closure| method_index(closure).is_some())?;
    if !self_decl.is_implicit_self()
        && body
            .get_params_list()?
            .get_params()
            .next()
            .is_none_or(|param| param.get_position() != self_decl.get_id().position)
    {
        return None;
    }
    let deferred = call_expr
        .ancestors::<LuaClosureExpr>()
        .next()
        .is_none_or(|closure| closure.syntax() != body.syntax());
    Some(SelfCall {
        method,
        body,
        index: index_expr.get_syntax_id(),
        receiver,
        deferred,
    })
}

/// Checks whether the self binding named by `receiver` is assigned anywhere.
fn self_is_reassigned(db: &DbIndex, file_id: FileId, receiver: &LuaNameExpr) -> bool {
    let Some(self_decl) = db
        .get_decl_index()
        .get_decl_tree(&file_id)
        .and_then(|tree| tree.find_local_decl("self", receiver.get_position()))
    else {
        return true;
    };
    db.get_reference_index()
        .get_decl_references(&file_id, &self_decl.get_id())
        .is_some_and(|references| references.cells.iter().any(|cell| cell.is_write))
}

/// Finds colon methods and direct definitions with an explicit self parameter.
fn method_index(body: &LuaClosureExpr) -> Option<LuaIndexExpr> {
    if let Some(func_stat) = body.get_parent::<LuaFuncStat>() {
        let LuaVarExpr::IndexExpr(index_expr) = func_stat.get_func_name()? else {
            return None;
        };
        return index_expr
            .get_index_token()
            .is_some_and(|token| {
                token.is_colon()
                    || body
                        .get_params_list()
                        .and_then(|params| params.get_params().next())
                        .and_then(|param| param.get_name_token())
                        .is_some_and(|name| name.get_name_text() == "self")
            })
            .then_some(index_expr);
    }
    let param = body.get_params_list()?.get_params().next()?;
    if param.get_name_token()?.get_name_text() != "self" {
        return None;
    }
    let (vars, exprs) = body.get_parent::<LuaAssignStat>()?.get_var_and_expr_list();
    let position = exprs
        .iter()
        .position(|expr| expr.syntax() == body.syntax())?;
    match vars.get(position)? {
        LuaVarExpr::IndexExpr(index) => Some(index.clone()),
        _ => None,
    }
}

/// Finds the class and key of a method body.
fn method_class_and_key(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    body: &LuaClosureExpr,
) -> Option<(LuaTypeDeclId, LuaMemberKey)> {
    let index_expr = method_index(body)?;
    let key = match index_expr.get_index_key()? {
        LuaIndexKey::Name(name) => LuaMemberKey::Name(name.get_name_text().into()),
        _ => return None,
    };
    let class_id = match infer_expr(db, cache, index_expr.get_prefix_expr()?).ok()? {
        LuaType::Ref(id) | LuaType::Def(id) => id,
        _ => return None,
    };
    Some((class_id, key))
}

/// The class and its supers, each before every class it derives from.
fn class_chain(db: &DbIndex, class_id: &LuaTypeDeclId) -> Vec<LuaTypeDeclId> {
    let mut supers = Vec::new();
    class_id.collect_super_types(db, &mut supers);
    let classes = std::iter::once(class_id.clone())
        .chain(supers.into_iter().filter_map(|typ| match typ {
            LuaType::Ref(id) | LuaType::Def(id) => Some(id),
            _ => None,
        }))
        .collect::<Vec<_>>();
    let direct_supers = classes
        .iter()
        .map(|id| {
            db.get_type_index()
                .get_super_types(id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|typ| match typ {
                    LuaType::Ref(super_id) | LuaType::Def(super_id) => Some(super_id),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut ordered = Vec::with_capacity(classes.len());
    let mut placed = vec![false; classes.len()];
    while ordered.len() < classes.len() {
        let next = (0..classes.len())
            .find(|&candidate| {
                !placed[candidate]
                    && (0..classes.len()).all(|other| {
                        placed[other] || !direct_supers[other].contains(&classes[candidate])
                    })
            })
            .or_else(|| (0..classes.len()).find(|&candidate| !placed[candidate]));
        let Some(next) = next else { break };
        placed[next] = true;
        ordered.push(classes[next].clone());
    }
    ordered
}

/// Memoises a body's entries with the indexed reads that derived them.
fn owner_entries(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    class_id: &LuaTypeDeclId,
    key: &LuaMemberKey,
    getter: &SmolStr,
) -> Arc<[OwnerEntry]> {
    let memo_key = (class_id.clone(), key.clone(), getter.clone());
    if let Some(entries) = cache.tracked_get(|cache| &mut cache.self_call_owner_cache, &memo_key) {
        return entries;
    }
    let frame = crate::db_index::derivation::open_frame();
    let entries: Arc<[OwnerEntry]> = ClassGraph::build(db, class_id)
        .entries_reaching(db, cache, key, getter)
        .into();
    cache.tracked_insert(
        |cache| &mut cache.self_call_owner_cache,
        memo_key,
        entries.clone(),
        frame,
    );
    entries
}

/// The methods a class resolves and the synchronous calls between them.
struct ClassGraph {
    chain: Vec<LuaTypeDeclId>,
    /// Traversable bodies of the nearest Lua implementation, including empty entries for aliases.
    methods: FxHashMap<LuaMemberKey, Vec<(FileId, LuaClosureExpr)>>,
    /// Synchronous callers, including defaults of hooks without a Lua implementation.
    callers: FxHashMap<LuaMemberKey, Vec<LuaMemberKey>>,
    /// Methods reached through deferred or reassigned receivers.
    deferred: FxHashSet<LuaMemberKey>,
    /// The `self:M` index expressions of every call on `self` in the bodies.
    internal_calls: FxHashSet<InFiled<LuaSyntaxId>>,
}

impl ClassGraph {
    fn build(db: &DbIndex, class_id: &LuaTypeDeclId) -> Self {
        let chain = class_chain(db, class_id);
        let methods = class_method_bodies(db, &chain);
        let mut callers = FxHashMap::<LuaMemberKey, Vec<LuaMemberKey>>::default();
        let mut deferred = FxHashSet::default();
        let mut internal_calls = FxHashSet::default();
        let mut keys = methods.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        for caller in &keys {
            for (file_id, body) in &methods[caller] {
                for call in body
                    .descendants::<LuaCallExpr>()
                    .filter_map(|call_expr| self_call(db, *file_id, &call_expr))
                    .filter(|call| call.body.syntax() == body.syntax())
                {
                    let reassigned = self_is_reassigned(db, *file_id, &call.receiver);
                    if !reassigned {
                        internal_calls.insert(InFiled::new(*file_id, call.index));
                    }
                    let callee = LuaMemberKey::Name(call.method);
                    if call.deferred || reassigned {
                        deferred.insert(callee);
                    } else {
                        callers.entry(callee).or_default().push(caller.clone());
                    }
                }
            }
        }
        for (hook, targets) in default_self_calls(db, &chain) {
            if methods.contains_key(&hook) {
                continue;
            }
            for target in targets {
                callers.entry(target).or_default().push(hook.clone());
            }
        }
        for hook_callers in callers.values_mut() {
            hook_callers.sort();
            hook_callers.dedup();
        }
        Self {
            chain,
            methods,
            callers,
            deferred,
            internal_calls,
        }
    }

    /// Collects entries of `key` and its synchronous predecessors.
    fn entries_reaching(
        &self,
        db: &DbIndex,
        cache: &LuaInferCache,
        key: &LuaMemberKey,
        getter: &SmolStr,
    ) -> Vec<OwnerEntry> {
        let mut visited = FxHashSet::default();
        let mut pending = vec![(key.clone(), false)];
        let mut entries = Vec::<OwnerEntry>::new();
        while let Some((method, valid)) = pending.pop() {
            let valid = valid || hook_marks_self_call_valid(db, &self.chain, &method, getter);
            if !visited.insert((method.clone(), valid)) {
                continue;
            }
            for entry in self.entries_of(db, cache, &method, getter, valid) {
                if !entries.contains(&entry) {
                    entries.push(entry);
                }
            }
            if let Some(callers) = self.callers.get(&method) {
                pending.extend(callers.iter().cloned().map(|caller| (caller, valid)));
            }
        }
        // A contract on a getter the class implements itself does not describe it.
        entries.retain(|entry| match entry {
            OwnerEntry::Contract { contract, .. } => !self
                .methods
                .contains_key(&LuaMemberKey::Name(contract.method.clone())),
            OwnerEntry::Unknown => true,
        });
        entries
    }

    /// The entries `method` has of its own, apart from its callers.
    fn entries_of(
        &self,
        db: &DbIndex,
        cache: &LuaInferCache,
        method: &LuaMemberKey,
        getter: &SmolStr,
        valid: bool,
    ) -> Vec<OwnerEntry> {
        let mut entries = Vec::new();
        if self.deferred.contains(method) {
            entries.push(OwnerEntry::Unknown);
        }
        let hook = hook_attribute_uses(db, &self.chain, method, GMOD_ATTR_SELF_CALL_TYPE);
        let contracts = hook
            .iter()
            .filter(|(_, attribute_use)| attribute_use.id.get_name() == GMOD_ATTR_SELF_CALL_TYPE)
            .filter_map(|(signature_id, attribute_use)| {
                contract_from_attribute(db, *signature_id, attribute_use)
            })
            .collect::<Vec<_>>();
        if !contracts.is_empty() {
            let matching = contracts
                .into_iter()
                .filter(|contract| &contract.method == getter);
            entries.extend(matching.map(|contract| OwnerEntry::Contract { contract, valid }));
            if !entries
                .iter()
                .any(|entry| matches!(entry, OwnerEntry::Contract { .. }))
            {
                entries.push(OwnerEntry::Unknown);
            }
            return entries;
        }
        let declared_hook = declares_meta_member(db, &self.chain, method);
        let not_engine_called =
            !hook_attribute_uses(db, &self.chain, method, GMOD_ATTR_NOT_ENGINE_CALLED).is_empty();
        if (declared_hook && !not_engine_called) || self.has_outside_reference(db, cache, method) {
            entries.push(OwnerEntry::Unknown);
        }
        entries
    }

    /// Finds escaping or external reads whose receivers could dispatch to this class.
    fn has_outside_reference(
        &self,
        db: &DbIndex,
        cache: &LuaInferCache,
        method: &LuaMemberKey,
    ) -> bool {
        let Some(references) = db.get_reference_index().get_index_references(method) else {
            return false;
        };
        references.iter().any(|reference| {
            if self.internal_calls.contains(reference)
                || db
                    .get_member_index()
                    .get_member(&LuaMemberId::new(reference.value, reference.file_id))
                    .is_some()
            {
                return false;
            }
            let Some(index) = db
                .get_vfs()
                .syntax_node_around(&reference.file_id, reference.value)
                .and_then(LuaIndexExpr::cast)
            else {
                return true;
            };
            if self.is_presence_test(db, reference.file_id, &index) {
                return false;
            }
            let Some(prefix) = index.get_prefix_expr() else {
                return true;
            };
            let Some(_guard) = ReferenceInference::enter(reference.clone()) else {
                return true;
            };
            let mut reference_cache =
                LuaInferCache::new(reference.file_id, cache.get_config().clone());
            let Ok(receiver) = infer_expr(db, &mut reference_cache, prefix) else {
                return true;
            };
            self.receiver_may_dispatch(db, &receiver)
        })
    }

    fn receiver_may_dispatch(&self, db: &DbIndex, receiver: &LuaType) -> bool {
        match receiver {
            LuaType::Ref(id) | LuaType::Def(id) => {
                self.chain.contains(id) || class_chain(db, id).contains(&self.chain[0])
            }
            LuaType::Union(union) => union.types().any(|typ| self.receiver_may_dispatch(db, typ)),
            _ => true,
        }
    }

    fn is_presence_test(&self, db: &DbIndex, file_id: FileId, index: &LuaIndexExpr) -> bool {
        let Some(LuaExpr::NameExpr(receiver)) = index.get_prefix_expr() else {
            return false;
        };
        if receiver.get_name_text().as_deref() != Some("self")
            || self_is_reassigned(db, file_id, &receiver)
        {
            return false;
        }
        let Some(body) = index.ancestors::<LuaClosureExpr>().next() else {
            return false;
        };
        let Some(decl) = db
            .get_decl_index()
            .get_decl_tree(&file_id)
            .and_then(|tree| tree.find_local_decl("self", receiver.get_position()))
        else {
            return false;
        };
        if !decl.is_implicit_self()
            && body
                .get_params_list()
                .and_then(|params| params.get_params().next())
                .is_none_or(|param| param.get_position() != decl.get_id().position)
        {
            return false;
        }
        if !self
            .methods
            .values()
            .flatten()
            .any(|(id, method_body)| *id == file_id && method_body.syntax() == body.syntax())
        {
            return false;
        }
        let mut expr = LuaExpr::IndexExpr(index.clone());
        loop {
            let Some(parent) = expr.syntax().parent() else {
                return false;
            };
            if let Some(paren) = LuaParenExpr::cast(parent.clone()) {
                expr = LuaExpr::ParenExpr(paren);
            } else if let Some(unary) = LuaUnaryExpr::cast(parent.clone()) {
                return unary
                    .get_op_token()
                    .is_some_and(|op| op.get_op() == UnaryOperator::OpNot);
            } else if let Some(binary) = LuaBinaryExpr::cast(parent.clone()) {
                let Some(op) = binary.get_op_token().map(|op| op.get_op()) else {
                    return false;
                };
                let Some((left, right)) = binary.get_exprs() else {
                    return false;
                };
                if op == BinaryOperator::OpAnd && left.syntax() == expr.syntax() {
                    return true;
                }
                return matches!(op, BinaryOperator::OpEq | BinaryOperator::OpNe)
                    && (left.syntax().text() == "nil" || right.syntax().text() == "nil");
            } else {
                return LuaIfStat::cast(parent.clone())
                    .and_then(|stat| stat.get_condition_expr())
                    .or_else(|| {
                        LuaElseIfClauseStat::cast(parent).and_then(|stat| stat.get_condition_expr())
                    })
                    .is_some_and(|condition| condition.syntax() == expr.syntax());
            }
        }
    }
}

/// Records nearest Lua implementations independently of whether their bodies are traversable.
fn class_method_bodies(
    db: &DbIndex,
    chain: &[LuaTypeDeclId],
) -> FxHashMap<LuaMemberKey, Vec<(FileId, LuaClosureExpr)>> {
    let mut methods = FxHashMap::<LuaMemberKey, Vec<(FileId, LuaClosureExpr)>>::default();
    let mut declared = FxHashSet::default();
    for class_id in chain {
        let owner = LuaMemberOwner::Type(class_id.clone());
        let Some(members) = db.get_member_index().get_members(&owner) else {
            continue;
        };
        declared.extend(
            members
                .iter()
                .filter(|member| member.get_feature() == LuaMemberFeature::MetaMethodDecl)
                .map(|member| member.get_key().clone()),
        );
        let mut class_bodies = FxHashMap::<LuaMemberKey, Vec<(FileId, LuaClosureExpr)>>::default();
        for member in members {
            if !matches!(
                member.get_feature(),
                LuaMemberFeature::FileDefine | LuaMemberFeature::FileMethodDecl
            ) || methods.contains_key(member.get_key())
                || declared.contains(member.get_key())
            {
                continue;
            }
            let file_id = member.get_file_id();
            let bodies = class_bodies.entry(member.get_key().clone()).or_default();
            let Some(index_expr) = db
                .get_vfs()
                .syntax_node_around(&file_id, member.get_syntax_id())
                .and_then(LuaIndexExpr::cast)
            else {
                continue;
            };
            let body = index_expr
                .get_parent::<LuaFuncStat>()
                .and_then(|stat| stat.get_closure())
                .or_else(|| {
                    let assignment = index_expr.get_parent::<LuaAssignStat>()?;
                    let (vars, exprs) = assignment.get_var_and_expr_list();
                    let position = vars
                        .iter()
                        .position(|var| var.syntax() == index_expr.syntax())?;
                    match exprs.get(position)? {
                        LuaExpr::ClosureExpr(body) => Some(body.clone()),
                        _ => None,
                    }
                });
            if let Some(body) = body {
                bodies.push((file_id, body));
            }
        }
        for (method_key, mut bodies) in class_bodies {
            bodies.sort_by_key(|(file_id, body)| (file_id.id, body.get_position()));
            methods.insert(method_key, bodies);
        }
    }
    methods
}

/// Checks whether annotations declare a possible engine entry.
fn declares_meta_member(db: &DbIndex, chain: &[LuaTypeDeclId], method: &LuaMemberKey) -> bool {
    chain.iter().any(|class_id| {
        db.get_member_index()
            .get_members(&LuaMemberOwner::Type(class_id.clone()))
            .is_some_and(|members| {
                members.iter().any(|member| {
                    member.get_key() == method
                        && member.get_feature() == LuaMemberFeature::MetaMethodDecl
                })
            })
    })
}

/// Resolves each attribute family independently from its nearest declaration.
fn hook_attribute_uses(
    db: &DbIndex,
    chain: &[LuaTypeDeclId],
    method: &LuaMemberKey,
    family: &str,
) -> Vec<(LuaSignatureId, LuaAttributeUse)> {
    for class_id in chain {
        let Some(item) = db
            .get_member_index()
            .get_member_item(&LuaMemberOwner::Type(class_id.clone()), method)
        else {
            continue;
        };
        let mut uses = Vec::new();
        for member_id in item.member_ids_slice() {
            let Some(LuaType::Signature(signature_id)) = db
                .get_type_index()
                .get_type_cache(&(*member_id).into())
                .map(|type_cache| type_cache.as_type().clone())
            else {
                continue;
            };
            let Some(attribute_uses) = db
                .get_property_index()
                .get_property(&LuaSemanticDeclId::Signature(signature_id))
                .and_then(|property| property.attribute_uses())
            else {
                continue;
            };
            uses.extend(
                attribute_uses
                    .iter()
                    .filter(|attribute_use| attribute_use.id.get_name() == family)
                    .map(|attribute_use| (signature_id, attribute_use.clone())),
            );
        }
        if !uses.is_empty() {
            return uses;
        }
    }
    Vec::new()
}

/// Whether the hook `method` overrides marks `self:<getter>()` valid.
fn hook_marks_self_call_valid(
    db: &DbIndex,
    chain: &[LuaTypeDeclId],
    method: &LuaMemberKey,
    getter: &str,
) -> bool {
    hook_attribute_uses(db, chain, method, GMOD_ATTR_SELF_CALL_VALID)
        .iter()
        .any(|(_, attribute_use)| {
            attribute_use.id.get_name() == GMOD_ATTR_SELF_CALL_VALID
                && string_arg(attribute_use, "method").as_deref() == Some(getter)
        })
}

/// Lists stock hook dispatch edges in a stable order.
fn default_self_calls(
    db: &DbIndex,
    chain: &[LuaTypeDeclId],
) -> Vec<(LuaMemberKey, Vec<LuaMemberKey>)> {
    let mut hooks = Vec::new();
    for class_id in chain {
        let Some(members) = db
            .get_member_index()
            .get_members(&LuaMemberOwner::Type(class_id.clone()))
        else {
            continue;
        };
        for member in members {
            if member.get_feature() != LuaMemberFeature::MetaMethodDecl {
                continue;
            }
            let targets =
                hook_attribute_uses(db, chain, member.get_key(), GMOD_ATTR_DEFAULT_SELF_CALL)
                    .iter()
                    .filter(|(_, attribute_use)| {
                        attribute_use.id.get_name() == GMOD_ATTR_DEFAULT_SELF_CALL
                    })
                    .filter_map(|(_, attribute_use)| string_arg(attribute_use, "method"))
                    .map(LuaMemberKey::Name)
                    .collect::<Vec<_>>();
            if !targets.is_empty() {
                hooks.push((member.get_key().clone(), targets));
            }
        }
    }
    hooks.sort_by(|(left, _), (right, _)| left.cmp(right));
    hooks.dedup_by(|(left, _), (right, _)| left == right);
    hooks
}

pub(crate) fn string_arg(attribute_use: &LuaAttributeUse, name: &str) -> Option<SmolStr> {
    match attribute_use.get_param_by_name(name) {
        Some(LuaType::DocStringConst(value) | LuaType::StringConst(value)) => {
            Some(SmolStr::new(value.as_str()))
        }
        _ => None,
    }
}

/// Uses the hook's strict realm when the contract omits one.
fn contract_from_attribute(
    db: &DbIndex,
    signature_id: LuaSignatureId,
    attribute_use: &LuaAttributeUse,
) -> Option<SelfCallContract> {
    let realm = match string_arg(attribute_use, "realm").as_deref() {
        Some("client") => Some(GmodRealm::Client),
        Some("server") => Some(GmodRealm::Server),
        Some(_) => return None,
        None => match db
            .get_gmod_infer_index()
            .get_member_annotation_realm_at_offset(
                &signature_id.get_file_id(),
                signature_id.get_position(),
            )
            .unwrap_or_else(|| {
                db.get_gmod_infer_index()
                    .get_realm_at_offset(&signature_id.get_file_id(), signature_id.get_position())
            }) {
            realm @ (GmodRealm::Client | GmodRealm::Server) => Some(realm),
            _ => None,
        },
    };
    Some(SelfCallContract {
        method: string_arg(attribute_use, "method")?,
        class: LuaTypeDeclId::global(&string_arg(attribute_use, "class")?),
        realm,
    })
}
