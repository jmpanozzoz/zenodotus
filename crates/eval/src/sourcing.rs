//! The M6 question set: questions sourced from Stack Overflow through the public Stack Exchange API, independent of
//! any system's output.
//!
//! `fetch` downloads, per library of `eval/v2/libraries.toml`, the highest-voted questions with an accepted answer
//! that carry the library's tags, and those accepted answers, and saves the raw API responses. `build` turns the
//! saved responses into questions, deterministically and offline:
//!
//! - a question counts only when its accepted answer links to one of the library's documentation hosts;
//! - its query is the question title;
//! - a returned section is a hit when it contains at least one identifier that the accepted answer writes as inline
//!   code (the `any_of` group), with the same rule for every system;
//! - it is version-specific when the answer or a linked documentation URL names a version, and that version is the
//!   one asked for.
//!
//! The content comes from Stack Overflow under CC BY-SA; every question keeps its link and author for attribution.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dom_query::Document;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use zenodotus_core::time::Clock;

use crate::error::EvalError;
use crate::questions::{Kind, Question, Set, Tag, set_of};

/// The most questions kept per library.
pub const MAX_PER_LIBRARY: usize = 15;
/// The most identifiers kept in a question's `any_of` group.
pub const MAX_IDENTIFIERS: usize = 5;

/// One library of the benchmark.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibrarySpec {
    /// The canonical id, such as `npm:react`.
    pub id: String,
    /// The Stack Overflow tags a question must all carry.
    pub tags: Vec<String>,
    /// Documentation host and path prefixes, without a scheme, such as `react.dev/`.
    pub docs: Vec<String>,
}

/// `eval/v2/libraries.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryList {
    /// The libraries.
    pub library: Vec<LibrarySpec>,
}

/// A question as the Stack Exchange API returns it, with the fields used here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiQuestion {
    /// The question id.
    pub question_id: u64,
    /// The title, HTML-escaped.
    pub title: String,
    /// The accepted answer.
    #[serde(default)]
    pub accepted_answer_id: Option<u64>,
    /// The question's score.
    pub score: i64,
    /// The question URL.
    pub link: String,
    /// The author.
    #[serde(default)]
    pub owner: Option<ApiOwner>,
}

/// An answer as the Stack Exchange API returns it with the `withbody` filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiAnswer {
    /// The answer id.
    pub answer_id: u64,
    /// The answer body, as HTML.
    pub body: String,
    /// The author.
    #[serde(default)]
    pub owner: Option<ApiOwner>,
}

/// An answer's author.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiOwner {
    /// The display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// The profile URL.
    #[serde(default)]
    pub link: Option<String>,
}

/// The saved API responses of one library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawLibrary {
    /// The canonical id.
    pub library: String,
    /// When the responses were fetched, as RFC 3339.
    pub fetched_at: String,
    /// The questions, in the API's order (highest score first).
    pub questions: Vec<ApiQuestion>,
    /// The accepted answers of those questions.
    pub answers: Vec<ApiAnswer>,
}

/// Parses `eval/v2/libraries.toml`.
///
/// # Errors
///
/// [`EvalError::Parse`] when the text is not a valid library list.
pub fn parse_libraries(path: &Path, text: &str) -> Result<LibraryList, EvalError> {
    toml::from_str(text).map_err(|error: toml::de::Error| EvalError::Parse {
        path: path.to_path_buf(),
        line: 0,
        message: error.message().to_owned(),
    })
}

/// The characters an identifier may hold besides letters and digits.
const IDENTIFIER_PUNCTUATION: &str = "_.:#$@->/";

/// The identifiers an answer writes as inline code (not inside `pre`), in order of first appearance, without
/// duplicates. An identifier, once any trailing `()` is removed, is 4 to 60 characters long, starts with a letter, `_`,
/// `$` or `@`, is made of letters, digits and `_ . : # $ @ - > /`, and looks like code rather than a word: it holds one
/// of those punctuation characters, a digit, or a lowercase letter followed by an uppercase one (`useState`). Plain
/// words such as `null` or `render` match too much prose to judge a hit.
#[must_use]
pub fn identifiers(html: &str) -> Vec<String> {
    let document = Document::from(html);
    document.select("pre").remove();
    let mut found: Vec<String> = Vec::new();
    for code in &document.select("code") {
        let text = code.text();
        let candidate = text.trim().trim_end_matches("()");
        if is_identifier(candidate) && !found.iter().any(|known| known == candidate) {
            found.push(candidate.to_owned());
        }
    }
    found
}

