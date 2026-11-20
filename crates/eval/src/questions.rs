//! The question sets: one JSON object per line, split between dev and test by a hash of the id.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::EvalError;

/// A question whose id hashes to a first byte below this value is in the dev set: 179/256, about 70%.
pub const DEV_BELOW: u8 = 179;

/// A question set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum Set {
    /// The only set used for tuning.
    Dev,
    /// The held-out set, used only for checkpoint and final reports.
    Test,
}

impl Set {
    /// Both sets, dev first.
    pub const ALL: [Self; 2] = [Self::Dev, Self::Test];

    /// The name of the set, which is also the stem of its file.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Test => "test",
        }
    }
}

impl fmt::Display for Set {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What a question asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// An API: a function, type or option.
    Api,
    /// How to do something.
    Howto,
    /// What something is.
    Concept,
    /// A configuration setting.
    Config,
    /// An error message.
    Error,
}

/// A category used for per-category reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tag {
    /// The query names an identifier.
    Symbol,
    /// The answer differs between versions.
    VersionSpecific,
    /// The query quotes text from the docs.
    ExactPhrase,
    /// The answer is a code example.
    CodeExample,
    /// The query is a natural-language question.
    NaturalLanguage,
}

impl Tag {
    /// Every tag, in report order.
    pub const ALL: [Self; 5] = [
        Self::Symbol,
        Self::VersionSpecific,
        Self::ExactPhrase,
        Self::CodeExample,
        Self::NaturalLanguage,
    ];

    /// The name of the tag, as in the question files.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::VersionSpecific => "version_specific",
            Self::ExactPhrase => "exact_phrase",
            Self::CodeExample => "code_example",
            Self::NaturalLanguage => "natural_language",
        }
    }
}

/// One question, with the strings a returned section must contain to count as a hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    /// A unique id such as `fastapi-001`.
    pub id: String,
    /// The library id in the corpus.
    pub library: String,
    /// The version; the library's default version when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The query string every system receives.
    pub query: String,
    /// What the question asks for.
    pub kind: Kind,
    /// Categories for per-category reports.
    #[serde(default)]
    pub tags: Vec<Tag>,
    /// Strings that must all appear in a hit.
    #[serde(default)]
    pub must_contain: Vec<String>,
    /// Groups of strings: a hit contains at least one string of each group.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub any_of: Vec<Vec<String>>,
    /// The document and heading where the answer lives, for people reading the results.
    pub target: String,
}

/// The set that question `id` belongs to.
#[must_use]
pub fn set_of(id: &str) -> Set {
    if Sha256::digest(id.as_bytes())[0] < DEV_BELOW {
        Set::Dev
    } else {
        Set::Test
    }
}

/// The file of `set` under the repository `root`.
#[must_use]
pub fn path(root: &Path, set: Set) -> PathBuf {
    root.join("eval").join("questions").join(format!("{set}.jsonl"))
}

/// The questions of `set` in `text`, the content of `path`.
///
/// # Errors
///
/// [`EvalError::Parse`] for a malformed line or a question with nothing to match, [`EvalError::WrongSet`] for a
/// question whose id hashes to the other set, and [`EvalError::DuplicateId`].
pub fn parse(path: &Path, text: &str, set: Set) -> Result<Vec<Question>, EvalError> {
    let mut questions = Vec::new();
    let mut ids = BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let malformed = |message: String| EvalError::Parse {
            path: path.to_owned(),
            line: index + 1,
            message,
        };
        let question: Question = serde_json::from_str(line).map_err(|error| malformed(error.to_string()))?;
        if question.must_contain.is_empty() && question.any_of.is_empty() {
            return Err(malformed(format!("question {} has nothing to match", question.id)));
        }
        if question.any_of.iter().any(Vec::is_empty) {
            return Err(malformed(format!("question {} has an empty any_of group", question.id)));
        }
        let expected = set_of(&question.id);
        if expected != set {
            return Err(EvalError::WrongSet {
                id: question.id,
                found: set,
                expected,
            });
        }
        if !ids.insert(question.id.clone()) {
            return Err(EvalError::DuplicateId(question.id));
        }
        questions.push(question);
    }
    Ok(questions)
}

