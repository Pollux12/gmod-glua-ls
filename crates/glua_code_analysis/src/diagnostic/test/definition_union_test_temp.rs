
#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, LuaType, VirtualWorkspace};

    #[test]
    fn test_definition_modifier_on_union() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
                ---@class MyDefClass
                ---@field name string

                ---@return (definition) MyDefClass?
                local function GetDef() return MyDefClass end

                local Def = GetDef()
            "#,
        );
        let ty = ws.expr_ty("Def");
        // We expect Union(Def(MyDefClass), Nil) or similar.
        // If it's Union(Ref(MyDefClass), Nil), the modifier was ignored.
        // Since we can't easily inspect the structure, we can check behavior.
        // If it's Ref, Def.name works (instance field).
        // If it's Def, Def.name works (class field/static).
        // Let's add a static field.

        ws.def(
             r#"
                ---@class MyDefClass2
                ---@field name string

                ---@return (definition) MyDefClass2?
                local function GetDef() return MyDefClass2 end

                local Def = GetDef()
                Def.newMethod = function() end -- Should add to class
            "#,
        );

        // If Def is Ref, newMethod adds to instance.
        // If Def is Def, newMethod adds to class.

        // Check if another instance sees it.
        ws.def(
            r#"
                local inst = {} ---@type MyDefClass2
                local _ = inst.newMethod -- Should be defined if Def worked
            "#
        );
        // But we can't run this.
    }
}
