#[cfg(test)]
use std::{collections::BTreeSet, path::Path};
use std::{ops::Deref, sync::Arc};

use glua_parser::{LuaAstNode, LuaAstToken, LuaLocalName};
use lsp_types::NumberOrString;
#[cfg(test)]
use lsp_types::{Diagnostic, DiagnosticSeverity};
use tokio_util::sync::CancellationToken;

use crate::{
    DbIndex, DiagnosticCode, EmmyLuaAnalysis, Emmyrc, FileId, LuaType, RenderLevel,
    VirtualUrlGenerator, check_type_compact, humanize_type,
};

/// A virtual workspace for testing.
#[allow(unused)]
#[derive(Debug)]
pub struct VirtualWorkspace {
    pub virtual_url_generator: VirtualUrlGenerator,
    pub analysis: EmmyLuaAnalysis,
    id_counter: u32,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DiagnosticSnapshot {
    pub file: String,
    pub range_start_line: u32,
    pub range_start_character: u32,
    pub range_end_line: u32,
    pub range_end_character: u32,
    pub severity: Option<i32>,
    pub code: Option<String>,
    pub message: String,
}

pub const GMOD_CALL_ARG_BUILTINS_FIXTURE: &str = r#"
---@meta
---@attribute call_arg(domain: string, role: string, priority: integer?)
---@attribute call_arg_field(domain: string, role: string, field_path: string, priority: integer?)
---@attribute overload_call_arg(param: integer, domain: string, role: string, priority: integer?)
---@attribute overload_call_arg_field(param: integer, domain: string, role: string, field_path: string, priority: integer?)
---@attribute net_send(receiver_realm: string)
---@attribute net_payload(direction: string, wire_format: string)

util = util or {}
net = net or {}
hook = hook or {}
timer = timer or {}
concommand = concommand or {}
vgui = vgui or {}
derma = derma or {}
file = file or {}

Entity = Entity or {}

---@[call_arg("gmod.file_find", "glob")]
---@param name string
---@[call_arg("gmod.file_find", "search_path")]
---@param path string
function file.Find(name, path, sorting) end

---@[call_arg("gmod.load", "include")]
---@param fileName string
function include(fileName) end

---@[call_arg("gmod.load", "addcsluafile")]
---@param fileName string
function AddCSLuaFile(fileName) end

---@[call_arg("gmod.load", "includecs")]
---@param fileName string
function IncludeCS(fileName) end

---@[call_arg("gmod.load", "require")]
---@param moduleName string
function require(moduleName) end

---@[call_arg("gmod.load", "compilefile")]
---@param path string
---@param showError? boolean
---@return function?
function CompileFile(path, showError) end

---@[call_arg("gmod.environment", "target")]
---@param target function|integer
---@[call_arg("gmod.environment", "environment")]
---@param environment table
---@return function?
function setfenv(target, environment) end

---@[call_arg("gmod.net_message", "define")]
---@param str string
function util.AddNetworkString(str) end

---@[call_arg("gmod.net_message", "start")]
---@param messageName string
function net.Start(messageName, unreliable) end

---@[call_arg("gmod.net_message", "receive")]
---@param messageName string
---@[call_arg("gmod.net_message", "callback")]
---@param callback function
function net.Receive(messageName, callback) end

-- Net payload ops. Recognition is annotation-driven, so these carry the
-- same `net_payload` / `net_send` metadata as the shipped annotations;
-- without it the analyzer sees no net flows at all.

---@param angle any
---@[net_payload("write", "angle")]
function net.WriteAngle(angle) end

---@return Angle
---@[net_payload("read", "angle")]
function net.ReadAngle() end

---@param boolean any
---@[net_payload("write", "bit")]
function net.WriteBit(boolean) end

---@return number
---@[net_payload("read", "bit")]
function net.ReadBit() end

---@param boolean any
---@[net_payload("write", "bool")]
function net.WriteBool(boolean) end

---@return boolean
---@[net_payload("read", "bool")]
function net.ReadBool() end

---@param color any
---@[net_payload("write", "color")]
function net.WriteColor(color) end

---@return table
---@[net_payload("read", "color")]
function net.ReadColor() end

---@param binaryData any
---@[net_payload("write", "data")]
function net.WriteData(binaryData) end

---@return string
---@[net_payload("read", "data")]
function net.ReadData() end

---@param double any
---@[net_payload("write", "double")]
function net.WriteDouble(double) end

---@return number
---@[net_payload("read", "double")]
function net.ReadDouble() end

---@param entity any
---@[net_payload("write", "entity")]
function net.WriteEntity(entity) end

---@return Entity
---@[net_payload("read", "entity")]
function net.ReadEntity() end

---@param float any
---@[net_payload("write", "float")]
function net.WriteFloat(float) end

---@return number
---@[net_payload("read", "float")]
function net.ReadFloat() end

---@param matrix any
---@[net_payload("write", "matrix")]
function net.WriteMatrix(matrix) end

---@return VMatrix
---@[net_payload("read", "matrix")]
function net.ReadMatrix() end

---@param normal any
---@[net_payload("write", "normal")]
function net.WriteNormal(normal) end

---@return Vector
---@[net_payload("read", "normal")]
function net.ReadNormal() end

---@param ply any
---@[net_payload("write", "player")]
function net.WritePlayer(ply) end

---@return Player
---@[net_payload("read", "player")]
function net.ReadPlayer() end

---@param str any
---@[net_payload("write", "string")]
function net.WriteString(str) end

---@return string
---@[net_payload("read", "string")]
function net.ReadString() end

---@param tbl any
---@[net_payload("write", "table")]
function net.WriteTable(tbl) end

---@return table
---@[net_payload("read", "table")]
function net.ReadTable() end

---@param data any
---@[net_payload("write", "type")]
function net.WriteType(data) end

---@return any
---@[net_payload("read", "type")]
function net.ReadType() end

---@param uint64 any
---@[net_payload("write", "uint64")]
function net.WriteUInt64(uint64) end

---@return string
---@[net_payload("read", "uint64")]
function net.ReadUInt64() end

---@param vector any
---@[net_payload("write", "vector")]
function net.WriteVector(vector) end

---@return Vector
---@[net_payload("read", "vector")]
function net.ReadVector() end

---@param value number
---@[call_arg("gmod.net_payload", "bits")]
---@param bitCount number
---@[net_payload("write", "int")]
function net.WriteInt(value, bitCount) end

---@[call_arg("gmod.net_payload", "bits")]
---@param bitCount number
---@return number
---@[net_payload("read", "int")]
function net.ReadInt(bitCount) end

---@param value number
---@[call_arg("gmod.net_payload", "bits")]
---@param bitCount number
---@[net_payload("write", "uint")]
function net.WriteUInt(value, bitCount) end

---@[call_arg("gmod.net_payload", "bits")]
---@param bitCount number
---@return number
---@[net_payload("read", "uint")]
function net.ReadUInt(bitCount) end

---@[call_arg("gmod.net_payload", "target")]
---@param ply any
---@[net_send("client")]
function net.Send(ply) end

---@[call_arg("gmod.net_payload", "target")]
---@param ply any
---@[net_send("client")]
function net.SendOmit(ply) end

---@[call_arg("gmod.net_payload", "target")]
---@param position any
---@[net_send("client")]
function net.SendPAS(position) end

---@[call_arg("gmod.net_payload", "target")]
---@param position any
---@[net_send("client")]
function net.SendPVS(position) end

---@[net_send("client")]
function net.Broadcast() end

---@[net_send("server")]
function net.SendToServer() end

---@[call_arg("gmod.hook", "add")]
---@param eventName string
---@param identifier any
---@[call_arg("gmod.hook", "callback")]
---@param func function
function hook.Add(eventName, identifier, func) end

---@[call_arg("gmod.hook", "emit")]
---@param eventName string
function hook.Run(eventName, ...) end

---@[call_arg("gmod.hook", "emit")]
---@param eventName string
---@[call_arg("gmod.hook", "gamemode_table")]
---@param gamemodeTable table
function hook.Call(eventName, gamemodeTable, ...) end

---@[call_arg("gmod.hook", "remove")]
---@param eventName string
---@param identifier any
function hook.Remove(eventName, identifier) end

---@[call_arg("gmod.concommand", "define")]
---@param name string
---@[call_arg("gmod.concommand", "callback")]
---@param callback function
function concommand.Add(name, callback, autoComplete, helpText, flags) end

---@[call_arg("gmod.convar", "define_server")]
---@param name string
function _G.CreateConVar(name, value, flags, helptext, min, max) end

---@[call_arg("gmod.convar", "define_client")]
---@param name string
function _G.CreateClientConVar(name, default, shouldsave, userinfo, helptext, min, max) end

---@[call_arg("gmod.class_base", "reference")]
---@param value string
function _G.DEFINE_BASECLASS(value) end

---@[call_arg("gmod.gamemode", "reference")]
---@param base string
function _G.DeriveGamemode(base) end

---@[call_arg("gmod.color", "r")]
---@param r number
---@[call_arg("gmod.color", "g")]
---@param g number
---@[call_arg("gmod.color", "b")]
---@param b number
---@[call_arg("gmod.color", "a")]
---@param a? number
---@return Color
function _G.Color(r, g, b, a) end

---@[call_arg("gmod.timer", "define")]
---@param identifier string
---@[call_arg("gmod.timer", "callback")]
---@param func function
function timer.Create(identifier, delay, repetitions, func) end

---@param delay number
---@[call_arg("gmod.timer", "simple")]
---@param func function
function timer.Simple(delay, func) end

---@generic T: Panel
---@[call_arg("gmod.vgui_panel", "reference")]
---@param className `T`
---@[call_arg("gmod.vgui_panel", "parent")]
---@param parent Panel?
---@return (instance) T?
function vgui.Create(className, parent, name) end

---@generic T: Panel
---@[call_arg("gmod.vgui_panel", "reference")]
---@[call_arg("gmod.vgui_panel", "parent_self")]
---@param className `T`
---@return (instance) T
function Panel:Add(className) end

-- Panel is intentionally caller-provided: declaring it here changes generic
-- vgui.Create return resolution for every test workspace.
---@[call_arg("gmod.vgui_panel", "child_self")]
---@[call_arg("gmod.vgui_panel", "parent")]
---@param parent Panel
function Panel:SetParent(parent) end

---@[call_arg("gmod.vgui_panel", "own_class")]
---@param class string
---@param dpropertysheet any
---@param width number
---@param height number
function Panel:GenerateExample(class, dpropertysheet, width, height) end

---@class DDragBase: Panel
---@field GetParent fun(self: DDragBase): Panel
---@class DHorizontalScroller: Panel
---@field GetParent fun(self: DHorizontalScroller): Panel
---@field pnlCanvas DDragBase
---@[call_arg_field("gmod.vgui_panel", "parent_self", "pnlCanvas")]
---@[call_arg("gmod.vgui_panel", "reference")]
---@param panel Panel
function DHorizontalScroller:AddPanel(panel) end

---@generic T: table
---@[call_arg("gmod.vgui_panel", "register_table")]
---@[call_arg_field("gmod.vgui_panel", "base", "Base")]
---@param metatable T
---@param parent? Panel
---@param name? string
---@return (instance) T?
function vgui.CreateFromTable(metatable, parent, name) end

---@[call_arg("gmod.vgui_panel", "define")]
---@param name string
---@[call_arg("gmod.vgui_panel", "table")]
---@param panel table
---@[call_arg("gmod.vgui_panel", "base")]
---@param base string
function vgui.Register(name, panel, base) end

---@generic T: table
---@[call_arg("gmod.vgui_panel", "register_table")]
---@param panel T
---@[call_arg("gmod.vgui_panel", "base")]
---@param base? string
---@return T
function vgui.RegisterTable(panel, base) end

---@[call_arg("gmod.vgui_panel", "define_control")]
---@param class string
---@param description string
---@[call_arg("gmod.vgui_panel", "table")]
---@param panel table
---@[call_arg("gmod.vgui_panel", "base")]
---@param base string
function derma.DefineControl(class, description, panel, base) end

---@[call_arg("gmod.derma_skin", "define")]
---@param name string
function derma.DefineSkin(name, description, skin) end

---@[call_arg("gmod.derma_skin", "reference")]
---@param name string
function derma.GetNamedSkin(name) end

function derma.GetSkinTable() end

---@[call_arg("gmod.derma_skin", "reference")]
---@param skinName string
function Panel:SetSkin(skinName) end

---@[overload_call_arg(0, "gmod.network_var", "type")]
---@[overload_call_arg(1, "gmod.network_var", "define")]
---@overload fun(type: string, name: string, extended?: table)
---@[call_arg("gmod.network_var", "type")]
---@param type string
---@param slot number
---@[call_arg("gmod.network_var", "define")]
---@param name string
---@param extended? table
function Entity:NetworkVar(type, slot, name, extended) end

---@[overload_call_arg(0, "gmod.network_var", "type")]
---@[overload_call_arg(1, "gmod.network_var", "define_slot")]
---@overload fun(type: string, name: string)
---@[overload_call_arg(0, "gmod.network_var", "type")]
---@[overload_call_arg(2, "gmod.network_var", "define_slot")]
---@overload fun(type: string, slot: nil, name: string)
---@[call_arg("gmod.network_var", "type")]
---@param type string
---@param slot number
---@[call_arg("gmod.network_var", "define_slot")]
---@param name string
function Entity:DTVar(type, slot, name) end

---@[overload_call_arg(0, "gmod.network_var", "type")]
---@[overload_call_arg(2, "gmod.network_var", "define_element")]
---@overload fun(type: string, element: string, name: string, extended?: table)
---@[call_arg("gmod.network_var", "type")]
---@param type string
---@param slot number
---@param element string
---@[call_arg("gmod.network_var", "define_element")]
---@param name string
---@param extended? table
function Entity:NetworkVarElement(type, slot, element, name, extended) end
"#;

#[cfg(test)]
pub fn diagnostics_to_snapshot_set(
    file: impl Into<String>,
    diagnostics: Vec<Diagnostic>,
) -> BTreeSet<DiagnosticSnapshot> {
    let file = file.into();
    diagnostics
        .into_iter()
        .map(|diagnostic| DiagnosticSnapshot {
            file: file.clone(),
            range_start_line: diagnostic.range.start.line,
            range_start_character: diagnostic.range.start.character,
            range_end_line: diagnostic.range.end.line,
            range_end_character: diagnostic.range.end.character,
            severity: diagnostic.severity.map(diagnostic_severity_to_i32),
            code: diagnostic.code.map(number_or_string_to_string),
            message: diagnostic.message,
        })
        .collect()
}

#[cfg(test)]
fn diagnostic_severity_to_i32(severity: DiagnosticSeverity) -> i32 {
    match severity {
        DiagnosticSeverity::ERROR => 1,
        DiagnosticSeverity::WARNING => 2,
        DiagnosticSeverity::INFORMATION => 3,
        DiagnosticSeverity::HINT => 4,
        _ => 0,
    }
}

#[cfg(test)]
fn number_or_string_to_string(code: NumberOrString) -> String {
    match code {
        NumberOrString::Number(number) => number.to_string(),
        NumberOrString::String(text) => text,
    }
}

#[cfg(test)]
fn normalize_snapshot_file(base: &Path, file_path: &Path) -> String {
    let normalized = file_path
        .strip_prefix(base)
        .unwrap_or(file_path)
        .to_string_lossy()
        .replace('\\', "/");
    if normalized.is_empty() {
        file_path.to_string_lossy().replace('\\', "/")
    } else {
        normalized
    }
}

#[allow(unused, clippy::unwrap_used)]
impl Default for VirtualWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualWorkspace {
    pub fn new() -> Self {
        let generator = VirtualUrlGenerator::new();
        let mut analysis = EmmyLuaAnalysis::new();
        let base = &generator.base;
        analysis.add_main_workspace(base.clone());
        VirtualWorkspace {
            virtual_url_generator: generator,
            analysis,
            id_counter: 0,
        }
    }

