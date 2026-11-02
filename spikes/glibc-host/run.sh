#!/usr/bin/env bash
# Spike: run Fil-C code inside a glibc process, loaded by glibc's ld.so.
# Run from inside the repo's nix-shell (needs `cc` and `filcc`/`fil++`).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=${OUT:-$here/build}
mkdir -p "$out"

cc -O2 -shared -fPIC -Wl,-soname,libyoloc.so -o "$out/libyoloc_compat.so" "$here/yoloc_compat.c" -lm
cc -O2 -o "$out/host" "$here/host.c"
filcc -O2 -g -shared -fPIC -o "$out/libadd.so" "$here/libadd.c"
filcc -O2 -g -shared -fPIC -o "$out/fmain.so" "$here/fmain.c" -L"$out" -ladd -Wl,-rpath,"$out"
filcc -O2 -g -shared -fPIC -o "$out/fstress.so" "$here/fstress.c"
fil++ -O2 -g -shared -fPIC -o "$out/fcxx.so" "$here/fcxx.cpp"

cd "$out"
echo "== basic call + libc"; ./host "$out" fmain.so || echo "exit=$?"
echo "== threads, GC churn, futex, stat/readdir, signals"; FUGC_MIN_THRESHOLD=0 ./host "$out" fstress.so
echo "== C++ exceptions, dlopen from Fil-C"; ./host "$out" fcxx.so "$out"
echo "== safety panic (expected to abort)"; OOB=1 ./host "$out" fmain.so || echo "exit=$?"
