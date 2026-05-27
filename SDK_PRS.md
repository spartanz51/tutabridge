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
# rebuild the integration branch from the rebased branches
git checkout -B tutabridge-integration upstream/master
git cherry-pick sdk-load-multiple sdk-blob-element-reading sdk-2fa-session sdk-folder-system
```

When an upstream PR merges, drop that branch from the cherry-pick list — the
integration branch shrinks until (ideally) it equals `upstream/master`.
