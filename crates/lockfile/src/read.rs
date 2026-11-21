//! Reading project files safely (design §8.3): no symlinks, regular files only, the size checked from metadata
//! before any byte is read, and at most [`MAX_FILES`] files per root.

use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use crate::error::LockfileError;

/// The size limit of a JSON lockfile.
pub(crate) const JSON_LIMIT: u64 = 32 << 20;
/// The size limit of every other file.
pub(crate) const OTHER_LIMIT: u64 = 16 << 20;
/// The most files read in one root.
pub(crate) const MAX_FILES: usize = 64;

/// How much of `Package.swift` is read: its first line, at most 1 KiB (design §8.2).
pub(crate) const HEAD_LIMIT: u64 = 1 << 10;

/// JSON lockfiles, which get the larger limit.
const JSON_LOCKFILES: [&str; 7] = [
    "package-lock.json",
    "npm-shrinkwrap.json",
    "bun.lock",
    "Pipfile.lock",
    "packages.lock.json",
    "composer.lock",
    "Package.resolved",
];

/// The size limit for the file at `path`, by its name.
pub(crate) fn limit_for(path: &Path) -> u64 {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    if JSON_LOCKFILES.contains(&name) {
        JSON_LIMIT
    } else {
        OTHER_LIMIT
    }
}

/// Whether two metadata records describe the same regular file.
#[cfg(unix)]
fn same_file(checked: &fs::Metadata, opened: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    opened.is_file() && (checked.dev(), checked.ino()) == (opened.dev(), opened.ino())
}

/// Whether two metadata records describe the same regular file.
#[cfg(not(unix))]
fn same_file(_checked: &fs::Metadata, opened: &fs::Metadata) -> bool {
    opened.is_file()
}

/// Reads files under one root and counts what it reads.
pub(crate) struct Files {
    root: PathBuf,
    count: usize,
    bytes_read: u64,
}

