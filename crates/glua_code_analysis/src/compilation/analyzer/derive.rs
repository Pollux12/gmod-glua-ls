//! Which derivations a file has, and how each kind is evaluated.

use glua_parser::{LuaAst, LuaAstNode, LuaChunk, LuaSyntaxId};
use rustc_hash::FxHashMap;

use super::{
    AnalysisPipeline, AnalyzeContext, ScriptedScopes, call_site_params, dynamic_field, gmod,
    local_inference, lua, setmetatable_factory, synthesize_accessorfunc_members, unresolve,
};
use crate::db_index::derivation::{DerivationId, DerivationKind};
use crate::{DbIndex, FileId, InFiled};

/// Every derivation of one file except its source layer, in canonical order.
pub(super) fn file_derivations(
    file_id: FileId,
    root: &LuaChunk,
    guard_candidates: &[InFiled<LuaSyntaxId>],
) -> Vec<DerivationId> {
    let chunk = LuaSyntaxId::from_node(root.syntax());
    let mut ids = Vec::new();
    for node in root.descendants::<LuaAst>() {
        let kind = match &node {
            LuaAst::LuaLocalStat(_) => DerivationKind::LocalStat,
            LuaAst::LuaAssignStat(_) => DerivationKind::AssignStat,
            LuaAst::LuaForRangeStat(_) => DerivationKind::ForRangeStat,
            LuaAst::LuaFuncStat(_) => DerivationKind::FuncStat,
            LuaAst::LuaLocalFuncStat(_) => DerivationKind::LocalFuncStat,
            LuaAst::LuaTableField(_) => DerivationKind::TableField,
            LuaAst::LuaClosureExpr(_) => DerivationKind::Closure,
            LuaAst::LuaCallExpr(call) if call.is_setmetatable() => DerivationKind::Setmetatable,
            LuaAst::LuaCallExpr(_) => DerivationKind::Call,
            _ => continue,
        };
        ids.push(DerivationId::new(
            kind,
            file_id,
            LuaSyntaxId::from_node(node.syntax()),
        ));
    }
    ids.extend(
        guard_candidates
            .iter()
            .filter(|candidate| candidate.file_id == file_id)
            .map(|candidate| DerivationId::new(DerivationKind::Guard, file_id, candidate.value)),
    );
    for kind in [
        DerivationKind::GmodHelperScan,
        DerivationKind::ValidGuardScan,
        DerivationKind::GmodLoadCallSites,
        DerivationKind::GmodPre,
        DerivationKind::ChunkReturn,
        DerivationKind::GmodClassCallSites,
        DerivationKind::GmodPost,
        DerivationKind::AccessorFunc,
        DerivationKind::DynamicFields,
        DerivationKind::SetmetatableFactory,
        DerivationKind::CallSiteParams,
        DerivationKind::LocalFallbacks,
        DerivationKind::NetHelpers,
        DerivationKind::Network,
    ] {
        ids.push(DerivationId::new(kind, file_id, chunk));
    }
    ids
}

/// The workspace derivations: each folds per-file facts into one result.
pub(super) const WORKSPACE_DERIVATIONS: [DerivationKind; 5] = [
    DerivationKind::GmodCallRoles,
    DerivationKind::NetHelperNames,
    DerivationKind::GmodLoad,
    DerivationKind::ValidGuards,
    DerivationKind::VguiChains,
];

/// What every evaluation of one engine run shares.
pub(super) struct Recipes {
    special_call_direct_matcher: lua::call::SpecialCallDirectMatcher,
    scopes: ScriptedScopes,
}

impl Recipes {
    pub(super) fn new(db: &DbIndex, scopes: ScriptedScopes) -> Self {
        let roots = db
            .get_vfs()
            .get_all_local_file_ids()
            .into_iter()
            .filter_map(|file_id| {
                let tree = db.get_vfs().get_syntax_tree(&file_id)?;
                Some((file_id, tree.get_chunk_node()))
            })
            .collect::<FxHashMap<_, _>>();
        Self {
            special_call_direct_matcher: lua::call::build_special_call_direct_matcher(db, &roots),
            scopes,
        }
    }

