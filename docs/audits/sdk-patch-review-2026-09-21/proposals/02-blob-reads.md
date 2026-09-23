# Add scoped blob reads to the existing blob facade

Technical proposal only; not submitted. Prepared with AI assistance. The maintainers’ stated policy currently rejects such contributions; see REVIEW.md.

The Rust SDK uploads blobs but cannot read them. Add instance-scoped tokens for attachment downloads and archive-scoped tokens for owned blob elements. Reuse the existing cache algorithm with typed keys, bound token refresh to one retry and validate binary framing before allocation. Downloads use batches of 100 and leave decryption to the caller.

Validation: SDK tests cover scopes, cache eviction, server failover, bounded refresh, binary framing and missing/extra blobs. A bridge integration test verifies encrypted multi-archive assembly and corrupted ciphertext rejection.

Base: `aea5846b93a1412451e885bf99002401c3b087e8`. Patch: `../patches/02-blob-reads.patch`.
