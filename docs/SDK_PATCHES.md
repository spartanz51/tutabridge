# Vendored SDK patch list

What `tuta-repo` carries on top of tutao's release, and why. Keep this
current when the pin moves — it is the only place the series is described
as a whole.

- **Base:** tutao `v359.260904.0` (`aea5846b93a1412451e885bf99002401c3b087e8`)
- **Branch:** `tutabridge-integration` on the SDK fork (`spartanz51/tutanota`)
- **Pin:** `7a3d4f9fe333d78b3002a3a3cab223a2ea9167ff`

The base is a real tutao release tag, not a relabelled older tree. `git
diff <base>..<pin> -- Cargo.toml` is empty: the SDK reports
`359.260904.0` because the release says so. Check that before trusting
the version — see "History" below for why.

The pin is a **merge commit** with two parents, the previous integration
branch and tutao's release. It is not a rebase rooted at the release, so
it descends from what it targets and merges rather than conflicting.

The shas below are Anthony's original commits, reachable on
`tutabridge-integration`. They are **not** the rebased copies inside this
pin: the series is squashed, so per-patch shas within the pin do not
exist. Cite the originals — they stay reachable and they are what you
want when tracing a patch to its author.

## What the series adds

| Original commit | Capability | Why it is in the SDK |
|---|---|---|
| `ab3747344` | `load_multiple` batch entity loading | Transport and parsing are partly private; avoids one request per mail. |
| `16c95161b` | Blob element reading (`MailDetailsBlob` bodies) | Reuses the SDK's blob pipeline rather than building a parallel one. |
| `8219ebe3a` | Interactive session creation with TOTP | Reuses services, key derivation and session bootstrap. |
| `8fafe8c8e` | Folder tree in `FolderSystem` | Folder hierarchy the bridge exposes over IMAP. |
| `7ff31bc5c` | `MailFacade::move_mails` | Arbitrary target folders, for IMAP MOVE. |
| `335b75d78` | WebSocket event-bus client | Live updates; the bridge has no polling path. |
| `e27780725` | `MailSetEntry` element id codec | Id encoding the mail-set APIs require. |
| `f97f5b1e0` | Inline event payload decryption | Avoids a REST round-trip per event. |
| `42170313a` | `MailDetailsDraft` loading | Draft bodies, which are not blob-backed. |
| `00266f32d` | Attachment blob download and decrypt | Serving attachments over IMAP. |
| `6d31dec6f` | `BlobGetIn` aggregate `_id` | Required by the instance mapper. |
| `1036d6f2e` | WebSocket heartbeat and idle timeout | Detects zombie sockets. |

Two further changes are not bridge capabilities and have no original
commit on `tutabridge-integration` — they were written for the 359 move:

**Integration.** `MailFacade::new` keeps upstream's three-argument
signature. The blob patch originally widened it to six, and 359 adds
archive tests calling the official form — **the edits do not overlap
textually, so a cherry-pick succeeds and the tree does not compile.** A
patch that applies is not a patch that fits. Blob, draft and attachment
support is supplied by `with_mail_details_support` instead, and each
method resolves only what it uses:

| method | needs |
| --- | --- |
| `load_mail_details_draft` | key loader |
| `load_mail_details_blob` | key loader, blob transport, serialiser |
| `load_file_attachment_data` | key loader, blob transport |

A facade built without them returns `ApiCallError` rather than panicking.
Upstream's tests compile untouched, which is the point: it keeps the next
rebase cheap.

**Empty optional encrypted values resolve to null.** `Body` has `text`
(1275) and `compressedText` (1276), both `ZeroOrOne` and encrypted, and
only one is ever populated — so the other comes back empty. That value
matched neither the empty-string arm (guarded to `Cardinality::One`) nor
the `Null` arm, fell through to `decrypt_data`, and failed the IV-length
check in `aes.rs`. **Every draft read failed with `InvalidDataSizeError`**
— on every send, not on odd data. Non-fatal, because the retry wrapper
swallowed it after the send had already succeeded, which is why it went
unreported for months.

TS does exactly this in `CryptoMapper.decryptValue`, for both
cardinalities. The fix predates the rebase: the same gap is in the 348
tree. It is also proposed standalone as spartanz51/tutanota#8, and sits
rebased onto tutao's current master on `sdk-empty-optional-upstream`,
where the function is byte-identical — so the bug is live upstream too.

## History: the relabelled SDK

Before this series the pin was tutao's `348.260528.0` tree with one line
of `Cargo.toml` changed to report `359.260904.0`, to clear Tuta's
client-version floor (issue #37). `CLIENT_VERSION` is
`env!("CARGO_PKG_VERSION")` and the `cv` header is its only consumer, so
that opened the gate without moving any protocol code.

The middle field of the version is a date. The code was 2026-05-28
announcing itself as 2026-09-04.

Moving to the genuine release surfaced what the relabelling had hidden:

- `crypto-primitives` renamed `Iv` to `InitializationVector` — six call
  sites in `store.rs` and `tuta.rs`.
- `rustfmt.toml` specifies edition 2024, and three `EventBusClient`
  accessors needed `#[must_use]`. Neither had ever been enforced against
  these patches.

All small. That is the point: they sat between the bridge and the version
it claimed to be for months, invisible because the two were never
compiled together.

**Expect the floor to rise again**, every few months. Tuta shipped
`360.260917.0` and `360.260921.0` within weeks of 359. The fix is to move
to a newer real release, not to edit the version string.

## Verifying a pin

```sh
# the version must come from the release, not from an edit
git -C tuta-repo diff <base-tag>..HEAD -- Cargo.toml   # must be empty

# use the toolchain CI pins, not whatever is on PATH -- the project
# specifies 1.84.0 and edition 2024, and newer toolchains disagree
rustup run 1.84.0 cargo fmt --all -- --check
rustup run 1.84.0 cargo clippy --all --no-deps -- -Dwarnings
cargo test --workspace
```

Then run it: log in, send one plaintext and one HTML message, and read
the log. A clean run has no `ERROR` or `WARN` at all. Compiling proves
nothing about whether the reported version is accepted — only an
authenticated login does.
