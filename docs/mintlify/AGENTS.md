# Docs Agent Notes

## Mintlify Site
- This directory is the Mintlify docs site; `docs.json` owns navigation and site config, and pages are MDX with frontmatter.
- Run docs commands from `docs/mintlify`: `mint dev` for local preview and `mint broken-links` for link checks.
- Config option docs live under `configuration/**`; keep them in sync with `crates/glua_code_analysis/resources/schema.json` and generated schema changes.

## Writing Style
- Write for end users: practical examples first, brief explanations second, simple language that is easy to understand.
- Use active voice and second person; keep warnings direct about what breaks and what to do instead.
- Use code formatting for commands, file names, paths, config keys, annotation names, and code references.
- Prefer sentence-case headings and short sections.
