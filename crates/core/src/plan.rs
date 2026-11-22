//! Plans: the sources and revisions a scope was built from, stored in its scope record (design §12.5), and the
//! kebab-case source kinds of recipes and plans (§9.4).

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::error::CoreError;
use crate::ids::sha256_hex;
use crate::model::SourceSpec;

/// The plan JSON's `schema`.
pub const PLAN_SCHEMA: u32 = 1;

/// A source kind, kebab-case in recipes and plans and `snake_case` as a [`SourceSpec`] tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceKind {
    /// Files of a GitHub repository.
    Github,
    /// Pages of a documentation website.
    Website,
    /// An `llms.txt` file and the pages it links to.
    LlmsTxt,
    /// Documents at fixed URLs.
    Url,
    /// A Read the Docs project.
    Rtd,
    /// A crate's docs.rs documentation.
    Rustdoc,
    /// A Go module's pkg.go.dev documentation.
    Godoc,
    /// A Maven artifact's javadoc or sources jar.
    Javadoc,
    /// A `NuGet` package's XML documentation.
    NugetXml,
    /// The `dotnet-api-docs` artifact.
    DotnetApiDocs,
    /// Doc comments of an npm package's type declarations.
    NpmTypes,
    /// A pub.dev documentation tarball.
    Dartdoc,
    /// `DocC` JSON from the Swift Package Index.
    Docc,
    /// Doc comments in a repository's source files.
    SourceComments,
    /// The Dart SDK's API documentation zip, for `runtime:dart` only.
    DartDocsZip,
    /// The Ruby documentation archive, for `runtime:ruby` only.
    RubyDocsArchive,
}

/// Each kind with its plan name and its [`SourceSpec`] tag.
const KINDS: [(SourceKind, &str, &str); 16] = [
    (SourceKind::Github, "github", "github"),
    (SourceKind::Website, "website", "website"),
    (SourceKind::LlmsTxt, "llms-txt", "llms_txt"),
    (SourceKind::Url, "url", "url"),
    (SourceKind::Rtd, "rtd", "rtd"),
    (SourceKind::Rustdoc, "rustdoc", "rustdoc"),
    (SourceKind::Godoc, "godoc", "godoc"),
    (SourceKind::Javadoc, "javadoc", "javadoc"),
    (SourceKind::NugetXml, "nuget-xml", "nuget_xml"),
    (SourceKind::DotnetApiDocs, "dotnet-api-docs", "dotnet_api_docs"),
    (SourceKind::NpmTypes, "npm-types", "npm_types"),
    (SourceKind::Dartdoc, "dartdoc", "dartdoc"),
    (SourceKind::Docc, "docc", "docc"),
    (SourceKind::SourceComments, "source-comments", "source_comments"),
    (SourceKind::DartDocsZip, "dart-docs-zip", "dart_docs_zip"),
    (SourceKind::RubyDocsArchive, "ruby-docs-archive", "ruby_docs_archive"),
];

impl SourceKind {
    /// Every kind: the recipe kinds of design §9.4, then the runtime-only kinds of §11.6.
    pub const ALL: [Self; 16] = [
        Self::Github,
        Self::Website,
        Self::LlmsTxt,
        Self::Url,
        Self::Rtd,
        Self::Rustdoc,
        Self::Godoc,
        Self::Javadoc,
        Self::NugetXml,
        Self::DotnetApiDocs,
        Self::NpmTypes,
        Self::Dartdoc,
        Self::Docc,
        Self::SourceComments,
        Self::DartDocsZip,
        Self::RubyDocsArchive,
    ];

