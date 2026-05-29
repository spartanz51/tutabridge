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

    if attachments.is_empty() {
        msg.push_str("Content-Type: text/html; charset=UTF-8\r\n");
        msg.push_str("Content-Transfer-Encoding: base64\r\n");
        msg.push_str("\r\n");
        msg.push_str(&base64_encode_body(body_text.as_bytes()));
        msg.push_str("\r\n");
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
        msg.push_str("Content-Type: text/html; charset=UTF-8\r\n");
        msg.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
        msg.push_str(&base64_encode_body(body_text.as_bytes()));
        msg.push_str("\r\n");

        for (file, data) in attachments {
            msg.push_str(&format!("--{}\r\n", boundary));
            let mime = file
                .mimeType
                .as_deref()
                .unwrap_or("application/octet-stream");
            let name_encoded = encode_header_value(&file.name);
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

pub(crate) fn format_address(addr: &MailAddress) -> String {
    if addr.name.is_empty() {
        addr.address.clone()
    } else {
        format!("{} <{}>", encode_header_value(&addr.name), addr.address)
    }
}

pub(crate) fn encode_header_value(s: &str) -> String {
    if s.is_ascii() && !s.contains('\r') && !s.contains('\n') {
        s.to_string()
    } else {
        format!(
            "=?UTF-8?B?{}?=",
            base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
        )
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_id(s: &str) -> tutasdk::GeneratedId {
        tutasdk::GeneratedId(s.to_string())
    }

    #[test]
    fn test_days_to_ymd_epoch() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
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
        // Body should be base64 of "<p>Hello World</p>"
        let body_b64 = base64::engine::general_purpose::STANDARD.encode(b"<p>Hello World</p>");
        assert!(rfc.contains(&body_b64));
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
        // Body part: text/html base64
        assert!(rfc.contains("Content-Type: text/html; charset=UTF-8\r\n"));
        let body_b64 = base64::engine::general_purpose::STANDARD.encode(b"<p>The body</p>");
        assert!(rfc.contains(&body_b64));
        // Attachment part
        assert!(rfc.contains("Content-Type: application/pdf; name=\"doc.pdf\""));
        assert!(rfc.contains("Content-Disposition: attachment; filename=\"doc.pdf\""));
        let pdf_b64 = base64::engine::general_purpose::STANDARD.encode(data);
        assert!(rfc.contains(&pdf_b64));
        // Closing boundary
        assert!(rfc.ends_with("--=_TutaBridge_list_att_elem_att--\r\n"));
    }
}
