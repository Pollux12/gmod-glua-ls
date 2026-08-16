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

    /// Whether the server may create its own progress tokens via
    /// `window/workDoneProgress/create`. Without it, a server-initiated
    /// progress token is never registered, so the `$/progress` notifications
    /// that follow have nothing to attach to.
    pub fn supports_work_done_progress(&self) -> bool {
        self.client_capabilities
            .window
            .as_ref()
            .and_then(|window| window.work_done_progress)
            .unwrap_or(false)
    }

    /// Whether the server may send `workspace/applyEdit`. LSP 3.17 gates it on
    /// `workspace.applyEdit`.
    pub fn supports_apply_edit(&self) -> bool {
        self.client_capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.apply_edit)
            .unwrap_or(false)
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

    /// Whether the client re-sends `method` after a `ContentModified` error
    /// instead of treating it as "no result".
    ///
    /// LSP 3.17 `general.staleRequestSupport.retryOnContentModified` is a
    /// per-method list, so this answer differs between features on the same
    /// client: VS Code lists only the semantic token methods, and clears the
    /// UI for every method it does not list.
    pub fn retries_on_content_modified(&self, method: &str) -> bool {
        self.client_capabilities
            .general
            .as_ref()
            .and_then(|general| general.stale_request_support.as_ref())
            .is_some_and(|stale| {
                stale
                    .retry_on_content_modified
                    .iter()
                    .any(|retried| retried == method)
            })
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

#[cfg(test)]
mod tests {
    use super::LspFeatures;
    use googletest::prelude::*;
    use lsp_types::ClientCapabilities;

    fn features_from(capabilities: serde_json::Value) -> LspFeatures {
        LspFeatures::new(
            serde_json::from_value::<ClientCapabilities>(capabilities)
                .expect("capabilities should deserialize"),
        )
    }

    /// VS Code lists only the semantic token methods. Answering
    /// `ContentModified` for anything else clears that feature's UI, which is
    /// what made the March 2026 attempt regress inlay hints.
    #[gtest]
    fn retries_on_content_modified_follows_the_client_method_list() -> Result<()> {
        let features = features_from(serde_json::json!({
            "general": {
                "staleRequestSupport": {
                    "cancel": true,
                    "retryOnContentModified": ["textDocument/semanticTokens/full"],
                }
            }
        }));

        verify_that!(
            features.retries_on_content_modified("textDocument/semanticTokens/full"),
            eq(true)
        )?;
        verify_that!(
            features.retries_on_content_modified("textDocument/inlayHint"),
            eq(false)
        )?;
        Ok(())
    }

    #[gtest]
    fn retries_on_content_modified_is_false_without_the_capability() -> Result<()> {
        let features = features_from(serde_json::json!({}));

        verify_that!(
            features.retries_on_content_modified("textDocument/semanticTokens/full"),
            eq(false)
        )?;
        Ok(())
    }
}
