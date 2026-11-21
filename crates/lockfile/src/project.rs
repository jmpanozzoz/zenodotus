//! Reading one project root (design §8.2 to §8.6): the lockfile of each ecosystem in precedence order, workspace
//! members, .NET projects, manifests, runtime version files and diagnostics, with a cache keyed by the files' sizes
//! and modification times.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use zenodotus_core::Ecosystem;

use crate::diagnostics::Diagnostic;
use crate::error::LockfileError;
use crate::formats::manifests::{self, Manifest};
use crate::formats::{
    bun_lock, cargo_lock, composer_lock, csproj, directory_packages, gemfile_lock, go_mod, gradle_build,
    gradle_catalog, gradle_lockfile, npm_lock, nuget_lock, package_resolved, pdm_lock, pipfile_lock, pnpm_lock,
    poetry_lock, pom, pubspec_lock, requirements, sln, uv_lock, yarn_lock,
};
use crate::members::{self, MAX_MEMBERS};
use crate::private::resolve;
use crate::read::{Files, HEAD_LIMIT, limit_for};
use crate::roots::{has_extension, is_requirements};
use crate::runtime::{self, Requested, Runtime};
use crate::{Dependency, Entry, Options};

/// What one project root holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The root that was read.
    pub root: PathBuf,
    /// The dependencies, each canonical ID once, the first found winning.
    pub dependencies: Vec<Dependency>,
    /// The runtimes the root uses, in the order of [`Runtime::ALL`].
    pub runtimes: Vec<RuntimeVersion>,
    /// What was skipped or could not be parsed, and the dependencies kept out.
    pub diagnostics: Vec<Diagnostic>,
    /// The files read, relative to the root, in reading order.
    pub files: Vec<PathBuf>,
    /// The bytes read from them.
    pub bytes_read: u64,
}

/// A runtime the project uses and the version it asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeVersion {
    /// The runtime.
    pub runtime: Runtime,
    /// The version its version files ask for.
    pub requested: Requested,
}

/// The lockfile formats of the precedence table.
#[derive(Debug, Clone, Copy)]
enum Format {
    Npm,
    Pnpm,
    Yarn,
    Bun,
    Uv,
    Poetry,
    Pdm,
    Pipfile,
    GradleLockfile,
    GradleCatalog,
    GradleBuild,
    Pom,
    GoMod,
    CargoLock,
    ComposerLock,
    GemfileLock,
    PubspecLock,
    PackageResolved,
}

/// One row of the precedence table (design §8.4).
enum Group {
    /// The first file present wins; for the Python Package Index, the `requirements*.txt` files follow the fixed
    /// names.
    Files(Ecosystem, &'static [(&'static str, Format)]),
    /// The .NET projects of the solution, or of the root.
    Nuget,
}

const GROUPS: [Group; 10] = [
    Group::Files(
        Ecosystem::Npm,
        &[
            ("npm-shrinkwrap.json", Format::Npm),
            ("package-lock.json", Format::Npm),
            ("pnpm-lock.yaml", Format::Pnpm),
            ("yarn.lock", Format::Yarn),
            ("bun.lock", Format::Bun),
        ],
    ),
    Group::Files(
        Ecosystem::Pypi,
        &[
            ("uv.lock", Format::Uv),
            ("poetry.lock", Format::Poetry),
            ("pdm.lock", Format::Pdm),
            ("Pipfile.lock", Format::Pipfile),
        ],
    ),
    Group::Files(
        Ecosystem::Maven,
        &[
            ("gradle.lockfile", Format::GradleLockfile),
            ("gradle/libs.versions.toml", Format::GradleCatalog),
            ("build.gradle", Format::GradleBuild),
            ("build.gradle.kts", Format::GradleBuild),
            ("pom.xml", Format::Pom),
        ],
    ),
    Group::Nuget,
    Group::Files(Ecosystem::Go, &[("go.mod", Format::GoMod)]),
    Group::Files(Ecosystem::Crates, &[("Cargo.lock", Format::CargoLock)]),
    Group::Files(Ecosystem::Packagist, &[("composer.lock", Format::ComposerLock)]),
    Group::Files(Ecosystem::Rubygems, &[("Gemfile.lock", Format::GemfileLock)]),
    Group::Files(Ecosystem::Pub, &[("pubspec.lock", Format::PubspecLock)]),
    Group::Files(Ecosystem::Swift, &[("Package.resolved", Format::PackageResolved)]),
];

type ManifestReader = fn(&str, &Path) -> Result<Manifest, LockfileError>;

/// The manifests, read for their declared names when their ecosystem has no lockfile in the root.
const MANIFESTS: [(&str, Ecosystem, ManifestReader); 6] = [
    ("package.json", Ecosystem::Npm, manifests::package_json),
    ("pyproject.toml", Ecosystem::Pypi, manifests::pyproject),
    ("Cargo.toml", Ecosystem::Crates, manifests::cargo_toml),
    ("composer.json", Ecosystem::Packagist, manifests::composer_json),
    ("Gemfile", Ecosystem::Rubygems, |text, _| Ok(manifests::gemfile(text))),
    ("pubspec.yaml", Ecosystem::Pub, manifests::pubspec_yaml),
];

type VersionReader = fn(&str, &Path) -> Result<Option<String>, LockfileError>;

/// A one-line version file.
const PLAIN: VersionReader = |text, _| Ok(runtime::plain_version(text).map(str::to_owned));

/// A runtime, its version files in order, and the other files that show it is used.
type RuntimeFiles = (
    Runtime,
    &'static [(&'static str, VersionReader)],
    &'static [&'static str],
);

