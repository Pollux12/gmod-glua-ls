mod bind_analyze;
mod binder;

use crate::{
    AnalyzeError, FileId, FlowTree,
    compilation::analyzer::flow::{
        bind_analyze::{bind_analyze, check_goto_label},
        binder::FlowBinder,
    },
    db_index::DbIndex,
};

/// Build the flow tree for a single file. Reads only the file's own AST plus
/// pre-existing immutable `&DbIndex` state (reference index from the decl pass),
/// so this is safe to run concurrently across files.
pub(super) fn bind_flow_tree_for_file(
    db: &DbIndex,
    file_id: FileId,
    chunk: glua_parser::LuaChunk,
) -> (FlowTree, Vec<AnalyzeError>) {
    let mut binder = FlowBinder::new(db, file_id);
    bind_analyze(&mut binder, chunk);
    check_goto_label(&mut binder);
    binder.finish()
}
