use googletest::prelude::*;
use lsp_types::Uri;
use rowan::{TextRange, TextSize};

use super::{ExportKey, FileRemap, PositionMap, export_map, remap_into_file};
use crate::{
    DbIndex, Emmyrc, FileId, InFiled, LuaSignatureId, LuaType, LuaTypeOwner, SignatureReturnStatus,
    TypeVisitTrait, VirtualWorkspace, diff_exports,
};

fn range(start: u32, end: u32) -> TextRange {
    TextRange::new(TextSize::new(start), TextSize::new(end))
}

// --- PositionMap arithmetic ---

#[gtest]
fn an_insertion_shifts_everything_after_it_and_loses_nothing() {
    let map = PositionMap::new("abcdef", "abcXXdef");
    expect_that!(map.hunk(), eq((3, 3, 5)));
    expect_that!(map.map(TextSize::new(2)), some(eq(TextSize::new(2))));
    // The insertion point itself belongs to what follows it.
    expect_that!(map.map(TextSize::new(3)), some(eq(TextSize::new(5))));
    expect_that!(map.map(TextSize::new(6)), some(eq(TextSize::new(8))));
    // Nothing is strictly inside an empty hunk.
    expect_that!(map.map_range(range(1, 5)), some(eq(range(1, 7))));
    expect_that!(map.map_range(range(0, 3)), some(eq(range(0, 3))));
    expect_that!(map.map_range(range(3, 6)), some(eq(range(5, 8))));
}

#[gtest]
fn a_deletion_loses_only_what_was_inside_it() {
    let map = PositionMap::new("abcXXdef", "abcdef");
    expect_that!(map.hunk(), eq((3, 5, 3)));
    expect_that!(map.map(TextSize::new(2)), some(eq(TextSize::new(2))));
    expect_that!(map.map(TextSize::new(4)), none());
    expect_that!(map.map(TextSize::new(5)), some(eq(TextSize::new(3))));
    expect_that!(map.map_range(range(4, 7)), none());
    expect_that!(map.map_range(range(0, 2)), some(eq(range(0, 2))));
    expect_that!(map.map_range(range(6, 8)), some(eq(range(4, 6))));
}

#[gtest]
fn a_replacement_maps_the_text_on_either_side() {
    let map = PositionMap::new("abcXYZdef", "abcQdef");
    expect_that!(map.hunk(), eq((3, 6, 4)));
    expect_that!(map.map(TextSize::new(1)), some(eq(TextSize::new(1))));
    expect_that!(map.map(TextSize::new(4)), none());
    expect_that!(map.map(TextSize::new(6)), some(eq(TextSize::new(4))));
}

#[gtest]
fn identical_texts_are_the_identity_map() {
    let map = PositionMap::new("abcdef", "abcdef");
    expect_that!(map.is_identity(), is_true());
    expect_that!(PositionMap::identity().is_identity(), is_true());
    expect_that!(map.map(TextSize::new(3)), some(eq(TextSize::new(3))));
    expect_that!(map.map_range(range(0, 6)), some(eq(range(0, 6))));
    expect_that!(PositionMap::new("abc", "abd").is_identity(), is_false());
}

#[gtest]
fn a_whole_file_replacement_keeps_only_a_range_that_brackets_it() {
    let map = PositionMap::whole_file(10, 4);
    expect_that!(map.map(TextSize::new(0)), none());
    expect_that!(map.map(TextSize::new(10)), some(eq(TextSize::new(4))));
    expect_that!(map.map_range(range(2, 8)), none());
    // The bracketing range still maps: the edit happened inside it.
    expect_that!(map.map_range(range(0, 10)), some(eq(range(0, 4))));

    let deleted = PositionMap::whole_file(10, 0);
    expect_that!(deleted.map_range(range(1, 3)), none());
}

/// A hub table literal or a long function brackets almost every edit made
/// inside it. Calling those ranges lost would invalidate every dependent on
/// every keystroke; what actually changed shows up as a per-member or
/// per-signature key instead.
#[gtest]
fn a_range_that_spans_the_hunk_moves_with_its_end() {
    let map = PositionMap::new("aaXXbb", "aaYYYbb");
    expect_that!(map.hunk(), eq((2, 4, 5)));
    expect_that!(map.map_range(range(0, 6)), some(eq(range(0, 7))));
    expect_that!(map.map_range(range(2, 4)), some(eq(range(2, 5))));
    // One endpoint strictly inside is still lost.
    expect_that!(map.map_range(range(3, 6)), none());
    expect_that!(map.map_range(range(0, 3)), none());
}

#[gtest]
fn a_hunk_boundary_never_splits_a_code_point() {
    // The two strings share the first byte of `é` (0xC3) but differ in the
    // second, so the raw byte prefix would end mid-character.
    let map = PositionMap::new("aé", "aè");
    let (start, old_end, new_end) = map.hunk();
    expect_that!(start, eq(1));
    expect_that!(old_end, eq(3));
    expect_that!(new_end, eq(3));
    expect_that!("aé".is_char_boundary(start as usize), is_true());
    expect_that!("aè".is_char_boundary(start as usize), is_true());

    // A trailing multibyte character shared by both sides must stay whole in
    // the suffix too.
    let map = PositionMap::new("aXé", "aYYé");
    let (start, old_end, new_end) = map.hunk();
    expect_that!(start, eq(1));
    expect_that!("aXé".is_char_boundary(old_end as usize), is_true());
    expect_that!("aYYé".is_char_boundary(new_end as usize), is_true());
    expect_that!(
        map.map(TextSize::new(old_end)),
        some(eq(TextSize::new(new_end)))
    );
}

// --- Export map ---

fn workspace() -> VirtualWorkspace {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    ws.update_emmyrc(Emmyrc::default());
    ws
}

fn write(ws: &mut VirtualWorkspace, uri: &Uri, text: &str) -> FileId {
    ws.analysis
        .update_file_by_uri(uri, Some(text.to_string()))
        .map(|(id, _)| id)
        .expect("file id")
}

