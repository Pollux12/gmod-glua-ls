mod bind_analyze;
mod binder;

use crate::{
    AnalyzeError, FileId, FlowTree,
    compilation::analyzer::{
        AnalysisPipeline,
        flow::{
            bind_analyze::{bind_analyze, check_goto_label},
            binder::FlowBinder,
        },
    },
    db_index::DbIndex,
    profile::Profile,
};

use super::AnalyzeContext;

pub struct FlowAnalysisPipeline;

/// Build the flow tree for a single file. Reads only the file's own AST plus
/// pre-existing immutable `&DbIndex` state (reference index from the decl pass),
/// so this is safe to run concurrently across files.
fn bind_flow_tree_for_file(
    db: &DbIndex,
    file_id: FileId,
    chunk: glua_parser::LuaChunk,
) -> (FlowTree, Vec<AnalyzeError>) {
    let mut binder = FlowBinder::new(db, file_id);
    bind_analyze(&mut binder, chunk);
    check_goto_label(&mut binder);
    binder.finish()
}

impl AnalysisPipeline for FlowAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        let _p = Profile::cond_new("flow analyze", context.tree_list.len() > 1);

        let file_ids: Vec<FileId> = context.tree_list.iter().map(|t| t.file_id).collect();

        // Flow binding is per-file independent: each file reads only its own AST
        // and the already-built (immutable) reference index, and produces a
        // FlowTree plus a list of diagnostics. Run the binding in parallel and
        // merge the results into the db sequentially in deterministic file order.
        let results = super::parallel::map_files_collect(db, &file_ids, |db, file_id| {
            // Rebuild the red tree locally from the (Send) green tree so no
            // non-Send rowan node crosses the thread boundary.
            let chunk = db.get_vfs().get_syntax_tree(&file_id)?.get_chunk_node();
            Some(bind_flow_tree_for_file(db, file_id, chunk))
        });

        for (file_id, result) in file_ids.iter().zip(results) {
            let Some((flow_tree, errors)) = result else {
                continue;
            };
            db.get_flow_index_mut().add_flow_tree(*file_id, flow_tree);
            if !errors.is_empty() {
                let diagnostic_index = db.get_diagnostic_index_mut();
                for error in errors {
                    diagnostic_index.add_diagnostic(*file_id, error);
                }
            }
        }
    }
}
