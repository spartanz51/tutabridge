use base64::Engine;
use mail_parser::{
    Address, HeaderValue, Message, MessageParser, MessagePartId, MimeHeaders, PartType,
};

/// One parsed file attachment from an incoming RFC 2822 message — the
/// minimum the bridge needs to forward it to Tuta as a `DraftAttachment`.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub filename: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct ParsedMessage {
    pub from_address: String,
    pub from_name: String,
    pub to: Vec<(String, String)>,
    pub cc: Vec<(String, String)>,
    pub bcc: Vec<(String, String)>,
    /// `Reply-To`, when the submission set one. Parsed but NOT sent: Tuta
    /// drops `DraftData.replyTos` (see `tuta::build_draft_data`), so the send
    /// path logs a warning instead, rather than losing the submitter's intent
    /// in silence. Kept so that warning can name the addresses, and so the
    /// field is ready if Tuta ever carries it.
    pub reply_to: Vec<(String, String)>,
    pub subject: String,
    pub body_html: String,
    /// The body exactly as submitted, when the submission carried only a
    /// text/plain body (no HTML part) — e.g. git send-email patches.
    /// Preserved verbatim: no escaping, no wrapping, original line endings.
    /// Tuta's outbound text/plain generation is a lossy HTML→text
    /// conversion, so anything the send path derives from `body_html`
    /// cannot round-trip a patch — this is the only faithful source.
    pub body_text: Option<String>,
    /// True when the submission carried only a text/plain body. The send
    /// path uses this to ask the server for text/plain outbound delivery
    /// (and sends `body_text` as the draft body so there is no HTML for
    /// the server's converter to mangle).
    pub is_plaintext: bool,
    /// The submission's own `Message-ID`, without angle brackets. The send
    /// path records the server-assigned id against it so later submissions
    /// referencing this one (a git send-email series) can be threaded.
    pub message_id: Option<String>,
    /// The message this submission replies to: `In-Reply-To`, falling back
    /// to the last id in `References` (both set by git send-email; some
    /// MUAs only set one). Without angle brackets.
    pub in_reply_to: Option<String>,
    pub attachments: Vec<Attachment>,
}

pub fn parse_rfc2822(raw: &str) -> ParsedMessage {
    let Some(message) = MessageParser::default().parse(raw) else {
        // Not a message at all: keep the text as a plaintext body.
        return ParsedMessage {
            body_html: raw.to_string(),
            body_text: Some(raw.to_string()),
            is_plaintext: true,
            ..ParsedMessage::default()
        };
    };

    let (from_name, from_address) = message
        .from()
        .and_then(Address::first)
        .map(name_and_address)
        .unwrap_or_default();
    let AddressHeaders {
        to,
        cc,
        bcc,
        reply_to,
    } = address_headers_of(&message);

    let in_reply_to = msg_ids(message.in_reply_to())
        .first()
        .or(msg_ids(message.references()).last())
        .map(|id| id.to_string());

    let Body {
        html,
        text,
        attachments,
    } = body_of(&message, raw);
    ParsedMessage {
        from_address,
        from_name,
        to,
        cc,
        bcc,
        reply_to,
        subject: message.subject().unwrap_or_default().trim().to_string(),
        body_html: html,
        is_plaintext: text.is_some(),
        body_text: text,
        message_id: message.message_id().map(str::to_string),
        in_reply_to,
        attachments,
    }
}

/// The address-list headers of a message, as `(name, address)` pairs with
/// display names decoded. A header that is absent yields an empty list.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct AddressHeaders {
    pub to: Vec<(String, String)>,
    pub cc: Vec<(String, String)>,
    pub bcc: Vec<(String, String)>,
    /// RFC 5322 §3.6.2 makes Reply-To an address-list, not a single address.
    pub reply_to: Vec<(String, String)>,
}

/// A message's Message-ID, without angle brackets, read from the header
/// section only: same normalization as [`parse_rfc2822`], without decoding the
/// body or attachments.
pub(crate) fn parse_message_id(raw: &str) -> Option<String> {
    MessageParser::default()
        .parse_headers(raw)?
        .message_id()
        .map(str::to_string)
}

/// The address-list headers of a rendered message. Reads the header section
/// only, so it is cheap on a message with a large body.
pub(crate) fn parse_address_headers(raw: &str) -> AddressHeaders {
    MessageParser::default()
        .parse_headers(raw)
        .map(|message| address_headers_of(&message))
        .unwrap_or_default()
}

fn address_headers_of(message: &Message<'_>) -> AddressHeaders {
    AddressHeaders {
        to: mailboxes(message.to()),
        cc: mailboxes(message.cc()),
        bcc: mailboxes(message.bcc()),
        reply_to: mailboxes(message.reply_to()),
    }
}

/// Every mailbox of an address header as `(name, address)`, the members of
/// a group included.
fn mailboxes(header: Option<&Address<'_>>) -> Vec<(String, String)> {
    header
        .map(|address| address.iter().map(name_and_address).collect())
        .unwrap_or_default()
}

fn name_and_address(mailbox: &mail_parser::Addr<'_>) -> (String, String) {
    let name = mailbox.name().unwrap_or_default();
    match mailbox.address() {
        Some(address) => (name.to_string(), address.to_string()),
        // Sloppy senders write `"John <j@x.com>`, with a quote that never
        // closes, and mean `John`. The parser then sees a name and no
        // address: take the angle-addr out of it.
        None => match name.rsplit_once('<') {
            Some((phrase, rest)) if rest.contains('>') => (
                phrase.trim().trim_matches('"').trim().to_string(),
                rest.split('>')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            ),
            _ => (name.to_string(), String::new()),
        },
    }
}