    pub fn new_with_init_std_lib() -> Self {
        let generator = VirtualUrlGenerator::new();
        let mut analysis = EmmyLuaAnalysis::new();
        analysis.init_std_lib();
        let base = &generator.base;
        analysis.add_main_workspace(base.clone());
        VirtualWorkspace {
            virtual_url_generator: generator,
            analysis,
            id_counter: 0,
        }
    }

    pub fn def(&mut self, content: &str) -> FileId {
        let id = self.id_counter;
        self.id_counter += 1;
        let uri = self
            .virtual_url_generator
            .new_uri(&format!("virtual_{}.lua", id));

        self.analysis
            .update_file_by_uri(&uri, Some(content.to_string()))
            .map(|(id, _)| id)
            .expect("File ID must be present")
    }

    pub fn def_gmod_type_predicates(&mut self) -> FileId {
        self.def(
            r#"
            ---@meta
            ---@attribute call_arg(domain: string, role: string, priority: integer?)
            ---@attribute self_guard(member: string)

            ---@[call_arg("gmod.member_guard", "function")]
            ---@param value any
            ---@return TypeGuard<function>
            function isfunction(value) end

            ---@param value any
            ---@return TypeGuard<string>
            function isstring(value) end

            ---@param value any
            ---@return TypeGuard<number>
            function isnumber(value) end

            ---@param value any
            ---@return TypeGuard<boolean>
            function isbool(value) end

            ---@param value any
            ---@return TypeGuard<table>
            function istable(value) end

            ---@class Entity

            ---@param value any
            ---@return TypeGuard<Entity>
            function isentity(value) end

            ---@class Vector

            ---@param value any
            ---@return TypeGuard<Vector>
            function isvector(value) end

            ---@class Angle

            ---@param value any
            ---@return TypeGuard<Angle>
            function isangle(value) end

            ---@class VMatrix

            ---@param value any
            ---@return TypeGuard<VMatrix>
            function ismatrix(value) end

            ---@class Panel

            ---@param value any
            ---@return TypeGuard<Panel>
            function ispanel(value) end

            ---@class Color

            ---@param value any
            ---@return TypeGuard<Color>
            function IsColor(value) end

            ---@param value any
            ---@return TypeGuard<Entity>
            function IsValid(value) end
            "#,
        )
    }