    /// The kebab-case name used in recipes and plans.
    #[must_use]
    pub const fn name(self) -> &'static str {
        KINDS[self as usize].1
    }

    /// The `snake_case` tag of the matching [`SourceSpec`] variant.
    #[must_use]
    pub const fn spec_tag(self) -> &'static str {
        KINDS[self as usize].2
    }

    /// The kind called `name` in recipes and plans.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        KINDS
            .iter()
            .find(|(_, kebab, _)| *kebab == name)
            .map(|(kind, ..)| *kind)
    }

    /// The kind whose [`SourceSpec`] tag is `tag`.
    #[must_use]
    pub fn from_spec_tag(tag: &str) -> Option<Self> {
        KINDS.iter().find(|(.., snake)| *snake == tag).map(|(kind, ..)| *kind)
    }

    /// The kind of a v1 source; `None` for `local`, which no plan holds.
    #[must_use]
    pub const fn of(spec: &SourceSpec) -> Option<Self> {
        match spec {
            SourceSpec::Local { .. } => None,
            SourceSpec::GitHub { .. } => Some(Self::Github),
            SourceSpec::Url { .. } => Some(Self::Url),
            SourceSpec::LlmsTxt { .. } => Some(Self::LlmsTxt),
            SourceSpec::Website { .. } => Some(Self::Website),
        }
    }
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl Serialize for SourceKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for SourceKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::from_name(&name).ok_or_else(|| serde::de::Error::custom(format!("unknown source kind {name:?}")))
    }
}

/// The recipe set a plan came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeRef {
    /// The 40-hex commit of the recipe set, `snapshot` or `builtin`.
    pub commit: String,
    /// Whether a person reviewed the recipe.
    pub reviewed: bool,
}

/// What a source's text was read at.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revision {
    /// A git commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The ref the commit was resolved from.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// A registry version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// A documentation URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Whether the source has text only for its latest version (design §7.5).
    pub latest_only: bool,
    /// The `dotnet-api-docs` moniker, such as `net-9.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moniker: Option<String>,
    /// The candidate directory a discovered `github` source chose (design §10.4). It is an outcome of the job, so the
    /// fingerprint leaves it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
}

/// One source of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSource {
    /// Unique in the plan, recorded on each of its documents.
    pub id: String,
    /// The kind.
    pub kind: SourceKind,
    /// The kind's fields.
    #[serde(default)]
    pub spec: Map<String, Value>,
    /// What the text was read at; `None` before the source is resolved.
    #[serde(default)]
    pub revision: Option<Revision>,
}

impl PlanSource {
    /// The v1 source this plan source runs as.
    ///
    /// # Errors
    /// [`CoreError::InvalidPlan`] when the kind has no [`SourceSpec`] variant yet or the fields do not fit it.
    pub fn to_source_spec(&self) -> Result<SourceSpec, CoreError> {
        let mut fields = self.spec.clone();
        fields.insert("kind".to_owned(), Value::from(self.kind.spec_tag()));
        serde_json::from_value(Value::Object(fields)).map_err(|e| CoreError::InvalidPlan {
            reason: format!("source {:?}: {e}", self.id),
        })
    }
}

/// The sources and revisions of one scope, with the recipe, license and attribution they came with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "PlanRecord", into = "PlanRecord")]
pub struct Plan {
    fingerprint: String,
    recipe: Option<RecipeRef>,
    license: Option<String>,
    attribution: Vec<String>,
    sources: Vec<PlanSource>,
}

/// The stored form of a [`Plan`].
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanRecord {
    schema: u32,
    fingerprint: String,
    recipe: Option<RecipeRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    license: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attribution: Vec<String>,
    sources: Vec<PlanSource>,
}

impl From<Plan> for PlanRecord {
    fn from(plan: Plan) -> Self {
        Self {
            schema: PLAN_SCHEMA,
            fingerprint: plan.fingerprint,
            recipe: plan.recipe,
            license: plan.license,
            attribution: plan.attribution,
            sources: plan.sources,
        }
    }
}

impl TryFrom<PlanRecord> for Plan {
    type Error = String;

    fn try_from(record: PlanRecord) -> Result<Self, String> {
        if record.schema != PLAN_SCHEMA {
            return Err(format!("unsupported plan schema {}", record.schema));
        }
        if fingerprint(&record.sources)? != record.fingerprint {
            return Err("the fingerprint does not match the sources".to_owned());
        }
        Ok(Self {
            fingerprint: record.fingerprint,
            recipe: record.recipe,
            license: record.license,
            attribution: record.attribution,
            sources: record.sources,
        })
    }
}

