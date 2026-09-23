# Map optional encrypted empty values to null

Technical proposal only; not submitted. Prepared with AI assistance, disclosed here. See TS_PARITY.md for the reference implementation, scope differences and upstream policy.

An optional encrypted field received as an empty string is currently converted to empty bytes and sent to decryption, causing InvalidDataSizeError. Match the existing TypeScript CryptoMapper compatibility behavior by mapping this sentinel to null. Required values retain their existing behavior.

Validation: Tests cover optional Body.text and Body.compressedText for null, empty string, valid encoded content and invalid base64. A bridge regression test confirms a draft body with an empty optional field can be read.

Base: `aea5846b93a1412451e885bf99002401c3b087e8`. Patch: `../patches/05-optional-empty.patch`.

TS reference: CryptoMapper.decryptValue explicitly treats encrypted ZeroOrOne values containing an empty string as null, for historical cardinality changes. Required/encrypted and optional/unencrypted values retain their existing behavior.
