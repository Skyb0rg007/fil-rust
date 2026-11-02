/* Breaks memory safety in a constructor: loading the library must stop the
   process with a Fil-C panic before anything else runs. */
#include <stdlib.h>

int ctor_crash_loaded = 0;

__attribute__((constructor)) static void start(void) {
    volatile int *p = malloc(4 * sizeof(int));
    ctor_crash_loaded = p[100];
}
