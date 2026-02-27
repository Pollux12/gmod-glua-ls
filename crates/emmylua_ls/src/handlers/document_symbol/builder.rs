use std::collections::HashMap;

use emmylua_code_analysis::{
    DbIndex, FileId, GmodClassCallLiteral, GmodScriptedClassCallMetadata, LuaDecl, LuaDeclId,
    LuaDeclarationTree, LuaDocument, LuaType, LuaTypeOwner,
};
use emmylua_parser::{LuaAstNode, LuaChunk, LuaSyntaxId, LuaSyntaxNode, LuaSyntaxToken};
use lsp_types::{DocumentSymbol, SymbolKind};
use rowan::{TextRange, TextSize};

pub struct DocumentSymbolBuilder<'a> {
    db: &'a DbIndex,
    decl_tree: &'a LuaDeclarationTree,
    document: &'a LuaDocument<'a>,
    document_symbols: HashMap<LuaSyntaxId, Box<LuaSymbol>>,
    decl_symbol_ids: HashMap<LuaDeclId, LuaSyntaxId>,
    vgui_panel_names: HashMap<LuaDeclId, String>,
}

impl<'a> DocumentSymbolBuilder<'a> {
    pub fn new(
        db: &'a DbIndex,
        decl_tree: &'a LuaDeclarationTree,
        document: &'a LuaDocument,
    ) -> Self {
        let vgui_panel_names =
            Self::collect_vgui_panel_names(db, decl_tree, document.get_file_id());

        Self {
            db,
            decl_tree,
            document,
            document_symbols: HashMap::new(),
            decl_symbol_ids: HashMap::new(),
            vgui_panel_names,
        }
    }

    fn collect_vgui_panel_names(
        db: &DbIndex,
        decl_tree: &LuaDeclarationTree,
        file_id: FileId,
    ) -> HashMap<LuaDeclId, String> {
        let mut panel_names = HashMap::new();
        let Some(file_metadata) = db
            .get_gmod_class_metadata_index()
            .get_file_metadata(&file_id)
        else {
            return panel_names;
        };

        for call in &file_metadata.vgui_register_calls {
            Self::collect_vgui_panel_call(decl_tree, call, 1, &mut panel_names);
        }

        for call in &file_metadata.derma_define_control_calls {
            Self::collect_vgui_panel_call(decl_tree, call, 2, &mut panel_names);
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

        panel_names.insert(decl.get_id(), panel_name.clone());
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
                        return (SymbolKind::FUNCTION, Some(detail));
                    } else {
                        return (SymbolKind::FUNCTION, None);
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

#[derive(Debug)]
pub struct LuaSymbol {
    name: String,
    detail: Option<String>,
    kind: SymbolKind,
    range: TextRange,
    selection_range: Option<TextRange>,
    children: Vec<LuaSyntaxId>,
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
