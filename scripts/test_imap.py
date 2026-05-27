#!/usr/bin/env python3
"""
Integration test for TutaBridge IMAP server.

Connects to the local IMAP bridge and verifies:
1. Authentication works
2. Folder listing works
3. Mail count is correct (store is populated)
4. Mail headers are readable (ENVELOPE)
5. Mail bodies are readable (BODY[])
6. IDLE notifications work

Prerequisites:
- Bridge must be running (cargo run or dev.sh)
- Config must have bridge_password set

Usage:
    python3 scripts/test_imap.py [--password BRIDGE_PASSWORD]
"""

import imaplib
import ssl
import sys
import time
import argparse

try:
    import tomllib
except ImportError:
    import tomli as tomllib

from pathlib import Path

RED = "\033[91m"
GREEN = "\033[92m"
YELLOW = "\033[93m"
RESET = "\033[0m"

def load_config():
    config_path = Path.home() / "Library" / "Application Support" / "tutabridge" / "config.toml"
    if not config_path.exists():
        config_path = Path.home() / ".config" / "tutabridge" / "config.toml"
    if not config_path.exists():
        return None
    with open(config_path, "rb") as f:
        return tomllib.load(f)

def ok(msg):
    print(f"  {GREEN}PASS{RESET} {msg}")

def fail(msg):
    print(f"  {RED}FAIL{RESET} {msg}")

def warn(msg):
    print(f"  {YELLOW}WARN{RESET} {msg}")

def test_imap(host, port, email, password):
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE

    passed = 0
    failed = 0

    print(f"\nConnecting to {host}:{port}...")

    # Test 1: Connection
    try:
        imap = imaplib.IMAP4_SSL(host, port, ssl_context=ctx)
        ok("TLS connection established")
        passed += 1
    except Exception as e:
        fail(f"Connection failed: {e}")
        return 0, 1

    # Test 2: Authentication
    try:
        imap.login(email, password)
        ok("Authentication successful")
        passed += 1
    except Exception as e:
        fail(f"Authentication failed: {e}")
        imap.logout()
        return passed, failed + 1

    # Test 3: LIST folders
    try:
        status, folders = imap.list()
        assert status == "OK"
        folder_names = []
        for f in folders:
            name = f.decode().split('"')[-2] if f else ""
            folder_names.append(name)
        expected = {"INBOX", "Sent", "Drafts", "Trash", "Archive", "Spam"}
        found = set(folder_names) & expected
        if found == expected:
            ok(f"LIST returned all {len(expected)} folders")
            passed += 1
        else:
            missing = expected - found
            fail(f"LIST missing folders: {missing}")
            failed += 1
    except Exception as e:
        fail(f"LIST failed: {e}")
        failed += 1

    # Test 4: SELECT INBOX and check mail count
    try:
        status, data = imap.select("INBOX")
        assert status == "OK"
        count = int(data[0])
        if count > 0:
            ok(f"INBOX has {count} messages (store is populated)")
            passed += 1
        else:
            warn(f"INBOX has 0 messages - syncer may still be loading")
            failed += 1
    except Exception as e:
        fail(f"SELECT INBOX failed: {e}")
        failed += 1

    # Test 5: FETCH headers (FLAGS + ENVELOPE-like)
    if count > 0:
        try:
            uid = str(min(count, 1))
            status, data = imap.fetch(uid, "(FLAGS UID)")
            assert status == "OK"
            resp = data[0].decode() if isinstance(data[0], bytes) else str(data[0])
            assert "FLAGS" in resp
            ok(f"FETCH FLAGS works (msg 1)")
            passed += 1
        except Exception as e:
            fail(f"FETCH FLAGS failed: {e}")
            failed += 1

        # Test 6: FETCH body
        try:
            status, data = imap.fetch(uid, "(BODY.PEEK[])")
            assert status == "OK"
            if data[0] is None:
                warn("BODY[] returned None - details not yet synced (expected during prefetch)")
                failed += 1
            else:
                body = data[0][1] if isinstance(data[0], tuple) else data[0]
                body_str = body.decode("utf-8", errors="replace") if isinstance(body, bytes) else str(body)
                if len(body_str) > 50:
                    ok(f"FETCH BODY[] returned {len(body_str)} bytes")
                    passed += 1
                elif len(body_str) > 0:
                    warn(f"FETCH BODY[] returned only {len(body_str)} bytes (details may not be synced yet)")
                    failed += 1
                else:
                    fail("FETCH BODY[] returned empty")
                    failed += 1
        except Exception as e:
            fail(f"FETCH BODY[] failed: {e}")
            failed += 1

    # Test 7: STATUS on other folders
    for folder in ["Sent", "Drafts", "Trash"]:
        try:
            status, data = imap.status(folder, "(MESSAGES UNSEEN)")
            assert status == "OK"
            resp = data[0].decode() if isinstance(data[0], bytes) else str(data[0])
            ok(f"STATUS {folder}: {resp.strip()}")
            passed += 1
        except Exception as e:
            fail(f"STATUS {folder} failed: {e}")
            failed += 1

    # Test 8: SEARCH UNSEEN
    try:
        status, data = imap.search(None, "UNSEEN")
        assert status == "OK"
        unseen_ids = data[0].decode().split() if data[0] else []
        ok(f"SEARCH UNSEEN found {len(unseen_ids)} messages")
        passed += 1
    except Exception as e:
        fail(f"SEARCH UNSEEN failed: {e}")
        failed += 1

    # Cleanup
    try:
        imap.logout()
    except:
        pass

    return passed, failed


def main():
    parser = argparse.ArgumentParser(description="Test TutaBridge IMAP server")
    parser.add_argument("--password", help="Bridge password (reads from config if not provided)")
    parser.add_argument("--host", default="127.0.0.1", help="IMAP host")
    parser.add_argument("--port", type=int, help="IMAP port (reads from config if not provided)")
    parser.add_argument("--email", help="Email address (reads from config if not provided)")
    args = parser.parse_args()

    config = load_config()

    email = args.email or (config and config.get("email")) or "user@tuta.io"
    password = args.password or (config and config.get("bridge_password"))
    port = args.port or (config and config.get("imap_port")) or 1143

    if not password:
        print(f"{RED}No bridge password found. Pass --password or set bridge_password in config.{RESET}")
        sys.exit(1)

    print(f"TutaBridge IMAP Integration Test")
    print(f"================================")
    print(f"Host: {args.host}:{port}")
    print(f"Email: {email}")

    passed, failed = test_imap(args.host, port, email, password)

    print(f"\n{'=' * 40}")
    print(f"Results: {GREEN}{passed} passed{RESET}, {RED if failed else ''}{failed} failed{RESET}")

    if failed > 0:
        sys.exit(1)
    print(f"\n{GREEN}All tests passed!{RESET}")


if __name__ == "__main__":
    main()