#[test]
fn a_setmetatable_instance_keeps_inherited_members_after_edit_and_reopen() {
    fn inherited_member(
        ws: &VirtualWorkspace,
        consumer: FileId,
    ) -> std::result::Result<LuaType, crate::InferFailReason> {
        let expr = ws.get_node::<glua_parser::LuaIndexExpr>(consumer);
        ws.analysis
            .compilation
            .get_semantic_model(consumer)
            .unwrap()
            .infer_expr(glua_parser::LuaExpr::IndexExpr(expr))
    }

    let first = "Provider = { inherited = 42 }\nProviderMeta = { __index = Provider }\n";
    let second = format!("local padding = true\n{first}");
    let consumer =
        "local instance = setmetatable({ own = true }, ProviderMeta)\nreturn instance.inherited\n";
    let mut ws = workspace();
    let provider_uri = ws
        .virtual_url_generator
        .new_uri("lua/autorun/sh_provider.lua");
    let provider_id = write(&mut ws, &provider_uri, first);
    let consumer_uri = ws
        .virtual_url_generator
        .new_uri("lua/autorun/sh_consumer.lua");
    let consumer_id = write(&mut ws, &consumer_uri, consumer);
    let before = inherited_member(&ws, consumer_id).unwrap();
    let db = ws.analysis.compilation.get_db();
    assert!(db.get_type_index().file_type_owners(consumer_id).unwrap().iter().any(|owner| {
        matches!(db.get_type_index().get_type_cache(owner).map(|cache| cache.as_type()),
            Some(LuaType::Instance(instance)) if matches!(instance.get_base(), LuaType::TableConst(range) if range.file_id == provider_id))
    }));

    assert!(
        !edit_and_remap(&mut ws, &provider_uri, provider_id, first, &second).contains(&consumer_id)
    );
    let after = inherited_member(&ws, consumer_id).unwrap();
    let mut cold = workspace();
    let cold_provider = cold
        .virtual_url_generator
        .new_uri("lua/autorun/sh_provider.lua");
    write(&mut cold, &cold_provider, &second);
    let cold_consumer = cold
        .virtual_url_generator
        .new_uri("lua/autorun/sh_consumer.lua");
    let cold_consumer_id = write(&mut cold, &cold_consumer, consumer);
    assert_eq!(after, inherited_member(&cold, cold_consumer_id).unwrap());
    assert_eq!(after, before);

    edit_and_remap(&mut ws, &provider_uri, provider_id, &second, first);
    assert_eq!(inherited_member(&ws, consumer_id).unwrap(), before);
    write(&mut ws, &provider_uri, first);
    assert_eq!(inherited_member(&ws, consumer_id).unwrap(), before);
    ws.analysis.update_file_by_uri(&provider_uri, None);
    assert!(inherited_member(&ws, consumer_id).is_err());
}

/// The diff an edit to one file produces, with the old map expressed in the
/// new text's coordinates - which is what makes the two comparable.
fn diff_after_edit(first: &str, second: &str) -> super::ChangedExports {
    let mut ws = workspace();
    let uri = ws.virtual_url_generator.new_uri("lua/subject.lua");
    let file_id = write(&mut ws, &uri, first);
    let map = PositionMap::new(first, second);
    let old = export_map(
        ws.analysis.compilation.get_db(),
        file_id,
        &FileRemap::unvalidated(file_id, map),
    );
    write(&mut ws, &uri, second);
    let new = export_map(
        ws.analysis.compilation.get_db(),
        file_id,
        &FileRemap::identity(file_id),
    );
    diff_exports(&old, &new)
}

/// The case the whole redesign exists for: a statement inside a function body
/// is not observable from any other file, so it must produce no diff at all.
#[gtest]
fn a_body_only_edit_changes_no_export() {
    let diff = diff_after_edit(
        r#"
        config = config or {}
        config.Mode = "server"
        function config.Match(a, b)
            if a == b then return true end
            return false
        end
        "#,
        r#"
        config = config or {}
        config.Mode = "server"
        function config.Match(a, b)
            local scratch = 1
            _ = scratch
            if a == b then return true end
            return false
        end
        "#,
    );
    expect_that!(
        diff.is_empty(),
        is_true(),
        "unexpected export diff: {diff:?}"
    );
}

#[gtest]
fn adding_a_parameter_changes_that_signature_only() {
    let diff = diff_after_edit(
        r#"
        config = config or {}
        function config.Match(a) end
        "#,
        r#"
        config = config or {}
        function config.Match(a, b) end
        "#,
    );
    expect_that!(diff.is_empty(), is_false());
    expect_that!(
        diff.keys()
            .any(|key| matches!(key, ExportKey::Signature(_))),
        is_true()
    );
}

#[gtest]
fn adding_an_exported_function_adds_a_member_key() {
    let diff = diff_after_edit(
        r#"
        config = config or {}
        function config.First() end
        "#,
        r#"
        config = config or {}
        function config.Second() end
        function config.First() end
        "#,
    );
    expect_that!(
        diff.added
            .iter()
            .any(|key| matches!(key, ExportKey::Member(_))),
        is_true()
    );
}

#[gtest]
fn removing_an_exported_function_removes_its_keys() {
    let diff = diff_after_edit(
        r#"
        config = config or {}
        function config.Second() end
        function config.First() end
        "#,
        r#"
        config = config or {}
        function config.First() end
        "#,
    );
    expect_that!(
        diff.removed
            .iter()
            .any(|key| matches!(key, ExportKey::Member(_) | ExportKey::Signature(_))),
        is_true()
    );
}

#[gtest]
fn adding_a_field_to_a_table_literal_adds_a_member_key() {
    let diff = diff_after_edit(
        r#"
        registry = { first = 1 }
        "#,
        r#"
        registry = { first = 1, second = 2 }
        "#,
    );
    expect_that!(
        diff.added
            .iter()
            .any(|key| matches!(key, ExportKey::Member(_))),
        is_true()
    );
}
/// Shift-only probe: insert a blank line at the top and dump every export key
/// whose hash moves. Must stay empty: a pure offset shift is not a change.
/// The fixture covers every export family in one file: members, type decls
/// (+supers, generics), signatures (+doc attrs, overloads, param/return
/// docs), type caches, properties with attribute uses, operators,
/// metatables, module exports, load edges and namespaces.
#[gtest]
fn shift_only_edit_produces_an_empty_export_diff_dumping_movers() {
    let first = r#"---@meta
---@class ShiftProbe
---@field name string
---@generic T
---@param value T
---@return T
---@nodiscard
---@version >5.1
---@export namespace
local ShiftProbe = {}
function ShiftProbe.identify(value) return value end
---@param count integer
---@return string
function ShiftProbe.describe(count) return tostring(count) end
registry = { first = 1 }
result = ShiftProbe.describe(1)
return ShiftProbe
"#;
    let second = format!("\n{first}");
    let mut ws = workspace();
    let uri = ws.virtual_url_generator.new_uri("lua/shift_probe.lua");
    let file_id = write(&mut ws, &uri, first);
    let map = PositionMap::new(first, &second);
    let old = export_map(
        ws.analysis.compilation.get_db(),
        file_id,
        &FileRemap::unvalidated(file_id, map),
    );
    write(&mut ws, &uri, &second);
    let new = export_map(
        ws.analysis.compilation.get_db(),
        file_id,
        &FileRemap::identity(file_id),
    );
    let diff = diff_exports(&old, &new);
    expect_that!(diff.is_empty(), is_true(), "shift-only movers: {diff:?}");
}

