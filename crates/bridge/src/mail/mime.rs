//! The MIME entities of a rendered message, as slices of it.
//!
//! `BODYSTRUCTURE` and `BODY[<part>]` both read this tree, so the part
//! numbers and sizes a client learns from the first always match the octets
//! it gets from the second.

use mail_parser::{MessageParser, MessagePartId, PartType};
use std::borrow::Cow;

/// A message parsed into its MIME parts. Parsing and part boundaries are
/// `mail-parser`'s; this only maps its offsets back onto the raw message.
pub(crate) struct Tree<'a> {
    raw: &'a str,
    message: mail_parser::Message<'a>,
}

/// A message or one of its body parts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Entity<'a> {
    /// The header block, with the blank line that ends it.
    pub head: &'a str,
    /// The body. The line break before a MIME delimiter belongs to the
    /// delimiter (RFC 2046 §5.1.1), not to the part it ends.
    pub body: &'a str,
}

impl<'a> Tree<'a> {
    /// `None` for input that holds no message at all.
    pub fn parse(raw: &'a str) -> Option<Self> {
        let message = MessageParser::default().parse(raw)?;
        (!message.parts.is_empty()).then_some(Self { raw, message })
    }

    /// The message itself.
    pub const ROOT: MessagePartId = 0;

    pub fn entity(&self, id: MessagePartId) -> Option<Entity<'a>> {
        let part = self.message.parts.get(id as usize)?;
        Some(Entity {
            head: self
                .raw
                .get(part.offset_header as usize..part.offset_body as usize)?,
            body: self
                .raw
                .get(part.offset_body as usize..part.offset_end as usize)?,
        })
    }

    /// The parts of a multipart entity; `None` for anything else, which is
    /// then a single part.
    pub fn children(&self, id: MessagePartId) -> Option<&[MessagePartId]> {
        match &self.message.parts.get(id as usize)?.body {
            PartType::Multipart(parts) if !parts.is_empty() => Some(parts),
            _ => None,
        }
    }
}

/// The octets of a numbered `BODY` section (RFC 3501 §6.4.5): `1`, `2.1`,
/// or the same followed by `.MIME`. `None` when the message has no such
/// part, and for the `HEADER` and `TEXT` of a part, which only exist for an
/// embedded message and the bridge renders none.
pub(crate) fn section<'a>(message: &'a str, spec: &str) -> Option<Cow<'a, str>> {
    let (path, modifier) = parse_spec(spec)?;
    let tree = Tree::parse(message)?;
    let mut id = Tree::ROOT;
    // A message that is not multipart has exactly one part, itself.
    let mut whole_message = false;
    for (depth, &number) in path.iter().enumerate() {
        match tree.children(id) {
            Some(parts) => id = *parts.get(number - 1)?,
            None if depth == 0 && path == [1] => whole_message = true,
            None => return None,
        }
    }
    let entity = tree.entity(id)?;
    match modifier {
        "" => Some(Cow::Borrowed(entity.body)),
        // A part's MIME header is its whole header. For the message itself
        // it is the MIME fields among the message header.
        "MIME" if whole_message => Some(Cow::Owned(mime_fields(entity.head))),
        "MIME" => Some(Cow::Borrowed(entity.head)),
        _ => None,
    }
}

/// `1.2.MIME` as the part path `[1, 2]` and the modifier `MIME`.
fn parse_spec(spec: &str) -> Option<(Vec<usize>, &str)> {
    if spec.ends_with('.') {
        return None;
    }
    let mut path = Vec::new();
    let mut rest = spec;
    loop {
        let (token, tail) = rest.split_once('.').unwrap_or((rest, ""));
        if token.is_empty() || !token.bytes().all(|b| b.is_ascii_digit()) {
            break;
        }
        let number = token.parse::<usize>().ok().filter(|&n| n > 0)?;
        path.push(number);
        rest = tail;
    }
    let modifier = match rest {
        "" => "",
        m if m.eq_ignore_ascii_case("MIME") => "MIME",
        _ => return None,
    };
    (!path.is_empty()).then_some((path, modifier))
}

