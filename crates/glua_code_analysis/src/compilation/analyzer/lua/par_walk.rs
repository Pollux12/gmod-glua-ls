//! Intra-level parallelization of the lua analyze walk.
//!
//! Files in one dependency level are independent under the serial walk's
//! ordering — a file only observes writes from files earlier in the level —
//! so the level is split into contiguous chunks, each walked on a forked
//! [`DbIndex`] by a worker. The walk itself runs unchanged against the fork:
//! its own writes are immediately visible (a local bound then inferred reads
//! the bind), which per-file speculation cannot provide.
//!
//! Exactness is enforced at the merge, not assumed:
//!
//! - **Writes** travel as replayable [`op_log`] ops plus boundary replaces
//!   for objects borrowed through `get_*_mut` (`MutRef`). Replaying the real
//!   call in file order reproduces the serial merge by construction.
//! - **Reads** are recorded in an [`InferenceReadSet`]; a file whose reads
//!   intersect writes it could not have seen (earlier files outside its
//!   chunk prefix, or a chunk-mate whose outcome was discarded and re-run)
//!   is re-run serially on the canonical index.
//! - **Untracked reads** (`untracked_reads` bits) stale against the matching
//!   earlier-write bits; a mutable borrow escaping instrumentation shows up
//!   as a touched domain with no logged op and also re-runs the file.
//! - **Order conflicts** between a `MutRef` object's boundary replace and a
//!   later op touching the same slot re-run the file — the replace lands
//!   after all ops, so such an op would see pre-poke state the serial walk
//!   saw post-poke.
//! - **Context deltas** drain per file as `Send` descriptors
//!   (`unresolve::spec::ItemDesc`, syntax ids) and replay on the canonical
//!   `AnalyzeContext` in file order; an unmodeled context write is caught by
//!   [`AnalyzeContext::walk_delta_residual_nonempty`] and re-runs the file.
//! - **Infer caches** are seeded from a clone of the canonical per-file
//!   cache, walked on the worker, and installed back wholesale, so memo
//!   entries and side-effect collections match the serial walk exactly.

use std::time::{Duration, Instant};

use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use glua_parser::{LuaAst, LuaAstNode, with_syntax_resolver};

use crate::{
    FileId, LuaDeclId, LuaInferCache, LuaMemberId, LuaSignatureId, LuaTypeDeclId,
    compilation::analyzer::{
        AnalyzeContext, WalkCtxDelta, common::estimate_file_index_entries, parallel,
    },
    db_index::{
        DbIndex, LuaDecl, LuaDeclarationTree, LuaMember, LuaSignature, LuaTypeDecl, ModuleInfo,
        op_log::{self, Domain, LoggedOp, MutRef, MutRefNote, UntrackedDomain, untracked_bit},
        read_set::{self, InferenceReadSet, WriteKey},
    },
    profile::Profile,
};

use super::{
    LuaAnalyzeProfile, LuaAnalyzer, analyze_node, call::SpecialCallDirectMatcher,
    lua_ast_profile_kind, module::analyze_chunk_return,
    stats::flush_pending_dynamic_key_collection_widenings,
};

/// Smallest level worth parallelizing: below this the fork/read-set overhead
/// buys no parallelism (a single chunk is serial work plus bookkeeping).
pub(in crate::compilation::analyzer) const PARALLEL_MIN_LEVEL: usize = 16;

/// Files per speculative wave. A worker's fork sees the level-start state
/// plus earlier waves' merged writes; a read can only stale on writes made
/// inside its own wave, so bounding the wave bounds the unseen prefix the
/// merge's staleness check can trip on. Smaller waves rerun less but pay
/// more dispatch rounds.
const WAVE_FILES_PER_WORKER: usize = 8;

/// One file's walk outcome on a worker: replayable writes, boundary
/// replacements, read/touch evidence for staleness, context deltas, and the
/// file's infer cache. Discarded wholesale when the merge re-runs the file.
struct WalkOutcome {
    file_id: FileId,
    /// Wall time the worker spent inside this file's walk — reported through
    /// the same slow-file/level-shape channels as the serial path.
    elapsed: Duration,
    /// Node-kind profile when `GLUALS_PROFILE` is on — merged into the
    /// workspace profile like the serial path's per-file profile.
    profile: Option<LuaAnalyzeProfile>,
    ops: Vec<LoggedOp>,
    mut_refs: Vec<MutRefNote>,
    /// `(splice ordinal, replacement)` — the replace lands in the op stream
    /// just before the op at its ordinal (the object's last borrow point).
    replacements: Vec<(usize, MutReplacement)>,
    untracked_reads: u32,
    touched: u32,
    /// First `get_*_mut` caller per domain bit — stats-only diagnostics.
    touch_sites: Vec<(u8, &'static std::panic::Location<'static>)>,
    /// The file's recorded index reads, probed field-wise against the
    /// canonical/clean/rerun write sets at merge — no key materialization.
    reads: InferenceReadSet,
    write_keys: FxHashSet<WriteKey>,
    write_bits: u32,
    ctx: WalkCtxDelta,
    residual_ctx: bool,
    /// Names of the non-empty residual collections — diagnostics only.
    residual_fields: Vec<&'static str>,
    infer_cache: Option<LuaInferCache>,
}