/// Narrower shift probes, one per hashed-payload family, so a failure names
/// the leaking value shape rather than just "something moved".
#[gtest]
fn shift_only_member_key_and_owner_hash_stable() {
    let diff = diff_after_edit(
        "registry = { first = 1 }\nregistry.second = 2\n",
        "\nregistry = { first = 1 }\nregistry.second = 2\n",
    );
    expect_that!(diff.is_empty(), is_true(), "shift-only movers: {diff:?}");
}

#[gtest]
fn shift_only_signature_doc_payload_hash_stable() {
    let diff = diff_after_edit(
        "---@param name string\n---@return string\nfunction describe(name) return name end\n",
        "\n---@param name string\n---@return string\nfunction describe(name) return name end\n",
    );
    expect_that!(diff.is_empty(), is_true(), "shift-only movers: {diff:?}");
}

#[gtest]
fn shift_only_type_decl_payload_hash_stable() {
    let diff = diff_after_edit(
        "---@class ShiftProbe\n---@field name string\nlocal ShiftProbe = {}\nreturn ShiftProbe\n",
        "\n---@class ShiftProbe\n---@field name string\nlocal ShiftProbe = {}\nreturn ShiftProbe\n",
    );
    expect_that!(diff.is_empty(), is_true(), "shift-only movers: {diff:?}");
}

#[gtest]
fn shift_only_property_attribute_payload_hash_stable() {
    let diff = diff_after_edit(
        "---@nodiscard\n---@version >5.1\nfunction compute(value) return value end\n",
        "\n---@nodiscard\n---@version >5.1\nfunction compute(value) return value end\n",
    );
    expect_that!(diff.is_empty(), is_true(), "shift-only movers: {diff:?}");
}
#[gtest]
fn shift_only_doc_instance_and_table_const_payload_hash_stable() {
    let diff = diff_after_edit(
        "---@class Widget\nlocal Widget = {}\n---@type Widget\ninstance = {}\n---@return Widget\nfunction make() return instance end\n",
        "\n---@class Widget\nlocal Widget = {}\n---@type Widget\ninstance = {}\n---@return Widget\nfunction make() return instance end\n",
    );
    expect_that!(diff.is_empty(), is_true(), "shift-only movers: {diff:?}");
}
#[gtest]
fn shift_only_branch_realm_block_hash_stable() {
    let diff = diff_after_edit(
        "shared = 1\nif CLIENT then\nclient_only = 2\nend\nshared_tail = 3\n",
        "\nshared = 1\nif CLIENT then\nclient_only = 2\nend\nshared_tail = 3\n",
    );
    expect_that!(diff.is_empty(), is_true(), "shift-only movers: {diff:?}");
}
#[gtest]
fn shift_only_metatable_binding_hash_stable() {
    let diff = diff_after_edit(
        "MT = { __index = {} }\ninstance = setmetatable({}, MT)\n",
        "\nMT = { __index = {} }\ninstance = setmetatable({}, MT)\n",
    );
    expect_that!(diff.is_empty(), is_true(), "shift-only movers: {diff:?}");
}

#[gtest]
fn shift_only_operator_payload_hash_stable() {
    let diff = diff_after_edit(
        "---@class Counter\n---@operator add(Counter): Counter\nlocal Counter = {}\nreturn Counter\n",
        "\n---@class Counter\n---@operator add(Counter): Counter\nlocal Counter = {}\nreturn Counter\n",
    );
    expect_that!(diff.is_empty(), is_true(), "shift-only movers: {diff:?}");
}
#[gtest]
fn shift_only_net_flow_payload_hash_stable() {
    let first = "util.AddNetworkString(\"ProbeMessage\")\nnet.Start(\"ProbeMessage\")\nnet.WriteString(\"hello\")\nnet.Broadcast()\nnet.Receive(\"ProbeMessage\", function() local text = net.ReadString() end)\n";
    let diff = diff_after_edit(first, &format!("\n{first}"));
    expect_that!(diff.is_empty(), is_true(), "shift-only movers: {diff:?}");
}

#[gtest]
fn shift_only_module_and_load_edge_hash_stable() {
    let first = "local helper = include(\"sh_helper.lua\")\nhelper.ping()\nreturn {}\n";
    let diff = diff_after_edit(first, &format!("\n{first}"));
    expect_that!(diff.is_empty(), is_true(), "shift-only movers: {diff:?}");
}
#[gtest]
fn shift_only_instance_return_flag_payload_hash_stable() {
    let first = "---@class Widget\nlocal Widget = {}\n---@return (instance) Widget\nfunction make() return {} end\n";
    let diff = diff_after_edit(first, &format!("\n{first}"));
    expect_that!(diff.is_empty(), is_true(), "shift-only movers: {diff:?}");
}
#[gtest]
fn shift_only_overload_and_generic_payload_hash_stable() {
    let first = "---@generic T\n---@param value T\n---@return T\n---@overload fun(other: string): string\nfunction identify(value) return value end\n";
    let diff = diff_after_edit(first, &format!("\n{first}"));
    expect_that!(diff.is_empty(), is_true(), "shift-only movers: {diff:?}");
}
#[gtest]
fn shift_only_blank_line_inside_body_hash_stable() {
    let first = "config = config or {}\nfunction config.describe(name)\nreturn name\nend\n";
    let second = "config = config or {}\nfunction config.describe(name)\n\nreturn name\nend\n";
    let diff = diff_after_edit(first, second);
    expect_that!(diff.is_empty(), is_true(), "shift-only movers: {diff:?}");
}

