//! `gradle.lockfile`: lines `group:artifact:version=configurations`.

use zenodotus_core::Ecosystem;

use crate::Entry;

/// Whether `part` is a Maven group or artifact id, `[A-Za-z0-9._-]+`.
fn is_id(part: &str) -> bool {
    !part.is_empty()
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The `group:artifact` name and the version of a coordinate `group:artifact:version[:classifier][@extension]`.
pub(crate) fn coordinate(text: &str) -> Option<(String, &str)> {
    let text = text.split_once('@').map_or(text, |(coordinate, _)| coordinate);
    let parts: Vec<&str> = text.split(':').collect();
    match parts[..] {
        [group, artifact, version] | [group, artifact, version, _]
            if is_id(group) && is_id(artifact) && !version.is_empty() =>
        {
            Some((format!("{group}:{artifact}"), version))
        }
        _ => None,
    }
}

/// Every locked coordinate, in file order.
pub(crate) fn parse(text: &str) -> Vec<Entry> {
    text.lines()
        .filter_map(|line| coordinate(line.split_once('=')?.0))
        .map(|(name, version)| Entry::registry(Ecosystem::Maven, &name, Some(version)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Entry;
    use zenodotus_core::Ecosystem;

    const SAMPLE: &str = include_str!("../../../../fixtures/lockfiles/gradle/gradle.lockfile");

    #[test]
    fn coordinate_lines_map_to_entries() {
        let maven = |name, version| Entry::registry(Ecosystem::Maven, name, Some(version));
        assert_eq!(
            parse(SAMPLE),
            [
                maven("com.google.guava:failureaccess", "1.0.2"),
                maven("com.google.guava:guava", "33.2.1-jre"),
                maven("org.junit.jupiter:junit-jupiter-api", "5.10.2"),
            ]
        );
    }

    #[test]
    fn every_gradle_lockfile_entry_is_direct() {
        let root = crate::testing::root_with(&[(
            "gradle.lockfile",
            include_str!("../../../../fixtures/lockfiles/gradle/gradle.lockfile"),
        )]);
        crate::testing::assert_all_direct(root.path());
    }
}
