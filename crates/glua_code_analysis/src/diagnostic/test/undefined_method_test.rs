#[cfg(test)]
mod tests {
    use crate::{DiagnosticCode, Emmyrc, VirtualWorkspace};
    use glua_parser::{LuaAstNode, LuaAstToken};
    use lsp_types::{DiagnosticSeverity, NumberOrString};
    use tokio_util::sync::CancellationToken;

    fn diagnostics(ws: &mut VirtualWorkspace, source: &str) -> Vec<lsp_types::Diagnostic> {
        let file_id = ws.def(source);
        ws.analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
    }

    fn has_code(diagnostics: &[lsp_types::Diagnostic], code: DiagnosticCode) -> bool {
        let code = Some(NumberOrString::String(code.get_name().to_string()));
        diagnostics.iter().any(|diagnostic| diagnostic.code == code)
    }

    #[test]
    fn unknown_colon_call_reports_undefined_method_error_without_undefined_field() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@class MethodTest.Entity
            local Entity = {}

            ---@type MethodTest.Entity
            local entity
            entity:MissingMethod()
            "#,
        );

        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .expect("undefined-method diagnostic");
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(diagnostic.message, "Undefined method `MissingMethod`. ");
        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedField));
    }

    #[test]
    fn unknown_colon_call_in_condition_reports_undefined_method() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@class MethodTest.Conditional
            local Conditional = {}

            ---@type MethodTest.Conditional
            local value
            if value:MissingMethod() then
                print("unreachable")
            end
            "#,
        );

        assert!(has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn known_method_does_not_report_undefined_method() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@class MethodTest.Known
            local Known = {}
            function Known:PresentMethod() end

            ---@type MethodTest.Known
            local value
            value:PresentMethod()
            "#,
        );

        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn short_circuit_guarded_optional_method_does_not_report() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@class MethodTest.Optional
            local Optional = {}

            ---@type MethodTest.Optional
            local value
            if value.OptionalMethod and value:OptionalMethod() then
                print("optional")
            end
            "#,
        );

        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn truthy_player_or_false_result_allows_player_methods() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def(
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field IsActive fun(self: Player): boolean
            ---@field Nick fun(self: Player): string

            ---@return boolean
            ---@return_cast self Player
            function Entity:IsPlayer() end

            ---@param value any
            ---@return TypeGuard<any>
            function IsValid(value) end

            local function IsPlayer(ent)
                return IsValid(ent) and ent:IsPlayer()
            end

            ---@return Entity
            function FindEntity() end

            local function FindPlayer()
                local ent = FindEntity()
                if not ent then return false end
                return (IsPlayer(ent) and ent:IsActive()) and ent or false
            end

            local target = FindPlayer()
            if target then
                target:Nick()
            end
            "#,
        );
        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn gamemode_methods_defined_across_files_are_visible() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_file(
            "annotations/gm.lua",
            r#"
            ---@class GM
            GM = {}
            ---@type GM
            GAMEMODE = nil
            "#,
        );
        let file_ids = ws.def_files(vec![
            (
                "gamemodes/terrortown/gamemode/cl_init.lua",
                r#"
                function GM:InitializeClient()
                    GAMEMODE:ClearClientState()
                end
                "#,
            ),
            (
                "gamemodes/terrortown/gamemode/client_state.lua",
                r#"
                function GM:ClearClientState() end
                "#,
            ),
        ]);

        let diagnostics = ws
            .analysis
            .diagnose_file(file_ids[0], CancellationToken::new())
            .unwrap_or_default();
        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn early_return_valid_player_guard_preserves_player_methods() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@class Entity
            ---@class Player: Entity
            ---@field IsActive fun(self: Player): boolean
            ---@field Nick fun(self: Player): string

            player = {}
            ---@return Player|false
            function player.GetBySteamID64(id) end

            ---@param value any
            ---@return TypeGuard<any>
            ---@return_cast value -NULL
            ---@[valid_guard]
            function IsValid(value) end

            ---@param ply Player
            local function Transfer(id, ply)
                local target = player.GetBySteamID64(id)
                if not IsValid(target) or not target:IsActive() or target == ply then return end
                target:Nick()
            end
            "#,
        );

        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn detached_entity_isvalid_guard_preserves_player_methods() {
        let diagnostics = diagnostics(
            &mut VirtualWorkspace::new(),
            r#"
            ---@attribute self_guard(member: string)

            ---@class Entity
            ---@class Player: Entity
            ---@class NULL: Entity

            Entity = {}
            Player = {}
            function Player:Name() end
            function Player:SteamID() end

            ---@return boolean
            ---@return_cast self Entity
            ---@[self_guard("gmod.entity")]
            function Entity:IsValid() end

            ---@generic T
            ---@param name `T`
            ---@return T
            function FindMetaTable(name) end

            local IsValid = FindMetaTable("Entity").IsValid
            ---@type Player|NULL
            local ply
            if IsValid(ply) then
                ply:Name()
                ply:SteamID()
            end
            "#,
        );

        assert!(
            !has_code(&diagnostics, DiagnosticCode::UndefinedMethod),
            "detached Entity:IsValid should preserve Player methods, got {diagnostics:?}"
        );
    }

    #[test]
    fn dynamic_callback_table_uses_call_site_argument_type() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def(
            r#"
            ---@class ScoreReport
            local ScoreReport = {}
            function ScoreReport:BuildSummaryPanel() end
            function ScoreReport:BuildEventLogPanel() end

            local tabs = {
                summary = function(panel)
                    panel:BuildSummaryPanel()
                end,
                events = function(panel)
                    panel:BuildEventLogPanel()
                end,
            }

            function ScoreReport:Show(selected)
                tabs[selected](self)
            end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        assert!(!has_code(&diagnostics, DiagnosticCode::UndefinedMethod));
    }

    #[test]
    fn later_same_file_global_field_assign_in_other_function_types_earlier_read() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@class DForm: Panel
            ---@class ControlPanel: DForm
            local ControlPanel = {}
            function ControlPanel:Help(text) end
            function ControlPanel:Clear() end

            ---@param value any
            ---@return TypeGuard<any>
            ---@return_cast value -NULL
            ---@[valid_guard]
            function IsValid(value) end

            G = G or {}

            function G.Use()
                local panel = G.panel
                if not IsValid(panel) then return end
                panel:Help("x")
                panel:Clear()
            end

            ---@param panel ControlPanel
            function G.Init(panel)
                G.panel = panel
            end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        assert!(
            !has_code(&diagnostics, DiagnosticCode::UndefinedMethod),
            "cross-function later FileDefine should type earlier read, got {diagnostics:?}"
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let panel_local = semantic_model
            .get_root()
            .descendants::<glua_parser::LuaLocalName>()
            .find(|local_name| {
                local_name
                    .get_name_token()
                    .is_some_and(|token| token.get_name_text() == "panel")
            })
            .expect("panel local");
        let panel_ty = semantic_model
            .get_semantic_info(
                panel_local
                    .get_name_token()
                    .expect("name token")
                    .syntax()
                    .clone()
                    .into(),
            )
            .map(|info| info.display_typ().clone())
            .expect("panel type");
        let humanized = ws.humanize_type(panel_ty);
        assert!(
            humanized.contains("ControlPanel"),
            "expected ControlPanel for earlier cross-function read, got {humanized}"
        );
    }

    #[test]
    fn real_glide_transmission_tool_panel_help_is_defined() {
        use std::path::PathBuf;

        let annotations = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../annotations-gmod-glua-ls/output");
        let vehicle_base =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../cityrp-vehicle-base");
        let stool = vehicle_base.join("lua/weapons/gmod_tool/stools/glide_transmission_editor.lua");
        let glide_autorun = vehicle_base.join("lua/autorun/sh_glide.lua");
        if !annotations.is_dir() || !stool.is_file() || !glide_autorun.is_file() {
            // Adjacent checkouts are optional on CI; unit fixtures cover the rule.
            return;
        }

        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.analysis.add_library_workspace(annotations.clone());

        // Load a representative subset of panel/tool annotations so the test stays
        // bounded while still using real hierarchy + Help/BuildCPanel signatures.
        for name in [
            "panel.lua",
            "dcollapsiblecategory.lua",
            "dform.lua",
            "controlpanel.lua",
            "controlpresets.lua",
            "dcheckboxlabel.lua",
            "dgrid.lua",
            "dnotify.lua",
            "tool.lua",
            "global.lua",
        ] {
            let path = annotations.join(name);
            if !path.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read annotation");
            let uri = lsp_types::Uri::parse_from_file_path(&path).expect("uri");
            ws.analysis.update_file_by_uri(&uri, Some(text));
        }

        let glide_text = std::fs::read_to_string(&glide_autorun).expect("read glide");
        let glide_uri = lsp_types::Uri::parse_from_file_path(&glide_autorun).expect("glide uri");
        ws.analysis.update_file_by_uri(&glide_uri, Some(glide_text));

        let stool_text = std::fs::read_to_string(&stool).expect("read stool");
        let stool_uri = lsp_types::Uri::parse_from_file_path(&stool).expect("stool uri");
        let file_id = ws
            .analysis
            .update_file_by_uri(&stool_uri, Some(stool_text))
            .expect("stool file id");

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");

        let mut panel_locals = Vec::new();
        for local_name in semantic_model
            .get_root()
            .descendants::<glua_parser::LuaLocalName>()
        {
            if local_name
                .get_name_token()
                .is_some_and(|token| token.get_name_text() == "panel")
            {
                let ty = semantic_model
                    .get_semantic_info(
                        local_name
                            .get_name_token()
                            .expect("name token")
                            .syntax()
                            .clone()
                            .into(),
                    )
                    .map(|info| ws.humanize_type(info.display_typ().clone()))
                    .unwrap_or_else(|| "<no-info>".into());
                panel_locals.push(ty);
            }
        }

        let field_types: Vec<String> = semantic_model
            .get_root()
            .descendants::<glua_parser::LuaIndexExpr>()
            .filter(|index| format!("{}", index.syntax().text()).contains("transmissionToolPanel"))
            .filter_map(|index| {
                semantic_model
                    .infer_expr(glua_parser::LuaExpr::IndexExpr(index))
                    .ok()
                    .map(|ty| ws.humanize_type(ty))
            })
            .collect();

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let help_undefined = diagnostics.iter().any(|diagnostic| {
            diagnostic.code
                == Some(NumberOrString::String(
                    DiagnosticCode::UndefinedMethod.get_name().to_string(),
                ))
                && diagnostic.message.contains(
                    "For more information on a specific command, type HELP command-name
ASSOC          Displays or modifies file extension associations.
ATTRIB         Displays or changes file attributes.
BREAK          Sets or clears extended CTRL+C checking.
BCDEDIT        Sets properties in boot database to control boot loading.
CACLS          Displays or modifies access control lists (ACLs) of files.
CALL           Calls one batch program from another.
CD             Displays the name of or changes the current directory.
CHCP           Displays or sets the active code page number.
CHDIR          Displays the name of or changes the current directory.
CHKDSK         Checks a disk and displays a status report.
CHKNTFS        Displays or modifies the checking of disk at boot time.
CLS            Clears the screen.
CMD            Starts a new instance of the Windows command interpreter.
COLOR          Sets the default console foreground and background colors.
COMP           Compares the contents of two files or sets of files.
COMPACT        Displays or alters the compression of files on NTFS partitions.
CONVERT        Converts FAT volumes to NTFS.  You cannot convert the
               current drive.
COPY           Copies one or more files to another location.
DATE           Displays or sets the date.
DEL            Deletes one or more files.
DIR            Displays a list of files and subdirectories in a directory.
DISKPART       Displays or configures Disk Partition properties.
DOSKEY         Edits command lines, recalls Windows commands, and 
               creates macros.
DRIVERQUERY    Displays current device driver status and properties.
ECHO           Displays messages, or turns command echoing on or off.
ENDLOCAL       Ends localization of environment changes in a batch file.
ERASE          Deletes one or more files.
EXIT           Quits the CMD.EXE program (command interpreter).
FC             Compares two files or sets of files, and displays the 
               differences between them.
FIND           Searches for a text string in a file or files.
FINDSTR        Searches for strings in files.
FOR            Runs a specified command for each file in a set of files.
FORMAT         Formats a disk for use with Windows.
FSUTIL         Displays or configures the file system properties.
FTYPE          Displays or modifies file types used in file extension 
               associations.
GOTO           Directs the Windows command interpreter to a labeled line in 
               a batch program.
GPRESULT       Displays Group Policy information for machine or user.
HELP           Provides Help information for Windows commands.
ICACLS         Display, modify, backup, or restore ACLs for files and 
               directories.
IF             Performs conditional processing in batch programs.
LABEL          Creates, changes, or deletes the volume label of a disk.
MD             Creates a directory.
MKDIR          Creates a directory.
MKLINK         Creates Symbolic Links and Hard Links
MODE           Configures a system device.
MORE           Displays output one screen at a time.
MOVE           Moves one or more files from one directory to another 
               directory.
OPENFILES      Displays files opened by remote users for a file share.
PATH           Displays or sets a search path for executable files.
PAUSE          Suspends processing of a batch file and displays a message.
POPD           Restores the previous value of the current directory saved by 
               PUSHD.
PRINT          Prints a text file.
PROMPT         Changes the Windows command prompt.
PUSHD          Saves the current directory then changes it.
RD             Removes a directory.
RECOVER        Recovers readable information from a bad or defective disk.
REM            Records comments (remarks) in batch files or CONFIG.SYS.
REN            Renames a file or files.
RENAME         Renames a file or files.
REPLACE        Replaces files.
RMDIR          Removes a directory.
ROBOCOPY       Advanced utility to copy files and directory trees
SET            Displays, sets, or removes Windows environment variables.
SETLOCAL       Begins localization of environment changes in a batch file.
SC             Displays or configures services (background processes).
SCHTASKS       Schedules commands and programs to run on a computer.
SHIFT          Shifts the position of replaceable parameters in batch files.
SHUTDOWN       Allows proper local or remote shutdown of machine.
SORT           Sorts input.
START          Starts a separate window to run a specified program or command.
SUBST          Associates a path with a drive letter.
SYSTEMINFO     Displays machine specific properties and configuration.
TASKLIST       Displays all currently running tasks including services.
TASKKILL       Kill or stop a running process or application.
TIME           Displays or sets the system time.
TITLE          Sets the window title for a CMD.EXE session.
TREE           Graphically displays the directory structure of a drive or 
               path.
TYPE           Displays the contents of a text file.
VER            Displays the Windows version.
VERIFY         Tells Windows whether to verify that your files are written
               correctly to a disk.
VOL            Displays a disk volume label and serial number.
XCOPY          Copies files and directory trees.
WMIC           Displays WMI information inside interactive command shell.

For more information on tools see the command-line reference in the online help.",
                )
        });
        assert!(
            !help_undefined,
            "real glide transmission editor must not report undefined-method Help; panel_locals={panel_locals:?}; field_types={field_types:?}; diagnostics={diagnostics:?}"
        );
        assert!(
            panel_locals.iter().any(|ty| ty.contains("ControlPanel")),
            "expected ControlPanel for local panel from Glide.transmissionToolPanel, got panel_locals={panel_locals:?}; field_types={field_types:?}"
        );
    }

    #[test]
    fn later_class_global_field_assign_from_buildcpanel_types_earlier_read() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        // Mirror real annotations: Help/Clear/Button on DForm, AddItem also on
        // other Panel children that must not steal the ControlPanel binding.
        let file_id = ws.def_file(
            "lua/weapons/gmod_tool/stools/glide_transmission_editor.lua",
            r#"
            ---@class Panel
            ---@field x number
            ---@field y number
            function Panel:Clear() end

            ---@class DCollapsibleCategory: Panel
            ---@class DForm: DCollapsibleCategory
            function DForm:Help(text) end
            function DForm:Clear() end
            function DForm:AddItem(left, right) end
            function DForm:Button(text) end

            ---@class ControlPanel: DForm
            ---@class ControlPresets: Panel
            function ControlPresets:AddItem(left, right) end
            ---@class DCheckBoxLabel: Panel
            function DCheckBoxLabel:AddItem(left, right) end
            ---@class DGrid: Panel
            function DGrid:AddItem(left, right) end
            ---@class DNotify: Panel
            function DNotify:AddItem(left, right) end

            ---@param value any
            ---@return TypeGuard<any>
            ---@return_cast value -NULL
            ---@[valid_guard]
            function IsValid(value) end

            ---@class Glide
            Glide = Glide or {}

            ---@class Tool
            ---@field BuildCPanel fun(panel: ControlPanel)
            ---@class TOOL: Tool
            TOOL = {}

            if not CLIENT then return end

            function Glide.RefreshTransmissionToolPanel()
                local panel = Glide.transmissionToolPanel
                if not IsValid(panel) then return end
                panel:Clear()
                panel:Help("desc")
                local row = panel
                panel:AddItem(row)
                panel:Button("add")
            end

            function TOOL.BuildCPanel(panel)
                Glide.transmissionToolPanel = panel
                Glide.RefreshTransmissionToolPanel()
            end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        assert!(
            !has_code(&diagnostics, DiagnosticCode::UndefinedMethod),
            "class-global later FileDefine from BuildCPanel should type earlier read, got {diagnostics:?}"
        );

        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("semantic model");
        let panel_local = semantic_model
            .get_root()
            .descendants::<glua_parser::LuaLocalName>()
            .find(|local_name| {
                local_name
                    .get_name_token()
                    .is_some_and(|token| token.get_name_text() == "panel")
            })
            .expect("panel local");
        let panel_ty = semantic_model
            .get_semantic_info(
                panel_local
                    .get_name_token()
                    .expect("name token")
                    .syntax()
                    .clone()
                    .into(),
            )
            .map(|info| info.display_typ().clone())
            .expect("panel type");
        let humanized = ws.humanize_type(panel_ty);
        assert!(
            humanized.contains("ControlPanel"),
            "expected ControlPanel for Glide.transmissionToolPanel read, got {humanized}"
        );
    }

    #[test]
    fn same_function_later_global_field_assign_stays_order_sensitive() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def(
            r#"
            G = G or {}

            function G.Use()
                A = G.field
                ---@type string
                G.field = "s"
                B = G.field
            end
            "#,
        );

        let before_ty = ws.expr_ty("A");
        let after_ty = ws.expr_ty("B");
        assert_ne!(
            ws.humanize_type(before_ty.clone()),
            ws.humanize_type(after_ty.clone()),
            "same-function later assign must not type earlier read as the later type; before={}, after={}",
            ws.humanize_type(before_ty),
            ws.humanize_type(after_ty)
        );
        assert_eq!(ws.humanize_type(after_ty), "string");
    }

    #[test]
    fn indexed_panel_parent_returns_known_parent_type_and_reports_unknown_methods() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class ParentPanel: Panel
            local ParentPanel = {}
            function ParentPanel:UpdatePlayerData() end
            function ParentPanel:CreateChild()
                return vgui.Create("ChildPanel", self)
            end
            function ParentPanel:CreateAddedChild()
                return self:Add("AddedChildPanel")
            end

            ---@class AlternateParentPanel: Panel
            local AlternateParentPanel = {}
            function AlternateParentPanel:UpdatePlayerData() end
            function AlternateParentPanel:CreateChild()
                return vgui.Create("ChildPanel", self)
            end

            ---@class ChildPanel: Panel
            local ChildPanel = {}
            function ChildPanel:UpdateParent()
                self:GetParent():UpdatePlayerData()
                self:GetParent():DefinitelyMissing()
            end

            ---@class AddedChildPanel: Panel
            local AddedChildPanel = {}
            function AddedChildPanel:UpdateParent()
                self:GetParent():UpdatePlayerData()
            end
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default();
        let undefined_methods = diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            undefined_methods,
            ["Undefined method `DefinitelyMissing`. "]
        );
    }

    #[test]
    fn vgui_parent_chain_resolves_add_panel_canvas_to_owner() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class PANEL: Panel
            PANEL = Panel
            local PANEL = {}
            function PANEL:EditorMethod() end
            function PANEL:Init()
                self.tabContainer = vgui.Create("DHorizontalScroller", self)
            end
            function PANEL:AddTab()
                local tab = {}
                tab.button = vgui.Create("StreamTabButton")
                self.tabContainer:AddPanel(tab.button)
            end
            vgui.Register("StreamEditor", PANEL, "Panel")

            ---@class StreamTab
            ---@field button StreamTabButton
            ---@class StreamTabButton: Panel
            ---@field GetParent fun(self: StreamTabButton): Panel
            local StreamTabButton = {}
            function StreamTabButton:UseEditor()
                self:GetParent():GetParent():GetParent():EditorMethod()
                self:GetParent():GetParent():GetParent():Missing()
            end

            vgui.Register("StreamTabButton", StreamTabButton, "Panel")
            "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert_eq!(
            metadata.get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("StreamTabButton")),
            Some(
                [
                    crate::LuaTypeDeclId::global("DDragBase"),
                    crate::LuaTypeDeclId::global("DHorizontalScroller"),
                    crate::LuaTypeDeclId::global("StreamEditor"),
                ]
                .as_slice()
            )
        );

        let undefined_methods = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>();
        assert_eq!(undefined_methods, ["Undefined method `Missing`. "]);
    }

    #[test]
    fn vgui_parent_chain_resolves_create_parent_field_assignment() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class DPanel: Panel

            ---@class TabButton: Panel
            local TabButton = {}
            function TabButton:Click()
                self:GetParent():GetParent():SetActiveTab()
                self:GetParent():GetParent():Missing()
            end
            vgui.Register("TabButton", TabButton, "Panel")

            ---@class TabbedFrame: Panel
            local TabbedFrame = {}
            function TabbedFrame:SetActiveTab() end
            function TabbedFrame:Init()
                self.tabList = vgui.Create("DPanel", self)
            end
            function TabbedFrame:AddTab()
                local button = vgui.Create("TabButton", self.tabList)
            end
            vgui.Register("TabbedFrame", TabbedFrame, "Panel")
            "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert_eq!(
            metadata.get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("TabButton")),
            Some(
                [
                    crate::LuaTypeDeclId::global("DPanel"),
                    crate::LuaTypeDeclId::global("TabbedFrame"),
                ]
                .as_slice()
            )
        );

        let undefined_methods = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>();
        assert_eq!(undefined_methods, ["Undefined method `Missing`. "]);
    }

    #[test]
    fn vgui_parent_chain_rejects_disagreeing_create_parent_field_owners() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.def(
            r#"
            ---@class Panel
            ---@class DPanel: Panel

            ---@class TabButton: Panel
            local TabButton = {}
            vgui.Register("TabButton", TabButton, "Panel")

            ---@class OwnerA: Panel
            local OwnerA = {}
            function OwnerA:Init()
                self.tabList = vgui.Create("DPanel", self)
            end
            function OwnerA:AddTab()
                local button = vgui.Create("TabButton", self.tabList)
            end
            vgui.Register("OwnerA", OwnerA, "Panel")

            ---@class OwnerB: Panel
            local OwnerB = {}
            function OwnerB:Init()
                self.tabList = vgui.Create("DPanel", self)
            end
            function OwnerB:AddTab()
                local button = vgui.Create("TabButton", self.tabList)
            end
            vgui.Register("OwnerB", OwnerB, "Panel")
            "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert!(
            metadata
                .get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("TabButton"))
                .is_none()
        );
    }

    #[test]
    fn vgui_parent_chain_does_not_use_another_scroller_owner() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let _file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class OwnerA: Panel
            local OwnerA = {}
            function OwnerA:OwnerAMethod() end
            ---@param externalOwner Panel
            function OwnerA:Init(externalOwner)
                self.tabContainer = vgui.Create("DHorizontalScroller", externalOwner)
                self.otherScroller = vgui.Create("DHorizontalScroller", self)
            end
            ---@param child Child
            function OwnerA:AddChild(child)
                self.tabContainer:AddPanel(child)
            end

            ---@class Child: Panel
            local Child = {}
            function Child:UseParent()
                self:GetParent():GetParent():GetParent():OwnerAMethod()
            end
            vgui.Register("Child", Child, "Panel")
            "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        let child = crate::LuaTypeDeclId::global("Child");
        assert_eq!(
            metadata.get_vgui_panel_parent_chain(&child),
            Some(
                [
                    crate::LuaTypeDeclId::global("DDragBase"),
                    crate::LuaTypeDeclId::global("DHorizontalScroller"),
                    crate::LuaTypeDeclId::global("Panel"),
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn stream_editor_tab_button_resolves_parent_chain() {
        use std::path::PathBuf;

        let annotations = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../annotations-gmod-glua-ls/output");
        let vehicle_base =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../cityrp-vehicle-base");
        let stream_editor = vehicle_base.join("lua/glide/client/vgui/stream_editor.lua");
        if !annotations.is_dir() || !stream_editor.is_file() {
            return;
        }

        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        ws.analysis.add_library_workspace(annotations.clone());
        ws.analysis.add_main_workspace(vehicle_base.clone());

        let mut files = Vec::new();
        for entry in std::fs::read_dir(&annotations).expect("read annotations") {
            let path = entry.expect("read annotation entry").path();
            if path.extension().is_none_or(|extension| extension != "lua") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read annotation");
            files.push((path, Some(text)));
        }
        files.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        let vehicle_files =
            crate::load_workspace_files(&vehicle_base, &["**/*.lua".to_string()], &[], &[], None)
                .expect("read vehicle base");
        files.extend(
            vehicle_files
                .into_iter()
                .map(crate::LuaFileInfo::into_tuple),
        );
        ws.analysis.update_files_by_path(files);
        let uri = lsp_types::Uri::parse_from_file_path(&stream_editor).expect("stream editor uri");
        let file_id = ws
            .analysis
            .get_file_id(&uri)
            .expect("stream editor file id");

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert_eq!(
            metadata.get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global(
                "Styled_StreamEditorTabButton",
            )),
            Some(
                [
                    crate::LuaTypeDeclId::global("DDragBase"),
                    crate::LuaTypeDeclId::global("DHorizontalScroller"),
                    crate::LuaTypeDeclId::global("Glide_EngineStreamEditor"),
                ]
                .as_slice()
            )
        );

        let undefined_methods = ws
            .analysis
            .diagnose_file(file_id, CancellationToken::new())
            .unwrap_or_default()
            .into_iter()
            .filter(|diagnostic| {
                diagnostic.code
                    == Some(NumberOrString::String(
                        DiagnosticCode::UndefinedMethod.get_name().to_string(),
                    ))
            })
            .map(|diagnostic| diagnostic.message)
            .collect::<Vec<_>>();
        assert!(
            !undefined_methods.iter().any(|message| {
                message.contains("SetActiveTabById") || message.contains("CloseTabById")
            }),
            "expected the tab button parent chain to reach Glide_EngineStreamEditor, got {undefined_methods:?}"
        );
    }

    #[test]
    fn vgui_parent_chain_rejects_disagreeing_owners() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let _file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class OwnerA: Panel
            local OwnerA = {}
            function OwnerA:OnlyOwnerA() end
            function OwnerA:MakeChild()
                return vgui.Create("SharedChild", self)
            end

            ---@class OwnerB: Panel
            local OwnerB = {}
            function OwnerB:MakeChild()
                return vgui.Create("SharedChild", self)
            end

            ---@class SharedChild: Panel
            local SharedChild = {}
            function SharedChild:UseParent()
                self:GetParent():OnlyOwnerA()
            end
            "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert!(
            metadata
                .get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("SharedChild"))
                .is_none()
        );
    }

    #[test]
    fn vgui_parent_chain_supports_typed_set_parent_and_fails_closed_at_depth() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let _file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class TypedOwner: Panel
            ---@field GetParent fun(self: TypedOwner): Panel
            local TypedOwner = {}
            function TypedOwner:OwnerMethod() end

            ---@class TypedChild: Panel
            ---@field GetParent fun(self: TypedChild): Panel
            local TypedChild = {}
            ---@param owner TypedOwner
            function TypedChild:Attach(owner)
                local child = self
                child:SetParent(owner)
            end
            function TypedChild:UseOwner()
                self:GetParent():OwnerMethod()
                self:GetParent():GetParent():OwnerMethod()
            end

            vgui.Register("TypedChild", TypedChild, "Panel")
            "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        assert_eq!(
            metadata.get_vgui_panel_parent_chain(&crate::LuaTypeDeclId::global("TypedChild")),
            Some([crate::LuaTypeDeclId::global("TypedOwner")].as_slice())
        );
    }

    #[test]
    fn vgui_parent_chain_marks_omitted_set_parent_incomplete() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();
        let _file_id = ws.def(
            r#"
            ---@class Panel
            ---@field GetParent fun(self: Panel): Panel
            ---@class Child: Panel
            local Child = {}
            function Child:Detach()
                self:SetParent()
            end
            vgui.Register("Child", Child, "Panel")
            "#,
        );

        let metadata = ws
            .analysis
            .compilation
            .get_db()
            .get_gmod_class_metadata_index();
        let child = crate::LuaTypeDeclId::global("Child");
        assert!(metadata.get_vgui_panel_parent_chain(&child).is_none());
        assert!(!metadata.vgui_panel_parent_chain_is_complete(&child));
    }
}