impl Files {
    /// A reader for files under `root`.
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            count: 0,
            bytes_read: 0,
        }
    }

    /// The number of files opened so far.
    pub(crate) fn files_read(&self) -> usize {
        self.count
    }

    /// The number of bytes read so far.
    pub(crate) fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Reads `path`, relative to the root, as UTF-8 text of at most `limit` bytes; `None` when it does not exist.
    pub(crate) fn read(&mut self, path: &Path, limit: u64) -> Result<Option<String>, LockfileError> {
        let Some(bytes) = self.read_bytes(path, limit, true)? else {
            return Ok(None);
        };
        String::from_utf8(bytes).map(Some).map_err(|_| LockfileError::Parse {
            file: path.to_path_buf(),
            line: None,
            message: "not UTF-8".to_owned(),
        })
    }

    /// Reads the first `bytes` bytes of `path`, whatever its size, as text; a character cut at the end is dropped.
    pub(crate) fn read_head(&mut self, path: &Path, bytes: u64) -> Result<Option<String>, LockfileError> {
        let Some(head) = self.read_bytes(path, bytes, false)? else {
            return Ok(None);
        };
        let valid = std::str::from_utf8(&head).map_or_else(|error| error.valid_up_to(), str::len);
        Ok(Some(String::from_utf8_lossy(&head[..valid]).into_owned()))
    }

    /// Opens `path` after the checks of design §8.3 and reads at most `limit` bytes; with `whole`, a file whose
    /// metadata size is over `limit` is refused before any byte is read.
    fn read_bytes(&mut self, path: &Path, limit: u64, whole: bool) -> Result<Option<Vec<u8>>, LockfileError> {
        let full = self.root.join(path);
        let io = |source| LockfileError::Io {
            path: path.to_path_buf(),
            source,
        };
        let not_regular = || LockfileError::NotRegularFile {
            path: path.to_path_buf(),
        };
        // Every directory on the way, then the file itself, is checked without following symlinks.
        let mut on_the_way = self.root.clone();
        let mut last = None;
        for component in path.components() {
            on_the_way.push(component);
            match fs::symlink_metadata(&on_the_way) {
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(io(error)),
                Ok(metadata) if metadata.file_type().is_symlink() => return Err(not_regular()),
                Ok(metadata) => last = Some(metadata),
            }
        }
        // A FIFO is refused here, before `open` could block on it.
        let checked = last.filter(fs::Metadata::is_file).ok_or_else(not_regular)?;
        if whole && checked.len() > limit {
            return Err(LockfileError::TooLarge {
                path: path.to_path_buf(),
                size: checked.len(),
                limit,
            });
        }
        if self.count >= MAX_FILES {
            return Err(LockfileError::FileLimit {
                path: path.to_path_buf(),
                limit: MAX_FILES,
            });
        }
        self.count += 1;
        let file = fs::File::open(&full).map_err(io)?;
        let opened = file.metadata().map_err(io)?;
        // The file opened must be the one checked: a swap between the check and the open is refused.
        same_file(&checked, &opened).then_some(()).ok_or_else(not_regular)?;
        // A file that grew since the check is cut at the limit and then fails to parse.
        let mut bytes = Vec::new();
        file.take(limit).read_to_end(&mut bytes).map_err(io)?;
        self.bytes_read += bytes.len() as u64;
        Ok(Some(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_regular_file_is_read_and_a_missing_one_is_absent() {
        let dir = root();
        fs::write(dir.path().join("Cargo.lock"), "version = 4\n").unwrap();
        let mut files = Files::new(dir.path());
        assert_eq!(
            files.read(Path::new("Cargo.lock"), JSON_LIMIT).unwrap().as_deref(),
            Some("version = 4\n")
        );
        assert_eq!(files.read(Path::new("go.mod"), JSON_LIMIT).unwrap(), None);
        assert_eq!(files.files_read(), 1);
        assert_eq!(files.bytes_read(), 12);
    }

    #[test]
    fn a_file_that_is_not_utf8_is_a_parse_error() {
        let dir = root();
        fs::write(dir.path().join("go.mod"), [0xff, 0xfe]).unwrap();
        let error = Files::new(dir.path())
            .read(Path::new("go.mod"), OTHER_LIMIT)
            .unwrap_err();
        assert!(matches!(error, LockfileError::Parse { ref file, line: None, .. } if file == Path::new("go.mod")));
    }

    #[test]
    fn a_path_under_a_regular_file_is_an_io_error() {
        let dir = root();
        fs::write(dir.path().join("go.mod"), "module x\n").unwrap();
        let error = Files::new(dir.path())
            .read(Path::new("go.mod/inner"), OTHER_LIMIT)
            .unwrap_err();
        assert!(matches!(error, LockfileError::Io { ref path, .. } if path == Path::new("go.mod/inner")));
    }

    #[test]
    fn the_limit_depends_on_the_file_name() {
        for json in [
            "package-lock.json",
            "npm-shrinkwrap.json",
            "bun.lock",
            "Pipfile.lock",
            "packages.lock.json",
            "composer.lock",
            "Package.resolved",
        ] {
            assert_eq!(limit_for(Path::new(json)), JSON_LIMIT, "{json}");
            assert_eq!(limit_for(&Path::new("src/app").join(json)), JSON_LIMIT, "{json}");
        }
        for other in ["Cargo.lock", "pnpm-lock.yaml", "go.mod", "package.json", "pom.xml"] {
            assert_eq!(limit_for(Path::new(other)), OTHER_LIMIT, "{other}");
        }
        assert_eq!(JSON_LIMIT, 32 << 20);
        assert_eq!(OTHER_LIMIT, 16 << 20);
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_are_refused_even_to_regular_files_inside_the_root() {
        let dir = root();
        fs::write(dir.path().join("real.lock"), "version = 4\n").unwrap();
        std::os::unix::fs::symlink("/dev/zero", dir.path().join("package-lock.json")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real.lock"), dir.path().join("Cargo.lock")).unwrap();
        let mut files = Files::new(dir.path());
        for name in ["package-lock.json", "Cargo.lock"] {
            let error = files.read(Path::new(name), JSON_LIMIT).err();
            assert!(
                matches!(error, Some(LockfileError::NotRegularFile { ref path }) if path == Path::new(name)),
                "{name}"
            );
        }
        assert_eq!((files.files_read(), files.bytes_read()), (0, 0));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_on_the_way_is_refused() {
        let dir = root();
        let outside = root();
        fs::write(outside.path().join("libs.versions.toml"), "[versions]\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("gradle")).unwrap();
        let path = Path::new("gradle/libs.versions.toml");
        let mut files = Files::new(dir.path());
        let error = files.read(path, OTHER_LIMIT).unwrap_err();
        assert!(matches!(error, LockfileError::NotRegularFile { path: ref p } if p == path));
        assert_eq!(files.bytes_read(), 0);
    }

    #[test]
    fn a_missing_directory_on_the_way_means_the_file_is_absent() {
        let dir = root();
        assert_eq!(
            Files::new(dir.path())
                .read(Path::new("gradle/libs.versions.toml"), OTHER_LIMIT)
                .unwrap(),
            None
        );
    }
    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let dir = root();
        let status = std::process::Command::new("mkfifo")
            .arg(dir.path().join("yarn.lock"))
            .status()
            .unwrap();
        assert!(status.success());
        let root_path = dir.path().to_path_buf();
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut files = Files::new(&root_path);
            let error = files.read(Path::new("yarn.lock"), OTHER_LIMIT).err();
            send.send((error, files.files_read())).unwrap();
        });
        let (error, files_read) = receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the read blocked");
        assert!(matches!(error, Some(LockfileError::NotRegularFile { ref path }) if path == Path::new("yarn.lock")));
        assert_eq!(files_read, 0);
    }

    #[test]
    fn a_directory_is_not_a_regular_file() {
        let dir = root();
        fs::create_dir(dir.path().join("Cargo.lock")).unwrap();
        let error = Files::new(dir.path()).read(Path::new("Cargo.lock"), OTHER_LIMIT).err();
        assert!(matches!(error, Some(LockfileError::NotRegularFile { ref path }) if path == Path::new("Cargo.lock")));
    }
    #[cfg(unix)]
    #[test]
    fn a_swapped_file_is_not_the_same_file() {
        let dir = root();
        fs::write(dir.path().join("a"), "a").unwrap();
        fs::write(dir.path().join("b"), "b").unwrap();
        let a = fs::metadata(dir.path().join("a")).unwrap();
        let b = fs::metadata(dir.path().join("b")).unwrap();
        assert!(same_file(&a, &a));
        assert!(!same_file(&a, &b));
        assert!(!same_file(&a, &fs::metadata(dir.path()).unwrap()));
    }

    #[test]
    fn the_size_is_checked_from_metadata_before_reading() {
        let dir = root();
        let file = fs::File::create(dir.path().join("package-lock.json")).unwrap();
        file.set_len(33 << 20).unwrap();
        fs::write(dir.path().join("go.mod"), "x".repeat(10)).unwrap();
        let mut files = Files::new(dir.path());
        let error = files.read(Path::new("package-lock.json"), JSON_LIMIT).err();
        assert!(matches!(
            error,
            Some(LockfileError::TooLarge { ref path, size, limit })
                if path == Path::new("package-lock.json") && size == 33 << 20 && limit == JSON_LIMIT
        ));
        assert!(matches!(
            files.read(Path::new("go.mod"), 9).err(),
            Some(LockfileError::TooLarge { size: 10, .. })
        ));
        assert_eq!(
            files.read(Path::new("go.mod"), 10).unwrap().map(|text| text.len()),
            Some(10)
        );
        assert_eq!(files.bytes_read(), 10, "only the file under its limit was read");
    }
    #[test]
    fn a_root_reads_at_most_64_files() {
        let dir = root();
        for index in 0..65 {
            fs::write(dir.path().join(format!("requirements-{index:02}.txt")), "x==1\n").unwrap();
        }
        let mut files = Files::new(dir.path());
        for index in 0..64 {
            assert!(
                files
                    .read(Path::new(&format!("requirements-{index:02}.txt")), OTHER_LIMIT)
                    .unwrap()
                    .is_some()
            );
        }
        let error = files.read(Path::new("requirements-64.txt"), OTHER_LIMIT).err();
        assert!(matches!(
            error,
            Some(LockfileError::FileLimit { ref path, limit: MAX_FILES }) if path == Path::new("requirements-64.txt")
        ));
        assert_eq!(files.files_read(), MAX_FILES);
        assert_eq!(
            files.read(Path::new("absent.txt"), OTHER_LIMIT).unwrap(),
            None,
            "absent files still answer"
        );
    }

    #[test]
    fn hostile_a_2_kib_first_line_of_package_swift_is_cut_at_1_kib() {
        let dir = root();
        let line = format!(
            "// swift-tools-version:5.9 {}\nimport PackageDescription\n",
            "x".repeat(2048)
        );
        fs::write(dir.path().join("Package.swift"), &line).unwrap();
        let mut files = Files::new(dir.path());
        let head = files
            .read_head(Path::new("Package.swift"), HEAD_LIMIT)
            .unwrap()
            .unwrap();
        assert_eq!(head.len(), 1024);
        assert!(head.starts_with("// swift-tools-version:5.9 xxx") && !head.contains('\n'));
        assert_eq!(files.bytes_read(), 1024);
        assert_eq!(files.files_read(), 1);
        // A character cut by the limit is dropped rather than failing the read.
        fs::write(dir.path().join("Package.swift"), format!("{}é", "x".repeat(1023))).unwrap();
        let head = files
            .read_head(Path::new("Package.swift"), HEAD_LIMIT)
            .unwrap()
            .unwrap();
        assert_eq!(head, "x".repeat(1023));
        assert_eq!(files.read_head(Path::new("absent.swift"), HEAD_LIMIT).unwrap(), None);
    }
}
