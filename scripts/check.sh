#!/usr/bin/env bash
# What CI checks, run locally before pushing.
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n== %s\n' "$*"; }

step "SDK"
python3 scripts/sdk-prepare.py

step "UI build and types"
[ -d ui/node_modules ] || npm --prefix ui ci
npm --prefix ui run build
(cd ui && npx tsc --noEmit)

step "Format"
cargo fmt --check -p tutabridge -p tutabridge-core -p tutabridge-gui -p tutabridge-tuta

step "Clippy"
cargo clippy --locked --workspace --all-targets

step "Tests"
cargo test --locked --workspace
python3 -m unittest discover -s scripts/tests

printf '\nAll checks passed.\n'
