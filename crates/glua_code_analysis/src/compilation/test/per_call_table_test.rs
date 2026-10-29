#![cfg(test)]

use crate::{DiagnosticCode, VirtualWorkspace};

const FACTORY_PATH: &str = "lua/autorun/factory.lua";
const WRITER_PATH: &str = "lua/autorun/writer.lua";
const READER_PATH: &str = "lua/autorun/reader.lua";

const FACTORY: &str = r#"
    function MakeOne()
        return {}
    end
"#;

const WRITER: &str = r#"
    local made = MakeOne()
    function made:Ping() return 1 end
"#;

const READER: &str = r#"
    local made = MakeOne()
    made:Ping()
"#;

fn undefined_method_free(ws: &mut VirtualWorkspace, path: &str, text: &str) -> bool {
    ws.check_file_for(DiagnosticCode::UndefinedMethod, path, text)
}

#[test]
fn a_table_a_function_builds_per_call_is_not_shared_between_its_callers() {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    ws.def_file(FACTORY_PATH, FACTORY);
    ws.def_file(WRITER_PATH, WRITER);
    assert!(!undefined_method_free(&mut ws, READER_PATH, READER));
}

#[test]
fn a_table_a_caller_received_keeps_the_members_its_own_file_writes() {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    ws.def_file(FACTORY_PATH, FACTORY);
    assert!(undefined_method_free(
        &mut ws,
        WRITER_PATH,
        &format!("{WRITER}\nmade:Ping()")
    ));
}

#[test]
fn a_table_built_into_a_local_and_returned_is_one_per_call() {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    ws.def_file(
        FACTORY_PATH,
        r#"
        function MakeOne()
            local made = {}
            made.Created = true
            return made
        end
        "#,
    );
    ws.def_file(WRITER_PATH, WRITER);
    assert!(!undefined_method_free(&mut ws, READER_PATH, READER));
}

#[test]
fn a_table_built_in_a_nil_guarded_return_is_one_per_call() {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    ws.def_file(
        FACTORY_PATH,
        r#"
        function MakeOne(name)
            if not name then return nil end
            return {}
        end
        "#,
    );
    ws.def_file(WRITER_PATH, WRITER);
    assert!(!undefined_method_free(
        &mut ws,
        READER_PATH,
        "local made = MakeOne('a')\nmade:Ping()"
    ));
}

#[test]
fn the_functions_own_methods_see_what_every_caller_installed() {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    ws.def_file(WRITER_PATH, WRITER);
    ws.def_file(
        "lua/autorun/other_writer.lua",
        "local made = MakeOne()\nfunction made:Pong() return 2 end",
    );
    assert!(undefined_method_free(
        &mut ws,
        FACTORY_PATH,
        r#"
        function MakeOne()
            local made = {}
            function made:Run() return self:Ping() + self:Pong() end
            return made
        end
        "#,
    ));
}

#[test]
fn a_module_table_a_getter_returns_stays_shared() {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    ws.def_file(
        FACTORY_PATH,
        r#"
        local M = {}
        function GetModule() return M end
        "#,
    );
    ws.def_file(
        WRITER_PATH,
        "local m = GetModule()\nfunction m:Ping() return 1 end",
    );
    assert!(undefined_method_free(
        &mut ws,
        READER_PATH,
        "local m = GetModule()\nm:Ping()"
    ));
}

#[test]
fn tables_returned_beside_other_values_stay_shared() {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    ws.def_file(
        FACTORY_PATH,
        r#"
        function MakeTwo() return {}, {} end
        "#,
    );
    ws.def_file(
        WRITER_PATH,
        "local first = MakeTwo()\nfunction first:Ping() return 1 end",
    );
    assert!(undefined_method_free(
        &mut ws,
        READER_PATH,
        "local first = MakeTwo()\nfirst:Ping()"
    ));
}

#[test]
fn a_table_that_may_start_as_a_copy_of_something_unresolved_stays_shared() {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    ws.def_file(
        FACTORY_PATH,
        r#"
        REGISTRY = REGISTRY or {}
        function MakeOne(name)
            local made
            if REGISTRY[name] then
                made = CopyOf(REGISTRY[name])
            else
                made = {}
            end
            return made
        end
        "#,
    );
    ws.def_file(WRITER_PATH, WRITER);
    assert!(undefined_method_free(&mut ws, READER_PATH, READER));
}

#[test]
fn a_table_inheriting_through_a_metatable_keeps_its_base() {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    ws.def_file(
        FACTORY_PATH,
        r#"
        BASE = {}
        BASE.__index = BASE
        function BASE:Greet() return 1 end
        function MakeOne()
            local made = {}
            setmetatable(made, BASE)
            return made
        end
        "#,
    );
    assert!(undefined_method_free(
        &mut ws,
        "lua/autorun/inherit_reader.lua",
        "local made = MakeOne()
made:Greet()",
    ));
}

const COPY_API: &str = r#"
    ---@generic T : table
    ---@param source T
    ---@return T
    ---@[copy_result]
    function CopyTable(source) end
"#;

const TEMPLATE: &str = r#"
    TEMPLATE = {}
    function TEMPLATE:Base() return 1 end
"#;

#[test]
fn writes_to_a_copy_reach_neither_the_original_nor_other_copies() {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    ws.def_file("lua/autorun/api.lua", COPY_API);
    ws.def_file(FACTORY_PATH, TEMPLATE);
    ws.def_file(
        WRITER_PATH,
        "local copy = CopyTable(TEMPLATE)\nfunction copy:Extra() return 1 end",
    );
    assert!(!undefined_method_free(
        &mut ws,
        READER_PATH,
        "TEMPLATE:Extra()"
    ));
    assert!(!undefined_method_free(
        &mut ws,
        "lua/autorun/other_reader.lua",
        "local copy = CopyTable(TEMPLATE)\ncopy:Extra()",
    ));
}

#[test]
fn a_copy_has_the_members_of_the_table_it_copies() {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    ws.def_file("lua/autorun/api.lua", COPY_API);
    ws.def_file(FACTORY_PATH, TEMPLATE);
    assert!(undefined_method_free(
        &mut ws,
        READER_PATH,
        "local copy = CopyTable(TEMPLATE)\nfunction copy:Extra() return 1 end\ncopy:Base()\ncopy:Extra()",
    ));
}

#[test]
fn per_call_tables_survive_editing_deleting_and_reopening_the_factory() {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    ws.def_file(FACTORY_PATH, FACTORY);
    ws.def_file(WRITER_PATH, WRITER);
    assert!(!undefined_method_free(&mut ws, READER_PATH, READER));

    ws.def_file(FACTORY_PATH, &format!("-- edited\n{FACTORY}"));
    assert!(!undefined_method_free(&mut ws, READER_PATH, READER));

    let uri = ws.virtual_url_generator.new_uri(FACTORY_PATH);
    ws.analysis.update_file_by_uri(&uri, None);
    ws.def_file(FACTORY_PATH, FACTORY);
    assert!(!undefined_method_free(&mut ws, READER_PATH, READER));
}

#[test]
fn editing_the_factory_matches_a_cold_analysis() {
    VirtualWorkspace::assert_warm_matches_cold(
        |_| {},
        &[
            (FACTORY_PATH, FACTORY),
            (WRITER_PATH, WRITER),
            (READER_PATH, READER),
        ],
        &[(
            FACTORY_PATH,
            "-- edited\nfunction MakeOne() local made = {} return made end",
        )],
    );
}
