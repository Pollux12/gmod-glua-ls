use serde_json::{Map, Value};

use super::Emmyrc;

/// How far a configuration change reaches, ordered from the least work to
/// the most: applying a change takes the work of every lower variant too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EmmyrcChange {
    /// No field differs.
    Unchanged,
    /// Only request handling differs (completion, hover, hints and the like):
    /// the index and every diagnostic stay as they are.
    Editor,
    /// Only diagnostic output differs: the index stays, every file is
    /// diagnosed again.
    Diagnostics,
    /// What the index derives differs: every loaded file is analysed again.
    Analysis,
    /// Which files load differs: the workspace is collected again.
    Workspace,
}

impl EmmyrcChange {
    /// Whether facts indexed under the old configuration may no longer hold.
    pub fn affects_index(self) -> bool {
        self >= Self::Analysis
    }
}

/// The reach of a change to each serialized key path. A path takes the entry
/// of its closest listed ancestor; a path with none counts as
/// [`EmmyrcChange::Analysis`], so a field only lands in a cheaper category by
/// being listed here.
const PATH_CHANGES: &[(&str, EmmyrcChange)] = &[
    ("$schema", EmmyrcChange::Editor),
    ("completion", EmmyrcChange::Editor),
    ("hint", EmmyrcChange::Editor),
    ("codeLens", EmmyrcChange::Editor),
    ("semanticTokens", EmmyrcChange::Editor),
    ("hover", EmmyrcChange::Editor),
    ("documentColor", EmmyrcChange::Editor),
    ("codeAction", EmmyrcChange::Editor),
    ("inlineValues", EmmyrcChange::Editor),
    ("diagnostics", EmmyrcChange::Diagnostics),
    // The code style it generates is what the code-style-check diagnostic reports against.
    ("format", EmmyrcChange::Diagnostics),
    ("workspace", EmmyrcChange::Workspace),
    ("workspace.moduleMap", EmmyrcChange::Analysis),
    ("workspace.enableReindex", EmmyrcChange::Editor),
    ("workspace.reindexDuration", EmmyrcChange::Editor),
    ("runtime.extensions", EmmyrcChange::Workspace),
    ("gmod.enabled", EmmyrcChange::Workspace),
    ("gmod.annotationsPath", EmmyrcChange::Workspace),
    ("gmod.autoLoadAnnotations", EmmyrcChange::Workspace),
    ("gmod.autoDetectGamemodeBase", EmmyrcChange::Workspace),
];

impl Emmyrc {
    /// How far moving from this configuration to `new` reaches.
    pub fn change_to(&self, new: &Emmyrc) -> EmmyrcChange {
        match (serde_json::to_value(self), serde_json::to_value(new)) {
            (Ok(Value::Object(old)), Ok(Value::Object(new))) => object_change("", &old, &new),
            _ => EmmyrcChange::Workspace,
        }
    }
}

fn object_change(prefix: &str, old: &Map<String, Value>, new: &Map<String, Value>) -> EmmyrcChange {
    old.keys()
        .chain(new.keys().filter(|key| !old.contains_key(*key)))
        .map(|key| {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            value_change(&path, old.get(key), new.get(key))
        })
        .max()
        .unwrap_or(EmmyrcChange::Unchanged)
}

fn value_change(path: &str, old: Option<&Value>, new: Option<&Value>) -> EmmyrcChange {
    if old == new {
        return EmmyrcChange::Unchanged;
    }
    if let (Some(Value::Object(old)), Some(Value::Object(new))) = (old, new)
        && has_listed_descendant(path)
    {
        return object_change(path, old, new);
    }
    listed_change(path)
}

fn has_listed_descendant(path: &str) -> bool {
    PATH_CHANGES.iter().any(|(listed, _)| {
        listed
            .strip_prefix(path)
            .is_some_and(|rest| rest.starts_with('.'))
    })
}

fn listed_change(path: &str) -> EmmyrcChange {
    let mut current = path;
    loop {
        if let Some((_, change)) = PATH_CHANGES.iter().find(|(listed, _)| *listed == current) {
            return *change;
        }
        match current.rsplit_once('.') {
            Some((parent, _)) => current = parent,
            None => return EmmyrcChange::Analysis,
        }
    }
}

#[cfg(test)]
mod tests {
    use googletest::prelude::*;

    use super::EmmyrcChange;
    use crate::{DiagnosticCode, DiagnosticSeveritySetting, EmmyLibraryItem, Emmyrc};

    fn change(edit: impl FnOnce(&mut Emmyrc)) -> EmmyrcChange {
        let old = Emmyrc::default();
        let mut new = old.clone();
        edit(&mut new);
        old.change_to(&new)
    }

    #[gtest]
    fn an_equal_configuration_is_unchanged() -> Result<()> {
        verify_that!(change(|_| {}), eq(EmmyrcChange::Unchanged))
    }

    #[gtest]
    fn request_settings_reach_only_the_editor() -> Result<()> {
        verify_that!(
            change(|config| config.hover.enable = !config.hover.enable),
            eq(EmmyrcChange::Editor)
        )?;
        verify_that!(
            change(|config| config.workspace.reindex_duration += 1),
            eq(EmmyrcChange::Editor)
        )
    }

    #[gtest]
    fn diagnostic_settings_reach_only_diagnostics() -> Result<()> {
        verify_that!(
            change(|config| config.diagnostics.disable.push(DiagnosticCode::Unused)),
            eq(EmmyrcChange::Diagnostics)
        )?;
        verify_that!(
            change(|config| {
                config
                    .diagnostics
                    .severity
                    .insert(DiagnosticCode::Unused, DiagnosticSeveritySetting::Error);
            }),
            eq(EmmyrcChange::Diagnostics)
        )?;
        verify_that!(
            change(|config| config.diagnostics.globals.push("Extra".to_string())),
            eq(EmmyrcChange::Diagnostics)
        )
    }

    #[gtest]
    fn inference_settings_reach_the_index() -> Result<()> {
        verify_that!(
            change(|config| config.strict.array_index = !config.strict.array_index),
            eq(EmmyrcChange::Analysis)
        )?;
        verify_that!(
            change(|config| config.runtime.require_pattern.push("?.lua".to_string())),
            eq(EmmyrcChange::Analysis)
        )?;
        verify_that!(
            change(|config| config.gmod.infer_dynamic_fields = !config.gmod.infer_dynamic_fields),
            eq(EmmyrcChange::Analysis)
        )
    }

    #[gtest]
    fn loaded_file_settings_reach_the_workspace() -> Result<()> {
        verify_that!(
            change(|config| config.workspace.ignore_globs.push("**/x/**".to_string())),
            eq(EmmyrcChange::Workspace)
        )?;
        verify_that!(
            change(|config| {
                config
                    .workspace
                    .library
                    .push(EmmyLibraryItem::Path("lib".to_string()));
            }),
            eq(EmmyrcChange::Workspace)
        )?;
        verify_that!(
            change(|config| config.runtime.extensions.push(".glua".to_string())),
            eq(EmmyrcChange::Workspace)
        )?;
        verify_that!(
            change(|config| config.gmod.annotations_path = Some("annotations".to_string())),
            eq(EmmyrcChange::Workspace)
        )
    }

    #[gtest]
    fn the_widest_reach_wins() -> Result<()> {
        verify_that!(
            change(|config| {
                config.hover.enable = !config.hover.enable;
                config.diagnostics.disable.push(DiagnosticCode::Unused);
                config.strict.array_index = !config.strict.array_index;
            }),
            eq(EmmyrcChange::Analysis)
        )
    }
}
