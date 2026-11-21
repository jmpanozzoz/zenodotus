//! Turning parsed entries into dependencies (design §6.2 and §8.5): canonical names, git dependencies, private
//! registries, `private_scopes` and `GOPRIVATE`.

use std::path::Path;

use zenodotus_core::Ecosystem;

use crate::{Dependency, Diagnostic, DiagnosticKind, Entry, Options, Source};

/// The public npm registries.
pub(crate) const NPM_REGISTRIES: &[&str] = &["registry.npmjs.org", "registry.yarnpkg.com"];
/// The public Python package index and its file host.
pub(crate) const PYPI_HOSTS: &[&str] = &["pypi.org", "files.pythonhosted.org"];
/// The public Ruby gem host.
pub(crate) const RUBYGEMS_HOSTS: &[&str] = &["rubygems.org"];
/// The public pub.dev hosts.
pub(crate) const PUB_HOSTS: &[&str] = &["pub.dev", "pub.dartlang.org"];
/// Longest canonical name, in bytes (design §6.1).
const MAX_NAME_LEN: usize = 214;
/// Longest version, in bytes (design §6.3).
const MAX_VERSION_LEN: usize = 128;

/// The host and the path of `url`, for `scheme://[user@]host[:port]/path` and scp-style `user@host:path`.
fn split_url(url: &str) -> Option<(&str, &str)> {
    let (authority, path) = if let Some((_, rest)) = url.split_once("://") {
        rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()))
    } else {
        let (user_host, path) = url.split_once(':')?;
        (user_host.rsplit_once('@')?.1, path)
    };
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = host_port.split(':').next().unwrap_or_default();
    (!host.is_empty()).then_some((host, path))
}

/// The host of `url`.
pub(crate) fn host(url: &str) -> Option<&str> {
    split_url(url).map(|(host, _)| host)
}

/// [`Source::Registry`] when `url`'s host is one of `public`, otherwise [`Source::Private`].
pub(crate) fn registry_source(url: &str, public: &[&str]) -> Source {
    let is_public = host(url).is_some_and(|host| public.iter().any(|known| known.eq_ignore_ascii_case(host)));
    if is_public {
        Source::Registry
    } else {
        Source::Private { url: url.to_owned() }
    }
}

/// `owner/repo`, lowercased, when `url` names a repository on `github.com`.
fn github_repo(url: &str) -> Option<String> {
    let (host, path) = split_url(url)?;
    if !["github.com", "www.github.com"]
        .iter()
        .any(|known| known.eq_ignore_ascii_case(host))
    {
        return None;
    }
    let mut segments = path.split(['/', '?', '#']).filter(|segment| !segment.is_empty());
    let owner = segments.next()?;
    let repo = segments.next()?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    Some(format!("{owner}/{repo}").to_ascii_lowercase())
}

/// The name part of the canonical ID of `name` in `ecosystem` (design §6.2).
pub(crate) fn canonical_name(ecosystem: Ecosystem, name: &str) -> String {
    match ecosystem {
        Ecosystem::Pypi => {
            let mut normalized = String::with_capacity(name.len());
            for character in name.chars() {
                if matches!(character, '-' | '_' | '.') {
                    if !normalized.ends_with('-') {
                        normalized.push('-');
                    }
                } else {
                    normalized.push(character.to_ascii_lowercase());
                }
            }
            normalized
        }
        Ecosystem::Nuget | Ecosystem::Crates | Ecosystem::Packagist | Ecosystem::Pub | Ecosystem::Github => {
            name.to_ascii_lowercase()
        }
        _ => name.to_owned(),
    }
}

/// Whether `name` is a valid canonical name (design §6.1).
fn valid_name(name: &str) -> bool {
    (1..=MAX_NAME_LEN).contains(&name.len())
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.split('/').any(|segment| segment == "..")
}

/// Whether `version` is valid text for a scope (design §6.3); `/` only in a `github:` scope.
fn valid_version(version: &str, ecosystem: Ecosystem) -> bool {
    (1..=MAX_VERSION_LEN).contains(&version.len())
        && version.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(b, b'.' | b'_' | b'+' | b'-' | b'!')
                || (b == b'/' && ecosystem == Ecosystem::Github)
        })
}

/// Whether the Go module `module` matches a `GOPRIVATE` or `GONOPROXY` pattern: a glob over as many leading path
/// elements as the pattern has.
fn go_private(patterns: &[String], module: &str) -> bool {
    patterns.iter().any(|pattern| {
        let elements = pattern.split('/').count();
        let prefix: Vec<&str> = module.split('/').take(elements).collect();
        globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .is_ok_and(|glob| prefix.len() == elements && glob.compile_matcher().is_match(prefix.join("/")))
    })
}

