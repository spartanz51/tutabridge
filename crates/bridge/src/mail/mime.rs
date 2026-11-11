//! The MIME entities of a rendered message, as slices of it.
//!
//! `BODYSTRUCTURE` and `BODY[<part>]` both read this tree, so the part
//! numbers and sizes a client learns from the first always match the octets
//! it gets from the second.

use super::parser::{extract_boundary, get_header, parse_headers};
use std::borrow::Cow;

/// A message or one of its body parts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Entity<'a> {
    /// The header block, with the blank line that ends it.
    pub head: &'a str,
    pub body: &'a str,
}

impl<'a> Entity<'a> {
    pub fn parse(raw: &'a str) -> Self {
        // A part may have no header at all: it then starts with the blank line.
        let blank = ["\r\n", "\n"].into_iter().find(|eol| raw.starts_with(eol));
        let split = match blank {
            Some(eol) => eol.len(),
            None => match raw
                .find("\r\n\r\n")
                .map(|pos| pos + 4)
                .or_else(|| raw.find("\n\n").map(|pos| pos + 2))
            {
                Some(end) => end,
                None => raw.len(),
            },
        };
        Self {
            head: &raw[..split],
            body: &raw[split..],
        }
    }

    /// Header fields, names lowercased.
    pub fn headers(&self) -> Vec<(String, String)> {
        parse_headers(self.head.trim_end_matches(['\r', '\n']))
    }

    /// The parts of a multipart entity with a usable boundary; `None` for
    /// anything else, which is then a single part.
    pub fn children(&self) -> Option<Vec<Entity<'a>>> {
        let content_type = get_header(&self.headers(), "content-type")?;
        if !content_type.to_ascii_lowercase().starts_with("multipart/") {
            return None;
        }
        let boundary = extract_boundary(&content_type)?;
        Some(
            split_parts(self.body, &boundary)
                .into_iter()
                .map(Entity::parse)
                .collect(),
        )
    }
}

/// The raw parts between the delimiter lines of a multipart body. The line
/// break before a delimiter belongs to the delimiter (RFC 2046 §5.1.1), not
/// to the part it ends. A part with no closing delimiter is dropped.
fn split_parts<'a>(body: &'a str, boundary: &str) -> Vec<&'a str> {
    let delimiter = format!("--{boundary}");
    let mut parts = Vec::new();
    let mut start = None;
    let mut pos = 0;
    for line in body.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix(&delimiter) {
            if let Some(start) = start.filter(|&start| start < pos) {
                let part = &body[start..pos];
                let part = part
                    .strip_suffix("\r\n")
                    .or_else(|| part.strip_suffix('\n'))
                    .unwrap_or(part);
                parts.push(part);
            }
            if rest.starts_with("--") {
                break;
            }
            start = Some(pos + line.len());
        }
        pos += line.len();
    }
    parts
}

/// The octets of a numbered `BODY` section (RFC 3501 §6.4.5): `1`, `2.1`,
/// or the same followed by `.MIME`. `None` when the message has no such
/// part, and for the `HEADER` and `TEXT` of a part, which only exist for an
/// embedded message and the bridge renders none.
pub(crate) fn section<'a>(message: &'a str, spec: &str) -> Option<Cow<'a, str>> {
    let (path, modifier) = parse_spec(spec)?;
    let mut entity = Entity::parse(message);
    // A message that is not multipart has exactly one part, itself.
    let mut whole_message = false;
    for (depth, &number) in path.iter().enumerate() {
        match entity.children() {
            Some(parts) => entity = *parts.get(number - 1)?,
            None if depth == 0 && path == [1] => whole_message = true,
            None => return None,
        }
    }
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
    }

    #[test]
    fn entities_split_at_the_first_blank_line() {
        let entity = Entity::parse("A: 1\r\n\r\nbody\r\n\r\nmore");
        assert_eq!(
            (entity.head, entity.body),
            ("A: 1\r\n\r\n", "body\r\n\r\nmore")
        );
        let bare_lf = Entity::parse("A: 1\n\nbody");
        assert_eq!((bare_lf.head, bare_lf.body), ("A: 1\n\n", "body"));
        let no_header = Entity::parse("\r\nbody");
        assert_eq!((no_header.head, no_header.body), ("\r\n", "body"));
        let no_body = Entity::parse("A: 1\r\n");
        assert_eq!((no_body.head, no_body.body), ("A: 1\r\n", ""));
    }

    #[test]
    fn parts_are_slices_between_delimiters() {
        assert_eq!(
            split_parts("pre\r\n--B\r\none\r\n--B\r\n\r\n--B\r\n--B\r\nlast\r\n--B--\r\nepilogue\r\n--B\r\nno", "B"),
            // An empty line is a part, two adjacent delimiters are not.
            vec!["one", "", "last"]
        );
        assert_eq!(split_parts("--B\nlf\n--B--", "B"), vec!["lf"]);
        // Unterminated: nothing after the last delimiter counts.
        assert_eq!(split_parts("--B\r\none\r\n--B\r\ntwo", "B"), vec!["one"]);
        assert!(split_parts("no delimiter", "B").is_empty());
    }
}
