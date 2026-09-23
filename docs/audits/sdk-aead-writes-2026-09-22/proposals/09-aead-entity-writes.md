# Write AEAD entities with the authoritative instance nonce

Local technical proposal, not submitted. Prepared with AI assistance, disclosed here. Depends on proposal 08.

AeadEntityWriter creates entities with a new nonce and updates existing entities with a retained or server-registered nonce. Nonce migration uses the generated UpdateKdfNonceService and takes its response as authoritative. CryptoEntityClient's ordinary update path now preserves AEAD for an entity already carrying a nonce.

Transport integration tests exercise real serialization, complete mail round trips, fresh aggregate IDs, replacement of copied nonces, missing-nonce migration, server errors, malformed responses and refusal to send PUT after registration failure. The transport is simulated; real-account authorization and rollout are not claimed.

The optional versioned owner key is caller-resolved for owner-provider contexts. Default account rollout selection is left to the caller. Service payload encryption, including the bridge's draft-send path, remains CBC as in the audited TypeScript. The facade/API split is a proposal for review, not a claim that it is already the upstream preferred API.
