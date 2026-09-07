use crate::{DiagnosticCode, SemanticModel};
use glua_parser::{LuaAstNode, LuaAstToken, LuaDocTagOther, LuaTokenKind};
use rustc_hash::FxHashSet;
use serde_json::Value;

use super::{Checker, DiagnosticContext};

pub struct UnknownDocTag;

impl Checker for UnknownDocTag {
    const CODES: &[DiagnosticCode] = &[
        DiagnosticCode::UndefinedDocParam,
        DiagnosticCode::UnknownDocTag,
    ];

    fn check(context: &mut DiagnosticContext, semantic_model: &SemanticModel) {
        let known_tags: FxHashSet<_> = semantic_model
            .get_emmyrc()
            .doc
            .known_tags
            .iter()
            .map(|tag| tag.as_str())
            .collect();
        let mut known_tags = known_tags;
        known_tags.insert("accessorfunc");
        if semantic_model.get_emmyrc().gmod.enabled {
            known_tags.insert("hook");
            known_tags.insert("realm");
        }

        let Some(buckets) = context.file_buckets() else {
            return;
        };
        for tag_other in buckets.doc_tag_others.iter() {
            check_tag(context, tag_other, &known_tags);
        }
    }
}

fn check_tag(
    context: &mut DiagnosticContext,
    tag_other: &LuaDocTagOther,
    known_tags: &FxHashSet<&str>,
) -> Option<()> {
    if let Some(token) = tag_other.token_by_kind(LuaTokenKind::TkTagOther)
        && !known_tags.contains(token.get_text())
    {
        context.add_diagnostic(
            DiagnosticCode::UnknownDocTag,
            token.get_range(),
            format!("Unknown doc tag: `{name}`", name = token.get_text()).to_string(),
            Some(Value::String(token.get_text().to_string())),
        );
    }
    Some(())
}
