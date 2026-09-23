# Allow callers to complete second-factor challenges before login

Technical proposal only; not submitted. Prepared with AI assistance. The maintainers’ stated policy currently rejects such contributions; see REVIEW.md.

Expose the existing session creation step so callers can obtain credentials and challenges before resuming login. Add TOTP validation and challenge polling using generated services; share session creation with create_session. Reject unsupported password KDFs instead of silently assuming Argon2.

Validation: Three transport integration tests exercise session creation with/without challenges, unsupported KDFs, TOTP serialization and polling. Invalid tokens are covered by a unit test. Mobile binding generation and live accounts are not tested.

Base: `aea5846b93a1412451e885bf99002401c3b087e8`. Patch: `../patches/03-interactive-session.patch`.