/// Per-file merge report handed back to the pipeline: timing plus the
/// optional node profile, in level order — the same accounting the serial
/// loop produces.
pub(in crate::compilation::analyzer) struct WalkFileReport {
    pub file_id: FileId,
    pub elapsed: Duration,
    pub profile: Option<LuaAnalyzeProfile>,
}

/// The fork's final value of an object the walk borrowed mutably, installed
/// on the canonical index after the file's ops replay.
enum MutReplacement {
    Signature(LuaSignatureId, LuaSignature),
    Member(LuaMemberId, LuaMember),
    TypeDecl(LuaTypeDeclId, LuaTypeDecl),
    Decl(LuaDeclId, LuaDecl),
    DeclTree(LuaDeclarationTree),
    Module(FileId, ModuleInfo),
}

/// The canonical/clean/rerun write sets, bucketed by key variant so a file's
/// `InferenceReadSet` probes typed sets directly — intersecting 15k reads
/// against a flat `WriteKey` set materialized a fresh enum key per probe.
#[derive(Default)]
struct WriteKeySet {
    type_owners: FxHashSet<crate::LuaTypeOwner>,
    member_owners: FxHashSet<crate::LuaMemberOwner>,
    member_keys: FxHashSet<crate::LuaMemberKey>,
    member_slots: FxHashSet<(crate::LuaMemberOwner, crate::LuaMemberKey)>,
    signatures: FxHashSet<LuaSignatureId>,
    params: FxHashSet<LuaSignatureId>,
    modules: FxHashSet<FileId>,
    special_calls: FxHashSet<(FileId, rowan::TextSize)>,
    contribs: FxHashSet<(LuaSignatureId, usize)>,
    dynamic_fields: FxHashSet<(crate::DynamicFieldOwner, Option<smol_str::SmolStr>)>,
    type_decls: FxHashSet<LuaTypeDeclId>,
    decl_miscs: FxHashSet<LuaDeclId>,
    definition_facts: FxHashSet<crate::LuaDefinitionId>,
    global_paths: FxHashSet<crate::GlobalId>,
    operator_owners: FxHashSet<crate::LuaOperatorOwner>,
    metatable_sites: FxHashSet<crate::InFiled<rowan::TextRange>>,
    definition_sites: FxHashSet<crate::InFiled<rowan::TextRange>>,
    gmod_metas: FxHashSet<smol_str::SmolStr>,
    member_ids: FxHashSet<LuaMemberId>,
    unattributed_fields: FxHashSet<smol_str::SmolStr>,
    file_scopeds: FxHashSet<FileId>,
    helper_caches: FxHashSet<FileId>,
    type_decl_scan: bool,
    helper_registry: bool,
}

impl WriteKeySet {
    fn insert(&mut self, key: WriteKey) {
        match key {
            WriteKey::TypeOwner(owner) => {
                self.type_owners.insert(owner);
            }
            WriteKey::MemberOwner(owner) => {
                self.member_owners.insert(owner);
            }
            WriteKey::MemberKey(key) => {
                self.member_keys.insert(key);
            }
            WriteKey::MemberSlot(owner, key) => {
                self.member_slots.insert((owner, key));
            }
            WriteKey::Signature(id) => {
                self.signatures.insert(id);
            }
            WriteKey::Param(id) => {
                self.params.insert(id);
            }
            WriteKey::Module(file) => {
                self.modules.insert(file);
            }
            WriteKey::SpecialCall(file, pos) => {
                self.special_calls.insert((file, pos));
            }
            WriteKey::Contrib(sig, param) => {
                self.contribs.insert((sig, param));
            }
            WriteKey::DynamicField(owner, field) => {
                self.dynamic_fields.insert((owner, field));
            }
            WriteKey::TypeDecl(id) => {
                self.type_decls.insert(id);
            }
            WriteKey::DeclMisc(id) => {
                self.decl_miscs.insert(id);
            }
            WriteKey::DefinitionFact(id) => {
                self.definition_facts.insert(id);
            }
            WriteKey::GlobalPath(id) => {
                self.global_paths.insert(id);
            }
            WriteKey::OperatorOwner(owner) => {
                self.operator_owners.insert(owner);
            }
            WriteKey::Metatable(site) => {
                self.metatable_sites.insert(site);
            }
            WriteKey::GmodMeta(name) => {
                self.gmod_metas.insert(name);
            }
            WriteKey::MemberId(id) => {
                self.member_ids.insert(id);
            }
            WriteKey::TypeDeclScan => self.type_decl_scan = true,
            WriteKey::DefinitionSite(site) => {
                self.definition_sites.insert(site);
            }
            WriteKey::UnattributedField(name) => {
                self.unattributed_fields.insert(name);
            }
            WriteKey::FileScoped(file) => {
                self.file_scopeds.insert(file);
            }
            WriteKey::HelperCache(file) => {
                self.helper_caches.insert(file);
            }
            WriteKey::HelperRegistry => self.helper_registry = true,
        }
    }

    fn extend_keys(&mut self, keys: impl IntoIterator<Item = WriteKey>) {
        for key in keys {
            self.insert(key);
        }
    }
}

