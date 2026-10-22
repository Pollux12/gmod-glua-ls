mod auto_require_provider;
mod desc_provider;
mod doc_name_token_provider;
mod doc_tag_provider;
mod doc_type_provider;
mod env_provider;
mod equality_provider;
mod file_path_provider;
mod function_provider;
mod gmod_system_provider;
mod keywords_provider;
mod member_provider;
mod module_path_provider;
mod postfix_provider;
mod table_field_provider;

use super::completion_builder::CompletionBuilder;
pub use function_provider::get_function_remove_nil;
use glua_parser::LuaAstToken;
use glua_parser::LuaStringToken;
pub use gmod_system_provider::apply_staged_call_snippet;
use rowan::{TextRange, TextSize};

pub fn add_completions(builder: &mut CompletionBuilder) -> Option<()> {
    postfix_provider::add_completion(builder);
    if builder.is_cancelled() {
        return None;
    }
    // `function_provider` must have higher priority than `env_provider`
    function_provider::add_completion(builder);
    if builder.is_cancelled() {
        return None;
    }
    equality_provider::add_completion(builder);
    if builder.is_cancelled() {
        return None;
    }
    // If `table_field_provider` succeeds it aborts completion, and it must have higher priority than `env_provider`
    table_field_provider::add_completion(builder);
    if builder.is_cancelled() {
        return None;
    }
    env_provider::add_completion(builder);
    if builder.is_cancelled() {
        return None;
    }
    keywords_provider::add_completion(builder);
    if builder.is_cancelled() {
        return None;
    }
    member_provider::add_completion(builder);
    if builder.is_cancelled() {
        return None;
    }
    gmod_system_provider::add_completion(builder);
    if builder.is_cancelled() {
        return None;
    }

    module_path_provider::add_completion(builder);
    if builder.is_cancelled() {
        return None;
    }
    file_path_provider::add_completion(builder);
    if builder.is_cancelled() {
        return None;
    }
    auto_require_provider::add_completion(builder);
    if builder.is_cancelled() {
        return None;
    }
    doc_tag_provider::add_completion(builder);
    if builder.is_cancelled() {
        return None;
    }
    doc_type_provider::add_completion(builder);
    if builder.is_cancelled() {
        return None;
    }
    doc_name_token_provider::add_completion(builder);
    if builder.is_cancelled() {
        return None;
    }
    desc_provider::add_completions(builder);

    for (index, item) in builder.get_completion_items_mut().iter_mut().enumerate() {
        if item.sort_text.is_none() {
            item.sort_text = Some(format!("{:04}", index + 32));
        }
    }

    Some(())
}

fn get_text_edit_range_in_string(
    builder: &mut CompletionBuilder,
    string_token: LuaStringToken,
) -> Option<lsp_types::Range> {
    let text = string_token.get_text();
    let range = string_token.get_range();
    if text.is_empty() {
        return None;
    }

    let (open_len, close_len) = string_delimiter_lens(text);
    let new_text_range = TextRange::new(
        range.start() + TextSize::from(open_len as u32),
        range.end() - TextSize::from(close_len as u32),
    );

    builder
        .semantic_model
        .get_document()
        .to_lsp_range(new_text_range)
}

fn string_delimiter_lens(text: &str) -> (usize, usize) {
    if let Some(quote) = text.chars().next().filter(|c| matches!(c, '"' | '\'')) {
        let closed = text.len() > 1 && text.ends_with(quote);
        return (1, usize::from(closed));
    }

    if let Some(rest) = text.strip_prefix('[') {
        let level = rest.bytes().take_while(|&b| b == b'=').count();
        if rest[level..].starts_with('[') {
            let open_len = level + 2;
            let close = format!("]{}]", "=".repeat(level));
            let closed = text.len() >= open_len + close.len() && text.ends_with(&close);
            return (open_len, if closed { close.len() } else { 0 });
        }
    }

    (0, 0)
}
