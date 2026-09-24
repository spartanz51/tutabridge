# TutaBridge

## Architecture

TutaBridge is an IMAP/SMTP bridge for Tuta encrypted email. It exposes a local IMAP+SMTP server that mail clients (Thunderbird, etc.) connect to.

### Core principle: store-backed, kept current by the event bus

```
Tuta API ──► Syncer (startup, body prefetch) ──► MailStore (in-memory) ◄── IMAP server ◄── mail client
Tuta WS  ──► Event bus ──► Event handler ────────┘  + encrypted disk cache   ◄── Tauri UI (stats)
```

- **Startup** (`sync.rs`): the syncer loads the folder list and the encrypted
  disk cache into the `MailStore`, runs a full metadata sync only on first
  launch or migration, then signals `startup_done`.
- **Realtime** (`event_handler.rs`): the event bus (WebSocket, `crates/tuta`)
  delivers entity updates, catch-up included. The handler holds them until
  the syncer's startup is done (#44), then applies them to memory and disk.
  There is no periodic re-listing.
- **Bodies**: the syncer's prefetch loop downloads mail bodies up to
  `sync_limit`; older bodies are fetched on demand.
- The **IMAP server** (`imap/`) only reads from the `MailStore`. The only
  IMAP→network calls are mutations: `STORE \Seen`, `MOVE`, `EXPUNGE` (trash).
  Sending goes through SMTP to Tuta's draft and send services.

## Testing

### Unit tests

```bash
cargo test --workspace        # bridge, CLI, GUI and crates/tuta
```

### Integration test (IMAP)

Requires a running bridge instance (either `cargo run` or `./dev.sh` for GUI).

```bash
python3 scripts/test_imap.py
```

This connects to the local IMAP server and verifies: TLS, auth, folder list, mail count, body fetch, search. It reads the bridge password from `~/Library/Application Support/tutabridge/config.toml` automatically.

### Manual Thunderbird test

1. Start bridge: `./dev.sh` (GUI) or `cargo run` (CLI)
2. Wait for "Event bus initial sync done" in logs
3. In Thunderbird: IMAP server `127.0.0.1:1143` SSL/TLS, SMTP `127.0.0.1:1025` SSL/TLS
4. Username: your tuta email, Password: bridge_password from config
5. Accept self-signed cert

## Build

```bash
cargo build                   # CLI + GUI
cargo build -p tutabridge-core  # Core library only
```

For a live run, use a binary from `cargo build`, not one produced by
`cargo test`: the test build enables the SDK's `logging` feature, which
panics at startup next to the bridge's own logger.

## Vendored SDK (tuta-repo submodule)

Generated, never edited: the official Tuta release in `sdk/BASE` plus
`sdk/patches/`, built by `scripts/sdk-generate.sh` (`--check` verifies the
pin, CI runs it). Bridge-specific SDK extensions live in `crates/tuta`.
See `docs/SDK_PATCHES.md`.