/// The `diff_after_edit` probes above re-index through the fast test-only
/// `update_file_by_uri` path; the production edit path (`update_file_text_only`
/// + `self_index_and_diff` with a validated remap over the re-indexed file)
/// runs `remap_into_file` first, so a key that survives here must also
/// survive there. A shift-only edit through that path must produce an empty
/// diff and no remap-loss dirty set.
#[gtest]
fn shift_only_edit_through_self_index_and_diff_is_empty() {
    let first = "config = config or {}\nfunction config.describe(name)\nreturn name\nend\n";
    let second = format!("\n{first}");
    let mut ws = workspace();
    let uri = ws.virtual_url_generator.new_uri("lua/shift_settle.lua");
    let file_id = write(&mut ws, &uri, first);
    ws.analysis
        .update_file_text_only(&uri, second.clone())
        .expect("staged text");
    let dirty = ws.analysis.self_index_and_diff(vec![file_id]);
    expect_that!(dirty.dirty_len(), eq(0));
    expect_that!(
        dirty.textual_refresh_names_len(),
        eq(0),
        "shift-only edit must name no refresh names"
    );
    ws.analysis.ripple(dirty);
}
/// `ContextualUnknown` support names a synthesized `DocTagClass` id per
/// `---@class` site of the inferred type (`local_inference/mod.rs`
/// `contextual_type_support`): the `DocTagClass` *kind* paired with the
/// *name-token* range. That pair exists nowhere in the syntax tree — the real
/// `DocTagClass` node spans the whole tag, and the name range itself holds a
/// `TkName` token — so `FileFacts` validation reported the mapped id lost.
/// Live CityRP observation: a one-line shift of `sh_util.lua` lost 20 files'
/// `Vector` / `Entity` facts this way. `FileFacts::collect` now admits those
/// ids by geometry (any type decl declared in the file at that range counts),
/// so the remap must carry them to their shifted image instead of `Lost`.
#[gtest]
fn shift_only_edit_moves_synthesized_doc_class_support_ids() {
    use glua_parser::{LuaKind, LuaSyntaxKind};
    let provider = "---@class Vector\nlocal Vector = {}\nreturn Vector\n";
    let shifted = format!("\n{provider}");
    let mut ws = workspace();
    let provider_uri = ws.virtual_url_generator.new_uri("lua/vector_provider.lua");
    let provider_id = write(&mut ws, &provider_uri, provider);
    // The name-token range the decl analyzer recorded for `Vector`.
    let decl_range = ws
        .analysis
        .compilation
        .get_db()
        .get_type_index()
        .get_file_type_decl_ids(provider_id)
        .expect("provider declares a type")
        .iter()
        .find_map(|decl_id| {
            ws.analysis
                .compilation
                .get_db()
                .get_type_index()
                .get_type_decl(decl_id)
                .and_then(|decl| {
                    decl.get_locations()
                        .iter()
                        .find(|location| location.file_id == provider_id)
                        .map(|location| location.range)
                })
        })
        .expect("decl location in provider");
    // Re-index under the shifted text, then remap exactly the id shape
    // `contextual_type_support` synthesizes for that site.
    ws.analysis
        .update_file_text_only(&provider_uri, shifted.clone());
    ws.analysis.compilation.remove_index(vec![provider_id]);
    ws.analysis.compilation.update_index(vec![provider_id]);
    let map = PositionMap::new(provider, &shifted);
    let remap = FileRemap::validated(ws.analysis.compilation.get_db(), provider_id, map);
    let id = glua_parser::LuaSyntaxId::new(LuaKind::Syntax(LuaSyntaxKind::DocTagClass), decl_range);
    let expected = map.map_range(decl_range).expect("a shift maps every range");
    assert_eq!(
        remap.syntax_id(provider_id, id),
        super::Remap::Moved(glua_parser::LuaSyntaxId::new(
            LuaKind::Syntax(LuaSyntaxKind::DocTagClass),
            expected,
        ))
    );
}
/// Whole-file (chunk) decl spans — the shape scoped-class synthesis
/// (`ensure_scoped_class_type_decl`) registers — never equal their shift
/// image, so the exact admission cannot see them. `FileRemap::syntax_id`
/// admits a synthesized `DocTagClass` id whose mapped range has exactly one
/// covering declaration site in the file, re-homed onto that site. Live
/// CityRP observation: a one-line shift of `cl_umsgs.lua` lost 3 files'
/// `cw_*` facts this way (`(DocTagClass, 0..3033)` → `1..3034` vs the new
/// `0..3034` site). Identity is uniqueness, never a name: a same-span
/// rename keeps one site (still moves), a deletion leaves none (still
/// `Lost`).
#[gtest]
fn shift_only_edit_rehomes_a_whole_file_doc_class_id_onto_its_unique_cover() {
    use glua_parser::{LuaKind, LuaSyntaxKind};
    use rowan::TextRange;
    // One class decl spanning the whole file, the synthesis shape.
    let provider = "local ENT = {}\nreturn ENT\n";
    let shifted = format!("\n{provider}");
    let mut ws = workspace();
    let provider_uri = ws.virtual_url_generator.new_uri("lua/synth_provider.lua");
    let provider_id = write(&mut ws, &provider_uri, provider);
    // Register a whole-file class site directly: the unit analogue of what
    // `ensure_scoped_class_type_decl` records for a scoped file.
    let whole = TextRange::new(TextSize::new(0), TextSize::new(provider.len() as u32));
    ws.analysis
        .compilation
        .get_db_mut()
        .get_type_index_mut()
        .add_type_decl(
            provider_id,
            crate::LuaTypeDecl::new(
                provider_id,
                whole,
                "SynthClass".to_string(),
                crate::LuaDeclTypeKind::Class,
                crate::LuaTypeFlag::None.into(),
                crate::LuaTypeDeclId::global("SynthClass"),
            ),
        );
    ws.analysis
        .update_file_text_only(&provider_uri, shifted.clone());
    ws.analysis.compilation.remove_index(vec![provider_id]);
    ws.analysis.compilation.update_index(vec![provider_id]);
    // Re-register the shifted whole-file site the way synthesis would.
    let shifted_whole = TextRange::new(TextSize::new(0), TextSize::new(shifted.len() as u32));
    ws.analysis
        .compilation
        .get_db_mut()
        .get_type_index_mut()
        .add_type_decl(
            provider_id,
            crate::LuaTypeDecl::new(
                provider_id,
                shifted_whole,
                "SynthClass".to_string(),
                crate::LuaDeclTypeKind::Class,
                crate::LuaTypeFlag::None.into(),
                crate::LuaTypeDeclId::global("SynthClass"),
            ),
        );
    let map = PositionMap::new(provider, &shifted);
    let remap = FileRemap::validated(ws.analysis.compilation.get_db(), provider_id, map);
    let id = glua_parser::LuaSyntaxId::new(LuaKind::Syntax(LuaSyntaxKind::DocTagClass), whole);
    assert_eq!(
        remap.syntax_id(provider_id, id),
        super::Remap::Moved(glua_parser::LuaSyntaxId::new(
            LuaKind::Syntax(LuaSyntaxKind::DocTagClass),
            shifted_whole,
        ))
    );
}

