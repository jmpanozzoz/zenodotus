//! Reports: per-system summaries, the table at Context7's response size, Recall@5 by tag, the Goal 4 decision, and
//! the raw results on disk.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the report is the program's output"
)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::bootstrap::{Estimate, Interval, RESAMPLES, SEED, estimate, paired_difference};
use crate::error::EvalError;
use crate::judge::Judge;
use crate::metrics::{estimate_tokens, hit_within, mean, median, percentile, reciprocal_rank};
use crate::questions::{Question, Set, Tag};
use crate::run::{DEPTH, Outcome, Record};

/// The cut-off of Recall.
pub const RECALL_AT: usize = 5;

/// The name Context7 runs under.
pub const CONTEXT7: &str = "context7";

/// The name zenodotus runs under.
pub const ZENODOTUS: &str = "zenodotus";

/// Goal 4 is met when the lower bound of `Recall@5(zenodotus) − Recall@5(Context7)` is above this.
pub const MARGIN: f64 = -0.02;

/// What one system produced: its records, or why it could not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemResult {
    /// The system's name.
    pub name: &'static str,
    /// Its records, or the reason it did not run.
    pub outcome: Result<Vec<Record>, String>,
}

/// The metrics of one system.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    /// The system's name.
    pub system: String,
    /// Questions answered.
    pub answered: usize,
    /// Questions excluded because the system lacks the version.
    pub unavailable: usize,
    /// Questions whose call failed.
    pub failed: usize,
    /// Recall@5 over the answered questions.
    pub recall: Option<Estimate>,
    /// MRR@10 over the answered questions.
    pub mrr: Option<Estimate>,
    /// The median latency in milliseconds.
    pub p50_ms: Option<f64>,
    /// The 95th percentile latency in milliseconds.
    pub p95_ms: Option<f64>,
    /// The median response size in estimated tokens.
    pub median_tokens: Option<usize>,
    /// Recall@5 by tag: the tag, the answered questions with it, and the recall.
    pub by_tag: Vec<(Tag, usize, Option<f64>)>,
}

/// Summarizes the records of `system`.
#[must_use]
pub fn summarize(system: &str, records: &[Record]) -> Summary {
    let answered: Vec<&Record> = records.iter().filter(|record| record.answered()).collect();
    let ranks: Vec<Option<usize>> = answered.iter().map(|record| record.first_hit()).collect();
    let recall: Vec<f64> = ranks.iter().map(|rank| hit_within(*rank, RECALL_AT)).collect();
    let mrr: Vec<f64> = ranks.iter().map(|rank| reciprocal_rank(*rank, DEPTH)).collect();
    let latencies: Vec<Duration> = answered
        .iter()
        .filter_map(|record| record.latency_micros)
        .map(Duration::from_micros)
        .collect();
    let tokens: Vec<usize> = answered
        .iter()
        .map(|record| estimate_tokens(record.raw.chars().count()))
        .collect();
    let millis = |p: f64| percentile(&latencies, p).map(|latency| latency.as_secs_f64() * 1000.0);
    let by_tag = Tag::ALL
        .iter()
        .map(|tag| {
            let tagged: Vec<f64> = answered
                .iter()
                .filter(|record| record.tags.contains(tag))
                .map(|record| hit_within(record.first_hit(), RECALL_AT))
                .collect();
            (*tag, tagged.len(), mean(&tagged))
        })
        .collect();
    Summary {
        system: system.to_owned(),
        answered: answered.len(),
        unavailable: count(records, |outcome| matches!(outcome, Outcome::VersionUnavailable)),
        failed: count(records, |outcome| matches!(outcome, Outcome::Failed { .. })),
        recall: estimate(&recall),
        mrr: estimate(&mrr),
        p50_ms: millis(50.0),
        p95_ms: millis(95.0),
        median_tokens: median(&tokens),
        by_tag,
    }
}

/// How many records have an outcome that `matches`.
fn count(records: &[Record], matches: impl Fn(&Outcome) -> bool) -> usize {
    records.iter().filter(|record| matches(&record.outcome)).count()
}

