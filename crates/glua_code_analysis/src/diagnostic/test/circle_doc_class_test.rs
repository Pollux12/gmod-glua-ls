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
