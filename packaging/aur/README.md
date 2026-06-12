# AUR packaging

Two packages for the **headless TutaBridge daemon** (CLI only, no GUI, so no
Node / Tauri / webkit dependencies):

| Directory | AUR package | What it does |
|-----------|-------------|--------------|
| `tutabridge-git/` | `tutabridge-git` | Builds the `tutabridge` binary from the latest commit (needs the Rust toolchain). |
| `tutabridge-bin/` | `tutabridge-bin` | Downloads the prebuilt x86_64 binary from the GitHub release. No build. |

Each directory holds a `PKGBUILD` and a `.SRCINFO`. They install
`/usr/bin/tutabridge` plus a user systemd unit.

## Local build / test (on Arch)

```bash
cd packaging/aur/tutabridge-bin     # or tutabridge-git
makepkg -si        # build + install
namcap PKGBUILD    # lint
```

## First run

The daemon resumes a saved keyring session on start, so sign in **once**
interactively before enabling the service:

```bash
tutabridge                                   # prompts for email, then Tuta password + TOTP
systemctl --user enable --now tutabridge     # run it in the background from now on
```

Connect your mail client to `127.0.0.1:1143` (IMAP) and `127.0.0.1:1025`
(SMTP), using the bridge password printed in the logs
(`journalctl --user -u tutabridge`).

## Publishing to the AUR

Each AUR package is its own git repo. From an Arch host (so `makepkg` is
available to verify the `.SRCINFO`):

```bash
git clone ssh://aur@aur.archlinux.org/tutabridge-bin.git
cp packaging/aur/tutabridge-bin/PKGBUILD tutabridge-bin/
cd tutabridge-bin
makepkg --printsrcinfo > .SRCINFO    # regenerate to be safe, then diff against the committed one
git add PKGBUILD .SRCINFO
git commit -m "Initial import" && git push
```

Same flow for `tutabridge-git`. The `.SRCINFO` files here are committed for
convenience, but regenerate them with `makepkg --printsrcinfo` on Arch before
pushing so they match the PKGBUILD exactly.

## Updating on a new release

- `tutabridge-git` updates itself: `pkgver()` derives `0.r<commits>.<short-sha>`
  from git, so a rebuild always tracks the latest commit. Only re-push if the
  PKGBUILD itself changes.
- `tutabridge-bin` pins a release: bump `pkgver` and `_tag`, refresh the three
  `sha256sums` (`updpkgsums`), regenerate `.SRCINFO`, and push.

## Notes

- The keyring Secret Service backend links system `libdbus` (via `libdbus-sys`),
  so `dbus` is a build + runtime dependency. It is present on essentially every
  Linux desktop already.
- Keyring *persistence* additionally needs a running Secret Service provider
  (gnome-keyring / kwallet), listed as optdepends. Without one the daemon still
  works but asks for the Tuta password on every start.