/// Whether `text` is an identifier as [`identifiers`] describes it.
fn is_identifier(text: &str) -> bool {
    let length_fits = (4..=60).contains(&text.chars().count());
    let starts_well = text
        .chars()
        .next()
        .is_some_and(|first| first.is_alphabetic() || "_$@".contains(first));
    let chars_fit = text
        .chars()
        .all(|c| c.is_alphanumeric() || IDENTIFIER_PUNCTUATION.contains(c));
    let looks_like_code = text
        .chars()
        .any(|c| c.is_ascii_digit() || IDENTIFIER_PUNCTUATION.contains(c))
        || text
            .chars()
            .zip(text.chars().skip(1))
            .any(|(a, b)| a.is_lowercase() && b.is_uppercase());
    length_fits && starts_well && chars_fit && looks_like_code && !text.contains("//")
}

/// The links of an answer that point under one of `docs`, compared without the scheme and a leading `www.`; a link to a
/// bare host counts as a link to its `/`.
#[must_use]
pub fn doc_links(html: &str, docs: &[String]) -> Vec<String> {
    let document = Document::from(html);
    let mut found = Vec::new();
    for anchor in &document.select("a[href]") {
        let href = anchor.attr("href").unwrap_or_default().trim().to_owned();
        let Some(rest) = href.strip_prefix("https://").or_else(|| href.strip_prefix("http://")) else {
            continue;
        };
        let rest = rest.strip_prefix("www.").unwrap_or(rest);
        let rest = if rest.contains('/') {
            rest.to_owned()
        } else {
            format!("{rest}/")
        };
        if docs.iter().any(|prefix| rest.starts_with(prefix.as_str())) && !found.contains(&href) {
            found.push(href);
        }
    }
    found
}

/// The version a question is about: the first dotted number in a documentation link's path, such as `4.2` in
/// `docs.djangoproject.com/en/4.2/`, or else the first version `text` names: a number after `version`, a dotted number
/// after `since` (so that `since 2019` is not a version), or a lowercase `v` glued to a number, such as `v18`. Only
/// the major and minor parts are kept.
#[must_use]
pub fn version_of(text: &str, links: &[String]) -> Option<String> {
    let from_links = links.iter().find_map(|link| {
        let path = link.split_once("://").map_or(link.as_str(), |(_, rest)| rest);
        path.split('/')
            .skip(1)
            .map(|segment| segment.strip_prefix('v').unwrap_or(segment))
            .find(|segment| dotted(segment))
            .map(major_minor)
    });
    from_links.or_else(|| {
        let words: Vec<&str> = text
            .split_whitespace()
            .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric()))
            .collect();
        words.iter().enumerate().find_map(|(index, word)| {
            let next = words.get(index + 1).copied().unwrap_or_default();
            let found = match word.to_lowercase().as_str() {
                "version" if numeric(next) || dotted(next) => Some(next),
                "since" if dotted(next) => Some(next),
                _ => word.strip_prefix('v').filter(|rest| numeric(rest) || dotted(rest)),
            };
            found.map(major_minor)
        })
    })
}

/// Whether `text` is a number of digits only.
fn numeric(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
}

/// Whether `text` is two or more numbers joined by dots, such as `4.2` or `2.0.3`.
fn dotted(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() >= 2 && parts.iter().all(|part| numeric(part))
}

/// The major and minor parts of a version: `2.0.3` becomes `2.0`, `18` stays `18`.
fn major_minor(version: &str) -> String {
    version.split('.').take(2).collect::<Vec<_>>().join(".")
}

/// `text` with its HTML character references decoded, as the Stack Exchange API escapes titles and names.
fn unescape(text: &str) -> String {
    Document::from(format!("<p>{text}</p>").as_str())
        .select("p")
        .text()
        .to_string()
}

/// The display name of `owner`, decoded, or `an unknown author`.
fn author(owner: Option<&ApiOwner>) -> String {
    owner
        .and_then(|owner| owner.display_name.as_deref())
        .map_or_else(|| "an unknown author".to_owned(), unescape)
}