    pub fn def_gmod_call_arg_builtins(&mut self) -> FileId {
        self.def_file(
            "lua/includes/glua_ls_gmod_call_arg_builtins.lua",
            GMOD_CALL_ARG_BUILTINS_FIXTURE,
        )
    }

    pub fn def_file(&mut self, file_name: &str, content: &str) -> FileId {
        let uri = self.virtual_url_generator.new_uri(file_name);

        self.analysis
            .update_file_by_uri(&uri, Some(content.to_string()))
            .map(|(id, _)| id)
            .expect("File ID must be present")
    }

    pub fn def_files(&mut self, files: Vec<(&str, &str)>) -> Vec<FileId> {
        let file_infos: Vec<_> = files
            .iter()
            .map(|(file_name, content)| {
                let uri = self.virtual_url_generator.new_uri(file_name);
                (uri, Some(content.to_string()))
            })
            .collect();

        let uris = file_infos
            .iter()
            .map(|(uri, _)| uri.clone())
            .collect::<Vec<_>>();
        self.analysis.update_files_by_uri(file_infos);
        let mut file_ids = uris
            .iter()
            .map(|uri| self.analysis.get_file_id(uri).expect("defined file id"))
            .collect::<Vec<_>>();
        file_ids.sort();

        file_ids
    }

