mod cache_options;
mod footprint;
mod tracked;
mod transaction;

pub(crate) use footprint::MemoFootprint;
pub(crate) use transaction::detach_transaction;

pub use cache_options::{CacheOptions, LuaAnalysisPhase};
use glua_parser::LuaSyntaxId;
use internment::ArcIntern;
use rowan::{TextRange, TextSize};
use rustc_hash::{FxHashMap, FxHashSet};
use smol_str::SmolStr;
use std::sync::Arc;
pub use tracked::{Lookup, TrackedMap, Untracked};

use crate::db_index::derivation::{
    self, CALL_SITE_PARAMS_GATE, DIAGNOSTICS_PHASE_GATE, DYNAMIC_FIELDS_GATE, FORCE_PHASE_GATE,
    Gates, ORDERED_PHASE_GATE, OpenFrame, Provenance, Reuse,
};
use crate::{
    DbIndex, FileId, FlowId, GmodRealm, InFiled, LuaDeclId, LuaFunctionType, LuaMemberId,
    LuaMemberKey, LuaMemberOwner, LuaSemanticDeclId, VarRefId, VarRefRootId,
    db_index::{LuaType, LuaTypeDeclId},
    semantic::{
        generic::FuncGenericBinding,
        infer::{InferFailReason, ParamInferenceSource},
    },
};

pub type MemberQueryLocalKey = (LuaSyntaxId, LuaType, Option<GmodRealm>, Gates);

pub type FlowCacheInnerKey = (FlowId, GmodRealm, FlowOrigin);

trait IsEmpty {
    fn is_empty(&self) -> bool;
}

impl<K, V, S> IsEmpty for std::collections::HashMap<K, V, S> {
    fn is_empty(&self) -> bool {
        std::collections::HashMap::is_empty(self)
    }
}

impl<K, S> IsEmpty for std::collections::HashSet<K, S> {
    fn is_empty(&self) -> bool {
        std::collections::HashSet::is_empty(self)
    }
}
pub type FlowEntries = TrackedMap<FlowCacheInnerKey, CacheEntry<Arc<LuaType>>>;

/// The classes the configured scripted-class scopes bind global singleton
/// names to. Config alone decides it.
#[derive(Debug, Clone, Default)]
pub struct ScriptedSingletons {
    /// The class each global singleton name names.
    pub classes: FxHashMap<String, LuaTypeDeclId>,
    /// The names a workspace rooted at a gamemode folder binds to its
    /// gamemode class instead.
    pub root_gamemode_names: FxHashSet<String>,
}

