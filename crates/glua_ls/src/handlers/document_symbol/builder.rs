use std::collections::HashMap;

use glua_code_analysis::{
    DbIndex, EmmyrcGmodOutlineVerbosity, FileId, GmodClassCallLiteral, GmodHookKind,
    GmodScriptedClassCallMetadata, GmodTimerKind, LuaDecl, LuaDeclId, LuaDeclarationTree,
    LuaDocument, LuaType, LuaTypeOwner, get_scripted_class_info_for_file,
};
use glua_parser::{LuaAstNode, LuaChunk, LuaSyntaxId, LuaSyntaxNode, LuaSyntaxToken};
use lsp_types::{DocumentSymbol, SymbolKind};
use rowan::{TextRange, TextSize};

/// An entry in the gmod-call symbol map keyed by the call-expression syntax id.
pub struct GmodCallEntry {
    pub kind: SymbolKind,
    pub label: String,
    /// 0-based index of the callback closure argument in the call expression.
    pub callback_arg_index: Option<usize>,
}

struct ScriptedClassInfo {
    /// Human-readable class name (e.g. "my_entity").
    #[allow(dead_code)]
    class_name: String,
    /// Global variable name used in the file (e.g. "ENT", "SWEP", "TOOL").
    global_name: &'static str,
    /// Label to show in the outline (e.g. "my_entity (Entity)").
    class_label: String,
}

pub struct DocumentSymbolBuilder<'a> {
    db: &'a DbIndex,
    decl_tree: &'a LuaDeclarationTree,
    document: &'a LuaDocument<'a>,
    document_symbols: HashMap<LuaSyntaxId, Box<LuaSymbol>>,
    decl_symbol_ids: HashMap<LuaDeclId, LuaSyntaxId>,
    /// Maps a variable's decl id to its display name for VGUI panels and scripted entity classes.
    vgui_panel_names: HashMap<LuaDeclId, String>,
    /// Maps call-expression syntax ids to gmod-specific symbol data (hook.Add, net.Receive, …).
    gmod_call_map: HashMap<LuaSyntaxId, GmodCallEntry>,
    /// Information about the scripted entity class this file belongs to (if any).
    scripted_class_info: Option<ScriptedClassInfo>,
    /// Syntax id of the lazily-created top-level class symbol for scripted entities.
    scripted_class_symbol_id: Option<LuaSyntaxId>,
    /// Outline verbosity setting from config.
    verbosity: EmmyrcGmodOutlineVerbosity,
    /// Whether GMod analysis is enabled.
    gmod_enabled: bool,
}

impl<'a> DocumentSymbolBuilder<'a> {
    pub fn new(
        db: &'a DbIndex,
        decl_tree: &'a LuaDeclarationTree,
        document: &'a LuaDocument,
    ) -> Self {
        let emmyrc = db.get_emmyrc();
        let gmod_enabled = emmyrc.gmod.enabled;
        let verbosity = if gmod_enabled {
            emmyrc.gmod.outline.verbosity
        } else {
            // Keep non-GMod workspaces on legacy outline behavior unless GMod analysis is enabled.
            EmmyrcGmodOutlineVerbosity::Verbose
        };

        let file_id = document.get_file_id();
        let vgui_panel_names =
            Self::collect_class_panel_names(db, decl_tree, file_id, gmod_enabled);
        let gmod_call_map = Self::collect_gmod_call_map(db, file_id, gmod_enabled);
        let scripted_class_info = if gmod_enabled {
            get_scripted_class_info_for_file(db, file_id).map(|(class_name, global_name)| {
                let type_label = scripted_class_type_label(global_name);
                let class_label = format!("{class_name} ({type_label})");
                ScriptedClassInfo {
                    class_name,
                    global_name,
                    class_label,
                }
            })
        } else {
            None
        };

        Self {
            db,
            decl_tree,
            document,
            document_symbols: HashMap::new(),
            decl_symbol_ids: HashMap::new(),
            vgui_panel_names,
            gmod_call_map,
            scripted_class_info,
            scripted_class_symbol_id: None,
            verbosity,
            gmod_enabled,
        }
    }

