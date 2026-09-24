#!/usr/bin/env bash
# Generate the vendored SDK from an official Tuta release plus our patches.
#
#   scripts/sdk-generate.sh            apply sdk/patches on sdk/BASE in tuta-repo
#   scripts/sdk-generate.sh --check    verify the tuta-repo pin is exactly that
#   scripts/sdk-generate.sh --push     also publish it on the SDK fork
#
# The fork is only a place to host the result: nothing there is edited by
# hand. The patches in sdk/patches are the source of truth, and the output
# is reproducible (fixed committer, committer date = author date), so the
# same base and patches always give the same commit.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
UPSTREAM=https://github.com/tutao/tutanota.git
FORK=https://github.com/spartanz51/tutanota.git

mode=generate
case "${1:-}" in
	"") ;;
	--check) mode=check ;;
	--push) mode=push ;;
	*) echo "usage: $0 [--check|--push]" >&2; exit 2 ;;
esac

tag=$(sed -n 's/^tag=//p' sdk/BASE)
base=$(sed -n 's/^commit=//p' sdk/BASE)
[ -n "$tag" ] && [ -n "$base" ] || { echo "sdk/BASE needs tag= and commit=" >&2; exit 1; }

sdk=tuta-repo
git -C "$sdk" fetch --quiet "$UPSTREAM" "refs/tags/$tag:refs/tags/$tag"
tagged=$(git -C "$sdk" rev-parse "refs/tags/$tag^{commit}")
if [ "$tagged" != "$base" ]; then
	echo "sdk/BASE says $base but $tag is $tagged" >&2
	exit 1
fi

# Build on a scratch branch inside the submodule so a failed patch leaves the
# current checkout alone.
work=$(mktemp -d)
trap 'git -C "$sdk" worktree remove --force "$work" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT
git -C "$sdk" worktree add --quiet --detach "$work" "$base"

export GIT_COMMITTER_NAME="TutaBridge SDK generator"
export GIT_COMMITTER_EMAIL="a.m@tuta.com"
if ! git -C "$work" am --quiet --keep-non-patch --committer-date-is-author-date "$PWD"/sdk/patches/*.patch; then
	echo "sdk/patches do not apply on $tag" >&2
	git -C "$work" am --abort || true
	exit 1
fi
generated=$(git -C "$work" rev-parse HEAD)

# The reported client version must be the release's own, never an edit.
if ! git -C "$work" diff --quiet "$base" HEAD -- Cargo.toml; then
	echo "the patches change Cargo.toml; the SDK version must come from the release" >&2
	exit 1
fi

case "$mode" in
check)
	pinned=$(git ls-files --stage "$sdk" | awk '{print $2}')
	if [ "$pinned" != "$generated" ]; then
		echo "tuta-repo is pinned at $pinned, but $tag + sdk/patches is $generated" >&2
		exit 1
	fi
	echo "tuta-repo matches $tag + $(ls sdk/patches/*.patch | wc -l | tr -d ' ') patches ($generated)"
	;;
generate | push)
	git -C "$sdk" checkout --quiet --detach "$generated"
	if [ "$mode" = push ]; then
		git -C "$sdk" push --quiet "$FORK" "$generated:refs/heads/generated/${tag#tutanota-release-}"
		echo "pushed generated/${tag#tutanota-release-}"
	fi
	echo "tuta-repo now at $generated ($tag + sdk/patches); stage it with: git add tuta-repo"
	;;
esac