/// The questions of one library, at most [`MAX_PER_LIBRARY`], in the API's order: those whose accepted answer links to
/// the library's documentation and names at least one identifier.
#[must_use]
pub fn build(spec: &LibrarySpec, raw: &RawLibrary) -> Vec<Question> {
    raw.questions
        .iter()
        .filter_map(|question| {
            let accepted = question.accepted_answer_id?;
            let answer = raw.answers.iter().find(|answer| answer.answer_id == accepted)?;
            let links = doc_links(&answer.body, &spec.docs);
            let mut names = identifiers(&answer.body);
            let first_link = links.first()?;
            if names.is_empty() {
                return None;
            }
            names.truncate(MAX_IDENTIFIERS);
            let title = unescape(&question.title);
            let answer_text = Document::from(answer.body.as_str()).select("body").text().to_string();
            let version = version_of(&format!("{title} {answer_text}"), &links);
            let asker = author(question.owner.as_ref());
            let answerer = author(answer.owner.as_ref());
            let mut tags = Vec::new();
            if version.is_some() {
                tags.push(Tag::VersionSpecific);
            }
            tags.push(Tag::NaturalLanguage);
            Some(Question {
                id: format!("so-{}", question.question_id),
                library: spec.id.clone(),
                version,
                query: title,
                kind: Kind::Howto,
                tags,
                must_contain: Vec::new(),
                any_of: vec![names],
                target: format!(
                    "{} (question by {asker}, answer by {answerer}, CC BY-SA) -> {first_link}",
                    question.link
                ),
            })
        })
        .take(MAX_PER_LIBRARY)
        .collect()
}

/// The public Stack Exchange API, keyless (300 requests per IP and day).
#[derive(Debug, Clone)]
pub struct StackExchange {
    /// The API root, such as `https://api.stackexchange.com`.
    base: String,
    /// The HTTP client.
    client: reqwest::Client,
    /// How long one second of a `backoff` the API asks for lasts; shorter in tests.
    second: Duration,
}

/// One response of the Stack Exchange API.
#[derive(Debug, Deserialize)]
struct Page<T> {
    /// The results.
    items: Vec<T>,
    /// Whether another page follows.
    #[serde(default)]
    has_more: bool,
    /// Seconds to wait before the next request to the same method.
    #[serde(default)]
    backoff: Option<u64>,
}

/// An error response of the Stack Exchange API.
#[derive(Debug, Deserialize)]
struct ApiError {
    /// The error's name, such as `throttle_violation`.
    error_name: String,
    /// What went wrong.
    error_message: String,
}

impl StackExchange {
    /// The API at `base`.
    #[must_use]
    pub fn new(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            client: reqwest::Client::new(),
            second: Duration::from_secs(1),
        }
    }

    /// The questions of `spec` with an accepted answer, highest score first, `pages` pages of 100, and their accepted
    /// answers, as of `clock`.
    ///
    /// # Errors
    ///
    /// [`EvalError::StackExchange`] when a request fails or the API answers with an error.
    pub async fn fetch(&self, spec: &LibrarySpec, pages: u32, clock: &dyn Clock) -> Result<RawLibrary, EvalError> {
        let tagged = spec.tags.join(";");
        let mut questions = Vec::new();
        for number in 1..=pages {
            let number = number.to_string();
            let query = [
                ("site", "stackoverflow"),
                ("accepted", "True"),
                ("tagged", tagged.as_str()),
                ("sort", "votes"),
                ("order", "desc"),
                ("pagesize", "100"),
                ("page", number.as_str()),
            ];
            let page: Page<ApiQuestion> = self.get("search/advanced", &query).await?;
            questions.extend(page.items);
            if !page.has_more {
                break;
            }
        }
        let ids: Vec<String> = questions
            .iter()
            .filter_map(|question| question.accepted_answer_id)
            .map(|id| id.to_string())
            .collect();
        let mut answers = Vec::new();
        for chunk in ids.chunks(100) {
            let query = [("site", "stackoverflow"), ("filter", "withbody"), ("pagesize", "100")];
            let page: Page<ApiAnswer> = self.get(&format!("answers/{}", chunk.join(";")), &query).await?;
            answers.extend(page.items);
        }
        Ok(RawLibrary {
            library: spec.id.clone(),
            fetched_at: clock.now().to_string(),
            questions,
            answers,
        })
    }

    /// One request to `method`, waiting afterwards as long as the API's `backoff` asks.
    async fn get<T: DeserializeOwned>(&self, method: &str, query: &[(&str, &str)]) -> Result<Page<T>, EvalError> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(query)
            .finish();
        let response = self
            .client
            .get(format!("{}/2.3/{method}?{query}", self.base))
            .send()
            .await
            .map_err(request_failed)?;
        let status = response.status();
        let text = response.text().await.map_err(request_failed)?;
        if !status.is_success() {
            return Err(EvalError::StackExchange(
                match serde_json::from_str::<ApiError>(&text) {
                    Ok(error) => format!("{}: {}", error.error_name, error.error_message),
                    Err(_) => format!("HTTP {}: {}", status.as_u16(), text.trim()),
                },
            ));
        }
        let page: Page<T> = serde_json::from_str(&text).map_err(|error| EvalError::StackExchange(error.to_string()))?;
        if let Some(seconds) = page.backoff {
            tokio::time::sleep(self.second.saturating_mul(u32::try_from(seconds).unwrap_or(u32::MAX))).await;
        }
        Ok(page)
    }
}

