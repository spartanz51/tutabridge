#!/usr/bin/env bash
# Container-only setup. The bridge owns authentication and session persistence.
set -euo pipefail
umask 077

assets=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
data=${TUTABRIDGE_DATA_DIR:-/data}
export XDG_CONFIG_HOME=${XDG_CONFIG_HOME:-$data/config}
export XDG_CACHE_HOME=${XDG_CACHE_HOME:-$data/cache}
export GNUPGHOME=${GNUPGHOME:-$data/gnupg}
export PASSWORD_STORE_DIR=${PASSWORD_STORE_DIR:-$data/pass}
# The container creates an unprotected key. Never wait for a pinentry UI.
export PASSWORD_STORE_GPG_OPTS='--batch --pinentry-mode error'
config="$XDG_CONFIG_HOME/tutabridge/config.toml"

fail() { printf 'tutabridge: %s\n' "$*" >&2; exit 1; }

# Only primary fingerprints; encryption subkeys must not count as extra keys.
secret_keys() {
    gpg --batch --with-colons --list-secret-keys 9>&- |
        awk -F: '$1 == "sec" { primary = 1; next } $1 == "fpr" && primary { print $10; primary = 0 }'
}

verify_store() {
    [[ -f "$PASSWORD_STORE_DIR/.gpg-id" && -d "$GNUPGHOME" ]] ||
        fail 'Password store is not initialized; run docker compose run --rm tutabridge init.'
    local recipient keys
    recipient=$(cat -- "$PASSWORD_STORE_DIR/.gpg-id")
    [[ "$recipient" =~ ^[A-F0-9]{40}$ ]] || fail 'Unexpected pass recipient; keep the existing data and repair .gpg-id.'
    keys=$(secret_keys)
    # Compare complete lines, never a substring of another key identifier.
    [[ $'\n'"$keys"$'\n' == *$'\n'"$recipient"$'\n'* ]] ||
        fail 'The private GPG key for this password store is missing; restore it before continuing.'
}

prepare_store() {
    mkdir -p -- "$GNUPGHOME" "$PASSWORD_STORE_DIR" "$XDG_CONFIG_HOME/tutabridge" "$XDG_CACHE_HOME"
    chmod 700 "$GNUPGHOME" "$PASSWORD_STORE_DIR"
    if [[ ! -e "$PASSWORD_STORE_DIR/.gpg-id" ]]; then
        # An incomplete key generation can be resumed. Existing encrypted
        # entries without their recipient file need recovery, not a new key.
        [[ -z $(find "$PASSWORD_STORE_DIR" -mindepth 1 -print -quit) ]] ||
            fail 'Password store contains data but .gpg-id is missing; restore it before continuing.'
        local keys
        keys=$(secret_keys)
        if [[ -z "$keys" ]]; then
            gpg --batch --generate-key "$assets/gpg-key-parameters" 9>&-
            keys=$(secret_keys)
        fi
        [[ "$keys" =~ ^[A-F0-9]{40}$ ]] ||
            fail 'Expected exactly one private GPG key; select the intended key with pass init before retrying.'
        pass init "$keys" 9>&-
    fi
    verify_store
    if [[ ! -e "$config" ]]; then
        cp -- "$assets/config.toml" "$config"
    fi
}

command=${1:-run}
case "$command" in
    --version|-V|version|--help|-h|help|password)
        exec tutabridge "$@"
        ;;
    init|run)
        [[ $# -le 1 ]] || fail 'Container usage: init | run | password | --version'
        ;;
    *) fail 'Container usage: init | run | password | --version' ;;
esac

# The descriptor survives exec into the bridge. Close it explicitly for GPG
# setup commands so a daemon cannot outlive the owner while holding the lock.
mkdir -p -- "$data"
exec 9>"$data/.lock"
flock --exclusive --nonblock 9 || fail 'This data volume is already in use; stop the service before running init.'
command -v gpg >/dev/null || fail 'gpg is unavailable.'
command -v pass >/dev/null || fail 'pass is unavailable.'

if [[ "$command" == init ]]; then
    prepare_store
else
    [[ -f "$config" ]] || fail 'No configuration; run docker compose run --rm tutabridge init first.'
    verify_store
fi
# Start the agent with the lock descriptor closed before any bridge pass call.
gpgconf --launch gpg-agent 9>&-
if [[ "$command" == init ]]; then
    exec tutabridge init
fi
exec tutabridge run --non-interactive
