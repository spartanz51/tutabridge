# TutaBridge

A local IMAP/SMTP bridge for [Tuta](https://tuta.com) encrypted email. It runs a
local IMAP+SMTP server that ordinary mail clients (Thunderbird, Apple Mail, mutt,
…) connect to, while talking to Tuta's API and handling the end-to-end
encryption transparently.

Available as a **CLI** and a **desktop GUI** (Tauri).

## Features

- **IMAP + SMTP** servers on localhost (TLS), so any standard mail client works.
- **Realtime sync** over Tuta's WebSocket event bus — new mail, reads, moves and
  deletes show up without polling. A heartbeat + idle-timeout detect dead
  sockets and reconnect automatically.
- **Attachments** both ways — incoming mail is served as `multipart/mixed`;
  attachments composed in your client are uploaded to Tuta on send.
- **Drafts**, **custom / nested folders**, **move**, **trash**, and read/unread
  flags.
- **2FA (TOTP)** login.
- **Encrypted local cache** — metadata in SQLCipher, bodies as individually
  encrypted `.eml.enc` files. Subsequent launches load from cache and only fetch
  the delta, so the client is usable immediately.
- **Complete mailbox backup** to portable `.eml` files (see below).

## Architecture

Syncer-driven, store-backed:

```
Tuta API  ←──  Syncer (background)  ──→  MailStore (in-memory)  ←──  IMAP server  ──→  mail client
                                                                ←──  GUI (stats)
```

- The **syncer** pulls from the Tuta API and populates an in-memory `MailStore`,
  backed by the on-disk encrypted cache.
- The **IMAP server** only ever *reads* from the store — it never makes API
  calls for reads.
- The only IMAP→network calls are mutations: mark read/unread (`STORE \Seen`)
  and trash (`EXPUNGE`). Sending goes through SMTP → Tuta's `DraftService` +
  `SendDraftService`.

The storage encryption key is derived from your Tuta session, so there's no extra
password to manage; the cache is encrypted at rest.

## Build

```bash
cargo build                      # CLI + core
cargo build -p tutabridge-core   # core library only
```

GUI (Tauri + React) in dev mode:

```bash
./dev.sh                         # cargo tauri dev — opens the desktop app
```

## Running the bridge

CLI:

```bash
cargo run            # or: ./target/debug/tutabridge
```

On first run it prompts for your Tuta email; the session is then stored in the OS
keychain so later launches resume without a password (TOTP is prompted when
required). The bridge prints the local connection details:

```
IMAP server: 127.0.0.1:1143 (SSL/TLS)
SMTP server: 127.0.0.1:1025 (SSL/TLS)
Username:    <your tuta email>
Password:    <bridge password — shown in the logs / GUI>
```

Point your mail client at those, accept the self-signed certificate, and use the
**bridge password** (not your Tuta password) for IMAP/SMTP auth.

Config and cache live under
`~/Library/Application Support/tutabridge/` (macOS):

```
config.toml          account + ports + bridge_password + sync_limit
store.db             SQLCipher metadata index
mails/<id>.eml.enc   per-mail encrypted bodies
```

## Backup

Export **every** email to a folder of plain `.eml` files — one file per message,
in a directory tree mirroring your IMAP folders. This is a *complete* backup: it
enumerates all mail from the server, not just the messages currently synced, so
nothing is silently left out.

```
<output>/
├── INBOX/
│   ├── 20260528-144935_OtjDuDU--3-9.eml
│   └── …
├── Sent/
├── Trash/
└── Café/Projets/…
```

CLI:

```bash
tutabridge backup ~/TutaBackup
```

GUI: the **Backup** tab — pick a folder, watch the per-folder progress, done.
(The bridge must be running; the backup reuses its signed-in session.)

Notes:

- **Format**: `.eml` (RFC 2822). Opens natively in Thunderbird / Apple Mail /
  Outlook, survives Windows filesystems, and one corrupt file never takes down
  the whole archive. Filenames are date-prefixed so a listing sorts
  chronologically.
- **Resumable / incremental**: re-running into the same folder skips messages
  already on disk, so an interrupted backup resumes and a periodic re-backup only
  fetches new mail.
- **Speed**: messages already in the local cache export instantly; the rest are
  fetched from the server with a small politeness delay, so a first full backup of
  a large mailbox can take several minutes. Subsequent runs are fast.
- **Scope**: every folder, including Trash and Spam. Labels aren't separate
  folders, so a labelled mail is backed up once, in its real folder.

## Testing

```bash
cargo test --workspace        # unit + integration tests

python3 scripts/test_imap.py  # integration test against a running bridge
```

The IMAP integration test connects to the local server and verifies TLS, auth,
folder list, mail count, body fetch and search. It reads the bridge password from
`config.toml` automatically.

## SDK

TutaBridge depends on a few additions to Tuta's Rust SDK, vendored as the
`tuta-repo` submodule. Each change is kept as its own single-commit branch off
upstream for easy review / upstreaming — see [`SDK_PRS.md`](SDK_PRS.md) for the
status of each.

## License

[GPL-3.0-or-later](LICENSE). TutaBridge links Tuta's Rust SDK (part of the
GPLv3-licensed [tutanota](https://github.com/tutao/tutanota) project), so it is
distributed under the same license.
