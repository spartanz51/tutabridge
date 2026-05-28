//! Build the IMAP4rev1 `BODYSTRUCTURE` response for a cached RFC 2822 mail.
//!
//! Thunderbird tolerates a hardcoded `("TEXT" "HTML" ...)` reply even when the
//! actual body is `multipart/mixed` (it parses the body itself to surface
//! attachments), but stricter clients use BODYSTRUCTURE as the source of truth
//! for "does this message have files I can save?" — so we emit a proper
//! structure derived from the cached envelope.
//!
//! Format reference: RFC 3501 §7.4.2 (BODYSTRUCTURE) — basic form. Each part is
//! `("type" "subtype" (params) NIL NIL "encoding" size [lines])`; multipart
//! wraps its parts in `((part1)(part2)... "subtype" ("boundary" "..."))`.

use super::parser::{
    extract_boundary, get_header, parse_headers, split_headers_body, split_mime_parts,
};

/// Compute the parenthesised BODYSTRUCTURE payload for a full RFC 2822
/// envelope (no surrounding `BODYSTRUCTURE ` prefix — the caller composes
/// that with the appropriate FETCH response token).
pub fn compute_bodystructure(rfc2822: &str) -> String {
    let (headers_text, body) = split_headers_body(rfc2822);
    let headers = parse_headers(&headers_text);
    let content_type = get_header(&headers, "content-type")
        .unwrap_or_else(|| "text/html; charset=UTF-8".to_owned());
    let cte = get_header(&headers, "content-transfer-encoding")
        .unwrap_or_else(|| "7bit".to_owned());

    bodystructure_for(&content_type, &cte, &body, &headers)
}

fn bodystructure_for(
    content_type: &str,
    cte: &str,
    body: &str,
    headers: &[(String, String)],
) -> String {
    let ct_lower = content_type.to_lowercase();
    if ct_lower.starts_with("multipart/") {
        return multipart_structure(content_type, body);
    }
    single_part_structure(content_type, cte, body, headers)
}

fn multipart_structure(content_type: &str, body: &str) -> String {
    let boundary = extract_boundary(content_type).unwrap_or_default();
    let subtype = subtype_of(content_type).unwrap_or_else(|| "mixed".to_owned());

    let mut out = String::new();
    if boundary.is_empty() {
        // No boundary — treat as a degenerate single-part fallback so
        // clients see something instead of a malformed parenthesised tree.
        return format!(
            "(\"TEXT\" \"HTML\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" {} 0)",
            body.len()
        );
    }
    let parts = split_mime_parts(body, &boundary);
    for part in &parts {
        out.push_str(&part_structure(part));
    }

    out.push(' ');
    out.push_str(&quoted(&subtype.to_uppercase()));
    out.push(' ');
    out.push_str(&format!("(\"BOUNDARY\" {})", quoted(&boundary)));
    format!("({})", out)
}

fn part_structure(part: &str) -> String {
    let (headers_text, body) = split_headers_body(part);
    let headers = parse_headers(&headers_text);
    let content_type =
        get_header(&headers, "content-type").unwrap_or_else(|| "text/plain".to_owned());
    let cte = get_header(&headers, "content-transfer-encoding")
        .unwrap_or_else(|| "7bit".to_owned());

    bodystructure_for(&content_type, &cte, &body, &headers)
}

fn single_part_structure(
    content_type: &str,
    cte: &str,
    body: &str,
    headers: &[(String, String)],
) -> String {
    let (type_, subtype) = parse_type_subtype(content_type);
    let params = build_params(content_type);
    let size = body.len();
    let upper_cte = cte.to_uppercase();

    // `lines` is required for text/* parts and forbidden elsewhere.
    let lines_field = if type_.eq_ignore_ascii_case("text") {
        format!(" {}", count_lines(body))
    } else {
        String::new()
    };

    // Disposition extension: include only when the part declares one — that
    // way clients learn `attachment` + filename for the binary part and the
    // text/html body stays disposition-less.
    let disposition = build_disposition(headers);
    let ext = if let Some(d) = disposition {
        format!(" NIL {}", d) // md5 (NIL) + disposition
    } else {
        String::new()
    };

    format!(
        "({} {} {} NIL NIL {} {}{}{})",
        quoted(&type_.to_uppercase()),
        quoted(&subtype.to_uppercase()),
        params,
        quoted(&upper_cte),
        size,
        lines_field,
        ext,
    )
}

