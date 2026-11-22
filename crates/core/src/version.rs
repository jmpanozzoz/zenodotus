//! Version ordering per ecosystem (design §7.2) and matching a requested version against a version list (§7.1).

use std::cmp::Ordering;
use std::str::FromStr;

use crate::ecosystem::Ecosystem;

/// One version of a registry's version list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// The version as the registry writes it.
    pub version: String,
    /// Whether the registry withdrew it: yanked, retracted (Go), unlisted (`NuGet`) or deprecated (npm).
    pub withdrawn: bool,
}

/// Compares two versions of `ecosystem` by its ordering (design §7.2); `None` when either does not parse.
#[must_use]
pub fn compare(ecosystem: Ecosystem, a: &str, b: &str) -> Option<Ordering> {
    match ecosystem {
        Ecosystem::Npm | Ecosystem::Crates | Ecosystem::Pub => Some(semver(a)?.cmp_precedence(&semver(b)?)),
        Ecosystem::Go => Some(go(a)?.cmp_precedence(&go(b)?)),
        Ecosystem::Github | Ecosystem::Swift => Some(tag(a)?.cmp_precedence(&tag(b)?)),
        Ecosystem::Nuget => Some(nuget(a)?.cmp(&nuget(b)?)),
        Ecosystem::Packagist => Some(packagist(a)?.cmp(&packagist(b)?)),
        Ecosystem::Pypi => Some(pep440(a)?.cmp(&pep440(b)?)),
        Ecosystem::Maven => Some(cmp_list(&maven(a)?, &maven(b)?)),
        Ecosystem::Rubygems => Some(cmp_gems(&gem(a)?, &gem(b)?)),
        Ecosystem::Runtime => Some(cmp_numbers(&runtime(a)?, &runtime(b)?)),
    }
}

/// Whether `version` is a prerelease in `ecosystem`. A version that does not parse counts as one, so that it is
/// never chosen as the latest stable version.
#[must_use]
pub fn is_prerelease(ecosystem: Ecosystem, version: &str) -> bool {
    match ecosystem {
        Ecosystem::Npm | Ecosystem::Crates | Ecosystem::Pub => semver(version).is_none_or(|v| !v.pre.is_empty()),
        Ecosystem::Go => go(version).is_none_or(|v| !v.pre.is_empty()),
        Ecosystem::Github | Ecosystem::Swift => tag(version).is_none_or(|v| !v.pre.is_empty()),
        Ecosystem::Nuget => nuget(version).is_none_or(|v| v.pre.is_some()),
        Ecosystem::Packagist => packagist(version).is_none_or(|v| v.stability < STABLE),
        Ecosystem::Pypi => pep440(version).is_none_or(|v| v.any_prerelease()),
        Ecosystem::Maven => maven(version).is_none_or(|items| has_prerelease_qualifier(&items)),
        Ecosystem::Rubygems => gem(version).is_none_or(|segments| segments.iter().any(|s| matches!(s, Gem::Text(_)))),
        Ecosystem::Runtime => runtime(version).is_none(),
    }
}

/// The newest version that is neither withdrawn nor a prerelease (design §7.1 step 4).
#[must_use]
pub fn latest_stable(ecosystem: Ecosystem, listed: &[Listed]) -> Option<&str> {
    newest(
        ecosystem,
        listed
            .iter()
            .filter(|l| !l.withdrawn && !is_prerelease(ecosystem, &l.version))
            .map(|l| l.version.as_str()),
    )
}

/// The version a requested `version` selects (design §7.1): an exact match, else the newest version it is a
/// whole-component prefix of (`4.2` matches `4.2.16`, never `4.20.0`), stable ones first and withdrawn ones never.
///
/// # Errors
/// Up to five nearest listed versions when nothing matches.
pub fn select<'a>(ecosystem: Ecosystem, version: &str, listed: &'a [Listed]) -> Result<&'a str, Vec<String>> {
    if let Some(exact) = listed.iter().find(|l| l.version == version) {
        return Ok(&exact.version);
    }
    let prefix = format!("{version}.");
    let matches: Vec<&str> = listed
        .iter()
        .filter(|l| !l.withdrawn && l.version.starts_with(&prefix))
        .map(|l| l.version.as_str())
        .collect();
    let stable = matches.iter().copied().filter(|v| !is_prerelease(ecosystem, v));
    newest(ecosystem, stable)
        .or_else(|| newest(ecosystem, matches.iter().copied()))
        .ok_or_else(|| nearest(ecosystem, version, listed))
}

