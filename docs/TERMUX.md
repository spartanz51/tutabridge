# Android / Termux CLI (experimental)

Android support is experimental. The Android CI job builds the CLI for
`aarch64-linux-android` with Android API 24 as its minimum target. A successful
build does not validate sign-in, GPG prompts, TLS or background operation on a
phone. No on-device validation is claimed yet.

This path uses the existing `pass` backend with GnuPG installed in Termux. The
Android build deliberately has no OS-keyring backend: a nonempty `pass_path`
is required. An empty value or a value containing only `/` is not a selection.
There is no automatic fallback to a plaintext session file.

## Prepare Termux and GPG

Run these commands in a Termux terminal:

```sh
pkg update
pkg install pass gnupg
export GPG_TTY="$(tty)"
```

Termux packages [pass with a GnuPG dependency](https://github.com/termux/termux-packages/blob/master/packages/pass/build.sh);
its [GnuPG package includes pinentry](https://github.com/termux/termux-packages/blob/master/packages/gnupg/build.sh).

If you do not already have a GPG key in Termux, generate one with encryption
capability and a passphrase, then display its fingerprint:

```sh
gpg --full-generate-key
gpg --list-secret-keys --fingerprint
```

For a **new** password store, replace the placeholder with the full fingerprint:

```sh
pass init YOUR_GPG_FINGERPRINT
```

If you already use `pass`, keep its existing configuration. To select a key only
for TutaBridge's subtree, use `pass init --path Tuta/Bridge YOUR_GPG_FINGERPRINT`.
Changing the key with `pass init` can reencrypt existing entries in that scope;
see the [pass manual](https://git.zx2c4.com/password-store/about/).

## Configure TutaBridge

With Termux's normal environment, the config is
`$HOME/.config/tutabridge/config.toml`. An absolute `XDG_CONFIG_HOME` overrides
`$HOME/.config`; use that directory consistently if you have configured one.
TutaBridge uses `dirs::config_dir()` on Android, with `$HOME` as its home-directory
source.

For the default location:

```sh
umask 077
mkdir -p "$HOME/.config/tutabridge"
chmod 700 "$HOME/.config/tutabridge"
```

Create or edit `config.toml` with these settings, preserving other settings if
the file already exists:

```toml
email = "you@tuta.com"
imap_port = 1143
smtp_port = 1025
pass_path = "Tuta/Bridge"
```

Do not put your Tuta password in this file. TutaBridge asks for it and any TOTP
code on first sign-in, then saves the session at `Tuta/Bridge/you@tuta.com` in
`pass`. The password-store defaults to `$HOME/.password-store`; an existing
`PASSWORD_STORE_DIR` configuration is also respected.

## Run the Android binary

Use the `tutabridge` executable inside the CI artifact
`tutabridge-android-aarch64` from a successful Android build. This is a debug
build for testing, not an Android release package. It targets an ARM64 device
with Android API 24 or newer. A desktop Linux binary is not a substitute.

Copy the executable into Termux's private home directory as `$HOME/tutabridge`,
then run:

```sh
chmod 700 "$HOME/tutabridge"
"$HOME/tutabridge" --version
export GPG_TTY="$(tty)"
"$HOME/tutabridge"
```

Keep the terminal and bridge running during testing. In another Termux terminal,
print the generated local mail-client password with:

```sh
"$HOME/tutabridge" password
```

Configure a mail client on the same device with your Tuta address as username,
that **bridge password**, and SSL/TLS on `127.0.0.1:1143` for IMAP and
`127.0.0.1:1025` for SMTP. The bridge creates a self-signed TLS certificate;
the client must support trusting it. These loopback listeners are not reachable
from a separate computer.

## GPG prompts and troubleshooting

Set `GPG_TTY` from the current terminal before starting the bridge. To keep it
current in new interactive shells, add `export GPG_TTY="$(tty)"` to the relevant
shell startup file. GnuPG starts `gpg-agent` on demand. Its
[agent documentation](https://www.gnupg.org/documentation/manuals/gnupg/Invoking-GPG_002dAGENT.html)
explains `GPG_TTY` and the `pinentry-program` option.

If decryption fails, first check the entry directly from that terminal without
printing its contents:

```sh
pass show "Tuta/Bridge/you@tuta.com" > /dev/null
```

For a `No pinentry` error, check `command -v pinentry` and the installed `pinentry`
package. If you configured `pinentry-program` in GPG's `gpg-agent.conf`, its
absolute path must point to a program installed in Termux. After correcting it,
`gpgconf --kill gpg-agent` lets GnuPG restart the agent with the new configuration;
this also clears its cached passphrases.

A locked key, canceled prompt or invalid stored session is a storage error, not
a reason to overwrite the entry or ask for a new Tuta login. Resolve the GPG
problem and rerun the bridge. A restart may request the **GPG passphrase** even
when the saved Tuta session remains valid.

## Device validation still required

Before treating this as a supported deployment, record the Android and Termux
versions and validate first sign-in with TOTP, GPG prompt cancellation and retry,
restart with the same session, TLS trust, IMAP and SMTP operation, and operation
while the device is locked. CI compilation alone does not cover these cases.