/// The answered records of `records` with every ranking cut to `budget` characters: sections are kept in order
/// until the budget runs out, and the section that crosses it is cut short. Hits are judged again.
#[must_use]
pub fn at_budget(records: &[Record], questions: &BTreeMap<&str, &Question>, budget: usize) -> Vec<Record> {
    records
        .iter()
        .filter(|record| record.answered())
        .map(|record| {
            let mut left = budget;
            let mut sections = Vec::new();
            for section in &record.sections {
                if left == 0 {
                    break;
                }
                let kept: String = section.chars().take(left).collect();
                left -= kept.chars().count();
                sections.push(kept);
            }
            let first_hit = questions
                .get(record.id.as_str())
                .and_then(|question| Judge::new(question).first_hit(&sections, DEPTH));
            Record {
                outcome: Outcome::Answered { first_hit },
                raw: sections.concat(),
                sections,
                ..record.clone()
            }
        })
        .collect()
}

/// The Goal 4 decision.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Decision {
    /// Test questions that both systems answered.
    pub paired: usize,
    /// The 95% interval of the paired difference in Recall@5.
    pub difference: Interval,
    /// Whether its lower bound is above [`MARGIN`].
    pub met: bool,
}

/// The paired-bootstrap decision over the questions both systems answered, or `None` when there are none.
#[must_use]
pub fn decide(zenodotus: &[Record], context7: &[Record]) -> Option<Decision> {
    let theirs: BTreeMap<&str, Option<usize>> = context7
        .iter()
        .filter(|record| record.answered())
        .map(|record| (record.id.as_str(), record.first_hit()))
        .collect();
    let (ours, other): (Vec<f64>, Vec<f64>) = zenodotus
        .iter()
        .filter(|record| record.answered())
        .filter_map(|record| {
            let other = theirs.get(record.id.as_str())?;
            Some((hit_within(record.first_hit(), RECALL_AT), hit_within(*other, RECALL_AT)))
        })
        .unzip();
    let difference = paired_difference(&ours, &other, RESAMPLES, SEED)?;
    Some(Decision {
        paired: ours.len(),
        difference,
        met: difference.low > MARGIN,
    })
}

/// A whole report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    /// The question set.
    pub set: &'static str,
    /// Its size.
    pub questions: usize,
    /// Every system that ran.
    pub summaries: Vec<Summary>,
    /// Every system that did not run, with the reason.
    pub not_run: Vec<(String, String)>,
    /// Context7's median response size in characters, when it answered anything.
    pub budget_chars: Option<usize>,
    /// Every system that ran, cut to that size.
    pub at_budget: Vec<Summary>,
    /// The Goal 4 decision, when both zenodotus and Context7 answered a common question.
    pub decision: Option<Decision>,
}

/// Builds the report of `results` on `questions` of `set`.
#[must_use]
pub fn compose(set: Set, questions: &[Question], results: &[SystemResult]) -> Report {
    let by_id: BTreeMap<&str, &Question> = questions
        .iter()
        .map(|question| (question.id.as_str(), question))
        .collect();
    let ran: Vec<(&str, &Vec<Record>)> = results
        .iter()
        .filter_map(|result| Some((result.name, result.outcome.as_ref().ok()?)))
        .collect();
    let not_run = results
        .iter()
        .filter_map(|result| Some((result.name.to_owned(), result.outcome.as_ref().err()?.clone())))
        .collect();
    let records_of = |name: &str| {
        ran.iter()
            .find(|(system, _)| *system == name)
            .map(|(_, records)| *records)
    };
    let budget_chars = records_of(CONTEXT7).and_then(|records| {
        let sizes: Vec<usize> = records
            .iter()
            .filter(|record| record.answered())
            .map(|record| record.raw.chars().count())
            .collect();
        median(&sizes)
    });
    let at_budget = budget_chars.map_or_else(Vec::new, |budget| {
        ran.iter()
            .map(|(name, records)| summarize(name, &at_budget(records, &by_id, budget)))
            .collect()
    });
    let decision = records_of(ZENODOTUS)
        .zip(records_of(CONTEXT7))
        .and_then(|(ours, theirs)| decide(ours, theirs));
    Report {
        set: set.name(),
        questions: questions.len(),
        summaries: ran.iter().map(|(name, records)| summarize(name, records)).collect(),
        not_run,
        budget_chars,
        at_budget,
        decision,
    }
}

/// An estimate as `0.812 [0.750, 0.870]`, or a dash.
fn show_estimate(found: Option<&Estimate>) -> String {
    found.map_or_else(
        || "-".to_owned(),
        |found| {
            format!(
                "{:.3} [{:.3}, {:.3}]",
                found.value, found.interval.low, found.interval.high
            )
        },
    )
}