/// Deleting the covering declaration must still lose the fact: with no
/// covering site the unique-cover rule has no candidate, so the dependent is
/// re-analysed instead of being re-homed onto a guess.
#[gtest]
fn deleted_covering_decl_still_loses_the_synthesized_id() {
    use glua_parser::{LuaKind, LuaSyntaxKind};
    use rowan::TextRange;
    let provider = "local ENT = {}\nreturn ENT\n";
    // Destroys the class site while keeping a mapped image: removing the
    // table literal's braces shrinks the file, so the whole-file range maps
    // onto the shorter text, which the re-index holds no class site at.
    let second = "local ENT = nil\nreturn EN";
    assert!(second.len() < provider.len());
    let mut ws = workspace();
    let provider_uri = ws.virtual_url_generator.new_uri("lua/synth_gone.lua");
    let provider_id = write(&mut ws, &provider_uri, provider);
    let whole = TextRange::new(TextSize::new(0), TextSize::new(provider.len() as u32));
    ws.analysis
        .compilation
        .get_db_mut()
        .get_type_index_mut()
        .add_type_decl(
            provider_id,
            crate::LuaTypeDecl::new(
                provider_id,
                whole,
                "GoneClass".to_string(),
                crate::LuaDeclTypeKind::Class,
                crate::LuaTypeFlag::None.into(),
                crate::LuaTypeDeclId::global("GoneClass"),
            ),
        );
    ws.analysis
        .update_file_text_only(&provider_uri, second.to_string());
    ws.analysis.compilation.remove_index(vec![provider_id]);
    ws.analysis.compilation.update_index(vec![provider_id]);
    let map = PositionMap::new(provider, second);
    let remap = FileRemap::validated(ws.analysis.compilation.get_db(), provider_id, map);
    let id = glua_parser::LuaSyntaxId::new(LuaKind::Syntax(LuaSyntaxKind::DocTagClass), whole);
    assert!(remap.syntax_id(provider_id, id).is_lost());
}

// --- Remap ---

/// Writes `text` to `uri` without re-indexing, then re-indexes that file alone
/// and applies the remap. Returns the files the remap could not rewrite.
fn edit_and_remap(
    ws: &mut VirtualWorkspace,
    uri: &Uri,
    file_id: FileId,
    old_text: &str,
    new_text: &str,
) -> Vec<FileId> {
    ws.analysis.update_file_text_only(uri, new_text.to_string());
    ws.analysis.compilation.remove_index(vec![file_id]);
    ws.analysis.compilation.update_index(vec![file_id]);
    let map = PositionMap::new(old_text, new_text);
    let remap = FileRemap::validated(ws.analysis.compilation.get_db(), file_id, map);
    let mut dirty: Vec<FileId> = remap_into_file(ws.analysis.compilation.get_db_mut(), &remap)
        .into_iter()
        .collect();
    dirty.sort_by_key(|file_id| file_id.id);
    dirty
}

/// Every position-carrying identity a file's cached types name, as text.
fn cached_identities(db: &DbIndex, file_id: FileId) -> Vec<String> {
    let mut found = Vec::new();
    let Some(owners) = db.get_type_index().file_type_owners(file_id) else {
        return found;
    };
    for owner in owners {
        let Some(cache) = db.get_type_index().get_type_cache(owner) else {
            continue;
        };
        TypeVisitTrait::visit_type(cache.as_type(), &mut |inner| match inner {
            LuaType::Signature(id) => found.push(format!("{id:?}")),
            LuaType::TableConst(range) => found.push(format!("{range:?}")),
            _ => {}
        });
    }
    found.sort();
    found
}

/// A dependent's cache names the callee's signature by byte position, so an
/// edit above the function renames the identity without changing anything the
/// dependent reads. The remap is what keeps the two in step without
/// re-analysing the dependent.
#[gtest]
fn a_cross_file_signature_cache_follows_an_offset_shift() {
    let mut ws = workspace();
    let provider_uri = ws.virtual_url_generator.new_uri("lua/provider.lua");
    let first = "provider = provider or {}\nfunction provider.Describe() end\n";
    let second = "provider = provider or {}\nlocal padding = 1\n_ = padding\nfunction provider.Describe() end\n";
    let provider_id = write(&mut ws, &provider_uri, first);

    let consumer_uri = ws.virtual_url_generator.new_uri("lua/consumer.lua");
    let consumer_id = write(
        &mut ws,
        &consumer_uri,
        "consumer_handle = provider.Describe\n",
    );

    let before = cached_identities(ws.analysis.compilation.get_db(), consumer_id);
    expect_that!(
        before.iter().any(|id| id.contains("LuaSignatureId")),
        is_true()
    );

    edit_and_remap(&mut ws, &provider_uri, provider_id, first, second);

    let after = cached_identities(ws.analysis.compilation.get_db(), consumer_id);
    expect_that!(after, not(eq(&before)));
    // The rewritten id must be one the re-indexed provider actually holds.
    let live: Vec<String> = ws
        .analysis
        .compilation
        .get_db()
        .get_signature_index()
        .get_file_signature_ids(provider_id)
        .map(|ids| ids.iter().map(|id| format!("{id:?}")).collect())
        .unwrap_or_default();
    for id in after.iter().filter(|id| id.contains("LuaSignatureId")) {
        expect_that!(live.iter().any(|live_id| live_id == id), is_true());
    }
}

/// A collision at the old offset must not read as "unrelated": the consumer
/// names what *used* to sit there, and the re-indexed file holds a different
/// signature at exactly that offset, so the reference has to follow the
/// function to its image instead of being written off as someone else's
/// entry.
#[gtest]
fn a_cross_file_signature_cache_survives_a_collision_at_its_old_offset() {
    let mut ws = workspace();
    let provider_uri = ws.virtual_url_generator.new_uri("lua/provider.lua");
    let first = "A = 1\nfunction fa() end\nfb = function() end\n";
    // Replaces `fa` with a longer line whose own closure starts exactly where
    // `fb`'s closure used to, so after the edit a different signature
    // occupies the offset the consumer names.
    let second = "A = 1\nlocal xxxxxxxxxxxxxx = function() end\nfb = function() end\n";
    let provider_id = write(&mut ws, &provider_uri, first);

    let consumer_uri = ws.virtual_url_generator.new_uri("lua/consumer.lua");
    let consumer_id = write(&mut ws, &consumer_uri, "handle = fb\n");

    let before = cached_signature_ids(ws.analysis.compilation.get_db(), consumer_id);
    let old_id = *before
        .iter()
        .find(|id| id.get_file_id() == provider_id)
        .expect("the consumer caches the provider's signature");
    let old_pos = u32::from(old_id.get_position());
    // Fixture guard: the old offset names `fb`'s closure, and the edited text
    // puts a different closure at exactly that offset.
    expect_that!(
        first[old_pos as usize..].starts_with("function()"),
        is_true()
    );
    expect_that!(
        second[old_pos as usize..].starts_with("function()"),
        is_true()
    );

    edit_and_remap(&mut ws, &provider_uri, provider_id, first, second);

    let mapped_pos = PositionMap::new(first, second)
        .map(old_id.get_position())
        .expect("the edit maps the signature position");
    expect_that!(u32::from(mapped_pos), not(eq(old_pos)));

    let after = cached_signature_ids(ws.analysis.compilation.get_db(), consumer_id);
    expect_that!(
        after.contains(&LuaSignatureId::new(provider_id, mapped_pos)),
        is_true()
    );
    // The collision was real: the provider holds signatures at both offsets.
    let live = live_signature_positions(ws.analysis.compilation.get_db(), provider_id);
    expect_that!(live.contains(&old_pos), is_true());
    expect_that!(live.contains(&u32::from(mapped_pos)), is_true());
}

