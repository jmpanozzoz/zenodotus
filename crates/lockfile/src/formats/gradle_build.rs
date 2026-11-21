//! `build.gradle` and `build.gradle.kts`, read as text with patterns and never evaluated: quoted `"g:a:v"` strings
//! passed to a dependency configuration.

use zenodotus_core::Ecosystem;

use super::gradle_lockfile::coordinate;
use crate::Entry;

/// The quoted strings of `text`, in `'…'` or `"…"`; an unterminated one is dropped.
pub(crate) fn quoted(text: &str) -> Vec<&str> {
    let mut strings = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(['\'', '"']) {
        let quote = &rest[start..=start];
        let Some((string, after)) = rest[start + 1..].split_once(quote) else {
            break;
        };
        strings.push(string);
        rest = after;
    }
    strings
}

/// Every coordinate string on a line that starts with a configuration name such as `implementation`; a version
/// built from a variable (`$x`) is left out, and a group or artifact built from one is not a coordinate.
pub(crate) fn parse(text: &str) -> Vec<Entry> {
    text.lines()
        .map(str::trim_start)
        .filter(|line| line.starts_with(|character: char| character.is_ascii_alphabetic()))
        .flat_map(quoted)
        .filter_map(coordinate)
        .map(|(name, version)| {
            let version = (!version.contains('$')).then_some(version);
            Entry::registry(Ecosystem::Maven, &name, version)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Entry;
    use zenodotus_core::Ecosystem;

    const GROOVY: &str = include_str!("../../../../fixtures/lockfiles/gradle/build.gradle");
    const KOTLIN: &str = include_str!("../../../../fixtures/lockfiles/gradle/build.gradle.kts");

    fn maven(name: &str, version: Option<&str>) -> Entry {
        Entry::registry(Ecosystem::Maven, name, version)
    }

    #[test]
    fn string_notation_in_dependency_configurations_maps_to_entries() {
        assert_eq!(
            parse(GROOVY),
            [
                maven("com.google.guava:guava", Some("33.2.1-jre")),
                maven("org.slf4j:slf4j-api", Some("2.0.13")),
                maven("org.junit:junit-bom", Some("5.10.2")),
                maven("com.squareup.okhttp3:okhttp", None),
                maven("org.projectlombok:lombok", Some("1.18.32")),
            ]
        );
        assert_eq!(
            parse(KOTLIN),
            [
                maven("io.ktor:ktor-client-core", Some("2.3.11")),
                maven("io.kotest:kotest-runner-junit5", Some("5.9.0")),
            ]
        );
    }

    #[test]
    fn an_unterminated_string_is_ignored() {
        assert_eq!(parse("implementation 'a:b:1.0").len(), 0);
    }
}
