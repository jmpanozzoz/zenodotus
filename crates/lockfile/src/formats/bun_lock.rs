//! `bun.lock`: JSONC whose `packages.<key>` arrays start with `"<name>@<version>"` and the registry URL.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;
use zenodotus_core::Ecosystem;

use super::json;
use super::npm_lock::npm_resolved;
use crate::error::LockfileError;
use crate::private::{NPM_REGISTRIES, registry_source};
use crate::{Entry, Source};

#[derive(Deserialize)]
struct Lockfile {
    #[serde(default)]
    packages: BTreeMap<String, Vec<Value>>,
}

/// Every package, after the JSONC pre-pass.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    let plain = strip_jsonc(text);
    let lockfile: Lockfile = json(&plain, file)?;
    Ok(lockfile
        .packages
        .values()
        .filter_map(|package| entry(package))
        .collect())
}

/// The entry of one `packages` array; workspace, file and link packages are skipped.
fn entry(package: &[Value]) -> Option<Entry> {
    let id = package.first()?.as_str()?;
    let at = id.get(1..)?.find('@')? + 1;
    let (name, version) = (&id[..at], &id[at + 1..]);
    if version.contains(':') {
        return npm_resolved(version).map(|source| Entry {
            ecosystem: Ecosystem::Npm,
            name: name.to_owned(),
            version: None,
            source,
        });
    }
    let registry = package.get(1).and_then(Value::as_str).unwrap_or_default();
    let source = if registry.is_empty() {
        Source::Registry
    } else {
        registry_source(registry, NPM_REGISTRIES)
    };
    Some(Entry {
        ecosystem: Ecosystem::Npm,
        name: name.to_owned(),
        version: Some(version.to_owned()),
        source,
    })
}

/// JSON from JSONC: comments outside strings are removed (keeping their newlines, so lines still match), then
/// trailing commas before `}` or `]`.
fn strip_jsonc(text: &str) -> String {
    let without_comments = scan(text.as_bytes(), |bytes, index, out| {
        match (bytes[index], bytes.get(index + 1)) {
            (b'/', Some(b'/')) => bytes[index..]
                .iter()
                .position(|&b| b == b'\n')
                .map_or(bytes.len(), |end| index + end),
            (b'/', Some(b'*')) => {
                let end = bytes[index + 2..]
                    .windows(2)
                    .position(|pair| pair == b"*/")
                    .map_or(bytes.len(), |end| index + 2 + end + 2);
                out.extend(bytes[index..end].iter().filter(|&&b| b == b'\n'));
                end
            }
            _ => index,
        }
    });
    let plain = scan(&without_comments, |bytes, index, _| {
        let next = bytes[index + 1..].iter().find(|b| !b.is_ascii_whitespace());
        if bytes[index] == b',' && matches!(next, Some(b'}' | b']')) {
            index + 1
        } else {
            index
        }
    });
    String::from_utf8(plain).unwrap_or_default()
}

/// Copies `bytes`, letting `skip` replace any stretch outside a string: it returns where copying resumes, or the
/// index it was given to copy that byte.
fn scan(bytes: &[u8], skip: impl Fn(&[u8], usize, &mut Vec<u8>) -> usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut in_string = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            out.push(byte);
            if byte == b'\\' {
                out.extend(bytes.get(index + 1));
                index += 1;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        let resume = skip(bytes, index, &mut out);
        if resume == index {
            in_string = byte == b'"';
            out.push(byte);
            index += 1;
        } else {
            index = resume;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Source;

    fn registry(name: &str, version: &str) -> Entry {
        Entry::registry(Ecosystem::Npm, name, Some(version))
    }

    #[test]
    fn packages_map_to_entries() {
        let text = include_str!("../../../../fixtures/lockfiles/bun/bun.lock");
        let mut secret = registry("@acme/secret", "1.0.0");
        secret.source = Source::Private {
            url: "https://npm.acme.dev/@acme/secret/-/secret-1.0.0.tgz".to_owned(),
        };
        assert_eq!(
            parse(text, Path::new("bun.lock")).unwrap(),
            [
                secret,
                Entry::git(
                    Ecosystem::Npm,
                    "glab",
                    "git+https://gitlab.com/acme/glab.git",
                    "89abcdef0123456789abcdef0123456789abcdef"
                ),
                registry("loose-envify", "1.4.0"),
                registry("react", "18.2.0"),
                registry("slashes", "1.0.0"),
                Entry::git(
                    Ecosystem::Npm,
                    "widget",
                    "https://github.com/acme/widget",
                    "0123456789abcdef0123456789abcdef01234567"
                ),
            ]
        );
    }

    #[test]
    fn comments_and_trailing_commas_are_removed_outside_strings_only() {
        assert_eq!(
            strip_jsonc("{\"a\": \"//x\", // c\n\"b\": [1, 2,],\n/* x\ny */ \"c\": \"\\\"/*\",\n}"),
            "{\"a\": \"//x\", \n\"b\": [1, 2],\n\n \"c\": \"\\\"/*\"\n}"
        );
        assert_eq!(strip_jsonc("[1, /* unterminated"), "[1, ");
        assert_eq!(strip_jsonc("\"open \\"), "\"open \\");
    }

    #[test]
    fn malformed_json_is_a_parse_error_on_the_original_line() {
        let error = parse("{\n// comment\n\"packages\": {\"a\": [1,]],\n}", Path::new("bun.lock")).err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(3), .. })),
            "{error:?}"
        );
    }
}
