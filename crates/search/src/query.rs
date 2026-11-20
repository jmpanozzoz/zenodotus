//! Query analysis, query construction, retrieval and the `query_docs` response.

use std::collections::HashSet;

use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, ConstScoreQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{Searcher, Term};
use zenodotus_core::config::RankingSettings;
use zenodotus_core::ids::Scope;
use zenodotus_core::model::{Section, VersionState};

use crate::analyzer::{code_ident, code_words};
use crate::error::SearchError;
use crate::fences::split_code;
use crate::render_model::{Notice, NoticeKind, QueryDocsResult, ResultItem, Signal, SyncStatus, VersionResolution};
use crate::resolve::resolve_version;
use crate::schema::{Fields, KIND_SECTION};
use crate::store::Store;

/// Queries longer than this many characters are refused.
pub const MAX_QUERY_CHARS: usize = 1000;
/// Words and identifiers beyond this many are ignored.
pub const MAX_QUERY_TERMS: usize = 64;
/// The boost of a synonym relative to the word it stands for.
const SYNONYM_BOOST: f32 = 0.5;
/// Slop of the phrase built from all the query's words.
const PHRASE_SLOP: u32 = 2;

/// English function words, sorted. They are optional clauses: a section that matches only these is never a result.
const STOP_WORDS: [&str; 52] = [
    "a", "about", "an", "and", "are", "as", "at", "be", "by", "can", "could", "do", "does", "for", "from", "has",
    "have", "how", "i", "if", "in", "into", "is", "it", "its", "me", "my", "of", "on", "or", "should", "so", "some",
    "that", "the", "their", "then", "there", "these", "this", "to", "was", "we", "what", "when", "where", "which",
    "who", "why", "with", "would", "you",
];

/// Whether `word`, already lowercased, is a stop word.
#[must_use]
pub fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.binary_search(&word).is_ok()
}

/// A query after analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analyzed {
    /// Every word, in order, as `code_words` emits it.
    pub words: Vec<String>,
    /// Distinct words of which enough must match: the non-stop words, or the stop words when there are no others.
    pub required: Vec<String>,
    /// Distinct stop words that only add to the score, when there are non-stop words.
    pub optional: Vec<String>,
    /// Identifier terms, as `code_ident` emits them.
    pub identifiers: Vec<String>,
    /// The words of each "quoted" part with two or more words.
    pub quoted: Vec<Vec<String>>,
    /// Whether `required` holds non-stop words.
    pub has_content_words: bool,
}

fn texts(text: &str) -> Vec<String> {
    code_words(text).into_iter().map(|token| token.text).collect()
}

fn distinct(words: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = Vec::new();
    for word in words {
        if !seen.contains(&word) {
            seen.push(word);
        }
    }
    seen
}

/// Analyses `query`.
///
/// # Errors
///
/// [`SearchError::QueryTooLong`] above [`MAX_QUERY_CHARS`] characters, or [`SearchError::EmptyQuery`] when no
/// word survives analysis.
pub fn analyze(query: &str) -> Result<Analyzed, SearchError> {
    let chars = query.chars().count();
    if chars > MAX_QUERY_CHARS {
        return Err(SearchError::QueryTooLong {
            chars,
            max: MAX_QUERY_CHARS,
        });
    }
    let mut words = texts(query);
    words.truncate(MAX_QUERY_TERMS);
    if words.is_empty() {
        return Err(SearchError::EmptyQuery);
    }
    let (stop, content): (Vec<String>, Vec<String>) = distinct(words.iter().cloned())
        .into_iter()
        .partition(|word| is_stop_word(word));
    let has_content_words = !content.is_empty();
    let (required, optional) = if has_content_words {
        (content, stop)
    } else {
        (stop, Vec::new())
    };
    let mut identifiers = code_ident(query);
    identifiers.truncate(MAX_QUERY_TERMS);
    let quoted = query
        .split('"')
        .skip(1)
        .step_by(2)
        .map(texts)
        .filter(|phrase| phrase.len() >= 2)
        .collect();
    Ok(Analyzed {
        words,
        required,
        optional,
        identifiers,
        quoted,
        has_content_words,
    })
}

fn term_query(field: Field, text: &str, option: IndexRecordOption) -> Box<dyn Query> {
    Box::new(TermQuery::new(Term::from_field_text(field, text), option))
}

fn boosted(query: Box<dyn Query>, boost: f32) -> Box<dyn Query> {
    Box::new(BoostQuery::new(query, boost))
}