/// A same-length edit still destroys what it replaced: an identity mapping
/// onto its own offset is not evidence it survived, so the dependent must be
/// re-analysed rather than left pointing at the replacement bytes.
#[gtest]
fn a_same_length_replacement_that_destroys_its_target_dirties_the_dependent() {
    let mut ws = workspace();
    let provider_uri = ws.virtual_url_generator.new_uri("lua/provider.lua");
    let first = "A = 1\nfunction fa() end\nfb = function() end\n";
    // The comment line is exactly as long as the function it replaces.
    let second = "A = 1\n-----------------\nfb = function() end\n";
    let provider_id = write(&mut ws, &provider_uri, first);

    let consumer_uri = ws.virtual_url_generator.new_uri("lua/consumer.lua");
    let consumer_id = write(&mut ws, &consumer_uri, "handle = fa\n");
    expect_that!(
        cached_signature_ids(ws.analysis.compilation.get_db(), consumer_id)
            .iter()
            .any(|id| id.get_file_id() == provider_id),
        is_true()
    );

    let dirty = edit_and_remap(&mut ws, &provider_uri, provider_id, first, &second);

    let db = ws.analysis.compilation.get_db();
    // The signature is gone ...
    let old_pos = cached_signature_ids(db, consumer_id)
        .iter()
        .find(|id| id.get_file_id() == provider_id)
        .map(|id| u32::from(id.get_position()))
        .expect("the consumer still names the stale signature");
    expect_that!(
        live_signature_positions(db, provider_id).contains(&old_pos),
        is_false()
    );
    // ... and the dependent is dirty rather than silently kept.
    expect_that!(dirty, contains(eq(&consumer_id)));
}

/// The table's range maps onto itself under a same-length edit, which must
/// not read as "before the hunk": what the range names is gone, and value
/// equality between a range and its image proves nothing about the bytes.
#[gtest]
fn a_same_length_edit_mapping_a_table_onto_itself_still_loses_it() {
    let mut ws = workspace();
    let provider_uri = ws.virtual_url_generator.new_uri("lua/tables.lua");
    let first = "registry = { first = 1 }\n";
    // Padded to the same length, so the table literal's range is its own
    // image while the literal itself is destroyed.
    let second = "registry = nil          \n";
    let provider_id = write(&mut ws, &provider_uri, first);

    let consumer_uri = ws.virtual_url_generator.new_uri("lua/reads_tables.lua");
    let consumer_id = write(&mut ws, &consumer_uri, "handle = registry\n");
    expect_that!(
        cached_identities(ws.analysis.compilation.get_db(), consumer_id)
            .iter()
            .any(|id| id.contains("InFiled")),
        is_true()
    );

    let dirty = edit_and_remap(&mut ws, &provider_uri, provider_id, first, second);

    let db = ws.analysis.compilation.get_db();
    expect_that!(live_table_ranges(db, provider_id), is_empty());
    expect_that!(dirty, contains(eq(&consumer_id)));
}

#[gtest]
fn a_cross_file_table_cache_follows_an_offset_shift() {
    let mut ws = workspace();
    let provider_uri = ws.virtual_url_generator.new_uri("lua/tables.lua");
    let first = "registry = { first = 1 }\n";
    let second = "local padding = 1\n_ = padding\nregistry = { first = 1 }\n";
    let provider_id = write(&mut ws, &provider_uri, first);

    let consumer_uri = ws.virtual_url_generator.new_uri("lua/reads_tables.lua");
    let consumer_id = write(&mut ws, &consumer_uri, "handle = registry\n");

    let before = cached_identities(ws.analysis.compilation.get_db(), consumer_id);
    expect_that!(before.iter().any(|id| id.contains("InFiled")), is_true());

    edit_and_remap(&mut ws, &provider_uri, provider_id, first, second);

    let after = cached_identities(ws.analysis.compilation.get_db(), consumer_id);
    expect_that!(after, not(eq(&before)));
    let live = live_table_ranges(ws.analysis.compilation.get_db(), provider_id);
    for id in after.iter().filter(|id| id.contains("InFiled")) {
        expect_that!(live.iter().any(|live_id| id.contains(live_id)), is_true());
    }
}

fn live_table_ranges(db: &DbIndex, file_id: FileId) -> Vec<String> {
    use glua_parser::{LuaAstNode, LuaTableExpr};
    let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) else {
        return Vec::new();
    };
    tree.get_chunk_node()
        .descendants::<LuaTableExpr>()
        .map(|table| format!("{:?}", table.get_range()))
        .collect()
}

/// The `LuaSignatureId`s a file's cached types name, as values.
fn cached_signature_ids(db: &DbIndex, file_id: FileId) -> Vec<LuaSignatureId> {
    let mut found = Vec::new();
    let Some(owners) = db.get_type_index().file_type_owners(file_id) else {
        return found;
    };
    for owner in owners {
        let Some(cache) = db.get_type_index().get_type_cache(owner) else {
            continue;
        };
        TypeVisitTrait::visit_type(cache.as_type(), &mut |inner| {
            if let LuaType::Signature(id) = inner {
                found.push(*id);
            }
        });
    }
    found
}

fn live_signature_positions(db: &DbIndex, file_id: FileId) -> Vec<u32> {
    let mut found: Vec<u32> = db
        .get_signature_index()
        .get_file_signature_ids(file_id)
        .map(|ids| ids.iter().map(|id| u32::from(id.get_position())).collect())
        .unwrap_or_default();
    found.sort();
    found
}

/// An edit that destroys the literal a dependent's cache names cannot be
/// rewritten, so the dependent has to be re-analysed instead.
#[gtest]
fn a_destroyed_table_reports_its_dependent_as_dirty() {
    let mut ws = workspace();
    let provider_uri = ws.virtual_url_generator.new_uri("lua/tables.lua");
    let first = "registry = { first = 1 }\n";
    let second = "registry = nil\n";
    let provider_id = write(&mut ws, &provider_uri, first);

    let consumer_uri = ws.virtual_url_generator.new_uri("lua/reads_tables.lua");
    let consumer_id = write(&mut ws, &consumer_uri, "handle = registry\n");
    expect_that!(
        cached_identities(ws.analysis.compilation.get_db(), consumer_id)
            .iter()
            .any(|id| id.contains("InFiled")),
        is_true()
    );

    let dirty = edit_and_remap(&mut ws, &provider_uri, provider_id, first, second);
    expect_that!(dirty, contains(eq(&consumer_id)));
}

