//! `go.mod`: `require` lines and blocks, with `replace` directives applied and the `go` directive kept.

use zenodotus_core::Ecosystem;

use crate::Entry;

/// What a `go.mod` declares.
#[derive(Debug, Default)]
pub(crate) struct GoMod {
    /// The required modules after `replace`, in file order, direct unless marked `// indirect`; modules replaced by
    /// a local path are left out.
    pub(crate) entries: Vec<Entry>,
    /// The version of the `go` directive.
    pub(crate) go: Option<String>,
}

/// One directive of a `go.mod` or `go.work`.
pub(crate) struct Directive<'a> {
    /// The verb; a block line carries its block's verb.
    pub(crate) verb: &'a str,
    /// The arguments, unquoted.
    pub(crate) arguments: Vec<&'a str>,
    /// Whether its comment is `// indirect`, alone or before a `;`.
    pub(crate) indirect: bool,
}

/// Each directive of `text`, in file order.
pub(crate) fn directives(text: &str) -> Vec<Directive<'_>> {
    let mut directives = Vec::new();
    let mut block: Option<&str> = None;
    for line in text.lines() {
        let (line, comment) = line.split_once("//").unwrap_or((line, ""));
        let comment = comment.trim();
        let indirect = comment == "indirect" || comment.starts_with("indirect;");
        let mut tokens = line.split_whitespace().map(|token| token.trim_matches('"'));
        let (verb, arguments) = match (block, tokens.next()) {
            (_, None) => continue,
            (Some(_), Some(")")) => {
                block = None;
                continue;
            }
            (Some(verb), Some(first)) => (verb, std::iter::once(first).chain(tokens).collect()),
            (None, Some(verb)) => {
                let arguments: Vec<&str> = tokens.collect();
                if arguments == ["("] {
                    block = Some(verb);
                    continue;
                }
                (verb, arguments)
            }
        };
        directives.push(Directive {
            verb,
            arguments,
            indirect,
        });
    }
    directives
}

/// The requirements, replacements applied, and the `go` version.
pub(crate) fn parse(text: &str) -> GoMod {
    let directives = directives(text);
    let replacements: Vec<(&[&str], &[&str])> = directives
        .iter()
        .filter(|directive| directive.verb == "replace")
        .filter_map(|Directive { arguments, .. }| {
            let arrow = arguments.iter().position(|token| *token == "=>")?;
            Some((&arguments[..arrow], &arguments[arrow + 1..]))
        })
        .collect();
    let mut module = GoMod::default();
    for directive in &directives {
        match (directive.verb, directive.arguments.as_slice()) {
            ("go", [version]) => module.go = Some((*version).to_owned()),
            ("require", [path, version]) => {
                let replacement = replacements.iter().find(|(old, _)| match old {
                    [old_path] => old_path == path,
                    [old_path, old_version] => old_path == path && old_version == version,
                    _ => false,
                });
                let (path, version) = match replacement.map(|(_, new)| *new) {
                    None => (*path, *version),
                    Some([new_path, new_version]) => (*new_path, *new_version),
                    Some(_) => continue,
                };
                let entry = Entry::registry(Ecosystem::Go, path, Some(version));
                module.entries.push(entry.with_direct(!directive.indirect));
            }
            _ => {}
        }
    }
    module
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Entry;
    use zenodotus_core::Ecosystem;

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/go/go.mod");

    #[test]
    fn requirements_map_to_entries_after_replacements() {
        let module = parse(SAMPLE);
        let go = |name, version| Entry::registry(Ecosystem::Go, name, Some(version)).with_direct(true);
        assert_eq!(
            module.entries,
            [
                go("github.com/gin-gonic/gin", "v1.10.0"),
                go("github.com/stretchr/testify", "v1.9.0").with_direct(false),
                go("golang.org/x/net", "v0.26.0"),
                go("git.corp.example.com/team/lib", "v0.3.1"),
                go("github.com/new/module", "v1.1.0"),
                go("github.com/pinned/module", "v1.2.0"),
            ]
        );
        assert_eq!(module.go.as_deref(), Some("1.22.3"));
    }

    #[test]
    fn a_replace_with_a_malformed_left_side_replaces_nothing() {
        let module = parse("require a v1.0.0\nreplace a v1.0.0 extra => b v2.0.0\nreplace => c v3.0.0\n");
        assert_eq!(
            module.entries,
            [Entry::registry(Ecosystem::Go, "a", Some("v1.0.0")).with_direct(true)]
        );
    }

    #[test]
    fn a_file_without_a_go_directive_has_none() {
        let module = parse("module a\nrequire b\nrequire (\n)\n");
        assert_eq!(module.entries.len(), 0);
        assert_eq!(module.go, None);
    }

    #[test]
    fn require_lines_without_an_indirect_comment_are_direct() {
        let root = crate::testing::root_with(&[("go.mod", include_str!("../../../../fixtures/lockfiles/go/go.mod"))]);
        let direct = |id: &str| (id.to_owned(), true);
        assert_eq!(
            crate::testing::direct_flags(root.path()),
            [
                direct("go:github.com/gin-gonic/gin@v1.10.0"),
                ("go:github.com/stretchr/testify@v1.9.0".to_owned(), false),
                direct("go:golang.org/x/net@v0.26.0"),
                direct("go:git.corp.example.com/team/lib@v0.3.1"),
                direct("go:github.com/new/module@v1.1.0"),
                direct("go:github.com/pinned/module@v1.2.0"),
            ]
        );
    }

    #[test]
    fn an_indirect_comment_may_carry_more_text_after_a_semicolon() {
        let module = parse(
            "module x\nrequire a.dev/one v1.0.0 // indirect; kept for a fix\nrequire a.dev/two v1.0.0 // indirectly needed\n",
        );
        let flags: Vec<(&str, bool)> = module
            .entries
            .iter()
            .map(|entry| (entry.name.as_str(), entry.direct))
            .collect();
        assert_eq!(flags, [("a.dev/one", false), ("a.dev/two", true)]);
    }
}
