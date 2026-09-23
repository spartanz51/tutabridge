# Expose model-aware parsing of an already received entity

Technical proposal only; not submitted. Prepared with AI assistance, disclosed here. See TS_PARITY.md for the reference implementation, scope differences and upstream policy.

Inline events and blob responses already contain the encrypted entity JSON. Expose EntityClient::parse_raw as a narrow entry point into the existing JsonSerializer, allowing callers to reuse the normal entity decryption pipeline.

Validation: This patch is byte-identical to the version compiled independently in the preceding review; the current assembled series also compiles and passes the ordinary suites. Bridge integration tests exercise the entry point with the upstream encrypted mail fixture, malformed JSON and draft bodies. This proposal adds no separate serializer implementation.

Base: `aea5846b93a1412451e885bf99002401c3b087e8`. Patch: `../patches/04-parse-raw.patch`.

TS reference: TypeMapper.parseServerJson is already the common parsing entry point used by EntityRestClient. This Rust addition exposes its existing equivalent; it does not introduce a separate protocol mapper.
