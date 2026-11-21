//! The manifests that declare names without locking versions: `package.json`, `pyproject.toml`, `Cargo.toml`,
//! `composer.json`, `Gemfile` and `pubspec.yaml`. Each also gives its workspace members and its runtime version
//! text, which [`crate::runtime`] interprets. Ranges are never parsed, so every entry has no version.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use zenodotus_core::Ecosystem;

use super::gradle_build::quoted;
use super::{json, toml, yaml};
use crate::error::LockfileError;
use crate::private::{PUB_HOSTS, RUBYGEMS_HOSTS, registry_source};
use crate::{Entry, Source};

/// What a manifest declares.
#[derive(Debug, Default)]
pub(crate) struct Manifest {
    /// The declared registry names, each without a version.
    pub(crate) entries: Vec<Entry>,
    /// Every declared name as a lockfile writes it, git and path dependencies included, which decides which locked
    /// packages are direct (design §8.4).
    pub(crate) names: Vec<String>,
    /// The workspace member patterns, as written.
    pub(crate) members: Vec<String>,
    /// The runtime version text it carries, as written: `engines.node`, `requires-python`, `rust-version`,
    /// `require.php` or `environment.sdk`.
    pub(crate) runtime: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PackageJson {
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    peer_dependencies: BTreeMap<String, String>,
    workspaces: Option<Workspaces>,
    engines: Option<Engines>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Workspaces {
    List(Vec<String>),
    Object { packages: Vec<String> },
}

#[derive(Deserialize)]
struct Engines {
    node: Option<String>,
}

/// The registry name a `package.json` dependency resolves to: the key, or the target of an `npm:` alias. Local,
/// workspace, git and URL specs name no registry package.
fn npm_name<'a>(key: &'a str, spec: &'a str) -> Option<&'a str> {
    match spec.strip_prefix("npm:") {
        Some(aliased) => Some(aliased.get(1..)?.rfind('@').map_or(aliased, |at| &aliased[..=at])),
        None => (!spec.contains([':', '/'])).then_some(key),
    }
}

/// `package.json`: the four dependency maps, `workspaces` and `engines.node`. A git, path or URL dependency is
/// declared under its key.
pub(crate) fn package_json(text: &str, file: &Path) -> Result<Manifest, LockfileError> {
    let package: PackageJson = json(text, file)?;
    let declared = [
        &package.dependencies,
        &package.dev_dependencies,
        &package.optional_dependencies,
        &package.peer_dependencies,
    ]
    .into_iter()
    .flatten();
    let entries = declared
        .clone()
        .filter_map(|(key, spec)| npm_name(key, spec))
        .map(|name| Entry::registry(Ecosystem::Npm, name, None))
        .collect();
    let names = declared
        .map(|(key, spec)| npm_name(key, spec).unwrap_or(key).to_owned())
        .collect();
    let members = match package.workspaces {
        Some(Workspaces::List(members) | Workspaces::Object { packages: members }) => members,
        None => Vec::new(),
    };
    Ok(Manifest {
        entries,
        names,
        members,
        runtime: package.engines.and_then(|engines| engines.node),
    })
}

#[derive(Deserialize)]
struct Pyproject {
    project: Option<Project>,
    #[serde(default, rename = "dependency-groups")]
    dependency_groups: BTreeMap<String, Vec<GroupItem>>,
    tool: Option<Tool>,
}

#[derive(Deserialize)]
struct Project {
    #[serde(rename = "requires-python")]
    requires_python: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default, rename = "optional-dependencies")]
    optional_dependencies: BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GroupItem {
    Requirement(String),
    Include(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
struct Tool {
    poetry: Option<Poetry>,
}

#[derive(Deserialize)]
struct Poetry {
    #[serde(default)]
    dependencies: BTreeMap<String, PoetryDependency>,
    #[serde(default, rename = "dev-dependencies")]
    dev_dependencies: BTreeMap<String, PoetryDependency>,
    #[serde(default)]
    group: BTreeMap<String, PoetryGroup>,
}

#[derive(Deserialize)]
struct PoetryGroup {
    #[serde(default)]
    dependencies: BTreeMap<String, PoetryDependency>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PoetryDependency {
    Version(#[expect(dead_code, reason = "a range is never parsed (design §8.4)")] String),
    Table {
        git: Option<String>,
        path: Option<String>,
        url: Option<String>,
    },
}

impl PoetryDependency {
    /// Whether it comes from an index, not from git, a path or a URL.
    fn is_registry(&self) -> bool {
        match self {
            Self::Version(_) => true,
            Self::Table { git, path, url } => git.is_none() && path.is_none() && url.is_none(),
        }
    }
}

/// The name of a PEP 508 requirement, the text before any extras, specifier or marker, and whether it is a direct
/// URL reference (`name @ url`), which names no index package.
fn pep508(requirement: &str) -> Option<(&str, bool)> {
    let requirement = requirement.trim_start();
    let end = requirement
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        .unwrap_or(requirement.len());
    let (name, rest) = requirement.split_at(end);
    (!name.is_empty()).then(|| (name, rest.trim_start().starts_with('@')))
}

/// `pyproject.toml`: `[project]` dependencies and optional dependencies, `[dependency-groups]`, the Poetry
/// dependency tables without `python`, and `requires-python`. Direct URL, git and path dependencies are declared but
/// name no index package.
pub(crate) fn pyproject(text: &str, file: &Path) -> Result<Manifest, LockfileError> {
    let pyproject: Pyproject = toml(text, file)?;
    let mut requirements: Vec<&str> = Vec::new();
    if let Some(project) = &pyproject.project {
        let optional = project.optional_dependencies.values().flatten();
        requirements.extend(project.dependencies.iter().chain(optional).map(String::as_str));
    }
    let groups = pyproject.dependency_groups.values().flatten();
    requirements.extend(groups.filter_map(|item| match item {
        GroupItem::Requirement(requirement) => Some(requirement.as_str()),
        GroupItem::Include(_) => None,
    }));
    let mut declared: Vec<(&str, bool)> = requirements
        .into_iter()
        .filter_map(pep508)
        .map(|(name, url)| (name, !url))
        .collect();
    if let Some(poetry) = pyproject.tool.as_ref().and_then(|tool| tool.poetry.as_ref()) {
        let groups = poetry.group.values().map(|group| &group.dependencies);
        let tables = [&poetry.dependencies, &poetry.dev_dependencies]
            .into_iter()
            .chain(groups);
        declared.extend(
            tables
                .flatten()
                .filter(|(name, _)| *name != "python")
                .map(|(name, dependency)| (name.as_str(), dependency.is_registry())),
        );
    }
    Ok(Manifest {
        entries: declared
            .iter()
            .filter(|(_, registry)| *registry)
            .map(|(name, _)| Entry::registry(Ecosystem::Pypi, name, None))
            .collect(),
        names: declared.iter().map(|(name, _)| (*name).to_owned()).collect(),
        members: Vec::new(),
        runtime: pyproject.project.and_then(|project| project.requires_python),
    })
}

#[derive(Deserialize)]
struct CargoToml {
    package: Option<CargoPackage>,
    workspace: Option<CargoWorkspace>,
    #[serde(default)]
    dependencies: BTreeMap<String, CargoDependency>,
    #[serde(default, rename = "dev-dependencies")]
    dev_dependencies: BTreeMap<String, CargoDependency>,
    #[serde(default, rename = "build-dependencies")]
    build_dependencies: BTreeMap<String, CargoDependency>,
    #[serde(default)]
    target: BTreeMap<String, DependencyTables>,
}

#[derive(Deserialize)]
struct CargoPackage {
    #[serde(rename = "rust-version")]
    rust_version: Option<Inheritable>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Inheritable {
    Value(String),
    Workspace(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
struct CargoWorkspace {
    #[serde(default)]
    members: Vec<String>,
    package: Option<CargoWorkspacePackage>,
    #[serde(default)]
    dependencies: BTreeMap<String, CargoDependency>,
}

#[derive(Deserialize)]
struct CargoWorkspacePackage {
    #[serde(rename = "rust-version")]
    rust_version: Option<String>,
}

#[derive(Deserialize)]
struct DependencyTables {
    #[serde(default)]
    dependencies: BTreeMap<String, CargoDependency>,
    #[serde(default, rename = "dev-dependencies")]
    dev_dependencies: BTreeMap<String, CargoDependency>,
    #[serde(default, rename = "build-dependencies")]
    build_dependencies: BTreeMap<String, CargoDependency>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CargoDependency {
    Version(#[expect(dead_code, reason = "a range is never parsed (design §8.4)")] String),
    Table {
        package: Option<String>,
        path: Option<String>,
        git: Option<String>,
    },
}

/// The crate name of a dependency, `package` when renamed, and whether it comes from a registry: path and git
/// dependencies name no registry crate.
fn crate_name<'a>(key: &'a str, dependency: &'a CargoDependency) -> (&'a str, bool) {
    match dependency {
        CargoDependency::Version(_) => (key, true),
        CargoDependency::Table { package, path, git } => {
            (package.as_deref().unwrap_or(key), path.is_none() && git.is_none())
        }
    }
}

/// `Cargo.toml`: `[workspace.dependencies]`, then the dependency tables of the package and its targets (not flattened
/// into one struct, which would lose the line of a bad value),
/// `[workspace].members`, and `rust-version` from `[package]` or `[workspace.package]`.
pub(crate) fn cargo_toml(text: &str, file: &Path) -> Result<Manifest, LockfileError> {
    let manifest: CargoToml = toml(text, file)?;
    let workspace_dependencies = manifest.workspace.iter().map(|workspace| &workspace.dependencies);
    let own = [
        &manifest.dependencies,
        &manifest.dev_dependencies,
        &manifest.build_dependencies,
    ];
    let targets = manifest.target.values();
    let declared = workspace_dependencies
        .chain(own)
        .chain(targets.flat_map(|tables| {
            [
                &tables.dependencies,
                &tables.dev_dependencies,
                &tables.build_dependencies,
            ]
        }))
        .flatten()
        .map(|(key, dependency)| crate_name(key, dependency));
    let (registry, other): (Vec<_>, Vec<_>) = declared.partition(|(_, registry)| *registry);
    let entries = registry
        .iter()
        .map(|(name, _)| Entry::registry(Ecosystem::Crates, name, None))
        .collect();
    let names = registry
        .into_iter()
        .chain(other)
        .map(|(name, _)| name.to_owned())
        .collect();
    let own = manifest.package.and_then(|package| package.rust_version);
    let runtime = match (own, &manifest.workspace) {
        (Some(Inheritable::Value(version)), _) => Some(version),
        (_, Some(workspace)) => workspace
            .package
            .as_ref()
            .and_then(|package| package.rust_version.clone()),
        (_, None) => None,
    };
    Ok(Manifest {
        entries,
        names,
        members: manifest
            .workspace
            .map(|workspace| workspace.members)
            .unwrap_or_default(),
        runtime,
    })
}

#[derive(Deserialize)]
struct ComposerJson {
    #[serde(default)]
    require: BTreeMap<String, String>,
    #[serde(default, rename = "require-dev")]
    require_dev: BTreeMap<String, String>,
}

/// `composer.json`: the `vendor/name` keys of `require` and `require-dev`, which leaves out `php`, `ext-*`,
/// `lib-*` and the Composer APIs, and `require.php`.
pub(crate) fn composer_json(text: &str, file: &Path) -> Result<Manifest, LockfileError> {
    let mut composer: ComposerJson = json(text, file)?;
    let names: Vec<String> = composer
        .require
        .keys()
        .chain(composer.require_dev.keys())
        .filter(|name| name.contains('/'))
        .cloned()
        .collect();
    Ok(Manifest {
        entries: names
            .iter()
            .map(|name| Entry::registry(Ecosystem::Packagist, name, None))
            .collect(),
        names,
        members: Vec::new(),
        runtime: composer.require.remove("php"),
    })
}

/// `Gemfile`, read as text: `gem "name"` lines without a `path`, `git` or `github` option, each from the innermost
/// `source "url" do` block or the top-level `source`.
pub(crate) fn gemfile(text: &str) -> Manifest {
    let mut entries = Vec::new();
    let mut default_source: Option<&str> = None;
    let mut blocks: Vec<Option<&str>> = Vec::new();
    for line in text.lines().map(str::trim) {
        let opens_block = line.ends_with(" do") || line.contains(" do |");
        if line.starts_with("source ") {
            let url = quoted(line).first().copied();
            if opens_block {
                blocks.push(url);
            } else {
                default_source = url;
            }
        } else if opens_block {
            blocks.push(None);
        } else if line == "end" {
            blocks.pop();
        } else if let Some(arguments) = line.strip_prefix("gem ") {
            let local = ["path:", "git:", "github:", ":path", ":git", ":github"]
                .iter()
                .any(|option| arguments.contains(option));
            let Some(name) = quoted(arguments).first().copied().filter(|_| !local) else {
                continue;
            };
            let source = blocks.iter().rev().flatten().next().copied().or(default_source);
            let source = source.map_or(Source::Registry, |url| registry_source(url, RUBYGEMS_HOSTS));
            entries.push(Entry::from_source(Ecosystem::Rubygems, name.to_owned(), None, source));
        }
    }
    Manifest {
        entries,
        ..Manifest::default()
    }
}

#[derive(Deserialize)]
struct PubspecYaml {
    environment: Option<PubEnvironment>,
    #[serde(default)]
    dependencies: BTreeMap<String, Option<PubDependency>>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, Option<PubDependency>>,
}

#[derive(Deserialize)]
struct PubEnvironment {
    sdk: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PubDependency {
    Version(#[expect(dead_code, reason = "a range is never parsed (design §8.4)")] String),
    Table {
        hosted: Option<Hosted>,
        sdk: Option<serde::de::IgnoredAny>,
        path: Option<serde::de::IgnoredAny>,
        git: Option<serde::de::IgnoredAny>,
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Hosted {
    Url(String),
    Table { url: Option<String> },
}

/// The source of a `pubspec.yaml` dependency; SDK, path and git dependencies are not from a pub server.
fn pub_source(dependency: Option<&PubDependency>) -> Option<Source> {
    match dependency {
        None | Some(PubDependency::Version(_)) => Some(Source::Registry),
        Some(PubDependency::Table {
            sdk: None,
            path: None,
            git: None,
            hosted,
        }) => {
            let url = hosted.as_ref().and_then(|hosted| match hosted {
                Hosted::Url(url) => Some(url),
                Hosted::Table { url } => url.as_ref(),
            });
            Some(url.map_or(Source::Registry, |url| registry_source(url, PUB_HOSTS)))
        }
        Some(PubDependency::Table { .. }) => None,
    }
}

/// `pubspec.yaml`: the hosted names of `dependencies` and `dev_dependencies`, and `environment.sdk`.
pub(crate) fn pubspec_yaml(text: &str, file: &Path) -> Result<Manifest, LockfileError> {
    let pubspec: PubspecYaml = yaml(text, file)?;
    let entries = pubspec
        .dependencies
        .iter()
        .chain(&pubspec.dev_dependencies)
        .filter_map(|(name, dependency)| {
            let source = pub_source(dependency.as_ref())?;
            Some(Entry::from_source(Ecosystem::Pub, name.clone(), None, source))
        })
        .collect();
    Ok(Manifest {
        entries,
        names: Vec::new(),
        members: Vec::new(),
        runtime: pubspec.environment.and_then(|environment| environment.sdk),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Entry;
    use std::path::Path;
    use zenodotus_core::Ecosystem;

    fn names(manifest: &Manifest) -> Vec<&str> {
        manifest.entries.iter().map(|entry| entry.name.as_str()).collect()
    }

    #[test]
    fn npm_package_json_declares_registry_names_workspaces_and_engines_node() {
        let text = include_str!("../../../../fixtures/lockfiles/manifests/package.json");
        let manifest = package_json(text, Path::new("package.json")).unwrap();
        assert_eq!(
            names(&manifest),
            ["new-name", "react", "@scope/real", "vitest", "fsevents", "react-dom"]
        );
        assert!(
            manifest
                .entries
                .iter()
                .all(|entry| *entry == Entry::registry(Ecosystem::Npm, &entry.name, None))
        );
        assert_eq!(manifest.members, ["packages/*", "tools/cli"]);
        assert_eq!(manifest.runtime.as_deref(), Some(">=20.11"));
    }

    #[test]
    fn npm_package_json_workspaces_may_be_an_object_and_errors_have_a_line() {
        let manifest = package_json(r#"{"workspaces": {"packages": ["apps/*"]}}"#, Path::new("package.json")).unwrap();
        assert_eq!(manifest.members, ["apps/*"]);
        assert_eq!(manifest.entries.len(), 0);
        assert_eq!(manifest.runtime, None);
        let error = package_json("{\n\"dependencies\": 1}", Path::new("package.json")).err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(2), .. })),
            "{error:?}"
        );
    }

    #[test]
    fn pypi_pyproject_declares_pep_621_dependency_group_and_poetry_names() {
        let text = include_str!("../../../../fixtures/lockfiles/manifests/pyproject.toml");
        let manifest = pyproject(text, Path::new("pyproject.toml")).unwrap();
        assert_eq!(
            names(&manifest),
            [
                "fastapi", "Pydantic", "uvicorn", "ruff", "pytest", "httpx", "requests", "black", "mkdocs"
            ]
        );
        assert!(
            manifest
                .entries
                .iter()
                .all(|entry| entry.ecosystem == Ecosystem::Pypi && entry.version.is_none())
        );
        assert_eq!(manifest.runtime.as_deref(), Some(">=3.10"));
        assert_eq!(manifest.members.len(), 0);
        let error = pyproject(
            "[project]
dependencies = [
1]
",
            Path::new("pyproject.toml"),
        )
        .err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(3), .. })),
            "{error:?}"
        );
        assert_eq!(pyproject("", Path::new("pyproject.toml")).unwrap().entries.len(), 0);
    }

    #[test]
    fn crates_cargo_toml_declares_dependency_names_members_and_rust_version() {
        let text = include_str!("../../../../fixtures/lockfiles/manifests/Cargo.toml.sample");
        let manifest = cargo_toml(text, Path::new("Cargo.toml")).unwrap();
        assert_eq!(
            names(&manifest),
            ["tokio", "serde_json", "serde", "tokio", "proptest", "cc", "libc"]
        );
        assert!(
            manifest
                .entries
                .iter()
                .all(|entry| entry.ecosystem == Ecosystem::Crates && entry.version.is_none())
        );
        assert_eq!(manifest.members, ["crates/*", "xtask"]);
        assert_eq!(manifest.runtime.as_deref(), Some("1.85"));
        let package = cargo_toml("[package]\nrust-version = \"1.80\"\n", Path::new("Cargo.toml")).unwrap();
        assert_eq!(package.runtime.as_deref(), Some("1.80"));
        let inherited = cargo_toml("[package]\nrust-version.workspace = true\n", Path::new("Cargo.toml")).unwrap();
        assert_eq!(inherited.runtime, None);
        let error = cargo_toml("[dependencies]\nserde = 1\n", Path::new("Cargo.toml")).err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(2), .. })),
            "{error:?}"
        );
    }

    #[test]
    fn packagist_composer_json_declares_package_names_and_require_php() {
        let text = include_str!("../../../../fixtures/lockfiles/manifests/composer.json");
        let manifest = composer_json(text, Path::new("composer.json")).unwrap();
        assert_eq!(
            names(&manifest),
            ["laravel/framework", "monolog/monolog", "phpunit/phpunit"]
        );
        assert!(
            manifest
                .entries
                .iter()
                .all(|entry| entry.ecosystem == Ecosystem::Packagist && entry.version.is_none())
        );
        assert_eq!(manifest.runtime.as_deref(), Some("^8.2"));
        let error = composer_json("{\n\"require\": []}", Path::new("composer.json")).err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(2), .. })),
            "{error:?}"
        );
    }

