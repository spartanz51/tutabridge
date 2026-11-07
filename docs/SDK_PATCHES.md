# Official SDK and local patches

`tuta-repo` points directly at an official `tutao/tutanota` release commit.
The bridge consumes a prepared local copy, never the unpatched submodule.
No patched SDK commits are published to a fork.

```
sdk/BASE                  official release tag and exact commit
tuta-repo                 official submodule, kept clean
sdk/patches/NN-*.patch     ordered changes, one concern each
sdk/EXPECTED              base, patch SHA256s, patched tree, exported digest
sdk/generated             ignored prepared SDK, used by Cargo and fixtures
scripts/sdk-prepare.py    offline preparation and verification
crates/tuta               bridge-specific extensions and build guard
```

## Building and checking

Initialize the submodule, then prepare the SDK before any Cargo command:

```sh
git submodule sync
git submodule update --init tuta-repo
python3 scripts/sdk-prepare.py
cargo build --locked
```

Preparation needs Python 3 and Git, and works offline once the official commit
is available. It applies patches in an isolated Git index, verifies the full
tree against EXPECTED, and extracts the root manifest, license, Rust SDK and
compatibility fixture into sdk/generated. Source checkouts stay unchanged;
Git may retain unreachable tree/blob objects until garbage collection.

The content digest includes sorted UTF-8 relative paths and raw file bytes,
with u64 big-endian lengths. Modes and timestamps are excluded for Windows
portability. Symlinks and special files are refused. Git configuration is
isolated and export uses LF bytes on all platforms. Successful repeated
preparation is a no-op; a corrupted output is rebuilt in staging and verified
before replacement. Do not run preparation concurrently with compilation.

```sh
python3 scripts/sdk-prepare.py check
python3 scripts/sdk-prepare.py --verify-each
```

`check` verifies inputs and output without modifying them. `--verify-each`
also formats/checks and tests the complete SDK after every patch. Tests run
in disposable source directories, not in sdk/generated. Prepared sources
must remain unchanged; keep build output in the bridge's target directory.

The Rust build guard checks BASE, EXPECTED, patch SHA256s and all generated
file bytes. It runs without Python, Git, network access or writes. Missing,
stale or altered SDK content prevents a successful bridge build. Cargo may
load dependency manifests and start compiling dependencies before the guard;
it is an integrity check, not a sandbox for untrusted code. Sources and
locked dependencies are reproducible; bit-identical binaries additionally
require a controlled compiler and build environment.

## Updating patches or the official base

Change sdk/patches deliberately; never edit generated files. For a new base,
update sdk/BASE and the official submodule gitlink together. Then run:

```sh
python3 scripts/sdk-prepare.py --update-expected --verify-each
cargo update --workspace
cargo test --locked --workspace
```

Review EXPECTED and Cargo.lock with the source changes. The ordinary build
never updates EXPECTED. A patch already included upstream fails with a
removal diagnostic; partially integrated patches require manual review.
Patches may not change SDK crate versions. The client version remains the
official release's own. Offline tests do not prove server acceptance.

## The patches

Each patch is one concern, formatted, and builds and passes the SDK tests
on its own, with a few focused tests. Patches that could go to Tuta come
first, internal ones last; which is which, and their status, is tracked in
[`SDK_UPSTREAM.md`](SDK_UPSTREAM.md).