/// Maps a runtime's tag to its version (design §7.7): `go1.22.5`, `v3_3_6`, `jdk-21.0.5-ga`, `swift-6.0.3-RELEASE`
/// and `v20.11.1` give dotted numbers. `None` for any other form, such as a release candidate.
#[must_use]
pub fn runtime_tag_version(tag: &str) -> Option<String> {
    let inner = tag
        .strip_prefix("go")
        .or_else(|| tag.strip_prefix("jdk-").and_then(|rest| rest.strip_suffix("-ga")))
        .or_else(|| {
            tag.strip_prefix("swift-")
                .and_then(|rest| rest.strip_suffix("-RELEASE"))
        })
        .or_else(|| tag.strip_prefix('v'))
        .unwrap_or(tag)
        .replace('_', ".");
    numbers(&inner).map(|_| inner)
}

/// The newest of `versions` by the ecosystem's ordering; versions that do not parse are skipped.
fn newest<'a>(ecosystem: Ecosystem, versions: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    sorted(ecosystem, versions).pop()
}

/// The versions that parse, in ascending order.
fn sorted<'a>(ecosystem: Ecosystem, versions: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut parsed: Vec<&str> = versions.filter(|v| compare(ecosystem, v, v).is_some()).collect();
    parsed.sort_by(|a, b| compare(ecosystem, a, b).unwrap_or(Ordering::Equal));
    parsed
}

/// Up to five listed versions nearest to `version`, alternating below and above it, the closest first; the newest
/// ones when `version` does not parse.
fn nearest(ecosystem: Ecosystem, version: &str, listed: &[Listed]) -> Vec<String> {
    const LIMIT: usize = 5;
    let ascending = sorted(ecosystem, listed.iter().map(|l| l.version.as_str()));
    if compare(ecosystem, version, version).is_none() {
        return ascending.iter().rev().take(LIMIT).map(|v| (*v).to_owned()).collect();
    }
    let split = ascending
        .iter()
        .take_while(|v| compare(ecosystem, v, version) == Some(Ordering::Less))
        .count();
    let (mut below, mut above) = (ascending[..split].iter().rev(), ascending[split..].iter());
    let mut found = Vec::new();
    while found.len() < LIMIT {
        let next = [below.next(), above.next()];
        if next == [None, None] {
            break;
        }
        found.extend(next.into_iter().flatten().map(|v| (*v).to_owned()));
    }
    found.truncate(LIMIT);
    found
}

/// A semantic 2.0 version.
fn semver(version: &str) -> Option<semver::Version> {
    semver::Version::parse(version).ok()
}

/// A Go module version: semantic version with a `v` prefix, `+incompatible` stripped; pseudo-versions parse as prereleases.
fn go(version: &str) -> Option<semver::Version> {
    let version = version.strip_prefix('v')?;
    semver(version.strip_suffix("+incompatible").unwrap_or(version))
}

/// A git tag in semantic version order, with an optional `v` prefix.
fn tag(version: &str) -> Option<semver::Version> {
    semver(version.strip_prefix('v').unwrap_or(version))
}

/// A PEP 440 version.
fn pep440(version: &str) -> Option<pep440_rs::Version> {
    pep440_rs::Version::from_str(version).ok()
}

/// Dotted numeric components, each non-empty.
fn numbers(text: &str) -> Option<Vec<u64>> {
    text.split('.')
        .map(|part| {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            part.parse().ok()
        })
        .collect()
}

/// Compares numeric components left to right, a missing component counting as 0.
fn cmp_numbers(a: &[u64], b: &[u64]) -> Ordering {
    let len = a.len().max(b.len());
    let at = |parts: &[u64], i: usize| parts.get(i).copied().unwrap_or(0);
    (0..len)
        .map(|i| at(a, i).cmp(&at(b, i)))
        .find(|order| order.is_ne())
        .unwrap_or(Ordering::Equal)
}

/// A runtime version, after its tag form is mapped.
fn runtime(version: &str) -> Option<Vec<u64>> {
    numbers(&runtime_tag_version(version)?)
}

/// A `NuGet` version: up to four numeric parts and a case-insensitive semantic prerelease label.
#[derive(Debug, PartialEq, Eq)]
struct Nuget {
    parts: [u64; 4],
    pre: Option<semver::Prerelease>,
}