/// The syntax-derived facts about one dynamic-field definition site.
///
/// Both are a pure function of the definition's file tree, so they are read
/// once per definition instead of re-walking from the root on every access
/// position that consults it.
#[derive(Debug, Clone, Copy, Default)]
pub struct DynamicFieldDefinitionSyntax {
    pub member_id: Option<LuaMemberId>,
    pub enclosing_assign_range: Option<TextRange>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub enum FlowOrigin {
    #[default]
    Real,
    NilCounterfactual,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VarRefCacheRootKey {
    Decl(LuaDeclId),
    Member(LuaMemberId),
    SelfRef(LuaDeclId),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VarRefCacheKey {
    VarRef(LuaDeclId),
    SelfRef(LuaDeclId),
    IndexRef(VarRefCacheRootKey, ArcIntern<SmolStr>),
    GlobalName(ArcIntern<SmolStr>),
}

impl From<&VarRefRootId> for VarRefCacheRootKey {
    fn from(value: &VarRefRootId) -> Self {
        match value {
            VarRefRootId::Decl(decl_id) => Self::Decl(*decl_id),
            VarRefRootId::Member(member_id) => Self::Member(*member_id),
            VarRefRootId::SelfRef(self_ref_id) => Self::SelfRef(self_ref_id.self_decl_id),
        }
    }
}

impl From<&VarRefId> for VarRefCacheKey {
    fn from(value: &VarRefId) -> Self {
        match value {
            VarRefId::VarRef(decl_id) => Self::VarRef(*decl_id),
            VarRefId::SelfRef(self_ref_id) => Self::SelfRef(self_ref_id.self_decl_id),
            VarRefId::IndexRef(root, path) => Self::IndexRef(root.into(), path.clone()),
            VarRefId::GlobalName(name, _) => Self::GlobalName(name.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FlowConditionKey {
    pub var_ref: VarRefId,
    pub flow: FlowId,
    pub antecedent: Option<crate::FlowAntecedent>,
    pub view: (u8, bool, bool),
    pub syntax: LuaSyntaxId,
    pub outcome: bool,
    pub realm: Option<GmodRealm>,
    pub origin: FlowOrigin,
    pub baseline: bool,
    pub keyed_origin: Option<LuaType>,
}

#[derive(Debug, Clone)]
pub enum CacheEntry<T> {
    Ready,
    Cache(T),
    /// Cached error result — used during diagnostics to prevent recomputation
    /// of expressions whose type couldn't be resolved. Shared, so an entry
    /// stays two words and the copies a flow walk stores share one reason.
    Error(Arc<InferFailReason>),
}

impl<T> CacheEntry<T> {
    pub fn error(reason: InferFailReason) -> Self {
        Self::Error(Arc::new(reason))
    }
}

/// A `self` seed replaced by [`LuaInferCache::seed_self`], and where its
/// scope began.
/// A closure-baseline walk set aside by [`LuaInferCache::suspend_baseline`].
pub(crate) struct SuspendedBaseline {
    memo: FxHashMap<(VarRefCacheKey, FlowCacheInnerKey), LuaType>,
    depth: u32,
    root: Option<VarRefCacheKey>,
    scope: derivation::ScopeMark,
}

#[derive(Debug)]
pub(crate) struct SelfSeed {
    seed: Option<(VarRefId, LuaType)>,
    mark: derivation::ScopeMark,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenericCallBindings {
    Known(Vec<FuncGenericBinding>),
    Ambiguous,
}

impl GenericCallBindings {
    pub fn record(&mut self, new_bindings: Vec<FuncGenericBinding>) {
        match self {
            GenericCallBindings::Known(existing) => {
                if existing != &new_bindings {
                    *self = GenericCallBindings::Ambiguous;
                }
            }
            GenericCallBindings::Ambiguous => {}
        }
    }
}

#[derive(Debug, Clone)]
pub struct LuaInferCache {
    file_id: FileId,
    config: CacheOptions,
    transaction: Option<Box<transaction::CacheUndo>>,
    pub expr_cache: TrackedMap<LuaSyntaxId, CacheEntry<LuaType>>,
    pub member_query_cache: TrackedMap<(LuaSyntaxId, LuaType), Result<LuaType, InferFailReason>>,
    pub member_query_in_progress: FxHashSet<(LuaSyntaxId, LuaType)>,
    pub member_query_local_cache: FxHashMap<MemberQueryLocalKey, Result<LuaType, InferFailReason>>,
    member_query_local_scope: derivation::ScopeMark,
    /// Expressions being inferred, and where each began: reaching one again
    /// is a recursion cut.
    pub(crate) expr_in_progress: FxHashMap<LuaSyntaxId, derivation::ScopeMark>,
    /// Call-site function resolution, keyed by the cheap syntax identity.
    ///
    /// The previous key hashed the whole prefix `LuaType` on every call, even
    /// on a miss. The same call site can legitimately be queried with
    /// different prefix types (e.g. `infer_tpl_ref_call` re-enters with the
    /// extended type while the outer query holds `Ready`), so the prefix type
    /// cannot be dropped from the key — it now selects among a short
    /// per-site bucket by equality instead. Buckets stay tiny (one entry for
    /// almost every call site), lookup order never affects the answer, and the
    /// `Ready` recursion guard keeps its per-type granularity.
    #[allow(clippy::type_complexity)]
    pub call_cache: FxHashMap<
        (LuaSyntaxId, Option<usize>),
        Vec<(LuaType, CacheEntry<Arc<LuaFunctionType>>, Provenance)>,
    >,
    /// Call resolutions only the evaluation that made them may use.
    #[allow(clippy::type_complexity)]
    call_cache_local:
        FxHashMap<(LuaSyntaxId, Option<usize>), Vec<(LuaType, CacheEntry<Arc<LuaFunctionType>>)>>,
    pub call_arg_types_cache: TrackedMap<(LuaSyntaxId, Option<usize>), Arc<[(LuaType, TextRange)]>>,
    pub generic_call_bindings: Untracked<FxHashMap<LuaSyntaxId, GenericCallBindings>>,
    pub flow_node_cache: FxHashMap<VarRefCacheKey, FlowEntries>,
    pub(crate) condition_flow_cache:
        TrackedMap<FlowConditionKey, Arc<crate::semantic::infer::narrow::ResultTypeOrContinue>>,
    /// Var refs whose flow entries include some only this evaluation may use.
    flow_local_keys: Vec<VarRefCacheKey>,
    /// The realm flow queries inherit, and the frame depth its scope began at.
    flow_query_realm: Option<(GmodRealm, u32)>,
    /// Scratch memo for one top-level closure-baseline query. Without it that
    /// walk re-derives each merge point once per path into it.
    ///
    /// Cleared when the outermost baseline query returns: a baseline answer
    /// depends on how far the pass has got, which is not in the key, so one
    /// must never answer a later query. A query for another variable that an
    /// inference during the walk starts is a query of its own, with its own
    /// memo (see [`Self::suspend_baseline`]).
    pub baseline_flow_memo: FxHashMap<(VarRefCacheKey, FlowCacheInnerKey), LuaType>,
    pub baseline_flow_depth: u32,
    /// The variable the closure-baseline walk in progress is for.
    pub(crate) baseline_root: Option<VarRefCacheKey>,
    /// Where the outermost closure-baseline query began.
    pub(crate) baseline_scope: derivation::ScopeMark,
    pub flow_node_realm_cache: TrackedMap<FlowId, GmodRealm>,
    /// Whether the numeric-index fast path applies to a var ref, keyed by the
    /// var ref plus the realm the query runs under (the index-key probe goes
    /// through `infer_expr`, which is realm sensitive).
    ///
    /// Derives from the immutable decl initializer plus inference results, so
    /// it is dropped wherever `expr_cache` is dropped.
    pub numeric_table_index_query_cache:
        TrackedMap<(VarRefCacheKey, Option<GmodRealm>), Option<(VarRefRootId, i64)>>,
    /// Result of the unambiguous-integer-const scan, keyed by name and the
    /// read position. The scan walks the whole decl tree, so each distinct
    /// (name, position) pays it at most once per inference wave. Dropped
    /// wherever `expr_cache` is dropped: the scanned types settle over waves.
    pub unambiguous_int_const_cache: Untracked<FxHashMap<(SmolStr, TextSize), Option<i64>>>,
    /// Lazily built per-file `name -> decls` lookup feeding the scan above.
    /// One decl-tree walk per wave instead of one per distinct (name,
    /// position) query. Derives only from the immutable decl tree, so it is
    /// dropped alongside the scan results.
    pub(crate) decl_name_index: Option<FxHashMap<SmolStr, Vec<LuaDeclId>>>,
    /// Whether the branch region between a flow node and a stop point
    /// contains a special call effect matching a var ref, keyed by
    /// (node, stop, var ref).
    ///
    /// Pure syntax plus the immutable flow index, so it survives deferred and
    /// unresolve clearing like the other structural caches.
    pub branch_special_call_effect_cache: TrackedMap<(FlowId, FlowId, VarRefCacheKey), bool>,
    /// Empty `visited_flow_ids` buffers for the flow walk. A walk takes one
    /// and returns it afterwards, so repeated and nested walks (a merge point
    /// re-enters through `get_type_at_flow_with_origin`) reuse allocations.
    flow_walk_scratch: Vec<Vec<FlowId>>,
    /// The origin type flow walks of an index ref start from, and where the
    /// caller that set it began.
    index_ref_origin_type_cache:
        FxHashMap<VarRefCacheKey, (CacheEntry<LuaType>, derivation::ScopeMark)>,
    /// The context keys of the references whose walks start from a keyed
    /// origin now: index refs with an origin, and a seeded `self` by
    /// reference and by declaration.
    pub(crate) keyed: Vec<u64>,
    pub param_type_cache: Untracked<FxHashMap<LuaDeclId, CacheEntry<LuaType>>>,
    pub(crate) param_type_source_cache:
        Untracked<FxHashMap<LuaDeclId, CacheEntry<ParamInferenceSource>>>,
    pub expr_var_ref_id_cache: TrackedMap<LuaSyntaxId, VarRefId>,
    /// Field comparisons with a literal whose owner is being narrowed by them,
    /// and where each narrowing began: a walk of a path under the owner stops
    /// there.
    pub(crate) field_literal_narrowing: FxHashMap<LuaSyntaxId, derivation::ScopeMark>,
    pub scripted_global_singleton_type_cache: Option<ScriptedSingletons>,
    /// The gamemode class of the file's workspace when it is rooted at a
    /// gamemode folder.
    pub root_gamemode_class_cache: TrackedMap<(), Option<LuaTypeDeclId>>,
    /// Cache for `self` type per enclosing method (keyed by LuaFuncStat syntax_id).
    /// Avoids repeated ancestor walks and type resolution for each `self` reference
    /// within the same method body.
    pub self_type_cache: TrackedMap<LuaSyntaxId, Option<LuaType>>,
    /// Region-aware base type seed for an implicit `self` flow query, set by
    /// `infer_self` for the duration of a single `infer_expr_narrow_type_with_self_base`
    /// call. When the flow walk reaches the origin for the matching `SelfRef`,
    /// this seed is used as the base type instead of the (position-insensitive)
    /// receiver decl/member cache, so reused locals resolve `self` per region
    /// while still going through the normal narrowing pipeline.
    pub self_base_seed: Option<(VarRefId, LuaType)>,
    /// Where `self_base_seed`'s scope began.
    pub(crate) self_base_seed_mark: derivation::ScopeMark,
    /// Flow entries of a reference walked from a given origin — a `self`
    /// seed or an index ref's member type — by that origin: they hold for it
    /// alone.
    keyed_flow_node_cache: FxHashMap<(VarRefCacheKey, LuaType), FlowEntries>,
    /// Keyed flow entry sets holding entries only this evaluation may use.
    keyed_flow_local_keys: Vec<(VarRefCacheKey, LuaType)>,
    /// Cache for `find_decl` results so that multiple diagnostic checkers
    /// processing the same file don't redo the full member-resolution chain.
    pub decl_cache: Untracked<FxHashMap<LuaSyntaxId, Option<LuaSemanticDeclId>>>,
    /// Cache for resolved generic-for variable types. For `pairs` loops over
    /// templated tables, each use of the loop value can otherwise re-run the
    /// full iterator inference from the enclosing `for` statement.
    pub for_range_iter_var_type_cache: Untracked<FxHashMap<LuaDeclId, CacheEntry<LuaType>>>,
    /// Cache for the in-place `ipairs` transform recogniser, keyed by the array
    /// local's declaration. `None` means the block holds no such transform loop;
    /// `Some((loop_end, element_type))` records where the transform completes and
    /// the element type it leaves, so a read is answered without re-scanning the
    /// declaration's block each time.
    pub in_place_ipairs_transform_cache:
        Untracked<FxHashMap<LuaDeclId, Option<(TextSize, LuaType)>>>,
    pub dynamic_field_resolution_cache: TrackedMap<
        (LuaType, LuaMemberKey, Option<TextSize>),
        Option<(LuaType, Option<LuaSemanticDeclId>)>,
    >,
    /// A table literal's own informative answer for a literal key, by where
    /// in the caller file it holds. Union reads repeat it for every arm.
    pub exact_table_member_cache: TrackedMap<
        (
            (
                InFiled<TextRange>,
                LuaMemberKey,
                FileId,
                Option<InFiled<rowan::TextSize>>,
            ),
            crate::semantic::member::MemberReadSite,
        ),
        LuaType,
    >,
    /// Memo for `infer_owner_raw_member_type_with_realm`, keyed by owner, key,
    /// caller file, where in the file the answer holds and table-generation
    /// cutoff. The union fold over writers is identical for every read of the
    /// same member from the same place, so repeated reads share one answer.
    /// Position sensitivity comes only from offset visibility, which the read
    /// site and cutoff in the key capture. Derived from member/type/realm index state, so it is
    /// dropped wherever expression inference results are dropped.
    pub owner_raw_member_type_cache: TrackedMap<
        (
            LuaMemberOwner,
            LuaMemberKey,
            FileId,
            crate::semantic::member::MemberReadSite,
            Option<InFiled<TextSize>>,
        ),
        Result<LuaType, InferFailReason>,
    >,
    /// Memo for `local_class_table_member_ids`, keyed by class and member key.
    /// Each entry carries the `DbIndex::type_structure_revision` it was built
    /// against; a hit is valid only when the revision still matches. The
    /// revision bumps on every mutable type/member/signature/module access
    /// and on remove/clear, which covers the type-bound-decl and member-slot
    /// writes this query reads, so mutable analysis never sees a stale hit.
    /// Decl initializers and VFS syntax are fixed within a batch, and a
    /// synthetic decl only becomes relevant after a type-index bind that
    /// bumps the revision.
    pub local_class_table_member_ids_cache:
        TrackedMap<(LuaTypeDeclId, LuaMemberKey), (u64, Arc<Vec<LuaMemberId>>)>,
    pub dynamic_field_type_cache: TrackedMap<LuaMemberId, Option<LuaType>>,
    pub dynamic_field_definition_syntax_cache:
        TrackedMap<InFiled<TextRange>, DynamicFieldDefinitionSyntax>,
    /// Dynamic fields being resolved, and where each began: a read of one is a
    /// recursion cut.
    pub dynamic_field_resolving: FxHashMap<LuaMemberId, derivation::ScopeMark>,
    pub vgui_parent_fallback_calls: FxHashSet<LuaSyntaxId>,
    /// `GetParent` reads answered *through* a resolved vgui parent chain.
    ///
    /// The mirror of [`Self::vgui_parent_fallback_calls`]: a chain answer taken
    /// before every group's relations landed can be one the final chain state
    /// contradicts, so the files holding these reads are re-derived alongside
    /// the fallback files once the chains settle.
    pub vgui_parent_chain_calls: FxHashSet<LuaSyntaxId>,
    /// Call sites of a local function, keyed by its declaration. Syntax ids,
    /// not nodes: red nodes are `!Send`.
    pub local_function_call_sites_cache: FxHashMap<LuaDeclId, Arc<Vec<LuaSyntaxId>>>,
    /// Whether a call diverges, keyed by the call expression. The flow walk asks
    /// this of every call node it reaches, and answering means resolving what
    /// the call targets through the reference, property, member and signature
    /// indexes.
    pub call_returns_never_cache: TrackedMap<LuaSyntaxId, bool>,
    /// Whether a call site's signature is generic, keyed by the call
    /// expression. Consulted per call-site during inference, so re-inferring
    /// the call prefix each time is avoided.
    pub signature_is_generic_cache: Untracked<FxHashMap<LuaSyntaxId, Option<bool>>>,
}

impl LuaInferCache {
    pub fn new(file_id: FileId, config: CacheOptions) -> Self {
        Self {
            file_id,
            transaction: None,
            config,
            expr_cache: Default::default(),
            member_query_cache: Default::default(),
            member_query_in_progress: Default::default(),
            member_query_local_cache: Default::default(),
            member_query_local_scope: Default::default(),
            expr_in_progress: Default::default(),
            call_cache: Default::default(),
            call_cache_local: Default::default(),
            call_arg_types_cache: Default::default(),
            flow_node_cache: Default::default(),
            condition_flow_cache: Default::default(),
            flow_local_keys: Vec::new(),
            flow_query_realm: None,
            baseline_flow_memo: Default::default(),
            baseline_flow_depth: 0,
            baseline_root: None,
            baseline_scope: derivation::ScopeMark::default(),
            flow_node_realm_cache: Default::default(),
            numeric_table_index_query_cache: Default::default(),
            unambiguous_int_const_cache: Default::default(),
            decl_name_index: None,
            branch_special_call_effect_cache: Default::default(),
            flow_walk_scratch: Vec::new(),
            index_ref_origin_type_cache: Default::default(),
            keyed: Vec::new(),
            param_type_cache: Default::default(),
            param_type_source_cache: Default::default(),
            expr_var_ref_id_cache: Default::default(),
            field_literal_narrowing: Default::default(),
            scripted_global_singleton_type_cache: None,
            root_gamemode_class_cache: Default::default(),
            self_type_cache: Default::default(),
            self_base_seed: None,
            self_base_seed_mark: derivation::ScopeMark::default(),
            keyed_flow_node_cache: Default::default(),
            keyed_flow_local_keys: Vec::new(),
            decl_cache: Default::default(),
            for_range_iter_var_type_cache: Default::default(),
            in_place_ipairs_transform_cache: Default::default(),
            dynamic_field_resolution_cache: Default::default(),
            exact_table_member_cache: Default::default(),
            owner_raw_member_type_cache: Default::default(),
            local_class_table_member_ids_cache: Default::default(),
            dynamic_field_type_cache: Default::default(),
            dynamic_field_definition_syntax_cache: Default::default(),
            dynamic_field_resolving: Default::default(),
            vgui_parent_fallback_calls: Default::default(),
            vgui_parent_chain_calls: Default::default(),
            local_function_call_sites_cache: Default::default(),
            call_returns_never_cache: Default::default(),
            signature_is_generic_cache: Default::default(),
            generic_call_bindings: Default::default(),
        }
    }

    pub fn member_query_local_key(
        &mut self,
        syntax: LuaSyntaxId,
        prefix: &LuaType,
    ) -> Option<MemberQueryLocalKey> {
        if !self.config.analysis_phase.is_diagnostics()
            || derivation::current() != Some(derivation::DerivationId::diagnostics(self.file_id))
            || !self.expr_in_progress.is_empty()
            || !self.member_query_in_progress.is_empty()
            || !self.keyed.is_empty()
            || self.self_base_seed.is_some()
            || self.baseline_flow_depth != 0
        {
            return None;
        }
        let scope = derivation::scope_mark();
        if scope != self.member_query_local_scope {
            self.member_query_local_cache.clear();
            self.member_query_local_scope = scope;
        }
        Some((
            syntax,
            prefix.clone(),
            self.flow_query_realm.map(|(realm, _)| realm),
            self.view_gates(),
        ))
    }

    pub fn get_config(&self) -> &CacheOptions {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut CacheOptions {
        &mut self.config
    }

    pub fn get_file_id(&self) -> FileId {
        self.file_id
    }

    /// An empty cache inferring under the same view and scoped context.
    pub fn fork_fresh(&self) -> Self {
        let mut fresh = Self::new(self.file_id, self.config.clone());
        fresh.flow_query_realm = self.flow_query_realm;
        fresh.self_base_seed = self.self_base_seed.clone();
        fresh.self_base_seed_mark = self.self_base_seed_mark;
        fresh.index_ref_origin_type_cache = self.index_ref_origin_type_cache.clone();
        fresh.keyed = self.keyed.clone();
        fresh
    }

    /// The view gates this cache infers under.
    fn view_gates(&self) -> Gates {
        let phase = match self.config.analysis_phase {
            LuaAnalysisPhase::Ordered => ORDERED_PHASE_GATE,
            LuaAnalysisPhase::Force => FORCE_PHASE_GATE,
            LuaAnalysisPhase::Diagnostics => DIAGNOSTICS_PHASE_GATE,
            LuaAnalysisPhase::Unordered => 0,
        };
        (u8::from(self.config.dynamic_fields_visible) * DYNAMIC_FIELDS_GATE)
            | (u8::from(self.config.call_site_params_visible) * CALL_SITE_PARAMS_GATE)
            | phase
    }

    /// What a tracked lookup is made under now.
    pub fn lookup(&self) -> Lookup {
        Lookup {
            gates: self.view_gates(),
            realm: self.flow_query_realm,
        }
    }

    /// Closes a frame opened for a memo miss: what the value was computed
    /// from, or `None` when it stays with this evaluation.
    #[track_caller]
    pub fn close_frame(&self, frame: Option<OpenFrame>) -> Option<Provenance> {
        derivation::close_frame(frame?, self.view_gates())
    }

    /// Under `GLUALS_CHECK_CACHE`, starts watching lookups for a value another
    /// evaluation computed; see [`Self::check_reuse`].
    pub(crate) fn watch_reuse(&self) {
        if derivation::check_cache() {
            derivation::take_reused_across();
        }
    }

    /// Under `GLUALS_CHECK_CACHE`, when a lookup since [`Self::watch_reuse`]
    /// took a value another evaluation computed, computes it again in a fresh
    /// cache and reports any difference.
    pub(crate) fn check_reuse<T: PartialEq + std::fmt::Debug>(
        &self,
        site: impl std::fmt::Debug,
        shared: &T,
        recompute: impl FnOnce(&mut Self) -> Option<T>,
    ) {
        if !(derivation::check_cache() && derivation::take_reused_across()) {
            return;
        }
        let fresh = recompute(&mut self.fork_fresh());
        if derivation::worker_requires_canonical() {
            return;
        }
        if fresh.as_ref() != Some(shared) {
            eprintln!(
                "[cache] mismatch {:?} {site:?} {:?} shared={shared:?} fresh={fresh:?}",
                self.get_file_id(),
                derivation::current(),
            );
        }
    }

    /// The realm flow queries inherit, if one is set.
    pub fn query_realm(&self) -> Option<GmodRealm> {
        match self.flow_query_realm {
            Some((realm, base)) => {
                derivation::consult_realm(Some(realm), base);
                Some(realm)
            }
            None => {
                derivation::consult_realm(None, 0);
                None
            }
        }
    }

    /// Sets the realm nested flow queries inherit; hand the result back to
    /// [`Self::restore_query_realm`].
    pub fn set_query_realm(&mut self, realm: Option<GmodRealm>) -> Option<(GmodRealm, u32)> {
        let depth = derivation::frame_depth();
        std::mem::replace(
            &mut self.flow_query_realm,
            realm.map(|realm| (realm, depth)),
        )
    }

    pub fn restore_query_realm(&mut self, previous: Option<(GmodRealm, u32)>) {
        self.flow_query_realm = previous;
    }

    /// Seeds the walks of the implicit `self` `var_ref_id` with `seed`; hand
    /// the result back to [`Self::restore_self_seed`].
    pub(crate) fn seed_self(&mut self, var_ref_id: VarRefId, seed: LuaType) -> SelfSeed {
        let previous = SelfSeed {
            seed: self.self_base_seed.take(),
            mark: std::mem::replace(&mut self.self_base_seed_mark, derivation::scope_mark()),
        };
        if let Some((seeded, _)) = &previous.seed {
            self.unkey(Self::seed_keys(seeded));
        }
        self.key(Self::seed_keys(&var_ref_id));
        self.self_base_seed = Some((var_ref_id, seed));
        previous
    }

    pub(crate) fn restore_self_seed(&mut self, previous: SelfSeed) {
        if let Some((seeded, _)) = self.self_base_seed.take() {
            self.unkey(Self::seed_keys(&seeded));
        }
        if let Some((seeded, _)) = &previous.seed {
            self.key(Self::seed_keys(seeded));
        }
        self.self_base_seed = previous.seed;
        self.self_base_seed_mark = previous.mark;
    }

    /// The context keys a seed for `var_ref_id` answers: the reference, and
    /// its declaration.
    fn seed_keys(var_ref_id: &VarRefId) -> impl Iterator<Item = u64> {
        std::iter::once(Self::flow_context_key(&VarRefCacheKey::from(var_ref_id))).chain(
            var_ref_id
                .get_decl_id_ref()
                .map(Self::self_decl_context_key),
        )
    }

    fn key(&mut self, keys: impl Iterator<Item = u64>) {
        self.keyed.extend(keys);
    }

    fn unkey(&mut self, keys: impl Iterator<Item = u64>) {
        for key in keys {
            if let Some(at) = self.keyed.iter().position(|keyed| *keyed == key) {
                self.keyed.swap_remove(at);
            }
        }
    }

    /// Sets the closure-baseline walk in progress aside, so a query for another
    /// variable runs as a walk of its own; hand the result back to
    /// [`Self::resume_baseline`].
    pub(crate) fn suspend_baseline(&mut self) -> SuspendedBaseline {
        SuspendedBaseline {
            memo: std::mem::take(&mut self.baseline_flow_memo),
            depth: std::mem::take(&mut self.baseline_flow_depth),
            root: self.baseline_root.take(),
            scope: self.baseline_scope,
        }
    }

    /// Resumes the walk [`Self::suspend_baseline`] set aside.
    pub(crate) fn resume_baseline(&mut self, suspended: SuspendedBaseline) {
        self.baseline_flow_memo = suspended.memo;
        self.baseline_flow_depth = suspended.depth;
        self.baseline_root = suspended.root;
        self.baseline_scope = suspended.scope;
    }

    /// Whether dynamic fields are visible, noting the gate consulted.
    pub fn dynamic_fields_visible(&self) -> bool {
        derivation::consult_gates(DYNAMIC_FIELDS_GATE);
        self.config.dynamic_fields_visible
    }

    /// Marks `call` as a `GetParent` read the vgui parent fallback answered.
    /// Readers of the call learn it from here, not from the value, so the
    /// value stays with this evaluation.
    pub(crate) fn mark_vgui_parent_fallback(&mut self, call: LuaSyntaxId) {
        derivation::taint();
        self.vgui_parent_fallback_calls.insert(call);
    }

    /// Marks `call` as a `GetParent` read answered through a resolved vgui
    /// parent chain; see [`Self::mark_vgui_parent_fallback`].
    pub(crate) fn mark_vgui_parent_chain(&mut self, call: LuaSyntaxId) {
        derivation::taint();
        self.vgui_parent_chain_calls.insert(call);
    }

    /// Whether inference runs in the ordered phase, noting the gate consulted.
    pub fn phase_is_ordered(&self) -> bool {
        derivation::consult_gates(ORDERED_PHASE_GATE);
        self.config.analysis_phase.is_ordered()
    }

    /// Whether inference runs in the force phase, noting the gate consulted.
    pub fn phase_is_force(&self) -> bool {
        derivation::consult_gates(FORCE_PHASE_GATE);
        self.config.analysis_phase.is_force()
    }

    /// Whether inference runs for diagnostics, noting the gate consulted.
    pub fn phase_is_diagnostics(&self) -> bool {
        derivation::consult_gates(DIAGNOSTICS_PHASE_GATE);
        self.config.analysis_phase.is_diagnostics()
    }

    /// Whether call-site parameter facts are visible, noting the gate
    /// consulted.
    pub fn call_site_params_visible(&self) -> bool {
        derivation::consult_gates(CALL_SITE_PARAMS_GATE);
        self.config.call_site_params_visible
    }

    /// Presize the maps the file walk and the deferred waves fill, from the
    /// same text-length hint the member/reference indexes reserve with
    /// (`crate::compilation::analyzer::common::estimate_file_index_entries`).
    ///
    /// `expr_cache` alone takes roughly one entry per expression — for a large
    /// file that is thousands of inserts, each one a rehash step on an
    /// unsized map, repeated on every worker walk. `HashMap::clear` keeps
    /// capacity, so sizing once here also covers every deferred-results wave
    /// that reuses this cache. Fractions keep the small maps near their
    /// measured occupancy; under- or over-reserving only changes how much
    /// rehashing remains, never what is cached.
    pub fn reserve_inference_maps(&mut self, entries: usize) {
        self.expr_cache.reserve(entries);
        self.expr_var_ref_id_cache.reserve(entries / 2);
        self.decl_cache.reserve(entries / 4);
        self.call_cache.reserve(entries / 8);
        self.self_type_cache.reserve(entries / 8);
        self.flow_node_cache.reserve(entries / 8);
        self.param_type_cache.reserve(entries / 8);
        self.for_range_iter_var_type_cache.reserve(entries / 16);
    }

    /// The value `map` holds for `key` if it holds now, recording what it was
    /// computed from.
    #[track_caller]
    pub fn tracked_get<K: std::hash::Hash + Eq + Clone, V: Clone>(
        &mut self,
        map: fn(&mut Self) -> &mut TrackedMap<K, V>,
        key: &K,
    ) -> Option<V> {
        let lookup = self.lookup();
        let keyed = std::mem::take(&mut self.keyed);
        let value = map(self).get(key, lookup, &keyed).cloned();
        self.keyed = keyed;
        value
    }

    /// Closes `frame` and stores the value computed in it.
    #[track_caller]
    pub fn tracked_insert<K: std::hash::Hash + Eq + Clone, V>(
        &mut self,
        map: fn(&mut Self) -> &mut TrackedMap<K, V>,
        key: K,
        value: V,
        frame: Option<OpenFrame>,
    ) {
        let provenance = self.close_frame(frame);
        map(self).insert(key, value, provenance);
    }

    /// Runs `f` inferring under `config`: memos made under another view are
    /// kept only where tracked, and checked against the view they consulted.
    pub fn with_config<T>(&mut self, config: CacheOptions, f: impl FnOnce(&mut Self) -> T) -> T {
        let previous = std::mem::replace(&mut self.config, config);
        self.clear();
        let result = f(self);
        self.clear();
        self.config = previous;
        result
    }

    pub fn set_phase(&mut self, phase: LuaAnalysisPhase) {
        self.config.analysis_phase = phase;
    }

    /// Records the bindings a generic call resolved with, for the statement
    /// walk to read back: a side channel, so a memo computed through it is
    /// not reusable without it.
    pub fn record_generic_call_bindings(
        &mut self,
        syntax_id: LuaSyntaxId,
        bindings: Vec<FuncGenericBinding>,
    ) {
        derivation::taint();
        match self.generic_call_bindings.0.entry(syntax_id) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().record(bindings);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(GenericCallBindings::Known(bindings));
            }
        }
    }

    pub fn get_generic_call_bindings(
        &self,
        syntax_id: &LuaSyntaxId,
    ) -> Option<&[FuncGenericBinding]> {
        match self.generic_call_bindings.get(syntax_id) {
            Some(GenericCallBindings::Known(bindings)) => Some(bindings),
            _ => None,
        }
    }

    pub fn record_missed_member_read(
        &mut self,
        db: &crate::DbIndex,
        owner: crate::LuaMemberOwner,
        key: LuaMemberKey,
    ) {
        crate::semantic::member::record_member_slot_read(db, &owner, &key);
    }

    /// Drops every memo a write may have made stale. Tracked entries stay:
    /// each is checked against its reads when next used.
    pub fn clear(&mut self) {
        fn empty<M: Default + IsEmpty>(map: &mut M) {
            if !map.is_empty() {
                *map = M::default();
            }
        }
        self.expr_cache.clear_local();
        self.condition_flow_cache.clear_local();
        self.member_query_cache.clear_local();
        self.member_query_in_progress.clear();
        self.member_query_local_cache.clear();
        empty(&mut self.expr_in_progress);
        self.expr_var_ref_id_cache.clear_local();
        self.call_returns_never_cache.clear_local();
        self.local_class_table_member_ids_cache.clear_local();
        self.dynamic_field_resolution_cache.clear_local();
        self.exact_table_member_cache.clear_local();
        self.flow_node_realm_cache.clear_local();
        self.numeric_table_index_query_cache.clear_local();
        self.branch_special_call_effect_cache.clear_local();
        for key in self.flow_local_keys.drain(..) {
            if let Some(entries) = self.flow_node_cache.get_mut(&key) {
                entries.clear_local();
            }
        }
        self.clear_keyed_flow_locals();
        empty(&mut self.call_cache_local);
        self.call_arg_types_cache.clear_local();
        empty(&mut self.generic_call_bindings.0);
        self.flow_query_realm = None;
        empty(&mut self.baseline_flow_memo);
        self.baseline_flow_depth = 0;
        self.baseline_root = None;
        empty(&mut self.unambiguous_int_const_cache.0);
        self.decl_name_index = None;
        empty(&mut self.index_ref_origin_type_cache);
        self.keyed.clear();
        empty(&mut self.param_type_cache.0);
        empty(&mut self.param_type_source_cache.0);
        empty(&mut self.field_literal_narrowing);
        self.self_type_cache.clear_local();
        self.self_base_seed = None;
        self.root_gamemode_class_cache.clear_local();

        empty(&mut self.decl_cache.0);
        empty(&mut self.for_range_iter_var_type_cache.0);
        empty(&mut self.in_place_ipairs_transform_cache.0);
        self.owner_raw_member_type_cache.clear_local();
        self.dynamic_field_type_cache.clear_local();
        self.dynamic_field_definition_syntax_cache.clear_local();
        empty(&mut self.dynamic_field_resolving);
        empty(&mut self.vgui_parent_fallback_calls);
        empty(&mut self.vgui_parent_chain_calls);
        empty(&mut self.local_function_call_sites_cache);
        empty(&mut self.signature_is_generic_cache.0);
    }

    /// [`Self::clear`]s a memo stored between evaluations, and frees the room
    /// its local values took.
    pub fn park(&mut self) {
        for key in std::mem::take(&mut self.flow_local_keys) {
            if let Some(entries) = self.flow_node_cache.get_mut(&key) {
                entries.clear_local();
                entries.release_local();
            }
        }
        for key in std::mem::take(&mut self.keyed_flow_local_keys) {
            if let Some(entries) = self.keyed_flow_node_cache.get_mut(&key) {
                entries.clear_local();
                entries.release_local();
            }
        }
        self.clear();
        macro_rules! release {
            ($($field:ident),+ $(,)?) => {
                $(self.$field.release_local();)+
            };
        }
        release!(
            expr_cache,
            member_query_cache,
            call_arg_types_cache,
            condition_flow_cache,
            flow_node_realm_cache,
            numeric_table_index_query_cache,
            branch_special_call_effect_cache,
            expr_var_ref_id_cache,
            self_type_cache,
            dynamic_field_resolution_cache,
            exact_table_member_cache,
            local_class_table_member_ids_cache,
            dynamic_field_type_cache,
            dynamic_field_definition_syntax_cache,
            call_returns_never_cache,
            owner_raw_member_type_cache,
            root_gamemode_class_cache,
        );
    }

    /// Discards the inference a wave of deferred resolution can have
    /// invalidated.
    pub fn clear_deferred_inference_results(&mut self) {
        self.clear_keyed_flow_locals();
        self.expr_cache.clear();
        self.condition_flow_cache.clear();
        self.member_query_cache.clear();
        self.member_query_in_progress.clear();
        self.member_query_local_cache.clear();
        self.clear_call_cache();
        self.call_cache_local.clear();
        self.call_arg_types_cache.clear();
        self.numeric_table_index_query_cache.clear();
        self.unambiguous_int_const_cache.clear();
        self.decl_name_index = None;
        // A resolved signature return is exactly what turns this answer from
        // `false` to `true`, so it cannot survive a wave.
        self.call_returns_never_cache.clear();
        self.signature_is_generic_cache.clear();
        self.generic_call_bindings.clear();
        self.owner_raw_member_type_cache.clear();
        if let Some(undo) = Self::active_undo(&mut self.transaction) {
            let removed = self
                .flow_node_cache
                .iter_mut()
                .filter_map(|(key, inner)| {
                    if inner.retain_with_changes(|_, entry| !matches!(entry, CacheEntry::Error(_)))
                    {
                        undo.touch_flow(key);
                    }
                    inner.is_empty().then(|| key.clone())
                })
                .collect::<Vec<_>>();
            for key in removed {
                self.capture_flow(&key);
                self.flow_node_cache.remove(&key);
            }
        } else {
            self.flow_node_cache.retain(|_, inner| {
                inner.retain(|_, entry| !matches!(entry, CacheEntry::Error(_)));
                !inner.is_empty()
            });
        }
        self.param_type_cache
            .retain(|_, entry| !matches!(entry, CacheEntry::Error(_)));
        self.param_type_source_cache
            .retain(|_, entry| !matches!(entry, CacheEntry::Error(_)));
        self.for_range_iter_var_type_cache
            .retain(|_, entry| !matches!(entry, CacheEntry::Error(_)));
        // The transform's element type comes from inferring the loop body's
        // right-hand side, which a resolved signature return can change.
        self.in_place_ipairs_transform_cache.clear();
        self.dynamic_field_resolution_cache.clear();
        self.exact_table_member_cache.clear();
        self.dynamic_field_type_cache.clear();
    }

    /// Clears inference results that can become stale as deferred declarations,
    /// members, signatures, and dynamic fields are resolved. Structural caches
    /// derived only from the immutable syntax/reference/flow indexes survive the
    /// phase transition so unresolve does not rebuild them for every expression.
    pub fn clear_for_unresolve(&mut self, db: &DbIndex) {
        self.clear_keyed_flow_locals();
        self.expr_cache.clear();
        self.condition_flow_cache.clear();
        self.member_query_cache.clear();
        self.member_query_in_progress.clear();
        self.member_query_local_cache.clear();
        self.clear_call_cache();
        self.call_cache_local.clear();
        self.call_arg_types_cache.clear();
        self.numeric_table_index_query_cache.clear();
        self.unambiguous_int_const_cache.clear();
        self.clear_flow_cache();
        self.flow_query_realm = None;
        self.baseline_flow_memo.clear();
        self.baseline_flow_depth = 0;
        self.baseline_root = None;
        self.index_ref_origin_type_cache.clear();
        self.keyed.clear();
        self.param_type_cache.clear();
        self.param_type_source_cache.clear();
        self.call_returns_never_cache.clear();
        self.signature_is_generic_cache.clear();
        self.generic_call_bindings.clear();
        self.owner_raw_member_type_cache.clear();
        // Local reference identities come directly from immutable reference
        // indexes and are safe to retain. Global/member/self roots can be
        // selected through types and overloads that unresolve is about to
        // materialize, so discard only those semantic entries.
        self.expr_var_ref_id_cache
            .retain(|syntax_id, var_ref_id| var_ref_is_phase_stable(db, syntax_id, var_ref_id));
        self.field_literal_narrowing.clear();
        self.self_type_cache.clear();
        self.self_base_seed = None;
        self.decl_cache.clear();
        self.for_range_iter_var_type_cache.clear();
        self.in_place_ipairs_transform_cache.clear();
        self.dynamic_field_resolution_cache.clear();
        self.exact_table_member_cache.clear();
        self.dynamic_field_type_cache.clear();
        self.dynamic_field_definition_syntax_cache.clear();
        self.dynamic_field_resolving.clear();
        self.vgui_parent_fallback_calls.clear();
        self.vgui_parent_chain_calls.clear();
    }

    pub fn get_flow_cache(
        &mut self,
        var_ref_id: &VarRefId,
        flow_id: FlowId,
        query_realm: GmodRealm,
    ) -> Option<&CacheEntry<Arc<LuaType>>> {
        self.get_flow_cache_with_origin(var_ref_id, flow_id, query_realm, FlowOrigin::Real)
    }

    pub fn get_flow_cache_with_origin(
        &mut self,
        var_ref_id: &VarRefId,
        flow_id: FlowId,
        query_realm: GmodRealm,
        origin: FlowOrigin,
    ) -> Option<&CacheEntry<Arc<LuaType>>> {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        self.get_flow_cache_with_origin_by_key(&cache_key, flow_id, query_realm, origin)
    }

    pub fn set_flow_cache(
        &mut self,
        var_ref_id: &VarRefId,
        flow_id: FlowId,
        query_realm: GmodRealm,
        entry: CacheEntry<Arc<LuaType>>,
    ) {
        self.set_flow_cache_with_origin(var_ref_id, flow_id, query_realm, FlowOrigin::Real, entry);
    }

    pub fn set_flow_cache_with_origin(
        &mut self,
        var_ref_id: &VarRefId,
        flow_id: FlowId,
        query_realm: GmodRealm,
        origin: FlowOrigin,
        entry: CacheEntry<Arc<LuaType>>,
    ) {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        self.set_flow_cache_with_origin_by_key(
            &cache_key,
            flow_id,
            query_realm,
            origin,
            entry,
            None,
        );
    }

    #[track_caller]
    pub fn get_flow_cache_with_origin_by_key(
        &mut self,
        cache_key: &VarRefCacheKey,
        flow_id: FlowId,
        query_realm: GmodRealm,
        origin: FlowOrigin,
    ) -> Option<&CacheEntry<Arc<LuaType>>> {
        let lookup = self.lookup();
        let (value, changed) = self.flow_node_cache.get_mut(cache_key)?.get_with_changes(
            &(flow_id, query_realm, origin),
            lookup,
            &self.keyed,
        );
        if changed && let Some(undo) = &mut self.transaction {
            undo.touch_flow(cache_key);
        }
        value
    }

    pub fn set_flow_cache_with_origin_by_key(
        &mut self,
        cache_key: &VarRefCacheKey,
        flow_id: FlowId,
        query_realm: GmodRealm,
        origin: FlowOrigin,
        entry: CacheEntry<Arc<LuaType>>,
        provenance: Option<Provenance>,
    ) {
        if provenance.is_none() {
            self.flow_local_keys.push(cache_key.clone());
        }
        if provenance.is_some() {
            self.touch_flow(cache_key);
        }
        self.capture_new_flow(cache_key);
        self.flow_node_cache
            .entry(cache_key.clone())
            .or_default()
            .insert((flow_id, query_realm, origin), entry, provenance);
    }

    pub(crate) fn set_flow_cache_batch_with_origin_by_key(
        &mut self,
        cache_key: &VarRefCacheKey,
        visited_flow_ids: &[FlowId],
        flow_id: FlowId,
        query_realm: GmodRealm,
        origin: FlowOrigin,
        entry: CacheEntry<Arc<LuaType>>,
        provenance: Option<Provenance>,
    ) {
        if provenance.is_none() {
            self.flow_local_keys.push(cache_key.clone());
        }
        if provenance.is_some() {
            self.touch_flow(cache_key);
        }
        self.capture_new_flow(cache_key);
        let by_flow = self.flow_node_cache.entry(cache_key.clone()).or_default();
        for visited_flow_id in visited_flow_ids {
            by_flow.insert(
                (*visited_flow_id, query_realm, origin),
                entry.clone(),
                provenance.clone(),
            );
        }
        by_flow.insert((flow_id, query_realm, origin), entry, provenance);
    }

    /// Takes an empty flow-walk scratch buffer.
    pub(crate) fn take_flow_walk_scratch(&mut self) -> Vec<FlowId> {
        self.flow_walk_scratch.pop().unwrap_or_default()
    }

    pub(crate) fn return_flow_walk_scratch(&mut self, mut scratch: Vec<FlowId>) {
        scratch.clear();
        self.flow_walk_scratch.push(scratch);
    }

    #[track_caller]
    pub fn get_call_func_cache(
        &mut self,
        syntax_id: LuaSyntaxId,
        args_count: Option<usize>,
        call_expr_type: &LuaType,
    ) -> Option<CacheEntry<Arc<LuaFunctionType>>> {
        let key = (syntax_id, args_count);
        if let Some((_, entry)) = self
            .call_cache_local
            .get(&key)
            .and_then(|bucket| bucket.iter().find(|(ty, _)| ty == call_expr_type))
        {
            derivation::taint();
            return Some(entry.clone());
        }
        let lookup = self.lookup();
        let bucket = self.call_cache.get_mut(&key)?;
        let at = bucket.iter().position(|(ty, _, _)| ty == call_expr_type)?;
        match derivation::reuse(&bucket[at].2, lookup.gates, &self.keyed, lookup.realm) {
            Reuse::Held => Some(bucket[at].1.clone()),
            Reuse::Stale => {
                self.capture_call(&key);
                if let Some(bucket) = self.call_cache.get_mut(&key) {
                    bucket.swap_remove(at);
                }
                None
            }
            Reuse::Elsewhere => None,
        }
    }

    pub fn insert_call_func_cache(
        &mut self,
        syntax_id: LuaSyntaxId,
        args_count: Option<usize>,
        call_expr_type: LuaType,
        entry: CacheEntry<Arc<LuaFunctionType>>,
        provenance: Option<Provenance>,
    ) {
        let key = (syntax_id, args_count);
        let Some(provenance) = provenance else {
            let bucket = self
                .call_cache_local
                .entry(key)
                .or_insert_with(|| Vec::with_capacity(1));
            match bucket.iter_mut().find(|(ty, _)| *ty == call_expr_type) {
                Some(slot) => *slot = (call_expr_type, entry),
                None => bucket.push((call_expr_type, entry)),
            }
            return;
        };
        if let Some(bucket) = self.call_cache_local.get_mut(&key) {
            bucket.retain(|(ty, _)| *ty != call_expr_type);
        }
        self.capture_call(&key);
        // Almost every call site holds one entry: a bucket starts sized for it.
        let bucket = self
            .call_cache
            .entry(key)
            .or_insert_with(|| Vec::with_capacity(1));
        match bucket.iter_mut().find(|(ty, _, _)| *ty == call_expr_type) {
            Some(slot) => *slot = (call_expr_type, entry, provenance),
            None => bucket.push((call_expr_type, entry, provenance)),
        }
    }

    pub fn remove_call_func_cache(
        &mut self,
        syntax_id: LuaSyntaxId,
        args_count: Option<usize>,
        call_expr_type: &LuaType,
    ) {
        let key = (syntax_id, args_count);
        if let Some(bucket) = self.call_cache_local.get_mut(&key) {
            bucket.retain(|(ty, _)| ty != call_expr_type);
        }
        self.capture_call(&key);
        if let Some(bucket) = self.call_cache.get_mut(&key) {
            bucket.retain(|(ty, _, _)| ty != call_expr_type);
            if bucket.is_empty() {
                self.call_cache.remove(&key);
            }
        }
    }

    /// Swaps in the flow entries `var_ref_id` holds walked from `origin`,
    /// returning the ones it held before; see [`Self::unkey_flow_cache`].
    pub(crate) fn key_flow_cache(
        &mut self,
        var_ref_id: &VarRefId,
        origin: &LuaType,
    ) -> Option<FlowEntries> {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        let keyed_key = (cache_key.clone(), origin.clone());
        self.capture_keyed_flow(&keyed_key);
        self.capture_flow(&cache_key);
        let keyed = self.keyed_flow_node_cache.remove(&keyed_key);
        match keyed {
            Some(keyed) => self.flow_node_cache.insert(cache_key, keyed),
            None => self.flow_node_cache.remove(&cache_key),
        }
    }

    /// Keeps the entries walked from `origin` and restores `previous`. The
    /// entries only this evaluation may use stay until it clears them, as
    /// unkeyed walks keep theirs.
    pub(crate) fn unkey_flow_cache(
        &mut self,
        var_ref_id: &VarRefId,
        origin: LuaType,
        previous: Option<FlowEntries>,
    ) {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        self.capture_flow(&cache_key);
        let keyed_key = (cache_key.clone(), origin);
        self.capture_keyed_flow(&keyed_key);
        if let Some(keyed) = self.flow_node_cache.remove(&cache_key) {
            if keyed.has_local() {
                self.keyed_flow_local_keys.push(keyed_key.clone());
            }
            self.keyed_flow_node_cache.insert(keyed_key, keyed);
        }
        if let Some(previous) = previous {
            self.flow_node_cache.insert(cache_key, previous);
        }
    }

    /// Drops the keyed flow entries only this evaluation may use.
    fn clear_keyed_flow_locals(&mut self) {
        for key in self.keyed_flow_local_keys.drain(..) {
            if let Some(entries) = self.keyed_flow_node_cache.get_mut(&key) {
                entries.clear_local();
            }
        }
    }

    /// The origin walks of `cache_key` start from, when they are keyed by one:
    /// an index ref's member type or a `self` seed.
    pub(crate) fn keyed_origin(&self, cache_key: &VarRefCacheKey) -> Option<&LuaType> {
        if let Some((CacheEntry::Cache(origin), _)) =
            self.index_ref_origin_type_cache.get(cache_key)
        {
            return Some(origin);
        }
        self.self_base_seed
            .as_ref()
            .filter(|(seed_ref, _)| VarRefCacheKey::from(seed_ref) == *cache_key)
            .map(|(_, seed)| seed)
    }

    /// The context key flow walks of `cache_key` absorb reads of.
    pub(crate) fn flow_context_key(cache_key: &VarRefCacheKey) -> u64 {
        use std::hash::BuildHasher;
        rustc_hash::FxBuildHasher.hash_one(cache_key) | 1
    }

    /// The context key of whether an implicit `self` declared at `decl_id`
    /// is seeded.
    pub(crate) fn self_decl_context_key(decl_id: LuaDeclId) -> u64 {
        use std::hash::BuildHasher;
        rustc_hash::FxBuildHasher.hash_one((u8::MAX, decl_id)) | 1
    }

    pub fn take_flow_cache_for_var_ref(&mut self, var_ref_id: &VarRefId) -> Option<FlowEntries> {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        self.capture_flow(&cache_key);
        self.flow_node_cache.remove(&cache_key)
    }

    pub fn restore_flow_cache_for_var_ref(
        &mut self,
        var_ref_id: &VarRefId,
        previous: Option<FlowEntries>,
    ) {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        self.capture_flow(&cache_key);
        if let Some(previous) = previous {
            self.flow_node_cache.insert(cache_key, previous);
        } else {
            self.flow_node_cache.remove(&cache_key);
        }
    }

    #[track_caller]
    pub fn get_index_ref_origin_type_cache(
        &self,
        var_ref_id: &VarRefId,
    ) -> Option<&CacheEntry<LuaType>> {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        let Some((entry, mark)) = self.index_ref_origin_type_cache.get(&cache_key) else {
            // No origin is itself an answer: a walk from inside a setter's
            // scope would have found one.
            derivation::consult_unkeyed(Self::flow_context_key(&cache_key));
            return None;
        };
        // The walks of this reference are keyed by its origin.
        derivation::consult_absorbed(Self::flow_context_key(&cache_key), *mark);
        Some(entry)
    }

    pub fn set_index_ref_origin_type_cache(
        &mut self,
        var_ref_id: &VarRefId,
        entry: CacheEntry<LuaType>,
    ) {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        let key = Self::flow_context_key(&cache_key);
        if self
            .index_ref_origin_type_cache
            .insert(cache_key, (entry, derivation::scope_mark()))
            .is_none()
        {
            self.keyed.push(key);
        }
    }

    pub fn replace_index_ref_origin_type_cache(
        &mut self,
        var_ref_id: &VarRefId,
        entry: CacheEntry<LuaType>,
    ) -> Option<(CacheEntry<LuaType>, derivation::ScopeMark)> {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        let key = Self::flow_context_key(&cache_key);
        let previous = self
            .index_ref_origin_type_cache
            .insert(cache_key, (entry, derivation::scope_mark()));
        if previous.is_none() {
            self.keyed.push(key);
        }
        previous
    }

    pub fn restore_index_ref_origin_type_cache(
        &mut self,
        var_ref_id: &VarRefId,
        previous: Option<(CacheEntry<LuaType>, derivation::ScopeMark)>,
    ) {
        let cache_key = VarRefCacheKey::from(var_ref_id);
        if let Some(previous) = previous {
            self.index_ref_origin_type_cache.insert(cache_key, previous);
        } else if self
            .index_ref_origin_type_cache
            .remove(&cache_key)
            .is_some()
        {
            self.unkey(std::iter::once(Self::flow_context_key(&cache_key)));
        }
    }

    pub fn flow_cache_entry_count(&self) -> usize {
        self.flow_node_cache
            .values()
            .map(|entries| entries.len())
            .sum()
    }
}

fn var_ref_is_phase_stable(db: &DbIndex, syntax_id: &LuaSyntaxId, var_ref_id: &VarRefId) -> bool {
    let local_decl_is_stable = |decl_id: &LuaDeclId| {
        db.get_decl_index()
            .get_decl(decl_id)
            .is_some_and(|decl| decl.is_local())
    };

    match var_ref_id {
        VarRefId::VarRef(decl_id) => local_decl_is_stable(decl_id),
        VarRefId::IndexRef(VarRefRootId::Decl(decl_id), _) => {
            // A call annotated as rawget can also produce an IndexRef rooted at
            // a local, but that identity depends on its resolved signature.
            // Only a syntactic index expression is structural across unresolve.
            syntax_id.get_kind() == glua_parser::LuaSyntaxKind::IndexExpr
                && local_decl_is_stable(decl_id)
        }
        VarRefId::SelfRef(_)
        | VarRefId::IndexRef(VarRefRootId::Member(_) | VarRefRootId::SelfRef(_), _)
        | VarRefId::GlobalName(_, _) => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use glua_parser::{LuaSyntaxId, LuaSyntaxKind};
    use internment::ArcIntern;
    use rowan::{TextRange, TextSize};
    use smol_str::SmolStr;

    use super::{CacheEntry, FlowOrigin, LuaInferCache, VarRefCacheKey};
    use crate::{
        DbIndex, FileId, FlowId, GmodRealm, InferFailReason, LuaDecl, LuaDeclExtra, LuaDeclId,
        LuaDeclarationTree, LuaType, VarRefId, VarRefRootId,
    };

    #[test]
    fn clear_for_unresolve_preserves_only_structural_lookup_caches() {
        let file_id = FileId { id: 7 };
        let syntax_id = LuaSyntaxId::new(
            LuaSyntaxKind::NameExpr.into(),
            TextRange::new(TextSize::from(3), TextSize::from(4)),
        );
        let decl_id = LuaDeclId::new(file_id, TextSize::from(3));
        let mut cache = LuaInferCache::new(file_id, Default::default());

        cache
            .expr_cache
            .insert(syntax_id, CacheEntry::Cache(LuaType::Integer), None);
        cache
            .expr_var_ref_id_cache
            .insert(syntax_id, VarRefId::VarRef(decl_id), None);
        cache
            .flow_node_realm_cache
            .insert(FlowId(1), GmodRealm::Server, None);
        cache
            .field_literal_narrowing
            .insert(syntax_id, crate::db_index::derivation::ScopeMark::default());

        cache.clear_for_unresolve(&DbIndex::new());

        assert!(cache.expr_cache.is_empty());
        assert!(cache.expr_var_ref_id_cache.is_empty());
        assert_eq!(
            cache.flow_node_realm_cache.peek(&FlowId(1)),
            Some(&GmodRealm::Server)
        );
        assert!(cache.field_literal_narrowing.is_empty());
    }

    #[test]
    fn clear_discards_literal_narrowing_stop_markers() {
        let file_id = FileId { id: 7 };
        let syntax_id = LuaSyntaxId::new(
            LuaSyntaxKind::IndexExpr.into(),
            TextRange::new(TextSize::from(3), TextSize::from(9)),
        );
        let mut cache = LuaInferCache::new(file_id, Default::default());
        cache
            .field_literal_narrowing
            .insert(syntax_id, crate::db_index::derivation::ScopeMark::default());

        cache.clear();

        assert!(cache.field_literal_narrowing.is_empty());
    }

    #[test]
    fn clear_for_unresolve_discards_semantic_call_expression_var_refs() {
        let file_id = FileId { id: 7 };
        let local_range = TextRange::new(TextSize::from(3), TextSize::from(4));
        let local_decl = LuaDecl::new(
            "value",
            file_id,
            local_range,
            LuaDeclExtra::Local {
                kind: LuaSyntaxKind::LocalName.into(),
                attrib: None,
            },
            None,
        );
        let decl_id = local_decl.get_id();
        let mut decl_tree = LuaDeclarationTree::new(file_id);
        decl_tree.add_decl(local_decl);
        let mut db = DbIndex::new();
        db.get_decl_index_mut().add_decl_tree(decl_tree);

        let call_expr_id = LuaSyntaxId::new(
            LuaSyntaxKind::CallExpr.into(),
            TextRange::new(TextSize::from(10), TextSize::from(20)),
        );
        let index_expr_id = LuaSyntaxId::new(
            LuaSyntaxKind::IndexExpr.into(),
            TextRange::new(TextSize::from(21), TextSize::from(30)),
        );
        let index_ref = VarRefId::IndexRef(
            VarRefRootId::Decl(decl_id),
            ArcIntern::new(SmolStr::new("value.key")),
        );
        let mut cache = LuaInferCache::new(file_id, Default::default());
        cache
            .expr_var_ref_id_cache
            .insert(call_expr_id, index_ref.clone(), None);
        cache
            .expr_var_ref_id_cache
            .insert(index_expr_id, index_ref.clone(), None);

        cache.clear_for_unresolve(&db);

        assert!(cache.expr_var_ref_id_cache.peek(&call_expr_id).is_none());
        assert_eq!(
            cache.expr_var_ref_id_cache.peek(&index_expr_id),
            Some(&index_ref)
        );
    }

    #[test]
    fn flow_cache_batch_backfills_success_entries() {
        let file_id = FileId { id: 7 };
        let mut cache = LuaInferCache::new(file_id, Default::default());
        let cache_key = VarRefCacheKey::GlobalName(ArcIntern::new(SmolStr::new("value")));
        let query_realm = GmodRealm::Shared;

        cache.set_flow_cache_batch_with_origin_by_key(
            &cache_key,
            &[],
            FlowId(1),
            query_realm,
            FlowOrigin::Real,
            CacheEntry::Cache(Arc::new(LuaType::String)),
            None,
        );
        assert!(matches!(
            cache.get_flow_cache_with_origin_by_key(
                &cache_key,
                FlowId(1),
                query_realm,
                FlowOrigin::Real
            ),
            Some(CacheEntry::Cache(narrow_type)) if **narrow_type == LuaType::String
        ));

        cache.set_flow_cache_batch_with_origin_by_key(
            &cache_key,
            &[FlowId(2), FlowId(3)],
            FlowId(4),
            query_realm,
            FlowOrigin::Real,
            CacheEntry::Cache(Arc::new(LuaType::Number)),
            None,
        );
        for flow_id in [FlowId(2), FlowId(3), FlowId(4)] {
            assert!(matches!(
                cache.get_flow_cache_with_origin_by_key(
                    &cache_key,
                    flow_id,
                    query_realm,
                    FlowOrigin::Real
                ),
                Some(CacheEntry::Cache(narrow_type)) if **narrow_type == LuaType::Number
            ));
        }

        cache.set_flow_cache_batch_with_origin_by_key(
            &cache_key,
            &[FlowId(5), FlowId(6)],
            FlowId(5),
            query_realm,
            FlowOrigin::Real,
            CacheEntry::Cache(Arc::new(LuaType::Integer)),
            None,
        );
        for flow_id in [FlowId(5), FlowId(6)] {
            assert!(matches!(
                cache.get_flow_cache_with_origin_by_key(
                    &cache_key,
                    flow_id,
                    query_realm,
                    FlowOrigin::Real
                ),
                Some(CacheEntry::Cache(narrow_type)) if **narrow_type == LuaType::Integer
            ));
        }
    }

    #[test]
    fn flow_cache_batch_preserves_error_entries() {
        let file_id = FileId { id: 7 };
        let mut cache = LuaInferCache::new(file_id, Default::default());
        let cache_key = VarRefCacheKey::GlobalName(ArcIntern::new(SmolStr::new("value")));
        let query_realm = GmodRealm::Shared;

        cache.set_flow_cache_batch_with_origin_by_key(
            &cache_key,
            &[],
            FlowId(1),
            query_realm,
            FlowOrigin::Real,
            CacheEntry::error(InferFailReason::None),
            None,
        );
        assert!(matches!(
            cache.get_flow_cache_with_origin_by_key(
                &cache_key,
                FlowId(1),
                query_realm,
                FlowOrigin::Real
            ),
            Some(CacheEntry::Error(reason)) if **reason == InferFailReason::None
        ));

        cache.set_flow_cache_batch_with_origin_by_key(
            &cache_key,
            &[FlowId(2), FlowId(3)],
            FlowId(2),
            query_realm,
            FlowOrigin::Real,
            CacheEntry::error(InferFailReason::FieldNotFound),
            None,
        );
        for flow_id in [FlowId(2), FlowId(3)] {
            assert!(matches!(
                cache.get_flow_cache_with_origin_by_key(
                    &cache_key,
                    flow_id,
                    query_realm,
                    FlowOrigin::Real
                ),
                Some(CacheEntry::Error(reason)) if **reason == InferFailReason::FieldNotFound
            ));
        }
    }

    /// A flow entry is two words whichever answer it holds, so a flow walk's
    /// entries stay small.
    #[test]
    fn flow_cache_entries_stay_two_words() {
        assert_eq!(
            std::mem::size_of::<CacheEntry<Arc<LuaType>>>(),
            2 * std::mem::size_of::<usize>()
        );
    }
}