/// The merge's seen/unseen question over read-slot keys: a slot was written
/// canonically but the file's fork never covered it (not a clean chunk-mate's
/// write), or a rerun mate's speculative write it saw no longer stands.
#[inline]
fn keys_unseen<'a, T: Eq + std::hash::Hash + 'a>(
    mut keys: impl Iterator<Item = &'a T>,
    canon: &FxHashSet<T>,
    clean: &FxHashSet<T>,
    rerun: &FxHashSet<T>,
) -> bool {
    keys.any(|key| (canon.contains(key) && !clean.contains(key)) || rerun.contains(key))
}

/// Whether the file's recorded reads hit a canonical write its fork never
/// saw — the field-wise equivalent of intersecting flat `WriteKey` sets.
fn reads_stale(
    reads: &InferenceReadSet,
    canon: &WriteKeySet,
    clean: &WriteKeySet,
    rerun: &WriteKeySet,
) -> bool {
    macro_rules! unseen {
        ($field:ident) => {
            keys_unseen(
                reads.$field.iter(),
                &canon.$field,
                &clean.$field,
                &rerun.$field,
            )
        };
    }
    macro_rules! unseen_map {
        ($field:ident) => {
            keys_unseen(
                reads.$field.keys(),
                &canon.$field,
                &clean.$field,
                &rerun.$field,
            )
        };
        ($read_field:ident, $write_field:ident) => {
            keys_unseen(
                reads.$read_field.keys(),
                &canon.$write_field,
                &clean.$write_field,
                &rerun.$write_field,
            )
        };
    }
    unseen!(type_owners)
        || keys_unseen(
            reads.member_maps.keys(),
            &canon.member_owners,
            &clean.member_owners,
            &rerun.member_owners,
        )
        || keys_unseen(
            reads.member_keys.keys(),
            &canon.member_keys,
            &clean.member_keys,
            &rerun.member_keys,
        )
        || keys_unseen(
            reads.missing_member_slots.keys(),
            &canon.member_slots,
            &clean.member_slots,
            &rerun.member_slots,
        )
        || unseen!(signatures)
        || unseen!(params)
        || unseen!(modules)
        || unseen!(special_calls)
        || unseen!(contribs)
        || unseen!(dynamic_fields)
        || keys_unseen(
            reads.type_decls.keys().map(|(decl_id, _)| decl_id),
            &canon.type_decls,
            &clean.type_decls,
            &rerun.type_decls,
        )
        || keys_unseen(
            reads.bound_decls.keys().map(|(type_id, _)| type_id),
            &canon.type_decls,
            &clean.type_decls,
            &rerun.type_decls,
        )
        || unseen_map!(vgui_type_decls, type_decls)
        || unseen_map!(decl_miscs)
        || unseen_map!(decl_claims, decl_miscs)
        || unseen_map!(decl_defaults, decl_miscs)
        || unseen_map!(definition_facts)
        || unseen_map!(global_paths)
        || unseen_map!(operator_owners)
        || unseen_map!(metatable_sites)
        || unseen_map!(definition_sites)
        || unseen!(gmod_metas)
        || keys_unseen(
            reads.member_ids.keys(),
            &canon.member_ids,
            &clean.member_ids,
            &rerun.member_ids,
        )
        || keys_unseen(
            reads.file_scopeds.keys().map(|(file_id, _)| file_id),
            &canon.file_scopeds,
            &clean.file_scopeds,
            &rerun.file_scopeds,
        )
        // A whole-map decl scan is stale when the canonical index holds a decl
        // the scan never saw, or the scan saw a rerun mate's speculative decl
        // that its serial replay did not produce.
        || (reads.type_decl_scan
            && (canon
                .type_decls
                .iter()
                .any(|id| !clean.type_decls.contains(id))
                || rerun
                    .type_decls
                    .iter()
                    .any(|id| !canon.type_decls.contains(id))))
}

