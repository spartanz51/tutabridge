# Docker

TutaBridge runs as a headless service in a container, for a home server or a
NAS. The image contains the CLI only, with `pass` and GnuPG to keep the Tuta
session encrypted on its data volume.

| Registry | Image |
|---|---|
| GitHub Container Registry | `ghcr.io/spartanz51/tutabridge` |
| Docker Hub | `docker.io/spartanz51/tutabridge` |

Both registries hold the same images for `linux/amd64` and `linux/arm64`.
Each release is tagged with its version (for example `0.1.0-rc.17`), and
`latest` follows the newest release. Pin a version tag if you want to choose
when to upgrade.

## Quick start

```sh
docker run -d --name tutabridge --restart unless-stopped \
  -p 127.0.0.1:1143:1143 -p 127.0.0.1:1025:1025 \
  -v tutabridge:/data ghcr.io/spartanz51/tutabridge
docker exec -it tutabridge init
```

The container starts and waits for an account; its log shows the command to
run. `init` asks for your Tuta address, password and TOTP code if your account
uses one. It creates a dedicated GPG key on the volume, saves the session, the
local bridge password and a TLS certificate, then exits. The waiting service
picks up the session within about ten seconds and starts IMAP and SMTP. Later
starts resume the saved session and never ask for credentials.

On a NAS without a shell (Synology Container Manager, QNAP Container Station,
Unraid, Portainer), search for `spartanz51/tutabridge`, map the two ports and
a volume or folder to `/data`, start the container, then open its terminal or
console and run `init`.

With Compose, the repository's [`compose.yaml`](../compose.yaml) adds a few
hardening options (all capabilities dropped, no privilege escalation):

```sh
docker compose up -d
docker compose exec tutabridge init
```

The commands below use Compose; with `docker run`, replace
`docker compose exec tutabridge` with `docker exec tutabridge`.

## Mail client settings

| Setting | Value |
|---|---|
| IMAP | `127.0.0.1:1143`, SSL/TLS |
| SMTP | `127.0.0.1:1025`, SSL/TLS |
| Username | Your Tuta email address |
| Password | Output of `password` in the container |
| Authentication | Normal password, for both servers |

The certificate is self-signed: accept it once in your client. Thunderbird
often asks only for the IMAP port; if sending then fails, add the exception
for SMTP by hand (Settings, Privacy & Security, Manage Certificates, Servers,
Add Exception, `127.0.0.1:1025`). If you used the desktop app before, delete
its saved passwords for `127.0.0.1`: the container has its own bridge
password. The supplied ports are published on the host's loopback interface
only. Inside the container the servers listen on all interfaces so that Docker
can forward to them; the MCP server stays disabled.

## Status, health and logs

```sh
docker compose ps           # shows healthy once IMAP and SMTP answer
docker compose logs -f
docker compose exec tutabridge tutabridge health
```

The image declares a health check. It opens a TLS connection to the IMAP and
SMTP ports with the bridge's own certificate pinned and checks their
greetings, without logging in. While the service waits for an account or a
repair, it reports itself unhealthy and logs why:

| Log message | What to do |
|---|---|
| Waiting for configuration / for a saved session | Run `init`. |
| Saved session rejected | Tuta no longer accepts the session, for example after a password change. Run `init` again; the old entry is kept until a new login succeeds. |
| Tuta rejects this version | Tuta requires a newer client. Update the image. |
| Tuta temporarily unavailable | Nothing to do; the service retries with a growing delay, up to five minutes. |
| Session storage unavailable | Check the volume permissions, the GPG key and the `pass` store (see below). |
| Data volume occupied | Another `init` or another container is using the same volume. |

Stopping the container is immediate while it waits. While the bridge runs,
it gets five seconds to shut down cleanly before being killed.

## Updating

```sh
docker compose pull
docker compose up -d
```

Tuta only accepts clients above a minimum version that moves from time to
time, so keep the image current. With a pinned tag, change the tag first.

## Data, backups and security

Everything lives on the `/data` volume:

```text
/data/config/tutabridge/    config, TLS certificate, encrypted mail cache
/data/cache/tutabridge/     SDK cache
/data/gnupg/                dedicated GPG private key
/data/pass/                 encrypted Tuta session
/data/.lock                 held while init or the service uses the volume
```

The GPG key has no passphrase, so the service can restart unattended. A copy
of the complete volume therefore contains both the encrypted session and the
key to decrypt it: protect access to the volume and to its backups as you
would protect the account itself. The Tuta account password is never saved.

Back up the whole volume with the service stopped. `docker compose down`
keeps the volume; `docker compose down --volumes` deletes the saved login,
GPG key, bridge password, certificate and cache.

Use a local Docker volume or a local directory; network filesystems can have
different lock semantics. For a bind mount, the directory must be writable by
the container's user. The image runs as UID/GID `10001` by default; on a NAS
you can instead run it as your own user with `user: "1000:1000"` in Compose
(or `--user` with `docker run`), as long as that user owns the directory.

## Configuration

To change settings, stop the service and edit
`/data/config/tutabridge/config.toml`. Keep `pass_path = "Tuta/Bridge"` with
this image. Serving mail clients on other machines requires publishing the
ports beyond loopback and, ideally, a certificate those clients trust; that
is a deliberate deployment choice, not a default.

## Recovery

`init` can be run again at any time. It keeps the configured account, storage
and listening settings and the GPG key, and resumes a valid session when it
can. If the session decrypts but its content is invalid, `init` says so and
offers a password login; only a successful login replaces the entry.

`init` refuses to guess among several GPG keys or to initialize over encrypted
entries without their `.gpg-id` file. If the private key or `.gpg-id` is
missing, restore them from a backup of the volume before retrying.

## Building from source

Contributors can build and run a local image from a checkout with its pinned
official SDK submodule:

```sh
git submodule update --init tuta-repo
python3 scripts/sdk-prepare.py
docker build -t tutabridge:local .
TUTABRIDGE_IMAGE=tutabridge:local docker compose up -d
```

The multi-stage build compiles only the CLI with a fixed Rust version and
`Cargo.lock`, from the prepared `sdk/generated` tree, whose content and patch
hashes the Rust build guard verifies again inside the image. It defaults to
three Cargo jobs to limit compiler memory in Docker VMs; pass
`--build-arg CARGO_BUILD_JOBS=2` if your builder has less memory. Pinned
inputs do not make the output bit-identical: Debian packages and base-image
tags can change.

The container test suites, which need no Tuta account and no network, are
described in [docker/tests/README.md](../docker/tests/README.md). Before
calling a deployment validated, also sign in with a test account, recreate the
container on the same volume, check that it resumes without password or TOTP,
and check IMAP and SMTP from a mail client.
