# Allow callers to complete second-factor challenges before login

Technical proposal only; not submitted. Prepared with AI assistance, disclosed here. See TS_PARITY.md for the reference implementation, scope differences and upstream policy.

Expose the existing session creation step so callers can obtain credentials and challenges before resuming login. Add TOTP validation and challenge polling using generated services; share session creation with create_session. Reject unsupported password KDFs instead of silently assuming Argon2.

Validation: Three transport integration tests exercise session creation with/without challenges, unsupported KDFs, TOTP serialization and polling. Invalid tokens are covered by a unit test. UniFFI Kotlin and Swift bindings generate successfully with the new signature. Android/iOS compilation and live accounts are not tested.

Base: `aea5846b93a1412451e885bf99002401c3b087e8`. Patch: `../patches/03-interactive-session.patch`.

TS parity: callers supply clientIdentifier; generated SessionService and SecondFactorAuthService models are reused. The legacy create_session wrapper retains its previous client name. The TOTP session ID matches a value calculated by the upstream TS implementation. Scope is persistent Argon2 sessions; BCrypt migration, UI, repeated polling and cancellation are outside this change.