    pub fn get_emmyrc(&self) -> Emmyrc {
        self.analysis.emmyrc.deref().clone()
    }

    pub fn update_emmyrc(&mut self, emmyrc: Emmyrc) {
        self.analysis.update_config(Arc::new(emmyrc));
    }

    pub fn get_node<Ast: LuaAstNode>(&self, file_id: FileId) -> Ast {
        let tree = self
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_syntax_tree(&file_id)
            .expect("Tree must exist");
        tree.get_chunk_node()
            .descendants::<Ast>()
            .next()
            .expect("Node must exist")
    }

    pub fn ty(&mut self, type_repr: &str) -> LuaType {
        let virtual_content = format!("---@type {}\nlocal t", type_repr);
        let file_id = self.def(&virtual_content);
        let local_name = self.get_node::<LuaLocalName>(file_id);
        let semantic_model = self
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Semantic model must exist");
        let token = local_name.get_name_token().expect("Name token must exist");
        let info = semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .expect("Semantic info must exist");
        info.display_typ().clone()
    }

    pub fn expr_ty(&mut self, expr: &str) -> LuaType {
        let virtual_content = format!("local t = {}", expr);
        let file_id = self.def(&virtual_content);
        let local_name = self.get_node::<LuaLocalName>(file_id);
        let semantic_model = self
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Model must exist");
        let token = local_name.get_name_token().expect("Name token must exist");
        let info = semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .expect("Semantic info must exist");
        info.display_typ().clone()
    }