/// An optional value with `decimals` decimals, or a dash.
fn show(value: Option<f64>, decimals: usize) -> String {
    value.map_or_else(|| "-".to_owned(), |value| format!("{value:.decimals$}"))
}

/// The main table: one row per system.
fn table(summaries: &[Summary]) -> String {
    let mut out = "| System | Answered | Excluded | Failed | Recall@5 [95% CI] | MRR@10 [95% CI] | p50 ms | p95 ms | \
                   Median tokens (est.) |\n|---|---|---|---|---|---|---|---|---|\n"
        .to_owned();
    for summary in summaries {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            summary.system,
            summary.answered,
            summary.unavailable,
            summary.failed,
            show_estimate(summary.recall.as_ref()),
            show_estimate(summary.mrr.as_ref()),
            show(summary.p50_ms, 1),
            show(summary.p95_ms, 1),
            summary
                .median_tokens
                .map_or_else(|| "-".to_owned(), |tokens| tokens.to_string()),
        );
    }
    out
}

/// The report as text.
#[must_use]
pub fn render(report: &Report, results_dir: &Path) -> String {
    let mut out = format!(
        "zenodotus-eval: {} set, {} questions\nRaw results: {}\n\n{}",
        report.set,
        report.questions,
        results_dir.display(),
        table(&report.summaries)
    );
    for (name, reason) in &report.not_run {
        let _ = writeln!(out, "| {name} | not run: {reason} |");
    }
    if let Some(budget) = report.budget_chars {
        let _ = write!(
            out,
            "\nAt Context7's median response size ({budget} characters, about {} tokens):\n\n{}",
            estimate_tokens(budget),
            table(&report.at_budget)
        );
    }
    out.push_str("\nRecall@5 by tag (answered questions with the tag):\n\n| System |");
    for tag in Tag::ALL {
        let _ = write!(out, " {} |", tag.name());
    }
    out.push_str("\n|---|---|---|---|---|---|\n");
    for summary in &report.summaries {
        let _ = write!(out, "| {} |", summary.system);
        for (_, answered, recall) in &summary.by_tag {
            let _ = write!(out, " {} (n={answered}) |", show(*recall, 3));
        }
        out.push('\n');
    }
    if let Some(decision) = report.decision {
        let _ = writeln!(
            out,
            "\nGoal 4: Recall@5(zenodotus) - Recall@5(Context7) on {} paired questions: 95% CI [{:.3}, {:.3}]; \
             met when the lower bound is above {MARGIN}: {}.",
            decision.paired,
            decision.difference.low,
            decision.difference.high,
            if decision.met { "met" } else { "not met" }
        );
    }
    out.push_str(
        "\nTokens are estimated as ceil(characters / 4) for every system. Context7 is a hosted service reached over \
         the network, while the other systems run locally, so their latencies are not like for like.\n",
    );
    out
}

/// Writes the raw records of every system that ran as `<system>.jsonl`, the report as `summary.json`, and the text
/// as `summary.txt`, into `dir`.
///
/// # Errors
///
/// [`EvalError::Io`] when a file cannot be written.
pub fn write(dir: &Path, results: &[SystemResult], report: &Report, text: &str) -> Result<(), EvalError> {
    std::fs::create_dir_all(dir).map_err(EvalError::io(dir))?;
    for result in results {
        if let Ok(records) = &result.outcome {
            let lines: String = records
                .iter()
                .map(|record| serde_json::to_string(record).expect("records serialize") + "\n")
                .collect();
            let path = dir.join(format!("{}.jsonl", result.name));
            std::fs::write(&path, lines).map_err(EvalError::io(&path))?;
        }
    }
    let path = dir.join("summary.json");
    let json = serde_json::to_string_pretty(report).expect("reports serialize");
    std::fs::write(&path, json).map_err(EvalError::io(&path))?;
    let path = dir.join("summary.txt");
    std::fs::write(&path, text).map_err(EvalError::io(&path))
}

/// Prints `text` on stdout.
pub fn print(text: &str) {
    print!("{text}");
}

