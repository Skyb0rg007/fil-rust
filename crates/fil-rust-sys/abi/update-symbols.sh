#!/usr/bin/env bash
# Regenerates the runtime symbol lists that the libpizlo.so stub is built from.
#
#   crates/fil-rust-sys/abi/update-symbols.sh <fil-c source tree>
#
# Run it inside nix-shell, once per Fil-C version, with the source tree checked
# out at that version. It also checks the runtime's layouts against the tree's
# headers, which aren't available otherwise. Each line of an output file
# is one symbol the stub must define:
#
#   H name   a helper the compiler can call directly
#   D name   a data object
#   N name   a getter for a native function or object
#
# The helpers come from the compiler pass rather than from any library's
# imports: a user library can use one that no shipped library happens to use,
# and one missing symbol fails its whole dlopen.
set -euo pipefail

source_tree=${1:?usage: update-symbols.sh <fil-c source tree>}
here=$(cd "$(dirname "$0")" && pwd)
pass=$source_tree/llvm/lib/Transforms/Instrumentation/FilPizlonator.cpp

# filc_pollcheck is a placeholder that DeleteRedundantPollchecks rewrites into
# filc_pollcheck_slow. That, and filc_stack_overflow_failure, which is only
# reached from generated assembly, are the two helpers the pass doesn't declare
# by name.
helpers=$(tr -d '\n' <"$pass" | grep -oE 'getOrInsertFunction\( *"(filc|verse)_[a-z_0-9]+"' \
    | grep -oE '(filc|verse)_[a-z_0-9]+' | grep -vx 'filc_pollcheck' | sort -u)

generate() {
    local libpizlo=$1 out=$2
    {
        for symbol in $helpers filc_pollcheck_slow filc_stack_overflow_failure; do
            echo "H $symbol"
        done
        echo "D filc_current_marking_state"
        nm -D --defined-only "$libpizlo" | awk '{print $NF}' \
            | grep -E '^pizlonated_' | sed 's/^/N /'
    } | sort -u -k2,2 >"$out"
}

generate "$FILC_ROOT/pizfix/lib/libpizlo.so" "$here/runtime_symbols_musl.txt"
generate "$FILC_GLIBC_ROOT/lib/libpizlo.so" "$here/runtime_symbols_glibc.txt"

cd "$here/../../.."
FILC_RUNTIME_SRC=$source_tree/libpas/src/libpas \
    cargo test -p fil-rust-rt --test layout -- --ignored