/// A failed request as an [`EvalError::StackExchange`].
#[expect(clippy::needless_pass_by_value, reason = "it is the argument of `map_err`")]
fn request_failed(error: reqwest::Error) -> EvalError {
    EvalError::StackExchange(error.to_string())
}

/// Where the raw responses of library `id` are saved under `dir`.
#[must_use]
pub fn raw_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{}.json", id.replace([':', '/'], "_")))
}

/// Reads and parses the library list at `path`.
fn read_libraries(path: &Path) -> Result<LibraryList, EvalError> {
    let text = std::fs::read_to_string(path).map_err(EvalError::io(path))?;
    parse_libraries(path, &text)
}

/// Fetches every library of `libraries` (or of `only`, when not empty) that has no raw file in `raw` yet, and saves its
/// responses; returns one line per library.
///
/// # Errors
///
/// Every error of reading the list, fetching and saving; the libraries fetched before an error stay saved.
pub async fn fetch_all(
    api: &StackExchange,
    libraries: &Path,
    raw: &Path,
    only: &[String],
    pages: u32,
    clock: &dyn Clock,
) -> Result<String, EvalError> {
    let list = read_libraries(libraries)?;
    std::fs::create_dir_all(raw).map_err(EvalError::io(raw))?;
    let mut text = String::new();
    for spec in list
        .library
        .iter()
        .filter(|spec| only.is_empty() || only.contains(&spec.id))
    {
        let file = raw_path(raw, &spec.id);
        if file.exists() {
            let _ = writeln!(text, "{}: already fetched", spec.id);
            continue;
        }
        let fetched = api.fetch(spec, pages, clock).await?;
        let json = serde_json::to_string_pretty(&fetched).unwrap_or_default() + "\n";
        std::fs::write(&file, json).map_err(EvalError::io(&file))?;
        let _ = writeln!(
            text,
            "{}: {} questions, {} answers",
            spec.id,
            fetched.questions.len(),
            fetched.answers.len()
        );
    }
    Ok(text)
}