impl Plan {
    /// A plan over `sources`, with its fingerprint.
    ///
    /// # Errors
    /// [`CoreError::InvalidPlan`] when a source's fields hold a number that is not an integer.
    pub fn new(
        recipe: Option<RecipeRef>,
        license: Option<String>,
        attribution: Vec<String>,
        sources: Vec<PlanSource>,
    ) -> Result<Self, CoreError> {
        let fingerprint = fingerprint(&sources).map_err(|reason| CoreError::InvalidPlan { reason })?;
        Ok(Self {
            fingerprint,
            recipe,
            license,
            attribution,
            sources,
        })
    }

    /// Parses a stored plan and checks its schema and fingerprint.
    ///
    /// # Errors
    /// [`CoreError::InvalidPlan`] when the JSON does not parse, names an unknown kind or field, has another schema or a
    /// fingerprint that does not match its sources.
    pub fn from_json(text: &str) -> Result<Self, CoreError> {
        serde_json::from_str(text).map_err(|e| CoreError::InvalidPlan { reason: e.to_string() })
    }

    /// The hex SHA-256 of the canonical JSON of the sources.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The recipe set; `None` for a discovered plan.
    #[must_use]
    pub const fn recipe(&self) -> Option<&RecipeRef> {
        self.recipe.as_ref()
    }

    /// The SPDX id of the documentation's license.
    #[must_use]
    pub fn license(&self) -> Option<&str> {
        self.license.as_deref()
    }

    /// The attribution lines shown with results.
    #[must_use]
    pub fn attribution(&self) -> &[String] {
        &self.attribution
    }

    /// The sources, in plan order.
    #[must_use]
    pub fn sources(&self) -> &[PlanSource] {
        &self.sources
    }
}

/// The hex SHA-256 of the canonical JSON of `sources`, without each revision's `candidate`.
fn fingerprint(sources: &[PlanSource]) -> Result<String, String> {
    let mut value = serde_json::to_value(sources).expect("plan sources serialize to JSON");
    for source in value.as_array_mut().into_iter().flatten() {
        if let Some(Value::Object(revision)) = source.get_mut("revision") {
            revision.remove("candidate");
        }
    }
    let mut text = String::new();
    write_canonical(&value, &mut text)?;
    Ok(sha256_hex(&text))
}

/// The canonical JSON of `value` (design §12.5): compact, every object's keys sorted, integers only.
///
/// # Errors
/// [`CoreError::InvalidPlan`] when `value` holds a number that is not an integer.
pub fn canonical_json(value: &Value) -> Result<String, CoreError> {
    let mut text = String::new();
    write_canonical(value, &mut text).map_err(|reason| CoreError::InvalidPlan { reason })?;
    Ok(text)
}