/// Per-variant count of staling read slots — stats only, mirrors `reads_stale`
/// field by field so the rerun report can name the driver domains.
fn count_stale_reads(
    reads: &InferenceReadSet,
    canon: &WriteKeySet,
    clean: &WriteKeySet,
    rerun: &WriteKeySet,
    counts: &mut FxHashMap<&'static str, usize>,
) {
    macro_rules! count {
        ($name:literal, $read:expr, $field:ident) => {{
            let n = $read
                .filter(|key| {
                    (canon.$field.contains(*key) && !clean.$field.contains(*key))
                        || rerun.$field.contains(*key)
                })
                .count();
            if n > 0 {
                *counts.entry($name).or_insert(0) += n;
            }
        }};
    }
    count!("TypeOwner", reads.type_owners.iter(), type_owners);
    count!("MemberOwner", reads.member_maps.keys(), member_owners);
    count!("MemberKey", reads.member_keys.keys(), member_keys);
    count!(
        "MemberSlot",
        reads.missing_member_slots.keys(),
        member_slots
    );
    count!("Signature", reads.signatures.iter(), signatures);
    count!("Param", reads.params.iter(), params);
    count!("Module", reads.modules.iter(), modules);
    count!("SpecialCall", reads.special_calls.iter(), special_calls);
    count!("Contrib", reads.contribs.iter(), contribs);
    count!("DynamicField", reads.dynamic_fields.iter(), dynamic_fields);
    count!(
        "TypeDecl",
        reads.type_decls.keys().map(|(decl_id, _)| decl_id),
        type_decls
    );
    count!(
        "BoundDecl",
        reads.bound_decls.keys().map(|(type_id, _)| type_id),
        type_decls
    );
    count!("VguiTypeDecl", reads.vgui_type_decls.keys(), type_decls);
    count!("DeclMisc", reads.decl_miscs.keys(), decl_miscs);
    count!("DeclClaim", reads.decl_claims.keys(), decl_miscs);
    count!("DeclDefault", reads.decl_defaults.keys(), decl_miscs);
    count!(
        "DefinitionFact",
        reads.definition_facts.keys(),
        definition_facts
    );
    count!("GlobalPath", reads.global_paths.keys(), global_paths);
    count!(
        "OperatorOwner",
        reads.operator_owners.keys(),
        operator_owners
    );
    count!("Metatable", reads.metatable_sites.keys(), metatable_sites);
    count!(
        "DefinitionSite",
        reads.definition_sites.keys(),
        definition_sites
    );
    count!("GmodMeta", reads.gmod_metas.iter(), gmod_metas);
    count!("MemberId", reads.member_ids.keys(), member_ids);
    count!(
        "FileScoped",
        reads.file_scopeds.keys().map(|(file_id, _)| file_id),
        file_scopeds
    );
    if reads.type_decl_scan
        && (canon
            .type_decls
            .iter()
            .any(|id| !clean.type_decls.contains(id))
            || rerun
                .type_decls
                .iter()
                .any(|id| !canon.type_decls.contains(id)))
    {
        *counts.entry("TypeDeclScan").or_insert(0) += 1;
    }
}

impl WalkOutcome {
    /// Domain bits the file took a `&mut` handle on, covered by a logged op
    /// or a boundary replace. Any `touched` bit outside this mask marks a
    /// write path that never reached the log — uninstrumented, so the file
    /// cannot be merged and is re-run serially.
    fn covered_domain_bits(&self) -> u32 {
        let mut bits = 0u32;
        for op in &self.ops {
            bits |= op.domain.bit();
        }
        for note in &self.mut_refs {
            bits |= note.reference.domain().bit();
        }
        bits
    }

    /// Whether a logged op runs strictly between an object's first and last
    /// mutable borrow and touches a slot that could observe the poke — the
    /// one window where no single replacement position can reproduce the
    /// intermediate state the op saw serially.
    fn mutref_order_conflict(&self) -> bool {
        let mut spans: FxHashMap<MutRef, (usize, usize)> = FxHashMap::default();
        for note in &self.mut_refs {
            spans
                .entry(note.reference.clone())
                .and_modify(|(lo, hi)| {
                    *lo = (*lo).min(note.ordinal);
                    *hi = (*hi).max(note.ordinal);
                })
                .or_insert((note.ordinal, note.ordinal));
        }
        spans.iter().any(|(reference, &(lo, hi))| {
            let touch = reference.touch_keys();
            self.ops
                .iter()
                .skip(lo)
                .take(hi.saturating_sub(lo))
                .any(|op| op.keys.iter().any(|key| touch.contains(key)))
        })
    }
}

/// The untracked-domain bits a write set dirties: direct domain writes for
/// the coarsely-read domains plus key-tagged writes into the aux maps those
/// bits guard.
fn untracked_bits_for(domain: Domain, keys: &[WriteKey]) -> u32 {
    let mut bits = match domain {
        Domain::Operator => untracked_bit(UntrackedDomain::Operator),
        Domain::Metatable => untracked_bit(UntrackedDomain::Metatable),
        Domain::GmodInfer => untracked_bit(UntrackedDomain::GmodInfer),
        Domain::GmodClass => untracked_bit(UntrackedDomain::GmodClass),
        Domain::Property => untracked_bit(UntrackedDomain::Property),
        _ => 0,
    };
    for key in keys {
        bits |= match key {
            WriteKey::DeclMisc(_) => untracked_bit(UntrackedDomain::DeclMisc),
            WriteKey::GlobalPath(_) | WriteKey::DefinitionSite(_) => {
                untracked_bit(UntrackedDomain::MemberGlobal)
            }
            WriteKey::Signature(_) => untracked_bit(UntrackedDomain::SignatureAux),
            // `GmodMeta` keys both the signature receiver-out-param name maps
            // and the gmod-class name-keyed caches (vgui panels, derma skins).
            WriteKey::GmodMeta(_) => {
                untracked_bit(UntrackedDomain::SignatureAux)
                    | untracked_bit(UntrackedDomain::GmodClass)
            }
            WriteKey::DefinitionFact(_) => untracked_bit(UntrackedDomain::DefinitionFact),
            WriteKey::HelperCache(_) | WriteKey::HelperRegistry => {
                untracked_bit(UntrackedDomain::HelperCache)
            }
            _ => 0,
        };
    }
    bits
}

