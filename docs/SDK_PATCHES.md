# Vendored SDK

The bridge builds against Tuta's official Rust SDK, not a hand-maintained
fork. `tuta-repo` is **generated**: the official release named in
`sdk/BASE`, plus the patches in `sdk/patches/`, applied in order. Nothing in
the generated tree is edited by hand.

```
sdk/BASE                  official release tag + its commit
sdk/patches/NN-*.patch    our changes, one concern each
scripts/sdk-generate.sh   applies them, reproducibly
tuta-repo                 submodule pinned at the generated commit
crates/tuta               bridge-specific code that used to live in the fork
```

The result is hosted on the SDK fork (`spartanz51/tutanota`, branch
`generated/<version>`) only so that `cargo`, CI, `dev.sh` and the AUR
`-git` package keep building from a plain submodule. The fork is an output,
not a source.

## Checking a pin

```sh
scripts/sdk-generate.sh --check
```

Regenerates the SDK from `sdk/BASE` and `sdk/patches/` and fails unless the
pinned `tuta-repo` commit is exactly that. Generation is reproducible (fixed
committer, committer date taken from each patch), so the same inputs always
give the same commit. It also fails if the patches touch the SDK's
`Cargo.toml`: the client version Tuta checks must be the release's own,
never an edit. CI runs this on every pull request.

```sh
scripts/sdk-generate.sh --verify-each
```

Also checks every patch on its own: after each one, the SDK must be
formatted and pass its tests. Slower (about 8 minutes); run it whenever a
patch changes.

## Moving to a new Tuta release

1. Put the new tag and its commit in `sdk/BASE`.
2. `scripts/sdk-generate.sh`. A patch Tuta has taken is reported as
   already included: delete it. A patch that no longer applies is fixed in
   `sdk/patches/` (regenerate it with `git format-patch` from a fixed tree),
   never in `tuta-repo`. Then `scripts/sdk-generate.sh --verify-each`.
3. Run the SDK and bridge test suites, then a live check: log in, list and
   read mail, send one plaintext and one HTML message, read the log.
4. `scripts/sdk-generate.sh --push`, `git add tuta-repo sdk`, open a PR.

Tuta raises the minimum client version it accepts every few months (HTTP
474 below it), so staying on recent releases is what keeps the bridge able
to log in.

## The patches

Each patch is one concern, formatted, and builds and passes the SDK tests
on its own, with a few focused tests. Patches that could go to Tuta come
first, internal ones last; which is which, and their status, is tracked in
[`SDK_UPSTREAM.md`](SDK_UPSTREAM.md).

| Patch | Adds | Why it lives in the SDK |
|---|---|---|
| 01 optional empty | An empty optional encrypted value is null, as in TS `CryptoMapper` | Protocol fix. |
| 02 create session KDF | `create_session` normalizes the address and refuses a non-Argon2 KDF instead of sending a wrong verifier | Bug fix in the SDK's own login. |
| 03 load multiple | `load_multiple`: list elements by id, 100 per request | Transport and parsing are partly private; avoids one request per mail. |
| 04 blob downloads | `download_blobs`: instance-scoped read tokens, 100 blobs per request, retry and failover | Reuses the SDK's token service and blob servers. Attachments. |
| 05 blob elements | `load_blob_element`: a blob element (`MailDetailsBlob`) from its archive | Same pipeline as 04. Mail bodies. |
| 06 parse raw | `parse_raw`: a raw JSON entity through the serializer | Inline event payloads and blob contents use the official parser. |
| 07 interactive session | `initiate_session` and the TOTP second-factor calls | Reuses services, key derivation and session bootstrap. |
| 08 owner session key | `decrypt_parsed` with a session key from the owning instance | Blob elements and draft details have no key of their own. |
| 09 AEAD session reads | Decrypts AEAD v3 values with the session key and field context | Hooks the existing primitives into the entity decoder. |
| 10 AEAD group reads | Decrypts AEAD v2 values with versioned group keys and the KDF nonce | Key resolution belongs with the key loader. |

What the fork used to carry beyond this (event bus client, folder tree,
MOVE, `MailSetEntry` id codec, inline event decryption) composes from the
SDK's public API and now lives in `crates/tuta`.

AEAD **writes** are not included: they are prototyped on the
`lab/sdk-official` branch but not validated against a real account.

## History

Until September 2026 the SDK was a fork of tutao's 348 release carrying
twelve bridge commits. Its version string was raised to 359 to clear Tuta's
version floor (#38), then @ninjapanzer rebased the commits onto the real
359 release (#47) and documented why a relabelled version is not a real
one. This layout replaces the fork with generated releases, so a new Tuta
release is a one-line change to `sdk/BASE` plus a test run.
