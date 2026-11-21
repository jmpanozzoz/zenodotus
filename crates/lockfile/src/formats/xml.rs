//! A small XML tree for `pom.xml`, `.csproj`, `Directory.Packages.props` and `.slnx`. A document type declaration is
//! refused, so no entity is ever defined or expanded; only the predefined entities and character references resolve.

use std::path::Path;

use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use super::{line_at, parse_error};
use crate::error::LockfileError;

/// Deepest element nesting accepted.
pub(crate) const MAX_DEPTH: usize = 256;

/// One element: its local name, attributes, text and child elements.
#[derive(Debug, Default)]
pub(crate) struct Element {
    /// The local name, without a namespace prefix.
    pub(crate) name: String,
    /// The attributes by local name, values unescaped.
    pub(crate) attributes: Vec<(String, String)>,
    /// The text directly inside it, unescaped.
    pub(crate) text: String,
    /// The child elements in document order.
    pub(crate) children: Vec<Element>,
}

impl Element {
    /// The value of the attribute `name`.
    pub(crate) fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The child elements called `name`.
    pub(crate) fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.children.iter().filter(move |child| child.name == name)
    }

    /// The first child element called `name`.
    pub(crate) fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|child| child.name == name)
    }

    /// The trimmed text of the first child element called `name`.
    pub(crate) fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(|child| child.text.trim())
    }
}

/// The text of a character reference or predefined entity such as `#x26` or `amp`; others stay as written.
fn resolve_reference(name: &str) -> String {
    let character = name.strip_prefix("#x").map_or_else(
        || name.strip_prefix('#').and_then(|decimal| decimal.parse().ok()),
        |hex| u32::from_str_radix(hex, 16).ok(),
    );
    character.and_then(char::from_u32).map_or_else(
        || resolve_predefined_entity(name).map_or_else(|| format!("&{name};"), str::to_owned),
        String::from,
    )
}

/// An element with its name and attributes, and no content yet.
fn open(start: &BytesStart<'_>) -> Result<Element, String> {
    let mut element = Element {
        name: start.local_name().as_ref().to_owned(),
        ..Element::default()
    };
    for attribute in start.attributes() {
        let attribute = attribute.map_err(|error| error.to_string())?;
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|error| error.to_string())?;
        element
            .attributes
            .push((attribute.key.local_name().as_ref().to_owned(), value.into_owned()));
    }
    Ok(element)
}

/// Adds a finished element to its parent, or makes it the root when no element is open.
fn close(stack: &mut [Element], root: &mut Option<Element>, element: Element) -> Result<(), String> {
    match stack.last_mut() {
        Some(parent) => parent.children.push(element),
        None if root.is_none() => *root = Some(element),
        None => return Err("more than one root element".to_owned()),
    }
    Ok(())
}

/// The root element of `text`.
pub(crate) fn parse(text: &str, file: &Path) -> Result<Element, LockfileError> {
    let mut reader = Reader::from_str(text);
    let mut stack: Vec<Element> = Vec::new();
    let mut root = None;
    loop {
        let start = reader.buffer_position();
        let step = match reader.read_event() {
            Err(error) => Err(error.to_string()),
            Ok(Event::DocType(_)) => Err("a document type declaration is refused".to_owned()),
            Ok(Event::Start(_)) if stack.len() >= MAX_DEPTH => Err(format!("elements nest deeper than {MAX_DEPTH}")),
            Ok(Event::Start(element)) => open(&element).map(|element| stack.push(element)),
            Ok(Event::Empty(element)) => open(&element).and_then(|element| close(&mut stack, &mut root, element)),
            Ok(Event::End(_)) => {
                let element = stack.pop().unwrap_or_default();
                close(&mut stack, &mut root, element)
            }
            Ok(Event::Text(content)) => {
                append(&mut stack, &content.into_inner());
                Ok(())
            }
            Ok(Event::CData(content)) => {
                append(&mut stack, &content.into_inner());
                Ok(())
            }
            Ok(Event::GeneralRef(reference)) => {
                append(&mut stack, &resolve_reference(&reference.into_inner()));
                Ok(())
            }
            Ok(Event::Eof) => break,
            Ok(_) => Ok(()),
        };
        step.map_err(|message| {
            let at = if message.starts_with("a document type") {
                start
            } else {
                reader.buffer_position()
            };
            parse_error(
                file,
                Some(line_at(text, usize::try_from(at).unwrap_or(usize::MAX))),
                message,
            )
        })?;
    }
    let eof = Some(line_at(text, text.len()));
    if !stack.is_empty() {
        return Err(parse_error(file, eof, "an element is not closed"));
    }
    root.ok_or_else(|| parse_error(file, eof, "no root element"))
}