/// `bit` of every untracked-read domain a file's writes could have changed —
/// the write-side counterpart of `untracked_reads`.
fn write_untracked_bits(ops: &[LoggedOp], notes: &[MutRefNote]) -> u32 {
    let mut bits = 0;
    for op in ops {
        bits |= untracked_bits_for(op.domain, &op.keys);
    }
    for note in notes {
        bits |= untracked_bits_for(note.reference.domain(), &note.reference.keys());
    }
    bits
}

fn collect_write_keys(ops: &[LoggedOp], notes: &[MutRefNote]) -> FxHashSet<WriteKey> {
    let mut keys = FxHashSet::default();
    for op in ops {
        keys.extend(op.keys.iter().cloned());
    }
    for note in notes {
        keys.extend(note.reference.keys().into_vec());
    }
    keys
}

/// Splits `level` into contiguous byte-balanced chunks, at most `max_chunks`.
/// Contiguity is load-bearing: a file's fork sees exactly its earlier
/// chunk-mates' writes, which must also be earlier in level order for the
/// merge's seen-set bookkeeping to hold.
fn partition_level(db: &DbIndex, level: &[FileId], max_chunks: usize) -> Vec<Vec<FileId>> {
    let vfs = db.get_vfs();
    let weights: Vec<usize> = level
        .iter()
        .map(|file_id| vfs.get_file_content(file_id).map_or(0, |text| text.len()))
        .collect();
    let total: usize = weights.iter().sum();
    let target = (total / max_chunks.max(1)).max(1);
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut accrued = 0usize;
    for (idx, &file_id) in level.iter().enumerate() {
        if accrued >= target && chunks.len() + 1 < max_chunks {
            chunks.push(std::mem::take(&mut current));
            accrued = 0;
        }
        accrued += weights[idx];
        current.push(file_id);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// The shared per-file walk — identical to the serial path's body. Runs on
/// the worker against `&mut fork` and on the merge thread for a serial
/// re-run against `&mut DbIndex`; `false` mirrors the serial skip for a
/// file whose syntax tree is absent.
fn walk_one(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    file_id: FileId,
    is_scripted: bool,
    special_call_direct_matcher: &SpecialCallDirectMatcher,
    gmod_enabled: bool,
    node_profile_enabled: bool,
) -> (bool, Option<LuaAnalyzeProfile>) {
    let Some(chunk) = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_chunk_node())
    else {
        return (false, None);
    };
    let estimate = estimate_file_index_entries(usize::from(chunk.syntax().text_range().len()));
    context
        .infer_manager
        .get_infer_cache(file_id)
        .reserve_inference_maps(estimate);
    let mut analyzer = LuaAnalyzer::new(
        db,
        file_id,
        context,
        gmod_enabled,
        is_scripted,
        special_call_direct_matcher,
    );
    let mut profile = node_profile_enabled.then(LuaAnalyzeProfile::default);
    with_syntax_resolver(chunk.syntax(), || {
        for node in chunk.descendants::<LuaAst>() {
            if let Some(profile) = profile.as_mut() {
                let kind = lua_ast_profile_kind(&node);
                let node_start = Instant::now();
                analyze_node(&mut analyzer, node);
                profile.record(kind, node_start.elapsed());
            } else {
                analyze_node(&mut analyzer, node);
            }
        }
        analyze_chunk_return(&mut analyzer, chunk.clone());
        flush_pending_dynamic_key_collection_widenings(&mut analyzer);
    });
    (true, profile)
}

/// Extracts the fork's final object for every mutably-borrowed slot, deduped
/// by object and tagged with the op-stream index its replace splices in
/// before — the position of the object's last borrow. An absent object
/// (removed mid-walk) drops its replace.
fn extract_replacements(fork: &DbIndex, notes: &[MutRefNote]) -> Vec<(usize, MutReplacement)> {
    let mut lasts: FxHashMap<MutRef, usize> = FxHashMap::default();
    for note in notes {
        lasts
            .entry(note.reference.clone())
            .and_modify(|ord| *ord = (*ord).max(note.ordinal))
            .or_insert(note.ordinal);
    }
    let mut seen = FxHashSet::default();
    let mut replacements = Vec::new();
    for note in notes {
        if !seen.insert(note.reference.clone()) {
            continue;
        }
        let ordinal = lasts[&note.reference];
        let replacement = match &note.reference {
            MutRef::Signature(id) => fork
                .get_signature_index()
                .get(id)
                .map(|signature| MutReplacement::Signature(*id, signature.clone())),
            MutRef::Member(id) => fork
                .get_member_index()
                .get_member(id)
                .map(|member| MutReplacement::Member(*id, member.clone())),
            MutRef::TypeDecl(id) => fork
                .get_type_index()
                .get_type_decl(id)
                .map(|decl| MutReplacement::TypeDecl(id.clone(), decl.clone())),
            MutRef::Decl(id) => fork
                .get_decl_index()
                .get_decl(id)
                .map(|decl| MutReplacement::Decl(*id, decl.clone())),
            MutRef::DeclTree(file_id) => fork
                .get_decl_index()
                .get_decl_tree(file_id)
                .map(|tree| MutReplacement::DeclTree(tree.clone())),
            MutRef::Module(file_id) => fork
                .get_module_index()
                .get_module(*file_id)
                .map(|module| MutReplacement::Module(*file_id, module.clone())),
        };
        if let Some(replacement) = replacement {
            replacements.push((ordinal, replacement));
        }
    }
    replacements
}

/// Arms op/read collection around one file's walk on the worker fork and
/// packs the outcome. `None` mirrors the serial path's skip for a file
/// without a syntax tree.
fn walk_file_outcome(
    fork: &mut DbIndex,
    context: &mut AnalyzeContext,
    file_id: FileId,
    is_scripted: bool,
    special_call_direct_matcher: &SpecialCallDirectMatcher,
    gmod_enabled: bool,
    node_profile_enabled: bool,
) -> Option<WalkOutcome> {
    let start = Instant::now();
    read_set::arm_with_member_maps();
    op_log::arm_ops();
    let (walked, profile) = walk_one(
        fork,
        context,
        file_id,
        is_scripted,
        special_call_direct_matcher,
        gmod_enabled,
        node_profile_enabled,
    );
    let (ops, mut_refs, untracked_reads, touched) = op_log::drain_ops();
    let touch_sites = op_log::drain_touch_sites();
    let reads = read_set::disarm();
    if !walked {
        return None;
    }
    let replacements = extract_replacements(fork, &mut_refs);
    let ctx = context.take_walk_deltas();
    let residual_ctx = context.walk_delta_residual_nonempty();
    let residual_fields = if residual_ctx {
        context.walk_delta_residual_fields()
    } else {
        Vec::new()
    };
    let infer_cache = context.infer_manager.take_file(file_id);
    Some(WalkOutcome {
        file_id,
        elapsed: start.elapsed(),
        profile,
        write_keys: collect_write_keys(&ops, &mut_refs),
        write_bits: write_untracked_bits(&ops, &mut_refs),
        reads,
        ops,
        mut_refs,
        replacements,
        untracked_reads,
        touched,
        touch_sites,
        ctx,
        residual_ctx,
        residual_fields,
        infer_cache,
    })
}

/// Owned shadow context moved onto a worker. `AnalyzeContext` is `!Send`
/// solely because `tree_list` holds red chunks — the shadow is built with an
/// empty `tree_list`, the walk never inserts into it, and nodes created on
/// the worker (unresolve exprs, drained context collections) are converted
/// to `Send` descriptors before the outcome crosses back, so the context
/// itself never ferries red nodes between threads.
struct SendContext(AnalyzeContext);

// SAFETY: see the type docs — the shadow's `tree_list` is permanently empty
// and every node-holding field is drained into `ItemDesc`/`LuaSyntaxId`
// descriptors on the worker before the outcome returns.
unsafe impl Send for SendContext {}

/// One worker's share of a level: fork once, then walk each file in order,
/// so the file sees the level-base state plus its earlier chunk-mates'
/// writes — exactly what it would see mid-chunk in the serial walk.
fn compute_chunk(
    db: &DbIndex,
    files: &[FileId],
    mut context: SendContext,
    cache_seeds: &FxHashMap<FileId, LuaInferCache>,
    scripted_scope_files: &FxHashSet<FileId>,
    special_call_direct_matcher: &SpecialCallDirectMatcher,
    gmod_enabled: bool,
    node_profile_enabled: bool,
) -> Vec<WalkOutcome> {
    let mut fork = db.fork();
    files
        .iter()
        .filter_map(|&file_id| {
            if let Some(cache) = cache_seeds.get(&file_id) {
                context.0.infer_manager.restore_file(file_id, cache.clone());
            }
            walk_file_outcome(
                &mut fork,
                &mut context.0,
                file_id,
                scripted_scope_files.contains(&file_id),
                special_call_direct_matcher,
                gmod_enabled,
                node_profile_enabled,
            )
        })
        .collect()
}

/// Runs one dependency level through forked workers and merges outcomes in
/// level order. Returns per-file reports in level order (the serial path's
/// slow-file and profile accounting expects them) plus the rerun count.
pub(in crate::compilation::analyzer) fn analyze_level(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    level: &[FileId],
    scripted_scope_files: &FxHashSet<FileId>,
    special_call_direct_matcher: &SpecialCallDirectMatcher,
    gmod_enabled: bool,
    node_profile_enabled: bool,
) -> (Vec<WalkFileReport>, usize) {
    let _p = Profile::cond_new("lua analyze parallel level", level.len() > 1);
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(16);
    let chunk_count = workers.min((level.len() / 8).max(1));

    // Seed each file's worker cache from a clone of the canonical one so the
    // walk sees exactly what the serial walk would have found.
    let mut cache_seeds: FxHashMap<FileId, LuaInferCache> = FxHashMap::default();
    for &file_id in level {
        if let Some(cache) = context.infer_manager.peek_infer_cache(file_id) {
            cache_seeds.insert(file_id, cache.clone());
        }
    }

    let stats = std::env::var_os("GLUALS_PARWALK_STATS").is_some();
    op_log::set_stats_enabled(stats);

    // Deterministic merge in level order. `canon_*` accumulates every merged
    // file's canonical writes; `clean_*`/`rerun_*` accumulate the chunk
    // prefix each file's fork actually saw (clean mates replayed identically,
    // rerun mates diverged — their fork keys stay suspect for later mates).
    let mut stale_read_keys = 0usize;
    let mut stale_bits = 0usize;
    let mut stale_coverage = 0usize;
    let mut stale_residual = 0usize;
    let mut stale_order = 0usize;
    let mut uncovered_mask = 0u32;
    let mut residual_names: Vec<&'static str> = Vec::new();
    let mut uncovered_sites: FxHashMap<u8, String> = FxHashMap::default();
    let mut bit_mask = 0u32;
    let mut stale_key_counts: FxHashMap<&'static str, usize> = FxHashMap::default();
    let mut reports = Vec::with_capacity(level.len());
    let mut reruns = 0usize;
    let mut compute_elapsed = Duration::ZERO;
    let mut merge_elapsed = Duration::ZERO;

    // Ordered waves: a file's speculative wave sees every earlier wave fully
    // merged, so its reads can only stale on same-wave writes it never saw.
    // The merge semantics are unchanged — only the speculation window shrinks.
    let wave_size = (workers * WAVE_FILES_PER_WORKER).max(1);
    for wave in level.chunks(wave_size) {
        // The stale window for this wave: writes merged *inside* it. Earlier
        // waves' writes are in every fork's base state — the file saw them.
        let mut canon_keys = WriteKeySet::default();
        let mut canon_bits = 0u32;
        let chunks = partition_level(db, wave, chunk_count);

        // Each chunk gets its own shadow context built on this thread: a bare
        // `&AnalyzeContext` cannot cross to workers (`tree_list` is `!Sync`),
        // and each worker must drain deltas against private state anyway.
        // Built per wave so the shadow reflects the canonical state the
        // earlier waves' merges left behind.
        let tasks: Vec<(Vec<FileId>, SendContext)> = chunks
            .into_iter()
            .map(|files| (files, SendContext(context.worker_shadow_context())))
            .collect();

        let compute_start = std::time::Instant::now();
        let run = |(files, context): (Vec<FileId>, SendContext)| {
            compute_chunk(
                db,
                &files,
                context,
                &cache_seeds,
                scripted_scope_files,
                special_call_direct_matcher,
                gmod_enabled,
                node_profile_enabled,
            )
        };
        let chunk_outcomes: Vec<Vec<WalkOutcome>> = if parallel::runs_inline(tasks.len()) {
            tasks.into_iter().map(run).collect()
        } else {
            parallel::init_pool();
            let mut computed = Vec::new();
            rayon::in_place_scope(|_| {
                computed = tasks.into_par_iter().map(run).collect();
            });
            computed
        };
        compute_elapsed += compute_start.elapsed();
        let merge_start = std::time::Instant::now();
        for outcomes in chunk_outcomes {
            let mut clean_keys = WriteKeySet::default();
            let mut clean_bits = 0u32;
            let mut rerun_keys = WriteKeySet::default();
            let mut rerun_bits = 0u32;
            for outcome in outcomes {
                let file_id = outcome.file_id;

                let unseen_bits = (canon_bits & !clean_bits) | rerun_bits;
                let stale_read = reads_stale(&outcome.reads, &canon_keys, &clean_keys, &rerun_keys);
                let stale_bit = (outcome.untracked_reads & unseen_bits) != 0;
                let uncovered = outcome.touched & !outcome.covered_domain_bits();
                let stale_cover = uncovered != 0;
                let stale_ord = outcome.mutref_order_conflict();
                let stale =
                    stale_read || stale_bit || stale_cover || outcome.residual_ctx || stale_ord;
                if stats {
                    stale_read_keys += stale_read as usize;
                    stale_bits += stale_bit as usize;
                    stale_coverage += stale_cover as usize;
                    stale_residual += outcome.residual_ctx as usize;
                    stale_order += stale_ord as usize;
                    uncovered_mask |= uncovered;
                    bit_mask |= outcome.untracked_reads & unseen_bits;
                    if stale_read {
                        count_stale_reads(
                            &outcome.reads,
                            &canon_keys,
                            &clean_keys,
                            &rerun_keys,
                            &mut stale_key_counts,
                        );
                    }
                    residual_names.extend(outcome.residual_fields.iter().copied());
                    for (domain, site) in &outcome.touch_sites {
                        if uncovered & (1 << domain) != 0 {
                            uncovered_sites
                                .entry(*domain)
                                .or_insert_with(|| format!("{}:{}", site.file(), site.line()));
                        }
                    }
                }

                let WalkOutcome {
                    ops,
                    replacements,
                    ctx,
                    infer_cache,
                    write_keys,
                    write_bits,
                    elapsed,
                    profile,
                    mut_refs: _,
                    reads: _,
                    untracked_reads: _,
                    touched: _,
                    touch_sites: _,
                    residual_ctx: _,
                    residual_fields: _,
                    file_id: _,
                } = outcome;

                let mut applied = false;
                if !stale {
                    // Context deltas hydrate before any op replays, so a failure
                    // still leaves the file fully re-runnable.
                    let merged_ctx = {
                        let root = db
                            .get_vfs()
                            .get_syntax_tree(&file_id)
                            .map(|tree| tree.get_red_root());
                        match root {
                            Some(root) => context.apply_walk_deltas(ctx, &root),
                            None => true,
                        }
                    };
                    if merged_ctx {
                        // Each replace splices in just before the op at its
                        // ordinal — the point the object's last borrow happened
                        // — so later ops replay against the final object state
                        // exactly as the serial walk had it.
                        let mut replacements = replacements;
                        replacements.sort_by_key(|(ordinal, _)| *ordinal);
                        let mut replacements = replacements.into_iter().peekable();
                        for (index, op) in ops.into_iter().enumerate() {
                            while let Some(&(ordinal, _)) = replacements.peek() {
                                if ordinal != index {
                                    break;
                                }
                                apply_replacement(
                                    db,
                                    replacements
                                        .next()
                                        .expect("peeked replacement at this ordinal")
                                        .1,
                                );
                            }
                            (op.apply)(db);
                        }
                        for (_, replacement) in replacements {
                            apply_replacement(db, replacement);
                        }
                        if let Some(cache) = infer_cache {
                            context.infer_manager.take_file(file_id);
                            context.infer_manager.restore_file(file_id, cache);
                        }
                        canon_keys.extend_keys(write_keys.iter().cloned());
                        canon_bits |= write_bits;
                        clean_keys.extend_keys(write_keys.iter().cloned());
                        clean_bits |= write_bits;
                        reports.push(WalkFileReport {
                            file_id,
                            elapsed,
                            profile,
                        });
                        applied = true;
                    }
                }

                if !applied {
                    reruns += 1;
                    rerun_keys.extend_keys(write_keys);
                    rerun_bits |= write_bits;
                    let (rerun_key_set, rerun_bit_set, rerun_profile) = rerun_file(
                        db,
                        context,
                        file_id,
                        scripted_scope_files,
                        special_call_direct_matcher,
                        gmod_enabled,
                        node_profile_enabled,
                    );
                    canon_keys.extend_keys(rerun_key_set);
                    canon_bits |= rerun_bit_set;
                    reports.push(WalkFileReport {
                        file_id,
                        elapsed,
                        profile: rerun_profile,
                    });
                }
            }
        }
        merge_elapsed += merge_start.elapsed();
    }
    if stats {
        eprintln!(
            "par_walk level: {} files, {} reruns | stale: {} reads {} bits {} cover {} residual {} order | compute={:?} merge={:?} | uncovered={:#x} residual_fields={:?}",
            reports.len(),
            reruns,
            stale_read_keys,
            stale_bits,
            stale_coverage,
            stale_residual,
            stale_order,
            compute_elapsed,
            merge_elapsed,
            uncovered_mask,
            residual_names,
        );
        if !uncovered_sites.is_empty() {
            eprintln!("  uncovered touch sites: {:?}", uncovered_sites);
        }
        if bit_mask != 0 {
            eprintln!("  stale untracked bits: {:#x}", bit_mask);
        }
        if !stale_key_counts.is_empty() {
            let mut counts: Vec<_> = stale_key_counts.iter().collect();
            counts.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
            eprintln!("  stale read keys: {:?}", counts);
        }
    }
    (reports, reruns)
}

/// Serial re-run of one file on the canonical index — the same walk the
/// serial level loop takes, armed only to harvest its write slots for the
/// staleness prefix. Context deltas and the infer cache land natively.
fn rerun_file(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    file_id: FileId,
    scripted_scope_files: &FxHashSet<FileId>,
    special_call_direct_matcher: &SpecialCallDirectMatcher,
    gmod_enabled: bool,
    node_profile_enabled: bool,
) -> (FxHashSet<WriteKey>, u32, Option<LuaAnalyzeProfile>) {
    op_log::arm_ops();
    let (_, profile) = walk_one(
        db,
        context,
        file_id,
        scripted_scope_files.contains(&file_id),
        special_call_direct_matcher,
        gmod_enabled,
        node_profile_enabled,
    );
    let (ops, mut_refs, _, _) = op_log::drain_ops();
    let keys = collect_write_keys(&ops, &mut_refs);
    let bits = write_untracked_bits(&ops, &mut_refs);
    (keys, bits, profile)
}

/// Installs a mutably-borrowed object's fork-final value on the canonical
/// index through the domain's boundary replace.
fn apply_replacement(db: &mut DbIndex, replacement: MutReplacement) {
    match replacement {
        MutReplacement::Signature(id, signature) => db
            .get_signature_index_mut()
            .replace_signature_object(id, signature),
        MutReplacement::Member(id, member) => {
            db.get_member_index_mut().replace_member_object(id, member)
        }
        MutReplacement::TypeDecl(id, decl) => {
            db.get_type_index_mut().replace_type_decl_object(&id, decl)
        }
        MutReplacement::Decl(id, decl) => db.get_decl_index_mut().replace_decl_object(&id, decl),
        MutReplacement::DeclTree(tree) => db.get_decl_index_mut().add_decl_tree(tree),
        MutReplacement::Module(file_id, module) => db
            .get_module_index_mut()
            .replace_module_object(file_id, module),
    }
}