    /// Collect panel/class names for VGUI panels, derma controls, and scripted entity classes.
    fn collect_class_panel_names(
        db: &DbIndex,
        decl_tree: &LuaDeclarationTree,
        file_id: FileId,
        gmod_enabled: bool,
    ) -> HashMap<LuaDeclId, String> {
        let mut panel_names = HashMap::new();
        if !gmod_enabled {
            return panel_names;
        }

        if let Some(file_metadata) = db
            .get_gmod_class_metadata_index()
            .get_file_metadata(&file_id)
        {
            for call in &file_metadata.vgui_register_calls {
                Self::collect_vgui_panel_call(decl_tree, call, 1, &mut panel_names);
            }
            for call in &file_metadata.derma_define_control_calls {
                Self::collect_vgui_panel_call(decl_tree, call, 2, &mut panel_names);
            }
        }

        // Scripted entity classes (ENT, SWEP, TOOL, EFFECT, PLUGIN).
        if let Some((class_name, global_name)) = get_scripted_class_info_for_file(db, file_id) {
            let type_label = scripted_class_type_label(global_name);
            let class_label = format!("{class_name} ({type_label})");
            let class_decl_id = decl_tree
                .get_decls()
                .values()
                .filter(|decl| decl.get_name() == global_name)
                .min_by_key(|decl| decl.get_position())
                .map(|decl| decl.get_id());
            if let Some(class_decl_id) = class_decl_id {
                panel_names.insert(class_decl_id, class_label);
            }
        }

        panel_names
    }

    fn collect_vgui_panel_call(
        decl_tree: &LuaDeclarationTree,
        call: &GmodScriptedClassCallMetadata,
        table_var_arg_index: usize,
        panel_names: &mut HashMap<LuaDeclId, String>,
    ) {
        let panel_name = match call.literal_args.first() {
            Some(Some(GmodClassCallLiteral::String(name))) if !name.is_empty() => name,
            _ => return,
        };

        let table_var_name = match call.literal_args.get(table_var_arg_index) {
            Some(Some(GmodClassCallLiteral::NameRef(name))) if !name.is_empty() => name,
            _ => return,
        };

        let register_position = call.syntax_id.get_range().start();
        let Some(decl) = decl_tree.find_local_decl(table_var_name, register_position) else {
            return;
        };

        panel_names.insert(decl.get_id(), format!("{panel_name} (VGUI)"));
    }

    /// Build the lookup map from gmod call-expression syntax ids to outline entries.
    fn collect_gmod_call_map(
        db: &DbIndex,
        file_id: FileId,
        gmod_enabled: bool,
    ) -> HashMap<LuaSyntaxId, GmodCallEntry> {
        let mut map = HashMap::new();
        if !gmod_enabled {
            return map;
        }

        let infer_index = db.get_gmod_infer_index();

        // hook.Add / gamemode method hooks
        if let Some(hook_meta) = infer_index.get_hook_file_metadata(&file_id) {
            for site in &hook_meta.sites {
                if site.kind != GmodHookKind::Add {
                    continue;
                }
                let label = match &site.hook_name {
                    Some(name) if !name.is_empty() => format!("hook: {name}"),
                    _ => "hook: (dynamic)".to_string(),
                };
                map.insert(
                    site.syntax_id,
                    GmodCallEntry {
                        kind: SymbolKind::EVENT,
                        label,
                        callback_arg_index: Some(2),
                    },
                );
            }
        }

        // System calls: net.Receive, concommand.Add, timer.Create, timer.Simple
        if let Some(sys_meta) = infer_index.get_system_file_metadata(&file_id) {
            for site in &sys_meta.net_receive_calls {
                let label = match &site.message_name {
                    Some(name) if !name.is_empty() => format!("net.Receive: {name}"),
                    _ => "net.Receive: (dynamic)".to_string(),
                };
                map.insert(
                    site.syntax_id,
                    GmodCallEntry {
                        kind: SymbolKind::EVENT,
                        label,
                        callback_arg_index: Some(1),
                    },
                );
            }

            for site in &sys_meta.concommand_add_calls {
                let label = match &site.command_name {
                    Some(name) if !name.is_empty() => format!("concommand: {name}"),
                    _ => "concommand: (dynamic)".to_string(),
                };
                map.insert(
                    site.syntax_id,
                    GmodCallEntry {
                        kind: SymbolKind::FUNCTION,
                        label,
                        callback_arg_index: Some(1),
                    },
                );
            }

            for site in &sys_meta.timer_calls {
                let label = match &site.timer_name {
                    Some(name) if !name.is_empty() => format!("timer: {name}"),
                    _ => "timer.Simple".to_string(),
                };
                let callback_arg_index = match site.kind {
                    GmodTimerKind::Create => Some(3),
                    GmodTimerKind::Simple => Some(1),
                };
                map.insert(
                    site.syntax_id,
                    GmodCallEntry {
                        kind: SymbolKind::EVENT,
                        label,
                        callback_arg_index,
                    },
                );
            }
        }

        map
    }