fn parse_type_subtype(content_type: &str) -> (String, String) {
    let head = content_type
        .split(';')
        .next()
        .unwrap_or("text/plain")
        .trim();
    let mut split = head.splitn(2, '/');
    let type_ = split.next().unwrap_or("text").trim().to_string();
    let subtype = split.next().unwrap_or("plain").trim().to_string();
    (type_, subtype)
}

fn subtype_of(content_type: &str) -> Option<String> {
    let head = content_type.split(';').next()?.trim();
    let mut split = head.splitn(2, '/');
    let _ = split.next()?;
    split.next().map(|s| s.trim().to_string())
}

/// Build the parenthesised parameter list of a Content-Type (e.g.
/// `("CHARSET" "UTF-8" "NAME" "doc.pdf")`). Returns the literal `NIL` when
/// the part has no parameters at all.
fn build_params(content_type: &str) -> String {
    let mut pairs: Vec<(String, String)> = Vec::new();
    // Skip the leading "type/subtype" segment, then walk `key=value` items.
    for raw in content_type.split(';').skip(1) {
        let item = raw.trim();
        if let Some(eq) = item.find('=') {
            let key = item[..eq].trim();
            let raw_value = item[eq + 1..].trim();
            let value = unquote(raw_value);
            if !key.is_empty() && !value.is_empty() {
                pairs.push((key.to_string(), value));
            }
        }
    }
    if pairs.is_empty() {
        return "NIL".to_string();
    }
    let mut s = String::from("(");
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&quoted(&k.to_uppercase()));
        s.push(' ');
        s.push_str(&quoted(v));
    }
    s.push(')');
    s
}

fn build_disposition(headers: &[(String, String)]) -> Option<String> {
    let cd = get_header(headers, "content-disposition")?;
    let head = cd.split(';').next()?.trim();
    if head.is_empty() {
        return None;
    }
    let mut pairs: Vec<(String, String)> = Vec::new();
    for raw in cd.split(';').skip(1) {
        let item = raw.trim();
        if let Some(eq) = item.find('=') {
            let key = item[..eq].trim();
            let raw_value = item[eq + 1..].trim();
            let value = unquote(raw_value);
            if !key.is_empty() && !value.is_empty() {
                pairs.push((key.to_string(), value));
            }
        }
    }
    let params = if pairs.is_empty() {
        "NIL".to_string()
    } else {
        let mut s = String::from("(");
        for (i, (k, v)) in pairs.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            s.push_str(&quoted(&k.to_uppercase()));
            s.push(' ');
            s.push_str(&quoted(v));
        }
        s.push(')');
        s
    };
    Some(format!("({} {})", quoted(&head.to_uppercase()), params))
}

fn count_lines(body: &str) -> usize {
    // RFC 3501 counts physical lines (CRLF-separated). An empty trailing
    // line that follows a final CRLF still counts.
    body.matches('\n').count()
}