impl Ord for Nuget {
    fn cmp(&self, other: &Self) -> Ordering {
        self.parts
            .cmp(&other.parts)
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl PartialOrd for Nuget {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Parses a `NuGet` version, ignoring build metadata.
fn nuget(version: &str) -> Option<Nuget> {
    let version = version.split('+').next().unwrap_or_default();
    let (release, pre) = match version.split_once('-') {
        Some((release, pre)) => (release, Some(semver::Prerelease::new(&pre.to_ascii_lowercase()).ok()?)),
        None => (version, None),
    };
    if pre.as_ref().is_some_and(semver::Prerelease::is_empty) {
        return None;
    }
    let release = numbers(release)?;
    let mut parts = [0; 4];
    if release.len() > parts.len() {
        return None;
    }
    parts[..release.len()].copy_from_slice(&release);
    Some(Nuget { parts, pre })
}

/// The rank of a stable Composer version; lower ranks are prereleases, the higher one is a patch release.
const STABLE: u8 = 4;

/// A Composer version: four numeric parts, a stability rank and its number.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Packagist {
    parts: [u64; 4],
    stability: u8,
    number: u64,
}

/// Parses a Composer version such as `v1.2.3`, `1.0.0-beta2`, `1.0.0RC1` or `1.0.0-p1`.
fn packagist(version: &str) -> Option<Packagist> {
    let version = version.strip_prefix(['v', 'V']).unwrap_or(version);
    let split = version
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(version.len());
    let (release, suffix) = version.split_at(split);
    let release = numbers(release)?;
    let mut parts = [0; 4];
    if release.len() > parts.len() {
        return None;
    }
    parts[..release.len()].copy_from_slice(&release);
    if suffix.is_empty() {
        return Some(Packagist {
            parts,
            stability: STABLE,
            number: 0,
        });
    }
    let suffix = suffix
        .strip_prefix(['-', '.', '_'])
        .unwrap_or(suffix)
        .to_ascii_lowercase();
    let letters = suffix.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(suffix.len());
    let (word, rest) = suffix.split_at(letters);
    let stability = match word {
        "dev" => 0,
        "alpha" | "a" => 1,
        "beta" | "b" => 2,
        "rc" => 3,
        "patch" | "pl" | "p" => STABLE + 1,
        _ => return None,
    };
    let digits = rest.strip_prefix('.').unwrap_or(rest);
    let number = if digits.is_empty() {
        0
    } else {
        numbers(digits).filter(|n| n.len() == 1)?[0]
    };
    Some(Packagist {
        parts,
        stability,
        number,
    })
}

/// One item of a Maven `ComparableVersion`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    /// A number, without leading zeros; empty for 0.
    Int(String),
    /// A qualifier, after its aliases are applied; empty for a release.
    Text(String),
    /// The items after a `-` or a digit-letter transition.
    List(Vec<Item>),
}

/// Maven's known qualifiers, in order; an empty qualifier is a release.
const QUALIFIERS: [&str; 7] = ["alpha", "beta", "milestone", "rc", "snapshot", "", "sp"];
/// The index of the release qualifier in [`QUALIFIERS`].
const RELEASE: usize = 5;

impl Item {
    /// A number item.
    fn int(digits: &str) -> Self {
        Self::Int(digits.trim_start_matches('0').to_owned())
    }

    /// A qualifier item; a single letter followed by a digit abbreviates `alpha`, `beta` or `milestone`.
    fn text(value: &str, followed_by_digit: bool) -> Self {
        let value = match (followed_by_digit, value) {
            (true, "a") => "alpha",
            (true, "b") => "beta",
            (true, "m") => "milestone",
            (_, "ga" | "final" | "release") => "",
            (_, "cr") => "rc",
            (_, other) => other,
        };
        Self::Text(value.to_owned())
    }