/// A foreign inferred return names the provider's table literal by range. An
/// insertion above it shifts that range; the stored return must follow without
/// re-analysing the consumer.
#[gtest]
fn a_foreign_inferred_return_table_follows_an_offset_shift() {
    let mut ws = workspace();
    let provider_uri = ws.virtual_url_generator.new_uri("lua/tables.lua");
    let first = "registry = { first = 1 }\n";
    let second = "local padding = 1\n_ = padding\nregistry = { first = 1 }\n";
    let provider_id = write(&mut ws, &provider_uri, first);

    let consumer_uri = ws.virtual_url_generator.new_uri("lua/fetch.lua");
    let consumer_id = write(
        &mut ws,
        &consumer_uri,
        "function fetchRegistry() return registry end\n",
    );

    // Fixture guard: the consumer owns one inferred return embedding the
    // provider's table, and no type cache names it, so only the signature
    // payload can carry the shift.
    let (consumer_sig, old_range) = {
        let db = ws.analysis.compilation.get_db();
        let consumer_sig = *db
            .get_signature_index()
            .get_file_signature_ids(consumer_id)
            .expect("consumer signatures")
            .iter()
            .next()
            .expect("consumer signature");
        let sig = db
            .get_signature_index()
            .get(&consumer_sig)
            .expect("consumer signature");
        expect_that!(sig.resolve_return, eq(SignatureReturnStatus::InferResolve));
        expect_that!(sig.return_docs.len(), eq(1));
        let LuaType::TableConst(old_range) = sig.return_docs[0].type_ref.clone() else {
            panic!(
                "consumer inferred return should embed the provider table, got {:?}",
                sig.return_docs[0].type_ref
            );
        };
        expect_that!(old_range.file_id, eq(provider_id));
        expect_that!(old_range.value, eq(range(11, 24)));
        expect_that!(
            cached_identities(db, consumer_id)
                .iter()
                .any(|id| id.contains("InFiled")),
            is_false()
        );
        (consumer_sig, old_range)
    };

    edit_and_remap(&mut ws, &provider_uri, provider_id, first, second);

    let db = ws.analysis.compilation.get_db();
    let after = db
        .get_signature_index()
        .get(&consumer_sig)
        .expect("consumer signature")
        .return_docs[0]
        .type_ref
        .clone();
    let expected_range = PositionMap::new(first, second)
        .map_range(old_range.value)
        .expect("the shift maps the table range");
    let expected = LuaType::TableConst(InFiled::new(provider_id, expected_range));
    // The remapped payload must name the live literal ...
    expect_that!(
        live_table_ranges(db, provider_id)
            .iter()
            .any(|s| s == &format!("{expected_range:?}")),
        is_true()
    );
    // ... and be exactly the remapped old identity.
    expect_that!(after, eq(&expected));
}

/// The same for a `Signature` identity: returning the provider's function
/// stores its id in the consumer's inferred return, which must follow the
/// provider's offset shift.
#[gtest]
fn a_foreign_inferred_return_signature_follows_an_offset_shift() {
    let mut ws = workspace();
    let provider_uri = ws.virtual_url_generator.new_uri("lua/provider.lua");
    let first = "provider = provider or {}\nfunction provider.Describe() end\n";
    let second = "provider = provider or {}\nlocal padding = 1\n_ = padding\nfunction provider.Describe() end\n";
    let provider_id = write(&mut ws, &provider_uri, first);

    let consumer_uri = ws.virtual_url_generator.new_uri("lua/fetch.lua");
    let consumer_id = write(
        &mut ws,
        &consumer_uri,
        "function fetchDescribe() return provider.Describe end\n",
    );

    let (consumer_sig, old_id) = {
        let db = ws.analysis.compilation.get_db();
        let consumer_sig = *db
            .get_signature_index()
            .get_file_signature_ids(consumer_id)
            .expect("consumer signatures")
            .iter()
            .next()
            .expect("consumer signature");
        let sig = db
            .get_signature_index()
            .get(&consumer_sig)
            .expect("consumer signature");
        expect_that!(sig.resolve_return, eq(SignatureReturnStatus::InferResolve));
        expect_that!(sig.return_docs.len(), eq(1));
        let LuaType::Signature(old_id) = sig.return_docs[0].type_ref.clone() else {
            panic!(
                "consumer inferred return should embed the provider signature, got {:?}",
                sig.return_docs[0].type_ref
            );
        };
        expect_that!(old_id.get_file_id(), eq(provider_id));
        expect_that!(
            db.get_signature_index()
                .get_file_signature_ids(provider_id)
                .is_some_and(|ids| ids.contains(&old_id)),
            is_true()
        );
        let old_debug = format!("{old_id:?}");
        expect_that!(
            cached_identities(db, consumer_id)
                .iter()
                .any(|id| id == &old_debug),
            is_false()
        );
        (consumer_sig, old_id)
    };

    edit_and_remap(&mut ws, &provider_uri, provider_id, first, second);

    let db = ws.analysis.compilation.get_db();
    let after = db
        .get_signature_index()
        .get(&consumer_sig)
        .expect("consumer signature")
        .return_docs[0]
        .type_ref
        .clone();
    let expected_pos = PositionMap::new(first, second)
        .map(old_id.get_position())
        .expect("the shift maps the signature position");
    let expected_id = LuaSignatureId::new(provider_id, expected_pos);
    let expected = LuaType::Signature(expected_id);
    expect_that!(
        db.get_signature_index()
            .get_file_signature_ids(provider_id)
            .is_some_and(|ids| ids.contains(&expected_id)),
        is_true()
    );
    expect_that!(after, eq(&expected));
}

