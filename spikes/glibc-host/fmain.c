#include <stdio.h>
#include <stdlib.h>
#include <string.h>
int add(int a, int b);
int main(int argc, char **argv) {
    char *s = strdup("hello from Fil-C, inside a glibc process");
    printf("%s; add(2,3)=%d; argc=%d argv[0]=%s\n", s, add(2, 3), argc, argv[0]);
    if (getenv("OOB")) { int *p = malloc(16); p[4] = 1; }
    return 7;
}
