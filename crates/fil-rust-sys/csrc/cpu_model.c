/* compiler-rt's __cpu_model and __cpu_features2, under names of the Rust
   runtime's, which copies them into the objects Fil-C code sees. */
#define __cpu_model fr_cpu_model
#define __cpu_features2 fr_cpu_features2
#define __cpu_indicator_init fr_cpu_indicator_init
#include "compiler-rt/x86.c"
