//! `CommonMark` code fences: splitting section Markdown into prose and fenced code, so each can carry its own weight
//! and signal, and the fence rule itself for line-by-line scanners.

/// The prose and the fenced code of a section, each in document order. Fence lines themselves are in neither.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Split {
    /// Everything outside code fences.
    pub prose: String,
    /// The contents of code fences.
    pub code: String,
}

/// An open fence: its character and length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fence {
    marker: char,
    len: usize,
}

impl Fence {
    /// The fence `line` opens, if any: three or more backticks or tildes, indented by at most three spaces. A backtick
    /// fence's info string cannot contain a backtick.
    #[must_use]
    pub fn opened_by(line: &str) -> Option<Self> {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let rest = &line[indent..];
        let marker = rest.chars().next().filter(|&c| indent <= 3 && (c == '`' || c == '~'))?;
        let len = rest.len() - rest.trim_start_matches(marker).len();
        let info = &rest[len..];
        (len >= 3 && !(marker == '`' && info.contains('`'))).then_some(Self { marker, len })
    }

    /// Whether `line` closes this fence: the same character, at least as long, and nothing after it.
    #[must_use]
    pub fn is_closed_by(self, line: &str) -> bool {
        Self::opened_by(line).is_some_and(|fence| {
            let after = line.trim_start_matches(' ')[fence.len..].trim();
            fence.marker == self.marker && fence.len >= self.len && after.is_empty()
        })
    }
}

/// Splits `text` at `CommonMark` code fences: three or more backticks or tildes, indented by at most three spaces. A
/// fence that is never closed runs to the end of the text.
#[must_use]
pub fn split_code(text: &str) -> Split {
    let mut split = Split::default();
    let mut open: Option<Fence> = None;
    for line in text.split_inclusive('\n') {
        match open {
            Some(current) if current.is_closed_by(line) => open = None,
            Some(_) => split.code.push_str(line),
            None => match Fence::opened_by(line) {
                Some(fence) => open = Some(fence),
                None => split.prose.push_str(line),
            },
        }
    }
    split
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fence_opens_and_closes_by_the_commonmark_rule() {
        let fence = Fence::opened_by("   ````md\n").unwrap();
        assert_eq!(Fence::opened_by("    ```\n"), None);
        assert!(!fence.is_closed_by("```\n"));
        assert!(!fence.is_closed_by("````js\n"));
        assert!(!fence.is_closed_by("text\n"));
        assert!(fence.is_closed_by("`````  \n"));
    }

    fn split(text: &str) -> (String, String) {
        let Split { prose, code } = split_code(text);
        (prose, code)
    }

    #[test]
    fn separates_prose_from_fenced_code() {
        assert_eq!(
            split("# Title\n\nText.\n\n```rust\nfn main() {}\n```\nAfter\n"),
            ("# Title\n\nText.\n\nAfter\n".to_owned(), "fn main() {}\n".to_owned())
        );
    }

    #[test]
    fn closing_fences_must_match_and_stand_alone() {
        assert_eq!(
            split("````\n```\n~~~~\n```` x\n````\nout"),
            ("out".to_owned(), "```\n~~~~\n```` x\n".to_owned())
        );
        assert_eq!(split("~~~\na\n~~~~~\nb\n"), ("b\n".to_owned(), "a\n".to_owned()));
    }

    #[test]
    fn only_shallow_three_character_fences_open() {
        assert_eq!(
            split("``\n    ```\n   ```\ncode\n"),
            ("``\n    ```\n".to_owned(), "code\n".to_owned())
        );
        assert_eq!(split("``` a`b\nx\n"), ("``` a`b\nx\n".to_owned(), String::new()));
        assert_eq!(split("~~~ a`b\nx\n"), (String::new(), "x\n".to_owned()));
    }

    #[test]
    fn empty_text_has_no_parts() {
        assert_eq!(split_code(""), Split::default());
    }
}
