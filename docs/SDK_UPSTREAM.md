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

Status: `planned`, `open #N`, `merged in <release>`, `declined #N`,
`internal` (not meant for Tuta).

| Patch | PR title | Status | Notes |
|---|---|---|---|
| 05 optional empty | Treat empty optional encrypted values as null | planned | Bug fix, exact mirror of TS `CryptoMapper.decryptValue`. First candidate. |
| 01 batch | Add load_multiple to EntityClient | planned | Same scope as declined #10854; only as a hand-written resubmission. |
| 02 blob reads | Blob read tokens, blob download, BlobElement loading | planned, needs reshaping | Same area as declined #10870. To be split into PR-sized patches in the TS `loadMultiple` shape. |
| 03 interactive session | KDF check and address normalisation in create_session | planned, needs splitting | The bug fix part only; the 2FA API is the declined #10871 scope and stays internal. |
| 04 parse raw | | internal | Plumbing with no TS counterpart. |
| 06 AEAD session reads | | internal | Tuta is reworking AEAD on `crypto/dev` with a different v2 protocol. |
| 07 AEAD group reads | | internal | Same. Its owner-session-key decryption part may become a separate candidate. |

## History

- **May 2026**: #10854 (load_multiple), #10870 (blob element reading) and
  #10871 (interactive 2FA) were submitted from the old SDK fork. All three
  were closed on 2026-07-03: Tuta does not accept LLM-assisted pull
  requests and did not want to add to the Rust SDK at the time.
- **September 2026**: #11539 (AEAD field paths for sibling aggregates) was
  closed as already fixed on Tuta's side.
