# Encode AEAD attributes with the existing Rust primitives

Local technical proposal, not submitted. Prepared with AI assistance, disclosed here. Builds on the read prototype (06 + 07).

EntityFacade can now encode version-2 group-key and version-3 session-key attributes. Reuse the existing entity traversal and value conversions while passing the root type and authenticated field path to AeadFacade. Preserve CBC behavior, encrypt AEAD empty values, and assign missing aggregate IDs before constructing their authenticated paths.

Tests compare the actual mapper output byte for byte with vectors produced by upstream TypeScript CryptoMapper, and cover invalid key contexts and sibling aggregate paths. The Rust traversal intentionally avoids the sibling-prefix accumulation reproduced in the upstream TS writer, following the TS reader's independent paths instead. See ../STYLE_AND_PARITY.md.

The legacy CBC entry point still refuses a nonce-bearing entity; callers must select its AEAD context. No changes to cryptographic primitives or generated protocol types.