    /// The type of the local `name` declared in `file_id`.
    pub fn local_ty(&self, file_id: FileId, name: &str) -> LuaType {
        let semantic_model = self
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("Model must exist");
        let local_name = semantic_model
            .get_root()
            .descendants::<LuaLocalName>()
            .find(|local_name| {
                local_name
                    .get_name_token()
                    .is_some_and(|token| token.get_name_text() == name)
            })
            .expect("Local must exist");
        let token = local_name.get_name_token().expect("Name token must exist");
        semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .expect("Semantic info must exist")
            .display_typ()
            .clone()
    }

    pub fn check_type(&self, source: &LuaType, compact_type: &LuaType) -> bool {
        let db = &self.analysis.compilation.get_db();
        check_type_compact(db, source, compact_type).is_ok()
    }

    pub fn enable_check(&mut self, diagnostic_code: DiagnosticCode) {
        let mut emmyrc = Emmyrc::default();
        emmyrc.diagnostics.enables.push(diagnostic_code);
        self.analysis.diagnostic.update_config(Arc::new(emmyrc));
    }

    /// Only runs the checks of the given diagnostic code; the code must be added to `const CODES` in the corresponding `Checker`
    pub fn check_code_for(&mut self, diagnostic_code: DiagnosticCode, block_str: &str) -> bool {
        // Only enable the given diagnostic
        self.analysis.diagnostic.enable_only(diagnostic_code);
        let file_id = self.def(block_str);
        let result = self
            .analysis
            .diagnose_file(file_id, CancellationToken::new());
        if let Some(diagnostics) = result {
            let code_string = Some(NumberOrString::String(
                diagnostic_code.get_name().to_string(),
            ));
            for diagnostic in diagnostics {
                if diagnostic.code == code_string {
                    return false;
                }
            }
        }

        true
    }