    /// Whether the item equals the absence of an item: 0, a release qualifier or an empty list.
    fn is_null(&self) -> bool {
        match self {
            Self::Int(digits) | Self::Text(digits) => digits.is_empty(),
            Self::List(items) => items.is_empty(),
        }
    }
}

/// The sort key of a qualifier: its index among the known ones, else after them in text order.
fn qualifier_key(qualifier: &str) -> String {
    QUALIFIERS
        .iter()
        .position(|known| *known == qualifier)
        .map_or_else(|| format!("{}-{qualifier}", QUALIFIERS.len()), |i| i.to_string())
}

/// Parses a Maven version into its `ComparableVersion` items.
fn maven(version: &str) -> Option<Vec<Item>> {
    if version.is_empty() {
        return None;
    }
    let version = version.to_ascii_lowercase();
    let mut stack: Vec<Vec<Item>> = vec![Vec::new()];
    let mut is_digit = false;
    let mut start = 0;
    let parse = |is_digit: bool, text: &str| {
        if is_digit {
            Item::int(text)
        } else {
            Item::text(text, false)
        }
    };
    for (i, c) in version.bytes().enumerate() {
        let current = stack.len() - 1;
        if c == b'.' || c == b'-' {
            let item = if i == start {
                Item::int("0")
            } else {
                parse(is_digit, &version[start..i])
            };
            stack[current].push(item);
            start = i + 1;
            if c == b'-' {
                stack.push(Vec::new());
            }
        } else if c.is_ascii_digit() != is_digit && i > start {
            let item = if is_digit {
                Item::int(&version[start..i])
            } else {
                Item::text(&version[start..i], true)
            };
            stack[current].push(item);
            start = i;
            stack.push(Vec::new());
            is_digit = c.is_ascii_digit();
        } else {
            is_digit = c.is_ascii_digit();
        }
    }
    if version.len() > start {
        let current = stack.len() - 1;
        stack[current].push(parse(is_digit, &version[start..]));
    }
    let mut list = Vec::new();
    while let Some(mut items) = stack.pop() {
        if !list.is_empty() || !items.is_empty() || stack.is_empty() {
            items.push(Item::List(std::mem::take(&mut list)));
        }
        normalize(&mut items);
        list = items;
    }
    Some(list)
}

/// Removes trailing null items, stopping at the first non-null item that is not a list.
fn normalize(items: &mut Vec<Item>) {
    for i in (0..items.len()).rev() {
        if items[i].is_null() {
            items.remove(i);
        } else if !matches!(items[i], Item::List(_)) {
            break;
        }
    }
}

/// Compares two numbers written without leading zeros.
fn cmp_digits(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// Maven's `Item.compareTo`, where `None` is the absence of an item.
fn cmp_item(left: &Item, right: Option<&Item>) -> Ordering {
    match (left, right) {
        (Item::Int(a), None) => cmp_digits(a, ""),
        (Item::Int(a), Some(Item::Int(b))) => cmp_digits(a, b),
        (Item::Int(_), Some(_)) | (Item::List(_), Some(Item::Text(_))) => Ordering::Greater,
        (Item::Text(a), None) => qualifier_key(a).cmp(&RELEASE.to_string()),
        (Item::Text(a), Some(Item::Text(b))) => qualifier_key(a).cmp(&qualifier_key(b)),
        (Item::Text(_), Some(_)) | (Item::List(_), Some(Item::Int(_))) => Ordering::Less,
        (Item::List(items), None) => items.first().map_or(Ordering::Equal, |first| cmp_item(first, None)),
        (Item::List(a), Some(Item::List(b))) => cmp_list(a, b),
    }
}

/// Compares two item lists element by element.
fn cmp_list(a: &[Item], b: &[Item]) -> Ordering {
    (0..a.len().max(b.len()))
        .map(|i| match (a.get(i), b.get(i)) {
            (Some(left), right) => cmp_item(left, right),
            (None, right) => right.map_or(Ordering::Equal, |right| cmp_item(right, None).reverse()),
        })
        .find(|order| order.is_ne())
        .unwrap_or(Ordering::Equal)
}

/// Whether any qualifier sorts before a release: `alpha`, `beta`, `milestone`, `rc` or `snapshot`.
fn has_prerelease_qualifier(items: &[Item]) -> bool {
    items.iter().any(|item| match item {
        Item::Int(_) => false,
        Item::Text(qualifier) => QUALIFIERS[..RELEASE].contains(&qualifier.as_str()),
        Item::List(inner) => has_prerelease_qualifier(inner),
    })
}

/// One segment of a `Gem::Version`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Gem {
    /// A number.
    Number(u64),
    /// Letters, which make the version a prerelease.
    Text(String),
}

/// Parses a `Gem::Version` into its canonical segments: `-` reads as `.pre.`, and trailing zeros of the release
/// part and of the prerelease part are dropped.
fn gem(version: &str) -> Option<Vec<Gem>> {
    let version = version.replace('-', ".pre.");
    if !version.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let mut segments = Vec::new();
    for part in version.split('.') {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        let mut rest = part;
        while !rest.is_empty() {
            let digits = rest.starts_with(|c: char| c.is_ascii_digit());
            let end = rest.find(|c: char| c.is_ascii_digit() != digits).unwrap_or(rest.len());
            let (run, tail) = rest.split_at(end);
            segments.push(if digits {
                Gem::Number(run.parse().ok()?)
            } else {
                Gem::Text(run.to_owned())
            });
            rest = tail;
        }
    }
    let split = segments
        .iter()
        .position(|s| matches!(s, Gem::Text(_)))
        .unwrap_or(segments.len());
    let mut prerelease = segments.split_off(split);
    for part in [&mut segments, &mut prerelease] {
        while part.last() == Some(&Gem::Number(0)) {
            part.pop();
        }
    }
    segments.append(&mut prerelease);
    Some(segments)
}

/// `Gem::Version#<=>` over canonical segments, missing segments counting as 0.
fn cmp_gems(a: &[Gem], b: &[Gem]) -> Ordering {
    const ZERO: Gem = Gem::Number(0);
    (0..a.len().max(b.len()))
        .map(|i| match (a.get(i).unwrap_or(&ZERO), b.get(i).unwrap_or(&ZERO)) {
            (Gem::Number(x), Gem::Number(y)) => x.cmp(y),
            (Gem::Text(x), Gem::Text(y)) => x.cmp(y),
            (Gem::Number(_), Gem::Text(_)) => Ordering::Greater,
            (Gem::Text(_), Gem::Number(_)) => Ordering::Less,
        })
        .find(|order| order.is_ne())
        .unwrap_or(Ordering::Equal)
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::*;

    /// Asserts that every version in `ascending` sorts strictly before the next, in both directions.
    fn ascending(ecosystem: Ecosystem, ascending: &[&str]) {
        assert!(ascending.len() > 10, "at least 10 pairs per ecosystem");
        for pair in ascending.windows(2) {
            assert_eq!(
                compare(ecosystem, pair[0], pair[1]),
                Some(Ordering::Less),
                "{ecosystem}: {pair:?}"
            );
            assert_eq!(
                compare(ecosystem, pair[1], pair[0]),
                Some(Ordering::Greater),
                "{ecosystem}: {pair:?}"
            );
        }
    }

    fn equal(ecosystem: Ecosystem, a: &str, b: &str) {
        assert_eq!(
            compare(ecosystem, a, b),
            Some(Ordering::Equal),
            "{ecosystem}: {a} = {b}"
        );
    }

    #[test]
    fn semver_ecosystems_order_prereleases_first() {
        let order = [
            "0.9.9",
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
            "1.9.0",
            "1.10.0",
            "2.0.0",
        ];
        for ecosystem in [Ecosystem::Npm, Ecosystem::Crates, Ecosystem::Pub] {
            ascending(ecosystem, &order);
            equal(ecosystem, "1.0.0+build.1", "1.0.0+build.2");
            assert_eq!(compare(ecosystem, "1.0", "1.0.0"), None);
        }
    }

    #[test]
    fn go_orders_pseudo_versions_as_prereleases_and_strips_incompatible() {
        ascending(
            Ecosystem::Go,
            &[
                "v0.0.0-20230101000000-aaaaaaaaaaaa",
                "v0.0.0-20240101000000-4ea0e648e38a",
                "v0.0.1",
                "v1.0.0-rc.1",
                "v1.0.0",
                "v1.2.3",
                "v1.2.4-0.20240101000000-abcdef123456",
                "v1.2.4",
                "v1.9.9",
                "v1.10.0",
                "v2.0.0+incompatible",
                "v2.0.1+incompatible",
            ],
        );
        equal(Ecosystem::Go, "v2.0.0", "v2.0.0+incompatible");
        assert_eq!(compare(Ecosystem::Go, "1.0.0", "v1.0.0"), None);
    }

    #[test]
    fn nuget_has_a_fourth_part_and_case_insensitive_labels() {
        ascending(
            Ecosystem::Nuget,
            &[
                "1.0",
                "1.0.0.1",
                "1.0.1-alpha",
                "1.0.1-beta",
                "1.0.1-beta.2",
                "1.0.1-beta.10",
                "1.0.1-rc.1",
                "1.0.1",
                "1.2.3.4",
                "1.2.3.5",
                "9.0.0",
                "10.0.0",
                "13.0.3",
            ],
        );
        equal(Ecosystem::Nuget, "2.0.0.0", "2.0.0");
        equal(Ecosystem::Nuget, "1.0.0-Beta", "1.0.0-beta");
        equal(Ecosystem::Nuget, "1.0.0+meta", "1.0.0");
        assert!(nuget("1.0.0-rc.1").unwrap() < nuget("1.0.0").unwrap());
        for bad in ["", "1.2.3.4.5", "a.b", "1..2", "1.0.0-"] {
            assert_eq!(compare(Ecosystem::Nuget, bad, "1.0.0"), None, "{bad:?}");
        }
    }

    #[test]
    fn packagist_orders_stability_suffixes() {
        ascending(
            Ecosystem::Packagist,
            &[
                "0.9.0",
                "1.0.0-dev",
                "1.0.0-alpha",
                "1.0.0-alpha2",
                "1.0.0-a3",
                "1.0.0-beta1",
                "1.0.0-b2",
                "1.0.0-RC1",
                "1.0.0",
                "1.0.0-p1",
                "1.0.0-patch2",
                "1.0.0.1",
                "3.8.1",
                "3.10.0",
            ],
        );
        equal(Ecosystem::Packagist, "v2.0.0", "2.0.0");
        equal(Ecosystem::Packagist, "2.0", "2.0.0.0");
        equal(Ecosystem::Packagist, "1.0.0-rc.1", "1.0.0RC1");
        for bad in ["dev-master", "1.0.0-gamma", "1.2.3.4.5", "1.0.0-beta1x", ""] {
            assert_eq!(compare(Ecosystem::Packagist, bad, "1.0.0"), None, "{bad:?}");
        }
    }

    #[test]
    fn pypi_follows_pep_440() {
        ascending(
            Ecosystem::Pypi,
            &[
                "0.9",
                "1.0.dev1",
                "1.0a1",
                "1.0a2",
                "1.0b1",
                "1.0rc1",
                "1.0",
                "1.0.post1",
                "1.1",
                "2.0",
                "10.0",
                "1!0.1",
            ],
        );
        equal(Ecosystem::Pypi, "1.0", "1.0.0");
        assert_eq!(compare(Ecosystem::Pypi, "not a version", "1.0"), None);
    }

    #[test]
    fn maven_follows_comparable_version() {
        ascending(
            Ecosystem::Maven,
            &[
                "1.0-alpha-1",
                "1.0-alpha-2",
                "1.0-beta-1",
                "1.0-milestone-1",
                "1.0-rc-1",
                "1.0-SNAPSHOT",
                "1.0",
                "1.0-sp-1",
                "1.0-whatever",
                "1.0.1",
                "6.1.14",
                "6.10.0",
                "6.10.0.1",
            ],
        );
        for (a, b) in [
            ("1.0", "1.0.0"),
            ("1.0", "1.0-final"),
            ("1.0", "1.0-ga"),
            ("1.0", "1.0-release"),
            ("1.0-a1", "1.0-alpha-1"),
            ("1.0-b1", "1.0-beta-1"),
            ("1.0-m1", "1.0-milestone-1"),
            ("1.0-cr1", "1.0-rc-1"),
            ("1.0..0", "1.0"),
            ("1-", "1"),
            ("2.0.0", "02.0"),
            ("1.0-RC1", "1.0-rc1"),
            ("1a1", "1-alpha-1"),
        ] {
            equal(Ecosystem::Maven, a, b);
        }
        assert_eq!(compare(Ecosystem::Maven, "1.0-1", "1.0"), Some(Ordering::Greater));
        assert_eq!(compare(Ecosystem::Maven, "1.0-1", "1.0-alpha"), Some(Ordering::Greater));
        assert_eq!(compare(Ecosystem::Maven, "1.0-alpha", "1.0-1"), Some(Ordering::Less));
        assert_eq!(compare(Ecosystem::Maven, "1.0-1", "1.0.1"), Some(Ordering::Less));
        assert_eq!(compare(Ecosystem::Maven, "1.0-sp", "1.0-1"), Some(Ordering::Less));
        assert_eq!(compare(Ecosystem::Maven, "1.0.1", "1.0-1"), Some(Ordering::Greater));
        assert_eq!(compare(Ecosystem::Maven, "1-alpha", "1-alpha-0"), Some(Ordering::Equal));
        assert_eq!(compare(Ecosystem::Maven, "1-1", "1-alpha"), Some(Ordering::Greater));
        assert_eq!(compare(Ecosystem::Maven, "1-1.1", "1-1"), Some(Ordering::Greater));
        assert_eq!(compare(Ecosystem::Maven, "1-1", "1-1.1"), Some(Ordering::Less));
        assert_eq!(compare(Ecosystem::Maven, "1.a", "1-1"), Some(Ordering::Less));
        assert_eq!(compare(Ecosystem::Maven, "1-1", "1.a"), Some(Ordering::Greater));
        assert_eq!(
            compare(Ecosystem::Maven, "99999999999999999999999", "99999999999999999999998"),
            Some(Ordering::Greater)
        );
        assert_eq!(compare(Ecosystem::Maven, "", "1"), None);
    }

    #[test]
    fn rubygems_letter_segments_are_prereleases() {
        ascending(
            Ecosystem::Rubygems,
            &[
                "0.9",
                "1.0.a",
                "1.0.b",
                "1.0.beta",
                "1.0.rc",
                "1.0.rc1",
                "1.0",
                "1.0.1",
                "1.9",
                "1.10",
                "2.0.0.pre",
                "2.0.0",
                "7.2.2",
                "7.10.0",
            ],
        );
        for (a, b) in [
            ("1.0", "1.0.0"),
            ("1.0.0.a", "1.0.a"),
            ("1.0-1", "1.0.pre.1"),
            ("2.0.rc.0", "2.0.rc"),
        ] {
            equal(Ecosystem::Rubygems, a, b);
        }
        for bad in ["", "1..0", "1.0 beta", "-1"] {
            assert_eq!(compare(Ecosystem::Rubygems, bad, "1.0"), None, "{bad:?}");
        }
    }

    #[test]
    fn github_and_swift_tags_order_as_semver_where_they_parse() {
        let order = [
            "v0.9.0",
            "v1.0.0-beta",
            "v1.0.0",
            "1.0.1",
            "v1.2.0",
            "5.9.0",
            "5.10.2",
            "v6.0.0-rc.1",
            "v6.0.0",
            "v10.0.0",
            "v10.0.1",
        ];
        for ecosystem in [Ecosystem::Github, Ecosystem::Swift] {
            ascending(ecosystem, &order);
            assert_eq!(compare(ecosystem, "release/1.x", "v1.0.0"), None);
            assert_eq!(compare(ecosystem, "v1.0.0", "main"), None);
        }
    }

    #[test]
    fn runtime_tag_forms_are_mapped_then_compared_numerically() {
        for (tag, version) in [
            ("go1.22.5", "1.22.5"),
            ("v3_3_6", "3.3.6"),
            ("jdk-21.0.5-ga", "21.0.5"),
            ("jdk-21-ga", "21"),
            ("swift-6.0.3-RELEASE", "6.0.3"),
            ("v20.11.1", "20.11.1"),
            ("1.85.1", "1.85.1"),
            ("9", "9"),
        ] {
            assert_eq!(runtime_tag_version(tag).as_deref(), Some(version), "{tag}");
        }
        for not_release in ["go1.23rc1", "v3_4_0_preview1", "jdk-22+35", "latest", "", "v", "1..2"] {
            assert_eq!(runtime_tag_version(not_release), None, "{not_release}");
        }
        let mapped: Vec<String> = [
            "1.9.0",
            "1.10.0",
            "go1.21.0",
            "go1.22.5",
            "1.84.0",
            "1.85",
            "1.85.1",
            "v3_2_9",
            "v3_3_6",
            "swift-5.10.1-RELEASE",
            "swift-6.0.3-RELEASE",
            "8",
            "9",
            "jdk-21-ga",
            "jdk-21.0.4-ga",
            "jdk-21.0.5-ga",
        ]
        .iter()
        .map(|tag| runtime_tag_version(tag).unwrap())
        .collect();
        ascending(
            Ecosystem::Runtime,
            &mapped.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        equal(Ecosystem::Runtime, "1.85", "1.85.0");
        assert_eq!(compare(Ecosystem::Runtime, "latest", "1.0"), None);
    }

    fn listed(versions: &[(&str, bool)]) -> Vec<Listed> {
        versions
            .iter()
            .map(|&(version, withdrawn)| Listed {
                version: version.to_owned(),
                withdrawn,
            })
            .collect()
    }

    #[test]
    fn a_request_matches_exactly_then_on_whole_components() {
        let pypi = listed(&[("4.1.13", false), ("4.2", false), ("4.2.16", false), ("4.20.0", false)]);
        assert_eq!(select(Ecosystem::Pypi, "4.2", &pypi), Ok("4.2"));
        let npm = listed(&[
            ("18.3.1", false),
            ("19.0.0", false),
            ("19.1.0", false),
            ("19.2.0-rc.1", false),
            ("190.0.0", false),
        ]);
        assert_eq!(select(Ecosystem::Npm, "19", &npm), Ok("19.1.0"));
        let django = listed(&[("4.2.16", false), ("4.20.0", false)]);
        assert_eq!(select(Ecosystem::Pypi, "4.2", &django), Ok("4.2.16"));
        assert_eq!(select(Ecosystem::Pypi, "4.20", &django), Ok("4.20.0"));
        let maven = listed(&[("6.1.14", false), ("6.10.0", false)]);
        assert_eq!(select(Ecosystem::Maven, "6.1", &maven), Ok("6.1.14"));
        let only_prerelease = listed(&[("2.0.0-rc.1", false)]);
        assert_eq!(select(Ecosystem::Npm, "2.0.0-rc.1", &only_prerelease), Ok("2.0.0-rc.1"));
        assert_eq!(select(Ecosystem::Npm, "2", &only_prerelease), Ok("2.0.0-rc.1"));
    }

    #[test]
    fn no_match_lists_the_nearest_versions() {
        let npm = listed(&[
            ("1.0.0", false),
            ("2.0.0", false),
            ("3.0.0", false),
            ("4.0.0", false),
            ("5.0.0", false),
            ("6.0.0", false),
            ("7.0.0", false),
        ]);
        assert_eq!(
            select(Ecosystem::Npm, "4.5.0", &npm),
            Err(vec![
                "4.0.0".to_owned(),
                "5.0.0".to_owned(),
                "3.0.0".to_owned(),
                "6.0.0".to_owned(),
                "2.0.0".to_owned()
            ])
        );
        assert_eq!(
            select(Ecosystem::Npm, "0.1.0", &npm),
            Err(["1.0.0", "2.0.0", "3.0.0", "4.0.0", "5.0.0"]
                .map(str::to_owned)
                .to_vec())
        );
        assert_eq!(
            select(
                Ecosystem::Github,
                "nightly",
                &listed(&[("v1.0.0", false), ("v2.0.0", false)])
            ),
            Err(vec!["v2.0.0".to_owned(), "v1.0.0".to_owned()])
        );
        assert_eq!(select(Ecosystem::Npm, "1", &[]), Err(vec![]));
        assert_eq!(
            select(Ecosystem::Npm, "2.5.0", &listed(&[("1.0.0", false), ("3.0.0", false)])),
            Err(vec!["1.0.0".to_owned(), "3.0.0".to_owned()])
        );
    }

    #[test]
    fn latest_stable_skips_prereleases_and_withdrawn_versions() {
        let npm = listed(&[
            ("1.0.0", false),
            ("1.2.0", false),
            ("1.3.0", true),
            ("2.0.0-beta.1", false),
        ]);
        assert_eq!(latest_stable(Ecosystem::Npm, &npm), Some("1.2.0"));
        let go = listed(&[
            ("v1.4.0", false),
            ("v1.5.0", true),
            ("v1.6.0-0.20240101000000-abcdef123456", false),
        ]);
        assert_eq!(latest_stable(Ecosystem::Go, &go), Some("v1.4.0"));
        let pypi = listed(&[("2.31.0", false), ("2.32.0rc1", false), ("2.32.0.dev1", false)]);
        assert_eq!(latest_stable(Ecosystem::Pypi, &pypi), Some("2.31.0"));
        let maven = listed(&[("2.18.2", false), ("2.19.0-rc1", false), ("2.19.0-SNAPSHOT", false)]);
        assert_eq!(latest_stable(Ecosystem::Maven, &maven), Some("2.18.2"));
        let gems = listed(&[("7.2.2", false), ("8.0.0.beta1", false)]);
        assert_eq!(latest_stable(Ecosystem::Rubygems, &gems), Some("7.2.2"));
        let composer = listed(&[("3.8.1", false), ("3.9.0-RC1", false), ("3.8.1-p1", false)]);
        assert_eq!(latest_stable(Ecosystem::Packagist, &composer), Some("3.8.1-p1"));
        let nuget = listed(&[("13.0.3", false), ("13.0.4-beta1", false), ("13.0.5", true)]);
        assert_eq!(latest_stable(Ecosystem::Nuget, &nuget), Some("13.0.3"));
        let tags = listed(&[("main", false), ("v1.0.0", false)]);
        assert_eq!(latest_stable(Ecosystem::Github, &tags), Some("v1.0.0"));
        assert_eq!(latest_stable(Ecosystem::Npm, &listed(&[("1.0.0", true)])), None);
        let runtime = listed(&[("1.85.1", false), ("1.86.0", false)]);
        assert_eq!(latest_stable(Ecosystem::Runtime, &runtime), Some("1.86.0"));
    }

    #[test]
    fn prereleases_per_ecosystem() {
        for (ecosystem, version, expected) in [
            (Ecosystem::Npm, "1.0.0-rc.1", true),
            (Ecosystem::Npm, "1.0.0", false),
            (Ecosystem::Npm, "not semver", true),
            (Ecosystem::Go, "v0.0.0-20240101000000-4ea0e648e38a", true),
            (Ecosystem::Nuget, "1.0.0-beta", true),
            (Ecosystem::Nuget, "1.0.0.1", false),
            (Ecosystem::Packagist, "1.0.0-dev", true),
            (Ecosystem::Packagist, "1.0.0-p1", false),
            (Ecosystem::Packagist, "dev-main", true),
            (Ecosystem::Pypi, "1.0.dev1", true),
            (Ecosystem::Pypi, "1.0.post1", false),
            (Ecosystem::Pypi, "x y", true),
            (Ecosystem::Maven, "1.0-milestone-1", true),
            (Ecosystem::Maven, "1.0-sp-1", false),
            (Ecosystem::Maven, "1.0-1", false),
            (Ecosystem::Maven, "", true),
            (Ecosystem::Rubygems, "1.0.rc1", true),
            (Ecosystem::Rubygems, "1..0", true),
            (Ecosystem::Github, "v1.0.0-beta", true),
            (Ecosystem::Runtime, "1.85.1", false),
            (Ecosystem::Runtime, "latest", true),
        ] {
            assert_eq!(is_prerelease(ecosystem, version), expected, "{ecosystem}: {version}");
        }
    }
}
