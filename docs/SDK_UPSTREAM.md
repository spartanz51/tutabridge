# Upstreaming SDK patches to Tuta

Every patch in `sdk/patches/` is either a future pull request to Tuta's Rust
SDK (`tutao/tutanota`, `tuta-sdk/rust`) or explicitly internal. This file
tracks which, and in what state. The patch reference itself is
[`SDK_PATCHES.md`](SDK_PATCHES.md).

## Rules

- **One patch, one PR, same content.** A patch meant for Tuta is shaped as
  the PR it will become: one concern, small, formatted, 1 to 3 focused
  tests. What we apply is exactly what we propose, so an accepted PR simply
  removes the patch on the next release bump, and a refused one stays with
  no divergence to manage.
- **Written by hand.** Tuta does not accept LLM-assisted contributions. A
  patch goes upstream only once it has been rewritten by hand; that
  rewritten version then replaces the patch here, so the two stay
  identical.
- **Transparent.** Say who is behind the PR and why the bridge needs it:
  depending on Tuta's own SDK is safer for its users than maintaining a
  fork.
- **Fixes before features.** Tuta has said it does not want to grow the
  Rust SDK. Bug fixes with a TypeScript reference come first.
- **New PRs only.** Never reopen or comment on the closed ones below.

## Tracking

Status: `planned`, `maybe` (needs a decision or a different shape first),
`open #N`, `merged in <release>`, `declined #N`, `internal` (not meant for
Tuta, possibly revisited later).

| Patch | PR title | Status | Notes |
|---|---|---|---|
| 01 optional empty | Treat an empty optional encrypted value as null | planned | Bug fix, exact mirror of TS `CryptoMapper.decryptValue`. First candidate. |
| 02 normalize address | Normalize the mail address in create_session | planned | Bug fix, TS `LoginFacade` parity. |
| 03 account KDF | Derive the passphrase key with the account's KDF | planned | Bug fix: a Bcrypt account got a wrong verifier, and a panic in the KDF helper. |
| 04 load multiple | Load list elements by id in batches | planned | Same scope as declined #10854; only as a hand-written resubmission. |
| 05 blob downloads | | internal | Same area as declined #10870. An upstream version would follow TS, where blob elements load through `loadMultiple`. |
| 06 blob elements | | internal | Same. |
| 07 parse raw | | internal | Plumbing with no TS counterpart. |
| 08 interactive session | | internal | The declined #10871 scope. |
| 09 owner session key | Decrypt a parsed entity with a session key from its owner | maybe | TS passes an owner-encrypted key provider (`keyProviderFromInstance`) and leaves the child's own key fields alone; an upstream version would take that shape. |
| 10 AEAD session reads | | internal | Tuta is reworking AEAD on `crypto/dev` with a different v2 protocol. |
| 11 AEAD group reads | | internal | Same. |

## History

- **May 2026**: #10854 (load_multiple), #10870 (blob element reading) and
  #10871 (interactive 2FA) were submitted from the old SDK fork. All three
  were closed on 2026-07-03: Tuta does not accept LLM-assisted pull
  requests and did not want to add to the Rust SDK at the time.
- **September 2026**: #11539 (AEAD field paths for sibling aggregates) was
  closed as already fixed on Tuta's side.
