use googletest::prelude::*;
use lsp_types::Uri;
use rowan::{TextRange, TextSize};

use super::{FileRemap, PositionMap, remap_into_file};
use crate::{
    DbIndex, Emmyrc, FileId, InFiled, LuaSignatureId, LuaType, LuaTypeOwner, SignatureReturnStatus,
    TypeVisitTrait, VirtualWorkspace,
};

fn range(start: u32, end: u32) -> TextRange {
    TextRange::new(TextSize::new(start), TextSize::new(end))
}

// --- PositionMap arithmetic ---

/// Texts that differ the way edits make them differ: lines inserted, removed,
/// replaced, edited in place and re-indented, from a fixed seed.
fn edited_texts(seed: u64) -> (String, String) {
    let mut state = seed;
    let mut next = |bound: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % bound
    };
    let words = [
        "local", "x", "t", "end", "function", "f(a)", "=", "{}", "1", "return", "if",
    ];
    let line = |next: &mut dyn FnMut(u64) -> u64| {
        let indent = "  ".repeat(next(3) as usize);
        let body = (0..1 + next(4))
            .map(|_| words[next(words.len() as u64) as usize])
            .collect::<Vec<_>>()
            .join(" ");
        format!("{indent}{body}\n")
    };
    let old = (0..20).map(|_| line(&mut next)).collect::<Vec<_>>();
    let mut new = Vec::new();
    for old_line in &old {
        match next(8) {
            0 => {}
            1 => new.push(line(&mut next)),
            2 => {
                new.push(line(&mut next));
                new.push(old_line.clone());
            }
            3 => new.push(old_line.replacen(' ', "  ", 1)),
            4 => new.push(format!("x{old_line}")),
            _ => new.push(old_line.clone()),
        }
    }
    (old.concat(), new.concat())
}

/// Whatever a map keeps is the same text on both sides, in order, and two
/// maps in a row compose to the map of every offset both keep.
#[gtest]
fn every_kept_offset_names_the_same_byte_and_maps_compose() {
    for seed in 1..400u64 {
        let (old, new) = edited_texts(seed * 7919);
        let (_, third) = edited_texts(seed * 104_729 + 1);
        let third = format!("{new}{third}");
        let map = PositionMap::new(&old, &new);
        let mut last = None;
        for offset in 0..=old.len() as u32 {
            let Some(image) = map.map(TextSize::new(offset)) else {
                continue;
            };
            let image = u32::from(image);
            if (offset as usize) < old.len() && (image as usize) < new.len() {
                assert_eq!(
                    old.as_bytes()[offset as usize],
                    new.as_bytes()[image as usize],
                    "seed {seed} offset {offset}: {old:?} -> {new:?}"
                );
            }
            assert!(
                last.is_none_or(|last| last < image),
                "seed {seed}: not monotonic"
            );
            last = Some(image);
        }
        let later = PositionMap::new(&new, &third);
        let both = map.then(&later);
        for offset in 0..=old.len() as u32 {
            let offset = TextSize::new(offset);
            let stepwise = map.map(offset).and_then(|moved| later.map(moved));
            assert_eq!(both.map(offset), stepwise, "seed {seed} offset {offset:?}");
        }
    }
}