    /// Like `check_code_for` but registers the content under a specific file path.
    /// Useful for testing diagnostics that depend on path-based detection (e.g. scripted classes).
    /// Returns `true` if no diagnostic of the given code is emitted.
    pub fn check_file_for(
        &mut self,
        diagnostic_code: DiagnosticCode,
        file_name: &str,
        content: &str,
    ) -> bool {
        self.analysis.diagnostic.enable_only(diagnostic_code);
        let file_id = self.def_file(file_name, content);
        let result = self
            .analysis
            .diagnose_file(file_id, CancellationToken::new());
        if let Some(diagnostics) = result {
            let code_string = Some(NumberOrString::String(
                diagnostic_code.get_name().to_string(),
            ));
            for diagnostic in diagnostics {
                if diagnostic.code == code_string {
                    return false;
                }
            }
        }
        true
    }

    pub fn check_code_for_namespace(
        &mut self,
        diagnostic_code: DiagnosticCode,
        block_str: &str,
    ) -> bool {
        self.check_code_for(
            diagnostic_code,
            &format!(
                "---@namespace TestNamespace{}\n{}",
                self.id_counter, block_str
            ),
        )
    }

    pub fn enable_full_diagnostic(&mut self) {
        let mut emmyrc = Emmyrc::default();
        let mut enables = emmyrc.diagnostics.enables;
        enables.push(DiagnosticCode::IncompleteSignatureDoc);
        enables.push(DiagnosticCode::MissingGlobalDoc);
        emmyrc.diagnostics.enables = enables;
        self.analysis.diagnostic.update_config(Arc::new(emmyrc));
    }

    pub fn humanize_type(&self, ty: LuaType) -> String {
        let db = &self.analysis.compilation.get_db();
        humanize_type(db, &ty, RenderLevel::Brief)
    }

