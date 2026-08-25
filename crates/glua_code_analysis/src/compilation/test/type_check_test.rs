#[cfg(test)]
mod test {
    use crate::{DiagnosticCode, LuaType, VirtualWorkspace};
    use glua_parser::{LuaAstNode, LuaAstToken, LuaLocalName};

    #[allow(dead_code)]
    fn local_name_type(ws: &mut VirtualWorkspace, file_id: crate::FileId, name: &str) -> LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");

        let local_name = semantic_model
            .get_root()
            .descendants::<LuaLocalName>()
            .find(|local_name| {
                local_name
                    .get_name_token()
                    .is_some_and(|token| token.get_name_text() == name)
            })
            .expect("expected local name");
        let token = local_name
            .get_name_token()
            .expect("expected local name token");

        semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .map(|info| info.display_typ().clone())
            .expect("expected semantic info for local name")
    }

    #[test]
    fn test_issue_421() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::AssignTypeMismatch,
            r#"
        local a         --- @type string?
        local b = { a } --- @type string[] error

        b[2] = nil
        "#,
        ));
    }

    #[test]
    fn test_issue_645() {
        let mut ws = VirtualWorkspace::new();

        assert!(ws.check_code_for(
            DiagnosticCode::ParamTypeMismatch,
            r#"
        --- @alias Dir -1|1

        ---@param d Dir
        local function foo(d) end

        foo(1)
        "#,
        ));
    }

    #[test]
    fn test_guarded_bootstrap_assign_type_mismatch() {
        let mut ws = VirtualWorkspace::new();
        let file_1 = ws.def_file(
            "lua/sh_item.lua",
            r#"
cityrp = cityrp or {}
if not cityrp.item then cityrp.item = {stored = {}, cats = {}, catIndex = 1} end
function cityrp.item.new(base)
    return {}
end
"#,
        );
        let _file_2 = ws.def_file(
            "lua/sv_item.lua",
            r#"
cityrp = cityrp or {}
if not cityrp.item then cityrp.item = {stored = {}, cats = {}, catIndex = 1} end
"#,
        );
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::AssignTypeMismatch);
        let diags = ws
            .analysis
            .diagnose_file(file_1, tokio_util::sync::CancellationToken::new())
            .unwrap_or_default();
        println!("DIAGNOSTICS: {:?}", diags);
        assert!(
            diags.is_empty(),
            "expected no assign type mismatch, got {:?}",
            diags
        );
    }

    #[test]
    fn test_weapon_velements_need_check_nil() {
        let mut ws = VirtualWorkspace::new();
        let _file_0 = ws.def_file(
            "gamemodes/test/entities/weapons/base/shared.lua",
            r#"
---@class Vector
---@field x number
---@field y number
---@field z number

---@class Angle
---@field p number
---@field y number
---@field r number

---@return Vector
function Vector(x, y, z) return {} end

---@return Angle
function Angle(p, y, r) return {} end
"#,
        );
        let file_1 = ws.def_file(
            "gamemodes/test/entities/weapons/swep_test/shared.lua",
            r#"
SWEP = {}
SWEP.VElements = {
	["element_name"] = { type = "Model", pos = Vector(1, 2, 3), angle = Angle(0, 0, 0), size = Vector(1, 1, 1) }
}

function SWEP:Initialize()
	if CLIENT then
		self.VElements = table.FullCopy( self.VElements )
	end
end

if CLIENT then
	function SWEP:ViewModelDrawn()
		local v = self.VElements["element_name"]
		if not v then return end
		local px = v.pos.x
		local ax = v.angle.y
		local sx = v.size.z
	end

	function table.FullCopy(tab)
		if not tab then return nil end
		local res = {}
		for k, v in pairs(tab) do
			if (type(v) == "table") then
				res[k] = table.FullCopy(v)
			elseif (type(v) == "Vector") then
				res[k] = Vector(v.x, v.y, v.z)
			elseif (type(v) == "Angle") then
				res[k] = Angle(v.p, v.y, v.r)
			else
				res[k] = v
			end
		end
		return res
	end
end
"#,
        );
        ws.analysis
            .diagnostic
            .enable_only(DiagnosticCode::NeedCheckNil);
        let diags = ws
            .analysis
            .diagnose_file(file_1, tokio_util::sync::CancellationToken::new())
            .unwrap_or_default();
        assert!(
            diags.is_empty(),
            "expected 0 need-check-nil diagnostics, got: {:?}",
            diags
        );
    }
}