    pub fn get_file_id(&self) -> FileId {
        self.document.get_file_id()
    }

    pub fn get_decl(&self, id: &LuaDeclId) -> Option<&LuaDecl> {
        self.decl_tree.get_decl(id)
    }

    pub fn resolve_local_decl_id(&self, name: &str, position: TextSize) -> Option<LuaDeclId> {
        self.decl_tree
            .find_local_decl(name, position)
            .map(|decl| decl.get_id())
    }

    pub fn get_vgui_panel_name(&self, decl_id: &LuaDeclId) -> Option<&str> {
        self.vgui_panel_names.get(decl_id).map(String::as_str)
    }

    pub fn get_gmod_call_entry(&self, syntax_id: &LuaSyntaxId) -> Option<&GmodCallEntry> {
        self.gmod_call_map.get(syntax_id)
    }

    pub fn get_verbosity(&self) -> EmmyrcGmodOutlineVerbosity {
        self.verbosity
    }

    pub fn is_gmod_enabled(&self) -> bool {
        self.gmod_enabled
    }

    /// Returns true if a symbol with the given type is "interesting" enough to show at the given
    /// verbosity level.  Always true for `Verbose`.
    pub fn is_type_interesting(&self, ty: &LuaType) -> bool {
        match self.verbosity {
            EmmyrcGmodOutlineVerbosity::Verbose => true,
            EmmyrcGmodOutlineVerbosity::Normal => {
                // Show anything that is more than a plain primitive.
                !matches!(
                    ty,
                    LuaType::String
                        | LuaType::StringConst(_)
                        | LuaType::Integer
                        | LuaType::IntegerConst(_)
                        | LuaType::Number
                        | LuaType::FloatConst(_)
                        | LuaType::Boolean
                        | LuaType::BooleanConst(_)
                        | LuaType::Nil
                        | LuaType::Unknown
                )
            }
            EmmyrcGmodOutlineVerbosity::Minimal => {
                // Only keep functions, classes, and named type references.
                matches!(
                    ty,
                    LuaType::Signature(_)
                        | LuaType::DocFunction(_)
                        | LuaType::Def(_)
                        | LuaType::Ref(_)
                )
            }
        }
    }

    /// Returns the global name prefix this file uses for scripted class methods (e.g. "ENT"),
    /// if the file is in a scripted entity scope.
    pub fn scripted_class_global_name(&self) -> Option<&str> {
        self.scripted_class_info
            .as_ref()
            .map(|info| info.global_name)
    }

    /// Returns the syntax id of the pre-created top-level scripted class symbol, if one exists.
    pub fn get_scripted_class_symbol_id(&self) -> Option<LuaSyntaxId> {
        self.scripted_class_symbol_id
    }

    /// Create the top-level scripted-class symbol using the chunk's block node as anchor.
    /// Must be called before processing any statements so that function routing works.
    /// Does nothing if the file is not in a scripted entity scope or it's already created.
    pub fn maybe_ensure_scripted_class_symbol(
        &mut self,
        block_node: LuaSyntaxNode,
        root_id: LuaSyntaxId,
        file_range: TextRange,
    ) {
        if self.scripted_class_symbol_id.is_some() {
            return;
        }
        let Some(info) = self.scripted_class_info.as_ref() else {
            return;
        };
        let label = info.class_label.clone();
        let symbol = LuaSymbol::new(label, None, SymbolKind::CLASS, file_range);
        let id = self.add_node_symbol(block_node, symbol, Some(root_id));
        self.scripted_class_symbol_id = Some(id);
    }

    pub fn bind_decl_symbol(&mut self, decl_id: LuaDeclId, symbol_id: LuaSyntaxId) {
        self.decl_symbol_ids.insert(decl_id, symbol_id);
    }

    pub fn get_decl_symbol_id(&self, decl_id: &LuaDeclId) -> Option<LuaSyntaxId> {
        self.decl_symbol_ids.get(decl_id).copied()
    }