    pub(super) fn evaluate(&self, db: &mut DbIndex, id: DerivationId) {
        match id.kind {
            DerivationKind::GmodHelperScan => gmod::evaluate_helper_scan(db, id.file_id),
            DerivationKind::NetHelpers => gmod::evaluate_net_helpers(db, id.file_id),
            DerivationKind::ValidGuardScan => {
                crate::db_index::scan_valid_guard_signatures(db, id.file_id)
            }
            DerivationKind::GmodCallRoles => gmod::evaluate_call_roles(db),
            DerivationKind::NetHelperNames => gmod::evaluate_net_helper_names(db),
            DerivationKind::GmodLoad => gmod::evaluate_load(db),
            DerivationKind::ValidGuards => gmod::evaluate_valid_guards(db),
            DerivationKind::VguiChains => {
                let file_ids = db.get_vfs().get_all_local_file_ids();
                let mut context = AnalyzeContext::for_files(db, file_ids, &self.scopes);
                gmod::evaluate_vgui_chains(db, &mut context);
            }
            DerivationKind::Source | DerivationKind::Diagnostics => {
                unreachable!("{:?} derivations are not evaluated by recipe", id.kind)
            }
            _ => {
                let mut context = AnalyzeContext::for_files(db, vec![id.file_id], &self.scopes);
                self.evaluate_file_derivation(db, &mut context, id);
                unresolve::resolve_inline(db, &mut context);
            }
        }
    }

    fn evaluate_file_derivation(
        &self,
        db: &mut DbIndex,
        context: &mut AnalyzeContext,
        id: DerivationId,
    ) {
        let file_ids = [id.file_id];
        match id.kind {
            DerivationKind::Guard => {
                context.add_inferred_guard_candidate(InFiled::new(id.file_id, id.syntax_id()));
                local_inference::publish_inferred_positive_guards(db, context);
            }
            DerivationKind::LocalStat
            | DerivationKind::AssignStat
            | DerivationKind::ForRangeStat
            | DerivationKind::FuncStat
            | DerivationKind::LocalFuncStat
            | DerivationKind::TableField
            | DerivationKind::Closure
            | DerivationKind::Call
            | DerivationKind::Setmetatable
            | DerivationKind::ChunkReturn => lua::evaluate_node(
                db,
                context,
                &self.special_call_direct_matcher,
                id.file_id,
                id.syntax_id(),
            ),
            DerivationKind::GmodLoadCallSites => gmod::evaluate_load_call_sites(db, context),
            DerivationKind::GmodPre => gmod::evaluate_pre(db, context),
            DerivationKind::GmodClassCallSites => gmod::evaluate_class_call_sites(db, context),
            DerivationKind::GmodPost => gmod::evaluate_post(db, context),
            DerivationKind::AccessorFunc => synthesize_accessorfunc_members(db, &file_ids),
            DerivationKind::DynamicFields => {
                dynamic_field::DynamicFieldAnalysisPipeline::analyze(db, context)
            }
            DerivationKind::SetmetatableFactory => {
                setmetatable_factory::synthesize_setmetatable_factory_members(db, &file_ids)
            }
            DerivationKind::CallSiteParams => {
                call_site_params::CallSiteParamAnalysisPipeline::analyze(db, context)
            }
            DerivationKind::LocalFallbacks => {
                let mut sites = local_inference::UnguardedChildSiteCache::new();
                local_inference::stabilize_unguarded_children(db, context, false, &mut sites);
                local_inference::stabilize_unknown_locals(db, context);
            }
            DerivationKind::Network => gmod::evaluate_network(db, context),
            DerivationKind::Source
            | DerivationKind::GmodHelperScan
            | DerivationKind::NetHelpers
            | DerivationKind::ValidGuardScan
            | DerivationKind::GmodCallRoles
            | DerivationKind::NetHelperNames
            | DerivationKind::GmodLoad
            | DerivationKind::ValidGuards
            | DerivationKind::VguiChains
            | DerivationKind::Diagnostics => unreachable!(),
        }
    }
}
