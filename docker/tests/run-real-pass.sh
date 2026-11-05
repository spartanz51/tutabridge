#!/bin/bash
# Disposable synthetic keys only. Run in the image with --network none.
set -euo pipefail
umask 077
export TUTABRIDGE_REAL_PASS_ROOT
TUTABRIDGE_REAL_PASS_ROOT=$(mktemp -d /tmp/tutabridge-real-pass-XXXXXXXX)
export PASSWORD_STORE_DIR=$TUTABRIDGE_REAL_PASS_ROOT/store
export GNUPGHOME=$TUTABRIDGE_REAL_PASS_ROOT/gpg
export PASSWORD_STORE_GPG_OPTS="--batch --pinentry-mode error --auto-key-locate clear"
export GIT_AUTHOR_NAME=Fixture GIT_COMMITTER_NAME=Fixture
export GIT_AUTHOR_EMAIL=fixture@example.invalid GIT_COMMITTER_EMAIL=fixture@example.invalid
cleanup() {
    gpgconf --kill gpg-agent || true
    GNUPGHOME="$TUTABRIDGE_REAL_PASS_ROOT/public-key-source" gpgconf --kill gpg-agent || true
    rm -rf -- "$TUTABRIDGE_REAL_PASS_ROOT"
}
trap cleanup EXIT
mkdir -p "$GNUPGHOME" "$PASSWORD_STORE_DIR" "$TUTABRIDGE_REAL_PASS_ROOT/public-key-source"
gpg --batch --generate-key /usr/local/lib/tutabridge/gpg-key-parameters >/dev/null 2>&1
recipient=$(gpg --batch --with-colons --list-secret-keys 2>/dev/null | awk -F: '$1=="fpr"{ print $10; exit }')
pass init "$recipient" >/dev/null 2>&1
pass git init >/dev/null 2>&1
# A public-only recipient exercises read-back failure after successful encryption.
GNUPGHOME="$TUTABRIDGE_REAL_PASS_ROOT/public-key-source" gpg --batch --generate-key /usr/local/lib/tutabridge/gpg-key-parameters >/dev/null 2>&1
GNUPGHOME="$TUTABRIDGE_REAL_PASS_ROOT/public-key-source" gpg --batch --with-colons --list-secret-keys 2>/dev/null | awk -F: '$1=="fpr"{ print $10; exit }' > "$TUTABRIDGE_REAL_PASS_ROOT/public-only-recipient"
GNUPGHOME="$TUTABRIDGE_REAL_PASS_ROOT/public-key-source" gpg --batch --export > "$TUTABRIDGE_REAL_PASS_ROOT/public-key"
gpg --batch --import "$TUTABRIDGE_REAL_PASS_ROOT/public-key" >/dev/null 2>&1
# Trust this synthetic public key so encryption succeeds; decryption must fail.
printf '%s:6:\n' "$(cat "$TUTABRIDGE_REAL_PASS_ROOT/public-only-recipient")" | gpg --batch --import-ownertrust >/dev/null 2>&1
/tmp/test-core --exact session_store::pass::preservation_tests::real_pass_atomic_roundtrip_and_git_history --ignored --nocapture
