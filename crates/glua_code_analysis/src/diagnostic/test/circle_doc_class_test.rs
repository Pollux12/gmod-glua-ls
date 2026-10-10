#![cfg(test)]

use crate::{DiagnosticCode, VirtualWorkspace};

#[test]
fn raw_circular_inheritance_reports_diagnostic() {
    let mut ws = VirtualWorkspace::new();

    assert!(!ws.check_code_for(
        DiagnosticCode::CircleDocClass,
        r#"
        ---@class First: Second
        ---@class Second: First
        "#,
    ));
}

#[test]
fn private_namespaced_class_ignores_cyclic_global_with_same_name() {
    let mut ws = VirtualWorkspace::new();
    ws.def(
        r#"
        ---@class Shared: Other
        ---@class Other: Shared
        "#,
    );

    assert!(ws.check_code_for(
        DiagnosticCode::CircleDocClass,
        r#"
        ---@namespace Circle
        ---@class (private) Shared
        "#,
    ));
}

#[test]
fn private_namespaced_cycle_reports_diagnostic() {
    let mut ws = VirtualWorkspace::new();

    assert!(!ws.check_code_for(
        DiagnosticCode::CircleDocClass,
        r#"
        ---@namespace Circle
        ---@class (private) First: Second
        ---@class (private) Second: First
        "#,
    ));
}

#[test]
fn private_namespaced_class_resolves_as_param_type_in_own_file_only() {
    let mut ws = VirtualWorkspace::new();
    let own = ws.def_file(
        "addons/a/lua/a.lua",
        r#"
        ---@namespace Ns
        ---@class (private) Hidden
        ---@param value Hidden
        function Use(value) end
        "#,
    );
    let other = ws.def_file(
        "addons/b/lua/b.lua",
        r#"
        ---@namespace Ns
        ---@param value Hidden
        function UseOther(value) end
        "#,
    );
    let own_id = crate::LuaTypeDeclId::local(own, "Ns.Hidden");
    let index = ws.analysis.compilation.get_db().get_type_index();
    assert_eq!(
        index.find_type_decl(own, "Hidden").map(|d| d.get_id()),
        Some(own_id)
    );
    assert!(index.find_type_decl(other, "Hidden").is_none());
}
