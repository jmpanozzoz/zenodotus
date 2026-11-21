//! Runtime version files (design §7.7), read as text and never run. A channel, an alias or a missing file means the
//! latest stable version; a lower bound is taken as the version, cut to the runtime's granularity.

use std::path::Path;

use serde::Deserialize;

use crate::error::LockfileError;
use crate::formats::{json, toml, xml};

/// A runtime whose docs are versioned with the project (design §7.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Runtime {
    /// The Rust standard library.
    Rust,
    /// The JDK.
    Jdk,
    /// The Python interpreter.
    Python,
    /// Node.js.
    Node,
    /// Go.
    Go,
    /// .NET.
    Dotnet,
    /// The web platform.
    Web,
    /// Dart.
    Dart,
    /// Swift.
    Swift,
    /// PHP.
    Php,
    /// Ruby.
    Ruby,
}

impl Runtime {
    /// Every runtime, in the order of design §7.7.
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

    /// The name in its canonical ID, `runtime:<name>`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Jdk => "jdk",
            Self::Python => "python",
            Self::Node => "node",
            Self::Go => "go",
            Self::Dotnet => "dotnet",
            Self::Web => "web",
            Self::Dart => "dart",
            Self::Swift => "swift",
            Self::Php => "php",
            Self::Ruby => "ruby",
        }
    }

    /// How many version components the docs source distinguishes: major only, or major.minor.
    const fn components(self) -> usize {
        match self {
            Self::Jdk | Self::Node | Self::Dotnet => 1,
            _ => 2,
        }
    }
}

/// The version a project asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requested {
    /// The latest stable version: a channel, an alias, or no version file.
    Latest,
    /// A line at the runtime's granularity, such as `1.85` or `21`; its newest patch is used.
    Line(String),
}

/// The version `text` asks for: the first lower bound or plain version among its clauses, cut to the runtime's
/// granularity. Upper bounds, exclusions, channels and aliases ask for no version.
pub(crate) fn requested(runtime: Runtime, text: Option<&str>) -> Requested {
    if runtime == Runtime::Web {
        return Requested::Latest;
    }
    let clauses = text.into_iter().flat_map(|text| text.split(['|', ',', ' ']));
    let version = clauses
        .map(str::trim)
        .filter(|clause| !clause.starts_with(['<', '!']))
        .find_map(|clause| {
            let clause = ["ruby-", "net"]
                .iter()
                .fold(clause, |clause, prefix| clause.strip_prefix(prefix).unwrap_or(clause));
            let clause = clause.trim_start_matches(['>', '=', '~', '^', 'v']);
            let end = clause
                .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                .unwrap_or(clause.len());
            let numbers: Vec<&str> = clause[..end].split('.').filter(|part| !part.is_empty()).collect();
            (!numbers.is_empty()).then_some(numbers)
        });
    let Some(mut numbers) = version else {
        return Requested::Latest;
    };
    if runtime == Runtime::Jdk && numbers.len() > 1 && numbers[0] == "1" {
        numbers.remove(0);
    }
    numbers.truncate(runtime.components());
    Requested::Line(numbers.join("."))
}

