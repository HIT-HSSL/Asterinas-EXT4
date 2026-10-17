#!/bin/bash

set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

read_make_config() {
    make \
        --silent \
        --no-print-directory \
        --directory "$repo_dir" \
        --eval 'print-build-config:;@printf "%s|%s\n" "$(RELEASE)" "$(CARGO_OSDK_COMMON_ARGS)"' \
        print-build-config
}

default_config="$(read_make_config)"
if [[ "$default_config" != 1\|*--release* ]]; then
    echo "expected the default build profile to be release, got: $default_config" >&2
    exit 1
fi

debug_config="$(RELEASE=0 read_make_config)"
if [[ "$debug_config" == *--release* ]]; then
    echo "expected RELEASE=0 to disable the release profile, got: $debug_config" >&2
    exit 1
fi