    pub fn get_type(&self, id: LuaTypeOwner) -> LuaType {
        self.db
            .get_type_index()
            .get_type_cache(&id)
            .map(|cache| cache.as_type())
            .unwrap_or(&LuaType::Unknown)
            .clone()
    }

    pub fn add_node_symbol(
        &mut self,
        node: LuaSyntaxNode,
        symbol: LuaSymbol,
        parent: Option<LuaSyntaxId>,
    ) -> LuaSyntaxId {
        let syntax_id = LuaSyntaxId::new(node.kind(), node.text_range());
        self.document_symbols.insert(syntax_id, Box::new(symbol));

        if let Some(parent_id) = parent {
            self.link_parent_child(parent_id, syntax_id);
            return syntax_id;
        }

        let mut current = node;
        while let Some(parent_node) = current.parent() {
            let parent_syntax_id = LuaSyntaxId::new(parent_node.kind(), parent_node.text_range());
            if let Some(parent_symbol) = self.document_symbols.get_mut(&parent_syntax_id) {
                parent_symbol.add_child(syntax_id);
                break;
            }

            current = parent_node;
        }

        syntax_id
    }

    pub fn add_token_symbol(
        &mut self,
        token: LuaSyntaxToken,
        symbol: LuaSymbol,
        parent: Option<LuaSyntaxId>,
    ) -> LuaSyntaxId {
        let syntax_id = LuaSyntaxId::new(token.kind(), token.text_range());
        self.document_symbols.insert(syntax_id, Box::new(symbol));

        if let Some(parent_id) = parent {
            self.link_parent_child(parent_id, syntax_id);
            return syntax_id;
        }

        let mut node = token.parent();
        while let Some(parent_node) = node {
            let parent_syntax_id = LuaSyntaxId::new(parent_node.kind(), parent_node.text_range());
            if let Some(symbol) = self.document_symbols.get_mut(&parent_syntax_id) {
                symbol.add_child(syntax_id);
                break;
            }

            node = parent_node.parent();
        }

        syntax_id
    }

    pub fn contains_symbol(&self, id: &LuaSyntaxId) -> bool {
        self.document_symbols.contains_key(id)
    }

    pub fn with_symbol_mut<F>(&mut self, id: &LuaSyntaxId, func: F) -> Option<()>
    where
        F: FnOnce(&mut LuaSymbol),
    {
        let symbol = self.document_symbols.get_mut(id)?;
        func(symbol);
        Some(())
    }

    fn link_parent_child(&mut self, parent: LuaSyntaxId, child: LuaSyntaxId) {
        if let Some(parent_symbol) = self.document_symbols.get_mut(&parent) {
            parent_symbol.add_child(child);
        }
    }

    #[allow(deprecated)]
    pub fn build(self, root: &LuaChunk) -> DocumentSymbol {
        let id = root.get_syntax_id();
        let lua_symbol = self.document_symbols.get(&id).unwrap();
        let lsp_range = self.document.to_lsp_range(lua_symbol.range).unwrap();
        let lsp_selection_range = lua_symbol
            .selection_range
            .and_then(|range| self.document.to_lsp_range(range))
            .unwrap_or(lsp_range);

        let mut document_symbol = DocumentSymbol {
            name: lua_symbol.name.clone(),
            detail: lua_symbol.detail.clone(),
            kind: lua_symbol.kind,
            range: lsp_range,
            selection_range: lsp_selection_range,
            children: None,
            tags: None,
            deprecated: None,
        };

        self.build_child_symbol(&mut document_symbol, lua_symbol);

        document_symbol
    }

    #[allow(deprecated)]
    fn build_child_symbol(
        &self,
        document_symbol: &mut DocumentSymbol,
        symbol: &LuaSymbol,
    ) -> Option<()> {
        for child in &symbol.children {
            let child_symbol = self.document_symbols.get(child)?;
            let lsp_range = self.document.to_lsp_range(child_symbol.range)?;
            let lsp_selection_range = child_symbol
                .selection_range
                .and_then(|range| self.document.to_lsp_range(range))
                .unwrap_or(lsp_range);

            let child_symbol_name = if child_symbol.name.is_empty() {
                "(empty)".to_string()
            } else {
                child_symbol.name.clone()
            };

            let mut lsp_document_symbol = DocumentSymbol {
                name: child_symbol_name,
                detail: child_symbol.detail.clone(),
                kind: child_symbol.kind,
                range: lsp_range,
                selection_range: lsp_selection_range,
                children: None,
                tags: None,
                deprecated: None,
            };

            self.build_child_symbol(&mut lsp_document_symbol, child_symbol);
            document_symbol
                .children
                .get_or_insert_with(Vec::new)
                .push(lsp_document_symbol);
        }

        Some(())
    }

