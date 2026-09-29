#!/usr/bin/env bash
# Generate the vendored SDK from an official Tuta release plus our patches.
#
#   scripts/sdk-generate.sh            apply sdk/patches on sdk/BASE in tuta-repo
#   scripts/sdk-generate.sh --check    verify the tuta-repo pin is exactly that
#   scripts/sdk-generate.sh --push     also publish it on the SDK fork, on a
#                                      branch named after the generated commit
#   scripts/sdk-generate.sh --verify-each
#                                      also check every patch on its own: after
#                                      each one the SDK must be formatted and
#                                      pass its tests (slow; run before a PR)
#
# The fork is only a place to host the result: nothing there is edited by
# hand. The patches in sdk/patches are the source of truth, and the output
# is reproducible (fixed committer, committer date = author date, no local
# git configuration), so the same base and patches always give the same
# commit.
set -euo pipefail

cd "$(dirname "$0")/.."
root=$PWD
sdk=tuta-repo
UPSTREAM=https://github.com/tutao/tutanota.git
FORK=https://github.com/spartanz51/tutanota.git

mode=generate
case "${1:-}" in
	"") ;;
	--check) mode=check ;;
	--push) mode=push ;;
	--verify-each) mode=verify-each ;;
	*) echo "usage: $0 [--check|--push|--verify-each]" >&2; exit 2 ;;
esac

# An uninitialised submodule is an empty directory, and git would then act on
# this repository instead: refuse before touching anything.
if [ "$(git -C "$sdk" rev-parse --show-toplevel 2>/dev/null)" != "$root/$sdk" ]; then
	echo "$sdk is not initialised; run: git submodule update --init $sdk" >&2
	exit 1
fi

patches=("$root"/sdk/patches/*.patch)
[ -e "${patches[0]}" ] || { echo "no patches in sdk/patches" >&2; exit 1; }

tag=$(sed -n 's/^tag=//p' sdk/BASE)
base=$(sed -n 's/^commit=//p' sdk/BASE)
[ -n "$tag" ] && [ -n "$base" ] || { echo "sdk/BASE needs tag= and commit=" >&2; exit 1; }

git -C "$sdk" fetch --quiet "$UPSTREAM" "refs/tags/$tag:refs/tags/$tag"
tagged=$(git -C "$sdk" rev-parse "refs/tags/$tag^{commit}")
if [ "$tagged" != "$base" ]; then
	echo "sdk/BASE says $base but $tag is $tagged" >&2
	exit 1
fi

# Build on a scratch branch inside the submodule so a failed patch leaves the
# current checkout alone.
work=$(mktemp -d)
log="$work.log"
trap 'git -C "$sdk" worktree remove --force "$work" >/dev/null 2>&1 || true; rm -rf "$work" "$log"' EXIT
git -C "$sdk" worktree add --quiet --detach "$work" "$base"

# Only these settings may shape the commits: no user or system git
# configuration (signing, whitespace fixes, hooks) can change the result.
am() {
	GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
		GIT_COMMITTER_NAME="TutaBridge SDK generator" GIT_COMMITTER_EMAIL="a.m@tuta.com" \
		git -C "$work" -c commit.gpgSign=false -c apply.whitespace=nowarn -c core.hooksPath=/dev/null \
		am --quiet --keep-non-patch --committer-date-is-author-date "$@"
}

for patch in "${patches[@]}"; do
	name=$(basename "$patch")
	if ! am "$patch"; then
		git -C "$work" am --abort || true
		# A patch whose change is already in the release reverses cleanly: Tuta
		# has taken it, and it can be deleted.
		if git -C "$work" apply --check --reverse "$patch" 2>/dev/null; then
			echo "$name is already included in $tag: remove it from sdk/patches" >&2
		else
			echo "$name does not apply on $tag: update it in sdk/patches (Tuta may have taken part of it)" >&2
		fi
		exit 1
	fi
	if [ "$mode" = verify-each ]; then
		(cd "$work" && cargo fmt --check -p tuta-sdk >"$log" 2>&1) ||
			{ tail -n 30 "$log" >&2; echo "$name: not rustfmt-clean" >&2; exit 1; }
		(cd "$work" && CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target/sdk-verify}" \
			cargo test --quiet -p tuta-sdk >"$log" 2>&1) ||
			{ tail -n 30 "$log" >&2; echo "$name: the SDK tests fail after it" >&2; exit 1; }
		echo "$name: formatted, tests pass"
	fi
done
generated=$(git -C "$work" rev-parse HEAD)

# The reported client version must be the release's own, never an edit.
if git -C "$work" diff "$base" HEAD -- '*Cargo.toml' | grep -qE '^[+-][[:space:]]*version[[:space:]]*='; then
	echo "the patches change a crate version; the SDK version must come from the release" >&2
	exit 1
fi

case "$mode" in
check)
	pinned=$(git ls-files --stage "$sdk" | awk '{print $2}')
	if [ "$pinned" != "$generated" ]; then
		echo "$sdk is pinned at $pinned, but $tag + sdk/patches is $generated" >&2
		exit 1
	fi
	echo "$sdk matches $tag + ${#patches[@]} patches ($generated)"
	;;
generate | push | verify-each)
	git -C "$sdk" checkout --quiet --detach "$generated"
	stage="git add $sdk"
	if [ "$mode" = push ]; then
		# One branch per generated commit, never rewritten: every pin that main
		# ever had stays fetchable.
		branch="generated/${tag#tutanota-release-}-${generated:0:12}"
		git -C "$sdk" push --quiet "$FORK" "$generated:refs/heads/$branch"
		git config -f .gitmodules submodule.$sdk.branch "$branch"
		echo "pushed $branch (recorded in .gitmodules)"
		stage="git add $sdk .gitmodules"
	fi
	echo "$sdk now at $generated ($tag + sdk/patches); stage it with: $stage"
	;;
esac