/// The version files of each runtime, first found wins (design §7.7); a runtime is used when one of its files, or a
/// file of its ecosystem, is present.
const RUNTIMES: [RuntimeFiles; 10] = [
    (
        Runtime::Rust,
        &[
            ("rust-toolchain.toml", runtime::rust_toolchain_toml),
            ("rust-toolchain", PLAIN),
            ("Cargo.toml", |text, file| {
                manifests::cargo_toml(text, file).map(|manifest| manifest.runtime)
            }),
        ],
        &["Cargo.lock"],
    ),
    (
        Runtime::Jdk,
        &[
            ("pom.xml", runtime::pom_release),
            ("build.gradle", |text, _| Ok(runtime::gradle_language_version(text))),
            ("build.gradle.kts", |text, _| Ok(runtime::gradle_language_version(text))),
        ],
        &[
            "gradle.lockfile",
            "gradle/libs.versions.toml",
            "settings.gradle",
            "settings.gradle.kts",
        ],
    ),
    (
        Runtime::Python,
        &[
            (".python-version", PLAIN),
            ("pyproject.toml", |text, file| {
                manifests::pyproject(text, file).map(|manifest| manifest.runtime)
            }),
        ],
        &["uv.lock", "poetry.lock", "pdm.lock", "Pipfile.lock", "requirements.txt"],
    ),
    (
        Runtime::Node,
        &[
            (".nvmrc", PLAIN),
            (".node-version", PLAIN),
            ("package.json", |text, file| {
                manifests::package_json(text, file).map(|manifest| manifest.runtime)
            }),
        ],
        &[
            "npm-shrinkwrap.json",
            "package-lock.json",
            "pnpm-lock.yaml",
            "pnpm-workspace.yaml",
            "yarn.lock",
            "bun.lock",
        ],
    ),
    (
        Runtime::Go,
        &[("go.mod", |text, _| Ok(go_mod::parse(text).go))],
        &["go.work"],
    ),
    (
        Runtime::Dotnet,
        &[("global.json", runtime::global_json)],
        &["Directory.Packages.props"],
    ),
    (
        Runtime::Dart,
        &[("pubspec.yaml", |text, file| {
            manifests::pubspec_yaml(text, file).map(|manifest| manifest.runtime)
        })],
        &["pubspec.lock"],
    ),
    (
        Runtime::Swift,
        &[("Package.swift", |text, _| {
            Ok(runtime::swift_tools_version(text).map(str::to_owned))
        })],
        &["Package.resolved"],
    ),
    (
        Runtime::Php,
        &[("composer.json", |text, file| {
            manifests::composer_json(text, file).map(|manifest| manifest.runtime)
        })],
        &["composer.lock"],
    ),
    (
        Runtime::Ruby,
        &[
            (".ruby-version", PLAIN),
            ("Gemfile.lock", |text, _| Ok(gemfile_lock::parse(text).ruby)),
        ],
        &["Gemfile"],
    ),
];

/// The state of reading one root.
struct Reader<'a> {
    root: &'a Path,
    options: &'a Options,
    files: Files,
    /// Each file asked for, with its text when it was read; `None` when it is absent or was refused.
    texts: HashMap<PathBuf, Option<Rc<str>>>,
    read: Vec<PathBuf>,
    dependencies: Vec<Dependency>,
    seen: HashSet<(Ecosystem, String)>,
    diagnostics: Vec<Diagnostic>,
    /// Whether the file limit was already reported; it is reported once.
    file_limit: bool,
    /// The .NET projects, in precedence order.
    projects: Vec<PathBuf>,
    /// Whether a solution file is in the root.
    solution: bool,
    /// Every path looked at, files and directories, for the cache's fingerprints.
    watched: Vec<PathBuf>,
}

impl<'a> Reader<'a> {
    fn new(root: &'a Path, options: &'a Options) -> Self {
        Self {
            root,
            options,
            files: Files::new(root),
            texts: HashMap::new(),
            read: Vec::new(),
            dependencies: Vec::new(),
            seen: HashSet::new(),
            diagnostics: Vec::new(),
            file_limit: false,
            projects: Vec::new(),
            solution: false,
            watched: vec![PathBuf::new()],
        }
    }

    fn watch(&mut self, path: &Path) {
        if !self.watched.iter().any(|watched| watched == path) {
            self.watched.push(path.to_path_buf());
        }
    }

    /// Whether `path` exists, symlinks and other non-regular files included.
    fn present(&mut self, path: &Path) -> bool {
        self.watch(path);
        fs::symlink_metadata(self.root.join(path)).is_ok()
    }

    /// The file names directly in `dir`, in ordinal order.
    fn listing(&mut self, dir: &Path) -> Vec<String> {
        self.watch(dir);
        let mut names: Vec<String> = fs::read_dir(self.root.join(dir))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        names
    }

    fn diagnose(&mut self, diagnostic: Diagnostic) {
        if !self.diagnostics.contains(&diagnostic) {
            self.diagnostics.push(diagnostic);
        }
    }

    fn error(&mut self, error: LockfileError) {
        if matches!(error, LockfileError::FileLimit { .. }) {
            if self.file_limit {
                return;
            }
            self.file_limit = true;
        }
        self.diagnose(error.into());
    }

    /// The text of `path`, read once: whole, or only its first [`HEAD_LIMIT`] bytes.
    fn text_of(&mut self, path: &Path, head: bool) -> Option<Rc<str>> {
        if let Some(text) = self.texts.get(path) {
            return text.clone();
        }
        self.watch(path);
        let before = self.files.files_read();
        let result = if head {
            self.files.read_head(path, HEAD_LIMIT)
        } else {
            self.files.read(path, limit_for(path))
        };
        if self.files.files_read() > before {
            self.read.push(path.to_path_buf());
        }
        let text = result.unwrap_or_else(|error| {
            self.error(error);
            None
        });
        let text: Option<Rc<str>> = text.map(Rc::from);
        self.texts.insert(path.to_path_buf(), text.clone());
        text
    }

    fn text(&mut self, path: &Path) -> Option<Rc<str>> {
        self.text_of(path, false)
    }

    /// `path` read and parsed; a parse error becomes a diagnostic.
    fn parsed<T>(&mut self, path: &Path, parse: impl FnOnce(&str, &Path) -> Result<T, LockfileError>) -> Option<T> {
        let text = self.text(path)?;
        parse(&text, path).map_err(|error| self.error(error)).ok()
    }

    /// Each entry resolved: a dependency not seen yet is kept, a git or private one becomes a diagnostic.
    fn add(&mut self, entries: Vec<Entry>, file: &Path) {
        for entry in entries {
            match resolve(&entry, file, self.options) {
                Ok(dependency) => {
                    if self.seen.insert((dependency.ecosystem, dependency.name.clone())) {
                        self.dependencies.push(dependency);
                    }
                }
                Err(diagnostic) => self.diagnose(diagnostic),
            }
        }
    }

