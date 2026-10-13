#![cfg(test)]

use crate::{DiagnosticCode, FileId, VirtualWorkspace};
use glua_parser::{LuaAstNode, LuaCallExpr, LuaExpr};
use tokio_util::sync::CancellationToken;

const API: &str = include_str!("fixtures/getter_guard/api.lua");
const READER: &str = include_str!("fixtures/getter_guard/bf_read.lua");
const PATH: &str = "gamemodes/terrortown/entities/weapons/weapon_ttt_m16/shared.lua";

fn stable_reader() -> String {
    READER
        .lines()
        .filter(|line| *line != "---@[fresh_result]")
        .collect::<Vec<_>>()
        .join("\n")
}

fn workspace() -> VirtualWorkspace {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    ws.def_gmod_call_arg_builtins();
    ws.def_file("lua/annotations/getter_api.lua", API);
    ws
}

fn diagnostics(ws: &mut VirtualWorkspace, id: FileId, code: DiagnosticCode) -> usize {
    ws.analysis.diagnostic.enable_only(code);
    ws.analysis
        .diagnose_file(id, CancellationToken::new())
        .unwrap()
        .len()
}

fn last_call_type(ws: &VirtualWorkspace, id: FileId, text: &str) -> String {
    let model = ws.analysis.compilation.get_semantic_model(id).unwrap();
    let call = model
        .get_root()
        .descendants::<LuaCallExpr>()
        .filter(|call| call.syntax().text() == text)
        .last()
        .unwrap();
    ws.humanize_type(model.infer_expr(LuaExpr::CallExpr(call)).unwrap())
}

#[test]
fn getter_guard_compound_ttt() {
    let mut ws = workspace();
    let id = ws.def_file(PATH, "function SWEP:SetZoom(state) if IsValid(self:GetOwner()) and self:GetOwner():IsPlayer() then if state then self:GetOwner():SetFOV(20, 0.3) else self:GetOwner():SetFOV(0, 0.2) end end end");
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        0
    );
    assert_eq!(diagnostics(&mut ws, id, DiagnosticCode::NeedCheckNil), 0);
}

#[test]
fn getter_guard_early_return() {
    let mut ws = workspace();
    let id = ws.def_file(PATH, "function SWEP:Zoom() if not IsValid(self:GetOwner()) then return end self:GetOwner():GetPos() end");
    assert_eq!(last_call_type(&ws, id, "self:GetOwner()"), "Entity");
    assert_eq!(diagnostics(&mut ws, id, DiagnosticCode::NeedCheckNil), 0);
}

#[test]
fn getter_guard_receiver_reassignment() {
    for (receiver, reset) in [
        ("self", "self = other"),
        ("w", "w = other"),
        ("self.weapon", "self.weapon = other"),
    ] {
        let mut ws = workspace();
        let id = ws.def_file(PATH, &format!("---@param other Weapon\nfunction SWEP:Zoom(other) local w = self\n self.weapon = self\n if IsValid({receiver}:GetOwner()) then {receiver}:GetOwner():GetPos() {reset} {receiver}:GetOwner():GetPos() end end"));
        assert_eq!(
            diagnostics(&mut ws, id, DiagnosticCode::NeedCheckNil),
            1,
            "{receiver}"
        );
    }
}

#[test]
fn getter_guard_fresh_result_reader() {
    let mut ws = workspace();
    ws.def_file("lua/annotations/bf_read.lua", READER);
    let id = ws.def_file("lua/autorun/client/cl_reader.lua", "---@param msg bf_read\nfunction Read(msg) if IsValid(msg:ReadEntity()) and msg:ReadEntity():IsPlayer() then msg:ReadEntity():SetFOV(20, 0.3) end end");
    assert_eq!(last_call_type(&ws, id, "msg:ReadEntity()"), "Entity");
    let model = ws.analysis.compilation.get_semantic_model(id).unwrap();
    let call = model
        .get_root()
        .descendants::<LuaCallExpr>()
        .find(|c| c.syntax().text() == "msg:ReadEntity()")
        .unwrap();
    let prefix = call.get_prefix_expr().unwrap();
    let crate::LuaType::Signature(signature) = model.infer_expr(prefix).unwrap() else {
        panic!("reader signature");
    };
    assert!(
        crate::find_signature_attribute_use(model.get_db(), signature, "fresh_result").is_some()
    );
    drop(model);
    ws.def_file("lua/annotations/bf_read.lua", &stable_reader());
    assert_eq!(last_call_type(&ws, id, "msg:ReadEntity()"), "Player");
}