/// A phrase over `words` in `combined`. `words` has at least two entries.
fn phrase(fields: &Fields, words: &[String], slop: u32) -> Box<dyn Query> {
    let terms = words
        .iter()
        .map(|word| Term::from_field_text(fields.combined, word))
        .collect();
    let mut query = PhraseQuery::new(terms);
    query.set_slop(slop);
    Box::new(query)
}

/// The clause for one required or optional word: the word itself, or the word or any of its synonyms.
fn word_query(fields: &Fields, word: &str, ranking: &RankingSettings) -> Box<dyn Query> {
    let exact = term_query(fields.combined, word, IndexRecordOption::WithFreqs);
    let synonyms: Vec<Box<dyn Query>> = ranking
        .synonyms
        .iter()
        .filter(|(key, _)| key.to_lowercase() == word)
        .flat_map(|(_, values)| values)
        .map(|synonym| texts(synonym))
        .filter(|words| !words.is_empty())
        .map(|words| match words.as_slice() {
            [one] => term_query(fields.combined, one, IndexRecordOption::WithFreqs),
            _ => phrase(fields, &words, 0),
        })
        .collect();
    if synonyms.is_empty() {
        return exact;
    }
    let clauses = std::iter::once(exact)
        .chain(synonyms.into_iter().map(|query| boosted(query, SYNONYM_BOOST)))
        .map(|query| (Occur::Should, query))
        .collect();
    Box::new(BooleanQuery::new(clauses))
}

/// How many of `count` required words must match.
fn minimum_should_match(fraction: f32, count: usize) -> usize {
    let count_f64 = f64::from(u32::try_from(count).unwrap_or(u32::MAX));
    let wanted = (f64::from(fraction) * count_f64).ceil();
    // `wanted` is between 0 and `count`, so the conversion is exact.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "0 <= wanted <= count"
    )]
    let wanted = wanted as usize;
    wanted.max(1)
}

/// Builds the query for `analyzed` in `scope`, as described in the design's query construction.
#[must_use]
pub fn build(
    fields: &Fields,
    analyzed: &Analyzed,
    scope: &Scope,
    language: Option<&str>,
    ranking: &RankingSettings,
) -> BooleanQuery {
    let basic = IndexRecordOption::Basic;
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![
        (
            Occur::Must,
            Box::new(ConstScoreQuery::new(term_query(fields.kind, KIND_SECTION, basic), 0.0)),
        ),
        (
            Occur::Must,
            Box::new(ConstScoreQuery::new(
                term_query(fields.scope, &scope.to_string(), basic),
                0.0,
            )),
        ),
    ];
    let group: Vec<(Occur, Box<dyn Query>)> = analyzed
        .required
        .iter()
        .map(|word| (Occur::Should, word_query(fields, word, ranking)))
        .collect();
    let minimum = minimum_should_match(ranking.min_should_match, group.len());
    clauses.push((
        Occur::Must,
        Box::new(BooleanQuery::with_minimum_required_clauses(group, minimum)),
    ));
    for word in &analyzed.optional {
        clauses.push((
            Occur::Should,
            term_query(fields.combined, word, IndexRecordOption::WithFreqs),
        ));
    }
    if ranking.symbol_boost > 0.0 {
        for identifier in &analyzed.identifiers {
            let query = term_query(fields.symbols_exact, identifier, IndexRecordOption::WithFreqs);
            clauses.push((Occur::Should, boosted(query, ranking.symbol_boost)));
        }
    }
    if analyzed.words.len() >= 2 && ranking.phrase_boost > 0.0 {
        clauses.push((
            Occur::Should,
            boosted(phrase(fields, &analyzed.words, PHRASE_SLOP), ranking.phrase_boost),
        ));
    }
    if ranking.quoted_phrase_boost > 0.0 {
        for words in &analyzed.quoted {
            clauses.push((
                Occur::Should,
                boosted(phrase(fields, words, 0), ranking.quoted_phrase_boost),
            ));
        }
    }
    if let Some(language) = language.filter(|_| ranking.language_boost > 0.0) {
        let query = term_query(fields.lang, &language.to_lowercase(), basic);
        clauses.push((Occur::Should, boosted(query, ranking.language_boost)));
    }
    BooleanQuery::new(clauses)
}

/// One retrieved section with its score.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// The section; its text may include an appended code continuation.
    pub section: Section,
    /// The relevance score.
    pub score: f32,
}

