# Expose model-aware parsing of an already received entity

Technical proposal only; not submitted. Prepared with AI assistance. The maintainers’ stated policy currently rejects such contributions; see REVIEW.md.

Inline events and blob responses already contain the encrypted entity JSON. Expose EntityClient::parse_raw as a narrow entry point into the existing JsonSerializer, allowing callers to reuse the normal entity decryption pipeline.

Validation: The patch compiles independently. Bridge integration tests exercise the entry point with the upstream encrypted mail fixture, malformed JSON and draft bodies. This proposal adds no separate serializer implementation.

Base: `aea5846b93a1412451e885bf99002401c3b087e8`. Patch: `../patches/04-parse-raw.patch`.
