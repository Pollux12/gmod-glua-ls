#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, VirtualWorkspace};
    use googletest::prelude::*;

    #[test]
    fn test_issue_245() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
        local a --- @type table?
        local _ = (a and a.type == 'change') and a.field
        "#
        ));
    }
    #[test]
    fn test_issue_402() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class A
            local a = {}

            ---@param self table?
            function a.new(self)
                if self then
                    self.a = 1
                end
            end
        "#
        ));
    }

    #[test]
    fn test_issue_474() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@class Range4
            ---@class TSNode: userdata
            ---@field range fun(self: TSNode): Range4

            ---@param node_or_range TSNode|Range4
            ---@return Range4
            function foo(node_or_range)
                if type(node_or_range) == 'table' then
                    return node_or_range
                else
                    return node_or_range:range()
                end
            end
            "#
        ));
    }

    #[test]
    fn test_cast() {
        let mut ws = VirtualWorkspace::new();
        ws.def(
            r#"
            ---@class Cast1
            ---@field get fun(self: self, a: number): Cast1?
            ---@field get2 fun(self: self, a: number): Cast1?
        "#,
        );
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
                ---@type Cast1
                local A

                local a = A:get(1) --[[@cast -?]]
                    :get2(2)
            "#
        ));
    }

    #[test]
    fn test_issue_895_891() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
        local t = {
        123,
        234,
        345,
        }

        ---@param id number
        function test(id) end

        for i = 1, #t do
            test(t[i]) -- expected 'number' but found (123|234|345)?
        end
        "#,
        ));
    }

    #[test]
    fn test_issue_886() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
        ---@type string[]
        local a = {}

        -- if #a == 0 then return end
        if not a[1] then return end

        -- ---@type string
        -- local s = a[1]

        ---@type string
        local s = a[#a]
        "#,
        ));
    }

    #[test]
    fn test_no_false_positive_deferred_local_function_call() {
        let mut ws = VirtualWorkspace::new();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local RefreshPanel
            local button = {}

            button.DoClick = function()
                RefreshPanel()
            end

            RefreshPanel = function()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isvalid_narrows_nil_from_nullable_type() {
        // Bug repro: IsValid(maybe) should narrow away nil in the true branch
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@type string?
            local maybe = "string"
            if IsValid(maybe) then
                maybe:reverse()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isvalid_narrows_nil_negative_branch() {
        // Bug repro: if not IsValid(x) then return end — x should be non-nil after
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@type string?
            local maybe = "string"
            if not IsValid(maybe) then
                return
            end
            maybe:reverse()
            "#,
        ));
    }

    #[gtest]
    fn test_isfunction_narrows_nil_from_nullable_type() {
        // Bug repro: isfunction(func) should narrow away nil in the true branch
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@type function?
            local func = function() end
            if isfunction(func) then
                func()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isvalid_with_glua_library_annotations() {
        // Test that simulates production: load an IsValid annotation from a "library" file
        // This tests whether loading IsValid with @return boolean (as in output/global.lua)
        // conflicts with the hardcoded try_narrow_isvalid fallback
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        // Simulate the GLua annotation library defining IsValid with boolean return
        ws.def(
            r#"
            ---@param toBeValidated any The table or object to be validated.
            ---@return boolean # True if the object is valid.
            function _G.IsValid(toBeValidated) end
            "#,
        );

        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@type string?
            local maybe = "string"
            if IsValid(maybe) then
                maybe:reverse()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_local_cached_isvalid_narrows_nil() {
        // Regression: `local IsValid = IsValid` is common in GMod addons for performance.
        // The cached local must still narrow away nil.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local IsValid = IsValid
            ---@type string?
            local maybe = "string"
            if IsValid(maybe) then
                maybe:reverse()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_local_cached_isfunction_narrows_nil() {
        // Regression: `local isfunction = isfunction` is common in GMod addons for performance.
        // The cached local must still narrow away nil.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            local isfunction = isfunction
            ---@type function?
            local func = function() end
            if isfunction(func) then
                func()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isstring_narrows_nil_from_nullable_type() {
        // Regression: `isstring(x)` should narrow nil from `string?` via try_narrow_istype_function.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        assert!(ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@type string?
            local maybe = "string"
            if isstring(maybe) then
                maybe:reverse()
            end
            "#,
        ));
    }

    #[gtest]
    fn test_isfunction_member_narrows_to_subtype_nullable_return() {
        // After narrowing Entity→base_glide via isfunction(vehicle.GetFreeSeat),
        // GetFreeSeat returns Entity?, so accessing seat fields needs nil check.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity
            ---@field IsValid fun(self: Entity): boolean

            ---@class base_glide: Entity
            ---@field IsGlideVehicle boolean
            ---@field GetFreeSeat fun(self: base_glide): Entity?
            "#,
        );

        // isfunction(vehicle.GetFreeSeat) narrows vehicle to base_glide.
        // vehicle:GetFreeSeat() returns Entity?  →  seat:IsValid() needs nil check.
        let no_diag = ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param vehicle Entity
            local function test(vehicle)
                if isfunction(vehicle.GetFreeSeat) then
                    local seat = vehicle:GetFreeSeat()
                    seat:IsValid()
                end
            end
            "#,
        );
        assert_that!(no_diag, eq(false), "Expected NeedCheckNil: GetFreeSeat returns Entity?");
    }

    #[gtest]
    fn test_field_truthiness_plus_isfunction_member_narrows() {
        // Combined: vehicle.IsGlideVehicle AND isfunction(vehicle.GetFreeSeat)
        // Both conditions narrow vehicle from Entity to base_glide.
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity
            ---@field IsValid fun(self: Entity): boolean

            ---@class base_glide: Entity
            ---@field IsGlideVehicle boolean
            ---@field GetFreeSeat fun(self: base_glide): Entity?
            "#,
        );

        let no_diag = ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param vehicle Entity
            local function test(vehicle)
                if vehicle.IsGlideVehicle and isfunction(vehicle.GetFreeSeat) then
                    local seat = vehicle:GetFreeSeat()
                    seat:IsValid()
                end
            end
            "#,
        );
        assert_that!(no_diag, eq(false), "Expected NeedCheckNil with AND narrowing");
    }

    #[gtest]
    fn test_field_truthiness_narrows_to_subtype_with_field() {
        // vehicle.IsGlideVehicle alone (truthiness of a field) should narrow
        // vehicle from Entity to base_glide (the subtype that owns that field).
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity
            ---@field IsValid fun(self: Entity): boolean

            ---@class base_glide: Entity
            ---@field IsGlideVehicle boolean
            ---@field GetFreeSeat fun(self: base_glide): Entity?
            "#,
        );

        let no_diag = ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param vehicle Entity
            local function test(vehicle)
                if vehicle.IsGlideVehicle then
                    local seat = vehicle:GetFreeSeat()
                    seat:IsValid()
                end
            end
            "#,
        );
        assert_that!(no_diag, eq(false), "Expected NeedCheckNil after field truthiness narrowing");
    }

    #[gtest]
    fn test_no_narrowing_no_diagnostic_on_unknown_method() {
        // Without narrowing, vehicle is Entity which has no GetFreeSeat.
        // Calling vehicle:GetFreeSeat() without a guard should NOT produce
        // NeedCheckNil (it would produce a different diagnostic like undefined-field).
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
            ---@class Entity
            ---@field IsValid fun(self: Entity): boolean

            ---@class base_glide: Entity
            ---@field IsGlideVehicle boolean
            ---@field GetFreeSeat fun(self: base_glide): Entity?
            "#,
        );

        let no_diag = ws.check_code_for(
            DiagnosticCode::NeedCheckNil,
            r#"
            ---@param vehicle Entity
            local function test(vehicle)
                local seat = vehicle:GetFreeSeat()
                seat:IsValid()
            end
            "#,
        );
        assert_that!(no_diag, eq(true), "No NeedCheckNil expected: Entity has no GetFreeSeat");
    }
}
