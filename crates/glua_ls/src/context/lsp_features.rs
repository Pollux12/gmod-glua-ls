use lsp_types::ClientCapabilities;

#[derive(Debug)]
pub struct LspFeatures {
    client_capabilities: ClientCapabilities,
}

#[allow(unused)]
impl LspFeatures {
    pub fn new(client_capabilities: ClientCapabilities) -> Self {
        Self {
            client_capabilities,
        }
    }

    pub fn supports_multiline_tokens(&self) -> bool {
        if let Some(semantic) = &self.client_capabilities.text_document {
            if let Some(semantic) = &semantic.semantic_tokens {
                if let Some(supports) = semantic.multiline_token_support {
                    return supports;
                }
            }
        }
        false
    }

    pub fn supports_config_request(&self) -> bool {
        if let Some(workspace) = &self.client_capabilities.workspace {
            if let Some(supports) = workspace.configuration {
                return supports;
            }
        }
        false
    }

    pub fn supports_pull_diagnostic(&self) -> bool {
        if let Some(text_document) = &self.client_capabilities.text_document {
            return text_document.diagnostic.is_some();
        }
        false
    }

    pub fn supports_completion_item_deprecated_tags(&self) -> bool {
        self.client_capabilities
            .text_document
            .as_ref()
            .and_then(|text_document| text_document.completion.as_ref())
            .and_then(|completion| completion.completion_item.as_ref())
            .and_then(|completion_item| completion_item.tag_support.as_ref())
            .is_some_and(|tag_support| {
                tag_support.value_set.is_empty()
                    || tag_support
                        .value_set
                        .contains(&lsp_types::CompletionItemTag::DEPRECATED)
            })
    }

    pub fn supports_workspace_diagnostic(&self) -> bool {
        self.supports_pull_diagnostic()
    }

    pub fn supports_refresh_diagnostic(&self) -> bool {
        if let Some(workspace) = &self.client_capabilities.workspace {
            if let Some(diagnostic) = &workspace.diagnostics {
                if let Some(supports) = diagnostic.refresh_support {
                    return supports;
                }
            }
        }
        false
    }

    pub fn supports_semantic_tokens_refresh(&self) -> bool {
        if let Some(workspace) = &self.client_capabilities.workspace {
            if let Some(semantic) = &workspace.semantic_tokens {
                if let Some(supports) = semantic.refresh_support {
                    return supports;
                }
            }
        }
        false
    }

    pub fn supports_inlay_hint_refresh(&self) -> bool {
        if let Some(workspace) = &self.client_capabilities.workspace {
            if let Some(inlay_hint) = &workspace.inlay_hint {
                if let Some(supports) = inlay_hint.refresh_support {
                    return supports;
                }
            }
        }
        false
    }

    pub fn supports_code_lens_refresh(&self) -> bool {
        if let Some(workspace) = &self.client_capabilities.workspace {
            if let Some(code_lens) = &workspace.code_lens {
                if let Some(supports) = code_lens.refresh_support {
                    return supports;
                }
            }
        }
        false
    }
}
