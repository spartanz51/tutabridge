# SDK changes tracking

TutaBridge depends on a few changes to the Tuta Rust SDK (`tuta-sdk/rust/sdk`,
vendored as the `tuta-repo` submodule). To stay able to switch back to Tuta's
upstream at any time, **every SDK change is kept as its own single-commit
branch off `upstream/master`**, each independently reviewable / mergeable by
Tuta. They are combined only in the `tutabridge-integration` branch, which is
what the submodule actually checks out.

- Fork (our branches live here): `spartanz51/tutanota`
- Upstream: `tutao/tutanota`
- Integration branch (sum of the changes below): `tutabridge-integration`

Rule: never accumulate unrelated SDK changes on one branch. One concern = one
branch = one commit, rebasable on `upstream/master`.

## Branches

| Branch | Summary | Upstream PR | Fork PR | Submitted to upstream | Merged | Live-tested |
|---|---|---|---|---|---|---|
| `sdk-load-multiple` | `EntityClient`/`CryptoEntityClient.load_multiple` (batch entity loading) | [tutao#10854](https://github.com/tutao/tutanota/pull/10854) | — | yes (open) | no | yes (sync 500 mails) |
| `sdk-blob-element-reading` | `BlobFacade.load_blob_element` + `MailFacade.load_mail_details_blob` (read `MailDetailsBlob`) | [tutao#10870](https://github.com/tutao/tutanota/pull/10870) | — | yes (open) | no | yes (body decrypt over IMAP) |
| `sdk-2fa-session` | Interactive 2FA: `initiate_session`, `authenticate_with_second_factor_totp`, `is_second_factor_pending`, `cancel_create_session` | [tutao#10871](https://github.com/tutao/tutanota/pull/10871) | — | yes (open) | no | yes (full TOTP login) |
| `sdk-folder-system` | Rebuild `FolderSystem` tree (system/custom/nested), add `MailSetKind` Label/Imported/Scheduled + accessors | — | [spartanz51#4](https://github.com/spartanz51/tutanota/pull/4) | no (held) | no | yes (custom folders listed + read over IMAP) |
| `sdk-move-mails` | `MailFacade.move_mails` (move to an arbitrary folder via `MoveMailService`) | — | [spartanz51#5](https://github.com/spartanz51/tutanota/pull/5) | no (held) | no | yes (IMAP MOVE between folders) |
| `sdk-event-bus` | WebSocket `EventBusClient` (`/event?…`) — realtime entity updates with catch-up via `groupsToLastEventBatchIds`, plus observable `WsState` | — | — | no (held) | no | yes (live-tested over Phase 2/3 bridge integration, 26 unit tests) |
| `sdk-mail-set-entry-id` | `mail_set_entry_id::{construct, deconstruct}` — encode/decode `MailSetEntry._id` (4-byte truncated timestamp + 9-byte raw Mail id, base64url-no-pad) | — | — | no (held) | no | yes (consumed by the bridge realtime delta optimisation; 8 unit tests, TS test vector asserted) |
| `sdk-inline-decrypt` | `CryptoEntityClient::decrypt_inline_and_parse<T>` — decrypt an entity payload arriving inline via the event bus, with no REST round-trip; `EntityClient::parse_raw` helper | — | — | no (held) | no | yes (consumed by the bridge realtime path to skip `load_mail` on every Mail UPDATE / new mail; 3 integration tests against the live decryption fixture) |

## Notes per branch

### sdk-load-multiple
Additive utility, mirrors TS `EntityClient.loadMultiple`. Maintainer (charlag)
asked why it's submitted (not user-facing) and about LLM use; answered honestly.

### sdk-blob-element-reading
Additive. Mirrors TS blob reading + `doBlobRequestWithRetry`/`tryServers`.
Returns `MailDetails` from `load_mail_details_blob` (matches TS).

### sdk-2fa-session
Refactors `create_session` to delegate to `initiate_session`; reuses the
existing `parse_session_id`; no `clientIdentifier` change. Additive otherwise.

### sdk-folder-system
**Held — not submitted upstream.** It modifies the existing `FolderSystem`
struct, which upstream marks as WIP (`// this structure should probably change
rather soon`), so they likely want to design it themselves. Faithful port of
`FolderSystem.ts`. Submit only if the other PRs get traction and a maintainer
signals appetite — align the API with them first. Needed locally regardless for
custom/nested folder support in the bridge. Live-tested in the bridge: custom
folders are listed and read over IMAP (the `custom-folders` bridge change keys
everything by folder id).

### sdk-event-bus
**Held — not submitted upstream.** Port of
`src/common/api/worker/EventBusClient.ts`: WebSocket client for `/event?…`,
reconnect/backoff matching the TS close-code semantics, catch-up of missed
batches via the `groupsToLastEventBatchIds` query param. Entity-update batches
are decoded from the server's untyped wire format into a small typed
`EntityUpdateBatch` (the other message kinds stay as raw `serde_json::Value`
since the bridge does not need them yet — consumers can apply the SDK's type
machinery if they want a typed view). Exposes a `WsState`
(`Stopped`/`Connecting`/`Connected`/`Reconnecting`) via `state() ->
watch::Receiver<_>` so UIs can render the connection lifecycle live. New
dependency: `tokio-tungstenite` configured to reuse the existing rustls
stack; gated behind the existing `net` feature. 26 unit tests cover URL
building, wire parsing, reconnect logic and state-broadcast behaviour.
Live-tested as part of the bridge realtime sync (Phase 2/3).

### sdk-mail-set-entry-id
**Held — not submitted upstream.** New module `mail_set_entry_id` exposing
`construct(receive_date, mail_id) -> CustomId` and the inverse
`deconstruct(custom_id) -> Result<(DateTime, GeneratedId), _>`. Mirrors the
TypeScript helpers in `src/platform-kit/meta/EntityUtils.ts` — a
`MailSetEntry._id.element_id` is a 13-byte buffer (4 bytes of timestamp
shifted right by 10 bits, then 9 raw bytes of the referenced `Mail.element_id`)
encoded as base64url-no-pad. Knowing the encoding lets a realtime consumer
extract the mail id directly from a `MailSetEntry` CREATE/DELETE event
without an extra REST round-trip. 8 unit tests assert the TS test vector
verbatim plus round-trip and four error shapes. Live-tested in the bridge
realtime delta path (Phase 2 of the realtime work).

### sdk-inline-decrypt
**Held — not submitted upstream.** `CryptoEntityClient::decrypt_inline_and_parse<T>`
takes the still-encrypted JSON delivered inside a WebSocket
`EntityUpdate.instance` and walks the existing decryption pipeline
locally (`JsonSerializer::parse` → `CryptoFacade::resolve_session_key`
→ `EntityFacade::decrypt_and_map` → `InstanceMapper::parse_entity`),
producing the same typed entity as a full REST `load` would, with no
network call. Returns `Ok(None)` when the session key cannot be
resolved — a transient state (post-reply attachment key propagation in
the TS) that should fall through to a REST `load` rather than surface
as an error. `EntityClient::parse_raw` is exposed as a thin public
wrapper around the previously-private `JsonSerializer` so the new
method can feed an arbitrary deserialised JSON object into the same
parsing pass `load` uses internally. The `MockEntityClient` mock
declaration is extended for the new accessor. Three integration tests
reuse the captured `download_mail_test/mail.json` fixture as a
simulated event-bus payload — the happy-path test asserts the inline
decryption yields the same Mail (subject, recipientCount) that the
REST-backed `download_mail_test` extracts. Live-tested in the bridge
realtime path (Phase 3 of the realtime work).

## Rebasing on a newer upstream

```
cd tuta-repo
git fetch upstream
# rebase each SDK branch on the new master (resolve only if upstream touched
# the same files — so far it hasn't)
git rebase upstream/master sdk-load-multiple
git rebase upstream/master sdk-blob-element-reading
git rebase upstream/master sdk-2fa-session
git rebase upstream/master sdk-folder-system
git rebase upstream/master sdk-move-mails
git rebase upstream/master sdk-event-bus
git rebase upstream/master sdk-mail-set-entry-id
git rebase upstream/master sdk-inline-decrypt
# rebuild the integration branch from the rebased branches
git checkout -B tutabridge-integration upstream/master
git cherry-pick sdk-load-multiple sdk-blob-element-reading sdk-2fa-session sdk-folder-system sdk-move-mails sdk-event-bus sdk-mail-set-entry-id sdk-inline-decrypt
```

When an upstream PR merges, drop that branch from the cherry-pick list — the
integration branch shrinks until (ideally) it equals `upstream/master`.