    /// The member directories of every member list in the root, in list order, at most [`MAX_MEMBERS`].
    fn members(&mut self) -> Vec<PathBuf> {
        let mut lists: Vec<(PathBuf, Vec<String>)> = Vec::new();
        for (name, read) in [
            ("package.json", manifests::package_json as ManifestReader),
            ("Cargo.toml", manifests::cargo_toml),
        ] {
            let path = PathBuf::from(name);
            if let Some(manifest) = self.parsed(&path, read) {
                lists.push((path, manifest.members));
            }
        }
        let pnpm = PathBuf::from("pnpm-workspace.yaml");
        if let Some(patterns) = self.parsed(&pnpm, members::pnpm_workspace) {
            lists.push((pnpm, patterns));
        }
        let go_work = PathBuf::from("go.work");
        if let Some(text) = self.text(&go_work) {
            lists.push((go_work, members::go_work(&text)));
        }
        for name in ["settings.gradle", "settings.gradle.kts"] {
            let path = PathBuf::from(name);
            if let Some(text) = self.text(&path) {
                lists.push((path, members::settings_gradle(&text)));
            }
        }
        let mut dirs: Vec<PathBuf> = Vec::new();
        for (file, patterns) in lists {
            for pattern in &patterns {
                let parent = members::normalize(Path::new(pattern.trim_start_matches('!')))
                    .and_then(|path| path.parent().map(Path::to_path_buf));
                if let Some(parent) = parent {
                    self.watch(&parent);
                }
            }
            let found = members::expand(self.root, &file, &patterns);
            for error in found.errors {
                self.error(error);
            }
            for dir in found.dirs {
                if dirs.contains(&dir) {
                    continue;
                }
                if dirs.len() == MAX_MEMBERS {
                    self.error(LockfileError::TooManyMembers {
                        file: file.clone(),
                        limit: MAX_MEMBERS,
                    });
                    break;
                }
                self.watch(&dir);
                dirs.push(dir);
            }
        }
        dirs
    }

    /// The first lockfile present in `dir` among `files`, read; for the Python Package Index, the `requirements*.txt` files when no
    /// fixed name is present. Whether one was present.
    fn first_lockfile(&mut self, dir: &Path, ecosystem: Ecosystem, files: &[(&str, Format)]) -> bool {
        let first = files
            .iter()
            .map(|(name, format)| (dir.join(name), *format))
            .find(|(path, _)| self.present(path));
        if let Some((path, format)) = first {
            if let Some(text) = self.text(&path) {
                match lockfile(format, &text, &path) {
                    Ok(entries) => self.add(entries, &path),
                    Err(error) => self.error(error),
                }
            }
            return true;
        }
        if ecosystem != Ecosystem::Pypi {
            return false;
        }
        let mut names: Vec<String> = self
            .listing(dir)
            .into_iter()
            .filter(|name| is_requirements(name))
            .collect();
        if let Some(index) = names.iter().position(|name| name == "requirements.txt") {
            let main = names.remove(index);
            names.insert(0, main);
        }
        let mut visited = HashSet::new();
        for name in &names {
            self.requirements(&dir.join(name), &mut visited);
        }
        !names.is_empty()
    }

    /// A requirements file, then the files it includes, each once.
    fn requirements(&mut self, path: &Path, visited: &mut HashSet<PathBuf>) {
        if !visited.insert(path.to_path_buf()) {
            return;
        }
        let Some(text) = self.text(path) else {
            return;
        };
        let parsed = requirements::parse(&text);
        self.add(parsed.entries, path);
        let base = path.parent().unwrap_or(Path::new(""));
        for include in parsed.includes {
            match members::normalize(&base.join(&include.path)) {
                Some(target) => self.requirements(&target, visited),
                None => self.error(LockfileError::PathEscapesRoot {
                    file: path.to_path_buf(),
                    member: include.path,
                }),
            }
        }
    }

    /// The .NET projects: those of the first solution file in the root, else the root's `*.csproj` in ordinal order.
    /// Each project's `packages.lock.json` wins, then `Directory.Packages.props`, then the project file.
    fn nuget(&mut self) {
        let names = self.listing(Path::new(""));
        let solution = names
            .iter()
            .find(|name| has_extension(name, "sln"))
            .or_else(|| names.iter().find(|name| has_extension(name, "slnx")));
        let projects: Vec<String> = match solution {
            Some(name) => {
                self.solution = true;
                let path = PathBuf::from(name);
                let paths = if has_extension(name, "sln") {
                    self.text(&path).map(|text| sln::parse_sln(&text))
                } else {
                    self.parsed(&path, sln::parse_slnx)
                };
                let mut projects = Vec::new();
                for project in paths.unwrap_or_default() {
                    match members::normalize(Path::new(&project)) {
                        Some(normalized) => projects.push(normalized),
                        None => self.error(LockfileError::PathEscapesRoot {
                            file: path.clone(),
                            member: project,
                        }),
                    }
                }
                self.projects = projects;
                return self.nuget_projects();
            }
            None => names.into_iter().filter(|name| has_extension(name, "csproj")).collect(),
        };
        self.projects = projects.into_iter().map(PathBuf::from).collect();
        self.nuget_projects();
    }

    fn nuget_projects(&mut self) {
        let props = PathBuf::from("Directory.Packages.props");
        for project in self.projects.clone() {
            let lock = project.parent().unwrap_or(Path::new("")).join("packages.lock.json");
            let (file, entries) = if self.present(&lock) {
                let entries = self.parsed(&lock, nuget_lock::parse);
                (lock, entries)
            } else if self.present(&props) {
                let entries = self.parsed(&props, directory_packages::parse);
                (props.clone(), entries)
            } else {
                let entries = self.parsed(&project, csproj::parse).map(|project| project.entries);
                (project, entries)
            };
            self.add(entries.unwrap_or_default(), &file);
        }
    }

