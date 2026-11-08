#!/bin/sh
# Builds and runs the rseq-pin prototype. The kernels are built with gcc, and with clang too
# when CLANG names a host clang (not Fil-C's): CLANG=/path/to/clang ./run.sh [what].
# `what` is stores, native, barrier, verify MODE [WIDEN], or all (the default).
set -eu
cd "$(dirname "$0")"
mkdir -p build
build() {
    name=$1 kcc=$2
    $kcc -O3 -ffreestanding -DSUFFIX=_v1 -c kernels.c -o build/kernels_v1_$name.o
    $kcc -O3 -ffreestanding -march=x86-64-v3 -DSUFFIX=_v3 -c kernels.c -o build/kernels_v3_$name.o
    gcc -O2 -g -Wall -Wextra -pthread pin.c build/kernels_v1_$name.o build/kernels_v3_$name.o -o build/pin-$name
}
build gcc gcc
[ -n "${CLANG:-}" ] && build clang "$CLANG"
build/pin-gcc "$@"
if [ -n "${CLANG:-}" ] && { [ $# -eq 0 ] || [ "$1" = native ] || [ "$1" = all ]; }; then
    echo; echo "### kernels built with $("$CLANG" --version | head -1)"
    build/pin-clang native
fi
