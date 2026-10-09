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
