# Contributing

Thanks for helping. A few rules keep the project reviewable.

## Before you code

- Open an issue first, unless the fix is obvious. Agree on the approach, then write it.
- One PR, one change. Small, few files. Split anything bigger.
- No drive-by refactors, renames or reformatting outside your change.

## Code

- Simple first. No abstraction for a single use.
- Pure functions where you can; keep I/O at the edges.
- Every change comes with a unit test. Use synthetic data, never real mail or accounts.
- Never edit `sdk/generated` or `tuta-repo`: see [docs/SDK_PATCHES.md](docs/SDK_PATCHES.md).
- No deprecated or unmaintained dependencies.

## Before you push

```bash
scripts/check.sh
```

It runs what CI runs (format, clippy, tests, UI build). If it fails, CI fails.
Then test the change by hand: build with `cargo build` and run it against a mail client.

## Commits and PRs

- Target `develop`.
- Write short, plain, concrete English. Say what changes and why, nothing else.
- Commit subject: imperative, under 72 characters, no trailing period (`Keep the Sent copy's UID on APPEND`).
- The PR description fills the template: what, why, how you tested it.
- No AI attribution anywhere: no `Co-authored-by` for a tool, no "Generated with", no mention of the assistant. CI rejects it.

## AI tools

Allowed, as long as you understand and can defend every line you submit. Use a current model. Write issues, PR descriptions and replies yourself.

## Security

Report vulnerabilities privately through GitHub's "Report a vulnerability", not in an issue.
