use base64::Engine;
use tutasdk::entities::generated::tutanota::{Mail, MailAddress, MailDetails, TutanotaFile};

/// One decrypted attachment as it lands in the RFC 2822 we serve over IMAP:
/// the [`TutanotaFile`] entity (for name + MIME type + cid) and the raw
/// decrypted bytes (for the body of the part).
pub type AttachmentPart<'a> = (&'a TutanotaFile, &'a [u8]);

pub fn mail_to_rfc2822(
    mail: &Mail,
    details: Option<&MailDetails>,
    attachments: &[AttachmentPart<'_>],
) -> String {
    let mut msg = String::with_capacity(4096);

    let date_str = format_rfc2822_date(mail.receivedDate.as_millis());
    msg.push_str(&format!("Date: {}\r\n", date_str));

    msg.push_str(&format!("From: {}\r\n", format_address(&mail.sender)));

    msg.push_str(&format!(
        "Subject: {}\r\n",
        encode_header_value(&mail.subject)
    ));

    if let Some(details) = details {
        let to_addrs: Vec<String> = details
            .recipients
            .toRecipients
            .iter()
            .map(format_address)
            .collect();
        if !to_addrs.is_empty() {
            msg.push_str(&format!("To: {}\r\n", to_addrs.join(", ")));
        }

        let cc_addrs: Vec<String> = details
            .recipients
            .ccRecipients
            .iter()
            .map(format_address)
            .collect();
        if !cc_addrs.is_empty() {
            msg.push_str(&format!("Cc: {}\r\n", cc_addrs.join(", ")));
        }

        // Mail that arrives with a `Reply-To:` keeps it in Tuta's
        // `MailDetails.replyTos` (transactional senders use it constantly:
        // they send from a no-reply address and want answers elsewhere).
        // Without this header the client replies to `From:`, i.e. to the
        // wrong place, while the official Tuta client gets it right.
        let reply_tos: Vec<String> = details
            .replyTos
            .iter()
            .filter(|a| !a.address.trim().is_empty())
            .map(|a| format_name_and_address(&a.name, &a.address))
            .collect();
        if !reply_tos.is_empty() {
            msg.push_str(&format!("Reply-To: {}\r\n", reply_tos.join(", ")));
        }
    } else if let Some(ref first) = mail.firstRecipient {
        msg.push_str(&format!("To: {}\r\n", format_address(first)));
    }

    if let Some(ref id) = mail._id {
        msg.push_str(&format!(
            "Message-ID: <{}.{}@tutabridge.local>\r\n",
            id.list_id, id.element_id
        ));
    }

    msg.push_str("MIME-Version: 1.0\r\n");

    let body_text = details
        .and_then(|d| d.body.compressedText.as_deref().or(d.body.text.as_deref()))
        .unwrap_or("<p>(No body available)</p>");
    let body_plain = html_to_text(body_text);
    let alt_boundary = build_alt_boundary(mail);

    if attachments.is_empty() {
        push_alternative_body(&mut msg, &alt_boundary, &body_plain, body_text);
    } else {
        // The boundary is derived from the mail's element id so the same
        // mail always produces the same MIME boundary — keeps `.eml.enc`
        // bytes stable across rewrites.
        let boundary = build_boundary(mail);
        msg.push_str(&format!(
            "Content-Type: multipart/mixed; boundary=\"{}\"\r\n",
            boundary
        ));
        msg.push_str("\r\n");
        msg.push_str("This is a multi-part message in MIME format.\r\n");

        msg.push_str(&format!("--{}\r\n", boundary));
        push_alternative_body(&mut msg, &alt_boundary, &body_plain, body_text);

        for (file, data) in attachments {
            msg.push_str(&format!("--{}\r\n", boundary));
            let mime = file
                .mimeType
                .as_deref()
                .unwrap_or("application/octet-stream");
            let name_encoded = escape_param_value(&encode_header_value(&file.name));
            msg.push_str(&format!(
                "Content-Type: {}; name=\"{}\"\r\n",
                mime, name_encoded
            ));
            msg.push_str("Content-Transfer-Encoding: base64\r\n");
            msg.push_str(&format!(
                "Content-Disposition: attachment; filename=\"{}\"\r\n",
                name_encoded
            ));
            if let Some(ref cid) = file.cid {
                // Some Tuta files (inline images) carry a Content-ID — propagate
                // it so HTML `<img src="cid:…">` references still resolve.
                msg.push_str(&format!("Content-ID: <{}>\r\n", cid));
            }
            msg.push_str("\r\n");
            msg.push_str(&base64_encode_body(data));
            msg.push_str("\r\n");
        }
        msg.push_str(&format!("--{}--\r\n", boundary));
    }

    msg
}

/// Build a MIME boundary that is stable for a given mail and unlikely to
/// collide with payload bytes. Format: `=_TutaBridge_<list>_<elem>` where the
/// ids are the mail's `IdTuple` — they contain only base64-ext characters
/// (so safe in a Content-Type header) and uniquely identify the mail.
fn build_boundary(mail: &Mail) -> String {
    if let Some(ref id) = mail._id {
        format!("=_TutaBridge_{}_{}", id.list_id, id.element_id)
    } else {
        "=_TutaBridge_unknown".to_owned()
    }
}

/// Boundary for the inner `multipart/alternative` part. Same stability rules
/// as [`build_boundary`]; the `Alt` sits before the ids so neither boundary
/// is a prefix of the other (the lenient `split_mime_parts` matches on
/// `starts_with`, so a shared prefix would split the outer part on inner
/// delimiters).
fn build_alt_boundary(mail: &Mail) -> String {
    if let Some(ref id) = mail._id {
        format!("=_TutaBridgeAlt_{}_{}", id.list_id, id.element_id)
    } else {
        "=_TutaBridgeAlt_unknown".to_owned()
    }
}

/// Write the `multipart/alternative` body — text/plain first, text/html
/// second (RFC 2046 §5.1.4: order of increasing preference), both base64.
fn push_alternative_body(msg: &mut String, boundary: &str, plain: &str, html: &str) {
    msg.push_str(&format!(
        "Content-Type: multipart/alternative; boundary=\"{}\"\r\n",
        boundary
    ));
    msg.push_str("\r\n");
    msg.push_str(&format!("--{}\r\n", boundary));
    msg.push_str("Content-Type: text/plain; charset=UTF-8\r\n");
    msg.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
    msg.push_str(&base64_encode_body(plain.as_bytes()));
    msg.push_str("\r\n");
    msg.push_str(&format!("--{}\r\n", boundary));
    msg.push_str("Content-Type: text/html; charset=UTF-8\r\n");
    msg.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
    msg.push_str(&base64_encode_body(html.as_bytes()));
    msg.push_str(&format!("\r\n--{}--\r\n", boundary));
}

/// Convert a Tuta HTML body to text for the text/plain alternative part.
///
/// Must invert the storage encoding exactly, or `git am` breaks on received
/// patches. Two shapes recover byte-for-byte:
/// - escape+`<br>`, what `plaintext_to_tuta_html` writes for plaintext sends
///   — handled by the general path below.
/// - exactly one flat `<pre>` block of entity-escaped text, how bridge
///   versions before the plaintext fix stored them — the fast path.
///
/// Anything else gets a whitespace-PRESERVING strip: `<br>` and closing
/// block tags become newlines, script/style contents are dropped, entities
/// are decoded. (`strip_html` is unsuitable here — it collapses whitespace
/// for search indexing.)
pub(crate) fn html_to_text(html: &str) -> String {
    let trimmed = html.trim();
    let lower = trimmed.to_ascii_lowercase();
    if let Some(inner) = lower
        .strip_prefix("<pre>")
        .and_then(|r| r.strip_suffix("</pre>"))
    {
        // No raw '<' inside means this really is a single flat <pre> block —
        // escaped content never contains one. Tags are ASCII, so the byte
        // offsets found in `lower` slice `trimmed` safely.
        if !inner.contains('<') {
            return decode_entities(&trimmed[5..trimmed.len() - 6]);
        }
    }

    let mut out = String::with_capacity(trimmed.len());
    let mut chars = trimmed.chars();
    while let Some(c) = chars.next() {
        if c != '<' {
            out.push(c);
            continue;
        }
        let mut tag = String::new();
        for t in chars.by_ref() {
            if t == '>' {
                break;
            }
            tag.push(t.to_ascii_lowercase());
        }
        let closing = tag.starts_with('/');
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        if !closing && matches!(name, "script" | "style") {
            // Swallow everything until the matching closing tag.
            let close = format!("</{name}");
            let mut window = String::new();
            for t in chars.by_ref() {
                window.push(t.to_ascii_lowercase());
                if window.ends_with(&close) {
                    for t2 in chars.by_ref() {
                        if t2 == '>' {
                            break;
                        }
                    }
                    break;
                }
            }
        } else if name == "br"
            || (closing
                && matches!(
                    name,
                    "p" | "div"
                        | "tr"
                        | "li"
                        | "pre"
                        | "blockquote"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                ))
        {
            out.push('\n');
        }
    }
    decode_entities(&out)
}

pub(crate) fn format_address(addr: &MailAddress) -> String {
    format_name_and_address(&addr.name, &addr.address)
}

/// Render one address for a header: the bare address when there is no display
/// name, `Name <addr>` otherwise, with the name RFC 2047 encoded.
///
/// Shared by `To`/`Cc`/`Reply-To` so they cannot drift apart. Tuta's
/// `Reply-To` addresses are an `EncryptedMailAddress`, a different type from
/// `MailAddress` with the same two fields, hence the plain `&str` arguments.
///
/// CR and LF are stripped from the address: it comes from mail we did not
/// write, and a newline there would let it inject arbitrary headers into the
/// message we serve over IMAP.
pub(crate) fn format_name_and_address(name: &str, address: &str) -> String {
    let address = address.replace(['\r', '\n'], "");
    if name.is_empty() {
        return address;
    }
    format!("{} <{}>", format_display_name(name), address)
}

/// A display name safe to drop in an address header.
///
/// Non-ASCII (or CR/LF) becomes an RFC 2047 encoded-word, which must not be
/// quoted or the receiver shows the raw `=?UTF-8?B?...?=`. Otherwise a name
/// holding RFC 5322 specials is quoted, with `\` and `"` escaped: an
/// unquoted comma is the dangerous one, since `Doe, John <a@b.c>` parses as
/// two addresses rather than one.
fn format_display_name(name: &str) -> String {
    if !name.is_ascii() || name.contains('\r') || name.contains('\n') {
        return encode_header_value(name);
    }
    const SPECIALS: [char; 12] = ['(', ')', '<', '>', '[', ']', ':', ';', '@', '\\', ',', '"'];
    if name.contains(SPECIALS) {
        format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        name.to_string()
    }
}

/// Escape a value going inside a quoted MIME parameter (`name="..."`).
///
/// A filename comes from mail we did not write. An unescaped `"` in it would
/// close the parameter early and let the rest be read as further parameters,
/// e.g. `name="evil".pdf"; x="y"`. RFC 2045 quoted-string rules: backslash
/// and double quote are backslash escaped. An RFC 2047 encoded-word contains
/// neither, so this is a no-op for encoded names.
fn escape_param_value(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

pub(crate) fn encode_header_value(s: &str) -> String {
    if s.is_ascii() && !s.contains('\r') && !s.contains('\n') {
        return s.to_string();
    }
    // RFC 2047: an encoded-word must be at most 75 chars. `=?UTF-8?B?` + `?=`
    // is 12, so cap each word's base64 payload at 63 chars, i.e. 45 input
    // bytes (base64 of 45 bytes is 60 chars, total 72). A multibyte UTF-8
    // char is never split across words. Several words fold onto continuation
    // lines (RFC 5322 header folding), which a decoder rejoins.
    const MAX_BYTES: usize = 45;
    let mut words: Vec<String> = Vec::new();
    let mut chunk: Vec<u8> = Vec::new();
    for ch in s.chars() {
        let mut buf = [0u8; 4];
        let bytes = ch.encode_utf8(&mut buf).as_bytes();
        if !chunk.is_empty() && chunk.len() + bytes.len() > MAX_BYTES {
            words.push(encode_word(&chunk));
            chunk.clear();
        }
        chunk.extend_from_slice(bytes);
    }
    if !chunk.is_empty() {
        words.push(encode_word(&chunk));
    }
    // Empty input stays empty, matching the pre-fold behaviour.
    words.join("\r\n ")
}

fn encode_word(bytes: &[u8]) -> String {
    format!(
        "=?UTF-8?B?{}?=",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

pub(crate) fn format_rfc2822_date(millis: u64) -> String {
    let secs = millis / 1000;
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    let weekday = ((days + 4) % 7) as usize; // 0=Sun, epoch was Thursday
    let weekdays = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

    let (year, month, day) = days_to_ymd(days);
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let month_idx = month.saturating_sub(1).min(11) as usize;

    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} +0000",
        weekdays[weekday], day, months[month_idx], year, hours, minutes, seconds
    )
}

/// Howard Hinnant's civil_from_days algorithm
/// Returns (year, month 1-12, day 1-31)
pub(crate) fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

pub(crate) fn format_internal_date(millis: u64) -> String {
    let secs = millis / 1000;
    let days = secs / 86400;
    let tod = secs % 86400;
    let h = tod / 3600;
    let m = (tod % 3600) / 60;
    let s = tod % 60;

    let (year, month, day) = days_to_ymd(days);
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let month_idx = month.saturating_sub(1).min(11) as usize;

    format!(
        "{:02}-{}-{:04} {:02}:{:02}:{:02} +0000",
        day, months[month_idx], year, h, m, s
    )
}

pub(crate) fn base64_encode_body(data: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    encoded
        .as_bytes()
        .chunks(76)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\r\n")
}

pub(crate) fn extract_headers(rfc: &str) -> String {
    if let Some(pos) = rfc.find("\r\n\r\n") {
        format!("{}\r\n", &rfc[..pos + 2])
    } else {
        rfc.to_string()
    }
}

/// Strip HTML markup down to readable text for full-text indexing. Drops tags,
/// the contents of `<script>`/`<style>` blocks, and decodes the handful of
/// entities that actually show up in mail bodies, then collapses whitespace.
/// This is lossy by design — it only needs to be good enough that a body
/// search matches the words a human would see, not the markup around them.
pub(crate) fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut chars = html.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            // Capture the tag name to detect script/style blocks we must skip
            // wholesale (their text content is not human-visible).
            let mut tag = String::new();
            for t in chars.clone().take(6) {
                if t == '>' || t.is_whitespace() {
                    break;
                }
                tag.push(t.to_ascii_lowercase());
            }
            let skip_block = matches!(tag.trim_start_matches('/'), "script" | "style");
            // Consume up to and including the closing '>'.
            for t in chars.by_ref() {
                if t == '>' {
                    break;
                }
            }
            if skip_block && !tag.starts_with('/') {
                // Swallow everything until the matching closing tag.
                let close = format!("</{tag}");
                let mut window = String::new();
                for t in chars.by_ref() {
                    window.push(t.to_ascii_lowercase());
                    if window.ends_with(&close) {
                        // Drop the rest of the closing tag.
                        for t2 in chars.by_ref() {
                            if t2 == '>' {
                                break;
                            }
                        }
                        break;
                    }
                }
            }
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    let decoded = decode_entities(&out);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_entities(s: &str) -> String {
    // `&amp;` strictly last: decoding it earlier makes a literal `&lt;` in
    // the source text (stored as `&amp;lt;`) collapse to `<` in a second
    // pass — a real hazard for patches touching HTML/XML.
    s.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Extract the readable body text from one of our own RFC 2822 messages, for
/// full-text indexing: locate the first `text/*` part, base64-decode it, and
/// strip HTML. Works for both the single-part and multipart layouts produced
/// by [`mail_to_rfc2822`]. Returns an empty string when no text part is found.
pub(crate) fn extract_body_text(rfc: &str) -> String {
    // Find the first textual MIME part.
    let lower = rfc.to_lowercase();
    let part_start = lower.find("text/html").or_else(|| lower.find("text/plain"));
    let Some(part_start) = part_start else {
        return String::new();
    };
    // Body begins after that part's header/body separator.
    let Some(sep) = rfc[part_start..].find("\r\n\r\n") else {
        return String::new();
    };
    let body_start = part_start + sep + 4;
    // Body ends at the next MIME boundary delimiter (multipart) or end of input.
    let body_end = rfc[body_start..]
        .find("\r\n--")
        .map(|i| body_start + i)
        .unwrap_or(rfc.len());
    let raw = &rfc[body_start..body_end];

    let stripped_b64: String = raw.split_whitespace().collect();
    match base64::engine::general_purpose::STANDARD.decode(stripped_b64) {
        Ok(bytes) => strip_html(&String::from_utf8_lossy(&bytes)),
        // Not base64 (shouldn't happen for our own messages) — treat as text.
        Err(_) => strip_html(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_id(s: &str) -> tutasdk::GeneratedId {
        tutasdk::GeneratedId(s.to_string())
    }

    // --- RFC 2047 encoded-word length / folding ---

    #[test]
    fn short_non_ascii_is_a_single_encoded_word() {
        let out = encode_header_value("Héllo");
        assert!(out.starts_with("=?UTF-8?B?") && out.ends_with("?="));
        assert!(!out.contains("\r\n"), "should not fold: {out}");
        assert!(out.len() <= 75, "single word must be <= 75: {out}");
    }

    #[test]
    fn long_non_ascii_folds_into_words_each_within_75() {
        let long = "Rénée ".repeat(30); // ~ 210 bytes UTF-8
        let out = encode_header_value(&long);
        assert!(out.contains("\r\n "), "must fold: {out}");
        for word in out.split("\r\n ") {
            assert!(
                word.len() <= 75,
                "encoded-word exceeds 75 chars ({}): {word}",
                word.len()
            );
            assert!(word.starts_with("=?UTF-8?B?") && word.ends_with("?="));
        }
    }

    #[test]
    fn every_rendered_header_line_is_within_998_octets() {
        // The hard SMTP line limit. A long non-ASCII subject used to blow
        // past it as one giant encoded-word.
        let (mut mail, details) = mail_with_reply_tos(&[]);
        mail.subject = "Δοκιμή πολύ μακροσκελούς θέματος ".repeat(20);
        mail.sender.name = "Пётр Великий ".repeat(10);
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        for line in rfc.split("\r\n") {
            assert!(
                line.len() <= 998,
                "header/body line too long: {}",
                line.len()
            );
        }
    }

    #[test]
    fn folded_long_name_round_trips_back_to_the_text() {
        // A folded display name must survive re-parsing (our parser unfolds
        // continuation lines, like any mail client's).
        let name = "Χρήστος Παπαδόπουλος ".repeat(6);
        let (to, _) = render_then_parse(&[(name.trim(), "c@x.com")], &[]);
        assert_eq!(to.len(), 1, "folding split the recipient: {to:?}");
        assert_eq!(to[0].1, "c@x.com");
        // The decoded name is the original with any inter-word space folded
        // out; compare with whitespace collapsed so a cosmetic space between
        // words does not fail the check.
        let got: String = to[0].0.split_whitespace().collect();
        let want: String = name.split_whitespace().collect();
        assert_eq!(got, want, "name did not decode back: {:?}", to[0].0);
    }

    // --- round-trip: our rendered headers must survive a re-parse ---

    /// A parsed `(name, address)` address list.
    type AddrList = Vec<(String, String)>;

    /// Render a received mail, then parse it back with the real submission
    /// parser (what a mail client's parser must also accept). Returns the
    /// reparsed lists for To and Reply-To.
    fn render_then_parse(to: &[(&str, &str)], reply_tos: &[(&str, &str)]) -> (AddrList, AddrList) {
        let (mail, mut details) = mail_with_reply_tos(reply_tos);
        details.recipients.toRecipients = to
            .iter()
            .map(|(name, addr)| MailAddress {
                _id: None,
                name: (*name).to_string(),
                address: (*addr).to_string(),
                contact: None,
                _errors: Default::default(),
            })
            .collect();
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        let parsed = crate::mail::parser::parse_rfc2822(&rfc);
        (parsed.to, parsed.reply_to)
    }

    #[test]
    fn roundtrip_comma_name_stays_one_recipient() {
        let (to, _) = render_then_parse(&[("Doe, John", "j@x.com")], &[]);
        assert_eq!(to.len(), 1, "comma split the recipient: {to:?}");
        assert_eq!(to[0].1, "j@x.com");
    }

    #[test]
    fn roundtrip_parens_and_at_in_name() {
        let (to, _) = render_then_parse(&[("Sales (EU) @HQ", "s@x.com")], &[]);
        assert_eq!(to.len(), 1, "{to:?}");
        assert_eq!(to[0].1, "s@x.com");
    }

    #[test]
    fn roundtrip_two_recipients_one_with_comma() {
        let (to, _) = render_then_parse(&[("Doe, John", "j@x.com"), ("Ann", "a@x.com")], &[]);
        assert_eq!(to.len(), 2, "{to:?}");
        assert_eq!(to[0].1, "j@x.com");
        assert_eq!(to[1].1, "a@x.com");
    }

    #[test]
    fn roundtrip_reply_to_addresses_survive() {
        let (_, rt) = render_then_parse(
            &[("", "me@tuta.io")],
            &[("Support, Ltd", "help@x.com"), ("", "second@x.com")],
        );
        assert_eq!(rt.len(), 2, "reply-to list mangled: {rt:?}");
        assert_eq!(rt[0].1, "help@x.com");
        assert_eq!(rt[1].1, "second@x.com");
    }

    #[test]
    fn roundtrip_non_ascii_name() {
        let (to, _) = render_then_parse(&[("Café Réçu", "c@x.com")], &[]);
        assert_eq!(to.len(), 1, "{to:?}");
        assert_eq!(to[0].1, "c@x.com");
        assert_eq!(to[0].0, "Café Réçu", "name did not decode back: {to:?}");
    }

    // --- more Reply-To edge cases ---

    #[test]
    fn reply_to_mixes_blank_and_real_keeping_order() {
        let (mail, details) =
            mail_with_reply_tos(&[("", "  "), ("A", "a@x.com"), ("", ""), ("B", "b@x.com")]);
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        assert_eq!(
            reply_to_header(&rfc).as_deref(),
            Some("Reply-To: A <a@x.com>, B <b@x.com>")
        );
    }

    #[test]
    fn reply_to_whitespace_only_address_is_skipped() {
        let (mail, details) = mail_with_reply_tos(&[("Ghost", "\t \t")]);
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        assert!(reply_to_header(&rfc).is_none(), "{rfc:?}");
    }

    // --- header-rendering hardening: adversarial inputs ---

    /// Every header line of a rendered message, up to the blank line.
    fn header_lines(rfc: &str) -> Vec<String> {
        rfc.split("\r\n\r\n")
            .next()
            .unwrap_or("")
            .lines()
            .map(|l| l.to_string())
            .collect()
    }

    /// Header names a rendered message is allowed to contain. Anything else
    /// appearing at the start of a line means a value injected one.
    fn assert_no_injected_header(rfc: &str) {
        const ALLOWED: [&str; 8] = [
            "Date:",
            "From:",
            "Subject:",
            "To:",
            "Cc:",
            "Reply-To:",
            "Message-ID:",
            "MIME-Version:",
        ];
        for line in header_lines(rfc) {
            if line.is_empty() || line.starts_with("Content-") || line.starts_with(' ') {
                continue;
            }
            assert!(
                ALLOWED.iter().any(|h| line.starts_with(h)),
                "unexpected header line, value injected one: {line:?}"
            );
        }
    }

    #[test]
    fn crlf_in_subject_cannot_inject_a_header() {
        let (mut mail, details) = mail_with_reply_tos(&[]);
        mail.subject = "hi\r\nBcc: victim@x.com".to_string();
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        assert_no_injected_header(&rfc);
    }

    #[test]
    fn crlf_in_display_name_cannot_inject_a_header() {
        let (mut mail, details) = mail_with_reply_tos(&[]);
        mail.sender.name = "evil\r\nBcc: victim@x.com".to_string();
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        assert_no_injected_header(&rfc);
    }

    #[test]
    fn crlf_in_reply_to_name_cannot_inject_a_header() {
        let (mail, details) = mail_with_reply_tos(&[("bad\r\nBcc: victim@x.com", "a@b.c")]);
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        assert_no_injected_header(&rfc);
    }

    #[test]
    fn crlf_in_recipient_address_cannot_inject_a_header() {
        let (mail, mut details) = mail_with_reply_tos(&[]);
        details.recipients.toRecipients[0].address = "a@b.c\r\nBcc: victim@x.com".to_string();
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        assert_no_injected_header(&rfc);
    }

    #[test]
    fn recipient_with_a_comma_in_the_name_stays_one_address() {
        let (mail, mut details) = mail_with_reply_tos(&[]);
        details.recipients.toRecipients[0].name = "Doe, John".to_string();
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        let to = header_lines(&rfc)
            .into_iter()
            .find(|l| l.starts_with("To:"))
            .expect("To header");
        assert_eq!(to, "To: \"Doe, John\" <me@tuta.io>", "got {to}");
    }

    #[test]
    fn attachment_filename_with_a_quote_cannot_break_the_parameter() {
        use tutasdk::entities::generated::tutanota::TutanotaFile;
        let (mail, details) = mail_with_reply_tos(&[]);
        let file = TutanotaFile {
            _id: None,
            _permissions: test_id("p"),
            _format: 0,
            _ownerEncSessionKey: None,
            _ownerGroup: None,
            _ownerKeyVersion: None,
            name: "evil\".pdf\"; x=\"y".to_string(),
            mimeType: Some("application/pdf".to_string()),
            size: 3,
            cid: None,
            blobs: vec![],
            parent: None,
            subFiles: None,
            _kdfNonce: None,
            _errors: Default::default(),
        };
        let data: Vec<u8> = vec![1, 2, 3];
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[(&file, &data)]);
        // The filename must not be able to close its own quoted parameter and
        // start another one. Escaped quotes stay inside the value, so only
        // UNESCAPED ones delimit parameters and they must come in pairs.
        for line in rfc.lines().filter(|l| l.starts_with("Content-")) {
            let delimiters = line
                .replace("\\\\", "")
                .replace("\\\"", "")
                .matches('"')
                .count();
            assert!(delimiters % 2 == 0, "unbalanced parameter quotes: {line:?}");
            // and the injected parameter must not have become a real one
            assert!(
                !line.contains("; x=\"y\""),
                "filename smuggled a parameter: {line:?}"
            );
        }
    }

    #[test]
    fn attachment_filename_with_crlf_cannot_inject_a_header() {
        use tutasdk::entities::generated::tutanota::TutanotaFile;
        let (mail, details) = mail_with_reply_tos(&[]);
        let file = TutanotaFile {
            _id: None,
            _permissions: test_id("p"),
            _format: 0,
            _ownerEncSessionKey: None,
            _ownerGroup: None,
            _ownerKeyVersion: None,
            name: "ok.pdf\r\nBcc: victim@x.com".to_string(),
            mimeType: Some("application/pdf".to_string()),
            size: 3,
            cid: None,
            blobs: vec![],
            parent: None,
            subFiles: None,
            _kdfNonce: None,
            _errors: Default::default(),
        };
        let data: Vec<u8> = vec![1, 2, 3];
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[(&file, &data)]);
        assert!(
            !rfc.lines().any(|l| l.starts_with("Bcc:")),
            "header injected: {rfc:?}"
        );
    }

    #[test]
    fn body_cannot_collide_with_the_mime_boundary() {
        // The body is base64 encoded, so a body quoting the boundary cannot
        // terminate the part early.
        let (mail, mut details) = mail_with_reply_tos(&[]);
        details.body.text = Some("--=_TutaBridgeAlt_rl_re--\r\nsmuggled: yes".to_string());
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        assert_eq!(
            rfc.matches("--=_TutaBridgeAlt_rl_re--").count(),
            1,
            "the closing boundary must appear exactly once: {rfc:?}"
        );
    }

    // --- Reply-To on received mail ---

    /// Minimal `Mail` + `MailDetails` pair for header-rendering tests, with
    /// `replyTos` built from `(name, address)` pairs.
    fn mail_with_reply_tos(pairs: &[(&str, &str)]) -> (Mail, MailDetails) {
        use tutasdk::date::DateTime;
        use tutasdk::entities::generated::tutanota::{Body, EncryptedMailAddress, Recipients};
        use tutasdk::IdTupleGenerated;

        let mail = Mail {
            _id: Some(IdTupleGenerated::new(test_id("rl"), test_id("re"))),
            _permissions: test_id("perm"),
            _format: 0,
            _ownerEncSessionKey: None,
            subject: "Receipt".to_string(),
            receivedDate: DateTime::from_millis(0),
            state: 2,
            unread: false,
            confidential: false,
            replyType: 0,
            _ownerGroup: None,
            differentEnvelopeSender: None,
            listUnsubscribe: false,
            movedTime: None,
            phishingStatus: 0,
            authStatus: None,
            method: 0,
            recipientCount: 1,
            encryptionAuthStatus: None,
            _ownerKeyVersion: None,
            processingState: 0,
            processNeeded: false,
            sendAt: None,
            serverClassificationData: None,
            _kdfNonce: None,
            sender: MailAddress {
                _id: None,
                name: String::new(),
                address: "noreply@uber.com".to_string(),
                contact: None,
                _errors: Default::default(),
            },
            attachments: vec![],
            conversationEntry: IdTupleGenerated::new(test_id("cl"), test_id("ce")),
            firstRecipient: None,
            mailDetails: None,
            mailDetailsDraft: None,
            bucketKey: None,
            sets: vec![],
            clientSpamClassifierResult: None,
            _errors: Default::default(),
        };

        let details = MailDetails {
            _id: None,
            sentDate: DateTime::from_millis(0),
            authStatus: 0,
            replyTos: pairs
                .iter()
                .map(|(name, addr)| EncryptedMailAddress {
                    _id: None,
                    name: (*name).to_string(),
                    address: (*addr).to_string(),
                    _errors: Default::default(),
                })
                .collect(),
            recipients: Recipients {
                _id: None,
                toRecipients: vec![MailAddress {
                    _id: None,
                    name: String::new(),
                    address: "me@tuta.io".to_string(),
                    contact: None,
                    _errors: Default::default(),
                }],
                ccRecipients: vec![],
                bccRecipients: vec![],
            },
            headers: None,
            body: Body {
                _id: None,
                text: Some("<p>body</p>".to_string()),
                compressedText: None,
                _errors: Default::default(),
            },
        };
        (mail, details)
    }

    fn reply_to_header(rfc: &str) -> Option<String> {
        rfc.lines()
            .find(|l| l.starts_with("Reply-To:"))
            .map(|l| l.trim_end().to_string())
    }

    #[test]
    fn reply_to_is_emitted_with_empty_display_name() {
        // The real-world shape: transactional senders (Uber, Kraken) send from
        // a no-reply address and set a bare Reply-To with no display name.
        let (mail, details) = mail_with_reply_tos(&[("", "no-reply@replies.uber.com")]);
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        assert_eq!(
            reply_to_header(&rfc).as_deref(),
            Some("Reply-To: no-reply@replies.uber.com")
        );
    }

    #[test]
    fn reply_to_keeps_the_display_name() {
        let (mail, details) = mail_with_reply_tos(&[("Support", "help@example.com")]);
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        assert_eq!(
            reply_to_header(&rfc).as_deref(),
            Some("Reply-To: Support <help@example.com>")
        );
    }

    #[test]
    fn reply_to_joins_several_addresses() {
        let (mail, details) = mail_with_reply_tos(&[("One", "one@x.com"), ("", "two@x.com")]);
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        assert_eq!(
            reply_to_header(&rfc).as_deref(),
            Some("Reply-To: One <one@x.com>, two@x.com")
        );
    }

    #[test]
    fn no_reply_tos_emits_no_header() {
        // An absent Reply-To must stay absent, not become an empty header or
        // silently echo the sender.
        let (mail, details) = mail_with_reply_tos(&[]);
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        assert!(reply_to_header(&rfc).is_none(), "got: {rfc:?}");
        assert!(!rfc.contains("Reply-To"));
    }

    #[test]
    fn blank_reply_to_address_is_skipped() {
        let (mail, details) = mail_with_reply_tos(&[("Ghost", "   ")]);
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        assert!(reply_to_header(&rfc).is_none(), "got: {rfc:?}");
    }

    #[test]
    fn reply_to_encodes_a_non_ascii_display_name() {
        let (mail, details) = mail_with_reply_tos(&[("Café Support", "cafe@example.com")]);
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        let hdr = reply_to_header(&rfc).unwrap();
        assert!(hdr.contains("=?UTF-8?B?"), "name must be encoded: {hdr}");
        assert!(hdr.contains("<cafe@example.com>"));
    }

    #[test]
    fn reply_to_cannot_inject_headers() {
        // The address comes from mail we did not write; a newline in it must
        // not be able to append headers to the message we serve.
        let (mail, details) = mail_with_reply_tos(&[("", "evil@x.com\r\nBcc: victim@x.com")]);
        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);
        // Injection means a new header LINE, so check line starts: the
        // address text itself may legitimately contain those characters.
        assert!(
            !rfc.lines().any(|l| l.starts_with("Bcc:")),
            "header injected: {rfc:?}"
        );
        assert_eq!(
            reply_to_header(&rfc).as_deref(),
            Some("Reply-To: evil@x.comBcc: victim@x.com")
        );
    }

    // --- format_name_and_address ---

    #[test]
    fn display_name_with_a_comma_is_quoted() {
        // Unquoted, `Doe, John <a@b.c>` parses as TWO addresses. This also
        // affected To and Cc before the formatter was shared.
        assert_eq!(
            format_name_and_address("Doe, John", "a@b.c"),
            "\"Doe, John\" <a@b.c>"
        );
    }

    #[test]
    fn display_name_specials_are_quoted_and_escaped() {
        assert_eq!(
            format_name_and_address("Support (EU)", "a@b.c"),
            "\"Support (EU)\" <a@b.c>"
        );
        assert_eq!(format_name_and_address("a@b", "a@b.c"), "\"a@b\" <a@b.c>");
        // Inner quote and backslash are escaped, not dropped.
        assert_eq!(
            format_name_and_address("He said \"hi\"", "a@b.c"),
            "\"He said \\\"hi\\\"\" <a@b.c>"
        );
        assert_eq!(
            format_name_and_address("back\\slash", "a@b.c"),
            "\"back\\\\slash\" <a@b.c>"
        );
    }

    #[test]
    fn plain_display_name_is_left_alone() {
        // No specials means no quotes, so existing renderings do not churn.
        assert_eq!(
            format_name_and_address("John Doe", "a@b.c"),
            "John Doe <a@b.c>"
        );
        assert_eq!(
            format_name_and_address("Mr Smith", "a@b.c"),
            "Mr Smith <a@b.c>"
        );
    }

    #[test]
    fn non_ascii_display_name_is_encoded_not_quoted() {
        // An RFC 2047 encoded-word inside quotes is not decoded by receivers,
        // so encoding must win over quoting.
        let out = format_name_and_address("Café, Support", "a@b.c");
        assert!(out.starts_with("=?UTF-8?B?"), "got {out}");
        assert!(!out.contains('"'), "encoded-word must not be quoted: {out}");
    }

    #[test]
    fn format_name_and_address_shapes() {
        assert_eq!(format_name_and_address("", "a@b.c"), "a@b.c");
        assert_eq!(format_name_and_address("Bob", "a@b.c"), "Bob <a@b.c>");
        // CR/LF in the address is stripped, never passed through.
        assert_eq!(format_name_and_address("", "a@b.c\r\nX: y"), "a@b.cX: y");
    }

    #[test]
    fn test_days_to_ymd_epoch() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn strip_html_drops_tags_and_decodes_entities() {
        let html = "<p>Hello&nbsp;<b>World</b> &amp; goodbye</p>";
        assert_eq!(strip_html(html), "Hello World & goodbye");
    }

    #[test]
    fn strip_html_skips_script_and_style() {
        let html = "<style>.a{color:red}</style><div>Visible</div><script>alert(1)</script>";
        assert_eq!(strip_html(html), "Visible");
    }

    #[test]
    fn extract_body_text_from_single_part() {
        let body = base64::engine::general_purpose::STANDARD.encode("<p>Quarterly invoice</p>");
        let rfc = format!(
            "Subject: Test\r\nContent-Type: text/html; charset=UTF-8\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{body}\r\n"
        );
        assert_eq!(extract_body_text(&rfc), "Quarterly invoice");
    }

    #[test]
    fn extract_body_text_from_multipart_stops_at_boundary() {
        let body = base64::engine::general_purpose::STANDARD.encode("<p>Body words here</p>");
        let rfc = format!(
            "Content-Type: multipart/mixed; boundary=\"BND\"\r\n\r\n\
             --BND\r\nContent-Type: text/html; charset=UTF-8\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{body}\r\n\
             --BND\r\nContent-Type: application/pdf\r\n\r\nIGNOREDATTACHMENT\r\n--BND--\r\n"
        );
        assert_eq!(extract_body_text(&rfc), "Body words here");
    }

    #[test]
    fn test_days_to_ymd_known_dates() {
        // 2024-01-01 = day 19723 since epoch
        assert_eq!(days_to_ymd(19723), (2024, 1, 1));
        // 2000-02-29 (leap year) = day 11016
        assert_eq!(days_to_ymd(11016), (2000, 2, 29));
        // 2026-05-20 = day 20593
        assert_eq!(days_to_ymd(20593), (2026, 5, 20));
    }

    #[test]
    fn test_format_rfc2822_date_epoch() {
        let result = format_rfc2822_date(0);
        assert_eq!(result, "Thu, 01 Jan 1970 00:00:00 +0000");
    }

    #[test]
    fn test_format_rfc2822_date_known() {
        // 2024-12-25 12:37:25 UTC = 1735130245000 ms
        let result = format_rfc2822_date(1735130245000);
        assert_eq!(result, "Wed, 25 Dec 2024 12:37:25 +0000");
    }

    #[test]
    fn test_format_internal_date_epoch() {
        let result = format_internal_date(0);
        assert_eq!(result, "01-Jan-1970 00:00:00 +0000");
    }

    #[test]
    fn test_format_internal_date_known() {
        let result = format_internal_date(1735130245000);
        assert_eq!(result, "25-Dec-2024 12:37:25 +0000");
    }

    #[test]
    fn test_encode_header_ascii() {
        assert_eq!(encode_header_value("Hello World"), "Hello World");
    }

    #[test]
    fn test_encode_header_utf8() {
        let result = encode_header_value("Héllo Wörld");
        assert!(result.starts_with("=?UTF-8?B?"));
        assert!(result.ends_with("?="));

        // Decode to verify round-trip
        let b64_part = &result[10..result.len() - 2];
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64_part)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), "Héllo Wörld");
    }

    #[test]
    fn test_encode_header_with_newline() {
        let result = encode_header_value("Line1\r\nLine2");
        assert!(result.starts_with("=?UTF-8?B?"));
    }

    #[test]
    fn test_encode_header_empty() {
        assert_eq!(encode_header_value(""), "");
    }

    #[test]
    fn test_format_address_name_and_email() {
        let addr = MailAddress {
            _id: None,
            name: "John Doe".to_string(),
            address: "john@example.com".to_string(),
            contact: None,
            _errors: Default::default(),
        };
        assert_eq!(format_address(&addr), "John Doe <john@example.com>");
    }

    #[test]
    fn test_format_address_email_only() {
        let addr = MailAddress {
            _id: None,
            name: "".to_string(),
            address: "john@example.com".to_string(),
            contact: None,
            _errors: Default::default(),
        };
        assert_eq!(format_address(&addr), "john@example.com");
    }

    #[test]
    fn test_format_address_utf8_name() {
        let addr = MailAddress {
            _id: None,
            name: "Jéan-François".to_string(),
            address: "jf@example.com".to_string(),
            contact: None,
            _errors: Default::default(),
        };
        let result = format_address(&addr);
        assert!(result.contains("=?UTF-8?B?"));
        assert!(result.ends_with(" <jf@example.com>"));
    }

    #[test]
    fn test_base64_encode_body_short() {
        let result = base64_encode_body(b"Hello");
        assert_eq!(result, "SGVsbG8=");
    }

    #[test]
    fn test_base64_encode_body_long_wraps() {
        let long_text = "A".repeat(200);
        let result = base64_encode_body(long_text.as_bytes());
        for line in result.split("\r\n") {
            assert!(line.len() <= 76, "Line too long: {} chars", line.len());
        }
    }

    #[test]
    fn test_base64_encode_body_empty() {
        assert_eq!(base64_encode_body(b""), "");
    }

    #[test]
    fn test_extract_headers_normal() {
        let rfc = "From: a@b.com\r\nTo: c@d.com\r\n\r\nBody here";
        let headers = extract_headers(rfc);
        // extract_headers includes the trailing \r\n\r\n separator
        assert_eq!(headers, "From: a@b.com\r\nTo: c@d.com\r\n\r\n");
        assert!(!headers.contains("Body"));
    }

    #[test]
    fn test_extract_headers_no_body() {
        let rfc = "From: a@b.com\r\nTo: c@d.com";
        let headers = extract_headers(rfc);
        assert_eq!(headers, rfc);
    }

    #[test]
    fn test_mail_to_rfc2822_minimal() {
        use tutasdk::date::DateTime;
        use tutasdk::IdTupleGenerated;

        let mail = Mail {
            _id: Some(IdTupleGenerated::new(test_id("list1"), test_id("elem1"))),
            _permissions: test_id("perm1"),
            _format: 0,
            _ownerEncSessionKey: None,
            subject: "Test Subject".to_string(),
            receivedDate: DateTime::from_millis(1735130245000),
            state: 2,
            unread: false,
            confidential: false,
            replyType: 0,
            _ownerGroup: None,
            differentEnvelopeSender: None,
            listUnsubscribe: false,
            movedTime: None,
            phishingStatus: 0,
            authStatus: None,
            method: 0,
            recipientCount: 1,
            encryptionAuthStatus: None,
            _ownerKeyVersion: None,
            processingState: 0,
            processNeeded: false,
            sendAt: None,
            serverClassificationData: None,
            _kdfNonce: None,
            sender: MailAddress {
                _id: None,
                name: "Alice".to_string(),
                address: "alice@tuta.com".to_string(),
                contact: None,
                _errors: Default::default(),
            },
            attachments: vec![],
            conversationEntry: IdTupleGenerated::new(test_id("conv_list1"), test_id("conv_elem1")),
            firstRecipient: Some(MailAddress {
                _id: None,
                name: "Bob".to_string(),
                address: "bob@example.com".to_string(),
                contact: None,
                _errors: Default::default(),
            }),
            mailDetails: None,
            mailDetailsDraft: None,
            bucketKey: None,
            sets: vec![],
            clientSpamClassifierResult: None,
            _errors: Default::default(),
        };

        let rfc = mail_to_rfc2822(&mail, None, &[]);

        assert!(rfc.contains("Date: Wed, 25 Dec 2024 12:37:25 +0000\r\n"));
        assert!(rfc.contains("From: Alice <alice@tuta.com>\r\n"));
        assert!(rfc.contains("Subject: Test Subject\r\n"));
        assert!(rfc.contains("To: Bob <bob@example.com>\r\n"));
        assert!(rfc.contains("MIME-Version: 1.0\r\n"));
        assert!(rfc.contains("Content-Type: text/html; charset=UTF-8\r\n"));
        assert!(rfc.contains("Content-Transfer-Encoding: base64\r\n"));
        assert!(rfc.contains("Message-ID: <"));
        // Body should be base64 of "<p>(No body available)</p>"
        assert!(rfc.contains("\r\n\r\n"));
    }

    #[test]
    fn test_mail_to_rfc2822_with_details() {
        use tutasdk::date::DateTime;
        use tutasdk::entities::generated::tutanota::{Body, Recipients};
        use tutasdk::IdTupleGenerated;

        let mail = Mail {
            _id: Some(IdTupleGenerated::new(test_id("list2"), test_id("elem2"))),
            _permissions: test_id("perm2"),
            _format: 0,
            _ownerEncSessionKey: None,
            subject: "With Details".to_string(),
            receivedDate: DateTime::from_millis(0),
            state: 2,
            unread: true,
            confidential: false,
            replyType: 0,
            _ownerGroup: None,
            differentEnvelopeSender: None,
            listUnsubscribe: false,
            movedTime: None,
            phishingStatus: 0,
            authStatus: None,
            method: 0,
            recipientCount: 2,
            encryptionAuthStatus: None,
            _ownerKeyVersion: None,
            processingState: 0,
            processNeeded: false,
            sendAt: None,
            serverClassificationData: None,
            _kdfNonce: None,
            sender: MailAddress {
                _id: None,
                name: "".to_string(),
                address: "sender@tuta.com".to_string(),
                contact: None,
                _errors: Default::default(),
            },
            attachments: vec![],
            conversationEntry: IdTupleGenerated::new(test_id("conv_list2"), test_id("conv_elem2")),
            firstRecipient: None,
            mailDetails: None,
            mailDetailsDraft: None,
            bucketKey: None,
            sets: vec![],
            clientSpamClassifierResult: None,
            _errors: Default::default(),
        };

        let details = MailDetails {
            _id: None,
            sentDate: DateTime::from_millis(0),
            authStatus: 0,
            replyTos: vec![],
            recipients: Recipients {
                _id: None,
                toRecipients: vec![
                    MailAddress {
                        _id: None,
                        name: "Bob".to_string(),
                        address: "bob@example.com".to_string(),
                        contact: None,
                        _errors: Default::default(),
                    },
                    MailAddress {
                        _id: None,
                        name: "".to_string(),
                        address: "charlie@example.com".to_string(),
                        contact: None,
                        _errors: Default::default(),
                    },
                ],
                ccRecipients: vec![MailAddress {
                    _id: None,
                    name: "Dave".to_string(),
                    address: "dave@example.com".to_string(),
                    contact: None,
                    _errors: Default::default(),
                }],
                bccRecipients: vec![],
            },
            headers: None,
            body: Body {
                _id: None,
                text: Some("<p>Hello World</p>".to_string()),
                compressedText: None,
                _errors: Default::default(),
            },
        };

        let rfc = mail_to_rfc2822(&mail, Some(&details), &[]);

        assert!(rfc.contains("From: sender@tuta.com\r\n"));
        assert!(rfc.contains("To: Bob <bob@example.com>, charlie@example.com\r\n"));
        assert!(rfc.contains("Cc: Dave <dave@example.com>\r\n"));
        // multipart/alternative with text/plain BEFORE text/html
        assert!(rfc.contains(
            "Content-Type: multipart/alternative; boundary=\"=_TutaBridgeAlt_list2_elem2\"\r\n"
        ));
        let plain_pos = rfc.find("Content-Type: text/plain").unwrap();
        let html_pos = rfc.find("Content-Type: text/html").unwrap();
        assert!(plain_pos < html_pos);
        // html part is base64 of the stored body, plain part of its conversion
        let body_b64 = base64::engine::general_purpose::STANDARD.encode(b"<p>Hello World</p>");
        assert!(rfc.contains(&body_b64));
        let plain_b64 = base64::engine::general_purpose::STANDARD.encode(b"Hello World\n");
        assert!(rfc.contains(&plain_b64));
        assert!(rfc.ends_with("--=_TutaBridgeAlt_list2_elem2--\r\n"));

        // BODYSTRUCTURE reflects the alternative tree
        let bs = crate::mail::bodystructure::compute_bodystructure(&rfc);
        assert!(bs.contains("\"ALTERNATIVE\""));
        assert!(bs.contains("\"PLAIN\""));
        assert!(bs.contains("\"HTML\""));

        // and the search indexer still finds the readable text
        assert_eq!(extract_body_text(&rfc), "Hello World");
    }

    #[test]
    fn test_mail_to_rfc2822_with_attachment_emits_multipart() {
        use tutasdk::date::DateTime;
        use tutasdk::entities::generated::tutanota::{Body, Recipients, TutanotaFile};
        use tutasdk::IdTupleGenerated;

        let mail = Mail {
            _id: Some(IdTupleGenerated::new(
                test_id("list_att"),
                test_id("elem_att"),
            )),
            _permissions: test_id("perm_att"),
            _format: 0,
            _ownerEncSessionKey: None,
            subject: "With Attachment".to_string(),
            receivedDate: DateTime::from_millis(0),
            state: 2,
            unread: false,
            confidential: false,
            replyType: 0,
            _ownerGroup: None,
            differentEnvelopeSender: None,
            listUnsubscribe: false,
            movedTime: None,
            phishingStatus: 0,
            authStatus: None,
            method: 0,
            recipientCount: 1,
            encryptionAuthStatus: None,
            _ownerKeyVersion: None,
            processingState: 0,
            processNeeded: false,
            sendAt: None,
            serverClassificationData: None,
            _kdfNonce: None,
            sender: MailAddress {
                _id: None,
                name: "Alice".to_string(),
                address: "alice@tuta.com".to_string(),
                contact: None,
                _errors: Default::default(),
            },
            attachments: vec![],
            conversationEntry: IdTupleGenerated::new(test_id("conv_l"), test_id("conv_e")),
            firstRecipient: Some(MailAddress {
                _id: None,
                name: "".to_string(),
                address: "bob@example.com".to_string(),
                contact: None,
                _errors: Default::default(),
            }),
            mailDetails: None,
            mailDetailsDraft: None,
            bucketKey: None,
            sets: vec![],
            clientSpamClassifierResult: None,
            _errors: Default::default(),
        };
        let details = MailDetails {
            _id: None,
            sentDate: DateTime::from_millis(0),
            authStatus: 0,
            replyTos: vec![],
            recipients: Recipients {
                _id: None,
                toRecipients: vec![],
                ccRecipients: vec![],
                bccRecipients: vec![],
            },
            headers: None,
            body: Body {
                _id: None,
                text: Some("<p>The body</p>".to_string()),
                compressedText: None,
                _errors: Default::default(),
            },
        };
        let file = TutanotaFile {
            _id: Some(IdTupleGenerated::new(
                test_id("file_list"),
                test_id("file_elem"),
            )),
            _permissions: test_id("file_perm"),
            _format: 0,
            _ownerEncSessionKey: None,
            name: "doc.pdf".to_string(),
            size: 5,
            mimeType: Some("application/pdf".to_string()),
            _ownerGroup: None,
            cid: None,
            _ownerKeyVersion: None,
            _kdfNonce: None,
            parent: None,
            subFiles: None,
            blobs: vec![],
            _errors: Default::default(),
        };
        let data: &[u8] = b"PDFDA";
        let attachments: Vec<super::AttachmentPart> = vec![(&file, data)];
        let rfc = mail_to_rfc2822(&mail, Some(&details), &attachments);

        assert!(rfc.contains(
            "Content-Type: multipart/mixed; boundary=\"=_TutaBridge_list_att_elem_att\""
        ));
        assert!(rfc.contains("--=_TutaBridge_list_att_elem_att\r\n"));
        // Body part: nested multipart/alternative (plain + html), closed
        // before the attachment part starts.
        assert!(rfc.contains(
            "Content-Type: multipart/alternative; boundary=\"=_TutaBridgeAlt_list_att_elem_att\""
        ));
        assert!(rfc.contains("Content-Type: text/plain; charset=UTF-8\r\n"));
        assert!(rfc.contains("Content-Type: text/html; charset=UTF-8\r\n"));
        let alt_close = rfc.find("--=_TutaBridgeAlt_list_att_elem_att--").unwrap();
        let att_part = rfc.find("Content-Type: application/pdf").unwrap();
        assert!(alt_close < att_part);
        let body_b64 = base64::engine::general_purpose::STANDARD.encode(b"<p>The body</p>");
        assert!(rfc.contains(&body_b64));
        // Attachment part
        assert!(rfc.contains("Content-Type: application/pdf; name=\"doc.pdf\""));
        assert!(rfc.contains("Content-Disposition: attachment; filename=\"doc.pdf\""));
        let pdf_b64 = base64::engine::general_purpose::STANDARD.encode(data);
        assert!(rfc.contains(&pdf_b64));
        // Closing boundary
        assert!(rfc.ends_with("--=_TutaBridge_list_att_elem_att--\r\n"));

        // BODYSTRUCTURE: mixed( alternative(plain, html), pdf )
        let bs = crate::mail::bodystructure::compute_bodystructure(&rfc);
        assert!(bs.contains("\"MIXED\""));
        assert!(bs.contains("\"ALTERNATIVE\""));
        assert!(bs.contains("\"PLAIN\""));
        let alt_pos = bs.find("\"ALTERNATIVE\"").unwrap();
        let pdf_pos = bs.find("\"PDF\"").unwrap();
        assert!(alt_pos < pdf_pos);
    }

    #[test]
    fn html_to_text_recovers_pre_wrapped_patch_exactly() {
        // Bridge versions before the plaintext fix stored a text/plain
        // submission as <pre>{html_escape(body)}</pre>. That mail is still
        // in mailboxes, so the conversion back must stay byte-exact or
        // `git am` corrupts patches containing < > &.
        let patch = "Subject: [PATCH] fix\n\n\
                     diff --git a/x.c b/x.c\n\
                     -if (a < b && c > d)\n\
                     +if (a <= b || c >= d)\n\
                     \t\"quoted\"  double  spaces\n";
        let escaped = patch
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        let stored = format!("<pre>{}</pre>", escaped);
        assert_eq!(html_to_text(&stored), patch);
    }

    #[test]
    fn html_to_text_pre_with_inner_tags_falls_through_to_strip() {
        // Not a flat pre block — must take the lossy path, not exact recovery.
        let html = "<pre>line one<br>line two</pre>";
        assert_eq!(html_to_text(html), "line one\nline two\n");
    }

    #[test]
    fn html_to_text_converts_breaks_and_blocks_to_newlines() {
        let html = "<div>first</div><p>second<br>third</p><style>.x{}</style><script>bad()</script>tail &amp; end";
        assert_eq!(html_to_text(html), "first\nsecond\nthird\ntail & end");
    }

    #[test]
    fn html_to_text_preserves_whitespace() {
        let html = "<pre>a  b\n\tc</pre>";
        assert_eq!(html_to_text(html), "a  b\n\tc");
    }
}
