#!/usr/bin/env bash
# Regenerates the symbol lists the stubs are built from: the runtime's, for the
# libpizlo.so stub, and the glibc flavor's libc's, for its stand-ins.
#
#   crates/fil-rust-sys/abi/update-symbols.sh <fil-c source tree>
#
# Run it inside nix-shell, once per Fil-C version, with the source tree checked
# out at that version. It also checks the runtime's layouts against the tree's
# headers, which aren't available otherwise. Each line of a runtime list is one
# symbol the stub must define:
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

# The glibc flavor's libc, which the stand-ins take the place of. For each
# soname, its version nodes in order:
#
#   V node
#
# then every getter it exports, by kind, with the version it's under (`@@` for
# the default one):
#
#   F name@@node   a function's getter
#   D name@node    a variable's getter (thread-local ones too)
#
# The distribution's getters tell the kinds apart by size: a function's is 11
# bytes, or 5 when it jumps to another; a variable's is 53 or 57, and a
# thread-local's 34.
libc_exports() {
    local lib=$1 out=$2 soname
    soname=$(basename "$lib")
    {
        readelf -V "$lib" | sed -n '/Version definition/,/Version needs/p' \
            | grep -oE 'Name: \S+' | awk -v soname="$soname" '$2 != soname {print "V " $2}'
        nm -DS --defined-only --with-symbol-versions "$lib" | awk '
            $4 ~ /^pizlonated_/ {
                size = strtonum("0x" $2)
                if (size == 11 || size == 5) kind = "F"
                else if (size == 53 || size == 57 || size == 34) kind = "D"
                else { print "unknown getter size " size " for " $4 > "/dev/stderr"; exit 1 }
                print kind " " $4
            }' | sort -k2,2
    } >"$out"
}

for soname in libc.so.6666 libm.so.6666 libpthread.so.6660 libdl.so.6662; do
    libc_exports "$FILC_GLIBC_ROOT/lib/$soname" "$here/glibc_$soname.txt"
done

cd "$here/../../.."
FILC_RUNTIME_SRC=$source_tree/libpas/src/libpas \
    cargo test -p fil-rust-rt --test layout -- --ignored
