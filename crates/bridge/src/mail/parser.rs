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
    pub subject: String,
    pub body_html: String,
    pub attachments: Vec<Attachment>,
}

pub fn parse_rfc2822(raw: &str) -> ParsedMessage {
    let (header_section, body_section) = split_headers_body(raw);
    let headers = parse_headers(&header_section);

    let from_raw = get_header(&headers, "from").unwrap_or_default();
    let (from_name, from_address) = parse_address_single(&from_raw);

    let to = get_header(&headers, "to")
        .map(|v| parse_address_list(&v))
        .unwrap_or_default();
    let cc = get_header(&headers, "cc")
        .map(|v| parse_address_list(&v))
        .unwrap_or_default();
    let bcc = get_header(&headers, "bcc")
        .map(|v| parse_address_list(&v))
        .unwrap_or_default();

    let subject = get_header(&headers, "subject")
        .map(|s| decode_header_value(&s))
        .unwrap_or_default();

    let content_type = get_header(&headers, "content-type").unwrap_or_default();
    let content_transfer_encoding = get_header(&headers, "content-transfer-encoding")
        .unwrap_or_default()
        .to_lowercase();

    let (body_html, attachments) = if content_type.to_lowercase().contains("multipart/") {
        extract_multipart_body_and_attachments(&body_section, &content_type)
    } else {
        (
            decode_body(&body_section, &content_transfer_encoding, &content_type.to_lowercase()),
            Vec::new(),
        )
    };

    ParsedMessage {
        from_address,
        from_name,
        to,
        cc,
        bcc,
        subject,
        body_html,
        attachments,
    }
}

pub(super) fn split_headers_body(raw: &str) -> (String, String) {
    if let Some(pos) = raw.find("\r\n\r\n") {
        (raw[..pos].to_string(), raw[pos + 4..].to_string())
    } else if let Some(pos) = raw.find("\n\n") {
        (raw[..pos].to_string(), raw[pos + 2..].to_string())
    } else {
        (raw.to_string(), String::new())
    }
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
                headers.push((current_name.to_lowercase(), current_value.trim().to_string()));
            }
            current_name = name.trim().to_string();
            current_value = value.to_string();
        }
    }
    if !current_name.is_empty() {
        headers.push((current_name.to_lowercase(), current_value.trim().to_string()));
    }
    headers
}

pub(super) fn get_header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
}

fn parse_address_single(raw: &str) -> (String, String) {
    let raw = raw.trim();
    if let Some(lt) = raw.find('<') {
        if let Some(gt) = raw.find('>') {
            let addr = raw[lt + 1..gt].trim().to_string();
            let name = decode_header_value(raw[..lt].trim().trim_matches('"'));
            return (name, addr);
        }
    }
    (String::new(), raw.to_string())
}

fn parse_address_list(raw: &str) -> Vec<(String, String)> {
    let mut result = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();

    for ch in raw.chars() {
        match ch {
            '<' => {
                depth += 1;
                current.push(ch);
            }
            '>' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
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
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
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
            rest.split(|c: char| c.is_whitespace() || c == ';').next().unwrap_or("")
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
fn extract_multipart_body_and_attachments(
    body: &str,
    content_type: &str,
) -> (String, Vec<Attachment>) {
    let boundary = match extract_boundary(content_type) {
        Some(b) => b,
        None => return (body.to_string(), Vec::new()),
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
            || (extract_param(&part_ct, "name").is_some()
                && !part_ct_lower.contains("text/"));

        if part_ct_lower.contains("multipart/") {
            let (nested_body, nested_atts) =
                extract_multipart_body_and_attachments(&part_body, &part_ct);
            if html_part.is_none() && !nested_body.is_empty() {
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
                },
                cte if cte.contains("quoted-printable") => {
                    decode_quoted_printable(&part_body).into_bytes()
                },
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
            html_part = Some(decode_body(&part_body, &part_cte, &part_ct_lower));
        } else if part_ct_lower.contains("text/plain") && html_part.is_none() && text_part.is_none() {
            text_part = Some(decode_body(&part_body, &part_cte, &part_ct_lower));
        }
    }

    let body = html_part.or(text_part).unwrap_or_else(|| body.to_string());
    (body, attachments)
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
                parts.push(current.trim_start_matches("\r\n").trim_start_matches('\n').to_string());
            }
            break;
        }
        if line.starts_with(&delimiter) {
            if in_part && !current.is_empty() {
                parts.push(current.trim_start_matches("\r\n").trim_start_matches('\n').to_string());
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

fn decode_body(body: &str, transfer_encoding: &str, content_type: &str) -> String {
    let decoded = if transfer_encoding.contains("base64") {
        let clean: String = body.chars().filter(|c| !c.is_whitespace()).collect();
        base64::engine::general_purpose::STANDARD
            .decode(&clean)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_else(|| body.to_string())
    } else if transfer_encoding.contains("quoted-printable") {
        decode_quoted_printable(body)
    } else {
        body.to_string()
    };

    if content_type.contains("text/plain") && !content_type.contains("text/html") {
        format!("<pre>{}</pre>", html_escape(&decoded))
    } else {
        decoded
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
                if let Ok(val) = u8::from_str_radix(
                    std::str::from_utf8(&hex).unwrap_or(""),
                    16,
                ) {
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
    String::from_utf8(result).unwrap_or_else(|_| s.to_string())
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
}
