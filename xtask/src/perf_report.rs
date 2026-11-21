//! The parsing behind `cargo xtask perf`'s report: the fields of a GNU `time -v` report, the counts of
//! `zenodotus status`, and which of a run's files are its own. Kept apart from `perf.rs`, which only drives tools and
//! is excluded from coverage, so that every number the report computes is tested.

/// The name of the `time -v` report of the sync named `name`.
pub fn time_report(name: &str) -> String {
    format!("{name}-time.txt")
}

/// Whether the file `name` in `target/perf/` belongs to this run: all do, except the initial sync's `time -v` report
/// when the run did not sync, since an earlier run wrote it.
pub fn belongs_to_run(name: &str, synced: bool) -> bool {
    synced || name != time_report("sync")
}

/// The value after `label` in a GNU `time -v` report.
pub fn time_field<'a>(report: &'a str, label: &str) -> Option<&'a str> {
    report
        .lines()
        .find_map(|line| line.trim().strip_prefix(label))
        .map(str::trim)
}

/// The document and section counts `zenodotus status` reports, from its `…, N documents, M sections` line.
pub fn counts(status: &str) -> Option<(u32, u32)> {
    status.lines().find_map(|line| {
        let (before, after) = line.split_once(" documents, ")?;
        let documents = before.rsplit(' ').next()?.parse().ok()?;
        let sections = after.strip_suffix(" sections")?.parse().ok()?;
        Some((documents, sections))
    })
}

/// User plus system CPU seconds in a GNU `time -v` report.
pub fn cpu_seconds(report: &str) -> Option<f64> {
    let field = |label| time_field(report, label)?.parse::<f64>().ok();
    Some(field("User time (seconds):")? + field("System time (seconds):")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIME: &str = "\tCommand being timed: \"zenodotus sync\"\n\tUser time (seconds): 4.25\n\tSystem time \
                        (seconds): 0.75\n\tMaximum resident set size (kbytes): 91234\n";

    #[test]
    fn a_run_keeps_the_sync_report_only_when_it_synced() {
        assert_eq!(time_report("resync"), "resync-time.txt");
        assert!(belongs_to_run("sync-time.txt", true));
        assert!(!belongs_to_run("sync-time.txt", false));
        assert!(belongs_to_run("resync-time.txt", false));
        assert!(belongs_to_run("summary.txt", false));
    }

    #[test]
    fn time_fields_are_read_by_label_and_cpu_time_adds_user_and_system() {
        assert_eq!(time_field(TIME, "Maximum resident set size (kbytes):"), Some("91234"));
        assert_eq!(time_field(TIME, "Elapsed (wall clock) time:"), None);
        assert_eq!(cpu_seconds(TIME), Some(5.0));
        assert_eq!(cpu_seconds("\tUser time (seconds): 4.25\n"), None);
        assert_eq!(
            cpu_seconds("\tUser time (seconds): x\n\tSystem time (seconds): 1\n"),
            None
        );
    }

    #[test]
    fn counts_come_from_the_status_line() {
        let status = "Library mdn\nmdn@main: ok, revision 3, 14012 documents, 90876 sections\n";
        assert_eq!(counts(status), Some((14012, 90876)));
        assert_eq!(counts("mdn@main: ok, revision 3, x documents, 2 sections"), None);
        assert_eq!(
            counts("mdn@main: ok, revision 3, 1 documents, 2 sections, 3 errors"),
            None
        );
        assert_eq!(counts("nothing here"), None);
    }
}