    pub fn humanize_type_detailed(&self, ty: LuaType) -> String {
        let db = &self.analysis.compilation.get_db();
        humanize_type(db, &ty, RenderLevel::Detailed)
    }

    pub fn get_db_mut(&mut self) -> &mut DbIndex {
        (self.analysis.compilation.get_db_mut()) as _
    }

    #[cfg(test)]
    pub fn run_diagnostics_with_shared_snapshots(
        &self,
        file_ids: &[FileId],
    ) -> BTreeSet<DiagnosticSnapshot> {
        let shared_snapshot = self.analysis.precompute_diagnostic_shared_data();
        let mut combined = BTreeSet::new();

        for &file_id in file_ids {
            let diagnostics = self
                .analysis
                .diagnose_file_with_shared(file_id, CancellationToken::new(), shared_snapshot.clone())
                .unwrap_or_else(|| {
                    let file = self
                        .analysis
                        .compilation
                        .get_db()
                        .get_vfs()
                        .get_file_path(&file_id)
                        .map(|path| normalize_snapshot_file(&self.virtual_url_generator.base, path))
                        .unwrap_or_else(|| format!("file-id:{}", file_id.id));
                    panic!(
                        "expected diagnostics vector for selected file while collecting shared snapshots: {}",
                        file
                    );
                });
            combined.extend(self.diagnostic_snapshots_for_file(file_id, diagnostics));
        }

        combined
    }

    #[cfg(test)]
    pub fn diagnostic_snapshots_for_file(
        &self,
        file_id: FileId,
        diagnostics: Vec<Diagnostic>,
    ) -> BTreeSet<DiagnosticSnapshot> {
        let normalized_file = self
            .analysis
            .compilation
            .get_db()
            .get_vfs()
            .get_file_path(&file_id)
            .map(|path| normalize_snapshot_file(&self.virtual_url_generator.base, path))
            .unwrap_or_else(|| format!("file-id:{}", file_id.id));

        diagnostics_to_snapshot_set(normalized_file, diagnostics)
    }

    /// Every main-workspace file's name/index expression types and
    /// diagnostics, in path order.
    #[cfg(test)]
    pub fn semantic_digest(&self) -> Vec<String> {
        use glua_parser::LuaExpr;

        let db = self.analysis.compilation.get_db();
        let mut files = db
            .get_module_index()
            .get_main_workspace_file_ids()
            .into_iter()
            .map(|file_id| {
                let path = db
                    .get_vfs()
                    .get_file_path(&file_id)
                    .map(|path| normalize_snapshot_file(&self.virtual_url_generator.base, path))
                    .unwrap_or_default();
                (path, file_id)
            })
            .collect::<Vec<_>>();
        files.sort();

        let mut digest = Vec::new();
        for (path, file_id) in files {
            let model = self
                .analysis
                .compilation
                .get_semantic_model(file_id)
                .expect("semantic model");
            for expr in model.get_root().descendants::<LuaExpr>() {
                if !matches!(expr, LuaExpr::NameExpr(_) | LuaExpr::IndexExpr(_)) {
                    continue;
                }
                let typ = model
                    .get_semantic_info(expr.syntax().clone().into())
                    .map(|info| self.humanize_type(info.display_typ().clone()))
                    .unwrap_or_default();
                let range = expr.syntax().text_range();
                digest.push(format!(
                    "{path}@{:?}: {} = {typ}",
                    range,
                    expr.syntax().text()
                ));
            }
            let diagnostics = self
                .analysis
                .diagnose_file(file_id, CancellationToken::new())
                .unwrap_or_default();
            digest.extend(
                diagnostics_to_snapshot_set(path, diagnostics)
                    .into_iter()
                    .map(|snapshot| format!("{snapshot:?}")),
            );
        }
        digest
    }

    /// The derivations a read of which no longer holds: after an update the
    /// engine must leave none.
    #[cfg(test)]
    pub fn stale_derivations(&self) -> Vec<crate::db_index::derivation::DerivationId> {
        use crate::db_index::derivation::DerivationKind;
        let db = self.analysis.compilation.get_db();
        let mut graph = db.reader_graph().clone();
        graph.forget_fingerprints();
        let mut stale = graph
            .derivations()
            .filter(|id| id.kind != DerivationKind::Diagnostics)
            .filter(|id| graph.is_stale(*id, |key| db.fact_fingerprint(key), |_, _| false))
            .collect::<Vec<_>>();
        stale.sort();
        stale
    }

