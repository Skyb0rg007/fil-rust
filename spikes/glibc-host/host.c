/* A plain glibc program that loads the Fil-C runtime, the Fil-C libc and a
   Fil-C library through glibc's own ld.so, then hands control to the Fil-C
   program's main via filc_start_program (which never returns). */
#include <dlfcn.h>
#include <gnu/libc-version.h>
#include <stdio.h>
#include <stdlib.h>

typedef void *(*getter)(void);

static void *must(void *p, const char *what)
{
    if (!p) {
        fprintf(stderr, "%s: %s\n", what, dlerror());
        exit(1);
    }
    return p;
}

int main(int argc, char **argv)
{
    if (argc < 3) {
        fprintf(stderr, "usage: %s <dir> <fil-c-main.so> [args...]\n", argv[0]);
        return 2;
    }
    const char *dir = argv[1];
    char path[4096];
    printf("host: glibc %s\n", gnu_get_libc_version());

    /* Must be loaded before any Fil-C object so that their NEEDED
       "libyoloc.so" binds to this shim (by soname) instead of the musl
       libyoloc.so found through their RUNPATH. */
    snprintf(path, sizeof path, "%s/libyoloc_compat.so", dir);
    must(dlopen(path, RTLD_NOW | RTLD_GLOBAL), "compat");

    snprintf(path, sizeof path, "%s/%s", dir, argv[2]);
    void *fm = must(dlopen(path, RTLD_NOW), "fil-c main");
    void (*start)(int, char **, getter, getter) =
        must(dlsym(fm, "filc_start_program"), "filc_start_program");
    getter lsm = must(dlsym(fm, "pizlonated___libc_start_main"), "__libc_start_main");
    getter m = must(dlsym(fm, "pizlonated_main"), "main");
    printf("host: Fil-C libraries loaded by glibc ld.so, starting runtime\n");
    fflush(stdout);
    start(argc - 2, argv + 2, lsm, m);
    return 99;
}