/// Reads the questions of `set` under the repository `root`.
///
/// # Errors
///
/// [`EvalError::Io`] when the file cannot be read, and the errors of [`parse`].
pub fn load(root: &Path, set: Set) -> Result<Vec<Question>, EvalError> {
    let path = path(root, set);
    let text = std::fs::read_to_string(&path).map_err(EvalError::io(&path))?;
    parse(&path, &text, set)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A dev question on `library` that needs `must_contain`.
    pub(crate) fn question(id: &str, library: &str, version: Option<&str>, must_contain: &[&str]) -> Question {
        Question {
            id: id.to_owned(),
            library: library.to_owned(),
            version: version.map(str::to_owned),
            query: format!("query of {id}"),
            kind: Kind::Api,
            tags: vec![Tag::Symbol],
            must_contain: must_contain.iter().map(|text| (*text).to_owned()).collect(),
            any_of: Vec::new(),
            target: "guide.md".to_owned(),
        }
    }

    /// The first id `prefix-N` that hashes to `set`.
    pub(crate) fn id_in(set: Set, prefix: &str) -> String {
        (0..1000)
            .map(|n| format!("{prefix}-{n}"))
            .find(|id| set_of(id) == set)
            .expect("both sets are reachable")
    }

    fn line(id: &str) -> String {
        format!(
            r#"{{"id": "{id}", "library": "tokio", "query": "spawn a task", "kind": "howto", "tags": ["symbol", "natural_language"], "must_contain": ["tokio::spawn"], "any_of": [["JoinHandle", "await"]], "target": "tutorial/spawning.md"}}"#
        )
    }

    #[test]
    fn ids_split_by_the_first_byte_of_their_hash() {
        assert_eq!(set_of("fastapi-001"), Set::Dev);
        let ids: Vec<String> = (0..1000).map(|n| format!("q-{n}")).collect();
        let dev = ids.iter().filter(|id| set_of(id) == Set::Dev).count();
        assert!((650..750).contains(&dev), "{dev} of 1000 in dev");
        assert_eq!(Set::ALL.map(Set::name), ["dev", "test"]);
        assert_eq!(Set::Test.to_string(), "test");
        assert_eq!(
            Tag::ALL.map(Tag::name),
            [
                "symbol",
                "version_specific",
                "exact_phrase",
                "code_example",
                "natural_language"
            ]
        );
    }

    #[test]
    fn a_question_file_parses_line_by_line() {
        let dev = id_in(Set::Dev, "tokio");
        let text = format!("{}\n\n{}\n", line(&dev), line(&id_in(Set::Dev, "vite")));
        let questions = parse(Path::new("dev.jsonl"), &text, Set::Dev).unwrap();
        assert_eq!(questions.len(), 2);
        let first = &questions[0];
        assert_eq!(first.id, dev);
        assert_eq!(first.version, None);
        assert_eq!(first.kind, Kind::Howto);
        assert_eq!(first.tags, [Tag::Symbol, Tag::NaturalLanguage]);
        assert_eq!(first.any_of, [vec!["JoinHandle".to_owned(), "await".to_owned()]]);
        assert_eq!(first.target, "tutorial/spawning.md");
    }

    #[test]
    fn malformed_misplaced_and_repeated_questions_are_rejected() {
        let path = Path::new("dev.jsonl");
        let dev = id_in(Set::Dev, "tokio");
        let test = id_in(Set::Test, "tokio");
        let error = parse(path, "{\"id\": 1}\n", Set::Dev).unwrap_err();
        assert!(
            matches!(&error, EvalError::Parse { line: 1, message, .. } if message.contains("invalid type")),
            "{error}"
        );
        let nothing = line(&dev).replace(
            r#""must_contain": ["tokio::spawn"], "any_of": [["JoinHandle", "await"]], "#,
            "",
        );
        let error = parse(path, &format!("\n{nothing}"), Set::Dev).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("dev.jsonl:2: question {dev} has nothing to match")
        );
        let empty_group = line(&dev).replace(r#"[["JoinHandle", "await"]]"#, "[[]]");
        let error = parse(path, &empty_group, Set::Dev).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("dev.jsonl:1: question {dev} has an empty any_of group")
        );
        let error = parse(path, &line(&test), Set::Dev).unwrap_err();
        assert!(
            matches!(&error, EvalError::WrongSet { id, found: Set::Dev, expected: Set::Test } if *id == test),
            "{error}"
        );
        let error = parse(path, &format!("{}\n{}", line(&dev), line(&dev)), Set::Dev).unwrap_err();
        assert!(matches!(&error, EvalError::DuplicateId(id) if *id == dev), "{error}");
        let unknown = line(&dev).replace(r#""target""#, r#""extra": 1, "target""#);
        let error = parse(path, &unknown, Set::Dev).unwrap_err();
        assert!(error.to_string().contains("unknown field `extra`"), "{error}");
    }

    #[test]
    fn question_files_load_from_the_repository() {
        let root = tempfile::TempDir::new().unwrap();
        let error = load(root.path(), Set::Test).unwrap_err();
        assert!(matches!(error, EvalError::Io { .. }), "{error}");
        let file = path(root.path(), Set::Test);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, line(&id_in(Set::Test, "vite"))).unwrap();
        assert_eq!(load(root.path(), Set::Test).unwrap().len(), 1);
    }

    #[test]
    fn the_committed_question_sets_are_well_formed() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let dev = load(&root, Set::Dev).unwrap();
        let test = load(&root, Set::Test).unwrap();
        assert!(dev.len() + test.len() >= 200);
        let ids: BTreeSet<&str> = dev.iter().chain(&test).map(|q| q.id.as_str()).collect();
        assert_eq!(ids.len(), dev.len() + test.len());
        for tag in Tag::ALL {
            assert!(
                dev.iter().any(|q| q.tags.contains(&tag)),
                "no dev question tagged {}",
                tag.name()
            );
        }
    }
}