#[gtest]
fn an_insertion_shifts_everything_after_it_and_loses_nothing() {
    let map = PositionMap::new("abcdef", "abcXXdef");
    expect_that!(map.hunks(), eq(&vec![(3, 3, 3, 5)]));
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
fn two_edits_in_a_row_compose_to_one_map_of_every_offset_both_keep() {
    let texts = [
        ("abcdef", "XXabcdef", "XXYYabcdef"),
        ("abcdef", "abcXXdef", "abcXXdeYf"),
        ("abcdefgh", "abgh", "aZZbgh"),
        ("abcdef", "abcXXdef", "abcdef"),
    ];
    for (first, second, third) in texts {
        let earlier = PositionMap::new(first, second);
        let later = PositionMap::new(second, third);
        let both = earlier.then(&later);
        for offset in 0..=first.len() as u32 {
            let offset = TextSize::new(offset);
            if let Some(composed) = both.map(offset) {
                expect_that!(
                    earlier.map(offset).and_then(|moved| later.map(moved)),
                    some(eq(composed)),
                    "{first:?} -> {second:?} -> {third:?} at {offset:?}"
                );
            }
        }
    }
    expect_that!(
        PositionMap::new("abcdef", "XXabcdef")
            .then(&PositionMap::new("XXabcdef", "XXYYabcdef"))
            .hunks(),
        eq(&PositionMap::new("abcdef", "XXYYabcdef").hunks())
    );
    expect_that!(
        PositionMap::identity()
            .then(&PositionMap::new("abc", "aXbc"))
            .hunks(),
        eq(&vec![(1, 1, 1, 2)])
    );
}

#[gtest]
fn a_deletion_loses_only_what_was_inside_it() {
    let map = PositionMap::new("abcXXdef", "abcdef");
    expect_that!(map.hunks(), eq(&vec![(3, 5, 3, 3)]));
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
    expect_that!(map.hunks(), eq(&vec![(3, 6, 3, 4)]));
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
    expect_that!(map.hunks(), eq(&vec![(2, 4, 2, 5)]));
    expect_that!(map.map_range(range(0, 6)), some(eq(range(0, 7))));
    expect_that!(map.map_range(range(2, 4)), some(eq(range(2, 5))));
    // One endpoint strictly inside is still lost.
    expect_that!(map.map_range(range(3, 6)), none());
    expect_that!(map.map_range(range(0, 3)), none());
}

/// An agent's rewrite adds lines at the top and parameters further down: the
/// code between the changes keeps its identity, shifted, rather than every
/// position from the first change to the last being lost.
#[gtest]
fn separate_changes_keep_the_text_between_them() {
    let old = "function f()
  return 1
end
function g()
end
";
    let new = "X = 1
function f(a)
  return 1
end
function g(b)
end
";
    let map = PositionMap::new(old, new);
    expect_that!(map.hunks().len(), eq(3));
    let body = old.find("return").unwrap() as u32;
    let moved = new.find("return").unwrap() as u32;
    expect_that!(map.map(TextSize::new(body)), some(eq(TextSize::new(moved))));
    let g = old.find("g()").unwrap() as u32;
    expect_that!(
        map.map(TextSize::new(g)),
        some(eq(TextSize::new(new.find("g(b)").unwrap() as u32)))
    );
    // The function's range takes the parameter in.
    expect_that!(
        map.map_range(range(0, old.find("end").unwrap() as u32 + 3)),
        some(eq(range(6, new.find("end").unwrap() as u32 + 3)))
    );
    expect_that!(
        map.inverse().map(TextSize::new(moved)),
        some(eq(TextSize::new(body)))
    );
}

/// Re-indenting or changing line endings changes only whitespace: every
/// token keeps its identity.
#[gtest]
fn whitespace_changes_lose_no_token() {
    let old = "if x then
	y()
end
";
    let reindented = "if x then
    y()
end
";
    let map = PositionMap::new(old, reindented);
    expect_that!(map.hunks(), eq(&vec![(10, 11, 10, 14)]));
    expect_that!(map.map(TextSize::new(11)), some(eq(TextSize::new(14))));

    let crlf = old.replace('\n', "\r\n");
    let map = PositionMap::new(old, &crlf);
    expect_that!(map.hunks().len(), eq(3));
    let end = old.find("end").unwrap() as u32;
    expect_that!(
        map.map(TextSize::new(end)),
        some(eq(TextSize::new(crlf.find("end").unwrap() as u32)))
    );
}

/// A line break inserted before a CRLF blank first line is an insertion, not
/// an edit of that line, so a range starting at the top of the file keeps its
/// start in both directions.
#[gtest]
fn a_line_inserted_before_a_crlf_blank_line_is_one_insertion() {
    let map = PositionMap::new("\r\nlocal x = 1\r\n", "\n\r\nlocal x = 1\r\n");
    expect_that!(map.hunks(), eq(&vec![(0, 0, 0, 1)]));
}

#[gtest]
fn a_hunk_boundary_never_splits_a_code_point() {
    // The two strings share the first byte of `é` (0xC3) but differ in the
    // second, so the raw byte prefix would end mid-character.
    let map = PositionMap::new("aé", "aè");
    let (start, old_end, _, new_end) = map.hunks()[0];
    expect_that!(start, eq(1));
    expect_that!(old_end, eq(3));
    expect_that!(new_end, eq(3));
    expect_that!("aé".is_char_boundary(start as usize), is_true());
    expect_that!("aè".is_char_boundary(start as usize), is_true());

    // A trailing multibyte character shared by both sides must stay whole in
    // the suffix too.
    let map = PositionMap::new("aXé", "aYYé");
    let (start, old_end, _, new_end) = map.hunks()[0];
    expect_that!(start, eq(1));
    expect_that!("aXé".is_char_boundary(old_end as usize), is_true());
    expect_that!("aYYé".is_char_boundary(new_end as usize), is_true());
    expect_that!(
        map.map(TextSize::new(old_end)),
        some(eq(TextSize::new(new_end)))
    );
}

/// A thread's fingerprint maps the edited positions back by its own edits,
/// however another thread fingerprinting at the same time finishes.
#[test]
fn a_fingerprint_maps_positions_back_while_another_thread_finishes_its_own() {
    use crate::db_index::derivation::{EditMaps, fingerprint_before_edits, fingerprint_of};
    use std::sync::Barrier;

    let file_id = FileId::new(7);
    let edits = EditMaps::new([(
        file_id,
        PositionMap::new(
            "abc", "
abc",
        ),
    )]);
    let before_edit = fingerprint_of(&crate::LuaDeclId::new(file_id, TextSize::new(0)));
    let both_mapping = Barrier::new(2);
    let other_finished = Barrier::new(2);

    let mapped = std::thread::scope(|scope| {
        scope.spawn(|| {
            fingerprint_before_edits(&edits, || {
                both_mapping.wait();
                1
            });
            other_finished.wait();
        });
        fingerprint_before_edits(&edits, || {
            both_mapping.wait();
            other_finished.wait();
            fingerprint_of(&crate::LuaDeclId::new(file_id, TextSize::new(1)))
        })
    });

    assert_eq!(mapped, before_edit);
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
        !edit_and_reevaluated(&mut ws, &provider_uri, provider_id, &second).contains(&consumer_id)
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

    edit_and_reevaluated(&mut ws, &provider_uri, provider_id, first);
    assert_eq!(inherited_member(&ws, consumer_id).unwrap(), before);
    write(&mut ws, &provider_uri, first);
    assert_eq!(inherited_member(&ws, consumer_id).unwrap(), before);
    ws.analysis.update_file_by_uri(&provider_uri, None);
    assert!(inherited_member(&ws, consumer_id).is_err());
}

/// A panel one file creates by name and another registers is declared by
/// what both files say, not by whether the registration had run when the
/// creating file was last analysed.
#[test]
fn a_panel_another_file_registers_keeps_its_declaration_when_the_creating_file_is_reindexed() {
    const MENU: &str = "cityrp = cityrp or {}
cityrp.menu = cityrp.menu or {}
function cityrp.menu.populateNoChar()
    cityrp.menu.panel:AddSheet(\"Guides\", vgui.Create(\"cityrp_guides\"), \"icon\")
end
function cityrp.menu.populate()
    cityrp.menu.panel:AddSheet(\"Guides\", vgui.Create(\"cityrp_guides\"), \"icon\")
end
";
    let mut ws = VirtualWorkspace::new();
    let mut emmyrc = Emmyrc::default();
    emmyrc.gmod.enabled = true;
    ws.update_emmyrc(emmyrc);
    let menu_uri = ws
        .virtual_url_generator
        .new_uri("gamemodes/cityrp/gamemode/core/derma/cl_menu.lua");
    ws.def_files(vec![
        (
            "lua/includes/builtins.lua",
            crate::GMOD_CALL_ARG_BUILTINS_FIXTURE,
        ),
        (
            "lua/includes/panels.lua",
            "---@class Panel
---@class DPanel: Panel
",
        ),
        (
            "gamemodes/cityrp/gamemode/core/derma/cl_guides.lua",
            "local PANEL = {}
vgui.Register(\"cityrp_guides\", PANEL, \"Panel\")
",
        ),
        ("gamemodes/cityrp/gamemode/core/derma/cl_menu.lua", MENU),
    ]);
    let declaration = |ws: &VirtualWorkspace| {
        let index = ws.analysis.compilation.get_db().get_type_index();
        let id = crate::LuaTypeDeclId::global("cityrp_guides");
        let mut locations = index
            .get_type_decl(&id)
            .map(|decl| {
                decl.get_locations()
                    .iter()
                    .map(|location| format!("{location:?}"))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        locations.sort();
        let mut supers = index
            .get_super_types(&id)
            .unwrap_or_default()
            .iter()
            .map(|super_type| format!("{super_type:?}"))
            .collect::<Vec<_>>();
        supers.sort();
        (locations, supers)
    };
    let cold = declaration(&ws);
    let menu_id = ws.analysis.get_file_id(&menu_uri).expect("menu file");

    edit_and_reevaluated(&mut ws, &menu_uri, menu_id, MENU);

    assert_eq!(declaration(&ws), cold);
}

/// Deleting a file retracts what it contributed to other files' facts: a
/// class member read through its subclasses' writes loses the deleted
/// subclass's table, as a cold build without that file never had it.
#[test]
fn deleting_a_subclass_file_leaves_what_a_cold_build_without_it_does() {
    const BASE: (&str, &str) = (
        "lua/entities/base_car/shared.lua",
        "ENT.Type = \"anim\"
ENT.Base = \"base_anim\"
ENT.FuelDefaults = ENT.FuelDefaults or { capacity = 60 }
",
    );
    const MOTORCYCLE: (&str, &str) = (
        "lua/entities/base_motorcycle/shared.lua",
        "ENT.Type = \"anim\"
ENT.Base = \"base_car\"
ENT.FuelDefaults = ENT.FuelDefaults or { capacity = 10 }
",
    );
    const DELETED: (&str, &str) = (
        "lua/entities/car_deleted.lua",
        "ENT.Type = \"anim\"
ENT.Base = \"base_car\"
ENT.FuelDefaults = { capacity = 50, fillPort = 1 }
",
    );
    fn gmod_workspace(files: &[(&str, &str)]) -> VirtualWorkspace {
        let mut ws = workspace();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        for (path, text) in files {
            let uri = ws.virtual_url_generator.new_uri(path);
            write(&mut ws, &uri, text);
        }
        ws
    }
    fn settled(ws: &VirtualWorkspace) -> Vec<String> {
        let db = ws.analysis.compilation.get_db();
        let mut settled = db
            .get_type_index()
            .iter_type_caches()
            .map(|(owner, cache)| format!("{owner:?} = {:?}", cache.as_type()))
            .chain(
                db.get_dynamic_field_index()
                    .iter_all_field_definitions()
                    .flat_map(|(owner, fields)| {
                        fields
                            .keys()
                            .map(move |field| format!("{owner:?} has {field}"))
                    }),
            )
            .collect::<Vec<_>>();
        settled.sort();
        settled
    }

    let mut warm = gmod_workspace(&[BASE, MOTORCYCLE, DELETED]);
    let deleted = warm.virtual_url_generator.new_uri(DELETED.0);
    warm.analysis.update_file_by_uri(&deleted, None);
    let cold = gmod_workspace(&[BASE, MOTORCYCLE]);

    let warm = settled(&warm);
    let cold = settled(&cold);
    let only_warm = warm
        .iter()
        .filter(|line| !cold.contains(line))
        .collect::<Vec<_>>();
    let only_cold = cold
        .iter()
        .filter(|line| !warm.contains(line))
        .collect::<Vec<_>>();
    assert!(
        only_warm.is_empty() && only_cold.is_empty(),
        "warm only: {only_warm:#?}
cold only: {only_cold:#?}"
    );
}

/// A shift-only edit through the production edit path (`update_file_text_only`
/// + `reindex_files`) must report only the edited file: a diagnosed reader of
/// its exports reads nothing that moved.
#[gtest]
fn shift_only_edit_through_self_index_and_diff_is_empty() {
    let first = "config = config or {}\nfunction config.describe(name)\nreturn name\nend\n";
    let second = format!("\n{first}");
    let mut ws = workspace();
    let uri = ws.virtual_url_generator.new_uri("lua/shift_settle.lua");
    let file_id = write(&mut ws, &uri, first);
    let reader_uri = ws.virtual_url_generator.new_uri("lua/shift_reader.lua");
    let reader_id = write(
        &mut ws,
        &reader_uri,
        "local described = config.describe(\"name\")\n",
    );
    ws.analysis
        .diagnose_file(reader_id, tokio_util::sync::CancellationToken::new())
        .expect("reader diagnostics");
    ws.analysis
        .update_file_text_only(&uri, second.clone())
        .expect("staged text");
    let affected = ws.analysis.reindex_files(vec![file_id]);
    expect_that!(affected, eq(&vec![file_id]));
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
    let expected = map.map_range(decl_range).expect("a shift maps every range");
    let remap = FileRemap::validated(ws.analysis.compilation.get_db(), provider_id, map);
    let id = glua_parser::LuaSyntaxId::new(LuaKind::Syntax(LuaSyntaxKind::DocTagClass), decl_range);
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

/// Writes `new_text` over `uri` and re-indexes it through the ordinary
/// path. Returns the other files whose derivations had to be evaluated
/// again.
fn edit_and_reevaluated(
    ws: &mut VirtualWorkspace,
    uri: &Uri,
    file_id: FileId,
    new_text: &str,
) -> Vec<FileId> {
    ws.analysis.update_file_text_only(uri, new_text.to_string());
    ws.analysis.reindex_files(vec![file_id]);
    let mut evaluated = ws
        .analysis
        .compilation
        .last_evaluated_files()
        .iter()
        .copied()
        .filter(|evaluated| *evaluated != file_id)
        .collect::<Vec<_>>();
    evaluated.sort_by_key(|file_id| file_id.id);
    evaluated
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

    edit_and_reevaluated(&mut ws, &provider_uri, provider_id, second);

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

    edit_and_reevaluated(&mut ws, &provider_uri, provider_id, second);

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

/// The edited file's own types are written at the new positions by its
/// re-indexed source layer, so remapping them again would shift them twice;
/// here the second shift lands `fa` exactly on `fb`'s closure.
#[gtest]
fn an_edited_files_own_signature_types_shift_once() {
    let first = "function fa(x) end\nfunction fb(y, z) end\n";
    let second = "local pad = 123456\nfunction fa(x) end\nfunction fb(y, z) end\n";
    // Fixture guard: the inserted line is as long as `fa`'s, so a second shift
    // moves `fa`'s closure onto `fb`'s.
    expect_that!(second.find('\n'), eq(first.find('\n')));

    let mut ws = workspace();
    let uri = ws.virtual_url_generator.new_uri("lua/provider.lua");
    let file_id = write(&mut ws, &uri, first);
    edit_and_reevaluated(&mut ws, &uri, file_id, second);

    let mut cold = workspace();
    let cold_uri = cold.virtual_url_generator.new_uri("lua/provider.lua");
    let cold_id = write(&mut cold, &cold_uri, second);

    let positions = |db: &DbIndex, file_id| {
        let mut found = cached_signature_ids(db, file_id)
            .iter()
            .map(|id| u32::from(id.get_position()))
            .collect::<Vec<_>>();
        found.sort();
        found
    };
    expect_that!(
        positions(ws.analysis.compilation.get_db(), file_id),
        eq(&positions(cold.analysis.compilation.get_db(), cold_id))
    );
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

    let dirty = edit_and_reevaluated(&mut ws, &provider_uri, provider_id, &second);

    let db = ws.analysis.compilation.get_db();
    // The dependent was re-derived and names no signature the edit destroyed.
    let live = live_signature_positions(db, provider_id);
    expect_that!(
        cached_signature_ids(db, consumer_id)
            .iter()
            .filter(|id| id.get_file_id() == provider_id)
            .all(|id| live.contains(&u32::from(id.get_position()))),
        is_true()
    );
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

    let dirty = edit_and_reevaluated(&mut ws, &provider_uri, provider_id, second);

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

    edit_and_reevaluated(&mut ws, &provider_uri, provider_id, second);

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

    let dirty = edit_and_reevaluated(&mut ws, &provider_uri, provider_id, second);
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

    edit_and_reevaluated(&mut ws, &provider_uri, provider_id, second);

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

    edit_and_reevaluated(&mut ws, &provider_uri, provider_id, second);

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

    let dirty = edit_and_reevaluated(&mut ws, &provider_uri, provider_id, second);

    let db = ws.analysis.compilation.get_db();
    // The literal is gone ...
    expect_that!(live_table_ranges(db, provider_id), is_empty());
    // ... and the owner was re-derived rather than left dangling on the old
    // range.
    expect_that!(
        db.get_signature_index()
            .get(&consumer_sig)
            .expect("consumer signature")
            .return_docs[0]
            .type_ref
            .clone(),
        not(eq(&LuaType::TableConst(old_range)))
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

    let dirty = edit_and_reevaluated(&mut ws, &provider_uri, provider_id, second);

    let db = ws.analysis.compilation.get_db();
    // The provider signature is gone ...
    expect_that!(db.get_signature_index().get(&old_id).is_none(), is_true());
    // ... and the owner was re-derived rather than left dangling on it.
    expect_that!(
        db.get_signature_index()
            .get(&consumer_sig)
            .expect("consumer signature")
            .return_docs[0]
            .type_ref
            .clone(),
        not(eq(&LuaType::Signature(old_id)))
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

const SHIFT_FIXTURE: [(&str, &str); 8] = [
    (
        "lua/autorun/sh_shift_lib.lua",
        r#"---@class ShiftLib
ShiftLib = ShiftLib or {}
ShiftLib.Items = {}
local Meta = {}
Meta.__index = Meta
Meta.__add = function(a, b) return a end
---@param name string
---@return table
function ShiftLib.New(name)
    local obj = setmetatable({ name = name }, Meta)
    return obj
end
function Meta:GetName() return self.name end
if SERVER then
    util.AddNetworkString("shift_msg")
    function ShiftLib.Send(ply)
        net.Start("shift_msg")
        net.WriteString("x")
        net.Send(ply)
    end
else
    net.Receive("shift_msg", function()
        local s = net.ReadString()
        print(s)
    end)
end
hook.Add("Think", "shift_think", function() end)
for i = 1, 3 do ShiftLib.Items[i] = i end
include("vgui/shift_panel.lua")
"#,
    ),
    (
        "lua/vgui/shift_panel.lua",
        r#"local PANEL = {}
AccessorFunc(PANEL, "m_Value", "Value")
function PANEL:Init() self.label = "x" end
vgui.Register("ShiftPanel", PANEL, "DPanel")
"#,
    ),
    (
        "lua/autorun/client/cl_shift_reader.lua",
        r#"local lib = ShiftLib.New("a")
print(lib:GetName())
local panel = vgui.Create("ShiftPanel")
panel:SetValue(1)
---@diagnostic disable-next-line: undefined-global
print(UNDEFINED_SHIFT)
local sum = lib + lib
print(ShiftLib.Items[1], sum)
local animal = ShiftAnimal.new(4)
print(animal.legs, ShiftAnimal.Make("cat"), ShiftCounts[2])
local total = ShiftVec + ShiftVec
print(total, ShiftVec(1), shiftmod.value)
---@type shift_ent
local ent
print(ent:GetSpeed(), ent.Speed, ent.IsShiftVehicle, ent:GetDriver())
---@type ShiftTrace
local trace
print(trace.Entity)
---@type ShiftBag
local bag
print(bag.size, bag.weight)
local ns = require("lua.shiftns")
"#,
    ),
    (
        "lua/autorun/sh_shift_more.lua",
        r#"---@class ShiftAnimal
---@field legs integer
ShiftAnimal = {}
ShiftAnimal.__index = ShiftAnimal
function ShiftAnimal.new(legs)
    return setmetatable({ legs = legs }, ShiftAnimal)
end
---@param kind "cat"|"dog"
---@return string
function ShiftAnimal.Make(kind) return kind end
ShiftVec = {}
local VecMeta = {}
VecMeta.__add = function(a, b) return a end
VecMeta.__call = function(self, x) return x end
setmetatable(ShiftVec, VecMeta)
ShiftCounts = {}
for i = 1, 10 do ShiftCounts[i] = i * 2 end
local mode = "fast"
ShiftMode = mode
local claimed
claimed = ShiftCounts[1]
claimed = ShiftMode
ShiftClaimed = claimed
---@class ShiftTrace
---@field Entity table
---@param trace ShiftTrace
function ShiftSwap(trace)
    trace.Entity = trace.Entity.Owner
end
---@class ShiftBag
local ShiftBag = {}
ShiftBag.size = 1
"#,
    ),
    (
        "lua/entities/shift_base/shared.lua",
        r#"ENT.Type = "anim"
ENT.Base = "base_anim"
ENT.IsShiftVehicle = true
function ENT:GetDriver() return nil end
"#,
    ),
    (
        "lua/entities/shift_ent/shared.lua",
        r#"ENT.Type = "anim"
ENT.Base = "shift_base"
ENT.Speed = 1
function ENT:GetSpeed() return self.Speed end
"#,
    ),
    (
        "lua/includes/modules/shiftmod.lua",
        r#"module("shiftmod", package.seeall)
value = 1
function get() return value end
"#,
    ),
    (
        "lua/shiftns.lua",
        r#"---@namespace ShiftNs

---@class ShiftNs.Thing
local Thing = {}

---@export namespace
local M = { Thing = Thing }
return M
"#,
    ),
];

/// A pure shift, or a comment appended at the end, moves positions and
/// nothing else, so every fact must fingerprint, with the edit mapped back,
/// exactly as it did before: the shifted file's own facts and the facts
/// elsewhere that hold its positions. A fact that does not re-runs its
/// readers in other files on every such edit.
#[gtest]
fn every_fact_fingerprints_as_before_a_shift_or_an_appended_comment() {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    let mut emmyrc = Emmyrc::default();
    emmyrc.gmod.enabled = true;
    ws.update_emmyrc(emmyrc);
    ws.def_gmod_call_arg_builtins();
    ws.def_files(SHIFT_FIXTURE.to_vec());

    let mut unstable = std::collections::BTreeSet::new();
    for (path, text) in SHIFT_FIXTURE {
        let uri = ws.virtual_url_generator.new_uri(path);
        let file_id = ws.analysis.get_file_id(&uri).expect("fixture file");
        let shifted = format!(
            "
{text}"
        );
        let touched = format!(
            "{shifted}-- touched
"
        );
        for (before_text, after_text) in [(text, shifted.as_str()), (shifted.as_str(), &touched)] {
            for (other, _) in SHIFT_FIXTURE {
                let other = ws.virtual_url_generator.new_uri(other);
                let other = ws.analysis.get_file_id(&other).expect("fixture file");
                ws.analysis
                    .diagnose_file(other, tokio_util::sync::CancellationToken::new());
            }
            let diagnostic_keys = ws.analysis.compilation.diagnostic_read_keys();
            let db = ws.analysis.compilation.get_db();
            let before = db
                .reader_graph()
                .read_keys()
                .chain(diagnostic_keys.iter())
                .filter(|key| {
                    !matches!(key, crate::db_index::derivation::FactKey::FileText(text_file) if *text_file == file_id)
                })
                .map(|key| (key.clone(), db.fact_fingerprint(key)))
                .collect::<Vec<_>>();

            ws.analysis
                .update_file_text_only(&uri, after_text.to_string());
            ws.analysis.reindex_files(vec![file_id]);

            let edits = crate::db_index::derivation::EditMaps::new([(
                file_id,
                PositionMap::new(before_text, after_text),
            )]);
            let db = ws.analysis.compilation.get_db();
            for (key, fingerprint) in before {
                let moved = edits.map_key(&key).expect("the edit loses no position");
                let after = crate::db_index::derivation::fingerprint_before_edits(&edits, || {
                    db.fact_fingerprint(&moved)
                });
                if after != fingerprint {
                    unstable.insert(format!("{path}: {key:?}"));
                }
            }
        }
    }
    expect_that!(unstable, is_empty());
}

/// A member filed under a key whose inferred type names another file's table
/// literals keeps naming them when that file shifts, as a cold build of the
/// shifted text does.
#[gtest]
fn a_member_keyed_by_another_files_tables_follows_that_file_shifting() {
    const CONFIG: &str = "lua/cfg.lua";
    const CONFIG_TEXT: &str = "cfg = {}\ncfg.models = { { model = \"a\", skin = 1, groups = { torso = 1 } }, { model = \"b\", skin = 2, groups = { torso = 2 } } }\n";
    const USER: &str = "lua/user.lua";
    const USER_TEXT: &str = "optimised = {}\nfor _, data in pairs(cfg.models) do\n    optimised[data.skin > 0 and data or data.model] = data\nend\n";
    let member_keys = |ws: &VirtualWorkspace, file_id: FileId| {
        let mut keys = ws
            .analysis
            .compilation
            .get_db()
            .get_member_index()
            .get_file_members(file_id)
            .into_iter()
            .map(|member| format!("{:?}", member.get_key()))
            .collect::<Vec<_>>();
        keys.sort();
        keys
    };
    let shifted = format!("\n{CONFIG_TEXT}");
    let mut warm = workspace();
    let config_uri = warm.virtual_url_generator.new_uri(CONFIG);
    write(&mut warm, &config_uri, CONFIG_TEXT);
    let user_uri = warm.virtual_url_generator.new_uri(USER);
    let user = write(&mut warm, &user_uri, USER_TEXT);
    let mut cold = workspace();
    let cold_config = cold.virtual_url_generator.new_uri(CONFIG);
    write(&mut cold, &cold_config, &shifted);
    let cold_user_uri = cold.virtual_url_generator.new_uri(USER);
    let cold_user = write(&mut cold, &cold_user_uri, USER_TEXT);

    write(&mut warm, &config_uri, &shifted);

    expect_that!(member_keys(&warm, user), eq(&member_keys(&cold, cold_user)));
}
