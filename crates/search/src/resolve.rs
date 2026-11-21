//! From a library name to indexed libraries, and from a library and an optional version to one indexed version.

use std::cmp::Reverse;

use zenodotus_core::model::VersionState;

use crate::analyzer::code_words;
use crate::error::SearchError;
use crate::query::is_stop_word;
use crate::render_model::{LibraryMatch, MatchKind, VersionResolution};

/// The lowest Jaro-Winkler similarity that counts as a fuzzy match.
pub const FUZZY_THRESHOLD: f64 = 0.88;

/// `text` lowercased, without anything but letters and digits.
fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The versions of `library` among `states`, in config order.
fn versions_of<'a>(states: &'a [VersionState], library: &str) -> Vec<&'a VersionState> {
    let mut versions: Vec<&VersionState> = states
        .iter()
        .filter(|state| state.scope.library.as_str() == library)
        .collect();
    versions.sort_by_key(|state| (state.list_order, state.scope.version.clone()));
    versions
}

/// The version a query without an explicit version uses: the default, or else the first listed.
fn default_version<'a>(versions: &[&'a VersionState]) -> (&'a VersionState, VersionResolution) {
    versions
        .iter()
        .find(|state| state.default)
        .map_or((versions[0], VersionResolution::FirstListed), |state| {
            (*state, VersionResolution::Default)
        })
}

/// Picks the version of `library` a query runs on.
///
/// # Errors
///
/// [`SearchError::UnknownLibrary`] when no version of `library` is indexed, or [`SearchError::UnknownVersion`]
/// when `version` is given but not indexed.
pub fn resolve_version<'a>(
    states: &'a [VersionState],
    library: &str,
    version: Option<&str>,
) -> Result<(&'a VersionState, VersionResolution), SearchError> {
    let versions = versions_of(states, library);
    if versions.is_empty() {
        let mut available: Vec<String> = states.iter().map(|state| state.scope.library.to_string()).collect();
        available.dedup();
        return Err(SearchError::UnknownLibrary {
            library: library.to_owned(),
            available,
        });
    }
    let Some(version) = version else {
        return Ok(default_version(&versions));
    };
    versions
        .iter()
        .find(|state| state.scope.version.as_str() == version)
        .map(|state| (*state, VersionResolution::Explicit))
        .ok_or_else(|| SearchError::UnknownVersion {
            library: library.to_owned(),
            version: version.to_owned(),
            available: versions.iter().map(|state| state.scope.version.to_string()).collect(),
        })
}

/// How well `name` matches one library, as `(kind, similarity)`; the similarity orders fuzzy matches.
fn match_library(name: &str, state: &VersionState) -> Option<(MatchKind, f64)> {
    let wanted = name.trim().to_lowercase();
    if wanted == state.scope.library.as_str() {
        return Some((MatchKind::Exact, 1.0));
    }
    let names: Vec<&str> = std::iter::once(state.name.as_str())
        .chain(state.aliases.iter().map(String::as_str))
        .collect();
    if names.iter().any(|candidate| candidate.to_lowercase() == wanted) {
        return Some((MatchKind::Alias, 1.0));
    }
    let wanted = normalize(&wanted);
    if wanted.is_empty() {
        return None;
    }
    let candidates: Vec<String> = std::iter::once(state.scope.library.as_str())
        .chain(names)
        .map(normalize)
        .collect();
    if candidates.contains(&wanted) {
        return Some((MatchKind::Normalized, 1.0));
    }
    if candidates.iter().any(|candidate| candidate.starts_with(&wanted)) {
        return Some((MatchKind::Prefix, 1.0));
    }
    let similarity = candidates
        .iter()
        .map(|candidate| strsim::jaro_winkler(&wanted, candidate))
        .fold(0.0, f64::max);
    (similarity >= FUZZY_THRESHOLD).then_some((MatchKind::Fuzzy, similarity))
}

/// How many distinct non-stop words of `query` appear in `description`.
fn query_overlap(query: Option<&str>, description: &str) -> usize {
    let described: Vec<String> = code_words(description).into_iter().map(|token| token.text).collect();
    let mut words: Vec<String> = query
        .map(code_words)
        .unwrap_or_default()
        .into_iter()
        .map(|token| token.text)
        .filter(|word| !is_stop_word(word))
        .collect();
    words.sort();
    words.dedup();
    words.iter().filter(|word| described.contains(word)).count()
}

/// The libraries among `states` that match `name`, best first: by match kind, then by how many words of `query`
/// their description contains, then by similarity, then by id.
#[must_use]
pub fn resolve_library(states: &[VersionState], name: &str, query: Option<&str>) -> Vec<LibraryMatch> {
    let mut libraries: Vec<&str> = states.iter().map(|state| state.scope.library.as_str()).collect();
    libraries.sort_unstable();
    libraries.dedup();
    let mut ranked = Vec::new();
    for library in libraries {
        let versions = versions_of(states, library);
        let (main, _) = default_version(&versions);
        let Some((kind, similarity)) = match_library(name, main) else {
            continue;
        };
        let overlap = query_overlap(query, &main.description);
        let found = LibraryMatch {
            library_id: library.to_owned(),
            name: main.name.clone(),
            description: main.description.clone(),
            versions: versions.iter().map(|state| state.scope.version.to_string()).collect(),
            default_version: main.scope.version.to_string(),
            source: main.source.clone(),
            last_synced: versions
                .iter()
                .filter_map(|state| state.last_success)
                .max()
                .map(|at| at.to_string()),
            match_kind: kind,
        };
        ranked.push(((kind, Reverse(overlap), Reverse(similarity.to_bits())), found));
    }
    ranked.sort_by(|(a, x), (b, y)| a.cmp(b).then_with(|| x.library_id.cmp(&y.library_id)));
    ranked.into_iter().map(|(_, found)| found).collect()
}