| Patch | Adds | Why it lives in the SDK |
|---|---|---|
| 01 optional empty | An empty optional encrypted value is null, as in TS `CryptoMapper` | Protocol fix. |
| 02 normalize address | `create_session` trims and lowercases the address for the salt and the session, as TS does | Fix in the SDK's own login. |
| 03 account KDF | The passphrase key uses the KDF the salt service reports; Bcrypt is an error instead of a panic | Fix in the SDK's own login. |
| 04 load multiple | `load_multiple`: list elements by id, 100 per request | Transport and parsing are partly private; avoids one request per mail. |
| 05 blob downloads | `download_blobs`: instance-scoped read tokens, 100 blobs per request, retry and failover | Reuses the SDK's token service and blob servers. Attachments. |
| 06 blob elements | `load_blob_element`: a blob element (`MailDetailsBlob`) from its archive | Same pipeline as 05. Mail bodies. |
| 07 parse raw | `parse_raw`: a raw JSON entity through the serializer | Inline event payloads and blob contents use the official parser. |
| 08 interactive session | `initiate_session` and the TOTP second-factor calls | Reuses services, key derivation and session bootstrap. |
| 09 owner session key | `decrypt_parsed` with a session key from the owning instance | Blob elements and draft details have no key of their own. |
| 10 AEAD session reads | Decrypts AEAD v3 values with the session key and field context | Hooks the existing primitives into the entity decoder. |
| 11 AEAD group reads | Decrypts AEAD v2 values with versioned group keys and the KDF nonce | Key resolution belongs with the key loader. |
| 12 bucket instance keys | Retains keys for attachment children from their parent Mail | Decapsulation and key context remain in the SDK. |
| 13 Android logger | Honors the `logging` feature before installing the Android logger | The CLI already owns its logger; installing a second global logger panics on first SDK initialization. |

What the fork used to carry beyond this (event bus client, folder tree,
MOVE, `MailSetEntry` id codec, inline event decryption) composes from the
SDK's public API and now lives in `crates/tuta`.

AEAD **writes** are not included: they are prototyped on the
`lab/sdk-official` branch but not validated against a real account.

## Automatic releases

Every other Monday at 06:00 UTC (anchor 2026-10-19), sdk-bump.yml selects the
latest official release. The alternate weekly wake only resumes an unfinished
release. A manual dispatch bypasses the calendar guard. The GUI's six-hour
update check is independent.

The isolated verifier prepares the SDK, tests each patch and resolves the
bridge lockfile. It passes the patched tree and bounded lock diff/digest to a
fresh writer. The writer independently prepares the same SDK using Git and
Python only, compares its tree, and creates the bridge candidate. It never
executes upstream code or pushes to an SDK fork. Allowed candidate changes:
sdk/BASE, sdk/EXPECTED, the official tuta-repo gitlink, Cargo.lock and
release-plan.json. Patch changes and .gitmodules changes require separate
review. Full locked bridge CI gates normal fast-forward integration.

New plans record version, tag, upstream, tree, base and lock. The introducing
commit remains the immutable release source; a retry resumes that source
before proposing another SDK. Legacy plans with a generated commit remain
valid. Historical builds through rc.14 still use their fork pin and original
build scripts, so that fork must be retained for old releases. The migration
does not rewrite release-plan.json or publish another version.

SDK_BUMP_TOKEN remains necessary for bridge candidate branches and PRs under
the existing GitHub settings. New code never uses it to write the SDK fork;
its permissions can be narrowed separately. Removing the PAT requires the
repository to allow PR creation by GITHUB_TOKEN (and changes which automatic
PR events trigger CI); that settings change is separate. Integration uses
GITHUB_TOKEN. Signing credentials remain confined to the signing job.

CI, release builds, dev.sh and AUR source package preparation invoke the same
SDK preparer before Cargo. AUR binary packages are unchanged. Packaging and
CLI builds use --locked. Release source from before the migration retains
its old preparation path. Publication still requires every platform, signature
and complete manifest to pass. Published releases are never overwritten.

SDK dry runs make no remote writes. Release previews use an ephemeral signing
key and publish only workflow artifacts; production applications do not trust
those signatures. The migration's initial validation is local only: Windows
and Linux hosted builds remain to be exercised after publication is authorized.

The controller checks the `content=` digest format and binds the expected tree
and patch hashes to the candidate. Preparation in each bridge CI job and the
Rust build guard verify the actual file bytes against that digest.
