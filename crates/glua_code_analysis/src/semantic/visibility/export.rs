use crate::{LuaExportScope, ModuleInfo, SemanticModel};

/// Checks whether a module is visible.
///
/// A module without an export tag is visible.
pub fn check_export_visibility(
    semantic_model: &SemanticModel,
    module_info: &ModuleInfo,
) -> Option<bool> {
    // Check whether the module has an export tag
    let Some(export) = module_info.get_export(semantic_model.get_db()) else {
        return check_default_export_visibility(semantic_model, module_info);
    };

    match export.scope {
        LuaExportScope::Namespace => {
            let type_index = semantic_model.get_db().get_type_index();
            let module_namespace = type_index.get_file_namespace(&module_info.file_id)?;

            if let Some(using_namespaces) =
                type_index.get_file_using_namespace(&semantic_model.get_file_id())
            {
                for using_namespace in using_namespaces {
                    if using_namespace == module_namespace
                        || using_namespace.starts_with(&format!("{}.", module_namespace))
                    {
                        return Some(true);
                    }
                }
            }
            let file_namespace = type_index.get_file_namespace(&semantic_model.get_file_id())?;
            if file_namespace == module_namespace
                || file_namespace.starts_with(&format!("{}.", module_namespace))
            {
                return Some(true);
            }
        }
        LuaExportScope::Global => {
            return Some(true);
        }
        LuaExportScope::Default => {
            return check_default_export_visibility(semantic_model, module_info);
        }
    }

    Some(false)
}

/// Checks visibility under the default export scope
fn check_default_export_visibility(
    semantic_model: &SemanticModel,
    module_info: &ModuleInfo,
) -> Option<bool> {
    // If require_export_global is not enabled, the module is visible by default.
    if !semantic_model.emmyrc.strict.require_export_global {
        return Some(true);
    }

    // A module declared as a library file is not considered visible.
    if semantic_model
        .db
        .get_module_index()
        .is_library(&module_info.file_id)
    {
        return Some(false);
    }
    Some(true)
}