#[cfg(test)]
mod tests {
    use zenodotus_core::time::Timestamp;

    use super::*;
    use crate::schema::tests::{scope, version_state};

    fn state(library: &str, version: &str, list_order: u32, default: bool) -> VersionState {
        let mut state = version_state(&scope(library, version));
        state.list_order = list_order;
        state.default = default;
        state
    }

    fn states() -> Vec<VersionState> {
        vec![
            state("react", "18", 1, true),
            state("react", "19", 0, false),
            state("tokio", "1", 0, false),
            state("tokio", "2", 1, false),
        ]
    }

    #[test]
    fn explicit_default_and_first_listed_versions() {
        let states = states();
        let pick = |library, version| {
            let (state, how) = resolve_version(&states, library, version).unwrap();
            (state.scope.to_string(), how)
        };
        assert_eq!(
            pick("react", Some("19")),
            ("react@19".to_owned(), VersionResolution::Explicit)
        );
        assert_eq!(pick("react", None), ("react@18".to_owned(), VersionResolution::Default));
        assert_eq!(
            pick("tokio", None),
            ("tokio@1".to_owned(), VersionResolution::FirstListed)
        );
    }

    #[test]
    fn unknown_libraries_and_versions_list_what_exists() {
        let states = states();
        let error = resolve_version(&states, "vue", None).unwrap_err();
        assert_eq!(
            error.to_string(),
            "unknown library \"vue\"; indexed libraries: react, tokio"
        );
        let error = resolve_version(&states, "react", Some("17")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "library \"react\" has no version \"17\"; available versions: 19, 18"
        );
    }

    fn kinds(states: &[VersionState], name: &str) -> Vec<(String, MatchKind)> {
        resolve_library(states, name, None)
            .into_iter()
            .map(|found| (found.library_id, found.match_kind))
            .collect()
    }

    #[test]
    fn match_kinds_are_tried_in_order() {
        let mut next = state("next.js", "15", 0, false);
        next.name = "Next.js".to_owned();
        next.aliases = vec!["nextjs".to_owned()];
        let mut vue_like = state("nuxt", "3", 0, false);
        vue_like.name = "Nuxt".to_owned();
        let states = vec![next, vue_like, state("tokio", "1", 0, false)];
        assert_eq!(kinds(&states, " Next.js "), [("next.js".to_owned(), MatchKind::Exact)]);
        assert_eq!(kinds(&states, "NEXTJS"), [("next.js".to_owned(), MatchKind::Alias)]);
        assert_eq!(
            kinds(&states, "next js"),
            [("next.js".to_owned(), MatchKind::Normalized)]
        );
        assert_eq!(kinds(&states, "nex"), [("next.js".to_owned(), MatchKind::Prefix)]);
        assert_eq!(kinds(&states, "tokyo"), [("tokio".to_owned(), MatchKind::Fuzzy)]);
        assert_eq!(kinds(&states, "nuxt"), [("nuxt".to_owned(), MatchKind::Exact)]);
        assert_eq!(kinds(&states, "zzz"), []);
        assert_eq!(kinds(&states, "!!"), []);
    }

    #[test]
    fn ties_break_on_query_words_then_similarity_then_id() {
        let mut a = state("axum", "1", 0, false);
        a.description = "Web framework".to_owned();
        let mut b = state("actix", "1", 0, false);
        b.description = "Actor based web framework with routing".to_owned();
        let c = state("aaa", "1", 0, false);
        let states = vec![a, b, c];
        let order = |query| -> Vec<String> {
            resolve_library(&states, "a", query)
                .into_iter()
                .map(|found| found.library_id)
                .collect()
        };
        assert_eq!(order(None), ["aaa", "actix", "axum"]);
        assert_eq!(
            order(Some("how do I add routing to a web app")),
            ["actix", "axum", "aaa"]
        );

        let fuzzy = vec![state("tokia", "1", 0, false), state("tokyo", "1", 0, false)];
        let order: Vec<String> = resolve_library(&fuzzy, "tokio", None)
            .into_iter()
            .map(|found| found.library_id)
            .collect();
        assert_eq!(order, ["tokia", "tokyo"]);
    }

    #[test]
    fn a_match_describes_the_library_from_its_default_version() {
        let mut states = states();
        states[0].name = "React".to_owned();
        states[0].description = "UI library".to_owned();
        states[0].source = "github:facebook/react@v18".to_owned();
        states[0].last_success = Some(Timestamp::from_unix(0));
        states[1].last_success = Some(Timestamp::from_unix(60));
        let found = resolve_library(&states, "react", Some("hooks")).remove(0);
        assert_eq!(
            found,
            LibraryMatch {
                library_id: "react".to_owned(),
                name: "React".to_owned(),
                description: "UI library".to_owned(),
                versions: vec!["19".to_owned(), "18".to_owned()],
                default_version: "18".to_owned(),
                source: "github:facebook/react@v18".to_owned(),
                last_synced: Some("1970-01-01T00:01:00Z".to_owned()),
                match_kind: MatchKind::Exact,
            }
        );
        assert_eq!(resolve_library(&states, "tokio", None)[0].last_synced, None);
    }
}