    #[test]
    fn rubygems_gemfile_declares_gem_names_with_their_source_block() {
        let text = include_str!("../../../../fixtures/lockfiles/manifests/Gemfile");
        let manifest = gemfile(text);
        let gem = |name| Entry::registry(Ecosystem::Rubygems, name, None);
        assert_eq!(
            manifest.entries,
            [
                gem("rails"),
                gem("puma"),
                gem("rspec-rails"),
                Entry {
                    source: crate::Source::Private {
                        url: "https://gems.acme.dev".to_owned()
                    },
                    ..gem("acme-internal")
                },
                gem("after-block"),
            ]
        );
        assert_eq!(manifest.runtime, None);
        let private = gemfile("source 'https://gems.acme.dev'\ngem 'a'\n");
        assert!(matches!(private.entries[0].source, crate::Source::Private { .. }));
    }

    #[test]
    fn pub_pubspec_yaml_declares_hosted_names_and_the_dart_sdk() {
        let text = include_str!("../../../../fixtures/lockfiles/manifests/pubspec.yaml");
        let manifest = pubspec_yaml(text, Path::new("pubspec.yaml")).unwrap();
        let package = |name| Entry::registry(Ecosystem::Pub, name, None);
        let private = |name| Entry {
            source: Source::Private {
                url: "https://pub.acme.dev".to_owned(),
            },
            ..package(name)
        };
        assert_eq!(
            manifest.entries,
            [
                private("acme_internal"),
                private("acme_other"),
                package("collection"),
                package("explicit_public"),
                package("hosted_name_only"),
                package("http"),
                package("plain_table"),
                package("test"),
            ]
        );
        assert_eq!(manifest.runtime.as_deref(), Some(">=3.3.0 <4.0.0"));
        let error = pubspec_yaml("name: a\ndependencies: [1]\n", Path::new("pubspec.yaml")).err();
        assert!(
            matches!(error, Some(LockfileError::Parse { line: Some(2), .. })),
            "{error:?}"
        );
    }