/// The dependency `entry` names, or the diagnostic that keeps it out (design §8.5).
pub(crate) fn resolve(entry: &Entry, file: &Path, options: &Options) -> Result<Dependency, Diagnostic> {
    let diagnostic = |kind, message: String| Diagnostic::dependency(file.to_path_buf(), kind, &entry.name, message);
    let (ecosystem, name, version) = match &entry.source {
        Source::Private { url } => {
            let host = host(url).unwrap_or(url);
            return Err(diagnostic(
                DiagnosticKind::PrivateDependency,
                format!("{host} is not the public registry"),
            ));
        }
        Source::Git { url, commit } => {
            let Some(repo) = github_repo(url) else {
                let host = host(url).unwrap_or(url);
                return Err(diagnostic(
                    DiagnosticKind::UnsupportedGitHost,
                    format!("git dependency on {host}: only github.com repositories are supported"),
                ));
            };
            (Ecosystem::Github, repo, Some(commit.clone()))
        }
        Source::Registry => (
            entry.ecosystem,
            canonical_name(entry.ecosystem, &entry.name),
            entry.version.clone(),
        ),
    };
    if !valid_name(&name) {
        return Err(diagnostic(
            DiagnosticKind::ParseError,
            "invalid package name".to_owned(),
        ));
    }
    if version
        .as_deref()
        .is_some_and(|version| !valid_version(version, ecosystem))
    {
        return Err(diagnostic(DiagnosticKind::ParseError, "invalid version".to_owned()));
    }
    let id = format!("{}:{name}", ecosystem.prefix());
    if let Some(scope) = options
        .private_scopes
        .iter()
        .find(|scope| id.starts_with(scope.as_str()))
    {
        return Err(diagnostic(
            DiagnosticKind::PrivateDependency,
            format!("{id} is under the private scope {scope:?}"),
        ));
    }
    if ecosystem == Ecosystem::Go && go_private(&options.go_private, &name) {
        return Err(diagnostic(
            DiagnosticKind::PrivateDependency,
            format!("{name} matches GOPRIVATE or GONOPROXY"),
        ));
    }
    Ok(Dependency {
        ecosystem,
        name,
        version,
        direct: entry.direct,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Dependency, DiagnosticKind, Entry, Options, Source};
    use std::path::Path;
    use zenodotus_core::Ecosystem;

    fn options() -> Options {
        let mut options = Options {
            private_scopes: vec!["npm:@acme/".to_owned(), "nuget:acme.".to_owned()],
            ..Options::default()
        };
        options.add_go_patterns("*.corp.example.com,github.com/acme");
        options
    }

    fn ok(entry: &Entry) -> Dependency {
        resolve(entry, Path::new("f"), &options()).unwrap()
    }

    fn diagnostic(entry: &Entry) -> (DiagnosticKind, Option<String>) {
        let diagnostic = resolve(entry, Path::new("f"), &options()).unwrap_err();
        assert_eq!(diagnostic.file, Path::new("f"));
        (diagnostic.kind, diagnostic.name)
    }

    fn dependency(ecosystem: Ecosystem, name: &str, version: Option<&str>) -> Dependency {
        Dependency {
            ecosystem,
            name: name.to_owned(),
            version: version.map(str::to_owned),
            direct: false,
        }
    }

    #[test]
    fn names_are_canonicalized_per_ecosystem() {
        let cases = [
            (Ecosystem::Npm, "JSONStream", "JSONStream"),
            (Ecosystem::Pypi, "Django_REST.framework", "django-rest-framework"),
            (Ecosystem::Pypi, "a__b", "a-b"),
            (Ecosystem::Maven, "org.Example:Core", "org.Example:Core"),
            (Ecosystem::Nuget, "Newtonsoft.Json", "newtonsoft.json"),
            (
                Ecosystem::Go,
                "github.com/BurntSushi/toml",
                "github.com/BurntSushi/toml",
            ),
            (Ecosystem::Crates, "Serde_Json", "serde_json"),
            (Ecosystem::Packagist, "Laravel/Framework", "laravel/framework"),
            (Ecosystem::Rubygems, "ActiveSupport", "ActiveSupport"),
            (Ecosystem::Pub, "HTTP", "http"),
            (Ecosystem::Github, "Tokio-RS/Axum", "tokio-rs/axum"),
            (Ecosystem::Swift, "Kept", "Kept"),
        ];
        for (ecosystem, name, canonical) in cases {
            assert_eq!(canonical_name(ecosystem, name), canonical, "{ecosystem} {name}");
        }
    }

    #[test]
    fn registry_entries_become_dependencies_and_keep_absent_versions() {
        assert_eq!(
            ok(&Entry::registry(Ecosystem::Pypi, "Flask", Some("3.0.0"))),
            dependency(Ecosystem::Pypi, "flask", Some("3.0.0"))
        );
        assert_eq!(
            ok(&Entry::registry(Ecosystem::Npm, "react", None)),
            dependency(Ecosystem::Npm, "react", None)
        );
        assert_eq!(dependency(Ecosystem::Npm, "@a/b", None).canonical_id(), "npm:@a/b");
    }

    #[test]
    fn github_dependencies_pin_the_commit_and_other_git_hosts_are_unsupported() {
        let commit = "0123456789abcdef0123456789abcdef01234567";
        for url in [
            "git+https://github.com/Acme/Widget.git",
            "https://github.com/acme/widget",
            "git+ssh://git@github.com/acme/widget.git",
            "git@github.com:acme/widget.git",
            "ssh://git@github.com:22/acme/widget",
            "https://www.github.com/acme/widget/tree/main",
        ] {
            assert_eq!(
                ok(&Entry::git(Ecosystem::Crates, "widget", url, commit)),
                dependency(Ecosystem::Github, "acme/widget", Some(commit)),
                "{url}"
            );
        }
        for url in [
            "https://gitlab.com/acme/widget.git",
            "https://github.com/acme",
            "not a url",
        ] {
            assert_eq!(
                diagnostic(&Entry::git(Ecosystem::Crates, "widget", url, commit)),
                (DiagnosticKind::UnsupportedGitHost, Some("widget".to_owned())),
                "{url}"
            );
        }
    }

    #[test]
    fn private_registries_scopes_and_go_patterns_are_private() {
        let private = (DiagnosticKind::PrivateDependency, Some("x".to_owned()));
        let entry = |ecosystem, name: &str| Entry::registry(ecosystem, name, Some("1.0.0"));
        let mut from_private_registry = entry(Ecosystem::Npm, "x");
        from_private_registry.source = Source::Private {
            url: "https://npm.acme.dev/x".to_owned(),
        };
        assert_eq!(diagnostic(&from_private_registry), private);
        assert_eq!(
            diagnostic(&entry(Ecosystem::Npm, "@acme/x")).0,
            DiagnosticKind::PrivateDependency
        );
        assert_eq!(
            diagnostic(&entry(Ecosystem::Nuget, "Acme.X")).0,
            DiagnosticKind::PrivateDependency
        );
        assert_eq!(ok(&entry(Ecosystem::Nuget, "AcmeX")).name, "acmex");
        for module in [
            "git.corp.example.com/team/x",
            "github.com/acme/x",
            "github.com/acme/x/v2",
        ] {
            assert_eq!(
                diagnostic(&entry(Ecosystem::Go, module)).0,
                DiagnosticKind::PrivateDependency,
                "{module}"
            );
        }
        for module in ["github.com/acmeco/x", "corp.example.com/x", "github.com/other/x"] {
            assert_eq!(ok(&entry(Ecosystem::Go, module)).name, module);
        }
        assert_eq!(ok(&entry(Ecosystem::Npm, "acme")).name, "acme");
    }

    #[test]
    fn invalid_names_and_versions_are_parse_errors() {
        let long = "a".repeat(215);
        for name in ["", "a b", "a\u{1f}b", "../x", "a/../b", "/a", "a/", long.as_str()] {
            assert_eq!(
                diagnostic(&Entry::registry(Ecosystem::Npm, name, Some("1"))).0,
                DiagnosticKind::ParseError,
                "{name:?}"
            );
        }
        let long_version = "1".repeat(129);
        for version in ["", "1 0", "1/0", "${x}", long_version.as_str()] {
            assert_eq!(
                diagnostic(&Entry::registry(Ecosystem::Npm, "a", Some(version))).0,
                DiagnosticKind::ParseError,
                "{version:?}"
            );
        }
        assert!(
            resolve(
                &Entry::registry(Ecosystem::Pypi, "a", Some("1!2.0")),
                Path::new("f"),
                &options()
            )
            .is_ok()
        );
        assert!(
            resolve(
                &Entry::registry(Ecosystem::Npm, &"a".repeat(214), Some(&"1".repeat(128))),
                Path::new("f"),
                &options()
            )
            .is_ok()
        );
        assert_eq!(
            diagnostic(&Entry::git(
                Ecosystem::Npm,
                "w",
                "https://github.com/a/b",
                "not/a commit"
            ))
            .0,
            DiagnosticKind::ParseError
        );
    }

    #[test]
    fn hosts_are_read_from_urls_and_scp_paths() {
        assert_eq!(
            host("https://registry.npmjs.org/a/-/a-1.tgz"),
            Some("registry.npmjs.org")
        );
        assert_eq!(host("git+https://user:pw@GitHub.com:443/a/b"), Some("GitHub.com"));
        assert_eq!(host("git@host.example:a/b"), Some("host.example"));
        assert_eq!(host("registry+https://my.registry/index"), Some("my.registry"));
        assert_eq!(host("plain"), None);
        assert_eq!(host("https:///a"), None);
        assert_eq!(
            registry_source("https://registry.yarnpkg.com/x", NPM_REGISTRIES),
            Source::Registry
        );
        assert_eq!(
            registry_source("https://REGISTRY.npmjs.org/x", NPM_REGISTRIES),
            Source::Registry
        );
        assert_eq!(
            registry_source("https://npm.pkg.github.com/x", NPM_REGISTRIES),
            Source::Private {
                url: "https://npm.pkg.github.com/x".to_owned()
            }
        );
    }
}