    /// The runtimes the root uses and the versions they ask for.
    fn runtimes(&mut self) -> Vec<RuntimeVersion> {
        let mut runtimes = Vec::new();
        for (runtime, versions, others) in RUNTIMES {
            let used = versions
                .iter()
                .map(|(name, _)| *name)
                .chain(others.iter().copied())
                .any(|name| self.present(Path::new(name)))
                || (runtime == Runtime::Python && self.listing(Path::new("")).iter().any(|name| is_requirements(name)))
                || (runtime == Runtime::Dotnet && (self.solution || !self.projects.is_empty()));
            if !used {
                continue;
            }
            let mut text = None;
            for (name, read) in versions {
                let path = Path::new(name);
                let Some(file) = self.text_of(path, *name == "Package.swift") else {
                    continue;
                };
                text = read(&file, path).unwrap_or_else(|error| {
                    self.error(error);
                    None
                });
                if text.is_some() {
                    break;
                }
            }
            if runtime == Runtime::Dotnet && text.is_none() {
                text = self.projects.first().cloned().and_then(|project| {
                    let project = self.parsed(&project, csproj::parse)?;
                    runtime::dotnet_framework(&project.frameworks).map(str::to_owned)
                });
            }
            runtimes.push(RuntimeVersion {
                runtime,
                requested: runtime::requested(runtime, text.as_deref()),
            });
        }
        runtimes
    }
}

/// The entries of one lockfile.
fn lockfile(format: Format, text: &str, file: &Path) -> Result<Vec<Entry>, LockfileError> {
    match format {
        Format::Npm => npm_lock::parse(text, file),
        Format::Pnpm => pnpm_lock::parse(text, file),
        Format::Yarn => yarn_lock::parse(text, file),
        Format::Bun => bun_lock::parse(text, file),
        Format::Uv => uv_lock::parse(text, file),
        Format::Poetry => poetry_lock::parse(text, file),
        Format::Pdm => pdm_lock::parse(text, file),
        Format::Pipfile => pipfile_lock::parse(text, file),
        Format::GradleLockfile => Ok(gradle_lockfile::parse(text)),
        Format::GradleCatalog => gradle_catalog::parse(text, file),
        Format::GradleBuild => Ok(gradle_build::parse(text)),
        Format::Pom => pom::parse(text, file),
        Format::GoMod => Ok(go_mod::parse(text).entries),
        Format::CargoLock => cargo_lock::parse(text, file),
        Format::ComposerLock => composer_lock::parse(text, file),
        Format::GemfileLock => Ok(gemfile_lock::parse(text).entries),
        Format::PubspecLock => pubspec_lock::parse(text, file),
        Format::PackageResolved => package_resolved::parse(text, file),
    }
}

/// Reads the project at `root` (design §8.2 to §8.6): its lockfiles in precedence order, its members' lockfiles for
/// the ecosystems the root does not lock, its manifests for the ecosystems with no lockfile in the root, and its
/// runtime version files. Nothing fails: every problem is a diagnostic.
#[must_use]
pub fn read_project(root: &Path, options: &Options) -> Project {
    read(root, options).0
}

/// The project at `root` and every path looked at to read it.
fn read(root: &Path, options: &Options) -> (Project, Vec<PathBuf>) {
    let mut reader = Reader::new(root, options);
    let members = reader.members();
    let mut locked = HashSet::new();
    for group in GROUPS {
        match group {
            Group::Nuget => reader.nuget(),
            Group::Files(ecosystem, files) => {
                if reader.first_lockfile(Path::new(""), ecosystem, files) {
                    locked.insert(ecosystem);
                } else {
                    for member in &members {
                        reader.first_lockfile(member, ecosystem, files);
                    }
                }
            }
        }
    }
    for dir in std::iter::once(Path::new("")).chain(members.iter().map(PathBuf::as_path)) {
        for (name, ecosystem, read) in MANIFESTS {
            if locked.contains(&ecosystem) {
                continue;
            }
            let path = dir.join(name);
            if let Some(manifest) = reader.parsed(&path, read) {
                reader.add(manifest.entries, &path);
            }
        }
    }
    let runtimes = reader.runtimes();
    let project = Project {
        root: root.to_path_buf(),
        dependencies: reader.dependencies,
        runtimes,
        diagnostics: reader.diagnostics,
        files: reader.read,
        bytes_read: reader.files.bytes_read(),
    };
    (project, reader.watched)
}

/// How long a root's files are trusted before their sizes and times are checked again.
const RECHECK: Duration = Duration::from_secs(1);

/// The size and modification time of a path, without following symlinks; `None` when it is absent.
type Fingerprint = Option<(u64, Option<SystemTime>)>;

fn fingerprints(root: &Path, paths: &[PathBuf]) -> Vec<Fingerprint> {
    paths
        .iter()
        .map(|path| {
            let metadata = fs::symlink_metadata(root.join(path)).ok()?;
            Some((metadata.len(), metadata.modified().ok()))
        })
        .collect()
}

/// One root's cached project.
#[derive(Debug)]
struct Cached {
    options: Options,
    checked: Instant,
    watched: Vec<PathBuf>,
    fingerprints: Vec<Fingerprint>,
    project: Arc<Project>,
}

/// Parsed projects keyed by root (design §8.3): a root is reused while the size and modification time of every path
/// looked at to read it, directories included, are unchanged. They are checked at most once per second per root.
#[derive(Debug, Default)]
pub struct ProjectCache {
    roots: HashMap<PathBuf, Cached>,
}

impl ProjectCache {
    /// The project at `root`, read again only when `options` differ from the cached read's, or when at least a
    /// second has passed since the last check at `now` and a path looked at changed.
    pub fn get(&mut self, root: &Path, options: &Options, now: Instant) -> Arc<Project> {
        if let Some(cached) = self.roots.get_mut(root).filter(|cached| cached.options == *options) {
            if now.saturating_duration_since(cached.checked) < RECHECK {
                return Arc::clone(&cached.project);
            }
            if fingerprints(root, &cached.watched) == cached.fingerprints {
                cached.checked = now;
                return Arc::clone(&cached.project);
            }
        }
        let (project, watched) = read(root, options);
        let project = Arc::new(project);
        let cached = Cached {
            options: options.clone(),
            checked: now,
            fingerprints: fingerprints(root, &watched),
            watched,
            project: Arc::clone(&project),
        };
        self.roots.insert(root.to_path_buf(), cached);
        project
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cause, DiagnosticKind, Requested, Runtime};
    use std::fs;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use zenodotus_core::Ecosystem;

