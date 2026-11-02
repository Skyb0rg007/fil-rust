/* Linked against libstartup_dep.so without an RPATH to find it. */
#define _GNU_SOURCE
#include <errno.h>
#include <string.h>

int dep_value(int x);

int use_dep(int x) { return dep_value(x) + 1000; }

/* The argv[0] the Fil-C libc was started with. */
const char *argv0(void) { return program_invocation_name; }
