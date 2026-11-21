//! `go.mod`: `require` lines and blocks, with `replace` directives applied and the `go` directive kept.

use zenodotus_core::Ecosystem;

use crate::Entry;

/// What a `go.mod` declares.
#[derive(Debug, Default)]
pub(crate) struct GoMod {
    /// The required modules after `replace`, in file order; modules replaced by a local path are left out.
    pub(crate) entries: Vec<Entry>,
    /// The version of the `go` directive.
    pub(crate) go: Option<String>,
}

/// Each directive as its verb and its arguments, unquoted; block lines carry the block's verb.
fn directives(text: &str) -> Vec<(&str, Vec<&str>)> {
    let mut directives = Vec::new();
    let mut block: Option<&str> = None;
    for line in text.lines() {
        let line = line.split_once("//").map_or(line, |(code, _)| code);
        let mut tokens = line.split_whitespace().map(|token| token.trim_matches('"'));
        match (block, tokens.next()) {
            (_, None) => {}
            (Some(_), Some(")")) => block = None,
            (Some(verb), Some(first)) => directives.push((verb, std::iter::once(first).chain(tokens).collect())),
            (None, Some(verb)) => {
                let arguments: Vec<&str> = tokens.collect();
                if arguments == ["("] {
                    block = Some(verb);
                } else {
                    directives.push((verb, arguments));
                }
            }
        }
    }
    directives
}

/// The requirements, replacements applied, and the `go` version.
pub(crate) fn parse(text: &str) -> GoMod {
    let directives = directives(text);
    let replacements: Vec<(&[&str], &[&str])> = directives
        .iter()
        .filter(|(verb, _)| *verb == "replace")
        .filter_map(|(_, arguments)| {
            let arrow = arguments.iter().position(|token| *token == "=>")?;
            Some((&arguments[..arrow], &arguments[arrow + 1..]))
        })
        .collect();
    let mut module = GoMod::default();
    for (verb, arguments) in &directives {
        match (*verb, arguments.as_slice()) {
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
                module.entries.push(Entry::registry(Ecosystem::Go, path, Some(version)));
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
        let go = |name, version| Entry::registry(Ecosystem::Go, name, Some(version));
        assert_eq!(
            module.entries,
            [
                go("github.com/gin-gonic/gin", "v1.10.0"),
                go("github.com/stretchr/testify", "v1.9.0"),
                go("golang.org/x/net", "v0.26.0"),
                go("git.corp.example.com/team/lib", "v0.3.1"),
                go("github.com/new/module", "v1.1.0"),
                go("github.com/pinned/module", "v1.2.0"),
            ]
        );
        assert_eq!(module.go.as_deref(), Some("1.22.3"));
    }

    #[test]
    fn a_file_without_a_go_directive_has_none() {
        let module = parse("module a\nrequire b\nrequire (\n)\n");
        assert!(module.entries.is_empty());
        assert_eq!(module.go, None);
    }
}
