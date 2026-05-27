# TutaBridge

## Architecture

TutaBridge is an IMAP/SMTP bridge for Tuta encrypted email. It exposes a local IMAP+SMTP server that mail clients (Thunderbird, etc.) connect to.

### Core principle: syncer-driven, store-backed

```
Tuta API  ←──  Syncer (background)  ──→  MailStore (in-memory)  ←──  IMAP server  ──→  Thunderbird
                                                                 ←──  Tauri UI (stats)
```

- The **syncer** (`sync.rs`) runs independently in a background tokio task. It pulls emails from the Tuta API and populates the `MailStore`.
- The **IMAP server** (`imap/`) ONLY reads from the `MailStore`. It NEVER makes API calls for reads.
- The only IMAP→network calls are **mutations**: marking read/unread (`STORE \Seen`) and trashing (`EXPUNGE`).

### Syncer two-phase cycle

1. **Phase 1 (fast, ~3s)**: Sync mail lists for ALL 6 folders. Store gets populated with mail metadata immediately.
2. **Phase 2 (slow, ~2min)**: Prefetch mail details (body) one by one with rate limiting (150ms/mail). Bodies become available progressively.
3. Wait 60s, repeat.

## Testing

### Unit tests

```bash
cargo test --workspace        # 113 bridge tests + SDK tests
```

### Integration test (IMAP)

Requires a running bridge instance (either `cargo run` or `./dev.sh` for GUI).

```bash
python3 scripts/test_imap.py
```

This connects to the local IMAP server and verifies: TLS, auth, folder list, mail count, body fetch, search. It reads the bridge password from `~/Library/Application Support/tutabridge/config.toml` automatically.

### Manual Thunderbird test

1. Start bridge: `./dev.sh` (GUI) or `cargo run` (CLI)
2. Wait for "Pre-fetching N mail details for Inbox" in logs
3. In Thunderbird: IMAP server `127.0.0.1:1143` SSL/TLS, SMTP `127.0.0.1:1025` SSL/TLS
4. Username: your tuta email, Password: bridge_password from config
5. Accept self-signed cert

## Build

```bash
cargo build                   # CLI + GUI
cargo build -p tutabridge-core  # Core library only
```

## SDK branches (tuta-repo submodule)

- `feat/rust-sdk-blob-read` — blob element reading (MailDetailsBlob)
- `feat/rust-sdk-load-multiple` — batch entity loading (load_multiple)
- Locally, `feat/rust-sdk-blob-read` has both merged for development
