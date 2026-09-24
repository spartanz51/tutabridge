//! Encoding/decoding of `MailSetEntry._id.element_id` (CustomId).
//!
//! A `MailSetEntry` is the placement of one [`Mail`] in one [`MailSet`]
//! (folder). Its element id is a 13-byte buffer encoded as base64url-no-pad:
//!
//! ```text
//! ┌──────────────────────────┬───────────────────────────────────────┐
//! │  4 bytes — timestamp     │  9 bytes — Mail.element_id (raw)      │
//! │  receivedDate >> 10 (BE) │                                       │
//! └──────────────────────────┴───────────────────────────────────────┘
//! ```
//!
//! The timestamp is shifted right by 10 bits — i.e. quantised to ~1.024s
//! resolution — to fit into 4 bytes (covers up to year ~2109). The 9 raw
//! bytes of the [`Mail`] id are appended verbatim; the same bytes encoded
//! with Tuta's `base64ext` alphabet form the [`GeneratedId`] string of the
//! mail.
//!
//! This module mirrors the TypeScript helpers in
//! `src/platform-kit/meta/EntityUtils.ts` (`constructMailSetEntryId` and
//! `deconstructMailSetEntryId`). Knowing the encoding lets a realtime event
//! consumer extract the `mail_id` directly from a `MailSetEntry` CREATE /
//! DELETE event, without any extra REST round-trip.
//!
//! [`Mail`]: crate::entities::generated::tutanota::Mail
//! [`MailSet`]: crate::entities::generated::tutanota::MailSet
//! [`GeneratedId`]: crate::GeneratedId

use base64::prelude::BASE64_URL_SAFE_NO_PAD;
use base64::Engine;
use thiserror::Error;

use crate::date::DateTime;
use crate::util::BASE64_EXT;
use crate::{CustomId, GeneratedId};

/// Size of a [`Mail`] `GeneratedId` in raw bytes (9 bytes = 12 base64ext chars).
///
/// [`Mail`]: crate::entities::generated::tutanota::Mail
const MAIL_ID_BYTES: usize = 9;

/// Size of the 4-byte big-endian timestamp prefix.
const TIMESTAMP_BYTES: usize = 4;

/// Total decoded length of a `MailSetEntry` element id.
const MAIL_SET_ENTRY_ID_BYTES: usize = TIMESTAMP_BYTES + MAIL_ID_BYTES;

/// The timestamp is encoded as `(millis >> SHIFT)` so 4 bytes are enough to
/// cover roughly 2 ^ 32 × 1024 ms ≈ year 2109.
const TIMESTAMP_SHIFT: u32 = 10;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MailSetEntryIdError {
	#[error("invalid base64url encoding for MailSetEntry id: {0}")]
	InvalidBase64Url(String),
	#[error(
		"unexpected MailSetEntry id length: got {got} bytes, expected {}",
		MAIL_SET_ENTRY_ID_BYTES
	)]
	WrongLength { got: usize },
	#[error("invalid base64ext encoding inside mail id: {0}")]
	InvalidMailIdEncoding(String),
}

/// Build the [`CustomId`] used as the element id of a `MailSetEntry`
/// referring to `mail_id` received at `receive_date`.
///
/// # Panics
///
/// Panics if `mail_id` does not decode to exactly `MAIL_ID_BYTES` (9) bytes
/// when interpreted as base64ext — i.e. if it is not a well-formed Tuta
/// `GeneratedId`. Such an id would never appear in practice; this guards
/// against misuse in tests / new code paths.
#[must_use]
pub fn construct(receive_date: DateTime, mail_id: &GeneratedId) -> CustomId {
	let mut buffer = [0u8; MAIL_SET_ENTRY_ID_BYTES];

	let truncated = (receive_date.as_millis() >> TIMESTAMP_SHIFT) as u32;
	buffer[..TIMESTAMP_BYTES].copy_from_slice(&truncated.to_be_bytes());

	let mail_bytes = BASE64_EXT
		.decode(mail_id.as_str())
		.expect("malformed Tuta GeneratedId (must be valid base64ext)");
	assert_eq!(
		mail_bytes.len(),
		MAIL_ID_BYTES,
		"Tuta GeneratedId must decode to {MAIL_ID_BYTES} bytes, got {}",
		mail_bytes.len(),
	);
	buffer[TIMESTAMP_BYTES..].copy_from_slice(&mail_bytes);

	CustomId(BASE64_URL_SAFE_NO_PAD.encode(buffer))
}

