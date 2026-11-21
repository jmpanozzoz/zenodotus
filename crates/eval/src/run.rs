//! Running one system over a question set, judging every ranking the same way.

use serde::Serialize;

use crate::corpus::ScopeCase;
use crate::judge::Judge;
use crate::questions::{Question, Tag};
use crate::systems::{QueryFailure, RetrievalSystem};

/// How many results each system is asked for, and the depth of MRR.
pub const DEPTH: usize = 10;

/// What happened to one question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// The system answered; `first_hit` is the rank of the first hit within [`DEPTH`], from 1.
    Answered {
        /// The rank of the first hit.
        first_hit: Option<usize>,
    },
    /// The system does not have the version; the question is excluded from its score.
    VersionUnavailable,
    /// The call failed; the question counts against no metric but is reported.
    Failed {
        /// Why.
        error: String,
    },
}

/// The result of one question on one system, as written to the raw results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Record {
    /// The question.
    pub id: String,
    /// Its query string.
    pub query: String,
    /// Its tags.
    pub tags: Vec<Tag>,
    /// What happened.
    #[serde(flatten)]
    pub outcome: Outcome,
    /// The call latency in microseconds, when the system answered.
    pub latency_micros: Option<u64>,
    /// The ranked section texts.
    pub sections: Vec<String>,
    /// The whole response text.
    pub raw: String,
}

impl Record {
    /// Whether the system answered.
    #[must_use]
    pub const fn answered(&self) -> bool {
        matches!(self.outcome, Outcome::Answered { .. })
    }

    /// The rank of the first hit, from 1; `None` when the system missed or did not answer.
    #[must_use]
    pub const fn first_hit(&self) -> Option<usize> {
        match self.outcome {
            Outcome::Answered { first_hit } => first_hit,
            _ => None,
        }
    }
}

/// Asks `system` every question of `cases`, in order, for [`DEPTH`] results.
pub async fn evaluate<S: RetrievalSystem>(system: &mut S, cases: &[(&Question, &ScopeCase)]) -> Vec<Record> {
    let mut records = Vec::with_capacity(cases.len());
    for (question, case) in cases {
        let mut record = Record {
            id: question.id.clone(),
            query: question.query.clone(),
            tags: question.tags.clone(),
            outcome: Outcome::VersionUnavailable,
            latency_micros: None,
            sections: Vec::new(),
            raw: String::new(),
        };
        match system.query(question, case, DEPTH).await {
            Ok(ranked) => {
                record.outcome = Outcome::Answered {
                    first_hit: Judge::new(question).first_hit(&ranked.sections, DEPTH),
                };
                record.latency_micros = Some(u64::try_from(ranked.latency.as_micros()).unwrap_or(u64::MAX));
                record.sections = ranked.sections;
                record.raw = ranked.raw;
            }
            Err(QueryFailure::VersionUnavailable) => {}
            Err(QueryFailure::Failed(error)) => record.outcome = Outcome::Failed { error },
        }
        records.push(record);
    }
    records
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use super::*;
    use crate::corpus::tests::corpus;
    use crate::questions::tests::question;
    use crate::systems::Ranked;

    /// A system that answers from a script keyed by question id.
    struct Scripted;

    impl RetrievalSystem for Scripted {
        async fn query(&mut self, question: &Question, _case: &ScopeCase, k: usize) -> Result<Ranked, QueryFailure> {
            assert_eq!(k, DEPTH);
            tokio::task::yield_now().await;
            match question.id.as_str() {
                "old" => Err(QueryFailure::VersionUnavailable),
                "down" => Err(QueryFailure::Failed("timeout".to_owned())),
                _ => Ok(Ranked {
                    sections: vec!["nothing".to_owned(), "tokio::spawn here".to_owned()],
                    raw: "raw text".to_owned(),
                    latency: Duration::from_micros(1500),
                }),
            }
        }
    }

    /// A record of `id` with `outcome`, tags `tags` and a raw text of `chars` characters.
    pub(crate) fn record(id: &str, outcome: Outcome, tags: &[Tag], chars: usize, micros: u64) -> Record {
        Record {
            id: id.to_owned(),
            query: format!("query of {id}"),
            tags: tags.to_vec(),
            outcome,
            latency_micros: Some(micros),
            sections: Vec::new(),
            raw: "x".repeat(chars),
        }
    }

    #[tokio::test]
    async fn every_question_is_asked_judged_and_timed() {
        let (_root, corpus) = corpus();
        let case = &corpus.scopes[0];
        let questions = [
            question("hit", "tokio", None, &["tokio::spawn"]),
            question("miss", "tokio", None, &["select!"]),
            question("old", "tokio", None, &["x"]),
            question("down", "tokio", None, &["x"]),
        ];
        let cases: Vec<(&Question, &ScopeCase)> = questions.iter().map(|q| (q, case)).collect();
        let records = evaluate(&mut Scripted, &cases).await;
        let outcomes: Vec<&Outcome> = records.iter().map(|record| &record.outcome).collect();
        assert_eq!(
            outcomes,
            [
                &Outcome::Answered { first_hit: Some(2) },
                &Outcome::Answered { first_hit: None },
                &Outcome::VersionUnavailable,
                &Outcome::Failed {
                    error: "timeout".to_owned()
                },
            ]
        );
        assert_eq!(records[0].latency_micros, Some(1500));
        assert_eq!(records[0].raw, "raw text");
        assert_eq!(records[0].tags, [Tag::Symbol]);
        assert_eq!(records[2].latency_micros, None);
        assert_eq!((records[0].answered(), records[0].first_hit()), (true, Some(2)));
        assert_eq!((records[2].answered(), records[2].first_hit()), (false, None));
        let json = serde_json::to_value(&records[3]).unwrap();
        assert_eq!(json["outcome"], "failed");
        assert_eq!(json["error"], "timeout");
    }
}
