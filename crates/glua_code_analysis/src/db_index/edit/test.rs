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
        .expect("file id")
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
