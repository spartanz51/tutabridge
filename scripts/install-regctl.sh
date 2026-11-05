#!/usr/bin/env bash
set -euo pipefail
# Reviewed upstream release, including independent platform checksums.
version=v0.11.7
case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) platform=linux-amd64; checksum=3737fbbdd791e04e7a831640b681f3397a376498e80e2ad9b1b2d27296f239bc ;;
  Linux/aarch64) platform=linux-arm64; checksum=9f349bab5c407ac04f3dabf99c7c7a6b0f9e9723c23f2775380e804d02c6dbb9 ;;
  Darwin/arm64) platform=darwin-arm64; checksum=8ac077a1eeb834d3053714e86ccbd838d0da46567df8cd7c2695d1a19135bf60 ;;
  *) echo 'Unsupported regctl host platform' >&2; exit 1 ;;
esac
output=${1:?supply a destination directory}
mkdir -p "$output"
curl --fail --silent --show-error --location "https://github.com/regclient/regclient/releases/download/$version/regctl-$platform" --output "$output/regctl.download"
printf '%s  %s\n' "$checksum" "$output/regctl.download" | shasum -a 256 --check -
chmod 755 "$output/regctl.download"
mv "$output/regctl.download" "$output/regctl"
