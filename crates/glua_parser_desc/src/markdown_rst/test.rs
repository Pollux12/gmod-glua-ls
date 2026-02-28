#[cfg(test)]
mod tests {
    use crate::markdown_rst::MarkdownRstParser;
    #[allow(unused)]
    use crate::testlib::{print_result, test};
    use googletest::prelude::*;

    #[gtest]
    fn test_rst() -> Result<()> {
        let code = r#"
--- Inline markup
--- =============
---
--- Not valid markup
--- ----------------
---
--- - 2 * x a ** b (* BOM32_* ` `` _ __ | (breaks rule 1)
--- - || (breaks rule 3)
--- - "*" '|' (*) [*] {*} <*> “*” »*« ›*‹ «*» »*» ›*› (breaks rule 5)
--- - 2*x a**b O(N**2) e**(x*y) f(x)*f(y) a|b file*.* __init__ __init__() (breaks rule 6)
---
---
--- Valid markup
--- ------------
---
--- Style: *em*, **strong**, ***strong, with stars***, *broken `em`.
--- Implicit ref: `something`, `a\` b`. Broken: `something
--- Explicit ref: :role:`ref`. Broken: :role:`ref
--- Lua ref: :lua:obj:`a.b.c`, :lua:obj:`~a.b.c`, :lua:obj:`title <a.b.c>`.
--- Code block: ``code``, ``code `backticks```,
--- ``escapes don't work here\``, ``{ 1, 2, nil, 4 }``.
--- Broken code block: ``foo *bar*
--- Implicit hyperlinks: target_, anonymous__, not a link___.
--- Explicit hyperlinks: `target`_, `anonymous`__.
--- Malformed ref: :lua:obj:`~a.b.c`__ (still parsed as ref).
--- Hyperlink: `foo bar`_, `foo bar`__, `foo <bar>`_.
--- Internal hyperlink: _`foo bar`. Broken _`foo bar
--- Footnote: [1]_, [2], [3
--- Replacement: |foo|, |bar|_, |baz|__.
--- *2 * x  *a **b *.rst*
--- *2*x a**b O(N**2) e**(x*y) f(x)*f(y) a*(1+2)*
---
--- Block markup
--- ============
---
--- Lists
--- -----
---
--- - List 1
---   Continuation
---
---   Continuation 2
---
--- -  List 2
---    Continuation
---   Not a continuation
---
--- - List
---
---   - Nested list
---
--- - List
--- Not list
---
--- - List
--- -
--- - List
---
---
--- Numbered lists
--- --------------
---
--- 1.  List.
---
--- 1)  List.
---
--- (1) List.
---
--- A) This is not
--- a list
---
--- A) This is a list.
---
--- 1) This is not a list...
--- 2. because list style changes without a blank line.
---
--- 1) This is a list...
--- 2) because list style doesn't change.
---
--- \A. Einstein was a really smart dude.
---
--- 1. Item 1 initial text.
---
---    a) Item 1a.
---    b) Item 1b.
---
--- 2. a) Item 2a.
---    b) Item 2b.
---
--- 1. List
--- 2.
--- 3. List
---
---
--- Field list
--- ----------
---
--- :Field: content
--- :Field:2: Content
--- :Field:3: Content
---           Continuation
--- :Field\: 4: Content
---
---
--- Line block
--- ----------
---
--- | Lend us a couple of bob till Thursday.
--- | I'm absolutely skint.
--- | But I'm expecting a postal order and I can pay you back
---   as soon as it comes.
--- | Love, Ewan.
---
---   Not a continuation.
---
---
--- Block quotes
--- ------------
---
--- This is an ordinary paragraph, introducing a block quote.
---
---     "It is my business to know things.  That is my trade."
---
---     -- Sherlock Holmes
---
--- * List item.
---
--- ..
---
---     Block quote 3.
---
---
--- Doctest blocks
--- --------------
---
--- >>> print('this is a Doctest block')
--- this is a Doctest block
--- >>> print('foo bar')
--- ... None
--- foo bar
---
--- Explicit markup
--- ---------------
---
--- .. Comment
---
---    With continuation
---
--- .. [1] Footnote
---
--- .. [#] Long footnote
---    with continuation.
---
---    - And nested content.
---
--- .. _target:
---
--- .. _hyperlink-name: link-block
---
--- .. _`FAQTS: Computers: Programming: Languages: Python`:
---    http://python.faqts.com/
---
--- .. _entirely-below:
---    https://docutils.
---    sourceforge.net/rst.html
---
--- .. _Chapter One\: "Tadpole Days":
---
--- It's not easy being green...
---
--- .. directive::
---
--- .. directive:: args
---    :param: value
---    param 2
---
---    Content.
---
---    - Nested content.
---
--- .. code-block:: python
---
---    def foo():
---        pass
---
--- .. code-block:: lua
---    :linenos:
---
---    function foo(x)
---        print([[
---            long string
---        ]])
---    end
---
---
--- Implicit hyperlink target
--- -------------------------
---
--- __ anonymous-hyperlink-target-link-block
---
---
--- Literal blocks
--- --------------
---
--- ::
---
---   This is code!
---
--- Some code::
---
---   Code...
---
---   ...continues.
---
--- ::
---
--- - This is also code!
--- -
--- - Continues.
---
--- - And this is list.
"#;

        let expected = r#"
--- <Scope>Inline markup
--- <Markup>=============</Markup></Scope>
---
--- <Scope>Not valid markup
--- <Markup>----------------</Markup></Scope>
---
--- <Scope><Markup>-</Markup> 2 * x a ** b (* BOM32_* ` `` _ __ | (breaks rule 1)</Scope>
--- <Scope><Markup>-</Markup> || (breaks rule 3)</Scope>
--- <Scope><Markup>-</Markup> "*" '|' (*) [*] {*} <*> “*” »*« ›*‹ «*» »*» ›*› (breaks rule 5)</Scope>
--- <Scope><Markup>-</Markup> 2*x a**b O(N**2) e**(x*y) f(x)*f(y) a|b file*.* __init__ __init__() (breaks rule 6)</Scope>
---
---
--- <Scope>Valid markup
--- <Markup>------------</Markup></Scope>
---
--- Style: <Em><Markup>*</Markup>em<Markup>*</Markup></Em>, <Strong><Markup>**</Markup>strong<Markup>**</Markup></Strong>, <Strong><Markup>**</Markup>*strong, with stars*<Markup>**</Markup></Strong>, *broken <Markup>`</Markup><Code>em</Code><Markup>`</Markup>.
--- Implicit ref: <Markup>`</Markup><Code>something</Code><Markup>`</Markup>, <Markup>`</Markup><Code>a\` b</Code><Markup>`</Markup>. Broken: `something
--- Explicit ref: <Markup>:</Markup><Arg>role</Arg><Markup>:`</Markup><Code>ref</Code><Markup>`</Markup>. Broken: :role:`ref
--- Lua ref: <Markup>:</Markup><Arg>lua:obj</Arg><Markup>:`</Markup><Ref>a.b.c</Ref><Markup>`</Markup>, <Markup>:</Markup><Arg>lua:obj</Arg><Markup>:`</Markup><Code>~</Code><Ref>a.b.c</Ref><Markup>`</Markup>, <Markup>:</Markup><Arg>lua:obj</Arg><Markup>:`</Markup><Code>title <</Code><Ref>a.b.c</Ref><Code>></Code><Markup>`</Markup>.
--- Code block: <Markup>``</Markup><Code>code</Code><Markup>``</Markup>, <Markup>``</Markup><Code>code `backticks`</Code><Markup>``</Markup>,
--- <Markup>``</Markup><Code>escapes don't work here\</Code><Markup>``</Markup>, <Markup>``</Markup><Code>{ 1, 2, nil, 4 }</Code><Markup>``</Markup>.
--- Broken code block: ``foo <Em><Markup>*</Markup>bar<Markup>*</Markup></Em>
--- Implicit hyperlinks: <Link>target</Link><Markup>_</Markup>, <Link>anonymous</Link><Markup>__</Markup>, not a link___.
--- Explicit hyperlinks: <Markup>`</Markup><Link>target</Link><Markup>`_</Markup>, <Markup>`</Markup><Link>anonymous</Link><Markup>`__</Markup>.
--- Malformed ref: <Markup>:</Markup><Arg>lua:obj</Arg><Markup>:`</Markup><Code>~</Code><Ref>a.b.c</Ref><Markup>`__</Markup> (still parsed as ref).
--- Hyperlink: <Markup>`</Markup><Link>foo bar</Link><Markup>`_</Markup>, <Markup>`</Markup><Link>foo bar</Link><Markup>`__</Markup>, <Markup>`</Markup><Link>foo <bar></Link><Markup>`_</Markup>.
--- Internal hyperlink: <Markup>_`</Markup><Link>foo bar</Link><Markup>`</Markup>. Broken _`foo bar
--- Footnote: <Markup>[</Markup><Link>1</Link><Markup>]_</Markup>, [2], [3
--- Replacement: <Markup>|</Markup><Code>foo</Code><Markup>|</Markup>, <Markup>|</Markup><Link>bar</Link><Markup>|_</Markup>, <Markup>|</Markup><Link>baz</Link><Markup>|__</Markup>.
--- <Em><Markup>*</Markup>2 * x  *a **b *.rst<Markup>*</Markup></Em>
--- <Em><Markup>*</Markup>2*x a**b O(N**2) e**(x*y) f(x)*f(y) a*(1+2)<Markup>*</Markup></Em>
---
--- <Scope>Block markup
--- <Markup>============</Markup></Scope>
---
--- <Scope>Lists
--- <Markup>-----</Markup></Scope>
---
--- <Scope><Markup>-</Markup> List 1
---   Continuation
---
---   Continuation 2</Scope>
---
--- <Scope><Markup>-</Markup>  List 2
---    Continuation</Scope>
--- <Scope>  Not a continuation</Scope>
---
--- <Scope><Markup>-</Markup> List
---
---   <Scope><Markup>-</Markup> Nested list</Scope></Scope>
---
--- <Scope><Markup>-</Markup> List</Scope>
--- Not list
---
--- <Scope><Markup>-</Markup> List</Scope>
--- <Scope><Markup>-</Markup></Scope>
--- <Scope><Markup>-</Markup> List</Scope>
---
---
--- <Scope>Numbered lists
--- <Markup>--------------</Markup></Scope>
---
--- <Scope><Markup>1.</Markup>  List.</Scope>
---
--- <Scope><Markup>1)</Markup>  List.</Scope>
---
--- <Scope><Markup>(1)</Markup> List.</Scope>
---
--- A) This is not
--- a list
---
--- <Scope><Markup>A)</Markup> This is a list.</Scope>
---
--- 1) This is not a list...
--- 2. because list style changes without a blank line.
---
--- <Scope><Markup>1)</Markup> This is a list...</Scope>
--- <Scope><Markup>2)</Markup> because list style doesn't change.</Scope>
---
--- <Markup>\A</Markup>. Einstein was a really smart dude.
---
--- <Scope><Markup>1.</Markup> Item 1 initial text.
---
---    <Scope><Markup>a)</Markup> Item 1a.</Scope>
---    <Scope><Markup>b)</Markup> Item 1b.</Scope></Scope>
---
--- <Scope><Markup>2.</Markup> <Scope><Markup>a)</Markup> Item 2a.</Scope>
---    <Scope><Markup>b)</Markup> Item 2b.</Scope></Scope>
---
--- <Scope><Markup>1.</Markup> List</Scope>
--- <Scope><Markup>2.</Markup></Scope>
--- <Scope><Markup>3.</Markup> List</Scope>
---
---
--- <Scope>Field list
--- <Markup>----------</Markup></Scope>
---
--- :Field: content
--- :Field:2: Content
--- :Field:3: Content
---           Continuation
--- :Field<Markup>\:</Markup> 4: Content
---
---
--- <Scope>Line block
--- <Markup>----------</Markup></Scope>
---
--- <Scope><Markup>|</Markup> Lend us a couple of bob till Thursday.</Scope>
--- <Scope><Markup>|</Markup> I'm absolutely skint.</Scope>
--- <Scope><Markup>|</Markup> But I'm expecting a postal order and I can pay you back
---   as soon as it comes.</Scope>
--- <Scope><Markup>|</Markup> Love, Ewan.</Scope>
---
--- <Scope>  Not a continuation.</Scope>
---
---
--- <Scope>Block quotes
--- <Markup>------------</Markup></Scope>
---
--- This is an ordinary paragraph, introducing a block quote.
---
--- <Scope>    "It is my business to know things.  That is my trade."
---
---     -- Sherlock Holmes</Scope>
---
--- <Scope><Markup>*</Markup> List item.</Scope>
---
--- ..
---
--- <Scope>    Block quote 3.</Scope>
---
---
--- <Scope>Doctest blocks
--- <Markup>--------------</Markup></Scope>
---
--- <Scope><Markup>>>></Markup> <CodeBlock>print('this is a Doctest block')</CodeBlock>
--- <CodeBlock>this is a Doctest block</CodeBlock>
--- <Markup>>>></Markup> <CodeBlock>print('foo bar')</CodeBlock>
--- <Markup>...</Markup> <CodeBlock>None</CodeBlock>
--- <CodeBlock>foo bar</CodeBlock>
---</Scope>
--- <Scope>Explicit markup
--- <Markup>---------------</Markup></Scope>
---
--- <Scope><Markup>..</Markup> Comment
---
---    With continuation</Scope>
---
--- <Scope><Markup>..</Markup> <Markup>[</Markup><Arg>1</Arg><Markup>]</Markup> Footnote</Scope>
---
--- <Scope><Markup>..</Markup> <Markup>[</Markup><Arg>#</Arg><Markup>]</Markup> Long footnote
---    <CodeBlock>with continuation.</CodeBlock>
---
---    <Scope><Markup>-</Markup> And nested content.</Scope></Scope>
---
--- <Scope><Markup>..</Markup> <Markup>_</Markup><Arg>target</Arg><Markup>:</Markup></Scope>
---
--- <Scope><Markup>..</Markup> <Markup>_</Markup><Arg>hyperlink-name</Arg><Markup>:</Markup> <Link>link-block</Link></Scope>
---
--- <Scope><Markup>..</Markup> <Markup>_</Markup><Arg>`FAQTS: Computers: Programming: Languages: Python`</Arg><Markup>:</Markup>
---    <Link>http://python.faqts.com/</Link></Scope>
---
--- <Scope><Markup>..</Markup> <Markup>_</Markup><Arg>entirely-below</Arg><Markup>:</Markup>
---    <Link>https://docutils.</Link>
---    <Link>sourceforge.net/rst.html</Link></Scope>
---
--- <Scope><Markup>..</Markup> <Markup>_</Markup><Arg>Chapter One\: "Tadpole Days"</Arg><Markup>:</Markup></Scope>
---
--- It's not easy being green...
---
--- <Scope><Markup>..</Markup> <Arg>directive</Arg><Markup>::</Markup></Scope>
---
--- <Scope><Markup>..</Markup> <Arg>directive</Arg><Markup>::</Markup> <CodeBlock>args</CodeBlock>
---    <Markup>:</Markup><Arg>param</Arg><Markup>:</Markup> <CodeBlock>value</CodeBlock>
---    <CodeBlock>param 2</CodeBlock>
---
---    Content.
---
---    <Scope><Markup>-</Markup> Nested content.</Scope></Scope>
---
--- <Scope><Markup>..</Markup> <Arg>code-block</Arg><Markup>::</Markup> <CodeBlock>python</CodeBlock>
---
---    <CodeBlock>def foo():</CodeBlock>
---    <CodeBlock>    pass</CodeBlock></Scope>
---
--- <Scope><Markup>..</Markup> <Arg>code-block</Arg><Markup>::</Markup> <CodeBlock>lua</CodeBlock>
---    <Markup>:</Markup><Arg>linenos</Arg><Markup>:</Markup>
---
---    <CodeBlockHl(Keyword)>function</CodeBlockHl(Keyword)> <CodeBlockHl(Function)>foo</CodeBlockHl(Function)><CodeBlockHl(Operators)>(</CodeBlockHl(Operators)><CodeBlockHl(Variable)>x</CodeBlockHl(Variable)><CodeBlockHl(Operators)>)</CodeBlockHl(Operators)>
---        <CodeBlockHl(Function)>print</CodeBlockHl(Function)><CodeBlockHl(Operators)>(</CodeBlockHl(Operators)><CodeBlockHl(String)>[[</CodeBlockHl(String)>
---    <CodeBlockHl(String)>        long string</CodeBlockHl(String)>
---    <CodeBlockHl(String)>    ]]</CodeBlockHl(String)><CodeBlockHl(Operators)>)</CodeBlockHl(Operators)>
---    <CodeBlockHl(Keyword)>end</CodeBlockHl(Keyword)></Scope>
---
---
--- <Scope>Implicit hyperlink target
--- <Markup>-------------------------</Markup></Scope>
---
--- <Scope><Link>__</Link> <Link>anonymous-hyperlink-target-link-block</Link></Scope>
---
---
--- <Scope>Literal blocks
--- <Markup>--------------</Markup></Scope>
---
--- ::
---
---   <Scope><CodeBlock>This is code!</CodeBlock></Scope>
---
--- Some code::
---
---   <Scope><CodeBlock>Code...</CodeBlock>
---
---   <CodeBlock>...continues.</CodeBlock></Scope>
---
--- ::
---
--- <Markup>-</Markup><Scope><CodeBlock> This is also code!</CodeBlock>
--- <Markup>-</Markup>
--- <Markup>-</Markup><CodeBlock> Continues.</CodeBlock></Scope>
---
--- <Scope><Markup>-</Markup> And this is list.</Scope>
"#;

        test(
            code,
            Box::new(MarkdownRstParser::new(None, None, None)),
            expected,
        )
        .or_fail()?;
        Ok(())
    }

    #[gtest]
    fn test_rst_no_indent() -> Result<()> {
        let code = r#"
---```lua
---
--- local t = 213
---```
---
--- .. code-block:: lua
---
---    local t = 123
---    yes = 1123
local t = 123
"#;

        let expected = r#"
---```lua
---
---<Scope> local t = 213</Scope>
---```
---
---<Scope> <Scope><Markup>..</Markup> <Arg>code-block</Arg><Markup>::</Markup> <CodeBlock>lua</CodeBlock>
---
---    <CodeBlockHl(Keyword)>local</CodeBlockHl(Keyword)> <CodeBlockHl(Variable)>t</CodeBlockHl(Variable)> <CodeBlockHl(Operators)>=</CodeBlockHl(Operators)> <CodeBlockHl(Number)>123</CodeBlockHl(Number)>
---    <CodeBlockHl(Variable)>yes</CodeBlockHl(Variable)> <CodeBlockHl(Operators)>=</CodeBlockHl(Operators)> <CodeBlockHl(Number)>1123</CodeBlockHl(Number)></Scope></Scope>
local t = 123
"#;

        test(
            code,
            Box::new(MarkdownRstParser::new(
                None,
                Some("lua:obj".to_string()),
                None,
            )),
            expected,
        )
        .or_fail()?;
        Ok(())
    }

    #[gtest]
    fn test_rst_default_role() -> Result<()> {
        let code = r#"--- See `ref`"#;

        let expected = r#"--- See <Markup>`</Markup><Ref>ref</Ref><Markup>`</Markup>"#;

        test(
            code,
            Box::new(MarkdownRstParser::new(
                None,
                Some("lua:obj".to_string()),
                None,
            )),
            expected,
        )
        .or_fail()?;
        Ok(())
    }

    #[gtest]
    fn test_rst_primary_domain() -> Result<()> {
        let code = r#"--- See :obj:`ref`"#;

        let expected = r#"
            --- See <Markup>:</Markup><Arg>obj</Arg><Markup>:`</Markup><Ref>ref</Ref><Markup>`</Markup>
        "#;

        test(
            code,
            Box::new(MarkdownRstParser::new(Some("lua".to_string()), None, None)),
            expected,
        )
        .or_fail()?;
        Ok(())
    }

    #[gtest]
    fn test_rst_search_at_offset() -> Result<()> {
        let code = r#"--- See :lua:obj:`x` :lua:obj:`ref`"#;
        let expected = r#"--- See :lua:obj:`x` :lua:obj:`<Ref>ref</Ref>`"#;
        test(
            code,
            Box::new(MarkdownRstParser::new(None, None, Some(31))),
            expected,
        )
        .or_fail()?;
        test(
            code,
            Box::new(MarkdownRstParser::new(None, None, Some(32))),
            expected,
        )
        .or_fail()?;
        test(
            code,
            Box::new(MarkdownRstParser::new(None, None, Some(34))),
            expected,
        )
        .or_fail()?;

        let code = r#"--- See :lua:obj:`x` :lua:obj:`"#;
        let expected = r#"--- See :lua:obj:`x` :lua:obj:`<Ref></Ref>"#;
        test(
            code,
            Box::new(MarkdownRstParser::new(None, None, Some(31))),
            expected,
        )
        .or_fail()?;

        let code = r#"--- See :lua:obj:`x` :lua:obj:``..."#;
        let expected = r#"--- See :lua:obj:`x` :lua:obj:`<Ref>`</Ref>..."#;
        test(
            code,
            Box::new(MarkdownRstParser::new(None, None, Some(31))),
            expected,
        )
        .or_fail()?;
        Ok(())
    }

    #[gtest]
    fn test_rst_search_at_offset_default_role() -> Result<()> {
        let code = r#"--- See `ab`"#;
        let expected = r#"--- See `<Ref>ab</Ref>`"#;
        test(
            code,
            Box::new(MarkdownRstParser::new(
                None,
                Some("lua:obj".to_string()),
                Some(9),
            )),
            expected,
        )
        .or_fail()?;
        test(
            code,
            Box::new(MarkdownRstParser::new(
                None,
                Some("lua:obj".to_string()),
                Some(10),
            )),
            expected,
        )
        .or_fail()?;
        test(
            code,
            Box::new(MarkdownRstParser::new(
                None,
                Some("lua:obj".to_string()),
                Some(11),
            )),
            expected,
        )
        .or_fail()?;

        let code = r#"--- See `"#;
        let expected = r#"--- See `<Ref></Ref>"#;
        test(
            code,
            Box::new(MarkdownRstParser::new(
                None,
                Some("lua:obj".to_string()),
                Some(9),
            )),
            expected,
        )
        .or_fail()?;

        let code = r#"--- See `..."#;
        let expected = r#"--- See `<Ref>...</Ref>"#;
        test(
            code,
            Box::new(MarkdownRstParser::new(
                None,
                Some("lua:obj".to_string()),
                Some(9),
            )),
            expected,
        )
        .or_fail()?;

        let code = r#"--- See ``"#;
        let expected = r#"--- See `<Ref>`</Ref>"#;
        test(
            code,
            Box::new(MarkdownRstParser::new(
                None,
                Some("lua:obj".to_string()),
                Some(9),
            )),
            expected,
        )
        .or_fail()?;
        Ok(())
    }
}