/// The first non-empty line of a one-line version file such as `.nvmrc`, `.python-version` or `rust-toolchain`.
pub(crate) fn plain_version(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

#[derive(Deserialize)]
struct RustToolchain {
    toolchain: Option<Toolchain>,
}

#[derive(Deserialize)]
struct Toolchain {
    channel: Option<String>,
}

/// `[toolchain] channel` of `rust-toolchain.toml`.
pub(crate) fn rust_toolchain_toml(text: &str, file: &Path) -> Result<Option<String>, LockfileError> {
    let toolchain: RustToolchain = toml(text, file)?;
    Ok(toolchain.toolchain.and_then(|toolchain| toolchain.channel))
}

#[derive(Deserialize)]
struct GlobalJson {
    sdk: Option<Sdk>,
}

#[derive(Deserialize)]
struct Sdk {
    version: Option<String>,
}

/// `sdk.version` of `global.json`.
pub(crate) fn global_json(text: &str, file: &Path) -> Result<Option<String>, LockfileError> {
    let global: GlobalJson = json(text, file)?;
    Ok(global.sdk.and_then(|sdk| sdk.version))
}

/// `maven.compiler.release` of `pom.xml`, then the compiler plugin's `<release>`.
pub(crate) fn pom_release(text: &str, file: &Path) -> Result<Option<String>, LockfileError> {
    let project = xml::parse(text, file)?;
    let property = project
        .child("properties")
        .and_then(|properties| properties.child_text("maven.compiler.release"));
    let plugin = || {
        project
            .child("build")?
            .child("plugins")?
            .children_named("plugin")
            .find(|plugin| plugin.child_text("artifactId") == Some("maven-compiler-plugin"))?
            .child("configuration")?
            .child_text("release")
    };
    Ok(property.or_else(plugin).map(str::to_owned))
}

/// `N` of a Gradle toolchain's `JavaLanguageVersion.of(N)`, read as text.
pub(crate) fn gradle_language_version(text: &str) -> Option<String> {
    let (_, after) = text.split_once("JavaLanguageVersion.of(")?;
    let argument = after.split_once(')')?.0.trim();
    (!argument.is_empty() && argument.bytes().all(|b| b.is_ascii_digit())).then(|| argument.to_owned())
}

/// The version of the `// swift-tools-version:` first line of `Package.swift`.
pub(crate) fn swift_tools_version(text: &str) -> Option<&str> {
    let first = text.lines().next()?.strip_prefix("//")?.trim_start();
    let version = first.strip_prefix("swift-tools-version:")?;
    Some(version.split(';').next().unwrap_or_default().trim())
}

/// The first `netN.0`-style moniker of a project's target frameworks.
pub(crate) fn dotnet_framework(frameworks: &[String]) -> Option<&str> {
    frameworks.iter().map(String::as_str).find(|moniker| {
        moniker
            .strip_prefix("net")
            .and_then(|version| version.split_once('.'))
            .is_some_and(|(major, _)| !major.is_empty() && major.bytes().all(|b| b.is_ascii_digit()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn line(version: &str) -> Requested {
        Requested::Line(version.to_owned())
    }

    #[test]
    fn rust_channels_and_versions() {
        assert_eq!(requested(Runtime::Rust, Some("stable")), Requested::Latest);
        assert_eq!(requested(Runtime::Rust, Some("beta")), Requested::Latest);
        assert_eq!(requested(Runtime::Rust, Some("nightly")), Requested::Latest);
        assert_eq!(requested(Runtime::Rust, Some("nightly-2026-01-01")), Requested::Latest);
        assert_eq!(requested(Runtime::Rust, Some("1.85")), line("1.85"));
        assert_eq!(requested(Runtime::Rust, Some("1.85.1")), line("1.85"));
        assert_eq!(
            requested(Runtime::Rust, Some("1.85.0-x86_64-unknown-linux-gnu")),
            line("1.85")
        );
        assert_eq!(requested(Runtime::Rust, None), Requested::Latest);
        let toml = include_str!("../../../fixtures/lockfiles/runtime/rust-toolchain.toml");
        assert_eq!(
            rust_toolchain_toml(toml, Path::new("rust-toolchain.toml"))
                .unwrap()
                .as_deref(),
            Some("1.85")
        );
        assert_eq!(
            rust_toolchain_toml("[toolchain]\n", Path::new("rust-toolchain.toml")).unwrap(),
            None
        );
        assert!(rust_toolchain_toml("[toolchain\n", Path::new("rust-toolchain.toml")).is_err());
    }

    #[test]
    fn node_python_ruby_and_go_lines() {
        assert_eq!(requested(Runtime::Node, Some("lts/*")), Requested::Latest);
        assert_eq!(requested(Runtime::Node, Some("lts/iron")), Requested::Latest);
        assert_eq!(requested(Runtime::Node, Some("node")), Requested::Latest);
        assert_eq!(requested(Runtime::Node, Some("v20.11.1")), line("20"));
        assert_eq!(requested(Runtime::Node, Some(">=18.17 <23")), line("18"));
        assert_eq!(requested(Runtime::Python, Some(">=3.10")), line("3.10"));
        assert_eq!(requested(Runtime::Python, Some("<3.13,>=3.9")), line("3.9"));
        assert_eq!(requested(Runtime::Python, Some("<3.13")), Requested::Latest);
        assert_eq!(requested(Runtime::Python, Some("3.12.1")), line("3.12"));
        assert_eq!(requested(Runtime::Ruby, Some("ruby-3.3.2")), line("3.3"));
        assert_eq!(requested(Runtime::Go, Some("1.22.3")), line("1.22"));
        assert_eq!(requested(Runtime::Go, Some("1")), line("1"));
        assert_eq!(plain_version("\n  3.12.1  \n3.11\n"), Some("3.12.1"));
        assert_eq!(plain_version(" \n"), None);
    }

    #[test]
    fn jdk_dotnet_dart_swift_php_and_web() {
        assert_eq!(requested(Runtime::Jdk, Some("21")), line("21"));
        assert_eq!(requested(Runtime::Jdk, Some("1.8")), line("8"));
        assert_eq!(requested(Runtime::Dotnet, Some("8.0.204")), line("8"));
        assert_eq!(requested(Runtime::Dotnet, Some("net9.0")), line("9"));
        assert_eq!(requested(Runtime::Dart, Some(">=3.3.0 <4.0.0")), line("3.3"));
        assert_eq!(requested(Runtime::Swift, Some("5.10.1")), line("5.10"));
        assert_eq!(requested(Runtime::Php, Some("^8.2")), line("8.2"));
        assert_eq!(requested(Runtime::Php, Some("~8.1.0 || ^8.3")), line("8.1"));
        assert_eq!(requested(Runtime::Web, Some("1.0")), Requested::Latest);
        assert_eq!(requested(Runtime::Php, Some("*")), Requested::Latest);
    }

    #[test]
    fn version_texts_are_extracted_from_their_files() {
        let global = include_str!("../../../fixtures/lockfiles/runtime/global.json");
        assert_eq!(
            global_json(global, Path::new("global.json")).unwrap().as_deref(),
            Some("8.0.204")
        );
        assert_eq!(global_json("{}", Path::new("global.json")).unwrap(), None);
        assert!(global_json("{\n\"sdk\": 1}", Path::new("global.json")).is_err());
        let pom = include_str!("../../../fixtures/lockfiles/pom/pom.xml");
        assert_eq!(pom_release(pom, Path::new("pom.xml")).unwrap().as_deref(), Some("21"));
        let plugin = "<project><build><plugins><plugin><artifactId>maven-compiler-plugin</artifactId>\
            <configuration><release>17</release></configuration></plugin></plugins></build></project>";
        assert_eq!(
            pom_release(plugin, Path::new("pom.xml")).unwrap().as_deref(),
            Some("17")
        );
        let other =
            "<project><build><plugins><plugin><artifactId>other</artifactId></plugin></plugins></build></project>";
        assert_eq!(pom_release(other, Path::new("pom.xml")).unwrap(), None);
        assert!(pom_release("<project>", Path::new("pom.xml")).is_err());
        let gradle = include_str!("../../../fixtures/lockfiles/gradle/build.gradle");
        assert_eq!(gradle_language_version(gradle).as_deref(), Some("21"));
        assert_eq!(
            gradle_language_version("languageVersion.set(JavaLanguageVersion.of( 17 ))").as_deref(),
            Some("17")
        );
        assert_eq!(gradle_language_version("JavaLanguageVersion.of(x)"), None);
        assert_eq!(
            swift_tools_version("// swift-tools-version:5.9\nimport PackageDescription"),
            Some("5.9")
        );
        assert_eq!(swift_tools_version("// swift-tools-version: 6.0.1"), Some("6.0.1"));
        assert_eq!(swift_tools_version("import Foundation"), None);
        assert_eq!(
            dotnet_framework(&["netstandard2.0".to_owned(), "net8.0".to_owned()]),
            Some("net8.0")
        );
        assert_eq!(dotnet_framework(&["net48".to_owned()]), None);
    }

    #[test]
    fn runtimes_have_their_canonical_names() {
        let names: Vec<&str> = Runtime::ALL.iter().map(|runtime| runtime.name()).collect();
        assert_eq!(
            names,
            [
                "rust", "jdk", "python", "node", "go", "dotnet", "web", "dart", "swift", "php", "ruby"
            ]
        );
    }
}
