use base64::Engine;

/// One parsed file attachment from an incoming RFC 2822 message — the
/// minimum the bridge needs to forward it to Tuta as a `DraftAttachment`.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub filename: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
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
    let (header_section, body_section) = split_headers_body(raw);
    let headers = parse_headers(&header_section);

    let from_raw = get_header(&headers, "from").unwrap_or_default();
    let (from_name, from_address) = parse_address_single(&from_raw);

    let AddressHeaders {
        to,
        cc,
        bcc,
        reply_to,
    } = address_headers_of(&headers);

    let subject = get_header(&headers, "subject")
        .map(|s| decode_header_value(&s))
        .unwrap_or_default();

    let message_id = get_header(&headers, "message-id")
        .as_deref()
        .and_then(first_msg_id);
    let in_reply_to = get_header(&headers, "in-reply-to")
        .as_deref()
        .and_then(first_msg_id)
        .or_else(|| {
            get_header(&headers, "references")
                .as_deref()
                .and_then(last_msg_id)
        });

    let content_type = get_header(&headers, "content-type").unwrap_or_default();
    let content_transfer_encoding = get_header(&headers, "content-transfer-encoding")
        .unwrap_or_default()
        .to_lowercase();

    let ct_lower = content_type.to_lowercase();
    let (body_html, attachments, body_text) = if ct_lower.contains("multipart/") {
        extract_multipart_body_and_attachments(&body_section, &content_type)
    } else {
        // An absent Content-Type defaults to text/plain (RFC 2045 §5.2).
        let is_plaintext = ct_lower.is_empty() || ct_lower.contains("text/plain");
        let decoded = decode_transfer(&body_section, &content_transfer_encoding);
        let body_html = wrap_plain_as_html(&decoded, &ct_lower);
        (body_html, Vec::new(), is_plaintext.then_some(decoded))
    };

    ParsedMessage {
        from_address,
        from_name,
        to,
        cc,
        bcc,
        reply_to,
        subject,
        body_html,
        is_plaintext: body_text.is_some(),
        body_text,
        message_id,
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

/// The address-list headers of a rendered message. Reads the header section
/// only, so it is cheap on a message with a large body.
pub(crate) fn parse_address_headers(raw: &str) -> AddressHeaders {
    address_headers_of(&parse_headers(header_section(raw)))
}

fn address_headers_of(headers: &[(String, String)]) -> AddressHeaders {
    let list = |name: &str| {
        get_header(headers, name)
            .map(|v| parse_address_list(&v))
            .unwrap_or_default()
    };
    AddressHeaders {
        to: list("to"),
        cc: list("cc"),
        bcc: list("bcc"),
        reply_to: list("reply-to"),
    }
}

/// All RFC 5322 msg-ids in a header value, angle brackets stripped. A value
/// with no `<...>` spans (some MUAs write bare ids) yields the trimmed value
/// itself, if non-empty.
fn msg_ids(raw: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = raw;
    while let Some(open) = rest.find('<') {
        let Some(close) = rest[open + 1..].find('>') else {
            break;
        };
        let id = rest[open + 1..open + 1 + close].trim();
        if !id.is_empty() {
            ids.push(id.to_string());
        }
        rest = &rest[open + 1 + close + 1..];
    }
    if ids.is_empty() {
        let bare = raw.trim();
        if !bare.is_empty() {
            ids.push(bare.to_string());
        }
    }
    ids
}

fn first_msg_id(raw: &str) -> Option<String> {
    msg_ids(raw).into_iter().next()
}

/// `References` lists oldest→newest, so the last id is the direct parent.
fn last_msg_id(raw: &str) -> Option<String> {
    msg_ids(raw).into_iter().next_back()
}

pub(super) fn split_headers_body(raw: &str) -> (String, String) {
    let headers = header_section(raw);
    let body = raw[headers.len()..]
        .strip_prefix("\r\n\r\n")
        .or_else(|| raw[headers.len()..].strip_prefix("\n\n"))
        .unwrap_or("");
    (headers.to_string(), body.to_string())
}

/// The header section of a message: everything before the first blank line,
/// CRLF or bare LF. The whole message when there is none.
fn header_section(raw: &str) -> &str {
    raw.find("\r\n\r\n")
        .or_else(|| raw.find("\n\n"))
        .map_or(raw, |pos| &raw[..pos])
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

/// One mailbox: `Name <addr>` or a bare `addr`. The angle-addr is the last
/// `<`…`>` pair, since a quoted display name may itself hold `<` or `>`
/// (`"a > b" <x@y>` used to slice backwards and panic).
fn parse_address_single(raw: &str) -> (String, String) {
    let raw = raw.trim();
    if let Some(lt) = raw.rfind('<') {
        if let Some(gt) = raw[lt..].find('>') {
            let addr = raw[lt + 1..lt + gt].trim().to_string();
            let name = decode_header_value(&unquote(raw[..lt].trim()));
            return (name, addr);
        }
    }
    (String::new(), raw.to_string())
}

/// Strip the quotes of an RFC 5322 quoted-string and undo its `\\` escapes.
/// A phrase that is not one whole quoted-string only loses stray quotes at
/// its ends, as sloppy senders write `"John <j@x>` and mean `John`.
fn unquote(phrase: &str) -> String {
    let Some(inner) = phrase.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else {
        return phrase.trim_matches('"').to_string();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            _ => out.push(c),
        }
    }
    out
}

fn parse_address_list(raw: &str) -> Vec<(String, String)> {
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut in_quotes = false;
    let mut current = String::new();

    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        match ch {
            // A quoted display name may contain commas and angle brackets that
            // are NOT list separators, e.g. `"Doe, John" <j@x.com>`. Track the
            // quote state so those stay part of the same entry instead of
            // splitting it into bogus recipients.
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            // An escaped character inside the quoted-string (`\\"`, `\\\\`) is
            // not a delimiter: keep the pair and leave the quote state alone.
            '\\' if in_quotes => {
                current.push(ch);
                current.extend(chars.next());
            }
            '<' if !in_quotes => {
                depth += 1;
                current.push(ch);
            }
            '>' if !in_quotes => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 && !in_quotes => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    result.push(parse_address_single(&trimmed));
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        result.push(parse_address_single(&trimmed));
    }
    result
}

fn decode_header_value(s: &str) -> String {
    let s = s.trim();
    if !s.contains("=?") {
        return s.to_string();
    }

    let mut result = String::new();
    let mut remaining = s;

    while let Some(start) = remaining.find("=?") {
        result.push_str(&remaining[..start]);
        remaining = &remaining[start + 2..];

        let parts: Vec<&str> = remaining.splitn(4, '?').collect();
        if parts.len() >= 3 {
            let encoding = parts[1].to_uppercase();
            let encoded = parts[2];
            if let Some(end_marker) = remaining.find("?=") {
                let decoded = if encoding == "B" {
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .ok()
                        .and_then(|bytes| String::from_utf8(bytes).ok())
                } else if encoding == "Q" {
                    Some(decode_q_encoding(encoded))
                } else {
                    None
                };

                if let Some(text) = decoded {
                    result.push_str(&text);
                    remaining = &remaining[end_marker + 2..];
                    let ws_stripped = remaining.trim_start();
                    if ws_stripped.starts_with("=?") {
                        remaining = ws_stripped;
                    }
                    continue;
                }
            }
        }
        result.push_str("=?");
    }
    result.push_str(remaining);
    result
}

fn decode_q_encoding(s: &str) -> String {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'=' && i + 2 < bytes.len() {
            if let Ok(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                result.push(byte);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'_' {
            result.push(b' ');
        } else {
            result.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8(result).unwrap_or_else(|_| s.to_string())
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

/// Walk a multipart MIME body, picking up:
///   * the user-facing HTML (or plain) body (first non-attachment text part);
///   * every part that looks like a file attachment (non-text, or a part
///     with `Content-Disposition: attachment` / a `name=` in its Content-Type).
/// Returns `(body_html, attachments, body_text)` — `body_text` is the raw
/// decoded text/plain body, present only when no HTML alternative existed
/// anywhere (the submission was genuinely plaintext).
fn extract_multipart_body_and_attachments(
    body: &str,
    content_type: &str,
) -> (String, Vec<Attachment>, Option<String>) {
    let boundary = match extract_boundary(content_type) {
        Some(b) => b,
        None => return (body.to_string(), Vec::new(), None),
    };

    let parts = split_mime_parts(body, &boundary);
    let mut html_part = None;
    let mut text_part = None;
    let mut attachments: Vec<Attachment> = Vec::new();

    for part in &parts {
        let (part_headers_str, part_body) = split_headers_body(part);
        let part_headers = parse_headers(&part_headers_str);
        let part_ct = get_header(&part_headers, "content-type").unwrap_or_default();
        let part_cte = get_header(&part_headers, "content-transfer-encoding")
            .unwrap_or_default()
            .to_lowercase();
        let part_cd = get_header(&part_headers, "content-disposition").unwrap_or_default();
        let part_ct_lower = part_ct.to_lowercase();
        let part_cd_lower = part_cd.to_lowercase();

        let is_attachment = part_cd_lower.contains("attachment")
            || (extract_param(&part_ct, "name").is_some() && !part_ct_lower.contains("text/"));

        if part_ct_lower.contains("multipart/") {
            let (nested_body, nested_atts, nested_text) =
                extract_multipart_body_and_attachments(&part_body, &part_ct);
            if let Some(raw) = nested_text {
                if text_part.is_none() {
                    text_part = Some(raw);
                }
            } else if html_part.is_none() && !nested_body.is_empty() {
                html_part = Some(nested_body);
            }
            attachments.extend(nested_atts);
        } else if is_attachment {
            let data = match part_cte.as_str() {
                cte if cte.contains("base64") => {
                    let clean: String = part_body.chars().filter(|c| !c.is_whitespace()).collect();
                    base64::engine::general_purpose::STANDARD
                        .decode(&clean)
                        .unwrap_or_default()
                }
                cte if cte.contains("quoted-printable") => {
                    decode_quoted_printable(&part_body).into_bytes()
                }
                _ => part_body.as_bytes().to_vec(),
            };
            let filename = extract_param(&part_cd, "filename")
                .or_else(|| extract_param(&part_ct, "name"))
                .unwrap_or_else(|| "attachment.bin".to_owned());
            let filename = decode_header_value(&filename);
            let mime_type = part_ct
                .split(';')
                .next()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "application/octet-stream".to_owned());
            attachments.push(Attachment {
                filename,
                mime_type,
                data,
            });
        } else if part_ct_lower.contains("text/html") && html_part.is_none() {
            html_part = Some(decode_transfer(&part_body, &part_cte));
        } else if part_ct_lower.contains("text/plain") && html_part.is_none() && text_part.is_none()
        {
            // Raw — the `<pre>` HTML rendering is derived below only if no
            // HTML alternative shows up in a later part.
            text_part = Some(decode_transfer(&part_body, &part_cte));
        }
    }

    // Plaintext only when no HTML alternative existed anywhere: an unknown
    // structure that falls back to the raw section is conservatively HTML.
    match (html_part, text_part) {
        (Some(html), _) => (html, attachments, None),
        (None, Some(raw)) => {
            let html = wrap_plain_as_html(&raw, "text/plain");
            (html, attachments, Some(raw))
        }
        (None, None) => (body.to_string(), attachments, None),
    }
}

/// Pull a `key=value` parameter out of a header value such as a Content-Type
/// (`text/plain; charset="UTF-8"; name="doc.pdf"`). Handles both quoted and
/// unquoted forms; returns `None` if the parameter is absent.
fn extract_param(header: &str, key: &str) -> Option<String> {
    let lower = header.to_lowercase();
    let needle = format!("{}=", key.to_lowercase());
    let pos = lower.find(&needle)?;
    let rest = &header[pos + needle.len()..];
    let value = if rest.starts_with('"') {
        rest[1..].split('"').next().unwrap_or("")
    } else {
        rest.split(|c: char| c == ';' || c.is_whitespace())
            .next()
            .unwrap_or("")
    };
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

pub(super) fn split_mime_parts(body: &str, boundary: &str) -> Vec<String> {
    let delimiter = format!("--{}", boundary);
    let end_delimiter = format!("--{}--", boundary);
    let mut parts = Vec::new();
    let mut in_part = false;
    let mut current = String::new();

    for line in body.lines() {
        if line.starts_with(&end_delimiter) {
            if in_part && !current.is_empty() {
                parts.push(
                    current
                        .trim_start_matches("\r\n")
                        .trim_start_matches('\n')
                        .to_string(),
                );
            }
            break;
        }
        if line.starts_with(&delimiter) {
            if in_part && !current.is_empty() {
                parts.push(
                    current
                        .trim_start_matches("\r\n")
                        .trim_start_matches('\n')
                        .to_string(),
                );
            }
            current = String::new();
            in_part = true;
            continue;
        }
        if in_part {
            current.push_str(line);
            current.push('\n');
        }
    }
    parts
}

/// Undo the Content-Transfer-Encoding, nothing else — the result is the body
/// exactly as the submitter wrote it.
fn decode_transfer(body: &str, transfer_encoding: &str) -> String {
    if transfer_encoding.contains("base64") {
        let clean: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        base64::engine::general_purpose::STANDARD
            .decode(&clean)
            // Decode the bytes lossily rather than echoing the raw base64 when
            // the payload is not valid UTF-8 (e.g. a Latin-1 body). Returning
            // the base64 blob as "the body" was the worst possible fallback.
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_else(|_| body.to_string())
    } else if transfer_encoding.contains("quoted-printable") {
        decode_quoted_printable(body)
    } else {
        body.to_string()
    }
}

/// The HTML rendering of a decoded body: text/plain becomes an escaped
/// `<pre>` block (for display in Tuta clients), anything else passes through.
fn wrap_plain_as_html(decoded: &str, content_type: &str) -> String {
    if content_type.contains("text/plain") && !content_type.contains("text/html") {
        format!("<pre>{}</pre>", html_escape(decoded))
    } else {
        decoded.to_string()
    }
}

fn decode_quoted_printable(s: &str) -> String {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'=' {
            if i + 2 < bytes.len() && bytes[i + 1] == b'\r' && bytes[i + 2] == b'\n' {
                i += 3;
            } else if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 2;
            } else if i + 2 < bytes.len() {
                let hex = [bytes[i + 1], bytes[i + 2]];
                if let Ok(val) = u8::from_str_radix(std::str::from_utf8(&hex).unwrap_or(""), 16) {
                    result.push(val);
                }
                i += 3;
            } else {
                result.push(b'=');
                i += 1;
            }
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    // Use the decoded bytes (lossily) rather than echoing the raw `=XX`
    // source when the result is not valid UTF-8.
    String::from_utf8_lossy(&result).into_owned()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(msg.body_text.as_deref(), Some("read </dev/null\na && b\n"));
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
        let raw = "From: a@b.com\nTo: b@c.com\n\nBody";
        let (h, b) = split_headers_body(raw);
        assert_eq!(h, "From: a@b.com\nTo: b@c.com");
        assert_eq!(b, "Body");
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
}
