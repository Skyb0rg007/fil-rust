#!/usr/bin/env bash
# Copies Fil-C's filc/src, the Fil-C-compiled part of its runtime, from a
# checkout at the pinned version:
#
#   crates/fil-rust-sys/csrc/filc/update.sh <fil-c checkout> v0.687
set -euo pipefail
checkout=${1:?usage: update.sh <fil-c checkout> <tag>}
tag=${2:?usage: update.sh <fil-c checkout> <tag>}
here=$(cd "$(dirname "$0")" && pwd)
for file in gcc_personality.c pizlonated_eh_landing_pad.c runtime.c snprintf.c syscall.h unwind.c; do
    git -C "$checkout" show "$tag:filc/src/$file" >"$here/$file"
done