#[test]
fn getter_guard_argument_calls_stay_fresh() {
    let mut ws = workspace();
    ws.def_file("lua/annotations/lookup.lua", "---@class Lookup\nlocal Lookup = {}\n---@param key string\n---@return Entity|NULL\nfunction Lookup:Get(key) end");
    let id = ws.def_file(PATH, "---@param lookup Lookup\nfunction SWEP:Zoom(lookup) if IsValid(self:GetOwner()) then self:GetOwner():GetPos() end if IsValid(lookup:Get('owner')) then lookup:Get('owner'):GetPos() end end");
    assert_eq!(diagnostics(&mut ws, id, DiagnosticCode::NeedCheckNil), 1);
}

#[test]
fn getter_guard_chained_receiver() {
    let mut ws = workspace();
    let id = ws.def_file(PATH, "function SWEP:Zoom() if IsValid(self:GetOwner()) and IsValid(self:GetOwner():GetOwner()) then self:GetOwner():GetOwner():GetPos() end end");
    assert_eq!(diagnostics(&mut ws, id, DiagnosticCode::NeedCheckNil), 0);
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        0
    );
}

#[test]
fn getter_guard_fresh_result_incremental() {
    let mut ws = workspace();
    let path = "lua/annotations/bf_read.lua";
    ws.def_file(path, READER);
    let id = ws.def_file("lua/autorun/client/cl_reader.lua", "---@param msg bf_read\nfunction Read(msg) if IsValid(msg:ReadEntity()) and msg:ReadEntity():IsPlayer() then msg:ReadEntity():SetFOV(20, 0.3) end end");
    assert_eq!(last_call_type(&ws, id, "msg:ReadEntity()"), "Entity");
    let stable = stable_reader();
    ws.def_file(path, &stable);
    assert_eq!(last_call_type(&ws, id, "msg:ReadEntity()"), "Player");
    let uri = ws.virtual_url_generator.new_uri(path);
    ws.analysis.update_file_by_uri(&uri, None);
    assert_eq!(last_call_type(&ws, id, "msg:ReadEntity()"), "unknown");
    ws.def_file(path, READER);
    assert_eq!(last_call_type(&ws, id, "msg:ReadEntity()"), "Entity");
    ws.def_file(path, &stable);
    assert_eq!(last_call_type(&ws, id, "msg:ReadEntity()"), "Player");
}

#[test]
fn getter_guard_call_path_does_not_alias_string_field() {
    let mut ws = workspace();
    let id = ws.def_file(PATH, "function SWEP:Zoom() if IsValid(self:GetOwner()) then self:GetOwner():GetPos() self['GetOwner()']:GetPos() end end");
    let model = ws.analysis.compilation.get_semantic_model(id).unwrap();
    let calls: Vec<_> = model
        .get_root()
        .descendants::<LuaCallExpr>()
        .filter(|c| c.syntax().text() == "self:GetOwner()")
        .collect();
    let getter = crate::get_var_expr_var_ref_id(
        model.get_db(),
        &mut model.get_cache().borrow_mut(),
        LuaExpr::CallExpr(calls[0].clone()),
    )
    .unwrap();
    let field = model
        .get_root()
        .descendants::<glua_parser::LuaIndexExpr>()
        .find(|i| i.syntax().text() == "self['GetOwner()']")
        .unwrap();
    let field = crate::get_var_expr_var_ref_id(
        model.get_db(),
        &mut model.get_cache().borrow_mut(),
        LuaExpr::IndexExpr(field),
    )
    .unwrap();
    assert_ne!(getter, field);
}

