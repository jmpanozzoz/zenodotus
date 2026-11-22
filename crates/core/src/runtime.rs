//! Runtime libraries: the standard libraries and platforms that are no registry package (design §6.7).

use std::fmt;

use crate::ecosystem::Ecosystem;
use crate::ids::CanonicalId;

/// A language runtime, written `runtime:<name>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Runtime {
    /// Rust's `std`, `core` and `alloc`.
    Rust,
    /// The JDK class library.
    Jdk,
    /// The Python standard library.
    Python,
    /// Node.js.
    Node,
    /// Go's standard library.
    Go,
    /// The .NET base class library.
    Dotnet,
    /// The web platform: DOM and JavaScript built-ins.
    Web,
    /// Dart's core libraries.
    Dart,
    /// The Swift standard library.
    Swift,
    /// PHP's functions and classes.
    Php,
    /// Ruby's core and standard library.
    Ruby,
}

/// Each runtime with its name, its aliases and the ecosystems that imply it, in the order of design §6.7.
const TABLE: [(Runtime, &str, &[&str], &[Ecosystem]); 11] = [
    (
        Runtime::Rust,
        "rust",
        &["std", "core", "alloc", "rust", "rust-std"],
        &[Ecosystem::Crates],
    ),
    (
        Runtime::Jdk,
        "jdk",
        &["java", "jdk", "openjdk", "java.base", "java.lang", "java.util"],
        &[Ecosystem::Maven],
    ),
    (
        Runtime::Python,
        "python",
        &["python", "stdlib", "python-stdlib"],
        &[Ecosystem::Pypi],
    ),
    (Runtime::Node, "node", &["node", "nodejs", "node.js"], &[Ecosystem::Npm]),
    (Runtime::Go, "go", &["go", "golang", "std"], &[Ecosystem::Go]),
    (
        Runtime::Dotnet,
        "dotnet",
        &["dotnet", ".net", "bcl"],
        &[Ecosystem::Nuget],
    ),
    (
        Runtime::Web,
        "web",
        &["web", "dom", "mdn", "javascript"],
        &[Ecosystem::Npm],
    ),
    (Runtime::Dart, "dart", &["dart"], &[Ecosystem::Pub]),
    (Runtime::Swift, "swift", &["swift", "swift-stdlib"], &[Ecosystem::Swift]),
    (Runtime::Php, "php", &["php"], &[Ecosystem::Packagist]),
    (Runtime::Ruby, "ruby", &["ruby", "ruby-core"], &[Ecosystem::Rubygems]),
];

/// Alias prefixes that name a module of a runtime: `node:fs`, `dart:core`.
const MODULE_PREFIXES: [(&str, Runtime); 2] = [("node:", Runtime::Node), ("dart:", Runtime::Dart)];

impl Runtime {
    /// Every runtime, in the order of design §6.7.
    pub const ALL: [Self; 11] = [
        Self::Rust,
        Self::Jdk,
        Self::Python,
        Self::Node,
        Self::Go,
        Self::Dotnet,
        Self::Web,
        Self::Dart,
        Self::Swift,
        Self::Php,
        Self::Ruby,
    ];

    /// The name after `runtime:`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        TABLE[self as usize].1
    }

    /// The runtime called exactly `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|runtime| runtime.name() == name)
    }

    /// The canonical ID, `runtime:<name>`.
    #[must_use]
    pub fn canonical_id(self) -> CanonicalId {
        CanonicalId::runtime(self)
    }

    /// The runtimes `alias` names, compared without ASCII case; `node:<module>` and `dart:<library>` name their
    /// runtime. Empty when it is no alias. `std` names two runtimes.
    #[must_use]
    pub fn from_alias(alias: &str) -> Vec<Self> {
        let alias = alias.to_ascii_lowercase();
        if let Some((_, runtime)) = MODULE_PREFIXES
            .iter()
            .find(|(prefix, _)| alias.len() > prefix.len() && alias.starts_with(prefix))
        {
            return vec![*runtime];
        }
        TABLE
            .iter()
            .filter(|(_, _, aliases, _)| aliases.contains(&alias.as_str()))
            .map(|(runtime, ..)| *runtime)
            .collect()
    }
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What an alias resolves to for one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasMatch {
    /// The name is no runtime alias.
    None,
    /// One runtime.
    One(Runtime),
    /// Several runtimes and no project that picks one: the caller fails with `ambiguous_library`.
    Ambiguous(Vec<Runtime>),
}

/// Resolves `alias` for a project whose ecosystems are `project`: an alias shared by several runtimes picks the one
/// runtime the project implies (design §6.7).
#[must_use]
pub fn resolve_alias(alias: &str, project: &[Ecosystem]) -> AliasMatch {
    let runtimes = Runtime::from_alias(alias);
    let implied = implied_runtimes(project);
    let picked: Vec<Runtime> = runtimes.iter().copied().filter(|r| implied.contains(r)).collect();
    match (runtimes.as_slice(), picked.as_slice()) {
        ([], _) => AliasMatch::None,
        ([only], _) | (_, [only]) => AliasMatch::One(*only),
        _ => AliasMatch::Ambiguous(runtimes),
    }
}

