//! A `pairs` copy `t[k] = v` writes each field of the tables it iterates into
//! the same field of `t`: a field it copies from `t` itself is the value `t`
//! already holds there.
#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, Emmyrc, FileId, VirtualWorkspace};
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    /// Items registered on one metatable: an item copies the fields of the
    /// base item it names, and base items are tables of that same metatable.
    const REGISTRY: (&str, &str) = (
        "lua/autorun/sh_a_item.lua",
        r#"Lib = Lib or {}
Lib.meta = Lib.meta or {}
local META = Lib.meta
META.__index = META
META.name = "Undefined"
META.width = 1
function META:GetName()
    return self.name
end
Lib.base = Lib.base or {}
Lib.extra = { color = "red" }

---@param id string
---@param baseID string?
function Lib.Register(id, baseID)
    ITEM = Lib.base[id] or setmetatable({}, META)
    ITEM.base = baseID or ITEM.base
    local baseTable = Lib.base[ITEM.base]
    if baseTable then
        for k, v in pairs(baseTable) do
            if ITEM[k] == nil then
                ITEM[k] = v
            end
        end
    end
    Lib.base[id] = ITEM
end
"#,
    );

    /// Methods of the metatable, in another file, reading fields the copy
    /// may write, and saving and restoring one.
    const META_READS: (&str, &str) = (
        "lua/autorun/sh_b_meta.lua",
        r#"---@param entity Entity?
function Lib.meta:Call(entity)
    local oldEntity = self.entity
    self.entity = entity or self.entity
    self.entity = oldEntity
end

function Lib.meta:Print()
    local base = self.base
    local width = self.width
    local color = self.color
    print(self.gridX, self.owner, base:lower(), width + 1, color)
end
"#,
    );

    fn workspace(files: &[(&str, &str)]) -> (VirtualWorkspace, Vec<FileId>) {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_file("lua/includes/entity.lua", "---@class Entity\n");
        let file_ids = ws.def_files(files.to_vec());
        (ws, file_ids)
    }

    fn local_type(ws: &mut VirtualWorkspace, file_id: FileId, name: &str) -> String {
        let typ = ws.local_ty(file_id, name);
        ws.humanize_type(typ)
    }

    /// The `code` diagnostics `file_id` gets, as `line:message`.
    fn diagnostics(
        ws: &mut VirtualWorkspace,
        file_id: FileId,
        code: DiagnosticCode,
    ) -> Vec<String> {
        ws.analysis.diagnostic.enable_only(code);
        let name = Some(NumberOrString::String(code.get_name().to_string()));
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| diagnostic.code == name)
            .map(|diagnostic| format!("{}:{}", diagnostic.range.start.line, diagnostic.message))
            .collect()
    }

    #[test]
    fn a_table_copied_into_itself_gives_each_field_only_its_own_values() {
        let (mut ws, file_ids) = workspace(&[REGISTRY, META_READS]);

        assert_eq!(
            (
                local_type(&mut ws, file_ids[1], "base"),
                local_type(&mut ws, file_ids[1], "width"),
                local_type(&mut ws, file_ids[1], "oldEntity"),
            ),
            (
                "(string|unknown)".to_string(),
                "1".to_string(),
                "any".to_string()
            )
        );
    }

    #[test]
    fn a_table_copied_into_itself_type_checks_each_field_against_its_own_values() {
        let (mut ws, file_ids) = workspace(&[REGISTRY, META_READS]);

        assert_eq!(
            diagnostics(&mut ws, file_ids[1], DiagnosticCode::ParamTypeMismatch),
            Vec::<String>::new()
        );
    }

    #[test]
    fn fields_a_copy_into_the_table_may_write_stay_defined() {
        let (mut ws, file_ids) = workspace(&[REGISTRY, META_READS]);

        assert_eq!(
            diagnostics(&mut ws, file_ids[1], DiagnosticCode::UndefinedField),
            Vec::<String>::new()
        );
    }

    #[test]
    fn copying_the_same_reference_does_not_define_new_fields() {
        let (mut ws, files) = workspace(&[(
            "lua/autorun/sh_self_copy.lua",
            r#"
            local value = { known = 1 }
            for key, item in pairs(value) do value[key] = item end
            local missing = value.notWritten
        "#,
        )]);
        let reports = diagnostics(&mut ws, files[0], DiagnosticCode::UndefinedField);
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert!(reports[0].contains("notWritten"), "{reports:?}");
    }

    #[test]
    fn a_loop_writing_new_values_back_into_the_table_it_iterates_gives_them_to_its_fields() {
        let (mut ws, file_ids) = workspace(&[(
            "lua/autorun/sh_defaults.lua",
            r#"local Defaults = { name = "x" }
for k, v in pairs(Defaults) do
    v = 1
    Defaults[k] = v
end
local name = Defaults.name
"#,
        )]);

        assert_eq!(local_type(&mut ws, file_ids[0], "name"), "(integer|string)");
    }

    #[test]
    fn a_loop_writing_values_back_under_new_keys_gives_them_to_those_fields() {
        let (mut ws, file_ids) = workspace(&[(
            "lua/autorun/sh_names.lua",
            r#"local Names = { name = "x" }
for k, v in pairs(Names) do
    k = "copy"
    Names[k] = v
end
local copy = Names.copy
"#,
        )]);

        assert_eq!(local_type(&mut ws, file_ids[0], "copy"), "\"x\"");
    }

    #[test]
    fn a_copy_from_another_table_gives_the_fields_that_tables_values() {
        let (mut ws, file_ids) = workspace(&[(
            "lua/autorun/sh_sounds.lua",
            r#"Sounds = {}
local source = { drip = "ambient/water/drip1.wav" }
for k, v in pairs(source) do
    Sounds[k] = v
end
local drip = Sounds.drip
"#,
        )]);

        assert_eq!(
            local_type(&mut ws, file_ids[0], "drip"),
            "\"ambient/water/drip1.wav\""
        );
    }

    #[test]
    fn a_key_and_value_from_different_loops_give_the_fields_the_inner_loops_values() {
        let (mut ws, file_ids) = workspace(&[(
            "lua/autorun/sh_sizes.lua",
            r#"local Sizes = { name = "x" }
local Other = { size = 1 }
for k in pairs(Sizes) do
    for _, v in pairs(Other) do
        Sizes[k] = v
    end
end
local name = Sizes.name
"#,
        )]);

        assert_eq!(local_type(&mut ws, file_ids[0], "name"), "(integer|string)");
    }

    #[test]
    fn a_table_copied_into_itself_settles_the_same_in_any_order() {
        crate::compilation::test::schedule_order_test::test::assert_schedule_independent(&[
            ("lua/includes/entity.lua", "---@class Entity\n"),
            REGISTRY,
            META_READS,
        ]);
    }

    #[test]
    fn a_copy_from_the_table_and_another_settles_the_same_in_any_order() {
        let mixed = REGISTRY.1.replace(
            "local baseTable = Lib.base[ITEM.base]",
            "local baseTable = Lib.base[ITEM.base] or Lib.extra",
        );
        crate::compilation::test::schedule_order_test::test::assert_schedule_independent(&[
            ("lua/includes/entity.lua", "---@class Entity\n"),
            (REGISTRY.0, &mixed),
            META_READS,
        ]);
    }

    #[test]
    fn a_copy_whose_source_stops_being_the_table_settles_as_a_cold_build() {
        let setup = |ws: &mut VirtualWorkspace| {
            let mut emmyrc = Emmyrc::default();
            emmyrc.gmod.enabled = true;
            ws.update_emmyrc(emmyrc);
            ws.def_file("lua/includes/entity.lua", "---@class Entity\n");
        };
        let other_source = REGISTRY.1.replace(
            "local baseTable = Lib.base[ITEM.base]",
            "local baseTable = Lib.extra",
        );

        VirtualWorkspace::assert_warm_matches_cold(
            setup,
            &[REGISTRY, META_READS],
            &[(REGISTRY.0, &other_source), (REGISTRY.0, REGISTRY.1)],
        );
        VirtualWorkspace::assert_warm_matches_cold(
            setup,
            &[(REGISTRY.0, &other_source), META_READS],
            &[(REGISTRY.0, REGISTRY.1)],
        );
    }

    /// `local pairs = pairs`, which many files open with, is the builtin
    /// `pairs` wherever it is read.
    #[test]
    fn a_local_alias_of_pairs_copies_a_table_into_itself_as_pairs_does() {
        let aliased = format!(
            "local pairs = pairs
{}",
            REGISTRY.1
        );
        let (mut ws, file_ids) = workspace(&[(REGISTRY.0, &aliased), META_READS]);

        assert_eq!(
            (
                local_type(&mut ws, file_ids[1], "base"),
                local_type(&mut ws, file_ids[1], "width"),
                local_type(&mut ws, file_ids[1], "oldEntity"),
            ),
            (
                "(string|unknown)".to_string(),
                "1".to_string(),
                "any".to_string()
            )
        );
    }
}