/// The `MIME-Version` and `Content-*` fields of a header block, folded lines
/// included, ended by a blank line.
fn mime_fields(head: &str) -> String {
    let mut out = String::new();
    let mut keep = false;
    for line in head.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']).is_empty() {
            break;
        }
        if !line.starts_with([' ', '\t']) {
            let name = line.split(':').next().unwrap_or("").to_ascii_lowercase();
            keep = name.starts_with("content-") || name == "mime-version";
        }
        if keep {
            out.push_str(line);
        }
    }
    out.push_str("\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// multipart/mixed holding a multipart/alternative and an attachment.
    const NESTED: &str = "Subject: t\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"OUT\"\r\n\r\npreamble\r\n--OUT\r\nContent-Type: multipart/alternative; boundary=\"IN\"\r\n\r\n--IN\r\nContent-Type: text/plain\r\n\r\nhi plain\r\n--IN\r\nContent-Type: text/html\r\n\r\n<p>hi</p>\r\n\r\n--IN--\r\n--OUT\r\nContent-Type: image/png; name=\"x.png\"\r\n\r\nABC\r\n--OUT--\r\n";

    fn get(message: &str, spec: &str) -> Option<String> {
        section(message, spec).map(Cow::into_owned)
    }

    #[test]
    fn numbered_sections_follow_the_part_tree() {
        assert_eq!(get(NESTED, "1.1").as_deref(), Some("hi plain"));
        // Only the last line break belongs to the delimiter.
        assert_eq!(get(NESTED, "1.2").as_deref(), Some("<p>hi</p>\r\n"));
        assert_eq!(get(NESTED, "2").as_deref(), Some("ABC"));
        assert_eq!(
            get(NESTED, "2.MIME").as_deref(),
            Some("Content-Type: image/png; name=\"x.png\"\r\n\r\n")
        );
        assert_eq!(
            get(NESTED, "1.1.mime").as_deref(),
            Some("Content-Type: text/plain\r\n\r\n")
        );
        // A multipart part is its body, delimiters included.
        let alternative = get(NESTED, "1").unwrap();
        assert!(alternative.starts_with("--IN\r\nContent-Type: text/plain"));
        assert!(alternative.ends_with("--IN--"));
    }

    #[test]
    fn sections_the_message_does_not_have_are_none() {
        for spec in [
            "3",
            "1.3",
            "2.1",
            "0",
            "1.0",
            "",
            "MIME",
            "1.",
            "1..2",
            "+1",
            "1.HEADER",
            "1.TEXT",
            "1.HEADER.FIELDS (From)",
            "1.MIME.2",
            "99999999999999999999",
        ] {
            assert_eq!(get(NESTED, spec), None, "{spec}");
        }
    }

    #[test]
    fn a_message_that_is_not_multipart_is_its_own_part_1() {
        let message = "Subject: t\r\nMIME-Version: 1.0\r\nContent-Type: text/plain;\r\n charset=UTF-8\r\nX-Other: no\r\n\r\nbody\r\n";
        assert_eq!(get(message, "1").as_deref(), Some("body\r\n"));
        assert_eq!(
            get(message, "1.MIME").as_deref(),
            Some("MIME-Version: 1.0\r\nContent-Type: text/plain;\r\n charset=UTF-8\r\n\r\n")
        );
        assert_eq!(get(message, "2"), None);
        assert_eq!(get(message, "1.1"), None);
        // A multipart header with no boundary cannot be split either.
        let broken = "Content-Type: multipart/mixed\r\n\r\nbroken";
        assert_eq!(get(broken, "1").as_deref(), Some("broken"));
        assert_eq!(get("", "1"), None);
    }

    #[test]
    fn bare_line_feeds_and_a_part_without_header_are_read() {
        let message = "A: 1\nContent-Type: multipart/mixed; boundary=B\n\n--B\n\nfirst\n--B\nContent-Type: text/plain\n\nsecond\n\n--B--\n";
        assert_eq!(get(message, "1").as_deref(), Some("first"));
        assert_eq!(get(message, "1.MIME").as_deref(), Some("\n"));
        assert_eq!(get(message, "2").as_deref(), Some("second\n"));
    }
}