/// The ids of a `Message-ID`-like header, angle brackets stripped.
fn msg_ids<'a>(value: &'a HeaderValue<'_>) -> Vec<&'a str> {
    match value {
        HeaderValue::Text(id) => vec![id.as_ref()],
        HeaderValue::TextList(ids) => ids.iter().map(AsRef::as_ref).collect(),
        _ => Vec::new(),
    }
}

/// What the bridge sends to Tuta from a submitted message.
struct Body {
    /// The HTML body. A text/plain body is shown as an escaped `<pre>` block.
    html: String,
    /// The body exactly as submitted, only when there is no HTML body at all.
    text: Option<String>,
    attachments: Vec<Attachment>,
}

fn body_of(message: &Message<'_>, raw: &str) -> Body {
    let root = message.root_part();
    if root.is_multipart() {
        let mut found = Found::default();
        collect(message, raw, 0, &mut found);
        let (html, text) = match (found.html, found.text) {
            (Some(html), _) => (html, None),
            (None, Some(text)) => (plain_as_html(&text), Some(text)),
            (None, None) => (String::new(), None),
        };
        return Body {
            html,
            text,
            attachments: found.attachments,
        };
    }
    let decoded = match root.text_contents() {
        Some(text) => text.to_string(),
        None => String::from_utf8_lossy(root.contents()).into_owned(),
    };
    // An absent Content-Type defaults to text/plain (RFC 2045 §5.2), but
    // only a declared text/plain is shown as preformatted text.
    let content_type = root.content_type();
    let declared_plain = root.is_content_type("text", "plain");
    Body {
        html: if declared_plain {
            plain_as_html(&decoded)
        } else {
            decoded.clone()
        },
        text: (content_type.is_none() || declared_plain).then_some(decoded),
        attachments: Vec::new(),
    }
}

#[derive(Default)]
struct Found {
    html: Option<String>,
    text: Option<String>,
    attachments: Vec<Attachment>,
}

/// Walk the parts of a multipart, in order, picking up:
///   * the first HTML body, or failing that the first text/plain body;
///   * every part that looks like a file attachment (a
///     `Content-Disposition: attachment`, or a non-text part with a `name=`
///     in its Content-Type).
///
/// A text/plain part is the body only as long as no HTML one has been seen;
/// a part with no Content-Type is text/plain (RFC 2045 §5.2).
fn collect(message: &Message<'_>, raw: &str, id: MessagePartId, found: &mut Found) {
    let Some(children) = message.part(id).and_then(|part| part.sub_parts()) else {
        return;
    };
    for &child in children {
        let Some(part) = message.part(child) else {
            continue;
        };
        if part.is_multipart() {
            collect(message, raw, child, found);
            continue;
        }
        let is_text = part
            .content_type()
            .is_none_or(|ct| ct.ctype().eq_ignore_ascii_case("text"));
        let is_attachment = part
            .content_disposition()
            .is_some_and(|cd| cd.ctype().eq_ignore_ascii_case("attachment"))
            || (!is_text
                && part
                    .content_type()
                    .is_some_and(|ct| ct.attribute("name").is_some()));
        if is_attachment {
            found.attachments.push(Attachment {
                filename: attachment_name(part)
                    .unwrap_or("attachment.bin")
                    .to_string(),
                mime_type: mime_type(part),
                data: attachment_bytes(part, raw),
            });
        } else if part.is_content_type("text", "html") {
            if found.html.is_none() {
                found.html = part.text_contents().map(str::to_string);
            }
        } else if (part.is_content_type("text", "plain") || part.content_type().is_none())
            && found.html.is_none()
            && found.text.is_none()
        {
            found.text = part.text_contents().map(str::to_string);
        }
    }
}

/// The file name of an attachment: `filename` of its Content-Disposition,
/// else `name` of its Content-Type. A sender may give both a plain ASCII
/// fallback and the RFC 2231 form (`filename*=utf-8''...`); the parser
/// returns both under the same name, so prefer the one that needed encoding.
fn attachment_name<'a>(part: &'a mail_parser::MessagePart<'_>) -> Option<&'a str> {
    [
        (part.content_disposition(), "filename"),
        (part.content_type(), "name"),
    ]
    .into_iter()
    .find_map(|(header, key)| {
        let values: Vec<&str> = header?
            .attributes
            .as_deref()?
            .iter()
            .filter(|a| a.name.eq_ignore_ascii_case(key) && !a.value.is_empty())
            .map(|a| a.value.as_ref())
            .collect();
        values
            .iter()
            .find(|value| !value.is_ascii())
            .or(values.first())
            .copied()
    })
}

/// `type/subtype`, lowercased; a part that declares none is opaque data.
fn mime_type(part: &mail_parser::MessagePart<'_>) -> String {
    match part.content_type() {
        Some(ct) => match ct.subtype() {
            Some(subtype) => format!("{}/{}", ct.ctype(), subtype).to_lowercase(),
            None => ct.ctype().to_lowercase(),
        },
        None => "application/octet-stream".to_owned(),
    }
}

/// The file as it was attached: the transfer encoding undone, nothing else.
/// A text attachment is read from the raw message, because the parsed text
/// has been converted to UTF-8 and a file must keep its own bytes.
fn attachment_bytes(part: &mail_parser::MessagePart<'_>, raw: &str) -> Vec<u8> {
    if !matches!(part.body, PartType::Text(_) | PartType::Html(_)) {
        return part.contents().to_vec();
    }
    let encoded = raw
        .as_bytes()
        .get(part.offset_body as usize..part.offset_end as usize)
        .unwrap_or_default();
    match part.encoding {
        mail_parser::Encoding::Base64 => {
            let clean: Vec<u8> = encoded
                .iter()
                .copied()
                .filter(|b| !b.is_ascii_whitespace())
                .collect();
            base64::engine::general_purpose::STANDARD
                .decode(clean)
                .unwrap_or_default()
        }
        mail_parser::Encoding::QuotedPrintable => {
            mail_parser::decoders::quoted_printable::quoted_printable_decode(encoded)
                .unwrap_or_else(|| encoded.to_vec())
        }
        mail_parser::Encoding::None => encoded.to_vec(),
    }
}