/// The best `k` sections for `query`, ordered by score, then by chunk id. Ties at the cut are fetched whole first,
/// so the result does not depend on the order in which the index visits documents.
///
/// # Errors
///
/// An index error, or a corrupt payload.
pub fn top_sections(store: &Store, searcher: &Searcher, query: &dyn Query, k: usize) -> Result<Vec<Hit>, SearchError> {
    let k = k.max(1);
    let mut limit = k + 1;
    let found = loop {
        let found = searcher.search(query, &TopDocs::with_limit(limit).order_by_score())?;
        if found.len() < limit || found[limit - 1].0 < found[k - 1].0 {
            break found;
        }
        limit *= 2;
    };
    let cut = found.get(k - 1).map_or(f32::MIN, |&(score, _)| score);
    let mut hits = found
        .into_iter()
        .filter(|&(score, _)| score >= cut)
        .map(|(score, address)| {
            Ok(Hit {
                section: store.payload(searcher, address)?,
                score,
            })
        })
        .collect::<Result<Vec<Hit>, SearchError>>()?;
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.section.chunk_id.cmp(&b.section.chunk_id))
    });
    hits.truncate(k);
    Ok(hits)
}

/// Drops hits whose text was already returned by a better hit.
fn dedup(hits: &mut Vec<Hit>) {
    let mut seen = HashSet::new();
    hits.retain(|hit| seen.insert(hit.section.content_hash.clone()));
}

/// Whether `text` opens with a code fence, after any heading lines.
fn starts_with_code(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .is_some_and(|line| line.starts_with("```") || line.starts_with("~~~"))
}

/// Appends to each hit with less than `max_prose_chars` of prose the next section of its document, when that
/// section starts with code.
fn expand_adjacent(
    store: &Store,
    searcher: &Searcher,
    hits: &mut [Hit],
    max_prose_chars: u64,
) -> Result<(), SearchError> {
    if max_prose_chars == 0 {
        return Ok(());
    }
    for hit in hits {
        let prose = split_code(&hit.section.text).prose.chars().count() as u64;
        if prose >= max_prose_chars {
            continue;
        }
        let siblings = store.sections_of(searcher, &hit.section.doc_key)?;
        let next = siblings.into_iter().find(|s| s.ordinal == hit.section.ordinal + 1);
        if let Some(next) = next.filter(|next| starts_with_code(&next.text)) {
            hit.section.text = format!("{}\n\n{}", hit.section.text.trim_end(), next.text);
        }
    }
    Ok(())
}

/// The characters a hit adds to a response: its text, title, headings and source.
fn cost(hit: &Hit) -> usize {
    let section = &hit.section;
    section.text.chars().count()
        + section.title.chars().count()
        + section.heading_path.iter().map(|h| h.chars().count()).sum::<usize>()
        + section.source_ref.chars().count()
}

/// Drops hits from the tail until the rest costs at most `max_chars`, keeping at least one.
fn fit_budget(hits: &mut Vec<Hit>, max_chars: usize) {
    let mut total: usize = hits.iter().map(cost).sum();
    while hits.len() > 1 && total > max_chars {
        total -= hits.pop().as_ref().map_or(0, cost);
    }
}

/// The words of a section's title, headings, text and symbols.
fn section_words(section: &Section) -> HashSet<String> {
    let mut words: HashSet<String> = texts(&section.title).into_iter().collect();
    words.extend(section.heading_path.iter().flat_map(|h| texts(h)));
    words.extend(texts(&section.text));
    words.extend(section.symbols.iter().flat_map(|s| texts(s)));
    words
}