    /// The derivations of a file that name no node of its current text.
    #[cfg(test)]
    pub fn phantom_derivations(&self) -> Vec<crate::db_index::derivation::DerivationId> {
        use crate::db_index::derivation::DerivationKind;
        let db = self.analysis.compilation.get_db();
        let mut phantoms = db
            .reader_graph()
            .derivations()
            .filter(|id| id.file_id != FileId::VIRTUAL && id.kind != DerivationKind::Diagnostics)
            .filter(|id| {
                if id.kind.is_query() {
                    let signature_id =
                        crate::LuaSignatureId::new(id.file_id, id.syntax_id().get_range().start());
                    return db.get_signature_index().get(&signature_id).is_none();
                }
                db.get_vfs()
                    .get_syntax_tree(&id.file_id)
                    .is_none_or(|tree| {
                        id.syntax_id()
                            .to_node_from_root(&tree.get_red_root())
                            .is_none()
                    })
            })
            .collect::<Vec<_>>();
        phantoms.sort();
        phantoms
    }

    /// Every type cache, type declaration, dynamic field, net flow and the
    /// semantic digest, as sorted text.
    #[cfg(test)]
    pub fn settled_digest(&self) -> Vec<String> {
        let db = self.analysis.compilation.get_db();
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
        settled.extend(db.get_type_index().iter_type_decls().map(|decl| {
            let mut locations = decl
                .get_locations()
                .iter()
                .map(|location| format!("{location:?}"))
                .collect::<Vec<_>>();
            locations.sort();
            format!("{:?} at {locations:?}", decl.get_id())
        }));
        settled.extend(db.get_type_index().iter_type_decls().filter_map(|decl| {
            let owner = crate::LuaMemberOwner::Type(decl.get_id());
            let mut members = db
                .get_member_index()
                .get_members(&owner)?
                .into_iter()
                .map(|member| format!("{:?}", member.get_id()))
                .collect::<Vec<_>>();
            members.sort();
            Some(format!("{:?} members {members:?}", decl.get_id()))
        }));
        let network = db.get_gmod_network_index();
        settled.extend(
            network
                .iter_send_flows()
                .map(|(file_id, _, flow)| format!("{file_id:?} sends {flow:?}")),
        );
        settled.extend(
            network
                .iter_receive_flows()
                .map(|(file_id, flow)| format!("{file_id:?} receives {flow:?}")),
        );
        settled.extend(self.semantic_digest());
        settled.sort();
        settled
    }

    /// Applies `edits` one at a time to a workspace holding `files`, builds a
    /// fresh workspace from the final texts, and asserts both digests match.
    /// `setup` configures each workspace before any file is added. Returns
    /// the incrementally edited workspace.
    #[cfg(test)]
    pub fn assert_warm_matches_cold(
        setup: impl Fn(&mut VirtualWorkspace),
        files: &[(&str, &str)],
        edits: &[(&str, &str)],
    ) -> VirtualWorkspace {
        let mut warm = VirtualWorkspace::new();
        setup(&mut warm);
        warm.def_files(files.to_vec());
        for (path, text) in edits {
            warm.def_file(path, text);
        }

        let mut final_files = files.to_vec();
        for (path, text) in edits {
            match final_files
                .iter_mut()
                .find(|(existing, _)| existing == path)
            {
                Some(entry) => entry.1 = text,
                None => final_files.push((path, text)),
            }
        }
        let mut cold = VirtualWorkspace::new();
        setup(&mut cold);
        cold.def_files(final_files);

        assert_eq!(warm.semantic_digest(), cold.semantic_digest());
        warm
    }
}

#[cfg(test)]
pub(crate) fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{id}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use crate::LuaType;

    use super::VirtualWorkspace;

    #[test]
    fn test_basic() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
        ---@class a
        "#,
        );

        let ty = ws.ty("a");
        match ty {
            LuaType::Ref(i) => {
                assert_eq!(i.get_name(), "a");
            }
            _ => unreachable!(),
        }
    }
}
