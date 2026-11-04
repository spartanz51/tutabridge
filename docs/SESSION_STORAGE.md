# Session storage

TutaBridge stores the Tuta session, not the Tuta account password. Desktop
installations use the OS keychain by default. Setting the existing `pass_path`
option selects `pass` instead:

```toml
email = "you@tuta.com"
pass_path = "Tuta/Bridge"
```

The entry is `Tuta/Bridge/you@tuta.com`. Leading and trailing slashes in
`pass_path` are ignored; an empty result keeps the default keychain. The keychain
service name (`tutabridge`), account name, and serialized session format are
unchanged. No migration or new configuration section is required.

## Ownership and boundaries

`session_store::SessionStore` owns one account's storage context and memory
cache. Clones share that state; different accounts, API endpoints, or backend
selections use different instances. The CLI reuses one instance throughout an
authentication attempt. `BridgeHandle` owns the GUI instance and replaces it
when its configuration context changes.

The module contains a pure codec and backend-selection logic, small keychain
and `pass` adapters, and the cache state machine. Storage operations run on
Tokio's blocking pool, with serialization inside the worker. Waiting for
pinentry therefore does not occupy an async worker; it is still an external
interactive operation and is not made cancellable by this module.

## Error policy

| Result | Interactive login | Automatic resume |
| --- | --- | --- |
| Entry absent | Ask for password, save the new session | No session to resume |
| Valid entry | Resume it | Resume it |
| Keychain access/platform failure | Show a warning and allow a password login in memory | Return the storage error |
| Readable bytes with invalid session JSON or encoding | Show the backend and entry; replace only after an explicit password login succeeds | Return an error; preserve the entry |
| Ambiguous keychain entry or `pass` read/decryption failure | Return an error; preserve the entry | Return an error |
| Save failure after successful authentication | Keep the session in memory and show a warning | Caller must decide whether persistence is required |

An inaccessible entry is not an absent entry. After a failed read, writes and
deletions are inhibited until a successful read establishes what is stored.
Choosing a memory-only login keeps persistence disabled for that session: it
must not overwrite an entry that the process could not read. Restarting the app
retries the configured backend; stopping and starting only the bridge in the GUI
keeps the in-memory session and does not retry persistence. No backend is selected
as an automatic fallback.

`load()` is strict; `inspect()` reports whether a session is available and a
user-facing warning, without choosing a volatile login or authorizing replacement.
Only an explicit password login uses `load_interactive()`. It reads a corrupt
entry again before authorizing replacement, so an intervening repair or access
failure is respected. It never deletes the old entry before authentication; an
abandoned or failed login leaves it intact and automatic resume remains strict.
A cached session remains usable after a
failed write. A rejected token is removed from the cache even if its persistent
deletion fails, so the same process cannot resume it again.

The CLI's `init` and `run --non-interactive` modes require persistence.
`load_persisted` rejects a previously cached memory-only session, and
`PersistencePolicy::Required` propagates a failed save instead of treating
authentication alone as successful initialization. The service never supplies
a password or a TOTP callback. A missing or expired session requires a separate
interactive `init`. An explicit password supplied to `init` also permits
replacement of a readable but invalid entry after authentication; it never
enables the unavailable-keychain fallback. A failed replacement save keeps
initialization unsuccessful.

The local IMAP/SMTP password, TLS private key, and encrypted mail cache have
separate lifecycles and are outside this module.

## Validation

Unit tests exercise the codec, selection and context rules, cache transitions,
error classification, and concurrency with isolated in-memory backends. The
`pass_session` integration tests run a fake executable in a child process with
isolated environment and storage directories. They verify the actual command
arguments, stdin, failures, retries, and redaction without accessing a developer's
keychain, password store, or Tuta account.

```sh
cargo test --locked -p tutabridge-core --lib
cargo test --locked -p tutabridge --test pass_session
```
