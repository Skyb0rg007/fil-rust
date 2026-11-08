`x86.c` and `cpu_model.h` are compiler-rt's `lib/builtins/cpu_model` at
`llvmorg-20.1.8`, the LLVM that Fil-C 0.687's clang is built from, unmodified.
They're under the Apache License v2.0 with LLVM Exceptions
(https://llvm.org/LICENSE.txt).

Fil-C code reads `__cpu_model` and `__cpu_features2` in the layout this clang
gives them, which is what this code fills in. `../cpu_model.c` renames the
symbols so they can't meet libgcc's.