/// Whether `needle` occurs as a contiguous run in `haystack`.
fn contains_run(haystack: &[String], needle: &[String]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// What `section` has in common with the query.
fn signals(section: &Section, analyzed: &Analyzed, language: Option<&str>) -> Vec<Signal> {
    let any_in = |words: &[String]| analyzed.required.iter().any(|word| words.contains(word));
    let symbols = code_ident(&section.symbols.join(" "));
    let headings: Vec<String> = std::iter::once(&section.title)
        .chain(&section.heading_path)
        .flat_map(|heading| texts(heading))
        .collect();
    let checks = [
        (
            Signal::ExactSymbol,
            analyzed.identifiers.iter().any(|i| symbols.contains(i)),
        ),
        (Signal::Title, any_in(&headings)),
        (Signal::Code, any_in(&texts(&split_code(&section.text).code))),
        (
            Signal::Language,
            language.is_some_and(|l| section.code_languages.iter().any(|c| c.eq_ignore_ascii_case(l))),
        ),
        (
            Signal::Phrase,
            analyzed.words.len() >= 2 && contains_run(&texts(&section.text), &analyzed.words),
        ),
    ];
    checks
        .into_iter()
        .filter(|&(_, on)| on)
        .map(|(signal, _)| signal)
        .collect()
}

/// Whether no hit has an exact symbol or at least half of the query's non-stop words.
fn is_weak(hits: &[Hit], items: &[ResultItem], analyzed: &Analyzed) -> bool {
    let has_symbol = items.iter().any(|item| item.signals.contains(&Signal::ExactSymbol));
    let well_covered = hits.iter().any(|hit| {
        let words = section_words(&hit.section);
        let found = analyzed.required.iter().filter(|word| words.contains(*word)).count();
        found * 2 >= analyzed.required.len()
    });
    analyzed.has_content_words && !has_symbol && !well_covered
}

/// The notice for `hits`, if any.
fn notice(
    hits: &[Hit],
    items: &[ResultItem],
    analyzed: &Analyzed,
    state: &VersionState,
    others: Vec<String>,
) -> Option<Notice> {
    let listed = if others.is_empty() {
        String::new()
    } else {
        format!(" Other indexed versions: {}.", others.join(", "))
    };
    if hits.is_empty() {
        return Some(Notice {
            kind: NoticeKind::NoResults,
            message: format!("No section of {} matched the query.{listed}", state.scope),
            other_versions: others,
        });
    }
    is_weak(hits, items, analyzed).then(|| Notice {
        kind: NoticeKind::WeakMatch,
        message: format!(
            "No result contains an exact symbol or at least half of the query's words, so these may not answer \
             it.{listed}"
        ),
        other_versions: others,
    })
}

/// A `query_docs` request.
#[derive(Debug, Clone, Copy)]
pub struct QueryRequest<'a> {
    /// The library id.
    pub library_id: &'a str,
    /// The question or keywords.
    pub query: &'a str,
    /// The version; the default when absent.
    pub version: Option<&'a str>,
    /// A code language to favour.
    pub language: Option<&'a str>,
    /// How many sections to return at most.
    pub max_results: usize,
    /// The character budget of the response.
    pub max_chars: usize,
}

/// Runs `request` on one snapshot of `store`.
///
/// # Errors
///
/// [`SearchError::QueryTooLong`], [`SearchError::EmptyQuery`], [`SearchError::UnknownLibrary`],
/// [`SearchError::UnknownVersion`], an index error, or a corrupt payload.
pub fn query_docs(
    store: &Store,
    request: &QueryRequest<'_>,
    ranking: &RankingSettings,
) -> Result<QueryDocsResult, SearchError> {
    let analyzed = analyze(request.query)?;
    let searcher = store.searcher();
    let states = store.version_states_at(&searcher)?;
    let (state, resolution) = resolve_version(&states, request.library_id, request.version)?;
    let query = build(store.fields(), &analyzed, &state.scope, request.language, ranking);
    let mut hits = top_sections(store, &searcher, &query, request.max_results)?;
    dedup(&mut hits);
    expand_adjacent(store, &searcher, &mut hits, ranking.adjacent_max_prose_chars)?;
    fit_budget(&mut hits, request.max_chars);
    Ok(respond(state, resolution, &states, &hits, &analyzed, request.language))
}

