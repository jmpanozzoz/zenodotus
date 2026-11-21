//! `requirements*.txt`: only exact pins `name==version` count, plus `-r` includes, which the caller reads when they
//! stay inside the root.

use zenodotus_core::Ecosystem;

use crate::private::{PYPI_HOSTS, registry_source};
use crate::{Entry, Source};

/// A `-r` include: the path as written, relative to the including file's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Include {
    /// The path as written.
    pub(crate) path: String,
    /// The line of the `-r` option.
    pub(crate) line: u64,
}

/// The pins and includes of one requirements file.
#[derive(Debug, Default)]
pub(crate) struct Requirements {
    /// The exact pins, in file order.
    pub(crate) entries: Vec<Entry>,
    /// The `-r` includes, in file order.
    pub(crate) includes: Vec<Include>,
}

/// The logical lines of `text` with their first physical line: `\` continuations are joined and comments, which
/// start a line or follow whitespace, are removed.
fn logical_lines(text: &str) -> Vec<(u64, String)> {
    let mut lines = Vec::new();
    let mut current: Option<(u64, String)> = None;
    for (index, raw) in (1..).zip(text.lines()) {
        let (start, mut joined) = current.take().unwrap_or((index, String::new()));
        let (body, continued) = raw.strip_suffix('\\').map_or((raw, false), |body| (body, true));
        joined.push_str(body);
        if continued {
            current = Some((start, joined));
        } else {
            lines.push((start, strip_comment(&joined).trim().to_owned()));
        }
    }
    lines.extend(current.map(|(start, joined)| (start, strip_comment(&joined).trim().to_owned())));
    lines
}

/// `line` without a comment: `#` at its start or after whitespace.
fn strip_comment(line: &str) -> &str {
    let at = line
        .char_indices()
        .find(|&(index, character)| character == '#' && (index == 0 || line[..index].ends_with(char::is_whitespace)));
    at.map_or(line, |(index, _)| &line[..index])
}

/// The option name and value of `-r file`, `--requirement=file` and the like.
fn option(line: &str) -> (&str, &str) {
    let (name, value) = line
        .split_once('=')
        .filter(|(name, _)| name.starts_with("--") && !name.contains(char::is_whitespace))
        .unwrap_or_else(|| line.split_once(char::is_whitespace).unwrap_or((line, "")));
    (name, value.trim())
}

/// The name and exact version of a requirement written `name[extras] == version ; markers --hash=…`.
fn exact_pin(line: &str) -> Option<(&str, String)> {
    let requirement = line.split(" --").next().unwrap_or_default();
    let requirement = requirement.split(';').next().unwrap_or_default();
    let name_end = requirement
        .find(|character: char| !(character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')))
        .unwrap_or(requirement.len());
    let (name, rest) = requirement.split_at(name_end);
    let rest = rest.trim_start();
    let rest = match rest.strip_prefix('[') {
        Some(extras) => extras.split_once(']')?.1,
        None => rest,
    };
    let specifier: String = rest.chars().filter(|character| !character.is_whitespace()).collect();
    let version = specifier.strip_prefix("==")?;
    let exact = !name.is_empty() && !version.is_empty() && !version.contains(['=', ',', '*', '<', '>', '!', '~']);
    exact.then(|| (name, version.to_owned()))
}

/// The exact pins and `-r` includes of a requirements file. An index option naming a host other than the Python Package Index makes every
/// pin of the file private, since pip may install any of them from it.
pub(crate) fn parse(text: &str) -> Requirements {
    let mut requirements = Requirements::default();
    let mut private_index = None;
    for (line, logical) in logical_lines(text) {
        if logical.starts_with('-') {
            match option(&logical) {
                ("-r" | "--requirement", path) if !path.is_empty() => requirements.includes.push(Include {
                    path: path.to_owned(),
                    line,
                }),
                ("-i" | "--index-url" | "--extra-index-url", url) => {
                    if let Source::Private { url } = registry_source(url, PYPI_HOSTS) {
                        private_index.get_or_insert(url);
                    }
                }
                _ => {}
            }
        } else if let Some((name, version)) = exact_pin(&logical) {
            requirements
                .entries
                .push(Entry::registry(Ecosystem::Pypi, name, Some(&version)));
        }
    }
    if let Some(url) = private_index {
        for entry in &mut requirements.entries {
            entry.source = Source::Private { url: url.clone() };
        }
    }
    requirements
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Entry, Source};
    use zenodotus_core::Ecosystem;

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/requirements/requirements.txt");

    fn pin(name: &str, version: &str) -> Entry {
        Entry::registry(Ecosystem::Pypi, name, Some(version))
    }

    #[test]
    fn exact_pins_count_and_includes_are_listed_with_their_line() {
        let requirements = parse(SAMPLE);
        assert_eq!(
            requirements.entries,
            [
                pin("requests", "2.32.3"),
                pin("Django", "5.0.6"),
                pin("typing_extensions", "4.12.2#not-a-comment"),
            ]
        );
        assert_eq!(
            requirements.includes,
            [
                Include {
                    path: "requirements-base.txt".to_owned(),
                    line: 2
                },
                Include {
                    path: "extra/more.txt".to_owned(),
                    line: 3
                },
            ]
        );
    }

    #[test]
    fn a_private_index_makes_every_pin_of_the_file_private() {
        for option in [
            "-i https://pypi.acme.dev/simple",
            "--index-url=https://pypi.acme.dev/simple",
            "--extra-index-url https://pypi.acme.dev/simple",
        ] {
            let requirements = parse(&format!("a==1\n{option}\n"));
            let private = Entry {
                source: Source::Private {
                    url: "https://pypi.acme.dev/simple".to_owned(),
                },
                ..pin("a", "1")
            };
            assert_eq!(requirements.entries, [private], "{option}");
        }
        let public = parse("--index-url https://pypi.org/simple\n-r\na==1\n");
        assert_eq!(public.entries, [pin("a", "1")]);
        assert_eq!(public.includes.len(), 0);
    }

    #[test]
    fn a_continuation_at_the_end_of_the_file_still_counts() {
        let parsed = parse("six==1.16.0\nrequests==2.32.3 \\");
        let names: Vec<&str> = parsed.entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["six", "requests"]);
    }
}
