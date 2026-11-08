# Rules for coding agents

Read [CONTRIBUTING.md](CONTRIBUTING.md) and [CLAUDE.md](CLAUDE.md) first. These rules add to them.

## Scope

- Do only what was asked. No refactors, renames, reformatting or "improvements" outside the task.
- Smallest change that solves it. No new abstraction, option or file unless the task needs it.
- Prefer pure functions; keep I/O at the edges. Add a unit test for every behaviour change.
- Never edit `sdk/generated` or `tuta-repo` (see docs/SDK_PATCHES.md).

## Verify

- Run `scripts/check.sh` before saying a change is done. Report its result.
- Say plainly what you did not test (live account, other OS, real mail client).
- Tests use synthetic data. Never read, print or commit real mail, addresses, passwords or tokens.
- For a live run, use a `cargo build` binary, never one built by `cargo test`.

## Git and GitHub

- Never push, merge, publish, comment or open a PR unless the human asked for that step.
- No AI attribution: no `Co-authored-by` for a model or tool, no "Generated with", no mention of the assistant in commits, PRs, issues or code.
- Commits and PR texts: short, plain, concrete English. What changed and why. No filler, no headings for a three-line PR.
- Branch from and target `develop`.