/// How a text/plain body is shown in Tuta clients: an escaped `<pre>` block.
fn plain_as_html(text: &str) -> String {
    format!("<pre>{}</pre>", html_escape(text))
}

pub(super) fn parse_headers(header_section: &str) -> Vec<(String, String)> {
    let mut headers = Vec::new();
    let mut current_name = String::new();
    let mut current_value = String::new();

    for line in header_section.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            current_value.push(' ');
            current_value.push_str(line.trim());
        } else if let Some((name, value)) = line.split_once(':') {
            if !current_name.is_empty() {
                headers.push((
                    current_name.to_lowercase(),
                    current_value.trim().to_string(),
                ));
            }
            current_name = name.trim().to_string();
            current_value = value.to_string();
        }
    }
    if !current_name.is_empty() {
        headers.push((
            current_name.to_lowercase(),
            current_value.trim().to_string(),
        ));
    }
    headers
}

pub(super) fn get_header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.clone())
}

pub(super) fn extract_boundary(content_type: &str) -> Option<String> {
    let lower = content_type.to_lowercase();
    if let Some(pos) = lower.find("boundary=") {
        let rest = &content_type[pos + 9..];
        let boundary = if rest.starts_with('"') {
            rest[1..].split('"').next().unwrap_or("")
        } else {
            rest.split(|c: char| c.is_whitespace() || c == ';')
                .next()
                .unwrap_or("")
        };
        if !boundary.is_empty() {
            return Some(boundary.to_string());
        }
    }
    None
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests were written against the hand-written decoders. The same
    // inputs now go through a whole message, so they pin the behaviour of
    // the parser the bridge actually uses.
    fn decode_transfer(body: &str, encoding: &str) -> String {
        parse_rfc2822(&format!(
            "Content-Type: text/plain\r\nContent-Transfer-Encoding: {encoding}\r\n\r\n{body}"
        ))
        .body_text
        .unwrap()
    }

    fn decode_quoted_printable(body: &str) -> String {
        decode_transfer(body, "quoted-printable")
    }

    fn parse_address_single(raw: &str) -> (String, String) {
        parse_rfc2822(&format!("To: {raw}\r\n\r\n")).to.remove(0)
    }

    fn decode_header_value(value: &str) -> String {
        parse_rfc2822(&format!("Subject: {value}\r\n\r\n")).subject
    }

    fn decode_q_encoding(text: &str) -> String {
        decode_header_value(&format!("=?UTF-8?Q?{text}?="))
    }

    /// The file name of an attachment whose Content-Disposition (for
    /// `filename`) or Content-Type (for `name`) is `header`.
    fn extract_param(header: &str, key: &str) -> Option<String> {
        let part_headers = match key {
            "filename" => {
                format!("Content-Type: application/octet-stream\r\nContent-Disposition: {header}")
            }
            _ => format!("Content-Type: {header}\r\nContent-Disposition: attachment"),
        };
        let raw = format!(
            "Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\n{part_headers}\r\n\r\nx\r\n--B--\r\n"
        );
        let name = parse_rfc2822(&raw).attachments.remove(0).filename;
        (name != "attachment.bin").then_some(name)
    }

    #[test]
    fn test_parse_simple_message() {
        let raw = "From: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nSubject: Hello\r\n\r\n<p>Hi Bob</p>";
        let msg = parse_rfc2822(raw);
        assert_eq!(msg.from_name, "Alice");
        assert_eq!(msg.from_address, "alice@example.com");
        assert_eq!(msg.to.len(), 1);
        assert_eq!(msg.to[0].1, "bob@example.com");
        assert_eq!(msg.subject, "Hello");
        assert_eq!(msg.body_html, "<p>Hi Bob</p>");
    }

    #[test]
    fn quoted_comma_in_display_name_is_one_recipient() {
        let raw = "From: me@tuta.io\r\nTo: \"Doe, John\" <john@x.com>, bob@y.com\r\nSubject: t\r\n\r\nbody\r\n";
        let msg = parse_rfc2822(raw);
        let addrs: Vec<&str> = msg.to.iter().map(|(_, a)| a.as_str()).collect();
        assert_eq!(
            addrs,
            vec!["john@x.com", "bob@y.com"],
            "a comma inside a quoted display name must not split the recipient"
        );
        assert_eq!(msg.to[0].0, "Doe, John", "display name should be preserved");
    }

    #[test]
    fn plain_address_list_still_splits_on_commas() {
        let raw = "From: me@tuta.io\r\nTo: a@x.com, b@y.com, c@z.com\r\nSubject: t\r\n\r\nbody\r\n";
        let msg = parse_rfc2822(raw);
        let addrs: Vec<&str> = msg.to.iter().map(|(_, a)| a.as_str()).collect();
        assert_eq!(addrs, vec!["a@x.com", "b@y.com", "c@z.com"]);
    }

    #[test]
    fn base64_body_non_utf8_is_not_echoed_as_base64() {
        // "caf" + 0xE9 (Latin-1 'é'): valid base64, not valid UTF-8.
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"caf\xe9");
        let out = decode_transfer(&b64, "base64");
        assert!(!out.contains(&b64), "must not emit the raw base64 blob");
        assert!(
            out.starts_with("caf"),
            "decoded text should be readable, got {out:?}"
        );
    }

    #[test]
    fn qp_non_utf8_byte_is_decoded_not_echoed() {
        let out = decode_quoted_printable("caf=E9");
        assert!(
            !out.contains("=E9"),
            "QP source must be decoded, not echoed"
        );
        assert!(out.starts_with("caf"), "got {out:?}");
    }

    #[test]
    fn test_parse_multiple_recipients() {
        let raw = "From: a@b.com\r\nTo: Bob <bob@x.com>, Charlie <charlie@x.com>\r\nCc: Dave <dave@x.com>\r\nSubject: Test\r\n\r\nbody";
        let msg = parse_rfc2822(raw);
        assert_eq!(msg.to.len(), 2);
        assert_eq!(msg.to[0].1, "bob@x.com");
        assert_eq!(msg.to[1].1, "charlie@x.com");
        assert_eq!(msg.cc.len(), 1);
        assert_eq!(msg.cc[0].1, "dave@x.com");
    }

    #[test]
    fn test_parse_base64_body() {
        let body_b64 = base64::engine::general_purpose::STANDARD.encode(b"<p>Hello</p>");
        let raw = format!(
            "From: a@b.com\r\nTo: b@c.com\r\nSubject: Test\r\nContent-Transfer-Encoding: base64\r\nContent-Type: text/html\r\n\r\n{}",
            body_b64
        );
        let msg = parse_rfc2822(&raw);
        assert_eq!(msg.body_html, "<p>Hello</p>");
    }

    #[test]
    fn test_parse_plain_text_body() {
        let raw = "From: a@b.com\r\nTo: b@c.com\r\nSubject: Test\r\nContent-Type: text/plain\r\n\r\nHello <world>";
        let msg = parse_rfc2822(raw);
        assert_eq!(msg.body_html, "<pre>Hello &lt;world&gt;</pre>");
        assert!(msg.is_plaintext);
    }

    #[test]
    fn message_id_and_in_reply_to_are_extracted_bare() {
        let raw = "From: a@b.com\r\nMessage-ID: <20260802-1-paul@scarrone.co>\r\nIn-Reply-To: <20260802-0-paul@scarrone.co>\r\nReferences: <old@x> <20260802-0-paul@scarrone.co>\r\n\r\nbody";
        let msg = parse_rfc2822(raw);
        assert_eq!(
            msg.message_id.as_deref(),
            Some("20260802-1-paul@scarrone.co")
        );
        assert_eq!(
            msg.in_reply_to.as_deref(),
            Some("20260802-0-paul@scarrone.co")
        );
    }

    #[test]
    fn in_reply_to_falls_back_to_last_reference() {
        // git send-email sets both; some MUAs only set References. The
        // last id there is the direct parent.
        let raw = "From: a@b.com\r\nReferences: <root@x>\r\n <parent@x>\r\n\r\nbody";
        let msg = parse_rfc2822(raw);
        assert_eq!(msg.in_reply_to.as_deref(), Some("parent@x"));
    }

    #[test]
    fn reply_to_is_extracted_with_name_and_address() {
        let raw = "From: a@b.com\r\nReply-To: Paul Scarrone <paul@scarrone.co>\r\n\r\nbody";
        let msg = parse_rfc2822(raw);
        assert_eq!(
            msg.reply_to,
            vec![("Paul Scarrone".to_string(), "paul@scarrone.co".to_string())]
        );
    }

    #[test]
    fn reply_to_is_an_address_list() {
        // RFC 5322 §3.6.2 makes this a list, so a second address must not
        // be lost from the warning the send path logs.
        let raw = "From: a@b.com\r\nReply-To: one@x.com, Two <two@x.com>\r\n\r\nbody";
        let msg = parse_rfc2822(raw);
        assert_eq!(msg.reply_to.len(), 2);
        assert_eq!(msg.reply_to[0].1, "one@x.com");
        assert_eq!(
            msg.reply_to[1],
            ("Two".to_string(), "two@x.com".to_string())
        );
    }

    #[test]
    fn absent_reply_to_is_empty_not_the_from() {
        // The whole point of the field: no Reply-To must stay no Reply-To,
        // rather than quietly becoming the sender.
        let raw = "From: a@b.com\r\n\r\nbody";
        let msg = parse_rfc2822(raw);
        assert!(msg.reply_to.is_empty());
    }

    #[test]
    fn bare_message_id_without_brackets_is_accepted() {
        let raw = "From: a@b.com\r\nIn-Reply-To: bare-id@host\r\n\r\nbody";
        let msg = parse_rfc2822(raw);
        assert_eq!(msg.in_reply_to.as_deref(), Some("bare-id@host"));
        assert!(msg.message_id.is_none());
    }

    #[test]
    fn plaintext_body_is_preserved_verbatim() {
        // The draft body sent to Tuta must be the SMTP bytes, untouched:
        // shell metacharacters, blank lines, indentation, trailing spaces.
        // The <pre> rendering exists alongside, not instead.
        let body = "cmd >/dev/null 2>&1\r\nread </dev/null\r\na && b || c\r\nif [ $x -lt 5 ]; then echo \"<tag>\"; fi\r\n\r\n\tindented\r\n  two  spaces  \r\n";
        let raw = format!(
            "From: a@b.com\r\nTo: b@c.com\r\nSubject: probe\r\nContent-Type: text/plain\r\n\r\n{}",
            body
        );
        let msg = parse_rfc2822(&raw);
        assert!(msg.is_plaintext);
        assert_eq!(msg.body_text.as_deref(), Some(body));
        assert!(msg.body_html.starts_with("<pre>"));
        assert!(msg.body_html.contains("&lt;tag&gt;"));
    }

    #[test]
    fn plaintext_body_survives_quoted_printable() {
        let raw = "From: a@b.com\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\ndiff --git a/x b/x\r\n-a =3D b\r\n+a < b && c\r\n";
        let msg = parse_rfc2822(raw);
        assert_eq!(
            msg.body_text.as_deref(),
            Some("diff --git a/x b/x\r\n-a = b\r\n+a < b && c\r\n")
        );
    }

    #[test]
    fn multipart_plain_only_preserves_raw_body_text() {
        let raw = "From: a@b.com\r\nContent-Type: multipart/mixed; boundary=\"bnd\"\r\n\r\n--bnd\r\nContent-Type: text/plain\r\n\r\nread </dev/null\na && b\n--bnd--";
        let msg = parse_rfc2822(raw);
        assert!(msg.is_plaintext);
        // The line break before the delimiter belongs to the delimiter.
        assert_eq!(msg.body_text.as_deref(), Some("read </dev/null\na && b"));
    }

    #[test]
    fn html_alternative_clears_body_text() {
        let raw = "From: a@b.com\r\nContent-Type: multipart/alternative; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nplain\r\n--b\r\nContent-Type: text/html\r\n\r\n<p>html</p>\r\n--b--";
        let msg = parse_rfc2822(raw);
        assert!(!msg.is_plaintext);
        assert!(msg.body_text.is_none());
    }

    #[test]
    fn is_plaintext_false_for_html_and_missing_default() {
        let html = "From: a@b.com\r\nContent-Type: text/html\r\n\r\n<p>Hi</p>";
        assert!(!parse_rfc2822(html).is_plaintext);
        // No Content-Type defaults to text/plain (RFC 2045 §5.2).
        let bare = "From: a@b.com\r\n\r\nHi";
        assert!(parse_rfc2822(bare).is_plaintext);
    }

    #[test]
    fn test_parse_encoded_subject() {
        let raw = "From: a@b.com\r\nTo: b@c.com\r\nSubject: =?UTF-8?B?SMOpbGxv?=\r\n\r\nbody";
        let msg = parse_rfc2822(raw);
        assert_eq!(msg.subject, "H\u{e9}llo");
    }

    #[test]
    fn test_parse_address_no_name() {
        let (name, addr) = parse_address_single("bob@example.com");
        assert_eq!(name, "");
        assert_eq!(addr, "bob@example.com");
    }

    #[test]
    fn test_parse_address_with_quotes() {
        let (name, addr) = parse_address_single("\"John Doe\" <john@x.com>");
        assert_eq!(name, "John Doe");
        assert_eq!(addr, "john@x.com");
    }

    #[test]
    fn test_decode_q_encoding() {
        assert_eq!(decode_q_encoding("Hello_=C3=A9"), "Hello \u{e9}");
    }

    #[test]
    fn test_split_headers_body_lf() {
        let msg = parse_rfc2822("From: a@b.com\nTo: b@c.com\n\nBody");
        assert_eq!(msg.from_address, "a@b.com");
        assert_eq!(msg.to, vec![(String::new(), "b@c.com".to_string())]);
        assert_eq!(msg.body_text.as_deref(), Some("Body"));
    }

    #[test]
    fn test_folded_headers() {
        let raw = "From: a@b.com\r\nSubject: very long\r\n subject line\r\nTo: b@c.com\r\n\r\nbody";
        let msg = parse_rfc2822(raw);
        assert_eq!(msg.subject, "very long subject line");
    }

    #[test]
    fn test_multipart_alternative() {
        let raw = "From: a@b.com\r\nTo: b@c.com\r\nSubject: Test\r\nContent-Type: multipart/alternative; boundary=\"abc123\"\r\n\r\n--abc123\r\nContent-Type: text/plain\r\n\r\nHello plain\r\n--abc123\r\nContent-Type: text/html\r\n\r\n<p>Hello HTML</p>\r\n--abc123--";
        let msg = parse_rfc2822(raw);
        assert!(msg.body_html.contains("Hello HTML"));
        assert!(!msg.is_plaintext);
    }

    #[test]
    fn test_multipart_mixed_with_nested() {
        let raw = "From: a@b.com\r\nTo: b@c.com\r\nSubject: Test\r\nContent-Type: multipart/mixed; boundary=\"outer\"\r\n\r\n--outer\r\nContent-Type: multipart/alternative; boundary=\"inner\"\r\n\r\n--inner\r\nContent-Type: text/plain\r\n\r\nPlain text\r\n--inner\r\nContent-Type: text/html\r\n\r\n<p>HTML body</p>\r\n--inner--\r\n--outer--";
        let msg = parse_rfc2822(raw);
        assert!(msg.body_html.contains("HTML body"));
    }

    #[test]
    fn test_multipart_plain_only() {
        let raw = "From: a@b.com\r\nTo: b@c.com\r\nSubject: Test\r\nContent-Type: multipart/alternative; boundary=\"bnd\"\r\n\r\n--bnd\r\nContent-Type: text/plain\r\n\r\nJust plain\r\n--bnd--";
        let msg = parse_rfc2822(raw);
        assert!(msg.body_html.contains("Just plain"));
        assert!(msg.is_plaintext);
    }

    #[test]
    fn test_extract_boundary_quoted() {
        assert_eq!(
            extract_boundary("multipart/alternative; boundary=\"abc_123\""),
            Some("abc_123".to_string())
        );
    }

    #[test]
    fn test_extract_boundary_unquoted() {
        assert_eq!(
            extract_boundary("multipart/mixed; boundary=abc123"),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn test_multipart_with_attachment_extracts_both_body_and_file() {
        let body = b"this is a fake pdf";
        let body_b64 = base64::engine::general_purpose::STANDARD.encode(body);
        let raw = format!(
            "From: a@b.com\r\nTo: b@c.com\r\nSubject: Test\r\nContent-Type: multipart/mixed; boundary=\"xx\"\r\n\r\n--xx\r\nContent-Type: text/html\r\n\r\n<p>HTML body</p>\r\n--xx\r\nContent-Type: application/pdf; name=\"doc.pdf\"\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"doc.pdf\"\r\n\r\n{}\r\n--xx--",
            body_b64
        );
        let msg = parse_rfc2822(&raw);
        assert!(msg.body_html.contains("HTML body"));
        assert_eq!(msg.attachments.len(), 1);
        let att = &msg.attachments[0];
        assert_eq!(att.filename, "doc.pdf");
        assert_eq!(att.mime_type, "application/pdf");
        assert_eq!(att.data, body);
    }

    #[test]
    fn test_multipart_attachment_without_filename_uses_content_type_name() {
        let raw = "From: a@b.com\r\nTo: b@c.com\r\nSubject: Test\r\nContent-Type: multipart/mixed; boundary=\"yy\"\r\n\r\n--yy\r\nContent-Type: text/plain\r\n\r\nbody\r\n--yy\r\nContent-Type: image/png; name=\"avatar.png\"\r\nContent-Transfer-Encoding: base64\r\n\r\nUE5HSEVBREVS\r\n--yy--";
        let msg = parse_rfc2822(raw);
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].filename, "avatar.png");
        assert_eq!(msg.attachments[0].mime_type, "image/png");
        assert_eq!(msg.attachments[0].data, b"PNGHEADER");
    }

    /// #101: Thunderbird sends a non-ASCII name in the RFC 2231 form.
    #[test]
    fn test_attachment_name_in_rfc2231_form_is_kept() {
        let raw = "From: a@b.com\r\nTo: b@c.com\r\nSubject: Test\r\nContent-Type: multipart/mixed; boundary=\"zz\"\r\n\r\n--zz\r\nContent-Type: text/plain\r\n\r\nbody\r\n--zz\r\nContent-Type: application/pdf\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment;\r\n filename*=utf-8''rapport%20%C3%A9t%C3%A9.pdf\r\n\r\nUERG\r\n--zz--";
        let msg = parse_rfc2822(raw);
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].filename, "rapport été.pdf");
        assert_eq!(msg.attachments[0].data, b"PDF");
    }

    #[test]
    fn test_extract_param_reads_rfc2231_forms() {
        let cd = "attachment; filename*=utf-8''rapport%20%C3%A9t%C3%A9.pdf";
        assert_eq!(
            extract_param(cd, "filename").as_deref(),
            Some("rapport été.pdf")
        );
        // Split over sections, the first one carrying the charset.
        let cd =
            "attachment; filename*0*=utf-8''rapport%20; filename*1*=%C3%A9t%C3%A9; filename*2=.pdf";
        assert_eq!(
            extract_param(cd, "filename").as_deref(),
            Some("rapport été.pdf")
        );
        // The RFC 2231 form wins over the plain fallback.
        let cd = "attachment; filename=\"rapport ete.pdf\"; filename*=utf-8''rapport%20%C3%A9t%C3%A9.pdf";
        assert_eq!(
            extract_param(cd, "filename").as_deref(),
            Some("rapport été.pdf")
        );
        let ct = "text/plain; name*=iso-8859-1'fr'%E9t%E9.txt";
        assert_eq!(extract_param(ct, "name").as_deref(), Some("été.txt"));
    }

    #[test]
    fn rfc2231_sections_are_assembled_in_order() {
        let header = "attachment; filename*1*=.pdf; filename*0*=utf-8''report";
        assert_eq!(
            extract_param(header, "filename").as_deref(),
            Some("report.pdf")
        );
        // Malformed section numbers still give a usable name.
        for sections in [
            "filename*1*=utf-8''part.pdf",
            "filename*0*=utf-8''report; filename*2*=.pdf",
        ] {
            let header = format!("attachment; filename=backup.pdf; {sections}");
            assert!(extract_param(&header, "filename").is_some(), "{header}");
        }
    }

    #[test]
    fn rfc2231_names_in_other_charsets_are_decoded() {
        let header = "attachment; filename=price.pdf; filename*=windows-1252''prix%80.pdf";
        assert_eq!(
            extract_param(header, "filename").as_deref(),
            Some("prix€.pdf")
        );
        // Known limit of the parser: split over sections, only the first
        // one is read in a charset other than UTF-8. The name stays usable.
        let split = "attachment; filename*0*=windows-1252''prix; filename*1*=%80.pdf";
        assert_eq!(
            extract_param(split, "filename").as_deref(),
            Some("prix\u{fffd}.pdf")
        );
    }

    #[test]
    fn test_extract_param_matches_whole_names_only() {
        let cd = "attachment; filename=\"a;b.pdf\"";
        assert_eq!(extract_param(cd, "filename").as_deref(), Some("a;b.pdf"));
        assert_eq!(
            extract_param("image/png; name=avatar.png", "name").as_deref(),
            Some("avatar.png")
        );
    }

    #[test]
    fn test_multipart_alternative_ignores_alternative_parts_as_attachments() {
        let raw = "From: a@b.com\r\nTo: b@c.com\r\nSubject: Test\r\nContent-Type: multipart/alternative; boundary=\"alt\"\r\n\r\n--alt\r\nContent-Type: text/plain\r\n\r\nplain body\r\n--alt\r\nContent-Type: text/html\r\n\r\n<p>HTML body</p>\r\n--alt--";
        let msg = parse_rfc2822(raw);
        // text/html and text/plain alternatives must not be picked up as attachments
        assert!(msg.attachments.is_empty());
        assert!(msg.body_html.contains("HTML body"));
    }

    #[test]
    fn test_multipart_base64_part() {
        let body_b64 = base64::engine::general_purpose::STANDARD.encode(b"<p>Encoded</p>");
        let raw = format!(
            "From: a@b.com\r\nTo: b@c.com\r\nSubject: Test\r\nContent-Type: multipart/alternative; boundary=\"sep\"\r\n\r\n--sep\r\nContent-Type: text/html\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n--sep--",
            body_b64
        );
        let msg = parse_rfc2822(&raw);
        assert_eq!(msg.body_html, "<p>Encoded</p>");
    }

    // --- multi-encoded-word subjects ---

    #[test]
    fn test_decode_multi_encoded_words() {
        let s = "=?UTF-8?B?SMOpbGxv?= =?UTF-8?B?IE1vbmRl?=";
        let result = decode_header_value(s);
        assert_eq!(result, "H\u{e9}llo Monde");
    }

    #[test]
    fn test_decode_mixed_encoded_and_plain() {
        let s = "Re: =?UTF-8?B?SMOpbGxv?= there";
        let result = decode_header_value(s);
        assert_eq!(result, "Re: H\u{e9}llo there");
    }

    #[test]
    fn test_decode_q_encoded_word() {
        let s = "=?UTF-8?Q?Hello_=C3=A9?=";
        let result = decode_header_value(s);
        assert_eq!(result, "Hello \u{e9}");
    }

    #[test]
    fn test_decode_q_word_starting_with_escape() {
        let s = "=?UTF-8?Q?=C3=89t=C3=A9?=";
        let result = decode_header_value(s);
        assert_eq!(result, "\u{c9}t\u{e9}");
    }

    #[test]
    fn test_decode_unterminated_word_stays_raw() {
        assert_eq!(decode_header_value("a =?UTF-8?Q?abc"), "a =?UTF-8?Q?abc");
        assert_eq!(
            decode_header_value("a =?UTF-8?Q?abc? x"),
            "a =?UTF-8?Q?abc? x"
        );
    }

    #[test]
    fn test_parse_folded_subject_with_q_word_starting_with_escape() {
        // Python's email package (3.14) folds this subject so that the second
        // word starts with an escape.
        let raw = "From: a@x.com\r\nTo: b@y.com\r\nSubject: Re: Ponuda za =?utf-8?q?odr=C5=BEavanje_=E2=80=93_Nikola_Nikoli?=\r\n =?utf-8?q?=C4=87?=\r\n\r\nbody\r\n";
        let msg = parse_rfc2822(raw);
        assert_eq!(
            msg.subject,
            "Re: Ponuda za odr\u{17e}avanje \u{2013} Nikola Nikoli\u{107}"
        );
    }

    // --- quoted-printable soft break ---

    #[test]
    fn test_qp_soft_break_crlf() {
        let input = "Hello=\r\nWorld";
        let result = decode_quoted_printable(input);
        assert_eq!(result, "HelloWorld");
    }

    #[test]
    fn test_qp_soft_break_lf() {
        let input = "Hello=\nWorld";
        let result = decode_quoted_printable(input);
        assert_eq!(result, "HelloWorld");
    }

    #[test]
    fn test_qp_no_byte_loss() {
        let input = "line1=\nABC";
        let result = decode_quoted_printable(input);
        assert_eq!(result, "line1ABC");
    }

    #[test]
    fn test_qp_encoded_chars() {
        let input = "caf=C3=A9";
        let result = decode_quoted_printable(input);
        assert_eq!(result, "caf\u{e9}");
    }

    #[test]
    fn message_id_matches_the_full_parse_and_ignores_the_body() {
        let raw = "Message-ID: <abc@client>\r\nSubject: Hi\r\n\r\nMessage-ID: <body@x>\r\n";
        assert_eq!(parse_message_id(raw).as_deref(), Some("abc@client"));
        assert_eq!(parse_message_id(raw), parse_rfc2822(raw).message_id);
        assert_eq!(parse_message_id("Subject: none\r\n\r\nbody"), None);
    }

    #[test]
    fn address_headers_come_from_the_header_section_only() {
        // Folded To, encoded Cc name, Reply-To list; the body's "To:" line and
        // the missing Bcc must not leak in.
        let raw = "From: a@b.com\r\nTo: Bob <bob@x.com>,\r\n charlie@x.com\r\nCc: =?UTF-8?B?Wm/Dqw==?= <zoe@x.com>\r\nReply-To: one@x.com, Two <two@x.com>\r\n\r\nTo: not-a-header@x.com";
        let parsed = parse_address_headers(raw);
        assert_eq!(
            parsed,
            AddressHeaders {
                to: vec![
                    ("Bob".to_string(), "bob@x.com".to_string()),
                    (String::new(), "charlie@x.com".to_string()),
                ],
                cc: vec![("Zoë".to_string(), "zoe@x.com".to_string())],
                bcc: vec![],
                reply_to: vec![
                    (String::new(), "one@x.com".to_string()),
                    ("Two".to_string(), "two@x.com".to_string()),
                ],
            }
        );
    }

    #[test]
    fn address_headers_of_a_message_without_any_are_empty() {
        assert_eq!(
            parse_address_headers("Subject: hi\r\n\r\nbody"),
            AddressHeaders::default()
        );
    }

    #[test]
    fn angle_brackets_inside_a_quoted_name_do_not_break_the_mailbox() {
        // A `>` before the `<` used to slice backwards and panic the parser.
        let raw = "From: \"a > b\" <x@y.com>\r\nTo: \"c <d>\" <e@f.com>, g@h.com\r\n\r\n";
        let msg = parse_rfc2822(raw);
        assert_eq!(
            (msg.from_name.as_str(), msg.from_address.as_str()),
            ("a > b", "x@y.com")
        );
        assert_eq!(
            msg.to,
            vec![
                ("c <d>".to_string(), "e@f.com".to_string()),
                (String::new(), "g@h.com".to_string()),
            ]
        );
    }

    #[test]
    fn quoted_name_escapes_are_undone() {
        let raw = "From: \"Say \\\"hi\\\" \\\\ bye.\" <a@b.c>\r\n\r\n";
        let msg = parse_rfc2822(raw);
        assert_eq!(msg.from_name, "Say \"hi\" \\ bye.");
        assert_eq!(msg.from_address, "a@b.c");
    }

    #[test]
    fn escaped_quotes_inside_a_quoted_name_do_not_split_the_list() {
        let raw = "To: \"5\\\" screen, big\" <a@b.c>, x@y.z\r\n\r\n";
        assert_eq!(
            parse_rfc2822(raw).to,
            vec![
                ("5\" screen, big".to_string(), "a@b.c".to_string()),
                (String::new(), "x@y.z".to_string()),
            ]
        );
    }

    #[test]
    fn stray_quotes_around_a_name_are_still_trimmed() {
        let msg = parse_rfc2822("From: \"John <j@x.com>\r\n\r\n");
        assert_eq!(
            (msg.from_name.as_str(), msg.from_address.as_str()),
            ("John", "j@x.com")
        );
    }

    #[test]
    fn address_headers_accept_a_bare_lf_message() {
        // Same separator rules as parse_rfc2822: the body's "To:" is not a header.
        assert_eq!(
            parse_address_headers("To: a@b.c\n\nTo: not-a-header@x.y").to,
            vec![(String::new(), "a@b.c".to_string())]
        );
    }

    // --- what the hand-written parser got wrong ---

    #[test]
    fn body_in_another_charset_is_decoded() {
        let latin1 = "Content-Type: text/plain; charset=\"iso-8859-1\"\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n=E9t=E9 =E0 =E7a";
        assert_eq!(parse_rfc2822(latin1).body_text.as_deref(), Some("été à ça"));
        let windows = "Content-Type: text/plain; charset=\"windows-1252\"\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nprix 5=80 et =9Cuvre";
        assert_eq!(
            parse_rfc2822(windows).body_text.as_deref(),
            Some("prix 5€ et œuvre")
        );
    }

    #[test]
    fn multibyte_charsets_are_decoded() {
        // "日本語" in ISO-2022-JP, body and subject.
        let raw = "Subject: =?iso-2022-jp?B?GyRCRnxLXDhsGyhC?=\r\nContent-Type: text/plain; charset=\"iso-2022-jp\"\r\nContent-Transfer-Encoding: base64\r\n\r\nGyRCRnxLXDhsGyhC";
        let msg = parse_rfc2822(raw);
        assert_eq!(msg.subject, "日本語");
        assert_eq!(msg.body_text.as_deref(), Some("日本語"));
    }

    #[test]
    fn address_groups_give_their_members() {
        let msg = parse_rfc2822(
            "To: Team: a@example.com, \"B, b\" <b@example.com>;, c@example.com\r\nCc: undisclosed-recipients:;\r\n\r\n",
        );
        assert_eq!(
            msg.to,
            vec![
                (String::new(), "a@example.com".to_string()),
                ("B, b".to_string(), "b@example.com".to_string()),
                (String::new(), "c@example.com".to_string()),
            ]
        );
        assert!(msg.cc.is_empty());
    }

    #[test]
    fn a_message_with_only_an_attachment_has_an_empty_body() {
        let raw = "Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: application/zip\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"only.zip\"\r\n\r\nUEsDBA==\r\n--B--\r\n";
        let msg = parse_rfc2822(raw);
        // Not the raw MIME source.
        assert_eq!(msg.body_html, "");
        assert!(!msg.is_plaintext);
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].data, b"PK\x03\x04");
    }

    #[test]
    fn attachments_keep_their_own_bytes() {
        // An attached message and a text file in another charset: line
        // endings and bytes are the sender's, not converted.
        let raw = "Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nbody\r\n--B\r\nContent-Type: message/rfc822\r\nContent-Disposition: attachment; filename=\"fwd.eml\"\r\n\r\nSubject: inner\r\n\r\ninner body\r\n\r\n--B\r\nContent-Type: text/csv; charset=iso-8859-1\r\nContent-Transfer-Encoding: quoted-printable\r\nContent-Disposition: attachment; filename=\"d.csv\"\r\n\r\n=E9;=E0\r\n--B--\r\n";
        let msg = parse_rfc2822(raw);
        assert_eq!(msg.body_text.as_deref(), Some("body"));
        assert_eq!(msg.attachments.len(), 2);
        assert_eq!(msg.attachments[0].mime_type, "message/rfc822");
        assert_eq!(
            msg.attachments[0].data,
            b"Subject: inner\r\n\r\ninner body\r\n"
        );
        assert_eq!(msg.attachments[1].data, b"\xe9;\xe0");
    }

    #[test]
    fn a_part_without_content_type_is_the_plain_body() {
        let raw = "Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\n\r\nno type\r\n--B\r\nContent-Type: application/pdf; name=x.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\nUERG\r\n--B--\r\n";
        let msg = parse_rfc2822(raw);
        assert_eq!(msg.body_text.as_deref(), Some("no type"));
        assert_eq!(msg.attachments[0].filename, "x.pdf");
    }

    #[test]
    fn the_rfc2231_name_wins_whichever_comes_first() {
        for cd in [
            "attachment; filename=\"rapport ete.pdf\"; filename*=utf-8''rapport%20%C3%A9t%C3%A9.pdf",
            "attachment; filename*=utf-8''rapport%20%C3%A9t%C3%A9.pdf; filename=\"rapport ete.pdf\"",
        ] {
            assert_eq!(
                extract_param(cd, "filename").as_deref(),
                Some("rapport été.pdf"),
                "{cd}"
            );
        }
    }
}
