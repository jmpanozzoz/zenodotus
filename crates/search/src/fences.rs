//! Splits section Markdown into prose and fenced code, so each can carry its own weight and signal.

/// The prose and the fenced code of a section, each in document order. Fence lines themselves are in neither.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Split {
    /// Everything outside code fences.
    pub prose: String,
    /// The contents of code fences.
    pub code: String,
}

/// An open fence: its character and length.
#[derive(Clone, Copy)]
struct Fence {
    marker: char,
    len: usize,
}

/// Splits `text` at `CommonMark` code fences: three or more backticks or tildes, indented by at most three spaces. A
/// fence that is never closed runs to the end of the text.
#[must_use]
pub fn split_code(text: &str) -> Split {
    let mut split = Split::default();
    let mut open: Option<Fence> = None;
    for line in text.split_inclusive('\n') {
        match (open, fence_of(line)) {
            (Some(current), Some(fence)) if closes(current, fence, line) => open = None,
            (Some(_), _) => split.code.push_str(line),
            (None, Some(fence)) => open = Some(fence),
            (None, None) => split.prose.push_str(line),
        }
    }
    split
}

/// The fence that `line` starts with, if any. A backtick fence's info string cannot contain a backtick.
fn fence_of(line: &str) -> Option<Fence> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let marker = rest.chars().next().filter(|&c| indent <= 3 && (c == '`' || c == '~'))?;
    let len = rest.len() - rest.trim_start_matches(marker).len();
    let info = &rest[len..];
    (len >= 3 && !(marker == '`' && info.contains('`'))).then_some(Fence { marker, len })
}

/// Whether `fence`, found on `line`, closes `current`: same character, at least as long, and nothing after it.
fn closes(current: Fence, fence: Fence, line: &str) -> bool {
    let after = line.trim_start_matches(' ')[fence.len..].trim();
    fence.marker == current.marker && fence.len >= current.len && after.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

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
