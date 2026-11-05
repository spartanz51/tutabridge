# Docker (experimental)

The image runs the CLI as UID/GID `10001`, with `pass` and GnuPG for session
storage. It does not need a desktop keyring or Secret Service. The first
initialization is interactive; normal starts only resume the saved session.

## Build and initialize

Use a source checkout with its pinned official SDK submodule:

```sh
git submodule update --init tuta-repo
python3 scripts/sdk-prepare.py
docker compose build
docker compose run --rm tutabridge init
docker compose up -d
```

`init` asks for your Tuta address, password and TOTP code if required. It saves
the session, prepares the local bridge password and TLS certificate, then
exits. It fails if the session cannot be persisted. It does not start mailbox
synchronization. A second `init` keeps the configured account, storage and
listening settings and the GPG key, and resumes a valid session when possible.

Configure your mail client with:

| Setting | Value |
|---|---|
| IMAP | `127.0.0.1:1143`, SSL/TLS |
| SMTP | `127.0.0.1:1025`, SSL/TLS |
| Username | Your Tuta email address |
| Password | Output of the command below |

```sh
docker compose exec tutabridge tutabridge password
```

Accept the self-signed certificate in your client. Compose publishes the mail
ports only on the host's loopback interface. Inside the container, IMAP and
SMTP listen on `0.0.0.0` so Docker port forwarding can reach them. The MCP
server is disabled and has no published port; its listener remains loopback.

## Storage and recovery

The named volume `tutabridge-data` holds:

```text
/data/config/tutabridge/    config, TLS files, encrypted mail cache
/data/cache/tutabridge/     SDK cache
/data/gnupg/               dedicated GPG private key
/data/pass/                encrypted Tuta session
/data/.lock                exclusive lock while init or run is active
```

The GPG key has no passphrase, allowing unattended restarts. A complete copy
of the volume includes both the ciphertext and its decryption key. Protect
volume access and backups accordingly; this does not protect against theft
of the complete volume. The Tuta account password is not saved.

Normal `run` never creates replacement GPG keys or prompts for credentials.
When Tuta rejects an expired or revoked session, the bridge removes that
session and exits. Compose's `restart: unless-stopped` policy then retries
without a saved login and keeps failing until you stop the service and sign
in again:

```sh
docker compose stop
docker compose run --rm tutabridge init
docker compose up -d
```

If the session decrypts successfully but its JSON or encoding is invalid,
`init` identifies the entry and offers a password login. Only successful
authentication replaces the entry; cancelling or failing login preserves it.
An access or decryption failure still stops initialization. Restore the
existing key/recipient file from backup before retrying. `init` refuses to
guess among multiple GPG keys or to
initialize over encrypted entries without a `.gpg-id` file. `init` and `run`
cannot use the same volume concurrently. Use a local Docker volume; network
filesystems can have different lock semantics.

`docker compose down` keeps the named volume. `docker compose down --volumes`
deletes the saved login, GPG key, bridge password, certificate and cache.
Back up the complete volume with the service stopped. For a host bind mount,
prepare the directory for UID/GID `10001` and restrict access to its owner.

To change advanced settings, stop the service and edit
`/data/config/tutabridge/config.toml`. Keep `pass_path = "Tuta/Bridge"` for
this image. For remote mail clients, exposing host ports and providing a
trusted certificate require an explicit deployment decision; the supplied
Compose file serves clients on this host.

## Build and test details

The multi-stage build compiles only the CLI with a fixed Rust version and
`Cargo.lock`. It consumes the prepared `sdk/generated` tree: the Rust build
guard verifies its complete content and patch hashes inside the image. The
Docker context allowlist excludes Git metadata, the upstream checkout, host
build outputs and unrelated local files. Run SDK preparation again after
changing the SDK base or patches. Override `TUTABRIDGE_VERSION` as a Docker
build argument when preparing a release build.

The build defaults to three Cargo jobs to limit concurrent compiler memory
use in Docker VMs. Adjust this for the memory available to your builder:

```sh
docker compose build --build-arg CARGO_BUILD_JOBS=2
```

These inputs are pinned, but Debian package repositories and base-image tags
can change; the build does not claim bit-identical output. The runtime keeps
libdbus because the normal Linux binary also includes the keyring backend.
It does not launch a D-Bus service.

```sh
python3 -m unittest discover -s scripts/tests -p 'test_docker_entrypoint.py' -v
docker build -t tutabridge:test .
TUTABRIDGE_DOCKER_IMAGE=tutabridge:test \
  python3 -m unittest discover -s scripts/tests -p 'test_docker_smoke.py' -v
```

The first suite uses isolated fake executables and tests initialization,
recovery, argument handling, locking and signals. The opt-in image suite uses
real GPG/pass and disposable volumes to check encryption across container
recreation, error handling, permissions, lock ownership and SIGTERM. It also
kills `gpg-agent` and restarts it through `pass show` from the process holding
the volume lock, checking that the new agent does not keep that lock open. It
substitutes a tiny CLI only for account-free provisioning and signal checks;
missing/corrupt-session checks use the real bridge binary. No test signs in
to Tuta or accesses an existing host keyring.

Before declaring a deployment validated, manually initialize a test Tuta
account, stop and recreate the container with the same volume, verify that
it resumes without password/TOTP, and check IMAP/SMTP access. These network
and account checks are separate from the offline CI smoke tests.