/// Backslash-escape `\` and `"` then wrap in `"`. IMAP literal strings are an
/// option for arbitrary bytes but we keep it simple: every Content-Type / name
/// we emit is ASCII-printable after header decoding.
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn unquote(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_text_html_body() {
        let rfc = "Subject: t\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=UTF-8\r\nContent-Transfer-Encoding: base64\r\n\r\nPHA+aGk8L3A+";
        let bs = compute_bodystructure(rfc);
        assert!(bs.starts_with("(\"TEXT\" \"HTML\""));
        assert!(bs.contains("(\"CHARSET\" \"UTF-8\")"));
        assert!(bs.contains("\"BASE64\""));
        // Single-part: ends with the trailing ")" after lines field (no
        // disposition extension since the part has no Content-Disposition).
        assert!(bs.ends_with(')'));
    }

    #[test]
    fn multipart_mixed_with_pdf_attachment_emits_two_parts() {
        let rfc = "From: a\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"BB\"\r\n\r\n--BB\r\nContent-Type: text/html; charset=UTF-8\r\nContent-Transfer-Encoding: base64\r\n\r\nPHA+aGk8L3A+\r\n--BB\r\nContent-Type: application/pdf; name=\"doc.pdf\"\r\nContent-Transfer-Encoding: base64\r\nContent-Disposition: attachment; filename=\"doc.pdf\"\r\n\r\nJVBERi0=\r\n--BB--";
        let bs = compute_bodystructure(rfc);
        // Outer multipart wrapping
        assert!(bs.starts_with("(("));
        assert!(bs.contains("\"MIXED\""));
        assert!(bs.contains("\"BOUNDARY\" \"BB\""));
        // First part: text/html
        assert!(bs.contains("\"TEXT\" \"HTML\""));
        // Second part: application/pdf with disposition
        assert!(bs.contains("\"APPLICATION\" \"PDF\""));
        assert!(bs.contains("\"NAME\" \"doc.pdf\""));
        assert!(bs.contains("\"ATTACHMENT\""));
        assert!(bs.contains("\"FILENAME\" \"doc.pdf\""));
    }

    #[test]
    fn count_lines_basic() {
        assert_eq!(count_lines(""), 0);
        assert_eq!(count_lines("hello"), 0);
        assert_eq!(count_lines("a\nb\nc"), 2);
        assert_eq!(count_lines("a\r\nb\r\n"), 2);
    }

    #[test]
    fn quoted_escapes_quote_and_backslash() {
        assert_eq!(quoted("hi"), r#""hi""#);
        assert_eq!(quoted("a\"b"), r#""a\"b""#);
        assert_eq!(quoted("a\\b"), r#""a\\b""#);
    }

    #[test]
    fn build_params_picks_charset_and_name() {
        let s = build_params("application/pdf; name=\"doc.pdf\"; charset=UTF-8");
        assert!(s.contains("\"NAME\" \"doc.pdf\""));
        assert!(s.contains("\"CHARSET\" \"UTF-8\""));
    }

    #[test]
    fn build_params_returns_nil_when_no_params() {
        assert_eq!(build_params("text/html"), "NIL");
    }

    #[test]
    fn no_boundary_falls_back_to_text() {
        // Multipart with no boundary= is malformed; the BODYSTRUCTURE has
        // to keep producing valid output anyway.
        let rfc = "Content-Type: multipart/mixed\r\n\r\nbroken body";
        let bs = compute_bodystructure(rfc);
        assert!(bs.starts_with("(\"TEXT\" \"HTML\""));
    }

    #[test]
    fn nested_multipart_alternative_inside_mixed() {
        // Real-world clients sometimes wrap the body in multipart/alternative
        // (plain + html) and then add attachments via multipart/mixed.
        let rfc = "Content-Type: multipart/mixed; boundary=\"OUT\"\r\n\r\n--OUT\r\nContent-Type: multipart/alternative; boundary=\"IN\"\r\n\r\n--IN\r\nContent-Type: text/plain\r\n\r\nhi plain\r\n--IN\r\nContent-Type: text/html\r\n\r\n<p>hi</p>\r\n--IN--\r\n--OUT\r\nContent-Type: image/png; name=\"x.png\"\r\nContent-Disposition: attachment; filename=\"x.png\"\r\n\r\nABC\r\n--OUT--";
        let bs = compute_bodystructure(rfc);
        // The outer is mixed
        assert!(bs.contains("\"MIXED\""));
        assert!(bs.contains("\"BOUNDARY\" \"OUT\""));
        // The inner alternative is present
        assert!(bs.contains("\"ALTERNATIVE\""));
        assert!(bs.contains("\"BOUNDARY\" \"IN\""));
        // Both text parts and the image are present
        assert!(bs.contains("\"TEXT\" \"PLAIN\""));
        assert!(bs.contains("\"TEXT\" \"HTML\""));
        assert!(bs.contains("\"IMAGE\" \"PNG\""));
        assert!(bs.contains("\"FILENAME\" \"x.png\""));
    }
}