/// Appends text to the innermost open element; text outside the root is ignored.
fn append(stack: &mut [Element], content: &str) {
    if let Some(element) = stack.last_mut() {
        element.text.push_str(content);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse_error_line(text: &str) -> Option<u64> {
        let error = parse(text, Path::new("pom.xml")).unwrap_err();
        assert_eq!(error.cause(), crate::Cause::Parse, "{error}");
        let diagnostic = crate::Diagnostic::from(error);
        assert_eq!(diagnostic.file, Path::new("pom.xml"));
        diagnostic.line
    }

    #[test]
    fn elements_attributes_and_text_form_a_tree() {
        let root = parse(
            "<?xml version=\"1.0\"?>\n<!-- c -->\n<project a=\"x &amp; y\"><v>1&lt;2&#x41;&#66;&nbsp;</v><e k=\"1\"/>\
             <v><![CDATA[<raw>]]></v><ns:w>t</ns:w></project>",
            Path::new("pom.xml"),
        )
        .unwrap();
        assert_eq!(root.name, "project");
        assert_eq!(root.attribute("a"), Some("x & y"));
        assert_eq!(root.attribute("b"), None);
        assert_eq!(root.child_text("v"), Some("1<2AB&nbsp;"));
        assert_eq!(
            root.children_named("v").map(|v| v.text.as_str()).collect::<Vec<_>>(),
            ["1<2AB&nbsp;", "<raw>"]
        );
        assert_eq!(root.child("e").and_then(|e| e.attribute("k")), Some("1"));
        assert_eq!(root.child_text("w"), Some("t"));
        assert!(root.child("missing").is_none());
    }

    #[test]
    fn a_doctype_is_refused_so_no_entity_expands() {
        let bomb = "<?xml version=\"1.0\"?>\n<!DOCTYPE lolz [\n <!ENTITY lol \"lol\">\n]>\n<project>&lol;</project>";
        assert_eq!(parse_error_line(bomb), Some(2));
    }

    #[test]
    fn malformed_xml_is_a_parse_error_with_a_line() {
        assert_eq!(parse_error_line("<project>\n<a>\n</b>\n</project>"), Some(3));
        assert_eq!(parse_error_line("<project>\n<a>\n"), Some(3));
        assert_eq!(parse_error_line("<!-- only a comment -->"), Some(1));
        assert_eq!(parse_error_line("<a/><b/>"), Some(1));
        assert_eq!(parse_error_line("<a x=\"&bad;\"/>"), Some(1));
    }

    #[test]
    fn nesting_deeper_than_the_limit_is_refused() {
        let deep = "<a>".repeat(MAX_DEPTH + 1) + &"</a>".repeat(MAX_DEPTH + 1);
        assert_eq!(parse_error_line(&deep), Some(1));
        let fine = "<a>".repeat(MAX_DEPTH) + &"</a>".repeat(MAX_DEPTH);
        assert!(parse(&fine, Path::new("pom.xml")).is_ok());
    }

    #[test]
    fn a_malformed_attribute_is_a_parse_error() {
        assert_eq!(parse_error_line("<project>\n<a b=\"&bogus;\"/>\n</project>\n"), Some(2));
        assert_eq!(
            parse_error_line("<project>\n\n<a b=\"1\" b=\"2\"/>\n</project>\n"),
            Some(3)
        );
    }
}
