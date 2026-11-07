/* Loaded after the program started: its constructor gets what the program got. */

#include <stdio.h>
#include <string.h>

static char variable[64];

__attribute__((constructor)) static void construct(int argc, char** argv, char** envp)
{
    for (; *envp; ++envp) {
        if (!strncmp(*envp, "FIL_RUST_RT_LIBC_CHILD=", 23))
            snprintf(variable, sizeof(variable), "%s", *envp);
    }
}

const char* variable_seen(void)
{
    return variable;
}