/// Destroying the table a foreign inferred return names cannot be rewritten,
/// so the signature owner has to be re-analysed instead. The consumer holds
/// no type-cache reference to the table, so only signature-payload loss can
/// dirty it.
#[gtest]
fn a_destroyed_table_in_a_foreign_inferred_return_dirties_its_owner() {
    let mut ws = workspace();
    let provider_uri = ws.virtual_url_generator.new_uri("lua/tables.lua");
    let first = "registry = { first = 1 }\n";
    let second = "registry = nil\n";
    let provider_id = write(&mut ws, &provider_uri, first);

    let consumer_uri = ws.virtual_url_generator.new_uri("lua/fetch.lua");
    let consumer_id = write(
        &mut ws,
        &consumer_uri,
        "function fetchRegistry() return registry end\n",
    );

    let (consumer_sig, old_range) = {
        let db = ws.analysis.compilation.get_db();
        let consumer_sig = *db
            .get_signature_index()
            .get_file_signature_ids(consumer_id)
            .expect("consumer signatures")
            .iter()
            .next()
            .expect("consumer signature");
        let sig = db
            .get_signature_index()
            .get(&consumer_sig)
            .expect("consumer signature");
        expect_that!(sig.resolve_return, eq(SignatureReturnStatus::InferResolve));
        let LuaType::TableConst(old_range) = sig.return_docs[0].type_ref.clone() else {
            panic!(
                "consumer inferred return should embed the provider table, got {:?}",
                sig.return_docs[0].type_ref
            );
        };
        expect_that!(old_range.file_id, eq(provider_id));
        expect_that!(
            cached_identities(db, consumer_id)
                .iter()
                .any(|id| id.contains("InFiled")),
            is_false()
        );
        let deps = db
            .get_type_index()
            .files_with_type_caches_referencing_files(&std::collections::HashSet::from([
                provider_id,
            ]));
        expect_that!(deps.contains(&consumer_id), is_false());
        (consumer_sig, old_range)
    };

    let dirty = edit_and_remap(&mut ws, &provider_uri, provider_id, first, second);

    let db = ws.analysis.compilation.get_db();
    // The literal is gone ...
    expect_that!(live_table_ranges(db, provider_id), is_empty());
    // ... and the owner is dirty rather than left dangling on the old range.
    expect_that!(
        db.get_signature_index()
            .get(&consumer_sig)
            .expect("consumer signature")
            .return_docs[0]
            .type_ref
            .clone(),
        eq(&LuaType::TableConst(old_range))
    );
    expect_that!(dirty, contains(eq(&consumer_id)));
}

/// Destroying the signature a foreign inferred return names must dirty the
/// signature owner. The consumer holds no type-cache reference to it, so only
/// signature-payload loss can dirty it.
#[gtest]
fn a_destroyed_signature_in_a_foreign_inferred_return_dirties_its_owner() {
    let mut ws = workspace();
    let provider_uri = ws.virtual_url_generator.new_uri("lua/provider.lua");
    let first = "provider = provider or {}\nfunction provider.Describe() end\n";
    let second = "provider = provider or {}\nprovider.Describe = 1\n";
    let provider_id = write(&mut ws, &provider_uri, first);

    let consumer_uri = ws.virtual_url_generator.new_uri("lua/fetch.lua");
    let consumer_id = write(
        &mut ws,
        &consumer_uri,
        "function fetchDescribe() return provider.Describe end\n",
    );

    let (consumer_sig, old_id) = {
        let db = ws.analysis.compilation.get_db();
        let consumer_sig = *db
            .get_signature_index()
            .get_file_signature_ids(consumer_id)
            .expect("consumer signatures")
            .iter()
            .next()
            .expect("consumer signature");
        let sig = db
            .get_signature_index()
            .get(&consumer_sig)
            .expect("consumer signature");
        expect_that!(sig.resolve_return, eq(SignatureReturnStatus::InferResolve));
        let LuaType::Signature(old_id) = sig.return_docs[0].type_ref.clone() else {
            panic!(
                "consumer inferred return should embed the provider signature, got {:?}",
                sig.return_docs[0].type_ref
            );
        };
        expect_that!(old_id.get_file_id(), eq(provider_id));
        let old_debug = format!("{old_id:?}");
        expect_that!(
            cached_identities(db, consumer_id)
                .iter()
                .any(|id| id == &old_debug),
            is_false()
        );
        let deps = db
            .get_type_index()
            .files_with_type_caches_referencing_files(&std::collections::HashSet::from([
                provider_id,
            ]));
        expect_that!(deps.contains(&consumer_id), is_false());
        (consumer_sig, old_id)
    };

    let dirty = edit_and_remap(&mut ws, &provider_uri, provider_id, first, second);

    let db = ws.analysis.compilation.get_db();
    // The provider signature is gone ...
    expect_that!(db.get_signature_index().get(&old_id).is_none(), is_true());
    // ... and the owner is dirty rather than left dangling on it.
    expect_that!(
        db.get_signature_index()
            .get(&consumer_sig)
            .expect("consumer signature")
            .return_docs[0]
            .type_ref
            .clone(),
        eq(&LuaType::Signature(old_id))
    );
    expect_that!(dirty, contains(eq(&consumer_id)));
}

/// A `LuaTypeOwner` that no longer exists after the edit must not be written
/// back onto a position that happens to be occupied by something else.
#[gtest]
fn a_validated_remap_rejects_an_identity_the_new_index_does_not_hold() {
    let mut ws = workspace();
    let uri = ws.virtual_url_generator.new_uri("lua/subject.lua");
    let first = "provider = provider or {}\nfunction provider.Describe() end\n";
    let file_id = write(&mut ws, &uri, first);
    let signature_id = *ws
        .analysis
        .compilation
        .get_db()
        .get_signature_index()
        .get_file_signature_ids(file_id)
        .and_then(|ids| ids.iter().next())
        .expect("a signature");

    let second = "provider = provider or {}\nprovider.Describe = 1\n";
    ws.analysis.update_file_text_only(&uri, second.to_string());
    ws.analysis.compilation.remove_index(vec![file_id]);
    ws.analysis.compilation.update_index(vec![file_id]);

    let remap = FileRemap::validated(
        ws.analysis.compilation.get_db(),
        file_id,
        PositionMap::new(first, second),
    );
    expect_that!(remap.signature_id(signature_id).is_lost(), is_true());
}

#[gtest]
fn an_identity_remap_leaves_every_owner_alone() {
    let mut ws = workspace();
    let uri = ws.virtual_url_generator.new_uri("lua/subject.lua");
    let file_id = write(&mut ws, &uri, "registry = { first = 1 }\n");
    let db = ws.analysis.compilation.get_db();
    let owners: Vec<LuaTypeOwner> = db
        .get_type_index()
        .file_type_owners(file_id)
        .map(|owners| owners.iter().cloned().collect())
        .unwrap_or_default();
    let remap = FileRemap::identity(file_id);
    for owner in owners {
        expect_that!(
            matches!(remap.type_owner(&owner), super::Remap::Moved(_)),
            is_true()
        );
    }
    expect_that!(
        remap_into_file(ws.analysis.compilation.get_db_mut(), &remap),
        is_empty()
    );
}