/// Appends the canonical JSON of `value` to `out`.
fn write_canonical(value: &Value, out: &mut String) -> Result<(), String> {
    match value {
        Value::Number(number) if number.is_f64() => return Err(format!("{number} is not an integer")),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::from(key.as_str()).to_string());
                out.push(':');
                write_canonical(item, out)?;
            }
            out.push('}');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const COMMIT: &str = "3f2a9c1e0b7d4a5f6e8c9b0a1d2e3f4a5b6c7d8e";

    fn guides() -> PlanSource {
        PlanSource {
            id: "guides".to_owned(),
            kind: SourceKind::Github,
            spec: object(json!({"repo": "example/widgets", "paths": ["docs/reference"], "format": "mdx"})),
            revision: Some(Revision {
                commit: Some(COMMIT.to_owned()),
                reference: Some("refs/heads/main".to_owned()),
                latest_only: true,
                ..Revision::default()
            }),
        }
    }

    fn api() -> PlanSource {
        PlanSource {
            id: "api".to_owned(),
            kind: SourceKind::NpmTypes,
            spec: object(json!({"package": "widgets"})),
            revision: Some(Revision {
                version: Some("19.1.0".to_owned()),
                ..Revision::default()
            }),
        }
    }

    fn object(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_value(value).expect("an object")
    }

    fn plan(sources: Vec<PlanSource>) -> Plan {
        Plan::new(
            Some(RecipeRef {
                commit: COMMIT.to_owned(),
                reviewed: true,
            }),
            Some("CC-BY-4.0".to_owned()),
            vec!["Widgets documentation, CC BY 4.0, https://widgets.example".to_owned()],
            sources,
        )
        .unwrap()
    }

    /// The message of an [`CoreError::InvalidPlan`] error.
    fn invalid_plan<T: std::fmt::Debug>(result: Result<T, CoreError>) -> String {
        let error = result.unwrap_err();
        assert!(matches!(error, CoreError::InvalidPlan { .. }), "{error:?}");
        error.to_string()
    }

    #[test]
    fn canonical_json_sorts_keys_and_drops_whitespace() {
        let value = json!({"b": [1, {"z": null, "a": true}], "a": "x\"y", "c": -2});
        assert_eq!(
            canonical_json(&value).unwrap(),
            r#"{"a":"x\"y","b":[1,{"a":true,"z":null}],"c":-2}"#
        );
        let float = canonical_json(&json!({"ratio": 0.5}));
        assert!(matches!(float, Err(CoreError::InvalidPlan { .. })), "{float:?}");
    }

    #[test]
    fn key_order_does_not_change_the_fingerprint() {
        let stored = serde_json::to_string(&plan(vec![guides(), api()])).unwrap();
        let reordered = stored.replace(
            r#""spec":{"format":"mdx","paths":["docs/reference"],"repo":"example/widgets"}"#,
            r#""spec":{"repo":"example/widgets","format":"mdx","paths":["docs/reference"]}"#,
        );
        assert_ne!(stored, reordered);
        let (a, b) = (Plan::from_json(&stored).unwrap(), Plan::from_json(&reordered).unwrap());
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint().len(), 64);
        assert_eq!(a, b);
        assert_eq!(
            a.fingerprint(),
            crate::ids::sha256_hex(&canonical_json(&serde_json::to_value(a.sources()).unwrap()).unwrap())
        );

        let mut changed = guides();
        changed.spec.insert("format".to_owned(), json!("markdown"));
        assert_ne!(plan(vec![changed, api()]).fingerprint(), a.fingerprint());
        assert_ne!(plan(vec![api(), guides()]).fingerprint(), a.fingerprint());
    }

    #[test]
    fn the_chosen_candidate_is_outside_the_fingerprint() {
        let mut discovered = guides();
        discovered.spec = object(json!({"repo": "example/widgets", "candidates": ["docs/", "README*"]}));
        let before = plan(vec![discovered.clone()]);
        discovered.revision.as_mut().expect("a revision").candidate = Some("docs/".to_owned());
        let after = plan(vec![discovered.clone()]);
        assert_eq!(before.fingerprint(), after.fingerprint());
        assert_ne!(before, after);
        discovered.spec = object(json!({"repo": "example/widgets", "candidates": ["doc/", "README*"]}));
        assert_ne!(plan(vec![discovered]).fingerprint(), before.fingerprint());
    }

    #[test]
    fn a_plan_round_trips_through_its_stored_json() {
        let stored = plan(vec![guides(), api()]);
        let text = serde_json::to_string(&stored).unwrap();
        assert!(text.starts_with(r#"{"schema":1,"fingerprint":""#), "{text}");
        assert!(text.contains(r#""kind":"npm-types""#), "{text}");
        assert!(text.contains(r#""ref":"refs/heads/main""#), "{text}");
        assert_eq!(Plan::from_json(&text).unwrap(), stored);
        assert_eq!(stored.recipe().map(|r| r.reviewed), Some(true));
        assert_eq!(stored.license(), Some("CC-BY-4.0"));
        assert_eq!(stored.attribution().len(), 1);
        assert_eq!(stored.sources().len(), 2);

        let discovered = Plan::new(None, None, Vec::new(), vec![api()]).unwrap();
        let text = serde_json::to_string(&discovered).unwrap();
        assert!(text.contains(r#""recipe":null"#), "{text}");
        assert_eq!(Plan::from_json(&text).unwrap(), discovered);
    }

    #[test]
    fn a_broken_stored_plan_is_invalid() {
        let text = serde_json::to_string(&plan(vec![guides(), api()])).unwrap();
        let unknown_kind = text.replace(r#""kind":"npm-types""#, r#""kind":"cobol-docs""#);
        assert!(invalid_plan(Plan::from_json(&unknown_kind)).contains("cobol-docs"));
        let schema = text.replace(r#""schema":1"#, r#""schema":2"#);
        assert!(invalid_plan(Plan::from_json(&schema)).contains("schema"));
        let tampered = text.replace("example/widgets", "example/gadgets");
        assert!(invalid_plan(Plan::from_json(&tampered)).contains("fingerprint"));
        let unknown_field = text.replace(r#""latest_only":true"#, r#""latest_only":true,"extra":1"#);
        assert!(invalid_plan(Plan::from_json(&unknown_field)).contains("extra"));
        invalid_plan(Plan::from_json("{"));

        let mut float = api();
        float.spec.insert("ratio".to_owned(), json!(0.5));
        invalid_plan(Plan::new(None, None, Vec::new(), vec![float]));

        let record = format!(r#"{{"plan":{unknown_kind}}}"#);
        assert!(serde_json::from_str::<std::collections::BTreeMap<String, Plan>>(&record).is_err());
    }

    #[test]
    fn every_kind_maps_to_its_source_spec_tag_and_back() {
        let kinds = [
            "github",
            "website",
            "llms-txt",
            "url",
            "rtd",
            "rustdoc",
            "godoc",
            "javadoc",
            "nuget-xml",
            "dotnet-api-docs",
            "npm-types",
            "dartdoc",
            "docc",
            "source-comments",
            "dart-docs-zip",
            "ruby-docs-archive",
        ];
        assert_eq!(SourceKind::ALL.len(), kinds.len());
        for (kind, name) in SourceKind::ALL.into_iter().zip(kinds) {
            assert_eq!(kind.name(), name);
            assert_eq!(kind.to_string(), name);
            assert_eq!(SourceKind::from_name(name), Some(kind));
            let tag = name.replace('-', "_");
            assert_eq!(kind.spec_tag(), tag);
            assert_eq!(SourceKind::from_spec_tag(&tag), Some(kind));
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(name));
            assert_eq!(serde_json::from_value::<SourceKind>(json!(name)).unwrap(), kind);
        }
        for unknown in ["local", "llms_txt", "GitHub", ""] {
            assert_eq!(SourceKind::from_name(unknown), None, "{unknown}");
        }
        assert_eq!(SourceKind::from_spec_tag("llms-txt"), None);
        assert!(serde_json::from_value::<SourceKind>(json!("local")).is_err());
        assert!(serde_json::from_value::<SourceKind>(json!(3)).is_err());
    }

    #[test]
    fn v1_source_specs_carry_the_snake_case_tags_of_their_kinds() {
        let specs = [
            (
                SourceKind::Github,
                json!({"kind": "github", "repo": "example/widgets", "reference": COMMIT}),
            ),
            (
                SourceKind::Url,
                json!({"kind": "url", "urls": ["https://widgets.example/llms-full.txt"]}),
            ),
            (
                SourceKind::LlmsTxt,
                json!({"kind": "llms_txt", "url": "https://widgets.example/llms.txt"}),
            ),
            (
                SourceKind::Website,
                json!({"kind": "website", "start_url": "https://widgets.example/docs/"}),
            ),
        ];
        for (kind, value) in specs {
            let spec: SourceSpec = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(SourceKind::of(&spec), Some(kind));
            assert_eq!(serde_json::to_value(&spec).unwrap()["kind"], json!(kind.spec_tag()));
            let mut fields = object(value);
            fields.remove("kind");
            let source = PlanSource {
                id: "docs".to_owned(),
                kind,
                spec: fields,
                revision: None,
            };
            assert_eq!(source.to_source_spec().unwrap(), spec);
        }
        let local: SourceSpec = serde_json::from_value(json!({"kind": "local", "path": "docs"})).unwrap();
        assert_eq!(SourceKind::of(&local), None);
    }

    #[test]
    fn kinds_without_a_v1_source_do_not_convert_yet() {
        let source = PlanSource {
            id: "reference".to_owned(),
            kind: SourceKind::Rtd,
            spec: object(json!({"project_url": "https://widgets.example/"})),
            revision: None,
        };
        let reason = invalid_plan(source.to_source_spec());
        assert!(reason.contains("reference"), "{reason}");
    }
}