/// Builds the response for `hits`.
fn respond(
    state: &VersionState,
    resolution: VersionResolution,
    states: &[VersionState],
    hits: &[Hit],
    analyzed: &Analyzed,
    language: Option<&str>,
) -> QueryDocsResult {
    let items: Vec<ResultItem> = hits
        .iter()
        .zip(1..)
        .map(|(hit, rank)| ResultItem {
            rank,
            score: hit.score,
            signals: signals(&hit.section, analyzed, language),
            chunk_id: hit.section.chunk_id.as_str().to_owned(),
            title: hit.section.title.clone(),
            heading_path: hit.section.heading_path.clone(),
            source_ref: hit.section.source_ref.clone(),
            version: state.scope.version.to_string(),
            languages: hit.section.code_languages.clone(),
            text: hit.section.text.clone(),
        })
        .collect();
    let mut others: Vec<&VersionState> = states
        .iter()
        .filter(|other| other.scope.library == state.scope.library && other.scope != state.scope)
        .collect();
    others.sort_by_key(|other| other.list_order);
    let others = others.iter().map(|other| other.scope.version.to_string()).collect();
    QueryDocsResult {
        library_id: state.scope.library.to_string(),
        version: state.scope.version.to_string(),
        version_resolution: resolution,
        sync_status: SyncStatus {
            revision: state.revision,
            last_success: state.last_success.map(|at| at.to_string()),
            outcome: state.outcome.as_str().to_owned(),
        },
        notice: notice(hits, &items, analyzed, state, others),
        results: items,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::TempDir;
    use zenodotus_core::config::IndexSettings;
    use zenodotus_core::ids::DocKey;
    use zenodotus_core::model::{DocState, Document, VersionChanges, content_hash};
    use zenodotus_core::time::Timestamp;

    use super::*;
    use crate::schema::tests::{scope, section, version_state};
    use crate::store::StoreWriter;

    struct Fixture {
        _dir: TempDir,
        store: Store,
    }

    /// Indexes `sections`, grouped into documents, with one version state per scope.
    fn fixture(sections: &[Section], versions: &[VersionState]) -> Fixture {
        let dir = TempDir::new().unwrap();
        let mut writer = StoreWriter::open(dir.path(), &IndexSettings::default()).unwrap();
        for state in versions {
            let mut documents: BTreeMap<DocKey, Vec<Section>> = BTreeMap::new();
            for s in sections
                .iter()
                .filter(|s| DocKey::new(&state.scope, s.doc_key.doc_path()).unwrap() == s.doc_key)
            {
                documents.entry(s.doc_key.clone()).or_default().push(s.clone());
            }
            let upserts = documents
                .into_iter()
                .map(|(doc_key, sections)| Document {
                    state: DocState {
                        doc_key,
                        etag: None,
                        last_modified: None,
                        content_hash: content_hash("doc"),
                        fetched_at: Timestamp::from_unix(0),
                    },
                    sections,
                })
                .collect();
            let changes = VersionChanges {
                upserts,
                listing_complete: true,
                ..VersionChanges::default()
            };
            writer.apply(state, &changes).unwrap();
        }
        writer.finish().unwrap();
        Fixture {
            store: Store::open(dir.path(), &IndexSettings::default()).unwrap(),
            _dir: dir,
        }
    }

    fn one_version(sections: &[Section]) -> Fixture {
        fixture(sections, &[version_state(&scope("lib", "1"))])
    }

    fn lib() -> Scope {
        scope("lib", "1")
    }

    fn request(query: &str) -> QueryRequest<'_> {
        QueryRequest {
            library_id: "lib",
            query,
            version: None,
            language: None,
            max_results: 5,
            max_chars: 100_000,
        }
    }

    fn run(fixture: &Fixture, query: &str) -> QueryDocsResult {
        query_docs(&fixture.store, &request(query), &RankingSettings::default()).unwrap()
    }

    fn sources(result: &QueryDocsResult) -> Vec<&str> {
        result.results.iter().map(|item| item.source_ref.as_str()).collect()
    }

    #[test]
    fn stop_words_are_sorted_and_recognized() {
        assert!(STOP_WORDS.is_sorted());
        assert!(is_stop_word("the") && is_stop_word("how"));
        assert!(!is_stop_word("spawn") && !is_stop_word("The"));
    }

    #[test]
    fn analysis_separates_required_optional_identifiers_and_quotes() {
        let analyzed = analyze("How do I use tokio::spawn with \"blocking tasks\" and \"x\"? use").unwrap();
        assert_eq!(
            analyzed.words,
            [
                "how", "do", "i", "use", "tokio", "spawn", "with", "blocking", "tasks", "and", "x", "use"
            ]
        );
        assert_eq!(analyzed.required, ["use", "tokio", "spawn", "blocking", "tasks", "x"]);
        assert_eq!(analyzed.optional, ["how", "do", "i", "with", "and"]);
        assert!(analyzed.identifiers.contains(&"tokio.spawn".to_owned()));
        assert_eq!(analyzed.quoted, [["blocking", "tasks"]]);
        assert!(analyzed.has_content_words);

        let stops = analyze("how to").unwrap();
        assert_eq!(
            (stops.required, stops.optional, stops.has_content_words),
            (vec!["how".to_owned(), "to".to_owned()], vec![], false)
        );
    }

    #[test]
    fn analysis_limits_length_and_terms() {
        assert!(matches!(analyze("?! --"), Err(SearchError::EmptyQuery)));
        let longest = "é ".repeat(MAX_QUERY_CHARS / 2);
        assert!(analyze(&longest).is_ok());
        let error = analyze(&format!("{longest}x")).unwrap_err();
        assert_eq!(error.to_string(), "the query has 1001 characters; the limit is 1000");
        let many: Vec<String> = (0..100).map(|i| format!("w{i}")).collect();
        let analyzed = analyze(&many.join(" ")).unwrap();
        assert_eq!(
            (analyzed.words.len(), analyzed.identifiers.len()),
            (MAX_QUERY_TERMS, MAX_QUERY_TERMS)
        );
    }

    #[test]
    fn minimum_should_match_rounds_up_and_requires_one() {
        assert_eq!(minimum_should_match(0.0, 3), 1);
        assert_eq!(minimum_should_match(0.5, 3), 2);
        assert_eq!(minimum_should_match(1.0, 3), 3);
        assert_eq!(minimum_should_match(0.0, 0), 1);
    }

    #[test]
    fn titles_outrank_body_mentions_and_stop_words_alone_never_match() {
        let s = lib();
        let fixture = one_version(&[
            section(
                &s,
                "body.md",
                0,
                &["Overview"],
                "You can spawn work here, among many other words in the body.",
            ),
            section(
                &s,
                "title.md",
                0,
                &["Spawn"],
                "Details follow in this section of the guide.",
            ),
            section(&s, "stop.md", 0, &["How to"], "How to do it with the thing."),
        ]);
        let result = run(&fixture, "how to spawn");
        assert_eq!(sources(&result), ["local:title.md", "local:body.md"]);
        assert_eq!(result.results[0].signals, [Signal::Title]);
        assert_eq!(result.results[0].rank, 1);
        assert!(result.results[0].score > result.results[1].score);
        assert_eq!(result.notice, None);
        assert_eq!(
            sources(&run(&fixture, "how to")),
            ["local:stop.md", "local:title.md", "local:body.md"][..1]
        );
    }

    #[test]
    fn min_should_match_requires_more_words() {
        let s = lib();
        let fixture = one_version(&[
            section(&s, "a.md", 0, &["A"], "alpha beta"),
            section(&s, "b.md", 0, &["B"], "alpha"),
        ]);
        let strict = RankingSettings {
            min_should_match: 1.0,
            ..RankingSettings::default()
        };
        let result = query_docs(&fixture.store, &request("alpha beta"), &strict).unwrap();
        assert_eq!(sources(&result), ["local:a.md"]);
        assert_eq!(sources(&run(&fixture, "alpha beta")), ["local:a.md", "local:b.md"]);
    }

    #[test]
    fn synonyms_match_with_a_lower_weight() {
        let s = lib();
        let fixture = one_version(&[
            section(&s, "word.md", 0, &["A"], "set the env first"),
            section(&s, "synonym.md", 0, &["B"], "set the environment first"),
            section(&s, "phrase.md", 0, &["C"], "set the environment variable first"),
        ]);
        let mut ranking = RankingSettings::default();
        ranking.synonyms.insert(
            "ENV".to_owned(),
            vec!["environment variable".to_owned(), "?".to_owned()],
        );
        ranking
            .synonyms
            .insert("other".to_owned(), vec!["environment".to_owned()]);
        let result = query_docs(&fixture.store, &request("env"), &ranking).unwrap();
        assert_eq!(sources(&result), ["local:word.md", "local:phrase.md"]);
        assert!(result.results[0].score > result.results[1].score);
        assert_eq!(sources(&run(&fixture, "env")), ["local:word.md"]);
        let mut single = RankingSettings::default();
        single.synonyms.insert("variable".to_owned(), vec!["first".to_owned()]);
        let result = query_docs(&fixture.store, &request("variable"), &single).unwrap();
        assert_eq!(
            sources(&result),
            ["local:phrase.md", "local:synonym.md", "local:word.md"]
        );
    }

    #[test]
    fn a_corrupt_section_payload_is_reported() {
        let dir = TempDir::new().unwrap();
        let mut writer = StoreWriter::open(dir.path(), &IndexSettings::default()).unwrap();
        let fields = *writer.store().fields();
        let mut doc = tantivy::TantivyDocument::new();
        doc.add_text(fields.kind, KIND_SECTION);
        doc.add_text(fields.scope, "lib@1");
        doc.add_text(fields.combined, "broken");
        doc.add_text(fields.payload, "{");
        writer.add_raw_for_test(doc);
        writer
            .apply(&version_state(&lib()), &VersionChanges::default())
            .unwrap();
        let error = query_docs(writer.store(), &request("broken"), &RankingSettings::default()).unwrap_err();
        assert!(matches!(error, SearchError::Payload(_)), "{error}");
    }

    #[test]
    fn exact_symbols_phrases_and_languages_add_signals_and_score() {
        let s = lib();
        let mut symbol = section(
            &s,
            "symbol.md",
            0,
            &["Tasks"],
            "Use tokio::spawn to start a task.\n\n```python\nrun()\n```\n",
        );
        symbol.symbols = vec!["tokio::spawn".to_owned()];
        symbol.code_languages = vec!["Python".to_owned()];
        let plain = section(&s, "plain.md", 0, &["Tasks"], "Tokio can spawn a task, said the docs.");
        let fixture = one_version(&[symbol, plain]);

        let result = run(&fixture, "tokio::spawn");
        assert_eq!(sources(&result), ["local:symbol.md", "local:plain.md"]);
        assert_eq!(result.results[0].signals, [Signal::ExactSymbol, Signal::Phrase]);
        assert_eq!(result.results[1].signals, [] as [Signal; 0]);

        let mut python = request("start task run");
        python.language = Some("PYTHON");
        let result = query_docs(&fixture.store, &python, &RankingSettings::default()).unwrap();
        assert_eq!(result.results[0].signals, [Signal::Code, Signal::Language]);

        let flat = RankingSettings {
            symbol_boost: 0.0,
            phrase_boost: 0.0,
            quoted_phrase_boost: 0.0,
            language_boost: 0.0,
            ..RankingSettings::default()
        };
        let mut quoted = request("\"spawn a task\"");
        quoted.language = Some("python");
        let flat_result = query_docs(&fixture.store, &quoted, &flat).unwrap();
        let boosted_result = query_docs(&fixture.store, &quoted, &RankingSettings::default()).unwrap();
        assert_eq!(sources(&boosted_result)[0], "local:plain.md");
        assert!(boosted_result.results[0].score > flat_result.results[0].score);
    }

    #[test]
    fn versions_are_queried_in_isolation() {
        let (one, two) = (scope("lib", "1"), scope("lib", "2"));
        let mut second = version_state(&two);
        second.default = true;
        second.list_order = 1;
        second.last_success = Some(Timestamp::from_unix(0));
        let fixture = fixture(
            &[
                section(&one, "a.md", 0, &["A"], "old api"),
                section(&two, "a.md", 0, &["A"], "new api"),
            ],
            &[version_state(&one), second],
        );
        let result = run(&fixture, "api");
        assert_eq!(
            (result.version.as_str(), result.version_resolution),
            ("2", VersionResolution::Default)
        );
        assert_eq!(result.results.len(), 1);
        assert!(result.results[0].text.contains("new"));
        assert_eq!(result.results[0].version, "2");
        assert_eq!(
            result.sync_status,
            SyncStatus {
                revision: 1,
                last_success: Some("1970-01-01T00:00:00Z".to_owned()),
                outcome: "ok".to_owned(),
            }
        );
        let mut explicit = request("api");
        explicit.version = Some("1");
        let result = query_docs(&fixture.store, &explicit, &RankingSettings::default()).unwrap();
        assert_eq!(
            (result.version.as_str(), result.version_resolution),
            ("1", VersionResolution::Explicit)
        );
        assert!(result.results[0].text.contains("old"));
        explicit.version = Some("3");
        let error = query_docs(&fixture.store, &explicit, &RankingSettings::default()).unwrap_err();
        assert!(matches!(error, SearchError::UnknownVersion { .. }), "{error}");
        let mut other = request("api");
        other.library_id = "nope";
        assert!(matches!(
            query_docs(&fixture.store, &other, &RankingSettings::default()),
            Err(SearchError::UnknownLibrary { .. })
        ));
        assert!(matches!(
            query_docs(&fixture.store, &request(""), &RankingSettings::default()),
            Err(SearchError::EmptyQuery)
        ));
    }

    #[test]
    fn ties_at_the_cut_break_on_chunk_id_and_responses_repeat_exactly() {
        let s = lib();
        let sections: Vec<Section> = (0..6)
            .map(|i| section(&s, &format!("d{i}.md"), 0, &["T"], &format!("alpha n{i}")))
            .collect();
        let mut expected: Vec<String> = sections.iter().map(|s| s.chunk_id.as_str().to_owned()).collect();
        expected.sort();
        let fixture = one_version(&sections);
        let mut tied = request("alpha");
        tied.max_results = 2;
        let first = query_docs(&fixture.store, &tied, &RankingSettings::default()).unwrap();
        let ids: Vec<&str> = first.results.iter().map(|item| item.chunk_id.as_str()).collect();
        assert_eq!(ids, expected[..2]);
        assert_eq!(first.results[0].score, first.results[1].score);
        let again = query_docs(&fixture.store, &tied, &RankingSettings::default()).unwrap();
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&again).unwrap()
        );
        tied.max_results = 0;
        assert_eq!(
            query_docs(&fixture.store, &tied, &RankingSettings::default())
                .unwrap()
                .results
                .len(),
            1
        );
    }

    #[test]
    fn duplicates_are_dropped_and_the_budget_keeps_at_least_one() {
        let s = lib();
        let fixture = one_version(&[
            section(&s, "a.md", 0, &["Same"], "gamma text"),
            section(&s, "b.md", 0, &["Same"], "gamma text"),
            section(&s, "c.md", 0, &["Other"], "gamma different and longer text"),
        ]);
        let result = run(&fixture, "gamma");
        assert_eq!(result.results.len(), 2);
        let mut small = request("gamma");
        small.max_chars = 1;
        assert_eq!(
            query_docs(&fixture.store, &small, &RankingSettings::default())
                .unwrap()
                .results
                .len(),
            1
        );
        let first = &result.results[0];
        small.max_chars = first.text.chars().count()
            + first.title.chars().count()
            + first.heading_path[0].chars().count()
            + first.source_ref.chars().count();
        assert_eq!(
            query_docs(&fixture.store, &small, &RankingSettings::default())
                .unwrap()
                .results
                .len(),
            1
        );
        small.max_chars += 1000;
        assert_eq!(
            query_docs(&fixture.store, &small, &RankingSettings::default())
                .unwrap()
                .results
                .len(),
            2
        );
    }

    #[test]
    fn notices_report_no_results_and_weak_matches() {
        let (one, two) = (scope("lib", "1"), scope("lib", "2"));
        let mut second = version_state(&two);
        second.list_order = 1;
        let mut zeroth = version_state(&scope("lib", "0"));
        zeroth.list_order = 5;
        let fixture = fixture(
            &[section(&one, "a.md", 0, &["A"], "alpha beta gamma")],
            &[version_state(&one), second, zeroth, version_state(&scope("other", "1"))],
        );
        let result = run(&fixture, "zeta");
        assert_eq!(result.results, []);
        assert_eq!(
            result.notice,
            Some(Notice {
                kind: NoticeKind::NoResults,
                message: "No section of lib@1 matched the query. Other indexed versions: 2, 0.".to_owned(),
                other_versions: vec!["2".to_owned(), "0".to_owned()],
            })
        );
        let weak = run(&fixture, "alpha delta epsilon zeta");
        assert_eq!(weak.notice.as_ref().map(|n| n.kind), Some(NoticeKind::WeakMatch));
        assert!(
            weak.notice
                .unwrap()
                .message
                .ends_with("may not answer it. Other indexed versions: 2, 0.")
        );
        assert_eq!(run(&fixture, "alpha delta").notice, None);

        let lonely = one_version(&[section(&lib(), "a.md", 0, &["A"], "alpha")]);
        let result = run(&lonely, "omega");
        assert_eq!(result.notice.unwrap().message, "No section of lib@1 matched the query.");
        assert_eq!(run(&lonely, "how a").notice, None);
    }

    #[test]
    fn short_hits_absorb_a_following_code_section() {
        let s = lib();
        let fixture = one_version(&[
            section(&s, "a.md", 0, &["Example"], "## Example\n\nShort intro.\n"),
            section(&s, "a.md", 1, &["Example (part 2)"], "```rust\nfn main() {}\n```\n"),
            section(&s, "b.md", 0, &["Sample"], "## Sample\n\nShort intro.\n"),
            section(&s, "b.md", 1, &["Next"], "## Next\n\nMore prose.\n"),
            section(
                &s,
                "c.md",
                0,
                &["Lengthy"],
                "## Lengthy\n\nA much longer introduction to things.\n",
            ),
            section(&s, "c.md", 1, &["Lengthy (part 2)"], "~~~\ncode\n~~~\n"),
            section(&s, "d.md", 0, &["Last"], "## Last\n\nShort intro.\n"),
        ]);
        let expand = RankingSettings {
            adjacent_max_prose_chars: 30,
            ..RankingSettings::default()
        };
        let text_of = |query: &str, ranking: &RankingSettings| {
            query_docs(&fixture.store, &request(query), ranking).unwrap().results[0]
                .text
                .clone()
        };
        assert_eq!(
            text_of("example", &expand),
            "## Example\n\nShort intro.\n\n```rust\nfn main() {}\n```\n"
        );
        assert_eq!(
            text_of("example", &RankingSettings::default()),
            "## Example\n\nShort intro.\n"
        );
        assert_eq!(text_of("sample", &expand), "## Sample\n\nShort intro.\n");
        assert_eq!(
            text_of("lengthy", &expand),
            "## Lengthy\n\nA much longer introduction to things.\n"
        );
        assert_eq!(text_of("last", &expand), "## Last\n\nShort intro.\n");
    }

    #[test]
    fn code_detection_skips_headings_and_blank_lines() {
        assert!(starts_with_code("## Title\n\n```js\nx\n```"));
        assert!(starts_with_code("\n~~~\nx\n~~~"));
        assert!(!starts_with_code("Text\n```\nx\n```"));
        assert!(!starts_with_code("# Only a heading\n"));
        assert!(contains_run(&texts("a b c"), &texts("b c")));
        assert!(!contains_run(&texts("a b c"), &texts("c b")));
    }
}