/// Split a [`CustomId`] coming from a `MailSetEntry._id` back into the
/// (received-date, mail id) pair it encodes. The returned timestamp has the
/// ~1s quantisation baked into the wire format (`millis & !0x3FF`) — fine for
/// sorting / display, but do not compare for strict equality with a fresh
/// `Date.now()`.
pub fn deconstruct(id: &CustomId) -> Result<(DateTime, GeneratedId), MailSetEntryIdError> {
	let buffer = BASE64_URL_SAFE_NO_PAD
		.decode(id.as_str())
		.map_err(|e| MailSetEntryIdError::InvalidBase64Url(e.to_string()))?;
	if buffer.len() != MAIL_SET_ENTRY_ID_BYTES {
		return Err(MailSetEntryIdError::WrongLength { got: buffer.len() });
	}

	let mut ts_bytes = [0u8; TIMESTAMP_BYTES];
	ts_bytes.copy_from_slice(&buffer[..TIMESTAMP_BYTES]);
	let truncated = u32::from_be_bytes(ts_bytes) as u64;
	let millis = truncated << TIMESTAMP_SHIFT;

	let mail_id = GeneratedId(BASE64_EXT.encode(&buffer[TIMESTAMP_BYTES..]));

	Ok((DateTime::from_millis(millis), mail_id))
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Test vector copied verbatim from the TS test suite
	/// (`test/tests/typerefs/EntityUtilsTest.ts`) — guarantees we stay
	/// wire-compatible with the official client.
	#[test]
	fn ts_test_vector() {
		let mail_id = GeneratedId("-----------0".to_string());
		// 2017-10-03T13:46:13Z = 1_507_038_373_000 ms.
		let receive_date = DateTime::from_millis(1_507_038_373_000);
		assert_eq!(
			construct(receive_date, &mail_id).as_str(),
			"V7ifKQAAAAAAAAAAAQ",
		);
	}

	#[test]
	fn deconstruct_recovers_the_mail_id() {
		let id = CustomId("V7ifKQAAAAAAAAAAAQ".to_string());
		let (_date, mail_id) = deconstruct(&id).expect("must decode the TS vector");
		assert_eq!(mail_id.as_str(), "-----------0");
	}

	#[test]
	fn deconstruct_recovers_a_quantised_timestamp() {
		// The wire format drops the bottom 10 bits, so the recovered
		// timestamp is rounded down to the previous 1024-ms boundary.
		let id = CustomId("V7ifKQAAAAAAAAAAAQ".to_string());
		let (date, _mail_id) = deconstruct(&id).unwrap();
		let original: u64 = 1_507_038_373_000;
		let expected: u64 = original & !((1u64 << TIMESTAMP_SHIFT) - 1);
		assert_eq!(date.as_millis(), expected);
		// And it stays close to the original.
		assert!(original - date.as_millis() < 1024);
	}

	#[test]
	fn round_trip_quantises_the_timestamp_but_keeps_the_mail_id() {
		// `construct` then `deconstruct` produces the same mail id back, and
		// a timestamp that matches the original modulo the quantisation.
		let cases = [
			(0u64, "------------"),
			(1_507_038_373_000u64, "-----------0"),
			(1_700_000_000_000u64, "------------"),
		];
		for (millis, mail_id_str) in cases {
			let mail_id = GeneratedId(mail_id_str.to_string());
			let custom = construct(DateTime::from_millis(millis), &mail_id);
			let (date, recovered) = deconstruct(&custom).unwrap();
			assert_eq!(
				recovered.as_str(),
				mail_id_str,
				"mail id changed by round-trip"
			);
			assert_eq!(
				date.as_millis(),
				millis & !((1u64 << TIMESTAMP_SHIFT) - 1),
				"timestamp does not match the expected quantisation",
			);
		}
	}

	#[test]
	fn deconstruct_rejects_a_short_id() {
		// 12 bytes of base64url payload = "AAAAAAAAAAAAAAAA" (16 chars).
		let id = CustomId("AAAAAAAAAAAAAAAA".to_string());
		assert_eq!(
			deconstruct(&id).unwrap_err(),
			MailSetEntryIdError::WrongLength { got: 12 },
		);
	}

	#[test]
	fn deconstruct_rejects_a_long_id() {
		// 14 bytes of base64url payload — explicitly more than the 13-byte
		// envelope so the parser cannot silently truncate.
		let id = CustomId("AAAAAAAAAAAAAAAAAAA".to_string());
		assert_eq!(
			deconstruct(&id).unwrap_err(),
			MailSetEntryIdError::WrongLength { got: 14 },
		);
	}

	#[test]
	fn deconstruct_rejects_invalid_base64url() {
		let id = CustomId("not base64 at all!".to_string());
		match deconstruct(&id) {
			Err(MailSetEntryIdError::InvalidBase64Url(_)) => {},
			other => panic!("expected InvalidBase64Url, got {other:?}"),
		}
	}

	#[test]
	#[should_panic(expected = "malformed Tuta GeneratedId")]
	fn construct_panics_on_a_non_generated_id() {
		// Passing something that does not decode to 9 bytes is a contract
		// violation — caller bug. We assert loudly rather than silently
		// truncate.
		let bogus = GeneratedId("!".to_string());
		let _ = construct(DateTime::from_millis(0), &bogus);
	}
}
