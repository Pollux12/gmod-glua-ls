mod lang;
mod markdown;
mod markdown_rst;
mod ref_target;
mod util;

use glua_parser::LuaDocDescription;
use rowan::TextRange;

pub use lang::{CodeBlockLang, process_code};
pub use ref_target::*;
pub use util::ResultContainer;
use util::sort_result;

#[cfg(test)]
mod testlib;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum DescItemKind {
    /// Generic block of documentation.
    Scope,

    /// Cross-reference to a Lua object.
    Ref,

    /// Emphasis.
    Em,

    /// Strong emphasis.
    Strong,

    /// Code markup.
    Code,

    /// Hyperlink.
    Link,

    /// Javadoc @link
    JavadocLink,

    /// Inline markup, like stars around emphasized text.
    Markup,

    /// Directive name, code-block syntax name, role name,
    /// or some other form of argument.
    Arg,

    /// Line of code in a code block.
    CodeBlock,

    /// Line of code in a code block highlighted by Lua lexer.
    CodeBlockHl(CodeBlockHighlightKind),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CodeBlockHighlightKind {
    None,
    String,
    Number,
    Keyword,
    Operators,
    Comment,
    Function,
    Class,
    Enum,
    Variable,
    Property,
    Decorator,
}

#[derive(Debug, Clone)]
pub struct DescItem {
    pub range: TextRange,
    pub kind: DescItemKind,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Default)]
pub enum DescParserType {
    #[default]
    None,
    Md,
    MySt {
        primary_domain: Option<String>,
    },
    Rst {
        primary_domain: Option<String>,
        default_role: Option<String>,
    },
}

/// Parses markup in comments.
pub trait LuaDescParser {
    /// Process a description node and yield found documentation ranges.
    fn parse(&mut self, text: &str, desc: LuaDocDescription) -> Vec<DescItem>;
}

pub fn parse(
    kind: DescParserType,
    text: &str,
    desc: LuaDocDescription,
    cursor_position: Option<usize>,
) -> Vec<DescItem> {
    let mut items = match kind {
        DescParserType::None => Vec::new(),
        DescParserType::Md => markdown::MarkdownParser::new(cursor_position).parse(text, desc),
        DescParserType::MySt { primary_domain } => {
            markdown::MarkdownParser::new_myst(primary_domain, cursor_position).parse(text, desc)
        }
        DescParserType::Rst {
            primary_domain,
            default_role,
        } => markdown_rst::MarkdownRstParser::new(primary_domain, default_role, cursor_position)
            .parse(text, desc),
    };

    sort_result(&mut items);

    items
}