    #[test]
    fn npm_declared_names_without_a_lockfile_are_direct() {
        let root = crate::testing::root_with(&[(
            "package.json",
            include_str!("../../../../fixtures/lockfiles/manifests/package.json"),
        )]);
        crate::testing::assert_all_direct(root.path());
    }

    #[test]
    fn pypi_declared_names_without_a_lockfile_are_direct() {
        let root = crate::testing::root_with(&[(
            "pyproject.toml",
            include_str!("../../../../fixtures/lockfiles/manifests/pyproject.toml"),
        )]);
        crate::testing::assert_all_direct(root.path());
    }

    #[test]
    fn crates_declared_names_without_a_lockfile_are_direct() {
        let root = crate::testing::root_with(&[(
            "Cargo.toml",
            include_str!("../../../../fixtures/lockfiles/manifests/Cargo.toml.sample"),
        )]);
        crate::testing::assert_all_direct(root.path());
    }

    #[test]
    fn packagist_declared_names_without_a_lockfile_are_direct() {
        let root = crate::testing::root_with(&[(
            "composer.json",
            include_str!("../../../../fixtures/lockfiles/manifests/composer.json"),
        )]);
        crate::testing::assert_all_direct(root.path());
    }

    #[test]
    fn rubygems_declared_names_without_a_lockfile_are_direct() {
        let root = crate::testing::root_with(&[(
            "Gemfile",
            include_str!("../../../../fixtures/lockfiles/manifests/Gemfile"),
        )]);
        crate::testing::assert_all_direct(root.path());
    }

    #[test]
    fn pub_declared_names_without_a_lockfile_are_direct() {
        let root = crate::testing::root_with(&[(
            "pubspec.yaml",
            include_str!("../../../../fixtures/lockfiles/manifests/pubspec.yaml"),
        )]);
        crate::testing::assert_all_direct(root.path());
    }
}
