//! Judging: a section is a hit when, after whitespace normalization, it contains every `must_contain` string and at
//! least one string of each `any_of` group. The same rule applies to every system.

use crate::questions::Question;

/// `text` with every run of whitespace replaced by one space, and no leading or trailing whitespace.
#[must_use]
pub fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The normalized strings of one question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judge {
    must_contain: Vec<String>,
    any_of: Vec<Vec<String>>,
}

impl Judge {
    /// The judge of `question`.
    #[must_use]
    pub fn new(question: &Question) -> Self {
        Self {
            must_contain: question.must_contain.iter().map(|text| normalize(text)).collect(),
            any_of: question
                .any_of
                .iter()
                .map(|group| group.iter().map(|text| normalize(text)).collect())
                .collect(),
        }
    }

    /// Whether `text` answers the question.
    #[must_use]
    pub fn is_hit(&self, text: &str) -> bool {
        let text = normalize(text);
        self.must_contain.iter().all(|needle| text.contains(needle.as_str()))
            && self
                .any_of
                .iter()
                .all(|group| group.iter().any(|needle| text.contains(needle.as_str())))
    }

    /// The rank, from 1, of the first hit among the first `k` sections.
    #[must_use]
    pub fn first_hit(&self, sections: &[String], k: usize) -> Option<usize> {
        sections
            .iter()
            .take(k)
            .position(|section| self.is_hit(section))
            .map(|index| index + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::questions::tests::question;

    fn sections(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_owned()).collect()
    }

    #[test]
    fn whitespace_is_normalized_on_both_sides() {
        assert_eq!(normalize("  a\n\tb   c \n"), "a b c");
        let judge = Judge::new(&question("q", "tokio", None, &["fn  spawn<F>(\n  future: F)"]));
        assert!(judge.is_hit("pub fn spawn<F>( future: F) -> JoinHandle"));
        assert!(!judge.is_hit("pub fn spawn<T>(future: T)"));
    }

    #[test]
    fn every_must_contain_string_is_required_and_matching_is_case_sensitive() {
        let judge = Judge::new(&question("q", "fastapi", None, &["Depends", "yield"]));
        assert!(judge.is_hit("Use Depends with yield."));
        assert!(!judge.is_hit("Use Depends."));
        assert!(!judge.is_hit("use depends with yield."));
    }

    #[test]
    fn each_any_of_group_needs_one_of_its_strings() {
        let mut q = question("q", "vite", None, &["build.target"]);
        q.any_of = vec![
            vec!["es2020".to_owned(), "esnext".to_owned()],
            vec!["browserslist".to_owned()],
        ];
        let judge = Judge::new(&q);
        assert!(judge.is_hit("build.target defaults to esnext; see browserslist"));
        assert!(judge.is_hit("build.target: es2020 and browserslist"));
        assert!(!judge.is_hit("build.target: es2020"));
        assert!(!judge.is_hit("build.target and browserslist"));
        q.must_contain.clear();
        assert!(Judge::new(&q).is_hit("esnext browserslist"));
    }

    #[test]
    fn the_first_hit_is_ranked_from_one_within_k() {
        let judge = Judge::new(&question("q", "tokio", None, &["select!"]));
        let ranked = sections(&["intro", "use tokio::select!", "select! again"]);
        assert_eq!(judge.first_hit(&ranked, 10), Some(2));
        assert_eq!(judge.first_hit(&ranked, 1), None);
        assert_eq!(judge.first_hit(&[], 10), None);
    }
}
