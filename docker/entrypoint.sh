#!/usr/bin/env bash
set -euo pipefail
umask 077
if [[ $# == 0 ]] || [[ $# == 1 && $1 == run ]]; then
    exec tutabridge run --wait
fi
exec tutabridge "$@"
