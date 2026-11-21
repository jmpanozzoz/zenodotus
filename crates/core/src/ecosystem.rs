//! Package ecosystems and the prefixes of their canonical IDs (design §6.2).

use std::fmt;

use serde::{Deserialize, Serialize};

/// A package ecosystem, written as the prefix of a canonical ID such as `npm:react` or `crates:serde`.
///
/// `Swift` is accepted as input only (`swift:github.com/<owner>/<repo>` names a GitHub repository), and `Runtime`
/// prefixes the language runtimes (`runtime:rust`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ecosystem {
    /// The npm registry.
    Npm,
    /// The Python Package Index.
    Pypi,
    /// Maven Central.
    Maven,
    /// NuGet.
    Nuget,
    /// Go modules.
    Go,
    /// crates.io.
    Crates,
    /// Packagist.
    Packagist,
    /// RubyGems.
    Rubygems,
    /// pub.dev.
    Pub,
    /// Swift packages; input only.
    Swift,
    /// A GitHub repository.
    Github,
    /// A language runtime.
    Runtime,
}

impl Ecosystem {
    /// Every ecosystem, in the order of design §6.2.
    pub const ALL: [Self; 12] = [
        Self::Npm,
        Self::Pypi,
        Self::Maven,
        Self::Nuget,
        Self::Go,
        Self::Crates,
        Self::Packagist,
        Self::Rubygems,
        Self::Pub,
        Self::Swift,
        Self::Github,
        Self::Runtime,
    ];

    /// The canonical-ID prefix, without the `:`.
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pypi => "pypi",
            Self::Maven => "maven",
            Self::Nuget => "nuget",
            Self::Go => "go",
            Self::Crates => "crates",
            Self::Packagist => "packagist",
            Self::Rubygems => "rubygems",
            Self::Pub => "pub",
            Self::Swift => "swift",
            Self::Github => "github",
            Self::Runtime => "runtime",
        }
    }

    /// The ecosystem whose prefix is exactly `prefix`.
    #[must_use]
    pub fn from_prefix(prefix: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|ecosystem| ecosystem.prefix() == prefix)
    }
}

impl fmt::Display for Ecosystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.prefix())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_ecosystem_has_its_design_prefix() {
        let expected = [
            (Ecosystem::Npm, "npm"),
            (Ecosystem::Pypi, "pypi"),
            (Ecosystem::Maven, "maven"),
            (Ecosystem::Nuget, "nuget"),
            (Ecosystem::Go, "go"),
            (Ecosystem::Crates, "crates"),
            (Ecosystem::Packagist, "packagist"),
            (Ecosystem::Rubygems, "rubygems"),
            (Ecosystem::Pub, "pub"),
            (Ecosystem::Swift, "swift"),
            (Ecosystem::Github, "github"),
            (Ecosystem::Runtime, "runtime"),
        ];
        assert_eq!(Ecosystem::ALL.len(), expected.len());
        for ((ecosystem, prefix), listed) in expected.into_iter().zip(Ecosystem::ALL) {
            assert_eq!(ecosystem, listed);
            assert_eq!(ecosystem.prefix(), prefix);
            assert_eq!(ecosystem.to_string(), prefix);
            assert_eq!(Ecosystem::from_prefix(prefix), Some(ecosystem));
        }
    }

    #[test]
    fn unknown_or_differently_cased_prefixes_are_not_ecosystems() {
        for unknown in ["", "NPM", "cargo", "pypi:", "crates.io"] {
            assert_eq!(Ecosystem::from_prefix(unknown), None, "{unknown:?}");
        }
    }

    #[test]
    fn serde_uses_the_prefix() {
        assert_eq!(serde_json_like(Ecosystem::Packagist), "\"packagist\"");
        let parsed: Ecosystem = toml::from_str::<Wrapper>("e = \"pub\"").unwrap().e;
        assert_eq!(parsed, Ecosystem::Pub);
        assert!(toml::from_str::<Wrapper>("e = \"cargo\"").is_err());
    }

    #[derive(serde::Deserialize, serde::Serialize)]
    struct Wrapper {
        e: Ecosystem,
    }

    fn serde_json_like(ecosystem: Ecosystem) -> String {
        let text = toml::to_string(&Wrapper { e: ecosystem }).unwrap();
        text.trim().trim_start_matches("e = ").to_owned()
    }
}