#[test]
fn getter_guard_fresh_result_does_not_remove_null() {
    let mut ws = workspace();
    ws.def_file(
        "lua/annotations/sequence.lua",
        "---@class Sequence
local Sequence = {}
---@return Entity|NULL
---@[fresh_result]
function Sequence:Next() end
---@return Entity|NULL
function Sequence:Peek() end",
    );
    let id = ws.def_file(PATH, "---@param seq Sequence
function SWEP:Zoom(seq) if IsValid(seq:Next()) then seq:Next():GetPos() end if IsValid(seq:Peek()) then seq:Peek():GetPos() end end");
    assert_eq!(diagnostics(&mut ws, id, DiagnosticCode::NeedCheckNil), 1);
}

#[test]
fn getter_guard_dot_calls_and_unreferenced_receivers_stay_fresh() {
    for call in [
        "GetEntity()",
        "GetWeapon():GetOwner()",
        "self.GetOwner(self)",
    ] {
        let mut ws = workspace();
        ws.def_file(
            "lua/annotations/globals.lua",
            "---@return Entity|NULL
function GetEntity() end
---@return Weapon
function GetWeapon() end",
        );
        let id = ws.def_file(PATH, &format!("function SWEP:Zoom() if IsValid(self:GetOwner()) then self:GetOwner():GetPos() end if IsValid({call}) then {call}:GetPos() end end"));
        assert_eq!(
            diagnostics(&mut ws, id, DiagnosticCode::NeedCheckNil),
            1,
            "{call}"
        );
    }
}

#[test]
fn getter_guard_field_on_chained_receiver() {
    let mut ws = workspace();
    ws.def_file(
        "lua/annotations/player_weapon.lua",
        "---@class (partial) Player
---@field weapon Weapon",
    );
    let id = ws.def_file(PATH, "function SWEP:Zoom() if IsValid(self:GetOwner()) and self:GetOwner():IsPlayer() and IsValid(self:GetOwner().weapon:GetOwner()) then self:GetOwner().weapon:GetOwner():GetPos() end end");
    assert_eq!(diagnostics(&mut ws, id, DiagnosticCode::NeedCheckNil), 0);
}

#[test]
fn getter_guard_field_receiver_reassignment() {
    let mut ws = workspace();
    ws.def_file(
        "lua/annotations/owner_field.lua",
        "---@class Holder
---@field owner Entity|NULL",
    );
    let id = ws.def_file(PATH, "---@param w Holder
---@param other Holder
function Use(w, other) if IsValid(w.owner) then w.owner:GetPos() w = other w.owner:GetPos() end end");
    let model = ws.analysis.compilation.get_semantic_model(id).unwrap();
    let field = model
        .get_root()
        .descendants::<glua_parser::LuaIndexExpr>()
        .filter(|index| index.syntax().text() == "w.owner")
        .last()
        .unwrap();
    let ty = model.infer_expr(LuaExpr::IndexExpr(field)).unwrap();
    assert!(crate::semantic::contains_gmod_null_type(
        model.get_db(),
        &ty
    ));
}

#[test]
fn getter_guard_union_predicates() {
    for second_cast in [true, false] {
        let mut ws = workspace();
        let annotation = if second_cast {
            "---@return_cast value Player"
        } else {
            ""
        };
        ws.def_file(
            "lua/annotations/guards.lua",
            &format!(
                r#"
---@param value any
---@return boolean
---@return_cast value Player
function FirstGuard(value) end
---@param value any
---@return boolean
{annotation}
function SecondGuard(value) end
"#
            ),
        );
        let id = ws.def_file(PATH, "---@param state boolean
function SWEP:Zoom(state) local predicate = state and FirstGuard or SecondGuard if predicate(self:GetOwner()) then self:GetOwner():GetPos() end end");
        let model = ws.analysis.compilation.get_semantic_model(id).unwrap();
        let predicate = model
            .get_root()
            .descendants::<LuaCallExpr>()
            .find(|call| call.syntax().text() == "predicate(self:GetOwner())")
            .unwrap();
        assert!(matches!(
            model
                .infer_expr(predicate.get_prefix_expr().unwrap())
                .unwrap(),
            crate::LuaType::Union(_)
        ));
        drop(model);
        let ty = last_call_type(&ws, id, "self:GetOwner()");
        assert_eq!(
            ty,
            if second_cast {
                "Player"
            } else {
                "(Entity|NULL)"
            }
        );
    }
}