    const WIDGET: &str = "0123456789abcdef0123456789abcdef01234567";

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let path = root.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        root
    }

    fn ids(project: &Project) -> Vec<String> {
        project
            .dependencies
            .iter()
            .map(|dependency| match &dependency.version {
                Some(version) => format!("{}@{version}", dependency.canonical_id()),
                None => dependency.canonical_id(),
            })
            .collect()
    }

    fn kinds(project: &Project) -> Vec<(String, DiagnosticKind, Option<String>)> {
        project
            .diagnostics
            .iter()
            .map(|diagnostic| {
                (
                    diagnostic.file.display().to_string(),
                    diagnostic.kind,
                    diagnostic.name.clone(),
                )
            })
            .collect()
    }

    fn lock(packages: &[(&str, &str)]) -> String {
        let entries: Vec<String> = packages
            .iter()
            .map(|(name, version)| format!("\"node_modules/{name}\": {{\"version\": \"{version}\"}}"))
            .collect();
        format!("{{\"lockfileVersion\": 3, \"packages\": {{{}}}}}", entries.join(","))
    }

    #[test]
    fn the_first_lockfile_present_wins_and_the_root_wins_over_members() {
        let root_lock = lock(&[("left-pad", "1.3.0"), ("react", "18.2.0")]);
        let member_lock = lock(&[("react", "17.0.0"), ("member-only", "1.0.0")]);
        let dir = project(&[
            (
                "package.json",
                r#"{"workspaces": ["packages/*"], "dependencies": {"declared-only": "^1"}}"#,
            ),
            ("package-lock.json", &root_lock),
            (
                "yarn.lock",
                "left-pad@^1:\n  version \"0.0.1\"\n  resolved \"https://registry.yarnpkg.com/x\"\n",
            ),
            (
                "packages/a/package.json",
                r#"{"dependencies": {"member-declared": "^1"}}"#,
            ),
            ("packages/a/package-lock.json", &member_lock),
        ]);
        let project = read_project(dir.path(), &Options::default());
        assert_eq!(ids(&project), ["npm:left-pad@1.3.0", "npm:react@18.2.0"]);
        assert!(project.diagnostics.is_empty(), "{:?}", project.diagnostics);
        assert!(
            !project.files.contains(&PathBuf::from("yarn.lock")),
            "{:?}",
            project.files
        );
        assert_eq!(project.root, dir.path());
    }

    #[test]
    fn member_lockfiles_and_manifests_fill_in_when_the_root_has_no_lockfile() {
        let dir = project(&[
            (
                "package.json",
                r#"{"workspaces": ["packages/*"], "dependencies": {"root-declared": "^1"}}"#,
            ),
            (
                "packages/a/package.json",
                r#"{"dependencies": {"member-declared": "^1", "locked": "^1"}}"#,
            ),
            ("packages/a/package-lock.json", &lock(&[("locked", "2.0.0")])),
            (
                "packages/b/package-lock.json",
                &lock(&[("locked", "1.0.0"), ("other", "3.0.0")]),
            ),
            ("Cargo.toml", "[dependencies]\nserde = \"1\"\n"),
        ]);
        let project = read_project(dir.path(), &Options::default());
        assert_eq!(
            ids(&project),
            [
                "npm:locked@2.0.0",
                "npm:other@3.0.0",
                "npm:root-declared",
                "crates:serde",
                "npm:member-declared"
            ]
        );
    }

    #[test]
    fn requirements_txt_comes_before_the_other_requirements_files_and_includes_are_followed() {
        let dir = project(&[
            (
                "requirements.txt",
                "-r base/common.txt\n-r ../../etc/passwd\n-r requirements.txt\nrequests==2.32.3\n",
            ),
            ("requirements-dev.txt", "requests==2.0.0\npytest==8.2.2\n"),
            ("base/common.txt", "-r ../requirements.txt\nsix==1.16.0\n"),
            ("pyproject.toml", "[project]\ndependencies = [\"declared\"]\n"),
        ]);
        let project = read_project(dir.path(), &Options::default());
        assert_eq!(
            ids(&project),
            ["pypi:requests@2.32.3", "pypi:six@1.16.0", "pypi:pytest@8.2.2"]
        );
        assert_eq!(
            kinds(&project),
            [("requirements.txt".to_owned(), DiagnosticKind::SkippedFile, None)]
        );
        assert_eq!(project.diagnostics[0].cause, Some(Cause::PathEscapesRoot));
    }

    #[test]
    fn the_version_catalog_one_level_down_comes_before_build_gradle() {
        let dir = project(&[
            (
                "gradle/libs.versions.toml",
                "[libraries]\nguava = \"com.google.guava:guava:33.2.1-jre\"\n",
            ),
            (
                "build.gradle",
                "dependencies {\n  implementation 'org.slf4j:slf4j-api:2.0.13'\n}\n",
            ),
            ("settings.gradle", "include ':app'\n"),
            (
                "app/build.gradle",
                "dependencies {\n  implementation 'member:artifact:1.0'\n}\n",
            ),
        ]);
        let project = read_project(dir.path(), &Options::default());
        assert_eq!(ids(&project), ["maven:com.google.guava:guava@33.2.1-jre"]);
    }

    #[test]
    fn git_and_private_dependencies_become_diagnostics() {
        let dir = project(&[
            (
                "Cargo.lock",
                include_str!("../../../fixtures/lockfiles/cargo/Cargo.lock"),
            ),
            (
                "go.mod",
                "module x\n\nrequire (\n\tgit.corp.example.com/team/lib v1.0.0\n\tgithub.com/gin-gonic/gin v1.10.0\n)\n",
            ),
            (
                "App.csproj",
                r#"<Project><ItemGroup><PackageReference Include="Acme.Secret" Version="1.0.0" /></ItemGroup></Project>"#,
            ),
        ]);
        let mut options = Options {
            private_scopes: vec!["nuget:acme.".to_owned()],
            ..Options::default()
        };
        options.add_go_patterns("*.corp.example.com");
        let project = read_project(dir.path(), &options);
        assert_eq!(
            ids(&project),
            [
                "go:github.com/gin-gonic/gin@v1.10.0".to_owned(),
                "crates:serde@1.0.203".to_owned(),
                "crates:serde_json@1.0.117".to_owned(),
                format!("github:acme/widget@{WIDGET}"),
            ]
        );
        let private = DiagnosticKind::PrivateDependency;
        assert_eq!(
            kinds(&project),
            [
                ("App.csproj".to_owned(), private, Some("Acme.Secret".to_owned())),
                (
                    "go.mod".to_owned(),
                    private,
                    Some("git.corp.example.com/team/lib".to_owned())
                ),
                (
                    "Cargo.lock".to_owned(),
                    DiagnosticKind::UnsupportedGitHost,
                    Some("glab-lib".to_owned())
                ),
                ("Cargo.lock".to_owned(), private, Some("internal".to_owned())),
            ]
        );
    }

    fn nuget_lock(packages: &[(&str, &str)]) -> String {
        let entries: Vec<String> = packages
            .iter()
            .map(|(id, version)| format!("\"{id}\": {{\"type\": \"Direct\", \"resolved\": \"{version}\"}}"))
            .collect();
        format!(
            "{{\"version\": 1, \"dependencies\": {{\"net8.0\": {{{}}}}}}}",
            entries.join(",")
        )
    }

    #[test]
    fn dotnet_projects_follow_the_solution_order_and_the_first_lock_wins() {
        let sln = "Project(\"{X}\") = \"Lib\", \"src\\Lib\\Lib.csproj\", \"{1}\"\nEndProject\n\
                   Project(\"{X}\") = \"App\", \"src\\App\\App.csproj\", \"{2}\"\nEndProject\n\
                   Project(\"{X}\") = \"Out\", \"..\\Out\\Out.csproj\", \"{3}\"\nEndProject\n";
        let dir = project(&[
            ("Demo.sln", sln),
            (
                "src/Lib/Lib.csproj",
                "<Project><PropertyGroup><TargetFramework>net9.0</TargetFramework></PropertyGroup></Project>",
            ),
            (
                "src/Lib/packages.lock.json",
                &nuget_lock(&[("Newtonsoft.Json", "13.0.3")]),
            ),
            ("src/App/App.csproj", "<Project/>"),
            (
                "src/App/packages.lock.json",
                &nuget_lock(&[("Newtonsoft.Json", "12.0.1"), ("Serilog", "3.1.1")]),
            ),
        ]);
        let project = read_project(dir.path(), &Options::default());
        assert_eq!(ids(&project), ["nuget:newtonsoft.json@13.0.3", "nuget:serilog@3.1.1"]);
        assert_eq!(project.diagnostics.len(), 1, "{:?}", project.diagnostics);
        assert_eq!(project.diagnostics[0].cause, Some(Cause::PathEscapesRoot));
        assert_eq!(project.diagnostics[0].file, Path::new("Demo.sln"));
        let dotnet = project
            .runtimes
            .iter()
            .find(|runtime| runtime.runtime == Runtime::Dotnet)
            .unwrap();
        assert_eq!(dotnet.requested, Requested::Line("9".to_owned()));
    }

    #[test]
    fn without_a_solution_root_projects_are_read_in_ordinal_order_with_central_versions() {
        let dir = project(&[
            (
                "B.csproj",
                r#"<Project><ItemGroup><PackageReference Include="Serilog" Version="2.0.0" /></ItemGroup></Project>"#,
            ),
            (
                "A.csproj",
                r#"<Project><ItemGroup><PackageReference Include="Serilog" Version="3.0.0" /></ItemGroup></Project>"#,
            ),
        ]);
        let project = read_project(dir.path(), &Options::default());
        assert_eq!(ids(&project), ["nuget:serilog@3.0.0"]);
        let central = project_with_props();
        assert_eq!(
            ids(&read_project(central.path(), &Options::default())),
            ["nuget:polly@8.4.0"]
        );
    }

    fn project_with_props() -> tempfile::TempDir {
        project(&[
            (
                "Demo.slnx",
                r#"<Solution><Project Path="src/App/App.csproj" /></Solution>"#,
            ),
            (
                "src/App/App.csproj",
                r#"<Project><ItemGroup><PackageReference Include="Polly" /></ItemGroup></Project>"#,
            ),
            (
                "Directory.Packages.props",
                r#"<Project><ItemGroup><PackageVersion Include="Polly" Version="8.4.0" /></ItemGroup></Project>"#,
            ),
        ])
    }

    #[test]
    fn runtime_versions_come_from_the_first_version_file_found() {
        let dir = project(&[
            ("rust-toolchain.toml", "[toolchain]\nchannel = \"1.85\"\n"),
            ("rust-toolchain", "1.70\n"),
            ("Cargo.toml", "[package]\nname = \"x\"\nrust-version = \"1.60\"\n"),
            (".nvmrc", "lts/*\n"),
            ("package.json", r#"{"engines": {"node": ">=18"}}"#),
            ("pyproject.toml", "[project]\nrequires-python = \">=3.10\"\n"),
            (
                "build.gradle",
                "java { toolchain { languageVersion = JavaLanguageVersion.of(21) } }\n",
            ),
            ("go.mod", "module x\n\ngo 1.22.3\n"),
            ("global.json", r#"{"sdk": {"version": "8.0.204"}}"#),
            ("pubspec.yaml", "name: x\nenvironment:\n  sdk: \">=3.3.0 <4.0.0\"\n"),
            (
                "Package.swift",
                "// swift-tools-version:5.9\nimport PackageDescription\n",
            ),
            ("composer.json", r#"{"require": {"php": "^8.2"}}"#),
            ("Gemfile.lock", "GEM\n  specs:\n\nRUBY VERSION\n   ruby 3.3.2p78\n"),
        ]);
        let project = read_project(dir.path(), &Options::default());
        let line = |version: &str| Requested::Line(version.to_owned());
        let runtimes: Vec<(Runtime, Requested)> = project
            .runtimes
            .iter()
            .map(|runtime| (runtime.runtime, runtime.requested.clone()))
            .collect();
        assert_eq!(
            runtimes,
            [
                (Runtime::Rust, line("1.85")),
                (Runtime::Jdk, line("21")),
                (Runtime::Python, line("3.10")),
                (Runtime::Node, Requested::Latest),
                (Runtime::Go, line("1.22")),
                (Runtime::Dotnet, line("8")),
                (Runtime::Dart, line("3.3")),
                (Runtime::Swift, line("5.9")),
                (Runtime::Php, line("8.2")),
                (Runtime::Ruby, line("3.3")),
            ]
        );
        assert!(project.diagnostics.is_empty(), "{:?}", project.diagnostics);
    }

    #[test]
    fn later_runtime_files_apply_when_earlier_ones_are_missing() {
        let dir = project(&[
            ("rust-toolchain", "stable\n"),
            ("pom.xml", include_str!("../../../fixtures/lockfiles/pom/pom.xml")),
            (".node-version", "20.11.1\n"),
            (".python-version", "3.12.1\n"),
            ("Gemfile", "gem 'rack'\n"),
        ]);
        let project = read_project(dir.path(), &Options::default());
        let runtimes: Vec<(Runtime, Requested)> = project
            .runtimes
            .iter()
            .map(|runtime| (runtime.runtime, runtime.requested.clone()))
            .collect();
        let line = |version: &str| Requested::Line(version.to_owned());
        assert_eq!(
            runtimes,
            [
                (Runtime::Rust, Requested::Latest),
                (Runtime::Jdk, line("21")),
                (Runtime::Python, line("3.12")),
                (Runtime::Node, line("20")),
                (Runtime::Ruby, Requested::Latest),
            ]
        );
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(read_project(empty.path(), &Options::default()).runtimes.len(), 0);
    }

    #[test]
    fn the_cache_rechecks_a_root_at_most_once_per_second() {
        let dir = project(&[("package-lock.json", &lock(&[("react", "18.2.0")]))]);
        let options = Options::default();
        let mut cache = ProjectCache::default();
        let start = Instant::now();
        let first = cache.get(dir.path(), &options, start);
        assert_eq!(ids(&first), ["npm:react@18.2.0"]);
        fs::write(dir.path().join("package-lock.json"), lock(&[("react", "19.0.0")])).unwrap();
        let within = cache.get(dir.path(), &options, start + Duration::from_millis(999));
        assert!(
            Arc::ptr_eq(&first, &within),
            "within a second the files are not checked"
        );
        let after = cache.get(dir.path(), &options, start + Duration::from_secs(1));
        assert_eq!(ids(&after), ["npm:react@19.0.0"]);
    }

    #[test]
    fn unchanged_files_keep_the_cached_project_and_other_options_read_again() {
        let dir = project(&[("package-lock.json", &lock(&[("@acme/ui", "1.0.0")]))]);
        let options = Options::default();
        let mut cache = ProjectCache::default();
        let start = Instant::now();
        let first = cache.get(dir.path(), &options, start);
        let later = cache.get(dir.path(), &options, start + Duration::from_secs(5));
        assert!(
            Arc::ptr_eq(&first, &later),
            "same sizes and times: the parsed result is reused"
        );
        let private = Options {
            private_scopes: vec!["npm:@acme/".to_owned()],
            ..Options::default()
        };
        let other = cache.get(dir.path(), &private, start + Duration::from_secs(5));
        assert_eq!(other.dependencies.len(), 0);
        assert_eq!(other.diagnostics[0].kind, DiagnosticKind::PrivateDependency);
    }

    #[test]
    fn a_new_lockfile_or_member_is_seen_after_the_recheck() {
        let dir = project(&[
            ("package.json", r#"{"workspaces": ["packages/*"]}"#),
            ("packages/a/package-lock.json", &lock(&[("a", "1.0.0")])),
        ]);
        let options = Options::default();
        let mut cache = ProjectCache::default();
        let start = Instant::now();
        assert_eq!(ids(&cache.get(dir.path(), &options, start)), ["npm:a@1.0.0"]);
        fs::create_dir(dir.path().join("packages/b")).unwrap();
        fs::write(dir.path().join("packages/b/package-lock.json"), lock(&[("b", "2.0.0")])).unwrap();
        let members = cache.get(dir.path(), &options, start + Duration::from_secs(1));
        assert_eq!(ids(&members), ["npm:a@1.0.0", "npm:b@2.0.0"]);
        fs::write(dir.path().join("package-lock.json"), lock(&[("root", "3.0.0")])).unwrap();
        let root = cache.get(dir.path(), &options, start + Duration::from_secs(2));
        assert_eq!(ids(&root), ["npm:root@3.0.0"]);
    }

    #[test]
    fn every_lockfile_format_is_read_through_its_precedence_row() {
        let rows: [(&str, &str, Ecosystem); 14] = [
            (
                "npm-shrinkwrap.json",
                include_str!("../../../fixtures/lockfiles/npm/package-lock.json"),
                Ecosystem::Npm,
            ),
            (
                "yarn.lock",
                include_str!("../../../fixtures/lockfiles/yarn/yarn-v1.lock"),
                Ecosystem::Npm,
            ),
            (
                "bun.lock",
                include_str!("../../../fixtures/lockfiles/bun/bun.lock"),
                Ecosystem::Npm,
            ),
            (
                "uv.lock",
                include_str!("../../../fixtures/lockfiles/uv/uv.lock"),
                Ecosystem::Pypi,
            ),
            (
                "poetry.lock",
                include_str!("../../../fixtures/lockfiles/poetry/poetry.lock"),
                Ecosystem::Pypi,
            ),
            (
                "pdm.lock",
                include_str!("../../../fixtures/lockfiles/pdm/pdm.lock"),
                Ecosystem::Pypi,
            ),
            (
                "Pipfile.lock",
                include_str!("../../../fixtures/lockfiles/pipfile/Pipfile.lock"),
                Ecosystem::Pypi,
            ),
            (
                "gradle.lockfile",
                include_str!("../../../fixtures/lockfiles/gradle/gradle.lockfile"),
                Ecosystem::Maven,
            ),
            (
                "build.gradle.kts",
                include_str!("../../../fixtures/lockfiles/gradle/build.gradle.kts"),
                Ecosystem::Maven,
            ),
            (
                "pom.xml",
                include_str!("../../../fixtures/lockfiles/pom/pom.xml"),
                Ecosystem::Maven,
            ),
            (
                "composer.lock",
                include_str!("../../../fixtures/lockfiles/composer/composer.lock"),
                Ecosystem::Packagist,
            ),
            (
                "Gemfile.lock",
                include_str!("../../../fixtures/lockfiles/gemfile/Gemfile.lock"),
                Ecosystem::Rubygems,
            ),
            (
                "pubspec.lock",
                include_str!("../../../fixtures/lockfiles/pubspec/pubspec.lock"),
                Ecosystem::Pub,
            ),
            (
                "Package.resolved",
                include_str!("../../../fixtures/lockfiles/swift/Package.resolved"),
                Ecosystem::Github,
            ),
        ];
        for (name, text, ecosystem) in rows {
            let dir = project(&[(name, text)]);
            let project = read_project(dir.path(), &Options::default());
            assert!(
                project
                    .dependencies
                    .iter()
                    .any(|dependency| dependency.ecosystem == ecosystem),
                "{name}: {:?}",
                project.dependencies
            );
            assert_eq!(project.files.first().map(PathBuf::as_path), Some(Path::new(name)));
            let parse_errors = project
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.kind == DiagnosticKind::ParseError);
            assert_eq!(parse_errors.count(), 0, "{name}: {:?}", project.diagnostics);
        }
    }

    #[test]
    fn a_broken_version_file_is_a_diagnostic_and_the_next_file_applies() {
        let dir = project(&[
            ("rust-toolchain.toml", "[toolchain\n"),
            ("rust-toolchain", "1.80.1\n"),
            (
                "build.gradle.kts",
                "java { toolchain { languageVersion.set(JavaLanguageVersion.of(17)) } }\n",
            ),
            ("package.json", "{"),
        ]);
        let project = read_project(dir.path(), &Options::default());
        let line = |version: &str| Requested::Line(version.to_owned());
        let runtimes: Vec<(Runtime, Requested)> = project
            .runtimes
            .iter()
            .map(|runtime| (runtime.runtime, runtime.requested.clone()))
            .collect();
        assert_eq!(
            runtimes,
            [
                (Runtime::Rust, line("1.80")),
                (Runtime::Jdk, line("17")),
                (Runtime::Node, Requested::Latest)
            ]
        );
        let parse_errors: Vec<(String, Option<Cause>)> = project
            .diagnostics
            .iter()
            .map(|diagnostic| (diagnostic.file.display().to_string(), diagnostic.cause))
            .collect();
        assert_eq!(
            parse_errors,
            [
                ("package.json".to_owned(), Some(Cause::Parse)),
                ("rust-toolchain.toml".to_owned(), Some(Cause::Parse))
            ],
            "package.json is parsed three times and reported once"
        );
    }

    #[test]
    fn member_lists_merge_in_file_order_each_member_once() {
        let dir = project(&[
            ("package.json", r#"{"workspaces": ["a"]}"#),
            ("Cargo.toml", "[workspace]\nmembers = [\"c\"]\n"),
            ("pnpm-workspace.yaml", "packages:\n  - a\n  - shared\n"),
            ("go.work", "go 1.22\n\nuse (\n\t./shared\n\t./b\n)\n"),
            ("a/package-lock.json", &lock(&[("from-a", "1.0.0")])),
            ("b/package-lock.json", &lock(&[("from-b", "1.0.0")])),
            ("c/package-lock.json", &lock(&[("from-c", "1.0.0")])),
            ("shared/package-lock.json", &lock(&[("from-shared", "1.0.0")])),
        ]);
        let project = read_project(dir.path(), &Options::default());
        assert_eq!(
            ids(&project),
            [
                "npm:from-a@1.0.0",
                "npm:from-c@1.0.0",
                "npm:from-shared@1.0.0",
                "npm:from-b@1.0.0"
            ]
        );
    }

    #[test]
    fn members_over_the_limit_across_lists_are_dropped_with_one_diagnostic() {
        let first: Vec<String> = (0..40).map(|index| format!("\"p{index:02}\"")).collect();
        let second: Vec<String> = (0..40).map(|index| format!("  - q{index:02}")).collect();
        let package = format!("{{\"workspaces\": [{}]}}", first.join(","));
        let pnpm = format!("packages:\n{}\n", second.join("\n"));
        let dir = project(&[("package.json", &package), ("pnpm-workspace.yaml", &pnpm)]);
        for index in 0..40 {
            fs::create_dir(dir.path().join(format!("p{index:02}"))).unwrap();
            fs::create_dir(dir.path().join(format!("q{index:02}"))).unwrap();
        }
        let project = read_project(dir.path(), &Options::default());
        assert_eq!(
            kinds(&project),
            [("pnpm-workspace.yaml".to_owned(), DiagnosticKind::SkippedFile, None)]
        );
        assert_eq!(project.diagnostics[0].cause, Some(Cause::TooManyMembers));
    }

    #[test]
    fn the_file_limit_is_reported_once() {
        let members: Vec<String> = (0..64).map(|index| format!("\"m{index:02}\"")).collect();
        let mut files = vec![
            (
                "package.json".to_owned(),
                format!("{{\"workspaces\": [{}]}}", members.join(",")),
            ),
            ("Cargo.toml".to_owned(), "[package]\nname = \"x\"\n".to_owned()),
        ];
        files.extend((0..64).map(|index| (format!("m{index:02}/package.json"), "{}".to_owned())));
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let dir = project(&files);
        let project = read_project(dir.path(), &Options::default());
        assert_eq!(project.files.len(), 64);
        assert_eq!(
            kinds(&project),
            [("m62/package.json".to_owned(), DiagnosticKind::SkippedFile, None)]
        );
        assert_eq!(project.diagnostics[0].cause, Some(Cause::FileLimit));
    }

    #[test]
    fn a_missing_requirements_include_is_ignored() {
        let dir = project(&[("requirements.txt", "-r missing.txt\nsix==1.16.0\n")]);
        let project = read_project(dir.path(), &Options::default());
        assert_eq!(ids(&project), ["pypi:six@1.16.0"]);
        assert!(project.diagnostics.is_empty(), "{:?}", project.diagnostics);
    }

    #[cfg(unix)]
    #[test]
    fn refused_members_symlinked_and_broken_lockfiles_leave_the_rest_readable() {
        let dir = project(&[
            ("package.json", r#"{"workspaces": ["../out"]}"#),
            (
                "pnpm-lock.yaml",
                include_str!("../../../fixtures/lockfiles/pnpm/pnpm-lock.yaml"),
            ),
            ("Cargo.lock", "version = 4\n[[package]\n"),
        ]);
        std::os::unix::fs::symlink("/dev/null", dir.path().join("go.mod")).unwrap();
        let project = read_project(dir.path(), &Options::default());
        assert!(
            project
                .dependencies
                .iter()
                .any(|dependency| dependency.ecosystem == Ecosystem::Npm)
        );
        let causes: Vec<(String, Option<Cause>)> = project
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.cause.is_some())
            .map(|diagnostic| (diagnostic.file.display().to_string(), diagnostic.cause))
            .collect();
        assert_eq!(
            causes,
            [
                ("package.json".to_owned(), Some(Cause::PathEscapesRoot)),
                ("go.mod".to_owned(), Some(Cause::NotRegularFile)),
                ("Cargo.lock".to_owned(), Some(Cause::Parse)),
            ]
        );
    }
}
