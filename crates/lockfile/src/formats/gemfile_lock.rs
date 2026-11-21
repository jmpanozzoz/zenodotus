//! `Gemfile.lock`: `GEM`, `GIT` and `PATH` sections whose `specs:` list `name (version)` at four spaces, and the
//! `RUBY VERSION` section.

use zenodotus_core::Ecosystem;

use crate::private::{RUBYGEMS_HOSTS, registry_source};
use crate::{Entry, Source};

/// What a `Gemfile.lock` records.
#[derive(Debug, Default)]
pub(crate) struct GemfileLock {
    /// Every locked gem from a `GEM` or `GIT` section, in file order.
    pub(crate) entries: Vec<Entry>,
    /// The `x.y.z` of the `RUBY VERSION` section, for MRI.
    pub(crate) ruby: Option<String>,
}

/// The section a line belongs to, with what it has said so far.
#[derive(Default)]
struct Section<'a> {
    name: &'a str,
    remote: Option<&'a str>,
    revision: Option<&'a str>,
}

impl Section<'_> {
    /// The source of the gems listed in this section; `None` for `PATH` and non-source sections.
    fn source(&self) -> Option<Source> {
        match self.name {
            "GEM" => Some(
                self.remote
                    .map_or(Source::Registry, |remote| registry_source(remote, RUBYGEMS_HOSTS)),
            ),
            "GIT" => Some(Source::Git {
                url: self.remote.unwrap_or_default().to_owned(),
                commit: self.revision.unwrap_or_default().to_owned(),
            }),
            _ => None,
        }
    }
}

/// The gems and the Ruby version of a `Gemfile.lock`.
pub(crate) fn parse(text: &str) -> GemfileLock {
    let mut lock = GemfileLock::default();
    let mut section = Section::default();
    for line in text.lines() {
        if !line.starts_with(' ') {
            section = Section {
                name: line.trim(),
                ..Section::default()
            };
        } else if let Some(remote) = line.strip_prefix("  remote: ") {
            section.remote.get_or_insert(remote.trim());
        } else if let Some(revision) = line.strip_prefix("  revision: ") {
            section.revision = Some(revision.trim());
        } else if section.name == "RUBY VERSION" {
            let version = line.trim().strip_prefix("ruby ").map(|version| {
                let end = version
                    .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                    .unwrap_or(version.len());
                version[..end].to_owned()
            });
            lock.ruby = version;
        } else if let Some(spec) = line.strip_prefix("    ").filter(|spec| !spec.starts_with(' ')) {
            let Some((name, version)) = spec.split_once(" (") else {
                continue;
            };
            let version = version.trim_end_matches(')');
            let version = version.split_once('-').map_or(version, |(version, _platform)| version);
            if let Some(source) = section.source() {
                lock.entries.push(Entry::from_source(
                    Ecosystem::Rubygems,
                    name.to_owned(),
                    Some(version.to_owned()),
                    source,
                ));
            }
        }
    }
    lock
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Entry, Source};
    use zenodotus_core::Ecosystem;

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/gemfile/Gemfile.lock");

    #[test]
    fn specs_map_to_entries_with_their_source() {
        let lock = parse(SAMPLE);
        let gem = |name, version| Entry::registry(Ecosystem::Rubygems, name, Some(version));
        assert_eq!(
            lock.entries,
            [
                Entry::git(
                    Ecosystem::Rubygems,
                    "widget",
                    "https://github.com/acme/widget.git",
                    "0123456789abcdef0123456789abcdef01234567"
                ),
                Entry::git(
                    Ecosystem::Rubygems,
                    "glab-lib",
                    "https://gitlab.com/acme/glab-lib.git",
                    "89abcdef0123456789abcdef0123456789abcdef"
                ),
                gem("nokogiri", "1.16.6"),
                gem("racc", "1.8.0"),
                gem("rack", "3.1.3"),
                Entry {
                    source: Source::Private {
                        url: "https://gems.acme.dev/".to_owned()
                    },
                    ..gem("acme-internal", "2.0.0")
                },
            ]
        );
        assert_eq!(lock.ruby.as_deref(), Some("3.3.2"));
    }

    #[test]
    fn a_spec_outside_a_source_section_or_without_a_version_is_skipped() {
        let lock = parse("    stray (1.0)\nGEM\n  specs:\n    bare\n    ok (1.0)\nRUBY VERSION\n   jruby 9.4\n");
        assert_eq!(lock.entries, [Entry::registry(Ecosystem::Rubygems, "ok", Some("1.0"))]);
        assert_eq!(lock.ruby, None);
    }
}