/// Prints `error` on stderr.
pub fn print_error(error: &EvalError) {
    eprintln!("error: {error}");
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::questions::tests::question;
    use crate::run::tests::record;

    fn answered(id: &str, rank: Option<usize>, chars: usize, micros: u64) -> Record {
        record(id, Outcome::Answered { first_hit: rank }, &[Tag::Symbol], chars, micros)
    }

    #[test]
    fn a_summary_scores_only_answered_questions() {
        let records = [
            answered("a", Some(1), 40, 1000),
            answered("b", Some(4), 8, 3000),
            answered("c", Some(7), 9, 2000),
            answered("d", None, 12, 4000),
            record("e", Outcome::VersionUnavailable, &[], 0, 0),
            record("f", Outcome::Failed { error: "x".to_owned() }, &[Tag::Symbol], 0, 0),
        ];
        let summary = summarize("sys", &records);
        assert_eq!((summary.answered, summary.unavailable, summary.failed), (4, 1, 1));
        let recall = summary.recall.unwrap();
        assert!((recall.value - 0.5).abs() < 1e-12);
        let mrr = summary.mrr.unwrap();
        assert!((mrr.value - (1.0 + 0.25 + 1.0 / 7.0) / 4.0).abs() < 1e-12);
        assert_eq!(summary.p50_ms, Some(2.0));
        assert_eq!(summary.p95_ms, Some(4.0));
        assert_eq!(summary.median_tokens, Some(3));
        assert_eq!(summary.by_tag[0], (Tag::Symbol, 4, Some(0.5)));
        assert_eq!(summary.by_tag[1], (Tag::VersionSpecific, 0, None));
        let empty = summarize("none", &[]);
        assert_eq!((empty.recall, empty.p50_ms, empty.median_tokens), (None, None, None));
    }

    #[test]
    fn rankings_are_cut_to_the_budget_and_judged_again() {
        let q = question("a", "tokio", None, &["needle"]);
        let questions = BTreeMap::from([("a", &q)]);
        let mut long = answered("a", Some(2), 0, 1);
        long.sections = vec!["0123456789".to_owned(), "has needle".to_owned(), "needle".to_owned()];
        let cut = at_budget(std::slice::from_ref(&long), &questions, 14);
        assert_eq!(cut[0].sections, ["0123456789", "has "]);
        assert_eq!(cut[0].raw, "0123456789has ");
        assert_eq!((cut[0].answered(), cut[0].first_hit()), (true, None));
        let whole = at_budget(std::slice::from_ref(&long), &questions, 100);
        assert_eq!(whole[0].first_hit(), Some(2));
        assert_eq!(whole[0].sections.len(), 3);
        let exact = at_budget(std::slice::from_ref(&long), &questions, 20);
        assert_eq!(exact[0].sections, ["0123456789", "has needle"]);
        let unknown = answered("zzz", Some(1), 0, 1);
        let skipped = record("e", Outcome::VersionUnavailable, &[], 0, 0);
        let cut = at_budget(&[unknown, skipped], &questions, 10);
        assert_eq!(cut.len(), 1);
        assert_eq!((cut[0].answered(), cut[0].first_hit()), (true, None));
    }

    #[test]
    fn the_decision_pairs_questions_both_systems_answered() {
        let ours: Vec<Record> = (0..20).map(|n| answered(&format!("q{n}"), Some(1), 1, 1)).collect();
        let mut theirs: Vec<Record> = (0..20)
            .map(|n| answered(&format!("q{n}"), if n < 10 { Some(1) } else { None }, 1, 1))
            .collect();
        theirs[0] = record("q0", Outcome::VersionUnavailable, &[], 0, 0);
        let decision = decide(&ours, &theirs).unwrap();
        assert_eq!(decision.paired, 19);
        assert!(decision.met);
        assert!(decision.difference.low > 0.2, "{decision:?}");
        let decision = decide(&theirs[1..], &ours).unwrap();
        assert!(!decision.met);
        assert_eq!(decide(&ours[..1], &theirs[..1]), None);
    }

    fn results() -> (Vec<Question>, Vec<SystemResult>) {
        let questions: Vec<Question> = (0..4)
            .map(|n| question(&format!("q{n}"), "tokio", None, &["hit"]))
            .collect();
        let records = |ranks: [Option<usize>; 4], chars: usize| -> Vec<Record> {
            ranks
                .iter()
                .enumerate()
                .map(|(n, rank)| {
                    let mut found = answered(&format!("q{n}"), *rank, chars, 1000);
                    found.sections = vec!["hit".repeat(chars / 3)];
                    found
                })
                .collect()
        };
        let results = vec![
            SystemResult {
                name: ZENODOTUS,
                outcome: Ok(records([Some(1), Some(1), Some(2), None], 30)),
            },
            SystemResult {
                name: CONTEXT7,
                outcome: Ok(records([Some(1), None, None, None], 12)),
            },
            SystemResult {
                name: "grounded-docs",
                outcome: Err("npx not found".to_owned()),
            },
        ];
        (questions, results)
    }

    #[test]
    fn a_report_compares_every_system_that_ran() {
        let (questions, results) = results();
        let report = compose(Set::Test, &questions, &results);
        assert_eq!((report.set, report.questions), ("test", 4));
        assert_eq!(report.summaries.len(), 2);
        assert_eq!(
            report.not_run,
            [("grounded-docs".to_owned(), "npx not found".to_owned())]
        );
        assert_eq!(report.budget_chars, Some(12));
        assert_eq!(report.at_budget.len(), 2);
        assert_eq!(report.decision.unwrap().paired, 4);
        let text = render(&report, Path::new("eval/results/x"));
        assert!(text.starts_with("zenodotus-eval: test set, 4 questions\nRaw results: eval/results/x\n\n| System |"));
        assert!(text.contains("| zenodotus | 4 | 0 | 0 | 0.750 ["), "{text}");
        assert!(text.contains("| grounded-docs | not run: npx not found |"), "{text}");
        assert!(
            text.contains("At Context7's median response size (12 characters, about 3 tokens):"),
            "{text}"
        );
        assert!(text.contains("| context7 | 0.250 (n=4) | - (n=0) |"), "{text}");
        assert!(text.contains("\nGoal 4: Recall@5(zenodotus) - Recall@5(Context7) on 4 paired questions: 95% CI ["));
        assert!(text.contains("are not like for like.\n"), "{text}");

        let alone = compose(Set::Dev, &questions, &results[..1]);
        assert_eq!((alone.budget_chars, alone.decision), (None, None));
        let text = render(&alone, Path::new("r"));
        assert!(!text.contains("At Context7's"));
        assert!(!text.contains("Goal 4"));
        let mut unscored = summarize("none", &[]);
        unscored.system = "none".to_owned();
        assert!(table(&[unscored]).ends_with("| none | 0 | 0 | 0 | - | - | - | - | - |\n"));
    }

    #[test]
    fn a_failing_decision_reads_not_met() {
        let (questions, mut results) = results();
        results.swap(0, 1);
        results[0].name = ZENODOTUS;
        results[1].name = CONTEXT7;
        let report = compose(Set::Test, &questions, &results);
        let text = render(&report, Path::new("r"));
        assert!(text.contains(": not met."), "{text}");
    }

    #[test]
    fn results_are_written_as_jsonl_and_summaries() {
        let (questions, results) = results();
        let report = compose(Set::Test, &questions, &results);
        let dir = TempDir::new().unwrap();
        let out = dir.path().join("run");
        write(&out, &results, &report, "text").unwrap();
        let zenodotus = std::fs::read_to_string(out.join("zenodotus.jsonl")).unwrap();
        assert_eq!(zenodotus.lines().count(), 4);
        assert!(!out.join("grounded-docs.jsonl").exists());
        let summary: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out.join("summary.json")).unwrap()).unwrap();
        assert_eq!(summary["summaries"][0]["system"], "zenodotus");
        assert_eq!(std::fs::read_to_string(out.join("summary.txt")).unwrap(), "text");

        let blocked = dir.path().join("file");
        std::fs::write(&blocked, "").unwrap();
        let error = write(&blocked, &results, &report, "text").unwrap_err();
        assert!(matches!(error, EvalError::Io { .. }), "{error}");
        for name in ["zenodotus.jsonl", "summary.json", "summary.txt"] {
            let out = dir.path().join(format!("blocked-{name}"));
            std::fs::create_dir_all(out.join(name)).unwrap();
            let error = write(&out, &results, &report, "text").unwrap_err();
            assert!(error.to_string().contains(name), "{error}");
        }
    }

    #[test]
    fn printing_goes_to_the_standard_streams() {
        print("");
        print_error(&EvalError::Invalid(0));
    }
}
