#![cfg(test)]
use crate::VirtualWorkspace;

#[test]
fn scalar_dictionary_fields_can_be_promoted_to_collections() {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    let file = ws.def_file(
        "gamemodes/cityrp/gamemode/core/libraries/sh_entities.lua",
        r#"
local _tableTypes = {OnTrigger = true}
---@return EntityObject
local function ParseEntity(str)
    ---@class EntityObject
    local t = {}
    for key, value in string.gmatch(str, '"(.-)".-"(.-)"') do
        value = tonumber(value) or value
        if t[key] then
            if type(t[key]) ~= "table" then
                t[key] = { t[key] }
            else
                t[key][#t[key] + 1] = value
            end
        elseif _tableTypes[key] then
            t[key] = { value }
        else
            t[key] = value
        end
    end
    return t
end
local entity = ParseEntity('"name" "value"')
local value = entity[tostring(1)]
"#,
    );
    let ty = ws.local_ty(file, "value");
    let display = ws.humanize_type(ty);
    assert!(
        display.contains("number") && display.contains("string"),
        "{display}"
    );
}

#[test]
fn promoted_collections_appended_a_new_element_type_settle() {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    let file = ws.def_file(
        "gamemodes/cityrp/gamemode/core/libraries/sh_entities.lua",
        r#"
---@return EntityObject
local function ParseEntity(str)
    ---@class EntityObject
    local t = {}
    for key, value in string.gmatch(str, '"(.-)".-"(.-)"') do
        if t[key] then
            if type(t[key]) ~= "table" then
                t[key] = { t[key] }
            else
                t[key][#t[key] + 1] = tonumber(value) or false
            end
        else
            t[key] = value
        end
    end
    return t
end
local entity = ParseEntity('"name" "value"')
local value = entity[tostring(1)]
"#,
    );
    let ty = ws.local_ty(file, "value");
    let display = ws.humanize_type(ty);
    assert!(display.contains("string"), "{display}");
}
