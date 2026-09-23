# Add scoped blob reads to the existing blob facade

Technical proposal only; not submitted. Prepared with AI assistance, disclosed here. See TS_PARITY.md for the reference implementation, scope differences and upstream policy.

The Rust SDK uploads blobs but cannot read them. Add instance-scoped tokens for attachment downloads and archive-scoped tokens for owned blob elements. Reuse the existing cache algorithm with typed keys, bound token refresh to one retry and validate binary framing before allocation. Downloads use batches of 100 and leave decryption to the caller.

Validation: SDK tests cover scopes, cache eviction, server failover, bounded refresh, binary framing and missing/extra blobs. A bridge integration test verifies encrypted multi-archive assembly and corrupted ciphertext rejection.

Base: `aea5846b93a1412451e885bf99002401c3b087e8`. Patch: `../patches/02-blob-reads.patch`.

TS parity: JSON GET input is encoded in the `_body` query parameter; GET succeeds only on HTTP 200. `with_read_token` retries the full archive read once on 403, matching the role of doBlobRequestWithRetry; failover follows tryServers. Counters/sizes are signed i32. Tests consume 11 binary and 8 retry vectors obtained by executing the upstream TS functions. Two malformed responses accepted by TS are explicitly rejected by the Rust framing checks.

Scope: one archive per call, no progress/cancellation API or archive-token cache promotion. Decryption uses existing primitives in the caller. These limitations are intentional and are not presented as full BlobFacade feature parity.