    pub fn get_symbol_kind_and_detail(&self, ty: Option<&LuaType>) -> (SymbolKind, Option<String>) {
        if ty.is_none() {
            return (SymbolKind::VARIABLE, None);
        }

        let ty = ty.unwrap();

        if let LuaType::Def(decl_id) = ty {
            if let Some(base_name) = self
                .db
                .get_gmod_class_metadata_index()
                .get_vgui_panel_base(decl_id.get_simple_name())
            {
                let detail = if let Some(base_name) = base_name {
                    format!(
                        "VGUI Panel: {} (Base: {})",
                        decl_id.get_simple_name(),
                        base_name
                    )
                } else {
                    format!("VGUI Panel: {}", decl_id.get_simple_name())
                };
                return (SymbolKind::CLASS, Some(detail));
            }
        }

        if ty.is_def() {
            return (SymbolKind::CLASS, None);
        } else if ty.is_string() {
            return (SymbolKind::STRING, None);
        } else if ty.is_table() {
            return (SymbolKind::OBJECT, None);
        } else if ty.is_number() {
            return match ty {
                LuaType::IntegerConst(i) => (SymbolKind::NUMBER, Some(i.to_string())),
                LuaType::FloatConst(f) => (SymbolKind::NUMBER, Some(f.to_string())),
                _ => (SymbolKind::NUMBER, None),
            };
        } else if ty.is_function() {
            return match ty {
                LuaType::DocFunction(f) => {
                    let params = f.get_params();
                    let mut param_names = Vec::new();
                    for param in params {
                        param_names.push(param.0.to_string());
                    }

                    let detail = format!("({})", param_names.join(", "));
                    (SymbolKind::FUNCTION, Some(detail))
                }
                LuaType::Signature(s) => {
                    let signature = self.db.get_signature_index().get(s);
                    if let Some(signature) = signature {
                        let params = signature.get_type_params();
                        let mut param_names = Vec::new();
                        for param in params {
                            param_names.push(param.0.to_string());
                        }

                        let detail = format!("({})", param_names.join(", "));
                        (SymbolKind::FUNCTION, Some(detail))
                    } else {
                        (SymbolKind::FUNCTION, None)
                    }
                }
                _ => (SymbolKind::FUNCTION, None),
            };
        } else if ty.is_boolean() {
            return (SymbolKind::BOOLEAN, None);
        }

        (SymbolKind::VARIABLE, None)
    }
}

/// Returns a human-readable type label for a scripted entity global name.
pub fn scripted_class_type_label(global_name: &str) -> &'static str {
    match global_name {
        "ENT" => "Entity",
        "SWEP" => "Weapon",
        "TOOL" => "Tool",
        "EFFECT" => "Effect",
        "PLUGIN" => "Plugin",
        _ => "Class",
    }
}

#[derive(Debug)]
pub struct LuaSymbol {
    pub name: String,
    pub detail: Option<String>,
    pub kind: SymbolKind,
    pub range: TextRange,
    pub selection_range: Option<TextRange>,
    pub children: Vec<LuaSyntaxId>,
}

impl LuaSymbol {
    pub fn new(name: String, detail: Option<String>, kind: SymbolKind, range: TextRange) -> Self {
        Self {
            name,
            detail,
            kind,
            range,
            selection_range: None,
            children: Vec::new(),
        }
    }

    pub fn with_selection_range(
        name: String,
        detail: Option<String>,
        kind: SymbolKind,
        range: TextRange,
        selection_range: TextRange,
    ) -> Self {
        Self {
            name,
            detail,
            kind,
            range,
            selection_range: Some(selection_range),
            children: Vec::new(),
        }
    }

    pub fn add_child(&mut self, child: LuaSyntaxId) {
        self.children.push(child);
    }

    pub fn set_kind(&mut self, kind: SymbolKind) {
        self.kind = kind;
    }

    pub fn set_detail(&mut self, detail: Option<String>) {
        self.detail = detail;
    }
}