/// The runtimes implied by `ecosystems`, in the order of design §6.7, each once.
#[must_use]
pub fn implied_runtimes(ecosystems: &[Ecosystem]) -> Vec<Runtime> {
    TABLE
        .iter()
        .filter(|(.., implied_by)| implied_by.iter().any(|e| ecosystems.contains(e)))
        .map(|(runtime, ..)| *runtime)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_runtime_has_its_canonical_id() {
        let names = [
            "rust", "jdk", "python", "node", "go", "dotnet", "web", "dart", "swift", "php", "ruby",
        ];
        assert_eq!(Runtime::ALL.len(), names.len());
        for (runtime, name) in Runtime::ALL.into_iter().zip(names) {
            assert_eq!(runtime.name(), name);
            assert_eq!(Runtime::from_name(name), Some(runtime));
            assert_eq!(runtime.canonical_id().as_str(), format!("runtime:{name}"));
            assert_eq!(runtime.to_string(), name);
        }
        assert_eq!(Runtime::from_name("cobol"), None);
        assert_eq!(Runtime::from_name("Rust"), None);
    }

    #[test]
    fn aliases_name_their_runtimes() {
        let cases: [(&str, &[Runtime]); 16] = [
            ("std", &[Runtime::Rust, Runtime::Go]),
            ("rust-std", &[Runtime::Rust]),
            ("Java", &[Runtime::Jdk]),
            ("java.util", &[Runtime::Jdk]),
            ("stdlib", &[Runtime::Python]),
            ("node.js", &[Runtime::Node]),
            ("node:fs", &[Runtime::Node]),
            ("golang", &[Runtime::Go]),
            (".net", &[Runtime::Dotnet]),
            ("mdn", &[Runtime::Web]),
            ("dart", &[Runtime::Dart]),
            ("dart:core", &[Runtime::Dart]),
            ("swift-stdlib", &[Runtime::Swift]),
            ("php", &[Runtime::Php]),
            ("ruby-core", &[Runtime::Ruby]),
            ("npm:node", &[]),
        ];
        for (alias, expected) in cases {
            assert_eq!(Runtime::from_alias(alias), expected, "{alias}");
        }
        for not_alias in ["", "node:", "dart:", "react", "nodejs:fs"] {
            assert!(Runtime::from_alias(not_alias).is_empty(), "{not_alias}");
        }
    }

    #[test]
    fn a_shared_alias_resolves_through_the_project() {
        let resolve = |alias, ecosystems: &[Ecosystem]| resolve_alias(alias, ecosystems);
        assert_eq!(resolve("std", &[Ecosystem::Crates]), AliasMatch::One(Runtime::Rust));
        assert_eq!(resolve("std", &[Ecosystem::Go]), AliasMatch::One(Runtime::Go));
        assert_eq!(
            resolve("std", &[Ecosystem::Crates, Ecosystem::Go]),
            AliasMatch::Ambiguous(vec![Runtime::Rust, Runtime::Go])
        );
        assert_eq!(
            resolve("std", &[]),
            AliasMatch::Ambiguous(vec![Runtime::Rust, Runtime::Go])
        );
        assert_eq!(resolve("node", &[]), AliasMatch::One(Runtime::Node));
        assert_eq!(resolve("node", &[Ecosystem::Pypi]), AliasMatch::One(Runtime::Node));
        assert_eq!(resolve("npm:node", &[Ecosystem::Npm]), AliasMatch::None);
    }

    #[test]
    fn ecosystems_imply_their_runtimes() {
        assert_eq!(implied_runtimes(&[Ecosystem::Npm]), [Runtime::Node, Runtime::Web]);
        assert_eq!(
            implied_runtimes(&[Ecosystem::Maven, Ecosystem::Crates, Ecosystem::Maven]),
            [Runtime::Rust, Runtime::Jdk]
        );
        for (runtime, ecosystem) in [
            (Runtime::Python, Ecosystem::Pypi),
            (Runtime::Go, Ecosystem::Go),
            (Runtime::Dotnet, Ecosystem::Nuget),
            (Runtime::Dart, Ecosystem::Pub),
            (Runtime::Swift, Ecosystem::Swift),
            (Runtime::Php, Ecosystem::Packagist),
            (Runtime::Ruby, Ecosystem::Rubygems),
        ] {
            assert_eq!(implied_runtimes(&[ecosystem]), [runtime]);
        }
        assert_eq!(implied_runtimes(&[Ecosystem::Github, Ecosystem::Runtime]), []);
    }
}