/// Builds the question sets from the raw responses of every library of `libraries`, writes `dev.jsonl` and
/// `test.jsonl` into `out`, split by [`set_of`], and returns a summary. A Stack Overflow question that two libraries
/// share is kept once, under the first library.
///
/// # Errors
///
/// Every error of reading the list and the raw files and of writing the sets.
pub fn build_all(libraries: &Path, raw: &Path, out: &Path) -> Result<String, EvalError> {
    let list = read_libraries(libraries)?;
    let mut seen = BTreeSet::new();
    let (mut dev_lines, mut test_lines, mut summary) = (String::new(), String::new(), String::new());
    let (mut total, mut versioned, mut covered) = (0_usize, 0_usize, 0_usize);
    for spec in &list.library {
        let file = raw_path(raw, &spec.id);
        let body = std::fs::read_to_string(&file).map_err(EvalError::io(&file))?;
        let fetched: RawLibrary = serde_json::from_str(&body).map_err(|error| EvalError::Parse {
            path: file.clone(),
            line: error.line(),
            message: error.to_string(),
        })?;
        let kept: Vec<Question> = build(spec, &fetched)
            .into_iter()
            .filter(|question| seen.insert(question.id.clone()))
            .collect();
        let _ = writeln!(
            summary,
            "{}: {} of {} questions kept",
            spec.id,
            kept.len(),
            fetched.questions.len()
        );
        covered += usize::from(!kept.is_empty());
        for question in kept {
            total += 1;
            versioned += usize::from(question.version.is_some());
            let file = if set_of(&question.id) == Set::Dev {
                &mut dev_lines
            } else {
                &mut test_lines
            };
            file.push_str(&serde_json::to_string(&question).unwrap_or_default());
            file.push('\n');
        }
    }
    std::fs::create_dir_all(out).map_err(EvalError::io(out))?;
    for (name, lines) in [("dev.jsonl", &dev_lines), ("test.jsonl", &test_lines)] {
        let path = out.join(name);
        std::fs::write(&path, lines).map_err(EvalError::io(&path))?;
    }
    let _ = writeln!(
        summary,
        "{total} questions (dev {}, test {}), {versioned} version-specific ({}%), {covered} of {} libraries with a question",
        dev_lines.lines().count(),
        test_lines.lines().count(),
        versioned * 100 / total.max(1),
        list.library.len()
    );
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zenodotus_core::time::{FixedClock, Timestamp};

    use super::*;

    fn spec() -> LibrarySpec {
        LibrarySpec {
            id: "pypi:django".to_owned(),
            tags: vec!["django".to_owned()],
            docs: vec!["docs.djangoproject.com/".to_owned()],
        }
    }

    fn question(id: u64, title: &str, accepted: Option<u64>) -> ApiQuestion {
        ApiQuestion {
            question_id: id,
            title: title.to_owned(),
            accepted_answer_id: accepted,
            score: 10,
            link: format!("https://stackoverflow.com/questions/{id}"),
            owner: None,
        }
    }

    fn answer(id: u64, body: &str) -> ApiAnswer {
        ApiAnswer {
            answer_id: id,
            body: body.to_owned(),
            owner: Some(ApiOwner {
                display_name: Some("Ana".to_owned()),
                link: Some("https://stackoverflow.com/users/1/ana".to_owned()),
            }),
        }
    }

    #[test]
    fn the_library_list_parses_and_rejects_unknown_keys() {
        let path = Path::new("libraries.toml");
        let list = parse_libraries(
            path,
            "[[library]]\nid = \"npm:react\"\ntags = [\"reactjs\"]\ndocs = [\"react.dev/\"]\n",
        )
        .unwrap();
        assert_eq!(list.library[0].id, "npm:react");
        assert!(matches!(
            parse_libraries(path, "[[library]]\nid = \"x\"\ntags = []\ndocs = []\nextra = 1\n"),
            Err(EvalError::Parse { .. })
        ));
    }

    #[test]
    fn identifiers_come_from_inline_code_only() {
        let html = "<p>Use <code>select_related()</code> or <code>prefetch_related</code>, not <code>null</code> \
                    or <code>a b</code> or <code>x</code>; again <code>select_related</code>.</p>\
                    <pre><code>Model.objects.all()</code></pre><p><code>QuerySet.values_list</code> \
                    <code>@login_required</code> <code>9lives</code> <code>&lt;div&gt;</code></p>";
        assert_eq!(
            identifiers(html),
            [
                "select_related",
                "prefetch_related",
                "QuerySet.values_list",
                "@login_required"
            ]
        );
    }

    #[test]
    fn doc_links_match_hosts_without_scheme_or_www() {
        let docs = vec!["docs.djangoproject.com/".to_owned(), "react.dev/".to_owned()];
        let html = "<a href=\"https://docs.djangoproject.com/en/4.2/ref/models/\">ref</a> \
                    <a href=\"http://www.react.dev/learn\">learn</a> \
                    <a href=\"https://example.com/docs.djangoproject.com/\">no</a> <a>none</a> \
                    <a href=\"ftp://react.dev/\">ftp</a> <a href=\"https://react.dev\">root</a> \
                    <a href=\"https://react.dev\">again</a>";
        assert_eq!(
            doc_links(html, &docs),
            [
                "https://docs.djangoproject.com/en/4.2/ref/models/",
                "http://www.react.dev/learn",
                "https://react.dev"
            ]
        );
    }

    #[test]
    fn versions_come_from_links_first_then_text() {
        let link = vec!["https://docs.djangoproject.com/en/4.2.1/ref/".to_owned()];
        assert_eq!(version_of("since version 3.0", &link).as_deref(), Some("4.2"));
        assert_eq!(
            version_of("This changed in version 2.0.3.", &[]).as_deref(),
            Some("2.0")
        );
        assert_eq!(version_of("React v18 added it", &[]).as_deref(), Some("18"));
        assert_eq!(version_of("Since 1.5 you can", &[]).as_deref(), Some("1.5"));
        assert_eq!(version_of("no number here, v. none", &[]), None);
        let plain = vec!["https://docs.djangoproject.com/en/stable/ref/".to_owned()];
        assert_eq!(version_of("plain text", &plain), None);
    }

    #[test]
    fn build_keeps_answers_that_link_to_the_docs_and_name_identifiers() {
        let raw = RawLibrary {
            library: "pypi:django".to_owned(),
            fetched_at: "2026-11-21T00:00:00Z".to_owned(),
            questions: vec![
                question(1, "How to use &quot;select_related&quot;?", Some(11)),
                question(2, "No accepted answer", None),
                question(3, "Answer without a docs link", Some(33)),
                question(4, "Docs link but no identifier", Some(44)),
                question(5, "Accepted answer missing", Some(55)),
                question(6, "Version 4.2 question", Some(66)),
            ],
            answers: vec![
                answer(
                    11,
                    "<p>See <a href=\"https://docs.djangoproject.com/en/stable/ref/\">docs</a>: \
                     <code>select_related</code></p>",
                ),
                answer(33, "<p><code>select_related</code></p>"),
                answer(44, "<p><a href=\"https://docs.djangoproject.com/\">docs</a></p>"),
                answer(
                    66,
                    "<p><a href=\"https://docs.djangoproject.com/en/4.2/x/\">x</a> <code>QuerySet.alias</code></p>",
                ),
            ],
        };
        let built = build(&spec(), &raw);
        assert_eq!(built.len(), 2);
        let first = &built[0];
        assert_eq!(first.id, "so-1");
        assert_eq!(first.library, "pypi:django");
        assert_eq!(first.query, "How to use \"select_related\"?");
        assert_eq!(first.version, None);
        assert_eq!(first.kind, Kind::Howto);
        assert_eq!(first.tags, [Tag::NaturalLanguage]);
        assert_eq!(first.any_of, [vec!["select_related".to_owned()]]);
        assert_eq!(first.must_contain, Vec::<String>::new());
        assert_eq!(
            first.target,
            "https://stackoverflow.com/questions/1 (question by an unknown author, answer by Ana, CC BY-SA) -> \
             https://docs.djangoproject.com/en/stable/ref/"
        );
        assert_eq!(built[1].version.as_deref(), Some("4.2"));
        assert_eq!(built[1].tags, [Tag::VersionSpecific, Tag::NaturalLanguage]);
    }

    #[test]
    fn build_caps_questions_and_identifiers() {
        let body = "<a href=\"https://docs.djangoproject.com/a\">a</a> <code>one_1</code> <code>two_2</code> \
                    <code>three_3</code> <code>four_4</code> <code>five_5</code> <code>six_6</code>";
        let questions: Vec<ApiQuestion> = (1..=20).map(|id| question(id, "t", Some(100 + id))).collect();
        let answers: Vec<ApiAnswer> = (1..=20).map(|id| answer(100 + id, body)).collect();
        let raw = RawLibrary {
            library: "pypi:django".to_owned(),
            fetched_at: String::new(),
            questions,
            answers,
        };
        let built = build(&spec(), &raw);
        assert_eq!(built.len(), MAX_PER_LIBRARY);
        assert_eq!(built[0].any_of[0].len(), MAX_IDENTIFIERS);
        let anonymous = RawLibrary {
            answers: vec![ApiAnswer {
                owner: None,
                ..answer(101, body)
            }],
            ..raw
        };
        assert!(
            build(&spec(), &anonymous)[0]
                .target
                .contains("(question by an unknown author, answer by an unknown author, CC BY-SA)")
        );
    }

    #[test]
    fn identifier_edge_cases() {
        let ids: BTreeSet<String> = identifiers("<code>tokio::spawn()</code><code>a.b</code><code>$scope</code>")
            .into_iter()
            .collect();
        assert_eq!(ids, BTreeSet::from(["$scope".to_owned(), "tokio::spawn".to_owned()]));
    }

    fn library_file(dir: &Path) -> PathBuf {
        let path = dir.join("libraries.toml");
        std::fs::write(
            &path,
            "[[library]]\nid = \"pypi:django\"\ntags = [\"django\"]\ndocs = [\"docs.djangoproject.com/\"]\n\n\
             [[library]]\nid = \"npm:@scope/pkg\"\ntags = [\"a\", \"b\"]\ndocs = [\"pkg.dev/\"]\n",
        )
        .unwrap();
        path
    }

    fn api(server: &MockServer) -> StackExchange {
        StackExchange {
            second: Duration::from_millis(1),
            ..StackExchange::new(&format!("{}/", server.uri()))
        }
    }

    fn page(items: &serde_json::Value, has_more: bool, backoff: Option<u64>) -> ResponseTemplate {
        let mut body = serde_json::json!({"items": items, "has_more": has_more, "quota_remaining": 250});
        if let Some(seconds) = backoff {
            body["backoff"] = seconds.into();
        }
        ResponseTemplate::new(200).set_body_json(body)
    }

    fn api_question(id: u64, accepted: Option<u64>) -> serde_json::Value {
        let mut question = serde_json::json!({"question_id": id, "title": format!("Q{id} &amp; more"), "score": 5,
            "link": format!("https://stackoverflow.com/q/{id}"), "tags": ["django"],
            "owner": {"display_name": "Q&amp;A"}});
        if let Some(answer) = accepted {
            question["accepted_answer_id"] = answer.into();
        }
        question
    }

    fn api_answer(id: u64) -> serde_json::Value {
        serde_json::json!({"answer_id": id, "body": format!("<a href=\"https://docs.djangoproject.com/en/5.1/\">d</a> \
            <code>name_{id}</code>"), "owner": {"display_name": "B&#39;o", "link": "https://stackoverflow.com/u/2"}})
    }

    #[tokio::test]
    async fn fetch_pages_through_questions_then_their_accepted_answers() {
        let server = MockServer::start().await;
        let ids: Vec<u64> = (1..=101).collect();
        let first: Vec<serde_json::Value> = ids.iter().map(|&id| api_question(id, Some(1000 + id))).collect();
        Mock::given(method("GET"))
            .and(path("/2.3/search/advanced"))
            .and(query_param("page", "1"))
            .and(query_param("tagged", "a;b"))
            .and(query_param("accepted", "True"))
            .and(query_param("sort", "votes"))
            .and(query_param("site", "stackoverflow"))
            .respond_with(page(&serde_json::json!(first), true, Some(1)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2.3/search/advanced"))
            .and(query_param("page", "2"))
            .respond_with(page(&serde_json::json!([api_question(200, None)]), false, None))
            .mount(&server)
            .await;
        let first_hundred: Vec<String> = (1001..=1100).map(|id| id.to_string()).collect();
        Mock::given(method("GET"))
            .and(path(format!("/2.3/answers/{}", first_hundred.join(";"))))
            .and(query_param("filter", "withbody"))
            .respond_with(page(
                &serde_json::json!((1001..=1100).map(api_answer).collect::<Vec<_>>()),
                false,
                None,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2.3/answers/1101"))
            .respond_with(page(&serde_json::json!([api_answer(1101)]), false, None))
            .mount(&server)
            .await;
        let spec = LibrarySpec {
            id: "npm:@scope/pkg".to_owned(),
            tags: vec!["a".to_owned(), "b".to_owned()],
            docs: Vec::new(),
        };
        let clock = FixedClock::new(Timestamp::from_unix(0));
        let raw = api(&server).fetch(&spec, 3, &clock).await.unwrap();
        assert_eq!(raw.library, "npm:@scope/pkg");
        assert_eq!(raw.fetched_at, "1970-01-01T00:00:00Z");
        assert_eq!(raw.questions.len(), 102);
        assert_eq!(raw.answers.len(), 101);
        assert_eq!(raw.questions[0].title, "Q1 &amp; more");
        let one_page = api(&server).fetch(&spec, 1, &clock).await.unwrap();
        assert_eq!(one_page.questions.len(), 101);
    }

    #[tokio::test]
    async fn api_errors_name_the_error() {
        let server = MockServer::start().await;
        Mock::given(path("/2.3/search/advanced"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error_id": 502, "error_name": "throttle_violation", "error_message": "too many requests"})))
            .mount(&server)
            .await;
        let clock = FixedClock::new(Timestamp::from_unix(0));
        let error = api(&server).fetch(&spec(), 1, &clock).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "the Stack Exchange API failed: throttle_violation: too many requests"
        );
        server.reset().await;
        Mock::given(path("/2.3/search/advanced"))
            .respond_with(ResponseTemplate::new(503).set_body_string("down"))
            .mount(&server)
            .await;
        let error = api(&server).fetch(&spec(), 1, &clock).await.unwrap_err();
        assert_eq!(error.to_string(), "the Stack Exchange API failed: HTTP 503: down");
        server.reset().await;
        Mock::given(path("/2.3/search/advanced"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{"))
            .mount(&server)
            .await;
        let error = api(&server).fetch(&spec(), 1, &clock).await.unwrap_err();
        assert!(
            error.to_string().starts_with("the Stack Exchange API failed: "),
            "{error}"
        );
        let unreachable = StackExchange::new("http://127.0.0.1:9");
        assert!(matches!(
            unreachable.fetch(&spec(), 1, &clock).await,
            Err(EvalError::StackExchange(_))
        ));
    }

    #[test]
    fn raw_files_are_named_after_the_id() {
        assert_eq!(
            raw_path(Path::new("r"), "npm:@scope/pkg"),
            Path::new("r/npm_@scope_pkg.json")
        );
    }

    #[tokio::test]
    async fn fetch_all_saves_each_library_once_and_build_all_splits_the_sets() {
        let server = MockServer::start().await;
        Mock::given(path("/2.3/search/advanced"))
            .respond_with(page(
                &serde_json::json!([api_question(7, Some(70)), api_question(8, Some(80))]),
                false,
                None,
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/2.3/answers/70;80"))
            .respond_with(page(&serde_json::json!([api_answer(70), api_answer(80)]), false, None))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let libraries = library_file(dir.path());
        let raw = dir.path().join("raw");
        let clock = FixedClock::new(Timestamp::from_unix(0));
        let only = vec!["pypi:django".to_owned()];
        let api = api(&server);
        let report = fetch_all(&api, &libraries, &raw, &only, 1, &clock).await.unwrap();
        assert_eq!(report, "pypi:django: 2 questions, 2 answers\n");
        let again = fetch_all(&api, &libraries, &raw, &only, 1, &clock).await.unwrap();
        assert_eq!(again, "pypi:django: already fetched\n");
        let missing = build_all(&libraries, &raw, &dir.path().join("out")).unwrap_err();
        assert!(matches!(missing, EvalError::Io { .. }), "{missing}");
        let other = RawLibrary {
            library: "npm:@scope/pkg".to_owned(),
            fetched_at: String::new(),
            questions: vec![question(7, "shared", Some(70))],
            answers: vec![answer(70, "<a href=\"https://pkg.dev/\">p</a> <code>dup_name</code>")],
        };
        std::fs::write(raw_path(&raw, "npm:@scope/pkg"), serde_json::to_string(&other).unwrap()).unwrap();
        let out = dir.path().join("out");
        let summary = build_all(&libraries, &raw, &out).unwrap();
        let dev = std::fs::read_to_string(out.join("dev.jsonl")).unwrap();
        let test = std::fs::read_to_string(out.join("test.jsonl")).unwrap();
        let mut lines: Vec<Question> = dev
            .lines()
            .chain(test.lines())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        lines.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(lines.len(), 2);
        assert_eq!(
            (lines[0].id.as_str(), lines[0].library.as_str()),
            ("so-7", "pypi:django")
        );
        assert_eq!(lines[0].query, "Q7 & more");
        assert_eq!(lines[0].version.as_deref(), Some("5.1"));
        assert!(
            lines[0].target.contains("(question by Q&A, answer by B'o, CC BY-SA)"),
            "{}",
            lines[0].target
        );
        for question in &lines {
            let file = if set_of(&question.id) == Set::Dev { &dev } else { &test };
            assert!(file.contains(&format!("\"id\":\"{}\"", question.id)));
        }
        assert_eq!(
            summary,
            format!(
                "pypi:django: 2 of 2 questions kept\nnpm:@scope/pkg: 0 of 1 questions kept\n\
                 2 questions (dev {}, test {}), 2 version-specific (100%), 1 of 2 libraries with a question\n",
                dev.lines().count(),
                test.lines().count()
            )
        );
        std::fs::write(raw_path(&raw, "npm:@scope/pkg"), "{").unwrap();
        assert!(matches!(
            build_all(&libraries, &raw, &out),
            Err(EvalError::Parse { line: 1, .. })
        ));
        std::fs::write(&libraries, "nonsense").unwrap();
        assert!(matches!(
            build_all(&libraries, &raw, &out),
            Err(EvalError::Parse { .. })
        ));
        assert!(matches!(
            fetch_all(&api, &dir.path().join("none.toml"), &raw, &[], 1, &clock).await,
            Err(EvalError::Io { .. })
        ));
    }
}
